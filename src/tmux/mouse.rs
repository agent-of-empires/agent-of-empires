//! Terminal mouse reports for forwarding pointer input into a pane, shared by
//! the TUI preview and the daemon's live socket so both speak the same bytes.

use super::PaneCursor;

/// One mouse report at 1-based pane cell `(cx, cy)`. `cb` is the button code
/// with any motion bit (32) already added; `sgr` picks SGR (1006) over the
/// legacy X10 encoding.
pub fn mouse_report_bytes(cb: u16, release: bool, sgr: bool, cx: u16, cy: u16) -> Vec<u8> {
    if sgr {
        // SGR keeps the button on release and ends it with `m`.
        let end = if release { 'm' } else { 'M' };
        format!("\x1b[<{cb};{cx};{cy}{end}").into_bytes()
    } else {
        // X10 packs each value + 32 into one byte, so values clamp at 223, and
        // a release cannot name its button.
        let enc = |v: u16| (v.min(223) + 32) as u8;
        let btn = if release { 3 } else { cb };
        vec![0x1b, b'[', b'M', enc(btn), enc(cx), enc(cy)]
    }
}

/// What one wheel notch at 1-based pane cell `(cx, cy)` sends to the app in
/// `cursor`'s pane, or `None` on the normal screen, whose real scrollback the
/// viewer scrolls itself. A mouse-tracking app gets a wheel report; any other
/// full-screen app gets `PageUp`/`PageDown`, since arrow keys move its cursor
/// or input history instead of scrolling (#2407).
pub fn wheel_notch_bytes(cursor: &PaneCursor, up: bool, cx: u16, cy: u16) -> Option<Vec<u8>> {
    if !cursor.alternate_on {
        return None;
    }
    Some(if cursor.mouse_tracking {
        mouse_report_bytes(if up { 64 } else { 65 }, false, cursor.mouse_sgr, cx, cy)
    } else if up {
        b"\x1b[5~".to_vec()
    } else {
        b"\x1b[6~".to_vec()
    })
}

/// Most notches one coalesced wheel message may carry. A burst beyond this is
/// a fling no viewer is tracking, and the cap is what stops a malformed client
/// from making the daemon inject input by the megabyte.
pub const MAX_WHEEL_NOTCHES: u16 = 128;

/// `count` notches at 1-based pane cell `(cx, cy)`, clamped to
/// [`MAX_WHEEL_NOTCHES`]; see [`wheel_notch_bytes`] for what one notch sends.
pub fn wheel_bytes(cursor: &PaneCursor, up: bool, cx: u16, cy: u16, count: u16) -> Option<Vec<u8>> {
    let count = count.min(MAX_WHEEL_NOTCHES);
    if count == 0 {
        return None;
    }
    Some(wheel_notch_bytes(cursor, up, cx, cy)?.repeat(count as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(alternate_on: bool, mouse_tracking: bool, mouse_sgr: bool) -> PaneCursor {
        PaneCursor {
            x: 0,
            y: 0,
            visible: true,
            pane_height: 24,
            history_size: 0,
            pane_width: 80,
            alternate_on,
            mouse_tracking,
            mouse_sgr,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        }
    }

    #[test]
    fn a_wheel_notch_follows_the_panes_screen_and_mouse_modes() {
        let cases: [(PaneCursor, bool, Option<&[u8]>); 5] = [
            (cursor(false, true, true), true, None),
            (cursor(true, false, false), true, Some(b"\x1b[5~")),
            (cursor(true, false, false), false, Some(b"\x1b[6~")),
            (cursor(true, true, true), true, Some(b"\x1b[<64;3;4M")),
            (
                cursor(true, true, false),
                false,
                Some(&[0x1b, b'[', b'M', 65 + 32, 3 + 32, 4 + 32]),
            ),
        ];
        for (cursor, up, want) in cases {
            assert_eq!(
                wheel_notch_bytes(&cursor, up, 3, 4).as_deref(),
                want,
                "{cursor:?} up={up}"
            );
        }
        // X10 packs coordinates into single bytes, clamped at 223.
        assert_eq!(
            mouse_report_bytes(64, false, false, 300, 300),
            [0x1b, b'[', b'M', 64 + 32, 223 + 32, 223 + 32]
        );
    }

    #[test]
    fn a_coalesced_wheel_repeats_one_notch_up_to_the_cap() {
        let pane = cursor(true, true, true);
        assert_eq!(
            wheel_bytes(&pane, true, 3, 4, 3).as_deref(),
            Some(&b"\x1b[<64;3;4M\x1b[<64;3;4M\x1b[<64;3;4M"[..])
        );
        assert_eq!(wheel_bytes(&pane, true, 3, 4, 0), None, "nothing to send");
        assert_eq!(
            wheel_bytes(&pane, true, 3, 4, u16::MAX).map(|b| b.len()),
            Some(MAX_WHEEL_NOTCHES as usize * b"\x1b[<64;3;4M".len()),
            "a hostile count clamps"
        );
        assert_eq!(
            wheel_bytes(&cursor(false, true, true), true, 3, 4, 5),
            None,
            "the normal screen still scrolls itself"
        );
    }
}
