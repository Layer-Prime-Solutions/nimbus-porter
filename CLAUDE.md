# Porter — Claude Instructions

## What This Is

Porter is a standalone MCP gateway that aggregates MCP servers behind a single unified MCP endpoint. It can run independently (`porter serve` / `porter stdio`) or as part of the Nimbus platform.

## Keeping Docs In Sync (Critical)

When making changes to Porter, you MUST update the following in the same commit:

1. **README.md** — If you add/change/remove any user-facing feature, CLI flag, config option, or behavior, update the README to match. This includes: config schema examples, usage examples, client configuration snippets, and safety guarantees.
2. **porter.example.toml** — If you add/change/remove config fields or introduce new config patterns, update the example config to demonstrate them.

Do not defer doc updates to a follow-up commit. The README and example config are the primary user-facing documentation — they must always reflect the current state of the code.

## Development Workflow (Critical)

### Worktrees

All feature work and non-trivial changes MUST be done in a git worktree. Trivial changes (typos, small config tweaks, doc fixes) can skip worktrees. Use the `using-git-worktrees` skill to create an isolated worktree before starting implementation.

### Tests

All new code MUST include tests. All existing tests MUST pass before committing. Run `cargo test --workspace` before every commit. If a change breaks existing tests, fix them — do not skip or delete tests to make things green.

- New public functions need unit tests
- New behavior needs coverage for both success and error paths
- Bug fixes need a regression test demonstrating the fix

### Code Review

After completing a logical chunk of work (feature, bugfix, refactor), request a code review using the `requesting-code-review` skill before merging or creating a PR. Agent-authored code is not exempt — all implementation work gets reviewed.

## Project Structure

```
porter/
├── Cargo.toml              # Workspace root (resolver 3, members: ["cli"])
├── src/                    # Core library (nimbus-porter)
│   ├── lib.rs              # Public API re-exports
│   ├── error.rs            # PorterError enum + Result alias
│   ├── config.rs           # TOML config deserialization & validation
│   ├── registry.rs         # PorterRegistry — central tool aggregator
│   ├── namespace.rs        # Tool namespacing (slug__tool_name)
│   ├── server/             # MCP server management
│   │   ├── mod.rs          # ServerHandle struct
│   │   ├── health.rs       # HealthState, ErrorRateTracker
│   │   ├── stdio.rs        # STDIO subprocess transport
│   │   └── http.rs         # HTTP client transport
│   └── standalone/         # Standalone server mode
│       ├── mod.rs
│       ├── server.rs       # PorterMcpServer (ServerHandler impl)
│       └── hot_reload.rs   # File watcher + registry swap
├── cli/                    # Binary crate
│   ├── Cargo.toml
│   └── src/main.rs         # Entry point: clap CLI, serve/stdio subcommands, banner
├── README.md               # User-facing docs (keep in sync!)
├── porter.example.toml     # Example config (keep in sync!)
└── .github/workflows/ci.yml
```

## Rust Conventions

### Edition & Workspace

- Rust 2024 edition, resolver 3
- Keep dependencies up to date when practical. If a major version bump requires large-scale changes, note it and defer — but prefer staying current.
- Library: `nimbus-porter` (root `src/`)
- Binary: `porter` (`cli/src/main.rs`)
- License: MIT OR Apache-2.0

### Naming

- Files: `snake_case` — `hot_reload.rs`, `health.rs`
- Structs/Traits: `PascalCase` — `PorterRegistry`, `ServerHandle`
- Constants: `SCREAMING_SNAKE_CASE` — `MAX_FAILURES`, `BACKOFF_INITIAL`
- Error variants: `PascalCase(String)` — `DuplicateSlug(String)`
- Functions: `snake_case` — `spawn_server`, `from_config`

### DRY (Critical)

Be paranoid about duplication. Before writing new code, search for existing implementations that do the same thing. Extract shared logic into functions or traits rather than copying patterns across files. If you find yourself writing something that looks like it already exists elsewhere, it probably does — find it and reuse it.

### Unwrap/Expect Policy

- **Never `unwrap()` in library code** (`src/`). Always propagate errors with `?` or `.map_err()`.
- `expect("reason")` is allowed only for invariants that are provably unreachable (e.g., a regex that is known valid at compile time). The message must explain WHY it can't fail.
- **`unwrap()` is fine in tests** — test failures are the point.
- CLI binary (`cli/src/main.rs`) may use `anyhow` for top-level error handling but should still avoid `unwrap()`.

### Unsafe

No `unsafe` code. There is no use case in Porter that warrants it. If you think you need `unsafe`, you're solving the wrong problem.

### Visibility

- Default to private. Only make things `pub` if they're part of the library's public API (re-exported from `lib.rs`).
- Use `pub(crate)` for items shared across modules but not exported to consumers.
- Agents tend to over-expose — when in doubt, keep it private.

### String Types

- Function parameters: prefer `&str` for borrowed input, `impl Into<String>` sparingly
- Struct fields: `String` for owned data
- Return types: `String` for owned, avoid returning `&str` unless lifetime is obvious
- Error payloads: always `String` (owned, no lifetime complications)

### Clone Discipline

- Prefer `Arc<T>` for shared ownership across tasks — don't `.clone()` large structs
- `.clone()` is fine for small types (`String`, config values, identifiers)
- If you're cloning an `Arc`, that's cheap and expected
- If you're cloning a `Vec<Tool>` or similar collection, consider whether a reference or `Arc` would work

### Doc Comments

- `///` on all public API items (functions, structs, traits, enum variants exported from `lib.rs`)
- Internal/private items don't need doc comments — code should be self-explanatory
- Don't add `//` comments restating what the code obviously does

### Derive Ordering

Consistent order: `#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]` — only include what's needed. `Debug` first, then `Clone`, then comparison traits, then serde.

### Module Organization

- Use `mod.rs` for directories (not Rust 2018 `foo.rs` + `foo/` pattern)
- `lib.rs` re-exports curated public surface with explicit `pub mod` declarations

### Imports

- External crates alphabetically, then `crate::` — no blank line separation
- Group items from same crate: `use tokio::sync::{mpsc, watch, RwLock};`
- In tests: `use super::*;` first

### Errors

- `thiserror` enum: `PorterError` in `src/error.rs`
- Type alias: `pub type Result<T> = std::result::Result<T, PorterError>;`
- All variants carry contextual slug (server name) as `String` payload
- Cross-boundary conversion: `.map_err(|e| PorterError::Variant(e.to_string()))`
- CLI binary uses `anyhow::Result` at top level, maps `PorterError` to `anyhow`

### Tracing

- Use `tracing` (not `log`): `tracing::info!(field = %value, "message")`
- Structured fields, not format string interpolation
- No `.entered()` calls in library code — spans applied at call sites via `.instrument()`
- Subscriber initialized in `cli/src/main.rs` with `EnvFilter` and stderr writer

### Testing

- All unit tests inline: `#[cfg(test)] mod tests { use super::*; ... }`
- Test helpers (mock builders, fixtures) defined inside `mod tests` block — self-contained per file
- `#[tokio::test]` for async, `#[test]` for sync
- No mocking framework — mock structs built directly
- Assertions: `assert_eq!()`, `assert!()`, `matches!()` with guards

## Architecture

### PorterRegistry (Central Aggregator)

- `from_config(config)` — validates, spawns all servers, returns Registry
- `tools()` — aggregates namespaced tools from all healthy servers
- `call_tool(name, args)` — routes to correct server by slug prefix
- `shutdown()` — cancels all server tasks

### ServerHandle (MCP Server Wrapper)

- Uniform interface for STDIO and HTTP transports
- Fields: `slug`, `health_rx` (watch channel), `tools` (Arc<RwLock>), `call_tx` (mpsc)
- Transport details (restart loops, reconnect) are fully encapsulated

### PorterMcpServer (ServerHandler)

- Double-Arc pattern: `Arc<RwLock<Arc<PorterRegistry>>>`
- Outer Arc shared by all sessions; inner Arc swapped by hot-reload
- Implements `rmcp::handler::server::ServerHandler`

### Tool Namespacing

- Format: `slug__tool_name` (double underscore separator)
- Descriptions prefixed: `[via slug] original description`
- Slugs validated: alphanumeric + hyphens, no double underscores

### Health Tracking

- Sliding-window `ErrorRateTracker` (VecDeque of timestamped outcomes)
- States: Starting (< 5 samples) → Healthy (< 5%) → Degraded (5-50%) → Unhealthy (> 50%)
- Unhealthy servers excluded from tool listing and tool calls

### Hot-Reload

- `notify` crate watches config file, 100ms debounce
- On change: reload TOML → rebuild PorterRegistry → swap inner Arc → notify peers
- CRITICAL: keep watcher variable alive (`_watcher`) — dropping it silently stops OS watch

## Critical Pitfalls

1. **Slug constraints**: Alphanumeric + hyphens only, no double underscores (conflicts with namespace separator).
2. **Hot-reload watcher lifetime**: The `notify` watcher must be kept alive via variable binding (`_watcher`). Dropping it silently stops file watching.
3. **Health state persistence**: Sliding window is not reset on reconnect — error history carries over.
4. **tokio::sync::Mutex** (not std) for fields shared across `tokio::spawn` — `std::sync::MutexGuard` is `!Send` across `.await`.
5. **Banner respects NO_COLOR**: `print_banner()` in `cli/src/main.rs` checks `NO_COLOR` env var and terminal detection before emitting ANSI codes.

## CI

- Format: `cargo fmt --all --check`
- Lint: `cargo clippy --workspace -- -D warnings`
- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- `RUSTFLAGS = "-D warnings"` (warnings as errors)
