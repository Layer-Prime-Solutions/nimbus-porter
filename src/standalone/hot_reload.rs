//! Hot-reload for `porter serve`.
//!
//! Watches the porter.toml config file using the `notify` crate. On each
//! detected change (with 100ms debounce), it re-parses the config and rebuilds
//! the PorterRegistry. On success, the inner Arc<PorterRegistry> is swapped
//! inside the outer Arc<RwLock<...>>, and all connected MCP client peers
//! receive a tools-list-changed notification.
//!
//! Stale peers (whose transport has closed) are pruned on notification error.
//! On reload failure, the previous registry is preserved and a warning is logged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::RecursiveMode;
use notify::Watcher;
use rmcp::service::{Peer, RoleServer};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::PorterRegistry;
use crate::config::PorterConfig;

/// Watch `config_path` for changes and reload the registry on each change.
///
/// # Arguments
/// * `config_path` - Path to the porter.toml config file to watch
/// * `registry_handle` - Shared registry handle; inner Arc is swapped on reload
/// * `peers_handle` - Shared peers vec; tools-list-changed is sent to each peer
/// * `cancel` - CancellationToken; function returns when cancelled
pub async fn run_hot_reload(
    config_path: PathBuf,
    registry_handle: Arc<RwLock<Arc<PorterRegistry>>>,
    peers_handle: Arc<tokio::sync::Mutex<Vec<Peer<RoleServer>>>>,
    cancel: CancellationToken,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();

    // Wrap the tokio sender in a closure — notify v8's EventHandler is implemented
    // for FnMut, so we pass a closure that sends events to the tokio mpsc channel.
    // std::sync::mpsc::Sender also implements EventHandler directly, but tokio's
    // UnboundedSender does not.
    let mut watcher = match notify::recommended_watcher(move |event| {
        // Ignore send errors — if the channel is closed, hot-reload is shutting down
        let _ = tx.send(event);
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = %e, "failed to create file watcher for hot-reload");
            return;
        }
    };

    if let Err(e) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
        tracing::error!(
            path = %config_path.display(),
            error = %e,
            "failed to watch config file for hot-reload"
        );
        return;
    }

    // IMPORTANT: Keep watcher alive for the duration of this task (Pitfall 2 from research).
    // If _watcher is dropped, the OS-level watch stops and events stop arriving silently.
    let _watcher = watcher;

    tracing::info!(
        path = %config_path.display(),
        "hot-reload watching config file"
    );

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(Ok(_)) => {
                        // Debounce: wait 100ms for burst of events to settle
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        // Drain any remaining events that arrived during the sleep
                        while rx.try_recv().is_ok() {}

                        match reload_registry(&config_path).await {
                            Ok(new_registry) => {
                                let tool_count = new_registry.server_count();
                                // Swap the inner registry under write lock, then shut
                                // down the old one to cancel its server tasks.
                                let old_registry = {
                                    let mut guard = registry_handle.write().await;
                                    std::mem::replace(&mut *guard, Arc::new(new_registry))
                                };
                                old_registry.shutdown().await;
                                tracing::info!(
                                    tools = %tool_count,
                                    path = %config_path.display(),
                                    "config reloaded"
                                );
                                // Notify all connected peers; prune stale ones on error
                                notify_peers(&peers_handle).await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    path = %config_path.display(),
                                    "hot-reload failed, keeping previous config"
                                );
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "file watcher error during hot-reload");
                    }
                    None => {
                        tracing::debug!("hot-reload watcher channel closed");
                        return;
                    }
                }
            }
            _ = cancel.cancelled() => {
                tracing::debug!("hot-reload cancelled");
                return;
            }
        }
    }
}

/// Notify all connected peers of a tools-list-changed event.
///
/// Peers that fail with a transport error are pruned from the vec.
async fn notify_peers(peers_handle: &Arc<tokio::sync::Mutex<Vec<Peer<RoleServer>>>>) {
    let mut peers = peers_handle.lock().await;
    let mut live_peers = Vec::with_capacity(peers.len());
    for peer in peers.drain(..) {
        match peer.notify_tool_list_changed().await {
            Ok(_) => {
                live_peers.push(peer);
            }
            Err(e) => {
                tracing::debug!(error = %e, "pruning stale peer after tools-list-changed error");
            }
        }
    }
    *peers = live_peers;
}

/// Load and parse the porter.toml config file, then build a new PorterRegistry.
async fn reload_registry(config_path: &Path) -> crate::Result<PorterRegistry> {
    let content = tokio::fs::read_to_string(config_path)
        .await
        .map_err(|e| crate::PorterError::InvalidConfig("hot-reload".into(), e.to_string()))?;
    let config: PorterConfig = toml::from_str(&content)
        .map_err(|e| crate::PorterError::InvalidConfig("hot-reload".into(), e.to_string()))?;
    PorterRegistry::from_config(config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_reload_registry_empty_config() {
        // Create a temp file with a minimal valid porter.toml
        let mut temp = NamedTempFile::new().expect("create temp file");
        writeln!(temp, "# empty porter.toml").expect("write to temp file");

        let result = reload_registry(temp.path()).await;
        assert!(
            result.is_ok(),
            "empty config should reload successfully: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn test_reload_registry_invalid_toml() {
        let mut temp = NamedTempFile::new().expect("create temp file");
        writeln!(temp, "this is not valid toml {{{{").expect("write to temp file");

        let result = reload_registry(temp.path()).await;
        assert!(result.is_err(), "invalid TOML should fail to reload");
        let err_str = result.err().unwrap().to_string();
        assert!(
            err_str.contains("hot-reload"),
            "error should mention hot-reload: {}",
            err_str
        );
    }

    #[tokio::test]
    async fn test_reload_registry_missing_file() {
        let path = PathBuf::from("/nonexistent/path/porter.toml");
        let result = reload_registry(&path).await;
        assert!(result.is_err(), "missing file should fail to reload");
    }

    #[tokio::test]
    async fn test_notify_peers_empty_vec() {
        // Should not panic on empty peers vec
        let peers: Arc<tokio::sync::Mutex<Vec<Peer<RoleServer>>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        notify_peers(&peers).await;
        assert!(peers.lock().await.is_empty());
    }

    /// Verify reload_registry preserves server configs from TOML.
    #[tokio::test]
    async fn test_reload_registry_with_valid_config() {
        let mut temp = NamedTempFile::new().expect("create temp file");
        // Write a valid config with no enabled servers (to avoid spawning actual processes)
        writeln!(
            temp,
            r#"
[servers.test-server]
slug = "test"
transport = "stdio"
command = "echo"
enabled = false
"#
        )
        .expect("write");

        let result = reload_registry(temp.path()).await;
        assert!(
            result.is_ok(),
            "valid config with disabled server should load: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
        // Disabled server is not in the registry
        let registry = result.unwrap();
        assert_eq!(
            registry.server_count(),
            0,
            "disabled server should not be spawned"
        );
    }
}
