//! Data model for a single clipboard entry.
//!
//! Text payloads are stored inline; image payloads are written to
//! `images/<hash>.png` next to the store file and referenced by relative path.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Kind of payload captured from the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Image,
}

/// One recorded clipboard event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    /// Unix timestamp in milliseconds.
    pub ts: i64,
    pub kind: Kind,
    /// UTF-8 text payload (empty for images).
    pub text: String,
    /// Byte length of the original payload.
    pub size: u64,
    #[serde(default)]
    pub pinned: bool,
}

impl Entry {
    /// Build a text entry. `now_ms` is injected so callers can pin timestamps.
    pub fn new_text(id: u64, ts: i64, text: impl Into<String>) -> Self {
        let text = text.into();
        let size = text.len() as u64;
        Entry {
            id,
            ts,
            kind: Kind::Text,
            text,
            size,
            pinned: false,
        }
    }

    /// Build an image entry pointing at a stored PNG file.
    pub fn new_image(id: u64, ts: i64, rel_path: impl Into<String>, size: u64) -> Self {
        Entry {
            id,
            ts,
            kind: Kind::Image,
            text: rel_path.into(),
            size,
            pinned: false,
        }
    }

    /// Single-line summary used by list views: first line only, control
    /// characters stripped, hard-truncated to `max` chars.
    pub fn preview(&self, max: usize) -> String {
        match self.kind {
            Kind::Image => return "[image]".to_string(),
            Kind::Text => {}
        }
        let mut line = self.text.lines().next().unwrap_or("").to_string();
        line.retain(|c| !c.is_control());
        if line.chars().count() > max {
            let cut: String = line.chars().take(max.saturating_sub(1)).collect();
            format!("{cut}…")
        } else {
            line
        }
    }

    pub fn is_multiline(&self) -> bool {
        matches!(self.kind, Kind::Text) && self.text.contains('\n')
    }

    /// Human-readable age like "3s", "5m", "2h", "4d".
    pub fn age(&self, now_ms: i64) -> String {
        let secs = ((now_ms - self.ts).max(0) / 1000) as u64;
        match secs {
            0..=59 => format!("{secs}s"),
            60..=3599 => format!("{}m", secs / 60),
            3600..=86_399 => format!("{}h", secs / 3600),
            _ => format!("{}d", secs / 86_400),
        }
    }

    /// Size rendered for humans ("12 B", "3.4 KiB", ...).
    pub fn human_size(&self) -> String {
        let units = ["B", "KiB", "MiB", "GiB"];
        let mut v = self.size as f64;
        let mut u = 0;
        while v >= 1024.0 && u < units.len() - 1 {
            v /= 1024.0;
            u += 1;
        }
        if u == 0 {
            format!("{} B", self.size)
        } else {
            format!("{v:.1} {}", units[u])
        }
    }

    /// Resolve the absolute path of the payload for image entries.
    pub fn payload_path(&self, store_dir: &Path) -> Option<PathBuf> {
        match self.kind {
            Kind::Text => None,
            Kind::Image => Some(store_dir.join(&self.text)),
        }
    }
}

/// Current wall-clock time as Unix milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
