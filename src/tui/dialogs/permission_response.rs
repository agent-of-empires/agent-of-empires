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
use crate::tui::styles::Theme;

/// Style for the focused choice: `theme.accent` bold on the dialog's own
/// background, the same fg-only treatment `components::buttons::render_yes_no`
/// gives its focused button. `theme.selection` is a background surface token
/// (see DESIGN.md); as a foreground it sits within ~1.7:1 of every builtin
/// theme's background, so the focused choice would read as the dimmest item
/// on the row. Unfocused choices drop to `theme.dimmed` so focus is carried
/// by the brightness gap, not by a background block.
fn focused_choice_style(theme: &Theme) -> Style {
    Style::default().fg(theme.accent).bold()
}

/// Style for the choices that are not focused.
fn unfocused_choice_style(theme: &Theme) -> Style {
    Style::default().fg(theme.dimmed)
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
                unfocused_choice_style(theme)
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

    /// The focused choice paints a foreground on the dialog's own background,
    /// so its color has to be a foreground token. Every builtin's `accent`
    /// clears 2.5:1 against that theme's background (catppuccin-latte is the
    /// tightest at 2.64); a background surface token such as `theme.selection`
    /// lands between 1.10:1 and 1.71:1 and would fail here.
    #[test]
    fn focused_choice_fg_stays_legible_on_every_builtin_background() {
        const MIN_FOCUSED_CONTRAST_RATIO: f32 = 2.5;

        for name in crate::tui::styles::builtin_theme_names() {
            let theme = crate::tui::styles::load_theme(name);
            let style = focused_choice_style(&theme);

            let fg = style.fg.expect("focused choice must set a foreground");
            assert!(
                crate::tui::styles::has_min_contrast(
                    fg,
                    theme.background,
                    MIN_FOCUSED_CONTRAST_RATIO
                ),
                "{name}: focused choice fg is illegible on the theme background"
            );
            assert_eq!(style.bg, None, "{name}: focused choice must not set a bg");
            assert!(
                style.add_modifier.contains(ratatui::style::Modifier::BOLD),
                "{name}: focused choice must be bold"
            );
            assert_ne!(
                unfocused_choice_style(&theme).fg,
                style.fg,
                "{name}: focused and unfocused choices must differ"
            );
        }
    }
}
