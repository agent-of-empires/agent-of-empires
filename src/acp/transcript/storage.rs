use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    ops::Deref,
};

use indexmap::IndexMap;

use super::reducer::ToolLifecycle;
use super::{DiffCommentsPayload, TranscriptDelta, TranscriptRow, UnfinishedToolOutput};
use crate::acp::{
    elicitations::ElicitationAnswer,
    state::{DiffComment, DiffPreview, MemoryRecall, PayloadCollection, ToolCall, ToolOutputBlock},
};
use crate::daemon::PromptAttachmentRef;

pub(crate) type ToolOf<B> =
    ToolCall<<B as Backend>::Text, <B as Backend>::Recall, <B as Backend>::Diffs>;
pub(crate) type InputTool<'a, B> = ToolCall<
    <B as Backend>::Input<'a>,
    <B as Backend>::RecallInput<'a>,
    <B as Backend>::DiffsInput<'a>,
>;
pub(crate) type RowOf<B> = TranscriptRow<
    <B as Backend>::Text,
    ToolOf<B>,
    <B as Backend>::Output,
    <B as Backend>::Attachments,
    <B as Backend>::Comments,
    <B as Backend>::Answers,
    <B as Backend>::PromptIds,
>;
pub(crate) type DeltaOf<B> = TranscriptDelta<
    <B as Backend>::Text,
    ToolOf<B>,
    <B as Backend>::Output,
    <B as Backend>::Attachments,
    <B as Backend>::Comments,
    <B as Backend>::Answers,
    <B as Backend>::PromptIds,
>;

pub(crate) enum Part<'a, T> {
    Literal(&'a str),
    Text(T),
    Number(u64),
}

pub(crate) trait TextWriter<B: Backend>: Sized {
    fn literal(&mut self, value: &str) -> Result<(), B::Error>;
    fn text<'a>(&mut self, value: B::Input<'a>) -> Result<(), B::Error>;
    fn number(&mut self, value: u64) -> Result<(), B::Error>;
    fn finish(self) -> Result<B::Text, B::Error>;
}

pub(crate) trait Rows<B: Backend> {
    type Read<'a>: Deref<Target = RowOf<B>>
    where
        Self: 'a;
    fn len(&self) -> u64;
    fn read(&self, index: u64) -> Result<Self::Read<'_>, B::Error>;
    fn update<R>(
        &mut self,
        index: u64,
        operation: impl FnOnce(&mut RowOf<B>) -> Result<R, B::Error>,
    ) -> Result<R, B::Error>;
    fn push(&mut self, row: &RowOf<B>) -> Result<(), B::Error>;
    fn retain(
        &mut self,
        keep: impl FnMut(&RowOf<B>) -> Result<bool, B::Error>,
    ) -> Result<(), B::Error>;
}

pub(crate) trait Set<B: Backend> {
    fn contains<'a>(&self, value: B::Input<'a>) -> Result<bool, B::Error>;
    fn insert(&mut self, value: B::Text) -> Result<(), B::Error>;
    fn remove<'a>(&mut self, value: B::Input<'a>) -> Result<bool, B::Error>;
    fn clear(&mut self) -> Result<(), B::Error>;
}

pub(crate) trait Positions<B: Backend> {
    fn get<'a>(&self, value: B::Input<'a>) -> Result<Option<u64>, B::Error>;
    fn insert(&mut self, value: B::Text, index: u64) -> Result<(), B::Error>;
    fn remove<'a>(&mut self, value: B::Input<'a>) -> Result<(), B::Error>;
    fn clear(&mut self) -> Result<(), B::Error>;
}

pub(crate) trait Tools<B: Backend>: Sized {
    type Read<'a>: Deref<Target = ToolLifecycle<B::Text>>
    where
        Self: 'a;
    type Key<'a>: Deref<Target = B::Text>
    where
        Self: 'a;
    fn get<'a>(&self, value: B::Input<'a>) -> Result<Option<(u64, Self::Read<'_>)>, B::Error>;
    fn next(
        &self,
        after: Option<u64>,
    ) -> Result<Option<(u64, Self::Key<'_>, Self::Read<'_>)>, B::Error>;
    fn update<R>(
        &mut self,
        cursor: u64,
        operation: impl FnOnce(&mut ToolLifecycle<B::Text>) -> Result<R, B::Error>,
    ) -> Result<R, B::Error>;
    fn insert(&mut self, id: B::Text, value: ToolLifecycle<B::Text>) -> Result<(), B::Error>;
    fn promote(&mut self, cursor: u64) -> Result<(), B::Error>;
    fn into_iter(self)
        -> impl Iterator<Item = Result<(B::Text, ToolLifecycle<B::Text>), B::Error>>;
}

pub(crate) trait Outputs<B: Backend>: Sized {
    fn push(&mut self, output: UnfinishedToolOutput<B::Text>) -> Result<(), B::Error>;
    fn retain(
        &mut self,
        keep: impl FnMut(&UnfinishedToolOutput<B::Text>) -> Result<bool, B::Error>,
    ) -> Result<(), B::Error>;
}

pub(crate) trait Changes<B: Backend>: Sized {
    fn is_empty(&self) -> bool;
    fn push(&mut self, delta: DeltaOf<B>) -> Result<(), B::Error>;
    fn into_iter(self) -> impl Iterator<Item = Result<DeltaOf<B>, B::Error>>;
}

pub(crate) trait Backend: Sized + 'static {
    type Error;
    type Text: Clone;
    type Input<'a>: Copy;
    type Recall: Clone;
    type RecallInput<'a>: Copy;
    type Diffs: Clone + Default + PayloadCollection;
    type DiffsInput<'a>: Copy + PayloadCollection;
    type Output: Clone + Default + PayloadCollection;
    type OutputInput<'a>: Copy + PayloadCollection;
    type Attachments: Clone + Default + PayloadCollection;
    type AttachmentsInput<'a>: Copy + PayloadCollection;
    type Comments: Clone;
    type CommentsInput<'a>: Copy + PayloadCollection;
    type Answers: Clone + Default + PayloadCollection;
    type AnswersInput<'a>: Copy + PayloadCollection;
    type PromptIds: Clone + Default + PayloadCollection;
    type PromptIdsInput<'a>: Copy + PayloadCollection;
    type Writer<'a>: TextWriter<Self>
    where
        Self: 'a;
    type Rows: Rows<Self>;
    type Set: Set<Self>;
    type Positions: Positions<Self>;
    type Tools: Tools<Self>;
    type Outputs: Outputs<Self>;
    type Changes: Changes<Self>;

    fn rows(&self) -> Result<Self::Rows, Self::Error>;
    fn set(&self) -> Result<Self::Set, Self::Error>;
    fn positions(&self) -> Result<Self::Positions, Self::Error>;
    fn tools(&self) -> Result<Self::Tools, Self::Error>;
    fn outputs(&self) -> Result<Self::Outputs, Self::Error>;
    fn changes(&self) -> Self::Changes;
    fn empty_text(&self) -> Self::Text;
    fn text_len(&self, value: &Self::Text) -> u64;
    fn input_len<'a>(&self, value: Self::Input<'a>) -> u64;
    fn borrow<'a>(&self, value: &'a Self::Text) -> Self::Input<'a>;
    fn retain<'a>(&self, value: Self::Input<'a>) -> Result<Self::Text, Self::Error>;
    fn replace<'a>(
        &self,
        target: &mut Self::Text,
        value: Self::Input<'a>,
    ) -> Result<(), Self::Error>;
    fn equal<'a>(&self, left: &Self::Text, right: Self::Input<'a>) -> Result<bool, Self::Error>;
    fn literal_equal<'a>(&self, value: Self::Input<'a>, literal: &str)
        -> Result<bool, Self::Error>;
    fn trim_empty<'a>(&self, value: Self::Input<'a>) -> Result<bool, Self::Error>;
    fn trim<'a>(&self, value: Self::Input<'a>) -> Result<Self::Input<'a>, Self::Error>;
    fn starts_with<'a>(&self, value: Self::Input<'a>, prefix: &str) -> Result<bool, Self::Error>;
    fn ends_with<'a>(&self, value: Self::Input<'a>, suffix: &str) -> Result<bool, Self::Error>;
    fn contains<'a>(&self, value: Self::Input<'a>, needle: &str) -> Result<bool, Self::Error>;
    fn writer(&self, capacity_hint: u64) -> Self::Writer<'_>;
    fn borrow_recall<'a>(&self, value: &'a Self::Recall) -> Self::RecallInput<'a>;
    fn borrow_diffs<'a>(&self, value: &'a Self::Diffs) -> Self::DiffsInput<'a>;
    fn retain_recall<'a>(&self, value: Self::RecallInput<'a>) -> Result<Self::Recall, Self::Error>;
    fn retain_diffs<'a>(&self, value: Self::DiffsInput<'a>) -> Result<Self::Diffs, Self::Error>;
    fn retain_output<'a>(&self, value: Self::OutputInput<'a>) -> Result<Self::Output, Self::Error>;
    fn retain_attachments<'a>(
        &self,
        value: Self::AttachmentsInput<'a>,
    ) -> Result<Self::Attachments, Self::Error>;
    fn retain_answers<'a>(
        &self,
        value: Self::AnswersInput<'a>,
    ) -> Result<Self::Answers, Self::Error>;
    fn retain_prompt_ids<'a>(
        &self,
        value: Self::PromptIdsInput<'a>,
    ) -> Result<Self::PromptIds, Self::Error>;
    fn comments<'a>(
        &self,
        intro: Self::Input<'a>,
        outro: Self::Input<'a>,
        multi_repo: bool,
        comments: Self::CommentsInput<'a>,
    ) -> Result<Self::Comments, Self::Error>;
    fn answer_capacity<'a>(&self, value: Self::AnswersInput<'a>) -> u64;
    fn answer_pairs<'a>(
        &self,
        value: Self::AnswersInput<'a>,
    ) -> impl Iterator<Item = Result<(Self::Input<'a>, Self::Input<'a>), Self::Error>>;

    fn text<'a>(&self, parts: &[Part<'a, Self::Input<'a>>]) -> Result<Self::Text, Self::Error> {
        if let [Part::Text(value)] = parts {
            return self.retain(*value);
        }
        if let [Part::Literal("")] = parts {
            return Ok(self.empty_text());
        }
        let capacity = parts.iter().fold(0u64, |sum, part| {
            sum.saturating_add(match part {
                Part::Literal(value) => value.len() as u64,
                Part::Text(value) => self.input_len(*value),
                Part::Number(value) => u64::from(value.checked_ilog10().unwrap_or(0)) + 1,
            })
        });
        let mut writer = self.writer(capacity);
        for part in parts {
            match part {
                Part::Literal(value) => writer.literal(value)?,
                Part::Text(value) => writer.text(*value)?,
                Part::Number(value) => writer.number(*value)?,
            }
        }
        writer.finish()
    }

    fn tool_input<'a>(&self, value: &'a ToolOf<Self>) -> InputTool<'a, Self> {
        ToolCall {
            id: self.borrow(&value.id),
            name: self.borrow(&value.name),
            kind: self.borrow(&value.kind),
            args_preview: self.borrow(&value.args_preview),
            prompt: value.prompt.as_ref().map(|value| self.borrow(value)),
            started_at: value.started_at,
            parent_tool_call_id: value
                .parent_tool_call_id
                .as_ref()
                .map(|value| self.borrow(value)),
            memory_recall: value
                .memory_recall
                .as_ref()
                .map(|value| self.borrow_recall(value)),
            diffs: self.borrow_diffs(&value.diffs),
        }
    }

    fn retain_tool<'a>(&self, value: InputTool<'a, Self>) -> Result<ToolOf<Self>, Self::Error> {
        Ok(ToolCall {
            id: self.retain(value.id)?,
            name: self.retain(value.name)?,
            kind: self.retain(value.kind)?,
            args_preview: self.retain(value.args_preview)?,
            prompt: value.prompt.map(|value| self.retain(value)).transpose()?,
            started_at: value.started_at,
            parent_tool_call_id: value
                .parent_tool_call_id
                .map(|value| self.retain(value))
                .transpose()?,
            memory_recall: value
                .memory_recall
                .map(|value| self.retain_recall(value))
                .transpose()?,
            diffs: self.retain_diffs(value.diffs)?,
        })
    }

    fn merge_tool<'a>(
        &self,
        prev: &ToolOf<Self>,
        value: InputTool<'a, Self>,
    ) -> Result<ToolOf<Self>, Self::Error> {
        Ok(ToolCall {
            id: self.retain(value.id)?,
            name: if self.input_len(value.name) != 0 {
                self.retain(value.name)?
            } else {
                prev.name.clone()
            },
            kind: if self.input_len(value.kind) != 0 && !self.literal_equal(value.kind, "other")? {
                self.retain(value.kind)?
            } else {
                prev.kind.clone()
            },
            args_preview: if !self.trim_empty(value.args_preview)? {
                self.retain(value.args_preview)?
            } else {
                prev.args_preview.clone()
            },
            prompt: match value.prompt {
                Some(prompt) => Some(self.retain(prompt)?),
                None if self.contains(value.args_preview, r#""prompt""#)? => None,
                None => prev.prompt.clone(),
            },
            started_at: value.started_at.max(prev.started_at),
            parent_tool_call_id: match value.parent_tool_call_id {
                Some(id) => Some(self.retain(id)?),
                None => prev.parent_tool_call_id.clone(),
            },
            memory_recall: match value.memory_recall {
                Some(recall) => Some(self.retain_recall(recall)?),
                None => prev.memory_recall.clone(),
            },
            diffs: if !value.diffs.is_empty() {
                self.retain_diffs(value.diffs)?
            } else {
                prev.diffs.clone()
            },
        })
    }

    fn answers_text<'a>(&self, value: Self::AnswersInput<'a>) -> Result<Self::Text, Self::Error> {
        let mut writer = self.writer(self.answer_capacity(value));
        let mut first = true;
        for pair in self.answer_pairs(value) {
            let (question, answer) = pair?;
            if !first {
                writer.literal("\n")?;
            }
            writer.text(question)?;
            writer.literal(": ")?;
            writer.text(answer)?;
            first = false;
        }
        writer.finish()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Memory;
pub(crate) struct MemoryWriter(String);
impl TextWriter<Memory> for MemoryWriter {
    fn literal(&mut self, value: &str) -> Result<(), Infallible> {
        self.0.push_str(value);
        Ok(())
    }
    fn text(&mut self, value: &str) -> Result<(), Infallible> {
        self.literal(value)
    }
    fn number(&mut self, value: u64) -> Result<(), Infallible> {
        use std::fmt::Write;
        write!(&mut self.0, "{value}").expect("String formatting is infallible");
        Ok(())
    }
    fn finish(self) -> Result<String, Infallible> {
        Ok(self.0)
    }
}
impl Backend for Memory {
    type Error = Infallible;
    type Text = String;
    type Input<'a> = &'a str;
    type Recall = MemoryRecall;
    type RecallInput<'a> = &'a MemoryRecall;
    type Diffs = Vec<DiffPreview>;
    type DiffsInput<'a> = &'a [DiffPreview];
    type Output = Vec<ToolOutputBlock>;
    type OutputInput<'a> = &'a [ToolOutputBlock];
    type Attachments = Vec<PromptAttachmentRef>;
    type AttachmentsInput<'a> = &'a [PromptAttachmentRef];
    type Comments = DiffCommentsPayload;
    type CommentsInput<'a> = &'a [DiffComment];
    type Answers = Vec<ElicitationAnswer>;
    type AnswersInput<'a> = &'a [ElicitationAnswer];
    type PromptIds = Vec<String>;
    type PromptIdsInput<'a> = &'a [String];
    type Writer<'a> = MemoryWriter;
    type Rows = Vec<TranscriptRow>;
    type Set = HashSet<String>;
    type Positions = HashMap<String, u64>;
    type Tools = IndexMap<String, ToolLifecycle<String>>;
    type Outputs = Vec<UnfinishedToolOutput>;
    type Changes = Vec<TranscriptDelta>;
    fn rows(&self) -> Result<Self::Rows, Infallible> {
        Ok(Vec::new())
    }
    fn set(&self) -> Result<Self::Set, Infallible> {
        Ok(HashSet::new())
    }
    fn positions(&self) -> Result<Self::Positions, Infallible> {
        Ok(HashMap::new())
    }
    fn tools(&self) -> Result<Self::Tools, Infallible> {
        Ok(IndexMap::new())
    }
    fn outputs(&self) -> Result<Self::Outputs, Infallible> {
        Ok(Vec::new())
    }
    fn changes(&self) -> Self::Changes {
        Vec::new()
    }
    fn empty_text(&self) -> String {
        String::new()
    }
    fn text_len(&self, value: &String) -> u64 {
        value.len() as u64
    }
    fn input_len(&self, value: &str) -> u64 {
        value.len() as u64
    }
    fn borrow<'a>(&self, value: &'a String) -> &'a str {
        value
    }
    fn retain(&self, value: &str) -> Result<String, Infallible> {
        Ok(value.to_owned())
    }
    fn replace(&self, target: &mut String, value: &str) -> Result<(), Infallible> {
        target.clear();
        target.push_str(value);
        Ok(())
    }
    fn equal(&self, left: &String, right: &str) -> Result<bool, Infallible> {
        Ok(left == right)
    }
    fn literal_equal(&self, value: &str, literal: &str) -> Result<bool, Infallible> {
        Ok(value == literal)
    }
    fn trim_empty(&self, value: &str) -> Result<bool, Infallible> {
        Ok(value.trim().is_empty())
    }
    fn trim<'a>(&self, value: Self::Input<'a>) -> Result<Self::Input<'a>, Self::Error> {
        Ok(value.trim())
    }
    fn starts_with(&self, value: &str, prefix: &str) -> Result<bool, Infallible> {
        Ok(value.starts_with(prefix))
    }
    fn ends_with(&self, value: &str, suffix: &str) -> Result<bool, Infallible> {
        Ok(value.ends_with(suffix))
    }
    fn contains(&self, value: &str, needle: &str) -> Result<bool, Infallible> {
        Ok(value.contains(needle))
    }
    fn writer(&self, capacity: u64) -> MemoryWriter {
        MemoryWriter(String::with_capacity(
            usize::try_from(capacity).expect("text capacity overflow"),
        ))
    }
    fn borrow_recall<'a>(&self, value: &'a MemoryRecall) -> &'a MemoryRecall {
        value
    }
    fn borrow_diffs<'a>(&self, value: &'a Vec<DiffPreview>) -> &'a [DiffPreview] {
        value
    }
    fn retain_recall(&self, value: &MemoryRecall) -> Result<MemoryRecall, Infallible> {
        Ok(value.clone())
    }
    fn retain_diffs(&self, value: &[DiffPreview]) -> Result<Vec<DiffPreview>, Infallible> {
        Ok(value.to_vec())
    }
    fn retain_output(&self, value: &[ToolOutputBlock]) -> Result<Vec<ToolOutputBlock>, Infallible> {
        Ok(value.to_vec())
    }
    fn retain_prompt_ids(&self, value: &[String]) -> Result<Vec<String>, Infallible> {
        Ok(value.to_vec())
    }
    fn retain_attachments(
        &self,
        value: &[PromptAttachmentRef],
    ) -> Result<Vec<PromptAttachmentRef>, Infallible> {
        Ok(value.to_vec())
    }
    fn retain_answers(
        &self,
        value: &[ElicitationAnswer],
    ) -> Result<Vec<ElicitationAnswer>, Infallible> {
        Ok(value.to_vec())
    }
    fn comments<'a>(
        &self,
        intro: &'a str,
        outro: &'a str,
        multi_repo: bool,
        comments: &'a [DiffComment],
    ) -> Result<DiffCommentsPayload, Infallible> {
        Ok(DiffCommentsPayload {
            intro: intro.to_owned(),
            outro: outro.to_owned(),
            is_multi_repo: multi_repo,
            comments: comments.to_vec(),
        })
    }
    fn answer_capacity(&self, value: &[ElicitationAnswer]) -> u64 {
        value
            .iter()
            .fold(value.len().saturating_sub(1) as u64, |sum, answer| {
                sum.saturating_add(answer.question.len() as u64)
                    .saturating_add(2)
                    .saturating_add(answer.answer.len() as u64)
            })
    }
    fn answer_pairs<'a>(
        &self,
        value: Self::AnswersInput<'a>,
    ) -> impl Iterator<Item = Result<(Self::Input<'a>, Self::Input<'a>), Self::Error>> {
        value
            .iter()
            .map(|answer| Ok((answer.question.as_str(), answer.answer.as_str())))
    }
}

impl Rows<Memory> for Vec<TranscriptRow> {
    type Read<'a> = &'a TranscriptRow;
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }
    fn read(&self, index: u64) -> Result<Self::Read<'_>, Infallible> {
        Ok(&self[index as usize])
    }
    fn update<R>(
        &mut self,
        index: u64,
        operation: impl FnOnce(&mut TranscriptRow) -> Result<R, Infallible>,
    ) -> Result<R, Infallible> {
        operation(&mut self[index as usize])
    }
    fn push(&mut self, row: &TranscriptRow) -> Result<(), Infallible> {
        Vec::push(self, row.clone());
        Ok(())
    }
    fn retain(
        &mut self,
        mut keep: impl FnMut(&TranscriptRow) -> Result<bool, Infallible>,
    ) -> Result<(), Infallible> {
        Vec::retain(self, |row| match keep(row) {
            Ok(value) => value,
            Err(error) => match error {},
        });
        Ok(())
    }
}
impl Set<Memory> for HashSet<String> {
    fn contains(&self, value: &str) -> Result<bool, Infallible> {
        Ok(HashSet::contains(self, value))
    }
    fn insert(&mut self, value: String) -> Result<(), Infallible> {
        HashSet::insert(self, value);
        Ok(())
    }
    fn remove(&mut self, value: &str) -> Result<bool, Infallible> {
        Ok(HashSet::remove(self, value))
    }
    fn clear(&mut self) -> Result<(), Infallible> {
        HashSet::clear(self);
        Ok(())
    }
}
impl Positions<Memory> for HashMap<String, u64> {
    fn get(&self, value: &str) -> Result<Option<u64>, Infallible> {
        Ok(HashMap::get(self, value).copied())
    }
    fn insert(&mut self, value: String, index: u64) -> Result<(), Infallible> {
        HashMap::insert(self, value, index);
        Ok(())
    }
    fn remove(&mut self, value: &str) -> Result<(), Infallible> {
        HashMap::remove(self, value);
        Ok(())
    }
    fn clear(&mut self) -> Result<(), Infallible> {
        HashMap::clear(self);
        Ok(())
    }
}
impl Tools<Memory> for IndexMap<String, ToolLifecycle<String>> {
    type Read<'a> = &'a ToolLifecycle<String>;
    type Key<'a> = &'a String;
    fn get(&self, value: &str) -> Result<Option<(u64, Self::Read<'_>)>, Infallible> {
        Ok(self
            .get_full(value)
            .map(|(index, _, value)| (index as u64, value)))
    }
    fn next(
        &self,
        after: Option<u64>,
    ) -> Result<Option<(u64, Self::Key<'_>, Self::Read<'_>)>, Infallible> {
        let index = after.map_or(0, |index| index as usize + 1);
        Ok(self
            .get_index(index)
            .map(|(key, value)| (index as u64, key, value)))
    }
    fn update<R>(
        &mut self,
        cursor: u64,
        operation: impl FnOnce(&mut ToolLifecycle<String>) -> Result<R, Infallible>,
    ) -> Result<R, Infallible> {
        operation(self.get_index_mut(cursor as usize).expect("tool cursor").1)
    }
    fn insert(&mut self, id: String, value: ToolLifecycle<String>) -> Result<(), Infallible> {
        IndexMap::insert(self, id, value);
        Ok(())
    }
    fn promote(&mut self, cursor: u64) -> Result<(), Infallible> {
        self.move_index(cursor as usize, IndexMap::len(self) - 1);
        Ok(())
    }
    fn into_iter(
        self,
    ) -> impl Iterator<Item = Result<(String, ToolLifecycle<String>), Infallible>> {
        IntoIterator::into_iter(self).map(Ok)
    }
}
impl Outputs<Memory> for Vec<UnfinishedToolOutput> {
    fn push(&mut self, value: UnfinishedToolOutput) -> Result<(), Infallible> {
        Vec::push(self, value);
        Ok(())
    }
    fn retain(
        &mut self,
        mut keep: impl FnMut(&UnfinishedToolOutput) -> Result<bool, Infallible>,
    ) -> Result<(), Infallible> {
        Vec::retain(self, |value| match keep(value) {
            Ok(value) => value,
            Err(error) => match error {},
        });
        Ok(())
    }
}
impl Changes<Memory> for Vec<TranscriptDelta> {
    fn is_empty(&self) -> bool {
        Vec::is_empty(self)
    }
    fn push(&mut self, delta: TranscriptDelta) -> Result<(), Infallible> {
        Vec::push(self, delta);
        Ok(())
    }
    fn into_iter(self) -> impl Iterator<Item = Result<TranscriptDelta, Infallible>> {
        IntoIterator::into_iter(self).map(Ok)
    }
}
