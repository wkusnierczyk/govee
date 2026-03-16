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
        })
    }

    /// Create a `CloudBackend` for testing with an arbitrary base URL.
    ///
    /// Skips HTTPS enforcement — intended for wiremock tests with `http://`
    /// mock servers. Not part of the public API stability contract.
    #[doc(hidden)]
    pub fn new_for_testing(api_key: String, base_url: String) -> Self {
        let parsed = reqwest::Url::parse(&base_url).expect("test base URL must be valid");
        Self {
            client: Client::builder()
                .timeout(DEFAULT_TIMEOUT)
                .build()
                .expect("failed to build test HTTP client"),
            base_url: parsed,
            api_key,
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

        debug!(count = devices.len(), "listed cloud devices");
        Ok(devices)
    }

    async fn get_state(&self, _id: &DeviceId) -> Result<DeviceState> {
        Err(GoveeError::NotImplemented("CloudBackend::get_state".into()))
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
        f.debug_struct("CloudBackend")
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &"[REDACTED]")
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
}
