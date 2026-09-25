//! Poll-based clipboard watcher. Polling (rather than platform event hooks)
//! keeps one code path for all four platforms and needs no extra privileges.

use crate::backend::Clipboard;
use crate::config::{Config, DedupMode};
use crate::entry::Kind;
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
    active_config: Config,
    poll_override_ms: Option<u64>,
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
            active_config: cfg.clone(),
            poll_override_ms: None,
        })
    }

    pub fn with_poll_override(mut self, poll_ms: Option<u64>) -> Self {
        self.poll_override_ms = poll_ms.map(|p| p.max(50));
        if let Some(p) = self.poll_override_ms {
            self.poll = Duration::from_millis(p);
        }
        self
    }

    /// One poll cycle against an open store. Public so tests can step it.
    #[cfg(test)]
    pub fn tick(&mut self, store: &mut Store) -> Result<Tick> {
        let _ = self.reload_config()?;

        if let Some(png) = self.clip.get_image_png()? {
            return self.record_image(store, png);
        }

        let text = self.clip.get_text()?;
        self.record_text(store, text)
    }

    fn reload_config(&mut self) -> Result<bool> {
        let path = self.store_dir.join("config.toml");
        match Config::load(&path) {
            Ok(cfg) => {
                let capture_rules_changed = cfg.min_length != self.active_config.min_length
                    || cfg.max_length != self.active_config.max_length
                    || cfg.deny_patterns != self.active_config.deny_patterns
                    || cfg.dedup != self.active_config.dedup;
                self.filter = Filter::new(&cfg)?;
                self.dedup = cfg.dedup;
                let poll_ms = self.poll_override_ms.unwrap_or(cfg.poll_ms.max(50));
                self.poll = Duration::from_millis(poll_ms);
                self.active_config = cfg;
                Ok(capture_rules_changed)
            }
            Err(e) => {
                // Keep the last valid configuration rather than killing a long-running
                // watcher because a config edit was momentarily incomplete.
                eprintln!("clipcrate: ignoring invalid config reload: {e:#}");
                Ok(false)
            }
        }
    }

    fn record_text(&mut self, store: &mut Store, text: String) -> Result<Tick> {
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
        let digest = content_hash(&png);
        let img_dir = self.store_dir.join("images");
        std::fs::create_dir_all(&img_dir)?;
        let rel = format!("images/{digest}.png");
        let abs = self.store_dir.join(&rel);
        if abs.exists() {
            let existing = std::fs::read(&abs)?;
            anyhow::ensure!(
                existing == png,
                "image payload collision at {}",
                abs.display()
            );
        } else {
            let tmp = abs.with_extension("png.tmp");
            {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(&png)?;
                f.sync_all()?;
            }
            if let Err(e) = std::fs::rename(&tmp, &abs) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
        match self.dedup {
            DedupMode::All => {
                store.push_image_entry(&rel, png.len() as u64)?;
                Ok(Tick::Recorded)
            }
            DedupMode::Bump => {
                if let Some(i) = store
                    .entries
                    .iter()
                    .rposition(|e| e.kind == Kind::Image && e.text == rel)
                {
                    let mut e = store.entries.remove(i);
                    e.ts = crate::entry::now_ms();
                    store.entries.push(e);
                    store.rewrite()?;
                    Ok(Tick::Deduped)
                } else {
                    store.push_image_entry(&rel, png.len() as u64)?;
                    Ok(Tick::Recorded)
                }
            }
            DedupMode::Update => {
                if let Some(e) = store
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|e| e.kind == Kind::Image && e.text == rel)
                {
                    e.ts = crate::entry::now_ms();
                    store.rewrite()?;
                    Ok(Tick::Deduped)
                } else {
                    store.push_image_entry(&rel, png.len() as u64)?;
                    Ok(Tick::Recorded)
                }
            }
        }
    }

    /// Run until `stop` is set. Sleeps between polls; each tick opens the
    /// store briefly so CLI commands can interleave between ticks.
    pub fn run(mut self, stop: Arc<AtomicBool>) -> Result<()> {
        let mut last_seen: Option<String> = None;
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }

            if self.reload_config()? {
                // A filter or dedup change can make the unchanged clipboard eligible again.
                last_seen = None;
            }

            let snapshot = match self.clip.get_image_png() {
                Ok(Some(png)) => Some((format!("i:{}", content_hash(&png)), Some(png), None)),
                Ok(None) => match self.clip.get_text() {
                    Ok(text) => Some((format!("t:{text}"), None, Some(text))),
                    Err(_) => None,
                },
                Err(_) => None,
            };

            let Some((fingerprint, image, text)) = snapshot else {
                std::thread::sleep(self.poll);
                continue;
            };
            if last_seen.as_deref() == Some(fingerprint.as_str()) {
                std::thread::sleep(self.poll);
                continue;
            }

            let mut store = match Store::open(&self.store_dir) {
                Ok(s) => s,
                Err(e) if Store::is_lock_contended(&e) => {
                    // Lock held by a CLI command right now: retry this same snapshot later.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => return Err(e),
            };

            let result = if let Some(png) = image {
                self.record_image(&mut store, png)
            } else {
                self.record_text(&mut store, text.unwrap_or_default())
            };
            if result.is_ok() {
                last_seen = Some(fingerprint);
            }
            drop(store);
            std::thread::sleep(self.poll);
        }
    }
}

/// 128-bit FNV-1a content fingerprint used for local image deduplication.
fn content_hash(data: &[u8]) -> String {
    let mut h: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    for b in data {
        h ^= *b as u128;
        h = h.wrapping_mul(PRIME);
    }
    h ^= data.len() as u128;
    format!("{h:032x}")
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
    fn existing_image_payload_must_match_hash_target() {
        let dir = tmpdir("img-collision");
        let png: Vec<u8> = {
            let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([11, 12, 13, 255]));
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            buf
        };
        let digest = content_hash(&png);
        let images = dir.join("images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join(format!("{digest}.png")), b"corrupt").unwrap();

        let clip = shared("");
        let mut w = w(clip, &dir);
        let mut s = Store::open(&dir).unwrap();
        let err = w.record_image(&mut s, png).unwrap_err().to_string();
        assert!(err.contains("image payload collision"), "{err}");
        assert!(s.is_empty());
    }

    #[test]
    fn image_dedup_all_records_each_event_but_reuses_payload() {
        let dir = tmpdir("img-all");
        let png: Vec<u8> = {
            let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([5, 6, 7, 255]));
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
        let mut w = w(clip, &dir);
        w.dedup = DedupMode::All;
        let mut s = Store::open(&dir).unwrap();

        assert_eq!(w.record_image(&mut s, png.clone()).unwrap(), Tick::Recorded);
        assert_eq!(w.record_image(&mut s, png).unwrap(), Tick::Recorded);
        assert_eq!(s.len(), 2);
        assert_eq!(std::fs::read_dir(dir.join("images")).unwrap().count(), 1);
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
    fn reload_reports_capture_rule_changes_only() {
        let dir = tmpdir("reload");
        let clip = shared("same clipboard");
        let base = Config {
            poll_ms: 50,
            ..Default::default()
        };
        base.save(&dir.join("config.toml")).unwrap();
        let mut w = Watcher::new(clip, dir.clone(), &base).unwrap();

        assert!(!w.reload_config().unwrap());

        let mut changed = base.clone();
        changed.preview_lines = 20;
        changed.save(&dir.join("config.toml")).unwrap();
        assert!(!w.reload_config().unwrap());

        changed.deny_patterns.push("^blocked$".into());
        changed.save(&dir.join("config.toml")).unwrap();
        assert!(w.reload_config().unwrap());
        assert!(!w.reload_config().unwrap());
    }

    #[test]
    fn identical_images_get_same_name() {
        let a = content_hash(b"same bytes");
        let b = content_hash(b"same bytes");
        assert_eq!(a, b);
        assert_ne!(a, content_hash(b"other bytes"));
        assert_eq!(a.len(), 32);
    }
}
