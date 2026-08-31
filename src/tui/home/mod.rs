//! Home view - main session list and navigation

pub(crate) mod bindings;
mod config_refresh;
mod creation;
mod dialogs;
#[cfg(test)]
mod file_watch_tests;
mod icons;
mod input;
mod layout;
mod lifecycle;
mod live_send;
mod live_send_prep;
mod operations;
mod overlays;
mod panes;
mod persistence;
mod pollers;
mod preview;
mod profiles;
mod projects;
pub(crate) mod render;
mod rows;
mod selection;
mod send;
mod status;
#[cfg(test)]
mod tests;
mod user_action;
mod watchers;

// LiveSendState is intentionally NOT re-exported: it's an internal
// detail of the home module. Tests that need to install it directly
// go through the `super::live_send::LiveSendState` path.

use std::collections::{HashMap, HashSet};

use ratatui::prelude::Rect;
use tui_input::Input;

use crate::session::{
    append_archived_section, append_archived_section_by_project, append_trash_section,
    config::{load_config, update_app_state, update_config, GroupByMode, SortOrder},
    flatten_sessions_by_attention, flatten_tree, flatten_tree_all_profiles, resolve_config_or_warn,
    DefaultTerminalMode, EnsureReadyOutcome, Group, GroupTree, Instance, Item, Storage,
};
use crate::tmux::AvailableTools;

use super::creation_poller::{CreatedWorktreeInfo, CreationPoller, CreationRequest};
use super::deletion_poller::DeletionPoller;
use super::dialogs::ServeView;
use super::dialogs::{
    AttachProjectDialog, ChangelogDialog, CommandPaletteDialog, ConfirmDialog, ContextMenuDialog,
    GroupDeleteOptionsDialog, GroupPickerDialog, HooksInstallDialog, InfoDialog, IntroDialog,
    NewSessionData, NewSessionDialog, NoAgentsDialog, ProfilePickerDialog,
    ProjectSessionPickerDialog, ProjectsDialog, RenameDialog, RepoTrustDialog, RestartDialog,
    SnoozeDurationDialog, SortPickerDialog, UnifiedDeleteDialog, UpdateConfirmDialog,
    WorktreeNameDialog,
};
use super::diff::DiffView;
use super::restart_poller::RestartPoller;
use super::settings::SettingsView;
use super::status_poller::{StatusPoller, StatusUpdate};
use super::stop_poller::StopPoller;

use self::creation::SessionMutationGuards;
use self::icons::{
    get_indent, ICON_ARCHIVED_SECTION, ICON_COLLAPSED, ICON_DELETING, ICON_DORMANT, ICON_ERROR,
    ICON_EXPANDED, ICON_IDLE, ICON_PINNED, ICON_STOPPED, ICON_TRASH_SECTION, ICON_UNKNOWN,
    ICON_UNREAD, UNREAD_DWELL,
};
use self::preview::{PreviewCache, PreviewSelection, PreviewTextView, PreviewTimings};
use self::rows::project_group_key;
pub(super) use self::watchers::{
    log_legacy_duplicates_once, tips_unseen_count, ConfigRefreshOrigin, ConfigWatchState,
    DiskWatchState, ReloadFailureState,
};
use self::watchers::{RELOAD_FAILED_TITLE, WATCHER_WARNING_TITLE};

/// Kinds of in-progress mouse drags. Today only the list/preview divider
/// is draggable; the enum keeps future drag targets (diff split, group
/// reorder) from churning the `Option<...>` shape on `HomeView`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DragKind {
    /// Resizing the side-by-side list/preview divider. `start_col` is the
    /// column where the user pressed; `start_width` is the requested
    /// `list_width` at that moment. The new requested width is
    /// `start_width + (current_col - start_col)`, clamped on apply.
    ListDivider { start_col: u16, start_width: u16 },
    /// Drag-selecting text inside the preview pane. Available whenever
    /// the pane is on screen (in or out of live-send mode). The anchor
    /// cell is where the user pressed; `preview_selection` on
    /// `HomeView` carries the live extent and is what the renderer
    /// reads. We keep the kind here (with no payload beyond a marker)
    /// so `handle_drag_move` / `handle_drag_end` can dispatch by
    /// variant without re-checking `live_send`.
    PreviewSelect,
    /// Grab-dragging the Settings fields-panel scrollbar. The live row is
    /// mapped to a scroll offset on each move (`scrollbar_drag_to_row`);
    /// there's no payload because the settings view owns the offset.
    SettingsScrollbar,
}

pub(super) struct GroupRenameContext {
    pub(super) old_path: String,
    pub(super) old_profile: String,
}

/// View mode for the home screen
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Structured,
    Terminal,
    /// Previewing a tool session (lazygit, yazi, etc.)
    Tool(String),
}

/// Terminal mode for sandboxed sessions (container vs host)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalMode {
    #[default]
    Host,
    Container,
}

/// Hook progress for a session being created in the background
pub(super) struct CreatingHookProgress {
    pub(super) hook_output: Vec<String>,
    pub(super) current_hook: Option<String>,
}

/// Result delivered by a startup-recovery worker back to the TUI tick.
struct RecoveryUpdate {
    instance_id: String,
    title: String,
    /// Updated `Instance` snapshot (post-cascade), so the TUI can replace
    /// its in-memory copy without a disk reload that would lose the
    /// freshly-set `last_start_time` (which is `#[serde(skip)]`).
    instance: Box<crate::session::Instance>,
    result: Result<crate::session::StartOutcome, String>,
}

pub struct HomeView {
    pub(super) storages: HashMap<String, Storage>,
    pub(super) active_profile: Option<String>,
    instances: indexmap::IndexMap<String, Instance>,
    /// Per-profile tombstones for ids removed since last `save`. Drained
    /// on Ok return so the next save retries on transient failure.
    pending_deletions: HashMap<String, HashSet<String>>,
    /// Per-profile tombstones for group paths removed since last `save`.
    /// Mirrors `pending_deletions` for groups so concurrent peer-added
    /// groups (e.g. `aoe add --group X`) survive the next save.
    pending_group_deletions: HashMap<String, HashSet<String>>,
    /// Per-profile ids added via `add_instance` since last save. In
    /// `save()`, only ids present here are pushed when the disk row is
    /// missing; TUI rows absent from disk AND absent from this set are
    /// treated as peer-deleted (CLI/`aoe serve`) and dropped from the
    /// in-memory mirror. Drained on Ok save.
    pending_added: HashMap<String, HashSet<String>>,
    pub(super) group_trees: HashMap<String, GroupTree>,
    /// Duplicate session ids that remain ambiguous after journal-guided
    /// reconciliation (#3459): every copy is excluded from `instances` and
    /// these details name the exact profiles, files, and mtimes to resolve.
    pub(super) legacy_duplicate_reports: Vec<crate::session::DuplicateIdReport>,
    pub(super) flat_items: Vec<Item>,

    // UI state
    pub(super) cursor: usize,
    pub(super) selected_session: Option<String>,
    pub(super) selected_group: Option<String>,
    /// Which profile the selected group belongs to (for scoped group operations)
    pub(super) selected_group_profile: Option<String>,
    pub(super) view_mode: ViewMode,
    pub(super) sort_order: SortOrder,
    pub(super) group_by: GroupByMode,
    /// Per-row tag config; what to show next to each session title.
    /// Cached from resolved SessionConfig at construction + reload_settings;
    /// the render layer reads this rather than re-resolving the config on
    /// every paint.
    pub(super) row_tag_mode: crate::session::config::RowTagMode,
    /// Whether an agent's OSC 52 clipboard write (surfaced by the VT capture
    /// worker) is forwarded to the host clipboard (#2420). Cached from
    /// `[tmux] clipboard != disabled` at construction + config refresh. Auto
    /// forwards too: that mode's "respect the user's tmux config" rationale
    /// is about tmux server options, which cannot influence this in-process
    /// path.
    pub(super) agent_clipboard_forward: bool,
    /// Whether live previews may use the VT transport (`[tmux] vt_live`).
    /// Cached at construction + config refresh and pushed into the capture
    /// worker (`set_vt_enabled`), so a settings toggle applies in place.
    pub(super) vt_live_enabled: bool,
    /// Active profile's `default_attach_mode`, cached at construction and
    /// refreshed by `refresh_from_config` / `switch_profile`. The help
    /// overlay falls back to this when no session row is selected so the
    /// render path never touches disk for the Enter/Tab labels.
    pub(super) profile_default_attach_mode: crate::session::AttachMode,
    /// Collapsed state for project-mode groups (persists across rebuilds)
    pub(super) project_group_collapsed: HashMap<String, bool>,
    /// Collapsed state for org-mode groups (persists across rebuilds), same
    /// shape and lifecycle as `project_group_collapsed`.
    pub(super) org_group_collapsed: HashMap<String, bool>,
    /// Memoizes `crate::git::get_remote_owner_with_key` per repo path so
    /// org-mode grouping doesn't re-open a git repo on every
    /// `rebuild_flat_items`. Stores `(display owner, host-scoped identity
    /// key)`: the key disambiguates same-named owners on different hosts
    /// (GitHub "acme" vs GitLab "acme"), the owner alone is the header's
    /// display text. Mirrors the server's `AppState.remote_owner_cache`
    /// (`src/server/api/sessions.rs`), but process-local to this TUI
    /// instance. `RefCell` gives interior mutability so `org_group_key` can
    /// stay `&self`, matching every other `*_group_name` call site.
    /// Cleared on every `reload_storage_only` so a `git remote
    /// add`/`set-url` picked up on the next periodic reload, not stuck
    /// under a stale cached owner (or lack thereof) for the rest of the
    /// process.
    pub(super) remote_owner_cache: std::cell::RefCell<HashMap<String, Option<(String, String)>>>,
    /// Merged project registry (global + active profile), refreshed on reload
    /// and after a pin/unpin. Project view injects the registered projects
    /// with no live sessions as empty "pinned" headers, and the renderer reads
    /// it to mark pinned headers. Mirrors the WebUI, where an empty project is
    /// just a registry entry decoupled from any session.
    pub(super) registered_projects: Vec<crate::session::Project>,

    // Dialogs
    pub(super) show_help: bool,
    pub(super) help_scroll: u16,
    pub(super) new_dialog: Option<NewSessionDialog>,
    pub(super) confirm_dialog: Option<ConfirmDialog>,
    pub(super) unified_delete_dialog: Option<UnifiedDeleteDialog>,
    pub(super) group_delete_options_dialog: Option<GroupDeleteOptionsDialog>,
    pub(super) rename_dialog: Option<RenameDialog>,
    pub(super) worktree_name_dialog: Option<WorktreeNameDialog>,
    pub(super) restart_dialog: Option<RestartDialog>,
    /// Right-click popup on the sidebar list. Anchored to a screen
    /// position when opened; the renderer clamps it into view.
    pub(super) context_menu: Option<ContextMenuDialog>,
    pub(super) group_rename_context: Option<GroupRenameContext>,
    pub(super) repo_trust_dialog: Option<RepoTrustDialog>,
    /// Session data pending repo trust approval (hooks and/or project MCP)
    pub(super) pending_repo_trust_data: Option<NewSessionData>,
    pub(super) hooks_install_dialog: Option<HooksInstallDialog>,
    /// Session data pending agent hooks acknowledgment
    pub(super) pending_hooks_install_data: Option<NewSessionData>,
    /// One-time confirm shown before a sandbox session whose resolved config
    /// has glob `volume_ignores` (e.g. `**/bin`), explaining the create-time
    /// snapshot expansion (#2045). Reuses [`ConfirmDialog`] with a
    /// "don't warn me again" checkbox persisted to app_state.
    pub(super) volume_ignores_glob_dialog: Option<ConfirmDialog>,
    /// Session data pending the volume_ignores glob expansion acknowledgment.
    pub(super) pending_volume_ignores_glob_data: Option<NewSessionData>,
    pub(super) intro_dialog: Option<IntroDialog>,
    /// Theme name queued by a click on the intro dialog (live preview or
    /// final pick). Drained by the `App` mouse handler after
    /// `handle_dialog_click` so the click path can apply the theme without
    /// returning an Action.
    pub(super) pending_intro_theme: Option<String>,
    pub(super) no_agents_dialog: Option<NoAgentsDialog>,
    pub(super) changelog_dialog: Option<ChangelogDialog>,
    pub(super) info_dialog: Option<InfoDialog>,
    pub(super) snooze_duration_dialog: Option<SnoozeDurationDialog>,
    /// Session id the snooze duration picker targets. Set when the dialog
    /// opens, consumed on submit.
    pub(super) pending_snooze_session: Option<String>,
    pub(super) profile_picker_dialog: Option<ProfilePickerDialog>,
    pub(super) group_picker_dialog: Option<GroupPickerDialog>,
    pub(super) sort_picker_dialog: Option<SortPickerDialog>,
    /// Attach-a-project picker for the selected session (#3103).
    pub(super) attach_project_dialog: Option<AttachProjectDialog>,
    pub(super) project_session_picker_dialog: Option<ProjectSessionPickerDialog>,
    pub(super) projects_dialog: Option<ProjectsDialog>,
    pub(super) plugin_manager_dialog: Option<crate::tui::dialogs::PluginManagerDialog>,
    pub(super) skills_manager_dialog: Option<crate::tui::dialogs::SkillsManagerDialog>,
    pub(super) command_palette: Option<CommandPaletteDialog>,
    pub(super) serve_view: Option<ServeView>,
    pub(super) update_confirm_dialog: Option<UpdateConfirmDialog>,
    /// One-time opt-in popup for users who finished the walkthrough before
    /// telemetry existed. Startup gating keeps it from rendering over the
    /// changelog or the version update modal.
    pub(super) telemetry_consent_dialog: Option<super::dialogs::TelemetryConsentDialog>,
    /// Tips overlay (the browsable list from `crate::tips`), when open. Reached
    /// from the command palette, the `?` help screen, or the tips badge.
    pub(super) tips_dialog: Option<super::dialogs::TipsDialog>,
    /// Cached count of eligible, unseen tips for the home-view badge. Recomputed
    /// from `app_state` on config refresh and after tips state changes, so the
    /// badge doesn't read config on every frame. Zero when tips are disabled.
    pub(super) tips_unseen: usize,
    /// An earned tip queued to pop gently once the user is back on the idle home
    /// view (set after the new-session dialog closes; see #2262). Drained on the
    /// next keystroke into a small one-tip overlay so it never interrupts an
    /// in-flight action.
    pub(super) pending_tip_pop: Option<&'static crate::tips::Tip>,
    /// Screen rect of the tips badge in the footer, captured each frame so a
    /// click can target it. `None` when the badge isn't drawn (nothing unseen,
    /// tips disabled, live-send banner up, or no room for it).
    pub(super) tips_badge_rect: Option<ratatui::layout::Rect>,
    /// Whether the mouse is currently over the tips badge, so it can paint a
    /// hover highlight like a session row does. Updated by `handle_hover`.
    pub(super) tips_badge_hovered: bool,
    pub(super) send_message_dialog: Option<super::dialogs::SendMessageDialog>,
    pub(super) permission_response_dialog: Option<super::dialogs::PermissionResponseDialog>,
    /// Session to receive the permission-response keystrokes once the
    /// dialog resolves.
    pub(super) pending_permission_response_session: Option<String>,
    /// Session to receive the message from the send dialog
    pub(super) pending_send_session: Option<String>,
    /// Which pane the pending send-message dialog will target. Set
    /// alongside `pending_send_session` and read when the dialog
    /// submits, so 'm' in Terminal view routes to the terminal pane
    /// instead of the agent. Defaults to Agent for the historical
    /// path (paste/dictation capture, palette compose).
    pub(super) pending_send_target: live_send::LiveSendTarget,
    /// Which pane the next `Action::EnterLiveSend` should target.
    /// Set by `start_live_send` whenever it returns an action; read
    /// (and reset to Agent) by `prepare_live_send` so each action
    /// carries its own target without a stale value leaking into a
    /// later live-send call. Defaults to Agent for the historical
    /// path (Tab in Structured view).
    pub(super) pending_live_send_target: live_send::LiveSendTarget,
    /// Live-send mode: when `Some`, every key event in the home view is
    /// translated to a tmux send-keys call against this session's pane
    /// until the user presses the exit chord (Ctrl+q). Set by `Tab` (in
    /// both modes) and by the palette entry; cleared by the exit chord
    /// inside the live handler.
    pub(super) live_send: Option<live_send::LiveSendState>,
    /// Background dispatcher created alongside `live_send`. Owns the
    /// tmux Session and a worker thread that drains a channel of
    /// translated keystrokes, coalescing runs of literals into single
    /// `tmux send-keys` calls so the UI thread never blocks on fork
    /// latency. Dropping (set to None when live mode exits) closes the
    /// channel and the worker thread exits cleanly on its own.
    pub(super) live_send_worker: Option<live_send::LiveSendWorker>,
    /// Background capture worker for whichever pane the preview is showing
    /// (agent, terminal, container shell, or tool). Forks `tmux
    /// capture-pane` on its own thread so no preview path ever forks on the
    /// render thread (the per-frame capture was ~90% of a frame on macOS).
    /// One long-lived worker: spawned lazily on first use by
    /// `sync_preview_capture_worker` and retargeted in place via
    /// `set_target` as the displayed pane changes; stays `None` until the
    /// first session is previewed.
    pub(super) preview_capture_worker: Option<live_send::LiveCaptureWorker>,
    /// The tmux session name `preview_capture_worker` is currently pointed
    /// at, so the reconcile can tell when the displayed pane changed and
    /// retarget. `None` before the first preview or when nothing is selected.
    pub(super) preview_capture_target: Option<String>,
    /// Last observed `LiveCaptureWorker::cycles` value and when it advanced.
    /// `None` before the first observation and after every retarget. If it
    /// remains unchanged beyond the shared tmux operation deadline plus grace,
    /// render replaces the worker without executing tmux synchronously.
    pub(super) preview_worker_pulse: Option<(u64, std::time::Instant)>,
    /// Notified by the capture worker thread when it has fresh, changed
    /// content. The event loop selects on this to repaint without
    /// busy-polling; an idle pane (no new content) never wakes it.
    pub(super) preview_wake: std::sync::Arc<tokio::sync::Notify>,
    /// Last (cols, rows) we asked the worker to resize the pane to in
    /// the current live-send session. Used to dedup the resize messages
    /// fired from the preview refresh path; cleared on live-send exit.
    pub(super) live_send_last_resize: Option<(u16, u16)>,
    /// Earliest time the same live-send geometry may retry after a worker
    /// failure. Keeps a dead or unreachable pane from turning the render
    /// ticker into a tmux subprocess loop.
    pub(super) live_send_resize_retry_at: Option<std::time::Instant>,
    /// True between a live-send leader press and the next key. While armed,
    /// the next key is interpreted as a live-send command (palette, sidebar
    /// toggle, exit) rather than forwarded to the agent, and the status bar
    /// shows the which-key menu. Always false outside live mode; cleared on
    /// live-send exit. See `handle_live_send_key`.
    pub(super) live_send_pending_leader: bool,
    /// Deadline until which the live-send footer flashes a "Ctrl+C sent to
    /// agent" reminder. Set on every Ctrl+C forwarded through live mode
    /// (#2894) so the user learns the keystroke reached the agent rather
    /// than quitting aoe, and cleared on live-send exit. `None` outside the
    /// flash window; the ~30fps live-mode ticker reverts the footer once it
    /// lapses.
    pub(super) live_send_ctrl_c_flash_until: Option<std::time::Instant>,
    /// When true, the session list (sidebar) collapses to a narrow,
    /// click-to-expand strip so the preview pane gets nearly the full
    /// terminal width. Toggled by the collapse button on the list border,
    /// by clicking the collapsed strip, or from live mode via the leader
    /// (`leader b`). Persisted to `app_state.home_sidebar_collapsed` so the
    /// choice survives restarts.
    pub(super) sidebar_collapsed: bool,
    /// `(session_id, cols, rows)` of the last NON-live preview resize we sent
    /// to the selected agent's pane, so the 250ms preview poll doesn't
    /// SIGWINCH-storm it every tick. Invalidated (set to None) on attach and on
    /// live-send enter/exit, where the window's real size changes out from
    /// under us and the next render must re-assert the preview geometry. See
    /// `refresh_preview_cache_if_needed`.
    pub(super) preview_pane_synced: Option<(String, u16, u16)>,
    /// `(session_id, cols, rows)` the NON-live preview sync wants but has only
    /// seen for one refresh so far. The sync fires a resize only once the same
    /// geometry is wanted on two consecutive refreshes: the `EnterLiveSend` /
    /// `SendMessage` handlers each draw exactly one frame with a transient
    /// "Reviving session..." toast up, that frame's output rect is one row
    /// shorter (the toast claims a bottom bar row), and chasing it resized the
    /// agent's pane down and back up within ~30ms. The double SIGWINCH made
    /// agents with a bottom-anchored input box (claude) visibly jump right as
    /// live mode opened. See `passive_resize_step`.
    pub(super) preview_pane_pending: Option<(String, u16, u16)>,
    /// Pasted text captured at the home view that we couldn't immediately
    /// route (no session selected, cursor on a group header, etc.). Drained
    /// into the next compose dialog the user opens, so voice/dictation never
    /// gets thrown on the floor with a scolding info dialog.
    pub(super) pending_paste: Option<String>,
    /// Session to attach after the custom instruction warning dialog is dismissed
    pub(super) pending_attach_after_warning: Option<String>,
    /// Session to stop after the confirmation dialog is accepted
    pub(super) pending_stop_session: Option<String>,
    /// Paired terminal to kill after the Terminal-view "kill terminal" confirm
    /// dialog is accepted. Carries the session id and which terminal (host vs
    /// container) the row was showing, so the accept path kills exactly the
    /// terminal the user was looking at without touching the agent session.
    pub(super) pending_stop_terminal: Option<(String, TerminalMode)>,
    /// Tool session to kill after the Tool-view "kill tool" confirm dialog is
    /// accepted: session id plus the tool name the view was previewing. Same
    /// contract as `pending_stop_terminal`, the agent session is untouched.
    pub(super) pending_stop_tool: Option<(String, String)>,
    /// Sandbox image to pull after the "image update available" confirm dialog
    /// is accepted. Carries the image through the generic `ConfirmDialog`,
    /// which only knows its action string.
    pub(super) pending_image_pull: Option<String>,
    /// Session whose persisted view flips (structured ↔ terminal) after the
    /// switch-view confirm dialog is accepted.
    pub(super) pending_switch_view_session: Option<String>,
    /// Session whose structured-view open is waiting on the "start a
    /// local daemon?" confirm (see `prompt_start_daemon_for_structured`).
    pub(super) pending_daemon_start_session: Option<String>,
    /// The structured-view session mounted in the preview pane, if any:
    /// a streaming transcript that `render_preview` paints as the
    /// preview content for a selected structured row (read-only until
    /// entered; see `EmbeddedView::is_active`). Owned here so the
    /// preview renderer, info header, and drag-select all compose with
    /// it; the `App` loop drives its async sides (connect, WS pump,
    /// active-mode key routing).
    pub(in crate::tui) structured_preview:
        Option<crate::tui::structured_view::embedded::EmbeddedView>,
    /// True while the App's preview-on-select reconcile has picked a
    /// structured session but its view hasn't finished mounting. The
    /// preview renderer shows a quiet placeholder instead of the wordy
    /// "press Enter" page, which otherwise flashes for the connect
    /// window on every selection.
    pub(in crate::tui) structured_preview_pending: bool,
    /// Session to force-remove after the confirmation dialog is accepted
    pub(super) pending_force_remove_session: Option<String>,
    /// Session to trash after the `session.confirm_delete` dialog is accepted
    pub(super) pending_trash_session: Option<String>,
    /// Action emitted by a mouse-click on a modal dialog (e.g. clicking
    /// `[Yes]` on a stop-session confirm). The keyboard path returns
    /// these via `handle_key -> Option<Action>`, but the mouse path
    /// goes through `handle_dialog_click` which has no return slot for
    /// an Action. Stashed here and drained by `app.rs` after the click
    /// is consumed so both paths produce the same downstream effect.
    pub(super) pending_dialog_click_action: Option<crate::tui::app::Action>,
    // Search
    pub(super) search_active: bool,
    pub(super) search_query: Input,
    pub(super) search_matches: Vec<usize>,
    pub(super) search_match_index: usize,

    // Tool availability
    pub(super) available_tools: AvailableTools,

    // Performance: background status polling
    pub(super) status_poller: StatusPoller,
    pub(super) pending_status_refresh: bool,

    // Compact system-health strip controlled by `session.show_diagnostics_pane`.
    // The poller samples host resources and agent counts off the UI thread;
    // `metrics` holds the latest sample for the strip and detail view.
    pub(super) show_diagnostics: bool,
    pub(super) metrics_poller: super::metrics_poller::MetricsPoller,
    pub(super) pending_metrics_refresh: bool,
    pub(super) metrics: crate::process::metrics::MetricsSnapshot,
    pub(super) system_health_open: bool,
    pub(super) system_health_scroll: usize,
    pub(super) diagnostics_area: Rect,
    pub(super) diagnostics_hovered: bool,
    pub(super) system_health_tip_high_samples: u8,
    pub(super) system_health_tip_earned: bool,
    pub(super) system_health_discovered: bool,

    // Structured (ACP) rows: the tmux poller above bails on them, so their
    // status comes from the daemon instead. See `daemon_status_poller`.
    pub(super) daemon_status_poller: super::daemon_status_poller::DaemonStatusPoller,
    pub(super) pending_daemon_status_refresh: bool,

    // Performance: background deletion
    pub(super) deletion_poller: DeletionPoller,

    // Performance: background stop (docker stop can block up to ~10s)
    pub(super) stop_poller: StopPoller,

    // Performance: background trash prep (stops the sandbox container, so the
    // same ~10s docker stop block as `stop_poller`, plus the worktree move)
    pub(super) trash_poller: crate::tui::trash_poller::TrashPoller,
    /// Load-time healing (trashed-worktree relocation, worktree paths moved
    /// outside aoe) kicked once from `HomeView::new` so it never delays the
    /// first frame; `apply_reconcile_results` reloads when it lands. See #3611.
    pub(super) reconcile_poller: crate::tui::reconcile_poller::ReconcilePoller,
    /// When the startup-recovery gate was armed, held until the first reconcile
    /// sweep lands so auto-recovery runs against repaired paths rather than the
    /// stale ones the sweep is about to fix. Carries the arming instant, not a
    /// bare flag, so a sweep that never lands cannot strand recovery for the
    /// whole boot. See `release_startup_recovery_gate`.
    pub(super) startup_recovery_gate: Option<std::time::Instant>,
    /// A landed sweep whose repair has not reached `instances` yet, because
    /// live-send is holding the reload. Keeps the recovery gate armed until the
    /// repair is applied. See `apply_reconcile_results`.
    pub(super) pending_reconcile_reload: bool,
    /// Earliest retry for a reconcile reload that failed. The tick calls
    /// `apply_reconcile_results` ~30 times a second, so an unreadable store
    /// would otherwise spin on storage and flood the log. See
    /// `RECONCILE_RELOAD_RETRY_INTERVAL`.
    pub(super) reconcile_reload_retry_at: Option<std::time::Instant>,

    // Performance: background restart (the start cascade shells out to docker
    // and runs the before_start host hook, which can block for seconds)
    pub(super) restart_poller: RestartPoller,
    /// Sessions whose restart cascade is in flight on the restart poller.
    /// Suppresses the StatusPoller's missing-tmux Error transition until the
    /// worker reports back via `apply_restart_results`.
    pub(super) restart_in_flight: std::collections::HashSet<String>,

    // Performance: background attach-a-project (#3103). `git worktree add`, an
    // optional fetch and submodule init, the worker bounce and the container
    // removal all shell out, so an inline attach froze the UI for its duration.
    pub(super) attach_project_poller: crate::tui::attach_project_poller::AttachProjectPoller,
    /// Sessions whose attach is in flight. One at a time per session: a second
    /// attach would race the first one's worktree creation and its worker bounce.
    pub(super) attach_project_in_flight: std::collections::HashSet<String>,

    // Performance: background session creation (for sandbox)
    pub(super) creation_poller: CreationPoller,
    /// Set to true if user cancelled while creation was pending
    pub(super) creation_cancelled: bool,
    /// Sessions whose on_launch hooks already ran in the creation poller
    pub(super) on_launch_hooks_ran: HashSet<String>,

    /// Hook progress for sessions in Creating state, keyed by stub instance ID
    pub(super) creating_hook_progress: HashMap<String, CreatingHookProgress>,
    /// The stub instance ID for the current background creation
    pub(super) creating_stub_id: Option<String>,
    /// Group paths introduced only to display the current Creating stub.
    /// Finalization removes these from persisted metadata before replacing the
    /// stub, while preserving groups that were already present when requested.
    creating_provisional_group_paths: HashSet<String>,

    // Performance: preview caching
    pub(super) preview_cache: PreviewCache,
    pub(super) terminal_preview_cache: PreviewCache,
    pub(super) container_terminal_preview_cache: PreviewCache,
    pub(super) tool_preview_cache: PreviewCache,

    /// Paint-side preview timings used to split mailbox/cache application,
    /// ANSI parsing, and widget construction in slow-frame traces.
    pub(super) preview_timings: PreviewTimings,
    pub(super) preview_scroll_offset: u16,
    pub(super) preview_area: Rect,
    /// Sub-rect of `preview_area` where the agent's captured pane content
    /// is actually painted: `preview_area` minus the info header when
    /// the user has it expanded (Structured view, non-compact). When the
    /// info header is hidden or the layout is compact, this matches
    /// `preview_area` exactly.
    ///
    /// `refresh_preview_cache_if_needed` and the live-send sync resize
    /// both read this so the tmux pane is sized to the visible output
    /// portion, not the full inner. Sizing to the full inner caused the
    /// agent to render `info_height` extra rows that the user couldn't
    /// see; tail-anchored display clipped those rows off the top, so
    /// every frame in info-expanded mode looked shifted up.
    pub(super) preview_pane_area: Rect,
    /// Rows of captured output the renderer actually paints into the preview
    /// body. This is just `PreviewLayout::compute(..).output.height`: the
    /// single split helper already accounts for the info header and the inner
    /// ` Output ` / ` Terminal Output ` banner. Set in `render_preview` from the
    /// same layout the renderer paints with, and shared with
    /// `clamp_scroll_to_capture` and the live-send `[offset/max]` banner so
    /// every consumer of "how many rows are visible" agrees with what's on
    /// screen.
    pub(super) preview_visible_rows: usize,
    /// Snapshot of the output pane's text layout from the last render,
    /// used by the drag-select handlers to map screen cells to absolute
    /// content lines (and back) between frames. Set in `render_preview`
    /// for the output-bearing paths; left at `total_lines == 0` for the
    /// creating / no-selection paths so a drag there is inert.
    pub(super) preview_text_view: PreviewTextView,
    /// Outer rect of the preview pane (block + borders + content), captured
    /// during `render_preview`. The live-send preview-only fast path uses
    /// this to call back into `render_preview` with the correct OUTER area,
    /// since `preview_area` itself is the INNER rect (used for hit-tests
    /// on the content). Passing the inner as if it were the outer would
    /// make `render_preview` draw a nested block.
    pub(in crate::tui) preview_outer_area: Rect,
    pub(super) diff_area: Rect,
    pub(super) list_area: Rect,
    /// Inner content rect of the session list (borders/padding stripped).
    /// Used to map a click coordinate to a `flat_items` index. The outer
    /// `list_area` still drives `hit_list` so wheel events over the border
    /// keep working; clicks use the inner rect so we don't try to select
    /// the border row.
    pub(super) list_inner_area: Rect,
    /// Inner content rect of the pinned bottom "shelf" that holds the
    /// synthetic Trash / Archived sections, rendered below the scrolling list
    /// and its divider. Zeroed on frames with no shelf (nothing trashed or
    /// archived) or while the sidebar is collapsed, so a stale rect can't
    /// resolve a click to a shelf row that isn't drawn. Clicks inside it map
    /// to the `flat_items` shelf suffix; see `resolve_row_to_index`.
    pub(super) shelf_inner_area: Rect,
    /// Clickable rect of the collapse button drawn on the list block's
    /// top-right border (expanded side-by-side/stacked view). Zeroed on
    /// frames where the button isn't drawn (e.g. while collapsed) so a
    /// stale rect can't swallow clicks.
    pub(super) collapse_button_area: Rect,
    /// Clickable rect of the narrow strip shown in place of the list when
    /// collapsed. Clicking anywhere in it re-expands the sidebar. Zeroed
    /// while the full list is drawn.
    pub(super) expand_strip_area: Rect,
    /// Clickable footer-toolbar buttons captured during `render_status_bar`,
    /// each paired with the `KeyEvent` a click synthesizes (so a click is
    /// dispatched through the exact same path as pressing the shortcut).
    /// Rebuilt every frame; empty in live mode and the takeover views.
    pub(super) footer_buttons: Vec<(Rect, crossterm::event::KeyEvent)>,
    /// The `KeyEvent` of the footer button the pointer is currently over, used
    /// to draw a hover highlight. Recomputed on every `Moved` event. Keyed by
    /// the button's shortcut rather than its index into `footer_buttons` so the
    /// highlight follows the right button when a sort/group/view-mode change
    /// reorders the toolbar between the move and the next render, and can never
    /// index a button that no longer exists.
    pub(super) footer_hover: Option<crossterm::event::KeyEvent>,
    /// Last reported mouse position when it was over `list_inner_area`,
    /// `None` when the cursor is outside the list. Stored as a position
    /// rather than a resolved item index so wheel scrolls implicitly
    /// re-resolve the hovered item without an extra event round-trip.
    pub(super) mouse_pos: Option<(u16, u16)>,
    /// Timestamp and row of the previous left-click. The next click is
    /// classified as a double-click when it lands within
    /// `DOUBLE_CLICK_THRESHOLD` on the same row, which then activates the
    /// session (same as pressing Enter on the selected row).
    pub(super) last_click: Option<(std::time::Instant, u16, u16)>,

    /// Same as `last_click`, but for left-presses on the preview pane. Kept
    /// separate so preview and sidebar double-click detection don't cross-talk.
    /// A second qualifying press within `DOUBLE_CLICK_THRESHOLD` activates the
    /// previewed session, matching a sidebar double-click.
    pub(super) last_preview_click: Option<(std::time::Instant, u16, u16)>,

    /// Dwell tracker for unread clear-on-read: the currently-selected session
    /// id and the instant the selection landed on it (while the list is the
    /// foreground, no dialog/live-send). When the same row stays selected for
    /// `UNREAD_DWELL`, the marker is cleared, distinguishing "scrolled past"
    /// from "actually read." Reset on selection change and on leaving the list.
    pub(super) unread_dwell: Option<(String, std::time::Instant)>,

    /// Session id the user just flagged unread by hand (`u`) and has not yet
    /// navigated away from. While this row stays selected, dwell-to-read won't
    /// undo the mark, so flagging a session and keeping the cursor on it
    /// sticks. It is released the moment the cursor leaves the row (see
    /// `tick_unread_dwell`), so returning to it later lets the dwell clear it
    /// like any other unread row. In-memory only: a manual flag is a
    /// this-visit hint, not a durable badge.
    pub(super) manual_unread_hold: Option<String>,

    // Terminal mode for sandboxed sessions (per-session, ephemeral)
    pub(super) terminal_modes: HashMap<String, TerminalMode>,
    // Default terminal mode from config
    pub(super) default_terminal_mode: TerminalMode,

    // Sound config for state transition sounds
    pub(super) sound_config: crate::sound::SoundConfig,
    pub(super) status_hook_config: crate::status_hooks::StatusHookConfig,
    pub(super) status_hook_configs: HashMap<String, crate::status_hooks::StatusHookConfig>,

    /// Resolved decay window from `Config.theme.idle_decay_minutes`. Read
    /// at startup and re-resolved on settings reload. Used by render to
    /// drive the breathe rattle and fresh-idle color, and by the `w`
    /// keybind to gate which Idle sessions are still "actionable".
    pub(super) idle_decay_window: std::time::Duration,

    // When true, letter-based action hotkeys require SHIFT (guard against
    // dictation / stray keystrokes triggering destructive actions).
    pub(super) strict_hotkeys: bool,

    // When true, pressing `q` to leave the home screen shows a quit
    // confirmation first (guards against accidental exits, #1569).
    pub(super) confirm_before_quit: bool,

    // Number of live `aoe` TUI processes (including this one), refreshed on a
    // throttle from the app loop. The footer surfaces it when >1 so the user
    // knows another instance is attached (the two clash over agent pane sizes
    // since tmux reflows to the smallest attached client).
    pub(super) active_tui_count: usize,

    // Settings view
    pub(super) settings_view: Option<SettingsView>,
    /// Flag to indicate we're confirming settings close (unsaved changes)
    pub(super) settings_close_confirm: bool,

    // Diff view
    pub(super) diff_view: Option<DiffView>,

    // Resizable list column width (percentage-like units)
    pub(super) list_width: u16,

    /// Visible column of the list/preview divider in side-by-side mode,
    /// `None` in stacked layout or while the diff view is open. Set in
    /// `render()` after the layout split; read by mouse handlers to
    /// hit-test divider clicks and clamp drag updates.
    pub(super) divider_col: Option<u16>,
    /// Width of the main horizontal area (list + preview) captured at the
    /// last render. Used as the clamp ceiling when a divider drag updates
    /// `list_width`, so the new width can't push the preview below
    /// `PREVIEW_MIN_WIDTH`.
    pub(super) main_area_width: u16,
    /// Active mouse-drag state, `None` when no button is held. Set on
    /// `Down(Left)` over a draggable target (the list/preview divider
    /// today), updated on each `Drag(Left)`, cleared on `Up(Left)`.
    pub(super) drag_state: Option<DragKind>,

    /// The SGR base button code (0/1/2) of a mouse press currently being
    /// forwarded to the previewed agent (`forward_mouse_to_preview`), so its
    /// drag and release reach the agent even after the pointer leaves the
    /// preview rect. `None` when no forwarded button is held.
    pub(super) mouse_forward_btn: Option<u16>,

    /// Last 1-based pane cell reported to the previewed agent as a bare
    /// mouse-motion (hover) event, so `forward_hover_to_preview` reports each
    /// cell once, the way a real terminal reports motion once per cell
    /// crossed. Cleared when the pointer leaves the preview so re-entering
    /// the same cell reports again.
    pub(super) hover_forward_cell: Option<(u16, u16)>,

    /// Last pointer cell reported during a `PreviewSelect` drag, `None`
    /// outside one. The event-loop ticker reads it (`tick_preview_autoscroll`)
    /// to keep scrolling while the cursor is held at the pane edge:
    /// crossterm only emits `Drag` events on movement, so without a
    /// ticker-driven scroll, holding still at the edge wouldn't advance.
    pub(super) preview_drag_pos: Option<(u16, u16)>,

    /// When the edge auto-scroll last advanced a line. Paces the scroll to
    /// a steady cadence so it doesn't race: the event loop wakes more often
    /// than the ticker (capture-worker wakes, post-key wakes), and stepping
    /// once per wake made the scroll speed lurch with pane activity.
    /// Reset whenever the cursor leaves the edge so re-entering scrolls at
    /// once.
    pub(super) preview_autoscroll_at: Option<std::time::Instant>,

    /// In-app text selection over the preview pane, populated only in
    /// live-send mode (where terminal-native drag-select doesn't reach
    /// us because mouse capture is on). The renderer reads this to
    /// paint a reversed-style highlight. Cleared on the next key
    /// press / click / mode change.
    pub(super) preview_selection: Option<PreviewSelection>,

    /// Set by `handle_drag_end` when a non-empty selection finalizes.
    /// On the next render, the highlight-paint pass reads cell symbols
    /// from the populated frame buffer, joins them into a string, and
    /// stashes that in `preview_copy_text` for the app loop to drain
    /// after the draw returns. Without this hop, reading
    /// `terminal.current_buffer_mut()` post-draw returns ratatui's
    /// blank back-buffer (it swaps current ↔ previous after every
    /// frame) so the extracted text is all empty cells.
    pub(super) preview_copy_pending: bool,

    /// Captured text from the most recently finalized preview
    /// selection, awaiting clipboard write. Drained by `App` right
    /// after the draw that paints the finalized highlight.
    pub(super) preview_copy_text: Option<String>,

    /// Show the info header (profile/tool/path/status/sandbox/worktree) at
    /// the top of the preview pane. Toggled with `i` and persisted to
    /// `app_state.show_preview_info`.
    pub(super) show_preview_info: bool,

    /// Collapsed state of the synthetic "Archived" sidebar section.
    /// Defaults to `true` (collapsed) so archived rows stay tucked at the
    /// bottom until the user opts to see them. Persisted to
    /// `app_state.archived_section_collapsed`.
    pub(super) archived_section_collapsed: bool,

    /// Collapsed state of the synthetic "Trash" sidebar section. Defaults to
    /// `true` (collapsed): trash is a recovery shelf, kept out of the way
    /// until the user opens it. In-memory only (not persisted): a fresh
    /// session starts with trash collapsed, which is the right default for a
    /// rarely-used shelf.
    // ponytail: in-memory only; persist to app_state if users ask for it.
    pub(super) trashed_section_collapsed: bool,

    /// Channel that startup-recovery workers send results back on. `None`
    /// when no recovery was attempted at construction (live tmux, daemon
    /// owns recovery, lock contended, or no candidates). Drained on every
    /// tick by `apply_recovery_updates`.
    recovery_rx: Option<std::sync::mpsc::Receiver<RecoveryUpdate>>,
    /// Lock guard kept alive for the recovery pass so a peer (a daemon
    /// that starts after the TUI) cannot duplicate cascades. Released
    /// when the field is set to `None` after the last worker has
    /// reported back.
    recovery_lock: Option<crate::session::recovery::RecoveryLock>,

    /// Ids whose startup-recovery cascade is still in flight. Filtered
    /// out of `request_status_refresh` so the 500ms poller does not
    /// observe missing tmux state and broadcast `Status::Error` while a
    /// worker is mid-cascade. Drained per-id by `apply_recovery_updates`
    /// (success, error, or panic). Mirrors the `on_launch_hooks_ran`
    /// HashSet pattern: TUI-local, event-driven, no TTL needed.
    recovery_in_flight: std::collections::HashSet<String>,

    /// Spam-debounce for the `e` / `E` / `F5` restart keybind: maps
    /// session id to the wall-clock instant of the last restart attempt.
    /// Presses arriving within 1.5s of the prior entry are dropped so
    /// rapid key-repeat doesn't race overlapping `restart_with_size`
    /// calls and tear down the still-booting tmux pane.
    pub(super) restart_cooldown_at: std::collections::HashMap<String, std::time::Instant>,

    // Tool sessions config (lazygit, yazi, etc.)
    pub(super) tool_configs: HashMap<String, crate::session::config::ToolSessionConfig>,
    /// Pre-parsed and sorted view of valid tool hotkeys: (name, KeyCode, KeyModifiers).
    /// Built once at construction and on settings reload, then iterated on every
    /// keystroke to look up matching tools. Sorted by name so the alphabetically-first
    /// tool wins on duplicate hotkeys.
    pub(super) tool_hotkey_cache: Vec<(
        String,
        crossterm::event::KeyCode,
        crossterm::event::KeyModifiers,
    )>,
    pub(super) tool_picker_dialog: Option<super::dialogs::ToolPickerDialog>,

    /// Process-wide file-watch primitive. Threaded into per-profile
    /// `Storage` instances so writes from this process surface
    /// immediately via the in-process Local fast path, and used to
    /// register per-profile subscriptions on `sessions.json` /
    /// `groups.json` so peer-process writes propagate within the
    /// primitive's debounce window.
    pub(super) file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    /// Per-profile storage-mirror subscriptions and the shared dirty
    /// latch they feed. The dirty flag is swapped to `false` by the tick
    /// loop when it consumes the kick; idempotent `store(true, Release)`
    /// is a cap-1 fan-in across all profile forwarders, collapsing
    /// multiple events between two reloads into one `reload_storage_only`
    /// regardless of source file.
    pub(super) disk_watch: DiskWatchState,
    /// Per-key config-file subscriptions (global + per-profile) and the
    /// shared dirty latch they feed. Distinct from `disk_watch.dirty`
    /// because config reloads call `refresh_from_config` while storage
    /// reloads call `reload_storage_only`; the two paths must remain
    /// independently schedulable on the same tick (config first, then
    /// storage; see `App::run`).
    pub(super) config_watch: ConfigWatchState,
    /// Monotonic counter incremented on every watcher-driven config
    /// refresh attempt (`try_refresh_from_config_watcher` invocation,
    /// including parse failures that return Err before
    /// `apply_config_to_state` runs). Surfaced to e2e tests via
    /// `<app_dir>/.aoe_e2e_refresh_count` when `AOE_E2E_DEBUG=1` is
    /// set on the TUI process; harness-driven tests poll the file for
    /// a post-edit refresh attempt as a deterministic completion
    /// signal. Production builds and non-e2e test runs never set the
    /// env var, so the file is never written.
    pub(super) watcher_config_refresh_count: std::sync::atomic::AtomicU64,
    /// Tracks tick-driven reload failures so a malformed `sessions.json`,
    /// `groups.json`, or `config.toml` does not crash the TUI. Populated
    /// by `handle_tick_reload_*`; consumed once per tick to surface a
    /// single aggregated `info_dialog` (multi-source body) and avoid
    /// spamming on every tick while a file remains broken.
    pub(super) reload_failure_state: ReloadFailureState,
    /// Theme name queued by `apply_config_to_state` on the Watcher path.
    /// Drained by the tick loop in `App::run` via `take_pending_watcher_theme`
    /// so `App::set_theme` can be called (theme state lives on `App`, not
    /// `HomeView`). The Interactive path dispatches `Action::SetTheme`
    /// directly and never sets this field. On a settings save the watcher
    /// echo also fires this path (idempotent: the Interactive dispatch
    /// already applied the same theme).
    pub(super) pending_watcher_theme: Option<String>,
}
