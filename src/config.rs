//! User configuration, loaded from `config.toml` in the data directory.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How the watcher deduplicates repeated copies of the same content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DedupMode {
    /// Move the existing entry to the top instead of creating a new one.
    #[default]
    Bump,
    /// Keep the original position; only update the timestamp.
    Update,
    /// Record every copy as a separate entry.
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Maximum entries kept (excluding pinned ones).
    pub max_entries: usize,
    /// Polling interval of the watcher in milliseconds.
    pub poll_ms: u64,
    /// Ignore clips shorter than this many bytes.
    pub min_length: usize,
    /// Ignore clips longer than this many bytes.
    pub max_length: usize,
    /// Regex patterns; matching clips are not recorded.
    pub deny_patterns: Vec<String>,
    pub dedup: DedupMode,
    /// Number of preview lines shown in the picker.
    pub preview_lines: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_entries: 1000,
            poll_ms: 700,
            min_length: 1,
            max_length: 1_000_000,
            deny_patterns: Vec::new(),
            dedup: DedupMode::Bump,
            preview_lines: 8,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return Ok(Config::default()),
        };
        // Unknown keys are ignored so newer configs don't break older builds.
        let cfg: Config = toml::from_str(&raw)?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Compile deny patterns once; bad regexes are reported to the caller.
    pub fn compile_denies(&self) -> anyhow::Result<Vec<regex::Regex>> {
        self.deny_patterns
            .iter()
            .map(|p| Ok(regex::Regex::new(p)?))
            .collect()
    }

    /// Data directory: `$CLIPCRATE_HOME` override, else platform data dir.
    pub fn data_dir() -> PathBuf {
        if let Ok(p) = std::env::var("CLIPCRATE_HOME") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        directories::ProjectDirs::from("dev", "", "clipcrate")
            .map(|d| d.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".clipcrate"))
    }

    pub fn default_path() -> PathBuf {
        Self::data_dir().join("config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_through_toml() {
        let dir = tempfile_dir();
        let p = dir.join("config.toml");
        Config::default().save(&p).unwrap();
        let loaded = Config::load(&p).unwrap();
        assert_eq!(loaded.max_entries, 1000);
        assert_eq!(loaded.poll_ms, 700);
        assert_eq!(loaded.dedup, DedupMode::Bump);
    }

    #[test]
    fn unknown_keys_and_missing_file_are_tolerated() {
        let dir = tempfile_dir();
        let p = dir.join("c.toml");
        std::fs::write(&p, "max_entries = 42\nfuture_key = true\n").unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.max_entries, 42);
        assert_eq!(cfg.poll_ms, 700);

        let missing = Config::load(&dir.join("nope.toml")).unwrap();
        assert_eq!(missing, Config::default());
    }

    #[test]
    fn deny_patterns_compile_or_fail() {
        let bad = Config {
            deny_patterns: vec!["sk-[a-zA-Z0-9]{10,}".into(), "(bad".into()],
            ..Default::default()
        };
        assert!(bad.compile_denies().is_err());
        let good = Config {
            deny_patterns: vec!["sk-[a-zA-Z0-9]{10,}".into()],
            ..Default::default()
        };
        assert_eq!(good.compile_denies().unwrap().len(), 1);
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("clipcrate-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
