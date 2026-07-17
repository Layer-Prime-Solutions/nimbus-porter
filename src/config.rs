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
                Some(var_name) => std::env::var(var_name).unwrap_or_else(|_| {
                    tracing::warn!(
                        key = %k,
                        var = %var_name,
                        "env var reference ${{{var_name}}} is not set, resolving to empty string"
                    );
                    String::new()
                }),
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
    /// Tool allow-list, matched against the downstream (un-namespaced) tool
    /// name — e.g. `create_issue`, not `github__create_issue`.
    ///
    /// Omitted (`None`, the default) means "allow everything not explicitly
    /// denied". When present, a tool is permitted only if it matches at least
    /// one entry — an explicit empty list (`allow = []`) therefore blocks every
    /// tool. Entries are exact names or a simple glob (`prefix*`, `*suffix`,
    /// `*inner*`).
    pub allow: Option<Vec<String>>,
    /// Tool deny-list, matched against the downstream (un-namespaced) tool
    /// name — e.g. `delete_issue`, not `github__delete_issue`.
    ///
    /// A tool matching any deny entry is blocked regardless of `allow` — deny
    /// always wins. Entries are exact names or a simple glob (`prefix*`,
    /// `*suffix`, `*inner*`).
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Supported MCP transport types.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Stdio,
    Http,
}

fn default_enabled() -> bool {
    true
}

fn default_handshake_timeout_secs() -> u64 {
    30
}

impl ServerConfig {
    /// Build the compiled tool-access filter for this server from its
    /// `allow`/`deny` lists.
    pub(crate) fn tool_filter(&self) -> ToolFilter {
        ToolFilter::new(self.allow.clone(), self.deny.clone())
    }
}

/// Compiled allow/deny policy that decides whether a downstream tool is exposed.
///
/// A tool name is permitted iff:
/// - `allow` is `None` **or** the name matches at least one `allow` entry, AND
/// - the name matches **no** `deny` entry.
///
/// Deny always wins over allow. Every entry is matched with [`glob_match`]:
/// an exact name, or a `*` wildcard at the start (suffix match), end (prefix
/// match), or both ends (substring match).
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolFilter {
    allow: Option<Vec<String>>,
    deny: Vec<String>,
}

impl ToolFilter {
    pub(crate) fn new(allow: Option<Vec<String>>, deny: Vec<String>) -> Self {
        Self { allow, deny }
    }

    /// Return why `tool_name` (the downstream, un-namespaced name) is blocked
    /// by this policy, or `None` if it is permitted.
    ///
    /// Deny is checked first so a tool present in both lists reports the
    /// deny-list reason — deny always wins.
    pub(crate) fn block_reason(&self, tool_name: &str) -> Option<&'static str> {
        if self
            .deny
            .iter()
            .any(|pattern| glob_match(pattern, tool_name))
        {
            return Some("deny list");
        }
        match &self.allow {
            None => None,
            Some(allow) if allow.iter().any(|pattern| glob_match(pattern, tool_name)) => None,
            Some(_) => Some("not in allow list"),
        }
    }

    /// Return `true` if `tool_name` (the downstream, un-namespaced name) is
    /// permitted by this policy.
    pub(crate) fn permits(&self, tool_name: &str) -> bool {
        self.block_reason(tool_name).is_none()
    }
}

/// Match a tool name against a single pattern.
///
/// Supported forms (kept deliberately simple and predictable):
/// - `name`     — exact match.
/// - `prefix*`  — prefix match (trailing wildcard).
/// - `*suffix`  — suffix match (leading wildcard).
/// - `*inner*`  — substring match (wildcard both ends).
///
/// A bare `*` (or `**`) matches everything. `*` is the only wildcard; it is not
/// interpreted anywhere other than the ends of the pattern.
fn glob_match(pattern: &str, name: &str) -> bool {
    let has_prefix_star = pattern.starts_with('*');
    let has_suffix_star = pattern.ends_with('*');
    let core = pattern.trim_matches('*');
    match (has_prefix_star, has_suffix_star) {
        (true, true) => name.contains(core),
        (true, false) => name.ends_with(core),
        (false, true) => name.starts_with(core),
        (false, false) => name == core,
    }
}

/// Return `true` if `pattern` contains a `*` in a position [`glob_match`] does
/// not interpret — anywhere other than a single leading and/or trailing
/// character. Such a `*` is silently treated as a literal, so the pattern would
/// fail open (match nothing / never fire); [`PorterConfig::validate`] rejects it.
///
/// Valid: `name`, `prefix*`, `*suffix`, `*inner*`, `*`, `**`.
/// Invalid: `delete*confirm`, `a*b*c`, `*a*b*`, `**abc`.
fn has_unsupported_glob_star(pattern: &str) -> bool {
    // Strip at most one leading and one trailing '*' — the only positions
    // glob_match honors — then any remaining '*' is an unsupported interior one.
    let after_prefix = pattern.strip_prefix('*').unwrap_or(pattern);
    let core = after_prefix.strip_suffix('*').unwrap_or(after_prefix);
    core.contains('*')
}

/// Validate slug format: non-empty, alphanumeric + hyphens only, no double underscores.
fn validate_slug_format(slug: &str) -> crate::Result<()> {
    if slug.is_empty()
        || slug.contains("__")
        || !slug.chars().all(|c| c.is_alphanumeric() || c == '-')
    {
        return Err(PorterError::InvalidConfig(
            slug.to_string(),
            "slug must be non-empty alphanumeric with hyphens, no double underscores".to_string(),
        ));
    }
    Ok(())
}

impl PorterConfig {
    /// Validate the config, failing fast on misconfigurations before any servers are spawned.
    pub fn validate(&self) -> crate::Result<()> {
        // 1. Check for duplicate slugs and validate slug format for all servers
        let mut seen_slugs: HashSet<&str> = HashSet::new();
        for config in self.servers.values() {
            validate_slug_format(&config.slug)?;
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

            // 3. Validate transport-specific required fields
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
            }

            // 4. Validate env var references: must be ${VAR}
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

            // 5. Validate allow/deny tool lists: no empty entries; a tool in
            // both lists is legal (deny wins) but almost certainly a mistake.
            let allow = config.allow.as_deref().unwrap_or(&[]);
            for entry in allow.iter().chain(&config.deny) {
                if entry.is_empty() {
                    return Err(PorterError::InvalidConfig(
                        slug.clone(),
                        "allow/deny entries must be non-empty tool names".to_string(),
                    ));
                }
                // Reject a `*` in any position glob_match cannot interpret (an
                // interior star, or a doubled leading/trailing one). Such a
                // pattern is silently treated as a literal and never fires — a
                // fail-open misconfiguration — so fail fast instead.
                if has_unsupported_glob_star(entry) {
                    return Err(PorterError::InvalidConfig(
                        slug.clone(),
                        format!(
                            "allow/deny entry '{}' has a '*' in an unsupported position — \
                             wildcards are only allowed as a single leading and/or trailing \
                             character (e.g. 'get_*', '*_issue', '*delete*')",
                            entry
                        ),
                    ));
                }
            }
            for entry in allow {
                if config.deny.iter().any(|d| d == entry) {
                    tracing::warn!(
                        server = %slug,
                        tool = %entry,
                        "tool listed in both allow and deny — deny wins, tool will be blocked"
                    );
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

    #[test]
    fn test_glob_match_forms() {
        // Exact
        assert!(glob_match("call_aws", "call_aws"));
        assert!(!glob_match("call_aws", "call_awss"));
        // Prefix (trailing *)
        assert!(glob_match("get_*", "get_issue"));
        assert!(glob_match("get_*", "get_"));
        assert!(!glob_match("get_*", "list_issue"));
        // Suffix (leading *)
        assert!(glob_match("*_issue", "create_issue"));
        assert!(!glob_match("*_issue", "issue_note"));
        // Substring (both ends)
        assert!(glob_match("*create*", "batch_create_issues"));
        assert!(glob_match("*create*", "create_repository"));
        assert!(!glob_match("*create*", "delete_issue"));
        // Bare star matches everything
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn test_tool_filter_default_allows_all() {
        let filter = ToolFilter::default();
        assert!(filter.permits("call_aws"));
        assert!(filter.permits("delete_everything"));
    }

    #[test]
    fn test_tool_filter_allow_only() {
        let filter = ToolFilter::new(
            Some(vec!["get_*".to_string(), "list_issues".to_string()]),
            vec![],
        );
        assert!(filter.permits("get_issue"));
        assert!(filter.permits("list_issues"));
        // Not in the allow-list — rejected.
        assert!(!filter.permits("create_issue"));
        assert_eq!(
            filter.block_reason("create_issue"),
            Some("not in allow list")
        );
    }

    #[test]
    fn test_tool_filter_empty_allow_blocks_everything() {
        // An explicit `allow = []` is a lockdown: nothing passes.
        let filter = ToolFilter::new(Some(vec![]), vec![]);
        assert!(!filter.permits("get_issue"));
        assert_eq!(filter.block_reason("get_issue"), Some("not in allow list"));
    }

    #[test]
    fn test_tool_filter_deny_only() {
        let filter = ToolFilter::new(None, vec!["*delete*".to_string(), "merge_*".to_string()]);
        // Everything not denied is permitted.
        assert!(filter.permits("get_issue"));
        assert!(filter.permits("create_issue"));
        // Denied by substring / prefix pattern.
        assert!(!filter.permits("delete_task"));
        assert!(!filter.permits("merge_pull_request"));
        assert_eq!(filter.block_reason("delete_task"), Some("deny list"));
    }

    #[test]
    fn test_tool_filter_deny_overrides_allow() {
        // A tool present in BOTH allow and deny is rejected — deny wins,
        // and the reported reason is the deny list.
        let filter = ToolFilter::new(
            Some(vec!["*".to_string(), "push_files".to_string()]),
            vec!["push_files".to_string(), "*delete*".to_string()],
        );
        assert!(filter.permits("get_file_contents"));
        assert!(!filter.permits("push_files"));
        assert!(!filter.permits("delete_issue"));
        assert_eq!(filter.block_reason("push_files"), Some("deny list"));
    }

    #[test]
    fn test_tool_filter_glob_prefix_match() {
        let filter = ToolFilter::new(Some(vec!["jira_get_*".to_string()]), vec![]);
        assert!(filter.permits("jira_get_issue"));
        assert!(filter.permits("jira_get_sprint_issues"));
        assert!(!filter.permits("jira_create_issue"));
    }

    #[test]
    fn test_server_config_tool_filter_from_toml() {
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            allow = ["get_*", "search_*"]
            deny = ["*delete*"]
            "#,
        );
        let server = config.servers.get("gh").unwrap();
        assert_eq!(
            server.allow,
            Some(vec!["get_*".to_string(), "search_*".to_string()])
        );
        assert_eq!(server.deny, vec!["*delete*"]);
        let filter = server.tool_filter();
        assert!(filter.permits("get_issue"));
        assert!(!filter.permits("delete_issue"));
        assert!(!filter.permits("create_issue"));
    }

    #[test]
    fn test_server_config_filter_lists_default_absent() {
        // Existing configs without allow/deny still parse and allow everything.
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            "#,
        );
        let server = config.servers.get("gh").unwrap();
        assert!(server.allow.is_none());
        assert!(server.deny.is_empty());
        assert!(server.tool_filter().permits("anything"));
    }

    #[test]
    fn test_validate_rejects_empty_allow_entry() {
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            allow = ["get_issue", ""]
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, msg)) if slug == "gh" && msg.contains("non-empty"))
        );
    }

    #[test]
    fn test_validate_rejects_empty_deny_entry() {
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            deny = [""]
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(result, Err(PorterError::InvalidConfig(slug, msg)) if slug == "gh" && msg.contains("non-empty"))
        );
    }

    #[test]
    fn test_validate_rejects_interior_star_pattern() {
        // An interior `*` (`delete*confirm`) is treated as a literal by
        // glob_match and would silently match nothing — a fail-open misconfig.
        // validate() must reject it up front.
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            deny = ["delete*confirm"]
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(&result, Err(PorterError::InvalidConfig(slug, msg)) if slug == "gh" && msg.contains("unsupported position")),
            "interior-* pattern should be rejected, got {result:?}"
        );
    }

    #[test]
    fn test_validate_rejects_doubled_leading_star_pattern() {
        // `**abc` has a second star at position 1 — not the single leading
        // position glob_match honors — so it must also be rejected.
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            allow = ["**abc"]
            "#,
        );
        let result = config.validate();
        assert!(
            matches!(&result, Err(PorterError::InvalidConfig(slug, _)) if slug == "gh"),
            "doubled leading-* pattern should be rejected, got {result:?}"
        );
    }

    #[test]
    fn test_validate_accepts_supported_glob_positions() {
        // Every form glob_match honors must pass validation.
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            allow = ["get_issue", "get_*", "*_issue", "*delete*", "*"]
            "#,
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_has_unsupported_glob_star() {
        // Supported forms.
        assert!(!has_unsupported_glob_star("name"));
        assert!(!has_unsupported_glob_star("prefix*"));
        assert!(!has_unsupported_glob_star("*suffix"));
        assert!(!has_unsupported_glob_star("*inner*"));
        assert!(!has_unsupported_glob_star("*"));
        assert!(!has_unsupported_glob_star("**"));
        // Unsupported forms.
        assert!(has_unsupported_glob_star("delete*confirm"));
        assert!(has_unsupported_glob_star("a*b*c"));
        assert!(has_unsupported_glob_star("*a*b*"));
        assert!(has_unsupported_glob_star("**abc"));
    }

    #[test]
    fn test_validate_accepts_allow_deny_overlap() {
        // A tool in both lists is legal (deny wins, warning emitted) — validate
        // must not reject it.
        let config = parse_toml(
            r#"
            [servers.gh]
            slug = "gh"
            transport = "stdio"
            command = "gh-mcp"
            allow = ["get_issue", "push_files"]
            deny = ["push_files"]
            "#,
        );
        assert!(config.validate().is_ok());
        assert!(
            !config
                .servers
                .get("gh")
                .unwrap()
                .tool_filter()
                .permits("push_files")
        );
    }
}
