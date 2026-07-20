//! Render of a structured view session, stacked top to bottom: transcript,
//! approval shelf, queue, composer, and a status line pinned at the bottom.
//! The slash and `@` mention pickers
//! float above the composer when open rather than taking a pane. Successful
//! tools collapse to target-aware summaries; running and failed tools use the
//! per-kind detail renderer. Image previews and syntax highlighting stay
//! deferred to the web structured view; press `o` from the transcript pane to
//! open it for full-fidelity inspection.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::Frame;
use similar::{ChangeTag, TextDiff};

use aoe_plugin_api::UiSlot;

use ansi_to_tui::IntoText;

use super::input::Focus;
use super::reducer::{AcpTranscript, ActivityRow, NoteKind, ToolCallRow};
use super::state::{FileIndex, StructuredViewState, ViewLayout};
use crate::acp::approvals::ApprovalDecision;
use crate::acp::session_paths::{relative_display_path, SessionPathRoots};
use crate::acp::state::SessionUsage;
use crate::tui::plugin_ui;
use crate::tui::styles::Theme;

/// Render the structured view into `area`. `active` is true when the
/// view has the keyboard (full-screen attach, or an embedded view the
/// user entered); false for an embedded preview that is merely showing
/// the transcript of the selected session. When inactive the composer
/// caret is not shown and its chrome reads as a prompt to enter.
/// Returns the transcript geometry so the embedded caller can feed the
/// home view's drag-select machinery.
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
    // Pickers float above the composer (the composer sits at the screen
    // bottom, so a dropdown below it would render off-screen). Drawn
    // last so they overlay the transcript's lower rows. The choice
    // picker (mode / answer) wins over the composer-driven pickers: it
    // owns the navigation keys while open, so it must own the pixels
    // too. Slash and `@` pickers are mutually exclusive; slash wins the
    // tie defensively.
    if let Some(picker) = &state.choice {
        render_choice_picker(frame, layout.composer, theme, picker);
    } else if matches!(state.focus, Focus::Composer) && state.slash_picker_open() {
        render_slash_picker(frame, layout.composer, theme, state);
    } else if matches!(state.focus, Focus::Composer) && state.mention.is_some() {
        render_mention_picker(frame, layout.composer, theme, state);
    }
    geometry
}

/// Floating single-choice picker (permission mode, elicitation answer),
/// anchored above the composer like the slash picker. Rows window around
/// the selection on short terminals.
fn render_choice_picker(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    picker: &super::state::ChoicePicker,
) {
    const CHOICE_PICKER_MAX_ROWS: usize = 8;
    let max_rows = (composer_area.y as usize)
        .saturating_sub(2)
        .min(CHOICE_PICKER_MAX_ROWS);
    if max_rows == 0 || picker.options.is_empty() {
        return;
    }
    let total = picker.options.len();
    let cap = max_rows.min(total).max(1);
    let selected = picker.selected.min(total - 1);
    let start = if selected >= cap {
        (selected - cap + 1).min(total.saturating_sub(cap))
    } else {
        0
    };
    let mut lines = Vec::with_capacity(cap);
    for (offset, (_, label)) in picker.options[start..(start + cap).min(total)]
        .iter()
        .enumerate()
    {
        let idx = start + offset;
        let marker = if idx == selected { "▶ " } else { "  " };
        lines.push(Line::from(Span::styled(
            format!("{marker}{label}"),
            if idx == selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        )));
    }
    let desired = lines.len() as u16 + 2;
    let y = composer_area.y.saturating_sub(desired);
    let area = Rect {
        x: composer_area.x,
        y,
        width: composer_area.width,
        height: composer_area.y - y,
    };
    if area.height < 3 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(picker.title.clone())
        .border_style(Style::default().fg(theme.title));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Split `area` into the view's vertical panes. Pure so the redraw path
/// can stash the result on state (`state.layout`) for mouse hit-testing
/// while `render` recomputes it per frame.
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

/// The queue is a compact shelf rather than another boxed transcript. Recall
/// keeps the individual messages reachable without permanently spending rows.
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
        .rows
        .iter()
        .find_map(|activity| match activity {
            ActivityRow::Approval(row) if row.nonce == selected && row.decision.is_none() => {
                Some(row)
            }
            _ => None,
        })
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
    let actions = approval_actions_line(theme, active);
    frame.render_widget(Paragraph::new(actions), inner);
}

fn approval_actions_line(theme: &Theme, active: bool) -> Line<'static> {
    if !active {
        return Line::from(Span::styled(
            "Enter to respond",
            Style::default().fg(theme.hint),
        ));
    }
    Line::from(vec![
        Span::styled(
            "a",
            Style::default()
                .fg(theme.running)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" allow once", Style::default().fg(theme.hint)),
        Span::styled("  ·  ", Style::default().fg(theme.border)),
        Span::styled(
            "A",
            Style::default()
                .fg(theme.running)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" always", Style::default().fg(theme.hint)),
        Span::styled("  ·  ", Style::default().fg(theme.border)),
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
    ])
}

fn approval_target(
    row: &super::reducer::ApprovalRow,
    path_roots: Option<&SessionPathRoots>,
) -> String {
    let args = parse_args_object(&row.args);
    match row.kind.as_str() {
        "edit" | "write" | "read" | "delete" | "move" => pick_str(args.as_ref(), PATH_KEYS)
            .map(|path| relative_display_path(path, path_roots))
            .unwrap_or_default(),
        "execute" => pick_str(args.as_ref(), CMD_KEYS)
            .and_then(|command| command.lines().next())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Most picker rows visible at once before the list windows around the
/// selection. Keeps the popup from eating the whole transcript when the
/// daemon advertises a long command list.
const SLASH_PICKER_MAX_ROWS: usize = 8;

fn render_slash_picker(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
) {
    let matches = state.slash_matches();
    if matches.is_empty() {
        return;
    }
    // Cap the visible rows to the space above the composer (minus the 2
    // border rows) before windowing, so on a short terminal the window
    // can't hand back more rows than will paint and hide the selection
    // at the bottom. width matches the composer so the popup lines up
    // with the input it completes.
    let max_rows = (composer_area.y as usize)
        .saturating_sub(2)
        .min(SLASH_PICKER_MAX_ROWS);
    if max_rows == 0 {
        return;
    }
    let lines = picker_lines(&matches, state.slash_selected, max_rows);
    let desired = lines.len() as u16 + 2;
    // Anchor the popup's bottom edge to the composer's top edge, growing
    // upward. max_rows already guarantees the list fits above the
    // composer, so the height below won't truncate the windowed rows.
    let y = composer_area.y.saturating_sub(desired);
    let area = Rect {
        x: composer_area.x,
        y,
        width: composer_area.width,
        height: composer_area.y - y,
    };
    if area.height < 3 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(" Commands (↑/↓ or Ctrl+n/p · Enter/Tab select · Esc dismiss) ")
        .border_style(Style::default().fg(theme.title));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Build the picker's visible rows, windowed around `selected` so a
/// selection past the visible cap still shows. Each row is
/// `▶ /name  description`, with the marker only on the selected row.
fn picker_lines<'a>(
    matches: &[&'a crate::acp::state::AvailableCommand],
    selected: usize,
    max_rows: usize,
) -> Vec<Line<'a>> {
    let total = matches.len();
    let cap = max_rows.min(total).max(1);
    // Slide the window so `selected` stays inside [start, start+cap).
    let start = if selected >= cap {
        (selected - cap + 1).min(total.saturating_sub(cap))
    } else {
        0
    };
    let mut out = Vec::with_capacity(cap);
    for (offset, cmd) in matches[start..(start + cap).min(total)].iter().enumerate() {
        let idx = start + offset;
        let is_sel = idx == selected;
        let marker = if is_sel { "▶ " } else { "  " };
        let mut spans = vec![Span::styled(
            format!("{marker}/{}", cmd.name),
            if is_sel {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        )];
        if !cmd.description.is_empty() {
            spans.push(Span::styled(
                format!("  {}", cmd.description),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        out.push(Line::from(spans));
    }
    out
}

/// Most `@`-mention rows visible at once before the list windows around
/// the selection.
const MENTION_PICKER_MAX_ROWS: usize = 8;

/// Floating `@`-mention picker, anchored above the composer like the
/// slash picker. Shows a loading / error / empty placeholder when the
/// file index is not ready or nothing matches, otherwise the windowed
/// list of matching paths.
fn render_mention_picker(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
) {
    let selected = state.mention.as_ref().map(|s| s.selected).unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();
    let mut truncated_note = false;
    match &state.file_index {
        FileIndex::Unloaded | FileIndex::Loading => {
            lines.push(Line::from(Span::styled(
                "  loading files…",
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
        FileIndex::Failed(err) => {
            lines.push(Line::from(Span::styled(
                format!("  file list unavailable: {err}"),
                Style::default().fg(theme.error),
            )));
        }
        FileIndex::Loaded { truncated, .. } => {
            let files = super::filtered_mention_files(state);
            if files.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  no matching files",
                    Style::default().add_modifier(Modifier::DIM),
                )));
            } else {
                let max_rows = (composer_area.y as usize)
                    .saturating_sub(2)
                    .min(MENTION_PICKER_MAX_ROWS);
                if max_rows == 0 {
                    return;
                }
                let total = files.len();
                let cap = max_rows.min(total).max(1);
                let start = if selected >= cap {
                    (selected - cap + 1).min(total.saturating_sub(cap))
                } else {
                    0
                };
                for (offset, path) in files[start..(start + cap).min(total)].iter().enumerate() {
                    let idx = start + offset;
                    let marker = if idx == selected { "▶ " } else { "  " };
                    lines.push(Line::from(Span::styled(
                        format!("{marker}{path}"),
                        if idx == selected {
                            Style::default().add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    )));
                }
                truncated_note = *truncated;
            }
        }
    }
    if truncated_note {
        lines.push(Line::from(Span::styled(
            "  (workspace over 5000 files; list capped)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    // Anchor the popup's bottom edge to the composer's top edge, growing
    // upward, exactly like the slash picker.
    let desired = lines.len() as u16 + 2;
    let y = composer_area.y.saturating_sub(desired);
    let area = Rect {
        x: composer_area.x,
        y,
        width: composer_area.width,
        height: composer_area.y - y,
    };
    if area.height < 3 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(" Files (↑/↓ or Ctrl+n/p · Enter/Tab insert · Esc close) ")
        .border_style(Style::default().fg(theme.title));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One separator row above the terminal-native prompt rail.
const COMPOSER_CHROME_ROWS: u16 = 1;
/// Maximum content rows the composer is allowed to take before the
/// transcript starts losing space. Multi-line prompts beyond this
/// scroll inside the textarea instead of growing the pane.
const COMPOSER_MAX_CONTENT_ROWS: u16 = 6;

fn composer_height(state: &StructuredViewState) -> u16 {
    // Composer is two rows tall by default: one separator and one prompt
    // row. Multi-line prompts grow up to the content cap, then scroll
    // inside the textarea instead of squeezing the transcript.
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
    // The lines are pre-wrapped at the render width, so visual rows ARE
    // logical rows: the scroll clamp is exact (no wrap estimation), and
    // the same geometry serves the home view's drag-select machinery.
    let total = text.lines.len().min(u16::MAX as usize) as u16;
    let max = total.saturating_sub(text_area.height);
    // Record the concrete max so a wheel/PageUp step can resolve the
    // stick-to-bottom sentinel before moving (see `apply_scroll`).
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

    let agent = state.transcript.agent_name.as_deref().unwrap_or("agent");
    let directory = state
        .path_roots
        .as_ref()
        .map(|roots| roots.project_path.as_str())
        .unwrap_or("loading…");
    let mode = state
        .transcript
        .current_mode
        .as_deref()
        .unwrap_or("default");
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

fn render_metadata_card(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &StructuredViewState,
    friendly_title: &str,
) {
    let inner_width = area.width.saturating_sub(4) as usize;
    let agent = state.transcript.agent_name.as_deref().unwrap_or("agent");
    let directory = state
        .path_roots
        .as_ref()
        .map(|roots| roots.project_path.as_str())
        .unwrap_or("loading…");
    let mode = state
        .transcript
        .current_mode
        .as_deref()
        .unwrap_or("default");
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

/// Where the transcript text landed in the last render: the inner text
/// rect (borders stripped), the absolute index of the top visible row,
/// and the total wrapped row count. The home view feeds this into its
/// preview drag-select machinery so selection coordinates line up with
/// the painted cells.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptGeometry {
    pub text_area: Rect,
    pub first_line: usize,
    pub total_lines: usize,
}

/// Build the transcript as pre-wrapped lines at `width` columns. This is
/// the single source of transcript geometry: the renderer paints precisely
/// these rows (no Paragraph wrap), the scroll clamp counts them, and the
/// home view's selection extraction slices them.
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

/// Detach a line from whatever transcript strings it borrows so the
/// wrapped text can outlive the state borrow.
fn own_line(line: Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect();
    Line::from(spans).style(line.style)
}

/// Word-wrap one styled line into rows of at most `width` columns,
/// preserving span styles. Char-level flatten + regroup: simple, style
/// exact, and O(len). Breaks at the last space when one exists in the
/// current row, hard-breaks otherwise (long paths, hashes). Wide chars
/// count via their display width.
fn wrap_line_into(line: Line<'static>, width: u16, out: &mut Vec<Line<'static>>) {
    use unicode_width::UnicodeWidthChar;

    let width = width.max(1) as usize;
    if line.width() <= width {
        out.push(line);
        return;
    }
    // Flatten to (char, style); wrap; regroup runs of equal style.
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
                // Break at the space: it ends the current row (and is
                // dropped, like a terminal word wrap); the tail carries
                // into the next row.
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
    if state.transcript.turn_active {
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
    if state.transcript.context_primer_pending {
        spans.push(Span::styled(
            " context lost; next prompt re-primes ",
            Style::default().fg(theme.error),
        ));
    }
    if state.transcript.lagged {
        spans.push(Span::styled(
            " broadcast lagged; refetching ",
            Style::default().fg(theme.error),
        ));
    }
    if state.scroll_offset != u16::MAX {
        spans.push(Span::styled(
            " · G latest ",
            Style::default().fg(theme.hint),
        ));
    }
    // Plugin host-rendered slots (#2402): global status-bar segments and this
    // session's detail badges, tone-colored. Icons / tooltips / hrefs have no
    // terminal surface and are dropped; malformed entries are skipped.
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
    // Context-window token meter, mirroring the web composer's usage
    // chip (`formatTokens` / `formatCost` in Composer.tsx). Rendered
    // right-aligned in its own reserved slice so a long help hint or
    // banner can't push it off-screen.
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
        help_hint(state.focus)
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

/// Context fill percentage at which the token meter turns alarm-colored.
const USAGE_WARN_PERCENT: u64 = 90;

/// Rounded context-fill percentage; 0 when the agent reported no window
/// size (avoids a divide-by-zero on a malformed snapshot). Capped at
/// 100: some agents report `used > size` transiently (e.g. before a
/// compaction lands), and "105%" reads as a rendering bug (#2927). The
/// web composer caps identically.
fn usage_percent(usage: &SessionUsage) -> u64 {
    if usage.size == 0 {
        return 0;
    }
    (((usage.used as f64 / usage.size as f64) * 100.0).round() as u64).min(100)
}

/// `12.3k/200k (6%) · $0.42`-style usage summary, matching the web
/// composer's number formatting so the two surfaces read the same.
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

/// Compact token count: `842`, `12.3k`, `1.25M`. Mirrors the web
/// `formatTokens` thresholds.
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
    // Leave one open spacer row above the input. Recall / inactive state can
    // use it for context without drawing a divider across the terminal.
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
    // Only show the caret when the view is active: a preview must not
    // plant a blinking cursor in a box the keyboard isn't routed to.
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

/// User turns use the same open chevron gutter as Codex: one marker on the
/// first line, continuation indentation on subsequent lines, and no filled
/// background that would turn the message into a card.
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

/// Render an agent message as markdown-styled transcript lines.
///
/// We parse the message with `pulldown-cmark` and map its events to
/// ratatui `Line`s ourselves (see [`MarkdownBuilder`]). This strips the
/// raw `#`/`**`/backtick/fence markers and styles content with modifiers
/// only (BOLD/ITALIC/DIM), so the output tracks the app theme rather than
/// carrying hardcoded colors. The agent's reply is rendered as plain
/// body text with no speaker label, the way a native agent prints its
/// response; the user's turns are what stand out through their chevron gutter, not the
/// agent's. Empty or marker-only input falls back to a bare `…`.
fn render_agent_message_lines(text: &str) -> Vec<Line<'static>> {
    if text.trim().is_empty() {
        return vec![Line::from("…".to_string())];
    }
    let body = MarkdownBuilder::render(text);
    if body.is_empty() {
        return vec![Line::from("…".to_string())];
    }
    body
}

/// Accumulates `pulldown-cmark` events into themed ratatui lines.
///
/// Inline emphasis pushes/pops modifiers on `mod_stack`; the union of the
/// stack is the active style. Block elements (headings, paragraphs, code
/// blocks) are separated by a single blank line at top level. Code-block
/// content is emitted line-by-line with `DIM`, never the ``` fences.
#[derive(Default)]
struct MarkdownBuilder {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    mod_stack: Vec<Modifier>,
    /// One entry per open list; `Some(n)` is the next ordinal of an
    /// ordered list, `None` an unordered list.
    list_stack: Vec<Option<u64>>,
    in_code_block: bool,
    /// Destination of the innermost open link, so the URL can be appended
    /// (dimmed, in parens) after the link text on close. `None` when the
    /// URL matches the visible text (autolinks), which would just repeat.
    link_dest: Option<String>,
    /// Visible text accumulated inside the open link, for the
    /// autolink-repeat check.
    link_text: String,
    /// Open-table state: cells of the in-progress row; rows are flushed
    /// pipe-separated (the TUI has no column layout pass).
    table_row: Option<Vec<String>>,
    in_table_head: bool,
}

impl MarkdownBuilder {
    fn render(text: &str) -> Vec<Line<'static>> {
        let mut builder = MarkdownBuilder::default();
        let options =
            pulldown_cmark::Options::ENABLE_STRIKETHROUGH | pulldown_cmark::Options::ENABLE_TABLES;
        for event in pulldown_cmark::Parser::new_ext(text, options) {
            builder.handle(event);
        }
        builder.finish()
    }

    fn active_modifier(&self) -> Modifier {
        self.mod_stack
            .iter()
            .fold(Modifier::empty(), |acc, m| acc | *m)
    }

    fn push_span(&mut self, content: &str, extra: Modifier) {
        let style = Style::default().add_modifier(self.active_modifier() | extra);
        self.current.push(Span::styled(content.to_string(), style));
    }

    /// Flush the in-progress line, dropping it if it has no spans.
    fn flush(&mut self) {
        let spans = std::mem::take(&mut self.current);
        if !spans.is_empty() {
            self.lines.push(Line::from(spans));
        }
    }

    /// Flush a code line, preserving blank lines inside the block.
    fn flush_code_line(&mut self) {
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
    }

    /// Insert a blank separator before a new top-level block.
    fn block_break(&mut self) {
        if self.list_stack.is_empty() && !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
    }

    fn handle(&mut self, event: pulldown_cmark::Event) {
        use pulldown_cmark::{Event, Tag, TagEnd};
        match event {
            Event::Start(Tag::Heading { .. }) => {
                self.block_break();
                self.mod_stack.push(Modifier::BOLD);
            }
            Event::End(TagEnd::Heading(_)) => {
                self.flush();
                self.mod_stack.pop();
            }
            Event::Start(Tag::Paragraph) => self.block_break(),
            Event::End(TagEnd::Paragraph) => self.flush(),
            Event::Start(Tag::Strong) => self.mod_stack.push(Modifier::BOLD),
            Event::Start(Tag::Emphasis) => self.mod_stack.push(Modifier::ITALIC),
            Event::Start(Tag::Strikethrough) => self.mod_stack.push(Modifier::CROSSED_OUT),
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                self.mod_stack.pop();
            }
            Event::Start(Tag::CodeBlock(_)) => {
                self.block_break();
                self.in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.flush();
                self.in_code_block = false;
            }
            Event::Start(Tag::List(first)) => self.list_stack.push(first),
            Event::End(TagEnd::List(_)) => {
                self.list_stack.pop();
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.link_dest = Some(dest_url.to_string());
                self.link_text.clear();
            }
            Event::End(TagEnd::Link) => {
                // Append the URL (dimmed, in parens) unless it repeats the
                // visible text, as an autolink or `[url](url)` would.
                if let Some(dest) = self.link_dest.take() {
                    let text = std::mem::take(&mut self.link_text);
                    if dest != text && !dest.is_empty() {
                        self.push_span(&format!(" ({dest})"), Modifier::DIM);
                    }
                }
            }
            Event::Start(Tag::Table(_)) => self.block_break(),
            Event::End(TagEnd::Table) => {
                self.table_row = None;
            }
            Event::Start(Tag::TableHead) => {
                self.in_table_head = true;
                self.table_row = Some(Vec::new());
            }
            Event::End(TagEnd::TableHead) => {
                self.flush_table_row(Modifier::BOLD);
                self.in_table_head = false;
            }
            Event::Start(Tag::TableRow) => {
                self.table_row = Some(Vec::new());
            }
            Event::End(TagEnd::TableRow) => {
                self.flush_table_row(Modifier::empty());
            }
            Event::Start(Tag::TableCell) => {
                if let Some(row) = self.table_row.as_mut() {
                    row.push(String::new());
                }
            }
            Event::End(TagEnd::TableCell) => {}
            Event::Start(Tag::Item) => {
                self.flush();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.current.push(Span::raw(format!("{indent}{marker}")));
            }
            Event::End(TagEnd::Item) => self.flush(),
            Event::Text(text) => {
                if let Some(row) = self.table_row.as_mut() {
                    if let Some(cell) = row.last_mut() {
                        cell.push_str(&text);
                    }
                } else if self.in_code_block {
                    self.push_code_text(&text);
                } else {
                    if self.link_dest.is_some() {
                        self.link_text.push_str(&text);
                    }
                    self.push_span(&text, Modifier::empty());
                }
            }
            Event::Code(text) => {
                if let Some(row) = self.table_row.as_mut() {
                    if let Some(cell) = row.last_mut() {
                        cell.push_str(&text);
                    }
                } else {
                    if self.link_dest.is_some() {
                        self.link_text.push_str(&text);
                    }
                    self.push_span(&text, Modifier::DIM);
                }
            }
            // A soft break (single newline in the source) renders as a
            // real line break, matching how Claude Code prints agent
            // output: a reply formatted "one item per line" must not
            // collapse into one wrapped paragraph, which is what the
            // markdown-standard space treatment did.
            Event::SoftBreak if !self.in_code_block => self.flush(),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.block_break();
                self.lines.push(Line::from("───"));
            }
            _ => {}
        }
    }

    /// Flush the in-progress table row as one pipe-separated line. The
    /// TUI markdown pass is single-sweep, so cells are not column-aligned;
    /// the head row is bolded and rows keep their reading order.
    fn flush_table_row(&mut self, extra: Modifier) {
        let Some(cells) = self.table_row.take() else {
            return;
        };
        if cells.is_empty() {
            return;
        }
        let style = Style::default().add_modifier(self.active_modifier() | extra);
        self.lines
            .push(Line::from(Span::styled(cells.join(" │ "), style)));
    }

    /// Split code-block text on newlines, flushing one styled line per
    /// row so multi-line blocks render distinctly without fence markers.
    fn push_code_text(&mut self, text: &str) {
        let style = Style::default().add_modifier(Modifier::DIM);
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                self.current.push(Span::styled(part.to_string(), style));
            }
            if parts.peek().is_some() {
                self.flush_code_line();
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        while self.lines.last().is_some_and(|l| l.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

fn transcript_lines<'a>(
    transcript: &'a AcpTranscript,
    theme: &Theme,
    path_roots: Option<&SessionPathRoots>,
) -> Vec<Line<'a>> {
    let mut out: Vec<Line<'a>> = Vec::new();
    for row in &transcript.rows {
        match row {
            ActivityRow::UserPrompt(text) => {
                out.extend(user_message_lines(text, theme));
                out.push(Line::default());
            }
            ActivityRow::AgentMessage(text) => {
                out.extend(agent_message_lines(text, theme));
                out.push(Line::default());
            }
            ActivityRow::ToolCall(tool) => {
                out.extend(render_tool_lines(tool, theme, path_roots));
                out.push(Line::default());
            }
            ActivityRow::Approval(row) => {
                let (marker, label) = match row.decision {
                    Some(ApprovalDecision::Allow) => ("✓", "Allowed once"),
                    Some(ApprovalDecision::AllowAlways) => ("✓", "Always allowed"),
                    Some(ApprovalDecision::Deny) => ("✕", "Denied"),
                    Some(ApprovalDecision::Cancelled) => ("·", "Cancelled"),
                    // Pending approvals live in the modal shelf below the
                    // transcript, so they do not duplicate here.
                    None => continue,
                };
                out.push(Line::from(Span::styled(
                    format!("{marker} {label} · {}", row.title),
                    Style::default().fg(theme.hint),
                )));
                out.push(Line::default());
            }
            ActivityRow::ElicitationAnswer(answers) => {
                // The user's answer is one of their turns, so it reads
                // the same as a user prompt: highlighted, no label.
                for answer in answers {
                    out.extend(user_message_lines(
                        &format!("{}: {}", answer.question, answer.answer),
                        theme,
                    ));
                }
                out.push(Line::default());
            }
            ActivityRow::Note { kind, text } => {
                let modifier = match kind {
                    NoteKind::Info => Modifier::DIM,
                    NoteKind::Warning => Modifier::BOLD,
                    NoteKind::Error => Modifier::BOLD,
                };
                out.push(Line::from(Span::styled(
                    format!("· {text}"),
                    Style::default().add_modifier(modifier),
                )));
                out.push(Line::default());
            }
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

/// Return the first `max_chars` characters of `s`, or `None` if `s`
/// is already short enough. Char-safe so an LLM response that places a
/// multi-byte codepoint at the truncation boundary doesn't panic the
/// TUI (byte-slicing `&s[..N]` would).
fn truncate_chars(s: &str, max_chars: usize) -> Option<String> {
    let mut iter = s.char_indices();
    if let Some((byte_idx, _)) = iter.nth(max_chars) {
        Some(s[..byte_idx].to_string())
    } else {
        None
    }
}

/// Arg-name variants the agents use for a tool's primary path, command,
/// and edit before/after text. Mirrors the web structured view's `pickStr` key
/// lists in `web/src/components/acp/ToolCards.tsx` so the TUI and the
/// dashboard surface the same field across agent versions.
const PATH_KEYS: &[&str] = &["path", "file_path", "filePath", "filename"];
const OLD_KEYS: &[&str] = &["old_string", "oldString", "old_str"];
const NEW_KEYS: &[&str] = &["new_string", "newString", "new_str", "content"];
const CMD_KEYS: &[&str] = &["command", "cmd", "args"];

/// +/- lines beyond this budget collapse into a "+N more" footer so a
/// large Edit can't flood the transcript on a narrow terminal.
const TOOL_DIFF_MAX_LINES: usize = 20;
/// Read/execute output previews are capped to this many lines.
const TOOL_PREVIEW_MAX_LINES: usize = 12;

/// Render one tool call. Dispatches on `tool.kind` (the lowercased ACP
/// `ToolKind`) to a per-kind body; any kind we don't special-case, or
/// one whose args don't parse into the expected shape, falls back to the
/// generic one-liner so unknown tools still render.
fn render_tool_lines(
    tool: &ToolCallRow,
    theme: &Theme,
    path_roots: Option<&SessionPathRoots>,
) -> Vec<Line<'static>> {
    if tool
        .completed
        .as_ref()
        .is_some_and(|completion| completion.ok)
    {
        return vec![compact_tool_line(tool, theme, path_roots)];
    }
    let mut lines = Vec::new();
    let header = format!(
        "tool {} · {}",
        match tool.completed.as_ref() {
            None => "▶",
            Some(c) if c.ok => "✓",
            Some(_) => "✗",
        },
        tool.name
    );
    lines.push(Line::from(Span::styled(
        header,
        Style::default().add_modifier(Modifier::BOLD),
    )));

    // Structured per-file diffs win over the args-derived compact diff:
    // they cover multi-file patches (Codex apply_patch) and tools whose
    // args carry no old/new text. Any tool kind can ship them.
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
        match tool.kind.as_str() {
            "edit" | "write" | "read" | "delete" | "move" => pick_str(args.as_ref(), PATH_KEYS)
                .map(|path| relative_display_path(path, path_roots)),
            "execute" => pick_str(args.as_ref(), CMD_KEYS)
                .and_then(|command| command.lines().next())
                .map(ToString::to_string),
            _ => None,
        }
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

/// Per-file compact diffs from the structured `tool_call.diffs` payload:
/// each file's (shortened) path followed by its +/- lines, sharing the
/// same budget as the args-derived diff so a large patch stays bounded.
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

/// Parse `args_preview` as a JSON object. Mirrors the web structured view's
/// `parseJsonObject`: returns `None` for non-object, unparsable, or
/// truncated payloads so callers fall back to the generic renderer.
fn parse_args_object(args: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// First string-valued key from `keys`, mirroring the web `pickStr`.
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

/// Edit/Write: the file path plus a compact line diff built from the
/// `old_string`/`new_string` (or `content`) args, the same source the
/// web Edit card uses. `None` when no after-text arg is present (the
/// generic renderer then handles it).
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

/// Compact line diff in the style of `src/tui/diff/render.rs`: only the
/// changed (`+`/`-`) lines, bounded to `TOOL_DIFF_MAX_LINES`.
fn diff_lines(old: &str, new: &str, theme: &Theme) -> Vec<Line<'static>> {
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    let mut hidden = 0usize;
    for change in diff.iter_all_changes() {
        let (sign, style) = match change.tag() {
            ChangeTag::Delete => ("-", Style::default().fg(theme.diff_delete)),
            ChangeTag::Insert => ("+", Style::default().fg(theme.diff_add)),
            // Context lines carry no signal in the compact card; drop them.
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

/// Execute: the command plus a bounded preview of its output.
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

/// Read: the file path plus a bounded preview of the read content.
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

/// Delete: just the target path.
fn render_delete_body(
    args: Option<&serde_json::Map<String, serde_json::Value>>,
    path_roots: Option<&SessionPathRoots>,
) -> Option<Vec<Line<'static>>> {
    let path = relative_display_path(pick_str(args, PATH_KEYS)?, path_roots);
    Some(vec![Line::from(format!("  {path}"))])
}

/// Bounded preview of a tool's completion content, shared by the read
/// and execute cards. Falls back to a status word before completion or
/// when the agent shipped no body.
fn output_preview_lines(tool: &ToolCallRow) -> Vec<Line<'static>> {
    let Some(completion) = &tool.completed else {
        return vec![Line::from(Span::styled(
            "  (running…)",
            Style::default().add_modifier(Modifier::DIM),
        ))];
    };
    if completion.content.is_empty() {
        let msg = if completion.ok {
            "  (no output)"
        } else {
            "  (tool failed; press `o` for details)"
        };
        return vec![Line::from(msg.to_string())];
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

/// Tool output as display lines, interpreting ANSI SGR color/style
/// sequences the way the web execute card does (a `cargo test` or
/// `eslint` run keeps its colors instead of leaking `\x1b[31m`
/// escapes). Plain text takes the cheap path untouched; a parse
/// failure falls back to the raw text rather than dropping output.
fn styled_output_lines(content: &str) -> Vec<Line<'static>> {
    if content.contains('\u{1b}') {
        if let Ok(text) = content.into_text() {
            return text.lines;
        }
    }
    content.lines().map(|l| Line::from(l.to_string())).collect()
}

/// Generic one-liner fallback for unknown tool kinds: the truncated args
/// preview plus a truncated output snapshot. This is the pre-#1702
/// rendering, preserved verbatim so unrecognized tools are unchanged.
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
            let msg = if completion.ok {
                "  (no output)"
            } else {
                "  (tool failed; press `o` for details)"
            };
            lines.push(Line::from(msg.to_string()));
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

fn help_hint(focus: Focus) -> &'static str {
    match focus {
        // The composer is the resting state, so keep its hint to the two
        // things that aren't obvious from the placeholder: how to send
        // and how to leave. Scrolling is just the wheel / PageUp-Down;
        // Ctrl+Q leaves the view (Esc interrupts the agent, like native).
        Focus::Composer => " Enter to send · Ctrl+Q to exit ",
        Focus::Transcript => " scroll to read · Ctrl+Q to exit ",
        Focus::Approval => " a allow · A always · d deny · Esc stop ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::Source;
    use crate::acp::client::{DaemonEndpoint, HttpClient};

    fn test_state() -> StructuredViewState {
        let endpoint = DaemonEndpoint {
            base_url: "http://127.0.0.1:8080".into(),
            token: None,
            source: Source::Env,
        };
        let http = HttpClient::new(endpoint.clone()).unwrap();
        StructuredViewState::new("s-1".into(), endpoint, http, None)
    }

    #[test]
    fn queued_strip_height_is_zero_when_empty() {
        let state = test_state();
        assert_eq!(queued_strip_height(&state), 0);
    }

    #[test]
    fn queued_strip_stays_one_row_with_any_number_of_entries() {
        let mut state = test_state();
        state.queue.push("one".into());
        assert_eq!(queued_strip_height(&state), 1);
        state.queue.push("two".into());
        state.queue.push("three".into());
        assert_eq!(queued_strip_height(&state), 1);
        state.queue.push("four".into());
        state.queue.push("five".into());
        assert_eq!(queued_strip_height(&state), 1);
    }

    #[test]
    fn composer_height_reserves_one_prompt_separator() {
        let mut state = test_state();
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
    fn approval_actions_read_as_key_hints_not_buttons() {
        let line = approval_actions_line(&Theme::default(), true);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("a allow once"), "{text:?}");
        assert!(text.contains("A always"), "{text:?}");
        assert!(text.contains("d deny"), "{text:?}");
        assert!(!text.contains('['), "button chrome leaked: {text:?}");
        assert!(!text.contains(']'), "button chrome leaked: {text:?}");
    }

    /// Wrap one line and return the resulting row count.
    fn wrap_rows(line: Line<'static>, width: u16) -> usize {
        let mut out = Vec::new();
        wrap_line_into(line, width, &mut out);
        out.len()
    }

    #[test]
    fn wrap_hard_breaks_unbreakable_text() {
        // 40 chars, no spaces, at width 10: four hard-broken rows.
        assert_eq!(wrap_rows(Line::from("a".repeat(40)), 10), 4);
    }

    #[test]
    fn wrap_keeps_empty_line_as_one_row() {
        assert_eq!(wrap_rows(Line::default(), 10), 1);
    }

    #[test]
    fn wrap_survives_zero_width() {
        // Degenerate area (e.g. during teardown): the width floors to 1
        // instead of dividing by zero.
        assert_eq!(wrap_rows(Line::from("x"), 0), 1);
    }

    #[test]
    fn wrap_streaming_growth_adds_rows() {
        // Regression for the agent-message auto-scroll bug: as a single
        // logical line grows, the wrapped row count must grow so
        // `scroll_offset = u16::MAX` keeps tracking the bottom.
        assert!(
            wrap_rows(Line::from("a".repeat(200)), 40) > wrap_rows(Line::from("a".repeat(20)), 40)
        );
    }

    #[test]
    fn wrap_breaks_at_word_boundary_and_keeps_styles() {
        let styled = Style::default().add_modifier(Modifier::BOLD);
        let line = Line::from(vec![
            Span::raw("hello brave "),
            Span::styled("new world", styled),
        ]);
        let mut out = Vec::new();
        wrap_line_into(line, 12, &mut out);
        let texts: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        // Breaks at the space before "new", dropping the break space.
        assert_eq!(
            texts,
            vec!["hello brave".to_string(), "new world".to_string()]
        );
        // The bold style survives on the wrapped-away words.
        assert!(out[1]
            .spans
            .iter()
            .any(|s| s.style == styled && s.content.contains("new")));
    }

    #[test]
    fn truncate_chars_returns_none_when_already_short() {
        assert_eq!(truncate_chars("hi", 10), None);
    }

    #[test]
    fn truncate_chars_respects_utf8_codepoint_boundaries() {
        // Regression for the byte-slice panic: a 4-byte codepoint
        // straddling the requested byte boundary used to crash the
        // TUI with `byte index N is not a char boundary`.
        // 3 ASCII + 4-byte emoji (U+1F600) repeated; ask for 4 chars.
        let s = "abc😀def😀ghi😀";
        let head = truncate_chars(s, 4).expect("longer than 4 chars");
        assert_eq!(head, "abc😀");
        assert!(s.chars().count() > 4);
    }

    #[test]
    fn truncate_chars_handles_pure_multibyte_input() {
        // Pure non-ASCII (CJK ideographs are 3 bytes each in UTF-8).
        let s = "日本語のテスト";
        let head = truncate_chars(s, 3).expect("longer than 3 chars");
        assert_eq!(head, "日本語");
    }

    /// Concatenated text of every span on a line, gutter included.
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// True if any span on the line carries the given modifier.
    fn line_has_modifier(line: &Line, modifier: Modifier) -> bool {
        line.spans
            .iter()
            .any(|s| s.style.add_modifier.contains(modifier))
    }

    /// No span on any rendered line should keep a foreground color, so
    /// markdown output tracks the app theme instead of tui-markdown's
    /// built-in palette.
    fn no_span_has_fg(lines: &[Line]) -> bool {
        lines
            .iter()
            .all(|l| l.spans.iter().all(|s| s.style.fg.is_none()))
    }

    #[test]
    fn agent_message_styles_markdown_and_drops_raw_markers() {
        let lines = render_agent_message_lines("# Title\n\n**bold** and `code`");
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        // Raw markdown punctuation is consumed by the parser.
        assert!(!joined.contains('#'), "heading marker leaked: {joined:?}");
        assert!(!joined.contains("**"), "bold marker leaked: {joined:?}");
        assert!(!joined.contains('`'), "code-span marker leaked: {joined:?}");
        // Visible text survives.
        assert!(joined.contains("Title"));
        assert!(joined.contains("bold"));
        assert!(joined.contains("code"));
        // At least one line carries BOLD styling (heading and/or strong).
        assert!(
            lines.iter().any(|l| line_has_modifier(l, Modifier::BOLD)),
            "expected BOLD styling somewhere: {lines:?}"
        );
        // Colors are stripped so the theme owns the palette.
        assert!(no_span_has_fg(&lines), "fg color leaked: {lines:?}");
    }

    #[test]
    fn agent_message_renders_fenced_code_without_fence_lines() {
        let lines = render_agent_message_lines("before\n\n```\nlet x = 1;\n```\n\nafter");
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // The ``` fence markers must not appear as literal text.
        assert!(
            texts.iter().all(|t| !t.contains("```")),
            "fence markers leaked: {texts:?}"
        );
        // Code content and surrounding prose are present.
        let joined = texts.join("\n");
        assert!(joined.contains("let x = 1;"));
        assert!(joined.contains("before"));
        assert!(joined.contains("after"));
    }

    #[test]
    fn agent_message_honors_single_newlines() {
        // A reply formatted one-item-per-line must keep its line breaks
        // (markdown soft breaks), matching how a native agent prints it,
        // instead of collapsing into one wrapped paragraph.
        let lines = render_agent_message_lines("1\n2\n3");
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts.iter().any(|t| t.trim() == "1") && texts.iter().any(|t| t.trim() == "3"),
            "each source line should render as its own row: {texts:?}"
        );
    }

    #[test]
    fn agent_message_has_no_speaker_label() {
        // The agent's reply is plain body text: no "aoe" gutter, no
        // speaker label. The user's turns are what stand out, not this.
        let lines = render_agent_message_lines("line one\n\nline two");
        for line in &lines {
            let text = line_text(line);
            assert!(
                !text.trim_start().starts_with("aoe"),
                "agent message must carry no speaker label: {text:?}"
            );
        }
        assert!(line_text(&lines[0]).contains("line one"));
    }

    #[test]
    fn user_message_uses_one_chevron_and_no_background() {
        use crate::tui::styles::load_theme;
        let theme = load_theme("empire");
        let lines = user_message_lines("first\nsecond", &theme);
        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[0]), "› first");
        assert_eq!(line_text(&lines[1]), "  second");
        assert!(!line_text(&lines[0]).contains("you"));
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.bg.is_none()),
            "user turns must stay open, not render as filled cards: {lines:?}"
        );
    }

    use crate::acp::state::AvailableCommand;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn cmd(name: &str, desc: &str) -> AvailableCommand {
        AvailableCommand {
            name: name.to_string(),
            description: desc.to_string(),
            accepts_input: false,
        }
    }

    #[test]
    fn agent_message_renders_list_markers_without_dashes() {
        let lines = render_agent_message_lines("- one\n- two\n\n1. first\n2. second");
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let joined = texts.join("\n");
        // Bullet items get `•`, not the raw `-` marker.
        assert!(joined.contains("• one"), "{texts:?}");
        assert!(joined.contains("• two"), "{texts:?}");
        // Ordered items keep their numbers.
        assert!(joined.contains("1. first"), "{texts:?}");
        assert!(joined.contains("2. second"), "{texts:?}");
        // No line is just the raw `- ` source marker.
        assert!(
            texts.iter().all(|t| !t.trim_start().starts_with("- ")),
            "{texts:?}"
        );
    }

    #[test]
    fn agent_message_empty_falls_back_to_placeholder() {
        for input in ["", "   ", "\n\n"] {
            let lines = render_agent_message_lines(input);
            assert_eq!(lines.len(), 1, "input {input:?}");
            assert_eq!(line_text(&lines[0]), "…");
        }
    }

    #[test]
    fn picker_lines_window_follows_selection_past_cap() {
        let cmds: Vec<AvailableCommand> = (0..10).map(|i| cmd(&format!("c{i}"), "")).collect();
        let refs: Vec<&AvailableCommand> = cmds.iter().collect();
        // Selecting row 9 with a 3-row cap must keep it inside the window.
        let lines = picker_lines(&refs, 9, 3);
        assert_eq!(lines.len(), 3);
        // Window should be rows 7,8,9; row 9 is the last visible line.
        let last = &lines[2];
        let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("/c9"), "expected /c9 in {text:?}");
        assert!(text.starts_with("▶"), "selected row marked: {text:?}");
    }

    #[test]
    fn render_shows_slash_picker_overlay() {
        let endpoint = DaemonEndpoint {
            base_url: "http://127.0.0.1:8080".to_string(),
            token: None,
            source: Source::LocalDaemon,
        };
        let http = HttpClient::new(endpoint.clone()).expect("http client");
        let mut state = StructuredViewState::new("sess".to_string(), endpoint, http, None);
        state.focus = Focus::Composer;
        state.transcript.available_commands =
            vec![cmd("compact", "shrink context"), cmd("clear", "wipe")];
        state.composer.insert_str("/comp");
        assert!(state.slash_picker_open());

        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                render(f, f.area(), &theme, &state, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("Commands"), "picker title missing");
        assert!(dump.contains("/compact"), "command label missing");
        assert!(dump.contains('▶'), "selection marker missing");
    }

    #[test]
    fn short_terminal_keeps_selected_row_visible() {
        // Regression: on a short terminal the popup's drawable height is
        // tiny, but the window was sized to SLASH_PICKER_MAX_ROWS, so a
        // bottom selection painted above the fold and vanished. Render a
        // 9-row terminal with many commands, select the last, and assert
        // the selected label + marker actually paint.
        let endpoint = DaemonEndpoint {
            base_url: "http://127.0.0.1:8080".to_string(),
            token: None,
            source: Source::LocalDaemon,
        };
        let http = HttpClient::new(endpoint.clone()).expect("http client");
        let mut state = StructuredViewState::new("sess".to_string(), endpoint, http, None);
        state.focus = Focus::Composer;
        state.transcript.available_commands =
            (0..12).map(|i| cmd(&format!("cmd{i:02}"), "")).collect();
        state.composer.insert_str("/cmd");
        assert!(state.slash_picker_open());
        // Drive the highlight to the last match.
        let last = state.slash_matches().len() - 1;
        state.move_slash_selection(last as i32);
        let last_name = state.slash_matches()[last].name.clone();

        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let backend = TestBackend::new(40, 9);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                render(f, f.area(), &theme, &state, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            dump.contains('▶'),
            "selection marker missing on short terminal"
        );
        assert!(
            dump.contains(&format!("/{last_name}")),
            "selected row /{last_name} scrolled off-screen: {dump:?}"
        );
    }

    fn tool_row(kind: &str, args: &str, completion: Option<(bool, &str)>) -> ToolCallRow {
        use super::super::reducer::ToolCompletion;
        ToolCallRow {
            name: "Tool".into(),
            kind: kind.into(),
            args: args.into(),
            diffs: Vec::new(),
            completed: completion.map(|(ok, content)| ToolCompletion {
                ok,
                content: content.into(),
            }),
        }
    }

    #[test]
    fn structured_diffs_win_over_args_derived_diff() {
        use crate::acp::state::DiffPreview;
        let mut row = tool_row(
            "edit",
            r#"{"file_path":"args.rs","old_string":"stale","new_string":"ignored"}"#,
            None,
        );
        row.diffs = vec![
            DiffPreview {
                path: "src/one.rs".into(),
                old_text: Some("let a = 1;".into()),
                new_text: Some("let a = 2;".into()),
                created_at: chrono::Utc::now(),
            },
            DiffPreview {
                path: "src/two.rs".into(),
                old_text: None,
                new_text: Some("brand new".into()),
                created_at: chrono::Utc::now(),
            },
        ];
        let out = joined(&render_tool_lines(&row, &Theme::default(), None));
        // Both files render with their own diff bodies.
        assert!(out.contains("src/one.rs"), "{out:?}");
        assert!(out.contains("- let a = 1;"), "{out:?}");
        assert!(out.contains("+ let a = 2;"), "{out:?}");
        assert!(out.contains("src/two.rs"), "{out:?}");
        assert!(out.contains("+ brand new"), "{out:?}");
        // The args-derived diff is superseded, not rendered too.
        assert!(!out.contains("stale"), "{out:?}");
    }

    #[test]
    fn markdown_links_show_text_and_dimmed_url() {
        let lines = render_agent_message_lines("see [the docs](https://example.com/d) here");
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("the docs"), "{joined:?}");
        assert!(joined.contains("(https://example.com/d)"), "{joined:?}");
        // Raw markdown link punctuation is consumed.
        assert!(!joined.contains("["), "{joined:?}");
    }

    #[test]
    fn markdown_autolink_url_not_repeated() {
        let lines = render_agent_message_lines("see <https://example.com> here");
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert_eq!(
            joined.matches("https://example.com").count(),
            1,
            "autolink URL must not repeat: {joined:?}"
        );
    }

    #[test]
    fn markdown_tables_render_rows_without_raw_pipes_leaking() {
        let lines = render_agent_message_lines(
            "| Name | Value |\n| --- | --- |\n| alpha | 1 |\n| beta | 2 |",
        );
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let joined = texts.join("\n");
        assert!(joined.contains("Name │ Value"), "{texts:?}");
        assert!(joined.contains("alpha │ 1"), "{texts:?}");
        assert!(joined.contains("beta │ 2"), "{texts:?}");
        // The separator row (---) is consumed by the parser.
        assert!(!joined.contains("---"), "{texts:?}");
    }

    fn joined(lines: &[Line]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn transcript_renders_elicitation_answers_as_user_rows() {
        use crate::acp::elicitations::ElicitationAnswer;
        let mut t = AcpTranscript::new("s-1");
        t.rows.push(ActivityRow::ElicitationAnswer(vec![
            ElicitationAnswer {
                question: "Proceed?".into(),
                answer: "Yes".into(),
            },
            ElicitationAnswer {
                question: "Mode".into(),
                answer: "Fast".into(),
            },
        ]));
        let out = joined(&transcript_lines(&t, &Theme::default(), None));
        // Rendered as user turns: chevron gutter, no "you" label.
        assert!(out.contains("Proceed?: Yes"), "{out:?}");
        assert!(out.contains("Mode: Fast"), "{out:?}");
        assert!(!out.contains("you  ▸"), "{out:?}");
    }

    #[test]
    fn transcript_hides_pending_approval_and_records_resolved_without_nonce() {
        use super::super::reducer::ApprovalRow;

        let mut t = AcpTranscript::new("s-1");
        t.rows.push(ActivityRow::Approval(ApprovalRow {
            nonce: "internal-pending".into(),
            title: "Read file".into(),
            kind: "read".into(),
            args: r#"{"path":"src/lib.rs"}"#.into(),
            destructive: false,
            decision: None,
        }));
        t.rows.push(ActivityRow::Approval(ApprovalRow {
            nonce: "internal-resolved".into(),
            title: "Edit file".into(),
            kind: "edit".into(),
            args: r#"{"path":"src/lib.rs"}"#.into(),
            destructive: false,
            decision: Some(ApprovalDecision::Allow),
        }));
        let out = joined(&transcript_lines(&t, &Theme::default(), None));
        assert!(
            !out.contains("Read file"),
            "pending request duplicated: {out:?}"
        );
        assert!(out.contains("Allowed once · Edit file"), "{out:?}");
        assert!(!out.contains("internal-"), "nonce leaked: {out:?}");
    }

    #[test]
    fn edit_kind_renders_added_and_removed_diff_lines() {
        let row = tool_row(
            "edit",
            r#"{"file_path":"src/a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#,
            None,
        );
        let out = joined(&render_tool_lines(&row, &Theme::default(), None));
        assert!(out.contains("src/a.rs"), "path missing: {out:?}");
        assert!(
            out.contains("- let x = 1;"),
            "removed line missing: {out:?}"
        );
        assert!(out.contains("+ let x = 2;"), "added line missing: {out:?}");
    }

    #[test]
    fn write_kind_renders_all_inserts_from_content() {
        let row = tool_row(
            "write",
            r#"{"file_path":"new.txt","content":"line one\nline two"}"#,
            None,
        );
        let out = joined(&render_tool_lines(&row, &Theme::default(), None));
        assert!(out.contains("new.txt"));
        assert!(out.contains("+ line one"), "{out:?}");
        assert!(out.contains("+ line two"), "{out:?}");
    }

    #[test]
    fn edit_diff_caps_at_budget_with_more_footer() {
        // 30 changed lines exceed TOOL_DIFF_MAX_LINES (20).
        let new_body: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let args =
            serde_json::json!({ "file_path": "big.txt", "old_string": "", "new_string": new_body });
        let row = tool_row("edit", &args.to_string(), None);
        let lines = render_tool_lines(&row, &Theme::default(), None);
        let plus = lines
            .iter()
            .filter(|l| line_text(l).trim_start().starts_with("+ "))
            .count();
        assert_eq!(plus, TOOL_DIFF_MAX_LINES, "diff not capped: {plus}");
        assert!(
            joined(&lines).contains("+10 more diff lines"),
            "missing more-footer: {:?}",
            joined(&lines)
        );
    }

    #[test]
    fn successful_execute_collapses_to_command_summary() {
        let row = tool_row(
            "execute",
            r#"{"command":"ls -la"}"#,
            Some((true, "file_a\nfile_b")),
        );
        let out = joined(&render_tool_lines(&row, &Theme::default(), None));
        assert!(out.contains("✓ Tool · ls -la"), "summary missing: {out:?}");
        assert!(
            !out.contains("file_a"),
            "output should be collapsed: {out:?}"
        );
    }

    #[test]
    fn successful_read_collapses_to_path_summary() {
        let row = tool_row(
            "read",
            r#"{"path":"src/lib.rs"}"#,
            Some((true, "pub fn main() {}")),
        );
        let out = joined(&render_tool_lines(&row, &Theme::default(), None));
        assert!(out.contains("src/lib.rs"), "path missing: {out:?}");
        assert!(
            !out.contains("pub fn main()"),
            "content should be collapsed: {out:?}"
        );
    }

    #[test]
    fn delete_kind_renders_only_path() {
        let row = tool_row("delete", r#"{"path":"old.txt"}"#, Some((true, "")));
        let out = joined(&render_tool_lines(&row, &Theme::default(), None));
        assert!(out.contains("old.txt"), "path missing: {out:?}");
        // No diff gutters for a delete.
        assert!(!out.contains("+ "), "{out:?}");
        assert!(!out.contains("- "), "{out:?}");
    }

    fn path_roots() -> SessionPathRoots {
        SessionPathRoots {
            id: "s-1".into(),
            project_path: "/Users/me/.aoe/worktrees/feat".into(),
            main_repo_path: Some("/Users/me/repo".into()),
            workspace_repos: vec![crate::acp::session_paths::WorkspaceRepoRoot {
                name: "api".into(),
                source_path: "/Users/me/api".into(),
            }],
        }
    }

    #[test]
    fn edit_path_under_worktree_renders_repo_relative() {
        let row = tool_row(
            "edit",
            r#"{"file_path":"/Users/me/.aoe/worktrees/feat/src/a.rs","old_string":"a","new_string":"b"}"#,
            None,
        );
        let roots = path_roots();
        let out = joined(&render_tool_lines(&row, &Theme::default(), Some(&roots)));
        assert!(out.contains("src/a.rs"), "relative path missing: {out:?}");
        assert!(
            !out.contains("/Users/me/.aoe/worktrees/feat/src/a.rs"),
            "absolute path leaked: {out:?}"
        );
    }

    #[test]
    fn read_path_under_workspace_repo_renders_repo_prefixed() {
        let row = tool_row(
            "read",
            r#"{"path":"/Users/me/api/src/h.ts"}"#,
            Some((true, "export const h = 1;")),
        );
        let roots = path_roots();
        let out = joined(&render_tool_lines(&row, &Theme::default(), Some(&roots)));
        assert!(out.contains("api/src/h.ts"), "repo path missing: {out:?}");
        assert!(
            !out.contains("/Users/me/api/src/h.ts"),
            "absolute path leaked: {out:?}"
        );
    }

    #[test]
    fn delete_path_outside_roots_stays_absolute() {
        let row = tool_row("delete", r#"{"path":"/etc/hosts"}"#, Some((true, "")));
        let roots = path_roots();
        let out = joined(&render_tool_lines(&row, &Theme::default(), Some(&roots)));
        assert!(
            out.contains("/etc/hosts"),
            "absolute fallback missing: {out:?}"
        );
    }

    #[test]
    fn sibling_prefix_path_stays_absolute() {
        let row = tool_row(
            "read",
            r#"{"path":"/Users/me/repo_old/src/lib.rs"}"#,
            Some((true, "pub fn main() {}")),
        );
        let roots = path_roots();
        let out = joined(&render_tool_lines(&row, &Theme::default(), Some(&roots)));
        assert!(
            out.contains("/Users/me/repo_old/src/lib.rs"),
            "sibling path should stay absolute: {out:?}"
        );
    }

    #[test]
    fn format_tokens_matches_web_thresholds() {
        assert_eq!(format_tokens(842), "842");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(9_940), "9.9k");
        assert_eq!(format_tokens(12_300), "12k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(1_250_000), "1.25M");
        assert_eq!(format_tokens(12_500_000), "12.5M");
    }

    #[test]
    fn format_usage_includes_percent_and_cost() {
        use crate::acp::state::UsageCost;
        let usage = SessionUsage {
            used: 12_300,
            size: 200_000,
            cost: Some(UsageCost {
                amount: 0.4231,
                currency: "USD".into(),
            }),
        };
        assert_eq!(format_usage(&usage), "12k/200k (6%) · $0.4231");
        let no_cost = SessionUsage {
            used: 100_000,
            size: 200_000,
            cost: None,
        };
        assert_eq!(format_usage(&no_cost), "100k/200k (50%)");
        let eur = SessionUsage {
            used: 1_000,
            size: 200_000,
            cost: Some(UsageCost {
                amount: 2.5,
                currency: "EUR".into(),
            }),
        };
        assert_eq!(format_usage(&eur), "1.0k/200k (1%) · 2.50 EUR");
    }

    #[test]
    fn usage_percent_survives_zero_size() {
        let usage = SessionUsage {
            used: 5,
            size: 0,
            cost: None,
        };
        assert_eq!(usage_percent(&usage), 0);
    }

    #[test]
    fn usage_percent_caps_at_100_when_used_exceeds_size() {
        // Some agents transiently report used > size (e.g. right before a
        // compaction lands); "105%" reads as a rendering bug (#2927).
        let usage = SessionUsage {
            used: 210_000,
            size: 200_000,
            cost: None,
        };
        assert_eq!(usage_percent(&usage), 100);
    }

    #[test]
    fn status_line_renders_usage_meter() {
        let mut state = test_state();
        state.transcript.usage = Some(SessionUsage {
            used: 12_300,
            size: 200_000,
            cost: None,
        });
        let dump = render_dump(&state, 80, 24);
        assert!(dump.contains("12k/200k (6%)"), "usage meter missing");
    }

    #[test]
    fn execute_output_interprets_ansi_colors() {
        // Red "FAILED" via SGR: the escape bytes must not leak into the
        // rendered text, and the color must survive onto the span.
        let row = tool_row(
            "execute",
            r#"{"command":"cargo test"}"#,
            Some((false, "test result: \u{1b}[31mFAILED\u{1b}[0m. 1 failed")),
        );
        let lines = render_tool_lines(&row, &Theme::default(), None);
        let out = joined(&lines);
        assert!(!out.contains('\u{1b}'), "escape bytes leaked: {out:?}");
        assert!(out.contains("FAILED"), "text missing: {out:?}");
        let red_span = lines.iter().flat_map(|l| &l.spans).find(|s| {
            s.content.contains("FAILED") && s.style.fg == Some(ratatui::style::Color::Red)
        });
        assert!(red_span.is_some(), "red SGR color dropped: {lines:?}");
    }

    #[test]
    fn generic_output_interprets_ansi_colors() {
        let row = tool_row(
            "fetch",
            "https://example.com",
            Some((false, "\u{1b}[32m200 OK\u{1b}[0m")),
        );
        let out = joined(&render_tool_lines(&row, &Theme::default(), None));
        assert!(!out.contains('\u{1b}'), "escape bytes leaked: {out:?}");
        assert!(out.contains("200 OK"), "{out:?}");
    }

    #[test]
    fn plain_output_unchanged_by_ansi_path() {
        let lines = styled_output_lines("plain\ntext");
        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[0]), "plain");
        assert_eq!(line_text(&lines[1]), "text");
    }

    #[test]
    fn running_unknown_kind_falls_back_to_generic_one_liner() {
        let row = tool_row("fetch", "https://example.com", None);
        let out = joined(&render_tool_lines(&row, &Theme::default(), None));
        // Generic body shows the raw args prefixed with `$ ` while running.
        assert!(out.contains("$ https://example.com"), "{out:?}");
    }

    #[test]
    fn edit_with_unparsable_args_falls_back_to_generic() {
        // Truncated JSON (16KB ingest cap can clip mid-object) must not
        // panic or vanish; it falls through to the generic renderer.
        let row = tool_row("edit", r#"{"file_path":"a.rs","old_str"#, None);
        let out = joined(&render_tool_lines(&row, &Theme::default(), None));
        assert!(out.contains("$ {\"file_path\""), "{out:?}");
    }

    fn mention_state(query: &str, files: &[&str]) -> StructuredViewState {
        use super::super::state::MentionSession;
        let mut state = test_state();
        state.focus = Focus::Composer;
        state.composer.insert_str(format!("@{query}"));
        state.file_index = FileIndex::Loaded {
            files: files.iter().map(|f| f.to_string()).collect(),
            truncated: false,
        };
        state.mention = Some(MentionSession { selected: 0 });
        state
    }

    fn render_rows(state: &StructuredViewState, w: u16, h: u16, active: bool) -> Vec<String> {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                render(f, f.area(), &theme, state, active);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        buf.content()
            .chunks(w as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    fn render_dump(state: &StructuredViewState, w: u16, h: u16) -> String {
        render_rows(state, w, h, true).concat()
    }

    #[test]
    fn composer_renders_as_prompt_rail_without_bottom_box() {
        let state = test_state();
        let rows = render_rows(&state, 60, 12, true);
        let prompt = rows
            .iter()
            .find(|row| row.contains("Message the agent"))
            .expect("prompt row");
        assert!(
            prompt.trim_start().starts_with('›') && prompt.contains("Message the agent"),
            "prompt rail missing: {prompt:?}"
        );
        assert!(
            !prompt.contains('╰'),
            "composer still has a box: {prompt:?}"
        );
        assert!(
            !prompt.contains('╯'),
            "composer still has a box: {prompt:?}"
        );
    }

    #[test]
    fn metadata_card_is_compact_and_transcript_is_unframed() {
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
        state
            .transcript
            .rows
            .push(ActivityRow::UserPrompt("Hello.".into()));
        state
            .transcript
            .rows
            .push(ActivityRow::AgentMessage("What should we build?".into()));

        let rows = render_rows(&state, 80, 20, true);
        let card_right = rows[0]
            .chars()
            .position(|ch| ch == '╮')
            .expect("metadata card right edge");
        assert!(card_right < 79, "card still spans the viewport: {rows:?}");
        assert!(rows
            .iter()
            .any(|row| row.contains("Agent of Empires · codex")));
        assert!(rows.iter().any(|row| row.contains("virtual-wardrobe")));
        assert!(rows
            .iter()
            .any(|row| row.contains("/workspace/virtual-wardrobe")));
        assert!(rows.iter().any(|row| row.contains("permissions: yolo")));

        let prompt = rows
            .iter()
            .find(|row| row.contains("› Hello."))
            .expect("user turn");
        assert!(
            !prompt.starts_with('│'),
            "transcript kept a left frame: {prompt:?}"
        );
        assert!(
            !prompt.ends_with('│'),
            "transcript kept a right frame: {prompt:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("• What should we build?")),
            "agent gutter missing: {rows:?}"
        );
    }

    #[test]
    fn inactive_header_and_prompt_keep_calm_affordances() {
        let state = test_state();
        let rows = render_rows(&state, 60, 12, false);
        assert!(
            rows[0].contains("○ s-1"),
            "preview marker missing: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Press Enter to reply")),
            "entry hint missing: {rows:?}"
        );
    }

    #[test]
    fn render_shows_mention_picker_lists_daemon_files() {
        // Story 1: the open picker lists files from the (seeded) daemon
        // index. Empty query lists everything.
        let state = mention_state("", &["src/main.rs", "docs/readme.md"]);
        let dump = render_dump(&state, 80, 24);
        assert!(dump.contains("Files"), "picker title missing: {dump:?}");
        assert!(dump.contains("src/main.rs"), "file missing: {dump:?}");
        assert!(dump.contains("docs/readme.md"), "file missing: {dump:?}");
    }

    #[test]
    fn render_mention_picker_narrows_to_query() {
        // Story 1: as the query grows, the list narrows to matches only.
        let state = mention_state("src", &["src/main.rs", "zzz/other.md"]);
        let dump = render_dump(&state, 80, 24);
        assert!(dump.contains("src/main.rs"), "match missing: {dump:?}");
        assert!(!dump.contains("zzz/other.md"), "non-match leaked: {dump:?}");
    }
}
