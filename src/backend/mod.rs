//! Platform clipboard backends behind a tiny trait, so the watcher and CLI
//! are testable without a real clipboard.

use anyhow::{Context as _, Result};
use arboard::ImageData;

/// Reads/writes UTF-8 text (and PNG bytes) from the system clipboard.
pub trait Clipboard: Send {
    fn get_text(&mut self) -> Result<String>;
    fn set_text(&mut self, text: &str) -> Result<()>;
    /// PNG-encoded image bytes, if the clipboard currently holds an image.
    fn get_image_png(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Put PNG bytes on the clipboard as an image (best-effort per platform).
    fn set_image_png(&mut self, _png: &[u8]) -> Result<()> {
        Err(anyhow::anyhow!("image clipboard not supported here"))
    }
}

/// arboard-backed implementation for macOS / Windows / X11 / Wayland.
#[derive(Default)]
pub struct SystemClipboard {
    inner: Option<arboard::Clipboard>,
}

impl SystemClipboard {
    pub fn new() -> Self {
        Self { inner: None }
    }

    fn with<T>(&mut self, f: impl FnOnce(&mut arboard::Clipboard) -> Result<T>) -> Result<T> {
        if self.inner.is_none() {
            self.inner = Some(
                arboard::Clipboard::new()
                    .map_err(|e| anyhow::anyhow!("clipboard unavailable: {e}"))?,
            );
        }
        f(self.inner.as_mut().unwrap())
    }

    /// Run `f`, retrying once on a fresh handle. A clipboard that changed
    /// ownership (common on X11/Wayland) invalidates cached handles.
    fn with_retry<T>(&mut self, f: impl Fn(&mut arboard::Clipboard) -> Result<T>) -> Result<T> {
        match self.with(&f) {
            Ok(v) => Ok(v),
            Err(first) => {
                self.inner = None;
                self.with(f).map_err(|second| {
                    anyhow::anyhow!("clipboard op failed twice: {first}; {second}")
                })
            }
        }
    }

    fn decode_png(png: &[u8]) -> Result<ImageData<'static>> {
        let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
            .context("payload is not a valid PNG")?;
        let rgba = img.to_rgba8();
        Ok(ImageData {
            width: rgba.width() as usize,
            height: rgba.height() as usize,
            bytes: rgba.into_raw().into(),
        })
    }
}

impl Clipboard for SystemClipboard {
    fn get_text(&mut self) -> Result<String> {
        self.with_retry(|c| Ok(c.get_text()?))
    }

    fn set_text(&mut self, text: &str) -> Result<()> {
        self.with_retry(|c| Ok(c.set_text(text.to_string())?))
    }

    fn get_image_png(&mut self) -> Result<Option<Vec<u8>>> {
        // arboard errors (ContentNotAvailable) when no image is held.
        let img: Option<ImageData> = self.with_retry(|c| Ok(c.get_image()?)).ok();
        let Some(ImageData {
            width,
            height,
            bytes,
        }) = img
        else {
            return Ok(None);
        };
        let rgba: Vec<u8> = bytes.into_owned();
        debug_assert_eq!(rgba.len(), width * height * 4);
        let mut png = Vec::new();
        PngEncoder {
            data: &rgba,
            width: width as u32,
            height: height as u32,
        }
        .encode(&mut png)?;
        Ok(Some(png))
    }

    fn set_image_png(&mut self, png: &[u8]) -> Result<()> {
        self.with_retry(|c| {
            let data = Self::decode_png(png)?;
            Ok(c.set_image(data)?)
        })
    }
}

/// RGBA → PNG encoder on arboard's own `image` dependency so we don't pull a
/// second imaging stack into the tree.
struct PngEncoder<'a> {
    data: &'a [u8],
    width: u32,
    height: u32,
}

impl PngEncoder<'_> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let img = image::RgbaImage::from_raw(self.width, self.height, self.data.to_vec())
            .ok_or_else(|| anyhow::anyhow!("clipboard image buffer size mismatch"))?;
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(out), image::ImageFormat::Png)?;
        Ok(())
    }
}

/// In-memory backend for tests.
#[derive(Default, Clone)]
#[allow(dead_code)]
pub struct FakeClipboard {
    pub text: String,
    pub png: Option<Vec<u8>>,
    pub fail_reads: usize,
}

impl Clipboard for FakeClipboard {
    fn get_text(&mut self) -> Result<String> {
        if self.fail_reads > 0 {
            self.fail_reads -= 1;
            Err(anyhow::anyhow!("fake clipboard failure"))
        } else {
            Ok(self.text.clone())
        }
    }

    fn set_text(&mut self, text: &str) -> Result<()> {
        self.text = text.to_string();
        Ok(())
    }

    fn get_image_png(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.png.clone())
    }

    fn set_image_png(&mut self, png: &[u8]) -> Result<()> {
        self.png = Some(png.to_vec());
        Ok(())
    }
}

/// Shared handle so tests can drive the clipboard while the watcher owns it.
pub type SharedFake = std::sync::Arc<std::sync::Mutex<FakeClipboard>>;

#[allow(clippy::needless_return)]
impl Clipboard for SharedFake {
    fn get_text(&mut self) -> Result<String> {
        return self.lock().unwrap().get_text();
    }
    fn set_text(&mut self, text: &str) -> Result<()> {
        return self.lock().unwrap().set_text(text);
    }
    fn get_image_png(&mut self) -> Result<Option<Vec<u8>>> {
        return self.lock().unwrap().get_image_png();
    }
    fn set_image_png(&mut self, png: &[u8]) -> Result<()> {
        return self.lock().unwrap().set_image_png(png);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_roundtrip_and_transient_failure() {
        let mut c = FakeClipboard::default();
        c.set_text("hello").unwrap();
        assert_eq!(c.get_text().unwrap(), "hello");

        c.fail_reads = 1;
        assert!(c.get_text().is_err());
        assert_eq!(c.get_text().unwrap(), "hello");
    }

    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 8, 7, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn fake_image_roundtrip_through_trait() {
        let png = tiny_png();
        let mut c = FakeClipboard::default();
        c.set_image_png(&png).unwrap();
        assert_eq!(c.get_image_png().unwrap().as_deref(), Some(png.as_slice()));
    }

    #[test]
    fn invalid_png_is_rejected_before_touching_clipboard() {
        let mut c = FakeClipboard::default();
        assert!(SystemClipboard::decode_png(b"not a png").is_err());
        assert!(c.get_image_png().unwrap().is_none());
    }
}
