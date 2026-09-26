//! Structured view render: transcript, approval shelf, queue, composer, and
//! status line, with pickers floating above the composer.

use std::collections::HashMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::Frame;
use similar::{ChangeTag, TextDiff};

use aoe_plugin_api::UiSlot;

use ansi_to_tui::IntoText;

use super::input::Focus;
use super::reducer::{
    AcpTranscript, NoteKind, PendingApproval, ToolCallRow, ToolCompletion, ToolOutcome,
};
use super::state::{FileIndex, StructuredViewState, ViewLayout};
use crate::acp::approvals::{tool_target, ToolTarget, CMD_KEYS, PATH_KEYS};
use crate::acp::session_paths::{relative_display_path, SessionPathRoots};
use crate::acp::state::{SessionUsage, ToolOutputBlock};
use crate::acp::transcript::{TranscriptRow, TranscriptRowKind};
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

    let geometry = render_transcript(frame, layout.transcript, theme, state, active);
    render_status(frame, layout.status, theme, state, active);
    if layout.approval.height > 0 {
        render_approval_shelf(frame, layout.approval, theme, state, active);
    }
    if layout.queue.height > 0 {
        render_queue(frame, layout.queue, theme, state);
    }
    render_composer(frame, layout.composer, theme, state, active);
    // Pickers float above the bottom-anchored composer. The choice picker owns
    // the navigation keys while open, so it wins the pixels too.
    if let Some(picker) = &state.choice {
        render_choice_picker(frame, layout.composer, theme, picker);
    } else if matches!(state.focus, Focus::Composer) && state.slash_picker_open() {
        render_slash_picker(frame, layout.composer, theme, state);
    } else if matches!(state.focus, Focus::Composer) && state.mention.is_some() {
        render_mention_picker(frame, layout.composer, theme, state);
    }
    // The plugin pane is a modal overlay; an open choice picker still owns the
    // navigation keys, so it must stay visible on top.
    if matches!(state.focus, Focus::Pane) && state.choice.is_none() {
        render_pane_panel(frame, area, theme, state);
    }
    geometry
}

/// Permission mode / elicitation answer picker.
fn render_choice_picker(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    picker: &super::state::ChoicePicker,
) {
    let lines = window_rows(
        composer_area,
        8,
        picker.selected,
        &picker.options,
        |(_, label)| vec![label.clone()],
    );
    render_popup_above(frame, composer_area, theme, picker.title.clone(), lines);
}

/// Up to `max_rows` picker rows (fewer on a short terminal), windowed so
/// `selected` stays visible and marked.
fn window_rows<T, S: Into<Span<'static>>>(
    composer_area: Rect,
    max_rows: usize,
    selected: usize,
    items: &[T],
    spans: impl Fn(&T) -> Vec<S>,
) -> Vec<Line<'static>> {
    // Minus the two border rows, so the selection can't paint off-screen.
    let max_rows = (composer_area.y as usize).saturating_sub(2).min(max_rows);
    if max_rows == 0 || items.is_empty() {
        return Vec::new();
    }
    let total = items.len();
    let cap = max_rows.min(total);
    let start = window_start(selected, cap, total);
    items[start..(start + cap).min(total)]
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
        .collect()
}

/// First visible index of a `cap`-row window that keeps `selected` inside it.
fn window_start(selected: usize, cap: usize, total: usize) -> usize {
    if selected >= cap {
        (selected - cap + 1).min(total.saturating_sub(cap))
    } else {
        0
    }
}

/// Bordered popup whose bottom edge sits on the composer's top edge.
fn render_popup_above(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    title: String,
    lines: Vec<Line<'_>>,
) {
    if lines.is_empty() {
        return;
    }
    let y = composer_area.y.saturating_sub(lines.len() as u16 + 2);
    let area = Rect {
        y,
        height: composer_area.y - y,
        ..composer_area
    };
    if area.height < 3 {
        return;
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
}

/// Pure so the redraw path can stash it on `state.layout` for hit-testing.
pub(super) fn compute_layout(area: Rect, state: &StructuredViewState) -> ViewLayout {
    let queue_height = queued_strip_height(state);
    let approval_height = u16::from(!state.transcript.pending_approvals.is_empty()) * 3;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5), // transcript
            Constraint::Length(approval_height),
            Constraint::Length(queue_height), // queued prompts strip (0 when empty)
            Constraint::Length(composer_height(state)),
            Constraint::Length(1), // status line
        ])
        .split(area);
    ViewLayout {
        transcript: chunks[0],
        approval: chunks[1],
        queue: chunks[2],
        composer: chunks[3],
        status: chunks[4],
    }
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
        // The status hint is covered by this overlay, so paint the way out here.
        .title_bottom(" Esc to close ")
        .border_style(Style::default().fg(theme.title));
    let inner = block.inner(panel);
    frame.render_widget(Clear, panel);
    frame.render_widget(block, panel);
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

fn render_approval_shelf(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    active: bool,
) {
    let Some(selected) = state.selected_approval.as_deref() else {
        return;
    };
    let Some(row) = state
        .transcript
        .pending_approvals
        .iter()
        .find(|pending| pending.nonce == selected)
    else {
        return;
    };
    let position = state
        .transcript
        .pending_approvals
        .iter()
        .position(|pending| pending.nonce == selected)
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
    let actions = approval_actions_line(theme, active, row.choice && !row.options.is_empty());
    frame.render_widget(Paragraph::new(actions), inner);
}

/// Choice approvals ask a question, so `a` answers and "always" doesn't apply.
fn approval_actions_line(theme: &Theme, active: bool, choice: bool) -> Line<'static> {
    if !active {
        return Line::from(Span::styled(
            "Enter to respond",
            Style::default().fg(theme.hint),
        ));
    }
    let mut spans = vec![
        Span::styled(
            "a",
            Style::default()
                .fg(theme.running)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if choice { " answer" } else { " allow once" },
            Style::default().fg(theme.hint),
        ),
        Span::styled("  ·  ", Style::default().fg(theme.border)),
    ];
    if !choice {
        spans.extend([
            Span::styled(
                "A",
                Style::default()
                    .fg(theme.running)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" always", Style::default().fg(theme.hint)),
            Span::styled("  ·  ", Style::default().fg(theme.border)),
        ]);
    }
    spans.extend([
        Span::styled(
            "d",
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" deny", Style::default().fg(theme.hint)),
        Span::styled("  ·  ", Style::default().fg(theme.border)),
        Span::styled(
            "Esc",
            Style::default().fg(theme.hint).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" stop", Style::default().fg(theme.hint)),
    ]);
    Line::from(spans)
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
) {
    let matches = state.slash_matches();
    let lines = window_rows(composer_area, 8, state.slash_selected, &matches, |cmd| {
        let mut spans = vec![Span::raw(format!("/{}", cmd.name))];
        if !cmd.description.is_empty() {
            spans.push(Span::styled(
                format!("  {}", cmd.description),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        spans
    });
    render_popup_above(
        frame,
        composer_area,
        theme,
        " Commands (↑/↓ or Ctrl+n/p · Enter/Tab select · Esc dismiss) ".into(),
        lines,
    );
}

/// `@` mention picker, with placeholders while the file index loads or fails.
fn render_mention_picker(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let placeholder = |text: String, style: Style| vec![Line::from(Span::styled(text, style))];
    let selected = state.mention.as_ref().map(|s| s.selected).unwrap_or(0);
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
                let mut lines = window_rows(composer_area, 8, selected, &files, |path| {
                    vec![path.to_string()]
                });
                if lines.is_empty() {
                    return;
                }
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
    render_popup_above(
        frame,
        composer_area,
        theme,
        " Files (↑/↓ or Ctrl+n/p · Enter/Tab insert · Esc close) ".into(),
        lines,
    );
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

    let text = wrapped_transcript(state, theme, text_area.width);
    // Pre-wrapped at the render width, so visual rows are logical rows.
    let total = text.lines.len().min(u16::MAX as usize) as u16;
    let max = total.saturating_sub(text_area.height);
    // Lets a scroll step resolve the stick-to-bottom sentinel first.
    state.last_scroll_max.set(max);
    let first = state.scroll_offset.min(max);
    let para = Paragraph::new(text).scroll((first, 0));
    frame.render_widget(para, text_area);
    TranscriptGeometry {
        text_area,
        first_line: first as usize,
        total_lines: total as usize,
    }
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

/// The transcript pre-wrapped at `width`: the single source of transcript
/// geometry for painting, scroll clamping, and selection.
pub(crate) fn wrapped_transcript(
    state: &StructuredViewState,
    theme: &Theme,
    width: u16,
) -> Text<'static> {
    let lines = transcript_lines(&state.transcript, theme, state.path_roots.as_ref());
    let mut wrapped: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    for line in lines {
        wrap_line_into(own_line(line), width, &mut wrapped);
    }
    Text::from(wrapped)
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

/// Word-wrap a styled line at the last space, hard-breaking long words.
/// Unlike `markdown::wrap_line_into`, tabs are not expanded.
fn wrap_line_into(line: Line<'static>, width: u16, out: &mut Vec<Line<'static>>) {
    use unicode_width::UnicodeWidthChar;

    let width = width.max(1) as usize;
    if line.width() <= width {
        out.push(line);
        return;
    }
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut row_width = 0usize;
    let mut last_space: Option<usize> = None;
    let flush = |row: &mut Vec<(char, Style)>, out: &mut Vec<Line<'static>>| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (c, style) in row.drain(..) {
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(c),
                _ => spans.push(Span::styled(c.to_string(), style)),
            }
        }
        out.push(Line::from(spans));
    };
    for (c, style) in chars {
        let cw = c.width().unwrap_or(0);
        if row_width + cw > width && !row.is_empty() {
            if let Some(cut) = last_space {
                // The break space is dropped, like a terminal word wrap.
                let tail: Vec<(char, Style)> = row.split_off(cut + 1);
                row.truncate(cut);
                flush(&mut row, out);
                row = tail;
                row_width = row.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
            } else {
                flush(&mut row, out);
                row_width = 0;
            }
            last_space = None;
        }
        if c == ' ' {
            last_space = Some(row.len());
        }
        row.push((c, style));
        row_width += cw;
    }
    flush(&mut row, out);
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
    if state.scroll_offset != u16::MAX {
        spans.push(Span::styled(
            " · G latest ",
            Style::default().fg(theme.hint),
        ));
    }
    // Plugin status-bar segments and this session's badges; icons, tooltips,
    // and links have no terminal surface.
    for entry in plugin_ui::global_entries(&state.plugin_ui, UiSlot::StatusBar).chain(
        plugin_ui::session_entries(&state.plugin_ui, UiSlot::DetailBadge, &state.session_id),
    ) {
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
        help_hint(state.focus, selected_approval_is_choice(state))
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
) -> Vec<Line<'static>> {
    let rows = &transcript.server_rows;
    // One card per tool call at its start row; the last terminal row wins.
    let mut completions: HashMap<&str, &TranscriptRow> = HashMap::new();
    for row in rows {
        if is_tool_terminal(row.kind) {
            if let Some(id) = row.tool_call_id.as_deref() {
                completions.insert(id, row);
            }
        }
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let row = &rows[i];
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
            TranscriptRowKind::UserPrompt => {
                // Note attachments so an image-only prompt doesn't look empty.
                let text = if row.attachments.is_empty() {
                    row.text.clone()
                } else {
                    format!("{} [{} attachment(s)]", row.text, row.attachments.len())
                };
                out.extend(user_message_lines(&text, theme));
                out.push(Line::default());
            }
            TranscriptRowKind::UserDiffComments => {
                // The assembled markdown the agent received reads as a prompt.
                out.extend(user_message_lines(&row.text, theme));
                out.push(Line::default());
            }
            TranscriptRowKind::ToolStart => {
                let card = tool_card_from_rows(row, &completions);
                out.extend(render_tool_lines(&card, theme, path_roots));
                out.push(Line::default());
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
            TranscriptRowKind::EmptyOutput
            | TranscriptRowKind::ContextReset
            | TranscriptRowKind::SessionCleared
            | TranscriptRowKind::Compacted
            | TranscriptRowKind::Summary
            | TranscriptRowKind::Notice => {
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
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "(no events yet, waiting for the agent…)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    out
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

fn help_hint(focus: Focus, approval_is_choice: bool) -> &'static str {
    match focus {
        Focus::Composer => " Enter to send · Ctrl+Q to exit ",
        // `render_status` drops the hint unless it has `len + 24` spare columns,
        // so keep these short.
        Focus::Transcript => " scroll · p pane · Ctrl+Q exit ",
        Focus::Approval if approval_is_choice => " a answer · d deny · Esc stop ",
        Focus::Approval => " a allow · A always · d deny · Esc stop ",
        Focus::Pane => " scroll to read · Esc to close ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::Source;
    use crate::acp::client::{DaemonEndpoint, HttpClient};
    use crate::acp::state::{AvailableCommand, Event};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn test_state() -> StructuredViewState {
        let endpoint = DaemonEndpoint::new("http://127.0.0.1:8080".into(), None, Source::Env);
        let http = HttpClient::new(endpoint.clone()).unwrap();
        StructuredViewState::new("s-1".into(), endpoint, http, None)
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
        let text = line_text(&approval_actions_line(&Theme::default(), true, false));
        for want in ["a allow once", "A always", "d deny"] {
            assert!(text.contains(want), "{text:?}");
        }
        assert!(
            !text.contains('[') && !text.contains(']'),
            "button chrome: {text:?}"
        );

        // A question offers answering instead of the permission vocabulary (#3741).
        let text = line_text(&approval_actions_line(&Theme::default(), true, true));
        assert!(
            text.contains("a answer") && text.contains("d deny"),
            "{text:?}"
        );
        assert!(!text.contains("always"), "{text:?}");
        assert!(help_hint(Focus::Approval, true).contains("a answer"));
        assert!(!help_hint(Focus::Approval, true).contains("always"));
        assert!(help_hint(Focus::Approval, false).contains("A always"));
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
        assert_eq!(window_start(0, 3, 10), 0);
        assert_eq!(window_start(9, 3, 10), 7);
        let cmds: Vec<AvailableCommand> = (0..10).map(|i| cmd(&format!("c{i}"), "")).collect();
        let area = Rect::new(0, 5, 20, 2);
        let lines = window_rows(area, 8, 9, &cmds, |c| vec![format!("/{}", c.name)]);
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
        t.server_rows = server_rows(&[
            Event::AgentMessageChunk {
                text: "working on it".into(),
            },
            Event::ElicitationResolved {
                nonce: Nonce("e-1".into()),
                outcome: ElicitationOutcome::Accepted,
                answers: vec![answer("Proceed?", "Yes"), answer("Mode", "Fast")],
            },
        ]);
        let out = joined(&transcript_lines(&t, &Theme::default(), None));
        for want in ["working on it", "› Proceed?: Yes", "› Mode: Fast"] {
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
            created_at: chrono::Utc::now(),
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
        state.transcript.server_rows = server_rows(&[
            Event::UserPromptSent {
                prompt_id: None,
                text: "Hello.".into(),
                attachments: Vec::new(),
                synthesized: false,
            },
            Event::AgentMessageChunk {
                text: "What should we build?".into(),
            },
        ]);

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
    }
}
