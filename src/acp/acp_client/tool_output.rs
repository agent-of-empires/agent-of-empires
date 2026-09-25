//! Reading a tool call's content: text, media and resource blocks, memory
//! recalls, and diffs, each under a size cap.

use crate::acp::state::{DiffPreview, Event, MemoryRecall, ToolOutputBlock};
use agent_client_protocol::schema::v1::ContentBlock;

pub(super) fn raw_event<T: serde::Serialize>(value: &T) -> Event {
    Event::RawAgentUpdate {
        payload: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
    }
}

/// Drives the per-tool renderer dispatch on the web side.
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

/// A missing field and an explicit JSON `null` both mean "no args", so the UI
/// renders an empty state rather than the literal "null" `preview_args` would
/// produce for `Value::Null`. Gemini ships argless tool calls this way (#1713).
pub(super) fn preview_optional_args(raw: Option<&serde_json::Value>) -> String {
    match raw {
        Some(value) if !value.is_null() => preview_args(value),
        _ => String::new(),
    }
}

/// The textual portion of a tool call's `content`, which is all the renderer
/// fallback can display. Diffs are bridged by `extract_diffs_from_content`.
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

/// Inline media is persisted and reshipped on every WS replay, so past this
/// cap only the placeholder/uri survives. ~3 MiB of bytes, well above a
/// typical screenshot.
pub(super) const MAX_INLINE_MEDIA_B64: usize = 4 * 1024 * 1024;

/// The renderable block list, keeping the media payloads
/// `extract_tool_content_text` drops. An embedded terminal surfaces as a text
/// placeholder; the structured view does not own ACP terminals. Empty when
/// every block is text or diff, which the `content` text path already renders
/// (#1818).
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
                        // Keep the capped bytes so a blob with no fetchable
                        // uri is still recoverable as a download.
                        EmbeddedResourceResource::BlobResourceContents(b) => {
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

/// The `memory_recall` shape claude-agent-acp routes through the tool channel:
/// `_meta.claudeCode.toolName == "memory_recall"` plus `locations` (recall) or
/// `content` (synthesize). Callers gate this on
/// `AgentProfile::supports_memory_recall_tool` so an agent that merely shares
/// the field shapes cannot trip the classifier.
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
    let synthesized_text = (mode == "synthesize")
        .then(|| extract_tool_content_text(content))
        .filter(|text| !text.is_empty());
    Some(MemoryRecall {
        mode,
        paths,
        synthesized_text,
    })
}

/// Per side (old/new). The card previews ~20 lines, but the untrimmed text is
/// persisted and reshipped on every WS replay frame.
pub(super) const MAX_DIFF_TEXT_BYTES: usize = 16 * 1024;

/// Per tool call, so one patch cannot grow an event unbounded.
pub(super) const MAX_TOOL_DIFFS: usize = 16;

/// Truncates on a char boundary, with a sentinel so the cut reads as
/// intentional rather than as a corrupt diff.
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

/// Codex routes `apply_patch` edits through `ToolCallContent::Diff`, one block
/// per touched file, instead of the legacy `old_string`/`new_string` raw_input
/// keys, so the edit card reads its path and preview from here (#1721).
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

/// claude-agent-acp emits no `ToolCallContent::Diff` for `Write`; the new
/// content appears only in `_meta.claudeCode.toolResponse`, which would
/// otherwise fall through as an inert `RawAgentUpdate` and leave the edit card
/// bodyless. `create` carries no `oldContent`, so its diff is against an empty
/// file. `None` for anything else, which keeps the passthrough.
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
    use agent_client_protocol::schema::v1::{
        AudioContent, BlobResourceContents, Content, ContentBlock, Diff, EmbeddedResource,
        EmbeddedResourceResource, ImageContent, ResourceLink, TextResourceContents,
        ToolCallContent,
    };

    /// #1713: a missing or null raw_input previews as empty, not "null".
    #[test]
    fn preview_args_is_empty_for_missing_or_null_and_capped() {
        assert_eq!(preview_optional_args(None), "");
        assert_eq!(preview_optional_args(Some(&serde_json::Value::Null)), "");
        let obj = serde_json::json!({ "command": "ls" });
        assert_eq!(preview_optional_args(Some(&obj)), r#"{"command":"ls"}"#);

        let preview = preview_args(&serde_json::Value::String("x".repeat(20_000)));
        assert!(preview.len() <= 16 * 1024 + 32);
        assert!(preview.contains("[truncated]"));
    }

    #[test]
    fn extract_text_and_diffs_from_content() {
        let text = [
            ToolCallContent::Content(Content::new("stdout line 1")),
            ToolCallContent::Content(Content::new("stdout line 2")),
        ];
        assert_eq!(
            extract_tool_content_text(&text),
            "stdout line 1\nstdout line 2"
        );
        // Empty rather than absent: the reducer falls back to the status word.
        assert_eq!(extract_tool_content_text(&[]), "");

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

        // `create` diffs against an empty file, `update` carries `oldContent`,
        // and anything else falls through to the `RawAgentUpdate` passthrough.
        {
            fn meta(
                response: serde_json::Value,
            ) -> Option<serde_json::Map<String, serde_json::Value>> {
                serde_json::json!({ "claudeCode": { "toolResponse": response } })
                    .as_object()
                    .cloned()
            }

            let create = write_diff_from_meta(&meta(serde_json::json!({
                "type": "create",
                "filePath": "/repo/src/new.rs",
                "content": "fn main() {}\n",
            })))
            .expect("create synthesizes a diff");
            assert_eq!(create.len(), 1);
            assert_eq!(create[0].path, "/repo/src/new.rs");
            assert_eq!(create[0].old_text, None);
            assert_eq!(create[0].new_text.as_deref(), Some("fn main() {}\n"));

            let update = write_diff_from_meta(&meta(serde_json::json!({
                "type": "update",
                "filePath": "/repo/src/existing.rs",
                "content": "new body",
                "oldContent": "old body",
            })))
            .expect("update synthesizes a diff");
            assert_eq!(update[0].old_text.as_deref(), Some("old body"));
            assert_eq!(update[0].new_text.as_deref(), Some("new body"));

            assert!(write_diff_from_meta(&None).is_none());
            assert!(
                write_diff_from_meta(&meta(serde_json::json!({ "status": "async_launched" })))
                    .is_none()
            );
        }
    }

    #[test]
    fn extract_diffs_from_content_caps_text_and_count() {
        let huge = "x".repeat(MAX_DIFF_TEXT_BYTES + 4096);
        let blocks = vec![ToolCallContent::Diff(
            Diff::new("src/big.rs", huge.clone()).old_text(huge),
        )];
        let diffs = extract_diffs_from_content(&blocks);
        assert_eq!(diffs.len(), 1);
        for side in [&diffs[0].new_text, &diffs[0].old_text] {
            let text = side.as_deref().expect("both sides present");
            assert!(text.len() < MAX_DIFF_TEXT_BYTES + 64, "{}", text.len());
            assert!(text.contains("[truncated]"));
        }

        let blocks: Vec<ToolCallContent> = (0..MAX_TOOL_DIFFS + 8)
            .map(|i| ToolCallContent::Diff(Diff::new(format!("f{i}.rs"), "x")))
            .collect();
        assert_eq!(
            extract_diffs_from_content(&blocks).len(),
            MAX_TOOL_DIFFS,
            "diff count must be bounded"
        );
    }

    #[test]
    fn extract_tool_output_blocks_cases() {
        let content = |block: ContentBlock| ToolCallContent::Content(Content::new(block));
        // The `content` string path already renders pure text.
        let text_only = [ToolCallContent::Content(Content::new("just text"))];
        assert!(extract_tool_output_blocks(&text_only).is_empty());

        let blocks = vec![
            ToolCallContent::Content(Content::new("a caption")),
            content(ContentBlock::Image(
                ImageContent::new("BASE64IMG", "image/png").uri("file:///shot.png".to_string()),
            )),
            content(ContentBlock::Audio(AudioContent::new(
                "BASE64AUDIO",
                "audio/wav",
            ))),
            content(ContentBlock::ResourceLink(ResourceLink::new(
                "report.pdf",
                "file:///report.pdf",
            ))),
            content(ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                    "inline body",
                    "file:///note.txt",
                )),
            ))),
            // #1818 review: a blob resource keeps its inline bytes so it stays
            // recoverable as a download.
            content(ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new("QkxPQg==", "file:///out.bin")
                        .mime_type(Some("application/octet-stream".to_string())),
                ),
            ))),
            // Oversized inline data is dropped (no uri to fall back on) but the
            // block survives so the card still shows the media placeholder.
            content(ContentBlock::Image(ImageContent::new(
                "A".repeat(MAX_INLINE_MEDIA_B64 + 1),
                "image/png",
            ))),
        ];
        let out = extract_tool_output_blocks(&blocks);
        assert_eq!(out.len(), 7, "all blocks preserved in order: {out:?}");
        assert!(matches!(&out[0], ToolOutputBlock::Text { text } if text == "a caption"));
        assert!(matches!(
            &out[1],
            ToolOutputBlock::Image { mime_type, data: Some(data), uri: Some(uri) }
                if mime_type == "image/png" && data == "BASE64IMG" && uri == "file:///shot.png"
        ));
        assert!(
            matches!(&out[2], ToolOutputBlock::Audio { mime_type, .. } if mime_type == "audio/wav")
        );
        assert!(
            matches!(&out[3], ToolOutputBlock::ResourceLink { name, uri, .. } if name == "report.pdf" && uri == "file:///report.pdf")
        );
        assert!(
            matches!(&out[4], ToolOutputBlock::Resource { text: Some(t), .. } if t == "inline body")
        );
        assert!(matches!(
            &out[5],
            ToolOutputBlock::Resource { uri, data: Some(data), text: None, mime_type: Some(mime) }
                if uri == "file:///out.bin" && data == "QkxPQg==" && mime == "application/octet-stream"
        ));
        assert!(matches!(
            &out[6],
            ToolOutputBlock::Image {
                data: None,
                uri: None,
                ..
            }
        ));
    }
}
