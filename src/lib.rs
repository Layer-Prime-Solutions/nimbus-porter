//! Porter — standalone MCP server gateway
//! Manages external MCP server connections via STDIO and HTTP transports,
//! namespaces their tools, validates config, and reports per-server health.
//! Zero Nimbus dependencies — publishable independently to crates.io.

pub mod config;
pub mod error;
pub mod namespace;
pub mod registry;
pub mod server;
pub mod standalone;

pub use config::{
    ListenConfig, PorterConfig, ServerConfig, TransportKind, parse_env_ref, resolve_env_vars,
};
pub use error::{PorterError, Result};
pub use registry::PorterRegistry;
pub use server::ServerHandle;
pub use server::health::HealthState;
pub use standalone::hot_reload::run_hot_reload;
pub use standalone::server::PorterMcpServer;
