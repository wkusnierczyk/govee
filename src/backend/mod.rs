pub mod cloud;
pub mod local;

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{BackendType, Color, Device, DeviceId, DeviceState};

/// Unified interface for controlling Govee devices.
///
/// Implemented by [`CloudBackend`](cloud::CloudBackend) and
/// [`LocalBackend`](local::LocalBackend). The trait is object-safe
/// (`Box<dyn GoveeBackend>`) and requires `Send + Sync`.
#[async_trait]
pub trait GoveeBackend: Send + Sync {
    /// List all devices visible to this backend.
    async fn list_devices(&self) -> Result<Vec<Device>>;

    /// Query the current state of a device.
    async fn get_state(&self, id: &DeviceId) -> Result<DeviceState>;

    /// Turn a device on or off.
    async fn set_power(&self, id: &DeviceId, on: bool) -> Result<()>;

    /// Set brightness (0–100).
    async fn set_brightness(&self, id: &DeviceId, value: u8) -> Result<()>;

    /// Set the device color.
    async fn set_color(&self, id: &DeviceId, color: Color) -> Result<()>;

    /// Set color temperature in Kelvin.
    async fn set_color_temp(&self, id: &DeviceId, kelvin: u32) -> Result<()>;

    /// Which backend type this implementation represents.
    fn backend_type(&self) -> BackendType;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configurable mock backend for testing trait consumers.
    ///
    /// Devices and state are set via builder methods. `get_state` returns
    /// `DeviceNotFound` when no state is configured. Setter methods always
    /// return `Ok(())`.
    struct MockBackend {
        devices: Vec<Device>,
        state: Option<DeviceState>,
        backend_type: BackendType,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                devices: Vec::new(),
                state: None,
                backend_type: BackendType::Cloud,
            }
        }

        fn with_devices(mut self, devices: Vec<Device>) -> Self {
            self.devices = devices;
            self
        }

        fn with_state(mut self, state: DeviceState) -> Self {
            self.state = Some(state);
            self
        }

        fn with_backend_type(mut self, backend_type: BackendType) -> Self {
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
                .ok_or_else(|| crate::error::GoveeError::DeviceNotFound(id.to_string()))
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

    // Compile-time verification: GoveeBackend is Send + Sync
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _assert_object_safe(_: &dyn GoveeBackend) {}

    #[test]
    fn trait_is_send_sync() {
        _assert_send_sync::<MockBackend>();
    }

    #[tokio::test]
    async fn mock_list_devices_empty() {
        let mock = MockBackend::new();
        let devices = mock.list_devices().await.unwrap();
        assert!(devices.is_empty());
    }

    #[tokio::test]
    async fn mock_list_devices_with_entries() {
        let device = Device {
            id: DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap(),
            model: "H6076".into(),
            name: "Test Light".into(),
            alias: None,
            backend: BackendType::Cloud,
        };
        let mock = MockBackend::new().with_devices(vec![device.clone()]);
        let devices = mock.list_devices().await.unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, device.id);
    }

    #[tokio::test]
    async fn mock_get_state_returns_configured() {
        let state = DeviceState::new(true, 75, Color::new(255, 0, 0), None, false).unwrap();
        let mock = MockBackend::new().with_state(state);
        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        let result = mock.get_state(&id).await.unwrap();
        assert_eq!(result.brightness, 75);
        assert!(result.on);
    }

    #[tokio::test]
    async fn mock_get_state_not_found() {
        let mock = MockBackend::new();
        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        let result = mock.get_state(&id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mock_set_operations_succeed() {
        let mock = MockBackend::new();
        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        assert!(mock.set_power(&id, true).await.is_ok());
        assert!(mock.set_brightness(&id, 50).await.is_ok());
        assert!(mock.set_color(&id, Color::new(0, 255, 0)).await.is_ok());
        assert!(mock.set_color_temp(&id, 4000).await.is_ok());
    }

    #[test]
    fn mock_backend_type_default_cloud() {
        let mock = MockBackend::new();
        assert_eq!(mock.backend_type(), BackendType::Cloud);
    }

    #[test]
    fn mock_backend_type_configurable() {
        let mock = MockBackend::new().with_backend_type(BackendType::Local);
        assert_eq!(mock.backend_type(), BackendType::Local);
    }

    #[tokio::test]
    async fn trait_object_dispatch() {
        let mock = MockBackend::new();
        let backend: Box<dyn GoveeBackend> = Box::new(mock);
        let devices = backend.list_devices().await.unwrap();
        assert!(devices.is_empty());
        assert_eq!(backend.backend_type(), BackendType::Cloud);
    }
}
