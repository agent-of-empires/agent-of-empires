//! User configuration management: the app `Config` plus the profile, repo,
//! container, and settings-schema layers that derive from it.

pub(crate) mod container_config;
pub mod profile_config;
pub mod repo_config;
pub mod settings_schema;

use self::repo_config::{HooksConfig, HostHooksConfig};
use super::get_app_dir;
use anyhow::Result;
use aoe_settings_derive::SettingsSection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_profile")]
    pub default_profile: String,

    #[serde(default)]
    pub theme: ThemeConfig,

    #[serde(default)]
    pub updates: UpdatesConfig,

    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub skills: SkillsConfig,

    #[serde(default)]
    pub worktree: WorktreeConfig,

    #[serde(default)]
    pub sandbox: SandboxConfig,

    #[serde(default)]
    pub tmux: TmuxConfig,

    #[serde(default)]
    pub session: SessionConfig,

    #[serde(default)]
    pub diff: DiffConfig,

    #[serde(default)]
    pub hooks: HooksConfig,

    /// Host-side lifecycle hooks. Profile/global only; never honored from a
    /// repo's `.agent-of-empires/config.toml` (these run commands on the host).
    #[serde(default)]
    pub host_hooks: HostHooksConfig,

    #[serde(default)]
    pub sound: crate::sound::SoundConfig,

    #[serde(default)]
    pub status_hooks: crate::status_hooks::StatusHookConfig,

    #[serde(default)]
    pub app_state: AppStateConfig,

    #[serde(default)]
    pub web: WebConfig,

    #[serde(default)]
    pub auth: AuthConfig,

    #[serde(default)]
    pub acp: AcpConfig,

    #[serde(default)]
    pub logging: LoggingConfig,

    /// Trusted global/profile agent runtime overrides. Repo config does not
    /// merge this section, because hook installation writes durable agent files.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, AgentRuntimeConfig>,

    /// Environment variables injected into the host command line for every
    /// session spawned at global scope. Entries are `KEY=value`, `KEY=$VAR`
    /// (read VAR from the host env), `KEY=$$literal` (escape a `$`), or
    /// bare `KEY` (passthrough from the host env). Values are passed through
    /// verbatim; `~` is not expanded, use an absolute path. Profiles can
    /// replace this list via their own `environment` field. Sandboxed
    /// sessions ignore this list; configure `sandbox.environment` instead.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    pub environment: Vec<String>,

    /// User-defined tool sessions: name -> config.
    /// Tools are launched in the selected session's working directory and
    /// persist as independent tmux sessions until the parent agent session
    /// is deleted. Access via hotkey, the tool picker (`;`), or command
    /// palette (Ctrl+K).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tools: HashMap<String, ToolSessionConfig>,

    /// Per-plugin configuration keyed by plugin id (`[plugins."aoe.web"]`).
    /// An explicit typed map rather than a root-level flatten, so plugin
    /// enable-state survives every save without a root catch-all quietly
    /// absorbing mistyped core keys.
    ///
    /// Unknown core keys are dropped rather than rejected: there is no
    /// `deny_unknown_fields`, so serde ignores them on load and the
    /// re-serialize in [`update_config`] does not write them back. This is
    /// the one limit on that function's "unrelated edits survive" contract:
    /// it holds for fields this binary knows, so a key written by a newer
    /// `aoe` does not survive an older `aoe`'s save.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub plugins: std::collections::BTreeMap<String, PluginConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRuntimeConfig {
    /// Per-agent hook event to AoE status mapping. Overrides built-in hook
    /// defaults by event name when status hooks are installed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub status_map: BTreeMap<String, crate::agents::HookStatus>,

    /// Declarative pane status rules (`[[agents.<name>.status_rules]]`).
    /// Gives an agent with no built-in pane detector, typically a
    /// `[session.custom_agents]` harness that is not the same binary as any
    /// built-in, basic status detection without a code change. Ordered,
    /// first match wins, no match reports `idle`. Rules take precedence over
    /// `agent_detect_as` and over a built-in detector of the same name.
    /// Compiled into `tmux::status_rules` on config resolve.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status_rules: Vec<StatusRule>,
}

/// One declarative pane status rule. `status` is required; exactly one of
/// `contains` (case-insensitive substring) or `regex` (Rust regex syntax)
/// must be set. Both are matched against the ANSI-stripped pane snapshot.
/// A rule with neither, both, or an invalid regex is skipped with a warning
/// at compile time (`tmux::status_rules::install_from_config`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusRule {
    /// Status to report when the rule matches: `running`, `waiting`,
    /// `idle`, or `error`.
    pub status: crate::agents::HookStatus,

    /// Case-insensitive substring to look for in the pane text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,

    /// Regex (Rust `regex` crate syntax) matched against the pane text as
    /// written; prefix with `(?i)` for case-insensitive matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

/// Configuration for one plugin: whether it is enabled, its install source and
/// capability grant (external plugins only), plus its schema-free persisted
/// settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    /// Whether the plugin is active. A disabled plugin contributes nothing to
    /// any surface.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Install source for an external plugin: a `gh:owner/repo[@ref]` slug or a
    /// local path. Absent for builtins, which are compiled in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// The plugin's persisted settings (`[plugins."<id>".settings]`). Kept as
    /// an opaque `toml::Table` so values survive on disk even while the plugin
    /// is disabled; the typed schema that validates and renders them lands
    /// with Tier 0 registries (#2094). The toml serializer emits scalars before
    /// subtables regardless, so field order here is for readability. Empty is
    /// omitted.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub settings: toml::Table,

    /// The capability grant the user approved for an external plugin, pinned to
    /// the manifest hash it was approved against. Absent until granted;
    /// builtins are auto-granted and never store one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<CapabilityGrant>,

    /// An available update the user declined in-app, recorded by its content
    /// fingerprint so the popup and auto-update notification stop nagging until
    /// the next version. Cleared on any successful apply or uninstall. Absent
    /// when no update has been dismissed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_update: Option<String>,
}

/// A user's approval of an external plugin's requested capabilities, pinned to
/// the exact manifest it was approved against. When the installed manifest hash
/// later differs (an update that changed the capability set), the grant no
/// longer applies and the plugin's runtime contributions stay inactive until
/// re-approved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityGrant {
    /// `sha256:<hex>` of the manifest bytes this grant was approved against.
    pub manifest_hash: String,
    /// The capabilities the user approved.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// When the user approved the grant.
    pub granted_at: chrono::DateTime<chrono::Utc>,
}

fn default_enabled() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            source: None,
            settings: toml::Table::new(),
            grant: None,
            dismissed_update: None,
        }
    }
}

/// Configuration for a user-defined tool session (lazygit, yazi, tig, etc.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSessionConfig {
    /// Shell command to run (e.g. "lazygit", "yazi", "tig --all").
    /// The string is passed to the shell, so pipes and `&&` work.
    #[serde(default)]
    pub command: String,
    /// Optional hotkey binding in `Alt+<letter>` format (e.g. "Alt+g", "Alt+f").
    /// Only Alt+ single-character bindings are supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<String>,
    /// Run fire-and-forget in the selected session's working directory instead
    /// of opening a persistent tmux tool session.
    #[serde(default, skip_serializing_if = "is_false")]
    pub background: bool,
}

/// Persistent logging configuration. Drives the default tracing
/// filter when no `AOE_LOG_LEVEL` env var is set, and is the
/// source of truth the settings UI writes to.
///
/// `default_level` is the baseline applied to every known target
/// root (see `crate::logging::DEFAULT_TARGET_ROOTS`). Entries in
/// `targets` override per-target.
///
/// Env var takes precedence: when `AOE_LOG_LEVEL` is set at startup,
/// this config is ignored for the initial filter (env wins for
/// CI/scripted runs). Runtime changes via `/api/log-level` always
/// honor whichever is active.
#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "logging", category = "Logging")]
pub struct LoggingConfig {
    /// Baseline applied to every known target root. Per-target overrides win.
    #[serde(default = "default_log_level")]
    #[setting(
        label = "Default level",
        widget = "select",
        options = "trace:trace,debug:debug,info:info,warn:warn,error:error",
        global_only
    )]
    pub default_level: String,

    /// Per-target log level overrides. Each entry maps a tracing target root
    /// to a level; (default) inherits the baseline.
    #[serde(default)]
    #[setting(
        label = "Per-target overrides",
        widget = "custom:logging-targets",
        global_only
    )]
    pub targets: std::collections::BTreeMap<String, String>,

    /// Where tracing lands: file (default) or stdout. TUI / daemon child /
    /// runner coerce to file regardless. Restart aoe for changes to take
    /// effect.
    #[serde(default)]
    #[setting(
        label = "Output (restart req.)",
        widget = "select",
        options = "file:file,stdout:stdout",
        global_only,
        advanced
    )]
    pub output: SinkKind,

    /// Log file location. Relative paths resolve under the app data dir;
    /// absolute paths are used verbatim. Restart aoe for changes.
    #[serde(default = "default_file_path")]
    #[setting(
        label = "File path (restart req.)",
        widget = "text",
        global_only,
        advanced
    )]
    pub file_path: String,

    /// size rotates when the live file crosses the threshold; never disables
    /// rotation. Restart aoe for changes.
    #[serde(default)]
    #[setting(
        label = "Rotation (restart req.)",
        widget = "select",
        options = "size:size,never:never",
        global_only,
        advanced
    )]
    pub rotation: RotationKind,

    /// Rotation threshold in MiB. Ignored when rotation = never.
    #[serde(default = "default_max_size_mib")]
    #[setting(
        label = "Max size MiB (restart req.)",
        widget = "number",
        min = 0,
        global_only,
        advanced
    )]
    pub max_size_mib: u64,

    /// How many rotated files to retain (.1 through .keep_count).
    #[serde(default = "default_keep_count")]
    #[setting(
        label = "Keep count (restart req.)",
        widget = "number",
        min = 0,
        global_only,
        advanced
    )]
    pub keep_count: u8,

    /// When on, every log line is prefixed with the names and fields of the
    /// spans wrapping it (e.g. `http_request{request_id=... method=GET
    /// path=...}` from the per-request middleware). Useful for
    /// grep-correlation across async boundaries when triaging; noisy on idle
    /// polling endpoints. Off by default keeps the log readable.
    #[serde(default = "default_show_spans")]
    #[setting(
        label = "Show span context (restart req.)",
        widget = "toggle",
        global_only,
        advanced
    )]
    pub show_spans: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkKind {
    #[default]
    File,
    Stdout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationKind {
    #[default]
    Size,
    Never,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            default_level: default_log_level(),
            targets: std::collections::BTreeMap::new(),
            output: SinkKind::default(),
            file_path: default_file_path(),
            rotation: RotationKind::default(),
            max_size_mib: default_max_size_mib(),
            keep_count: default_keep_count(),
            show_spans: default_show_spans(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_file_path() -> String {
    "debug.log".to_string()
}

fn default_max_size_mib() -> u64 {
    50
}

fn default_keep_count() -> u8 {
    5
}

fn default_show_spans() -> bool {
    false
}

/// Configuration for the acp (ACP-based native rendering of agent
/// state). Defaults match the documented v4 design and v005 migration.
///
/// `#[derive(SettingsSection)]` makes every `#[setting]`-annotated field the
/// single source of truth for the TUI, web dashboard, server policy, and
/// validation (#1692). Adding a field here, with its `#[setting]` attributes,
/// is the only edit needed for it to appear and round-trip everywhere.
#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "acp", category = "Acp")]
pub struct AcpConfig {
    /// Show the "Structured view" toggle in the new-session dialog, and
    /// offer switching an existing terminal session into the structured
    /// view. The structured view is still maturing, so both are hidden by
    /// default; turn this on to opt in. Opening already-structured sessions,
    /// and switching a structured session back to a terminal, are unaffected.
    #[serde(default)]
    #[setting(
        label = "Offer structured view when creating a session",
        widget = "toggle"
    )]
    pub offer_structured_in_new_session: bool,
    /// Acp agent used when --agent is not specified (e.g. claude-code,
    /// codex). Must name an agent that can start: `aoe-agent` is not
    /// packaged yet (#3553).
    #[serde(default = "default_agent")]
    #[setting(label = "Default agent", widget = "text", validate = "nonempty")]
    pub default_agent: String,
    /// Restrict structured view sessions to the agents named in
    /// `allowed_agents`. Off by default, which leaves every registered agent
    /// available and matches the behavior before this setting existed.
    ///
    /// This is an operator control, not a preference: it is read from the
    /// global config only (see `crate::acp::agent_policy`), so a profile
    /// override cannot widen it, and the web surface requires elevation.
    #[serde(default)]
    #[setting(
        label = "Restrict agents to the allowlist",
        widget = "toggle",
        global_only,
        advanced,
        web = "elevation:restricts which coding agents a session may run"
    )]
    pub restrict_agents: bool,
    /// Agent registry keys a structured view session may run while
    /// `restrict_agents` is on (e.g. claude, codex, opencode). These are
    /// registry keys, not binary names. An empty list with the restriction on
    /// denies every agent, so a lockdown names the agents it permits.
    #[serde(default)]
    #[setting(
        label = "Allowed agents",
        widget = "list",
        global_only,
        advanced,
        web = "elevation:restricts which coding agents a session may run"
    )]
    pub allowed_agents: Vec<String>,
    /// Hard cap on simultaneously running acp agent subprocesses;
    /// additional sessions queue.
    #[serde(default = "default_max_workers")]
    #[setting(
        label = "Max concurrent workers",
        widget = "number",
        min = 1,
        validate = "range:1",
        advanced
    )]
    pub max_concurrent_workers: u32,
    /// Per-session retention cap on acp events. 0 = unlimited (default);
    /// set a non-zero value to bound disk usage on long-running sessions.
    #[serde(default = "default_replay_events")]
    #[setting(label = "History cap (events)", widget = "number", min = 0)]
    pub replay_events: u32,
    /// Override Node.js binary location. Empty resolves via
    /// AOE_ACP_NODE then PATH then the bundled fallback.
    #[serde(default)]
    #[setting(
        label = "Node path",
        widget = "text",
        web = "local_only:host Node binary path, a local execution surface"
    )]
    pub node_path: String,
    /// Render a per-tool elapsed-time label on every acp tool card.
    /// Cross-device via config.toml. The underlying measurement is currently
    /// imprecise on claude-agent-acp (no `status: in_progress` signal), so
    /// durations include stream-arrival skew; turn off if the inflated
    /// numbers are more confusing than useful.
    #[serde(default = "default_true")]
    #[setting(label = "Show tool-call durations", widget = "toggle")]
    pub show_tool_durations: bool,
    /// Show a dismissable reminder in the structured view once the agent's
    /// context window passes `compaction_reminder_percent`, suggesting
    /// `/compact`. Off by default: the composer's usage chip already
    /// reports the percentage passively, and a banner is an interruption
    /// only some users want. Agents that do not advertise a `compact`
    /// command never show it. See #3253.
    #[serde(default)]
    #[setting(label = "Compaction reminder", widget = "toggle")]
    pub compaction_reminder: bool,
    /// Context-window percentage at which the compaction reminder appears.
    /// Independent of the usage meter's own warn colour, which stays at a
    /// fixed 90%: that is passive severity, this is when to interrupt.
    #[serde(default = "default_compaction_reminder_percent")]
    #[setting(
        label = "Compaction reminder threshold (%)",
        widget = "number",
        min = 1,
        max = 99,
        validate = "range:1:99"
    )]
    pub compaction_reminder_percent: u8,
    /// Silent-orphan watchdog: vendor-agnostic correctness grace. When
    /// a prompt is in flight, `tool_calls_in_flight` is empty, at least
    /// one progress notification has arrived, and no further progress
    /// arrives for this many seconds, the daemon sends best-effort
    /// `session/cancel` and arms the existing cancel-escalation grace.
    /// Closes the gap where claude-agent-acp finishes streaming but
    /// never sends `PromptResponse` (upstream
    /// agentclientprotocol/claude-agent-acp#688). Upstream
    /// agentclientprotocol/claude-agent-acp#706 (shipped in 0.37.0)
    /// recovers the prompt stream after a failed turn for some cases,
    /// reducing the false-positive rate, but cannot rescue every wedge
    /// (transport-level stalls, child process hangs, lost terminal
    /// frames), so the watchdog stays as the vendor-agnostic floor.
    /// Default 120s; raised
    /// from 60s in #1360 so async-agent flows (Claude SDK `Agent` tool
    /// with `isAsync: true`) get a longer wait window before the
    /// watchdog cancels them. `0` disables the watchdog. Long-running
    /// tools are not affected; the watchdog only fires when no
    /// in-flight tool call is open. The async-agent extension lifts the
    /// effective grace to at least 30 minutes when the daemon observes
    /// an async-agent launch in the current prompt. Nonzero values
    /// below 120 clamp up at runtime so a typo cannot disable the
    /// watchdog accidentally. See #1240, #1360.
    #[serde(default = "default_silent_orphan_grace_secs")]
    #[setting(
        label = "Silent-orphan grace (s)",
        widget = "number",
        min = 0,
        advanced
    )]
    pub silent_orphan_grace_secs: u32,
    /// Auto-stop idle acp workers: seconds of inactivity (no acp
    /// events and no in-flight turn) after which the daemon shuts a
    /// worker down and marks its session dormant so the reconciler does
    /// not respawn it. The next user prompt wakes the session and the
    /// reconciler spawns a fresh worker. Default 3600 (1 hour); `0`
    /// disables the feature entirely, so no worker is ever stopped for
    /// inactivity.
    #[serde(default = "default_acp_auto_stop_idle_secs")]
    #[setting(
        label = "Auto-stop idle worker (s)",
        widget = "number",
        min = 0,
        advanced
    )]
    pub auto_stop_idle_secs: u32,
    /// Opt-in auto-resume after a provider usage/rate-limit reset. When a
    /// acp worker stops with `Stopped { reason: "rate_limited" }`, the
    /// session is parked and (by default) waits for explicit user
    /// recovery via `/acp/spawn` or agent handoff (the #1281
    /// behavior). With this enabled, the reconciler instead respawns the
    /// same worker automatically once the adapter-reported reset time
    /// (plus a fixed grace for clock skew and adapter jitter) has passed, publishing a
    /// `RateLimitAutoResumed` breadcrumb for timeline clarity. Resume
    /// timing is read from the persisted `RateLimit` event, so it survives
    /// a daemon restart; a re-rate-limit writes a fresh reset time, so
    /// there is no tight restart loop. Vendor-agnostic: any ACP backend
    /// that reports `kind == "rate_limit"` is eligible. Default false,
    /// preserving the manual-first behavior. See #1722.
    #[serde(default)]
    #[setting(label = "Auto-resume after rate limit", widget = "toggle")]
    pub rate_limit_auto_resume: bool,
    /// Allow the web dashboard's "Update & restart" control to run the
    /// agent's `npm install -g <pkg>` on the host and respawn the worker.
    /// Off by default: the daemon executing a global package install is a
    /// host-level capability (it runs arbitrary npm lifecycle scripts as the
    /// daemon user), so it stays opt-in, is always blocked in read-only mode,
    /// and is `local_only` so a remote dashboard client cannot flip it on.
    /// Only npm-installable agents are eligible; others keep the manual
    /// install hint. See #2109.
    #[serde(default)]
    #[setting(
        label = "Allow agent install from web",
        widget = "toggle",
        web = "local_only:runs npm install on the host as the daemon user",
        global_only,
        advanced
    )]
    pub allow_agent_install: bool,

    /// Per-agent structured-view startup defaults, keyed by agent name
    /// (`{"<agent>": {"model": "...", "effort": "...", "mode": "...",
    /// "effort_by_model": {"<model>": "..."}}}`). `model` is forwarded at spawn;
    /// `effort` and `mode` are applied through ACP config options when
    /// advertised. `effort_by_model` overrides `effort` for a matching model.
    ///
    /// Edited per agent through the `acp-defaults` custom widget (composer-style
    /// dropdowns on the web, an inline JSON field in the TUI).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[setting(label = "Structured View Defaults", widget = "custom:acp-defaults")]
    pub acp_defaults: HashMap<String, AcpAgentDefaults>,
}

fn default_auto_stop_idle_secs() -> u32 {
    0
}

fn default_prevent_sleep_idle_grace_minutes() -> u32 {
    15
}

fn default_acp_auto_stop_idle_secs() -> u32 {
    3600
}

fn default_compaction_reminder_percent() -> u8 {
    75
}

fn default_silent_orphan_grace_secs() -> u32 {
    120
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            offer_structured_in_new_session: false,
            default_agent: default_agent(),
            restrict_agents: false,
            allowed_agents: Vec::new(),
            max_concurrent_workers: default_max_workers(),
            replay_events: default_replay_events(),
            node_path: String::new(),
            show_tool_durations: true,
            compaction_reminder: false,
            compaction_reminder_percent: default_compaction_reminder_percent(),
            silent_orphan_grace_secs: default_silent_orphan_grace_secs(),
            auto_stop_idle_secs: default_acp_auto_stop_idle_secs(),
            rate_limit_auto_resume: false,
            allow_agent_install: false,
            acp_defaults: HashMap::new(),
        }
    }
}

/// Built-in `acp.default_agent`. `aoe-agent` is not packaged yet (#3553), so
/// the default has to be an adapter `aoe acp doctor --fix` can install.
pub const DEFAULT_ACP_AGENT: &str = "claude-code";

fn default_agent() -> String {
    DEFAULT_ACP_AGENT.to_string()
}
fn default_max_workers() -> u32 {
    100
}
fn default_replay_events() -> u32 {
    // 0 = unlimited. The event store's prune already gates on `> 0`
    // (see `EventStore::record`), so the default flip is end-to-end
    // safe: a fresh install never truncates history; users who want a
    // ceiling for disk-space reasons can set a non-zero value in
    // config.toml or the settings TUI. See #1065.
    0
}

/// Session list sort order
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    Newest,
    Attention,
    LastActivity,
    Oldest,
    AZ,
    ZA,
}

impl SortOrder {
    pub fn cycle(self) -> Self {
        match self {
            SortOrder::Newest => SortOrder::Attention,
            SortOrder::Attention => SortOrder::LastActivity,
            SortOrder::LastActivity => SortOrder::Oldest,
            SortOrder::Oldest => SortOrder::AZ,
            SortOrder::AZ => SortOrder::ZA,
            SortOrder::ZA => SortOrder::Newest,
        }
    }

    pub fn cycle_reverse(self) -> Self {
        match self {
            SortOrder::Newest => SortOrder::ZA,
            SortOrder::Attention => SortOrder::Newest,
            SortOrder::LastActivity => SortOrder::Attention,
            SortOrder::Oldest => SortOrder::LastActivity,
            SortOrder::AZ => SortOrder::Oldest,
            SortOrder::ZA => SortOrder::AZ,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortOrder::Newest => "Newest",
            SortOrder::Attention => "Attention",
            SortOrder::LastActivity => "Recent",
            SortOrder::Oldest => "Oldest",
            SortOrder::AZ => "A-Z",
            SortOrder::ZA => "Z-A",
        }
    }
}

/// Session list grouping mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupByMode {
    #[default]
    Manual,
    Project,
    Org,
}

impl GroupByMode {
    pub fn cycle(self) -> Self {
        match self {
            GroupByMode::Manual => GroupByMode::Project,
            GroupByMode::Project => GroupByMode::Org,
            GroupByMode::Org => GroupByMode::Manual,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GroupByMode::Manual => "Manual",
            GroupByMode::Project => "Project",
            GroupByMode::Org => "Org",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppStateConfig {
    #[serde(default)]
    pub has_seen_welcome: bool,

    /// Whether the user has completed or skipped the web dashboard's
    /// first-run interactive tour. Stored server-side (rather than in
    /// per-browser localStorage) so a new browser or device does not
    /// re-show the tour. Distinct from `has_seen_welcome`, which gates
    /// the native TUI intro.
    #[serde(default)]
    pub has_seen_web_tour: bool,

    #[serde(default)]
    pub last_seen_version: Option<String>,

    /// Latest version for which the user dismissed the update banner. The
    /// banner stays hidden as long as the latest available version equals
    /// this value; it returns automatically when a newer release ships.
    /// Cleared by switching `update_check_mode` or upgrading past the
    /// snoozed version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_update_version: Option<String>,

    /// Registry digest of the sandbox image the user dismissed the
    /// "image update available" banner for. The banner stays hidden while the
    /// registry still resolves to this digest and returns automatically once a
    /// newer image is published (the digest no longer matches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_image_digest: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_list_width: Option<u16>,

    /// Whether the home-view session list is collapsed to a narrow,
    /// click-to-expand strip. Persisted so the choice survives restarts,
    /// mirroring `home_list_width`. `None`/absent means expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_sidebar_collapsed: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_file_list_width: Option<u16>,

    /// Show the info header (profile/tool/path/status/sandbox/worktree) at
    /// the top of the home preview pane. Defaults to `true` when absent;
    /// users hide it with `i` when they want the full pane for live output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_preview_info: Option<bool>,

    /// True once the user has answered the telemetry opt-in prompt (in any
    /// surface, either by enabling or declining). Gates the one-time
    /// standalone consent popup shown to users who completed the walkthrough
    /// before telemetry existed, so it appears once and never again.
    #[serde(default)]
    pub has_responded_to_telemetry: bool,

    #[serde(default)]
    pub has_seen_custom_instruction_warning: bool,

    /// Latches once the user has been warned (when adding a project in the TUI)
    /// that the directory is not a git repository, so the one-time notice about
    /// git features being unavailable does not repeat on every non-git add.
    #[serde(default)]
    pub has_seen_non_git_project_warning: bool,

    #[serde(default)]
    pub has_acknowledged_agent_hooks: bool,

    /// True once the user has acknowledged that glob `volume_ignores` entries
    /// (e.g. `**/bin`) are expanded against the workspace at session-create time,
    /// a point-in-time snapshot that won't shadow directories created later by an
    /// in-container build (#2045). Gates the one-time confirm dialog shown before a
    /// sandbox session whose resolved config contains a glob ignore.
    #[serde(default)]
    pub has_acknowledged_volume_ignores_globs: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<SortOrder>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<GroupByMode>,

    /// Last directory the user navigated to in the new-session dir picker.
    /// Restored on subsequent opens so users don't re-navigate every time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_browse_dir: Option<PathBuf>,

    /// Collapsed state for the synthetic "Archived" sidebar section.
    /// Defaults to collapsed when absent. Archived sessions are pulled out
    /// of the natural sort and grouped under this section at the bottom
    /// across every sort mode, so they stop interleaving with active rows
    /// without users in non-Attention modes losing the ability to find a
    /// shelved session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_section_collapsed: Option<bool>,

    /// Paths of project-mode sidebar folders the user has collapsed. Stored as
    /// the set of collapsed paths (absent/expanded folders are not listed), so
    /// the choice survives restarts. Group-mode collapse lives on the per-profile
    /// GroupTrees in session storage; project-mode folders are auto-derived and
    /// have no group record, so their collapse state is persisted here instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_group_collapsed: Vec<String>,

    /// Paths of org-mode sidebar folders the user has collapsed. Same shape
    /// and rationale as `project_group_collapsed`: org headers are derived
    /// from each session's resolved remote owner rather than a persisted
    /// group record, so their collapse state has nowhere else to live.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_group_collapsed: Vec<String>,

    /// Ids of tips the user has already seen/acknowledged. Drives the unseen
    /// badge count and stops earned tips from re-popping. Ids come from
    /// [`crate::tips`] and are stable, so this list stays meaningful across
    /// upgrades.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tips_seen: Vec<String>,

    /// How many times the new-session dialog has been opened while a project or
    /// session was selected. Once this passes
    /// [`crate::tips::NEW_FROM_SELECTION_TIP_THRESHOLD`], the "new from
    /// selection" tip becomes eligible (the earned trigger that closes #2262).
    #[serde(default)]
    pub new_session_with_selection_count: u32,

    /// Set once the user has actually used `N` (new-from-selection). They've
    /// discovered the feature, so the tip that teaches it is suppressed even if
    /// the count above crosses the threshold.
    #[serde(default)]
    pub used_new_from_selection: bool,

    /// Set after six live agents have been observed for three consecutive
    /// health samples, making the System Health discovery tip eligible.
    #[serde(default)]
    pub system_health_tip_earned: bool,

    /// Set once the detailed System Health view has been opened. Users who
    /// found it themselves do not need its earned discovery tip.
    #[serde(default)]
    pub used_system_health: bool,

    /// Server-side mirror of the web dashboard's syncable UI state, keyed by
    /// the frontend's localStorage key (the value is the opaque string the
    /// browser stored). Single-tenant: there is one user, so these prefs
    /// (sidebar sort/axis, tool density, repo appearance/order, group collapse,
    /// last-used tool, welcome-seen, etc.) live here so they follow the user
    /// across browsers and devices instead of being trapped in per-browser
    /// localStorage. The server never interprets the values; the web owns the
    /// shape. See `GET`/`PATCH /api/app-state/web-ui-state`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub web_ui_state: std::collections::BTreeMap<String, String>,
}

/// Session-related configuration defaults
#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
// `repo_default = "deny"`: most of this section is personal preference, but
// several fields name or build the command AoE hands to tmux
// (`custom_agents`, `agent_command_override`, `agent_extra_args`,
// `agent_acp_cmd`, `smart_rename_agent`, `smart_rename_model`) or weaken the
// agent's own permission gate (`yolo_mode_default`). Honoring those from a
// checked-out repo is arbitrary host command execution at session launch, the
// hazard that already keeps `host_hooks` out of `REPO_OVERRIDABLE_SECTIONS`.
// Denying by default means a field added here later stays repo-denied until
// someone marks it `repo = "allow"` (#3154).
#[setting_section(name = "session", category = "Session", repo_default = "deny")]
pub struct SessionConfig {
    /// Default coding tool for new sessions. If not set or the tool is
    /// unavailable, falls back to the first available tool. Repo-denied: the
    /// launch path exact-matches the user's `custom_agents` before built-ins,
    /// so a repo could name a user-defined host command, and validating
    /// against built-in names would not help because custom agents may
    /// shadow them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "Default Tool",
        widget = "custom:default-tool",
        category = "Agents",
        repo = "deny"
    )]
    pub default_tool: Option<String>,

    /// Enable YOLO mode by default for new sessions (skip permission prompts).
    #[serde(default)]
    #[setting(label = "YOLO Mode Default", widget = "toggle")]
    pub yolo_mode_default: bool,

    /// Pre-trust each session's worktree in the agent's own config so it does
    /// not open on a folder-trust prompt. Sandboxed sessions always do this;
    /// this setting extends it to host sessions, where the record is written to
    /// your real agent config and persists after the session is gone. Trust is
    /// also what activates a repo's own `.claude/settings.json`, so a
    /// pre-trusted worktree runs that file's hooks at session start without
    /// anyone having looked at the repo. Enable it only for directories you
    /// would have trusted by hand.
    #[serde(default)]
    #[setting(
        label = "Pre-trust worktrees on the host",
        widget = "toggle",
        category = "Agents",
        web = "elevation:lets a repo's own hooks run unprompted on the host"
    )]
    pub pre_trust_agent_folders: bool,

    /// Show the compact system-health strip below the session list. It reports
    /// CPU, memory pressure, and running agent and process counts. Off by
    /// default; also toggleable from the command palette.
    #[serde(default)]
    #[setting(label = "Show system health strip", widget = "toggle")]
    pub show_diagnostics_pane: bool,

    /// Forward AoE's whole environment to host sessions instead of just the
    /// desktop vars (DISPLAY, XDG_*, DBUS). Lets vars like GOPATH reach an
    /// agent without naming each one in the Host Environment list. AoE's own
    /// internals (AOE_* and AGENT_OF_EMPIRES_*) are never forwarded.
    #[serde(default)]
    #[setting(
        label = "Inherit Host Environment",
        widget = "toggle",
        web = "elevation:widens what every agent process can read from the host environment",
        advanced
    )]
    pub inherit_host_environment: bool,

    /// Per-agent extra arguments appended after the binary (e.g.
    /// opencode=--port 8080).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[setting(
        label = "Agent Extra Args",
        widget = "list",
        web = "local_only:per-agent argv injection, a host execution surface",
        category = "Agents"
    )]
    pub agent_extra_args: HashMap<String, String>,

    /// Per-agent command override. Native conversation resume requires the
    /// resolved agent binary first; wrappers and shell syntax disable it.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[setting(
        label = "Agent Command Override",
        widget = "list",
        web = "local_only:replaces the agent binary, a host execution surface",
        category = "Agents"
    )]
    pub agent_command_override: HashMap<String, String>,

    /// Install status-detection hooks into the agent's config file (e.g.
    /// ~/.claude/settings.json). When disabled, AoE will not modify the
    /// agent's settings file; status detection falls back to tmux pane
    /// content parsing, which is less reliable.
    #[serde(default = "default_true")]
    #[setting(label = "Agent Status Hooks", widget = "toggle", category = "Agents")]
    pub agent_status_hooks: bool,

    /// For agents whose hooks are scoped to a user-selected named agent (e.g.
    /// Kiro's `--agent NAME`), install AoE's status hooks into that agent's own
    /// config file so status detection keeps working, on both host and sandbox
    /// sessions. Such CLIs have no global hooks, so without this AoE's standalone
    /// hooks agent is never loaded for a user-selected agent and status goes
    /// dark. When disabled, AoE installs its standalone hooks agent instead and
    /// leaves the user's agent file untouched.
    #[serde(default = "default_true")]
    #[setting(
        label = "Merge Hooks Into Selected Agent",
        widget = "toggle",
        category = "Agents"
    )]
    pub merge_hooks_into_selected_agent: bool,

    /// Auto-rename a new session from its first turn, using the configured
    /// utility agent (the `smart_rename_agent` setting, falling back to the
    /// session's own agent) in one-shot mode (e.g. `claude -p`). Covers both
    /// structured-view
    /// (ACP) sessions, renamed at the end of the first turn, and terminal
    /// sessions, renamed when the poller first sees the pane go idle (so it
    /// works for a native `tmux attach` too). Only applies while the session
    /// still carries its auto-generated name; a manually named session is never
    /// touched. Title only: the worktree directory is not moved (the running
    /// agent holds it). Agents without a one-shot mode, sandboxed sessions, and
    /// command-overridden agents keep the generated name.
    #[serde(default = "default_true")]
    #[setting(label = "Smart Session Rename", widget = "toggle", category = "Agents")]
    pub smart_rename: bool,

    /// Agent used for one-shot utility calls (the smart-rename title and the
    /// conversation summary). Empty means use the session's own agent. Set
    /// this to point utility calls at a cheaper or more obedient model (e.g.
    /// codex or opencode) without changing the session's working agent. Only
    /// agents with a one-shot mode qualify; an unknown or one-shot-incapable
    /// value falls back to the session's own agent behavior. The picker lists
    /// installed one-shot-capable agents.
    #[serde(default)]
    #[setting(
        label = "Utility Agent",
        widget = "custom:smart-rename-agent",
        category = "Agents"
    )]
    pub smart_rename_agent: String,

    /// Per-agent model for the throwaway smart-rename title one-shot, keyed by
    /// agent name (e.g. `claude` = `haiku`). A three-to-five-word title should
    /// not bill the CLI's default frontier model. An absent key uses the
    /// agent's built-in default (claude pins its cheap alias, others the CLI
    /// default); an empty value forces the CLI default (opt out of the built-in
    /// pin); a non-empty value pins that model via the agent's model flag
    /// (`--model` / `-m`). Free-form: AoE pins no CLI version, so ids are not
    /// validated, and an id the CLI rejects simply keeps the generated name.
    /// Does not affect the conversation summary, which always uses the CLI
    /// default. Only agents with a one-shot mode are tunable.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[setting(
        label = "Smart Rename Model",
        widget = "custom:smart-rename-model",
        web = "elevation:pins the model argv token for the rename one-shot CLI call",
        category = "Agents"
    )]
    pub smart_rename_model: HashMap<String, String>,

    /// Periodically generate a "summary of the conversation so far" for a
    /// structured-view (ACP) session by running the session's own agent
    /// one-shot over the transcript (agent-agnostic, like smart rename).
    /// The summary appears as a callout in the transcript. Off by default:
    /// it is recurring token spend and sends the transcript to another
    /// agent invocation. The on-demand "Summarize" action works regardless
    /// of this setting. See #2808.
    #[serde(default)]
    #[setting(label = "Conversation summary", widget = "toggle", category = "Agents")]
    pub conversation_summary: bool,

    /// Pass `--resume <sid>` (or the agent's equivalent) when restarting (`e`)
    /// or reattaching (`Enter`) a terminal-mode session with a stored session
    /// id. Disable to always start these sessions fresh instead, e.g. to
    /// resume manually via the agent's own `/resume` picker. Does not affect
    /// Send Message or Live Send, which always try to preserve context when
    /// respawning a dead pane. See #2609.
    #[serde(default = "default_true")]
    #[setting(
        label = "Auto-resume on restart/reattach",
        widget = "toggle",
        category = "Agents"
    )]
    pub auto_resume_on_restart: bool,
    /// Pre-assign OpenCode's session id before launch so AoE knows the exact
    /// native identity before the first prompt. AoE creates the session through
    /// a short-lived `opencode serve` call. This avoids guessing from the shared
    /// SQLite store, at the cost of about two seconds on each new host launch.
    /// Off by default. Sandboxed OpenCode automatic capture is unsupported.
    #[serde(default)]
    #[setting(
        label = "Pre-assign opencode session id",
        widget = "toggle",
        category = "Agents",
        advanced
    )]
    pub opencode_preassign_session_id: bool,
    /// Request xterm mouse tracking so the TUI handles the scroll wheel
    /// (preview-pane scroll) and click-to-select rows. Disable to hand the
    /// wheel and text selection back to the terminal, e.g. iOS Mosh +
    /// Termius/Blink where mouse-tracking escapes aren't forwarded reliably.
    /// The AOE_MOUSE_CAPTURE env var remains an opt-out backstop and can still
    /// force capture off when set.
    #[serde(default = "default_true")]
    #[setting(label = "Mouse Capture", widget = "toggle", category = "Interaction")]
    pub mouse_capture: bool,

    /// User-defined agents: name=command (e.g. lenovo-claude=ssh -t lenovo
    /// claude). Custom agent names appear in the TUI agent picker alongside
    /// built-in agents.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[setting(
        label = "Custom Agents",
        widget = "list",
        web = "local_only:maps names to arbitrary shell commands, a host execution surface",
        category = "Agents"
    )]
    pub custom_agents: HashMap<String, String>,

    /// Status detection mapping: agent=builtin (e.g. lenovo-claude=claude).
    /// Maps a custom (or built-in) agent to another agent's status detection
    /// heuristics.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[setting(
        label = "Agent Detect As",
        widget = "list",
        web = "local_only:part of the agent-command surface, edited locally only",
        category = "Agents",
        repo = "allow"
    )]
    pub agent_detect_as: HashMap<String, String>,

    /// ACP launch command for a custom agent, enabling it to run in the
    /// structured acp UI (e.g., "oc-superpowers" = "ocp run sp acp").
    /// A custom agent with an entry here is acp-capable; without one it
    /// is tmux-only.
    ///
    /// Note: unlike `custom_agents` (a shell command run in a tmux pane),
    /// this value is split into argv with shell-word rules and executed
    /// directly, with no shell. For shell features, wrap explicitly, e.g.
    /// `sh -lc 'source ~/.profile && ocp run sp acp'`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[setting(
        label = "Agent Acp Command",
        widget = "list",
        web = "local_only:argv launched directly to run a custom agent in acp, a host execution surface",
        category = "Agents"
    )]
    pub agent_acp_cmd: HashMap<String, String>,

    /// Config directory read by the session's agent instead of its built-in
    /// default. Host sessions use the directory directly. Sandboxed sessions
    /// use its `sandbox` subdirectory, which AoE mounts at the resolved
    /// built-in config path and uses for hooks, credentials, and native-session
    /// capture. Native MCP discovery reads it too, so AoE reconciles the
    /// servers the agent loads.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[setting(
        label = "Agent Config Dir",
        widget = "list",
        web = "local_only:points an agent at a config dir whose settings file can run commands",
        category = "Agents"
    )]
    pub agent_config_dir: HashMap<String, String>,

    /// Require SHIFT on letter-based TUI hotkeys (e.g. SHIFT+N for New, SHIFT+D for Delete).
    /// Guards against accidental destructive actions from dictation software, a forgotten
    /// focus, or stray keystrokes. Navigation keys (h/j/k/l, arrows, Enter, Esc), punctuation
    /// (/, ?), and numeric modifiers stay unshifted. Previously-uppercase bindings
    /// (P, R, T, N, D, G) relocate to Ctrl+letter so nothing is lost.
    /// Note: Ctrl+D (diff view) may conflict with terminal EOF in some tmux configs;
    /// if so, rebind tmux's send-prefix or use the `D` key from the help overlay.
    /// Off by default; existing users keep the legacy single-letter UX.
    #[serde(default)]
    #[setting(label = "Strict Hotkeys", widget = "toggle")]
    pub strict_hotkeys: bool,

    /// Default snooze for `aoe session snooze` (1-43200 min, picker overrides).
    /// During the snooze window the session is treated like archive: sinks to
    /// the bottom, renders italic+dim with a `z ` prefix, ignored by the
    /// attention sort, then rejoins the active list when the timer expires.
    #[serde(default = "default_snooze_duration_minutes")]
    #[setting(
        label = "Snooze Duration (minutes)",
        widget = "number",
        min = 1,
        max = 43200,
        validate = "range:1:43200"
    )]
    pub snooze_duration_minutes: u32,

    /// Move deleted sessions to the trash instead of purging them
    /// immediately. When enabled (default), `delete`/`rm` and the TUI/web
    /// delete actions stop the session and hide it in a recoverable trash
    /// bucket; durable state (transcript, worktree, branch, container) is
    /// kept until the session is purged or its retention window expires.
    /// When disabled, delete performs the historical irreversible purge.
    /// An explicit purge (`aoe rm --purge`, the web "Delete permanently"
    /// action) always purges regardless of this setting.
    #[serde(default = "default_true")]
    #[setting(label = "Delete to Trash", widget = "toggle")]
    pub delete_to_trash: bool,

    /// Ask for confirmation before deleting a session with the TUI `d` key.
    /// On by default: `d` opens a confirmation dialog that a second `d`
    /// accepts and `Esc` dismisses, so typing into the sidebar while a
    /// session is selected can no longer trash it outright. Turn it off (here
    /// or with the dialog's "don't warn me again" checkbox) to get the
    /// historical one-keystroke trash back. Only affects the TUI
    /// trash path; the web delete dialog already confirms, and the
    /// permanent-delete/force-remove paths are gated by their own dialogs
    /// regardless. See #2583, #3364.
    #[serde(default = "default_true")]
    #[setting(label = "Confirm Before Delete", widget = "toggle")]
    pub confirm_delete: bool,

    /// Days a session stays in the trash before it is automatically purged,
    /// measured from when it was trashed. `0` keeps trashed sessions
    /// forever (manual purge only). Auto-purge is enforced by the `aoe serve`
    /// daemon (a startup sweep plus an hourly tick); without a running daemon,
    /// expired trash is purged on the next daemon start or by an explicit
    /// manual purge (`aoe rm --purge`, `aoe session empty-trash`).
    #[serde(default = "default_trash_retention_days")]
    #[setting(
        label = "Trash Retention (days)",
        widget = "number",
        min = 0,
        max = 3650,
        validate = "range:0:3650"
    )]
    pub trash_retention_days: u32,

    /// Seconds of inactivity after which a plain TUI/tmux session that has
    /// been `Idle` this long is auto-stopped (its tmux session and any
    /// sandbox container are killed and the row becomes a restartable
    /// `Stopped` row). `0` disables (default); no session is ever auto-stopped
    /// for inactivity. Idle age is anchored on the later of the last
    /// transition into `Idle` and the last user interaction, and a session
    /// with a currently attached tmux client is never stopped, so a session
    /// the user is reading is spared. Checked about once a minute, so the stop
    /// can lag the threshold by up to a minute. Acp workers use the
    /// separate `acp.auto_stop_idle_secs` knob; see #1689 and #1690.
    #[serde(default = "default_auto_stop_idle_secs")]
    #[setting(
        label = "Auto-stop idle session (s)",
        widget = "number",
        min = 0,
        category = "Interaction",
        advanced
    )]
    pub auto_stop_idle_secs: u32,

    /// Hold an OS assertion preventing user-idle system sleep while any
    /// session is active. Released once every session has been idle past
    /// `prevent_sleep_idle_grace_minutes`. Global toggle, daemon only: the
    /// `aoe serve` status loop owns the assertion, so a user without a
    /// running daemon gets no inhibition.
    ///
    /// `global_only`: the status loop reads only the global config to drive a
    /// single process-wide assertion, so a per-profile override would be
    /// silently ignored; keep the schema honest by scoping it global.
    #[serde(default)]
    #[setting(label = "Keep OS Awake While Active", widget = "toggle", global_only)]
    pub prevent_sleep_when_active: bool,

    /// Minutes a session must stay idle before the sleep-inhibit assertion
    /// may be released. Only consulted when `prevent_sleep_when_active`.
    /// `global_only` for the same reason as that toggle.
    #[serde(default = "default_prevent_sleep_idle_grace_minutes")]
    #[setting(
        label = "Sleep-Inhibit Grace (min)",
        widget = "number",
        min = 0,
        max = 240,
        validate = "range:0:240",
        global_only,
        advanced
    )]
    pub prevent_sleep_idle_grace_minutes: u32,

    /// Text sent to the agent after a successful `aoe session restart` /
    /// `e`-keybind restart, once the post-restart readiness probe says the
    /// pane is alive. Restart re-execs the agent at a blank prompt; this
    /// nudge tells the agent to pick up where it left off. Set to an
    /// empty string to disable the wake-up message entirely (the restart
    /// itself still runs).
    #[serde(default = "default_restart_wake_message")]
    #[setting(label = "Restart Wake Message", widget = "text")]
    pub restart_wake_message: String,

    /// What to show next to each session title: Branch (default), Auto
    /// (profile in all-profiles view), None, Profile (always), or Sandbox
    /// (sb on sandboxed rows).
    #[serde(default)]
    #[setting(
        label = "Row Tag",
        widget = "select",
        options = "none:None,auto:Auto,profile:Profile,sandbox:Sandbox,branch:Branch"
    )]
    pub row_tag: RowTagMode,

    /// Comma-separated chord specs that exit live-send mode. Tmux-style: C-q,
    /// M-x, F12. The first chord in the list that matches an event ends live
    /// mode. Default `C-q` works in every terminal we ship to; add entries for
    /// additional exits if you need to send C-q through to the agent.
    #[serde(default = "default_live_send_exit_chord")]
    #[setting(
        label = "Live-Send Exit Chord",
        widget = "text",
        category = "Interaction"
    )]
    pub live_send_exit_chord: String,

    /// Leader (prefix) chord for live-send mode commands, tmux-style
    /// (`C-b`, `C-a`, `M-Space`, `F1`, …). In live mode the leader arms
    /// a one-shot menu: leader then `k` opens the command palette, `b`
    /// toggles the sidebar, `q` exits. Pressing the leader twice sends a
    /// literal leader keystroke to the agent (matches tmux `send-prefix`).
    /// Default `C-b` lines up with tmux and herdr; the only chord it
    /// steals from the agent is the leader itself, and double-tap still
    /// delivers it. Set empty to disable the leader entirely (every key,
    /// including `C-b`, then passes straight through). The dedicated exit
    /// chord (`live_send_exit_chord`, default `C-q`) is independent of the
    /// leader and stays a single-press fast exit.
    #[serde(default = "default_live_send_leader")]
    #[setting(
        label = "Live-Send Leader Chord",
        widget = "text",
        category = "Interaction"
    )]
    pub live_send_leader: String,

    /// How the TUI activates an existing terminal-mode session when you press
    /// Enter or double-click its row. `Tmux` (default) opens the tmux attach
    /// view. `LiveSend` enters live-send mode, so the home list stays visible
    /// and keystrokes go to the agent. Terminal/Tool views and acp sessions
    /// ignore this setting.
    #[serde(default)]
    #[setting(
        label = "Attach Mode",
        widget = "select",
        options = "tmux:Tmux,live_send:Live mode",
        category = "Interaction"
    )]
    pub default_attach_mode: AttachMode,

    /// How the TUI opens a terminal-mode session after it is created.
    #[serde(default)]
    #[setting(
        label = "New Session Mode",
        widget = "select",
        options = "match_default:Match default attach,tmux:Tmux,live_send:Live mode",
        category = "Interaction"
    )]
    pub new_session_mode: NewSessionMode,

    /// Automatically start live-send when switching into Terminal or Tool
    /// view, instead of requiring a separate Enter/Tab/click.
    #[serde(default)]
    #[setting(
        label = "Auto Live-Send On View Switch",
        widget = "toggle",
        category = "Interaction"
    )]
    pub live_send_on_view_switch: bool,

    /// What a single mouse click on a session row does in the Agent view. Live
    /// mode (default) enters live-send for the clicked row, the historical
    /// behavior. Select only just moves the cursor so you can read the preview
    /// without entering live-send. Double-click still activates via Default
    /// Attach Mode regardless of this setting.
    #[serde(default)]
    #[setting(
        label = "Mouse Click Action",
        widget = "select",
        options = "live_send:Live mode,select_only:Select only",
        category = "Interaction"
    )]
    pub click_action: ClickAction,

    /// Warn before quitting aoe when you press `q` on the home screen (the
    /// dialog can also turn this off). Ctrl+C always force-quits.
    #[serde(default = "default_true")]
    #[setting(label = "Confirm Before Quit", widget = "toggle", global_only)]
    pub confirm_before_quit: bool,

    /// Show an unread indicator on sessions. When on (default), a session
    /// whose turn just finished is painted in the theme's unread color until
    /// you view it (Tab into live-send or Enter to attach), and you can flag
    /// a session unread for later with `U`; unread rows also sort just below
    /// Waiting in the Attention sort. Turn this off to disable the indicator,
    /// the auto-marking, and the `U` toggle entirely.
    ///
    /// `global_only`: the gate is a single process-wide flag
    /// (`crate::session::unread_enabled`), refreshed from the active profile's
    /// resolved config, so it can't honor a per-profile override. Exposing it
    /// as profile-overridable would silently ignore the override; keep the
    /// schema honest by scoping it global.
    #[serde(default = "default_true")]
    #[setting(
        label = "Unread Session Indicator",
        widget = "toggle",
        category = "Interaction",
        global_only
    )]
    pub unread_indicator: bool,

    /// Show per-session color labels: the colored dot on sidebar rows and the
    /// `Color` section in the session context menu. Web dashboard only; the TUI
    /// does not render session colors. Turning this off hides them without
    /// forbidding anything, `aoe session color` and the REST endpoint keep
    /// working and stored values are preserved, so flipping back reveals them
    /// again.
    ///
    /// `global_only`: the dashboard resolves one settings object for the whole
    /// client, not one per workspace, and the sidebar mixes sessions from
    /// several profiles in the all-profiles view. A per-profile override would
    /// advertise semantics the web cannot honor.
    #[serde(default = "default_true")]
    #[setting(
        label = "Session Color Labels",
        widget = "toggle",
        category = "Interaction",
        global_only
    )]
    pub show_session_colors: bool,

    /// Pin favorited sessions to the top of their sibling scope in every sort
    /// order of the TUI session list, not just Attention. A group holding a
    /// favorited session is pinned the same way. When on (default), favoriting
    /// is a general "keep this where I can find it" marker; when off, the star
    /// only biases the Attention sort (its tier-local tiebreak) and the
    /// favorite key is inert everywhere else, which is the pre-1.14 behavior.
    ///
    /// Governs the TUI list only. The web dashboard keeps favorite as a
    /// within-tier Attention signal and has its own pin control.
    ///
    /// `global_only`: read through a single process-wide flag
    /// (`crate::session::favorites_first`) on the sort hot path, so a
    /// per-profile override could not be honored. Same reasoning as
    /// `unread_indicator`.
    #[serde(default = "default_true")]
    #[setting(
        label = "Favorites First",
        widget = "toggle",
        category = "Interaction",
        global_only
    )]
    pub favorites_first: bool,

    /// Show occasional discovery tips: the `💡` badge in the footer, the
    /// browsable tips overlay, and the one-time earned pop. Turn this off to
    /// hide the badge and stop tips from popping; seen/earned state still lives
    /// in `app_state`. A global UX preference, not profile-overridable. See
    /// [`crate::tips`].
    #[serde(default = "default_true")]
    #[setting(
        label = "Show tips",
        widget = "toggle",
        category = "Interaction",
        global_only
    )]
    pub show_tips: bool,

    /// Keep an aoe-managed worktree session's directory leaf in sync with its
    /// title. When enabled (default), renaming the session also moves its
    /// worktree directory, and new sessions derive the directory leaf from the
    /// title. Renaming a tied worktree session requires it to be stopped first.
    /// The git branch is never swept in; it keeps its own opt-in toggle.
    /// No-op for non-worktree sessions.
    #[serde(default = "default_true")]
    #[setting(
        label = "Tie Worktree Directory to Session Name",
        widget = "toggle",
        category = "Worktree"
    )]
    pub tie_workdir_to_name: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpAgentDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// Default structured view mode (ACP `category:"mode"` config option
    /// value). Applied after `session/new` only when the agent advertises a
    /// live mode option; a stale value no-ops with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,

    /// Per-model effort overrides, keyed by the exact ACP model option value.
    /// Takes precedence over the flat `effort` when the resolved model matches
    /// a key; falls back to `effort` otherwise.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub effort_by_model: HashMap<String, String>,
}

impl AcpAgentDefaults {
    pub fn is_empty(&self) -> bool {
        self.model.as_deref().is_none_or(str::is_empty)
            && self.effort.as_deref().is_none_or(str::is_empty)
            && self.mode.as_deref().is_none_or(str::is_empty)
            && self.effort_by_model.is_empty()
    }

    /// Default model, with empty strings treated as unset (mirrors `mode`) so a
    /// blank value never overrides the agent's own default at spawn.
    pub fn model(&self) -> Option<String> {
        self.model.clone().filter(|value| !value.is_empty())
    }

    /// Default mode, with empty strings treated as unset (mirrors
    /// `effort_for_model`) so a blank value never triggers a pointless ACP
    /// config update on spawn.
    pub fn mode(&self) -> Option<String> {
        self.mode.clone().filter(|value| !value.is_empty())
    }

    /// Effort for a resolved model: the per-model override wins when the model
    /// matches a key, otherwise the flat `effort`. Empty strings are treated as
    /// unset.
    pub fn effort_for_model(&self, model: Option<&str>) -> Option<String> {
        if let Some(model) = model {
            if let Some(effort) = self
                .effort_by_model
                .get(model)
                .filter(|value| !value.is_empty())
            {
                return Some(effort.clone());
            }
        }
        self.effort.clone().filter(|value| !value.is_empty())
    }
}

impl AcpConfig {
    /// The agent name a structured-view spawn falls back to when nothing more
    /// specific applies. A hand-edited config can leave `default_agent` blank
    /// (the settings layer rejects empty, a file edit does not), which would
    /// otherwise resolve to `UnknownAgent("")`.
    pub fn resolved_default_agent(&self) -> &str {
        let configured = self.default_agent.trim();
        if configured.is_empty() {
            DEFAULT_ACP_AGENT
        } else {
            configured
        }
    }

    pub fn acp_defaults_for(&self, agent: &str) -> Option<&AcpAgentDefaults> {
        self.acp_defaults
            .get(agent)
            .filter(|defaults| !defaults.is_empty())
    }
}

/// Resolve the model + effort a structured-view spawn should use: an explicit
/// per-request value (trimmed, non-empty) always wins, otherwise the per-agent
/// structured-view default. Effort is keyed on the resolved model so a
/// per-model override in `effort_by_model` applies to a defaulted model too.
///
/// Single source for every spawn path (CLI create, reconciler respawn, web
/// create); see `AcpConfig::acp_defaults_for`.
pub fn resolve_spawn_model_effort(
    defaults: Option<&AcpAgentDefaults>,
    req_model: Option<String>,
    req_effort: Option<String>,
) -> (Option<String>, Option<String>) {
    let model = req_model
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| defaults.and_then(|d| d.model()));
    let effort = req_effort
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| defaults.and_then(|d| d.effort_for_model(model.as_deref())));
    (model, effort)
}

/// What a single mouse click on a session row does in the Agent view.
/// See `SessionConfig::click_action`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClickAction {
    /// Single-click enters live-send mode for the clicked session
    /// (the historical behavior on `main` before this setting landed).
    #[default]
    LiveSend,
    /// Single-click only moves the cursor to the clicked row, so the
    /// user can browse session previews without entering live-send.
    /// Double-click still activates the session via the configured
    /// `default_attach_mode`.
    SelectOnly,
}

/// How the TUI opens a terminal-mode session after creation. `MatchDefault`
/// preserves the historical behavior by using
/// `SessionConfig::default_attach_mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewSessionMode {
    #[default]
    MatchDefault,
    Tmux,
    LiveSend,
}

/// How the TUI activates an existing terminal-mode session. See
/// `SessionConfig::default_attach_mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    /// Attach to the session's tmux pane (the historical behavior; the
    /// user lands inside tmux with the agent running).
    #[default]
    Tmux,
    /// Enter live-send mode against the session's pane: the agent runs
    /// in the background, the TUI stays on the home view, and
    /// keystrokes pipe straight to the agent. Users who never want to
    /// see a raw tmux session pick this.
    LiveSend,
}

/// What to render in the per-row tag slot next to the session title.
///
/// Defaults to `Branch` to preserve worktree branch visibility. Users can pick
/// `None` to hide the suffix, `Auto` for profile tags in all-profiles view,
/// `Profile`, or `Sandbox`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowTagMode {
    /// Never render suffix metadata next to the session title.
    None,
    /// Show the profile short code in all-profiles view, nothing in
    /// filtered views.
    Auto,
    /// Always render the profile short code (`fb` for `forit-backup`).
    Profile,
    /// Render `sb` on sandboxed sessions, nothing on host sessions.
    Sandbox,
    /// Render the worktree or workspace branch name, compacted into the row tag.
    #[default]
    Branch,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            default_tool: None,
            yolo_mode_default: false,
            pre_trust_agent_folders: false,
            show_diagnostics_pane: false,
            inherit_host_environment: false,
            agent_extra_args: HashMap::new(),
            agent_command_override: HashMap::new(),
            agent_status_hooks: true,
            merge_hooks_into_selected_agent: true,
            conversation_summary: false,
            smart_rename: true,
            smart_rename_agent: String::new(),
            smart_rename_model: HashMap::new(),
            auto_resume_on_restart: true,
            opencode_preassign_session_id: false,
            mouse_capture: true,
            custom_agents: HashMap::new(),
            agent_detect_as: HashMap::new(),
            agent_config_dir: HashMap::new(),
            agent_acp_cmd: HashMap::new(),
            strict_hotkeys: false,
            snooze_duration_minutes: 30,
            delete_to_trash: true,
            confirm_delete: true,
            trash_retention_days: default_trash_retention_days(),
            auto_stop_idle_secs: default_auto_stop_idle_secs(),
            prevent_sleep_when_active: false,
            prevent_sleep_idle_grace_minutes: default_prevent_sleep_idle_grace_minutes(),
            restart_wake_message: default_restart_wake_message(),
            row_tag: RowTagMode::default(),
            live_send_exit_chord: default_live_send_exit_chord(),
            live_send_leader: default_live_send_leader(),
            default_attach_mode: AttachMode::default(),
            new_session_mode: NewSessionMode::default(),
            live_send_on_view_switch: false,
            click_action: ClickAction::default(),
            confirm_before_quit: true,
            unread_indicator: true,
            show_session_colors: true,
            favorites_first: true,
            show_tips: true,
            tie_workdir_to_name: true,
        }
    }
}

fn default_snooze_duration_minutes() -> u32 {
    30
}

fn default_trash_retention_days() -> u32 {
    30
}

fn default_restart_wake_message() -> String {
    "wake up: pick up what you were doing".to_string()
}

fn default_live_send_exit_chord() -> String {
    // Ctrl+q: mobile-friendly, passes Termius, well-known quit chord.
    // Kept in sync with live_send::DEFAULT_EXIT_CHORD.
    "C-q".to_string()
}

fn default_live_send_leader() -> String {
    // Ctrl+b: the tmux (and herdr) leader. Familiar to multiplexer users
    // and steals only one chord from the agent. Kept in sync with
    // live_send::DEFAULT_LEADER.
    "C-b".to_string()
}

/// Upper bound on snooze duration: 30 days (43,200 minutes). Originally
/// capped at 24 hours but the TUI snooze dialog now offers up to a 1-week
/// preset and longer ad-hoc values via the API are reasonable for
/// long-tail "circle back next month" workflows.
pub const SNOOZE_MAX_MINUTES: u64 = 30 * 24 * 60;

pub fn validate_snooze_duration(minutes: u64) -> Result<(), String> {
    if !(1..=SNOOZE_MAX_MINUTES).contains(&minutes) {
        return Err(format!(
            "Snooze duration must be between 1 and {} minutes (got {})",
            SNOOZE_MAX_MINUTES, minutes
        ));
    }
    Ok(())
}

pub fn validate_auto_stop_idle_secs(secs: u64) -> Result<(), String> {
    if secs > u32::MAX as u64 {
        return Err(format!(
            "Auto-stop idle seconds must be at most {} (got {})",
            u32::MAX,
            secs
        ));
    }
    Ok(())
}

/// The forms [`SessionConfig::agent_config_dir_for`] resolves: `~`, a `~/`
/// path, or a path already absolute for this platform (so a Windows
/// `C:\Users\me\.claude` counts and `~bob/.claude` does not). Warnings and
/// the settings editor both gate on this so no value they accept is silently
/// dropped at resolution.
pub fn is_resolvable_agent_config_dir(dir: &str) -> bool {
    dir == "~" || dir.starts_with("~/") || Path::new(dir).is_absolute()
}

impl SessionConfig {
    /// Resolve the command override for a tool, checking agent_command_override first,
    /// then falling back to custom_agents. Returns empty string if no override found.
    pub fn resolve_tool_command(&self, tool: &str) -> String {
        self.agent_command_override
            .get(tool)
            .filter(|s| !s.is_empty())
            .or_else(|| self.custom_agents.get(tool))
            .cloned()
            .unwrap_or_default()
    }

    /// The `agent_config_dir` entry for `tool`, with a leading `~` expanded.
    ///
    /// `tool` is the name the session runs, so a custom agent is looked up
    /// under its own name and a built-in one under the registry name they
    /// share. Relative paths are rejected: the value is written to, and a path
    /// resolved against AoE's working directory would be a surprise.
    pub fn agent_config_dir_for(&self, tool: &str, home: &std::path::Path) -> Option<PathBuf> {
        let dir = self.agent_config_dir.get(tool).filter(|d| !d.is_empty())?;
        match dir.as_str() {
            "~" => Some(home.to_path_buf()),
            _ => match dir.strip_prefix("~/") {
                Some(rest) => Some(home.join(rest)),
                None => Path::new(dir).is_absolute().then(|| PathBuf::from(dir)),
            },
        }
    }

    /// Log warnings for misconfigured custom agent entries.
    /// Called after config load to surface TOML editing mistakes.
    pub fn warn_custom_agent_issues(&self) {
        for (name, command) in &self.custom_agents {
            if name.is_empty() {
                tracing::warn!(target: "session.store", "custom_agents: entry with empty name will be ignored");
            }
            if command.is_empty() {
                tracing::warn!(target: "session.store",
                    "custom_agents: '{}' has an empty command, session will launch with no command",
                    name
                );
            }
            if crate::agents::get_agent(name).is_some() {
                tracing::warn!(target: "session.store",
                    "custom_agents: '{}' shadows a built-in agent; use agent_command_override instead",
                    name
                );
            }
        }
        for (name, target) in &self.agent_detect_as {
            if name.is_empty() {
                tracing::warn!(target: "session.store", "agent_detect_as: entry with empty agent name will be ignored");
            }
            if target.is_empty() {
                tracing::warn!(target: "session.store",
                    "agent_detect_as: '{}' maps to an empty target, status detection will default to Idle",
                    name
                );
            } else if crate::agents::get_agent(target).is_none() {
                tracing::warn!(target: "session.store",
                    "agent_detect_as: '{}' maps to unknown agent '{}', status detection will default to Idle. Known agents: {}",
                    name,
                    target,
                    crate::agents::agent_names().join(", ")
                );
            }
        }
        for (name, command) in &self.agent_acp_cmd {
            if name.is_empty() {
                tracing::warn!(target: "session.store", "agent_acp_cmd: entry with empty agent name will be ignored");
                continue;
            }
            if crate::agents::get_agent(name).is_some() {
                tracing::warn!(target: "session.store",
                    "agent_acp_cmd: '{}' shadows a built-in agent; built-in agents already have an acp adapter and the entry will be ignored",
                    name
                );
                continue;
            }
            if !self.custom_agents.contains_key(name) {
                tracing::warn!(target: "session.store",
                    "agent_acp_cmd: '{}' has no matching custom_agents entry; it will not appear in the agent picker",
                    name
                );
            }
            match shell_words::split(command) {
                Ok(argv) if argv.is_empty() => {
                    tracing::warn!(target: "session.store",
                        "agent_acp_cmd: '{}' has an empty command, acp will be unavailable for it",
                        name
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(target: "session.store",
                        "agent_acp_cmd: '{}' has a malformed command ({}), acp will be unavailable for it",
                        name, e
                    );
                }
            }
        }
        for (name, dir) in &self.agent_config_dir {
            if name.is_empty() {
                tracing::warn!(target: "session.store", "agent_config_dir: entry with empty agent name will be ignored");
            } else if dir.is_empty() {
                tracing::warn!(target: "session.store",
                    "agent_config_dir: '{}' has an empty directory and will be ignored", name);
            } else if !is_resolvable_agent_config_dir(dir) {
                tracing::warn!(target: "session.store",
                    "agent_config_dir: '{}' maps to unresolvable path '{}'; use an absolute path, ~ or ~/, the entry will be ignored",
                    name, dir
                );
            }
        }
    }
}

/// Diff view configuration
#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "diff", category = "Diff")]
pub struct DiffConfig {
    /// Default branch to compare against (e.g., "main", "master")
    /// If not set, will try to auto-detect from the repository
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(label = "Default Branch", widget = "optional_text")]
    pub default_branch: Option<String>,

    /// Number of context lines to show around changes
    #[serde(default = "default_context_lines")]
    #[setting(label = "Context Lines", widget = "number", min = 0)]
    pub context_lines: usize,

    /// Render diffs side-by-side (split) instead of unified.
    #[serde(default)]
    #[setting(label = "Side-by-side diff", widget = "toggle")]
    pub split_view: bool,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            default_branch: None,
            context_lines: 3,
            split_view: false,
        }
    }
}

fn default_context_lines() -> usize {
    3
}

/// Web dashboard runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "web", category = "Web")]
pub struct WebConfig {
    /// Allow the web dashboard to deliver browser push notifications
    /// (server-wide kill switch). When false, `/api/push/*` returns 404 and
    /// the status-change consumer drops events without sending. Existing
    /// subscriptions persist across flips, so toggling back to true resumes
    /// delivery without requiring users to re-opt-in.
    #[serde(default = "default_true")]
    #[setting(label = "Push notifications", widget = "toggle", global_only)]
    pub notifications_enabled: bool,

    /// Default: send a push when a session transitions Running to Waiting
    /// (agent is asking for input). Sessions can override individually.
    #[serde(default = "default_true")]
    #[setting(label = "Notify on waiting", widget = "toggle", global_only)]
    pub notify_on_waiting: bool,

    /// Default: send a push when a session finishes (Running to Idle). Off by
    /// default because short sessions make this noisy; sessions can opt in
    /// individually.
    #[serde(default)]
    #[setting(label = "Notify on idle", widget = "toggle", global_only)]
    pub notify_on_idle: bool,

    /// Default: send a push when a session errors (Running to Error).
    #[serde(default = "default_true")]
    #[setting(label = "Notify on error", widget = "toggle", global_only)]
    pub notify_on_error: bool,

    /// Default: send a push when an acp session's ScheduleWakeup timer
    /// fires (the next /loop turn starts). Suppressed if the TUI or web
    /// dashboard has been active in the last 30s. See #1091.
    #[serde(default = "default_true")]
    #[setting(label = "Notify on scheduled wake", widget = "toggle", global_only)]
    pub notify_on_wake_fire: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            notifications_enabled: true,
            notify_on_waiting: true,
            notify_on_idle: false,
            notify_on_error: true,
            notify_on_wake_fire: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "auth", category = "Web")]
pub struct AuthConfig {
    /// Keep dashboard login sessions across `aoe serve` restarts. When on,
    /// signed-in devices stay signed in after a daemon restart instead of
    /// being re-prompted for the passphrase; sessions are stored owner-only
    /// (0600) under the app dir and dropped if the passphrase changes. Turn
    /// off to make every restart force re-authentication. See #1235.
    #[serde(default = "default_true")]
    #[setting(label = "Persist login sessions", widget = "toggle", global_only)]
    pub persist_sessions: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            persist_sessions: true,
        }
    }
}

/// Serde default for `Config.default_profile`. Empty means "not explicitly
/// chosen"; the active profile is then resolved at runtime by
/// `resolve_default_profile`, which picks the first existing profile or
/// bootstraps one. There is no magic profile name.
fn default_profile() -> String {
    String::new()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    /// Emit 24-bit RGB escapes (\e[38;2;R;G;Bm). Default; best fidelity on
    /// modern terminals and SSH sessions that pass RGB correctly.
    #[default]
    Truecolor,
    /// Emit 256-palette escapes (`\e[38;5;<idx>m`) by converting every theme
    /// Rgb(r,g,b) to the nearest xterm-256 index. Use this when the transport
    /// (notably some mosh clients) mishandles 24-bit RGB; preview panes in
    /// aoe already use 256-palette via ansi-to-tui, so palette mode renders
    /// chrome through the same escape path and survives the same transports.
    Palette,
}

#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "theme", category = "Theme")]
pub struct ThemeConfig {
    /// Color theme for the TUI. A global preference: one theme paints every
    /// surface regardless of the active session profile (see
    /// `config::resolve_theme_name`), so it is not profile-overridable.
    #[serde(default)]
    #[setting(label = "Theme", widget = "custom:theme-name", global_only)]
    pub name: String,
    /// Truecolor (24-bit RGB) or palette (xterm-256). Use palette if your
    /// terminal mangles RGB escapes. Global, like the theme itself.
    #[serde(default)]
    #[setting(
        label = "Color Mode",
        widget = "select",
        options = "truecolor:truecolor,palette:palette",
        global_only
    )]
    pub color_mode: ColorMode,
    /// Off by default (0). Set a positive value to opt in: a freshly-stopped
    /// Idle session keeps a fresh-idle tint and an animated breathe icon for
    /// this many minutes before snapping back to the static look, and is
    /// treated as actionable by the `w` keybind. The time-since-stop column
    /// on Idle rows shows regardless of this setting.
    #[serde(default = "default_idle_decay_minutes")]
    #[setting(label = "Idle Decay (minutes)", widget = "number", min = 0)]
    pub idle_decay_minutes: u64,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            color_mode: ColorMode::default(),
            idle_decay_minutes: default_idle_decay_minutes(),
        }
    }
}

fn default_idle_decay_minutes() -> u64 {
    0
}

/// Controls how the TUI and CLI surface update availability. See #1140.
///
/// `Auto` quietly installs new releases in the background on next launch
/// after detection (mid-session restart is intentionally out of scope, the
/// new binary is picked up next time `aoe` starts). `Notify` is the
/// default: shows the TUI banner and the CLI eprintln nag. `Off`
/// suppresses every check, banner, and fetch.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UpdateCheckMode {
    /// Silently install detected updates; user picks them up next launch.
    Auto,
    /// Surface the banner / CLI notice (default).
    #[default]
    Notify,
    /// Skip every check, banner, and fetch.
    Off,
}

impl UpdateCheckMode {
    /// True when the runtime should call `check_for_update` at all.
    /// Both `Auto` and `Notify` need the check to fire; only `Off`
    /// short-circuits.
    pub fn is_enabled(self) -> bool {
        !matches!(self, UpdateCheckMode::Off)
    }

    /// True when the user should see a TUI banner / CLI notice once a
    /// newer version is detected.
    pub fn notifies(self) -> bool {
        matches!(self, UpdateCheckMode::Notify)
    }

    /// True when the runtime should kick off a background install on
    /// detection (no banner; binary picked up next launch).
    pub fn auto_installs(self) -> bool {
        matches!(self, UpdateCheckMode::Auto)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "updates", category = "Updates")]
pub struct UpdatesConfig {
    /// auto = install in background on detection (picked up next launch).
    /// notify = show banner / CLI notice (default). off = skip every check.
    #[serde(default)]
    #[setting(
        label = "Update Check Mode",
        widget = "select",
        options = "auto:auto,notify:notify,off:off"
    )]
    pub update_check_mode: UpdateCheckMode,

    /// Auto-update installed external plugins at TUI and `aoe serve` startup.
    /// Off by default. The sweep applies only updates that need no new consent;
    /// any version that changes capabilities, build steps, or UI slots is left
    /// for a manual `aoe plugin update` so its new grant is reviewed.
    // global_only: the startup sweep reads the global config
    // (`Config::load_or_warn`), so a profile/repo override would be silently
    // ignored; show it but do not offer non-global scopes.
    #[serde(default)]
    #[setting(label = "Auto-update plugins", widget = "toggle", global_only)]
    pub auto_update_plugins: bool,
}

fn default_true() -> bool {
    true
}

/// Cross-agent sharing of AoE-managed skills. Off by default, and deliberately
/// so: turning it on lets AoE create, replace, and remove directories inside the
/// user's real agent config dirs (`~/.claude/skills` and friends). That is a
/// distinct privilege from editing AoE's own store, so it is opt-in rather than
/// opt-out; an upgrade must never start writing there on its own.
///
/// AoE only ever touches copies it deployed and that are still byte-identical to
/// what it deployed, so a hand-written skill, or a propagated one the user has
/// since edited, is preserved.
#[derive(Debug, Clone, Default, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "skills", category = "Skills")]
pub struct SkillsConfig {
    /// Copy AoE-managed skills into an agent's own skills directory when
    /// launching a session for it, so a skill authored once in AoE is available
    /// to every agent. Defaults to `false`.
    #[serde(default)]
    #[setting(
        label = "Share skills with agents",
        widget = "toggle",
        web = "elevation:writes into the agent config directories in your home dir",
        global_only
    )]
    pub auto_propagate: bool,
}

/// Anonymous, opt-in usage telemetry. Off by default; mirrors the privacy
/// posture of [`UpdatesConfig`] (the only other outbound call in the tool).
///
/// The single `enabled` flag is the consent boundary the user controls in
/// every settings surface. The anonymous install id lives in a dedicated
/// `telemetry.json` (NOT here), so pasting `config.toml` into a bug report
/// can never leak it. `DO_NOT_TRACK` overrides this flag at runtime and
/// suppresses both sending and id generation regardless of its value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "telemetry", category = "Telemetry")]
pub struct TelemetryConfig {
    /// User opted in to anonymous usage telemetry. Defaults to `false`.
    #[serde(default)]
    #[setting(label = "Anonymous usage telemetry", widget = "toggle", global_only)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "worktree", category = "Worktree")]
pub struct WorktreeConfig {
    /// Enable worktree mode by default for new sessions.
    #[serde(default)]
    #[setting(
        label = "Enabled by Default",
        widget = "toggle",
        web = "elevation:worktree config affects host filesystem"
    )]
    pub enabled: bool,

    /// Template for worktree paths ({repo-name}, {branch}).
    #[serde(default = "default_worktree_template")]
    #[setting(
        label = "Path Template",
        widget = "text",
        web = "elevation:worktree config affects host filesystem"
    )]
    pub path_template: String,

    /// Template for bare repo worktree paths. Defaults to "./{branch}" to keep
    /// worktrees as siblings within the repo directory.
    #[serde(default = "default_bare_repo_template")]
    #[setting(
        label = "Bare Repo Template",
        widget = "text",
        web = "elevation:worktree config affects host filesystem",
        advanced
    )]
    pub bare_repo_path_template: String,

    /// Automatically clean up worktrees on session delete.
    #[serde(default = "default_true")]
    #[setting(
        label = "Auto Cleanup",
        widget = "toggle",
        web = "elevation:worktree config affects host filesystem"
    )]
    pub auto_cleanup: bool,

    /// Also delete the git branch when deleting a worktree. Default: false
    /// (unchecked in the delete dialog).
    #[serde(default)]
    #[setting(
        label = "Delete Branch on Cleanup",
        widget = "toggle",
        web = "elevation:worktree config affects host filesystem",
        advanced
    )]
    pub delete_branch_on_cleanup: bool,

    /// Template for multi-repo workspace directories ({branch}, {session-id}).
    #[serde(default = "default_workspace_template")]
    #[setting(
        label = "Workspace Path Template",
        widget = "text",
        web = "elevation:worktree config affects host filesystem",
        advanced
    )]
    pub workspace_path_template: String,

    /// Run `git submodule update --init --recursive` after creating a worktree
    /// when the checkout contains a `.gitmodules` file. Disable for repos with
    /// large or deeply-nested submodule trees that you don't need inside agent
    /// sessions; new sessions then finish creating instead of stalling in
    /// `Creating` while submodules clone.
    #[serde(default = "default_true")]
    #[setting(
        label = "Init Submodules",
        widget = "toggle",
        web = "elevation:worktree config affects host filesystem",
        advanced
    )]
    pub init_submodules: bool,

    /// Default base branch for new worktree branches. When empty, falls back to
    /// the repository's detected default branch. A per-project entry in the
    /// registry, or an explicit base branch supplied at session creation,
    /// takes precedence over this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "Default Base Branch",
        widget = "optional_text",
        web = "elevation:worktree config affects host filesystem",
        advanced
    )]
    pub default_base_branch: Option<String>,
}

impl WorktreeConfig {
    /// Branch cleanup only makes sense when the worktree is also removed; a
    /// preserved worktree keeps its branch checked out, so deleting it would
    /// fail (#2532). Single source for that invariant across the cleanup
    /// default and auto-purge call sites.
    pub fn should_delete_branch_on_cleanup(&self) -> bool {
        self.auto_cleanup && self.delete_branch_on_cleanup
    }
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path_template: default_worktree_template(),
            bare_repo_path_template: default_bare_repo_template(),
            auto_cleanup: true,
            delete_branch_on_cleanup: false,
            workspace_path_template: default_workspace_template(),
            init_submodules: true,
            default_base_branch: None,
        }
    }
}

fn default_worktree_template() -> String {
    "../{repo-name}-worktrees/{branch}".to_string()
}

fn default_bare_repo_template() -> String {
    "./{branch}".to_string()
}

fn default_workspace_template() -> String {
    "../{branch}-workspace-{session-id}".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "sandbox", category = "Sandbox")]
pub struct SandboxConfig {
    /// Enable sandbox mode by default for new sessions.
    #[serde(default)]
    #[setting(
        label = "Enabled by Default",
        widget = "toggle",
        web = "elevation:sandbox config affects host isolation",
        repo = "deny"
    )]
    pub enabled_by_default: bool,

    /// Container image to use for sandboxes.
    #[serde(default = "default_sandbox_image")]
    #[setting(
        label = "Default Image",
        widget = "text",
        web = "elevation:sandbox config affects host isolation",
        repo = "deny"
    )]
    pub default_image: String,

    /// Additional volume mounts (host:container or host:container:ro).
    #[serde(default, deserialize_with = "super::serde_helpers::string_or_vec")]
    #[setting(
        label = "Extra Volumes",
        widget = "list",
        validate = "volume_list",
        web = "elevation:sandbox config affects host isolation",
        repo = "deny",
        advanced
    )]
    pub extra_volumes: Vec<String>,

    /// Env vars injected into the container: KEY=value (literal, appears in
    /// argv), KEY=$VAR (passthrough from host, hidden from argv), KEY=$$literal
    /// (escape a leading $), or bare KEY (passthrough). For host (non-sandboxed)
    /// sessions, see Session > Host Environment instead.
    #[serde(
        default = "default_sandbox_environment",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    #[setting(
        label = "Sandbox Environment",
        widget = "list",
        validate = "env_list",
        web = "elevation:sandbox config affects host isolation",
        advanced
    )]
    pub environment: Vec<String>,

    /// Remove containers when sessions are deleted.
    #[serde(default = "default_true")]
    #[setting(
        label = "Auto Cleanup",
        widget = "toggle",
        web = "elevation:sandbox config affects host isolation"
    )]
    pub auto_cleanup: bool,

    /// CPU limit for containers (e.g. "4").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "CPU Limit",
        widget = "optional_text",
        web = "elevation:sandbox config affects host isolation",
        advanced
    )]
    pub cpu_limit: Option<String>,

    /// Memory limit for containers (e.g. "8g", "512m").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "Memory Limit",
        widget = "optional_text",
        validate = "memory_limit",
        web = "elevation:sandbox config affects host isolation",
        advanced
    )]
    pub memory_limit: Option<String>,

    /// Expose container ports to host (e.g. 3000:3000).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    #[setting(
        label = "Port Mappings",
        widget = "list",
        validate = "port_mapping_list",
        web = "elevation:sandbox config affects host isolation",
        advanced
    )]
    pub port_mappings: Vec<String>,

    /// Container network mode: unset or "bridge" for the default (full outbound
    /// via the runtime's bridge), "none" for no network (isolates the agent but
    /// also cuts off its own model API unless a proxy is routed in), or a named
    /// network to attach a user-defined network with its own egress filtering.
    /// "host" and namespace-sharing forms ("container:", "ns:") are rejected
    /// because they defeat sandbox isolation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "Network",
        widget = "optional_text",
        validate = "network",
        web = "elevation:sandbox config affects host isolation",
        advanced
    )]
    pub network: Option<String>,

    /// Default terminal for sandboxed sessions (toggle with 'c' key).
    #[serde(default)]
    #[setting(
        label = "Default Terminal Mode",
        widget = "select",
        options = "host:Host,container:Container",
        web = "elevation:sandbox config affects host isolation"
    )]
    pub default_terminal_mode: DefaultTerminalMode,

    /// Directories to exclude from host mount (e.g. target, node_modules).
    #[serde(default, deserialize_with = "super::serde_helpers::string_or_vec")]
    #[setting(
        label = "Volume Ignores",
        widget = "list",
        web = "elevation:sandbox config affects host isolation",
        advanced
    )]
    pub volume_ignores: Vec<String>,

    /// anonymous: default, works on Linux. named: use deterministic
    /// Docker/Podman named volumes, required on macOS/VirtioFS to reliably
    /// shadow bind-mount subdirectories.
    #[serde(default)]
    #[setting(
        label = "Volume Ignores Strategy",
        widget = "select",
        options = "anonymous:anonymous,named:named",
        web = "elevation:sandbox config affects host isolation",
        advanced
    )]
    pub volume_ignores_strategy: VolumeIgnoresStrategy,

    /// Mount ~/.ssh into sandbox containers (for git SSH access).
    #[serde(default)]
    #[setting(
        label = "Mount SSH",
        widget = "toggle",
        web = "elevation:sandbox config affects host isolation",
        repo = "deny"
    )]
    pub mount_ssh: bool,

    /// Append the :z SELinux relabel flag to sandbox bind mounts (needed on
    /// Fedora/RHEL; relabels host paths). Off by default; only emitted for
    /// Docker/Podman.
    #[serde(default)]
    #[setting(
        label = "SELinux Relabel",
        widget = "toggle",
        web = "elevation:sandbox config affects host isolation",
        repo = "deny",
        advanced
    )]
    pub selinux_relabel: bool,

    /// Run the container with `--privileged`: full device access and all
    /// capabilities. Needed to run a container engine (rootless podman,
    /// dockerd) inside the sandbox.
    #[serde(default)]
    #[setting(
        label = "Privileged",
        widget = "toggle",
        web = "local_only:grants the container full host privileges",
        repo = "deny",
        advanced
    )]
    pub privileged: bool,

    /// Capabilities to add to the container (`--cap-add`), e.g. "SYS_ADMIN".
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    #[setting(
        label = "Add Capabilities",
        widget = "list",
        validate = "capability_list",
        web = "local_only:grants the container extra Linux capabilities",
        repo = "deny",
        advanced
    )]
    pub cap_add: Vec<String>,

    /// Capabilities to drop from the container (`--cap-drop`), e.g. "ALL".
    /// A write replaces rather than adds, so clearing it undoes hardening.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    #[setting(
        label = "Drop Capabilities",
        widget = "list",
        validate = "capability_list",
        web = "local_only:a write replaces the list, so it can undo hardening",
        repo = "deny",
        advanced
    )]
    pub cap_drop: Vec<String>,

    /// Security options for the container (`--security-opt`), e.g.
    /// "seccomp=unconfined" or "no-new-privileges:true".
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    #[setting(
        label = "Security Options",
        widget = "list",
        validate = "security_opt_list",
        web = "local_only:relaxes the container security profile",
        repo = "deny",
        advanced
    )]
    pub security_opt: Vec<String>,

    /// Extra arguments appended verbatim to the container `run` invocation,
    /// before the image. Unvalidated, so entries bypass `sandbox.network`'s
    /// refusal of `host`.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    #[setting(
        label = "Extra Run Args",
        widget = "list",
        web = "local_only:arbitrary container runtime argv, a host execution surface",
        repo = "deny",
        advanced
    )]
    pub extra_run_args: Vec<String>,

    /// Custom instruction text appended to the agent's system prompt in
    /// sandboxed sessions (Claude, Codex only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "Custom Instruction",
        widget = "optional_text",
        web = "elevation:sandbox config affects host isolation",
        advanced
    )]
    pub custom_instruction: Option<String>,

    /// Container runtime for sandboxing.
    #[serde(default)]
    #[setting(
        label = "Container Runtime",
        widget = "select",
        options = "docker:Docker,podman:Podman,apple_container:Apple Container",
        web = "elevation:sandbox config affects host isolation",
        global_only
    )]
    pub container_runtime: ContainerRuntimeName,
}

/// Container runtime options for sandboxing
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntimeName {
    AppleContainer,
    #[default]
    Docker,
    Podman,
}

/// Volume mounting strategy for volume_ignores paths.
///
/// On macOS with Docker Desktop's VirtioFS, anonymous volumes don't always shadow
/// bind-mount subdirectories. Use `Named` to mount deterministic named Docker/Podman
/// volumes instead, which live in the Docker VM and bypass VirtioFS reliably.
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeIgnoresStrategy {
    /// Use anonymous volumes (default; works on Linux; may not shadow on macOS/VirtioFS)
    #[default]
    Anonymous,
    /// Use deterministic named volumes (required on macOS/VirtioFS; explicitly cleaned up on session delete)
    Named,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled_by_default: false,
            default_image: default_sandbox_image(),
            extra_volumes: Vec::new(),
            environment: default_sandbox_environment(),
            auto_cleanup: true,
            cpu_limit: None,
            memory_limit: None,
            port_mappings: Vec::new(),
            network: None,
            default_terminal_mode: DefaultTerminalMode::default(),
            volume_ignores: Vec::new(),
            volume_ignores_strategy: VolumeIgnoresStrategy::default(),
            mount_ssh: false,
            selinux_relabel: false,
            privileged: false,
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            security_opt: Vec::new(),
            extra_run_args: Vec::new(),
            custom_instruction: None,
            container_runtime: ContainerRuntimeName::default(),
        }
    }
}

fn default_sandbox_image() -> String {
    "ghcr.io/agent-of-empires/aoe-sandbox:latest".to_string()
}

fn default_sandbox_environment() -> Vec<String> {
    crate::session::environment::DEFAULT_TERMINAL_ENV_VARS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Default terminal mode for sandboxed sessions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DefaultTerminalMode {
    /// Default to host terminal (shell on the host machine)
    #[default]
    Host,
    /// Default to container terminal (shell inside the Docker container)
    Container,
}

/// The three-way mode shared by every tmux option aoe manages on its own
/// sessions: always apply aoe's value, never apply it, or decide from the
/// user's own tmux config.
///
/// One enum for all of them, because a per-setting copy invites exactly the
/// drift #3207 was: `mouse` grew a tri-state resolution while its siblings
/// stayed two-state. What `Auto` keys on still differs per setting, and that
/// difference is declared in one place, `TmuxSetting::row`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TmuxSettingMode {
    #[default]
    Auto,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "tmux", category = "Tmux")]
pub struct TmuxConfig {
    /// Control tmux status bar styling (Auto respects your tmux config).
    #[serde(default)]
    #[setting(
        label = "Status Bar",
        widget = "select",
        options = "auto:Auto,enabled:Enabled,disabled:Disabled"
    )]
    pub status_bar: TmuxSettingMode,

    /// Control mouse scrolling in aoe's tmux sessions (Auto steps aside only
    /// when your own tmux config sets `mouse`, and enables it otherwise).
    #[serde(default)]
    #[setting(
        label = "Mouse Support",
        widget = "select",
        options = "auto:Auto,enabled:Enabled,disabled:Disabled"
    )]
    pub mouse: TmuxSettingMode,

    /// Forward OSC 52 clipboard from agents to your terminal. Controls
    /// `set-clipboard on` and `allow-passthrough on` so OSC 52 from the wrapped
    /// agent reaches the terminal, and (unless Disabled) live-send's own copy
    /// forwarding to the host clipboard. Auto steps aside only when your own
    /// tmux config sets one of those two options.
    #[serde(default)]
    #[setting(
        label = "Clipboard Pass-through",
        widget = "select",
        options = "auto:Auto,enabled:Enabled,disabled:Disabled"
    )]
    pub clipboard: TmuxSettingMode,

    /// Run aoe's sessions on a private tmux server with this socket name (tmux
    /// `-L`), so your own `tmux ls` and hand-managed sessions stay separate
    /// from aoe's. Leave empty to share the default tmux server (the current
    /// behavior). A bare name, not a path; takes effect on the next aoe start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "Socket Name",
        widget = "optional_text",
        global_only,
        web = "local_only:changes which tmux server hosts your sessions on this machine"
    )]
    pub socket_name: Option<String>,

    /// Render native agent and tool previews from a persistent VT channel
    /// (`tmux pipe-pane` into an in-process terminal grid) instead of polling
    /// `capture-pane` and forking `send-keys` per keystroke. Terminal previews,
    /// including the web terminal, always use tmux's rendered capture and keep
    /// OSC 52 forwarding through a raw observer.
    #[serde(default = "default_true")]
    #[setting(label = "VT Live Transport", widget = "toggle", advanced, global_only)]
    pub vt_live: bool,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            status_bar: TmuxSettingMode::Auto,
            mouse: TmuxSettingMode::Auto,
            clipboard: TmuxSettingMode::Auto,
            socket_name: None,
            vt_live: true,
        }
    }
}

/// The files aoe treats as "the user's tmux config", in tmux's own search
/// order. Shared so the existence check and the option recognizer can never
/// drift to different locations.
///
/// tmux reads `$XDG_CONFIG_HOME/tmux/tmux.conf` and `~/.config/tmux/tmux.conf`
/// as separate entries, and they differ whenever `XDG_CONFIG_HOME` points
/// somewhere other than `~/.config`; missing the first is how a user with
/// `set -g mouse off` there stayed overridden (#3207).
///
/// tmux's search path also starts with `/etc/tmux.conf`, deliberately left out:
/// it is not the user's file, and some distros ship one, so folding it in would
/// make [`user_has_tmux_config`] true on every account of those machines and
/// silently drop aoe's status bar for all of them.
fn user_tmux_config_paths() -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let mut paths = Vec::with_capacity(3);
    if let Some(home) = &home {
        paths.push(home.join(".tmux.conf"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        paths.push(PathBuf::from(xdg).join("tmux").join("tmux.conf"));
    }
    if let Some(home) = &home {
        let default_xdg = home.join(".config").join("tmux").join("tmux.conf");
        if !paths.contains(&default_xdg) {
            paths.push(default_xdg);
        }
    }
    paths
}

/// Check if user has a tmux configuration file, in any of the paths
/// `user_tmux_config_paths` lists.
pub fn user_has_tmux_config() -> bool {
    user_tmux_config_paths().iter().any(|path| path.exists())
}

/// A tmux option group aoe manages on its own sessions, one variant per
/// `[tmux]` setting that writes to tmux.
///
/// Adding a managed option is a variant plus a `TmuxSetting::row` arm and an
/// entry in `TmuxSetting::ALL`, not a new `should_apply_*` helper plus an
/// edit to each of the three create paths. A setting whose application is
/// post-creation (theme painting, unsets of stale session-scoped values)
/// also needs a change in
/// [`crate::tmux::status_bar::apply_all_tmux_options`], which does not
/// iterate `ALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxSetting {
    /// aoe's themed status bar, nine session options wide.
    StatusBar,
    /// tmux's `mouse`, which is what turns a wheel scroll (or the dashboard's
    /// touch scroll) into copy-mode scrollback.
    ///
    /// A session option outranks the user's `set -g mouse ...`, so applying
    /// aoe's value unconditionally ignored the file they wrote and made
    /// `[tmux] mouse` look like a setting that did nothing (#3207). But keying
    /// `auto` on mere config *existence* would silently break touch scroll for
    /// everyone who keeps a `tmux.conf` for a prefix key or a theme and never
    /// mentions `mouse`, since tmux's own default is off. Hence the
    /// per-option `WhenUserSets` defer rule:
    ///
    /// | `[tmux] mouse` | their tmux config | result |
    /// |---|---|---|
    /// | `auto` | sets `mouse` | leave their `mouse` in charge |
    /// | `auto` | silent on `mouse` | `mouse on` |
    /// | `auto` | no config at all | `mouse on` |
    /// | `enabled` | anything | `mouse on` |
    /// | `disabled` | anything | `mouse off` |
    Mouse,
    /// OSC 52 forwarding out of the wrapped agent: `set-clipboard` plus
    /// `allow-passthrough`. Without it, "select to copy" inside the agent
    /// silently fails because tmux swallows the escape sequence (#897).
    Clipboard,
}

/// What makes [`TmuxSettingMode::Auto`] step aside for a given setting.
enum TmuxAutoDefer {
    /// Defer only when the user's own tmux config sets one of these options.
    /// The precise rule, and the only one that can honor the `set -g <option>`
    /// they actually wrote while still applying aoe's value for everyone who
    /// keeps a tmux config for unrelated reasons (#3207).
    WhenUserSets(&'static [&'static str]),
    /// Defer whenever the user has a tmux config at all. Coarse by design, for
    /// a setting that paints a whole group of options rather than mirroring
    /// one: probing nine `status*` options individually would be fuzzy, and a
    /// user who themed their own bar wants their theme, not a partial merge of
    /// it with aoe's.
    WhenUserHasAnyConfig,
}

/// One tmux `set-option` aoe writes on its own sessions, at creation.
///
/// The variant is the scope the write addresses (session `-t <target>`,
/// server `-s`, window `-w -t <target>`), so a write's scope flags follow
/// from its variant alone; the target of session/window writes is a runtime
/// parameter of the emitter, not a property of the write. Scope flags are
/// emitted explicitly (`-s`, `-w`, `-t`) rather than left to tmux's scope
/// inference, so the write is unambiguous and resilient to future inference
/// changes (same convention as `append_remain_on_exit_args`). `quiet` adds
/// tmux's `-q` (ignore unknown options), which aoe needs for
/// `allow-passthrough` on tmux < 3.3, where `allow-passthrough` does not
/// exist and the set-option call would otherwise fail the whole
/// `new-session` invocation; it is a per-write property, not a scope
/// property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxOptionWrite {
    /// `set-option [-q] -t <target> <option> <value>` on the new session.
    Session {
        option: &'static str,
        value: &'static str,
        quiet: bool,
    },
    /// `set-option [-q] -s <option> <value>` on the shared server (no target).
    Server {
        option: &'static str,
        value: &'static str,
        quiet: bool,
    },
    /// `set-option [-q] -w -t <target> <option> <value>` on the new window.
    Window {
        option: &'static str,
        value: &'static str,
        quiet: bool,
    },
}

/// One row of the managed-settings table: where the mode is read from, what
/// makes `auto` defer, and the writes aoe emits at creation (`apply`) or to
/// turn the setting off where "off" is expressible (`force_off`).
///
/// The enum variant is the row's identity; there is no `key` field because
/// nothing consumes one (the issue sketch reserved it for docs and logs).
struct TmuxManagedSetting {
    mode: fn(&TmuxConfig) -> TmuxSettingMode,
    defer: TmuxAutoDefer,
    /// Writes emitted when the setting resolves to [`TmuxSettingAction::Apply`].
    apply: &'static [TmuxOptionWrite],
    /// Writes emitted when the setting resolves to
    /// [`TmuxSettingAction::ForceOff`]; empty when "off" is not expressible.
    force_off: &'static [TmuxOptionWrite],
}

impl TmuxSetting {
    /// The settings aoe manages, in the order the create path applies them.
    /// `StatusBar` is resolved but emits nothing at creation (its write lists
    /// are empty), so the emitted order is `Mouse` then `Clipboard`, as the
    /// create path has always used. Not exhaustive by construction: a variant
    /// added without an `ALL` entry compiles and is silently skipped, so keep
    /// the list in step with the enum (a tripwire test enforces it).
    pub(crate) const ALL: [TmuxSetting; 3] = [Self::StatusBar, Self::Mouse, Self::Clipboard];

    /// The single source of truth for how each managed setting resolves and
    /// applies. Every fact of the row lives in one arm so they cannot drift
    /// apart: the config field the mode is read from, the `auto` defer rule,
    /// and the writes each action emits.
    fn row(self) -> TmuxManagedSetting {
        match self {
            // Painted after creation by `status_bar::apply_all_tmux_options`,
            // which computes the theme's values dynamically; both lists are
            // empty so creation emits nothing for it.
            Self::StatusBar => TmuxManagedSetting {
                mode: |tmux| tmux.status_bar,
                defer: TmuxAutoDefer::WhenUserHasAnyConfig,
                apply: &[],
                force_off: &[],
            },
            Self::Mouse => TmuxManagedSetting {
                mode: |tmux| tmux.mouse,
                defer: TmuxAutoDefer::WhenUserSets(&["mouse"]),
                apply: &[TmuxOptionWrite::Session {
                    option: "mouse",
                    value: "on",
                    quiet: false,
                }],
                force_off: &[TmuxOptionWrite::Session {
                    option: "mouse",
                    value: "off",
                    quiet: false,
                }],
            },
            // Both options aoe writes count: a user who set either one has
            // taken over OSC 52 forwarding, and aoe applying only the other
            // half would be a partial override of a deliberate choice. Both
            // are written defensively: programs vary in which form they emit
            // (raw OSC 52 vs the `\ePtmux;...\e\\`-wrapped form OpenCode
            // uses), so one half alone would drop the other's passthrough.
            // `force_off` stays empty: the options are server- and
            // window-scoped, so unsetting them would reach past aoe's own
            // sessions into the user's whole tmux server.
            Self::Clipboard => TmuxManagedSetting {
                mode: |tmux| tmux.clipboard,
                defer: TmuxAutoDefer::WhenUserSets(&["set-clipboard", "allow-passthrough"]),
                apply: &[
                    TmuxOptionWrite::Server {
                        option: "set-clipboard",
                        value: "on",
                        quiet: true,
                    },
                    TmuxOptionWrite::Window {
                        option: "allow-passthrough",
                        value: "on",
                        quiet: true,
                    },
                ],
                force_off: &[],
            },
        }
    }
}

/// The writes one action emits for one managed setting, straight from the
/// table. `LeaveToUser` is creation-time skip: a fresh session has no
/// session-scoped value aoe wrote, so declining to write one already leaves
/// the user's own config in charge (#3207). (The post-creation path,
/// `status_bar::apply_all_tmux_options`, actively unsets instead; that is a
/// different concern and keeps its own code.)
pub(crate) fn tmux_setting_writes(
    setting: TmuxSetting,
    action: TmuxSettingAction,
) -> &'static [TmuxOptionWrite] {
    let row = setting.row();
    match action {
        TmuxSettingAction::Apply => row.apply,
        TmuxSettingAction::ForceOff => row.force_off,
        TmuxSettingAction::LeaveToUser => &[],
    }
}

/// What aoe should do with one managed tmux setting.
///
/// Resolution is uniform across settings; *application* is not, because tmux
/// options differ in scope and not all of them can express every action. The
/// table's `apply` / `force_off` lists map the three actions like this:
///
/// | setting | `Apply` | `ForceOff` | `LeaveToUser` |
/// |---|---|---|---|
/// | `Mouse` | `mouse on` | `mouse off` | unset the session's `mouse` |
/// | `StatusBar` | paint aoe's themed bar | unset aoe's `status*` overrides | same as `ForceOff` |
/// | `Clipboard` | set both options | write nothing | write nothing |
///
/// The `StatusBar` row and the `Mouse` `LeaveToUser` column describe the
/// post-creation path (`status_bar::apply_all_tmux_options`), which paints the
/// theme and clears stale session-scoped values on existing sessions; at
/// creation only the `apply` / `force_off` lists are emitted, so `StatusBar`
/// writes nothing and `Mouse` `LeaveToUser` writes nothing.
///
/// `StatusBar` has no way to express "hide the bar": `disabled` means "stop
/// painting aoe's", which reverts to whatever the user's own config says. The
/// `Clipboard` options are server- and window-scoped rather than session-scoped
/// (`set -s set-clipboard`, `setw allow-passthrough`), so unsetting them would
/// reach past aoe's own sessions into the user's whole tmux server; declining to
/// write them is as far as aoe can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxSettingAction {
    /// Write aoe's values.
    Apply,
    /// The mode is `disabled`: turn the setting off where "off" is expressible.
    ForceOff,
    /// Leave the setting to the user's own tmux config, clearing any
    /// session-scoped value aoe wrote earlier so their global one governs.
    LeaveToUser,
}

/// Resolve one managed `[tmux]` setting against the config layer that governs.
///
/// Takes the already-resolved config so the caller decides which layer that is.
/// Every tmux call site passes the profile-merged config from
/// [`crate::session::config::profile_config::resolve_config_or_warn`]; reading the
/// global config here would silently drop a profile's `[tmux]` overrides, which
/// is the second half of #3207.
pub fn resolve_tmux_setting(setting: TmuxSetting, config: &Config) -> TmuxSettingAction {
    let row = setting.row();
    let user_defers = match row.defer {
        TmuxAutoDefer::WhenUserSets(options) => user_tmux_config_sets_any(options),
        TmuxAutoDefer::WhenUserHasAnyConfig => user_has_tmux_config(),
    };
    tmux_setting_action((row.mode)(&config.tmux), user_defers)
}

/// Pure core of [`resolve_tmux_setting`], with the filesystem probe lifted into
/// a parameter so the whole decision table is testable.
pub(crate) fn tmux_setting_action(mode: TmuxSettingMode, user_defers: bool) -> TmuxSettingAction {
    match mode {
        TmuxSettingMode::Enabled => TmuxSettingAction::Apply,
        TmuxSettingMode::Disabled => TmuxSettingAction::ForceOff,
        TmuxSettingMode::Auto if user_defers => TmuxSettingAction::LeaveToUser,
        TmuxSettingMode::Auto => TmuxSettingAction::Apply,
    }
}

/// Join tmux's `\`-continued physical lines into the logical lines it parses.
///
/// A trailing `\` continues a comment too, verified against tmux 3.6: `# note \`
/// followed by `set -g mouse on` leaves `mouse` at its default. Without the
/// join, that second physical line reads as a live `set` on its own, which is
/// the false-positive direction [`tmux_line_commands`] describes.
fn tmux_logical_lines(contents: &str) -> Vec<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    let mut lines: Vec<Cow<'_, str>> = Vec::new();
    let mut pending = String::new();
    for line in contents.lines() {
        match line.strip_suffix('\\') {
            Some(body) => pending.push_str(body),
            // The common case owns nothing: only a continued line allocates.
            None if pending.is_empty() => lines.push(Cow::Borrowed(line)),
            None => {
                pending.push_str(line);
                lines.push(Cow::Owned(std::mem::take(&mut pending)));
            }
        }
    }
    if !pending.is_empty() {
        lines.push(Cow::Owned(pending));
    }
    lines
}

/// Split one logical tmux config line into the commands it actually runs.
///
/// tmux separates commands on a line with `;` and treats a `#` that starts a
/// token as a comment to end of line; both lose that meaning inside quotes, and
/// a `\` escapes the next character so `\;` is an argument rather than a
/// separator (that is how a `bind` carries a command list). Tracking all three
/// matters for correctness, not tidiness: splitting naively on `;` makes
/// `# set -g status off; set -g mouse on` and
/// `bind m display "x" \; set -g mouse on` parse a tail as a live command, so a
/// line that never sets `mouse` reads as "the user set `mouse`" and
/// [`resolve_tmux_setting`] then steps aside and unsets the option. That
/// direction of error is the one worth engineering against, because it silently
/// removes the web dashboard's scrollback.
fn tmux_line_commands(line: &str) -> Vec<&str> {
    let mut commands = Vec::new();
    let bytes = line.as_bytes();
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    // Whether the next byte begins a token, which is the only position where
    // tmux reads `#` as a comment: line start, after whitespace, or right after
    // a command-separating `;` (`set -g status off ;# note` is a comment).
    let mut token_start = true;
    // `'`, `"`, `#`, `;`, and `\` are ASCII, so a match is never mid-codepoint
    // and slicing on `i` is safe; a multibyte char's bytes are all non-ASCII,
    // so they only ever clear `token_start`.
    for (i, &b) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            token_start = false;
            continue;
        }
        match b {
            // Single quotes suppress escaping in tmux, as in a POSIX shell.
            b'\\' if !in_single => {
                escaped = true;
                token_start = false;
            }
            b'\'' if !in_double => {
                in_single = !in_single;
                token_start = false;
            }
            b'"' if !in_single => {
                in_double = !in_double;
                token_start = false;
            }
            b'#' if !in_single && !in_double && token_start => {
                commands.push(&line[start..i]);
                return commands;
            }
            b';' if !in_single && !in_double => {
                commands.push(&line[start..i]);
                start = i + 1;
                token_start = true;
            }
            _ => token_start = b.is_ascii_whitespace(),
        }
    }
    commands.push(&line[start..]);
    commands
}

/// Does this tmux config text configure any of `options`?
///
/// Pure over the file contents so the recognizer is table-testable. Accepts the
/// four `set` aliases with any flag bundle in between (`set -g`, `set-option
/// -gq`, `setw`, `set -s`, ...), and requires the option name to match one of
/// `options` exactly, so tmux's historical `mouse-select-pane` /
/// `mouse-resize-pane` / `mouse-utf8` family does not count as configuring
/// `mouse`.
///
/// Known gaps, all of them false negatives (`Auto` still applies aoe's value and
/// overrides the user): a line reached through `source-file` is not followed, a
/// `set` nested inside a quoted argument (`if-shell '...' 'set -g mouse on'`,
/// `run-shell 'tmux set ...'`) is not unwrapped, a `set` inside a `%if` block
/// whose condition is false counts anyway, a `set` that begins a key binding
/// (`bind m set -g mouse`, the common toggle idiom) is hidden behind the `bind`
/// verb, and an explicit target (`set -t other mouse on`) misreads the target as
/// the option name because no flag here is treated as taking a value. The
/// direction is deliberate: each leaves `Auto` doing what it did unconditionally
/// before, whereas a false positive would silently strip the feature. A user in
/// one of these positions sets the mode to `disabled`.
fn tmux_config_sets_any(contents: &str, options: &[&str]) -> bool {
    tmux_logical_lines(contents).iter().any(|line| {
        tmux_line_commands(line).into_iter().any(|command| {
            let mut tokens = command.split_whitespace();
            if !tokens.next().is_some_and(|verb| {
                matches!(verb, "set" | "set-option" | "setw" | "set-window-option")
            }) {
                return false;
            }
            // The first non-flag token is the option name.
            tokens
                .find(|token| !token.starts_with('-'))
                .is_some_and(|name| options.contains(&name.trim_matches(['"', '\''])))
        })
    })
}

/// Does the user's own tmux config set any of `options`? Reads the same
/// [`user_tmux_config_paths`] that [`user_has_tmux_config`] probes.
fn user_tmux_config_sets_any(options: &[&str]) -> bool {
    user_tmux_config_paths().iter().any(|path| {
        fs::read_to_string(path)
            .map(|contents| tmux_config_sets_any(&contents, options))
            .unwrap_or(false)
    })
}

pub(crate) fn config_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("config.toml"))
}

/// Sidecar lock file name for the global `config.toml`. Lives in `<app_dir>`
/// next to `config.toml`, mirroring `storage.rs`'s `.storage.lock` /
/// `.workspace-ordering.lock` sidecars.
const CONFIG_LOCK_FILENAME: &str = ".config.lock";

/// Process-wide mutex serialising [`update_config`] calls. Paired with a
/// cross-process `flock` on [`CONFIG_LOCK_FILENAME`]; see that function and
/// the lock-layering rationale in `storage.rs`'s module docs.
///
/// Non-reentrant, and the `flock` beneath it is taken on a fresh descriptor
/// per call, so it does not re-enter either: calling [`update_config`] from
/// inside an [`update_config`] closure deadlocks against itself. Do the
/// nested work before or after the closure, not within it.
fn config_save_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl Config {
    /// Read and parse `config.toml` into a raw `toml::Table`, with the
    /// stale `app_state` section stripped. Shared prelude for [`Config::load`]
    /// and [`Config::config_ignored_keys`]; keeping the read + strip in one
    /// place stops the ignored-key probe from ever flagging a section `load`
    /// silently drops.
    fn load_raw_table() -> Result<toml::Table> {
        let path = config_path()?;
        let mut table: toml::Table = if path.exists() {
            toml::from_str(&fs::read_to_string(&path)?)?
        } else {
            toml::Table::new()
        };
        // `app_state` now lives in state.toml; strip any stale key left over
        // from before the split (or written by an out-of-date peer) so it
        // never shadows the authoritative source below.
        table.remove("app_state");
        Ok(table)
    }

    pub fn load() -> Result<Self> {
        let table = Self::load_raw_table()?;
        let mut config: Config = table.try_into()?;
        config.app_state = AppStateConfig::load()?;
        Ok(config)
    }

    /// Dotted paths of keys in the on-disk `config.toml` that `Config` does not
    /// recognize (unknown struct fields at any depth), so a typo like
    /// `[sandbox] privildged = true` surfaces instead of being silently
    /// dropped. Only called on the Ok path: `Config::load` errored out already
    /// carries the load-error text, and the two failure classes are per-file
    /// mutually exclusive (no `deny_unknown_fields`, so an unknown key never
    /// fails the load). Map-keyed sections (`agents`, `tools`, `plugins`,
    /// `session.custom_agents`, `acp.acp_defaults`, ...) never flag because
    /// their keys are entries, not struct fields; nested struct-field typos
    /// inside them still do.
    pub(crate) fn config_ignored_keys() -> Vec<String> {
        let Ok(table) = Self::load_raw_table() else {
            return Vec::new();
        };
        let mut ignored = Vec::new();
        let _ = serde_ignored::deserialize::<_, _, Config>(toml::Value::Table(table), |path| {
            ignored.push(path.to_string());
        });
        ignored
    }

    /// Like [`Config::load`], but logs a warning on failure and returns defaults
    /// instead of propagating the error.
    pub fn load_or_warn() -> Self {
        match Self::load() {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(target: "session.store", "Failed to load global config, using defaults: {e}");
                Config::default()
            }
        }
    }

    /// Effective theme name to paint, mapping the empty default to the
    /// `zinc` builtin (the default theme). Theme is a global preference (see
    /// [`resolve_theme_name`]); callers read it from the global config, never
    /// the profile-merged config.
    pub fn effective_theme_name(&self) -> String {
        if self.theme.name.is_empty() {
            "zinc".to_string()
        } else {
            self.theme.name.clone()
        }
    }

    /// Whether the theme requests xterm-256 palette downsampling.
    pub fn theme_palette_mode(&self) -> bool {
        matches!(self.theme.color_mode, ColorMode::Palette)
    }
}

/// Returns `None` only when there is truly nothing persisted yet (neither
/// `config.toml` nor `state.toml` exists), so a caller that only ever wrote
/// `app_state` (via [`update_app_state`]) still sees it here rather than
/// silently falling back to defaults just because `config.toml` itself was
/// never created.
pub fn load_config() -> Result<Option<Config>> {
    if !config_path()?.exists() && !state_path()?.exists() {
        return Ok(None);
    }
    Ok(Some(Config::load()?))
}

/// Atomically read-modify-write the global `config.toml`.
///
/// Loads a *fresh* [`Config`] from disk inside a process-wide mutex plus a
/// cross-process `flock`, applies `f`, then writes `config.toml` back out.
/// Any field `f` does not touch is preserved from the fresh on-disk copy, so
/// a concurrent writer's unrelated edits survive: the fresh load itself is
/// the merge, because `f` only mutates the fields it cares about and
/// everything else already reflects the current on-disk state.
///
/// `app_state` is always stripped from the written table; it is persisted
/// separately in `state.toml` (see [`update_app_state`]). Mutating
/// `config.app_state` inside `f` has no durable effect here.
///
/// Whatever `f` leaves `config` in gets written to disk, even if `f` mutates
/// `config` and then returns an error (e.g. via `?` partway through). There is
/// no rollback: a caller that wants "no error, no mutation" must check its
/// error condition and return before touching `config`, not after.
pub fn update_config<R>(f: impl FnOnce(&mut Config) -> R) -> Result<R> {
    let _mu = config_save_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let app_dir = get_app_dir()?;
    let _flock = super::storage::acquire_storage_flock(&app_dir, CONFIG_LOCK_FILENAME)?;

    let mut config = Config::load()?;
    let result = f(&mut config);

    let mut table = toml::Table::try_from(&config)?;
    table.remove("app_state");
    let content = toml::to_string_pretty(&table)?;
    super::atomic_write(&config_path()?, content.as_bytes())?;

    Ok(result)
}

pub(crate) fn state_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("state.toml"))
}

impl AppStateConfig {
    /// Read `state.toml` (fields at the TOML top level, not nested under an
    /// `[app_state]` table). A missing file deserializes to defaults.
    pub fn load() -> Result<Self> {
        let path = state_path()?;
        let table: toml::Table = if path.exists() {
            toml::from_str(&fs::read_to_string(&path)?)?
        } else {
            toml::Table::new()
        };
        Ok(table.try_into()?)
    }
}

/// Atomically read-modify-write `state.toml`.
///
/// Delegates to `storage::locked_update`, the
/// same serialised read-modify-write primitive `sessions.json` / `groups.json`
/// go through: under a cross-process `flock` on `state.toml`'s sidecar it loads
/// a *fresh* [`AppStateConfig`], applies `f`, and writes `state.toml` back out.
/// Any field `f` does not touch is preserved from the fresh on-disk copy, so a
/// concurrent writer's unrelated edits survive, and the TUI and an `aoe serve`
/// daemon (or any two `aoe` processes) can call this concurrently without
/// losing an update. Symlinked `state.toml` files are resolved and written
/// through, the same as every other `locked_update` file.
pub fn update_app_state<R>(f: impl FnOnce(&mut AppStateConfig) -> R) -> Result<R> {
    let outcome = super::storage::locked_update(
        &state_path()?,
        |content| Ok(content.parse::<toml::Table>()?.try_into()?),
        |state| Ok(toml::to_string_pretty(&toml::Table::try_from(state)?)?),
        |state| -> std::result::Result<R, std::convert::Infallible> { Ok(f(state)) },
    )?;
    match outcome {
        Ok(result) => Ok(result),
        Err(never) => match never {},
    }
}

/// Theme name to paint, read from the **global** config only.
///
/// The theme is a global preference: it is never profile-merged, so the TUI
/// boot, the Settings-close repaint, the tmux status bar, and the web
/// `/api/theme/current` all paint the same theme regardless of which session
/// profile is active. Reading the profile-merged theme on some surfaces but
/// the global theme on others let a per-profile override (which the web theme
/// picker used to write) shadow the global pick on every Settings open/close,
/// flipping the theme until the next restart. An empty name maps to the
/// `zinc` builtin, matching the web dashboard's empty-name fallback.
pub fn resolve_theme_name() -> String {
    Config::load_or_warn().effective_theme_name()
}

/// Whether the global theme requests xterm-256 palette downsampling. Reads the
/// global config only, for the same reason as [`resolve_theme_name`].
pub fn resolve_theme_palette_mode() -> bool {
    Config::load_or_warn().theme_palette_mode()
}

/// Resolve the active profile name.
///
/// If the user has explicitly set `config.default_profile`, that name is
/// returned verbatim. Otherwise this returns the first profile directory
/// found under `<app_dir>/profiles/` (sorted, so the choice is
/// deterministic). On a genuine first run, when no profile directory exists
/// yet, one is bootstrapped (see `ensure_bootstrap_profile`).
pub fn resolve_default_profile() -> String {
    let config = Config::load_or_warn();
    if !config.default_profile.is_empty() {
        return config.default_profile;
    }
    match super::list_profiles() {
        Ok(profiles) => match profiles.into_iter().next() {
            Some(first) => first,
            None => ensure_bootstrap_profile(),
        },
        Err(_) => ensure_bootstrap_profile(),
    }
}

/// Name of the profile created on a genuine first run.
const BOOTSTRAP_PROFILE: &str = "main";

/// Create the first profile on a genuine first run and return its name.
///
/// AoE always needs at least one profile (somewhere to file sessions). When
/// `profiles/` has no entries, this creates `main`. It is idempotent: calling
/// it when `main` already exists just returns the name.
fn ensure_bootstrap_profile() -> String {
    let _ = super::get_profile_dir(BOOTSTRAP_PROFILE);
    BOOTSTRAP_PROFILE.to_string()
}

/// Return `profile` if non-empty, otherwise the user's globally configured
/// default profile. Used at start-time config-resolution sites that prefer
/// an instance's `source_profile` but tolerate it being unset (e.g. tests
/// or pre-`source_profile`-wiring callers).
pub fn effective_profile(profile: &str) -> String {
    if profile.is_empty() {
        resolve_default_profile()
    } else {
        profile.to_string()
    }
}

pub fn get_update_settings() -> UpdatesConfig {
    Config::load_or_warn().updates
}

pub fn get_telemetry_settings() -> TelemetryConfig {
    Config::load_or_warn().telemetry
}

/// Wrap a value so the launching shell passes it through literally.
///
/// Only quotes when the value contains something a shell would interpret, so
/// ordinary model ids keep their current, unquoted command line.
fn shell_quote_value(value: &str) -> String {
    const SAFE: &str = "-_./:=@,+";
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || SAFE.contains(c));
    if plain {
        return value.to_string();
    }
    // Already quoted by whoever wrote it: re-quoting would nest, and the agent
    // would receive a model id with literal quote characters in it. Both
    // quote styles are shell-valid ways to protect a value, so either wrapper
    // is left alone.
    let already_quoted = value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')));
    if already_quoted {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Quote a shell-active `--model` / `-m` VALUE inside a free-form extra-args
/// string, leaving every other token exactly as the user wrote it.
///
/// `extra_args` is spliced into the launch command verbatim, so a model id
/// carrying shell metacharacters aborts the whole line before the agent
/// starts. A context-window suffix does exactly that: `--model claude-x[1m]`
/// dies under zsh with `no matches found: claude-x[1m]`, status 1, and the
/// pane is dead at launch with nothing to show for it.
///
/// Quoting only the model value keeps the rest of `extra_args` usable as the
/// caller's own argv, where shell syntax may well be intended. Untouched
/// regions (including their original whitespace) are copied byte-for-byte
/// rather than round-tripped through a tokenize/rejoin, which would collapse
/// runs of whitespace elsewhere in the string.
pub fn quote_model_value_in_args(args: &str) -> String {
    let mut out = String::with_capacity(args.len());
    let mut rest = args;
    let mut expect_model_value = false;
    loop {
        let ws_len = rest.len() - rest.trim_start().len();
        out.push_str(&rest[..ws_len]);
        rest = &rest[ws_len..];
        if rest.is_empty() {
            break;
        }
        let tok_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let tok = &rest[..tok_len];
        let assigned_model_value = tok.split_once('=').and_then(|(lhs, value)| {
            if (lhs == "--model" || lhs == "-m") && !value.is_empty() {
                Some((lhs, value))
            } else {
                None
            }
        });
        if expect_model_value {
            out.push_str(&shell_quote_value(tok));
            expect_model_value = false;
        } else if let Some((lhs, value)) = assigned_model_value {
            out.push_str(lhs);
            out.push('=');
            out.push_str(&shell_quote_value(value));
        } else {
            out.push_str(tok);
            expect_model_value = tok == "--model" || tok == "-m";
        }
        rest = &rest[tok_len..];
    }
    out
}

#[cfg(test)]
mod model_value_quoting_tests {
    use super::*;

    #[test]
    fn a_context_window_suffix_is_quoted() {
        assert_eq!(
            quote_model_value_in_args("--model claude-x[1m]"),
            "--model 'claude-x[1m]'"
        );
        assert_eq!(
            quote_model_value_in_args("--model=claude-x[1m]"),
            "--model='claude-x[1m]'"
        );
        assert_eq!(
            quote_model_value_in_args("-m claude-x[1m]"),
            "-m 'claude-x[1m]'"
        );
    }

    #[test]
    fn an_ordinary_model_id_is_left_alone() {
        // Quoting everything would change every existing command line.
        for args in ["--model claude-opus-4-8", "-m gpt-5", "--model=sonnet"] {
            assert_eq!(quote_model_value_in_args(args), args);
        }
    }

    #[test]
    fn other_arguments_are_never_rewritten() {
        let args = "--verbose --model claude-x[1m] --flag value";
        let got = quote_model_value_in_args(args);
        assert_eq!(got, "--verbose --model 'claude-x[1m]' --flag value");
    }

    #[test]
    fn unrelated_whitespace_and_quoting_survive_byte_for_byte() {
        // A tokenize/join round trip would collapse the double space here and
        // re-wrap an already double-quoted model value, changing what the
        // agent receives even though neither token needed rewriting.
        let cases = [
            ("--prompt \"hello  world\"", "--prompt \"hello  world\""),
            ("--model \"gpt-5\"", "--model \"gpt-5\""),
            ("--flag1   --flag2", "--flag1   --flag2"),
        ];
        for (input, expected) in cases {
            assert_eq!(quote_model_value_in_args(input), expected, "{input:?}");
        }
    }

    #[test]
    fn an_already_quoted_value_is_not_nested() {
        let args = "--model 'claude-x[1m]'";
        assert_eq!(quote_model_value_in_args(args), args);
    }

    #[test]
    fn a_dangling_flag_is_harmless() {
        assert_eq!(quote_model_value_in_args("--model"), "--model");
        assert_eq!(quote_model_value_in_args(""), "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the `auto` branch of [`resolve_tmux_setting`]: a config that does
    /// not set the option must read as "silent" so aoe still applies its own
    /// value, which for `mouse` is what keeps the dashboard's touch scroll
    /// working (#3207). The false rows are where a sloppy match would misfire,
    /// especially tmux's separate `mouse-*` options and a commented-out line.
    ///
    /// Rows are `mouse` unless noted; the recognizer is generic over the option
    /// name, and the clipboard rows at the end cover the second caller.
    #[test]
    fn test_tmux_config_sets_option() {
        let cases = [
            ("set -g mouse on", true),
            ("set -g mouse off", true),
            ("set-option -g mouse on", true),
            ("setw -g mouse on", true),
            ("set-window-option -g mouse on", true),
            // No flags, bundled flags, and stray indentation all still count.
            ("set mouse on", true),
            ("set -gq mouse on", true),
            ("   set -g   mouse   on   ", true),
            // Real-world file: the `mouse` line is one of several.
            (
                "# my tmux config\nset -g prefix C-a\nset -g mouse on\nset -g status-style bg=black",
                true,
            ),
            // Several commands on one line.
            ("set -g status off ; set -g mouse on", true),
            ("", false),
            // A config that never mentions the option: the case that regressed
            // when `auto` keyed on mere existence of a tmux.conf.
            ("set -g prefix C-a\nbind r source-file ~/.tmux.conf", false),
            ("# set -g mouse on", false),
            ("#set -g mouse on", false),
            ("  # set -g mouse on", false),
            // tmux's other `mouse`-prefixed options are not `mouse`.
            ("set -g mouse-select-pane on", false),
            ("set -g mouse-resize-pane on", false),
            ("set -g mouse-utf8 on", false),
            // Neither is a plugin name or a key binding that mentions the wheel.
            ("set -g @plugin 'tmux-better-mouse-mode'", false),
            ("bind -n WheelUpPane select-pane -t= \\; copy-mode -e", false),
            // Documented gaps, asserted so narrowing them stays a deliberate
            // change. All fail safe: `Auto` keeps setting `mouse on`, which is
            // what it did unconditionally before.
            ("bind m set -g mouse", false),
            ("if-shell '[ -n \"$SSH_TTY\" ]' 'set -g mouse on'", false),
            ("set -t other mouse on", false),
            // A `;` does not end a comment, and does not split a quoted value.
            // Reading either as a second command is a false *positive*, the one
            // direction that silently strips mouse support, so these are the
            // rows that matter most.
            ("# set -g status off; set -g mouse on", false),
            ("# see the manual; set -g mouse on works", false),
            ("set -g prefix C-a  # or; set -g mouse on", false),
            ("set -g default-command \"reattach; set -g mouse on\"", false),
            // A trailing comment after a real setting still counts.
            ("set -g mouse on # enable mouse", true),
            // The escape and continuation forms, all verified against tmux 3.6
            // by loading the line and reading back `show -g -v mouse`. Each is
            // a line tmux does *not* set `mouse` from, so reading one as a live
            // `set` would be the damaging false positive.
            //
            // `\;` is an argument, not a separator: this is a key binding.
            ("bind m display \"x\" \\; set -g mouse on", false),
            // A `#` immediately after a `;` still starts a comment.
            ("set -g status off ;# reload; set -g mouse on", false),
            // A trailing `\` continues the logical line, so the `set` here is
            // part of the `bind`, and a continued comment stays a comment.
            ("bind M \\\n    set -g mouse on \\; \\\n    display \"on\"", false),
            ("# note \\\nset -g mouse on", false),
            // An escaped quote does not end the quoted value.
            ("set -g status-right \"\\\"; set -g mouse on\"", false),
            // A continued line that really does set `mouse` still counts.
            ("set -g mouse \\\n    on", true),
        ];
        for (contents, expected) in cases {
            assert_eq!(
                tmux_config_sets_any(contents, &["mouse"]),
                expected,
                "{contents:?}"
            );
        }

        // The clipboard options aoe manages. `set-clipboard` is a server option
        // and `allow-passthrough` a window option, so their real-world spellings
        // carry `-s` / `-w`, which the flag skip has to walk past.
        let clipboard = ["set-clipboard", "allow-passthrough"];
        let clipboard_cases = [
            ("set -s set-clipboard on", true),
            ("set -s set-clipboard external", true),
            ("set -sg set-clipboard off", true),
            ("setw -g allow-passthrough on", true),
            ("set -wg allow-passthrough on", true),
            // Either option alone is enough to defer: a user who took over one
            // half of OSC 52 forwarding owns the feature.
            ("set -g prefix C-a\nset -s set-clipboard on", true),
            // Neither option mentioned, so aoe's `auto` still applies both.
            ("set -g prefix C-a\nset -g mouse on", false),
            // Not these options, however much they look like them.
            ("set -g set-clipboard-external on", false),
            ("# set -s set-clipboard on", false),
        ];
        for (contents, expected) in clipboard_cases {
            assert_eq!(
                tmux_config_sets_any(contents, &clipboard),
                expected,
                "{contents:?}"
            );
        }
    }

    /// The `$XDG_CONFIG_HOME/tmux/tmux.conf` path, which tmux reads as its own
    /// search-path entry and aoe used to miss: a user who keeps their config
    /// there with `set -g mouse off` stayed overridden by aoe's session option,
    /// which is #3207's own complaint. Covers the probe and the recognizer
    /// together, since the wiring between them is what regressed.
    #[test]
    #[serial_test::serial]
    fn test_user_tmux_config_found_under_xdg_config_home() {
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let temp = tempfile::TempDir::new().unwrap();
        // An XDG root deliberately outside `$HOME/.config`, the case the
        // two-path list could not see.
        let xdg = temp.path().join("xdg-elsewhere");
        std::fs::create_dir_all(xdg.join("tmux")).unwrap();
        std::env::set_var("HOME", temp.path().join("home"));
        std::env::set_var("XDG_CONFIG_HOME", &xdg);

        assert!(
            !user_has_tmux_config(),
            "no tmux.conf anywhere yet, so `auto` must still apply aoe's values"
        );

        std::fs::write(xdg.join("tmux").join("tmux.conf"), "set -g mouse off\n").unwrap();
        assert!(user_has_tmux_config(), "tmux.conf under XDG_CONFIG_HOME");
        assert!(
            user_tmux_config_sets_any(&["mouse"]),
            "the `mouse` there must be seen, or aoe overrides it again"
        );
        assert!(
            !user_tmux_config_sets_any(&["set-clipboard"]),
            "a file silent on clipboard must not defer clipboard too"
        );

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    /// The one resolver, over every setting and mode. `Auto` is the only row
    /// that consults the user's config, and each setting declares what makes it
    /// defer in `TmuxSetting::row`; this pins the mode-to-action half, which is
    /// what every applier keys on.
    #[test]
    fn test_tmux_setting_action_table() {
        use TmuxSettingAction::{Apply, ForceOff, LeaveToUser};
        let cases = [
            (TmuxSettingMode::Auto, false, Apply),
            (TmuxSettingMode::Auto, true, LeaveToUser),
            // An explicit mode never consults the user's tmux config; that is
            // what makes it explicit, and it is the documented escape hatch for
            // every recognizer gap.
            (TmuxSettingMode::Enabled, false, Apply),
            (TmuxSettingMode::Enabled, true, Apply),
            (TmuxSettingMode::Disabled, false, ForceOff),
            (TmuxSettingMode::Disabled, true, ForceOff),
        ];
        for (mode, user_defers, expected) in cases {
            assert_eq!(
                tmux_setting_action(mode, user_defers),
                expected,
                "{mode:?} user_defers={user_defers}"
            );
        }
    }

    /// The options `auto` defers on must be exactly the options the row writes
    /// on `Apply`: a write added without its defer entry would override a user
    /// who took that option in hand, the #3207 failure mode for a new option.
    /// Compared as sets, because the defer's order is unobservable: the
    /// recognizer matches any order.
    #[test]
    fn test_tmux_setting_defer_matches_apply_options() {
        for setting in TmuxSetting::ALL {
            let row = setting.row();
            let TmuxAutoDefer::WhenUserSets(defer_options) = row.defer else {
                // StatusBar defers on config existence, not on any option.
                continue;
            };
            let mut defer_names: Vec<&str> = defer_options.to_vec();
            defer_names.sort_unstable();
            let mut apply_names: Vec<&str> = row
                .apply
                .iter()
                .map(|w| match *w {
                    TmuxOptionWrite::Session { option, .. }
                    | TmuxOptionWrite::Server { option, .. }
                    | TmuxOptionWrite::Window { option, .. } => option,
                })
                .collect();
            apply_names.sort_unstable();
            assert_eq!(
                apply_names, defer_names,
                "{setting:?}: defer options and apply writes must name the same options"
            );
        }
    }

    /// The compiler tripwire for [`TmuxSetting::ALL`]: the exhaustive match
    /// fails to compile when an enum variant is added, forcing a reviewer to
    /// add a brace here; the equality then pins `ALL`'s content and emission
    /// order against this literal. The unified applier iterates `ALL`, so a
    /// setting missing from both the list and this literal would be silently
    /// never applied; keeping the two in step is the point of the test.
    #[test]
    fn test_tmux_setting_all_is_exhaustive() {
        let setting = TmuxSetting::StatusBar;
        match setting {
            TmuxSetting::StatusBar => {}
            TmuxSetting::Mouse => {}
            TmuxSetting::Clipboard => {}
        }
        assert_eq!(
            TmuxSetting::ALL,
            [
                TmuxSetting::StatusBar,
                TmuxSetting::Mouse,
                TmuxSetting::Clipboard
            ],
            "ALL must list every variant exactly once, in emission order"
        );
    }

    #[test]
    fn test_effective_profile_returns_input_when_non_empty() {
        // Non-empty input is passed through verbatim, regardless of what's
        // configured globally as the default. No filesystem access needed.
        assert_eq!(effective_profile("personal"), "personal");
        assert_eq!(effective_profile("default"), "default");
        assert_eq!(effective_profile("alpha-beta_v2"), "alpha-beta_v2");
    }

    #[test]
    #[serial_test::serial]
    fn test_effective_profile_falls_back_to_global_default_when_empty() {
        let temp_home = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let app_dir = temp_home
            .path()
            .join(".config")
            .join(crate::session::APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let app_dir = temp_home.path().join(crate::session::APP_DIR_NAME_OTHER);

        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("config.toml"), r#"default_profile = "alpha""#).unwrap();

        assert_eq!(
            effective_profile(""),
            "alpha",
            "empty profile must fall back to the user's globally configured default",
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_load_or_warn_returns_defaults_on_malformed_toml() {
        let temp_home = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let app_dir = temp_home
            .path()
            .join(".config")
            .join(crate::session::APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let app_dir = temp_home.path().join(crate::session::APP_DIR_NAME_OTHER);

        std::fs::create_dir_all(&app_dir).unwrap();
        // Malformed: 'enabled_by_default' under [sandbox] expects a boolean.
        std::fs::write(
            app_dir.join("config.toml"),
            "[sandbox]\nenabled_by_default = \"not-a-bool\"\n",
        )
        .unwrap();

        let config = Config::load_or_warn();
        // Defaults restored rather than propagated; the parse error is logged.
        let defaults = Config::default();
        assert_eq!(
            config.sandbox.enabled_by_default,
            defaults.sandbox.enabled_by_default,
        );
    }

    /// A symlinked global `config.toml` must survive a save via
    /// `update_config`: the link stays a link and its target receives the
    /// new content, instead of the save replacing the symlink with a
    /// regular file (#2784).
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn update_config_preserves_symlink() {
        use std::os::unix::fs::symlink;

        let guard = crate::session::test_support::isolate_app_dir();
        let temp_home = guard.path();

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let app_dir = temp_home
            .join(".config")
            .join(crate::session::APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let app_dir = temp_home.join(crate::session::APP_DIR_NAME_OTHER);
        std::fs::create_dir_all(&app_dir).unwrap();

        // Simulate a dotfiles repo the user symlinks config.toml into.
        let dotfiles = temp_home.join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("aoe-config.toml");
        std::fs::write(&target, "default_profile = \"old\"\n").unwrap();

        let link = app_dir.join("config.toml");
        symlink(&target, &link).unwrap();

        update_config(|c| c.default_profile = "new".to_string()).unwrap();

        // The link is still a link, not a fresh regular file.
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "update_config must not replace the symlink with a regular file",
        );
        // The write landed on the target, not beside the link.
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(
            written.contains("default_profile = \"new\""),
            "symlink target should hold the saved config, got: {written}",
        );
    }

    // Tests for Config defaults
    #[test]
    fn test_config_default() {
        let config = Config::default();
        // An unset default_profile deserializes empty: "not explicitly
        // chosen". The active profile is resolved at runtime, not baked in
        // as a magic name here.
        let deserialized: Config = toml::from_str("").unwrap();
        assert_eq!(deserialized.default_profile, "");
        assert!(!config.worktree.enabled);
        assert!(!config.sandbox.enabled_by_default);
        assert_eq!(config.updates.update_check_mode, UpdateCheckMode::Notify);
    }

    #[test]
    fn test_config_deserialize_empty_toml() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.default_profile, "");
    }

    #[test]
    fn test_config_deserialize_partial_toml() {
        let toml = r#"
            default_profile = "custom"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.default_profile, "custom");
        // Other fields should have defaults
        assert!(!config.worktree.enabled);
    }

    #[test]
    fn test_plugins_table_round_trips_through_save() {
        // A plugin's enable-state must survive serialize/deserialize.
        let toml_in = r#"
            [plugins."aoe.web"]
            enabled = false
        "#;
        let config: Config = toml::from_str(toml_in).unwrap();
        assert!(!config.plugins["aoe.web"].enabled);

        let serialized = toml::to_string(&config).unwrap();
        let reloaded: Config = toml::from_str(&serialized).unwrap();
        assert!(!reloaded.plugins["aoe.web"].enabled);
    }

    #[test]
    fn test_plugins_default_empty_and_omitted_from_toml() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.plugins.is_empty());
        let serialized = toml::to_string(&config).unwrap();
        assert!(
            !serialized.contains("[plugins"),
            "empty plugins map must not serialize a stray section"
        );
    }

    #[test]
    fn test_plugin_settings_persist_even_while_disabled() {
        // Disabling a plugin hides its settings from every surface but must
        // never destroy them: the values survive a save/load round-trip.
        let toml_in = r#"
            [plugins."aoe.status"]
            enabled = false

            [plugins."aoe.status".settings]
            poll_interval_ms = 1000
            verbose = true
        "#;
        let config: Config = toml::from_str(toml_in).unwrap();
        let plugin = &config.plugins["aoe.status"];
        assert!(!plugin.enabled);
        assert_eq!(plugin.settings["poll_interval_ms"].as_integer(), Some(1000));

        let serialized = toml::to_string(&config).unwrap();
        let reloaded: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reloaded.plugins["aoe.status"].settings["verbose"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn test_plugin_empty_settings_omitted_from_toml() {
        let toml_in = r#"
            [plugins."aoe.web"]
            enabled = true
        "#;
        let config: Config = toml::from_str(toml_in).unwrap();
        assert!(config.plugins["aoe.web"].settings.is_empty());
        let serialized = toml::to_string(&config).unwrap();
        assert!(
            !serialized.contains("settings"),
            "empty plugin settings must not serialize a stray section"
        );
    }

    // Tests for ThemeConfig
    #[test]
    fn test_theme_config_default() {
        let theme = ThemeConfig::default();
        assert_eq!(theme.name, "");
        // Freshness signal is off by default; users opt in by setting a
        // positive value via Settings -> Theme -> Idle Decay (minutes)
        // or in config.toml directly.
        assert_eq!(theme.idle_decay_minutes, 0);
    }

    #[test]
    fn test_theme_config_deserialize() {
        let toml = r#"name = "dark""#;
        let theme: ThemeConfig = toml::from_str(toml).unwrap();
        assert_eq!(theme.name, "dark");
        // Missing field defaults to the off state. Existing configs
        // without `idle_decay_minutes` get the calmer (no-rattle)
        // default rather than being opted into the visual signal.
        assert_eq!(theme.idle_decay_minutes, 0);
    }

    #[test]
    fn test_theme_config_idle_decay_override() {
        let toml = r#"
            name = "dracula"
            idle_decay_minutes = 5
        "#;
        let theme: ThemeConfig = toml::from_str(toml).unwrap();
        assert_eq!(theme.idle_decay_minutes, 5);
    }

    #[test]
    fn test_theme_config_idle_decay_zero_disables() {
        // 0 is a valid setting that disables the freshness signal
        // entirely. Verifying it round-trips cleanly so users can opt
        // out without having to remove the field.
        let toml = r#"
            idle_decay_minutes = 0
        "#;
        let theme: ThemeConfig = toml::from_str(toml).unwrap();
        assert_eq!(theme.idle_decay_minutes, 0);
    }

    // Tests for UpdatesConfig
    #[test]
    fn test_updates_config_default() {
        let updates = UpdatesConfig::default();
        assert_eq!(updates.update_check_mode, UpdateCheckMode::Notify);
    }

    #[test]
    fn test_updates_config_deserialize() {
        let toml = r#"
            update_check_mode = "off"
        "#;
        let updates: UpdatesConfig = toml::from_str(toml).unwrap();
        assert_eq!(updates.update_check_mode, UpdateCheckMode::Off);
    }

    #[test]
    fn test_updates_config_partial_deserialize() {
        let toml = r#"update_check_mode = "auto""#;
        let updates: UpdatesConfig = toml::from_str(toml).unwrap();
        assert_eq!(updates.update_check_mode, UpdateCheckMode::Auto);
    }

    #[test]
    fn test_update_check_mode_helpers() {
        assert!(UpdateCheckMode::Notify.is_enabled());
        assert!(UpdateCheckMode::Notify.notifies());
        assert!(!UpdateCheckMode::Notify.auto_installs());

        assert!(UpdateCheckMode::Auto.is_enabled());
        assert!(!UpdateCheckMode::Auto.notifies());
        assert!(UpdateCheckMode::Auto.auto_installs());

        assert!(!UpdateCheckMode::Off.is_enabled());
        assert!(!UpdateCheckMode::Off.notifies());
        assert!(!UpdateCheckMode::Off.auto_installs());
    }

    /// Regression: earlier schemas had `check_enabled`, `auto_update`,
    /// `check_interval_hours`, `notify_in_cli`, and
    /// `web_poll_interval_minutes` on UpdatesConfig. All are gone now;
    /// the on-disk migration runs at startup, but configs read between
    /// upgrade and migration must still deserialize cleanly with the
    /// unknown fields silently dropped by serde.
    #[test]
    fn test_legacy_updates_fields_are_ignored() {
        let old_toml = r#"
            check_enabled = false
            auto_update = true
            check_interval_hours = 12
            notify_in_cli = true
            web_poll_interval_minutes = 30
        "#;
        let updates: UpdatesConfig =
            toml::from_str(old_toml).expect("legacy fields should not error");
        assert_eq!(updates.update_check_mode, UpdateCheckMode::Notify);
    }

    // Tests for WorktreeConfig
    #[test]
    fn test_worktree_config_default() {
        let wt = WorktreeConfig::default();
        assert!(!wt.enabled);
        assert_eq!(wt.path_template, "../{repo-name}-worktrees/{branch}");
        assert!(wt.auto_cleanup);
        assert!(
            wt.init_submodules,
            "init_submodules must default to true to preserve #942 behavior"
        );
    }

    #[test]
    fn test_worktree_config_deserialize() {
        let toml = r#"
            enabled = true
            path_template = "/custom/{branch}"
            auto_cleanup = false
            init_submodules = false
        "#;
        let wt: WorktreeConfig = toml::from_str(toml).unwrap();
        assert!(wt.enabled);
        assert_eq!(wt.path_template, "/custom/{branch}");
        assert!(!wt.auto_cleanup);
        assert!(!wt.init_submodules);
    }

    #[test]
    fn test_worktree_config_init_submodules_defaults_when_absent() {
        // Configs predating this option must continue to recursively init
        // submodules (preserve #942 behavior) when upgrading.
        let toml = r#"
            enabled = true
        "#;
        let wt: WorktreeConfig = toml::from_str(toml).unwrap();
        assert!(wt.init_submodules);
    }

    // Tests for SandboxConfig
    #[test]
    fn test_sandbox_config_default() {
        let sb = SandboxConfig::default();
        assert!(!sb.enabled_by_default);
        assert!(sb.auto_cleanup);
        assert!(sb.extra_volumes.is_empty());
        assert!(sb.environment.contains(&"TERM".to_string()));
        assert!(sb.environment.contains(&"COLORTERM".to_string()));
        assert!(sb.cpu_limit.is_none());
        assert!(sb.memory_limit.is_none());
        assert!(sb.volume_ignores.is_empty());
        assert!(sb.network.is_none());
    }

    #[test]
    fn test_sandbox_config_deserialize() {
        let toml = r#"
            enabled_by_default = true
            default_image = "custom:latest"
            extra_volumes = ["/data:/data"]
            environment = ["MY_VAR"]
            auto_cleanup = false
            cpu_limit = "2"
            memory_limit = "4g"
            port_mappings = ["3000:3000", "5432:5432"]
            network = "none"
        "#;
        let sb: SandboxConfig = toml::from_str(toml).unwrap();
        assert!(sb.enabled_by_default);
        assert_eq!(sb.default_image, "custom:latest");
        assert_eq!(sb.extra_volumes, vec!["/data:/data"]);
        assert_eq!(sb.environment, vec!["MY_VAR"]);
        assert!(!sb.auto_cleanup);
        assert_eq!(sb.cpu_limit, Some("2".to_string()));
        assert_eq!(sb.memory_limit, Some("4g".to_string()));
        assert_eq!(
            sb.port_mappings,
            vec!["3000:3000".to_string(), "5432:5432".to_string()]
        );
        assert_eq!(sb.network, Some("none".to_string()));
    }

    #[test]
    fn test_sandbox_config_volume_ignores_deserialize() {
        let toml = r#"
            volume_ignores = ["target", ".venv", "node_modules"]
        "#;
        let sb: SandboxConfig = toml::from_str(toml).unwrap();
        assert_eq!(sb.volume_ignores, vec!["target", ".venv", "node_modules"]);
    }

    #[test]
    fn test_sandbox_config_volume_ignores_defaults_empty() {
        let toml = r#"enabled_by_default = false"#;
        let sb: SandboxConfig = toml::from_str(toml).unwrap();
        assert!(sb.volume_ignores.is_empty());
    }

    #[test]
    fn test_sandbox_config_volume_ignores_roundtrip() {
        let mut config = Config::default();
        config.sandbox.volume_ignores = vec!["target".to_string(), "node_modules".to_string()];

        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(
            deserialized.sandbox.volume_ignores,
            vec!["target", "node_modules"]
        );
    }

    #[test]
    fn test_sandbox_config_string_shorthand() {
        // Regression test: all Vec<String> sandbox fields accept a plain string
        let toml = r#"
            environment = "ANTHROPIC_API_KEY"
            extra_volumes = "/data:/data:ro"
            volume_ignores = "node_modules"
            port_mappings = "3000:3000"
        "#;
        let sb: SandboxConfig = toml::from_str(toml).unwrap();
        assert_eq!(sb.environment, vec!["ANTHROPIC_API_KEY"]);
        assert_eq!(sb.extra_volumes, vec!["/data:/data:ro"]);
        assert_eq!(sb.volume_ignores, vec!["node_modules"]);
        assert_eq!(sb.port_mappings, vec!["3000:3000"]);
    }

    // Tests for AppStateConfig
    #[test]
    fn test_app_state_config_default() {
        let app = AppStateConfig::default();
        assert!(!app.has_seen_welcome);
        assert!(!app.has_seen_web_tour);
        assert!(app.last_seen_version.is_none());
        assert!(app.dismissed_update_version.is_none());
    }

    #[test]
    fn test_app_state_config_deserialize() {
        let toml = r#"
            has_seen_welcome = true
            last_seen_version = "1.0.0"
            dismissed_update_version = "1.0.0"
        "#;
        let app: AppStateConfig = toml::from_str(toml).unwrap();
        assert!(app.has_seen_welcome);
        // Absent from the toml: defaults to false (backward compatible).
        assert!(!app.has_seen_web_tour);
        assert_eq!(app.last_seen_version, Some("1.0.0".to_string()));
        assert_eq!(app.dismissed_update_version, Some("1.0.0".to_string()));
    }

    #[test]
    fn test_app_state_config_web_tour_roundtrip() {
        let toml = r#"
            has_seen_web_tour = true
        "#;
        let app: AppStateConfig = toml::from_str(toml).unwrap();
        assert!(app.has_seen_web_tour);
        assert!(!app.has_seen_welcome);
    }

    #[test]
    fn test_app_state_config_tips_defaults_and_roundtrip() {
        // Absent from old configs: nothing seen, zero count. (The on/off toggle
        // lives in `SessionConfig::show_tips`, not here.)
        let app = AppStateConfig::default();
        assert!(app.tips_seen.is_empty());
        assert_eq!(app.new_session_with_selection_count, 0);
        assert!(!app.used_new_from_selection);
        assert!(!app.system_health_tip_earned);
        assert!(!app.used_system_health);

        let toml = r#"
            tips_seen = ["new-from-selection"]
            new_session_with_selection_count = 4
            used_new_from_selection = true
            system_health_tip_earned = true
            used_system_health = true
        "#;
        let app: AppStateConfig = toml::from_str(toml).unwrap();
        assert_eq!(app.tips_seen, vec!["new-from-selection"]);
        assert_eq!(app.new_session_with_selection_count, 4);
        assert!(app.used_new_from_selection);
        assert!(app.system_health_tip_earned);
        assert!(app.used_system_health);

        // Round-trips back out.
        let serialized = toml::to_string(&app).unwrap();
        let reparsed: AppStateConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.tips_seen, app.tips_seen);
        assert_eq!(reparsed.new_session_with_selection_count, 4);
    }

    #[test]
    fn test_session_config_show_tips_defaults_on() {
        // Absent from old configs, tips default to on.
        let toml = "default_tool = \"claude\"\n";
        let session: SessionConfig = toml::from_str(toml).unwrap();
        assert!(session.show_tips);
    }

    #[test]
    fn test_session_config_row_tag_defaults_to_branch() {
        let session: SessionConfig = toml::from_str("").unwrap();
        assert_eq!(session.row_tag, RowTagMode::Branch);
    }

    #[test]
    fn test_session_config_row_tag_roundtrip() {
        let session: SessionConfig = toml::from_str("row_tag = \"none\"\n").unwrap();
        assert_eq!(session.row_tag, RowTagMode::None);

        let serialized = toml::to_string(&session).unwrap();
        let reparsed: SessionConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.row_tag, RowTagMode::None);
    }

    // Full config serialization roundtrip
    #[test]
    fn test_config_serialization_roundtrip() {
        let config = Config {
            default_profile: "test".to_string(),
            worktree: WorktreeConfig {
                enabled: true,
                ..Default::default()
            },
            sandbox: SandboxConfig {
                enabled_by_default: true,
                ..Default::default()
            },
            updates: UpdatesConfig {
                update_check_mode: UpdateCheckMode::Auto,
                ..Default::default()
            },
            ..Default::default()
        };

        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(config.default_profile, deserialized.default_profile);
        assert_eq!(config.worktree.enabled, deserialized.worktree.enabled);
        assert_eq!(
            config.sandbox.enabled_by_default,
            deserialized.sandbox.enabled_by_default
        );
        assert_eq!(
            config.updates.update_check_mode,
            deserialized.updates.update_check_mode
        );
    }

    // Test nested sections in TOML
    #[test]
    fn test_config_nested_sections() {
        let toml = r#"
            default_profile = "work"

            [theme]
            name = "monokai"

            [worktree]
            enabled = true
            path_template = "../wt/{branch}"

            [sandbox]
            enabled_by_default = true

            [updates]
            update_check_mode = "notify"

            [app_state]
            has_seen_welcome = true
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.default_profile, "work");
        assert_eq!(config.theme.name, "monokai");
        assert!(config.worktree.enabled);
        assert_eq!(config.worktree.path_template, "../wt/{branch}");
        assert!(config.sandbox.enabled_by_default);
        assert_eq!(config.updates.update_check_mode, UpdateCheckMode::Notify);
        assert!(config.app_state.has_seen_welcome);
    }

    // Test get_update_settings helper
    #[test]
    fn test_get_update_settings_returns_defaults_when_no_config() {
        // This test doesn't access the filesystem, so it should return defaults
        let settings = UpdatesConfig::default();
        assert_eq!(settings.update_check_mode, UpdateCheckMode::Notify);
    }

    // Tests for TmuxConfig

    /// The three `[tmux]` mode fields share one `TmuxSettingMode`, so this is
    /// the compatibility contract for that: every field still accepts each
    /// lowercase spelling in `config.toml`, still defaults to `auto` when
    /// absent, and still round-trips. Serialized values are what users have
    /// already written to disk, so a rename here is a breaking change.
    #[test]
    fn test_tmux_config_modes_deserialize() {
        use TmuxSettingMode::{Auto, Disabled, Enabled};
        let modes = |tmux: &TmuxConfig| (tmux.status_bar, tmux.mouse, tmux.clipboard);
        assert_eq!(modes(&TmuxConfig::default()), (Auto, Auto, Auto));

        let cases = [
            // Absent fields fall back to `auto` through `#[serde(default)]`.
            ("", (Auto, Auto, Auto)),
            (r#"status_bar = "enabled""#, (Enabled, Auto, Auto)),
            (r#"status_bar = "disabled""#, (Disabled, Auto, Auto)),
            (r#"status_bar = "auto""#, (Auto, Auto, Auto)),
            (r#"mouse = "enabled""#, (Auto, Enabled, Auto)),
            (r#"mouse = "disabled""#, (Auto, Disabled, Auto)),
            (r#"clipboard = "enabled""#, (Auto, Auto, Enabled)),
            (r#"clipboard = "disabled""#, (Auto, Auto, Disabled)),
            (
                "status_bar = \"enabled\"\nmouse = \"enabled\"\nclipboard = \"disabled\"",
                (Enabled, Enabled, Disabled),
            ),
        ];
        for (toml_src, expected) in cases {
            let tmux: TmuxConfig = toml::from_str(toml_src).unwrap();
            assert_eq!(modes(&tmux), expected, "{toml_src:?}");
            // Nested under `[tmux]` in a whole config, the path every real
            // config.toml takes.
            let config: Config = toml::from_str(&format!("[tmux]\n{toml_src}")).unwrap();
            assert_eq!(modes(&config.tmux), expected, "[tmux] {toml_src:?}");
            // And a round-trip through the serializer the settings surfaces use.
            let round_tripped: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(
                modes(&round_tripped.tmux),
                expected,
                "roundtrip {toml_src:?}"
            );
        }
    }

    #[test]
    fn test_tmux_config_vt_live_defaults_on_and_deserializes_off() {
        // Absent from an existing config.toml => on (the pre-setting
        // behavior; the AOE_VT_LIVE env hatch this replaces defaulted on).
        let tmux: TmuxConfig = toml::from_str(r#""#).unwrap();
        assert!(tmux.vt_live, "vt_live must default on when absent");
        // Explicit off round-trips.
        let tmux: TmuxConfig = toml::from_str(r#"vt_live = false"#).unwrap();
        assert!(!tmux.vt_live);
    }

    #[test]
    fn test_vt_live_in_settings_schema() {
        // The single-source schema must expose the toggle so both the TUI
        // and web settings render it (docs/development/adding-settings.md).
        let schema = crate::session::config::settings_schema::schema();
        let field = schema
            .iter()
            .find(|f| f.section == "tmux" && f.field == "vt_live")
            .expect("vt_live field in tmux schema section");
        assert!(field.advanced, "vt_live should sit under the Advanced fold");
        assert!(
            !field.profile_overridable,
            "vt_live is machine-level (the server reads global config); a \
             profile override would desync the TUI and web transports"
        );
    }

    #[test]
    fn test_new_session_mode_in_settings_schema() {
        // The single-source schema must expose the select so the TUI and
        // web settings both render it (docs/development/adding-settings.md).
        // The option values are hand-written in the `#[setting(...)]`
        // attribute, so each one is round-tripped through `NewSessionMode`:
        // a typo would offer a choice that the config layer rejects on save.
        let schema = crate::session::config::settings_schema::schema();
        let field = schema
            .iter()
            .find(|f| f.section == "session" && f.field == "new_session_mode")
            .expect("new_session_mode field in session schema section");
        let settings_schema::WidgetKind::Select { options } = &field.widget else {
            panic!("new_session_mode must render as a select");
        };
        let values: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(values, ["match_default", "tmux", "live_send"]);
        for value in values {
            let parsed: NewSessionMode = serde_json::from_str(&format!("\"{value}\""))
                .unwrap_or_else(|e| {
                    panic!("select option {value:?} must deserialize into NewSessionMode: {e}")
                });
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                format!("\"{value}\""),
                "select option {value:?} must round-trip"
            );
        }
    }

    // Tests for DiffConfig
    #[test]
    fn test_diff_config_default() {
        let diff = DiffConfig::default();
        assert!(diff.default_branch.is_none());
        assert_eq!(diff.context_lines, 3);
    }

    #[test]
    fn test_diff_config_deserialize() {
        let toml = r#"
            default_branch = "main"
            context_lines = 5
        "#;
        let diff: DiffConfig = toml::from_str(toml).unwrap();
        assert_eq!(diff.default_branch, Some("main".to_string()));
        assert_eq!(diff.context_lines, 5);
    }

    #[test]
    fn test_diff_config_partial_deserialize() {
        let toml = r#"default_branch = "develop""#;
        let diff: DiffConfig = toml::from_str(toml).unwrap();
        assert_eq!(diff.default_branch, Some("develop".to_string()));
        assert_eq!(diff.context_lines, 3);
    }

    #[test]
    fn test_diff_config_in_full_config() {
        let toml = r#"
            [diff]
            default_branch = "main"
            context_lines = 10
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.diff.default_branch, Some("main".to_string()));
        assert_eq!(config.diff.context_lines, 10);
    }

    #[test]
    fn diff_config_split_view_roundtrips() {
        let mut cfg = DiffConfig::default();
        assert!(!cfg.split_view);
        cfg.split_view = true;
        let toml = toml::to_string(&cfg).unwrap();
        let back: DiffConfig = toml::from_str(&toml).unwrap();
        assert!(back.split_view);
    }

    #[test]
    fn test_session_config_agent_override_roundtrip() {
        let mut config = Config::default();
        config
            .session
            .agent_command_override
            .insert("claude".to_string(), "safehouse".to_string());
        config
            .session
            .agent_extra_args
            .insert("opencode".to_string(), "--port 8080".to_string());
        config.acp.acp_defaults.insert(
            "opencode".to_string(),
            AcpAgentDefaults {
                model: Some("openai/gpt-5.5".to_string()),
                effort: Some("high".to_string()),
                ..Default::default()
            },
        );

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.session.agent_command_override.get("claude"),
            Some(&"safehouse".to_string()),
            "agent_command_override should survive roundtrip"
        );
        assert_eq!(
            deserialized.session.agent_extra_args.get("opencode"),
            Some(&"--port 8080".to_string()),
            "agent_extra_args should survive roundtrip"
        );
        assert_eq!(
            deserialized.acp.acp_defaults.get("opencode"),
            Some(&AcpAgentDefaults {
                model: Some("openai/gpt-5.5".to_string()),
                effort: Some("high".to_string()),
                ..Default::default()
            }),
            "acp_defaults should survive roundtrip"
        );
    }

    #[test]
    fn agent_status_map_roundtrips() {
        let mut config = Config::default();
        config
            .agents
            .entry("claude".to_string())
            .or_default()
            .status_map
            .insert("Stop".to_string(), crate::agents::HookStatus::Error);

        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("[agents.claude.status_map]"));
        assert!(serialized.contains("Stop = \"error\""));

        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.agents["claude"].status_map.get("Stop"),
            Some(&crate::agents::HookStatus::Error)
        );
    }

    #[test]
    fn agent_status_map_rejects_invalid_status() {
        let toml = r#"
            [agents.claude.status_map]
            Stop = "stopped"
        "#;

        let err = toml::from_str::<Config>(toml).unwrap_err();
        assert!(err.to_string().contains("stopped"));
    }

    #[test]
    fn agent_status_rules_roundtrip() {
        let toml = r#"
            [[agents.gjc.status_rules]]
            status = "running"
            contains = "esc to interrupt"

            [[agents.gjc.status_rules]]
            status = "waiting"
            regex = "\\(y/n\\)"
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        let rules = &config.agents["gjc"].status_rules;
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].status, crate::agents::HookStatus::Running);
        assert_eq!(rules[0].contains.as_deref(), Some("esc to interrupt"));
        assert!(rules[0].regex.is_none());
        assert_eq!(rules[1].status, crate::agents::HookStatus::Waiting);
        assert_eq!(rules[1].regex.as_deref(), Some(r"\(y/n\)"));

        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("[[agents.gjc.status_rules]]"));
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.agents["gjc"].status_rules, *rules);
    }

    #[test]
    fn agent_status_rules_reject_invalid_status() {
        let toml = r#"
            [[agents.gjc.status_rules]]
            status = "stopped"
            contains = "x"
        "#;

        let err = toml::from_str::<Config>(toml).unwrap_err();
        assert!(err.to_string().contains("stopped"));
    }

    #[test]
    fn acp_defaults_effort_for_model_prefers_per_model_then_flat() {
        let mut defaults = AcpAgentDefaults {
            effort: Some("low".to_string()),
            ..Default::default()
        };
        defaults
            .effort_by_model
            .insert("gpt-5".to_string(), "high".to_string());

        // Matching model uses the per-model override.
        assert_eq!(
            defaults.effort_for_model(Some("gpt-5")).as_deref(),
            Some("high")
        );
        // Non-matching model and the no-model case fall back to the flat effort.
        assert_eq!(
            defaults.effort_for_model(Some("other")).as_deref(),
            Some("low")
        );
        assert_eq!(defaults.effort_for_model(None).as_deref(), Some("low"));
        // An empty per-model value is treated as unset and falls back.
        defaults
            .effort_by_model
            .insert("gpt-5".to_string(), String::new());
        assert_eq!(
            defaults.effort_for_model(Some("gpt-5")).as_deref(),
            Some("low")
        );
    }

    #[test]
    fn acp_defaults_is_empty_covers_new_fields() {
        let mut defaults = AcpAgentDefaults::default();
        assert!(defaults.is_empty());
        defaults.mode = Some("plan".to_string());
        assert!(!defaults.is_empty());

        let mut with_map = AcpAgentDefaults::default();
        with_map
            .effort_by_model
            .insert("gpt-5".to_string(), "high".to_string());
        assert!(!with_map.is_empty());
    }

    #[test]
    fn acp_defaults_mode_treats_empty_as_unset() {
        let mut defaults = AcpAgentDefaults::default();
        assert_eq!(defaults.mode(), None);
        defaults.mode = Some(String::new());
        assert_eq!(defaults.mode(), None);
        defaults.mode = Some("plan".to_string());
        assert_eq!(defaults.mode().as_deref(), Some("plan"));
    }

    #[test]
    fn acp_defaults_model_treats_empty_as_unset() {
        let mut defaults = AcpAgentDefaults::default();
        assert_eq!(defaults.model(), None);
        defaults.model = Some(String::new());
        assert_eq!(defaults.model(), None);
        defaults.model = Some("openai/gpt-5.5".to_string());
        assert_eq!(defaults.model().as_deref(), Some("openai/gpt-5.5"));
    }

    #[test]
    fn resolve_spawn_model_effort_explicit_request_wins() {
        let defaults = AcpAgentDefaults {
            model: Some("openai/gpt-5.5".to_string()),
            effort: Some("low".to_string()),
            ..Default::default()
        };
        let (model, effort) = resolve_spawn_model_effort(
            Some(&defaults),
            Some("anthropic/claude".to_string()),
            Some("high".to_string()),
        );
        assert_eq!(model.as_deref(), Some("anthropic/claude"));
        assert_eq!(effort.as_deref(), Some("high"));
    }

    #[test]
    fn resolve_spawn_model_effort_falls_back_to_default() {
        let defaults = AcpAgentDefaults {
            model: Some("openai/gpt-5.5".to_string()),
            effort: Some("low".to_string()),
            ..Default::default()
        };
        let (model, effort) = resolve_spawn_model_effort(Some(&defaults), None, None);
        assert_eq!(model.as_deref(), Some("openai/gpt-5.5"));
        assert_eq!(effort.as_deref(), Some("low"));
    }

    #[test]
    fn resolve_spawn_model_effort_blank_request_treated_as_unset() {
        let defaults = AcpAgentDefaults {
            model: Some("openai/gpt-5.5".to_string()),
            effort: Some("low".to_string()),
            ..Default::default()
        };
        let (model, effort) = resolve_spawn_model_effort(
            Some(&defaults),
            Some("   ".to_string()),
            Some(String::new()),
        );
        assert_eq!(model.as_deref(), Some("openai/gpt-5.5"));
        assert_eq!(effort.as_deref(), Some("low"));
    }

    #[test]
    fn resolve_spawn_model_effort_per_model_effort_keyed_on_resolved_model() {
        let mut defaults = AcpAgentDefaults {
            model: Some("gpt-5".to_string()),
            effort: Some("low".to_string()),
            ..Default::default()
        };
        defaults
            .effort_by_model
            .insert("gpt-5".to_string(), "high".to_string());
        // Model resolves to the default gpt-5, so the per-model effort applies.
        let (model, effort) = resolve_spawn_model_effort(Some(&defaults), None, None);
        assert_eq!(model.as_deref(), Some("gpt-5"));
        assert_eq!(effort.as_deref(), Some("high"));
        // An explicit model that has no per-model override falls back to flat.
        let (model, effort) =
            resolve_spawn_model_effort(Some(&defaults), Some("other".to_string()), None);
        assert_eq!(model.as_deref(), Some("other"));
        assert_eq!(effort.as_deref(), Some("low"));
    }

    #[test]
    fn resolve_spawn_model_effort_trims_padded_request_values() {
        let mut defaults = AcpAgentDefaults {
            effort: Some("low".to_string()),
            ..Default::default()
        };
        defaults
            .effort_by_model
            .insert("gpt-5".to_string(), "high".to_string());
        // A padded request model is trimmed before it is retained, so it both
        // persists clean and matches its per-model effort override.
        let (model, effort) = resolve_spawn_model_effort(
            Some(&defaults),
            Some("  gpt-5  ".to_string()),
            Some("  high  ".to_string()),
        );
        assert_eq!(model.as_deref(), Some("gpt-5"));
        assert_eq!(effort.as_deref(), Some("high"));
        // With no explicit effort, the trimmed model still keys the per-model
        // override.
        let (model, effort) =
            resolve_spawn_model_effort(Some(&defaults), Some("  gpt-5  ".to_string()), None);
        assert_eq!(model.as_deref(), Some("gpt-5"));
        assert_eq!(effort.as_deref(), Some("high"));
    }

    #[test]
    fn resolve_spawn_model_effort_no_defaults_no_request_is_none() {
        let (model, effort) = resolve_spawn_model_effort(None, None, None);
        assert_eq!(model, None);
        assert_eq!(effort, None);
    }

    #[test]
    fn test_session_config_confirm_before_quit_defaults_on() {
        // Default-on so existing users get the accidental-exit guard
        // without opting in (#1569).
        assert!(SessionConfig::default().confirm_before_quit);
    }

    #[test]
    fn test_default_on_guards_absent_from_toml_default_on() {
        // An older config.toml with no key for a default-on guard must
        // deserialize to the enabled default, not false. A plain
        // `#[serde(default)]` would give `bool::default()` here and silently
        // strand every pre-existing config on the old behavior.
        let session: SessionConfig = toml::from_str("").unwrap();
        assert!(session.confirm_before_quit, "confirm_before_quit (#1569)");
        assert!(session.confirm_delete, "confirm_delete (#3364)");
    }

    #[test]
    fn test_tool_background_defaults_false_when_absent() {
        let toml = r#"
            [tools.github]
            command = "gh repo view --web"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.tools["github"].background);
    }

    #[test]
    fn test_tool_background_roundtrips_when_enabled() {
        let toml = r#"
            [tools.github]
            command = "gh repo view --web"
            hotkey = "Alt+o"
            background = true
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.tools["github"].background);

        let serialized = toml::to_string_pretty(&cfg).unwrap();
        assert!(serialized.contains("background = true"));
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert!(reparsed.tools["github"].background);
    }

    #[test]
    fn test_tool_background_false_is_omitted() {
        let mut cfg = Config::default();
        cfg.tools.insert(
            "lazygit".to_string(),
            ToolSessionConfig {
                command: "lazygit".to_string(),
                hotkey: None,
                background: false,
            },
        );

        let serialized = toml::to_string_pretty(&cfg).unwrap();
        assert!(!serialized.contains("background"));
    }

    #[test]
    fn test_resolve_tool_command_prefers_command_override() {
        let mut config = SessionConfig::default();
        config
            .agent_command_override
            .insert("my-agent".to_string(), "override-cmd".to_string());
        config
            .custom_agents
            .insert("my-agent".to_string(), "custom-cmd".to_string());
        assert_eq!(config.resolve_tool_command("my-agent"), "override-cmd");
    }

    #[test]
    fn test_resolve_tool_command_falls_back_to_custom_agents() {
        let mut config = SessionConfig::default();
        config
            .custom_agents
            .insert("my-agent".to_string(), "ssh -t host claude".to_string());
        assert_eq!(
            config.resolve_tool_command("my-agent"),
            "ssh -t host claude"
        );
    }

    #[test]
    fn test_resolve_tool_command_skips_empty_override() {
        let mut config = SessionConfig::default();
        config
            .agent_command_override
            .insert("my-agent".to_string(), String::new());
        config
            .custom_agents
            .insert("my-agent".to_string(), "custom-cmd".to_string());
        assert_eq!(config.resolve_tool_command("my-agent"), "custom-cmd");
    }

    #[test]
    fn test_resolve_tool_command_returns_empty_for_unknown() {
        let config = SessionConfig::default();
        assert_eq!(config.resolve_tool_command("nonexistent"), "");
    }

    #[test]
    fn test_agent_config_dir_for_resolves_only_usable_paths() {
        let home = std::path::Path::new("/home/me");
        let mut config = SessionConfig::default();
        for (value, expected) in [
            ("~/.claude-personal", Some("/home/me/.claude-personal")),
            ("~", Some("/home/me")),
            ("/opt/claude", Some("/opt/claude")),
            // A relative path would resolve against AoE's working directory,
            // and an empty one against nothing at all. `~bob` is another
            // user's home, which nothing here expands.
            (".claude-personal", None),
            ("~bob/.claude", None),
            ("", None),
        ] {
            config
                .agent_config_dir
                .insert("my-agent".to_string(), value.to_string());
            assert_eq!(
                config.agent_config_dir_for("my-agent", home),
                expected.map(PathBuf::from),
                "value: {value:?}"
            );
            // What the warning and the settings editor accept must be what
            // resolution keeps, or a value is dropped without a word.
            assert_eq!(
                is_resolvable_agent_config_dir(value),
                expected.is_some(),
                "value: {value:?}"
            );
        }
        assert_eq!(config.agent_config_dir_for("other-agent", home), None);
    }

    #[test]
    fn test_session_config_default_snooze_duration_is_30() {
        let config = SessionConfig::default();
        assert_eq!(
            config.snooze_duration_minutes, 30,
            "default snooze duration must be 30 minutes"
        );
    }

    #[test]
    fn test_validate_snooze_duration_accepts_valid_range() {
        assert!(validate_snooze_duration(1).is_ok());
        assert!(validate_snooze_duration(30).is_ok());
        assert!(validate_snooze_duration(1440).is_ok());
    }

    #[test]
    fn test_validate_snooze_duration_rejects_out_of_range() {
        assert!(validate_snooze_duration(0).is_err());
        assert!(validate_snooze_duration(SNOOZE_MAX_MINUTES + 1).is_err());
    }

    #[test]
    fn test_validate_auto_stop_idle_secs_accepts_u32_range() {
        assert!(validate_auto_stop_idle_secs(0).is_ok());
        assert!(validate_auto_stop_idle_secs(3600).is_ok());
        assert!(validate_auto_stop_idle_secs(u32::MAX as u64).is_ok());
    }

    #[test]
    fn test_validate_auto_stop_idle_secs_rejects_above_u32() {
        assert!(validate_auto_stop_idle_secs(u32::MAX as u64 + 1).is_err());
    }

    #[test]
    fn test_validate_snooze_duration_accepts_dialog_presets() {
        // The TUI dialog presets must all pass the validator; otherwise
        // the API silently rejects what the UI offered. Presets:
        // 1-6h (60-360 min), 24h (1 day), 1 week.
        for &m in &[60u64, 120, 180, 240, 300, 360, 1440, 7 * 1440] {
            assert!(
                validate_snooze_duration(m).is_ok(),
                "preset {m} min must pass validator"
            );
        }
    }

    #[test]
    fn test_custom_agents_roundtrip() {
        let mut config = Config::default();
        config.session.custom_agents.insert(
            "lenovo-claude".to_string(),
            "ssh -t lenovo claude".to_string(),
        );
        config
            .session
            .agent_detect_as
            .insert("lenovo-claude".to_string(), "claude".to_string());

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.session.custom_agents.get("lenovo-claude"),
            Some(&"ssh -t lenovo claude".to_string()),
        );
        assert_eq!(
            deserialized.session.agent_detect_as.get("lenovo-claude"),
            Some(&"claude".to_string()),
        );
    }

    #[test]
    fn test_agent_acp_cmd_roundtrip() {
        let mut config = Config::default();
        config
            .session
            .custom_agents
            .insert("oc-sp".to_string(), "ocp run sp".to_string());
        config
            .session
            .agent_acp_cmd
            .insert("oc-sp".to_string(), "ocp run sp acp".to_string());

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.session.agent_acp_cmd.get("oc-sp"),
            Some(&"ocp run sp acp".to_string()),
        );
    }

    #[test]
    fn test_agent_acp_cmd_defaults_empty() {
        // A config with no agent_acp_cmd must deserialize to an empty
        // map (serde default), not error, so existing configs keep loading.
        let config: Config = toml::from_str("").unwrap();
        assert!(config.session.agent_acp_cmd.is_empty());
    }

    #[test]
    fn test_container_runtime_podman_round_trip() {
        // Users on Linux configure podman via `container_runtime = "podman"`
        // in config.toml; if the snake_case rename ever drifts, their config
        // would silently fall back to the docker default.
        let toml_str = r#"container_runtime = "podman""#;
        let parsed: SandboxConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.container_runtime, ContainerRuntimeName::Podman);

        let serialized = toml::to_string(&parsed).unwrap();
        assert!(serialized.contains(r#"container_runtime = "podman""#));
    }

    #[test]
    fn logging_config_old_shape_populates_new_defaults() {
        // Existing user configs predate output/file_path/rotation; their
        // [logging] section is just default_level + targets. The new fields
        // must populate from serde defaults rather than failing to parse.
        let toml_str = r#"
default_level = "debug"

[targets]
"acp.acp" = "trace"
"#;
        let parsed: LoggingConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.default_level, "debug");
        assert_eq!(parsed.targets.get("acp.acp"), Some(&"trace".to_string()));
        assert_eq!(parsed.output, SinkKind::File);
        assert_eq!(parsed.file_path, "debug.log");
        assert_eq!(parsed.rotation, RotationKind::Size);
        assert_eq!(parsed.max_size_mib, 50);
        assert_eq!(parsed.keep_count, 5);
    }

    #[test]
    fn logging_config_new_shape_round_trip() {
        let toml_str = r#"
default_level = "info"
output = "stdout"
file_path = "/tmp/aoe.log"
rotation = "never"
max_size_mib = 100
keep_count = 10

[targets]
"#;
        let parsed: LoggingConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.output, SinkKind::Stdout);
        assert_eq!(parsed.file_path, "/tmp/aoe.log");
        assert_eq!(parsed.rotation, RotationKind::Never);
        assert_eq!(parsed.max_size_mib, 100);
        assert_eq!(parsed.keep_count, 10);

        let serialized = toml::to_string(&parsed).unwrap();
        let reparsed: LoggingConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.output, SinkKind::Stdout);
        assert_eq!(reparsed.rotation, RotationKind::Never);
    }

    #[test]
    fn test_volume_ignores_strategy_defaults_to_anonymous() {
        let config: SandboxConfig = toml::from_str("").unwrap();
        assert_eq!(
            config.volume_ignores_strategy,
            VolumeIgnoresStrategy::Anonymous
        );
    }

    #[test]
    fn test_volume_ignores_strategy_named_roundtrip() {
        let toml_str = r#"
volume_ignores = ["node_modules"]
volume_ignores_strategy = "named"
"#;
        let config: SandboxConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.volume_ignores_strategy, VolumeIgnoresStrategy::Named);
        assert_eq!(config.volume_ignores, vec!["node_modules"]);

        let serialized = toml::to_string(&config).unwrap();
        let reparsed: SandboxConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.volume_ignores_strategy,
            VolumeIgnoresStrategy::Named
        );
    }

    #[test]
    fn test_volume_ignores_strategy_anonymous_roundtrip() {
        let toml_str = r#"volume_ignores_strategy = "anonymous""#;
        let config: SandboxConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.volume_ignores_strategy,
            VolumeIgnoresStrategy::Anonymous
        );
    }

    // Tests for the config.toml / state.toml split and update_config /
    // update_app_state (#2306-adjacent: long-running-process clobber fix).

    #[test]
    #[serial_test::serial]
    fn update_config_preserves_concurrent_external_edit() {
        let _guard = crate::session::test_support::isolate_app_dir();

        update_config(|c| {
            c.default_profile = "a1".to_string();
            c.session.confirm_before_quit = true;
        })
        .unwrap();

        // Simulate an external `aoe` process writing an unrelated field
        // directly to disk between our load and our next `update_config`
        // call below. `update_config` loads fresh internally, so this must
        // survive.
        let mut external = Config::load().unwrap();
        external.session.confirm_delete = false;
        let table = toml::Table::try_from(&external).unwrap();
        super::super::atomic_write(
            &config_path().unwrap(),
            toml::to_string_pretty(&table).unwrap().as_bytes(),
        )
        .unwrap();

        update_config(|c| {
            c.default_profile = "a2".to_string();
        })
        .unwrap();

        let final_config = Config::load().unwrap();
        assert_eq!(
            final_config.default_profile, "a2",
            "the field update_config touched must be applied"
        );
        assert!(
            !final_config.session.confirm_delete,
            "an external process's concurrent edit to an unrelated field must survive"
        );
    }

    #[test]
    #[serial_test::serial]
    fn update_config_strips_app_state_from_config_toml() {
        let _guard = crate::session::test_support::isolate_app_dir();

        update_config(|c| {
            c.app_state.has_seen_welcome = true;
            c.default_profile = "x".to_string();
        })
        .unwrap();

        let raw = fs::read_to_string(config_path().unwrap()).unwrap();
        let table: toml::Table = raw.parse().unwrap();
        assert!(
            !table.contains_key("app_state"),
            "app_state must never be written into config.toml: {raw}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn config_load_reads_app_state_from_state_toml() {
        let _guard = crate::session::test_support::isolate_app_dir();

        update_app_state(|s| {
            s.has_seen_welcome = true;
        })
        .unwrap();

        let config = Config::load().unwrap();
        assert!(config.app_state.has_seen_welcome);
    }

    #[test]
    #[serial_test::serial]
    fn config_load_ignores_app_state_in_config_toml() {
        let _guard = crate::session::test_support::isolate_app_dir();

        fs::create_dir_all(get_app_dir().unwrap()).unwrap();
        fs::write(
            config_path().unwrap(),
            "[app_state]\nhas_seen_welcome = true\n",
        )
        .unwrap();

        // No state.toml exists, so app_state must default rather than fall
        // back to the stale config.toml value.
        let config = Config::load().unwrap();
        assert!(
            !config.app_state.has_seen_welcome,
            "a stale [app_state] left in config.toml must never be consulted"
        );
    }

    #[test]
    #[serial_test::serial]
    fn update_app_state_roundtrip() {
        let _guard = crate::session::test_support::isolate_app_dir();

        update_app_state(|s| {
            s.has_seen_welcome = true;
            s.tips_seen = vec!["new-from-selection".to_string()];
        })
        .unwrap();

        let loaded = AppStateConfig::load().unwrap();
        assert!(loaded.has_seen_welcome);
        assert_eq!(loaded.tips_seen, vec!["new-from-selection".to_string()]);
    }

    #[test]
    #[serial_test::serial]
    fn update_app_state_applies_mutation_and_persists() {
        let _guard = crate::session::test_support::isolate_app_dir();

        let returned = update_app_state(|s| {
            s.has_seen_web_tour = true;
            42
        })
        .unwrap();
        assert_eq!(
            returned, 42,
            "update_app_state must return the closure's value"
        );

        let loaded = AppStateConfig::load().unwrap();
        assert!(loaded.has_seen_web_tour);
    }

    #[test]
    #[serial_test::serial]
    fn update_config_concurrent_increments_lose_no_updates() {
        let _guard = crate::session::test_support::isolate_app_dir();

        update_config(|c| {
            c.session.snooze_duration_minutes = 1;
        })
        .unwrap();

        let n_threads = 16usize;
        std::thread::scope(|scope| {
            for _ in 0..n_threads {
                scope.spawn(|| {
                    update_config(|c| {
                        c.session.snooze_duration_minutes += 1;
                    })
                    .unwrap();
                });
            }
        });

        let loaded = Config::load().unwrap();
        assert_eq!(
            loaded.session.snooze_duration_minutes as usize,
            1 + n_threads,
            "every concurrent update_config increment must be observed, none lost"
        );
    }

    #[test]
    #[serial_test::serial]
    fn update_app_state_preserves_concurrent_external_edit() {
        let _guard = crate::session::test_support::isolate_app_dir();

        update_app_state(|s| {
            s.has_seen_welcome = true;
            s.has_seen_web_tour = true;
        })
        .unwrap();

        // Simulate an external `aoe` process (e.g. the TUI while `aoe serve`
        // is also running) writing an unrelated field directly to disk
        // between our load and our next `update_app_state` call below.
        // `update_app_state` now loads fresh under a cross-process flock,
        // so this must survive.
        let mut external = AppStateConfig::load().unwrap();
        external.last_seen_version = Some("1.0.0".to_string());
        let table = toml::Table::try_from(&external).unwrap();
        super::super::atomic_write(
            &state_path().unwrap(),
            toml::to_string_pretty(&table).unwrap().as_bytes(),
        )
        .unwrap();

        update_app_state(|s| {
            s.has_seen_welcome = false;
        })
        .unwrap();

        let final_state = AppStateConfig::load().unwrap();
        assert!(
            !final_state.has_seen_welcome,
            "the field update_app_state touched must be applied"
        );
        assert_eq!(
            final_state.last_seen_version,
            Some("1.0.0".to_string()),
            "an external process's concurrent edit to an unrelated field must survive"
        );
    }

    #[test]
    #[serial_test::serial]
    fn update_app_state_concurrent_increments_lose_no_updates() {
        let _guard = crate::session::test_support::isolate_app_dir();

        update_app_state(|s| {
            s.home_list_width = Some(0);
        })
        .unwrap();

        let n_threads = 16usize;
        std::thread::scope(|scope| {
            for _ in 0..n_threads {
                scope.spawn(|| {
                    update_app_state(|s| {
                        s.home_list_width = Some(s.home_list_width.unwrap_or(0) + 1);
                    })
                    .unwrap();
                });
            }
        });

        let loaded = AppStateConfig::load().unwrap();
        assert_eq!(
            loaded.home_list_width,
            Some(n_threads as u16),
            "every concurrent update_app_state increment must be observed, none lost"
        );
    }
}
