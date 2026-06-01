//! Update confirmation dialog: shows the prompt block and Y/N buttons.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::buttons::render_yes_no;
use crate::tui::components::hover::HoverState;
use crate::tui::styles::Theme;
use crate::update::install::InstallMethod;

pub struct UpdateConfirmDialog {
    prompt_block: String,
    pub method: InstallMethod,
    pub latest_version: String,
    pub needs_sudo: bool,
    selected: bool, // true = Yes, false = No
    yes_button_area: Rect,
    no_button_area: Rect,
    /// Which Yes/No button the mouse is over, for the hover highlight.
    /// Visual only; never changes `selected`.
    hover: HoverState,
}

impl UpdateConfirmDialog {
    pub fn new(
        current_version: String,
        latest_version: String,
        method: InstallMethod,
        needs_sudo: bool,
    ) -> Self {
        let prompt_block = crate::update::install::format_prompt_block(
            &current_version,
            &latest_version,
            &method,
            needs_sudo,
        );
        Self {
            prompt_block,
            method,
            latest_version,
            needs_sudo,
            selected: false,
            yes_button_area: Rect::default(),
            no_button_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

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

    /// Highlight the Yes/No button under the cursor without changing
    /// `selected`. See `ConfirmDialog::handle_hover` for the rationale.
    /// Returns `true` when the highlighted button changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.hover
            .update(col, row, &[self.yes_button_area, self.no_button_area])
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
        let height = if self.needs_sudo { 11 } else { 10 };
        let dialog_area = super::centered_rect(area, 60, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.waiting))
            .title(" Update aoe ")
            .title_style(Style::default().fg(theme.waiting).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(inner);

        let body =
            Paragraph::new(self.prompt_block.as_str()).style(Style::default().fg(theme.text));
        frame.render_widget(body, chunks[0]);

        let (yes, no) = render_yes_no(frame, chunks[1], theme, self.selected, self.hover.current());
        self.yes_button_area = yes;
        self.no_button_area = no;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use std::path::PathBuf;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn dialog() -> UpdateConfirmDialog {
        UpdateConfirmDialog::new(
            "0.4.5".into(),
            "0.5.0".into(),
            InstallMethod::Tarball {
                binary_path: PathBuf::from("/usr/local/bin/aoe"),
            },
            true,
        )
    }

    #[test]
    fn default_selection_is_no() {
        assert!(!dialog().selected);
    }

    #[test]
    fn esc_cancels() {
        assert!(matches!(
            dialog().handle_key(k(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn y_submits() {
        assert!(matches!(
            dialog().handle_key(k(KeyCode::Char('y'))),
            DialogResult::Submit(())
        ));
    }

    #[test]
    fn enter_with_no_selected_cancels() {
        assert!(matches!(
            dialog().handle_key(k(KeyCode::Enter)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn enter_with_yes_selected_submits() {
        let mut d = dialog();
        d.selected = true;
        assert!(matches!(
            d.handle_key(k(KeyCode::Enter)),
            DialogResult::Submit(())
        ));
    }
}
