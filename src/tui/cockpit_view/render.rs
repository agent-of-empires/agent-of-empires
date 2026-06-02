//! Render of a cockpit session, stacked top to bottom: transcript /
//! status banner / queued-prompts strip (zero height when empty) /
//! composer. The slash and `@` mention pickers float above the composer
//! when open rather than taking a pane. Tool calls render through a
//! per-kind dispatcher (`render_tool_lines`): edit/write show a compact
//! line diff, execute shows the command and an output preview, read
//! shows the path and a content preview, delete shows the path, and any
//! other kind falls back to the generic one-liner. Image previews and
//! syntax highlighting stay deferred to the web cockpit; press `o` from
//! the transcript pane to open it for full-fidelity inspection.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui::Frame;
use similar::{ChangeTag, TextDiff};

use super::input::Focus;
use super::reducer::{ActivityRow, CockpitTranscript, NoteKind, ToolCallRow};
use super::state::{CockpitViewState, FileIndex};
use crate::cockpit::approvals::ApprovalDecision;
use crate::tui::styles::Theme;

pub fn render(frame: &mut Frame, area: Rect, theme: &Theme, state: &CockpitViewState) {
    let queue_height = queued_strip_height(state);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),               // transcript
            Constraint::Length(1),            // status line
            Constraint::Length(queue_height), // queued prompts strip (0 when empty)
            Constraint::Length(composer_height(state)),
        ])
        .split(area);

    render_transcript(frame, chunks[0], theme, state);
    render_status(frame, chunks[1], theme, state);
    if queue_height > 0 {
        render_queue(frame, chunks[2], theme, state);
    }
    render_composer(frame, chunks[3], theme, state);
    // Pickers float above the composer (the composer sits at the screen
    // bottom, so a dropdown below it would render off-screen). Drawn
    // last so they overlay the transcript's lower rows. Slash and `@`
    // pickers are mutually exclusive; slash wins the tie defensively.
    if state.slash_picker_open() {
        render_slash_picker(frame, chunks[3], theme, state);
    } else if state.mention.is_some() {
        render_mention_picker(frame, chunks[3], theme, state);
    }
}

/// Up to this many queued prompts are previewed in the strip; the rest
/// collapse into a "(+N more)" line so a large backlog can't squeeze the
/// transcript off-screen.
const QUEUE_PREVIEW_ROWS: usize = 3;

/// Height of the queued-prompts strip: zero when the queue is empty,
/// otherwise the previewed rows plus the block's top and bottom borders.
fn queued_strip_height(state: &CockpitViewState) -> u16 {
    if state.queue.is_empty() {
        return 0;
    }
    let shown = state.queue.len().min(QUEUE_PREVIEW_ROWS);
    let overflow = usize::from(state.queue.len() > QUEUE_PREVIEW_ROWS);
    (shown + overflow) as u16 + 2
}

fn render_queue(frame: &mut Frame, area: Rect, theme: &Theme, state: &CockpitViewState) {
    let title = format!(
        " Queued ({}) · drains on idle · Ctrl-x clears ",
        state.queue.len()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(title)
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, prompt) in state.queue.iter().take(QUEUE_PREVIEW_ROWS).enumerate() {
        // Queued prompts can hold newlines (Shift+Enter in the composer);
        // ratatui's Line strips them, so collapse whitespace first to keep
        // the preview on one tidy line and truncate predictably.
        let one_line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        let preview = match truncate_chars(&one_line, 80) {
            Some(head) => format!("{}. {head}…", i + 1),
            None => format!("{}. {one_line}", i + 1),
        };
        lines.push(Line::from(Span::styled(
            preview,
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    if state.queue.len() > QUEUE_PREVIEW_ROWS {
        let extra = state.queue.len() - QUEUE_PREVIEW_ROWS;
        lines.push(Line::from(Span::styled(
            format!("(+{extra} more)"),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Most picker rows visible at once before the list windows around the
/// selection. Keeps the popup from eating the whole transcript when the
/// daemon advertises a long command list.
const SLASH_PICKER_MAX_ROWS: usize = 8;

fn render_slash_picker(
    frame: &mut Frame,
    composer_area: Rect,
    theme: &Theme,
    state: &CockpitViewState,
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
    matches: &[&'a crate::cockpit::state::AvailableCommand],
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
    state: &CockpitViewState,
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

/// Top + bottom border rows wrapping the composer textarea.
const COMPOSER_BORDER_ROWS: u16 = 2;
/// Maximum content rows the composer is allowed to take before the
/// transcript starts losing space. Multi-line prompts beyond this
/// scroll inside the textarea instead of growing the pane.
const COMPOSER_MAX_CONTENT_ROWS: u16 = 6;

fn composer_height(state: &CockpitViewState) -> u16 {
    // Composer is `1 + COMPOSER_BORDER_ROWS = 3` rows tall by default,
    // growing one row per typed newline up to
    // `COMPOSER_MAX_CONTENT_ROWS + COMPOSER_BORDER_ROWS = 8` rows so
    // multi-line prompts don't squash the transcript.
    let lines = state.composer.lines().len().max(1) as u16;
    lines.clamp(1, COMPOSER_MAX_CONTENT_ROWS) + COMPOSER_BORDER_ROWS
}

fn render_transcript(frame: &mut Frame, area: Rect, theme: &Theme, state: &CockpitViewState) {
    let title = format!(
        " Cockpit · {}{} ",
        state.session_id,
        match state.transcript.current_mode.as_deref() {
            Some(m) => format!(" · mode: {m}"),
            None => String::new(),
        }
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(theme, state, Focus::Transcript));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = transcript_lines(
        &state.transcript,
        state.selected_approval,
        state.focus,
        theme,
    );
    // Clamp scroll against the *wrapped* visual row count, not
    // `lines.len()`. Streaming `AgentMessage` rows grew text inside
    // a single logical line: Paragraph's wrap inflated the
    // rendered row count while `lines.len()` stayed constant, so
    // `state.scroll_offset = u16::MAX` (stick to bottom) clipped
    // short of the newest chunk. Tool calls didn't show the bug
    // because each call adds whole new Line entries.
    let total = visual_line_count(&lines, inner.width);
    let max = total.saturating_sub(inner.height);
    let scroll = (state.scroll_offset.min(max), 0);
    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll(scroll);
    frame.render_widget(para, inner);
}

/// Estimate the number of terminal rows `lines` will occupy when
/// rendered into a paragraph of width `width`. Each `Line`'s display
/// width divided by the available columns, rounded up, summed. Used
/// to keep `scroll_offset = u16::MAX` pinned to the bottom as
/// streaming chunks grow inside a single logical line.
fn visual_line_count(lines: &[Line], width: u16) -> u16 {
    if width == 0 {
        return lines.len() as u16;
    }
    let w = width as usize;
    let mut total: usize = 0;
    for line in lines {
        let lw = line.width().max(1);
        total = total.saturating_add(lw.div_ceil(w));
    }
    total.min(u16::MAX as usize) as u16
}

fn render_status(frame: &mut Frame, area: Rect, theme: &Theme, state: &CockpitViewState) {
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
    if let Some(banner) = &state.transcript.status_text {
        spans.push(Span::styled(
            format!(" {banner} "),
            Style::default().fg(theme.title),
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
    if !state.transcript.pending_approvals.is_empty() {
        let n = state.transcript.pending_approvals.len();
        spans.push(Span::styled(
            format!(
                " {n} pending approval{}; Tab to focus ",
                if n == 1 { "" } else { "s" }
            ),
            Style::default().fg(theme.error),
        ));
    }
    if spans.is_empty() {
        // Footer help when nothing else is going on.
        spans.push(Span::styled(
            help_hint(state.focus),
            Style::default().fg(theme.hint),
        ));
    }
    let para = Paragraph::new(Line::from(spans));
    frame.render_widget(para, area);
}

fn render_composer(frame: &mut Frame, area: Rect, theme: &Theme, state: &CockpitViewState) {
    let title = match state.focus {
        Focus::Composer => " Composer (Enter=send, Shift+Enter=newline, Esc=back) ",
        _ => " Composer (Tab/i to focus) ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(theme, state, Focus::Composer));
    // ratatui-textarea borrows the Frame's buffer indirectly via
    // widget impl; render the block first, then the textarea inside.
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&state.composer, inner);
    if matches!(state.focus, Focus::Composer) && inner.width > 0 && inner.height > 0 {
        let cursor = state.composer.screen_cursor();
        let max_x = inner.x.saturating_add(inner.width.saturating_sub(1));
        let max_y = inner.y.saturating_add(inner.height.saturating_sub(1));
        let cursor_x = inner.x.saturating_add(cursor.col as u16).min(max_x);
        let cursor_y = inner.y.saturating_add(cursor.row as u16).min(max_y);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Gutter marking the first line of an agent message. Continuation
/// lines align under the text with `AGENT_GUTTER_CONT`.
const AGENT_GUTTER: &str = "aoe  ";
const AGENT_GUTTER_CONT: &str = "     ";

/// Render an agent message as markdown-styled transcript lines.
///
/// We parse the message with `pulldown-cmark` and map its events to
/// ratatui `Line`s ourselves (see [`MarkdownBuilder`]). This strips the
/// raw `#`/`**`/backtick/fence markers and styles content with modifiers
/// only (BOLD/ITALIC/DIM), so the output tracks the app theme rather than
/// carrying hardcoded colors. Each line is prefixed with the `aoe` gutter
/// on the first row and an aligned indent on continuation rows. Empty or
/// marker-only input falls back to the bare `aoe  …` placeholder the
/// streaming UI showed before.
fn render_agent_message_lines(text: &str) -> Vec<Line<'static>> {
    if text.trim().is_empty() {
        return vec![Line::from(format!("{AGENT_GUTTER}…"))];
    }
    let body = MarkdownBuilder::render(text);
    if body.is_empty() {
        return vec![Line::from(format!("{AGENT_GUTTER}…"))];
    }
    body.into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            let prefix = if i == 0 {
                AGENT_GUTTER
            } else {
                AGENT_GUTTER_CONT
            };
            line.spans.insert(0, Span::raw(prefix));
            line
        })
        .collect()
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
}

impl MarkdownBuilder {
    fn render(text: &str) -> Vec<Line<'static>> {
        let mut builder = MarkdownBuilder::default();
        for event in
            pulldown_cmark::Parser::new_ext(text, pulldown_cmark::Options::ENABLE_STRIKETHROUGH)
        {
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
                if self.in_code_block {
                    self.push_code_text(&text);
                } else {
                    self.push_span(&text, Modifier::empty());
                }
            }
            Event::Code(text) => self.push_span(&text, Modifier::DIM),
            Event::SoftBreak if !self.in_code_block => self.current.push(Span::raw(" ")),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.block_break();
                self.lines.push(Line::from("───"));
            }
            _ => {}
        }
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
    transcript: &'a CockpitTranscript,
    selected_approval: Option<usize>,
    focus: Focus,
    theme: &Theme,
) -> Vec<Line<'a>> {
    let mut out: Vec<Line<'a>> = Vec::new();
    let mut approval_render_idx: usize = 0;
    for row in &transcript.rows {
        match row {
            ActivityRow::UserPrompt(text) => {
                out.push(Line::from(Span::styled(
                    format!("you  ▸ {text}"),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                out.push(Line::default());
            }
            ActivityRow::AgentMessage(text) => {
                out.extend(render_agent_message_lines(text));
                out.push(Line::default());
            }
            ActivityRow::ToolCall(tool) => {
                out.extend(render_tool_lines(tool, theme));
                out.push(Line::default());
            }
            ActivityRow::Approval(row) => {
                let highlighted = focus == Focus::Approval
                    && selected_approval
                        .map(|i| i == approval_render_idx)
                        .unwrap_or(false);
                approval_render_idx += 1;
                let mut header = Vec::new();
                header.push(Span::raw(if highlighted { "▶ " } else { "  " }));
                header.push(Span::styled(
                    format!("approval · {} ", row.title),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                if row.destructive {
                    header.push(Span::styled(
                        "[destructive] ",
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                }
                header.push(Span::styled(
                    format!("nonce={}", row.nonce),
                    Style::default().add_modifier(Modifier::DIM),
                ));
                out.push(Line::from(header));
                let body = match row.decision {
                    Some(ApprovalDecision::Allow) => "  → allowed",
                    Some(ApprovalDecision::AllowAlways) => "  → allow-always",
                    Some(ApprovalDecision::Deny) => "  → denied",
                    Some(ApprovalDecision::Cancelled) => "  → cancelled",
                    None => "  press a / A / d to resolve, Esc to leave",
                };
                out.push(Line::from(body));
                out.push(Line::default());
            }
            ActivityRow::Plan(steps) => {
                out.push(Line::from(Span::styled(
                    "plan",
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                for step in steps {
                    let marker = match step.status {
                        crate::cockpit::state::PlanStepStatus::Pending => "[ ]",
                        crate::cockpit::state::PlanStepStatus::InProgress => "[~]",
                        crate::cockpit::state::PlanStepStatus::Done => "[x]",
                        crate::cockpit::state::PlanStepStatus::Cancelled => "[-]",
                    };
                    out.push(Line::from(format!("  {marker} {}", step.title)));
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
/// and edit before/after text. Mirrors the web cockpit's `pickStr` key
/// lists in `web/src/components/cockpit/ToolCards.tsx` so the TUI and the
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
fn render_tool_lines(tool: &ToolCallRow, theme: &Theme) -> Vec<Line<'static>> {
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

    let args = parse_args_object(&tool.args);
    let body = match tool.kind.as_str() {
        "edit" | "write" => render_edit_body(args.as_ref(), theme),
        "execute" => render_execute_body(args.as_ref(), tool),
        "read" => render_read_body(args.as_ref(), tool),
        "delete" => render_delete_body(args.as_ref()),
        _ => None,
    };
    lines.extend(body.unwrap_or_else(|| render_generic_body(tool)));
    lines
}

/// Parse `args_preview` as a JSON object. Mirrors the web cockpit's
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
) -> Option<Vec<Line<'static>>> {
    let new = pick_str(args, NEW_KEYS)?;
    let old = pick_str(args, OLD_KEYS).unwrap_or("");
    if old.is_empty() && new.is_empty() {
        return None;
    }
    let path = pick_str(args, PATH_KEYS).unwrap_or("(unknown file)");
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
) -> Option<Vec<Line<'static>>> {
    let path = pick_str(args, PATH_KEYS)?;
    let mut lines = vec![Line::from(format!("  {path}"))];
    lines.extend(output_preview_lines(tool));
    Some(lines)
}

/// Delete: just the target path.
fn render_delete_body(
    args: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<Vec<Line<'static>>> {
    let path = pick_str(args, PATH_KEYS)?;
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
    let total = completion.content.lines().count();
    for line in completion.content.lines().take(TOOL_PREVIEW_MAX_LINES) {
        out.push(Line::from(format!("  {line}")));
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
        let content = if completion.content.is_empty() {
            if completion.ok {
                "  (no output)".to_string()
            } else {
                "  (tool failed; press `o` for details)".to_string()
            }
        } else if let Some(head) = truncate_chars(&completion.content, 400) {
            format!("  {head}…\n  (output truncated; press `o` for full)")
        } else {
            completion
                .content
                .lines()
                .map(|l| format!("  {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        for line in content.lines() {
            lines.push(Line::from(line.to_string()));
        }
    }
    lines
}

fn border_style(theme: &Theme, state: &CockpitViewState, this_focus: Focus) -> Style {
    if state.focus == this_focus {
        Style::default().fg(theme.title)
    } else {
        Style::default().fg(theme.border)
    }
}

fn help_hint(focus: Focus) -> &'static str {
    match focus {
        Focus::Composer => {
            " Enter=send · Shift+Enter=newline · /=commands · Esc=back · Ctrl-C=cancel "
        }
        Focus::Transcript => " j/k=scroll · i=compose · Tab=approvals · o=browser · Esc=exit ",
        Focus::Approval => " a=allow · A=always · d=deny · Esc=back ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cockpit::client::discovery::Source;
    use crate::cockpit::client::{DaemonEndpoint, HttpClient};

    fn test_state() -> CockpitViewState {
        let endpoint = DaemonEndpoint {
            base_url: "http://127.0.0.1:8080".into(),
            token: None,
            source: Source::Env,
        };
        let http = HttpClient::new(endpoint.clone()).unwrap();
        CockpitViewState::new("s-1".into(), endpoint, http, None)
    }

    #[test]
    fn queued_strip_height_is_zero_when_empty() {
        let state = test_state();
        assert_eq!(queued_strip_height(&state), 0);
    }

    #[test]
    fn queued_strip_height_grows_with_entries_then_caps() {
        let mut state = test_state();
        state.queue.push("one".into());
        assert_eq!(queued_strip_height(&state), 1 + 2);
        state.queue.push("two".into());
        state.queue.push("three".into());
        assert_eq!(queued_strip_height(&state), 3 + 2);
        // Beyond the preview cap, an extra "+N more" row is added but the
        // height stays bounded.
        state.queue.push("four".into());
        state.queue.push("five".into());
        assert_eq!(
            queued_strip_height(&state),
            QUEUE_PREVIEW_ROWS as u16 + 1 + 2
        );
    }

    #[test]
    fn visual_line_count_counts_wrapped_rows() {
        // 40 chars at width 10 wraps to 4 visual rows.
        let lines = vec![Line::from("a".repeat(40))];
        assert_eq!(visual_line_count(&lines, 10), 4);
    }

    #[test]
    fn visual_line_count_floors_empty_line_to_one() {
        // A logical empty line still occupies one row.
        let lines = vec![Line::default()];
        assert_eq!(visual_line_count(&lines, 10), 1);
    }

    #[test]
    fn visual_line_count_handles_zero_width() {
        // Degenerate area (e.g. during teardown); fall back to logical
        // line count so we don't divide by zero.
        let lines = vec![Line::from("x"), Line::from("y")];
        assert_eq!(visual_line_count(&lines, 0), 2);
    }

    #[test]
    fn visual_line_count_streaming_growth_advances_max_scroll() {
        // Regression for the agent-message auto-scroll bug: as a
        // single logical line grows, the visual row count must
        // grow so `scroll_offset = u16::MAX` keeps tracking the
        // bottom.
        let short = vec![Line::from("a".repeat(20))];
        let long = vec![Line::from("a".repeat(200))];
        assert!(visual_line_count(&long, 40) > visual_line_count(&short, 40));
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
    fn agent_message_gutter_marks_first_line_then_indents() {
        let lines = render_agent_message_lines("line one\n\nline two");
        // First rendered line gets the `aoe  ` gutter.
        assert!(
            line_text(&lines[0]).starts_with(AGENT_GUTTER),
            "first line missing gutter: {:?}",
            line_text(&lines[0])
        );
        // Every continuation line aligns under the text with spaces, no
        // repeated `aoe` literal.
        for line in &lines[1..] {
            let text = line_text(line);
            assert!(
                text.is_empty() || text.starts_with(AGENT_GUTTER_CONT),
                "continuation line not indented: {text:?}"
            );
            assert!(
                !text.trim_start().starts_with("aoe"),
                "gutter literal repeated on continuation: {text:?}"
            );
        }
    }

    use crate::cockpit::state::AvailableCommand;
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
            assert_eq!(line_text(&lines[0]), format!("{AGENT_GUTTER}…"));
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
        let mut state = CockpitViewState::new("sess".to_string(), endpoint, http, None);
        state.focus = Focus::Composer;
        state.transcript.available_commands =
            vec![cmd("compact", "shrink context"), cmd("clear", "wipe")];
        state.composer.insert_str("/comp");
        assert!(state.slash_picker_open());

        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render(f, f.area(), &theme, &state))
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
        let mut state = CockpitViewState::new("sess".to_string(), endpoint, http, None);
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
            .draw(|f| render(f, f.area(), &theme, &state))
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
            completed: completion.map(|(ok, content)| ToolCompletion {
                ok,
                content: content.into(),
            }),
        }
    }

    fn joined(lines: &[Line]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn edit_kind_renders_added_and_removed_diff_lines() {
        let row = tool_row(
            "edit",
            r#"{"file_path":"src/a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#,
            None,
        );
        let out = joined(&render_tool_lines(&row, &Theme::default()));
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
        let out = joined(&render_tool_lines(&row, &Theme::default()));
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
        let lines = render_tool_lines(&row, &Theme::default());
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
    fn execute_kind_renders_command_and_output_preview() {
        let row = tool_row(
            "execute",
            r#"{"command":"ls -la"}"#,
            Some((true, "file_a\nfile_b")),
        );
        let out = joined(&render_tool_lines(&row, &Theme::default()));
        assert!(out.contains("$ ls -la"), "command missing: {out:?}");
        assert!(out.contains("file_a"), "output preview missing: {out:?}");
        assert!(out.contains("file_b"), "{out:?}");
    }

    #[test]
    fn read_kind_renders_path_and_content_preview() {
        let row = tool_row(
            "read",
            r#"{"path":"src/lib.rs"}"#,
            Some((true, "pub fn main() {}")),
        );
        let out = joined(&render_tool_lines(&row, &Theme::default()));
        assert!(out.contains("src/lib.rs"), "path missing: {out:?}");
        assert!(
            out.contains("pub fn main()"),
            "content preview missing: {out:?}"
        );
    }

    #[test]
    fn delete_kind_renders_only_path() {
        let row = tool_row("delete", r#"{"path":"old.txt"}"#, Some((true, "")));
        let out = joined(&render_tool_lines(&row, &Theme::default()));
        assert!(out.contains("old.txt"), "path missing: {out:?}");
        // No diff gutters for a delete.
        assert!(!out.contains("+ "), "{out:?}");
        assert!(!out.contains("- "), "{out:?}");
    }

    #[test]
    fn unknown_kind_falls_back_to_generic_one_liner() {
        let row = tool_row("fetch", "https://example.com", Some((true, "200 OK")));
        let out = joined(&render_tool_lines(&row, &Theme::default()));
        // Generic body shows the raw args prefixed with `$ ` and the output.
        assert!(out.contains("$ https://example.com"), "{out:?}");
        assert!(out.contains("200 OK"), "{out:?}");
    }

    #[test]
    fn edit_with_unparsable_args_falls_back_to_generic() {
        // Truncated JSON (16KB ingest cap can clip mid-object) must not
        // panic or vanish; it falls through to the generic renderer.
        let row = tool_row("edit", r#"{"file_path":"a.rs","old_str"#, None);
        let out = joined(&render_tool_lines(&row, &Theme::default()));
        assert!(out.contains("$ {\"file_path\""), "{out:?}");
    }

    fn mention_state(query: &str, files: &[&str]) -> CockpitViewState {
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

    fn render_dump(state: &CockpitViewState, w: u16, h: u16) -> String {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render(f, f.area(), &theme, state))
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        buf.content().iter().map(|c| c.symbol()).collect()
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
