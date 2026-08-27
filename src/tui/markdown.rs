//! Markdown rendering shared by native TUI surfaces.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

const TAB_STOP_WIDTH: usize = 4;

/// Parse Markdown into styled ratatui lines.
pub(crate) fn render(text: &str) -> Vec<Line<'static>> {
    MarkdownBuilder::render(text)
}

/// Parse Markdown and wrap it to the available terminal width.
pub(crate) fn render_wrapped(text: &str, width: u16) -> Vec<Line<'static>> {
    let mut wrapped = Vec::new();
    for line in render(text) {
        wrap_line_into(line, width, &mut wrapped);
    }
    wrapped
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
                self.table_row = Some(Vec::new());
            }
            Event::End(TagEnd::TableHead) => {
                self.flush_table_row(Modifier::BOLD);
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
            // output. A reply formatted one item per line must not collapse
            // into one wrapped paragraph.
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
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

/// Word-wrap one styled line while preserving span styles.
fn wrap_line_into(line: Line<'static>, width: u16, out: &mut Vec<Line<'static>>) {
    let width = width.max(1) as usize;
    let line_style = line.style;
    let mut chars: Vec<(char, Style)> = Vec::new();
    let mut expanded_width = 0usize;
    let mut expanded_tabs = false;
    for span in &line.spans {
        for ch in span.content.chars() {
            if ch == '\t' {
                expanded_tabs = true;
                let spaces = TAB_STOP_WIDTH - (expanded_width % TAB_STOP_WIDTH);
                chars.extend(std::iter::repeat_n((' ', span.style), spaces));
                expanded_width += spaces;
            } else {
                expanded_width += ch.width().unwrap_or(0);
                chars.push((ch, span.style));
            }
        }
    }
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut row_width = 0usize;
    let mut last_space: Option<usize> = None;
    let flush = |row: &mut Vec<(char, Style)>, out: &mut Vec<Line<'static>>| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (ch, style) in row.drain(..) {
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(ch),
                _ => spans.push(Span::styled(ch.to_string(), style)),
            }
        }
        out.push(Line::from(spans).style(line_style));
    };
    if expanded_width <= width {
        if expanded_tabs {
            flush(&mut chars, out);
        } else {
            out.push(line);
        }
        return;
    }
    for (ch, style) in chars {
        let char_width = ch.width().unwrap_or(0);
        if row_width + char_width > width && !row.is_empty() {
            if let Some(cut) = last_space {
                let tail: Vec<(char, Style)> = row.split_off(cut + 1);
                row.truncate(cut);
                if !row.is_empty() {
                    flush(&mut row, out);
                }
                row = tail;
                row_width = row.iter().map(|(ch, _)| ch.width().unwrap_or(0)).sum();
            } else {
                flush(&mut row, out);
                row_width = 0;
            }
            last_space = None;
        }
        if ch == ' ' {
            last_space = Some(row.len());
        }
        row.push((ch, style));
        row_width += char_width;
    }
    flush(&mut row, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_line_handles_width_whitespace_and_styles() {
        struct Case {
            name: &'static str,
            input: Line<'static>,
            width: u16,
            expected: Vec<Line<'static>>,
        }

        let bold = Style::default().add_modifier(Modifier::BOLD);
        let italic = Style::default().add_modifier(Modifier::ITALIC);
        let styled = |text: &'static str, style| Line::from(Span::styled(text, style));
        let cases = vec![
            Case {
                name: "short line",
                input: styled("short", bold),
                width: 8,
                expected: vec![styled("short", bold)],
            },
            Case {
                name: "exact width",
                input: styled("exact", italic),
                width: 5,
                expected: vec![styled("exact", italic)],
            },
            Case {
                name: "word longer than width",
                input: styled("abcdefgh", bold),
                width: 3,
                expected: vec![styled("abc", bold), styled("def", bold), styled("gh", bold)],
            },
            Case {
                name: "consecutive spaces",
                input: styled("ab  cd", italic),
                width: 4,
                expected: vec![styled("ab ", italic), styled("cd", italic)],
            },
            Case {
                name: "wide characters",
                input: styled("你好世界", bold),
                width: 4,
                expected: vec![styled("你好", bold), styled("世界", bold)],
            },
            Case {
                name: "tab expansion",
                input: styled("foo\tbar", italic),
                width: 5,
                expected: vec![styled("foo", italic), styled("bar", italic)],
            },
            Case {
                name: "leading space does not emit an empty line",
                input: styled(" abcdef", bold),
                width: 3,
                expected: vec![styled("abc", bold), styled("def", bold)],
            },
            Case {
                name: "styles survive a whitespace break",
                input: Line::from(vec![
                    Span::styled("hello ", bold),
                    Span::styled("world", italic),
                ]),
                width: 5,
                expected: vec![styled("hello", bold), styled("world", italic)],
            },
        ];

        for case in cases {
            let mut actual = Vec::new();
            wrap_line_into(case.input, case.width, &mut actual);
            assert_eq!(actual, case.expected, "{}", case.name);
        }
    }
}
