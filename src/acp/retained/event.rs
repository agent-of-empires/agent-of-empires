use std::io;

use chrono::{DateTime, Utc};
use pulldown_cmark::disk::TextRef;

use super::json::{Kind, ValueRef};
use super::{invalid, Context, Disk, List};
use crate::acp::{
    state::{Event, ToolCall},
    transcript::event::View,
};

struct Object<'a> {
    context: &'a Context,
    value: ValueRef,
}
impl<'a> Object<'a> {
    fn new(context: &'a Context, value: ValueRef) -> io::Result<Self> {
        if context.arena.node(&context.store, value)?.kind != Kind::Object {
            return Err(invalid("event object expected"));
        }
        Ok(Self { context, value })
    }
    fn required(&self, name: &str) -> io::Result<ValueRef> {
        self.context
            .property(self.value, name)?
            .ok_or_else(|| invalid("required event field missing"))
    }
    fn optional(&self, name: &str) -> io::Result<Option<ValueRef>> {
        self.context
            .property(self.value, name)?
            .map(|value| {
                Ok(
                    (self.context.arena.node(&self.context.store, value)?.kind != Kind::Null)
                        .then_some(value),
                )
            })
            .transpose()
            .map(Option::flatten)
    }
    fn text(&self, name: &str) -> io::Result<TextRef> {
        self.context.text(self.required(name)?)
    }
    fn optional_text(&self, name: &str) -> io::Result<Option<TextRef>> {
        self.optional(name)?
            .map(|value| self.context.text(value))
            .transpose()
    }
    fn default_text(&self, name: &str) -> io::Result<TextRef> {
        self.context
            .property(self.value, name)?
            .map(|value| self.context.text(value))
            .transpose()
            .map(|value| value.unwrap_or(self.context.empty))
    }
    fn boolean(&self, name: &str, default: Option<bool>) -> io::Result<bool> {
        match self.context.property(self.value, name)? {
            Some(value) => self.context.arena.boolean(&self.context.store, value),
            None => default.ok_or_else(|| invalid("required event flag missing")),
        }
    }
    fn list(&self, name: &str) -> io::Result<List> {
        self.context
            .property(self.value, name)?
            .map(|value| self.context.list(value))
            .transpose()
            .map(Option::unwrap_or_default)
    }
    fn optional_list(&self, name: &str) -> io::Result<Option<List>> {
        self.optional(name)?
            .map(|value| self.context.list(value))
            .transpose()
    }
    fn date(&self, name: &str) -> io::Result<DateTime<Utc>> {
        super::date::read(self.context, self.text(name)?)
    }
    fn optional_date(&self, name: &str) -> io::Result<Option<DateTime<Utc>>> {
        self.optional_text(name)?
            .map(|value| super::date::read(self.context, value))
            .transpose()
    }
}

impl Context {
    pub fn event(&self, value: ValueRef) -> io::Result<View<'_, Disk>> {
        let node = self.arena.node(&self.store, value)?;
        let (name, payload) = match node.kind {
            Kind::String => (node.value, None),
            Kind::Object if node.count == 1 => {
                let value = self
                    .arena
                    .children(&self.store, value)?
                    .next()
                    .ok_or_else(|| invalid("event variant missing"))??;
                let name = self
                    .arena
                    .node(&self.store, value)?
                    .key
                    .ok_or_else(|| invalid("event variant name missing"))?;
                (name, Some(value))
            }
            _ => return Err(invalid("externally tagged event expected")),
        };
        let is = |expected| super::json::text_is(&self.spool, name, expected);
        let object = || {
            Object::new(
                self,
                payload.ok_or_else(|| invalid("event payload missing"))?,
            )
        };
        if is("AgentMessageChunk")? || is("AgentThoughtChunk")? {
            let object = object()?;
            let text = object.text("text")?;
            let message_id = object.optional_text("message_id")?;
            return Ok(if is("AgentMessageChunk")? {
                View::AgentMessageChunk { text, message_id }
            } else {
                View::AgentThoughtChunk { text, message_id }
            });
        }
        if is("ToolCallStarted")? {
            let value = object()?.required("tool_call")?;
            let tool = Object::new(self, value)?;
            let diffs = tool.list("diffs")?;
            self.check_diffs(diffs)?;
            let memory_recall = tool.optional("memory_recall")?;
            if let Some(value) = memory_recall {
                let recall = Object::new(self, value)?;
                recall.text("mode")?;
                self.each(recall.list("paths")?, |value| self.text(value).map(|_| ()))?;
                recall.optional_text("synthesized_text")?;
            }
            return Ok(View::ToolCallStarted {
                tool_call: ToolCall {
                    id: tool.text("id")?,
                    name: tool.text("name")?,
                    kind: tool.default_text("kind")?,
                    args_preview: tool.text("args_preview")?,
                    prompt: tool.optional_text("prompt")?,
                    started_at: tool.date("started_at")?,
                    parent_tool_call_id: tool.optional_text("parent_tool_call_id")?,
                    memory_recall,
                    diffs,
                },
            });
        }
        if is("ToolCallCompleted")? {
            let object = object()?;
            let output = object.list("output")?;
            self.check_output(output)?;
            let completed_at = match self.property(object.value, "completed_at")? {
                Some(value) => super::date::read(self, self.text(value)?)?,
                None => Utc::now(),
            };
            return Ok(View::ToolCallCompleted {
                tool_call_id: object.text("tool_call_id")?,
                is_error: object.boolean("is_error", None)?,
                content: object.default_text("content")?,
                output,
                completed_at,
                async_subagent: object.boolean("async_subagent", Some(false))?,
            });
        }
        if is("ToolCallContent")? {
            let object = object()?;
            return Ok(View::ToolCallContent {
                tool_call_id: object.text("tool_call_id")?,
                content: object.text("content")?,
            });
        }
        if is("ToolCallUpdated")? {
            let object = object()?;
            let diffs = object.optional_list("diffs")?;
            if let Some(diffs) = diffs {
                self.check_diffs(diffs)?;
            }
            return Ok(View::ToolCallUpdated {
                tool_call_id: object.text("tool_call_id")?,
                title: object.optional_text("title")?,
                args_preview: object.optional_text("args_preview")?,
                prompt: object.optional_text("prompt")?,
                started_at: object.optional_date("started_at")?,
                diffs,
            });
        }
        if is("UserPromptSent")? {
            let object = object()?;
            let attachments = object.list("attachments")?;
            self.each(attachments, |value| {
                let (id, mime_type, name, size, kind) =
                    if self.arena.node(&self.store, value)?.kind == Kind::Array {
                        let mut fields = self.arena.children(&self.store, value)?;
                        let mut next = || {
                            fields
                                .next()
                                .ok_or_else(|| invalid("attachment field missing"))?
                        };
                        let id = next()?;
                        let kind = next()?;
                        let mime_type = next()?;
                        let name = Some(next()?);
                        let size = next()?;
                        let values = (id, mime_type, name, size, kind);
                        if fields.next().is_some() {
                            return Err(invalid("too many attachment fields"));
                        }
                        values
                    } else {
                        let attachment = Object::new(self, value)?;
                        (
                            attachment.required("id")?,
                            attachment.required("mime_type")?,
                            attachment.optional("name")?,
                            attachment.required("size")?,
                            attachment.required("kind")?,
                        )
                    };
                self.text(id)?;
                self.text(mime_type)?;
                if let Some(name) = name {
                    if self.arena.node(&self.store, name)?.kind != Kind::Null {
                        self.text(name)?;
                    }
                }
                self.arena.unsigned(&self.store, &self.spool, size)?;
                let kind = self.text(kind)?;
                if !super::json::text_is(&self.spool, kind, "image")?
                    && !super::json::text_is(&self.spool, kind, "audio")?
                    && !super::json::text_is(&self.spool, kind, "resource")?
                {
                    return Err(invalid("unknown attachment kind"));
                }
                Ok(())
            })?;
            let merged_prompt_ids = object.list("merged_prompt_ids")?;
            self.each(merged_prompt_ids, |value| self.text(value).map(|_| ()))?;
            return Ok(View::UserPromptSent {
                text: object.text("text")?,
                attachments,
                prompt_id: object.optional_text("prompt_id")?,
                merged_prompt_ids,
                synthesized: object.boolean("synthesized", Some(false))?,
                origin_plugin: object.optional_text("origin_plugin")?,
            });
        }
        if is("UserDiffCommentsPrompt")? {
            let object = object()?;
            let comments = object.list("comments")?;
            self.each(comments, |value| {
                let comment = Object::new(self, value)?;
                for name in [
                    "id",
                    "filePath",
                    "side",
                    "body",
                    "capturedSnippet",
                    "createdAt",
                ] {
                    comment.text(name)?;
                }
                for name in ["repoName", "language", "updatedAt"] {
                    comment.optional_text(name)?;
                }
                for name in ["startLine", "endLine"] {
                    let line =
                        self.arena
                            .unsigned(&self.store, &self.spool, comment.required(name)?)?;
                    u32::try_from(line).map_err(|_| invalid("diff line number overflow"))?;
                }
                Ok(())
            })?;
            return Ok(View::UserDiffCommentsPrompt {
                intro: object.text("intro")?,
                outro: object.text("outro")?,
                is_multi_repo: object.boolean("isMultiRepo", None)?,
                comments,
                assembled_markdown: object.text("assembledMarkdown")?,
            });
        }
        if is("ElicitationRequested")? {
            let elicitation = Object::new(self, object()?.required("elicitation")?)?;
            self.check_elicitation(&elicitation)?;
            return Ok(View::ElicitationRequested {
                tool_call_id: elicitation.optional_text("tool_call_id")?,
            });
        }
        if is("ElicitationResolved")? {
            let object = object()?;
            if !known::<crate::acp::elicitations::ElicitationOutcome>(
                self,
                object.text("outcome")?,
            )? {
                return Err(invalid("invalid elicitation outcome"));
            }
            let answers = object.list("answers")?;
            self.each(answers, |value| {
                let answer = Object::new(self, value)?;
                answer.text("question")?;
                answer.text("answer")?;
                Ok(())
            })?;
            return Ok(View::ElicitationResolved {
                nonce: object.text("nonce")?,
                answers,
            });
        }
        if is("Stopped")? {
            object()?.text("reason")?;
            return Ok(View::Stopped);
        }
        if is("AgentStartupError")? {
            return Ok(View::AgentStartupError {
                message: object()?.text("message")?,
            });
        }
        if is("IncompatibleAgent")? {
            return Ok(View::IncompatibleAgent);
        }
        if is("AgentSwitched")? {
            let object = object()?;
            return Ok(View::AgentSwitched {
                from: object.text("from")?,
                to: object.text("to")?,
                reason: object.text("reason")?,
            });
        }
        if is("ConversationCompactionSummary")? {
            let object = object()?;
            return Ok(View::ConversationCompactionSummary {
                compaction_id: object.text("compaction_id")?,
                text: object.text("text")?,
            });
        }
        if is("SessionContextReset")? {
            return Ok(View::SessionContextReset {
                reason: object()?.text("reason")?,
            });
        }
        if is("PromptRuntimeError")? {
            return Ok(View::PromptRuntimeError {
                message: object()?.text("message")?,
            });
        }
        if is("ModeSwitchFailed")? {
            let object = object()?;
            return Ok(View::ModeSwitchFailed {
                mode_id: object.text("mode_id")?,
                reason: object.text("reason")?,
            });
        }
        if is("SessionNotice")? {
            let object = object()?;
            return Ok(View::SessionNotice {
                severity: object.text("severity")?,
                title: object.text("title")?,
                description: object.optional_text("description")?,
                key: object.optional_text("key")?,
            });
        }
        if is("RateLimitAutoResumed")? {
            let object = object()?;
            return Ok(View::RateLimitAutoResumed {
                resets_at: object.date("resets_at")?,
                manual: object.boolean("manual", Some(false))?,
            });
        }
        if is("ConversationSummary")? {
            return Ok(View::ConversationSummary {
                text: object()?.text("text")?,
            });
        }
        if is("PromptCapabilities")? {
            return Ok(View::PromptCapabilities {
                steering: object()?.boolean("steering", Some(false))?,
            });
        }
        for (name, view) in [
            ("ThinkingStarted", View::ThinkingStarted),
            ("SessionCleared", View::SessionCleared),
            ("ConversationCompacted", View::ConversationCompacted),
        ] {
            if is(name)? {
                if let Some(payload) = payload {
                    if self.arena.node(&self.store, payload)?.kind != Kind::Null {
                        return Err(invalid("unit event payload expected"));
                    }
                }
                return Ok(view);
            }
        }
        if known::<Event>(self, name)? {
            Ok(View::Other)
        } else {
            Err(invalid("unknown retained event variant"))
        }
    }

    fn each(
        &self,
        list: List,
        mut operation: impl FnMut(ValueRef) -> io::Result<()>,
    ) -> io::Result<()> {
        if let Some(value) = self.check_list(list)?.value {
            for child in self.arena.children(&self.store, value)? {
                operation(child?)?;
            }
        }
        Ok(())
    }
    fn check_elicitation(&self, elicitation: &Object<'_>) -> io::Result<()> {
        elicitation.text("nonce")?;
        elicitation.text("message")?;
        elicitation.date("requested_at")?;
        for field in ["title", "description", "tool_call_id"] {
            elicitation.optional_text(field)?;
        }
        self.each(self.list(elicitation.required("questions")?)?, |value| {
            let question = Object::new(self, value)?;
            question.text("field_key")?;
            question.boolean("required", None)?;
            if !known::<crate::acp::elicitations::ElicitationFieldKind>(
                self,
                question.text("kind")?,
            )? {
                return Err(invalid("invalid elicitation field kind"));
            }
            for field in ["title", "description", "pattern", "format"] {
                question.optional_text(field)?;
            }
            self.each(self.list(question.required("options")?)?, |value| {
                let option = Object::new(self, value)?;
                option.text("value")?;
                option.text("label")?;
                option.optional_text("description")?;
                option.optional_text("preview")?;
                Ok(())
            })
        })?;
        if let Some(value) = elicitation.optional("resolved")? {
            let resolved = Object::new(self, value)?;
            resolved.date("resolved_at")?;
            if !known::<crate::acp::elicitations::ElicitationOutcome>(
                self,
                resolved.text("outcome")?,
            )? {
                return Err(invalid("invalid elicitation outcome"));
            }
        }
        Ok(())
    }

    fn check_diffs(&self, diffs: List) -> io::Result<()> {
        self.each(diffs, |value| {
            let diff = Object::new(self, value)?;
            diff.text("path")?;
            diff.optional_text("old_text")?;
            diff.optional_text("new_text")?;
            diff.date("created_at")?;
            Ok(())
        })
    }
    fn check_output(&self, output: List) -> io::Result<()> {
        self.each(output, |value| {
            let block = Object::new(self, value)?;
            let kind = block.text("kind")?;
            let is = |name| super::json::text_is(&self.spool, kind, name);
            if is("text")? {
                block.text("text")?;
            } else if is("image")? || is("audio")? {
                block.text("mime_type")?;
                block.optional_text("data")?;
                if is("image")? {
                    block.optional_text("uri")?;
                }
            } else if is("resource_link")? {
                block.text("uri")?;
                block.text("name")?;
                block.optional_text("mime_type")?;
            } else if is("resource")? {
                block.text("uri")?;
                for field in ["mime_type", "text", "data"] {
                    block.optional_text(field)?;
                }
            } else {
                return Err(invalid("unknown tool output kind"));
            }
            Ok(())
        })
    }
}

fn known<T: serde::de::DeserializeOwned>(context: &Context, name: TextRef) -> io::Result<bool> {
    crate::acp::transcript::event::matches_variant::<T>(|variants| {
        for variant in variants {
            if super::json::text_is(&context.spool, name, variant)? {
                return Ok(true);
            }
        }
        Ok(false)
    })
}
