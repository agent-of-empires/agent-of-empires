//! Confirmation dialog

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::buttons::{render_buttons_with_focus, ButtonFocus};
use crate::tui::components::checkbox::{checkbox_line, CheckboxStyle};
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

const DONT_ASK_AGAIN: &str = "dont_ask_again";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Checkbox(usize),
    Yes,
    No,
}

struct Checkbox {
    key: &'static str,
    label: String,
    checked: bool,
}

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
    tone: Tone,
    /// Optional checkboxes the caller reads back by key on Submit.
    checkboxes: Vec<Checkbox>,
    /// Up/Down cycle through the checkboxes, then Yes, then No.
    focus: Focus,
    /// An extra confirm key beside `y` and Enter, so the hotkey that opened
    /// the dialog also accepts it. Unset elsewhere, so no stray keystroke can
    /// fire an unrelated destructive confirm.
    confirm_char: Option<char>,
    /// Button labels, `("Yes", "No")` unless the caller names its verbs.
    buttons: (String, String),
    yes_button_area: Rect,
    no_button_area: Rect,
    checkbox_areas: Vec<Rect>,
    /// The hovered target. Visual only; never changes the focus.
    hover: HoverState,
}

impl ConfirmDialog {
    pub fn new(title: &str, message: &str, action: &str) -> Self {
        Self {
            title: title.to_string(),
            message: message.to_string(),
            action: action.to_string(),
            tone: Tone::Destructive,
            checkboxes: Vec::new(),
            focus: Focus::No,
            confirm_char: None,
            buttons: ("Yes".to_string(), "No".to_string()),
            yes_button_area: Rect::default(),
            no_button_area: Rect::default(),
            checkbox_areas: Vec::new(),
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
    pub fn offering_dont_ask_again(self) -> Self {
        self.checkbox(DONT_ASK_AGAIN, "Don't warn me again")
    }

    /// Add a checkbox, unchecked to start, that `is_checked(key)` reads back.
    pub fn checkbox(mut self, key: &'static str, label: &str) -> Self {
        self.checkboxes.push(Checkbox {
            key,
            label: label.to_string(),
            checked: false,
        });
        self
    }

    pub fn is_checked(&self, key: &str) -> bool {
        self.checkboxes.iter().any(|c| c.key == key && c.checked)
    }

    pub fn dont_ask_again(&self) -> bool {
        self.is_checked(DONT_ASK_AGAIN)
    }

    /// Keys of the checked boxes, for a caller that reads them after the dialog closes.
    pub fn checked_keys(&self) -> Vec<&'static str> {
        self.checkboxes
            .iter()
            .filter(|c| c.checked)
            .map(|c| c.key)
            .collect()
    }

    /// Route a left-click: `Submit` on `[Yes]`, `Cancel` on `[No]`, a toggle
    /// on the "don't warn me again" checkbox, `None` anywhere else.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<()>> {
        let pos = ratatui::layout::Position::from((col, row));
        if let Some(i) = self.checkbox_areas.iter().position(|a| a.contains(pos)) {
            self.focus = Focus::Checkbox(i);
            self.checkboxes[i].checked = !self.checkboxes[i].checked;
            return Some(DialogResult::Continue);
        }
        if self.yes_button_area.contains(pos) {
            return Some(DialogResult::Submit(()));
        }
        if self.no_button_area.contains(pos) {
            return Some(DialogResult::Cancel);
        }
        None
    }

    /// Highlight the button under the cursor without changing the focus:
    /// a drift between reading the prompt and pressing Enter must not flip
    /// which action fires. True when the highlight changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let mut targets = vec![self.yes_button_area, self.no_button_area];
        targets.extend_from_slice(&self.checkbox_areas);
        self.hover.update(col, row, &targets)
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

    #[cfg(test)]
    pub(crate) fn checkbox_labels_for_test(&self) -> Vec<&str> {
        self.checkboxes.iter().map(|c| c.label.as_str()).collect()
    }

    /// Whether `c` is the opt-in confirm key, ignoring ASCII case.
    fn is_confirm_char(&self, c: char) -> bool {
        self.confirm_char
            .is_some_and(|k| k.eq_ignore_ascii_case(&c))
    }

    /// Focus order: checkboxes top to bottom, then Yes, then No.
    fn focus_order(&self) -> Vec<Focus> {
        (0..self.checkboxes.len())
            .map(Focus::Checkbox)
            .chain([Focus::Yes, Focus::No])
            .collect()
    }

    fn move_focus(&mut self, forward: bool) {
        let order = self.focus_order();
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = order.len();
        self.focus = order[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }];
    }

    fn toggle_focused_checkbox(&mut self) {
        if let Focus::Checkbox(i) = self.focus {
            self.checkboxes[i].checked = !self.checkboxes[i].checked;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => DialogResult::Cancel,
            KeyCode::Enter => match self.focus {
                Focus::Yes => DialogResult::Submit(()),
                Focus::No => DialogResult::Cancel,
                Focus::Checkbox(_) => {
                    self.toggle_focused_checkbox();
                    DialogResult::Continue
                }
            },
            KeyCode::Char('y') | KeyCode::Char('Y') => DialogResult::Submit(()),
            KeyCode::Char(c) if self.is_confirm_char(c) => DialogResult::Submit(()),
            KeyCode::Char(' ') => {
                self.toggle_focused_checkbox();
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.move_focus(false);
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.move_focus(true);
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.focus = Focus::Yes;
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.focus = Focus::No;
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // The height follows the wrapped message so a multi-sentence body is
        // never clipped, with a minimum that keeps routine confirms compact.
        let rows = self.checkboxes.len() as u16;
        // Widen for the longest checkbox row: "[x] " + label + " (space)" + border and margin.
        let widest_checkbox = self
            .checkboxes
            .iter()
            .map(|c| unicode_width::UnicodeWidthStr::width(c.label.as_str()) as u16 + 16)
            .max();
        let width: u16 = widest_checkbox.map_or(50, |w| w.max(56)).min(area.width);
        let text_width = width.saturating_sub(4).max(1);
        let message_rows = wrapped_line_count(&self.message, text_width as usize);
        // Border, margin and buttons, plus the checkbox rows and their spacers.
        let chrome: u16 = if rows > 0 { 8 + rows } else { 6 };
        let min_height: u16 = if rows > 0 { 10 + rows } else { 8 };
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

        let constraints: &[Constraint] = if rows > 0 {
            &[
                Constraint::Min(1),       // message
                Constraint::Length(1),    // spacer
                Constraint::Length(rows), // checkboxes
                Constraint::Length(1),    // spacer
                Constraint::Length(2),    // buttons
            ]
        } else {
            &[Constraint::Min(1), Constraint::Length(2)]
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        self.render_message(frame, chunks[0], theme);
        self.checkbox_areas.clear();
        if rows > 0 {
            for (i, c) in self.checkboxes.iter().enumerate() {
                let focused = self.focus == Focus::Checkbox(i);
                let line = checkbox_line(
                    theme,
                    &c.label,
                    focused.then_some("space"),
                    0,
                    c.checked,
                    focused,
                    CheckboxStyle::confirm(theme),
                );
                let row = Rect {
                    y: chunks[2].y + i as u16,
                    height: 1,
                    width: (line.width() as u16).min(chunks[2].width),
                    ..chunks[2]
                };
                frame.render_widget(Paragraph::new(line), row);
                self.checkbox_areas.push(row);
            }
            if let Some(rect) = self.hover.current_in(&self.checkbox_areas) {
                paint_hover_bg(frame, rect, theme.selection);
            }
        }
        let button_focus = match self.focus {
            Focus::Yes => ButtonFocus::Yes,
            Focus::No => ButtonFocus::No,
            Focus::Checkbox(_) => ButtonFocus::Neither,
        };
        let (yes, no) = render_buttons_with_focus(
            frame,
            chunks[chunks.len() - 1],
            theme,
            (&self.buttons.0, &self.buttons.1),
            button_focus,
            self.hover.current(),
        );
        self.yes_button_area = yes;
        self.no_button_area = no;
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
        assert_eq!(dialog().focus, Focus::No, "No is the default");

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
        d.focus = Focus::Yes;
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(())
        ));

        // Left / h select Yes, Right / l select No.
        for (code, want) in [
            (KeyCode::Left, Focus::Yes),
            (KeyCode::Right, Focus::No),
            (KeyCode::Char('h'), Focus::Yes),
            (KeyCode::Char('l'), Focus::No),
        ] {
            let mut d = dialog();
            d.focus = if want == Focus::Yes {
                Focus::No
            } else {
                Focus::Yes
            };
            d.handle_key(key(code));
            assert_eq!(d.focus, want, "{code:?}");
        }
        // Up/Down, j/k and Tab cycle checkboxes, then Yes, then No, wrapping.
        let mut d = dialog().checkbox("a", "A").checkbox("b", "B");
        for (code, want) in [
            (KeyCode::Down, Focus::Checkbox(0)),
            (KeyCode::Char('j'), Focus::Checkbox(1)),
            (KeyCode::Tab, Focus::Yes),
            (KeyCode::Up, Focus::Checkbox(1)),
            (KeyCode::Char('k'), Focus::Checkbox(0)),
            (KeyCode::BackTab, Focus::No),
        ] {
            d.handle_key(key(code));
            assert_eq!(d.focus, want, "{code:?}");
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
    fn space_and_enter_toggle_only_the_focused_checkbox() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Char(' ')));
        assert!(!d.dont_ask_again(), "space is inert without the checkbox");

        // Space is inert until the checkbox is focused.
        let mut d = ConfirmDialog::new("Quit", "Quit?", "quit").offering_dont_ask_again();
        d.handle_key(key(KeyCode::Char(' ')));
        assert!(!d.dont_ask_again(), "space on No must not toggle");
        d.handle_key(key(KeyCode::Down));
        for want in [true, false, true] {
            d.handle_key(key(KeyCode::Char(' ')));
            assert_eq!(d.dont_ask_again(), want);
        }
        // Enter on a checkbox toggles rather than submitting.
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert!(!d.dont_ask_again());
        d.handle_key(key(KeyCode::Enter));

        // And it survives into the submit the caller reads it on.
        assert!(matches!(
            d.handle_key(key(KeyCode::Char('y'))),
            DialogResult::Submit(())
        ));
        assert!(d.dont_ask_again());

        // With several, Space toggles only the focused one.
        let mut d = dialog().checkbox("a", "A").checkbox("b", "B");
        d.handle_key(key(KeyCode::Up));
        d.handle_key(key(KeyCode::Up));
        d.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(d.checked_keys(), vec!["b"]);
    }

    #[test]
    fn only_the_focused_button_renders_focused() {
        // (focus, Yes emphasized, No emphasized)
        for (focus, yes_lit, no_lit) in [
            (Focus::Checkbox(0), false, false),
            (Focus::Yes, true, false),
            (Focus::No, false, true),
        ] {
            let mut d = dialog().checkbox("a", "A");
            d.focus = focus;
            let (_screen, buf, theme) = render_to(&mut d, 70, 14);
            let fg = |rect: Rect| buf[(rect.x, rect.y)].fg;
            assert_eq!(fg(d.yes_button_area) == theme.error, yes_lit, "{focus:?}");
            assert_eq!(fg(d.no_button_area) == theme.running, no_lit, "{focus:?}");
        }
    }

    #[test]
    fn a_long_checkbox_label_widens_the_dialog_instead_of_clipping() {
        // Wide characters take two cells each, so sizing must use display width.
        for label in [
            "Remove 1 from aoe that failed a forced delete",
            "削除削除削除削除削除削除削除削除削除削除削除削除",
        ] {
            let mut d = dialog().checkbox("drop", label);
            d.handle_key(key(KeyCode::Down));
            let (screen, _buf, _theme) = render_to(&mut d, 120, 20);
            assert!(
                screen.contains("(space)"),
                "label and hint must fit:\n{screen}"
            );
        }
    }

    #[test]
    fn clicking_the_rendered_checkbox_toggles_it_and_hover_lights_it() {
        let mut d = ConfirmDialog::new("Quit", "Quit?", "quit").offering_dont_ask_again();
        let (screen, _buf, _theme) = render_to(&mut d, 70, 14);
        let (y, line) = screen
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("Don't warn"))
            .expect("checkbox row");
        let x = line[..line.find("[ ]").unwrap()].chars().count() as u16;
        let y = y as u16;

        assert!(d.handle_hover(x, y));
        assert_eq!(d.hover.current(), Some(d.checkbox_areas[0]));
        for want in [true, false] {
            assert!(matches!(d.handle_click(x, y), Some(DialogResult::Continue)));
            assert_eq!(d.dont_ask_again(), want);
        }

        // Without the checkbox its row is inert.
        let mut plain = dialog();
        render_to(&mut plain, 70, 14);
        assert!(plain.handle_click(x, y).is_none());
        assert!(!plain.dont_ask_again());
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
