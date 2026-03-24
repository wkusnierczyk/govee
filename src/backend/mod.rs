pub mod cloud;
pub mod local;
#[cfg(test)]
pub(crate) mod mock;

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{BackendType, Color, Device, DeviceId, DeviceState, DiyScene};

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

    /// List available DIY scenes for a device.
    async fn list_diy_scenes(&self, id: &DeviceId) -> Result<Vec<DiyScene>>;

    /// Activate a DIY scene by its ID.
    async fn set_diy_scene(&self, id: &DeviceId, scene: &DiyScene) -> Result<()>;

    /// Which backend type this implementation represents.
    fn backend_type(&self) -> BackendType;
}

#[cfg(test)]
mod tests {
    use super::mock::MockBackend;
    use super::*;

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
        let state = DeviceState::new(
            true,
            75,
            Color::new(255, 0, 0),
            None,
            false,
            std::collections::HashMap::new(),
        )
        .unwrap();
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
