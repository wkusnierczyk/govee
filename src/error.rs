/// Govee domain error type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GoveeError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("UDP error: {0}")]
    Udp(#[from] std::io::Error),

    #[error("API error {code}: {message}")]
    Api { code: u16, message: String },

    #[error("rate limited — retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),

    #[error("discovery timeout")]
    DiscoveryTimeout,

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("not implemented: {0}")]
    NotImplemented(String),

    #[error("invalid device ID: {0}")]
    InvalidDeviceId(String),

    #[error("brightness must be 0–100, got {0}")]
    InvalidBrightness(u8),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, GoveeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_api() {
        let err = GoveeError::Api {
            code: 429,
            message: "too many requests".into(),
        };
        assert_eq!(err.to_string(), "API error 429: too many requests");
    }

    #[test]
    fn error_display_rate_limited() {
        let err = GoveeError::RateLimited {
            retry_after_secs: 60,
        };
        assert_eq!(err.to_string(), "rate limited — retry after 60s");
    }

    #[test]
    fn error_display_device_not_found() {
        let err = GoveeError::DeviceNotFound("bedroom".into());
        assert_eq!(err.to_string(), "device not found: bedroom");
    }

    #[test]
    fn error_display_discovery_timeout() {
        assert_eq!(
            GoveeError::DiscoveryTimeout.to_string(),
            "discovery timeout"
        );
    }

    #[test]
    fn error_display_not_implemented() {
        let err = GoveeError::NotImplemented("workflow engine".into());
        assert_eq!(err.to_string(), "not implemented: workflow engine");
    }

    #[test]
    fn error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::AddrInUse, "port 4002 in use");
        let err: GoveeError = io_err.into();
        assert!(matches!(err, GoveeError::Udp(_)));
        assert!(err.to_string().contains("port 4002 in use"));
    }

    #[test]
    fn error_from_serde_json() {
        let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err: GoveeError = json_err.into();
        assert!(matches!(err, GoveeError::Serialization(_)));
    }
}
