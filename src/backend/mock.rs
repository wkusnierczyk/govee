use async_trait::async_trait;

use crate::error::{GoveeError, Result};
use crate::types::{BackendType, Color, Device, DeviceId, DeviceState};

use super::GoveeBackend;

/// A configurable mock backend for testing trait consumers.
///
/// Devices and state are set via builder methods. `get_state` returns
/// `DeviceNotFound` when no state is configured. Setter methods always
/// return `Ok(())`.
pub(crate) struct MockBackend {
    devices: Vec<Device>,
    state: Option<DeviceState>,
    backend_type: BackendType,
}

impl MockBackend {
    pub(crate) fn new() -> Self {
        Self {
            devices: Vec::new(),
            state: None,
            backend_type: BackendType::Cloud,
        }
    }

    pub(crate) fn with_devices(mut self, devices: Vec<Device>) -> Self {
        self.devices = devices;
        self
    }

    pub(crate) fn with_state(mut self, state: DeviceState) -> Self {
        self.state = Some(state);
        self
    }

    pub(crate) fn with_backend_type(mut self, backend_type: BackendType) -> Self {
        self.backend_type = backend_type;
        self
    }
}

#[async_trait]
impl GoveeBackend for MockBackend {
    async fn list_devices(&self) -> Result<Vec<Device>> {
        Ok(self.devices.clone())
    }

    async fn get_state(&self, id: &DeviceId) -> Result<DeviceState> {
        self.state
            .clone()
            .ok_or_else(|| GoveeError::DeviceNotFound(id.to_string()))
    }

    async fn set_power(&self, _id: &DeviceId, _on: bool) -> Result<()> {
        Ok(())
    }

    async fn set_brightness(&self, _id: &DeviceId, _value: u8) -> Result<()> {
        Ok(())
    }

    async fn set_color(&self, _id: &DeviceId, _color: Color) -> Result<()> {
        Ok(())
    }

    async fn set_color_temp(&self, _id: &DeviceId, _kelvin: u32) -> Result<()> {
        Ok(())
    }

    fn backend_type(&self) -> BackendType {
        self.backend_type
    }
}
