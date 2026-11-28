// Copyright (c) 2014-2026 Sean McArthur
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
//

mod fold {
    #[derive(Clone, Copy)]
    pub enum Fold {
        Zero,
        One(char),
        Two(char, char),
        Three(char, char, char),
    }

    impl Iterator for Fold {
        type Item = char;
        #[inline]
        fn next(&mut self) -> Option<char> {
            match *self {
                Fold::Zero => None,
                Fold::One(one) => {
                    *self = Fold::Zero;
                    Some(one)
                }
                Fold::Two(one, two) => {
                    *self = Fold::One(two);
                    Some(one)
                }
                Fold::Three(one, two, three) => {
                    *self = Fold::Two(two, three);
                    Some(one)
                }
            }
        }

        #[inline]
        fn size_hint(&self) -> (usize, Option<usize>) {
            match *self {
                Fold::Zero => (0, Some(0)),
                Fold::One(..) => (1, Some(1)),
                Fold::Two(..) => (2, Some(2)),
                Fold::Three(..) => (3, Some(3)),
            }
        }
    }
    impl From<(char,)> for Fold {
        #[inline]
        fn from((one,): (char,)) -> Fold {
            Fold::One(one)
        }
    }

    impl From<(char, char)> for Fold {
        #[inline]
        fn from((one, two): (char, char)) -> Fold {
            Fold::Two(one, two)
        }
    }

    impl From<(char, char, char)> for Fold {
        #[inline]
        fn from((one, two, three): (char, char, char)) -> Fold {
            Fold::Three(one, two, three)
        }
    }
}

use std::io;
use std::ops::Range;

use sha2::{Digest, Sha256};

use super::{ByteSource, Cursor};

#[path = "folding/map.rs"]
mod map;

struct Folded<'a, S: ByteSource + ?Sized> {
    cursor: Cursor<'a, S>,
    pending: fold::Fold,
}

impl<'a, S: ByteSource + ?Sized> Folded<'a, S> {
    fn new(source: &'a S, range: Range<u64>) -> io::Result<Self> {
        Ok(Self {
            cursor: Cursor::new(source, range)?,
            pending: fold::Fold::Zero,
        })
    }

    fn next(&mut self) -> io::Result<Option<char>> {
        loop {
            if let Some(value) = self.pending.next() {
                return Ok(Some(value));
            }
            let Some(value) = self.cursor.next_char()? else {
                return Ok(None);
            };
            self.pending = map::lookup(value);
        }
    }
}

pub(crate) fn key<S: ByteSource + ?Sized>(source: &S, range: Range<u64>) -> io::Result<[u8; 32]> {
    let mut values = Folded::new(source, range)?;
    let mut digest = Sha256::new();
    let mut bytes = [0; 4];
    while let Some(value) = values.next()? {
        digest.update(value.encode_utf8(&mut bytes).as_bytes());
    }
    Ok(digest.finalize().into())
}

pub(crate) fn equal<L: ByteSource + ?Sized, R: ByteSource + ?Sized>(
    left: &L,
    left_range: Range<u64>,
    right: &R,
    right_range: Range<u64>,
) -> io::Result<bool> {
    let mut left = Folded::new(left, left_range)?;
    let mut right = Folded::new(right, right_range)?;
    loop {
        match (left.next()?, right.next()?) {
            (None, None) => return Ok(true),
            (Some(left), Some(right)) if left == right => {}
            _ => return Ok(false),
        }
    }
}
