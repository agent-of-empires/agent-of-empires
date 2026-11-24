//! Whether an agent's input box holds text a person typed, read from a coloured capture.
//!
//! Claude dims its suggestion text with SGR 2 (faint) and draws typed text in the default
//! foreground, so the style of each cell, not the text, separates a ghost from a draft. A
//! printing cell in an explicit colour matches neither and fails open as `Unknown`.

use regex::Regex;

/// What one capture shows on the agent's input line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLine {
    /// Only whitespace and faint suggestion text.
    Empty,
    /// Text a person typed and has not submitted.
    Draft,
    /// No input box was found, or its text cannot be told apart: callers type as before.
    Unknown,
}

impl InputLine {
    pub const BUSY_MESSAGE: &'static str =
        "The agent's input box holds text someone typed and has not sent, so nothing was typed. \
         Send or clear it in the terminal, then retry.";
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cell {
    Blank,
    Faint,
    Typed,
    Coloured,
}

#[derive(Default)]
struct Style {
    faint: bool,
    coloured: bool,
}

impl Style {
    fn apply(&mut self, params: &str) {
        let mut tokens = params.split(';');
        while let Some(token) = tokens.next() {
            let code = token.split(':').next().unwrap_or("");
            match code.parse::<u16>().unwrap_or(0) {
                0 => *self = Self::default(),
                2 => self.faint = true,
                22 => self.faint = false,
                39 => self.coloured = false,
                30..=37 | 90..=97 => self.coloured = true,
                extended @ (38 | 48 | 58) => {
                    if extended == 38 {
                        self.coloured = true;
                    }
                    // `38;5;n` and `38;2;r;g;b` spread the colour over the following tokens.
                    if !token.contains(':') {
                        let skip = match tokens.next() {
                            Some("5") => 1,
                            Some("2") => 3,
                            _ => 0,
                        };
                        tokens.by_ref().take(skip).for_each(drop);
                    }
                }
                _ => {}
            }
        }
    }

    fn cell(&self, ch: char) -> Cell {
        if ch.is_whitespace() {
            Cell::Blank
        } else if self.faint {
            Cell::Faint
        } else if self.coloured {
            Cell::Coloured
        } else {
            Cell::Typed
        }
    }
}

/// Each line's plain text and its cells' classes. SGR state carries across lines, as tmux
/// emits a style once and leaves it on until the next change.
fn styled_lines(capture: &str) -> Vec<(String, Vec<Cell>)> {
    let mut lines = vec![(String::new(), Vec::new())];
    let mut style = Style::default();
    let mut chars = capture.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            if c == 'm' {
                                style.apply(&params);
                            }
                            break;
                        }
                        params.push(c);
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' => lines.push((String::new(), Vec::new())),
            '\r' => {}
            ch => {
                let line = lines.last_mut().expect("starts with one line");
                line.0.push(ch);
                line.1.push(style.cell(ch));
            }
        }
    }
    lines
}

/// The box is the last two rules starting at column 0, with `prompt` on the row below the top
/// one. Every row a person types is indented, so typed text cannot pass for either; any other
/// layout is `Unknown`.
pub(super) fn classify(prompt: &Regex, capture: &str) -> InputLine {
    let lines = styled_lines(capture);
    let mut rules = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, (text, _))| super::region::line_is_horizontal_rule(text.trim_end()))
        .map(|(row, _)| row);
    let (Some(bottom), Some(top)) = (rules.next(), rules.next()) else {
        return InputLine::Unknown;
    };
    let (text, cells) = &lines[top + 1];
    let Some(glyph) = prompt.find(text) else {
        return InputLine::Unknown;
    };
    let glyph = text[..glyph.end()].chars().count();
    let cells = cells[glyph..]
        .iter()
        .chain(lines[top + 2..bottom].iter().flat_map(|(_, c)| c));
    let mut coloured = false;
    for cell in cells {
        match cell {
            Cell::Typed => return InputLine::Draft,
            Cell::Coloured => coloured = true,
            Cell::Blank | Cell::Faint => {}
        }
    }
    if coloured {
        InputLine::Unknown
    } else {
        InputLine::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/input-line");

    #[test]
    fn claude_input_line_reads_typed_text_and_skips_its_ghost() {
        let claude = |capture: &str| crate::tmux::detect::input_line("claude", capture);
        for (fixture, want) in [
            ("empty-0s", InputLine::Empty),
            ("empty-repo-6", InputLine::Empty),
            ("empty-after-clear", InputLine::Empty),
            ("light-empty", InputLine::Empty),
            ("draft-single", InputLine::Draft),
            ("light-draft-single", InputLine::Draft),
            ("draft-cursor-mid", InputLine::Draft),
            ("draft-multi", InputLine::Draft),
            ("draft-blank-first", InputLine::Draft),
            // A scrollback `❯` line above the box has bright text.
            ("ghost-postturn", InputLine::Empty),
            ("ghost-placeholder", InputLine::Empty),
            ("ghost-postturn-one-char", InputLine::Draft),
            ("ghost-placeholder-one-char", InputLine::Draft),
            ("draft-cursor-start-2131", InputLine::Draft),
            ("draft-cursor-start", InputLine::Draft),
            ("draft-ends-bare-prompt", InputLine::Draft),
            ("draft-rule-line", InputLine::Draft),
            ("draft-wrapped", InputLine::Draft),
            ("ghost-after-ctrl-c", InputLine::Empty),
        ] {
            let capture = std::fs::read_to_string(format!("{FIXTURES}/claude-{fixture}.ans"))
                .unwrap_or_else(|e| panic!("{fixture}: {e}"));
            assert_eq!(claude(&capture), want, "{fixture}");
        }
        const RULE: &str = "────────";
        for (capture, want) in [
            (
                format!("{RULE}\n\x1b[39m❯\u{a0}\x1b[38;5;244mgrey text\x1b[0m\n{RULE}"),
                InputLine::Unknown,
            ),
            (
                format!("{RULE}\n❯\u{a0}\x1b[38;2;1;2;3mx\x1b[39my\n{RULE}"),
                InputLine::Draft,
            ),
            (
                format!("{RULE}\n❯\u{a0}\x1b[2mwrapped\nghost\x1b[0m\n{RULE}"),
                InputLine::Empty,
            ),
            (
                format!("{RULE}\n❯\u{a0}keep this\n  ❯\u{a0}\n{RULE}"),
                InputLine::Draft,
            ),
            // Not the layout the rules expect: fail open.
            (
                format!("{RULE}\n❯ typed, no bottom rule"),
                InputLine::Unknown,
            ),
            (format!("  {RULE}\n❯ typed\n  {RULE}"), InputLine::Unknown),
            (format!("{RULE}\n  ❯ typed\n{RULE}"), InputLine::Unknown),
            (format!("{RULE}\n\n❯ typed\n{RULE}"), InputLine::Unknown),
            (format!("{RULE}\n{RULE}"), InputLine::Unknown),
            ("no box".to_string(), InputLine::Unknown),
        ] {
            assert_eq!(claude(&capture), want, "{capture:?}");
        }
        assert_eq!(
            crate::tmux::detect::input_line("codex", &format!("{RULE}\n❯ typed\n{RULE}")),
            InputLine::Unknown,
            "an agent without an input_line table"
        );
    }
}
