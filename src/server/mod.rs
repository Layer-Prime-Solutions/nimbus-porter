//! Server management modules for Porter.
//!
//! Each submodule handles a specific transport type or concern.
//! mod.rs declares all submodules upfront so Plans 02 and 03 only create
//! new files without needing to modify this file.

pub mod health;
pub mod http;
pub mod stdio;

use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, mpsc, watch};

use crate::config::ToolFilter;
use crate::namespace::unnamespace_tool_name;
use crate::server::health::HealthState;

/// Maximum consecutive failures before marking server Unhealthy.
pub(crate) const MAX_FAILURES: u32 = 5;

/// Initial backoff duration for restart/reconnect loops.
pub(crate) const BACKOFF_INITIAL: Duration = Duration::from_secs(1);

/// Maximum backoff duration cap.
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A request to call a tool on a managed MCP server, with a one-shot channel for the response.
pub(crate) struct ToolCallRequest {
    pub(crate) params: CallToolRequestParams,
    pub(crate) response_tx: tokio::sync::oneshot::Sender<crate::Result<CallToolResult>>,
}

/// External-facing handle for a managed MCP server.
///
/// Provides health monitoring, tool discovery, and tool invocation without
/// exposing the underlying transport or lifecycle management internals.
pub struct ServerHandle {
    pub(crate) slug: String,
    pub(crate) health_rx: watch::Receiver<HealthState>,
    pub(crate) tools: Arc<RwLock<Vec<Tool>>>,
    pub(crate) call_tx: mpsc::Sender<ToolCallRequest>,
    pub(crate) filter: ToolFilter,
}

impl ServerHandle {
    /// Returns the current health state of the managed server.
    pub fn health(&self) -> HealthState {
        *self.health_rx.borrow()
    }

    /// Returns a snapshot of the currently cached tools (namespaced).
    ///
    /// Tools blocked by this server's allow/deny policy are omitted, so callers
    /// never see a tool they are not permitted to invoke.
    pub async fn tools(&self) -> Vec<Tool> {
        self.tools
            .read()
            .await
            .iter()
            .filter(|tool| {
                self.filter
                    .permits(Self::downstream_name(tool.name.as_ref()))
            })
            .cloned()
            .collect()
    }

    /// Return why the given tool is blocked by this server's allow/deny
    /// policy ("deny list" / "not in allow list"), or `None` if permitted.
    ///
    /// Accepts either the namespaced name (`slug__tool`) or the downstream,
    /// un-namespaced name — the policy is always evaluated against the
    /// downstream name.
    pub(crate) fn tool_block_reason(&self, name: &str) -> Option<&'static str> {
        self.filter.block_reason(Self::downstream_name(name))
    }

    /// Strip the namespace prefix if present, yielding the downstream tool name.
    fn downstream_name(name: &str) -> &str {
        unnamespace_tool_name(name).map_or(name, |(_slug, tool)| tool)
    }

    /// Invoke a tool on the managed server.
    ///
    /// Sends the call request through the channel to the server loop and awaits
    /// the one-shot response. Returns an error if the server is unhealthy or
    /// the channel is closed.
    pub async fn call_tool(&self, params: CallToolRequestParams) -> crate::Result<CallToolResult> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let request = ToolCallRequest {
            params,
            response_tx,
        };
        self.call_tx.send(request).await.map_err(|_| {
            crate::PorterError::ServerUnhealthy(
                self.slug.clone(),
                "server channel closed".to_string(),
            )
        })?;
        response_rx.await.map_err(|_| {
            crate::PorterError::Protocol(self.slug.clone(), "response channel dropped".to_string())
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::{RwLock, mpsc, watch};

    #[tokio::test]
    async fn test_server_handle_health() {
        let (health_tx, health_rx) = watch::channel(HealthState::Starting);
        let (call_tx, _call_rx) = mpsc::channel(32);
        let tools = Arc::new(RwLock::new(Vec::<Tool>::new()));

        let handle = ServerHandle {
            slug: "test".to_string(),
            health_rx,
            tools,
            call_tx,
            filter: ToolFilter::default(),
        };

        assert_eq!(handle.health(), HealthState::Starting);

        // Transition to Healthy
        health_tx.send(HealthState::Healthy).unwrap();
        assert_eq!(handle.health(), HealthState::Healthy);
    }

    #[tokio::test]
    async fn test_server_handle_tools_empty() {
        let (_health_tx, health_rx) = watch::channel(HealthState::Starting);
        let (call_tx, _call_rx) = mpsc::channel(32);
        let tools = Arc::new(RwLock::new(Vec::<Tool>::new()));

        let handle = ServerHandle {
            slug: "test".to_string(),
            health_rx,
            tools,
            call_tx,
            filter: ToolFilter::default(),
        };

        let tool_list = handle.tools().await;
        assert!(tool_list.is_empty());
    }

    #[tokio::test]
    async fn test_server_handle_call_tool_unhealthy_when_channel_closed() {
        let (_health_tx, health_rx) = watch::channel(HealthState::Unhealthy);
        let (call_tx, call_rx) = mpsc::channel(1);
        let tools = Arc::new(RwLock::new(Vec::<Tool>::new()));

        let handle = ServerHandle {
            slug: "test-server".to_string(),
            health_rx,
            tools,
            call_tx,
            filter: ToolFilter::default(),
        };

        // Drop receiver to simulate a closed channel
        drop(call_rx);

        let params = CallToolRequestParams {
            name: "test_tool".into(),
            arguments: None,
            task: None,
            meta: None,
        };

        let result = handle.call_tool(params).await;
        assert!(matches!(
            result,
            Err(crate::PorterError::ServerUnhealthy(slug, _)) if slug == "test-server"
        ));
    }

    #[tokio::test]
    async fn test_server_handle_tools_hides_denied() {
        use rmcp::model::Tool;
        use std::sync::Arc as StdArc;

        fn tool(name: &str) -> Tool {
            let schema = StdArc::new(
                serde_json::json!({"type": "object", "properties": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            Tool {
                name: name.to_string().into(),
                title: None,
                description: None,
                input_schema: schema,
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            }
        }

        let (_health_tx, health_rx) = watch::channel(HealthState::Healthy);
        let (call_tx, _call_rx) = mpsc::channel(1);
        // Tools are stored namespaced, as the transport loops record them.
        let tools = Arc::new(RwLock::new(vec![
            tool("gh__get_issue"),
            tool("gh__delete_issue"),
        ]));
        let handle = ServerHandle {
            slug: "gh".to_string(),
            health_rx,
            tools,
            call_tx,
            filter: ToolFilter::new(None, vec!["*delete*".to_string()]),
        };

        let visible = handle.tools().await;
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].name.as_ref(), "gh__get_issue");
        // Permission check accepts both namespaced and un-namespaced names.
        assert!(handle.tool_block_reason("gh__get_issue").is_none());
        assert!(handle.tool_block_reason("get_issue").is_none());
        assert_eq!(
            handle.tool_block_reason("gh__delete_issue"),
            Some("deny list")
        );
        assert_eq!(handle.tool_block_reason("delete_issue"), Some("deny list"));
    }
}
