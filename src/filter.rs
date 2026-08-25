//! Decides whether a clipboard snapshot should be recorded.

use crate::config::Config;

pub struct Filter {
    denies: Vec<regex::Regex>,
    min_length: usize,
    max_length: usize,
}

impl Filter {
    pub fn new(cfg: &Config) -> anyhow::Result<Filter> {
        Ok(Filter {
            denies: cfg.compile_denies()?,
            min_length: cfg.min_length,
            max_length: cfg.max_length,
        })
    }

    /// `Ok(true)` = record, `Ok(false)` = ignore by rule, `Err` = bad config.
    pub fn accepts(&self, text: &str) -> anyhow::Result<bool> {
        let n = text.len();
        if n < self.min_length || n > self.max_length {
            return Ok(false);
        }
        for re in &self.denies {
            if re.is_match(text) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DedupMode;

    fn cfg(denies: &[&str]) -> Config {
        Config {
            deny_patterns: denies.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn length_bounds() {
        let f = Filter::new(&Config {
            min_length: 3,
            max_length: 10,
            ..Default::default()
        })
        .unwrap();
        assert!(!f.accepts("ab").unwrap());
        assert!(f.accepts("abc").unwrap());
        assert!(f.accepts("0123456789").unwrap());
        assert!(!f.accepts("0123456789x").unwrap());
    }

    #[test]
    fn secret_patterns_denied() {
        let f = Filter::new(&cfg(&["sk-[A-Za-z0-9]{8,}", "ghp_[A-Za-z0-9]{20,}"])).unwrap();
        assert!(!f.accepts("my key is sk-abcdefgh1234 ok").unwrap());
        assert!(!f.accepts("token ghp_abcdefghijklmnopqrst here").unwrap());
        assert!(f.accepts("harmless text").unwrap());
    }

    #[test]
    fn invalid_regex_is_error() {
        assert!(Filter::new(&cfg(&["(unclosed"])).is_err());
    }

    #[test]
    fn default_config_accepts_typical_text() {
        let f = Filter::new(&Config::default()).unwrap();
        let sample = "cargo build --release\n".repeat(50);
        assert_eq!(
            f.accepts(&sample).unwrap(),
            DedupMode::Bump == DedupMode::Bump
        );
        assert!(f.accepts(&sample).unwrap());
    }
}
