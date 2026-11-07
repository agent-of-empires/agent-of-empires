//! MCP server naming rules and limits for plugin-served servers.

pub const MCP_SERVE_CAPABILITY: &str = "mcp.serve";
pub const MCP_SERVE_MAX_PER_PLUGIN: usize = 64;

const MAX_NAME_LEN: usize = 128;

/// Whether `name` is a valid `mcpServers` key: 1-128 chars of letters, digits,
/// `.`, `_` or `-`, starting with a letter or digit.
pub fn is_valid_mcp_server_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= MAX_NAME_LEN
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The plugin id with every char that is not a letter, digit, `_` or `-`
/// replaced by `_`, for namespacing the plugin's server names.
pub fn mcp_prefix(plugin_id: &str) -> String {
    plugin_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_serve_name_predicate() {
        let long = "a".repeat(129);
        for (name, ok) in [
            ("gate", true),
            ("a.b_c-d", true),
            ("9lives", true),
            ("", false),
            ("a:b", false),
            ("a/b", false),
            ("..", false),
            ("a b", false),
            (".hidden", false),
            ("-x", false),
            (long.as_str(), false),
        ] {
            assert_eq!(is_valid_mcp_server_name(name), ok, "{name:?}");
        }
        assert!(is_valid_mcp_server_name(&"a".repeat(128)));
    }

    #[test]
    fn mcp_serve_prefix_replaces_invalid_chars() {
        assert_eq!(mcp_prefix("acme.gatekeeper"), "acme_gatekeeper");
        assert_eq!(mcp_prefix("a-b_c"), "a-b_c");
        assert_eq!(mcp_prefix("a.b:c d"), "a_b_c_d");
    }
}
