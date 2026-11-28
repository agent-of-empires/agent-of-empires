// Copyright 2017 Google Inc. All rights reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

//! Tree-based two pass parser.

use core::cell::Cell;
use std::cmp::{max, min};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::iter::FusedIterator;
use std::num::NonZeroUsize;
use std::ops::Range;

use unicase::UniCase;

use crate::backend::{Backend, HeadingValue, LinkValue, Memory, ParsedEvent};
use crate::firstpass::run_first_pass;
use crate::linklabel::{scan_link_label_rest, FootnoteLabel, LinkLabel, ReferenceLabel};
use crate::scanners::*;
use crate::source::{Bytes, Text};
use crate::storage::{Dictionary, QueueCursor, Queues, Sequence, Set};
use crate::strings::CowStr;
use crate::text::TextBuffer;
use crate::tree::{ListFlagStorage, Node, Tree, TreeIndex};
use crate::{
    BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, LinkType, MetadataBlockKind, Options, Tag,
    TagEnd,
};

#[cfg(feature = "disk")]
mod record;

// Allowing arbitrary depth nested parentheses inside link destinations
// can create denial of service vulnerabilities if we're not careful.
// The simplest countermeasure is to limit their depth, which is
// explicitly allowed by the spec as long as the limit is at least 3:
// https://spec.commonmark.org/0.29/#link-destination
pub(crate) const LINK_MAX_NESTED_PARENS: usize = 32;

#[derive(Debug, Default, Clone)]
pub(crate) struct Item {
    pub start: usize,
    pub end: usize,
    pub body: ItemBody,
}

#[derive(Debug, PartialEq, Clone, Default)]
pub(crate) enum ItemBody {
    // These are possible inline items, need to be resolved in second pass.

    // repeats, can_open, can_close
    MaybeEmphasis(usize, bool, bool),
    // can_open, can_close, brace context
    MaybeMath(bool, bool, u8),
    // quote byte, can_open, can_close
    MaybeSmartQuote(u8, bool, bool),
    MaybeCode(usize, bool), // number of backticks, preceded by backslash
    MaybeHtml,
    MaybeLinkOpen,
    // bool indicates whether or not the preceding section could be a reference
    MaybeLinkClose(bool),
    MaybeImage,

    // These are inline items after resolution.
    Emphasis,
    Strong,
    Strikethrough,
    Superscript,
    Subscript,
    Math(CowIndex, bool), // true for display math
    Code(CowIndex),
    Link(LinkIndex),
    Image(LinkIndex),
    FootnoteReference(CowIndex),
    TaskListMarker(bool), // true for checked

    // These are also inline items.
    InlineHtml,
    OwnedInlineHtml(CowIndex),
    SynthesizeText(CowIndex),
    SynthesizeChar(char),
    Html,
    Text {
        backslash_escaped: bool,
    },
    SoftBreak,
    // true = is backlash
    HardBreak(bool),

    // Dummy node at the top of the tree - should not be used otherwise!
    #[default]
    Root,

    // These are block items.
    Paragraph,
    TightParagraph,
    Rule,
    Heading(HeadingLevel, Option<HeadingIndex>), // heading level
    FencedCodeBlock(CowIndex),
    IndentCodeBlock,
    HtmlBlock,
    BlockQuote(Option<BlockQuoteKind>),
    // is_tight|last_line empty, list character, list start index
    List(Cell<ListFlags>, u8, u64),
    ListItem(usize), // indent level
    FootnoteDefinition(CowIndex),
    MetadataBlock(MetadataBlockKind),

    // Definition lists
    DefinitionList(Cell<ListFlags>),
    // gets turned into either a paragraph or a definition list title,
    // depending on whether there's a definition after it
    MaybeDefinitionListTitle,
    DefinitionListTitle,
    DefinitionListDefinition(usize),

    // Tables
    Table(AlignmentIndex),
    TableHead,
    TableRow,
    TableCell,
}

impl ItemBody {
    fn is_maybe_inline(&self) -> bool {
        use ItemBody::*;
        matches!(
            *self,
            MaybeEmphasis(..)
                | MaybeMath(..)
                | MaybeSmartQuote(..)
                | MaybeCode(..)
                | MaybeHtml
                | MaybeLinkOpen
                | MaybeLinkClose(..)
                | MaybeImage
        )
    }
    fn is_inline(&self) -> bool {
        use ItemBody::*;
        matches!(
            *self,
            MaybeEmphasis(..)
                | MaybeMath(..)
                | MaybeSmartQuote(..)
                | MaybeCode(..)
                | MaybeHtml
                | MaybeLinkOpen
                | MaybeLinkClose(..)
                | MaybeImage
                | Emphasis
                | Strong
                | Strikethrough
                | Math(..)
                | Code(..)
                | Link(..)
                | Image(..)
                | FootnoteReference(..)
                | TaskListMarker(..)
                | InlineHtml
                | OwnedInlineHtml(..)
                | SynthesizeText(..)
                | SynthesizeChar(..)
                | Html
                | Text { .. }
                | SoftBreak
                | HardBreak(..)
        )
    }
}

bitflags::bitflags! {
    /// Option struct containing flags for enabling extra features
    /// that are not part of the CommonMark spec.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub(crate) struct ListFlags: u8 {
        const IS_TIGHT = 1;
        const LAST_LINE_EMPTY = 1 << 1;
    }
}

#[derive(Debug)]
pub struct BrokenLink<Text> {
    pub span: std::ops::Range<usize>,
    pub link_type: LinkType,
    pub reference: Text,
}

/// Markdown event iterator.
pub struct Parser<'input, F = DefaultBrokenLinkCallback> {
    inner: Core<'input, F>,
}

/// Fallible Markdown parser with disk-backed nodes and event payloads.
#[cfg(feature = "disk")]
pub struct DiskParser<'input> {
    inner: Core<'input, DefaultBrokenLinkCallback, crate::backend::Disk<'input>>,
}

#[cfg(feature = "disk")]
impl std::fmt::Debug for DiskParser<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("DiskParser").finish_non_exhaustive()
    }
}

#[cfg(feature = "disk")]
impl<'input> DiskParser<'input> {
    /// Parse a UTF-8 source range. Payload references require the store and spool to remain alive.
    pub fn new(
        text: crate::disk::TextRef,
        options: Options,
        store: &'input crate::disk::Store,
        spool: &'input crate::disk::Spool,
    ) -> std::io::Result<Self> {
        spool.view(text)?;
        usize::try_from(crate::disk::ByteSource::len(spool)).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "source offset overflow")
        })?;
        let start = usize::try_from(text.start).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "source offset overflow")
        })?;
        let length = usize::try_from(text.len).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "source length overflow")
        })?;
        let text = Text::new(spool).slice(start..start + length);
        Ok(Self {
            inner: Core::with_backend(
                text,
                options,
                None,
                crate::backend::Disk::new(store, spool),
            )?,
        })
    }

    /// Yield an event and its source byte range.
    pub fn next_event(&mut self) -> std::io::Result<Option<(crate::disk::Event, Range<usize>)>> {
        self.inner.next_event()
    }
}

struct Core<'input, F, B: Backend = Memory> {
    text: Text<'input, B::Input>,
    options: Options,
    tree: Tree<Item, B::Nodes, B::Spine>,
    allocs: Allocations<'input, B>,
    broken_link_callback: Option<F>,
    html_scan_guard: HtmlScanGuard,

    // Bound repeated reference expansion: https://github.com/pulldown-cmark/pulldown-cmark/issues/844.
    link_ref_expansion_limit: u64,

    inline_stack: InlineStack<B::InlineItems>,
    link_stack: LinkStack<B::LinkItems>,
    wikilink_stack: LinkStack<B::LinkItems>,
    code_delims: CodeDelims<B::CodeQueues>,
    math_delims: MathDelims<B::MathQueues>,
}

impl<'input, F, B: Backend> std::fmt::Debug for Core<'input, F, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Only print the fields that have public types.
        f.debug_struct("Parser")
            .field("text", &self.text)
            .field("options", &self.options)
            .field(
                "broken_link_callback",
                &self.broken_link_callback.as_ref().map(|_| ..),
            )
            .finish()
    }
}

impl<F> std::fmt::Debug for Parser<'_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl<'a> BrokenLink<CowStr<'a>> {
    /// Own the reference text independently of the input.
    pub fn into_static(self) -> BrokenLink<CowStr<'static>> {
        BrokenLink {
            span: self.span,
            link_type: self.link_type,
            reference: self.reference.into_static(),
        }
    }
}

impl<'input> Parser<'input, DefaultBrokenLinkCallback> {
    /// Creates a new event iterator for a markdown string without any options enabled.
    pub fn new(text: &'input str) -> Self {
        Self::new_ext(text, Options::empty())
    }

    /// Creates a new event iterator for a markdown string with given options.
    pub fn new_ext(text: &'input str, options: Options) -> Self {
        Self::new_with_broken_link_callback(text, options, None)
    }
}

impl<'input, F: BrokenLinkCallback<CowStr<'input>>> Parser<'input, F> {
    /// Calls the callback for unresolved reference links.
    pub fn new_with_broken_link_callback(
        text: &'input str,
        options: Options,
        broken_link_callback: Option<F>,
    ) -> Self {
        let inner = match Core::with_backend(Text::new(text), options, broken_link_callback, Memory)
        {
            Ok(inner) => inner,
            Err(error) => match error {},
        };
        Self { inner }
    }

    /// Returns the document's reference definitions.
    pub fn reference_definitions(&self) -> &RefDefs<'_> {
        &self.inner.allocs.refdefs
    }

    /// Yields events and their source byte ranges.
    pub fn into_offset_iter(self) -> OffsetIter<'input, F> {
        OffsetIter { inner: self.inner }
    }
}

impl<'input, F: BrokenLinkCallback<B::Text<'input>>, B: Backend> Core<'input, F, B> {
    fn with_backend(
        text: Text<'input, B::Input>,
        options: Options,
        broken_link_callback: Option<F>,
        backend: B,
    ) -> Result<Self, B::Error> {
        let inline_stack = InlineStack::with_storage(backend.inline_items()?);
        let link_stack = LinkStack::with_storage(backend.link_items()?);
        let wikilink_stack = LinkStack::with_storage(backend.link_items()?);
        let code_delims = CodeDelims::with_storage(backend.code_queues()?);
        let math_delims = MathDelims::with_storage(backend.math_queues()?);
        let (mut tree, allocs) = run_first_pass(text, options, backend)?;
        tree.reset()?;
        let html_scan_guard = Default::default();
        Ok(Self {
            text,
            options,
            tree,
            allocs,
            broken_link_callback,
            inline_stack,
            link_stack,
            wikilink_stack,
            html_scan_guard,
            link_ref_expansion_limit: (text.len() as u64).max(100_000),
            code_delims,
            math_delims,
        })
    }
}

impl<'input, F: BrokenLinkCallback<B::Text<'input>>, B: Backend> Core<'input, F, B> {
    /// Bound reference expansion, including callback-provided destinations.
    fn fetch_link_type_url_title(
        &mut self,
        link_label: B::Text<'input>,
        span: Range<usize>,
        link_type: LinkType,
    ) -> Result<Option<(LinkType, B::Text<'input>, B::Text<'input>)>, B::Error> {
        if self.link_ref_expansion_limit == 0 {
            return Ok(None);
        }
        let resolved = match self.allocs.refdefs.lookup(&link_label)? {
            Some(definition) => Some((
                link_type,
                definition.dest,
                definition
                    .title
                    .unwrap_or_else(|| self.allocs.backend.empty_text()),
            )),
            None => self
                .broken_link_callback
                .as_mut()
                .and_then(|callback| {
                    callback.handle_broken_link(BrokenLink {
                        span,
                        link_type,
                        reference: link_label,
                    })
                })
                .map(|(url, title)| (link_type.to_unknown(), url, title)),
        };
        let Some((link_type, url, title)) = resolved else {
            return Ok(None);
        };
        let bytes = self
            .allocs
            .backend
            .text_len(&url)
            .saturating_add(self.allocs.backend.text_len(&title));
        self.link_ref_expansion_limit = self.link_ref_expansion_limit.saturating_sub(bytes);
        Ok(Some((link_type, url, title)))
    }

    /// Resolve inline markup for the current chain.
    fn handle_inline(&mut self) -> Result<(), B::Error> {
        self.handle_inline_pass1()?;
        self.handle_emphasis_and_hard_break()?;
        Ok(())
    }

    /// Handle inline HTML, code spans, and links.
    ///
    /// This function handles both inline HTML and code spans, because they have
    /// the same precedence. It also handles links, even though they have lower
    /// precedence, because the URL of links must not be processed.
    fn handle_inline_pass1(&mut self) -> Result<(), B::Error> {
        let mut cur = self.tree.cur();
        let mut prev = None;

        let block_end = self.tree.node(self.tree.peek_up()?.unwrap())?.item.end;
        let block_text = self.text.slice(..block_end);

        while let Some(mut cur_ix) = cur {
            match self.tree.body_snapshot(cur_ix)? {
                ItemBody::MaybeHtml => {
                    let next = self.tree.node(cur_ix)?.next;
                    let autolink = if let Some(next_ix) = next {
                        scan_autolink(block_text, self.tree.node(next_ix)?.item.start)?
                    } else {
                        None
                    };

                    if let Some((ix, uri, link_type)) = autolink {
                        let node = scan_nodes_to_ix(&self.tree, next, ix)?;
                        let text_start = self.tree.node(cur_ix)?.item.start + 1;
                        let text_node = self.tree.create_node(Item {
                            start: text_start,
                            end: ix - 1,
                            body: ItemBody::Text {
                                backslash_escaped: false,
                            },
                        })?;
                        let link_ix = self.allocs.allocate_link(
                            link_type,
                            self.allocs.backend.source_text(uri)?,
                            self.allocs.backend.empty_text(),
                            self.allocs.backend.empty_text(),
                        )?;
                        {
                            let value = ItemBody::Link(link_ix);
                            self.tree.update(cur_ix, |node| node.item.body = value)?;
                        };
                        {
                            let value = ix;
                            self.tree.update(cur_ix, |node| node.item.end = value)?;
                        };
                        {
                            let value = node;
                            self.tree.update(cur_ix, |node| node.next = value)?;
                        };
                        {
                            let value = Some(text_node);
                            self.tree.update(cur_ix, |node| node.child = value)?;
                        };
                        prev = cur;
                        cur = node;
                        if let Some(node_ix) = cur {
                            {
                                let value = max(self.tree.node(node_ix)?.item.start, ix);
                                self.tree.update(node_ix, |node| node.item.start = value)?;
                            };
                        }
                        continue;
                    } else {
                        let inline_html = match next {
                            Some(next_ix) => {
                                let start = self.tree.node(next_ix)?.item.start;
                                self.scan_inline_html(block_text, start)?
                            }
                            None => None,
                        };
                        if let Some((span, ix)) = inline_html {
                            let node = scan_nodes_to_ix(&self.tree, next, ix)?;
                            {
                                let value = match span {
                                    Some(span) => {
                                        ItemBody::OwnedInlineHtml(self.allocs.allocate_cow(span)?)
                                    }
                                    None => ItemBody::InlineHtml,
                                };
                                self.tree.update(cur_ix, |node| node.item.body = value)?;
                            };
                            {
                                let value = ix;
                                self.tree.update(cur_ix, |node| node.item.end = value)?;
                            };
                            {
                                let value = node;
                                self.tree.update(cur_ix, |node| node.next = value)?;
                            };
                            prev = cur;
                            cur = node;
                            if let Some(node_ix) = cur {
                                {
                                    let value = max(self.tree.node(node_ix)?.item.start, ix);
                                    self.tree.update(node_ix, |node| node.item.start = value)?;
                                };
                            }
                            continue;
                        }
                    }
                    {
                        let value = ItemBody::Text {
                            backslash_escaped: false,
                        };
                        self.tree.update(cur_ix, |node| node.item.body = value)?;
                    };
                }
                ItemBody::MaybeMath(can_open, _can_close, brace_context) => {
                    if !can_open {
                        {
                            let value = ItemBody::Text {
                                backslash_escaped: false,
                            };
                            self.tree.update(cur_ix, |node| node.item.body = value)?;
                        };
                        prev = cur;
                        cur = self.tree.node(cur_ix)?.next;
                        continue;
                    }
                    let is_display = self
                        .tree
                        .node(cur_ix)?
                        .next
                        .map(|next_ix| {
                            self.tree
                                .node(next_ix)
                                .map(|node| matches!(node.item.body, ItemBody::MaybeMath(..)))
                        })
                        .transpose()?
                        .unwrap_or(false);
                    let result = if self.math_delims.is_populated() {
                        // we have previously scanned all math environment delimiters,
                        // so we can reuse that work
                        self.math_delims
                            .find(&self.tree, cur_ix, is_display, brace_context)?
                    } else {
                        // we haven't previously scanned all math delimiters,
                        // so walk the AST
                        let mut scan = self.tree.node(cur_ix)?.next;
                        if is_display {
                            // a display delimiter, `$$`, is actually two delimiters
                            // skip the second one
                            scan = self.tree.node(scan.unwrap())?.next;
                        }
                        let mut invalid = false;
                        while let Some(scan_ix) = scan {
                            if let ItemBody::MaybeMath(_can_open, can_close, delim_brace_context) =
                                self.tree.node(scan_ix)?.item.body
                            {
                                let delim_is_display = self
                                    .tree
                                    .node(scan_ix)?
                                    .next
                                    .map(|next_ix| {
                                        self.tree.node(next_ix).map(|node| {
                                            matches!(node.item.body, ItemBody::MaybeMath(..))
                                        })
                                    })
                                    .transpose()?
                                    .unwrap_or(false);
                                if !invalid && delim_brace_context == brace_context {
                                    if (!is_display && can_close)
                                        || (is_display && delim_is_display)
                                    {
                                        // This will skip ahead past everything we
                                        // just inserted. Needed for correctness to
                                        // ensure that a new scan is done after this item.
                                        self.math_delims.clear()?;
                                        break;
                                    } else {
                                        // Math cannot contain $, so the current item
                                        // is invalid. Keep scanning to fill math_delims.
                                        invalid = true;
                                    }
                                }
                                self.math_delims.insert(
                                    delim_is_display,
                                    delim_brace_context,
                                    scan_ix,
                                    can_close,
                                )?;
                            }
                            scan = self.tree.node(scan_ix)?.next;
                        }
                        scan
                    };

                    if let Some(scan_ix) = result {
                        self.make_math_span(cur_ix, scan_ix)?;
                    } else {
                        {
                            let value = ItemBody::Text {
                                backslash_escaped: false,
                            };
                            self.tree.update(cur_ix, |node| node.item.body = value)?;
                        };
                    }
                }
                ItemBody::MaybeCode(mut search_count, preceded_by_backslash) => {
                    if preceded_by_backslash {
                        search_count -= 1;
                        if search_count == 0 {
                            {
                                let value = ItemBody::Text {
                                    backslash_escaped: false,
                                };
                                self.tree.update(cur_ix, |node| node.item.body = value)?;
                            };
                            prev = cur;
                            cur = self.tree.node(cur_ix)?.next;
                            continue;
                        }
                    }

                    if self.code_delims.is_populated() {
                        // we have previously scanned all codeblock delimiters,
                        // so we can reuse that work
                        if let Some(scan_ix) = self.code_delims.find(cur_ix, search_count)? {
                            self.make_code_span(cur_ix, scan_ix, preceded_by_backslash)?;
                        } else {
                            {
                                let value = ItemBody::Text {
                                    backslash_escaped: false,
                                };
                                self.tree.update(cur_ix, |node| node.item.body = value)?;
                            };
                        }
                    } else {
                        // we haven't previously scanned all codeblock delimiters,
                        // so walk the AST
                        let mut scan = if search_count > 0 {
                            self.tree.node(cur_ix)?.next
                        } else {
                            None
                        };
                        while let Some(scan_ix) = scan {
                            let code_count = match self.tree.node(scan_ix)?.item.body {
                                ItemBody::MaybeCode(count, _) => Some(count),
                                _ => None,
                            };
                            if let Some(delim_count) = code_count {
                                if search_count == delim_count {
                                    self.make_code_span(cur_ix, scan_ix, preceded_by_backslash)?;
                                    self.code_delims.clear()?;
                                    break;
                                } else {
                                    self.code_delims.insert(delim_count, scan_ix)?;
                                }
                            }
                            scan = self.tree.node(scan_ix)?.next;
                        }
                        if scan.is_none() {
                            {
                                let value = ItemBody::Text {
                                    backslash_escaped: false,
                                };
                                self.tree.update(cur_ix, |node| node.item.body = value)?;
                            };
                        }
                    }
                }
                ItemBody::MaybeLinkOpen => {
                    {
                        let value = ItemBody::Text {
                            backslash_escaped: false,
                        };
                        self.tree.update(cur_ix, |node| node.item.body = value)?;
                    };
                    let link_open_doubled = self
                        .tree
                        .node(cur_ix)?
                        .next
                        .map(|ix| {
                            self.tree
                                .node(ix)
                                .map(|node| node.item.body == ItemBody::MaybeLinkOpen)
                        })
                        .transpose()?
                        .unwrap_or(false);
                    if self.options.contains(Options::ENABLE_WIKILINKS) && link_open_doubled {
                        self.wikilink_stack.push(LinkStackEl {
                            node: cur_ix,
                            ty: LinkStackTy::Link,
                        })?;
                    }
                    self.link_stack.push(LinkStackEl {
                        node: cur_ix,
                        ty: LinkStackTy::Link,
                    })?;
                }
                ItemBody::MaybeImage => {
                    {
                        let value = ItemBody::Text {
                            backslash_escaped: false,
                        };
                        self.tree.update(cur_ix, |node| node.item.body = value)?;
                    };
                    let link_open_doubled = self
                        .tree
                        .node(cur_ix)?
                        .next
                        .map(|ix| {
                            self.tree
                                .node(ix)
                                .map(|node| node.item.body == ItemBody::MaybeLinkOpen)
                        })
                        .transpose()?
                        .unwrap_or(false);
                    if self.options.contains(Options::ENABLE_WIKILINKS) && link_open_doubled {
                        self.wikilink_stack.push(LinkStackEl {
                            node: cur_ix,
                            ty: LinkStackTy::Image,
                        })?;
                    }
                    self.link_stack.push(LinkStackEl {
                        node: cur_ix,
                        ty: LinkStackTy::Image,
                    })?;
                }
                ItemBody::MaybeLinkClose(could_be_ref) => {
                    {
                        let value = ItemBody::Text {
                            backslash_escaped: false,
                        };
                        self.tree.update(cur_ix, |node| node.item.body = value)?;
                    };
                    let tos_link = self.link_stack.pop()?;
                    if self.options.contains(Options::ENABLE_WIKILINKS)
                        && self
                            .tree
                            .node(cur_ix)?
                            .next
                            .map(|ix| {
                                self.tree.node(ix).map(|node| {
                                    matches!(node.item.body, ItemBody::MaybeLinkClose(..))
                                })
                            })
                            .transpose()?
                            .unwrap_or(false)
                    {
                        if let Some(node) = self.handle_wikilink(block_text, cur_ix, prev)? {
                            prev = Some(node);
                            cur = self.tree.node(node)?.next;
                            continue;
                        }
                    }
                    if let Some(tos) = tos_link {
                        if tos.ty != LinkStackTy::Image
                            && matches!(
                                self.tree.node(self.tree.peek_up()?.unwrap())?.item.body,
                                ItemBody::Link(..)
                            )
                        {
                            continue;
                        }
                        if tos.ty == LinkStackTy::Disabled {
                            continue;
                        }
                        let next = self.tree.node(cur_ix)?.next;
                        let label_end = self.tree.node(cur_ix)?.item.end;
                        if let Some((next_ix, url, title)) =
                            self.scan_inline_link(block_text, label_end, next)?
                        {
                            let next_node = scan_nodes_to_ix(&self.tree, next, next_ix)?;
                            if let Some(prev_ix) = prev {
                                {
                                    let value = None;
                                    self.tree.update(prev_ix, |node| node.next = value)?;
                                };
                            }
                            cur = Some(tos.node);
                            cur_ix = tos.node;
                            let link_ix = self.allocs.allocate_link(
                                LinkType::Inline,
                                url,
                                title,
                                self.allocs.backend.empty_text(),
                            )?;
                            {
                                let value = if tos.ty == LinkStackTy::Image {
                                    ItemBody::Image(link_ix)
                                } else {
                                    ItemBody::Link(link_ix)
                                };
                                self.tree.update(cur_ix, |node| node.item.body = value)?;
                            };
                            {
                                let value = self.tree.node(cur_ix)?.next;
                                self.tree.update(cur_ix, |node| node.child = value)?;
                            };
                            {
                                let value = next_node;
                                self.tree.update(cur_ix, |node| node.next = value)?;
                            };
                            {
                                let value = next_ix;
                                self.tree.update(cur_ix, |node| node.item.end = value)?;
                            };
                            if let Some(next_node_ix) = next_node {
                                {
                                    let value =
                                        max(self.tree.node(next_node_ix)?.item.start, next_ix);
                                    self.tree
                                        .update(next_node_ix, |node| node.item.start = value)?;
                                };
                            }

                            if tos.ty == LinkStackTy::Link {
                                self.disable_all_links();
                            }
                        } else {
                            // ok, so its not an inline link. maybe it is a reference
                            // to a defined link?
                            let scan_result = scan_reference(
                                &self.tree,
                                block_text,
                                next,
                                self.options,
                                &self.allocs.backend,
                            )?;
                            let (node_after_link, link_type) = match scan_result {
                                // [label][reference]
                                RefScan::LinkLabel(_, end_ix) => {
                                    // Toggle reference viability of the last closing bracket,
                                    // so that we can skip it on future iterations in case
                                    // it fails in this one. In particular, we won't call
                                    // the broken link callback twice on one reference.
                                    let reference_close_node = if let Some(node) =
                                        scan_nodes_to_ix(&self.tree, next, end_ix - 1)?
                                    {
                                        node
                                    } else {
                                        continue;
                                    };
                                    {
                                        let value = ItemBody::MaybeLinkClose(false);
                                        self.tree.update(reference_close_node, |node| {
                                            node.item.body = value
                                        })?;
                                    };
                                    let next_node = self.tree.node(reference_close_node)?.next;

                                    (next_node, LinkType::Reference)
                                }
                                // [reference][]
                                RefScan::Collapsed(next_node) => {
                                    // This reference has already been tried, and it's not
                                    // valid. Skip it.
                                    if !could_be_ref {
                                        continue;
                                    }
                                    (next_node, LinkType::Collapsed)
                                }
                                // [shortcut]
                                //
                                // [shortcut]: /blah
                                RefScan::Failed | RefScan::UnexpectedFootnote => {
                                    if !could_be_ref {
                                        continue;
                                    }
                                    (next, LinkType::Shortcut)
                                }
                            };

                            let label: Option<(ReferenceLabel<B::Text<'input>>, usize)> =
                                match scan_result {
                                    RefScan::LinkLabel(l, end_ix) => {
                                        Some((ReferenceLabel::Link(l), end_ix))
                                    }
                                    RefScan::Collapsed(..)
                                    | RefScan::Failed
                                    | RefScan::UnexpectedFootnote => {
                                        // No label? maybe it is a shortcut reference
                                        let label_start = self.tree.node(tos.node)?.item.end - 1;
                                        let label_end = self.tree.node(cur_ix)?.item.end;
                                        scan_link_label(
                                            &self.tree,
                                            self.text.slice(label_start..label_end),
                                            self.options,
                                            &self.allocs.backend,
                                        )?
                                        .map(|(ix, label)| (label, label_start + ix))
                                        .filter(|(_, end)| *end == label_end)
                                    }
                                };

                            if let Some((ReferenceLabel::Footnote(l), end)) = label {
                                if !self.options.has_gfm_footnotes()
                                    || self.allocs.footdefs.contains(&l)?
                                {
                                    let footref = self.allocs.allocate_cow(l)?;
                                    // If this came from a MaybeImage, then the `!` prefix
                                    // isn't part of the footnote reference.
                                    let footnote_ix = if tos.ty == LinkStackTy::Image {
                                        {
                                            let value = Some(cur_ix);
                                            self.tree.update(tos.node, |node| node.next = value)?;
                                        };
                                        {
                                            let value = None;
                                            self.tree
                                                .update(tos.node, |node| node.child = value)?;
                                        };
                                        {
                                            let value = ItemBody::SynthesizeChar('!');
                                            self.tree
                                                .update(tos.node, |node| node.item.body = value)?;
                                        };
                                        {
                                            let value = self.tree.node(tos.node)?.item.start + 1;
                                            self.tree
                                                .update(cur_ix, |node| node.item.start = value)?;
                                        };
                                        {
                                            let value = self.tree.node(tos.node)?.item.start + 1;
                                            self.tree
                                                .update(tos.node, |node| node.item.end = value)?;
                                        };
                                        cur_ix
                                    } else {
                                        tos.node
                                    };
                                    // use `next` instead of `node_after_link` because
                                    // node_after_link is calculated for a [collapsed][] link,
                                    // which footnotes don't support.
                                    {
                                        let value = next;
                                        self.tree.update(footnote_ix, |node| node.next = value)?;
                                    };
                                    {
                                        let value = None;
                                        self.tree.update(footnote_ix, |node| node.child = value)?;
                                    };
                                    {
                                        let value = ItemBody::FootnoteReference(footref);
                                        self.tree
                                            .update(footnote_ix, |node| node.item.body = value)?;
                                    };
                                    {
                                        let value = end;
                                        self.tree
                                            .update(footnote_ix, |node| node.item.end = value)?;
                                    };
                                    prev = Some(footnote_ix);
                                    cur = next;
                                    self.link_stack.clear()?;
                                    continue;
                                }
                            } else if let Some((ReferenceLabel::Link(link_label), end)) = label {
                                let id = link_label.clone();
                                let reference_start = self.tree.node(tos.node)?.item.start;
                                if let Some((def_link_type, url, title)) = self
                                    .fetch_link_type_url_title(
                                        link_label,
                                        reference_start..end,
                                        link_type,
                                    )?
                                {
                                    let link_ix =
                                        self.allocs.allocate_link(def_link_type, url, title, id)?;
                                    {
                                        let value = if tos.ty == LinkStackTy::Image {
                                            ItemBody::Image(link_ix)
                                        } else {
                                            ItemBody::Link(link_ix)
                                        };
                                        self.tree
                                            .update(tos.node, |node| node.item.body = value)?;
                                    };
                                    let label_node = self.tree.node(tos.node)?.next;

                                    // lets do some tree surgery to add the link to the tree
                                    // 1st: skip the label node and close node
                                    {
                                        let value = node_after_link;
                                        self.tree.update(tos.node, |node| node.next = value)?;
                                    };

                                    // then, if it exists, add the label node as a child to the link node
                                    if label_node != cur {
                                        {
                                            let value = label_node;
                                            self.tree
                                                .update(tos.node, |node| node.child = value)?;
                                        };

                                        // finally: disconnect list of children
                                        if let Some(prev_ix) = prev {
                                            {
                                                let value = None;
                                                self.tree
                                                    .update(prev_ix, |node| node.next = value)?;
                                            };
                                        }
                                    }

                                    {
                                        let value = end;
                                        self.tree.update(tos.node, |node| node.item.end = value)?;
                                    };

                                    // set up cur so next node will be node_after_link
                                    cur = Some(tos.node);
                                    cur_ix = tos.node;

                                    if tos.ty == LinkStackTy::Link {
                                        self.disable_all_links();
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            prev = cur;
            cur = self.tree.node(cur_ix)?.next;
        }
        self.link_stack.clear()?;
        self.wikilink_stack.clear()?;
        self.code_delims.clear()?;
        self.math_delims.clear()?;
        Ok(())
    }

    /// Handles a wikilink.
    ///
    /// This function may bail early in case the link is malformed, so this
    /// acts as a control flow guard. Returns the link node if a wikilink was
    /// found and created.
    fn handle_wikilink(
        &mut self,
        block_text: Text<'input, B::Input>,
        cur_ix: TreeIndex,
        prev: Option<TreeIndex>,
    ) -> Result<Option<TreeIndex>, B::Error> {
        let next_ix = self.tree.node(cur_ix)?.next.unwrap();
        // this is a wikilink closing delim, try popping from
        // the wikilink stack
        if let Some(tos) = self.wikilink_stack.pop()? {
            if tos.ty == LinkStackTy::Disabled {
                return Ok(None);
            }
            // fetches the beginning of the wikilink body
            let body_node = match self.tree.node(tos.node)?.next {
                Some(index) => self.tree.node(index)?.next,
                None => None,
            };
            let Some(body_node) = body_node else {
                // skip if no next node exists, like at end of input
                return Ok(None);
            };
            let start_ix = self.tree.node(body_node)?.item.start;
            let end_ix = self.tree.node(cur_ix)?.item.start;
            // bail early in case the link is malformed: on some inputs the
            // body can begin at or after the closing delimiter, which would
            // otherwise underflow the length below and invert the slice.
            if end_ix <= start_ix {
                return Ok(None);
            }
            let wikilink = match scan_wikilink_pipe(
                block_text,
                start_ix, // bounded by closing tag
                end_ix - start_ix,
            )? {
                Some((rest, wikitext)) => {
                    // bail early if the wikiname would be empty
                    if wikitext.is_empty() {
                        return Ok(None);
                    }
                    // [[WikiName|rest]]
                    if rest >= end_ix {
                        // Empty display text: the `|` is immediately followed
                        // by `]]`. Create a zero-length synthetic node so the
                        // anchor has no content, rather than accidentally
                        // capturing the closing `]` delimiter.
                        let body_node = self.tree.create_node(Item {
                            start: rest,
                            end: rest,
                            body: ItemBody::Text {
                                backslash_escaped: false,
                            },
                        })?;
                        Some((true, body_node, wikitext))
                    } else {
                        let body_node = scan_nodes_to_ix(&self.tree, Some(body_node), rest)?;
                        if let Some(body_node) = body_node {
                            // break node so passes can actually format
                            // the display text
                            {
                                let value = rest;
                                self.tree
                                    .update(body_node, |node| node.item.start = value)?;
                            };

                            Some((true, body_node, wikitext))
                        } else {
                            None
                        }
                    }
                }
                None => {
                    let wikitext = block_text.slice(start_ix..end_ix);
                    // bail early if the wikiname would be empty
                    if wikitext.is_empty() {
                        return Ok(None);
                    }
                    let body_node = self.tree.create_node(Item {
                        start: start_ix,
                        end: end_ix,
                        body: ItemBody::Text {
                            backslash_escaped: false,
                        },
                    })?;
                    Some((false, body_node, wikitext))
                }
            };

            if let Some((has_pothole, body_node, wikiname)) = wikilink {
                let link_ix = self.allocs.allocate_link(
                    LinkType::WikiLink { has_pothole },
                    self.allocs.backend.source_text(wikiname)?,
                    self.allocs.backend.empty_text(),
                    self.allocs.backend.empty_text(),
                )?;
                if let Some(prev_ix) = prev {
                    {
                        let value = None;
                        self.tree.update(prev_ix, |node| node.next = value)?;
                    };
                }
                if tos.ty == LinkStackTy::Image {
                    {
                        let value = ItemBody::Image(link_ix);
                        self.tree.update(tos.node, |node| node.item.body = value)?;
                    };
                } else {
                    {
                        let value = ItemBody::Link(link_ix);
                        self.tree.update(tos.node, |node| node.item.body = value)?;
                    };
                }
                {
                    let value = Some(body_node);
                    self.tree.update(tos.node, |node| node.child = value)?;
                };
                {
                    let value = self.tree.node(next_ix)?.next;
                    self.tree.update(tos.node, |node| node.next = value)?;
                };
                {
                    let value = end_ix + 2;
                    self.tree.update(tos.node, |node| node.item.end = value)?;
                };
                self.disable_all_links();
                // The deepest wikilink takes precedence. `disable_all_links`
                // leaves image openers alone, so drop the enclosing `![[` too,
                // or each of them would rescan this body as its own target.
                self.wikilink_stack.clear()?;
                return Ok(Some(tos.node));
            }
        }

        Ok(None)
    }

    fn handle_emphasis_and_hard_break(&mut self) -> Result<(), B::Error> {
        let mut prev = None;
        let mut prev_ix: TreeIndex;
        let mut cur = self.tree.cur();

        let mut single_quote_open: Option<TreeIndex> = None;
        let mut double_quote_open: bool = false;

        while let Some(mut cur_ix) = cur {
            match self.tree.body_snapshot(cur_ix)? {
                ItemBody::MaybeEmphasis(mut count, can_open, can_close) => {
                    let run_length = count;
                    let c = self.text.byte(self.tree.node(cur_ix)?.item.start)?;
                    let both = can_open && can_close;
                    if can_close {
                        while let Some(el) =
                            self.inline_stack
                                .find_match(&mut self.tree, c, run_length, both)?
                        {
                            // have a match!
                            if let Some(prev_ix) = prev {
                                {
                                    let value = None;
                                    self.tree.update(prev_ix, |node| node.next = value)?;
                                };
                            }
                            let match_count = min(count, el.count);
                            // start, end are tree node indices
                            let mut end = cur_ix - 1;
                            let mut start = el.start + el.count;

                            // work from the inside out
                            while start > el.start + el.count - match_count {
                                let inc = if start > el.start + el.count - match_count + 1 {
                                    2
                                } else {
                                    1
                                };
                                let ty = if c == b'~' {
                                    if inc == 2 {
                                        if self.options.contains(Options::ENABLE_STRIKETHROUGH) {
                                            ItemBody::Strikethrough
                                        } else {
                                            ItemBody::Text {
                                                backslash_escaped: false,
                                            }
                                        }
                                    } else {
                                        if self.options.contains(Options::ENABLE_SUBSCRIPT) {
                                            ItemBody::Subscript
                                        } else if self
                                            .options
                                            .contains(Options::ENABLE_STRIKETHROUGH)
                                        {
                                            ItemBody::Strikethrough
                                        } else {
                                            ItemBody::Text {
                                                backslash_escaped: false,
                                            }
                                        }
                                    }
                                } else if c == b'^' {
                                    if self.options.contains(Options::ENABLE_SUPERSCRIPT) {
                                        ItemBody::Superscript
                                    } else {
                                        ItemBody::Text {
                                            backslash_escaped: false,
                                        }
                                    }
                                } else if inc == 2 {
                                    ItemBody::Strong
                                } else {
                                    ItemBody::Emphasis
                                };

                                let root = start - inc;
                                end = end + inc;
                                {
                                    let value = ty;
                                    self.tree.update(root, |node| node.item.body = value)?;
                                };
                                {
                                    let value = self.tree.node(end)?.item.end;
                                    self.tree.update(root, |node| node.item.end = value)?;
                                };
                                {
                                    let value = Some(start);
                                    self.tree.update(root, |node| node.child = value)?;
                                };
                                {
                                    let value = None;
                                    self.tree.update(root, |node| node.next = value)?;
                                };
                                start = root;
                            }

                            // set next for top most emph level
                            prev_ix = el.start + el.count - match_count;
                            prev = Some(prev_ix);
                            cur = self.tree.node(cur_ix + match_count - 1)?.next;
                            {
                                let value = cur;
                                self.tree.update(prev_ix, |node| node.next = value)?;
                            };

                            if el.count > match_count {
                                self.inline_stack.push(InlineEl {
                                    start: el.start,
                                    count: el.count - match_count,
                                    run_length: el.run_length,
                                    c: el.c,
                                    both: el.both,
                                })?
                            }
                            count -= match_count;
                            if count > 0 {
                                cur_ix = cur.unwrap();
                            } else {
                                break;
                            }
                        }
                    }
                    if count > 0 {
                        if can_open {
                            self.inline_stack.push(InlineEl {
                                start: cur_ix,
                                run_length,
                                count,
                                c,
                                both,
                            })?;
                        } else {
                            for i in 0..count {
                                {
                                    let value = ItemBody::Text {
                                        backslash_escaped: false,
                                    };
                                    self.tree
                                        .update(cur_ix + i, |node| node.item.body = value)?;
                                };
                            }
                        }
                        prev_ix = cur_ix + count - 1;
                        prev = Some(prev_ix);
                        cur = self.tree.node(prev_ix)?.next;
                    }
                }
                ItemBody::MaybeSmartQuote(c, can_open, can_close) => {
                    let value = match c {
                        b'\'' => {
                            if let (Some(open_ix), true) = (single_quote_open, can_close) {
                                { let value = ItemBody::SynthesizeChar('‘'); self.tree.update(open_ix, |node| node.item.body = value)?; };
                                single_quote_open = None;
                            } else if can_open {
                                single_quote_open = Some(cur_ix);
                            }
                            ItemBody::SynthesizeChar('’')
                        }
                        _ /* double quote */ => {
                            if can_close && double_quote_open {
                                double_quote_open = false;
                                ItemBody::SynthesizeChar('”')
                            } else {
                                if can_open && !double_quote_open {
                                    double_quote_open = true;
                                }
                                ItemBody::SynthesizeChar('“')
                            }
                        }
                    };
                    self.tree.update(cur_ix, |node| node.item.body = value)?;
                    prev = cur;
                    cur = self.tree.node(cur_ix)?.next;
                }
                ItemBody::HardBreak(true) => {
                    if self.tree.node(cur_ix)?.next.is_none() {
                        {
                            let value = ItemBody::SynthesizeChar('\\');
                            self.tree.update(cur_ix, |node| node.item.body = value)?;
                        };
                    }
                    prev = cur;
                    cur = self.tree.node(cur_ix)?.next;
                }
                _ => {
                    prev = cur;
                    cur = self.tree.node(cur_ix)?.next;
                }
            }
        }
        self.inline_stack.pop_all(&mut self.tree)?;
        Ok(())
    }

    fn disable_all_links(&mut self) {
        self.link_stack.disable_all_links();
        self.wikilink_stack.disable_all_links();
    }

    /// Returns next byte index, url and title.
    fn scan_inline_link(
        &self,
        underlying: Text<'input, B::Input>,
        mut ix: usize,
        node: Option<TreeIndex>,
    ) -> Result<Option<(usize, B::Text<'input>, B::Text<'input>)>, B::Error> {
        self.allocs.backend.speculate_text(|| {
            if underlying.get(ix)? != Some(b'(') {
                return Ok(None);
            }
            ix += 1;
            let scan_separator = |ix: &mut usize| -> Result<(), B::Error> {
                *ix += scan_while(underlying.slice(*ix..), is_ascii_whitespace_no_nl)?;
                if let Some(bl) = scan_eol(underlying.slice(*ix..))? {
                    *ix += bl;
                    *ix +=
                        skip_container_prefixes(&self.tree, underlying.slice(*ix..), self.options)?;
                }
                *ix += scan_while(underlying.slice(*ix..), is_ascii_whitespace_no_nl)?;
                Ok(())
            };
            scan_separator(&mut ix)?;
            let Some((length, dest)) = scan_link_dest(underlying, ix, LINK_MAX_NESTED_PARENS)?
            else {
                return Ok(None);
            };
            let dest = unescape(
                self.allocs.backend.source_text(dest)?,
                self.tree.is_in_table()?,
                &self.allocs.backend,
            )?;
            ix += length;
            scan_separator(&mut ix)?;
            let title = if let Some((length, title)) = self.scan_link_title(underlying, ix, node)? {
                ix += length;
                scan_separator(&mut ix)?;
                title
            } else {
                self.allocs.backend.empty_text()
            };
            if underlying.get(ix)? != Some(b')') {
                return Ok(None);
            }
            Ok(Some((ix + 1, dest, title)))
        })
    }

    fn scan_link_title(
        &self,
        text: Text<'input, B::Input>,
        start_ix: usize,
        node: Option<TreeIndex>,
    ) -> Result<Option<(usize, B::Text<'input>)>, B::Error> {
        self.allocs.backend.speculate_text(|| {
            let open = match text.get(start_ix)? {
                Some(b @ b'\'') | Some(b @ b'"') | Some(b @ b'(') => b,
                _ => return Ok(None),
            };
            let close = if open == b'(' { b')' } else { open };
            let mut title = self.allocs.backend.text_buffer();
            let mut mark = start_ix + 1;
            let mut i = mark;
            while i < text.len() {
                let c = text.byte(i)?;
                if c == close {
                    title.push_source(text.slice(mark..i))?;
                    return Ok(Some((i - start_ix + 1, title.finish()?)));
                }
                if c == open {
                    return Ok(None);
                }
                if c == b'\n' || c == b'\r' {
                    if let Some(node_ix) = scan_nodes_to_ix(&self.tree, node, i + 1)? {
                        if self.tree.node(node_ix)?.item.start > i {
                            title.push_source(text.slice(mark..i))?;
                            title.push_char('\n')?;
                            i = self.tree.node(node_ix)?.item.start;
                            mark = i;
                            continue;
                        }
                    }
                }
                if c == b'&' {
                    if let (n, Some(value)) = scan_entity(text.slice(i..))? {
                        title.push_source(text.slice(mark..i))?;
                        title.push_str(&value)?;
                        i += n;
                        mark = i;
                        continue;
                    }
                }
                if self.tree.is_in_table()?
                    && c == b'\\'
                    && i + 2 < text.len()
                    && text.byte(i + 1)? == b'\\'
                    && text.byte(i + 2)? == b'|'
                {
                    title.push_source(text.slice(mark..i))?;
                    i += 2;
                    mark = i;
                }
                if c == b'\\' && i + 1 < text.len() && is_ascii_punctuation(text.byte(i + 1)?) {
                    title.push_source(text.slice(mark..i))?;
                    i += 1;
                    mark = i;
                }
                i += 1;
            }
            Ok(None)
        })
    }

    fn make_math_span(&mut self, open: TreeIndex, mut close: TreeIndex) -> Result<(), B::Error> {
        let start_is_display = self
            .tree
            .node(open)?
            .next
            .filter(|&next_ix| next_ix != close)
            .map(|next_ix| {
                self.tree.node(next_ix).map(|node| {
                    matches!(node.item.body, ItemBody::MaybeMath(..)).then_some(next_ix)
                })
            })
            .transpose()?
            .flatten();
        let end_is_display = self
            .tree
            .node(close)?
            .next
            .map(|next_ix| {
                self.tree.node(next_ix).map(|node| {
                    matches!(node.item.body, ItemBody::MaybeMath(..)).then_some(next_ix)
                })
            })
            .transpose()?
            .flatten();
        let is_display = start_is_display.is_some() && end_is_display.is_some();
        if is_display {
            close = self.tree.node(close)?.next.unwrap();
            {
                let value = Some(close);
                self.tree.update(open, |node| node.next = value)?;
            };
            {
                let value = 1;
                self.tree.update(open, |node| node.item.end += value)?;
            };
            {
                let value = 1;
                self.tree.update(close, |node| node.item.start -= value)?;
            };
        } else {
            if self.tree.node(open)?.item.end == self.tree.node(close)?.item.start {
                // inline math spans cannot be empty
                {
                    let value = ItemBody::Text {
                        backslash_escaped: false,
                    };
                    self.tree.update(open, |node| node.item.body = value)?;
                };
                return Ok(());
            }
            {
                let value = Some(close);
                self.tree.update(open, |node| node.next = value)?;
            };
        }
        let span_start = self.tree.node(open)?.item.end;
        let span_end = self.tree.node(close)?.item.start;

        let spanned_text = self.text.slice(span_start..span_end);
        let mut buf = self.allocs.backend.text_buffer();
        let mut start_ix = 0;
        let mut ix = 0;
        while ix < spanned_text.len() {
            let c = spanned_text.byte(ix)?;
            if c == b'\r' || c == b'\n' {
                ix += 1;
                buf.push_source(spanned_text.slice(start_ix..ix))?;
                ix += skip_container_prefixes(&self.tree, spanned_text.slice(ix..), self.options)?;
                start_ix = ix;
            } else if c == b'\\'
                && spanned_text.get(ix + 1)? == Some(b'|')
                && self.tree.is_in_table()?
            {
                buf.push_source(spanned_text.slice(start_ix..ix))?;
                buf.push_char('|')?;
                ix += 2;
                start_ix = ix;
            } else {
                ix += 1;
            }
        }
        buf.push_source(spanned_text.slice(start_ix..))?;
        let value = ItemBody::Math(self.allocs.allocate_cow(buf.finish()?)?, is_display);
        self.tree.update(open, |node| node.item.body = value)?;
        {
            let value = self.tree.node(close)?.item.end;
            self.tree.update(open, |node| node.item.end = value)?;
        };
        {
            let value = self.tree.node(close)?.next;
            self.tree.update(open, |node| node.next = value)?;
        };
        Ok(())
    }

    /// Make a code span.
    ///
    /// Both `open` and `close` are matching MaybeCode items.
    fn make_code_span(
        &mut self,
        open: TreeIndex,
        close: TreeIndex,
        preceding_backslash: bool,
    ) -> Result<(), B::Error> {
        let span_start = self.tree.node(open)?.item.end;
        let span_end = self.tree.node(close)?.item.start;
        let mut buf = self.allocs.backend.text_buffer();
        let spanned_text = self.text.slice(span_start..span_end);
        let mut start_ix = 0;
        let mut ix = 0;
        while ix < spanned_text.len() {
            let c = spanned_text.byte(ix)?;
            if c == b'\r' || c == b'\n' {
                buf.push_source(spanned_text.slice(start_ix..ix))?;
                buf.push_char(' ')?;
                ix += 1;
                ix += skip_container_prefixes(&self.tree, spanned_text.slice(ix..), self.options)?;
                start_ix = ix;
            } else if c == b'\\'
                && spanned_text.get(ix + 1)? == Some(b'|')
                && self.tree.is_in_table()?
            {
                buf.push_source(spanned_text.slice(start_ix..ix))?;
                buf.push_char('|')?;
                ix += 2;
                start_ix = ix;
            } else {
                ix += 1;
            }
        }
        buf.push_source(spanned_text.slice(start_ix..))?;
        let cow = buf.finish()?;
        let trim = {
            let source = self.allocs.backend.text_source(&cow)?;
            source.get(0)? == Some(b' ')
                && source.get(source.len().saturating_sub(1))? == Some(b' ')
                && scan_while(source, |byte| byte == b' ')? != source.len()
        };
        let cow = if trim {
            let length = self.allocs.backend.text_len(&cow) as usize;
            self.allocs
                .backend
                .slice_text(cow, 1..(length - 1).max(1))?
        } else {
            cow
        };

        if preceding_backslash {
            {
                let value = ItemBody::Text {
                    backslash_escaped: true,
                };
                self.tree.update(open, |node| node.item.body = value)?;
            };
            {
                let value = self.tree.node(open)?.item.start + 1;
                self.tree.update(open, |node| node.item.end = value)?;
            };
            {
                let value = Some(close);
                self.tree.update(open, |node| node.next = value)?;
            };
            {
                let value = ItemBody::Code(self.allocs.allocate_cow(cow)?);
                self.tree.update(close, |node| node.item.body = value)?;
            };
            {
                let value = self.tree.node(open)?.item.start + 1;
                self.tree.update(close, |node| node.item.start = value)?;
            };
        } else {
            {
                let value = ItemBody::Code(self.allocs.allocate_cow(cow)?);
                self.tree.update(open, |node| node.item.body = value)?;
            };
            {
                let value = self.tree.node(close)?.item.end;
                self.tree.update(open, |node| node.item.end = value)?;
            };
            {
                let value = self.tree.node(close)?.next;
                self.tree.update(open, |node| node.next = value)?;
            };
        }
        Ok(())
    }

    /// A normalized payload is present only when container prefixes were skipped.
    fn scan_inline_html(
        &mut self,
        bytes: Text<'input, B::Input>,
        ix: usize,
    ) -> Result<Option<(Option<B::Text<'input>>, usize)>, B::Error> {
        let Some(byte) = bytes.get(ix)? else {
            return Ok(None);
        };
        if byte == b'!' {
            Ok(
                scan_inline_html_comment(bytes, ix + 1, &mut self.html_scan_guard)?
                    .map(|end| (None, end)),
            )
        } else if byte == b'?' {
            Ok(
                scan_inline_html_processing(bytes, ix + 1, &mut self.html_scan_guard)?
                    .map(|end| (None, end)),
            )
        } else {
            let Some((span, end)) = scan_html_block_inner(
                bytes.slice((ix - 1)..),
                Some(&|bytes| skip_container_prefixes(&self.tree, bytes, self.options)),
                &self.allocs.backend,
            )?
            else {
                return Ok(None);
            };
            Ok(Some((span, end + ix - 1)))
        }
    }
}

/// Returns number of containers scanned.
pub(crate) fn scan_containers<N, P, S>(
    tree: &Tree<Item, N, P>,
    line_start: &mut LineStart<'_, S>,
    options: Options,
    update_flags: bool,
) -> Result<usize, N::Error>
where
    N: ListFlagStorage,
    P: Sequence<TreeIndex, Error = N::Error>,
    S: Bytes<Error = N::Error> + ?Sized,
{
    let mut i = 0;
    let mut spine_iter = tree.walk_spine();
    for node_ix in &mut spine_iter {
        let node_ix = node_ix?;
        match &tree.node(node_ix)?.item.body {
            // See https://github.com/commonmark/cmark/pull/520/changes.
            ItemBody::List(list_flags, _, _) | ItemBody::DefinitionList(list_flags)
                if update_flags =>
            {
                if line_start.is_at_eol()? {
                    if list_flags.get().contains(ListFlags::LAST_LINE_EMPTY)
                        && line_start.remaining_space() == 0
                    {
                        // Non-indented descendants also accept this blank line.
                        return Ok(tree.spine_len());
                    }
                    tree.set_list_flags(node_ix, list_flags.get() | ListFlags::LAST_LINE_EMPTY)?;
                } else {
                    tree.set_list_flags(node_ix, list_flags.get() & !ListFlags::LAST_LINE_EMPTY)?;
                }
            }
            ItemBody::BlockQuote(..) => {
                let save = line_start.clone();
                let _ = line_start.scan_space(3)?;
                if !line_start.scan_blockquote_marker()? {
                    *line_start = save;
                    break;
                }
            }
            ItemBody::ListItem(indent) | ItemBody::DefinitionListDefinition(indent) => {
                let save = line_start.clone();
                if !line_start.scan_space(*indent)? && !line_start.is_at_eol()? {
                    *line_start = save;
                    break;
                }
            }
            ItemBody::FootnoteDefinition(..) if options.has_gfm_footnotes() => {
                let save = line_start.clone();
                if !line_start.scan_space(4)? && !line_start.is_at_eol()? {
                    *line_start = save;
                    break;
                }
            }
            _ => (),
        }
        i += 1;
    }
    for node_ix in &mut spine_iter {
        let node_ix = node_ix?;
        if let ItemBody::List(list_flags, _, _) | ItemBody::DefinitionList(list_flags) =
            &tree.node(node_ix)?.item.body
        {
            tree.set_list_flags(node_ix, list_flags.get() & !ListFlags::LAST_LINE_EMPTY)?;
        }
    }
    Ok(i)
}

pub(crate) fn skip_container_prefixes<N, P, S>(
    tree: &Tree<Item, N, P>,
    bytes: Text<'_, S>,
    options: Options,
) -> Result<usize, N::Error>
where
    N: ListFlagStorage,
    P: Sequence<TreeIndex, Error = N::Error>,
    S: Bytes<Error = N::Error> + ?Sized,
{
    let mut line_start = LineStart::new(bytes);
    scan_containers(tree, &mut line_start, options, false)?;
    Ok(line_start.bytes_scanned())
}

impl<N, P> Tree<Item, N, P>
where
    N: Sequence<Node<Item>>,
    P: Sequence<TreeIndex, Error = N::Error>,
{
    pub(crate) fn append_text(
        &mut self,
        start: usize,
        end: usize,
        backslash_escaped: bool,
    ) -> Result<(), N::Error> {
        if end > start {
            if let Some(index) = self.cur() {
                let merge = {
                    let node = self.node(index)?;
                    matches!(node.item.body, ItemBody::Text { .. }) && node.item.end == start
                };
                if merge {
                    self.update(index, |node| node.item.end = end)?;
                    return Ok(());
                }
            }
            self.append(Item {
                start,
                end,
                body: ItemBody::Text { backslash_escaped },
            })?;
        }
        Ok(())
    }

    /// Stop ancestry lookup at the first non-inline container.
    pub(crate) fn is_in_table(&self) -> Result<bool, N::Error> {
        fn might_be_in_table(item: &Item) -> bool {
            item.body.is_inline()
                || matches!(
                    item.body,
                    ItemBody::TableHead | ItemBody::TableRow | ItemBody::TableCell
                )
        }
        for index in self.walk_spine().rev() {
            let node = self.node(index?)?;
            if matches!(node.item.body, ItemBody::Table(_)) {
                return Ok(true);
            }
            if !might_be_in_table(&node.item) {
                return Ok(false);
            }
        }
        Ok(false)
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct InlineEl {
    /// offset of tree node
    start: TreeIndex,
    /// number of delimiters available for matching
    count: usize,
    /// length of the run that these delimiters came from
    run_length: usize,
    /// b'*', b'_', or b'~'
    c: u8,
    /// can both open and close
    both: bool,
}

#[derive(Debug)]
struct InlineStack<S = Vec<InlineEl>> {
    stack: S,
    // Skip prefixes already ruled out for each delimiter class.
    lower_bounds: [usize; 9],
}

impl<S: Sequence<InlineEl>> InlineStack<S> {
    fn with_storage(stack: S) -> Self {
        Self {
            stack,
            lower_bounds: [0; 9],
        }
    }

    /// These are indices into the lower bounds array.
    /// Not both refers to the property that the delimiter can not both
    /// be opener as a closer.
    const UNDERSCORE_NOT_BOTH: usize = 0;
    const ASTERISK_NOT_BOTH: usize = 1;
    const ASTERISK_BASE: usize = 2;
    const TILDES: usize = 5;
    const UNDERSCORE_BASE: usize = 6;

    fn pop_all<N, P>(&mut self, tree: &mut Tree<Item, N, P>) -> Result<(), S::Error>
    where
        N: Sequence<Node<Item>, Error = S::Error>,
        P: Sequence<TreeIndex, Error = S::Error>,
    {
        for index in 0..self.stack.len() {
            let el = *self.stack.read(index)?;
            for offset in 0..el.count {
                tree.update(el.start + offset, |node| {
                    node.item.body = ItemBody::Text {
                        backslash_escaped: false,
                    };
                })?;
            }
        }
        self.stack.truncate(0)?;
        self.lower_bounds = [0; 9];
        Ok(())
    }

    fn get_lowerbound(&self, c: u8, count: usize, both: bool) -> usize {
        if c == b'_' {
            let mod3_lower = self.lower_bounds[Self::UNDERSCORE_BASE + count % 3];
            if both {
                mod3_lower
            } else {
                min(mod3_lower, self.lower_bounds[Self::UNDERSCORE_NOT_BOTH])
            }
        } else if c == b'*' {
            let mod3_lower = self.lower_bounds[Self::ASTERISK_BASE + count % 3];
            if both {
                mod3_lower
            } else {
                min(mod3_lower, self.lower_bounds[Self::ASTERISK_NOT_BOTH])
            }
        } else {
            self.lower_bounds[Self::TILDES]
        }
    }

    fn set_lowerbound(&mut self, c: u8, count: usize, both: bool, new_bound: usize) {
        if c == b'_' {
            self.lower_bounds[Self::UNDERSCORE_BASE + count % 3] = new_bound;
            if !both {
                self.lower_bounds[Self::UNDERSCORE_NOT_BOTH] = new_bound;
            }
        } else if c == b'*' {
            self.lower_bounds[Self::ASTERISK_BASE + count % 3] = new_bound;
            if !both {
                self.lower_bounds[Self::ASTERISK_NOT_BOTH] = new_bound;
            }
        } else {
            self.lower_bounds[Self::TILDES] = new_bound;
        }
    }

    fn truncate(&mut self, new_bound: usize) -> Result<(), S::Error> {
        self.stack.truncate(new_bound)?;
        for lower_bound in &mut self.lower_bounds {
            if *lower_bound > new_bound {
                *lower_bound = new_bound;
            }
        }
        Ok(())
    }

    fn find_match<N, P>(
        &mut self,
        tree: &mut Tree<Item, N, P>,
        c: u8,
        run_length: usize,
        both: bool,
    ) -> Result<Option<InlineEl>, S::Error>
    where
        N: Sequence<Node<Item>, Error = S::Error>,
        P: Sequence<TreeIndex, Error = S::Error>,
    {
        let lowerbound = min(self.stack.len(), self.get_lowerbound(c, run_length, both));
        let mut matching = None;
        for index in (lowerbound..self.stack.len()).rev() {
            let el = *self.stack.read(index)?;
            if c == b'~' && run_length != el.run_length {
                continue;
            }
            if el.c == c
                && (!both && !el.both
                    || (run_length + el.run_length) % 3 != 0
                    || run_length % 3 == 0)
            {
                matching = Some((index, el));
                break;
            }
        }
        if let Some((matching_ix, matching_el)) = matching {
            for index in (matching_ix + 1)..self.stack.len() {
                let el = *self.stack.read(index)?;
                for offset in 0..el.count {
                    tree.update(el.start + offset, |node| {
                        node.item.body = ItemBody::Text {
                            backslash_escaped: false,
                        };
                    })?;
                }
            }
            self.truncate(matching_ix)?;
            Ok(Some(matching_el))
        } else {
            self.set_lowerbound(c, run_length, both, self.stack.len());
            Ok(None)
        }
    }

    fn trim_lower_bound(&mut self, ix: usize) {
        self.lower_bounds[ix] = self.lower_bounds[ix].min(self.stack.len());
    }

    fn push(&mut self, el: InlineEl) -> Result<(), S::Error> {
        if el.c == b'~' {
            self.trim_lower_bound(Self::TILDES);
        }
        self.stack.push(el)
    }
}

#[derive(Debug, Clone)]
enum RefScan<Text> {
    LinkLabel(Text, usize),
    // contains next node index
    Collapsed(Option<TreeIndex>),
    UnexpectedFootnote,
    Failed,
}

/// Skips forward within a block to a node which spans (ends inclusive) the given
/// index into the source.
fn scan_nodes_to_ix<N, P>(
    tree: &Tree<Item, N, P>,
    mut node: Option<TreeIndex>,
    ix: usize,
) -> Result<Option<TreeIndex>, N::Error>
where
    N: Sequence<Node<Item>>,
    P: Sequence<TreeIndex, Error = N::Error>,
{
    while let Some(node_ix) = node {
        if tree.node(node_ix)?.item.end <= ix {
            node = tree.node(node_ix)?.next;
        } else {
            break;
        }
    }
    Ok(node)
}

/// Scan an inline label, including brackets in the consumed length.
fn scan_link_label<'text, B: Backend>(
    tree: &Tree<Item, B::Nodes, B::Spine>,
    text: Text<'text, B::Input>,
    options: Options,
    backend: &B,
) -> Result<Option<(usize, ReferenceLabel<B::Text<'text>>)>, B::Error> {
    if text.len() < 2 || text.byte(0)? != b'[' {
        return Ok(None);
    }
    let linebreak_handler = |bytes| skip_container_prefixes(tree, bytes, options).map(Some);
    if options.contains(Options::ENABLE_FOOTNOTES)
        && b'^' == text.byte(1)?
        && text.get(2)? != Some(b']')
    {
        let handler: &dyn Fn(Text<'text, B::Input>) -> Result<Option<usize>, B::Error> =
            if options.has_gfm_footnotes() {
                &|_| Ok(None)
            } else {
                &linebreak_handler
            };
        if let Some((length, label)) =
            scan_link_label_rest(text.slice(2..), handler, tree.is_in_table()?, backend)?
        {
            return Ok(Some((length + 2, ReferenceLabel::Footnote(label))));
        }
    }
    let Some((length, label)) = scan_link_label_rest(
        text.slice(1..),
        &linebreak_handler,
        tree.is_in_table()?,
        backend,
    )?
    else {
        return Ok(None);
    };
    Ok(Some((length + 1, ReferenceLabel::Link(label))))
}

fn scan_reference<'a, B: Backend>(
    tree: &Tree<Item, B::Nodes, B::Spine>,
    text: Text<'a, B::Input>,
    cur: Option<TreeIndex>,
    options: Options,
    backend: &B,
) -> Result<RefScan<B::Text<'a>>, B::Error> {
    let Some(index) = cur else {
        return Ok(RefScan::Failed);
    };
    let start = tree.node(index)?.item.start;
    let tail = text.slice(start..);
    Ok(if tail.starts_with(b"[]")? {
        let closing = tree.node(index)?.next.unwrap();
        RefScan::Collapsed(tree.node(closing)?.next)
    } else {
        match scan_link_label(tree, tail, options, backend)? {
            Some((length, ReferenceLabel::Link(label))) => {
                RefScan::LinkLabel(label, start + length)
            }
            Some((_, ReferenceLabel::Footnote(_))) => RefScan::UnexpectedFootnote,
            None => RefScan::Failed,
        }
    })
}

struct LinkStack<S = Vec<LinkStackEl>> {
    inner: S,
    disabled_ix: usize,
}

impl<S: Sequence<LinkStackEl>> LinkStack<S> {
    fn with_storage(inner: S) -> Self {
        Self {
            inner,
            disabled_ix: 0,
        }
    }

    fn push(&mut self, el: LinkStackEl) -> Result<(), S::Error> {
        self.inner.push(el)
    }

    fn pop(&mut self) -> Result<Option<LinkStackEl>, S::Error> {
        let mut el = self.inner.pop()?;
        let remaining = self.inner.len();
        if remaining < self.disabled_ix {
            if let Some(el) = &mut el {
                if el.ty == LinkStackTy::Link {
                    el.ty = LinkStackTy::Disabled;
                }
            }
        }
        self.disabled_ix = self.disabled_ix.min(remaining);
        Ok(el)
    }

    fn clear(&mut self) -> Result<(), S::Error> {
        self.inner.truncate(0)?;
        self.disabled_ix = 0;
        Ok(())
    }

    fn disable_all_links(&mut self) {
        self.disabled_ix = self.inner.len();
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LinkStackEl {
    node: TreeIndex,
    ty: LinkStackTy,
}

#[derive(PartialEq, Clone, Debug)]
enum LinkStackTy {
    Link,
    Image,
    Disabled,
}

/// Contains the destination URL, title and source span of a reference definition.
#[derive(Clone, Debug)]
pub struct LinkDef<Text> {
    pub dest: Text,
    pub title: Option<Text>,
    pub span: Range<usize>,
}

impl<'a> LinkDef<CowStr<'a>> {
    pub fn into_static(self) -> LinkDef<CowStr<'static>> {
        LinkDef {
            dest: self.dest.into_static(),
            title: self.title.map(|s| s.into_static()),
            span: self.span,
        }
    }
}

/// Index closing code delimiters by run length to avoid rescanning the AST.
struct CodeDelims<Q> {
    inner: Q,
    seen_first: bool,
    populated: bool,
}

impl<Q: Queues<usize, TreeIndex>> CodeDelims<Q> {
    fn with_storage(inner: Q) -> Self {
        Self {
            inner,
            seen_first: false,
            populated: false,
        }
    }

    fn insert(&mut self, count: usize, ix: TreeIndex) -> Result<(), Q::Error> {
        if self.seen_first {
            self.inner.push_back(count, ix)?;
            self.populated = true;
        } else {
            // The first delimiter is always an opener.
            self.seen_first = true;
        }
        Ok(())
    }

    fn is_populated(&self) -> bool {
        self.populated
    }

    fn find(&mut self, open_ix: TreeIndex, count: usize) -> Result<Option<TreeIndex>, Q::Error> {
        let mut queue = self.inner.cursor(count)?;
        while let Some(index) = queue.front()? {
            queue.pop_front()?;
            if index > open_ix {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    fn clear(&mut self) -> Result<(), Q::Error> {
        self.inner.clear()?;
        self.seen_first = false;
        self.populated = false;
        Ok(())
    }
}

/// Keep math delimiters ordered within each brace context.
struct MathDelims<Q> {
    inner: Q,
    populated: bool,
}

impl<Q: Queues<u8, (TreeIndex, bool, bool)>> MathDelims<Q> {
    fn with_storage(inner: Q) -> Self {
        Self {
            inner,
            populated: false,
        }
    }

    fn insert(
        &mut self,
        delim_is_display: bool,
        brace_context: u8,
        index: TreeIndex,
        can_close: bool,
    ) -> Result<(), Q::Error> {
        self.inner
            .push_back(brace_context, (index, can_close, delim_is_display))?;
        self.populated = true;
        Ok(())
    }

    fn is_populated(&self) -> bool {
        self.populated
    }

    fn find<N, P>(
        &mut self,
        tree: &Tree<Item, N, P>,
        open_ix: TreeIndex,
        is_display: bool,
        brace_context: u8,
    ) -> Result<Option<TreeIndex>, Q::Error>
    where
        N: Sequence<Node<Item>, Error = Q::Error>,
        P: Sequence<TreeIndex, Error = Q::Error>,
    {
        let mut queue = self.inner.cursor(brace_context)?;
        while let Some((index, can_close, delim_is_display)) = queue.front()? {
            if index <= open_ix || (is_display && tree.node(open_ix)?.next == Some(index)) {
                queue.pop_front()?;
                continue;
            }
            let eligible_close =
                can_close && tree.node(open_ix)?.item.end != tree.node(index)?.item.start;
            if (!is_display && eligible_close) || (is_display && delim_is_display) {
                queue.pop_front()?;
                return Ok(Some(index));
            }
            if eligible_close != can_close {
                queue.replace_front((index, eligible_close, delim_is_display))?;
            }
            break;
        }
        Ok(None)
    }

    fn clear(&mut self) -> Result<(), Q::Error> {
        self.inner.clear()?;
        self.populated = false;
        Ok(())
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct LinkIndex(usize);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct CowIndex(usize);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct AlignmentIndex(usize);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct HeadingIndex(NonZeroUsize);

pub(crate) struct Allocations<'a, B: Backend = Memory> {
    pub refdefs: B::ReferenceValues<'a>,
    pub footdefs: B::FootnoteValues<'a>,
    links: B::LinkValues<'a>,
    cows: B::CowValues<'a>,
    alignments: B::AlignmentValues,
    headings: B::HeadingValues<'a>,
    pub backend: B,
}

#[derive(Clone)]
pub(crate) struct HeadingAttributes<
    Text,
    Classes = Vec<Text>,
    Attributes = Vec<(Text, Option<Text>)>,
> {
    pub id: Option<Text>,
    pub classes: Classes,
    pub attrs: Attributes,
}

/// Keeps track of the reference definitions defined in the document.
#[derive(Clone, Default, Debug)]
pub struct RefDefs<'input>(pub(crate) HashMap<LinkLabel<'input>, LinkDef<CowStr<'input>>>);

/// Keeps track of the footnote definitions defined in the document.
#[derive(Clone, Default, Debug)]
pub struct FootnoteDefs<'input>(pub(crate) HashSet<FootnoteLabel<'input>>);

impl<'input, 'b, 's> RefDefs<'input>
where
    's: 'b,
{
    /// Performs a lookup on reference label using unicode case folding.
    pub fn get(&'s self, key: &'b str) -> Option<&'b LinkDef<CowStr<'input>>> {
        self.0.get(&UniCase::new(key.into()))
    }

    /// Provides an iterator over all the document's reference definitions.
    pub fn iter(&'s self) -> impl Iterator<Item = (&'s str, &'s LinkDef<CowStr<'input>>)> {
        self.0.iter().map(|(k, v)| (k.as_ref(), v))
    }
}

impl<'a> Dictionary<CowStr<'a>, LinkDef<CowStr<'a>>> for RefDefs<'a> {
    type Error = Infallible;

    fn lookup(&self, key: &CowStr<'a>) -> Result<Option<LinkDef<CowStr<'a>>>, Infallible> {
        Ok(self.get(key.as_ref()).cloned())
    }

    fn insert_first(
        &mut self,
        key: CowStr<'a>,
        value: LinkDef<CowStr<'a>>,
    ) -> Result<(), Infallible> {
        self.0.entry(UniCase::new(key)).or_insert(value);
        Ok(())
    }
}

impl<'a> Set<CowStr<'a>> for FootnoteDefs<'a> {
    type Error = Infallible;

    fn contains(&self, key: &CowStr<'a>) -> Result<bool, Infallible> {
        Ok(self.0.contains(&UniCase::new(key.as_ref().into())))
    }

    fn insert(&mut self, key: CowStr<'a>) -> Result<(), Infallible> {
        self.0.insert(UniCase::new(key));
        Ok(())
    }
}

impl<'a, B: Backend> Allocations<'a, B> {
    pub fn new(backend: B) -> Result<Self, B::Error> {
        Ok(Self {
            refdefs: backend.reference_values()?,
            footdefs: backend.footnote_values()?,
            links: backend.link_values()?,
            cows: backend.cow_values()?,
            alignments: backend.alignment_values()?,
            headings: backend.heading_values()?,
            backend,
        })
    }

    pub fn allocate_cow(&mut self, cow: B::Text<'a>) -> Result<CowIndex, B::Error> {
        let ix = self.cows.len();
        self.cows.push(cow)?;
        Ok(CowIndex(ix))
    }

    pub fn allocate_link(
        &mut self,
        ty: LinkType,
        url: B::Text<'a>,
        title: B::Text<'a>,
        id: B::Text<'a>,
    ) -> Result<LinkIndex, B::Error> {
        let ix = self.links.len();
        self.links.push((ty, url, title, id))?;
        Ok(LinkIndex(ix))
    }

    pub fn allocate_alignment(
        &mut self,
        alignment: B::Columns,
    ) -> Result<AlignmentIndex, B::Error> {
        let ix = self.alignments.len();
        self.alignments
            .push(self.backend.finish_columns(alignment)?)?;
        Ok(AlignmentIndex(ix))
    }

    pub fn allocate_heading(
        &mut self,
        attrs: HeadingValue<'a, B>,
    ) -> Result<HeadingIndex, B::Error> {
        let ix = self.headings.len();
        self.headings.push(attrs)?;
        let ix = NonZeroUsize::new(ix.wrapping_add(1)).expect("too many headings");
        Ok(HeadingIndex(ix))
    }

    pub fn take_cow(&mut self, ix: CowIndex) -> Result<B::Text<'a>, B::Error> {
        let empty = self.backend.empty_text();
        self.cows
            .update(ix.0, |value| std::mem::replace(value, empty))
    }

    pub fn take_link(&mut self, ix: LinkIndex) -> Result<LinkValue<'a, B>, B::Error> {
        let empty = (
            LinkType::ShortcutUnknown,
            self.backend.empty_text(),
            self.backend.empty_text(),
            self.backend.empty_text(),
        );
        self.links
            .update(ix.0, |value| std::mem::replace(value, empty))
    }

    pub fn take_alignment(&mut self, ix: AlignmentIndex) -> Result<B::Alignments, B::Error> {
        self.alignments.update(ix.0, std::mem::take)
    }

    pub fn alignment_len(&self, ix: AlignmentIndex) -> Result<usize, B::Error> {
        Ok(self.backend.alignment_len(&*self.alignments.read(ix.0)?))
    }

    pub fn heading(&self, ix: HeadingIndex) -> Result<HeadingValue<'a, B>, B::Error> {
        Ok((*self.headings.read(ix.0.get() - 1)?).clone())
    }
}

/// A struct containing information on the reachability of certain inline HTML
/// elements. In particular, for cdata elements (`<![CDATA[`), processing
/// elements (`<?`) and declarations (`<!DECLARATION`). The respectives usizes
/// represent the indices before which a scan will always fail and can hence
/// be skipped.
#[derive(Clone, Default)]
pub(crate) struct HtmlScanGuard {
    pub cdata: usize,
    pub processing: usize,
    pub declaration: usize,
    pub comment: usize,
}

/// Trait for broken link callbacks.
///
/// See [Parser::new_with_broken_link_callback].
/// Automatically implemented for closures with the appropriate signature.
pub trait BrokenLinkCallback<Text> {
    fn handle_broken_link(&mut self, link: BrokenLink<Text>) -> Option<(Text, Text)>;
}

impl<Text, T> BrokenLinkCallback<Text> for T
where
    T: FnMut(BrokenLink<Text>) -> Option<(Text, Text)>,
{
    fn handle_broken_link(&mut self, link: BrokenLink<Text>) -> Option<(Text, Text)> {
        self(link)
    }
}

impl<Text> BrokenLinkCallback<Text> for Box<dyn BrokenLinkCallback<Text>> {
    fn handle_broken_link(&mut self, link: BrokenLink<Text>) -> Option<(Text, Text)> {
        (**self).handle_broken_link(link)
    }
}

/// Broken link callback that does nothing.
#[derive(Debug)]
pub struct DefaultBrokenLinkCallback;

impl<Text> BrokenLinkCallback<Text> for DefaultBrokenLinkCallback {
    fn handle_broken_link(&mut self, _link: BrokenLink<Text>) -> Option<(Text, Text)> {
        None
    }
}

/// Markdown event and source range iterator.
///
/// Generates tuples where the first element is the markdown event and the second
/// is a the corresponding range in the source string.
///
/// Constructed from a `Parser` using its
/// [`into_offset_iter`](struct.Parser.html#method.into_offset_iter) method.
#[derive(Debug)]
pub struct OffsetIter<'a, F = DefaultBrokenLinkCallback> {
    inner: Core<'a, F>,
}

impl<'a, F: BrokenLinkCallback<CowStr<'a>>> OffsetIter<'a, F> {
    /// Returns a reference to the internal reference definition tracker.
    pub fn reference_definitions(&self) -> &RefDefs<'_> {
        &self.inner.allocs.refdefs
    }
}

impl<'a, F: BrokenLinkCallback<CowStr<'a>>> Iterator for OffsetIter<'a, F> {
    type Item = (Event<CowStr<'a>>, Range<usize>);

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next_event() {
            Ok(event) => event,
            Err(error) => match error {},
        }
    }
}

fn body_to_tag_end(body: &ItemBody) -> TagEnd {
    match *body {
        ItemBody::Paragraph => TagEnd::Paragraph,
        ItemBody::Emphasis => TagEnd::Emphasis,
        ItemBody::Superscript => TagEnd::Superscript,
        ItemBody::Subscript => TagEnd::Subscript,
        ItemBody::Strong => TagEnd::Strong,
        ItemBody::Strikethrough => TagEnd::Strikethrough,
        ItemBody::Link(..) => TagEnd::Link,
        ItemBody::Image(..) => TagEnd::Image,
        ItemBody::Heading(level, _) => TagEnd::Heading(level),
        ItemBody::IndentCodeBlock | ItemBody::FencedCodeBlock(..) => TagEnd::CodeBlock,
        ItemBody::BlockQuote(kind) => TagEnd::BlockQuote(kind),
        ItemBody::HtmlBlock => TagEnd::HtmlBlock,
        ItemBody::List(_, c, _) => {
            let is_ordered = c == b'.' || c == b')';
            TagEnd::List(is_ordered)
        }
        ItemBody::ListItem(_) => TagEnd::Item,
        ItemBody::TableHead => TagEnd::TableHead,
        ItemBody::TableCell => TagEnd::TableCell,
        ItemBody::TableRow => TagEnd::TableRow,
        ItemBody::Table(..) => TagEnd::Table,
        ItemBody::FootnoteDefinition(..) => TagEnd::FootnoteDefinition,
        ItemBody::MetadataBlock(kind) => TagEnd::MetadataBlock(kind),
        ItemBody::DefinitionList(_) => TagEnd::DefinitionList,
        ItemBody::DefinitionListTitle => TagEnd::DefinitionListTitle,
        ItemBody::DefinitionListDefinition(_) => TagEnd::DefinitionListDefinition,
        _ => panic!("unexpected item body {:?}", body),
    }
}

fn item_to_event<'a, B: Backend>(
    item: &Item,
    text: Text<'a, B::Input>,
    allocs: &mut Allocations<'a, B>,
) -> Result<ParsedEvent<'a, B>, B::Error> {
    let tag = match item.body {
        ItemBody::Text { .. } => {
            return Ok(Event::Text(
                allocs
                    .backend
                    .source_text(text.slice(item.start..item.end))?,
            ))
        }
        ItemBody::Code(cow_ix) => return Ok(Event::Code(allocs.take_cow(cow_ix)?)),
        ItemBody::SynthesizeText(cow_ix) => return Ok(Event::Text(allocs.take_cow(cow_ix)?)),
        ItemBody::SynthesizeChar(c) => {
            return Ok(Event::Text(allocs.backend.store_text(c.into())?))
        }
        ItemBody::HtmlBlock => Tag::HtmlBlock,
        ItemBody::Html => {
            return Ok(Event::Html(
                allocs
                    .backend
                    .source_text(text.slice(item.start..item.end))?,
            ))
        }
        ItemBody::InlineHtml => {
            return Ok(Event::InlineHtml(
                allocs
                    .backend
                    .source_text(text.slice(item.start..item.end))?,
            ))
        }
        ItemBody::OwnedInlineHtml(cow_ix) => {
            return Ok(Event::InlineHtml(allocs.take_cow(cow_ix)?))
        }
        ItemBody::SoftBreak => return Ok(Event::SoftBreak),
        ItemBody::HardBreak(_) => return Ok(Event::HardBreak),
        ItemBody::FootnoteReference(cow_ix) => {
            return Ok(Event::FootnoteReference(allocs.take_cow(cow_ix)?))
        }
        ItemBody::TaskListMarker(checked) => return Ok(Event::TaskListMarker(checked)),
        ItemBody::Rule => return Ok(Event::Rule),
        ItemBody::Paragraph => Tag::Paragraph,
        ItemBody::Emphasis => Tag::Emphasis,
        ItemBody::Superscript => Tag::Superscript,
        ItemBody::Subscript => Tag::Subscript,
        ItemBody::Strong => Tag::Strong,
        ItemBody::Strikethrough => Tag::Strikethrough,
        ItemBody::Link(link_ix) => {
            let (link_type, dest_url, title, id) = allocs.take_link(link_ix)?;
            Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }
        }
        ItemBody::Image(link_ix) => {
            let (link_type, dest_url, title, id) = allocs.take_link(link_ix)?;
            Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }
        }
        ItemBody::Heading(level, Some(heading_ix)) => {
            let HeadingAttributes { id, classes, attrs } = allocs.heading(heading_ix)?;
            Tag::Heading {
                level,
                id,
                classes,
                attrs,
            }
        }
        ItemBody::Heading(level, None) => Tag::Heading {
            level,
            id: None,
            classes: Default::default(),
            attrs: Default::default(),
        },
        ItemBody::FencedCodeBlock(cow_ix) => {
            Tag::CodeBlock(CodeBlockKind::Fenced(allocs.take_cow(cow_ix)?))
        }
        ItemBody::IndentCodeBlock => Tag::CodeBlock(CodeBlockKind::Indented),
        ItemBody::BlockQuote(kind) => Tag::BlockQuote(kind),
        ItemBody::List(_, c, listitem_start) => {
            if c == b'.' || c == b')' {
                Tag::List(Some(listitem_start))
            } else {
                Tag::List(None)
            }
        }
        ItemBody::ListItem(_) => Tag::Item,
        ItemBody::TableHead => Tag::TableHead,
        ItemBody::TableCell => Tag::TableCell,
        ItemBody::TableRow => Tag::TableRow,
        ItemBody::Table(alignment_ix) => Tag::Table(allocs.take_alignment(alignment_ix)?),
        ItemBody::FootnoteDefinition(cow_ix) => Tag::FootnoteDefinition(allocs.take_cow(cow_ix)?),
        ItemBody::MetadataBlock(kind) => Tag::MetadataBlock(kind),
        ItemBody::Math(cow_ix, is_display) => {
            return Ok(if is_display {
                Event::DisplayMath(allocs.take_cow(cow_ix)?)
            } else {
                Event::InlineMath(allocs.take_cow(cow_ix)?)
            });
        }
        ItemBody::DefinitionList(_) => Tag::DefinitionList,
        ItemBody::DefinitionListTitle => Tag::DefinitionListTitle,
        ItemBody::DefinitionListDefinition(_) => Tag::DefinitionListDefinition,
        _ => panic!("unexpected item body {:?}", item.body),
    };

    Ok(Event::Start(tag))
}

impl<'a, F: BrokenLinkCallback<B::Text<'a>>, B: Backend> Core<'a, F, B> {
    fn next_event(&mut self) -> Result<Option<(ParsedEvent<'a, B>, Range<usize>)>, B::Error> {
        loop {
            match self.tree.cur() {
                None => {
                    let Some(index) = self.tree.pop()? else {
                        return Ok(None);
                    };
                    let item = self.tree.node(index)?.item.clone();
                    self.tree.next_sibling(index)?;
                    if matches!(item.body, ItemBody::TightParagraph) {
                        continue;
                    }
                    return Ok(Some((
                        Event::End(body_to_tag_end(&item.body)),
                        item.start..item.end,
                    )));
                }
                Some(index) => {
                    if matches!(self.tree.node(index)?.item.body, ItemBody::TightParagraph) {
                        self.tree.push()?;
                        continue;
                    }
                    if self.tree.node(index)?.item.body.is_maybe_inline() {
                        self.handle_inline()?;
                    }
                    let item = self.tree.node(index)?.item.clone();
                    let event = item_to_event(&item, self.text, &mut self.allocs)?;
                    if matches!(event, Event::Start(..)) {
                        self.tree.push()?;
                    } else {
                        self.tree.next_sibling(index)?;
                    }
                    return Ok(Some((event, item.start..item.end)));
                }
            }
        }
    }
}

impl<'a, F: BrokenLinkCallback<CowStr<'a>>> Iterator for Parser<'a, F> {
    type Item = Event<CowStr<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next_event() {
            Ok(event) => event.map(|(event, _)| event),
            Err(error) => match error {},
        }
    }
}

impl<'a, F: BrokenLinkCallback<CowStr<'a>>> FusedIterator for Parser<'a, F> {}

#[cfg(test)]
mod test {
    use super::*;

    fn parser_with_extensions(text: &str) -> Parser<'_> {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_FOOTNOTES);
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_SUPERSCRIPT);
        opts.insert(Options::ENABLE_SUBSCRIPT);
        opts.insert(Options::ENABLE_TASKLISTS);

        Parser::new_ext(text, opts)
    }

    #[cfg(feature = "disk")]
    #[test]
    fn disk_ranges_survive_eviction_and_cancel() -> std::io::Result<()> {
        use crate::disk::{ByteSource, Spool, Store, TextRef};
        use std::io::Write;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let store = Store::new(Arc::clone(&cancelled))?;
        let spool = Spool::new(123, Arc::clone(&cancelled))?;
        spool.append(b"unrelated prefix\n")?;
        let document =
            "[KEY]: /retained \"first\n[^x  y] text\ntail\"\n\n` café\n e\u{301}🙂 ` [key]\n";
        let input = spool.append(document.as_bytes())?;
        spool.append(b"unrelated suffix\n")?;
        let options = Options::ENABLE_FOOTNOTES;
        assert_eq!(
            DiskParser::new(
                TextRef {
                    source: 124,
                    ..input
                },
                options,
                &store,
                &spool
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput,
        );
        let expect_text = |reference: TextRef, expected: &str| -> std::io::Result<()> {
            assert_eq!(reference.len, expected.len() as u64);
            let mut bytes = [0; 64];
            let count = spool.view(reference)?.read_at(0, &mut bytes)?;
            assert_eq!(&bytes[..count], expected.as_bytes());
            Ok(())
        };
        store.with_transaction(|_| {
            let mut parser = DiskParser::new(input, options, &store, &spool)?;
            let mut saw_code = false;
            while let Some((event, range)) = parser.next_event()? {
                match event {
                    Event::Code(reference) => {
                        assert!(!saw_code);
                        assert_eq!(
                            range,
                            document.find('`').unwrap()..document.rfind('`').unwrap() + 1
                        );
                        let mut page = [b'z'; 64 * 1024];
                        let mut writer = spool.writer();
                        for _ in 0..5 {
                            writer.write_all(&page)?;
                        }
                        let eviction = writer.finish()?;
                        for index in 0..5 {
                            assert_eq!(
                                spool.read_at(
                                    eviction.start + index * page.len() as u64,
                                    &mut page
                                )?,
                                page.len()
                            );
                        }
                        expect_text(reference, "café  e\u{301}🙂")?;
                        saw_code = true;
                    }
                    Event::Start(Tag::Link {
                        dest_url,
                        title,
                        id,
                        ..
                    }) => {
                        assert!(saw_code);
                        expect_text(dest_url, "/retained")?;
                        expect_text(title, "first\n[^x  y] text\ntail")?;
                        expect_text(id, "key")?;
                        cancelled.store(true, Ordering::Relaxed);
                        assert_eq!(
                            parser.next_event().unwrap_err().kind(),
                            std::io::ErrorKind::Interrupted
                        );
                        assert_eq!(
                            spool.view(input).unwrap_err().kind(),
                            std::io::ErrorKind::Interrupted
                        );
                        return Ok(());
                    }
                    _ => {}
                }
            }
            panic!("reference link missing");
        })
    }

    #[test]
    fn single_open_fish_bracket() {
        // dont crash
        assert_eq!(3, Parser::new("<").count());
    }

    #[test]
    fn lone_hashtag() {
        // dont crash
        assert_eq!(2, Parser::new("#").count());
    }

    #[test]
    fn lots_of_backslashes() {
        // dont crash
        Parser::new("\\\\\r\r").count();
        Parser::new("\\\r\r\\.\\\\\r\r\\.\\").count();
    }

    #[test]
    fn issue_1095() {
        let s = "- [n]:Z\r\n\t\t";
        let parser = Parser::new_ext(s, Options::all());
        for _ in parser {}
    }

    #[test]
    fn issue_1084() {
        let markdown_input = "* [ ] ~![=?\\*\x0c\x00\x00  \x0d* [  1=1\x00\x0d<!]:[=?\\\x0d\x0c\n* [ ] \x0d\x0c%    ";

        let mut options = Options::empty();
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_STRIKETHROUGH);

        let parser = Parser::new_ext(markdown_input, options);
        for _ in parser {}
    }

    #[test]
    fn issue_1030() {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_WIKILINKS);

        let parser = Parser::new_ext("For a new ferrari, [[Wikientry|click here]]!", opts);

        let offsets = parser
            .into_offset_iter()
            .map(|(_ev, range)| range)
            .collect::<Vec<_>>();
        let expected_offsets = vec![
            (0..44),  // Paragraph START
            (0..19),  // `For a new ferrari, `
            (19..43), // Wikilink START
            (31..41), // `click here`
            (19..43), // Wikilink END
            (43..44), // `!`
            (0..44),  // Paragraph END
        ];
        assert_eq!(offsets, expected_offsets);
    }

    #[test]
    fn issue_320() {
        // dont crash
        parser_with_extensions(":\r\t> |\r:\r\t> |\r").count();
    }

    #[test]
    fn issue_319() {
        // dont crash
        parser_with_extensions("|\r-]([^|\r-]([^").count();
        parser_with_extensions("|\r\r=][^|\r\r=][^car").count();
    }

    #[test]
    fn issue_303() {
        // dont crash
        parser_with_extensions("[^\r\ra]").count();
        parser_with_extensions("\r\r]Z[^\x00\r\r]Z[^\x00").count();
    }

    #[test]
    fn issue_313() {
        // dont crash
        parser_with_extensions("*]0[^\r\r*]0[^").count();
        parser_with_extensions("[^\r> `][^\r> `][^\r> `][").count();
    }

    #[test]
    fn issue_311() {
        // dont crash
        parser_with_extensions("\\\u{0d}-\u{09}\\\u{0d}-\u{09}").count();
    }

    #[test]
    fn issue_283() {
        let input = std::str::from_utf8(b"\xf0\x9b\xb2\x9f<td:^\xf0\x9b\xb2\x9f").unwrap();
        // dont crash
        parser_with_extensions(input).count();
    }

    #[test]
    fn issue_289() {
        // dont crash
        parser_with_extensions("> - \\\n> - ").count();
        parser_with_extensions("- \n\n").count();
    }

    #[test]
    fn issue_306() {
        // dont crash
        parser_with_extensions("*\r_<__*\r_<__*\r_<__*\r_<__").count();
    }

    #[test]
    fn issue_305() {
        // dont crash
        parser_with_extensions("_6**6*_*").count();
    }

    #[test]
    fn another_emphasis_panic() {
        parser_with_extensions("*__#_#__*").count();
    }

    #[test]
    fn offset_iter() {
        let event_offsets: Vec<_> = Parser::new("*hello* world")
            .into_offset_iter()
            .map(|(_ev, range)| range)
            .collect();
        let expected_offsets = vec![(0..13), (0..7), (1..6), (0..7), (7..13), (0..13)];
        assert_eq!(expected_offsets, event_offsets);
    }

    #[test]
    fn issue_1142_deflist_offset_is_char_boundary() {
        // Title `%`, definition whose text is combining acute U+0301, then a
        // TAB-indented second definition. The second definition's start used
        // to land inside the combining character.
        let doc = "%\n:\u{301}\n\t:";
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_DEFINITION_LIST);
        for (_event, range) in Parser::new_ext(doc, opts).into_offset_iter() {
            let _ = &doc[range];
        }
    }

    #[test]
    fn reference_link_offsets() {
        let range =
            Parser::new("# H1\n[testing][Some reference]\n\n[Some reference]: https://github.com")
                .into_offset_iter()
                .filter_map(|(ev, range)| match ev {
                    Event::Start(
                        Tag::Link {
                            link_type: LinkType::Reference,
                            ..
                        },
                        ..,
                    ) => Some(range),
                    _ => None,
                })
                .next()
                .unwrap();
        assert_eq!(5..30, range);
    }

    #[test]
    fn footnote_offsets() {
        let range = parser_with_extensions("Testing this[^1] out.\n\n[^1]: Footnote.")
            .into_offset_iter()
            .filter_map(|(ev, range)| match ev {
                Event::FootnoteReference(..) => Some(range),
                _ => None,
            })
            .next()
            .unwrap();
        assert_eq!(12..16, range);
    }

    #[test]
    fn footnote_offsets_exclamation() {
        let mut immediately_before_footnote = None;
        let range = parser_with_extensions("Testing this![^1] out.\n\n[^1]: Footnote.")
            .into_offset_iter()
            .filter_map(|(ev, range)| match ev {
                Event::FootnoteReference(..) => Some(range),
                _ => {
                    immediately_before_footnote = Some((ev, range));
                    None
                }
            })
            .next()
            .unwrap();
        assert_eq!(13..17, range);
        if let (Event::Text(exclamation), range_exclamation) =
            immediately_before_footnote.as_ref().unwrap()
        {
            assert_eq!("!", &exclamation[..]);
            assert_eq!(&(12..13), range_exclamation);
        } else {
            panic!("what came first, then? {immediately_before_footnote:?}");
        }
    }

    #[test]
    fn table_offset() {
        let markdown = "a\n\nTesting|This|Outtt\n--|:--:|--:\nSome Data|Other data|asdf";
        let event_offset = parser_with_extensions(markdown)
            .into_offset_iter()
            .map(|(_ev, range)| range)
            .nth(3)
            .unwrap();
        let expected_offset = 3..59;
        assert_eq!(expected_offset, event_offset);
    }

    #[test]
    fn table_cell_span() {
        let markdown = "a|b|c\n--|--|--\na|  |c";
        let event_offset = parser_with_extensions(markdown)
            .into_offset_iter()
            .filter_map(|(ev, span)| match ev {
                Event::Start(Tag::TableCell) => Some(span),
                _ => None,
            })
            .nth(4)
            .unwrap();
        let expected_offset_start = "a|b|c\n--|--|--\na|".len();
        assert_eq!(
            expected_offset_start..(expected_offset_start + 2),
            event_offset
        );
    }

    #[test]
    fn offset_iter_issue_378() {
        let event_offsets: Vec<_> = Parser::new("a [b](c) d")
            .into_offset_iter()
            .map(|(_ev, range)| range)
            .collect();
        let expected_offsets = vec![(0..10), (0..2), (2..8), (3..4), (2..8), (8..10), (0..10)];
        assert_eq!(expected_offsets, event_offsets);
    }

    #[test]
    fn offset_iter_issue_404() {
        let event_offsets: Vec<_> = Parser::new("###\n")
            .into_offset_iter()
            .map(|(_ev, range)| range)
            .collect();
        let expected_offsets = vec![(0..4), (0..4)];
        assert_eq!(expected_offsets, event_offsets);
    }

    // FIXME: add this one regression suite
    #[cfg(feature = "html")]
    #[test]
    fn link_def_at_eof() {
        let test_str = "[My site][world]\n\n[world]: https://vincentprouillet.com";
        let expected = "<p><a href=\"https://vincentprouillet.com\">My site</a></p>\n";

        let mut buf = String::new();
        crate::html::push_html(&mut buf, Parser::new(test_str));
        assert_eq!(expected, buf);
    }

    #[cfg(feature = "html")]
    #[test]
    fn no_footnote_refs_without_option() {
        let test_str = "a [^a]\n\n[^a]: yolo";
        let expected = "<p>a <a href=\"yolo\">^a</a></p>\n";

        let mut buf = String::new();
        crate::html::push_html(&mut buf, Parser::new(test_str));
        assert_eq!(expected, buf);
    }

    #[cfg(feature = "html")]
    #[test]
    fn ref_def_at_eof() {
        let test_str = "[test]:\\";
        let expected = "";

        let mut buf = String::new();
        crate::html::push_html(&mut buf, Parser::new(test_str));
        assert_eq!(expected, buf);
    }

    #[cfg(feature = "html")]
    #[test]
    fn ref_def_cr_lf() {
        let test_str = "[a]: /u\r\n\n[a]";
        let expected = "<p><a href=\"/u\">a</a></p>\n";

        let mut buf = String::new();
        crate::html::push_html(&mut buf, Parser::new(test_str));
        assert_eq!(expected, buf);
    }

    #[cfg(feature = "html")]
    #[test]
    fn no_dest_refdef() {
        let test_str = "[a]:";
        let expected = "<p>[a]:</p>\n";

        let mut buf = String::new();
        crate::html::push_html(&mut buf, Parser::new(test_str));
        assert_eq!(expected, buf);
    }

    #[test]
    fn broken_links_called_only_once() {
        for &(markdown, expected) in &[
            ("See also [`g()`][crate::g].", 1),
            ("See also [`g()`][crate::g][].", 1),
            ("[brokenlink1] some other node [brokenlink2]", 2),
        ] {
            let mut times_called = 0;
            let callback = &mut |_broken_link: BrokenLink<CowStr<'_>>| {
                times_called += 1;
                None
            };
            let parser =
                Parser::new_with_broken_link_callback(markdown, Options::empty(), Some(callback));
            for _ in parser {}
            assert_eq!(times_called, expected);
        }
    }

    #[test]
    fn simple_broken_link_callback() {
        let test_str = "This is a link w/o def: [hello][world]";
        let mut callback = |broken_link: BrokenLink<CowStr<'_>>| {
            assert_eq!("world", broken_link.reference.as_ref());
            assert_eq!(&test_str[broken_link.span], "[hello][world]");
            let url = "YOLO".into();
            let title = "SWAG".to_owned().into();
            Some((url, title))
        };
        let parser =
            Parser::new_with_broken_link_callback(test_str, Options::empty(), Some(&mut callback));
        let mut link_tag_count = 0;
        for (typ, url, title, id) in parser.filter_map(|event| match event {
            Event::Start(tag) => match tag {
                Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                } => Some((link_type, dest_url, title, id)),
                _ => None,
            },
            _ => None,
        }) {
            link_tag_count += 1;
            assert_eq!(typ, LinkType::ReferenceUnknown);
            assert_eq!(url.as_ref(), "YOLO");
            assert_eq!(title.as_ref(), "SWAG");
            assert_eq!(id.as_ref(), "world");
        }
        assert!(link_tag_count > 0);
    }

    #[test]
    fn code_block_kind_check_fenced() {
        let parser = Parser::new("hello\n```test\ntadam\n```");
        let mut found = 0;
        for (ev, _range) in parser.into_offset_iter() {
            match ev {
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(syntax))) => {
                    assert_eq!(syntax.as_ref(), "test");
                    found += 1;
                }
                _ => {}
            }
        }
        assert_eq!(found, 1);
    }

    #[test]
    fn code_block_kind_check_indented() {
        let parser = Parser::new("hello\n\n    ```test\n    tadam\nhello");
        let mut found = 0;
        for (ev, _range) in parser.into_offset_iter() {
            match ev {
                Event::Start(Tag::CodeBlock(CodeBlockKind::Indented)) => {
                    found += 1;
                }
                _ => {}
            }
        }
        assert_eq!(found, 1);
    }

    #[test]
    fn ref_defs() {
        let input = r###"[a B c]: http://example.com
[another]: https://google.com

text

[final ONE]: http://wikipedia.org
"###;
        let mut parser = Parser::new(input);

        assert!(parser.reference_definitions().get("a b c").is_some());
        assert!(parser.reference_definitions().get("nope").is_none());

        if let Some(_event) = parser.next() {
            // testing keys with shorter lifetimes than parser and its input
            let s = "final one".to_owned();
            let link_def = parser.reference_definitions().get(&s).unwrap();
            let span = &input[link_def.span.clone()];
            assert_eq!(span, "[final ONE]: http://wikipedia.org");
        }
    }

    #[test]
    fn common_lifetime_patterns_allowed<'b>() {
        let temporary_str = String::from("xyz");

        // Bind the closure's input lifetime to the enclosing function.
        let mut closure = |link: BrokenLink<CowStr<'b>>| Some(("#".into(), link.reference));

        fn function(link: BrokenLink<CowStr<'_>>) -> Option<(CowStr<'_>, CowStr<'_>)> {
            Some(("#".into(), link.reference))
        }

        for _ in Parser::new_with_broken_link_callback(
            "static lifetime",
            Options::empty(),
            Some(&mut closure),
        ) {}

        for _ in Parser::new_with_broken_link_callback(
            "static lifetime",
            Options::empty(),
            Some(&mut function),
        ) {}
        for _ in Parser::new_with_broken_link_callback(
            &temporary_str,
            Options::empty(),
            Some(&mut function),
        ) {}
    }

    #[test]
    fn inline_html_inside_blockquote() {
        // Regression for #960
        let input = "> <foo\n> bar>";
        let events: Vec<_> = Parser::new(input).collect();
        let expected = [
            Event::Start(Tag::BlockQuote(None)),
            Event::Start(Tag::Paragraph),
            Event::InlineHtml(CowStr::Boxed("<foo\nbar>".to_string().into())),
            Event::End(TagEnd::Paragraph),
            Event::End(TagEnd::BlockQuote(None)),
        ];
        assert_eq!(&events, &expected);
    }

    #[test]
    fn wikilink_has_pothole() {
        let input = "[[foo]] [[bar|baz]]";
        let events: Vec<_> = Parser::new_ext(input, Options::ENABLE_WIKILINKS).collect();
        let expected = [
            Event::Start(Tag::Paragraph),
            Event::Start(Tag::Link {
                link_type: LinkType::WikiLink { has_pothole: false },
                dest_url: CowStr::Borrowed("foo"),
                title: CowStr::Borrowed(""),
                id: CowStr::Borrowed(""),
            }),
            Event::Text(CowStr::Borrowed("foo")),
            Event::End(TagEnd::Link),
            Event::Text(CowStr::Borrowed(" ")),
            Event::Start(Tag::Link {
                link_type: LinkType::WikiLink { has_pothole: true },
                dest_url: CowStr::Borrowed("bar"),
                title: CowStr::Borrowed(""),
                id: CowStr::Borrowed(""),
            }),
            Event::Text(CowStr::Borrowed("baz")),
            Event::End(TagEnd::Link),
            Event::End(TagEnd::Paragraph),
        ];
        assert_eq!(&events, &expected);
    }

    #[test]
    fn wikilink_no_event_blowup() {
        // Regression: handle_wikilink reused an existing tree node as
        // the wikilink's child without terminating that node's `next`
        // chain.  The `next` chain threaded through the closing `]]`
        // into the content after the wikilink, so the EventIter
        // visited that tail content both as children (via child ptr)
        // and as siblings (via next ptr). Adversarial repeated
        // `[[|]]` patterns compounded exponentially.
        let one = "[[[[^(\n|]]]]=]]]]]]]]\n".repeat(1);
        let eight = "[[[[^(\n|]]]]=]]]]]]]]\n".repeat(8);

        let n1 = Parser::new_ext(&one, Options::ENABLE_WIKILINKS).count();
        let n8 = Parser::new_ext(&eight, Options::ENABLE_WIKILINKS).count();

        assert!(
            n8 <= n1 * 20,
            "Event count should scale linearly with input length; \
             got {n1} events for 1× and {n8} events for 8× ({}× ratio, expected ≤20×)",
            n8 / n1.max(1)
        );
    }

    #[test]
    fn issue_1154() {
        // If a `_` that can only close finds no opener, a `_` of the same length
        // that can also open won't find one either, so its bound can go up too.
        let mut tree = Tree::with_capacity(4);
        let mut stack = InlineStack::with_storage(Vec::new());
        for _ in 0..3 {
            let start = tree
                .append(Item {
                    start: 0,
                    end: 1,
                    body: ItemBody::Text {
                        backslash_escaped: false,
                    },
                })
                .unwrap();
            stack
                .push(InlineEl {
                    start,
                    count: 1,
                    run_length: 1,
                    c: b'*',
                    both: false,
                })
                .unwrap();
        }
        assert!(stack
            .find_match(&mut tree, b'_', 1, false)
            .unwrap()
            .is_none());
        assert_eq!(3, stack.get_lowerbound(b'_', 1, false));
        assert_eq!(3, stack.get_lowerbound(b'_', 1, true));
    }
}
