# Design: STDIO Hot-Reload

**Date:** 2026-04-10
**Goal:** Wire hot-reload into the STDIO transport so both `porter serve` and `porter stdio` share the same initialization and config-reload behavior.

## Problem

`porter serve` spawns a hot-reload background task that watches `porter.toml`, rebuilds the `PorterRegistry` on change, and notifies connected MCP clients via `tools/list_changed`. `porter stdio` skips this entirely — config changes require a full restart.

Both modes are persistent streaming connections that differ only in transport (HTTP vs stdin/stdout). The initialization and lifecycle should be the same.

## Design

### Extract shared setup into `init_server()`

Create a function in `cli/src/main.rs`:

```rust
async fn init_server(
    config_path: &PathBuf,
    cancel: &CancellationToken,
) -> Result<(PorterMcpServer, PorterConfig)>
```

This function owns the full initialization sequence:

1. `load_config(config_path)` — parse TOML
2. `PorterRegistry::from_config(config)` — validate config, spawn server handles
3. `PorterMcpServer::new(registry)` — wrap in double-Arc server
4. Get `registry_handle()` and `peers_handle()` from the server
5. Spawn `run_hot_reload()` on `cancel.child_token()`
6. Return `(server, config)` — config is returned because `run_serve` needs `listen.host`/`listen.port`

### Simplify `run_serve`

Before:
```
load_config → build registry → create server → get handles → spawn hot-reload → HTTP setup → serve
```

After:
```
init_server() → extract host/port from config → HTTP setup → serve
```

`run_serve` becomes responsible only for: resolving host/port (with CLI overrides), setting up `StreamableHttpService` + axum, and serving.

### Simplify `run_stdio`

Before:
```
load_config → build registry → create server → STDIO transport → serve_with_ct → wait
```

After:
```
init_server() → STDIO transport → serve_with_ct → wait
```

`run_stdio` becomes responsible only for: setting up stdin/stdout transport and waiting for completion.

### Peer notification in STDIO

The MCP `tools/list_changed` notification works identically in both modes:

- `PorterMcpServer::on_initialized()` stores the connected peer (called by rmcp after handshake)
- `run_hot_reload` → `notify_peers()` iterates stored peers and sends the notification
- In serve mode: multiple peers (one per HTTP session)
- In stdio mode: single peer (the connected client)

No changes needed to the notification machinery — it's already transport-agnostic.

### Doc updates

- `src/standalone/hot_reload.rs` module doc: change "Hot-reload for `porter serve`" to "Hot-reload for Porter" (or similar) since it's now shared by both modes.

### What does NOT change

- `src/standalone/hot_reload.rs` — no code changes, only module doc update
- `src/standalone/server.rs` — unchanged
- `src/config.rs` — unchanged
- `README.md` — already says "Porter watches the config file" without scoping to serve-only
- `porter.example.toml` — unchanged

### Tests

The hot-reload unit tests (`hot_reload.rs::tests`) cover the reload/notify machinery. The `init_server` function is a thin composition of already-tested functions (`load_config`, `PorterRegistry::from_config`, `PorterMcpServer::new`, `run_hot_reload`) — no new unit tests needed for the wiring itself.

## Files Changed

| File | Change |
|------|--------|
| `cli/src/main.rs` | Extract `init_server()`, simplify `run_serve` and `run_stdio` |
| `src/standalone/hot_reload.rs` | Update module doc comment only |
