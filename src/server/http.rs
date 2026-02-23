//! HTTP transport server management for Porter.
//!
//! Manages MCP servers that communicate via Streamable HTTP transport.
//! HTTP servers are simpler than STDIO — no subprocess management, no noisy-server
//! filtering. The server manager connects, performs the MCP handshake, lists tools,
//! and forwards tool calls.
//!
//! A reconnect loop with exponential backoff handles connection failures (1s → 30s cap).

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::Tool;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use tokio::sync::{RwLock, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::config::ServerConfig;
use crate::error::PorterError;
use crate::namespace::namespace_tool;
use crate::server::health::{ErrorRateTracker, HealthState};
use crate::server::{BACKOFF_INITIAL, BACKOFF_MAX, MAX_FAILURES, ServerHandle, ToolCallRequest};

/// Connect to a remote Streamable HTTP MCP server and perform the handshake.
///
/// Constructs the transport from the server URL and performs the MCP handshake
/// with a configurable timeout.
async fn connect_and_handshake(
    config: &ServerConfig,
    slug: &str,
    cancel: CancellationToken,
) -> crate::Result<RunningService<RoleClient, ()>> {
    let url = config.url.as_ref().ok_or_else(|| {
        PorterError::InvalidConfig(
            slug.to_string(),
            "HTTP transport requires 'url' field".to_string(),
        )
    })?;

    let transport = StreamableHttpClientTransport::from_uri(url.as_str());

    let timeout_secs = config.handshake_timeout_secs;
    let handshake_result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        ().serve_with_ct(transport, cancel),
    )
    .await;

    match handshake_result {
        Err(_elapsed) => Err(PorterError::InitializationFailed(
            slug.to_string(),
            format!("HTTP MCP handshake timed out after {}s", timeout_secs),
        )),
        Ok(Err(e)) => Err(PorterError::InitializationFailed(
            slug.to_string(),
            e.to_string(),
        )),
        Ok(Ok(running)) => Ok(running),
    }
}

/// Main loop that manages the full lifecycle of an HTTP MCP server.
///
/// Runs in a `tokio::spawn` task. Handles connection, handshake, tool discovery,
/// call forwarding, reconnect with exponential backoff, and clean shutdown.
pub(crate) async fn run_http_server(
    config: ServerConfig,
    slug: String,
    tools: Arc<RwLock<Vec<Tool>>>,
    call_rx: mpsc::Receiver<ToolCallRequest>,
    health_tx: watch::Sender<HealthState>,
    cancel: CancellationToken,
) {
    let call_rx = Arc::new(tokio::sync::Mutex::new(call_rx));

    let mut consecutive_failures: u32 = 0;
    let mut backoff = BACKOFF_INITIAL;

    loop {
        // --- Connect and handshake ---
        let _ = health_tx.send(HealthState::Starting);

        tracing::info!(server = %slug, "connecting to HTTP MCP server");

        match connect_and_handshake(&config, &slug, cancel.clone()).await {
            Err(e) => {
                tracing::warn!(server = %slug, error = %e, "HTTP server connect/handshake failed");
                consecutive_failures += 1;
                if consecutive_failures >= MAX_FAILURES {
                    tracing::error!(
                        server = %slug,
                        failures = consecutive_failures,
                        "HTTP server exceeded max consecutive failures — marking Unhealthy"
                    );
                    let _ = health_tx.send(HealthState::Unhealthy);
                    return;
                }
                let _ = health_tx.send(HealthState::Degraded);
                tracing::info!(
                    server = %slug,
                    backoff_secs = backoff.as_secs(),
                    "backing off before reconnect"
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => {
                        tracing::info!(server = %slug, "cancelled during backoff sleep");
                        return;
                    }
                }
                backoff = std::cmp::min(backoff * 2, BACKOFF_MAX);
                continue;
            }
            Ok(running) => {
                // --- Tool discovery ---
                let peer = running.peer().clone();

                match peer.list_all_tools().await {
                    Ok(discovered_tools) => {
                        let namespaced: Vec<Tool> = discovered_tools
                            .into_iter()
                            .map(|t| namespace_tool(&slug, t))
                            .collect();
                        let count = namespaced.len();
                        *tools.write().await = namespaced;
                        tracing::info!(server = %slug, tool_count = count, "HTTP tools discovered");
                    }
                    Err(e) => {
                        tracing::warn!(server = %slug, error = %e, "failed to list tools after HTTP handshake");
                    }
                }

                // Reset failure counters on successful connection
                consecutive_failures = 0;
                backoff = BACKOFF_INITIAL;
                let _ = health_tx.send(HealthState::Healthy);

                // --- Spawn a task to watch for session termination ---
                let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel::<()>();
                tokio::spawn(async move {
                    let _ = running.waiting().await;
                    let _ = exit_tx.send(());
                });

                // --- Call-forwarding + exit detection loop ---
                let mut error_tracker = ErrorRateTracker::new(Duration::from_secs(60));
                let exited_unexpectedly = loop {
                    let mut rx_guard = call_rx.lock().await;
                    tokio::select! {
                        maybe_req = rx_guard.recv() => {
                            drop(rx_guard);
                            match maybe_req {
                                None => {
                                    // Channel closed — caller is shutting down
                                    tracing::info!(server = %slug, "call channel closed, shutting down HTTP server");
                                    return;
                                }
                                Some(req) => {
                                    let result = peer.call_tool(req.params).await
                                        .map_err(|e| PorterError::Protocol(slug.clone(), e.to_string()));
                                    match &result {
                                        Ok(_) => error_tracker.record_success(),
                                        Err(_) => error_tracker.record_error(),
                                    }
                                    let new_health = error_tracker.health_state();
                                    let _ = health_tx.send(new_health);
                                    let _ = req.response_tx.send(result);
                                }
                            }
                        }
                        _ = &mut exit_rx => {
                            // Session terminated unexpectedly — reconnect
                            drop(rx_guard);
                            break true;
                        }
                        _ = cancel.cancelled() => {
                            drop(rx_guard);
                            tracing::info!(server = %slug, "cancellation received, shutting down HTTP server");
                            return;
                        }
                    }
                };

                if exited_unexpectedly {
                    tracing::warn!(server = %slug, "HTTP session terminated unexpectedly, reconnecting");
                    // Clear tools since connection is down
                    tools.write().await.clear();
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_FAILURES {
                        tracing::error!(
                            server = %slug,
                            failures = consecutive_failures,
                            "HTTP server exceeded max consecutive failures — marking Unhealthy"
                        );
                        let _ = health_tx.send(HealthState::Unhealthy);
                        return;
                    }
                    let _ = health_tx.send(HealthState::Degraded);
                    tracing::info!(
                        server = %slug,
                        backoff_secs = backoff.as_secs(),
                        "backing off before reconnect"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => {
                            tracing::info!(server = %slug, "cancelled during backoff sleep");
                            return;
                        }
                    }
                    backoff = std::cmp::min(backoff * 2, BACKOFF_MAX);
                }
            }
        }
    }
}

/// Spawn an HTTP-transport MCP server in a background task and return its handle.
///
/// This is the primary entry point for Porter to start managing an external
/// HTTP MCP server. The returned `ServerHandle` provides health monitoring,
/// tool listing, and tool call routing.
pub fn spawn_http_server(
    config: ServerConfig,
    slug: String,
    cancel: CancellationToken,
) -> ServerHandle {
    let (health_tx, health_rx) = watch::channel(HealthState::Starting);
    let (call_tx, call_rx) = mpsc::channel(32);
    let tools = Arc::new(RwLock::new(Vec::<Tool>::new()));

    let tools_clone = tools.clone();
    let slug_clone = slug.clone();

    tokio::spawn(run_http_server(
        config,
        slug_clone,
        tools_clone,
        call_rx,
        health_tx,
        cancel,
    ));

    ServerHandle {
        slug,
        health_rx,
        tools,
        call_tx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_http_transport_construction() {
        // Verify StreamableHttpClientTransport::from_uri accepts a valid URL
        // without panicking. Does not require an actual server.
        // Note: from_uri spawns a worker task internally, requiring a tokio runtime.
        let transport = StreamableHttpClientTransport::from_uri("http://localhost:8080/mcp");
        // Transport was constructed successfully — just verify it doesn't panic
        drop(transport);
    }

    #[tokio::test]
    async fn test_http_transport_construction_https() {
        // Also verify HTTPS URLs are accepted
        let transport = StreamableHttpClientTransport::from_uri("https://api.example.com/mcp");
        drop(transport);
    }

    #[test]
    fn test_backoff_cap_at_30s() {
        let mut backoff = BACKOFF_INITIAL;
        for _ in 0..10 {
            backoff = std::cmp::min(backoff * 2, BACKOFF_MAX);
        }
        assert_eq!(backoff, BACKOFF_MAX, "Backoff should cap at 30s");
    }
}
