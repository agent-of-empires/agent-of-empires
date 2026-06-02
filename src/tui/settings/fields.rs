//! Setting field definitions and config mapping

use crate::session::{
    validate_check_interval, validate_snooze_duration, Config, ContainerRuntimeName,
    DefaultTerminalMode, ProfileConfig, TmuxClipboardMode, TmuxMouseMode, TmuxStatusBarMode,
    VolumeIgnoresStrategy,
};
use crate::sound::{
    validate_sound_exists, volume_from_option, volume_options, volume_to_index, SoundMode,
};
use crate::tui::styles::available_themes;

use super::SettingsScope;

/// Categories of settings
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCategory {
    Theme,
    Updates,
    Telemetry,
    Worktree,
    Sandbox,
    Tmux,
    Session,
    Agents,
    Interaction,
    Sound,
    StatusHooks,
    Hooks,
    Web,
    Cockpit,
    Diff,
    Logging,
}

impl SettingsCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Theme => "Theme",
            Self::Updates => "Updates",
            Self::Telemetry => "Telemetry",
            Self::Worktree => "Worktree",
            Self::Sandbox => "Sandbox",
            Self::Tmux => "Tmux",
            Self::Session => "Session",
            Self::Agents => "Agents",
            Self::Interaction => "Interaction",
            Self::Sound => "Sound",
            Self::StatusHooks => "Status Hooks",
            Self::Hooks => "Lifecycle Hooks",
            Self::Web => "Web",
            Self::Cockpit => "Cockpit",
            Self::Diff => "Diff",
            Self::Logging => "Logging",
        }
    }
}

/// Type-safe field identifiers (prevents typos in string matching)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKey {
    // Profile (only relevant when the Profile scope is active)
    ProfileDescription,
    // Theme
    ThemeName,
    ThemeColorMode,
    IdleDecayMinutes,
    // Updates
    UpdateCheckMode,
    CheckIntervalHours,
    NotifyInCli,
    WebPollIntervalMinutes,
    // Telemetry
    TelemetryEnabled,
    // Worktree
    WorktreeEnabled,
    PathTemplate,
    BareRepoPathTemplate,
    WorktreeAutoCleanup,
    DeleteBranchOnCleanup,
    WorkspacePathTemplate,
    InitSubmodules,
    // Sandbox
    SandboxEnabledByDefault,
    YoloModeDefault,
    DefaultImage,
    Environment,
    SandboxAutoCleanup,
    CpuLimit,
    MemoryLimit,
    DefaultTerminalMode,
    ExtraVolumes,
    PortMappings,
    VolumeIgnores,
    VolumeIgnoresStrategy,
    MountSsh,
    SelinuxRelabel,
    CustomInstruction,
    ContainerRuntime,
    // Tmux
    StatusBar,
    Mouse,
    Clipboard,
    // Session
    DefaultTool,
    StrictHotkeys,
    ConfirmBeforeQuit,
    SnoozeDurationMinutes,
    SessionAutoStopIdleSecs,
    RestartWakeMessage,
    RowTag,
    AgentExtraArgs,
    AgentCommandOverride,
    AgentStatusHooks,
    CustomAgents,
    AgentDetectAs,
    AgentCockpitCmd,
    HostEnvironment,
    SessionIdPollerMaxThreads,
    LiveSendExitChord,
    LiveSendLeader,
    NewSessionAttachMode,
    DefaultAttachMode,
    ClickAction,
    MouseCapture,
    // Sound
    SoundEnabled,
    SoundMode,
    SoundVolume,
    SoundOnStart,
    SoundOnRunning,
    SoundOnWaiting,
    SoundOnIdle,
    SoundOnError,
    SoundOnApproval,
    // Status hooks
    StatusHooksEnabled,
    StatusHookDebounceMs,
    StatusHookOnStarting,
    StatusHookOnRunning,
    StatusHookOnWaiting,
    StatusHookOnIdle,
    StatusHookOnError,
    StatusHookOnChange,
    // Hooks
    HookOnCreate,
    HookOnLaunch,
    HookOnDestroy,
    // Web
    WebNotificationsEnabled,
    WebNotifyOnWaiting,
    WebNotifyOnIdle,
    WebNotifyOnError,
    WebNotifyOnWakeFire,
    // Cockpit (gated on the `serve` feature; the variants are always
    // present in the enum so external callers don't have to cfg-gate
    // their match arms)
    CockpitEnabled,
    CockpitDefaultForClaude,
    CockpitDefaultAgent,
    CockpitMaxConcurrentWorkers,
    CockpitReplayEvents,
    CockpitReplayBytes,
    CockpitNodePath,
    CockpitShowToolDurations,
    CockpitQueueDrainMode,
    CockpitMaxConcurrentResumes,
    CockpitForceEndTurnThresholdSecs,
    CockpitSilentOrphanGraceSecs,
    CockpitSilentOrphanFastGraceSecs,
    CockpitAutoStopIdleSecs,
    // Diff
    DiffSplitView,
    // Logging
    LoggingDefaultLevel,
    /// Per-target override; carries an index into `crate::logging::KNOWN_SUB_TARGETS`
    /// so the FieldKey enum stays `Copy` without carrying owned strings.
    LoggingTarget(u8),
    LoggingOutput,
    LoggingFilePath,
    LoggingRotation,
    LoggingMaxSizeMib,
    LoggingKeepCount,
    LoggingShowSpans,
    /// Pseudo-key for `FieldValue::SectionHeader` rows so apply/clear
    /// match arms have a defined-but-no-op variant rather than falling
    /// through to a panic-on-missing-arm wildcard. Carries no payload;
    /// multiple section markers in one category are disambiguated by
    /// the label string on the parent `SettingField`.
    SectionMarker,
}

/// Map `UpdateCheckMode` to the Select index used by the settings TUI.
/// Order matches the labels in `build_update_fields()`.
fn update_check_mode_to_index(mode: crate::session::config::UpdateCheckMode) -> usize {
    match mode {
        crate::session::config::UpdateCheckMode::Auto => 0,
        crate::session::config::UpdateCheckMode::Notify => 1,
        crate::session::config::UpdateCheckMode::Off => 2,
    }
}

fn update_check_mode_from_index(idx: usize) -> crate::session::config::UpdateCheckMode {
    match idx {
        0 => crate::session::config::UpdateCheckMode::Auto,
        2 => crate::session::config::UpdateCheckMode::Off,
        _ => crate::session::config::UpdateCheckMode::Notify,
    }
}

/// Resolve a field value from global config and optional profile override.
/// Returns (value, has_override).
fn resolve_value<T: Clone>(scope: SettingsScope, global: T, profile: Option<T>) -> (T, bool) {
    match scope {
        SettingsScope::Global | SettingsScope::Repo => (global, false),
        SettingsScope::Profile => {
            let has_override = profile.is_some();
            let value = profile.unwrap_or(global);
            (value, has_override)
        }
    }
}

/// Resolve an optional field (Option<T>) where both global and profile values are Option<T>.
/// The `has_explicit_override` flag indicates if the profile explicitly set this field.
fn resolve_optional<T: Clone>(
    scope: SettingsScope,
    global: Option<T>,
    profile: Option<T>,
    has_explicit_override: bool,
) -> (Option<T>, bool) {
    match scope {
        SettingsScope::Global | SettingsScope::Repo => (global, false),
        SettingsScope::Profile => {
            let value = profile.or(global);
            (value, has_explicit_override)
        }
    }
}

/// Convert a FieldValue to a human-readable display string.
fn value_display_string(value: &FieldValue) -> String {
    match value {
        FieldValue::Bool(v) => if *v { "on" } else { "off" }.to_string(),
        FieldValue::Text(v) => {
            if v.is_empty() {
                "(empty)".to_string()
            } else {
                v.clone()
            }
        }
        FieldValue::Number(v) => v.to_string(),
        FieldValue::Select { selected, options } => {
            options.get(*selected).cloned().unwrap_or_default()
        }
        FieldValue::List(items) => format!("[{} items]", items.len()),
        FieldValue::OptionalText(v) => v.clone().unwrap_or_else(|| "(empty)".to_string()),
        FieldValue::SectionHeader => String::new(),
    }
}

/// Build the inherited display string when a field has an override.
fn inherited_if(has_override: bool, global_value: FieldValue) -> Option<String> {
    if has_override {
        Some(value_display_string(&global_value))
    } else {
        None
    }
}

/// Helper to set a profile override. Always stores the value; use 'r' key to clear overrides.
fn set_profile_override<T, S, F>(new_value: T, section: &mut Option<S>, set_field: F)
where
    T: Clone,
    S: Default,
    F: FnOnce(&mut S, Option<T>),
{
    let s = section.get_or_insert_with(S::default);
    set_field(s, Some(new_value));
}

/// Parse a list of "key=value" strings into a HashMap.
fn parse_key_value_list(items: &[String]) -> std::collections::HashMap<String, String> {
    items
        .iter()
        .filter_map(|item| {
            let (k, v) = item.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Value types for settings fields
#[derive(Debug, Clone)]
pub enum FieldValue {
    Bool(bool),
    Text(String),
    Number(u64),
    Select {
        selected: usize,
        options: Vec<String>,
    },
    List(Vec<String>),
    OptionalText(Option<String>),
    /// Non-interactive section divider rendered as a styled heading.
    /// The `SettingField::label` carries the heading text and
    /// `description` carries the (optional) subtitle below it. Input
    /// handlers skip cursor navigation past entries carrying this
    /// variant; apply / clear pathways no-op for them.
    SectionHeader,
}

/// A setting field with metadata
#[derive(Debug, Clone)]
pub struct SettingField {
    pub key: FieldKey,
    pub label: &'static str,
    pub description: &'static str,
    pub value: FieldValue,
    pub category: SettingsCategory,
    /// Whether this field has a profile/repo override
    pub has_override: bool,
    /// Human-readable display of the inherited (global/base) value, set when has_override is true
    pub inherited_display: Option<String>,
}

impl SettingField {
    /// True when this entry is a non-interactive section divider
    /// (`FieldValue::SectionHeader`). The renderer paints it as a
    /// styled heading and the input handler skips navigation past it.
    pub fn is_section_header(&self) -> bool {
        matches!(self.value, FieldValue::SectionHeader)
    }
}

impl SettingField {
    pub fn validate(&self) -> Result<(), String> {
        match (&self.key, &self.value) {
            (FieldKey::CheckIntervalHours, FieldValue::Number(n)) => {
                validate_check_interval(*n)?;
                Ok(())
            }
            (FieldKey::SnoozeDurationMinutes, FieldValue::Number(n)) => {
                validate_snooze_duration(*n)?;
                Ok(())
            }
            (FieldKey::SessionAutoStopIdleSecs, FieldValue::Number(n)) => {
                crate::session::config::validate_auto_stop_idle_secs(*n)?;
                Ok(())
            }
            (FieldKey::MemoryLimit, FieldValue::OptionalText(Some(v))) => {
                crate::session::validate_memory_limit(v)?;
                Ok(())
            }
            // Sound field validation - check if sound file exists
            (
                FieldKey::SoundOnStart
                | FieldKey::SoundOnRunning
                | FieldKey::SoundOnWaiting
                | FieldKey::SoundOnIdle
                | FieldKey::SoundOnError
                | FieldKey::SoundOnApproval,
                FieldValue::OptionalText(Some(name)),
            ) => {
                if !name.is_empty() {
                    validate_sound_exists(name)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Build fields for a category based on scope and current config values.
///
/// For Repo scope, `global` should be the resolved (global+profile merged) config,
/// and `profile` should be the repo config converted to ProfileConfig via `repo_config_to_profile`.
pub fn build_fields_for_category(
    category: SettingsCategory,
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    match category {
        SettingsCategory::Theme => build_theme_fields(scope, global, profile),
        SettingsCategory::Updates => build_updates_fields(scope, global, profile),
        SettingsCategory::Telemetry => build_telemetry_fields(global),
        SettingsCategory::Worktree => build_worktree_fields(scope, global, profile),
        SettingsCategory::Sandbox => build_sandbox_fields(scope, global, profile),
        SettingsCategory::Tmux => build_tmux_fields(scope, global, profile),
        SettingsCategory::Session => build_session_fields(scope, global, profile),
        SettingsCategory::Agents => build_agents_fields(scope, global, profile),
        SettingsCategory::Interaction => build_interaction_fields(scope, global, profile),
        SettingsCategory::Sound => build_sound_fields(scope, global, profile),
        SettingsCategory::StatusHooks => build_status_hook_fields(scope, global, profile),
        SettingsCategory::Hooks => build_hooks_fields(scope, global, profile),
        SettingsCategory::Web => build_web_fields(scope, global, profile),
        SettingsCategory::Cockpit => build_cockpit_fields(scope, global, profile),
        SettingsCategory::Diff => build_diff_fields(scope, global, profile),
        SettingsCategory::Logging => build_logging_fields(global),
    }
}

const LOG_LEVEL_OPTIONS: &[&str] = &["trace", "debug", "info", "warn", "error"];
const LOG_LEVEL_OVERRIDE_OPTIONS: &[&str] =
    &["(default)", "trace", "debug", "info", "warn", "error"];
const SINK_OPTIONS: &[&str] = &["file", "stdout"];
const ROTATION_OPTIONS: &[&str] = &["size", "never"];

/// Display labels for `RowTagMode` in the Settings picker. Order must match
/// `row_tag_to_index` / `index_to_row_tag` so the index round-trips. `None`
/// is first because it is the default; existing users see no tag.
const ROW_TAG_OPTIONS: &[&str] = &["None", "Auto", "Profile", "Sandbox", "Branch"];

fn row_tag_to_index(mode: crate::session::config::RowTagMode) -> usize {
    use crate::session::config::RowTagMode::*;
    match mode {
        None => 0,
        Auto => 1,
        Profile => 2,
        Sandbox => 3,
        Branch => 4,
    }
}

fn index_to_row_tag(idx: usize) -> crate::session::config::RowTagMode {
    use crate::session::config::RowTagMode::*;
    match idx {
        1 => Auto,
        2 => Profile,
        3 => Sandbox,
        4 => Branch,
        _ => None,
    }
}

/// Display labels for `NewSessionAttachMode`. Order must match
/// `new_session_attach_mode_to_index` / `index_to_new_session_attach_mode`.
/// `Tmux` is first so it is the default selection.
const NEW_SESSION_ATTACH_MODE_OPTIONS: &[&str] = &["Tmux", "Live mode"];

fn new_session_attach_mode_to_index(mode: crate::session::NewSessionAttachMode) -> usize {
    use crate::session::NewSessionAttachMode::*;
    match mode {
        Tmux => 0,
        LiveSend => 1,
    }
}

fn index_to_new_session_attach_mode(idx: usize) -> crate::session::NewSessionAttachMode {
    use crate::session::NewSessionAttachMode::*;
    match idx {
        1 => LiveSend,
        _ => Tmux,
    }
}

/// Display labels for `ClickAction`. Order must match
/// `click_action_to_index` / `index_to_click_action`. `LiveSend` is
/// first so it is the default selection (matches the historical
/// single-click-enters-live behavior).
const CLICK_ACTION_OPTIONS: &[&str] = &["Live mode", "Select only"];

fn click_action_to_index(mode: crate::session::ClickAction) -> usize {
    use crate::session::ClickAction::*;
    match mode {
        LiveSend => 0,
        SelectOnly => 1,
    }
}

fn index_to_click_action(idx: usize) -> crate::session::ClickAction {
    use crate::session::ClickAction::*;
    match idx {
        1 => SelectOnly,
        _ => LiveSend,
    }
}

fn level_index(level: &str, opts: &[&str]) -> usize {
    opts.iter().position(|&o| o == level).unwrap_or(0)
}

fn build_logging_fields(global: &Config) -> Vec<SettingField> {
    let mut fields = Vec::with_capacity(1 + crate::logging::KNOWN_SUB_TARGETS.len());

    let default_idx = level_index(&global.logging.default_level, LOG_LEVEL_OPTIONS);
    fields.push(SettingField {
        key: FieldKey::LoggingDefaultLevel,
        label: "Default level",
        description: "Baseline applied to every known target root. Per-target overrides win.",
        value: FieldValue::Select {
            selected: default_idx,
            options: LOG_LEVEL_OPTIONS.iter().map(|s| s.to_string()).collect(),
        },
        category: SettingsCategory::Logging,
        has_override: false,
        inherited_display: None,
    });

    for (i, target) in crate::logging::KNOWN_SUB_TARGETS.iter().enumerate() {
        let current = global
            .logging
            .targets
            .get(*target)
            .map(|s| s.as_str())
            .unwrap_or("(default)");
        let idx = level_index(current, LOG_LEVEL_OVERRIDE_OPTIONS);
        fields.push(SettingField {
            key: FieldKey::LoggingTarget(i as u8),
            label: target,
            description: "Per-target override; (default) inherits the baseline.",
            value: FieldValue::Select {
                selected: idx,
                options: LOG_LEVEL_OVERRIDE_OPTIONS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            },
            category: SettingsCategory::Logging,
            has_override: false,
            inherited_display: None,
        });
    }

    // Sink-shape knobs. Changing any of these requires a process restart
    // (the tracing subscriber is a global singleton; the rotating writer
    // holds its policy + file handle for the life of the process).
    let output_idx = level_index(
        match global.logging.output {
            crate::session::config::SinkKind::File => "file",
            crate::session::config::SinkKind::Stdout => "stdout",
        },
        SINK_OPTIONS,
    );
    fields.push(SettingField {
        key: FieldKey::LoggingOutput,
        label: "Output (restart req.)",
        description:
            "Where tracing lands: file (default) or stdout. TUI / daemon child / runner coerce \
             to file regardless. Restart aoe for changes to take effect.",
        value: FieldValue::Select {
            selected: output_idx,
            options: SINK_OPTIONS.iter().map(|s| s.to_string()).collect(),
        },
        category: SettingsCategory::Logging,
        has_override: false,
        inherited_display: None,
    });
    fields.push(SettingField {
        key: FieldKey::LoggingFilePath,
        label: "File path (restart req.)",
        description: "Log file location. Relative paths resolve under the app data dir; absolute \
                      paths are used verbatim. Restart aoe for changes.",
        value: FieldValue::Text(global.logging.file_path.clone()),
        category: SettingsCategory::Logging,
        has_override: false,
        inherited_display: None,
    });

    let rotation_idx = level_index(
        match global.logging.rotation {
            crate::session::config::RotationKind::Size => "size",
            crate::session::config::RotationKind::Never => "never",
        },
        ROTATION_OPTIONS,
    );
    fields.push(SettingField {
        key: FieldKey::LoggingRotation,
        label: "Rotation (restart req.)",
        description: "size rotates when the live file crosses the threshold; never disables \
                      rotation. Restart aoe for changes.",
        value: FieldValue::Select {
            selected: rotation_idx,
            options: ROTATION_OPTIONS.iter().map(|s| s.to_string()).collect(),
        },
        category: SettingsCategory::Logging,
        has_override: false,
        inherited_display: None,
    });
    fields.push(SettingField {
        key: FieldKey::LoggingMaxSizeMib,
        label: "Max size MiB (restart req.)",
        description: "Rotation threshold in MiB. Ignored when rotation = never.",
        value: FieldValue::Number(global.logging.max_size_mib),
        category: SettingsCategory::Logging,
        has_override: false,
        inherited_display: None,
    });
    fields.push(SettingField {
        key: FieldKey::LoggingKeepCount,
        label: "Keep count (restart req.)",
        description: "How many rotated files to retain (.1 through .keep_count).",
        value: FieldValue::Number(global.logging.keep_count as u64),
        category: SettingsCategory::Logging,
        has_override: false,
        inherited_display: None,
    });
    fields.push(SettingField {
        key: FieldKey::LoggingShowSpans,
        label: "Show span context (restart req.)",
        description:
            "When on, every log line is prefixed with the names + fields of the spans wrapping it \
             (e.g. `http_request{request_id=... method=GET path=...}` from the per-request middleware). \
             Useful for grep-correlation across async boundaries when triaging; noisy on idle polling endpoints. \
             Off by default keeps the log readable.",
        value: FieldValue::Bool(global.logging.show_spans),
        category: SettingsCategory::Logging,
        has_override: false,
        inherited_display: None,
    });

    fields
}

fn build_cockpit_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let p = profile.cockpit.as_ref();

    let (enabled, enabled_override) =
        resolve_value(scope, global.cockpit.enabled, p.and_then(|c| c.enabled));
    let (default_for_claude, dfc_override) = resolve_value(
        scope,
        global.cockpit.default_for_claude,
        p.and_then(|c| c.default_for_claude),
    );
    let (default_agent, da_override) = resolve_value(
        scope,
        global.cockpit.default_agent.clone(),
        p.and_then(|c| c.default_agent.clone()),
    );
    let (max_workers, mw_override) = resolve_value(
        scope,
        global.cockpit.max_concurrent_workers,
        p.and_then(|c| c.max_concurrent_workers),
    );
    let (replay_events, re_override) = resolve_value(
        scope,
        global.cockpit.replay_events,
        p.and_then(|c| c.replay_events),
    );
    let (replay_bytes, rb_override) = resolve_value(
        scope,
        global.cockpit.replay_bytes,
        p.and_then(|c| c.replay_bytes),
    );
    let (node_path, np_override) = resolve_value(
        scope,
        global.cockpit.node_path.clone(),
        p.and_then(|c| c.node_path.clone()),
    );
    let (show_tool_durations, std_override) = resolve_value(
        scope,
        global.cockpit.show_tool_durations,
        p.and_then(|c| c.show_tool_durations),
    );
    let (queue_drain_mode, qdm_override) = resolve_value(
        scope,
        global.cockpit.queue_drain_mode,
        p.and_then(|c| c.queue_drain_mode),
    );
    let (max_concurrent_resumes, mcr_override) = resolve_value(
        scope,
        global.cockpit.max_concurrent_resumes,
        p.and_then(|c| c.max_concurrent_resumes),
    );
    let (force_end_turn_threshold_secs, fet_override) = resolve_value(
        scope,
        global.cockpit.force_end_turn_threshold_secs,
        p.and_then(|c| c.force_end_turn_threshold_secs),
    );
    let (silent_orphan_grace_secs, sog_override) = resolve_value(
        scope,
        global.cockpit.silent_orphan_grace_secs,
        p.and_then(|c| c.silent_orphan_grace_secs),
    );
    let (silent_orphan_fast_grace_secs, sofg_override) = resolve_value(
        scope,
        global.cockpit.silent_orphan_fast_grace_secs,
        p.and_then(|c| c.silent_orphan_fast_grace_secs),
    );
    let (auto_stop_idle_secs, asis_override) = resolve_value(
        scope,
        global.cockpit.auto_stop_idle_secs,
        p.and_then(|c| c.auto_stop_idle_secs),
    );

    vec![
        SettingField {
            key: FieldKey::CockpitEnabled,
            label: "Cockpit enabled",
            description: "Master switch for cockpit (ACP-based native agent rendering). When off, sessions use the terminal/PTY view even with --cockpit.",
            value: FieldValue::Bool(enabled),
            category: SettingsCategory::Cockpit,
            has_override: enabled_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitDefaultForClaude,
            label: "Default for Claude (mobile)",
            description: "On mobile clients, default new Claude sessions to cockpit mode.",
            value: FieldValue::Bool(default_for_claude),
            category: SettingsCategory::Cockpit,
            has_override: dfc_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitDefaultAgent,
            label: "Default agent",
            description: "Cockpit agent to use when --agent is not specified (e.g., aoe-agent, claude-code, gemini).",
            value: FieldValue::Text(default_agent),
            category: SettingsCategory::Cockpit,
            has_override: da_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitReplayEvents,
            label: "History cap (events)",
            description: "Per-session retention cap on cockpit events. 0 = unlimited (default); set a non-zero value to bound disk usage on long-running sessions.",
            value: FieldValue::Number(u64::from(replay_events)),
            category: SettingsCategory::Cockpit,
            has_override: re_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitNodePath,
            label: "Node path",
            description: "Override Node.js binary location. Empty -> auto-resolve via AOE_COCKPIT_NODE / PATH / bundled.",
            value: FieldValue::Text(node_path),
            category: SettingsCategory::Cockpit,
            has_override: np_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitShowToolDurations,
            label: "Show tool-call durations",
            description: "Render an elapsed-time label on every cockpit tool card. Cross-device via config.toml. Note: the underlying measurement is currently imprecise on claude-agent-acp (no `status: in_progress` signal); durations include stream-arrival skew. Defaults to on; turn off if the inflated numbers are more confusing than useful.",
            value: FieldValue::Bool(show_tool_durations),
            category: SettingsCategory::Cockpit,
            has_override: std_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::SectionMarker,
            label: "Advanced",
            description: "Operational tuning, rarely needed after first setup. Adjust only if you've read the description and know what you're changing.",
            value: FieldValue::SectionHeader,
            category: SettingsCategory::Cockpit,
            has_override: false,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitMaxConcurrentWorkers,
            label: "Max concurrent workers",
            description: "Hard cap on simultaneously running cockpit agent subprocesses; additional sessions queue.",
            value: FieldValue::Number(u64::from(max_workers)),
            category: SettingsCategory::Cockpit,
            has_override: mw_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitMaxConcurrentResumes,
            label: "Max concurrent resumes",
            description: "Upper bound on parallel cockpit worker spawns/attaches the reconciler runs on `aoe serve` cold start. Default 4 keeps Node.js bootup memory within bounds for laptops/Pis (each claude-agent-acp is ~50-80 MB transient). Bounded at runtime by `min(this, max_concurrent_workers).max(1)`. See #1088.",
            value: FieldValue::Number(u64::from(max_concurrent_resumes)),
            category: SettingsCategory::Cockpit,
            has_override: mcr_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitQueueDrainMode,
            label: "Queue drain mode",
            description: "How the web composer dispatches follow-up prompts queued while the agent was busy. Combined (default) joins every queued entry with a blank line and sends them as a single prompt on the next Stopped; one response covers the whole batch. Serial fires one entry at a time; each gets its own response. See #1031.",
            value: FieldValue::Select {
                selected: match queue_drain_mode {
                    crate::session::config::QueueDrainMode::Combined => 0,
                    crate::session::config::QueueDrainMode::Serial => 1,
                },
                options: vec!["combined".to_string(), "serial".to_string()],
            },
            category: SettingsCategory::Cockpit,
            has_override: qdm_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitReplayBytes,
            label: "Replay buffer bytes",
            description: "Maximum bytes of cockpit events kept in the per-session replay buffer.",
            value: FieldValue::Number(replay_bytes),
            category: SettingsCategory::Cockpit,
            has_override: rb_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitForceEndTurnThresholdSecs,
            label: "Force end turn threshold (s)",
            description: "Seconds of streaming inactivity after which the cockpit web UI offers a \"Force end turn\" button. When the spinner is stuck (a missed Stopped event), clicking the button clears the local spinner and asks the daemon to publish a synthetic Stopped + best-effort session/cancel. Default 30s. See #1100.",
            value: FieldValue::Number(u64::from(force_end_turn_threshold_secs)),
            category: SettingsCategory::Cockpit,
            has_override: fet_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitSilentOrphanGraceSecs,
            label: "Silent-orphan grace (s)",
            description: "Daemon-side watchdog that detects when the agent finishes streaming but the adapter never resolves the session/prompt request. Fires after this many seconds of no progress notifications, when no in-flight tool call is open and the prompt has produced at least one progress event. On fire, sends best-effort session/cancel and reuses the existing cancel-escalation path to SIGTERM + session/load respawn. Default 120s; raised from 60s in #1360 so async-agent flows (Claude SDK Agent tool with isAsync) survive normal sub-agent waits. Nonzero values below 120 clamp up to 120 at runtime. Set 0 to disable. Long-running tools are not affected (watchdog suppresses while any tool call is active). When the daemon detects an async-agent launch in the current prompt, the effective grace lifts to at least 30 minutes. See #1240, #1360.",
            value: FieldValue::Number(u64::from(silent_orphan_grace_secs)),
            category: SettingsCategory::Cockpit,
            has_override: sog_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitSilentOrphanFastGraceSecs,
            label: "Silent-orphan fast grace (s)",
            description: "Accelerated silent-orphan grace, used in place of the default once a cost-populated UsageUpdate has arrived for the current prompt (the claude-agent-acp wrap-up accounting marker emitted just before PromptResponse). Lowers MTTR on the known adapter wedge without weakening the vendor-agnostic baseline. Default 20s. Set 0 to disable the accelerator (cost UsageUpdate no longer reduces the effective grace). Only consulted when silent-orphan grace is enabled (non-zero). See #1240.",
            value: FieldValue::Number(u64::from(silent_orphan_fast_grace_secs)),
            category: SettingsCategory::Cockpit,
            has_override: sofg_override,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::CockpitAutoStopIdleSecs,
            label: "Auto-stop idle workers (s)",
            description: "Seconds of inactivity (no cockpit events, no in-flight turn) after which the daemon stops an idle cockpit worker and frees its resources. The session stays stopped until the next prompt, which respawns the worker seamlessly. Default 0 disables the feature; no worker is ever auto-stopped for inactivity. Evaluated roughly once a minute, so the effective stop time can lag the threshold by up to a minute. See #1689.",
            value: FieldValue::Number(u64::from(auto_stop_idle_secs)),
            category: SettingsCategory::Cockpit,
            has_override: asis_override,
            inherited_display: None,
        },
    ]
}

fn build_web_fields(
    scope: SettingsScope,
    global: &Config,
    _profile: &ProfileConfig,
) -> Vec<SettingField> {
    // Web settings are server-global, not profile-scoped. In Profile mode
    // we still surface the field (read-only) so users discover it; writes
    // always apply to the global config.
    let _ = scope;

    vec![
        SettingField {
            key: FieldKey::WebNotificationsEnabled,
            label: "Push notifications",
            description: "Allow the web dashboard to deliver browser push notifications (server-wide kill switch).",
            value: FieldValue::Bool(global.web.notifications_enabled),
            category: SettingsCategory::Web,
            has_override: false,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::WebNotifyOnWaiting,
            label: "Notify on waiting",
            description: "Default: send a push when a session transitions Running to Waiting (agent is asking for input). Sessions can override individually.",
            value: FieldValue::Bool(global.web.notify_on_waiting),
            category: SettingsCategory::Web,
            has_override: false,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::WebNotifyOnIdle,
            label: "Notify on idle",
            description: "Default: send a push when a session finishes (Running to Idle). Off by default because short sessions make this noisy; sessions can opt in individually.",
            value: FieldValue::Bool(global.web.notify_on_idle),
            category: SettingsCategory::Web,
            has_override: false,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::WebNotifyOnError,
            label: "Notify on error",
            description: "Default: send a push when a session errors (Running to Error).",
            value: FieldValue::Bool(global.web.notify_on_error),
            category: SettingsCategory::Web,
            has_override: false,
            inherited_display: None,
        },
        SettingField {
            key: FieldKey::WebNotifyOnWakeFire,
            label: "Notify on scheduled wake",
            description: "Default: send a push when a cockpit session's ScheduleWakeup timer fires (the next /loop turn starts). Suppressed if the TUI or web dashboard has been active in the last 30s.",
            value: FieldValue::Bool(global.web.notify_on_wake_fire),
            category: SettingsCategory::Web,
            has_override: false,
            inherited_display: None,
        },
    ]
}

fn build_diff_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let diff = profile.diff.as_ref();

    let (split_view, has_override) = resolve_value(
        scope,
        global.diff.split_view,
        diff.and_then(|d| d.split_view),
    );

    vec![SettingField {
        key: FieldKey::DiffSplitView,
        label: "Side-by-side diff",
        description: "Show diffs in a split (side-by-side) layout instead of unified",
        value: FieldValue::Bool(split_view),
        category: SettingsCategory::Diff,
        has_override,
        inherited_display: inherited_if(has_override, FieldValue::Bool(global.diff.split_view)),
    }]
}

fn build_theme_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let theme = profile.theme.as_ref();

    let (name, has_override) = resolve_value(
        scope,
        global.theme.name.clone(),
        theme.and_then(|t| t.name.clone()),
    );

    let options: Vec<String> = available_themes();
    let selected = options.iter().position(|s| s == &name).unwrap_or(0);

    let global_selected = options
        .iter()
        .position(|s| s == &global.theme.name)
        .unwrap_or(0);
    let inherited = inherited_if(
        has_override,
        FieldValue::Select {
            selected: global_selected,
            options: options.clone(),
        },
    );

    let color_mode_options: Vec<String> = vec!["truecolor".to_string(), "palette".to_string()];
    let (color_mode_val, cm_has_override) = resolve_value(
        scope,
        global.theme.color_mode.clone(),
        theme.and_then(|t| t.color_mode.clone()),
    );
    let cm_selected = match color_mode_val {
        crate::session::config::ColorMode::Truecolor => 0,
        crate::session::config::ColorMode::Palette => 1,
    };
    let global_cm_selected = match global.theme.color_mode {
        crate::session::config::ColorMode::Truecolor => 0,
        crate::session::config::ColorMode::Palette => 1,
    };
    let cm_inherited = inherited_if(
        cm_has_override,
        FieldValue::Select {
            selected: global_cm_selected,
            options: color_mode_options.clone(),
        },
    );

    let (idle_decay_minutes, idle_decay_override) = resolve_value(
        scope,
        global.theme.idle_decay_minutes,
        theme.and_then(|t| t.idle_decay_minutes),
    );

    let mut fields = Vec::with_capacity(4);
    // The profile description is a profile-only field (no global counterpart),
    // so we surface it only when the user is editing the Profile scope. It
    // sits at the top of the Theme category so it is the first thing they
    // see when looking at a profile's settings.
    if scope == SettingsScope::Profile {
        fields.push(SettingField {
            key: FieldKey::ProfileDescription,
            label: "Description",
            description:
                "Short, human-readable description of what this profile does. Shown as helper \
                 text under the profile name in the new-session picker (TUI + web).",
            value: FieldValue::OptionalText(profile.description.clone()),
            category: SettingsCategory::Theme,
            has_override: profile.description.is_some(),
            inherited_display: None,
        });
    }
    fields.extend([
        SettingField {
            key: FieldKey::ThemeName,
            label: "Theme",
            description: "Color theme for the TUI",
            value: FieldValue::Select { selected, options },
            category: SettingsCategory::Theme,
            has_override,
            inherited_display: inherited,
        },
        SettingField {
            key: FieldKey::ThemeColorMode,
            label: "Color Mode",
            description: "Truecolor (24-bit RGB) or palette (xterm-256). Use palette if your terminal mangles RGB escapes.",
            value: FieldValue::Select {
                selected: cm_selected,
                options: color_mode_options,
            },
            category: SettingsCategory::Theme,
            has_override: cm_has_override,
            inherited_display: cm_inherited,
        },
        SettingField {
            key: FieldKey::IdleDecayMinutes,
            label: "Idle Decay (minutes)",
            description: "Off by default (0). Set a positive value to opt in: a freshly-stopped Idle session keeps a fresh-idle tint and an animated breathe icon for this many minutes before snapping back to the static look, and is treated as actionable by the `w` keybind. The time-since-stop column on Idle rows shows regardless of this setting.",
            value: FieldValue::Number(idle_decay_minutes),
            category: SettingsCategory::Theme,
            has_override: idle_decay_override,
            inherited_display: inherited_if(
                idle_decay_override,
                FieldValue::Number(global.theme.idle_decay_minutes),
            ),
        },
    ]);
    fields
}

fn build_updates_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let updates = profile.updates.as_ref();

    let (mode_val, o1) = resolve_value(
        scope,
        global.updates.update_check_mode,
        updates.and_then(|u| u.update_check_mode),
    );
    let (check_interval, o2) = resolve_value(
        scope,
        global.updates.check_interval_hours,
        updates.and_then(|u| u.check_interval_hours),
    );
    let (notify_in_cli, o3) = resolve_value(
        scope,
        global.updates.notify_in_cli,
        updates.and_then(|u| u.notify_in_cli),
    );
    let (web_poll, o4) = resolve_value(
        scope,
        global.updates.web_poll_interval_minutes,
        updates.and_then(|u| u.web_poll_interval_minutes),
    );

    let mode_options: Vec<String> =
        vec!["auto".to_string(), "notify".to_string(), "off".to_string()];
    let mode_index = update_check_mode_to_index(mode_val);
    let global_mode_index = update_check_mode_to_index(global.updates.update_check_mode);

    vec![
        SettingField {
            key: FieldKey::UpdateCheckMode,
            label: "Update Check Mode",
            description: "auto = install in background on detection (picked up next launch). \
                          notify = show banner / CLI notice (default). off = skip every check.",
            value: FieldValue::Select {
                selected: mode_index,
                options: mode_options.clone(),
            },
            category: SettingsCategory::Updates,
            has_override: o1,
            inherited_display: inherited_if(
                o1,
                FieldValue::Select {
                    selected: global_mode_index,
                    options: mode_options,
                },
            ),
        },
        SettingField {
            key: FieldKey::CheckIntervalHours,
            label: "Check Interval (hours)",
            description: "How often to check for updates",
            value: FieldValue::Number(check_interval),
            category: SettingsCategory::Updates,
            has_override: o2,
            inherited_display: inherited_if(
                o2,
                FieldValue::Number(global.updates.check_interval_hours),
            ),
        },
        SettingField {
            key: FieldKey::NotifyInCli,
            label: "Notify in CLI",
            description: "Show update notifications in CLI output",
            value: FieldValue::Bool(notify_in_cli),
            category: SettingsCategory::Updates,
            has_override: o3,
            inherited_display: inherited_if(o3, FieldValue::Bool(global.updates.notify_in_cli)),
        },
        SettingField {
            key: FieldKey::WebPollIntervalMinutes,
            label: "Web Poll Interval (minutes)",
            description: "How often the web dashboard re-polls for new releases",
            value: FieldValue::Number(web_poll),
            category: SettingsCategory::Updates,
            has_override: o4,
            inherited_display: inherited_if(
                o4,
                FieldValue::Number(global.updates.web_poll_interval_minutes),
            ),
        },
    ]
}

/// Telemetry is a single install-level consent toggle, so it is global-only
/// (no profile override) and mirrors the `build_logging_fields` shape. When
/// `DO_NOT_TRACK` is set the description calls out that nothing is sent
/// regardless of the toggle, so the suppressed state is never silent.
fn build_telemetry_fields(global: &Config) -> Vec<SettingField> {
    let description = if crate::telemetry::do_not_track() {
        "Anonymous, opt-in usage telemetry. DO_NOT_TRACK is set, so nothing is sent and no \
         install id is generated regardless of this toggle. Off by default; never sends content."
    } else {
        "Anonymous, opt-in usage telemetry (counts of sessions, agents, version, OS). Off by \
         default. Never sends prompts, paths, names, or commands. Honors DO_NOT_TRACK."
    };
    vec![SettingField {
        key: FieldKey::TelemetryEnabled,
        label: "Enable usage telemetry",
        description,
        value: FieldValue::Bool(global.telemetry.enabled),
        category: SettingsCategory::Telemetry,
        has_override: false,
        inherited_display: None,
    }]
}

fn build_worktree_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let wt = profile.worktree.as_ref();

    let (enabled, o0) = resolve_value(scope, global.worktree.enabled, wt.and_then(|w| w.enabled));
    let (path_template, o1) = resolve_value(
        scope,
        global.worktree.path_template.clone(),
        wt.and_then(|w| w.path_template.clone()),
    );
    let (bare_repo_template, o2) = resolve_value(
        scope,
        global.worktree.bare_repo_path_template.clone(),
        wt.and_then(|w| w.bare_repo_path_template.clone()),
    );
    let (auto_cleanup, o3) = resolve_value(
        scope,
        global.worktree.auto_cleanup,
        wt.and_then(|w| w.auto_cleanup),
    );
    let (delete_branch_on_cleanup, o4) = resolve_value(
        scope,
        global.worktree.delete_branch_on_cleanup,
        wt.and_then(|w| w.delete_branch_on_cleanup),
    );
    let (workspace_path_template, o5) = resolve_value(
        scope,
        global.worktree.workspace_path_template.clone(),
        wt.and_then(|w| w.workspace_path_template.clone()),
    );
    let (init_submodules, o6) = resolve_value(
        scope,
        global.worktree.init_submodules,
        wt.and_then(|w| w.init_submodules),
    );

    vec![
        SettingField {
            key: FieldKey::WorktreeEnabled,
            label: "Enabled by Default",
            description: "Enable worktree mode by default for new sessions",
            value: FieldValue::Bool(enabled),
            category: SettingsCategory::Worktree,
            has_override: o0,
            inherited_display: inherited_if(o0, FieldValue::Bool(global.worktree.enabled)),
        },
        SettingField {
            key: FieldKey::PathTemplate,
            label: "Path Template",
            description: "Template for worktree paths ({repo-name}, {branch})",
            value: FieldValue::Text(path_template),
            category: SettingsCategory::Worktree,
            has_override: o1,
            inherited_display: inherited_if(
                o1,
                FieldValue::Text(global.worktree.path_template.clone()),
            ),
        },
        SettingField {
            key: FieldKey::BareRepoPathTemplate,
            label: "Bare Repo Template",
            description: "Template for bare repo worktree paths",
            value: FieldValue::Text(bare_repo_template),
            category: SettingsCategory::Worktree,
            has_override: o2,
            inherited_display: inherited_if(
                o2,
                FieldValue::Text(global.worktree.bare_repo_path_template.clone()),
            ),
        },
        SettingField {
            key: FieldKey::WorktreeAutoCleanup,
            label: "Auto Cleanup",
            description: "Automatically clean up worktrees on session delete",
            value: FieldValue::Bool(auto_cleanup),
            category: SettingsCategory::Worktree,
            has_override: o3,
            inherited_display: inherited_if(o3, FieldValue::Bool(global.worktree.auto_cleanup)),
        },
        SettingField {
            key: FieldKey::DeleteBranchOnCleanup,
            label: "Delete Branch on Cleanup",
            description: "Also delete the git branch when deleting a worktree",
            value: FieldValue::Bool(delete_branch_on_cleanup),
            category: SettingsCategory::Worktree,
            has_override: o4,
            inherited_display: inherited_if(
                o4,
                FieldValue::Bool(global.worktree.delete_branch_on_cleanup),
            ),
        },
        SettingField {
            key: FieldKey::WorkspacePathTemplate,
            label: "Workspace Path Template",
            description: "Template for multi-repo workspace directories ({branch}, {session-id})",
            value: FieldValue::Text(workspace_path_template),
            category: SettingsCategory::Worktree,
            has_override: o5,
            inherited_display: inherited_if(
                o5,
                FieldValue::Text(global.worktree.workspace_path_template.clone()),
            ),
        },
        SettingField {
            key: FieldKey::InitSubmodules,
            label: "Init Submodules",
            description: "Run `git submodule update --init --recursive` after creating a worktree",
            value: FieldValue::Bool(init_submodules),
            category: SettingsCategory::Worktree,
            has_override: o6,
            inherited_display: inherited_if(o6, FieldValue::Bool(global.worktree.init_submodules)),
        },
    ]
}

fn build_sandbox_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let sb = profile.sandbox.as_ref();

    let (enabled_by_default, o1) = resolve_value(
        scope,
        global.sandbox.enabled_by_default,
        sb.and_then(|s| s.enabled_by_default),
    );
    let (default_image, o3) = resolve_value(
        scope,
        global.sandbox.default_image.clone(),
        sb.and_then(|s| s.default_image.clone()),
    );
    let (environment, o4) = resolve_value(
        scope,
        global.sandbox.environment.clone(),
        sb.and_then(|s| s.environment.clone()),
    );
    let (auto_cleanup, o5) = resolve_value(
        scope,
        global.sandbox.auto_cleanup,
        sb.and_then(|s| s.auto_cleanup),
    );
    let (cpu_limit, o_cpu) = resolve_optional(
        scope,
        global.sandbox.cpu_limit.clone(),
        sb.and_then(|s| s.cpu_limit.clone()),
        sb.map(|s| s.cpu_limit.is_some()).unwrap_or(false),
    );
    let (memory_limit, o_mem) = resolve_optional(
        scope,
        global.sandbox.memory_limit.clone(),
        sb.and_then(|s| s.memory_limit.clone()),
        sb.map(|s| s.memory_limit.is_some()).unwrap_or(false),
    );
    let (default_terminal_mode, o6) = resolve_value(
        scope,
        global.sandbox.default_terminal_mode,
        sb.and_then(|s| s.default_terminal_mode),
    );
    let (extra_volumes, o_ev) = resolve_value(
        scope,
        global.sandbox.extra_volumes.clone(),
        sb.and_then(|s| s.extra_volumes.clone()),
    );
    let (port_mappings, o_pm) = resolve_value(
        scope,
        global.sandbox.port_mappings.clone(),
        sb.and_then(|s| s.port_mappings.clone()),
    );
    let (volume_ignores, o7) = resolve_value(
        scope,
        global.sandbox.volume_ignores.clone(),
        sb.and_then(|s| s.volume_ignores.clone()),
    );
    let (mount_ssh, o8) = resolve_value(
        scope,
        global.sandbox.mount_ssh,
        sb.and_then(|s| s.mount_ssh),
    );
    let (selinux_relabel, o_sel) = resolve_value(
        scope,
        global.sandbox.selinux_relabel,
        sb.and_then(|s| s.selinux_relabel),
    );
    let (custom_instruction, o_ci) = resolve_optional(
        scope,
        global.sandbox.custom_instruction.clone(),
        sb.and_then(|s| s.custom_instruction.clone()),
        sb.map(|s| s.custom_instruction.is_some()).unwrap_or(false),
    );
    let (container_runtime, o_cr) = resolve_value(
        scope,
        global.sandbox.container_runtime,
        sb.and_then(|s| s.container_runtime),
    );
    let (volume_ignores_strategy, o_vis) = resolve_value(
        scope,
        global.sandbox.volume_ignores_strategy,
        sb.and_then(|s| s.volume_ignores_strategy),
    );

    let terminal_mode_selected = match default_terminal_mode {
        DefaultTerminalMode::Host => 0,
        DefaultTerminalMode::Container => 1,
    };

    let container_runtime_selected = match container_runtime {
        ContainerRuntimeName::Docker => 0,
        ContainerRuntimeName::Podman => 1,
        ContainerRuntimeName::AppleContainer => 2,
    };

    let global_terminal_mode_selected = match global.sandbox.default_terminal_mode {
        DefaultTerminalMode::Host => 0,
        DefaultTerminalMode::Container => 1,
    };
    let terminal_mode_options = vec!["Host".into(), "Container".into()];

    let global_container_runtime_selected = match global.sandbox.container_runtime {
        ContainerRuntimeName::Docker => 0,
        ContainerRuntimeName::Podman => 1,
        ContainerRuntimeName::AppleContainer => 2,
    };
    let container_runtime_options =
        vec!["Docker".into(), "Podman".into(), "Apple Container".into()];

    let volume_ignores_strategy_selected = match volume_ignores_strategy {
        VolumeIgnoresStrategy::Anonymous => 0,
        VolumeIgnoresStrategy::Named => 1,
    };
    let global_volume_ignores_strategy_selected = match global.sandbox.volume_ignores_strategy {
        VolumeIgnoresStrategy::Anonymous => 0,
        VolumeIgnoresStrategy::Named => 1,
    };
    let volume_ignores_strategy_options = vec!["anonymous".into(), "named".into()];

    vec![
        SettingField {
            key: FieldKey::SandboxEnabledByDefault,
            label: "Enabled by Default",
            description: "Enable sandbox mode by default for new sessions",
            value: FieldValue::Bool(enabled_by_default),
            category: SettingsCategory::Sandbox,
            has_override: o1,
            inherited_display: inherited_if(
                o1,
                FieldValue::Bool(global.sandbox.enabled_by_default),
            ),
        },
        SettingField {
            key: FieldKey::DefaultImage,
            label: "Default Image",
            description: "Container image to use for sandboxes",
            value: FieldValue::Text(default_image),
            category: SettingsCategory::Sandbox,
            has_override: o3,
            inherited_display: inherited_if(
                o3,
                FieldValue::Text(global.sandbox.default_image.clone()),
            ),
        },
        SettingField {
            key: FieldKey::Environment,
            label: "Sandbox Environment",
            description: "Env vars injected into the container: KEY=value (literal, appears in argv), KEY=$VAR (passthrough from host, hidden from argv), KEY=$$literal (escape a leading $), or bare KEY (passthrough). For host (non-sandboxed) sessions, see Session > Host Environment instead.",
            value: FieldValue::List(environment),
            category: SettingsCategory::Sandbox,
            has_override: o4,
            inherited_display: inherited_if(
                o4,
                FieldValue::List(global.sandbox.environment.clone()),
            ),
        },
        SettingField {
            key: FieldKey::SandboxAutoCleanup,
            label: "Auto Cleanup",
            description: "Remove containers when sessions are deleted",
            value: FieldValue::Bool(auto_cleanup),
            category: SettingsCategory::Sandbox,
            has_override: o5,
            inherited_display: inherited_if(o5, FieldValue::Bool(global.sandbox.auto_cleanup)),
        },
        SettingField {
            key: FieldKey::CpuLimit,
            label: "CPU Limit",
            description: "CPU limit for containers (e.g. \"4\")",
            value: FieldValue::OptionalText(cpu_limit),
            category: SettingsCategory::Sandbox,
            has_override: o_cpu,
            inherited_display: inherited_if(
                o_cpu,
                FieldValue::OptionalText(global.sandbox.cpu_limit.clone()),
            ),
        },
        SettingField {
            key: FieldKey::MemoryLimit,
            label: "Memory Limit",
            description: "Memory limit for containers (e.g. \"8g\", \"512m\")",
            value: FieldValue::OptionalText(memory_limit),
            category: SettingsCategory::Sandbox,
            has_override: o_mem,
            inherited_display: inherited_if(
                o_mem,
                FieldValue::OptionalText(global.sandbox.memory_limit.clone()),
            ),
        },
        SettingField {
            key: FieldKey::DefaultTerminalMode,
            label: "Default Terminal Mode",
            description: "Default terminal for sandboxed sessions (toggle with 'c' key)",
            value: FieldValue::Select {
                selected: terminal_mode_selected,
                options: terminal_mode_options.clone(),
            },
            category: SettingsCategory::Sandbox,
            has_override: o6,
            inherited_display: inherited_if(
                o6,
                FieldValue::Select {
                    selected: global_terminal_mode_selected,
                    options: terminal_mode_options,
                },
            ),
        },
        SettingField {
            key: FieldKey::ExtraVolumes,
            label: "Extra Volumes",
            description: "Additional volume mounts (host:container or host:container:ro)",
            value: FieldValue::List(extra_volumes),
            category: SettingsCategory::Sandbox,
            has_override: o_ev,
            inherited_display: inherited_if(
                o_ev,
                FieldValue::List(global.sandbox.extra_volumes.clone()),
            ),
        },
        SettingField {
            key: FieldKey::PortMappings,
            label: "Port Mappings",
            description: "Expose container ports to host (e.g. 3000:3000)",
            value: FieldValue::List(port_mappings),
            category: SettingsCategory::Sandbox,
            has_override: o_pm,
            inherited_display: inherited_if(
                o_pm,
                FieldValue::List(global.sandbox.port_mappings.clone()),
            ),
        },
        SettingField {
            key: FieldKey::VolumeIgnores,
            label: "Volume Ignores",
            description: "Directories to exclude from host mount (e.g. target, node_modules)",
            value: FieldValue::List(volume_ignores),
            category: SettingsCategory::Sandbox,
            has_override: o7,
            inherited_display: inherited_if(
                o7,
                FieldValue::List(global.sandbox.volume_ignores.clone()),
            ),
        },
        SettingField {
            key: FieldKey::VolumeIgnoresStrategy,
            label: "Volume Ignores Strategy",
            description: "anonymous: default, works on Linux. named: use deterministic Docker/Podman named volumes, required on macOS/VirtioFS to reliably shadow bind-mount subdirectories.",
            value: FieldValue::Select {
                selected: volume_ignores_strategy_selected,
                options: volume_ignores_strategy_options.clone(),
            },
            category: SettingsCategory::Sandbox,
            has_override: o_vis,
            inherited_display: inherited_if(
                o_vis,
                FieldValue::Select {
                    selected: global_volume_ignores_strategy_selected,
                    options: volume_ignores_strategy_options,
                },
            ),
        },
        SettingField {
            key: FieldKey::MountSsh,
            label: "Mount SSH",
            description: "Mount ~/.ssh into sandbox containers (for git SSH access)",
            value: FieldValue::Bool(mount_ssh),
            category: SettingsCategory::Sandbox,
            has_override: o8,
            inherited_display: inherited_if(o8, FieldValue::Bool(global.sandbox.mount_ssh)),
        },
        SettingField {
            key: FieldKey::SelinuxRelabel,
            label: "SELinux Relabel",
            description: "Append the :z SELinux relabel flag to sandbox bind mounts (needed on Fedora/RHEL; relabels host paths)",
            value: FieldValue::Bool(selinux_relabel),
            category: SettingsCategory::Sandbox,
            has_override: o_sel,
            inherited_display: inherited_if(o_sel, FieldValue::Bool(global.sandbox.selinux_relabel)),
        },
        SettingField {
            key: FieldKey::CustomInstruction,
            label: "Custom Instruction",
            description: "Custom instruction text appended to the agent's system prompt in sandboxed sessions (Claude, Codex only)",
            value: FieldValue::OptionalText(custom_instruction),
            category: SettingsCategory::Sandbox,
            has_override: o_ci,
            inherited_display: inherited_if(
                o_ci,
                FieldValue::OptionalText(global.sandbox.custom_instruction.clone()),
            ),
        },
        SettingField {
            key: FieldKey::ContainerRuntime,
            label: "Container Runtime",
            description: "Container runtime for sandboxing",
            value: FieldValue::Select {
                selected: container_runtime_selected,
                options: container_runtime_options.clone(),
            },
            category: SettingsCategory::Sandbox,
            has_override: o_cr,
            inherited_display: inherited_if(
                o_cr,
                FieldValue::Select {
                    selected: global_container_runtime_selected,
                    options: container_runtime_options,
                },
            ),
        },
    ]
}

fn build_tmux_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let tmux = profile.tmux.as_ref();

    let (status_bar, status_bar_override) = resolve_value(
        scope,
        global.tmux.status_bar,
        tmux.and_then(|t| t.status_bar),
    );

    let (mouse, mouse_override) =
        resolve_value(scope, global.tmux.mouse, tmux.and_then(|t| t.mouse));

    let (clipboard, clipboard_override) =
        resolve_value(scope, global.tmux.clipboard, tmux.and_then(|t| t.clipboard));

    let status_bar_selected = match status_bar {
        TmuxStatusBarMode::Auto => 0,
        TmuxStatusBarMode::Enabled => 1,
        TmuxStatusBarMode::Disabled => 2,
    };

    let mouse_selected = match mouse {
        TmuxMouseMode::Auto => 0,
        TmuxMouseMode::Enabled => 1,
        TmuxMouseMode::Disabled => 2,
    };

    let clipboard_selected = match clipboard {
        TmuxClipboardMode::Auto => 0,
        TmuxClipboardMode::Enabled => 1,
        TmuxClipboardMode::Disabled => 2,
    };

    let global_status_bar_selected = match global.tmux.status_bar {
        TmuxStatusBarMode::Auto => 0,
        TmuxStatusBarMode::Enabled => 1,
        TmuxStatusBarMode::Disabled => 2,
    };
    let global_mouse_selected = match global.tmux.mouse {
        TmuxMouseMode::Auto => 0,
        TmuxMouseMode::Enabled => 1,
        TmuxMouseMode::Disabled => 2,
    };
    let global_clipboard_selected = match global.tmux.clipboard {
        TmuxClipboardMode::Auto => 0,
        TmuxClipboardMode::Enabled => 1,
        TmuxClipboardMode::Disabled => 2,
    };
    let tmux_options = vec!["Auto".into(), "Enabled".into(), "Disabled".into()];

    vec![
        SettingField {
            key: FieldKey::StatusBar,
            label: "Status Bar",
            description: "Control tmux status bar styling (Auto respects your tmux config)",
            value: FieldValue::Select {
                selected: status_bar_selected,
                options: tmux_options.clone(),
            },
            category: SettingsCategory::Tmux,
            has_override: status_bar_override,
            inherited_display: inherited_if(
                status_bar_override,
                FieldValue::Select {
                    selected: global_status_bar_selected,
                    options: tmux_options.clone(),
                },
            ),
        },
        SettingField {
            key: FieldKey::Mouse,
            label: "Mouse Support",
            description: "Control mouse scrolling (Auto respects your tmux config)",
            value: FieldValue::Select {
                selected: mouse_selected,
                options: tmux_options.clone(),
            },
            category: SettingsCategory::Tmux,
            has_override: mouse_override,
            inherited_display: inherited_if(
                mouse_override,
                FieldValue::Select {
                    selected: global_mouse_selected,
                    options: tmux_options.clone(),
                },
            ),
        },
        SettingField {
            key: FieldKey::Clipboard,
            label: "Clipboard Pass-through",
            description: "Forward OSC 52 clipboard from agents to your terminal (Auto respects your tmux config)",
            value: FieldValue::Select {
                selected: clipboard_selected,
                options: tmux_options.clone(),
            },
            category: SettingsCategory::Tmux,
            has_override: clipboard_override,
            inherited_display: inherited_if(
                clipboard_override,
                FieldValue::Select {
                    selected: global_clipboard_selected,
                    options: tmux_options,
                },
            ),
        },
    ]
}

fn build_session_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let session = profile.session.as_ref();

    let (yolo_mode_default, yolo_override) = resolve_value(
        scope,
        global.session.yolo_mode_default,
        session.and_then(|s| s.yolo_mode_default),
    );

    let (strict_hotkeys, strict_hotkeys_override) = resolve_value(
        scope,
        global.session.strict_hotkeys,
        session.and_then(|s| s.strict_hotkeys),
    );

    let (snooze_duration_minutes, snooze_duration_override) = resolve_value(
        scope,
        global.session.snooze_duration_minutes as u64,
        session
            .and_then(|s| s.snooze_duration_minutes)
            .map(|v| v as u64),
    );

    let (auto_stop_idle_secs, auto_stop_idle_secs_override) = resolve_value(
        scope,
        global.session.auto_stop_idle_secs as u64,
        session
            .and_then(|s| s.auto_stop_idle_secs)
            .map(|v| v as u64),
    );

    let (restart_wake_message, restart_wake_message_override) = resolve_value(
        scope,
        global.session.restart_wake_message.clone(),
        session.and_then(|s| s.restart_wake_message.clone()),
    );

    let (row_tag, row_tag_override) = resolve_value(
        scope,
        global.session.row_tag,
        session.and_then(|s| s.row_tag),
    );

    let (host_environment, host_env_override) = resolve_value(
        scope,
        global.environment.clone(),
        profile.environment.clone(),
    );

    let mut fields = vec![
        SettingField {
            key: FieldKey::YoloModeDefault,
            label: "YOLO Mode Default",
            description: "Enable YOLO mode by default for new sessions",
            value: FieldValue::Bool(yolo_mode_default),
            category: SettingsCategory::Session,
            has_override: yolo_override,
            inherited_display: inherited_if(
                yolo_override,
                FieldValue::Bool(global.session.yolo_mode_default),
            ),
        },
        SettingField {
            key: FieldKey::StrictHotkeys,
            label: "Strict Hotkeys",
            description:
                "Require Shift/Ctrl for action hotkeys (guards against dictation/stray input)",
            value: FieldValue::Bool(strict_hotkeys),
            category: SettingsCategory::Session,
            has_override: strict_hotkeys_override,
            inherited_display: inherited_if(
                strict_hotkeys_override,
                FieldValue::Bool(global.session.strict_hotkeys),
            ),
        },
        SettingField {
            key: FieldKey::SnoozeDurationMinutes,
            label: "Snooze Duration (minutes)",
            description: "Default snooze for `aoe session snooze` (1-43200 min, picker overrides)",
            value: FieldValue::Number(snooze_duration_minutes),
            category: SettingsCategory::Session,
            has_override: snooze_duration_override,
            inherited_display: inherited_if(
                snooze_duration_override,
                FieldValue::Number(global.session.snooze_duration_minutes as u64),
            ),
        },
        SettingField {
            key: FieldKey::SessionAutoStopIdleSecs,
            label: "Auto-stop Idle Sessions (seconds)",
            description: "Stop a plain tmux session after it sits idle this long (0 disables; attached or recently-used sessions are spared)",
            value: FieldValue::Number(auto_stop_idle_secs),
            category: SettingsCategory::Session,
            has_override: auto_stop_idle_secs_override,
            inherited_display: inherited_if(
                auto_stop_idle_secs_override,
                FieldValue::Number(global.session.auto_stop_idle_secs as u64),
            ),
        },
        SettingField {
            key: FieldKey::RestartWakeMessage,
            label: "Restart Wake Message",
            description: "Sent to the agent after restart to resume work. Empty = no wake nudge.",
            value: FieldValue::Text(restart_wake_message),
            category: SettingsCategory::Session,
            has_override: restart_wake_message_override,
            inherited_display: inherited_if(
                restart_wake_message_override,
                FieldValue::Text(global.session.restart_wake_message.clone()),
            ),
        },
        SettingField {
            key: FieldKey::RowTag,
            label: "Row Tag",
            description:
                "What to show next to each session title: Auto (profile in all-profiles view), \
                 None, Profile (always), Sandbox (sb on sandboxed rows), or Branch.",
            value: FieldValue::Select {
                selected: row_tag_to_index(row_tag),
                options: ROW_TAG_OPTIONS.iter().map(|s| s.to_string()).collect(),
            },
            category: SettingsCategory::Session,
            has_override: row_tag_override,
            inherited_display: inherited_if(
                row_tag_override,
                FieldValue::Select {
                    selected: row_tag_to_index(global.session.row_tag),
                    options: ROW_TAG_OPTIONS.iter().map(|s| s.to_string()).collect(),
                },
            ),
        },
        SettingField {
            key: FieldKey::HostEnvironment,
            label: "Host Environment",
            description: "Env vars injected into the host command line: KEY=value (literal), KEY=$VAR (passthrough from host), KEY=$$literal (escape a leading $), or bare KEY (passthrough). All forms resolve to a literal `KEY=value` arg in the spawned process, visible in `ps`; for secrets you want hidden from argv, configure Sandbox > Sandbox Environment instead. Profile value replaces the global list.",
            value: FieldValue::List(host_environment),
            category: SettingsCategory::Session,
            has_override: host_env_override,
            inherited_display: inherited_if(
                host_env_override,
                FieldValue::List(global.environment.clone()),
            ),
        },
    ];

    if scope == SettingsScope::Global {
        fields.push(SettingField {
            key: FieldKey::ConfirmBeforeQuit,
            label: "Confirm Before Quit",
            description: "Warn before quitting aoe when you press `q` on the home screen \
                          (the dialog can also turn this off). Ctrl+C always force-quits.",
            value: FieldValue::Bool(global.session.confirm_before_quit),
            category: SettingsCategory::Session,
            has_override: false,
            inherited_display: None,
        });
        fields.push(SettingField {
            key: FieldKey::SessionIdPollerMaxThreads,
            label: "Max Session-ID Poller Threads",
            description:
                "Process-wide cap on threads polling the tmux session ID for live sessions \
                 (one thread per session). When the cap is reached, new sessions are not \
                 polled and their session ID will not refresh.",
            value: FieldValue::Number(u64::from(global.session.session_id_poller_max_threads)),
            category: SettingsCategory::Session,
            has_override: false,
            inherited_display: None,
        });
    }

    fields
}

/// Per-agent / per-tool configuration. Split out of Session because
/// the bucket was a grab-bag and DefaultTool + the agent_* maps are a
/// coherent mental model on their own.
fn build_agents_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let session = profile.session.as_ref();

    let (default_tool, has_override) = resolve_optional(
        scope,
        global.session.default_tool.clone(),
        session.and_then(|s| s.default_tool.clone()),
        session.map(|s| s.default_tool.is_some()).unwrap_or(false),
    );

    let selected = crate::agents::settings_index_from_name(default_tool.as_deref());

    let mut options = vec!["Auto (first available)".to_string()];
    options.extend(crate::agents::agent_names().iter().map(|n| n.to_string()));

    let global_tool_selected =
        crate::agents::settings_index_from_name(global.session.default_tool.as_deref());

    let (agent_status_hooks, status_hooks_override) = resolve_value(
        scope,
        global.session.agent_status_hooks,
        session.and_then(|s| s.agent_status_hooks),
    );

    let (extra_args_map, extra_args_override) = resolve_value(
        scope,
        global.session.agent_extra_args.clone(),
        session.and_then(|s| s.agent_extra_args.clone()),
    );
    let extra_args_list: Vec<String> = {
        let mut items: Vec<_> = extra_args_map
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };
    let global_extra_args_list: Vec<String> = {
        let mut items: Vec<_> = global
            .session
            .agent_extra_args
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };

    let (cmd_override_map, cmd_override_override) = resolve_value(
        scope,
        global.session.agent_command_override.clone(),
        session.and_then(|s| s.agent_command_override.clone()),
    );
    let cmd_override_list: Vec<String> = {
        let mut items: Vec<_> = cmd_override_map
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };
    let global_cmd_override_list: Vec<String> = {
        let mut items: Vec<_> = global
            .session
            .agent_command_override
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };

    let (custom_agents_map, custom_agents_override) = resolve_value(
        scope,
        global.session.custom_agents.clone(),
        session.and_then(|s| s.custom_agents.clone()),
    );
    let custom_agents_list: Vec<String> = {
        let mut items: Vec<_> = custom_agents_map
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };
    let global_custom_agents_list: Vec<String> = {
        let mut items: Vec<_> = global
            .session
            .custom_agents
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };

    let (detect_as_map, detect_as_override) = resolve_value(
        scope,
        global.session.agent_detect_as.clone(),
        session.and_then(|s| s.agent_detect_as.clone()),
    );
    let detect_as_list: Vec<String> = {
        let mut items: Vec<_> = detect_as_map
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };
    let global_detect_as_list: Vec<String> = {
        let mut items: Vec<_> = global
            .session
            .agent_detect_as
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };

    let (cockpit_cmd_map, cockpit_cmd_override) = resolve_value(
        scope,
        global.session.agent_cockpit_cmd.clone(),
        session.and_then(|s| s.agent_cockpit_cmd.clone()),
    );
    let cockpit_cmd_list: Vec<String> = {
        let mut items: Vec<_> = cockpit_cmd_map
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };
    let global_cockpit_cmd_list: Vec<String> = {
        let mut items: Vec<_> = global
            .session
            .agent_cockpit_cmd
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        items.sort();
        items
    };

    vec![
        SettingField {
            key: FieldKey::DefaultTool,
            label: "Default Tool",
            description: "Default coding tool for new sessions",
            value: FieldValue::Select {
                selected,
                options: options.clone(),
            },
            category: SettingsCategory::Agents,
            has_override,
            inherited_display: inherited_if(
                has_override,
                FieldValue::Select {
                    selected: global_tool_selected,
                    options,
                },
            ),
        },
        SettingField {
            key: FieldKey::AgentExtraArgs,
            label: "Agent Extra Args",
            description:
                "Per-agent extra arguments appended after the binary (e.g. opencode=--port 8080)",
            value: FieldValue::List(extra_args_list),
            category: SettingsCategory::Agents,
            has_override: extra_args_override,
            inherited_display: inherited_if(
                extra_args_override,
                FieldValue::List(global_extra_args_list),
            ),
        },
        SettingField {
            key: FieldKey::AgentCommandOverride,
            label: "Agent Command Override",
            description: "Per-agent command override replacing the binary (e.g. claude=my-wrapper)",
            value: FieldValue::List(cmd_override_list),
            category: SettingsCategory::Agents,
            has_override: cmd_override_override,
            inherited_display: inherited_if(
                cmd_override_override,
                FieldValue::List(global_cmd_override_list),
            ),
        },
        SettingField {
            key: FieldKey::CustomAgents,
            label: "Custom Agents",
            description:
                "User-defined agents: name=command (e.g. lenovo-claude=ssh -t lenovo claude)",
            value: FieldValue::List(custom_agents_list),
            category: SettingsCategory::Agents,
            has_override: custom_agents_override,
            inherited_display: inherited_if(
                custom_agents_override,
                FieldValue::List(global_custom_agents_list),
            ),
        },
        SettingField {
            key: FieldKey::AgentDetectAs,
            label: "Agent Detect As",
            description: "Status detection mapping: agent=builtin (e.g. lenovo-claude=claude)",
            value: FieldValue::List(detect_as_list),
            category: SettingsCategory::Agents,
            has_override: detect_as_override,
            inherited_display: inherited_if(
                detect_as_override,
                FieldValue::List(global_detect_as_list),
            ),
        },
        SettingField {
            key: FieldKey::AgentCockpitCmd,
            label: "Agent Cockpit Command",
            description:
                "ACP launch command enabling a custom agent in cockpit: agent=command (e.g. oc-sp=ocp run sp acp)",
            value: FieldValue::List(cockpit_cmd_list),
            category: SettingsCategory::Agents,
            has_override: cockpit_cmd_override,
            inherited_display: inherited_if(
                cockpit_cmd_override,
                FieldValue::List(global_cockpit_cmd_list),
            ),
        },
        SettingField {
            key: FieldKey::AgentStatusHooks,
            label: "Agent Status Hooks",
            description: "Install status-detection hooks into the agent's config file",
            value: FieldValue::Bool(agent_status_hooks),
            category: SettingsCategory::Agents,
            has_override: status_hooks_override,
            inherited_display: inherited_if(
                status_hooks_override,
                FieldValue::Bool(global.session.agent_status_hooks),
            ),
        },
    ]
}

/// "How do I get into a session" configuration. The attach-mode trio
/// (Tab vs Enter vs new-session) is a coherent decision the user
/// thinks about together; pulling it out of the Session grab-bag puts
/// the related knobs next to each other.
fn build_interaction_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let session = profile.session.as_ref();

    let (live_send_exit_chord, live_send_exit_chord_override) = resolve_value(
        scope,
        global.session.live_send_exit_chord.clone(),
        session.and_then(|s| s.live_send_exit_chord.clone()),
    );

    let (live_send_leader, live_send_leader_override) = resolve_value(
        scope,
        global.session.live_send_leader.clone(),
        session.and_then(|s| s.live_send_leader.clone()),
    );

    let (new_session_attach_mode, new_session_attach_mode_override) = resolve_value(
        scope,
        global.session.new_session_attach_mode,
        session.and_then(|s| s.new_session_attach_mode),
    );

    let (default_attach_mode, default_attach_mode_override) = resolve_value(
        scope,
        global.session.default_attach_mode,
        session.and_then(|s| s.default_attach_mode),
    );

    let (click_action, click_action_override) = resolve_value(
        scope,
        global.session.click_action,
        session.and_then(|s| s.click_action),
    );

    let (mouse_capture, mouse_capture_override) = resolve_value(
        scope,
        global.session.mouse_capture,
        session.and_then(|s| s.mouse_capture),
    );

    vec![
        SettingField {
            key: FieldKey::DefaultAttachMode,
            label: "Default Attach Mode",
            description: "What Enter (and double-click) does on a session row in \
                 the Agent view: attach to tmux (default, historical \
                 behavior) or enter live-send mode so the home list stays \
                 visible and keystrokes pipe through to the agent. \
                 Terminal/Tool views and cockpit sessions ignore this \
                 setting.",
            value: FieldValue::Select {
                selected: new_session_attach_mode_to_index(default_attach_mode),
                options: NEW_SESSION_ATTACH_MODE_OPTIONS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            },
            category: SettingsCategory::Interaction,
            has_override: default_attach_mode_override,
            inherited_display: inherited_if(
                default_attach_mode_override,
                FieldValue::Select {
                    selected: new_session_attach_mode_to_index(global.session.default_attach_mode),
                    options: NEW_SESSION_ATTACH_MODE_OPTIONS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                },
            ),
        },
        SettingField {
            key: FieldKey::NewSessionAttachMode,
            label: "New Session Attach Mode",
            description: "What to do after creating a new session: drop into tmux \
                 (default, historical behavior) or enter live-send mode so \
                 you never have to be inside tmux. Cockpit sessions ignore \
                 this setting.",
            value: FieldValue::Select {
                selected: new_session_attach_mode_to_index(new_session_attach_mode),
                options: NEW_SESSION_ATTACH_MODE_OPTIONS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            },
            category: SettingsCategory::Interaction,
            has_override: new_session_attach_mode_override,
            inherited_display: inherited_if(
                new_session_attach_mode_override,
                FieldValue::Select {
                    selected: new_session_attach_mode_to_index(
                        global.session.new_session_attach_mode,
                    ),
                    options: NEW_SESSION_ATTACH_MODE_OPTIONS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                },
            ),
        },
        SettingField {
            key: FieldKey::ClickAction,
            label: "Mouse Click Action",
            description: "What a single mouse click on a session row does in the \
                 Agent view. `Live mode` (default) enters live-send for the \
                 clicked row, the historical behavior. `Select only` just \
                 moves the cursor so you can read the preview without ever \
                 entering live-send. Double-click still activates via Default \
                 Attach Mode regardless of this setting.",
            value: FieldValue::Select {
                selected: click_action_to_index(click_action),
                options: CLICK_ACTION_OPTIONS.iter().map(|s| s.to_string()).collect(),
            },
            category: SettingsCategory::Interaction,
            has_override: click_action_override,
            inherited_display: inherited_if(
                click_action_override,
                FieldValue::Select {
                    selected: click_action_to_index(global.session.click_action),
                    options: CLICK_ACTION_OPTIONS.iter().map(|s| s.to_string()).collect(),
                },
            ),
        },
        SettingField {
            key: FieldKey::LiveSendExitChord,
            label: "Live-Send Exit Chord",
            description: "Comma-separated chord specs that exit live-send mode. \
                 Tmux-style: C-q, M-x, F12. Default `C-q` works in \
                 every terminal we ship to; add entries for additional \
                 exits if you need to send C-q through to the agent.",
            value: FieldValue::Text(live_send_exit_chord),
            category: SettingsCategory::Interaction,
            has_override: live_send_exit_chord_override,
            inherited_display: inherited_if(
                live_send_exit_chord_override,
                FieldValue::Text(global.session.live_send_exit_chord.clone()),
            ),
        },
        SettingField {
            key: FieldKey::LiveSendLeader,
            label: "Live-Send Leader",
            description: "Tmux-style leader (prefix) chord for live-send commands. \
                 Default `C-b` matches tmux. In live mode the leader arms a \
                 menu: leader then `k` opens the command palette, `b` toggles \
                 the sidebar, `q` exits. Press the leader twice to send it \
                 through to the agent. Leave empty to disable the leader. The \
                 exit chord above stays a separate single-press exit.",
            value: FieldValue::Text(live_send_leader),
            category: SettingsCategory::Interaction,
            has_override: live_send_leader_override,
            inherited_display: inherited_if(
                live_send_leader_override,
                FieldValue::Text(global.session.live_send_leader.clone()),
            ),
        },
        SettingField {
            key: FieldKey::MouseCapture,
            label: "Mouse Capture",
            description: "Request xterm mouse tracking so the TUI handles the \
                 scroll wheel (preview-pane scroll) and click-to-select rows. \
                 Disable to hand the wheel and text selection back to the \
                 terminal, e.g. iOS Mosh + Termius/Blink where mouse-tracking \
                 escapes aren't forwarded reliably. The AOE_MOUSE_CAPTURE env \
                 var remains an opt-out backstop and can still force capture \
                 off when set.",
            value: FieldValue::Bool(mouse_capture),
            category: SettingsCategory::Interaction,
            has_override: mouse_capture_override,
            inherited_display: inherited_if(
                mouse_capture_override,
                FieldValue::Bool(global.session.mouse_capture),
            ),
        },
    ]
}

fn build_sound_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let snd = profile.sound.as_ref();

    let (enabled, o1) = resolve_value(scope, global.sound.enabled, snd.and_then(|s| s.enabled));

    let (mode, o2) = resolve_value(
        scope,
        global.sound.mode.clone(),
        snd.and_then(|s| s.mode.clone()),
    );

    let mode_selected = match &mode {
        SoundMode::Random => 0,
        SoundMode::Specific(_) => 1,
    };

    let (on_start, o3) = resolve_optional(
        scope,
        global.sound.on_start.clone(),
        snd.and_then(|s| s.on_start.clone()),
        snd.map(|s| s.on_start.is_some()).unwrap_or(false),
    );
    let (on_running, o4) = resolve_optional(
        scope,
        global.sound.on_running.clone(),
        snd.and_then(|s| s.on_running.clone()),
        snd.map(|s| s.on_running.is_some()).unwrap_or(false),
    );
    let (on_waiting, o5) = resolve_optional(
        scope,
        global.sound.on_waiting.clone(),
        snd.and_then(|s| s.on_waiting.clone()),
        snd.map(|s| s.on_waiting.is_some()).unwrap_or(false),
    );
    let (on_idle, o6) = resolve_optional(
        scope,
        global.sound.on_idle.clone(),
        snd.and_then(|s| s.on_idle.clone()),
        snd.map(|s| s.on_idle.is_some()).unwrap_or(false),
    );
    let (on_error, o7) = resolve_optional(
        scope,
        global.sound.on_error.clone(),
        snd.and_then(|s| s.on_error.clone()),
        snd.map(|s| s.on_error.is_some()).unwrap_or(false),
    );
    let (on_approval, o8) = resolve_optional(
        scope,
        global.sound.on_approval.clone(),
        snd.and_then(|s| s.on_approval.clone()),
        snd.map(|s| s.on_approval.is_some()).unwrap_or(false),
    );

    let global_mode_selected = match &global.sound.mode {
        SoundMode::Random => 0,
        SoundMode::Specific(_) => 1,
    };
    let sound_mode_options = vec!["Random".into(), "Specific".into()];

    let (volume, o_vol) = resolve_value(scope, global.sound.volume, snd.and_then(|s| s.volume));
    let vol_opts = volume_options();
    let vol_idx = volume_to_index(volume);

    vec![
        SettingField {
            key: FieldKey::SoundEnabled,
            label: "Enabled",
            description: "Play sounds on agent state transitions",
            value: FieldValue::Bool(enabled),
            category: SettingsCategory::Sound,
            has_override: o1,
            inherited_display: inherited_if(o1, FieldValue::Bool(global.sound.enabled)),
        },
        SettingField {
            key: FieldKey::SoundMode,
            label: "Mode",
            description: "How to select sounds (Random or Specific file name)",
            value: FieldValue::Select {
                selected: mode_selected,
                options: sound_mode_options.clone(),
            },
            category: SettingsCategory::Sound,
            has_override: o2,
            inherited_display: inherited_if(
                o2,
                FieldValue::Select {
                    selected: global_mode_selected,
                    options: sound_mode_options,
                },
            ),
        },
        SettingField {
            key: FieldKey::SoundVolume,
            label: "Volume",
            description: "Playback volume (0.1 = min, 1.0 = normal, 1.5 = max), step 0.1. Ignored when aplay is the Linux backend.",
            value: FieldValue::Select {
                selected: vol_idx,
                options: vol_opts.clone(),
            },
            category: SettingsCategory::Sound,
            has_override: o_vol,
            inherited_display: inherited_if(
                o_vol,
                FieldValue::Select {
                    selected: volume_to_index(global.sound.volume),
                    options: vol_opts,
                },
            ),
        },
        SettingField {
            key: FieldKey::SoundOnStart,
            label: "On Start",
            description: "Specify file name with extension",
            value: FieldValue::OptionalText(on_start),
            category: SettingsCategory::Sound,
            has_override: o3,
            inherited_display: inherited_if(
                o3,
                FieldValue::OptionalText(global.sound.on_start.clone()),
            ),
        },
        SettingField {
            key: FieldKey::SoundOnRunning,
            label: "On Running",
            description: "Specify file name with extension",
            value: FieldValue::OptionalText(on_running),
            category: SettingsCategory::Sound,
            has_override: o4,
            inherited_display: inherited_if(
                o4,
                FieldValue::OptionalText(global.sound.on_running.clone()),
            ),
        },
        SettingField {
            key: FieldKey::SoundOnWaiting,
            label: "On Waiting",
            description: "Specify file name with extension",
            value: FieldValue::OptionalText(on_waiting),
            category: SettingsCategory::Sound,
            has_override: o5,
            inherited_display: inherited_if(
                o5,
                FieldValue::OptionalText(global.sound.on_waiting.clone()),
            ),
        },
        SettingField {
            key: FieldKey::SoundOnIdle,
            label: "On Idle",
            description: "Specify file name with extension",
            value: FieldValue::OptionalText(on_idle),
            category: SettingsCategory::Sound,
            has_override: o6,
            inherited_display: inherited_if(
                o6,
                FieldValue::OptionalText(global.sound.on_idle.clone()),
            ),
        },
        SettingField {
            key: FieldKey::SoundOnError,
            label: "On Error",
            description: "Specify file name with extension",
            value: FieldValue::OptionalText(on_error),
            category: SettingsCategory::Sound,
            has_override: o7,
            inherited_display: inherited_if(
                o7,
                FieldValue::OptionalText(global.sound.on_error.clone()),
            ),
        },
        SettingField {
            key: FieldKey::SoundOnApproval,
            label: "On Approval",
            description: "Cockpit only. Played in the browser when a session needs permission. Specify file name with extension",
            value: FieldValue::OptionalText(on_approval),
            category: SettingsCategory::Sound,
            has_override: o8,
            inherited_display: inherited_if(
                o8,
                FieldValue::OptionalText(global.sound.on_approval.clone()),
            ),
        },
    ]
}

fn build_hooks_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let hooks = profile.hooks.as_ref();

    let (on_create, o1) = resolve_value(
        scope,
        global.hooks.on_create.clone(),
        hooks.and_then(|h| h.on_create.clone()),
    );
    let (on_launch, o2) = resolve_value(
        scope,
        global.hooks.on_launch.clone(),
        hooks.and_then(|h| h.on_launch.clone()),
    );
    let (on_destroy, o3) = resolve_value(
        scope,
        global.hooks.on_destroy.clone(),
        hooks.and_then(|h| h.on_destroy.clone()),
    );

    vec![
        SettingField {
            key: FieldKey::HookOnCreate,
            label: "On Create",
            description: "Commands run once when a session is first created. Runs inside sandbox when enabled.",
            value: FieldValue::List(on_create),
            category: SettingsCategory::Hooks,
            has_override: o1,
            inherited_display: inherited_if(
                o1,
                FieldValue::List(global.hooks.on_create.clone()),
            ),
        },
        SettingField {
            key: FieldKey::HookOnLaunch,
            label: "On Launch",
            description: "Commands run every time a session starts. Runs inside sandbox when enabled.",
            value: FieldValue::List(on_launch),
            category: SettingsCategory::Hooks,
            has_override: o2,
            inherited_display: inherited_if(
                o2,
                FieldValue::List(global.hooks.on_launch.clone()),
            ),
        },
        SettingField {
            key: FieldKey::HookOnDestroy,
            label: "On Destroy",
            description: "Commands run when a session is deleted, before cleanup. Use for teardown (e.g. docker-compose down).",
            value: FieldValue::List(on_destroy),
            category: SettingsCategory::Hooks,
            has_override: o3,
            inherited_display: inherited_if(
                o3,
                FieldValue::List(global.hooks.on_destroy.clone()),
            ),
        },
    ]
}

fn build_status_hook_fields(
    scope: SettingsScope,
    global: &Config,
    profile: &ProfileConfig,
) -> Vec<SettingField> {
    let hooks = profile.status_hooks.as_ref();

    let (enabled, o1) = resolve_value(
        scope,
        global.status_hooks.enabled,
        hooks.and_then(|h| h.enabled),
    );
    let (debounce_ms, debounce_override) = resolve_value(
        scope,
        global.status_hooks.debounce_ms,
        hooks.and_then(|h| h.debounce_ms),
    );
    let (on_starting, o2) = resolve_optional(
        scope,
        global.status_hooks.on_starting.clone(),
        hooks.and_then(|h| h.on_starting.clone()),
        hooks.map(|h| h.on_starting.is_some()).unwrap_or(false),
    );
    let (on_running, o3) = resolve_optional(
        scope,
        global.status_hooks.on_running.clone(),
        hooks.and_then(|h| h.on_running.clone()),
        hooks.map(|h| h.on_running.is_some()).unwrap_or(false),
    );
    let (on_waiting, o4) = resolve_optional(
        scope,
        global.status_hooks.on_waiting.clone(),
        hooks.and_then(|h| h.on_waiting.clone()),
        hooks.map(|h| h.on_waiting.is_some()).unwrap_or(false),
    );
    let (on_idle, o5) = resolve_optional(
        scope,
        global.status_hooks.on_idle.clone(),
        hooks.and_then(|h| h.on_idle.clone()),
        hooks.map(|h| h.on_idle.is_some()).unwrap_or(false),
    );
    let (on_error, o6) = resolve_optional(
        scope,
        global.status_hooks.on_error.clone(),
        hooks.and_then(|h| h.on_error.clone()),
        hooks.map(|h| h.on_error.is_some()).unwrap_or(false),
    );
    let (on_change, o7) = resolve_optional(
        scope,
        global.status_hooks.on_change.clone(),
        hooks.and_then(|h| h.on_change.clone()),
        hooks.map(|h| h.on_change.is_some()).unwrap_or(false),
    );

    vec![
        SettingField {
            key: FieldKey::StatusHooksEnabled,
            label: "Enabled",
            description: "Run local commands when TUI sessions change status",
            value: FieldValue::Bool(enabled),
            category: SettingsCategory::StatusHooks,
            has_override: o1,
            inherited_display: inherited_if(o1, FieldValue::Bool(global.status_hooks.enabled)),
        },
        SettingField {
            key: FieldKey::StatusHookDebounceMs,
            label: "Debounce (ms)",
            description: "Milliseconds a status must remain stable before running hook commands",
            value: FieldValue::Number(debounce_ms),
            category: SettingsCategory::StatusHooks,
            has_override: debounce_override,
            inherited_display: inherited_if(
                debounce_override,
                FieldValue::Number(global.status_hooks.debounce_ms),
            ),
        },
        SettingField {
            key: FieldKey::StatusHookOnStarting,
            label: "On Starting",
            description: "Shell command run when a session enters Starting",
            value: FieldValue::OptionalText(on_starting),
            category: SettingsCategory::StatusHooks,
            has_override: o2,
            inherited_display: inherited_if(
                o2,
                FieldValue::OptionalText(global.status_hooks.on_starting.clone()),
            ),
        },
        SettingField {
            key: FieldKey::StatusHookOnRunning,
            label: "On Running",
            description: "Shell command run when a session enters Running",
            value: FieldValue::OptionalText(on_running),
            category: SettingsCategory::StatusHooks,
            has_override: o3,
            inherited_display: inherited_if(
                o3,
                FieldValue::OptionalText(global.status_hooks.on_running.clone()),
            ),
        },
        SettingField {
            key: FieldKey::StatusHookOnWaiting,
            label: "On Waiting",
            description: "Shell command run when a session enters Waiting",
            value: FieldValue::OptionalText(on_waiting),
            category: SettingsCategory::StatusHooks,
            has_override: o4,
            inherited_display: inherited_if(
                o4,
                FieldValue::OptionalText(global.status_hooks.on_waiting.clone()),
            ),
        },
        SettingField {
            key: FieldKey::StatusHookOnIdle,
            label: "On Idle",
            description: "Shell command run when a session enters Idle",
            value: FieldValue::OptionalText(on_idle),
            category: SettingsCategory::StatusHooks,
            has_override: o5,
            inherited_display: inherited_if(
                o5,
                FieldValue::OptionalText(global.status_hooks.on_idle.clone()),
            ),
        },
        SettingField {
            key: FieldKey::StatusHookOnError,
            label: "On Error",
            description: "Shell command run when a session enters Error",
            value: FieldValue::OptionalText(on_error),
            category: SettingsCategory::StatusHooks,
            has_override: o6,
            inherited_display: inherited_if(
                o6,
                FieldValue::OptionalText(global.status_hooks.on_error.clone()),
            ),
        },
        SettingField {
            key: FieldKey::StatusHookOnChange,
            label: "On Any Change",
            description:
                "Shell command run after the status-specific command on every status change",
            value: FieldValue::OptionalText(on_change),
            category: SettingsCategory::StatusHooks,
            has_override: o7,
            inherited_display: inherited_if(
                o7,
                FieldValue::OptionalText(global.status_hooks.on_change.clone()),
            ),
        },
    ]
}

/// Apply a field's value back to the appropriate config.
/// For profile scope, the value is always stored as an override.
pub fn apply_field_to_config(
    field: &SettingField,
    scope: SettingsScope,
    global: &mut Config,
    profile: &mut ProfileConfig,
) {
    match scope {
        SettingsScope::Global => apply_field_to_global(field, global),
        SettingsScope::Profile | SettingsScope::Repo => {
            apply_field_to_profile(field, global, profile)
        }
    }
}

fn apply_field_to_global(field: &SettingField, config: &mut Config) {
    match (&field.key, &field.value) {
        // ProfileDescription is profile-only; the field never appears in
        // Global scope, but match it so the fallthrough doesn't have to.
        (FieldKey::ProfileDescription, _) => {}
        // Theme
        (FieldKey::ThemeName, FieldValue::Select { selected, options }) => {
            config.theme.name = options.get(*selected).cloned().unwrap_or_default();
        }
        (FieldKey::ThemeColorMode, FieldValue::Select { selected, .. }) => {
            config.theme.color_mode = match selected {
                1 => crate::session::config::ColorMode::Palette,
                _ => crate::session::config::ColorMode::Truecolor,
            };
        }
        (FieldKey::IdleDecayMinutes, FieldValue::Number(v)) => {
            config.theme.idle_decay_minutes = *v;
        }
        // Updates
        (FieldKey::UpdateCheckMode, FieldValue::Select { selected, .. }) => {
            config.updates.update_check_mode = update_check_mode_from_index(*selected);
        }
        (FieldKey::CheckIntervalHours, FieldValue::Number(v)) => {
            config.updates.check_interval_hours = *v
        }
        (FieldKey::NotifyInCli, FieldValue::Bool(v)) => config.updates.notify_in_cli = *v,
        (FieldKey::WebPollIntervalMinutes, FieldValue::Number(v)) => {
            config.updates.web_poll_interval_minutes = *v
        }
        // Telemetry
        (FieldKey::TelemetryEnabled, FieldValue::Bool(v)) => config.telemetry.enabled = *v,
        // Worktree
        (FieldKey::WorktreeEnabled, FieldValue::Bool(v)) => config.worktree.enabled = *v,
        (FieldKey::PathTemplate, FieldValue::Text(v)) => config.worktree.path_template = v.clone(),
        (FieldKey::BareRepoPathTemplate, FieldValue::Text(v)) => {
            config.worktree.bare_repo_path_template = v.clone()
        }
        (FieldKey::WorktreeAutoCleanup, FieldValue::Bool(v)) => config.worktree.auto_cleanup = *v,
        (FieldKey::DeleteBranchOnCleanup, FieldValue::Bool(v)) => {
            config.worktree.delete_branch_on_cleanup = *v
        }
        (FieldKey::WorkspacePathTemplate, FieldValue::Text(v)) => {
            config.worktree.workspace_path_template = v.clone()
        }
        (FieldKey::InitSubmodules, FieldValue::Bool(v)) => config.worktree.init_submodules = *v,
        // Sandbox
        (FieldKey::SandboxEnabledByDefault, FieldValue::Bool(v)) => {
            config.sandbox.enabled_by_default = *v
        }
        (FieldKey::YoloModeDefault, FieldValue::Bool(v)) => config.session.yolo_mode_default = *v,
        (FieldKey::StrictHotkeys, FieldValue::Bool(v)) => config.session.strict_hotkeys = *v,
        (FieldKey::ConfirmBeforeQuit, FieldValue::Bool(v)) => {
            config.session.confirm_before_quit = *v
        }
        (FieldKey::SnoozeDurationMinutes, FieldValue::Number(v)) => {
            config.session.snooze_duration_minutes = *v as u32;
        }
        (FieldKey::SessionAutoStopIdleSecs, FieldValue::Number(v)) => {
            config.session.auto_stop_idle_secs = (*v).min(u32::MAX as u64) as u32;
        }
        (FieldKey::RestartWakeMessage, FieldValue::Text(v)) => {
            config.session.restart_wake_message = v.clone();
        }
        (FieldKey::LiveSendExitChord, FieldValue::Text(v)) => {
            config.session.live_send_exit_chord = v.clone();
        }
        (FieldKey::LiveSendLeader, FieldValue::Text(v)) => {
            config.session.live_send_leader = v.clone();
        }
        (FieldKey::NewSessionAttachMode, FieldValue::Select { selected, .. }) => {
            config.session.new_session_attach_mode = index_to_new_session_attach_mode(*selected);
        }
        (FieldKey::DefaultAttachMode, FieldValue::Select { selected, .. }) => {
            config.session.default_attach_mode = index_to_new_session_attach_mode(*selected);
        }
        (FieldKey::ClickAction, FieldValue::Select { selected, .. }) => {
            config.session.click_action = index_to_click_action(*selected);
        }
        (FieldKey::RowTag, FieldValue::Select { selected, .. }) => {
            config.session.row_tag = index_to_row_tag(*selected);
        }
        (FieldKey::AgentStatusHooks, FieldValue::Bool(v)) => {
            config.session.agent_status_hooks = *v;
        }
        (FieldKey::MouseCapture, FieldValue::Bool(v)) => {
            config.session.mouse_capture = *v;
        }
        (FieldKey::DefaultImage, FieldValue::Text(v)) => config.sandbox.default_image = v.clone(),
        (FieldKey::Environment, FieldValue::List(v)) => config.sandbox.environment = v.clone(),
        (FieldKey::ExtraVolumes, FieldValue::List(v)) => config.sandbox.extra_volumes = v.clone(),
        (FieldKey::PortMappings, FieldValue::List(v)) => config.sandbox.port_mappings = v.clone(),
        (FieldKey::VolumeIgnores, FieldValue::List(v)) => config.sandbox.volume_ignores = v.clone(),
        (FieldKey::MountSsh, FieldValue::Bool(v)) => config.sandbox.mount_ssh = *v,
        (FieldKey::SelinuxRelabel, FieldValue::Bool(v)) => config.sandbox.selinux_relabel = *v,
        (FieldKey::SandboxAutoCleanup, FieldValue::Bool(v)) => config.sandbox.auto_cleanup = *v,
        (FieldKey::CpuLimit, FieldValue::OptionalText(v)) => {
            config.sandbox.cpu_limit = v.clone();
        }
        (FieldKey::MemoryLimit, FieldValue::OptionalText(v)) => {
            config.sandbox.memory_limit = v.clone();
        }
        (FieldKey::CustomInstruction, FieldValue::OptionalText(v)) => {
            config.sandbox.custom_instruction = v.clone();
        }
        (FieldKey::DefaultTerminalMode, FieldValue::Select { selected, .. }) => {
            config.sandbox.default_terminal_mode = match selected {
                0 => DefaultTerminalMode::Host,
                _ => DefaultTerminalMode::Container,
            };
        }
        (FieldKey::ContainerRuntime, FieldValue::Select { selected, .. }) => {
            config.sandbox.container_runtime = match selected {
                0 => ContainerRuntimeName::Docker,
                1 => ContainerRuntimeName::Podman,
                _ => ContainerRuntimeName::AppleContainer,
            };
        }
        (FieldKey::VolumeIgnoresStrategy, FieldValue::Select { selected, .. }) => {
            config.sandbox.volume_ignores_strategy = match selected {
                1 => VolumeIgnoresStrategy::Named,
                _ => VolumeIgnoresStrategy::Anonymous,
            };
        }
        // Tmux
        (FieldKey::StatusBar, FieldValue::Select { selected, .. }) => {
            config.tmux.status_bar = match selected {
                0 => TmuxStatusBarMode::Auto,
                1 => TmuxStatusBarMode::Enabled,
                _ => TmuxStatusBarMode::Disabled,
            };
        }
        (FieldKey::Mouse, FieldValue::Select { selected, .. }) => {
            config.tmux.mouse = match selected {
                0 => TmuxMouseMode::Auto,
                1 => TmuxMouseMode::Enabled,
                _ => TmuxMouseMode::Disabled,
            };
        }
        (FieldKey::Clipboard, FieldValue::Select { selected, .. }) => {
            config.tmux.clipboard = match selected {
                0 => TmuxClipboardMode::Auto,
                1 => TmuxClipboardMode::Enabled,
                _ => TmuxClipboardMode::Disabled,
            };
        }
        // Session
        (FieldKey::DefaultTool, FieldValue::Select { selected, .. }) => {
            config.session.default_tool =
                crate::agents::name_from_settings_index(*selected).map(|s| s.to_string());
        }
        (FieldKey::AgentExtraArgs, FieldValue::List(v)) => {
            config.session.agent_extra_args = parse_key_value_list(v);
        }
        (FieldKey::AgentCommandOverride, FieldValue::List(v)) => {
            config.session.agent_command_override = parse_key_value_list(v);
        }
        (FieldKey::CustomAgents, FieldValue::List(v)) => {
            config.session.custom_agents = parse_key_value_list(v);
        }
        (FieldKey::AgentDetectAs, FieldValue::List(v)) => {
            config.session.agent_detect_as = parse_key_value_list(v);
        }
        (FieldKey::AgentCockpitCmd, FieldValue::List(v)) => {
            config.session.agent_cockpit_cmd = parse_key_value_list(v);
        }
        // Sound
        (FieldKey::SoundEnabled, FieldValue::Bool(v)) => config.sound.enabled = *v,
        (FieldKey::SoundMode, FieldValue::Select { selected, .. }) => {
            config.sound.mode = match selected {
                1 => SoundMode::Specific(String::new()),
                _ => SoundMode::Random,
            };
        }
        (FieldKey::SoundVolume, FieldValue::Select { selected, options }) => {
            if let Some(s) = options.get(*selected) {
                config.sound.volume = volume_from_option(s);
            }
        }
        (FieldKey::SoundOnStart, FieldValue::OptionalText(v)) => {
            config.sound.on_start = v.clone();
        }
        (FieldKey::SoundOnRunning, FieldValue::OptionalText(v)) => {
            config.sound.on_running = v.clone();
        }
        (FieldKey::SoundOnWaiting, FieldValue::OptionalText(v)) => {
            config.sound.on_waiting = v.clone();
        }
        (FieldKey::SoundOnIdle, FieldValue::OptionalText(v)) => {
            config.sound.on_idle = v.clone();
        }
        (FieldKey::SoundOnError, FieldValue::OptionalText(v)) => {
            config.sound.on_error = v.clone();
        }
        (FieldKey::SoundOnApproval, FieldValue::OptionalText(v)) => {
            config.sound.on_approval = v.clone();
        }
        // Status hooks
        (FieldKey::StatusHooksEnabled, FieldValue::Bool(v)) => {
            config.status_hooks.enabled = *v;
        }
        (FieldKey::StatusHookDebounceMs, FieldValue::Number(v)) => {
            config.status_hooks.debounce_ms = *v;
        }
        (FieldKey::StatusHookOnStarting, FieldValue::OptionalText(v)) => {
            config.status_hooks.on_starting = v.clone();
        }
        (FieldKey::StatusHookOnRunning, FieldValue::OptionalText(v)) => {
            config.status_hooks.on_running = v.clone();
        }
        (FieldKey::StatusHookOnWaiting, FieldValue::OptionalText(v)) => {
            config.status_hooks.on_waiting = v.clone();
        }
        (FieldKey::StatusHookOnIdle, FieldValue::OptionalText(v)) => {
            config.status_hooks.on_idle = v.clone();
        }
        (FieldKey::StatusHookOnError, FieldValue::OptionalText(v)) => {
            config.status_hooks.on_error = v.clone();
        }
        (FieldKey::StatusHookOnChange, FieldValue::OptionalText(v)) => {
            config.status_hooks.on_change = v.clone();
        }
        // Hooks
        (FieldKey::HookOnCreate, FieldValue::List(v)) => config.hooks.on_create = v.clone(),
        (FieldKey::HookOnLaunch, FieldValue::List(v)) => config.hooks.on_launch = v.clone(),
        (FieldKey::HookOnDestroy, FieldValue::List(v)) => config.hooks.on_destroy = v.clone(),
        // Web
        (FieldKey::WebNotificationsEnabled, FieldValue::Bool(v)) => {
            config.web.notifications_enabled = *v;
        }
        (FieldKey::WebNotifyOnWaiting, FieldValue::Bool(v)) => {
            config.web.notify_on_waiting = *v;
        }
        (FieldKey::WebNotifyOnIdle, FieldValue::Bool(v)) => {
            config.web.notify_on_idle = *v;
        }
        (FieldKey::WebNotifyOnWakeFire, FieldValue::Bool(v)) => {
            config.web.notify_on_wake_fire = *v;
        }
        (FieldKey::WebNotifyOnError, FieldValue::Bool(v)) => {
            config.web.notify_on_error = *v;
        }
        // Diff
        (FieldKey::DiffSplitView, FieldValue::Bool(v)) => config.diff.split_view = *v,
        // Cockpit
        (FieldKey::CockpitEnabled, FieldValue::Bool(v)) => config.cockpit.enabled = *v,
        (FieldKey::CockpitDefaultForClaude, FieldValue::Bool(v)) => {
            config.cockpit.default_for_claude = *v
        }
        (FieldKey::CockpitDefaultAgent, FieldValue::Text(v)) => {
            config.cockpit.default_agent = v.clone()
        }
        (FieldKey::CockpitMaxConcurrentWorkers, FieldValue::Number(v)) => {
            config.cockpit.max_concurrent_workers = (*v).max(1) as u32
        }
        (FieldKey::CockpitReplayEvents, FieldValue::Number(v)) => {
            // 0 = unlimited history; clamp non-zero values to u32 range.
            // See #1065.
            config.cockpit.replay_events = (*v).min(u32::MAX as u64) as u32
        }
        (FieldKey::CockpitReplayBytes, FieldValue::Number(v)) => {
            config.cockpit.replay_bytes = (*v).max(1024)
        }
        (FieldKey::CockpitNodePath, FieldValue::Text(v)) => config.cockpit.node_path = v.clone(),
        (FieldKey::CockpitShowToolDurations, FieldValue::Bool(v)) => {
            config.cockpit.show_tool_durations = *v
        }
        (FieldKey::CockpitQueueDrainMode, FieldValue::Select { selected, options }) => {
            if let Some(name) = options.get(*selected) {
                if let Some(mode) = crate::session::config::QueueDrainMode::parse(name) {
                    config.cockpit.queue_drain_mode = mode;
                }
            }
        }
        (FieldKey::CockpitMaxConcurrentResumes, FieldValue::Number(v)) => {
            config.cockpit.max_concurrent_resumes = (*v).max(1).min(u32::MAX as u64) as u32
        }
        (FieldKey::CockpitForceEndTurnThresholdSecs, FieldValue::Number(v)) => {
            config.cockpit.force_end_turn_threshold_secs = (*v).max(1).min(u32::MAX as u64) as u32
        }
        (FieldKey::CockpitSilentOrphanGraceSecs, FieldValue::Number(v)) => {
            // 0 = disabled; anything 1..=9 clamps to 10 so a typo can't
            // produce an absurdly tight grace that false-positives on
            // healthy turns.
            let raw = (*v).min(u32::MAX as u64) as u32;
            config.cockpit.silent_orphan_grace_secs = if raw == 0 { 0 } else { raw.max(10) };
        }
        (FieldKey::CockpitSilentOrphanFastGraceSecs, FieldValue::Number(v)) => {
            // 0 = disable the accelerator: cost-populated UsageUpdate
            // no longer reduces the watchdog's effective grace. Anything
            // 1..=4 clamps up to 5 so a typo can't produce an absurdly
            // tight fast grace.
            let raw = (*v).min(u32::MAX as u64) as u32;
            config.cockpit.silent_orphan_fast_grace_secs = if raw == 0 { 0 } else { raw.max(5) };
        }
        (FieldKey::CockpitAutoStopIdleSecs, FieldValue::Number(v)) => {
            config.cockpit.auto_stop_idle_secs = (*v).min(u32::MAX as u64) as u32;
        }
        // Logging
        (FieldKey::LoggingDefaultLevel, FieldValue::Select { selected, options }) => {
            if let Some(level) = options.get(*selected) {
                config.logging.default_level = level.clone();
            }
        }
        (FieldKey::LoggingTarget(idx), FieldValue::Select { selected, options }) => {
            if let Some(target) = crate::logging::KNOWN_SUB_TARGETS.get(*idx as usize) {
                let level = options.get(*selected).cloned().unwrap_or_default();
                if level.is_empty() || level == "(default)" {
                    config.logging.targets.remove(*target);
                } else {
                    config.logging.targets.insert(target.to_string(), level);
                }
            }
        }
        (FieldKey::LoggingOutput, FieldValue::Select { selected, options }) => {
            if let Some(value) = options.get(*selected) {
                config.logging.output = match value.as_str() {
                    "stdout" => crate::session::config::SinkKind::Stdout,
                    _ => crate::session::config::SinkKind::File,
                };
            }
        }
        (FieldKey::LoggingFilePath, FieldValue::Text(v)) => {
            let trimmed = v.trim();
            config.logging.file_path = if trimmed.is_empty() {
                "debug.log".to_string()
            } else {
                trimmed.to_string()
            };
        }
        (FieldKey::LoggingRotation, FieldValue::Select { selected, options }) => {
            if let Some(value) = options.get(*selected) {
                config.logging.rotation = match value.as_str() {
                    "never" => crate::session::config::RotationKind::Never,
                    _ => crate::session::config::RotationKind::Size,
                };
            }
        }
        (FieldKey::LoggingMaxSizeMib, FieldValue::Number(v)) => {
            config.logging.max_size_mib = (*v).max(1);
        }
        (FieldKey::LoggingKeepCount, FieldValue::Number(v)) => {
            config.logging.keep_count = (*v).clamp(1, u8::MAX as u64) as u8;
        }
        (FieldKey::LoggingShowSpans, FieldValue::Bool(v)) => {
            config.logging.show_spans = *v;
        }
        (FieldKey::HostEnvironment, FieldValue::List(v)) => config.environment = v.clone(),
        (FieldKey::SessionIdPollerMaxThreads, FieldValue::Number(v)) => {
            config.session.session_id_poller_max_threads = (*v).clamp(1, u32::MAX as u64) as u32;
        }
        _ => {}
    }
}

/// Apply a field to the profile config.
/// Always stores the value as an override; use 'r' key to clear overrides.
fn apply_field_to_profile(field: &SettingField, _global: &Config, config: &mut ProfileConfig) {
    match (&field.key, &field.value) {
        // Profile description: empty input clears the field, otherwise we
        // store the trimmed string so a stray space doesn't promote the
        // profile to "has overrides".
        (FieldKey::ProfileDescription, FieldValue::OptionalText(v)) => {
            config.description = v
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        }
        // Theme
        (FieldKey::ThemeName, FieldValue::Select { selected, options }) => {
            let name = options.get(*selected).cloned().unwrap_or_default();
            use crate::session::ThemeConfigOverride;
            let t = config
                .theme
                .get_or_insert_with(ThemeConfigOverride::default);
            t.name = Some(name);
        }
        (FieldKey::ThemeColorMode, FieldValue::Select { selected, .. }) => {
            use crate::session::ThemeConfigOverride;
            let t = config
                .theme
                .get_or_insert_with(ThemeConfigOverride::default);
            t.color_mode = Some(match selected {
                1 => crate::session::config::ColorMode::Palette,
                _ => crate::session::config::ColorMode::Truecolor,
            });
        }
        (FieldKey::IdleDecayMinutes, FieldValue::Number(v)) => {
            use crate::session::ThemeConfigOverride;
            let t = config
                .theme
                .get_or_insert_with(ThemeConfigOverride::default);
            t.idle_decay_minutes = Some(*v);
        }
        // Updates
        (FieldKey::UpdateCheckMode, FieldValue::Select { selected, .. }) => {
            let mode = update_check_mode_from_index(*selected);
            set_profile_override(mode, &mut config.updates, |s, val| {
                s.update_check_mode = val
            });
        }
        (FieldKey::CheckIntervalHours, FieldValue::Number(v)) => {
            set_profile_override(*v, &mut config.updates, |s, val| {
                s.check_interval_hours = val
            });
        }
        (FieldKey::NotifyInCli, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.updates, |s, val| s.notify_in_cli = val);
        }
        (FieldKey::WebPollIntervalMinutes, FieldValue::Number(v)) => {
            set_profile_override(*v, &mut config.updates, |s, val| {
                s.web_poll_interval_minutes = val
            });
        }
        // Worktree
        (FieldKey::WorktreeEnabled, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.worktree, |s, val| s.enabled = val);
        }
        (FieldKey::PathTemplate, FieldValue::Text(v)) => {
            set_profile_override(v.clone(), &mut config.worktree, |s, val| {
                s.path_template = val
            });
        }
        (FieldKey::BareRepoPathTemplate, FieldValue::Text(v)) => {
            set_profile_override(v.clone(), &mut config.worktree, |s, val| {
                s.bare_repo_path_template = val
            });
        }
        (FieldKey::WorktreeAutoCleanup, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.worktree, |s, val| s.auto_cleanup = val);
        }
        (FieldKey::DeleteBranchOnCleanup, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.worktree, |s, val| {
                s.delete_branch_on_cleanup = val
            });
        }
        (FieldKey::WorkspacePathTemplate, FieldValue::Text(v)) => {
            set_profile_override(v.clone(), &mut config.worktree, |s, val| {
                s.workspace_path_template = val
            });
        }
        (FieldKey::InitSubmodules, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.worktree, |s, val| s.init_submodules = val);
        }
        // Diff
        (FieldKey::DiffSplitView, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.diff, |d, val| d.split_view = val);
        }
        // Sandbox
        (FieldKey::SandboxEnabledByDefault, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.sandbox, |s, val| s.enabled_by_default = val);
        }
        (FieldKey::DefaultImage, FieldValue::Text(v)) => {
            set_profile_override(v.clone(), &mut config.sandbox, |s, val| {
                s.default_image = val
            });
        }
        (FieldKey::Environment, FieldValue::List(v)) => {
            set_profile_override(v.clone(), &mut config.sandbox, |s, val| s.environment = val);
        }
        (FieldKey::ExtraVolumes, FieldValue::List(v)) => {
            set_profile_override(v.clone(), &mut config.sandbox, |s, val| {
                s.extra_volumes = val
            });
        }
        (FieldKey::PortMappings, FieldValue::List(v)) => {
            set_profile_override(v.clone(), &mut config.sandbox, |s, val| {
                s.port_mappings = val
            });
        }
        (FieldKey::VolumeIgnores, FieldValue::List(v)) => {
            set_profile_override(v.clone(), &mut config.sandbox, |s, val| {
                s.volume_ignores = val
            });
        }
        (FieldKey::MountSsh, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.sandbox, |s, val| s.mount_ssh = val);
        }
        (FieldKey::SelinuxRelabel, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.sandbox, |s, val| s.selinux_relabel = val);
        }
        (FieldKey::SandboxAutoCleanup, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.sandbox, |s, val| s.auto_cleanup = val);
        }
        (FieldKey::CpuLimit, FieldValue::OptionalText(v)) => {
            use crate::session::SandboxConfigOverride;
            let s = config
                .sandbox
                .get_or_insert_with(SandboxConfigOverride::default);
            s.cpu_limit = v.clone();
        }
        (FieldKey::MemoryLimit, FieldValue::OptionalText(v)) => {
            use crate::session::SandboxConfigOverride;
            let s = config
                .sandbox
                .get_or_insert_with(SandboxConfigOverride::default);
            s.memory_limit = v.clone();
        }
        (FieldKey::CustomInstruction, FieldValue::OptionalText(v)) => {
            use crate::session::SandboxConfigOverride;
            let s = config
                .sandbox
                .get_or_insert_with(SandboxConfigOverride::default);
            s.custom_instruction = v.clone();
        }
        (FieldKey::DefaultTerminalMode, FieldValue::Select { selected, .. }) => {
            let mode = match selected {
                0 => DefaultTerminalMode::Host,
                _ => DefaultTerminalMode::Container,
            };
            set_profile_override(mode, &mut config.sandbox, |s, val| {
                s.default_terminal_mode = val
            });
        }
        (FieldKey::ContainerRuntime, FieldValue::Select { selected, .. }) => {
            let runtime = match selected {
                0 => ContainerRuntimeName::Docker,
                1 => ContainerRuntimeName::Podman,
                _ => ContainerRuntimeName::AppleContainer,
            };
            set_profile_override(runtime, &mut config.sandbox, |s, val| {
                s.container_runtime = val
            });
        }
        (FieldKey::VolumeIgnoresStrategy, FieldValue::Select { selected, .. }) => {
            let strategy = match selected {
                1 => VolumeIgnoresStrategy::Named,
                _ => VolumeIgnoresStrategy::Anonymous,
            };
            set_profile_override(strategy, &mut config.sandbox, |s, val| {
                s.volume_ignores_strategy = val
            });
        }
        // Tmux
        (FieldKey::StatusBar, FieldValue::Select { selected, .. }) => {
            let mode = match selected {
                0 => TmuxStatusBarMode::Auto,
                1 => TmuxStatusBarMode::Enabled,
                _ => TmuxStatusBarMode::Disabled,
            };
            set_profile_override(mode, &mut config.tmux, |s, val| s.status_bar = val);
        }
        (FieldKey::Mouse, FieldValue::Select { selected, .. }) => {
            let mode = match selected {
                0 => TmuxMouseMode::Auto,
                1 => TmuxMouseMode::Enabled,
                _ => TmuxMouseMode::Disabled,
            };
            set_profile_override(mode, &mut config.tmux, |s, val| s.mouse = val);
        }
        (FieldKey::Clipboard, FieldValue::Select { selected, .. }) => {
            let mode = match selected {
                0 => TmuxClipboardMode::Auto,
                1 => TmuxClipboardMode::Enabled,
                _ => TmuxClipboardMode::Disabled,
            };
            set_profile_override(mode, &mut config.tmux, |s, val| s.clipboard = val);
        }
        // Session
        (FieldKey::DefaultTool, FieldValue::Select { selected, .. }) => {
            let tool = crate::agents::name_from_settings_index(*selected).map(|s| s.to_string());
            use crate::session::SessionConfigOverride;
            let session = config
                .session
                .get_or_insert_with(SessionConfigOverride::default);
            session.default_tool = tool;
        }
        (FieldKey::YoloModeDefault, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.session, |s, val| s.yolo_mode_default = val);
        }
        (FieldKey::StrictHotkeys, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.session, |s, val| s.strict_hotkeys = val);
        }
        (FieldKey::SnoozeDurationMinutes, FieldValue::Number(v)) => {
            set_profile_override(*v as u32, &mut config.session, |s, val| {
                s.snooze_duration_minutes = val
            });
        }
        (FieldKey::SessionAutoStopIdleSecs, FieldValue::Number(v)) => {
            set_profile_override(
                (*v).min(u32::MAX as u64) as u32,
                &mut config.session,
                |s, val| s.auto_stop_idle_secs = val,
            );
        }
        (FieldKey::RestartWakeMessage, FieldValue::Text(v)) => {
            set_profile_override(v.clone(), &mut config.session, |s, val| {
                s.restart_wake_message = val
            });
        }
        (FieldKey::LiveSendExitChord, FieldValue::Text(v)) => {
            set_profile_override(v.clone(), &mut config.session, |s, val| {
                s.live_send_exit_chord = val
            });
        }
        (FieldKey::LiveSendLeader, FieldValue::Text(v)) => {
            set_profile_override(v.clone(), &mut config.session, |s, val| {
                s.live_send_leader = val
            });
        }
        (FieldKey::NewSessionAttachMode, FieldValue::Select { selected, .. }) => {
            set_profile_override(
                index_to_new_session_attach_mode(*selected),
                &mut config.session,
                |s, val| s.new_session_attach_mode = val,
            );
        }
        (FieldKey::DefaultAttachMode, FieldValue::Select { selected, .. }) => {
            set_profile_override(
                index_to_new_session_attach_mode(*selected),
                &mut config.session,
                |s, val| s.default_attach_mode = val,
            );
        }
        (FieldKey::ClickAction, FieldValue::Select { selected, .. }) => {
            set_profile_override(
                index_to_click_action(*selected),
                &mut config.session,
                |s, val| s.click_action = val,
            );
        }
        (FieldKey::RowTag, FieldValue::Select { selected, .. }) => {
            set_profile_override(
                index_to_row_tag(*selected),
                &mut config.session,
                |s, val| s.row_tag = val,
            );
        }
        (FieldKey::AgentStatusHooks, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.session, |s, val| {
                s.agent_status_hooks = val;
            });
        }
        (FieldKey::MouseCapture, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.session, |s, val| {
                s.mouse_capture = val;
            });
        }
        (FieldKey::AgentExtraArgs, FieldValue::List(v)) => {
            let map = parse_key_value_list(v);
            use crate::session::SessionConfigOverride;
            let s = config
                .session
                .get_or_insert_with(SessionConfigOverride::default);
            s.agent_extra_args = Some(map);
        }
        (FieldKey::AgentCommandOverride, FieldValue::List(v)) => {
            let map = parse_key_value_list(v);
            use crate::session::SessionConfigOverride;
            let s = config
                .session
                .get_or_insert_with(SessionConfigOverride::default);
            s.agent_command_override = Some(map);
        }
        (FieldKey::CustomAgents, FieldValue::List(v)) => {
            let map = parse_key_value_list(v);
            use crate::session::SessionConfigOverride;
            let s = config
                .session
                .get_or_insert_with(SessionConfigOverride::default);
            s.custom_agents = Some(map);
        }
        (FieldKey::AgentDetectAs, FieldValue::List(v)) => {
            let map = parse_key_value_list(v);
            use crate::session::SessionConfigOverride;
            let s = config
                .session
                .get_or_insert_with(SessionConfigOverride::default);
            s.agent_detect_as = Some(map);
        }
        (FieldKey::AgentCockpitCmd, FieldValue::List(v)) => {
            let map = parse_key_value_list(v);
            use crate::session::SessionConfigOverride;
            let s = config
                .session
                .get_or_insert_with(SessionConfigOverride::default);
            s.agent_cockpit_cmd = Some(map);
        }
        // Sound
        (FieldKey::SoundEnabled, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.sound, |s, val| s.enabled = val);
        }
        (FieldKey::SoundMode, FieldValue::Select { selected, .. }) => {
            let mode = match selected {
                1 => SoundMode::Specific(String::new()),
                _ => SoundMode::Random,
            };
            set_profile_override(mode, &mut config.sound, |s, val| s.mode = val);
        }
        (FieldKey::SoundVolume, FieldValue::Select { selected, options }) => {
            if let Some(s) = options.get(*selected) {
                let vol = volume_from_option(s);
                set_profile_override(vol, &mut config.sound, |s, val| s.volume = val);
            }
        }
        (FieldKey::SoundOnStart, FieldValue::OptionalText(v)) => {
            let s = config
                .sound
                .get_or_insert_with(crate::sound::SoundConfigOverride::default);
            s.on_start = v.clone();
        }
        (FieldKey::SoundOnRunning, FieldValue::OptionalText(v)) => {
            let s = config
                .sound
                .get_or_insert_with(crate::sound::SoundConfigOverride::default);
            s.on_running = v.clone();
        }
        (FieldKey::SoundOnWaiting, FieldValue::OptionalText(v)) => {
            let s = config
                .sound
                .get_or_insert_with(crate::sound::SoundConfigOverride::default);
            s.on_waiting = v.clone();
        }
        (FieldKey::SoundOnIdle, FieldValue::OptionalText(v)) => {
            let s = config
                .sound
                .get_or_insert_with(crate::sound::SoundConfigOverride::default);
            s.on_idle = v.clone();
        }
        (FieldKey::SoundOnError, FieldValue::OptionalText(v)) => {
            let s = config
                .sound
                .get_or_insert_with(crate::sound::SoundConfigOverride::default);
            s.on_error = v.clone();
        }
        (FieldKey::SoundOnApproval, FieldValue::OptionalText(v)) => {
            let s = config
                .sound
                .get_or_insert_with(crate::sound::SoundConfigOverride::default);
            s.on_approval = v.clone();
        }
        // Status hooks
        (FieldKey::StatusHooksEnabled, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.status_hooks, |s, val| s.enabled = val);
        }
        (FieldKey::StatusHookDebounceMs, FieldValue::Number(v)) => {
            set_profile_override(*v, &mut config.status_hooks, |s, val| s.debounce_ms = val);
        }
        (FieldKey::StatusHookOnStarting, FieldValue::OptionalText(v)) => {
            let s = config
                .status_hooks
                .get_or_insert_with(crate::status_hooks::StatusHookConfigOverride::default);
            s.on_starting = v.clone();
        }
        (FieldKey::StatusHookOnRunning, FieldValue::OptionalText(v)) => {
            let s = config
                .status_hooks
                .get_or_insert_with(crate::status_hooks::StatusHookConfigOverride::default);
            s.on_running = v.clone();
        }
        (FieldKey::StatusHookOnWaiting, FieldValue::OptionalText(v)) => {
            let s = config
                .status_hooks
                .get_or_insert_with(crate::status_hooks::StatusHookConfigOverride::default);
            s.on_waiting = v.clone();
        }
        (FieldKey::StatusHookOnIdle, FieldValue::OptionalText(v)) => {
            let s = config
                .status_hooks
                .get_or_insert_with(crate::status_hooks::StatusHookConfigOverride::default);
            s.on_idle = v.clone();
        }
        (FieldKey::StatusHookOnError, FieldValue::OptionalText(v)) => {
            let s = config
                .status_hooks
                .get_or_insert_with(crate::status_hooks::StatusHookConfigOverride::default);
            s.on_error = v.clone();
        }
        (FieldKey::StatusHookOnChange, FieldValue::OptionalText(v)) => {
            let s = config
                .status_hooks
                .get_or_insert_with(crate::status_hooks::StatusHookConfigOverride::default);
            s.on_change = v.clone();
        }
        // Hooks
        (FieldKey::HookOnCreate, FieldValue::List(v)) => {
            set_profile_override(v.clone(), &mut config.hooks, |s, val| s.on_create = val);
        }
        (FieldKey::HookOnLaunch, FieldValue::List(v)) => {
            set_profile_override(v.clone(), &mut config.hooks, |s, val| s.on_launch = val);
        }
        (FieldKey::HookOnDestroy, FieldValue::List(v)) => {
            set_profile_override(v.clone(), &mut config.hooks, |s, val| s.on_destroy = val);
        }
        // Cockpit
        (FieldKey::CockpitEnabled, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.cockpit, |s, val| s.enabled = val);
        }
        (FieldKey::CockpitDefaultForClaude, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.cockpit, |s, val| s.default_for_claude = val);
        }
        (FieldKey::CockpitDefaultAgent, FieldValue::Text(v)) => {
            set_profile_override(v.clone(), &mut config.cockpit, |s, val| {
                s.default_agent = val
            });
        }
        (FieldKey::CockpitMaxConcurrentWorkers, FieldValue::Number(v)) => {
            set_profile_override((*v).max(1) as u32, &mut config.cockpit, |s, val| {
                s.max_concurrent_workers = val
            });
        }
        (FieldKey::CockpitReplayEvents, FieldValue::Number(v)) => {
            // 0 = unlimited; #1065.
            set_profile_override(
                (*v).min(u32::MAX as u64) as u32,
                &mut config.cockpit,
                |s, val| s.replay_events = val,
            );
        }
        (FieldKey::CockpitReplayBytes, FieldValue::Number(v)) => {
            set_profile_override((*v).max(1024), &mut config.cockpit, |s, val| {
                s.replay_bytes = val
            });
        }
        (FieldKey::CockpitNodePath, FieldValue::Text(v)) => {
            set_profile_override(v.clone(), &mut config.cockpit, |s, val| s.node_path = val);
        }
        (FieldKey::CockpitShowToolDurations, FieldValue::Bool(v)) => {
            set_profile_override(*v, &mut config.cockpit, |s, val| {
                s.show_tool_durations = val
            });
        }
        (FieldKey::CockpitQueueDrainMode, FieldValue::Select { selected, options }) => {
            if let Some(name) = options.get(*selected) {
                if let Some(mode) = crate::session::config::QueueDrainMode::parse(name) {
                    set_profile_override(mode, &mut config.cockpit, |s, val| {
                        s.queue_drain_mode = val
                    });
                }
            }
        }
        (FieldKey::CockpitMaxConcurrentResumes, FieldValue::Number(v)) => {
            let clamped = (*v).max(1).min(u32::MAX as u64) as u32;
            set_profile_override(clamped, &mut config.cockpit, |s, val| {
                s.max_concurrent_resumes = val
            });
        }
        (FieldKey::CockpitForceEndTurnThresholdSecs, FieldValue::Number(v)) => {
            let clamped = (*v).max(1).min(u32::MAX as u64) as u32;
            set_profile_override(clamped, &mut config.cockpit, |s, val| {
                s.force_end_turn_threshold_secs = val
            });
        }
        (FieldKey::CockpitSilentOrphanGraceSecs, FieldValue::Number(v)) => {
            let raw = (*v).min(u32::MAX as u64) as u32;
            let clamped = if raw == 0 { 0 } else { raw.max(10) };
            set_profile_override(clamped, &mut config.cockpit, |s, val| {
                s.silent_orphan_grace_secs = val
            });
        }
        (FieldKey::CockpitSilentOrphanFastGraceSecs, FieldValue::Number(v)) => {
            let raw = (*v).min(u32::MAX as u64) as u32;
            let clamped = if raw == 0 { 0 } else { raw.max(5) };
            set_profile_override(clamped, &mut config.cockpit, |s, val| {
                s.silent_orphan_fast_grace_secs = val
            });
        }
        (FieldKey::CockpitAutoStopIdleSecs, FieldValue::Number(v)) => {
            let clamped = (*v).min(u32::MAX as u64) as u32;
            set_profile_override(clamped, &mut config.cockpit, |s, val| {
                s.auto_stop_idle_secs = val
            });
        }
        (FieldKey::HostEnvironment, FieldValue::List(v)) => {
            // Empty list clears the override (no env entries); otherwise store
            // the list as the profile-scope replacement of the global list.
            config.environment = if v.is_empty() { None } else { Some(v.clone()) };
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Config, ProfileConfig};

    #[test]
    fn test_cockpit_auto_stop_idle_secs_applies_to_global_and_profile() {
        let mut global = Config::default();
        let mut profile = ProfileConfig::default();

        // Pull the real field from the builder so the test tracks the
        // production definition (label/category), then pin the value.
        let mut field = build_fields_for_category(
            SettingsCategory::Cockpit,
            SettingsScope::Global,
            &global,
            &profile,
        )
        .into_iter()
        .find(|f| f.key == FieldKey::CockpitAutoStopIdleSecs)
        .expect("CockpitAutoStopIdleSecs field must exist in the Cockpit category");
        field.value = FieldValue::Number(28800);

        apply_field_to_config(&field, SettingsScope::Global, &mut global, &mut profile);
        assert_eq!(global.cockpit.auto_stop_idle_secs, 28800);

        apply_field_to_config(&field, SettingsScope::Profile, &mut global, &mut profile);
        assert_eq!(
            profile.cockpit.as_ref().and_then(|c| c.auto_stop_idle_secs),
            Some(28800),
            "profile scope must store the value as an override"
        );
    }

    #[test]
    fn test_profile_field_has_no_override_after_global_change() {
        use crate::session::config::UpdateCheckMode;
        // Start with default configs
        let mut global = Config::default();
        let profile = ProfileConfig::default();

        // Verify initial state - profile shows no override
        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &profile,
        );

        let mode_field = fields
            .iter()
            .find(|f| f.key == FieldKey::UpdateCheckMode)
            .unwrap();
        assert!(
            !mode_field.has_override,
            "Profile should not show override initially"
        );

        // Change global setting
        global.updates.update_check_mode = UpdateCheckMode::Off;

        // Rebuild profile fields - should still show no override
        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &profile,
        );

        let mode_field = fields
            .iter()
            .find(|f| f.key == FieldKey::UpdateCheckMode)
            .unwrap();
        assert!(
            !mode_field.has_override,
            "Profile should NOT show override after global change - it should inherit"
        );
    }

    #[test]
    fn test_profile_field_shows_override_after_profile_change() {
        use crate::session::config::UpdateCheckMode;
        let global = Config::default();
        let mut profile = ProfileConfig::default();

        // Initially no override
        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        let mode_field = fields
            .iter()
            .find(|f| f.key == FieldKey::UpdateCheckMode)
            .unwrap();
        assert!(!mode_field.has_override);

        // Set a profile override
        profile.updates = Some(crate::session::UpdatesConfigOverride {
            update_check_mode: Some(UpdateCheckMode::Off),
            ..Default::default()
        });

        // Rebuild - should now show override
        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        let mode_field = fields
            .iter()
            .find(|f| f.key == FieldKey::UpdateCheckMode)
            .unwrap();
        assert!(
            mode_field.has_override,
            "Profile SHOULD show override after explicit profile change"
        );
    }

    #[test]
    fn test_default_tool_options_include_all_registered_agents() {
        let global = Config::default();
        let profile = ProfileConfig::default();

        let fields = build_fields_for_category(
            SettingsCategory::Agents,
            SettingsScope::Global,
            &global,
            &profile,
        );

        let tool_field = fields
            .iter()
            .find(|f| f.key == FieldKey::DefaultTool)
            .expect("DefaultTool field should exist");

        let options = match &tool_field.value {
            FieldValue::Select { options, .. } => options,
            _ => panic!("DefaultTool should be a Select field"),
        };

        let tool_options: Vec<&str> = options.iter().skip(1).map(|s| s.as_str()).collect();
        let agent_names = crate::agents::agent_names();

        for name in &agent_names {
            assert!(
                tool_options.contains(name),
                "Settings UI missing agent '{}'. UI options: {:?}",
                name,
                tool_options
            );
        }

        for option in &tool_options {
            assert!(
                agent_names.contains(option),
                "Settings UI has unknown agent '{}' not in registry.",
                option
            );
        }
    }

    #[test]
    fn test_profile_override_preserved_when_matching_global() {
        let global = Config::default();
        let mut profile = ProfileConfig::default();

        // Set a profile override that matches the global value
        let global_mode = global.updates.update_check_mode;
        profile.updates = Some(crate::session::UpdatesConfigOverride {
            update_check_mode: Some(global_mode),
            ..Default::default()
        });

        // Apply the same value through the field system
        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        let field = fields
            .iter()
            .find(|f| f.key == FieldKey::UpdateCheckMode)
            .unwrap();

        // Re-apply the field (simulates user saving without changing the value)
        apply_field_to_profile(field, &global, &mut profile);

        // The override should still be present
        assert!(
            profile
                .updates
                .as_ref()
                .and_then(|u| u.update_check_mode)
                .is_some(),
            "Profile override should be preserved even when value matches global"
        );
    }

    #[test]
    fn test_select_toggle_back_to_global_preserves_override() {
        use crate::session::config::UpdateCheckMode;
        let global = Config::default();
        let mut profile = ProfileConfig::default();
        let original = global.updates.update_check_mode;

        // Toggle to non-global value (Off when default is Notify)
        let other = if original == UpdateCheckMode::Off {
            UpdateCheckMode::Notify
        } else {
            UpdateCheckMode::Off
        };
        profile.updates = Some(crate::session::UpdatesConfigOverride {
            update_check_mode: Some(other),
            ..Default::default()
        });

        // Now toggle back to match global
        let field = SettingField {
            key: FieldKey::UpdateCheckMode,
            label: "Update Check Mode",
            description: "",
            value: FieldValue::Select {
                selected: update_check_mode_to_index(original),
                options: vec!["auto".to_string(), "notify".to_string(), "off".to_string()],
            },
            category: SettingsCategory::Updates,
            has_override: true,
            inherited_display: None,
        };

        apply_field_to_profile(&field, &global, &mut profile);

        // Override should still be present (not silently cleared)
        assert!(
            profile
                .updates
                .as_ref()
                .and_then(|u| u.update_check_mode)
                .is_some(),
            "Toggling back to match global should preserve the override, not silently clear it"
        );
        assert_eq!(
            profile.updates.as_ref().unwrap().update_check_mode,
            Some(original),
            "Override value should match what was set"
        );
    }

    #[test]
    fn test_worktree_enabled_field_uses_existing_config_value() {
        let mut global = Config::default();
        global.worktree.enabled = true;
        let profile = ProfileConfig::default();

        let fields = build_fields_for_category(
            SettingsCategory::Worktree,
            SettingsScope::Global,
            &global,
            &profile,
        );
        let field = fields
            .iter()
            .find(|f| f.key == FieldKey::WorktreeEnabled)
            .unwrap();

        assert_eq!(field.label, "Enabled by Default");
        assert!(matches!(field.value, FieldValue::Bool(true)));
    }

    #[test]
    fn test_worktree_enabled_profile_override() {
        let global = Config::default();
        let profile = ProfileConfig {
            worktree: Some(crate::session::WorktreeConfigOverride {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };

        let fields = build_fields_for_category(
            SettingsCategory::Worktree,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        let field = fields
            .iter()
            .find(|f| f.key == FieldKey::WorktreeEnabled)
            .unwrap();

        assert!(field.has_override);
        assert!(matches!(field.value, FieldValue::Bool(true)));
    }

    #[test]
    fn test_diff_split_view_applies_to_global() {
        let mut global = Config::default();
        assert!(!global.diff.split_view);

        let field = SettingField {
            key: FieldKey::DiffSplitView,
            label: "Side-by-side diff",
            description: "",
            value: FieldValue::Bool(true),
            category: SettingsCategory::Diff,
            has_override: false,
            inherited_display: None,
        };
        apply_field_to_global(&field, &mut global);

        assert!(global.diff.split_view);
    }

    #[test]
    fn test_status_hook_debounce_field_uses_default() {
        let global = Config::default();
        let profile = ProfileConfig::default();

        let fields = build_fields_for_category(
            SettingsCategory::StatusHooks,
            SettingsScope::Global,
            &global,
            &profile,
        );
        let field = fields
            .iter()
            .find(|f| f.key == FieldKey::StatusHookDebounceMs)
            .expect("StatusHookDebounceMs field should exist");

        assert_eq!(field.label, "Debounce (ms)");
        assert!(matches!(field.value, FieldValue::Number(100)));
    }

    #[test]
    fn test_status_hook_debounce_profile_override() {
        let global = Config::default();
        let profile = ProfileConfig {
            status_hooks: Some(crate::status_hooks::StatusHookConfigOverride {
                debounce_ms: Some(500),
                ..Default::default()
            }),
            ..Default::default()
        };

        let fields = build_fields_for_category(
            SettingsCategory::StatusHooks,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        let field = fields
            .iter()
            .find(|f| f.key == FieldKey::StatusHookDebounceMs)
            .expect("StatusHookDebounceMs field should exist");

        assert!(field.has_override);
        assert!(matches!(field.value, FieldValue::Number(500)));
        assert_eq!(field.inherited_display.as_deref(), Some("100"));
    }

    #[test]
    fn test_status_hook_debounce_apply_global_and_profile() {
        let mut global = Config::default();
        let mut profile = ProfileConfig::default();
        let field = SettingField {
            key: FieldKey::StatusHookDebounceMs,
            label: "Debounce (ms)",
            description: "",
            value: FieldValue::Number(250),
            category: SettingsCategory::StatusHooks,
            has_override: false,
            inherited_display: None,
        };

        apply_field_to_config(&field, SettingsScope::Global, &mut global, &mut profile);
        assert_eq!(global.status_hooks.debounce_ms, 250);

        apply_field_to_config(&field, SettingsScope::Profile, &mut global, &mut profile);
        assert_eq!(
            profile.status_hooks.as_ref().and_then(|s| s.debounce_ms),
            Some(250)
        );
    }

    /// The Cockpit tab inserts a "Advanced" section header between
    /// common settings and operational tuning fields. This test pins
    /// (1) the header is present, (2) the common keys are before it,
    /// and (3) the tuning keys are after it. The exact ordering
    /// matters because the renderer puts the visual divider at the
    /// header's position; if the split drifts, users see headings in
    /// the wrong place rather than a clean common-then-advanced split.
    #[test]
    fn cockpit_fields_have_advanced_section_marker() {
        let global = Config::default();
        let profile = ProfileConfig::default();
        let fields = build_fields_for_category(
            SettingsCategory::Cockpit,
            SettingsScope::Global,
            &global,
            &profile,
        );
        let header_idx = fields
            .iter()
            .position(|f| matches!(f.value, FieldValue::SectionHeader))
            .expect("Cockpit should contain an 'Advanced' section header");
        let header = &fields[header_idx];
        assert_eq!(header.label, "Advanced");
        assert_eq!(header.key, FieldKey::SectionMarker);
        // Common settings (user-facing) live before the header.
        let common_keys = [
            FieldKey::CockpitEnabled,
            FieldKey::CockpitDefaultForClaude,
            FieldKey::CockpitDefaultAgent,
            FieldKey::CockpitReplayEvents,
            FieldKey::CockpitNodePath,
            FieldKey::CockpitShowToolDurations,
        ];
        for k in common_keys {
            let pos = fields.iter().position(|f| f.key == k).unwrap();
            assert!(
                pos < header_idx,
                "common cockpit field {:?} must precede the Advanced header (pos={}, header={})",
                k,
                pos,
                header_idx
            );
        }
        // Advanced tuning lives after.
        let advanced_keys = [
            FieldKey::CockpitMaxConcurrentWorkers,
            FieldKey::CockpitMaxConcurrentResumes,
            FieldKey::CockpitQueueDrainMode,
            FieldKey::CockpitReplayBytes,
            FieldKey::CockpitForceEndTurnThresholdSecs,
            FieldKey::CockpitSilentOrphanGraceSecs,
            FieldKey::CockpitSilentOrphanFastGraceSecs,
            FieldKey::CockpitAutoStopIdleSecs,
        ];
        for k in advanced_keys {
            let pos = fields.iter().position(|f| f.key == k).unwrap();
            assert!(
                pos > header_idx,
                "advanced cockpit field {:?} must follow the Advanced header (pos={}, header={})",
                k,
                pos,
                header_idx
            );
        }
    }

    /// Splitting Session moved DefaultTool, agent maps, and attach-mode
    /// settings out into dedicated Agents / Interaction tabs. Pin the
    /// new homes so a future refactor doesn't silently re-merge them
    /// without re-evaluating the UX justification.
    #[test]
    fn session_split_moved_fields_to_their_new_tabs() {
        let global = Config::default();
        let profile = ProfileConfig::default();

        let agents_keys: Vec<FieldKey> = build_fields_for_category(
            SettingsCategory::Agents,
            SettingsScope::Global,
            &global,
            &profile,
        )
        .iter()
        .map(|f| f.key)
        .collect();
        for k in [
            FieldKey::DefaultTool,
            FieldKey::AgentExtraArgs,
            FieldKey::AgentCommandOverride,
            FieldKey::CustomAgents,
            FieldKey::AgentDetectAs,
            FieldKey::AgentCockpitCmd,
            FieldKey::AgentStatusHooks,
        ] {
            assert!(
                agents_keys.contains(&k),
                "expected {:?} in Agents tab, got {:?}",
                k,
                agents_keys
            );
        }

        let interaction_keys: Vec<FieldKey> = build_fields_for_category(
            SettingsCategory::Interaction,
            SettingsScope::Global,
            &global,
            &profile,
        )
        .iter()
        .map(|f| f.key)
        .collect();
        for k in [
            FieldKey::DefaultAttachMode,
            FieldKey::NewSessionAttachMode,
            FieldKey::ClickAction,
            FieldKey::LiveSendExitChord,
            FieldKey::LiveSendLeader,
            FieldKey::MouseCapture,
        ] {
            assert!(
                interaction_keys.contains(&k),
                "expected {:?} in Interaction tab, got {:?}",
                k,
                interaction_keys
            );
        }

        let session_keys: Vec<FieldKey> = build_fields_for_category(
            SettingsCategory::Session,
            SettingsScope::Global,
            &global,
            &profile,
        )
        .iter()
        .map(|f| f.key)
        .collect();
        // None of the moved fields should remain in Session.
        for k in [
            FieldKey::DefaultTool,
            FieldKey::AgentExtraArgs,
            FieldKey::AgentCommandOverride,
            FieldKey::CustomAgents,
            FieldKey::AgentDetectAs,
            FieldKey::AgentStatusHooks,
            FieldKey::DefaultAttachMode,
            FieldKey::NewSessionAttachMode,
            FieldKey::ClickAction,
            FieldKey::LiveSendExitChord,
        ] {
            assert!(
                !session_keys.contains(&k),
                "{:?} should have moved out of the Session tab, but it's still there: {:?}",
                k,
                session_keys
            );
        }
    }
}
