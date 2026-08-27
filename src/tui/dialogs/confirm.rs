//! Confirmation dialog

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::buttons::render_buttons;
use crate::tui::components::checkbox::{checkbox_line, CheckboxStyle};
use crate::tui::components::hover::HoverState;
use crate::tui::styles::Theme;

/// The dialog's emphasis color. Destructive confirmations (delete, stop,
/// cancel-a-running-hook) alarm in red; neutral ones (quitting, with
/// sessions left running) use the calmer "heads-up" amber so a routine
/// prompt doesn't read like a data-loss warning.
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
    /// When set, the dialog shows a "don't warn me again" checkbox the
    /// user can toggle with Space. The caller reads `dont_ask_again()`
    /// on Submit to persist the opt-out. `None` hides the checkbox.
    dont_ask_again: Option<bool>,
    /// Extra character that confirms, alongside `y` and Enter-on-Yes. Set
    /// for the delete confirm so the `d` that opened the dialog also
    /// accepts it; left unset everywhere else so a stray keystroke can't
    /// fire an unrelated destructive confirm.
    confirm_char: Option<char>,
    /// Button labels, `("Yes", "No")` unless the caller names its verbs.
    /// A confirm whose question can't be answered by "Yes" alone (the
    /// trash prompt) says what each button does instead.
    buttons: (String, String),
    yes_button_area: Rect,
    no_button_area: Rect,
    /// Which Yes/No button the mouse is over, for the hover highlight.
    /// Visual only; never changes `selected`.
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

    /// Render with the calmer "heads-up" emphasis instead of the default
    /// destructive red. For confirmations that aren't about losing data.
    pub fn neutral(mut self) -> Self {
        self.tone = Tone::Neutral;
        self
    }

    /// Also accept `c` (case-insensitively) as a confirm key, so a dialog
    /// opened by a hotkey can be accepted by pressing that hotkey again.
    /// Cancel keys still win: `Esc` / `n` cancel even when one of them is
    /// passed here.
    pub fn confirmed_by(mut self, c: char) -> Self {
        self.confirm_char = Some(c);
        self
    }

    /// Name what the buttons do instead of the default Yes/No, for a
    /// confirm where "Yes" alone doesn't say what is about to happen.
    pub fn buttons(mut self, yes: &str, no: &str) -> Self {
        self.buttons = (yes.to_string(), no.to_string());
        self
    }

    /// Offer a "don't warn me again" checkbox (unchecked to start). The
    /// caller inspects `dont_ask_again()` after a Submit to act on it.
    pub fn offering_dont_ask_again(mut self) -> Self {
        self.dont_ask_again = Some(false);
        self
    }

    /// Whether the user ticked "don't warn me again". Always false when
    /// the checkbox wasn't offered.
    pub fn dont_ask_again(&self) -> bool {
        self.dont_ask_again.unwrap_or(false)
    }

    /// Route a left-click. `Some(Submit)` for `[Yes]`, `Some(Cancel)`
    /// for `[No]`, `None` for clicks that hit elsewhere inside the
    /// dialog. Mirrors UnifiedDeleteDialog so the home view's
    /// `handle_dialog_click` can fan out the same way.
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

    /// Highlight the Yes/No button under the cursor. Hover does not
    /// change `selected`: otherwise the mouse drifting over the opposite
    /// button between the user reading the prompt and pressing Enter
    /// would silently flip which action fires. Click commits explicitly
    /// via `handle_click`. Returns `true` when the highlighted button
    /// changed so the caller can redraw.
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
        // Spacer rows separate message / checkbox / buttons so the dialog
        // breathes; grow the height (and a touch of width) to fit them when
        // the checkbox is shown. The height follows the wrapped message so
        // a multi-sentence body (e.g. the switch-view confirm) is never
        // clipped by a fixed row budget; short messages keep the historical
        // minimum so routine confirms don't shrink.
        let width: u16 = if self.dont_ask_again.is_some() {
            56
        } else {
            50
        };
        // Border (2) + horizontal layout margin (2) eat four columns.
        let text_width = width.saturating_sub(4).max(1);
        let message_rows = wrapped_line_count(&self.message, text_width as usize);
        // Border (2) + vertical margin (2) + buttons (2); the checkbox
        // variant adds spacer + checkbox + spacer.
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(emphasis))
            .title(format!(" {} ", self.title))
            .title_style(Style::default().fg(emphasis).bold());

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

/// Rows a message occupies when word-wrapped at `width` columns.
/// Greedy fill on whitespace with long words broken mid-word, matching
/// ratatui's `Wrap { trim: true }` closely enough to size the dialog
/// (a one-row overestimate just leaves a blank line; an underestimate
/// would clip, which is what this exists to prevent).
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
            // Either the first word on the row, or a word longer than
            // the row: consume full rows until the remainder fits.
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
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn test_default_selection_is_no() {
        let dialog = ConfirmDialog::new("Test", "Are you sure?", "test_action");
        assert!(!dialog.selected);
    }

    #[test]
    fn test_action_accessor() {
        let dialog = ConfirmDialog::new("Title", "Message", "delete");
        assert_eq!(dialog.action(), "delete");
    }

    #[test]
    fn test_esc_cancels() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_n_cancels() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        let result = dialog.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_uppercase_n_cancels() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        let result = dialog.handle_key(key(KeyCode::Char('N')));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_y_confirms() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        let result = dialog.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(result, DialogResult::Submit(())));
    }

    #[test]
    fn test_uppercase_y_confirms() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        let result = dialog.handle_key(key(KeyCode::Char('Y')));
        assert!(matches!(result, DialogResult::Submit(())));
    }

    #[test]
    fn test_enter_with_no_selected_cancels() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_with_yes_selected_submits() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        dialog.selected = true;
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(())));
    }

    #[test]
    fn test_tab_toggles_selection() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        assert!(!dialog.selected);

        dialog.handle_key(key(KeyCode::Tab));
        assert!(dialog.selected);

        dialog.handle_key(key(KeyCode::Tab));
        assert!(!dialog.selected);
    }

    #[test]
    fn test_left_selects_yes() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        dialog.handle_key(key(KeyCode::Left));
        assert!(dialog.selected);
    }

    #[test]
    fn test_right_selects_no() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        dialog.selected = true;
        dialog.handle_key(key(KeyCode::Right));
        assert!(!dialog.selected);
    }

    #[test]
    fn test_h_selects_yes() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        dialog.handle_key(key(KeyCode::Char('h')));
        assert!(dialog.selected);
    }

    #[test]
    fn test_l_selects_no() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        dialog.selected = true;
        dialog.handle_key(key(KeyCode::Char('l')));
        assert!(!dialog.selected);
    }

    /// `confirmed_by` opts a dialog into accepting the hotkey that opened
    /// it, in either case, and only when it was asked for.
    #[test]
    fn confirmed_by_accepts_the_opening_hotkey() {
        let mut opted_in =
            ConfirmDialog::new("Confirm Delete", "Message", "trash_session").confirmed_by('d');
        for code in [KeyCode::Char('d'), KeyCode::Char('D')] {
            assert!(
                matches!(opted_in.handle_key(key(code)), DialogResult::Submit(())),
                "{code:?} should confirm"
            );
        }
        // Cancel keys still win over an opt-in confirm char.
        let mut cancel_wins =
            ConfirmDialog::new("Confirm Delete", "Message", "trash_session").confirmed_by('n');
        assert!(matches!(
            cancel_wins.handle_key(key(KeyCode::Char('n'))),
            DialogResult::Cancel
        ));
        // Without the opt-in, `d` is inert, so an unrelated confirm can't be
        // fired by a stray keystroke.
        let mut default_dialog = ConfirmDialog::new("Quit", "Quit?", "quit");
        assert!(matches!(
            default_dialog.handle_key(key(KeyCode::Char('d'))),
            DialogResult::Continue
        ));
    }

    #[test]
    fn test_unknown_key_continues() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        let result = dialog.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(result, DialogResult::Continue));
    }

    #[test]
    fn dont_ask_again_defaults_false_when_not_offered() {
        let mut dialog = ConfirmDialog::new("Test", "Message", "action");
        assert!(!dialog.dont_ask_again());
        // Space is inert when the checkbox isn't offered.
        let result = dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(matches!(result, DialogResult::Continue));
        assert!(!dialog.dont_ask_again());
    }

    #[test]
    fn space_toggles_dont_ask_again_when_offered() {
        let mut dialog = ConfirmDialog::new("Quit", "Quit?", "quit").offering_dont_ask_again();
        assert!(!dialog.dont_ask_again());

        let result = dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(matches!(result, DialogResult::Continue));
        assert!(dialog.dont_ask_again());

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.dont_ask_again());
    }

    /// Render the quit dialog and return the foreground color of the cell
    /// under a given character of the "Don't warn me again" label, plus the
    /// top-border color. Guards the styling: the label must read as normal
    /// text (not the disabled-looking `dimmed`), and the border must use the
    /// neutral heads-up tone rather than destructive red.
    #[test]
    fn quit_dialog_label_is_readable_and_border_is_neutral() {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut dialog = ConfirmDialog::new("Quit", "Quit aoe?", "quit")
            .neutral()
            .offering_dont_ask_again();
        let theme = load_theme("empire");
        let backend = TestBackend::new(70, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dialog.render(f, f.area(), &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // Find the checkbox row and the column where the label "D" starts.
        let mut label_fg = None;
        let mut border_fg = None;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if border_fg.is_none() && row.contains('╭') {
                let bx = row.find('╭').unwrap() as u16;
                border_fg = Some(buf[(bx, y)].fg);
            }
            if let Some(idx) = row.find("Don't warn") {
                label_fg = Some(buf[(idx as u16, y)].fg);
            }
        }

        assert_eq!(
            label_fg,
            Some(theme.text),
            "checkbox label should use normal text color, not dimmed/disabled"
        );
        assert_ne!(
            label_fg,
            Some(theme.dimmed),
            "checkbox label must not be dimmed"
        );
        assert_eq!(
            border_fg,
            Some(theme.waiting),
            "neutral quit dialog should use the heads-up tone, not destructive red"
        );
        assert_ne!(border_fg, Some(theme.error));
    }

    /// A multi-sentence body (the switch-view confirm) must be fully
    /// visible: the old fixed 8-row height clipped it to two lines with
    /// the buttons painted over the rest (#2923 follow-up).
    #[test]
    fn long_message_is_not_clipped() {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let body = "Switch this session to the structured view? The tmux pane \
                    and its scrollback are destroyed; the agent restarts under \
                    the aoe serve daemon (a local one is started if none is \
                    running) with a fresh conversation.";
        let mut dialog = ConfirmDialog::new("Switch to structured view", body, "switch_view");
        let theme = load_theme("empire");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
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

        // The tail of the message survives wrapping, and the buttons
        // render below it rather than over it.
        assert!(
            screen.contains("fresh"),
            "message tail should be visible, not clipped:\n{screen}"
        );
        assert!(
            screen.contains("Yes") && screen.contains("No"),
            "buttons should still render:\n{screen}"
        );
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
            "buttons must be below the last message line (yes_row={yes_row}, msg_row={msg_row})"
        );
    }

    /// Short bodies keep the historical compact height so routine
    /// confirms don't change shape.
    #[test]
    fn wrapped_line_count_basics() {
        assert_eq!(wrapped_line_count("", 46), 1);
        assert_eq!(wrapped_line_count("short", 46), 1);
        assert_eq!(wrapped_line_count("a\nb", 46), 2);
        // 10-char words at width 10: one per row.
        assert_eq!(wrapped_line_count("aaaaaaaaaa bbbbbbbbbb", 10), 2);
        // A single word longer than the row breaks across rows.
        assert_eq!(wrapped_line_count(&"x".repeat(25), 10), 3);
    }

    #[test]
    fn dont_ask_again_survives_into_submit() {
        let mut dialog = ConfirmDialog::new("Quit", "Quit?", "quit").offering_dont_ask_again();
        dialog.handle_key(key(KeyCode::Char(' ')));
        let result = dialog.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(result, DialogResult::Submit(())));
        assert!(dialog.dont_ask_again());
    }
}
