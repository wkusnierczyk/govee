use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tracing::{debug, instrument, warn};

use super::GoveeBackend;
use crate::error::{GoveeError, Result};
use crate::types::{BackendType, Color, Device, DeviceId, DeviceState};

/// Default base URL for the Govee cloud API.
const DEFAULT_BASE_URL: &str = "https://developer-api.govee.com";

/// Default request timeout (covers the entire request lifecycle).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default connection timeout (TCP + TLS handshake).
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum retry-after delay honored from a 429 response (RT-M07-02).
/// Prevents a server from blocking the client indefinitely.
const MAX_RETRY_AFTER_SECS: u64 = 300;

/// Maximum number of retries for transient errors and rate limiting.
const MAX_RETRIES: u32 = 3;

/// User-Agent header identifying the library and version.
fn user_agent() -> String {
    format!("govee/{}", env!("CARGO_PKG_VERSION"))
}

/// Check if a URL points to a loopback address (127.0.0.1, ::1, localhost).
///
/// This allows HTTP for local test servers (e.g., wiremock) while enforcing
/// HTTPS for all remote hosts. Callers should not rely on this for production
/// configurations — if a production config accidentally resolves to localhost,
/// API keys would be sent over plaintext.
fn is_loopback(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some("localhost") | Some("127.0.0.1") | Some("[::1]") => true,
        // Url::host_str() returns IPv6 in brackets (e.g., "[::1]"),
        // but IpAddr::parse expects bare addresses. Strip brackets.
        Some(host) => host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

/// Build a configured `reqwest::Client` with timeouts and User-Agent.
fn build_client() -> std::result::Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .user_agent(user_agent())
        .build()
}

/// Cloud API backend using the Govee v1 REST API.
///
/// Authenticates via `Govee-API-Key` header. Base URL defaults to
/// `https://developer-api.govee.com` but can be overridden for testing.
///
/// # Security
///
/// - **MITM risk:** Uses the system CA bundle for TLS verification
///   (no certificate pinning). A corporate MITM proxy or CA-installing
///   malware can intercept all traffic, capturing the API key. The
///   Govee API does not provide key rotation or revocation. (RT-08)
///
/// # Resource lifecycle
///
/// - **HTTP client:** A single `reqwest::Client` is created at construction
///   time and shared across all requests. `reqwest` pools connections
///   internally — no manual connection management needed.
/// - **Device model cache:** `std::sync::RwLock<HashMap>` holding device→model
///   mappings. Populated by `list_devices`, read by `get_state`/`send_control`.
///   Lock is held only for brief lookups/swaps and never across `.await` points.
pub struct CloudBackend {
    client: Client,
    base_url: reqwest::Url,
    api_key: String,
    /// Device ID → model mapping, populated by `list_devices`.
    /// Required because `GET /v1/devices/state` needs both `device` and `model`.
    device_models: RwLock<HashMap<DeviceId, String>>,
    /// Single-flight guard for auto-refresh of device_models cache (RT-M07-04).
    refresh_guard: tokio::sync::Mutex<()>,
}

impl CloudBackend {
    /// Create a new `CloudBackend`.
    ///
    /// Returns `GoveeError::InvalidConfig` if `base_url` is not a valid URL
    /// or does not use HTTPS (unless the host is a loopback address, which
    /// allows HTTP for local testing with wiremock).
    ///
    /// # Security
    ///
    /// `base_url` is a privileged parameter. If an attacker controls it,
    /// all API calls (including the API key) are sent to the attacker's
    /// endpoint. Callers must never derive `base_url` from untrusted
    /// input. (RT-09)
    pub fn new(api_key: String, base_url: Option<String>) -> Result<Self> {
        let raw = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let parsed = reqwest::Url::parse(&raw)
            .map_err(|e| GoveeError::InvalidConfig(format!("invalid base URL \"{raw}\": {e}")))?;
        if parsed.scheme() != "https" && !is_loopback(&parsed) {
            return Err(GoveeError::InvalidConfig(format!(
                "base URL must use HTTPS (HTTP is only allowed for loopback addresses), got: {raw}"
            )));
        }
        let client = build_client()
            .map_err(|e| GoveeError::InvalidConfig(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            base_url: parsed,
            api_key,
            device_models: RwLock::new(HashMap::new()),
            refresh_guard: tokio::sync::Mutex::new(()),
        })
    }

    /// Look up the model for a device ID from the internal cache.
    ///
    /// Returns `DeviceNotFound` if the device is not cached.
    /// Call `list_devices` first to populate the cache.
    fn get_model(&self, id: &DeviceId) -> Result<String> {
        let models = self
            .device_models
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        models.get(id).cloned().ok_or_else(|| {
            GoveeError::DeviceNotFound(format!(
                "{} (call list_devices first to populate the device cache)",
                id
            ))
        })
    }

    /// Send a control command to a device via `PUT /v1/devices/control`.
    ///
    /// Parses the response body for API-level errors (HTTP 200 with
    /// `code != 200` in the JSON envelope).
    async fn send_control(
        &self,
        id: &DeviceId,
        cmd_name: &str,
        cmd_value: serde_json::Value,
    ) -> Result<()> {
        let model = self.get_model(id)?;
        let url = self
            .base_url
            .join("v1/devices/control")
            .map_err(|e| GoveeError::InvalidConfig(format!("failed to build URL: {e}")))?;

        let payload = serde_json::json!({
            "device": id.as_str(),
            "model": model,
            "cmd": {
                "name": cmd_name,
                "value": cmd_value,
            }
        });

        for attempt in 0..=MAX_RETRIES {
            let result = self
                .client
                .put(url.clone())
                .header("Govee-API-Key", &self.api_key)
                .json(&payload)
                .send()
                .await;

            let response = match result {
                Ok(r) => r,
                Err(e) => {
                    let err = GoveeError::Request(e);
                    if let Some(delay) = Self::retry_delay(&err, attempt)
                        && attempt < MAX_RETRIES
                    {
                        debug!(attempt, ?delay, "retrying after request error");
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(err);
                }
            };

            match self.check_response(response).await {
                Ok(response) => {
                    let body: V1ControlResponse = response.json().await?;
                    if body.code != 200 {
                        return Err(GoveeError::Api {
                            code: body.code,
                            message: body.message,
                        });
                    }
                    debug!(device = %id, cmd = cmd_name, "sent control command");
                    return Ok(());
                }
                Err(err) => {
                    if let Some(delay) = Self::retry_delay(&err, attempt)
                        && attempt < MAX_RETRIES
                    {
                        debug!(attempt, ?delay, "retrying after error");
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        unreachable!("retry loop always returns on the final attempt")
    }

    /// Check an HTTP response for rate limiting and error status codes.
    ///
    /// Returns the response unchanged on success (2xx). For 429, returns
    /// `RateLimited`. For other non-2xx, returns `Api` with the response body.
    async fn check_response(&self, response: reqwest::Response) -> Result<reqwest::Response> {
        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after_secs = parse_retry_after(&response);
            warn!(retry_after_secs, "rate limited by Govee API");
            return Err(GoveeError::RateLimited { retry_after_secs });
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));
            return Err(GoveeError::Api {
                code: status.as_u16(),
                message: body,
            });
        }
        Ok(response)
    }

    /// Compute retry delay for a failed request.
    ///
    /// Returns `Some(duration)` if the error is retryable, `None` otherwise.
    fn retry_delay(err: &GoveeError, attempt: u32) -> Option<Duration> {
        match err {
            GoveeError::RateLimited { retry_after_secs } => {
                let capped = (*retry_after_secs).min(MAX_RETRY_AFTER_SECS);
                Some(Duration::from_secs(capped))
            }
            GoveeError::Request(_) | GoveeError::Api { code: 500.., .. } => {
                // Exponential backoff: 1s, 2s, 4s (deterministic, no randomness)
                let delay_ms = 1000u64 * 2u64.pow(attempt);
                Some(Duration::from_millis(delay_ms))
            }
            _ => None,
        }
    }
}

// --- v1 API response types (internal) ---

/// Top-level response envelope from `GET /v1/devices`.
#[derive(serde::Deserialize)]
struct V1DevicesResponse {
    data: V1DevicesData,
    code: u16,
    message: String,
}

/// The `data` field inside a v1 devices response.
#[derive(serde::Deserialize)]
struct V1DevicesData {
    devices: Vec<V1Device>,
}

/// A single device as returned by the v1 API.
///
/// Only fields we use are declared; extra API fields (`retrievable`,
/// `supportCmds`, etc.) are silently ignored by serde.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct V1Device {
    device: String,
    model: String,
    device_name: String,
}

impl V1Device {
    /// Convert the API device into our domain `Device`.
    ///
    /// Returns an error if the MAC address is invalid.
    fn into_domain(self) -> Result<Device> {
        let id = DeviceId::new(&self.device)?;
        Ok(Device {
            id,
            model: self.model,
            name: self.device_name,
            alias: None,
            backend: BackendType::Cloud,
        })
    }
}

/// Top-level response envelope from `GET /v1/devices/state`.
#[derive(serde::Deserialize)]
struct V1StateResponse {
    data: V1StateData,
    code: u16,
    message: String,
}

/// The `data` field inside a v1 state response.
///
/// Only `properties` is used; `device` and `model` echo back the request
/// params and are ignored by serde's default permissive parsing.
#[derive(serde::Deserialize)]
struct V1StateData {
    properties: Vec<serde_json::Value>,
}

/// Build a `DeviceState` from the v1 property array.
///
/// The v1 API returns state as `[{"online": true}, {"powerState": "on"}, ...]`
/// — each element is a JSON object with a single key. We parse each as a
/// `serde_json::Value` map and extract known keys.
///
/// Values are clamped to valid ranges before construction:
/// - brightness: clamped to 0–100 on the u64 before cast
/// - color components: clamped to 0–255 on the u64 before cast
/// - colorTem: clamped to u32::MAX via saturating conversion
fn build_state_from_properties(properties: Vec<serde_json::Value>) -> Result<DeviceState> {
    let mut on = false;
    let mut brightness: u8 = 0;
    let mut color = Color::new(0, 0, 0);
    let mut color_temp: Option<u32> = None;
    let mut online = true;

    for prop in properties {
        if let Some(v) = prop.get("online").and_then(|v| v.as_bool()) {
            online = v;
        }
        if let Some(v) = prop.get("powerState").and_then(|v| v.as_str()) {
            on = v == "on";
        }
        if let Some(v) = prop.get("brightness").and_then(|v| v.as_u64()) {
            brightness = v.min(100) as u8;
        }
        if let Some(obj) = prop.get("color").and_then(|v| v.as_object()) {
            let r = obj.get("r").and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8;
            let g = obj.get("g").and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8;
            let b = obj.get("b").and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8;
            color = Color::new(r, g, b);
        }
        if let Some(v) = prop.get("colorTem").and_then(|v| v.as_u64()) {
            color_temp = Some(u32::try_from(v).unwrap_or(u32::MAX));
        }
    }

    DeviceState::new(on, brightness, color, color_temp, !online)
}

/// Response envelope from `PUT /v1/devices/control`.
#[derive(serde::Deserialize)]
struct V1ControlResponse {
    code: u16,
    message: String,
}

/// Parse the `Retry-After` header value as seconds.
fn parse_retry_after(response: &reqwest::Response) -> u64 {
    response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
}

#[async_trait]
impl GoveeBackend for CloudBackend {
    #[instrument(skip(self), fields(backend = "cloud"))]
    async fn list_devices(&self) -> Result<Vec<Device>> {
        let url = self
            .base_url
            .join("v1/devices")
            .map_err(|e| GoveeError::InvalidConfig(format!("failed to build URL: {e}")))?;
        let response = self
            .client
            .get(url)
            .header("Govee-API-Key", &self.api_key)
            .send()
            .await?;

        let response = self.check_response(response).await?;

        let body: V1DevicesResponse = response.json().await?;
        if body.code != 200 {
            return Err(GoveeError::Api {
                code: body.code,
                message: body.message,
            });
        }

        let devices: Vec<Device> = body
            .data
            .devices
            .into_iter()
            .map(V1Device::into_domain)
            .collect::<Result<Vec<_>>>()?;

        // Cache device→model mappings for get_state (atomic swap).
        {
            let new_map: HashMap<DeviceId, String> = devices
                .iter()
                .map(|d| (d.id.clone(), d.model.clone()))
                .collect();
            let mut models = self
                .device_models
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *models = new_map;
        }

        debug!(count = devices.len(), "listed cloud devices");
        Ok(devices)
    }

    /// Query the current state of a device.
    ///
    /// Uses an internal device→model cache and will automatically refresh
    /// the cache with `list_devices` on a cache miss. Returns
    /// `DeviceNotFound` if the device is still unknown after refreshing.
    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn get_state(&self, id: &DeviceId) -> Result<DeviceState> {
        // Auto-refresh device cache on miss (single-flight guard).
        let model = match self.get_model(id) {
            Ok(m) => m,
            Err(_) => {
                let _guard = self.refresh_guard.lock().await;
                // Re-check after acquiring lock (another task may have refreshed).
                match self.get_model(id) {
                    Ok(m) => m,
                    Err(_) => {
                        debug!(device = %id, "model cache miss, refreshing device list");
                        self.list_devices().await?;
                        self.get_model(id)?
                    }
                }
            }
        };
        let mut url = self
            .base_url
            .join("v1/devices/state")
            .map_err(|e| GoveeError::InvalidConfig(format!("failed to build URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("device", id.as_str())
            .append_pair("model", &model);

        let response = self
            .client
            .get(url)
            .header("Govee-API-Key", &self.api_key)
            .send()
            .await?;

        let response = self.check_response(response).await?;

        let body: V1StateResponse = response.json().await?;
        if body.code != 200 {
            return Err(GoveeError::Api {
                code: body.code,
                message: body.message,
            });
        }

        let state = build_state_from_properties(body.data.properties)?;
        debug!(device = %id, stale = state.stale, "queried device state");
        Ok(state)
    }

    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn set_power(&self, id: &DeviceId, on: bool) -> Result<()> {
        let value = if on { "on" } else { "off" };
        self.send_control(id, "turn", serde_json::json!(value))
            .await
    }

    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn set_brightness(&self, id: &DeviceId, value: u8) -> Result<()> {
        if value > 100 {
            return Err(GoveeError::InvalidBrightness(value));
        }
        self.send_control(id, "brightness", serde_json::json!(value))
            .await
    }

    #[instrument(skip(self, color), fields(backend = "cloud", device = %id))]
    async fn set_color(&self, id: &DeviceId, color: Color) -> Result<()> {
        self.send_control(
            id,
            "color",
            serde_json::json!({"r": color.r, "g": color.g, "b": color.b}),
        )
        .await
    }

    /// Set color temperature in Kelvin (1-10000).
    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn set_color_temp(&self, id: &DeviceId, kelvin: u32) -> Result<()> {
        if kelvin == 0 || kelvin > 10000 {
            return Err(GoveeError::InvalidConfig(
                "color temperature must be 1-10000K".into(),
            ));
        }
        self.send_control(id, "colorTem", serde_json::json!(kelvin))
            .await
    }

    fn backend_type(&self) -> BackendType {
        BackendType::Cloud
    }
}

impl std::fmt::Debug for CloudBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cached = self.device_models.read().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("CloudBackend")
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &"[REDACTED]")
            .field("cached_devices", &cached)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_http_non_loopback() {
        let result = CloudBackend::new("key".into(), Some("http://example.com".into()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, GoveeError::InvalidConfig(_)));
        assert!(err.to_string().contains("HTTPS"));
    }

    #[test]
    fn allows_http_loopback() {
        assert!(CloudBackend::new("key".into(), Some("http://127.0.0.1:8080".into())).is_ok());
        assert!(CloudBackend::new("key".into(), Some("http://localhost:8080".into())).is_ok());
        assert!(CloudBackend::new("key".into(), Some("http://[::1]:8080".into())).is_ok());
    }

    #[test]
    fn rejects_invalid_url() {
        let result = CloudBackend::new("key".into(), Some("not a url".into()));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GoveeError::InvalidConfig(_)));
    }

    #[test]
    fn accepts_https_base_url() {
        let result = CloudBackend::new("key".into(), Some("https://example.com".into()));
        assert!(result.is_ok());
    }

    #[test]
    fn default_base_url_is_https() {
        let backend = CloudBackend::new("key".into(), None).unwrap();
        assert_eq!(backend.base_url.scheme(), "https");
    }

    #[test]
    fn trailing_slash_normalized() {
        let backend = CloudBackend::new("key".into(), Some("https://example.com/".into())).unwrap();
        let url = backend.base_url.join("v1/devices").unwrap();
        assert_eq!(url.path(), "/v1/devices");
    }

    #[test]
    fn debug_redacts_api_key() {
        let backend = CloudBackend::new("super-secret-key".into(), None).unwrap();
        let debug = format!("{:?}", backend);
        assert!(!debug.contains("super-secret-key"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn v1_device_into_domain() {
        let v1 = V1Device {
            device: "AA:BB:CC:DD:EE:FF".into(),
            model: "H6076".into(),
            device_name: "Kitchen Light".into(),
        };
        let device = v1.into_domain().unwrap();
        assert_eq!(device.id.as_str(), "AA:BB:CC:DD:EE:FF");
        assert_eq!(device.model, "H6076");
        assert_eq!(device.name, "Kitchen Light");
        assert_eq!(device.backend, BackendType::Cloud);
        assert!(device.alias.is_none());
    }

    #[test]
    fn v1_device_invalid_mac_returns_error() {
        let v1 = V1Device {
            device: "not-a-mac".into(),
            model: "H6076".into(),
            device_name: "Bad Device".into(),
        };
        assert!(v1.into_domain().is_err());
    }

    #[test]
    fn build_state_all_properties() {
        let props: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                {"online": true},
                {"powerState": "on"},
                {"brightness": 75},
                {"color": {"r": 255, "g": 128, "b": 0}},
                {"colorTem": 5000}
            ]"#,
        )
        .unwrap();
        let state = build_state_from_properties(props).unwrap();
        assert!(state.on);
        assert_eq!(state.brightness, 75);
        assert_eq!(state.color, Color::new(255, 128, 0));
        assert_eq!(state.color_temp_kelvin, Some(5000));
        assert!(!state.stale);
    }

    #[test]
    fn build_state_offline_is_stale() {
        let props: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"online": false}, {"powerState": "off"}, {"brightness": 50}]"#,
        )
        .unwrap();
        let state = build_state_from_properties(props).unwrap();
        assert!(state.stale);
        assert!(!state.on);
    }

    #[test]
    fn build_state_clamps_brightness() {
        let props: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"brightness": 200}]"#).unwrap();
        let state = build_state_from_properties(props).unwrap();
        assert_eq!(state.brightness, 100);
    }

    #[test]
    fn build_state_clamps_brightness_above_255() {
        let props: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"brightness": 300}]"#).unwrap();
        let state = build_state_from_properties(props).unwrap();
        assert_eq!(state.brightness, 100);
    }

    #[test]
    fn build_state_clamps_color_above_255() {
        let props: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"color": {"r": 300, "g": 500, "b": 1000}}]"#).unwrap();
        let state = build_state_from_properties(props).unwrap();
        assert_eq!(state.color, Color::new(255, 255, 255));
    }

    #[test]
    fn build_state_unknown_properties_ignored() {
        let props: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"unknownProp": 42}]"#).unwrap();
        let state = build_state_from_properties(props).unwrap();
        assert!(!state.on);
        assert_eq!(state.brightness, 0);
    }

    #[test]
    fn user_agent_contains_version() {
        let ua = user_agent();
        assert!(ua.starts_with("govee/"));
        assert!(ua.contains(env!("CARGO_PKG_VERSION")));
    }
}
