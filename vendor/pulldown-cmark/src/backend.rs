use std::convert::Infallible;

use crate::parse::{
    FootnoteDefs, HeadingAttributes, InlineEl, Item, LinkDef, LinkStackEl, RefDefs,
};
use crate::storage::{Dictionary, Queues, Sequence, Set};
use crate::tree::{ListFlagStorage, Node, Tree, TreeIndex};
use crate::{Alignment, CowStr, Event, LinkType};

pub(crate) type LinkValue<'a, B> = (
    LinkType,
    <B as Backend>::Text<'a>,
    <B as Backend>::Text<'a>,
    <B as Backend>::Text<'a>,
);
pub(crate) type HeadingValue<'a, B> = HeadingAttributes<
    <B as Backend>::Text<'a>,
    <B as Backend>::Classes<'a>,
    <B as Backend>::Attributes<'a>,
>;
pub(crate) type ParsedEvent<'a, B> = Event<
    <B as Backend>::Text<'a>,
    <B as Backend>::Classes<'a>,
    <B as Backend>::Attributes<'a>,
    <B as Backend>::Alignments,
>;

pub(crate) trait Backend: Sized {
    type Error;
    type Nodes: Sequence<Node<Item>, Error = Self::Error> + ListFlagStorage;
    type Spine: Sequence<TreeIndex, Error = Self::Error>;
    type InlineItems: Sequence<InlineEl, Error = Self::Error>;
    type LinkItems: Sequence<LinkStackEl, Error = Self::Error>;
    type CodeQueues: Queues<usize, TreeIndex, Error = Self::Error>;
    type MathQueues: Queues<u8, (TreeIndex, bool, bool), Error = Self::Error>;
    type Input: crate::source::Source<Error = Self::Error> + ?Sized;
    type Text<'a>: Clone;
    type Buffer<'input>: crate::text::TextBuffer<'input, Self::Input, Value = Self::Text<'input>>;
    type Classes<'a>: Clone + Default;
    type Attributes<'a>: Clone + Default;
    type Alignments: Clone + Default;
    type CowValues<'a>: Sequence<Self::Text<'a>, Error = Self::Error>;
    type Columns: Sequence<Alignment, Error = Self::Error>;
    type AttributeRows<'a>: Sequence<(Self::Text<'a>, Option<Self::Text<'a>>), Error = Self::Error>;
    type LinkValues<'a>: Sequence<LinkValue<'a, Self>, Error = Self::Error>;
    type AlignmentValues: Sequence<Self::Alignments, Error = Self::Error>;
    type HeadingValues<'a>: Sequence<HeadingValue<'a, Self>, Error = Self::Error>;
    type ReferenceValues<'a>: Dictionary<
        Self::Text<'a>,
        LinkDef<Self::Text<'a>>,
        Error = Self::Error,
    >;
    type FootnoteValues<'a>: Set<Self::Text<'a>, Error = Self::Error>;

    fn tree(&self, input_bytes: usize)
        -> Result<Tree<Item, Self::Nodes, Self::Spine>, Self::Error>;
    fn inline_items(&self) -> Result<Self::InlineItems, Self::Error>;
    fn link_items(&self) -> Result<Self::LinkItems, Self::Error>;
    fn code_queues(&self) -> Result<Self::CodeQueues, Self::Error>;
    fn math_queues(&self) -> Result<Self::MathQueues, Self::Error>;
    fn cow_values<'a>(&self) -> Result<Self::CowValues<'a>, Self::Error>;
    fn columns(&self) -> Result<Self::Columns, Self::Error>;
    fn attribute_rows<'a>(&self) -> Result<Self::AttributeRows<'a>, Self::Error>;
    fn finish_classes<'a>(
        &self,
        rows: Self::CowValues<'a>,
    ) -> Result<Self::Classes<'a>, Self::Error>;
    fn finish_attributes<'a>(
        &self,
        rows: Self::AttributeRows<'a>,
    ) -> Result<Self::Attributes<'a>, Self::Error>;
    fn link_values<'a>(&self) -> Result<Self::LinkValues<'a>, Self::Error>;
    fn alignment_values(&self) -> Result<Self::AlignmentValues, Self::Error>;
    fn heading_values<'a>(&self) -> Result<Self::HeadingValues<'a>, Self::Error>;
    fn reference_values<'a>(&self) -> Result<Self::ReferenceValues<'a>, Self::Error>;
    fn footnote_values<'a>(&self) -> Result<Self::FootnoteValues<'a>, Self::Error>;
    fn store_text<'a>(&self, text: CowStr<'a>) -> Result<Self::Text<'a>, Self::Error>;
    fn empty_text<'a>(&self) -> Self::Text<'a>;
    fn text_len(&self, text: &Self::Text<'_>) -> u64;
    fn text_buffer<'input>(&self) -> Self::Buffer<'input>;
    fn text_source<'value, 'input>(
        &'value self,
        value: &'value Self::Text<'input>,
    ) -> Result<crate::source::Text<'value, Self::Input>, Self::Error>;
    fn own_text<'from, 'to>(text: Self::Text<'from>) -> Self::Text<'to>;
    fn slice_text<'input>(
        &self,
        text: Self::Text<'input>,
        range: std::ops::Range<usize>,
    ) -> Result<Self::Text<'input>, Self::Error>;

    fn trim_text<'input>(
        &self,
        text: Self::Text<'input>,
        predicate: impl Fn(char) -> bool,
    ) -> Result<Self::Text<'input>, Self::Error> {
        let range = {
            let source = self.text_source(&text)?;
            let trimmed = source.trim(predicate)?;
            trimmed.start - source.start..trimmed.end - source.start
        };
        self.slice_text(text, range)
    }

    fn speculate_text<T>(
        &self,
        operation: impl FnOnce() -> Result<Option<T>, Self::Error>,
    ) -> Result<Option<T>, Self::Error> {
        operation()
    }

    fn lookahead(
        &self,
        operation: impl FnOnce() -> Result<bool, Self::Error>,
    ) -> Result<bool, Self::Error> {
        operation()
    }

    fn source_text<'input>(
        &self,
        text: crate::source::Text<'input, Self::Input>,
    ) -> Result<Self::Text<'input>, Self::Error> {
        use crate::text::TextBuffer as _;

        let mut buffer = self.text_buffer();
        buffer.push_source(text)?;
        buffer.finish()
    }
    fn finish_columns(&self, alignment: Self::Columns) -> Result<Self::Alignments, Self::Error>;
    fn alignment_len(&self, alignment: &Self::Alignments) -> usize;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Memory;

impl Backend for Memory {
    type Error = Infallible;
    type Nodes = Vec<Node<Item>>;
    type Spine = Vec<TreeIndex>;
    type InlineItems = Vec<InlineEl>;
    type LinkItems = Vec<LinkStackEl>;
    type CodeQueues = std::collections::HashMap<usize, std::collections::VecDeque<TreeIndex>>;
    type MathQueues =
        std::collections::HashMap<u8, std::collections::VecDeque<(TreeIndex, bool, bool)>>;
    type Input = str;
    type Text<'a> = CowStr<'a>;
    type Buffer<'input> = crate::text::MemoryBuffer<'input>;
    type Classes<'a> = Vec<CowStr<'a>>;
    type Attributes<'a> = Vec<(CowStr<'a>, Option<CowStr<'a>>)>;
    type Alignments = Vec<Alignment>;
    type CowValues<'a> = Vec<CowStr<'a>>;
    type Columns = Vec<Alignment>;
    type AttributeRows<'a> = Vec<(CowStr<'a>, Option<CowStr<'a>>)>;
    type LinkValues<'a> = Vec<LinkValue<'a, Self>>;
    type AlignmentValues = Vec<Vec<Alignment>>;
    type HeadingValues<'a> = Vec<HeadingValue<'a, Self>>;
    type ReferenceValues<'a> = RefDefs<'a>;
    type FootnoteValues<'a> = FootnoteDefs<'a>;

    fn tree(&self, input_bytes: usize) -> Result<Tree<Item>, Infallible> {
        Ok(Tree::with_capacity((input_bytes / 32).max(128)))
    }

    fn inline_items(&self) -> Result<Self::InlineItems, Infallible> {
        Ok(Vec::new())
    }

    fn link_items(&self) -> Result<Self::LinkItems, Infallible> {
        Ok(Vec::new())
    }

    fn code_queues(&self) -> Result<Self::CodeQueues, Infallible> {
        Ok(Default::default())
    }

    fn math_queues(&self) -> Result<Self::MathQueues, Infallible> {
        Ok(Default::default())
    }

    fn cow_values<'a>(&self) -> Result<Self::CowValues<'a>, Infallible> {
        Ok(Vec::new())
    }

    fn columns(&self) -> Result<Self::Columns, Infallible> {
        Ok(Vec::new())
    }

    fn attribute_rows<'a>(&self) -> Result<Self::AttributeRows<'a>, Infallible> {
        Ok(Vec::new())
    }

    fn finish_classes<'a>(
        &self,
        rows: Self::CowValues<'a>,
    ) -> Result<Self::Classes<'a>, Infallible> {
        Ok(rows)
    }

    fn finish_attributes<'a>(
        &self,
        rows: Self::AttributeRows<'a>,
    ) -> Result<Self::Attributes<'a>, Infallible> {
        Ok(rows)
    }

    fn link_values<'a>(&self) -> Result<Self::LinkValues<'a>, Infallible> {
        Ok(Vec::with_capacity(128))
    }

    fn alignment_values(&self) -> Result<Self::AlignmentValues, Infallible> {
        Ok(Vec::new())
    }

    fn heading_values<'a>(&self) -> Result<Self::HeadingValues<'a>, Infallible> {
        Ok(Vec::new())
    }

    fn reference_values<'a>(&self) -> Result<Self::ReferenceValues<'a>, Infallible> {
        Ok(RefDefs::default())
    }

    fn footnote_values<'a>(&self) -> Result<Self::FootnoteValues<'a>, Infallible> {
        Ok(FootnoteDefs::default())
    }

    fn store_text<'a>(&self, text: CowStr<'a>) -> Result<Self::Text<'a>, Infallible> {
        Ok(text)
    }

    fn empty_text<'a>(&self) -> Self::Text<'a> {
        "".into()
    }

    fn text_len(&self, text: &Self::Text<'_>) -> u64 {
        text.len() as u64
    }

    fn text_buffer<'input>(&self) -> Self::Buffer<'input> {
        crate::text::MemoryBuffer::default()
    }

    fn text_source<'value, 'input>(
        &'value self,
        value: &'value Self::Text<'input>,
    ) -> Result<crate::source::Text<'value, str>, Infallible> {
        Ok(crate::source::Text::new(value.as_ref()))
    }

    fn own_text<'from, 'to>(text: Self::Text<'from>) -> Self::Text<'to> {
        text.into_static()
    }

    fn slice_text<'input>(
        &self,
        text: Self::Text<'input>,
        range: std::ops::Range<usize>,
    ) -> Result<Self::Text<'input>, Infallible> {
        assert!(range.start <= range.end && range.end <= text.len());
        if range.start == 0 && range.end == text.len() {
            return Ok(text);
        }
        Ok(match text {
            CowStr::Borrowed(text) => text[range].into(),
            CowStr::Inlined(text) => CowStr::from(&text[range]).into_static(),
            text => {
                let mut text = text.into_string();
                text.truncate(range.end);
                text.drain(..range.start);
                text.into()
            }
        })
    }

    fn finish_columns(&self, alignment: Self::Columns) -> Result<Self::Alignments, Infallible> {
        Ok(alignment)
    }

    fn alignment_len(&self, alignment: &Self::Alignments) -> usize {
        alignment.len()
    }
}

#[cfg(feature = "disk")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Disk<'a> {
    store: &'a crate::disk::Store,
    spool: &'a crate::disk::Spool,
}

#[cfg(feature = "disk")]
impl<'a> Disk<'a> {
    pub(crate) fn new(store: &'a crate::disk::Store, spool: &'a crate::disk::Spool) -> Self {
        Self { store, spool }
    }
}

#[cfg(feature = "disk")]
impl<'a> Backend for Disk<'a> {
    type Error = std::io::Error;
    type Nodes = crate::disk::Sequence<'a, Node<Item>>;
    type Spine = crate::disk::Sequence<'a, TreeIndex>;
    type InlineItems = crate::disk::Sequence<'a, InlineEl>;
    type LinkItems = crate::disk::Sequence<'a, LinkStackEl>;
    type CodeQueues = crate::disk::DiskQueues<'a, usize, TreeIndex>;
    type MathQueues = crate::disk::DiskQueues<'a, u8, (TreeIndex, bool, bool)>;
    type Input = crate::disk::Spool;
    type Text<'input> = crate::disk::TextRef;
    type Buffer<'input> = crate::text::DiskBuffer<'a>;
    type Classes<'input> = crate::disk::ArrayRef<crate::disk::TextRef>;
    type Attributes<'input> =
        crate::disk::ArrayRef<(crate::disk::TextRef, Option<crate::disk::TextRef>)>;
    type Alignments = crate::disk::ArrayRef<Alignment>;
    type CowValues<'input> = crate::disk::Sequence<'a, Self::Text<'input>>;
    type Columns = crate::disk::Sequence<'a, Alignment>;
    type AttributeRows<'input> =
        crate::disk::Sequence<'a, (Self::Text<'input>, Option<Self::Text<'input>>)>;
    type LinkValues<'input> = crate::disk::Sequence<'a, LinkValue<'input, Self>>;
    type AlignmentValues = crate::disk::Sequence<'a, Self::Alignments>;
    type HeadingValues<'input> = crate::disk::Sequence<'a, HeadingValue<'input, Self>>;
    type ReferenceValues<'input> =
        crate::disk::DiskLabels<'a, (crate::disk::TextRef, LinkDef<crate::disk::TextRef>)>;
    type FootnoteValues<'input> = crate::disk::DiskLabels<'a, crate::disk::TextRef>;

    fn tree(&self, _input_bytes: usize) -> std::io::Result<Tree<Item, Self::Nodes, Self::Spine>> {
        Tree::with_storage(
            crate::disk::Sequence::new(self.store)?,
            crate::disk::Sequence::new(self.store)?,
        )
    }

    fn inline_items(&self) -> std::io::Result<Self::InlineItems> {
        crate::disk::Sequence::new(self.store)
    }

    fn link_items(&self) -> std::io::Result<Self::LinkItems> {
        crate::disk::Sequence::new(self.store)
    }

    fn code_queues(&self) -> std::io::Result<Self::CodeQueues> {
        crate::disk::DiskQueues::new(self.store)
    }

    fn math_queues(&self) -> std::io::Result<Self::MathQueues> {
        crate::disk::DiskQueues::new(self.store)
    }

    fn cow_values<'input>(&self) -> std::io::Result<Self::CowValues<'input>> {
        crate::disk::Sequence::new(self.store)
    }

    fn link_values<'input>(&self) -> std::io::Result<Self::LinkValues<'input>> {
        crate::disk::Sequence::new(self.store)
    }

    fn alignment_values(&self) -> std::io::Result<Self::AlignmentValues> {
        crate::disk::Sequence::new(self.store)
    }

    fn columns(&self) -> std::io::Result<Self::Columns> {
        crate::disk::Sequence::new(self.store)
    }

    fn attribute_rows<'input>(&self) -> std::io::Result<Self::AttributeRows<'input>> {
        crate::disk::Sequence::new(self.store)
    }

    fn finish_classes<'input>(
        &self,
        rows: Self::CowValues<'input>,
    ) -> std::io::Result<Self::Classes<'input>> {
        rows.view()
    }

    fn finish_attributes<'input>(
        &self,
        rows: Self::AttributeRows<'input>,
    ) -> std::io::Result<Self::Attributes<'input>> {
        rows.view()
    }

    fn heading_values<'input>(&self) -> std::io::Result<Self::HeadingValues<'input>> {
        crate::disk::Sequence::new(self.store)
    }

    fn reference_values<'input>(&self) -> std::io::Result<Self::ReferenceValues<'input>> {
        crate::disk::DiskLabels::new(self.store, self.spool)
    }

    fn footnote_values<'input>(&self) -> std::io::Result<Self::FootnoteValues<'input>> {
        crate::disk::DiskLabels::new(self.store, self.spool)
    }

    fn store_text<'input>(&self, text: CowStr<'input>) -> std::io::Result<Self::Text<'input>> {
        if text.is_empty() {
            Ok(self.empty_text())
        } else {
            self.spool.append(text.as_bytes())
        }
    }

    fn empty_text<'input>(&self) -> Self::Text<'input> {
        self.spool.empty_reference()
    }

    fn text_len(&self, text: &Self::Text<'_>) -> u64 {
        text.len
    }

    fn text_buffer<'input>(&self) -> Self::Buffer<'input> {
        crate::text::DiskBuffer::new(self.spool)
    }

    fn text_source<'value, 'input>(
        &'value self,
        value: &'value Self::Text<'input>,
    ) -> std::io::Result<crate::source::Text<'value, Self::Input>> {
        let _: usize = crate::disk::ByteSource::len(self.spool)
            .try_into()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        self.spool.view(*value)?;
        Ok(crate::source::Text::new(self.spool)
            .slice(value.start as usize..(value.start + value.len) as usize))
    }

    fn own_text<'from, 'to>(text: Self::Text<'from>) -> Self::Text<'to> {
        text
    }

    fn slice_text<'input>(
        &self,
        text: Self::Text<'input>,
        range: std::ops::Range<usize>,
    ) -> std::io::Result<Self::Text<'input>> {
        text.subrange(range.start as u64..range.end as u64)
    }

    fn speculate_text<T>(
        &self,
        operation: impl FnOnce() -> std::io::Result<Option<T>>,
    ) -> std::io::Result<Option<T>> {
        self.spool.with_speculation(operation)
    }

    fn lookahead(
        &self,
        operation: impl FnOnce() -> std::io::Result<bool>,
    ) -> std::io::Result<bool> {
        let mut matched = false;
        self.spool.with_speculation(|| {
            matched = operation()?;
            Ok(None::<()>)
        })?;
        Ok(matched)
    }

    fn finish_columns(&self, alignment: Self::Columns) -> std::io::Result<Self::Alignments> {
        alignment.view()
    }

    fn alignment_len(&self, alignment: &Self::Alignments) -> usize {
        alignment.len() as usize
    }
}
