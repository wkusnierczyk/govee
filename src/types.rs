/// Opaque device identifier (wraps MAC address string internally).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(pub(crate) String);

/// A Govee device as seen by the library.
#[derive(Debug, Clone)]
pub struct Device {
    pub id: DeviceId,
    pub model: String,
    pub name: String,
    pub alias: Option<String>,
    pub backend: BackendType,
}

/// Which backend is active for a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendType {
    Cloud,
    Local,
}

/// Point-in-time device state.
#[derive(Debug, Clone)]
pub struct DeviceState {
    pub on: bool,
    pub brightness: u8,
    pub color: Color,
    pub color_temp_kelvin: Option<u32>,
    pub stale: bool,
}

/// RGB color value.
#[derive(Debug, Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}
