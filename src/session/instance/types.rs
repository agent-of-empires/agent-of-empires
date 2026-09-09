//! Serialized value types carried on an `Instance` row.

use super::*;

pub(super) fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

pub(super) fn is_zero_u8(value: &u8) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalInfo {
    #[serde(default)]
    pub created: bool,
}

/// How a session is rendered. `Structured` uses the ACP-based native
/// rendering (plan panels, tool-call cards, approvals); `Terminal` streams
/// the raw tmux/PTY through xterm.js. `Terminal` is the conservative
/// deserialization default; session creation sets the value explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Terminal,
    Structured,
}

impl View {
    /// `skip_serializing_if` predicate: only the non-default `Structured`
    /// value is persisted, mirroring the old `structured_view` bool shape.
    pub fn is_terminal(&self) -> bool {
        matches!(self, View::Terminal)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub branch: String,
    pub main_repo_path: String,
    pub managed_by_aoe: bool,
    pub created_at: DateTime<Utc>,
    /// Branch the worktree was created from when `managed_by_aoe` is
    /// true. None means "the repo's default branch was used" (the
    /// historical behavior before #948) or the worktree was attached
    /// to a pre-existing branch (`create_branch = false`). Surfaced
    /// in `aoe list --json`, the TUI preview, and the web sessions
    /// API; not used by core logic, so old `sessions.json` files
    /// deserialize without the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRepo {
    pub name: String,
    pub source_path: String,
    pub branch: String,
    pub worktree_path: String,
    pub main_repo_path: String,
    pub managed_by_aoe: bool,
    /// True when `branch` already existed in this repo and aoe merely checked it
    /// out, which makes branch deletion on session delete a no-op.
    ///
    /// Only ever set by `attach_project` with `--attach-existing-branch` (#3103):
    /// the workspace builder always creates the branch it names, so branch and
    /// worktree ownership coincide for a repo present at creation. Phrased as
    /// "pre-existing" rather than "aoe created it" so the serde default is
    /// correct for every record written before the field existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub branch_preexisting: bool,
    /// Branch this repo's worktree branch was forked from, recorded at
    /// creation. The per-repo counterpart of [`WorktreeInfo::base_branch`],
    /// and the reason a workspace member's diff can default to the right
    /// ref: workspace sessions leave `worktree_info` unset, so before this
    /// field existed there was nothing per-repo to fall back to (#3329).
    ///
    /// Only set when aoe actually created the branch from that base. A repo
    /// attached to a pre-existing branch records None, so "reset to default"
    /// never compares against a ref that was not the checkout's base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Explicit diff-base override for this repo alone, set by the web
    /// diff picker or `aoe session set-base --repo <name>`. Wins over
    /// `base_branch`. `Instance::base_branch_override` does NOT apply to a
    /// workspace member; that field covers a single-repo session's own
    /// checkout. See #3329.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch_override: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub branch: String,
    pub workspace_dir: String,
    pub repos: Vec<WorkspaceRepo>,
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_true")]
    pub cleanup_on_delete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SandboxStoreTransitionPath {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    pub image: String,
    pub container_name: String,
    /// Additional environment entries (session-specific).
    /// `KEY` = pass through from host, `KEY=VALUE` = set explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_env: Option<Vec<String>>,
    /// Custom instruction text to inject into agent launch command
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instruction: Option<String>,
    /// The container's working directory, captured from
    /// `ContainerConfig::working_dir` when the container is created (and
    /// backfilled from a live container for sessions created before this field
    /// existed). [`Instance::container_workdir`] returns this verbatim so every
    /// `docker exec -w` targets the path the container was actually built with,
    /// instead of a live recomputation that can drift once the host worktree's
    /// git linkage breaks (#2414).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_workdir: Option<String>,
    /// `KEY=VALUE` pairs minted on the host by `host_hooks.before_start` when
    /// the container last came up. Injected into the container environment as
    /// inherited (leak-safe) entries by `crate::session::environment::collect_environment`.
    ///
    /// Runtime-only and secret: never serialized (so short-lived tokens never
    /// hit disk and a stale value never survives a restart) and re-minted on the
    /// next container come-up. See `Instance::ensure_before_start_env`.
    #[serde(skip)]
    pub before_start_env: Vec<(String, String)>,
}

/// Deserialize agent_session_id, treating empty/whitespace strings as None.
pub(super) fn deserialize_session_id<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.filter(|s| !s.trim().is_empty()))
}

/// The session ids one agent left behind when an engine swap moved a row to a
/// different `tool`, parked in `Instance::prior_tool_session_ids` under that
/// agent's name so a swap back can resume where it left off. Both fields are
/// per-agent namespaces, which is exactly why they cannot travel with the row.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PriorToolSession {
    /// The tmux-path conversation id, as `Instance::agent_session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent_session_id: Option<String>,
    /// The structured-view conversation id, as `Instance::acp_session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) acp_session_id: Option<String>,
}

impl PriorToolSession {
    /// Nothing worth parking: an agent that never got a conversation id (never
    /// launched, or `/clear`ed) leaves no entry behind.
    pub(super) fn is_empty(&self) -> bool {
        self.agent_session_id.is_none() && self.acp_session_id.is_none()
    }
}

/// User intent gating `acquire_session_id`, persisted independently of the
/// poller's observation in `agent_session_id`. CLI/REST/TUI write intent;
/// the poller writes observation. Disjoint writers, no race.
///
/// `#[serde(rename)]` pins wire names so a Rust-side variant rename
/// cannot silently break existing `sessions.json` deserialisation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value")]
pub(crate) enum ResumeIntent {
    /// Fall back to the poller's observed `agent_session_id`.
    #[default]
    #[serde(rename = "Default")]
    Default,
    /// Pin to this sid: pass `--resume <sid>` regardless of observation.
    #[serde(rename = "Use")]
    Use(String),
    /// Force a fresh start on the next launch. Auto-promotes to `Default`
    /// after the launch completes (one-shot semantics).
    #[serde(rename = "Cleared")]
    Cleared,
    /// One-shot fork seed: on the next (first) launch, resume `from` and fork
    /// into a NEW session whose id was pre-pinned in `agent_session_id`.
    /// Auto-promotes to `Default` after that launch, exactly like `Cleared`,
    /// so later restarts resume the child's own id with a plain `--resume`.
    #[serde(rename = "Fork")]
    Fork { from: String },
}

impl ResumeIntent {
    pub(super) fn is_default(&self) -> bool {
        matches!(self, ResumeIntent::Default)
    }
}

/// Create-idempotency record for a plugin-created session (#2897). `key` is
/// the plugin-supplied idempotency key, unique within the creating plugin's
/// sessions; `payload_hash` is the host-computed hash of the semantic create
/// request, so a retried key with a different payload is rejected instead of
/// silently returning a session that does not match the request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginCreateIdempotency {
    pub key: String,
    pub payload_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugin_meta_serde_round_trip() {
        // Empty map is omitted from disk.
        let inst = Instance::new("t", "/tmp");
        let json = serde_json::to_value(&inst).unwrap();
        assert!(
            json.get("plugin_meta").is_none(),
            "empty plugin_meta must skip serialization"
        );

        // A plugin's namespaced slot round-trips.
        let mut set = Instance::new("t", "/tmp");
        set.plugin_meta
            .insert("aoe.status".to_string(), serde_json::json!({ "score": 3 }));
        let json = serde_json::to_value(&set).unwrap();
        let back: Instance = serde_json::from_value(json).unwrap();
        assert_eq!(back.plugin_meta["aoe.status"]["score"], 3);

        // Rows written before the field existed deserialize to an empty map.
        let inst: Instance = serde_json::from_value(serde_json::json!({
            "id": "abc",
            "title": "t",
            "project_path": "/tmp",
            "tool": "claude",
            "status": "idle",
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("deserialize without plugin_meta");
        assert!(inst.plugin_meta.is_empty());
    }

    // A non-fork session omits fork_pending on the wire (skip_serializing_if),
    // so legacy sessions.json without the key deserializes to None and no
    // migration is needed. A seeded fork id round-trips.
    #[test]
    fn test_fork_pending_serde_roundtrip_and_default() {
        let fresh = Instance::new("s", "/tmp/x");
        let fresh_json = serde_json::to_string(&fresh).expect("serialize fresh");
        assert!(
            !fresh_json.contains("fork_pending"),
            "None fork_pending must not be serialized"
        );
        let parsed: Instance = serde_json::from_str(&fresh_json).expect("parse fresh");
        assert_eq!(parsed.fork_pending, None, "missing fork_pending => None");

        let mut inst = Instance::new("s", "/tmp/x");
        inst.fork_pending = Some("parent-acp-id".into());
        let json = serde_json::to_string(&inst).expect("serialize");
        let back: Instance = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back.fork_pending.as_deref(), Some("parent-acp-id"));
    }

    // Tests for WorktreeInfo
    #[test]
    fn test_worktree_info_serialization() {
        let info = WorktreeInfo {
            branch: "feature/test".to_string(),
            main_repo_path: "/home/user/repo".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        };

        let json = serde_json::to_string(&info).unwrap();
        let deserialized: WorktreeInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(info.branch, deserialized.branch);
        assert_eq!(info.main_repo_path, deserialized.main_repo_path);
        assert_eq!(info.managed_by_aoe, deserialized.managed_by_aoe);
    }

    // Tests for SandboxInfo
    #[test]
    fn test_sandbox_info_serialization() {
        let info = SandboxInfo {
            enabled: true,
            container_id: Some("abc123".to_string()),
            image: "myimage:latest".to_string(),
            container_name: "test_container".to_string(),
            extra_env: Some(vec!["MY_VAR".to_string(), "OTHER_VAR".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let json = serde_json::to_string(&info).unwrap();
        let deserialized: SandboxInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(info.enabled, deserialized.enabled);
        assert_eq!(info.container_id, deserialized.container_id);
        assert_eq!(info.image, deserialized.image);
        assert_eq!(info.container_name, deserialized.container_name);
        assert_eq!(info.extra_env, deserialized.extra_env);
    }

    #[test]
    fn test_sandbox_info_minimal_serialization() {
        // Required fields: enabled, image, container_name
        let json = r#"{"enabled":false,"image":"test-image","container_name":"test"}"#;
        let info: SandboxInfo = serde_json::from_str(json).unwrap();

        assert!(!info.enabled);
        assert_eq!(info.image, "test-image");
        assert_eq!(info.container_name, "test");
        assert!(info.container_id.is_none());
    }

    #[test]
    fn test_empty_string_deserializes_to_none() {
        let json = r#"{"id":"test123","title":"Test","project_path":"/tmp/test","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z","agent_session_id":""}"#;
        let inst: Instance = serde_json::from_str(json).unwrap();
        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_whitespace_string_deserializes_to_none() {
        let json = r#"{"id":"test123","title":"Test","project_path":"/tmp/test","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z","agent_session_id":"   "}"#;
        let inst: Instance = serde_json::from_str(json).unwrap();
        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_valid_session_id_preserved() {
        let json = r#"{"id":"test123","title":"Test","project_path":"/tmp/test","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z","agent_session_id":"abc-123"}"#;
        let inst: Instance = serde_json::from_str(json).unwrap();
        assert_eq!(inst.agent_session_id, Some("abc-123".to_string()));
    }

    #[test]
    fn resume_intent_serde_round_trip() {
        for intent in [
            ResumeIntent::Default,
            ResumeIntent::Use("abc".to_string()),
            ResumeIntent::Cleared,
            ResumeIntent::Fork {
                from: "some-parent-id".to_string(),
            },
        ] {
            let json = serde_json::to_string(&intent).unwrap();
            let back: ResumeIntent = serde_json::from_str(&json).unwrap();
            assert_eq!(intent, back);
        }
    }

    #[test]
    fn resume_intent_wire_format_is_pinned() {
        assert_eq!(
            serde_json::to_string(&ResumeIntent::Default).unwrap(),
            r#"{"kind":"Default"}"#
        );
        assert_eq!(
            serde_json::to_string(&ResumeIntent::Use("abc".to_string())).unwrap(),
            r#"{"kind":"Use","value":"abc"}"#
        );
        assert_eq!(
            serde_json::to_string(&ResumeIntent::Cleared).unwrap(),
            r#"{"kind":"Cleared"}"#
        );
        // `Fork` is a struct variant, so its `value` is a nested object
        // (`{"from":...}`), not a bare string like `Use`. This shape is
        // persisted to `sessions.json`; pin it so a refactor cannot break
        // deserialisation of saved fork seeds.
        assert_eq!(
            serde_json::to_string(&ResumeIntent::Fork {
                from: "some-parent-id".to_string()
            })
            .unwrap(),
            r#"{"kind":"Fork","value":{"from":"some-parent-id"}}"#
        );
    }

    #[test]
    fn resume_intent_missing_in_json_defaults_to_default() {
        let mut inst = Instance::new("title", "/tmp/x");
        inst.resume_intent = ResumeIntent::Use("X".to_string());
        let json: serde_json::Value = serde_json::to_value(&inst).unwrap();
        let mut obj = json.as_object().unwrap().clone();
        obj.remove("resume_intent");
        let stripped = serde_json::Value::Object(obj);

        let back: Instance = serde_json::from_value(stripped).unwrap();
        assert_eq!(back.resume_intent, ResumeIntent::Default);
    }
}

/// Where a Pi pane publishes its conversation. Sandboxed panes write into the
/// config bind; host panes into the per-instance hook directory. Kept distinct
/// so an unresolvable sandbox path cannot read as "use the host one".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PiSidecarSource {
    HostHooks,
    SandboxDir(std::path::PathBuf),
}
