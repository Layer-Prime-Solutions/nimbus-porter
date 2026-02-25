//! PorterRegistry — the single public entry point for all Porter operations.
//!
//! PorterRegistry validates config, spawns all enabled servers (STDIO, HTTP, or CLI),
//! aggregates their namespaced tools, routes tool calls by slug, and exposes
//! per-server health state.

use std::collections::HashMap;

use rmcp::model::{CallToolResult, Tool};
use tokio_util::sync::CancellationToken;

use crate::cli::harness::{spawn_cli_server, CliHandle};
use crate::config::{PorterConfig, TransportKind};
use crate::error::PorterError;
use crate::namespace::unnamespace_tool_name;
use crate::server::health::HealthState;
use crate::server::http::spawn_http_server;
use crate::server::stdio::spawn_stdio_server;
use crate::server::ServerHandle;

/// The single public entry point for Porter's multi-server MCP gateway.
///
/// Manages the lifecycle of all configured MCP servers (STDIO, HTTP) and
/// CLI tool handles, aggregates their namespaced tool surfaces, and routes
/// tool calls to the correct backend based on the slug embedded in the
/// namespaced tool name.
pub struct PorterRegistry {
    /// Map from server slug to its managed MCP server handle.
    servers: HashMap<String, ServerHandle>,
    /// Map from CLI tool slug to its managed CLI handle.
    cli_handles: HashMap<String, CliHandle>,
    /// Root cancellation token — cancelling this shuts down all server tasks.
    cancel: CancellationToken,
}

impl PorterRegistry {
    /// Build a registry from validated config, spawning all enabled servers.
    ///
    /// Calls `config.validate()` first — returns an error without spawning
    /// anything if config is invalid. Disabled servers are silently skipped.
    pub async fn from_config(config: PorterConfig) -> crate::Result<Self> {
        config.validate()?;

        let cancel = CancellationToken::new();
        let mut servers: HashMap<String, ServerHandle> = HashMap::new();
        let mut cli_handles: HashMap<String, CliHandle> = HashMap::new();

        // Spawn MCP servers (STDIO / HTTP)
        for (_key, server_config) in config.servers {
            if !server_config.enabled {
                tracing::debug!(
                    server = %server_config.slug,
                    "skipping disabled server"
                );
                continue;
            }

            let slug = server_config.slug.clone();
            let child_token = cancel.child_token();

            let handle = match server_config.transport {
                TransportKind::Stdio => {
                    spawn_stdio_server(server_config, slug.clone(), child_token)
                }
                TransportKind::Http => spawn_http_server(server_config, slug.clone(), child_token),
                TransportKind::Cli => {
                    // CLI transport in servers map is rejected by validate() — unreachable in practice
                    return Err(PorterError::InvalidConfig(
                        slug.clone(),
                        "CLI transport must be configured under [cli.*], not [servers.*]"
                            .to_string(),
                    ));
                }
            };

            servers.insert(slug, handle);
        }

        // Spawn CLI handles
        for (_key, cli_config) in config.cli {
            if !cli_config.enabled {
                tracing::debug!(
                    server = %cli_config.slug,
                    "skipping disabled CLI tool"
                );
                continue;
            }

            let slug = cli_config.slug.clone();

            let handle = spawn_cli_server(cli_config, slug.clone()).await?;
            cli_handles.insert(slug, handle);
        }

        Ok(PorterRegistry {
            servers,
            cli_handles,
            cancel,
        })
    }

    /// Return all tools from all non-Unhealthy servers and CLI handles, aggregated into one list.
    ///
    /// Tools from Starting, Healthy, and Degraded MCP servers are all included —
    /// they may be stale but are still available. CLI handles are always Healthy.
    pub async fn tools(&self) -> Vec<Tool> {
        let mut all_tools = Vec::new();
        for handle in self.servers.values() {
            if handle.health() != HealthState::Unhealthy {
                all_tools.extend(handle.tools().await);
            }
        }
        for cli_handle in self.cli_handles.values() {
            all_tools.extend(cli_handle.tools().await);
        }
        all_tools
    }

    /// Call a tool by its namespaced name, routing to the correct backend.
    ///
    /// The namespaced name must have the form `slug__tool_name`. The slug is
    /// used to look up the correct handle. CLI handles are checked first, then
    /// MCP servers. The tool call is forwarded with the ORIGINAL (un-namespaced)
    /// tool name per the backend's expectation.
    pub async fn call_tool(
        &self,
        namespaced_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> crate::Result<CallToolResult> {
        // Parse slug from namespaced name
        let (slug, original_name) = unnamespace_tool_name(namespaced_name).ok_or_else(|| {
            PorterError::Protocol(
                "unknown".into(),
                format!("tool name '{}' has no namespace prefix", namespaced_name),
            )
        })?;

        // Check CLI handles first
        if let Some(cli_handle) = self.cli_handles.get(slug) {
            let params = rmcp::model::CallToolRequestParams {
                name: original_name.to_string().into(),
                arguments,
                task: None,
                meta: None,
            };
            return cli_handle.call_tool(params).await;
        }

        // Look up MCP server by slug
        let handle = self.servers.get(slug).ok_or_else(|| {
            PorterError::Protocol(slug.to_string(), format!("no server with slug '{}'", slug))
        })?;

        // Refuse calls to Unhealthy servers
        if handle.health() == HealthState::Unhealthy {
            return Err(PorterError::ServerUnhealthy(
                slug.to_string(),
                "server is unhealthy".to_string(),
            ));
        }

        // Build call params with the original (un-namespaced) tool name
        let params = rmcp::model::CallToolRequestParams {
            name: original_name.to_string().into(),
            arguments,
            task: None,
            meta: None,
        };

        handle.call_tool(params).await
    }

    /// Return the health state for a specific server slug, or None if not found.
    ///
    /// Checks both MCP servers and CLI handles. CLI handles are always Healthy.
    pub fn server_health(&self, slug: &str) -> Option<HealthState> {
        if let Some(cli_handle) = self.cli_handles.get(slug) {
            return Some(cli_handle.health());
        }
        self.servers.get(slug).map(|h| h.health())
    }

    /// Return a map of all server slugs (MCP + CLI) to their current health states.
    pub fn all_server_health(&self) -> HashMap<String, HealthState> {
        let mut health_map: HashMap<String, HealthState> = self
            .servers
            .iter()
            .map(|(slug, handle)| (slug.clone(), handle.health()))
            .collect();
        for (slug, cli_handle) in &self.cli_handles {
            health_map.insert(slug.clone(), cli_handle.health());
        }
        health_map
    }

    /// Return a sorted list of all managed server slugs (MCP + CLI).
    pub fn server_slugs(&self) -> Vec<String> {
        let mut slugs: Vec<String> = self
            .servers
            .keys()
            .chain(self.cli_handles.keys())
            .cloned()
            .collect();
        slugs.sort();
        slugs
    }

    /// Return the total number of managed handles (MCP servers + CLI handles, enabled at startup).
    pub fn server_count(&self) -> usize {
        self.servers.len() + self.cli_handles.len()
    }

    /// Cancel all server tasks, initiating a clean shutdown.
    ///
    /// Server tasks observe the cancellation token and exit. Shutdown is
    /// asynchronous — use this in conjunction with runtime shutdown for
    /// full cleanup.
    pub async fn shutdown(&self) {
        tracing::info!("PorterRegistry shutting down all servers");
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::access_guard::AccessGuard;
    use crate::cli::harness::CliHandle;
    use crate::config::{CliServerConfig, PorterConfig, ServerConfig, TransportKind};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::RwLock;

    /// Build a PorterConfig programmatically (without TOML parsing).
    fn make_config(servers: Vec<ServerConfig>) -> PorterConfig {
        let mut map = HashMap::new();
        for s in servers {
            map.insert(s.slug.clone(), s);
        }
        PorterConfig {
            servers: map,
            cli: HashMap::new(),
            ..Default::default()
        }
    }

    fn stdio_config(slug: &str, enabled: bool) -> ServerConfig {
        ServerConfig {
            slug: slug.to_string(),
            enabled,
            transport: TransportKind::Stdio,
            command: Some("echo".to_string()),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            url: None,
            handshake_timeout_secs: 30,
        }
    }

    fn make_cli_config(slug: &str, command: &str, enabled: bool) -> CliServerConfig {
        CliServerConfig {
            slug: slug.to_string(),
            enabled,
            transport: TransportKind::Cli,
            command: command.to_string(),
            profile: None,
            args: vec![],
            env: HashMap::new(),
            allow: vec![],
            deny: vec![],
            write_access: HashMap::new(),
            timeout_secs: 5,
            inject_flags: vec![],
            expand_subcommands: None,
            schema_override: Some(serde_json::json!({"type": "object", "properties": {}})),
            help_depth: None,
            discovery_budget_secs: 60,
        }
    }

    /// Create a mock ServerHandle for testing registry routing logic.
    fn mock_server_handle(slug: &str, health: HealthState) -> ServerHandle {
        let (health_tx, health_rx) = tokio::sync::watch::channel(health);
        let (call_tx, _call_rx) = tokio::sync::mpsc::channel(1);
        let tools = Arc::new(RwLock::new(vec![]));
        // Keep health_tx alive so health state doesn't get reset
        std::mem::forget(health_tx);
        ServerHandle {
            slug: slug.to_string(),
            health_rx,
            tools,
            call_tx,
        }
    }

    /// Create a mock CliHandle for testing registry routing logic.
    fn mock_cli_handle(slug: &str) -> CliHandle {
        let tools = Arc::new(RwLock::new(vec![]));
        let guard = Arc::new(AccessGuard::new(&make_cli_config(slug, "echo", true)));
        CliHandle {
            slug: slug.to_string(),
            tools,
            guard,
            command: "echo".to_string(),
            inject_flags: vec![],
            env: HashMap::new(),
            timeout: Duration::from_secs(5),
            expanded: false,
            discovery_in_progress: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn test_from_config_validates_duplicate_slugs() {
        // Two servers with the same slug value but different TOML keys should fail validation.
        // Note: PorterConfig.servers is a HashMap<String, ServerConfig> where the key is the
        // TOML section name (e.g., "servers.github"), but the slug field inside ServerConfig
        // is what validate() checks for uniqueness.
        let mut map = HashMap::new();
        map.insert(
            "server-a".to_string(),
            ServerConfig {
                slug: "same".to_string(),
                enabled: true,
                transport: TransportKind::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
                url: None,
                handshake_timeout_secs: 30,
            },
        );
        map.insert(
            "server-b".to_string(), // different TOML key, same slug value
            ServerConfig {
                slug: "same".to_string(),
                enabled: true,
                transport: TransportKind::Http,
                command: None,
                args: vec![],
                env: HashMap::new(),
                cwd: None,
                url: Some("http://example.com/mcp".to_string()),
                handshake_timeout_secs: 30,
            },
        );
        let config = PorterConfig {
            servers: map,
            cli: HashMap::new(),
            ..Default::default()
        };
        let result = PorterRegistry::from_config(config).await;
        assert!(
            matches!(result, Err(PorterError::DuplicateSlug(s)) if s == "same"),
            "Expected DuplicateSlug error for duplicate slug 'same'"
        );
    }

    #[tokio::test]
    async fn test_from_config_skips_disabled_servers() {
        let config = make_config(vec![
            stdio_config("enabled-server", true),
            stdio_config("disabled-server", false),
        ]);
        let registry = PorterRegistry::from_config(config).await.unwrap();
        let slugs = registry.server_slugs();
        assert_eq!(slugs, vec!["enabled-server".to_string()]);
        assert_eq!(registry.server_count(), 1);
    }

    #[tokio::test]
    async fn test_call_tool_no_namespace() {
        // Build a registry with one mock server, test routing error for non-namespaced name
        let mut servers = HashMap::new();
        servers.insert(
            "gh".to_string(),
            mock_server_handle("gh", HealthState::Healthy),
        );
        let registry = PorterRegistry {
            servers,
            cli_handles: HashMap::new(),
            cancel: CancellationToken::new(),
        };

        let result = registry.call_tool("list_repos", None).await;
        assert!(
            matches!(result, Err(PorterError::Protocol(slug, msg)) if slug == "unknown" && msg.contains("no namespace prefix")),
            "Expected Protocol error for missing namespace"
        );
    }

    #[tokio::test]
    async fn test_call_tool_unknown_slug() {
        // Build a registry with no servers, test routing error for unknown slug
        let registry = PorterRegistry {
            servers: HashMap::new(),
            cli_handles: HashMap::new(),
            cancel: CancellationToken::new(),
        };

        let result = registry.call_tool("gh__list_repos", None).await;
        assert!(
            matches!(result, Err(PorterError::Protocol(slug, msg)) if slug == "gh" && msg.contains("no server with slug")),
            "Expected Protocol error for unknown slug"
        );
    }

    #[tokio::test]
    async fn test_call_tool_unhealthy_server_rejected() {
        let mut servers = HashMap::new();
        servers.insert(
            "broken".to_string(),
            mock_server_handle("broken", HealthState::Unhealthy),
        );
        let registry = PorterRegistry {
            servers,
            cli_handles: HashMap::new(),
            cancel: CancellationToken::new(),
        };

        let result = registry.call_tool("broken__some_tool", None).await;
        assert!(
            matches!(result, Err(PorterError::ServerUnhealthy(slug, _)) if slug == "broken"),
            "Expected ServerUnhealthy error"
        );
    }

    #[test]
    fn test_server_health_returns_none_for_unknown() {
        let registry = PorterRegistry {
            servers: HashMap::new(),
            cli_handles: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        assert!(registry.server_health("nonexistent").is_none());
    }

    #[test]
    fn test_all_server_health_empty() {
        let registry = PorterRegistry {
            servers: HashMap::new(),
            cli_handles: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        assert!(registry.all_server_health().is_empty());
    }

    #[test]
    fn test_server_slugs_sorted() {
        let mut servers = HashMap::new();
        servers.insert(
            "zebra".to_string(),
            mock_server_handle("zebra", HealthState::Healthy),
        );
        servers.insert(
            "alpha".to_string(),
            mock_server_handle("alpha", HealthState::Healthy),
        );
        servers.insert(
            "mango".to_string(),
            mock_server_handle("mango", HealthState::Healthy),
        );
        let registry = PorterRegistry {
            servers,
            cli_handles: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        assert_eq!(
            registry.server_slugs(),
            vec![
                "alpha".to_string(),
                "mango".to_string(),
                "zebra".to_string()
            ]
        );
    }

    #[test]
    fn test_cli_handle_health_always_healthy() {
        // CLI handles are always Healthy — no persistent connection to lose
        let mut cli_handles = HashMap::new();
        cli_handles.insert("mycli".to_string(), mock_cli_handle("mycli"));
        let registry = PorterRegistry {
            servers: HashMap::new(),
            cli_handles,
            cancel: CancellationToken::new(),
        };

        assert_eq!(registry.server_health("mycli"), Some(HealthState::Healthy));
        assert_eq!(registry.server_count(), 1);
    }

    #[test]
    fn test_cli_handle_included_in_server_slugs() {
        let mut servers = HashMap::new();
        servers.insert(
            "mcp-server".to_string(),
            mock_server_handle("mcp-server", HealthState::Healthy),
        );
        let mut cli_handles = HashMap::new();
        cli_handles.insert("aws-cli".to_string(), mock_cli_handle("aws-cli"));

        let registry = PorterRegistry {
            servers,
            cli_handles,
            cancel: CancellationToken::new(),
        };

        let slugs = registry.server_slugs();
        assert!(slugs.contains(&"mcp-server".to_string()));
        assert!(slugs.contains(&"aws-cli".to_string()));
        assert_eq!(registry.server_count(), 2);
    }

    #[test]
    fn test_all_server_health_includes_cli_handles() {
        let mut servers = HashMap::new();
        servers.insert(
            "mcp".to_string(),
            mock_server_handle("mcp", HealthState::Healthy),
        );
        let mut cli_handles = HashMap::new();
        cli_handles.insert("cli".to_string(), mock_cli_handle("cli"));

        let registry = PorterRegistry {
            servers,
            cli_handles,
            cancel: CancellationToken::new(),
        };

        let health_map = registry.all_server_health();
        assert!(health_map.contains_key("mcp"));
        assert!(health_map.contains_key("cli"));
        assert_eq!(health_map["cli"], HealthState::Healthy);
    }

    #[tokio::test]
    async fn test_from_config_spawns_cli_handles() {
        // CLI tool configured with schema_override should be spawned and included
        let mut cli_map = HashMap::new();
        cli_map.insert(
            "echo-tool".to_string(),
            make_cli_config("echo-tool", "echo", true),
        );
        let config = PorterConfig {
            servers: HashMap::new(),
            cli: cli_map,
            ..Default::default()
        };

        let registry = PorterRegistry::from_config(config).await.unwrap();
        assert_eq!(registry.server_count(), 1);
        assert!(registry.server_slugs().contains(&"echo-tool".to_string()));
        assert_eq!(
            registry.server_health("echo-tool"),
            Some(HealthState::Healthy)
        );
    }

    #[tokio::test]
    async fn test_call_tool_routes_to_cli_handle() {
        // CLI tool call routing through the registry
        let cli_handle = mock_cli_handle("echo-tool");
        let mut cli_handles = HashMap::new();
        cli_handles.insert("echo-tool".to_string(), cli_handle);

        let registry = PorterRegistry {
            servers: HashMap::new(),
            cli_handles,
            cancel: CancellationToken::new(),
        };

        // Call the tool — echo will be called with args from the namespaced tool
        // The original tool name after unnamespacing is "echo"
        let mut arguments = serde_json::Map::new();
        arguments.insert("args".to_string(), serde_json::json!(["hello"]));

        let result = registry.call_tool("echo-tool__echo", Some(arguments)).await;
        // echo should succeed
        assert!(result.is_ok(), "CLI tool call should succeed: {:?}", result);
    }
}
