use pulldown_cmark::disk::Record;
use ratatui::style::{Color, Modifier, Style};
use std::io;

#[derive(Clone, Copy)]
pub(super) struct StoredStyle(pub Style);
impl Record for StoredStyle {
    const SIZE: usize = 28;
    fn encode(&self, bytes: &mut [u8]) {
        bytes[..4].copy_from_slice(&color(self.0.fg));
        bytes[4..8].copy_from_slice(&color(self.0.bg));
        bytes[8..12].copy_from_slice(&color(self.0.underline_color));
        bytes[12..20].copy_from_slice(&u64::from(self.0.add_modifier.bits()).to_le_bytes());
        bytes[20..28].copy_from_slice(&u64::from(self.0.sub_modifier.bits()).to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(invalid());
        }
        let add = u64::from_le_bytes(bytes[12..20].try_into().map_err(io::Error::other)?);
        let sub = u64::from_le_bytes(bytes[20..28].try_into().map_err(io::Error::other)?);
        Ok(Self(Style {
            fg: decode_color(&bytes[..4])?,
            bg: decode_color(&bytes[4..8])?,
            underline_color: decode_color(&bytes[8..12])?,
            add_modifier: Modifier::from_bits(add.try_into().map_err(io::Error::other)?)
                .ok_or_else(invalid)?,
            sub_modifier: Modifier::from_bits(sub.try_into().map_err(io::Error::other)?)
                .ok_or_else(invalid)?,
        }))
    }
}
fn color(value: Option<Color>) -> [u8; 4] {
    use Color::*;
    match value {
        None => [0, 0, 0, 0],
        Some(Reset) => [1, 0, 0, 0],
        Some(Black) => [2, 0, 0, 0],
        Some(Red) => [3, 0, 0, 0],
        Some(Green) => [4, 0, 0, 0],
        Some(Yellow) => [5, 0, 0, 0],
        Some(Blue) => [6, 0, 0, 0],
        Some(Magenta) => [7, 0, 0, 0],
        Some(Cyan) => [8, 0, 0, 0],
        Some(Gray) => [9, 0, 0, 0],
        Some(DarkGray) => [10, 0, 0, 0],
        Some(LightRed) => [11, 0, 0, 0],
        Some(LightGreen) => [12, 0, 0, 0],
        Some(LightYellow) => [13, 0, 0, 0],
        Some(LightBlue) => [14, 0, 0, 0],
        Some(LightMagenta) => [15, 0, 0, 0],
        Some(LightCyan) => [16, 0, 0, 0],
        Some(White) => [17, 0, 0, 0],
        Some(Indexed(index)) => [18, index, 0, 0],
        Some(Rgb(r, g, b)) => [19, r, g, b],
    }
}
fn decode_color(bytes: &[u8]) -> io::Result<Option<Color>> {
    use Color::*;
    let [tag, a, b, c]: [u8; 4] = bytes.try_into().map_err(io::Error::other)?;
    Ok(match tag {
        0 => None,
        1 => Some(Reset),
        2 => Some(Black),
        3 => Some(Red),
        4 => Some(Green),
        5 => Some(Yellow),
        6 => Some(Blue),
        7 => Some(Magenta),
        8 => Some(Cyan),
        9 => Some(Gray),
        10 => Some(DarkGray),
        11 => Some(LightRed),
        12 => Some(LightGreen),
        13 => Some(LightYellow),
        14 => Some(LightBlue),
        15 => Some(LightMagenta),
        16 => Some(LightCyan),
        17 => Some(White),
        18 => Some(Indexed(a)),
        19 => Some(Rgb(a, b, c)),
        _ => return Err(invalid()),
    })
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid retained text style")
}
