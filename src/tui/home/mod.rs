//! Home view - main session list and navigation

pub(crate) mod bindings;
mod input;
mod live_send;
mod operations;
pub(crate) mod render;

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
    append_archived_section, append_archived_section_by_project, append_trash_section,
    config::{load_config, update_app_state, update_config, GroupByMode, SortOrder},
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
use super::restart_poller::RestartPoller;
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

    crate::session::projects::repo_label(inst.repo_path())
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
    /// Grab-dragging the Settings fields-panel scrollbar. The live row is
    /// mapped to a scroll offset on each move (`scrollbar_drag_to_row`);
    /// there's no payload because the settings view owns the offset.
    SettingsScrollbar,
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
/// Unread rows swap the muted idle braille dot for a solid filled circle so
/// the marker reads at a glance (and matches the web sidebar's unread dot).
/// Paired with bold + `theme.unread` in the row formatter.
pub(super) const ICON_UNREAD: &str = "●";
/// How long a session row must stay selected (with the list in the foreground)
/// before its unread marker clears. Long enough that arrowing past a row to get
/// somewhere doesn't read it, short enough that pausing to read the preview
/// does. See `tick_unread_dwell`.
pub(super) const UNREAD_DWELL: std::time::Duration = std::time::Duration::from_secs(2);
pub(super) const ICON_ERROR: &str = "✕";
pub(super) const ICON_UNKNOWN: &str = "⠤";
pub(super) const ICON_STOPPED: &str = "⠒";
pub(super) const ICON_DELETING: &str = "✕";
pub(super) const ICON_COLLAPSED: &str = "▶";
pub(super) const ICON_EXPANDED: &str = "▼";
/// Marks a pinned project header in project view. Geometric per DESIGN.md
/// (clean readable glyphs, not emoji).
pub(super) const ICON_PINNED: &str = "◆";
/// Type glyphs for the two synthetic bottom-shelf section headers, so they read
/// as system shelves rather than user groups. Single-width geometric glyphs per
/// DESIGN.md (emoji would break column alignment and the shelf's mouse
/// hit-testing on terminals that render them double-width or as tofu).
pub(super) const ICON_TRASH_SECTION: &str = "⊘";
pub(super) const ICON_ARCHIVED_SECTION: &str = "▤";

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
    pub(super) project_session_picker_dialog: Option<ProjectSessionPickerDialog>,
    pub(super) projects_dialog: Option<ProjectsDialog>,
    pub(super) plugin_manager_dialog: Option<crate::tui::dialogs::PluginManagerDialog>,
    pub(super) command_palette: Option<CommandPaletteDialog>,
    #[cfg(feature = "serve")]
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
    #[cfg(feature = "serve")]
    pub(super) pending_daemon_start_session: Option<String>,
    /// The structured-view session mounted in the preview pane, if any:
    /// a streaming transcript that `render_preview` paints as the
    /// preview content for a selected structured row (read-only until
    /// entered; see `EmbeddedView::is_active`). Owned here so the
    /// preview renderer, info header, and drag-select all compose with
    /// it; the `App` loop drives its async sides (connect, WS pump,
    /// active-mode key routing).
    #[cfg(feature = "serve")]
    pub(in crate::tui) structured_preview:
        Option<crate::tui::structured_view::embedded::EmbeddedView>,
    /// True while the App's preview-on-select reconcile has picked a
    /// structured session but its view hasn't finished mounting. The
    /// preview renderer shows a quiet placeholder instead of the wordy
    /// "press Enter" page, which otherwise flashes for the connect
    /// window on every selection.
    #[cfg(feature = "serve")]
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

    // Performance: background deletion
    pub(super) deletion_poller: DeletionPoller,

    // Performance: background stop (docker stop can block up to ~10s)
    pub(super) stop_poller: StopPoller,

    // Performance: background trash prep (stops the sandbox container, so the
    // same ~10s docker stop block as `stop_poller`, plus the worktree move)
    pub(super) trash_poller: crate::tui::trash_poller::TrashPoller,

    // Performance: background restart (the start cascade shells out to docker
    // and runs the before_start host hook, which can block for seconds)
    pub(super) restart_poller: RestartPoller,
    /// Sessions whose restart cascade is in flight on the restart poller.
    /// Suppresses the StatusPoller's missing-tmux Error transition until the
    /// worker reports back via `apply_restart_results`.
    pub(super) restart_in_flight: std::collections::HashSet<String>,

    /// Trashed sessions whose permanent-purge Purge claim (#2541) this TUI won
    /// before dispatching the teardown. Their delete finalize applies the #2534
    /// restore-race recheck and releases the claim (ownership-guarded), instead
    /// of the plain live-session removal.
    pub(super) purge_claimed: std::collections::HashSet<String>,

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

/// Identifies config-watch entries without letting a profile literally named
/// `"<global>"` collide with the app-wide config subscription.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ConfigWatchKey {
    /// The app-wide `<app_dir>/config.toml` subscription.
    Global,
    /// A per-profile `<profile>/config.toml` subscription.
    Profile(String),
}

impl ConfigWatchKey {
    fn profile(name: &str) -> Self {
        Self::Profile(name.to_string())
    }
}

const RELOAD_FAILED_TITLE: &str = "Reload Failed";
const WATCHER_WARNING_TITLE: &str = "Watcher Warning";

/// Distinguishes user-driven config reloads from watcher kicks so
/// `refresh_from_config` can suppress interactive-only dialogs on
/// background refreshes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfigRefreshOrigin {
    /// A user action triggered the reload and may surface dialogs.
    Interactive,
    /// A watcher kick triggered the reload and should stay silent.
    Watcher,
}

/// Eligible, unseen tip count for the home-view badge, honoring the
/// `session.show_tips` setting. Shared by the constructor, config refresh, and
/// the tips-state writers so the cached badge count can't drift.
pub(super) fn tips_unseen_count(config: &crate::session::Config) -> usize {
    if !config.session.show_tips {
        return 0;
    }
    crate::tips::unseen_count(
        crate::tips::TipSurface::Tui,
        &config.app_state.tips_seen,
        &crate::tips::TipSignals {
            new_session_with_selection_count: config.app_state.new_session_with_selection_count,
            used_new_from_selection: config.app_state.used_new_from_selection,
        },
    )
}

/// Per-profile subscription pair, held in `HomeView::disk_watch.handles`
/// and `HomeView::config_watch.handles`.
///
/// Two teardown paths exist:
/// 1. Explicit-remove (rewire / profile delete via `drop_disk_watch_entry`):
///    drop the `SubscriptionHandle` first to close the source channel; the
///    forwarder's `rx.recv().await` returns `None` and exits naturally;
///    `forwarder.abort()` then runs as a fast-path safeguard for any
///    `recv` future that has not yet observed the close.
/// 2. HomeView field-drop on shutdown: the same order is guaranteed by
///    struct field declaration order. `disk_watch.handles` drops, each
///    entry's handle drops first (channel-close cascade), the forwarder
///    exits naturally; the AbortHandle drop is a no-op (Tokio's
///    `AbortHandle` does not abort on drop) but the forwarder is already
///    gone.
pub(super) struct DiskWatchEntry {
    handle: crate::file_watch::SubscriptionHandle,
    forwarder: tokio::task::AbortHandle,
    /// Canonicalized dir at install time. Compared against the current
    /// canonical resolution on rewire to detect path-level moves.
    /// notify NonRecursive watches do not auto-reattach to a recreated
    /// directory on Linux inotify or macOS FSEvents.
    canonical_dir: std::path::PathBuf,
    /// Filesystem identity (`(dev, ino, btime)` on Unix; `()`
    /// elsewhere) captured when the subscription was installed. The
    /// canonical path string survives a peer `rm -rf X && mkdir X`
    /// race because the new dir resolves to the same string, and on
    /// ext4/overlayfs the freed inode number is routinely recycled by
    /// the immediate recreate; the birth time component is what
    /// distinguishes the new dir there. On rewire, mismatch against a
    /// fresh stat forces an entry rebuild even when the canonical path
    /// is unchanged. Stat failure at install stores the type's
    /// `Default` (`(0, 0, None)` on Unix; `()` elsewhere) as a
    /// sentinel; on Unix `(0, 0, _)` cannot collide with a real
    /// filesystem identity, so the next rewire that successfully stats
    /// the dir mismatches against the sentinel and forces a rebuild.
    installed_identity: crate::file_watch::WatchIdentity,
}

/// Drop the subscription handle FIRST. Closing the source channel
/// before aborting the forwarder ensures no in-flight event reaches
/// an aborted task.
fn drop_disk_watch_entry(entry: DiskWatchEntry) {
    let DiskWatchEntry {
        handle,
        forwarder,
        canonical_dir: _,
        installed_identity: _,
    } = entry;
    drop(handle);
    forwarder.abort();
}

/// Sibling of [`ConfigWatchState`]; groups the per-profile storage-mirror
/// subscriptions with the shared dirty latch their forwarders set. The
/// two fields must move together because every install/uninstall of a
/// subscription is paired with a kick or compensation `store` on the
/// latch. Per the file-watch service contract, the rewire path reuses
/// the single `Arc<FileWatchService>` constructed once for this TUI
/// process and never builds a second one.
pub(super) struct DiskWatchState {
    /// Dirty latch (cap-1 fan-in) set with `Release` ordering by every
    /// profile forwarder and swapped to `false` with `Acquire` ordering
    /// by the tick loop in `App::run`; idempotent across forwarders so
    /// multiple events between two reloads collapse into one
    /// `reload_storage_only`.
    pub(super) dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Per-profile subscription pairs keyed by profile name. Drop a
    /// value through [`drop_disk_watch_entry`] to honor the
    /// drop-then-abort teardown protocol; field-drop on `HomeView`
    /// shutdown falls back to the same order via declaration order.
    pub(super) handles: HashMap<String, DiskWatchEntry>,
}

/// Sibling of [`DiskWatchState`]; groups the per-key config-file
/// subscriptions with the shared dirty latch their forwarders set. The
/// typed key keeps the global `<app_dir>/config.toml` entry from
/// colliding with any literal profile name. The Arc-reuse rule is the
/// same as [`DiskWatchState`]: the rewire path reuses the single
/// `Arc<FileWatchService>` constructed once for this TUI process.
pub(super) struct ConfigWatchState {
    /// Dirty latch (cap-1 fan-in) set with `Release` ordering by every
    /// config forwarder (global + per-profile) and swapped to `false`
    /// with `Acquire` ordering by the tick loop in `App::run`;
    /// idempotent across forwarders so multiple events between two
    /// reloads collapse into one `refresh_from_config`.
    pub(super) dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Per-key subscription pairs. Reuses [`DiskWatchEntry`] because
    /// the drop-then-abort teardown protocol is identical to the
    /// disk-watch sibling.
    pub(super) handles: HashMap<ConfigWatchKey, DiskWatchEntry>,
}

impl DiskWatchState {
    /// Reconcile per-profile storage-mirror subscriptions
    /// (`sessions.json` / `groups.json`) against `current` via set-diff:
    /// drop entries for profiles in `prior - current`, keep entries in
    /// `prior ∩ current` untouched, install fresh entries for profiles
    /// in `current - prior`. Same-set rewires are a no-op.
    ///
    /// Inode-invalidation case (profile dir deleted and recreated under
    /// the same name): the caller must drop the stale entry first via
    /// `drop_disk_watch_entry` before invoking this helper, so the name
    /// is missing from `prior` and the install path runs.
    ///
    /// Service ownership: the rewire path reuses the caller's
    /// `Arc<FileWatchService>`, the single instance constructed once for
    /// this TUI process (per the file-watch service design's "one Arc
    /// per process" rule). It must NEVER construct a second service.
    /// In-process storage writes propagate through the Local fast path
    /// in `Storage::update`; cross-process writes propagate through the
    /// kernel watcher within its debounce window.
    pub(super) fn rewire(
        &mut self,
        file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
        current: &[String],
        reload_failure: &mut ReloadFailureState,
    ) {
        use crate::file_watch::{FileMatcher, WatchSpec};
        use std::collections::HashSet;
        use std::time::Duration;

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        let prior: HashSet<String> = self.handles.keys().cloned().collect();
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
                let entry = match self.handles.get(*name) {
                    Some(e) => e,
                    None => return false,
                };
                let current_canonical = crate::session::get_profile_dir_path(name)
                    .ok()
                    .and_then(|p| std::fs::canonicalize(&p).ok());
                match current_canonical {
                    Some(canonical) => {
                        canonical != entry.canonical_dir
                            || crate::file_watch::capture_watch_identity(&canonical)
                                .map(|id| id != entry.installed_identity)
                                .unwrap_or(false)
                    }
                    None => true,
                }
            })
            .cloned()
            .collect();

        if prior == current.iter().cloned().collect()
            && inode_invalidated.is_empty()
            && !reload_failure.disk_watcher_init_error_references_missing_profile(current)
        {
            return;
        }

        // Buffer the install-loop outcome and apply it as one transition
        // at the end of the pass: an identical failure recurring across
        // rewires must not re-arm the ack latch (issue #2112).
        let mut new_init_error: Option<WatcherInitError> = None;

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
            if let Some(entry) = self.handles.remove(name) {
                drop_disk_watch_entry(entry);
            }
        }

        for name in &to_add {
            let dir = match crate::session::get_profile_dir_path(name) {
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
            if !dir.exists() {
                tracing::debug!(
                    target: "tui.file_watch",
                    profile = %name,
                    "skipping disk subscribe; profile dir absent (peer delete raced the list_profiles snapshot)"
                );
                continue;
            }
            let canonical_dir = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            let sessions_path = dir.join("sessions.json");
            let groups_path = dir.join("groups.json");
            let spec = WatchSpec {
                dir: dir.clone(),
                matcher: FileMatcher::AnyOf(vec![sessions_path, groups_path]),
                debounce: Some(Duration::from_millis(75)),
            };
            match file_watch.subscribe_channel(spec, 16) {
                Ok((mut rx, handle)) => {
                    use tracing::Instrument;
                    let dirty = self.dirty.clone();
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
                    self.handles.insert(
                        name.clone(),
                        DiskWatchEntry {
                            handle,
                            forwarder: join.abort_handle(),
                            canonical_dir: canonical_dir.clone(),
                            installed_identity: crate::file_watch::capture_watch_identity(
                                &canonical_dir,
                            )
                            .unwrap_or_default(),
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
                    new_init_error = Some(WatcherInitError {
                        profile: Some(name.clone()),
                        kind: WatcherInitErrorKind::Watch(e.kind()),
                        message: e.to_string(),
                    });
                }
            }
        }
        reload_failure.apply_disk_watcher_init_pass(new_init_error);
        tracing::debug!(
            target: "tui.file_watch",
            added = ?to_add,
            removed = ?to_remove,
            "reconciled per-profile disk-watch subscriptions"
        );
        // Missed-window compensation, mirroring the config rewire: a
        // sessions.json/groups.json write into a recreated dir before
        // this rebuild produced no event, so kick the latch and let the
        // next tick reload storage from disk.
        if !inode_invalidated.is_empty() {
            self.dirty.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

impl ConfigWatchState {
    /// Reconcile the per-key config-file subscriptions against the live
    /// profile set + the always-present global key. The global key
    /// (`<app_dir>/config.toml`) is subscribed once and kept across
    /// rewires because the app dir is never deleted mid-session, so the
    /// kernel watch on it stays valid.
    ///
    /// Per-profile entries are fully torn down then re-subscribed on
    /// each rewire: every existing per-profile entry is dropped first,
    /// then a fresh subscription is installed for each profile in
    /// `current`. This handles the "profile dir deleted and recreated
    /// under the same name" case where the kernel watch is invalidated
    /// by the unlink even though the profile name has not changed.
    /// Unlike [`DiskWatchState::rewire`] which uses set-diff to
    /// preserve stable subscriptions across calls, full teardown is
    /// cheap here because each profile has a single config-file
    /// subscription rather than a directory watch with multiple matched
    /// files, and the per-rewire churn is bounded by profile count.
    ///
    /// Drop order on remove is canonical: drop the `SubscriptionHandle`
    /// FIRST, then abort the forwarder, so the source channel closes
    /// and the forwarder's `rx.recv()` returns `None` naturally before
    /// the abort fires as a safeguard.
    ///
    /// Service ownership: the rewire path reuses the caller's
    /// `Arc<FileWatchService>`, the single instance constructed once for
    /// this TUI process (per the file-watch service design's "one Arc
    /// per process" rule). It must NEVER construct a second service.
    /// Cross-process config edits (user `$EDITOR` save, peer
    /// `aoe profile create/delete`) propagate through the kernel
    /// watcher; in-process config writes are out of scope here because
    /// `Storage::update` does not write config files (only sessions /
    /// groups), so no `notify_local_change` is wired on this path.
    pub(super) fn rewire(
        &mut self,
        file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
        current: &[String],
        reload_failure: &mut ReloadFailureState,
    ) {
        use crate::file_watch::{FileMatcher, WatchSpec};
        use std::time::Duration;

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        // Drop the existing global entry when its stored canonical_dir
        // does not match the live canonicalized app dir, mirroring the
        // disk-watch inode-aware rewire. The install-once branch below
        // picks up the new inode.
        let global_invalidated = match self.handles.get(&ConfigWatchKey::Global) {
            Some(entry) => {
                let current_canonical = crate::session::get_app_dir()
                    .ok()
                    .and_then(|p| std::fs::canonicalize(&p).ok());
                match current_canonical {
                    Some(canonical) => {
                        canonical != entry.canonical_dir
                            || crate::file_watch::capture_watch_identity(&canonical)
                                .map(|id| id != entry.installed_identity)
                                .unwrap_or(false)
                    }
                    None => true,
                }
            }
            None => false,
        };
        if global_invalidated {
            if let Some(entry) = self.handles.remove(&ConfigWatchKey::Global) {
                drop_disk_watch_entry(entry);
            }
        }
        let global_needs_install = !self.handles.contains_key(&ConfigWatchKey::Global);

        let prior_profiles: std::collections::HashSet<String> = self
            .handles
            .keys()
            .filter_map(|key| match key {
                ConfigWatchKey::Global => None,
                ConfigWatchKey::Profile(name) => Some(name.clone()),
            })
            .collect();
        let target: std::collections::HashSet<&String> = current.iter().collect();

        // Per-profile inode invalidation: peer-driven `aoe profile delete X
        // && aoe profile new X` keeps the same name but produces a new
        // inode, and notify NonRecursive watches do not auto-reattach.
        // Compare each prior entry's stored canonical_dir against the
        // current canonical resolution; mismatch forces a rewire even
        // when the name set is unchanged. Resolution goes through the
        // non-creating `get_profile_dir_path`; `get_profile_dir` calls
        // `fs::create_dir_all`, which recreates a profile directory
        // the user just deleted and re-surfaces it in `list_profiles()`
        // on the next heartbeat.
        let inode_invalidated: Vec<String> = prior_profiles
            .iter()
            .filter(|name| {
                let entry = match self.handles.get(&ConfigWatchKey::profile(name)) {
                    Some(e) => e,
                    None => return false,
                };
                let current_canonical = crate::session::get_profile_dir_path(name)
                    .ok()
                    .and_then(|p| std::fs::canonicalize(&p).ok());
                match current_canonical {
                    Some(canonical) => {
                        canonical != entry.canonical_dir
                            || crate::file_watch::capture_watch_identity(&canonical)
                                .map(|id| id != entry.installed_identity)
                                .unwrap_or(false)
                    }
                    None => true,
                }
            })
            .cloned()
            .collect();

        let to_remove: Vec<String> = prior_profiles
            .iter()
            .filter(|n| !target.contains(*n) || inode_invalidated.iter().any(|i| i == *n))
            .cloned()
            .collect();
        let to_add: Vec<String> = current
            .iter()
            .filter(|n| !prior_profiles.contains(*n) || inode_invalidated.iter().any(|i| i == *n))
            .cloned()
            .collect();

        if !global_needs_install
            && to_remove.is_empty()
            && to_add.is_empty()
            && !reload_failure.config_watcher_init_error_references_missing_profile(current)
        {
            return;
        }

        // Buffer the install-loop outcome and apply it as one transition
        // at the end of the pass: an identical failure recurring across
        // rewires must not re-arm the ack latch (issue #2112).
        let mut new_init_error: Option<WatcherInitError> = None;

        if global_needs_install {
            match crate::session::get_app_dir() {
                Ok(app_dir) => {
                    let canonical_dir =
                        std::fs::canonicalize(&app_dir).unwrap_or_else(|_| app_dir.clone());
                    let target = app_dir.join("config.toml");
                    let spec = WatchSpec {
                        dir: app_dir,
                        matcher: FileMatcher::Exact(target),
                        debounce: Some(Duration::from_millis(100)),
                    };
                    match file_watch.subscribe_channel(spec, 4) {
                        Ok((mut rx, handle)) => {
                            use tracing::Instrument;
                            let dirty = std::sync::Arc::clone(&self.dirty);
                            let span = tracing::debug_span!("tui.config_watch.global.forwarder");
                            let join = crate::task_util::spawn_supervised(
                                "tui.config_watch.global.forwarder",
                                crate::task_util::PanicPolicy::Log,
                                async move {
                                    while rx.recv().await.is_some() {
                                        dirty.store(true, std::sync::atomic::Ordering::Release);
                                    }
                                }
                                .instrument(span),
                            );
                            self.handles.insert(
                                ConfigWatchKey::Global,
                                DiskWatchEntry {
                                    handle,
                                    forwarder: join.abort_handle(),
                                    canonical_dir: canonical_dir.clone(),
                                    installed_identity: crate::file_watch::capture_watch_identity(
                                        &canonical_dir,
                                    )
                                    .unwrap_or_default(),
                                },
                            );
                            tracing::debug!(
                                target: "tui.file_watch",
                                "global config.toml subscription installed"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "tui.file_watch",
                                error = %e,
                                "global config subscribe_channel failed; \
                                 falling back to settings-close + profile-switch reload"
                            );
                            new_init_error = Some(WatcherInitError {
                                profile: None,
                                kind: WatcherInitErrorKind::Watch(e.kind()),
                                message: e.to_string(),
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        error = %e,
                        "skipping global config subscribe; app dir resolution failed"
                    );
                    new_init_error = Some(WatcherInitError {
                        profile: None,
                        kind: WatcherInitErrorKind::Resolution,
                        message: format!("app dir resolution failed: {e}"),
                    });
                }
            }
        }

        for name in &to_remove {
            if let Some(entry) = self.handles.remove(&ConfigWatchKey::profile(name)) {
                drop_disk_watch_entry(entry);
            }
        }

        for name in &to_add {
            let dir = match crate::session::get_profile_dir_path(name) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        profile = %name,
                        error = %e,
                        "skipping config subscribe; profile dir resolution failed"
                    );
                    continue;
                }
            };
            if !dir.exists() {
                tracing::debug!(
                    target: "tui.file_watch",
                    profile = %name,
                    "skipping config subscribe; profile dir absent (peer delete raced the list_profiles snapshot)"
                );
                continue;
            }
            let canonical_dir = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            let target_path = dir.join("config.toml");
            let spec = WatchSpec {
                dir: dir.clone(),
                matcher: FileMatcher::Exact(target_path),
                debounce: Some(Duration::from_millis(100)),
            };
            match file_watch.subscribe_channel(spec, 4) {
                Ok((mut rx, handle)) => {
                    use tracing::Instrument;
                    let dirty = self.dirty.clone();
                    let span = tracing::debug_span!(
                        "tui.config_watch.profile.forwarder",
                        profile = %name
                    );
                    let join = crate::task_util::spawn_supervised(
                        "tui.config_watch.profile.forwarder",
                        crate::task_util::PanicPolicy::Log,
                        async move {
                            while rx.recv().await.is_some() {
                                dirty.store(true, std::sync::atomic::Ordering::Release);
                            }
                        }
                        .instrument(span),
                    );
                    self.handles.insert(
                        ConfigWatchKey::profile(name),
                        DiskWatchEntry {
                            handle,
                            forwarder: join.abort_handle(),
                            canonical_dir: canonical_dir.clone(),
                            installed_identity: crate::file_watch::capture_watch_identity(
                                &canonical_dir,
                            )
                            .unwrap_or_default(),
                        },
                    );
                    tracing::debug!(
                        target: "tui.file_watch",
                        profile = %name,
                        "profile config.toml subscription installed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        profile = %name,
                        error = %e,
                        "config subscribe_channel failed; \
                         falling back to settings-close + profile-switch reload for this profile"
                    );
                    new_init_error = Some(WatcherInitError {
                        profile: Some(name.clone()),
                        kind: WatcherInitErrorKind::Watch(e.kind()),
                        message: e.to_string(),
                    });
                }
            }
        }
        reload_failure.apply_config_watcher_init_pass(new_init_error);
        if !to_add.is_empty() || !to_remove.is_empty() {
            tracing::debug!(
                target: "tui.file_watch",
                added = ?to_add,
                removed = ?to_remove,
                "rewire_config_subscriptions: per-profile set-diff update"
            );
        }
        // Missed-window compensation: an invalidation-driven rebuild means
        // the kernel watch was dead for some interval (peer rm+recreate of
        // the watched dir), and any config write landing in that interval
        // produced no event. Kick the dirty latch so the next tick
        // re-reads config from disk rather than trusting the (silent)
        // fresh watch. Scoped to invalidation rebuilds; plain set-diff
        // adds/removes have no dead window to compensate.
        if global_invalidated || !inode_invalidated.is_empty() {
            self.dirty.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Stable identity for a watcher-init failure across rewire passes.
/// The `notify` crate's Display string is not part of its stability
/// guarantee; ack-equality is keyed on the structured kind so a
/// future Display drift does not silently re-arm the dialog on the
/// same persistent failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatcherInitErrorKind {
    Watch(crate::file_watch::WatchErrorKind),
    /// The app-dir resolution path errored before a subscribe attempt
    /// could be made. Distinct from any `Watch(_)` variant so a
    /// resolution failure followed by a backend failure surfaces as a
    /// content change.
    Resolution,
}

/// Latched record of a watcher-init failure. The disk slot always carries
/// `Some(profile)`; the config slot carries `None` for the global config
/// watch and `Some(profile)` per-profile. Equality is keyed on
/// `(profile, kind)`; `message` is display-only.
pub(super) struct WatcherInitError {
    pub(super) profile: Option<String>,
    pub(super) kind: WatcherInitErrorKind,
    pub(super) message: String,
}

impl PartialEq for WatcherInitError {
    fn eq(&self, other: &Self) -> bool {
        self.profile == other.profile && self.kind == other.kind
    }
}

impl Eq for WatcherInitError {}

/// Per-tick reload failure tracking. Tick-driven reload paths in
/// `App::run` (heartbeat `reload()`, watcher-driven `reload_storage_only()`,
/// watcher-driven `refresh_from_config()`) route results through
/// `handle_tick_reload_storage` / `handle_tick_reload_config`, which
/// record failures here so the tick loop surfaces a single aggregated
/// `info_dialog` per failure burst rather than one dialog per tick.
///
/// `dialog_acknowledged` latches once the dialog is shown and clears
/// only after every source returns to healthy, so the user is notified
/// once per failure burst, not once per tick. The dialog body aggregates
/// every currently-failing source (storage, config, disk-watcher init,
/// config-watcher init) into a single message.
#[derive(Default)]
pub(super) struct ReloadFailureState {
    storage_failed: bool,
    storage_error: Option<String>,
    config_failed: bool,
    config_error: Option<String>,
    /// Latched record of the most recent disk-watcher init failure
    /// (typically `subscribe_channel` returning Err on disk rewire).
    /// Surfaced in the reload-failure dialog body. Cleared on the next
    /// successful disk rewire pass for the affected profile.
    disk_watcher_init_error: Option<WatcherInitError>,
    /// Latched record of the most recent config-watcher init failure
    /// (typically `subscribe_channel` returning Err on config rewire).
    /// Independent from `disk_watcher_init_error`: a config init failure
    /// is not overwritten by a disk rewire and persists until the next
    /// successful config rewire pass for the affected key.
    config_watcher_init_error: Option<WatcherInitError>,
    dialog_acknowledged: bool,
}

impl ReloadFailureState {
    pub(super) fn record_storage(&mut self, result: &anyhow::Result<()>) -> bool {
        match result {
            Ok(()) => {
                if self.storage_failed {
                    self.storage_failed = false;
                    self.storage_error = None;
                    if !self.has_any_failure() {
                        self.dialog_acknowledged = false;
                    }
                    return true;
                }
                false
            }
            Err(e) => {
                // Healthy-to-failed transition re-arms the dialog so a new
                // source failing during a previously acknowledged burst
                // surfaces a fresh notification rather than being silently
                // absorbed by the ack latch.
                if !self.storage_failed {
                    self.dialog_acknowledged = false;
                }
                self.storage_failed = true;
                self.storage_error = Some(format!("{e:#}"));
                false
            }
        }
    }

    pub(super) fn record_config(&mut self, result: &anyhow::Result<()>) -> bool {
        match result {
            Ok(()) => {
                if self.config_failed {
                    self.config_failed = false;
                    self.config_error = None;
                    if !self.has_any_failure() {
                        self.dialog_acknowledged = false;
                    }
                    return true;
                }
                false
            }
            Err(e) => {
                if !self.config_failed {
                    self.dialog_acknowledged = false;
                }
                self.config_failed = true;
                self.config_error = Some(format!("{e:#}"));
                false
            }
        }
    }

    /// Apply the outcome of a disk-watch rewire pass as one transition.
    /// `new` is the per-pass install-loop result (`Some` if any profile's
    /// `subscribe_channel` returned `Err`, `None` otherwise). The latch
    /// is re-armed only on a content change: a same-as-before failure
    /// inside an acknowledged burst is treated as a no-op so the user
    /// is not re-notified every rewire pass while the underlying
    /// failure persists. A clean transition to `None` resets the ack
    /// latch when no other source remains failing, so a later identical
    /// failure surfaces a fresh dialog.
    pub(super) fn apply_disk_watcher_init_pass(&mut self, new: Option<WatcherInitError>) {
        let was = std::mem::replace(&mut self.disk_watcher_init_error, new);
        match (&was, &self.disk_watcher_init_error) {
            (Some(prev), Some(curr)) if prev == curr => {}
            (None, None) => {}
            (Some(_), None) => {
                if !self.has_any_failure() {
                    self.dialog_acknowledged = false;
                }
            }
            (_, Some(_)) => {
                self.dialog_acknowledged = false;
            }
        }
    }

    /// Apply the outcome of a config-watch rewire pass as one transition.
    /// See [`Self::apply_disk_watcher_init_pass`] for the latch semantics;
    /// the two slots are independent.
    pub(super) fn apply_config_watcher_init_pass(&mut self, new: Option<WatcherInitError>) {
        let was = std::mem::replace(&mut self.config_watcher_init_error, new);
        match (&was, &self.config_watcher_init_error) {
            (Some(prev), Some(curr)) if prev == curr => {}
            (None, None) => {}
            (Some(_), None) => {
                if !self.has_any_failure() {
                    self.dialog_acknowledged = false;
                }
            }
            (_, Some(_)) => {
                self.dialog_acknowledged = false;
            }
        }
    }

    pub(super) fn disk_watcher_init_error_references_missing_profile(
        &self,
        current: &[String],
    ) -> bool {
        self.disk_watcher_init_error
            .as_ref()
            .and_then(|e| e.profile.as_deref())
            .is_some_and(|name| !current.iter().any(|p| p == name))
    }

    pub(super) fn config_watcher_init_error_references_missing_profile(
        &self,
        current: &[String],
    ) -> bool {
        self.config_watcher_init_error
            .as_ref()
            .and_then(|e| e.profile.as_deref())
            .is_some_and(|name| !current.iter().any(|p| p == name))
    }

    pub(super) fn has_any_failure(&self) -> bool {
        self.storage_failed
            || self.config_failed
            || self.disk_watcher_init_error.is_some()
            || self.config_watcher_init_error.is_some()
    }

    pub(super) fn has_unacknowledged_failure(&self) -> bool {
        self.has_any_failure() && !self.dialog_acknowledged
    }

    pub(super) fn build_dialog_body(&self) -> String {
        let mut lines: Vec<String> = vec!["The following reload sources are degraded:".to_string()];
        if let Some(e) = &self.storage_error {
            lines.push(format!("- Storage: {e}"));
        }
        if let Some(e) = &self.config_error {
            lines.push(format!("- Config: {e}"));
        }
        if let Some(e) = &self.disk_watcher_init_error {
            let detail = match &e.profile {
                Some(name) => format!("{name}: {}", e.message),
                None => e.message.clone(),
            };
            lines.push(format!("- Disk watcher init: {detail}"));
        }
        if let Some(e) = &self.config_watcher_init_error {
            let detail = match &e.profile {
                Some(name) => format!("profile {name} config: {}", e.message),
                None => format!("global config: {}", e.message),
            };
            lines.push(format!("- Config watcher init: {detail}"));
        }
        lines.push(String::new());
        lines.push("In-memory state preserved; sources retry automatically.".to_string());
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
            // Backfill / heal trashed worktrees: relocate any still sitting in
            // the active dir (rows trashed before relocation existed) and fix a
            // pointer a crash left stale. Best-effort and only touches trashed
            // rows, which are typically few. See #2522.
            for inst in &mut instances {
                if crate::session::trash::reconcile_trashed_location(inst) {
                    let new_path = inst.project_path.clone();
                    let pre = inst.pre_trash_project_path.clone();
                    let target_id = inst.id.clone();
                    let _ = storage.update(|disk, _groups| {
                        if let Some(d) = disk.iter_mut().find(|i| i.id == target_id) {
                            d.project_path = new_path.clone();
                            d.pre_trash_project_path = pre.clone();
                        }
                        Ok(())
                    });
                }
            }
            // Self-heal (#2541): clear op_claims left by a purge/restore that
            // crashed mid-operation. `try_claim` already treats an expired claim
            // as free, so this is belt-and-suspenders that stops a stranded
            // claim from lingering on disk after a crash.
            let ttl = crate::session::Instance::OP_CLAIM_TTL;
            let now = chrono::Utc::now();
            for inst in &mut instances {
                if inst.clear_expired_op_claim(ttl, now) {
                    let target_id = inst.id.clone();
                    let _ = storage.update(|disk, _groups| {
                        if let Some(d) = disk.iter_mut().find(|i| i.id == target_id) {
                            d.clear_expired_op_claim(ttl, now);
                        }
                        Ok(())
                    });
                }
            }
            let tree = GroupTree::new_with_groups(&instances, &groups);
            group_trees.insert(profile_name.clone(), tree);
            all_instances.extend(instances);
            storages.insert(profile_name.clone(), storage);
        }

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
        crate::session::set_unread_enabled(resolved.session.unread_indicator);
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
        let tips_unseen = user_config.as_ref().map_or_else(
            || tips_unseen_count(&crate::session::Config::default()),
            tips_unseen_count,
        );
        let view_mode = ViewMode::default();

        let disk_watch = DiskWatchState {
            dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            handles: HashMap::new(),
        };

        let config_watch = ConfigWatchState {
            dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            handles: HashMap::new(),
        };

        let mut view = Self {
            storages,
            active_profile,
            instances: Self::build_instances_map(all_instances),
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
            agent_clipboard_forward: resolved.tmux.clipboard
                != crate::session::config::TmuxClipboardMode::Disabled,
            vt_live_enabled: resolved.tmux.vt_live,
            profile_default_attach_mode: resolved.session.default_attach_mode,
            project_group_collapsed: user_config
                .as_ref()
                .map(|c| {
                    c.app_state
                        .project_group_collapsed
                        .iter()
                        .map(|path| (path.clone(), true))
                        .collect()
                })
                .unwrap_or_default(),
            registered_projects: Vec::new(),
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
            volume_ignores_glob_dialog: None,
            pending_volume_ignores_glob_data: None,
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
            plugin_manager_dialog: None,
            command_palette: None,
            #[cfg(feature = "serve")]
            serve_view: None,
            update_confirm_dialog: None,
            telemetry_consent_dialog: None,
            tips_dialog: None,
            tips_unseen,
            pending_tip_pop: None,
            tips_badge_rect: None,
            tips_badge_hovered: false,
            send_message_dialog: None,
            permission_response_dialog: None,
            pending_permission_response_session: None,
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
            live_send_ctrl_c_flash_until: None,
            sidebar_collapsed: user_config
                .as_ref()
                .and_then(|c| c.app_state.home_sidebar_collapsed)
                .unwrap_or(false),
            collapse_button_area: Rect::default(),
            expand_strip_area: Rect::default(),
            footer_buttons: Vec::new(),
            footer_hover: None,
            preview_pane_synced: None,
            pending_paste: None,
            pending_attach_after_warning: None,
            pending_stop_session: None,
            pending_stop_terminal: None,
            pending_stop_tool: None,
            pending_image_pull: None,
            pending_switch_view_session: None,
            #[cfg(feature = "serve")]
            pending_daemon_start_session: None,
            #[cfg(feature = "serve")]
            structured_preview: None,
            #[cfg(feature = "serve")]
            structured_preview_pending: false,
            pending_force_remove_session: None,
            pending_trash_session: None,
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
            trash_poller: crate::tui::trash_poller::TrashPoller::new(),
            restart_poller: RestartPoller::new(),
            restart_in_flight: std::collections::HashSet::new(),
            purge_claimed: std::collections::HashSet::new(),
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
            shelf_inner_area: Rect::default(),
            mouse_pos: None,
            last_click: None,
            last_preview_click: None,
            unread_dwell: None,
            manual_unread_hold: None,
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
            mouse_forward_btn: None,
            hover_forward_cell: None,
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
            trashed_section_collapsed: true,
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
            disk_watch,
            config_watch,
            watcher_config_refresh_count: std::sync::atomic::AtomicU64::new(0),
            reload_failure_state: ReloadFailureState::default(),
            // App::new loads the boot theme; no startup stash from HomeView.
            pending_watcher_theme: None,
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
            .values()
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
            for inst in view.instances.values() {
                let Some(tmux_name) = inst.tmux_env_session_name() else {
                    continue;
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
        for inst in view.instances.values_mut() {
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

        view.refresh_registered_projects();
        view.flat_items = view.build_flat_items();
        view.update_selected();
        // Disk subscriptions stay scoped to the loaded storages: in
        // single-profile mode (`aoe --profile X`) the user opted into
        // exactly that profile's instance state, so we don't watch
        // sessions.json/groups.json for unrelated profiles. Sorted so
        // the install-loop's last-write-wins target stays stable across
        // HashMap rehash between rewire ticks (see #2584).
        let mut initial_disk_profiles: Vec<String> = view.storages.keys().cloned().collect();
        initial_disk_profiles.sort();
        view.rewire_disk_subscriptions(&initial_disk_profiles);
        // Config subscriptions are intentionally asymmetric: even in
        // single-profile mode, peer edits to ANY profile's config.toml
        // (or the global config) must be observable so the picker UI
        // and status-hook config cache reflect external changes (e.g.
        // a peer process creating a new profile while the user runs in
        // filtered mode). The reload helper rewires the same way on
        // every tick once running, so this is the startup-side
        // counterpart that closes the boot-time window.
        let initial_config_profiles: Vec<String> = match crate::session::list_profiles() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    target: "tui.file_watch",
                    error = %e,
                    "list_profiles failed at startup; falling back to loaded storages for config wiring"
                );
                initial_disk_profiles.clone()
            }
        };
        view.rewire_config_subscriptions(&initial_config_profiles);
        Ok(view)
    }

    /// Full reload: status-hook config-cache refresh + storage. Used by
    /// the 5s heartbeat tick and by event-driven sites (attach-return,
    /// save+reload pairs, profile switch). Watcher-driven ticks call
    /// `reload_storage_only` because the disk watcher only fires on
    /// `sessions.json` / `groups.json`; the config watcher drives
    /// `refresh_from_config` independently.
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

        let current_profiles = match list_profiles() {
            Ok(profiles) => profiles,
            Err(error) => {
                tracing::warn!(
                    target: "tui.file_watch",
                    error = %error,
                    "list_profiles failed during reload_storage_only; reusing loaded storages for watcher rewires"
                );
                let mut keys: Vec<String> = self.storages.keys().cloned().collect();
                keys.sort();
                keys
            }
        };

        // Asymmetric rewire mirrors `HomeView::new` startup wiring. Config
        // rewire covers the full `list_profiles()` set so peer config edits
        // surface to the picker UI and status-hook cache regardless of mode
        // (e.g. a peer process creating a new profile while the user runs
        // `aoe --profile X`). Disk rewire is scoped: unified mode tracks
        // every profile, single-profile mode stays bounded to
        // `self.storages.keys()` (the active profile, plus any profile
        // loaded via `move_to_profile`). Helpers are set-diff idempotent,
        // so the unconditional call is a no-op on a stable profile set.
        self.rewire_config_subscriptions(&current_profiles);
        if self.active_profile.is_some() {
            let mut active_only: Vec<String> = self.storages.keys().cloned().collect();
            active_only.sort();
            self.rewire_disk_subscriptions(&active_only);
        } else {
            self.rewire_disk_subscriptions(&current_profiles);
        }

        // Storage rebuild: unified mode only. Single-profile mode keeps the
        // explicit scope set at startup; only the active profile is loaded
        // into memory.
        if self.active_profile.is_none() {
            for name in &current_profiles {
                if !self.storages.contains_key(name) {
                    self.storages
                        .insert(name.clone(), Storage::new(name, self.file_watch.clone())?);
                }
            }
            self.storages.retain(|k, _| current_profiles.contains(k));
        }

        for (profile_name, storage) in &self.storages {
            let (mut instances, groups) = storage.load_with_groups()?;
            for inst in &mut instances {
                inst.source_profile = profile_name.clone();
                if let Some(prev) = self.instances.get(&inst.id) {
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

        // Snapshot the in-flight Creating stub before `self.instances` is
        // overwritten. The stub isn't on disk (added via `add_instance` from
        // `create_session` before the async worker starts) and would
        // otherwise vanish across reload.
        let creating_stub_snapshot: Option<Instance> = self
            .creating_stub_id
            .as_ref()
            .and_then(|id| self.instances.get(id).cloned());

        self.instances = Self::build_instances_map(all_instances);

        if let Some(stub) = creating_stub_snapshot {
            self.instances.entry(stub.id.clone()).or_insert(stub);
        }

        // Refresh the project registry so project view's empty pinned headers
        // and pin indicators reflect the current on-disk registry.
        self.refresh_registered_projects();

        // Remember what the cursor was pointing at so we can follow it
        let prev_selected_session = self.selected_session.clone();
        let prev_selected_group = self.selected_group.clone();

        self.rebuild_flat_items();

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

    /// Forwards to [`DiskWatchState::rewire`], lending it the
    /// `file_watch` Arc and `reload_failure_state` owned by `HomeView`.
    pub(super) fn rewire_disk_subscriptions(&mut self, current: &[String]) {
        self.disk_watch
            .rewire(&self.file_watch, current, &mut self.reload_failure_state);
    }

    /// Forwards to [`ConfigWatchState::rewire`], lending it the
    /// `file_watch` Arc and `reload_failure_state` owned by `HomeView`.
    pub(super) fn rewire_config_subscriptions(&mut self, current: &[String]) {
        self.config_watch
            .rewire(&self.file_watch, current, &mut self.reload_failure_state);
    }

    /// Rewire disk + config subscriptions after a successful profile
    /// delete. Surfaces a `Watcher Warning` dialog when
    /// `list_profiles()` cannot enumerate profiles, since the dialog
    /// is the only user-facing signal the delete path has; the next
    /// successful reload repairs watcher state.
    pub(super) fn rewire_after_profile_delete(&mut self, profile_name: &str) {
        match crate::session::list_profiles() {
            Ok(profiles) => {
                let disk_targets: Vec<String> = if self.active_profile.is_some() {
                    let mut keys: Vec<String> = self.storages.keys().cloned().collect();
                    keys.sort();
                    keys
                } else {
                    profiles.clone()
                };
                self.rewire_disk_subscriptions(&disk_targets);
                self.rewire_config_subscriptions(&profiles);
            }
            Err(e) => {
                tracing::warn!(
                    target: "tui.file_watch",
                    profile = %profile_name,
                    op = "delete_profile",
                    error = %e,
                    "list_profiles failed during rewire after profile delete; watcher state will repair on next reload"
                );
                if self.info_dialog.is_none() {
                    self.info_dialog = Some(InfoDialog::new(
                        WATCHER_WARNING_TITLE,
                        &format!(
                            "Profile '{}' was deleted but the watcher rewire could not enumerate profiles: {}\n\nThe next successful reload will repair watcher state.",
                            profile_name, e
                        ),
                    ));
                }
            }
        }
    }

    /// Open or refresh the `Reload Failed` dialog from the current
    /// `reload_failure_state`. Returns `true` when the dialog was
    /// opened or its body refreshed in place so the caller can
    /// request a redraw.
    ///
    /// Three update paths converge here:
    /// * New burst presentation: `has_unacknowledged_failure()` is
    ///   true. The dialog opens (or re-opens) and the ack latch is
    ///   consumed.
    /// * Body refresh: when a `Reload Failed` dialog is on screen
    ///   and the ack latch is acknowledged, the body is rebuilt if
    ///   the failing-source set has shifted (partial recovery that
    ///   leaves at least one source still failing, or a new source
    ///   recorded for the same acknowledged burst). The ack latch
    ///   stays in place; the user is not re-notified for the same
    ///   ongoing burst.
    /// * No-op: nothing failing, body unchanged, or an unrelated
    ///   dialog (a `Watcher Warning` from `rewire_after_profile_delete`,
    ///   or a profile create/delete `Error`) occupies the slot. In
    ///   the foreign-dialog case the ack latch stays armed so the
    ///   next tick can present once the foreign dialog is dismissed.
    pub(super) fn try_present_reload_failure_dialog(&mut self) -> bool {
        if !self.reload_failure_state.has_any_failure() {
            return false;
        }
        let title = RELOAD_FAILED_TITLE;
        let occupied_by_other = self
            .info_dialog
            .as_ref()
            .is_some_and(|d| d.title() != title);
        if occupied_by_other {
            return false;
        }

        let needs_ack = self.reload_failure_state.has_unacknowledged_failure();
        let dialog_open = self
            .info_dialog
            .as_ref()
            .is_some_and(|d| d.title() == title);

        if !needs_ack && !dialog_open {
            return false;
        }

        let body = self.reload_failure_state.build_dialog_body();
        let body_matches = self
            .info_dialog
            .as_ref()
            .is_some_and(|d| d.message() == body);
        if !needs_ack && body_matches {
            return false;
        }

        self.info_dialog = Some(InfoDialog::sized_to_fit(title, &body));
        if needs_ack {
            self.reload_failure_state.acknowledge_dialog();
        }
        true
    }

    /// Recovery-edge cleanup: clear a stale `Reload Failed` dialog
    /// when every reload source returns to healthy. Returns `true`
    /// when the dialog was cleared so the caller can request a redraw.
    /// The `Watcher Warning` dialog raised by
    /// `rewire_after_profile_delete` is intentionally outside
    /// `reload_failure_state` and is left for the user to dismiss.
    pub(super) fn try_clear_recovered_reload_dialog(&mut self) -> bool {
        if !self.reload_failure_state.has_any_failure()
            && self
                .info_dialog
                .as_ref()
                .is_some_and(|d| d.title() == RELOAD_FAILED_TITLE)
        {
            self.info_dialog = None;
            true
        } else {
            false
        }
    }

    /// Snapshot of `self.instances` eligible for status polling.
    /// In-flight recovery and restart candidates are excluded; their
    /// post-cascade `Instance` arrives via `apply_recovery_updates` /
    /// `apply_restart_results` and skipping the parallel poll prevents racing
    /// transitions during the suppression window.
    pub(super) fn pollable_instances(&self) -> Vec<Instance> {
        self.instances
            .values()
            .filter(|i| {
                !self.recovery_in_flight.contains(&i.id) && !self.restart_in_flight.contains(&i.id)
            })
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
        use std::sync::mpsc::TryRecvError;

        match self.status_poller.try_recv_updates() {
            Ok(updates) => {
                for update in updates {
                    self.apply_one_status_update(update);
                }
                self.pending_status_refresh = false;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                // The worker thread is gone (a panic in poll_statuses_once).
                // Without a respawn, pending_status_refresh stays set and
                // request_status_refresh never fires again, freezing every
                // session's live status for the rest of the process.
                tracing::error!(
                    target: "tui.home",
                    "status poller worker gone; respawning a fresh poller",
                );
                self.reset_status_refresh();
                true
            }
        }
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
            use crate::tui::status_poller::IdleIntent;

            let new_status = update.status;
            let new_error = update.last_error;
            let new_idle_entered_at = update.idle_entered_at;
            let new_live_status_baseline = update.live_status_baseline;
            self.mutate_instance(&update.id, |inst| {
                inst.status = new_status;
                inst.last_error = new_error;
                // Match on the producer's stated intent for `idle_entered_at`
                // instead of overloading `None`. See `IdleIntent` in
                // `status_poller` for the three-variant contract that
                // replaces the pre-fix `Option<DateTime<Utc>>` (which
                // conflated "producer observed a transition out of Idle" with
                // "producer has no observation"). See #2690.
                match new_idle_entered_at {
                    IdleIntent::Set(ts) => inst.idle_entered_at = Some(ts),
                    IdleIntent::Clear => inst.idle_entered_at = None,
                    IdleIntent::Keep => {}
                }
                if new_last_accessed.is_some() {
                    inst.last_accessed_at = new_last_accessed;
                }
                // A producer that has no baseline yet (`None`) must not
                // clear one the real instance already has, or every
                // subsequent poll of that instance re-seeds from `None`
                // and silently disables restamping on real transitions.
                // Locked by
                // [`apply_status_update_propagates_live_status_baseline_from_poller`]
                // in `src/tui/home/tests.rs`. See #2690.
                if let Some(baseline) = new_live_status_baseline {
                    inst.live_status_baseline = Some(baseline);
                }
                inst.pane_dead_observed = new_pane_dead;
            });

            if let Some(old) = old_status {
                if old != new_status {
                    // Auto-mark unread when a turn finishes (Running ->
                    // Idle), unless the user is currently viewing this
                    // session in live-send. This runs in both the with-
                    // and without-hooks apply paths, so a *different*
                    // session finishing while the user is attached
                    // elsewhere still gets marked. The attached session
                    // itself is cleared on attach-return, so a turn that
                    // finishes during an attach nets to read.
                    let is_live_target = self
                        .live_send
                        .as_ref()
                        .is_some_and(|s| s.session_id == update.id);
                    // Skip when already unread (the mark is a no-op) so a
                    // re-finishing session doesn't churn the flock once
                    // per turn.
                    let already_unread =
                        self.get_instance(&update.id).is_some_and(|i| i.is_unread());
                    let should_mark_unread = crate::session::unread_enabled()
                        && old == Status::Running
                        && new_status == Status::Idle
                        && !is_live_target
                        && !already_unread;

                    // One flock for both the status/timestamp patch and the
                    // unread mark, matching the daemon's per-tick batching
                    // shape (server/mod.rs's status_poll_loop) instead of
                    // two separate Storage::update calls on the same row.
                    self.persist_passive_status_transition(&update.id, should_mark_unread);
                    if should_mark_unread {
                        self.mutate_instance(&update.id, |inst| inst.mark_unread());
                    }

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
        use std::sync::mpsc::TryRecvError;

        match self.deletion_poller.try_recv_result() {
            Ok(result) => {
                if result.success {
                    // Captured before the remove (the instance is still in
                    // `self.instances`); recorded only after the deletion is
                    // durably saved, so a failed save leaves no tombstone (#2141).
                    let recent_entry = self
                        .instances
                        .get(&result.session_id)
                        .and_then(crate::session::recent_project_entry_for);

                    // A claimed trashed-purge (#2541) commits under the flock with
                    // the #2534 restore-race recheck: if a peer restored the session
                    // mid-purge, keep the restored row and release our claim rather
                    // than dropping it. Otherwise it removes the row on disk itself,
                    // so the normal in-memory removal is skipped in favor of a
                    // reload that converges with disk.
                    if self.purge_claimed.remove(&result.session_id) {
                        match self.finalize_claimed_purge(&result.session_id) {
                            Ok(true) => {
                                self.info_dialog = Some(InfoDialog::new(
                                    "Session restored",
                                    "This session was restored while its delete ran; the record was kept, but its worktree, branch, container, or transcript may already be gone. Inspect and repair it.",
                                ));
                            }
                            Ok(false) => {
                                if let Some(entry) = recent_entry {
                                    if let Err(e) = crate::session::record_recent_project(entry) {
                                        tracing::warn!(target: "tui.home",
                                            "recording recent project after delete failed: {e}");
                                    }
                                }
                            }
                            Err(()) => {
                                // Storage failed: the row is untouched on disk (still
                                // trashed + Purge-claimed by us). Release our claim so
                                // it is not wedged until the TTL, surface the error,
                                // and let the reload bring the row back.
                                self.release_trashed_purge_claim(&result.session_id);
                                self.info_dialog = Some(InfoDialog::new(
                                    "Delete Failed",
                                    "Could not finalize the delete under the storage lock. Try again.",
                                ));
                            }
                        }
                        if let Err(e) = self.reload() {
                            tracing::warn!(target: "tui.home", "Failed to reload session state: {e}");
                        }
                        return true;
                    }

                    self.remove_instance(&result.session_id);
                    self.rebuild_group_trees();

                    if let Err(e) = self.save() {
                        tracing::error!(target: "tui.home", "Failed to save after deletion: {}", e);
                    } else if let Some(entry) = recent_entry {
                        // Best-effort; keeps the project in the wizard Recent tab.
                        if let Err(e) = crate::session::record_recent_project(entry) {
                            tracing::warn!(target: "tui.home",
                                "recording recent project after delete failed: {e}");
                        }
                    }
                    if let Err(e) = self.reload() {
                        tracing::warn!(target: "tui.home", "Failed to reload session state: {e}");
                    }
                } else {
                    // A claimed trashed-purge whose teardown failed keeps the row
                    // for retry; release our owned Purge claim so it is not wedged
                    // (a peer restore is then free to win). See #2541.
                    if self.purge_claimed.remove(&result.session_id) {
                        self.release_trashed_purge_claim(&result.session_id);
                    }
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
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                // The single worker thread is gone (a panic in
                // perform_deletion dropped result_tx). Deleting rows are
                // frozen for the StatusPoller (tier 0), so without this they
                // would sit on "Deleting" forever. Mirrors the Disconnected
                // handling in `apply_restart_results`.
                let stuck: Vec<String> = self
                    .instances
                    .values()
                    .filter(|i| i.status == Status::Deleting)
                    .map(|i| i.id.clone())
                    .collect();
                // The dead worker will never finalize any purge we claimed;
                // release every owned claim so peers are not wedged until the
                // claim TTL expires (#2541).
                let claimed: Vec<String> = self.purge_claimed.drain().collect();
                for id in &claimed {
                    self.release_trashed_purge_claim(id);
                }
                if stuck.is_empty() && claimed.is_empty() {
                    return false;
                }
                tracing::error!(
                    target: "tui.home",
                    rows = stuck.len(),
                    "deletion poller worker gone; marking stuck Deleting rows Error",
                );
                for id in &stuck {
                    self.mutate_instance(id, |inst| {
                        inst.status = Status::Error;
                        inst.last_error =
                            Some("Deletion worker crashed; session was not deleted".to_string());
                    });
                }
                if let Err(e) = self.save() {
                    tracing::error!(target: "tui.home", "Failed to save after deletion: {}", e);
                }
                true
            }
        }
    }

    /// Apply the result of a background stop. Returns true if an instance was
    /// updated so the caller can trigger a redraw.
    pub fn apply_stop_results(&mut self) -> bool {
        use crate::session::Status;
        use std::sync::mpsc::TryRecvError;

        match self.stop_poller.try_recv_result() {
            Ok(result) => {
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
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                // The single worker thread is gone (a panic in perform_stop
                // dropped result_tx). Rows were optimistically marked Stopped
                // at request time and Stopped is frozen for the StatusPoller
                // (tier 0), so a lost failure result would otherwise show
                // "Stopped" over a still-running container forever; only the
                // poller's in-flight set knows which rows those are. Mirrors
                // the Disconnected handling in `apply_restart_results`.
                let stuck = self.stop_poller.take_pending();
                if stuck.is_empty() {
                    return false;
                }
                tracing::error!(
                    target: "tui.home",
                    rows = stuck.len(),
                    "stop poller worker gone; marking in-flight stops Error",
                );
                for id in &stuck {
                    self.set_instance_error(
                        id,
                        Some("Stop worker crashed; the session may not have stopped".to_string()),
                    );
                    self.set_instance_status(id, Status::Error);
                }
                if let Err(e) = self.save() {
                    tracing::error!(target: "tui.home", "Failed to save after stop: {}", e);
                }
                true
            }
        }
    }

    /// Apply the result of a background trash prepare: persist the relocated
    /// worktree path onto the (already-trashed) row. Returns true if an
    /// instance was updated so the caller can trigger a redraw.
    pub fn apply_trash_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        match self.trash_poller.try_recv_result() {
            Ok(result) => {
                let mut changed = false;
                // Only persist the relocation if the row is still trashed. The
                // teardown ran off-thread, so a fast restore or purge could have
                // landed in between; applying the holding-area path to a row the
                // user just restored would repoint a live session's worktree
                // into `.aoe-trash/`. A purged row is gone from the map and this
                // skips too. The relocation is best-effort regardless: a later
                // reconcile pass heals a still-trashed row whose move was
                // dropped here.
                if result.relocation.is_none() {
                    // Skipped/Failed teardown: nothing moved, but the drain is
                    // this teardown's terminal path, so release its in-flight
                    // Trash claim (ownership-guarded; a claim a purge/restore
                    // seized is untouched). The row stays trashed in place and
                    // the next reconcile pass relocates it.
                    if let Some(storage) = self
                        .instances
                        .get(&result.session_id)
                        .map(|i| i.source_profile.clone())
                        .and_then(|profile| self.storages.get(&profile))
                    {
                        if let Err(e) = storage.update(|insts, _groups| {
                            crate::session::claim::release_trash_claim(insts, &result.session_id);
                            Ok(())
                        }) {
                            tracing::warn!(
                                target: "tui.session",
                                session = %result.session_id,
                                "could not release the Trash claim: {e}"
                            );
                        }
                    }
                }
                if let Some(reloc) = result.relocation {
                    // Atomic durable check-and-commit under the storage flock.
                    // The worker's pre-move re-check leaves a window before
                    // this drain in which a restore or purge (local or from a
                    // peer process) can supersede the teardown; deciding
                    // against the in-memory map would re-open that window
                    // cross-process, so the decision is re-taken on the
                    // durable row instead, serialized with the restore/purge
                    // commits. Superseded means such a peer won: the disk move
                    // is undone (a same-filesystem rename, the same tick-loop
                    // trade `restore_selected_from_trash` makes). A row absent
                    // from the map was purged locally and is skipped; its
                    // holding dir falls to the purge teardown.
                    // The decision travels through this captured slot rather
                    // than the closure's return value, so it survives an
                    // update that decided Superseded and then failed its
                    // final write; the undo below keys off the decision
                    // alone, since the durable row was already restored in
                    // that case and skipping the undo would strand its
                    // worktree in the holding area.
                    let mut decided: Option<(
                        crate::session::claim::RelocationCommit,
                        Option<crate::session::Instance>,
                    )> = None;
                    let update_result = self
                        .instances
                        .get(&result.session_id)
                        .map(|i| i.source_profile.clone())
                        .and_then(|profile| self.storages.get(&profile))
                        .map(|storage| {
                            storage.update(|insts, _groups| {
                                let commit = crate::session::claim::commit_trash_relocation(
                                    insts,
                                    &result.session_id,
                                    &reloc,
                                    chrono::Utc::now(),
                                );
                                let row = insts.iter().find(|i| i.id == result.session_id).cloned();
                                decided = Some((commit, row));
                                Ok(())
                            })
                        });
                    if let Some(Err(e)) = &update_result {
                        // A Persisted decision whose write failed needs no
                        // repair here: the durable row is still trashed at its
                        // old path and the load-time reconcile repoints it to
                        // the holding area.
                        tracing::error!(
                            target: "tui.home",
                            session = %result.session_id,
                            "failed to persist trash worktree relocation: {e}",
                        );
                    }
                    use crate::session::claim::RelocationCommit;
                    match decided {
                        Some((RelocationCommit::Persisted, _))
                            if matches!(update_result, Some(Ok(()))) =>
                        {
                            if let Some(inst) = self.instances.get_mut(&result.session_id) {
                                inst.project_path = reloc.new_project_path.clone();
                                inst.pre_trash_project_path = reloc.pre_trash_project_path.clone();
                            }
                            changed = true;
                        }
                        Some((RelocationCommit::Superseded, row)) => {
                            let undo_with =
                                row.or_else(|| self.instances.get(&result.session_id).cloned());
                            if let Some(live) = undo_with {
                                match crate::session::trash::undo_raced_relocation(&live, &reloc) {
                                    crate::session::trash::RestoreOutcome::Failed { reason } => {
                                        tracing::warn!(
                                            target: "tui.session",
                                            session = %result.session_id,
                                            "could not move a superseded trash relocation back ({reason}); worktree remains at {}",
                                            reloc.new_project_path,
                                        );
                                    }
                                    outcome => {
                                        tracing::info!(
                                            target: "tui.session",
                                            session = %result.session_id,
                                            "trash relocation superseded by a restore/claim; undone ({outcome:?})",
                                        );
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(reason) = result.relocate_warning {
                    tracing::warn!(
                        target: "tui.session",
                        session = %result.session_id,
                        "trash worktree relocation skipped: {reason}",
                    );
                }
                changed
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                // The worker thread is gone (a panic in perform_trash dropped
                // result_tx). The rows are already durably trashed, so no
                // status surgery is needed; their worktrees just did not
                // relocate. A later reconcile pass (`reconcile_trashed_location`
                // at load) moves them, so this only logs which are deferred.
                let stuck = self.trash_poller.take_pending();
                if stuck.is_empty() {
                    return false;
                }
                tracing::error!(
                    target: "tui.home",
                    rows = stuck.len(),
                    "trash poller worker gone; worktree relocation deferred to next reconcile",
                );
                false
            }
        }
    }

    /// Apply any pending session ID updates from background pollers.
    /// Returns true if any instance's in-memory `agent_session_id` changed.
    /// Tmux env may also be republished when this returns `false`
    /// (filtered or Failed paths republish the in-memory mirror).
    pub fn apply_session_id_updates(&mut self) -> bool {
        // Fast path: no poller can produce a sid update this tick, so skip
        // the whole-map snapshot clone on idle ticks (this function runs
        // every 500ms).
        if !self
            .instances
            .values()
            .any(|i| i.session_id_poller.is_some())
        {
            return false;
        }
        // `drain_and_persist_session_ids` takes `&mut [Instance]` and is
        // shared with `src/server/mod.rs`. Snapshot into a `Vec` at the
        // boundary, then re-`insert` touched ids back into the map;
        // `IndexMap::insert` on an existing key updates in place, preserving
        // position.
        let mut snapshot: Vec<Instance> = self.cloned_instances();
        let outcome =
            crate::session::sync::drain_and_persist_session_ids(&mut snapshot, &self.file_watch);
        if !outcome.touched() {
            return false;
        }
        let touched: HashSet<&str> = outcome
            .applied
            .iter()
            .chain(outcome.rolled_back.iter())
            .map(String::as_str)
            .collect();
        for inst in snapshot
            .into_iter()
            .filter(|i| touched.contains(i.id.as_str()))
        {
            self.instances.insert(inst.id.clone(), inst);
        }
        !outcome.applied.is_empty() || !outcome.rolled_back.is_empty()
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
                        Ok(crate::session::StartOutcome::ResumeFailed { sid }) => {
                            tracing::warn!(
                                target: "session.startup_recovery",
                                id = %instance_id,
                                %title,
                                %sid,
                                "resume failed; sid preserved for explicit retry",
                            );
                        }
                        Ok(crate::session::StartOutcome::Fresh) => {}
                        Ok(crate::session::StartOutcome::FreshAfterFailedResume { sid }) => {
                            // Defensive: `is_recovery_candidate` already excludes
                            // sids equal to `resume_probe_failed_sid`, so this
                            // should not normally fire here. See #2609.
                            tracing::info!(
                                target: "session.startup_recovery",
                                id = %instance_id,
                                %title,
                                %sid,
                                "started fresh; sid previously failed a resume probe",
                            );
                        }
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
                    if let Some(slot) = self.instances.get_mut(&instance_id) {
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
            self.refresh_rows_preserving_selection();
        }
        touched
    }

    /// Rebuild `flat_items` after a background worker replaced an `Instance`
    /// snapshot, preserving the current selection. Without the
    /// selection restore, a completion that reorders rows (e.g. a shifted
    /// `last_start_time` under `SortOrder::LastActivity`) would silently latch
    /// the cursor onto a neighbour, since `update_selected()` resolves through
    /// `flat_items[cursor]`. Mirrors the canonical sequence in `reload()`.
    /// Shared by `apply_recovery_updates` and `apply_restart_results`.
    fn refresh_rows_preserving_selection(&mut self) {
        let prev_selected_session = self.selected_session.clone();
        let prev_selected_group = self.selected_group.clone();

        self.rebuild_flat_items();

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

    /// Apply results from the restart poller. Writes the post-cascade `Instance`
    /// snapshot back into memory (so `restart_with_size`'s mutations and the
    /// `#[serde(skip)]` `last_start_time` survive), clears the in-flight marker,
    /// and persists. A failed cascade or preserved resume-probe failure surfaces
    /// as a "Restart Failed" dialog (the user explicitly initiated the restart).
    /// Returns true if any instance changed.
    pub fn apply_restart_results(&mut self) -> bool {
        use crate::session::Status;
        use std::sync::mpsc::TryRecvError;

        let mut touched = false;
        loop {
            match self.restart_poller.try_recv_result() {
                Ok(result) => {
                    let crate::session::restart::RestartResult {
                        session_id,
                        before,
                        mut instance,
                        outcome,
                    } = result;

                    self.restart_in_flight.remove(&session_id);

                    match outcome {
                        Ok(crate::session::StartOutcome::ResumeFailed { sid }) => {
                            tracing::warn!(
                                target: "session.restart",
                                id = %session_id,
                                %sid,
                                "resume failed; sid preserved for explicit retry",
                            );
                            self.info_dialog = Some(InfoDialog::new(
                                "Restart Failed",
                                &format!(
                                    "Resume failed for sid {sid}; preserved for explicit retry"
                                ),
                            ));
                        }
                        Ok(crate::session::StartOutcome::FreshAfterFailedResume { sid }) => {
                            tracing::info!(
                                target: "session.restart",
                                id = %session_id,
                                %sid,
                                "started fresh; sid previously failed a resume probe",
                            );
                            self.info_dialog = Some(InfoDialog::new(
                                "Restarted",
                                &format!(
                                    "Started fresh; a prior resume attempt failed for sid {sid}. \
                                     The old conversation is still reachable via the agent's \
                                     own resume/history picker."
                                ),
                            ));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                target: "session.restart",
                                id = %session_id,
                                error = %e,
                                "restart cascade failed",
                            );
                            instance.status = Status::Error;
                            instance.last_error = Some(e.clone());
                            // Surface it: a cascade failure now arrives async, so
                            // the input handler's "Restart Failed" dialog can no
                            // longer catch it (restart_selected_session returned
                            // Ok once the work was enqueued).
                            self.info_dialog = Some(InfoDialog::new(
                                "Restart Failed",
                                &format!("Could not restart session: {e}"),
                            ));
                        }
                    }

                    if let Some(slot) = self.instances.get_mut(&session_id) {
                        slot.merge_post_restart_with_baseline(&before, &instance);
                        slot.last_error = if instance.status == Status::Error {
                            instance.last_error.clone()
                        } else {
                            None
                        };
                        slot.last_error_check = instance.last_error_check;
                        slot.last_start_time = instance.last_start_time;
                        slot.retroactive_capture_excludes =
                            instance.retroactive_capture_excludes.clone();
                        touched = true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The single worker thread is gone (a panic in
                    // perform_restart dropped result_tx). Clear the in-flight set
                    // defensively so the stuck rows fall back to the StatusPoller
                    // (which marks them Error) instead of being filtered out of
                    // polling forever by `pollable_instances`. Mirrors the
                    // Disconnected handling in `apply_recovery_updates`.
                    if !self.restart_in_flight.is_empty() {
                        tracing::error!(
                            target: "session.restart",
                            "restart poller worker gone; clearing in-flight set",
                        );
                        self.restart_in_flight.clear();
                        touched = true;
                    }
                    break;
                }
            }
        }

        if touched {
            self.refresh_rows_preserving_selection();
            if let Err(e) = self.save() {
                tracing::error!(target: "tui.home", "Failed to save after restart: {}", e);
            }
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
        for inst in self.instances.values_mut() {
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
                .filter(|i| i.source_profile == data.profile)
                .map(|i| i.title.as_str())
                .collect();
            let existing_branches: Vec<&str> = self
                .instances()
                .filter(|i| i.source_profile == data.profile)
                .filter_map(|i| i.worktree_info.as_ref().map(|w| w.branch.as_str()))
                .collect();
            let taken_branches = crate::session::builder::collect_taken_branches_for_derived_dedupe(
                &existing_branches,
                &data.path,
                &data.extra_repo_paths,
                data.worktree_enabled,
                data.create_new_branch,
                data.scratch,
            );
            if let Ok(title) = crate::session::builder::resolve_title(
                &data.title,
                data.worktree_branch.as_deref(),
                data.worktree_enabled,
                &existing_titles,
                &taken_branches,
            ) {
                data.title = title;
            }
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
        self.rebuild_flat_items();

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
            .values()
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
            self.rebuild_flat_items();
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
            self.rebuild_flat_items();
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
                // The creation poller may have minted `before_start_env` while
                // bringing the container up. It is `#[serde(skip)]`, so the
                // reload above dropped it; carry it back onto the live instance
                // (mirroring the CLI's `merge_post_start` and the structured-view
                // stamp-back) so the agent launch reuses it instead of re-minting.
                let minted = instance
                    .sandbox_info
                    .as_mut()
                    .map(|sb| std::mem::take(&mut sb.before_start_env))
                    .unwrap_or_default();
                if !minted.is_empty() {
                    self.mutate_instance(&session_id, |inst| {
                        if let Some(sb) = inst.sandbox_info.as_mut() {
                            sb.before_start_env = minted.clone();
                        }
                    });
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
                    self.rebuild_flat_items();
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
        if let Err(e) = update_config(|config| {
            config.session.confirm_before_quit = false;
        }) {
            tracing::warn!(target: "tui.home", "Failed to save config: {e}");
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

    /// Expire the settings view's transient "Settings saved" toast when its
    /// window passes, so it fades even while the keyboard is idle. Returns true
    /// when a redraw is needed. No-op when the settings overlay isn't open.
    pub fn tick_settings_status(&mut self) -> bool {
        self.settings_view
            .as_mut()
            .map(|view| view.tick_status())
            .unwrap_or(false)
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

        // Poll the plugin manager's in-flight discovery / update-check task.
        if let Some(dialog) = &mut self.plugin_manager_dialog {
            if dialog.tick() {
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
            || self.volume_ignores_glob_dialog.is_some()
            || self.intro_dialog.is_some()
            || self.no_agents_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self.info_dialog.is_some()
            || self.snooze_duration_dialog.is_some()
            || self.profile_picker_dialog.is_some()
            || self.project_session_picker_dialog.is_some()
            || self.projects_dialog.is_some()
            || self.plugin_manager_dialog.is_some()
            || self.command_palette.is_some()
            || self.tool_picker_dialog.is_some()
            || self.send_message_dialog.is_some()
            || self.permission_response_dialog.is_some()
            || self.update_confirm_dialog.is_some()
            || self.telemetry_consent_dialog.is_some()
            || self.tips_dialog.is_some()
            || serve_open
            || self.settings_view.is_some()
            || self.diff_view.is_some()
    }

    /// True when live-send owns the keyboard: live mode is active and no
    /// other overlay has stolen focus on top of it. This is the same
    /// predicate `handle_key` uses to route keys straight to the agent pane,
    /// lifted into a helper so the app-level global keybindings (Ctrl+C in
    /// particular) can defer to live-send instead of quitting aoe (#2894).
    pub(in crate::tui) fn is_live_send_capturing(&self) -> bool {
        self.live_send.is_some() && !self.has_non_live_send_overlay()
    }

    /// Arm the live-send footer's "Ctrl+C sent to agent" flash. Called each
    /// time a Ctrl+C is forwarded to the agent in live mode so the reminder
    /// re-shows on every press (#2894).
    pub(super) fn flash_ctrl_c_hint(&mut self) {
        self.live_send_ctrl_c_flash_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
    }

    /// Whether the "Ctrl+C sent to agent" footer flash is currently within
    /// its display window.
    pub(super) fn live_send_ctrl_c_flash_active(&self) -> bool {
        self.live_send_ctrl_c_flash_until
            .is_some_and(|deadline| std::time::Instant::now() < deadline)
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
            || self.volume_ignores_glob_dialog.is_some()
            || self.intro_dialog.is_some()
            || self.no_agents_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self.info_dialog.is_some()
            || self.snooze_duration_dialog.is_some()
            || self.profile_picker_dialog.is_some()
            || self.project_session_picker_dialog.is_some()
            || self.projects_dialog.is_some()
            || self.plugin_manager_dialog.is_some()
            || self.command_palette.is_some()
            || self.tool_picker_dialog.is_some()
            || self.send_message_dialog.is_some()
            || self.permission_response_dialog.is_some()
            || self.update_confirm_dialog.is_some()
            || self.telemetry_consent_dialog.is_some()
            || self.tips_dialog.is_some()
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

    /// Collapse the session list to a narrow click-to-expand strip, or
    /// re-expand it. The next render reflows the preview into the freed
    /// width; in live mode the resize loop pushes the new geometry to the
    /// agent's pane. Persisted so the choice survives restarts.
    pub fn toggle_sidebar_collapsed(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.save_sidebar_collapsed();
    }

    /// Apply `mutate` to `state.toml`'s `AppStateConfig` and write it back. The
    /// failure path is logged, so a UI-preference write never fails silently.
    /// Centralizes the load/mutate/save boilerplate the home view's preference
    /// persisters would otherwise each repeat.
    fn persist_app_state(
        what: &str,
        mutate: impl FnOnce(&mut crate::session::config::AppStateConfig),
    ) {
        if let Err(e) = update_app_state(mutate) {
            tracing::warn!(target: "tui.home", "Failed to save app state ({what}): {e}");
        }
    }

    fn save_sidebar_collapsed(&self) {
        let collapsed = self.sidebar_collapsed;
        Self::persist_app_state("sidebar collapsed", |s| {
            s.home_sidebar_collapsed = Some(collapsed)
        });
    }

    fn save_list_width(&self) {
        let width = self.list_width;
        Self::persist_app_state("list width", |s| s.home_list_width = Some(width));
    }

    /// Folder paths that currently map to a real project-mode folder: the
    /// project group of every live session (across all profiles, so switching
    /// the active profile never prunes another profile's collapse state),
    /// registered empty projects, and the per-project archived sub-folders.
    /// Used to drop collapse entries for projects that no longer exist.
    fn known_project_group_paths(&self) -> std::collections::HashSet<String> {
        let mut paths = std::collections::HashSet::new();
        for inst in self.instances.values() {
            let group = project_group_name(inst);
            if group.is_empty() {
                continue;
            }
            if inst.is_archived() {
                paths.insert(crate::session::archived_project_sub_path(&group));
            } else {
                paths.insert(group);
            }
        }
        for project in &self.registered_projects {
            // Only pinned registered projects surface as empty folder headers,
            // keyed by repo label (mirroring `unpopulated_projects`).
            if project.pinned {
                paths.insert(crate::session::projects::repo_label(&project.path));
            }
        }
        paths
    }

    /// Persist the set of collapsed project-mode folders. Stored sorted for a
    /// stable on-disk order; only collapsed paths are written, so re-expanding a
    /// folder drops it from the list. Paths for projects that no longer exist
    /// are pruned here so the persisted set can't grow without bound as projects
    /// come and go.
    fn save_project_group_collapsed(&self) {
        let known = self.known_project_group_paths();
        let mut collapsed: Vec<String> = self
            .project_group_collapsed
            .iter()
            .filter(|(path, &c)| c && known.contains(path.as_str()))
            .map(|(path, _)| path.clone())
            .collect();
        collapsed.sort();
        Self::persist_app_state("project group collapsed", |s| {
            s.project_group_collapsed = collapsed
        });
    }

    pub fn toggle_preview_info(&mut self) {
        self.show_preview_info = !self.show_preview_info;
        let show = self.show_preview_info;
        Self::persist_app_state("preview info", |s| s.show_preview_info = Some(show));
    }

    /// Forget the last non-live preview resize so the next render re-asserts the
    /// preview geometry. Call whenever the agent window's real size changes out
    /// from under the preview (an attach grows it to the client; entering or
    /// leaving live mode hands the resize off and back).
    pub(super) fn clear_preview_pane_sync(&mut self) {
        self.preview_pane_synced = None;
    }

    /// Expand the synthetic Archived section if it is collapsed, persisting
    /// the change. Used when archiving a whole group, where the rows the
    /// user was looking at all sink at once and revealing the section shows
    /// where they went. Single-row archive does NOT reveal: the cursor
    /// advances to the next active session instead and the section header's
    /// count is the feedback. No-op (and no save) when already open.
    pub(super) fn reveal_archived_section(&mut self) {
        if !self.archived_section_collapsed {
            return;
        }
        self.archived_section_collapsed = false;
        Self::persist_app_state("archived section", |s| {
            s.archived_section_collapsed = Some(false)
        });
    }

    pub fn toggle_trashed_section(&mut self) {
        self.trashed_section_collapsed = !self.trashed_section_collapsed;
        self.rebuild_flat_items();
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
    }

    pub fn toggle_archived_section(&mut self) {
        self.archived_section_collapsed = !self.archived_section_collapsed;
        let collapsed = self.archived_section_collapsed;
        Self::persist_app_state("archived section", |s| {
            s.archived_section_collapsed = Some(collapsed)
        });
        self.rebuild_flat_items();
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

    pub fn instances(&self) -> impl ExactSizeIterator<Item = &Instance> + '_ {
        self.instances.values()
    }

    pub(super) fn has_instances(&self) -> bool {
        !self.instances.is_empty()
    }

    pub fn get_instance(&self, id: &str) -> Option<&Instance> {
        self.instances.get(id)
    }

    /// Materialize `self.instances` into a `Vec` for callsites that hand off
    /// a `&[Instance]` slice to a downstream API. Single seam so the day
    /// `HomeView` grows a cache, only this helper needs to change.
    pub(super) fn cloned_instances(&self) -> Vec<Instance> {
        self.instances.values().cloned().collect()
    }

    pub(super) fn cloned_instances_for_profile(&self, profile: &str) -> Vec<Instance> {
        self.instances
            .values()
            .filter(|i| i.source_profile == profile)
            .cloned()
            .collect()
    }

    /// Build the id-keyed `IndexMap` from a `Vec<Instance>` (the storage-load
    /// shape). Logs a warning on a duplicate id so a corrupt disk state
    /// surfaces in logs rather than silently keeping only the last row.
    fn build_instances_map(all_instances: Vec<Instance>) -> indexmap::IndexMap<String, Instance> {
        let mut map = indexmap::IndexMap::with_capacity(all_instances.len());
        for inst in all_instances {
            if let Some(prev) = map.insert(inst.id.clone(), inst) {
                tracing::warn!(
                    target: "tui.home",
                    id = %prev.id,
                    "duplicate session id in loaded rows; keeping later entry"
                );
            }
        }
        map
    }

    /// `cloned_instances_for_profile` on `self.active_profile` when set,
    /// else the unfiltered `cloned_instances`. The scope every UI-facing
    /// build path (flat items, project view) shares.
    pub(super) fn cloned_instances_in_active_view(&self) -> Vec<Instance> {
        match &self.active_profile {
            Some(profile) => self.cloned_instances_for_profile(profile),
            None => self.cloned_instances(),
        }
    }

    #[cfg(test)]
    #[track_caller]
    pub(super) fn instance_at(&self, idx: usize) -> &Instance {
        self.instances
            .get_index(idx)
            .map(|(_, v)| v)
            .unwrap_or_else(|| {
                panic!(
                    "instance_at: idx {idx} out of bounds (len={})",
                    self.instances.len()
                )
            })
    }

    #[cfg(test)]
    #[track_caller]
    pub(super) fn instance_at_mut(&mut self, idx: usize) -> &mut Instance {
        let len = self.instances.len();
        self.instances
            .get_index_mut(idx)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("instance_at_mut: idx {idx} out of bounds (len={len})"))
    }

    /// Returns true if any session has an animated status (Running, Waiting, Starting,
    /// Creating), which means the TUI needs periodic redraws for spinner animation.
    pub fn has_animated_sessions(&self) -> bool {
        use crate::session::Status;
        self.instances.values().any(|inst| {
            matches!(
                inst.status,
                Status::Running | Status::Waiting | Status::Starting | Status::Creating
            )
        })
    }

    /// Index of the first `flat_items` row that belongs to the pinned bottom
    /// shelf (the synthetic Archived / Trash sections), or `None` when neither
    /// section is present. The shelf is always a contiguous suffix: both
    /// sections are appended last (Archived then Trash) by `build_flat_items`,
    /// and nothing non-shelf follows them, so the first row whose path sits
    /// within either section marks where the workspace list ends and the shelf
    /// begins. The renderer splits the sidebar here and hit-testing maps clicks
    /// in the shelf region back to this suffix.
    pub(super) fn shelf_start(&self) -> Option<usize> {
        self.flat_items.iter().position(|it| match it {
            Item::Group { path, .. } => {
                crate::session::is_within_archived_section(path)
                    || crate::session::is_within_trash_section(path)
            }
            Item::Session { .. } => false,
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
            let filtered: Vec<Instance> = self.cloned_instances_in_active_view();
            let mut items = flatten_sessions_by_attention(&filtered);
            append_archived_section(&mut items, &filtered, self.archived_section_collapsed);
            append_trash_section(&mut items, &filtered, self.trashed_section_collapsed);
            return items;
        }

        let archive_pool: Vec<Instance> = self.cloned_instances_in_active_view();
        let mut items = if let Some(profile) = &self.active_profile {
            match self.group_trees.get(profile) {
                Some(tree) => flatten_tree(tree, &archive_pool, self.sort_order),
                None => Vec::new(),
            }
        } else if self.storages.len() <= 1 {
            match self.group_trees.values().next() {
                Some(tree) => flatten_tree(tree, &archive_pool, self.sort_order),
                None => Vec::new(),
            }
        } else {
            flatten_tree_all_profiles(&archive_pool, &self.group_trees, self.sort_order)
        };

        // Pin the synthetic Archived section to the bottom regardless of
        // sort order. Archived rows were filtered out of the natural flow
        // inside `flatten_tree` / `flatten_tree_all_profiles`.
        append_archived_section(&mut items, &archive_pool, self.archived_section_collapsed);
        // Trash sits below Archived, also pinned to the bottom.
        append_trash_section(&mut items, &archive_pool, self.trashed_section_collapsed);
        items
    }

    fn build_flat_items_by_project(&self) -> Vec<Item> {
        // In project mode, always merge all sessions into one tree regardless of
        // profile count. Project grouping unifies by repo across profiles.
        let base_instances: Vec<Instance> = self.cloned_instances_in_active_view();

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
            .filter(|i| !i.is_archived() && !i.is_trashed())
            .cloned()
            .collect();

        // Surface registered projects with no live session as empty "pinned"
        // headers, so a project can persist in the view without any sessions,
        // matching the WebUI where an empty project is just a registry entry.
        // Seed them as empty groups; their headers render even with zero
        // members (the phantom-header guard above only excludes archived-only
        // session groups, not deliberately pinned ones).
        let populated_labels: std::collections::HashSet<String> = tree_seed
            .iter()
            .map(|i| i.group_path.clone())
            .filter(|p| !p.is_empty())
            .collect();
        let empty_pinned: Vec<crate::session::Group> =
            crate::session::projects::unpopulated_projects(
                &populated_labels,
                &self.registered_projects,
            )
            .into_iter()
            .map(|p| crate::session::Group::new(&p.label, &p.label))
            .collect();
        let mut tree = GroupTree::new_with_groups(&tree_seed, &empty_pinned);
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
        // Trash is a flat shelf even in project mode (recovery list, not a
        // workspace), pinned below the Archived section.
        append_trash_section(&mut items, &grouped, self.trashed_section_collapsed);
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
            self.rewire_disk_subscriptions(std::slice::from_ref(&profile));
        }
        // Reconcile config-watch subscriptions explicitly so this contract
        // is local to switch_profile rather than implicit through
        // reload_storage_only's transitive call. Idempotent set-diff: the
        // global subscription is install-once and per-profile entries
        // converge to the on-disk profile set; redundant invocations are
        // no-ops.
        let config_targets = match crate::session::list_profiles() {
            Ok(profiles) => profiles,
            Err(e) => {
                tracing::warn!(
                    target: "tui.file_watch",
                    error = %e,
                    "list_profiles failed during switch_profile; reusing loaded storages for config rewire"
                );
                let mut keys: Vec<String> = self.storages.keys().cloned().collect();
                keys.sort();
                keys
            }
        };
        self.rewire_config_subscriptions(&config_targets);
        // Clear selection before reload so stale session/group refs don't linger
        self.selected_session = None;
        self.selected_group = None;
        self.selected_group_profile = None;
        self.reload()?;
        self.refresh_from_config(ConfigRefreshOrigin::Interactive);
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
    /// the chosen project's path. Opens the add-project form when none exist.
    pub(super) fn open_project_session_picker(&mut self) {
        let profile = self.config_profile();
        match crate::session::projects::load_merged(&profile) {
            Ok(projects) if projects.is_empty() => {
                self.projects_dialog = Some(ProjectsDialog::new_adding(&profile));
            }
            Ok(projects) => {
                self.project_session_picker_dialog =
                    Some(ProjectSessionPickerDialog::new(projects));
            }
            Err(e) => {
                self.info_dialog = Some(InfoDialog::new(
                    "Projects Failed",
                    &format!("Failed to load projects: {e}"),
                ));
            }
        }
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
            .instances
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
            self.rebuild_flat_items();
        } else {
            self.mutate_instance(id, |inst| inst.touch_last_accessed());
        }
    }

    /// Run the send-message work after the dialog has been dismissed: call
    /// `ensure_pane_ready` (which may auto-start or respawn), then deliver
    /// the keystrokes. Errors are surfaced via `info_dialog` so the caller
    /// (`execute_action`) only has to clear its transient status.
    ///
    pub fn execute_send_message(&mut self, session_id: &str, message: &str) {
        let target = std::mem::replace(
            &mut self.pending_send_target,
            live_send::LiveSendTarget::Agent,
        );
        let size = crate::terminal::get_size();
        // Same pane-readiness cascades as live-send: agent runs the
        // full `ensure_pane_ready` (Docker, splash, resume); terminals
        // just need their tmux session to exist with a live pane. Seed a
        // cold/dead agent pane at the terminal size for the same reason
        // live-send does (see `ensure_pane_ready_with_size`): otherwise it
        // boots at tmux's 80x24 default and runs narrow until something
        // resizes it.
        match &target {
            live_send::LiveSendTarget::Agent => {
                let outcome = self.try_mutate_instance_writeback_on_err(session_id, |inst| {
                    inst.ensure_pane_ready_with_size(size).map_err(Into::into)
                });
                match outcome {
                    Ok(Some(EnsureReadyOutcome::ResumeFailed { sid })) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Send Failed",
                            &format!("Resume failed for sid {sid}; preserved for explicit retry"),
                        ));
                        return;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Send Failed",
                            &format!("Cannot prepare session: {}", err),
                        ));
                        return;
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => {
                if let Err(e) = self.ensure_terminal_pane_ready(session_id, size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Send Failed",
                        &format!("Cannot prepare terminal: {}", e),
                    ));
                    return;
                }
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                if let Err(e) = self.ensure_container_terminal_pane_ready(session_id, size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Send Failed",
                        &format!("Cannot prepare container terminal: {}", e),
                    ));
                    return;
                }
            }
            live_send::LiveSendTarget::Tool(name) => {
                let name = name.clone();
                if let Err(e) = self.ensure_tool_pane_ready(session_id, &name, size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Send Failed",
                        &format!("Cannot prepare tool '{}': {}", name, e),
                    ));
                    return;
                }
            }
        };
        let Some(inst) = self.get_instance(session_id) else {
            self.info_dialog = Some(InfoDialog::new(
                "Send Failed",
                "Session disappeared before the message could be sent.",
            ));
            return;
        };
        let tmux_session = match &target {
            live_send::LiveSendTarget::Agent => {
                match crate::tmux::Session::new(&inst.id, &inst.title) {
                    Ok(s) => s,
                    Err(e) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Send Failed",
                            &format!("Failed to resolve session: {}", e),
                        ));
                        return;
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => crate::tmux::Session::from_name(
                &crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title),
            ),
            live_send::LiveSendTarget::ContainerTerminal => crate::tmux::Session::from_name(
                &crate::tmux::ContainerTerminalSession::generate_name(&inst.id, &inst.title),
            ),
            live_send::LiveSendTarget::Tool(name) => crate::tmux::Session::from_name(
                crate::tmux::ToolSession::new(&inst.id, &inst.title, name).session_name(),
            ),
        };
        // Agent gets a tool-specific Enter delay so paste-burst-aware
        // agents (e.g. Codex) don't swallow the final Enter. Shells in
        // the paired terminal panes don't need the delay.
        let delay = match &target {
            live_send::LiveSendTarget::Agent => crate::agents::send_keys_enter_delay(&inst.tool),
            live_send::LiveSendTarget::Terminal
            | live_send::LiveSendTarget::ContainerTerminal
            | live_send::LiveSendTarget::Tool(_) => 0,
        };
        if let Err(e) = tmux_session.send_keys_with_delay(message, delay) {
            self.info_dialog = Some(InfoDialog::new(
                "Send Failed",
                &format!("Failed to send message: {}", e),
            ));
            return;
        }
        self.stamp_last_accessed(session_id);
        if let Err(e) = self.save() {
            tracing::error!("Failed to save after send: {}", e);
        }
        if self.sort_order == crate::session::config::SortOrder::Attention {
            self.select_top_attention(None);
            self.selected_session = None;
        }
    }

    /// Send the tmux keystrokes for a permission-prompt decision straight
    /// to the selected session's agent pane. No pane-readiness wait like
    /// `execute_send_message` performs: this action only makes sense
    /// against an already-live pane showing a prompt, so there is nothing
    /// to revive.
    pub fn execute_permission_response(
        &mut self,
        session_id: &str,
        choice: crate::tui::dialogs::PermissionResponseChoice,
    ) {
        let Some(inst) = self.get_instance(session_id) else {
            return;
        };
        if inst.is_structured() {
            return;
        }
        let Some(response) =
            crate::agents::get_agent(&inst.tool).and_then(|a| a.permission_response)
        else {
            return;
        };
        let tokens = permission_response_tokens(&response, choice);
        let tmux_session = match crate::tmux::Session::new(&inst.id, &inst.title) {
            Ok(s) => s,
            Err(e) => {
                self.info_dialog = Some(InfoDialog::new(
                    "Respond Failed",
                    &format!("Failed to resolve session: {}", e),
                ));
                return;
            }
        };
        if let Err(e) = tmux_session.send_key_tokens(tokens) {
            self.info_dialog = Some(InfoDialog::new(
                "Respond Failed",
                &format!("Failed to send response: {}", e),
            ));
        }
    }
}

/// Map a decision to its agent-defined keystroke sequence. Pure and
/// tmux-free so the choice-to-field mapping is unit-testable without a
/// real pane; `execute_permission_response` is the only caller.
fn permission_response_tokens(
    response: &crate::agents::PermissionResponse,
    choice: crate::tui::dialogs::PermissionResponseChoice,
) -> &'static [crate::agents::KeyToken] {
    use crate::tui::dialogs::PermissionResponseChoice::*;
    match choice {
        Allow => response.allow,
        AllowAlways => response.allow_always,
        Deny => response.deny,
    }
}

#[cfg(test)]
mod permission_response_tokens_tests {
    use super::*;
    use crate::agents::{KeyToken, PermissionResponse};
    use crate::tui::dialogs::PermissionResponseChoice;

    #[test]
    fn maps_each_choice_to_its_own_field() {
        let response = PermissionResponse {
            allow: &[KeyToken::Literal("1")],
            allow_always: &[KeyToken::Literal("2")],
            deny: &[KeyToken::Literal("3")],
        };
        assert_eq!(
            permission_response_tokens(&response, PermissionResponseChoice::Allow),
            response.allow
        );
        assert_eq!(
            permission_response_tokens(&response, PermissionResponseChoice::AllowAlways),
            response.allow_always
        );
        assert_eq!(
            permission_response_tokens(&response, PermissionResponseChoice::Deny),
            response.deny
        );
    }
}

impl HomeView {
    /// Size to boot a cold/dead agent pane at on live-send entry: the visible
    /// preview output rect when known, else the full terminal. `preview_pane_area`
    /// is the exact rect `finalize_live_send_resize` resizes to, so seeding the
    /// boot here makes the post-boot resize a no-op for cold starts (no reflow,
    /// no SIGWINCH race). Falls back to the terminal size for the rare entry
    /// with no prior preview frame (e.g. attach-on-create), and to `None` if
    /// neither is available so tmux keeps its default.
    fn live_send_boot_size(&self) -> Option<(u16, u16)> {
        let pane = self.preview_pane_area;
        if pane.width > 0 && pane.height > 0 {
            Some((pane.width, pane.height))
        } else {
            // A zero-dimension terminal size is as unusable as no size at all;
            // drop it so the start path keeps tmux's default instead of being
            // handed `-x 0`/`-y 0`.
            crate::terminal::get_size().filter(|(cols, rows)| *cols > 0 && *rows > 0)
        }
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
    /// Returns `Err(())` if the pane could not be readied (`info_dialog` is
    /// set with the underlying error so the caller only has to clear its toast).
    pub fn prepare_live_send(&mut self, session_id: &str) -> Result<(), ()> {
        let target = std::mem::replace(
            &mut self.pending_live_send_target,
            live_send::LiveSendTarget::Agent,
        );
        let size = crate::terminal::get_size();
        // Agent targets revive the agent pane via the full
        // ensure_pane_ready cascade (Docker, splash, resume). Terminal
        // targets are simpler: the paired terminal is a plain shell,
        // so we just ensure the tmux session exists and re-spawn it if
        // the pane has died (matches `attach_terminal`).
        //
        // Boot the agent at the size it will be shown at, not tmux's 80x24
        // default. A cold-started agent that boots narrow relies on
        // `finalize_live_send_resize`'s single post-boot SIGWINCH to grow into
        // the live area, a resize that races the agent's startup and, when
        // lost, leaves the pane pinned at ~50% width until live mode is
        // re-entered. See `Instance::ensure_pane_ready_with_size`.
        let agent_boot_size = self.live_send_boot_size();
        match &target {
            live_send::LiveSendTarget::Agent => {
                let outcome = self.try_mutate_instance_writeback_on_err(session_id, |inst| {
                    inst.ensure_pane_ready_with_size(agent_boot_size)
                        .map_err(Into::into)
                });
                match outcome {
                    Ok(Some(EnsureReadyOutcome::ResumeFailed { sid })) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Live send failed",
                            &format!("Resume failed for sid {sid}; preserved for explicit retry"),
                        ));
                        return Err(());
                    }
                    Ok(_) => {}
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
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                if let Err(e) = self.ensure_container_terminal_pane_ready(session_id, size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Live send failed",
                        &format!("Cannot prepare container terminal: {}", e),
                    ));
                    return Err(());
                }
            }
            live_send::LiveSendTarget::Tool(name) => {
                let name = name.clone();
                if let Err(e) = self.ensure_tool_pane_ready(session_id, &name, size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Live send failed",
                        &format!("Cannot prepare tool '{}': {}", name, e),
                    ));
                    return Err(());
                }
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
        let tmux_name = match &target {
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
            live_send::LiveSendTarget::Tool(name) => {
                crate::tmux::ToolSession::new(&inst.id, &inst.title, name)
                    .session_name()
                    .to_string()
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
        // Entering live-send means the user is now viewing this session, so
        // clear any unread marker.
        self.clear_unread_on_view(&inst.id);
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
        Ok(())
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
        // Size through `Session::resize_window` so the pane lands at exactly
        // `pane.height` after tmux's status-bar chrome (#2766), matching the
        // worker's Resize arm and the passive preview sync. A raw
        // `resize-window -y pane.height` leaves a `pane.height - chrome` pane
        // one row shorter than the preview output area, desyncing the live
        // preview by a row (#2742).
        let session = crate::tmux::Session::from_name(&tmux_name);
        // Only register the dedup if the session still exists (so the resize was
        // actually attempted). If it died between our state install and now,
        // leaving `live_send_last_resize` as None lets the next
        // `refresh_preview_cache_if_needed` retry through the worker.
        if session.exists() {
            session.resize_window(pane.width, pane.height);
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
            let tui_rows: Vec<Instance> = self.cloned_instances_for_profile(profile_name);
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
                "Dropped peer-deleted rows from in-memory mirror"
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
        let drop: HashSet<&str> = ids.iter().map(String::as_str).collect();
        self.instances.retain(|k, _| !drop.contains(k.as_str()));
        if self
            .selected_session
            .as_ref()
            .is_some_and(|s| drop.contains(s.as_str()))
        {
            self.selected_session = None;
        }
        self.rebuild_group_trees();
        self.rebuild_flat_items();
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
                .values()
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
        let still_used = self.instances.values().any(|i| {
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
                        .values()
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

    /// Centralized instance addition: inserts into the ordered map (preserves
    /// insertion order = sidebar order) and records the id in `pending_added`
    /// so the next `save` distinguishes TUI-new rows from peer-deleted ones
    /// (which look identical at the disk layer: missing from sessions.json).
    pub(super) fn add_instance(&mut self, instance: Instance) {
        // Count only finalized session inserts for the opt-in create-trend
        // counter (#1897). `add_instance` is also the funnel for `Creating`
        // placeholder stubs in the async creation flow (removed and replaced by
        // the real row on success), so counting every call would double-count a
        // successful background create and count a cancelled one that never
        // finalized. A real create is never `Creating`. Mirrors the serve side's
        // single increment in `create_session`; no-op when not opted in.
        if instance.status != crate::session::Status::Creating {
            super::app::record_session_create();
        }
        self.pending_added
            .entry(instance.source_profile.clone())
            .or_default()
            .insert(instance.id.clone());
        self.instances.insert(instance.id.clone(), instance);
    }

    /// Centralized instance removal: shift-removes from the ordered map
    /// (preserves the order of trailing rows; swap_remove would silently
    /// reorder the sidebar), records the id in `pending_deletions` so the
    /// next `save` propagates the removal under the flock, and clears any
    /// `pending_added` entry so an add+remove in the same save cycle does
    /// not end up persisted. Idempotent: safe to call on ids already
    /// removed.
    pub(super) fn remove_instance(&mut self, id: &str) {
        if let Some(inst) = self.instances.get(id) {
            let profile = inst.source_profile.clone();
            self.pending_deletions
                .entry(profile.clone())
                .or_default()
                .insert(id.to_string());
            if let Some(set) = self.pending_added.get_mut(&profile) {
                set.remove(id);
            }
        }
        self.instances.shift_remove(id);
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

    /// Centralized instance mutation: applies `f` to the entry in place.
    /// No-op on unknown ids so callers can be idempotent (matches
    /// `remove_instance`).
    pub(super) fn mutate_instance(&mut self, id: &str, f: impl FnOnce(&mut Instance)) {
        if let Some(inst) = self.instances.get_mut(id) {
            f(inst);
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
        let Some(old_profile) = self.instances.get(id).map(|i| i.source_profile.clone()) else {
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

        if let Some(inst) = self.instances.get_mut(id) {
            inst.group_path = new_group_path;
            inst.source_profile = target.to_string();
        }
        Ok(())
    }

    /// Persist a passively-detected status transition for one instance so
    /// the next disk reload (a TUI relaunch, or a peer like `aoe serve`)
    /// finds disk already caught up instead of comparing against a stale
    /// snapshot and misreading it as a fresh transition. See #2690. Best
    /// effort: unlike `apply_user_action`, a write failure here does not
    /// roll back the in-memory status update, since the poller is the sole
    /// authority on live status regardless of whether disk persistence
    /// succeeds.
    ///
    /// `mark_unread` folds the Running -> Idle unread mark into the same
    /// `Storage::update` call instead of a second flock round-trip on the
    /// same row in the same tick, matching the daemon's per-tick batching
    /// shape in `status_poll_loop`.
    pub(super) fn persist_passive_status_transition(&self, id: &str, mark_unread: bool) {
        let Some(inst) = self.instances.get(id) else {
            return;
        };
        let Some(storage) = self.storages.get(&inst.source_profile) else {
            return;
        };
        let patch = crate::session::PassiveStatusPatch::from_instance(inst);
        if let Err(e) = storage.update(|insts, _groups| {
            if let Some(disk) = insts.iter_mut().find(|i| i.id == id) {
                disk.merge_passive_status_patch(id, &patch);
                if mark_unread {
                    disk.mark_unread();
                }
            }
            Ok(())
        }) {
            // Best-effort persistence (see method docstring): a write
            // failure here does not roll back the in-memory update, but
            // silence would obscure a persistent flock timeout or EIO
            // loop. The daemon's sibling path in
            // `api::persist_session_update` logs the same class of
            // failure at `target: "http.api.sessions"`; log here so a
            // TUI-only user has parity visibility under
            // `AOE_LOG_LEVEL=debug`.
            tracing::warn!(
                target: "session.store",
                session_id = %id,
                "persist_passive_status_transition failed: {e}"
            );
        }
    }

    /// Atomic per-action mutate: in-memory once, disk via
    /// `Instance::merge_user_action_diff` under the flock. On disk persist
    /// failure, in-memory is rolled back to `pre` so memory and disk stay
    /// consistent.
    pub(super) fn apply_user_action<F>(&mut self, id: &str, mutate: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut Instance),
    {
        self.apply_user_action_with(id, mutate, |_| {})
    }

    /// [`Self::apply_user_action`] with a same-flock post-mutation hook on the
    /// DISK row, run inside the same `storage.update` closure after
    /// `merge_user_action_diff`. For durable state the diff-merge deliberately
    /// drops (`op_claim`, #2541) that must still land atomically with the
    /// action: the trash path uses it so a peer can never read a trashed row
    /// without its in-flight Trash claim. The hook mutates the disk row only;
    /// in-memory rows never carry claims.
    pub(super) fn apply_user_action_with<F, H>(
        &mut self,
        id: &str,
        mutate: F,
        post_disk: H,
    ) -> anyhow::Result<()>
    where
        F: FnOnce(&mut Instance),
        H: FnOnce(&mut Instance),
    {
        let Some(profile) = self.instances.get(id).map(|i| i.source_profile.clone()) else {
            return Ok(());
        };
        let Some(in_mem) = self.instances.get_mut(id) else {
            return Ok(());
        };
        let pre = in_mem.clone();
        mutate(in_mem);
        let post = in_mem.clone();

        let id_owned = id.to_string();
        let res = if let Some(storage) = self.storages.get(&profile) {
            storage.update(|insts, _groups| {
                if let Some(disk) = insts.iter_mut().find(|i| i.id == id_owned) {
                    disk.merge_user_action_diff(&pre, &post);
                    post_disk(disk);
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
                if let Some(slot) = self.instances.get_mut(id) {
                    *slot = pre;
                }
                Err(e)
            }
        }
    }

    /// Clear the unread marker because the user engaged with the session
    /// (Tab into live-send, Enter to attach, or dwell on it in the list).
    /// Runs regardless of the feature flag so a stale marker can't survive a
    /// disable/re-enable and reappear later; only writes when the session is
    /// actually unread, so an already-read session doesn't churn the storage
    /// flock.
    pub(crate) fn clear_unread_on_view(&mut self, id: &str) {
        // Engaging with the row ends its manual-flag visit, so drop any hold;
        // otherwise a stale hold could later suppress an auto mark on this row.
        if self.manual_unread_hold.as_deref() == Some(id) {
            self.manual_unread_hold = None;
        }
        let is_unread = self.get_instance(id).is_some_and(|i| i.is_unread());
        if is_unread {
            let _ = self.apply_user_action(id, |i| i.mark_read());
        }
    }

    /// Dwell-to-read: clear the selected session's unread marker once it has
    /// stayed selected, with the list in the foreground, for `UNREAD_DWELL`.
    /// This is what separates "scrolled past it" from "stopped to read it."
    /// Driven from the app tick loop; returns true when it cleared a marker
    /// (so the caller can request a redraw).
    ///
    /// The clock is suspended (and reset) whenever the feature is off, a
    /// dialog or live-send is up (the list isn't being read then), or nothing
    /// is selected, and it restarts whenever the selection moves to a
    /// different row. A row the user just flagged unread by hand is held until
    /// the cursor leaves it (`manual_unread_hold`), so flagging it and sitting
    /// there doesn't instantly undo the mark; once you leave and come back, it
    /// clears on dwell like any other unread row.
    pub fn tick_unread_dwell(&mut self, now: std::time::Instant) -> bool {
        if !crate::session::unread_enabled() || self.has_dialog() {
            self.unread_dwell = None;
            return false;
        }
        let Some(id) = self.selected_session.clone() else {
            self.unread_dwell = None;
            return false;
        };
        // The manual hold only protects the row while it stays selected; the
        // moment the cursor moves elsewhere, release it so a later return reads
        // normally.
        if self
            .manual_unread_hold
            .as_deref()
            .is_some_and(|held| held != id)
        {
            self.manual_unread_hold = None;
        }
        let started = match &self.unread_dwell {
            Some((prev, started)) if *prev == id => *started,
            // First tick on this row (or selection moved): start the clock.
            _ => {
                self.unread_dwell = Some((id, now));
                return false;
            }
        };
        if now.duration_since(started) < UNREAD_DWELL {
            return false;
        }
        // A row the user just flagged by hand is held for this visit, so the
        // dwell doesn't undo the mark while they sit on it. The clock stays
        // parked on this row either way so we don't re-evaluate every tick.
        if self.manual_unread_hold.as_deref() == Some(id.as_str()) {
            return false;
        }
        if self.get_instance(&id).is_some_and(|i| i.is_unread()) {
            self.clear_unread_on_view(&id);
            return true;
        }
        false
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
            let Some(inst) = self.instances.get_mut(id) else {
                continue;
            };
            let pre = inst.clone();
            mutate(inst);
            let post = inst.clone();
            by_profile
                .entry(post.source_profile.clone())
                .or_default()
                .push((id.clone(), pre, post));
        }
        let mut peer_deleted: Vec<String> = Vec::new();
        for (profile, items) in by_profile {
            let Some(storage) = self.storages.get(&profile) else {
                tracing::warn!(
                    target: "tui.home",
                    profile = %profile,
                    count = items.len(),
                    "bulk_apply_user_action: no storage registered for profile; in-memory mutations will not persist"
                );
                continue;
            };
            let added: HashSet<String> = self
                .pending_added
                .get(&profile)
                .cloned()
                .unwrap_or_default();
            let res = storage.update(|insts, _groups| {
                let mut missing: Vec<String> = Vec::new();
                for (id, pre, post) in &items {
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
                        if let Some(slot) = self.instances.get_mut(&id) {
                            *slot = pre;
                        }
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
    /// applies `f` to the clone, and writes back only on success; the stored
    /// entry is left untouched on `Err`.
    pub(super) fn try_mutate_instance<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut Instance) -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        if let Some(inst) = self.instances.get_mut(id) {
            let mut updated = inst.clone();
            let out = f(&mut updated)?;
            *inst = updated;
            return Ok(Some(out));
        }
        Ok(None)
    }

    /// Like `try_mutate_instance`, but writes the mutated clone back even
    /// when `f` returns `Err`.
    ///
    /// Required for callers of `Instance::restart_with_size_opts` /
    /// `ensure_pane_ready`, because the resume path can mutate
    /// `agent_session_id`, `resume_probe_failed_sid`, and
    /// `retroactive_capture_excludes` before returning `Err`. The default
    /// `try_mutate_instance` drops the mutated clone on `Err`, leaving live
    /// state inconsistent with disk until a later reload. This helper keeps
    /// the live state consistent with the attempted restart.
    pub(super) fn try_mutate_instance_writeback_on_err<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut Instance) -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        if let Some(inst) = self.instances.get_mut(id) {
            let mut updated = inst.clone();
            let result = f(&mut updated);
            *inst = updated;
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

    /// Tool-pane counterpart of `ensure_terminal_pane_ready`: mirrors
    /// `App::attach_tool_session`'s on-demand creation so live-send can
    /// target a tool (lazygit, yazi, etc.) that hasn't been launched yet.
    /// Used by `prepare_live_send` when the live target is `Tool(name)`.
    fn ensure_tool_pane_ready(
        &mut self,
        session_id: &str,
        tool_name: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        let inst = self
            .get_instance(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", session_id))?
            .clone();
        let tool_config = self
            .tool_configs
            .get(tool_name)
            .ok_or_else(|| anyhow::anyhow!("tool '{}' is not configured", tool_name))?
            .clone();
        if tool_config.command.is_empty() {
            anyhow::bail!("Tool '{}' has no command configured", tool_name);
        }
        let tool = crate::tmux::ToolSession::new(&inst.id, &inst.title, tool_name);
        if !tool.exists() || tool.is_pane_dead() {
            if tool.exists() {
                let _ = tool.kill();
            }
            tool.create_with_size(&inst.project_path, &tool_config.command, size)?;
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

    /// Reload the merged project registry into `registered_projects`. Called on
    /// every storage reload and after a pin/unpin so the project view's empty
    /// headers and pin indicators track the on-disk registry.
    ///
    /// In all-profiles mode `build_flat_items_by_project` merges sessions from
    /// every loaded profile, so the registry must too: a profile-scoped pin
    /// would otherwise lose its header (and glyph) the moment its sessions are
    /// gone. Dedupe across profiles by canonical path since each
    /// `load_merged` repeats the global entries.
    pub(super) fn refresh_registered_projects(&mut self) {
        use crate::session::projects::{canonical_key, load_merged};
        if self.active_profile.is_some() {
            self.registered_projects = load_merged(&self.config_profile()).unwrap_or_default();
            return;
        }
        let profiles: Vec<String> = self.storages.keys().cloned().collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut merged = Vec::new();
        for profile in &profiles {
            for p in load_merged(profile).unwrap_or_default() {
                if seen.insert(canonical_key(&p.path)) {
                    merged.push(p);
                }
            }
        }
        self.registered_projects = merged;
    }

    /// The canonical repo path of the first live (non-archived) session under
    /// project header `label`, or `None` when no live session populates the
    /// header (an empty pinned header). This is the header's stable repo
    /// identity, so two repos that merely share a basename are judged against
    /// their own paths rather than the shared display label.
    ///
    /// Archived sessions are excluded on purpose: an empty main-flow header is
    /// injected by LABEL match against the registry, so its pin state and
    /// unpin toggle must resolve by the same rule. Letting an archived row
    /// lend the header its path made a registry entry with a different
    /// recorded path (repo deleted or moved, so `canonical_key` compares raw
    /// strings) render an unpinnable phantom header: pinned by label, judged
    /// by path.
    pub(super) fn project_header_repo_path(&self, label: &str) -> Option<String> {
        self.instances
            .values()
            .find(|i| !i.is_archived() && project_group_name(i) == label)
            .map(|i| crate::session::projects::canonical_key(i.repo_path()))
    }

    /// Whether the project-view header `label` is backed by a registered AND
    /// pinned project. A registry entry is the "saved project"; the `pinned`
    /// flag is the separate decision to keep its header visible (#2208), so a
    /// saved-but-unpinned repo reads as not pinned (no glyph; the toggle pins
    /// it). A header with live sessions is pinned iff its own repo path is a
    /// pinned entry, so two repos sharing a basename are judged independently.
    /// An empty header exists only because a pinned project carries that
    /// basename, so the label match still requires the flag. Used for the pin
    /// indicator and the pin toggle.
    pub(super) fn is_project_label_pinned(&self, label: &str) -> bool {
        match self.project_header_repo_path(label) {
            Some(path) => self
                .registered_projects
                .iter()
                .any(|p| p.pinned && crate::session::projects::canonical_key(&p.path) == path),
            None => self
                .registered_projects
                .iter()
                .any(|p| p.pinned && crate::session::projects::repo_label(&p.path) == label),
        }
    }

    /// The project-view header label under the cursor when it is a real,
    /// pinnable project: project grouping is active, the cursor is on a group
    /// header, and that header is neither the synthetic Archived section nor
    /// the `scratch` bucket (scratch sessions have no backing repo to pin).
    pub(super) fn project_group_at_cursor(&self) -> Option<String> {
        if self.group_by != GroupByMode::Project {
            return None;
        }
        match self.flat_items.get(self.cursor) {
            Some(Item::Group { path, name, .. })
                if !crate::session::is_within_archived_section(path)
                    && !crate::session::is_within_trash_section(path)
                    && name != "scratch" =>
            {
                Some(name.clone())
            }
            _ => None,
        }
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
    ) -> Option<crate::session::AttachMode> {
        self.resolve_session_config_for(session_id)
            .map(|s| s.default_attach_mode)
    }

    /// Resolve `live_send_on_view_switch` for an existing session row:
    /// whether switching into Terminal or Tool view should auto-start
    /// live-send instead of waiting for a separate Enter/Tab/click. See
    /// `resolve_session_config_for` for resolution rules.
    pub(super) fn live_send_on_view_switch(&self, session_id: &str) -> bool {
        self.resolve_session_config_for(session_id)
            .is_some_and(|s| s.live_send_on_view_switch)
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
        Some(matches!(mode, crate::session::AttachMode::LiveSend))
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
            self.rebuild_flat_items();
        }
        if let Some(pos) = self
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == session_id))
        {
            self.cursor = pos;
        }
    }

    /// Refresh config-derived state for the active profile (Interactive
    /// path). Uses the lenient `resolve_config_or_warn` so transient
    /// parse errors fall back to defaults; user-initiated callers
    /// tolerate that because the next save will fix it. The watcher
    /// path uses `try_refresh_from_config_watcher` instead, which
    /// preserves previous in-memory state on parse failure rather than
    /// silently applying defaults.
    pub(super) fn refresh_from_config(&mut self, origin: ConfigRefreshOrigin) {
        let profile = self.config_profile();
        let config = resolve_config_or_warn(&profile);
        self.apply_config_to_state(config, origin);
    }

    /// Watcher-path counterpart of `refresh_from_config`. Returns Err on
    /// TOML parse failure for the active profile so the tick loop can
    /// preserve the previous in-memory active config rather than silently
    /// flipping safety-affecting settings (e.g. `confirm_before_quit`)
    /// to defaults. The Err is consumed by `handle_tick_reload_config` in
    /// `App::run` and surfaced in the aggregated reload-failure dialog.
    ///
    /// Peer profile coverage: `apply_config_to_state` calls
    /// `refresh_status_hook_config_cache` which loads status_hook
    /// configs for every storage'd profile, so a peer-process edit to
    /// any `<profile>/config.toml` updates the visible status-hook
    /// state even in unified mode. Peer-profile status_hooks load
    /// through the lenient `resolve_config_or_warn` and fall back to
    /// `Default::default()` on parse error; the strict-resolve
    /// guarantee applies to the active profile only.
    ///
    /// Error attribution: `resolve_config` loads the global
    /// `<app_dir>/config.toml` first then merges per-profile overrides,
    /// so a Reload Failed dialog body rendered from this path can name
    /// a global parse error even when the watcher fired for a
    /// per-profile edit. `toml::de::Error` renders line and column
    /// without the source file path.
    pub(super) fn try_refresh_from_config_watcher(&mut self) -> anyhow::Result<()> {
        let new_count = self
            .watcher_config_refresh_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.maybe_export_watcher_refresh_count(new_count);
        let profile = self.config_profile();
        let config = crate::session::resolve_config(&profile)?;
        self.apply_config_to_state(config, ConfigRefreshOrigin::Watcher);
        Ok(())
    }

    fn apply_config_to_state(
        &mut self,
        config: crate::session::Config,
        origin: ConfigRefreshOrigin,
    ) {
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
        self.agent_clipboard_forward =
            config.tmux.clipboard != crate::session::config::TmuxClipboardMode::Disabled;
        self.vt_live_enabled = config.tmux.vt_live;
        if let Some(worker) = self.preview_capture_worker.as_ref() {
            worker.set_vt_enabled(self.vt_live_enabled);
        }
        self.profile_default_attach_mode = config.session.default_attach_mode;
        self.idle_decay_window =
            crate::tui::styles::idle_decay_window(config.theme.idle_decay_minutes);
        crate::session::set_unread_enabled(config.session.unread_indicator);
        self.tips_unseen = tips_unseen_count(&config);
        self.tool_configs = config.tools;
        self.tool_hotkey_cache = input::build_tool_hotkey_cache(&self.tool_configs);
        let hotkey_warnings = input::validate_tool_hotkeys(&self.tool_configs);
        if matches!(origin, ConfigRefreshOrigin::Interactive)
            && !hotkey_warnings.is_empty()
            && self.info_dialog.is_none()
        {
            self.info_dialog = Some(InfoDialog::new(
                "Tool hotkey config errors",
                &hotkey_warnings.join("\n"),
            ));
        }
        // Watcher path: stash for tick-loop dispatch (App owns theme state).
        // Reads via `resolve_theme_name` (global-only by contract), not
        // `config.theme.name` which would carry a stale per-profile override.
        // Guard is load-bearing: Interactive already returns
        // `Action::SetTheme` directly from input handlers, so stashing
        // unconditionally would double-dispatch on every settings save.
        // Note: `resolve_theme_name` swallows read errors via `load_or_warn`
        // and falls back to "zinc"; a peer write landing between the
        // `resolve_config` above and this call momentarily flips the theme
        // and the next watcher event recovers. Diverges from the
        // "preserve prior state on Err" contract honored by other fields
        // here; acceptable since `set_theme` is idempotent and the race
        // window is microseconds wide.
        if matches!(origin, ConfigRefreshOrigin::Watcher) {
            self.pending_watcher_theme = Some(crate::session::config::resolve_theme_name());
        }
    }

    /// Drain the theme name stashed by `apply_config_to_state` on the
    /// Watcher path. The tick loop in `App::run` calls this after
    /// `try_refresh_from_config_watcher` and dispatches the result to
    /// `App::set_theme`. Returns `None` outside the watcher path or
    /// after a previous take in the same tick.
    pub(super) fn take_pending_watcher_theme(&mut self) -> Option<String> {
        self.pending_watcher_theme.take()
    }

    /// Export the watcher-config-refresh counter to a hidden file in
    /// the app dir when `AOE_E2E_DEBUG=1` is set on the TUI process.
    /// The file (`<app_dir>/.aoe_e2e_refresh_count`) is polled by the
    /// e2e harness as a deterministic completion signal for the
    /// watcher path. Production builds and non-e2e test runs never
    /// set the env var, so the file is never written. Write failures
    /// fall through to a `tracing::trace!`; the file is debug-only,
    /// so a missing write surfaces as a harness poll timeout rather
    /// than a hard error on the TUI side.
    fn maybe_export_watcher_refresh_count(&self, count: u64) {
        if std::env::var("AOE_E2E_DEBUG").as_deref() != Ok("1") {
            return;
        }
        let app_dir = match crate::session::get_app_dir() {
            Ok(p) => p,
            Err(e) => {
                tracing::trace!(
                    target: "tui.e2e_debug",
                    error = %e,
                    "AOE_E2E_DEBUG export skipped; app dir resolution failed"
                );
                return;
            }
        };
        let path = app_dir.join(".aoe_e2e_refresh_count");
        if let Err(e) = std::fs::write(&path, count.to_string()) {
            tracing::trace!(
                target: "tui.e2e_debug",
                error = %e,
                path = %path.display(),
                "AOE_E2E_DEBUG export failed"
            );
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
