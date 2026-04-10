# STDIO Hot-Reload Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire hot-reload into the STDIO transport so both `porter serve` and `porter stdio` share identical initialization and config-reload behavior.

**Architecture:** Extract shared initialization (load config, build registry, create server, spawn hot-reload watcher) into `init_server()` in `cli/src/main.rs`. Both `run_serve` and `run_stdio` call it, then do only their transport-specific serving. Update the `hot_reload.rs` module doc to reflect that hot-reload is no longer serve-only.

**Tech Stack:** Rust, tokio, rmcp, notify (existing deps — no new crates)

---

### Task 1: Extract `init_server()` and refactor `run_serve` to use it

**Files:**
- Modify: `cli/src/main.rs`

- [ ] **Step 1: Add the `init_server` function**

Add this function after the existing `load_config` function (after line 244):

```rust
/// Shared initialization for both serve and stdio modes.
///
/// Loads config, builds the registry, creates the MCP server, and spawns
/// the hot-reload background task. Returns the server and parsed config
/// (callers may need config fields like `listen`).
async fn init_server(
    config_path: &PathBuf,
    cancel: &CancellationToken,
) -> Result<(PorterMcpServer, PorterConfig)> {
    let config = load_config(config_path).await?;

    let registry = PorterRegistry::from_config(config.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build Porter registry: {}", e))?;

    let server = PorterMcpServer::new(registry);

    // Spawn hot-reload background task — watches config file, swaps registry
    // on change, notifies connected MCP client peers of tools-list-changed
    tokio::spawn(run_hot_reload(
        config_path.clone(),
        server.registry_handle(),
        server.peers_handle(),
        cancel.child_token(),
    ));

    Ok((server, config))
}
```

- [ ] **Step 2: Refactor `run_serve` to use `init_server`**

Replace the body of `run_serve` (lines 104-168) with:

```rust
async fn run_serve(
    config_path: PathBuf,
    host_override: Option<String>,
    port_override: Option<u16>,
    cancel: CancellationToken,
) -> Result<()> {
    let (server, config) = init_server(&config_path, &cancel).await?;

    let host = host_override.unwrap_or(config.listen.host.clone());
    let port = port_override.unwrap_or(config.listen.port);

    // Set up Streamable HTTP MCP service (same pattern as Navigator's run_navigator_http)
    let session_manager = Arc::new(LocalSessionManager::default());
    let http_config = StreamableHttpServerConfig {
        cancellation_token: cancel.clone(),
        ..Default::default()
    };
    let server_for_factory = server.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(server_for_factory.clone()),
        session_manager,
        http_config,
    );

    let app = Router::new().fallback(move |req: Request<axum::body::Body>| {
        let svc = mcp_service.clone();
        async move {
            match svc.oneshot(req).await {
                Ok(resp) => resp.into_response(),
                Err(e) => {
                    tracing::error!(error = %e, "MCP service error");
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
    });

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", addr, e))?;

    tracing::info!(host = %host, port = %port, "Porter HTTP server listening");
    tracing::info!("Connect your MCP client to http://{}:{}/mcp", host, port);

    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .map_err(|e| anyhow::anyhow!("Porter HTTP server error: {}", e))?;

    tracing::info!("Porter HTTP server stopped");
    Ok(())
}
```

- [ ] **Step 3: Build to verify refactor compiles**

Run: `cargo build --workspace`
Expected: Compiles cleanly with no errors or warnings.

- [ ] **Step 4: Run tests to verify no regressions**

Run: `cargo test --workspace`
Expected: All existing tests pass.

- [ ] **Step 5: Commit**

```bash
git add cli/src/main.rs
git commit -m "refactor: extract init_server() from run_serve"
```

---

### Task 2: Wire `run_stdio` to use `init_server`

**Files:**
- Modify: `cli/src/main.rs`

- [ ] **Step 1: Refactor `run_stdio` to use `init_server`**

Replace the body of `run_stdio` (lines 175-209 in the original, offset after Task 1 changes) with:

```rust
async fn run_stdio(config_path: PathBuf, cancel: CancellationToken) -> Result<()> {
    let (server, _config) = init_server(&config_path, &cancel).await?;

    // Use rmcp's STDIO transport (same pattern as Navigator's run_navigator_stdio)
    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let running = server
        .serve_with_ct(transport, cancel.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize Porter stdio transport: {:?}", e))?;

    tracing::info!("Porter stdio transport initialized, waiting for messages");

    tokio::select! {
        result = running.waiting() => {
            match result {
                Ok(reason) => {
                    tracing::info!(?reason, "Porter stdio transport completed");
                }
                Err(e) => {
                    tracing::error!(error = %e, "Porter stdio transport error");
                    return Err(anyhow::anyhow!("Porter stdio transport error: {}", e));
                }
            }
        }
        _ = cancel.cancelled() => {
            tracing::info!("Porter stdio transport cancelled");
        }
    }

    Ok(())
}
```

The only change is the first line: `load_config` + `PorterRegistry::from_config` + `PorterMcpServer::new` collapse into `init_server()`. The rest is identical.

- [ ] **Step 2: Build to verify**

Run: `cargo build --workspace`
Expected: Compiles cleanly.

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add cli/src/main.rs
git commit -m "feat: wire hot-reload into porter stdio mode"
```

---

### Task 3: Update `hot_reload.rs` module doc

**Files:**
- Modify: `src/standalone/hot_reload.rs`

- [ ] **Step 1: Update the module doc comment**

Replace lines 1-10 of `src/standalone/hot_reload.rs`:

```rust
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
```

With:

```rust
//! Hot-reload for Porter (both `serve` and `stdio` modes).
//!
//! Watches the porter.toml config file using the `notify` crate. On each
//! detected change (with 100ms debounce), it re-parses the config and rebuilds
//! the PorterRegistry. On success, the inner Arc<PorterRegistry> is swapped
//! inside the outer Arc<RwLock<...>>, and all connected MCP client peers
//! receive a tools-list-changed notification.
//!
//! Stale peers (whose transport has closed) are pruned on notification error.
//! On reload failure, the previous registry is preserved and a warning is logged.
```

- [ ] **Step 2: Run CI checks**

Run: `cargo fmt --all --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: All pass.

- [ ] **Step 3: Commit**

```bash
git add src/standalone/hot_reload.rs
git commit -m "docs: update hot_reload module doc to reflect both serve and stdio modes"
```
