//! Confirmation dialog

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::buttons::render_buttons;
use crate::tui::components::checkbox::{checkbox_line, CheckboxStyle};
use crate::tui::components::hover::HoverState;
use crate::tui::styles::Theme;

/// The dialog's emphasis color: destructive prompts alarm in red, routine
/// ones use a calmer amber so they do not read as data loss.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Destructive,
    Neutral,
}

pub struct ConfirmDialog {
    title: String,
    message: String,
    action: String,
    selected: bool, // true = Yes, false = No
    tone: Tone,
    /// When set, a "don't warn me again" checkbox the caller reads back with
    /// `dont_ask_again()` on Submit.
    dont_ask_again: Option<bool>,
    /// An extra confirm key beside `y` and Enter, so the hotkey that opened
    /// the dialog also accepts it. Unset elsewhere, so no stray keystroke can
    /// fire an unrelated destructive confirm.
    confirm_char: Option<char>,
    /// Button labels, `("Yes", "No")` unless the caller names its verbs.
    buttons: (String, String),
    yes_button_area: Rect,
    no_button_area: Rect,
    /// The hovered button. Visual only; never changes `selected`.
    hover: HoverState,
}

impl ConfirmDialog {
    pub fn new(title: &str, message: &str, action: &str) -> Self {
        Self {
            title: title.to_string(),
            message: message.to_string(),
            action: action.to_string(),
            selected: false,
            tone: Tone::Destructive,
            dont_ask_again: None,
            confirm_char: None,
            buttons: ("Yes".to_string(), "No".to_string()),
            yes_button_area: Rect::default(),
            no_button_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    /// Use the calmer emphasis, for a confirm that is not about losing data.
    pub fn neutral(mut self) -> Self {
        self.tone = Tone::Neutral;
        self
    }

    /// Accept another key as confirm, so the hotkey that opened the dialog
    /// accepts it too. `Esc` / `n` still cancel.
    pub fn confirmed_by(mut self, c: char) -> Self {
        self.confirm_char = Some(c);
        self
    }

    /// Name what the buttons do, where "Yes" alone would not.
    pub fn buttons(mut self, yes: &str, no: &str) -> Self {
        self.buttons = (yes.to_string(), no.to_string());
        self
    }

    /// Offer a "don't warn me again" checkbox, unchecked to start.
    pub fn offering_dont_ask_again(mut self) -> Self {
        self.dont_ask_again = Some(false);
        self
    }

    pub fn dont_ask_again(&self) -> bool {
        self.dont_ask_again.unwrap_or(false)
    }

    /// Route a left-click: `Submit` on `[Yes]`, `Cancel` on `[No]`, `None`
    /// anywhere else inside the dialog.
    pub fn handle_click(&self, col: u16, row: u16) -> Option<DialogResult<()>> {
        let pos = ratatui::layout::Position::from((col, row));
        if self.yes_button_area.contains(pos) {
            return Some(DialogResult::Submit(()));
        }
        if self.no_button_area.contains(pos) {
            return Some(DialogResult::Cancel);
        }
        None
    }

    /// Highlight the button under the cursor without changing `selected`:
    /// a drift between reading the prompt and pressing Enter must not flip
    /// which action fires. True when the highlight changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.hover
            .update(col, row, &[self.yes_button_area, self.no_button_area])
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    /// The `[Yes]` button hit-rect, populated on `render`. Test-only so a
    /// click path can be exercised at the exact coordinates the dialog draws.
    #[cfg(test)]
    pub(crate) fn yes_button_area_for_test(&self) -> ratatui::layout::Rect {
        self.yes_button_area
    }

    /// Whether `c` is the opt-in confirm key, ignoring ASCII case.
    fn is_confirm_char(&self, c: char) -> bool {
        self.confirm_char
            .is_some_and(|k| k.eq_ignore_ascii_case(&c))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => DialogResult::Cancel,
            KeyCode::Enter => {
                if self.selected {
                    DialogResult::Submit(())
                } else {
                    DialogResult::Cancel
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => DialogResult::Submit(()),
            KeyCode::Char(c) if self.is_confirm_char(c) => DialogResult::Submit(()),
            KeyCode::Char(' ') if self.dont_ask_again.is_some() => {
                self.dont_ask_again = Some(!self.dont_ask_again.unwrap_or(false));
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.selected = true;
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.selected = false;
                DialogResult::Continue
            }
            KeyCode::Tab => {
                self.selected = !self.selected;
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // The height follows the wrapped message so a multi-sentence body is
        // never clipped, with a minimum that keeps routine confirms compact.
        let width: u16 = if self.dont_ask_again.is_some() {
            56
        } else {
            50
        };
        let text_width = width.saturating_sub(4).max(1);
        let message_rows = wrapped_line_count(&self.message, text_width as usize);
        // Border, margin and buttons, plus the checkbox row and its spacers.
        let chrome: u16 = if self.dont_ask_again.is_some() { 9 } else { 6 };
        let min_height: u16 = if self.dont_ask_again.is_some() { 11 } else { 8 };
        let height = (message_rows as u16)
            .saturating_add(chrome)
            .max(min_height)
            .min(area.height);
        let dialog_area = super::centered_rect(area, width, height);

        frame.render_widget(Clear, dialog_area);

        let emphasis = match self.tone {
            Tone::Destructive => theme.error,
            Tone::Neutral => theme.waiting,
        };
        let block = super::toned_dialog_block(format!(" {} ", self.title), emphasis, emphasis);

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        if let Some(checked) = self.dont_ask_again {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([
                    Constraint::Min(1),    // message
                    Constraint::Length(1), // spacer
                    Constraint::Length(1), // checkbox
                    Constraint::Length(1), // spacer
                    Constraint::Length(2), // buttons
                ])
                .split(inner);

            self.render_message(frame, chunks[0], theme);
            let line = checkbox_line(
                theme,
                "Don't warn me again",
                Some("space"),
                0,
                checked,
                false,
                CheckboxStyle::confirm(theme),
            );
            frame.render_widget(Paragraph::new(line), chunks[2]);
            let (yes, no) = render_buttons(
                frame,
                chunks[4],
                theme,
                (&self.buttons.0, &self.buttons.1),
                self.selected,
                self.hover.current(),
            );
            self.yes_button_area = yes;
            self.no_button_area = no;
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([Constraint::Min(1), Constraint::Length(2)])
                .split(inner);

            self.render_message(frame, chunks[0], theme);
            let (yes, no) = render_buttons(
                frame,
                chunks[1],
                theme,
                (&self.buttons.0, &self.buttons.1),
                self.selected,
                self.hover.current(),
            );
            self.yes_button_area = yes;
            self.no_button_area = no;
        }
    }

    fn render_message(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let message = Paragraph::new(&*self.message)
            .style(Style::default().fg(theme.text))
            .wrap(Wrap { trim: true });
        frame.render_widget(message, area);
    }
}

/// Rows a message occupies when word-wrapped at `width` columns: greedy fill
/// on whitespace, long words broken mid-word. Close enough to ratatui's
/// `Wrap { trim: true }` to size the dialog, and an overestimate is harmless
/// where an underestimate would clip.
fn wrapped_line_count(message: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 0usize;
    for line in message.lines() {
        let mut used = 0usize;
        let mut line_rows = 1usize;
        for word in line.split_whitespace() {
            let mut len = word.chars().count();
            if used > 0 && used + 1 + len <= width {
                used += 1 + len;
                continue;
            }
            if used > 0 && len <= width {
                line_rows += 1;
                used = len;
                continue;
            }
            // First word on the row, or one longer than it.
            if used > 0 {
                line_rows += 1;
            }
            while len > width {
                line_rows += 1;
                len -= width;
            }
            used = len;
        }
        rows += line_rows;
    }
    rows.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::key;

    fn dialog() -> ConfirmDialog {
        ConfirmDialog::new("Test", "Message", "action")
    }

    /// Draw `dialog` and return the screen as one newline-joined string plus
    /// the buffer, for tests that need cell colors.
    fn render_to(
        dialog: &mut ConfirmDialog,
        width: u16,
        height: u16,
    ) -> (String, ratatui::buffer::Buffer, crate::tui::styles::Theme) {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = load_theme("empire");
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| dialog.render(f, f.area(), &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let screen: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        (screen, buf, theme)
    }

    #[test]
    fn keys_decide_the_dialog_and_move_the_selection() {
        assert_eq!(dialog().action(), "action");
        assert!(!dialog().selected, "No is the default");

        for code in [
            KeyCode::Esc,
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Enter,
        ] {
            assert!(
                matches!(dialog().handle_key(key(code)), DialogResult::Cancel),
                "{code:?}"
            );
        }
        for code in [KeyCode::Char('y'), KeyCode::Char('Y')] {
            assert!(
                matches!(dialog().handle_key(key(code)), DialogResult::Submit(())),
                "{code:?}"
            );
        }
        assert!(matches!(
            dialog().handle_key(key(KeyCode::Char('x'))),
            DialogResult::Continue
        ));

        let mut d = dialog();
        d.selected = true;
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(())
        ));

        // Left / h select Yes, Right / l select No, Tab flips.
        for (code, want) in [
            (KeyCode::Left, true),
            (KeyCode::Right, false),
            (KeyCode::Char('h'), true),
            (KeyCode::Char('l'), false),
        ] {
            let mut d = dialog();
            d.selected = !want;
            d.handle_key(key(code));
            assert_eq!(d.selected, want, "{code:?}");
        }
        let mut d = dialog();
        for want in [true, false] {
            d.handle_key(key(KeyCode::Tab));
            assert_eq!(d.selected, want);
        }
    }

    #[test]
    fn confirmed_by_accepts_the_opening_hotkey() {
        // Opted in, either case confirms.
        let mut opted_in =
            ConfirmDialog::new("Confirm Delete", "Message", "trash_session").confirmed_by('d');
        for code in [KeyCode::Char('d'), KeyCode::Char('D')] {
            assert!(
                matches!(opted_in.handle_key(key(code)), DialogResult::Submit(())),
                "{code:?}"
            );
        }

        // A cancel key still wins over an opt-in confirm char.
        let mut cancel_wins =
            ConfirmDialog::new("Confirm Delete", "Message", "trash_session").confirmed_by('n');
        assert!(matches!(
            cancel_wins.handle_key(key(KeyCode::Char('n'))),
            DialogResult::Cancel
        ));

        // Without the opt-in, `d` is inert, so no stray keystroke fires an
        // unrelated confirm.
        assert!(matches!(
            ConfirmDialog::new("Quit", "Quit?", "quit").handle_key(key(KeyCode::Char('d'))),
            DialogResult::Continue
        ));
    }

    #[test]
    fn the_dont_ask_again_checkbox_toggles_only_when_offered() {
        let mut d = dialog();
        assert!(!d.dont_ask_again());
        assert!(matches!(
            d.handle_key(key(KeyCode::Char(' '))),
            DialogResult::Continue
        ));
        assert!(!d.dont_ask_again(), "space is inert without the checkbox");

        let mut d = ConfirmDialog::new("Quit", "Quit?", "quit").offering_dont_ask_again();
        for want in [true, false] {
            assert!(matches!(
                d.handle_key(key(KeyCode::Char(' '))),
                DialogResult::Continue
            ));
            assert_eq!(d.dont_ask_again(), want);
        }

        // And it survives into the submit the caller reads it on.
        d.handle_key(key(KeyCode::Char(' ')));
        assert!(matches!(
            d.handle_key(key(KeyCode::Char('y'))),
            DialogResult::Submit(())
        ));
        assert!(d.dont_ask_again());
    }

    #[test]
    fn a_neutral_dialog_reads_as_a_heads_up_not_a_warning() {
        let mut dialog = ConfirmDialog::new("Quit", "Quit aoe?", "quit")
            .neutral()
            .offering_dont_ask_again();
        let (_screen, buf, theme) = render_to(&mut dialog, 70, 14);

        let mut label_fg = None;
        let mut border_fg = None;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if border_fg.is_none() {
                if let Some(bx) = row.find('╭') {
                    border_fg = Some(buf[(bx as u16, y)].fg);
                }
            }
            if let Some(idx) = row.find("Don't warn") {
                label_fg = Some(buf[(idx as u16, y)].fg);
            }
        }

        assert_eq!(
            label_fg,
            Some(theme.text),
            "the checkbox label must read as normal text, not disabled"
        );
        assert_eq!(
            border_fg,
            Some(theme.waiting),
            "a neutral dialog must not borrow the destructive red"
        );
        assert_ne!(border_fg, Some(theme.error));
    }

    #[test]
    fn a_multi_sentence_body_grows_the_dialog_instead_of_clipping() {
        let body = "Switch this session to the structured view? The tmux pane \
                    and its scrollback are destroyed; the agent restarts under \
                    the aoe serve daemon (a local one is started if none is \
                    running) with a fresh conversation.";
        let mut dialog = ConfirmDialog::new("Switch to structured view", body, "switch_view");
        let (screen, _buf, _theme) = render_to(&mut dialog, 80, 24);

        assert!(screen.contains("fresh"), "message tail clipped:\n{screen}");
        assert!(screen.contains("Yes") && screen.contains("No"), "{screen}");
        let msg_row = screen
            .lines()
            .position(|l| l.contains("fresh"))
            .expect("message tail row");
        let yes_row = screen
            .lines()
            .position(|l| l.contains("Yes"))
            .expect("yes button row");
        assert!(
            yes_row > msg_row,
            "buttons must sit below the last message line, not over it"
        );
    }

    #[test]
    fn wrapped_line_count_matches_the_renderer_closely_enough_to_size_the_dialog() {
        // (message, width, rows)
        let cases: &[(&str, usize, usize)] = &[
            ("", 46, 1),
            ("short", 46, 1),
            ("a\nb", 46, 2),
            ("aaaaaaaaaa bbbbbbbbbb", 10, 2),
        ];
        for (message, width, rows) in cases {
            assert_eq!(wrapped_line_count(message, *width), *rows, "{message:?}");
        }
        // A single word longer than the row breaks across rows.
        assert_eq!(wrapped_line_count(&"x".repeat(25), 10), 3);
    }
}
