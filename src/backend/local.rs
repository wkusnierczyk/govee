use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, RwLock, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use serde::de::Error as _;

use crate::backend::GoveeBackend;
use crate::error::{GoveeError, Result};
use crate::types::{BackendType, Color, Device, DeviceId, DeviceState};

/// Helper to create a `GoveeError::Json` with a custom message.
fn json_error(msg: &str) -> GoveeError {
    GoveeError::Json(serde_json::Error::custom(msg))
}

/// Multicast group used by Govee LAN protocol for discovery.
const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
/// Port the device sends responses to (we listen on this).
const LISTEN_PORT: u16 = 4002;
/// Port the device listens on for commands.
const DEVICE_PORT: u16 = 4001;

/// A device discovered via the Govee LAN protocol.
struct DiscoveredDevice {
    ip: IpAddr,
    device_id: DeviceId,
    sku: String,
    last_seen: Instant,
}

/// Local LAN backend (tokio UDP, multicast discovery).
///
/// Communicates with Govee devices on the local network via the
/// Govee LAN API (UDP multicast). Requires port 4002 to be available.
pub struct LocalBackend {
    send_socket: UdpSocket,
    devices: Arc<RwLock<HashMap<DeviceId, DiscoveredDevice>>>,
    /// Used by Wave 2 command methods (get_state, etc.) to receive responses.
    #[allow(dead_code)]
    pending_state: Arc<Mutex<HashMap<IpAddr, oneshot::Sender<DeviceState>>>>,
    cancel: CancellationToken,
    receiver_handle: JoinHandle<()>,
    discovery_timeout: Duration,
    device_ttl: Duration,
}

impl LocalBackend {
    /// Create a new `LocalBackend`.
    ///
    /// Binds to port 4002 for receiving multicast responses and spawns a
    /// background receiver task. `discovery_timeout` controls how long
    /// `discover()` waits after sending a scan. `discovery_interval_secs`
    /// determines the device TTL (3x the interval).
    pub async fn new(discovery_timeout: Duration, discovery_interval_secs: u64) -> Result<Self> {
        // Build receive socket via socket2 for SO_REUSEADDR + multicast join.
        let recv_socket = {
            let socket = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            socket.set_reuse_address(true)?;

            let bind_addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, LISTEN_PORT).into();
            if let Err(e) = socket.bind(&bind_addr.into()) {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    return Err(GoveeError::BackendUnavailable(
                        "port 4002 already in use — another process \
                         (Home Assistant, govee2mqtt, etc.) may be using the Govee LAN API"
                            .into(),
                    ));
                }
                return Err(e.into());
            }

            socket.join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED)?;
            socket.set_nonblocking(true)?;

            UdpSocket::from_std(std::net::UdpSocket::from(socket))?
        };

        // Ephemeral send socket.
        let send_socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0u16)).await?;

        let devices: Arc<RwLock<HashMap<DeviceId, DiscoveredDevice>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let pending_state: Arc<Mutex<HashMap<IpAddr, oneshot::Sender<DeviceState>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let cancel = CancellationToken::new();

        let receiver_handle = {
            let devices = Arc::clone(&devices);
            let pending_state = Arc::clone(&pending_state);
            let cancel = cancel.clone();

            tokio::spawn(async move {
                receiver_loop(recv_socket, devices, pending_state, cancel).await;
            })
        };

        let device_ttl = Duration::from_secs(3 * discovery_interval_secs);

        Ok(Self {
            send_socket,
            devices,
            pending_state,
            cancel,
            receiver_handle,
            discovery_timeout,
            device_ttl,
        })
    }

    /// Send a multicast scan request and wait for responses.
    pub async fn discover(&self) -> Result<()> {
        let scan_msg = r#"{"msg":{"cmd":"scan","data":{"account_topic":"reserve"}}}"#;
        let target: SocketAddr = (MULTICAST_ADDR, DEVICE_PORT).into();
        self.send_socket
            .send_to(scan_msg.as_bytes(), target)
            .await?;
        tokio::time::sleep(self.discovery_timeout).await;
        Ok(())
    }

    /// Look up the IP address of a discovered device.
    pub fn get_device_ip(&self, id: &DeviceId) -> Result<IpAddr> {
        // Using std RwLock–style try_read would be wrong here since we hold
        // a tokio RwLock. However, `get_device_ip` is sync. We use
        // `blocking_read` which is safe as long as it's not called from an
        // async context where the lock is also acquired asynchronously.
        // In practice the callers (Wave 2 command methods) will use the
        // async version; this is a convenience for sync contexts.
        //
        // For now, we use try_read to avoid blocking.
        let guard = self
            .devices
            .try_read()
            .map_err(|_| GoveeError::BackendUnavailable("device cache lock contention".into()))?;
        guard
            .get(id)
            .map(|d| d.ip)
            .ok_or_else(|| GoveeError::DeviceNotFound(id.to_string()))
    }
}

/// Background receiver loop. Reads UDP packets from the multicast socket
/// and updates the device cache and pending state maps.
async fn receiver_loop(
    socket: UdpSocket,
    devices: Arc<RwLock<HashMap<DeviceId, DiscoveredDevice>>>,
    pending_state: Arc<Mutex<HashMap<IpAddr, oneshot::Sender<DeviceState>>>>,
    cancel: CancellationToken,
) {
    let mut buf = [0u8; 4096];

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, src_addr)) => {
                        let data = &buf[..len];
                        if let Err(e) = handle_packet(
                            data,
                            src_addr.ip(),
                            &devices,
                            &pending_state,
                        ).await {
                            tracing::warn!("failed to parse LAN packet from {}: {}", src_addr, e);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("recv_from error: {}", e);
                    }
                }
            }
        }
    }
}

/// Parse a single UDP packet from a Govee device.
async fn handle_packet(
    data: &[u8],
    source_ip: IpAddr,
    devices: &Arc<RwLock<HashMap<DeviceId, DiscoveredDevice>>>,
    pending_state: &Arc<Mutex<HashMap<IpAddr, oneshot::Sender<DeviceState>>>>,
) -> Result<()> {
    let envelope: serde_json::Value = serde_json::from_slice(data)?;
    let msg = envelope
        .get("msg")
        .ok_or_else(|| json_error("missing 'msg' field"))?;
    let cmd = msg
        .get("cmd")
        .and_then(|v| v.as_str())
        .ok_or_else(|| json_error("missing 'cmd' field"))?;

    match cmd {
        "scan" => {
            let data_obj = msg
                .get("data")
                .ok_or_else(|| json_error("missing 'data' in scan"))?;
            handle_scan_response(data_obj, source_ip, devices).await?;
        }
        "devStatus" => {
            let data_obj = msg
                .get("data")
                .ok_or_else(|| json_error("missing 'data' in devStatus"))?;
            handle_dev_status(data_obj, source_ip, pending_state).await?;
        }
        _ => {
            tracing::warn!("unknown LAN command: {}", cmd);
        }
    }

    Ok(())
}

/// Handle a scan response — upsert device into cache.
async fn handle_scan_response(
    data: &serde_json::Value,
    source_ip: IpAddr,
    devices: &Arc<RwLock<HashMap<DeviceId, DiscoveredDevice>>>,
) -> Result<()> {
    let ip_str = data.get("ip").and_then(|v| v.as_str());
    let device_mac = data
        .get("device")
        .and_then(|v| v.as_str())
        .ok_or_else(|| json_error("missing 'device' in scan"))?;
    let sku = data
        .get("sku")
        .and_then(|v| v.as_str())
        .ok_or_else(|| json_error("missing 'sku' in scan"))?;

    // Use the IP from the data field if present, otherwise fall back to source.
    let ip: IpAddr = ip_str.and_then(|s| s.parse().ok()).unwrap_or(source_ip);

    let device_id = DeviceId::new(device_mac)?;

    let discovered = DiscoveredDevice {
        ip,
        device_id: device_id.clone(),
        sku: sku.to_string(),
        last_seen: Instant::now(),
    };

    let mut cache = devices.write().await;
    cache.insert(device_id, discovered);

    Ok(())
}

/// Handle a devStatus response — resolve a pending oneshot if present.
async fn handle_dev_status(
    data: &serde_json::Value,
    source_ip: IpAddr,
    pending_state: &Arc<Mutex<HashMap<IpAddr, oneshot::Sender<DeviceState>>>>,
) -> Result<()> {
    let state = parse_dev_status(data)?;

    let mut pending = pending_state.lock().await;
    if let Some(sender) = pending.remove(&source_ip) {
        // Ignore send error — receiver may have dropped.
        let _ = sender.send(state);
    }

    Ok(())
}

/// Parse `devStatus` data into a `DeviceState`.
fn parse_dev_status(data: &serde_json::Value) -> Result<DeviceState> {
    let on_off = data.get("onOff").and_then(|v| v.as_u64()).unwrap_or(0);
    let on = on_off == 1;

    let brightness_raw = data.get("brightness").and_then(|v| v.as_u64()).unwrap_or(0);
    let brightness = brightness_raw.min(100) as u8;

    let color = if let Some(c) = data.get("color") {
        let r = c.get("r").and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8;
        let g = c.get("g").and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8;
        let b = c.get("b").and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8;
        Color::new(r, g, b)
    } else {
        Color::new(0, 0, 0)
    };

    let color_temp = data
        .get("colorTemInKelvin")
        .and_then(|v| v.as_u64())
        .map(|v| u32::try_from(v).unwrap_or(u32::MAX));

    DeviceState::new(on, brightness, color, color_temp, false)
}

#[async_trait]
impl GoveeBackend for LocalBackend {
    async fn list_devices(&self) -> Result<Vec<Device>> {
        // Discover if cache is empty or all entries are expired.
        {
            let cache = self.devices.read().await;
            let now = Instant::now();
            let all_expired = cache.is_empty()
                || cache
                    .values()
                    .all(|d| now.duration_since(d.last_seen) > self.device_ttl);
            if all_expired {
                drop(cache);
                self.discover().await?;
            }
        }

        // Prune expired entries and collect results.
        let mut cache = self.devices.write().await;
        let now = Instant::now();
        cache.retain(|_, d| now.duration_since(d.last_seen) <= self.device_ttl);

        let mut result = Vec::new();
        for d in cache.values() {
            if let Ok(id) = DeviceId::new(d.device_id.as_str()) {
                result.push(Device {
                    id,
                    model: d.sku.clone(),
                    name: d.sku.clone(),
                    alias: None,
                    backend: BackendType::Local,
                });
            }
        }

        Ok(result)
    }

    async fn get_state(&self, _id: &DeviceId) -> Result<DeviceState> {
        Err(GoveeError::NotImplemented("LocalBackend::get_state".into()))
    }

    async fn set_power(&self, _id: &DeviceId, _on: bool) -> Result<()> {
        Err(GoveeError::NotImplemented("LocalBackend::set_power".into()))
    }

    async fn set_brightness(&self, _id: &DeviceId, _value: u8) -> Result<()> {
        Err(GoveeError::NotImplemented(
            "LocalBackend::set_brightness".into(),
        ))
    }

    async fn set_color(&self, _id: &DeviceId, _color: Color) -> Result<()> {
        Err(GoveeError::NotImplemented("LocalBackend::set_color".into()))
    }

    async fn set_color_temp(&self, _id: &DeviceId, _kelvin: u32) -> Result<()> {
        Err(GoveeError::NotImplemented(
            "LocalBackend::set_color_temp".into(),
        ))
    }

    fn backend_type(&self) -> BackendType {
        BackendType::Local
    }
}

impl fmt::Debug for LocalBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.devices.try_read().map(|c| c.len()).unwrap_or(0);
        f.debug_struct("LocalBackend")
            .field("devices", &count)
            .field("listen_port", &LISTEN_PORT)
            .finish()
    }
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.receiver_handle.abort();
    }
}

// Compile-time Send + Sync assertions.
fn _assert_send_sync<T: Send + Sync>() {}

fn _assert_local_backend_send_sync() {
    _assert_send_sync::<LocalBackend>();
}

fn _assert_boxed_backend_send_sync() {
    _assert_send_sync::<Box<dyn GoveeBackend>>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scan_response_json() {
        let json = serde_json::json!({
            "ip": "192.168.1.100",
            "device": "AA:BB:CC:DD:EE:FF:00:11",
            "sku": "H6076",
            "bleVersionHard": "1.0",
            "bleVersionSoft": "1.0",
            "wifiVersionHard": "1.0",
            "wifiVersionSoft": "1.0"
        });

        let ip_str = json.get("ip").and_then(|v| v.as_str()).unwrap();
        let ip: IpAddr = ip_str.parse().unwrap();
        let device_mac = json.get("device").and_then(|v| v.as_str()).unwrap();
        let sku = json.get("sku").and_then(|v| v.as_str()).unwrap();

        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        assert_eq!(device_mac, "AA:BB:CC:DD:EE:FF:00:11");
        assert_eq!(sku, "H6076");

        let device_id = DeviceId::new(device_mac).unwrap();
        assert_eq!(device_id.as_str(), "AA:BB:CC:DD:EE:FF:00:11");
    }

    #[test]
    fn parse_dev_status_json() {
        let json = serde_json::json!({
            "onOff": 1,
            "brightness": 100,
            "color": {"r": 255, "g": 100, "b": 0},
            "colorTemInKelvin": 7200
        });

        let state = parse_dev_status(&json).unwrap();
        assert!(state.on);
        assert_eq!(state.brightness, 100);
        assert_eq!(state.color, Color::new(255, 100, 0));
        assert_eq!(state.color_temp_kelvin, Some(7200));
        assert!(!state.stale);
    }

    #[test]
    fn parse_dev_status_off() {
        let json = serde_json::json!({
            "onOff": 0,
            "brightness": 50,
            "color": {"r": 0, "g": 0, "b": 0},
            "colorTemInKelvin": 0
        });

        let state = parse_dev_status(&json).unwrap();
        assert!(!state.on);
        assert_eq!(state.brightness, 50);
    }

    #[test]
    fn parse_dev_status_clamps_brightness() {
        let json = serde_json::json!({
            "onOff": 1,
            "brightness": 200,
            "color": {"r": 0, "g": 0, "b": 0}
        });

        let state = parse_dev_status(&json).unwrap();
        assert_eq!(state.brightness, 100);
    }

    #[test]
    fn parse_dev_status_clamps_color() {
        let json = serde_json::json!({
            "onOff": 1,
            "brightness": 50,
            "color": {"r": 300, "g": 500, "b": 999}
        });

        let state = parse_dev_status(&json).unwrap();
        assert_eq!(state.color, Color::new(255, 255, 255));
    }

    #[test]
    fn parse_dev_status_missing_color_temp() {
        let json = serde_json::json!({
            "onOff": 1,
            "brightness": 50,
            "color": {"r": 128, "g": 128, "b": 128}
        });

        let state = parse_dev_status(&json).unwrap();
        assert_eq!(state.color_temp_kelvin, None);
    }

    #[test]
    fn cache_ttl_expiry() {
        let device_ttl = Duration::from_secs(30);

        // Fresh device — not expired.
        let fresh = DiscoveredDevice {
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            device_id: DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap(),
            sku: "H6076".into(),
            last_seen: Instant::now(),
        };
        let now = Instant::now();
        assert!(now.duration_since(fresh.last_seen) <= device_ttl);

        // Expired device — simulated by old last_seen.
        let expired = DiscoveredDevice {
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)),
            device_id: DeviceId::new("11:22:33:44:55:66").unwrap(),
            sku: "H6076".into(),
            last_seen: Instant::now() - Duration::from_secs(60),
        };
        let now = Instant::now();
        assert!(now.duration_since(expired.last_seen) > device_ttl);
    }

    #[test]
    fn discovered_device_to_device_conversion() {
        let discovered = DiscoveredDevice {
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)),
            device_id: DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap(),
            sku: "H6076".into(),
            last_seen: Instant::now(),
        };

        let device = Device {
            id: DeviceId::new(discovered.device_id.as_str()).unwrap(),
            model: discovered.sku.clone(),
            name: discovered.sku.clone(),
            alias: None,
            backend: BackendType::Local,
        };

        assert_eq!(device.id.as_str(), "AA:BB:CC:DD:EE:FF:00:11");
        assert_eq!(device.model, "H6076");
        assert_eq!(device.name, "H6076");
        assert!(device.alias.is_none());
        assert_eq!(device.backend, BackendType::Local);
    }

    #[test]
    fn send_sync_assertions() {
        _assert_local_backend_send_sync();
        _assert_boxed_backend_send_sync();
    }
}
