use std::convert::Infallible;

use crate::source::{Source, Text};
use crate::CowStr;

pub(crate) trait TextBuffer<'input, S: Source + ?Sized> {
    type Value;

    fn is_empty(&self) -> bool;
    fn push_source(&mut self, text: Text<'input, S>) -> Result<(), S::Error>;
    fn push_str(&mut self, text: &str) -> Result<(), S::Error>;
    fn finish(self) -> Result<Self::Value, S::Error>;

    fn push_char(&mut self, value: char) -> Result<(), S::Error> {
        let mut bytes = [0; 4];
        self.push_str(value.encode_utf8(&mut bytes))
    }
}

#[derive(Debug, Default)]
pub(crate) enum MemoryBuffer<'input> {
    #[default]
    Empty,
    Borrowed(Text<'input, str>),
    Owned(String),
}

impl<'input> TextBuffer<'input, str> for MemoryBuffer<'input> {
    type Value = CowStr<'input>;

    fn push_source(&mut self, text: Text<'input, str>) -> Result<(), Infallible> {
        if text.is_empty() {
            return Ok(());
        }
        match self {
            Self::Empty => *self = Self::Borrowed(text),
            Self::Borrowed(previous)
                if std::ptr::eq(previous.source, text.source) && previous.end == text.start =>
            {
                previous.end = text.end;
            }
            Self::Borrowed(previous) => {
                let mut value = String::with_capacity(previous.len() + text.len());
                value.push_str(previous.as_str());
                value.push_str(text.as_str());
                *self = Self::Owned(value);
            }
            Self::Owned(value) => value.push_str(text.as_str()),
        }
        Ok(())
    }

    fn push_str(&mut self, text: &str) -> Result<(), Infallible> {
        if text.is_empty() {
            return Ok(());
        }
        match self {
            Self::Empty => *self = Self::Owned(text.to_owned()),
            Self::Borrowed(previous) => {
                let mut value = String::with_capacity(previous.len() + text.len());
                value.push_str(previous.as_str());
                value.push_str(text);
                *self = Self::Owned(value);
            }
            Self::Owned(value) => value.push_str(text),
        }
        Ok(())
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Borrowed(text) => text.is_empty(),
            Self::Owned(text) => text.is_empty(),
        }
    }

    fn finish(self) -> Result<CowStr<'input>, Infallible> {
        Ok(match self {
            Self::Empty => "".into(),
            Self::Borrowed(text) => text.as_str().into(),
            Self::Owned(text) => text.into(),
        })
    }
}

#[cfg(feature = "disk")]
mod disk {
    use std::io::{self, Write};

    use super::{Text, TextBuffer};
    use crate::disk::{Spool, TextRef, Writer};

    pub(crate) struct DiskBuffer<'store> {
        spool: &'store Spool,
        content: Content<'store>,
    }

    enum Content<'store> {
        Empty,
        Borrowed(TextRef),
        Owned(Writer<'store>),
    }

    impl<'store> DiskBuffer<'store> {
        pub(crate) fn new(spool: &'store Spool) -> Self {
            Self {
                spool,
                content: Content::Empty,
            }
        }

        fn writer(&mut self) -> io::Result<&mut Writer<'store>> {
            if !matches!(self.content, Content::Owned(_)) {
                let mut writer = self.spool.writer();
                if let Content::Borrowed(reference) = &self.content {
                    self.spool.copy_to(*reference, &mut writer)?;
                }
                self.content = Content::Owned(writer);
            }
            match &mut self.content {
                Content::Owned(writer) => Ok(writer),
                _ => unreachable!("text buffer writer is initialized"),
            }
        }
    }

    impl<'input> TextBuffer<'input, Spool> for DiskBuffer<'_> {
        type Value = TextRef;

        fn push_source(&mut self, text: Text<'input, Spool>) -> io::Result<()> {
            if !std::ptr::eq(self.spool, text.source) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unknown text source",
                ));
            }
            let reference = self.spool.reference(text.start as u64..text.end as u64)?;
            if reference.len == 0 {
                return Ok(());
            }
            match &mut self.content {
                Content::Empty => self.content = Content::Borrowed(reference),
                Content::Borrowed(previous) if previous.start + previous.len == reference.start => {
                    previous.len += reference.len;
                }
                _ => {
                    let spool = self.spool;
                    let writer = self.writer()?;
                    spool.copy_to(reference, writer)?;
                }
            }
            Ok(())
        }

        fn push_str(&mut self, text: &str) -> io::Result<()> {
            if !text.is_empty() {
                self.writer()?.write_all(text.as_bytes())?;
            }
            Ok(())
        }

        fn is_empty(&self) -> bool {
            match &self.content {
                Content::Empty => true,
                Content::Borrowed(reference) => reference.len == 0,
                Content::Owned(writer) => writer.is_empty(),
            }
        }

        fn finish(self) -> io::Result<TextRef> {
            match self.content {
                Content::Empty => self.spool.reference(0..0),
                Content::Borrowed(reference) => self
                    .spool
                    .reference(reference.start..reference.start + reference.len),
                Content::Owned(writer) => writer.finish(),
            }
        }
    }
}

#[cfg(feature = "disk")]
pub(crate) use disk::DiskBuffer;
