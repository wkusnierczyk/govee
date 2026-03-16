use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tracing::debug;

use super::GoveeBackend;
use crate::error::{GoveeError, Result};
use crate::types::{BackendType, Color, Device, DeviceId, DeviceState};

/// Default base URL for the Govee cloud API.
const DEFAULT_BASE_URL: &str = "https://developer-api.govee.com";

/// Default request timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Cloud API backend using the Govee v1 REST API.
///
/// Authenticates via `Govee-API-Key` header. Base URL defaults to
/// `https://developer-api.govee.com` but can be overridden for testing.
pub struct CloudBackend {
    client: Client,
    base_url: reqwest::Url,
    api_key: String,
    /// Device ID → model mapping, populated by `list_devices`.
    /// Required because `GET /v1/devices/state` needs both `device` and `model`.
    device_models: RwLock<HashMap<DeviceId, String>>,
}

impl CloudBackend {
    /// Create a new `CloudBackend`.
    ///
    /// Returns `GoveeError::InvalidConfig` if `base_url` is not a valid URL
    /// or does not use HTTPS.
    pub fn new(api_key: String, base_url: Option<String>) -> Result<Self> {
        let raw = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let parsed = reqwest::Url::parse(&raw)
            .map_err(|e| GoveeError::InvalidConfig(format!("invalid base URL \"{raw}\": {e}")))?;
        if parsed.scheme() != "https" {
            return Err(GoveeError::InvalidConfig(format!(
                "base URL must use HTTPS, got: {raw}"
            )));
        }
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| GoveeError::InvalidConfig(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            base_url: parsed,
            api_key,
            device_models: RwLock::new(HashMap::new()),
        })
    }

    /// Create a `CloudBackend` for testing with an arbitrary base URL.
    ///
    /// Skips HTTPS enforcement — intended for wiremock tests with `http://`
    /// mock servers.
    ///
    /// Only available when the `test-utils` feature is enabled.
    #[cfg(feature = "test-utils")]
    pub fn new_for_testing(api_key: String, base_url: String) -> Self {
        let parsed = reqwest::Url::parse(&base_url).expect("test base URL must be valid");
        Self {
            client: Client::builder()
                .timeout(DEFAULT_TIMEOUT)
                .build()
                .expect("failed to build test HTTP client"),
            base_url: parsed,
            api_key,
            device_models: RwLock::new(HashMap::new()),
        }
    }

    /// Look up the model for a device ID from the internal cache.
    fn get_model(&self, id: &DeviceId) -> Result<String> {
        self.device_models
            .read()
            .expect("device_models lock poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| {
                GoveeError::DeviceNotFound(format!(
                    "{} (call list_devices first to populate the device cache)",
                    id
                ))
            })
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
#[derive(serde::Deserialize)]
struct V1StateData {
    #[allow(dead_code)]
    device: String,
    #[allow(dead_code)]
    model: String,
    properties: Vec<serde_json::Value>,
}

/// Build a `DeviceState` from the v1 property array.
///
/// The v1 API returns state as `[{"online": true}, {"powerState": "on"}, ...]`
/// — each element is a JSON object with a single key. We parse each as a
/// `serde_json::Value` map and extract known keys.
fn build_state_from_properties(properties: Vec<serde_json::Value>) -> DeviceState {
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
            brightness = (v as u8).min(100);
        }
        if let Some(obj) = prop.get("color").and_then(|v| v.as_object()) {
            let r = obj.get("r").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
            let g = obj.get("g").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
            let b = obj.get("b").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
            color = Color::new(r, g, b);
        }
        if let Some(v) = prop.get("colorTem").and_then(|v| v.as_u64()) {
            color_temp = Some(v as u32);
        }
    }

    DeviceState {
        on,
        brightness,
        color,
        color_temp_kelvin: color_temp,
        stale: !online,
    }
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

        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after_secs = parse_retry_after(&response);
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

        // Cache device→model mappings for get_state.
        {
            let mut models = self
                .device_models
                .write()
                .expect("device_models lock poisoned");
            models.clear();
            for device in &devices {
                models.insert(device.id.clone(), device.model.clone());
            }
        }

        debug!(count = devices.len(), "listed cloud devices");
        Ok(devices)
    }

    async fn get_state(&self, id: &DeviceId) -> Result<DeviceState> {
        let model = self.get_model(id)?;
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

        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after_secs = parse_retry_after(&response);
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

        let body: V1StateResponse = response.json().await?;
        if body.code != 200 {
            return Err(GoveeError::Api {
                code: body.code,
                message: body.message,
            });
        }

        let state = build_state_from_properties(body.data.properties);
        debug!(device = %id, stale = state.stale, "queried device state");
        Ok(state)
    }

    async fn set_power(&self, _id: &DeviceId, _on: bool) -> Result<()> {
        Err(GoveeError::NotImplemented("CloudBackend::set_power".into()))
    }

    async fn set_brightness(&self, _id: &DeviceId, _value: u8) -> Result<()> {
        Err(GoveeError::NotImplemented(
            "CloudBackend::set_brightness".into(),
        ))
    }

    async fn set_color(&self, _id: &DeviceId, _color: Color) -> Result<()> {
        Err(GoveeError::NotImplemented("CloudBackend::set_color".into()))
    }

    async fn set_color_temp(&self, _id: &DeviceId, _kelvin: u32) -> Result<()> {
        Err(GoveeError::NotImplemented(
            "CloudBackend::set_color_temp".into(),
        ))
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
    fn rejects_http_base_url() {
        let result = CloudBackend::new("key".into(), Some("http://example.com".into()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, GoveeError::InvalidConfig(_)));
        assert!(err.to_string().contains("HTTPS"));
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
        let state = build_state_from_properties(props);
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
        let state = build_state_from_properties(props);
        assert!(state.stale);
        assert!(!state.on);
    }

    #[test]
    fn build_state_clamps_brightness() {
        let props: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"brightness": 200}]"#).unwrap();
        let state = build_state_from_properties(props);
        assert_eq!(state.brightness, 100);
    }

    #[test]
    fn build_state_unknown_properties_ignored() {
        let props: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"unknownProp": 42}]"#).unwrap();
        let state = build_state_from_properties(props);
        assert!(!state.on);
        assert_eq!(state.brightness, 0);
    }
}
