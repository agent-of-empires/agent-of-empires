use std::{
    io::{self, Write},
    rc::Rc,
};

use pulldown_cmark::{
    disk::{ByteSource, TextRef},
    Options,
};
use ratatui::style::{Modifier, Style};

use crate::acp::{
    retained::{Context, Disk, List, Positions},
    session_paths::SessionPathRoots,
    transcript::{
        reducer::Projection,
        storage::{Backend, RowOf, Rows},
        TranscriptRowKind as Kind,
    },
};
use crate::tui::{
    markdown::MarkdownBuilder,
    rich_text::{
        ansi,
        disk::{DiskSource, Document},
        glyph, Sink, Source,
    },
    styles::Theme,
};

type Row = RowOf<Disk>;
pub(super) mod worker;

pub(super) fn document<'a>(
    context: &'a Rc<Context>,
    projection: &Projection<Disk>,
    theme: &Theme,
    roots: Option<&SessionPathRoots>,
    current: impl Fn() -> bool,
) -> io::Result<Document<'a>> {
    let source = DiskSource {
        store: &context.store,
        spool: &context.spool,
    };
    let mut document = Document::new(source)?;
    let mut completions = Positions::new(Rc::clone(context))?;
    for index in 0..projection.rows.len() {
        check(&current)?;
        let row = projection.rows.read(index)?;
        if matches!(
            row.kind,
            Kind::ToolComplete | Kind::ToolError | Kind::ToolStopped
        ) {
            if let Some(id) = row.tool_call_id {
                completions.insert(id, index)?;
            }
        }
    }
    let backend = Disk(Rc::clone(context));
    let mut index = 0;
    while index < projection.rows.len() {
        check(&current)?;
        let row = projection.rows.read(index)?;
        match row.kind {
            Kind::Message | Kind::Thinking => {
                let mut end = index + 1;
                while end < projection.rows.len() {
                    let next = projection.rows.read(end)?;
                    if next.kind != row.kind || !backend.equal(&row.group_id, next.group_id)? {
                        break;
                    }
                    end += 1;
                }
                let text = if end == index + 1 {
                    row.text
                } else {
                    let mut writer = context.spool.writer();
                    for position in index..end {
                        context
                            .spool
                            .copy_to(projection.rows.read(position)?.text, &mut writer)?;
                    }
                    writer.finish()?
                };
                if row.kind == Kind::Message {
                    markdown(context, &mut document, text, theme)?;
                } else {
                    plain(
                        &source,
                        &mut document,
                        text,
                        "· Thinking: ",
                        "  ",
                        Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
                        Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
                    )?;
                }
                document.blank_lines(1)?;
                index = end;
                continue;
            }
            Kind::UserPrompt => {
                let text =
                    if row.origin_plugin.is_none() && context.list_length(row.attachments)? == 0 {
                        row.text
                    } else {
                        let mut writer = context.spool.writer();
                        if let Some(plugin) = row.origin_plugin {
                            writer.write_all(b"[plugin ")?;
                            context.spool.copy_to(plugin, &mut writer)?;
                            writer.write_all(b"] ")?;
                        }
                        context.spool.copy_to(row.text, &mut writer)?;
                        let attachments = context.list_length(row.attachments)?;
                        if attachments != 0 {
                            write!(&mut writer, " [{attachments} attachment(s)]")?;
                        }
                        writer.finish()?
                    };
                user(&source, &mut document, text, theme)?;
                document.blank_lines(1)?;
            }
            Kind::UserDiffComments => {
                user(&source, &mut document, row.text, theme)?;
                document.blank_lines(1)?;
            }
            Kind::ToolStart => {
                let completion = row
                    .tool_call_id
                    .map(|id| completions.get(id))
                    .transpose()?
                    .flatten()
                    .map(|position| projection.rows.read(position))
                    .transpose()?;
                tool(
                    context,
                    &mut document,
                    &row,
                    completion.as_deref(),
                    theme,
                    roots,
                )?;
                document.blank_lines(1)?;
            }
            Kind::ToolComplete | Kind::ToolError | Kind::ToolStopped => {}
            Kind::ElicitationAnswered => {
                if context.list_length(row.elicitation_answers)? == 0 {
                    user(&source, &mut document, row.text, theme)?;
                } else {
                    for pair in backend.answer_pairs(row.elicitation_answers) {
                        let (question, answer) = pair?;
                        let mut writer = context.spool.writer();
                        context.spool.copy_to(question, &mut writer)?;
                        writer.write_all(b": ")?;
                        context.spool.copy_to(answer, &mut writer)?;
                        user(&source, &mut document, writer.finish()?, theme)?;
                    }
                }
                document.blank_lines(1)?;
            }
            Kind::CompactionSummary => {
                if row.text.len != 0 {
                    document.literal(
                        "· Compaction summary",
                        Style::default().add_modifier(Modifier::DIM),
                    )?;
                    document.end_line()?;
                    markdown(context, &mut document, row.text, theme)?;
                    document.blank_lines(1)?;
                }
            }
            Kind::EmptyOutput
            | Kind::ContextReset
            | Kind::SessionCleared
            | Kind::Compacted
            | Kind::Summary
            | Kind::Notice
            | Kind::Advisory => {
                let style = Style::default().add_modifier(
                    if matches!(
                        row.kind,
                        Kind::Notice | Kind::ContextReset | Kind::SessionCleared
                    ) {
                        Modifier::BOLD
                    } else {
                        Modifier::DIM
                    },
                );
                document.literal("· ", style)?;
                document.text(row.text, style)?;
                document.end_line()?;
                document.blank_lines(1)?;
            }
        }
        index += 1;
    }
    for output in projection.unfinished.iter() {
        check(&current)?;
        let output = output?;
        let style = Style::default().fg(theme.hint);
        document.literal("Retained tool output · completion unavailable · ", style)?;
        document.text(output.tool_call_id, style)?;
        document.end_line()?;
        ansi(context, &mut document, output.text)?;
        document.blank_lines(1)?;
    }
    glyph::index(&document, current)?;
    document.seal()?;
    Ok(document)
}

fn plain(
    source: &DiskSource<'_>,
    document: &mut Document<'_>,
    text: TextRef,
    first: &'static str,
    rest: &'static str,
    body: Style,
    prefix: Style,
) -> io::Result<()> {
    document.prefix(Some((first, rest, prefix)))?;
    let mut start = 0;
    loop {
        let end = source.find(&text, b'\n', start)?.unwrap_or(text.len);
        document.text(text.subrange(start..end)?, body)?;
        document.end_line()?;
        if end == text.len {
            break;
        }
        start = end + 1;
    }
    document.prefix(None)
}
fn user(
    source: &DiskSource<'_>,
    document: &mut Document<'_>,
    text: TextRef,
    theme: &Theme,
) -> io::Result<()> {
    plain(
        source,
        document,
        text,
        "› ",
        "  ",
        Style::default().fg(theme.text),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    )
}
fn markdown(
    context: &Rc<Context>,
    document: &mut Document<'_>,
    text: TextRef,
    theme: &Theme,
) -> io::Result<()> {
    document.prefix(Some(("• ", "  ", Style::default().fg(theme.hint))))?;
    let source = DiskSource {
        store: &context.store,
        spool: &context.spool,
    };
    if Disk(Rc::clone(context)).trim_empty(text)? {
        document.literal("…", Style::default())?;
        document.end_line()?;
    } else {
        let mut parser = pulldown_cmark::DiskParser::new(
            text,
            Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
            &context.store,
            &context.spool,
        )?;
        let mut builder = MarkdownBuilder::new(&source, document)?;
        while let Some((event, _)) = parser.next_event()? {
            builder.handle(event, |text| text)?;
        }
        if builder.finish()? == 0 {
            document.literal("…", Style::default())?;
            document.end_line()?;
        }
    }
    document.prefix(None)
}
fn ansi(context: &Context, document: &mut Document<'_>, text: TextRef) -> io::Result<()> {
    ansi::render(
        &context.spool.view(text)?,
        ansi::Mode::Text,
        |range| text.subrange(range),
        document,
    )
    .map_err(ansi::Error::into_io)
}
fn tool(
    context: &Context,
    document: &mut Document<'_>,
    start: &Row,
    completion: Option<&Row>,
    theme: &Theme,
    roots: Option<&SessionPathRoots>,
) -> io::Result<()> {
    let style = Style::default().fg(theme.text).add_modifier(Modifier::BOLD);
    document.literal("tool ", style)?;
    document.literal(
        match completion.map(|row| row.kind) {
            Some(Kind::ToolComplete) => "✓ ",
            Some(Kind::ToolError) => "✗ ",
            Some(Kind::ToolStopped) => "◼ ",
            _ => "",
        },
        style,
    )?;
    document.literal("· ", style)?;
    document.text(start.text, style)?;
    document.end_line()?;
    if let Some(tool) = &start.tool {
        ansi(context, document, tool.args_preview)?;
        for diff in context.items(tool.diffs)? {
            let diff = diff?;
            let path = display_path(context, context.string(diff, "path")?, roots)?;
            for (field, label) in [("old_text", "Before: "), ("new_text", "After: ")] {
                if let Some(text) = context.optional_text(diff, field)? {
                    document.literal(label, Style::default())?;
                    document.text(path, Style::default())?;
                    document.end_line()?;
                    ansi(context, document, text)?;
                }
            }
        }
    }
    if let Some(completion) = completion {
        if completion.async_subagent {
            document.literal("runs in background", Style::default())?;
            document.end_line()?;
        } else if context.list_length(completion.output)? == 0 {
            ansi(context, document, completion.text)?;
        } else {
            let mut has_text = false;
            for block in context.items(completion.output)? {
                let block = block?;
                let kind = context.string(block, "kind")?;
                if context.text_is(kind, "text")? || context.text_is(kind, "resource")? {
                    has_text |= context
                        .optional_text(block, "text")?
                        .is_some_and(|text| text.len != 0);
                }
            }
            if !has_text && completion.text.len != 0 {
                ansi(context, document, completion.text)?;
            }
            ansi(context, document, summarize(context, completion.output)?)?;
        }
    }
    Ok(())
}
fn summarize(context: &Context, blocks: List) -> io::Result<TextRef> {
    let mut writer = context.spool.writer();
    for (index, block) in context.items(blocks)?.enumerate() {
        if index != 0 {
            writer.write_all(b"\n")?;
        }
        let block = block?;
        let kind = context.string(block, "kind")?;
        if context.text_is(kind, "text")? {
            context
                .spool
                .copy_to(context.string(block, "text")?, &mut writer)?;
        } else if context.text_is(kind, "image")? || context.text_is(kind, "audio")? {
            writer.write_all(b"[")?;
            context.spool.copy_to(kind, &mut writer)?;
            writer.write_all(b" ")?;
            context
                .spool
                .copy_to(context.string(block, "mime_type")?, &mut writer)?;
            writer.write_all(b"]")?;
        } else if context.text_is(kind, "resource_link")? {
            writer.write_all(b"[link ")?;
            context
                .spool
                .copy_to(context.string(block, "name")?, &mut writer)?;
            writer.write_all(b": ")?;
            context
                .spool
                .copy_to(context.string(block, "uri")?, &mut writer)?;
            writer.write_all(b"]")?;
        } else {
            if let Some(text) = context.optional_text(block, "text")? {
                context.spool.copy_to(text, &mut writer)?;
                writer.write_all(b"\n")?;
            }
            writer.write_all(b"[resource ")?;
            context
                .spool
                .copy_to(context.string(block, "uri")?, &mut writer)?;
            writer.write_all(b"]")?;
        }
    }
    writer.finish()
}
fn display_path(
    context: &Context,
    raw: TextRef,
    roots: Option<&SessionPathRoots>,
) -> io::Result<TextRef> {
    let Some(roots) = roots else { return Ok(raw) };
    if raw.len == 0 {
        return Ok(raw);
    }
    let source = context.spool.view(raw)?;
    let windows = source
        .byte_at(0)?
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        && source.byte_at(1)? == Some(b':')
        && matches!(source.byte_at(2)?, Some(b'/' | b'\\'));
    let absolute = matches!(source.byte_at(0)?, Some(b'/' | b'\\')) || windows;
    let mut writer = context.spool.writer();
    let mut buffer = [0; 8192];
    let mut offset = 0;
    while offset < raw.len {
        let count = source.read_at(offset, &mut buffer)?;
        if count == 0 {
            return Err(invalid("retained path disappeared"));
        }
        for byte in &mut buffer[..count] {
            if *byte == b'\\' {
                *byte = b'/';
            }
        }
        if windows && offset == 0 {
            buffer[0] = buffer[0].to_ascii_lowercase();
        }
        writer.write_all(&buffer[..count])?;
        offset += count as u64;
    }
    let normalized = writer.finish()?;
    if !absolute {
        let source = context.spool.view(normalized)?;
        let start = if source.byte_at(0)? == Some(b'.') && source.byte_at(1)? == Some(b'/') {
            2
        } else {
            0
        };
        return if start < normalized.len {
            normalized.subrange(start..normalized.len)
        } else {
            Ok(raw)
        };
    }
    for (path, repo) in roots
        .workspace_repos
        .iter()
        .map(|repo| (repo.source_path.as_str(), Some(repo.name.as_str())))
        .chain(std::iter::once((roots.project_path.as_str(), None)))
        .chain(roots.main_repo_path.as_deref().map(|path| (path, None)))
    {
        let mut root = path.replace('\\', "/");
        if root.len() >= 3
            && root.as_bytes()[0].is_ascii_alphabetic()
            && root.as_bytes()[1] == b':'
            && root.as_bytes()[2] == b'/'
        {
            root.replace_range(0..1, &root[0..1].to_ascii_lowercase());
        }
        if !root.ends_with('/') {
            root.push('/');
        }
        if normalized.len >= root.len() as u64
            && context.text_is(normalized.subrange(0..root.len() as u64)?, &root)?
        {
            let relative = normalized.subrange(root.len() as u64..normalized.len)?;
            if let Some(repo) = repo {
                let mut writer = context.spool.writer();
                writer.write_all(repo.as_bytes())?;
                writer.write_all(b"/")?;
                context.spool.copy_to(relative, &mut writer)?;
                return writer.finish();
            }
            return Ok(relative);
        }
    }
    Ok(raw)
}
fn check(current: &impl Fn() -> bool) -> io::Result<()> {
    if current() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "retained document superseded",
        ))
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
