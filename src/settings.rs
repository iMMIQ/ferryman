use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_BATCH_SIZE: usize = 25;
pub const DEFAULT_CONTEXT_SEGMENTS: usize = 5;
pub const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 180;
pub const MAX_WEB_BATCH_SIZE: usize = 100;
pub const MAX_WEB_CONTEXT_SEGMENTS: usize = 50;

fn default_batch_size() -> usize {
    DEFAULT_BATCH_SIZE
}

fn default_context_segments() -> usize {
    DEFAULT_CONTEXT_SEGMENTS
}

fn cache_enabled_by_default() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TranslationSettings {
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_context_segments")]
    pub context_segments: usize,
    #[serde(default = "cache_enabled_by_default")]
    pub cache_enabled: bool,
}

impl Default for TranslationSettings {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            context_segments: DEFAULT_CONTEXT_SEGMENTS,
            cache_enabled: true,
        }
    }
}

impl TranslationSettings {
    pub fn validate_for_web(self) -> Result<Self> {
        if !(1..=MAX_WEB_BATCH_SIZE).contains(&self.batch_size) {
            bail!("batch size must be between 1 and {MAX_WEB_BATCH_SIZE}");
        }
        if self.context_segments > MAX_WEB_CONTEXT_SEGMENTS {
            bail!("context segments must be between 0 and {MAX_WEB_CONTEXT_SEGMENTS}");
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_defaults_match_cli_defaults() {
        let settings: TranslationSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, TranslationSettings::default());
    }

    #[test]
    fn web_bounds_reject_unsafe_values() {
        assert!(TranslationSettings {
            batch_size: 0,
            ..TranslationSettings::default()
        }
        .validate_for_web()
        .is_err());
        assert!(TranslationSettings {
            context_segments: MAX_WEB_CONTEXT_SEGMENTS + 1,
            ..TranslationSettings::default()
        }
        .validate_for_web()
        .is_err());
    }
}
