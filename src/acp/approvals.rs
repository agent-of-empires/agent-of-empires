//! Server-side approval nonces.

use chrono::{DateTime, Utc};
use rand::TryRng;
use serde::{Deserialize, Serialize};

use super::state::ToolCall;

const NONCE_BYTES: usize = 16;

/// Server-generated single-use token for approval round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Nonce(pub String);

impl Default for Nonce {
    fn default() -> Self {
        Self::new()
    }
}

impl Nonce {
    pub fn new() -> Self {
        let mut buf = [0u8; NONCE_BYTES];
        rand::rng()
            .try_fill_bytes(&mut buf)
            .expect("OS rng should not fail for 16 bytes");
        Self(hex::encode_lower(&buf))
    }
}

/// Tiny hex helper local to this module to avoid pulling in a crate just for
/// nonce display.
mod hex {
    pub fn encode_lower(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xF) as usize] as char);
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    /// One-time allow, equivalent to ACP `allow_once`.
    Allow,
    /// Allow now and add a permission rule, equivalent to ACP `allow_always`.
    AllowAlways,
    /// Deny, equivalent to ACP `reject`.
    Deny,
    /// Approval was not resolved by the user, e.g. the daemon was
    /// restarted while the approval card was on screen.
    Cancelled,
}

/// One option the agent offered on `session/request_permission`, kept in
/// the order it sent them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalOption {
    pub option_id: String,
    pub name: String,
    pub kind: ApprovalOptionKind,
}

/// Mirror of ACP `PermissionOptionKind`, owned here so the wire type the
/// clients see does not depend on the protocol crate's enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// True when the offered options are a list of answers rather than an
/// allow/deny vocabulary, so clients must render the labels and send back
/// the picked `option_id`.
pub fn is_choice_list(options: &[ApprovalOption]) -> bool {
    options.len() > 1 && options.iter().all(|o| o.kind == options[0].kind)
}

/// A pending or resolved approval for a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub nonce: Nonce,
    pub tool_call: ToolCall,
    /// True for tools the structured view considers destructive (rm -rf,
    /// `git push --force`, etc.).
    pub destructive: bool,
    /// Options the agent offered.
    #[serde(default)]
    pub options: Vec<ApprovalOption>,
    /// `is_choice_list(&options)`, resolved server-side so the TUI and the
    /// web dashboard cannot disagree about which cards are answer lists.
    #[serde(default)]
    pub choice: bool,
    pub requested_at: DateTime<Utc>,
    pub resolved: Option<ResolvedApproval>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedApproval {
    pub decision: ApprovalDecision,
    pub message: Option<String>,
    pub resolved_at: DateTime<Utc>,
}

/// Heuristic for "this tool call is destructive enough that mobile UI
/// should require long-press confirmation."
pub fn is_destructive(tool_name: &str, args_preview: &str) -> bool {
    let lower_args = args_preview.to_lowercase();
    match tool_name {
        "Bash" | "Terminal" => {
            lower_args.contains("rm -rf")
                || lower_args.contains("rm -fr")
                || lower_args.contains("git push --force")
                || lower_args.contains("git push -f ")
                || lower_args.contains("drop table")
                || lower_args.contains("truncate ")
                || lower_args.contains("> /dev/sda")
        }
        "Write" | "Edit" => {
            // Writes outside the worktree get treated as destructive at the
            // approval layer; the actual sandbox check is in fs_handler.
            lower_args.contains("/etc/")
                || lower_args.contains("/usr/")
                || lower_args.contains("/system/")
                || lower_args.contains("$home/.ssh")
        }
        _ => false,
    }
}

pub(crate) const PATH_KEYS: &[&str] = &["path", "file_path", "filePath", "filename"];
pub(crate) const CMD_KEYS: &[&str] = &["command", "cmd", "args"];

pub(crate) enum ToolTarget<'a> {
    Path(&'a str),
    Command(&'a str),
}

/// Primary target shared by approval and tool summaries.
pub(crate) fn tool_target<'a>(
    kind: &str,
    args: &'a serde_json::Map<String, serde_json::Value>,
) -> Option<ToolTarget<'a>> {
    let pick = |keys: &[&str]| keys.iter().find_map(|key| args.get(*key)?.as_str());
    match kind {
        "edit" | "write" | "read" | "delete" | "move" => pick(PATH_KEYS).map(ToolTarget::Path),
        "execute" => pick(CMD_KEYS)
            .and_then(|command| command.lines().next())
            .map(ToolTarget::Command),
        _ => None,
    }
}

/// Raw path or first command line for the home approval projection.
pub fn summarize_target(kind: &str, args_preview: &str) -> String {
    let Ok(serde_json::Value::Object(args)) = serde_json::from_str(args_preview) else {
        return String::new();
    };
    match tool_target(kind, &args) {
        Some(ToolTarget::Path(value) | ToolTarget::Command(value)) => value.to_owned(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_list_needs_every_option_to_share_one_kind() {
        let option = |id: &str, kind| ApprovalOption {
            option_id: id.into(),
            name: id.into(),
            kind,
        };
        let cases = [
            ("empty", vec![], false),
            (
                "single option",
                vec![option("once", ApprovalOptionKind::AllowOnce)],
                false,
            ),
            (
                "permission vocabulary",
                vec![
                    option("once", ApprovalOptionKind::AllowOnce),
                    option("always", ApprovalOptionKind::AllowAlways),
                    option("no", ApprovalOptionKind::RejectOnce),
                ],
                false,
            ),
            (
                "gemini mcp",
                vec![
                    option("proceed_always_server", ApprovalOptionKind::AllowAlways),
                    option("proceed_always_tool", ApprovalOptionKind::AllowAlways),
                    option("proceed_once", ApprovalOptionKind::AllowOnce),
                    option("cancel", ApprovalOptionKind::RejectOnce),
                ],
                false,
            ),
            (
                "pi confirm",
                vec![
                    option("yes", ApprovalOptionKind::AllowOnce),
                    option("no", ApprovalOptionKind::RejectOnce),
                ],
                false,
            ),
            (
                "pi two-option question",
                vec![
                    option("choice-0", ApprovalOptionKind::AllowOnce),
                    option("choice-1", ApprovalOptionKind::AllowOnce),
                ],
                true,
            ),
            (
                "pi four-option question",
                vec![
                    option("choice-0", ApprovalOptionKind::AllowOnce),
                    option("choice-1", ApprovalOptionKind::AllowOnce),
                    option("choice-2", ApprovalOptionKind::AllowOnce),
                    option("choice-3", ApprovalOptionKind::AllowOnce),
                ],
                true,
            ),
        ];
        for (name, options, expected) in cases {
            assert_eq!(is_choice_list(&options), expected, "{name}");
        }
    }

    #[test]
    fn nonce_heuristics_target_summary_and_legacy_decode() {
        assert!(is_destructive("Bash", r#"{"command":"rm -rf /tmp/foo"}"#));
        assert!(is_destructive(
            "Bash",
            r#"{"command":"git push --force origin main"}"#
        ));
        assert!(!is_destructive("Bash", r#"{"command":"ls -la"}"#));
        assert!(is_destructive("Write", r#"{"path":"/etc/hosts"}"#));
        assert!(!is_destructive("Read", r#"{"path":"/etc/hosts"}"#));

        let cases = [
            // kind, args_preview, expected
            ("read", r#"{"path":"src/foo.rs"}"#, "src/foo.rs"),
            ("edit", r#"{"file_path":"a/b.rs"}"#, "a/b.rs"),
            ("write", r#"{"filePath":"c.txt"}"#, "c.txt"),
            // execute keeps only the first command line
            ("execute", "{\"command\":\"ls -la\\nrm x\"}", "ls -la"),
            // unknown kind and non-object args yield no target
            ("think", r#"{"path":"x"}"#, ""),
            ("read", "not json", ""),
            ("read", "{}", ""),
        ];
        for (kind, args, expected) in cases {
            assert_eq!(summarize_target(kind, args), expected, "{kind}/{args}");
        }

        let a = Nonce::new();
        let b = Nonce::new();
        assert_ne!(a, b);
        assert_eq!(a.0.len(), NONCE_BYTES * 2);
        assert!(a.0.chars().all(|c| c.is_ascii_hexdigit()));

        // Event logs written before options existed still decode (#3779).
        let legacy = serde_json::json!({
            "nonce": "abc",
            "tool_call": {
                "id": "tc",
                "name": "Bash",
                "kind": "execute",
                "args_preview": "{}",
                "started_at": "2026-01-01T00:00:00Z",
            },
            "destructive": false,
            "requested_at": "2026-01-01T00:00:00Z",
            "resolved": null,
        });
        let approval: Approval = serde_json::from_value(legacy).expect("legacy approval");
        assert!(approval.options.is_empty());
        assert!(!approval.choice);
    }
}
