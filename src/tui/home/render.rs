//! Rendering for HomeView

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::time::{Duration, Instant};

use rattles::presets::prelude as spinners;

use super::{
    get_indent, live_send, HomeView, TerminalMode, ViewMode, ICON_COLLAPSED, ICON_DELETING,
    ICON_ERROR, ICON_EXPANDED, ICON_IDLE, ICON_PINNED, ICON_STOPPED, ICON_UNKNOWN, ICON_UNREAD,
};
use crate::containers::image_update::ImageUpdate;
use crate::session::config::{GroupByMode, RowTagMode, SortOrder};
use crate::session::{Item, Status};
use crate::tui::components::preview::{self, CachedPreview};
use crate::tui::components::{
    format_scroll_indicator, set_prefixed_input_cursor_position, HelpOverlay, Preview,
};
use crate::tui::responsive;
use crate::tui::styles::{has_min_contrast, Theme};
use crate::update::UpdateInfo;

/// Derive a frame offset from a session's creation timestamp so that
/// sessions started at different times show visually distinct spinner positions.
fn session_offset(created_at: &DateTime<Utc>) -> usize {
    created_at.timestamp_millis() as usize
}

/// Build the list-pane title.
///
/// `prefix` is the leading label ("aoe", "Terminals", "Tool: <name>").
/// `profile` is `Some(name)` only when a real filter is active; when `None`,
/// the `[<profile>]` segment is omitted so the default all-profiles state
/// stays uncluttered.
/// Group and sort state hang off the prefix as `· project` / `· <sort label>`
/// segments, each dropped when it matches the default.
fn compose_list_title(
    prefix: &str,
    profile: Option<&str>,
    group_by: GroupByMode,
    sort_order: SortOrder,
) -> String {
    let mut suffix = String::new();
    if group_by == GroupByMode::Project {
        suffix.push_str(" · project");
    }
    if sort_order != SortOrder::default() {
        suffix.push_str(" · ");
        suffix.push_str(sort_order.label());
    }
    let profile_tag = match profile {
        Some(name) => format!(" [{}]", name),
        None => String::new(),
    };
    format!(" {}{}{} ", prefix, profile_tag, suffix)
}

/// Source of truth for the pane-arrangement passed to `render_list` /
/// `render_preview`, so their border masks honor DESIGN.md's single-shared-
/// separator invariant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PaneLayout {
    Collapsed,
    Stacked,
    SideBySide,
}

impl PaneLayout {
    /// Border mask for the list block, per DESIGN.md's single-shared-separator
    /// invariant. Stacked (and defensively Collapsed) drop BOTTOM because the
    /// preview's TOP is the shared horizontal seam, but keep RIGHT since the
    /// pane spans full width. SideBySide keeps BOTTOM and drops RIGHT because
    /// the preview's LEFT is the shared vertical seam.
    fn list_borders(self) -> Borders {
        match self {
            PaneLayout::Stacked | PaneLayout::Collapsed => {
                Borders::TOP | Borders::LEFT | Borders::RIGHT
            }
            PaneLayout::SideBySide => Borders::TOP | Borders::LEFT | Borders::BOTTOM,
        }
    }

    /// Border mask for the preview block. All arms yield `Borders::ALL` today
    /// because the preview always owns the full box; the match is kept
    /// exhaustive so a future asymmetric change stays type-checked instead of
    /// silently regressing.
    fn preview_borders(self) -> Borders {
        match self {
            PaneLayout::Collapsed | PaneLayout::Stacked | PaneLayout::SideBySide => Borders::ALL,
        }
    }
}

/// Extra rows captured beyond the visible window so moderate scrolls don't
/// force a fresh capture on every wheel tick. Cache invalidation uses the same
/// reserve to decide when the captured window can no longer cover the
/// requested scroll.
const CAPTURE_BUFFER: u16 = 20;

/// Trim `text` to fit within `max_width` display cells, appending '…'
/// if anything was dropped. Used by the live-send banners so a long
/// session title never pushes the exit-chord hint off-screen on a
/// narrow terminal. Returns "" when max_width is 0 (the title gets
/// sacrificed entirely so the fixed chord text wins).
fn truncate_to_width(text: &str, max_width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    // Reserve one cell for the ellipsis.
    let budget = max_width.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for c in text.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('\u{2026}');
    out
}

/// Map a tmux pane cursor onto the preview's output rect for live-send.
///
/// `output` is the rect the captured pane text paints into; `visible_rows` is
/// its height in rows; `cursor` carries the pane's `(x, y)` (counted from the
/// top of the visible screen) plus `pane_height`. Assumes the preview is at
/// the live tail, where the bottom captured line pins to the bottom of
/// `output`, so a screen row maps to `output.y + (visible_rows - pane_height)
/// + cursor.y`. When the pane is sized to the output area (the live-send
/// resize) the delta is zero and this is just `output.y + cursor.y`; the delta
/// only bites for the frame or two after a resize. A hidden cursor, or one
/// that maps outside `output` (e.g. a pane taller than the output area clips
/// its top rows), yields `None` so nothing is painted.
fn map_live_preview_cursor(
    output: Rect,
    visible_rows: usize,
    cursor: crate::tmux::PaneCursor,
) -> Option<Position> {
    if !cursor.visible {
        return None;
    }
    let row = output.y as i32 + (visible_rows as i32 - cursor.pane_height as i32) + cursor.y as i32;
    let col = output.x as i32 + cursor.x as i32;
    if row < output.y as i32
        || row >= output.y as i32 + output.height as i32
        || col < output.x as i32
        || col >= output.x as i32 + output.width as i32
    {
        return None;
    }
    Some(Position::new(col as u16, row as u16))
}

/// Number of pane lines to capture for the preview, accounting for the user's
/// scrollback offset. A small buffer is added so moderate scrolls don't force a
/// fresh capture on every wheel tick.
fn capture_lines_for(height: u16, scroll_offset: u16) -> usize {
    height
        .saturating_add(scroll_offset)
        .saturating_add(CAPTURE_BUFFER) as usize
}

/// Decide whether the cached capture window still covers the requested scroll.
/// Returns true when the cache must be re-captured because the visible window
/// (plus BUFFER headroom) would run past the end of the captured content.
fn scroll_exceeds_cache(cache_captured_lines: usize, height: u16, scroll_offset: u16) -> bool {
    let needed = (height as usize)
        .saturating_add(scroll_offset as usize)
        .saturating_add(CAPTURE_BUFFER as usize);
    needed > cache_captured_lines
}

/// Clamp the user's preview scroll offset to what the freshly captured pane
/// can actually render. Prevents the offset from drifting into "phantom"
/// territory (M3 from the multi-AI review) when tmux history is shorter than
/// `MAX_PREVIEW_SCROLL`.
///
/// `visible_height` is the rendered output-body height the caller already
/// computed (`PreviewLayout::compute(..).output.height`, shared via
/// `preview_visible_rows`), NOT the raw pane height. Re-deriving it here with a
/// fixed `- 1` would over-count the max offset by a row whenever the inner
/// banner is hidden, leaving a phantom offset that stalls live-follow one row
/// early.
fn clamp_scroll_to_capture(
    scroll_offset: u16,
    captured_lines: usize,
    visible_height: usize,
) -> u16 {
    let real_max = captured_lines.saturating_sub(visible_height) as u16;
    scroll_offset.min(real_max)
}

fn spinner_running(created_at: &DateTime<Utc>) -> &'static str {
    spinners::dots()
        .set_interval(Duration::from_millis(220))
        .offset(session_offset(created_at))
        .current_frame()
}

fn spinner_waiting(created_at: &DateTime<Utc>) -> &'static str {
    spinners::orbit()
        .set_interval(Duration::from_millis(400))
        .offset(session_offset(created_at))
        .current_frame()
}

fn spinner_starting(created_at: &DateTime<Utc>) -> &'static str {
    spinners::breathe()
        .set_interval(Duration::from_millis(180))
        .offset(session_offset(created_at))
        .current_frame()
}

/// Slow `breathe` rattle for a freshly-stopped Idle session. Reuses the
/// same animation as Starting on purpose; differentiation is by color
/// (Starting uses `theme.dimmed`, fresh-idle uses `theme.fresh_idle`).
/// The longer interval reads as "gentle reminder" rather than "actively
/// transitioning". Phase offset uses `idle_entered_at` when available so
/// sessions that just stopped don't all sync to the same frame.
fn spinner_idle_fresh(
    created_at: &DateTime<Utc>,
    idle_entered_at: Option<DateTime<Utc>>,
) -> &'static str {
    let offset_ts = idle_entered_at.unwrap_or(*created_at);
    spinners::breathe()
        .set_interval(Duration::from_millis(280))
        .offset(session_offset(&offset_ts))
        .current_frame()
}

/// Pick the structured view row icon for a session instance. Centralizes the
/// archive/snooze override that kills the live spinner for sunk rows so the
/// list reads as parked instead of "still alive." Exposed at crate visibility
/// so tests can pin the override behavior without going through the full
/// render pipeline.
pub(crate) fn agent_row_icon(inst: &crate::session::Instance) -> &'static str {
    let icon = match inst.status {
        Status::Running => spinner_running(&inst.created_at),
        Status::Waiting => spinner_waiting(&inst.created_at),
        Status::Idle => ICON_IDLE,
        Status::Unknown => ICON_UNKNOWN,
        Status::Stopped => ICON_STOPPED,
        Status::Error => ICON_ERROR,
        Status::Starting => spinner_starting(&inst.created_at),
        Status::Deleting => ICON_DELETING,
        Status::Creating => spinner_starting(&inst.created_at),
    };
    if inst.is_archived() || inst.is_snoozed() || inst.is_trashed() {
        ICON_STOPPED
    } else {
        icon
    }
}

/// Compact display code for a profile name, used by the per-row profile tag
/// in all-profiles view where the full name is too wide.
///
/// Hyphen/underscore-delimited names collapse to their segment initials
/// (`forit-backup` becomes `fb`); single-segment names take their first three
/// chars (`default` becomes `def`). Always lowercased, capped at four chars.
/// The mapping is per-name and deterministic, so two profiles that collapse to
/// the same code render identically; the full name still shows in a filtered
/// view's list title and in the New/Restart dialogs.
/// Per-row tag content plus the mode's max content width. The renderer
/// right-pads `content` to `max_width` so the bracket span is fixed-width
/// across rows (`[fb  ]` vs `[def ]`), keeping the activity column from
/// reflowing as tag widths vary. `compute_row_tag` truncates each variant
/// to the same cap it carries here, so `rendered()` never truncates.
pub(crate) struct RowTag {
    pub content: String,
    pub max_width: usize,
}

const BRANCH_TAG_WIDTH: usize = 12;

impl RowTag {
    pub fn rendered(&self) -> String {
        format!("[{:<width$}]", self.content, width = self.max_width)
    }
}

/// Compute the per-row tag for a given instance + mode, or `None` when the
/// row should not render a tag in this context.
///
/// `Auto` only renders in all-profiles view (no `active_profile`). Other
/// modes always render when their content is available (e.g. `Branch`
/// returns `None` for sessions without branch metadata).
pub(crate) fn compute_row_tag(
    inst: &crate::session::Instance,
    mode: RowTagMode,
    in_all_profiles_view: bool,
) -> Option<RowTag> {
    match mode {
        RowTagMode::None => None,
        RowTagMode::Auto => {
            if !in_all_profiles_view {
                return None;
            }
            let code = profile_short_code(&inst.source_profile);
            if code.is_empty() {
                None
            } else {
                Some(RowTag {
                    content: code,
                    max_width: 4,
                })
            }
        }
        RowTagMode::Profile => {
            let code = profile_short_code(&inst.source_profile);
            if code.is_empty() {
                None
            } else {
                Some(RowTag {
                    content: code,
                    max_width: 4,
                })
            }
        }
        RowTagMode::Sandbox => {
            if inst.is_sandboxed() {
                Some(RowTag {
                    content: "sb".to_string(),
                    max_width: 2,
                })
            } else {
                None
            }
        }
        RowTagMode::Branch => branch_row_tag(inst),
    }
}

fn branch_row_tag(inst: &crate::session::Instance) -> Option<RowTag> {
    if let Some(ws) = &inst.workspace_info {
        workspace_branch_row_tag(&ws.branch, ws.repos.len())
    } else {
        inst.worktree_info
            .as_ref()
            .and_then(|w| branch_tag_content(&w.branch, BRANCH_TAG_WIDTH))
            .map(|content| RowTag {
                content,
                max_width: BRANCH_TAG_WIDTH,
            })
    }
}

fn workspace_branch_row_tag(branch: &str, repo_count: usize) -> Option<RowTag> {
    let suffix = format!("+{repo_count}");
    let suffix_width = suffix.chars().count();
    if suffix_width >= BRANCH_TAG_WIDTH {
        return Some(RowTag {
            content: suffix.chars().take(BRANCH_TAG_WIDTH).collect(),
            max_width: BRANCH_TAG_WIDTH,
        });
    }

    let branch_width = BRANCH_TAG_WIDTH - suffix_width;
    branch_tag_content(branch, branch_width).map(|mut content| {
        content.push_str(&suffix);
        RowTag {
            content,
            max_width: BRANCH_TAG_WIDTH,
        }
    })
}

fn branch_tag_content(branch: &str, max_width: usize) -> Option<String> {
    let last = branch.rsplit('/').next().unwrap_or("");
    let trimmed: String = last.chars().take(max_width).collect();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub(crate) fn profile_short_code(profile: &str) -> String {
    let segments: Vec<&str> = profile
        .split(['-', '_'])
        .filter(|s| !s.is_empty())
        .collect();
    let code: String = match segments.as_slice() {
        [] => String::new(),
        [single] => single.chars().take(3).collect(),
        many => many
            .iter()
            .filter_map(|s| s.chars().next())
            .take(4)
            .collect(),
    };
    code.to_lowercase()
}

/// Format a timestamp as a compact relative age (e.g. `3m`, `2h`, `4d`, `2mo`).
/// Returns an empty string for `None` so callers can unconditionally substitute
/// the result without guarding for absence.
fn format_relative_age(ts: Option<DateTime<Utc>>) -> String {
    let Some(ts) = ts else {
        return String::new();
    };
    let now = Utc::now();
    let secs = (now - ts).num_seconds();
    if secs <= 0 {
        return "<1m".to_string();
    }
    if secs < 60 {
        return "<1m".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h", hours);
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{}d", days);
    }
    let months = days / 30;
    format!("{}mo", months)
}

/// Format a remaining snooze duration as a compact countdown string that
/// fits in the `LAST_ACTIVITY_SLOT` (e.g. `23m`, `1h`, `5d`). Falls back
/// to `<1m` for sub-minute remainders so the user sees "about to wake"
/// rather than an empty slot. Picker tops out at 1 week; validator cap
/// is 30 days, so the day branch handles up to ~30d.
fn format_snooze_remaining(delta: chrono::Duration) -> String {
    let secs = delta.num_seconds();
    if secs < 60 {
        return "<1m".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h", hours);
    }
    let days = hours / 24;
    format!("{}d", days)
}

/// Minimum column width required to render the last-activity column.
/// When the session list is narrower than this, the column is hidden entirely.
/// Compared against `inner.width` (list pane minus 2-char border), so this is
/// effectively `home_list_width - 2`. Keeping it at 30 lets the column appear
/// for users who set `home_list_width` in the 35–45 range (the common narrow-
/// pane setting) and for mobile clients with tight pane widths; the 6-char
/// age slot plus ~24 chars for title/branch still fits comfortably.
///
/// Width reserved for the right-aligned last-activity column:
/// 5 chars for the label (e.g. `"<1m"`, `"30mo"`) + 1 char left padding.
const LAST_ACTIVITY_SLOT: usize = 6;

/// Trailing gap between the activity slot (or terminal-mode badge) and the
/// pane's right border. One cell looks consistent with the breathing room
/// other ratatui widgets leave around the rounded border without burning
/// horizontal budget on narrow panes.
const LAST_ACTIVITY_RIGHT_MARGIN: usize = 1;

const SELECTED_ROW_CONTRAST_RATIO: f32 = 3.0;

fn selected_row_style(style: Style, theme: &Theme) -> Style {
    let Some(fg) = style.fg else {
        return style.fg(theme.text).bold();
    };
    if has_min_contrast(fg, theme.session_selection, SELECTED_ROW_CONTRAST_RATIO) {
        style.bold()
    } else {
        style.fg(theme.text).bold()
    }
}

/// Decide where the right-aligned activity column lives on a session row.
///
/// `prefix_width` is the display width of the spans already pushed (indent,
/// icon, title, optional branch info). `list_width` is the inner width of
/// the list pane. `badge_width` is 0 when no terminal-mode badge follows
/// the column, otherwise the badge string's length.
///
/// Returns `Some(pad_len)` if the column fits with `LAST_ACTIVITY_SLOT` for
/// the value, the badge after, and `LAST_ACTIVITY_RIGHT_MARGIN` of trailing
/// space. The padding is what the row should push between the prefix and
/// the column to right-align it. `None` means the row is too wide and the
/// column should be skipped entirely (the title takes priority).
fn activity_column_padding(
    prefix_width: usize,
    list_width: u16,
    badge_width: usize,
) -> Option<usize> {
    let trailing = LAST_ACTIVITY_SLOT + badge_width + LAST_ACTIVITY_RIGHT_MARGIN;
    let total = prefix_width.checked_add(trailing)?;
    if total <= list_width as usize {
        Some(list_width as usize - total)
    } else {
        None
    }
}

impl HomeView {
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        update_info: Option<&UpdateInfo>,
        update_status: Option<&str>,
        image_update: Option<&ImageUpdate>,
    ) {
        // Start each frame with no footer buttons and no sidebar
        // collapse/expand hit rects; the home-view render paths
        // (`render_status_bar`, `render_list` / `render_collapsed_strip`)
        // repopulate them. The takeover views (settings/diff/serve) return
        // before those run, so clearing here keeps a stale rect from a prior
        // home frame from swallowing a click on the diff/serve surface (the
        // collapse handler runs ahead of `hit_diff`). The live-send banner
        // likewise replaces the footer, leaving the list empty.
        self.footer_buttons.clear();
        self.collapse_button_area = Rect::default();
        self.expand_strip_area = Rect::default();

        // Settings view takes over the whole screen
        if let Some(ref mut settings) = self.settings_view {
            self.divider_col = None;
            self.main_area_width = 0;
            settings.render(frame, area, theme);
            // Render unsaved changes confirmation dialog over settings
            if self.settings_close_confirm {
                if let Some(dialog) = &mut self.confirm_dialog {
                    dialog.render(frame, area, theme);
                }
            }
            return;
        }

        // Diff view takes over the whole screen
        if self.diff_view.is_some() {
            self.preview_area = Rect::default();
            self.preview_pane_area = Rect::default();
            self.preview_outer_area = Rect::default();
            self.diff_area = self.active_diff_area(area);
        }
        if let Some(ref mut diff) = self.diff_view {
            // Compute diff for selected file if not cached
            let _ = diff.get_current_diff();

            // No list/preview divider exists while the diff takeover owns
            // the screen; clear it so a stale value from the previous frame
            // can't hit-test as draggable.
            self.divider_col = None;
            self.main_area_width = 0;

            diff.render(frame, area, theme);
            return;
        }

        // Serve view takes over the whole screen
        #[cfg(feature = "serve")]
        if let Some(ref serve) = self.serve_view {
            self.divider_col = None;
            self.main_area_width = 0;
            serve.render(frame, area, theme);
            return;
        }

        // Layout: main area + status bar + optional update bar at bottom.
        // The update bar surfaces both persistent update-available banners
        // (update_info) and transient toasts (update_status); we need a row
        // for it whenever either is present, otherwise toasts fired without
        // a pending update would never reach the screen.
        let has_update_bar =
            update_info.is_some() || update_status.is_some() || image_update.is_some();
        let constraints = if has_update_bar {
            vec![
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ]
        } else {
            vec![Constraint::Min(0), Constraint::Length(1)]
        };
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        // Below STACKED_BREAKPOINT (80 cols), put the list above the preview
        // instead of side-by-side. At 80 cols a side-by-side preview is only
        // ~45 cols (with default list_width 35), too cramped for output;
        // stacking gives the preview the full width.
        let available_width = main_chunks[0].width;
        self.main_area_width = available_width;
        // Collapsed sidebar: the list shrinks to a narrow click-to-expand
        // strip on the left and the preview takes the rest of the width
        // (in live mode the resize loop then reflows the agent pane). This
        // path is width-independent: a collapsed list is narrow enough that
        // re-imposing the stacked breakpoint would only waste space.
        if self.sidebar_collapsed {
            self.divider_col = None;
            let strip_width = responsive::COLLAPSED_STRIP_WIDTH.min(available_width);
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(strip_width), Constraint::Min(0)])
                .split(main_chunks[0]);
            // The full list isn't drawn, so its hit-test rects would
            // otherwise keep last frame's values and a click in the now-
            // preview area could resolve to an invisible list row (and
            // switch the live target). Zero them so mouse hit-testing can't
            // target the hidden sidebar; `render` already cleared the
            // collapse button rect, and the strip sets its own.
            self.list_area = Rect::default();
            self.list_inner_area = Rect::default();
            self.render_collapsed_strip(frame, chunks[0], theme);
            self.render_preview(frame, chunks[1], theme, PaneLayout::Collapsed);
        } else if available_width < responsive::STACKED_BREAKPOINT {
            let main_height = main_chunks[0].height;
            let list_height = responsive::stacked_list_height(main_height);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(list_height),
                    Constraint::Min(responsive::STACKED_PREVIEW_MIN),
                ])
                .split(main_chunks[0]);

            // Stacked layout has no vertical divider; only the side-by-side
            // path exposes the resize-by-drag affordance.
            self.divider_col = None;

            self.render_list(frame, chunks[0], theme, PaneLayout::Stacked);
            self.render_preview(frame, chunks[1], theme, PaneLayout::Stacked);
        } else {
            // Side-by-side: cap list width so the preview pane keeps its
            // usability floor (PREVIEW_MIN_WIDTH).
            let effective_list_width = self
                .list_width
                .min(available_width.saturating_sub(responsive::PREVIEW_MIN_WIDTH))
                .max(10);
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(effective_list_width),
                    Constraint::Min(responsive::PREVIEW_MIN_WIDTH),
                ])
                .split(main_chunks[0]);

            // Layout chunks are contiguous, so chunks[1].x is the first
            // column of the preview block, i.e. the visible left border
            // that the user perceives as the divider. Hit-test uses the
            // list's y-range (matches preview's y-range in side-by-side).
            self.divider_col = Some(chunks[1].x);

            self.render_list(frame, chunks[0], theme, PaneLayout::SideBySide);
            self.render_preview(frame, chunks[1], theme, PaneLayout::SideBySide);
        }
        self.render_status_bar(frame, main_chunks[1], theme);

        if has_update_bar {
            self.render_update_bar(
                frame,
                main_chunks[2],
                theme,
                update_info,
                update_status,
                image_update,
            );
        }

        // Render dialogs on top
        if self.show_help {
            let live_on_enter = self.help_live_on_enter().unwrap_or(matches!(
                self.profile_default_attach_mode,
                crate::session::NewSessionAttachMode::LiveSend
            ));
            HelpOverlay::render(
                frame,
                area,
                theme,
                self.sort_order,
                self.strict_hotkeys,
                live_on_enter,
                &mut self.help_scroll,
            );
        }

        // Each Option<Dialog> field on HomeView gets the same render dispatch:
        // if present, call render(frame, area, theme). Macro-collapsed to keep
        // the list of active dialog types in one place — adding a new dialog
        // means adding one line here, not stamping out another five-line
        // if-let block.
        // `&mut self.$field` so dialogs whose `render` captures screen
        // rects on the struct (currently `unified_delete_dialog` for
        // clickable Yes/No buttons) can mutate self. Dialogs with
        // `&self` render methods still work; Rust auto-derefs the
        // mutable borrow.
        macro_rules! render_dialogs {
            ($($field:ident),* $(,)?) => {
                $(
                    if let Some(dialog) = &mut self.$field {
                        dialog.render(frame, area, theme);
                    }
                )*
            };
        }

        render_dialogs!(
            new_dialog,
            confirm_dialog,
            unified_delete_dialog,
            group_delete_options_dialog,
            rename_dialog,
            worktree_name_dialog,
            restart_dialog,
            hooks_install_dialog,
            volume_ignores_glob_dialog,
            repo_trust_dialog,
            intro_dialog,
            no_agents_dialog,
            changelog_dialog,
            telemetry_consent_dialog,
            tips_dialog,
            info_dialog,
            snooze_duration_dialog,
            profile_picker_dialog,
            group_picker_dialog,
            sort_picker_dialog,
            project_session_picker_dialog,
            projects_dialog,
            plugin_manager_dialog,
            command_palette,
            tool_picker_dialog,
            send_message_dialog,
            update_confirm_dialog,
            // context_menu renders last so its small popup sits on top of
            // any underlying dialog (e.g. an info dialog opened by a
            // gated rename/delete attempt).
            context_menu,
        );
    }

    fn active_diff_area(&self, area: Rect) -> Rect {
        let Some(diff) = &self.diff_view else {
            return Rect::default();
        };

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(area);
        let content_area = layout[1];
        let effective_file_list_width = diff
            .file_list_width
            .min(content_area.width.saturating_sub(40))
            .max(5);
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(effective_file_list_width),
                Constraint::Min(40),
            ])
            .split(content_area);
        Block::default().borders(Borders::ALL).inner(panes[1])
    }

    /// Render the collapsed sidebar: a narrow bordered strip standing in
    /// for the full list. The whole strip is the click target (stored in
    /// `expand_strip_area`) and re-expands the sidebar. A `»` glyph hints
    /// the expand direction; the session count sits below it so the strip
    /// still conveys "there are N sessions here".
    fn render_collapsed_strip(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.expand_strip_area = area;
        let border_color = match self.view_mode {
            ViewMode::Structured => theme.border,
            ViewMode::Terminal | ViewMode::Tool(_) => theme.terminal_border,
        };
        // Drop the right border so the preview's left border is the single
        // shared seam, matching the expanded list and DESIGN.md's
        // "eliminate the double-border between list and preview" rule.
        let block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::BOTTOM)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let mut lines = vec![
            Line::from(Span::styled(
                "\u{00BB}",
                Style::default().fg(theme.hint).bold(),
            )),
            Line::from(""),
        ];
        // Session count stacked one digit per row, since the strip is a
        // single cell wide.
        for ch in self.instances().len().to_string().chars() {
            lines.push(Line::from(Span::styled(
                ch.to_string(),
                Style::default().fg(theme.dimmed),
            )));
        }
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, layout: PaneLayout) {
        self.list_area = area;
        let profile = self.active_profile_display();
        let title = match &self.view_mode {
            ViewMode::Structured => {
                compose_list_title("aoe", profile, self.group_by, self.sort_order)
            }
            ViewMode::Terminal => {
                compose_list_title("Terminals", profile, self.group_by, self.sort_order)
            }
            ViewMode::Tool(name) => compose_list_title(
                &format!("Tool: {}", name),
                profile,
                self.group_by,
                self.sort_order,
            ),
        };
        let (border_color, title_color) = match self.view_mode {
            ViewMode::Structured => (theme.border, theme.title),
            ViewMode::Terminal | ViewMode::Tool(_) => {
                (theme.terminal_border, theme.terminal_border)
            }
        };
        let borders = layout.list_borders();
        // Sort indicator rides `title_bottom`; ratatui only renders it when the
        // BOTTOM border exists, so it yields in stacked mode (still reachable via `s`).
        let sort_indicator = format!(" sort: {} ", self.sort_order.label());
        let mut block = Block::default()
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(title)
            .title_style(Style::default().fg(title_color).bold())
            .padding(Padding::horizontal(1));
        if borders.contains(Borders::BOTTOM) {
            block = block.title_bottom(
                Line::from(Span::styled(
                    sort_indicator,
                    Style::default().fg(theme.dimmed),
                ))
                .right_aligned(),
            );
        }

        let inner = block.inner(area);
        self.list_inner_area = inner;
        frame.render_widget(block, area);

        // Collapse affordance on the top-right border. Clicking it shrinks
        // the list to the click-to-expand strip. Drawn as an overlay on the
        // border (after the block) so its clickable rect is known exactly,
        // and skipped on a list too narrow to spare the columns without
        // colliding with the title (`render` already zeroed the rect, so the
        // narrow case needs no else).
        const COLLAPSE_LABEL: &str = " \u{00AB} ";
        const COLLAPSE_LABEL_WIDTH: u16 = 3;
        // Columns kept clear for the title that shares this top border row, so
        // the collapse affordance only draws when it won't collide with it.
        const COLLAPSE_LABEL_TITLE_RESERVE: u16 = 6;
        if area.width > COLLAPSE_LABEL_WIDTH + COLLAPSE_LABEL_TITLE_RESERVE {
            let btn_rect = Rect {
                x: area.right() - COLLAPSE_LABEL_WIDTH,
                y: area.y,
                width: COLLAPSE_LABEL_WIDTH,
                height: 1,
            };
            self.collapse_button_area = btn_rect;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    COLLAPSE_LABEL,
                    Style::default().fg(theme.hint).bold(),
                )),
                btn_rect,
            );
        }

        if self.instances().is_empty() && !self.has_any_groups() {
            let empty_text = vec![
                Line::from(""),
                Line::from("No sessions yet").style(Style::default().fg(theme.dimmed)),
                Line::from(""),
                Line::from("Press 'n' to create one").style(Style::default().fg(theme.hint)),
                Line::from("or 'aoe add .'").style(Style::default().fg(theme.hint)),
            ];
            let para = Paragraph::new(empty_text).alignment(Alignment::Center);
            frame.render_widget(para, inner);
            return;
        }

        let visible_height = if self.search_active {
            (inner.height as usize).saturating_sub(1)
        } else {
            inner.height as usize
        };
        let scroll = crate::tui::components::scroll::calculate_scroll(
            self.flat_items.len(),
            self.cursor,
            visible_height,
        );

        let mut lines: Vec<Line> = Vec::new();

        if scroll.has_more_above {
            lines.push(Line::from(Span::styled(
                format!("  [{} more above]", scroll.scroll_offset),
                Style::default().fg(theme.dimmed),
            )));
        }

        let hover_idx = self.hovered_index();
        for (i, item) in self
            .flat_items
            .iter()
            .skip(scroll.scroll_offset)
            .take(scroll.list_visible)
            .enumerate()
        {
            let abs_idx = i + scroll.scroll_offset;
            let is_selected = abs_idx == self.cursor;
            let is_hovered = !is_selected && Some(abs_idx) == hover_idx;
            let is_match =
                !self.search_matches.is_empty() && self.search_matches.contains(&abs_idx);
            let mut line = self.render_item_line(item, is_selected, is_match, theme, inner.width);
            // Selection wins over hover: when the mouse is over the
            // already-selected row, keep the brighter selected bg rather
            // than the dimmer hover bg.
            if is_selected || is_hovered {
                let pad = (inner.width as usize).saturating_sub(line.width());
                if pad > 0 {
                    line.spans.push(Span::raw(" ".repeat(pad)));
                }
                let bg = if is_selected {
                    theme.session_selection
                } else {
                    theme.selection
                };
                line = line.style(Style::default().bg(bg));
            }
            lines.push(line);
        }

        if scroll.has_more_below {
            let remaining = self.flat_items.len() - scroll.scroll_offset - scroll.list_visible;
            lines.push(Line::from(Span::styled(
                format!("  [{} more below]", remaining),
                Style::default().fg(theme.dimmed),
            )));
        }

        frame.render_widget(Paragraph::new(lines), inner);

        // Render search bar if active
        if self.search_active {
            let search_area = Rect {
                x: inner.x,
                y: inner.y + inner.height.saturating_sub(1),
                width: inner.width,
                height: 1,
            };

            let value = self.search_query.value();
            let cursor_pos = self.search_query.cursor();
            let cursor_style = Style::default().fg(theme.background).bg(theme.search);
            let text_style = Style::default().fg(theme.search);

            // Split value into: before cursor, char at cursor, after cursor
            let before: String = value.chars().take(cursor_pos).collect();
            let cursor_char: String = value
                .chars()
                .nth(cursor_pos)
                .map(|c| c.to_string())
                .unwrap_or_else(|| " ".to_string());
            let after: String = value.chars().skip(cursor_pos + 1).collect();

            let mut spans = vec![Span::styled("/", text_style)];
            if !before.is_empty() {
                spans.push(Span::styled(before, text_style));
            }
            spans.push(Span::styled(cursor_char, cursor_style));
            if !after.is_empty() {
                spans.push(Span::styled(after, text_style));
            }

            if !self.search_matches.is_empty() {
                let count_text = format!(
                    " [{}/{}]",
                    self.search_match_index + 1,
                    self.search_matches.len()
                );
                spans.push(Span::styled(count_text, Style::default().fg(theme.dimmed)));
            } else if !value.is_empty() {
                spans.push(Span::styled(" [0/0]", Style::default().fg(theme.dimmed)));
            }

            frame.render_widget(Paragraph::new(Line::from(spans)), search_area);
            if !self.has_overlay_above_search() {
                set_prefixed_input_cursor_position(frame, search_area, "/", &self.search_query);
            }
        }
    }

    fn has_overlay_above_search(&self) -> bool {
        #[cfg(feature = "serve")]
        let serve_open = self.serve_view.is_some();
        #[cfg(not(feature = "serve"))]
        let serve_open = false;

        self.show_help
            || self.new_dialog.is_some()
            || self.confirm_dialog.is_some()
            || self.unified_delete_dialog.is_some()
            || self.group_delete_options_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.worktree_name_dialog.is_some()
            || self.repo_trust_dialog.is_some()
            || self.hooks_install_dialog.is_some()
            || self.volume_ignores_glob_dialog.is_some()
            || self.intro_dialog.is_some()
            || self.no_agents_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self.telemetry_consent_dialog.is_some()
            || self.tips_dialog.is_some()
            || self.info_dialog.is_some()
            || self.profile_picker_dialog.is_some()
            || self.group_picker_dialog.is_some()
            || self.sort_picker_dialog.is_some()
            || self.project_session_picker_dialog.is_some()
            || self.projects_dialog.is_some()
            || self.plugin_manager_dialog.is_some()
            || self.command_palette.is_some()
            || self.send_message_dialog.is_some()
            || self.update_confirm_dialog.is_some()
            || serve_open
    }

    pub(super) fn render_item_line(
        &self,
        item: &Item,
        is_selected: bool,
        is_match: bool,
        theme: &Theme,
        list_width: u16,
    ) -> Line<'static> {
        let indent = get_indent(item.depth());

        // Attention-mode-gated visuals. Favorite, snooze (decoration), and
        // urgent only render when the user is in Attention sort, so the
        // sidebar stays clean for users who don't run a high-volume
        // triage workflow. Archive stays universal because it's a
        // lifecycle action (the pane is killed), and its rows live in
        // the dedicated bottom-pinned "Archived" section regardless of
        // sort mode.
        let in_attention = self.sort_order == SortOrder::Attention;

        use std::borrow::Cow;

        let (icon, text, style): (&str, Cow<str>, Style) = match item {
            Item::Group {
                path,
                name,
                collapsed,
                session_count,
                archived_at,
                ..
            } => {
                let icon = if *collapsed {
                    ICON_COLLAPSED
                } else {
                    ICON_EXPANDED
                };
                // Mark pinned project headers with a trailing pin glyph so an
                // empty (sessionless) pinned project still reads as deliberate
                // rather than stale. Project view only; the registry lookup is
                // keyed by the header label.
                let pinned = self.group_by == GroupByMode::Project
                    && !crate::session::is_within_archived_section(path)
                    && !crate::session::is_within_trash_section(path)
                    && self.is_project_label_pinned(name);
                let text = if pinned {
                    Cow::Owned(format!("{} ({}) {}", name, session_count, ICON_PINNED))
                } else {
                    Cow::Owned(format!("{} ({})", name, session_count))
                };
                let mut style = Style::default().fg(theme.group).bold();
                if crate::session::is_within_archived_section(path)
                    || crate::session::is_within_trash_section(path)
                {
                    // Synthetic Archived section header (and any
                    // project sub-folder rendered under it in Project
                    // mode): muted + italic + dim so it reads as a
                    // divider rather than a user-created group. The
                    // contained rows aren't decorated individually;
                    // the section header is the sole visual signal
                    // that those sessions are shelved. Matches the
                    // modifier set used for archived user groups so
                    // terminals with weak dimmed-fg rendering still
                    // surface the parked affordance.
                    style = Style::default()
                        .fg(theme.dimmed)
                        .add_modifier(ratatui::style::Modifier::ITALIC)
                        .add_modifier(ratatui::style::Modifier::DIM);
                } else if archived_at.is_some() {
                    // Archived user groups: italic + dim, still visible.
                    style = style
                        .add_modifier(ratatui::style::Modifier::ITALIC)
                        .add_modifier(ratatui::style::Modifier::DIM);
                }
                (icon, text, style)
            }
            Item::Session { id, .. } => {
                if let Some(inst) = self.get_instance(id) {
                    match self.view_mode {
                        ViewMode::Structured => {
                            // For Idle sessions, decay color from `fresh_idle`
                            // toward `idle` over `idle_decay_window`. A slow
                            // `breathe` rattle replaces the static braille
                            // glyph while we're inside the window, matching
                            // the animated visual language of the other
                            // attention-worthy states (Running, Waiting,
                            // Starting). Also serves as a redundant cue for
                            // colorblind users / monochrome terminals.
                            //
                            // Archive/snooze then overrides the live spinner.
                            // A shelved session's underlying status is noise;
                            // an animated row reads as "still alive" and pulls
                            // the eye away from real attention items.
                            let idle_age = inst.idle_age();
                            let is_fresh_idle =
                                matches!(idle_age, Some(age) if age < self.idle_decay_window);
                            let mut icon = match inst.status {
                                Status::Running => spinner_running(&inst.created_at),
                                Status::Waiting => spinner_waiting(&inst.created_at),
                                Status::Idle if is_fresh_idle => {
                                    spinner_idle_fresh(&inst.created_at, inst.idle_entered_at)
                                }
                                Status::Idle => ICON_IDLE,
                                Status::Unknown => ICON_UNKNOWN,
                                Status::Stopped => ICON_STOPPED,
                                Status::Error => ICON_ERROR,
                                Status::Starting => spinner_starting(&inst.created_at),
                                Status::Deleting => ICON_DELETING,
                                Status::Creating => spinner_starting(&inst.created_at),
                            };
                            // Unread paints only on resting rows
                            // (Idle/Unknown): a live status (Running/Waiting/
                            // Starting/...) supersedes it and keeps its own
                            // color AND spinner. Auto-unread only ever lands
                            // on Idle; a manual flag on a live row defers to
                            // the live state. Archived/snoozed/urgent below
                            // still override on top.
                            // Sunk rows (archived/snoozed) never paint unread:
                            // the user dismissed the row, so surfacing it as
                            // unread contradicts that. The flag stays on disk,
                            // so unarchiving/unsnoozing restores it. Snooze is
                            // checked in every sort mode here (unlike the
                            // Attention-only snooze decoration below), so a
                            // snoozed unread row outside Attention sort still
                            // drops the dot (#2571).
                            let unread_resting = crate::session::unread_enabled()
                                && inst.is_unread()
                                && !inst.is_archived()
                                && !inst.is_snoozed()
                                && matches!(inst.status, Status::Idle | Status::Unknown);
                            let color = match inst.status {
                                Status::Running => theme.running,
                                Status::Waiting => theme.waiting,
                                Status::Idle if unread_resting => theme.unread,
                                Status::Idle => {
                                    theme.idle_color_at_age(idle_age, self.idle_decay_window)
                                }
                                Status::Unknown if unread_resting => theme.unread,
                                Status::Unknown => theme.waiting,
                                Status::Stopped => theme.dimmed,
                                Status::Error => theme.error,
                                Status::Starting => theme.dimmed,
                                Status::Deleting => theme.waiting,
                                Status::Creating => theme.accent,
                            };
                            let mut style = Style::default().fg(color);
                            if unread_resting {
                                // Make unread unmistakable: a solid dot glyph
                                // plus bold, on top of the `theme.unread`
                                // color set above. A plain color swap read as
                                // too subtle (#2088 review). Sink/urgent
                                // states below still override icon + style.
                                icon = ICON_UNREAD;
                                style = style.add_modifier(ratatui::style::Modifier::BOLD);
                            }
                            if inst.is_archived() || inst.is_trashed() {
                                // Archived and trashed rows render with one
                                // uniform muted glyph regardless of underlying
                                // status. The pane is dead, so painting
                                // the persisted Running/Waiting status
                                // would be misleading. The Archived
                                // section header is the sole textual
                                // cue, so no italic/dim modifier is
                                // applied here; just a dim color.
                                icon = agent_row_icon(inst);
                                style = Style::default().fg(theme.dimmed);
                            } else if in_attention && inst.is_snoozed() {
                                // Snooze decoration is Attention-only.
                                // Outside Attention the row paints its
                                // real status (the timer keeps running;
                                // the visual treatment just doesn't
                                // surface).
                                icon = agent_row_icon(inst);
                                style = Style::default()
                                    .fg(theme.dimmed)
                                    .add_modifier(ratatui::style::Modifier::ITALIC)
                                    .add_modifier(ratatui::style::Modifier::DIM);
                            } else if in_attention && inst.is_urgent() {
                                // Urgent decoration is Attention-only.
                                // The flag still persists in non-
                                // Attention modes, but the cross-tier
                                // promoter visual only makes sense when
                                // tier ordering is in effect.
                                style = Style::default()
                                    .fg(theme.error)
                                    .add_modifier(ratatui::style::Modifier::BOLD)
                                    .add_modifier(ratatui::style::Modifier::RAPID_BLINK);
                            } else if in_attention && inst.is_favorited() {
                                // Favorite decoration is Attention-only,
                                // since favorites are within-tier pins.
                                style = style
                                    .add_modifier(ratatui::style::Modifier::BOLD)
                                    .add_modifier(ratatui::style::Modifier::UNDERLINED);
                            }
                            // Prefix priority: archive (no prefix) wins
                            // over snooze (`z `) wins over urgent (`! `)
                            // wins over favorite (`* `). All three
                            // prefixes are Attention-mode-only so users
                            // in Newest / AZ / etc. don't see decoration
                            // for state they didn't opt into managing.
                            let title_text = if inst.is_archived() || inst.is_trashed() {
                                Cow::Owned(inst.title.clone())
                            } else if in_attention && inst.is_snoozed() {
                                Cow::Owned(format!("z {}", inst.title))
                            } else if in_attention && inst.is_urgent() {
                                Cow::Owned(format!("! {}", inst.title))
                            } else if in_attention && inst.is_favorited() {
                                Cow::Owned(format!("* {}", inst.title))
                            } else {
                                Cow::Owned(inst.title.clone())
                            };
                            (icon, title_text, style)
                        }
                        ViewMode::Terminal => {
                            // For sandboxed sessions, check the appropriate terminal based on mode
                            let terminal_mode = if inst.is_sandboxed() {
                                self.get_terminal_mode(id)
                            } else {
                                TerminalMode::Host
                            };
                            let terminal_running = match terminal_mode {
                                TerminalMode::Container => inst
                                    .container_terminal_tmux_session()
                                    .map(|s| s.exists())
                                    .unwrap_or(false),
                                TerminalMode::Host => inst
                                    .terminal_tmux_session()
                                    .map(|s| s.exists())
                                    .unwrap_or(false),
                            };
                            // Sunk rows suppress the unread overlay in every
                            // sort mode, same rule as the Agent-view path (#2571).
                            let unread_overlay = crate::session::unread_enabled()
                                && inst.is_unread()
                                && !inst.is_archived()
                                && !inst.is_snoozed();
                            let (mut icon, color) = if terminal_running {
                                (spinner_running(&inst.created_at), theme.terminal_active)
                            } else if unread_overlay {
                                (ICON_UNREAD, theme.unread)
                            } else {
                                (ICON_IDLE, theme.dimmed)
                            };
                            let mut style = Style::default().fg(color);
                            if unread_overlay && !terminal_running {
                                style = style.add_modifier(ratatui::style::Modifier::BOLD);
                            }
                            if inst.is_archived() || inst.is_trashed() {
                                // Archive/trash lifecycle override mirrors the
                                // Agent-view path: dim color, stopped
                                // icon, no italic/dim modifier; the
                                // Archived section header is the cue.
                                icon = ICON_STOPPED;
                                style = Style::default().fg(theme.dimmed);
                            } else if in_attention && inst.is_snoozed() {
                                icon = ICON_STOPPED;
                                style = Style::default()
                                    .fg(theme.dimmed)
                                    .add_modifier(ratatui::style::Modifier::ITALIC)
                                    .add_modifier(ratatui::style::Modifier::DIM);
                            } else if in_attention && inst.is_urgent() {
                                style = Style::default()
                                    .fg(theme.error)
                                    .add_modifier(ratatui::style::Modifier::BOLD)
                                    .add_modifier(ratatui::style::Modifier::RAPID_BLINK);
                            } else if in_attention && inst.is_favorited() {
                                style = style
                                    .add_modifier(ratatui::style::Modifier::BOLD)
                                    .add_modifier(ratatui::style::Modifier::UNDERLINED);
                            }
                            let title_text = if inst.is_archived() || inst.is_trashed() {
                                Cow::Owned(inst.title.clone())
                            } else if in_attention && inst.is_snoozed() {
                                Cow::Owned(format!("z {}", inst.title))
                            } else if in_attention && inst.is_urgent() {
                                Cow::Owned(format!("! {}", inst.title))
                            } else if in_attention && inst.is_favorited() {
                                Cow::Owned(format!("* {}", inst.title))
                            } else {
                                Cow::Owned(inst.title.clone())
                            };
                            (icon, title_text, style)
                        }
                        ViewMode::Tool(ref tool_name) => {
                            let tool_session =
                                crate::tmux::ToolSession::new(&inst.id, &inst.title, tool_name);
                            let tool_running =
                                tool_session.exists() && !tool_session.is_pane_dead();
                            let (icon, color) = if tool_running {
                                (spinner_running(&inst.created_at), theme.terminal_active)
                            } else {
                                (ICON_IDLE, theme.dimmed)
                            };
                            let style = Style::default().fg(color);
                            (icon, Cow::Owned(inst.title.clone()), style)
                        }
                    }
                } else {
                    (
                        "?",
                        Cow::Owned(id.clone()),
                        Style::default().fg(theme.dimmed),
                    )
                }
            }
        };

        let mut line_spans = Vec::with_capacity(5);
        line_spans.push(Span::raw(indent));
        let icon_style = if is_match {
            Style::default().fg(theme.search)
        } else {
            style
        };
        line_spans.push(Span::styled(format!("{} ", icon), icon_style));
        line_spans.push(Span::styled(
            text.into_owned(),
            if is_selected {
                selected_row_style(style, theme)
            } else {
                style
            },
        ));

        if let Item::Session { id, .. } = item {
            if let Some(inst) = self.get_instance(id) {
                // Config-driven suffix next to the session title. This owns
                // the branch/profile/sandbox slot, so `None` means no suffix.
                // Counted into `used_width` below so the activity column still
                // right-aligns past the tag.
                if let Some(tag) =
                    compute_row_tag(inst, self.row_tag_mode, self.active_profile.is_none())
                {
                    let tag_style =
                        Style::default().fg(if self.row_tag_mode == RowTagMode::Branch {
                            theme.branch
                        } else {
                            theme.dimmed
                        });
                    line_spans.push(Span::styled(
                        format!("  {}", tag.rendered()),
                        if is_selected {
                            selected_row_style(tag_style, theme)
                        } else {
                            tag_style
                        },
                    ));
                }

                // Right edge of the row: optional terminal-mode badge, and
                // an activity column (last-accessed for non-Idle rows,
                // time-since-stop for Idle rows, snooze remainder for
                // snoozed rows). Both pin to the pane's right edge so the
                // column lines up vertically across the session list.
                //
                // Decision is per-row: show the column only if the prefix
                // (indent + icon + title + branch info) plus the column
                // slot and any badge fits inside `list_width`. On narrow
                // panes a long title would otherwise clip the column or
                // push it off-screen, so we hide the column for that row
                // rather than mangle the title. The badge follows existing
                // behavior (always pushed in Terminal+sandboxed mode).
                //
                // Idle-row note: column drives off `idle_entered_at`, not
                // `last_accessed_at`. The latter is bumped by user
                // interaction (attach, send-keys), which would lie about
                // how long it's actually been since the agent stopped.
                //
                // Acp-mode sessions are web-only (the TUI has no
                // structured rendering surface). Surface this with a
                // [web] badge so the user knows pressing Enter will
                // open an info dialog instead of attaching to a tmux
                // pane that doesn't exist. Takes precedence over the
                // existing container/host badge in Structured view; the
                // Terminal view keeps its existing badging because
                // the host terminal still works against the worktree.
                let badge_text: Option<&'static str> =
                    if inst.is_structured() && self.view_mode != ViewMode::Terminal {
                        // Renamed from `[web]` now that the TUI renders
                        // structured-view sessions natively; `[structured]`
                        // better describes the view the badge marks.
                        Some(" [structured]")
                    } else if self.view_mode == ViewMode::Terminal && inst.is_sandboxed() {
                        Some(match self.get_terminal_mode(id) {
                            TerminalMode::Container => " [container]",
                            TerminalMode::Host => " [host]",
                        })
                    } else {
                        None
                    };
                let badge_width = badge_text.map_or(0, |s| s.len());

                let used_width: usize = line_spans.iter().map(|s| s.width()).sum();
                let column_pad = activity_column_padding(used_width, list_width, badge_width);
                let column_fits = column_pad.is_some();
                if let Some(pad_len) = column_pad {
                    if pad_len > 0 {
                        line_spans.push(Span::raw(" ".repeat(pad_len)));
                    }
                    // In Attention mode, snoozed rows show remaining sleep
                    // time ("23m" / "1h"). Outside Attention mode, snooze
                    // is invisible (the timer still ticks; we just don't
                    // surface it) so the column falls through to the
                    // normal age path.
                    // Idle rows show time-since-stop (`idle_entered_at`)
                    // since `last_accessed_at` would lie after attach/send.
                    // Fall back to `last_accessed_at` when `idle_entered_at`
                    // is missing.
                    let snooze_remaining = if in_attention {
                        inst.snooze_remaining()
                    } else {
                        None
                    };
                    let age = if let Some(remaining) = snooze_remaining {
                        format_snooze_remaining(remaining)
                    } else {
                        let age_ts = if inst.status == Status::Idle {
                            inst.idle_entered_at.or(inst.last_accessed_at)
                        } else {
                            inst.last_accessed_at
                        };
                        format_relative_age(age_ts)
                    };
                    let padded = format!("{:>width$}", age, width = LAST_ACTIVITY_SLOT);
                    let activity_style = Style::default().fg(theme.dimmed);
                    line_spans.push(Span::styled(
                        padded,
                        if is_selected {
                            selected_row_style(activity_style, theme)
                        } else {
                            activity_style
                        },
                    ));
                }

                if let Some(badge) = badge_text {
                    let badge_style = Style::default().fg(theme.sandbox);
                    line_spans.push(Span::styled(
                        badge,
                        if is_selected {
                            selected_row_style(badge_style, theme)
                        } else {
                            badge_style
                        },
                    ));
                }
                if column_fits {
                    let trailing_margin: String =
                        std::iter::repeat_n(' ', LAST_ACTIVITY_RIGHT_MARGIN).collect();
                    line_spans.push(Span::raw(trailing_margin));
                }
            }
        }

        Line::from(line_spans)
    }

    /// Refresh preview cache if needed (session changed, dimensions changed, or timer expired)
    // pub(super) so unit tests in `super::tests` can exercise the
    // cache-preservation behavior added with the kill-switch fix
    // without standing up a full render pipeline.
    /// Keep the live-send tmux pane sized to the preview's visible output area.
    ///
    /// No-op unless live-send is currently targeting `target`: without that gate,
    /// viewing the Agent pane while live-on-Terminal would resize the *terminal*
    /// pane (the worker is bound to it) to Agent-view dimensions, mis-fitting the
    /// shell the user is typing into. Deduped against `live_send_last_resize`
    /// (shared, since only one target is live at a time) so we only fire when the
    /// user enters live mode or the preview pane is resized (terminal resize,
    /// divider drag, layout flip). Each `refresh_*_cache_if_needed` calls this
    /// with its own target so the three copies stay in lockstep.
    fn resize_live_pane_if_target(
        &mut self,
        target: live_send::LiveSendTarget,
        width: u16,
        height: u16,
    ) {
        let targets_this_pane = self.live_send.as_ref().is_some_and(|s| s.target == target);
        if !targets_this_pane || width == 0 || height == 0 {
            return;
        }
        let next = (width, height);
        if self.live_send_last_resize != Some(next) {
            if let Some(worker) = &self.live_send_worker {
                worker.resize(width, height);
            }
            self.live_send_last_resize = Some(next);
        }
    }

    /// Shared core for the four `refresh_*_preview_cache_if_needed` methods.
    /// They all run the same needs-refresh gate (session id / dimensions /
    /// scroll-exceeds / 250ms timer) and the same capture, cache-update, and
    /// scroll-clamp body; they differ only in which cache field they target,
    /// where the capture comes from, and whether live-send forces a refresh.
    ///
    /// `select` is called twice: once for the gate, once to write the result.
    /// `capture` runs between those two borrows so it can take a shared `&self`
    /// to reach `get_instance`. It returns `None` to leave the cache untouched:
    /// the agent uses that for its live-send preserve-last-good kill switch
    /// (#1501); the terminal/tool wrappers use it when the instance has gone
    /// away, matching the original "only write inside `if let Some(inst)`".
    ///
    /// `force` bypasses the idle throttle; the agent passes `in_live` here so
    /// every render refreshes the preview in live-send mode (see the throttle
    /// note in `refresh_preview_cache_if_needed`), the others pass `false`.
    fn refresh_preview_cache_core(
        &mut self,
        width: u16,
        height: u16,
        force: bool,
        select: fn(&mut Self) -> &mut super::PreviewCache,
        capture: impl FnOnce(&Self, &str, usize) -> Option<String>,
    ) {
        const PREVIEW_REFRESH_MS: u128 = 250;
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let scroll_offset = self.preview_scroll_offset;

        let cache = select(self);
        let needs_refresh = force
            || cache.session_id.as_ref() != Some(&id)
            || cache.dimensions != (width, height)
            || scroll_exceeds_cache(cache.captured_lines, height, scroll_offset)
            || cache.last_refresh.elapsed().as_millis() > PREVIEW_REFRESH_MS;
        if !needs_refresh {
            return;
        }

        let capture_lines = capture_lines_for(height, scroll_offset);
        let Some(content) = capture(self, id.as_str(), capture_lines) else {
            return;
        };

        let captured_lines = select(self).store_capture(content, id, (width, height));

        self.preview_scroll_offset = clamp_scroll_to_capture(
            self.preview_scroll_offset,
            captured_lines,
            self.preview_visible_rows,
        );
    }

    /// tmux session name backing the pane the preview currently shows, as a
    /// function of the selected session and view mode (and, for Terminal,
    /// the host/container sub-mode). `None` when nothing is selected. Drives
    /// `sync_preview_capture_worker`.
    pub(super) fn displayed_pane_tmux_name(&self) -> Option<String> {
        let id = self.selected_session.as_ref()?;
        let inst = self.get_instance(id)?;
        let name = match &self.view_mode {
            ViewMode::Structured => crate::tmux::Session::generate_name(&inst.id, &inst.title),
            ViewMode::Terminal => {
                let mode = if inst.is_sandboxed() {
                    self.get_terminal_mode(id)
                } else {
                    TerminalMode::Host
                };
                match mode {
                    TerminalMode::Host => {
                        crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title)
                    }
                    TerminalMode::Container => {
                        crate::tmux::ContainerTerminalSession::generate_name(&inst.id, &inst.title)
                    }
                }
            }
            ViewMode::Tool(tool) => crate::tmux::ToolSession::new(&inst.id, &inst.title, tool)
                .session_name()
                .to_string(),
        };
        Some(name)
    }

    /// Point the off-thread capture worker at `desired` (the displayed
    /// pane's tmux session), then retune its cadence to live-send vs. idle.
    /// One long-lived worker is spawned lazily on first use and retargeted
    /// in place (no per-switch respawn); an empty target idles it. Cheap and
    /// idempotent when the target is unchanged, so render calls it every
    /// frame. This is what keeps the worker tracking whatever the user is
    /// looking at instead of only the agent during live-send.
    pub(super) fn sync_preview_capture_worker(&mut self, desired: Option<String>) {
        // Don't spawn the worker until there's actually something to show.
        if desired.is_none() && self.preview_capture_worker.is_none() {
            self.preview_capture_target = None;
            return;
        }
        if self.preview_capture_worker.is_none() {
            self.preview_capture_worker = Some(live_send::LiveCaptureWorker::spawn(
                self.preview_wake.clone(),
            ));
        }
        if self.preview_capture_target != desired {
            if let Some(worker) = self.preview_capture_worker.as_ref() {
                worker.set_target(desired.clone().unwrap_or_default());
            }
            self.preview_capture_target = desired;
        }
        // Fast cadence only when the displayed pane IS the live-send target.
        // Viewing the agent while live-send points at a terminal (or vice
        // versa) leaves this preview a background view, so it stays on the
        // idle interval instead of forking every 25ms.
        let live = self
            .live_send
            .as_ref()
            .is_some_and(|s| self.preview_capture_target.as_deref() == Some(s.tmux_name.as_str()));
        // Terminal / container panes forward empty captures so a cleared
        // shell drops its stale text; agent / tool panes preserve the
        // last-good frame (the #1501 kill switch). The policy follows the
        // displayed pane, not just the live-send target, so a backgrounded
        // terminal preview clears the same way the live one does.
        let forward_empty = matches!(self.view_mode, ViewMode::Terminal);
        if let Some(worker) = self.preview_capture_worker.as_ref() {
            worker.set_live(live);
            worker.set_forward_empty(forward_empty);
        }
    }

    /// If the capture worker has fresh content, store it into `select`'s
    /// cache and report `true` so the caller skips the synchronous fork.
    /// Returns `false` when the worker has nothing new (cold start, or an
    /// idle/unchanged pane), leaving the caller's `refresh_preview_cache_core`
    /// to populate the cache once via the fork gate. Steady state across
    /// every view goes through here, so `tmux capture-pane` stays off the
    /// render thread.
    fn apply_worker_capture(
        &mut self,
        width: u16,
        height: u16,
        select: fn(&mut Self) -> &mut super::PreviewCache,
    ) -> bool {
        let Some(id) = self.selected_session.clone() else {
            return false;
        };
        let scroll_offset = self.preview_scroll_offset;
        let capture_lines = capture_lines_for(height, scroll_offset);
        let Some(worker) = self.preview_capture_worker.as_ref() else {
            return false;
        };
        worker.set_capture_lines(capture_lines);
        let Some(content) = worker.take_latest() else {
            return false;
        };
        let captured_lines = select(self).store_capture(content, id, (width, height));
        // `set_capture_lines` is async, so this frame may carry a capture
        // produced under a smaller line budget (the user just scrolled back
        // or the pane grew). If it doesn't cover the requested window, fall
        // through so `refresh_preview_cache_core` does a one-off synchronous
        // catch-up instead of clamping the offset against an undersized
        // capture and snapping the preview toward the live edge.
        if scroll_exceeds_cache(captured_lines, height, scroll_offset) {
            return false;
        }
        self.preview_scroll_offset =
            clamp_scroll_to_capture(scroll_offset, captured_lines, self.preview_visible_rows);
        true
    }

    pub(super) fn refresh_preview_cache_if_needed(&mut self, width: u16, height: u16) {
        // The off-thread `LiveCaptureWorker` (retargeted to this pane by
        // `sync_preview_capture_worker` in `render_preview`) keeps fresh
        // content flowing on its own thread; `apply_worker_capture` below
        // just applies the newest it has produced. The synchronous fork via
        // `refresh_preview_cache_core` remains only as the cold-start /
        // worker-empty fallback (its 250ms gate still applies there). This
        // moves the per-frame capture cost (~8.5ms on macOS, ~90% of a
        // frame; the `tui.render` `capture_us` trace measures it) off the
        // render thread for every view, not just agent live-send.
        let in_live = self.live_send.is_some();
        // While in live-send mode, keep the agent's tmux pane sized to the
        // preview's visible output area so it renders directly into view.
        self.resize_live_pane_if_target(live_send::LiveSendTarget::Agent, width, height);

        // Outside live-send nothing keeps the agent's pane sized to the
        // preview's output area. A full-screen agent is sized to whatever
        // terminal it was last attached from (usually the full window), so it
        // renders taller than the preview and the bottom-anchored capture
        // clips the top rows; opening the info header shrinks the area and
        // clips even more. Resize the detached pane to the output geometry so
        // the preview is WYSIWYG. Deduped per (session, w, h) so the 250ms poll
        // doesn't SIGWINCH-storm the agent; the dedup is invalidated on attach
        // and on live enter/exit, where the real window size changes under us.
        // Live-send owns its own resize through the worker above, so skip there.
        if !in_live && width > 0 && height > 0 {
            if let Some(id) = self.selected_session.clone() {
                let want = (id, width, height);
                if self.preview_pane_synced.as_ref() != Some(&want) {
                    // Only record the dedup once the pane actually exists and was
                    // resized. If a Stopped session we're viewing is started later
                    // without an attach in this instance to clear the dedup (e.g.
                    // a peer or the web structured view launches it), marking it synced now
                    // would pin the preview to the pre-start size and keep clipping
                    // until the next geometry change. Leaving it unset retries on
                    // the next refresh; `exists()` is cache-backed, so the retry is
                    // cheap.
                    if let Some(session) = self
                        .get_instance(&want.0)
                        .and_then(|inst| inst.tmux_session().ok())
                        .filter(|s| s.exists())
                    {
                        // Defer to an active size owner (a phone/desktop live
                        // client, or this TUI's own live-send below). The
                        // detached preview is a passive display, so it only
                        // sizes a session nobody else is driving and never
                        // claims the lock itself; leaving the dedup unset
                        // retries once the owner disconnects.
                        if !session.has_active_size_owner() {
                            session.resize_window(width, height);
                            self.preview_pane_synced = Some(want);
                        }
                    }
                }
            }
        }

        // Apply the worker's latest capture if it has fresh content; that's
        // the steady-state path and never forks. Only a cold start (worker
        // just retargeted) or an idle/unchanged pane falls through to the
        // synchronous fork below.
        if self.apply_worker_capture(width, height, |s| &mut s.preview_cache) {
            return;
        }

        // Cold-start / fallback capture via the fork-based path
        // (`Session::capture_pane` via the instance helper). The
        // 250ms gate in `refresh_preview_cache_core` keeps this from forking
        // every frame; in steady state the worker above satisfies the render.
        //
        // Live vs. non-live failure semantics differ. In live mode an empty
        // capture (which is what `Session::capture_pane` returns when
        // the session is gone OR tmux had a transient hiccup) preserves the
        // last-known-good capture so the preview doesn't flash blank (the
        // kill-switch behavior introduced in #1501). The capture closure
        // returns `None` for that case so the core leaves every cache field
        // alone, including `session_id` and `dimensions`, which document
        // "what's in `content`" and would lie if updated past a stale snapshot.
        // Outside live mode the empty content surfaces as "No output
        // available", the intended signal that the underlying session is gone.
        self.refresh_preview_cache_core(
            width,
            height,
            false,
            |s| &mut s.preview_cache,
            |s, id, capture_lines| {
                let in_live = s
                    .live_send
                    .as_ref()
                    .is_some_and(|st| st.target == live_send::LiveSendTarget::Agent);
                // Only treat an empty fork capture as "preserve the existing
                // cache" when the cache is FOR THIS SAME SESSION. If the user
                // just switched live-send from session A to session B and B's
                // first capture comes back empty, holding the kill-switch would
                // leave A's content on screen under B's header. Cross-session we
                // always overwrite, falling back to an empty body (the same
                // "session looks gone" signal the non-live path uses).
                let same_session = s.preview_cache.session_id.as_deref() == Some(id);
                let fork_capture = s
                    .get_instance(id)
                    .and_then(|inst| inst.capture_output(capture_lines).ok());
                if in_live {
                    match fork_capture {
                        Some(content) if !content.is_empty() => Some(content),
                        _ if same_session => None,
                        _ => Some(String::new()),
                    }
                } else {
                    Some(fork_capture.unwrap_or_default())
                }
            },
        );
    }

    /// Refresh terminal preview cache if needed (for host terminals)
    pub(super) fn refresh_terminal_preview_cache_if_needed(&mut self, width: u16, height: u16) {
        // Symmetric with `refresh_preview_cache_if_needed`: when live-send
        // is pointed at the host-terminal pane, keep its tmux pane sized to
        // the visible output area so a window resize or info-header toggle
        // reflows the shell instead of waiting for a live-mode re-enter.
        self.resize_live_pane_if_target(live_send::LiveSendTarget::Terminal, width, height);
        // Worker (retargeted to this pane in `render_preview`) drives the
        // steady-state refresh fork-free; the core below is the cold-start /
        // empty-worker fallback.
        if self.apply_worker_capture(width, height, |s| &mut s.terminal_preview_cache) {
            return;
        }
        self.refresh_preview_cache_core(
            width,
            height,
            false,
            |s| &mut s.terminal_preview_cache,
            |s, id, capture_lines| {
                s.get_instance(id).map(|inst| {
                    inst.terminal_tmux_session()
                        .and_then(|sess| sess.capture_pane(capture_lines))
                        .unwrap_or_default()
                })
            },
        );
    }

    /// Refresh container terminal preview cache if needed
    fn refresh_container_terminal_preview_cache_if_needed(&mut self, width: u16, height: u16) {
        // Symmetric with `refresh_preview_cache_if_needed`: when live-send
        // is pointed at the in-container shell, keep its tmux pane sized to
        // the visible output area so a window resize or info-header toggle
        // reflows immediately.
        self.resize_live_pane_if_target(
            live_send::LiveSendTarget::ContainerTerminal,
            width,
            height,
        );
        if self.apply_worker_capture(width, height, |s| &mut s.container_terminal_preview_cache) {
            return;
        }
        self.refresh_preview_cache_core(
            width,
            height,
            false,
            |s| &mut s.container_terminal_preview_cache,
            |s, id, capture_lines| {
                s.get_instance(id).map(|inst| {
                    inst.container_terminal_tmux_session()
                        .and_then(|sess| sess.capture_pane(capture_lines))
                        .unwrap_or_default()
                })
            },
        );
    }

    fn refresh_tool_preview_cache_if_needed(&mut self, width: u16, height: u16, tool_name: &str) {
        if self.apply_worker_capture(width, height, |s| &mut s.tool_preview_cache) {
            return;
        }
        self.refresh_preview_cache_core(
            width,
            height,
            false,
            |s| &mut s.tool_preview_cache,
            |s, id, capture_lines| {
                s.get_instance(id).map(|inst| {
                    crate::tmux::ToolSession::new(&inst.id, &inst.title, tool_name)
                        .capture_pane(capture_lines)
                        .unwrap_or_default()
                })
            },
        );
    }

    /// `captured_lines` from whichever preview cache is currently on screen.
    /// Both the preview's own scroll indicator and the live-send footer need
    /// the active view's line count; reading `preview_cache` (the Agent cache)
    /// unconditionally shows a stale or empty `[offset/max]` in Terminal or
    /// Tool live mode, where a different cache backs the visible output.
    /// Record the output pane's text layout for the drag-select handlers.
    /// `total_lines` is the parsed scrollback length; `first_line` is
    /// derived from the same `compute_scroll` the renderer feeds to
    /// `Paragraph::scroll`, so the snapshot agrees cell-for-cell with what
    /// was painted this frame.
    fn set_preview_text_view(&mut self, pane: Rect, total_lines: usize) {
        let first_line = preview::compute_scroll(
            total_lines,
            pane.height as usize,
            self.preview_scroll_offset,
        );
        self.preview_text_view = crate::tui::home::PreviewTextView {
            pane,
            first_line: first_line as usize,
            total_lines,
        };
    }

    /// The preview cache backing whatever the pane currently shows,
    /// resolving the sandbox container-vs-host split for Terminal view.
    /// Shared by the scroll clamp, the scroll indicator, and the
    /// drag-select copy so they all read the same content the renderer
    /// painted.
    pub(super) fn active_preview_cache(&self) -> &super::PreviewCache {
        match &self.view_mode {
            ViewMode::Structured => &self.preview_cache,
            ViewMode::Tool(_) => &self.tool_preview_cache,
            ViewMode::Terminal => {
                let mode = self
                    .selected_session
                    .as_ref()
                    .and_then(|id| self.get_instance(id).map(|inst| (id, inst)))
                    .map(|(id, inst)| {
                        if inst.is_sandboxed() {
                            self.get_terminal_mode(id)
                        } else {
                            TerminalMode::Host
                        }
                    })
                    .unwrap_or(TerminalMode::Host);
                match mode {
                    TerminalMode::Container => &self.container_terminal_preview_cache,
                    TerminalMode::Host => &self.terminal_preview_cache,
                }
            }
        }
    }

    fn active_captured_lines(&self) -> usize {
        self.active_preview_cache().captured_lines
    }

    fn render_preview(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, layout: PaneLayout) {
        let compact = area.width < responsive::STACKED_BREAKPOINT;
        let (border_color, title_color) = match self.view_mode {
            ViewMode::Structured => (theme.border, theme.title),
            ViewMode::Terminal | ViewMode::Tool(_) => {
                (theme.terminal_border, theme.terminal_border)
            }
        };
        // Live-send mode swaps the preview border and title to `accent`
        // so the pane visually matches the M-compose modal's border
        // color. Without this affordance the only on-screen tell that
        // keystrokes are being routed to the agent is the status
        // banner; users have reported losing track when the banner
        // scrolls off in compact layouts. Title is overridden too so
        // the border and title color stay consistent when live mode is
        // entered from Terminal/Tool views (where the underlying
        // `title_color` is `terminal_border`, not `title`).
        let (border_color, title_color) = if self.live_send.is_some() {
            (theme.accent, theme.accent)
        } else {
            (border_color, title_color)
        };

        let mut block = Block::default()
            .borders(layout.preview_borders())
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .padding(Padding::horizontal(1));

        // In compact mode, hoist session name + status icon into the
        // outer title so the (now omitted) info header isn't missed.
        let compact_title: Option<Line> = if compact {
            self.selected_session
                .as_ref()
                .and_then(|id| self.get_instance(id))
                .map(|inst| {
                    let idle_age = inst.idle_age();
                    let is_fresh_idle =
                        matches!(idle_age, Some(age) if age < self.idle_decay_window);
                    // An archived row is parked; its preview body renders the
                    // "Archived" placeholder. Force the compact title icon to
                    // the stopped glyph so the hoisted title can't show a live
                    // spinner from a stale (pre-poll) status and contradict it.
                    let (icon, icon_color) = if inst.is_archived() {
                        (ICON_STOPPED, theme.dimmed)
                    } else {
                        match inst.status {
                            Status::Running => (spinner_running(&inst.created_at), theme.running),
                            Status::Waiting => (spinner_waiting(&inst.created_at), theme.waiting),
                            Status::Idle if is_fresh_idle => (
                                spinner_idle_fresh(&inst.created_at, inst.idle_entered_at),
                                theme.idle_color_at_age(idle_age, self.idle_decay_window),
                            ),
                            Status::Idle => (
                                ICON_IDLE,
                                theme.idle_color_at_age(idle_age, self.idle_decay_window),
                            ),
                            Status::Unknown => (ICON_UNKNOWN, theme.waiting),
                            Status::Stopped => (ICON_STOPPED, theme.dimmed),
                            Status::Error => (ICON_ERROR, theme.error),
                            Status::Starting => (spinner_starting(&inst.created_at), theme.dimmed),
                            Status::Deleting => (ICON_DELETING, theme.waiting),
                            Status::Creating => (spinner_starting(&inst.created_at), theme.accent),
                        }
                    };
                    Line::from(vec![
                        Span::raw(" "),
                        Span::styled(icon, Style::default().fg(icon_color)),
                        Span::raw(" "),
                        Span::styled(inst.title.clone(), Style::default().fg(title_color).bold()),
                        Span::raw(" "),
                    ])
                })
        } else {
            None
        };

        if let Some(line) = compact_title {
            block = block.title(line);
        } else {
            let title = match &self.view_mode {
                ViewMode::Structured => " Preview ".to_string(),
                ViewMode::Terminal => " Terminal Preview ".to_string(),
                ViewMode::Tool(name) => format!(" {} Preview ", name),
            };
            block = block
                .title(title)
                .title_style(Style::default().fg(title_color));

            // Advertise the info-header toggle. The `i` key toggles
            // `show_preview_info`, which gates the info header in every
            // view mode now (Agent uses the worktree-flavored header,
            // Terminal/Tool use the minimal header in `render_terminal_preview`),
            // so the hint applies everywhere except the compact branch
            // above, where the outer title is already taken.
            let key = if self.strict_hotkeys { "I" } else { "i" };
            let hint_text = if self.show_preview_info {
                format!(" hide info with {key} ")
            } else {
                format!(" show info with {key} ")
            };
            let hint_style = Style::default().fg(theme.dimmed).italic();

            // When the info section is hidden, the inner ` Output ` /
            // ` Terminal Output ` banner (which usually carries the
            // scroll indicator) is also gone. Surface the indicator
            // here so users still see how far back they've scrolled.
            // With borders::ALL the inner is area - 2; with the banner
            // hidden the output paragraph claims that full inner, so the
            // visible height is `inner_height` (no extra row dropped). That
            // equals `PreviewLayout::compute(..).output.height` for the
            // hidden-header case, which is what the renderers paint into.
            let scroll_indicator = if !self.show_preview_info {
                let inner_height = area.height.saturating_sub(2);
                let visible_height = inner_height as usize;
                let captured_lines = self.active_captured_lines();
                format_scroll_indicator(captured_lines, visible_height, self.preview_scroll_offset)
            } else {
                None
            };

            let mut hint_spans = vec![Span::styled(hint_text, hint_style)];
            if let Some(ind) = scroll_indicator {
                hint_spans.push(Span::styled(ind, hint_style));
            }
            block = block.title_top(Line::from(hint_spans).right_aligned());
        }

        let inner = block.inner(area);
        self.preview_area = inner;
        // `area` is the OUTER preview rect (the block + borders + content).
        // Stash it so `App::draw_preview_only` can call back into
        // `render_preview` with the right rect on `%output` wakes; passing
        // the inner there draws a nested block.
        self.preview_outer_area = area;
        self.diff_area = Rect::default();
        // The agent-pane sub-rect of `inner`: full inner when the info
        // header is hidden or the layout is compact, otherwise inner
        // shifted down past the info section. `Preview::render_with_cache`
        // splits the same way internally, so this mirrors what the user
        // actually sees and is what we size the tmux pane to in live mode.
        // Default to `inner`; the Agent branch below refines it if it can
        // resolve the selected instance.
        self.preview_pane_area = inner;
        // Track the rows the output body actually paints into, shared with the
        // scroll clamp and the live banner so their math matches the renderer.
        // Each view branch refines this after it resolves its real pane rect to
        // exactly `pane_area.height` (see below); the seed here is only used by
        // the no-output paths (creating / no selection).
        self.preview_visible_rows = inner.height as usize;
        // Seed the text-view snapshot inert; the output branches below
        // refine it once they know their pane rect and parsed line count.
        // Paths with no scrollback (creating / no selection) leave it here
        // so a drag-select over them does nothing.
        self.preview_text_view = crate::tui::home::PreviewTextView::default();
        frame.render_widget(block, area);

        // An archived session's pane was killed on archive, so there's nothing
        // live to capture. Short-circuit every view mode to a calm "Archived"
        // placeholder instead of forking captures that come back empty and
        // surface as "No output available".
        let selected_archived = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
            .is_some_and(|inst| inst.is_archived());

        // A session whose pane is simply gone (killed, exited, server reboot)
        // with no diagnostic detail carries the generic gone-error. Present
        // that as a calm "Stopped" placeholder rather than the red crash error;
        // a real crash leaves a specific message and still renders red. Covers
        // the just-unarchived row, which sits Stopped until restarted.
        //
        // Only in Structured view: the gone-error is about the agent pane, but
        // Tool / Terminal views show a different, independently-live pane (a tool
        // session can be running while the agent has exited), so the placeholder
        // must not hide that pane's output there.
        // A trashed session's pane was also killed (on trash). Same calm
        // placeholder treatment as archived, with a restore hint.
        let selected_trashed = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
            .is_some_and(|inst| inst.is_trashed());

        let selected_stopped = !selected_archived
            && !selected_trashed
            && matches!(self.view_mode, ViewMode::Structured)
            && self
                .selected_session
                .as_ref()
                .and_then(|id| self.get_instance(id))
                .is_some_and(|inst| {
                    inst.last_error.as_deref() == Some(crate::session::TMUX_SESSION_GONE_ERROR)
                });

        // Keep the off-thread capture worker pointed at whatever pane this
        // view shows (and tuned to live-send vs. idle cadence) before any
        // refresh reads from it. Done once here, not per-branch, so the
        // creating / no-selection / archived / stopped paths also retarget or
        // tear it down (no live pane feeds `None` so the worker stops capturing).
        let desired = if selected_archived || selected_trashed || selected_stopped {
            None
        } else {
            self.displayed_pane_tmux_name()
        };
        self.sync_preview_capture_worker(desired);

        if selected_archived {
            self.render_archived_preview(frame, inner, theme);
            self.paint_preview_selection(frame, theme);
            return;
        }

        if selected_trashed {
            self.render_trashed_preview(frame, inner, theme);
            self.paint_preview_selection(frame, theme);
            return;
        }

        if selected_stopped {
            self.render_stopped_preview(frame, inner, theme);
            self.paint_preview_selection(frame, theme);
            return;
        }

        match self.view_mode {
            ViewMode::Structured => {
                // Check if selected session is being created (show hook progress)
                let is_creating = self
                    .selected_session
                    .as_ref()
                    .and_then(|id| self.get_instance(id))
                    .is_some_and(|inst| inst.status == Status::Creating);

                if is_creating {
                    self.render_creating_preview(frame, inner, theme);
                } else {
                    // Size the tmux pane + cache to the SAME output rect the
                    // renderer paints into, via the one `PreviewLayout::compute`
                    // that `render_with_cache` also uses. `layout.output` already
                    // accounts for the info header and the ` Output ` banner row
                    // (or claims the full `inner` when the header is hidden /
                    // compact), so `output.height` is the exact visible body. No
                    // second banner subtraction here, no parallel split to drift.
                    let pane_area = self
                        .selected_session
                        .as_ref()
                        .and_then(|id| self.get_instance(id))
                        .map(|inst| {
                            preview::PreviewLayout::compute(
                                inner,
                                compact,
                                self.show_preview_info,
                                preview::agent_info_height(inst),
                            )
                            .output
                        })
                        .unwrap_or(inner);
                    self.preview_pane_area = pane_area;
                    self.preview_visible_rows = pane_area.height as usize;
                    // Refresh the raw `content` cache, then ensure the
                    // parsed `Text<'static>` cache reflects it. Doing
                    // the parse here (under `&mut self.preview_cache`)
                    // means subsequent shared borrows on
                    // `parsed_text` and on `self.get_instance` can
                    // coexist in the actual render call.
                    let cap_start = Instant::now();
                    self.refresh_preview_cache_if_needed(pane_area.width, pane_area.height);
                    self.preview_timings.capture = cap_start.elapsed();
                    let parse_start = Instant::now();
                    self.preview_cache.ensure_parsed();
                    self.preview_timings.parse = parse_start.elapsed();
                    let total_lines = self
                        .preview_cache
                        .parsed_text
                        .as_ref()
                        .map_or(0, |t| t.lines.len());
                    self.set_preview_text_view(pane_area, total_lines);

                    if let Some(id) = &self.selected_session {
                        if let Some(inst) = self.get_instance(id) {
                            Preview::render_with_cache(
                                frame,
                                inner,
                                inst,
                                CachedPreview::from_text(self.preview_cache.parsed_text.as_ref()),
                                self.preview_scroll_offset,
                                theme,
                                self.idle_decay_window,
                                compact,
                                self.show_preview_info,
                            );
                        }
                    } else {
                        let hint = Paragraph::new("Select a session to preview")
                            .style(Style::default().fg(theme.dimmed))
                            .alignment(Alignment::Center);
                        frame.render_widget(hint, inner);
                    }
                }
            }
            ViewMode::Terminal => {
                // Clone id early to avoid borrow conflicts
                let selected_id = self.selected_session.clone();

                if let Some(id) = selected_id {
                    // Determine which terminal to preview based on mode
                    let terminal_mode = if let Some(inst) = self.get_instance(&id) {
                        if inst.is_sandboxed() {
                            self.get_terminal_mode(&id)
                        } else {
                            TerminalMode::Host
                        }
                    } else {
                        TerminalMode::Host
                    };

                    // Compute the output sub-rect symmetric with Agent
                    // view: when the info header is visible we strip the
                    // header rows + one banner row off `inner`, so the
                    // tmux pane resizes match what the user actually
                    // sees. Without this, live-send against a terminal
                    // pane sizes tmux to `inner.height` while only
                    // `inner.height - info_h - 1` rows are visible, and
                    // the top of the shell output gets clipped on every
                    // frame.
                    // Same single-source split as the Agent branch: the tmux
                    // pane is sized to `PreviewLayout::compute(..).output`, which
                    // `render_terminal_preview` also paints into.
                    let pane_area = self
                        .get_instance(&id)
                        .map(|inst| {
                            preview::PreviewLayout::compute(
                                inner,
                                compact,
                                self.show_preview_info,
                                preview::terminal_info_height(inst),
                            )
                            .output
                        })
                        .unwrap_or(inner);
                    self.preview_pane_area = pane_area;
                    self.preview_visible_rows = pane_area.height as usize;

                    // Refresh the appropriate cache, then warm the
                    // matching `parsed_text` so the render call below
                    // can read it via a shared borrow alongside
                    // `get_instance`.
                    match terminal_mode {
                        TerminalMode::Container => {
                            self.refresh_container_terminal_preview_cache_if_needed(
                                pane_area.width,
                                pane_area.height,
                            );
                            self.container_terminal_preview_cache.ensure_parsed();
                        }
                        TerminalMode::Host => {
                            self.refresh_terminal_preview_cache_if_needed(
                                pane_area.width,
                                pane_area.height,
                            );
                            self.terminal_preview_cache.ensure_parsed();
                        }
                    }
                    let total_lines = match terminal_mode {
                        TerminalMode::Container => &self.container_terminal_preview_cache,
                        TerminalMode::Host => &self.terminal_preview_cache,
                    }
                    .parsed_text
                    .as_ref()
                    .map_or(0, |t| t.lines.len());
                    self.set_preview_text_view(pane_area, total_lines);

                    // Now borrow instance for rendering
                    if let Some(inst) = self.get_instance(&id) {
                        let (terminal_running, preview_text) = match terminal_mode {
                            TerminalMode::Container => {
                                let running = inst
                                    .container_terminal_tmux_session()
                                    .map(|s| s.exists())
                                    .unwrap_or(false);
                                (
                                    running,
                                    self.container_terminal_preview_cache.parsed_text.as_ref(),
                                )
                            }
                            TerminalMode::Host => {
                                let running = inst
                                    .terminal_tmux_session()
                                    .map(|s| s.exists())
                                    .unwrap_or(false);
                                (running, self.terminal_preview_cache.parsed_text.as_ref())
                            }
                        };

                        Preview::render_terminal_preview(
                            frame,
                            inner,
                            inst,
                            terminal_running,
                            CachedPreview::from_text(preview_text),
                            self.preview_scroll_offset,
                            theme,
                            compact,
                            self.show_preview_info,
                        );
                    }
                } else {
                    let hint = Paragraph::new("Select a session to preview terminal")
                        .style(Style::default().fg(theme.dimmed))
                        .alignment(Alignment::Center);
                    frame.render_widget(hint, inner);
                }
            }
            ViewMode::Tool(ref tool_name) => {
                let tool_name = tool_name.clone();
                let selected_id = self.selected_session.clone();

                if let Some(id) = selected_id {
                    // Same single-source split as the Agent branch: the tmux
                    // pane is sized to `PreviewLayout::compute(..).output`, which
                    // `render_terminal_preview` also paints into.
                    let pane_area = self
                        .get_instance(&id)
                        .map(|inst| {
                            preview::PreviewLayout::compute(
                                inner,
                                compact,
                                self.show_preview_info,
                                preview::terminal_info_height(inst),
                            )
                            .output
                        })
                        .unwrap_or(inner);
                    self.preview_pane_area = pane_area;
                    self.preview_visible_rows = pane_area.height as usize;

                    self.refresh_tool_preview_cache_if_needed(
                        pane_area.width,
                        pane_area.height,
                        &tool_name,
                    );
                    self.tool_preview_cache.ensure_parsed();
                    let total_lines = self
                        .tool_preview_cache
                        .parsed_text
                        .as_ref()
                        .map_or(0, |t| t.lines.len());
                    self.set_preview_text_view(pane_area, total_lines);

                    if let Some(inst) = self.get_instance(&id) {
                        let tool_session =
                            crate::tmux::ToolSession::new(&inst.id, &inst.title, &tool_name);
                        let tool_running = tool_session.exists() && !tool_session.is_pane_dead();

                        Preview::render_terminal_preview(
                            frame,
                            inner,
                            inst,
                            tool_running,
                            CachedPreview::from_text(self.tool_preview_cache.parsed_text.as_ref()),
                            self.preview_scroll_offset,
                            theme,
                            compact,
                            self.show_preview_info,
                        );
                    }
                } else {
                    let hint = Paragraph::new("Select a session to preview tool")
                        .style(Style::default().fg(theme.dimmed))
                        .alignment(Alignment::Center);
                    frame.render_widget(hint, inner);
                }
            }
        }

        // In live-send mode, place a real terminal cursor over the preview at
        // the target pane's cursor cell. `capture-pane` carries only cell text
        // (plus SGR color), not the cursor, so without this the
        // "feels-attached" preview shows no cursor for programs that rely on
        // the hardware cursor (shells, codex, anything using DECTCEM) even
        // though a direct tmux attach would. Programs that paint their own
        // caret into the cells (e.g. Claude Code's reverse-video block) hide
        // the hardware cursor, so `cursor_flag` is 0 and this paints nothing
        // over them, avoiding a double cursor.
        if let Some(pos) = self.live_preview_cursor_pos() {
            frame.set_cursor_position(pos);
        }

        // Selection highlight goes last so it sits on top of whatever
        // the active ViewMode painted into the inner area. The handlers
        // only populate `preview_selection` while a drag is live or a
        // finalized highlight is showing, so this branch is a no-op
        // otherwise.
        self.paint_preview_selection(frame, theme);
    }

    /// Where to paint the live-send cursor this frame, or `None` to paint no
    /// cursor. Maps the agent pane's `(cursor_x, cursor_y)` (counted from the
    /// top of the visible screen) onto the preview's output rect.
    ///
    /// Only fires while live-send is active and the preview is at the live
    /// tail (`preview_scroll_offset == 0`): over scrolled-back history the
    /// live cursor would land on the wrong row. The capture worker now
    /// publishes a cursor for every previewed pane (the wheel forward reads its
    /// mode flags), so this gates on `live_send` to keep painting confined to
    /// the driven pane, and on `position_reliable` (false while the pane
    /// scrolled mid-capture) to avoid painting on a row the cursor no longer
    /// indexes.
    fn live_preview_cursor_pos(&self) -> Option<Position> {
        if self.live_send.is_none() || self.preview_scroll_offset != 0 {
            return None;
        }
        let cursor = self.preview_capture_worker.as_ref()?.current_cursor()?;
        if !cursor.position_reliable {
            return None;
        }
        map_live_preview_cursor(self.preview_pane_area, self.preview_visible_rows, cursor)
    }

    /// Apply the drag-select highlight to cells inside the preview
    /// pane. Style is reversed (bg/fg swap) for AA-friendly contrast
    /// against arbitrary agent output, mirroring how most terminal
    /// emulators render their own native selections.
    ///
    /// Walks the frame buffer rather than re-rendering, so the
    /// underlying preview pane keeps its existing styles (colored
    /// diff text, syntax highlighting from the agent) — only the
    /// bg/fg pair swaps. Cells outside the buffer area are skipped
    /// rather than treated as an error: a terminal resize during a
    /// drag can leave a stale extent off-screen for one frame.
    fn paint_preview_selection(&mut self, frame: &mut Frame, theme: &Theme) {
        let Some(sel) = self.preview_selection else {
            return;
        };
        let view = self.preview_text_view;
        let pane = view.pane;
        // Screen rects for the visible slice of the selection. A selection
        // that has scrolled partly (or wholly) off screen only paints the
        // rows still in view; the copy still spans the full range.
        let segments = sel.screen_flow_rects(view);
        // Capture the selected text only on the first render that follows
        // a finalized drag; subsequent renders just keep painting the
        // highlight. Unlike the old cell-from-buffer read, the copy now
        // comes from the parsed scrollback cache, so it includes lines
        // that scrolled out of view.
        let capture = self.preview_copy_pending;
        if capture {
            self.preview_copy_pending = false;
            self.preview_copy_text = self.extract_preview_selection_text();
        }
        if segments.is_empty() {
            return;
        }
        let buf = frame.buffer_mut();
        let buf_area = buf.area;
        // After release the highlight darkens slightly so the user
        // can tell "selection finalized + copied" apart from "still
        // dragging". A non-finalized in-progress drag uses the
        // brighter selection-style swatch.
        let bg = if sel.finalized {
            theme.selection
        } else {
            theme.session_selection
        };
        for segment in segments {
            let clipped = segment.intersection(pane);
            if clipped.width == 0 || clipped.height == 0 {
                continue;
            }
            for row in clipped.y..clipped.bottom() {
                for col in clipped.x..clipped.right() {
                    if !buf_area.contains(Position::from((col, row))) {
                        continue;
                    }
                    let cell = &mut buf[(col, row)];
                    cell.set_bg(bg);
                    // Force the foreground to a high-contrast color so
                    // ANSI-painted bright/dim agent output stays
                    // readable on top of the new background.
                    cell.set_fg(theme.text);
                }
            }
        }
    }

    fn render_creating_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let selected_id = match &self.selected_session {
            Some(id) => id.clone(),
            None => return,
        };

        let inst = match self.get_instance(&selected_id) {
            Some(inst) => inst,
            None => return,
        };

        let spinner = spinners::orbit()
            .set_interval(Duration::from_millis(400))
            .current_frame();

        // Info section (3 lines) + separator + hook output
        let info_height: u16 = 4;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(info_height), Constraint::Min(1)])
            .split(area);

        // Info lines
        let info_lines = vec![
            Line::from(vec![
                Span::styled("Title:   ", Style::default().fg(theme.dimmed)),
                Span::styled(&inst.title, Style::default().fg(theme.text).bold()),
            ]),
            Line::from(vec![
                Span::styled("Path:    ", Style::default().fg(theme.dimmed)),
                Span::styled(&inst.project_path, Style::default().fg(theme.text)),
            ]),
            Line::from(vec![
                Span::styled("Status:  ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    format!("{} Creating...", spinner),
                    Style::default().fg(theme.accent),
                ),
            ]),
            Line::from(""),
        ];
        frame.render_widget(Paragraph::new(info_lines), chunks[0]);

        // Hook output section
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme.border))
            .title(" Hook Output ")
            .title_style(Style::default().fg(theme.dimmed));

        let inner = block.inner(chunks[1]);
        frame.render_widget(block, chunks[1]);

        let progress = self.creating_hook_progress.get(&selected_id);
        let inner_height = inner.height as usize;

        if let Some(progress) = progress {
            let mut lines: Vec<Line> = Vec::new();

            // Current hook command
            if let Some(ref cmd) = progress.current_hook {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {} ", spinner),
                        Style::default().fg(theme.accent).bold(),
                    ),
                    Span::styled(cmd.as_str(), Style::default().fg(theme.text)),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    format!(" {} Preparing...", spinner),
                    Style::default().fg(theme.dimmed),
                )));
            }

            // Show the last N lines of output that fit
            let max_output = inner_height.saturating_sub(3);
            let start = progress.hook_output.len().saturating_sub(max_output);
            for line in &progress.hook_output[start..] {
                lines.push(Line::from(Span::styled(
                    format!("  {}", line),
                    Style::default().fg(theme.dimmed),
                )));
            }

            // Pad and add cancel hint
            let used = lines.len();
            let available = inner_height.saturating_sub(1);
            for _ in used..available {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(vec![
                Span::styled(" Press ", Style::default().fg(theme.dimmed)),
                Span::styled("Ctrl+C", Style::default().fg(theme.hint)),
                Span::styled(" to cancel", Style::default().fg(theme.dimmed)),
            ]));

            frame.render_widget(Paragraph::new(lines), inner);
        } else {
            let hint = Paragraph::new(format!(" {} Setting up session...", spinner))
                .style(Style::default().fg(theme.dimmed));
            frame.render_widget(hint, inner);
        }
    }

    /// Calm placeholder shown in the preview pane when the selected session is
    /// archived. Archiving kills the pane, so the normal capture path would
    /// render an empty body ("No output available"); this explains the state
    /// instead and points at `z` to bring the row back to the active list.
    fn render_archived_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
            .map(|inst| inst.title.clone())
            .unwrap_or_default();
        let key = if self.strict_hotkeys { "Z" } else { "z" };
        let parked = if title.is_empty() {
            "This session is parked. Its agent was stopped.".to_string()
        } else {
            format!("\"{}\" is parked. Its agent was stopped.", title)
        };
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Archived",
                Style::default().fg(theme.text).bold(),
            )),
            Line::from(""),
            Line::from(Span::styled(parked, Style::default().fg(theme.dimmed))),
            Line::from(""),
            Line::from(vec![
                Span::styled("Press ", Style::default().fg(theme.dimmed)),
                Span::styled(key, Style::default().fg(theme.hint).bold()),
                Span::styled(" to unarchive it.", Style::default().fg(theme.dimmed)),
            ]),
        ];
        let para = Paragraph::new(lines).alignment(Alignment::Center);
        frame.render_widget(para, area);
    }

    /// Calm placeholder shown when the selected session is in the trash. Its
    /// agent was stopped on trash but its transcript and workspace are kept;
    /// it can be restored or permanently purged from here.
    fn render_trashed_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
            .map(|inst| inst.title.clone())
            .unwrap_or_default();
        let body = if title.is_empty() {
            "This session is in the trash. Its agent was stopped; its transcript and workspace are kept.".to_string()
        } else {
            format!(
                "\"{}\" is in the trash. Its agent was stopped; its transcript and workspace are kept.",
                title
            )
        };
        let restore_key = if self.strict_hotkeys { "Z" } else { "z" };
        // The permanent-delete keybind is blocked in Terminal view (it routes
        // to a "Cannot delete terminal" dialog), so only advertise it in
        // Structured view. Restore works in either. See #2489.
        let hint = if self.view_mode == ViewMode::Terminal {
            Line::from(vec![
                Span::styled("Press ", Style::default().fg(theme.dimmed)),
                Span::styled(restore_key, Style::default().fg(theme.hint).bold()),
                Span::styled(" to restore.", Style::default().fg(theme.dimmed)),
            ])
        } else {
            Line::from(vec![
                Span::styled("Press ", Style::default().fg(theme.dimmed)),
                Span::styled(restore_key, Style::default().fg(theme.hint).bold()),
                Span::styled(" to restore, or ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    if self.strict_hotkeys { "D" } else { "d" },
                    Style::default().fg(theme.hint).bold(),
                ),
                Span::styled(" to delete permanently.", Style::default().fg(theme.dimmed)),
            ])
        };
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Trash",
                Style::default().fg(theme.text).bold(),
            )),
            Line::from(""),
            Line::from(Span::styled(body, Style::default().fg(theme.dimmed))),
            Line::from(""),
            hint,
        ];
        let para = Paragraph::new(lines).alignment(Alignment::Center);
        frame.render_widget(para, area);
    }

    /// Calm placeholder shown when the selected session's pane is simply gone
    /// (the generic gone-error, no diagnostic detail). Replaces the red crash
    /// error with a "Stopped, enter to start" message; the row's real status
    /// icon still signals the state in the sidebar.
    fn render_stopped_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Stopped",
                Style::default().fg(theme.text).bold(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "This session isn't running.",
                Style::default().fg(theme.dimmed),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("Press ", Style::default().fg(theme.dimmed)),
                Span::styled("Enter", Style::default().fg(theme.hint).bold()),
                Span::styled(" to start it.", Style::default().fg(theme.dimmed)),
            ]),
        ];
        let para = Paragraph::new(lines).alignment(Alignment::Center);
        frame.render_widget(para, area);
    }

    fn render_status_bar(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Cleared each frame and only set when the badge is actually drawn, so
        // a stale rect can't make a footer click open tips when the badge is
        // hidden (live-send, nothing unseen, or no room).
        self.tips_badge_rect = None;
        // Live-send banner takes over the status bar so the user has an
        // always-visible reminder that keystrokes are being relayed to
        // the pane (and how to get out). Distinct color + bold so it
        // can't be confused with the regular footer. The scroll
        // indicator (only present when the user has scrolled back from
        // the live edge) sits between the title and the exit chord
        // hint so it gets noticed when there's something to notice.
        if let Some(state) = &self.live_send {
            let base_title = if state.title.is_empty() {
                "session"
            } else {
                state.title.as_str()
            };
            // Surface which pane keystrokes are landing on; the shared
            // formatter keeps this label in lockstep with the compose
            // dialog's title.
            let raw_title = live_send::format_target_label(base_title, state.target);
            let chip = " \u{25CF} LIVE \u{2192} ";
            let chip_style = Style::default()
                .fg(theme.background)
                .bg(theme.running)
                .bold();

            // Which-key menu: the leader is armed, so surface the live-send
            // commands the next key can pick instead of the normal exit
            // hint. This is the discoverability moment the issue asked for;
            // pressing the leader shows exactly what it does.
            if self.live_send_pending_leader {
                if let Some(leader) = state.leader {
                    let lead = live_send::display_chord(leader);
                    let sidebar_cmd = if self.sidebar_collapsed {
                        "b show sidebar"
                    } else {
                        "b hide sidebar"
                    };
                    let menu =
                        format!("  {lead}:  k palette \u{00b7} {sidebar_cmd} \u{00b7} q exit ");
                    let menu_budget = (area.width as usize)
                        .saturating_sub(unicode_width::UnicodeWidthStr::width(chip));
                    let menu = truncate_to_width(&menu, menu_budget);
                    let spans = vec![
                        Span::styled(chip, chip_style),
                        Span::styled(menu, Style::default().fg(theme.accent).bold()),
                    ];
                    frame.render_widget(Paragraph::new(Line::from(spans)), area);
                    return;
                }
            }

            // The chord display is built from the user's configured
            // exit-chord list so the hint always shows what actually
            // exits live mode for this user. Empty list (impossible
            // under normal config — parse_chord_list falls back to
            // the default set) renders as "?" so the user notices
            // something's wrong rather than thinking the mode is
            // unescapable.
            let chord = if state.exit_chords.is_empty() {
                "?".to_string()
            } else {
                live_send::display_chord_list(&state.exit_chords)
            };
            let suffix = " to exit ";
            // Compact reminder that the leader opens the command menu, so
            // the user can discover the palette / sidebar toggle without
            // having entered the menu yet. Empty when the leader is
            // disabled (the user cleared the setting).
            let leader_hint = state
                .leader
                .map(|l| format!(" \u{00b7} {} menu", live_send::display_chord(l)))
                .unwrap_or_default();
            // `preview_visible_rows` is the output-body height the renderer
            // last painted into (pane height minus the inner banner row only
            // when that banner is shown). Reuse it so the live `[offset/max]`
            // indicator agrees with the actual scroll math; deriving it from
            // `dimensions` with a fixed `- 1` would over-count the max by a
            // row whenever the info header is hidden.
            let visible_height = self.preview_visible_rows;
            // Pull `captured_lines` from whichever cache is on screen, not the
            // Agent cache unconditionally: in Terminal/Tool live mode the
            // wrong cache would show a stale or empty `[offset/max]`.
            let scroll = format_scroll_indicator(
                self.active_captured_lines(),
                visible_height,
                self.preview_scroll_offset,
            )
            .unwrap_or_default();
            // Spaces between chip→title and title→chord. Title gets the
            // budget after the fixed pieces; reserved last so the exit
            // chord never falls off on narrow terminals.
            let fixed_width = unicode_width::UnicodeWidthStr::width(chip)
                + 1 // single space after the chip
                + 2 // double space before the chord
                + unicode_width::UnicodeWidthStr::width(chord.as_str())
                + unicode_width::UnicodeWidthStr::width(suffix)
                + unicode_width::UnicodeWidthStr::width(leader_hint.as_str())
                + unicode_width::UnicodeWidthStr::width(scroll.as_str());
            let title_budget = (area.width as usize).saturating_sub(fixed_width);
            let title = truncate_to_width(&raw_title, title_budget);
            let mut spans: Vec<Span<'static>> = vec![
                Span::styled(chip, chip_style),
                Span::raw(" "),
                Span::styled(title, Style::default().fg(theme.text).bold()),
            ];
            if !scroll.is_empty() {
                spans.push(Span::styled(
                    scroll,
                    Style::default().fg(theme.dimmed).italic(),
                ));
            }
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                chord,
                Style::default().fg(theme.accent).bold(),
            ));
            spans.push(Span::styled(suffix, Style::default().fg(theme.dimmed)));
            if !leader_hint.is_empty() {
                spans.push(Span::styled(
                    leader_hint,
                    Style::default().fg(theme.dimmed).italic(),
                ));
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }

        let key_style = Style::default().fg(theme.accent).bold();
        let desc_style = Style::default().fg(theme.dimmed);
        let sep_style = Style::default().fg(theme.border);
        let strict = self.strict_hotkeys;

        // Priority-tagged shortcut groups. Lower priority = kept longer when
        // the footer can't fit everything (iPhone Mosh landscape is ~80 cols,
        // where the full label set used to truncate Help/Quit). Essentials
        // (Nav / Enter / Help / Quit / Serve indicator) survive first;
        // Diff / Search / Mode / Group drop first. Groups render in the
        // declared order; a · separator is inserted between kept groups
        // at render time.
        let mk = |key: &str, desc: &str| -> Vec<Span<'static>> {
            vec![
                Span::styled(format!("{} ", key), key_style),
                Span::styled(desc.to_string(), desc_style),
            ]
        };
        // Key-only entry for keys universal enough that a description would be
        // noise (? for help, / for search). Saves footer width at iPhone-Mosh
        // sizes.
        let mk_key =
            |key: &str| -> Vec<Span<'static>> { vec![Span::styled(key.to_string(), key_style)] };

        // Key a footer button synthesizes on click. The registry matches on
        // the bare keycode (Shift implied by an uppercase char, Ctrl by the
        // flag), so a plain char and a Ctrl char cover every footer chord.
        let kc = |c: char| Some(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let kctrl = |c: char| Some(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        let kenter = Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // (priority, click-key, spans). `click-key` is `None` for the
        // non-actionable status indicators (Serve / watching), which render
        // but aren't clickable.
        let mut groups: Vec<(u8, Option<KeyEvent>, Vec<Span<'static>>)> = Vec::new();

        // Serve indicator: shown only when the `aoe serve` daemon is live.
        // The TUI does not own the daemon, so we probe the PID file each
        // render. Mode comes from a PID-keyed cache so we don't read the
        // serve.mode file from disk on every frame.
        #[cfg(feature = "serve")]
        {
            let mode_label = crate::cli::serve::cached_serve_mode_label();
            if crate::cli::serve::daemon_pid().is_some() {
                let label = match mode_label {
                    Some(m) => format!(" \u{25CF} Serving ({}) ", m),
                    None => " \u{25CF} Serving ".to_string(),
                };
                groups.push((
                    0,
                    None,
                    vec![Span::styled(
                        label,
                        Style::default().fg(theme.running).bold(),
                    )],
                ));
            }
        }

        // Other-TUI indicator: shown only when more than one `aoe` TUI is
        // alive. Two TUIs watching the same agent sessions clash over pane
        // sizes (tmux reflows to the smallest attached client), so surface the
        // count as a heads-up. The value is recomputed on a throttle in the
        // app loop, not per frame.
        if self.active_tui_count > 1 {
            groups.push((
                0,
                None,
                vec![Span::styled(
                    format!(" \u{25C9} {} watching ", self.active_tui_count),
                    Style::default().fg(theme.accent).bold(),
                )],
            ));
        }

        // Pending-paste indicator: text was captured at the home view but
        // couldn't be routed yet (no runnable session selected). Surface a
        // high-priority hint so the user knows the paste/dictation didn't
        // vanish — pressing `m` after selecting a runnable session drains
        // pending_paste into the compose dialog.
        if let Some(buf) = &self.pending_paste {
            if !buf.is_empty() {
                let key = if strict { "M" } else { "m" };
                let desc = format!("send {} buffered", buf.chars().count());
                let mut spans = mk(key, &desc);
                spans[1] = Span::styled(desc, Style::default().fg(theme.running).bold());
                groups.push((0, kc(if strict { 'M' } else { 'm' }), spans));
            }
        }

        if let Some(enter_action_text) = match self.flat_items.get(self.cursor) {
            Some(Item::Group {
                collapsed: true, ..
            }) => Some("Expand"),
            Some(Item::Group {
                collapsed: false, ..
            }) => Some("Collapse"),
            Some(Item::Session { .. }) => Some("Attach"),
            None => None,
        } {
            // U+21B5 (↵) renders Enter/Return in one cell across most fonts;
            // saves 4 cols vs the literal word and matches k9s/lazygit/fzf
            // conventions. Trailing space inside the key string adds a second
            // visual gap before the description — at most fonts the arrow
            // glyph fills its cell tightly and a single mk-internal space
            // looks too close to the desc.
            groups.push((0, kenter, mk("↵ ", enter_action_text)));
        }

        groups.push((
            2,
            kc(if strict { 'T' } else { 't' }),
            mk(if strict { "T" } else { "t" }, "View"),
        ));
        if matches!(self.view_mode, ViewMode::Tool(_)) {
            groups.push((1, kc(';'), mk(";", "Back")));
        } else if !self.tool_configs.is_empty() {
            groups.push((2, kc(';'), mk(";", "Tools")));
        }
        groups.push((
            3,
            if strict { kctrl('g') } else { kc('g') },
            mk(if strict { "^G" } else { "g" }, "Group"),
        ));

        // c: container/host toggle hint for sandboxed sessions in Terminal view
        if self.view_mode == ViewMode::Terminal {
            if let Some(id) = &self.selected_session {
                if let Some(inst) = self.get_instance(id) {
                    if inst.is_sandboxed() {
                        groups.push((
                            4,
                            kc(if strict { 'C' } else { 'c' }),
                            mk(if strict { "C" } else { "c" }, "Mode"),
                        ));
                    }
                }
            }
        }

        groups.push((
            2,
            kc(if strict { 'N' } else { 'n' }),
            mk(if strict { "N" } else { "n" }, "New"),
        ));

        // Priority 1: user's core daily workflow (message / del).
        // These survive the greedy pack under narrow-pane widths (iPad
        // Termius / Moshi ~80 cols) because they're the actions the user
        // reaches for most often. Del stays at p3, less frequent,
        // OK to drop first.
        if self.selected_session.is_some() {
            groups.push((
                1,
                kc(if strict { 'M' } else { 'm' }),
                mk(if strict { "M" } else { "m" }, "Msg"),
            ));
        }
        if !self.flat_items.is_empty() {
            groups.push((
                3,
                kc(if strict { 'D' } else { 'd' }),
                mk(if strict { "D" } else { "d" }, "Del"),
            ));
        }
        // Attention-workflow shortcuts (Archive / Fav / Snooze) only render
        // when the user is in Attention sort. They are only useful for
        // shaping the Attention queue; in Newest / Created / Last Accessed
        // they just take footer space without changing what the user sees.
        if self.sort_order == SortOrder::Attention {
            if !self.flat_items.is_empty() {
                groups.push((
                    1,
                    kc(if strict { 'Z' } else { 'z' }),
                    mk(if strict { "Z" } else { "z" }, "Archive"),
                ));
            }
            if self.selected_session.is_some() {
                groups.push((
                    1,
                    kc(if strict { 'F' } else { 'f' }),
                    mk(if strict { "F" } else { "f" }, "Fav"),
                ));
                groups.push((
                    1,
                    kc(if strict { 'H' } else { 'h' }),
                    mk(if strict { "H" } else { "h" }, "Snooze"),
                ));
            }
        }

        groups.push((4, kc('/'), mk_key("/")));
        groups.push((
            4,
            if strict { kctrl('d') } else { kc('D') },
            mk(if strict { "^D" } else { "D" }, "Diff"),
        ));
        groups.push((1, kctrl('k'), mk("^K", "Cmds")));
        groups.push((0, kc('?'), mk_key("?")));

        // Greedy pack by priority. Width of a group = sum of span char counts;
        // separator between kept groups adds 3 cols each (" · "). Reserve 1
        // col for the leading space margin.
        // Display-cell width (not `chars().count()`) so the greedy pack and
        // the click rects line up with the cells ratatui actually paints.
        let widths: Vec<usize> = groups
            .iter()
            .map(|(_, _, g)| g.iter().map(|s| s.width()).sum::<usize>())
            .collect();

        // Tips badge: pinned to the bottom-right of the footer and clickable. It
        // takes priority over the keybind hints, the greedy pack below reserves
        // its width first, so on a thin terminal the hints drop rather than
        // collide with it. Hidden when nothing is unseen / tips disabled
        // (`tips_unseen` is zero then) or when even the badge can't fit.
        // Hover highlight mirrors a session row, but the footer's own bg is
        // already `theme.selection`, so hover uses the brighter
        // `session_selection` to actually stand out.
        let badge_bg = if self.tips_badge_hovered {
            theme.session_selection
        } else {
            theme.selection
        };
        let badge_line = (self.tips_unseen > 0).then(|| {
            Line::from(Span::styled(
                format!(" \u{1f4a1} {} tips ", self.tips_unseen),
                Style::default().fg(theme.accent).bold().bg(badge_bg),
            ))
        });
        let badge_width = badge_line.as_ref().map(|l| l.width()).unwrap_or(0);
        let badge_fits = badge_width > 0 && badge_width <= area.width as usize;
        let badge_reserve = if badge_fits { badge_width + 1 } else { 0 };

        let avail = (area.width as usize)
            .saturating_sub(1)
            .saturating_sub(badge_reserve);

        let mut order: Vec<usize> = (0..groups.len()).collect();
        order.sort_by_key(|&i| groups[i].0);

        let mut keep = vec![false; groups.len()];
        let mut used = 0usize;
        let mut count = 0usize;
        for i in order {
            let sep = if count == 0 { 0 } else { 3 };
            if used + widths[i] + sep <= avail {
                keep[i] = true;
                used += widths[i] + sep;
                count += 1;
            }
        }

        // Inverted-chip highlight for the button under the pointer, matching
        // the LIVE chip's fg/bg swap so hover reads as "this is clickable".
        let hover_style = Style::default()
            .fg(theme.background)
            .bg(theme.accent)
            .bold();

        let mut spans: Vec<Span> = vec![Span::raw(" ")];
        let mut first = true;
        // Column of the next span; starts past the leading space margin. Used
        // to record each clickable button's hit rect as it's laid out.
        let mut col = area.x.saturating_add(1);
        for (i, (_, key, group)) in groups.into_iter().enumerate() {
            if !keep[i] {
                continue;
            }
            if !first {
                spans.push(Span::styled(" · ", sep_style));
                col = col.saturating_add(3);
            }
            let width = widths[i] as u16;
            match key {
                Some(key) => {
                    self.footer_buttons.push((
                        Rect {
                            x: col,
                            y: area.y,
                            width,
                            height: area.height,
                        },
                        key,
                    ));
                    if self.footer_hover == Some(key) {
                        for s in group {
                            spans.push(Span::styled(s.content, hover_style));
                        }
                    } else {
                        spans.extend(group);
                    }
                }
                None => spans.extend(group),
            }
            col = col.saturating_add(width);
            first = false;
        }

        let status = Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.selection));
        frame.render_widget(status, area);

        // Draw the badge over the reserved right edge and remember its rect so
        // a click there opens the tips overlay.
        if badge_fits {
            if let Some(line) = badge_line {
                let bw = badge_width as u16;
                let rect = Rect {
                    x: area.x + area.width.saturating_sub(bw),
                    y: area.y,
                    width: bw,
                    height: 1,
                };
                frame.render_widget(
                    Paragraph::new(line).style(Style::default().bg(badge_bg)),
                    rect,
                );
                self.tips_badge_rect = Some(rect);
            }
        }
    }

    fn render_update_bar(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        info: Option<&UpdateInfo>,
        status: Option<&str>,
        image_update: Option<&ImageUpdate>,
    ) {
        let update_style = Style::default().fg(theme.waiting).bold();
        // The Update key is `u` (`Ctrl+u` in strict mode); pull the label from
        // the binding registry so this hint can't drift from the dispatcher.
        let update_key =
            super::bindings::label(super::bindings::ActionId::Update, self.strict_hotkeys);
        // Precedence (highest first): transient status, app update, then the
        // sandbox-image update. Only one banner shows at a time, so its keys
        // ([u]/[Ctrl+x]) are unambiguous; a lower-priority banner surfaces once
        // the ones above it clear.
        let text = if let Some(s) = status {
            format!(" {s}  [Ctrl+x] dismiss")
        } else if let Some(info) = info {
            // Reassure users (issue #2220) that updating is safe: it never
            // tears down or interrupts running sessions. Kept after the keys so
            // the action hints stay visible first on narrow terminals.
            format!(
                " update available {} → {}  [{update_key}] update  [Ctrl+x] dismiss  ·  running sessions stay safe",
                info.current_version, info.latest_version
            )
        } else if image_update.is_some() {
            format!(" sandbox image update available  [{update_key}] pull  [Ctrl+x] dismiss")
        } else {
            return;
        };
        let bar = Paragraph::new(Line::from(Span::styled(text, update_style)))
            .style(Style::default().bg(theme.selection));
        frame.render_widget(bar, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The preview split geometry (header / banner / output rows) is now owned
    // by `preview::PreviewLayout`; its tests live alongside it in
    // `components/preview.rs`. The render-side regression is covered end to end
    // by `preview_visible_rows_equal_output_area_with_info_shown` in
    // `home/tests.rs`, which renders a real frame and asserts
    // `preview_visible_rows == preview_pane_area.height`.

    fn pane_cursor(x: u16, y: u16, visible: bool, pane_height: u16) -> crate::tmux::PaneCursor {
        crate::tmux::PaneCursor {
            x,
            y,
            visible,
            pane_height,
            history_size: 0,
            pane_width: 0,
            alternate_on: false,
            mouse_tracking: false,
            mouse_sgr: false,
            position_reliable: true,
        }
    }

    #[test]
    fn live_cursor_maps_directly_when_pane_matches_output() {
        // Pane sized to the output area (the steady-state live-send case): the
        // delta is zero, so cursor (x, y) maps onto output.{x,y} + (x, y).
        let output = Rect::new(40, 5, 80, 24);
        let pos = map_live_preview_cursor(output, 24, pane_cursor(3, 2, true, 24));
        assert_eq!(pos, Some(Position::new(43, 7)));
    }

    #[test]
    fn live_cursor_anchored_to_bottom_when_pane_taller_than_output() {
        // Pane is 24 rows but only 10 are visible (top clipped). The bottom 10
        // pin to the output, so a cursor on the last screen row (y=23) lands on
        // the output's last row; a cursor in the clipped top maps out and drops.
        let output = Rect::new(0, 0, 80, 10);
        assert_eq!(
            map_live_preview_cursor(output, 10, pane_cursor(0, 23, true, 24)),
            Some(Position::new(0, 9)),
        );
        assert_eq!(
            map_live_preview_cursor(output, 10, pane_cursor(0, 5, true, 24)),
            None,
        );
    }

    #[test]
    fn live_cursor_hidden_or_out_of_bounds_paints_nothing() {
        let output = Rect::new(0, 0, 80, 24);
        // DECTCEM-hidden cursor: nothing to paint.
        assert_eq!(
            map_live_preview_cursor(output, 24, pane_cursor(3, 2, false, 24)),
            None,
        );
        // Column past the output width is dropped rather than clamped.
        assert_eq!(
            map_live_preview_cursor(output, 24, pane_cursor(80, 2, true, 24)),
            None,
        );
    }

    #[test]
    fn truncate_to_width_passthrough_when_fits() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello", 5), "hello");
    }

    #[test]
    fn truncate_to_width_appends_ellipsis_when_overflow() {
        // 5-char budget, 7-char input → 4 chars + ellipsis.
        assert_eq!(truncate_to_width("abcdefg", 5), "abcd\u{2026}");
    }

    #[test]
    fn truncate_to_width_zero_returns_empty() {
        // Zero budget: title is sacrificed entirely so the fixed exit-
        // chord text has space to render on very narrow terminals.
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    #[test]
    fn truncate_to_width_respects_wide_chars() {
        // East Asian wide char is 2 cells. Budget 3 should fit one wide
        // char + ellipsis (2 + 1 = 3) — but we reserve 1 for ellipsis
        // so budget for content is 2, fitting exactly one wide char.
        assert_eq!(truncate_to_width("你好世界", 3), "你\u{2026}");
    }

    #[test]
    fn selected_row_style_preserves_readable_status_color() {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let style = Style::default().fg(theme.running);

        assert_eq!(selected_row_style(style, &theme).fg, Some(theme.running));
    }

    #[test]
    fn selected_row_style_sets_text_for_default_foreground() {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let style = Style::default();

        assert_eq!(selected_row_style(style, &theme).fg, Some(theme.text));
    }

    #[test]
    fn selected_row_style_falls_back_when_color_clashes() {
        let mut theme = crate::tui::styles::load_theme_with_mode("empire", false);
        theme.dimmed = theme.session_selection;
        let style = Style::default().fg(theme.dimmed);

        assert_eq!(selected_row_style(style, &theme).fg, Some(theme.text));
    }

    #[test]
    fn compose_list_title_omits_profile_and_suffix_at_defaults() {
        // Default group/sort and no profile filter: title is just the prefix,
        // no `[all]` tag, no parenthesized suffix.
        let title = compose_list_title("aoe", None, GroupByMode::Manual, SortOrder::Newest);
        assert_eq!(title, " aoe ");
    }

    #[test]
    fn compose_list_title_includes_profile_when_filter_active() {
        let title = compose_list_title(
            "aoe",
            Some("my-profile"),
            GroupByMode::Manual,
            SortOrder::Newest,
        );
        assert_eq!(title, " aoe [my-profile] ");
    }

    #[test]
    fn compose_list_title_shows_by_project_only() {
        let title = compose_list_title("aoe", None, GroupByMode::Project, SortOrder::Newest);
        assert_eq!(title, " aoe · project ");
    }

    #[test]
    fn compose_list_title_shows_sort_only_when_non_default() {
        let title = compose_list_title("aoe", None, GroupByMode::Manual, SortOrder::LastActivity);
        assert_eq!(title, " aoe · Recent ");
    }

    #[test]
    fn compose_list_title_merges_group_and_sort_suffixes() {
        let title = compose_list_title(
            "aoe",
            Some("alpha"),
            GroupByMode::Project,
            SortOrder::LastActivity,
        );
        assert_eq!(title, " aoe [alpha] · project · Recent ");
    }

    #[test]
    fn compose_list_title_default_sort_drops_suffix_segment() {
        // Newest is the default; it must not appear in the title even when
        // group mode contributes its own suffix piece.
        let title = compose_list_title("aoe", None, GroupByMode::Project, SortOrder::Newest);
        assert_eq!(title, " aoe · project ");
    }

    #[test]
    fn compose_list_title_supports_tool_prefix() {
        let title = compose_list_title("Tool: foo", None, GroupByMode::Manual, SortOrder::AZ);
        assert_eq!(title, " Tool: foo · A-Z ");
    }

    #[test]
    fn compose_list_title_supports_terminal_prefix() {
        // Terminal view mode uses the "Terminals" prefix; verify it flows
        // through the helper just like the Agent and Tool prefixes do.
        let title = compose_list_title(
            "Terminals",
            Some("work"),
            GroupByMode::Project,
            SortOrder::Newest,
        );
        assert_eq!(title, " Terminals [work] · project ");
    }

    #[test]
    fn compose_list_title_default_sort_with_project_and_profile() {
        // Matrix cell: default sort + project group + active profile.
        let title = compose_list_title(
            "aoe",
            Some("alpha"),
            GroupByMode::Project,
            SortOrder::Newest,
        );
        assert_eq!(title, " aoe [alpha] · project ");
    }

    #[test]
    fn compose_list_title_non_default_sort_with_profile_only() {
        // Matrix cell: non-default sort + manual group + active profile.
        let title = compose_list_title(
            "aoe",
            Some("alpha"),
            GroupByMode::Manual,
            SortOrder::LastActivity,
        );
        assert_eq!(title, " aoe [alpha] · Recent ");
    }

    #[test]
    fn compose_list_title_non_default_sort_with_project_no_profile() {
        // Matrix cell: non-default sort + project group + no profile.
        let title = compose_list_title("aoe", None, GroupByMode::Project, SortOrder::LastActivity);
        assert_eq!(title, " aoe · project · Recent ");
    }

    #[test]
    fn compose_list_title_renders_oldest_sort_label() {
        let title = compose_list_title("aoe", None, GroupByMode::Manual, SortOrder::Oldest);
        assert_eq!(title, " aoe · Oldest ");
    }

    #[test]
    fn compose_list_title_renders_za_sort_label() {
        let title = compose_list_title("aoe", None, GroupByMode::Manual, SortOrder::ZA);
        assert_eq!(title, " aoe · Z-A ");
    }

    #[test]
    fn profile_short_code_multi_segment_takes_initials() {
        assert_eq!(profile_short_code("forit-backup"), "fb");
        assert_eq!(profile_short_code("pivot-main"), "pm");
        assert_eq!(profile_short_code("wma-work"), "ww");
    }

    #[test]
    fn profile_short_code_single_segment_takes_first_three() {
        assert_eq!(profile_short_code("default"), "def");
        assert_eq!(profile_short_code("ForIT"), "for");
    }

    #[test]
    fn profile_short_code_caps_at_four_chars() {
        assert_eq!(profile_short_code("a-b-c-d-e-f"), "abcd");
    }

    #[test]
    fn profile_short_code_lowercases_and_ignores_empty_segments() {
        assert_eq!(profile_short_code("Forit_Backup"), "fb");
        assert_eq!(profile_short_code("--foo--"), "foo");
        assert_eq!(profile_short_code(""), "");
    }

    #[test]
    fn format_relative_age_none_returns_empty() {
        assert_eq!(format_relative_age(None), "");
    }

    #[test]
    fn format_relative_age_future_timestamp_returns_less_than_1m() {
        let future = Utc::now() + chrono::Duration::hours(1);
        assert_eq!(format_relative_age(Some(future)), "<1m");
    }

    #[test]
    fn format_relative_age_recent_returns_less_than_1m() {
        let recent = Utc::now() - chrono::Duration::seconds(30);
        assert_eq!(format_relative_age(Some(recent)), "<1m");
    }

    #[test]
    fn format_relative_age_minutes() {
        let ts = Utc::now() - chrono::Duration::minutes(5);
        assert_eq!(format_relative_age(Some(ts)), "5m");
    }

    #[test]
    fn format_relative_age_hours() {
        let ts = Utc::now() - chrono::Duration::hours(3);
        assert_eq!(format_relative_age(Some(ts)), "3h");
    }

    #[test]
    fn format_relative_age_days() {
        let ts = Utc::now() - chrono::Duration::days(7);
        assert_eq!(format_relative_age(Some(ts)), "7d");
    }

    #[test]
    fn format_relative_age_months() {
        let ts = Utc::now() - chrono::Duration::days(60);
        assert_eq!(format_relative_age(Some(ts)), "2mo");
    }

    #[test]
    fn capture_lines_for_adds_buffer_to_height() {
        assert_eq!(capture_lines_for(30, 0), 50);
    }

    #[test]
    fn clamp_scroll_to_capture_uses_visible_height_verbatim() {
        // Content exactly fills a 40-row banner-less pane: visible_height == 40,
        // so there is nothing to scroll back to and any offset clamps to 0.
        // The pre-fix code derived `area_height - 1` internally, which left a
        // phantom max offset of 1 and stalled live-follow a row early.
        assert_eq!(clamp_scroll_to_capture(1, 40, 40), 0);
        assert_eq!(clamp_scroll_to_capture(5, 40, 40), 0);
    }

    #[test]
    fn clamp_scroll_to_capture_allows_real_scrollback() {
        // 60 captured lines into a 40-row view leaves 20 rows of real history;
        // offsets within that range pass through, larger ones clamp to the max.
        assert_eq!(clamp_scroll_to_capture(10, 60, 40), 10);
        assert_eq!(clamp_scroll_to_capture(50, 60, 40), 20);
    }

    #[test]
    fn capture_lines_for_extends_by_scroll_offset() {
        assert_eq!(capture_lines_for(30, 200), 250);
    }

    #[test]
    fn capture_lines_for_saturates_instead_of_overflowing() {
        assert_eq!(capture_lines_for(u16::MAX, u16::MAX), u16::MAX as usize);
    }

    #[test]
    fn scroll_exceeds_cache_false_when_buffer_covers_small_scroll() {
        // Cache was captured at scroll=0 with height=30, so
        // capture_lines_for(30, 0) = 30 + 0 + BUFFER(20) = 50 lines.
        // A wheel tick to scroll_offset=3 needs 30 + 3 + 20 = 53, but the
        // existing BUFFER reserve is what we check: the predicate should
        // only trip when `height + scroll + BUFFER > captured_lines`.
        //
        // With captured_lines = 60 (capture returned extra pane history),
        // small scroll increments must NOT force a re-capture.
        let height = 30u16;
        let captured = 60usize;
        assert!(!scroll_exceeds_cache(captured, height, 0));
        assert!(!scroll_exceeds_cache(captured, height, 3));
        assert!(!scroll_exceeds_cache(captured, height, 9));
    }

    #[test]
    fn scroll_exceeds_cache_true_when_scroll_runs_past_captured_window() {
        // Once the requested visible window + BUFFER exceeds captured_lines,
        // the cache can no longer cover the scroll and must be re-captured.
        let height = 30u16;
        let captured = 60usize;
        // height(30) + scroll(20) + BUFFER(20) = 70 > 60 → recapture.
        assert!(scroll_exceeds_cache(captured, height, 20));
    }

    #[test]
    fn scroll_exceeds_cache_true_for_empty_cache() {
        // First render: nothing captured yet, so any request forces capture.
        assert!(scroll_exceeds_cache(0, 30, 0));
    }

    // -- activity_column_padding -------------------------------------------
    //
    // The column lives at `list_width - badge_width - SLOT - MARGIN`; the
    // returned pad_len is what goes between the row prefix and the column
    // to right-align it. None means the row is too wide and the column
    // should be hidden so the title doesn't get clipped.

    #[test]
    fn activity_column_padding_short_title_with_room_to_spare() {
        // 35-col pane, 12-col prefix, no badge: trailing reserves 6 (slot)
        // + 0 (badge) + 1 (margin) = 7, total = 19, pad_len = 35 - 19 = 16.
        assert_eq!(activity_column_padding(12, 35, 0), Some(16));
    }

    #[test]
    fn activity_column_padding_exact_fit_yields_zero_pad() {
        // Prefix ends right where the trailing block begins.
        // list_width(20) - prefix(13) - trailing(7) = 0.
        assert_eq!(activity_column_padding(13, 20, 0), Some(0));
    }

    #[test]
    fn activity_column_padding_one_short_hides_column() {
        // One column over budget: prefix(14) + trailing(7) = 21 > 20.
        assert_eq!(activity_column_padding(14, 20, 0), None);
    }

    #[test]
    fn activity_column_padding_accounts_for_terminal_mode_badge() {
        // " [host]" is 7 chars. trailing = SLOT(6) + 7 + MARGIN(1) = 14.
        // 35 - 14 - prefix(10) = 11.
        assert_eq!(activity_column_padding(10, 35, 7), Some(11));
        // " [container]" is 12 chars. trailing = 6 + 12 + 1 = 19.
        // 35 - 19 - 10 = 6.
        assert_eq!(activity_column_padding(10, 35, 12), Some(6));
    }

    #[test]
    fn activity_column_padding_long_title_with_badge_hides_column() {
        // The badge by itself fits but the column doesn't. The decision
        // is per-row "show the column or not" — the badge gets its own
        // unconditional render path.
        // prefix(20) + slot(6) + badge(12) + margin(1) = 39 > 35.
        assert_eq!(activity_column_padding(20, 35, 12), None);
    }

    #[test]
    fn row_tag_content_fits_within_max_width() {
        // RowTag.rendered() right-pads to max_width via `{:<width$}` —
        // if content ever exceeds max_width the format width is ignored
        // and the bracket span jitters. profile_short_code's documented
        // cap of 4 is the tightest case to spot-check.
        assert!(profile_short_code("forit-backup-extra").len() <= 4);
    }

    #[test]
    fn row_tag_rendered_pads_to_max_width() {
        let short = RowTag {
            content: "fb".to_string(),
            max_width: 4,
        };
        assert_eq!(short.rendered(), "[fb  ]");
        let exact = RowTag {
            content: "forb".to_string(),
            max_width: 4,
        };
        assert_eq!(exact.rendered(), "[forb]");
        let sb = RowTag {
            content: "sb".to_string(),
            max_width: 2,
        };
        assert_eq!(sb.rendered(), "[sb]");
    }

    #[test]
    fn activity_column_padding_narrow_pane_short_title() {
        // Was the regression: a 25-col pane was previously hidden by the
        // old fixed-30 floor, even when there was easily room.
        // prefix(8) + 7 trailing = 15 ≤ 25. Now shows.
        assert_eq!(activity_column_padding(8, 25, 0), Some(10));
    }

    #[test]
    fn activity_column_padding_saturates_on_overflow() {
        // Defensive: prefix near usize::MAX must not wrap. The checked_add
        // returns None which we map to "doesn't fit".
        assert_eq!(activity_column_padding(usize::MAX, 1000, 0), None);
    }

    #[test]
    fn stacked_list_drops_bottom_border() {
        assert!(!PaneLayout::Stacked.list_borders().contains(Borders::BOTTOM));
    }

    #[test]
    fn collapsed_list_drops_bottom_border() {
        assert!(!PaneLayout::Collapsed
            .list_borders()
            .contains(Borders::BOTTOM));
    }

    #[test]
    fn side_by_side_list_keeps_bottom_border() {
        assert!(PaneLayout::SideBySide
            .list_borders()
            .contains(Borders::BOTTOM));
    }

    #[test]
    fn stacked_list_keeps_right_border() {
        assert!(PaneLayout::Stacked.list_borders().contains(Borders::RIGHT));
    }

    #[test]
    fn collapsed_list_keeps_right_border() {
        assert!(PaneLayout::Collapsed
            .list_borders()
            .contains(Borders::RIGHT));
    }

    #[test]
    fn side_by_side_list_drops_right_border() {
        assert!(!PaneLayout::SideBySide
            .list_borders()
            .contains(Borders::RIGHT));
    }

    #[test]
    fn preview_always_owns_full_box() {
        for layout in [
            PaneLayout::Collapsed,
            PaneLayout::Stacked,
            PaneLayout::SideBySide,
        ] {
            assert_eq!(layout.preview_borders(), Borders::ALL);
        }
    }
}
