/// Govee domain error type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GoveeError {
    #[error("invalid device ID: {0}")]
    InvalidDeviceId(String),

    #[error("brightness must be 0–100, got {0}")]
    InvalidBrightness(u8),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, GoveeError>;
