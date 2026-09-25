//! Append-only JSONL history store with an exclusive advisory lock,
//! dedup-aware insertion, LRU eviction and atomic rewrites.

use crate::config::DedupMode;
use crate::entry::{now_ms, Entry, Kind};
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read as _, Write};
use std::path::{Path, PathBuf};

pub const HISTORY_FILE: &str = "history.jsonl";
const LOCK_FILE: &str = "store.lock";

#[derive(Debug)]
struct LockContended {
    dir: PathBuf,
}

impl std::fmt::Display for LockContended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "another clipcrate process holds the store lock ({})",
            self.dir.display()
        )
    }
}

impl std::error::Error for LockContended {}

/// A handle to the on-disk history. All writers take an exclusive `flock`
/// for the lifetime of the handle so watcher and CLI never interleave.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    path: PathBuf,
    _lock: File,
    pub entries: Vec<Entry>,
    next_id: u64,
}

impl Store {
    /// Open (creating if needed) the store under `dir` and take the lock.
    /// Single attempt: callers that expect contention choose `open_blocking`.
    pub fn open(dir: &Path) -> Result<Store> {
        Store::open_inner(dir, None)
    }

    /// Retry only lock contention for up to `wait`. Parse, permission and
    /// filesystem errors are returned immediately.
    pub fn open_blocking(dir: &Path, wait: std::time::Duration) -> Result<Store> {
        Store::open_inner(dir, Some(wait))
    }

    fn open_inner(dir: &Path, wait: Option<std::time::Duration>) -> Result<Store> {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create data dir {}", dir.display()))?;
        let path = dir.join(HISTORY_FILE);

        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK_FILE))?;
        let deadline = wait.map(|w| std::time::Instant::now() + w);
        loop {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if deadline.is_some_and(|d| std::time::Instant::now() < d) {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    }
                    return Err(LockContended {
                        dir: dir.to_path_buf(),
                    }
                    .into());
                }
                Err(e) => return Err(e.into()),
            }
        }

        recover_interrupted_rewrite(&path)?;
        let entries = load_entries(&path)?;
        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        Ok(Store {
            dir: dir.to_path_buf(),
            path,
            _lock: lock,
            entries,
            next_id,
        })
    }

    pub fn is_lock_contended(err: &anyhow::Error) -> bool {
        err.downcast_ref::<LockContended>().is_some()
    }

    /// Newest entry first.
    pub fn iter_newest_first(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().rev()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn data_dir(&self) -> &Path {
        &self.dir
    }

    /// Record new content. Returns `None` when dedup suppressed the insert
    /// (the existing entry was bumped/kept instead).
    pub fn push_text(&mut self, text: &str, mode: DedupMode) -> Result<Option<u64>> {
        match mode {
            DedupMode::All => Ok(Some(self.append_text(text)?)),
            DedupMode::Bump => {
                if let Some(i) = self
                    .entries
                    .iter()
                    .rposition(|e| e.kind == Kind::Text && e.text == text)
                {
                    let mut e = self.entries.remove(i);
                    e.ts = now_ms();
                    self.entries.push(e);
                    self.rewrite()?;
                    Ok(None)
                } else {
                    Ok(Some(self.append_text(text)?))
                }
            }
            DedupMode::Update => {
                if let Some(e) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|e| e.kind == Kind::Text && e.text == text)
                {
                    e.ts = now_ms();
                    self.rewrite()?;
                    Ok(None)
                } else {
                    Ok(Some(self.append_text(text)?))
                }
            }
        }
    }

    fn append_text(&mut self, text: &str) -> Result<u64> {
        let entry = Entry::new_text(self.next_id, now_ms(), text.to_string());
        self.next_id += 1;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut f, &entry)?;
        f.write_all(b"\n")?;
        f.flush()?;
        self.entries.push(entry);
        self.evict_and_rewrite()?;
        Ok(self.next_id - 1)
    }

    /// Insert a pre-built image entry (PNG already written by the caller).
    pub fn push_image_entry(&mut self, rel_path: &str, size: u64) -> Result<u64> {
        let entry = Entry::new_image(self.next_id, now_ms(), rel_path, size);
        let id = entry.id;
        self.next_id += 1;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut f, &entry)?;
        f.write_all(b"\n")?;
        f.flush()?;
        self.entries.push(entry);
        self.evict_and_rewrite()?;
        Ok(id)
    }

    /// Evict unpinned overflow (oldest first), drop image payloads that are
    /// no longer referenced, and rewrite the file atomically.
    fn evict_and_rewrite(&mut self) -> Result<()> {
        let max = crate::config::Config::load(&self.dir.join("config.toml"))
            .ok()
            .map(|c| c.max_entries)
            .unwrap_or(1000);
        let unpinned_count = self.entries.iter().filter(|e| !e.pinned).count();
        let overflow = unpinned_count.saturating_sub(max);
        for _ in 0..overflow {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.pinned)
                .min_by_key(|(_, e)| (e.ts, e.id))
                .map(|(idx, _)| idx);
            match oldest {
                Some(idx) => {
                    self.entries.remove(idx);
                }
                None => break,
            }
        }
        self.rewrite()
    }

    /// Enforce retention limits after bulk mutations such as import.
    pub fn enforce_limits(&mut self) -> Result<()> {
        self.evict_and_rewrite()
    }

    /// Delete image files nothing references anymore.
    fn prune_orphan_images(&self) -> Result<()> {
        let img_dir = self.dir.join("images");
        let ok =
            |rd: std::io::Result<fs::DirEntry>| -> Option<PathBuf> { rd.ok().map(|e| e.path()) };
        let rd = match fs::read_dir(&img_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for p in rd.filter_map(ok) {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if !name.ends_with(".png") {
                continue;
            }
            let still_used = self
                .entries
                .iter()
                .any(|e| e.text == format!("images/{name}"));
            if !still_used {
                let _ = fs::remove_file(&p);
            }
        }
        Ok(())
    }

    /// Atomically replace history.jsonl with the current in-memory state:
    /// write `history.jsonl.tmp`, fsync, rename over the original.
    pub fn rewrite(&self) -> Result<()> {
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let f = File::create(&tmp)?;
            let mut w = std::io::BufWriter::new(f);
            for e in &self.entries {
                serde_json::to_writer(&mut w, e)?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        replace_file(&tmp, &self.path)?;
        self.prune_orphan_images()?;
        Ok(())
    }

    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Toggle pin state. Returns the new state, or `None` if not found.
    pub fn toggle_pin(&mut self, id: u64) -> Option<bool> {
        let e = self.entries.iter_mut().find(|e| e.id == id)?;
        e.pinned = !e.pinned;
        Some(e.pinned)
    }

    /// Delete by id; returns true when something was deleted.
    pub fn delete(&mut self, id: u64) -> bool {
        let mut removed_ids = std::collections::HashSet::new();
        self.entries.retain(|e| {
            if e.id == id {
                removed_ids.insert(e.id);
                false
            } else {
                true
            }
        });
        let deleted = !removed_ids.is_empty();
        deleted
    }

    /// Remove all unpinned entries; returns how many were removed.
    pub fn clear_unpinned(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.pinned);
        before - self.entries.len()
    }

    /// Payload bytes for an entry (text itself or PNG file contents).
    pub fn payload_bytes(&self, e: &Entry) -> Result<Vec<u8>> {
        match e.kind {
            Kind::Text => Ok(e.text.clone().into_bytes()),
            Kind::Image => {
                let p = e
                    .payload_path(&self.dir)
                    .context("image entry without path")?;
                let mut buf = Vec::new();
                File::open(p)?.read_to_end(&mut buf)?;
                Ok(buf)
            }
        }
    }
}

fn recover_interrupted_rewrite(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let backup = path.with_extension("jsonl.bak");
    let tmp = path.with_extension("jsonl.tmp");
    if backup.exists() {
        fs::rename(&backup, path)?;
    } else if tmp.exists() {
        fs::rename(&tmp, path)?;
    }
    Ok(())
}

fn replace_file(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(src, dst)?;
        Ok(())
    }

    #[cfg(windows)]
    {
        let backup = dst.with_extension("jsonl.bak");
        let _ = fs::remove_file(&backup);
        if dst.exists() {
            fs::rename(dst, &backup)?;
        }
        match fs::rename(src, dst) {
            Ok(()) => {
                let _ = fs::remove_file(backup);
                Ok(())
            }
            Err(e) => {
                if backup.exists() {
                    let _ = fs::rename(&backup, dst);
                }
                Err(e.into())
            }
        }
    }
}

fn load_entries(path: &Path) -> Result<Vec<Entry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = BufReader::new(File::open(path)?);
    let mut out = Vec::new();
    for (i, line) in f.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let e: Entry = serde_json::from_str(&line)
            .with_context(|| format!("corrupt history line {} in {}", i + 1, path.display()))?;
        if e.kind == Kind::Image {
            let root = path.parent().unwrap_or_else(|| Path::new("."));
            if e.payload_path(root).is_none() {
                anyhow::bail!(
                    "unsafe image path on history line {} in {}",
                    i + 1,
                    path.display()
                );
            }
        }
        out.push(e);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "clipcrate-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn append_load_and_dedup_bump() {
        let dir = tmpdir("bump");
        let mut s = Store::open(&dir).unwrap();
        s.push_text("alpha", DedupMode::Bump).unwrap();
        s.push_text("beta", DedupMode::Bump).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.iter_newest_first().next().unwrap().text, "beta");

        // Re-copying alpha bumps it to the top without adding a row.
        assert_eq!(s.push_text("alpha", DedupMode::Bump).unwrap(), None);
        assert_eq!(s.len(), 2);
        assert_eq!(s.iter_newest_first().next().unwrap().text, "alpha");
        drop(s);

        // State survives reopen.
        let s2 = Store::open(&dir).unwrap();
        assert_eq!(s2.len(), 2);
        assert_eq!(s2.iter_newest_first().next().unwrap().text, "alpha");
    }

    #[test]
    fn dedup_modes_update_and_all() {
        let dir = tmpdir("modes");
        let mut s = Store::open(&dir).unwrap();
        s.push_text("x", DedupMode::Update).unwrap();
        s.push_text("y", DedupMode::Update).unwrap();
        s.push_text("x", DedupMode::Update).unwrap(); // ts updated, stays in place
        assert_eq!(s.len(), 2);
        assert_eq!(s.iter_newest_first().next().unwrap().text, "y");

        s.push_text("x", DedupMode::All).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s.entries.iter().filter(|e| e.text == "x").count(), 2);
    }

    #[test]
    fn lru_eviction_respects_pins() {
        let dir = tmpdir("evict");
        // Write a config with max_entries = 3.
        crate::config::Config {
            max_entries: 3,
            ..Default::default()
        }
        .save(&dir.join("config.toml"))
        .unwrap();

        let mut s = Store::open(&dir).unwrap();
        for t in ["a", "b", "c"] {
            s.push_text(t, DedupMode::All).unwrap();
        }
        let b_id = s.entries.iter().find(|e| e.text == "b").unwrap().id;
        s.toggle_pin(b_id);
        for t in ["d", "e"] {
            s.push_text(t, DedupMode::All).unwrap();
        }
        let texts: Vec<&str> = s.entries.iter().map(|e| e.text.as_str()).collect();
        assert!(texts.contains(&"b"), "pinned entry must survive: {texts:?}");
        assert!(
            !texts.contains(&"a"),
            "oldest unpinned must be evicted: {texts:?}"
        );
        assert_eq!(s.len(), 4); // 3 cap + 1 pinned
    }

    #[test]
    fn update_mode_refreshes_lru_age() {
        let dir = tmpdir("update-lru");
        crate::config::Config {
            max_entries: 2,
            ..Default::default()
        }
        .save(&dir.join("config.toml"))
        .unwrap();

        let mut s = Store::open(&dir).unwrap();
        s.push_text("a", DedupMode::Update).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.push_text("b", DedupMode::Update).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.push_text("a", DedupMode::Update).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.push_text("c", DedupMode::Update).unwrap();

        let texts: Vec<&str> = s.entries.iter().map(|e| e.text.as_str()).collect();
        assert!(
            texts.contains(&"a"),
            "recently refreshed entry must survive: {texts:?}"
        );
        assert!(texts.contains(&"c"));
        assert!(
            !texts.contains(&"b"),
            "least-recently-used entry must be evicted: {texts:?}"
        );
    }

    #[test]
    fn delete_pin_clear_and_payload() {
        let dir = tmpdir("del");
        let mut s = Store::open(&dir).unwrap();
        let i1 = s.push_text("hello world", DedupMode::All).unwrap().unwrap();
        let _i2 = s.push_text("second", DedupMode::All).unwrap().unwrap();

        assert!(s.delete(i1));
        assert!(!s.delete(i1));
        assert!(s.get(i1).is_none());

        assert_eq!(s.toggle_pin(_i2), Some(true));
        assert_eq!(s.clear_unpinned(), 0, "only entry is pinned");
        assert_eq!(s.toggle_pin(_i2), Some(false));
        assert_eq!(s.clear_unpinned(), 1);

        let e = Entry::new_text(99, now_ms(), "payload");
        assert_eq!(s.payload_bytes(&e).unwrap(), b"payload");
    }

    #[test]
    fn corrupt_line_is_reported_not_swallowed() {
        let dir = tmpdir("corrupt");
        fs::write(dir.join(HISTORY_FILE), "{not json}\n").unwrap();
        let err = Store::open(&dir).unwrap_err().to_string();
        assert!(err.contains("corrupt history line"), "got: {err}");
    }

    #[test]
    fn blocking_open_does_not_retry_corrupt_history() {
        let dir = tmpdir("blocking-corrupt");
        fs::write(dir.join(HISTORY_FILE), "{not json}\n").unwrap();
        let err = Store::open_blocking(&dir, std::time::Duration::from_secs(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("corrupt history line"), "{err}");
    }

    #[test]
    fn rewrite_prunes_orphan_images_after_commit() {
        let dir = tmpdir("prune");
        let images = dir.join("images");
        fs::create_dir_all(&images).unwrap();
        fs::write(images.join("orphan.png"), b"not really png").unwrap();

        let s = Store::open(&dir).unwrap();
        s.rewrite().unwrap();
        assert!(!images.join("orphan.png").exists());
    }

    #[test]
    fn recovers_backup_if_history_is_missing() {
        let dir = tmpdir("recover");
        let path = dir.join(HISTORY_FILE);
        let backup = path.with_extension("jsonl.bak");
        let e = Entry::new_text(7, now_ms(), "restored");
        fs::write(&backup, format!("{}\n", serde_json::to_string(&e).unwrap())).unwrap();

        let s = Store::open(&dir).unwrap();
        assert_eq!(s.get(7).unwrap().text, "restored");
        assert!(path.exists());
    }

    #[test]
    fn second_lock_holder_fails_fast() {
        let dir = tmpdir("lock");
        let _a = Store::open(&dir).unwrap();
        assert!(
            Store::open(&dir).is_err(),
            "second exclusive lock must fail"
        );
    }
}
