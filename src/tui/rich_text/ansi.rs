// Derived from ansi-to-tui 8.0.1 (MIT); see THIRD_PARTY_NOTICES.md.

use std::{fmt, io, ops::Range};

use pulldown_cmark::disk::ByteSource;
use ratatui::style::{Color, Modifier, Style};

use super::Sink;

#[derive(Debug)]
pub(crate) enum Error<E> {
    Source(io::Error),
    Sink(E),
}
impl<E> From<io::Error> for Error<E> {
    fn from(error: io::Error) -> Self {
        Self::Source(error)
    }
}
impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::Sink(error) => error.fmt(formatter),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for Error<E> {}
impl Error<io::Error> {
    pub fn into_io(self) -> io::Error {
        match self {
            Self::Source(error) | Self::Sink(error) => error,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Text,
    Terminal,
}

/// Preserve ansi-to-tui 8.0.1 styling while emitting payload ranges incrementally.
pub(crate) fn render<S: ByteSource + ?Sized, T, K: Sink<T>>(
    source: &S,
    mode: Mode,
    text: impl FnMut(Range<u64>) -> Result<T, K::Error>,
    sink: &mut K,
) -> Result<(), Error<K::Error>> {
    render_inner(source, None, mode, text, sink)
}

pub(crate) fn render_str<T, K: Sink<T>>(
    source: &str,
    mode: Mode,
    text: impl FnMut(Range<u64>) -> Result<T, K::Error>,
    sink: &mut K,
) -> Result<(), Error<K::Error>> {
    render_inner(source, Some(source.as_bytes()), mode, text, sink)
}

fn render_inner<S: ByteSource + ?Sized, T, K: Sink<T>>(
    source: &S,
    contiguous: Option<&[u8]>,
    mode: Mode,
    mut text: impl FnMut(Range<u64>) -> Result<T, K::Error>,
    sink: &mut K,
) -> Result<(), Error<K::Error>> {
    let length = source.len();
    let mut buffer = Search {
        contiguous,
        bytes: [0; 8192],
        start: 0,
        length: 0,
    };
    let ansi = matches!(mode, Mode::Terminal)
        || find(source, 0, length, &mut buffer, |byte| byte == 27)?.is_some();
    if length == 0 && ansi {
        sink.end_line().map_err(Error::Sink)?;
    }
    let mut start = 0;
    let mut last = Style::default();
    while start < length {
        let newline = find(source, start, length, &mut buffer, |byte| {
            byte == b'\n' || (ansi && byte == b'\r')
        })?;
        let mut end = newline.map_or(length, |(position, _)| position);
        if !ansi && newline.is_some() && end > start && source.byte_at(end - 1)? == Some(b'\r') {
            end -= 1;
        }
        if ansi {
            let mut position = start;
            while position < end {
                let previous = last;
                let mut style = previous;
                if source.byte_at(position)? == Some(27) {
                    match sgr(source, position, end, previous)? {
                        Some((next, candidate)) => {
                            position = next;
                            style = previous.patch(candidate);
                        }
                        None => position = escape(source, position, end, &mut buffer)?,
                    }
                }
                let next = find(source, position, end, &mut buffer, |byte| byte == 27)?
                    .map_or(end, |(position, _)| position);
                if next != position {
                    sink.text(text(position..next).map_err(Error::Sink)?, style)
                        .map_err(Error::Sink)?;
                }
                last = previous.patch(style);
                position = next;
            }
        } else {
            sink.text(text(start..end).map_err(Error::Sink)?, Style::default())
                .map_err(Error::Sink)?;
        }
        sink.end_line().map_err(Error::Sink)?;
        let Some((position, byte)) = newline else {
            break;
        };
        start = position + 1;
        if ansi && byte == b'\r' && start < length && source.byte_at(start)? == Some(b'\n') {
            start += 1;
        }
    }
    Ok(())
}

struct Search<'a> {
    contiguous: Option<&'a [u8]>,
    bytes: [u8; 8192],
    start: u64,
    length: usize,
}
fn find<S: ByteSource + ?Sized>(
    source: &S,
    mut start: u64,
    end: u64,
    cache: &mut Search<'_>,
    predicate: impl Fn(u8) -> bool,
) -> io::Result<Option<(u64, u8)>> {
    if let Some(bytes) = cache.contiguous {
        return Ok(bytes[start as usize..end as usize]
            .iter()
            .position(|byte| predicate(*byte))
            .map(|index| (start + index as u64, bytes[start as usize + index])));
    }
    while start < end {
        if cache.length == 0 || start < cache.start || start - cache.start >= cache.length as u64 {
            let count = (source.len() - start).min(cache.bytes.len() as u64) as usize;
            cache.length = source.read_at(start, &mut cache.bytes[..count])?;
            cache.start = start;
            if cache.length == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ANSI source disappeared",
                ));
            }
        }
        let offset = (start - cache.start) as usize;
        let count = (end - start).min((cache.length - offset) as u64) as usize;
        if let Some(index) = cache.bytes[offset..offset + count]
            .iter()
            .position(|byte| predicate(*byte))
        {
            return Ok(Some((start + index as u64, cache.bytes[offset + index])));
        }
        start += count as u64;
    }
    Ok(None)
}
fn byte<S: ByteSource + ?Sized>(source: &S, position: u64, end: u64) -> io::Result<Option<u8>> {
    if position < end {
        source.byte_at(position)
    } else {
        Ok(None)
    }
}
fn optional_semicolon<S: ByteSource + ?Sized>(
    source: &S,
    position: u64,
    end: u64,
) -> io::Result<u64> {
    Ok(position + u64::from(byte(source, position, end)? == Some(b';')))
}
fn unsigned<S: ByteSource + ?Sized>(
    source: &S,
    position: u64,
    end: u64,
    maximum: u64,
) -> io::Result<Option<(u64, u64)>> {
    let mut next = position;
    let mut value = 0u64;
    while let Some(digit) = byte(source, next, end)?.filter(u8::is_ascii_digit) {
        let Some(number) = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            .filter(|value| *value <= maximum)
        else {
            return Ok(None);
        };
        value = number;
        next += 1;
    }
    Ok((next != position).then_some((next, value)))
}
fn color<S: ByteSource + ?Sized>(
    source: &S,
    mut position: u64,
    end: u64,
) -> io::Result<Option<(u64, Color)>> {
    match byte(source, position, end)? {
        Some(b'+') => position += 1,
        Some(b'-') => return Ok(None),
        _ => {}
    }
    let Some((next, kind)) = unsigned(source, position, end, i64::MAX as u64)? else {
        return Ok(None);
    };
    position = next;
    if byte(source, position, end)? != Some(b';') {
        return Ok(None);
    }
    position = optional_semicolon(source, position + 1, end)?;
    match kind {
        2 => {
            let Some((next, r)) = unsigned(source, position, end, 255)? else {
                return Ok(None);
            };
            if byte(source, next, end)? != Some(b';') {
                return Ok(None);
            }
            let Some((next, g)) = unsigned(source, next + 1, end, 255)? else {
                return Ok(None);
            };
            if byte(source, next, end)? != Some(b';') {
                return Ok(None);
            }
            let Some((next, b)) = unsigned(source, next + 1, end, 255)? else {
                return Ok(None);
            };
            Ok(Some((next, Color::Rgb(r as u8, g as u8, b as u8))))
        }
        5 => Ok(unsigned(source, position, end, 255)?
            .map(|(next, index)| (next, Color::Indexed(index as u8)))),
        _ => Ok(None),
    }
}
fn sgr<S: ByteSource + ?Sized>(
    source: &S,
    start: u64,
    end: u64,
    previous: Style,
) -> io::Result<Option<(u64, Style)>> {
    if byte(source, start + 1, end)? != Some(b'[') {
        return Ok(None);
    }
    let mut position = start + 2;
    let mut style = previous;
    let mut any = false;
    while let Some((next, code)) = unsigned(source, position, end, 255)? {
        let mut next = next;
        let color = if matches!(code, 38 | 48) {
            let Some((position, color)) =
                color(source, optional_semicolon(source, next, end)?, end)?
            else {
                break;
            };
            next = position;
            Some(color)
        } else {
            None
        };
        style = apply(style, code as u8, color);
        any = true;
        position = optional_semicolon(source, next, end)?;
    }
    if byte(source, position, end)? != Some(b'm') {
        return Ok(None);
    }
    Ok(Some((
        position + 1,
        if any { style } else { Style::reset() },
    )))
}
fn escape<S: ByteSource + ?Sized>(
    source: &S,
    start: u64,
    end: u64,
    buffer: &mut Search<'_>,
) -> io::Result<u64> {
    match byte(source, start + 1, end)? {
        Some(b'[') => Ok(find(source, start + 2, end, buffer, |byte| {
            byte.is_ascii_alphabetic()
        })?
        .map_or(end, |(position, _)| position + 1)),
        Some(b']') => Ok(find(source, start + 2, end, buffer, |byte| byte == 7)?
            .map_or(end, |(position, _)| position + 1)),
        _ => Ok(start + 1),
    }
}
fn apply(mut style: Style, code: u8, color: Option<Color>) -> Style {
    match code {
        0 => style = Style::reset(),
        1 => style = style.add_modifier(Modifier::BOLD),
        2 => style = style.add_modifier(Modifier::DIM),
        3 => style = style.add_modifier(Modifier::ITALIC),
        4 => style = style.add_modifier(Modifier::UNDERLINED),
        5 => style = style.add_modifier(Modifier::SLOW_BLINK),
        6 => style = style.add_modifier(Modifier::RAPID_BLINK),
        7 => style = style.add_modifier(Modifier::REVERSED),
        8 => style = style.add_modifier(Modifier::HIDDEN),
        9 => style = style.add_modifier(Modifier::CROSSED_OUT),
        22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
        23 => style = style.remove_modifier(Modifier::ITALIC),
        24 => style = style.remove_modifier(Modifier::UNDERLINED),
        25 => style = style.remove_modifier(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK),
        27 => style = style.remove_modifier(Modifier::REVERSED),
        28 => style = style.remove_modifier(Modifier::HIDDEN),
        29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
        30..=37 => style = style.fg(basic_color(code - 30, false)),
        38 => {
            if let Some(color) = color {
                style = style.fg(color);
            }
        }
        39 => style = style.fg(Color::Reset),
        40..=47 => style = style.bg(basic_color(code - 40, false)),
        48 => {
            if let Some(color) = color {
                style = style.bg(color);
            }
        }
        49 => style = style.bg(Color::Reset),
        90..=97 => style = style.fg(basic_color(code - 90, true)),
        100..=107 => style = style.bg(basic_color(code - 100, true)),
        _ => {}
    }
    style
}
fn basic_color(index: u8, bright: bool) -> Color {
    match (index, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        (7, true) => Color::White,
        _ => unreachable!("eight ANSI base colors"),
    }
}
