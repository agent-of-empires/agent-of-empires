//! Rendering for HomeView

use chrono::{DateTime, Utc};
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::time::{Duration, Instant};

use rattles::presets::prelude as spinners;

use super::{
    get_indent, live_send, HomeView, TerminalMode, ViewMode, ICON_COLLAPSED, ICON_DELETING,
    ICON_ERROR, ICON_EXPANDED, ICON_IDLE, ICON_STOPPED, ICON_UNKNOWN,
};
use crate::session::config::{GroupByMode, SortOrder};
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

/// Pick the agent view row icon for a session instance. Centralizes the
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
    if inst.is_archived() || inst.is_snoozed() {
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
/// returns `None` for sessions without a worktree).
pub(crate) fn compute_row_tag(
    inst: &crate::session::Instance,
    mode: crate::session::config::RowTagMode,
    in_all_profiles_view: bool,
) -> Option<RowTag> {
    use crate::session::config::RowTagMode;
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
        RowTagMode::Branch => inst.worktree_info.as_ref().and_then(|w| {
            // Complement the existing branch-on-divergence display
            // (rendered in `theme.branch` color earlier in the row) rather
            // than duplicate it. When `branch != title` the divergence
            // display already shows the branch, so the tag would just be
            // redundant. When `branch == title` the divergence display
            // stays quiet and the tag fills in.
            //
            // Workspace sessions (multi-repo, rendered as
            // `<branch> [N repos]`) are handled by a separate display
            // path and have no `worktree_info`, so they fall through to
            // `None` here naturally.
            if w.branch != inst.title {
                return None;
            }
            // Show the last `/`-segment of the branch (most informative
            // for `feature/foo` style names), truncated to 8 chars so the
            // tag stays narrow.
            let last = w.branch.rsplit('/').next().unwrap_or("");
            let trimmed: String = last.chars().take(8).collect();
            if trimmed.is_empty() {
                None
            } else {
                Some(RowTag {
                    content: trimmed,
                    max_width: 8,
                })
            }
        }),
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
    ) {
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
        let has_update_bar = update_info.is_some() || update_status.is_some();
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
        // Collapsed sidebar (live mode only): hand the whole main area to
        // the preview so the agent pane fills the terminal. The live-send
        // resize loop then reflows the agent to the wider geometry. Reset
        // on live-send exit, so the list always returns in the home view.
        if self.live_send.is_some() && self.sidebar_collapsed {
            self.divider_col = None;
            // render_list is skipped, so its hit-test rects would otherwise
            // keep last frame's values and a click in the now-preview area
            // could resolve to an invisible list row (and switch the live
            // target). Zero them so mouse hit-testing can't target the
            // hidden sidebar.
            self.list_area = Rect::default();
            self.list_inner_area = Rect::default();
            self.render_preview(frame, main_chunks[0], theme);
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

            self.render_list(frame, chunks[0], theme);
            self.render_preview(frame, chunks[1], theme);
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

            self.render_list(frame, chunks[0], theme);
            self.render_preview(frame, chunks[1], theme);
        }
        self.render_status_bar(frame, main_chunks[1], theme);

        if has_update_bar {
            self.render_update_bar(frame, main_chunks[2], theme, update_info, update_status);
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
            restart_dialog,
            hooks_install_dialog,
            hook_trust_dialog,
            intro_dialog,
            no_agents_dialog,
            changelog_dialog,
            info_dialog,
            snooze_duration_dialog,
            profile_picker_dialog,
            group_picker_dialog,
            sort_picker_dialog,
            project_session_picker_dialog,
            projects_dialog,
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

    fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.list_area = area;
        let profile = self.active_profile_display();
        let title = match &self.view_mode {
            ViewMode::Agent => compose_list_title("aoe", profile, self.group_by, self.sort_order),
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
            ViewMode::Agent => (theme.border, theme.title),
            ViewMode::Terminal | ViewMode::Tool(_) => {
                (theme.terminal_border, theme.terminal_border)
            }
        };
        // Current sort indicator on the bottom-right of the list block. Uses
        // ratatui's `title_bottom` so it renders on the existing border and
        // never intersects row content.
        let sort_indicator = format!(" sort: {} ", self.sort_order.label());
        let block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::BOTTOM)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(title)
            .title_style(Style::default().fg(title_color).bold())
            .title_bottom(
                Line::from(Span::styled(
                    sort_indicator,
                    Style::default().fg(theme.dimmed),
                ))
                .right_aligned(),
            )
            .padding(Padding::horizontal(1));

        let inner = block.inner(area);
        self.list_inner_area = inner;
        frame.render_widget(block, area);

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
            || self.hook_trust_dialog.is_some()
            || self.hooks_install_dialog.is_some()
            || self.intro_dialog.is_some()
            || self.no_agents_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self.info_dialog.is_some()
            || self.profile_picker_dialog.is_some()
            || self.group_picker_dialog.is_some()
            || self.sort_picker_dialog.is_some()
            || self.project_session_picker_dialog.is_some()
            || self.projects_dialog.is_some()
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
                let text = Cow::Owned(format!("{} ({})", name, session_count));
                let mut style = Style::default().fg(theme.group).bold();
                if crate::session::is_within_archived_section(path) {
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
                        ViewMode::Agent => {
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
                            let color = match inst.status {
                                Status::Running => theme.running,
                                Status::Waiting => theme.waiting,
                                Status::Idle => {
                                    theme.idle_color_at_age(idle_age, self.idle_decay_window)
                                }
                                Status::Unknown => theme.waiting,
                                Status::Stopped => theme.dimmed,
                                Status::Error => theme.error,
                                Status::Starting => theme.dimmed,
                                Status::Deleting => theme.waiting,
                                Status::Creating => theme.accent,
                            };
                            let mut style = Style::default().fg(color);
                            if inst.is_archived() {
                                // Archived rows render with one uniform
                                // muted glyph regardless of underlying
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
                            let title_text = if inst.is_archived() {
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
                            let (mut icon, color) = if terminal_running {
                                (spinner_running(&inst.created_at), theme.terminal_active)
                            } else {
                                (ICON_IDLE, theme.dimmed)
                            };
                            let mut style = Style::default().fg(color);
                            if inst.is_archived() {
                                // Archive lifecycle override mirrors the
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
                            let title_text = if inst.is_archived() {
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
                // Selected-row contrast gate. The previous unconditional
                // override stripped per-status color from every selected
                // row (running-green, error-red, etc.); the contrast check
                // only swaps in theme.text when the status fg actually
                // clashes with session_selection. 3:1 is WCAG AA Large /
                // bold-UI, which matches the row styling. Non-Rgb fg
                // (palette mode after downsample) falls through to the
                // override branch for safety. Italic/dim modifiers on
                // `style` survive both branches so archive/snooze visual
                // language reads either way.
                let fg = style.fg.unwrap_or(theme.text);
                if has_min_contrast(fg, theme.session_selection, 3.0) {
                    style.bold()
                } else {
                    style.fg(theme.text).bold()
                }
            } else {
                style
            },
        ));

        if let Item::Session { id, .. } = item {
            if let Some(inst) = self.get_instance(id) {
                if let Some(ws_info) = &inst.workspace_info {
                    line_spans.push(Span::styled(
                        format!("  {} [{} repos]", ws_info.branch, ws_info.repos.len()),
                        Style::default().fg(theme.branch),
                    ));
                } else if let Some(wt_info) = &inst.worktree_info {
                    if wt_info.branch != inst.title {
                        line_spans.push(Span::styled(
                            format!("  {}", wt_info.branch),
                            Style::default().fg(theme.branch),
                        ));
                    }
                }

                // Per-row tag. The mode is config-driven (see
                // `SessionConfig.row_tag` and the Settings UI "Row Tag"
                // field). Default is `None` so existing users see no
                // tag; power users opt in for `Auto` (profile in all-
                // profiles view), `Profile`, `Sandbox`, or `Branch`.
                // Counted into `used_width` below so the activity
                // column still right-aligns past the tag.
                if let Some(tag) =
                    compute_row_tag(inst, self.row_tag_mode, self.active_profile.is_none())
                {
                    line_spans.push(Span::styled(
                        format!("  {}", tag.rendered()),
                        Style::default().fg(theme.dimmed),
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
                // Cockpit-mode sessions are web-only (the TUI has no
                // structured rendering surface). Surface this with a
                // [web] badge so the user knows pressing Enter will
                // open an info dialog instead of attaching to a tmux
                // pane that doesn't exist. Takes precedence over the
                // existing container/host badge in Agent view; the
                // Terminal view keeps its existing badging because
                // the host terminal still works against the worktree.
                let badge_text: Option<&'static str> =
                    if inst.is_cockpit_mode() && self.view_mode != ViewMode::Terminal {
                        // Renamed from `[web]` now that the TUI renders
                        // cockpit sessions natively; `[cockpit]` better
                        // describes the substrate the badge marks.
                        Some(" [cockpit]")
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
                    line_spans.push(Span::styled(padded, Style::default().fg(theme.dimmed)));
                }

                if let Some(badge) = badge_text {
                    line_spans.push(Span::styled(badge, Style::default().fg(theme.sandbox)));
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

        let cache = select(self);
        cache.captured_lines = content.lines().count();
        cache.content = content;
        // Invalidate the cached parse; the next render that needs
        // `ensure_parsed` will re-run `ansi-to-tui`.
        cache.parsed_text = None;
        cache.session_id = Some(id);
        cache.dimensions = (width, height);
        cache.last_refresh = Instant::now();
        let captured_lines = cache.captured_lines;

        self.preview_scroll_offset = clamp_scroll_to_capture(
            self.preview_scroll_offset,
            captured_lines,
            self.preview_visible_rows,
        );
    }

    pub(super) fn refresh_preview_cache_if_needed(&mut self, width: u16, height: u16) {
        // Outside live-send, captures fork a fresh `tmux capture-pane`
        // so we throttle to 250ms (4 Hz). Inside live-send, captures
        // ride the long-lived `tmux -C` control-mode socket and cost
        // a single round-trip (~1-2ms), so there's no upside to
        // throttling: every render refreshes the preview, the agent's
        // output appears as soon as the main loop wakes (key event,
        // tokio ticker, or the %output wake-up the reader thread
        // pushes when tmux notifies us of new pane bytes). The result
        // is roughly attach-quality latency in the common case; the
        // residual gap is the cost of capture-pane + ratatui re-render
        // vs. tmux writing bytes straight into your terminal. The core's
        // `force` flag carries this: we pass `in_live` so live-send bypasses
        // the 250ms idle throttle.
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
                    // a peer or the web cockpit launches it), marking it synced now
                    // would pin the preview to the pre-start size and keep clipping
                    // until the next geometry change. Leaving it unset retries on
                    // the next refresh; `exists()` is cache-backed, so the retry is
                    // cheap.
                    if let Some(session) = self
                        .get_instance(&want.0)
                        .and_then(|inst| inst.tmux_session().ok())
                        .filter(|s| s.exists())
                    {
                        session.resize_window(width, height);
                        self.preview_pane_synced = Some(want);
                    }
                }
            }
        }

        // Captures always go through the fork-based path
        // (`Session::capture_pane_with_size` via the instance helper). The
        // long-lived `tmux -C` connection is reserved for `send-keys` from the
        // worker thread; on some tmux builds (macOS 3.x observed) the control-
        // mode connection EOFs mid-session, and routing captures through it as
        // well meant a dropped connection froze the preview until the user
        // exited live mode. Forking per capture costs ~5-10 ms on a local mac
        // and is invisible against the 250 ms idle throttle / `%output`-wake
        // cadence; we trade that overhead for "preview never gets stuck".
        //
        // Live vs. non-live failure semantics differ. In live mode an empty
        // capture (which is what `Session::capture_pane_with_size` returns when
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
            in_live,
            |s| &mut s.preview_cache,
            |s, id, capture_lines| {
                let in_live = s.live_send.is_some();
                // Only treat an empty fork capture as "preserve the existing
                // cache" when the cache is FOR THIS SAME SESSION. If the user
                // just switched live-send from session A to session B and B's
                // first capture comes back empty, holding the kill-switch would
                // leave A's content on screen under B's header. Cross-session we
                // always overwrite, falling back to an empty body (the same
                // "session looks gone" signal the non-live path uses).
                let same_session = s.preview_cache.session_id.as_deref() == Some(id);
                let fork_capture = s.get_instance(id).and_then(|inst| {
                    inst.capture_output_with_size(capture_lines, width, height)
                        .ok()
                });
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
    fn active_captured_lines(&self) -> usize {
        match &self.view_mode {
            ViewMode::Agent => self.preview_cache.captured_lines,
            ViewMode::Tool(_) => self.tool_preview_cache.captured_lines,
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
                    TerminalMode::Container => self.container_terminal_preview_cache.captured_lines,
                    TerminalMode::Host => self.terminal_preview_cache.captured_lines,
                }
            }
        }
    }

    fn render_preview(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let compact = area.width < responsive::STACKED_BREAKPOINT;
        let (border_color, title_color) = match self.view_mode {
            ViewMode::Agent => (theme.border, theme.title),
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
            .borders(Borders::ALL)
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
                    let (icon, icon_color) = match inst.status {
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
                ViewMode::Agent => " Preview ".to_string(),
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
        frame.render_widget(block, area);

        match self.view_mode {
            ViewMode::Agent => {
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
                    self.refresh_preview_cache_if_needed(pane_area.width, pane_area.height);
                    self.preview_cache.ensure_parsed();

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

        // Selection highlight goes last so it sits on top of whatever
        // the active ViewMode painted into the inner area. Only live
        // mode populates `preview_selection`, so this branch is a
        // no-op everywhere else.
        self.paint_preview_selection(frame, inner, theme);
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
    fn paint_preview_selection(&mut self, frame: &mut Frame, inner: Rect, theme: &Theme) {
        let Some(sel) = self.preview_selection else {
            return;
        };
        let segments = sel.flow_rects(inner);
        if segments.is_empty() {
            return;
        }
        // Capture cells only on the first render that follows a
        // finalized drag; subsequent renders just keep painting the
        // highlight. Reading from the buffer here (rather than from
        // `App` after `terminal.draw` returns) is load-bearing:
        // ratatui swaps front/back buffers on every frame, so
        // `terminal.current_buffer_mut()` post-draw points at the
        // empty next-frame buffer, not the cells we just drew.
        let capture = self.preview_copy_pending;
        if capture {
            self.preview_copy_pending = false;
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
            let clipped = segment.intersection(inner);
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
        if capture {
            let text = self.extract_preview_selection_text(&*frame.buffer_mut());
            self.preview_copy_text = text;
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

    fn render_status_bar(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
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

        let mut groups: Vec<(u8, Vec<Span<'static>>)> = Vec::new();

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
                groups.push((0, spans));
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
            groups.push((0, mk("↵ ", enter_action_text)));
        }

        groups.push((2, mk(if strict { "T" } else { "t" }, "View")));
        if matches!(self.view_mode, ViewMode::Tool(_)) {
            groups.push((1, mk(";", "Back")));
        } else if !self.tool_configs.is_empty() {
            groups.push((2, mk(";", "Tools")));
        }
        groups.push((3, mk(if strict { "^G" } else { "g" }, "Group")));

        // c: container/host toggle hint for sandboxed sessions in Terminal view
        if self.view_mode == ViewMode::Terminal {
            if let Some(id) = &self.selected_session {
                if let Some(inst) = self.get_instance(id) {
                    if inst.is_sandboxed() {
                        groups.push((4, mk(if strict { "C" } else { "c" }, "Mode")));
                    }
                }
            }
        }

        groups.push((2, mk(if strict { "N" } else { "n" }, "New")));

        // Priority 1: user's core daily workflow (message / del).
        // These survive the greedy pack under narrow-pane widths (iPad
        // Termius / Moshi ~80 cols) because they're the actions the user
        // reaches for most often. Del stays at p3, less frequent,
        // OK to drop first.
        if self.selected_session.is_some() {
            groups.push((1, mk(if strict { "M" } else { "m" }, "Msg")));
        }
        if !self.flat_items.is_empty() {
            groups.push((3, mk(if strict { "D" } else { "d" }, "Del")));
        }
        // Attention-workflow shortcuts (Archive / Fav / Snooze) only render
        // when the user is in Attention sort. They are only useful for
        // shaping the Attention queue; in Newest / Created / Last Accessed
        // they just take footer space without changing what the user sees.
        if self.sort_order == SortOrder::Attention {
            if !self.flat_items.is_empty() {
                groups.push((1, mk(if strict { "Z" } else { "z" }, "Archive")));
            }
            if self.selected_session.is_some() {
                groups.push((1, mk(if strict { "F" } else { "f" }, "Fav")));
                groups.push((1, mk(if strict { "H" } else { "h" }, "Snooze")));
            }
        }

        groups.push((4, mk_key("/")));
        groups.push((4, mk(if strict { "^D" } else { "D" }, "Diff")));
        groups.push((1, mk("^K", "Cmds")));
        groups.push((0, mk_key("?")));

        // Greedy pack by priority. Width of a group = sum of span char counts;
        // separator between kept groups adds 3 cols each (" · "). Reserve 1
        // col for the leading space margin.
        let widths: Vec<usize> = groups
            .iter()
            .map(|(_, g)| g.iter().map(|s| s.content.chars().count()).sum::<usize>())
            .collect();
        let avail = (area.width as usize).saturating_sub(1);

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

        let mut spans: Vec<Span> = vec![Span::raw(" ")];
        let mut first = true;
        for (i, (_, group)) in groups.into_iter().enumerate() {
            if !keep[i] {
                continue;
            }
            if !first {
                spans.push(Span::styled(" · ", sep_style));
            }
            spans.extend(group);
            first = false;
        }

        let status = Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.selection));
        frame.render_widget(status, area);
    }

    fn render_update_bar(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        info: Option<&UpdateInfo>,
        status: Option<&str>,
    ) {
        let update_style = Style::default().fg(theme.waiting).bold();
        // Transient status takes precedence over the persistent update banner
        // so users see the latest signal (e.g. "restart failed: ...") even
        // when an update banner is also up.
        let text = if let Some(s) = status {
            format!(" {s}  [Ctrl+x] dismiss")
        } else if let Some(info) = info {
            format!(
                " update available {} → {}  [u] update  [Ctrl+x] dismiss",
                info.current_version, info.latest_version
            )
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
}
