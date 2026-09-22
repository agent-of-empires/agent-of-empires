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
mod store_move;
#[cfg(test)]
mod tests;
mod user_action;
mod watchers;

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
    ICON_ARCHIVED_SECTION, ICON_COLLAPSED, ICON_DELETING, ICON_DORMANT, ICON_ERROR, ICON_EXPANDED,
    ICON_IDLE, ICON_PINNED, ICON_STOPPED, ICON_TRASH_SECTION, ICON_UNKNOWN, ICON_UNREAD,
    UNREAD_DWELL,
};
use self::preview::{PreviewCache, PreviewSelection, PreviewTextView, PreviewTimings};
use self::rows::project_group_key;
pub(super) use self::watchers::{
    log_legacy_duplicates_once, tips_unseen_count, ConfigRefreshOrigin, ConfigWatchState,
    DiskWatchState, ReloadFailureState,
};
use self::watchers::{RELOAD_FAILED_TITLE, WATCHER_WARNING_TITLE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DragKind {
    ListDivider,
    PreviewSelect,
    SettingsScrollbar,
}

pub(super) struct GroupRenameContext {
    pub(super) old_path: String,
    pub(super) old_profile: String,
}

pub(super) enum PermissionResponseTarget {
    Terminal(String),
    Structured { session_id: String, nonce: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Structured,
    Terminal,
    Tool(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalMode {
    #[default]
    Host,
    Container,
}

pub(super) struct CreatingHookProgress {
    pub(super) hook_output: Vec<String>,
    pub(super) current_hook: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PassiveSynced {
    pub(super) cols: u16,
    pub(super) rows: u16,
    pub(super) window_rows: u16,
    pub(super) adopted_at: std::time::Instant,
}

struct RecoveryUpdate {
    instance_id: String,
    title: String,
    instance: Box<crate::session::Instance>,
    result: Result<crate::session::StartOutcome, String>,
}

pub struct HomeView {
    pub(super) storages: HashMap<String, Storage>,
    pub(super) active_profile: Option<String>,
    instances: indexmap::IndexMap<String, Instance>,
    pending_deletions: HashMap<String, HashSet<String>>,
    pending_group_deletions: HashMap<String, HashSet<String>>,
    /// Ids added since the last save; TUI rows missing from both disk and this set are peer-deleted.
    pending_added: HashMap<String, HashSet<String>>,
    pub(super) group_trees: HashMap<String, GroupTree>,
    pub(super) legacy_duplicate_reports: Vec<crate::session::DuplicateIdReport>,
    pub(super) flat_items: Vec<Item>,

    pub(super) cursor: usize,
    pub(super) selected_session: Option<String>,
    pub(super) selected_group: Option<String>,
    pub(super) selected_group_profile: Option<String>,
    pub(super) view_mode: ViewMode,
    pub(super) sort_order: SortOrder,
    pub(super) group_by: GroupByMode,
    pub(super) row_tag_mode: crate::session::config::RowTagMode,
    pub(super) agent_clipboard_forward: bool,
    pub(super) hyperlink_cells: crate::tui::hyperlink::SharedHyperlinks,
    pub(super) vt_live_enabled: bool,
    pub(super) profile_default_attach_mode: crate::session::AttachMode,
    pub(super) project_group_collapsed: HashMap<String, bool>,
    pub(super) org_group_collapsed: HashMap<String, bool>,
    pub(super) remote_owner_cache: std::cell::RefCell<HashMap<String, Option<(String, String)>>>,
    pub(super) registered_projects: Vec<crate::session::Project>,

    pub(super) show_help: bool,
    pub(super) help_scroll: u16,
    pub(super) new_dialog: Option<NewSessionDialog>,
    pub(super) confirm_dialog: Option<ConfirmDialog>,
    pub(super) unified_delete_dialog: Option<UnifiedDeleteDialog>,
    pub(super) group_delete_options_dialog: Option<GroupDeleteOptionsDialog>,
    pub(super) rename_dialog: Option<RenameDialog>,
    pub(super) worktree_name_dialog: Option<WorktreeNameDialog>,
    pub(super) restart_dialog: Option<RestartDialog>,
    pub(super) context_menu: Option<ContextMenuDialog>,
    pub(super) group_rename_context: Option<GroupRenameContext>,
    pub(super) repo_trust_dialog: Option<RepoTrustDialog>,
    pub(super) pending_repo_trust_data: Option<NewSessionData>,
    pub(super) hooks_install_dialog: Option<HooksInstallDialog>,
    pub(super) pending_hooks_install_data: Option<NewSessionData>,
    pub(super) volume_ignores_glob_dialog: Option<ConfirmDialog>,
    pub(super) pending_volume_ignores_glob_data: Option<NewSessionData>,
    pub(super) intro_dialog: Option<IntroDialog>,
    pub(super) pending_intro_theme: Option<String>,
    pub(super) no_agents_dialog: Option<NoAgentsDialog>,
    pub(super) changelog_dialog: Option<ChangelogDialog>,
    pub(super) info_dialog: Option<InfoDialog>,
    pub(super) snooze_duration_dialog: Option<SnoozeDurationDialog>,
    pub(super) pending_snooze_session: Option<String>,
    pub(super) profile_picker_dialog: Option<ProfilePickerDialog>,
    pub(super) group_picker_dialog: Option<GroupPickerDialog>,
    pub(super) sort_picker_dialog: Option<SortPickerDialog>,
    pub(super) attach_project_dialog: Option<AttachProjectDialog>,
    pub(super) project_session_picker_dialog: Option<ProjectSessionPickerDialog>,
    pub(super) projects_dialog: Option<ProjectsDialog>,
    pub(super) plugin_manager_dialog: Option<crate::tui::dialogs::PluginManagerDialog>,
    pub(super) skills_manager_dialog: Option<crate::tui::dialogs::SkillsManagerDialog>,
    pub(super) command_palette: Option<CommandPaletteDialog>,
    pub(super) serve_view: Option<ServeView>,
    pub(super) update_confirm_dialog: Option<UpdateConfirmDialog>,
    pub(super) telemetry_consent_dialog: Option<super::dialogs::TelemetryConsentDialog>,
    pub(super) tips_dialog: Option<super::dialogs::TipsDialog>,
    pub(super) tips_unseen: usize,
    pub(super) pending_tip_pop: Option<&'static crate::tips::Tip>,
    pub(super) tips_badge_rect: Option<ratatui::layout::Rect>,
    pub(super) tips_badge_hovered: bool,
    pub(super) send_message_dialog: Option<super::dialogs::SendMessageDialog>,
    pub(super) permission_response_dialog: Option<super::dialogs::PermissionResponseDialog>,
    pub(super) pending_permission_response: Option<PermissionResponseTarget>,
    pub(super) pending_send_session: Option<String>,
    pub(super) pending_send_target: live_send::LiveSendTarget,
    pub(super) pending_live_send_target: live_send::LiveSendTarget,
    pub(super) live_send: Option<live_send::LiveSendState>,
    pub(super) live_send_worker: Option<live_send::LiveSendWorker>,
    pub(super) preview_capture_worker: Option<live_send::LiveCaptureWorker>,
    pub(super) preview_capture_target: Option<String>,
    pub(super) preview_worker_pulse: Option<(u64, std::time::Instant)>,
    pub(super) preview_wake: std::sync::Arc<tokio::sync::Notify>,
    pub(super) live_send_last_resize: Option<(u16, u16)>,
    pub(super) live_send_resize_retry_at: Option<std::time::Instant>,
    pub(super) live_send_pending_leader: bool,
    pub(super) hover_cell: Option<(u16, u16)>,
    pub(super) status_flash: Option<(String, std::time::Instant)>,
    pub(super) live_send_ctrl_c_flash_until: Option<std::time::Instant>,
    pub(super) sidebar_collapsed: bool,
    pub(super) sidebar_position: crate::session::config::SidebarPosition,
    pub(super) passive_pane_synced: std::collections::HashMap<String, PassiveSynced>,
    pub(super) passive_pane_declined:
        std::collections::HashMap<String, ((u16, u16), std::time::Instant)>,
    pub(super) passive_pane_queued: std::collections::HashMap<String, (u16, u16)>,
    pub(super) passive_fleet_armed: Option<Vec<(String, u16, u16)>>,
    /// Resize only when geometry repeats on two refreshes, so a one-frame toast can't resize the agent twice.
    pub(super) preview_pane_pending: Option<(String, u16, u16)>,
    pub(super) pending_paste: Option<String>,
    pub(super) pending_paste_for_structured_view: HashMap<String, String>,
    pub(super) pending_attach_after_warning: Option<String>,
    pub(super) pending_stop_session: Option<String>,
    pub(super) pending_stop_terminal: Option<(String, TerminalMode)>,
    pub(super) pending_stop_tool: Option<(String, String)>,
    pub(super) pending_image_pull: Option<String>,
    pub(super) pending_switch_view_session: Option<String>,
    pub(super) pending_daemon_start_session: Option<String>,
    pub(in crate::tui) structured_preview:
        Option<crate::tui::structured_view::embedded::EmbeddedView>,
    pub(in crate::tui) structured_preview_pending: bool,
    pub(super) pending_force_remove_session: Option<String>,
    pub(super) pending_trash_session: Option<String>,
    pub(super) pending_dialog_click_action: Option<crate::tui::app::Action>,
    pub(super) search_active: bool,
    pub(super) search_query: Input,
    pub(super) search_matches: Vec<usize>,
    pub(super) search_match_index: usize,

    pub(super) available_tools: AvailableTools,

    pub(super) status_poller: StatusPoller,
    pub(super) pending_status_refresh: bool,

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

    pub(super) session_feed: super::session_feed::SessionFeed,
    pub(super) pending_session_feed: bool,
    pub(super) daemon_sidebar: bool,
    pub(super) sidebar_source: super::session_feed::SidebarSource,
    pub(super) structured_pending_approvals: HashMap<String, Vec<crate::daemon::PendingApproval>>,
    pub(super) structured_approval_poller: super::approval_poller::StructuredApprovalPoller,

    pub(super) deletion_poller: DeletionPoller,

    pub(super) stop_poller: StopPoller,

    pub(super) trash_poller: crate::tui::trash_poller::TrashPoller,
    pub(super) reconcile_poller: crate::tui::reconcile_poller::ReconcilePoller,
    pub(super) startup_recovery_gate: Option<std::time::Instant>,
    pub(super) pending_reconcile_reload: bool,
    pub(super) reconcile_reload_retry_at: Option<std::time::Instant>,

    pub(super) restart_poller: RestartPoller,
    pub(super) restart_in_flight: std::collections::HashSet<String>,
    pub(super) attach_after_restart: std::collections::HashSet<String>,
    pub(super) restarted_attaches: Vec<String>,

    store_move_poller: crate::tui::store_move_poller::StoreMovePoller,
    store_move_in_flight: Option<store_move::StoreMoveInFlight>,
    store_move_bypass: Option<String>,

    pub(super) attach_project_poller: crate::tui::attach_project_poller::AttachProjectPoller,
    pub(super) attach_project_in_flight: std::collections::HashSet<String>,

    pub(super) creation_poller: CreationPoller,
    pub(super) creation_cancelled: bool,
    pub(super) on_launch_hooks_ran: HashSet<String>,

    pub(super) creating_hook_progress: HashMap<String, CreatingHookProgress>,
    pub(super) creating_stub_id: Option<String>,
    creating_provisional_group_paths: HashSet<String>,

    pub(super) preview_cache: PreviewCache,
    pub(super) terminal_preview_cache: PreviewCache,
    pub(super) container_terminal_preview_cache: PreviewCache,
    pub(super) tool_preview_cache: PreviewCache,

    pub(super) preview_timings: PreviewTimings,
    pub(super) preview_scroll_offset: u16,
    pub(super) preview_area: Rect,
    /// Output sub-rect of `preview_area` (minus the info header); tmux panes are sized to it.
    pub(super) preview_pane_area: Rect,
    pub(super) preview_visible_rows: usize,
    pub(super) preview_text_view: PreviewTextView,
    pub(in crate::tui) preview_outer_area: Rect,
    pub(super) diff_area: Rect,
    pub(super) list_area: Rect,
    pub(super) list_inner_area: Rect,
    pub(super) shelf_inner_area: Rect,
    pub(super) collapse_button_area: Rect,
    pub(super) expand_strip_area: Rect,
    pub(super) footer_buttons: Vec<(Rect, crossterm::event::KeyEvent)>,
    pub(super) footer_hover: Option<crossterm::event::KeyEvent>,
    pub(super) mouse_pos: Option<(u16, u16)>,
    pub(super) last_click: Option<(std::time::Instant, u16, u16)>,

    pub(super) last_preview_click: Option<(std::time::Instant, u16, u16)>,

    pub(super) unread_dwell: Option<(String, std::time::Instant)>,

    pub(super) manual_unread_hold: Option<String>,

    pub(super) terminal_modes: HashMap<String, TerminalMode>,
    pub(super) default_terminal_mode: TerminalMode,

    pub(super) sound_config: crate::sound::SoundConfig,
    pub(super) status_hook_config: crate::status_hooks::StatusHookConfig,
    pub(super) status_hook_configs: HashMap<String, crate::status_hooks::StatusHookConfig>,

    pub(super) idle_decay_window: std::time::Duration,

    pub(super) strict_hotkeys: bool,

    pub(super) confirm_before_quit: bool,

    pub(super) host_tab_title: bool,

    pub(super) active_tui_count: usize,

    pub(super) settings_view: Option<SettingsView>,
    pub(super) settings_close_confirm: bool,

    pub(super) diff_view: Option<DiffView>,

    pub(super) list_width: u16,

    pub(super) divider_col: Option<u16>,
    pub(super) main_area_width: u16,
    pub(super) drag_state: Option<DragKind>,

    pub(super) mouse_forward_btn: Option<u16>,

    pub(super) hover_forward_cell: Option<(u16, u16)>,

    pub(super) preview_drag_pos: Option<(u16, u16)>,

    pub(super) preview_autoscroll_at: Option<std::time::Instant>,

    pub(super) preview_selection: Option<PreviewSelection>,

    /// Copy text is read from the next frame's buffer; after `draw` returns the buffers are swapped.
    pub(super) preview_copy_pending: bool,

    pub(super) preview_copy_text: Option<String>,

    pub(super) show_preview_info: bool,

    pub(super) archived_section_collapsed: bool,

    pub(super) trashed_section_collapsed: bool,

    recovery_rx: Option<std::sync::mpsc::Receiver<RecoveryUpdate>>,
    /// Held while recovery workers run so a later daemon cannot duplicate cascades.
    recovery_lock: Option<crate::session::recovery::RecoveryLock>,

    recovery_in_flight: std::collections::HashSet<String>,

    pub(super) restart_cooldown_at: std::collections::HashMap<String, std::time::Instant>,

    pub(super) tool_configs: HashMap<String, crate::session::config::ToolSessionConfig>,
    pub(super) tool_hotkey_cache: Vec<(
        String,
        crossterm::event::KeyCode,
        crossterm::event::KeyModifiers,
    )>,
    pub(super) tool_picker_dialog: Option<super::dialogs::ToolPickerDialog>,

    pub(super) file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    pub(super) disk_watch: DiskWatchState,
    pub(super) config_watch: ConfigWatchState,
    pub(super) watcher_config_refresh_count: std::sync::atomic::AtomicU64,
    pub(super) reload_failure_state: ReloadFailureState,
    pub(super) pending_watcher_theme: Option<String>,
}
