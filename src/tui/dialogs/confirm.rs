//! Confirmation dialog

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::buttons::render_yes_no;
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
        // the checkbox is shown.
        let (width, height) = if self.dont_ask_again.is_some() {
            (56, 11)
        } else {
            (50, 8)
        };
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
            let (yes, no) =
                render_yes_no(frame, chunks[4], theme, self.selected, self.hover.current());
            self.yes_button_area = yes;
            self.no_button_area = no;
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([Constraint::Min(1), Constraint::Length(2)])
                .split(inner);

            self.render_message(frame, chunks[0], theme);
            let (yes, no) =
                render_yes_no(frame, chunks[1], theme, self.selected, self.hover.current());
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

    #[test]
    fn dont_ask_again_survives_into_submit() {
        let mut dialog = ConfirmDialog::new("Quit", "Quit?", "quit").offering_dont_ask_again();
        dialog.handle_key(key(KeyCode::Char(' ')));
        let result = dialog.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(result, DialogResult::Submit(())));
        assert!(dialog.dont_ask_again());
    }
}
