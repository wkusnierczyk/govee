use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{GoveeError, Result};

/// Opaque device identifier (wraps MAC address string internally).
///
/// Accepts colon-separated hex MAC addresses with 6 or 8 octets
/// (e.g., `"AA:BB:CC:DD:EE:FF"` or `"AA:BB:CC:DD:EE:FF:00:11"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub(crate) String);

impl DeviceId {
    /// Validate and create a new `DeviceId` from a MAC address string.
    pub fn new(mac: &str) -> Result<Self> {
        let id: DeviceId = mac.parse()?;
        Ok(id)
    }

    /// Return the inner MAC address string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for DeviceId {
    type Err = GoveeError;

    fn from_str(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        let valid_len = parts.len() == 6 || parts.len() == 8;
        let valid_hex = parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));

        if valid_len && valid_hex {
            Ok(DeviceId(s.to_uppercase()))
        } else {
            Err(GoveeError::InvalidDeviceId(s.to_string()))
        }
    }
}

/// A Govee device as seen by the library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub model: String,
    pub name: String,
    pub alias: Option<String>,
    pub backend: BackendType,
}

/// Which backend is active for a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendType {
    Cloud,
    Local,
}

impl fmt::Display for BackendType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendType::Cloud => write!(f, "cloud"),
            BackendType::Local => write!(f, "local"),
        }
    }
}

/// Point-in-time device state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceState {
    pub on: bool,
    pub brightness: u8,
    pub color: Color,
    pub color_temp_kelvin: Option<u32>,
    pub stale: bool,
}

impl DeviceState {
    /// Create a new `DeviceState`, validating brightness is 0–100.
    pub fn new(
        on: bool,
        brightness: u8,
        color: Color,
        color_temp_kelvin: Option<u32>,
        stale: bool,
    ) -> Result<Self> {
        if brightness > 100 {
            return Err(GoveeError::InvalidBrightness(brightness));
        }
        Ok(Self {
            on,
            brightness,
            color,
            color_temp_kelvin,
            stale,
        })
    }
}

/// RGB color value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // DeviceId tests

    #[test]
    fn device_id_valid_6_octet() {
        let id = DeviceId::new("aa:bb:cc:dd:ee:ff").unwrap();
        assert_eq!(id.as_str(), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn device_id_valid_8_octet() {
        let id = DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();
        assert_eq!(id.as_str(), "AA:BB:CC:DD:EE:FF:00:11");
    }

    #[test]
    fn device_id_normalizes_to_uppercase() {
        let id = DeviceId::new("ab:cd:ef:01:23:45").unwrap();
        assert_eq!(id.to_string(), "AB:CD:EF:01:23:45");
    }

    #[test]
    fn device_id_invalid_format() {
        assert!(DeviceId::new("not-a-mac").is_err());
        assert!(DeviceId::new("AA:BB:CC").is_err());
        assert!(DeviceId::new("GG:HH:II:JJ:KK:LL").is_err());
        assert!(DeviceId::new("AA:BB:CC:DD:EE:FF:00").is_err()); // 7 octets
    }

    #[test]
    fn device_id_equality() {
        let a = DeviceId::new("aa:bb:cc:dd:ee:ff").unwrap();
        let b = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn device_id_from_str() {
        let id: DeviceId = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        assert_eq!(id.as_str(), "AA:BB:CC:DD:EE:FF");
    }

    // Brightness validation tests

    #[test]
    fn device_state_valid_brightness() {
        let state = DeviceState::new(true, 50, Color::new(255, 0, 0), None, false);
        assert!(state.is_ok());
        assert_eq!(state.unwrap().brightness, 50);
    }

    #[test]
    fn device_state_brightness_boundary() {
        assert!(DeviceState::new(true, 0, Color::new(0, 0, 0), None, false).is_ok());
        assert!(DeviceState::new(true, 100, Color::new(0, 0, 0), None, false).is_ok());
        assert!(DeviceState::new(true, 101, Color::new(0, 0, 0), None, false).is_err());
    }

    // Color tests

    #[test]
    fn color_display() {
        let c = Color::new(255, 128, 0);
        assert_eq!(c.to_string(), "#FF8000");
    }

    // BackendType tests

    #[test]
    fn backend_type_display() {
        assert_eq!(BackendType::Cloud.to_string(), "cloud");
        assert_eq!(BackendType::Local.to_string(), "local");
    }
}
