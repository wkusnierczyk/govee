use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde::Deserialize;

use crate::error::Result;

/// Backend selection preference.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendPreference {
    #[default]
    Auto,
    #[serde(alias = "cloud")]
    CloudOnly,
    #[serde(alias = "local")]
    LocalOnly,
}

/// Library configuration.
///
/// Loaded from TOML. Consumer binaries are responsible for resolving the
/// config file path (conventionally `~/.config/govee/config.toml`).
#[derive(Clone, Deserialize)]
pub struct Config {
    /// Cloud API key. `None` means local-only mode.
    #[serde(default)]
    pub api_key: Option<String>,

    /// Backend selection preference.
    #[serde(default)]
    pub backend: BackendPreference,

    /// Local discovery interval in seconds.
    #[serde(default = "default_discovery_interval")]
    pub discovery_interval_secs: u64,

    /// User-defined aliases: alias → canonical device name.
    #[serde(default)]
    pub aliases: HashMap<String, String>,

    /// Device groups: group name → list of device names/aliases.
    #[serde(default)]
    pub groups: HashMap<String, Vec<String>>,
}

fn default_discovery_interval() -> u64 {
    60
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: None,
            backend: BackendPreference::Auto,
            discovery_interval_secs: default_discovery_interval(),
            aliases: HashMap::new(),
            groups: HashMap::new(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("backend", &self.backend)
            .field("discovery_interval_secs", &self.discovery_interval_secs)
            .field("aliases", &self.aliases)
            .field("groups", &self.groups)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default() {
        let cfg = Config::default();
        assert!(cfg.api_key.is_none());
        assert_eq!(cfg.backend, BackendPreference::Auto);
        assert_eq!(cfg.discovery_interval_secs, 60);
        assert!(cfg.aliases.is_empty());
        assert!(cfg.groups.is_empty());
    }

    #[test]
    fn config_parse_full() {
        let toml = r#"
            api_key = "gv-test-key-123"
            backend = "cloud"
            discovery_interval_secs = 30

            [aliases]
            bedroom = "H6078 Bedroom Light"
            kitchen = "H6076 Kitchen Strip"

            [groups]
            upstairs = ["bedroom"]
            all = ["bedroom", "kitchen"]
        "#;

        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.api_key.as_deref(), Some("gv-test-key-123"));
        assert_eq!(cfg.backend, BackendPreference::CloudOnly);
        assert_eq!(cfg.discovery_interval_secs, 30);
        assert_eq!(cfg.aliases.len(), 2);
        assert_eq!(cfg.aliases["bedroom"], "H6078 Bedroom Light");
        assert_eq!(cfg.groups["upstairs"], vec!["bedroom"]);
    }

    #[test]
    fn config_parse_minimal() {
        let toml = "";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.api_key.is_none());
        assert_eq!(cfg.backend, BackendPreference::Auto);
        assert_eq!(cfg.discovery_interval_secs, 60);
    }

    #[test]
    fn config_parse_local_only() {
        let toml = r#"backend = "local""#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.backend, BackendPreference::LocalOnly);
    }

    #[test]
    fn config_parse_invalid_toml() {
        let result: std::result::Result<Config, _> = toml::from_str("{{invalid");
        assert!(result.is_err());
    }

    #[test]
    fn config_debug_redacts_api_key() {
        let cfg = Config {
            api_key: Some("secret-key-12345".into()),
            ..Config::default()
        };
        let debug = format!("{:?}", cfg);
        assert!(!debug.contains("secret-key-12345"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn config_debug_shows_none_when_no_key() {
        let cfg = Config::default();
        let debug = format!("{:?}", cfg);
        assert!(debug.contains("None"));
    }

    #[test]
    fn config_load_missing_file() {
        let mut path = std::env::temp_dir();
        path.push("govee-test-nonexistent-config.toml");
        let result = Config::load(&path);
        assert!(result.is_err());
    }
}
