/// Govee domain error type.
#[derive(Debug, thiserror::Error)]
pub enum GoveeError {}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, GoveeError>;
