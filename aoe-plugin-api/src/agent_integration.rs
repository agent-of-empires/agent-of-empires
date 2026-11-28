//! Provider-native agent session communication, independent of a specific agent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Agent communication protocol declared by a plugin.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationContribution {
    pub tool: String,
    /// Requires API 29: how pickers, notifications and the API name the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Requires API 29: a lucide icon name for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Alternative semver requirements for provider-native status reporting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_versions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Enable provider-native prompt replies; status-only adapters leave this disabled.
    #[serde(default)]
    pub replies: bool,
    /// Requires API 21 and a host that implements agent declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<AgentLaunchDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<AgentDetectionDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_fallback: Option<AgentReplyDeclaration>,
    /// Requires API 22 and a host that implements the capture protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<AgentSessionIdDeclaration>,
    /// Requires API 28: where and which status hooks the host installs for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<AgentHooksDeclaration>,
    /// Requires API 28: the folder-trust record the host writes before launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<AgentTrustDeclaration>,
    /// Requires API 30 and `acp.agent.provide`: the plugin supplies the agent's ACP adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpDeclaration>,
}

/// Proxy wire versions the host speaks; `AcpProxy` must declare exactly these.
pub const ACP_PROXY_PROTOCOL_VERSION: u32 = 1;

/// The only `compat.capability_min_versions` key the host enforces: it floors `_session/steering`.
pub const ACP_STEERING_CAPABILITY: &str = "steering";
pub const ACP_PRIMITIVE_SCHEMA_VERSION: u32 = 1;

/// How a plugin supplies an agent's ACP adapter and proxy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpDeclaration {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub display_name: String,
    pub proxy: AcpProxy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<AcpAdapter>,
    #[serde(default, skip_serializing_if = "AcpEnv::is_empty")]
    pub env: AcpEnv,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<AcpCompat>,
    #[serde(default, skip_serializing_if = "AcpProfile::is_empty")]
    pub profile: AcpProfile,
    pub failure_policy: AcpFailurePolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpProxy {
    /// Plugin-relative path of the proxy executable.
    pub command: String,
    pub protocol_version: u32,
    pub primitive_schema_version: u32,
}

/// The upstream adapter package the host installs for the proxy to wrap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpAdapter {
    pub upstream_package: String,
    /// Exact upstream version, no range operators.
    pub upstream_pin: String,
    /// Plugin-relative directory holding the lockfile and install inputs.
    pub package_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<AcpAdapterBinary>,
}

/// A prebuilt standalone adapter the host prefers over the package install.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpAdapterBinary {
    /// Release asset name per Rust target triple, such as `aarch64-apple-darwin`.
    pub assets: BTreeMap<String, String>,
    pub digests: AcpDigestSource,
}

/// Where the host reads the expected digest of a release asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpDigestSource {
    /// A `<asset>.sha256` file in the same release.
    Sha256Asset,
    /// The digest the release API reports for the asset.
    ReleaseApi,
}

/// Environment the host passes to the adapter process.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpEnv {
    /// Exact variable names forwarded from the host environment.
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// Allowlisted variables kept out of sandboxed containers.
    #[serde(default)]
    pub host_only: Vec<String>,
    /// Values set when the host environment does not provide the variable.
    #[serde(default)]
    pub defaults: BTreeMap<String, String>,
}

impl AcpEnv {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpCompat {
    /// `agentInfo.name` the adapter must report in `initialize`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_agent_name: Option<String>,
    /// Lowest adapter version the host accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    /// Lowest adapter version that offers each named capability; only `steering` is accepted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capability_min_versions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpProfile {
    #[serde(default)]
    pub modes: Vec<AcpMode>,
}

impl AcpProfile {
    fn is_empty(&self) -> bool {
        self.modes.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpMode {
    pub id: String,
    pub approval_class: crate::acp::ApprovalClass,
}

/// What the host does when the plugin's adapter cannot start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpFailurePolicy {
    /// Start the session on the neutral bundled boot.
    NeutralBoot,
    /// Fail the session.
    Refuse,
}

/// Hook events the host knows how to install into a JSON settings file.
pub const AGENT_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "Notification",
    "ElicitationResult",
    "PermissionRequest",
    "BeforeTool",
    "BeforeAgent",
    "AfterAgent",
];

pub const AGENT_HOOK_EVENTS_MAX: usize = 32;
pub const AGENT_HOOK_PATH_MAX: usize = 256;
pub const AGENT_HOOK_STRING_MAX: usize = 256;
pub const AGENT_TRUST_POINTER_SEGMENTS_MAX: usize = 8;
pub const AGENT_DISPLAY_NAME_MAX: usize = 40;

/// Status hooks installed into the agent's JSON settings file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentHooksDeclaration {
    /// Settings file relative to the home directory, such as `.claude/settings.json`.
    pub settings_rel_path: String,
    /// Environment variable that relocates the settings directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir_env_var: Option<String>,
    pub format: AgentHookFormat,
    pub events: Vec<AgentHookEventDeclaration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHookFormat {
    JsonSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHookStatus {
    Running,
    Waiting,
    Idle,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHookIdentityField {
    SessionId,
    ConversationIdOrSessionId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentHookEventDeclaration {
    /// One of [`AGENT_HOOK_EVENTS`].
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentHookStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_field: Option<AgentHookIdentityField>,
    /// Tools that block on the user for their whole run; the hook writes `waiting` for them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTrustLocation {
    /// The agent's config directory, else the home directory.
    ConfigDirOrHome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTrustSandbox {
    Always,
    YoloOnly,
}

/// A literal leaf written at the pointer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentTrustValue {
    Bool(bool),
    String(String),
}

/// One leaf written into a JSON file, such as `/projects/{path}/hasTrustDialogAccepted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTrustDeclaration {
    /// A single file name inside the location.
    pub file: String,
    pub location: AgentTrustLocation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir_env_var: Option<String>,
    /// Absolute pointer with exactly one `{path}` segment that receives the project path as one key.
    pub pointer: String,
    pub value: AgentTrustValue,
    pub sandbox: AgentTrustSandbox,
}

/// A container directory a declared config-dir variable may name: absolute and
/// normalized, away from the home directory itself and the host-provided mounts.
pub fn container_dir_ok(dir: &str) -> bool {
    const RESERVED: [&str; 6] = [
        "/workspace",
        "/aoe",
        "/proc",
        "/sys",
        "/dev",
        "/tmp/aoe-hooks",
    ];
    let Some(rest) = dir.strip_prefix('/') else {
        return false;
    };
    dir.len() <= AGENT_HOOK_PATH_MAX
        && dir != "/root"
        && !rest.is_empty()
        && rest.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains([':', ',', '\\'])
                && part.chars().all(|c| !c.is_control())
        })
        && !RESERVED
            .iter()
            .any(|reserved| dir == *reserved || dir.starts_with(&format!("{reserved}/")))
}

impl AgentIntegrationContribution {
    /// The directory a declared config-dir variable names in the container, else `default`.
    fn container_config_dir(&self, var: Option<&str>, default: String) -> String {
        var.and_then(|var| self.launch.as_ref()?.container_env.get(var))
            .map_or(default, |dir| dir.trim_end_matches('/').to_string())
    }

    /// Where a sandbox sees the declared hooks file, else `None`.
    pub fn container_hooks_file(&self) -> Option<String> {
        let hooks = self.hooks.as_ref()?;
        let (dir, leaf) = hooks.settings_rel_path.rsplit_once('/')?;
        let dir =
            self.container_config_dir(hooks.config_dir_env_var.as_deref(), format!("/root/{dir}"));
        Some(format!("{dir}/{leaf}"))
    }

    /// Where a sandbox sees the declared trust file, else `None`.
    pub fn container_trust_file(&self) -> Option<String> {
        let trust = self.trust.as_ref()?;
        let dir =
            self.container_config_dir(trust.config_dir_env_var.as_deref(), "/root".to_string());
        Some(format!("{dir}/{}", trust.file))
    }
}

/// The pointer's segments, or `None` unless it is a valid trust pointer.
pub fn parse_trust_pointer(pointer: &str) -> Option<Vec<&str>> {
    let rest = pointer.strip_prefix('/')?;
    let segments: Vec<&str> = rest.split('/').collect();
    let valid = segments.len() <= AGENT_TRUST_POINTER_SEGMENTS_MAX
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.len() <= AGENT_HOOK_STRING_MAX
                && (*segment == "{path}"
                    || (!segment.contains(['{', '}', '~'])
                        && segment.chars().all(|c| !c.is_control())))
        })
        && segments
            .iter()
            .filter(|segment| **segment == "{path}")
            .count()
            == 1
        && segments
            .iter()
            .all(|segment| segment.parse::<usize>().is_err());
    valid.then_some(segments)
}

/// Session-id capture owned by the plugin worker.
///
/// The host owns the write only: it accepts a shape-checked report through the
/// guarded capture path, so the provider's stores and wire formats stay in the
/// plugin instead of in the host. The fields below describe the host-driven
/// polling that a later host version will add; this one validates them without
/// calling them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionIdDeclaration {
    /// Worker RPC that will return `{ session_id, generation }` per poll.
    pub method: String,
    /// Worker RPC that will return the launch wrapper and the tmux env it publishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_method: Option<String>,
    /// Deadline for one future call. A timeout means no capture for that poll.
    #[serde(default = "default_session_id_timeout_ms")]
    pub timeout_ms: u64,
}

pub const SESSION_ID_TIMEOUT_MS_MIN: u64 = 250;
pub const SESSION_ID_TIMEOUT_MS_MAX: u64 = 30_000;

fn default_session_id_timeout_ms() -> u64 {
    5_000
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLaunchDeclaration {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oneshot_flag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<AgentYoloDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_flag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_flag: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub container_env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
    #[serde(default)]
    pub host_only: bool,
    /// Pause between typing a message into the agent's pane and sending Enter,
    /// for an agent whose composer reads a fast burst as a paste and drops it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_keys_enter_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<AgentResumeDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<AgentCaptureDeclaration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentYoloDeclaration {
    Flag { flag: String },
    Always,
    Env { key: String, value: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentResumeDeclaration {
    Flag {
        flag: String,
    },
    FlagPair {
        existing: String,
        new_session: String,
    },
    Subcommand {
        command: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCaptureBackend {
    Claude,
    OpenCode,
    Codex,
    Gemini,
    Hermes,
    Kimi,
    Pi,
    PrimeAgent,
    Omp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCaptureContext {
    PaneScoped,
    Preassigned,
    ManagedExclusiveStore,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCaptureDeclaration {
    pub backend: AgentCaptureBackend,
    pub host: AgentCaptureContext,
    pub sandbox: AgentCaptureContext,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDetectionDeclaration {
    pub mode: AgentDetectionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDetectionMode {
    NativeOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReplyDeclaration {
    pub allow: Vec<AgentKeyDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_always: Option<Vec<AgentKeyDeclaration>>,
    pub deny: Vec<AgentKeyDeclaration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentKeyDeclaration {
    Literal { value: String },
    Named { value: String },
}

impl AgentIntegrationContribution {
    pub fn provider_compat(&self, version: &str) -> Result<(), String> {
        if self.provider_versions.is_empty() {
            return Ok(());
        }
        let parsed = semver::Version::parse(version)
            .map_err(|e| format!("invalid {} runtime version {version:?}: {e}", self.tool))?;
        for range in &self.provider_versions {
            let requirement = semver::VersionReq::parse(range)
                .map_err(|e| format!("invalid provider_versions range {range:?}: {e}"))?;
            if requirement.matches(&parsed) {
                return Ok(());
            }
        }
        Err(format!(
            "{} runtime {version} is unsupported; supported provider_versions: {}; install a supported runtime or update the integration plugin",
            self.tool,
            self.provider_versions.join(" or ")
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentIntegrationStatus {
    Running,
    Waiting,
    Idle,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationSnapshot {
    pub schema_version: u32,
    pub instance_id: String,
    pub provider_version: String,
    pub provider_session_id: String,
    pub writer_pid: u32,
    pub revision: u64,
    pub status: AgentIntegrationStatus,
    /// Function-key names sent only to wake the provider extension after a user reply.
    #[serde(default)]
    pub reply_keys: Vec<String>,
    pub prompt: Option<AgentIntegrationPrompt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum AgentIntegrationPrompt {
    Approval {
        id: String,
        title: String,
        allow_always: bool,
    },
    Question {
        id: String,
        title: String,
        questions: Vec<AgentIntegrationQuestion>,
    },
}

impl AgentIntegrationPrompt {
    pub fn id(&self) -> &str {
        match self {
            Self::Approval { id, .. } | Self::Question { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<AgentIntegrationOption>,
    #[serde(default)]
    pub multi: bool,
    pub recommended: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationAnswer {
    pub id: String,
    pub selected_options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_input: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationApprovalChoice {
    Allow,
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum AgentIntegrationResponse {
    Approval {
        choice: IntegrationApprovalChoice,
    },
    Question {
        answers: Vec<AgentIntegrationAnswer>,
    },
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationReply<'a> {
    pub schema_version: u32,
    pub instance_id: &'a str,
    pub provider_session_id: &'a str,
    pub writer_pid: u32,
    pub revision: u64,
    pub prompt_id: &'a str,
    pub response_id: String,
    pub response: AgentIntegrationResponse,
}

/// The leading `NAME=value` assignments and the executable word of a launch
/// command, plus how many of the words are assignments.
///
/// Discovery checks that executable without running anything, and the launch
/// passes these words to `exec env` unchanged. Only spaces and tabs separate
/// them, and no word may contain anything the launch shell would act on, so a
/// probed word is the word `env` receives. Quoting, expansion, field splitting
/// and command separators therefore cannot make discovery disagree with the
/// launch. Only the prefix is inspected: arguments after the executable are
/// never parsed.
pub fn launch_prefix(command: &str) -> Option<(Vec<&str>, usize)> {
    let mut words: Vec<&str> = Vec::new();
    for word in command.split([' ', '\t']) {
        // Repeated separators produce no word in the shell either.
        if word.is_empty() {
            continue;
        }
        if !literal_word(word) {
            return None;
        }
        if let Some((key, _)) = word.split_once('=') {
            // `env` reads every `NAME=value` word as an assignment, valid name
            // or not, so one it cannot set is not a launchable program either.
            if !valid_env_key(key) {
                return None;
            }
            words.push(word);
            continue;
        }
        // `env` reads a leading `-` word as its own option, never a program.
        if word.starts_with('-') {
            return None;
        }
        let assignments = words.len();
        words.push(word);
        return Some((words, assignments));
    }
    None
}

/// Every word of a direct launch command and how many leading ones are
/// assignments, or `None` when the whole command is not one.
///
/// A direct launch is [`launch_prefix`]'s executable plus only literal words
/// after it, so arguments appended to the command reach that executable instead
/// of a pipe, redirection or separate command. [`launch_prefix`] itself stops at
/// the executable and never inspects the tail.
pub fn direct_launch_words(command: &str) -> Option<(Vec<&str>, usize)> {
    let (_, assignments) = launch_prefix(command)?;
    let words: Vec<&str> = command
        .split([' ', '\t'])
        .filter(|word| !word.is_empty())
        .collect();
    words
        .iter()
        .all(|word| literal_word(word))
        .then_some((words, assignments))
}

/// Whether the launch shell leaves a word intact: no separators, redirections,
/// globs, braces, expansions, quoting or whitespace other than what
/// `launch_prefix` already split on. A newline is a command separator to the
/// shell and a non-breaking space is one to `split_whitespace`, so both would
/// make the probed words differ from the launched ones.
fn literal_word(word: &str) -> bool {
    word.chars().all(|c| {
        !c.is_whitespace()
            && !c.is_control()
            && !matches!(
                c,
                '\'' | '"'
                    | '\\'
                    | '$'
                    | '`'
                    | '~'
                    | '#'
                    | ';'
                    | '&'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '*'
                    | '?'
                    | '['
                    | ']'
            )
    })
}

/// Whether a plugin's ACP `env` may not name `key`: aoe's own variables, infrastructure and
/// dynamic-linker keys, and interpreter hooks that run code in the adapter. The host drops
/// these at resolve time, so a manifest declaring one is refused.
pub fn acp_env_denied(key: &str) -> bool {
    const DENIED: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "LC_ALL",
        "TERM",
        "NODE_OPTIONS",
        "BASH_ENV",
        "ENV",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "GIT_SSH_COMMAND",
        "SHELL",
        "TMPDIR",
    ];
    key.starts_with("AOE_")
        || key.starts_with("LD_")
        || key.starts_with("DYLD_")
        || DENIED.contains(&key)
}

pub(crate) fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::{direct_launch_words, launch_prefix};

    #[test]
    fn launch_prefix_stops_at_the_executable() {
        assert_eq!(
            launch_prefix("PATH=/opt/agents/bin acmebot --filter='a b'"),
            Some((vec!["PATH=/opt/agents/bin", "acmebot"], 1))
        );
        assert_eq!(
            launch_prefix("acmebot \"$(printf x)\" `unrun`"),
            Some((vec!["acmebot"], 0))
        );
        assert_eq!(launch_prefix("TOKEN=local"), None);
        // Not assignments `env` can set, and `env` options, not programs.
        assert_eq!(launch_prefix("9INVALID=1 acmebot"), None);
        assert_eq!(launch_prefix("=x acmebot"), None);
        assert_eq!(launch_prefix("-i acmebot"), None);
        assert_eq!(launch_prefix("  "), None);
    }

    #[test]
    fn launch_prefix_refuses_shell_syntax_before_the_executable() {
        for command in [
            "PATH=$HOME/bin acmebot",
            "PATH=\"$HOME/bin\" acmebot",
            "PATH=${HOME}/bin acmebot",
            "PATH=${HOME/bin acmebot",
            "PATH=~/bin acmebot",
            "~/bin/acmebot",
            "PATH=/opt/agents\\\n/bin acmebot",
            "\"acmebot",
            "'acmebot",
            "\"\" acmebot",
            "PATH=$(printf /tmp) acmebot",
            "PATH=`echo /tmp` acmebot",
            "PATH=/tmp # comment",
            // A newline ends the shell command, so `env` would run without an
            // executable instead of running the probed one.
            "TOKEN=local\nacmebot",
            "TOKEN=local\r\nacmebot",
            // Command separators and redirections split or redirect the launch.
            "TOKEN=local; acmebot",
            "TOKEN=local; acmebot",
            "TOKEN=local&acmebot",
            "TOKEN=local|acmebot",
            "TOKEN=local&&acmebot",
            "PATH=/opt/a>b acmebot",
            "PATH=/opt/a<b acmebot",
            "PATH=(x) acmebot",
            // Globs and braces expand to words discovery never sees.
            "PATH=/opt/*/bin acmebot",
            "PATH=/opt/[ab]/bin acmebot",
            "PATH=/opt/?/bin acmebot",
            "PATH={/opt/a,/opt/b} acmebot",
            // Non-ASCII whitespace is not a shell word separator.
            "TOKEN=local\u{a0}acmebot",
        ] {
            assert_eq!(launch_prefix(command), None, "{command:?}");
        }
    }

    #[test]
    fn direct_launch_words_keep_the_literal_tail() {
        assert_eq!(
            direct_launch_words("TOKEN=local acmebot --flag"),
            Some((vec!["TOKEN=local", "acmebot", "--flag"], 1))
        );
        assert_eq!(direct_launch_words("acmebot"), Some((vec!["acmebot"], 0)));
        for command in [
            // Appended arguments would land on the pipe or the next command.
            "acmebot | tee /tmp/acmebot.log",
            "acmebot; echo done",
            "acmebot && echo done",
            "acmebot > /tmp/acmebot.log",
            "acmebot # trailing comment",
            // A quoted argument cannot be re-formed from words `env` receives.
            "acmebot --flag='a b'",
            "",
        ] {
            assert_eq!(direct_launch_words(command), None, "{command}");
        }
    }

    #[test]
    fn launch_prefix_accepts_tabs_and_repeated_spaces_between_words() {
        assert_eq!(
            launch_prefix("PATH=/opt/agents/bin\tacmebot  --flag"),
            Some((vec!["PATH=/opt/agents/bin", "acmebot"], 1))
        );
    }
}
