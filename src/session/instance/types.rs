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

/// How a session is rendered: ACP-native `Structured` or raw tmux `Terminal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Terminal,
    Structured,
}

impl View {
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
    /// Branch an AoE-managed worktree was created from; `None` for the default
    /// branch or an attached pre-existing branch.
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
    /// The branch already existed and was only checked out, so deleting the
    /// session must not delete it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub branch_preexisting: bool,
    /// Per-repo counterpart of [`WorktreeInfo::base_branch`]; set only when AoE
    /// created the branch from that base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Diff-base override for this repo alone; wins over `base_branch`.
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
    /// `KEY` passes through from the host; `KEY=VALUE` sets explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instruction: Option<String>,
    /// Working directory the container was built with; recomputing it can drift (#2414).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_workdir: Option<String>,
    /// Values minted by `host_hooks.before_start`; secret, so never serialized.
    #[serde(skip)]
    pub before_start_env: Vec<(String, String)>,
}

/// Blank session ids deserialize as `None`.
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
    pub(super) fn is_empty(&self) -> bool {
        self.agent_session_id.is_none() && self.acp_session_id.is_none()
    }
}

/// User intent gating `acquire_session_id`, written separately from the poller's
/// observed `agent_session_id`. Wire names are pinned by `#[serde(rename)]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value")]
pub(crate) enum ResumeIntent {
    #[default]
    #[serde(rename = "Default")]
    Default,
    #[serde(rename = "Use")]
    Use(String),
    /// One-shot fresh start; promotes to `Default` after the launch.
    #[serde(rename = "Cleared")]
    Cleared,
    /// One-shot fork of `from` into the child id pre-pinned in `agent_session_id`.
    #[serde(rename = "Fork")]
    Fork { from: String },
}

impl ResumeIntent {
    pub(super) fn is_default(&self) -> bool {
        matches!(self, ResumeIntent::Default)
    }
}

/// Plugin create-idempotency record; a retried key with a different
/// `payload_hash` is rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginCreateIdempotency {
    pub key: String,
    pub payload_hash: String,
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
