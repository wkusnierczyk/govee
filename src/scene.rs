use std::collections::HashMap;

use crate::config::SceneConfig;
use crate::error::{GoveeError, Result};
use crate::types::{Color, DeviceId};

/// Target specification for applying a scene.
#[derive(Debug, Clone)]
pub enum SceneTarget {
    /// A specific device by ID.
    Device(DeviceId),
    /// A device resolved by name or alias.
    DeviceName(String),
    /// All devices in a named group.
    Group(String),
    /// Every registered device.
    All,
}

/// The color component of a scene: either an RGB value or a color temperature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneColor {
    /// An RGB color value.
    Rgb(Color),
    /// A color temperature in Kelvin (1–10000).
    Temp(u32),
}

/// A named lighting preset with brightness and color.
#[derive(Debug, Clone)]
pub struct Scene {
    name: String,
    brightness: u8,
    color: SceneColor,
}

impl Scene {
    /// Create a new scene, validating all fields.
    ///
    /// - `brightness` must be 0–100.
    /// - `SceneColor::Temp` value must be 1–10000.
    /// - `name` must be non-empty and contain only alphanumeric characters, `-`, or `_`.
    pub fn new(name: &str, brightness: u8, color: SceneColor) -> Result<Self> {
        if brightness > 100 {
            return Err(GoveeError::InvalidBrightness(brightness));
        }

        if let SceneColor::Temp(temp) = &color
            && (*temp == 0 || *temp > 10000)
        {
            return Err(GoveeError::InvalidConfig(
                "color temp must be 1-10000".to_string(),
            ));
        }

        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(GoveeError::InvalidConfig(
                "scene name must contain only alphanumeric, '-', '_' characters".to_string(),
            ));
        }

        Ok(Self {
            name: name.to_string(),
            brightness,
            color,
        })
    }

    /// Return the scene name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the scene brightness (0–100).
    pub fn brightness(&self) -> u8 {
        self.brightness
    }

    /// Return a reference to the scene color.
    pub fn color(&self) -> &SceneColor {
        &self.color
    }
}

/// A registry of named lighting scenes with case-insensitive lookup.
#[derive(Debug)]
pub struct SceneRegistry {
    scenes: HashMap<String, Scene>,
}

impl SceneRegistry {
    /// Create a new registry populated with built-in scenes.
    ///
    /// Built-ins are constructed directly (no validation) since their
    /// values are compile-time constants known to be valid.
    pub fn new() -> Self {
        let builtins = [
            Scene {
                name: "warm".into(),
                brightness: 40,
                color: SceneColor::Temp(2700),
            },
            Scene {
                name: "focus".into(),
                brightness: 80,
                color: SceneColor::Temp(5500),
            },
            Scene {
                name: "night".into(),
                brightness: 10,
                color: SceneColor::Rgb(Color::new(255, 0, 0)),
            },
            Scene {
                name: "movie".into(),
                brightness: 20,
                color: SceneColor::Temp(2200),
            },
            Scene {
                name: "bright".into(),
                brightness: 100,
                color: SceneColor::Temp(6500),
            },
        ];

        let mut scenes = HashMap::new();
        for scene in builtins {
            scenes.insert(scene.name().to_lowercase(), scene);
        }

        Self { scenes }
    }

    /// Look up a scene by name (case-insensitive).
    pub fn get(&self, name: &str) -> Result<&Scene> {
        self.scenes
            .get(&name.to_lowercase())
            .ok_or_else(|| GoveeError::DeviceNotFound(format!("scene: {name}")))
    }

    /// Return all registered scenes, sorted by name.
    pub fn list(&self) -> Vec<&Scene> {
        let mut scenes: Vec<_> = self.scenes.values().collect();
        scenes.sort_by_key(|s| s.name());
        scenes
    }

    /// Merge user-defined scenes from config into this registry.
    ///
    /// - Converts each `SceneConfig` to a `Scene` via `Scene::new()`.
    /// - Keys are lowercased for case-insensitive storage.
    /// - On name collision with a built-in, the user scene wins (logged at debug).
    /// - On case-insensitive collision between user scenes, last-wins (logged at warn).
    pub fn with_user_scenes(mut self, user: &HashMap<String, SceneConfig>) -> Result<Self> {
        // Track built-in keys to distinguish overrides from user/user collisions.
        let builtin_keys: std::collections::HashSet<String> = self.scenes.keys().cloned().collect();
        // Track keys inserted from user scenes in this merge.
        let mut user_keys = std::collections::HashSet::new();

        // Sort user scene names for deterministic iteration order.
        let mut sorted_names: Vec<&String> = user.keys().collect();
        sorted_names.sort();

        for name in sorted_names {
            let sc = &user[name];
            let color = match (&sc.color, sc.color_temp) {
                (Some(c), None) => SceneColor::Rgb(*c),
                (None, Some(temp)) => SceneColor::Temp(temp),
                _ => {
                    return Err(GoveeError::InvalidConfig(format!(
                        "scene \"{name}\": must set exactly one of color or color_temp"
                    )));
                }
            };

            let scene = Scene::new(name, sc.brightness, color)
                .map_err(|e| GoveeError::InvalidConfig(format!("scene \"{name}\": {e}")))?;
            let key = name.to_lowercase();

            if self.scenes.contains_key(&key) {
                if user_keys.contains(&key) {
                    // Collision between two user scenes (case-insensitive).
                    tracing::warn!(scene = %name, "case-insensitive collision with existing user scene");
                } else if builtin_keys.contains(&key) {
                    // User scene overriding a built-in.
                    tracing::debug!(scene = %name, "user scene overrides built-in");
                }
            }

            user_keys.insert(key.clone());
            self.scenes.insert(key, scene);
        }

        Ok(self)
    }
}

impl Default for SceneRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_all_present_with_values() {
        let registry = SceneRegistry::new();
        let scenes = registry.list();
        assert_eq!(scenes.len(), 5);

        // list() is sorted by name.
        assert_eq!(scenes[0].name(), "bright");
        assert_eq!(scenes[0].brightness(), 100);
        assert_eq!(*scenes[0].color(), SceneColor::Temp(6500));

        assert_eq!(scenes[1].name(), "focus");
        assert_eq!(scenes[1].brightness(), 80);
        assert_eq!(*scenes[1].color(), SceneColor::Temp(5500));

        assert_eq!(scenes[2].name(), "movie");
        assert_eq!(scenes[2].brightness(), 20);
        assert_eq!(*scenes[2].color(), SceneColor::Temp(2200));

        assert_eq!(scenes[3].name(), "night");
        assert_eq!(scenes[3].brightness(), 10);
        assert_eq!(*scenes[3].color(), SceneColor::Rgb(Color::new(255, 0, 0)));

        assert_eq!(scenes[4].name(), "warm");
        assert_eq!(scenes[4].brightness(), 40);
        assert_eq!(*scenes[4].color(), SceneColor::Temp(2700));
    }

    #[test]
    fn lookup_exact_name() {
        let registry = SceneRegistry::new();
        let scene = registry.get("warm").unwrap();
        assert_eq!(scene.name(), "warm");
        assert_eq!(scene.brightness(), 40);
        assert_eq!(*scene.color(), SceneColor::Temp(2700));
    }

    #[test]
    fn lookup_case_insensitive() {
        let registry = SceneRegistry::new();
        assert!(registry.get("WARM").is_ok());
        assert!(registry.get("Warm").is_ok());
        assert!(registry.get("wArM").is_ok());
        assert_eq!(registry.get("FOCUS").unwrap().name(), "focus");
    }

    #[test]
    fn lookup_unknown_scene() {
        let registry = SceneRegistry::new();
        let err = registry.get("nonexistent").unwrap_err();
        assert!(matches!(err, GoveeError::DeviceNotFound(_)));
    }

    #[test]
    fn reject_brightness_over_100() {
        let result = Scene::new("test", 101, SceneColor::Temp(3000));
        assert!(matches!(result, Err(GoveeError::InvalidBrightness(101))));
    }

    #[test]
    fn reject_temp_zero() {
        let result = Scene::new("test", 50, SceneColor::Temp(0));
        assert!(matches!(result, Err(GoveeError::InvalidConfig(_))));
    }

    #[test]
    fn reject_temp_over_10000() {
        let result = Scene::new("test", 50, SceneColor::Temp(10001));
        assert!(matches!(result, Err(GoveeError::InvalidConfig(_))));
    }

    #[test]
    fn reject_name_with_newline() {
        let result = Scene::new("bad\nname", 50, SceneColor::Temp(3000));
        assert!(matches!(result, Err(GoveeError::InvalidConfig(_))));
    }

    #[test]
    fn reject_name_with_space() {
        let result = Scene::new("bad name", 50, SceneColor::Temp(3000));
        assert!(matches!(result, Err(GoveeError::InvalidConfig(_))));
    }

    #[test]
    fn reject_empty_name() {
        let result = Scene::new("", 50, SceneColor::Temp(3000));
        assert!(matches!(result, Err(GoveeError::InvalidConfig(_))));
    }

    #[test]
    fn accept_valid_name_chars() {
        let result = Scene::new("my-Scene_01", 50, SceneColor::Temp(3000));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().name(), "my-Scene_01");
    }

    #[test]
    fn user_scene_loaded_via_with_user_scenes() {
        let mut user = HashMap::new();
        user.insert(
            "cozy".to_string(),
            SceneConfig {
                brightness: 30,
                color: Some(Color::new(255, 200, 100)),
                color_temp: None,
            },
        );

        let registry = SceneRegistry::new().with_user_scenes(&user).unwrap();
        let scene = registry.get("cozy").unwrap();
        assert_eq!(scene.name(), "cozy");
        assert_eq!(scene.brightness(), 30);
        assert_eq!(*scene.color(), SceneColor::Rgb(Color::new(255, 200, 100)));
    }

    #[test]
    fn user_scene_neither_color_nor_temp_rejected() {
        let mut user = HashMap::new();
        user.insert(
            "bad".to_string(),
            SceneConfig {
                brightness: 50,
                color: None,
                color_temp: None,
            },
        );
        let err = SceneRegistry::new().with_user_scenes(&user).unwrap_err();
        assert!(matches!(err, crate::error::GoveeError::InvalidConfig(_)));
    }

    #[test]
    fn user_scene_collision_between_two_user_scenes() {
        let mut user = HashMap::new();
        user.insert(
            "Cozy".to_string(),
            SceneConfig {
                brightness: 30,
                color: Some(Color::new(255, 200, 100)),
                color_temp: None,
            },
        );
        user.insert(
            "cozy".to_string(),
            SceneConfig {
                brightness: 80,
                color: Some(Color::new(100, 100, 255)),
                color_temp: None,
            },
        );
        // Both entries are valid; last-wins for case-insensitive collision.
        let registry = SceneRegistry::new().with_user_scenes(&user).unwrap();
        assert!(registry.get("cozy").is_ok());
    }

    #[test]
    fn user_scene_overrides_builtin() {
        let mut user = HashMap::new();
        user.insert(
            "warm".to_string(),
            SceneConfig {
                brightness: 80,
                color: None,
                color_temp: Some(3000),
            },
        );

        let registry = SceneRegistry::new().with_user_scenes(&user).unwrap();
        let scene = registry.get("warm").unwrap();
        assert_eq!(scene.brightness(), 80);
        assert_eq!(*scene.color(), SceneColor::Temp(3000));
    }

    #[test]
    fn user_color_temp_scene() {
        let mut user = HashMap::new();
        user.insert(
            "daylight".to_string(),
            SceneConfig {
                brightness: 100,
                color: None,
                color_temp: Some(6500),
            },
        );

        let registry = SceneRegistry::new().with_user_scenes(&user).unwrap();
        let scene = registry.get("daylight").unwrap();
        assert_eq!(scene.brightness(), 100);
        assert_eq!(*scene.color(), SceneColor::Temp(6500));
    }

    #[test]
    fn user_scene_case_insensitive_collision_last_wins() {
        // Two user scenes differing only by case. Sorted iteration
        // means "Cozy" comes before "cozy" — "cozy" wins.
        let mut user = HashMap::new();
        user.insert(
            "Cozy".to_string(),
            SceneConfig {
                brightness: 30,
                color: None,
                color_temp: Some(2700),
            },
        );
        user.insert(
            "cozy".to_string(),
            SceneConfig {
                brightness: 50,
                color: None,
                color_temp: Some(3000),
            },
        );

        let registry = SceneRegistry::new().with_user_scenes(&user).unwrap();
        let scene = registry.get("cozy").unwrap();
        // "cozy" (lowercase) sorts after "Cozy" (uppercase), so it wins.
        assert_eq!(scene.brightness(), 50);
        assert_eq!(*scene.color(), SceneColor::Temp(3000));
    }

    #[test]
    fn scene_registry_default_equals_new() {
        let default = SceneRegistry::default();
        let new = SceneRegistry::new();
        assert_eq!(default.scenes.len(), new.scenes.len());
    }
}
