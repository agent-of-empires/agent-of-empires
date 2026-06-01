//! Four-pane render of a cockpit session: transcript / status banner /
//! queued-prompts strip / composer. Tool-card breakdowns are intentionally minimal in the MVP
//! (one-liner per tool call); rich diff / image / file previews are
//! deferred to the followup issues called out in the implementation
//! plan. Press `o` from the transcript pane to open the web cockpit
//! for full-fidelity inspection.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

use super::input::Focus;
use super::reducer::{ActivityRow, CockpitTranscript, NoteKind, ToolCallRow};
use super::state::CockpitViewState;
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

    let lines = transcript_lines(&state.transcript, state.selected_approval, state.focus);
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
                out.extend(render_tool_lines(tool));
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

fn render_tool_lines(tool: &ToolCallRow) -> Vec<Line<'static>> {
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
        Focus::Composer => " Enter=send · Shift+Enter=newline · Esc=back · Ctrl-C=cancel ",
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
}
