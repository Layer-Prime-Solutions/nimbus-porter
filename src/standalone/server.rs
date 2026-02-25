//! PorterMcpServer — rmcp ServerHandler backed by PorterRegistry.
//!
//! Implements the MCP ServerHandler trait, delegating tool listing and tool
//! calls to the underlying PorterRegistry. The registry is stored behind an
//! Arc<RwLock<Arc<PorterRegistry>>> to support hot-reload: the hot-reload task
//! swaps the inner Arc<PorterRegistry> while all sessions share the outer
//! Arc<RwLock<...>>, ensuring they see the updated registry on next access.
//!
//! Connected MCP client peers are stored in a shared Vec so the hot-reload task
//! can broadcast tools-list-changed notifications after each reload.

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::{NotificationContext, Peer, RequestContext, RoleServer};
use rmcp::ErrorData as McpError;
use tokio::sync::RwLock;

use crate::PorterRegistry;

/// MCP server backed by a PorterRegistry.
///
/// The double-Arc pattern (`Arc<RwLock<Arc<PorterRegistry>>>`) ensures:
/// - All sessions share the same outer `Arc<RwLock<...>>` pointer.
/// - Hot-reload swaps the inner `Arc<PorterRegistry>` via a write lock.
/// - Existing sessions see the new registry on their next tool access.
///
/// `StreamableHttpService` calls the factory closure per session — each new
/// `PorterMcpServer` clone shares the same outer `Arc`s, so hot-reload
/// propagates to all sessions automatically.
#[derive(Clone)]
pub struct PorterMcpServer {
    /// Double-arc registry handle: outer Arc<RwLock<...>> shared by all clones;
    /// inner Arc<PorterRegistry> swapped by hot-reload on config changes.
    registry: Arc<RwLock<Arc<PorterRegistry>>>,
    /// Connected session peers for broadcasting tools-list-changed notifications.
    /// Stale peers are pruned on notification error.
    peers: Arc<tokio::sync::Mutex<Vec<Peer<RoleServer>>>>,
}

impl PorterMcpServer {
    /// Create a new PorterMcpServer wrapping a PorterRegistry.
    pub fn new(registry: PorterRegistry) -> Self {
        Self {
            registry: Arc::new(RwLock::new(Arc::new(registry))),
            peers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Return the registry handle for the hot-reload task to swap the inner registry.
    pub fn registry_handle(&self) -> Arc<RwLock<Arc<PorterRegistry>>> {
        self.registry.clone()
    }

    /// Return the peers handle for the hot-reload task to broadcast notifications.
    pub fn peers_handle(&self) -> Arc<tokio::sync::Mutex<Vec<Peer<RoleServer>>>> {
        self.peers.clone()
    }
}

impl ServerHandler for PorterMcpServer {
    /// Return server metadata: name "porter", tool capabilities enabled.
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            server_info: rmcp::model::Implementation {
                name: "porter".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "Porter CLI & MCP Gateway — wraps CLI tools and MCP servers as callable tools."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    /// List all tools from the registry (across all MCP servers and CLI handles).
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let registry = self.registry.read().await;
        let tools = registry.tools().await;
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    /// Call a tool by namespaced name, routing through the registry.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let registry = self.registry.read().await;
        registry
            .call_tool(&request.name, request.arguments)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    /// Store the connected peer for later tools-list-changed notifications.
    ///
    /// Called by rmcp after the client sends `InitializedNotification`. The peer
    /// is added to the shared peers vec so hot-reload can broadcast to all clients.
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        tracing::info!("MCP client initialized, storing peer for hot-reload notifications");
        self.peers.lock().await.push(context.peer.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PorterConfig;
    use std::collections::HashMap;

    /// Create a PorterMcpServer with an empty registry (no servers or CLI handles).
    async fn make_empty_server() -> PorterMcpServer {
        let config = PorterConfig {
            servers: HashMap::new(),
            cli: HashMap::new(),
            ..Default::default()
        };
        let registry = PorterRegistry::from_config(config)
            .await
            .expect("empty config should succeed");
        PorterMcpServer::new(registry)
    }

    #[tokio::test]
    async fn test_get_info_server_name() {
        let server = make_empty_server().await;
        let info = server.get_info();
        assert_eq!(info.server_info.name, "porter");
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability should be enabled"
        );
        assert!(info.instructions.is_some(), "instructions should be set");
    }

    #[tokio::test]
    async fn test_empty_registry_returns_empty_tool_list() {
        let server = make_empty_server().await;
        let registry = server.registry.read().await;
        let tools = registry.tools().await;
        assert!(tools.is_empty(), "empty registry should have no tools");
    }

    #[tokio::test]
    async fn test_registry_handle_is_shared() {
        // Both the server clone and the handle share the same outer Arc
        let server = make_empty_server().await;
        let handle = server.registry_handle();
        let server_clone = server.clone();

        // Both point to the same allocation
        assert!(Arc::ptr_eq(&server.registry, &handle));
        assert!(Arc::ptr_eq(&server.registry, &server_clone.registry));
    }

    #[tokio::test]
    async fn test_peers_handle_is_shared() {
        let server = make_empty_server().await;
        let peers_handle = server.peers_handle();
        let server_clone = server.clone();

        assert!(Arc::ptr_eq(&server.peers, &peers_handle));
        assert!(Arc::ptr_eq(&server.peers, &server_clone.peers));
    }
}
