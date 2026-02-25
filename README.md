<p align="center">

```
 ████████   ██████   ████████  ████████  ████████  ████████
 ██    ██  ██    ██  ██    ██     ██     ██        ██    ██
 ████████  ██    ██  ████████     ██     ██████    ████████
 ██        ██    ██  ██  ██       ██     ██        ██  ██
 ██         ██████   ██    ██     ██     ████████  ██    ██
```

</p>

<h3 align="center">Standalone MCP Gateway</h3>
<p align="center"><em>Part of the Nimbus Ecosystem</em></p>

---

Porter wraps CLI tools (aws, kubectl, gh, and more) and MCP servers into a single unified MCP endpoint. Use it standalone with `porter serve` or `porter stdio`, or as part of the [Nimbus](https://github.com/Layer-Prime-Solutions/nimbus) platform.

## What is Porter?

Porter is a library and binary that aggregates multiple tool sources behind a single MCP interface:

- **CLI tools**: Wraps arbitrary command-line programs (aws, kubectl, gh, az, etc.) as callable MCP tools, with built-in read-only profiles and `--help`-based schema discovery
- **MCP servers**: Manages external MCP servers over STDIO or Streamable HTTP transports, namespace-isolating their tools to prevent name collisions
- **Safety by default**: Read-only access enforced by default; write operations require explicit opt-in per subcommand

Works standalone without any Nimbus infrastructure. Connect any MCP client directly.

## Install

```bash
cargo install nimbus-porter-cli
```

Build from source:

```bash
git clone https://github.com/Layer-Prime-Solutions/porter
cd porter
cargo build --release
# Binary is at ./target/release/porter
```

## Quick Start

**1. Create a `porter.toml` config file** (or copy the example: `cp porter.example.toml porter.toml`):

```toml
# MCP servers (STDIO or HTTP transports)
[servers.github-mcp]
slug = "gh-mcp"
transport = "stdio"
command = "gh-mcp"

[servers.context7]
slug = "c7"
transport = "http"
url = "https://mcp.context7.com/mcp"

# CLI tools with built-in profiles
[cli.aws]
slug = "aws"
transport = "cli"
command = "aws"
profile = "aws"         # use built-in AWS profile (read-only by default)
env.AWS_PROFILE = "$AWS_PROFILE"
timeout_secs = 30

[cli.kubectl]
slug = "k8s"
transport = "cli"
command = "kubectl"
profile = "kubectl"     # use built-in kubectl profile
env.KUBECONFIG = "$KUBECONFIG"
```

**2. Start Porter:**

```bash
porter serve
```

Output:
```
Porter HTTP server listening host=127.0.0.1 port=3000
Connect your MCP client to http://127.0.0.1:3000/mcp
```

**3. Connect your MCP client** to `http://127.0.0.1:3000/mcp`

All tools from all configured servers and CLI tools are available at that single endpoint.

## Configuration

Porter config uses `porter.toml` (or a custom path via `--config`).

### Listen

Set default bind address and port for `porter serve`. CLI flags `--host` and `--port` override these when provided.

```toml
[listen]
host = "127.0.0.1"    # Default: "127.0.0.1"
port = 3000            # Default: 3000
```

### MCP Servers

```toml
[servers.<name>]
slug = "unique-id"          # Required: identifier used as tool namespace prefix
transport = "stdio"         # "stdio" or "http"
enabled = true              # Optional: default true

# For stdio transport:
command = "my-mcp-server"   # Required for stdio
args = ["--verbose"]        # Optional extra args
env.MY_VAR = "$MY_VAR"     # Optional env vars (must reference $ENV_VAR)
cwd = "/path/to/dir"       # Optional working directory

# For http transport:
url = "https://mcp.example.com/mcp"  # Required for http
```

### CLI Tools

```toml
[cli.<name>]
slug = "unique-id"              # Required: identifier used as tool namespace prefix
transport = "cli"               # Must be "cli"
command = "aws"                 # Required: command to run
enabled = true                  # Optional: default true

profile = "aws"                 # Optional: use built-in profile (see Built-in Profiles)
args = []                       # Optional: extra args always appended
env.AWS_PROFILE = "$AWS_PROFILE"  # Optional env vars (must reference $ENV_VAR)

# Access control (deny has highest priority)
allow = ["s3", "ec2"]           # Optional: only allow these subcommand prefixes
deny = ["s3 rm", "ec2 terminate"]  # Optional: always block these (overrides allow)

# Per-subcommand write access opt-in
[cli.<name>.write_access]
"s3 cp" = true                  # Allow s3 cp (write) while keeping other writes blocked

timeout_secs = 30               # Optional: default 30
inject_flags = ["--output", "json"]  # Optional: flags always appended to calls
expand_subcommands = true       # Optional: expose each subcommand as a separate MCP tool

# JSON Schema override when --help parsing is insufficient
schema_override = { type = "object", properties = { args = { type = "array" } } }
```

### Full Example

```toml
[servers.filesystem]
slug = "fs"
transport = "stdio"
command = "mcp-server-filesystem"
args = ["/home/user/projects"]

[cli.aws]
slug = "aws"
transport = "cli"
command = "aws"
profile = "aws"
env.AWS_PROFILE = "$AWS_PROFILE"
env.AWS_REGION = "$AWS_REGION"
timeout_secs = 60

[cli.kubectl]
slug = "k8s"
transport = "cli"
command = "kubectl"
profile = "kubectl"
env.KUBECONFIG = "$KUBECONFIG"
deny = ["delete", "drain", "cordon"]

[cli.custom-tool]
slug = "mytool"
transport = "cli"
command = "my-internal-tool"
inject_flags = ["--format", "json"]
timeout_secs = 10
```

## Usage

### porter serve

Start a Streamable HTTP MCP server:

```bash
# Default: localhost:3000, config from porter.toml
porter serve

# Custom config, port, and host
porter serve --config my-tools.toml --port 8080 --host 0.0.0.0
```

Options:
- `--config` / `-c`: Path to config file (default: `./porter.toml` or `~/.config/porter/porter.toml`)
- `--port` / `-p`: HTTP port (overrides `[listen].port` from config; default: `3000`)
- `--host`: Bind address (overrides `[listen].host` from config; default: `127.0.0.1`)

**Hot-reload**: Porter watches the config file for changes. When you edit `porter.toml`, Porter automatically reloads the tool surface and sends a `tools/list_changed` notification to all connected MCP clients — no restart required.

MCP endpoint: `http://<host>:<port>/mcp`

### porter stdio

Bridge all configured tools over STDIO for Claude Desktop and other STDIO-based MCP clients:

```bash
porter stdio
porter stdio --config /path/to/porter.toml
```

Options:
- `--config` / `-c`: Path to config file (default: `./porter.toml` or `~/.config/porter/porter.toml`)

## Client Configuration

Porter searches for config in order: `./porter.toml` then `~/.config/porter/porter.toml`. If you place your config at `~/.config/porter/porter.toml`, the examples below work without `--config`.

### Claude Code

```bash
claude mcp add porter -- porter stdio
```

Or with an explicit config path:

```bash
claude mcp add porter -- porter stdio --config ~/.config/porter/porter.toml
```

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "porter": {
      "command": "porter",
      "args": ["stdio"]
    }
  }
}
```

### Cursor

Add to Cursor MCP settings (`.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "porter": {
      "command": "porter",
      "args": ["stdio"]
    }
  }
}
```

### VS Code / Copilot

Add to `.vscode/mcp.json`:

```json
{
  "servers": {
    "porter": {
      "type": "stdio",
      "command": "porter",
      "args": ["stdio"]
    }
  }
}
```

### Windsurf

Add to `~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "porter": {
      "command": "porter",
      "args": ["stdio"]
    }
  }
}
```

### Any Streamable HTTP client

Start `porter serve` and connect to the endpoint:

```
http://127.0.0.1:3000/mcp
```

## Built-in Profiles

Built-in profiles provide pre-configured read-only access for common CLI tools, with default flag injection for structured output.

| Profile | Command | Default Inject Flags | Expands Subcommands |
|---------|---------|---------------------|---------------------|
| `aws` | `aws` | `--output json` | Yes |
| `gcloud` | `gcloud` | `--format json` | Yes |
| `kubectl` | `kubectl` | `--output json` | Yes |
| `gh` | `gh` | `--json` | Yes |
| `az` | `az` | `--output json` | Yes |
| `ansible` | `ansible` | _(none)_ | No |
| `gitlab` | `glab` | _(none)_ | No |
| `doggo` | `doggo` | _(none)_ | No |
| `rg` | `rg` | _(none)_ | No |
| `tldr` | `tldr` | _(none)_ | No |
| `whois` | `whois` | _(none)_ | No |

Profiles enforce read-only subcommands by default. Use `write_access` config to opt in to specific write operations.

## Use-Case Recipes

`porter.example.toml` includes ready-to-uncomment config blocks for connecting AI agents to popular project management and code review platforms via MCP servers. Each recipe documents every exposed tool by access tier so you can make informed decisions about what to allow.

### Access Tiers

| Tier | Default | Description | Examples |
|------|---------|-------------|----------|
| **Read** | Enabled | Passive observation — no side effects | search, list, get, view, diff |
| **Feedback** | Enabled | Participate in workflows without destructive side effects | create issues, add comments, update labels/status |
| **Destructive** | Blocked | Irreversible or high-impact actions | delete, merge, close, archive |
| **Code Write** | Blocked | Significant mutations to source code | push files, create branches, fork repos |

**Read** and **Feedback** tools are safe to expose to AI agents by default. **Destructive** and **Code Write** tools should be explicitly opted into after reviewing the implications.

### Available Recipes

| Platform | Package | Transport | Auth |
|----------|---------|-----------|------|
| GitHub | `@modelcontextprotocol/server-github` | STDIO | `GITHUB_TOKEN` |
| GitLab | `@modelcontextprotocol/server-gitlab` | STDIO | `GITLAB_TOKEN` (+ optional `GITLAB_API_URL`) |
| Linear | `mcp-remote` → `https://mcp.linear.app/sse` | STDIO (remote bridge) | OAuth (browser flow) |
| Jira | `mcp-atlassian` (via `uvx`) | STDIO | `JIRA_URL` + `JIRA_USERNAME` + `JIRA_API_TOKEN` |
| ClickUp | `clickup-mcp-server` | STDIO | `CLICKUP_API_KEY` + `CLICKUP_TEAM_ID` |

To use a recipe: open `porter.example.toml`, find the platform block, uncomment it, set the required environment variables, and copy it into your `porter.toml`.

> **Note**: These are third-party MCP servers maintained by their respective communities. Review each server's documentation and security posture before exposing it to AI agents in production environments.

## Safety

- **Read-only by default**: CLI tools are read-only unless `write_access` is configured
- **Deny overrides allow**: The `deny` list has highest priority; an explicit `deny` entry blocks even if the subcommand is in `allow`
- **No shell injection**: Porter constructs command arguments from structured JSON inputs — raw user strings are never passed to a shell
- **Per-subcommand write opt-in**: Write access is granular — allow `s3 cp` without enabling `s3 rm`
- **Structured invocation**: Porter builds the command from structured args, never interpolates strings

Error message for blocked commands:
```
Command blocked: aws s3 rm is a write operation. Enable write_access in config to allow.
```

## License

Licensed under MIT OR Apache-2.0 at your option.
