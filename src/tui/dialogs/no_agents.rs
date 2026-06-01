//! Onboarding dialog shown when no AI agents are installed.
//!
//! Displays install instructions for popular agents and offers a re-check
//! button that re-runs detection without restarting AoE.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

/// Result of the no-agents dialog interaction.
pub enum NoAgentsAction {
    /// User chose to re-check for installed agents.
    Recheck,
    /// User chose to quit AoE.
    Quit,
}

pub struct NoAgentsDialog {
    /// true = "Re-check" focused, false = "Quit" focused
    recheck_focused: bool,
    recheck_button_area: Rect,
    quit_button_area: Rect,
    /// Which button the mouse is over, for the hover highlight. Visual
    /// only; never changes `recheck_focused`.
    hover: HoverState,
}

impl NoAgentsDialog {
    pub fn new() -> Self {
        Self {
            recheck_focused: true,
            recheck_button_area: Rect::default(),
            quit_button_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    pub fn handle_click(&self, col: u16, row: u16) -> Option<DialogResult<NoAgentsAction>> {
        let pos = ratatui::layout::Position::from((col, row));
        if self.recheck_button_area.contains(pos) {
            return Some(DialogResult::Submit(NoAgentsAction::Recheck));
        }
        if self.quit_button_area.contains(pos) {
            return Some(DialogResult::Submit(NoAgentsAction::Quit));
        }
        None
    }

    /// Highlight the button under the cursor without changing the
    /// Re-check / Quit focus. See `ConfirmDialog::handle_hover` for the
    /// rationale. Returns `true` when the highlighted button changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.hover
            .update(col, row, &[self.recheck_button_area, self.quit_button_area])
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<NoAgentsAction> {
        match key.code {
            KeyCode::Enter => {
                if self.recheck_focused {
                    DialogResult::Submit(NoAgentsAction::Recheck)
                } else {
                    DialogResult::Submit(NoAgentsAction::Quit)
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                DialogResult::Submit(NoAgentsAction::Recheck)
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                DialogResult::Submit(NoAgentsAction::Quit)
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                self.recheck_focused = !self.recheck_focused;
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_area = super::centered_rect(area, 70, 20);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Welcome to Agent of Empires ")
            .title_style(Style::default().fg(theme.accent).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(inner);

        let content = vec![
            Line::from(Span::styled(
                "No AI coding agents detected.",
                Style::default().fg(theme.title).bold(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Before you can create a session, install at least one",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "AI coding agent. AoE manages and orchestrates them.",
                Style::default().fg(theme.text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Recommended for first-timers:",
                Style::default().fg(theme.hint).italic(),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Claude Code  ", Style::default().fg(theme.title).bold()),
                Span::styled(
                    "npm install -g @anthropic-ai/claude-code",
                    Style::default().fg(theme.dimmed),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Codex CLI    ", Style::default().fg(theme.title).bold()),
                Span::styled(
                    "npm install -g @openai/codex",
                    Style::default().fg(theme.dimmed),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Gemini CLI   ", Style::default().fg(theme.title).bold()),
                Span::styled(
                    "npm install -g @google/gemini-cli",
                    Style::default().fg(theme.dimmed),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Run `aoe agents` for a full list with install commands.",
                Style::default().fg(theme.dimmed),
            )),
        ];

        let paragraph = Paragraph::new(content).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, chunks[0]);

        let recheck_style = if self.recheck_focused {
            Style::default().fg(theme.accent).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };
        let quit_style = if !self.recheck_focused {
            Style::default().fg(theme.accent).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };

        let buttons = Line::from(vec![
            Span::styled("[Re-check]", recheck_style),
            Span::raw("    "),
            Span::styled("[Quit]", quit_style),
        ]);

        let button_area = chunks[1];
        // Compute centered button positions deterministically so the
        // hit rects line up with the rendered glyphs.
        let recheck_label = "[Re-check]";
        let quit_label = "[Quit]";
        let gap: u16 = 4;
        let total_width =
            recheck_label.chars().count() as u16 + gap + quit_label.chars().count() as u16;
        if button_area.width >= total_width {
            let left_pad = (button_area.width - total_width) / 2;
            let recheck_x = button_area.x + left_pad;
            let recheck_w = recheck_label.chars().count() as u16;
            let quit_x = recheck_x + recheck_w + gap;
            let quit_w = quit_label.chars().count() as u16;
            self.recheck_button_area = Rect::new(recheck_x, button_area.y, recheck_w, 1);
            self.quit_button_area = Rect::new(quit_x, button_area.y, quit_w, 1);
        } else {
            self.recheck_button_area = Rect::default();
            self.quit_button_area = Rect::default();
        }

        frame.render_widget(
            Paragraph::new(vec![
                buttons,
                Line::from(Span::styled(
                    "(install an agent, then press r)",
                    Style::default().fg(theme.dimmed),
                )),
            ])
            .alignment(Alignment::Center),
            button_area,
        );

        if let Some(rect) = self
            .hover
            .current_in(&[self.recheck_button_area, self.quit_button_area])
        {
            paint_hover_bg(frame, rect, theme.selection);
        }
    }
}

impl Default for NoAgentsDialog {
    fn default() -> Self {
        Self::new()
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
    fn test_recheck_on_enter() {
        let mut dialog = NoAgentsDialog::new();
        // Default focus is on Re-check
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(NoAgentsAction::Recheck)
        ));
    }

    #[test]
    fn test_quit_on_esc() {
        let mut dialog = NoAgentsDialog::new();
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Submit(NoAgentsAction::Quit)));
    }

    #[test]
    fn test_quit_on_q() {
        let mut dialog = NoAgentsDialog::new();
        let result = dialog.handle_key(key(KeyCode::Char('q')));
        assert!(matches!(result, DialogResult::Submit(NoAgentsAction::Quit)));
    }

    #[test]
    fn test_recheck_on_r() {
        let mut dialog = NoAgentsDialog::new();
        let result = dialog.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(
            result,
            DialogResult::Submit(NoAgentsAction::Recheck)
        ));
    }

    #[test]
    fn test_tab_toggles_focus() {
        let mut dialog = NoAgentsDialog::new();
        assert!(dialog.recheck_focused);
        dialog.handle_key(key(KeyCode::Tab));
        assert!(!dialog.recheck_focused);
        // Enter now submits Quit
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(NoAgentsAction::Quit)));
    }

    #[test]
    fn hover_highlights_button_without_changing_focus() {
        let mut dialog = NoAgentsDialog::new();
        dialog.recheck_button_area = Rect::new(2, 5, 10, 1);
        dialog.quit_button_area = Rect::new(16, 5, 6, 1);
        assert!(dialog.recheck_focused);

        // Over Quit: highlight it, focus unchanged.
        assert!(dialog.handle_hover(17, 5));
        assert_eq!(dialog.hover.current(), Some(dialog.quit_button_area));
        assert!(dialog.recheck_focused, "hover must not move focus");

        // Off the buttons clears.
        assert!(dialog.handle_hover(99, 99));
        assert_eq!(dialog.hover.current(), None);
    }

    #[test]
    fn test_other_keys_continue() {
        let mut dialog = NoAgentsDialog::new();
        let result = dialog.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(result, DialogResult::Continue));
    }
}
