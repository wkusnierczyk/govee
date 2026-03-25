use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tracing::{debug, instrument, warn};

use super::GoveeBackend;
use crate::capability::{Capability, CapabilityValue, DynamicSceneValue};
use crate::error::{GoveeError, Result};
use crate::types::{BackendType, Color, Device, DeviceId, DeviceState, DiyScene, LightScene};

/// Default base URL for the Govee cloud API.
const DEFAULT_BASE_URL: &str = "https://developer-api.govee.com";

/// Base URL for the Govee new (OpenAPI) cloud API.
const NEW_API_BASE: &str = "https://openapi.api.govee.com";

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
fn build_client(custom_ua: Option<&str>) -> std::result::Result<Client, reqwest::Error> {
    let ua = custom_ua.map(|s| s.to_string()).unwrap_or_else(user_agent);
    Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .user_agent(ua)
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
    /// Base URL for the new (OpenAPI) Govee API.
    new_api_base: reqwest::Url,
    api_key: String,
    /// Device ID → model mapping, populated by `list_devices`.
    /// Required because `GET /v1/devices/state` needs both `device` and `model`.
    device_models: RwLock<HashMap<DeviceId, String>>,
    /// Device ID → capability list, populated by `list_devices` via v2 API.
    device_capabilities: RwLock<HashMap<DeviceId, Vec<Capability>>>,
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
    pub fn new(
        api_key: String,
        base_url: Option<String>,
        user_agent: Option<String>,
    ) -> Result<Self> {
        // Validate user_agent: reject control characters (bytes < 0x20 or DEL 0x7F).
        if let Some(ref ua) = user_agent
            && ua.bytes().any(|b| b < 0x20 || b == 0x7f)
        {
            return Err(GoveeError::InvalidConfig(
                "user_agent contains invalid characters".into(),
            ));
        }

        let raw = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let parsed = reqwest::Url::parse(&raw)
            .map_err(|e| GoveeError::InvalidConfig(format!("invalid base URL \"{raw}\": {e}")))?;
        if parsed.scheme() != "https" && !is_loopback(&parsed) {
            return Err(GoveeError::InvalidConfig(format!(
                "base URL must use HTTPS (HTTP is only allowed for loopback addresses), got: {raw}"
            )));
        }
        let client = build_client(user_agent.as_deref())
            .map_err(|e| GoveeError::InvalidConfig(format!("failed to build HTTP client: {e}")))?;
        let new_api_base =
            reqwest::Url::parse(NEW_API_BASE).expect("NEW_API_BASE constant is a valid URL");
        Ok(Self {
            client,
            base_url: parsed,
            new_api_base,
            api_key,
            device_models: RwLock::new(HashMap::new()),
            device_capabilities: RwLock::new(HashMap::new()),
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

    /// Map a new-API envelope error code and message to a domain error.
    fn map_new_api_code_err(code: u32, msg: String) -> GoveeError {
        match code {
            400 => GoveeError::Api {
                code: 400,
                message: msg,
            },
            401 => GoveeError::Api {
                code: 401,
                message: msg,
            },
            404 => GoveeError::Api {
                code: 404,
                message: format!("not found: {msg}"),
            },
            429 => GoveeError::RateLimited {
                retry_after_secs: 60,
            },
            other => match u16::try_from(other) {
                Ok(c) => GoveeError::Api {
                    code: c,
                    message: msg,
                },
                Err(_) => GoveeError::Api {
                    code: 500,
                    message: format!("unexpected status code {other}: {msg}"),
                },
            },
        }
    }

    /// POST to the new (OpenAPI) Govee endpoint.
    ///
    /// Wraps `payload` in a `{requestId, payload}` envelope and deserializes
    /// the `{requestId, msg, code, payload}` response envelope, mapping all
    /// documented error codes to existing `GoveeError` variants.
    pub async fn new_api_post<Req, Res>(&self, path: &str, payload: Req) -> Result<Res>
    where
        Req: serde::Serialize,
        Res: serde::de::DeserializeOwned,
    {
        let url = self.new_api_base.join(path).map_err(|e| {
            GoveeError::InvalidConfig(format!("invalid new API path '{path}': {e}"))
        })?;
        let envelope = NewApiRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            payload,
        };
        let response = self
            .client
            .post(url)
            .header("Govee-API-Key", &self.api_key)
            .json(&envelope)
            .send()
            .await?;

        let response = self.check_response(response).await?;
        let body: NewApiResponse<serde_json::Value> = response.json().await?;
        match body.code {
            200 => {
                let payload =
                    serde_json::from_value::<Res>(body.payload).map_err(GoveeError::Json)?;
                Ok(payload)
            }
            code => Err(Self::map_new_api_code_err(code, body.msg)),
        }
    }

    /// GET from the new (OpenAPI) Govee endpoint.
    ///
    /// Deserializes the `{requestId, msg, code, payload}` response envelope,
    /// mapping all documented error codes to existing `GoveeError` variants.
    pub async fn new_api_get<Res, Q>(&self, path: &str, query_params: Option<&Q>) -> Result<Res>
    where
        Res: serde::de::DeserializeOwned,
        Q: serde::Serialize + ?Sized,
    {
        let url = self.new_api_base.join(path).map_err(|e| {
            GoveeError::InvalidConfig(format!("invalid new API path '{path}': {e}"))
        })?;
        let mut request = self.client.get(url).header("Govee-API-Key", &self.api_key);
        if let Some(params) = query_params {
            request = request.query(params);
        }
        let response = request.send().await?;

        let response = self.check_response(response).await?;
        let body: NewApiResponse<serde_json::Value> = response.json().await?;
        match body.code {
            200 => {
                let payload =
                    serde_json::from_value::<Res>(body.payload).map_err(GoveeError::Json)?;
                Ok(payload)
            }
            code => Err(Self::map_new_api_code_err(code, body.msg)),
        }
    }

    /// Query device state using the v2 OpenAPI endpoint.
    ///
    /// POSTs to `/router/api/v1/device/state` with `{sku, device}` body.
    /// Maps known capability instances to `DeviceState` fields; stores
    /// all unknown capabilities in `DeviceState::raw` keyed by `"type/instance"`.
    async fn get_state_v2(&self, id: &DeviceId) -> Result<DeviceState> {
        let sku = self.get_model(id)?;

        #[derive(serde::Deserialize)]
        struct V2StateResponse {
            #[allow(dead_code)]
            sku: String,
            #[allow(dead_code)]
            device: String,
            capabilities: Vec<crate::capability::CapabilityState>,
        }

        let payload = serde_json::json!({
            "sku": sku,
            "device": id.as_str(),
        });

        let resp: V2StateResponse = self
            .new_api_post("/router/api/v1/device/state", payload)
            .await?;

        let mut on = false;
        let mut brightness: u8 = 0;
        let mut color = Color::new(0, 0, 0);
        let mut color_temp_kelvin: Option<u32> = None;
        let mut raw: HashMap<String, serde_json::Value> = HashMap::new();

        for cap in resp.capabilities {
            match (cap.type_.as_str(), cap.instance.as_str()) {
                ("devices.capabilities.on_off", "powerSwitch") => {
                    if let Some(v) = cap.state.value.as_u64() {
                        on = v == 1;
                    }
                }
                ("devices.capabilities.range", "brightness") => {
                    if let Some(v) = cap.state.value.as_u64() {
                        brightness = v.min(100) as u8;
                    }
                }
                ("devices.capabilities.color_setting", "colorRgb") => {
                    if let Some(v) = cap.state.value.as_u64() {
                        let v = v as u32;
                        let r = ((v >> 16) & 0xFF) as u8;
                        let g = ((v >> 8) & 0xFF) as u8;
                        let b = (v & 0xFF) as u8;
                        color = Color::new(r, g, b);
                    }
                }
                ("devices.capabilities.color_setting", "colorTemperatureK") => {
                    if let Some(v) = cap.state.value.as_u64() {
                        color_temp_kelvin = u32::try_from(v).ok();
                    }
                }
                _ => {
                    let key = format!("{}/{}", cap.type_, cap.instance);
                    raw.insert(key, cap.state.value);
                }
            }
        }

        DeviceState::new(on, brightness, color, color_temp_kelvin, false, raw)
    }

    /// List available DIY scenes for a device.
    ///
    /// Returns an empty list if the device does not advertise the
    /// `devices.capabilities.dynamic_scene` capability. Otherwise POSTs to
    /// `/router/api/v1/device/diy-scenes` and parses the response.
    async fn list_diy_scenes_cloud(&self, id: &DeviceId) -> Result<Vec<DiyScene>> {
        // Check capability — skip the network call if the device doesn't support it.
        let has_cap = self
            .get_capabilities(id)
            .map(|caps| caps.iter().any(|c| c.type_.contains("dynamic_scene")))
            .unwrap_or(false);

        if !has_cap {
            return Ok(vec![]);
        }

        let sku = self.get_model(id)?;
        let payload = serde_json::json!({
            "sku": sku,
            "device": id.as_str(),
        });

        let resp: DiySceneListResponse = self
            .new_api_post("/router/api/v1/device/diy-scenes", payload)
            .await?;

        Ok(resp
            .diy_scenes
            .into_iter()
            .map(|s| DiyScene {
                id: s.scene_id,
                name: s.scene_name,
            })
            .collect())
    }

    /// Override the new API base URL.
    ///
    /// Returns `GoveeError::InvalidConfig` if `base` is not a valid URL or
    /// does not use HTTPS (unless the host is a loopback address, which allows
    /// HTTP for local testing with wiremock).
    ///
    /// This is provided primarily for testing (pointing at a mock server).
    /// In production, the default `NEW_API_BASE` constant is used.
    #[doc(hidden)]
    pub fn with_new_api_base(mut self, base: &str) -> Result<Self> {
        let parsed = reqwest::Url::parse(base).map_err(|e| {
            GoveeError::InvalidConfig(format!("invalid new API base URL \"{base}\": {e}"))
        })?;
        if parsed.scheme() != "https" && !is_loopback(&parsed) {
            return Err(GoveeError::InvalidConfig(format!(
                "new API base URL must use HTTPS (HTTP is only allowed for loopback addresses), got: {base}"
            )));
        }
        self.new_api_base = parsed;
        Ok(self)
    }

    /// Fetch the device list from the v2 (OpenAPI) endpoint.
    ///
    /// Returns a list of `V2Device` with capabilities. Callers handle errors
    /// from this method and fall back to the legacy v1 list on failure.
    async fn list_devices_v2(&self) -> Result<Vec<V2Device>> {
        self.new_api_get::<Vec<V2Device>, ()>("/router/api/v1/user/devices", None)
            .await
    }

    /// Return the capability list for a device, if known.
    ///
    /// Populated after a successful call to `list_devices`. Returns `None`
    /// if the device was not in the v2 response or `list_devices` has not
    /// been called yet.
    pub fn get_capabilities(&self, id: &DeviceId) -> Option<Vec<Capability>> {
        self.device_capabilities
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    /// Send a control command via the v2 OpenAPI endpoint.
    ///
    /// Maps `CapabilityValue` variants to the `POST /router/api/v1/device/control`
    /// payload format. Returns `GoveeError::NotImplemented` for variants that have
    /// no v2 mapping, and `GoveeError::DeviceNotFound` if the device SKU is not cached.
    async fn control_v2(&self, id: &DeviceId, value: CapabilityValue) -> Result<()> {
        let sku = self.get_model(id)?;

        let (type_, instance, json_value) = match value {
            CapabilityValue::OnOff(v) => (
                "devices.capabilities.on_off",
                "powerSwitch",
                serde_json::json!(v),
            ),
            CapabilityValue::Brightness(v) => (
                "devices.capabilities.range",
                "brightness",
                serde_json::json!(v),
            ),
            CapabilityValue::Rgb(v) => (
                "devices.capabilities.color_setting",
                "colorRgb",
                serde_json::json!(v),
            ),
            CapabilityValue::ColorTempK(v) => (
                "devices.capabilities.color_setting",
                "colorTemperatureK",
                serde_json::json!(v),
            ),
            CapabilityValue::DynamicScene(scene_value) => (
                "devices.capabilities.dynamic_scene",
                "lightScene",
                serde_json::to_value(&scene_value).map_err(GoveeError::Json)?,
            ),
            CapabilityValue::DiyScene(v) => (
                "devices.capabilities.dynamic_scene",
                "diyScene",
                serde_json::json!(v),
            ),
            other => {
                return Err(GoveeError::NotImplemented(format!(
                    "control_v2 does not support {other:?}"
                )));
            }
        };

        let payload = ControlPayload {
            sku: &sku,
            device: id.as_str(),
            capability: CapabilityPayload {
                type_,
                instance,
                value: json_value,
            },
        };

        self.new_api_post::<_, serde_json::Value>("/router/api/v1/device/control", payload)
            .await
            .map(|_| ())
    }
}

impl CloudBackend {
    /// Query the current state of a device using the legacy v1 API.
    ///
    /// Uses an internal device→model cache and will automatically refresh
    /// the cache with `list_devices` on a cache miss. Returns
    /// `DeviceNotFound` if the device is still unknown after refreshing.
    async fn v1_get_state(&self, id: &DeviceId) -> Result<DeviceState> {
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
        debug!(device = %id, stale = state.stale, "queried v1 device state");
        Ok(state)
    }
}

// --- DIY scene response types (internal) ---

/// Response payload for `POST /router/api/v1/device/diy-scenes`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiySceneListResponse {
    diy_scenes: Vec<RawDiyScene>,
}

/// A single DIY scene entry from the API.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDiyScene {
    scene_id: u32,
    scene_name: String,
}

// --- New (OpenAPI) request/response envelope types (internal) ---

/// Request envelope for the new Govee OpenAPI.
#[derive(serde::Serialize)]
struct NewApiRequest<T: serde::Serialize> {
    #[serde(rename = "requestId")]
    request_id: String,
    payload: T,
}

/// Response envelope for the new Govee OpenAPI.
#[derive(serde::Deserialize)]
struct NewApiResponse<T> {
    #[serde(rename = "requestId")]
    #[allow(dead_code)]
    request_id: Option<String>,
    msg: String,
    code: u32,
    #[serde(alias = "data")]
    payload: T,
}

// --- v2 (OpenAPI) device list types (internal) ---

/// A single device as returned by the v2 `GET /router/api/v1/user/devices` endpoint.
#[derive(serde::Deserialize)]
struct V2Device {
    sku: String,
    device: String,
    #[serde(rename = "deviceName")]
    device_name: String,
    capabilities: Vec<Capability>,
}

// --- v2 control payload types (internal) ---

/// Top-level payload for `POST /router/api/v1/device/control`.
#[derive(serde::Serialize)]
struct ControlPayload<'a> {
    sku: &'a str,
    device: &'a str,
    capability: CapabilityPayload,
}

/// The capability object inside a v2 control payload.
#[derive(serde::Serialize)]
struct CapabilityPayload {
    #[serde(rename = "type")]
    type_: &'static str,
    instance: &'static str,
    value: serde_json::Value,
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

    DeviceState::new(on, brightness, color, color_temp, !online, HashMap::new())
}

/// Response envelope from `PUT /v1/devices/control`.
#[derive(serde::Deserialize)]
struct V1ControlResponse {
    code: u16,
    message: String,
}

// --- Scene list response types (internal) ---

/// Top-level payload returned by `POST /router/api/v1/device/scenes`.
#[derive(serde::Deserialize)]
struct SceneListResponse {
    scenes: Vec<RawScene>,
}

/// A single scene entry in the scenes list response.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawScene {
    scene_id: u32,
    scene_name: String,
    scene_param_id: u32,
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
        // --- v2 attempt (primary): device list with capabilities ---
        let v2_result = self.list_devices_v2().await;
        match &v2_result {
            Ok(devs) => {
                // Atomic-swap capabilities cache so it exactly reflects the latest v2 response.
                let new_caps: HashMap<DeviceId, Vec<Capability>> = devs
                    .iter()
                    .filter_map(|d| {
                        DeviceId::new(&d.device)
                            .ok()
                            .map(|id| (id, d.capabilities.clone()))
                    })
                    .collect();
                *self
                    .device_capabilities
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_caps;
            }
            Err(e) => debug!("v2 device list unavailable: {e}"),
        }

        // --- v1 fetch (legacy: supplemental entries or sole source if v2 failed) ---
        let v1_result: Result<Vec<Device>> = async {
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
            body.data
                .devices
                .into_iter()
                .map(V1Device::into_domain)
                .collect::<Result<Vec<_>>>()
        }
        .await;

        // If v2 failed, fall back to v1 only (propagate v1 error if both failed).
        if v2_result.is_err() {
            let legacy = v1_result?;
            let new_map: HashMap<DeviceId, String> = legacy
                .iter()
                .map(|d| (d.id.clone(), d.model.clone()))
                .collect();
            *self
                .device_models
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_map;
            debug!(count = legacy.len(), "listed cloud devices (v1 only)");
            return Ok(legacy);
        }

        let v2_devices = v2_result.unwrap(); // safe: checked above
        let legacy = match v1_result {
            Ok(devs) => Some(devs),
            Err(e) => {
                debug!("v1 device list unavailable, using v2 only: {e}");
                None
            }
        };

        // Merge: v2 devices take precedence; v1-only devices appended.
        let v2_ids: HashSet<String> = v2_devices.iter().map(|d| d.device.clone()).collect();
        let mut result: Vec<Device> = v2_devices
            .iter()
            .filter_map(|d| {
                let id = DeviceId::new(&d.device).ok()?;
                Some(Device {
                    id,
                    model: d.sku.clone(),
                    name: d.device_name.clone(),
                    alias: None,
                    backend: BackendType::Cloud,
                })
            })
            .collect();
        if let Some(v1) = legacy {
            for dev in v1 {
                if !v2_ids.contains(dev.id.as_str()) {
                    debug!(device_id = %dev.id, "device not in v2 list, using legacy entry");
                    result.push(dev);
                }
            }
        }

        // Update model cache from merged result.
        {
            let new_map: HashMap<DeviceId, String> = result
                .iter()
                .map(|d| (d.id.clone(), d.model.clone()))
                .collect();
            *self
                .device_models
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_map;
        }

        debug!(count = result.len(), "listed cloud devices");
        Ok(result)
    }

    /// Query the current state of a device.
    ///
    /// Tries the v2 OpenAPI endpoint first; falls back to the legacy v1 API
    /// on `DeviceNotFound` or `Api` errors. `RateLimited` and other transient
    /// errors are propagated directly so callers can respect back-off signals.
    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn get_state(&self, id: &DeviceId) -> Result<DeviceState> {
        match self.get_state_v2(id).await {
            Ok(state) => Ok(state),
            Err(GoveeError::DeviceNotFound(_)) | Err(GoveeError::Api { .. }) => {
                debug!(device_id = %id, "v2 state unavailable, falling back to legacy");
                self.v1_get_state(id).await
            }
            Err(e) => Err(e),
        }
    }

    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn set_power(&self, id: &DeviceId, on: bool) -> Result<()> {
        let cap_value = CapabilityValue::OnOff(if on { 1 } else { 0 });
        match self.control_v2(id, cap_value).await {
            Ok(()) => Ok(()),
            Err(GoveeError::DeviceNotFound(_)) | Err(GoveeError::Api { code: 404, .. }) => {
                debug!(device_id = %id, "v2 control failed, falling back to legacy");
                let value = if on { "on" } else { "off" };
                self.send_control(id, "turn", serde_json::json!(value))
                    .await
            }
            Err(e) => Err(e),
        }
    }

    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn set_brightness(&self, id: &DeviceId, value: u8) -> Result<()> {
        if value > 100 {
            return Err(GoveeError::InvalidBrightness(value));
        }
        let cap_value = CapabilityValue::Brightness(value);
        match self.control_v2(id, cap_value).await {
            Ok(()) => Ok(()),
            Err(GoveeError::DeviceNotFound(_)) | Err(GoveeError::Api { code: 404, .. }) => {
                debug!(device_id = %id, "v2 control failed, falling back to legacy");
                self.send_control(id, "brightness", serde_json::json!(value))
                    .await
            }
            Err(e) => Err(e),
        }
    }

    #[instrument(skip(self, color), fields(backend = "cloud", device = %id))]
    async fn set_color(&self, id: &DeviceId, color: Color) -> Result<()> {
        let packed = color.to_rgb24();
        let cap_value = CapabilityValue::Rgb(packed);
        match self.control_v2(id, cap_value).await {
            Ok(()) => Ok(()),
            Err(GoveeError::DeviceNotFound(_)) | Err(GoveeError::Api { code: 404, .. }) => {
                debug!(device_id = %id, "v2 control failed, falling back to legacy");
                self.send_control(
                    id,
                    "color",
                    serde_json::json!({"r": color.r, "g": color.g, "b": color.b}),
                )
                .await
            }
            Err(e) => Err(e),
        }
    }

    /// Set color temperature in Kelvin (1-10000).
    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn set_color_temp(&self, id: &DeviceId, kelvin: u32) -> Result<()> {
        if kelvin == 0 || kelvin > 10000 {
            return Err(GoveeError::InvalidConfig(
                "color temperature must be 1-10000K".into(),
            ));
        }
        let cap_value = CapabilityValue::ColorTempK(kelvin);
        match self.control_v2(id, cap_value).await {
            Ok(()) => Ok(()),
            Err(GoveeError::DeviceNotFound(_)) | Err(GoveeError::Api { code: 404, .. }) => {
                debug!(device_id = %id, "v2 control failed, falling back to legacy");
                self.send_control(id, "colorTem", serde_json::json!(kelvin))
                    .await
            }
            Err(e) => Err(e),
        }
    }

    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn list_scenes(&self, id: &DeviceId) -> Result<Vec<LightScene>> {
        // Skip the network call if the device has no dynamic_scene capability.
        if let Some(caps) = self.get_capabilities(id) {
            let has_dynamic_scene = caps
                .iter()
                .any(|c| c.type_ == "devices.capabilities.dynamic_scene");
            if !has_dynamic_scene {
                return Ok(vec![]);
            }
        } else {
            return Ok(vec![]);
        }

        let sku = self.get_model(id)?;
        let payload = serde_json::json!({
            "sku": sku,
            "device": id.as_str(),
        });

        let resp: SceneListResponse = self
            .new_api_post("/router/api/v1/device/scenes", payload)
            .await?;

        let scenes = resp
            .scenes
            .into_iter()
            .map(|s| LightScene {
                id: s.scene_id,
                name: s.scene_name,
                param_id: s.scene_param_id,
            })
            .collect();

        Ok(scenes)
    }

    #[instrument(skip(self, scene), fields(backend = "cloud", device = %id))]
    async fn set_scene(&self, id: &DeviceId, scene: &LightScene) -> Result<()> {
        self.control_v2(
            id,
            CapabilityValue::DynamicScene(DynamicSceneValue::Preset {
                param_id: scene.param_id,
                id: scene.id,
            }),
        )
        .await
    }

    #[instrument(skip(self), fields(backend = "cloud", device = %id))]
    async fn list_diy_scenes(&self, id: &DeviceId) -> Result<Vec<DiyScene>> {
        self.list_diy_scenes_cloud(id).await
    }

    #[instrument(skip(self, scene), fields(backend = "cloud", device = %id, scene_id = scene.id))]
    async fn set_diy_scene(&self, id: &DeviceId, scene: &DiyScene) -> Result<()> {
        self.control_v2(id, CapabilityValue::DiyScene(scene.id))
            .await
    }

    fn backend_type(&self) -> BackendType {
        BackendType::Cloud
    }
}

impl std::fmt::Debug for CloudBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cached = self.device_models.read().map(|m| m.len()).unwrap_or(0);
        let cached_caps = self
            .device_capabilities
            .read()
            .map(|m| m.len())
            .unwrap_or(0);
        f.debug_struct("CloudBackend")
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &"[REDACTED]")
            .field("cached_devices", &cached)
            .field("cached_capabilities", &cached_caps)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_http_non_loopback() {
        let result = CloudBackend::new("key".into(), Some("http://example.com".into()), None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, GoveeError::InvalidConfig(_)));
        assert!(err.to_string().contains("HTTPS"));
    }

    #[test]
    fn allows_http_loopback() {
        assert!(
            CloudBackend::new("key".into(), Some("http://127.0.0.1:8080".into()), None).is_ok()
        );
        assert!(
            CloudBackend::new("key".into(), Some("http://localhost:8080".into()), None).is_ok()
        );
        assert!(CloudBackend::new("key".into(), Some("http://[::1]:8080".into()), None).is_ok());
    }

    #[test]
    fn rejects_invalid_url() {
        let result = CloudBackend::new("key".into(), Some("not a url".into()), None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GoveeError::InvalidConfig(_)));
    }

    #[test]
    fn accepts_https_base_url() {
        let result = CloudBackend::new("key".into(), Some("https://example.com".into()), None);
        assert!(result.is_ok());
    }

    #[test]
    fn default_base_url_is_https() {
        let backend = CloudBackend::new("key".into(), None, None).unwrap();
        assert_eq!(backend.base_url.scheme(), "https");
    }

    #[test]
    fn trailing_slash_normalized() {
        let backend =
            CloudBackend::new("key".into(), Some("https://example.com/".into()), None).unwrap();
        let url = backend.base_url.join("v1/devices").unwrap();
        assert_eq!(url.path(), "/v1/devices");
    }

    #[test]
    fn debug_redacts_api_key() {
        let backend = CloudBackend::new("super-secret-key".into(), None, None).unwrap();
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

    #[test]
    fn custom_user_agent_accepted() {
        let result = CloudBackend::new("key".into(), None, Some("my-app/1.0".into()));
        assert!(result.is_ok());
    }

    #[test]
    fn user_agent_crlf_rejected() {
        let result = CloudBackend::new("key".into(), None, Some("foo\r\nbar".into()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, GoveeError::InvalidConfig(_)));
        assert!(
            err.to_string()
                .contains("user_agent contains invalid characters")
        );
    }

    #[test]
    fn user_agent_null_byte_rejected() {
        let result = CloudBackend::new("key".into(), None, Some("foo\0bar".into()));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GoveeError::InvalidConfig(_)));
    }

    #[tokio::test]
    async fn set_color_temp_kelvin_zero_rejected() {
        let backend = CloudBackend::new("key".into(), None, None).unwrap();
        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        // Validation runs before cache lookup, so no need to populate cache.
        let result = backend.set_color_temp(&id, 0).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GoveeError::InvalidConfig(_)));
    }

    #[tokio::test]
    async fn set_color_temp_kelvin_above_10000_rejected() {
        let backend = CloudBackend::new("key".into(), None, None).unwrap();
        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        let result = backend.set_color_temp(&id, 10001).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GoveeError::InvalidConfig(_)));
    }

    #[test]
    fn map_new_api_code_err_overflows_u16() {
        // code > u16::MAX (65535) falls into the Err(_) branch of try_from
        let err = CloudBackend::map_new_api_code_err(100_000, "overflow".into());
        assert!(matches!(err, GoveeError::Api { code: 500, .. }));
    }

    #[test]
    fn with_new_api_base_rejects_invalid_url() {
        let backend = CloudBackend::new("key".into(), None, None).unwrap();
        let result = backend.with_new_api_base("not a url");
        assert!(matches!(result.unwrap_err(), GoveeError::InvalidConfig(_)));
    }

    #[test]
    fn allows_http_ipv6_loopback() {
        let result = CloudBackend::new("key".into(), Some("http://[::1]/".into()), None);
        assert!(result.is_ok());
    }

    #[test]
    fn get_capabilities_returns_none_when_empty() {
        let backend = CloudBackend::new("key".into(), None, None).unwrap();
        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        assert!(backend.get_capabilities(&id).is_none());
    }

    #[tokio::test]
    async fn control_v2_not_implemented_for_raw_variant() {
        let backend = CloudBackend::new("key".into(), None, None).unwrap();
        let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
        // Populate model cache so we get past DeviceNotFound.
        backend
            .device_models
            .write()
            .unwrap()
            .insert(id.clone(), "H6076".into());
        let result = backend
            .control_v2(
                &id,
                crate::capability::CapabilityValue::Raw(serde_json::json!({})),
            )
            .await;
        assert!(matches!(result.unwrap_err(), GoveeError::NotImplemented(_)));
    }
}
