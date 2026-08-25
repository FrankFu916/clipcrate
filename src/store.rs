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
        Store::open_inner(dir)
    }

    /// Retry acquiring the exclusive lock for up to `wait` — for CLI
    /// commands racing against the watcher or sibling invocations.
    pub fn open_blocking(dir: &Path, wait: std::time::Duration) -> Result<Store> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            match Store::open_inner(dir) {
                Ok(s) => return Ok(s),
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }

    fn open_inner(dir: &Path) -> Result<Store> {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create data dir {}", dir.display()))?;
        let path = dir.join(HISTORY_FILE);

        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK_FILE))?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| {
            anyhow::anyhow!(
                "another clipcrate process holds the store lock ({})",
                dir.display()
            )
        })?;

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
            match self.entries.iter().position(|e| !e.pinned) {
                Some(idx) => {
                    self.entries.remove(idx);
                }
                None => break,
            }
        }
        drop(self.prune_orphan_images());
        self.rewrite()
    }

    /// Delete image files nothing references anymore.
    fn prune_orphan_images(&self) -> Result<()> {
        let img_dir = self.dir.join("images");
        let ok =
            |rd: std::io::Result<fs::DirEntry>| -> Option<PathBuf> { rd.ok().map(|e| e.path()) };
        for p in fs::read_dir(&img_dir)?.filter_map(ok) {
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
        fs::rename(&tmp, &self.path)?;
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
        if deleted {
            drop(self.prune_orphan_images());
        }
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
    fn second_lock_holder_fails_fast() {
        let dir = tmpdir("lock");
        let _a = Store::open(&dir).unwrap();
        assert!(
            Store::open(&dir).is_err(),
            "second exclusive lock must fail"
        );
    }
}
