use std::{convert::Infallible, ops::Range};

use ratatui::{
    style::Style,
    text::{Line, Span},
};

pub(crate) mod ansi;
pub(crate) mod disk;
pub(crate) mod glyph;
mod style;
pub(crate) mod window;
pub(crate) mod wrap;

pub(crate) trait ListStack {
    type Error;
    fn len(&self) -> u64;
    fn push(&mut self, value: Option<u64>) -> Result<(), Self::Error>;
    fn pop(&mut self) -> Result<(), Self::Error>;
    fn last(&self) -> Result<Option<Option<u64>>, Self::Error>;
    fn set_last(&mut self, value: Option<u64>) -> Result<(), Self::Error>;
}

pub(crate) enum TablePart<T> {
    Text(T),
    Separator,
}
pub(crate) trait Table<T> {
    type Error;
    fn push(&mut self, part: TablePart<T>) -> Result<(), Self::Error>;
    fn clear(&mut self) -> Result<(), Self::Error>;
    fn byte_len(&self) -> u64;
    fn drain(&mut self) -> impl Iterator<Item = Result<TablePart<T>, Self::Error>>;
}

pub(crate) trait Source {
    type Text;
    type Error;
    type Lists: ListStack<Error = Self::Error>;
    type Table: Table<Self::Text, Error = Self::Error>;
    fn lists(&self) -> Result<Self::Lists, Self::Error>;
    fn table(&self) -> Result<Self::Table, Self::Error>;
    fn len(&self, text: &Self::Text) -> u64;
    fn slice(&self, text: &Self::Text, range: Range<u64>) -> Result<Self::Text, Self::Error>;
    fn find(&self, text: &Self::Text, byte: u8, from: u64) -> Result<Option<u64>, Self::Error>;
    fn prefix_matches(
        &self,
        destination: &Self::Text,
        offset: u64,
        text: &Self::Text,
    ) -> Result<bool, Self::Error>;
}

pub(crate) trait Sink<T> {
    type Error;
    fn text(&mut self, value: T, style: Style) -> Result<(), Self::Error>;
    fn literal(&mut self, value: &'static str, style: Style) -> Result<(), Self::Error>;
    fn spaces(&mut self, count: u64, style: Style) -> Result<(), Self::Error>;
    fn number(&mut self, value: u64, style: Style) -> Result<(), Self::Error>;
    fn blank_lines(&mut self, count: u64) -> Result<(), Self::Error>;
    fn end_line(&mut self) -> Result<(), Self::Error>;
    fn standalone_line(
        &mut self,
        parts: impl Iterator<Item = Result<TablePart<T>, Self::Error>>,
        bytes: u64,
        style: Style,
        leading_blanks: u64,
    ) -> Result<(), Self::Error>;
    fn standalone_literal(
        &mut self,
        value: &'static str,
        style: Style,
        leading_blanks: u64,
    ) -> Result<(), Self::Error>;
}

pub(crate) struct Memory;
impl Source for Memory {
    type Text = String;
    type Error = Infallible;
    type Lists = Vec<Option<u64>>;
    type Table = Vec<TablePart<String>>;
    fn lists(&self) -> Result<Self::Lists, Infallible> {
        Ok(Vec::new())
    }
    fn table(&self) -> Result<Self::Table, Infallible> {
        Ok(Vec::new())
    }
    fn len(&self, text: &String) -> u64 {
        text.len() as u64
    }
    fn slice(&self, text: &String, range: Range<u64>) -> Result<String, Infallible> {
        Ok(text[range.start as usize..range.end as usize].to_owned())
    }
    fn find(&self, text: &String, byte: u8, from: u64) -> Result<Option<u64>, Infallible> {
        Ok(text.as_bytes()[from as usize..]
            .iter()
            .position(|value| *value == byte)
            .map(|index| from + index as u64))
    }
    fn prefix_matches(
        &self,
        destination: &String,
        offset: u64,
        text: &String,
    ) -> Result<bool, Infallible> {
        let end = offset.checked_add(text.len() as u64);
        Ok(
            end.and_then(|end| destination.as_bytes().get(offset as usize..end as usize))
                == Some(text.as_bytes()),
        )
    }
}
impl ListStack for Vec<Option<u64>> {
    type Error = Infallible;
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }
    fn push(&mut self, value: Option<u64>) -> Result<(), Infallible> {
        Vec::push(self, value);
        Ok(())
    }
    fn pop(&mut self) -> Result<(), Infallible> {
        Vec::pop(self);
        Ok(())
    }
    fn last(&self) -> Result<Option<Option<u64>>, Infallible> {
        Ok(self.as_slice().last().copied())
    }
    fn set_last(&mut self, value: Option<u64>) -> Result<(), Infallible> {
        *self.as_mut_slice().last_mut().expect("open Markdown list") = value;
        Ok(())
    }
}
impl Table<String> for Vec<TablePart<String>> {
    type Error = Infallible;
    fn push(&mut self, part: TablePart<String>) -> Result<(), Infallible> {
        Vec::push(self, part);
        Ok(())
    }
    fn clear(&mut self) -> Result<(), Infallible> {
        Vec::clear(self);
        Ok(())
    }
    fn byte_len(&self) -> u64 {
        self.iter()
            .map(|part| match part {
                TablePart::Text(text) => text.len() as u64,
                TablePart::Separator => " │ ".len() as u64,
            })
            .sum()
    }
    fn drain(&mut self) -> impl Iterator<Item = Result<TablePart<String>, Infallible>> {
        Vec::drain(self, ..).map(Ok)
    }
}

#[derive(Default)]
pub(crate) struct MemoryLines {
    pub lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
}
impl Sink<String> for MemoryLines {
    type Error = Infallible;
    fn text(&mut self, value: String, style: Style) -> Result<(), Infallible> {
        self.current.push(Span::styled(value, style));
        Ok(())
    }
    fn literal(&mut self, value: &'static str, style: Style) -> Result<(), Infallible> {
        self.current.push(Span::styled(value, style));
        Ok(())
    }
    fn spaces(&mut self, count: u64, style: Style) -> Result<(), Infallible> {
        if count != 0 {
            self.current
                .push(Span::styled(" ".repeat(count as usize), style));
        }
        Ok(())
    }
    fn number(&mut self, value: u64, style: Style) -> Result<(), Infallible> {
        self.current.push(Span::styled(value.to_string(), style));
        Ok(())
    }
    fn blank_lines(&mut self, count: u64) -> Result<(), Infallible> {
        self.lines
            .extend(std::iter::repeat_with(Line::default).take(count as usize));
        Ok(())
    }
    fn end_line(&mut self) -> Result<(), Infallible> {
        self.lines
            .push(Line::from(std::mem::take(&mut self.current)));
        Ok(())
    }
    fn standalone_line(
        &mut self,
        parts: impl Iterator<Item = Result<TablePart<String>, Infallible>>,
        bytes: u64,
        style: Style,
        leading_blanks: u64,
    ) -> Result<(), Infallible> {
        self.blank_lines(leading_blanks)?;
        let mut text = String::with_capacity(bytes as usize);
        for part in parts {
            match part? {
                TablePart::Text(value) => text.push_str(&value),
                TablePart::Separator => text.push_str(" │ "),
            }
        }
        self.lines.push(Line::from(Span::styled(text, style)));
        Ok(())
    }
    fn standalone_literal(
        &mut self,
        value: &'static str,
        style: Style,
        leading_blanks: u64,
    ) -> Result<(), Infallible> {
        self.blank_lines(leading_blanks)?;
        self.lines.push(Line::from(Span::styled(value, style)));
        Ok(())
    }
}

pub(crate) fn infallible<T>(value: Result<T, Infallible>) -> T {
    match value {
        Ok(value) => value,
        Err(error) => match error {},
    }
}
