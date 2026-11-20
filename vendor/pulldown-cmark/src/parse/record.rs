use core::cell::Cell;
use std::io;
use std::num::NonZeroUsize;

use crate::disk::{Record, TextRef};
use crate::{Alignment, BlockQuoteKind, HeadingLevel, LinkType, MetadataBlockKind};

use super::{
    AlignmentIndex, CowIndex, HeadingAttributes, HeadingIndex, InlineEl, Item, ItemBody, LinkDef,
    LinkIndex, LinkStackEl, LinkStackTy, ListFlags, TreeIndex,
};

impl Record for Item {
    const SIZE: usize = 48;

    fn encode(&self, bytes: &mut [u8]) {
        use ItemBody::*;
        let (tag, a, b, c): (u64, u64, u64, u64) = match &self.body {
            MaybeEmphasis(count, open, close) => {
                (0, *count as u64, u64::from(*open), u64::from(*close))
            }
            MaybeMath(open, close, context) => {
                (1, u64::from(*open), u64::from(*close), u64::from(*context))
            }
            MaybeSmartQuote(byte, open, close) => {
                (2, u64::from(*byte), u64::from(*open), u64::from(*close))
            }
            MaybeCode(count, escaped) => (3, *count as u64, u64::from(*escaped), 0),
            MaybeHtml => (4, 0, 0, 0),
            MaybeLinkOpen => (5, 0, 0, 0),
            MaybeLinkClose(reference) => (6, u64::from(*reference), 0, 0),
            MaybeImage => (7, 0, 0, 0),
            Emphasis => (8, 0, 0, 0),
            Strong => (9, 0, 0, 0),
            Strikethrough => (10, 0, 0, 0),
            Superscript => (11, 0, 0, 0),
            Subscript => (12, 0, 0, 0),
            Math(index, display) => (13, index.0 as u64, u64::from(*display), 0),
            Code(index) => (14, index.0 as u64, 0, 0),
            Link(index) => (15, index.0 as u64, 0, 0),
            Image(index) => (16, index.0 as u64, 0, 0),
            FootnoteReference(index) => (17, index.0 as u64, 0, 0),
            TaskListMarker(checked) => (18, u64::from(*checked), 0, 0),
            InlineHtml => (19, 0, 0, 0),
            OwnedInlineHtml(index) => (20, index.0 as u64, 0, 0),
            SynthesizeText(index) => (21, index.0 as u64, 0, 0),
            SynthesizeChar(value) => (22, u64::from(u32::from(*value)), 0, 0),
            Html => (23, 0, 0, 0),
            Text { backslash_escaped } => (24, u64::from(*backslash_escaped), 0, 0),
            SoftBreak => (25, 0, 0, 0),
            HardBreak(escaped) => (26, u64::from(*escaped), 0, 0),
            Root => (27, 0, 0, 0),
            Paragraph => (28, 0, 0, 0),
            TightParagraph => (29, 0, 0, 0),
            Rule => (30, 0, 0, 0),
            Heading(level, index) => (
                31,
                *level as u64,
                index.map_or(0, |index| index.0.get() as u64),
                0,
            ),
            FencedCodeBlock(index) => (32, index.0 as u64, 0, 0),
            IndentCodeBlock => (33, 0, 0, 0),
            HtmlBlock => (34, 0, 0, 0),
            BlockQuote(kind) => (35, quote_word(*kind), 0, 0),
            List(flags, byte, start) => {
                (36, u64::from(flags.get().bits()), u64::from(*byte), *start)
            }
            ListItem(indent) => (37, *indent as u64, 0, 0),
            FootnoteDefinition(index) => (38, index.0 as u64, 0, 0),
            MetadataBlock(kind) => (
                39,
                match kind {
                    MetadataBlockKind::YamlStyle => 0,
                    MetadataBlockKind::PlusesStyle => 1,
                },
                0,
                0,
            ),
            DefinitionList(flags) => (40, u64::from(flags.get().bits()), 0, 0),
            MaybeDefinitionListTitle => (41, 0, 0, 0),
            DefinitionListTitle => (42, 0, 0, 0),
            DefinitionListDefinition(indent) => (43, *indent as u64, 0, 0),
            Table(index) => (44, index.0 as u64, 0, 0),
            TableHead => (45, 0, 0, 0),
            TableRow => (46, 0, 0, 0),
            TableCell => (47, 0, 0, 0),
        };
        let words = [self.start as u64, self.end as u64, tag, a, b, c];
        for (word, output) in words.into_iter().zip(bytes.chunks_exact_mut(8)) {
            word.encode(output);
        }
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        use ItemBody::*;
        let mut words = [0; 6];
        for (word, input) in words.iter_mut().zip(bytes.chunks_exact(8)) {
            *word = u64::decode(input)?;
        }
        let [start, end, tag, a, b, c] = words;
        let body = match tag {
            0 => MaybeEmphasis(index(a)?, boolean(b)?, boolean(c)?),
            1 => MaybeMath(boolean(a)?, boolean(b)?, byte(c)?),
            2 => MaybeSmartQuote(byte(a)?, boolean(b)?, boolean(c)?),
            3 => MaybeCode(index(a)?, boolean(b)?),
            4 => MaybeHtml,
            5 => MaybeLinkOpen,
            6 => MaybeLinkClose(boolean(a)?),
            7 => MaybeImage,
            8 => Emphasis,
            9 => Strong,
            10 => Strikethrough,
            11 => Superscript,
            12 => Subscript,
            13 => Math(CowIndex(index(a)?), boolean(b)?),
            14 => Code(CowIndex(index(a)?)),
            15 => Link(LinkIndex(index(a)?)),
            16 => Image(LinkIndex(index(a)?)),
            17 => FootnoteReference(CowIndex(index(a)?)),
            18 => TaskListMarker(boolean(a)?),
            19 => InlineHtml,
            20 => OwnedInlineHtml(CowIndex(index(a)?)),
            21 => SynthesizeText(CowIndex(index(a)?)),
            22 => SynthesizeChar(
                char::from_u32(u32::try_from(a).map_err(invalid)?).ok_or_else(invalid_record)?,
            ),
            23 => Html,
            24 => Text {
                backslash_escaped: boolean(a)?,
            },
            25 => SoftBreak,
            26 => HardBreak(boolean(a)?),
            27 => Root,
            28 => Paragraph,
            29 => TightParagraph,
            30 => Rule,
            31 => Heading(
                HeadingLevel::try_from(index(a)?).map_err(|_| invalid_record())?,
                NonZeroUsize::new(index(b)?).map(HeadingIndex),
            ),
            32 => FencedCodeBlock(CowIndex(index(a)?)),
            33 => IndentCodeBlock,
            34 => HtmlBlock,
            35 => BlockQuote(quote_kind(a)?),
            36 => List(Cell::new(flags(a)?), byte(b)?, c),
            37 => ListItem(index(a)?),
            38 => FootnoteDefinition(CowIndex(index(a)?)),
            39 => MetadataBlock(match a {
                0 => MetadataBlockKind::YamlStyle,
                1 => MetadataBlockKind::PlusesStyle,
                _ => return Err(invalid_record()),
            }),
            40 => DefinitionList(Cell::new(flags(a)?)),
            41 => MaybeDefinitionListTitle,
            42 => DefinitionListTitle,
            43 => DefinitionListDefinition(index(a)?),
            44 => Table(AlignmentIndex(index(a)?)),
            45 => TableHead,
            46 => TableRow,
            47 => TableCell,
            _ => return Err(invalid_record()),
        };
        Ok(Item {
            start: index(start)?,
            end: index(end)?,
            body,
        })
    }
}

fn invalid_record() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid parser item record")
}

fn invalid(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn index(value: u64) -> io::Result<usize> {
    usize::try_from(value).map_err(invalid)
}

fn byte(value: u64) -> io::Result<u8> {
    u8::try_from(value).map_err(invalid)
}

fn boolean(value: u64) -> io::Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid_record()),
    }
}

fn flags(value: u64) -> io::Result<ListFlags> {
    ListFlags::from_bits(byte(value)?).ok_or_else(invalid_record)
}

fn quote_word(value: Option<BlockQuoteKind>) -> u64 {
    match value {
        None => 0,
        Some(BlockQuoteKind::Note) => 1,
        Some(BlockQuoteKind::Tip) => 2,
        Some(BlockQuoteKind::Important) => 3,
        Some(BlockQuoteKind::Warning) => 4,
        Some(BlockQuoteKind::Caution) => 5,
    }
}

fn quote_kind(value: u64) -> io::Result<Option<BlockQuoteKind>> {
    match value {
        0 => Ok(None),
        1 => Ok(Some(BlockQuoteKind::Note)),
        2 => Ok(Some(BlockQuoteKind::Tip)),
        3 => Ok(Some(BlockQuoteKind::Important)),
        4 => Ok(Some(BlockQuoteKind::Warning)),
        5 => Ok(Some(BlockQuoteKind::Caution)),
        _ => Err(invalid_record()),
    }
}

impl Record for InlineEl {
    const SIZE: usize = 32;

    fn encode(&self, bytes: &mut [u8]) {
        self.start.encode(&mut bytes[..8]);
        let values = [
            self.count as u64,
            self.run_length as u64,
            u64::from(self.c) | (u64::from(self.both) << 8),
        ];
        for (word, value) in bytes[8..].chunks_exact_mut(8).zip(values) {
            word.copy_from_slice(&value.to_le_bytes());
        }
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let bytes: &[u8; Self::SIZE] = bytes.try_into().map_err(invalid)?;
        let count = u64::from_le_bytes(bytes[8..16].try_into().map_err(invalid)?);
        let run_length = u64::from_le_bytes(bytes[16..24].try_into().map_err(invalid)?);
        let flags = u64::from_le_bytes(bytes[24..32].try_into().map_err(invalid)?);
        if flags & !0x1ff != 0 {
            return Err(invalid_record());
        }
        Ok(Self {
            start: TreeIndex::decode(&bytes[..8])?,
            count: index(count)?,
            run_length: index(run_length)?,
            c: flags as u8,
            both: flags & 0x100 != 0,
        })
    }
}

impl Record for LinkStackEl {
    const SIZE: usize = 16;

    fn encode(&self, bytes: &mut [u8]) {
        self.node.encode(&mut bytes[..8]);
        let kind: u64 = match self.ty {
            LinkStackTy::Link => 0,
            LinkStackTy::Image => 1,
            LinkStackTy::Disabled => 2,
        };
        bytes[8..].copy_from_slice(&kind.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let bytes: &[u8; Self::SIZE] = bytes.try_into().map_err(invalid)?;
        let kind = u64::from_le_bytes(bytes[8..16].try_into().map_err(invalid)?);
        Ok(Self {
            node: TreeIndex::decode(&bytes[..8])?,
            ty: match kind {
                0 => LinkStackTy::Link,
                1 => LinkStackTy::Image,
                2 => LinkStackTy::Disabled,
                _ => return Err(invalid_record()),
            },
        })
    }
}

impl Record for (TreeIndex, bool, bool) {
    const SIZE: usize = 16;

    fn encode(&self, bytes: &mut [u8]) {
        self.0.encode(&mut bytes[..8]);
        let flags = u64::from(self.1) | (u64::from(self.2) << 1);
        bytes[8..].copy_from_slice(&flags.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let bytes: &[u8; Self::SIZE] = bytes.try_into().map_err(invalid)?;
        let flags = u64::from_le_bytes(bytes[8..16].try_into().map_err(invalid)?);
        if flags & !3 != 0 {
            return Err(invalid_record());
        }
        Ok((
            TreeIndex::decode(&bytes[..8])?,
            flags & 1 != 0,
            flags & 2 != 0,
        ))
    }
}

impl Record for Alignment {
    const SIZE: usize = 1;

    fn encode(&self, bytes: &mut [u8]) {
        bytes[0] = match self {
            Self::None => 0,
            Self::Left => 1,
            Self::Center => 2,
            Self::Right => 3,
        };
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        match bytes {
            [0] => Ok(Self::None),
            [1] => Ok(Self::Left),
            [2] => Ok(Self::Center),
            [3] => Ok(Self::Right),
            _ => Err(invalid_record()),
        }
    }
}

impl Record for LinkType {
    const SIZE: usize = 1;

    fn encode(&self, bytes: &mut [u8]) {
        bytes[0] = match self {
            Self::Inline => 0,
            Self::Reference => 1,
            Self::ReferenceUnknown => 2,
            Self::Collapsed => 3,
            Self::CollapsedUnknown => 4,
            Self::Shortcut => 5,
            Self::ShortcutUnknown => 6,
            Self::Autolink => 7,
            Self::Email => 8,
            Self::WikiLink { has_pothole } => 9 + u8::from(*has_pothole),
        };
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        match bytes {
            [0] => Ok(Self::Inline),
            [1] => Ok(Self::Reference),
            [2] => Ok(Self::ReferenceUnknown),
            [3] => Ok(Self::Collapsed),
            [4] => Ok(Self::CollapsedUnknown),
            [5] => Ok(Self::Shortcut),
            [6] => Ok(Self::ShortcutUnknown),
            [7] => Ok(Self::Autolink),
            [8] => Ok(Self::Email),
            [9] => Ok(Self::WikiLink { has_pothole: false }),
            [10] => Ok(Self::WikiLink { has_pothole: true }),
            _ => Err(invalid_record()),
        }
    }
}

impl Record for (LinkType, TextRef, TextRef, TextRef) {
    const SIZE: usize = 1 + 3 * TextRef::SIZE;

    fn encode(&self, bytes: &mut [u8]) {
        self.0.encode(&mut bytes[..1]);
        for (bytes, value) in bytes[1..]
            .chunks_exact_mut(TextRef::SIZE)
            .zip([&self.1, &self.2, &self.3])
        {
            value.encode(bytes);
        }
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let bytes: &[u8; Self::SIZE] = bytes.try_into().map_err(invalid)?;
        Ok((
            LinkType::decode(&bytes[..1])?,
            TextRef::decode(&bytes[1..25])?,
            TextRef::decode(&bytes[25..49])?,
            TextRef::decode(&bytes[49..73])?,
        ))
    }
}

impl<Text: Record, Classes: Record, Attributes: Record> Record
    for HeadingAttributes<Text, Classes, Attributes>
{
    const SIZE: usize = Option::<Text>::SIZE + Classes::SIZE + Attributes::SIZE;

    fn encode(&self, bytes: &mut [u8]) {
        let id_end = Option::<Text>::SIZE;
        let classes_end = id_end + Classes::SIZE;
        self.id.encode(&mut bytes[..id_end]);
        self.classes.encode(&mut bytes[id_end..classes_end]);
        self.attrs.encode(&mut bytes[classes_end..]);
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(invalid_record());
        }
        let id_end = Option::<Text>::SIZE;
        let classes_end = id_end + Classes::SIZE;
        Ok(Self {
            id: Option::<Text>::decode(&bytes[..id_end])?,
            classes: Classes::decode(&bytes[id_end..classes_end])?,
            attrs: Attributes::decode(&bytes[classes_end..])?,
        })
    }
}

impl<Text: Record> Record for LinkDef<Text> {
    const SIZE: usize = Text::SIZE + Option::<Text>::SIZE + 16;

    fn encode(&self, bytes: &mut [u8]) {
        let title_end = Text::SIZE + Option::<Text>::SIZE;
        self.dest.encode(&mut bytes[..Text::SIZE]);
        self.title.encode(&mut bytes[Text::SIZE..title_end]);
        (self.span.start as u64).encode(&mut bytes[title_end..title_end + 8]);
        (self.span.end as u64).encode(&mut bytes[title_end + 8..]);
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(invalid_record());
        }
        let title_end = Text::SIZE + Option::<Text>::SIZE;
        let start = usize::try_from(u64::decode(&bytes[title_end..title_end + 8])?)
            .map_err(|_| invalid_record())?;
        let end =
            usize::try_from(u64::decode(&bytes[title_end + 8..])?).map_err(|_| invalid_record())?;
        if start > end {
            return Err(invalid_record());
        }
        Ok(Self {
            dest: Text::decode(&bytes[..Text::SIZE])?,
            title: Option::<Text>::decode(&bytes[Text::SIZE..title_end])?,
            span: start..end,
        })
    }
}
