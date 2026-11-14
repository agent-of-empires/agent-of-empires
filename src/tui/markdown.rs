//! Markdown rendering shared by native TUI surfaces.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

const TAB_STOP_WIDTH: usize = 4;

/// Parse Markdown into styled ratatui lines.
pub(crate) fn render(text: &str) -> Vec<Line<'static>> {
    use super::rich_text::{infallible, Memory, MemoryLines};
    let source = Memory;
    let mut output = MemoryLines::default();
    let mut builder = infallible(MarkdownBuilder::new(&source, &mut output));
    let options =
        pulldown_cmark::Options::ENABLE_STRIKETHROUGH | pulldown_cmark::Options::ENABLE_TABLES;
    for event in pulldown_cmark::Parser::new_ext(text, options) {
        infallible(builder.handle(event, String::from));
    }
    infallible(builder.finish());
    output.lines
}

/// Parse Markdown and wrap it to the available terminal width.
pub(crate) fn render_wrapped(text: &str, width: u16) -> Vec<Line<'static>> {
    let mut wrapped = Vec::new();
    for line in render(text) {
        wrap_line_into(line, width, &mut wrapped);
    }
    wrapped
}

use super::rich_text::{ListStack, Sink, Source, Table, TablePart};

/// Markdown grammar and styling shared by in-memory and retained projections.
pub(crate) struct MarkdownBuilder<'a, S: Source, K: Sink<S::Text, Error = S::Error>> {
    source: &'a S,
    sink: &'a mut K,
    lists: S::Lists,
    table: S::Table,
    table_open: bool,
    table_cells: u64,
    modifiers: [u64; 3],
    in_code_block: bool,
    link_dest: Option<S::Text>,
    link_matches: bool,
    link_bytes: u64,
    lines: u64,
    pending_blanks: u64,
    current: bool,
}

impl<'a, S: Source, K: Sink<S::Text, Error = S::Error>> MarkdownBuilder<'a, S, K> {
    pub fn new(source: &'a S, sink: &'a mut K) -> Result<Self, S::Error> {
        Ok(Self {
            source,
            sink,
            lists: source.lists()?,
            table: source.table()?,
            table_open: false,
            table_cells: 0,
            modifiers: [0; 3],
            in_code_block: false,
            link_dest: None,
            link_matches: true,
            link_bytes: 0,
            lines: 0,
            pending_blanks: 0,
            current: false,
        })
    }
    fn active_modifier(&self) -> Modifier {
        let mut active = Modifier::empty();
        for (count, modifier) in
            self.modifiers
                .iter()
                .zip([Modifier::BOLD, Modifier::ITALIC, Modifier::CROSSED_OUT])
        {
            if *count != 0 {
                active |= modifier;
            }
        }
        active
    }
    fn span(&mut self, text: S::Text, extra: Modifier) -> Result<(), S::Error> {
        self.sink.text(
            text,
            Style::default().add_modifier(self.active_modifier() | extra),
        )?;
        self.current = true;
        Ok(())
    }
    fn flush(&mut self) -> Result<(), S::Error> {
        if self.current {
            self.sink
                .blank_lines(std::mem::take(&mut self.pending_blanks))?;
            self.sink.end_line()?;
            self.lines += 1;
            self.current = false;
        }
        Ok(())
    }
    fn flush_code_line(&mut self) -> Result<(), S::Error> {
        if self.current {
            self.flush()?;
        } else {
            self.lines += 1;
            self.pending_blanks += 1;
        }
        Ok(())
    }
    fn block_break(&mut self) {
        if self.lists.len() == 0 && self.lines != 0 {
            self.lines += 1;
            self.pending_blanks += 1;
        }
    }
    fn track_link(&mut self, text: &S::Text) -> Result<(), S::Error> {
        if let Some(destination) = &self.link_dest {
            if self.link_matches {
                self.link_matches =
                    self.source
                        .prefix_matches(destination, self.link_bytes, text)?;
            }
            self.link_bytes += self.source.len(text);
        }
        Ok(())
    }
    pub fn handle<T, C, A, L>(
        &mut self,
        event: pulldown_cmark::Event<T, C, A, L>,
        convert: impl FnOnce(T) -> S::Text,
    ) -> Result<(), S::Error> {
        use pulldown_cmark::{Event, Tag, TagEnd};
        match event {
            Event::Start(Tag::Heading { .. }) => {
                self.block_break();
                self.modifiers[0] += 1;
            }
            Event::End(TagEnd::Heading(_)) => {
                self.flush()?;
                self.modifiers[0] -= 1;
            }
            Event::Start(Tag::Paragraph) => self.block_break(),
            Event::End(TagEnd::Paragraph) => self.flush()?,
            Event::Start(Tag::Strong) => self.modifiers[0] += 1,
            Event::Start(Tag::Emphasis) => self.modifiers[1] += 1,
            Event::Start(Tag::Strikethrough) => self.modifiers[2] += 1,
            Event::End(TagEnd::Strong) => self.modifiers[0] -= 1,
            Event::End(TagEnd::Emphasis) => self.modifiers[1] -= 1,
            Event::End(TagEnd::Strikethrough) => self.modifiers[2] -= 1,
            Event::Start(Tag::CodeBlock(_)) => {
                self.block_break();
                self.in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.flush()?;
                self.in_code_block = false;
            }
            Event::Start(Tag::List(first)) => self.lists.push(first)?,
            Event::End(TagEnd::List(_)) => self.lists.pop()?,
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.link_dest = Some(convert(dest_url));
                self.link_matches = true;
                self.link_bytes = 0;
            }
            Event::End(TagEnd::Link) => {
                if let Some(destination) = self.link_dest.take() {
                    if self.source.len(&destination) != 0
                        && (!self.link_matches || self.link_bytes != self.source.len(&destination))
                    {
                        let style =
                            Style::default().add_modifier(self.active_modifier() | Modifier::DIM);
                        self.sink.literal(" (", style)?;
                        self.sink.text(destination, style)?;
                        self.sink.literal(")", style)?;
                        self.current = true;
                    }
                }
            }
            Event::Start(Tag::Table(_)) => self.block_break(),
            Event::End(TagEnd::Table) => {
                self.table_open = false;
                self.table.clear()?;
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                self.table.clear()?;
                self.table_open = true;
                self.table_cells = 0;
            }
            Event::End(TagEnd::TableHead) => self.flush_table_row(Modifier::BOLD)?,
            Event::End(TagEnd::TableRow) => self.flush_table_row(Modifier::empty())?,
            Event::Start(Tag::TableCell) => {
                if self.table_open {
                    if self.table_cells != 0 {
                        self.table.push(TablePart::Separator)?;
                    }
                    self.table_cells += 1;
                }
            }
            Event::Start(Tag::Item) => {
                self.flush()?;
                let depth = self.lists.len().saturating_sub(1);
                self.sink.spaces(depth * 2, Style::default())?;
                match self.lists.last()? {
                    Some(Some(number)) => {
                        self.sink.number(number, Style::default())?;
                        self.sink.literal(". ", Style::default())?;
                        self.lists.set_last(Some(number + 1))?;
                    }
                    _ => self.sink.literal("• ", Style::default())?,
                }
                self.current = true;
            }
            Event::End(TagEnd::Item) => self.flush()?,
            Event::Text(text) => {
                let text = convert(text);
                if self.table_open {
                    if self.table_cells != 0 {
                        self.table.push(TablePart::Text(text))?;
                    }
                } else if self.in_code_block {
                    self.push_code_text(text)?;
                } else {
                    self.track_link(&text)?;
                    self.span(text, Modifier::empty())?;
                }
            }
            Event::Code(text) => {
                let text = convert(text);
                if self.table_open {
                    if self.table_cells != 0 {
                        self.table.push(TablePart::Text(text))?;
                    }
                } else {
                    self.track_link(&text)?;
                    self.span(text, Modifier::DIM)?;
                }
            }
            Event::SoftBreak if !self.in_code_block => self.flush()?,
            Event::HardBreak => self.flush()?,
            Event::Rule => {
                self.block_break();
                self.sink.standalone_literal(
                    "───",
                    Style::default(),
                    std::mem::take(&mut self.pending_blanks),
                )?;
                self.lines += 1;
            }
            _ => {}
        }
        Ok(())
    }
    fn flush_table_row(&mut self, extra: Modifier) -> Result<(), S::Error> {
        if !self.table_open {
            return Ok(());
        }
        self.table_open = false;
        if self.table_cells == 0 {
            self.table.clear()?;
            return Ok(());
        }
        let style = Style::default().add_modifier(self.active_modifier() | extra);
        let bytes = self.table.byte_len();
        self.sink.standalone_line(
            self.table.drain(),
            bytes,
            style,
            std::mem::take(&mut self.pending_blanks),
        )?;
        self.lines += 1;
        Ok(())
    }
    fn push_code_text(&mut self, text: S::Text) -> Result<(), S::Error> {
        let style = Style::default().add_modifier(Modifier::DIM);
        let length = self.source.len(&text);
        let mut start = 0;
        while let Some(end) = self.source.find(&text, b'\n', start)? {
            if start != end {
                self.sink
                    .text(self.source.slice(&text, start..end)?, style)?;
                self.current = true;
            }
            self.flush_code_line()?;
            start = end + 1;
        }
        if start < length {
            let text = if start == 0 {
                text
            } else {
                self.source.slice(&text, start..length)?
            };
            self.sink.text(text, style)?;
            self.current = true;
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<u64, S::Error> {
        self.flush()?;
        Ok(self.lines - self.pending_blanks)
    }
}

/// Word-wrap one styled line while preserving span styles.
pub(crate) fn wrap_line_into(line: Line<'static>, width: u16, out: &mut Vec<Line<'static>>) {
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
