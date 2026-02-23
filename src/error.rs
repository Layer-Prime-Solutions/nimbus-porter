//! Error types for Porter MCP gateway operations.

use thiserror::Error;

/// Main error type for Porter operations
#[derive(Error, Debug)]
pub enum PorterError {
    /// Duplicate server slug found in config
    #[error("duplicate server slug: {0}")]
    DuplicateSlug(String),

    /// Invalid configuration for a named server
    #[error("invalid config for server '{0}': {1}")]
    InvalidConfig(String, String),

    /// Initialization failed for a named server
    #[error("initialization failed for server '{0}': {1}")]
    InitializationFailed(String, String),

    /// Server is unhealthy
    #[error("server '{0}' is unhealthy: {1}")]
    ServerUnhealthy(String, String),

    /// MCP protocol error for a named server
    #[error("protocol error for server '{0}': {1}")]
    Protocol(String, String),

    /// Transport-level error for a named server
    #[error("transport error for server '{0}': {1}")]
    Transport(String, String),

    /// Server is shutting down
    #[error("server '{0}' shutting down")]
    ShuttingDown(String),
}

/// Result type alias for Porter operations
pub type Result<T> = std::result::Result<T, PorterError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_duplicate_slug_display() {
        let err = PorterError::DuplicateSlug("gh".to_string());
        assert_eq!(err.to_string(), "duplicate server slug: gh");
    }

    #[test]
    fn test_invalid_config_display() {
        let err = PorterError::InvalidConfig(
            "gh".to_string(),
            "STDIO transport requires 'command' field".to_string(),
        );
        assert_eq!(
            err.to_string(),
            "invalid config for server 'gh': STDIO transport requires 'command' field"
        );
    }
}
