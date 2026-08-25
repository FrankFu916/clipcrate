//! Poll-based clipboard watcher. Polling (rather than platform event hooks)
//! keeps one code path for all four platforms and needs no extra privileges.

use crate::backend::Clipboard;
use crate::config::{Config, DedupMode};
use crate::filter::Filter;
use crate::store::Store;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// What the watcher did on its most recent tick — used by tests.
#[derive(Debug, PartialEq, Eq)]
pub enum Tick {
    Recorded,
    Deduped,
    Ignored,
}

pub struct Watcher<C: Clipboard> {
    clip: C,
    store_dir: PathBuf,
    poll: Duration,
    filter: Filter,
    dedup: DedupMode,
}

impl<C: Clipboard> Watcher<C> {
    /// Build a watcher; reloads config each tick so `clipcrate config set`
    /// takes effect without a daemon restart.
    pub fn new(clip: C, store_dir: PathBuf, cfg: &Config) -> Result<Watcher<C>> {
        let poll = Duration::from_millis(cfg.poll_ms.max(50));
        Ok(Watcher {
            clip,
            store_dir,
            poll,
            filter: Filter::new(cfg)?,
            dedup: cfg.dedup,
        })
    }

    /// One poll cycle against an open store. Public so tests can step it.
    pub fn tick(&mut self, store: &mut Store) -> Result<Tick> {
        // Reload rules that may have changed on disk.
        if let Ok(cfg) = Config::load(&self.store_dir.join("config.toml")) {
            self.filter = Filter::new(&cfg)?;
            self.dedup = cfg.dedup;
            self.poll = Duration::from_millis(cfg.poll_ms.max(50));
        }

        if let Some(png) = self.clip.get_image_png()? {
            return self.record_image(store, png);
        }

        let text = self.clip.get_text()?;
        if text.is_empty() || !self.filter.accepts(&text)? {
            return Ok(Tick::Ignored);
        }
        if store.push_text(&text, self.dedup)?.is_some() {
            Ok(Tick::Recorded)
        } else {
            Ok(Tick::Deduped)
        }
    }

    fn record_image(&mut self, store: &mut Store, png: Vec<u8>) -> Result<Tick> {
        use std::io::Write as _;
        let digest = blake3_hex(&png);
        let img_dir = self.store_dir.join("images");
        std::fs::create_dir_all(&img_dir)?;
        let rel = format!("images/{digest}.png");
        let abs = self.store_dir.join(&rel);
        if !abs.exists() {
            let mut f = std::fs::File::create(&abs)?;
            f.write_all(&png)?;
        }
        // Identical image re-copied: bump existing entry instead of duplicating.
        if let Some(e) = store.entries.iter_mut().rev().find(|e| e.text == rel) {
            e.ts = crate::entry::now_ms();
            store.rewrite()?;
            return Ok(Tick::Deduped);
        }
        store.push_image_entry(&rel, png.len() as u64)?;
        Ok(Tick::Recorded)
    }

    /// Run until `stop` is set. The clipboard probe is cheap; the store is
    /// only opened (and PNGs only re-encoded) when content actually changed,
    /// so an idle watcher costs one pasteboard read per poll.
    pub fn run(mut self, stop: Arc<AtomicBool>) -> Result<()> {
        let mut last_text = String::new();
        let mut last_image = String::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            match self.next_action(&last_text, &last_image) {
                Some(Action::Text(text)) => {
                    if let Ok(mut store) = Store::open(&self.store_dir) {
                        let _ = self.tick(&mut store);
                    }
                    last_text = text;
                }
                Some(Action::Image(png)) => {
                    last_image = blake3_hex(&png);
                    if let Ok(mut store) = Store::open(&self.store_dir) {
                        let _ = self.tick(&mut store);
                    }
                }
                None => {}
            }
            std::thread::sleep(self.poll);
        }
    }

    /// Probe the clipboard and decide whether anything changed enough to
    /// justify opening the store. `None` = idle.
    fn next_action(&mut self, last_text: &str, last_image: &str) -> Option<Action> {
        let text = self.clip.get_text().unwrap_or_default();
        if !text.is_empty() {
            if text == last_text {
                return None;
            }
            return Some(Action::Text(text));
        }
        // Empty text: the clipboard may hold an image instead.
        let png = self.clip.get_image_png().ok().flatten()?;
        let digest = blake3_hex(&png);
        if digest == last_image {
            return None;
        }
        Some(Action::Image(png))
    }
}

/// What the clipboard probe found on its most recent poll.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Text(String),
    Image(Vec<u8>),
}

/// FNV-1a hex digest — enough to dedupe image payloads locally without
/// pulling in a crypto stack.
fn blake3_hex(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    // Fold in length to avoid trivial prefix collisions.
    h ^= data.len() as u64;
    format!("{h:016x}{:016x}", h.swap_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{FakeClipboard, SharedFake};

    type Clip = SharedFake;

    fn shared(text: &str) -> Clip {
        let mut f = FakeClipboard::default();
        f.set_text(text).unwrap();
        std::sync::Arc::new(std::sync::Mutex::new(f))
    }

    fn w(clip: Clip, dir: &std::path::Path) -> Watcher<Clip> {
        Watcher::new(
            clip,
            dir.to_path_buf(),
            &Config {
                poll_ms: 50,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "clipcrate-watcher-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn records_ignores_and_dedups() {
        let dir = tmpdir("basic");
        let clip = shared("");
        let mut w = w(clip.clone(), &dir);

        let mut s = Store::open(&dir).unwrap();
        clip.lock().unwrap().set_text("first").unwrap();
        assert_eq!(w.tick(&mut s).unwrap(), Tick::Recorded);
        assert_eq!(w.tick(&mut s).unwrap(), Tick::Deduped, "same content again");
        clip.lock().unwrap().set_text("").unwrap();
        assert_eq!(w.tick(&mut s).unwrap(), Tick::Ignored);
        clip.lock().unwrap().set_text("second").unwrap();
        assert_eq!(w.tick(&mut s).unwrap(), Tick::Recorded);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn deny_pattern_blocks_recording() {
        let dir = tmpdir("deny");
        crate::config::Config {
            poll_ms: 50,
            deny_patterns: vec!["^sk-".into()],
            ..Default::default()
        }
        .save(&dir.join("config.toml"))
        .unwrap();

        let clip = shared("");
        let mut w = w(clip.clone(), &dir);
        let mut s = Store::open(&dir).unwrap();

        clip.lock().unwrap().set_text("sk-shouldnotsave").unwrap();
        assert_eq!(w.tick(&mut s).unwrap(), Tick::Ignored);
        clip.lock().unwrap().set_text("fine").unwrap();
        assert_eq!(w.tick(&mut s).unwrap(), Tick::Recorded);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn images_stored_once_and_deduped() {
        let dir = tmpdir("img");
        let png: Vec<u8> = {
            let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            buf
        };
        let fake = FakeClipboard {
            png: Some(png.clone()),
            ..Default::default()
        };
        let clip: Clip = std::sync::Arc::new(std::sync::Mutex::new(fake));
        let mut w = w(clip.clone(), &dir);
        let mut s = Store::open(&dir).unwrap();

        assert_eq!(w.tick(&mut s).unwrap(), Tick::Recorded);
        assert_eq!(w.tick(&mut s).unwrap(), Tick::Deduped);
        assert_eq!(s.len(), 1);
        let img_files = std::fs::read_dir(dir.join("images")).unwrap().count();
        assert_eq!(img_files, 1);
        let payload = s.payload_bytes(s.entries.first().unwrap()).unwrap();
        assert_eq!(payload, png);
    }

    #[test]
    fn run_loop_persists_across_reopens() {
        // Drive run()'s inner logic via repeated tick + reopen, which is what
        // run() does each cycle; actual thread timing is covered by e2e tests.
        let dir = tmpdir("loop");
        let clip = shared("");
        let mut w = w(clip.clone(), &dir);
        {
            let mut s = Store::open(&dir).unwrap();
            clip.lock().unwrap().set_text("persist me").unwrap();
            w.tick(&mut s).unwrap();
        }
        let s = Store::open(&dir).unwrap();
        assert_eq!(s.iter_newest_first().next().unwrap().text, "persist me");
    }

    #[test]
    fn identical_images_get_same_name() {
        let a = blake3_hex(b"same bytes");
        let b = blake3_hex(b"same bytes");
        assert_eq!(a, b);
        assert_ne!(a, blake3_hex(b"other bytes"));
    }

    #[test]
    fn next_action_gates_unchanged_content() {
        let dir = tmpdir("gate");
        let clip = shared("");
        let mut w = w(clip.clone(), &dir);

        // Unchanged text → idle.
        clip.lock().unwrap().set_text("stable").unwrap();
        assert_eq!(w.next_action("", ""), Some(Action::Text("stable".into())));
        assert_eq!(
            w.next_action("stable", ""),
            None,
            "same text must not reopen the store"
        );

        // Changed text fires again.
        clip.lock().unwrap().set_text("moved on").unwrap();
        assert_eq!(
            w.next_action("stable", ""),
            Some(Action::Text("moved on".into()))
        );

        // Image content is gated by digest.
        let png = tiny_png(7);
        clip.lock().unwrap().set_text("").unwrap();
        clip.lock().unwrap().png = Some(png.clone());
        match w.next_action("moved on", "") {
            Some(Action::Image(p)) => assert_eq!(p, png),
            other => panic!("expected image action, got {other:?}"),
        }
        assert_eq!(
            w.next_action("moved on", &blake3_hex(&png)),
            None,
            "same image digest must be idle"
        );
    }

    fn tiny_png(seed: u8) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([seed, seed, seed, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }
}
