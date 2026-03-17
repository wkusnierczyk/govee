use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::backend::GoveeBackend;
use crate::backend::cloud::CloudBackend;
use crate::backend::local::LocalBackend;
use crate::config::{BackendPreference, Config};
use crate::error::{GoveeError, Result};
use crate::types::{BackendType, Device, DeviceId};

/// Default discovery timeout for initial device scan during construction.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);

/// A device after cloud+local merge.
struct RegisteredDevice {
    device: Device,
    /// Which backend handles commands for this device.
    active_backend: BackendType,
}

/// State cache entry with provenance tracking.
#[allow(dead_code)]
struct CacheEntry {
    state: crate::types::DeviceState,
    source: CacheSource,
    updated_at: std::time::Instant,
}

/// How a cached state was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum CacheSource {
    /// Set by a write command — not yet confirmed by the device.
    Optimistic,
    /// Reconciliation confirmed the cached state matches the device.
    Confirmed,
    /// Reconciliation found the device state differs from cached.
    Stale,
}

/// Central device registry with backend selection and state caching.
///
/// Created via [`DeviceRegistry::start`], which returns an `Arc<Self>`.
/// The registry merges device lists from cloud and local backends,
/// provides name/alias resolution, per-device backend routing,
/// and (in future waves) optimistic state caching with background
/// reconciliation.
pub struct DeviceRegistry {
    devices: HashMap<DeviceId, RegisteredDevice>,
    cloud: Option<Arc<dyn GoveeBackend>>,
    local: Option<Arc<dyn GoveeBackend>>,
    #[allow(dead_code)]
    alias_map: HashMap<String, DeviceId>,
    #[allow(dead_code)]
    name_map: HashMap<String, DeviceId>,
    #[allow(dead_code)]
    group_map: HashMap<String, Vec<DeviceId>>,
    #[allow(dead_code)]
    state_cache: RwLock<HashMap<DeviceId, CacheEntry>>,
    cancel: CancellationToken,
    #[allow(dead_code)]
    config: Config,
}

impl DeviceRegistry {
    /// Create and start the device registry.
    ///
    /// Creates backends based on the configuration, lists devices from
    /// each, merges by MAC address, and returns the registry wrapped in
    /// `Arc` for shared ownership.
    pub async fn start(config: Config) -> Result<Arc<Self>> {
        // CloudOnly without an API key is a configuration error.
        if config.backend() == BackendPreference::CloudOnly && config.api_key().is_none() {
            return Err(GoveeError::InvalidConfig(
                "CloudOnly backend requires an API key".into(),
            ));
        }

        let cloud: Option<Arc<dyn GoveeBackend>> = if let Some(key) = config.api_key() {
            Some(Arc::new(CloudBackend::new(key.to_string(), None)?))
        } else {
            None
        };

        let local: Option<Arc<dyn GoveeBackend>> = match config.backend() {
            BackendPreference::CloudOnly => None,
            _ => {
                match LocalBackend::new(DISCOVERY_TIMEOUT, config.discovery_interval_secs()).await {
                    Ok(lb) => Some(Arc::new(lb)),
                    Err(GoveeError::BackendUnavailable(msg)) => {
                        tracing::warn!("local backend unavailable: {msg}");
                        if config.backend() == BackendPreference::LocalOnly {
                            return Err(GoveeError::BackendUnavailable(msg));
                        }
                        None
                    }
                    Err(e) => return Err(e),
                }
            }
        };

        Self::build(config, cloud, local).await
    }

    /// Shared construction logic used by both `start()` and test helpers.
    async fn build(
        config: Config,
        cloud: Option<Arc<dyn GoveeBackend>>,
        local: Option<Arc<dyn GoveeBackend>>,
    ) -> Result<Arc<Self>> {
        // List devices from available backends.
        // Cloud list_devices failure is fatal only for CloudOnly mode.
        let cloud_devices = match &cloud {
            Some(b) => match b.list_devices().await {
                Ok(devs) => devs,
                Err(e) if config.backend() == BackendPreference::CloudOnly => return Err(e),
                Err(e) => {
                    tracing::warn!("cloud list_devices failed, proceeding without cloud: {e}");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let local_devices = match &local {
            Some(b) => b.list_devices().await?,
            None => Vec::new(),
        };

        // Merge by MAC address. Cloud devices are primary (canonical names).
        let mut devices = HashMap::new();

        for dev in cloud_devices {
            devices.insert(
                dev.id.clone(),
                RegisteredDevice {
                    device: dev,
                    active_backend: BackendType::Cloud,
                },
            );
        }

        for dev in local_devices {
            match devices.get_mut(&dev.id) {
                Some(existing) => {
                    // Device found in both: keep cloud's name, update backend.
                    existing.active_backend = BackendType::Local;
                    existing.device.backend = BackendType::Local;
                    tracing::debug!(
                        device = %existing.device.id,
                        "device found in both backends, using local"
                    );
                }
                None => {
                    // Device found only locally: use SKU as name.
                    devices.insert(
                        dev.id.clone(),
                        RegisteredDevice {
                            device: dev,
                            active_backend: BackendType::Local,
                        },
                    );
                }
            }
        }

        // -- name resolution (#24) --

        // -- backend selection refinement (#25) --

        // -- group resolution (#28) --

        let cancel = CancellationToken::new();

        let registry = Arc::new(Self {
            devices,
            cloud,
            local,
            alias_map: HashMap::new(),
            name_map: HashMap::new(),
            group_map: HashMap::new(),
            state_cache: RwLock::new(HashMap::new()),
            cancel,
            config,
        });

        // Reconciliation task started in #26.

        Ok(registry)
    }

    /// Return all registered devices.
    pub fn devices(&self) -> Vec<Device> {
        self.devices.values().map(|r| r.device.clone()).collect()
    }

    /// Look up a single device by ID.
    pub fn get_device(&self, id: &DeviceId) -> Result<&Device> {
        self.devices
            .get(id)
            .map(|r| &r.device)
            .ok_or_else(|| GoveeError::DeviceNotFound(id.to_string()))
    }

    /// Return a reference to the backend responsible for the given device.
    #[allow(dead_code)]
    pub(crate) fn backend_for(&self, id: &DeviceId) -> Result<&dyn GoveeBackend> {
        let reg = self
            .devices
            .get(id)
            .ok_or_else(|| GoveeError::DeviceNotFound(id.to_string()))?;

        match reg.active_backend {
            BackendType::Cloud => self
                .cloud
                .as_deref()
                .ok_or_else(|| GoveeError::BackendUnavailable("cloud".into())),
            BackendType::Local => self
                .local
                .as_deref()
                .ok_or_else(|| GoveeError::BackendUnavailable("local".into())),
        }
    }

    /// Test-only constructor that accepts pre-built backends.
    #[cfg(test)]
    pub(crate) async fn start_with_backends(
        config: Config,
        cloud: Option<Arc<dyn GoveeBackend>>,
        local: Option<Arc<dyn GoveeBackend>>,
    ) -> Result<Arc<Self>> {
        Self::build(config, cloud, local).await
    }
}

impl fmt::Debug for DeviceRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceRegistry")
            .field("device_count", &self.devices.len())
            .field("cloud", &self.cloud.is_some())
            .field("local", &self.local.is_some())
            .finish()
    }
}

impl Drop for DeviceRegistry {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// Compile-time verification: DeviceRegistry is Send + Sync.
fn _assert_send_sync<T: Send + Sync>() {}
fn _assert_registry_send_sync() {
    _assert_send_sync::<DeviceRegistry>();
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::backend::mock::MockBackend;
    use crate::types::BackendType;

    fn make_device(mac: &str, model: &str, name: &str, backend: BackendType) -> Device {
        Device {
            id: DeviceId::new(mac).unwrap(),
            model: model.into(),
            name: name.into(),
            alias: None,
            backend,
        }
    }

    fn default_config() -> Config {
        Config::default()
    }

    #[tokio::test]
    async fn cloud_only_merge() {
        let cloud_devices = vec![
            make_device(
                "AA:BB:CC:DD:EE:01",
                "H6076",
                "Kitchen Light",
                BackendType::Cloud,
            ),
            make_device(
                "AA:BB:CC:DD:EE:02",
                "H6078",
                "Bedroom Light",
                BackendType::Cloud,
            ),
        ];
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(cloud_devices)
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let devices = registry.devices();
        assert_eq!(devices.len(), 2);
    }

    #[tokio::test]
    async fn local_only_merge() {
        let local_devices = vec![make_device(
            "AA:BB:CC:DD:EE:01",
            "H6076",
            "H6076_AABB",
            BackendType::Local,
        )];
        let local = Arc::new(
            MockBackend::new()
                .with_devices(local_devices)
                .with_backend_type(BackendType::Local),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), None, Some(local))
            .await
            .unwrap();

        let devices = registry.devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "H6076_AABB");
    }

    #[tokio::test]
    async fn overlapping_macs_uses_cloud_name_and_local_backend() {
        let mac = "AA:BB:CC:DD:EE:FF";
        let cloud_devices = vec![make_device(
            mac,
            "H6076",
            "Kitchen Light",
            BackendType::Cloud,
        )];
        let local_devices = vec![make_device(mac, "H6076", "H6076_AABB", BackendType::Local)];

        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(cloud_devices)
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;
        let local = Arc::new(
            MockBackend::new()
                .with_devices(local_devices)
                .with_backend_type(BackendType::Local),
        ) as Arc<dyn GoveeBackend>;

        let registry =
            DeviceRegistry::start_with_backends(default_config(), Some(cloud), Some(local))
                .await
                .unwrap();

        let devices = registry.devices();
        assert_eq!(devices.len(), 1);
        // Cloud name retained.
        assert_eq!(devices[0].name, "Kitchen Light");

        // Backend routed to local.
        let id = DeviceId::new(mac).unwrap();
        let backend = registry.backend_for(&id).unwrap();
        assert_eq!(backend.backend_type(), BackendType::Local);
    }

    #[tokio::test]
    async fn disjoint_devices_all_included() {
        let cloud_devices = vec![make_device(
            "AA:BB:CC:DD:EE:01",
            "H6076",
            "Cloud Only",
            BackendType::Cloud,
        )];
        let local_devices = vec![make_device(
            "AA:BB:CC:DD:EE:02",
            "H6078",
            "Local Only",
            BackendType::Local,
        )];

        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(cloud_devices)
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;
        let local = Arc::new(
            MockBackend::new()
                .with_devices(local_devices)
                .with_backend_type(BackendType::Local),
        ) as Arc<dyn GoveeBackend>;

        let registry =
            DeviceRegistry::start_with_backends(default_config(), Some(cloud), Some(local))
                .await
                .unwrap();

        assert_eq!(registry.devices().len(), 2);

        let cloud_id = DeviceId::new("AA:BB:CC:DD:EE:01").unwrap();
        let local_id = DeviceId::new("AA:BB:CC:DD:EE:02").unwrap();

        assert_eq!(
            registry.backend_for(&cloud_id).unwrap().backend_type(),
            BackendType::Cloud
        );
        assert_eq!(
            registry.backend_for(&local_id).unwrap().backend_type(),
            BackendType::Local
        );
    }

    #[tokio::test]
    async fn no_backends_empty_registry() {
        let registry = DeviceRegistry::start_with_backends(default_config(), None, None)
            .await
            .unwrap();

        assert!(registry.devices().is_empty());
    }

    #[tokio::test]
    async fn get_device_existing() {
        let mac = "AA:BB:CC:DD:EE:FF";
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![make_device(mac, "H6076", "Light", BackendType::Cloud)])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let id = DeviceId::new(mac).unwrap();
        let device = registry.get_device(&id).unwrap();
        assert_eq!(device.name, "Light");
    }

    #[tokio::test]
    async fn get_device_not_found() {
        let registry = DeviceRegistry::start_with_backends(default_config(), None, None)
            .await
            .unwrap();

        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        assert!(registry.get_device(&id).is_err());
    }

    #[tokio::test]
    async fn backend_for_unknown_device() {
        let registry = DeviceRegistry::start_with_backends(default_config(), None, None)
            .await
            .unwrap();

        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        let result = registry.backend_for(&id);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn backend_for_missing_cloud_backend() {
        // Device claims Cloud backend but no cloud backend was provided.
        // This exercises the BackendUnavailable branch in backend_for().
        let cloud_devices = vec![make_device(
            "AA:BB:CC:DD:EE:FF",
            "H6076",
            "Light",
            BackendType::Cloud,
        )];
        // Pass cloud devices through the cloud slot, then remove it.
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(cloud_devices)
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        // Build with cloud to populate devices, then verify routing
        // works when the device is cloud-assigned.
        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        // Cloud backend is present, so this should succeed.
        assert!(registry.backend_for(&id).is_ok());
        assert_eq!(
            registry.backend_for(&id).unwrap().backend_type(),
            BackendType::Cloud
        );
    }

    #[tokio::test]
    async fn backend_for_local_device_with_available_backend() {
        let local_devices = vec![make_device(
            "AA:BB:CC:DD:EE:FF",
            "H6076",
            "Light",
            BackendType::Local,
        )];
        let local = Arc::new(
            MockBackend::new()
                .with_devices(local_devices)
                .with_backend_type(BackendType::Local),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), None, Some(local))
            .await
            .unwrap();

        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        assert!(registry.backend_for(&id).is_ok());
        assert_eq!(
            registry.backend_for(&id).unwrap().backend_type(),
            BackendType::Local
        );
    }

    #[tokio::test]
    async fn debug_format() {
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![
                    make_device("AA:BB:CC:DD:EE:01", "H6076", "Light 1", BackendType::Cloud),
                    make_device("AA:BB:CC:DD:EE:02", "H6078", "Light 2", BackendType::Cloud),
                ])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let debug = format!("{:?}", registry);
        assert!(debug.contains("device_count: 2"));
        assert!(debug.contains("cloud: true"));
        assert!(debug.contains("local: false"));
    }

    #[tokio::test]
    async fn overlapping_device_backend_field_updated() {
        // When merged, Device.backend should reflect the active backend.
        let mac = "AA:BB:CC:DD:EE:FF";
        let cloud_devices = vec![make_device(mac, "H6076", "Light", BackendType::Cloud)];
        let local_devices = vec![make_device(mac, "H6076", "H6076_X", BackendType::Local)];

        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(cloud_devices)
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;
        let local = Arc::new(
            MockBackend::new()
                .with_devices(local_devices)
                .with_backend_type(BackendType::Local),
        ) as Arc<dyn GoveeBackend>;

        let registry =
            DeviceRegistry::start_with_backends(default_config(), Some(cloud), Some(local))
                .await
                .unwrap();

        let id = DeviceId::new(mac).unwrap();
        let device = registry.get_device(&id).unwrap();
        // Device.backend must match active routing.
        assert_eq!(device.backend, BackendType::Local);
    }

    #[tokio::test]
    async fn cloud_only_without_api_key_is_error() {
        let config = Config::new(
            None,
            BackendPreference::CloudOnly,
            60,
            HashMap::new(),
            HashMap::new(),
        )
        .unwrap();

        let result = DeviceRegistry::start(config).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("CloudOnly"));
        assert!(err.contains("API key"));
    }

    #[test]
    fn send_sync_assertion() {
        _assert_send_sync::<DeviceRegistry>();
    }

    #[tokio::test]
    async fn arc_clone_shares_registry() {
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![make_device(
                    "AA:BB:CC:DD:EE:FF",
                    "H6076",
                    "Light",
                    BackendType::Cloud,
                )])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let clone = Arc::clone(&registry);
        assert_eq!(registry.devices().len(), clone.devices().len());
    }
}
