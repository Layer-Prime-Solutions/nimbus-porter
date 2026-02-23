//! PorterRegistry — the single public entry point for all Porter operations.
//!
//! PorterRegistry validates config, spawns all enabled MCP servers (STDIO or HTTP),
//! aggregates their namespaced tools, routes tool calls by slug, and exposes
//! per-server health state.

use std::collections::HashMap;

use rmcp::model::{CallToolResult, Tool};
use tokio_util::sync::CancellationToken;

use crate::config::{PorterConfig, TransportKind};
use crate::error::PorterError;
use crate::namespace::unnamespace_tool_name;
use crate::server::ServerHandle;
use crate::server::health::HealthState;
use crate::server::http::spawn_http_server;
use crate::server::stdio::spawn_stdio_server;

/// The single public entry point for Porter's multi-server MCP gateway.
///
/// Manages the lifecycle of all configured MCP servers (STDIO, HTTP),
/// aggregates their namespaced tool surfaces, and routes tool calls to the
/// correct backend based on the slug embedded in the namespaced tool name.
pub struct PorterRegistry {
    /// Map from server slug to its managed MCP server handle.
    servers: HashMap<String, ServerHandle>,
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
            };

            servers.insert(slug, handle);
        }

        Ok(PorterRegistry { servers, cancel })
    }

    /// Return all tools from all non-Unhealthy servers, aggregated into one list.
    ///
    /// Tools from Starting, Healthy, and Degraded MCP servers are all included —
    /// they may be stale but are still available.
    pub async fn tools(&self) -> Vec<Tool> {
        let mut all_tools = Vec::new();
        for handle in self.servers.values() {
            if handle.health() != HealthState::Unhealthy {
                all_tools.extend(handle.tools().await);
            }
        }
        all_tools
    }

    /// Call a tool by its namespaced name, routing to the correct backend.
    ///
    /// The namespaced name must have the form `slug__tool_name`. The slug is
    /// used to look up the correct server handle. The tool call is forwarded
    /// with the ORIGINAL (un-namespaced) tool name per the backend's expectation.
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
    pub fn server_health(&self, slug: &str) -> Option<HealthState> {
        self.servers.get(slug).map(|h| h.health())
    }

    /// Return a map of all server slugs to their current health states.
    pub fn all_server_health(&self) -> HashMap<String, HealthState> {
        self.servers
            .iter()
            .map(|(slug, handle)| (slug.clone(), handle.health()))
            .collect()
    }

    /// Return a sorted list of all managed server slugs.
    pub fn server_slugs(&self) -> Vec<String> {
        let mut slugs: Vec<String> = self.servers.keys().cloned().collect();
        slugs.sort();
        slugs
    }

    /// Return the total number of managed server handles (enabled at startup).
    pub fn server_count(&self) -> usize {
        self.servers.len()
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
    use crate::config::{PorterConfig, ServerConfig, TransportKind};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Build a PorterConfig programmatically (without TOML parsing).
    fn make_config(servers: Vec<ServerConfig>) -> PorterConfig {
        let mut map = HashMap::new();
        for s in servers {
            map.insert(s.slug.clone(), s);
        }
        PorterConfig {
            servers: map,
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

    /// Create a mock ServerHandle for testing registry routing logic.
    ///
    /// Returns both the handle and the health sender — callers must bind the
    /// sender to `_health_tx` to keep the watch channel alive.
    fn mock_server_handle(
        slug: &str,
        health: HealthState,
    ) -> (ServerHandle, tokio::sync::watch::Sender<HealthState>) {
        let (health_tx, health_rx) = tokio::sync::watch::channel(health);
        let (call_tx, _call_rx) = tokio::sync::mpsc::channel(1);
        let tools = Arc::new(RwLock::new(vec![]));
        let handle = ServerHandle {
            slug: slug.to_string(),
            health_rx,
            tools,
            call_tx,
        };
        (handle, health_tx)
    }

    #[tokio::test]
    async fn test_from_config_validates_duplicate_slugs() {
        // Two servers with the same slug value but different TOML keys should fail validation.
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
            "server-b".to_string(),
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
        let mut servers = HashMap::new();
        let (handle, _health_tx) = mock_server_handle("gh", HealthState::Healthy);
        servers.insert("gh".to_string(), handle);
        let registry = PorterRegistry {
            servers,
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
        let registry = PorterRegistry {
            servers: HashMap::new(),
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
        let (handle, _health_tx) = mock_server_handle("broken", HealthState::Unhealthy);
        servers.insert("broken".to_string(), handle);
        let registry = PorterRegistry {
            servers,
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
            cancel: CancellationToken::new(),
        };
        assert!(registry.server_health("nonexistent").is_none());
    }

    #[test]
    fn test_all_server_health_empty() {
        let registry = PorterRegistry {
            servers: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        assert!(registry.all_server_health().is_empty());
    }

    #[test]
    fn test_server_slugs_sorted() {
        let mut servers = HashMap::new();
        let (h1, _tx1) = mock_server_handle("zebra", HealthState::Healthy);
        servers.insert("zebra".to_string(), h1);
        let (h2, _tx2) = mock_server_handle("alpha", HealthState::Healthy);
        servers.insert("alpha".to_string(), h2);
        let (h3, _tx3) = mock_server_handle("mango", HealthState::Healthy);
        servers.insert("mango".to_string(), h3);
        let registry = PorterRegistry {
            servers,
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
}
