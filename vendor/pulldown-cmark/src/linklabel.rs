// Copyright 2018 Google LLC
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

//! Link label parsing and matching.

use unicase::UniCase;

use crate::backend::Backend;
use crate::scanners::{is_ascii_punctuation, is_ascii_whitespace, scan_eol};
use crate::source::Text;
use crate::strings::CowStr;
use crate::text::TextBuffer;

#[derive(Debug)]
pub(crate) enum ReferenceLabel<Text> {
    Link(Text),
    Footnote(Text),
}

pub(crate) type LinkLabel<'a> = UniCase<CowStr<'a>>;

pub(crate) type FootnoteLabel<'a> = UniCase<CowStr<'a>>;

/// Scan after an opening bracket, including the closing bracket in the consumed length.
/// `Ok(None)` from the line-break handler rejects the label; `Err` aborts parsing.
pub(crate) fn scan_link_label_rest<'t, B: Backend>(
    bytes: Text<'t, B::Input>,
    linebreak_handler: &dyn Fn(Text<'t, B::Input>) -> Result<Option<usize>, B::Error>,
    is_in_table: bool,
    backend: &B,
) -> Result<Option<(usize, B::Text<'t>)>, B::Error> {
    backend.speculate_text(|| {
        let mut ix = 0;
        let mut only_white_space = true;
        let mut codepoints = 0;
        let mut label = backend.text_buffer();
        let mut mark = 0;

        loop {
            if codepoints >= 1000 {
                return Ok(None);
            }
            let Some(byte) = bytes.get(ix)? else {
                return Ok(None);
            };
            match byte {
                b'[' => return Ok(None),
                b']' => break,
                // Table recognition removes one escape before a pipe.
                b'|' if is_in_table && ix != 0 && bytes.get(ix - 1)? == Some(b'\\') => {
                    label.push_source(bytes.slice(mark..ix - 1))?;
                    label.push_char('|')?;
                    ix += 1;
                    only_white_space = false;
                    mark = ix;
                }
                b'\\' if is_in_table && bytes.get(ix + 1)? == Some(b'|') => {
                    label.push_source(bytes.slice(mark..ix))?;
                    label.push_char('|')?;
                    ix += 2;
                    codepoints += 1;
                    only_white_space = false;
                    mark = ix;
                }
                b'\\'
                    if match bytes.get(ix + 1)? {
                        Some(byte) => is_ascii_punctuation(byte),
                        None => return Ok(None),
                    } =>
                {
                    ix += 2;
                    codepoints += 2;
                    only_white_space = false;
                }
                b if is_ascii_whitespace(b) => {
                    // normalize labels by collapsing whitespaces, including linebreaks
                    let mut whitespaces = 0;
                    let mut linebreaks = 0;
                    let whitespace_start = ix;

                    while ix < bytes.len() && is_ascii_whitespace(bytes.byte(ix)?) {
                        if let Some(eol_bytes) = scan_eol(bytes.slice(ix..))? {
                            linebreaks += 1;
                            if linebreaks > 1 {
                                return Ok(None);
                            }
                            ix += eol_bytes;
                            let Some(skipped) = linebreak_handler(bytes.slice(ix..))? else {
                                return Ok(None);
                            };
                            ix += skipped;
                            whitespaces += 2; // indicate that we need to replace
                        } else {
                            whitespaces += if bytes.byte(ix)? == b' ' { 1 } else { 2 };
                            ix += 1;
                        }
                    }
                    if whitespaces > 1 {
                        label.push_source(bytes.slice(mark..whitespace_start))?;
                        label.push_char(' ')?;
                        mark = ix;
                        codepoints += ix - whitespace_start;
                    } else {
                        codepoints += 1;
                    }
                }
                b => {
                    only_white_space = false;
                    ix += 1;
                    if b & 0b1000_0000 != 0 {
                        codepoints += 1;
                    }
                }
            }
        }

        if only_white_space {
            Ok(None)
        } else {
            let value = if mark == 0 {
                backend.source_text(bytes.slice(..ix))?
            } else {
                label.push_source(bytes.slice(mark..ix))?;
                label.finish()?
            };
            let value =
                backend.trim_text(value, |value| matches!(value, ' ' | '\r' | '\n' | '\t'))?;
            Ok(Some((ix + 1, value)))
        }
    })
}

#[cfg(test)]
mod test {
    use super::scan_link_label_rest;

    #[test]
    fn whitespace_normalization() {
        for (input, expected) in [
            ("«\t\tBlurry Eyes\t\t»][blurry_eyes]", "« Blurry Eyes »"),
            ("hello\r\nworld\r\n]", "hello world"),
        ] {
            let (_, label) = scan_link_label_rest(
                crate::source::Text::new(input),
                &|_| Ok(Some(0)),
                false,
                &crate::backend::Memory,
            )
            .unwrap()
            .unwrap();
            assert_eq!(label.as_ref(), expected);
        }
    }
}
