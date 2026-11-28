// Copyright 2015 Google Inc. All rights reserved.
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

//! Scanners for fragments of CommonMark syntax

use std::char;

use crate::backend::Backend;
use crate::parse::HtmlScanGuard;
pub(crate) use crate::puncttable::{is_ascii_punctuation, is_punctuation};
use crate::source::{Bytes, Text};
use crate::strings::CowStr;
use crate::text::TextBuffer;
use crate::{entities, BlockQuoteKind, HeadingLevel};
use crate::{Alignment, LinkType};

// sorted for binary search
const HTML_TAGS: [&str; 62] = [
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

/// Analysis of the beginning of a line, including indentation and container
/// markers.
pub(crate) struct LineStart<'a, S: Bytes + ?Sized> {
    bytes: Text<'a, S>,
    ix: usize,

    // Byte after the last tab; a tab stop during initial indentation scanning.
    tab_start: usize,

    // Unconsumed columns from the last tab, before the byte at ix.
    spaces_remaining: usize,

    // no thematic breaks can occur before this offset.
    // this prevents scanning over and over up to a certain point
    min_hrule_offset: usize,
}

impl<S: Bytes + ?Sized> Clone for LineStart<'_, S> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes,
            ix: self.ix,
            tab_start: self.tab_start,
            spaces_remaining: self.spaces_remaining,
            min_hrule_offset: self.min_hrule_offset,
        }
    }
}

impl<'a, S: Bytes + ?Sized> LineStart<'a, S> {
    pub(crate) fn new(bytes: Text<'a, S>) -> Self {
        LineStart {
            bytes,
            tab_start: 0,
            ix: 0,
            spaces_remaining: 0,
            min_hrule_offset: 0,
        }
    }

    /// Consume up to n_space columns; return whether all were available.
    pub(crate) fn scan_space(&mut self, n_space: usize) -> Result<bool, S::Error> {
        Ok(self.scan_space_inner(n_space)? == 0)
    }

    pub(crate) fn scan_space_upto(&mut self, n_space: usize) -> Result<usize, S::Error> {
        Ok(n_space - self.scan_space_inner(n_space)?)
    }

    /// Returns unused remainder of spaces.
    fn scan_space_inner(&mut self, mut n_space: usize) -> Result<usize, S::Error> {
        // Consume any common prefix between the number of spaces we
        // want and the number of unscanned tab-introduced spaces.
        let n_from_remaining = self.spaces_remaining.min(n_space);
        self.spaces_remaining -= n_from_remaining;
        n_space -= n_from_remaining;

        while n_space > 0 && self.ix < self.bytes.len() {
            match self.bytes.byte(self.ix)? {
                b' ' => {
                    self.ix += 1;
                    n_space -= 1;
                }
                b'\t' => {
                    let spaces = 4 - (self.ix - self.tab_start) % 4;
                    self.ix += 1;
                    self.tab_start = self.ix;
                    let n = spaces.min(n_space);
                    n_space -= n;

                    // Record the unscanned portion of the tab.
                    self.spaces_remaining = spaces - n;
                }
                _ => break,
            }
        }
        Ok(n_space)
    }

    /// Scan all available ASCII whitespace (not including eol).
    pub(crate) fn scan_all_space(&mut self) -> Result<(), S::Error> {
        self.spaces_remaining = 0;
        self.ix += scan_while(self.bytes.slice(self.ix..), |byte| {
            byte == b' ' || byte == b'\t'
        })?;
        Ok(())
    }

    /// Determine whether we're at end of line (includes end of file).
    pub(crate) fn is_at_eol(&self) -> Result<bool, S::Error> {
        Ok(self
            .bytes
            .get(self.ix)?
            .map(|c| c == b'\r' || c == b'\n')
            .unwrap_or(true))
    }

    fn scan_ch(&mut self, c: u8) -> Result<bool, S::Error> {
        if self.bytes.get(self.ix)? == Some(c) {
            self.ix += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn scan_case_insensitive(&mut self, tag: &[u8]) -> Result<bool, S::Error> {
        if self.bytes.len() - self.ix < tag.len() {
            return Ok(false);
        }
        for (index, byte) in tag.iter().enumerate() {
            if !self.bytes.byte(self.ix + index)?.eq_ignore_ascii_case(byte) {
                return Ok(false);
            }
        }
        self.ix += tag.len();
        Ok(true)
    }

    pub(crate) fn scan_blockquote_tag(&mut self) -> Result<Option<BlockQuoteKind>, S::Error> {
        let saved_ix = self.ix;
        let tag = if self.scan_ch(b'[')? && self.scan_ch(b'!')? {
            let tag = if self.scan_case_insensitive(b"note")? {
                Some(BlockQuoteKind::Note)
            } else if self.scan_case_insensitive(b"tip")? {
                Some(BlockQuoteKind::Tip)
            } else if self.scan_case_insensitive(b"important")? {
                Some(BlockQuoteKind::Important)
            } else if self.scan_case_insensitive(b"warning")? {
                Some(BlockQuoteKind::Warning)
            } else if self.scan_case_insensitive(b"caution")? {
                Some(BlockQuoteKind::Caution)
            } else {
                None
            };
            if tag.is_some() && self.scan_ch(b']')? {
                if let Some(nl) = scan_blank_line(self.bytes.slice(self.ix..))? {
                    self.ix += nl;
                    tag
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if tag.is_none() {
            self.ix = saved_ix;
        }
        Ok(tag)
    }

    pub(crate) fn scan_blockquote_marker(&mut self) -> Result<bool, S::Error> {
        if self.scan_ch(b'>')? {
            self.scan_space(1)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Scan a definition marker.
    ///
    /// Definition markers are single colons, preceded by at most three spaces
    /// and followed by at most three spaces. The indentation of following
    /// lines is equal to the whole size of the marker, including the colon.
    ///
    /// If one is found, it will make the preceding paragraph into a definition
    /// list title.
    ///
    /// Return value is the amount of indentation, or `None` if it's not a
    /// definition list marker.
    pub(crate) fn scan_definition_list_definition_marker_with_indent(
        &mut self,
        indent: usize,
    ) -> Result<Option<usize>, S::Error> {
        let save = self.clone();
        if self.scan_ch(b':')? {
            let save = self.clone();
            if self.scan_space(5)? {
                *self = save;
                Ok(Some(indent + 1 + self.scan_space_upto(1)?))
            } else {
                *self = save;
                Ok(Some(indent + 1 + self.scan_space_upto(5)?))
            }
        } else {
            *self = save;
            Ok(None)
        }
    }

    /// Scan a list marker.
    ///
    /// Return value is the character, the start index, and the indent in spaces.
    /// For ordered list markers, the character will be one of b'.' or b')'. For
    /// bullet list markers, it will be one of b'-', b'+', or b'*'.
    pub(crate) fn scan_list_marker_with_indent(
        &mut self,
        indent: usize,
    ) -> Result<Option<(u8, u64, usize)>, S::Error> {
        let save = self.clone();
        if self.ix < self.bytes.len() {
            let c = self.bytes.byte(self.ix)?;
            if c == b'-' || c == b'+' || c == b'*' {
                if self.ix >= self.min_hrule_offset {
                    if let Err(min_offset) = scan_hrule(self.bytes.slice(self.ix..))? {
                        self.min_hrule_offset = min_offset;
                    } else {
                        *self = save;
                        return Ok(None);
                    }
                }
                self.ix += 1;
                if self.scan_space(1)? || self.is_at_eol()? {
                    return self.finish_list_marker(c, 0, indent + 2);
                }
            } else if c.is_ascii_digit() {
                let start_ix = self.ix;
                let mut ix = self.ix + 1;
                let mut val = u64::from(c - b'0');
                while ix < self.bytes.len() && ix - start_ix < 10 {
                    let c = self.bytes.byte(ix)?;
                    ix += 1;
                    if c.is_ascii_digit() {
                        val = val * 10 + u64::from(c - b'0');
                    } else if c == b')' || c == b'.' {
                        self.ix = ix;
                        if self.scan_space(1)? || self.is_at_eol()? {
                            return self.finish_list_marker(c, val, indent + 1 + ix - start_ix);
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        *self = save;
        Ok(None)
    }

    fn finish_list_marker(
        &mut self,
        c: u8,
        start: u64,
        mut indent: usize,
    ) -> Result<Option<(u8, u64, usize)>, S::Error> {
        let save = self.clone();

        if scan_blank_line(self.bytes.slice(self.ix..))?.is_some() {
            return Ok(Some((c, start, indent)));
        }

        let post_indent = self.scan_space_upto(4)?;
        if post_indent < 4 {
            indent += post_indent;
        } else {
            *self = save;
        }
        Ok(Some((c, start, indent)))
    }

    /// Returns Some(is_checked) when a task list marker was found. Resets itself
    /// to original state otherwise.
    pub(crate) fn scan_task_list_marker(&mut self) -> Result<Option<bool>, S::Error> {
        let save = self.clone();
        self.scan_space_upto(3)?;

        if !self.scan_ch(b'[')? {
            *self = save;
            return Ok(None);
        }
        let is_checked = match self.bytes.get(self.ix)? {
            Some(c) if is_ascii_whitespace_no_nl(c) => {
                self.ix += 1;
                false
            }
            Some(b'x') | Some(b'X') => {
                self.ix += 1;
                true
            }
            _ => {
                *self = save;
                return Ok(None);
            }
        };
        if !self.scan_ch(b']')? {
            *self = save;
            return Ok(None);
        }
        if !self
            .bytes
            .get(self.ix)?
            .map(is_ascii_whitespace)
            .unwrap_or(false)
        {
            *self = save;
            return Ok(None);
        }
        Ok(Some(is_checked))
    }

    pub(crate) fn bytes_scanned(&self) -> usize {
        self.ix
    }

    pub(crate) fn remaining_space(&self) -> usize {
        self.spaces_remaining
    }
}

pub(crate) fn is_ascii_whitespace(c: u8) -> bool {
    (0x09..=0x0d).contains(&c) || c == b' '
}

pub(crate) fn is_ascii_whitespace_no_nl(c: u8) -> bool {
    c == b'\t' || c == 0x0b || c == 0x0c || c == b' '
}

fn is_ascii_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

fn is_ascii_alphanumeric(c: u8) -> bool {
    matches!(c, b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
}

fn is_ascii_letterdigitdash(c: u8) -> bool {
    c == b'-' || is_ascii_alphanumeric(c)
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

fn is_valid_unquoted_attr_value_char(c: u8) -> bool {
    !matches!(
        c,
        b'\'' | b'"' | b' ' | b'=' | b'>' | b'<' | b'`' | b'\n' | b'\r'
    )
}

pub(crate) fn scan_ch<S: Bytes + ?Sized>(data: Text<'_, S>, c: u8) -> Result<usize, S::Error> {
    Ok(usize::from(data.get(0)? == Some(c)))
}

pub(crate) fn scan_while<S: Bytes + ?Sized, F>(
    data: Text<'_, S>,
    mut f: F,
) -> Result<usize, S::Error>
where
    F: FnMut(u8) -> bool,
{
    let mut count = 0;
    while count < data.len() && f(data.byte(count)?) {
        count += 1;
    }
    Ok(count)
}

pub(crate) fn scan_rev_while<S: Bytes + ?Sized, F>(
    data: Text<'_, S>,
    mut f: F,
) -> Result<usize, S::Error>
where
    F: FnMut(u8) -> bool,
{
    let mut start = data.len();
    while start > 0 && f(data.byte(start - 1)?) {
        start -= 1;
    }
    Ok(data.len() - start)
}

pub(crate) fn scan_ch_repeat<S: Bytes + ?Sized>(
    data: Text<'_, S>,
    c: u8,
) -> Result<usize, S::Error> {
    scan_while(data, |x| x == c)
}

// ASCII whitespace, excluding line endings.
pub(crate) fn scan_whitespace_no_nl<S: Bytes + ?Sized>(
    data: Text<'_, S>,
) -> Result<usize, S::Error> {
    scan_while(data, is_ascii_whitespace_no_nl)
}

fn scan_attr_value_chars<S: Bytes + ?Sized>(data: Text<'_, S>) -> Result<usize, S::Error> {
    scan_while(data, is_valid_unquoted_attr_value_char)
}

pub(crate) fn scan_eol<S: Bytes + ?Sized>(bytes: Text<'_, S>) -> Result<Option<usize>, S::Error> {
    Ok(match bytes.get(0)? {
        None => Some(0),
        Some(b'\n') => Some(1),
        Some(b'\r') => Some(if bytes.get(1)? == Some(b'\n') { 2 } else { 1 }),
        _ => None,
    })
}

pub(crate) fn scan_blank_line<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
) -> Result<Option<usize>, S::Error> {
    let i = scan_whitespace_no_nl(bytes)?;
    Ok(scan_eol(bytes.slice(i..))?.map(|n| i + n))
}

pub(crate) fn scan_nextline<S: Bytes + ?Sized>(bytes: Text<'_, S>) -> Result<usize, S::Error> {
    Ok(bytes.find_byte(b'\n')?.map_or(bytes.len(), |x| x + 1))
}

pub(crate) fn scan_closing_code_fence<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
    fence_char: u8,
    n_fence_char: usize,
) -> Result<Option<usize>, S::Error> {
    if bytes.is_empty() {
        return Ok(Some(0));
    }
    let mut i = 0;
    let num_fence_chars_found = scan_ch_repeat(bytes.slice(i..), fence_char)?;
    if num_fence_chars_found < n_fence_char {
        return Ok(None);
    }
    i += num_fence_chars_found;
    let num_trailing_spaces = scan_ch_repeat(bytes.slice(i..), b' ')?;
    i += num_trailing_spaces;
    Ok(scan_eol(bytes.slice(i..))?.map(|_| i))
}

pub(crate) fn scan_closing_metadata_block<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
    fence_char: u8,
) -> Result<Option<usize>, S::Error> {
    let mut i = 0;
    let mut num_fence_chars_found = scan_ch_repeat(bytes.slice(i..), fence_char)?;
    if num_fence_chars_found != 3 {
        // if YAML style metadata block the closing character can also be `.`
        if fence_char == b'-' {
            num_fence_chars_found = scan_ch_repeat(bytes.slice(i..), b'.')?;
            if num_fence_chars_found != 3 {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
    i += num_fence_chars_found;
    let num_trailing_spaces = scan_ch_repeat(bytes.slice(i..), b' ')?;
    i += num_trailing_spaces;
    Ok(scan_eol(bytes.slice(i..))?.map(|_| i))
}

// returned pair is (number of bytes, number of spaces)
pub(crate) fn calc_indent<S: Bytes + ?Sized>(
    text: Text<'_, S>,
    max: usize,
) -> Result<(usize, usize), S::Error> {
    let mut spaces = 0;
    let mut offset = 0;

    for i in 0..text.len() {
        let b = text.byte(i)?;
        offset = i;
        match b {
            b' ' => {
                spaces += 1;
                if spaces == max {
                    break;
                }
            }
            b'\t' => {
                let new_spaces = spaces + 4 - (spaces & 3);
                if new_spaces > max {
                    break;
                }
                spaces = new_spaces;
            }
            _ => break,
        }
    }

    Ok((offset, spaces))
}

/// Syntax result: consumed line length or earliest possible next break offset.
/// Source failures propagate separately.
pub(crate) fn scan_hrule<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
) -> Result<Result<usize, usize>, S::Error> {
    if bytes.len() < 3 {
        return Ok(Err(0));
    }
    let c = bytes.byte(0)?;
    if !(c == b'*' || c == b'-' || c == b'_') {
        return Ok(Err(0));
    }
    let mut n = 0;
    let mut i = 0;

    while i < bytes.len() {
        match bytes.byte(i)? {
            b'\n' | b'\r' => {
                i += scan_eol(bytes.slice(i..))?.unwrap_or(0);
                break;
            }
            c2 if c2 == c => {
                n += 1;
            }
            b' ' | b'\t' => (),
            _ => return Ok(Err(i)),
        }
        i += 1;
    }
    if n >= 3 {
        Ok(Ok(i))
    } else {
        Ok(Err(i))
    }
}

/// Scan an ATX heading opening sequence.
///
/// Returns number of bytes in prefix and level.
pub(crate) fn scan_atx_heading<S: Bytes + ?Sized>(
    data: Text<'_, S>,
) -> Result<Option<HeadingLevel>, S::Error> {
    let level = scan_ch_repeat(data, b'#')?;
    if data.get(level)?.map_or(true, is_ascii_whitespace) {
        Ok(HeadingLevel::try_from(level).ok())
    } else {
        Ok(None)
    }
}

/// Scan a setext heading underline.
///
/// Returns number of bytes in line (including trailing newline) and level.
pub(crate) fn scan_setext_heading<S: Bytes + ?Sized>(
    data: Text<'_, S>,
) -> Result<Option<(usize, HeadingLevel)>, S::Error> {
    let Some(c) = data.get(0)? else {
        return Ok(None);
    };
    let level = if c == b'=' {
        HeadingLevel::H1
    } else if c == b'-' {
        HeadingLevel::H2
    } else {
        return Ok(None);
    };
    let mut i = 1 + scan_ch_repeat(data.slice(1..), c)?;
    let Some(blank) = scan_blank_line(data.slice(i..))? else {
        return Ok(None);
    };
    i += blank;
    Ok(Some((i, level)))
}

pub(crate) fn scan_table_head<S: Bytes + ?Sized>(
    data: Text<'_, S>,
    mut push_column: impl FnMut(Alignment) -> Result<(), S::Error>,
) -> Result<usize, S::Error> {
    let (mut i, spaces) = calc_indent(data, 4)?;
    if spaces > 3 || i == data.len() {
        return Ok(0);
    }
    let mut active_col = Alignment::None;
    let mut start_col = true;
    let mut found_pipe = false;
    let mut found_hyphen = false;
    let mut found_hyphen_in_col = false;
    if data.byte(i)? == b'|' {
        i += 1;
        found_pipe = true;
    }
    while i < data.len() {
        if let Some(n) = scan_eol(data.slice(i..))? {
            i += n;
            break;
        }
        match data.byte(i)? {
            b' ' => (),
            b':' => {
                active_col = match (start_col, active_col) {
                    (true, Alignment::None) => Alignment::Left,
                    (false, Alignment::Left) => Alignment::Center,
                    (false, Alignment::None) => Alignment::Right,
                    _ => active_col,
                };
                start_col = false;
            }
            b'-' => {
                start_col = false;
                found_hyphen = true;
                found_hyphen_in_col = true;
            }
            b'|' => {
                start_col = true;
                found_pipe = true;
                push_column(active_col)?;
                active_col = Alignment::None;
                if !found_hyphen_in_col {
                    return Ok(0);
                }
                found_hyphen_in_col = false;
            }
            _ => {
                return Ok(0);
            }
        }
        i += 1;
    }

    if !start_col {
        push_column(active_col)?;
    }
    if !found_pipe || !found_hyphen {
        return Ok(0);
    }

    Ok(i)
}

/// Scan code fence.
///
/// Returns number of bytes scanned and the char that is repeated to make the code fence.
pub(crate) fn scan_code_fence<S: Bytes + ?Sized>(
    data: Text<'_, S>,
) -> Result<Option<(usize, u8)>, S::Error> {
    let Some(c) = data.get(0)? else {
        return Ok(None);
    };
    if !(c == b'`' || c == b'~') {
        return Ok(None);
    }
    let i = 1 + scan_ch_repeat(data.slice(1..), c)?;
    if i >= 3 {
        if c == b'`' {
            let suffix = data.slice(i..);
            let next_line = scan_nextline(suffix)?;
            if suffix.slice(..next_line).find_byte(b'`')?.is_some() {
                return Ok(None);
            }
        }
        Ok(Some((i, c)))
    } else {
        Ok(None)
    }
}

/// Scan metadata block, returning the number of delimiter bytes
/// (always 3 for now) and the delimiter character.
///
/// Differently to code blocks, metadata blocks must be closed with the closing
/// sequence not being a valid terminator the end of the file.
///
/// In addition, they cannot be empty (closing sequence in the next line) and
/// the next line cannot be an empty line.
pub(crate) fn scan_metadata_block<S: Bytes + ?Sized>(
    data: Text<'_, S>,
    yaml_style_enabled: bool,
    pluses_style_enabled: bool,
) -> Result<Option<(usize, u8)>, S::Error> {
    if yaml_style_enabled || pluses_style_enabled {
        let Some(c) = data.get(0)? else {
            return Ok(None);
        };
        if !((c == b'-' && yaml_style_enabled) || (c == b'+' && pluses_style_enabled)) {
            return Ok(None);
        }
        let i = 1 + scan_ch_repeat(data.slice(1..), c)?;
        // Only trailing spaces after the delimiters in the line
        let next_line = scan_nextline(data.slice(i..))?;
        for index in i..i + next_line {
            if !data.byte(index)?.is_ascii_whitespace() {
                return Ok(None);
            }
        }
        if i == 3 {
            // Search the closing sequence
            let mut j = i;
            let mut first_line = true;
            while j < data.len() {
                j += scan_nextline(data.slice(j..))?;
                let closed = scan_closing_metadata_block(data.slice(j..), c)?.is_some();
                // The first line of the metadata block cannot be an empty line
                // nor the end of the block
                if first_line {
                    if closed || scan_blank_line(data.slice(j..))?.is_some() {
                        return Ok(None);
                    }
                    first_line = false;
                }
                if closed {
                    return Ok(Some((i, c)));
                }
            }
            Ok(None)
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
}

pub(crate) fn scan_blockquote_start<S: Bytes + ?Sized>(
    data: Text<'_, S>,
) -> Result<Option<usize>, S::Error> {
    if data.get(0)? == Some(b'>') {
        let space = if data.get(1)? == Some(b' ') { 1 } else { 0 };
        Ok(Some(1 + space))
    } else {
        Ok(None)
    }
}

/// return number of bytes scanned, delimiter, start index, and indent
pub(crate) fn scan_listitem<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
) -> Result<Option<(usize, u8, usize, usize)>, S::Error> {
    let Some(mut c) = bytes.get(0)? else {
        return Ok(None);
    };
    let (w, start) = match c {
        b'-' | b'+' | b'*' => (1, 0),
        b'0'..=b'9' => {
            let (length, start) = parse_decimal(bytes, 9)?;
            let Some(next) = bytes.get(length)? else {
                return Ok(None);
            };
            c = next;
            if !(c == b'.' || c == b')') {
                return Ok(None);
            }
            (length + 1, start)
        }
        _ => {
            return Ok(None);
        }
    };
    let (mut postn, mut postindent) = calc_indent(bytes.slice(w..), 5)?;
    if postindent == 0 {
        if scan_eol(bytes.slice(w..))?.is_none() {
            return Ok(None);
        }
        postindent += 1;
    } else if postindent > 4 {
        postn = 1;
        postindent = 1;
    }
    if scan_blank_line(bytes.slice(w..))?.is_some() {
        postn = 0;
        postindent = 1;
    }
    Ok(Some((w + postn, c, start, w + postindent)))
}

// returns (number of bytes, parsed decimal)
fn parse_decimal<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
    limit: usize,
) -> Result<(usize, usize), S::Error> {
    let mut count = 0;
    let mut value = 0usize;
    while count < bytes.len().min(limit) {
        let byte = bytes.byte(count)?;
        if !is_digit(byte) {
            break;
        }
        let digit = usize::from(byte - b'0');
        let Some(next) = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit))
        else {
            break;
        };
        value = next;
        count += 1;
    }
    Ok((count, value))
}

// returns (number of bytes, parsed hex)
fn parse_hex<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
    limit: usize,
) -> Result<(usize, usize), S::Error> {
    let mut count = 0;
    let mut value = 0usize;
    while count < bytes.len().min(limit) {
        let byte = bytes.byte(count)?;
        let digit = if byte.is_ascii_digit() {
            usize::from(byte - b'0')
        } else if (b'a'..=b'f').contains(&(byte | 0x20)) {
            usize::from((byte | 0x20) - b'a' + 10)
        } else {
            break;
        };
        let Some(next) = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(digit))
        else {
            break;
        };
        value = next;
        count += 1;
    }
    Ok((count, value))
}

fn char_from_codepoint(input: usize) -> Option<char> {
    let codepoint = input.try_into().ok()?;
    if codepoint == 0 {
        return None;
    }
    char::from_u32(codepoint)
}

// doesn't bother to check data[0] == '&'
pub(crate) fn scan_entity<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
) -> Result<(usize, Option<CowStr<'static>>), S::Error> {
    let mut end = 1;
    if bytes.get(end)? == Some(b'#') {
        end += 1;
        let (bytecount, codepoint) = if end < bytes.len() && bytes.byte(end)? | 0x20 == b'x' {
            end += 1;
            parse_hex(bytes.slice(end..), 6)?
        } else {
            parse_decimal(bytes.slice(end..), 7)?
        };
        end += bytecount;
        return Ok(if bytecount == 0 || bytes.get(end)? != Some(b';') {
            (0, None)
        } else {
            (
                end + 1,
                Some(char_from_codepoint(codepoint).unwrap_or('\u{FFFD}').into()),
            )
        });
    }
    end += scan_while(bytes.slice(end..), is_ascii_alphanumeric)?;
    if bytes.get(end)? == Some(b';') {
        if let Some(value) = entities::get_entity(bytes.slice(1..end))? {
            return Ok((end + 1, Some(value.into())));
        }
    }
    Ok((0, None))
}

pub(crate) fn scan_wikilink_pipe<'a, S: Bytes + ?Sized>(
    data: Text<'a, S>,
    start_ix: usize,
    len: usize,
) -> Result<Option<(usize, Text<'a, S>)>, S::Error> {
    let end_ix = std::cmp::min(start_ix + len, data.len());
    let mut i = start_ix;

    while i < end_ix {
        if data.byte(i)? == b'|' {
            return Ok(Some((i + 1, data.slice(start_ix..i))));
        }
        i += 1;
    }
    Ok(None)
}

// Raw destination; escaping is resolved after recognition.
pub(crate) fn scan_link_dest<'a, S: Bytes + ?Sized>(
    data: Text<'a, S>,
    start_ix: usize,
    max_next: usize,
) -> Result<Option<(usize, Text<'a, S>)>, S::Error> {
    let bytes = data.slice(start_ix..);
    let mut i = scan_ch(bytes, b'<')?;

    if i != 0 {
        while i < bytes.len() {
            match bytes.byte(i)? {
                b'\n' | b'\r' | b'<' => return Ok(None),
                b'>' => return Ok(Some((i + 1, data.slice((start_ix + 1)..(start_ix + i))))),
                b'\\' if i + 1 < bytes.len() && is_ascii_punctuation(bytes.byte(i + 1)?) => {
                    i += 1;
                }
                _ => {}
            }
            i += 1;
        }
        Ok(None)
    } else {
        let mut nest = 0;
        while i < bytes.len() {
            match bytes.byte(i)? {
                0x0..=0x20 => {
                    break;
                }
                b'(' => {
                    if nest > max_next {
                        return Ok(None);
                    }
                    nest += 1;
                }
                b')' => {
                    if nest == 0 {
                        break;
                    }
                    nest -= 1;
                }
                b'\\' if i + 1 < bytes.len() && is_ascii_punctuation(bytes.byte(i + 1)?) => {
                    i += 1;
                }
                _ => {}
            }
            i += 1;
        }
        if nest != 0 {
            return Ok(None);
        }
        Ok(Some((i, data.slice(start_ix..(start_ix + i)))))
    }
}

/// Returns bytes scanned
fn scan_attribute_name<S: Bytes + ?Sized>(data: Text<'_, S>) -> Result<Option<usize>, S::Error> {
    let Some(c) = data.get(0)? else {
        return Ok(None);
    };
    if is_ascii_alpha(c) || c == b'_' || c == b':' {
        Ok(Some(
            1 + scan_while(data.slice(1..), |c| {
                is_ascii_alphanumeric(c) || c == b'_' || c == b'.' || c == b':' || c == b'-'
            })?,
        ))
    } else {
        Ok(None)
    }
}

/// Returns the index immediately following the attribute on success.
/// The argument `buffer_ix` refers to the index into `data` from which we
/// should copy into `buffer` when we find bytes to skip.
fn scan_attribute<'a, B: Backend>(
    data: Text<'a, B::Input>,
    mut ix: usize,
    newline_handler: Option<&dyn Fn(Text<'a, B::Input>) -> Result<usize, B::Error>>,
    buffer: &mut B::Buffer<'a>,
    buffer_ix: &mut usize,
) -> Result<Option<usize>, B::Error> {
    let Some(length) = scan_attribute_name(data.slice(ix..))? else {
        return Ok(None);
    };
    ix += length;
    let ix_after_attribute = ix;
    let Some(next) =
        scan_whitespace_with_newline_handler_without_buffer(data, ix, newline_handler)?
    else {
        return Ok(None);
    };
    ix = next;
    if data.get(ix)? == Some(b'=') {
        let Some(next) = scan_whitespace_with_newline_handler::<B>(
            data,
            ix_after_attribute,
            newline_handler,
            buffer,
            buffer_ix,
        )?
        else {
            return Ok(None);
        };
        ix = next + 1;
        let Some(next) = scan_whitespace_with_newline_handler::<B>(
            data,
            ix,
            newline_handler,
            buffer,
            buffer_ix,
        )?
        else {
            return Ok(None);
        };
        let Some(next) = scan_attribute_value::<B>(data, next, newline_handler, buffer, buffer_ix)?
        else {
            return Ok(None);
        };
        Ok(Some(next))
    } else {
        Ok(Some(ix_after_attribute))
    }
}

/// Scans whitespace and possibly newlines according to the
/// behavior defined by the newline handler. When bytes are skipped,
/// all preceding non-skipped bytes are pushed to the buffer.
fn scan_whitespace_with_newline_handler<'a, B: Backend>(
    data: Text<'a, B::Input>,
    mut i: usize,
    newline_handler: Option<&dyn Fn(Text<'a, B::Input>) -> Result<usize, B::Error>>,
    buffer: &mut B::Buffer<'a>,
    buffer_ix: &mut usize,
) -> Result<Option<usize>, B::Error> {
    while i < data.len() {
        if !is_ascii_whitespace(data.byte(i)?) {
            return Ok(Some(i));
        }
        if let Some(eol_bytes) = scan_eol(data.slice(i..))? {
            let Some(handler) = newline_handler else {
                return Ok(None);
            };
            i += eol_bytes;
            let skipped_bytes = handler(data.slice(i..))?;
            if skipped_bytes > 0 {
                buffer.push_source(data.slice(*buffer_ix..i))?;
                *buffer_ix = i + skipped_bytes;
            }
            i += skipped_bytes;
        } else {
            i += 1;
        }
    }
    Ok(Some(i))
}

/// Scans whitespace and possible newlines according to the behavior defined
/// by the newline handler.
///
/// Unlike [`scan_whitespace_with_newline_handler`], this function doesn't
/// copy skipped data into a buffer. Typically, if this function
/// returns `Some`, a call to `scan_whitespace_with_newline_handler` will
/// soon follow.
fn scan_whitespace_with_newline_handler_without_buffer<'a, S: Bytes + ?Sized>(
    data: Text<'a, S>,
    mut i: usize,
    newline_handler: Option<&dyn Fn(Text<'a, S>) -> Result<usize, S::Error>>,
) -> Result<Option<usize>, S::Error> {
    while i < data.len() {
        if !is_ascii_whitespace(data.byte(i)?) {
            return Ok(Some(i));
        }
        if let Some(eol_bytes) = scan_eol(data.slice(i..))? {
            let Some(handler) = newline_handler else {
                return Ok(None);
            };
            i += eol_bytes;
            i += handler(data.slice(i..))?;
        } else {
            i += 1;
        }
    }
    Ok(Some(i))
}

/// Returns the index immediately following the attribute value on success.
fn scan_attribute_value<'a, B: Backend>(
    data: Text<'a, B::Input>,
    mut i: usize,
    newline_handler: Option<&dyn Fn(Text<'a, B::Input>) -> Result<usize, B::Error>>,
    buffer: &mut B::Buffer<'a>,
    buffer_ix: &mut usize,
) -> Result<Option<usize>, B::Error> {
    let Some(first) = data.get(i)? else {
        return Ok(None);
    };
    match first {
        b @ b'"' | b @ b'\'' => {
            i += 1;
            while i < data.len() {
                if data.byte(i)? == b {
                    return Ok(Some(i + 1));
                }
                if let Some(eol_bytes) = scan_eol(data.slice(i..))? {
                    let Some(handler) = newline_handler else {
                        return Ok(None);
                    };
                    i += eol_bytes;
                    let skipped_bytes = handler(data.slice(i..))?;
                    if skipped_bytes > 0 {
                        buffer.push_source(data.slice(*buffer_ix..i))?;
                        *buffer_ix = i + skipped_bytes;
                    }
                    i += skipped_bytes;
                } else {
                    i += 1;
                }
            }
            return Ok(None);
        }
        b' ' | b'=' | b'>' | b'<' | b'`' | b'\n' | b'\r' => return Ok(None),
        _ => i += scan_attr_value_chars(data.slice(i..))?,
    }
    Ok(Some(i))
}

// Remove backslash escapes and resolve entities
pub(crate) fn unescape<'a, B: Backend>(
    input: B::Text<'a>,
    is_in_table: bool,
    backend: &B,
) -> Result<B::Text<'a>, B::Error> {
    let bytes = backend.text_source(&input)?;
    let mut result = backend.text_buffer();
    let mut mark = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes.byte(i)? {
            // Table recognition removes one escape before a pipe before inline parsing.
            b'\\'
                if is_in_table
                    && bytes.get(i + 1)? == Some(b'\\')
                    && bytes.get(i + 2)? == Some(b'|') =>
            {
                result.push_source(bytes.slice(mark..i))?;
                mark = i + 2;
                i += 3;
            }
            b'\\' if bytes.get(i + 1)?.is_some_and(is_ascii_punctuation) => {
                result.push_source(bytes.slice(mark..i))?;
                mark = i + 1;
                i += 2;
            }
            b'&' => match scan_entity(bytes.slice(i..))? {
                (n, Some(value)) => {
                    result.push_source(bytes.slice(mark..i))?;
                    result.push_str(value.as_ref())?;
                    i += n;
                    mark = i;
                }
                _ => i += 1,
            },
            b'\r' => {
                result.push_source(bytes.slice(mark..i))?;
                i += 1;
                mark = i;
            }
            _ => i += 1,
        }
    }
    if mark == 0 {
        drop(result);
        Ok(input)
    } else {
        result.push_source(bytes.slice(mark..))?;
        Ok(B::own_text(result.finish()?))
    }
}

/// Assumes `data` is preceded by `<`.
pub(crate) fn starts_html_block_type_6<S: Bytes + ?Sized>(
    data: Text<'_, S>,
) -> Result<bool, S::Error> {
    let i = scan_ch(data, b'/')?;
    let tail = data.slice(i..);
    let n = scan_while(tail, is_ascii_alphanumeric)?;
    if !is_html_tag(tail.slice(..n))? {
        return Ok(false);
    }
    let tail = tail.slice(n..);
    Ok(match tail.get(0)? {
        None | Some(b' ' | b'\t' | b'\r' | b'\n' | b'>') => true,
        Some(b'/') => tail.get(1)? == Some(b'>'),
        _ => false,
    })
}

fn is_html_tag<S: Bytes + ?Sized>(tag: Text<'_, S>) -> Result<bool, S::Error> {
    let index = crate::source::binary_search_by(&HTML_TAGS, |probe| -> Result<_, S::Error> {
        for (index, byte) in probe.as_bytes().iter().take(tag.len()).enumerate() {
            let order = byte.cmp(&(tag.byte(index)? | 0x20));
            if !order.is_eq() {
                return Ok(order);
            }
        }
        Ok(probe.len().cmp(&tag.len()))
    })?;
    Ok(index.is_some())
}

/// Assumes that `data` starts with `<`.
/// Returns the index into data directly after the html tag on success.
pub(crate) fn scan_html_type_7<'a, B: Backend>(
    data: Text<'a, B::Input>,
    backend: &B,
) -> Result<Option<usize>, B::Error> {
    let Some((_, i)) = scan_html_block_inner(data, None, backend)? else {
        return Ok(None);
    };
    Ok(scan_blank_line(data.slice(i..))?.map(|_| i))
}

/// Assumes that `data` starts with `<`.
/// Returns normalized HTML only when container prefixes were skipped.
pub(crate) fn scan_html_block_inner<'a, B: Backend>(
    data: Text<'a, B::Input>,
    newline_handler: Option<&dyn Fn(Text<'a, B::Input>) -> Result<usize, B::Error>>,
    backend: &B,
) -> Result<Option<(Option<B::Text<'a>>, usize)>, B::Error> {
    backend.speculate_text(|| {
        let mut buffer = backend.text_buffer();
        let mut last_buf_index = 0;

        let close_tag_bytes = scan_ch(data.slice(1..), b'/')?;
        let l = scan_while(data.slice((1 + close_tag_bytes)..), is_ascii_alpha)?;
        if l == 0 {
            return Ok(None);
        }
        let mut i = 1 + close_tag_bytes + l;
        i += scan_while(data.slice(i..), is_ascii_letterdigitdash)?;

        if close_tag_bytes == 0 {
            loop {
                let old_i = i;
                loop {
                    i += scan_whitespace_no_nl(data.slice(i..))?;
                    if let Some(eol_bytes) = scan_eol(data.slice(i..))? {
                        if eol_bytes == 0 {
                            return Ok(None);
                        }
                        let Some(handler) = newline_handler else {
                            return Ok(None);
                        };
                        i += eol_bytes;
                        let skipped_bytes = handler(data.slice(i..))?;

                        let data_len = data.len() - i;

                        debug_assert!(
                            skipped_bytes <= data_len,
                            "Handler tried to skip too many bytes, fed {}, skipped {}",
                            data_len,
                            skipped_bytes
                        );

                        if skipped_bytes > 0 {
                            buffer.push_source(data.slice(last_buf_index..i))?;
                            i += skipped_bytes;
                            last_buf_index = i;
                        }
                    } else {
                        break;
                    }
                }
                if let Some(b'/') | Some(b'>') = data.get(i)? {
                    break;
                }
                if old_i == i {
                    // No whitespace, which is mandatory.
                    return Ok(None);
                }
                let Some(next) = scan_attribute::<B>(
                    data,
                    i,
                    newline_handler,
                    &mut buffer,
                    &mut last_buf_index,
                )?
                else {
                    return Ok(None);
                };
                i = next;
            }
        }

        i += scan_whitespace_no_nl(data.slice(i..))?;

        if close_tag_bytes == 0 {
            i += scan_ch(data.slice(i..), b'/')?;
        }

        if data.get(i)? != Some(b'>') {
            Ok(None)
        } else {
            i += 1;
            let normalized = if buffer.is_empty() {
                None
            } else {
                buffer.push_source(data.slice(last_buf_index..i))?;
                Some(buffer.finish()?)
            };
            Ok(Some((normalized, i)))
        }
    })
}

/// Returns (next_byte_offset, uri, type)
pub(crate) fn scan_autolink<'a, S: Bytes + ?Sized>(
    text: Text<'a, S>,
    start_ix: usize,
) -> Result<Option<(usize, Text<'a, S>, LinkType)>, S::Error> {
    if let Some((bytes, uri)) = scan_uri(text, start_ix)? {
        return Ok(Some((bytes, uri, LinkType::Autolink)));
    }
    Ok(scan_email(text, start_ix)?.map(|(bytes, uri)| (bytes, uri, LinkType::Email)))
}

/// Returns (next_byte_offset, uri)
fn scan_uri<'a, S: Bytes + ?Sized>(
    text: Text<'a, S>,
    start_ix: usize,
) -> Result<Option<(usize, Text<'a, S>)>, S::Error> {
    let bytes = text.slice(start_ix..);
    if bytes.is_empty() || !is_ascii_alpha(bytes.byte(0)?) {
        return Ok(None);
    }

    let mut i = 1;

    while i < bytes.len() {
        let c = bytes.byte(i)?;
        i += 1;
        match c {
            c if is_ascii_alphanumeric(c) => (),
            b'.' | b'-' | b'+' => (),
            b':' => break,
            _ => return Ok(None),
        }
    }

    // scheme length must be between 2 and 32 characters long. scheme
    // must be followed by colon
    if !(3..=33).contains(&i) {
        return Ok(None);
    }

    while i < bytes.len() {
        match bytes.byte(i)? {
            b'>' => {
                return Ok(Some((
                    start_ix + i + 1,
                    text.slice(start_ix..(start_ix + i)),
                )))
            }
            b'\0'..=b' ' | b'<' => return Ok(None),
            _ => (),
        }
        i += 1;
    }

    Ok(None)
}

/// Returns (next_byte_offset, email)
fn scan_email<'a, S: Bytes + ?Sized>(
    text: Text<'a, S>,
    start_ix: usize,
) -> Result<Option<(usize, Text<'a, S>)>, S::Error> {
    let bytes = text.slice(start_ix..);
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes.byte(i)?;
        i += 1;
        match c {
            c if is_ascii_alphanumeric(c) => (),
            b'.' | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'/' | b'=' | b'?'
            | b'^' | b'_' | b'`' | b'{' | b'|' | b'}' | b'~' | b'-' => (),
            b'@' if i > 1 => break,
            _ => return Ok(None),
        }
    }

    loop {
        let label_start_ix = i;
        let mut fresh_label = true;

        while i < bytes.len() {
            match bytes.byte(i)? {
                c if is_ascii_alphanumeric(c) => (),
                b'-' if fresh_label => {
                    return Ok(None);
                }
                b'-' => (),
                _ => break,
            }
            fresh_label = false;
            i += 1;
        }

        if i == label_start_ix || i - label_start_ix > 63 || bytes.byte(i - 1)? == b'-' {
            return Ok(None);
        }

        if bytes.get(i)? != Some(b'.') {
            break;
        }
        i += 1;
    }

    if bytes.get(i)? != Some(b'>') {
        return Ok(None);
    }

    Ok(Some((
        start_ix + i + 1,
        text.slice(start_ix..(start_ix + i)),
    )))
}

/// Scan comment, declaration, or CDATA section, with initial "<!" already consumed.
/// Returns byte offset on match.
pub(crate) fn scan_inline_html_comment<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
    mut ix: usize,
    scan_guard: &mut HtmlScanGuard,
) -> Result<Option<usize>, S::Error> {
    let Some(c) = bytes.get(ix)? else {
        return Ok(None);
    };
    ix += 1;
    Ok(match c {
        // Start one byte early to also recognize <!-->.
        b'-' if ix > scan_guard.comment => {
            if bytes.get(ix)? != Some(b'-') {
                return Ok(None);
            }
            ix -= 1;

            while let Some(x) = bytes.slice(ix..).find_byte(b'-')? {
                ix += x + 1;
                scan_guard.comment = ix;
                if bytes.get(ix)? == Some(b'-') && bytes.get(ix + 1)? == Some(b'>') {
                    return Ok(Some(ix + 2));
                }
            }
            None
        }
        b'[' if bytes.slice(ix..).starts_with(b"CDATA[")? && ix > scan_guard.cdata => {
            ix += b"CDATA[".len();
            ix = bytes
                .slice(ix..)
                .find_byte(b']')?
                .map_or(bytes.len(), |x| ix + x);
            let close_brackets = scan_ch_repeat(bytes.slice(ix..), b']')?;
            ix += close_brackets;

            if close_brackets == 0 || bytes.get(ix)? != Some(b'>') {
                scan_guard.cdata = ix;
                None
            } else {
                Some(ix + 1)
            }
        }
        _ if c.is_ascii_alphabetic() && ix > scan_guard.declaration => {
            ix = bytes
                .slice(ix..)
                .find_byte(b'>')?
                .map_or(bytes.len(), |x| ix + x);
            if bytes.get(ix)? != Some(b'>') {
                scan_guard.declaration = ix;
                None
            } else {
                Some(ix + 1)
            }
        }
        _ => None,
    })
}

/// Scan processing directive, with initial "<?" already consumed.
/// Returns the next byte offset on success.
pub(crate) fn scan_inline_html_processing<S: Bytes + ?Sized>(
    bytes: Text<'_, S>,
    mut ix: usize,
    scan_guard: &mut HtmlScanGuard,
) -> Result<Option<usize>, S::Error> {
    if ix <= scan_guard.processing {
        return Ok(None);
    }
    while let Some(offset) = bytes.slice(ix..).find_byte(b'?')? {
        ix += offset + 1;
        if bytes.get(ix)? == Some(b'>') {
            return Ok(Some(ix + 1));
        }
    }
    scan_guard.processing = ix;
    Ok(None)
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn overflow_list() {
        assert!(scan_listitem(Text::new(
            &b"4444444444444444444444444444444444444444444444444444444444!"[..]
        ))
        .unwrap()
        .is_none());
    }

    #[test]
    fn overflow_by_addition() {
        assert!(scan_listitem(Text::new(&b"1844674407370955161615!"[..]))
            .unwrap()
            .is_none());
    }

    #[test]
    fn good_emails() {
        const EMAILS: &[&str] = &[
            "<a@b.c>",
            "<a@b>",
            "<a-zA-Z0-9.!#$%&'*+/=?^_`{|}~-@example.com>",
            "<a@sixty-three-letters-in-this-identifier-----------------------63>",
        ];
        for email in EMAILS {
            assert!(scan_email(Text::new(*email), 1).unwrap().is_some());
        }
    }

    #[test]
    fn bad_emails() {
        const EMAILS: &[&str] = &[
            "<@b.c>",
            "<foo@-example.com>",
            "<foo@example-.com>",
            "<a@notrailingperiod.>",
            "<a(noparens)@example.com>",
            "<\"noquotes\"@example.com>",
            "<a@sixty-four-letters-in-this-identifier-------------------------64>",
        ];
        for email in EMAILS {
            assert!(scan_email(Text::new(*email), 1).unwrap().is_none());
        }
    }
}
