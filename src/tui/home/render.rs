//! Rendering for HomeView

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::time::{Duration, Instant};

use rattles::presets::prelude as spinners;

use super::{
    live_send, HomeView, TerminalMode, ViewMode, ICON_ARCHIVED_SECTION, ICON_COLLAPSED,
    ICON_DELETING, ICON_DORMANT, ICON_ERROR, ICON_EXPANDED, ICON_IDLE, ICON_PINNED, ICON_STOPPED,
    ICON_TRASH_SECTION, ICON_UNKNOWN, ICON_UNREAD,
};
use crate::containers::image_update::ImageUpdate;
use crate::session::config::{GroupByMode, RowTagMode, SortOrder};
use crate::session::{Item, Status};
use crate::tui::components::preview::{self, CachedPreview};
use crate::tui::components::{
    format_scroll_indicator, prefix_within_width, rendered_width,
    set_prefixed_input_cursor_position, truncate_to_width, HelpOverlay, Preview,
};
use crate::tui::responsive;
use crate::tui::styles::{has_min_contrast, Theme};
use crate::update::UpdateInfo;

/// Derive a frame offset from a session's creation timestamp so that
/// sessions started at different times show visually distinct spinner positions.
fn session_offset(created_at: &DateTime<Utc>) -> usize {
    created_at.timestamp_millis() as usize
}

/// Build the list-pane title. `prefix` is the leading label ("aoe", "Terminals",
/// "Tool: <name>"); `profile` is `Some(name)` only when a real filter is active, so the
/// default all-profiles state drops the `[<profile>]` segment. Group and sort state hang
/// off the prefix as `· project` / `· <sort label>`, each dropped when it is the default.
fn compose_list_title(
    prefix: &str,
    profile: Option<&str>,
    group_by: GroupByMode,
    sort_order: SortOrder,
) -> String {
    let mut suffix = String::new();
    match group_by {
        GroupByMode::Project => suffix.push_str(" · project"),
        GroupByMode::Org => suffix.push_str(" · org"),
        GroupByMode::Manual => {}
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

/// Source of truth for the pane arrangement passed to `render_list` / `render_preview`,
/// so their border masks honor DESIGN.md's single-shared-separator invariant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PaneLayout {
    Collapsed,
    Stacked,
    SideBySide,
}

impl PaneLayout {
    /// Border mask for the list block. Stacked (and defensively Collapsed) drop BOTTOM
    /// because the preview's TOP is the shared seam but keep RIGHT, since the pane spans
    /// the full width; SideBySide keeps BOTTOM and drops RIGHT.
    fn list_borders(self) -> Borders {
        match self {
            PaneLayout::Stacked | PaneLayout::Collapsed => {
                Borders::TOP | Borders::LEFT | Borders::RIGHT
            }
            PaneLayout::SideBySide => Borders::TOP | Borders::LEFT | Borders::BOTTOM,
        }
    }

    /// Border mask for the preview block. Every arm is `Borders::ALL` today because the
    /// preview owns the full box; the match stays exhaustive so a future asymmetric
    /// change is type-checked rather than silently regressing.
    fn preview_borders(self) -> Borders {
        match self {
            PaneLayout::Collapsed | PaneLayout::Stacked | PaneLayout::SideBySide => Borders::ALL,
        }
    }
}

/// Extra rows captured beyond the visible window so moderate scrolls don't force a fresh
/// capture on every wheel tick. Cache invalidation uses the same reserve.
const CAPTURE_BUFFER: u16 = 20;

/// Rows the compact system-health strip occupies.
const DIAGNOSTICS_STRIP_HEIGHT: u16 = 1;

const LIVE_SEND_RESIZE_RETRY_DELAY: Duration = Duration::from_secs(1);

fn live_resize_retry_due(
    retry_at: &mut Option<Instant>,
    resize_failed: bool,
    now: Instant,
) -> bool {
    if resize_failed {
        *retry_at = Some(now + LIVE_SEND_RESIZE_RETRY_DELAY);
    }
    if retry_at.is_some_and(|deadline| deadline <= now) {
        *retry_at = None;
        true
    } else {
        false
    }
}

/// Window captured while the user is off the live edge: the full scrollback in one shot,
/// rather than a window that re-anchors to the advancing live edge on every capture.
/// Matches tmux's default `history-limit` and the VT grid's `SCROLLBACK_LINES`.
const READING_CAPTURE_LINES: u16 = 2000;

/// Map a tmux pane cursor onto the preview's output rect for live-send.
///
/// `cursor.x`/`y` are pane relative; on a composite, add the pane origin from
/// [`crate::tmux::PaneCursor::composite_pane0`]. The renderer bottom-anchors captures that
/// overflow `output`, so the row is `output.y + min(line_count, visible_rows) -
/// pane_height + top + cursor.y`, while a short capture anchors at the top. That keeps the
/// cursor on the same text row for the status-row offset (#3515) and the shorter-pane case
/// (#2742). A hidden or out-of-bounds cursor yields `None`.
pub(super) fn map_live_preview_cursor(
    output: Rect,
    visible_rows: usize,
    line_count: usize,
    cursor: crate::tmux::PaneCursor,
) -> Option<Position> {
    if !cursor.visible {
        return None;
    }
    let anchor = line_count.min(visible_rows) as i32;
    let (left, top) = cursor
        .composite_pane0
        .map_or((0, 0), |rect| (rect.left as i32, rect.top as i32));
    let row = output.y as i32 + (anchor - cursor.pane_height as i32) + top + cursor.y as i32;
    let col = output.x as i32 + left + cursor.x as i32;
    if row < output.y as i32
        || row >= output.y as i32 + output.height as i32
        || col < output.x as i32
        || col >= output.x as i32 + output.width as i32
    {
        return None;
    }
    Some(Position::new(col as u16, row as u16))
}

/// Pane lines to capture for the preview, accounting for the scrollback offset plus a
/// buffer so moderate scrolls don't force a fresh capture on every wheel tick.
fn capture_lines_for(height: u16, scroll_offset: u16) -> usize {
    // Off the live edge: capture the whole scrollback once so the snapshot stays put. A
    // window that grew per notch was re-anchored to the advancing live edge on every
    // capture, yanking the text under the reader toward the tail. `scroll_exceeds_cache`
    // still fires the single grow when reading begins; the render path then stops
    // refreshing while `preview_is_frozen`.
    //
    // Depth is at least `READING_CAPTURE_LINES` but grows with the offset, so a pane whose
    // `history-limit` exceeds the baseline stays readable to its top.
    if scroll_offset > 0 {
        let depth = (scroll_offset as usize).max(READING_CAPTURE_LINES as usize);
        return (height as usize)
            .saturating_add(depth)
            .saturating_add(CAPTURE_BUFFER as usize);
    }
    (height as usize).saturating_add(CAPTURE_BUFFER as usize)
}

/// Whether the preview holds its snapshot instead of following live output: true while
/// the user reads scrollback (`scroll_offset > 0`) or has a selection in flight
/// (`has_selection`). Applying the worker's bottom-anchored frames then would yank the
/// read position toward the tail or slide the drag anchors off their text.
fn preview_frozen(scroll_offset: u16, has_selection: bool) -> bool {
    scroll_offset > 0 || has_selection
}

/// Grace beyond the shared tmux operation deadline before a preview worker counts as
/// stalled. A healthy capture may spend the full deadline inside one logical sample, so
/// restarting earlier would overlap legitimate workers and multiply tmux load.
const WORKER_STALL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Fold one capture-worker cycle observation into a stall verdict. The worker dedupes
/// unchanged frames, so publication cannot prove liveness; its cycle counter advances
/// before each deadline-bounded sample. An unchanged counter is tolerated through the
/// deadline plus grace, after which the caller replaces the worker off the tmux hot path.
fn worker_stalled_step(
    cycles: u64,
    prior: Option<(u64, std::time::Instant)>,
    now: std::time::Instant,
) -> (bool, Option<(u64, std::time::Instant)>) {
    match prior {
        None => (false, Some((cycles, now))),
        Some((seen, _)) if cycles != seen => (false, Some((cycles, now))),
        Some((seen, at)) => (
            now.saturating_duration_since(at)
                >= crate::tmux::TMUX_COMMAND_TIMEOUT.saturating_add(WORKER_STALL_GRACE),
            Some((seen, at)),
        ),
    }
}

pub(super) fn passive_resize_invalidates_live_geometry(
    live_target: Option<&live_send::LiveSendTarget>,
    selected_session: Option<&str>,
    completed_session: &str,
) -> bool {
    live_target == Some(&live_send::LiveSendTarget::Agent)
        && selected_session == Some(completed_session)
}
/// Whether the cached capture window still covers the requested scroll: true when the
/// visible window plus BUFFER headroom would run past the end of the captured content.
fn scroll_exceeds_cache(cache_captured_lines: usize, height: u16, scroll_offset: u16) -> bool {
    let needed = (height as usize)
        .saturating_add(scroll_offset as usize)
        .saturating_add(CAPTURE_BUFFER as usize);
    needed > cache_captured_lines
}

/// Whether a capture handed back every line the pane holds, so re-capturing could not
/// grow it: `capture_lines_for` asks for `height + CAPTURE_BUFFER` (plus reading depth),
/// and a shorter result has hit the end of the content.
fn capture_is_exhausted(cache_captured_lines: usize, requested_lines: usize) -> bool {
    cache_captured_lines > 0 && cache_captured_lines < requested_lines
}
/// What the passive (non-live) preview sync should do this refresh for the
/// wanted `(session_id, cols, rows)` geometry.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PassiveResizeStep {
    /// The pane already matches; nothing to do (and any armed pending
    /// geometry was transient noise, so the caller drops it).
    InSync,
    /// First sighting of this geometry; remember it and wait one refresh
    /// before resizing.
    Arm,
    /// Same geometry wanted on two consecutive refreshes; resize now.
    Fire,
}

/// Debounce for the passive preview sync: resize only once the same geometry has been
/// wanted on two consecutive refreshes.
///
/// The `EnterLiveSend` / `SendMessage` handlers each draw one frame with a transient
/// toast that claims a bottom row, so that frame's output rect is a row shorter than the
/// frames either side. Chasing it resized the agent's pane down and straight back up
/// ~30ms later, and the double SIGWINCH made bottom-anchored agent UIs jump. Real
/// geometry changes land one refresh later, within the normal poll cadence.
fn passive_resize_step(
    want: &(String, u16, u16),
    synced: Option<&(String, u16, u16)>,
    pending: Option<&(String, u16, u16)>,
) -> PassiveResizeStep {
    if synced == Some(want) {
        PassiveResizeStep::InSync
    } else if pending == Some(want) {
        PassiveResizeStep::Fire
    } else {
        PassiveResizeStep::Arm
    }
}

/// What the fleet reconcile should do for one session wanting `(cols, rows)`. The fleet
/// debounces in `reconcile_passive_fleet`, so this only dedups: an already-synced,
/// declined or queued geometry is not re-sent.
#[derive(Debug, PartialEq, Eq)]
enum FleetPassiveStep {
    Skip,
    Queue,
}

fn fleet_passive_step(
    want: (u16, u16),
    synced: Option<(u16, u16)>,
    declined: Option<(u16, u16)>,
    queued: Option<(u16, u16)>,
) -> FleetPassiveStep {
    if synced == Some(want) || declined == Some(want) || queued == Some(want) {
        FleetPassiveStep::Skip
    } else {
        FleetPassiveStep::Queue
    }
}

/// How long a declined passive resize stays parked before the fleet retries it. Declines
/// come from a live attach or an active size owner and nothing announces when those go
/// away, so a bounded retry recovers within half a minute, at one attempt per interval.
pub(super) const PASSIVE_DECLINE_RETRY: Duration = Duration::from_secs(30);

/// Whether a pane observation contradicts an applied passive resize: taken after adoption
/// (the shared snapshot can lag our own resize) and showing a size other than the one we
/// applied. True means another client resized the window, so the synced entry must drop
/// and the reconcile re-assert the pane.
fn passive_synced_contradicted(
    synced: &super::PassiveSynced,
    observed: (u16, u16),
    observed_at: Instant,
) -> bool {
    observed_at > synced.adopted_at && observed != (synced.cols, synced.window_rows)
}

/// Clamp the user's preview scroll offset to what the freshly captured pane can render,
/// so it cannot drift into phantom territory when tmux history is shorter than
/// `MAX_PREVIEW_SCROLL`.
///
/// `visible_height` is the rendered output-body height the caller already computed
/// (`preview_visible_rows`), NOT the raw pane height: re-deriving it with a fixed `- 1`
/// over-counts the max offset by a row whenever the inner banner is hidden, stalling
/// live-follow one row early.
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

/// Slow `breathe` rattle for a freshly-stopped Idle session, the same animation as
/// Starting but slower and colored `theme.fresh_idle`, so it reads as a gentle reminder.
/// The phase offset uses `idle_entered_at` so sessions don't all sync to one frame.
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

/// Structured view row icon for a session. Centralizes the archive/snooze override that
/// kills the live spinner for sunk rows so the list reads as parked. Crate-visible so
/// tests can pin the override without the full render pipeline.
pub(crate) fn agent_row_icon(inst: &crate::session::Instance) -> &'static str {
    // A dormant (idle-reaped, resumable) structured worker gets its own glyph, taking
    // precedence over the raw status but yielding to the sink override below. See #2250.
    let icon = if inst.is_shown_dormant() {
        ICON_DORMANT
    } else {
        match inst.status {
            Status::Running => spinner_running(&inst.created_at),
            Status::Waiting => spinner_waiting(&inst.created_at),
            Status::Idle => ICON_IDLE,
            Status::Unknown => ICON_UNKNOWN,
            Status::Stopped => ICON_STOPPED,
            Status::Error => ICON_ERROR,
            Status::Starting => spinner_starting(&inst.created_at),
            Status::Deleting => ICON_DELETING,
            Status::Creating => spinner_starting(&inst.created_at),
        }
    };
    // Error and Deleting are live operation states this TUI set, not stale persisted pane
    // statuses, so they punch through the sunk-row mask below; swallowing them left a
    // failed Empty Trash indistinguishable from a healthy trash row.
    if matches!(inst.status, Status::Error | Status::Deleting) {
        return icon;
    }
    if inst.is_archived() || inst.is_snoozed() || inst.is_trashed() {
        ICON_STOPPED
    } else {
        icon
    }
}

/// A view mode's contribution to a session row: glyph, color and any modifier describing
/// its own backing pane. Structured seeds from `Instance.status`, Terminal from the paired
/// terminal's liveness, Tool from the tool pane's. Everything layered on top is
/// mode-independent and lives in [`decorate_row`].
struct RowSeed {
    icon: &'static str,
    color: Color,
    modifier: ratatui::style::Modifier,
}

/// How a view mode resolves a sunk row (archived / trashed / snoozed).
enum SunkRow {
    /// Structured: the agent's own resting glyph. Error and Deleting punch through the
    /// sink mask (they are live delete-op states, not stale pane statuses) and the seed
    /// carries `ICON_ERROR` + `theme.error`; [`agent_row_icon`] makes the same exception.
    AgentStatus(&'static str),
    /// Terminal / Tool: one muted glyph, unconditionally. These seeds describe pane
    /// liveness and carry no error affordance, so a delete-op status punching through
    /// would paint a bright "alive" row inside a shelf and still signal nothing.
    Pane,
}

/// The archive/trash, snooze, urgent and favorite overlays every view mode paints on top
/// of its [`RowSeed`], plus the matching title prefix. `sunk` says how this view resolves
/// a sunk row; see [`SunkRow`].
fn decorate_row(
    inst: &crate::session::Instance,
    in_attention: bool,
    show_favorite: bool,
    seed: RowSeed,
    sunk: SunkRow,
    theme: &Theme,
) -> (&'static str, std::borrow::Cow<'static, str>, Style) {
    use ratatui::style::Modifier;
    use std::borrow::Cow;

    let mut icon = seed.icon;
    let mut style = Style::default().fg(seed.color).add_modifier(seed.modifier);

    let (sunk_icon, punches_through) = match sunk {
        SunkRow::AgentStatus(resting) => (
            resting,
            matches!(inst.status, Status::Error | Status::Deleting),
        ),
        SunkRow::Pane => (ICON_STOPPED, false),
    };
    if (inst.is_archived() || inst.is_trashed()) && !punches_through {
        // Archived and trashed rows render one uniform muted glyph whatever the pane
        // status was: the pane is dead, so painting it would mislead. The section header
        // is the textual cue, so this is a dim color with no italic modifier.
        icon = sunk_icon;
        style = Style::default().fg(theme.dimmed);
    } else if in_attention && inst.is_snoozed() {
        // Snooze decoration is Attention-only; elsewhere the row paints its real state
        // and the timer keeps running.
        icon = sunk_icon;
        style = Style::default()
            .fg(theme.dimmed)
            .add_modifier(Modifier::ITALIC)
            .add_modifier(Modifier::DIM);
    } else if in_attention && inst.is_urgent() {
        // Urgent decoration is Attention-only: the flag persists in other modes, but the
        // cross-tier promoter visual only means something when tiers order the list.
        style = Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::RAPID_BLINK);
    } else if show_favorite && crate::session::is_live_favorite(inst) {
        style = style
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINED);
    }

    // Prefix priority: archive (none) > snooze (`z `) > urgent (`! `) > favorite (`* `).
    // Snooze and urgent are Attention-only so other sorts show no decoration for state the
    // user did not opt into; the star also shows elsewhere because favorites-first pins
    // the row there too.
    let title_text = if inst.is_archived() || inst.is_trashed() {
        Cow::Owned(inst.title.clone())
    } else if in_attention && inst.is_snoozed() {
        Cow::Owned(format!("z {}", inst.title))
    } else if in_attention && inst.is_urgent() {
        Cow::Owned(format!("! {}", inst.title))
    } else if show_favorite && crate::session::is_live_favorite(inst) {
        Cow::Owned(format!("* {}", inst.title))
    } else {
        Cow::Owned(inst.title.clone())
    };

    (icon, title_text, style)
}

/// Append the selected row's `last_error` in red to a shelf placeholder when the row sits
/// in `Status::Error`, where `apply_deletion_results` parks a failed permanent delete.
/// Without it the calm placeholder swallowed the failure.
fn push_shelf_error_lines(
    lines: &mut Vec<Line<'static>>,
    inst: Option<&crate::session::Instance>,
    theme: &Theme,
) {
    let Some(error) = inst
        .filter(|i| i.status == Status::Error)
        .and_then(|i| i.last_error.as_deref())
    else {
        return;
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Error:",
        Style::default().fg(theme.error).bold(),
    )));
    for l in error.split('\n') {
        lines.push(Line::from(Span::styled(
            l.to_string(),
            Style::default().fg(theme.error),
        )));
    }
}

/// Centered placeholder body: heading, message, the selected row's `last_error`, and an
/// optional hint. `wrap` is set by the shelf placeholders, whose prose is
/// title-dependent; the fixed-copy ones lay their own lines out.
struct Placeholder<'a> {
    heading: &'a str,
    body: String,
    inst: Option<&'a crate::session::Instance>,
    hint: Option<Line<'static>>,
    wrap: bool,
}

fn render_placeholder(frame: &mut Frame, area: Rect, theme: &Theme, placeholder: Placeholder) {
    let Placeholder {
        heading,
        body,
        inst,
        hint,
        wrap,
    } = placeholder;
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            heading.to_string(),
            Style::default().fg(theme.text).bold(),
        )),
        Line::from(""),
        Line::from(Span::styled(body, Style::default().fg(theme.dimmed))),
    ];
    push_shelf_error_lines(&mut lines, inst, theme);
    if let Some(hint) = hint {
        lines.push(Line::from(""));
        lines.push(hint);
    }
    let para = Paragraph::new(lines).alignment(Alignment::Center);
    let para = if wrap {
        para.wrap(Wrap { trim: false })
    } else {
        para
    };
    frame.render_widget(para, area);
}

/// A `Press <key><tail>` hint line for a placeholder.
fn press_hint(theme: &Theme, key: &str, tail: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("Press ", Style::default().fg(theme.dimmed)),
        Span::styled(key.to_string(), Style::default().fg(theme.hint).bold()),
        Span::styled(tail.to_string(), Style::default().fg(theme.dimmed)),
    ])
}

/// Per-row tag content plus the mode's max content width. The renderer right-pads
/// `content` to `max_width` so the bracket span is fixed-width across rows (`[fb  ]` vs
/// `[def ]`) and the activity column cannot reflow. `compute_row_tag` truncates to the
/// same cap, so `rendered()` never truncates.
pub(crate) struct RowTag {
    pub content: String,
    pub max_width: usize,
}

const BRANCH_TAG_WIDTH: usize = 12;

impl RowTag {
    /// The bracketed tag, right-padded to `max_width` terminal cells. Padding is measured
    /// by display width, so a wide glyph (`界`) or a combining mark still yields a bracket
    /// span of exactly `max_width + 2` cells.
    pub fn rendered(&self) -> String {
        // Cap first, then pad, both in painted cells: a `UnicodeWidthStr` pad overflows
        // the contract for scripts it scores below what ratatui spends. Grapheme-aligned,
        // so emoji + VS16 and skin-tone sequences stay whole.
        let content = prefix_within_width(&self.content, self.max_width);
        let pad = self.max_width.saturating_sub(rendered_width(content));
        format!("[{content}{}]", " ".repeat(pad))
    }
}

/// The per-row tag for an instance and mode, or `None` when this context renders none.
/// `Auto` only renders in all-profiles view; other modes render whenever their content
/// exists (`Branch` yields `None` without branch metadata).
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
    let suffix_width = rendered_width(&suffix);
    if suffix_width >= BRANCH_TAG_WIDTH {
        return Some(RowTag {
            content: prefix_within_width(&suffix, BRANCH_TAG_WIDTH).to_string(),
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
    // Cut in cells, not characters: `RowTag::rendered` caps the whole tag in cells, so a
    // character-sized cut lets a wide branch fill the cap and push the repo count off.
    let trimmed = prefix_within_width(last, max_width);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Compact display code for a profile name, for the per-row tag in all-profiles view.
///
/// Delimited names keep a short lead segment (at most three grapheme clusters) whole and
/// append the first cluster of each remaining segment, so siblings sharing an initial stay
/// distinguishable (`gna-main` -> `gnam`, `bsc-main` / `bso-main` -> `bscm` / `bsom`); a
/// longer lead contributes its initial only (`forit-backup` -> `fb`). Single-segment names
/// take their first three clusters (`default` -> `def`). The code is lowercased and cut to
/// four cells on a cluster boundary, measured as [`RowTag::rendered`] measures it, so a
/// case expansion (`İ`), a wide glyph or an emoji sequence cannot push the tag past
/// [`RowTag::max_width`]. Deterministic per name, so two profiles may share a code; the
/// full name still shows in the list title and the New/Restart dialogs.
pub(crate) fn profile_short_code(profile: &str) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    const MAX_CELLS: usize = 4;
    let segments: Vec<&str> = profile
        .split(['-', '_'])
        .filter(|s| !s.is_empty())
        .collect();
    let code: String = match segments.as_slice() {
        [] => String::new(),
        [single] => single.graphemes(true).take(3).collect(),
        [lead, rest @ ..] => {
            let mut code: String = if lead.graphemes(true).count() <= 3 {
                (*lead).to_string()
            } else {
                lead.graphemes(true).take(1).collect()
            };
            code.extend(rest.iter().filter_map(|s| s.graphemes(true).next()));
            code
        }
    };
    prefix_within_width(&code.to_lowercase(), MAX_CELLS).to_string()
}

/// Format a timestamp as a compact relative age (`3m`, `2h`, `4d`, `2mo`), empty for
/// `None` so callers can substitute unconditionally.
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

/// Format a remaining snooze as a compact countdown that fits `LAST_ACTIVITY_SLOT` (`23m`,
/// `1h`, `5d`), falling back to `<1m` so a sub-minute remainder reads as "about to wake".
/// The validator caps at 30 days, which the day branch covers.
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

/// Minimum list width for the last-activity column; below it the column is hidden.
/// Compared against `inner.width` (the pane minus its border), so 30 lets the column
/// appear for `home_list_width` in the common 35-45 range and on tight mobile panes, where
/// the 6-char age slot plus ~24 chars of title/branch still fits.
///
/// Width reserved for the right-aligned column: 5 for the label (`"<1m"`, `"30mo"`) plus
/// one of left padding.
const LAST_ACTIVITY_SLOT: usize = 6;

/// Trailing gap between the activity slot (or terminal-mode badge) and the pane's right
/// border, one cell, matching the breathing room other widgets leave around the border.
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

/// Where the right-aligned activity column lives on a session row.
///
/// `prefix_width` is the display width of the spans already pushed, `list_width` the inner
/// width of the list pane, `badge_width` 0 when no terminal-mode badge follows. `Some(pad)`
/// is the padding to push between prefix and column when it fits with
/// `LAST_ACTIVITY_SLOT`, the badge and `LAST_ACTIVITY_RIGHT_MARGIN`; `None` means the row
/// is too wide and the title wins.
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
        // Start each frame with no footer buttons and no sidebar collapse rects; the home
        // render paths repopulate them. The takeover views return before those run, so
        // clearing here keeps a stale rect from swallowing a click on the diff/serve
        // surface (the collapse handler runs ahead of `hit_diff`).
        self.footer_buttons.clear();
        self.collapse_button_area = Rect::default();
        self.expand_strip_area = Rect::default();
        // Hyperlink cells are per-frame: a takeover view or a link-free preview must
        // leave none behind for the backend to re-emit. Recover from poison rather than
        // skip, or the last recorded set would be re-wrapped over unrelated cells forever.
        self.hyperlink_cells
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

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
            if diff.selected_file_is_markdown() {
                let _ = diff.get_current_file_contents();
            }

            // No list/preview divider exists while the diff takeover owns the screen;
            // clear it so a stale value can't hit-test as draggable.
            self.divider_col = None;
            self.main_area_width = 0;

            diff.render(frame, area, theme);
            return;
        }

        // Serve view takes over the whole screen
        if let Some(ref serve) = self.serve_view {
            self.divider_col = None;
            self.main_area_width = 0;
            serve.render(frame, area, theme);
            return;
        }

        // Layout: main area + status bar + optional update bar. The update bar carries
        // both persistent banners and transient toasts, so it needs a row whenever either
        // is present, or a toast fired without a pending update would never show.
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

        // The diagnostics strip docks under the session-list column (see
        // `diagnostics_dock`) so it stays narrow and the preview keeps its height.
        let content_area = main_chunks[0];
        let available_width = content_area.width;
        self.main_area_width = available_width;
        // Collapsed sidebar: the list shrinks to a click-to-expand strip and the preview
        // takes the rest of the width. Width-independent, since a collapsed list is narrow
        // enough that re-imposing the stacked breakpoint would only waste space.
        if self.sidebar_collapsed {
            self.divider_col = None;
            let strip_width = responsive::COLLAPSED_STRIP_WIDTH.min(available_width);
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(strip_width), Constraint::Min(0)])
                .split(content_area);
            // The full list isn't drawn, so its hit-test rects would keep last frame's
            // values and a click in the preview area could resolve to an invisible row.
            // Zero them; `render` already cleared the collapse rect and the strip sets its
            // own.
            self.list_area = Rect::default();
            self.list_inner_area = Rect::default();
            self.shelf_inner_area = Rect::default();
            // Preview keeps full height; the strip docks under the collapsed
            // list column.
            let strip_col = self.diagnostics_dock(frame, chunks[0], theme);
            self.render_collapsed_strip(frame, strip_col, theme);
            self.render_preview(frame, chunks[1], theme, PaneLayout::Collapsed);
        } else if available_width < responsive::STACKED_BREAKPOINT {
            let main_height = content_area.height;
            let list_height = responsive::stacked_list_height(main_height);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(list_height),
                    Constraint::Min(responsive::STACKED_PREVIEW_MIN),
                ])
                .split(content_area);

            // Stacked layout has no vertical divider; only the side-by-side
            // path exposes the resize-by-drag affordance.
            self.divider_col = None;

            // Stacked: the list sits above the preview, so there is no column to dock
            // under; the strip spans the list's width.
            let list_rect = self.diagnostics_dock(frame, chunks[0], theme);
            self.render_list(frame, list_rect, theme, PaneLayout::Stacked);
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
                .split(content_area);

            // Layout chunks are contiguous, so chunks[1].x is the preview's visible left
            // border, which the user reads as the divider. Hit-test uses the list's
            // y-range, which matches the preview's side by side.
            self.divider_col = Some(chunks[1].x);

            // Strip docks under the list column; the preview keeps full height.
            let list_rect = self.diagnostics_dock(frame, chunks[0], theme);
            self.render_list(frame, list_rect, theme, PaneLayout::SideBySide);
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
                crate::session::AttachMode::LiveSend
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

        // Every `Option<Dialog>` field renders the same way, so the macro keeps the list
        // of active dialog types in one place. `&mut self.$field` lets a dialog whose
        // `render` records screen rects (currently `unified_delete_dialog`) mutate self;
        // `&self` render methods still work through auto-deref.
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
            attach_project_dialog,
            project_session_picker_dialog,
            projects_dialog,
            plugin_manager_dialog,
            skills_manager_dialog,
            command_palette,
            tool_picker_dialog,
            send_message_dialog,
            permission_response_dialog,
            update_confirm_dialog,
            // context_menu renders last so its popup sits above any underlying dialog.
            context_menu,
        );
    }

    /// Dock the diagnostics strip under the session-list column: carve
    /// `DIAGNOSTICS_STRIP_HEIGHT` rows off the bottom of `column`, render there, and return
    /// the reduced rect. Returns `column` unchanged when the strip is hidden or the column
    /// cannot spare the rows, so the list is never starved below one row.
    fn diagnostics_dock(&mut self, frame: &mut Frame, column: Rect, theme: &Theme) -> Rect {
        self.diagnostics_area = Rect::default();
        if !self.show_diagnostics || column.height <= DIAGNOSTICS_STRIP_HEIGHT {
            return column;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(DIAGNOSTICS_STRIP_HEIGHT),
            ])
            .split(column);
        crate::tui::components::diagnostics::render(
            frame,
            rows[1],
            theme,
            &self.metrics,
            self.diagnostics_hovered,
        );
        self.diagnostics_area = rows[1];
        rows[0]
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

    /// Render the collapsed sidebar: a narrow bordered strip standing in for the list.
    /// The whole strip is the click target (`expand_strip_area`) and re-expands it; a `»`
    /// hints the direction and the session count below it says how many rows are hidden.
    fn render_collapsed_strip(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.expand_strip_area = area;
        let border_color = match self.view_mode {
            ViewMode::Structured => theme.border,
            ViewMode::Terminal | ViewMode::Tool(_) => theme.terminal_border,
        };
        // Drop the right border so the preview's left border is the single shared seam,
        // matching the expanded list and DESIGN.md.
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
        let mut title = match &self.view_mode {
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
        if !self.legacy_duplicate_reports.is_empty() {
            // Fail-closed surface (#3459): ambiguous copies are hidden from
            // the list, so without this marker the loss is silent.
            let count = self.legacy_duplicate_reports.len();
            let plural = if count == 1 { "" } else { "s" };
            title.push_str(&format!("  \u{26a0} {count} ambiguous session{plural}"));
        }
        let (border_color, title_color) = match self.view_mode {
            ViewMode::Structured => (theme.border, theme.title),
            ViewMode::Terminal | ViewMode::Tool(_) => {
                (theme.terminal_border, theme.terminal_border)
            }
        };
        let borders = layout.list_borders();
        // The Trash / Archived sections render in a pinned bottom shelf instead of
        // scrolling with the list. They are a contiguous suffix of `flat_items` starting at
        // `list_len`, with a divider between; when that divider shows it carries the sort
        // indicator, so the bottom-border copy is suppressed.
        let shelf_start = self.shelf_start();
        let list_len = shelf_start.unwrap_or(self.flat_items.len());
        let show_divider = shelf_start.is_some() && list_len > 0;
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
        if borders.contains(Borders::BOTTOM) && !show_divider {
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
        // Zeroed by default; the shelf branch sets it when a shelf is drawn, so the
        // early-return paths leave no stale rect that could resolve a click to an undrawn
        // shelf row.
        self.shelf_inner_area = Rect::default();
        frame.render_widget(block, area);

        // Collapse affordance on the top-right border, drawn as an overlay after the
        // block so its clickable rect is exact, and skipped on a list too narrow to spare
        // the columns without colliding with the title (`render` already zeroed the rect).
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

        if !self.has_instances() && !self.has_any_groups() {
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

        // Split the inner area into the scrolling workspace list, an optional
        // divider carrying the sort indicator, and the pinned bottom shelf that
        // holds the Trash / Archived sections. With no shelf this reduces to the
        // list filling `inner`, identical to the pre-shelf layout.
        const SHELF_MIN_ROWS: usize = 2;
        let inner_h = inner.height as usize;
        let (list_region, divider_y, shelf_region) = if shelf_start.is_some() {
            let shelf_len = self.flat_items.len() - list_len;
            let divider_rows = if show_divider { 1 } else { 0 };
            // Keep the shelf near 40% of the pane at most so an expanded Trash
            // can't crowd out the workspace list, but always leave room for the
            // two section headers.
            let shelf_cap = ((inner_h * 2) / 5).max(SHELF_MIN_ROWS);
            let shelf_budget = inner_h.saturating_sub(divider_rows);
            let shelf_visible = shelf_len.min(shelf_cap).min(shelf_budget);
            let list_h = inner_h.saturating_sub(shelf_visible + divider_rows);
            let list_region = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: list_h as u16,
            };
            let divider_y = show_divider.then_some(inner.y + list_h as u16);
            let shelf_region = Rect {
                x: inner.x,
                y: inner.y + (list_h + divider_rows) as u16,
                width: inner.width,
                height: shelf_visible as u16,
            };
            (list_region, divider_y, shelf_region)
        } else {
            (inner, None, Rect::default())
        };
        self.list_inner_area = list_region;
        self.shelf_inner_area = shelf_region;

        let hover_idx = self.hovered_index();

        // --- Workspace list (every row before the shelf) ---
        let list_visible_height = if self.search_bar_visible() {
            (list_region.height as usize).saturating_sub(1)
        } else {
            list_region.height as usize
        };
        // The cursor may be parked in the shelf; clamp it to the last list row
        // for scroll purposes so the list keeps a stable offset instead of
        // trying to scroll to a shelf index. No list row ends up selected in
        // that case, because the real `self.cursor` never matches a list index.
        let list_cursor = self.cursor.min(list_len.saturating_sub(1));
        let scroll = crate::tui::components::scroll::calculate_scroll(
            list_len,
            list_cursor,
            list_visible_height,
        );

        let mut lines: Vec<Line> = Vec::new();
        if scroll.has_more_above {
            lines.push(Line::from(Span::styled(
                format!("  [{} more above]", scroll.scroll_offset),
                Style::default().fg(theme.dimmed),
            )));
        }
        for (i, item) in self.flat_items[..list_len]
            .iter()
            .skip(scroll.scroll_offset)
            .take(scroll.list_visible)
            .enumerate()
        {
            let abs_idx = i + scroll.scroll_offset;
            let is_selected = self.is_sidebar_item_selected(item, abs_idx);
            let is_hovered = !is_selected && Some(abs_idx) == hover_idx;
            let is_match =
                !self.search_matches.is_empty() && self.search_matches.contains(&abs_idx);
            let mut line = self.render_item_line(item, is_selected, is_match, theme, inner.width);
            // Selection wins over hover, so the already-selected row under the
            // mouse keeps the brighter selected background.
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
            let remaining = list_len - scroll.scroll_offset - scroll.list_visible;
            lines.push(Line::from(Span::styled(
                format!("  [{} more below]", remaining),
                Style::default().fg(theme.dimmed),
            )));
        }
        frame.render_widget(Paragraph::new(lines), list_region);

        // --- Divider between the workspace list and pinned shelf ---
        if let Some(dy) = divider_y {
            let divider = Line::from(Span::styled(
                "─".repeat(list_region.width as usize),
                Style::default().fg(theme.border),
            ));
            frame.render_widget(
                Paragraph::new(divider),
                Rect {
                    x: list_region.x,
                    y: dy,
                    width: list_region.width,
                    height: 1,
                },
            );
        }

        // --- Pinned shelf (Trash / Archived sections), scrolled on its own ---
        if shelf_start.is_some() && shelf_region.height > 0 {
            let shelf_items = &self.flat_items[list_len..];
            let shelf_visible = shelf_region.height as usize;
            let shelf_cursor = self
                .cursor
                .saturating_sub(list_len)
                .min(shelf_items.len().saturating_sub(1));
            let sscroll = crate::tui::components::scroll::calculate_scroll(
                shelf_items.len(),
                shelf_cursor,
                shelf_visible,
            );
            let mut slines: Vec<Line> = Vec::new();
            if sscroll.has_more_above {
                slines.push(Line::from(Span::styled(
                    format!("  [{} more above]", sscroll.scroll_offset),
                    Style::default().fg(theme.dimmed),
                )));
            }
            for (i, item) in shelf_items
                .iter()
                .skip(sscroll.scroll_offset)
                .take(sscroll.list_visible)
                .enumerate()
            {
                let abs_idx = list_len + sscroll.scroll_offset + i;
                let is_selected = self.is_sidebar_item_selected(item, abs_idx);
                let is_hovered = !is_selected && Some(abs_idx) == hover_idx;
                let is_match =
                    !self.search_matches.is_empty() && self.search_matches.contains(&abs_idx);
                let mut line =
                    self.render_item_line(item, is_selected, is_match, theme, inner.width);
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
                slines.push(line);
            }
            if sscroll.has_more_below {
                let remaining = shelf_items.len() - sscroll.scroll_offset - sscroll.list_visible;
                slines.push(Line::from(Span::styled(
                    format!("  [{} more below]", remaining),
                    Style::default().fg(theme.dimmed),
                )));
            }
            frame.render_widget(Paragraph::new(slines), shelf_region);
        }

        // Render the search bar while typing and while a committed search is live, so
        // the query stays pinned at the bottom until Esc. The inverted cursor cell and
        // the terminal caret are only drawn while typing; a committed bar is static.
        if self.search_bar_visible() {
            let search_area = Rect {
                x: list_region.x,
                y: list_region.y + list_region.height.saturating_sub(1),
                width: list_region.width,
                height: 1,
            };

            let value = self.search_query.value();
            let text_style = Style::default().fg(theme.search);

            let mut spans = vec![Span::styled("/", text_style)];
            if self.search_active {
                // Split value into: before cursor, char at cursor, after cursor
                // and invert the cursor cell so the caret is visible.
                let cursor_pos = self.search_query.cursor();
                let cursor_style = Style::default().fg(theme.background).bg(theme.search);
                let before: String = value.chars().take(cursor_pos).collect();
                let cursor_char: String = value
                    .chars()
                    .nth(cursor_pos)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| " ".to_string());
                let after: String = value.chars().skip(cursor_pos + 1).collect();
                if !before.is_empty() {
                    spans.push(Span::styled(before, text_style));
                }
                spans.push(Span::styled(cursor_char, cursor_style));
                if !after.is_empty() {
                    spans.push(Span::styled(after, text_style));
                }
            } else if !value.is_empty() {
                // Committed: the query is static, no caret.
                spans.push(Span::styled(value.to_string(), text_style));
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
            if self.search_active && !self.has_overlay_above_search() {
                set_prefixed_input_cursor_position(frame, search_area, "/", &self.search_query);
            }
        }
    }

    fn has_overlay_above_search(&self) -> bool {
        let serve_open = self.serve_view.is_some();

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
            || self.attach_project_dialog.is_some()
            || self.project_session_picker_dialog.is_some()
            || self.projects_dialog.is_some()
            || self.plugin_manager_dialog.is_some()
            || self.skills_manager_dialog.is_some()
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
        let indent = " ".repeat(item.depth().min(9));

        // Favorite, snooze and urgent visuals render only under Attention sort, so the
        // sidebar stays clean for users who don't run a triage workflow. Archive is
        // universal: it is a lifecycle action and its rows live in the pinned Archived
        // section in every sort mode.
        let in_attention = self.sort_order == SortOrder::Attention;
        // Favorite is a pin in every sort order once favorites-first is on, so its
        // decoration follows the keybinding's predicate (`Context::FavoritesUsable`).
        // Snooze and urgent stay Attention-only, tied to the tier model.
        let show_favorite = in_attention || crate::session::favorites_first();

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
                // Mark pinned project headers with a trailing pin glyph so an empty
                // pinned project reads as deliberate rather than stale. Project view
                // only; the registry lookup is keyed by the header label.
                let pinned = self.group_by == GroupByMode::Project
                    && !crate::session::is_synthetic_project_header(path)
                    && self.is_project_label_pinned(name);
                // Top-level shelf section headers get a leading type glyph so they read
                // as system shelves; project sub-folders under Archived keep the label.
                let section_glyph = if crate::session::is_trash_section_path(path) {
                    Some(ICON_TRASH_SECTION)
                } else if crate::session::is_archived_section_path(path) {
                    Some(ICON_ARCHIVED_SECTION)
                } else {
                    None
                };
                let text = if let Some(glyph) = section_glyph {
                    Cow::Owned(format!("{} {} ({})", glyph, name, session_count))
                } else if pinned {
                    Cow::Owned(format!("{} ({}) {}", name, session_count, ICON_PINNED))
                } else {
                    Cow::Owned(format!("{} ({})", name, session_count))
                };
                let mut style = Style::default().fg(theme.group).bold();
                // Shelf headers and their nested project sub-folders sit below the sort
                // divider, so their placement already reads as shelved; render them like
                // regular folder headers (theme.group + bold) so they read as clickable.
                if archived_at.is_some() {
                    // Archived user groups: italic + dim, still visible.
                    style = style
                        .add_modifier(ratatui::style::Modifier::ITALIC)
                        .add_modifier(ratatui::style::Modifier::DIM);
                }
                (icon, text, style)
            }
            Item::Session { id, .. } => {
                if let Some(inst) = self.get_instance(id) {
                    // Each view mode contributes only the live-state glyph and color for
                    // its own pane; the overlays on top belong to `decorate_row`.
                    let (seed, sunk) =
                        match self.view_mode {
                            ViewMode::Structured => {
                                // Idle sessions decay from `fresh_idle` toward `idle`
                                // over `idle_decay_window`, with a slow `breathe` rattle
                                // inside the window to match the other attention-worthy
                                // states and cue colorblind or monochrome terminals.
                                let idle_age = inst.idle_age();
                                let is_fresh_idle =
                                    matches!(idle_age, Some(age) if age < self.idle_decay_window);
                                // Dormant (idle-reaped, resumable) structured workers get
                                // their own glyph and dim amber, ahead of the raw status.
                                // Unread still wins below: an unseen finished turn is the
                                // more actionable signal, as in the web sidebar. See #2250.
                                let is_shown_dormant = inst.is_shown_dormant();
                                let mut icon = if is_shown_dormant {
                                    ICON_DORMANT
                                } else {
                                    match inst.status {
                                        Status::Running => spinner_running(&inst.created_at),
                                        Status::Waiting => spinner_waiting(&inst.created_at),
                                        Status::Idle if is_fresh_idle => spinner_idle_fresh(
                                            &inst.created_at,
                                            inst.idle_entered_at,
                                        ),
                                        Status::Idle => ICON_IDLE,
                                        Status::Unknown => ICON_UNKNOWN,
                                        Status::Stopped => ICON_STOPPED,
                                        Status::Error => ICON_ERROR,
                                        Status::Starting => spinner_starting(&inst.created_at),
                                        Status::Deleting => ICON_DELETING,
                                        Status::Creating => spinner_starting(&inst.created_at),
                                    }
                                };
                                // Unread paints only on resting rows (Idle/Unknown); a
                                // live status keeps its own color and spinner. Sunk rows
                                // (archived/snoozed) never paint it, since the user
                                // dismissed them; the flag stays on disk and comes back on
                                // restore. Snooze is checked in every sort mode here, not
                                // just Attention, so a snoozed unread row still drops the
                                // dot (#2571).
                                let unread_resting = crate::session::unread_enabled()
                                    && inst.is_unread()
                                    && !inst.is_archived()
                                    && !inst.is_snoozed()
                                    && matches!(inst.status, Status::Idle | Status::Unknown);
                                let color = if is_shown_dormant && !unread_resting {
                                    theme.dormant()
                                } else {
                                    match inst.status {
                                        Status::Running => theme.running,
                                        Status::Waiting => theme.waiting,
                                        Status::Idle if unread_resting => theme.unread,
                                        Status::Idle => theme
                                            .idle_color_at_age(idle_age, self.idle_decay_window),
                                        Status::Unknown if unread_resting => theme.unread,
                                        Status::Unknown => theme.waiting,
                                        Status::Stopped => theme.dimmed,
                                        Status::Error => theme.error,
                                        Status::Starting => theme.dimmed,
                                        Status::Deleting => theme.waiting,
                                        Status::Creating => theme.accent,
                                    }
                                };
                                let mut modifier = ratatui::style::Modifier::empty();
                                if unread_resting {
                                    // Make unread unmistakable: solid dot plus bold on
                                    // top of `theme.unread`; a color swap alone read as
                                    // too subtle (#2088 review).
                                    icon = ICON_UNREAD;
                                    modifier = ratatui::style::Modifier::BOLD;
                                }
                                (
                                    RowSeed {
                                        icon,
                                        color,
                                        modifier,
                                    },
                                    SunkRow::AgentStatus(agent_row_icon(inst)),
                                )
                            }
                            ViewMode::Terminal => {
                                let terminal_mode = self.effective_terminal_mode(id);
                                let terminal_running =
                                    match terminal_mode {
                                        TerminalMode::Container => {
                                            let name = crate::tmux::ContainerTerminalSession::
                                        resolve_name_for_display(&inst.id, &inst.title);
                                            crate::tmux::session_exists_for_display(&name)
                                        }
                                        TerminalMode::Host => {
                                            let name = crate::tmux::TerminalSession::
                                        resolve_name_for_display(&inst.id, &inst.title);
                                            crate::tmux::session_exists_for_display(&name)
                                        }
                                    };
                                // Unread means the agent produced output nobody looked
                                // at, which the paired terminal has no notion of, so
                                // Terminal view never paints the dot.
                                let (icon, color) = if terminal_running {
                                    (spinner_running(&inst.created_at), theme.terminal_active)
                                } else {
                                    (ICON_IDLE, theme.dimmed)
                                };
                                (
                                    RowSeed {
                                        icon,
                                        color,
                                        modifier: ratatui::style::Modifier::empty(),
                                    },
                                    SunkRow::Pane,
                                )
                            }
                            ViewMode::Tool(ref tool_name) => {
                                let tool_session = crate::tmux::ToolSession::for_display(
                                    &inst.id,
                                    &inst.title,
                                    tool_name,
                                );
                                let tool_running = crate::tmux::session_exists_for_display(
                                    tool_session.session_name(),
                                ) && !crate::tmux::pane_dead_for_display(
                                    tool_session.session_name(),
                                );
                                let (icon, color) = if tool_running {
                                    (spinner_running(&inst.created_at), theme.terminal_active)
                                } else {
                                    (ICON_IDLE, theme.dimmed)
                                };
                                (
                                    RowSeed {
                                        icon,
                                        color,
                                        modifier: ratatui::style::Modifier::empty(),
                                    },
                                    SunkRow::Pane,
                                )
                            }
                        };
                    decorate_row(inst, in_attention, show_favorite, seed, sunk, theme)
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
        // A search match highlights with weight only: recoloring to `theme.search` (amber
        // in most themes) turned a running match's spinner amber and read as "waiting"
        // (#3038 follow-up). Bolding the spinner and title keeps status honest.
        let mut icon_style = style;
        let mut text_style = if is_selected {
            selected_row_style(style, theme)
        } else {
            style
        };
        if is_match {
            icon_style = icon_style.add_modifier(ratatui::style::Modifier::BOLD);
            text_style = text_style.add_modifier(ratatui::style::Modifier::BOLD);
        }
        line_spans.push(Span::styled(format!("{} ", icon), icon_style));
        line_spans.push(Span::styled(text.into_owned(), text_style));

        if let Item::Session { id, .. } = item {
            if let Some(inst) = self.get_instance(id) {
                // Config-driven suffix next to the title; it owns the
                // branch/profile/sandbox slot, so `None` means no suffix. Counted into
                // `used_width` so the activity column still right-aligns past it.
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

                // Right edge of the row: an optional terminal-mode badge and the
                // activity column, both pinned to the pane's right edge so the column
                // lines up down the list. The column shows only if the prefix plus the
                // slot and badge fit inside `list_width`; on a narrow pane the row drops
                // the column rather than mangling the title.
                //
                // Idle rows drive off `idle_entered_at`, not `last_accessed_at`, which
                // user interaction bumps and would lie about how long the agent has been
                // stopped.
                //
                // Acp-mode sessions get a badge because Enter opens an info dialog rather
                // than attaching to a pane that doesn't exist; it takes precedence over
                // the container/host badge in Structured view, while Terminal view keeps
                // its own badging since the host terminal still works.
                let badge_text: Option<&'static str> =
                    if inst.is_structured() && self.view_mode != ViewMode::Terminal {
                        // `[structured]` rather than `[web]`: the TUI renders these
                        // sessions natively now, so the badge marks the view.
                        Some(" [structured]")
                    } else if self.view_mode == ViewMode::Terminal && inst.is_sandboxed() {
                        Some(match self.get_terminal_mode(id) {
                            TerminalMode::Container => " [container]",
                            TerminalMode::Host => " [host]",
                        })
                    } else if inst.is_structured() {
                        // Terminal view, non-sandboxed: the container/host badge does
                        // not apply, but structured rows still need marking or Enter
                        // opening the structured view surprises the user.
                        Some(" [structured]")
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
                    // Snoozed rows show remaining sleep time under Attention sort; in
                    // other sorts snooze is invisible and the column falls through to the
                    // normal age. Idle rows show time-since-stop (`idle_entered_at`),
                    // falling back to `last_accessed_at` when it is missing, which would
                    // otherwise lie after an attach or send.
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

    /// Keep the live-send tmux pane sized to the preview's visible output area.
    ///
    /// No-op unless live-send targets `target`: without the gate, viewing the Agent pane
    /// while live on Terminal would resize the terminal pane (the worker is bound to it)
    /// to Agent-view dimensions. Deduped against `live_send_last_resize` (shared, since
    /// one target is live at a time) so it fires only on live entry or a preview resize.
    /// Each `refresh_*_cache_if_needed` calls it with its own target.
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
        let now = Instant::now();
        let resize_failed = self
            .live_send_worker
            .as_ref()
            .is_some_and(live_send::LiveSendWorker::take_resize_failed);
        if live_resize_retry_due(&mut self.live_send_resize_retry_at, resize_failed, now) {
            self.live_send_last_resize = None;
        }
        let next = (width, height);
        if self.live_send_last_resize != Some(next) {
            if let Some(worker) = &self.live_send_worker {
                worker.resize(width, height);
                self.live_send_resize_retry_at = None;
            }
            self.live_send_last_resize = Some(next);
        }
    }

    pub(super) fn is_sidebar_item_selected(&self, item: &Item, index: usize) -> bool {
        // A reload can hide the live row while the cursor falls onto a peer.
        match (&self.live_send, &self.selected_session, item) {
            (Some(_), Some(selected), Item::Session { id, .. }) => id == selected,
            (Some(_), Some(_), _) => false,
            _ => index == self.cursor,
        }
    }

    /// The tmux session name backing the pane the preview shows, from the selection and
    /// view mode (and, for Terminal, the host/container sub-mode). Live-send pins the
    /// pane captured at entry. Drives `sync_preview_capture_worker`.
    pub(super) fn displayed_pane_tmux_name(&self) -> Option<String> {
        if let Some(state) = &self.live_send {
            return Some(state.tmux_name.clone());
        }
        let id = self.selected_session.as_ref()?;
        let inst = self.get_instance(id)?;
        let name = match &self.view_mode {
            ViewMode::Structured => {
                crate::tmux::Session::resolve_name_for_display(&inst.id, &inst.title)
            }
            ViewMode::Terminal => {
                let mode = self.effective_terminal_mode(id);
                match mode {
                    TerminalMode::Host => crate::tmux::TerminalSession::resolve_name_for_display(
                        &inst.id,
                        &inst.title,
                    ),
                    TerminalMode::Container => {
                        crate::tmux::ContainerTerminalSession::resolve_name_for_display(
                            &inst.id,
                            &inst.title,
                        )
                    }
                }
            }
            ViewMode::Tool(tool) => {
                crate::tmux::ToolSession::for_display(&inst.id, &inst.title, tool)
                    .session_name()
                    .to_string()
            }
        };
        Some(name)
    }

    /// Observe worker progress without relying on changed-frame publication. A stalled
    /// worker is replaced only after its tmux operation deadline and grace, so a slow but
    /// legitimate sample is never overlapped.
    fn preview_worker_stalled_at(&mut self, now: std::time::Instant) -> bool {
        let Some(worker) = self.preview_capture_worker.as_ref() else {
            self.preview_worker_pulse = None;
            return false;
        };
        let (stalled, observation) =
            worker_stalled_step(worker.cycles(), self.preview_worker_pulse, now);
        self.preview_worker_pulse = observation;
        stalled
    }
    /// Point the capture worker at `desired` (the displayed pane's tmux session) and
    /// retune its cadence for live-send vs idle. One long-lived worker is spawned lazily
    /// and retargeted in place; an empty target idles it. Idempotent and cheap when the
    /// target is unchanged, so render calls it every frame.
    pub(super) fn sync_preview_capture_worker(&mut self, desired: Option<String>) {
        if desired.is_some() && self.preview_worker_stalled_at(std::time::Instant::now()) {
            self.preview_capture_worker = None;
            self.preview_capture_target = None;
            self.preview_worker_pulse = None;
        }
        // Don't spawn the worker until there's actually something to show.
        if desired.is_none() && self.preview_capture_worker.is_none() {
            self.preview_capture_target = None;
            return;
        }
        if self.preview_capture_worker.is_none() {
            let worker = live_send::LiveCaptureWorker::spawn(self.preview_wake.clone());
            // Shell terminals use capture-pane's authoritative snapshot: their prompt
            // repaint can expose a PROMPT_EOL_MARK to a pipe-pane seed, producing a false
            // frame before the grid reconciles. Slower, but no seed handoff.
            worker.set_vt_enabled(
                self.vt_live_enabled && !matches!(self.view_mode, ViewMode::Terminal),
            );
            worker.set_clipboard_capture_enabled(self.agent_clipboard_forward);
            self.preview_capture_worker = Some(worker);
        }
        if self.preview_capture_target != desired {
            if let Some(worker) = self.preview_capture_worker.as_ref() {
                worker.set_target(desired.clone().unwrap_or_default());
            }
            let terminal_mode = if matches!(&self.view_mode, ViewMode::Terminal) {
                self.selected_session
                    .as_ref()
                    .and_then(|id| self.get_instance(id).map(|inst| (id, inst)))
                    .map(|(id, inst)| {
                        if inst.is_sandboxed() {
                            self.get_terminal_mode(id)
                        } else {
                            TerminalMode::Host
                        }
                    })
                    .unwrap_or(TerminalMode::Host)
            } else {
                TerminalMode::Host
            };
            let cache = match &self.view_mode {
                ViewMode::Structured => &mut self.preview_cache,
                ViewMode::Tool(_) => &mut self.tool_preview_cache,
                ViewMode::Terminal => match terminal_mode {
                    TerminalMode::Container => &mut self.container_terminal_preview_cache,
                    TerminalMode::Host => &mut self.terminal_preview_cache,
                },
            };
            if cache.capture_target.as_deref() != desired.as_deref() {
                *cache = super::PreviewCache::default();
            } else {
                cache.cursor = None;
            }
            self.preview_capture_target = desired;
            // A new target starts a fresh heartbeat window; progress from the
            // previous pane must not mask a stall on this one.
            self.preview_worker_pulse = None;
            // New pane under the pointer: drop the hover dedup cell so a
            // stationary pointer still reports its cell to the new agent.
            self.hover_forward_cell = None;
        }
        // Fast cadence only when the displayed pane is the live-send target; a preview
        // of some other pane stays on the idle interval instead of forking every 25ms.
        let live = self
            .live_send
            .as_ref()
            .is_some_and(|s| self.preview_capture_target.as_deref() == Some(s.tmux_name.as_str()));
        // Terminal / container panes forward empty captures so a cleared shell drops its
        // stale text; agent / tool panes preserve the last-good frame (the #1501 kill
        // switch). The policy follows the displayed pane, not the live-send target.
        let forward_empty = matches!(self.view_mode, ViewMode::Terminal);
        if let Some(worker) = self.preview_capture_worker.as_ref() {
            worker.set_live(live);
            worker.set_forward_empty(forward_empty);
            worker.set_vt_enabled(self.vt_live_enabled && !forward_empty);
            worker.set_clipboard_capture_enabled(self.agent_clipboard_forward);
        }
    }

    /// Apply the capture worker's newest frame to `select`'s cache. The worker is the
    /// only capture source: with nothing new (cold start, retarget, unchanged pane) the
    /// cache keeps its last-good content and this returns without writing, so paint never
    /// forks a capture. The frame is consumed atomically, so content, cursor, line budget
    /// and target generation cannot be split across a retarget.
    fn apply_worker_capture(
        &mut self,
        width: u16,
        height: u16,
        select: fn(&mut Self) -> &mut super::PreviewCache,
    ) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        // Drop a cache documenting a different pane than the one displayed: only worker
        // frames write the cache, so a quiet or dead new target would otherwise keep
        // painting the previous instance's bytes forever.
        {
            let cache = select(self);
            if cache.session_id.as_deref() != Some(id.as_str()) && !cache.content.is_empty() {
                *cache = super::PreviewCache::default();
            }
        }
        let scroll_offset = self.preview_scroll_offset;
        let frozen = self.preview_is_frozen();
        let capture_lines = capture_lines_for(height, scroll_offset);
        // Whether the HELD snapshot covers the current read. Computed before
        // the worker borrow so the cache stays reachable through `select`.
        let visible_rows = self.preview_visible_rows;
        let held_covers = {
            let cache = select(self);
            visible_rows.saturating_add(scroll_offset as usize) <= cache.captured_lines
        };
        let Some(worker) = self.preview_capture_worker.as_ref() else {
            return;
        };
        // Publish the budget before the frozen gate: `capture_lines_for`'s reading-depth
        // branch exists for frozen scrollback reads, so the worker must see it even
        // though no frame applies yet.
        worker.set_capture_lines(capture_lines);
        let Some(frame) = worker.take_latest() else {
            return;
        };
        // A frame captured before the last retarget must never land under the new view
        // (the worker re-checks too; this closes the race from the consumer side).
        if !worker.frame_is_current(&frame) {
            return;
        }
        // Empty-frame policy may change while the worker blocks in tmux, so revalidate
        // on the paint thread: a frame captured outside live-send must not blank an
        // agent/tool pane if live-send began before the capture returned (#1501).
        if frame.content.is_empty() && !worker.should_forward_empty() {
            worker.restore_latest(frame);
            return;
        }
        if frozen {
            // While frozen, a routine fresh frame would shift the held content out from
            // under the reader, so apply only when the held snapshot cannot cover the read
            // and this frame extends coverage, or was captured at the full budget and
            // still falls short (the pane ends there). Anything else goes back into the
            // mailbox: the worker's dedup would never republish it, and it is exactly what
            // the preview must show once unfrozen.
            let incoming_lines = frame.content.lines().count();
            let grows = !held_covers
                && (!scroll_exceeds_cache(incoming_lines, height, scroll_offset)
                    || (frame.budget >= capture_lines && incoming_lines > 0));
            if !grows {
                worker.restore_latest(frame);
                return;
            }
        }
        // All reject/restore paths are complete. Move the owned mailbox frame
        // into the cache without copying its content.
        let frame_budget = frame.budget;
        let content_is_empty = frame.content.is_empty();
        let captured_lines = select(self).store_capture(
            frame.content,
            id,
            frame.target,
            frame.generation,
            (width, height),
            frame.cursor,
        );

        // An empty frame always applies: terminal / container panes forward empties so a
        // cleared shell drops its stale text, and there is no offset to clamp anyway.
        //
        // Otherwise `set_capture_lines` is async, so this frame may carry a capture made
        // under a smaller budget. If it doesn't cover the requested window, skip the clamp
        // (it would snap the preview toward the live edge); the worker republishes at the
        // new budget. An exhausted capture, short because the pane holds no more, is not
        // undersized: apply it so scroll state tracks the real pane.
        if !content_is_empty
            && scroll_exceeds_cache(captured_lines, height, scroll_offset)
            && !capture_is_exhausted(captured_lines, frame_budget)
        {
            return;
        }
        self.preview_scroll_offset =
            clamp_scroll_to_capture(scroll_offset, captured_lines, self.preview_visible_rows);
    }

    /// Whether the preview holds its snapshot instead of following live output; the
    /// decision lives in [`preview_frozen`] so it is unit-tested without a `HomeView`.
    fn preview_is_frozen(&self) -> bool {
        preview_frozen(self.preview_scroll_offset, self.preview_selection.is_some())
    }

    /// Adopt passive-resize completions into the per-session bookkeeping. Applied
    /// geometry becomes the synced dedup (and clears the live-send dedup when the resize
    /// raced live entry, see `passive_resize_invalidates_live_geometry`); declined
    /// geometry is parked until the wanted geometry changes.
    fn adopt_passive_resize_completions(&mut self) {
        for done in crate::tmux::take_passive_resize_dones() {
            self.passive_pane_queued.remove(&done.session_id);
            let Some(window_rows) = done.applied_window_rows else {
                self.passive_pane_declined.insert(
                    done.session_id,
                    ((done.cols, done.rows), std::time::Instant::now()),
                );
                continue;
            };
            let invalidates_live = passive_resize_invalidates_live_geometry(
                self.live_send.as_ref().map(|live| &live.target),
                self.selected_session.as_deref(),
                &done.session_id,
            );
            self.passive_pane_declined.remove(&done.session_id);
            self.passive_pane_synced.insert(
                done.session_id.clone(),
                super::PassiveSynced {
                    cols: done.cols,
                    rows: done.rows,
                    window_rows,
                    adopted_at: std::time::Instant::now(),
                },
            );
            if self.preview_pane_pending.as_ref() == Some(&(done.session_id, done.cols, done.rows))
            {
                self.preview_pane_pending = None;
            }
            if invalidates_live {
                self.live_send_last_resize = None;
                self.live_send_resize_retry_at = None;
            }
        }
    }

    /// Drop synced entries contradicted by a newer pane snapshot: an external attach or
    /// the web live view resized the window after we set it, and trusting the entry would
    /// leave the preview clipped with no self-recovery.
    fn invalidate_externally_resized_panes(&mut self) {
        let stale: Vec<String> = self
            .passive_pane_synced
            .iter()
            .filter(|(id, synced)| {
                let Some(inst) = self.get_instance(id) else {
                    return false;
                };
                let name = crate::tmux::Session::resolve_name_for_display(id, &inst.title);
                match crate::tmux::observed_window_size_from_cache(&name) {
                    Some((observed, at)) => passive_synced_contradicted(synced, observed, at),
                    None => false,
                }
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.passive_pane_synced.remove(&id);
        }
    }

    /// Keep every open session's detached agent pane pre-sized to the preview rect it
    /// would be shown at, so selecting a row or entering live view lands on an already
    /// correct pane. `exclude` is the session whose per-frame sync owns its geometry;
    /// the live-send session is skipped because its worker owns the pane.
    ///
    /// The per-session target is `PreviewLayout::compute` over the shared preview rect and
    /// that instance's header height, the same split the renderer uses. Work runs on the
    /// passive-resize worker under its detached/no-owner guard. Two debounces bound the
    /// SIGWINCH cost: two consecutive refreshes must want the same fleet geometry, and a
    /// declined geometry waits for a change or [`PASSIVE_DECLINE_RETRY`].
    ///
    /// Single-TUI only: with two TUIs alive each would read the other's fleet resizes as
    /// external and re-assert its own, oscillating every pane. The presence count behind
    /// the "N watching" indicator gates the fleet pass and the invalidation; the
    /// selected-session sync stays on.
    pub(super) fn reconcile_passive_fleet(
        &mut self,
        inner: Rect,
        compact: bool,
        exclude: Option<&str>,
    ) {
        self.adopt_passive_resize_completions();
        if self.active_tui_count > 1 {
            return;
        }
        self.invalidate_externally_resized_panes();
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        // The excluded and live sessions are skipped at the firing loop, not while
        // building `wants`: the armed epoch key must be pure geometry, or moving the
        // selection would re-arm the fleet and retry every declined session.
        let mut wants: Vec<(String, u16, u16)> = Vec::new();
        for (id, inst) in &self.instances {
            if inst.is_archived() || inst.is_trashed() || inst.is_structured() {
                continue;
            }
            if !matches!(
                inst.status,
                Status::Running | Status::Waiting | Status::Idle
            ) {
                continue;
            }
            let output = preview::PreviewLayout::compute(
                inner,
                compact,
                self.show_preview_info,
                preview::agent_info_height(inst),
            )
            .output;
            if output.width == 0 || output.height == 0 {
                continue;
            }
            wants.push((id.clone(), output.width, output.height));
        }
        // Order-independent epoch key: `self.instances` is rebuilt from a HashMap on
        // reload, so an order-only shuffle must not read as new geometry and clear the
        // declines early. Sorting also makes the firing order deterministic.
        wants.sort_unstable();
        if self.passive_fleet_armed.as_ref() != Some(&wants) {
            // First sighting of this fleet geometry: arm it, let declined sessions retry
            // under the new epoch, and nudge the event loop so the confirming refresh
            // isn't left to an idle heartbeat. Queued entries drop too: the worker's
            // tickets suppress re-queueing work that is truly in flight, while an entry
            // orphaned by a lost completion would otherwise pin its session there.
            self.passive_fleet_armed = Some(wants);
            self.passive_pane_declined.clear();
            self.passive_pane_queued.clear();
            // Prune synced entries for sessions that no longer exist so the
            // map cannot grow without bound as sessions come and go.
            let instances = &self.instances;
            self.passive_pane_synced
                .retain(|id, _| instances.contains_key(id));
            self.preview_wake.notify_one();
            return;
        }
        let live_session = self.live_send.as_ref().map(|live| live.session_id.clone());
        let selected = self.selected_session.clone();
        for (id, cols, rows) in wants {
            if Some(id.as_str()) == exclude || Some(id.as_str()) == live_session.as_deref() {
                continue;
            }
            let want = (cols, rows);
            let synced = self.passive_pane_synced.get(&id).map(|s| (s.cols, s.rows));
            // An expired decline reads as absent so the session is retried
            // once its blocking attach or size owner may have gone away.
            let declined = self
                .passive_pane_declined
                .get(&id)
                .filter(|(_, at)| at.elapsed() < PASSIVE_DECLINE_RETRY)
                .map(|(geometry, _)| *geometry);
            match fleet_passive_step(
                want,
                synced,
                declined,
                self.passive_pane_queued.get(&id).copied(),
            ) {
                FleetPassiveStep::Skip => {}
                FleetPassiveStep::Queue => {
                    let Some(inst) = self.get_instance(&id) else {
                        continue;
                    };
                    crate::tmux::queue_passive_resize(crate::tmux::PassiveResizeIntent {
                        session_id: id.clone(),
                        session_name: crate::tmux::Session::resolve_name_for_display(
                            &id,
                            &inst.title,
                        ),
                        cols,
                        rows,
                        // The viewed session jumps the queue (selected here in
                        // Terminal/Tool view; Structured goes through
                        // `refresh_preview_cache_if_needed`).
                        priority: selected.as_deref() == Some(id.as_str()),
                    });
                    self.passive_pane_queued.insert(id, want);
                }
            }
        }
    }

    pub(super) fn refresh_preview_cache_if_needed(&mut self, width: u16, height: u16) {
        // Forward an agent's OSC 52 copy to the host clipboard (#2420): the VT reader
        // extracts it from the raw pane stream, the capture worker relays it here, and
        // `copy_to_clipboard` delivers it as drag-select copies do. Applied on the render
        // thread so the re-emitted escape can't interleave with a frame flush. Drained
        // unconditionally but gated at the forward, so a copy arriving while the setting
        // is off is discarded rather than parked to clobber the clipboard later.
        if let Some(text) = self
            .preview_capture_worker
            .as_ref()
            .and_then(|worker| worker.take_agent_clipboard())
        {
            if self.agent_clipboard_forward {
                crate::tui::clipboard::copy_to_clipboard(&text);
            }
        }
        // LiveCaptureWorker is the only capture source and runs off paint, so the
        // preview_apply_us metric covers moving the newest frame into the cache, not tmux
        // capture latency.
        let in_live = self.live_send.is_some();
        // Passive completions were adopted by `reconcile_passive_fleet` earlier this
        // frame, so the live-dedup invalidation for a resize that raced live entry is
        // already in place for the sizing below.
        self.resize_live_pane_if_target(live_send::LiveSendTarget::Agent, width, height);
        // Outside live-send nothing keeps the agent's pane sized to the preview: a
        // full-screen agent stays at the size of whatever terminal it was last attached
        // from, so the bottom-anchored capture clips its top rows. Resize the detached
        // pane to the output geometry for a WYSIWYG preview, deduped per (session, w, h)
        // so the 250ms poll doesn't SIGWINCH-storm the agent. The dedup is invalidated on
        // attach and on live enter/exit, where the real window size changes.
        if !in_live && width > 0 && height > 0 {
            if let Some(id) = self.selected_session.clone() {
                let want = (id, width, height);
                let synced = self
                    .passive_pane_synced
                    .get(&want.0)
                    .map(|s| (want.0.clone(), s.cols, s.rows));
                match passive_resize_step(
                    &want,
                    synced.as_ref(),
                    self.preview_pane_pending.as_ref(),
                ) {
                    PassiveResizeStep::InSync => {
                        crate::tmux::cancel_pending_passive_resize(&want.0);
                        self.passive_pane_queued.remove(&want.0);
                        self.preview_pane_pending = None;
                    }
                    PassiveResizeStep::Arm => {
                        self.preview_pane_pending = Some(want);
                        // Nudge the event loop so the confirming refresh isn't left to
                        // the next natural wake: an idle home view can go ~5s between
                        // draws, and the debounce would hold a real resize that long. On
                        // the nudged frame a genuine change fires and a one-frame toast
                        // transient lands back InSync without touching tmux.
                        self.preview_wake.notify_one();
                    }
                    PassiveResizeStep::Fire => {
                        // The tmux work runs on the resize worker, never paint, and
                        // re-runs the attach, size-owner and existence gates. The pending
                        // slot stays armed until completion adopts the dedup, so a session
                        // that does not exist yet retries once started.
                        if let Some(inst) = self.get_instance(&want.0) {
                            crate::tmux::queue_passive_resize(crate::tmux::PassiveResizeIntent {
                                session_id: want.0.clone(),
                                session_name: crate::tmux::Session::resolve_name_for_display(
                                    &want.0,
                                    &inst.title,
                                ),
                                cols: want.1,
                                rows: want.2,
                                // The user is viewing this session; its
                                // resize goes ahead of queued fleet work.
                                priority: true,
                            });
                            self.passive_pane_queued.insert(want.0, (want.1, want.2));
                        }
                    }
                }
            }
        }

        // The capture worker is the only capture source; with nothing new the cache keeps
        // its last-good content and the frame shows that or the empty state.
        self.apply_worker_capture(width, height, |s| &mut s.preview_cache);
    }

    /// Refresh terminal preview cache if needed (for host terminals)
    pub(super) fn refresh_terminal_preview_cache_if_needed(&mut self, width: u16, height: u16) {
        // Symmetric with `refresh_preview_cache_if_needed`: with live-send on the
        // host-terminal pane, keep it sized to the visible output area so a resize or
        // header toggle reflows the shell instead of waiting for a live re-enter.
        self.resize_live_pane_if_target(live_send::LiveSendTarget::Terminal, width, height);
        // Worker-only: no synchronous fallback. A cold or unchanged worker
        // leaves the last-good cache content on screen.
        self.apply_worker_capture(width, height, |s| &mut s.terminal_preview_cache);
    }

    /// Refresh container terminal preview cache if needed
    fn refresh_container_terminal_preview_cache_if_needed(&mut self, width: u16, height: u16) {
        // Same as the host-terminal path, for the in-container shell.
        self.resize_live_pane_if_target(
            live_send::LiveSendTarget::ContainerTerminal,
            width,
            height,
        );
        self.apply_worker_capture(width, height, |s| &mut s.container_terminal_preview_cache);
    }

    pub(super) fn refresh_tool_preview_cache_if_needed(
        &mut self,
        width: u16,
        height: u16,
        tool_name: &str,
    ) {
        // Same as the terminal paths, for this tool pane (lazygit, yazi, etc.).
        self.resize_live_pane_if_target(
            live_send::LiveSendTarget::Tool(tool_name.to_string()),
            width,
            height,
        );
        self.apply_worker_capture(width, height, |s| &mut s.tool_preview_cache);
    }

    /// Record the output pane's text layout for the drag-select handlers. `total_lines`
    /// is the parsed scrollback length; `first_line` comes from the same `compute_scroll`
    /// the renderer feeds to `Paragraph::scroll`, so the snapshot agrees cell-for-cell
    /// with what was painted this frame.
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

    /// The preview cache backing whatever the pane shows, resolving the sandbox
    /// container-vs-host split for Terminal view. Shared by the scroll clamp, the scroll
    /// indicator and the drag-select copy so they read what the renderer painted.
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

    pub(super) fn active_preview_cursor(&self) -> Option<crate::tmux::PaneCursor> {
        let cache = self.active_preview_cache();
        let target = cache.capture_target.as_deref()?;
        if self.preview_capture_target.as_deref() != Some(target) {
            return None;
        }
        let worker = self.preview_capture_worker.as_ref()?;
        worker
            .capture_identity_is_current(target, cache.capture_generation)
            .then_some(cache.cursor)
            .flatten()
    }
    fn active_captured_lines(&self) -> usize {
        self.active_preview_cache().captured_lines
    }

    fn render_preview(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, layout: PaneLayout) {
        if self.system_health_open {
            self.preview_outer_area = area;
            self.preview_area = area;
            self.preview_pane_area = area;
            self.preview_visible_rows = area.height as usize;
            self.sync_preview_capture_worker(None);
            crate::tui::components::diagnostics::render_system_health(
                frame,
                area,
                theme,
                &self.metrics,
                self.system_health_scroll,
            );
            return;
        }
        let compact = area.width < responsive::STACKED_BREAKPOINT;
        let (border_color, title_color) = match self.view_mode {
            ViewMode::Structured => (theme.border, theme.title),
            ViewMode::Terminal | ViewMode::Tool(_) => {
                (theme.terminal_border, theme.terminal_border)
            }
        };
        // Live-send swaps the preview border and title to `accent` so the pane matches
        // the compose modal: otherwise the only tell that keystrokes go to the agent is
        // the status banner, which scrolls off in compact layouts. The title is
        // overridden too, so entering live mode from Terminal/Tool views (where
        // `title_color` is `terminal_border`) stays consistent.
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
                    // An archived/trashed row is parked and its body renders the
                    // placeholder, so force the compact title icon to the stopped glyph:
                    // a stale pre-poll status could otherwise show a live spinner.
                    // Error/Deleting are live delete-op states and keep their icon.
                    let (icon, icon_color) = if (inst.is_archived() || inst.is_trashed())
                        && !matches!(inst.status, Status::Error | Status::Deleting)
                    {
                        (ICON_STOPPED, theme.dimmed)
                    } else if inst.is_shown_dormant() {
                        // Dormant (idle-reaped, resumable) structured worker;
                        // distinct glyph + dim amber. See #2250.
                        (ICON_DORMANT, theme.dormant())
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

            // Advertise the info-header toggle: `i` gates the header in every view mode
            // (Agent's worktree-flavored one, Terminal/Tool's minimal one), so the hint
            // applies everywhere except the compact branch, whose title is taken.
            let key = if self.strict_hotkeys { "I" } else { "i" };
            let hint_text = if self.show_preview_info {
                format!(" hide info with {key} ")
            } else {
                format!(" show info with {key} ")
            };
            let hint_style = Style::default().fg(theme.dimmed).italic();

            // With the info section hidden the inner ` Output ` banner that usually
            // carries the scroll indicator is gone, so surface the indicator here. The
            // output paragraph then claims the full inner, so the visible height is
            // `inner_height`, matching `PreviewLayout::compute(..).output.height`. A
            // mounted structured preview owns its own scroll state, and the generic
            // indicator reads the tmux cache, which is stale or empty for it.
            let structured_mounted = self.structured_preview.is_some();
            let scroll_indicator = if !self.show_preview_info && !structured_mounted {
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
        // `area` is the outer preview rect. Stash it so `App::draw_preview_only` can call
        // back with the right rect on `%output` wakes; the inner would draw a nested
        // block.
        self.preview_outer_area = area;
        self.diff_area = Rect::default();
        // The agent-pane sub-rect of `inner`: the full inner when the info header is
        // hidden or the layout is compact, otherwise inner shifted past the info section,
        // the same split `Preview::render_with_cache` makes and what live mode sizes the
        // tmux pane to. The Agent branch refines this once it resolves the instance.
        self.preview_pane_area = inner;
        // Track the rows the output body paints into, shared with the scroll clamp and
        // the live banner so their math matches the renderer. Each view branch refines it
        // to its real `pane_area.height`; this seed serves the no-output paths.
        self.preview_visible_rows = inner.height as usize;
        // Seed the text-view snapshot inert: the output branches refine it once they know
        // their pane rect and line count, and the no-scrollback paths leave it here so a
        // drag-select over them does nothing.
        self.preview_text_view = crate::tui::home::PreviewTextView::default();
        frame.render_widget(block, area);

        // An archived session's pane was killed, so short-circuit every view mode to the
        // calm placeholder instead of forking captures that come back empty and surface
        // as "No output available".
        let live_send_active = self.live_send.is_some();
        let selected_archived = !live_send_active
            && self
                .selected_session
                .as_ref()
                .and_then(|id| self.get_instance(id))
                .is_some_and(|inst| inst.is_archived());

        // A pane that is simply gone, with no diagnostic detail, carries the generic
        // gone-error; present it as a calm "Stopped" placeholder, while a real crash
        // keeps its specific red message. Covers a just-unarchived row.
        //
        // Structured view only: the gone-error is about the agent pane, and Tool /
        // Terminal views show independently live panes the placeholder must not hide.
        // A trashed session's pane was killed too, with a restore hint.
        let selected_trashed = !live_send_active
            && self
                .selected_session
                .as_ref()
                .and_then(|id| self.get_instance(id))
                .is_some_and(|inst| inst.is_trashed());

        let selected_stopped = !live_send_active
            && !selected_archived
            && !selected_trashed
            && matches!(self.view_mode, ViewMode::Structured)
            && self
                .selected_session
                .as_ref()
                .and_then(|id| self.get_instance(id))
                .is_some_and(|inst| {
                    inst.last_error.as_deref() == Some(crate::session::TMUX_SESSION_GONE_ERROR)
                });

        // A structured (ACP) session has no agent tmux pane; its transcript lives in the
        // `aoe serve` daemon, so capturing the generated pane name would show an empty
        // ` Output ` forever. Structured view only: Terminal and Tool show live panes.
        let selected_structured = !live_send_active
            && !selected_archived
            && !selected_trashed
            && matches!(self.view_mode, ViewMode::Structured)
            && self
                .selected_session
                .as_ref()
                .and_then(|id| self.get_instance(id))
                .is_some_and(|inst| inst.is_structured() && inst.status != Status::Creating);

        // Keep the capture worker pointed at whatever pane this view shows, and tuned to
        // the live-send cadence, before any refresh reads it. Done once here so the
        // creating / no-selection / archived / stopped paths also retarget or idle it.
        let desired =
            if selected_archived || selected_trashed || selected_stopped || selected_structured {
                None
            } else {
                self.displayed_pane_tmux_name()
            };
        self.sync_preview_capture_worker(desired);

        // Pre-size every other open session's detached pane to the rect it would be shown
        // at (and adopt worker completions), in every view mode. The selected session is
        // excluded exactly when the Structured branch runs its own per-frame sync.
        let selected_owns_sync = matches!(self.view_mode, ViewMode::Structured)
            && !selected_archived
            && !selected_trashed
            && !selected_stopped
            && !selected_structured;
        let fleet_exclude = if selected_owns_sync {
            self.selected_session.clone()
        } else {
            None
        };
        self.reconcile_passive_fleet(inner, compact, fleet_exclude.as_deref());

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

        if selected_structured {
            // A mounted structured preview is first-class preview content: info header
            // on top (same `i` toggle as the terminal previews), streaming transcript
            // below, drag-select pointed at the painted rows.
            let selected_id = self.selected_session.clone();
            let mounted_matches = self
                .structured_preview
                .as_ref()
                .zip(selected_id.as_deref())
                .is_some_and(|(v, id)| v.session_id() == id);
            if mounted_matches {
                // Take/put-back so the view's `&mut` render can't
                // fight the instance lookup's shared borrow of self.
                let mut view = self.structured_preview.take();
                let inst = selected_id.as_deref().and_then(|id| self.get_instance(id));
                let layout = preview::PreviewLayout::compute(
                    inner,
                    compact,
                    self.show_preview_info,
                    inst.map(preview::agent_info_height).unwrap_or(0),
                );
                if let (Some(info_area), Some(inst)) = (layout.info, inst) {
                    preview::Preview::render_info(
                        frame,
                        info_area,
                        inst,
                        theme,
                        self.idle_decay_window,
                    );
                }
                // No ` Output ` banner row: the transcript block has its own titled
                // border, so the banner slot stays a blank separator.
                let geometry = view
                    .as_mut()
                    .and_then(|v| v.render(frame, layout.output, theme));
                self.structured_preview = view;
                self.preview_pane_area = layout.output;
                if let Some(g) = geometry {
                    self.preview_visible_rows = g.text_area.height as usize;
                    self.preview_text_view = crate::tui::home::PreviewTextView {
                        pane: g.text_area,
                        first_line: g.first_line,
                        total_lines: g.total_lines,
                    };
                }
                self.paint_preview_selection(frame, theme);
                return;
            }
            if self.structured_preview_pending {
                // A mount is underway: render a quiet beat instead of the wordy "press
                // Enter" page, which would flash on every selection.
                let para = Paragraph::new(Line::from(Span::styled(
                    "…",
                    Style::default().fg(theme.dimmed),
                )))
                .alignment(Alignment::Center);
                frame.render_widget(para, inner);
                return;
            }
            self.render_structured_preview(frame, inner, theme);
            self.paint_preview_selection(frame, theme);
            return;
        }

        match self.view_mode {
            ViewMode::Structured => {
                // Check if selected session is being created (show hook progress)
                let is_creating = !live_send_active
                    && self
                        .selected_session
                        .as_ref()
                        .and_then(|id| self.get_instance(id))
                        .is_some_and(|inst| inst.status == Status::Creating);

                if is_creating {
                    self.render_creating_preview(frame, inner, theme);
                } else {
                    // Size the tmux pane and cache to the same output rect the renderer
                    // paints into, through the one `PreviewLayout::compute` that
                    // `render_with_cache` uses: `layout.output` already accounts for the
                    // info header and banner row, so there is no second subtraction and
                    // no parallel split to drift.
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
                    // Refresh the raw `content` cache and the parsed `Text<'static>`
                    // under `&mut self.preview_cache`, so the shared borrows of
                    // `parsed_text` and `get_instance` can coexist in the render call.
                    let cap_start = Instant::now();
                    self.refresh_preview_cache_if_needed(pane_area.width, pane_area.height);
                    self.preview_timings.apply = cap_start.elapsed();
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
                                CachedPreview::new(
                                    self.preview_cache.parsed_text.as_ref(),
                                    self.preview_cache.is_pending_for(id),
                                ),
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
                    let terminal_mode = self.effective_terminal_mode(&id);

                    // Same single-source split as the Agent branch: the tmux pane is
                    // sized to `PreviewLayout::compute(..).output`, which
                    // `render_terminal_preview` paints into. Sizing to `inner.height`
                    // instead clipped the top of the shell on every frame whenever the
                    // info header was visible.
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

                    // Refresh the matching cache and warm its `parsed_text`, so the
                    // render call can read it alongside `get_instance`.
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
                        // Snapshot-backed like the list rows: a per-name `has-session`
                        // here would be the only fork left in a steady-state frame.
                        let (terminal_running, cache) =
                            match terminal_mode {
                                TerminalMode::Container => {
                                    let name = crate::tmux::ContainerTerminalSession::
                                    resolve_name_for_display(&inst.id, &inst.title);
                                    (
                                        crate::tmux::session_exists_for_display(&name),
                                        &self.container_terminal_preview_cache,
                                    )
                                }
                                TerminalMode::Host => {
                                    let name =
                                        crate::tmux::TerminalSession::resolve_name_for_display(
                                            &inst.id,
                                            &inst.title,
                                        );
                                    (
                                        crate::tmux::session_exists_for_display(&name),
                                        &self.terminal_preview_cache,
                                    )
                                }
                            };

                        Preview::render_terminal_preview(
                            frame,
                            inner,
                            inst,
                            terminal_running,
                            CachedPreview::new(
                                cache.parsed_text.as_ref(),
                                cache.is_pending_for(&id),
                            ),
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
                    // Same single-source split as the Agent branch: the pane is sized to
                    // `PreviewLayout::compute(..).output`, which the renderer paints into.
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
                        let tool_session = crate::tmux::ToolSession::for_display(
                            &inst.id,
                            &inst.title,
                            &tool_name,
                        );
                        // Snapshot-backed for the same reason as the rows: this pair was
                        // the last per-frame fork on the render thread in Tool view.
                        let tool_running =
                            crate::tmux::session_exists_for_display(tool_session.session_name())
                                && !crate::tmux::pane_dead_for_display(tool_session.session_name());

                        Preview::render_terminal_preview(
                            frame,
                            inner,
                            inst,
                            tool_running,
                            CachedPreview::new(
                                self.tool_preview_cache.parsed_text.as_ref(),
                                self.tool_preview_cache.is_pending_for(&id),
                            ),
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

        // In live-send, place a real terminal cursor at the target pane's cursor cell:
        // `capture-pane` carries cell text and SGR color but no cursor, so without this
        // the preview shows none for programs that rely on the hardware cursor. Programs
        // that paint their own caret clear `cursor_flag`, so nothing is painted over them
        // and there is no double cursor.
        if let Some(pos) = self.live_preview_cursor_pos() {
            frame.set_cursor_position(pos);
        }

        // Hyperlink underlines go under the selection highlight: a link inside
        // a drag should still read as selected.
        self.paint_preview_links(frame.buffer_mut());

        // Selection highlight goes last so it sits on top of whatever the view mode
        // painted. `preview_selection` is populated only during a live drag or a
        // finalized highlight, so this is otherwise a no-op.
        self.paint_preview_selection(frame, theme);
    }

    /// Underline the hyperlink spans on the visible preview rows: the host terminal
    /// never sees the OSC 8 (the grid drops it) and has no URL to match, so without the
    /// underline a link whose text is not itself a URL is indistinguishable from the
    /// output around it. It is the affordance for `preview_link_at`.
    pub(super) fn paint_preview_links(&self, buf: &mut Buffer) {
        // Same guard as `preview_link_at`: an overlay swallows the click, so underlining
        // behind it would advertise nothing, and the dialog paints over these cells,
        // leaving the backend to wrap its own text in OSC 8.
        if self.has_non_live_send_overlay() {
            return;
        }
        let view = self.preview_text_view;
        let pane = view.pane;
        if pane.width == 0 || pane.height == 0 {
            return;
        }
        let cache = self.active_preview_cache();
        let Some(text) = cache.parsed_text.as_ref() else {
            return;
        };
        let buf_area = buf.area;
        let mut cells = Some(
            self.hyperlink_cells
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        for row_offset in 0..pane.height {
            let Some(line) = text.lines.get(view.first_line + row_offset as usize) else {
                break;
            };
            for span in crate::tui::links::link_spans_for_line(line, pane.width, &cache.links) {
                for col in span.start..span.end {
                    let pos = (pane.x + col, pane.y + row_offset);
                    if !buf_area.contains(Position::from(pos)) {
                        continue;
                    }
                    buf[pos].modifier |= Modifier::UNDERLINED;
                    // The URI lives outside the `Buffer`, so ratatui's diff cannot see a
                    // target change on otherwise identical cells: the same label repointed
                    // would leave the terminal holding the old target. Hand linked cells
                    // to the backend every frame instead.
                    buf[pos].set_diff_option(ratatui::buffer::CellDiffOption::AlwaysUpdate);
                    // Hand the target to the backend too, so the host terminal gets a
                    // real hyperlink rather than an underline it cannot act on.
                    if let Some(cells) = cells.as_mut() {
                        cells.insert(pos.0, pos.1, &span.uri);
                    }
                }
            }
        }
    }

    /// Where to paint the live-send cursor this frame, mapping the agent pane's
    /// `(cursor_x, cursor_y)` onto the preview's output rect, or `None` for no cursor.
    ///
    /// Only while live-send is active and the preview sits at the live tail
    /// (`preview_scroll_offset == 0`), since over scrolled-back history the cursor would
    /// land on the wrong row. The worker publishes a cursor for every previewed pane, so
    /// this also gates on `position_reliable`, false while the pane scrolled mid-capture.
    fn live_preview_cursor_pos(&self) -> Option<Position> {
        if self.live_send.is_none() || self.preview_scroll_offset != 0 {
            return None;
        }
        let cursor = self.active_preview_cursor()?;
        if !cursor.position_reliable {
            return None;
        }
        // `total_lines` is the parsed line count of the capture painted this frame (set
        // by `set_preview_text_view` just above), so the cursor anchors as the text did.
        map_live_preview_cursor(
            self.preview_pane_area,
            self.preview_visible_rows,
            self.preview_text_view.total_lines,
            cursor,
        )
    }

    /// Apply the drag-select highlight to cells inside the preview pane, reversing
    /// bg/fg for contrast against arbitrary agent output like a terminal's own selection.
    /// Walks the frame buffer rather than re-rendering, so the preview keeps its existing
    /// styles. Cells outside the buffer are skipped: a resize mid-drag can leave a stale
    /// extent off-screen for a frame.
    fn paint_preview_selection(&mut self, frame: &mut Frame, theme: &Theme) {
        let Some(sel) = self.preview_selection else {
            return;
        };
        let view = self.preview_text_view;
        let pane = view.pane;
        // Screen rects for the visible slice: a selection scrolled partly off screen
        // paints only the rows still in view, while the copy spans the full range.
        let segments = sel.screen_flow_rects(view);
        // Capture the selected text only on the first render after a finalized drag;
        // later renders just keep painting. The copy comes from the parsed scrollback
        // cache, so it includes lines that scrolled out of view.
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
        // After release the highlight darkens slightly so "finalized + copied" reads
        // differently from an in-progress drag.
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
                    // Force a high-contrast foreground so ANSI-painted bright/dim agent
                    // output stays readable on the new background.
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

    /// Calm placeholder for an archived session: archiving kills the pane, so the
    /// capture path would render "No output available". Points at `z` to bring the row
    /// back.
    fn render_archived_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let inst = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id));
        let title = inst.map(|i| i.title.clone()).unwrap_or_default();

        // A permanent delete in flight is a live operation on this row, so say so; the
        // parked placeholder's unarchive hint would race the purge.
        if inst.is_some_and(|i| i.status == Status::Deleting) {
            self.render_shelf_deleting_preview(frame, area, theme, &title);
            return;
        }

        let key = if self.strict_hotkeys { "Z" } else { "z" };
        let body = if title.is_empty() {
            "This session is parked. Its agent was stopped.".to_string()
        } else {
            format!("\"{}\" is parked. Its agent was stopped.", title)
        };
        render_placeholder(
            frame,
            area,
            theme,
            Placeholder {
                heading: "Archived",
                body,
                inst,
                hint: Some(press_hint(theme, key, " to unarchive it.")),
                wrap: true,
            },
        );
    }

    /// Shared "Deleting" takeover for the archived/trashed placeholders while a
    /// permanent delete runs. No hints: acting on the row would race the purge.
    fn render_shelf_deleting_preview(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        title: &str,
    ) {
        let body = if title.is_empty() {
            "This session is being permanently deleted.".to_string()
        } else {
            format!("\"{}\" is being permanently deleted.", title)
        };
        render_placeholder(
            frame,
            area,
            theme,
            Placeholder {
                heading: "Deleting",
                body,
                inst: None,
                hint: None,
                wrap: true,
            },
        );
    }

    /// Calm placeholder for a trashed session: its agent was stopped but its transcript
    /// and workspace are kept, so it can be restored or purged from here.
    fn render_trashed_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let inst = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id));
        let title = inst.map(|i| i.title.clone()).unwrap_or_default();

        // See `render_archived_preview`: an in-flight permanent delete takes
        // over the placeholder.
        if inst.is_some_and(|i| i.status == Status::Deleting) {
            self.render_shelf_deleting_preview(frame, area, theme, &title);
            return;
        }

        let body = if title.is_empty() {
            "This session is in the trash. Its agent was stopped; its transcript and workspace are kept.".to_string()
        } else {
            format!(
                "\"{}\" is in the trash. Its agent was stopped; its transcript and workspace are kept.",
                title
            )
        };
        let restore_key = if self.strict_hotkeys { "Z" } else { "z" };
        // The permanent-delete keybind routes to a "Cannot delete terminal" dialog in
        // Terminal view, so advertise it only in Structured view. See #2489.
        let hint = if self.view_mode == ViewMode::Terminal {
            press_hint(theme, restore_key, " to restore.")
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
        render_placeholder(
            frame,
            area,
            theme,
            Placeholder {
                heading: "Trash",
                body,
                inst,
                hint: Some(hint),
                wrap: true,
            },
        );
    }

    /// Calm placeholder for a pane that is simply gone (the generic gone-error), in
    /// place of the red crash error; the row's status icon still signals the state.
    fn render_stopped_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        render_placeholder(
            frame,
            area,
            theme,
            Placeholder {
                heading: "Stopped",
                body: "This session isn't running.".to_string(),
                inst: None,
                hint: Some(press_hint(theme, "Enter", " to start it.")),
                wrap: false,
            },
        );
    }

    /// Placeholder for a structured-view session: it has no agent tmux pane to capture
    /// (the transcript lives in the `aoe serve` daemon), so explain how to open the real
    /// view instead of leaving ` Output ` blank.
    fn render_structured_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let inst = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id));
        let title = inst.map(|i| i.title.clone()).unwrap_or_default();
        let name = if title.is_empty() {
            "This session".to_string()
        } else {
            format!("\"{title}\"")
        };
        let body = match inst.and_then(|i| i.agent_name.clone()) {
            Some(agent) => {
                format!("{name} runs {agent} as a structured transcript, not a terminal pane.")
            }
            None => format!("{name} renders as a structured transcript, not a terminal pane."),
        };
        render_placeholder(
            frame,
            area,
            theme,
            Placeholder {
                heading: "Structured view",
                body,
                inst: None,
                hint: Some(press_hint(
                    theme,
                    "Enter",
                    " to open it (offers to start a local `aoe serve` daemon if none is running).",
                )),
                wrap: false,
            },
        );
    }

    fn render_status_bar(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Cleared each frame and set only when the badge is drawn, so a stale rect can't
        // make a footer click open tips while the badge is hidden.
        self.tips_badge_rect = None;
        // A flash is one-shot feedback on something the user just did, so it takes the
        // row for its few seconds and wins over a hovered link's target, which it is
        // usually reporting anyway. The LIVE chip stays either way: which pane keystrokes
        // land on must never be hidden. A hovered target shows only while no overlay
        // covers the preview (the click is inert there) and is resolved per frame, and it
        // is suppressed while the leader is armed so the which-key menu stays visible.
        let hovered = (!self.live_send_pending_leader)
            .then(|| self.hovered_link())
            .flatten()
            .map(|uri| format!("\u{1f517} {uri}"));
        let transient = self.status_flash_text().map(str::to_string).or(hovered);
        if let Some(text) = transient {
            let mut spans = Vec::new();
            let mut budget = area.width as usize;
            // In live-send the chip and the way out both stay: a flash lasts three
            // seconds but a hovered target lasts as long as the pointer rests, and that
            // is too long to leave the user with no visible exit chord.
            let exit = self.live_send.as_ref().map(|state| {
                let chord = if state.exit_chords.is_empty() {
                    "?".to_string()
                } else {
                    live_send::display_chord_list(&state.exit_chords)
                };
                format!(" {chord} to exit ")
            });
            if self.live_send.is_some() {
                let chip = " \u{25CF} LIVE ";
                budget = budget.saturating_sub(unicode_width::UnicodeWidthStr::width(chip));
                spans.push(Span::styled(
                    chip,
                    Style::default()
                        .fg(theme.background)
                        .bg(theme.running)
                        .bold(),
                ));
            }
            if let Some(exit) = exit.as_deref() {
                budget = budget.saturating_sub(unicode_width::UnicodeWidthStr::width(exit));
            }
            let text = truncate_to_width(&format!(" {text} "), budget);
            spans.push(Span::styled(text, Style::default().fg(theme.accent).bold()));
            if let Some(exit) = exit {
                spans.push(Span::styled(exit, Style::default().fg(theme.dimmed)));
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
        // The live-send banner takes over the status bar as an always-visible reminder
        // that keystrokes are relayed, and how to get out. Distinct color and bold so it
        // is not read as the regular footer. The scroll indicator, present only when the
        // user scrolled back, sits between title and exit hint.
        if let Some(state) = &self.live_send {
            let base_title = if state.title.is_empty() {
                "session"
            } else {
                state.title.as_str()
            };
            // Surface which pane keystrokes land on; the shared formatter keeps this in
            // lockstep with the compose dialog's title.
            let raw_title = live_send::format_target_label(base_title, &state.target);
            let chip = " \u{25CF} LIVE \u{2192} ";
            let chip_style = Style::default()
                .fg(theme.background)
                .bg(theme.running)
                .bold();

            // Ctrl+C in live mode is forwarded to the agent, so the footer flashes a
            // reminder for a few seconds that the keystroke landed there (#2894). The
            // scroll indicator and leader hint step aside so the row can't overflow.
            let flash_ctrl_c = self.live_send_ctrl_c_flash_active();

            // The leader is armed, so surface the live-send commands the next key can
            // pick instead of the exit hint.
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

            // Built from the user's configured exit-chord list so the hint always shows
            // what exits live mode for them. An empty list (parse_chord_list falls back
            // to the defaults) renders "?" so the mode never looks inescapable.
            let chord = if state.exit_chords.is_empty() {
                "?".to_string()
            } else {
                live_send::display_chord_list(&state.exit_chords)
            };
            let suffix = " to exit ";
            // Compact reminder that the leader opens the command menu, so the palette
            // and sidebar toggle are discoverable. Empty when the leader is disabled.
            let leader_hint = if flash_ctrl_c {
                String::new()
            } else {
                state
                    .leader
                    .map(|l| format!(" \u{00b7} {} menu", live_send::display_chord(l)))
                    .unwrap_or_default()
            };
            // `preview_visible_rows` is the output-body height the renderer last painted
            // into, so the `[offset/max]` indicator agrees with the scroll math; deriving
            // it from `dimensions` with a fixed `- 1` over-counts whenever the info
            // header is hidden.
            let visible_height = self.preview_visible_rows;
            // Pull `captured_lines` from whichever cache is on screen: the Agent cache
            // would show a stale `[offset/max]` in Terminal/Tool live mode. Hidden while
            // the Ctrl+C flash needs the room.
            let scroll = if flash_ctrl_c {
                String::new()
            } else {
                format_scroll_indicator(
                    self.active_captured_lines(),
                    visible_height,
                    self.preview_scroll_offset,
                )
                .unwrap_or_default()
            };
            // The Ctrl+C reminder sits just before the exit chord so it reads as
            // "Ctrl+C sent to agent · <chord> to exit". Empty outside the flash window.
            let flash = if flash_ctrl_c {
                "Ctrl+C sent to agent \u{00b7} "
            } else {
                ""
            };
            // Spaces between chip, title and chord. The title gets what is left after
            // the fixed pieces, reserved last so the exit chord never falls off.
            let fixed_width = unicode_width::UnicodeWidthStr::width(chip)
                + 1 // single space after the chip
                + 2 // double space before the chord
                + unicode_width::UnicodeWidthStr::width(flash)
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
            if !flash.is_empty() {
                spans.push(Span::styled(
                    flash,
                    Style::default().fg(theme.running).bold(),
                ));
            }
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

        // A committed search borrows bare `n` for match cycling while its bar and
        // `[i/N]` counter are gone, so the mode is otherwise invisible (#3038). Say so.
        let committed_search = !self.search_active && !self.search_matches.is_empty();

        // Priority-tagged shortcut groups: lower priority survives longer when the footer
        // can't fit everything (iPhone Mosh landscape is ~80 cols). Essentials (Nav /
        // Enter / Help / Quit / Serve) survive first; Diff / Search / Mode / Group drop
        // first. Groups render in declaration order, joined with · at render time.
        let mk = |key: &str, desc: &str| -> Vec<Span<'static>> {
            vec![
                Span::styled(format!("{} ", key), key_style),
                Span::styled(desc.to_string(), desc_style),
            ]
        };
        // Key-only entry for keys universal enough that a description is noise (? and /),
        // saving footer width at iPhone-Mosh sizes.
        let mk_key =
            |key: &str| -> Vec<Span<'static>> { vec![Span::styled(key.to_string(), key_style)] };

        // Key a footer button synthesizes on click. The registry matches the bare keycode
        // (Shift implied by an uppercase char, Ctrl by the flag), so a plain char and a
        // Ctrl char cover every footer chord.
        let kc = |c: char| Some(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let kctrl = |c: char| Some(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        let kenter = Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let ktab = Some(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        // (priority, click-key, spans); `click-key` is `None` for the non-actionable
        // status indicators, which render but aren't clickable.
        let mut groups: Vec<(u8, Option<KeyEvent>, Vec<Span<'static>>)> = Vec::new();

        // Serve indicator, shown only while the `aoe serve` daemon is live. The TUI does
        // not own the daemon, so the PID file is probed each render; mode comes from a
        // PID-keyed cache rather than a per-frame file read.
        let mode_label = crate::cli::serve::cached_serve_mode_label();
        if crate::cli::serve::daemon_pid().is_some() {
            // A build without the dashboard bundle answers the API only, so
            // the badge must not read as "the dashboard is up".
            let what = if cfg!(feature = "web") {
                "Serving"
            } else {
                "Serving API"
            };
            let label = match mode_label {
                Some(m) => format!(" \u{25CF} {} ({}) ", what, m),
                None => format!(" \u{25CF} {} ", what),
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

        // Other-TUI indicator, shown when more than one `aoe` TUI is alive: two TUIs
        // watching the same sessions clash over pane sizes (tmux reflows to the smallest
        // client). Recomputed on a throttle in the app loop, not per frame.
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

        // Pending-paste indicator: text captured at the home view that couldn't be routed
        // yet (no runnable session selected). The hint says the paste didn't vanish;
        // `m` after selecting a runnable session drains it into the compose dialog.
        if let Some(buf) = &self.pending_paste {
            if !buf.is_empty() {
                let key = if strict { "M" } else { "m" };
                let desc = format!("send {} buffered", buf.chars().count());
                let mut spans = mk(key, &desc);
                spans[1] = Span::styled(desc, Style::default().fg(theme.running).bold());
                groups.push((0, kc(if strict { 'M' } else { 'm' }), spans));
            }
        }

        // On a session row Enter and Tab are complements: `default_attach_mode` routes
        // Enter to live-send or tmux attach and Tab does the other, so both labels resolve
        // here and can never advertise the same action. Acp rows ignore the setting and
        // keep the plain "Attach" label. Mirrors `HelpOverlay`'s wording.
        let (enter_action_text, tab_action_text) = match self.flat_items.get(self.cursor) {
            Some(Item::Group {
                collapsed: true, ..
            }) => (Some("Expand"), None),
            Some(Item::Group {
                collapsed: false, ..
            }) => (Some("Collapse"), None),
            Some(Item::Session { id, .. }) => {
                if self
                    .get_instance(id)
                    .is_some_and(|inst| inst.is_structured())
                {
                    (Some("Attach"), None)
                } else if matches!(
                    self.default_attach_mode(id),
                    Some(crate::session::AttachMode::LiveSend)
                ) {
                    (Some("Live"), Some("Attach"))
                } else {
                    (Some("Attach"), Some("Live"))
                }
            }
            None => (None, None),
        };
        if let Some(enter_action_text) = enter_action_text {
            // U+21B5 renders Enter in one cell across most fonts, saving 4 cols over the
            // word and matching k9s/lazygit/fzf. The trailing space adds a second visual
            // gap, since the glyph fills its cell tightly.
            groups.push((0, kenter, mk("↵ ", enter_action_text)));
        }
        if let Some(tab_action_text) = tab_action_text {
            groups.push((1, ktab, mk("⇥ ", tab_action_text)));
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

        // New session: bare `n` is the usual chord, but while a committed search borrows
        // `n` for cycling, advertise Shift+N (which still creates, #3038). Strict mode
        // already uses `N`.
        let new_uses_shift = committed_search && !strict;
        groups.push((
            2,
            kc(if strict || new_uses_shift { 'N' } else { 'n' }),
            mk(if strict || new_uses_shift { "N" } else { "n" }, "New"),
        ));

        // Priority 1 is the core daily workflow (message), which survives the greedy pack
        // at ~80 cols; del stays at p3 and can drop first.
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
        // Archive / Snooze render only in Attention sort: they shape the Attention queue
        // and do nothing visible elsewhere, so they would just take footer space.
        let in_attention = self.sort_order == SortOrder::Attention;
        if in_attention {
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
                    kc(if strict { 'H' } else { 'h' }),
                    mk(if strict { "H" } else { "h" }, "Snooze"),
                ));
            }
        }
        // Fav follows the key's own gate (`Context::FavoritesUsable`), so the footer
        // advertises it wherever `f` actually does something.
        if self.selected_session.is_some() && (in_attention || crate::session::favorites_first()) {
            groups.push((
                1,
                kc(if strict { 'F' } else { 'f' }),
                mk(if strict { "F" } else { "f" }, "Fav"),
            ));
        }

        // Committed-search cue: `n` cycles matches and `Esc` clears (#3038). The `[i/N]`
        // counter lives on the search bar, so it isn't duplicated here. Priority 0 so it
        // survives the pack; clicking it cycles to the next match.
        if committed_search {
            let hint = vec![
                Span::styled("n", key_style),
                Span::styled(" next ", desc_style),
                Span::styled("Esc", key_style),
                Span::styled(" clear", desc_style),
            ];
            groups.push((0, kc('n'), hint));
        }

        groups.push((4, kc('/'), mk_key("/")));
        groups.push((
            4,
            if strict { kctrl('d') } else { kc('D') },
            mk(if strict { "^D" } else { "D" }, "Diff"),
        ));
        groups.push((1, kctrl('k'), mk("^K", "Cmds")));
        groups.push((0, kc('?'), mk_key("?")));

        // Greedy pack by priority: a group's width is its span widths, each kept
        // separator adds 3 cols, and 1 col is reserved for the leading margin. Measured in
        // display cells so the pack and the click rects line up with painted cells.
        let widths: Vec<usize> = groups
            .iter()
            .map(|(_, _, g)| g.iter().map(|s| s.width()).sum::<usize>())
            .collect();

        // Tips badge: pinned bottom-right and clickable, its width reserved before the
        // greedy pack so thin terminals drop hints rather than collide with it. Hidden
        // when nothing is unseen or it cannot fit. The footer's own bg is
        // `theme.selection`, so hover uses the brighter `session_selection`.
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
        // Precedence: transient status, app update, then sandbox-image update. Only one
        // banner shows at a time so its keys are unambiguous; a lower-priority banner
        // surfaces once the ones above clear.
        let text = if let Some(s) = status {
            format!(" {s}  [Ctrl+x] dismiss")
        } else if let Some(info) = info {
            // Reassure users (#2220) that updating never tears down running sessions.
            // Kept after the keys so the action hints stay visible on narrow terminals.
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

    #[test]
    fn live_resize_failure_retries_only_after_backoff() {
        let now = Instant::now();
        let mut retry_at = None;

        assert!(!live_resize_retry_due(&mut retry_at, true, now));
        assert_eq!(retry_at, Some(now + LIVE_SEND_RESIZE_RETRY_DELAY));
        assert!(!live_resize_retry_due(
            &mut retry_at,
            false,
            now + LIVE_SEND_RESIZE_RETRY_DELAY - Duration::from_millis(1),
        ));
        assert!(live_resize_retry_due(
            &mut retry_at,
            false,
            now + LIVE_SEND_RESIZE_RETRY_DELAY,
        ));
        assert_eq!(retry_at, None);
    }

    // The preview split geometry is owned by `preview::PreviewLayout`, tested alongside
    // it; the render-side regression is covered by
    // `preview_visible_rows_equal_output_area_with_info_shown` in `home/tests/keys_and_nav.rs`.

    /// A preview worker gets the full shared tmux deadline plus grace before
    /// replacement, and the unchanged-observation timestamp must not slide on each render
    /// or a stalled worker stays trusted forever.
    #[test]
    fn worker_stall_detection_honors_deadline_and_progress() {
        let t0 = std::time::Instant::now();
        let timeout = crate::tmux::TMUX_COMMAND_TIMEOUT.saturating_add(WORKER_STALL_GRACE);
        let before = t0 + timeout - std::time::Duration::from_millis(1);
        let at = t0 + timeout;

        assert_eq!(worker_stalled_step(7, None, t0), (false, Some((7, t0))));
        assert_eq!(
            worker_stalled_step(8, Some((7, t0)), before),
            (false, Some((8, before)))
        );
        assert_eq!(
            worker_stalled_step(7, Some((7, t0)), before),
            (false, Some((7, t0)))
        );
        assert_eq!(
            worker_stalled_step(7, Some((7, t0)), at),
            (true, Some((7, t0)))
        );
    }
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
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        }
    }

    fn geo(id: &str, cols: u16, rows: u16) -> (String, u16, u16) {
        (id.to_string(), cols, rows)
    }

    #[test]
    fn passive_resize_arms_before_firing() {
        // A geometry change resizes only on its second consecutive sighting.
        let synced = geo("a", 141, 43);
        let want = geo("a", 141, 40);
        assert_eq!(
            passive_resize_step(&want, Some(&synced), None),
            PassiveResizeStep::Arm,
        );
        assert_eq!(
            passive_resize_step(&want, Some(&synced), Some(&want)),
            PassiveResizeStep::Fire,
        );
    }

    #[test]
    fn passive_resize_ignores_one_frame_toast_geometry() {
        // The toast frame drops the output rect one row for a single refresh, then
        // returns. Neither the shrink nor the bounce-back may reach tmux: the double
        // SIGWINCH is the cursor jiggle users saw on live-send entry.
        let steady = geo("a", 141, 43);
        let toast = geo("a", 141, 42);
        // Toast frame: shrink is armed, not fired.
        assert_eq!(
            passive_resize_step(&toast, Some(&steady), None),
            PassiveResizeStep::Arm,
        );
        // Post-toast frame: back in sync, and the caller drops the armed
        // geometry so a later real change still needs two sightings.
        assert_eq!(
            passive_resize_step(&steady, Some(&steady), Some(&toast)),
            PassiveResizeStep::InSync,
        );
    }

    #[test]
    fn passive_resize_refires_while_unsynced() {
        // A Fire whose tmux-side resize couldn't happen (session not started, or an
        // active size owner) leaves synced empty and pending armed, so the next refresh
        // fires again instead of re-arming.
        let want = geo("a", 141, 43);
        assert_eq!(
            passive_resize_step(&want, None, Some(&want)),
            PassiveResizeStep::Fire,
        );
    }

    #[test]
    fn passive_resize_session_switch_rearms() {
        // Selecting a different session is a new geometry key: it must go
        // through Arm again rather than firing against the stale pending.
        let pending = geo("a", 141, 43);
        let want = geo("b", 141, 43);
        assert_eq!(
            passive_resize_step(&want, None, Some(&pending)),
            PassiveResizeStep::Arm,
        );
    }

    #[test]
    fn fleet_passive_step_dedups_per_session() {
        let want = (120, 40);
        let other = (100, 30);
        let cases = [
            // Fresh session: hand it to the worker.
            (None, None, None, FleetPassiveStep::Queue),
            // Pane already matches: leave it alone.
            (Some(want), None, None, FleetPassiveStep::Skip),
            // The worker declined this exact geometry (attached, owned, or
            // missing): no retry until the fleet epoch changes.
            (None, Some(want), None, FleetPassiveStep::Skip),
            // Already handed to the worker: wait for its completion.
            (None, None, Some(want), FleetPassiveStep::Skip),
            // A decline for a different geometry does not block the new want.
            (None, Some(other), None, FleetPassiveStep::Queue),
            // A queued different geometry is superseded (the queue keeps only
            // the latest intent per session).
            (Some(other), None, Some(other), FleetPassiveStep::Queue),
        ];
        for (synced, declined, queued, expect) in cases {
            assert_eq!(
                fleet_passive_step(want, synced, declined, queued),
                expect,
                "synced={synced:?} declined={declined:?} queued={queued:?}"
            );
        }
    }

    #[test]
    fn passive_synced_contradiction_requires_a_fresher_mismatch() {
        let adopted_at = Instant::now();
        let synced = crate::tui::home::PassiveSynced {
            cols: 141,
            rows: 43,
            window_rows: 44,
            adopted_at,
        };
        let newer = adopted_at + Duration::from_millis(1);
        let older = adopted_at - Duration::from_millis(1);
        assert!(passive_synced_contradicted(&synced, (200, 50), newer));
        assert!(
            !passive_synced_contradicted(&synced, (141, 44), newer),
            "an observation matching the applied window size is not a contradiction"
        );
        assert!(
            !passive_synced_contradicted(&synced, (200, 50), older),
            "a snapshot that may predate our own resize must not invalidate"
        );
    }

    #[test]
    fn live_cursor_maps_single_and_composited_origins() {
        let output = Rect::new(40, 5, 80, 24);

        // Steady-state single pane: the origin and anchoring delta are zero.
        let single = map_live_preview_cursor(output, 24, 200, pane_cursor(3, 2, true, 24));
        assert_eq!(single, Some(Position::new(43, 7)));

        // A top border row makes the composite one row taller than the visible output and
        // shifts pane 0 down; the anchoring delta clips that row, and pane 0's origin puts
        // the pane-relative cursor back on the text.
        let mut split = pane_cursor(3, 2, true, 25);
        split.composite_pane0 = Some(crate::tmux::PaneGeom {
            left: 1,
            top: 1,
            width: 79,
            height: 24,
        });
        let composited = map_live_preview_cursor(output, 24, 200, split);
        assert_eq!(composited, Some(Position::new(44, 7)));
    }

    #[test]
    fn live_cursor_anchored_to_bottom_when_pane_taller_than_output() {
        // Pane is 24 rows with only 10 visible, so the capture overflows and its bottom
        // 10 pin to the output: a cursor on the last screen row lands on the output's last
        // row, and one in the clipped top maps out and drops.
        let output = Rect::new(0, 0, 80, 10);
        assert_eq!(
            map_live_preview_cursor(output, 10, 100, pane_cursor(0, 23, true, 24)),
            Some(Position::new(0, 9)),
        );
        assert_eq!(
            map_live_preview_cursor(output, 10, 100, pane_cursor(0, 5, true, 24)),
            None,
        );
    }

    #[test]
    fn live_cursor_tracks_top_anchored_short_capture() {
        // #2742: the pane is a row shorter than the output and its capture does not
        // overflow, so the renderer paints from the top and the cursor must anchor there
        // too, not to `visible_rows`, or it paints a row below the typed text.
        let output = Rect::new(0, 0, 80, 24);
        // 23-row alt-screen pane, capture is exactly its 23 lines (no scrollback
        // to overflow the 24-row output). Cursor on the pane's last row (y=22).
        let short = pane_cursor(5, 22, true, 23);
        assert_eq!(
            map_live_preview_cursor(output, 24, 23, short),
            Some(Position::new(5, 22)),
            "top-anchored capture must not drift the cursor down a row",
        );
        // The buggy formula (`visible_rows - pane_height`) would place it at
        // row 23; assert the fix does not.
        assert_ne!(
            map_live_preview_cursor(output, 24, 23, short),
            Some(Position::new(5, 23)),
        );
        // Cursor on the pane's top row lands on the output's top row.
        assert_eq!(
            map_live_preview_cursor(output, 24, 23, pane_cursor(0, 0, true, 23)),
            Some(Position::new(0, 0)),
        );
    }

    #[test]
    fn live_cursor_hidden_or_out_of_bounds_paints_nothing() {
        let output = Rect::new(0, 0, 80, 24);
        // DECTCEM-hidden cursor: nothing to paint.
        assert_eq!(
            map_live_preview_cursor(output, 24, 200, pane_cursor(3, 2, false, 24)),
            None,
        );
        // Column past the output width is dropped rather than clamped.
        assert_eq!(
            map_live_preview_cursor(output, 24, 200, pane_cursor(80, 2, true, 24)),
            None,
        );
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
    fn compose_list_title_group_by_suffix_per_mode() {
        let cases = [
            (GroupByMode::Manual, ""),
            (GroupByMode::Project, " · project"),
            (GroupByMode::Org, " · org"),
        ];
        for (mode, suffix) in cases {
            let title = compose_list_title("aoe", None, mode, SortOrder::Newest);
            assert_eq!(title, format!(" aoe{suffix} "), "{mode:?}");
        }
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
        assert_eq!(profile_short_code("connect_airlines-work"), "caw");
    }

    #[test]
    fn profile_short_code_keeps_short_lead_segment_whole() {
        assert_eq!(profile_short_code("gna-main"), "gnam");
        assert_eq!(profile_short_code("bsc-main"), "bscm");
        assert_eq!(profile_short_code("bso-main"), "bsom");
        assert_eq!(profile_short_code("RAS-Main"), "rasm");
        assert_eq!(profile_short_code("aoe-fiw"), "aoef");
        assert_eq!(profile_short_code("wma-work"), "wmaw");
        assert_eq!(profile_short_code("p9-main"), "p9m");
        assert_eq!(profile_short_code("bp-main"), "bpm");
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

    /// The four-cell cap is enforced after lowercasing and by display width: `İ`
    /// lowercases to two scalars in one cell and `界` is one scalar in two cells.
    #[test]
    fn profile_short_code_caps_by_display_width_after_lowercasing() {
        use unicode_width::UnicodeWidthStr;
        let expanded = profile_short_code("İab-main");
        assert_eq!(expanded, "i\u{307}abm");
        assert_eq!(expanded.width(), 4);
        assert_eq!(profile_short_code("界界界-main"), "界界");
        assert_eq!(profile_short_code("界界界-main").width(), 4);
        assert_eq!(profile_short_code("界-main"), "界m");
        assert_eq!(profile_short_code("x界-main"), "x界m");
        assert_eq!(profile_short_code("界界界界"), "界界");
        for name in ["İİİİ-main", "界界界界-main", "İ界-x-y-z", "ÀÉÎ-main"] {
            assert!(
                profile_short_code(name).width() <= 4,
                "{name:?} -> {:?} exceeds four cells",
                profile_short_code(name)
            );
        }
    }

    /// `UnicodeWidthChar` is not additive across an emoji sequence: `♥` plus VS16
    /// measures 1 + 0 per scalar but 2 as a string, and a skin tone measures 2 alone but 0
    /// once joined. The cap must measure grapheme-aligned prefixes as a string, the way
    /// `RowTag::rendered()` does.
    #[test]
    fn profile_short_code_measures_width_across_grapheme_clusters() {
        use unicode_width::UnicodeWidthStr;
        // Per-scalar sum admits `a` (1 + 0 + 2 + 1 = 4) but the string is 5 cells.
        assert_eq!(
            profile_short_code("\u{2665}\u{fe0f}界-a"),
            "\u{2665}\u{fe0f}界"
        );
        // Per-scalar sum (2 + 2) rejects `m`, yet `🤝🏽m` is 3 cells.
        assert_eq!(
            profile_short_code("\u{1f91d}\u{1f3fd}-main"),
            "\u{1f91d}\u{1f3fd}m"
        );
        // A segment's initial is its first cluster, so the VS16 that keeps
        // the heart in emoji presentation travels with it.
        assert_eq!(
            profile_short_code("x-\u{2665}\u{fe0f}"),
            "x\u{2665}\u{fe0f}"
        );
        for name in [
            "\u{2665}\u{fe0f}\u{2665}\u{fe0f}\u{2665}\u{fe0f}",
            "\u{1f91d}\u{1f3fd}-a-b-c-d",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}-main",
            "क्ष-main",
        ] {
            let code = profile_short_code(name);
            assert!(
                code.width() <= 4,
                "{name:?} -> {code:?} is {} cells",
                code.width()
            );
        }
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
        // Content exactly fills a 40-row banner-less pane, so nothing scrolls back and
        // any offset clamps to 0. Deriving `area_height - 1` internally left a phantom max
        // offset of 1 and stalled live-follow a row early.
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
    fn capture_lines_for_captures_full_scrollback_while_reading() {
        // A non-zero offset within the baseline switches to the wide reading window, so
        // the snapshot spans the whole scrollback and is captured once instead of
        // re-anchoring to the live edge each notch.
        let baseline = 30 + READING_CAPTURE_LINES as usize + CAPTURE_BUFFER as usize;
        assert_eq!(capture_lines_for(30, 1), baseline);
        assert_eq!(capture_lines_for(30, 200), baseline);
        // Past the baseline the window grows with the offset, so a pane whose
        // history-limit exceeds READING_CAPTURE_LINES stays readable to its top.
        let deep = READING_CAPTURE_LINES as usize + 3000;
        assert_eq!(
            capture_lines_for(30, deep as u16),
            30 + deep + CAPTURE_BUFFER as usize
        );
    }

    #[test]
    fn preview_frozen_while_reading_or_selecting() {
        // Live edge, no selection: follow live output.
        assert!(!preview_frozen(0, false));
        // Scrolled off the live edge: hold the snapshot so streaming output
        // can't yank the read position.
        assert!(preview_frozen(1, false));
        // Selection in flight at the live edge: hold so the drag anchors
        // (or a finalized highlight) don't slide off their text.
        assert!(preview_frozen(0, true));
        assert!(preview_frozen(5, true));
    }

    #[test]
    fn capture_lines_for_grows_without_overflow() {
        // usize arithmetic: an extreme offset extends the window past u16
        // without wrapping (u16::MAX height + u16::MAX depth + buffer).
        assert_eq!(
            capture_lines_for(u16::MAX, u16::MAX),
            u16::MAX as usize * 2 + CAPTURE_BUFFER as usize
        );
    }

    #[test]
    fn capture_is_exhausted_only_when_short_and_nonempty() {
        // capture_lines_for(48, 0) = 68, and an alternate-screen agent yields exactly its
        // 48 visible rows: fewer than requested means exhausted, so the live gate must not
        // treat it as stale and fork a capture every frame.
        let requested = capture_lines_for(48, 0);
        assert_eq!(requested, 68);
        assert!(capture_is_exhausted(48, requested));
        // A main-screen pane returns the full requested window: not exhausted.
        assert!(!capture_is_exhausted(68, requested));
        // Cold cache (zero lines) is not an exhausted pane; it must still capture.
        assert!(!capture_is_exhausted(0, requested));
    }

    #[test]
    fn scroll_exceeds_cache_false_when_buffer_covers_small_scroll() {
        // The cache was captured at scroll=0, height=30, so it holds 50 lines. The
        // predicate must trip only when `height + scroll + BUFFER > captured_lines`, so
        // with 60 captured lines small scroll increments force no re-capture.
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

    // -- activity_column_padding ------------------------------------------------------
    //
    // The column lives at `list_width - badge_width - SLOT - MARGIN`; `pad_len` goes
    // between the row prefix and the column, and None hides the column so the title is
    // not clipped.

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
        // The badge fits by itself but the column does not, and the decision is per-row:
        // prefix(20) + slot(6) + badge(12) + margin(1) = 39 > 35. The badge has its own
        // unconditional render path.
        assert_eq!(activity_column_padding(20, 35, 12), None);
    }

    #[test]
    fn row_tag_content_fits_within_max_width() {
        // RowTag.rendered() right-pads to max_width by display width; content over
        // max_width would zero the pad and make the bracket span jitter.
        // profile_short_code's 4-cell cap is the tightest case.
        use unicode_width::UnicodeWidthStr;
        assert!(profile_short_code("forit-backup-extra").width() <= 4);
        assert!(profile_short_code("界界界-main").width() <= 4);
    }

    /// The bracketed tag must occupy `max_width + 2` cells as the renderer paints them:
    /// locked `ratatui-core` spends a cell on each halfwidth katakana dakuten (U+FF9E,
    /// U+FF9F) that `UnicodeWidthStr` scores at zero, so a string-width metric draws a
    /// nine-cell tag for `ｶﾞｻﾞﾊﾟ`. Asserting through a `Buffer` reads back real cells.
    #[test]
    fn row_tag_rendered_fits_the_fixed_width_contract_in_a_buffer() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::text::Line;
        for (content, max_width) in [("ｶﾞｻﾞﾊﾟ", 4), ("main", 4), ("界界界", 4), ("♥️", 4)]
        {
            let rendered = RowTag {
                content: content.into(),
                max_width,
            }
            .rendered();
            let total = (max_width + 8) as u16;
            let mut buffer = Buffer::empty(Rect::new(0, 0, total, 1));
            // `set_line` returns the column after the last painted cell.
            let (x, _) = buffer.set_line(0, 0, &Line::raw(rendered.clone()), total);
            assert_eq!(
                x as usize,
                max_width + 2,
                "{content}: painted {rendered:?} past the fixed-width contract"
            );
        }
    }

    #[test]
    fn row_tag_rendered_caps_wide_content_to_max_width() {
        let tag = RowTag {
            content: "界".repeat(12),
            max_width: BRANCH_TAG_WIDTH,
        };
        let rendered = tag.rendered();
        assert_eq!(
            rendered_width(&rendered),
            BRANCH_TAG_WIDTH + 2,
            "a wide branch name must cap at the tag contract, got {rendered:?}"
        );
    }

    /// The repository count is what the workspace tag exists to carry, so the branch is
    /// cut in cells to leave room rather than filling the cap and pushing it off.
    #[test]
    fn workspace_branch_row_tag_keeps_the_repository_count() {
        let rendered = workspace_branch_row_tag(&"界".repeat(10), 2)
            .expect("a wide branch still yields a tag")
            .rendered();
        assert_eq!(rendered, "[界界界界界+2]");
        assert_eq!(rendered_width(&rendered), BRANCH_TAG_WIDTH + 2);
    }

    #[test]
    fn row_tag_rendered_keeps_grapheme_sequences_whole() {
        // VS16 sequence plus a wide glyph: the VS16 adds no cells and the
        // prefix measurement must not split the cluster or over-count it.
        let tag = RowTag {
            content: "\u{2665}\u{fe0f}界-a".to_string(),
            max_width: BRANCH_TAG_WIDTH,
        };
        let rendered = tag.rendered();
        assert!(
            rendered.contains('\u{2665}'),
            "the heart must survive grapheme-aligned truncation, got {rendered:?}"
        );
        assert_eq!(
            rendered_width(&rendered),
            BRANCH_TAG_WIDTH + 2,
            "got {rendered:?}"
        );

        // Skin-tone modifier: chars sum to 4 but the cluster is 2 cells, so
        // a per-char budget would drop the trailing "m" that actually fits.
        let tag = RowTag {
            content: "\u{1f91d}\u{1f3fd}m".to_string(),
            max_width: BRANCH_TAG_WIDTH,
        };
        assert!(
            tag.rendered().contains('m'),
            "the m fits in cells and must not be dropped"
        );
    }

    /// Padding is display-width-aware: a two-cell glyph counts as two.
    #[test]
    fn row_tag_rendered_pads_by_display_width() {
        let wide = RowTag {
            content: "界界".to_string(),
            max_width: 4,
        };
        assert_eq!(wide.rendered(), "[界界]");
        let one_wide = RowTag {
            content: "界m".to_string(),
            max_width: 4,
        };
        assert_eq!(one_wide.rendered(), "[界m ]");
        let combining = RowTag {
            content: "i\u{307}abm".to_string(),
            max_width: 4,
        };
        assert_eq!(combining.rendered(), "[i\u{307}abm]");
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
        // prefix(8) + 7 trailing = 15 <= 25: a 25-col pane was hidden by the old fixed
        // 30-col floor even with room to spare.
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
