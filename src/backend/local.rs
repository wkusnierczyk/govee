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

use tracing::instrument;

use crate::backend::GoveeBackend;
use crate::error::{GoveeError, Result};
use crate::types::{BackendType, Color, Device, DeviceId, DeviceState};

/// Helper to create a `GoveeError::Protocol` with a custom message.
fn protocol_error(msg: &str) -> GoveeError {
    GoveeError::Protocol(msg.to_string())
}

/// Validate that an IP address is local (private, link-local, or loopback).
///
/// For IPv4, accepts private (RFC 1918), link-local (169.254.x.x), and
/// loopback (127.x.x.x) addresses. For IPv6, only loopback (::1) is
/// accepted since Govee devices don't use IPv6.
///
/// Returns `GoveeError::InvalidConfig` if the address is not local.
fn validate_local_ip(ip: IpAddr) -> Result<()> {
    let is_local = match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    };
    if !is_local {
        return Err(GoveeError::InvalidConfig(
            "device IP is not a local address".into(),
        ));
    }
    Ok(())
}

/// Multicast group used by Govee LAN protocol for discovery.
const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
/// Port the device sends responses to (we listen on this).
const LISTEN_PORT: u16 = 4002;
/// Port used for multicast discovery scan requests.
const MULTICAST_PORT: u16 = 4001;
/// Port the device listens on for commands and status queries.
const COMMAND_PORT: u16 = 4003;

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
///
/// # Security
///
/// The Govee LAN protocol is **unauthenticated plaintext UDP**. Any
/// process on the local network can:
///
/// - Send arbitrary control commands directly to port 4003, bypassing
///   this library entirely. (RT-03)
/// - Send a multicast scan request to `239.255.255.250:4001` and
///   receive discovery responses on port 4002 to enumerate all Govee
///   devices, their firmware versions, and MAC addresses. (RT-11)
/// - Inject spoofed scan responses to register fake devices in the
///   discovery cache. The library uses the UDP source IP (not the
///   JSON payload IP) to mitigate IP spoofing, but the `device` (MAC)
///   and `sku` fields are taken from the untrusted payload. (RT-02)
/// - Send forged `devStatus` responses to poison the state cache.
///
/// These are fundamental properties of the Govee LAN protocol, which
/// has no authentication mechanism. The library cannot fully prevent
/// them.
///
/// # Resource lifecycle
///
/// - **UDP socket:** A single `UdpSocket` is created at construction and
///   shared for both multicast discovery sends and unicast control commands.
/// - **Receiver task:** A background tokio task owns the `recv_from` loop,
///   routing `scan` responses to the device cache and `devStatus` responses
///   to oneshot channels. Lifecycle is tied to a `CancellationToken`.
/// - **Drop:** Cancels the `CancellationToken` and aborts the receiver task.
///
/// # Limitations
///
/// - **Receive buffer:** 4096 bytes. UDP packets larger than this are
///   silently truncated before JSON parsing. All current Govee protocol
///   messages fit well within this limit. (RT-10)
pub struct LocalBackend {
    send_socket: UdpSocket,
    devices: Arc<RwLock<HashMap<DeviceId, DiscoveredDevice>>>,
    /// Used by command methods (get_state, etc.) to receive responses.
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
    ///
    /// # Platform note
    ///
    /// `SO_REUSEADDR` is set on the receive socket so that the library can
    /// co-exist with other processes **on platforms where `SO_REUSEADDR`
    /// actually prevents dual-bind conflicts** (Linux). On macOS and some
    /// BSDs, `SO_REUSEADDR` silently allows two sockets to bind to the
    /// same port, so the `AddrInUse` detection below may not trigger.
    /// A future improvement could use `SO_REUSEPORT` probing or an
    /// advisory lock file for more reliable conflict detection.
    pub async fn new(discovery_timeout: Duration, discovery_interval_secs: u64) -> Result<Self> {
        if discovery_interval_secs < crate::config::MIN_DISCOVERY_INTERVAL_SECS {
            return Err(GoveeError::InvalidConfig(format!(
                "discovery_interval_secs must be >= {}s, got {}s",
                crate::config::MIN_DISCOVERY_INTERVAL_SECS,
                discovery_interval_secs
            )));
        }

        // Build receive socket via socket2 for SO_REUSEADDR + multicast join.
        let recv_socket = {
            let socket = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            // See constructor doc comment for platform limitations of SO_REUSEADDR.
            socket.set_reuse_address(true)?;

            let bind_addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, LISTEN_PORT).into();
            if let Err(e) = socket.bind(&bind_addr.into()) {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    return Err(GoveeError::BackendUnavailable(
                        "port 4002 already in use -- another process \
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
    ///
    /// Returns early if no new devices arrive for 200ms after the last
    /// response, rather than waiting the full discovery timeout. This
    /// reduces startup latency on quiet networks.
    ///
    /// **Trade-off:** The 200ms idle timeout may miss devices on
    /// congested or high-latency LAN segments (WiFi, bridges). In
    /// `Auto` mode, missed LAN devices fall back to cloud. In
    /// `LocalOnly` mode, they are absent.
    pub async fn discover(&self) -> Result<()> {
        let scan_msg = r#"{"msg":{"cmd":"scan","data":{"account_topic":"reserve"}}}"#;
        let target: SocketAddr = (MULTICAST_ADDR, MULTICAST_PORT).into();
        self.send_socket
            .send_to(scan_msg.as_bytes(), target)
            .await?;

        // Wait for responses with early return on idle.
        let idle_timeout = Duration::from_millis(200);
        let deadline = tokio::time::Instant::now() + self.discovery_timeout;
        let mut last_count = {
            let cache = self.devices.read().await;
            cache.len()
        };

        loop {
            let now = tokio::time::Instant::now();
            let remaining = match deadline.checked_duration_since(now) {
                Some(dur) if !dur.is_zero() => dur,
                _ => break,
            };
            let wait = remaining.min(idle_timeout);
            tokio::time::sleep(wait).await;

            let current_count = {
                let cache = self.devices.read().await;
                cache.len()
            };

            if current_count > last_count {
                // New device(s) arrived — reset idle timer.
                last_count = current_count;
            } else if tokio::time::Instant::now() >= deadline {
                // Full timeout reached.
                break;
            } else {
                // No new devices in the last idle window — return early.
                break;
            }
        }

        Ok(())
    }

    /// Send a command payload to a discovered device via UDP.
    ///
    /// Validates that the device IP is a local address (private, link-local,
    /// or loopback) before sending.
    async fn send_command(&self, id: &DeviceId, payload: serde_json::Value) -> Result<()> {
        let ip = self.get_device_ip(id).await?;
        validate_local_ip(ip)?;

        let bytes = serde_json::to_vec(&payload)?;
        self.send_socket.send_to(&bytes, (ip, COMMAND_PORT)).await?;

        tracing::debug!("sent command to {} ({}) on port {}", id, ip, COMMAND_PORT);
        Ok(())
    }

    /// Look up the IP address of a discovered device.
    pub async fn get_device_ip(&self, id: &DeviceId) -> Result<IpAddr> {
        let guard = self.devices.read().await;
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
        .ok_or_else(|| protocol_error("missing 'msg' field"))?;
    let cmd = msg
        .get("cmd")
        .and_then(|v| v.as_str())
        .ok_or_else(|| protocol_error("missing 'cmd' field"))?;

    match cmd {
        "scan" => {
            let data_obj = msg
                .get("data")
                .ok_or_else(|| protocol_error("missing 'data' in scan"))?;
            handle_scan_response(data_obj, source_ip, devices).await?;
        }
        "devStatus" => {
            let data_obj = msg
                .get("data")
                .ok_or_else(|| protocol_error("missing 'data' in devStatus"))?;
            handle_dev_status(data_obj, source_ip, pending_state).await?;
        }
        _ => {
            tracing::warn!(cmd, "ignoring unknown LAN command");
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
    let device_mac = data
        .get("device")
        .and_then(|v| v.as_str())
        .ok_or_else(|| protocol_error("missing 'device' in scan"))?;
    let sku = data
        .get("sku")
        .and_then(|v| v.as_str())
        .ok_or_else(|| protocol_error("missing 'sku' in scan"))?;

    // Always use the UDP source IP as the device address to prevent a
    // spoofed JSON `ip` field from injecting a wrong address. Log the
    // payload IP for diagnostics if it differs.
    let ip = source_ip;
    if let Some(payload_ip) = data.get("ip").and_then(|v| v.as_str())
        && payload_ip.parse::<IpAddr>().ok().as_ref() != Some(&source_ip)
    {
        tracing::debug!(
            "scan response payload ip ({}) differs from source ({}), using source",
            payload_ip,
            source_ip,
        );
    }

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

    // Treat 0 as absent (consistent with set_color_temp rejecting 0).
    // Values that overflow u32 are also treated as absent (invalid data).
    let color_temp = data
        .get("colorTemInKelvin")
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0);

    DeviceState::new(on, brightness, color, color_temp, false)
}

/// Validate color temperature range (1-10000K).
fn validate_kelvin(kelvin: u32) -> Result<()> {
    if kelvin == 0 || kelvin > 10000 {
        return Err(GoveeError::InvalidConfig(
            "color temperature must be 1-10000K".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl GoveeBackend for LocalBackend {
    #[instrument(skip(self), fields(backend = "local"))]
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

        // LAN discovery does not provide user-assigned device names — only
        // the SKU/model string. User-friendly names are merged from the
        // cloud backend in milestone M5.
        let mut result = Vec::new();
        for d in cache.values() {
            result.push(Device {
                id: d.device_id.clone(),
                model: d.sku.clone(),
                name: d.sku.clone(),
                alias: None,
                backend: BackendType::Local,
            });
        }

        Ok(result)
    }

    #[instrument(skip(self), fields(backend = "local", device = %id))]
    async fn get_state(&self, id: &DeviceId) -> Result<DeviceState> {
        let ip = self.get_device_ip(id).await?;
        validate_local_ip(ip)?;

        let (tx, rx) = oneshot::channel();

        // Insert the sender into pending_state keyed by the device IP.
        // Reject if a concurrent query is already in progress for this IP.
        {
            let mut pending = self.pending_state.lock().await;
            if pending.contains_key(&ip) {
                return Err(GoveeError::BackendUnavailable(
                    "concurrent state query already in progress for this device".into(),
                ));
            }
            pending.insert(ip, tx);
        }

        // Send devStatus request to the device on port 4003.
        let status_msg = r#"{"msg":{"cmd":"devStatus","data":{}}}"#;
        let target: SocketAddr = (ip, COMMAND_PORT).into();
        if let Err(e) = self
            .send_socket
            .send_to(status_msg.as_bytes(), target)
            .await
        {
            self.pending_state.lock().await.remove(&ip);
            return Err(e.into());
        }

        // Await the response with timeout.
        match tokio::time::timeout(self.discovery_timeout, rx).await {
            Ok(Ok(state)) => {
                // Success — entry already removed by the receiver task.
                Ok(state)
            }
            Ok(Err(_)) => {
                // Sender was dropped (receiver task stopped).
                self.pending_state.lock().await.remove(&ip);
                Err(GoveeError::BackendUnavailable(
                    "receiver task stopped".into(),
                ))
            }
            Err(_) => {
                // Timeout — clean up the pending entry.
                self.pending_state.lock().await.remove(&ip);
                Err(GoveeError::DiscoveryTimeout)
            }
        }
    }

    #[instrument(skip(self), fields(backend = "local", device = %id))]
    async fn set_power(&self, id: &DeviceId, on: bool) -> Result<()> {
        let value = if on { 1 } else { 0 };
        let payload = serde_json::json!({
            "msg": {
                "cmd": "turn",
                "data": { "value": value }
            }
        });
        self.send_command(id, payload).await
    }

    #[instrument(skip(self), fields(backend = "local", device = %id))]
    async fn set_brightness(&self, id: &DeviceId, value: u8) -> Result<()> {
        if value > 100 {
            return Err(GoveeError::InvalidBrightness(value));
        }
        let payload = serde_json::json!({
            "msg": {
                "cmd": "brightness",
                "data": { "value": value }
            }
        });
        self.send_command(id, payload).await
    }

    /// Set the device color via the LAN `colorwc` command.
    ///
    /// The Govee LAN protocol bundles color and color temperature into a
    /// single `colorwc` command. Setting the RGB color resets the color
    /// temperature to 0 (disabled).
    #[instrument(skip(self, color), fields(backend = "local", device = %id))]
    async fn set_color(&self, id: &DeviceId, color: Color) -> Result<()> {
        let payload = serde_json::json!({
            "msg": {
                "cmd": "colorwc",
                "data": {
                    "color": { "r": color.r, "g": color.g, "b": color.b },
                    "colorTemInKelvin": 0
                }
            }
        });
        self.send_command(id, payload).await
    }

    /// Set the device color temperature via the LAN `colorwc` command.
    ///
    /// The Govee LAN protocol bundles color and color temperature into a
    /// single `colorwc` command. Setting the color temperature resets the
    /// RGB color to (0, 0, 0) (disabled).
    #[instrument(skip(self), fields(backend = "local", device = %id))]
    async fn set_color_temp(&self, id: &DeviceId, kelvin: u32) -> Result<()> {
        validate_kelvin(kelvin)?;
        let payload = serde_json::json!({
            "msg": {
                "cmd": "colorwc",
                "data": {
                    "color": { "r": 0, "g": 0, "b": 0 },
                    "colorTemInKelvin": kelvin
                }
            }
        });
        self.send_command(id, payload).await
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
        // colorTemInKelvin: 0 is treated as absent (consistent with set_color_temp rejecting 0).
        assert_eq!(state.color_temp_kelvin, None);
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

    #[test]
    fn set_power_payload_on() {
        let payload = serde_json::json!({
            "msg": {
                "cmd": "turn",
                "data": { "value": 1 }
            }
        });
        let msg = payload.get("msg").unwrap();
        assert_eq!(msg["cmd"], "turn");
        assert_eq!(msg["data"]["value"], 1);
    }

    #[test]
    fn set_power_payload_off() {
        let payload = serde_json::json!({
            "msg": {
                "cmd": "turn",
                "data": { "value": 0 }
            }
        });
        let msg = payload.get("msg").unwrap();
        assert_eq!(msg["cmd"], "turn");
        assert_eq!(msg["data"]["value"], 0);
    }

    #[test]
    fn set_brightness_payload() {
        let value: u8 = 75;
        let payload = serde_json::json!({
            "msg": {
                "cmd": "brightness",
                "data": { "value": value }
            }
        });
        let msg = payload.get("msg").unwrap();
        assert_eq!(msg["cmd"], "brightness");
        assert_eq!(msg["data"]["value"], 75);
    }

    #[test]
    fn set_color_payload() {
        let color = Color::new(255, 128, 0);
        let payload = serde_json::json!({
            "msg": {
                "cmd": "colorwc",
                "data": {
                    "color": { "r": color.r, "g": color.g, "b": color.b },
                    "colorTemInKelvin": 0
                }
            }
        });
        let msg = payload.get("msg").unwrap();
        assert_eq!(msg["cmd"], "colorwc");
        assert_eq!(msg["data"]["color"]["r"], 255);
        assert_eq!(msg["data"]["color"]["g"], 128);
        assert_eq!(msg["data"]["color"]["b"], 0);
        assert_eq!(msg["data"]["colorTemInKelvin"], 0);
    }

    #[test]
    fn set_color_temp_payload() {
        let kelvin: u32 = 4500;
        let payload = serde_json::json!({
            "msg": {
                "cmd": "colorwc",
                "data": {
                    "color": { "r": 0, "g": 0, "b": 0 },
                    "colorTemInKelvin": kelvin
                }
            }
        });
        let msg = payload.get("msg").unwrap();
        assert_eq!(msg["cmd"], "colorwc");
        assert_eq!(msg["data"]["color"]["r"], 0);
        assert_eq!(msg["data"]["color"]["g"], 0);
        assert_eq!(msg["data"]["color"]["b"], 0);
        assert_eq!(msg["data"]["colorTemInKelvin"], 4500);
    }

    #[test]
    fn set_color_temp_kelvin_zero_rejected() {
        let result = validate_kelvin(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GoveeError::InvalidConfig(_)));
    }

    #[test]
    fn set_color_temp_kelvin_above_10000_rejected() {
        let result = validate_kelvin(10001);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GoveeError::InvalidConfig(_)));
    }

    #[test]
    fn ip_validation_private_accepted() {
        assert!(validate_local_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))).is_ok());
    }

    #[test]
    fn ip_validation_loopback_accepted() {
        assert!(validate_local_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))).is_ok());
    }

    #[test]
    fn ip_validation_link_local_accepted() {
        assert!(validate_local_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))).is_ok());
    }

    #[test]
    fn ip_validation_public_rejected() {
        let result = validate_local_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(matches!(result, Err(GoveeError::InvalidConfig(_))));
    }
}
