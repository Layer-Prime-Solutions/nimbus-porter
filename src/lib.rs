//! Porter — standalone MCP server gateway
//! Manages external MCP server connections via STDIO and HTTP transports,
//! namespaces their tools, validates config, and reports per-server health.
//! Zero Nimbus dependencies — publishable independently to crates.io.

pub mod cli;
pub mod config;
pub mod error;
pub mod namespace;
pub mod registry;
pub mod server;
pub mod standalone;

pub use cli::access_guard::{AccessDenied, AccessGuard};
pub use cli::discovery::{DiscoveryConfig, DiscoveryResult};
pub use cli::harness::{CliHandle, CliHarness};
pub use cli::help_parser::{parse_help_output, ArgumentSchema};
pub use cli::read_only_heuristic::is_likely_read_only;
pub use cli::subcommand_parser::parse_subcommands;
pub use config::{
    parse_env_ref, resolve_env_vars, CliServerConfig, ListenConfig, PorterConfig, ServerConfig,
    TransportKind,
};
pub use error::{PorterError, Result};
pub use registry::PorterRegistry;
pub use server::health::HealthState;
pub use server::ServerHandle;
pub use standalone::hot_reload::run_hot_reload;
pub use standalone::server::PorterMcpServer;
