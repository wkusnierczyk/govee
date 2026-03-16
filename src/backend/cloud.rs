use async_trait::async_trait;
use reqwest::Client;

use super::GoveeBackend;
use crate::error::{GoveeError, Result};
use crate::types::{BackendType, Color, Device, DeviceId, DeviceState};

/// Default base URL for the Govee cloud API.
const DEFAULT_BASE_URL: &str = "https://developer-api.govee.com";

/// Cloud API backend using the Govee v1 REST API.
///
/// Authenticates via `Govee-API-Key` header. Base URL defaults to
/// `https://developer-api.govee.com` but can be overridden for testing.
pub struct CloudBackend {
    client: Client,
    base_url: String,
    api_key: String,
}

impl CloudBackend {
    /// Create a new `CloudBackend`.
    ///
    /// Returns `GoveeError::InvalidConfig` if `base_url` does not use HTTPS.
    pub fn new(api_key: String, base_url: Option<String>) -> Result<Self> {
        let base_url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        if !base_url.starts_with("https://") {
            return Err(GoveeError::InvalidConfig(format!(
                "base URL must use HTTPS, got: {}",
                base_url
            )));
        }
        Ok(Self {
            client: Client::new(),
            base_url,
            api_key,
        })
    }

    /// Create a `CloudBackend` for testing with an arbitrary base URL.
    ///
    /// Skips HTTPS enforcement — intended for wiremock tests with `http://`
    /// mock servers. Not part of the public API stability contract.
    #[doc(hidden)]
    pub fn new_for_testing(api_key: String, base_url: String) -> Self {
        Self {
            client: Client::new(),
            base_url,
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
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct V1Device {
    device: String,
    model: String,
    device_name: String,
    #[allow(dead_code)]
    controllable: bool,
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

#[async_trait]
impl GoveeBackend for CloudBackend {
    async fn list_devices(&self) -> Result<Vec<Device>> {
        let url = format!("{}/v1/devices", self.base_url);
        let response = self
            .client
            .get(&url)
            .header("Govee-API-Key", &self.api_key)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
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

        body.data
            .devices
            .into_iter()
            .map(V1Device::into_domain)
            .collect()
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
            .field("base_url", &self.base_url)
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
    fn accepts_https_base_url() {
        let result = CloudBackend::new("key".into(), Some("https://example.com".into()));
        assert!(result.is_ok());
    }

    #[test]
    fn default_base_url_is_https() {
        let backend = CloudBackend::new("key".into(), None).unwrap();
        assert!(backend.base_url.starts_with("https://"));
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
            controllable: true,
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
            controllable: true,
        };
        assert!(v1.into_domain().is_err());
    }
}
