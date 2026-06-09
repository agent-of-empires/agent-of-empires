//! Home view - main session list and navigation

pub(crate) mod bindings;
mod input;
mod live_send;
mod operations;
mod render;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod file_watch_tests;

// LiveSendState is intentionally NOT re-exported: it's an internal
// detail of the home module. Tests that need to install it directly
// go through the `super::live_send::LiveSendState` path.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use ratatui::prelude::Rect;
use tui_input::Input;

use crate::session::{
    append_archived_section, append_archived_section_by_project,
    config::{load_config, save_config, GroupByMode, SortOrder},
    flatten_sessions_by_attention, flatten_tree, flatten_tree_all_profiles, resolve_config_or_warn,
    DefaultTerminalMode, EnsureReadyOutcome, Group, GroupTree, Instance, Item, Storage,
};
use crate::tmux::AvailableTools;

use super::creation_poller::{CreationPoller, CreationRequest};
use super::deletion_poller::DeletionPoller;
#[cfg(feature = "serve")]
use super::dialogs::ServeView;
use super::dialogs::{
    ChangelogDialog, CommandPaletteDialog, ConfirmDialog, ContextMenuDialog,
    GroupDeleteOptionsDialog, GroupPickerDialog, HooksInstallDialog, InfoDialog, IntroDialog,
    NewSessionData, NewSessionDialog, NoAgentsDialog, ProfilePickerDialog,
    ProjectSessionPickerDialog, ProjectsDialog, RenameDialog, RepoTrustDialog, RestartDialog,
    SnoozeDurationDialog, SortPickerDialog, UnifiedDeleteDialog, UpdateConfirmDialog,
    WorktreeNameDialog,
};
use super::diff::DiffView;
use super::settings::SettingsView;
use super::status_poller::{StatusPoller, StatusUpdate};
use super::stop_poller::StopPoller;

/// Extract a project group name from a session instance.
/// Uses `worktree_info.main_repo_path` for worktree sessions (so all branches of the
/// same repo group together), otherwise uses `project_path`. Returns the last path segment.
fn project_group_name(inst: &Instance) -> String {
    // Scratch sessions live under `<app_dir>/scratch/<instance-id>/`, so the last
    // path segment is the opaque instance id. Group them under a readable label.
    if inst.scratch {
        return "scratch".to_string();
    }

    let base_path = inst
        .worktree_info
        .as_ref()
        .map(|wt| wt.main_repo_path.as_str())
        .unwrap_or(&inst.project_path);

    let path = std::path::Path::new(base_path);
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            // For root paths like "/", use a readable fallback
            if base_path == "/" || base_path.is_empty() {
                "(root)".to_string()
            } else {
                base_path.to_string()
            }
        })
}

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
}

/// The output pane's text layout, captured at render time so the input
/// handlers (which run between frames) can map a screen cell to the
/// absolute content line beneath it and back. `pane` is the on-screen
/// rect the parsed agent output is painted into (the info header and
/// banner are already stripped off); `first_line` is the index of the
/// content line drawn on `pane`'s top row (i.e. `compute_scroll`'s
/// result for the current scroll offset); `total_lines` is the parsed
/// scrollback length. The output Paragraph renders with no wrap and no
/// horizontal scroll, so screen row `pane.y + k` shows content line
/// `first_line + k`, and screen col `pane.x + c` shows content column
/// `c`. A `total_lines` of 0 means "no selectable content" (creating /
/// no-selection / empty panes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct PreviewTextView {
    pub(super) pane: ratatui::layout::Rect,
    pub(super) first_line: usize,
    pub(super) total_lines: usize,
}

impl PreviewTextView {
    /// True when `(col, row)` lands on a row/col that maps to real
    /// content. Used to gate drag-select start.
    pub(super) fn contains(self, col: u16, row: u16) -> bool {
        self.total_lines > 0
            && self
                .pane
                .contains(ratatui::layout::Position::from((col, row)))
    }

    /// Absolute parsed-text index of the line painted on screen row
    /// `row`, clamped into the pane and the scrollback.
    fn abs_line_at_row(self, row: u16) -> usize {
        let pane = self.pane;
        let max_y = pane.bottom().saturating_sub(1);
        let cy = row.clamp(pane.y, max_y);
        let mut line = self.first_line + (cy - pane.y) as usize;
        if self.total_lines > 0 {
            line = line.min(self.total_lines - 1);
        }
        line
    }

    /// Map a screen cell to selection coords `(col_offset, from_bottom)`,
    /// clamped into the pane and the scrollback. `col_offset` is 0-based
    /// from the pane's left edge; `from_bottom` counts lines up from the
    /// newest captured line (0 = the bottom line). See `PreviewSelection`
    /// for why selections anchor to the bottom rather than an absolute
    /// index.
    pub(super) fn screen_to_content(self, col: u16, row: u16) -> (u16, usize) {
        let pane = self.pane;
        let max_x = pane.right().saturating_sub(1);
        let col_off = col.clamp(pane.x, max_x) - pane.x;
        let abs = self.abs_line_at_row(row);
        (
            col_off,
            self.total_lines.saturating_sub(1).saturating_sub(abs),
        )
    }

    /// Absolute parsed-text index for a `from_bottom` distance under this
    /// view's current `total_lines`. The inverse of the `from_bottom` term
    /// in `screen_to_content`.
    fn abs_from_bottom(self, from_bottom: usize) -> usize {
        self.total_lines
            .saturating_sub(1)
            .saturating_sub(from_bottom)
    }
}

/// Flow-style text selection in the preview pane, matching tmux's
/// default mouse selection: from the anchor cell, the selection runs in
/// reading order (left-to-right, top-to-bottom) wrapping across every
/// row in between, and ends at the extent cell.
///
/// Coordinates are *content* coords, not screen cells: `col` is a 0-based
/// offset from the output pane's left edge and `from_bottom` counts lines
/// up from the newest captured line (0 = the bottom line). Anchoring to
/// the bottom (not an absolute index) is load-bearing: in live mode the
/// preview re-captures every frame, and the captured window *grows from
/// the top* as the user scrolls back (`capture_lines_for` adds the scroll
/// offset), so an absolute index would silently point at an older line as
/// the window grew, the exact bug where a drag-to-scroll copied the wrong
/// range. Distance from the newest line is invariant under that top-side
/// growth, so the highlight and the copy stay locked to the same text as
/// the user scrolls. The renderer re-derives screen rects each frame from
/// the live `PreviewTextView` via `screen_flow_rects`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PreviewSelection {
    /// Content cell the user pressed Down(Left) on: `(col_offset, from_bottom)`.
    pub(super) anchor: (u16, usize),
    /// Current (or final) extent. Equals `anchor` at drag start.
    pub(super) extent: (u16, usize),
    /// True once Up(Left) has fired. The renderer keeps the highlight
    /// visible after release until the user dismisses it (next key or
    /// click), so they can verify what was copied.
    pub(super) finalized: bool,
}

impl PreviewSelection {
    /// Anchor and extent resolved to absolute parsed-text indices under
    /// `total_lines` and ordered in reading order (line first, then
    /// column). The first tuple is where the selection starts in the flow;
    /// the second is where it ends. A drag that runs up-and-right still
    /// resolves to the higher line as the start.
    pub(super) fn ordered_abs(self, view: PreviewTextView) -> ((u16, usize), (u16, usize)) {
        let (ac, ad) = self.anchor;
        let (ec, ed) = self.extent;
        let a = (ac, view.abs_from_bottom(ad));
        let e = (ec, view.abs_from_bottom(ed));
        if (a.1, a.0) <= (e.1, e.0) {
            (a, e)
        } else {
            (e, a)
        }
    }

    /// Decompose the selection into per-row flow-shape screen `Rect`s,
    /// clipped to the visible window described by `view`. Lines above or
    /// below the visible window are skipped (the highlight just doesn't
    /// paint there); a partially-visible multi-line selection runs to the
    /// pane's right edge on every row but its last and from the left edge
    /// on every row but its first, matching the tmux default flow shape.
    /// Returns an empty vec when the pane is zero-sized.
    pub(super) fn screen_flow_rects(self, view: PreviewTextView) -> Vec<ratatui::layout::Rect> {
        let pane = view.pane;
        let mut out = Vec::new();
        if pane.width == 0 || pane.height == 0 {
            return out;
        }
        let ((start_col, start_line), (end_col, end_line)) = self.ordered_abs(view);
        let top = view.first_line;
        let bottom_excl = top + pane.height as usize;
        for line in start_line..=end_line {
            if line < top || line >= bottom_excl {
                continue;
            }
            let row = pane.y + (line - top) as u16;
            let left_off = if line == start_line { start_col } else { 0 };
            let right_off_excl = if line == end_line {
                end_col.saturating_add(1).min(pane.width)
            } else {
                pane.width
            };
            let left = pane.x + left_off.min(pane.width);
            let right_excl = pane.x + right_off_excl;
            if right_excl > left {
                out.push(ratatui::layout::Rect {
                    x: left,
                    y: row,
                    width: right_excl - left,
                    height: 1,
                });
            }
        }
        out
    }
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

/// Cached preview content to avoid subprocess calls on every frame
pub(super) struct PreviewCache {
    pub(super) session_id: Option<String>,
    pub(super) content: String,
    pub(super) last_refresh: Instant,
    pub(super) dimensions: (u16, u16),
    /// Number of lines that were captured into `content`. Used together with
    /// the BUFFER reserve so consecutive wheel ticks don't trigger a fresh
    /// `tmux capture-pane` subprocess while the cached window still covers
    /// the requested scroll.
    pub(super) captured_lines: usize,
    /// Lazily parsed ratatui `Text` view of `content`. Populated on the
    /// first render after a refresh that wasn't a no-op; reused as-is
    /// on every subsequent render until `content` is replaced. The
    /// invalidation point is `refresh_*_preview_cache_if_needed` which
    /// sets this to `None` whenever it writes a fresh `content`. See
    /// `PreviewCache::ensure_parsed` for the lazy-parse contract.
    ///
    /// Without this cache, `ansi-to-tui` re-parses the full pane
    /// payload (~12 KB of ANSI text for a typical agent) on every
    /// render iteration, including the many that fire on ticker
    /// wake-ups or unrelated key events. With it, the parse happens
    /// at most once per actual content change.
    pub(super) parsed_text: Option<ratatui::text::Text<'static>>,
}

impl Default for PreviewCache {
    fn default() -> Self {
        Self {
            session_id: None,
            content: String::new(),
            last_refresh: Instant::now(),
            dimensions: (0, 0),
            captured_lines: 0,
            parsed_text: None,
        }
    }
}

impl PreviewCache {
    /// Ensure `parsed_text` is populated, parsing `content` if it is
    /// not already cached. Side-effect only: returns nothing so the
    /// caller can drop the `&mut` borrow before reading
    /// `parsed_text` (which lets shared borrows on sibling fields of
    /// the parent struct coexist with the read).
    ///
    /// Cheap on cache-hit (single `is_none` check). Cache-miss runs
    /// `parse_output_text` once and stashes the result.
    pub(super) fn ensure_parsed(&mut self) {
        if self.content.is_empty() {
            self.parsed_text = None;
            return;
        }
        if self.parsed_text.is_none() {
            self.parsed_text = Some(crate::tui::components::preview::parse_output_text(
                &self.content,
            ));
        }
    }

    /// Store a fresh capture, invalidating the parsed cache and stamping
    /// the session/dimensions/time the content belongs to. Returns the
    /// captured line count so the caller can clamp scroll. Shared by the
    /// synchronous fork path (`refresh_preview_cache_core`) and the
    /// off-thread worker path so the two can't drift.
    pub(super) fn store_capture(
        &mut self,
        content: String,
        session_id: String,
        dimensions: (u16, u16),
    ) -> usize {
        self.captured_lines = content.lines().count();
        self.content = content;
        // Invalidate the cached parse; the next `ensure_parsed` re-runs
        // `ansi-to-tui`.
        self.parsed_text = None;
        self.session_id = Some(session_id);
        self.dimensions = dimensions;
        self.last_refresh = Instant::now();
        self.captured_lines
    }
}

/// Per-frame durations for the preview pipeline's two fork/CPU phases.
/// Lives on `HomeView`, reset each frame by `App::render`, and read back
/// by the render sampler so a slow or live-send frame logs the breakdown
/// instead of a single opaque `frame_ms`.
#[derive(Default, Clone, Copy)]
pub(super) struct PreviewTimings {
    pub(super) capture: std::time::Duration,
    pub(super) parse: std::time::Duration,
}

pub(super) const INDENTS: [&str; 10] = [
    "",
    " ",
    "  ",
    "   ",
    "    ",
    "     ",
    "      ",
    "       ",
    "        ",
    "         ",
];

pub(super) fn get_indent(depth: usize) -> &'static str {
    INDENTS.get(depth).copied().unwrap_or(INDENTS[9])
}

pub(super) const ICON_IDLE: &str = "⠒";
pub(super) const ICON_ERROR: &str = "✕";
pub(super) const ICON_UNKNOWN: &str = "⠤";
pub(super) const ICON_STOPPED: &str = "⠒";
pub(super) const ICON_DELETING: &str = "✕";
pub(super) const ICON_COLLAPSED: &str = "▶";
pub(super) const ICON_EXPANDED: &str = "▼";

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
    instances: Vec<Instance>,
    instance_map: HashMap<String, Instance>,
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
    /// Active profile's `default_attach_mode`, cached at construction and
    /// refreshed by `refresh_from_config` / `switch_profile`. The help
    /// overlay falls back to this when no session row is selected so the
    /// render path never touches disk for the Enter/Tab labels.
    pub(super) profile_default_attach_mode: crate::session::NewSessionAttachMode,
    /// Collapsed state for project-mode groups (persists across rebuilds)
    pub(super) project_group_collapsed: HashMap<String, bool>,

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
    pub(super) project_session_picker_dialog: Option<ProjectSessionPickerDialog>,
    pub(super) projects_dialog: Option<ProjectsDialog>,
    pub(super) command_palette: Option<CommandPaletteDialog>,
    #[cfg(feature = "serve")]
    pub(super) serve_view: Option<ServeView>,
    pub(super) update_confirm_dialog: Option<UpdateConfirmDialog>,
    /// One-time opt-in popup for users who finished the walkthrough before
    /// telemetry existed. Startup gating keeps it from rendering over the
    /// changelog or the version update modal.
    pub(super) telemetry_consent_dialog: Option<super::dialogs::TelemetryConsentDialog>,
    pub(super) send_message_dialog: Option<super::dialogs::SendMessageDialog>,
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
    /// Notified by the capture worker thread when it has fresh, changed
    /// content. The event loop selects on this to repaint without
    /// busy-polling; an idle pane (no new content) never wakes it.
    pub(super) preview_wake: std::sync::Arc<tokio::sync::Notify>,
    /// Last (cols, rows) we asked the worker to resize the pane to in
    /// the current live-send session. Used to dedup the resize messages
    /// fired from the preview refresh path; cleared on live-send exit.
    pub(super) live_send_last_resize: Option<(u16, u16)>,
    /// True between a live-send leader press and the next key. While armed,
    /// the next key is interpreted as a live-send command (palette, sidebar
    /// toggle, exit) rather than forwarded to the agent, and the status bar
    /// shows the which-key menu. Always false outside live mode; cleared on
    /// live-send exit. See `handle_live_send_key`.
    pub(super) live_send_pending_leader: bool,
    /// When true, the session list (sidebar) is hidden so the preview pane
    /// gets the full terminal width. Toggled from live mode via the leader
    /// (`leader b`) for a distraction-free structured view; reset on live-send
    /// exit so the list always reappears in the normal home view.
    pub(super) sidebar_collapsed: bool,
    /// `(session_id, cols, rows)` of the last NON-live preview resize we sent
    /// to the selected agent's pane, so the 250ms preview poll doesn't
    /// SIGWINCH-storm it every tick. Invalidated (set to None) on attach and on
    /// live-send enter/exit, where the window's real size changes out from
    /// under us and the next render must re-assert the preview geometry. See
    /// `refresh_preview_cache_if_needed`.
    pub(super) preview_pane_synced: Option<(String, u16, u16)>,
    /// Pasted text captured at the home view that we couldn't immediately
    /// route (no session selected, cursor on a group header, etc.). Drained
    /// into the next compose dialog the user opens, so voice/dictation never
    /// gets thrown on the floor with a scolding info dialog.
    pub(super) pending_paste: Option<String>,
    /// Session to attach after the custom instruction warning dialog is dismissed
    pub(super) pending_attach_after_warning: Option<String>,
    /// Session to stop after the confirmation dialog is accepted
    pub(super) pending_stop_session: Option<String>,
    /// Session to force-remove after the confirmation dialog is accepted
    pub(super) pending_force_remove_session: Option<String>,
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

    // Performance: background deletion
    pub(super) deletion_poller: DeletionPoller,

    // Performance: background stop (docker stop can block up to ~10s)
    pub(super) stop_poller: StopPoller,

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

    // Performance: preview caching
    pub(super) preview_cache: PreviewCache,
    pub(super) terminal_preview_cache: PreviewCache,
    pub(super) container_terminal_preview_cache: PreviewCache,
    pub(super) tool_preview_cache: PreviewCache,

    /// Per-frame timing of the preview pipeline's two latency-sensitive
    /// phases, reset by `App::render` before each `render` and populated
    /// at the agent-preview call site. `capture` is the `tmux
    /// capture-pane` fork (sub-100us when the gate short-circuits, ~1-10ms
    /// when it actually forks); `parse` is the `ansi-to-tui` pass (~0 on a
    /// parsed-cache hit). The app loop's render sampler reads these to
    /// break a live-send frame down into fork vs. parse vs. widget build.
    pub(super) preview_timings: PreviewTimings,

    /// Mouse wheel offset for the preview pane, in lines back from the bottom.
    /// Reset to 0 whenever the selected session changes.
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
    /// Set by the per-profile forwarder tasks; swapped to `false` by the
    /// tick loop when it consumes the kick. Cap-1 fan-in across all
    /// profile forwarders: idempotent `store(true, Release)` collapses
    /// multiple events between two reloads into one reload regardless of
    /// source file.
    pub(super) disk_dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Per-profile subscription pairs; see `rewire_disk_subscriptions`
    /// for the canonical drop-then-abort removal order.
    pub(super) disk_watch_handles: HashMap<String, DiskWatchEntry>,
    /// Tracks tick-driven reload failures so a malformed `sessions.json`
    /// or `groups.json` does not crash the TUI. Populated by
    /// `handle_tick_reload_storage`; consumed once per tick to surface a
    /// single aggregated `info_dialog` and avoid spamming on every tick
    /// while a file remains broken.
    pub(super) reload_failure_state: ReloadFailureState,
}

/// Per-profile subscription pair, held in `HomeView::disk_watch_handles`.
///
/// Two teardown paths exist:
/// 1. Explicit-remove (rewire / profile delete via `drop_disk_watch_entry`):
///    drop the `SubscriptionHandle` first to close the source channel; the
///    forwarder's `rx.recv().await` returns `None` and exits naturally;
///    `forwarder.abort()` then runs as a fast-path safeguard for any
///    `recv` future that has not yet observed the close.
/// 2. HomeView field-drop on shutdown: the same order falls out of struct
///    field declaration order. `disk_watch_handles` drops, each entry's
///    handle drops first (channel-close cascade), the forwarder exits
///    naturally; the AbortHandle drop is a no-op (Tokio's `AbortHandle`
///    does not abort on drop) but the forwarder is already gone.
pub(super) struct DiskWatchEntry {
    handle: crate::file_watch::SubscriptionHandle,
    forwarder: tokio::task::AbortHandle,
    /// Canonicalized profile dir at install time. Compared against the
    /// current canonical dir on rewire to detect peer-driven
    /// delete-and-recreate (same name, new inode); on mismatch the entry
    /// is treated as needing rewire even when the profile name set is
    /// unchanged. notify NonRecursive watches do not auto-reattach to a
    /// recreated directory on Linux inotify (IN_IGNORED) or macOS
    /// FSEvents (path-based but inconsistent across platforms).
    canonical_dir: std::path::PathBuf,
}

/// Drop the subscription handle FIRST. Closing the source channel
/// before aborting the forwarder ensures no in-flight event reaches
/// an aborted task.
fn drop_disk_watch_entry(entry: DiskWatchEntry) {
    let DiskWatchEntry {
        handle,
        forwarder,
        canonical_dir: _,
    } = entry;
    drop(handle);
    forwarder.abort();
}

/// Per-tick reload failure tracking. Tick-driven reload paths in
/// `App::run` (heartbeat `reload()`, watcher-driven `reload_storage_only()`)
/// route results through `handle_tick_reload_storage`, which records
/// failures here so the tick loop surfaces a single aggregated
/// `info_dialog` per failure burst rather than one dialog per tick.
///
/// `dialog_acknowledged` latches once the dialog is shown and clears
/// only after every source returns to healthy, so the user is
/// notified once per failure burst, not once per tick.
#[derive(Default)]
pub(super) struct ReloadFailureState {
    storage_failed: bool,
    storage_error: Option<String>,
    /// Latched description of the most recent watcher-init failure
    /// (typically `subscribe_channel` returning Err). Surfaced in the
    /// reload-failure dialog body so the user sees that live propagation
    /// is degraded for this profile. Cleared on the next successful
    /// rewire pass for that profile.
    watcher_init_error: Option<String>,
    dialog_acknowledged: bool,
}

impl ReloadFailureState {
    pub(super) fn record_storage(&mut self, result: &anyhow::Result<()>) {
        match result {
            Ok(()) => {
                if self.storage_failed {
                    self.storage_failed = false;
                    self.storage_error = None;
                    if !self.has_any_failure() {
                        self.dialog_acknowledged = false;
                    }
                }
            }
            Err(e) => {
                if !self.storage_failed {
                    self.dialog_acknowledged = false;
                }
                self.storage_failed = true;
                self.storage_error = Some(format!("{e:#}"));
            }
        }
    }

    pub(super) fn record_watcher_init_failure(&mut self, detail: &str) {
        let was_clear = self.watcher_init_error.is_none();
        self.watcher_init_error = Some(detail.to_string());
        if was_clear {
            self.dialog_acknowledged = false;
        }
    }

    pub(super) fn clear_watcher_init_failure(&mut self) {
        if self.watcher_init_error.is_some() {
            self.watcher_init_error = None;
            if !self.has_any_failure() {
                self.dialog_acknowledged = false;
            }
        }
    }

    pub(super) fn has_any_failure(&self) -> bool {
        self.storage_failed || self.watcher_init_error.is_some()
    }

    pub(super) fn has_unacknowledged_failure(&self) -> bool {
        self.has_any_failure() && !self.dialog_acknowledged
    }

    pub(super) fn build_dialog_body(&self) -> String {
        let mut lines: Vec<String> = vec!["The following reload sources are degraded:".to_string()];
        if let Some(e) = &self.storage_error {
            lines.push(format!("- Storage: {e}"));
        }
        if let Some(e) = &self.watcher_init_error {
            lines.push(format!("- Watcher init: {e}"));
        }
        lines.push(String::new());
        lines.push("Previous in-memory state preserved; will retry on next tick.".to_string());
        lines.join("\n")
    }

    pub(super) fn acknowledge_dialog(&mut self) {
        self.dialog_acknowledged = true;
    }
}

impl HomeView {
    pub fn new(
        active_profile: Option<String>,
        available_tools: AvailableTools,
        file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    ) -> anyhow::Result<Self> {
        use crate::session::list_profiles;

        let mut storages = HashMap::new();
        let mut all_instances = Vec::new();
        let mut group_trees = HashMap::new();

        let profile_names = match &active_profile {
            Some(name) => vec![name.clone()],
            None => list_profiles()?.into_iter().collect(),
        };

        for profile_name in &profile_names {
            let storage = Storage::new(profile_name, file_watch.clone())?;
            let (mut instances, groups) = storage.load_with_groups()?;
            for inst in &mut instances {
                inst.source_profile = profile_name.clone();
            }
            let tree = GroupTree::new_with_groups(&instances, &groups);
            group_trees.insert(profile_name.clone(), tree);
            all_instances.extend(instances);
            storages.insert(profile_name.clone(), storage);
        }

        let instance_map: HashMap<String, Instance> = all_instances
            .iter()
            .map(|i| (i.id.clone(), i.clone()))
            .collect();

        // In unified mode there is no single active profile, so config is
        // resolved from the user's default profile.
        let config_profile = active_profile
            .clone()
            .unwrap_or_else(crate::session::config::resolve_default_profile);
        let resolved = resolve_config_or_warn(&config_profile);
        let default_terminal_mode = match resolved.sandbox.default_terminal_mode {
            DefaultTerminalMode::Host => TerminalMode::Host,
            DefaultTerminalMode::Container => TerminalMode::Container,
        };
        let sound_config = resolved.sound.clone();
        let status_hook_configs = Self::load_status_hook_configs(Self::status_hook_profile_names(
            active_profile.as_deref(),
            &storages,
        ));
        let status_hook_config = status_hook_configs
            .get(&config_profile)
            .cloned()
            .unwrap_or_else(|| resolved.status_hooks.clone());
        let strict_hotkeys = resolved.session.strict_hotkeys;
        let confirm_before_quit = resolved.session.confirm_before_quit;
        let idle_decay_window =
            crate::tui::styles::idle_decay_window(resolved.theme.idle_decay_minutes);
        let user_config = load_config().ok().flatten();
        let sort_order = user_config
            .as_ref()
            .and_then(|c| c.app_state.sort_order)
            .unwrap_or_default();
        // New users (haven't dismissed the welcome screen) default to Project
        // grouping so they see the same layout as the web dashboard. Existing
        // users keep Manual (the existing behavior) unless they explicitly
        // toggle to Project with `g`.
        let is_new_user = user_config
            .as_ref()
            .is_none_or(|c| !c.app_state.has_seen_welcome);
        let default_group_by = if is_new_user {
            GroupByMode::Project
        } else {
            GroupByMode::Manual
        };
        let group_by = user_config
            .as_ref()
            .and_then(|c| c.app_state.group_by)
            .unwrap_or(default_group_by);
        let view_mode = ViewMode::default();

        let disk_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut view = Self {
            storages,
            active_profile,
            instances: all_instances,
            instance_map,
            pending_deletions: HashMap::new(),
            pending_group_deletions: HashMap::new(),
            pending_added: HashMap::new(),
            group_trees,
            flat_items: Vec::new(),
            cursor: 0,
            selected_session: None,
            selected_group: None,
            selected_group_profile: None,
            view_mode,
            sort_order,
            group_by,
            row_tag_mode: resolved.session.row_tag,
            profile_default_attach_mode: resolved.session.default_attach_mode,
            project_group_collapsed: HashMap::new(),
            show_help: false,
            help_scroll: 0,
            new_dialog: None,
            confirm_dialog: None,
            unified_delete_dialog: None,
            group_delete_options_dialog: None,
            rename_dialog: None,
            worktree_name_dialog: None,
            restart_dialog: None,
            context_menu: None,
            group_rename_context: None,
            repo_trust_dialog: None,
            pending_repo_trust_data: None,
            hooks_install_dialog: None,
            pending_hooks_install_data: None,
            intro_dialog: None,
            pending_intro_theme: None,
            no_agents_dialog: None,
            changelog_dialog: None,
            info_dialog: None,
            snooze_duration_dialog: None,
            pending_snooze_session: None,
            profile_picker_dialog: None,
            group_picker_dialog: None,
            sort_picker_dialog: None,
            project_session_picker_dialog: None,
            projects_dialog: None,
            command_palette: None,
            #[cfg(feature = "serve")]
            serve_view: None,
            update_confirm_dialog: None,
            telemetry_consent_dialog: None,
            send_message_dialog: None,
            pending_send_session: None,
            pending_send_target: live_send::LiveSendTarget::Agent,
            pending_live_send_target: live_send::LiveSendTarget::Agent,
            live_send: None,
            live_send_worker: None,
            preview_capture_worker: None,
            preview_capture_target: None,
            preview_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            live_send_last_resize: None,
            live_send_pending_leader: false,
            sidebar_collapsed: false,
            preview_pane_synced: None,
            pending_paste: None,
            pending_attach_after_warning: None,
            pending_stop_session: None,
            pending_force_remove_session: None,
            pending_dialog_click_action: None,
            search_active: false,
            search_query: Input::default(),
            search_matches: Vec::new(),
            search_match_index: 0,
            available_tools,
            status_poller: StatusPoller::new(),
            pending_status_refresh: false,
            deletion_poller: DeletionPoller::new(),
            stop_poller: StopPoller::new(),
            creation_poller: CreationPoller::new(),
            creation_cancelled: false,
            on_launch_hooks_ran: HashSet::new(),
            creating_hook_progress: HashMap::new(),
            creating_stub_id: None,
            preview_cache: PreviewCache::default(),
            preview_timings: PreviewTimings::default(),
            terminal_preview_cache: PreviewCache::default(),
            container_terminal_preview_cache: PreviewCache::default(),
            tool_preview_cache: PreviewCache::default(),
            preview_scroll_offset: 0,
            preview_text_view: PreviewTextView::default(),
            preview_area: Rect::default(),
            preview_pane_area: Rect::default(),
            preview_visible_rows: 0,
            preview_outer_area: Rect::default(),
            diff_area: Rect::default(),
            list_area: Rect::default(),
            list_inner_area: Rect::default(),
            mouse_pos: None,
            last_click: None,
            terminal_modes: HashMap::new(),
            default_terminal_mode,
            sound_config,
            status_hook_config,
            status_hook_configs,
            strict_hotkeys,
            confirm_before_quit,
            active_tui_count: 1,
            idle_decay_window,
            settings_view: None,
            settings_close_confirm: false,
            diff_view: None,
            list_width: user_config
                .as_ref()
                .and_then(|c| c.app_state.home_list_width)
                .unwrap_or(35),
            divider_col: None,
            main_area_width: 0,
            drag_state: None,
            preview_drag_pos: None,
            preview_autoscroll_at: None,
            preview_selection: None,
            preview_copy_pending: false,
            preview_copy_text: None,
            show_preview_info: user_config
                .as_ref()
                .and_then(|c| c.app_state.show_preview_info)
                .unwrap_or(true),
            archived_section_collapsed: user_config
                .as_ref()
                .and_then(|c| c.app_state.archived_section_collapsed)
                .unwrap_or(true),
            recovery_rx: None,
            recovery_lock: None,
            recovery_in_flight: std::collections::HashSet::new(),
            restart_cooldown_at: std::collections::HashMap::new(),
            tool_configs: user_config
                .as_ref()
                .map(|c| c.tools.clone())
                .unwrap_or_default(),
            tool_hotkey_cache: Vec::new(),
            tool_picker_dialog: None,
            file_watch,
            disk_dirty,
            disk_watch_handles: HashMap::new(),
            reload_failure_state: ReloadFailureState::default(),
        };

        view.tool_hotkey_cache = input::build_tool_hotkey_cache(&view.tool_configs);
        let hotkey_warnings = input::validate_tool_hotkeys(&view.tool_configs);
        if !hotkey_warnings.is_empty() && view.info_dialog.is_none() {
            view.info_dialog = Some(InfoDialog::new(
                "Tool hotkey config errors",
                &hotkey_warnings.join("\n"),
            ));
        }

        // Clean up orphaned Creating instances from a prior crash
        let orphan_ids: Vec<String> = view
            .instances
            .iter()
            .filter(|i| i.status == crate::session::Status::Creating)
            .map(|i| i.id.clone())
            .collect();
        for id in &orphan_ids {
            view.remove_instance(id);
        }
        if !orphan_ids.is_empty() {
            tracing::info!(target: "tui.home", "Cleaned up {} orphaned creating sessions", orphan_ids.len());
            if let Err(e) = view.save() {
                tracing::warn!(target: "tui.home", "Failed to save view state: {e}");
            }
        }

        // Batch-sync instance IDs and captured session IDs to tmux hidden env
        // so that build_exclusion_set() on other AoE instances can see them.
        {
            let mut set_batch: Vec<(String, String, String)> = Vec::new();
            let mut unset_batch: Vec<(String, String)> = Vec::new();
            for inst in &view.instances {
                let tmux_name = match inst.tmux_session() {
                    Ok(s) if s.exists() && !s.is_pane_dead() => s.name().to_string(),
                    _ => continue,
                };

                set_batch.push((
                    tmux_name.clone(),
                    crate::tmux::env::AOE_INSTANCE_ID_KEY.to_string(),
                    inst.id.clone(),
                ));
                if let Some(ref sid) = inst.agent_session_id {
                    set_batch.push((
                        tmux_name,
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                        sid.clone(),
                    ));
                } else {
                    unset_batch.push((
                        tmux_name,
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                    ));
                }
            }
            if !set_batch.is_empty() {
                let batch_refs: Vec<(&str, &str, &str)> = set_batch
                    .iter()
                    .map(|(s, k, v)| (s.as_str(), k.as_str(), v.as_str()))
                    .collect();
                if let Err(e) = crate::tmux::env::set_hidden_env_batch(&batch_refs) {
                    tracing::warn!(target: "tui.home", "Batch env sync failed: {}", e);
                }
            }
            if !unset_batch.is_empty() {
                let batch_refs: Vec<(&str, &str)> = unset_batch
                    .iter()
                    .map(|(s, k)| (s.as_str(), k.as_str()))
                    .collect();
                if let Err(e) = crate::tmux::env::remove_hidden_env_batch(&batch_refs) {
                    tracing::warn!(target: "tui.home", "Batch env unset failed: {}", e);
                }
            }
        }

        // Recover session IDs for pre-existing sessions via pollers.
        for inst in &mut view.instances {
            let has_live_tmux = inst.has_live_tmux_pane();
            if !has_live_tmux {
                continue;
            }

            if inst.supports_session_poller() && inst.session_id_poller.is_none() {
                inst.maybe_start_poller();
            }
        }

        // Startup auto-recovery: kick off a worker pool to restart any
        // resume-capable sessions whose tmux pane is missing. The TUI defers
        // to the daemon when one is running (the daemon owns recovery in
        // that case); when the TUI is standalone, it acquires the
        // cross-process recovery lock to keep a late-starting daemon from
        // duplicating cascades. See `crate::session::recovery` for the full
        // exclusion rationale.
        view.maybe_start_startup_recovery();

        view.instance_map = view
            .instances
            .iter()
            .map(|i| (i.id.clone(), i.clone()))
            .collect();

        view.flat_items = view.build_flat_items();
        view.update_selected();
        let initial_profiles: Vec<String> = view.storages.keys().cloned().collect();
        view.rewire_disk_subscriptions(&initial_profiles)?;
        Ok(view)
    }

    /// Full reload: status-hook config-cache refresh + storage. Used by
    /// the 5s heartbeat tick and by event-driven sites (attach-return,
    /// save+reload pairs, profile switch). Watcher-driven ticks call
    /// `reload_storage_only` because the watcher only fires on
    /// `sessions.json` / `groups.json`; status-hook config is not
    /// watched.
    pub fn reload(&mut self) -> anyhow::Result<()> {
        self.refresh_status_hook_config_cache();
        self.reload_storage_only()
    }

    /// Storage-only reload: profile rediscovery + per-profile load + tree
    /// rebuild + cursor restore. Skips the status-hook config-cache refresh,
    /// which is driven by the full `reload()` path. Used by the watcher-
    /// driven tick.
    pub(super) fn reload_storage_only(&mut self) -> anyhow::Result<()> {
        use crate::session::list_profiles;

        let mut all_instances = Vec::new();

        if self.active_profile.is_none() {
            let current_profiles = list_profiles()?;
            for name in &current_profiles {
                if !self.storages.contains_key(name) {
                    self.storages
                        .insert(name.clone(), Storage::new(name, self.file_watch.clone())?);
                }
            }
            self.storages.retain(|k, _| current_profiles.contains(k));
            self.rewire_disk_subscriptions(&current_profiles)?;
        }

        for (profile_name, storage) in &self.storages {
            let (mut instances, groups) = storage.load_with_groups()?;
            for inst in &mut instances {
                inst.source_profile = profile_name.clone();
                if let Some(prev) = self.instance_map.get(&inst.id) {
                    inst.status = prev.status;
                    inst.last_error = prev.last_error.clone();
                    inst.last_error_check = prev.last_error_check;
                    inst.last_start_time = prev.last_start_time;
                    inst.session_id_poller = prev.session_id_poller.clone();
                    // Carry the in-memory idle_entered_at across reloads
                    // so a freshly-stopped session doesn't lose its
                    // freshness state when the user toggles a setting
                    // that triggers a reload mid-window.
                    inst.idle_entered_at = prev.idle_entered_at;
                    // agent_session_id is disk-authoritative; writers persist
                    // synchronously through Storage::update before reload runs.
                    // Carry the resume-fallback exclusion set across
                    // reloads. Without this, a stale sid that the cascade
                    // just cleared would be re-imported on the next 5s reload
                    // (the on-disk session artifact persists for ~5-10
                    // min after the agent's crash). The set is
                    // `#[serde(skip)]` runtime-only so disk reloads
                    // would otherwise reset it to empty.
                    inst.retroactive_capture_excludes = prev.retroactive_capture_excludes.clone();
                }
            }
            // Rebuild this profile's tree from disk, preserving any collapsed
            // state that was toggled in-memory but not yet on disk
            let mut new_tree = GroupTree::new_with_groups(&instances, &groups);
            if let Some(old_tree) = self.group_trees.get(profile_name) {
                for g in old_tree.get_all_groups() {
                    if g.collapsed {
                        new_tree.set_collapsed(&g.path, true);
                    }
                }
            }
            self.group_trees.insert(profile_name.clone(), new_tree);
            all_instances.extend(instances);
        }

        // Remove trees for profiles that no longer exist
        let storage_keys: Vec<String> = self.storages.keys().cloned().collect();
        self.group_trees.retain(|k, _| storage_keys.contains(k));

        self.instances = all_instances;

        // Re-inject any in-flight Creating stub that won't be on disk
        if let Some(ref stub_id) = self.creating_stub_id {
            if !self.instances.iter().any(|i| i.id == *stub_id) {
                if let Some(stub) = self.instance_map.get(stub_id).cloned() {
                    self.instances.push(stub);
                }
            }
        }

        self.instance_map = self
            .instances
            .iter()
            .map(|i| (i.id.clone(), i.clone()))
            .collect();
        // Remember what the cursor was pointing at so we can follow it
        let prev_selected_session = self.selected_session.clone();
        let prev_selected_group = self.selected_group.clone();

        self.flat_items = self.build_flat_items();

        // Try to restore cursor to the same session/group after rebuild
        let mut restored = false;
        if let Some(ref sid) = prev_selected_session {
            for (idx, item) in self.flat_items.iter().enumerate() {
                if let Item::Session { id, .. } = item {
                    if id == sid {
                        self.cursor = idx;
                        restored = true;
                        break;
                    }
                }
            }
        } else if let Some(ref gpath) = prev_selected_group {
            for (idx, item) in self.flat_items.iter().enumerate() {
                if let Item::Group { path, .. } = item {
                    if path == gpath {
                        self.cursor = idx;
                        restored = true;
                        break;
                    }
                }
            }
        }
        if !restored && self.cursor >= self.flat_items.len() && !self.flat_items.is_empty() {
            self.cursor = self.flat_items.len() - 1;
        }

        if self.search_active && !self.search_query.value().is_empty() {
            self.update_search();
        } else if !self.search_matches.is_empty() {
            // Recalculate match indices without moving the cursor
            self.refresh_search_matches();
        }

        self.update_selected();
        Ok(())
    }

    /// Reconcile per-profile file-watch subscriptions against `current`
    /// via set-diff: drop entries for profiles in `prior - current`,
    /// keep entries in `prior ∩ current` untouched, install fresh
    /// entries for profiles in `current - prior`. Same-set rewires are
    /// a no-op.
    ///
    /// Inode-invalidation case (profile dir deleted and recreated under
    /// the same name): the caller must drop the stale entry first via
    /// `drop_disk_watch_entry` before invoking this helper, so the name
    /// is missing from `prior` and the install path runs.
    pub(super) fn rewire_disk_subscriptions(&mut self, current: &[String]) -> anyhow::Result<()> {
        use crate::file_watch::{FileMatcher, WatchSpec};
        use std::collections::HashSet;
        use std::time::Duration;

        if tokio::runtime::Handle::try_current().is_err() {
            return Ok(());
        }

        let prior: HashSet<String> = self.disk_watch_handles.keys().cloned().collect();
        let target: HashSet<&String> = current.iter().collect();

        // Detect peer-driven delete-and-recreate of any prior profile dir
        // by comparing each prior entry's stored canonical_dir against the
        // current canonical resolution. Mismatch forces a rewire of that
        // entry even when the name set is unchanged, since notify
        // NonRecursive watches do not auto-reattach across the inode
        // change on Linux inotify or macOS FSEvents. Resolve via the
        // non-creating `get_profile_dir_path`: this is a read-only
        // existence/canonicalization probe, and `get_profile_dir` would
        // resurrect a profile dir that a peer just deleted, leaving the
        // removed profile visible in `list_profiles()` forever.
        let inode_invalidated: HashSet<String> = prior
            .iter()
            .filter(|name| {
                let entry = match self.disk_watch_handles.get(*name) {
                    Some(e) => e,
                    None => return false,
                };
                let current_canonical = crate::session::get_profile_dir_path(name)
                    .ok()
                    .and_then(|p| std::fs::canonicalize(&p).ok());
                match current_canonical {
                    Some(canonical) => canonical != entry.canonical_dir,
                    None => true,
                }
            })
            .cloned()
            .collect();

        if prior == current.iter().cloned().collect() && inode_invalidated.is_empty() {
            return Ok(());
        }

        // Clear the latch ahead of the install loop. `record_watcher_init_failure`
        // re-latches it on any `subscribe_channel` Err below, so the latch
        // reflects the outcome of this rewire pass.
        self.reload_failure_state.clear_watcher_init_failure();

        let to_remove: Vec<String> = prior
            .iter()
            .filter(|n| !target.contains(*n) || inode_invalidated.contains(*n))
            .cloned()
            .collect();
        let to_add: Vec<String> = current
            .iter()
            .filter(|n| !prior.contains(*n) || inode_invalidated.contains(*n))
            .cloned()
            .collect();

        for name in &to_remove {
            if let Some(entry) = self.disk_watch_handles.remove(name) {
                drop_disk_watch_entry(entry);
            }
        }

        for name in &to_add {
            let dir = match crate::session::get_profile_dir(name) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        profile = %name,
                        error = %e,
                        "skipping subscribe; profile dir resolution failed"
                    );
                    continue;
                }
            };
            let canonical_dir = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            let sessions_path = dir.join("sessions.json");
            let groups_path = dir.join("groups.json");
            let spec = WatchSpec {
                dir: dir.clone(),
                matcher: FileMatcher::AnyOf(vec![sessions_path, groups_path]),
                debounce: Some(Duration::from_millis(75)),
            };
            match self.file_watch.subscribe_channel(spec, 16) {
                Ok((mut rx, handle)) => {
                    use tracing::Instrument;
                    let dirty = self.disk_dirty.clone();
                    // Forwarder exits via `rx.recv() = None` when its
                    // SubscriptionHandle is dropped (rewire / HomeView
                    // teardown). The TUI has no graceful-drain phase, so
                    // no `CancellationToken` is plumbed through here.
                    let span = tracing::debug_span!(
                        "tui.disk_watch.forwarder",
                        profile = %name
                    );
                    let join = crate::task_util::spawn_supervised(
                        "tui.disk_watch.forwarder",
                        crate::task_util::PanicPolicy::Log,
                        async move {
                            while rx.recv().await.is_some() {
                                dirty.store(true, std::sync::atomic::Ordering::Release);
                            }
                        }
                        .instrument(span),
                    );
                    self.disk_watch_handles.insert(
                        name.clone(),
                        DiskWatchEntry {
                            handle,
                            forwarder: join.abort_handle(),
                            canonical_dir,
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        profile = %name,
                        error = %e,
                        "subscribe_channel failed; falling back to 5s heartbeat for this profile"
                    );
                    self.reload_failure_state
                        .record_watcher_init_failure(&format!("{}: {}", name, e));
                }
            }
        }
        tracing::debug!(
            target: "tui.file_watch",
            added = ?to_add,
            removed = ?to_remove,
            "reconciled per-profile disk-watch subscriptions"
        );
        Ok(())
    }

    /// Snapshot of `self.instances` eligible for status polling.
    /// In-flight recovery candidates are excluded; their post-cascade
    /// `Instance` arrives via `apply_recovery_updates` and skipping the
    /// parallel poll prevents racing transitions during the suppression
    /// window.
    pub(super) fn pollable_instances(&self) -> Vec<Instance> {
        self.instances
            .iter()
            .filter(|i| !self.recovery_in_flight.contains(&i.id))
            .cloned()
            .collect()
    }

    pub(super) fn attached_status_hook_sessions(
        &self,
    ) -> Vec<super::attached_status_hooks::AttachedStatusHookSession> {
        self.pollable_instances()
            .into_iter()
            .filter_map(|instance| {
                let hook_config = self.status_hook_config_for(&instance);
                hook_config.enabled.then_some(
                    super::attached_status_hooks::AttachedStatusHookSession {
                        instance,
                        hook_config,
                    },
                )
            })
            .collect()
    }

    /// Request a status refresh in the background (non-blocking).
    /// Call `apply_status_updates` to check for and apply results.
    pub fn request_status_refresh(&mut self) {
        if !self.pending_status_refresh {
            self.status_poller
                .request_refresh(self.pollable_instances());
            self.pending_status_refresh = true;
        }
    }

    /// Apply any pending status updates from the background poller.
    /// Returns true if updates were applied.
    pub fn apply_status_updates(&mut self) -> bool {
        if let Some(updates) = self.status_poller.try_recv_updates() {
            for update in updates {
                self.apply_one_status_update(update);
            }
            self.pending_status_refresh = false;
            return true;
        }
        false
    }

    /// Apply a single status update from the poller. Extracted from the
    /// channel-pulling loop in `apply_status_updates` so tests can drive
    /// the apply path directly without having to push through the
    /// background polling thread.
    pub(super) fn apply_one_status_update(&mut self, update: StatusUpdate) {
        self.apply_status_update(update, true, true);
    }

    pub(super) fn apply_status_updates_without_hooks(&mut self, updates: Vec<StatusUpdate>) {
        for update in updates {
            self.apply_status_update(update, false, false);
        }
    }

    pub(super) fn reset_status_refresh(&mut self) {
        self.status_poller = StatusPoller::new();
        self.pending_status_refresh = false;
    }

    fn apply_status_update(&mut self, update: StatusUpdate, play_sound: bool, run_hooks: bool) {
        use crate::session::Status;

        let old_status = self.get_instance(&update.id).map(|i| i.status);
        let should_update = old_status.is_some_and(|s| {
            s != Status::Deleting
                && s != Status::Creating
                && s != Status::Stopped
                && update.status != Status::Stopped
        });

        let new_last_accessed = update.last_accessed_at;
        let new_pane_dead = update.pane_dead;

        if should_update {
            let new_status = update.status;
            let new_error = update.last_error;
            let new_idle_entered_at = update.idle_entered_at;
            self.mutate_instance(&update.id, |inst| {
                inst.status = new_status;
                inst.last_error = new_error;
                // Propagate the timestamp the polling clone wrote;
                // see StatusPoller for why this isn't a simple
                // `inst.idle_entered_at = …` from inside the poll.
                inst.idle_entered_at = new_idle_entered_at;
                if new_last_accessed.is_some() {
                    inst.last_accessed_at = new_last_accessed;
                }
                inst.pane_dead_observed = new_pane_dead;
            });

            if let Some(old) = old_status {
                if old != new_status {
                    if let Some(inst) = self.get_instance(&update.id).cloned() {
                        self.handle_status_transition(
                            &inst, old, new_status, play_sound, run_hooks,
                        );
                    }
                }
            }
        } else if new_last_accessed.is_some() {
            self.mutate_instance(&update.id, |inst| {
                inst.last_accessed_at = new_last_accessed;
                inst.pane_dead_observed = new_pane_dead;
            });
        } else {
            // No status change AND no fresh activity stamp. We still
            // need to refresh pane_dead_observed: a corpse can sit
            // unchanged for hours and the sort tier should reflect
            // current reality. Cheap mutate (one bool write).
            self.mutate_instance(&update.id, |inst| {
                inst.pane_dead_observed = new_pane_dead;
            });
        }
    }

    fn handle_status_transition(
        &self,
        inst: &Instance,
        old: crate::session::Status,
        new: crate::session::Status,
        play_sound: bool,
        run_hooks: bool,
    ) {
        if play_sound {
            crate::sound::play_for_transition(old, new, &self.sound_config);
        }
        if run_hooks {
            let hook_config = self.status_hook_config_for(inst);
            crate::status_hooks::run_for_transition(inst, old, new, &hook_config);
        }
    }

    fn status_hook_config_for(&self, inst: &Instance) -> crate::status_hooks::StatusHookConfig {
        if self.active_profile.is_some() {
            return self.status_hook_config.clone();
        }
        let profile = inst.effective_profile();
        self.status_hook_configs
            .get(&profile)
            .cloned()
            .unwrap_or_else(|| self.status_hook_config.clone())
    }

    pub fn apply_deletion_results(&mut self) -> bool {
        use crate::session::Status;

        if let Some(result) = self.deletion_poller.try_recv_result() {
            if result.success {
                self.remove_instance(&result.session_id);
                self.rebuild_group_trees();

                if let Err(e) = self.save() {
                    tracing::error!(target: "tui.home", "Failed to save after deletion: {}", e);
                }
                if let Err(e) = self.reload() {
                    tracing::warn!(target: "tui.home", "Failed to reload session state: {e}");
                }
            } else {
                let error = if result.errors.is_empty() {
                    None
                } else {
                    Some(result.errors.join("; "))
                };
                self.mutate_instance(&result.session_id, |inst| {
                    inst.status = Status::Error;
                    inst.last_error = error;
                });
            }
            return true;
        }
        false
    }

    /// Apply the result of a background stop. Returns true if an instance was
    /// updated so the caller can trigger a redraw.
    pub fn apply_stop_results(&mut self) -> bool {
        use crate::session::Status;

        if let Some(result) = self.stop_poller.try_recv_result() {
            if result.success {
                // Status was already set to Stopped optimistically when the
                // stop was requested; reassert it in case the disk reload or
                // a race changed it, and clear any stale error.
                self.set_instance_error(&result.session_id, None);
                self.set_instance_status(&result.session_id, Status::Stopped);
            } else {
                self.set_instance_error(&result.session_id, result.error);
                self.set_instance_status(&result.session_id, Status::Error);
            }
            if let Err(e) = self.save() {
                tracing::error!(target: "tui.home", "Failed to save after stop: {}", e);
            }
            return true;
        }
        false
    }

    /// Apply any pending session ID updates from background pollers.
    /// Returns true if any instance's in-memory `agent_session_id` changed.
    /// Tmux env may also be republished when this returns `false`
    /// (filtered or Failed paths republish the memory mirror).
    pub fn apply_session_id_updates(&mut self) -> bool {
        let mut updates: Vec<(String, String, Option<String>)> = Vec::new();
        let mut filtered_ids: HashSet<String> = HashSet::new();

        for inst in &self.instances {
            if let Some((_id, session_id)) = inst
                .session_id_poller
                .as_ref()
                .and_then(|p| p.lock().ok())
                .and_then(|p| p.try_recv_session_update())
            {
                let Some(session_id) = crate::session::capture::validated_session_id(session_id)
                else {
                    // `on_change` already published this raw sid to env;
                    // republish the memory mirror to overwrite it.
                    filtered_ids.insert(inst.id.clone());
                    continue;
                };
                // Defense-in-depth against the resume-fallback cascade: a sid
                // the cascade just cleared can still live on disk for several
                // minutes (opencode db, vibe meta.json, codex/gemini/pi/hermes
                // state). The poller closures filter via `compose_exclusion`,
                // but if a closure factory ever forgets to thread the per-
                // instance excludes, this guard prevents the cleared sid from
                // being re-imported into memory and disk.
                if inst.retroactive_capture_excludes.contains(&session_id) {
                    tracing::debug!(
                        target: "tui.home",
                        "Ignoring poller-reported sid {} for {}: in retroactive_capture_excludes",
                        session_id,
                        inst.id,
                    );
                    filtered_ids.insert(inst.id.clone());
                    continue;
                }
                if inst.agent_session_id.as_deref() != Some(session_id.as_str()) {
                    let expected_prior = inst.agent_session_id.clone();
                    updates.push((inst.id.clone(), session_id, expected_prior));
                }
                continue;
            }
        }

        if updates.is_empty() && filtered_ids.is_empty() {
            return false;
        }

        let mut to_apply: Vec<(String, String)> = Vec::new();
        let mut to_rollback: Vec<(String, Option<String>)> = Vec::new();

        for (id, session_id, expected_prior) in &updates {
            let Some(profile) = self.instance_map.get(id).map(|i| i.source_profile.clone()) else {
                continue;
            };
            match crate::session::persist_session_to_storage(
                &profile,
                id,
                session_id,
                expected_prior.as_deref(),
                &self.file_watch,
            ) {
                crate::session::SidWrite::Applied => {
                    to_apply.push((id.clone(), session_id.clone()));
                }
                crate::session::SidWrite::Skipped => {
                    let mut reloaded = false;
                    if let Ok(storage) =
                        crate::session::Storage::new(&profile, self.file_watch.clone())
                    {
                        if let Ok(disk_insts) = storage.load() {
                            if let Some(disk_inst) = disk_insts.iter().find(|i| i.id == *id) {
                                to_rollback.push((id.clone(), disk_inst.agent_session_id.clone()));
                                reloaded = true;
                            }
                        }
                    }
                    if !reloaded {
                        // Memory is known stale (Skipped CAS proved
                        // memory != disk) and we cannot read disk.
                        // Leave env at the poller's last write; the next
                        // poller event reconciles.
                        tracing::warn!(target: "tui.home",
                            instance = %id,
                            "Skipped reload failed; deferring env reconcile");
                    }
                }
                crate::session::SidWrite::Failed => {
                    // `on_change` published an unvalidated sid; republish memory.
                    filtered_ids.insert(id.clone());
                }
            }
        }

        for (id, session_id) in &to_apply {
            self.mutate_instance(id, |inst| {
                inst.agent_session_id = Some(session_id.clone());
            });
        }
        for (id, disk_sid) in &to_rollback {
            let disk_sid = disk_sid.clone();
            self.mutate_instance(id, |inst| {
                inst.agent_session_id = disk_sid.clone();
            });
        }

        let touched_ids: Vec<&str> = to_apply
            .iter()
            .map(|(id, _)| id.as_str())
            .chain(to_rollback.iter().map(|(id, _)| id.as_str()))
            .chain(filtered_ids.iter().map(|s| s.as_str()))
            .collect();
        let mut set_batch: Vec<(String, String, String)> = Vec::new();
        let mut unset_batch: Vec<(String, String)> = Vec::new();
        for id in &touched_ids {
            let Some(inst) = self.instance_map.get(*id) else {
                continue;
            };
            // `s.exists()` reads a 2s-TTL cache; tests bypassing
            // `Session::create` must call `refresh_session_cache()`.
            let tmux_name = match inst.tmux_session() {
                Ok(s) if s.exists() && !s.is_pane_dead() => s.name().to_string(),
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!(target: "tui.home",
                        instance = %id,
                        "Skipping tmux env publish; tmux_session() error: {}", e);
                    continue;
                }
            };
            match &inst.agent_session_id {
                Some(sid) => set_batch.push((
                    tmux_name,
                    crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                    sid.clone(),
                )),
                None => unset_batch.push((
                    tmux_name,
                    crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                )),
            }
        }
        if !set_batch.is_empty() {
            let refs: Vec<(&str, &str, &str)> = set_batch
                .iter()
                .map(|(s, k, v)| (s.as_str(), k.as_str(), v.as_str()))
                .collect();
            if let Err(e) = crate::tmux::env::set_hidden_env_batch(&refs) {
                tracing::warn!(target: "tui.home", "Post-CAS env publish failed: {}", e);
            }
        }
        if !unset_batch.is_empty() {
            let refs: Vec<(&str, &str)> = unset_batch
                .iter()
                .map(|(s, k)| (s.as_str(), k.as_str()))
                .collect();
            if let Err(e) = crate::tmux::env::remove_hidden_env_batch(&refs) {
                tracing::warn!(target: "tui.home", "Post-CAS env unset failed: {}", e);
            }
        }

        !to_apply.is_empty() || !to_rollback.is_empty()
    }

    /// Drain the startup-recovery channel and apply each `RecoveryUpdate`
    /// to the in-memory `Instance` snapshot. Released the recovery lock
    /// (and the receiver) when all workers have completed.
    ///
    /// Called from the `App::run` event-loop tick alongside
    /// `apply_session_id_updates`. Returns true if any instance was
    /// touched, so the caller can refresh the rendered tree.
    pub fn apply_recovery_updates(&mut self) -> bool {
        let Some(rx) = self.recovery_rx.as_ref() else {
            return false;
        };
        let mut touched = false;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(update) => {
                    let RecoveryUpdate {
                        instance_id,
                        title,
                        instance,
                        result,
                    } = update;
                    match result {
                        Ok(crate::session::StartOutcome::Resumed) => {
                            tracing::info!(
                                target: "session.startup_recovery",
                                id = %instance_id,
                                %title,
                                "resumed",
                            );
                        }
                        Ok(crate::session::StartOutcome::Restarted { stale_sid }) => {
                            tracing::warn!(
                                target: "session.startup_recovery",
                                id = %instance_id,
                                %title,
                                %stale_sid,
                                "restarted fresh after resume failure",
                            );
                        }
                        Ok(crate::session::StartOutcome::Fresh) => {}
                        Err(e) => {
                            tracing::warn!(
                                target: "session.startup_recovery",
                                id = %instance_id,
                                %title,
                                error = %e,
                                "recovery cascade failed",
                            );
                        }
                    }
                    // Drop the in-flight marker BEFORE replacing the
                    // snapshot so the next status poll sees the post-cascade
                    // instance through the normal pipeline.
                    self.recovery_in_flight.remove(&instance_id);
                    if let Some(slot) = self.instances.iter_mut().find(|i| i.id == instance_id) {
                        *slot = *instance;
                        touched = true;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            // All workers exited: drop the receiver and the lock so a
            // peer (a daemon that just started) can run recovery for any
            // session this TUI did not own. Clear the in-flight set
            // defensively in case a future early-return path bypassed
            // the per-id remove above.
            self.recovery_rx = None;
            self.recovery_lock = None;
            self.recovery_in_flight.clear();
        }
        if touched {
            self.instance_map = self
                .instances
                .iter()
                .map(|i| (i.id.clone(), i.clone()))
                .collect();
            // Preserve the selection across the rebuild. Without this, a
            // recovery completion that reorders rows under
            // `SortOrder::LastActivity` (the recovered session's
            // `last_start_time` shifted) would silently latch the
            // selection onto a neighbour because `update_selected()`
            // resolves through `flat_items[cursor]`. Mirrors the
            // canonical sequence in `reload()`.
            let prev_selected_session = self.selected_session.clone();
            let prev_selected_group = self.selected_group.clone();

            self.flat_items = self.build_flat_items();

            let mut restored = false;
            if let Some(ref sid) = prev_selected_session {
                for (idx, item) in self.flat_items.iter().enumerate() {
                    if let Item::Session { id, .. } = item {
                        if id == sid {
                            self.cursor = idx;
                            restored = true;
                            break;
                        }
                    }
                }
            } else if let Some(ref gpath) = prev_selected_group {
                for (idx, item) in self.flat_items.iter().enumerate() {
                    if let Item::Group { path, .. } = item {
                        if path == gpath {
                            self.cursor = idx;
                            restored = true;
                            break;
                        }
                    }
                }
            }
            if !restored && self.cursor >= self.flat_items.len() && !self.flat_items.is_empty() {
                self.cursor = self.flat_items.len() - 1;
            }

            if self.search_active && !self.search_query.value().is_empty() {
                self.update_search();
            } else if !self.search_matches.is_empty() {
                self.refresh_search_matches();
            }

            self.update_selected();
        }
        touched
    }

    /// Identify recovery candidates and spawn a worker pool. Sets
    /// `self.recovery_rx` to `Some(rx)` if at least one worker was spawned;
    /// otherwise leaves it `None` (the daemon owns recovery, the lock is
    /// contended, or there are no candidates).
    fn maybe_start_startup_recovery(&mut self) {
        // Requires a tokio runtime: each worker is `tokio::spawn`-ed below.
        // `HomeView::new` is sync and called from production via
        // `#[tokio::main]`, so the runtime is present at the real call site.
        // Unit tests construct `HomeView` directly without a runtime; today
        // they do not panic only because their test instances lack a valid
        // `agent_session_id` and `is_recovery_candidate` filters them out
        // before any spawn is attempted. This guard makes the function
        // resilient to a future test that constructs an instance with a
        // valid sid and no live tmux.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        // Defer to the daemon if one is running. The daemon's own
        // `daemon_startup_recovery` will handle the candidates from this
        // TUI's profile (and every other profile). Recovery split-brain
        // is the exact failure mode the file lock is meant to prevent;
        // checking `daemon_pid()` first short-circuits the more expensive
        // lock acquisition in the common case.
        #[cfg(feature = "serve")]
        if crate::cli::serve::daemon_pid().is_some() {
            return;
        }
        let lock = match crate::session::recovery::try_acquire_recovery_lock() {
            Ok(Some(l)) => l,
            Ok(None) => {
                tracing::info!(
                    target: "session.startup_recovery",
                    "another process holds the recovery lock; TUI skipping startup recovery",
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "session.startup_recovery",
                    error = %e,
                    "failed to acquire recovery lock; TUI skipping startup recovery",
                );
                return;
            }
        };

        let mut candidates: Vec<crate::session::Instance> = Vec::new();
        // Single fallible tmux probe instead of per-instance
        // `inst.has_live_tmux_pane()` calls. On Err: skip recovery this
        // launch (a transient tmux glitch must NOT collapse to "all panes
        // dead" and trigger phantom cascades). Bonus: one subprocess call
        // regardless of instance count (was 1-2 per instance).
        let pane_meta = match crate::tmux::batch_pane_metadata() {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(
                    target: "session.startup_recovery",
                    error = %e,
                    "tmux probe failed; TUI skipping startup recovery this launch",
                );
                drop(lock);
                return;
            }
        };
        for inst in &mut self.instances {
            let session_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let has_live_tmux = pane_meta
                .get(&session_name)
                .map(|m| !m.pane_dead)
                .unwrap_or(false);
            if has_live_tmux {
                continue;
            }
            if !crate::session::recovery::is_recovery_candidate(inst) {
                continue;
            }
            // Set Status::Starting AND last_start_time: the existing 3s
            // grace at `update_status_with_metadata_inner` only fires on
            // the latter, and without it the TUI's StatusPoller (every
            // 500ms) would observe missing tmux + no last_start_time and
            // immediately flip the status to `Error` before the worker
            // has finished its cascade.
            debug_assert!(inst.status != crate::session::Status::Creating);
            inst.status = crate::session::Status::Starting;
            inst.last_error = None;
            inst.last_start_time = Some(std::time::Instant::now());
            self.recovery_in_flight.insert(inst.id.clone());
            candidates.push(inst.clone());
        }

        if candidates.is_empty() {
            drop(lock);
            return;
        }

        crate::session::recovery::warm_tmux_server();

        tracing::info!(
            target: "session.startup_recovery",
            count = candidates.len(),
            "TUI starting recovery for missing tmux sessions",
        );

        let (tx, rx) = std::sync::mpsc::channel::<RecoveryUpdate>();
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
            crate::session::recovery::STARTUP_RECOVERY_CONCURRENCY,
        ));

        for inst in candidates {
            let tx = tx.clone();
            let permit_sem = semaphore.clone();
            tokio::spawn(async move {
                let _permit = permit_sem
                    .acquire_owned()
                    .await
                    .expect("recovery semaphore not closed");
                let id = inst.id.clone();
                let title = inst.title.clone();
                let inst_pre_panic = inst.clone();
                let mut working = inst;
                let result = tokio::task::spawn_blocking(move || {
                    let res = crate::session::recovery::run_recovery_for_instance(&mut working);
                    (working, res)
                })
                .await;
                let update = match result {
                    Ok((updated, Ok(outcome))) => RecoveryUpdate {
                        instance_id: id,
                        title,
                        instance: Box::new(updated),
                        result: Ok(outcome),
                    },
                    Ok((updated, Err(e))) => RecoveryUpdate {
                        instance_id: id,
                        title,
                        instance: Box::new(updated),
                        result: Err(e.to_string()),
                    },
                    Err(join_err) => {
                        tracing::error!(
                            target: "session.startup_recovery",
                            id = %id,
                            error = %join_err,
                            "recovery worker panicked",
                        );
                        // Surface the panic as a synthetic error update so
                        // `apply_recovery_updates` clears `recovery_in_flight`
                        // and the user sees Status::Error with a useful
                        // last_error instead of an instance stuck in
                        // `Status::Starting` until HomeView drops.
                        let mut recovered = inst_pre_panic;
                        recovered.status = crate::session::Status::Error;
                        recovered.last_error =
                            Some(format!("recovery worker panicked: {}", join_err));
                        RecoveryUpdate {
                            instance_id: id,
                            title,
                            instance: Box::new(recovered),
                            result: Err(format!("worker panicked: {}", join_err)),
                        }
                    }
                };
                let _ = tx.send(update);
            });
        }

        self.recovery_rx = Some(rx);
        self.recovery_lock = Some(lock);
    }

    /// Request background session creation. Used for sandbox sessions to avoid blocking UI.
    /// Creates a stub instance in the session list with Status::Creating so the user
    /// can see progress in the preview pane while continuing to use the TUI.
    pub fn request_creation(
        &mut self,
        mut data: NewSessionData,
        hooks: Option<crate::session::HooksConfig>,
    ) {
        // Pre-resolve the title using the same logic the builder will run, so the
        // stub instance, the background creation, and the eventual real instance
        // all agree on the title (otherwise an empty title would show as the path
        // basename in the stub but a civilization name in the final instance).
        if data.title.is_empty() {
            let existing_titles: Vec<&str> = self
                .instances()
                .iter()
                .filter(|i| i.source_profile == data.profile)
                .map(|i| i.title.as_str())
                .collect();
            data.title = crate::session::builder::resolve_title(
                &data.title,
                data.worktree_branch.as_deref(),
                data.worktree_enabled,
                &existing_titles,
            );
        }
        let stub_title = data.title.clone();
        let mut stub = Instance::new(&stub_title, &data.path);
        stub.tool = if data.tool.is_empty() {
            "claude".to_string()
        } else {
            data.tool.clone()
        };
        stub.group_path = data.group.clone();
        stub.status = crate::session::Status::Creating;
        stub.yolo_mode = data.yolo_mode;
        stub.source_profile = data.profile.clone();

        // Set stub worktree_info so project-mode grouping works during creation.
        // The real worktree_info (with resolved main_repo_path) replaces this
        // once build_instance completes.
        let stub_branch = data
            .worktree_branch
            .as_deref()
            .filter(|b| !b.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                data.worktree_enabled
                    .then(|| crate::session::builder::branch_name_from_title(&stub_title))
            });
        if let Some(branch) = stub_branch {
            stub.worktree_info = Some(crate::session::WorktreeInfo {
                branch,
                main_repo_path: data.path.clone(),
                managed_by_aoe: false,
                created_at: chrono::Utc::now(),
                base_branch: data.base_branch.clone(),
            });
        }

        let stub_id = stub.id.clone();
        let target_profile = data.profile.clone();

        // Add stub to instance list
        self.add_instance(stub);
        self.rebuild_group_trees();
        if !data.group.is_empty() {
            if let Some(tree) = self.group_trees.get_mut(&target_profile) {
                tree.create_group(&data.group);
            }
        }

        // Initialize progress tracking and select the stub
        self.creating_hook_progress.insert(
            stub_id.clone(),
            CreatingHookProgress {
                hook_output: Vec::new(),
                current_hook: None,
            },
        );
        self.creating_stub_id = Some(stub_id.clone());
        self.flat_items = self.build_flat_items();

        // Move cursor to the new stub
        if let Some(pos) = self
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == &stub_id))
        {
            self.cursor = pos;
            self.update_selected();
        }

        // Close the dialog
        self.new_dialog = None;

        self.creation_cancelled = false;
        // Filter out the stub from existing instances so the builder doesn't
        // treat its placeholder title as a duplicate to auto-increment.
        let existing_instances: Vec<Instance> = self
            .instances
            .iter()
            .filter(|i| i.id != stub_id)
            .cloned()
            .collect();
        let request = CreationRequest {
            data,
            existing_instances,
            hooks,
        };
        self.creation_poller.request_creation(request);
    }

    /// Mark the current creation operation as cancelled
    pub fn cancel_creation(&mut self) {
        if self.creation_poller.is_pending() {
            self.creation_cancelled = true;
        }
        // Remove the stub instance
        if let Some(stub_id) = self.creating_stub_id.take() {
            self.remove_instance(&stub_id);
            self.creating_hook_progress.remove(&stub_id);
            self.rebuild_group_trees();
            self.flat_items = self.build_flat_items();
            self.update_selected();
        }
        self.new_dialog = None;
    }

    /// Apply any pending creation results from the background poller.
    /// Returns Some(session_id) if creation succeeded and we should attach.
    pub fn apply_creation_results(&mut self) -> Option<String> {
        use super::creation_poller::CreationResult;
        use crate::session::builder::{self, CreatedWorktree};
        use std::path::PathBuf;

        let result = self.creation_poller.try_recv_result()?;

        // Clean up the stub and progress tracking
        let stub_id = self.creating_stub_id.take();
        if let Some(ref id) = stub_id {
            self.creating_hook_progress.remove(id);
        }

        // Check if the user cancelled while waiting
        if self.creation_cancelled {
            self.creation_cancelled = false;
            if let Some(id) = &stub_id {
                self.remove_instance(id);
            }
            if let CreationResult::Success {
                ref instance,
                ref created_worktree,
                ..
            } = result
            {
                let worktree = created_worktree.as_ref().map(|wt| CreatedWorktree {
                    path: PathBuf::from(&wt.path),
                    main_repo_path: PathBuf::from(&wt.main_repo_path),
                });
                builder::cleanup_instance(instance, worktree.as_ref(), &[]);
            }
            self.rebuild_group_trees();
            self.flat_items = self.build_flat_items();
            self.update_selected();
            return None;
        }

        match result {
            CreationResult::Success {
                session_id,
                instance,
                on_launch_hooks_ran,
                warnings,
                ..
            } => {
                // Remove the stub instance
                if let Some(id) = &stub_id {
                    self.remove_instance(id);
                }

                let mut instance = *instance;
                let target_profile = self.creation_poller.last_profile().unwrap_or_else(|| {
                    self.active_profile
                        .clone()
                        .unwrap_or_else(crate::session::config::resolve_default_profile)
                });
                instance.source_profile = target_profile.clone();

                // Ensure target profile storage exists
                if !self.storages.contains_key(&target_profile) {
                    if let Ok(s) = Storage::new(&target_profile, self.file_watch.clone()) {
                        self.storages.insert(target_profile.clone(), s);
                    }
                }

                self.add_instance(instance.clone());
                self.rebuild_group_trees();
                if !instance.group_path.is_empty() {
                    if let Some(tree) = self.group_trees.get_mut(&target_profile) {
                        tree.create_group(&instance.group_path);
                    }
                }

                if let Err(e) = self.save() {
                    tracing::error!(target: "tui.home", "Failed to save after creation: {}", e);
                }

                if on_launch_hooks_ran {
                    self.on_launch_hooks_ran.insert(session_id.clone());
                }

                if let Err(e) = self.reload() {
                    tracing::warn!(target: "tui.home", "Failed to reload session state: {e}");
                }
                // reload()'s restore-previous-selection fallback lands
                // the cursor on whichever flat_items index is closest
                // to the now-removed stub, which in project-grouped
                // layouts is often the new session's group folder.
                // Pin selection onto the new session directly so the
                // preview pane and dispatch in app.rs see the right
                // row.
                self.select_and_reveal_session(&session_id);
                self.new_dialog = None;

                if !warnings.is_empty() {
                    let body = warnings.join("\n\n");
                    let message = format!(
                        "Session was created, but the following warnings were emitted during setup:\n\n{}",
                        body
                    );
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Session warnings", &message));
                }

                Some(session_id)
            }
            CreationResult::Error(error) => {
                // Remove the stub and show the error in an info dialog
                if let Some(id) = &stub_id {
                    self.remove_instance(id);
                    self.rebuild_group_trees();
                    self.flat_items = self.build_flat_items();
                    self.update_selected();
                    // Hook failures carry multi-line output; size to fit so
                    // the actual error isn't clipped at the default 50x9.
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &error));
                } else if let Some(dialog) = &mut self.new_dialog {
                    dialog.set_loading(false);
                    dialog.set_error(error);
                }
                None
            }
        }
    }

    /// Check if on_launch hooks already ran for this session (and consume the flag).
    pub fn take_on_launch_hooks_ran(&mut self, session_id: &str) -> bool {
        self.on_launch_hooks_ran.remove(session_id)
    }

    /// Check if there's a pending creation operation
    pub fn is_creation_pending(&self) -> bool {
        self.creation_poller.is_pending()
    }

    /// Check if the currently selected session is the in-flight creating stub
    pub fn is_creating_stub_selected(&self) -> bool {
        match (&self.creating_stub_id, &self.selected_session) {
            (Some(stub_id), Some(selected)) => stub_id == selected,
            _ => false,
        }
    }

    /// Show a confirmation dialog warning that a session is being created.
    pub fn show_quit_during_creation_confirm(&mut self) {
        self.confirm_dialog = Some(ConfirmDialog::new(
            "Session Creating",
            "A session is still being created. Quit anyway? The hook will be cancelled.",
            "quit_during_creation",
        ));
    }

    /// Whether `q` on the home screen should confirm before quitting.
    pub fn confirm_before_quit(&self) -> bool {
        self.confirm_before_quit
    }

    /// Show the "quit aoe?" confirmation, with a "don't warn me again"
    /// checkbox that flips `confirm_before_quit` off when ticked (#1569).
    pub fn show_quit_confirm(&mut self) {
        self.confirm_dialog = Some(
            ConfirmDialog::new(
                "Quit Agent of Empires",
                "Quit?\nYour sessions persist in the background.",
                "quit",
            )
            .neutral()
            .offering_dont_ask_again(),
        );
    }

    /// Persist `confirm_before_quit = false` and update the cached flag so
    /// the quit confirmation stops appearing. Called when the user ticks
    /// "don't warn me again" in the quit dialog.
    pub(super) fn disable_confirm_before_quit(&mut self) {
        self.confirm_before_quit = false;
        match load_config() {
            Ok(Some(mut config)) => {
                config.session.confirm_before_quit = false;
                if let Err(e) = save_config(&config) {
                    tracing::warn!(target: "tui.home", "Failed to save config: {e}");
                }
            }
            Ok(None) => {
                let mut config = crate::session::config::Config::default();
                config.session.confirm_before_quit = false;
                if let Err(e) = save_config(&config) {
                    tracing::warn!(target: "tui.home", "Failed to save config: {e}");
                }
            }
            Err(e) => {
                tracing::warn!(target: "tui.home", "Failed to load config: {e}");
            }
        }
    }

    /// Clean up a pending creation on TUI shutdown. Waits briefly for the
    /// background thread to finish so we can clean up worktrees/instances.
    /// If the thread doesn't finish in time, the hook subprocess will
    /// complete on its own and orphaned Creating stubs are cleaned up on
    /// next launch.
    pub fn cleanup_pending_creation(&mut self) {
        if !self.creation_poller.is_pending() {
            return;
        }
        self.creation_cancelled = true;
        if let Some(stub_id) = self.creating_stub_id.take() {
            self.remove_instance(&stub_id);
            self.creating_hook_progress.remove(&stub_id);
        }

        // Wait briefly for the background thread to finish
        let result = self
            .creation_poller
            .recv_result_timeout(std::time::Duration::from_secs(2));

        if let Some(crate::tui::creation_poller::CreationResult::Success {
            ref instance,
            ref created_worktree,
            ..
        }) = result
        {
            let worktree =
                created_worktree
                    .as_ref()
                    .map(|wt| crate::session::builder::CreatedWorktree {
                        path: std::path::PathBuf::from(&wt.path),
                        main_repo_path: std::path::PathBuf::from(&wt.main_repo_path),
                    });
            crate::session::builder::cleanup_instance(instance, worktree.as_ref(), &[]);
            tracing::info!(target: "tui.home", "Cleaned up cancelled session on exit");
        }
    }

    /// Tick dialog animations/timers and drain hook progress.
    /// Returns true when a redraw is needed.
    pub fn tick_dialog(&mut self) -> bool {
        use crate::session::repo_config::HookProgress;

        let mut changed = false;

        if let Some(dialog) = &mut self.new_dialog {
            if dialog.tick() {
                changed = true;
            }

            if dialog.is_loading() {
                // Drain all pending hook progress messages
                while let Some(progress) = self.creation_poller.try_recv_progress() {
                    dialog.push_hook_progress(progress);
                    changed = true;
                }
            }
        }

        // Poll serve dialog for subprocess startup events.
        #[cfg(feature = "serve")]
        if let Some(view) = &mut self.serve_view {
            if view.tick() {
                changed = true;
            }
        }

        // Drain hook progress into the creating buffer when no dialog is open
        if self.new_dialog.is_none() {
            if let Some(ref stub_id) = self.creating_stub_id {
                let stub_id = stub_id.clone();
                if let Some(progress_buf) = self.creating_hook_progress.get_mut(&stub_id) {
                    while let Some(progress) = self.creation_poller.try_recv_progress() {
                        match progress {
                            HookProgress::Started(cmd) => {
                                progress_buf.current_hook = Some(cmd);
                            }
                            HookProgress::Output(line) => {
                                progress_buf.hook_output.push(line);
                                // Cap buffer to prevent unbounded memory growth
                                if progress_buf.hook_output.len() > 1000 {
                                    progress_buf.hook_output.drain(..500);
                                }
                            }
                        }
                        changed = true;
                    }
                }
            }
        }

        changed
    }

    /// Whether the user is currently looking at a surface where they're
    /// likely to want to copy text (URLs, error messages, release notes).
    /// The App uses this to release xterm mouse capture so the terminal's
    /// native drag-to-select works without a modifier; mouse capture comes
    /// back as soon as the surface is dismissed.
    ///
    /// Add new dialogs here only when their content is meant to be copied,
    /// not for every modal: capture toggling has a small but visible cost
    /// (the wheel-scroll on the dashboard preview won't work while it's
    /// off).
    pub fn wants_text_selection(&self) -> bool {
        #[cfg(feature = "serve")]
        let serve_open = self.serve_view.is_some();
        #[cfg(not(feature = "serve"))]
        let serve_open = false;

        serve_open
            || self.info_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self
                .intro_dialog
                .as_ref()
                .is_some_and(|d| d.wants_text_selection())
    }

    /// Same membership as `has_dialog()` minus live-send. Two callers:
    ///
    /// - List-row click routing: clicks must keep working in live mode
    ///   (that's how the user switches the live target by clicking another
    ///   row), but every other modal surface should still freeze the list.
    /// - Preview-only fast path gate (`App::draw_preview_only`): the fast
    ///   path is exactly what live-send wants, so live-send itself can't
    ///   gate it off; any OTHER overlay does, since the fast path repaints
    ///   the snapshot underneath and only re-renders the preview pane.
    ///
    /// `has_dialog()` ORs `live_send.is_some()` on top, so it would also
    /// gate off the fast path it's supposed to enable — that's why the
    /// fast path needs this method instead.
    pub(in crate::tui) fn has_non_live_send_overlay(&self) -> bool {
        #[cfg(feature = "serve")]
        let serve_open = self.serve_view.is_some();
        #[cfg(not(feature = "serve"))]
        let serve_open = false;

        self.show_help
            || self.search_active
            || self.new_dialog.is_some()
            || self.confirm_dialog.is_some()
            || self.unified_delete_dialog.is_some()
            || self.group_delete_options_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.worktree_name_dialog.is_some()
            || self.restart_dialog.is_some()
            || self.context_menu.is_some()
            || self.repo_trust_dialog.is_some()
            || self.hooks_install_dialog.is_some()
            || self.intro_dialog.is_some()
            || self.no_agents_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self.info_dialog.is_some()
            || self.snooze_duration_dialog.is_some()
            || self.profile_picker_dialog.is_some()
            || self.project_session_picker_dialog.is_some()
            || self.projects_dialog.is_some()
            || self.command_palette.is_some()
            || self.tool_picker_dialog.is_some()
            || self.send_message_dialog.is_some()
            || self.update_confirm_dialog.is_some()
            || self.telemetry_consent_dialog.is_some()
            || serve_open
            || self.settings_view.is_some()
            || self.diff_view.is_some()
    }

    pub fn has_dialog(&self) -> bool {
        #[cfg(feature = "serve")]
        let serve_open = self.serve_view.is_some();
        #[cfg(not(feature = "serve"))]
        let serve_open = false;

        self.live_send.is_some()
            || self.show_help
            || self.search_active
            || self.new_dialog.is_some()
            || self.confirm_dialog.is_some()
            || self.unified_delete_dialog.is_some()
            || self.group_delete_options_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.worktree_name_dialog.is_some()
            || self.restart_dialog.is_some()
            || self.context_menu.is_some()
            || self.repo_trust_dialog.is_some()
            || self.hooks_install_dialog.is_some()
            || self.intro_dialog.is_some()
            || self.no_agents_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self.info_dialog.is_some()
            || self.snooze_duration_dialog.is_some()
            || self.profile_picker_dialog.is_some()
            || self.project_session_picker_dialog.is_some()
            || self.projects_dialog.is_some()
            || self.command_palette.is_some()
            || self.tool_picker_dialog.is_some()
            || self.send_message_dialog.is_some()
            || self.update_confirm_dialog.is_some()
            || self.telemetry_consent_dialog.is_some()
            || serve_open
            || self.settings_view.is_some()
            || self.diff_view.is_some()
    }

    /// Whether the paste-burst detector should fire for incoming key events.
    ///
    /// The detector exists to solve the home-view shortcut-shadowing problem:
    /// Mosh strips bracketed-paste markers, so a pasted stream of `KeyCode::Char`
    /// events would fire `n`/`d`/`r`/etc. shortcuts on the home view. When a
    /// dialog captures keys into a text input, those shortcuts don't fire —
    /// but the dialog also won't receive a synthesized `Paste` event unless
    /// it routes through `handle_paste`. Bursting through a dialog that only
    /// handles `Key` events strands the text in `pending_paste` and leaves
    /// the dialog's input empty.
    ///
    /// So: burst is safe when no dialog is open (home shortcuts at risk) or
    /// when one of the four paste-routed dialogs is open (rename / send_message
    /// / new / settings — each forwards to `handle_paste`). For every other
    /// dialog (command palette, profile picker, projects, info, etc.) keys
    /// must dispatch individually so the dialog input receives them.
    pub fn wants_paste_burst(&self) -> bool {
        if !self.has_dialog() {
            return true;
        }
        // Live-send mode is also paste-aware: handle_paste forwards
        // the chunk straight to the pane via the control-mode worker,
        // which is strictly faster and safer than letting the chars
        // fan out as individual KeyEvents and stream per-char tmux
        // commands.
        self.live_send.is_some()
            || self.rename_dialog.is_some()
            || self.send_message_dialog.is_some()
            || self.new_dialog.is_some()
            || self.settings_view.is_some()
    }

    pub fn shrink_list(&mut self) {
        self.list_width = self.list_width.saturating_sub(5).max(10);
        self.save_list_width();
    }

    pub fn grow_list(&mut self) {
        self.list_width = (self.list_width + 5).min(80);
        self.save_list_width();
    }

    /// Hide or reveal the session list so the preview pane can use the
    /// full terminal width. Only meaningful while live mode is active
    /// (the render path ignores the flag otherwise and `exit_live_send_*`
    /// resets it), so this is deliberately ephemeral rather than persisted
    /// to config. The next render reflows the preview, and the live-send
    /// resize loop pushes the new geometry to the agent's pane.
    pub fn toggle_sidebar_collapsed(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
    }

    fn save_list_width(&self) {
        if let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) {
            config.app_state.home_list_width = Some(self.list_width);
            if let Err(e) = save_config(&config) {
                tracing::warn!(target: "tui.home", "Failed to save config: {e}");
            }
        }
    }

    pub fn toggle_preview_info(&mut self) {
        self.show_preview_info = !self.show_preview_info;
        if let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) {
            config.app_state.show_preview_info = Some(self.show_preview_info);
            if let Err(e) = save_config(&config) {
                tracing::warn!(target: "tui.home", "Failed to save config: {e}");
            }
        }
    }

    /// Forget the last non-live preview resize so the next render re-asserts the
    /// preview geometry. Call whenever the agent window's real size changes out
    /// from under the preview (an attach grows it to the client; entering or
    /// leaving live mode hands the resize off and back).
    pub(super) fn clear_preview_pane_sync(&mut self) {
        self.preview_pane_synced = None;
    }

    /// Expand the synthetic Archived section if it is collapsed, persisting
    /// the change. Used when archiving a session in the non-Attention sort so
    /// the freshly archived row is visible and can stay selected under the
    /// cursor. No-op (and no save) when the section is already open.
    pub(super) fn reveal_archived_section(&mut self) {
        if !self.archived_section_collapsed {
            return;
        }
        self.archived_section_collapsed = false;
        if let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) {
            config.app_state.archived_section_collapsed = Some(false);
            if let Err(e) = save_config(&config) {
                tracing::warn!(target: "tui.home", "Failed to save config: {e}");
            }
        }
    }

    pub fn toggle_archived_section(&mut self) {
        self.archived_section_collapsed = !self.archived_section_collapsed;
        if let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) {
            config.app_state.archived_section_collapsed = Some(self.archived_section_collapsed);
            if let Err(e) = save_config(&config) {
                tracing::warn!(target: "tui.home", "Failed to save config: {e}");
            }
        }
        self.flat_items = self.build_flat_items();
        // Defensive cursor clamp + selection refresh. Today the only
        // call site routes through `toggle_group_collapsed` after the
        // cursor lands on the section header, and the header survives
        // the rebuild at the same end-of-list index, so the cursor stays
        // valid. Programmatic callers (palette command, future macros)
        // wouldn't have that invariant, so clamp here rather than rely
        // on every caller to know about it.
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
    }

    pub fn show_intro(&mut self, current_theme: &str) {
        tracing::info!(target: "tui.dialog", dialog = "intro", "opening");
        self.intro_dialog = Some(IntroDialog::new(current_theme));
    }

    pub fn show_no_agents(&mut self) {
        tracing::info!(target: "tui.dialog", dialog = "no_agents", "opening");
        self.no_agents_dialog = Some(NoAgentsDialog::new());
    }

    /// Replace available tools (used after re-check from no-agents dialog).
    pub fn set_available_tools(&mut self, tools: AvailableTools) {
        tracing::debug!(target: "tui.home", count = tools.available_list().len(), "available tools refreshed");
        self.available_tools = tools;
    }

    pub fn show_changelog(&mut self, from_version: Option<String>) {
        tracing::info!(
            target: "tui.dialog",
            dialog = "changelog",
            from_version = ?from_version,
            "opening",
        );
        self.changelog_dialog = Some(ChangelogDialog::new(from_version));
    }

    pub fn show_telemetry_consent(&mut self) {
        tracing::info!(target: "tui.dialog", dialog = "telemetry_consent", "opening");
        self.telemetry_consent_dialog = Some(super::dialogs::TelemetryConsentDialog::new());
    }

    pub fn instances(&self) -> &[Instance] {
        &self.instances
    }

    pub fn get_instance(&self, id: &str) -> Option<&Instance> {
        self.instance_map.get(id)
    }

    /// Returns true if any session has an animated status (Running, Waiting, Starting,
    /// Creating), which means the TUI needs periodic redraws for spinner animation.
    pub fn has_animated_sessions(&self) -> bool {
        use crate::session::Status;
        self.instances.iter().any(|inst| {
            matches!(
                inst.status,
                Status::Running | Status::Waiting | Status::Starting | Status::Creating
            )
        })
    }

    pub(super) fn build_flat_items(&self) -> Vec<Item> {
        // Project grouping is honored across every sort order. Combined with
        // Attention sort, sessions sort by tier within each project and the
        // project headers float by their top-attention member (driven by
        // sort_groups + attention_group_key in flatten_tree). Check this
        // first so Project + Attention doesn't fall through to the flat
        // Attention branch and lose the project headers.
        if self.group_by == GroupByMode::Project {
            return self.build_flat_items_by_project();
        }

        // Manual grouping + Attention sort is the cross-cutting flat
        // priority view: skip groups entirely so Waiting/Error rows from
        // different groups can interleave by tier instead of being walled
        // off behind group headers. Project grouping above opts into a
        // different shape on purpose (attention triage within explicit
        // project boundaries).
        if self.sort_order == SortOrder::Attention {
            let filtered: Vec<Instance> = if let Some(profile) = &self.active_profile {
                self.instances
                    .iter()
                    .filter(|i| i.source_profile == *profile)
                    .cloned()
                    .collect()
            } else {
                self.instances.clone()
            };
            let mut items = flatten_sessions_by_attention(&filtered);
            append_archived_section(&mut items, &filtered, self.archived_section_collapsed);
            return items;
        }

        let (mut items, archive_pool) = if let Some(profile) = &self.active_profile {
            let filtered: Vec<Instance> = self
                .instances
                .iter()
                .filter(|i| i.source_profile == *profile)
                .cloned()
                .collect();
            let items = match self.group_trees.get(profile) {
                Some(tree) => flatten_tree(tree, &filtered, self.sort_order),
                None => Vec::new(),
            };
            (items, filtered)
        } else if self.storages.len() <= 1 {
            let items = match self.group_trees.values().next() {
                Some(tree) => flatten_tree(tree, &self.instances, self.sort_order),
                None => Vec::new(),
            };
            (items, self.instances.clone())
        } else {
            let items =
                flatten_tree_all_profiles(&self.instances, &self.group_trees, self.sort_order);
            (items, self.instances.clone())
        };

        // Pin the synthetic Archived section to the bottom regardless of
        // sort order. Archived rows were filtered out of the natural flow
        // inside `flatten_tree` / `flatten_tree_all_profiles`.
        append_archived_section(&mut items, &archive_pool, self.archived_section_collapsed);
        items
    }

    fn build_flat_items_by_project(&self) -> Vec<Item> {
        // In project mode, always merge all sessions into one tree regardless of
        // profile count. Project grouping unifies by repo across profiles.
        let base_instances: Vec<Instance> = if let Some(profile) = &self.active_profile {
            self.instances
                .iter()
                .filter(|i| i.source_profile == *profile)
                .cloned()
                .collect()
        } else {
            self.instances.clone()
        };

        let grouped: Vec<Instance> = base_instances
            .into_iter()
            .map(|mut inst| {
                inst.group_path = project_group_name(&inst);
                inst
            })
            .collect();

        // Project headers are derived purely from the live sessions, not a
        // persisted group list, so build the tree from non-archived members
        // only. An archived session already shows under the synthetic
        // Archived section (nested by project below); if it also seeded a
        // project node here, a project whose only remaining member is
        // archived would render an empty phantom header in the main flow.
        // That header is undeletable in project mode ("Project groups are
        // automatic"), leaving the user no way to clear it.
        let tree_seed: Vec<Instance> = grouped
            .iter()
            .filter(|i| !i.is_archived())
            .cloned()
            .collect();
        let mut tree = GroupTree::new_with_groups(&tree_seed, &[]);
        for (path, &collapsed) in &self.project_group_collapsed {
            if collapsed {
                tree.set_collapsed(path, true);
            }
        }
        let mut items = flatten_tree(&tree, &grouped, self.sort_order);
        append_archived_section_by_project(
            &mut items,
            &grouped,
            self.archived_section_collapsed,
            &self.project_group_collapsed,
            self.sort_order,
        );
        items
    }

    /// The active profile filter name, or `None` when no filter is applied.
    /// Returning `None` lets callers (e.g. the list-pane title) omit the
    /// `[<profile>]` segment entirely instead of rendering a noisy `[all]`.
    pub fn active_profile_display(&self) -> Option<&str> {
        self.active_profile.as_deref()
    }

    /// Switch the active profile filter in-place without destroying the view.
    /// Pass `None` for all-profiles mode, or `Some(name)` to filter to one profile.
    pub fn switch_profile(&mut self, new_profile: Option<String>) -> anyhow::Result<()> {
        self.active_profile = new_profile;
        if let Some(profile) = self.active_profile.clone() {
            if !self.storages.contains_key(&profile) {
                self.storages.insert(
                    profile.clone(),
                    Storage::new(&profile, self.file_watch.clone())?,
                );
            }
            self.storages.retain(|name, _| name == &profile);
            self.rewire_disk_subscriptions(std::slice::from_ref(&profile))?;
        }
        // Clear selection before reload so stale session/group refs don't linger
        self.selected_session = None;
        self.selected_group = None;
        self.selected_group_profile = None;
        self.reload()?;
        self.refresh_from_config();
        // Invalidate preview caches since the visible sessions changed
        self.preview_cache = PreviewCache::default();
        self.terminal_preview_cache = PreviewCache::default();
        self.container_terminal_preview_cache = PreviewCache::default();
        self.tool_preview_cache = PreviewCache::default();
        self.preview_scroll_offset = 0;
        // Clear search since match indices are invalid with new flat_items
        if self.search_active {
            self.search_active = false;
            self.search_query = Input::default();
            self.search_matches.clear();
            self.search_match_index = 0;
        }
        Ok(())
    }

    /// Show the profile picker dialog with fresh data from disk.
    pub(super) fn show_profile_picker(&mut self) {
        use crate::session::list_profiles;
        use crate::tui::dialogs::{ProfileEntry, ProfilePickerDialog};

        let current_profile = self
            .active_profile
            .clone()
            .unwrap_or_else(|| "all".to_string());
        let profiles = list_profiles()
            .unwrap_or_else(|_| vec![crate::session::config::resolve_default_profile()]);
        let mut entries: Vec<ProfileEntry> = profiles
            .iter()
            .map(|name| {
                let session_count = Storage::new(name, self.file_watch.clone())
                    .and_then(|s| s.load())
                    .map(|instances| instances.len())
                    .unwrap_or(0);
                ProfileEntry {
                    name: name.clone(),
                    session_count,
                    is_active: self.active_profile.as_deref() == Some(name.as_str()),
                }
            })
            .collect();

        // In filtered mode, add "all" entry at top
        if self.active_profile.is_some() {
            let total: usize = entries.iter().map(|e| e.session_count).sum();
            entries.insert(
                0,
                ProfileEntry {
                    name: "all".to_string(),
                    session_count: total,
                    is_active: false,
                },
            );
        }

        self.profile_picker_dialog = Some(ProfilePickerDialog::new(entries, &current_profile));
    }

    /// Show the group-by picker dialog seeded with the current mode.
    pub(super) fn show_group_picker(&mut self) {
        self.group_picker_dialog = Some(GroupPickerDialog::new(self.group_by));
    }

    /// Open the saved-project picker that starts a new session pre-filled with
    /// the chosen project's path. Shows an info dialog when no projects exist.
    pub(super) fn open_project_session_picker(&mut self) {
        let profile = self.config_profile();
        let projects = crate::session::projects::load_merged(&profile).unwrap_or_default();
        if projects.is_empty() {
            self.info_dialog = Some(InfoDialog::new(
                "No Projects",
                "No registered projects available. Add one with `aoe project add <path>`.",
            ));
            return;
        }
        self.project_session_picker_dialog = Some(ProjectSessionPickerDialog::new(projects));
    }

    /// Show the sort-order picker dialog seeded with the current order.
    pub(super) fn show_sort_picker(&mut self) {
        self.sort_picker_dialog = Some(SortPickerDialog::new(self.sort_order));
    }

    pub fn set_instance_status(&mut self, id: &str, status: crate::session::Status) {
        let old_status = self.get_instance(id).map(|inst| inst.status);
        self.mutate_instance(id, |inst| inst.status = status);
        if let Some(old) = old_status {
            if old != status {
                if let Some(inst) = self.get_instance(id).cloned() {
                    self.handle_status_transition(&inst, old, status, false, true);
                }
            }
        }
    }

    /// Stamp `last_accessed_at` on a session (user-initiated interaction).
    ///
    /// Sunk rows (archived or snoozed) take the heavier `apply_user_action`
    /// path so the auto-unarchive/unsnooze side effect in `touch_last_accessed`
    /// is persisted (merge_from_tui doesn't carry those fields; without this,
    /// reload would resurrect the sink from disk) and the row leaves the
    /// Archived section visually on the same frame. Non-sunk rows stay on
    /// the cheap mutate_instance path; their only mutation is the timestamp,
    /// which save() already mirrors via merge_from_tui.
    pub fn stamp_last_accessed(&mut self, id: &str) {
        let was_sunk = self
            .instance_map
            .get(id)
            .map(|i| i.is_archived() || i.snoozed_until.is_some())
            .unwrap_or(false);
        if was_sunk {
            if let Err(e) = self.apply_user_action(id, |inst| inst.touch_last_accessed()) {
                tracing::warn!(
                    target: "tui.home",
                    session_id = %id,
                    error = %e,
                    "stamp_last_accessed: failed to persist auto-unsink"
                );
            }
            self.flat_items = self.build_flat_items();
        } else {
            self.mutate_instance(id, |inst| inst.touch_last_accessed());
        }
    }

    /// Run the send-message work after the dialog has been dismissed: call
    /// `ensure_pane_ready` (which may auto-start or respawn), then deliver
    /// the keystrokes. Errors are surfaced via `info_dialog` so the caller
    /// (`execute_action`) only has to clear its transient status.
    ///
    /// Returns `Some(stale_sid)` when the resume-fallback cascade fired
    /// during the implicit respawn so the caller can toast the user about
    /// the lost history; `None` otherwise.
    pub fn execute_send_message(&mut self, session_id: &str, message: &str) -> Option<String> {
        let target = std::mem::replace(
            &mut self.pending_send_target,
            live_send::LiveSendTarget::Agent,
        );
        let size = crate::terminal::get_size();
        // Same pane-readiness cascades as live-send: agent runs the
        // full `ensure_pane_ready` (Docker, splash, resume); terminals
        // just need their tmux session to exist with a live pane.
        let stale_sid = match target {
            live_send::LiveSendTarget::Agent => {
                let outcome = self.try_mutate_instance_writeback_on_err(session_id, |inst| {
                    inst.ensure_pane_ready().map_err(Into::into)
                });
                match outcome {
                    Ok(Some(EnsureReadyOutcome::Respawned {
                        stale_sid: Some(sid),
                    }))
                    | Ok(Some(EnsureReadyOutcome::Started {
                        stale_sid: Some(sid),
                    })) => Some(sid),
                    Ok(_) => None,
                    Err(err) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Send Failed",
                            &format!("Cannot prepare session: {}", err),
                        ));
                        return None;
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => {
                if let Err(e) = self.ensure_terminal_pane_ready(session_id, size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Send Failed",
                        &format!("Cannot prepare terminal: {}", e),
                    ));
                    return None;
                }
                None
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                if let Err(e) = self.ensure_container_terminal_pane_ready(session_id, size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Send Failed",
                        &format!("Cannot prepare container terminal: {}", e),
                    ));
                    return None;
                }
                None
            }
        };
        let inst = self.get_instance(session_id)?;
        let tmux_session = match target {
            live_send::LiveSendTarget::Agent => {
                match crate::tmux::Session::new(&inst.id, &inst.title) {
                    Ok(s) => s,
                    Err(e) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Send Failed",
                            &format!("Failed to resolve session: {}", e),
                        ));
                        return None;
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => crate::tmux::Session::from_name(
                &crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title),
            ),
            live_send::LiveSendTarget::ContainerTerminal => crate::tmux::Session::from_name(
                &crate::tmux::ContainerTerminalSession::generate_name(&inst.id, &inst.title),
            ),
        };
        // Agent gets a tool-specific Enter delay so paste-burst-aware
        // agents (e.g. Codex) don't swallow the final Enter. Shells in
        // the paired terminal panes don't need the delay.
        let delay = match target {
            live_send::LiveSendTarget::Agent => crate::agents::send_keys_enter_delay(&inst.tool),
            live_send::LiveSendTarget::Terminal | live_send::LiveSendTarget::ContainerTerminal => 0,
        };
        if let Err(e) = tmux_session.send_keys_with_delay(message, delay) {
            self.info_dialog = Some(InfoDialog::new(
                "Send Failed",
                &format!("Failed to send message: {}", e),
            ));
            return None;
        }
        self.stamp_last_accessed(session_id);
        if let Err(e) = self.save() {
            tracing::error!("Failed to save after send: {}", e);
        }
        if self.sort_order == crate::session::config::SortOrder::Attention {
            self.select_top_attention(None);
            self.selected_session = None;
        }
        stale_sid
    }

    /// Stage live-send mode against `session_id`. Mirrors
    /// `execute_send_message`'s revive cascade so a cold-start (Docker
    /// pull, agent splash) is handled before the user starts typing,
    /// then installs `live_send` state so subsequent keystrokes are
    /// captured by `handle_live_send_key`.
    ///
    /// Geometry-sensitive work is intentionally split out into
    /// `finalize_live_send_resize`: the caller is expected to settle
    /// any toast/banner state (which can shift `preview_pane_area` by a
    /// row) and redraw between `prepare_live_send` and
    /// `finalize_live_send_resize`, so the sync resize targets the
    /// geometry the user will actually see for the next several frames.
    /// Without that split, the "Reviving session..." toast shown during
    /// this slow phase made `preview_pane_area` one row shorter than
    /// the post-toast frame, and the agent's first capture rendered
    /// shifted up.
    ///
    /// Returns `Ok(Some(stale_sid))` when the resume-fallback cascade
    /// fired during respawn, `Ok(None)` on a clean ready, and `Err(())`
    /// if the pane could not be readied (`info_dialog` is set with the
    /// underlying error so the caller only has to clear its toast).
    pub fn prepare_live_send(&mut self, session_id: &str) -> Result<Option<String>, ()> {
        let target = self.pending_live_send_target;
        self.pending_live_send_target = live_send::LiveSendTarget::Agent;
        let size = crate::terminal::get_size();
        // Agent targets revive the agent pane via the full
        // ensure_pane_ready cascade (Docker, splash, resume). Terminal
        // targets are simpler: the paired terminal is a plain shell,
        // so we just ensure the tmux session exists and re-spawn it if
        // the pane has died (matches `attach_terminal`).
        let stale_sid = match target {
            live_send::LiveSendTarget::Agent => {
                let outcome = self.try_mutate_instance_writeback_on_err(session_id, |inst| {
                    inst.ensure_pane_ready().map_err(Into::into)
                });
                match outcome {
                    Ok(Some(EnsureReadyOutcome::Respawned {
                        stale_sid: Some(sid),
                    }))
                    | Ok(Some(EnsureReadyOutcome::Started {
                        stale_sid: Some(sid),
                    })) => Some(sid),
                    Ok(_) => None,
                    Err(err) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Live send failed",
                            &format!("Cannot prepare session: {}", err),
                        ));
                        return Err(());
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => {
                if let Err(e) = self.ensure_terminal_pane_ready(session_id, size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Live send failed",
                        &format!("Cannot prepare terminal: {}", e),
                    ));
                    return Err(());
                }
                None
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                if let Err(e) = self.ensure_container_terminal_pane_ready(session_id, size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Live send failed",
                        &format!("Cannot prepare container terminal: {}", e),
                    ));
                    return Err(());
                }
                None
            }
        };
        let inst = match self.get_instance(session_id) {
            Some(inst) => inst.clone(),
            None => {
                // Defensive: ensure_pane_ready succeeded but the
                // instance is gone (deleted by a peer process between
                // those two calls). Without a dialog the user would
                // press Tab and see nothing happen, with no clue why.
                self.info_dialog = Some(InfoDialog::new(
                    "Live send failed",
                    "Session disappeared before live mode could start.",
                ));
                return Err(());
            }
        };
        // Resolve the tmux session name up front so the worker thread
        // can reconstruct a Session without re-touching HomeView.
        let tmux_name = match target {
            live_send::LiveSendTarget::Agent => {
                match crate::tmux::Session::new(&inst.id, &inst.title) {
                    Ok(s) => s.name().to_string(),
                    Err(e) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Live send failed",
                            &format!("Cannot resolve tmux session: {}", e),
                        ));
                        return Err(());
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => {
                crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title)
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                crate::tmux::ContainerTerminalSession::generate_name(&inst.id, &inst.title)
            }
        };
        // Switching live mode from session A to session B (click on a
        // different row while already live): we need to drop the old
        // worker BEFORE resetting the old session's window-size,
        // otherwise any `Resize` still queued in the old worker can
        // fire after the reset and flip the old pane back to manual
        // sizing. The worker thread is intentionally not joined, so
        // dropping its `Sender` is the only way to know its dispatch
        // loop has finished (its `recv` returns Err and the thread
        // exits on the next iteration).
        let prev_tmux_name = self
            .live_send
            .as_ref()
            .map(|state| state.tmux_name.clone())
            .filter(|name| name != &tmux_name);
        if prev_tmux_name.is_some() {
            // Drop worker first so its queued resizes (if any) drain
            // against the old session before we reset its sizing.
            self.live_send_worker = None;
            // The capture worker is retargeted by the render reconcile, not
            // here; but drop the previous session's cached previews so the
            // first frames after the switch don't paint session A's content
            // under session B's header while B's capture worker spins up.
            // (The synchronous path got this for free via its cross-session
            // kill-switch branch; the worker path applies content lazily,
            // so clear it explicitly here.) All targets are cleared because
            // a live-send switch can retarget to Terminal / ContainerTerminal
            // too, and the view can be flipped to any of them right after.
            self.preview_cache = PreviewCache::default();
            self.terminal_preview_cache = PreviewCache::default();
            self.container_terminal_preview_cache = PreviewCache::default();
            self.tool_preview_cache = PreviewCache::default();
            if let Some(name) = &prev_tmux_name {
                crate::tmux::Session::from_name(name).reset_size_to_latest_client();
            }
        }
        // Parse the configured exit-chord list now so the per-keystroke
        // dispatch path doesn't re-parse on every event. Config edits
        // during live mode aren't possible (settings_view participates
        // in has_dialog and lives in its own takeover), so a snapshot
        // at entry time is sufficient.
        let resolved_config = resolve_config_or_warn(&self.config_profile());
        let exit_chord_spec = resolved_config.session.live_send_exit_chord;
        let exit_chords = live_send::parse_chord_list(&exit_chord_spec);
        // The leader is a single chord, not a list. An empty configured
        // value disables it (so every key, including the default `C-b`,
        // passes straight through). A non-empty but unparseable value is
        // treated as a typo and falls back to the default leader rather
        // than silently dropping the feature, mirroring how the exit
        // chord recovers from a bad spec.
        let leader_spec = resolved_config.session.live_send_leader;
        let leader = if leader_spec.trim().is_empty() {
            None
        } else {
            live_send::parse_chord(&leader_spec).or_else(|| {
                tracing::warn!(
                    "live-send: unparseable leader chord '{}'; falling back to default '{}'",
                    leader_spec,
                    live_send::DEFAULT_LEADER
                );
                live_send::parse_chord(live_send::DEFAULT_LEADER)
            })
        };
        self.live_send = Some(live_send::LiveSendState {
            session_id: inst.id.clone(),
            title: inst.title.clone(),
            tmux_name: tmux_name.clone(),
            target,
            exit_chords,
            leader,
        });
        // Ensure the long-lived preview capture worker exists so we can hand
        // its waker to the send worker below. The worker isn't otherwise
        // spawned here (it follows the displayed pane for every view, not
        // just agent live-send, and is (re)targeted and retuned by
        // `sync_preview_capture_worker` on the next render); but it's already
        // running whenever a session was previewed before live-send entry,
        // which is the common path. Spawning it now closes the rare cold gap.
        if self.preview_capture_worker.is_none() {
            self.preview_capture_worker = Some(live_send::LiveCaptureWorker::spawn(
                self.preview_wake.clone(),
            ));
        }
        // Nudge the capture worker right after each dispatched keystroke
        // batch so typed echo is captured immediately instead of waiting up
        // to a full fast-cadence cycle. This keeps echo latency tied to
        // actual input rather than the background capture phase.
        let capture_wake = self
            .preview_capture_worker
            .as_ref()
            .map(live_send::LiveCaptureWorker::waker);
        // Spawn the background worker that dispatches translated
        // keystrokes as one-shot `tmux send-keys` subprocesses (the
        // pre-#1485 path; control-mode was tried as an optimization
        // but turned out to be unreliable on real-world tmux setups
        // and was removed in favor of this simpler model).
        self.live_send_worker = Some(live_send::LiveSendWorker::spawn(tmux_name, capture_wake));
        // Start every live-mode entry (including a switch from another
        // session) with a disarmed leader menu, so a half-entered chord
        // can't carry over from a prior target.
        self.live_send_pending_leader = false;
        // Clear the resize dedup so `finalize_live_send_resize` always
        // issues its sync resize, even if the cached geometry from a
        // prior session happens to match the current preview_pane_area.
        self.live_send_last_resize = None;
        // Live mode takes over the pane's size from here; drop the non-live
        // preview dedup so exiting re-asserts the preview geometry cleanly.
        self.preview_pane_synced = None;
        self.stamp_last_accessed(session_id);
        Ok(stale_sid)
    }

    /// Synchronously resize the live-send pane to match `self.preview_pane_area`,
    /// then block for ~50 ms so the agent has time to handle SIGWINCH and
    /// re-lay out before the next preview capture.
    ///
    /// Must be called after `prepare_live_send` returns `Ok(_)` and after
    /// the caller has redrawn the frame in the post-toast geometry the
    /// user will see for the next several frames. See `prepare_live_send`
    /// for why the two are split.
    ///
    /// `preview_pane_area` is the cached OUTPUT sub-rect: the full inner
    /// (after border + padding) minus the info header AND minus the
    /// inner ` Output ` / ` Terminal Output ` banner row when the user
    /// has the header expanded. Sizing to the full inner instead would
    /// leave the top `info_height + 1` rows of the agent's output
    /// outside the visible window; tail-clip semantics in the preview's
    /// `Paragraph` render then drop those rows on every frame, which
    /// the user perceives as content shifted up. The math is shared
    /// with the per-frame resize in `refresh_preview_cache_if_needed`
    /// and friends; the rect comes from `preview::PreviewLayout::compute`.
    pub fn finalize_live_send_resize(&mut self) {
        let Some(state) = self.live_send.as_ref() else {
            return;
        };
        let tmux_name = state.tmux_name.clone();
        let pane = self.preview_pane_area;
        if pane.width == 0 || pane.height == 0 {
            return;
        }
        let resize_status = std::process::Command::new("tmux")
            .args([
                "resize-window",
                "-t",
                &tmux_name,
                "-x",
                &pane.width.to_string(),
                "-y",
                &pane.height.to_string(),
            ])
            .stderr(std::process::Stdio::null())
            .status();
        // Only register the dedup if the resize subprocess actually
        // succeeded. If tmux failed (session died between our state
        // install and now, tmux binary missing, etc.), leaving
        // `live_send_last_resize` as None lets the next
        // `refresh_preview_cache_if_needed` try the resize again
        // through the worker.
        if matches!(&resize_status, Ok(s) if s.success()) {
            self.live_send_last_resize = Some((pane.width, pane.height));
        }
        // Give the agent ~50ms to handle SIGWINCH and re-lay out
        // before we capture the first frame. Some agents (claude-
        // code in particular) do a full clear-screen + redraw on
        // resize; capturing during that produces a partial frame.
        // 50ms is the smallest delay that empirically lets the
        // most-common agents settle.
        //
        // Wrap the sleep in `block_in_place` so the tokio
        // multi-threaded runtime can reschedule any other tasks
        // off this worker for the duration. Without it, the 50ms
        // would block every other tokio task (status pollers,
        // update checks, etc.) from running on this thread. The
        // call is a no-op on a current-thread runtime; aoe
        // always uses multi-threaded (`#[tokio::main]`).
        tokio::task::block_in_place(|| {
            std::thread::sleep(std::time::Duration::from_millis(50));
        });
    }

    pub fn save(&mut self) -> anyhow::Result<()> {
        let mut all_peer_deleted: Vec<String> = Vec::new();

        for (profile_name, storage) in &self.storages {
            let tui_rows: Vec<Instance> = self
                .instances
                .iter()
                .filter(|i| i.source_profile == *profile_name)
                .cloned()
                .collect();
            let dels: HashSet<String> = self
                .pending_deletions
                .get(profile_name)
                .cloned()
                .unwrap_or_default();
            let added: HashSet<String> = self
                .pending_added
                .get(profile_name)
                .cloned()
                .unwrap_or_default();
            let group_dels: HashSet<String> = self
                .pending_group_deletions
                .get(profile_name)
                .cloned()
                .unwrap_or_default();
            let groups_target = self
                .group_trees
                .get(profile_name)
                .map(|t| t.get_all_groups())
                .unwrap_or_default();

            let peer_deleted: Vec<String> = storage.update(|disk_instances, disk_groups| {
                disk_instances.retain(|d| !dels.contains(&d.id));
                let mut peer_deleted: Vec<String> = Vec::new();
                for tui_inst in &tui_rows {
                    if let Some(disk_inst) = disk_instances.iter_mut().find(|d| d.id == tui_inst.id)
                    {
                        disk_inst.merge_from_tui(tui_inst);
                    } else if added.contains(&tui_inst.id) {
                        disk_instances.push(tui_inst.clone());
                    } else {
                        // Disk had no row with this id and we did not add it
                        // this session: a peer (CLI / aoe serve) removed it.
                        peer_deleted.push(tui_inst.id.clone());
                    }
                }
                disk_groups.retain(|g| !group_dels.contains(&g.path));
                for tui_g in &groups_target {
                    if let Some(disk_g) = disk_groups.iter_mut().find(|g| g.path == tui_g.path) {
                        disk_g.name = tui_g.name.clone();
                        disk_g.collapsed = tui_g.collapsed;
                        disk_g.archived_at = tui_g.archived_at;
                    } else {
                        disk_groups.push(tui_g.clone());
                    }
                }
                Ok(peer_deleted)
            })?;

            self.pending_deletions.remove(profile_name);
            self.pending_group_deletions.remove(profile_name);
            self.pending_added.remove(profile_name);
            all_peer_deleted.extend(peer_deleted);
        }

        if !all_peer_deleted.is_empty() {
            self.drop_peer_deleted_rows(&all_peer_deleted);
            tracing::info!(
                target: "tui.home",
                count = all_peer_deleted.len(),
                "Dropped peer-deleted rows from TUI mirror"
            );
        }
        Ok(())
    }

    /// Drop in-memory mirror rows that no longer exist on disk (peer-deleted
    /// via CLI / aoe serve). Rebuilds derived UI state so callers don't
    /// render or target removed rows.
    fn drop_peer_deleted_rows(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let drop: HashSet<&String> = ids.iter().collect();
        self.instances.retain(|i| !drop.contains(&i.id));
        for id in ids {
            self.instance_map.remove(id);
        }
        if self
            .selected_session
            .as_ref()
            .is_some_and(|s| drop.contains(s))
        {
            self.selected_session = None;
        }
        self.rebuild_group_trees();
        self.flat_items = self.build_flat_items();
        if self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len().saturating_sub(1);
        }
    }

    /// Rebuild all per-profile GroupTrees from the current instances,
    /// preserving each tree's collapsed state.
    pub(super) fn rebuild_group_trees(&mut self) {
        for (profile_name, tree) in &mut self.group_trees {
            let existing_groups = tree.get_all_groups();
            let profile_instances: Vec<Instance> = self
                .instances
                .iter()
                .filter(|i| i.source_profile == *profile_name)
                .cloned()
                .collect();
            *tree = GroupTree::new_with_groups(&profile_instances, &existing_groups);
        }
    }

    /// Drop `group_path` from `profile`'s tree when no remaining session in
    /// that profile sits at the path or anywhere underneath it AND the path
    /// carries no user-anchored descendant group. Used after a session moves
    /// to a different profile: without this, the source profile keeps an
    /// empty group header that renders alongside the target profile's new
    /// copy of the same group, reading as a duplicate. Delegates to
    /// `delete_group_in_profile` so the deletion is tombstoned for `save()`
    /// and survives the next reload.
    pub(super) fn prune_empty_group(&mut self, profile: &str, group_path: &str) {
        if group_path.is_empty() {
            return;
        }
        let prefix = format!("{}/", group_path);
        let still_used = self.instances.iter().any(|i| {
            i.source_profile == profile
                && (i.group_path == group_path || i.group_path.starts_with(&prefix))
        });
        if still_used {
            return;
        }
        // Preserve hand-built structure: if the tree carries a descendant
        // group (e.g. user-anchored `work/anchor`) under this path, leave
        // the parent alone. The duplicate-header in unified view is the
        // lesser evil compared to nuking the user's hierarchy.
        let has_descendant_group = self.group_trees.get(profile).is_some_and(|tree| {
            tree.get_all_groups()
                .iter()
                .any(|g| g.path.starts_with(&prefix))
        });
        if has_descendant_group {
            return;
        }
        self.delete_group_in_profile(profile, group_path);
    }

    /// Determine which profile the item at the given cursor position belongs to.
    pub(super) fn profile_for_cursor(&self, cursor: usize) -> Option<String> {
        if let Some(profile) = &self.active_profile {
            return Some(profile.clone());
        }
        if let Some(item) = self.flat_items.get(cursor) {
            match item {
                crate::session::Item::Session { id, .. } => {
                    return self
                        .get_instance(id.as_str())
                        .map(|i| i.source_profile.clone());
                }
                crate::session::Item::Group { profile, path, .. } => {
                    if let Some(p) = profile {
                        return Some(p.clone());
                    }
                    // Fallback for single-profile mode: find any instance in this group
                    return self
                        .instances
                        .iter()
                        .find(|i| {
                            i.group_path == *path || i.group_path.starts_with(&format!("{}/", path))
                        })
                        .map(|i| i.source_profile.clone());
                }
            }
        }
        None
    }

    /// Collect all groups from all per-profile GroupTrees.
    pub(super) fn all_groups(&self) -> Vec<Group> {
        self.group_trees
            .values()
            .flat_map(|t| t.get_all_groups())
            .collect()
    }

    /// Check if any profile has groups, without collecting them all.
    pub(super) fn has_any_groups(&self) -> bool {
        self.group_trees
            .values()
            .any(|t| !t.get_all_groups().is_empty())
    }

    /// Centralized instance addition: adds to both the `instances` vec
    /// and `instance_map` to keep both collections in sync. Records the
    /// id in `pending_added` so the next `save` distinguishes TUI-new
    /// rows from peer-deleted ones (which look identical at the disk
    /// layer: missing from sessions.json).
    pub(super) fn add_instance(&mut self, instance: Instance) {
        self.pending_added
            .entry(instance.source_profile.clone())
            .or_default()
            .insert(instance.id.clone());
        self.instance_map
            .insert(instance.id.clone(), instance.clone());
        self.instances.push(instance);
    }

    /// Centralized instance removal: removes from both the `instances` vec
    /// and `instance_map`, records the id in `pending_deletions` so the
    /// next `save` propagates the removal under the flock, and clears any
    /// `pending_added` entry so an add+remove in the same save cycle does
    /// not end up persisted.
    pub(super) fn remove_instance(&mut self, id: &str) {
        if let Some(inst) = self.instance_map.get(id) {
            let profile = inst.source_profile.clone();
            self.pending_deletions
                .entry(profile.clone())
                .or_default()
                .insert(id.to_string());
            if let Some(set) = self.pending_added.get_mut(&profile) {
                set.remove(id);
            }
        }
        self.instances.retain(|i| i.id != id);
        self.instance_map.remove(id);
    }

    /// Tombstones `path` and every descendant from the per-profile tree so
    /// `save()` drops them under the flock instead of wholesale-replacing.
    pub(super) fn delete_group_in_profile(&mut self, profile: &str, path: &str) {
        let prefix = format!("{}/", path);
        let descendants: Vec<String> = self
            .group_trees
            .get(profile)
            .map(|tree| {
                tree.get_all_groups()
                    .into_iter()
                    .filter(|g| g.path == path || g.path.starts_with(&prefix))
                    .map(|g| g.path)
                    .collect()
            })
            .unwrap_or_else(|| vec![path.to_string()]);
        if let Some(tree) = self.group_trees.get_mut(profile) {
            tree.delete_group(path);
        }
        self.pending_group_deletions
            .entry(profile.to_string())
            .or_default()
            .extend(descendants);
    }

    /// Centralized instance mutation: applies `f` once to the `instances` vec
    /// entry, then clones the result into `instance_map`. This guarantees both
    /// collections stay in sync even for non-idempotent closures.
    pub(super) fn mutate_instance(&mut self, id: &str, f: impl FnOnce(&mut Instance)) {
        if let Some(inst) = self.instances.iter_mut().find(|i| i.id == id) {
            f(inst);
            self.instance_map.insert(id.to_string(), inst.clone());
        }
    }

    /// Cross-profile move: structurally distinct from `mutate_instance`
    /// because the row must be tombstoned in the old profile's disk file
    /// AND marked as TUI-new for the target profile. Without this, save()'s
    /// per-profile loop misclassifies the row as peer-deleted in the new
    /// profile and leaves the old profile's disk row, which next reload
    /// resurrects under the original profile.
    pub(super) fn move_to_profile(
        &mut self,
        id: &str,
        target: &str,
        new_group_path: String,
    ) -> anyhow::Result<()> {
        let Some(old_profile) = self.instance_map.get(id).map(|i| i.source_profile.clone()) else {
            return Ok(());
        };
        if old_profile == target {
            self.mutate_instance(id, |inst| inst.group_path = new_group_path);
            return Ok(());
        }

        if !self.storages.contains_key(target) {
            self.storages.insert(
                target.to_string(),
                Storage::new(target, self.file_watch.clone())?,
            );
        }

        self.pending_deletions
            .entry(old_profile.clone())
            .or_default()
            .insert(id.to_string());
        if let Some(set) = self.pending_added.get_mut(&old_profile) {
            set.remove(id);
        }
        self.pending_added
            .entry(target.to_string())
            .or_default()
            .insert(id.to_string());

        if let Some(inst) = self.instances.iter_mut().find(|i| i.id == id) {
            inst.group_path = new_group_path;
            inst.source_profile = target.to_string();
            self.instance_map.insert(id.to_string(), inst.clone());
        }
        Ok(())
    }

    /// Atomic per-action mutate: in-memory once, disk via
    /// `Instance::merge_user_action_diff` under the flock. On disk persist
    /// failure, in-memory is rolled back to `pre` so memory and disk stay
    /// consistent.
    pub(super) fn apply_user_action<F>(&mut self, id: &str, mutate: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut Instance),
    {
        let Some(profile) = self.instance_map.get(id).map(|i| i.source_profile.clone()) else {
            return Ok(());
        };
        let Some(in_mem) = self.instances.iter_mut().find(|i| i.id == id) else {
            return Ok(());
        };
        let pre = in_mem.clone();
        mutate(in_mem);
        let post = in_mem.clone();
        self.instance_map.insert(id.to_string(), post.clone());

        let id_owned = id.to_string();
        let res = if let Some(storage) = self.storages.get(&profile) {
            storage.update(|insts, _groups| {
                if let Some(disk) = insts.iter_mut().find(|i| i.id == id_owned) {
                    disk.merge_user_action_diff(&pre, &post);
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
        } else {
            tracing::warn!(
                target: "tui.home",
                profile = %profile,
                id = %id_owned,
                "apply_user_action: no storage registered for profile; in-memory mutation will not persist"
            );
            Ok(true)
        };
        match res {
            Ok(true) => Ok(()),
            Ok(false) => {
                let added = self
                    .pending_added
                    .get(&profile)
                    .is_some_and(|s| s.contains(id));
                if !added {
                    self.drop_peer_deleted_rows(&[id.to_string()]);
                }
                Ok(())
            }
            Err(e) => {
                if let Some(slot) = self.instances.iter_mut().find(|i| i.id == id) {
                    *slot = pre.clone();
                }
                self.instance_map.insert(id.to_string(), pre);
                Err(e)
            }
        }
    }

    /// Bulk `apply_user_action`: one `Storage::update` per affected
    /// profile (single flock cycle), grouping ids by `source_profile`.
    pub(super) fn bulk_apply_user_action<F>(
        &mut self,
        ids: &[String],
        mutate: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&mut Instance),
    {
        let mut by_profile: HashMap<String, Vec<(String, Instance, Instance)>> = HashMap::new();
        for id in ids {
            let Some(inst) = self.instances.iter_mut().find(|i| i.id == *id) else {
                continue;
            };
            let pre = inst.clone();
            mutate(inst);
            let post = inst.clone();
            self.instance_map.insert(id.clone(), post.clone());
            by_profile
                .entry(post.source_profile.clone())
                .or_default()
                .push((id.clone(), pre, post));
        }
        let mut peer_deleted: Vec<String> = Vec::new();
        for (profile, items) in &by_profile {
            let Some(storage) = self.storages.get(profile) else {
                tracing::warn!(
                    target: "tui.home",
                    profile = %profile,
                    count = items.len(),
                    "bulk_apply_user_action: no storage registered for profile; in-memory mutations will not persist"
                );
                continue;
            };
            let added: HashSet<String> =
                self.pending_added.get(profile).cloned().unwrap_or_default();
            let res = storage.update(|insts, _groups| {
                let mut missing: Vec<String> = Vec::new();
                for (id, pre, post) in items {
                    if let Some(disk) = insts.iter_mut().find(|i| i.id == *id) {
                        disk.merge_user_action_diff(pre, post);
                    } else if !added.contains(id) {
                        missing.push(id.clone());
                    }
                }
                Ok(missing)
            });
            match res {
                Ok(missing) => peer_deleted.extend(missing),
                Err(e) => {
                    for (id, pre, _post) in items {
                        if let Some(slot) = self.instances.iter_mut().find(|i| i.id == *id) {
                            *slot = pre.clone();
                        }
                        self.instance_map.insert(id.clone(), pre.clone());
                    }
                    return Err(e);
                }
            }
        }
        if !peer_deleted.is_empty() {
            self.drop_peer_deleted_rows(&peer_deleted);
        }
        Ok(())
    }

    /// Like `mutate_instance`, but for fallible operations. Clones the entry,
    /// applies `f` to the clone, and writes back to both collections only on
    /// success -- neither collection is modified on error.
    pub(super) fn try_mutate_instance<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut Instance) -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        if let Some(inst) = self.instances.iter_mut().find(|i| i.id == id) {
            let mut updated = inst.clone();
            let out = f(&mut updated)?;
            *inst = updated.clone();
            self.instance_map.insert(id.to_string(), updated);
            return Ok(Some(out));
        }
        Ok(None)
    }

    /// Like `try_mutate_instance`, but writes the mutated clone back even
    /// when `f` returns `Err`.
    ///
    /// Required for callers of `Instance::restart_with_size_opts` /
    /// `ensure_pane_ready`, because the resume-fallback cascade mutates
    /// `agent_session_id` and `retroactive_capture_excludes` BEFORE
    /// returning `Err` on Tier-2 failure. The default `try_mutate_instance`
    /// drops the mutated clone on `Err`, leaving the live entry with the
    /// stale sid in memory while disk has been cleared. Subsequent restarts
    /// then loop indefinitely on the same bad sid (the TUI's `reload()`
    /// merge prefers in-memory, so even the 5s disk refresh does not
    /// recover). This helper preserves the cascade's partial mutations so
    /// the live state stays consistent with disk.
    pub(super) fn try_mutate_instance_writeback_on_err<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut Instance) -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        if let Some(inst) = self.instances.iter_mut().find(|i| i.id == id) {
            let mut updated = inst.clone();
            let result = f(&mut updated);
            *inst = updated.clone();
            self.instance_map.insert(id.to_string(), updated);
            return result.map(Some);
        }
        Ok(None)
    }

    pub fn set_instance_error(&mut self, id: &str, error: Option<String>) {
        self.mutate_instance(id, |inst| inst.last_error = error);
    }

    pub fn start_terminal_for_instance_with_size(
        &mut self,
        id: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        self.try_mutate_instance(id, |inst| inst.start_terminal_with_size(size))?;
        self.save()?;
        Ok(())
    }

    /// Make sure the paired host-terminal tmux pane is alive and
    /// ready to receive keystrokes. Mirrors `attach_terminal`: if the
    /// session doesn't exist (or its pane has died), kill the
    /// tombstone and spawn a fresh one with the requested size. Used
    /// by `prepare_live_send` when the live target is the terminal.
    fn ensure_terminal_pane_ready(
        &mut self,
        session_id: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        let inst = self
            .get_instance(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", session_id))?
            .clone();
        let term = inst.terminal_tmux_session()?;
        if !term.exists() || term.is_pane_dead() {
            if term.exists() {
                let _ = term.kill();
            }
            self.start_terminal_for_instance_with_size(session_id, size)?;
        }
        Ok(())
    }

    /// Container-shell counterpart of `ensure_terminal_pane_ready`,
    /// used when the live-send target is the container terminal
    /// (sandboxed sessions in container terminal mode).
    fn ensure_container_terminal_pane_ready(
        &mut self,
        session_id: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        let inst = self
            .get_instance(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", session_id))?
            .clone();
        if !inst.is_sandboxed() {
            anyhow::bail!("Cannot prepare container terminal for non-sandboxed session");
        }
        let term = inst.container_terminal_tmux_session()?;
        if !term.exists() || term.is_pane_dead() {
            if term.exists() {
                let _ = term.kill();
            }
            self.start_container_terminal_for_instance_with_size(session_id, size)?;
        }
        Ok(())
    }

    pub fn restart_instance_with_size_opts(
        &mut self,
        id: &str,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) -> anyhow::Result<crate::session::StartOutcome> {
        let outcome = self.try_mutate_instance_writeback_on_err(id, |inst| {
            inst.restart_with_size_opts(size, skip_on_launch)
        })?;
        outcome.ok_or_else(|| anyhow::anyhow!("session not found: {}", id))
    }

    pub fn select_session_by_id(&mut self, session_id: &str) {
        for (idx, item) in self.flat_items.iter().enumerate() {
            if let Item::Session { id, .. } = item {
                if id == session_id {
                    self.cursor = idx;
                    self.update_selected();
                    return;
                }
            }
        }
    }

    pub fn sort_order(&self) -> SortOrder {
        self.sort_order
    }

    /// Move the cursor to the highest-priority session row, skipping
    /// `returning_id` if provided. Used after returning from an attach while
    /// sort_order=Attention: `stamp_last_accessed` bumps the returning session
    /// to the top of its tier, so picking row 0 blindly would leave the cursor
    /// on the session the user just handled. Skip it and land on the next
    /// session that actually needs attention. Falls back to the returning
    /// session itself if it's the only one in the list.
    pub fn select_top_attention(&mut self, returning_id: Option<&str>) {
        let mut fallback: Option<usize> = None;
        for (idx, item) in self.flat_items.iter().enumerate() {
            if let Item::Session { id, .. } = item {
                if returning_id.is_some_and(|r| r == id) {
                    fallback.get_or_insert(idx);
                    continue;
                }
                self.cursor = idx;
                self.update_selected();
                return;
            }
        }
        if let Some(idx) = fallback {
            self.cursor = idx;
            self.update_selected();
        }
    }

    /// Get the terminal mode for a session (uses config default if not set)
    pub fn get_terminal_mode(&self, session_id: &str) -> TerminalMode {
        self.terminal_modes
            .get(session_id)
            .copied()
            .unwrap_or(self.default_terminal_mode)
    }

    /// The profile whose config the view should resolve. The active profile
    /// when one is selected, otherwise (all-profiles mode) the user's default
    /// profile. Never an empty string and never a hard-coded name.
    pub(super) fn config_profile(&self) -> String {
        self.active_profile
            .clone()
            .unwrap_or_else(crate::session::config::resolve_default_profile)
    }

    /// Resolve the effective `SessionConfig` for an existing session
    /// row, honoring per-profile overrides. Reads the instance's
    /// `source_profile` so the picked config matches whatever profile
    /// the session was filed under (the home view's active profile may
    /// already have moved on); falls back to `config_profile()` when
    /// the instance has no recorded profile. Returns `None` for
    /// structured view-mode sessions because the attach-mode / click-action
    /// settings all have structured view-specific bypass paths upstream;
    /// callers treat `None` as "skip this setting, the structured view path
    /// handles activation."
    fn resolve_session_config_for(
        &self,
        session_id: &str,
    ) -> Option<crate::session::SessionConfig> {
        let inst = self.get_instance(session_id)?;
        if inst.is_structured() {
            return None;
        }
        let profile = if inst.source_profile.is_empty() {
            self.config_profile()
        } else {
            inst.source_profile.clone()
        };
        Some(crate::session::resolve_config_or_warn(&profile).session)
    }

    /// Whether renaming this session should also move its worktree directory
    /// leaf, per the resolved `session.tie_workdir_to_name` setting. True only
    /// for aoe-managed worktree sessions. Unlike `resolve_session_config_for`,
    /// this does not bypass structured-view sessions: the directory tie is
    /// orthogonal to the view. See #1927.
    pub(super) fn tie_workdir_applies_for(&self, session_id: &str) -> bool {
        let Some(inst) = self.get_instance(session_id) else {
            return false;
        };
        let profile = if inst.source_profile.is_empty() {
            self.config_profile()
        } else {
            inst.source_profile.clone()
        };
        let tie = crate::session::resolve_config_or_warn(&profile)
            .session
            .tie_workdir_to_name;
        inst.tie_workdir_applies(tie)
    }

    /// Resolve `new_session_attach_mode` for a freshly-created session.
    /// See `resolve_session_config_for` for the profile-resolution and
    /// structured view-bypass rules.
    pub fn new_session_attach_mode(
        &self,
        session_id: &str,
    ) -> Option<crate::session::NewSessionAttachMode> {
        self.resolve_session_config_for(session_id)
            .map(|s| s.new_session_attach_mode)
    }

    /// Resolve `click_action` for an existing session row when the
    /// user single-clicks it in the Structured view. See
    /// `resolve_session_config_for` for resolution rules; `None`
    /// (structured view) is treated by the caller as "fall through to the
    /// historical live-send path," which `start_live_send` itself
    /// short-circuits for structured view anyway.
    pub(super) fn click_action(&self, session_id: &str) -> Option<crate::session::ClickAction> {
        self.resolve_session_config_for(session_id)
            .map(|s| s.click_action)
    }

    /// Resolve `default_attach_mode` for an existing session row when
    /// the user activates it (Enter / double-click) in the Structured view.
    /// See `resolve_session_config_for` for resolution rules; callers
    /// short-circuit to the structured view-specific activation path before
    /// consulting this setting.
    pub(super) fn default_attach_mode(
        &self,
        session_id: &str,
    ) -> Option<crate::session::NewSessionAttachMode> {
        self.resolve_session_config_for(session_id)
            .map(|s| s.default_attach_mode)
    }

    /// True when Enter on the *currently selected session row* would
    /// enter live-send mode (and Tab would swap to a tmux attach).
    /// Returns `None` when the cursor is not on a session row (group or
    /// nothing selected) so the help overlay can fall back to a stable
    /// default rather than mislabel keys that don't apply. Honors per-
    /// profile overrides via `default_attach_mode(id)`.
    pub(super) fn help_live_on_enter(&self) -> Option<bool> {
        let id = self.selected_session.as_deref()?;
        let mode = self.default_attach_mode(id)?;
        Some(matches!(
            mode,
            crate::session::NewSessionAttachMode::LiveSend
        ))
    }

    /// Pin selection to `session_id` and place the cursor on its row.
    /// If the containing group is collapsed (manual grouping or
    /// project grouping), it's force-expanded and `flat_items` is
    /// rebuilt so the row is actually present before the cursor
    /// search. No-op when the session can't be resolved at all
    /// (deleted between caller and us): leaves the prior selection
    /// untouched so the user doesn't see the cursor leap to nowhere.
    ///
    /// Used by `apply_creation_results` so a freshly-created session
    /// becomes the visible cursor row; also a natural fit for any
    /// future "jump to session" path (command palette deep link,
    /// API-driven focus change) that wants the same reveal behavior.
    pub fn select_and_reveal_session(&mut self, session_id: &str) {
        let Some(inst) = self.get_instance(session_id) else {
            return;
        };
        let group_path = match self.group_by {
            GroupByMode::Project => Some(project_group_name(inst)),
            GroupByMode::Manual => {
                let p = inst.group_path.clone();
                if p.is_empty() {
                    None
                } else {
                    Some(p)
                }
            }
        };
        let target_profile = inst.source_profile.clone();
        self.selected_session = Some(session_id.to_string());
        self.selected_group = None;
        self.selected_group_profile = None;
        if let Some(gpath) = group_path {
            match self.group_by {
                GroupByMode::Project => {
                    self.project_group_collapsed.insert(gpath, false);
                }
                GroupByMode::Manual => {
                    if let Some(tree) = self.group_trees.get_mut(&target_profile) {
                        tree.set_collapsed(&gpath, false);
                    }
                }
            }
            self.flat_items = self.build_flat_items();
        }
        if let Some(pos) = self
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == session_id))
        {
            self.cursor = pos;
        }
    }

    /// Refresh all config-dependent state from the current profile's config.
    /// Call this after settings are saved to pick up any changes.
    pub fn refresh_from_config(&mut self) {
        let profile = self.config_profile();
        let config = resolve_config_or_warn(&profile);
        self.default_terminal_mode = match config.sandbox.default_terminal_mode {
            DefaultTerminalMode::Host => TerminalMode::Host,
            DefaultTerminalMode::Container => TerminalMode::Container,
        };
        self.sound_config = config.sound.clone();
        self.status_hook_config = config.status_hooks.clone();
        self.refresh_status_hook_config_cache();
        self.strict_hotkeys = config.session.strict_hotkeys;
        self.confirm_before_quit = config.session.confirm_before_quit;
        self.row_tag_mode = config.session.row_tag;
        self.profile_default_attach_mode = config.session.default_attach_mode;
        self.idle_decay_window =
            crate::tui::styles::idle_decay_window(config.theme.idle_decay_minutes);
        self.tool_configs = config.tools;
        self.tool_hotkey_cache = input::build_tool_hotkey_cache(&self.tool_configs);
        let hotkey_warnings = input::validate_tool_hotkeys(&self.tool_configs);
        if !hotkey_warnings.is_empty() && self.info_dialog.is_none() {
            self.info_dialog = Some(InfoDialog::new(
                "Tool hotkey config errors",
                &hotkey_warnings.join("\n"),
            ));
        }
    }

    fn status_hook_profile_names(
        active_profile: Option<&str>,
        storages: &HashMap<String, Storage>,
    ) -> Vec<String> {
        let mut profile_names = match active_profile {
            Some(profile) => vec![profile.to_string()],
            None => storages.keys().cloned().collect(),
        };
        // Make sure the user's default profile is always probed so its status
        // hooks load even when it currently has no sessions on disk.
        let default_profile = crate::session::config::resolve_default_profile();
        if !profile_names.contains(&default_profile) {
            profile_names.push(default_profile);
        }
        profile_names.sort();
        profile_names.dedup();
        profile_names
    }

    fn load_status_hook_configs(
        profile_names: Vec<String>,
    ) -> HashMap<String, crate::status_hooks::StatusHookConfig> {
        profile_names
            .into_iter()
            .map(|profile| {
                let status_hooks = resolve_config_or_warn(&profile).status_hooks;
                (profile, status_hooks)
            })
            .collect()
    }

    fn refresh_status_hook_config_cache(&mut self) {
        let profile_names =
            Self::status_hook_profile_names(self.active_profile.as_deref(), &self.storages);
        self.status_hook_configs = Self::load_status_hook_configs(profile_names);
        let profile = self.config_profile();
        if let Some(status_hooks) = self.status_hook_configs.get(&profile) {
            self.status_hook_config = status_hooks.clone();
        }
    }

    /// Toggle terminal mode between Container and Host for a session
    pub fn toggle_terminal_mode(&mut self, session_id: &str) {
        let current = self.get_terminal_mode(session_id);
        let new_mode = match current {
            TerminalMode::Container => TerminalMode::Host,
            TerminalMode::Host => TerminalMode::Container,
        };
        self.terminal_modes.insert(session_id.to_string(), new_mode);
    }

    pub fn start_container_terminal_for_instance_with_size(
        &mut self,
        id: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        self.try_mutate_instance(id, |inst| inst.start_container_terminal_with_size(size))
            .map(|_| ())
    }
}
