//! The first pass resolves all block structure, generating an AST. Within a block, items
//! are in a linear chain with potential inline markup identified.

use std::ops::Range;

use crate::backend::{Backend, HeadingValue};
use crate::parse::{
    scan_containers, Allocations, HeadingAttributes, Item, ItemBody, LinkDef,
    LINK_MAX_NESTED_PARENS,
};
use crate::source::{Bytes, Source, Text};
use crate::storage::{Dictionary, Sequence, Set};
use crate::text::TextBuffer;
use crate::tree::{ListFlagStorage, Tree, TreeIndex};
use crate::Options;
use crate::{linklabel::scan_link_label_rest, HeadingLevel};
use crate::{scanners::*, MetadataBlockKind};
use core::cell::Cell;

use crate::parse::ListFlags;

/// Resolve block structure and retain inline candidates.
pub(crate) fn run_first_pass<'a, B: Backend>(
    text: Text<'a, B::Input>,
    options: Options,
    backend: B,
) -> Result<(Tree<Item, B::Nodes, B::Spine>, Allocations<'a, B>), B::Error> {
    let lookup_table = &create_lut(&options);
    let first_pass = FirstPass {
        text,
        tree: backend.tree(text.len())?,
        begin_list_item: None,
        last_line_blank: false,
        allocs: Allocations::new(backend)?,
        options,
        lookup_table,
        brace_context_next: 0,
        brace_context_stack: Vec::new(),
    };
    first_pass.run()
}

// Each level of brace nesting adds another entry to a hash table.
// To limit the amount of memory consumed, do not create a new brace
// context beyond some amount deep.
//
// There are actually two limits at play here: this one,
// and the one where the maximum amount of distinct contexts passes
// the 255 item limit imposed by using `u8`. When over 255 distinct
// contexts are created, it wraps around, while this one instead makes it
// saturate, which is a better behavior.
const MATH_BRACE_CONTEXT_MAX_NESTING: usize = 25;

/// State for the first parsing pass.
struct FirstPass<'a, 'b, B: Backend> {
    text: Text<'a, B::Input>,
    tree: Tree<Item, B::Nodes, B::Spine>,
    begin_list_item: Option<usize>,
    last_line_blank: bool,
    allocs: Allocations<'a, B>,
    options: Options,
    lookup_table: &'b LookupTable,
    /// Math environment brace nesting.
    brace_context_stack: Vec<u8>,
    brace_context_next: usize,
}

impl<'a, 'b, B: Backend> FirstPass<'a, 'b, B> {
    fn run(mut self) -> Result<(Tree<Item, B::Nodes, B::Spine>, Allocations<'a, B>), B::Error> {
        let mut ix = 0;
        while ix < self.text.len() {
            ix = self.parse_block(ix)?;
        }
        while self.tree.spine_len() > 0 {
            self.pop(ix)?;
        }
        Ok((self.tree, self.allocs))
    }

    fn scan_definition_list_marker(
        &self,
        line_start: &mut LineStart<'_, B::Input>,
        outer_indent: usize,
    ) -> Result<Option<(usize, TreeIndex, Option<TreeIndex>, Item)>, B::Error> {
        if !self.options.contains(Options::ENABLE_DEFINITION_LIST) {
            return Ok(None);
        }
        let Some(index) = self.tree.cur() else {
            return Ok(None);
        };
        let node = self.tree.node(index)?;
        if !matches!(
            node.item.body,
            ItemBody::Paragraph
                | ItemBody::TightParagraph
                | ItemBody::MaybeDefinitionListTitle
                | ItemBody::DefinitionListDefinition(_)
        ) {
            return Ok(None);
        }
        Ok(line_start
            .scan_definition_list_definition_marker_with_indent(outer_indent)?
            .map(|indent| (indent, index, node.child, node.item.clone())))
    }

    /// Returns offset after block.
    fn parse_block(&mut self, mut start_ix: usize) -> Result<usize, B::Error> {
        let bytes = self.text;
        let mut line_start = LineStart::new(bytes.slice(start_ix..));

        // math spans and their braces are tracked only within a single block
        self.brace_context_stack.clear();
        self.brace_context_next = 0;

        let i = scan_containers(&self.tree, &mut line_start, self.options, true)?;
        for _ in i..self.tree.spine_len() {
            self.pop(start_ix)?;
        }

        if self.options.contains(Options::ENABLE_OLD_FOOTNOTES) {
            // finish footnote if it's still open and was preceded by blank line
            if let Some(node_ix) = self.tree.peek_up()? {
                if matches!(
                    self.tree.node(node_ix)?.item.body,
                    ItemBody::FootnoteDefinition(..)
                ) {
                    if self.last_line_blank {
                        self.pop(start_ix)?;
                    }
                }
            }
        }

        // Process new containers
        loop {
            let save = line_start.clone();
            let outer_indent = line_start.scan_space_upto(4)?;
            if outer_indent >= 4 {
                line_start = save;
                break;
            }
            if self.options.contains(Options::ENABLE_FOOTNOTES) {
                // Footnote definitions
                let container_start = start_ix + line_start.bytes_scanned();
                if let Some(bytecount) = self.parse_footnote(container_start)? {
                    start_ix = container_start + bytecount;
                    if self.options.contains(Options::ENABLE_OLD_FOOTNOTES) {
                        // gfm footnotes need indented, but old footnotes don't
                        // handle this, old footnotes treat the next line as part of the current line
                        start_ix += scan_blank_line(bytes.slice(start_ix..))?.unwrap_or(0);
                    }
                    line_start = LineStart::new(bytes.slice(start_ix..));
                    continue;
                }
            }
            let container_start = start_ix + line_start.bytes_scanned();
            if let Some((ch, index, indent)) =
                line_start.scan_list_marker_with_indent(outer_indent)?
            {
                let after_marker_index = start_ix + line_start.bytes_scanned();
                self.continue_list(container_start - outer_indent, ch, index)?;
                self.tree.append(Item {
                    start: container_start - outer_indent,
                    end: after_marker_index, // will get updated later if item not empty
                    body: ItemBody::ListItem(indent),
                })?;
                self.tree.push()?;
                if let Some(n) = scan_blank_line(bytes.slice(after_marker_index..))? {
                    self.begin_list_item = Some(after_marker_index + n);
                    return Ok(after_marker_index + n);
                }
                if self.options.contains(Options::ENABLE_TASKLISTS) {
                    let task_list_marker =
                        line_start.scan_task_list_marker()?.map(|is_checked| Item {
                            start: after_marker_index,
                            end: start_ix + line_start.bytes_scanned(),
                            body: ItemBody::TaskListMarker(is_checked),
                        });
                    if let Some(task_list_marker) = task_list_marker {
                        if let Some(n) = scan_blank_line(bytes.slice(task_list_marker.end..))? {
                            let end = task_list_marker.end;
                            self.tree.append(task_list_marker)?;
                            self.begin_list_item = Some(end + n);
                            return Ok(end + n);
                        } else {
                            line_start.scan_all_space()?;
                            let ix = start_ix + line_start.bytes_scanned();
                            return self.parse_paragraph(ix, Some(task_list_marker));
                        }
                    }
                }
            } else if let Some((indent, index, child, item)) =
                self.scan_definition_list_marker(&mut line_start, outer_indent)?
            {
                match item.body {
                    ItemBody::Paragraph | ItemBody::TightParagraph => {
                        self.tree.update(index, |node| {
                            node.item.body =
                                ItemBody::DefinitionList(Cell::new(ListFlags::IS_TIGHT));
                        })?;
                        let Item { start, end, .. } = item;
                        let title_idx = self.tree.create_node(Item {
                            start,
                            end, // will get updated later if item not empty
                            body: ItemBody::DefinitionListTitle,
                        })?;
                        {
                            let value = child;
                            self.tree.update(title_idx, |node| node.child = value)?;
                        };
                        {
                            let value = Some(title_idx);
                            self.tree.update(index, |node| node.child = value)?;
                        };
                        self.tree.push()?;
                    }
                    ItemBody::MaybeDefinitionListTitle => {
                        self.tree.update(index, |node| {
                            node.item.body = ItemBody::DefinitionListTitle;
                        })?;
                    }
                    ItemBody::DefinitionListDefinition(_) => {}
                    _ => unreachable!(),
                }
                let after_marker_index = start_ix + line_start.bytes_scanned();
                // `outer_indent` is columns, not bytes. Subtracting it from a
                // source offset can land inside a multibyte character (issue 1142).
                self.tree.append(Item {
                    start: start_ix + save.bytes_scanned(),
                    end: after_marker_index, // will get updated later if item not empty
                    body: ItemBody::DefinitionListDefinition(indent),
                })?;
                if let Some(parent) = self.tree.peek_up()? {
                    if let ItemBody::DefinitionList(list_flags) = &self.tree.node(parent)?.item.body
                    {
                        if self.last_line_blank {
                            self.tree
                                .set_list_flags(parent, list_flags.get() & !ListFlags::IS_TIGHT)?;
                            self.last_line_blank = false;
                        }
                    }
                }
                self.tree.push()?;
                if let Some(n) = scan_blank_line(bytes.slice(after_marker_index..))? {
                    self.begin_list_item = Some(after_marker_index + n);
                    return Ok(after_marker_index + n);
                }
            } else if line_start.scan_blockquote_marker()? {
                let kind = if self.options.contains(Options::ENABLE_GFM) {
                    line_start.scan_blockquote_tag()?
                } else {
                    None
                };
                self.finish_list(start_ix)?;
                self.tree.append(Item {
                    start: container_start,
                    end: 0, // will get set later
                    body: ItemBody::BlockQuote(kind),
                })?;
                self.tree.push()?;
                if kind.is_some() {
                    // blockquote tag leaves us at the end of the line
                    // we need to scan through all the container syntax for the next line
                    // and break out if we can't re-scan all of them
                    let ix = start_ix + line_start.bytes_scanned();
                    let mut lazy_line_start = LineStart::new(bytes.slice(ix..));
                    let tree_position =
                        scan_containers(&self.tree, &mut lazy_line_start, self.options, false)?;
                    let current_container = tree_position == self.tree.spine_len();
                    if !lazy_line_start.scan_space(4)?
                        && self.scan_paragraph_interrupt(
                            self.text.slice(ix + lazy_line_start.bytes_scanned()..),
                            current_container,
                            tree_position,
                        )?
                    {
                        return Ok(ix);
                    } else {
                        // blockquote tags act as if they were nested in a paragraph
                        // so you can lazily continue the imaginary paragraph off of them
                        line_start = lazy_line_start;
                        line_start.scan_all_space()?;
                        start_ix = ix;
                        break;
                    }
                }
            } else {
                line_start = save;
                break;
            }
        }

        let ix = start_ix + line_start.bytes_scanned();

        if let Some(n) = scan_blank_line(bytes.slice(ix..))? {
            if let Some(node_ix) = self.tree.peek_up()? {
                match self.tree.body_snapshot(node_ix)? {
                    ItemBody::BlockQuote(..) => (),
                    ItemBody::ListItem(_) | ItemBody::DefinitionListDefinition(_)
                        if self.begin_list_item.is_some() =>
                    {
                        self.last_line_blank = true;
                        // Empty items can continue, but cannot indent nested content.
                        self.tree
                            .update(node_ix, |node| match &mut node.item.body {
                                ItemBody::ListItem(indent)
                                | ItemBody::DefinitionListDefinition(indent) => *indent = 0,
                                _ => unreachable!(),
                            })?;
                    }
                    _ => {
                        self.last_line_blank = true;
                    }
                }
            } else {
                self.last_line_blank = true;
            }
            return Ok(ix + n);
        }

        // Save `remaining_space` here to avoid needing to backtrack `line_start` for HTML blocks
        let remaining_space = line_start.remaining_space();

        let indent = line_start.scan_space_upto(4)?;
        if indent == 4 {
            self.finish_list(start_ix)?;
            let ix = start_ix + line_start.bytes_scanned();
            let remaining_space = line_start.remaining_space();
            return self.parse_indented_code_block(ix, remaining_space);
        }

        let ix = start_ix + line_start.bytes_scanned();

        // metadata blocks cannot be indented
        if indent == 0 {
            if let Some((_n, metadata_block_ch)) = scan_metadata_block(
                bytes.slice(ix..),
                self.options
                    .contains(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS),
                self.options
                    .contains(Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS),
            )? {
                self.finish_list(start_ix)?;
                return self.parse_metadata_block(ix, metadata_block_ch);
            }
        }

        // HTML Blocks
        if bytes.byte(ix)? == b'<' {
            // Types 1-5 are all detected by one function and all end with the same
            // pattern
            if let Some(html_end_tag) = get_html_end_tag(bytes.slice((ix + 1)..))? {
                self.finish_list(start_ix)?;
                return self.parse_html_block_type_1_to_5(
                    ix,
                    html_end_tag,
                    remaining_space,
                    indent,
                );
            }

            // Detect type 6
            if starts_html_block_type_6(bytes.slice((ix + 1)..))? {
                self.finish_list(start_ix)?;
                return self.parse_html_block_type_6_or_7(ix, remaining_space, indent);
            }

            // Detect type 7
            if let Some(_html_bytes) = scan_html_type_7(bytes.slice(ix..), &self.allocs.backend)? {
                self.finish_list(start_ix)?;
                return self.parse_html_block_type_6_or_7(ix, remaining_space, indent);
            }
        }

        if let Ok(n) = scan_hrule(bytes.slice(ix..))? {
            self.finish_list(start_ix)?;
            return self.parse_hrule(n, ix);
        }

        if let Some(atx_size) = scan_atx_heading(bytes.slice(ix..))? {
            self.finish_list(start_ix)?;
            return self.parse_atx_heading(ix, atx_size);
        }

        if let Some((n, fence_ch)) = scan_code_fence(bytes.slice(ix..))? {
            self.finish_list(start_ix)?;
            return self.parse_fenced_code_block(ix, indent, fence_ch, n);
        }

        while let Some((bytecount, label, link_def)) =
            self.parse_refdef_total(start_ix + line_start.bytes_scanned())?
        {
            self.allocs.refdefs.insert_first(label, link_def)?;
            let container_start = start_ix + line_start.bytes_scanned();
            let mut ix = container_start + bytecount;
            // Refdefs act as if they were contained within a paragraph, for purposes of lazy
            // continuations. For example:
            //
            // ```
            // > [foo]: http://example.com
            // bar
            // ```
            //
            // is equivalent to
            //
            // ```
            // > bar
            //
            // [foo]: http://example.com
            // ```
            if let Some(nl) = scan_blank_line(bytes.slice(ix..))? {
                ix += nl;
            } else {
                self.finish_list(start_ix)?;
                return Ok(ix);
            }
            if let Some(lazy_line_start) =
                self.scan_next_line_or_lazy_continuation(bytes.slice(ix..))?
            {
                line_start = lazy_line_start;
                start_ix = ix;
            } else {
                self.finish_list(start_ix)?;
                return Ok(ix);
            }
        }

        let ix = start_ix + line_start.bytes_scanned();

        self.parse_paragraph(ix, None)
    }

    /// footnote definitions and GFM quote markers can be "interrupted"
    /// like paragraphs, but otherwise can't have other blocks after them.
    ///
    /// Call this at the end of the line to parse that. If it succeeeds,
    /// this returns the LineStart for the new line.
    fn scan_next_line_or_lazy_continuation<'input>(
        &mut self,
        bytes: Text<'input, B::Input>,
    ) -> Result<Option<LineStart<'input, B::Input>>, B::Error> {
        let mut line_start = LineStart::new(bytes);
        let tree_position = scan_containers(&self.tree, &mut line_start, self.options, false)?;
        let current_container = tree_position == self.tree.spine_len();
        if (!line_start.scan_space(4)?
            && self.scan_paragraph_interrupt(
                self.text
                    .slice(self.text.len() - bytes.len() + line_start.bytes_scanned()..),
                current_container,
                tree_position,
            )?)
            || scan_blank_line(bytes.slice(line_start.bytes_scanned()..))?.is_some()
        {
            Ok(None)
        } else {
            line_start.scan_all_space()?;
            Ok(Some(line_start))
        }
    }

    /// Returns the offset of the first line after the table.
    /// Assumptions: current focus is a table element and the table header
    /// matches the separator line (same number of columns).
    fn parse_table(
        &mut self,
        table_cols: usize,
        head_start: usize,
        body_start: usize,
    ) -> Result<Option<usize>, B::Error> {
        // filled empty cells are limited to protect against quadratic growth
        // https://github.com/raphlinus/pulldown-cmark/issues/832
        let mut missing_empty_cells = 0;
        let Some((_sep_start, thead_ix)) =
            self.parse_table_row_inner(head_start, table_cols, &mut missing_empty_cells)?
        else {
            return Ok(None);
        };
        {
            let value = ItemBody::TableHead;
            self.tree.update(thead_ix, |node| node.item.body = value)?;
        };

        // parse body
        let mut ix = body_start;
        while let Some((next_ix, _row_ix)) =
            self.parse_table_row(ix, table_cols, &mut missing_empty_cells)?
        {
            ix = next_ix;
        }

        self.pop(ix)?;
        Ok(Some(ix))
    }

    /// Call this when containers are taken care of.
    /// Returns bytes scanned, row_ix
    fn parse_table_row_inner(
        &mut self,
        mut ix: usize,
        row_cells: usize,
        missing_empty_cells: &mut usize,
    ) -> Result<Option<(usize, TreeIndex)>, B::Error> {
        // Limit to prevent a malicious input from causing a denial of service.
        const MAX_AUTOCOMPLETED_CELLS: usize = 1 << 18; // = 0x40000

        let bytes = self.text;
        let mut cells = 0;
        let mut final_cell_ix = None;

        let old_cur = self.tree.cur();
        let row_ix = self.tree.append(Item {
            start: ix,
            end: 0, // set at end of this function
            body: ItemBody::TableRow,
        })?;
        self.tree.push()?;

        loop {
            ix += scan_ch(bytes.slice(ix..), b'|')?;
            let start_ix = ix;
            ix += scan_whitespace_no_nl(bytes.slice(ix..))?;

            if let Some(eol_bytes) = scan_eol(bytes.slice(ix..))? {
                ix += eol_bytes;
                break;
            }

            let cell_ix = self.tree.append(Item {
                start: start_ix,
                end: ix,
                body: ItemBody::TableCell,
            })?;
            self.tree.push()?;
            let (next_ix, _brk) = self.parse_line(ix, None, TableParseMode::Active)?;

            {
                let value = next_ix;
                self.tree.update(cell_ix, |node| node.item.end = value)?;
            };
            self.tree.pop()?;

            ix = next_ix;
            cells += 1;

            if cells == row_cells {
                final_cell_ix = Some(cell_ix);
            }
        }

        if let (Some(cur), 0) = (old_cur, cells) {
            self.pop(ix)?;
            {
                let value = None;
                self.tree.update(cur, |node| node.next = value)?;
            };
            return Ok(None);
        }

        // fill empty cells if needed
        // note: this is where GFM and commonmark-extra diverge. we follow
        // GFM here
        for _ in cells..row_cells {
            if *missing_empty_cells >= MAX_AUTOCOMPLETED_CELLS {
                return Ok(None);
            }
            *missing_empty_cells += 1;
            self.tree.append(Item {
                start: ix,
                end: ix,
                body: ItemBody::TableCell,
            })?;
        }

        // drop excess cells
        if let Some(cell_ix) = final_cell_ix {
            {
                let value = None;
                self.tree.update(cell_ix, |node| node.next = value)?;
            };
        }

        self.pop(ix)?;

        Ok(Some((ix, row_ix)))
    }

    /// Returns first offset after the row and the tree index of the row.
    fn parse_table_row(
        &mut self,
        mut ix: usize,
        row_cells: usize,
        missing_empty_cells: &mut usize,
    ) -> Result<Option<(usize, TreeIndex)>, B::Error> {
        let bytes = self.text;
        let mut line_start = LineStart::new(bytes.slice(ix..));
        let tree_position = scan_containers(&self.tree, &mut line_start, self.options, false)?;
        let current_container = tree_position == self.tree.spine_len();
        if !current_container {
            return Ok(None);
        }
        line_start.scan_all_space()?;
        ix += line_start.bytes_scanned();
        if scan_paragraph_interrupt_no_table(
            self.text.slice(ix..),
            current_container,
            self.options.contains(Options::ENABLE_FOOTNOTES),
            self.options.contains(Options::ENABLE_DEFINITION_LIST),
            &self.tree,
            tree_position,
            &self.allocs.backend,
        )? {
            return Ok(None);
        }

        let Some((ix, row_ix)) = self.parse_table_row_inner(ix, row_cells, missing_empty_cells)?
        else {
            return Ok(None);
        };
        Ok(Some((ix, row_ix)))
    }

    /// Returns offset of line start after paragraph.
    fn parse_paragraph(
        &mut self,
        start_ix: usize,
        tasklist_marker: Option<Item>,
    ) -> Result<usize, B::Error> {
        let in_definition_list = match self.tree.peek_up()? {
            Some(index) => matches!(
                self.tree.node(index)?.item.body,
                ItemBody::DefinitionList(_)
            ),
            None => false,
        };
        let body = if in_definition_list {
            let follows_definition = match self.tree.cur() {
                Some(index) => matches!(
                    self.tree.node(index)?.item.body,
                    ItemBody::DefinitionListDefinition(_)
                ),
                None => true,
            };
            if follows_definition {
                // blank lines between the previous definition and this one don't count
                self.last_line_blank = false;
                ItemBody::MaybeDefinitionListTitle
            } else {
                self.finish_list(start_ix)?;
                ItemBody::Paragraph
            }
        } else {
            self.finish_list(start_ix)?;
            ItemBody::Paragraph
        };
        let node_ix = self.tree.append(Item {
            start: start_ix,
            end: 0, // will get set later
            body: body.clone(),
        })?;
        self.tree.push()?;

        if let Some(item) = tasklist_marker {
            self.tree.append(item)?;
        }

        let bytes = self.text;
        let mut ix = start_ix;
        loop {
            let scan_mode = if self.options.contains(Options::ENABLE_TABLES) && ix == start_ix {
                TableParseMode::Scan
            } else {
                TableParseMode::Disabled
            };
            let (next_ix, brk) = self.parse_line(ix, None, scan_mode)?;

            // break out when we find a table
            if let Some(Item {
                body: ItemBody::Table(alignment_ix),
                ..
            }) = brk
            {
                let table_cols = self.allocs.alignment_len(alignment_ix)?;
                {
                    let value = ItemBody::Table(alignment_ix);
                    self.tree.update(node_ix, |node| node.item.body = value)?;
                };
                // this clears out any stuff we may have appended - but there may
                // be a cleaner way
                {
                    let value = None;
                    self.tree.update(node_ix, |node| node.child = value)?;
                };
                self.tree.pop()?;
                if body == ItemBody::MaybeDefinitionListTitle {
                    self.finish_list(ix)?;
                }
                self.tree.push()?;
                if let Some(ix) = self.parse_table(table_cols, ix, next_ix)? {
                    return Ok(ix);
                }
            }

            ix = next_ix;
            let mut line_start = LineStart::new(bytes.slice(ix..));
            let tree_position = scan_containers(&self.tree, &mut line_start, self.options, false)?;
            let current_container = tree_position == self.tree.spine_len();
            let trailing_backslash_pos = match brk {
                Some(Item {
                    start,
                    body: ItemBody::HardBreak(true),
                    ..
                }) if bytes.byte(start)? == b'\\' => Some(start),
                _ => None,
            };
            if !line_start.scan_space(4)? {
                let ix_new = ix + line_start.bytes_scanned();
                if current_container {
                    if let Some(ix_setext) = self.parse_setext_heading(
                        ix_new,
                        node_ix,
                        trailing_backslash_pos.is_some(),
                    )? {
                        if let Some(pos) = trailing_backslash_pos {
                            self.tree.append_text(pos, pos + 1, false)?;
                        }
                        self.pop(ix_setext)?;
                        if body == ItemBody::MaybeDefinitionListTitle {
                            self.finish_list(ix)?;
                        }
                        return Ok(ix_setext);
                    }
                }
                // first check for non-empty lists, then for other interrupts
                let suffix = self.text.slice(ix_new..);
                if self.scan_paragraph_interrupt(suffix, current_container, tree_position)? {
                    if let Some(pos) = trailing_backslash_pos {
                        self.tree.append_text(pos, pos + 1, false)?;
                    }
                    break;
                }
            }
            line_start.scan_all_space()?;
            if line_start.is_at_eol()? {
                if let Some(pos) = trailing_backslash_pos {
                    self.tree.append_text(pos, pos + 1, false)?;
                }
                break;
            }
            ix = next_ix + line_start.bytes_scanned();
            if let Some(item) = brk {
                self.tree.append(item)?;
            }
        }

        self.pop(ix)?;
        Ok(ix)
    }

    /// Returns end ix of setext_heading on success.
    fn parse_setext_heading(
        &mut self,
        ix: usize,
        node_ix: TreeIndex,
        has_trailing_content: bool,
    ) -> Result<Option<usize>, B::Error> {
        let bytes = self.text;
        let Some((n, level)) = scan_setext_heading(bytes.slice(ix..))? else {
            return Ok(None);
        };
        let mut attrs = None;

        if let Some(cur_ix) = self.tree.cur() {
            let parent_ix = self.tree.peek_up()?.unwrap();
            let header_start = self.tree.node(parent_ix)?.item.start;
            // The parent end is unset until its container closes.
            // Use the last child's end for the heading range.
            let header_end = self.tree.node(cur_ix)?.item.end;

            // extract the trailing attribute block
            let (content_end, attrs_) =
                self.extract_and_parse_heading_attribute_block(header_start, header_end)?;
            attrs = attrs_;

            // strip trailing whitespace
            let new_end = if has_trailing_content {
                content_end
            } else {
                let mut last_line_start = header_start;
                if attrs.is_some() {
                    loop {
                        let next_line_start = last_line_start
                            + scan_nextline(bytes.slice(last_line_start..content_end))?;
                        if next_line_start >= content_end {
                            break;
                        }
                        let mut line_start =
                            LineStart::new(bytes.slice(next_line_start..content_end));
                        if scan_containers(&self.tree, &mut line_start, self.options, false)?
                            != self.tree.spine_len()
                        {
                            break;
                        }
                        last_line_start = next_line_start + line_start.bytes_scanned();
                    }
                }
                let trailing_ws = scan_rev_while(
                    bytes.slice(last_line_start..content_end),
                    is_ascii_whitespace_no_nl,
                )?;
                content_end - trailing_ws
            };

            if attrs.is_some() {
                // remove trailing block attributes
                self.tree.truncate_siblings(new_end)?;
            }

            if let Some(cur_ix) = self.tree.cur() {
                {
                    let value = new_end;
                    self.tree.update(cur_ix, |node| node.item.end = value)?;
                };
            }
        }

        {
            let value = ItemBody::Heading(
                level,
                attrs
                    .map(|attrs| self.allocs.allocate_heading(attrs))
                    .transpose()?,
            );
            self.tree.update(node_ix, |node| node.item.body = value)?;
        };

        Ok(Some(ix + n))
    }

    /// Parse a line of input, appending text and items to tree.
    ///
    /// Returns: index after line and an item representing the break.
    fn parse_line(
        &mut self,
        start: usize,
        end: Option<usize>,
        mode: TableParseMode,
    ) -> Result<(usize, Option<Item>), B::Error> {
        let bytes = self.text;
        let bytes = match end {
            Some(end) => bytes.slice(..end),
            None => bytes,
        };
        let bytes_len = bytes.len();
        let mut pipes = 0;
        let mut last_pipe_ix = start;
        let mut begin_text = start;
        let mut backslash_escaped = false;

        let (final_ix, brk) =
            iterate_special_bytes(self.lookup_table, bytes, start, |ix, byte| {
                Ok(match byte {
                    b'\n' | b'\r' => {
                        if let TableParseMode::Active = mode {
                            return Ok(LoopInstruction::BreakAtWith(ix, None));
                        }

                        let mut i = ix;
                        let eol_bytes = scan_eol(bytes.slice(ix..))?.unwrap();

                        let end_ix = ix + eol_bytes;
                        let trailing_backslashes =
                            scan_rev_while(bytes.slice(..ix), |b| b == b'\\')?;
                        if trailing_backslashes % 2 == 1 && end_ix < bytes_len {
                            i -= 1;
                            self.tree.append_text(begin_text, i, backslash_escaped)?;
                            backslash_escaped = false;
                            return Ok(LoopInstruction::BreakAtWith(
                                end_ix,
                                Some(Item {
                                    start: i,
                                    end: end_ix,
                                    body: ItemBody::HardBreak(true),
                                }),
                            ));
                        }

                        if mode == TableParseMode::Scan && pipes > 0 {
                            // check if we may be parsing a table
                            let next_line_ix = ix + eol_bytes;
                            let mut line_start = LineStart::new(bytes.slice(next_line_ix..));
                            if scan_containers(&self.tree, &mut line_start, self.options, false)?
                                == self.tree.spine_len()
                            {
                                let table_head_ix = next_line_ix + line_start.bytes_scanned();
                                let mut alignment = self.allocs.backend.columns()?;
                                let table_head_bytes =
                                    scan_table_head(bytes.slice(table_head_ix..), |value| {
                                        alignment.push(value)
                                    })?;

                                if table_head_bytes > 0 {
                                    // computing header count from number of pipes
                                    let header_count =
                                        count_header_cols(bytes, pipes, start, last_pipe_ix)?;

                                    // make sure they match the number of columns we find in separator line
                                    if alignment.len() == header_count {
                                        let alignment_ix =
                                            self.allocs.allocate_alignment(alignment)?;
                                        let end_ix = table_head_ix + table_head_bytes;
                                        return Ok(LoopInstruction::BreakAtWith(
                                            end_ix,
                                            Some(Item {
                                                start: i,
                                                end: end_ix, // must update later
                                                body: ItemBody::Table(alignment_ix),
                                            }),
                                        ));
                                    }
                                }
                                alignment.truncate(0)?;
                            }
                        }

                        let trailing_whitespace =
                            scan_rev_while(bytes.slice(..ix), is_ascii_whitespace_no_nl)?;
                        if trailing_whitespace >= 2 {
                            i -= trailing_whitespace;
                            self.tree.append_text(begin_text, i, backslash_escaped)?;
                            backslash_escaped = false;
                            return Ok(LoopInstruction::BreakAtWith(
                                end_ix,
                                Some(Item {
                                    start: i,
                                    end: end_ix,
                                    body: ItemBody::HardBreak(false),
                                }),
                            ));
                        }

                        self.tree.append_text(
                            begin_text,
                            ix - trailing_whitespace,
                            backslash_escaped,
                        )?;
                        backslash_escaped = false;

                        LoopInstruction::BreakAtWith(
                            end_ix,
                            Some(Item {
                                start: i,
                                end: end_ix,
                                body: ItemBody::SoftBreak,
                            }),
                        )
                    }
                    b'\\' => {
                        if ix + 1 < bytes_len && is_ascii_punctuation(bytes.byte(ix + 1)?) {
                            self.tree.append_text(begin_text, ix, backslash_escaped)?;
                            if bytes.byte(ix + 1)? == b'`' {
                                let count = 1 + scan_ch_repeat(bytes.slice((ix + 2)..), b'`')?;
                                self.tree.append(Item {
                                    start: ix + 1,
                                    end: ix + count + 1,
                                    body: ItemBody::MaybeCode(count, true),
                                })?;
                                begin_text = ix + 1 + count;
                                backslash_escaped = false;
                                LoopInstruction::ContinueAndSkip(count)
                            } else if bytes.byte(ix + 1)? == b'|' && TableParseMode::Active == mode
                            {
                                // Yeah, it's super weird that backslash escaped pipes in tables aren't "real"
                                // backslash escapes.
                                //
                                // This tree structure is intended for the benefit of inline analysis, and it
                                // is supposed to operate as-if backslash escaped pipes were stripped out in a
                                // separate pass.
                                begin_text = ix + 1;
                                backslash_escaped = false;
                                LoopInstruction::ContinueAndSkip(1)
                            } else if ix + 2 < bytes_len
                                && bytes.byte(ix + 1)? == b'\\'
                                && bytes.byte(ix + 2)? == b'|'
                                && TableParseMode::Active == mode
                            {
                                // To parse `\\|`, discard the backslashes and parse the `|` that follows it.
                                begin_text = ix + 2;
                                backslash_escaped = true;
                                LoopInstruction::ContinueAndSkip(2)
                            } else {
                                begin_text = ix + 1;
                                backslash_escaped = true;
                                LoopInstruction::ContinueAndSkip(1)
                            }
                        } else {
                            LoopInstruction::ContinueAndSkip(0)
                        }
                    }
                    c @ b'*' | c @ b'_' | c @ b'~' | c @ b'^' => {
                        let string_suffix = self.text.slice(ix..);
                        let count = 1 + scan_ch_repeat(string_suffix.slice(1..), c)?;
                        let can_open = delim_run_can_open(
                            self.text.slice(start..),
                            string_suffix,
                            count,
                            ix - start,
                            mode,
                        )?;
                        let can_close = delim_run_can_close(
                            self.text.slice(start..),
                            string_suffix,
                            count,
                            ix - start,
                            mode,
                        )?;
                        let is_valid_seq = (c != b'~' || count <= 2) || (c == b'~' && count == 2);

                        if (can_open || can_close) && is_valid_seq {
                            self.tree.append_text(begin_text, ix, backslash_escaped)?;
                            backslash_escaped = false;
                            for i in 0..count {
                                self.tree.append(Item {
                                    start: ix + i,
                                    end: ix + i + 1,
                                    body: ItemBody::MaybeEmphasis(count - i, can_open, can_close),
                                })?;
                            }
                            begin_text = ix + count;
                        }
                        LoopInstruction::ContinueAndSkip(count - 1)
                    }
                    b'$' => {
                        let can_open = !bytes.get(ix + 1)?.map_or(true, is_ascii_whitespace);
                        let can_close = ix > start && !is_ascii_whitespace(bytes.byte(ix - 1)?);

                        // 0xFFFF_FFFF... represents the root brace context. Using None would require
                        // storing Option<u8>, which is bigger than u8.
                        //
                        // These shouldn't conflict unless you have 255 levels of nesting, which is
                        // past the intended limit anyway.
                        //
                        // Unbalanced braces will cause the root to be changed, which is why it gets
                        // stored here.
                        let brace_context =
                            if self.brace_context_stack.len() > MATH_BRACE_CONTEXT_MAX_NESTING {
                                self.brace_context_next as u8
                            } else {
                                self.brace_context_stack.last().copied().unwrap_or_else(|| {
                                    self.brace_context_stack.push(!0);
                                    !0
                                })
                            };

                        self.tree.append_text(begin_text, ix, backslash_escaped)?;
                        self.tree.append(Item {
                            start: ix,
                            end: ix + 1,
                            body: ItemBody::MaybeMath(can_open, can_close, brace_context),
                        })?;
                        begin_text = ix + 1;
                        LoopInstruction::ContinueAndSkip(0)
                    }
                    b'{' => {
                        if self.brace_context_stack.len() == MATH_BRACE_CONTEXT_MAX_NESTING {
                            self.brace_context_stack.push(self.brace_context_next as u8);
                            self.brace_context_next = MATH_BRACE_CONTEXT_MAX_NESTING;
                        } else if self.brace_context_stack.len() > MATH_BRACE_CONTEXT_MAX_NESTING {
                            // When we reach the limit of nesting, switch from actually matching
                            // braces to just counting them.
                            self.brace_context_next += 1;
                        } else if !self.brace_context_stack.is_empty() {
                            // Store nothing if no math environment has been reached yet.
                            self.brace_context_stack.push(self.brace_context_next as u8);
                            self.brace_context_next += 1;
                        }
                        LoopInstruction::ContinueAndSkip(0)
                    }
                    b'}' => {
                        if let &mut [ref mut top_level_context] = &mut self.brace_context_stack[..]
                        {
                            // Unbalanced Braces
                            //
                            // The initial, root top-level brace context is -1, but this is changed whenever an unbalanced
                            // close brace is encountered:
                            //
                            //     This is not a math environment: $}$
                            //     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^|^
                            //     -1                               |-2
                            //
                            // To ensure this can't get parsed as math, each side of the unbalanced
                            // brace is an irreversibly separate brace context. As long as the math
                            // environment itself contains balanced braces, they should share a top level context.
                            //
                            //     Math environment contains 2+2: $}$2+2$
                            //                                       ^^^ this is a math environment
                            *top_level_context = top_level_context.wrapping_sub(1);
                        } else if self.brace_context_stack.len() > MATH_BRACE_CONTEXT_MAX_NESTING {
                            // When we exceed 25 levels of nesting, switch from accurately balancing braces
                            // to just counting them. When we dip back below the limit, switch back.
                            if self.brace_context_next <= MATH_BRACE_CONTEXT_MAX_NESTING {
                                self.brace_context_stack.pop();
                            } else {
                                self.brace_context_next -= 1;
                            }
                        } else {
                            self.brace_context_stack.pop();
                        }
                        LoopInstruction::ContinueAndSkip(0)
                    }
                    b'`' => {
                        self.tree.append_text(begin_text, ix, backslash_escaped)?;
                        backslash_escaped = false;
                        let count = 1 + scan_ch_repeat(bytes.slice((ix + 1)..), b'`')?;
                        self.tree.append(Item {
                            start: ix,
                            end: ix + count,
                            body: ItemBody::MaybeCode(count, false),
                        })?;
                        begin_text = ix + count;
                        LoopInstruction::ContinueAndSkip(count - 1)
                    }
                    b'<' if bytes.get(ix + 1)? != Some(b'\\') => {
                        self.tree.append_text(begin_text, ix, backslash_escaped)?;
                        backslash_escaped = false;
                        self.tree.append(Item {
                            start: ix,
                            end: ix + 1,
                            body: ItemBody::MaybeHtml,
                        })?;
                        begin_text = ix + 1;
                        LoopInstruction::ContinueAndSkip(0)
                    }
                    b'!' => {
                        if ix + 1 < bytes_len && bytes.byte(ix + 1)? == b'[' {
                            self.tree.append_text(begin_text, ix, backslash_escaped)?;
                            backslash_escaped = false;
                            self.tree.append(Item {
                                start: ix,
                                end: ix + 2,
                                body: ItemBody::MaybeImage,
                            })?;
                            begin_text = ix + 2;
                            LoopInstruction::ContinueAndSkip(1)
                        } else {
                            LoopInstruction::ContinueAndSkip(0)
                        }
                    }
                    b'[' => {
                        self.tree.append_text(begin_text, ix, backslash_escaped)?;
                        backslash_escaped = false;
                        self.tree.append(Item {
                            start: ix,
                            end: ix + 1,
                            body: ItemBody::MaybeLinkOpen,
                        })?;
                        begin_text = ix + 1;
                        LoopInstruction::ContinueAndSkip(0)
                    }
                    b']' => {
                        self.tree.append_text(begin_text, ix, backslash_escaped)?;
                        backslash_escaped = false;
                        self.tree.append(Item {
                            start: ix,
                            end: ix + 1,
                            body: ItemBody::MaybeLinkClose(true),
                        })?;
                        begin_text = ix + 1;
                        LoopInstruction::ContinueAndSkip(0)
                    }
                    b'&' => match scan_entity(bytes.slice(ix..))? {
                        (n, Some(value)) => {
                            self.tree.append_text(begin_text, ix, backslash_escaped)?;
                            backslash_escaped = false;
                            self.tree.append(Item {
                                start: ix,
                                end: ix + n,
                                body: ItemBody::SynthesizeText(
                                    self.allocs
                                        .allocate_cow(self.allocs.backend.store_text(value)?)?,
                                ),
                            })?;
                            begin_text = ix + n;
                            LoopInstruction::ContinueAndSkip(n - 1)
                        }
                        _ => LoopInstruction::ContinueAndSkip(0),
                    },
                    b'|' => {
                        if ix != 0 && bytes.byte(ix - 1)? == b'\\' {
                            LoopInstruction::ContinueAndSkip(0)
                        } else if let TableParseMode::Active = mode {
                            LoopInstruction::BreakAtWith(ix, None)
                        } else {
                            last_pipe_ix = ix;
                            pipes += 1;
                            LoopInstruction::ContinueAndSkip(0)
                        }
                    }
                    b'.' => {
                        if ix + 2 < bytes.len()
                            && bytes.byte(ix + 1)? == b'.'
                            && bytes.byte(ix + 2)? == b'.'
                        {
                            self.tree.append_text(begin_text, ix, backslash_escaped)?;
                            backslash_escaped = false;
                            self.tree.append(Item {
                                start: ix,
                                end: ix + 3,
                                body: ItemBody::SynthesizeChar('…'),
                            })?;
                            begin_text = ix + 3;
                            LoopInstruction::ContinueAndSkip(2)
                        } else {
                            LoopInstruction::ContinueAndSkip(0)
                        }
                    }
                    b'-' => {
                        let count = 1 + scan_ch_repeat(bytes.slice((ix + 1)..), b'-')?;
                        if count == 1 {
                            LoopInstruction::ContinueAndSkip(0)
                        } else {
                            let itembody = if count == 2 {
                                ItemBody::SynthesizeChar('–')
                            } else if count == 3 {
                                ItemBody::SynthesizeChar('—')
                            } else {
                                let (ems, ens) = match count % 6 {
                                    0 | 3 => (count / 3, 0),
                                    2 | 4 => (0, count / 2),
                                    1 => (count / 3 - 1, 2),
                                    _ => (count / 3, 1),
                                };
                                let mut buf = self.allocs.backend.text_buffer();
                                for _ in 0..ems {
                                    buf.push_char('—')?;
                                }
                                for _ in 0..ens {
                                    buf.push_char('–')?;
                                }
                                ItemBody::SynthesizeText(self.allocs.allocate_cow(buf.finish()?)?)
                            };

                            self.tree.append_text(begin_text, ix, backslash_escaped)?;
                            backslash_escaped = false;
                            self.tree.append(Item {
                                start: ix,
                                end: ix + count,
                                body: itembody,
                            })?;
                            begin_text = ix + count;
                            LoopInstruction::ContinueAndSkip(count - 1)
                        }
                    }
                    c @ b'\'' | c @ b'"' => {
                        let string_suffix = self.text.slice(ix..);
                        let can_open = delim_run_can_open(
                            self.text.slice(start..),
                            string_suffix,
                            1,
                            ix - start,
                            mode,
                        )?;
                        let can_close = delim_run_can_close(
                            self.text.slice(start..),
                            string_suffix,
                            1,
                            ix - start,
                            mode,
                        )?;

                        self.tree.append_text(begin_text, ix, backslash_escaped)?;
                        backslash_escaped = false;
                        self.tree.append(Item {
                            start: ix,
                            end: ix + 1,
                            body: ItemBody::MaybeSmartQuote(c, can_open, can_close),
                        })?;
                        begin_text = ix + 1;

                        LoopInstruction::ContinueAndSkip(0)
                    }
                    _ => LoopInstruction::ContinueAndSkip(0),
                })
            })?;

        if brk.is_none() {
            let trailing_whitespace =
                scan_rev_while(bytes.slice(begin_text..final_ix), is_ascii_whitespace_no_nl)?;
            // need to close text at eof
            self.tree.append_text(
                begin_text,
                final_ix - trailing_whitespace,
                backslash_escaped,
            )?;
        }
        Ok((final_ix, brk))
    }

    /// When start_ix is at the beginning of an HTML block of type 1 to 5,
    /// this will find the end of the block, adding the block itself to the
    /// tree and also keeping track of the lines of HTML within the block.
    ///
    /// The html_end_tag is the tag that must be found on a line to end the block.
    fn parse_html_block_type_1_to_5(
        &mut self,
        start_ix: usize,
        html_end_tag: &str,
        mut remaining_space: usize,
        mut indent: usize,
    ) -> Result<usize, B::Error> {
        self.tree.append(Item {
            start: start_ix,
            end: 0, // set later
            body: ItemBody::HtmlBlock,
        })?;
        self.tree.push()?;

        let bytes = self.text;
        let mut ix = start_ix;
        let end_ix;
        loop {
            let line_start_ix = ix;
            ix += scan_nextline(bytes.slice(ix..))?;
            self.append_html_line(remaining_space.max(indent), line_start_ix, ix)?;

            let mut line_start = LineStart::new(bytes.slice(ix..));
            let n_containers = scan_containers(&self.tree, &mut line_start, self.options, false)?;
            if n_containers < self.tree.spine_len() {
                end_ix = ix;
                break;
            }

            if self
                .text
                .slice(line_start_ix..ix)
                .contains(html_end_tag.as_bytes())?
            {
                end_ix = ix;
                break;
            }

            let next_line_ix = ix + line_start.bytes_scanned();
            if next_line_ix == self.text.len() {
                end_ix = next_line_ix;
                break;
            }
            ix = next_line_ix;
            remaining_space = line_start.remaining_space();
            indent = 0;
        }
        self.pop(end_ix)?;
        Ok(ix)
    }

    /// When start_ix is at the beginning of an HTML block of type 6 or 7,
    /// this will consume lines until there is a blank line and keep track of
    /// the HTML within the block.
    fn parse_html_block_type_6_or_7(
        &mut self,
        start_ix: usize,
        mut remaining_space: usize,
        mut indent: usize,
    ) -> Result<usize, B::Error> {
        self.tree.append(Item {
            start: start_ix,
            end: 0, // set later
            body: ItemBody::HtmlBlock,
        })?;
        self.tree.push()?;

        let bytes = self.text;
        let mut ix = start_ix;
        let end_ix;
        loop {
            let line_start_ix = ix;
            ix += scan_nextline(bytes.slice(ix..))?;
            self.append_html_line(remaining_space.max(indent), line_start_ix, ix)?;

            let mut line_start = LineStart::new(bytes.slice(ix..));
            let n_containers = scan_containers(&self.tree, &mut line_start, self.options, false)?;
            if n_containers < self.tree.spine_len() || line_start.is_at_eol()? {
                end_ix = ix;
                break;
            }

            let next_line_ix = ix + line_start.bytes_scanned();
            if next_line_ix == self.text.len()
                || scan_blank_line(bytes.slice(next_line_ix..))?.is_some()
            {
                end_ix = next_line_ix;
                break;
            }
            ix = next_line_ix;
            remaining_space = line_start.remaining_space();
            indent = 0;
        }
        self.pop(end_ix)?;
        Ok(ix)
    }

    fn parse_indented_code_block(
        &mut self,
        start_ix: usize,
        mut remaining_space: usize,
    ) -> Result<usize, B::Error> {
        self.tree.append(Item {
            start: start_ix,
            end: 0, // will get set later
            body: ItemBody::IndentCodeBlock,
        })?;
        self.tree.push()?;
        let bytes = self.text;
        let mut last_nonblank_child = None;
        let mut last_nonblank_ix = 0;
        let mut end_ix = 0;
        self.last_line_blank = false;

        let mut ix = start_ix;
        loop {
            let line_start_ix = ix;
            ix += scan_nextline(bytes.slice(ix..))?;
            self.append_code_text(remaining_space, line_start_ix, ix)?;
            // TODO(spec clarification): should we synthesize newline at EOF?

            if !self.last_line_blank {
                last_nonblank_child = self.tree.cur();
                last_nonblank_ix = ix;
                end_ix = ix;
            }

            let mut line_start = LineStart::new(bytes.slice(ix..));
            let n_containers = scan_containers(&self.tree, &mut line_start, self.options, false)?;
            if n_containers < self.tree.spine_len()
                || !(line_start.scan_space(4)? || line_start.is_at_eol()?)
            {
                break;
            }
            let next_line_ix = ix + line_start.bytes_scanned();
            if next_line_ix == self.text.len() {
                break;
            }
            ix = next_line_ix;
            remaining_space = line_start.remaining_space();
            self.last_line_blank = scan_blank_line(bytes.slice(ix..))?.is_some();
        }

        // Trim trailing blank lines.
        if let Some(child) = last_nonblank_child {
            {
                let value = None;
                self.tree.update(child, |node| node.next = value)?;
            };
            {
                let value = last_nonblank_ix;
                self.tree.update(child, |node| node.item.end = value)?;
            };
        }
        self.pop(end_ix)?;
        Ok(ix)
    }

    fn parse_fenced_code_block(
        &mut self,
        start_ix: usize,
        indent: usize,
        fence_ch: u8,
        n_fence_char: usize,
    ) -> Result<usize, B::Error> {
        let bytes = self.text;
        let mut info_start = start_ix + n_fence_char;
        info_start += scan_whitespace_no_nl(bytes.slice(info_start..))?;
        // TODO: info strings are typically very short. wouldn't it be faster
        // to just do a forward scan here?
        let mut ix = info_start + scan_nextline(bytes.slice(info_start..))?;
        let info_end = ix - scan_rev_while(bytes.slice(info_start..ix), is_ascii_whitespace)?;
        let info_string = unescape(
            self.allocs
                .backend
                .source_text(self.text.slice(info_start..info_end))?,
            self.tree.is_in_table()?,
            &self.allocs.backend,
        )?;
        self.tree.append(Item {
            start: start_ix,
            end: 0, // will get set later
            body: ItemBody::FencedCodeBlock(self.allocs.allocate_cow(info_string)?),
        })?;
        self.tree.push()?;
        loop {
            let mut line_start = LineStart::new(bytes.slice(ix..));
            let n_containers = scan_containers(&self.tree, &mut line_start, self.options, false)?;
            if n_containers < self.tree.spine_len() {
                // this line will get parsed again as not being part of the code
                // if it's blank, it should be parsed as a blank line
                self.pop(ix)?;
                return Ok(ix);
            }
            line_start.scan_space(indent)?;
            let mut close_line_start = line_start.clone();
            if !close_line_start.scan_space(4 - indent)? {
                let close_ix = ix + close_line_start.bytes_scanned();
                if let Some(n) =
                    scan_closing_code_fence(bytes.slice(close_ix..), fence_ch, n_fence_char)?
                {
                    ix = close_ix + n;
                    self.pop(ix)?;
                    // try to read trailing whitespace or it will register as a completely blank line
                    return Ok(ix + scan_blank_line(bytes.slice(ix..))?.unwrap_or(0));
                }
            }
            let remaining_space = line_start.remaining_space();
            ix += line_start.bytes_scanned();
            let next_ix = ix + scan_nextline(bytes.slice(ix..))?;
            self.append_code_text(remaining_space, ix, next_ix)?;
            ix = next_ix;
        }
    }

    fn parse_metadata_block(
        &mut self,
        start_ix: usize,
        metadata_block_ch: u8,
    ) -> Result<usize, B::Error> {
        let bytes = self.text;
        let metadata_block_kind = match metadata_block_ch {
            b'-' => MetadataBlockKind::YamlStyle,
            b'+' => MetadataBlockKind::PlusesStyle,
            _ => panic!("Erroneous metadata block character when parsing metadata block"),
        };
        // 3 delimiter characters
        let mut ix = start_ix + 3 + scan_nextline(bytes.slice(start_ix + 3..))?;
        self.tree.append(Item {
            start: start_ix,
            end: 0, // will get set later
            body: ItemBody::MetadataBlock(metadata_block_kind),
        })?;
        self.tree.push()?;
        loop {
            let mut line_start = LineStart::new(bytes.slice(ix..));
            let n_containers = scan_containers(&self.tree, &mut line_start, self.options, false)?;
            if n_containers < self.tree.spine_len() {
                break;
            }
            if let (_, 0) = calc_indent(bytes.slice(ix..), 4)? {
                if let Some(n) = scan_closing_metadata_block(bytes.slice(ix..), metadata_block_ch)?
                {
                    ix += n;
                    break;
                }
            }
            let remaining_space = line_start.remaining_space();
            ix += line_start.bytes_scanned();
            let next_ix = ix + scan_nextline(bytes.slice(ix..))?;
            self.append_code_text(remaining_space, ix, next_ix)?;
            ix = next_ix;
        }

        self.pop(ix)?;

        // try to read trailing whitespace or it will register as a completely blank line
        Ok(ix + scan_blank_line(bytes.slice(ix..))?.unwrap_or(0))
    }

    fn append_code_text(
        &mut self,
        remaining_space: usize,
        start: usize,
        end: usize,
    ) -> Result<(), B::Error> {
        if remaining_space > 0 {
            let cow_ix = self.allocs.allocate_cow(
                self.allocs
                    .backend
                    .store_text("   "[..remaining_space].into())?,
            )?;
            self.tree.append(Item {
                start,
                end: start,
                body: ItemBody::SynthesizeText(cow_ix),
            })?;
        }
        if self.text.byte(end - 2)? == b'\r' {
            // Normalize CRLF to LF
            self.tree.append_text(start, end - 2, false)?;
            self.tree.append_text(end - 1, end, false)?;
        } else {
            self.tree.append_text(start, end, false)?;
        }
        Ok(())
    }

    /// Appends a line of HTML to the tree.
    fn append_html_line(
        &mut self,
        remaining_space: usize,
        start: usize,
        end: usize,
    ) -> Result<(), B::Error> {
        if remaining_space > 0 {
            let cow_ix = self.allocs.allocate_cow(
                self.allocs
                    .backend
                    .store_text("   "[..remaining_space].into())?,
            )?;
            self.tree.append(Item {
                start,
                end: start,
                body: ItemBody::SynthesizeText(cow_ix),
            })?;
        }
        if self.text.byte(end - 2)? == b'\r' {
            // Normalize CRLF to LF
            self.tree.append(Item {
                start,
                end: end - 2,
                body: ItemBody::Html,
            })?;
            self.tree.append(Item {
                start: end - 1,
                end,
                body: ItemBody::Html,
            })?;
        } else {
            self.tree.append(Item {
                start,
                end,
                body: ItemBody::Html,
            })?;
        }
        Ok(())
    }

    /// Pop a container, setting its end.
    fn pop(&mut self, ix: usize) -> Result<(), B::Error> {
        let cur_ix = self.tree.pop()?.unwrap();
        {
            let value = ix;
            self.tree.update(cur_ix, |node| node.item.end = value)?;
        };
        if matches!(
            self.tree.node(cur_ix)?.item.body,
            ItemBody::DefinitionList(_)
        ) {
            fixup_end_of_definition_list(&mut self.tree, cur_ix)?;
            self.begin_list_item = None;
        }
        let tight_list = match &self.tree.node(cur_ix)?.item.body {
            ItemBody::List(flags, _, _) | ItemBody::DefinitionList(flags) => {
                flags.get().contains(ListFlags::IS_TIGHT)
            }
            _ => false,
        };
        if tight_list {
            surgerize_tight_list(&mut self.tree, cur_ix)?;
            self.begin_list_item = None;
        }
        Ok(())
    }

    /// Close a list if it's open. Also set loose if last line was blank
    /// and end current list if it's a lone, empty item
    fn finish_list(&mut self, ix: usize) -> Result<(), B::Error> {
        self.finish_empty_list_item()?;
        if let Some(node_ix) = self.tree.peek_up()? {
            if matches!(
                self.tree.node(node_ix)?.item.body,
                ItemBody::List(_, _, _) | ItemBody::DefinitionList(_)
            ) {
                self.pop(ix)?;
            }
        }
        if self.last_line_blank {
            if let Some(node_ix) = self.tree.peek_grandparent()? {
                if let ItemBody::List(ref list_flags, _, _)
                | ItemBody::DefinitionList(ref list_flags) = self.tree.node(node_ix)?.item.body
                {
                    self.tree
                        .set_list_flags(node_ix, list_flags.get() & !ListFlags::IS_TIGHT)?;
                }
            }
            self.last_line_blank = false;
        }
        Ok(())
    }

    fn finish_empty_list_item(&mut self) -> Result<(), B::Error> {
        if let Some(begin_list_item) = self.begin_list_item {
            if self.last_line_blank {
                // A list item can begin with at most one blank line.
                if let Some(node_ix) = self.tree.peek_up()? {
                    if matches!(
                        self.tree.node(node_ix)?.item.body,
                        ItemBody::ListItem(_) | ItemBody::DefinitionListDefinition(_)
                    ) {
                        self.pop(begin_list_item)?;
                    }
                }
            }
        }
        self.begin_list_item = None;
        Ok(())
    }

    /// Continue an existing list or start a new one if there's not an open
    /// list that matches.
    fn continue_list(&mut self, start: usize, ch: u8, index: u64) -> Result<(), B::Error> {
        self.finish_empty_list_item()?;
        if let Some(node_ix) = self.tree.peek_up()? {
            if let ItemBody::List(ref list_flags, existing_ch, _) =
                self.tree.node(node_ix)?.item.body
            {
                if existing_ch == ch {
                    if self.last_line_blank {
                        self.tree
                            .set_list_flags(node_ix, list_flags.get() & !ListFlags::IS_TIGHT)?;
                        self.last_line_blank = false;
                    }
                    return Ok(());
                }
            }
            // TODO: this is not the best choice for end; maybe get end from last list item.
            self.finish_list(start)?;
        }
        self.tree.append(Item {
            start,
            end: 0, // will get set later
            body: ItemBody::List(Cell::new(ListFlags::IS_TIGHT), ch, index),
        })?;
        self.tree.push()?;
        self.last_line_blank = false;
        Ok(())
    }

    /// Parse a thematic break.
    ///
    /// Returns index of start of next line.
    fn parse_hrule(&mut self, hrule_size: usize, ix: usize) -> Result<usize, B::Error> {
        self.tree.append(Item {
            start: ix,
            end: ix + hrule_size,
            body: ItemBody::Rule,
        })?;
        Ok(ix + hrule_size)
    }

    /// Parse an ATX heading.
    ///
    /// Returns index of start of next line.
    fn parse_atx_heading(
        &mut self,
        start: usize,
        atx_level: HeadingLevel,
    ) -> Result<usize, B::Error> {
        let mut ix = start;
        let heading_ix = self.tree.append(Item {
            start,
            end: 0,                    // set later
            body: ItemBody::default(), // set later
        })?;
        ix += atx_level as usize;
        // next char is space or eol (guaranteed by scan_atx_heading)
        let bytes = self.text;
        if let Some(eol_bytes) = scan_eol(bytes.slice(ix..))? {
            {
                let value = ix + eol_bytes;
                self.tree.update(heading_ix, |node| node.item.end = value)?;
            };
            {
                let value = ItemBody::Heading(atx_level, None);
                self.tree
                    .update(heading_ix, |node| node.item.body = value)?;
            };
            return Ok(ix + eol_bytes);
        }
        // skip leading spaces
        let skip_spaces = scan_whitespace_no_nl(bytes.slice(ix..))?;
        ix += skip_spaces;

        // now handle the header text
        let header_start = ix;
        let header_node_idx = self.tree.push()?; // so that we can set the endpoint later

        // trim the trailing attribute block before parsing the entire line, if necessary
        let (end, content_end, attrs) = if self.options.contains(Options::ENABLE_HEADING_ATTRIBUTES)
        {
            // the start of the next line is the end of the header since the
            // header cannot have line breaks
            let header_end = header_start + scan_nextline(bytes.slice(header_start..))?;
            let (content_end, attrs) =
                self.extract_and_parse_heading_attribute_block(header_start, header_end)?;
            self.parse_line(ix, Some(content_end), TableParseMode::Disabled)?;
            (header_end, content_end, attrs)
        } else {
            let (line_ix, line_brk) = self.parse_line(ix, None, TableParseMode::Disabled)?;
            ix = line_ix;
            // Backslash at end is actually hard line break
            if let Some(Item {
                start,
                end,
                body: ItemBody::HardBreak(true),
            }) = line_brk
            {
                self.tree.append_text(start, end, false)?;
            }
            (ix, ix, None)
        };
        {
            let value = end;
            self.tree
                .update(header_node_idx, |node| node.item.end = value)?;
        };

        // remove trailing matter from header text
        let mut empty_text_node = false;
        if let Some(cur_ix) = self.tree.cur() {
            // remove closing of the ATX heading
            let header_text = bytes.slice(header_start..content_end);
            let mut limit = header_text.len()
                - scan_rev_while(header_text, |b| matches!(b, b'\n' | b'\r' | b' '))?;
            let closer = limit - scan_rev_while(header_text.slice(..limit), |b| b == b'#')?;
            if closer == 0 {
                limit = closer;
            } else {
                let spaces = scan_rev_while(header_text.slice(..closer), |b| b == b' ')?;
                if spaces > 0 {
                    limit = closer - spaces;
                }
            }
            // if text is only spaces, then remove them
            {
                let value = limit + header_start;
                self.tree.update(cur_ix, |node| node.item.end = value)?;
            };

            // limit = 0 when text is empty after removing spaces
            if limit == 0 {
                empty_text_node = true;
            }
        }

        if empty_text_node {
            self.tree.remove_node()?;
        } else {
            self.tree.pop()?;
        }
        {
            let value = ItemBody::Heading(
                atx_level,
                attrs
                    .map(|attrs| self.allocs.allocate_heading(attrs))
                    .transpose()?,
            );
            self.tree
                .update(heading_ix, |node| node.item.body = value)?;
        };

        Ok(end)
    }

    /// Returns the number of bytes scanned on success.
    fn parse_footnote(&mut self, start: usize) -> Result<Option<usize>, B::Error> {
        let bytes = self.text.slice(start..);
        if !bytes.starts_with(b"[^")? {
            return Ok(None);
        }
        let parsed_label = if self.options.has_gfm_footnotes() {
            // GitHub doesn't allow footnote definition labels to contain line breaks.
            // It actually does allow this for link definitions under certain circumstances,
            // but for this it's simpler to avoid it.
            scan_link_label_rest(
                self.text.slice(start + 2..),
                &|_| Ok::<_, B::Error>(None),
                self.tree.is_in_table()?,
                &self.allocs.backend,
            )?
        } else {
            self.parse_refdef_label(start + 2)?
        };
        let Some((mut i, label)) = parsed_label else {
            return Ok(None);
        };
        if self.options.has_gfm_footnotes() {
            let source = self.allocs.backend.text_source(&label)?;
            if source.find_byte(b'\r')?.is_some() || source.find_byte(b'\n')?.is_some() {
                return Ok(None);
            }
        }
        i += 2;
        if bytes.get(i)? != Some(b':') {
            return Ok(None);
        }
        i += 1;
        self.finish_list(start)?;
        if let Some(node_ix) = self.tree.peek_up()? {
            if matches!(
                self.tree.node(node_ix)?.item.body,
                ItemBody::FootnoteDefinition(..)
            ) {
                self.pop(start)?;
            }
        }
        if self.options.has_gfm_footnotes() {
            i += scan_whitespace_no_nl(bytes.slice(i..))?;
        }
        self.allocs.footdefs.insert(label.clone())?;
        self.tree.append(Item {
            start,
            end: 0, // will get set later
            body: ItemBody::FootnoteDefinition(self.allocs.allocate_cow(label)?),
        })?;
        self.tree.push()?;
        Ok(Some(i))
    }

    /// Tries to parse a reference label, which can be interrupted by new blocks.
    /// On success, returns the number of bytes of the label and the label itself.
    fn parse_refdef_label(&self, start: usize) -> Result<Option<(usize, B::Text<'a>)>, B::Error> {
        scan_link_label_rest(
            self.text.slice(start..),
            &|bytes| {
                let mut line_start = LineStart::new(bytes);
                let tree_position =
                    scan_containers(&self.tree, &mut line_start, self.options, false)?;
                let current_container = tree_position == self.tree.spine_len();
                if line_start.scan_space(4)? {
                    return Ok(Some(line_start.bytes_scanned()));
                }
                let bytes_scanned = line_start.bytes_scanned();
                let off = self.text.len() - bytes.len() + bytes_scanned;
                let suffix = self.text.slice(off..);
                if self.scan_paragraph_interrupt(suffix, current_container, tree_position)?
                    || (current_container && scan_setext_heading(suffix)?.is_some())
                {
                    Ok(None)
                } else {
                    Ok(Some(bytes_scanned))
                }
            },
            self.tree.is_in_table()?,
            &self.allocs.backend,
        )
    }

    /// Returns number of bytes scanned, label and definition on success.
    fn parse_refdef_total(
        &mut self,
        start: usize,
    ) -> Result<Option<(usize, B::Text<'a>, LinkDef<B::Text<'a>>)>, B::Error> {
        let bytes = self.text.slice(start..);
        if bytes.get(0)? != Some(b'[') {
            return Ok(None);
        }
        let Some((mut i, label)) = self.parse_refdef_label(start + 1)? else {
            return Ok(None);
        };
        i += 1;
        if bytes.get(i)? != Some(b':') {
            return Ok(None);
        }
        i += 1;
        let Some((bytecount, link_def)) = self.scan_refdef(start, start + i)? else {
            return Ok(None);
        };
        Ok(Some((bytecount + i, label, link_def)))
    }

    /// Returns number of bytes and number of newlines
    fn scan_refdef_space(
        &self,
        bytes: Text<'_, B::Input>,
        mut i: usize,
    ) -> Result<Option<(usize, usize)>, B::Error> {
        let mut newlines = 0;
        loop {
            let whitespaces = scan_whitespace_no_nl(bytes.slice(i..))?;
            i += whitespaces;
            if let Some(eol_bytes) = scan_eol(bytes.slice(i..))? {
                i += eol_bytes;
                newlines += 1;
                if newlines > 1 {
                    return Ok(None);
                }
            } else {
                break;
            }
            let mut line_start = LineStart::new(bytes.slice(i..));
            let tree_position = scan_containers(&self.tree, &mut line_start, self.options, false)?;
            let current_container = tree_position == self.tree.spine_len();
            if !line_start.scan_space(4)? {
                let suffix = self.text.slice(i + line_start.bytes_scanned()..);
                if self.scan_paragraph_interrupt(suffix, current_container, tree_position)?
                    || scan_setext_heading(suffix)?.is_some()
                {
                    return Ok(None);
                }
            }
            i += line_start.bytes_scanned();
        }
        Ok(Some((i, newlines)))
    }

    fn scan_refdef_title<'t>(
        &self,
        text: Text<'t, B::Input>,
    ) -> Result<Option<(usize, B::Text<'t>)>, B::Error> {
        self.allocs.backend.speculate_text(|| {
            let Some(opening_delim) = text.get(0)? else {
                return Ok(None);
            };
            let closing_delim = match opening_delim {
                b'\'' => b'\'',
                b'"' => b'"',
                b'(' => b')',
                _ => return Ok(None),
            };
            let mut bytecount = 1;
            let mut linestart = 1;
            let mut linebuf = self.allocs.backend.text_buffer();
            let mut normalized = false;
            while let Some(c) = text.get(bytecount)? {
                match c {
                    b'(' if closing_delim == b')' => return Ok(None),
                    b'\n' | b'\r' => {
                        normalized = true;
                        linebuf.push_source(text.slice(linestart..bytecount))?;
                        linebuf.push_char('\n')?;
                        bytecount += 1;
                        if c == b'\r' && text.get(bytecount)? == Some(b'\n') {
                            bytecount += 1;
                        }
                        let mut line_start = LineStart::new(text.slice(bytecount..));
                        let tree_position =
                            scan_containers(&self.tree, &mut line_start, self.options, false)?;
                        let current_container = tree_position == self.tree.spine_len();
                        if !line_start.scan_space(4)? {
                            let suffix = text.slice(bytecount + line_start.bytes_scanned()..);
                            if self.scan_paragraph_interrupt(
                                suffix,
                                current_container,
                                tree_position,
                            )? || scan_setext_heading(suffix)?.is_some()
                            {
                                return Ok(None);
                            }
                        }
                        line_start.scan_all_space()?;
                        bytecount += line_start.bytes_scanned();
                        linestart = bytecount;
                        if scan_blank_line(text.slice(bytecount..))?.is_some() {
                            return Ok(None);
                        }
                    }
                    b'\\' => {
                        bytecount += 1;
                        if let Some(c) = text.get(bytecount)? {
                            if c != b'\r' && c != b'\n' {
                                bytecount += 1;
                            }
                        }
                    }
                    c if c == closing_delim => {
                        let value = if normalized {
                            linebuf.push_source(text.slice(linestart..bytecount))?;
                            linebuf.finish()?
                        } else {
                            self.allocs
                                .backend
                                .source_text(text.slice(linestart..bytecount))?
                        };
                        return Ok(Some((bytecount + 1, value)));
                    }
                    _ => bytecount += 1,
                }
            }
            Ok(None)
        })
    }

    /// Returns # of bytes and definition.
    /// Assumes the label of the reference including colon has already been scanned.
    fn scan_refdef(
        &self,
        span_start: usize,
        start: usize,
    ) -> Result<Option<(usize, LinkDef<B::Text<'a>>)>, B::Error> {
        let bytes = self.text;

        // whitespace between label and url (including up to one newline)
        let Some((mut i, _newlines)) = self.scan_refdef_space(bytes, start)? else {
            return Ok(None);
        };

        // scan link dest
        let Some((dest_length, dest)) = scan_link_dest(self.text, i, LINK_MAX_NESTED_PARENS)?
        else {
            return Ok(None);
        };
        if dest_length == 0 {
            return Ok(None);
        }
        let dest = unescape(
            self.allocs.backend.source_text(dest)?,
            self.tree.is_in_table()?,
            &self.allocs.backend,
        )?;
        i += dest_length;

        // no title
        let mut backup = (
            i - start,
            LinkDef {
                dest,
                title: None,
                span: span_start..i,
            },
        );

        // scan whitespace between dest and label
        let (mut i, newlines) =
            if let Some((new_i, mut newlines)) = self.scan_refdef_space(bytes, i)? {
                if i == self.text.len() {
                    newlines += 1;
                }
                if new_i == i && newlines == 0 {
                    return Ok(None);
                }
                if newlines > 1 {
                    return Ok(Some(backup));
                };
                (new_i, newlines)
            } else {
                return Ok(Some(backup));
            };

        // scan title
        // if this fails but newline == 1, return also a refdef without title
        if let Some((title_length, title)) = self.scan_refdef_title(self.text.slice(i..))? {
            i += title_length;
            if scan_blank_line(bytes.slice(i..))?.is_some() {
                backup.0 = i - start;
                backup.1.span = span_start..i;
                backup.1.title = Some(unescape(
                    title,
                    self.tree.is_in_table()?,
                    &self.allocs.backend,
                )?);
                return Ok(Some(backup));
            }
        }
        if newlines > 0 {
            Ok(Some(backup))
        } else {
            Ok(None)
        }
    }

    /// Checks whether we should break a paragraph on the given input.
    fn scan_paragraph_interrupt(
        &self,
        text: Text<'_, B::Input>,
        current_container: bool,
        tree_position: usize,
    ) -> Result<bool, B::Error> {
        let bytes = text;
        if scan_paragraph_interrupt_no_table(
            text,
            current_container,
            self.options.contains(Options::ENABLE_FOOTNOTES),
            self.options.contains(Options::ENABLE_DEFINITION_LIST),
            &self.tree,
            tree_position,
            &self.allocs.backend,
        )? {
            return Ok(true);
        }
        if !self.options.contains(Options::ENABLE_TABLES) || !bytes.starts_with(b"|")? {
            return Ok(false);
        }

        // Checking if something's a valid table or not requires looking at two lines.
        // First line, count unescaped pipes.
        let mut pipes = 0;
        let mut next_line_ix = 0;
        let mut bsesc = false;
        let mut last_pipe_ix = 0;
        for i in 0..bytes.len() {
            let byte = bytes.byte(i)?;
            match byte {
                b'\\' => {
                    bsesc = true;
                    continue;
                }
                b'|' if !bsesc => {
                    pipes += 1;
                    last_pipe_ix = i;
                }
                b'\r' | b'\n' => {
                    next_line_ix = i + scan_eol(bytes.slice(i..))?.unwrap();
                    break;
                }
                _ => {}
            }
            bsesc = false;
        }

        // scan_eol can't return 0, so this can't be zero
        if next_line_ix == 0 {
            return Ok(false);
        }

        let mut line_start = LineStart::new(bytes.slice(next_line_ix..));
        if scan_containers(&self.tree, &mut line_start, self.options, false)?
            != self.tree.spine_len()
        {
            return Ok(false);
        }
        let table_head_ix = next_line_ix + line_start.bytes_scanned();
        let mut columns = 0;
        let table_head_bytes = scan_table_head(bytes.slice(table_head_ix..), |_| {
            columns += 1;
            Ok(())
        })?;

        if table_head_bytes == 0 {
            return Ok(false);
        }

        // computing header count from number of pipes
        let header_count = count_header_cols(bytes, pipes, 0, last_pipe_ix)?;

        // make sure they match the number of columns we find in separator line
        Ok(columns == header_count)
    }

    /// Return the content end and the optional trailing heading attributes.
    fn extract_and_parse_heading_attribute_block(
        &mut self,
        header_start: usize,
        header_end: usize,
    ) -> Result<(usize, Option<HeadingValue<'a, B>>), B::Error> {
        if !self.options.contains(Options::ENABLE_HEADING_ATTRIBUTES) {
            return Ok((header_end, None));
        }

        // extract the trailing attribute block
        let header_bytes = self.text.slice(header_start..header_end);
        let (content_len, attr_block_range_rel) =
            extract_attribute_block_content_from_header_text(header_bytes)?;
        let content_end = header_start + content_len;
        let attrs = attr_block_range_rel
            .map(|r| {
                parse_inside_attribute_block(
                    self.text
                        .slice((header_start + r.start)..(header_start + r.end)),
                    &self.allocs.backend,
                )
            })
            .transpose()?;
        Ok((content_end, attrs))
    }
}

/// Scanning modes for `Parser`'s `parse_line` method.
#[derive(PartialEq, Eq, Copy, Clone)]
enum TableParseMode {
    /// Inside a paragraph, scanning for table headers.
    Scan,
    /// Inside a table.
    Active,
    /// Inside a paragraph, not scanning for table headers.
    Disabled,
}

/// Computes the number of header columns in a table line by computing the number of dividing pipes
/// that aren't followed or preceded by whitespace.
fn count_header_cols<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
    mut pipes: usize,
    mut start: usize,
    last_pipe_ix: usize,
) -> Result<usize, S::Error> {
    // was first pipe preceded by whitespace? if so, subtract one
    start += scan_whitespace_no_nl(bytes.slice(start..))?;
    if bytes.byte(start)? == b'|' {
        pipes -= 1;
    }

    // was last pipe followed by whitespace? if so, sub one
    Ok(
        if scan_blank_line(bytes.slice((last_pipe_ix + 1)..))?.is_some() {
            pipes
        } else {
            pipes + 1
        },
    )
}

/// Checks whether we should break a paragraph on the given input.
///
/// Use `FirstPass::scan_paragraph_interrupt` in any context that allows
/// tables to interrupt the paragraph.
fn scan_paragraph_interrupt_no_table<B: Backend>(
    bytes: Text<'_, B::Input>,
    current_container: bool,
    has_footnote: bool,
    definition_list: bool,
    tree: &Tree<Item, B::Nodes, B::Spine>,
    tree_position: usize,
    backend: &B,
) -> Result<bool, B::Error> {
    Ok(scan_eol(bytes)?.is_some()
        || scan_hrule(bytes)?.is_ok()
        || scan_atx_heading(bytes)?.is_some()
        || scan_code_fence(bytes)?.is_some()
        || scan_blockquote_start(bytes)?.is_some()
        || match scan_listitem(bytes)? {
            Some((ix, delim, index, _)) => {
                !current_container
                    || tree.is_in_table()?
                    || (delim == b'*' || delim == b'-' || delim == b'+' || index == 1)
                        && scan_blank_line(bytes.slice(ix..))?.is_none()
            }
            None => false,
        }
        || bytes.starts_with(b"<")?
            && (get_html_end_tag(bytes.slice(1..))?.is_some()
                || starts_html_block_type_6(bytes.slice(1..))?)
        || definition_list
            && ((current_container
                && match tree.peek_up()? {
                    Some(index) => matches!(
                        tree.node(index)?.item.body,
                        ItemBody::Paragraph
                            | ItemBody::TightParagraph
                            | ItemBody::MaybeDefinitionListTitle
                    ),
                    None => false,
                })
                || match tree.walk_spine().nth(tree_position).transpose()? {
                    Some(index) => matches!(
                        tree.node(index)?.item.body,
                        ItemBody::DefinitionListDefinition(_)
                    ),
                    None => false,
                })
            && bytes.starts_with(b":")?
        || (has_footnote
            && bytes.starts_with(b"[^")?
            && backend.lookahead(|| {
                Ok(
                    match scan_link_label_rest(
                        bytes.slice(2..),
                        &|_| Ok(None),
                        tree.is_in_table()?,
                        backend,
                    )? {
                        Some((length, _)) => bytes.get(2 + length)? == Some(b':'),
                        None => false,
                    },
                )
            })?))
}

/// Assumes `text_bytes` is preceded by `<`.
fn get_html_end_tag<S: Bytes + ?Sized>(
    text_bytes: Text<'_, S>,
) -> Result<Option<&'static str>, S::Error> {
    static BEGIN_TAGS: &[&[u8]; 4] = &[b"pre", b"style", b"script", b"textarea"];
    static ST_BEGIN_TAGS: &[&[u8]; 3] = &[b"!--", b"?", b"![CDATA["];

    for (beg_tag, end_tag) in BEGIN_TAGS
        .iter()
        .zip(["</pre>", "</style>", "</script>", "</textarea>"].iter())
    {
        let tag_len = beg_tag.len();

        if text_bytes.len() < tag_len {
            // begin tags are increasing in size
            break;
        }

        let mut matches = true;
        for (index, byte) in beg_tag.iter().enumerate() {
            if !text_bytes.byte(index)?.eq_ignore_ascii_case(byte) {
                matches = false;
                break;
            }
        }
        if !matches {
            continue;
        }

        // Must either be the end of the line...
        if text_bytes.len() == tag_len {
            return Ok(Some(end_tag));
        }

        // ...or be followed by whitespace, newline, or '>'.
        let s = text_bytes.byte(tag_len)?;
        if is_ascii_whitespace(s) || s == b'>' {
            return Ok(Some(end_tag));
        }
    }

    for (beg_tag, end_tag) in ST_BEGIN_TAGS.iter().zip(["-->", "?>", "]]>"].iter()) {
        if text_bytes.starts_with(beg_tag)? {
            return Ok(Some(end_tag));
        }
    }

    Ok(
        if text_bytes.len() > 1
            && text_bytes.byte(0)? == b'!'
            && text_bytes.byte(1)?.is_ascii_alphabetic()
        {
            Some(">")
        } else {
            None
        },
    )
}

fn surgerize_tight_list<N, P>(
    tree: &mut Tree<Item, N, P>,
    list_ix: TreeIndex,
) -> Result<(), N::Error>
where
    N: ListFlagStorage,
    P: Sequence<TreeIndex, Error = N::Error>,
{
    let mut list_item = tree.node(list_ix)?.child;
    while let Some(listitem_ix) = list_item {
        let mut node_ix = tree.node(listitem_ix)?.child;
        while let Some(index) = node_ix {
            if matches!(tree.node(index)?.item.body, ItemBody::Paragraph) {
                tree.update(index, |node| node.item.body = ItemBody::TightParagraph)?;
            }
            node_ix = tree.node(index)?.next;
        }
        list_item = tree.node(listitem_ix)?.next;
    }
    Ok(())
}

fn fixup_end_of_definition_list<N, P>(
    tree: &mut Tree<Item, N, P>,
    list_ix: TreeIndex,
) -> Result<(), N::Error>
where
    N: ListFlagStorage,
    P: Sequence<TreeIndex, Error = N::Error>,
{
    let mut list_item = tree.node(list_ix)?.child;
    let mut previous_list_item = None;
    while let Some(index) = list_item {
        match tree.body_snapshot(index)? {
            ItemBody::DefinitionListTitle | ItemBody::DefinitionListDefinition(_) => {
                previous_list_item = list_item;
                list_item = tree.node(index)?.next;
            }
            ItemBody::MaybeDefinitionListTitle => {
                tree.update(index, |node| node.item.body = ItemBody::Paragraph)?;
                break;
            }
            _ => break,
        }
    }
    if let Some(previous_list_item) = previous_list_item {
        tree.truncate_to_parent(previous_list_item)?;
    }
    Ok(())
}

/// Determines whether the delimiter run starting at given index is
/// left-flanking, as defined by the commonmark spec (and isn't intraword
/// for _ delims).
fn delim_run_can_open<S: Source + ?Sized>(
    s: Text<'_, S>,
    suffix: Text<'_, S>,
    run_len: usize,
    ix: usize,
    mode: TableParseMode,
) -> Result<bool, S::Error> {
    let next_char = if let Some(c) = suffix.slice(run_len..).first_char()? {
        c
    } else {
        return Ok(false);
    };
    if next_char.is_whitespace() {
        return Ok(false);
    }
    if ix == 0 {
        return Ok(true);
    }
    if mode == TableParseMode::Active {
        if s.slice(..ix).ends_with(b"|")? && !s.slice(..ix).ends_with(br"\|")? {
            return Ok(true);
        }
        if next_char == '|' {
            return Ok(false);
        }
    }
    let delim = suffix.byte(0)?;
    // `*` and `~~` can be intraword, `_` and `~` cannot
    if delim == b'*' && !is_punctuation(next_char) {
        return Ok(true);
    }
    if delim == b'~' && run_len > 1 {
        return Ok(true);
    }
    let prev_char = s.slice(..ix).last_char()?.unwrap();
    if delim == b'~' && prev_char == '~' && !is_punctuation(next_char) {
        return Ok(true);
    }

    Ok(prev_char.is_whitespace()
        || is_punctuation(prev_char) && (delim != b'\'' || ![']', ')'].contains(&prev_char)))
}

/// Determines whether the delimiter run starting at given index is
/// right-flanking, as defined by the commonmark spec (and isn't intraword
/// for _ delims)
fn delim_run_can_close<S: Source + ?Sized>(
    s: Text<'_, S>,
    suffix: Text<'_, S>,
    run_len: usize,
    ix: usize,
    mode: TableParseMode,
) -> Result<bool, S::Error> {
    if ix == 0 {
        return Ok(false);
    }
    let prev_char = s.slice(..ix).last_char()?.unwrap();
    if prev_char.is_whitespace() {
        return Ok(false);
    }
    let next_char = if let Some(c) = suffix.slice(run_len..).first_char()? {
        c
    } else {
        return Ok(true);
    };
    if mode == TableParseMode::Active {
        if s.slice(..ix).ends_with(b"|")? && !s.slice(..ix).ends_with(br"\|")? {
            return Ok(false);
        }
        if next_char == '|' {
            return Ok(true);
        }
    }
    let delim = suffix.byte(0)?;
    // `*` and `~~` can be intraword, `_` and `~` cannot
    if (delim == b'*' || (delim == b'~' && run_len > 1)) && !is_punctuation(prev_char) {
        return Ok(true);
    }
    if delim == b'~' && prev_char == '~' {
        return Ok(true);
    }

    Ok(next_char.is_whitespace() || is_punctuation(next_char))
}

fn create_lut(options: &Options) -> LookupTable {
    #[cfg(all(target_arch = "x86_64", feature = "simd"))]
    {
        LookupTable {
            simd: simd::compute_lookup(options),
            scalar: special_bytes(options),
        }
    }
    #[cfg(not(all(target_arch = "x86_64", feature = "simd")))]
    {
        special_bytes(options)
    }
}

fn special_bytes(options: &Options) -> [bool; 256] {
    let mut flags = [false; 256];
    let standard_bytes = [
        b'\n', b'\r', b'*', b'_', b'&', b'\\', b'[', b']', b'<', b'!', b'`',
    ];

    for &byte in &standard_bytes {
        flags[byte as usize] = true;
    }
    if options.contains(Options::ENABLE_TABLES) {
        flags[b'|' as usize] = true;
    }
    if options.contains(Options::ENABLE_STRIKETHROUGH)
        || options.contains(Options::ENABLE_SUBSCRIPT)
    {
        flags[b'~' as usize] = true;
    }
    if options.contains(Options::ENABLE_SUPERSCRIPT) {
        flags[b'^' as usize] = true;
    }
    if options.contains(Options::ENABLE_MATH) {
        flags[b'$' as usize] = true;
        flags[b'{' as usize] = true;
        flags[b'}' as usize] = true;
    }
    if options.contains(Options::ENABLE_SMART_PUNCTUATION) {
        for &byte in &[b'.', b'-', b'"', b'\''] {
            flags[byte as usize] = true;
        }
    }

    flags
}

enum LoopInstruction<T> {
    /// Continue looking for more special bytes, but skip next few bytes.
    ContinueAndSkip(usize),
    /// Break looping immediately, returning with the given index and value.
    BreakAtWith(usize, T),
}

#[cfg(all(target_arch = "x86_64", feature = "simd"))]
struct LookupTable {
    simd: [u8; 16],
    scalar: [bool; 256],
}

#[cfg(not(all(target_arch = "x86_64", feature = "simd")))]
type LookupTable = [bool; 256];

/// Call back on lookup-table bytes, honoring skip counts.
/// Stop on a break value or the first callback error.
fn iterate_special_bytes<S, F, T, E>(
    lut: &LookupTable,
    bytes: Text<'_, S>,
    ix: usize,
    callback: F,
) -> Result<(usize, Option<T>), E>
where
    S: Bytes<Error = E> + ?Sized,
    F: FnMut(usize, u8) -> Result<LoopInstruction<Option<T>>, E>,
{
    #[cfg(all(target_arch = "x86_64", feature = "simd"))]
    {
        if let Some(contiguous) = bytes.contiguous() {
            return simd::iterate_special_bytes(lut, contiguous, ix, callback);
        }
        scan_special_bytes(&lut.scalar, bytes.len(), ix, |ix| bytes.byte(ix), callback)
    }
    #[cfg(not(all(target_arch = "x86_64", feature = "simd")))]
    {
        scan_special_bytes(lut, bytes.len(), ix, |ix| bytes.byte(ix), callback)
    }
}

#[cfg(all(target_arch = "x86_64", feature = "simd"))]
fn scalar_iterate_special_bytes<F, T, E>(
    lut: &[bool; 256],
    bytes: &[u8],
    ix: usize,
    callback: F,
) -> Result<(usize, Option<T>), E>
where
    F: FnMut(usize, u8) -> Result<LoopInstruction<Option<T>>, E>,
{
    scan_special_bytes(lut, bytes.len(), ix, |ix| Ok(bytes[ix]), callback)
}

fn scan_special_bytes<F, R, T, E>(
    lut: &[bool; 256],
    length: usize,
    mut ix: usize,
    read: R,
    mut callback: F,
) -> Result<(usize, Option<T>), E>
where
    R: Fn(usize) -> Result<u8, E>,
    F: FnMut(usize, u8) -> Result<LoopInstruction<Option<T>>, E>,
{
    while ix < length {
        let b = read(ix)?;
        if lut[b as usize] {
            match callback(ix, b)? {
                LoopInstruction::ContinueAndSkip(skip) => {
                    ix += skip;
                }
                LoopInstruction::BreakAtWith(ix, val) => {
                    return Ok((ix, val));
                }
            }
        }
        ix += 1;
    }

    Ok((ix, None))
}

/// Split the usual heading content range and the content inside the trailing attribute block.
///
/// Returns `(leading_content_len, Option<trailing_attr_block_range>)`.
///
/// Note that `trailing_attr_block_range` will be empty range when the block
/// is `{}`, since the range is content inside the wrapping `{` and `}`.
///
/// The closing `}` of an attribute block can have trailing whitespaces.
/// They are automatically trimmed when the attribute block is being searched.
///
/// However, this method does not trim the trailing whitespaces of heading content.
/// It is callers' responsibility to trim them if necessary.
fn extract_attribute_block_content_from_header_text<S: Bytes + ?Sized>(
    heading: Text<'_, S>,
) -> Result<(usize, Option<Range<usize>>), S::Error> {
    let heading_len = heading.len();
    let mut ix = heading_len;
    ix -= scan_rev_while(heading, |b| {
        b == b'\n' || b == b'\r' || b == b' ' || b == b'\t'
    })?;
    if ix == 0 {
        return Ok((heading_len, None));
    }

    let attr_block_close = ix - 1;
    if heading.get(attr_block_close)? != Some(b'}') {
        return Ok((heading_len, None));
    }
    // move cursor before the closing right brace (`}`)
    ix -= 1;

    ix -= scan_rev_while(heading.slice(..ix), |b| {
        !matches!(b, b'{' | b'}' | b'<' | b'>' | b'\\' | b'\n' | b'\r')
    })?;
    if ix == 0 {
        return Ok((heading_len, None));
    }
    let attr_block_open = ix - 1;
    if heading.byte(attr_block_open)? != b'{' {
        return Ok((heading_len, None));
    }

    Ok((attr_block_open, Some(ix..attr_block_close)))
}

/// Parse whitespace-separated heading attributes without retaining a token list.
fn parse_inside_attribute_block<'a, B: Backend>(
    input: Text<'a, B::Input>,
    backend: &B,
) -> Result<HeadingValue<'a, B>, B::Error> {
    let mut id = None;
    let mut classes = backend.cow_values()?;
    let mut attrs = backend.attribute_rows()?;
    let mut i = 0;
    while i < input.len() {
        i += scan_while(input.slice(i..), |byte| byte.is_ascii_whitespace())?;
        let start = i;
        i += scan_while(input.slice(i..), |byte| !byte.is_ascii_whitespace())?;
        let attr = input.slice(start..i);
        if attr.len() <= 1 {
            continue;
        }
        match attr.byte(0)? {
            b'#' => id = Some(backend.source_text(attr.slice(1..))?),
            b'.' => classes.push(backend.source_text(attr.slice(1..))?)?,
            _ => {
                let (key, value) = match attr.find_byte(b'=')? {
                    Some(index) => (
                        backend.source_text(attr.slice(..index))?,
                        Some(backend.source_text(attr.slice(index + 1..))?),
                    ),
                    None => (backend.source_text(attr)?, None),
                };
                attrs.push((key, value))?;
            }
        }
    }
    Ok(HeadingAttributes {
        id,
        classes: backend.finish_classes(classes)?,
        attrs: backend.finish_attributes(attrs)?,
    })
}

#[cfg(all(target_arch = "x86_64", feature = "simd"))]
mod simd {
    //! SIMD byte scanning logic.
    //!
    //! This module provides functions that allow walking through byteslices, calling
    //! provided callback functions on special bytes and their indices using SIMD.
    //! The byteset is defined in `compute_lookup`.
    //!
    //! The idea is to load in a chunk of 16 bytes and perform a lookup into a set of
    //! bytes on all the bytes in this chunk simultaneously. We produce a 16 bit bitmask
    //! from this and call the callback on every index corresponding to a 1 in this mask
    //! before moving on to the next chunk. This allows us to move quickly when there
    //! are no or few matches.
    //!
    //! The table lookup is inspired by this [great overview]. However, since all of the
    //! bytes we're interested in are ASCII, we don't quite need the full generality of
    //! the universal algorithm and are hence able to skip a few instructions.
    //!
    //! [great overview]: http://0x80.pl/articles/simd-byte-lookup.html

    use super::{LookupTable, LoopInstruction};
    use crate::Options;
    use core::arch::x86_64::*;
    use std::ops::ControlFlow;

    const VECTOR_SIZE: usize = std::mem::size_of::<__m128i>();

    /// Generates a lookup table containing the bitmaps for our
    /// special marker bytes. This is effectively a 128 element 2d bitvector,
    /// that can be indexed by a four bit row index (the lower nibble)
    /// and a three bit column index (upper nibble).
    pub(super) fn compute_lookup(options: &Options) -> [u8; 16] {
        let mut lookup = [0u8; 16];
        let standard_bytes = [
            b'\n', b'\r', b'*', b'_', b'&', b'\\', b'[', b']', b'<', b'!', b'`',
        ];

        for &byte in &standard_bytes {
            add_lookup_byte(&mut lookup, byte);
        }
        if options.contains(Options::ENABLE_TABLES) {
            add_lookup_byte(&mut lookup, b'|');
        }
        if options.contains(Options::ENABLE_STRIKETHROUGH)
            || options.contains(Options::ENABLE_SUBSCRIPT)
        {
            add_lookup_byte(&mut lookup, b'~');
        }
        if options.contains(Options::ENABLE_SUPERSCRIPT) {
            add_lookup_byte(&mut lookup, b'^');
        }
        if options.contains(Options::ENABLE_MATH) {
            add_lookup_byte(&mut lookup, b'$');
            add_lookup_byte(&mut lookup, b'{');
            add_lookup_byte(&mut lookup, b'}');
        }
        if options.contains(Options::ENABLE_SMART_PUNCTUATION) {
            for &byte in &[b'.', b'-', b'"', b'\''] {
                add_lookup_byte(&mut lookup, byte);
            }
        }

        lookup
    }

    fn add_lookup_byte(lookup: &mut [u8; 16], byte: u8) {
        lookup[(byte & 0x0f) as usize] |= 1 << (byte >> 4);
    }

    /// Computes a bit mask for the given byteslice starting from the given index,
    /// where the 16 least significant bits indicate (by value of 1) whether or not
    /// there is a special character at that byte position. The least significant bit
    /// corresponds to `bytes[ix]` and the most significant bit corresponds to
    /// `bytes[ix + 15]`.
    /// It is only safe to call this function when `bytes.len() >= ix + VECTOR_SIZE`.
    #[target_feature(enable = "ssse3")]
    #[inline]
    unsafe fn compute_mask(lut: &[u8; 16], bytes: &[u8], ix: usize) -> i32 {
        debug_assert!(bytes.len() >= ix + VECTOR_SIZE);

        let bitmap = _mm_loadu_si128(lut.as_ptr() as *const __m128i);
        // Small lookup table to compute single bit bitshifts
        // for 16 bytes at once.
        let bitmask_lookup =
            _mm_setr_epi8(1, 2, 4, 8, 16, 32, 64, -128, -1, -1, -1, -1, -1, -1, -1, -1);

        // Load input from memory.
        let raw_ptr = bytes.as_ptr().add(ix) as *const __m128i;
        let input = _mm_loadu_si128(raw_ptr);
        // Compute the bitmap using the bottom nibble as an index
        // into the lookup table. Note that non-ascii bytes will have
        // their most significant bit set and will map to lookup[0].
        let bitset = _mm_shuffle_epi8(bitmap, input);
        // Compute the high nibbles of the input using a 16-bit rightshift of four
        // and a mask to prevent most-significant bit issues.
        let higher_nibbles = _mm_and_si128(_mm_srli_epi16(input, 4), _mm_set1_epi8(0x0f));
        // Create a bitmask for the bitmap by perform a left shift of the value
        // of the higher nibble. Bytes with their most significant set are mapped
        // to -1 (all ones).
        let bitmask = _mm_shuffle_epi8(bitmask_lookup, higher_nibbles);
        // Test the bit of the bitmap by AND'ing the bitmap and the mask together.
        let tmp = _mm_and_si128(bitset, bitmask);
        // Check whether the result was not null. NEQ is not a SIMD intrinsic,
        // but comparing to the bitmask is logically equivalent. This also prevents us
        // from matching any non-ASCII bytes since none of the bitmaps were all ones
        // (-1).
        let result = _mm_cmpeq_epi8(tmp, bitmask);

        // Return the resulting bitmask.
        _mm_movemask_epi8(result)
    }

    /// Use SSSE3 when supported; propagate callback errors.
    pub(super) fn iterate_special_bytes<F, T, E>(
        lut: &LookupTable,
        bytes: &[u8],
        ix: usize,
        callback: F,
    ) -> Result<(usize, Option<T>), E>
    where
        F: FnMut(usize, u8) -> Result<LoopInstruction<Option<T>>, E>,
    {
        if is_x86_feature_detected!("ssse3") && bytes.len() >= VECTOR_SIZE {
            unsafe { simd_iterate_special_bytes(&lut.simd, bytes, ix, callback) }
        } else {
            super::scalar_iterate_special_bytes(&lut.scalar, bytes, ix, callback)
        }
    }

    /// Process set bits until a break value or callback failure.
    /// Continue carries the next absolute byte offset.
    unsafe fn process_mask<F, T, E>(
        mut mask: i32,
        bytes: &[u8],
        mut offset: usize,
        callback: &mut F,
    ) -> Result<ControlFlow<(usize, Option<T>), usize>, E>
    where
        F: FnMut(usize, u8) -> Result<LoopInstruction<Option<T>>, E>,
    {
        while mask != 0 {
            let mask_ix = mask.trailing_zeros() as usize;
            offset += mask_ix;
            match callback(offset, *bytes.get_unchecked(offset))? {
                LoopInstruction::ContinueAndSkip(skip) => {
                    offset += skip + 1;
                    let shift = skip + 1 + mask_ix;
                    if shift >= 32 {
                        break;
                    }
                    mask >>= shift;
                }
                LoopInstruction::BreakAtWith(ix, val) => return Ok(ControlFlow::Break((ix, val))),
            }
        }
        Ok(ControlFlow::Continue(offset))
    }

    #[target_feature(enable = "ssse3")]
    /// Important: only call this function when `bytes.len() >= 16`. Doing
    /// so otherwise may exhibit undefined behaviour.
    unsafe fn simd_iterate_special_bytes<F, T, E>(
        lut: &[u8; 16],
        bytes: &[u8],
        mut ix: usize,
        mut callback: F,
    ) -> Result<(usize, Option<T>), E>
    where
        F: FnMut(usize, u8) -> Result<LoopInstruction<Option<T>>, E>,
    {
        debug_assert!(bytes.len() >= VECTOR_SIZE);
        let upperbound = bytes.len() - VECTOR_SIZE;

        while ix < upperbound {
            let mask = compute_mask(lut, bytes, ix);
            let block_start = ix;
            ix = match process_mask(mask, bytes, ix, &mut callback)? {
                ControlFlow::Continue(ix) => std::cmp::max(ix, VECTOR_SIZE + block_start),
                ControlFlow::Break((end_ix, val)) => return Ok((end_ix, val)),
            };
        }

        if bytes.len() > ix {
            // shift off the bytes at start we have already scanned
            let mask = compute_mask(lut, bytes, upperbound) >> ix - upperbound;
            if let ControlFlow::Break((end_ix, val)) = process_mask(mask, bytes, ix, &mut callback)?
            {
                return Ok((end_ix, val));
            }
        }

        Ok((bytes.len(), None))
    }

    #[cfg(test)]
    mod simd_test {
        use super::super::create_lut;
        use super::{iterate_special_bytes, LoopInstruction};
        use crate::Options;

        fn check_expected_indices(bytes: &[u8], expected: &[usize], skip: usize) {
            let mut opts = Options::empty();
            opts.insert(Options::ENABLE_MATH);
            opts.insert(Options::ENABLE_TABLES);
            opts.insert(Options::ENABLE_FOOTNOTES);
            opts.insert(Options::ENABLE_STRIKETHROUGH);
            opts.insert(Options::ENABLE_SUPERSCRIPT);
            opts.insert(Options::ENABLE_TASKLISTS);

            let lut = create_lut(&opts);
            let mut indices = vec![];

            iterate_special_bytes::<_, i32, std::convert::Infallible>(
                &lut,
                bytes,
                0,
                |ix, _byte_ty| {
                    indices.push(ix);
                    Ok(LoopInstruction::ContinueAndSkip(skip))
                },
            )
            .unwrap();

            assert_eq!(&indices[..], expected);
        }

        #[test]
        fn simple_no_match() {
            check_expected_indices("abcdef0123456789".as_bytes(), &[], 0);
        }

        #[test]
        fn simple_match() {
            check_expected_indices("*bcd&f0123456789".as_bytes(), &[0, 4], 0);
        }

        #[test]
        fn single_open_fish() {
            check_expected_indices("<".as_bytes(), &[0], 0);
        }

        #[test]
        fn long_match() {
            check_expected_indices("0123456789abcde~*bcd&f0".as_bytes(), &[15, 16, 20], 0);
        }

        #[test]
        fn border_skip() {
            check_expected_indices("0123456789abcde~~~~d&f0".as_bytes(), &[15, 20], 3);
        }

        #[test]
        fn exhaustive_search() {
            let chars = [
                b'\n', b'\r', b'*', b'_', b'~', b'^', b'|', b'&', b'\\', b'[', b']', b'<', b'!',
                b'`', b'$', b'{', b'}',
            ];

            for &c in &chars {
                for i in 0u8..=255 {
                    if !chars.contains(&i) {
                        // full match
                        let mut buf = [i; 18];
                        buf[3] = c;
                        buf[6] = c;

                        check_expected_indices(&buf[..], &[3, 6], 0);
                    }
                }
            }
        }
    }
}
