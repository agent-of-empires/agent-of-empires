//! Values shared by REST responses and the root crate's persisted models.

use std::collections::BTreeMap;

use serde::{de, Deserialize, Serialize};

use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuxiliaryTarget {
    Host { index: u32 },
    Container { index: u32 },
    Tool { tool_name: String },
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PanePresence {
    Absent,
    Alive,
    Dead,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LegacyToolIdentity {
    pub session_id: String,
    pub pane_id: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub pane_pid: Option<u32>,
}

impl std::fmt::Display for LegacyToolIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} / {}, ", self.session_id, self.pane_id)?;
        match self.pane_pid {
            Some(pid) => write!(formatter, "PID {pid}"),
            None => formatter.write_str("leader exited"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TmuxServerIdentity {
    pub socket: String,
    pub pid: u32,
    pub birth: ProcessBirthIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LegacyToolObservation {
    pub identity: LegacyToolIdentity,
    pub server: std::sync::Arc<TmuxServerIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyToolAdoption {
    pub tmux_session: String,
    pub identity: LegacyToolIdentity,
    pub server: std::sync::Arc<TmuxServerIdentity>,
    pub profile: String,
    pub lifecycle_generation: u64,
}

/// Native handoff requires Alive without a pending ownership adoption.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneObservation {
    #[serde(default)]
    pub state: PanePresence,
    #[serde(default)]
    pub tmux_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_tool: Option<LegacyToolObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AuxiliaryObservation {
    pub target: AuxiliaryTarget,
    #[serde(flatten)]
    pub pane: PaneObservation,
}

/// ACP-native structured view or raw tmux terminal.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
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

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ProcessBirthIdentity {
    pub boot_id: String,
    pub start_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleOperation {
    Launch,
    Capture,
    Stop,
    Purge,
    Restore,
    Trash,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct LifecycleReservation {
    pub op: LifecycleOperation,
    pub generation: u64,
    pub at: Timestamp,
}

/// What the caller asked to keep or remove; the daemon fills the rest from config.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CleanupIntent {
    #[serde(default)]
    pub delete_worktree: bool,
    #[serde(default)]
    pub delete_branch: bool,
    #[serde(default)]
    pub keep_container: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectScope {
    Global,
    Profile,
}

impl ProjectScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectScope::Global => "global",
            ProjectScope::Profile => "profile",
        }
    }
}

/// Per-project overrides for otherwise-global settings. Every field is
/// `None` when the project doesn't override that setting, so resolution
/// falls through to the global/profile default. Add a field here to make
/// a new global toggle project-overridable; existing call sites that
/// resolve overrides (`find_by_canonical_path`, `resolve_smart_rename_config`)
/// don't need to change shape, only the new call site that consults the
/// new field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectOverrides {
    /// Overrides `worktree.enabled` (create-worktree-by-default) for new
    /// sessions launched against this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_enabled: Option<bool>,
    /// Overrides `session.smart_rename` (agent-driven auto-naming) for
    /// sessions launched against this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_rename: Option<bool>,
    /// Overrides `sandbox.enabled_by_default` (start in container by default)
    /// for new sessions launched against this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_enabled: Option<bool>,
    /// Plugin settings declared `per_project`, keyed by plugin id then setting key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_settings: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

impl ProjectOverrides {
    pub fn is_empty(&self) -> bool {
        self.worktree_enabled.is_none()
            && self.smart_rename.is_none()
            && self.sandbox_enabled.is_none()
            && self.plugin_settings.is_empty()
    }

    /// Set (`Some`) or clear (`None`) one plugin setting, dropping emptied plugin entries.
    pub fn set_plugin_setting(
        &mut self,
        plugin_id: &str,
        key: &str,
        value: Option<serde_json::Value>,
    ) {
        match value {
            Some(value) => {
                self.plugin_settings
                    .entry(plugin_id.to_string())
                    .or_default()
                    .insert(key.to_string(), value);
            }
            None => {
                if let Some(settings) = self.plugin_settings.get_mut(plugin_id) {
                    settings.remove(key);
                    if settings.is_empty() {
                        self.plugin_settings.remove(plugin_id);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct ProjectPatch {
    /// Absent preserves the value; present None clears it.
    pub base_branch: Option<Option<String>>,
    pub pinned: Option<bool>,
    /// Absent preserves the override; present None restores inheritance.
    pub worktree_enabled: Option<Option<bool>>,
    pub smart_rename: Option<Option<bool>>,
    pub sandbox_enabled: Option<Option<bool>>,
    pub plugin_settings: BTreeMap<String, BTreeMap<String, Option<serde_json::Value>>>,
}

impl Serialize for ProjectPatch {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Overrides<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            worktree_enabled: Option<&'a Option<bool>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            smart_rename: Option<&'a Option<bool>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            sandbox_enabled: Option<&'a Option<bool>>,
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            plugin_settings: &'a BTreeMap<String, BTreeMap<String, Option<serde_json::Value>>>,
        }
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            default_base_branch: Option<&'a Option<String>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pinned: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            overrides: Option<Overrides<'a>>,
        }
        let overrides = (self.worktree_enabled.is_some()
            || self.smart_rename.is_some()
            || self.sandbox_enabled.is_some()
            || !self.plugin_settings.is_empty())
        .then_some(Overrides {
            worktree_enabled: self.worktree_enabled.as_ref(),
            smart_rename: self.smart_rename.as_ref(),
            sandbox_enabled: self.sandbox_enabled.as_ref(),
            plugin_settings: &self.plugin_settings,
        });
        Body {
            default_base_branch: self.base_branch.as_ref(),
            pinned: self.pinned,
            overrides,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<Timestamp>,
    #[serde(skip)]
    pub children: Vec<Group>,
}

impl Group {
    pub fn new(name: &str, path: &str) -> Self {
        Self {
            name: name.to_string(),
            path: path.to_string(),
            collapsed: false,
            archived_at: None,
            children: Vec::new(),
        }
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

/// Per-session smart-rename state surfaced to the dashboard so the sidebar can show that a session
/// will be (or is being) auto-named.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum SmartRenameState {
    #[default]
    Inactive,
    /// Eligible and still default-named: will auto-name on the next prompt.
    Pending,
    /// A one-shot title call is in flight for this session right now.
    Running,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RateLimitInfo {
    pub status: String,
    /// When the quota window clears, if the agent reported it.
    pub resets_at: Option<Timestamp>,
    pub kind: String,
}

impl RateLimitInfo {
    /// A park whose reporting `RateLimit` event was pruned: a limit with no reset time.
    pub fn undated() -> Self {
        Self {
            status: "limited".into(),
            resets_at: None,
            kind: "rate_limit".into(),
        }
    }
}

/// Why a prompt was parked, or why a session's server queue is still waiting.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum QueueReason {
    /// A running turn that cannot take this prompt: the agent is not
    /// steerable, or the prompt is a command that must run on its own.
    TurnActive,
    /// A cancel is pending on the running turn.
    Cancelling,
    /// A `/compact` is running.
    Compacting,
    /// No live worker, and not a case this POST would wake (idle dormancy or a
    /// rate-limit park).
    WorkerDown,
    /// The agent's proxy broke the `_meta.aoe` contract, so core discards its
    /// output; the queue drains once a new connection restores the integration.
    IntegrationLost,
}

/// Lifecycle hook commands; each accepts a string or an array in TOML.
/// `on_create` failures abort creation; `on_launch` and `on_destroy` failures
/// only warn, and `on_destroy` runs before worktree/sandbox cleanup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "string_or_vec"
    )]
    pub on_create: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "string_or_vec"
    )]
    pub on_launch: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "string_or_vec"
    )]
    pub on_destroy: Vec<String>,
}

impl HooksConfig {
    pub fn is_empty(&self) -> bool {
        self.on_create.is_empty() && self.on_launch.is_empty() && self.on_destroy.is_empty()
    }
}

/// Deserialize a field as either a single string or a Vec of strings.
/// Allows `key = "value"` as shorthand for `key = ["value"]` in TOML.
pub fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: de::Deserializer<'de>,
{
    struct Visitor;

    impl<'de> de::Visitor<'de> for Visitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or array of strings")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<String>, E> {
            Ok(vec![value.to_string()])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut vec = Vec::new();
            while let Some(val) = seq.next_element()? {
                vec.push(val);
            }
            Ok(vec)
        }
    }

    deserializer.deserialize_any(Visitor)
}
