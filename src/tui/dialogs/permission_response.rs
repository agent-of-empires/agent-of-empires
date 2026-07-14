//! Dialog for answering a session's own interactive permission prompt (the
//! CLI's "Do you want to proceed?" style prompt) by sending the exact
//! keystrokes a human would type, without attaching to the session.
//!
//! Always offers the same three choices regardless of what's actually on
//! screen: AoE never parses pane content to detect or validate a pending
//! prompt (see `AgentDef.permission_response`); the user has already seen
//! the prompt before pressing the shortcut that opens this dialog.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::styles::{has_min_contrast, Theme};

/// WCAG AA-large threshold (matches `remote_home::render::selected_row_style`).
const FOCUSED_CHOICE_CONTRAST_RATIO: f32 = 3.0;

/// Style for the focused choice, backed by `theme.selection` instead of a
/// terminal-level `.reversed()` swap. Falls back to `theme.text` when
/// `theme.accent` wouldn't be legible against the selection background.
fn focused_choice_style(theme: &Theme) -> Style {
    let fg = if has_min_contrast(theme.accent, theme.selection, FOCUSED_CHOICE_CONTRAST_RATIO) {
        theme.accent
    } else {
        theme.text
    };
    Style::default().fg(fg).bg(theme.selection).bold()
}

/// Which of the three static choices the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionResponseChoice {
    Allow,
    AllowAlways,
    Deny,
}

pub struct PermissionResponseDialog {
    session_title: String,
    /// Index of the focused choice: 0=Allow, 1=Allow Always, 2=Deny.
    focused: usize,
}

const CHOICES: [(&str, PermissionResponseChoice); 3] = [
    ("Allow", PermissionResponseChoice::Allow),
    ("Allow Always", PermissionResponseChoice::AllowAlways),
    ("Deny", PermissionResponseChoice::Deny),
];

impl PermissionResponseDialog {
    pub fn new(session_title: &str) -> Self {
        Self {
            session_title: session_title.to_string(),
            focused: 0,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<PermissionResponseChoice> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => DialogResult::Submit(CHOICES[self.focused].1),
            KeyCode::Left | KeyCode::Up => {
                self.focused = (self.focused + CHOICES.len() - 1) % CHOICES.len();
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                self.focused = (self.focused + 1) % CHOICES.len();
                DialogResult::Continue
            }
            // Mirrors structured_view's a/A/d mnemonics (src/tui/structured_view/input.rs)
            // for the same three decisions, so the shortcut is consistent whether the
            // user is inside the structured view or answering from the sidebar.
            KeyCode::Char('a') => DialogResult::Submit(PermissionResponseChoice::Allow),
            KeyCode::Char('A') => DialogResult::Submit(PermissionResponseChoice::AllowAlways),
            KeyCode::Char('d') | KeyCode::Char('D') => {
                DialogResult::Submit(PermissionResponseChoice::Deny)
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_area = super::centered_rect(area, 56, 9);
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Respond to Permission Prompt ")
            .title_style(Style::default().fg(theme.accent).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);

        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                self.session_title.clone(),
                Style::default().fg(theme.title).bold(),
            )),
            Line::from(Span::styled(
                "AoE sends these as raw keystrokes; make sure the prompt is on screen.",
                Style::default().fg(theme.dimmed),
            )),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(header, chunks[0]);

        let mut spans = Vec::new();
        for (i, (label, _)) in CHOICES.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("   "));
            }
            let style = if i == self.focused {
                focused_choice_style(theme)
            } else {
                Style::default().fg(theme.text)
            };
            spans.push(Span::styled(format!("[{}]", label), style));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
            chunks[1],
        );

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "a=allow  A=always  d=deny  Esc=cancel",
                Style::default().fg(theme.dimmed),
            )))
            .alignment(Alignment::Center),
            chunks[2],
        );
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
    fn enter_submits_focused_default_allow() {
        let mut dialog = PermissionResponseDialog::new("test");
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::Allow)
        ));
    }

    #[test]
    fn a_submits_allow_directly() {
        let mut dialog = PermissionResponseDialog::new("test");
        let result = dialog.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::Allow)
        ));
    }

    #[test]
    fn shift_a_submits_allow_always_directly() {
        let mut dialog = PermissionResponseDialog::new("test");
        let result = dialog.handle_key(key(KeyCode::Char('A')));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::AllowAlways)
        ));
    }

    #[test]
    fn d_submits_deny_directly() {
        let mut dialog = PermissionResponseDialog::new("test");
        let result = dialog.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::Deny)
        ));
    }

    #[test]
    fn right_cycles_focus_and_enter_submits_it() {
        let mut dialog = PermissionResponseDialog::new("test");
        dialog.handle_key(key(KeyCode::Right));
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::AllowAlways)
        ));
    }

    #[test]
    fn left_wraps_focus_backward() {
        let mut dialog = PermissionResponseDialog::new("test");
        dialog.handle_key(key(KeyCode::Left));
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::Deny)
        ));
    }

    #[test]
    fn esc_cancels() {
        let mut dialog = PermissionResponseDialog::new("test");
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn focused_choice_style_uses_themed_background_not_reversed() {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);

        let style = focused_choice_style(&theme);

        assert_eq!(style.bg, Some(theme.selection));
        assert!(!style
            .add_modifier
            .contains(ratatui::style::Modifier::REVERSED));
    }

    #[test]
    fn focused_choice_style_keeps_accent_when_contrast_is_sufficient() {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);

        let style = focused_choice_style(&theme);

        assert_eq!(style.fg, Some(theme.accent));
    }

    #[test]
    fn focused_choice_style_falls_back_to_text_for_low_contrast_accent() {
        let mut theme = crate::tui::styles::load_theme_with_mode("empire", false);
        theme.accent = theme.selection;

        let style = focused_choice_style(&theme);

        assert_eq!(style.fg, Some(theme.text));
    }
}
