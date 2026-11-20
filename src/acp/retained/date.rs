use std::io;

use chrono::{DateTime, Utc};
use pulldown_cmark::disk::{Cursor, TextRef};

use super::{invalid, Context};

pub(super) fn read(context: &Context, value: TextRef) -> io::Result<DateTime<Utc>> {
    let source = context.spool.view(value)?;
    let mut cursor = Cursor::new(&source, 0..value.len)?;
    let mut normalized = [0u8; 256];
    let mut length = 0;
    let mut previous = None;
    let mut prefix = 0usize;
    let mut first_number = true;
    while let Some(ch) = cursor.next_char()? {
        if ch.is_whitespace() {
            if !previous.is_some_and(char::is_whitespace) {
                push(&mut normalized, &mut length, ch)?;
            }
            previous = Some(ch);
            continue;
        }
        if ch.is_ascii_digit() {
            let signed_year = first_number && prefix == 1 && matches!(previous, Some('+' | '-'));
            let fraction = previous == Some('.');
            let mut significant = false;
            let mut digits = 0usize;
            let mut current = ch;
            loop {
                if signed_year {
                    if current != '0' || significant {
                        push(&mut normalized, &mut length, current)?;
                        significant = true;
                    }
                } else if !fraction || digits < 9 {
                    push(&mut normalized, &mut length, current)?;
                }
                digits = digits.saturating_add(1);
                match cursor.peek()? {
                    Some(byte) if byte.is_ascii_digit() => {
                        cursor.next_byte()?;
                        current = char::from(byte);
                    }
                    _ => break,
                }
            }
            if signed_year && !significant {
                push(&mut normalized, &mut length, '0')?;
            }
            first_number = false;
            previous = Some(current);
        } else {
            push(&mut normalized, &mut length, ch)?;
            prefix = prefix.saturating_add(1);
            previous = Some(ch);
        }
    }
    let text = std::str::from_utf8(&normalized[..length]).map_err(io::Error::other)?;
    text.parse::<DateTime<Utc>>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn push(destination: &mut [u8; 256], length: &mut usize, value: char) -> io::Result<()> {
    let end = length
        .checked_add(value.len_utf8())
        .ok_or_else(|| invalid("invalid retained timestamp"))?;
    let target = destination
        .get_mut(*length..end)
        .ok_or_else(|| invalid("invalid retained timestamp grammar"))?;
    value.encode_utf8(target);
    *length = end;
    Ok(())
}
