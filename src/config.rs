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
        recover_interrupted_save(path)?;
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e.into()),
        };
        // Unknown keys are ignored so newer configs don't break older builds.
        let cfg: Config = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(toml::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?;
        }
        replace_file(&tmp, path)?;
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.min_length <= self.max_length,
            "min_length must not exceed max_length"
        );
        self.compile_denies()?;
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

fn recover_interrupted_save(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let backup = path.with_extension("toml.bak");
    let tmp = path.with_extension("toml.tmp");
    if backup.exists() {
        std::fs::rename(&backup, path)?;
    } else if tmp.exists() {
        std::fs::rename(&tmp, path)?;
    }
    Ok(())
}

fn replace_file(src: &Path, dst: &Path) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(src, dst)?;
        Ok(())
    }

    #[cfg(windows)]
    {
        let backup = dst.with_extension("toml.bak");
        let _ = std::fs::remove_file(&backup);
        if dst.exists() {
            std::fs::rename(dst, &backup)?;
        }
        match std::fs::rename(src, dst) {
            Ok(()) => {
                let _ = std::fs::remove_file(backup);
                Ok(())
            }
            Err(e) => {
                if backup.exists() {
                    let _ = std::fs::rename(&backup, dst);
                }
                Err(e.into())
            }
        }
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
    fn invalid_ranges_are_rejected() {
        let bad = Config {
            min_length: 10,
            max_length: 2,
            ..Default::default()
        };
        assert!(bad.validate().is_err());
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
