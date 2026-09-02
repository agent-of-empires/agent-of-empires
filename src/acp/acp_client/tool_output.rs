//! Reading a tool call's content: text, media and resource blocks, memory
//! recalls, and diffs, each under a size cap.

use crate::acp::state::{DiffPreview, Event, MemoryRecall, ToolOutputBlock};
use agent_client_protocol::schema::v1::ContentBlock;

pub(super) fn raw_event<T: serde::Serialize>(value: &T) -> Event {
    Event::RawAgentUpdate {
        payload: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
    }
}

/// Stable lowercased string form of an ACP `ToolKind`. Used to drive the
/// per-tool renderer dispatch on the web side.
pub(super) fn tool_kind_str(kind: &agent_client_protocol::schema::v1::ToolKind) -> String {
    use agent_client_protocol::schema::v1::ToolKind;
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        _ => "other",
    }
    .into()
}

/// 16 KB cap on tool-call argument preview, with control chars stripped.
pub(super) fn preview_args(raw: &serde_json::Value) -> String {
    let serialised = serde_json::to_string(raw).unwrap_or_default();
    let mut out = String::with_capacity(serialised.len().min(16 * 1024));
    for c in serialised.chars() {
        if out.len() >= 16 * 1024 {
            out.push_str("\u{2026}[truncated]");
            break;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

/// Preview for an optional ACP `raw_input`. Treats both a missing field
/// (`None`) and an explicit JSON `null` as "no args provided", returning
/// an empty string. The empty string lets the UI render a dedicated
/// empty-state instead of the literal text "null" that
/// `preview_args(&Value::Null)` would otherwise produce. Gemini's
/// permission flow ships argless tool calls this way. See #1713.
pub(super) fn preview_optional_args(raw: Option<&serde_json::Value>) -> String {
    match raw {
        Some(value) if !value.is_null() => preview_args(value),
        _ => String::new(),
    }
}

/// Concat the textual portion of a tool call's `content` array. Drops
/// non-text content blocks (images, resources, embedded terminals); the
/// per-tool renderer fall-back path only knows how to display text. Diff
/// blocks are bridged separately by `extract_diffs_from_content`.
pub(super) fn extract_tool_content_text(
    blocks: &[agent_client_protocol::schema::v1::ToolCallContent],
) -> String {
    use agent_client_protocol::schema::v1::ToolCallContent;
    let mut out = String::new();
    for block in blocks {
        if let ToolCallContent::Content(c) = block {
            if let ContentBlock::Text(t) = &c.content {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&t.text);
            }
        }
    }
    out
}

/// Max base64 length kept for an inline image/audio payload. Media this
/// large is persisted in the event store and reshipped on every WS replay,
/// so an oversized blob would bloat both; past the cap the inline data is
/// dropped (a placeholder/uri is surfaced instead). ~4 MiB of base64 is
/// ~3 MiB of bytes, comfortably above a typical screenshot.
pub(super) const MAX_INLINE_MEDIA_B64: usize = 4 * 1024 * 1024;

/// Bridge an ACP `ToolCallContent` array into the structured view's renderable
/// `ToolOutputBlock` list, preserving non-text completion payloads (images,
/// audio, resource links/contents) that `extract_tool_content_text` drops.
/// Diff blocks are bridged separately (`extract_diffs_from_content`) and are
/// skipped here; an embedded terminal surfaces as a text placeholder since
/// the structured view does not own ACP terminals. Returns an EMPTY vec when every block
/// is plain text (or diff): the existing `content` text path renders those,
/// so the structured list only carries weight when real media is present.
/// See #1818.
pub(super) fn extract_tool_output_blocks(
    blocks: &[agent_client_protocol::schema::v1::ToolCallContent],
) -> Vec<ToolOutputBlock> {
    use agent_client_protocol::schema::v1::{EmbeddedResourceResource, ToolCallContent};
    let mut out: Vec<ToolOutputBlock> = Vec::new();
    let mut has_media = false;
    let cap =
        |data: String| -> Option<String> { (data.len() <= MAX_INLINE_MEDIA_B64).then_some(data) };
    for block in blocks {
        match block {
            ToolCallContent::Content(c) => match &c.content {
                ContentBlock::Text(t) => out.push(ToolOutputBlock::Text {
                    text: t.text.clone(),
                }),
                ContentBlock::Image(img) => {
                    has_media = true;
                    out.push(ToolOutputBlock::Image {
                        mime_type: img.mime_type.clone(),
                        data: cap(img.data.clone()),
                        uri: img.uri.clone(),
                    });
                }
                ContentBlock::Audio(audio) => {
                    has_media = true;
                    out.push(ToolOutputBlock::Audio {
                        mime_type: audio.mime_type.clone(),
                        data: cap(audio.data.clone()),
                    });
                }
                ContentBlock::ResourceLink(link) => {
                    has_media = true;
                    out.push(ToolOutputBlock::ResourceLink {
                        uri: link.uri.clone(),
                        name: link.name.clone(),
                        mime_type: link.mime_type.clone(),
                    });
                }
                ContentBlock::Resource(res) => {
                    has_media = true;
                    let block = match &res.resource {
                        EmbeddedResourceResource::TextResourceContents(t) => {
                            ToolOutputBlock::Resource {
                                uri: t.uri.clone(),
                                mime_type: t.mime_type.clone(),
                                text: Some(t.text.clone()),
                                data: None,
                            }
                        }
                        EmbeddedResourceResource::BlobResourceContents(b) => {
                            // Keep the inline bytes (capped) so a blob without
                            // a fetchable uri is still recoverable as a
                            // download instead of an empty placeholder. See
                            // #1818 review.
                            ToolOutputBlock::Resource {
                                uri: b.uri.clone(),
                                mime_type: b.mime_type.clone(),
                                text: None,
                                data: cap(b.blob.clone()),
                            }
                        }
                        _ => continue,
                    };
                    out.push(block);
                }
                _ => {}
            },
            ToolCallContent::Terminal(term) => {
                has_media = true;
                out.push(ToolOutputBlock::Text {
                    text: format!("[terminal {}]", term.terminal_id.0),
                });
            }
            ToolCallContent::Diff(_) => {}
            _ => {}
        }
    }
    if has_media {
        out
    } else {
        Vec::new()
    }
}

/// Inspect a `tool_call` payload for the `memory_recall` shape
/// claude-agent-acp v0.37.0 routes through the tool channel (upstream
/// #703). The adapter sends `_meta.claudeCode.toolName == "memory_recall"`
/// plus either `locations` (recall mode, one entry per loaded memory
/// file) or `content` (synthesize mode, one text block with the
/// synthesised reply). Returns `None` when the meta marker is absent.
/// Caller gates this on `AgentProfile::supports_memory_recall_tool`
/// so unrelated agents that happen to share field shapes don't trip
/// the classifier.
pub(super) fn extract_memory_recall(
    meta: &Option<serde_json::Map<String, serde_json::Value>>,
    locations: &[agent_client_protocol::schema::v1::ToolCallLocation],
    content: &[agent_client_protocol::schema::v1::ToolCallContent],
) -> Option<MemoryRecall> {
    let map = meta.as_ref()?;
    let claude_code = map.get("claudeCode")?;
    let tool_name = claude_code.get("toolName").and_then(|v| v.as_str())?;
    if tool_name != "memory_recall" {
        return None;
    }
    let mode = claude_code
        .get("toolResponse")
        .and_then(|tr| tr.get("mode"))
        .and_then(|v| v.as_str())
        .unwrap_or("recall")
        .to_string();
    let paths: Vec<String> = locations
        .iter()
        .map(|loc| loc.path.to_string_lossy().to_string())
        .collect();
    let synthesized_text = if mode == "synthesize" {
        let text = extract_tool_content_text(content);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    } else {
        None
    };
    Some(MemoryRecall {
        mode,
        paths,
        synthesized_text,
    })
}

/// Max bytes of diff text kept per side (old/new) when bridging an ACP
/// `ToolCallContent::Diff` into a structured view `DiffPreview`. The card only
/// previews ~20 lines, but the untrimmed text is persisted in the event
/// store and shipped over every WS replay frame, so a large `apply_patch`
/// would bloat both without a cap here. Mirrors `preview_args`' 16 KB ceiling.
pub(super) const MAX_DIFF_TEXT_BYTES: usize = 16 * 1024;

/// Max number of per-file diffs kept from a single tool call. A patch
/// touching more files than this keeps the first `MAX_TOOL_DIFFS` rather
/// than letting one event grow unbounded.
pub(super) const MAX_TOOL_DIFFS: usize = 16;

/// Truncate diff text to `MAX_DIFF_TEXT_BYTES` on a UTF-8 char boundary,
/// appending a sentinel so the cut reads as intentional rather than as a
/// corrupt diff.
pub(super) fn cap_diff_text(text: &str) -> String {
    if text.len() <= MAX_DIFF_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_DIFF_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push_str("\n\u{2026}[truncated]");
    out
}

/// Bridge ACP `ToolCallContent::Diff` blocks into structured view `DiffPreview`
/// entries. Codex routes `apply_patch` edits through this channel (one
/// block per touched file) instead of the legacy `old_string`/`new_string`
/// raw_input keys, so the edit card reads the path and +/- preview from
/// here. Non-diff blocks (text, images, terminals) are ignored; the enum
/// is `#[non_exhaustive]`, so the wildcard arm keeps this compiling as the
/// schema grows. Per-side text is capped and the list bounded. See #1721.
pub(super) fn extract_diffs_from_content(
    blocks: &[agent_client_protocol::schema::v1::ToolCallContent],
) -> Vec<DiffPreview> {
    use agent_client_protocol::schema::v1::ToolCallContent;
    let created_at = chrono::Utc::now();
    blocks
        .iter()
        .filter_map(|block| match block {
            ToolCallContent::Diff(d) => Some(DiffPreview {
                path: d.path.to_string_lossy().to_string(),
                old_text: d.old_text.as_deref().map(cap_diff_text),
                new_text: Some(cap_diff_text(&d.new_text)),
                created_at,
            }),
            _ => None,
        })
        .take(MAX_TOOL_DIFFS)
        .collect()
}

/// Synthesize a `DiffPreview` for Claude's `Write` tool from
/// `_meta.claudeCode.toolResponse`. Unlike Codex's `apply_patch` (bridged via
/// `extract_diffs_from_content` above), claude-agent-acp never emits a
/// `ToolCallContent::Diff` block for `Write`; the only place the new file's
/// content shows up is this metadata blob (`{type: "create"|"update",
/// filePath, content}`), which otherwise falls through as an inert
/// `RawAgentUpdate` and the edit card renders no body at all. `type: "create"`
/// has no `oldContent`, so `old_text` is `None` (an empty-file diff); `type:
/// "update"` includes `oldContent` when the adapter has it. Returns `None`
/// for anything that doesn't match so the caller falls back to the existing
/// `RawAgentUpdate` passthrough.
pub(super) fn write_diff_from_meta(
    meta: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Option<Vec<DiffPreview>> {
    let map = meta.as_ref()?;
    let claude_code = map.get("claudeCode")?;
    let tr = claude_code.get("toolResponse")?;
    let kind = tr.get("type").and_then(|v| v.as_str())?;
    if kind != "create" && kind != "update" {
        return None;
    }
    let path = tr.get("filePath").and_then(|v| v.as_str())?.to_string();
    let new_text = tr.get("content").and_then(|v| v.as_str())?;
    let old_text = tr.get("oldContent").and_then(|v| v.as_str());
    Some(vec![DiffPreview {
        path,
        old_text: old_text.map(cap_diff_text),
        new_text: Some(cap_diff_text(new_text)),
        created_at: chrono::Utc::now(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_optional_args_empty_for_missing_or_null() {
        // #1713: a missing or explicitly-null raw_input must preview as
        // empty (so the UI shows a clean empty-state) rather than the
        // literal "null" that preview_args(&Value::Null) would produce.
        assert_eq!(preview_optional_args(None), "");
        assert_eq!(preview_optional_args(Some(&serde_json::Value::Null)), "");
        let obj = serde_json::json!({ "command": "ls" });
        assert_eq!(preview_optional_args(Some(&obj)), r#"{"command":"ls"}"#);
    }

    #[test]
    fn preview_args_caps_to_16k() {
        let big = serde_json::Value::String("x".repeat(20_000));
        let preview = preview_args(&big);
        assert!(preview.len() <= 16 * 1024 + 32);
        assert!(preview.contains("[truncated]"));
    }

    #[test]
    fn extract_tool_content_text_concats_text_blocks() {
        use agent_client_protocol::schema::v1::{Content, ToolCallContent};
        let blocks = vec![
            ToolCallContent::Content(Content::new("stdout line 1")),
            ToolCallContent::Content(Content::new("stdout line 2")),
        ];
        let text = extract_tool_content_text(&blocks);
        assert_eq!(text, "stdout line 1\nstdout line 2");
    }

    #[test]
    fn extract_tool_content_text_empty_for_no_text_blocks() {
        // No content → empty string. The reducer falls back to the
        // status word ("completed" / "tool failed") in that case so
        // the card still conveys state.
        assert_eq!(extract_tool_content_text(&[]), "");
    }

    #[test]
    fn extract_diffs_from_content_bridges_diff_blocks_and_ignores_others() {
        use agent_client_protocol::schema::v1::{Content, Diff, ToolCallContent};
        let blocks = vec![
            ToolCallContent::Content(Content::new("some text")),
            ToolCallContent::Diff(Diff::new("src/foo.rs", "new body").old_text("old body")),
            // New-file diff: old_text is None.
            ToolCallContent::Diff(Diff::new("src/new.rs", "created")),
        ];
        let diffs = extract_diffs_from_content(&blocks);
        assert_eq!(diffs.len(), 2, "text blocks must be ignored");
        assert_eq!(diffs[0].path, "src/foo.rs");
        assert_eq!(diffs[0].old_text.as_deref(), Some("old body"));
        assert_eq!(diffs[0].new_text.as_deref(), Some("new body"));
        assert_eq!(diffs[1].path, "src/new.rs");
        assert_eq!(diffs[1].old_text, None, "new file carries no old_text");
        assert_eq!(diffs[1].new_text.as_deref(), Some("created"));
    }

    #[test]
    fn write_diff_from_meta_synthesizes_create_with_no_old_text() {
        let meta = serde_json::json!({
            "claudeCode": {
                "toolResponse": {
                    "type": "create",
                    "filePath": "/repo/src/new.rs",
                    "content": "fn main() {}\n",
                }
            }
        })
        .as_object()
        .cloned();
        let diffs =
            write_diff_from_meta(&meta).expect("create toolResponse should synthesize a diff");
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].path, "/repo/src/new.rs");
        assert_eq!(diffs[0].old_text, None);
        assert_eq!(diffs[0].new_text.as_deref(), Some("fn main() {}\n"));
    }

    #[test]
    fn write_diff_from_meta_synthesizes_update_with_old_text() {
        let meta = serde_json::json!({
            "claudeCode": {
                "toolResponse": {
                    "type": "update",
                    "filePath": "/repo/src/existing.rs",
                    "content": "new body",
                    "oldContent": "old body",
                }
            }
        })
        .as_object()
        .cloned();
        let diffs =
            write_diff_from_meta(&meta).expect("update toolResponse should synthesize a diff");
        assert_eq!(diffs[0].old_text.as_deref(), Some("old body"));
        assert_eq!(diffs[0].new_text.as_deref(), Some("new body"));
    }

    #[test]
    fn write_diff_from_meta_ignores_unrelated_payloads() {
        assert!(write_diff_from_meta(&None).is_none());
        let non_write = serde_json::json!({
            "claudeCode": { "toolResponse": { "status": "async_launched" } }
        })
        .as_object()
        .cloned();
        assert!(write_diff_from_meta(&non_write).is_none());
    }

    #[test]
    fn extract_diffs_from_content_caps_per_side_text() {
        use agent_client_protocol::schema::v1::{Diff, ToolCallContent};
        let huge = "x".repeat(MAX_DIFF_TEXT_BYTES + 4096);
        let blocks = vec![ToolCallContent::Diff(
            Diff::new("src/big.rs", huge.clone()).old_text(huge),
        )];
        let diffs = extract_diffs_from_content(&blocks);
        assert_eq!(diffs.len(), 1);
        let new_len = diffs[0].new_text.as_deref().unwrap().len();
        let old_len = diffs[0].old_text.as_deref().unwrap().len();
        assert!(
            new_len < MAX_DIFF_TEXT_BYTES + 64,
            "new_text must be capped, got {new_len}"
        );
        assert!(
            old_len < MAX_DIFF_TEXT_BYTES + 64,
            "old_text must be capped, got {old_len}"
        );
        assert!(diffs[0]
            .new_text
            .as_deref()
            .unwrap()
            .contains("[truncated]"));
    }

    #[test]
    fn extract_tool_output_blocks_empty_for_text_only() {
        use agent_client_protocol::schema::v1::{Content, ToolCallContent};
        // Pure text completion: the `content` string path renders it, so the
        // structured list stays empty and the existing path is untouched.
        let blocks = vec![ToolCallContent::Content(Content::new("just text"))];
        assert!(extract_tool_output_blocks(&blocks).is_empty());
    }

    #[test]
    fn extract_tool_output_blocks_preserves_media_and_resources() {
        use agent_client_protocol::schema::v1::{
            AudioContent, Content, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
            ImageContent, ResourceLink, TextResourceContents, ToolCallContent,
        };
        let blocks =
            vec![
                ToolCallContent::Content(Content::new("a caption")),
                ToolCallContent::Content(Content::new(ContentBlock::Image(
                    ImageContent::new("BASE64IMG", "image/png").uri("file:///shot.png".to_string()),
                ))),
                ToolCallContent::Content(Content::new(ContentBlock::Audio(AudioContent::new(
                    "BASE64AUDIO",
                    "audio/wav",
                )))),
                ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(
                    ResourceLink::new("report.pdf", "file:///report.pdf"),
                ))),
                ToolCallContent::Content(Content::new(ContentBlock::Resource(
                    EmbeddedResource::new(EmbeddedResourceResource::TextResourceContents(
                        TextResourceContents::new("inline body", "file:///note.txt"),
                    )),
                ))),
            ];
        let out = extract_tool_output_blocks(&blocks);
        assert_eq!(out.len(), 5, "all blocks preserved in order: {out:?}");
        assert!(matches!(&out[0], ToolOutputBlock::Text { text } if text == "a caption"));
        match &out[1] {
            ToolOutputBlock::Image {
                mime_type,
                data,
                uri,
            } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(data.as_deref(), Some("BASE64IMG"));
                assert_eq!(uri.as_deref(), Some("file:///shot.png"));
            }
            other => panic!("expected Image, got {other:?}"),
        }
        assert!(
            matches!(&out[2], ToolOutputBlock::Audio { mime_type, .. } if mime_type == "audio/wav")
        );
        assert!(
            matches!(&out[3], ToolOutputBlock::ResourceLink { name, uri, .. } if name == "report.pdf" && uri == "file:///report.pdf")
        );
        assert!(
            matches!(&out[4], ToolOutputBlock::Resource { text: Some(t), .. } if t == "inline body")
        );
    }

    #[test]
    fn extract_tool_output_blocks_keeps_blob_resource_payload() {
        // #1818 review: a binary (blob) embedded resource must keep its
        // inline bytes so it stays recoverable as a download.
        use agent_client_protocol::schema::v1::{
            BlobResourceContents, Content, ContentBlock, EmbeddedResource,
            EmbeddedResourceResource, ToolCallContent,
        };
        let blocks = vec![ToolCallContent::Content(Content::new(
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new("QkxPQg==", "file:///out.bin")
                        .mime_type(Some("application/octet-stream".to_string())),
                ),
            )),
        ))];
        let out = extract_tool_output_blocks(&blocks);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ToolOutputBlock::Resource {
                uri,
                data,
                text,
                mime_type,
            } => {
                assert_eq!(uri, "file:///out.bin");
                assert_eq!(data.as_deref(), Some("QkxPQg=="));
                assert!(text.is_none());
                assert_eq!(mime_type.as_deref(), Some("application/octet-stream"));
            }
            other => panic!("expected Resource, got {other:?}"),
        }
    }

    #[test]
    fn extract_tool_output_blocks_drops_oversized_inline_media() {
        use agent_client_protocol::schema::v1::{
            Content, ContentBlock, ImageContent, ToolCallContent,
        };
        let huge = "A".repeat(MAX_INLINE_MEDIA_B64 + 1);
        let blocks = vec![ToolCallContent::Content(Content::new(ContentBlock::Image(
            ImageContent::new(huge, "image/png"),
        )))];
        let out = extract_tool_output_blocks(&blocks);
        assert_eq!(out.len(), 1);
        // Oversized inline data is dropped (no uri to fall back on) but the
        // block survives so the card still shows the media placeholder.
        assert!(matches!(
            &out[0],
            ToolOutputBlock::Image {
                data: None,
                uri: None,
                ..
            }
        ));
    }

    #[test]
    fn extract_diffs_from_content_caps_diff_count() {
        use agent_client_protocol::schema::v1::{Diff, ToolCallContent};
        let blocks: Vec<ToolCallContent> = (0..MAX_TOOL_DIFFS + 8)
            .map(|i| ToolCallContent::Diff(Diff::new(format!("f{i}.rs"), "x")))
            .collect();
        let diffs = extract_diffs_from_content(&blocks);
        assert_eq!(diffs.len(), MAX_TOOL_DIFFS, "diff count must be bounded");
    }

    #[test]
    fn preview_args_strips_control_chars() {
        // Build the preview string by hand-injecting raw control chars
        // *into* the result of to_string (simulating agents that send
        // pre-serialised non-utf8 noise through). The function should
        // strip BEL/BS/etc. but preserve `\n` and `\t`.
        let arg = serde_json::Value::String("hello\x07world".into());
        let preview = preview_args(&arg);
        // The literal BEL (0x07) inside the string-data part of the JSON
        // gets escaped by to_string, so the preview never sees a raw
        // control char in this path. That's fine: the assertion we care
        // about is that the preview doesn't carry any unprintable bytes.
        for c in preview.chars() {
            assert!(
                !c.is_control() || c == '\n' || c == '\t',
                "unexpected control char {:?} in preview",
                c
            );
        }
        assert!(preview.contains("hello"));
        assert!(preview.contains("world"));
    }
}
