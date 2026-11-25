//! Structured view render: transcript, approval shelf, queue, composer, and
//! status line, with pickers floating above the composer.

use std::collections::{HashMap, HashSet};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::Frame;
use similar::{ChangeTag, TextDiff};

use aoe_plugin_api::UiSlot;

use ansi_to_tui::IntoText;

use super::input::{Focus, Intent};
use super::reducer::{
    AcpTranscript, NoteKind, PendingApproval, ToolCallRow, ToolCompletion, ToolOutcome,
};
use super::state::{
    FileIndex, PickerKind, PickerTarget, ScrollAnchor, StructuredViewState, TranscriptTextCache,
    ViewLayout,
};
use crate::acp::approvals::{tool_target, ToolTarget, CMD_KEYS, PATH_KEYS};
use crate::acp::session_paths::{relative_display_path, SessionPathRoots};
use crate::acp::state::{
    AuthStatusKind, SessionUsage, SubagentRecord, SubagentState, ToolOutputBlock,
};
use crate::acp::transcript::{TranscriptRow, TranscriptRowKind};
use crate::tui::components::hover::paint_hover_bg;
use crate::tui::plugin_ui;
use crate::tui::styles::Theme;

/// Render the view into `area`. `active` is false for an embedded preview,
/// which shows no caret. Returns the transcript geometry for drag-select.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    active: bool,
) -> TranscriptGeometry {
    let layout = compute_layout(area, state);
    // Keep the previous window start so hover cannot scroll rows away.
    let prev_picker = state.mouse_targets.take().picker;

    let geometry = render_transcript(frame, layout.transcript, theme, state, active);
    render_status(frame, layout.status, theme, state, active);
    if layout.approval.height > 0 {
        render_approval_shelf(frame, layout.approval, theme, state, active);
    }
    // After the approval shelf, which replaces the button list wholesale.
    if layout.notices.height > 0 {
        let close = render_notices(frame, layout.notices, theme, state);
        state.mouse_targets.borrow_mut().buttons.extend(close);
    }
    if layout.queue.height > 0 {
        render_queue(frame, layout.queue, theme, state);
    }
    render_composer(frame, layout.composer, theme, state, active);
    // Pickers float above the bottom-anchored composer. The choice picker owns
    // the navigation keys while open, so it wins the pixels too.
    let prev_first = |kind| {
        prev_picker
            .filter(|p: &PickerTarget| p.kind == kind)
            .map_or(0, |p| p.first)
    };
    if state.choice.is_some() {
        let first = prev_first(PickerKind::Choice);
        render_choice_picker(frame, layout.composer, theme, state, first);
    } else if matches!(state.focus, Focus::Composer) && state.slash_picker_open() {
        let first = prev_first(PickerKind::Slash);
        render_slash_picker(frame, layout.composer, theme, state, first);
    } else if matches!(state.focus, Focus::Composer) && state.mention.is_some() {
        let first = prev_first(PickerKind::Mention);
        render_mention_picker(frame, layout.composer, theme, state, first);
    }
    // The plugin pane is a modal overlay; an open choice picker still owns the
    // navigation keys, so it must stay visible on top.
    if matches!(state.focus, Focus::Pane) && state.choice.is_none() {
        render_pane_panel(frame, area, theme, state);
    }
    geometry
}

/// Permission mode / elicitation answer / plugin-link picker.
fn render_choice_picker(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    prev_first: usize,
) {
    let Some(picker) = &state.choice else {
        return;
    };
    // Body width inside the popup: its border and horizontal padding.
    let body_width = composer_area.width.saturating_sub(4);
    // Rows the popup can paint above the composer, less its two borders.
    let budget = (composer_area.y as usize).saturating_sub(2);
    // The detail block is reserved at its fullest across the open options, so
    // moving to another option cannot resize the popup and slide the rows out
    // from under the pointer.
    let detail_rows = picker
        .details
        .iter()
        .flatten()
        .map(|detail| detail_row_count(detail, body_width))
        .max()
        .unwrap_or(0)
        .min(CHOICE_DETAIL_MAX_ROWS)
        .min(budget.saturating_sub(1));
    // The answers come first: context is what gets dropped on a short terminal,
    // never the option the user is about to accept.
    let (first, mut lines) = window_rows(
        composer_area,
        budget.saturating_sub(detail_rows).min(8),
        picker.selected,
        prev_first,
        &picker.options,
        |(_, label)| vec![label.clone()],
    );
    let rows = lines.len();
    let context_cap = CHOICE_CONTEXT_MAX_ROWS.min(budget.saturating_sub(rows + detail_rows));
    let context_rows = picker.context.as_deref().map_or(0, |context| {
        let style = Style::default().fg(theme.hint);
        let mut wrapped = Vec::new();
        for text in context.lines() {
            wrap_line_into(
                Line::styled(text.to_string(), style),
                body_width,
                &mut wrapped,
            );
        }
        cap_rows(&mut wrapped, context_cap, style);
        let count = wrapped.len() as u16;
        lines.splice(0..0, wrapped);
        count
    });
    // The highlighted option carries its own detail, so the answer being
    // considered reads in full without opening the web form.
    let detail_style = Style::default()
        .fg(theme.dimmed)
        .add_modifier(Modifier::ITALIC);
    let mut detail = Vec::new();
    if let Some(Some(text)) = picker.details.get(picker.selected) {
        push_detail_rows(text, body_width, detail_style, &mut detail);
    }
    // Capped at the reserved height rather than the constant, so a short
    // terminal ends on the marker instead of dropping rows silently.
    cap_rows(&mut detail, detail_rows, detail_style);
    // Padded to the reserved height so the option rows keep their position.
    detail.resize(detail_rows, Line::default());
    lines.extend(detail);
    let drawn = render_popup_above(frame, composer_area, theme, picker.title.clone(), lines);
    record_picker(state, PickerKind::Choice, drawn, first, rows, context_rows);
}

/// Rows `detail` takes at `width`, counted the way it is painted.
fn detail_row_count(detail: &str, width: u16) -> usize {
    let style = Style::default().add_modifier(Modifier::ITALIC);
    let mut wrapped = Vec::new();
    push_detail_rows(detail, width, style, &mut wrapped);
    wrapped.len()
}

/// Wrap one option's detail into `rows`: the bullet opens the first source line
/// and later lines align under the text.
fn push_detail_rows(detail: &str, width: u16, style: Style, rows: &mut Vec<Line<'static>>) {
    for (index, line) in detail.lines().enumerate() {
        let prefix = if index == 0 { "· " } else { "  " };
        wrap_line_into(Line::styled(format!("{prefix}{line}"), style), width, rows);
    }
}

/// Context rows a choice picker shows above its options, and detail rows it
/// shows under them, so a long question never hides the answers themselves.
const CHOICE_CONTEXT_MAX_ROWS: usize = 4;
const CHOICE_DETAIL_MAX_ROWS: usize = 3;

/// Cap `rows` at `max`, ending on a marker row when text was dropped so a cut
/// sentence never reads as the whole of it.
fn cap_rows(rows: &mut Vec<Line<'static>>, max: usize, style: Style) {
    if rows.len() <= max {
        return;
    }
    if max == 0 {
        rows.clear();
        return;
    }
    rows.truncate(max - 1);
    rows.push(Line::styled("…", style));
}

/// Store a drawn picker as the frame's click / hover target. `rows` counts the
/// item rows, and `top_rows` the context rows painted above them; a trailing
/// note under the items is in neither.
fn record_picker(
    state: &StructuredViewState,
    kind: PickerKind,
    drawn: Option<(Rect, Rect)>,
    first: usize,
    rows: usize,
    top_rows: u16,
) {
    let Some((area, inner)) = drawn else {
        return;
    };
    let top_rows = top_rows.min(inner.height);
    let height = u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .min(inner.height - top_rows);
    state.mouse_targets.borrow_mut().picker = Some(PickerTarget {
        kind,
        area,
        rows: Rect {
            y: inner.y + top_rows,
            height,
            ..inner
        },
        first,
    });
}

/// Up to `max_rows` picker rows (fewer on a short terminal), windowed so
/// `selected` stays visible and marked. Returns the first shown item's index
/// with the rows.
fn window_rows<T, S: Into<Span<'static>>>(
    composer_area: Rect,
    max_rows: usize,
    selected: usize,
    prev_start: usize,
    items: &[T],
    spans: impl Fn(&T) -> Vec<S>,
) -> (usize, Vec<Line<'static>>) {
    // Minus the two border rows, so the selection can't paint off-screen.
    let max_rows = (composer_area.y as usize).saturating_sub(2).min(max_rows);
    if max_rows == 0 || items.is_empty() {
        return (0, Vec::new());
    }
    let total = items.len();
    let cap = max_rows.min(total);
    let start = window_start(selected, cap, total, prev_start);
    let lines = items[start..(start + cap).min(total)]
        .iter()
        .enumerate()
        .map(|(offset, item)| {
            let is_sel = start + offset == selected;
            let mut row: Vec<Span<'static>> = spans(item).into_iter().map(Into::into).collect();
            if let Some(first) = row.first_mut() {
                first.content =
                    format!("{}{}", if is_sel { "▶ " } else { "  " }, first.content).into();
                if is_sel {
                    first.style = first.style.add_modifier(Modifier::BOLD);
                }
            }
            Line::from(row)
        })
        .collect();
    (start, lines)
}

/// First visible index of a `cap`-row window that keeps `selected` inside it,
/// scrolling from `prev_start` only as far as needed.
fn window_start(selected: usize, cap: usize, total: usize, prev_start: usize) -> usize {
    let start = prev_start.min(total.saturating_sub(cap));
    if selected < start {
        selected
    } else if selected >= start + cap {
        selected + 1 - cap
    } else {
        start
    }
}

/// Bordered popup whose bottom edge sits on the composer's top edge. Returns
/// the popup and its body rects, or `None` when nothing was drawn.
fn render_popup_above(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    title: String,
    lines: Vec<Line<'_>>,
) -> Option<(Rect, Rect)> {
    if lines.is_empty() {
        return None;
    }
    let y = composer_area.y.saturating_sub(lines.len() as u16 + 2);
    let area = Rect {
        y,
        height: composer_area.y - y,
        ..composer_area
    };
    if area.height < 3 {
        return None;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(title)
        .border_style(Style::default().fg(theme.title));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
    Some((area, inner))
}

/// Pure so the redraw path can stash it on `state.layout` for hit-testing.
pub(super) fn compute_layout(area: Rect, state: &StructuredViewState) -> ViewLayout {
    let queue_height = queued_strip_height(state);
    // A tall shelf on a short terminal would crowd out the transcript behind it.
    let approval_height = approval_shelf_height(state, area.width).min(area.height / 2);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5), // transcript
            Constraint::Length(approval_height),
            Constraint::Length(notices_strip_height(state)), // 0 when none
            Constraint::Length(queue_height),                // queued prompts strip (0 when empty)
            Constraint::Length(composer_height(state)),
            Constraint::Length(1), // status line
        ])
        .split(area);
    ViewLayout {
        transcript: chunks[0],
        approval: chunks[1],
        notices: chunks[2],
        queue: chunks[3],
        composer: chunks[4],
        status: chunks[5],
    }
}

/// The approval shelf's height: borders, its request detail, and its actions
/// row, all from the rows the shelf will paint.
fn approval_shelf_height(state: &StructuredViewState, width: u16) -> u16 {
    let Some(row) = selected_pending_approval(state) else {
        return 0;
    };
    let body = approval_body_rows(
        row,
        state.path_roots.as_ref(),
        width.saturating_sub(4),
        APPROVAL_BODY_MAX_ROWS,
    );
    2 + body.len() as u16 + 1
}

/// Plugin pane overlay: right half when wide, full width when narrow.
/// Pre-wrapped so the painted rows and the scroll clamp agree.
fn render_pane_panel(frame: &mut Frame, area: Rect, theme: &Theme, state: &StructuredViewState) {
    let panel = if area.width >= 100 {
        let half = area.width / 2;
        Rect {
            x: area.x + (area.width - half),
            y: area.y,
            width: half,
            height: area.height,
        }
    } else {
        area
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(" Plugin pane ")
        .border_style(Style::default().fg(theme.title));
    let inner = block.inner(panel);
    frame.render_widget(Clear, panel);
    frame.render_widget(block, panel);
    // The status hint is covered by this overlay, so paint the way out on the
    // bottom border as a button. The modal covers every other target.
    let close = if panel.width > 2 && panel.height > 1 {
        let label = " Esc to close ";
        let rect = Rect {
            x: panel.x + 1,
            y: panel.bottom() - 1,
            width: (label.len() as u16).min(panel.width - 2),
            height: 1,
        };
        frame.render_widget(Paragraph::new(label), rect);
        vec![(rect, Intent::SetFocus(Focus::Transcript))]
    } else {
        Vec::new()
    };
    paint_hovered_button(frame, theme, state, &close);
    state.mouse_targets.borrow_mut().buttons = close;
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mut lines = plugin_ui::pane_lines(&state.plugin_ui, &state.session_id, theme);
    let home = plugin_ui::home_pane_lines(&state.plugin_ui, theme);
    if !home.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(home);
    }
    if lines.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No plugin pane for this session.",
                Style::default().fg(theme.dimmed),
            ))),
            inner,
        );
        return;
    }
    let mut wrapped: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    for line in lines {
        wrap_line_into(line, inner.width, &mut wrapped);
    }
    // Saturate: large payloads on a narrow panel can exceed u16 rows.
    let rows = wrapped.len().min(u16::MAX as usize) as u16;
    let max_scroll = rows.saturating_sub(inner.height);
    // Lets the next scroll step resolve the bottom sentinel to a real row.
    state.last_pane_scroll_max.set(max_scroll);
    let offset = state.pane_scroll.min(max_scroll);
    frame.render_widget(Paragraph::new(wrapped).scroll((offset, 0)), inner);
}

fn queued_strip_height(state: &StructuredViewState) -> u16 {
    u16::from(!state.queue.is_empty())
}

/// `×` is one column wide but two bytes, so the width is stated rather than
/// taken from the string's length.
const CLOSE_LABEL: &str = " × ";
const CLOSE_LABEL_WIDTH: u16 = 3;

/// One line per undismissed advisory. Capped daemon-side, so this never
/// starves the transcript.
fn notices_strip_height(state: &StructuredViewState) -> u16 {
    state.visible_notices().count() as u16
}

/// Agent-pushed advisories, toned by severity, each dismissable with `x` or a
/// click on its `×`. Dismissal is local, so it never clears another client.
fn render_notices(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
) -> Vec<(Rect, Intent)> {
    let mut buttons = Vec::new();
    for (row, notice) in state.visible_notices().enumerate() {
        let Some(y) = area
            .y
            .checked_add(row as u16)
            .filter(|y| *y < area.bottom())
        else {
            break;
        };
        let colour = match notice.severity.as_str() {
            "error" => theme.error,
            "warning" => theme.title,
            // An unknown future level reads as advisory rather than alarming.
            _ => theme.hint,
        };
        let mut text = format!(" {} ", notice.title);
        if let Some(description) = notice.description.as_deref().map(str::trim) {
            if !description.is_empty() {
                text.push_str(description);
                text.push(' ');
            }
        }
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", notice.severity),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            Span::styled(text, Style::default().fg(theme.hint)),
        ]);
        // The close button keeps its columns; the text wraps short of them so a
        // long advisory is clipped rather than painted under the `×`.
        let close = Rect {
            x: area.right().saturating_sub(CLOSE_LABEL_WIDTH),
            y,
            width: CLOSE_LABEL_WIDTH,
            height: 1,
        };
        let text_area = Rect {
            x: area.x,
            y,
            width: area.width.saturating_sub(CLOSE_LABEL_WIDTH),
            height: 1,
        };
        frame.render_widget(Paragraph::new(line), text_area);
        if close.x > text_area.x {
            frame.render_widget(
                Paragraph::new(Span::styled(CLOSE_LABEL, Style::default().fg(colour))),
                close,
            );
            buttons.push((close, Intent::DismissNotice(Some(notice.id.clone()))));
        }
    }
    buttons
}

fn render_queue(frame: &mut Frame, area: Rect, theme: &Theme, state: &StructuredViewState) {
    let line = Line::from(vec![
        Span::styled(
            format!(" Queued {} ", state.queue.len()),
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "↑ edit latest · Ctrl+X clear · sends when ready",
            Style::default().fg(theme.hint),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// The approval the shelf is showing: the selected nonce, if it is still
/// pending. The layout and the render both ask, so the reserved rows and the
/// painted card always agree.
fn selected_pending_approval(state: &StructuredViewState) -> Option<&PendingApproval> {
    let selected = state.selected_approval.as_deref()?;
    state
        .transcript
        .pending_approvals
        .iter()
        .find(|pending| pending.nonce == selected)
}

fn render_approval_shelf(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    active: bool,
) {
    let Some(row) = selected_pending_approval(state) else {
        return;
    };
    let position = state
        .transcript
        .pending_approvals
        .iter()
        .position(|pending| pending.nonce == row.nonce)
        .unwrap_or_default()
        + 1;
    let total = state.transcript.pending_approvals.len();
    let accent = if row.destructive {
        theme.error
    } else {
        theme.waiting
    };
    let target = approval_target(row, state.path_roots.as_ref());
    let mut title = format!(" Approval {position}/{total} · {}", row.title);
    if !target.is_empty() {
        title.push_str(&format!(" · {target}"));
    }
    if row.destructive {
        title.push_str(" · destructive");
    }
    title.push(' ');
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(title)
        .border_style(Style::default().fg(accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // The request itself, so the user reads what they are approving rather
    // than only the tool name: the actions sit on the bottom row.
    if inner.height > 1 {
        let body_area = Rect {
            height: inner.height - 1,
            ..inner
        };
        let body = approval_body_lines(
            row,
            state.path_roots.as_ref(),
            theme,
            body_area.width,
            body_area.height as usize,
        );
        frame.render_widget(Paragraph::new(body), body_area);
    }
    let actions_area = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1.min(inner.height),
        ..inner
    };
    let (actions, spans) =
        approval_actions_line(theme, active, row.choice && !row.options.is_empty());
    frame.render_widget(Paragraph::new(actions), actions_area);
    // An open choice picker owns the keyboard, so the decision buttons go
    // inert with their keys.
    if state.choice.is_some() {
        return;
    }
    let buttons: Vec<(Rect, Intent)> = spans
        .into_iter()
        .filter(|(offset, _, _)| *offset < inner.width)
        .map(|(offset, width, intent)| {
            let rect = Rect {
                x: inner.x + offset,
                y: actions_area.y,
                width: width.min(inner.width - offset),
                height: 1,
            };
            (rect, intent)
        })
        .collect();
    paint_hovered_button(frame, theme, state, &buttons);
    state.mouse_targets.borrow_mut().buttons = buttons;
}

/// Request detail rows the shelf shows above its actions. Capped so a modal
/// prompt leaves the transcript readable behind it.
const APPROVAL_BODY_MAX_ROWS: usize = 5;

/// How one body row of the shelf reads.
#[derive(Clone, Copy)]
enum ApprovalRowKind {
    /// The command or path the call is about, which leads the body.
    Primary,
    /// Any other argument, as `key: value`.
    Detail,
    /// The count of rows that did not fit.
    Hidden,
}

/// One line of the approval's request payload. `primary` marks the command or
/// path the call is about, which leads the body.
fn approval_body_entries(
    row: &PendingApproval,
    target: Option<&ToolTarget>,
) -> Vec<(String, bool)> {
    let Some(args) = parse_args_object(&row.args) else {
        let raw = row.args.trim();
        return if raw.is_empty() {
            Vec::new()
        } else {
            vec![(raw.to_string(), true)]
        };
    };
    // The kind decides which argument leads: a path-based call that also carries a
    // command string must not promote it over the path the call is about.
    let keys: Vec<&str> = match target {
        Some(ToolTarget::Path(_)) => PATH_KEYS.to_vec(),
        Some(ToolTarget::Command(_)) => CMD_KEYS.to_vec(),
        None => CMD_KEYS.iter().chain(PATH_KEYS).copied().collect(),
    };
    let primary_key = keys
        .iter()
        .find(|key| args.get(**key).is_some_and(|value| value.is_string()));
    let mut entries = Vec::new();
    if let Some(key) = primary_key {
        // The title elides this to its first line, so show it whole here.
        if let Some(value) = args.get(*key).and_then(|value| value.as_str()) {
            entries.push((value.to_string(), true));
        }
    }
    for (key, value) in &args {
        if key.starts_with("_aoe_") || Some(key.as_str()) == primary_key.copied() {
            continue;
        }
        let text = match value {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        if !text.trim().is_empty() {
            entries.push((format!("{key}: {text}"), false));
        }
    }
    entries
}

/// Render control characters as the control pictures that occupy one cell each,
/// so a tab or an escape byte is visible instead of vanishing into its neighbors.
fn visible_controls(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => '\u{2409}',
            // DEL sits past the C0 pictures, whose offset would land on a digit.
            '\x7f' => '\u{2421}',
            c if c.is_ascii_control() => char::from_u32(0x2400 + c as u32).unwrap_or('\u{fffd}'),
            // C1 controls have no control picture; a replacement keeps them from
            // vanishing out of the command being approved.
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect()
}

/// The request detail, wrapped to `width` and capped at
/// [`APPROVAL_BODY_MAX_ROWS`], unstyled. The shelf's height and the rows it
/// paints both come from here, so the reserved space cannot disagree with the
/// text, which matters when the text is a command the user is approving.
fn approval_body_rows(
    row: &PendingApproval,
    path_roots: Option<&SessionPathRoots>,
    width: u16,
    max_rows: usize,
) -> Vec<(Line<'static>, ApprovalRowKind)> {
    let mut rows: Vec<(Line<'static>, ApprovalRowKind)> = Vec::new();
    let args = parse_args_object(&row.args);
    let target = args.as_ref().and_then(|args| tool_target(&row.kind, args));
    for (text, primary) in approval_body_entries(row, target.as_ref()) {
        let is_command = primary && matches!(target, Some(ToolTarget::Command(_)));
        let text = match (target.as_ref(), primary) {
            (Some(ToolTarget::Path(_)), true) => {
                relative_display_path(&text, path_roots).to_string()
            }
            _ => text,
        };
        let kind = if primary {
            ApprovalRowKind::Primary
        } else {
            ApprovalRowKind::Detail
        };
        let mut wrapped = Vec::new();
        // A command may run over several lines, and wrapping it as one would run
        // those lines together: the shelf must show what would actually run.
        for (index, logical) in text.split('\n').enumerate() {
            let prefix = if is_command && index == 0 { "$ " } else { "" };
            // Ratatui drops control characters, so a tab would silently join words:
            // show them instead, wrapped as they will read on screen.
            wrap_line_into(
                Line::raw(format!("{prefix}{}", visible_controls(logical))),
                width,
                &mut wrapped,
            );
        }
        rows.extend(wrapped.into_iter().map(|line| (line, kind)));
    }
    // The shelf's real height wins: a capped body would otherwise be clipped by
    // the paragraph, hiding rows without saying so.
    let cap = max_rows.clamp(1, APPROVAL_BODY_MAX_ROWS);
    if rows.len() > cap {
        let hidden = rows.len() - (cap - 1);
        rows.truncate(cap - 1);
        rows.push((
            Line::raw(format!("… +{hidden} more")),
            ApprovalRowKind::Hidden,
        ));
    }
    rows
}

/// Style the shared body rows for painting.
fn approval_body_lines(
    row: &PendingApproval,
    path_roots: Option<&SessionPathRoots>,
    theme: &Theme,
    width: u16,
    max_rows: usize,
) -> Vec<Line<'static>> {
    approval_body_rows(row, path_roots, width, max_rows)
        .into_iter()
        .map(|(mut line, kind)| {
            let style = match kind {
                ApprovalRowKind::Primary => Style::default().fg(theme.text),
                ApprovalRowKind::Detail => Style::default().fg(theme.hint),
                ApprovalRowKind::Hidden => {
                    Style::default().fg(theme.hint).add_modifier(Modifier::DIM)
                }
            };
            for span in &mut line.spans {
                span.style = style;
            }
            line
        })
        .collect()
}

/// Highlight whichever of this frame's `buttons` the pointer is over.
fn paint_hovered_button(
    frame: &mut Frame,
    theme: &Theme,
    state: &StructuredViewState,
    buttons: &[(Rect, Intent)],
) {
    let rects: Vec<Rect> = buttons.iter().map(|(rect, _)| *rect).collect();
    if let Some(rect) = state.hover.current_in(&rects) {
        paint_hover_bg(frame, rect, theme.selection);
    }
}

/// The action hints, plus each clickable action as `(column offset, width,
/// intent)` with the intent its key sends. Choice approvals ask a question, so
/// `a` answers and "always" doesn't apply.
fn approval_actions_line(
    theme: &Theme,
    active: bool,
    choice: bool,
) -> (Line<'static>, Vec<(u16, u16, Intent)>) {
    use crate::acp::protocol::ApprovalDecisionWire as Decision;

    if !active {
        let hint = Span::styled("Enter to respond", Style::default().fg(theme.hint));
        return (Line::from(hint), Vec::new());
    }
    let resolve = Intent::ResolveApproval;
    let mut actions: Vec<(&'static str, Color, &'static str, Intent)> = vec![(
        "a",
        theme.running,
        if choice { " answer" } else { " allow once" },
        resolve(Decision::Allow),
    )];
    if !choice {
        actions.push((
            "A",
            theme.running,
            " always",
            resolve(Decision::AllowAlways),
        ));
    }
    actions.push(("d", theme.error, " deny", resolve(Decision::Deny)));
    actions.push(("Esc", theme.hint, " stop", Intent::CancelInFlight));

    let mut spans = Vec::new();
    let mut buttons = Vec::new();
    let mut offset = 0u16;
    for (i, (key, color, label, intent)) in actions.into_iter().enumerate() {
        if i > 0 {
            let sep = Span::styled("  ·  ", Style::default().fg(theme.border));
            offset += sep.width() as u16;
            spans.push(sep);
        }
        let key = Span::styled(key, Style::default().fg(color).add_modifier(Modifier::BOLD));
        let label = Span::styled(label, Style::default().fg(theme.hint));
        let width = (key.width() + label.width()) as u16;
        buttons.push((offset, width, intent));
        offset += width;
        spans.extend([key, label]);
    }
    (Line::from(spans), buttons)
}

fn approval_target(row: &PendingApproval, path_roots: Option<&SessionPathRoots>) -> String {
    let args = parse_args_object(&row.args);
    display_tool_target(&row.kind, args.as_ref(), path_roots).unwrap_or_default()
}

fn display_tool_target(
    kind: &str,
    args: Option<&serde_json::Map<String, serde_json::Value>>,
    path_roots: Option<&SessionPathRoots>,
) -> Option<String> {
    Some(match tool_target(kind, args?)? {
        ToolTarget::Path(path) => relative_display_path(path, path_roots),
        ToolTarget::Command(command) => command.to_owned(),
    })
}

fn render_slash_picker(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    prev_first: usize,
) {
    let matches = state.slash_matches();
    let (first, lines) = window_rows(
        composer_area,
        8,
        state.slash_selected,
        prev_first,
        &matches,
        |cmd| {
            let mut spans = vec![Span::raw(format!("/{}", cmd.name))];
            if let Some(hint) = &cmd.input_hint {
                spans.push(Span::styled(
                    format!(" {hint}"),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            if !cmd.description.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", cmd.description),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            spans
        },
    );
    let rows = lines.len();
    let drawn = render_popup_above(
        frame,
        composer_area,
        theme,
        " Commands (↑/↓ or Ctrl+n/p · Enter/Tab select · Esc dismiss) ".into(),
        lines,
    );
    record_picker(state, PickerKind::Slash, drawn, first, rows, 0);
}

/// `@` mention picker, with placeholders while the file index loads or fails.
fn render_mention_picker(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    prev_first: usize,
) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let placeholder = |text: String, style: Style| vec![Line::from(Span::styled(text, style))];
    let selected = state.mention.as_ref().map(|s| s.selected).unwrap_or(0);
    // Placeholders are not rows: the popup swallows clicks but picks nothing.
    let mut first = 0;
    let mut rows = 0;
    let lines = match &state.file_index {
        FileIndex::Unloaded | FileIndex::Loading => placeholder("  loading files…".into(), dim),
        FileIndex::Failed(err) => placeholder(
            format!("  file list unavailable: {err}"),
            Style::default().fg(theme.error),
        ),
        FileIndex::Loaded { truncated, .. } => {
            let files = super::filtered_mention_files(state);
            if files.is_empty() {
                placeholder("  no matching files".into(), dim)
            } else {
                let (start, mut lines) =
                    window_rows(composer_area, 8, selected, prev_first, &files, |path| {
                        vec![path.to_string()]
                    });
                if lines.is_empty() {
                    return;
                }
                first = start;
                rows = lines.len();
                if *truncated {
                    lines.push(Line::from(Span::styled(
                        "  (workspace over 5000 files; list capped)",
                        dim,
                    )));
                }
                lines
            }
        }
    };
    let drawn = render_popup_above(
        frame,
        composer_area,
        theme,
        " Files (↑/↓ or Ctrl+n/p · Enter/Tab insert · Esc close) ".into(),
        lines,
    );
    record_picker(state, PickerKind::Mention, drawn, first, rows, 0);
}

/// One separator row above the terminal-native prompt rail.
const COMPOSER_CHROME_ROWS: u16 = 1;
/// Content rows before multi-line prompts scroll inside the textarea.
const COMPOSER_MAX_CONTENT_ROWS: u16 = 6;

fn composer_height(state: &StructuredViewState) -> u16 {
    let lines = state.composer.lines().len().max(1) as u16;
    lines.clamp(1, COMPOSER_MAX_CONTENT_ROWS) + COMPOSER_CHROME_ROWS
}

fn render_transcript(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    active: bool,
) -> TranscriptGeometry {
    let friendly_title = state
        .transcript
        .session_title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&state.session_id);
    let body = if active && area.width >= 28 && area.height >= 8 {
        let card_width = metadata_card_width(state, friendly_title, area.width);
        let card_area = Rect {
            width: card_width,
            height: 6,
            ..area
        };
        render_metadata_card(frame, card_area, theme, state, friendly_title);
        Rect {
            y: area.y.saturating_add(7),
            height: area.height.saturating_sub(7),
            ..area
        }
    } else {
        let mut identity = vec![Span::styled(
            if active { "● " } else { "○ " },
            Style::default().fg(if active { theme.running } else { theme.hint }),
        )];
        identity.push(Span::styled(
            friendly_title.to_string(),
            Style::default()
                .fg(if active { theme.title } else { theme.hint })
                .add_modifier(Modifier::BOLD),
        ));
        if let Some(agent) = state.transcript.agent_name.as_deref() {
            identity.push(Span::styled(
                format!(" · {agent}"),
                Style::default().fg(theme.hint),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(identity)), area);
        Rect {
            y: area.y.saturating_add(2),
            height: area.height.saturating_sub(2),
            ..area
        }
    };

    let (plan_area, text_area) = if state.transcript.current_plan.is_empty() || body.height < 2 {
        (Rect::default(), body)
    } else {
        let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(body);
        (chunks[0], chunks[1])
    };
    if plan_area.height > 0 {
        frame.render_widget(
            Paragraph::new(plan_summary_line(&state.transcript.current_plan, theme)),
            plan_area,
        );
    }

    let (text, groups) =
        wrapped_transcript_rows(state, theme, text_area.width, ToolOutputMode::Compact);
    // Pre-wrapped at the render width, so visual rows are logical rows.
    let total = text.lines.len().min(u16::MAX as usize) as u16;
    let max = total.saturating_sub(text_area.height);
    // Lets a scroll step resolve the stick-to-bottom sentinel first.
    state.last_scroll_max.set(max);
    let mut first = state.scroll_offset.get().min(max);
    // A projection that grew above the reader moved their row; put the row they
    // were reading back where they had it.
    if let Some(anchor) = state.take_scroll_anchor() {
        if let Some(index) = anchored_offset(&text.lines, &groups, &anchor, first) {
            first = index.saturating_sub(anchor.pane_row).min(max);
            state.scroll_offset.set(first);
        }
    }
    // The topmost row with content is the one to find again: a blank separator
    // says nothing about which row the reader is on.
    let top_row = text
        .lines
        .iter()
        .enumerate()
        .skip(first as usize)
        .take(text_area.height as usize)
        .find_map(|(index, line)| {
            let content = line_text(line);
            if content.trim().is_empty() {
                return None;
            }
            let group = groups.get(index)?.as_ref()?;
            let (start, _) = group_bounds(&groups, group)?;
            Some(ScrollAnchor {
                group: group.clone(),
                row: (index - start).min(u16::MAX as usize) as u16,
                pane_row: (index - first as usize).min(u16::MAX as usize) as u16,
                text: content,
            })
        });
    state.note_top_row(top_row);
    let para = Paragraph::new(text).scroll((first, 0));
    frame.render_widget(para, text_area);
    TranscriptGeometry {
        text_area,
        first_line: first as usize,
        total_lines: total as usize,
    }
}

/// The plain content of one rendered row.
fn line_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// The rows of `groups` belonging to one transcript identity: `[start, end)`.
fn group_bounds(groups: &[Option<String>], group: &str) -> Option<(usize, usize)> {
    let start = groups
        .iter()
        .position(|row| row.as_deref() == Some(group))?;
    let len = groups[start..]
        .iter()
        .position(|row| row.as_deref() != Some(group))
        .unwrap_or(groups.len() - start);
    Some((start, start + len))
}

/// Last row of `[start, end)` with text, since an identity can end on the blank
/// separator that follows it. Anchoring on that blank pushes the identity's own
/// content off the top when the rows below it fill the pane.
fn last_content_row(lines: &[Line<'static>], start: usize, end: usize) -> usize {
    (start..end)
        .rev()
        .find(|&row| !line_text(&lines[row]).trim().is_empty())
        .unwrap_or(end.saturating_sub(1))
}

/// Row index of the identity the reader anchored on, found by identity rather
/// than by text, since identical replies either side of the row they were
/// reading are the same text. `first` hints the fallback search.
fn anchored_offset(
    lines: &[Line<'static>],
    groups: &[Option<String>],
    anchor: &ScrollAnchor,
    first: u16,
) -> Option<u16> {
    if let Some((start, end)) = group_bounds(groups, &anchor.group) {
        let expected = start + anchor.row as usize;
        // Inside the identity, its own text says which row grew: a projection
        // that expanded the row the reader was in still has that line.
        let row = lines[start..end]
            .iter()
            .enumerate()
            .filter(|(_, line)| line_text(line) == anchor.text)
            .map(|(offset, _)| start + offset)
            .min_by_key(|index| index.abs_diff(expected))
            .unwrap_or(expected)
            .min(last_content_row(lines, start, end));
        return u16::try_from(row).ok();
    }
    // The projection no longer holds that identity (a windowed row, say): fall
    // back to the nearest copy of its text.
    anchor_offset(lines, &anchor.text, first.saturating_add(anchor.pane_row))
}

/// Row index of `anchor` closest to `hint`, so a blank or repeated row keeps
/// the reader near where they were instead of jumping to the first match.
fn anchor_offset(lines: &[Line<'static>], anchor: &str, hint: u16) -> Option<u16> {
    // An empty anchor names no row: any match would be a different blank.
    if anchor.trim().is_empty() {
        return None;
    }
    let mut best: Option<(u16, usize)> = None;
    for (index, line) in lines.iter().enumerate() {
        if index > u16::MAX as usize {
            break;
        }
        if line_text(line) != anchor {
            continue;
        }
        let distance = (index as u16).abs_diff(hint);
        if best.is_none_or(|(closest, _)| distance < closest) {
            best = Some((distance, index));
        }
    }
    best.map(|(_, index)| index as u16)
}

const METADATA_CARD_MAX_WIDTH: u16 = 72;

fn metadata_card_width(
    state: &StructuredViewState,
    friendly_title: &str,
    available_width: u16,
) -> u16 {
    use unicode_width::UnicodeWidthStr;

    let (agent, directory, mode) = metadata_fields(state);
    let widest = [
        format!(
            "❯ Agent of Empires · {agent} (v{})",
            env!("CARGO_PKG_VERSION")
        ),
        format!("session:     {friendly_title}"),
        format!("directory:   {directory}"),
        format!("permissions: {mode}"),
    ]
    .into_iter()
    .map(|line| UnicodeWidthStr::width(line.as_str()))
    .max()
    .unwrap_or_default() as u16;
    widest
        .saturating_add(4)
        .clamp(36, METADATA_CARD_MAX_WIDTH)
        .min(available_width)
}

/// (agent, directory, permission mode) as shown on the metadata card.
fn metadata_fields(state: &StructuredViewState) -> (&str, &str, &str) {
    (
        state.transcript.agent_name.as_deref().unwrap_or("agent"),
        state
            .path_roots
            .as_ref()
            .map_or("loading…", |roots| roots.project_path.as_str()),
        state
            .transcript
            .current_mode
            .as_deref()
            .unwrap_or("default"),
    )
}

fn render_metadata_card(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    friendly_title: &str,
) {
    let inner_width = area.width.saturating_sub(4) as usize;
    let (agent, directory, mode) = metadata_fields(state);
    let lines = vec![
        Line::from(vec![
            Span::styled("❯ ", Style::default().fg(theme.title)),
            Span::styled(
                fit_display(
                    &format!(
                        "Agent of Empires · {agent} (v{})",
                        env!("CARGO_PKG_VERSION")
                    ),
                    inner_width.saturating_sub(2),
                ),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
        ]),
        metadata_line("session:", friendly_title, theme, inner_width),
        metadata_line("directory:", directory, theme, inner_width),
        metadata_line("permissions:", mode, theme, inner_width),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn metadata_line(
    label: &'static str,
    value: &str,
    theme: &Theme,
    available_width: usize,
) -> Line<'static> {
    const LABEL_WIDTH: usize = 13;
    Line::from(vec![
        Span::styled(
            format!("{label:<LABEL_WIDTH$}"),
            Style::default().fg(theme.hint),
        ),
        Span::styled(
            fit_display(value, available_width.saturating_sub(LABEL_WIDTH)),
            Style::default().fg(theme.text),
        ),
    ])
}

fn fit_display(value: &str, max_width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in value.chars() {
        let ch_width = ch.width().unwrap_or_default();
        if width.saturating_add(ch_width).saturating_add(1) > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push('…');
    out
}

/// Where the transcript text landed in the last render, for drag-select.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptGeometry {
    pub text_area: Rect,
    pub first_line: usize,
    pub total_lines: usize,
}

#[derive(Clone, Copy)]
pub(super) enum ToolOutputMode<'a> {
    Compact,
    Retained(&'a [crate::acp::transcript::UnfinishedToolOutput]),
}

/// The transcript pre-wrapped at `width`: the single source of transcript
/// geometry for painting, scroll clamping, and selection. The live projection
/// is cached, so an unchanged transcript costs a clone rather than a re-wrap.
pub(super) fn wrapped_transcript(
    state: &StructuredViewState,
    theme: &Theme,
    width: u16,
    tool_output: ToolOutputMode,
) -> Text<'static> {
    wrapped_transcript_rows(state, theme, width, tool_output).0
}

/// [`wrapped_transcript`] plus, per visual row, the transcript row identity it
/// came from, so a scroll anchor can find its own row again after the
/// projection moves it.
pub(super) fn wrapped_transcript_rows(
    state: &StructuredViewState,
    theme: &Theme,
    width: u16,
    tool_output: ToolOutputMode,
) -> (Text<'static>, Vec<Option<String>>) {
    let revision = state.transcript.projection_revision();
    if let ToolOutputMode::Compact = tool_output {
        let cached = state.transcript_cache.borrow();
        if let Some(hit) = cached.as_ref().filter(|hit| {
            hit.matches(
                revision,
                width,
                theme,
                state.path_roots.as_ref(),
                state.show_full_thinking,
            )
        }) {
            return (hit.text.clone(), hit.groups.clone());
        }
    }
    let projected = project_transcript_rows(state, theme, width, tool_output);
    // The retained reader builds its own cache over outputs the state does not
    // hold, so only the compact projection is stored here.
    if let ToolOutputMode::Compact = tool_output {
        *state.transcript_cache.borrow_mut() = Some(TranscriptTextCache {
            revision,
            width,
            theme: theme.clone(),
            roots: state.path_roots.clone(),
            full_thinking: state.show_full_thinking,
            text: projected.0.clone(),
            groups: projected.1.clone(),
        });
    }
    projected
}

/// Project and wrap without touching the cache. Selection extraction passes a
/// placeholder theme, so it reads this rather than evicting the painted text.
pub(super) fn project_transcript(
    state: &StructuredViewState,
    theme: &Theme,
    width: u16,
    tool_output: ToolOutputMode,
) -> Text<'static> {
    project_transcript_rows(state, theme, width, tool_output).0
}

fn project_transcript_rows(
    state: &StructuredViewState,
    theme: &Theme,
    width: u16,
    tool_output: ToolOutputMode,
) -> (Text<'static>, Vec<Option<String>>) {
    let text = transcript_lines(
        &state.transcript,
        theme,
        state.path_roots.as_ref(),
        state.show_full_thinking,
        tool_output,
    );
    let mut wrapped: Vec<Line<'static>> = Vec::with_capacity(text.lines.len());
    let mut groups: Vec<Option<String>> = Vec::with_capacity(text.lines.len());
    for (line, group) in text.lines.into_iter().zip(text.groups) {
        wrap_line_into(own_line(line), width, &mut wrapped);
        groups.resize(wrapped.len(), group);
    }
    (Text::from(wrapped), groups)
}

/// Transcript rows paired with the identity each row's lines came from.
struct TranscriptText {
    lines: Vec<Line<'static>>,
    groups: Vec<Option<String>>,
    current: Option<String>,
}

impl TranscriptText {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            groups: Vec::new(),
            current: None,
        }
    }

    /// Lines pushed from here belong to the transcript row `key` names.
    fn row_of(&mut self, key: String) {
        self.current = Some(key);
    }

    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
        self.groups.push(self.current.clone());
    }

    fn extend(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        for line in lines {
            self.push(line);
        }
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

fn plan_summary_line(plan: &[super::reducer::PlanLine], theme: &Theme) -> Line<'static> {
    use crate::acp::state::PlanStepStatus;

    let done = plan
        .iter()
        .filter(|step| {
            matches!(
                step.status,
                PlanStepStatus::Done | PlanStepStatus::Cancelled
            )
        })
        .count();
    let current = plan
        .iter()
        .find(|step| matches!(step.status, PlanStepStatus::InProgress))
        .or_else(|| {
            plan.iter()
                .find(|step| matches!(step.status, PlanStepStatus::Pending))
        });
    let complete = done == plan.len();
    let marker = if complete { "✓" } else { "◐" };
    let color = if complete { theme.running } else { theme.title };
    let mut spans = vec![Span::styled(
        format!(" {marker} Plan · {done}/{}", plan.len()),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    if let Some(step) = current {
        spans.push(Span::styled(
            format!(" · {}", step.title),
            Style::default().fg(theme.hint),
        ));
    } else if complete {
        spans.push(Span::styled(" · complete", Style::default().fg(theme.hint)));
    }
    Line::from(spans)
}

fn own_line(line: Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect();
    Line::from(spans).style(line.style)
}

/// Keep each grapheme in its first scalar's style without copying unaffected spans.
fn join_split_graphemes(line: &mut Line<'static>) {
    use std::borrow::Cow;
    use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

    let spans = &line.spans;
    if spans.len() < 2 {
        return;
    }
    let total = spans.iter().map(|span| span.content.len()).sum();
    let Some(mut chunk) = spans.iter().position(|span| !span.content.is_empty()) else {
        return;
    };
    let mut cursor = GraphemeCursor::new(0, total, true);
    let mut chunk_start = 0;
    let mut previous_chunk = chunk;
    let mut first = chunk;
    let mut crossings = Vec::new();
    let mut bridge = [0; 8];
    let mut bridge_len = 0;
    let mut bridge_start = 0;
    let mut backward_context = None;
    loop {
        let (text, start) = if bridge_len == 0 {
            (spans[chunk].content.as_ref(), chunk_start)
        } else {
            (
                std::str::from_utf8(&bridge[..bridge_len]).expect("encoded scalars"),
                bridge_start,
            )
        };
        match cursor.next_boundary(text, start) {
            Ok(Some(end)) => {
                backward_context = None;
                bridge_len = 0;
                let (last, prefix) = if end == chunk_start {
                    (previous_chunk, spans[previous_chunk].content.len())
                } else {
                    (chunk, end - chunk_start)
                };
                if first != last {
                    crossings.push((first, last, prefix));
                }
                first = chunk;
                if end == chunk_start + spans[chunk].content.len() {
                    first += 1;
                    while first < spans.len() && spans[first].content.is_empty() {
                        first += 1;
                    }
                }
            }
            Ok(None) => break,
            Err(GraphemeIncomplete::NextChunk) => {
                backward_context = None;
                if bridge_len != 0 {
                    bridge_len = 0;
                } else {
                    let previous = spans[chunk]
                        .content
                        .chars()
                        .next_back()
                        .expect("nonempty span");
                    previous_chunk = chunk;
                    chunk_start += spans[chunk].content.len();
                    chunk += 1;
                    while spans[chunk].content.is_empty() {
                        chunk += 1;
                    }
                    let next = spans[chunk].content.chars().next().expect("nonempty span");
                    // Overlap prevents chunk-start pre-context from recounting cached scalars.
                    previous.encode_utf8(&mut bridge);
                    next.encode_utf8(&mut bridge[previous.len_utf8()..]);
                    bridge_len = previous.len_utf8() + next.len_utf8();
                    bridge_start = chunk_start - previous.len_utf8();
                }
            }
            Err(GraphemeIncomplete::PreContext(end)) => {
                let (mut context, mut start) = backward_context
                    .filter(|(_, start)| end <= *start)
                    .unwrap_or((chunk, chunk_start));
                while start >= end {
                    context -= 1;
                    start -= spans[context].content.len();
                }
                cursor.provide_context(&spans[context].content[..end - start], start);
                backward_context = Some((context, start));
            }
            Err(error) => unreachable!("valid span cursor: {error:?}"),
        }
    }
    if crossings.is_empty() {
        return;
    }
    for (first, last, prefix) in crossings {
        let extra = line.spans[first + 1..last]
            .iter()
            .map(|span| span.content.len())
            .sum::<usize>()
            + prefix;
        let (before, after) = line.spans.split_at_mut(first + 1);
        let target = &mut before[first].content;
        match target {
            Cow::Borrowed(text) => {
                let mut joined = String::with_capacity(text.len() + extra);
                joined.push_str(text);
                *target = Cow::Owned(joined);
            }
            Cow::Owned(text) => text.reserve(extra),
        }
        let target = target.to_mut();
        for span in &mut after[..last - first - 1] {
            target.push_str(&std::mem::take(&mut span.content));
        }
        let tail = &mut after[last - first - 1].content;
        match std::mem::take(tail) {
            Cow::Borrowed(text) => {
                target.push_str(&text[..prefix]);
                *tail = Cow::Borrowed(&text[prefix..]);
            }
            Cow::Owned(mut text) => {
                target.push_str(&text[..prefix]);
                text.drain(..prefix);
                *tail = Cow::Owned(text);
            }
        }
    }
    line.spans.retain(|span| !span.content.is_empty());
}

/// Text whose graphemes all sit inside one span and paint one cell each. Most
/// transcript text is this, and it needs no grapheme walk to measure or split.
fn is_plain_ascii(text: &str) -> bool {
    text.is_ascii() && !text.bytes().any(|byte| byte.is_ascii_control())
}

fn painted_cells(text: &str) -> usize {
    if is_plain_ascii(text) {
        return text.len();
    }
    crate::tui::components::text::rendered_width(text)
}

pub(super) fn wrap_line_into(mut line: Line<'static>, width: u16, out: &mut Vec<Line<'static>>) {
    use ratatui::buffer::CellWidth;
    use unicode_segmentation::UnicodeSegmentation;

    // A grapheme can only span a span boundary when a span ends inside one, so
    // plain ASCII (which has no combining or joined clusters) skips the join.
    let plain = line.spans.iter().all(|span| is_plain_ascii(&span.content));
    if plain {
        line.spans.retain(|span| !span.content.is_empty());
    } else {
        join_split_graphemes(&mut line);
    }
    let width = width.max(1) as usize;
    let painted_width: usize = line
        .spans
        .iter()
        .map(|span| painted_cells(&span.content))
        .sum();
    if painted_width <= width {
        out.push(line);
        return;
    }
    let emit = |glyphs: &[(usize, std::ops::Range<usize>, usize)], out: &mut Vec<Line<'static>>| {
        if glyphs.is_empty() {
            return;
        }
        let mut spans = Vec::new();
        let mut i = 0;
        while i < glyphs.len() {
            let source = glyphs[i].0;
            let start = glyphs[i].1.start;
            let mut end = glyphs[i].1.end;
            i += 1;
            while i < glyphs.len() && glyphs[i].0 == source {
                end = glyphs[i].1.end;
                i += 1;
            }
            spans.push(Span::styled(
                line.spans[source].content[start..end].to_owned(),
                line.spans[source].style,
            ));
        }
        let mut wrapped = Line::from(spans).style(line.style);
        wrapped.alignment = line.alignment;
        out.push(wrapped);
    };
    let mut row: Vec<(usize, std::ops::Range<usize>, usize)> = Vec::new();
    let mut row_width = 0;
    let mut last_space = None;
    let mut push = |span_index: usize,
                    range: std::ops::Range<usize>,
                    cells: usize,
                    is_space: bool,
                    out: &mut Vec<Line<'static>>| {
        while row_width + cells > width && row_width > 0 {
            if let Some(cut) = last_space {
                emit(&row[..cut], out);
                row.drain(..=cut);
            } else {
                emit(&row, out);
                row.clear();
            }
            row_width = row.iter().map(|glyph| glyph.2).sum();
            last_space = row
                .iter()
                .rposition(|(index, range, _)| &line.spans[*index].content[range.clone()] == " ");
        }
        if is_space {
            last_space = Some(row.len());
        }
        row.push((span_index, range, cells));
        row_width += cells;
        if cells > width {
            emit(&row, out);
            row.clear();
            row_width = 0;
            last_space = None;
        }
    };
    for (span_index, span) in line.spans.iter().enumerate() {
        if plain {
            for (byte, glyph) in span.content.bytes().enumerate() {
                push(span_index, byte..byte + 1, 1, glyph == b' ', out);
            }
            continue;
        }
        for (byte, grapheme) in span.content.grapheme_indices(true) {
            let cells = if grapheme.contains(char::is_control) {
                0
            } else {
                grapheme.cell_width() as usize
            };
            push(
                span_index,
                byte..byte + grapheme.len(),
                cells,
                grapheme == " ",
                out,
            );
        }
    }
    drop(push);
    emit(&row, out);
}

fn render_status(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    active: bool,
) {
    let mut spans: Vec<Span> = Vec::new();
    if let Some(toast) = &state.toast {
        let color = match toast.kind {
            super::state::ToastKind::Info => theme.title,
            super::state::ToastKind::Error => theme.error,
        };
        spans.push(Span::styled(
            format!(" {} ", toast.text),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(
        format!(" {} ", state.session_id),
        Style::default().fg(theme.accent),
    ));
    if let Some(roots) = state.path_roots.as_ref() {
        spans.push(Span::styled(
            format!("· {} ", roots.project_path),
            Style::default().fg(theme.branch),
        ));
    }
    if let Some(agent) = state.transcript.agent_name.as_deref() {
        spans.push(Span::styled(
            format!("· {agent} "),
            Style::default().fg(theme.hint),
        ));
    }
    if let Some(mode) = state.transcript.current_mode.as_deref() {
        spans.push(Span::styled(
            format!("· {mode} "),
            Style::default().fg(theme.title),
        ));
    }
    for toggle in state.transcript.toggles() {
        let on = toggle.current_value == "on";
        spans.push(Span::styled(
            format!("· {} {} ", toggle.name, if on { "on" } else { "off" }),
            Style::default().fg(if on { theme.title } else { theme.hint }),
        ));
    }
    // Label only: the payload can carry the account email, which has no place
    // in a status bar that lives in screenshots and recordings.
    if let Some(auth) = state.transcript.auth_status.as_ref() {
        let color = if auth.kind == AuthStatusKind::None {
            theme.error
        } else {
            theme.hint
        };
        spans.push(Span::styled(
            format!("· {} ", auth.label),
            Style::default().fg(color),
        ));
    }
    if state.transcript.turn_active || state.transcript.background_agent_active {
        let banner = state.transcript.status_text.as_deref().unwrap_or("working");
        spans.push(Span::styled(
            format!("· ● {banner} "),
            Style::default().fg(theme.running),
        ));
    } else if state.ws.is_none() {
        spans.push(Span::styled(
            "· ○ Disconnected ",
            Style::default().fg(theme.error),
        ));
    } else {
        spans.push(Span::styled(
            "· ● Ready ",
            Style::default().fg(theme.running),
        ));
    }
    if state.transcript.context_primer_pending() {
        spans.push(Span::styled(
            " context lost; include needed context ",
            Style::default().fg(theme.error),
        ));
    }
    if state.transcript.lagged {
        spans.push(Span::styled(
            " broadcast lagged; refetching ",
            Style::default().fg(theme.error),
        ));
    }
    if compaction_reminder_due(state) {
        spans.push(Span::styled(
            " context filling; /compact ",
            Style::default().fg(theme.error),
        ));
    }
    if state.scroll_offset.get() != u16::MAX {
        spans.push(Span::styled(
            " · G latest ",
            Style::default().fg(theme.hint),
        ));
    }
    // Plugin status-bar segments and this session's badges; icons, tooltips,
    // and links have no terminal surface.
    let chips = |slot| plugin_ui::by_order(plugin_ui::global_entries(&state.plugin_ui, slot));
    let detail = plugin_ui::by_order(plugin_ui::session_entries(
        &state.plugin_ui,
        UiSlot::DetailBadge,
        &state.session_id,
    ));
    for entry in chips(UiSlot::HeaderStart)
        .into_iter()
        .chain(chips(UiSlot::StatusBar))
        .chain(detail)
    {
        if let Some(text) = plugin_ui::entry_text(entry) {
            spans.push(Span::styled(
                format!(" {text} "),
                plugin_ui::tone_style(plugin_ui::entry_tone(entry), theme),
            ));
        }
    }
    // Right-aligned in its own slice so a long hint can't push it off-screen.
    let mut left_area = area;
    if let Some(usage) = &state.transcript.usage {
        let text = format!(" {} ", format_usage(usage));
        let width = text.chars().count() as u16;
        if area.width > width {
            let pct = usage_percent(usage);
            let color = if pct >= USAGE_WARN_PERCENT {
                theme.error
            } else {
                theme.hint
            };
            let meter_area = Rect {
                x: area.x + area.width - width,
                y: area.y,
                width,
                height: area.height,
            };
            left_area.width = area.width - width;
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(text, Style::default().fg(color)))),
                meter_area,
            );
        }
    }
    let hint = if active {
        help_hint(
            state.focus,
            selected_approval_is_choice(state),
            state.visible_notices().next().is_some(),
        )
    } else {
        " Enter reply · wheel history "
    };
    let hint_width = hint.chars().count() as u16;
    if left_area.width > hint_width.saturating_add(24) {
        let hint_area = Rect {
            x: left_area.x + left_area.width - hint_width,
            y: left_area.y,
            width: hint_width,
            height: left_area.height,
        };
        left_area.width -= hint_width;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme.hint),
            ))),
            hint_area,
        );
    }
    let para = Paragraph::new(Line::from(spans));
    frame.render_widget(para, left_area);
}

const USAGE_WARN_PERCENT: u64 = 90;

/// Rounded context-fill percentage, capped at 100 since agents can report
/// `used > size` transiently (#2927).
fn usage_percent(usage: &SessionUsage) -> u64 {
    if usage.size == 0 {
        return 0;
    }
    (((usage.used as f64 / usage.size as f64) * 100.0).round() as u64).min(100)
}

/// Nudge toward `/compact` at the configured fill, except mid-compaction.
fn compaction_reminder_due(state: &StructuredViewState) -> bool {
    let Some(threshold) = state.compaction_reminder_percent else {
        return false;
    };
    if state.transcript.compacting {
        return false;
    }
    state
        .transcript
        .usage
        .as_ref()
        .is_some_and(|usage| usage.size > 0 && usage_percent(usage) >= u64::from(threshold))
}

/// `12.3k/200k (6%) · $0.42`, matching the web composer.
fn format_usage(usage: &SessionUsage) -> String {
    let mut out = format!(
        "{}/{} ({}%)",
        format_tokens(usage.used),
        format_tokens(usage.size),
        usage_percent(usage)
    );
    if let Some(cost) = &usage.cost {
        let precision = if cost.amount < 1.0 { 4 } else { 2 };
        if cost.currency == "USD" {
            out.push_str(&format!(" · ${:.precision$}", cost.amount));
        } else {
            out.push_str(&format!(" · {:.precision$} {}", cost.amount, cost.currency));
        }
    }
    out
}

fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let precision = usize::from(n < 10_000);
        format!("{:.precision$}k", n as f64 / 1_000.0)
    } else {
        let precision = if n < 10_000_000 { 2 } else { 1 };
        format!("{:.precision$}M", n as f64 / 1_000_000.0)
    }
}

fn render_composer(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    active: bool,
) {
    let context: String = if let Some(recall) = &state.recall {
        let total = state.queue.len();
        let pos = total.saturating_sub(recall.index);
        format!(
            "Editing queued message {pos} of {total} (Enter=save, Esc=restore draft, ↑/↓=browse)"
        )
    } else if active {
        String::new()
    } else {
        "Press Enter to reply".to_string()
    };
    let chrome_rows = COMPOSER_CHROME_ROWS.min(area.height);
    if !context.is_empty() && chrome_rows > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                context,
                Style::default().fg(if state.recall.is_some() {
                    theme.title
                } else {
                    theme.hint
                }),
            ))),
            Rect {
                height: chrome_rows,
                ..area
            },
        );
    }
    let inner = Rect {
        y: area.y.saturating_add(chrome_rows),
        height: area.height.saturating_sub(chrome_rows),
        ..area
    };
    let prompt_width = inner.width.min(2);
    let prompt_area = Rect {
        width: prompt_width,
        ..inner
    };
    let input_area = Rect {
        x: inner.x.saturating_add(prompt_width),
        width: inner.width.saturating_sub(prompt_width),
        ..inner
    };
    let prompt_color = if state.recall.is_some() {
        theme.waiting
    } else if active {
        theme.title
    } else {
        theme.hint
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "› ",
            Style::default()
                .fg(prompt_color)
                .add_modifier(Modifier::BOLD),
        ))),
        prompt_area,
    );
    if input_area.width > 0 {
        frame.render_widget(&state.composer, input_area);
    }
    // A preview must not plant a caret where the keyboard isn't routed.
    if active
        && matches!(state.focus, Focus::Composer)
        && input_area.width > 0
        && input_area.height > 0
    {
        let cursor = state.composer.screen_cursor();
        let max_x = input_area
            .x
            .saturating_add(input_area.width.saturating_sub(1));
        let max_y = input_area
            .y
            .saturating_add(input_area.height.saturating_sub(1));
        let cursor_x = input_area.x.saturating_add(cursor.col as u16).min(max_x);
        let cursor_y = input_area.y.saturating_add(cursor.row as u16).min(max_y);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn user_message_lines<'a>(text: &str, theme: &Theme) -> Vec<Line<'a>> {
    text.split('\n')
        .enumerate()
        .map(|(index, line)| {
            Line::from(vec![
                Span::styled(
                    if index == 0 { "› " } else { "  " },
                    Style::default()
                        .fg(theme.title)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(line.to_string(), Style::default().fg(theme.text)),
            ])
        })
        .collect()
}

fn agent_message_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    render_agent_message_lines(text)
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            line.spans.insert(
                0,
                Span::styled(
                    if index == 0 { "• " } else { "  " },
                    Style::default().fg(theme.hint),
                ),
            );
            line
        })
        .collect()
}

/// Agent reply as theme-neutral markdown, or `…` when it renders empty.
fn render_agent_message_lines(text: &str) -> Vec<Line<'static>> {
    let body = crate::tui::markdown::render(text);
    if text.trim().is_empty() || body.is_empty() {
        return vec![Line::from("…".to_string())];
    }
    body
}

/// Project the daemon's transcript rows to lines. Approvals are control state
/// shown in the shelf, not here.
fn transcript_lines(
    transcript: &AcpTranscript,
    theme: &Theme,
    path_roots: Option<&SessionPathRoots>,
    full_thinking: bool,
    tool_output: ToolOutputMode,
) -> TranscriptText {
    let rows = transcript.rows();
    // One card per tool call at its start row; the last terminal row wins.
    let mut completions: HashMap<&str, &TranscriptRow> = HashMap::new();
    for row in rows {
        if is_tool_terminal(row.kind) {
            if let Some(id) = row.tool_call_id.as_deref() {
                completions.insert(id, row);
            }
        }
    }

    // Child tool calls nest under a parent card present in the window; others
    // stay at top level.
    let starts: HashSet<&str> = rows
        .iter()
        .filter(|row| row.kind == TranscriptRowKind::ToolStart)
        .filter_map(|row| row.tool_call_id.as_deref())
        .collect();
    let mut children: HashMap<&str, Vec<&TranscriptRow>> = HashMap::new();
    for row in rows
        .iter()
        .filter(|r| r.kind == TranscriptRowKind::ToolStart)
    {
        if let Some(parent) = in_window_parent(row, &starts) {
            children.entry(parent).or_default().push(row);
        }
    }
    let blocks = ToolBlocks {
        completions: &completions,
        children: &children,
        subagents: transcript.subagents(),
        theme,
        path_roots,
        tool_output,
    };

    let mut out = TranscriptText::new();
    let mut i = 0;
    while i < rows.len() {
        let row = &rows[i];
        out.row_of(group_key(row));
        match row.kind {
            TranscriptRowKind::Message => {
                // Consecutive chunks of one group form one bubble.
                let group = &row.group_id;
                let mut text = String::new();
                while i < rows.len()
                    && rows[i].kind == TranscriptRowKind::Message
                    && &rows[i].group_id == group
                {
                    text.push_str(&rows[i].text);
                    i += 1;
                }
                out.extend(agent_message_lines(&text, theme));
                out.push(Line::default());
                continue;
            }
            TranscriptRowKind::Thinking => {
                let group = &row.group_id;
                let mut text = String::new();
                while i < rows.len()
                    && rows[i].kind == TranscriptRowKind::Thinking
                    && &rows[i].group_id == group
                {
                    text.push_str(&rows[i].text);
                    i += 1;
                }
                if full_thinking {
                    out.extend(thinking_lines(&text, theme));
                } else {
                    out.push(thinking_preview_line(&text));
                }
                out.push(Line::default());
                continue;
            }
            TranscriptRowKind::UserPrompt => {
                // Note attachments so an image-only prompt doesn't look empty.
                let mut text = match &row.origin_plugin {
                    Some(plugin) => format!("[plugin {plugin}] {}", row.text),
                    None => row.text.clone(),
                };
                if !row.attachments.is_empty() {
                    text = format!("{text} [{} attachment(s)]", row.attachments.len());
                }
                out.extend(user_message_lines(&text, theme));
                out.push(Line::default());
            }
            TranscriptRowKind::UserDiffComments => {
                // The assembled markdown the agent received reads as a prompt.
                out.extend(user_message_lines(&row.text, theme));
                out.push(Line::default());
            }
            TranscriptRowKind::ToolStart => {
                if in_window_parent(row, &starts).is_none() {
                    out.extend(blocks.lines(row));
                    out.push(Line::default());
                }
            }
            TranscriptRowKind::ToolComplete
            | TranscriptRowKind::ToolError
            | TranscriptRowKind::ToolStopped => {
                // Rendered with their `tool_start`, which the server always provides.
            }
            TranscriptRowKind::ElicitationAnswered => {
                if row.elicitation_answers.is_empty() {
                    out.extend(user_message_lines(&row.text, theme));
                } else {
                    for answer in &row.elicitation_answers {
                        out.extend(user_message_lines(
                            &format!("{}: {}", answer.question, answer.answer),
                            theme,
                        ));
                    }
                }
                out.push(Line::default());
            }
            // Empty until the summary arrives; the row only anchors its position.
            TranscriptRowKind::CompactionSummary => {
                if !row.text.is_empty() {
                    out.push(note_line(NoteKind::Info, "Compaction summary"));
                    out.extend(agent_message_lines(&row.text, theme));
                    out.push(Line::default());
                }
            }
            TranscriptRowKind::EmptyOutput
            | TranscriptRowKind::ContextReset
            | TranscriptRowKind::SessionCleared
            | TranscriptRowKind::Compacted
            | TranscriptRowKind::Summary
            | TranscriptRowKind::Notice
            | TranscriptRowKind::Advisory => {
                let kind = match row.kind {
                    // Failures the user must see.
                    TranscriptRowKind::Notice => NoteKind::Error,
                    TranscriptRowKind::ContextReset | TranscriptRowKind::SessionCleared => {
                        NoteKind::Warning
                    }
                    _ => NoteKind::Info,
                };
                out.push(note_line(kind, &row.text));
                out.push(Line::default());
            }
        }
        i += 1;
    }
    if let ToolOutputMode::Retained(outputs) = tool_output {
        for output in outputs {
            out.push(Line::styled(
                format!(
                    "Retained tool output · completion unavailable · {}",
                    output.tool_call_id
                ),
                Style::default().fg(theme.hint),
            ));
            out.extend(styled_output_lines(&output.text));
            out.push(Line::default());
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "(no events yet, waiting for the agent…)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    out
}

/// The identity a transcript row's lines belong to: the group its chunks share,
/// so one bubble stays one anchor, or the row's own id.
fn group_key(row: &TranscriptRow) -> String {
    match row.kind {
        TranscriptRowKind::Message | TranscriptRowKind::Thinking => row.group_id.clone(),
        _ => row.id.clone(),
    }
}

/// The spawning tool call of `row`, when its card is in the window.
fn in_window_parent<'a>(row: &'a TranscriptRow, starts: &HashSet<&str>) -> Option<&'a str> {
    let parent = row.tool.as_ref()?.parent_tool_call_id.as_deref()?;
    (starts.contains(parent) && row.tool_call_id.as_deref() != Some(parent)).then_some(parent)
}

/// What nested tool rendering needs, shared across the recursion.
struct ToolBlocks<'a> {
    completions: &'a HashMap<&'a str, &'a TranscriptRow>,
    children: &'a HashMap<&'a str, Vec<&'a TranscriptRow>>,
    subagents: &'a [SubagentRecord],
    theme: &'a Theme,
    path_roots: Option<&'a SessionPathRoots>,
    tool_output: ToolOutputMode<'a>,
}

impl ToolBlocks<'_> {
    /// A tool card, then its child calls indented below it.
    fn lines(&self, row: &TranscriptRow) -> Vec<Line<'static>> {
        let id = row.tool_call_id.as_deref();
        let record = id.and_then(|id| {
            self.subagents
                .iter()
                .find(|record| record.parent_tool_call_id.as_deref() == Some(id))
        });
        let mut lines = if matches!(self.tool_output, ToolOutputMode::Retained(_)) {
            let completion = id.and_then(|id| self.completions.get(id)).copied();
            retained_tool_lines(row, completion, self.theme, self.path_roots)
        } else {
            let mut card = tool_card_from_rows(row, self.completions);
            // The roster, not the launch call, says whether the child is still
            // active: a stop at turn end must not close a running child's card,
            // and an idle child may never send a reply.
            if let Some(record) = record {
                let stopped = matches!(
                    &card.completed,
                    Some(c) if c.outcome == ToolOutcome::Stopped
                );
                match record.state {
                    SubagentState::Running | SubagentState::RequiresAction if stopped => {
                        card.completed = None;
                    }
                    SubagentState::Idle if card.completed.is_none() || stopped => {
                        card.completed = Some(ToolCompletion {
                            outcome: match record.stop_reason.as_deref() {
                                Some("error") => ToolOutcome::Error,
                                Some("cancelled") => ToolOutcome::Stopped,
                                _ => ToolOutcome::Ok,
                            },
                            content: String::new(),
                        });
                    }
                    _ => {}
                }
            }
            if let Some(title) = record
                .and_then(|r| r.title.as_ref())
                .filter(|t| !t.is_empty())
            {
                card.name = title.clone();
            }
            render_tool_lines(&card, self.theme, self.path_roots)
        };
        if let (Some(record), Some(header)) = (record, lines.first_mut()) {
            header.spans.push(subagent_badge(record.state, self.theme));
        }
        for child in id
            .and_then(|id| self.children.get(id))
            .into_iter()
            .flatten()
        {
            for mut line in self.lines(child) {
                line.spans.insert(0, Span::raw("  "));
                lines.push(line);
            }
        }
        lines
    }
}

fn subagent_badge(state: SubagentState, theme: &Theme) -> Span<'static> {
    let (text, color) = match state {
        SubagentState::Running => ("running", theme.running),
        SubagentState::RequiresAction => ("waiting on you", theme.waiting),
        SubagentState::Idle => ("idle", theme.hint),
        SubagentState::Unknown => ("unknown", theme.hint),
    };
    Span::styled(format!(" · {text}"), Style::default().fg(color))
}

/// A thought in full, dimmed so it never competes with the agent's answer.
fn thinking_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    // A thought is quoted, not formatted: markup the parser would swallow as
    // HTML still reads as the model wrote it.
    let body = crate::tui::markdown::render_keeping_html(text);
    if text.trim().is_empty() || body.is_empty() {
        return vec![Line::from(Span::styled(
            "· Thinking: …",
            Style::default()
                .fg(theme.hint)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ))];
    }
    body.into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            line.spans.insert(
                0,
                Span::styled(
                    if index == 0 { "· " } else { "  " },
                    Style::default().fg(theme.hint),
                ),
            );
            line.style = line.style.add_modifier(Modifier::DIM);
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::DIM);
            }
            line
        })
        .collect()
}

/// One dim preview per thought, for when the transcript is collapsed.
fn thinking_preview_line(text: &str) -> Line<'static> {
    const MAX_CHARS: usize = 120;
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut preview: String = first.chars().take(MAX_CHARS).collect();
    if preview.len() < text.trim().len() {
        preview.push('…');
    }
    Line::from(Span::styled(
        format!("· Thinking: {preview}"),
        Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
    ))
}

fn is_tool_terminal(kind: TranscriptRowKind) -> bool {
    matches!(
        kind,
        TranscriptRowKind::ToolComplete
            | TranscriptRowKind::ToolError
            | TranscriptRowKind::ToolStopped
    )
}

fn note_line(kind: NoteKind, text: &str) -> Line<'static> {
    let modifier = match kind {
        NoteKind::Info => Modifier::DIM,
        NoteKind::Warning | NoteKind::Error => Modifier::BOLD,
    };
    Line::from(Span::styled(
        format!("· {text}"),
        Style::default().add_modifier(modifier),
    ))
}

fn retained_tool_lines(
    start: &TranscriptRow,
    completion: Option<&TranscriptRow>,
    theme: &Theme,
    path_roots: Option<&SessionPathRoots>,
) -> Vec<Line<'static>> {
    let marker = match completion.map(|row| row.kind) {
        Some(TranscriptRowKind::ToolComplete) => "✓ ",
        Some(TranscriptRowKind::ToolError) => "✗ ",
        Some(TranscriptRowKind::ToolStopped) => "◼ ",
        _ => "",
    };
    let mut lines = vec![Line::styled(
        format!("tool {marker}· {}", start.text),
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    )];
    if let Some(tool) = &start.tool {
        lines.extend(styled_output_lines(&tool.args_preview));
        for diff in &tool.diffs {
            let path = relative_display_path(&diff.path, path_roots);
            if let Some(before) = &diff.old_text {
                lines.push(Line::from(format!("Before: {path}")));
                lines.extend(styled_output_lines(before));
            }
            if let Some(after) = &diff.new_text {
                lines.push(Line::from(format!("After: {path}")));
                lines.extend(styled_output_lines(after));
            }
        }
    }
    if let Some(completion) = completion {
        if completion.async_subagent {
            lines.push(Line::from("runs in background"));
        } else if completion.output.is_empty() {
            lines.extend(styled_output_lines(&completion.text));
        } else {
            let has_text = completion.output.iter().any(|block| match block {
                ToolOutputBlock::Text { text }
                | ToolOutputBlock::Resource {
                    text: Some(text), ..
                } => !text.is_empty(),
                _ => false,
            });
            if !has_text && !completion.text.is_empty() {
                lines.extend(styled_output_lines(&completion.text));
            }
            lines.extend(styled_output_lines(&summarize_output_blocks(
                &completion.output,
            )));
        }
    }
    lines
}

fn tool_card_from_rows(
    start: &TranscriptRow,
    completions: &HashMap<&str, &TranscriptRow>,
) -> ToolCallRow {
    let tool = start.tool.as_ref();
    let completed = start
        .tool_call_id
        .as_deref()
        .and_then(|id| completions.get(id))
        .map(|term| tool_completion_from_row(term));
    ToolCallRow {
        name: tool
            .map(|t| t.name.clone())
            .unwrap_or_else(|| start.text.clone()),
        kind: tool.map(|t| t.kind.clone()).unwrap_or_default(),
        args: tool.map(|t| t.args_preview.clone()).unwrap_or_default(),
        diffs: tool.map(|t| t.diffs.clone()).unwrap_or_default(),
        completed,
    }
}

/// Only `tool_complete` is ok. An async sub-agent launch hides its internal id.
fn tool_completion_from_row(term: &TranscriptRow) -> ToolCompletion {
    let content = if term.async_subagent {
        "runs in background".to_string()
    } else if term.output.is_empty() {
        term.text.clone()
    } else {
        summarize_output_blocks(&term.output)
    };
    ToolCompletion {
        outcome: match term.kind {
            TranscriptRowKind::ToolComplete => ToolOutcome::Ok,
            TranscriptRowKind::ToolStopped => ToolOutcome::Stopped,
            _ => ToolOutcome::Error,
        },
        content,
    }
}

/// Text summary of output blocks; media becomes a placeholder.
fn summarize_output_blocks(blocks: &[ToolOutputBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ToolOutputBlock::Text { text } => text.clone(),
            ToolOutputBlock::Image { mime_type, .. } => format!("[image {mime_type}]"),
            ToolOutputBlock::Audio { mime_type, .. } => format!("[audio {mime_type}]"),
            ToolOutputBlock::ResourceLink { name, uri, .. } => format!("[link {name}: {uri}]"),
            ToolOutputBlock::Resource {
                uri,
                text: Some(text),
                ..
            } => format!("{text}\n[resource {uri}]"),
            ToolOutputBlock::Resource {
                uri, text: None, ..
            } => format!("[resource {uri}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first `max_chars` chars, or `None` when already short enough.
fn truncate_chars(s: &str, max_chars: usize) -> Option<String> {
    s.char_indices()
        .nth(max_chars)
        .map(|(byte_idx, _)| s[..byte_idx].to_string())
}

/// Edit payload variants shared with the web tool cards.
const OLD_KEYS: &[&str] = &["old_string", "oldString", "old_str"];
const NEW_KEYS: &[&str] = &["new_string", "newString", "new_str", "content"];

const TOOL_DIFF_MAX_LINES: usize = 20;
const TOOL_PREVIEW_MAX_LINES: usize = 12;

/// Successful tools collapse to one line; others get a per-kind body, or the
/// generic fallback for unknown kinds and unparsable args.
fn render_tool_lines(
    tool: &ToolCallRow,
    theme: &Theme,
    path_roots: Option<&SessionPathRoots>,
) -> Vec<Line<'static>> {
    if tool
        .completed
        .as_ref()
        .is_some_and(|completion| completion.outcome == ToolOutcome::Ok)
    {
        return vec![compact_tool_line(tool, theme, path_roots)];
    }
    let mut lines = Vec::new();
    let header = format!(
        "tool {} · {}",
        match tool.completed.as_ref() {
            None => "▶",
            Some(c) => match c.outcome {
                ToolOutcome::Ok => "✓",
                ToolOutcome::Stopped => "◼",
                ToolOutcome::Error => "✗",
            },
        },
        tool.name
    );
    lines.push(Line::from(Span::styled(
        header,
        Style::default().add_modifier(Modifier::BOLD),
    )));

    // Structured per-file diffs win over args-derived diffs.
    if !tool.diffs.is_empty() {
        lines.extend(render_structured_diffs(&tool.diffs, theme, path_roots));
        return lines;
    }

    let args = parse_args_object(&tool.args);
    let body = match tool.kind.as_str() {
        "edit" | "write" => render_edit_body(args.as_ref(), theme, path_roots),
        "execute" => render_execute_body(args.as_ref(), tool),
        "read" => render_read_body(args.as_ref(), tool, path_roots),
        "delete" => render_delete_body(args.as_ref(), path_roots),
        _ => None,
    };
    lines.extend(body.unwrap_or_else(|| render_generic_body(tool)));
    lines
}

fn compact_tool_line(
    tool: &ToolCallRow,
    theme: &Theme,
    path_roots: Option<&SessionPathRoots>,
) -> Line<'static> {
    let args = parse_args_object(&tool.args);
    let target = if let Some(diff) = tool.diffs.first() {
        Some(relative_display_path(&diff.path, path_roots))
    } else {
        display_tool_target(&tool.kind, args.as_ref(), path_roots)
    };
    let mut spans = vec![
        Span::styled("✓ ", Style::default().fg(theme.running)),
        Span::styled(
            tool.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(target) = target.filter(|target| !target.is_empty()) {
        spans.push(Span::styled(
            format!(" · {target}"),
            Style::default().fg(theme.hint),
        ));
    }
    let (added, removed) = tool_diff_counts(tool, args.as_ref());
    if added > 0 || removed > 0 {
        spans.push(Span::styled(
            format!(" · +{added} -{removed}"),
            Style::default().fg(theme.hint),
        ));
    }
    Line::from(spans)
}

fn tool_diff_counts(
    tool: &ToolCallRow,
    args: Option<&serde_json::Map<String, serde_json::Value>>,
) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    let mut count = |old: &str, new: &str| {
        for change in TextDiff::from_lines(old, new).iter_all_changes() {
            match change.tag() {
                ChangeTag::Insert => added += 1,
                ChangeTag::Delete => removed += 1,
                ChangeTag::Equal => {}
            }
        }
    };
    if tool.diffs.is_empty() {
        if let Some(new) = pick_str(args, NEW_KEYS) {
            count(pick_str(args, OLD_KEYS).unwrap_or(""), new);
        }
    } else {
        for diff in &tool.diffs {
            count(
                diff.old_text.as_deref().unwrap_or(""),
                diff.new_text.as_deref().unwrap_or(""),
            );
        }
    }
    (added, removed)
}

fn render_structured_diffs(
    diffs: &[crate::acp::state::DiffPreview],
    theme: &Theme,
    path_roots: Option<&SessionPathRoots>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for diff in diffs {
        let path = relative_display_path(&diff.path, path_roots);
        out.push(Line::from(format!("  {path}")));
        out.extend(diff_lines(
            diff.old_text.as_deref().unwrap_or(""),
            diff.new_text.as_deref().unwrap_or(""),
            theme,
        ));
    }
    out
}

/// `None` for non-object or truncated payloads, like the web `parseJsonObject`.
fn parse_args_object(args: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn pick_str<'a>(
    args: Option<&'a serde_json::Map<String, serde_json::Value>>,
    keys: &[&str],
) -> Option<&'a str> {
    let args = args?;
    keys.iter().find_map(|k| match args.get(*k) {
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        _ => None,
    })
}

/// `None` without an after-text arg, so the generic renderer takes over.
fn render_edit_body(
    args: Option<&serde_json::Map<String, serde_json::Value>>,
    theme: &Theme,
    path_roots: Option<&SessionPathRoots>,
) -> Option<Vec<Line<'static>>> {
    let new = pick_str(args, NEW_KEYS)?;
    let old = pick_str(args, OLD_KEYS).unwrap_or("");
    if old.is_empty() && new.is_empty() {
        return None;
    }
    let path = relative_display_path(
        pick_str(args, PATH_KEYS).unwrap_or("(unknown file)"),
        path_roots,
    );
    let mut lines = vec![Line::from(format!("  {path}"))];
    lines.extend(diff_lines(old, new, theme));
    Some(lines)
}

/// Only changed lines, capped at `TOOL_DIFF_MAX_LINES`.
fn diff_lines(old: &str, new: &str, theme: &Theme) -> Vec<Line<'static>> {
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    let mut hidden = 0usize;
    for change in diff.iter_all_changes() {
        let (sign, style) = match change.tag() {
            ChangeTag::Delete => ("-", Style::default().fg(theme.diff_delete)),
            ChangeTag::Insert => ("+", Style::default().fg(theme.diff_add)),
            ChangeTag::Equal => continue,
        };
        if out.len() >= TOOL_DIFF_MAX_LINES {
            hidden += 1;
            continue;
        }
        let text = change.value();
        let text = text.strip_suffix('\n').unwrap_or(text);
        out.push(Line::from(Span::styled(format!("  {sign} {text}"), style)));
    }
    if hidden > 0 {
        out.push(Line::from(Span::styled(
            format!("  … +{hidden} more diff lines; press `o` for full"),
            Style::default().fg(theme.dimmed),
        )));
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "  (no textual changes)",
            Style::default().fg(theme.dimmed),
        )));
    }
    out
}

fn render_execute_body(
    args: Option<&serde_json::Map<String, serde_json::Value>>,
    tool: &ToolCallRow,
) -> Option<Vec<Line<'static>>> {
    let command = pick_str(args, CMD_KEYS)?;
    let cmd_lines: Vec<&str> = command.lines().collect();
    let mut lines = vec![Line::from(format!(
        "  $ {}",
        cmd_lines.first().copied().unwrap_or("")
    ))];
    if cmd_lines.len() > 1 {
        lines.push(Line::from(Span::styled(
            format!("    (+{} more command lines)", cmd_lines.len() - 1),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    lines.extend(output_preview_lines(tool));
    Some(lines)
}

fn render_read_body(
    args: Option<&serde_json::Map<String, serde_json::Value>>,
    tool: &ToolCallRow,
    path_roots: Option<&SessionPathRoots>,
) -> Option<Vec<Line<'static>>> {
    let path = relative_display_path(pick_str(args, PATH_KEYS)?, path_roots);
    let mut lines = vec![Line::from(format!("  {path}"))];
    lines.extend(output_preview_lines(tool));
    Some(lines)
}

fn render_delete_body(
    args: Option<&serde_json::Map<String, serde_json::Value>>,
    path_roots: Option<&SessionPathRoots>,
) -> Option<Vec<Line<'static>>> {
    let path = relative_display_path(pick_str(args, PATH_KEYS)?, path_roots);
    Some(vec![Line::from(format!("  {path}"))])
}

/// A stopped call was closed by the turn-end sweep, not a failure.
fn empty_output_note(completion: &ToolCompletion) -> &'static str {
    match completion.outcome {
        ToolOutcome::Ok => "  (no output)",
        ToolOutcome::Stopped => "  (stopped when the turn ended)",
        ToolOutcome::Error => "  (tool failed; press `o` for details)",
    }
}

fn output_preview_lines(tool: &ToolCallRow) -> Vec<Line<'static>> {
    let Some(completion) = &tool.completed else {
        return vec![Line::from(Span::styled(
            "  (running…)",
            Style::default().add_modifier(Modifier::DIM),
        ))];
    };
    if completion.content.is_empty() {
        return vec![Line::from(empty_output_note(completion).to_string())];
    }
    let mut out = Vec::new();
    let styled = styled_output_lines(&completion.content);
    let total = styled.len();
    for mut line in styled.into_iter().take(TOOL_PREVIEW_MAX_LINES) {
        line.spans.insert(0, Span::raw("  "));
        out.push(line);
    }
    if total > TOOL_PREVIEW_MAX_LINES {
        out.push(Line::from(Span::styled(
            format!(
                "  … +{} more lines; press `o` for full",
                total - TOOL_PREVIEW_MAX_LINES
            ),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    out
}

/// Tool output lines with ANSI SGR styling applied, raw text on parse failure.
fn styled_output_lines(content: &str) -> Vec<Line<'static>> {
    if content.contains('\u{1b}') {
        if let Ok(text) = content.into_text() {
            return text.lines;
        }
    }
    content.lines().map(|l| Line::from(l.to_string())).collect()
}

/// Fallback for unknown kinds: truncated args and output.
fn render_generic_body(tool: &ToolCallRow) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !tool.args.is_empty() {
        let truncated = match truncate_chars(&tool.args, 200) {
            Some(head) => format!("  $ {head}…"),
            None => format!("  $ {}", tool.args),
        };
        lines.push(Line::from(truncated));
    }
    if let Some(completion) = &tool.completed {
        if completion.content.is_empty() {
            lines.push(Line::from(empty_output_note(completion).to_string()));
        } else {
            let (body, truncated) = match truncate_chars(&completion.content, 400) {
                Some(head) => (head, true),
                None => (completion.content.clone(), false),
            };
            for mut line in styled_output_lines(&body) {
                line.spans.insert(0, Span::raw("  "));
                lines.push(line);
            }
            if truncated {
                lines.push(Line::from(
                    "  … (output truncated; press `o` for full)".to_string(),
                ));
            }
        }
    }
    lines
}

fn selected_approval_is_choice(state: &StructuredViewState) -> bool {
    let Some(selected) = state.selected_approval.as_deref() else {
        return false;
    };
    state
        .transcript
        .pending_approvals
        .iter()
        .any(|pending| pending.nonce == selected && pending.choice && !pending.options.is_empty())
}

fn help_hint(focus: Focus, approval_is_choice: bool, has_notices: bool) -> &'static str {
    match focus {
        Focus::Composer => " Enter to send · Ctrl+Q to exit ",
        // `render_status` drops the hint unless it has `len + 24` spare columns,
        // so keep these short.
        Focus::Transcript if has_notices => " x dismiss · scroll · p pane · Ctrl+Q exit ",
        Focus::Transcript => " scroll · p pane · Ctrl+Q exit ",
        Focus::Approval if approval_is_choice => " a answer · d deny · Esc stop ",
        Focus::Approval => " a allow · A always · d deny · Esc stop ",
        Focus::Pane => " scroll to read · Esc to close ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::HttpClient;
    use crate::acp::state::{AvailableCommand, Event, SessionNotice};
    use crate::daemon::{DaemonEndpoint, Source};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn test_state() -> StructuredViewState {
        let endpoint = DaemonEndpoint::new("http://127.0.0.1:8080".into(), None, Source::Env);
        let http = HttpClient::new(endpoint.clone()).unwrap();
        StructuredViewState::new("s-1".into(), endpoint, http, None)
    }

    /// #4242: the strip is the TUI's only dismissible advisory surface, and the
    /// transcript row must outlive a dismissal so the history stays complete.
    #[test]
    fn session_notices_render_in_a_strip_and_dismiss_locally() {
        let mut state = test_state();
        state.transcript.session_notices = vec![
            SessionNotice {
                id: "notice-1".into(),
                severity: "warning".into(),
                title: "Model fallback".into(),
                description: Some("Switched to Sonnet.".into()),
            },
            SessionNotice {
                id: "notice-2".into(),
                severity: "info".into(),
                title: "Fast mode turned off".into(),
                description: None,
            },
        ];
        state
            .transcript
            .merge_server_rows(server_rows(&[Event::SessionNotice {
                severity: "warning".into(),
                title: "Model fallback".into(),
                description: Some("Switched to Sonnet.".into()),
                key: None,
            }]));

        // One strip row per undismissed notice, each with its own close target.
        let strip_rows = |state: &StructuredViewState| -> Vec<String> {
            render_rows(state, 80, 24, true)
                .into_iter()
                .filter(|row| row.contains('×'))
                .collect()
        };

        let rows = strip_rows(&state);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].contains("Model fallback"), "{rows:?}");
        assert!(rows[1].contains("Fast mode turned off"), "{rows:?}");
        assert!(
            help_hint(Focus::Transcript, false, true).contains("x dismiss"),
            "the key is advertised while a notice is up"
        );

        state.dismissed_notices.insert("notice-1".into());
        let rows = strip_rows(&state);
        assert_eq!(rows.len(), 1, "the dismissed notice leaves the strip");
        assert!(rows[0].contains("Fast mode turned off"), "{rows:?}");
        let painted = render_rows(&state, 80, 24, true).join("\n");
        assert!(
            painted.contains("· warning: Model fallback: Switched to Sonnet."),
            "the transcript row survives the dismissal: {painted}"
        );

        // A narrow terminal clips the text rather than painting under the `×`.
        let narrow = render_rows(&state, 24, 24, true);
        assert!(
            narrow.iter().any(|row| row.contains('×')),
            "the close target survives a narrow frame: {narrow:?}"
        );

        // The daemon retiring the notice must not leave the id behind.
        state.transcript.session_notices.clear();
        state.prune_dismissed_notices();
        assert!(state.dismissed_notices.is_empty());
    }

    #[test]
    fn notice_policy_hides_advisories_but_never_errors() {
        use crate::session::config::{NoticeDisplay, NoticePolicy};

        let notice = |id: &str, severity: &str| SessionNotice {
            id: id.into(),
            severity: severity.into(),
            title: id.into(),
            description: None,
        };
        let mut state = test_state();
        state.transcript.session_notices = vec![
            notice("info", "info"),
            notice("warning", "warning"),
            notice("error", "error"),
        ];
        let visible = |state: &StructuredViewState| -> Vec<String> {
            state.visible_notices().map(|n| n.id.clone()).collect()
        };

        assert_eq!(visible(&state), ["info", "warning", "error"]);

        for display in [NoticeDisplay::Hidden, NoticeDisplay::Timeout] {
            state.notice_policy = NoticePolicy {
                display,
                timeout_secs: 0,
            };
            assert_eq!(visible(&state), ["error"], "{display:?}");
        }

        state.notice_policy = NoticePolicy {
            display: NoticeDisplay::Timeout,
            timeout_secs: 3600,
        };
        assert_eq!(visible(&state), ["info", "warning", "error"]);
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn joined(lines: &[Line]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    fn render_rows(state: &StructuredViewState, w: u16, h: u16, active: bool) -> Vec<String> {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal
            .draw(|f| {
                render(f, f.area(), &theme, state, active);
            })
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(w as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    fn render_dump(state: &StructuredViewState, w: u16, h: u16) -> String {
        render_rows(state, w, h, true).concat()
    }

    /// Render once and report where the transcript landed, so a test can check
    /// the row the pane starts on.
    fn render_geometry(state: &StructuredViewState, w: u16, h: u16) -> TranscriptGeometry {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        let mut geometry = None;
        terminal
            .draw(|f| {
                geometry = Some(render(f, f.area(), &theme, state, true));
            })
            .expect("draw");
        geometry.expect("geometry")
    }

    /// Server transcript rows folded from events, as the daemon ships them.
    fn server_rows(events: &[Event]) -> Vec<TranscriptRow> {
        let mut m = crate::acp::transcript::TranscriptModel::new();
        for (i, e) in events.iter().enumerate() {
            m.apply_event(i as u64 + 1, e);
        }
        m.rows().to_vec()
    }

    fn cmd(name: &str, desc: &str) -> AvailableCommand {
        AvailableCommand {
            name: name.to_string(),
            description: desc.to_string(),
            accepts_input: false,
            input_hint: None,
        }
    }

    fn usage(used: u64, size: u64) -> SessionUsage {
        SessionUsage {
            used,
            size,
            cost: None,
        }
    }

    #[test]
    fn layout_heights() {
        let mut state = test_state();
        assert_eq!(queued_strip_height(&state), 0);
        for n in 1..=5 {
            state.queue.push(format!("q{n}"));
            assert_eq!(queued_strip_height(&state), 1);
        }

        assert_eq!(composer_height(&state), 2);
        state.composer.insert_newline();
        state.composer.insert_newline();
        assert_eq!(composer_height(&state), 4);
        for _ in 0..10 {
            state.composer.insert_newline();
        }
        assert_eq!(
            composer_height(&state),
            COMPOSER_MAX_CONTENT_ROWS + COMPOSER_CHROME_ROWS
        );
    }

    #[test]
    fn approval_actions_and_hints() {
        use crate::acp::protocol::ApprovalDecisionWire as Decision;

        // Each clickable span must cover exactly its hint and fire its key's
        // intent; a (choice, want) row per shelf variant.
        for (choice, want) in [
            (
                false,
                vec![
                    ("a allow once", Intent::ResolveApproval(Decision::Allow)),
                    ("A always", Intent::ResolveApproval(Decision::AllowAlways)),
                    ("d deny", Intent::ResolveApproval(Decision::Deny)),
                    ("Esc stop", Intent::CancelInFlight),
                ],
            ),
            // A question offers answering instead of the permission vocabulary (#3741).
            (
                true,
                vec![
                    ("a answer", Intent::ResolveApproval(Decision::Allow)),
                    ("d deny", Intent::ResolveApproval(Decision::Deny)),
                    ("Esc stop", Intent::CancelInFlight),
                ],
            ),
        ] {
            let (line, buttons) = approval_actions_line(&Theme::default(), true, choice);
            let text = line_text(&line);
            assert!(
                !text.contains('[') && !text.contains(']'),
                "button chrome: {text:?}"
            );
            let got: Vec<(String, Intent)> = buttons
                .into_iter()
                .map(|(offset, width, intent)| {
                    let label = text
                        .chars()
                        .skip(offset as usize)
                        .take(width as usize)
                        .collect();
                    (label, intent)
                })
                .collect();
            let want: Vec<(String, Intent)> =
                want.into_iter().map(|(l, i)| (l.to_string(), i)).collect();
            assert_eq!(got, want, "choice={choice}");
        }
        // The preview shelf has no keyboard, so nothing is clickable.
        assert!(approval_actions_line(&Theme::default(), false, false)
            .1
            .is_empty());
        assert!(help_hint(Focus::Approval, true, false).contains("a answer"));
        assert!(!help_hint(Focus::Approval, true, false).contains("always"));
        assert!(help_hint(Focus::Approval, false, false).contains("A always"));
    }

    #[test]
    fn wrapping_keeps_cross_style_ansi_graphemes_in_paint_and_copy() {
        use crate::tui::components::text::line_columns;
        use unicode_segmentation::UnicodeSegmentation;

        for text in [
            "Ae\u{301}XYZ",
            "A♥\u{fe0f}x",
            "A👩\u{200d}💻x",
            "A👩\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{200d}💻x",
            "A🇫🇷🇨🇦x",
            "Aक्\u{200d}षx",
            "Aｶﾞx",
        ] {
            for group in 1..=text.chars().count() {
                let mut ansi = String::new();
                for (index, ch) in text.chars().enumerate() {
                    let color = if (index / group) % 2 == 0 { 31 } else { 34 };
                    ansi.push_str(&format!("\x1b[{color}m{ch}"));
                }
                let expected: Vec<_> = text
                    .grapheme_indices(true)
                    .map(|(byte, glyph)| {
                        let index = text[..byte].chars().count();
                        let color = if (index / group) % 2 == 0 {
                            Color::Red
                        } else {
                            Color::Blue
                        };
                        (glyph.to_string(), color)
                    })
                    .collect();
                for width in [4, 7, 80] {
                    let mut line = styled_output_lines(&ansi)
                        .remove(0)
                        .style(Style::default().bg(Color::Green));
                    line.spans.insert(0, Span::raw(""));
                    line.spans.push(Span::raw(""));
                    let mut rows = Vec::new();
                    wrap_line_into(line, width, &mut rows);
                    let copied = rows
                        .iter()
                        .map(|row| line_columns(row, width).slice(0, width))
                        .collect::<String>();
                    assert_eq!(copied, text, "width={width}, group={group}");
                    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
                        width,
                        rows.len() as u16,
                    ))
                    .unwrap();
                    terminal
                        .draw(|frame| {
                            frame.render_widget(Paragraph::new(Text::from(rows)), frame.area());
                        })
                        .unwrap();
                    let painted: Vec<_> = terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .filter(|cell| cell.symbol() != " ")
                        .map(|cell| {
                            assert_eq!(cell.bg, Color::Green);
                            (cell.symbol().to_owned(), cell.fg)
                        })
                        .collect();
                    assert_eq!(
                        painted, expected,
                        "text={text:?}, width={width}, group={group}"
                    );
                }
            }
        }
    }

    #[test]
    fn wrapping_preserves_painted_and_copied_graphemes() {
        use crate::tui::components::text::line_columns;
        for (text, width) in [
            ("a".repeat(31) + "♥️x", 32),
            ("a".repeat(30) + "ｶﾞx", 32),
            ("a".repeat(31) + "e\u{301}x", 32),
            ("a".repeat(31) + "👩‍💻x", 32),
            (" bbb界".into(), 4),
            ("界a".into(), 1),
        ] {
            let mut rows = Vec::new();
            wrap_line_into(Line::from(text.clone()), width, &mut rows);
            let copied = rows
                .iter()
                .map(|row| line_columns(row, width).slice(0, width))
                .collect::<String>();
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
                width,
                rows.len() as u16,
            ))
            .unwrap();
            terminal
                .draw(|frame| frame.render_widget(Paragraph::new(Text::from(rows)), frame.area()))
                .unwrap();
            let painted: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            if width == 1 {
                assert_eq!(copied, "a");
                assert!(painted.contains('a'));
            } else {
                assert_eq!(copied, text.trim_start(), "width={width}, screen={painted}");
                assert!(painted.contains('x') || painted.contains('界'));
            }
        }
    }

    #[test]
    fn wrapping_preserves_equal_style_ansi_graphemes_and_line_style() {
        use crate::tui::components::text::line_columns;
        for (prefix, suffix, width) in [
            ("Ae".to_string(), "\u{301}XYZ", 3),
            ("a".repeat(31) + "♥", "\u{fe0f}x", 32),
        ] {
            let text = format!("{prefix}{suffix}");
            let ansi = format!("\x1b[31m{prefix}\x1b[31m{suffix}");
            let line = styled_output_lines(&ansi)
                .remove(0)
                .style(Style::default().bg(Color::Blue));
            let mut rows = Vec::new();
            wrap_line_into(line, width, &mut rows);
            let copied = rows
                .iter()
                .map(|row| line_columns(row, width).slice(0, width))
                .collect::<String>();
            assert_eq!(copied, text);
            assert!(rows.iter().all(|row| row.style.bg == Some(Color::Blue)));
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
                width,
                rows.len() as u16,
            ))
            .unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new(Text::from(rows)), frame.area());
                })
                .unwrap();
            let painted: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .filter(|symbol| *symbol != " ")
                .collect();
            assert_eq!(painted, text);
            assert_eq!(terminal.backend().buffer()[(0, 0)].fg, Color::Red);
        }
    }

    /// The wrapped transcript is cached across frames, so it must never serve
    /// text older than its rows or narrower than the frame it paints into.
    #[test]
    fn transcript_cache_follows_rows_and_width() {
        let chunk = |text: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: Some("m-1".into()),
        };
        let mut state = test_state();
        state.transcript.set_rows(server_rows(&[chunk("alpha")]));
        assert!(render_dump(&state, 60, 12).contains("alpha"));

        state.transcript.set_rows(server_rows(&[chunk("beta")]));
        let dump = render_dump(&state, 60, 12);
        assert!(
            dump.contains("beta") && !dump.contains("alpha"),
            "a replaced transcript must not reuse the cached text: {dump}"
        );

        state
            .transcript
            .set_rows(server_rows(&[chunk(&"wide ".repeat(20))]));
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let rows = |width| {
            wrapped_transcript(&state, &theme, width, ToolOutputMode::Compact)
                .lines
                .len()
        };
        let wide = rows(60);
        assert!(rows(30) > wide, "a narrower frame must re-wrap");
    }

    /// A child's state turns its parent's card on and off with no row delta, so
    /// the roster alone has to invalidate the cached transcript.
    #[test]
    fn transcript_cache_follows_the_subagent_roster() {
        use crate::acp::state::{SubagentRecord, SubagentState};
        let record = |state| SubagentRecord {
            session_id: "child".into(),
            parent_session_id: "s-1".into(),
            parent_tool_call_id: Some("msg-1".into()),
            title: Some("Explore repo".into()),
            description: None,
            state,
            stop_reason: None,
            error: None,
            cancel_capable: true,
            model: None,
            background: false,
            runs: 0,
        };
        let mut state = test_state();
        state
            .transcript
            .set_rows(server_rows(&[tool_event("msg-1", "Explore repo", None)]));
        state
            .transcript
            .set_subagents(vec![record(SubagentState::Running)]);
        assert!(
            render_dump(&state, 80, 20).contains("Explore repo · running"),
            "the roster badges the card"
        );

        state
            .transcript
            .set_subagents(vec![record(SubagentState::Idle)]);
        let dump = render_dump(&state, 80, 20);
        assert!(
            dump.contains("Explore repo · idle") && !dump.contains("running"),
            "a roster change must not reuse the cached badges: {dump}"
        );
    }

    #[test]
    fn wrap_line_rows_and_styles() {
        let rows = |line: Line<'static>, width| {
            let mut out = Vec::new();
            wrap_line_into(line, width, &mut out);
            out.len()
        };
        assert_eq!(rows(Line::from("a".repeat(40)), 10), 4, "hard break");
        assert_eq!(rows(Line::default(), 10), 1);
        assert_eq!(rows(Line::from("x"), 0), 1, "zero width floors to 1");
        // Streaming growth must add rows so stick-to-bottom keeps tracking.
        assert!(rows(Line::from("a".repeat(200)), 40) > rows(Line::from("a".repeat(20)), 40));

        let bold = Style::default().add_modifier(Modifier::BOLD);
        let line = Line::from(vec![
            Span::raw("hello brave "),
            Span::styled("new world", bold),
        ]);
        let mut out = Vec::new();
        wrap_line_into(line, 12, &mut out);
        let texts: Vec<String> = out.iter().map(line_text).collect();
        assert_eq!(texts, ["hello brave", "new world"]);
        assert!(out[1]
            .spans
            .iter()
            .any(|s| s.style == bold && s.content.contains("new")));
    }

    #[test]
    fn truncate_chars_is_char_safe() {
        assert_eq!(truncate_chars("hi", 10), None);
        // Byte slicing used to panic on a multi-byte boundary.
        assert_eq!(truncate_chars("abc😀def😀", 4).as_deref(), Some("abc😀"));
        assert_eq!(
            truncate_chars("日本語のテスト", 3).as_deref(),
            Some("日本語")
        );
    }

    #[test]
    fn message_lines_render_markdown_and_user_turns() {
        let lines = render_agent_message_lines("# Title\n\n**bold** and `code`");
        let text = joined(&lines);
        for marker in ["#", "**", "`"] {
            assert!(!text.contains(marker), "{marker} leaked: {text:?}");
        }
        for word in ["Title", "bold", "code"] {
            assert!(text.contains(word), "{text:?}");
        }
        assert!(lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .all(|s| s.style.fg.is_none()),
            "the theme owns colors: {lines:?}"
        );

        let text = joined(&render_agent_message_lines(
            "before\n\n```\nlet x = 1;\n```\n\nafter",
        ));
        assert!(!text.contains("```") && text.contains("let x = 1;") && text.contains("after"));

        let texts: Vec<String> = render_agent_message_lines("1\n2\n3")
            .iter()
            .map(line_text)
            .collect();
        assert!(texts.iter().any(|t| t.trim() == "1") && texts.iter().any(|t| t.trim() == "3"));

        let lines = render_agent_message_lines("line one\n\nline two");
        assert!(
            line_text(&lines[0]).contains("line one"),
            "no speaker label"
        );

        let text = joined(&render_agent_message_lines(
            "- one\n- two\n\n1. first\n2. second",
        ));
        for want in ["• one", "• two", "1. first", "2. second"] {
            assert!(text.contains(want), "{text:?}");
        }

        let text = joined(&render_agent_message_lines(
            "see [the docs](https://example.com/d) here <https://example.com>",
        ));
        assert!(text.contains("the docs") && text.contains("(https://example.com/d)"));
        assert!(!text.contains('['));
        assert_eq!(text.matches("https://example.com").count(), 2, "{text:?}");

        let text = joined(&render_agent_message_lines(
            "| Name | Value |\n| --- | --- |\n| alpha | 1 |",
        ));
        assert!(text.contains("Name │ Value") && text.contains("alpha │ 1"));
        assert!(!text.contains("---"));

        for input in ["", "   ", "\n\n"] {
            let lines = render_agent_message_lines(input);
            assert_eq!(joined(&lines), "…", "input {input:?}");
        }

        // A user message takes one chevron and no background.
        let theme = crate::tui::styles::load_theme("empire");
        let lines = user_message_lines("first\nsecond", &theme);
        assert_eq!(joined(&lines), "› first\n  second");
        assert!(lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn window_follows_selection_past_cap() {
        // (selected, previous start) -> start: scroll only as far as needed.
        for (selected, prev, want) in [(0, 0, 0), (9, 0, 7), (5, 4, 4), (3, 4, 3), (7, 9, 7)] {
            assert_eq!(
                window_start(selected, 3, 10, prev),
                want,
                "{selected} from {prev}"
            );
        }
        let cmds: Vec<AvailableCommand> = (0..10).map(|i| cmd(&format!("c{i}"), "")).collect();
        let area = Rect::new(0, 5, 20, 2);
        let (first, lines) = window_rows(area, 8, 9, 0, &cmds, |c| vec![format!("/{}", c.name)]);
        assert_eq!(first, 7);
        assert_eq!(lines.len(), 3, "capped by the rows above the composer");
        assert_eq!(line_text(&lines[2]), "▶ /c9");
    }

    #[test]
    fn overlays_render() {
        let mut state = test_state();
        state.focus = Focus::Pane;
        state.plugin_ui = serde_json::from_value(serde_json::json!({
            "entries": [{
                "plugin_id": "gh", "slot": "pane", "id": "p", "session_id": "s-1",
                "payload": {"title": "GitHub", "blocks": [{"kind": "heading", "text": "Checks"}]}
            }],
            "notifications": [],
        }))
        .expect("snapshot");
        let dump = render_dump(&state, 80, 24);
        // The status hint is covered, so the overlay carries the way out.
        assert!(
            dump.contains("GitHub") && dump.contains("Esc to close"),
            "{dump:?}"
        );

        let mut state = test_state();
        state.focus = Focus::Composer;
        state.transcript.available_commands =
            vec![cmd("compact", "shrink context"), cmd("clear", "wipe")];
        state.composer.insert_str("/comp");
        assert!(state.slash_picker_open());
        let dump = render_dump(&state, 80, 24);
        assert!(dump.contains("Commands") && dump.contains("/compact") && dump.contains('▶'));

        let mut state = test_state();
        state.focus = Focus::Composer;
        let mut mcp = cmd("mcp", "MCP servers");
        mcp.input_hint = Some("[reconnect|enable|disable]".into());
        state.transcript.available_commands = vec![mcp];
        state.composer.insert_str("/mc");
        let dump = render_dump(&state, 80, 24);
        assert!(dump.contains("/mcp [reconnect|enable|disable]"), "{dump:?}");

        // A short terminal must keep the selected last row on screen.
        let mut state = test_state();
        state.focus = Focus::Composer;
        state.transcript.available_commands =
            (0..12).map(|i| cmd(&format!("cmd{i:02}"), "")).collect();
        state.composer.insert_str("/cmd");
        let last = state.slash_matches().len() - 1;
        state.move_slash_selection(last as i32);
        let last_name = state.slash_matches()[last].name.clone();
        let dump = render_dump(&state, 40, 9);
        assert!(dump.contains(&format!("▶ /{last_name}")), "{dump:?}");
    }

    #[test]
    fn mention_picker_lists_matching_files() {
        let mention_state = |query: &str, files: &[&str]| {
            let mut state = test_state();
            state.focus = Focus::Composer;
            state.composer.insert_str(format!("@{query}"));
            state.file_index = FileIndex::Loaded {
                files: files.iter().map(|f| f.to_string()).collect(),
                truncated: false,
            };
            state.mention = Some(super::super::state::MentionSession { selected: 0 });
            state
        };
        let dump = render_dump(
            &mention_state("", &["src/main.rs", "docs/readme.md"]),
            80,
            24,
        );
        assert!(
            dump.contains("Files")
                && dump.contains("src/main.rs")
                && dump.contains("docs/readme.md")
        );
        let dump = render_dump(
            &mention_state("src", &["src/main.rs", "zzz/other.md"]),
            80,
            24,
        );
        assert!(dump.contains("src/main.rs") && !dump.contains("zzz/other.md"));
    }

    /// A pending question has to say what it is asking: the option labels alone
    /// leave the lead-in and the chosen answer's detail unsaid.
    #[test]
    fn question_picker_shows_the_question_context_and_highlighted_detail() {
        use super::super::state::{ChoicePicker, ChoicePurpose};

        let mut state = test_state();
        state.choice = Some(ChoicePicker {
            title: " Which store? (Enter=pick · Esc=dismiss) ".into(),
            context: Some(
                "Picking the store for the new service; the one we can migrate cheaply".into(),
            ),
            options: vec![
                ("a".into(), "Postgres".into()),
                ("b".into(), "SQLite".into()),
            ],
            details: vec![Some("Managed, boring, already there".into()), None],
            selected: 0,
            purpose: ChoicePurpose::Mode,
        });
        let dump = render_dump(&state, 80, 24);
        assert!(dump.contains("Picking the store for"), "{dump}");
        assert!(
            dump.contains("Postgres") && dump.contains("SQLite"),
            "{dump}"
        );
        assert!(dump.contains("Managed, boring"), "{dump}");

        // The context rows sit above the items, so clicking the first item row
        // must still resolve to the first option rather than to the context.
        let targets = state.mouse_targets.borrow();
        let picker = targets.picker.expect("choice picker target");
        let first_row = Rect {
            y: picker.rows.y,
            height: 1,
            ..picker.rows
        };
        assert_eq!(
            super::super::input::picker_row_at(
                &targets,
                ratatui::layout::Position::new(first_row.x, first_row.y)
            ),
            Some((PickerKind::Choice, 0))
        );
    }

    /// The approval shelf has to show the request itself: the tool name and an
    /// elided target in the title are not enough to decide on a command.
    #[test]
    fn approval_shelf_shows_the_request_payload() {
        let mut state = test_state();
        state.transcript.pending_approvals = vec![PendingApproval {
            nonce: "n-1".into(),
            title: "Bash".into(),
            kind: "execute".into(),
            args: r#"{"command":"rm -rf build && make","description":"clean the tree"}"#.into(),
            destructive: true,
            options: Vec::new(),
            choice: false,
        }];
        state.selected_approval = Some("n-1".into());
        state.focus = Focus::Approval;
        let dump = render_dump(&state, 80, 24);
        for want in [
            "$ rm -rf build && make",
            "description: clean the tree",
            "destructive",
            "allow once",
        ] {
            assert!(dump.contains(want), "missing {want:?}:\n{dump}");
        }

        // A payload longer than the shelf keeps the head and counts the rest.
        state.transcript.pending_approvals[0].args = serde_json::json!({
            "command": (0..40)
                .map(|i| format!("step-{i}"))
                .collect::<Vec<_>>()
                .join(" && "),
        })
        .to_string();
        let dump = render_dump(&state, 40, 24);
        assert!(dump.contains("step-0"), "{dump}");
        assert!(dump.contains(" more"), "the rest is counted: {dump}");

        // A shelf clamped by a short terminal clips the paragraph, so the count
        // has to come from the height it really has.
        let dump = render_dump(&state, 40, 12);
        assert!(
            dump.contains(" more"),
            "a clipped body still counts what it dropped: {dump}"
        );
    }

    /// A command that runs over several lines is shown as it will run, not run
    /// together the way a single wrapped line would show it.
    #[test]
    fn approval_shelf_keeps_the_lines_of_a_multiline_command() {
        let mut state = test_state();
        state.transcript.pending_approvals = vec![PendingApproval {
            nonce: "n-1".into(),
            title: "Bash".into(),
            kind: "execute".into(),
            args: serde_json::json!({ "command": "echo ok\nrm -rf build" }).to_string(),
            destructive: false,
            options: Vec::new(),
            choice: false,
        }];
        state.selected_approval = Some("n-1".into());
        state.focus = Focus::Approval;
        let lines = render_rows(&state, 80, 24, true);
        let dump = lines.join("\n");
        assert!(
            lines.iter().any(|line| line.contains("$ echo ok")),
            "the command's pilot line is missing:\n{dump}"
        );
        assert!(
            lines.iter().any(|line| line.contains("rm -rf build")),
            "the command's second line is missing:\n{dump}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("echo ok") && line.contains("rm -rf build")),
            "the command's lines ran together:\n{dump}"
        );
    }

    /// A command's control characters are shown, not dropped: a tab would
    /// otherwise join the words either side of it, and DEL would reach the
    /// terminal as a digit-like glyph.
    #[test]
    fn approval_shelf_shows_control_characters_in_the_command() {
        let cases = [
            ("\t", "echo\u{2409}ok", "the tab is shown as one cell"),
            ("\u{7f}", "echo\u{2421}ok", "DEL is the delete symbol"),
            ("\u{9b}", "echo\u{fffd}ok", "a C1 control is replaced"),
        ];
        for (byte, expected, why) in cases {
            let mut state = test_state();
            state.transcript.pending_approvals = vec![PendingApproval {
                nonce: "n-1".into(),
                title: "Bash".into(),
                kind: "execute".into(),
                args: serde_json::json!({ "command": format!("echo{byte}ok") }).to_string(),
                destructive: false,
                options: Vec::new(),
                choice: false,
            }];
            state.selected_approval = Some("n-1".into());
            state.focus = Focus::Approval;
            let lines = render_rows(&state, 80, 24, true);
            let dump = lines.join("\n");
            assert!(
                lines.iter().any(|line| line.contains(expected)),
                "{why} ({expected:?} missing):\n{dump}"
            );
        }
    }

    /// The kind decides the leading argument: a path-based call that also
    /// carries a command string must not promote that string over the path.
    #[test]
    fn approval_body_entries_lead_with_the_kinds_own_target() {
        let row = PendingApproval {
            nonce: "n-1".into(),
            title: "Edit".into(),
            kind: "edit".into(),
            args: serde_json::json!({ "command": "touch /tmp/x", "path": "src/a.rs" }).to_string(),
            destructive: false,
            options: Vec::new(),
            choice: false,
        };
        let target = ToolTarget::Path("src/a.rs");
        let entries = approval_body_entries(&row, Some(&target));
        assert_eq!(
            entries
                .first()
                .map(|(text, primary)| (text.as_str(), *primary)),
            Some(("src/a.rs", true)),
            "the edited path leads: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|(text, primary)| !*primary && text.contains("touch /tmp/x")),
            "the command stays a detail: {entries:?}"
        );
    }

    /// A question whose lead-in is longer than the room above the composer must
    /// still show its answers: they are what the keys act on.
    #[test]
    fn question_picker_keeps_its_options_visible_on_a_short_terminal() {
        use super::super::state::{ChoicePicker, ChoicePurpose};
        let mut state = test_state();
        state.choice = Some(ChoicePicker {
            title: " Which store? ".into(),
            context: Some(
                (0..6)
                    .map(|i| format!("line {i} of the lead-in"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            options: vec![
                ("a".into(), "Postgres".into()),
                ("b".into(), "SQLite".into()),
            ],
            details: vec![Some("Managed, boring, already there".into()), None],
            selected: 0,
            purpose: ChoicePurpose::Mode,
        });
        let dump = render_dump(&state, 60, 12);
        assert!(
            dump.contains("Postgres") && dump.contains("SQLite"),
            "the answers must stay on screen:\n{dump}"
        );
    }

    /// Moving between options must not resize the popup: it is bottom-anchored,
    /// so a shorter detail would slide the options out from under the pointer.
    #[test]
    fn question_picker_option_rows_hold_still_while_the_detail_changes() {
        use super::super::state::{ChoicePicker, ChoicePurpose};
        let mut state = test_state();
        state.choice = Some(ChoicePicker {
            title: " Which store? ".into(),
            context: None,
            options: vec![
                ("a".into(), "Postgres".into()),
                ("b".into(), "SQLite".into()),
            ],
            details: vec![
                Some("Managed, boring, already there and worth its own rows".into()),
                None,
            ],
            selected: 0,
            purpose: ChoicePurpose::Mode,
        });
        let _ = render_dump(&state, 80, 24);
        let with_detail = state.mouse_targets.borrow().picker.expect("picker").rows;

        state.choice.as_mut().unwrap().selected = 1;
        let _ = render_dump(&state, 80, 24);
        let without_detail = state.mouse_targets.borrow().picker.expect("picker").rows;
        assert_eq!(
            with_detail.y, without_detail.y,
            "the option rows must keep their place when the detail shrinks"
        );
    }

    /// An option detail written over several lines has to read as those lines:
    /// the popup has no other way to show a break, so losing it runs two
    /// sentences together.
    #[test]
    fn question_picker_keeps_the_lines_of_a_multiline_detail() {
        use super::super::state::{ChoicePicker, ChoicePurpose};
        let mut state = test_state();
        state.choice = Some(ChoicePicker {
            title: " Which store? ".into(),
            context: None,
            options: vec![
                ("a".into(), "Postgres".into()),
                ("b".into(), "SQLite".into()),
            ],
            details: vec![
                Some("Use the managed one\nOnly for the new service".into()),
                None,
            ],
            selected: 0,
            purpose: ChoicePurpose::Mode,
        });
        let rows = render_rows(&state, 80, 24, true);
        let dump = rows.join("\n");
        assert!(
            rows.iter().any(|row| row.contains("· Use the managed one")),
            "the detail's first line is missing:\n{dump}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("Only for the new service")),
            "the detail's second line is missing:\n{dump}"
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("managed one") && row.contains("Only for the new service")),
            "the detail's lines ran together:\n{dump}"
        );
    }

    /// A detail clipped by a short terminal ends on the marker, so a cut
    /// sentence never reads as the whole of it.
    #[test]
    fn question_picker_marks_a_detail_clipped_to_the_reserved_height() {
        use super::super::state::{ChoicePicker, ChoicePurpose};
        let tail = "and the tail of this answer must not survive the clip";
        let mut state = test_state();
        state.choice = Some(ChoicePicker {
            title: " Which store? ".into(),
            context: None,
            options: vec![
                ("a".into(), "Postgres".into()),
                ("b".into(), "SQLite".into()),
            ],
            details: vec![
                Some(format!(
                    "This answer needs more rows than the popup can spare here, {tail}"
                )),
                None,
            ],
            selected: 0,
            purpose: ChoicePurpose::Mode,
        });
        let rows = render_rows(&state, 40, 8, true);
        let dump = rows.join("\n");
        assert!(
            rows.iter().any(|row| row.contains("· This answer needs")),
            "the detail has to start on screen:\n{dump}"
        );
        let marker_rows: Vec<&String> = rows
            .iter()
            .filter(|row| row.trim_matches(['│', ' ']) == "…")
            .collect();
        assert_eq!(
            marker_rows.len(),
            1,
            "a clipped detail must end on a marker row of its own:\n{dump}"
        );
        assert!(
            !dump.contains("must not survive"),
            "the clip must still drop the tail:\n{dump}"
        );
    }

    /// Two agent messages render as two bubbles; one streamed message
    /// stays whole.
    #[test]
    fn agent_messages_with_different_ids_render_apart() {
        let chunk = |text: &str, id: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let mut t = AcpTranscript::new("s-1");
        t.set_rows(server_rows(&[
            chunk("for #4656.", "m-a"),
            chunk("", "m-b"),
            chunk("The waiter has the lock.", "m-b"),
            Event::ThinkingStarted,
            chunk("Build", "m-c"),
            chunk("ing now", "m-c"),
        ]));
        let out = joined(
            &transcript_lines(&t, &Theme::default(), None, false, ToolOutputMode::Compact).lines,
        );
        assert!(!out.contains("#4656.The waiter"), "{out:?}");
        assert!(
            out.contains("for #4656.") && out.contains("The waiter has the lock."),
            "{out:?}"
        );
        assert!(out.contains("Building now"), "{out:?}");
    }

    /// #5088: each thought renders as one truncated preview line.
    #[test]
    fn thoughts_render_one_preview_line_each() {
        let thought = |text: &str, id: &str| Event::AgentThoughtChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let long = "é".repeat(130);
        let mut t = AcpTranscript::new("s-1");
        t.set_rows(server_rows(&[
            thought("Weighing ", "t-a"),
            thought("options\nsecond line", "t-a"),
            thought(&long, "t-b"),
        ]));
        let lines: Vec<String> =
            transcript_lines(&t, &Theme::default(), None, false, ToolOutputMode::Compact)
                .lines
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .filter(|line: &String| !line.is_empty())
                .collect();
        assert_eq!(
            lines,
            [
                "· Thinking: Weighing options…".to_string(),
                format!("· Thinking: {}…", "é".repeat(120)),
            ]
        );

        // Expanded, the same thoughts render whole: every line, no elision.
        let expanded: Vec<String> =
            transcript_lines(&t, &Theme::default(), None, true, ToolOutputMode::Compact)
                .lines
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .filter(|line: &String| !line.trim().is_empty())
                .collect();
        let body = expanded.join("\n");
        assert!(body.contains("Weighing options"), "{body:?}");
        assert!(body.contains("second line"), "{body:?}");
        assert!(
            body.contains(&long),
            "the full thought, not a preview: {body:?}"
        );
    }

    /// A thought is a quote, not formatted output: markup inside it must not be
    /// swallowed by the parser and leave the thought looking empty.
    #[test]
    fn an_expanded_thought_keeps_markup_it_quotes() {
        let thought = |text: &str, id: &str| Event::AgentThoughtChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let mut t = AcpTranscript::new("s-1");
        t.set_rows(server_rows(&[thought(
            "<div>\nCompare both alternatives.\n</div>",
            "t-a",
        )]));
        let body = joined(
            &transcript_lines(&t, &Theme::default(), None, true, ToolOutputMode::Compact).lines,
        );
        assert!(body.contains("Compare both alternatives."), "{body:?}");
    }

    /// Expanding a thought above the reader grows the transcript under them, so
    /// the row they were reading has to stay at the top of the pane.
    #[test]
    fn expanding_a_thought_keeps_the_reader_on_their_row() {
        let chunk = |text: &str, id: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let thought = |text: &str, id: &str| Event::AgentThoughtChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let mut state = test_state();
        let mut events = vec![thought(
            "first thought\nweighing the options\nchecking the second one\nsettling on one",
            "t-a",
        )];
        for index in 0..12 {
            events.push(chunk(
                &format!("reply number {index}"),
                &format!("m-{index}"),
            ));
        }
        state.transcript.set_rows(server_rows(&events));

        // Scroll back into the replies, then expand the thought above them. The
        // projection separates rows with blanks, so land on a reply rather than
        // one of them.
        state.scroll_offset.set(2);
        let before = render_geometry(&state, 60, 20);
        let top_before = render_rows(&state, 60, 20, true)[before.text_area.y as usize]
            .trim()
            .to_string();
        assert!(!top_before.is_empty(), "the pane must start on a row");

        state.anchor_scroll_to_top_line();
        state.show_full_thinking = true;
        let after = render_geometry(&state, 60, 20);
        let top_after = render_rows(&state, 60, 20, true)[after.text_area.y as usize]
            .trim()
            .to_string();
        assert_eq!(
            top_after, top_before,
            "the reader keeps the row they were on"
        );
    }

    /// A separator at the top of the pane names no row, so anchoring on it used
    /// to pick whichever blank sat nearest the old offset and move the reader.
    #[test]
    fn a_blank_top_row_keeps_the_reader_on_their_content() {
        let chunk = |text: &str, id: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let thought = |text: &str, id: &str| Event::AgentThoughtChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let mut state = test_state();
        let mut events = vec![thought(
            "first thought\nweighing the options\nchecking the second one\nsettling on one",
            "t-a",
        )];
        for index in 0..12 {
            events.push(chunk(
                &format!("reply number {index}"),
                &format!("m-{index}"),
            ));
        }
        state.transcript.set_rows(server_rows(&events));

        // Find an offset that puts a blank separator at the top of the pane, so
        // the reader's own row is the first line of content below it.
        let offset = (1..20u16)
            .find(|offset| {
                state.scroll_offset.set(*offset);
                let rows = render_rows(&state, 60, 20, true);
                let top = render_geometry(&state, 60, 20).text_area.y as usize;
                rows[top].trim().is_empty() && !rows[top + 1].trim().is_empty()
            })
            .expect("fixture has a blank separator above a reply");
        state.scroll_offset.set(offset);
        let before = render_geometry(&state, 60, 20);
        let rows_before = render_rows(&state, 60, 20, true);
        let content_before = rows_before[(before.text_area.y + 1) as usize]
            .trim()
            .to_string();

        state.anchor_scroll_to_top_line();
        state.show_full_thinking = true;
        let after = render_geometry(&state, 60, 20);
        let rows_after = render_rows(&state, 60, 20, true);
        assert_eq!(
            rows_after[(after.text_area.y + 1) as usize].trim(),
            content_before,
            "the content under the separator stays where the reader had it"
        );
    }

    /// Identical replies either side of the reader must not hijack the anchor:
    /// the stored row says which copy they were reading, so an unchanged
    /// projection leaves the pane where it was.
    #[test]
    fn a_repeated_reply_keeps_the_reader_on_their_row() {
        let chunk = |text: &str, id: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let mut state = test_state();
        let events: Vec<Event> = (0..12)
            .map(|index| chunk("same reply", &format!("m-{index}")))
            .collect();
        state.transcript.set_rows(server_rows(&events));

        // A separator at the top puts the anchor one row down, where the copy
        // above the reader sits as near as the copy they are on.
        let offset = (2..18u16)
            .find(|offset| {
                state.scroll_offset.set(*offset);
                let rows = render_rows(&state, 60, 20, true);
                let top = render_geometry(&state, 60, 20).text_area.y as usize;
                if !rows[top].trim().is_empty() {
                    return false;
                }
                let below = rows[top + 1].trim().to_string();
                if below.is_empty() {
                    return false;
                }
                // The same reply also sits directly above that separator.
                state.scroll_offset.set(offset - 1);
                let above = render_rows(&state, 60, 20, true);
                above[top].trim() == below
            })
            .expect("fixture repeats a reply above the reader");
        state.scroll_offset.set(offset);
        let before = render_geometry(&state, 60, 20);
        let rows_before = render_rows(&state, 60, 20, true);
        let reply_before = rows_before[before.text_area.y as usize + 1].clone();

        state.anchor_scroll_to_top_line();
        let after = render_geometry(&state, 60, 20);
        let rows_after = render_rows(&state, 60, 20, true);
        assert_eq!(
            after.first_line, before.first_line,
            "an unchanged projection keeps the offset"
        );
        assert_eq!(
            rows_after[after.text_area.y as usize + 1],
            reply_before,
            "the reader stays on the copy they were reading"
        );
    }

    /// The same reply either side of a thought the reader expands: the copy they
    /// were on has to stay under them even though the other copy is nearer the
    /// text match once the thought grows.
    #[test]
    fn an_expanded_thought_keeps_the_reader_on_a_repeated_reply() {
        let chunk = |text: &str, id: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let thought = |text: &str, id: &str| Event::AgentThoughtChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let mut state = test_state();
        let mut events = vec![
            chunk("same reply", "m-a"),
            thought(
                &(0..10)
                    .map(|line| format!("thought line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                "t-a",
            ),
            chunk("same reply", "m-b"),
        ];
        for index in 0..12 {
            events.push(chunk(
                &format!("reply number {index}"),
                &format!("m-{index}"),
            ));
        }
        state.transcript.set_rows(server_rows(&events));

        // Land the pane on the second copy, with the thought's preview just above
        // it, so the first copy sits only a few rows further up.
        let row_at = |state: &StructuredViewState, offset: u16| {
            state.scroll_offset.set(offset);
            let geometry = render_geometry(state, 60, 20);
            let rows = render_rows(state, 60, 20, true);
            rows[geometry.text_area.y as usize].trim().to_string()
        };
        let offset = (3..20u16)
            .find(|offset| {
                if !row_at(&state, *offset).ends_with("same reply") {
                    return false;
                }
                (1..=3).any(|above| row_at(&state, offset - above).contains("Thinking"))
            })
            .expect("fixture puts the second copy of the reply below the thought");
        state.scroll_offset.set(offset);
        let before = render_geometry(&state, 60, 20);
        let rows_before = render_rows(&state, 60, 20, true);
        let visible_before = rows_before[before.text_area.y as usize..].to_vec();

        state.anchor_scroll_to_top_line();
        state.show_full_thinking = true;
        let after = render_geometry(&state, 60, 20);
        let rows_after = render_rows(&state, 60, 20, true);
        assert_eq!(
            rows_after[after.text_area.y as usize..].to_vec(),
            visible_before,
            "expanding the thought leaves the reader's pane as it was"
        );
    }

    /// Collapsing a thought the reader was inside has to keep its preview on
    /// screen. Its group ends on a blank separator, so anchoring there with
    /// later replies filling the pane drops the preview above the viewport.
    #[test]
    fn collapsing_a_thought_keeps_its_preview_visible() {
        let thought = |text: &str, id: &str| Event::AgentThoughtChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let mut state = test_state();
        let body = (0..10)
            .map(|line| format!("thought line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut events = vec![thought(&body, "t-a")];
        for index in 0..20 {
            events.push(Event::AgentMessageChunk {
                text: format!("reply number {index}"),
                message_id: Some(format!("m-{index}")),
            });
        }
        state.transcript.set_rows(server_rows(&events));
        state.show_full_thinking = true;

        let row_at = |state: &StructuredViewState, offset: u16| {
            state.scroll_offset.set(offset);
            let geometry = render_geometry(state, 60, 20);
            let rows = render_rows(state, 60, 20, true);
            rows[geometry.text_area.y as usize].trim().to_string()
        };
        let offset = (1..20u16)
            .find(|offset| row_at(&state, *offset).starts_with("thought line"))
            .expect("fixture puts the pane inside the expanded thought");
        state.scroll_offset.set(offset);
        state.anchor_scroll_to_top_line();

        state.show_full_thinking = false;
        let geometry = render_geometry(&state, 60, 20);
        let rows = render_rows(&state, 60, 20, true);
        assert!(
            rows[geometry.text_area.y as usize..]
                .iter()
                .any(|row| row.contains("Thinking")),
            "the collapsed preview stays visible: {rows:?}"
        );
    }

    #[test]
    fn transcript_projection() {
        use crate::acp::approvals::Nonce;
        use crate::acp::elicitations::{ElicitationAnswer, ElicitationOutcome};

        let mut t = AcpTranscript::new("s-1");
        let answer = |question: &str, answer: &str| ElicitationAnswer {
            question: question.into(),
            answer: answer.into(),
        };
        // Approvals are control state: a pending one never leaks into the body.
        t.pending_approvals.push(PendingApproval {
            nonce: "internal-pending".into(),
            title: "Read file".into(),
            kind: "read".into(),
            args: r#"{"path":"src/lib.rs"}"#.into(),
            destructive: false,
            options: Vec::new(),
            choice: false,
        });
        t.set_rows(server_rows(&[
            Event::AgentMessageChunk {
                text: "working on it".into(),
                message_id: None,
            },
            Event::ElicitationResolved {
                nonce: Nonce("e-1".into()),
                outcome: ElicitationOutcome::Accepted,
                answers: vec![answer("Proceed?", "Yes"), answer("Mode", "Fast")],
            },
            Event::ConversationCompactionSummary {
                compaction_id: "c-1".into(),
                text: "kept the codeword".into(),
            },
            Event::ConversationCompactionSummary {
                compaction_id: "c-2".into(),
                text: String::new(),
            },
        ]));
        let out = joined(
            &transcript_lines(&t, &Theme::default(), None, false, ToolOutputMode::Compact).lines,
        );
        assert_eq!(out.matches("Compaction summary").count(), 1, "{out:?}");
        for want in [
            "working on it",
            "› Proceed?: Yes",
            "› Mode: Fast",
            "· Compaction summary",
            "kept the codeword",
        ] {
            assert!(out.contains(want), "{out:?}");
        }
        assert!(
            !out.contains("Read file") && !out.contains("internal-"),
            "{out:?}"
        );
    }

    fn tool_row(kind: &str, args: &str, completion: Option<(bool, &str)>) -> ToolCallRow {
        ToolCallRow {
            name: "Tool".into(),
            kind: kind.into(),
            args: args.into(),
            diffs: Vec::new(),
            completed: completion.map(|(ok, content)| ToolCompletion {
                outcome: if ok {
                    ToolOutcome::Ok
                } else {
                    ToolOutcome::Error
                },
                content: content.into(),
            }),
        }
    }

    fn tool_text(row: &ToolCallRow, roots: Option<&SessionPathRoots>) -> String {
        joined(&render_tool_lines(row, &Theme::default(), roots))
    }

    fn tool_event(id: &str, name: &str, parent: Option<&str>) -> Event {
        Event::ToolCallStarted {
            tool_call: crate::acp::state::ToolCall {
                id: id.into(),
                name: name.into(),
                kind: "other".into(),
                args_preview: String::new(),
                prompt: None,
                started_at: crate::time::Timestamp::now(),
                parent_tool_call_id: parent.map(Into::into),
                memory_recall: None,
                diffs: Vec::new(),
            },
        }
    }

    fn tool_lines(t: &AcpTranscript) -> Vec<String> {
        transcript_lines(t, &Theme::default(), None, false, ToolOutputMode::Compact)
            .lines
            .iter()
            .map(line_text)
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// Children indent under their parent card; orphans and unparented rows
    /// stay at top level.
    #[test]
    fn child_tool_calls_nest_under_their_parent_with_orphan_fallback() {
        let mut t = AcpTranscript::new("s-1");
        t.set_rows(server_rows(&[
            tool_event("p1", "Agent", None),
            tool_event("c1", "Read", Some("p1")),
            tool_event("c2", "Grep", Some("p1")),
            tool_event("o1", "Orphan", Some("gone")),
            tool_event("n1", "Plain", None),
        ]));
        assert_eq!(
            tool_lines(&t),
            [
                "tool ▶ · Agent",
                "  tool ▶ · Read",
                "  tool ▶ · Grep",
                "tool ▶ · Orphan",
                "tool ▶ · Plain",
            ]
        );
    }

    /// The adapter's wire shape: the card is synthesized from the delegation
    /// message id, so no background-agent record backs it. The child's tool
    /// calls nest under it and the roster alone badges it.
    #[test]
    fn delegation_card_nests_children_and_badges_from_the_roster_alone() {
        use crate::acp::state::SubagentState::*;
        let cases = [
            (Running, None, vec!["tool ▶ · Explore repo · running"]),
            (
                RequiresAction,
                None,
                vec!["tool ▶ · Explore repo · waiting on you"],
            ),
            (Idle, None, vec!["✓ Explore repo · idle"]),
            (
                Idle,
                Some("error"),
                vec![
                    "tool ✗ · Explore repo · idle",
                    "  (tool failed; press `o` for details)",
                ],
            ),
            (
                Idle,
                Some("cancelled"),
                vec![
                    "tool ◼ · Explore repo · idle",
                    "  (stopped when the turn ended)",
                ],
            ),
            (Unknown, None, vec!["tool ▶ · Explore repo · unknown"]),
        ];
        for (state, stop_reason, head) in cases {
            let mut t = AcpTranscript::new("s-1");
            t.set_rows(server_rows(&[
                tool_event("msg-1", "Explore repo", None),
                tool_event("c1", "Read", Some("msg-1")),
            ]));
            t.set_subagents(vec![SubagentRecord {
                session_id: "child".into(),
                parent_session_id: "s-1".into(),
                parent_tool_call_id: Some("msg-1".into()),
                title: Some("Explore repo".into()),
                description: None,
                state,
                stop_reason: stop_reason.map(Into::into),
                error: None,
                cancel_capable: true,
                model: None,
                background: false,
                runs: 0,
            }]);
            let mut want: Vec<String> = head.into_iter().map(Into::into).collect();
            want.push("  tool ▶ · Read".into());
            assert_eq!(tool_lines(&t), want);
            assert_eq!(
                t.stoppable_subagent().map(|r| r.session_id.as_str()),
                matches!(state, Running | RequiresAction).then_some("child")
            );
        }
    }

    /// A background child outlives the turn whose end stopped its launch call.
    #[test]
    fn turn_end_stop_does_not_close_a_card_whose_child_is_active() {
        use crate::acp::state::SubagentState::*;
        let cases = [
            (Running, "tool ▶ · Explore repo · running"),
            (RequiresAction, "tool ▶ · Explore repo · waiting on you"),
            (Idle, "✓ Explore repo · idle"),
            (Unknown, "tool ◼ · Explore repo · unknown"),
        ];
        for (state, head) in cases {
            let mut t = AcpTranscript::new("s-1");
            let mut events = vec![tool_event("msg-1", "Explore repo", None)];
            events.push(Event::Stopped {
                reason: "end_turn".into(),
            });
            t.set_rows(server_rows(&events));
            t.set_subagents(vec![SubagentRecord {
                session_id: "child".into(),
                parent_session_id: "s-1".into(),
                parent_tool_call_id: Some("msg-1".into()),
                title: Some("Explore repo".into()),
                description: None,
                state,
                stop_reason: None,
                error: None,
                cancel_capable: true,
                model: None,
                background: false,
                runs: 0,
            }]);
            let lines = tool_lines(&t);
            assert_eq!(lines[0], head);
            assert!(
                lines.iter().all(|l| !l.contains("stopped when the turn")) || state == Unknown,
                "{lines:?}"
            );
        }
    }

    #[test]
    fn edit_diffs_prefer_structured_previews_and_cap_at_budget() {
        use crate::acp::state::DiffPreview;
        let mut row = tool_row(
            "edit",
            r#"{"file_path":"args.rs","old_string":"stale","new_string":"ignored"}"#,
            None,
        );
        let diff = |path: &str, old: Option<&str>, new: &str| DiffPreview {
            path: path.into(),
            old_text: old.map(Into::into),
            new_text: Some(new.into()),
            created_at: crate::time::Timestamp::now(),
        };
        row.diffs = vec![
            diff("src/one.rs", Some("let a = 1;"), "let a = 2;"),
            diff("src/two.rs", None, "brand new"),
        ];
        let out = tool_text(&row, None);
        for want in [
            "src/one.rs",
            "- let a = 1;",
            "+ let a = 2;",
            "src/two.rs",
            "+ brand new",
        ] {
            assert!(out.contains(want), "{out:?}");
        }
        assert!(!out.contains("stale"), "{out:?}");

        // An args-derived edit diff caps at its budget with a "more" footer.
        let new_body: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let args =
            serde_json::json!({ "file_path": "big.txt", "old_string": "", "new_string": new_body });
        let lines = render_tool_lines(
            &tool_row("edit", &args.to_string(), None),
            &Theme::default(),
            None,
        );
        let plus = lines
            .iter()
            .filter(|l| line_text(l).trim_start().starts_with("+ "))
            .count();
        assert_eq!(plus, TOOL_DIFF_MAX_LINES);
        assert!(joined(&lines).contains("+10 more diff lines"));
    }

    /// (kind, args, completion, must contain, must not contain)
    type ToolCardCase<'a> = (
        &'a str,
        &'a str,
        Option<(bool, &'a str)>,
        &'a [&'a str],
        &'a [&'a str],
    );

    #[test]
    fn tool_card_bodies() {
        let cases: &[ToolCardCase] = &[
            (
                "edit",
                r#"{"file_path":"src/a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#,
                None,
                &["src/a.rs", "- let x = 1;", "+ let x = 2;"],
                &[],
            ),
            (
                "write",
                r#"{"file_path":"new.txt","content":"line one\nline two"}"#,
                None,
                &["new.txt", "+ line one", "+ line two"],
                &[],
            ),
            (
                "execute",
                r#"{"command":"ls -la"}"#,
                Some((true, "file_a\nfile_b")),
                &["✓ Tool · ls -la"],
                &["file_a"],
            ),
            (
                "read",
                r#"{"path":"src/lib.rs"}"#,
                Some((true, "pub fn main() {}")),
                &["src/lib.rs"],
                &["pub fn main()"],
            ),
            (
                "delete",
                r#"{"path":"old.txt"}"#,
                Some((true, "")),
                &["old.txt"],
                &["+ ", "- "],
            ),
            (
                "fetch",
                "https://example.com",
                None,
                &["$ https://example.com"],
                &[],
            ),
            // Truncated JSON falls back to the generic renderer.
            (
                "edit",
                r#"{"file_path":"a.rs","old_str"#,
                None,
                &["$ {\"file_path\""],
                &[],
            ),
            (
                "fetch",
                "https://example.com",
                Some((false, "\u{1b}[32m200 OK\u{1b}[0m")),
                &["200 OK"],
                &["\u{1b}"],
            ),
        ];
        for (kind, args, completion, present, absent) in cases {
            let out = tool_text(&tool_row(kind, args, *completion), None);
            for want in *present {
                assert!(out.contains(want), "{kind}: {want:?} missing in {out:?}");
            }
            for unwanted in *absent {
                assert!(
                    !out.contains(unwanted),
                    "{kind}: {unwanted:?} leaked in {out:?}"
                );
            }
        }
    }

    #[test]
    fn tool_paths_render_relative_to_session_roots() {
        let roots = SessionPathRoots {
            id: "s-1".into(),
            project_path: "/Users/me/.aoe/worktrees/feat".into(),
            main_repo_path: Some("/Users/me/repo".into()),
            workspace_repos: vec![crate::acp::session_paths::WorkspaceRepoRoot {
                name: "api".into(),
                source_path: "/Users/me/api".into(),
            }],
        };
        let read = |path: &str| format!(r#"{{"path":"{path}"}}"#);
        // (kind, args, shown, absent)
        let cases = [
            (
                "edit",
                r#"{"file_path":"/Users/me/.aoe/worktrees/feat/src/a.rs","old_string":"a","new_string":"b"}"#
                    .to_string(),
                "src/a.rs",
                Some("/Users/me/.aoe/worktrees/feat/src/a.rs"),
            ),
            ("read", read("/Users/me/api/src/h.ts"), "api/src/h.ts", Some("/Users/me/api/src/h.ts")),
            ("delete", read("/etc/hosts"), "/etc/hosts", None),
            // A sibling with a shared prefix is not under the root.
            ("read", read("/Users/me/repo_old/src/lib.rs"), "/Users/me/repo_old/src/lib.rs", None),
        ];
        for (kind, args, shown, absent) in cases {
            let row = tool_row(kind, &args, (kind != "edit").then_some((true, "x")));
            let out = tool_text(&row, Some(&roots));
            assert!(out.contains(shown), "{out:?}");
            if let Some(absent) = absent {
                assert!(!out.contains(absent), "{out:?}");
            }
        }
    }

    #[test]
    fn execute_output_interprets_ansi_colors() {
        let row = tool_row(
            "execute",
            r#"{"command":"cargo test"}"#,
            Some((false, "test result: \u{1b}[31mFAILED\u{1b}[0m. 1 failed")),
        );
        let lines = render_tool_lines(&row, &Theme::default(), None);
        assert!(!joined(&lines).contains('\u{1b}'));
        assert!(lines.iter().flat_map(|l| &l.spans).any(|s| {
            s.content.contains("FAILED") && s.style.fg == Some(ratatui::style::Color::Red)
        }));
        assert_eq!(joined(&styled_output_lines("plain\ntext")), "plain\ntext");
    }

    #[test]
    fn usage_formatting() {
        use crate::acp::state::UsageCost;
        for (n, want) in [
            (842, "842"),
            (1_000, "1.0k"),
            (9_940, "9.9k"),
            (12_300, "12k"),
            (200_000, "200k"),
            (1_250_000, "1.25M"),
            (12_500_000, "12.5M"),
        ] {
            assert_eq!(format_tokens(n), want);
        }
        let cost = |amount, currency: &str| {
            Some(UsageCost {
                amount,
                currency: currency.into(),
            })
        };
        let with_cost = |used, size, cost| SessionUsage { used, size, cost };
        assert_eq!(
            format_usage(&with_cost(12_300, 200_000, cost(0.4231, "USD"))),
            "12k/200k (6%) · $0.4231"
        );
        assert_eq!(format_usage(&usage(100_000, 200_000)), "100k/200k (50%)");
        assert_eq!(
            format_usage(&with_cost(1_000, 200_000, cost(2.5, "EUR"))),
            "1.0k/200k (1%) · 2.50 EUR"
        );
        assert_eq!(usage_percent(&usage(5, 0)), 0);
        assert_eq!(usage_percent(&usage(210_000, 200_000)), 100, "#2927");
    }

    #[test]
    fn compaction_reminder_gating() {
        // (threshold, used, size, compacting, expected)
        let cases = [
            (None, 190_000, 200_000, false, false),
            (Some(75), 150_000, 200_000, false, true),
            (Some(75), 190_000, 200_000, false, true),
            (Some(75), 148_000, 200_000, false, false),
            (Some(75), 190_000, 200_000, true, false),
            (Some(75), 0, 0, false, false),
            (Some(99), 210_000, 200_000, false, true),
        ];
        for (threshold, used, size, compacting, expected) in cases {
            let mut state = test_state();
            state.compaction_reminder_percent = threshold;
            state.transcript.compacting = compacting;
            state.transcript.usage = Some(usage(used, size));
            assert_eq!(
                compaction_reminder_due(&state),
                expected,
                "threshold={threshold:?} used={used} size={size} compacting={compacting}"
            );
        }
        let mut state = test_state();
        state.compaction_reminder_percent = Some(75);
        assert!(!compaction_reminder_due(&state), "no snapshot yet");
    }

    #[test]
    fn status_line_renders_usage_and_reminder() {
        let mut state = test_state();
        state.transcript.usage = Some(usage(12_300, 200_000));
        assert!(render_dump(&state, 80, 24).contains("12k/200k (6%)"));

        let mut state = test_state();
        state.compaction_reminder_percent = Some(75);
        state.transcript.usage = Some(usage(160_000, 200_000));
        assert!(render_dump(&state, 100, 24).contains("/compact"));
    }

    #[test]
    fn status_line_shows_toggle_state() {
        use crate::acp::state::{ConfigOptionCategory, ConfigOptionChoice, ConfigOptionDescriptor};
        let toggle = |id: &str, name: &str, current: &str| ConfigOptionDescriptor {
            id: id.into(),
            name: name.into(),
            description: None,
            category: ConfigOptionCategory::Other("model_config".into()),
            current_value: current.into(),
            options: ["on", "off"]
                .map(|v| ConfigOptionChoice {
                    value: v.into(),
                    name: v.into(),
                    description: None,
                })
                .to_vec(),
        };
        let mut state = test_state();
        state.transcript.config_options = vec![
            toggle("fast", "Fast mode", "on"),
            toggle("ultracode", "Ultracode", "off"),
        ];
        let dump = render_dump(&state, 120, 24);
        assert!(dump.contains("Fast mode on"), "{dump}");
        assert!(dump.contains("Ultracode off"), "{dump}");
    }

    /// #4001: the banner lights up for a background sub-agent even while the
    /// main turn is idle.
    #[test]
    fn status_line_shows_working_banner_for_a_background_agent_alone() {
        let mut state = test_state();
        state.transcript.turn_active = false;
        state.transcript.background_agent_active = true;
        let dump = render_dump(&state, 80, 24);
        assert!(dump.contains("working"), "{dump}");
    }

    #[test]
    fn full_render_layout() {
        let rows = render_rows(&test_state(), 60, 12, true);
        let prompt = rows
            .iter()
            .find(|row| row.contains("Message the agent"))
            .expect("prompt row");
        assert!(prompt.trim_start().starts_with('›'), "{prompt:?}");
        assert!(!prompt.contains('╰') && !prompt.contains('╯'), "{prompt:?}");

        let rows = render_rows(&test_state(), 60, 12, false);
        assert!(rows[0].contains("○ s-1"), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("Press Enter to reply")));

        // The metadata card stays compact and the transcript is unframed.
        let mut state = test_state();
        state.transcript.session_title = Some("virtual-wardrobe".into());
        state.transcript.agent_name = Some("codex".into());
        state.transcript.current_mode = Some("yolo".into());
        state.path_roots = Some(SessionPathRoots {
            id: "s-1".into(),
            project_path: "/workspace/virtual-wardrobe".into(),
            main_repo_path: None,
            workspace_repos: Vec::new(),
        });
        state.transcript.set_rows(server_rows(&[
            Event::UserPromptSent {
                merged_prompt_ids: Vec::new(),
                prompt_id: None,
                text: "Hello.".into(),
                attachments: Vec::new(),
                synthesized: false,
                origin_plugin: None,
                origin: crate::daemon::PromptOrigin::Unknown,
            },
            Event::AgentMessageChunk {
                text: "What should we build?".into(),
                message_id: None,
            },
        ]));

        let rows = render_rows(&state, 80, 20, true);
        let card_right = rows[0].chars().position(|ch| ch == '╮').expect("card edge");
        assert!(card_right < 79, "card spans the viewport: {rows:?}");
        for want in [
            "Agent of Empires · codex",
            "virtual-wardrobe",
            "/workspace/virtual-wardrobe",
            "permissions: yolo",
            "• What should we build?",
        ] {
            assert!(
                rows.iter().any(|row| row.contains(want)),
                "{want}: {rows:?}"
            );
        }
        let prompt = rows
            .iter()
            .find(|row| row.contains("› Hello."))
            .expect("user turn");
        assert!(
            !prompt.starts_with('│') && !prompt.ends_with('│'),
            "{prompt:?}"
        );

        // A plugin-sent message names its sender; a typed one does not.
        state
            .transcript
            .set_rows(server_rows(&[Event::UserPromptSent {
                merged_prompt_ids: Vec::new(),
                prompt_id: None,
                text: "PR merged".into(),
                attachments: Vec::new(),
                synthesized: false,
                origin_plugin: Some("cron".into()),
                origin: crate::daemon::PromptOrigin::Unknown,
            }]));
        let rows = render_rows(&state, 80, 20, true);
        assert!(
            rows.iter()
                .any(|row| row.contains("› [plugin cron] PR merged")),
            "{rows:?}"
        );
    }
}
