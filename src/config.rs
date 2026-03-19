use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{GoveeError, Result};
use crate::types::Color;

/// Backend selection preference.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendPreference {
    #[default]
    Auto,
    #[serde(alias = "cloud")]
    CloudOnly,
    #[serde(alias = "local")]
    LocalOnly,
}

/// Minimum allowed discovery interval in seconds.
pub const MIN_DISCOVERY_INTERVAL_SECS: u64 = 5;

/// A user-defined scene loaded from the config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneConfig {
    /// Brightness 0–100.
    pub brightness: u8,
    /// RGB color (mutually exclusive with `color_temp`).
    pub color: Option<Color>,
    /// Color temperature in Kelvin (mutually exclusive with `color`).
    pub color_temp: Option<u32>,
}

/// Library configuration.
///
/// Loaded from TOML. Consumer binaries are responsible for resolving the
/// config file path (conventionally `~/.config/govee/config.toml`).
///
/// All construction paths (`new`, `load`, `Deserialize`) validate that
/// `discovery_interval_secs >= 5`.
#[derive(Clone)]
pub struct Config {
    api_key: Option<String>,
    backend: BackendPreference,
    discovery_interval_secs: u64,
    aliases: HashMap<String, String>,
    groups: HashMap<String, Vec<String>>,
    scenes: HashMap<String, SceneConfig>,
}

impl Serialize for Config {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Config", 6)?;
        // RT-01: never serialize the API key — redact as null.
        state.serialize_field("api_key", &None::<String>)?;
        state.serialize_field("backend", &self.backend)?;
        state.serialize_field("discovery_interval_secs", &self.discovery_interval_secs)?;
        state.serialize_field("aliases", &self.aliases)?;
        state.serialize_field("groups", &self.groups)?;
        state.serialize_field("scenes", &self.scenes)?;
        state.end()
    }
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
            scenes: HashMap::new(),
        }
    }
}

impl Config {
    /// Create a new `Config`, validating all fields.
    pub fn new(
        api_key: Option<String>,
        backend: BackendPreference,
        discovery_interval_secs: u64,
        aliases: HashMap<String, String>,
        groups: HashMap<String, Vec<String>>,
        scenes: HashMap<String, SceneConfig>,
    ) -> Result<Self> {
        let config = Self {
            api_key,
            backend,
            discovery_interval_secs,
            aliases,
            groups,
            scenes,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validate config values, returning `GoveeError::InvalidConfig` on failure.
    pub fn validate(&self) -> Result<()> {
        if self.discovery_interval_secs < MIN_DISCOVERY_INTERVAL_SECS {
            return Err(GoveeError::InvalidConfig(format!(
                "discovery_interval_secs must be >= {}s, got {}s",
                MIN_DISCOVERY_INTERVAL_SECS, self.discovery_interval_secs
            )));
        }

        for (name, sc) in &self.scenes {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            {
                return Err(GoveeError::InvalidConfig(format!(
                    "scene \"{name}\": name must be non-empty and contain only alphanumeric, '-', '_' characters"
                )));
            }

            if sc.brightness > 100 {
                return Err(GoveeError::InvalidConfig(format!(
                    "scene \"{name}\": brightness must be 0\u{2013}100, got {}",
                    sc.brightness
                )));
            }

            match (&sc.color, sc.color_temp) {
                (Some(_), Some(_)) => {
                    return Err(GoveeError::InvalidConfig(format!(
                        "scene \"{name}\": must set exactly one of color or color_temp, not both"
                    )));
                }
                (None, None) => {
                    return Err(GoveeError::InvalidConfig(format!(
                        "scene \"{name}\": must set exactly one of color or color_temp"
                    )));
                }
                (None, Some(temp)) if temp == 0 || temp > 10000 => {
                    return Err(GoveeError::InvalidConfig(format!(
                        "scene \"{name}\": color_temp must be 1\u{2013}10000, got {temp}"
                    )));
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Load configuration from a TOML file.
    ///
    /// Returns `GoveeError::Io` if the file cannot be read,
    /// `GoveeError::Config` for TOML syntax errors, or
    /// `GoveeError::InvalidConfig` for out-of-range values.
    pub fn load(path: &Path) -> Result<Self> {
        // RT-04: warn if config file has loose permissions (Unix only).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(path) {
                let mode = meta.permissions().mode();
                if mode & 0o077 != 0 {
                    tracing::warn!(
                        path = %path.display(),
                        mode = format_args!("{:04o}", mode & 0o777),
                        "config file has loose permissions, recommend 0600"
                    );
                }
            }
        }

        let content = std::fs::read_to_string(path)?;
        // Parse TOML (syntax errors → GoveeError::Config)
        let config: Config = toml::from_str(&content)?;
        // Re-validate to surface as GoveeError::InvalidConfig
        config.validate()?;
        Ok(config)
    }

    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    pub fn backend(&self) -> BackendPreference {
        self.backend
    }

    pub fn discovery_interval_secs(&self) -> u64 {
        self.discovery_interval_secs
    }

    pub fn aliases(&self) -> &HashMap<String, String> {
        &self.aliases
    }

    pub fn groups(&self) -> &HashMap<String, Vec<String>> {
        &self.groups
    }

    pub fn scenes(&self) -> &HashMap<String, SceneConfig> {
        &self.scenes
    }
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            api_key: Option<String>,
            #[serde(default)]
            backend: BackendPreference,
            #[serde(default = "default_discovery_interval")]
            discovery_interval_secs: u64,
            #[serde(default)]
            aliases: HashMap<String, String>,
            #[serde(default)]
            groups: HashMap<String, Vec<String>>,
            #[serde(default)]
            scenes: HashMap<String, SceneConfig>,
        }

        let raw = Raw::deserialize(deserializer)?;
        let config = Config {
            api_key: raw.api_key,
            backend: raw.backend,
            discovery_interval_secs: raw.discovery_interval_secs,
            aliases: raw.aliases,
            groups: raw.groups,
            scenes: raw.scenes,
        };
        config.validate().map_err(serde::de::Error::custom)?;
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
            .field("scenes", &self.scenes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default() {
        let cfg = Config::default();
        assert!(cfg.api_key().is_none());
        assert_eq!(cfg.backend(), BackendPreference::Auto);
        assert_eq!(cfg.discovery_interval_secs(), 60);
        assert!(cfg.aliases().is_empty());
        assert!(cfg.groups().is_empty());
    }

    #[test]
    fn config_new_valid() {
        let cfg = Config::new(
            Some("key".into()),
            BackendPreference::CloudOnly,
            30,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(cfg.api_key(), Some("key"));
        assert_eq!(cfg.discovery_interval_secs(), 30);
    }

    #[test]
    fn config_new_invalid_interval() {
        let result = Config::new(
            None,
            BackendPreference::Auto,
            2,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, GoveeError::InvalidConfig(_)));
        assert!(err.to_string().contains("must be >= 5s"));
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
        assert_eq!(cfg.api_key(), Some("gv-test-key-123"));
        assert_eq!(cfg.backend(), BackendPreference::CloudOnly);
        assert_eq!(cfg.discovery_interval_secs(), 30);
        assert_eq!(cfg.aliases().len(), 2);
        assert_eq!(cfg.aliases()["bedroom"], "H6078 Bedroom Light");
        assert_eq!(cfg.groups()["upstairs"], vec!["bedroom"]);
    }

    #[test]
    fn config_parse_minimal() {
        let toml = "";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.api_key().is_none());
        assert_eq!(cfg.backend(), BackendPreference::Auto);
        assert_eq!(cfg.discovery_interval_secs(), 60);
    }

    #[test]
    fn config_parse_local_only() {
        let toml = r#"backend = "local""#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.backend(), BackendPreference::LocalOnly);
    }

    #[test]
    fn config_parse_invalid_toml() {
        let result: std::result::Result<Config, _> = toml::from_str("{{invalid");
        assert!(result.is_err());
    }

    #[test]
    fn config_debug_redacts_api_key() {
        let cfg = Config::new(
            Some("secret-key-12345".into()),
            BackendPreference::Auto,
            60,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        )
        .unwrap();
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

    // Discovery interval validation

    #[test]
    fn config_discovery_interval_at_minimum() {
        let toml = "discovery_interval_secs = 5";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.discovery_interval_secs(), 5);
    }

    #[test]
    fn config_discovery_interval_below_minimum() {
        let toml = "discovery_interval_secs = 4";
        let result: std::result::Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn config_discovery_interval_zero() {
        let toml = "discovery_interval_secs = 0";
        let result: std::result::Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    // Scene config validation

    #[test]
    fn config_with_scenes_parses() {
        let toml = r#"
            [scenes.reading]
            brightness = 70
            color_temp = 4000

            [scenes.party]
            brightness = 100
            color = { r = 255, g = 0, b = 128 }
        "#;

        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.scenes().len(), 2);
        let reading = &cfg.scenes()["reading"];
        assert_eq!(reading.brightness, 70);
        assert_eq!(reading.color_temp, Some(4000));
        assert!(reading.color.is_none());

        let party = &cfg.scenes()["party"];
        assert_eq!(party.brightness, 100);
        assert_eq!(party.color, Some(Color::new(255, 0, 128)));
        assert!(party.color_temp.is_none());
    }

    #[test]
    fn config_scene_both_color_and_temp_rejected() {
        let toml = r#"
            [scenes.bad]
            brightness = 50
            color = { r = 255, g = 0, b = 0 }
            color_temp = 3000
        "#;
        let result: std::result::Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn config_scene_neither_color_nor_temp_rejected() {
        let toml = r#"
            [scenes.bad]
            brightness = 50
        "#;
        let result: std::result::Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn config_scene_brightness_over_100_rejected() {
        let toml = r#"
            [scenes.bad]
            brightness = 101
            color_temp = 3000
        "#;
        let result: std::result::Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn config_scene_color_temp_out_of_range_rejected() {
        let toml = r#"
            [scenes.bad]
            brightness = 50
            color_temp = 0
        "#;
        let result: std::result::Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());

        let toml = r#"
            [scenes.bad]
            brightness = 50
            color_temp = 10001
        "#;
        let result: std::result::Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn config_new_with_scenes() {
        let mut scenes = HashMap::new();
        scenes.insert(
            "cozy".to_string(),
            SceneConfig {
                brightness: 30,
                color: None,
                color_temp: Some(2700),
            },
        );
        let cfg = Config::new(
            None,
            BackendPreference::Auto,
            60,
            HashMap::new(),
            HashMap::new(),
            scenes,
        )
        .unwrap();
        assert_eq!(cfg.scenes().len(), 1);
        assert_eq!(cfg.scenes()["cozy"].brightness, 30);
    }
}
