//! Tool namespacing utilities for Porter.
//!
//! Prefixes tool names with the server slug using double underscore separator
//! (e.g., `gh__list_repos`) and prepends `[via slug]` to descriptions.

use rmcp::model::Tool;

/// Prefix a tool name with the server slug using double underscore separator.
/// E.g., slug="gh", tool="list_repos" -> "gh__list_repos"
///
/// Description is prepended with "[via slug]" so consuming LLMs understand
/// the proxy relationship and tool origin.
pub fn namespace_tool(slug: &str, mut tool: Tool) -> Tool {
    let new_name = format!("{}__{}", slug, tool.name);
    tool.name = new_name.into();
    if let Some(desc) = tool.description.as_mut() {
        let prefixed = format!("[via {}] {}", slug, desc);
        *desc = prefixed.into();
    }
    tool
}

/// Extract (slug, original_tool_name) from a namespaced tool name.
/// Returns None if no double underscore separator found.
pub fn unnamespace_tool_name(namespaced: &str) -> Option<(&str, &str)> {
    namespaced.split_once("__")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Tool;
    use serde_json::json;
    use std::sync::Arc;

    fn make_tool(name: &str, description: Option<&str>) -> Tool {
        let schema = Arc::new(
            json!({"type": "object", "properties": {}})
                .as_object()
                .unwrap()
                .clone(),
        );
        Tool {
            name: name.to_string().into(),
            title: None,
            description: description.map(|d| d.to_string().into()),
            input_schema: schema,
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        }
    }

    #[test]
    fn test_namespace_tool_name() {
        let tool = make_tool("list_repos", Some("List repositories"));
        let namespaced = namespace_tool("gh", tool);
        assert_eq!(namespaced.name.as_ref(), "gh__list_repos");
    }

    #[test]
    fn test_namespace_tool_description() {
        let tool = make_tool("list_repos", Some("List repositories"));
        let namespaced = namespace_tool("gh", tool);
        assert_eq!(
            namespaced.description.as_deref(),
            Some("[via gh] List repositories")
        );
    }

    #[test]
    fn test_unnamespace_roundtrip() {
        let tool = make_tool("list_repos", Some("List repositories"));
        let namespaced = namespace_tool("gh", tool);
        let (slug, name) = unnamespace_tool_name(namespaced.name.as_ref()).unwrap();
        assert_eq!(slug, "gh");
        assert_eq!(name, "list_repos");
    }

    #[test]
    fn test_unnamespace_no_separator() {
        assert!(unnamespace_tool_name("list_repos").is_none());
    }

    #[test]
    fn test_namespace_no_description() {
        let tool = make_tool("list_repos", None);
        let namespaced = namespace_tool("gh", tool);
        assert_eq!(namespaced.name.as_ref(), "gh__list_repos");
        assert!(namespaced.description.is_none());
    }
}
