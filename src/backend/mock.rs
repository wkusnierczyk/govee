use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::{GoveeError, Result};
use crate::types::{
    BackendType, Color, Device, DeviceId, DeviceState, DiyScene, LightScene, WorkMode,
};

use super::GoveeBackend;

/// A configurable mock backend for testing trait consumers.
///
/// Devices and state are set via builder methods. `get_state` returns
/// `DeviceNotFound` when no state is configured. Setter methods always
/// return `Ok(())` unless an error is injected via [`with_error`].
pub(crate) struct MockBackend {
    devices: Vec<Device>,
    state: Option<DeviceState>,
    backend_type: BackendType,
    /// If set, all setter and get_state calls return this error.
    /// Uses `Mutex<Option<...>>` for thread-safe optional configuration.
    injected_error: Mutex<Option<InjectedError>>,
}

/// Controls how injected errors behave.
enum InjectedError {
    /// Return the error on every call.
    Persistent(fn() -> GoveeError),
}

impl MockBackend {
    pub(crate) fn new() -> Self {
        Self {
            devices: Vec::new(),
            state: None,
            backend_type: BackendType::Cloud,
            injected_error: Mutex::new(None),
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

    /// Inject a persistent error: all setter and get_state calls will fail.
    pub(crate) fn with_error(self, error_fn: fn() -> GoveeError) -> Self {
        *self.injected_error.lock().unwrap() = Some(InjectedError::Persistent(error_fn));
        self
    }

    /// Check if an error should be returned.
    fn check_error(&self) -> Result<()> {
        let guard = self.injected_error.lock().unwrap();
        match &*guard {
            Some(InjectedError::Persistent(f)) => Err(f()),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl GoveeBackend for MockBackend {
    async fn list_devices(&self) -> Result<Vec<Device>> {
        Ok(self.devices.clone())
    }

    async fn get_state(&self, id: &DeviceId) -> Result<DeviceState> {
        self.check_error()?;
        self.state
            .clone()
            .ok_or_else(|| GoveeError::DeviceNotFound(id.to_string()))
    }

    async fn set_power(&self, _id: &DeviceId, _on: bool) -> Result<()> {
        self.check_error()?;
        Ok(())
    }

    async fn set_brightness(&self, _id: &DeviceId, _value: u8) -> Result<()> {
        self.check_error()?;
        Ok(())
    }

    async fn set_color(&self, _id: &DeviceId, _color: Color) -> Result<()> {
        self.check_error()?;
        Ok(())
    }

    async fn set_color_temp(&self, _id: &DeviceId, _kelvin: u32) -> Result<()> {
        self.check_error()?;
        Ok(())
    }

    async fn list_scenes(&self, _id: &DeviceId) -> Result<Vec<LightScene>> {
        Ok(vec![])
    }

    async fn set_scene(&self, _id: &DeviceId, _scene: &LightScene) -> Result<()> {
        self.check_error()?;
        Ok(())
    }

    async fn list_diy_scenes(&self, _id: &DeviceId) -> Result<Vec<DiyScene>> {
        Ok(vec![])
    }

    async fn set_diy_scene(&self, _id: &DeviceId, _scene: &DiyScene) -> Result<()> {
        self.check_error()?;
        Ok(())
    }

    async fn set_segment_color(
        &self,
        _id: &DeviceId,
        _segments: &[u8],
        _color: Color,
    ) -> Result<()> {
        self.check_error()?;
        Ok(())
    }

    async fn set_segment_brightness(
        &self,
        _id: &DeviceId,
        _segments: &[u8],
        _brightness: u8,
    ) -> Result<()> {
        self.check_error()?;
        Ok(())
    }

    async fn list_work_modes(&self, _id: &DeviceId) -> Result<Vec<WorkMode>> {
        Ok(vec![])
    }

    async fn set_work_mode(
        &self,
        _id: &DeviceId,
        _work_mode: u32,
        _mode_value: Option<u32>,
    ) -> Result<()> {
        self.check_error()?;
        Ok(())
    }

    fn backend_type(&self) -> BackendType {
        self.backend_type
    }
}
