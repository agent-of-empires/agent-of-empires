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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuxiliaryTarget {
    Host { index: u32 },
    Container { index: u32 },
    Tool { tool_name: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanePresence {
    Absent,
    Alive,
    Dead,
    #[default]
    Unknown,
}

/// Native handoff requires Alive and the same name as the preparation receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneObservation {
    #[serde(default)]
    pub state: PanePresence,
    #[serde(default)]
    pub tmux_session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuxiliaryObservation {
    pub target: AuxiliaryTarget,
    #[serde(flatten)]
    pub pane: PaneObservation,
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

/// Session ids parked by an engine swap so swapping back resumes the old conversation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PriorToolSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent_session_binding: Option<ConversationBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pi_session_path: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PrimeAgentCapturePlan {
    pub(crate) store: PathBuf,
    pub(crate) session_dir: PathBuf,
    pub(crate) container_session_dir: PathBuf,
    pub(crate) container_cwd: String,
}

/// The exact directory from which a pane publishes its conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SessionSidecarSource {
    HostHooks(PathBuf),
    SandboxDir(PathBuf),
}

impl SessionSidecarSource {
    pub(crate) fn read_file(
        &self,
        instance_id: &str,
        leaf: &str,
        cap: usize,
        max_age: Option<std::time::Duration>,
    ) -> Option<Vec<u8>> {
        crate::session::validate_instance_id(instance_id).ok()?;
        match self {
            Self::HostHooks(directory) => {
                crate::hooks::read_hook_sidecar_at(instance_id, directory, leaf, cap, max_age)
            }
            Self::SandboxDir(directory) => {
                let root = directory.parent()?.parent()?;
                if root.join("aoe-session").join(instance_id) != *directory {
                    return None;
                }
                crate::session::AnchoredDir::open(root)
                    .ok()?
                    .read_regular(&Path::new("aoe-session").join(instance_id).join(leaf), cap)
                    .ok()?
            }
        }
    }

    pub(crate) fn host_hooks(instance_id: &str) -> Self {
        let path = crate::hooks::hook_base_path().join(instance_id);
        Self::HostHooks(path.canonicalize().unwrap_or(path))
    }

    pub(crate) fn matches_host_hooks(&self, instance_id: &str) -> bool {
        *self == Self::host_hooks(instance_id)
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
    fn blank_agent_session_id_deserializes_to_none() {
        for (raw, expected) in [("", None), ("   ", None), ("abc-123", Some("abc-123"))] {
            let inst: Instance = serde_json::from_value(serde_json::json!({
                "id": "test123", "title": "Test", "project_path": "/tmp/test",
                "tool": "claude", "status": "idle", "created_at": "2024-01-01T00:00:00Z",
                "agent_session_id": raw,
            }))
            .unwrap();
            assert_eq!(inst.agent_session_id.as_deref(), expected, "{raw:?}");
        }
    }

    #[test]
    fn resume_intent_wire_format_is_pinned() {
        for (intent, wire) in [
            (ResumeIntent::Default, r#"{"kind":"Default"}"#),
            (
                ResumeIntent::Use("abc".to_string()),
                r#"{"kind":"Use","value":"abc"}"#,
            ),
            (ResumeIntent::Cleared, r#"{"kind":"Cleared"}"#),
            (
                ResumeIntent::Fork {
                    from: "some-parent-id".to_string(),
                },
                r#"{"kind":"Fork","value":{"from":"some-parent-id"}}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&intent).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ResumeIntent>(wire).unwrap(), intent);
        }
    }
}
