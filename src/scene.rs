use std::collections::HashMap;

use crate::error::{GoveeError, Result};
use crate::types::Color;

/// The color component of a scene: either an RGB value or a color temperature.
#[derive(Debug, Clone)]
pub enum SceneColor {
    Rgb(Color),
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
    /// - `name` must contain only alphanumeric characters, `-`, or `_`.
    pub fn new(name: &str, brightness: u8, color: SceneColor) -> Result<Self> {
        if brightness > 100 {
            return Err(GoveeError::InvalidBrightness(brightness));
        }

        if let SceneColor::Temp(temp) = &color
            && (*temp == 0 || *temp > 10000)
        {
            return Err(GoveeError::InvalidConfig(
                "color temp must be 1\u{2013}10000".to_string(),
            ));
        }

        if !name
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
    pub fn new() -> Self {
        let builtins = [
            Scene::new("warm", 40, SceneColor::Temp(2700)).unwrap(),
            Scene::new("focus", 80, SceneColor::Temp(5500)).unwrap(),
            Scene::new("night", 10, SceneColor::Rgb(Color::new(255, 0, 0))).unwrap(),
            Scene::new("movie", 20, SceneColor::Temp(2200)).unwrap(),
            Scene::new("bright", 100, SceneColor::Temp(6500)).unwrap(),
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

    /// Return all registered scenes.
    pub fn list(&self) -> Vec<&Scene> {
        self.scenes.values().collect()
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
    fn builtins_all_present() {
        let registry = SceneRegistry::new();
        let scenes = registry.list();
        assert_eq!(scenes.len(), 5);
        let names: Vec<&str> = scenes.iter().map(|s| s.name()).collect();
        assert!(names.contains(&"warm"));
        assert!(names.contains(&"focus"));
        assert!(names.contains(&"night"));
        assert!(names.contains(&"movie"));
        assert!(names.contains(&"bright"));
    }

    #[test]
    fn lookup_exact_name() {
        let registry = SceneRegistry::new();
        let scene = registry.get("warm").unwrap();
        assert_eq!(scene.name(), "warm");
        assert_eq!(scene.brightness(), 40);
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
    fn accept_valid_name_chars() {
        let result = Scene::new("my-Scene_01", 50, SceneColor::Temp(3000));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().name(), "my-Scene_01");
    }
}
