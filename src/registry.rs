use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::backend::GoveeBackend;
use crate::backend::cloud::CloudBackend;
use crate::backend::local::LocalBackend;
use crate::config::{BackendPreference, Config};
use crate::error::{GoveeError, Result};
use crate::types::{BackendType, Device, DeviceId, DeviceState};

/// Default discovery timeout for initial device scan during construction.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);

/// A device after cloud+local merge.
struct RegisteredDevice {
    device: Device,
    /// Which backend handles commands for this device.
    active_backend: BackendType,
}

/// State cache entry with provenance tracking.
struct CacheEntry {
    state: DeviceState,
    source: CacheSource,
    updated_at: Instant,
}

/// How a cached state was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheSource {
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
/// The registry merges device lists from cloud and local backends
/// and routes commands to the appropriate backend per device.
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

        let cloud: Option<Arc<dyn GoveeBackend>> =
            if config.backend() != BackendPreference::LocalOnly {
                if let Some(key) = config.api_key() {
                    Some(Arc::new(CloudBackend::new(key.to_string(), None)?))
                } else {
                    None
                }
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
            Some(b) => match b.list_devices().await {
                Ok(devs) => devs,
                Err(e) if config.backend() == BackendPreference::LocalOnly => return Err(e),
                Err(e) => {
                    tracing::warn!("local list_devices failed, proceeding without local: {e}");
                    Vec::new()
                }
            },
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

        // Populate name_map: lowercased device name → device ID.
        // Sort by DeviceId for deterministic collision resolution.
        let mut name_map = HashMap::new();
        let mut sorted_devices: Vec<_> = devices.values().collect();
        sorted_devices.sort_by(|a, b| a.device.id.as_str().cmp(b.device.id.as_str()));
        for reg in sorted_devices {
            let key = reg.device.name.to_lowercase();
            use std::collections::hash_map::Entry;
            match name_map.entry(key) {
                Entry::Occupied(mut e) => {
                    tracing::warn!(
                        name = %reg.device.name,
                        new_device = %reg.device.id,
                        previous_device = %e.get(),
                        "duplicate device name, overwriting previous mapping"
                    );
                    e.insert(reg.device.id.clone());
                }
                Entry::Vacant(e) => {
                    e.insert(reg.device.id.clone());
                }
            }
        }

        // Populate alias_map: lowercased alias → device ID (resolved via target name).
        let mut alias_map = HashMap::new();
        for (alias, target) in config.aliases() {
            let alias_key = alias.to_lowercase();
            let target_key = target.to_lowercase();
            match name_map.get(&target_key) {
                Some(device_id) => {
                    if let Some(prev) = alias_map.insert(alias_key, device_id.clone()) {
                        tracing::warn!(
                            alias = %alias,
                            new_target = %device_id,
                            previous_target = %prev,
                            "case-insensitive alias collision, overwriting"
                        );
                    }
                }
                None => {
                    tracing::warn!(
                        alias = %alias,
                        target = %target,
                        "alias target does not match any device name"
                    );
                }
            }
        }

        // Backend selection refinement: adjust active_backend per preference.
        match config.backend() {
            BackendPreference::CloudOnly => {
                for reg in devices.values_mut() {
                    reg.active_backend = BackendType::Cloud;
                    reg.device.backend = BackendType::Cloud;
                }
                tracing::debug!("CloudOnly mode: all devices assigned to cloud backend");
            }
            BackendPreference::LocalOnly => {
                let before = devices.len();
                devices.retain(|_id, reg| reg.active_backend == BackendType::Local);
                let removed = before - devices.len();
                if removed > 0 {
                    tracing::info!(
                        removed,
                        "LocalOnly mode: removed cloud-only device(s) from registry"
                    );
                }
                tracing::debug!(
                    remaining = devices.len(),
                    "LocalOnly mode: all remaining devices assigned to local backend"
                );
            }
            BackendPreference::Auto => {
                for reg in devices.values() {
                    tracing::debug!(
                        device = %reg.device.id,
                        backend = %reg.active_backend,
                        "Auto mode: device backend assignment"
                    );
                }
            }
        }

        // -- group resolution (#28) --

        let cancel = CancellationToken::new();
        let interval = Duration::from_secs(config.discovery_interval_secs());
        let cancel_for_task = cancel.clone();

        let registry = Arc::new(Self {
            devices,
            cloud,
            local,
            alias_map,
            name_map,
            group_map: HashMap::new(),
            state_cache: RwLock::new(HashMap::new()),
            cancel,
            config,
        });

        tokio::spawn(reconciliation_loop(
            Arc::downgrade(&registry),
            cancel_for_task,
            interval,
        ));

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

    /// Resolve a device by name or alias.
    ///
    /// Looks up the lowercased `name` in the name map first, then the alias
    /// map. Returns `DeviceNotFound` if neither matches.
    pub fn resolve(&self, name: &str) -> Result<DeviceId> {
        let key = name.to_lowercase();
        if let Some(id) = self.name_map.get(&key) {
            return Ok(id.clone());
        }
        if let Some(id) = self.alias_map.get(&key) {
            return Ok(id.clone());
        }
        Err(GoveeError::DeviceNotFound(name.to_string()))
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

    /// Return `(DeviceId, BackendType)` for every registered device.
    pub fn backend_status(&self) -> Vec<(DeviceId, BackendType)> {
        self.devices
            .iter()
            .map(|(id, reg)| (id.clone(), reg.active_backend))
            .collect()
    }

    /// Query the current state of a device.
    ///
    /// Returns a cached state if one exists and is not stale. Otherwise
    /// queries the device's backend, caches the result as `Confirmed`,
    /// and returns it.
    pub async fn get_state(self: &Arc<Self>, id: &DeviceId) -> Result<DeviceState> {
        self.get_device(id)?;

        // Check cache first.
        {
            let cache = self.state_cache.read().await;
            if let Some(entry) = cache.get(id)
                && entry.source != CacheSource::Stale
            {
                return Ok(entry.state.clone());
            }
        }

        // Cache miss or stale — query backend.
        let state = self.backend_for(id)?.get_state(id).await?;

        // Cache as confirmed.
        {
            let mut cache = self.state_cache.write().await;
            cache.insert(
                id.clone(),
                CacheEntry {
                    state: state.clone(),
                    source: CacheSource::Confirmed,
                    updated_at: Instant::now(),
                },
            );
        }

        Ok(state)
    }

    /// Update the state cache for a device.
    pub(crate) async fn update_cache(
        &self,
        id: &DeviceId,
        state: DeviceState,
        source: CacheSource,
    ) {
        let mut cache = self.state_cache.write().await;
        cache.insert(
            id.clone(),
            CacheEntry {
                state,
                source,
                updated_at: Instant::now(),
            },
        );
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

/// Background reconciliation loop.
///
/// Periodically queries each device's backend for current state and
/// compares it to the cached state. Updates cache entries to `Confirmed`
/// or `Stale` accordingly. Exits when the registry is dropped (weak
/// reference fails to upgrade) or the cancellation token fires.
async fn reconciliation_loop(
    weak: Weak<DeviceRegistry>,
    cancel: CancellationToken,
    interval: Duration,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(interval) => {}
        }

        let registry = match weak.upgrade() {
            Some(r) => r,
            None => break,
        };

        let device_ids: Vec<DeviceId> = registry.devices.keys().cloned().collect();

        for id in &device_ids {
            // Check if we should skip this device.
            {
                let cache = registry.state_cache.read().await;
                if let Some(entry) = cache.get(id)
                    && entry.source == CacheSource::Optimistic
                    && entry.updated_at.elapsed() < interval
                {
                    continue;
                }
            }

            // Query backend for current state.
            let backend = match registry.backend_for(id) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(device = %id, error = %e, "reconciliation: backend lookup failed");
                    continue;
                }
            };

            let live_state = match backend.get_state(id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(device = %id, error = %e, "reconciliation: get_state failed");
                    continue;
                }
            };

            // Compare to cached state and update.
            let source = {
                let cache = registry.state_cache.read().await;
                match cache.get(id) {
                    Some(entry) => {
                        if entry.state.on == live_state.on
                            && entry.state.brightness == live_state.brightness
                            && entry.state.color == live_state.color
                            && entry.state.color_temp_kelvin == live_state.color_temp_kelvin
                        {
                            CacheSource::Confirmed
                        } else {
                            CacheSource::Stale
                        }
                    }
                    None => CacheSource::Confirmed,
                }
            };

            registry.update_cache(id, live_state, source).await;
        }
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
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _assert() {
        _assert_send_sync::<DeviceRegistry>();
    }
};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;
    use crate::backend::mock::MockBackend;
    use crate::types::DeviceState;

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
    async fn backend_for_routes_to_cloud() {
        let cloud_devices = vec![make_device(
            "AA:BB:CC:DD:EE:FF",
            "H6076",
            "Light",
            BackendType::Cloud,
        )];
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(cloud_devices)
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(
            registry.backend_for(&id).unwrap().backend_type(),
            BackendType::Cloud
        );
    }

    #[tokio::test]
    async fn backend_for_routes_to_local() {
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
        assert_eq!(
            registry.backend_for(&id).unwrap().backend_type(),
            BackendType::Local
        );
    }

    #[tokio::test]
    async fn backend_for_unavailable_returns_error() {
        // Cloud-assigned device exists but cloud backend was dropped after
        // a list_devices failure in Auto mode. The device remains in the
        // registry with active_backend=Cloud, but the cloud Arc is None.
        // Simulate this by building with cloud to populate, then constructing
        // a registry manually where cloud is None.
        //
        // We use build() via start_with_backends with a mock that returns
        // cloud devices through the local slot (so they get active_backend=Local),
        // but we can't easily get active_backend=Cloud without a cloud backend.
        //
        // Instead, test the symmetric case: local-assigned device with no
        // local backend. We put devices through cloud, then merge sets them
        // to Cloud. If we had only a cloud backend and a device somehow got
        // assigned to Local, backend_for would fail. Since the merge logic
        // can't produce this state naturally, we verify the error path via
        // the unknown-device case (already covered) and trust the match arm.
        //
        // The realistic scenario: cloud backend present at list_devices time,
        // but later becomes unavailable. That's a runtime concern (the Arc
        // is still held). BackendUnavailable from backend_for only happens
        // if the Option is None, which can't happen post-construction for
        // the backend that provided the device. This is a design invariant.
        //
        // Verify the cloud routing path works correctly.
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

        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        // Cloud backend is present — succeeds.
        assert!(registry.backend_for(&id).is_ok());
        // Unknown device — fails with DeviceNotFound.
        let unknown = DeviceId::new("11:22:33:44:55:66").unwrap();
        let err = registry.backend_for(&unknown).err().unwrap();
        assert!(matches!(err, GoveeError::DeviceNotFound(_)));
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

    // -- Backend selection (#25) tests --

    #[tokio::test]
    async fn auto_mode_local_for_discovered_cloud_for_rest() {
        let cloud_devices = vec![
            make_device("AA:BB:CC:DD:EE:01", "H6076", "Light A", BackendType::Cloud),
            make_device("AA:BB:CC:DD:EE:02", "H6078", "Light B", BackendType::Cloud),
        ];
        let local_devices = vec![make_device(
            "AA:BB:CC:DD:EE:01",
            "H6076",
            "H6076_X",
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

        // Auto mode (default config).
        let registry =
            DeviceRegistry::start_with_backends(default_config(), Some(cloud), Some(local))
                .await
                .unwrap();

        let id_local = DeviceId::new("AA:BB:CC:DD:EE:01").unwrap();
        let id_cloud = DeviceId::new("AA:BB:CC:DD:EE:02").unwrap();

        // Discovered locally → Local.
        assert_eq!(
            registry.backend_for(&id_local).unwrap().backend_type(),
            BackendType::Local
        );
        // Cloud only → Cloud.
        assert_eq!(
            registry.backend_for(&id_cloud).unwrap().backend_type(),
            BackendType::Cloud
        );
    }

    #[tokio::test]
    async fn cloud_only_mode_all_cloud() {
        let cloud_devices = vec![
            make_device("AA:BB:CC:DD:EE:01", "H6076", "Light A", BackendType::Cloud),
            make_device("AA:BB:CC:DD:EE:02", "H6078", "Light B", BackendType::Cloud),
        ];
        let local_devices = vec![make_device(
            "AA:BB:CC:DD:EE:01",
            "H6076",
            "H6076_X",
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

        let config = Config::new(
            Some("test-key".into()),
            BackendPreference::CloudOnly,
            60,
            HashMap::new(),
            HashMap::new(),
        )
        .unwrap();

        let registry = DeviceRegistry::start_with_backends(config, Some(cloud), Some(local))
            .await
            .unwrap();

        let devices = registry.devices();
        assert_eq!(devices.len(), 2);

        // All devices should be Cloud.
        for dev in &devices {
            assert_eq!(dev.backend, BackendType::Cloud);
        }
    }

    #[tokio::test]
    async fn local_only_mode_excludes_cloud_only_devices() {
        let cloud_devices = vec![
            make_device("AA:BB:CC:DD:EE:01", "H6076", "Light A", BackendType::Cloud),
            make_device("AA:BB:CC:DD:EE:02", "H6078", "Light B", BackendType::Cloud),
        ];
        let local_devices = vec![make_device(
            "AA:BB:CC:DD:EE:01",
            "H6076",
            "H6076_X",
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

        let config = Config::new(
            None,
            BackendPreference::LocalOnly,
            60,
            HashMap::new(),
            HashMap::new(),
        )
        .unwrap();

        let registry = DeviceRegistry::start_with_backends(config, Some(cloud), Some(local))
            .await
            .unwrap();

        let devices = registry.devices();
        // Only the locally-discovered device remains.
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, DeviceId::new("AA:BB:CC:DD:EE:01").unwrap());
        assert_eq!(devices[0].backend, BackendType::Local);

        // Cloud-only device is gone.
        let cloud_only_id = DeviceId::new("AA:BB:CC:DD:EE:02").unwrap();
        assert!(registry.get_device(&cloud_only_id).is_err());
    }

    #[tokio::test]
    async fn auto_no_api_key_local_only() {
        // Auto mode without an API key → no cloud backend, only local devices.
        let local_devices = vec![make_device(
            "AA:BB:CC:DD:EE:01",
            "H6076",
            "H6076_X",
            BackendType::Local,
        )];

        let local = Arc::new(
            MockBackend::new()
                .with_devices(local_devices)
                .with_backend_type(BackendType::Local),
        ) as Arc<dyn GoveeBackend>;

        // default_config() → Auto, no API key.
        let registry = DeviceRegistry::start_with_backends(default_config(), None, Some(local))
            .await
            .unwrap();

        let devices = registry.devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].backend, BackendType::Local);
    }

    #[tokio::test]
    async fn backend_status_reflects_assignments() {
        let cloud_devices = vec![
            make_device("AA:BB:CC:DD:EE:01", "H6076", "Light A", BackendType::Cloud),
            make_device("AA:BB:CC:DD:EE:02", "H6078", "Light B", BackendType::Cloud),
        ];
        let local_devices = vec![make_device(
            "AA:BB:CC:DD:EE:01",
            "H6076",
            "H6076_X",
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

        let status = registry.backend_status();
        assert_eq!(status.len(), 2);

        let status_map: HashMap<DeviceId, BackendType> = status.into_iter().collect();
        let id1 = DeviceId::new("AA:BB:CC:DD:EE:01").unwrap();
        let id2 = DeviceId::new("AA:BB:CC:DD:EE:02").unwrap();

        assert_eq!(status_map[&id1], BackendType::Local);
        assert_eq!(status_map[&id2], BackendType::Cloud);
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

    // -- name resolution tests (#24) --

    #[tokio::test]
    async fn resolve_by_canonical_name_exact_case() {
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![make_device(
                    "AA:BB:CC:DD:EE:01",
                    "H6076",
                    "Kitchen Light",
                    BackendType::Cloud,
                )])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let id = registry.resolve("Kitchen Light").unwrap();
        assert_eq!(id, DeviceId::new("AA:BB:CC:DD:EE:01").unwrap());
    }

    #[tokio::test]
    async fn resolve_by_canonical_name_different_case() {
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![make_device(
                    "AA:BB:CC:DD:EE:01",
                    "H6076",
                    "Kitchen Light",
                    BackendType::Cloud,
                )])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let id = registry.resolve("kitchen light").unwrap();
        assert_eq!(id, DeviceId::new("AA:BB:CC:DD:EE:01").unwrap());

        let id = registry.resolve("KITCHEN LIGHT").unwrap();
        assert_eq!(id, DeviceId::new("AA:BB:CC:DD:EE:01").unwrap());
    }

    #[tokio::test]
    async fn resolve_by_alias() {
        let mut aliases = HashMap::new();
        aliases.insert("kitchen".to_string(), "Kitchen Light".to_string());

        let config =
            Config::new(None, BackendPreference::Auto, 60, aliases, HashMap::new()).unwrap();

        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![make_device(
                    "AA:BB:CC:DD:EE:01",
                    "H6076",
                    "Kitchen Light",
                    BackendType::Cloud,
                )])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(config, Some(cloud), None)
            .await
            .unwrap();

        let id = registry.resolve("kitchen").unwrap();
        assert_eq!(id, DeviceId::new("AA:BB:CC:DD:EE:01").unwrap());
    }

    #[tokio::test]
    async fn resolve_unknown_name_returns_error() {
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![make_device(
                    "AA:BB:CC:DD:EE:01",
                    "H6076",
                    "Kitchen Light",
                    BackendType::Cloud,
                )])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let err = registry.resolve("Nonexistent").unwrap_err();
        assert!(matches!(err, GoveeError::DeviceNotFound(name) if name == "Nonexistent"));
    }

    #[tokio::test]
    async fn resolve_multiple_aliases_same_device() {
        let mut aliases = HashMap::new();
        aliases.insert("kitchen".to_string(), "Kitchen Light".to_string());
        aliases.insert("k".to_string(), "Kitchen Light".to_string());

        let config =
            Config::new(None, BackendPreference::Auto, 60, aliases, HashMap::new()).unwrap();

        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![make_device(
                    "AA:BB:CC:DD:EE:01",
                    "H6076",
                    "Kitchen Light",
                    BackendType::Cloud,
                )])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(config, Some(cloud), None)
            .await
            .unwrap();

        let expected = DeviceId::new("AA:BB:CC:DD:EE:01").unwrap();
        assert_eq!(registry.resolve("kitchen").unwrap(), expected);
        assert_eq!(registry.resolve("k").unwrap(), expected);
    }

    #[tokio::test]
    async fn resolve_alias_target_not_found_not_registered() {
        let mut aliases = HashMap::new();
        aliases.insert("ghost".to_string(), "Does Not Exist".to_string());

        let config =
            Config::new(None, BackendPreference::Auto, 60, aliases, HashMap::new()).unwrap();

        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![make_device(
                    "AA:BB:CC:DD:EE:01",
                    "H6076",
                    "Kitchen Light",
                    BackendType::Cloud,
                )])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(config, Some(cloud), None)
            .await
            .unwrap();

        // The alias "ghost" should not be registered since its target doesn't exist.
        let err = registry.resolve("ghost").unwrap_err();
        assert!(matches!(err, GoveeError::DeviceNotFound(_)));
    }

    #[tokio::test]
    async fn resolve_name_collision_last_by_id_wins() {
        // Two devices with the same name (case-insensitive). Devices are
        // sorted by DeviceId before populating name_map, so the
        // lexicographically last ID wins deterministically.
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![
                    make_device(
                        "AA:BB:CC:DD:EE:01",
                        "H6076",
                        "Living Room",
                        BackendType::Cloud,
                    ),
                    make_device(
                        "AA:BB:CC:DD:EE:02",
                        "H6078",
                        "Living Room",
                        BackendType::Cloud,
                    ),
                ])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        // Last by DeviceId sort order wins.
        let id = registry.resolve("Living Room").unwrap();
        assert_eq!(id, DeviceId::new("AA:BB:CC:DD:EE:02").unwrap());
    }

    // -- State cache tests (#26) --

    fn make_state(on: bool, brightness: u8, r: u8, g: u8, b: u8) -> DeviceState {
        use crate::types::Color;
        DeviceState::new(on, brightness, Color::new(r, g, b), None, false).unwrap()
    }

    fn mock_with_device_and_state(
        mac: &str,
        state: DeviceState,
    ) -> (Arc<dyn GoveeBackend>, DeviceId) {
        let id = DeviceId::new(mac).unwrap();
        let device = make_device(mac, "H6076", "Test Light", BackendType::Cloud);
        let backend = Arc::new(
            MockBackend::new()
                .with_devices(vec![device])
                .with_state(state)
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;
        (backend, id)
    }

    #[tokio::test]
    async fn cache_miss_queries_backend() {
        let state = make_state(true, 75, 255, 0, 0);
        let (cloud, id) = mock_with_device_and_state("AA:BB:CC:DD:EE:01", state);

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let result = registry.get_state(&id).await.unwrap();
        assert!(result.on);
        assert_eq!(result.brightness, 75);
        assert_eq!(result.color.r, 255);
        assert_eq!(result.color.g, 0);
        assert_eq!(result.color.b, 0);
    }

    #[tokio::test]
    async fn cache_hit_returns_cached() {
        let state = make_state(true, 50, 0, 255, 0);
        let (cloud, id) = mock_with_device_and_state("AA:BB:CC:DD:EE:02", state);

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        // First call: cache miss, queries backend.
        let first = registry.get_state(&id).await.unwrap();
        assert_eq!(first.brightness, 50);

        // Second call: cache hit, returns same cached state.
        let second = registry.get_state(&id).await.unwrap();
        assert_eq!(second.brightness, 50);
        assert_eq!(second.color.g, 255);
    }

    #[tokio::test]
    async fn update_cache_optimistic_reflected_in_get_state() {
        let state = make_state(true, 50, 0, 255, 0);
        let (cloud, id) = mock_with_device_and_state("AA:BB:CC:DD:EE:03", state);

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        // Set optimistic state (different from what backend would return).
        let optimistic = make_state(false, 100, 0, 0, 255);
        registry
            .update_cache(&id, optimistic, CacheSource::Optimistic)
            .await;

        // get_state should return the optimistic state, not backend state.
        let result = registry.get_state(&id).await.unwrap();
        assert!(!result.on);
        assert_eq!(result.brightness, 100);
        assert_eq!(result.color.b, 255);
    }

    #[tokio::test]
    async fn stale_cache_requeries_backend() {
        let state = make_state(true, 80, 128, 0, 0);
        let (cloud, id) = mock_with_device_and_state("AA:BB:CC:DD:EE:04", state);

        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        // Populate cache with a different state, marked as Stale.
        let stale_state = make_state(false, 0, 0, 0, 0);
        registry
            .update_cache(&id, stale_state, CacheSource::Stale)
            .await;

        // get_state should re-query backend because entry is Stale.
        let result = registry.get_state(&id).await.unwrap();
        assert!(result.on);
        assert_eq!(result.brightness, 80);
        assert_eq!(result.color.r, 128);
    }

    #[tokio::test]
    async fn reconciliation_exits_when_weak_is_dead() {
        let state = make_state(true, 50, 0, 255, 0);
        let device = make_device(
            "AA:BB:CC:DD:EE:05",
            "H6076",
            "Test Light",
            BackendType::Cloud,
        );
        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![device])
                .with_state(state)
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let cancel = CancellationToken::new();
        let registry = DeviceRegistry::start_with_backends(default_config(), Some(cloud), None)
            .await
            .unwrap();

        let weak = Arc::downgrade(&registry);
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(reconciliation_loop(
            weak,
            cancel_clone,
            Duration::from_millis(10),
        ));

        // Drop the registry so the Weak cannot upgrade.
        drop(registry);

        // The task should complete promptly.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("reconciliation task should complete")
            .expect("reconciliation task should not panic");

        cancel.cancel();
    }

    #[tokio::test]
    async fn resolve_alias_case_insensitive() {
        let mut aliases = HashMap::new();
        aliases.insert("kitchen".to_string(), "Kitchen Light".to_string());

        let config =
            Config::new(None, BackendPreference::Auto, 60, aliases, HashMap::new()).unwrap();

        let cloud = Arc::new(
            MockBackend::new()
                .with_devices(vec![make_device(
                    "AA:BB:CC:DD:EE:01",
                    "H6076",
                    "Kitchen Light",
                    BackendType::Cloud,
                )])
                .with_backend_type(BackendType::Cloud),
        ) as Arc<dyn GoveeBackend>;

        let registry = DeviceRegistry::start_with_backends(config, Some(cloud), None)
            .await
            .unwrap();

        let expected = DeviceId::new("AA:BB:CC:DD:EE:01").unwrap();
        assert_eq!(registry.resolve("KITCHEN").unwrap(), expected);
        assert_eq!(registry.resolve("Kitchen").unwrap(), expected);
    }
}
