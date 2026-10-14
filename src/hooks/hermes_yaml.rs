//! Edits only the root `hooks` entry of Hermes's `config.yaml`. Every byte outside that entry
//! survives, including key types, tags, anchors and comments. The entry itself is normalized
//! when it changes, so tags, anchors, aliases and non-string keys inside it are refused.

use std::ops::Range;

use anyhow::{anyhow, bail, ensure, Result};
use granit_parser::{Event, Marker, Parser, ScalarStyle, StructureStyle};
use serde_json::{json, Map, Value};

pub(crate) type Hooks = Map<String, Value>;

enum Root {
    Empty,
    Mapping,
}

struct HooksEntry {
    /// From the start of the `hooks` line through its last line, newline included.
    range: Range<usize>,
    value: Hooks,
}

struct Layout {
    root: Root,
    /// Column of the root keys.
    indent: usize,
    /// End byte of the last root leaf; new entries go after its line.
    last_end: usize,
    hooks: Option<HooksEntry>,
}

struct Frame {
    map: bool,
    key_next: bool,
    flow: bool,
}

enum Slot {
    Absent,
    KeyPending(Range<usize>),
    Inside { key: Range<usize>, end: usize },
    Done { key: Range<usize>, end: usize },
}

/// Apply `change` to the root `hooks` mapping. `None` means the file is already as wanted.
pub(crate) fn edit(
    src: &str,
    change: impl FnOnce(&mut Hooks) -> Result<()>,
) -> Result<Option<String>> {
    let layout = parse_layout(src)?;
    let before = layout
        .hooks
        .as_ref()
        .map(|entry| entry.value.clone())
        .unwrap_or_default();
    let mut after = before.clone();
    change(&mut after)?;
    if after == before {
        return Ok(None);
    }

    let newline = if src.contains("\r\n") { "\r\n" } else { "\n" };
    let rendered = render(layout.indent, &after)?.replace('\n', newline);
    let mut out = match (&layout.hooks, after.is_empty()) {
        (Some(entry), true) => splice(src, entry.range.clone(), ""),
        (Some(entry), false) => splice(src, entry.range.clone(), &rendered),
        (None, _) => {
            let at = match layout.root {
                Root::Empty => src.len(),
                Root::Mapping => line_end(src, layout.last_end),
            };
            let separator = if at > 0 && !src[..at].ends_with('\n') {
                newline
            } else {
                ""
            };
            splice(src, at..at, &format!("{separator}{rendered}"))
        }
    };
    if out.is_empty() {
        out.push('\n');
    }

    let written = parse_layout(&out)?.hooks.map(|entry| entry.value);
    ensure!(
        written.unwrap_or_default() == after,
        "edited Hermes config did not round-trip"
    );
    Ok(Some(out))
}

fn splice(src: &str, range: Range<usize>, replacement: &str) -> String {
    format!("{}{replacement}{}", &src[..range.start], &src[range.end..])
}

fn render(indent: usize, hooks: &Hooks) -> Result<String> {
    let yaml = crate::yaml::to_string(&json!({ "hooks": hooks }))?;
    let pad = " ".repeat(indent);
    Ok(yaml.lines().map(|line| format!("{pad}{line}\n")).collect())
}

fn byte(marker: &Marker) -> Result<usize> {
    marker
        .byte_offset()
        .ok_or_else(|| anyhow!("YAML parser reported no byte offset"))
}

/// Byte just past the newline of the line containing `at`, or the end of input.
fn line_end(src: &str, at: usize) -> usize {
    if at > 0 && src.as_bytes()[at - 1] == b'\n' {
        return at;
    }
    src[at..].find('\n').map_or(src.len(), |i| at + i + 1)
}

/// Extend an entry to its line end, then over comment-only lines indented deeper than its key.
fn entry_end(src: &str, end: usize, key_col: usize) -> usize {
    let mut at = line_end(src, end);
    while at < src.len() {
        let next = line_end(src, at + 1);
        let line = &src[at..next];
        let text = line.trim_start();
        if !text.starts_with('#') || line.len() - text.len() <= key_col {
            break;
        }
        at = next;
    }
    at
}

/// Plain keys that YAML would read as numbers, booleans or null are not strings.
fn is_string_key(text: &str) -> bool {
    matches!(
        serde_saphyr::from_str_with_options::<Value>(text, crate::yaml::options()),
        Ok(Value::String(_))
    )
}

fn parse_layout(src: &str) -> Result<Layout> {
    let options = granit_parser::options! { emit_comments: false };
    let mut docs = 0;
    let mut stack: Vec<Frame> = Vec::new();
    let mut root = Root::Empty;
    let mut indent = None;
    let mut last_end = 0;
    let mut slot = Slot::Absent;

    for item in Parser::new_from_str_with_options(src, options) {
        let (event, span) = item.map_err(|e| anyhow!("{e}"))?;
        let (anchor, tag, kind) = match event {
            Event::DocumentStart(..) => {
                docs += 1;
                ensure!(docs == 1, "multiple YAML documents are not supported");
                continue;
            }
            Event::Scalar(value, style, anchor, tag) => (
                anchor,
                tag.is_some(),
                Kind::Scalar(value.into_owned(), style),
            ),
            Event::Alias(_) => (0, false, Kind::Alias),
            Event::MappingStart(style, anchor, tag) => {
                (anchor, tag.is_some(), Kind::Collection(true, style))
            }
            Event::SequenceStart(style, anchor, tag) => {
                (anchor, tag.is_some(), Kind::Collection(false, style))
            }
            Event::MappingEnd | Event::SequenceEnd => {
                if let Some(frame) = stack.pop() {
                    if frame.flow {
                        let end = byte(&span.end)?;
                        note_end(&mut last_end, &mut slot, end);
                    }
                }
                finish(&mut stack, &mut slot);
                continue;
            }
            _ => continue,
        };

        let key_role = stack.last().is_some_and(|f| f.map && f.key_next);
        let depth = stack.len();
        if depth == 0 {
            match &kind {
                Kind::Collection(true, StructureStyle::Block) => root = Root::Mapping,
                Kind::Collection(true, StructureStyle::Flow) => {
                    bail!("a flow-style Hermes config root is not supported")
                }
                Kind::Scalar(value, ScalarStyle::Plain) if value.is_empty() && !tag => {}
                _ => bail!("Hermes config root is not a YAML mapping"),
            }
        }

        if depth == 1 {
            if key_role {
                indent.get_or_insert(span.start.col());
                if matches!(&kind, Kind::Scalar(name, _) if name == "hooks") {
                    ensure!(matches!(slot, Slot::Absent), "duplicate `hooks` key");
                    slot = Slot::KeyPending(byte(&span.start)?..byte(&span.end)?);
                }
            } else if let Slot::KeyPending(key) = slot {
                let empty = matches!(&kind, Kind::Scalar(v, ScalarStyle::Plain) if matches!(v.as_str(), "" | "null" | "Null" | "NULL" | "~"));
                let end = key.end;
                ensure!(
                    empty || matches!(kind, Kind::Collection(true, _)),
                    "`hooks` must be a mapping"
                );
                slot = Slot::Inside { key, end };
            }
        }

        if matches!(slot, Slot::Inside { .. }) {
            ensure!(anchor == 0, "anchors inside `hooks` are not supported");
            ensure!(!tag, "tags inside `hooks` are not supported");
            ensure!(
                !matches!(kind, Kind::Alias),
                "aliases inside `hooks` are not supported"
            );
            if key_role {
                match &kind {
                    Kind::Scalar(text, ScalarStyle::Plain) => ensure!(
                        is_string_key(text),
                        "non-string key `{text}` inside `hooks` is not supported"
                    ),
                    Kind::Scalar(..) => {}
                    _ => bail!("complex keys inside `hooks` are not supported"),
                }
            }
        }

        match kind {
            Kind::Collection(map, style) => stack.push(Frame {
                map,
                key_next: true,
                flow: style == StructureStyle::Flow,
            }),
            Kind::Scalar(ref value, ScalarStyle::Plain) if value.is_empty() => {
                finish(&mut stack, &mut slot);
            }
            Kind::Scalar(..) | Kind::Alias => {
                note_end(&mut last_end, &mut slot, byte(&span.end)?);
                finish(&mut stack, &mut slot);
            }
        }
    }

    let indent = indent.unwrap_or(0);
    let hooks = match slot {
        Slot::Absent => None,
        Slot::Done { key, end } => Some(hooks_entry(src, key, end, indent)?),
        _ => bail!("`hooks` entry is incomplete"),
    };
    Ok(Layout {
        root,
        indent,
        last_end,
        hooks,
    })
}

enum Kind {
    Scalar(String, ScalarStyle),
    Alias,
    /// `true` for mappings.
    Collection(bool, StructureStyle),
}

fn note_end(last_end: &mut usize, slot: &mut Slot, end: usize) {
    *last_end = (*last_end).max(end);
    if let Slot::Inside { end: slot_end, .. } = slot {
        *slot_end = (*slot_end).max(end);
    }
}

/// A node just completed: advance its parent's key/value alternation and close `hooks`.
fn finish(stack: &mut [Frame], slot: &mut Slot) {
    if let Some(frame) = stack.last_mut().filter(|f| f.map) {
        frame.key_next = !frame.key_next;
    }
    if stack.len() == 1 {
        if let Slot::Inside { key, end } = slot {
            *slot = Slot::Done {
                key: key.clone(),
                end: *end,
            };
        }
    }
}

fn hooks_entry(src: &str, key: Range<usize>, end: usize, indent: usize) -> Result<HooksEntry> {
    let start = src[..key.start].rfind('\n').map_or(0, |i| i + 1);
    ensure!(
        src[start..key.start].trim().is_empty(),
        "`hooks` key is not at the start of its line"
    );
    let range = start..entry_end(src, end.max(key.end), indent);
    let parsed: Value =
        serde_saphyr::from_str_with_options(&src[range.clone()], crate::yaml::options())?;
    let value = match parsed.get("hooks") {
        Some(Value::Object(map)) => map.clone(),
        _ => Hooks::new(),
    };
    Ok(HooksEntry { range, value })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(hooks: &mut Hooks) -> Result<()> {
        let Value::Array(entries) = hooks.entry("pre_tool_call").or_insert_with(|| json!([]))
        else {
            bail!("not a sequence");
        };
        entries.retain(|e| e["command"] != "aoe-x");
        entries.push(json!({"command": "aoe-x"}));
        Ok(())
    }

    fn hooks_of(src: &str) -> Hooks {
        parse_layout(src)
            .unwrap()
            .hooks
            .map(|e| e.value)
            .unwrap_or_default()
    }

    #[test]
    fn edits_preserve_everything_outside_hooks() {
        for src in [
            "model: hermes-pro\ncustom: {1: foo}\n",
            "# top\ntagged: !vendor/type {a: 1}\nforced: !!str 123\nbase: &b {x: 1}\nother:\n  <<: *b\n",
            "model: a\nhooks:\n  pre_tool_call:\n    - command: user\n# root\ncustom: {1: foo}\n",
        ] {
            let installed = edit(src, add).unwrap().unwrap();
            assert!(installed.contains("aoe-x"), "{installed}");
            let user = hooks_of(src).contains_key("pre_tool_call");
            let restored = edit(&installed, |h| {
                let Value::Array(e) = &mut h["pre_tool_call"] else {
                    unreachable!()
                };
                e.retain(|e| e["command"] != "aoe-x");
                if e.is_empty() {
                    h.clear();
                }
                Ok(())
            })
            .unwrap()
            .unwrap();
            if user {
                assert_eq!(hooks_of(&restored), hooks_of(src));
            } else {
                assert_eq!(restored, src);
            }
            for kept in ["custom: {1: foo}\n", "tagged: !vendor/type {a: 1}\n", "# top\n"] {
                assert_eq!(src.contains(kept), installed.contains(kept), "{kept}");
            }
        }
    }

    #[test]
    fn edits_handle_hooks_shapes_and_layout() {
        for (src, kept_suffix) in [
            ("", ""),
            ("# only a comment\n", ""),
            ("hooks:\n", ""),
            ("hooks: null # gone\nmodel: a\n", "model: a\n"),
            ("hooks: {}\nmodel: a\n", "model: a\n"),
            ("model: a", ""),
            ("model: a\n...\n", "...\n"),
            ("name: h\u{e9}llo\nhooks: {}\n", ""),
            (
                "hooks:\n  pre: []\n  # mine\n# root\nmodel: a\n",
                "# root\nmodel: a\n",
            ),
            ("  model: a\n", ""),
        ] {
            let out = edit(src, add).unwrap().unwrap();
            assert!(out.ends_with(kept_suffix), "{src:?} -> {out:?}");
            assert!(
                !out.contains("# gone") && !out.contains("# mine"),
                "{out:?}"
            );
            assert_eq!(
                hooks_of(&out)["pre_tool_call"],
                json!([{"command": "aoe-x"}])
            );
            assert!(edit(&out, add).unwrap().is_none(), "{out:?}");
        }
        let out = edit("# only a comment\n", add).unwrap().unwrap();
        assert!(out.starts_with("# only a comment\nhooks:\n"), "{out:?}");
    }

    #[test]
    fn edits_keep_crlf_and_leave_current_formatting_alone() {
        let out = edit("model: a\r\n", add).unwrap().unwrap();
        assert!(out.starts_with("model: a\r\n"));
        assert!(!out.replace("\r\n", "").contains('\n'), "{out:?}");

        let hand_formatted = "hooks:\n    pre_tool_call:\n        - command: aoe-x\n";
        assert!(edit(hand_formatted, add).unwrap().is_none());
        assert!(edit("model: a\n", |_| Ok(())).unwrap().is_none());
    }

    #[test]
    fn edits_refuse_unsupported_shapes() {
        for src in [
            "- a\n",
            "null\n",
            "{a: 1}\n",
            "a: 1\n---\nb: 2\n",
            "hooks: {}\nhooks: {}\n",
            "hooks: 42\n",
            "hooks: []\n",
            "hooks:\n  pre_tool_call: 5\n",
            "hooks:\n  1: []\n",
            "hooks:\n  true: []\n",
            "hooks:\n  pre_tool_call: !t []\n",
            "hooks: &h\n  pre_tool_call: []\nother: *h\n",
            "base: &c {command: x}\nhooks:\n  pre_tool_call:\n    - *c\n",
            "hooks:\n  pre_tool_call:\n    - command: x\n      extra: {1: a}\n",
            "hooks:\n  pre_tool_call: [\n",
        ] {
            assert!(edit(src, add).is_err(), "{src:?}");
        }
    }
}
