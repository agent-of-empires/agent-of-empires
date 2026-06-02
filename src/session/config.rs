//! User configuration management

use super::get_app_dir;
use super::repo_config::HooksConfig;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

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

    #[serde(default)]
    pub sound: crate::sound::SoundConfig,

    #[serde(default)]
    pub status_hooks: crate::status_hooks::StatusHookConfig,

    #[serde(default)]
    pub app_state: AppStateConfig,

    #[serde(default)]
    pub web: WebConfig,

    #[serde(default)]
    pub cockpit: CockpitConfig,

    #[serde(default)]
    pub logging: LoggingConfig,

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub default_level: String,

    #[serde(default)]
    pub targets: std::collections::BTreeMap<String, String>,

    /// Where tracing output goes. `File` is the default; `Stdout` is only
    /// honored for foreground `aoe serve` and env-overridden one-shot CLI
    /// invocations. TUI, daemon child, and cockpit runners coerce to `File`
    /// because their alt-screen / detached-stdio would otherwise corrupt or
    /// discard the output.
    #[serde(default)]
    pub output: SinkKind,

    /// Log file location. Relative paths resolve under the app data dir;
    /// absolute paths are used verbatim. Defaults to `debug.log`.
    #[serde(default = "default_file_path")]
    pub file_path: String,

    /// Rotation policy. `Size` rotates when the live file crosses
    /// `max_size_mib`; `Never` disables rotation.
    #[serde(default)]
    pub rotation: RotationKind,

    /// Size threshold (MiB) for `RotationKind::Size`. The file may
    /// overshoot the threshold slightly between stat checks; bounded by
    /// the writer's stat-on-tick interval.
    #[serde(default = "default_max_size_mib")]
    pub max_size_mib: u64,

    /// How many rotated files to retain (`.1` through `.keep_count`).
    /// Older rotations are dropped.
    #[serde(default = "default_keep_count")]
    pub keep_count: u8,

    /// Whether the tracing formatter prefixes each event with the names
    /// and fields of the spans wrapping it (e.g. the per-request
    /// `http_request{request_id=... method=GET path=...}` introduced by
    /// the axum middleware). Useful for grep-correlation when triaging
    /// across async boundaries, noisy on idle polling endpoints.
    /// Defaults to `false` so the log stays readable; flip to `true`
    /// when investigating. Requires restart.
    #[serde(default = "default_show_spans")]
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

/// Configuration for the cockpit (ACP-based native rendering of agent
/// state). Defaults match the documented v4 design and v005 migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CockpitConfig {
    /// Master kill switch for cockpit mode. When false, every session
    /// runs as plain tmux even if --cockpit is passed.
    #[serde(default)]
    pub enabled: bool,
    /// On mobile viewports, default new Claude sessions to cockpit mode.
    #[serde(default = "default_true")]
    pub default_for_claude: bool,
    /// The agent name to use when --agent is not specified.
    #[serde(default = "default_agent")]
    pub default_agent: String,
    /// Hard cap on simultaneously running agent worker subprocesses.
    #[serde(default = "default_max_workers")]
    pub max_concurrent_workers: u32,
    /// Replay buffer event-count cap (per session).
    #[serde(default = "default_replay_events")]
    pub replay_events: u32,
    /// Replay buffer byte cap (per session).
    #[serde(default = "default_replay_bytes")]
    pub replay_bytes: u64,
    /// Optional path to the Node runtime used to spawn aoe-agent. If
    /// empty, aoe resolves Node via PATH then bundled fallback.
    #[serde(default)]
    pub node_path: String,
    /// Whether the cockpit web UI shows a per-tool elapsed-time label on
    /// every tool card. Default true. Honoured by the web client via
    /// `ServerAbout.cockpit_show_tool_durations` so toggling here flows
    /// across every device that connects to the same daemon. The
    /// underlying measurement is currently imprecise on
    /// claude-agent-acp (no `status: "in_progress"` is emitted, so we
    /// can't re-stamp `started_at` to the real subprocess start;
    /// see the comment on `CardChromeProps.startedAt` in
    /// `web/src/components/cockpit/ToolCards.tsx`); this setting lets
    /// users hide the label until upstream provides a trustworthy
    /// "subprocess started" signal.
    #[serde(default = "default_true")]
    pub show_tool_durations: bool,
    /// How the web composer drains client-side queued follow-up prompts
    /// when the agent finishes a turn (see #1031). `Combined` (default)
    /// joins every queued entry with a blank line and dispatches as one
    /// prompt; `Serial` pops the head and waits for the next Stopped to
    /// fire the next entry. The setting is surfaced via
    /// `ServerAbout.cockpit_queue_drain_mode` so toggling it here flows
    /// to every connected web client without restarting the daemon.
    #[serde(default)]
    pub queue_drain_mode: QueueDrainMode,
    /// Maximum number of cockpit worker resumes (spawn or attach) the
    /// reconciler runs in parallel on `aoe serve` cold start. Bounded
    /// at runtime by `min(max_concurrent_resumes, max_concurrent_workers).max(1)`
    /// so this knob can never exceed the total live worker cap. Default
    /// is 4: Node.js bootup is memory-heavy and 4 concurrent
    /// claude-agent-acp processes are around 200-320MB transient. Lower
    /// it on constrained hosts; raise on beefier machines. See #1088.
    #[serde(default = "default_max_concurrent_resumes")]
    pub max_concurrent_resumes: u32,
    /// Seconds of streaming inactivity after which the cockpit web UI
    /// shows a "Force end turn" button. When `turnActive=true` and no
    /// frame arrives for this long, the spinner is likely stuck on a
    /// missed `Stopped` (#1100); the button locally clears the
    /// spinner and POSTs to force_end_turn so the daemon publishes a
    /// synthetic `Stopped { reason: "user_forced" }` and best-effort
    /// `session/cancel` the agent. Default 30s.
    #[serde(default = "default_force_end_turn_threshold_secs")]
    pub force_end_turn_threshold_secs: u32,
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
    pub silent_orphan_grace_secs: u32,
    /// Silent-orphan watchdog: accelerated grace used when the current
    /// prompt has already received a cost-populated `UsageUpdate`
    /// notification (claude-agent-acp's "wrap up accounting" marker
    /// emitted just before `PromptResponse`). Lowers MTTR on the known
    /// adapter wedge without weakening the vendor-agnostic baseline.
    /// Default 20s. If `silent_orphan_grace_secs` is 0 (disabled), this
    /// has no effect. See #1240.
    #[serde(default = "default_silent_orphan_fast_grace_secs")]
    pub silent_orphan_fast_grace_secs: u32,
    /// Auto-stop idle cockpit workers: seconds of inactivity (no cockpit
    /// events and no in-flight turn) after which the daemon shuts a
    /// worker down and marks its session dormant so the reconciler does
    /// not respawn it. The next user prompt wakes the session and the
    /// reconciler spawns a fresh worker. `0` (default) disables the
    /// feature entirely; no worker is ever stopped for inactivity. See
    /// #1689.
    #[serde(default = "default_auto_stop_idle_secs")]
    pub auto_stop_idle_secs: u32,
}

fn default_auto_stop_idle_secs() -> u32 {
    0
}

fn default_max_concurrent_resumes() -> u32 {
    4
}

fn default_force_end_turn_threshold_secs() -> u32 {
    30
}

fn default_silent_orphan_grace_secs() -> u32 {
    120
}

fn default_silent_orphan_fast_grace_secs() -> u32 {
    20
}

/// Drain strategy for the cockpit composer's client-side prompt queue.
/// See `CockpitConfig::queue_drain_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueDrainMode {
    /// Join every queued entry with `\n\n` and dispatch as a single
    /// prompt when the current turn ends. One response covers the whole
    /// batch.
    #[default]
    Combined,
    /// Pop the head off the queue and dispatch it; wait for the next
    /// Stopped event before firing the following entry. One response
    /// per queued entry.
    Serial,
}

impl QueueDrainMode {
    pub fn as_str(self) -> &'static str {
        match self {
            QueueDrainMode::Combined => "combined",
            QueueDrainMode::Serial => "serial",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "combined" => Some(QueueDrainMode::Combined),
            "serial" => Some(QueueDrainMode::Serial),
            _ => None,
        }
    }
}

impl Default for CockpitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_for_claude: true,
            default_agent: default_agent(),
            max_concurrent_workers: default_max_workers(),
            replay_events: default_replay_events(),
            replay_bytes: default_replay_bytes(),
            node_path: String::new(),
            show_tool_durations: true,
            queue_drain_mode: QueueDrainMode::default(),
            max_concurrent_resumes: default_max_concurrent_resumes(),
            force_end_turn_threshold_secs: default_force_end_turn_threshold_secs(),
            silent_orphan_grace_secs: default_silent_orphan_grace_secs(),
            silent_orphan_fast_grace_secs: default_silent_orphan_fast_grace_secs(),
            auto_stop_idle_secs: default_auto_stop_idle_secs(),
        }
    }
}

fn default_agent() -> String {
    "aoe-agent".to_string()
}
fn default_max_workers() -> u32 {
    5
}
fn default_replay_events() -> u32 {
    // 0 = unlimited. The event store's prune already gates on `> 0`
    // (see `EventStore::record`), so the default flip is end-to-end
    // safe: a fresh install never truncates history; users who want a
    // ceiling for disk-space reasons can set a non-zero value in
    // config.toml or the settings TUI. See #1065.
    0
}
fn default_replay_bytes() -> u64 {
    5_242_880
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
}

impl GroupByMode {
    pub fn cycle(self) -> Self {
        match self {
            GroupByMode::Manual => GroupByMode::Project,
            GroupByMode::Project => GroupByMode::Manual,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GroupByMode::Manual => "Manual",
            GroupByMode::Project => "Project",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppStateConfig {
    #[serde(default)]
    pub has_seen_welcome: bool,

    #[serde(default)]
    pub last_seen_version: Option<String>,

    /// Latest version for which the user dismissed the update banner. The
    /// banner stays hidden as long as the latest available version equals
    /// this value; it returns automatically when a newer release ships.
    /// Cleared by switching `update_check_mode` or upgrading past the
    /// snoozed version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_update_version: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_list_width: Option<u16>,

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

    #[serde(default)]
    pub has_acknowledged_agent_hooks: bool,

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
}

/// Session-related configuration defaults
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Default coding tool for new sessions (claude, opencode, vibe, codex)
    /// If not set or tool is unavailable, falls back to first available tool
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tool: Option<String>,

    /// Enable YOLO mode by default for new sessions (skip permission prompts)
    #[serde(default)]
    pub yolo_mode_default: bool,

    /// Per-agent extra arguments appended after the binary (e.g., opencode = "--port 8080")
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub agent_extra_args: HashMap<String, String>,

    /// Per-agent command override replacing the binary entirely (e.g., claude = "happy cli claude")
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub agent_command_override: HashMap<String, String>,

    /// Install status-detection hooks into the agent's settings file (e.g. ~/.claude/settings.json).
    /// When disabled, AoE will not modify the agent's settings file. Status detection falls back
    /// to tmux pane content parsing, which is less reliable.
    #[serde(default = "default_true")]
    pub agent_status_hooks: bool,

    /// Request xterm mouse tracking from the terminal so the TUI handles the
    /// scroll wheel (preview-pane scroll, #795) and click-to-select rows.
    /// Disable to hand wheel and text selection back to the terminal, e.g.
    /// iOS Mosh + Termius/Blink, which don't reliably forward mouse-tracking
    /// escapes. The `AOE_MOUSE_CAPTURE` env var stays as an opt-out backstop:
    /// capture is requested only when this is true and the env var hasn't
    /// disabled it. Default on.
    #[serde(default = "default_true")]
    pub mouse_capture: bool,

    /// User-defined custom agents: name -> launch command
    /// (e.g., "lenovo-claude" = "ssh -t lenovo claude").
    /// Custom agent names appear in the TUI agent picker alongside built-in agents.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom_agents: HashMap<String, String>,

    /// Status detection mapping: agent name -> built-in agent name
    /// (e.g., "lenovo-claude" = "claude").
    /// Maps a custom (or built-in) agent to another agent's status detection heuristics.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub agent_detect_as: HashMap<String, String>,

    /// ACP launch command for a custom agent, enabling it to run in the
    /// structured cockpit UI (e.g., "oc-superpowers" = "ocp run sp acp").
    /// A custom agent with an entry here is cockpit-capable; without one it
    /// is tmux-only.
    ///
    /// Note: unlike `custom_agents` (a shell command run in a tmux pane),
    /// this value is split into argv with shell-word rules and executed
    /// directly, with no shell. For shell features, wrap explicitly, e.g.
    /// `sh -lc 'source ~/.profile && ocp run sp acp'`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub agent_cockpit_cmd: HashMap<String, String>,

    /// Require SHIFT on letter-based TUI hotkeys (e.g. SHIFT+N for New, SHIFT+D for Delete).
    /// Guards against accidental destructive actions from dictation software, a forgotten
    /// focus, or stray keystrokes. Navigation keys (h/j/k/l, arrows, Enter, Esc), punctuation
    /// (/, ?), and numeric modifiers stay unshifted. Previously-uppercase bindings
    /// (P, R, T, N, D, G) relocate to Ctrl+letter so nothing is lost.
    /// Note: Ctrl+D (diff view) may conflict with terminal EOF in some tmux configs;
    /// if so, rebind tmux's send-prefix or use the `D` key from the help overlay.
    /// Off by default; existing users keep the legacy single-letter UX.
    #[serde(default)]
    pub strict_hotkeys: bool,

    /// How long (in minutes) to snooze a session when the user presses
    /// `w`/`W` or runs `aoe session snooze`. During the snooze window the
    /// session is treated like archive: sinks to the bottom, renders
    /// italic+dim with a `z ` prefix, ignored by the attention sort,
    /// then rejoins the active list automatically when the timer expires.
    /// Default: 30 minutes.
    #[serde(default = "default_snooze_duration_minutes")]
    pub snooze_duration_minutes: u32,

    /// Seconds of inactivity after which a plain TUI/tmux session that has
    /// been `Idle` this long is auto-stopped (its tmux session and any
    /// sandbox container are killed and the row becomes a restartable
    /// `Stopped` row). `0` disables (default); no session is ever auto-stopped
    /// for inactivity. Idle age is anchored on the later of the last
    /// transition into `Idle` and the last user interaction, and a session
    /// with a currently attached tmux client is never stopped, so a session
    /// the user is reading is spared. Checked about once a minute, so the stop
    /// can lag the threshold by up to a minute. Cockpit workers use the
    /// separate `cockpit.auto_stop_idle_secs` knob; see #1689 and #1690.
    #[serde(default = "default_auto_stop_idle_secs")]
    pub auto_stop_idle_secs: u32,

    /// Text sent to the agent after a successful `aoe session restart` /
    /// `e`-keybind restart, once the post-restart readiness probe says the
    /// pane is alive. Restart re-execs the agent at a blank prompt; this
    /// nudge tells the agent to pick up where it left off. Set to an
    /// empty string to disable the wake-up message entirely (the restart
    /// itself still runs).
    #[serde(default = "default_restart_wake_message")]
    pub restart_wake_message: String,

    /// Per-row label shown next to the session title in the home view.
    /// `Auto` (default) preserves the historical UX: show a profile short
    /// code in all-profiles view and nothing in filtered views. Other
    /// variants override that to always show the chosen tag, or to
    /// suppress the row label entirely.
    #[serde(default)]
    pub row_tag: RowTagMode,

    /// Process-wide cap on concurrent session-id poller threads (one per
    /// live session). Edited via the TUI Settings panel; saving in Global
    /// scope pushes the new value into the runtime atomic for new sessions
    /// to pick up immediately.
    #[serde(default = "default_session_id_poller_max_threads")]
    pub session_id_poller_max_threads: u32,

    /// Comma-separated list of chord specs that exit live-send mode.
    /// Each chord is a tmux-style spec like `C-q`, `M-x`, `F12`; the
    /// first chord in the list that matches an event ends live mode.
    /// Default is `C-q` alone: mobile-friendly, passes through Termius,
    /// well-known as a quit chord, and verified to survive every common
    /// macOS terminal config (unlike `C-]` and `C-\`, both of which
    /// fail silently on at least one combination). Customize when the
    /// default conflicts with a workflow (vim quoted-insert needs `C-q`
    /// passthrough, so swap to `F12,M-q` or any chord that's free).
    #[serde(default = "default_live_send_exit_chord")]
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
    pub live_send_leader: String,

    /// What the TUI does immediately after a new session finishes
    /// creating. `Tmux` (default) drops into the tmux attach view, the
    /// historical behavior. `LiveSend` enters live-send mode against
    /// the new session's pane instead, so users who never want to be
    /// inside tmux directly can create-and-type without an extra
    /// keystroke. Cockpit-mode sessions ignore this setting because
    /// neither tmux nor live-send applies to them.
    #[serde(default)]
    pub new_session_attach_mode: NewSessionAttachMode,

    /// What `Enter` (and double-click) does on an existing session
    /// row in the Agent view. `Tmux` (default) attaches to the tmux
    /// pane, the historical behavior. `LiveSend` enters live-send
    /// mode instead so the TUI keeps the home list visible and pipes
    /// keystrokes through to the agent. Terminal/Tool views and
    /// cockpit-mode sessions ignore this setting; they keep their
    /// existing activation paths (terminal attach, cockpit open).
    #[serde(default)]
    pub default_attach_mode: NewSessionAttachMode,

    /// What a single mouse click on a session row does in the Agent
    /// view. `LiveSend` (default) enters live-send mode for that row,
    /// the historical behavior. `SelectOnly` just moves the cursor
    /// to the row so the user can read the preview without ever
    /// entering live-send. Double-click still activates via
    /// `default_attach_mode` regardless of this setting.
    #[serde(default)]
    pub click_action: ClickAction,

    /// Show a "quit aoe?" confirmation when the user presses `q` to leave
    /// the home screen, guarding against accidental exits (e.g. a Ctrl+Q
    /// live-mode-exit habit landing on the home view). On by default;
    /// the dialog offers a "don't warn me again" option that flips this
    /// off. Ctrl+C still force-quits without a prompt.
    #[serde(default = "default_true")]
    pub confirm_before_quit: bool,
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

/// What the TUI does after a new session is created. See
/// `SessionConfig::new_session_attach_mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewSessionAttachMode {
    /// Attach to the new session's tmux pane (the historical
    /// behavior; the user lands inside tmux with the agent running).
    #[default]
    Tmux,
    /// Enter live-send mode against the new session's pane: the agent
    /// runs in the background, the TUI stays on the home view, and
    /// keystrokes pipe straight to the agent. Users who never want to
    /// see a raw tmux session pick this so creating a session never
    /// detaches them from the home list.
    LiveSend,
}

/// What to render in the per-row tag slot next to the session title.
///
/// Defaults to `None` so existing users see no behavior change. Power
/// users opt in via Settings: pick `Auto` (profile tag in all-profiles
/// view only), `Profile`, `Sandbox`, or `Branch`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowTagMode {
    /// Never render a per-row tag. The historical behavior on `main`
    /// before the row-tag feature landed; default so the feature is
    /// fully opt-in.
    #[default]
    None,
    /// Show the profile short code in all-profiles view, nothing in
    /// filtered views.
    Auto,
    /// Always render the profile short code (`fb` for `forit-backup`).
    Profile,
    /// Render `sb` on sandboxed sessions, nothing on host sessions.
    Sandbox,
    /// Render the worktree branch name (last segment if `/`-namespaced,
    /// truncated to 8 chars).
    Branch,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            default_tool: None,
            yolo_mode_default: false,
            agent_extra_args: HashMap::new(),
            agent_command_override: HashMap::new(),
            agent_status_hooks: true,
            mouse_capture: true,
            custom_agents: HashMap::new(),
            agent_detect_as: HashMap::new(),
            agent_cockpit_cmd: HashMap::new(),
            strict_hotkeys: false,
            snooze_duration_minutes: 30,
            auto_stop_idle_secs: default_auto_stop_idle_secs(),
            restart_wake_message: default_restart_wake_message(),
            row_tag: RowTagMode::default(),
            session_id_poller_max_threads: default_session_id_poller_max_threads(),
            live_send_exit_chord: default_live_send_exit_chord(),
            live_send_leader: default_live_send_leader(),
            new_session_attach_mode: NewSessionAttachMode::default(),
            default_attach_mode: NewSessionAttachMode::default(),
            click_action: ClickAction::default(),
            confirm_before_quit: true,
        }
    }
}

fn default_snooze_duration_minutes() -> u32 {
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

fn default_session_id_poller_max_threads() -> u32 {
    crate::session::poller::DEFAULT_SESSION_ID_POLLER_MAX_THREADS
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
        for (name, command) in &self.agent_cockpit_cmd {
            if name.is_empty() {
                tracing::warn!(target: "session.store", "agent_cockpit_cmd: entry with empty agent name will be ignored");
                continue;
            }
            if crate::agents::get_agent(name).is_some() {
                tracing::warn!(target: "session.store",
                    "agent_cockpit_cmd: '{}' shadows a built-in agent; built-in agents already have a cockpit adapter and the entry will be ignored",
                    name
                );
                continue;
            }
            if !self.custom_agents.contains_key(name) {
                tracing::warn!(target: "session.store",
                    "agent_cockpit_cmd: '{}' has no matching custom_agents entry; it will not appear in the agent picker",
                    name
                );
            }
            match shell_words::split(command) {
                Ok(argv) if argv.is_empty() => {
                    tracing::warn!(target: "session.store",
                        "agent_cockpit_cmd: '{}' has an empty command, cockpit will be unavailable for it",
                        name
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(target: "session.store",
                        "agent_cockpit_cmd: '{}' has a malformed command ({}), cockpit will be unavailable for it",
                        name, e
                    );
                }
            }
        }
    }
}

/// Diff view configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffConfig {
    /// Default branch to compare against (e.g., "main", "master")
    /// If not set, will try to auto-detect from the repository
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,

    /// Number of context lines to show around changes
    #[serde(default = "default_context_lines")]
    pub context_lines: usize,

    /// Render diffs side-by-side (split) instead of unified.
    #[serde(default)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebConfig {
    /// Operator kill switch for browser push notifications. When false,
    /// `/api/push/*` returns 404 and the status-change consumer drops
    /// events without sending. Existing subscriptions persist across
    /// flips, so toggling back to true resumes delivery without requiring
    /// users to re-opt-in.
    #[serde(default = "default_true")]
    pub notifications_enabled: bool,

    /// Server-wide default: fire a push on Running to Waiting transitions.
    /// Sessions can override per-session via `Instance.notify_on_waiting`.
    #[serde(default = "default_true")]
    pub notify_on_waiting: bool,

    /// Server-wide default: fire a push on Running to Idle transitions.
    /// Off by default because Idle fires on every session completion and
    /// gets spammy quickly. Sessions can opt in via `Instance.notify_on_idle`.
    #[serde(default)]
    pub notify_on_idle: bool,

    /// Server-wide default: fire a push on Running to Error transitions.
    #[serde(default = "default_true")]
    pub notify_on_error: bool,

    /// Server-wide default: fire a push when a cockpit session's
    /// `ScheduleWakeup` timer fires (the next /loop turn starts). On by
    /// default because the headline use case for `/loop` dynamic mode
    /// is "walk away during the sleep window"; without a push the
    /// user has to keep peeking at the dashboard. Suppression for
    /// active TUI / web sessions still applies. See #1091.
    #[serde(default = "default_true")]
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
    /// Emit 256-palette escapes (\e[38;5;<idx>m) by converting every theme
    /// Rgb(r,g,b) to the nearest xterm-256 index. Use this when the transport
    /// (notably some mosh clients) mishandles 24-bit RGB; preview panes in
    /// aoe already use 256-palette via ansi-to-tui, so palette mode renders
    /// chrome through the same escape path and survives the same transports.
    Palette,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default)]
    pub name: String,
    /// How theme colors are emitted at the escape-sequence level.
    /// See `ColorMode` for the truecolor vs palette trade-off.
    #[serde(default)]
    pub color_mode: ColorMode,
    /// Minutes a freshly-stopped Idle session keeps the fresh-idle color
    /// and animated `breathe` rattle before snapping back to the regular
    /// static idle look. Sessions inside the window are also included in
    /// the `w` keybind's "needs attention" bucket.
    ///
    /// Default is `0` (off): the freshness rattle and fresh-idle color
    /// stay off, every Idle row renders with the regular static look
    /// the moment its Stop hook fires. The time-since-stop column on
    /// Idle rows is independent of this setting and shows regardless.
    /// Set a positive value (e.g. 20) to opt in to the visual freshness
    /// signal.
    #[serde(default = "default_idle_decay_minutes")]
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
/// default: shows the TUI banner and, when `notify_in_cli` is true, the
/// CLI eprintln nag. `Off` suppresses every check, banner, and fetch.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatesConfig {
    /// How updates are surfaced (auto / notify / off). Replaces the
    /// legacy `check_enabled` boolean (see migration `v009`).
    #[serde(default)]
    pub update_check_mode: UpdateCheckMode,

    #[serde(default = "default_check_interval")]
    pub check_interval_hours: u64,

    #[serde(default = "default_true")]
    pub notify_in_cli: bool,

    /// How often the web dashboard re-polls `/api/system/update-status`
    /// while a tab is open. Server-side cache is governed by
    /// `check_interval_hours`; this knob only controls how aggressively
    /// the frontend asks. Keep it lower than `check_interval_hours * 60`
    /// or every poll is a cache hit. See #984.
    #[serde(default = "default_web_poll_interval_minutes")]
    pub web_poll_interval_minutes: u64,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            update_check_mode: UpdateCheckMode::default(),
            check_interval_hours: 24,
            notify_in_cli: true,
            web_poll_interval_minutes: 60,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_check_interval() -> u64 {
    24
}

fn default_web_poll_interval_minutes() -> u64 {
    60
}

/// Anonymous, opt-in usage telemetry. Off by default; mirrors the privacy
/// posture of [`UpdatesConfig`] (the only other outbound call in the tool).
///
/// The single `enabled` flag is the consent boundary the user controls in
/// every settings surface. The anonymous install id lives in a dedicated
/// `telemetry.json` (NOT here), so pasting `config.toml` into a bug report
/// can never leak it. `DO_NOT_TRACK` overrides this flag at runtime and
/// suppresses both sending and id generation regardless of its value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// User opted in to anonymous usage telemetry. Defaults to `false`.
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_worktree_template")]
    pub path_template: String,

    /// Path template for bare repo setups (linked worktree pattern).
    /// Defaults to "./{branch}" to keep worktrees as siblings within the repo directory.
    #[serde(default = "default_bare_repo_template")]
    pub bare_repo_path_template: String,

    #[serde(default = "default_true")]
    pub auto_cleanup: bool,

    #[serde(default = "default_true")]
    pub show_branch_in_tui: bool,

    /// When deleting a worktree, also delete the associated git branch.
    /// Default: false (unchecked in delete dialog)
    #[serde(default)]
    pub delete_branch_on_cleanup: bool,

    /// Path template for multi-repo workspace directories.
    /// Supports {branch} and {session-id} placeholders.
    #[serde(default = "default_workspace_template")]
    pub workspace_path_template: String,

    /// Run `git submodule update --init --recursive` after creating a worktree
    /// when the checkout contains a `.gitmodules` file. Defaults to true to
    /// preserve the behavior introduced in #942. Disable for repos with large
    /// or deeply-nested submodule trees that you don't need inside agent
    /// sessions; new sessions then finish creating instead of stalling in
    /// `Creating…` while submodules clone.
    #[serde(default = "default_true")]
    pub init_submodules: bool,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path_template: default_worktree_template(),
            bare_repo_path_template: default_bare_repo_template(),
            auto_cleanup: true,
            show_branch_in_tui: true,
            delete_branch_on_cleanup: false,
            workspace_path_template: default_workspace_template(),
            init_submodules: true,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub enabled_by_default: bool,

    #[serde(default = "default_sandbox_image")]
    pub default_image: String,

    #[serde(default, deserialize_with = "super::serde_helpers::string_or_vec")]
    pub extra_volumes: Vec<String>,

    #[serde(
        default = "default_sandbox_environment",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    pub environment: Vec<String>,

    #[serde(default = "default_true")]
    pub auto_cleanup: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<String>,

    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    pub port_mappings: Vec<String>,

    /// Default terminal mode for sandboxed sessions (host or container)
    #[serde(default)]
    pub default_terminal_mode: DefaultTerminalMode,

    /// Relative directory paths to exclude from the host bind mount via anonymous volumes
    #[serde(default, deserialize_with = "super::serde_helpers::string_or_vec")]
    pub volume_ignores: Vec<String>,

    /// Strategy for mounting volume_ignores paths (anonymous or named)
    #[serde(default)]
    pub volume_ignores_strategy: VolumeIgnoresStrategy,

    /// Mount ~/.ssh into sandbox containers (default: false)
    #[serde(default)]
    pub mount_ssh: bool,

    /// Append the SELinux relabel flag (`:z`) to sandbox bind mounts so the
    /// container can read them on SELinux-enforcing hosts (Fedora, RHEL). Off by
    /// default; only emitted for Docker/Podman. Note: this relabels the host paths.
    #[serde(default)]
    pub selinux_relabel: bool,

    /// Custom instruction text appended to the agent's system prompt in sandboxed sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instruction: Option<String>,

    /// Container runtime to use for sandboxing (docker, podman, or apple_container)
    #[serde(default)]
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
            default_terminal_mode: DefaultTerminalMode::default(),
            volume_ignores: Vec::new(),
            volume_ignores_strategy: VolumeIgnoresStrategy::default(),
            mount_ssh: false,
            selinux_relabel: false,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TmuxStatusBarMode {
    #[default]
    Auto,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TmuxMouseMode {
    /// Only enable mouse if user doesn't have their own tmux config
    #[default]
    Auto,
    /// Always enable mouse for aoe sessions
    Enabled,
    /// Never enable mouse for aoe sessions (explicitly disable)
    Disabled,
}

/// Controls whether aoe configures tmux to forward OSC 52 clipboard escape
/// sequences from inner TUIs (Claude Code, OpenCode, Codex, etc.) to the
/// outer terminal. Without this, "select to copy" inside the wrapped agent
/// silently fails because tmux swallows the escape sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TmuxClipboardMode {
    /// Apply clipboard pass-through only if the user has no tmux config
    #[default]
    Auto,
    /// Always apply clipboard pass-through to aoe sessions
    Enabled,
    /// Never apply clipboard pass-through (use plain tmux defaults)
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxConfig {
    #[serde(default)]
    pub status_bar: TmuxStatusBarMode,

    /// Mouse support mode (auto, enabled, disabled)
    #[serde(default)]
    pub mouse: TmuxMouseMode,

    /// Clipboard pass-through mode (auto, enabled, disabled). Controls
    /// `set-clipboard on` and `allow-passthrough on` so OSC 52 from the
    /// wrapped agent reaches the terminal.
    #[serde(default)]
    pub clipboard: TmuxClipboardMode,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            status_bar: TmuxStatusBarMode::Auto,
            mouse: TmuxMouseMode::Auto,
            clipboard: TmuxClipboardMode::Auto,
        }
    }
}

/// Check if user has a tmux configuration file.
/// Returns true if ~/.tmux.conf or ~/.config/tmux/tmux.conf exists.
pub fn user_has_tmux_config() -> bool {
    if let Some(home) = dirs::home_dir() {
        let traditional = home.join(".tmux.conf");
        let xdg = home.join(".config").join("tmux").join("tmux.conf");
        return traditional.exists() || xdg.exists();
    }
    false
}

/// Determine if status bar styling should be applied based on config and environment.
pub fn should_apply_tmux_status_bar() -> bool {
    let config = Config::load_or_warn();
    match config.tmux.status_bar {
        TmuxStatusBarMode::Enabled => true,
        TmuxStatusBarMode::Disabled => false,
        TmuxStatusBarMode::Auto => !user_has_tmux_config(),
    }
}

/// Determine if mouse support should be enabled based on config and environment.
/// Returns Some(true) to enable, Some(false) to disable, None to not touch the setting.
pub fn should_apply_tmux_mouse() -> Option<bool> {
    let config = Config::load_or_warn();
    match config.tmux.mouse {
        TmuxMouseMode::Enabled => Some(true),
        TmuxMouseMode::Disabled => Some(false),
        TmuxMouseMode::Auto => {
            // In auto mode, only enable mouse if user doesn't have their own tmux config
            if user_has_tmux_config() {
                None // Don't touch - let user's config apply
            } else {
                Some(true) // Enable mouse for users without custom config
            }
        }
    }
}

/// Determine if clipboard pass-through (`set-clipboard on` +
/// `allow-passthrough on`) should be applied. Auto enables it when the user
/// has no tmux config of their own; users with custom tmux configs are
/// expected to manage these options themselves.
pub fn should_apply_tmux_clipboard() -> bool {
    let config = Config::load_or_warn();
    match config.tmux.clipboard {
        TmuxClipboardMode::Enabled => true,
        TmuxClipboardMode::Disabled => false,
        TmuxClipboardMode::Auto => !user_has_tmux_config(),
    }
}

pub(crate) fn config_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("config.toml"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }

        let content = fs::read_to_string(&path)?;
        let mut config: Config = toml::from_str(&content)?;
        config.normalize();
        Ok(config)
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

    /// Clamp invariants that the type system can't enforce. Keeps config,
    /// TUI, and runtime in agreement when a user hand-edits a value below
    /// its minimum (zero would silently disable session-id polling).
    fn normalize(&mut self) {
        if self.session.session_id_poller_max_threads == 0 {
            self.session.session_id_poller_max_threads = 1;
        }
    }
}

pub fn load_config() -> Result<Option<Config>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(Config::load()?))
}

pub fn save_config(config: &Config) -> Result<()> {
    let path = config_path()?;
    let content = toml::to_string_pretty(config)?;
    super::atomic_write(&path, content.as_bytes())?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
        #[cfg(target_os = "linux")]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        #[cfg(target_os = "linux")]
        let app_dir = temp_home
            .path()
            .join(".config")
            .join(crate::session::APP_DIR_NAME_LINUX);
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(target_os = "linux")]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        #[cfg(target_os = "linux")]
        let app_dir = temp_home
            .path()
            .join(".config")
            .join(crate::session::APP_DIR_NAME_LINUX);
        #[cfg(not(target_os = "linux"))]
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
        assert_eq!(updates.check_interval_hours, 24);
        assert!(updates.notify_in_cli);
    }

    #[test]
    fn test_updates_config_deserialize() {
        let toml = r#"
            update_check_mode = "off"
            check_interval_hours = 12
            notify_in_cli = false
        "#;
        let updates: UpdatesConfig = toml::from_str(toml).unwrap();
        assert_eq!(updates.update_check_mode, UpdateCheckMode::Off);
        assert_eq!(updates.check_interval_hours, 12);
        assert!(!updates.notify_in_cli);
    }

    #[test]
    fn test_updates_config_partial_deserialize() {
        let toml = r#"update_check_mode = "auto""#;
        let updates: UpdatesConfig = toml::from_str(toml).unwrap();
        assert_eq!(updates.update_check_mode, UpdateCheckMode::Auto);
        assert_eq!(updates.check_interval_hours, 24);
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

    /// Regression: the previous schema had `check_enabled = bool` and
    /// `auto_update = bool` on UpdatesConfig. Both fields are gone now;
    /// the on-disk migration runs at startup, but configs read between
    /// upgrade and migration must still deserialize cleanly with the
    /// unknown fields silently dropped by serde.
    #[test]
    fn test_legacy_check_enabled_and_auto_update_are_ignored() {
        let old_toml = r#"
            check_enabled = false
            auto_update = true
            check_interval_hours = 12
            notify_in_cli = true
        "#;
        let updates: UpdatesConfig =
            toml::from_str(old_toml).expect("legacy fields should not error");
        assert_eq!(updates.check_interval_hours, 12);
        assert_eq!(updates.update_check_mode, UpdateCheckMode::Notify);
        assert!(updates.notify_in_cli);
    }

    // Tests for WorktreeConfig
    #[test]
    fn test_worktree_config_default() {
        let wt = WorktreeConfig::default();
        assert!(!wt.enabled);
        assert_eq!(wt.path_template, "../{repo-name}-worktrees/{branch}");
        assert!(wt.auto_cleanup);
        assert!(wt.show_branch_in_tui);
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
            show_branch_in_tui = false
            init_submodules = false
        "#;
        let wt: WorktreeConfig = toml::from_str(toml).unwrap();
        assert!(wt.enabled);
        assert_eq!(wt.path_template, "/custom/{branch}");
        assert!(!wt.auto_cleanup);
        assert!(!wt.show_branch_in_tui);
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
        assert_eq!(app.last_seen_version, Some("1.0.0".to_string()));
        assert_eq!(app.dismissed_update_version, Some("1.0.0".to_string()));
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
                check_interval_hours: 48,
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
            config.updates.check_interval_hours,
            deserialized.updates.check_interval_hours
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
            check_interval_hours = 12

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
        assert_eq!(config.updates.check_interval_hours, 12);
        assert!(config.app_state.has_seen_welcome);
    }

    // Test get_update_settings helper
    #[test]
    fn test_get_update_settings_returns_defaults_when_no_config() {
        // This test doesn't access the filesystem, so it should return defaults
        let settings = UpdatesConfig::default();
        assert_eq!(settings.update_check_mode, UpdateCheckMode::Notify);
        assert_eq!(settings.check_interval_hours, 24);
    }

    // Tests for TmuxConfig
    #[test]
    fn test_tmux_config_default() {
        let tmux = TmuxConfig::default();
        assert_eq!(tmux.status_bar, TmuxStatusBarMode::Auto);
        assert_eq!(tmux.mouse, TmuxMouseMode::Auto);
        assert_eq!(tmux.clipboard, TmuxClipboardMode::Auto);
    }

    #[test]
    fn test_tmux_status_bar_mode_default() {
        let mode = TmuxStatusBarMode::default();
        assert_eq!(mode, TmuxStatusBarMode::Auto);
    }

    #[test]
    fn test_tmux_config_deserialize() {
        let toml = r#"status_bar = "enabled""#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.status_bar, TmuxStatusBarMode::Enabled);
    }

    #[test]
    fn test_tmux_config_deserialize_disabled() {
        let toml = r#"status_bar = "disabled""#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.status_bar, TmuxStatusBarMode::Disabled);
    }

    #[test]
    fn test_tmux_config_deserialize_auto() {
        let toml = r#"status_bar = "auto""#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.status_bar, TmuxStatusBarMode::Auto);
    }

    #[test]
    fn test_tmux_config_in_full_config() {
        let toml = r#"
            [tmux]
            status_bar = "enabled"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.tmux.status_bar, TmuxStatusBarMode::Enabled);
    }

    #[test]
    fn test_tmux_config_serialization_roundtrip() {
        let mut config = Config::default();
        config.tmux.status_bar = TmuxStatusBarMode::Disabled;

        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(config.tmux.status_bar, deserialized.tmux.status_bar);
    }

    #[test]
    fn test_tmux_config_mouse_deserialize() {
        let toml = r#"mouse = "enabled""#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.mouse, TmuxMouseMode::Enabled);
        assert_eq!(tmux.status_bar, TmuxStatusBarMode::Auto);
    }

    #[test]
    fn test_tmux_config_mouse_default_auto() {
        let toml = r#""#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.mouse, TmuxMouseMode::Auto);
    }

    #[test]
    fn test_tmux_config_clipboard_deserialize() {
        let toml = r#"clipboard = "enabled""#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.clipboard, TmuxClipboardMode::Enabled);
        assert_eq!(tmux.mouse, TmuxMouseMode::Auto);
    }

    #[test]
    fn test_tmux_config_clipboard_default_auto() {
        let toml = r#""#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.clipboard, TmuxClipboardMode::Auto);
    }

    #[test]
    fn test_tmux_config_clipboard_disabled() {
        let toml = r#"clipboard = "disabled""#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.clipboard, TmuxClipboardMode::Disabled);
    }

    #[test]
    fn test_tmux_config_mouse_disabled() {
        let toml = r#"mouse = "disabled""#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.mouse, TmuxMouseMode::Disabled);
    }

    #[test]
    fn test_tmux_mouse_mode_default() {
        let mode = TmuxMouseMode::default();
        assert_eq!(mode, TmuxMouseMode::Auto);
    }

    #[test]
    fn test_tmux_config_with_both_settings() {
        let toml = r#"
            status_bar = "enabled"
            mouse = "enabled"
        "#;
        let tmux: TmuxConfig = toml::from_str(toml).unwrap();
        assert_eq!(tmux.status_bar, TmuxStatusBarMode::Enabled);
        assert_eq!(tmux.mouse, TmuxMouseMode::Enabled);
    }

    #[test]
    fn test_tmux_config_in_full_config_with_mouse() {
        let toml = r#"
            [tmux]
            status_bar = "enabled"
            mouse = "enabled"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.tmux.status_bar, TmuxStatusBarMode::Enabled);
        assert_eq!(config.tmux.mouse, TmuxMouseMode::Enabled);
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
    }

    #[test]
    fn test_session_config_confirm_before_quit_defaults_on() {
        // Default-on so existing users get the accidental-exit guard
        // without opting in (#1569).
        assert!(SessionConfig::default().confirm_before_quit);
    }

    #[test]
    fn test_confirm_before_quit_absent_from_toml_defaults_on() {
        // An older config.toml with no `confirm_before_quit` key must
        // deserialize to the enabled default, not false.
        let session: SessionConfig = toml::from_str("").unwrap();
        assert!(session.confirm_before_quit);
    }

    #[test]
    fn test_session_config_default_session_id_poller_max_threads() {
        let cfg = SessionConfig::default();
        assert_eq!(
            cfg.session_id_poller_max_threads,
            crate::session::poller::DEFAULT_SESSION_ID_POLLER_MAX_THREADS
        );
    }

    #[test]
    fn test_session_config_session_id_poller_max_threads_roundtrip() {
        let mut config = Config::default();
        config.session.session_id_poller_max_threads = 137;
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.session.session_id_poller_max_threads, 137);
    }

    #[test]
    fn test_session_config_session_id_poller_max_threads_defaults_when_absent() {
        let toml = r#"
            [session]
            default_tool = "claude"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.session.session_id_poller_max_threads,
            crate::session::poller::DEFAULT_SESSION_ID_POLLER_MAX_THREADS
        );
    }

    #[test]
    fn test_session_config_normalize_clamps_zero_poller_threads() {
        let mut cfg = Config::default();
        cfg.session.session_id_poller_max_threads = 0;
        cfg.normalize();
        assert_eq!(
            cfg.session.session_id_poller_max_threads, 1,
            "normalize() must clamp zero to 1 to keep config, UI, and runtime aligned"
        );
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
    fn test_agent_cockpit_cmd_roundtrip() {
        let mut config = Config::default();
        config
            .session
            .custom_agents
            .insert("oc-sp".to_string(), "ocp run sp".to_string());
        config
            .session
            .agent_cockpit_cmd
            .insert("oc-sp".to_string(), "ocp run sp acp".to_string());

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.session.agent_cockpit_cmd.get("oc-sp"),
            Some(&"ocp run sp acp".to_string()),
        );
    }

    #[test]
    fn test_agent_cockpit_cmd_defaults_empty() {
        // A config with no agent_cockpit_cmd must deserialize to an empty
        // map (serde default), not error, so existing configs keep loading.
        let config: Config = toml::from_str("").unwrap();
        assert!(config.session.agent_cockpit_cmd.is_empty());
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
"cockpit.acp" = "trace"
"#;
        let parsed: LoggingConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.default_level, "debug");
        assert_eq!(
            parsed.targets.get("cockpit.acp"),
            Some(&"trace".to_string())
        );
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
}
