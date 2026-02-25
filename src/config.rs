//! Porter server configuration — deserialization and validation.

use crate::error::PorterError;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Strip an env var reference to its variable name.
///
/// Accepts `${VAR_NAME}` syntax only. Returns `None` if the value is not a
/// valid env-var reference.
pub fn parse_env_ref(value: &str) -> Option<&str> {
    value.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
}

/// Resolve a map of env-var references to their actual values.
///
/// Each value must be `${VAR}` or `$VAR`. Unknown variables resolve to the
/// empty string (same as shell `${UNSET-}`).
pub fn resolve_env_vars(env: &HashMap<String, String>) -> HashMap<String, String> {
    env.iter()
        .map(|(k, v)| {
            let resolved = match parse_env_ref(v) {
                Some(var_name) => std::env::var(var_name).unwrap_or_default(),
                None => v.clone(), // caught by validate(), but handle gracefully
            };
            (k.clone(), resolved)
        })
        .collect()
}

/// HTTP listen address defaults for `porter serve`.
///
/// Configured under `[listen]` in TOML. CLI flags `--host` and `--port`
/// override these values when provided.
#[derive(Debug, Clone, Deserialize)]
pub struct ListenConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    3000
}

/// Top-level Porter configuration, parsed from TOML.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PorterConfig {
    #[serde(default)]
    pub listen: ListenConfig,
    #[serde(default)]
    pub servers: HashMap<String, ServerConfig>,
    #[serde(default)]
    pub cli: HashMap<String, CliServerConfig>,
}

/// Configuration for a single managed MCP server.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub slug: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub transport: TransportKind,
    // STDIO fields
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
    // HTTP fields
    pub url: Option<String>,
    /// Configurable MCP handshake timeout per CONTEXT.md decision, default 30s
    #[serde(default = "default_handshake_timeout_secs")]
    pub handshake_timeout_secs: u64,
}

/// Supported MCP transport types.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Stdio,
    Http,
    Cli,
}

fn default_enabled() -> bool {
    true
}

fn default_handshake_timeout_secs() -> u64 {
    30
}

fn default_timeout_secs() -> u64 {
    30
}

fn default_discovery_budget_secs() -> u64 {
    60
}

/// Configuration for a CLI tool wrapped as a Porter MCP tool.
///
/// Configured under `[cli.*]` sections in TOML. The CLI tool is run via
/// `tokio::process::Command` (never a shell), with structured argument passing,
/// access control enforcement, and timeout-kill semantics.
#[derive(Debug, Clone, Deserialize)]
pub struct CliServerConfig {
    pub slug: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub transport: TransportKind,
    /// The executable to run (e.g., "aws", "kubectl").
    pub command: String,
    /// Built-in profile name for argument discovery and read-only enforcement.
    pub profile: Option<String>,
    /// Extra args always appended to every invocation.
    #[serde(default)]
    pub args: Vec<String>,
    /// Env var references (`${VAR}`), resolved at spawn time.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Allowed subcommand prefixes (empty means allow all).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Denied subcommand prefixes (highest priority, overrides allow).
    #[serde(default)]
    pub deny: Vec<String>,
    /// Per-subcommand write operation opt-in (maps subcommand path to true/false).
    #[serde(default)]
    pub write_access: HashMap<String, bool>,
    /// Command execution timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Flags always injected (e.g., ["--output", "json"]).
    #[serde(default)]
    pub inject_flags: Vec<String>,
    /// Override profile default for subcommand expansion.
    pub expand_subcommands: Option<bool>,
    /// JSON schema override when --help parsing is insufficient.
    pub schema_override: Option<serde_json::Value>,
    /// Recursive --help discovery depth. Default: Some(3) when a profile is set, None otherwise.
    /// 0 disables discovery. Max 5.
    #[serde(default)]
    pub help_depth: Option<u8>,
    /// Wall-clock budget in seconds for the entire discovery run. Default: 60.
    #[serde(default = "default_discovery_budget_secs")]
    pub discovery_budget_secs: u64,
}

impl PorterConfig {
    /// Validate the config, failing fast on misconfigurations before any servers are spawned.
    pub fn validate(&self) -> crate::Result<()> {
        // 1. Check for duplicate slugs
        let mut seen_slugs: HashSet<&str> = HashSet::new();
        for config in self.servers.values() {
            if !seen_slugs.insert(config.slug.as_str()) {
                return Err(PorterError::DuplicateSlug(config.slug.clone()));
            }
        }

        // 2. Validate each enabled server
        for config in self.servers.values() {
            if !config.enabled {
                continue;
            }

            let slug = &config.slug;

            // 3. Validate slug format: non-empty, alphanumeric + hyphens, no double underscores
            if slug.is_empty()
                || slug.contains("__")
                || !slug.chars().all(|c| c.is_alphanumeric() || c == '-')
            {
                return Err(PorterError::InvalidConfig(
                    slug.clone(),
                    "slug must be non-empty alphanumeric with hyphens, no double underscores"
                        .to_string(),
                ));
            }

            // 4. Validate transport-specific required fields
            match config.transport {
                TransportKind::Stdio => {
                    if config.command.is_none() {
                        return Err(PorterError::InvalidConfig(
                            slug.clone(),
                            "STDIO transport requires 'command' field".to_string(),
                        ));
                    }
                    if config.url.is_some() {
                        return Err(PorterError::InvalidConfig(
                            slug.clone(),
                            "STDIO transport should not have 'url' field".to_string(),
                        ));
                    }
                }
                TransportKind::Http => {
                    if config.url.is_none() {
                        return Err(PorterError::InvalidConfig(
                            slug.clone(),
                            "HTTP transport requires 'url' field".to_string(),
                        ));
                    }
                    if config.command.is_some() {
                        return Err(PorterError::InvalidConfig(
                            slug.clone(),
                            "HTTP transport should not have 'command' field".to_string(),
                        ));
                    }
                }
                TransportKind::Cli => {
                    // CLI transport is not valid in the servers map — use [cli.*] sections
                    return Err(PorterError::InvalidConfig(
                        slug.clone(),
                        "CLI transport must be configured under [cli.*], not [servers.*]"
                            .to_string(),
                    ));
                }
            }

            // 5. Validate env var references: must be ${VAR}
            for (key, value) in &config.env {
                if parse_env_ref(value).is_none() {
                    return Err(PorterError::InvalidConfig(
                        slug.clone(),
                        format!(
                            "env value for key '{}' must be a ${{VAR}} reference, got '{}'",
                            key, value
                        ),
                    ));
                }
            }
        }

        // 6. Validate CLI configs
        for cli_config in self.cli.values() {
            let slug = &cli_config.slug;

            // Check CLI slugs don't collide with MCP server slugs (or each other)
            if !seen_slugs.insert(slug.as_str()) {
                return Err(PorterError::DuplicateSlug(slug.clone()));
            }

            if !cli_config.enabled {
                continue;
            }

            // Validate command is not empty
            if cli_config.command.is_empty() {
                return Err(PorterError::InvalidConfig(
                    slug.clone(),
                    "CLI transport requires non-empty 'command' field".to_string(),
                ));
            }

            // Validate transport is Cli
            if cli_config.transport != TransportKind::Cli {
                return Err(PorterError::InvalidConfig(
                    slug.clone(),
                    "CLI tool must have transport = \"cli\"".to_string(),
                ));
            }

            // Validate env var references: must be ${VAR}
            for (key, value) in &cli_config.env {
                if parse_env_ref(value).is_none() {
                    return Err(PorterError::InvalidConfig(
                        slug.clone(),
                        format!(
                            "env value for key '{}' must be a ${{VAR}} reference, got '{}'",
                            key, value
                        ),
                    ));
                }
            }

            // Validate help_depth
            if let Some(depth) = cli_config.help_depth {
                if depth > 5 {
                    return Err(PorterError::InvalidConfig(
                        slug.clone(),
                        format!("help_depth {} exceeds maximum of 5", depth),
                    ));
                }
                if depth > 0 && cli_config.discovery_budget_secs == 0 {
                    return Err(PorterError::InvalidConfig(
                        slug.clone(),
                        "discovery_budget_secs must be > 0 when help_depth > 0".to_string(),
                    ));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_toml(toml_str: &str) -> PorterConfig {
        toml::from_str(toml_str).expect("valid TOML")
    }

    #[test]
    fn test_parse_env_ref() {
        assert_eq!(parse_env_ref("${FOO}"), Some("FOO"));
        assert_eq!(parse_env_ref("${AWS_PROFILE}"), Some("AWS_PROFILE"));
        assert_eq!(parse_env_ref("$FOO"), None);
        assert_eq!(parse_env_ref("literal"), None);
        assert_eq!(parse_env_ref("${"), None);
        assert_eq!(parse_env_ref("${}"), Some(""));
    }

    #[test]
    fn test_resolve_env_vars() {
        // SAFETY: test-only, no concurrent threads depend on this env var.
        unsafe { std::env::set_var("PORTER_TEST_VAR", "resolved_value") };
        let mut env = HashMap::new();
        env.insert("KEY".to_string(), "${PORTER_TEST_VAR}".to_string());
        let resolved = resolve_env_vars(&env);
        assert_eq!(resolved.get("KEY").unwrap(), "resolved_value");
        // SAFETY: test-only cleanup.
        unsafe { std::env::remove_var("PORTER_TEST_VAR") };
    }

    #[test]
    fn test_valid_stdio_config() {
        let config = parse_toml(
            r#"
            [servers.github]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            args = ["--port", "8080"]
            "#,
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_valid_http_config() {
        let config = parse_toml(
            r#"
            [servers.myapi]
            slug = "myapi"
            transport = "http"
            url = "https://api.example.com/mcp"
            "#,
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_duplicate_slug_fails() {
        let config = parse_toml(
            r#"
            [servers.a]
            slug = "same"
            transport = "stdio"
            command = "cmd-a"

            [servers.b]
            slug = "same"
            transport = "stdio"
            command = "cmd-b"
            "#,
        );
        let result = config.validate();
        assert!(matches!(result, Err(PorterError::DuplicateSlug(s)) if s == "same"));
    }

    #[test]
    fn test_stdio_missing_command() {
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, msg)) if slug == "gh" && msg.contains("command"))
        );
    }

    #[test]
    fn test_http_missing_url() {
        let config = parse_toml(
            r#"
            [servers.api]
            slug = "api"
            transport = "http"
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, msg)) if slug == "api" && msg.contains("url"))
        );
    }

    #[test]
    fn test_disabled_server_skips_validation() {
        let config = parse_toml(
            r#"
            [servers.broken]
            slug = "broken"
            transport = "stdio"
            enabled = false
            # command missing — but disabled, so should pass
            "#,
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_mixed_transport_fields_rejected() {
        let config = parse_toml(
            r#"
            [servers.mixed]
            slug = "mixed"
            transport = "stdio"
            command = "some-cmd"
            url = "https://example.com"
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, msg)) if slug == "mixed" && msg.contains("url"))
        );
    }

    #[test]
    fn test_env_var_reference_required() {
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"

            [servers.gh.env]
            GITHUB_TOKEN = "literal-secret"
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, msg)) if slug == "gh" && msg.contains("GITHUB_TOKEN"))
        );
    }

    #[test]
    fn test_env_var_reference_valid() {
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"

            [servers.gh.env]
            GITHUB_TOKEN = "${GITHUB_TOKEN}"
            "#,
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_env_var_bare_dollar_rejected() {
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"

            [servers.gh.env]
            GITHUB_TOKEN = "$GITHUB_TOKEN"
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, _)) if slug == "gh"),
            "bare $VAR should be rejected — use ${{VAR}} syntax"
        );
    }

    #[test]
    fn test_handshake_timeout_default() {
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            "#,
        );
        let server = config.servers.get("gh").unwrap();
        assert_eq!(server.handshake_timeout_secs, 30);
    }

    #[test]
    fn test_help_depth_defaults() {
        let config = parse_toml(
            r#"
            [cli.aws]
            slug = "aws"
            transport = "cli"
            command = "aws"
            "#,
        );
        let cli = config.cli.get("aws").unwrap();
        assert_eq!(cli.help_depth, None);
        assert_eq!(cli.discovery_budget_secs, 60);
    }

    #[test]
    fn test_help_depth_exceeds_max() {
        let config = parse_toml(
            r#"
            [cli.aws]
            slug = "aws"
            transport = "cli"
            command = "aws"
            help_depth = 6
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, msg)) if slug == "aws" && msg.contains("help_depth"))
        );
    }

    #[test]
    fn test_help_depth_zero_budget_invalid() {
        let config = parse_toml(
            r#"
            [cli.aws]
            slug = "aws"
            transport = "cli"
            command = "aws"
            help_depth = 2
            discovery_budget_secs = 0
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, msg)) if slug == "aws" && msg.contains("discovery_budget_secs"))
        );
    }

    #[test]
    fn test_help_depth_zero_valid() {
        let config = parse_toml(
            r#"
            [cli.aws]
            slug = "aws"
            transport = "cli"
            command = "aws"
            help_depth = 0
            "#,
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_listen_config_defaults() {
        let config: PorterConfig = toml::from_str("").expect("empty TOML");
        assert_eq!(config.listen.host, "127.0.0.1");
        assert_eq!(config.listen.port, 3000);
    }

    #[test]
    fn test_listen_config_custom() {
        let config = parse_toml(
            r#"
            [listen]
            host = "0.0.0.0"
            port = 8080
            "#,
        );
        assert_eq!(config.listen.host, "0.0.0.0");
        assert_eq!(config.listen.port, 8080);
    }

    #[test]
    fn test_listen_config_partial_override() {
        let config = parse_toml(
            r#"
            [listen]
            port = 9090
            "#,
        );
        assert_eq!(config.listen.host, "127.0.0.1");
        assert_eq!(config.listen.port, 9090);
    }
}
