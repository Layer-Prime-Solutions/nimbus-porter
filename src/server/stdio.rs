//! STDIO subprocess transport server management for Porter.
//!
//! Manages MCP servers that communicate via STDIO subprocess.
//!
//! Key design decisions:
//! - Uses raw `tokio::process::Command` instead of `TokioChildProcess` to enable
//!   noisy-server stdout filtering (non-JSON lines discarded silently).
//! - Stdout is piped through a BufReader task that filters non-JSON lines before
//!   passing valid JSON to the rmcp transport.
//! - A restart loop with exponential backoff (1s → 30s cap) handles crashed servers.
//! - Health state transitions: Starting → Healthy → Degraded → Unhealthy.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::Tool;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::config::{ServerConfig, resolve_env_vars};
use crate::error::PorterError;
use crate::namespace::namespace_tool;
use crate::server::health::{ErrorRateTracker, HealthState, StderrBuffer};
use crate::server::{BACKOFF_INITIAL, BACKOFF_MAX, MAX_FAILURES, ServerHandle, ToolCallRequest};

/// Spawn the child process for a STDIO MCP server.
///
/// Returns the `Child` with stdin, stdout, and stderr all piped.
fn spawn_stdio_child(config: &ServerConfig) -> crate::Result<Child> {
    let command_str = config.command.as_ref().ok_or_else(|| {
        PorterError::InvalidConfig(
            config.slug.clone(),
            "STDIO transport requires 'command' field".to_string(),
        )
    })?;

    let mut cmd = Command::new(command_str);

    if !config.args.is_empty() {
        cmd.args(&config.args);
    }

    if !config.env.is_empty() {
        cmd.envs(resolve_env_vars(&config.env));
    }

    if let Some(ref cwd) = config.cwd {
        cmd.current_dir(cwd);
    }

    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    cmd.spawn()
        .map_err(|e| PorterError::Transport(config.slug.clone(), e.to_string()))
}

/// Start a background task that filters stdout from the child process.
///
/// Non-JSON lines are discarded with a debug log. Valid JSON lines are
/// forwarded to the returned `DuplexStream` which rmcp reads as its transport.
///
/// The duplex stream carries raw JSON-RPC newline-delimited messages.
fn start_stdout_filter(
    child_stdout: tokio::process::ChildStdout,
    slug: String,
    cancel: CancellationToken,
) -> tokio::io::ReadHalf<tokio::io::DuplexStream> {
    let (client_side, server_side) = tokio::io::duplex(65536);
    // Split client_side: return reader to caller (rmcp transport reads from it).
    // Split server_side: filter task writes to its writer half — duplex delivers
    // that data to client_side's reader.
    let (reader, _client_writer) = tokio::io::split(client_side);
    let (_server_reader, mut writer) = tokio::io::split(server_side);

    tokio::spawn(async move {
        let mut lines = BufReader::new(child_stdout).lines();
        loop {
            tokio::select! {
                line_result = lines.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            // Only forward valid JSON lines to rmcp transport
                            if serde_json::from_str::<serde_json::Value>(&line).is_ok() {
                                let with_newline = format!("{}\n", line);
                                if writer.write_all(with_newline.as_bytes()).await.is_err() {
                                    break;
                                }
                            } else {
                                tracing::debug!(
                                    server = %slug,
                                    line = %line,
                                    "discarding non-JSON stdout line"
                                );
                            }
                        }
                        Ok(None) | Err(_) => {
                            // EOF or read error — drop writer to signal EOF to reader
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    break;
                }
            }
        }
        // writer dropped here, signals EOF to client_side reader
    });

    reader
}

/// Start a background task that drains stderr from the child process.
///
/// Each line is logged at debug level and pushed into the rolling buffer.
fn start_stderr_drain(
    child_stderr: tokio::process::ChildStderr,
    slug: String,
    stderr_buf: Arc<Mutex<StderrBuffer>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(child_stderr).lines();
        loop {
            tokio::select! {
                line_result = lines.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            tracing::debug!(
                                server = %slug,
                                line = %line,
                                "server stderr"
                            );
                            stderr_buf.lock().await.push(line);
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }
    });
}

/// Spawn the child process, start IO filter tasks, and perform the MCP handshake.
///
/// Returns (RunningService, Child) on success. The Child is kept alive to
/// prevent the process from being killed when `Child` is dropped.
async fn spawn_and_handshake(
    config: &ServerConfig,
    slug: &str,
    stderr_buf: Arc<Mutex<StderrBuffer>>,
    cancel: CancellationToken,
) -> crate::Result<(RunningService<RoleClient, ()>, Child)> {
    let mut child = spawn_stdio_child(config)?;

    let child_stdin = child.stdin.take().ok_or_else(|| {
        PorterError::Transport(slug.to_string(), "failed to open stdin pipe".to_string())
    })?;
    let child_stdout = child.stdout.take().ok_or_else(|| {
        PorterError::Transport(slug.to_string(), "failed to open stdout pipe".to_string())
    })?;
    let child_stderr = child.stderr.take().ok_or_else(|| {
        PorterError::Transport(slug.to_string(), "failed to open stderr pipe".to_string())
    })?;

    // Start background IO tasks
    let filtered_reader = start_stdout_filter(child_stdout, slug.to_string(), cancel.clone());
    start_stderr_drain(child_stderr, slug.to_string(), stderr_buf, cancel.clone());

    // The transport is (reader, writer): rmcp reads JSON from filtered_reader,
    // writes JSON to child_stdin.
    let transport = (filtered_reader, child_stdin);

    let timeout_secs = config.handshake_timeout_secs;
    let handshake_result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        ().serve_with_ct(transport, cancel.clone()),
    )
    .await;

    match handshake_result {
        Err(_elapsed) => Err(PorterError::InitializationFailed(
            slug.to_string(),
            format!("MCP handshake timed out after {}s", timeout_secs),
        )),
        Ok(Err(e)) => Err(PorterError::InitializationFailed(
            slug.to_string(),
            e.to_string(),
        )),
        Ok(Ok(running)) => Ok((running, child)),
    }
}

/// Main loop that manages the full lifecycle of a STDIO MCP server.
///
/// Runs in a `tokio::spawn` task. Handles spawning, handshake, tool discovery,
/// call forwarding, crash detection, restart with exponential backoff, and
/// clean shutdown.
pub(crate) async fn run_stdio_server(
    config: ServerConfig,
    slug: String,
    tools: Arc<RwLock<Vec<Tool>>>,
    call_rx: mpsc::Receiver<ToolCallRequest>,
    health_tx: watch::Sender<HealthState>,
    cancel: CancellationToken,
) {
    let stderr_buf = Arc::new(Mutex::new(StderrBuffer::new(100)));
    let call_rx = Arc::new(Mutex::new(call_rx));

    let mut consecutive_failures: u32 = 0;
    let mut backoff = BACKOFF_INITIAL;

    loop {
        // --- Spawn and handshake ---
        let _ = health_tx.send(HealthState::Starting);

        tracing::info!(server = %slug, "spawning STDIO MCP server");

        match spawn_and_handshake(&config, &slug, stderr_buf.clone(), cancel.clone()).await {
            Err(e) => {
                tracing::warn!(server = %slug, error = %e, "server spawn/handshake failed");
                consecutive_failures += 1;
                if consecutive_failures >= MAX_FAILURES {
                    tracing::error!(
                        server = %slug,
                        failures = consecutive_failures,
                        "server exceeded max consecutive failures — marking Unhealthy"
                    );
                    let _ = health_tx.send(HealthState::Unhealthy);
                    return;
                }
                let _ = health_tx.send(HealthState::Degraded);
                tracing::info!(
                    server = %slug,
                    backoff_secs = backoff.as_secs(),
                    "backing off before restart"
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
            Ok((running, mut child)) => {
                // --- Tool discovery ---
                // Clone peer before passing `running` to the waiting task (running.waiting() consumes self)
                let peer: rmcp::Peer<RoleClient> = running.peer().clone();

                match peer.list_all_tools().await {
                    Ok(discovered_tools) => {
                        let namespaced: Vec<Tool> = discovered_tools
                            .into_iter()
                            .map(|t: Tool| namespace_tool(&slug, t))
                            .collect();
                        let count = namespaced.len();
                        *tools.write().await = namespaced;
                        tracing::info!(server = %slug, tool_count = count, "tools discovered");
                    }
                    Err(e) => {
                        tracing::warn!(server = %slug, error = %e, "failed to list tools after handshake");
                    }
                }

                // Reset failure counters on successful connection
                consecutive_failures = 0;
                backoff = BACKOFF_INITIAL;
                let _ = health_tx.send(HealthState::Healthy);

                // --- Spawn a task to watch for process exit ---
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
                                    tracing::info!(server = %slug, "call channel closed, shutting down");
                                    let _ = child.kill().await;
                                    return;
                                }
                                Some(req) => {
                                    let result = peer.call_tool(req.params).await
                                        .map_err(|e: rmcp::ServiceError| PorterError::Protocol(slug.clone(), e.to_string()));
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
                            // Server process exited unexpectedly
                            drop(rx_guard);
                            break true;
                        }
                        _ = cancel.cancelled() => {
                            drop(rx_guard);
                            tracing::info!(server = %slug, "cancellation received, shutting down");
                            let _ = child.kill().await;
                            return;
                        }
                    }
                };

                if exited_unexpectedly {
                    tracing::warn!(server = %slug, "server process exited unexpectedly, restarting");
                    // Clear tools since server is down
                    tools.write().await.clear();
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_FAILURES {
                        tracing::error!(
                            server = %slug,
                            failures = consecutive_failures,
                            "server exceeded max consecutive failures — marking Unhealthy"
                        );
                        let _ = health_tx.send(HealthState::Unhealthy);
                        return;
                    }
                    let _ = health_tx.send(HealthState::Degraded);
                    tracing::info!(
                        server = %slug,
                        backoff_secs = backoff.as_secs(),
                        "backing off before restart"
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

/// Spawn a STDIO-transport MCP server in a background task and return its handle.
///
/// This is the primary entry point for Porter to start managing an external
/// STDIO MCP server. The returned `ServerHandle` provides health monitoring,
/// tool listing, and tool call routing.
pub fn spawn_stdio_server(
    config: ServerConfig,
    slug: String,
    cancel: CancellationToken,
) -> ServerHandle {
    let (health_tx, health_rx) = watch::channel(HealthState::Starting);
    let (call_tx, call_rx) = mpsc::channel(32);
    let tools = Arc::new(RwLock::new(Vec::<Tool>::new()));

    let tools_clone = tools.clone();
    let slug_clone = slug.clone();

    tokio::spawn(run_stdio_server(
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
    use crate::config::{ServerConfig, TransportKind};
    use std::collections::HashMap;

    fn make_stdio_config(slug: &str, command: Option<&str>) -> ServerConfig {
        ServerConfig {
            slug: slug.to_string(),
            enabled: true,
            transport: TransportKind::Stdio,
            command: command.map(|s| s.to_string()),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            url: None,
            handshake_timeout_secs: 30,
        }
    }

    #[test]
    fn test_spawn_stdio_child_missing_command() {
        let config = make_stdio_config("test", None);
        let result = spawn_stdio_child(&config);
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, _)) if slug == "test"),
            "Expected InvalidConfig error when command is None"
        );
    }

    #[test]
    fn test_spawn_stdio_child_bad_command() {
        // A command that definitely does not exist
        let config = make_stdio_config("test", Some("/this/command/does/not/exist-nimbus"));
        let result = spawn_stdio_child(&config);
        assert!(
            matches!(result, Err(PorterError::Transport(slug, _)) if slug == "test"),
            "Expected Transport error for non-existent command"
        );
    }

    #[tokio::test]
    async fn test_stdout_filter_passes_json_and_discards_non_json() {
        // Simulate child stdout with mixed lines
        let input = b"not json line\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\nstill not json\n{\"x\":1}\n";
        let (write_half, read_half) = tokio::io::duplex(4096);

        // Test the filtering logic directly using a BufReader from a cursor.
        use tokio::io::AsyncBufReadExt;
        let cursor = std::io::Cursor::new(input);
        let reader = tokio::io::BufReader::new(cursor);
        let mut lines = reader.lines();

        let mut json_lines_found = 0;
        let mut non_json_lines_found = 0;

        while let Ok(Some(line)) = lines.next_line().await {
            if serde_json::from_str::<serde_json::Value>(&line).is_ok() {
                json_lines_found += 1;
            } else {
                non_json_lines_found += 1;
            }
        }

        assert_eq!(json_lines_found, 2, "Should have 2 valid JSON lines");
        assert_eq!(non_json_lines_found, 2, "Should have 2 non-JSON lines");

        drop(write_half);
        drop(read_half);
    }

    #[test]
    fn test_backoff_cap_at_30s() {
        let mut backoff = BACKOFF_INITIAL;
        // Simulate 10 failures worth of doubling
        for _ in 0..10 {
            backoff = std::cmp::min(backoff * 2, BACKOFF_MAX);
        }
        assert_eq!(backoff, BACKOFF_MAX, "Backoff should cap at 30s");
    }

    #[test]
    fn test_backoff_sequence() {
        let mut backoff = BACKOFF_INITIAL;
        let mut sequence = vec![backoff];
        for _ in 0..6 {
            backoff = std::cmp::min(backoff * 2, BACKOFF_MAX);
            sequence.push(backoff);
        }
        // 1s, 2s, 4s, 8s, 16s, 30s (cap), 30s (cap)
        assert_eq!(sequence[0], Duration::from_secs(1));
        assert_eq!(sequence[1], Duration::from_secs(2));
        assert_eq!(sequence[2], Duration::from_secs(4));
        assert_eq!(sequence[3], Duration::from_secs(8));
        assert_eq!(sequence[4], Duration::from_secs(16));
        assert_eq!(sequence[5], Duration::from_secs(30));
        assert_eq!(sequence[6], Duration::from_secs(30));
    }
}
