//! A creation refused because its `on_create` hook failed. The daemon already rolled the
//! attempt back, so every choice here starts from a clean slate: Retry resubmits, Create
//! without setup resubmits with the hooks skipped, and Cancel drops the request.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateHookFailedAction {
    Retry,
    CreateWithoutSetup,
}

pub struct CreateHookFailedDialog {
    message: String,
    selected: CreateHookFailedAction,
    retry_area: Rect,
    skip_area: Rect,
    cancel_area: Rect,
    hover: HoverState,
}

const RETRY_LABEL: &str = "[Retry (r)]";
const SKIP_LABEL: &str = "[Create without setup (s)]";
const CANCEL_LABEL: &str = "[Cancel (Esc)]";
const EXPLANATION: &str =
    "The session was not created. Retry once the hook is fixed, or create it without running on_create.";

impl CreateHookFailedDialog {
    pub fn new(message: &str) -> Self {
        Self {
            message: message.trim_end().to_string(),
            selected: CreateHookFailedAction::Retry,
            retry_area: Rect::default(),
            skip_area: Rect::default(),
            cancel_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<CreateHookFailedAction> {
        use CreateHookFailedAction::*;
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => DialogResult::Submit(self.selected),
            KeyCode::Char('r' | 'R') => DialogResult::Submit(Retry),
            KeyCode::Char('s' | 'S') => DialogResult::Submit(CreateWithoutSetup),
            KeyCode::Left | KeyCode::Char('h') => {
                self.selected = Retry;
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.selected = CreateWithoutSetup;
                DialogResult::Continue
            }
            KeyCode::Tab => {
                self.selected = match self.selected {
                    Retry => CreateWithoutSetup,
                    CreateWithoutSetup => Retry,
                };
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn handle_click(&self, col: u16, row: u16) -> Option<DialogResult<CreateHookFailedAction>> {
        let pos = Position::from((col, row));
        if self.retry_area.contains(pos) {
            Some(DialogResult::Submit(CreateHookFailedAction::Retry))
        } else if self.skip_area.contains(pos) {
            Some(DialogResult::Submit(
                CreateHookFailedAction::CreateWithoutSetup,
            ))
        } else if self.cancel_area.contains(pos) {
            Some(DialogResult::Cancel)
        } else {
            None
        }
    }

    /// Highlights the button under the cursor without changing the selection.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.hover.update(
            col,
            row,
            &[self.retry_area, self.skip_area, self.cancel_area],
        )
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        const MAX_MESSAGE_ROWS: usize = 20;
        let dialog_width = 76.min(area.width.saturating_sub(4));
        let text_width = dialog_width.saturating_sub(4) as usize;
        let mut rows = super::info::wrap_to_width(&self.message, text_width);
        if rows.len() > MAX_MESSAGE_ROWS {
            let hidden = rows.len() - (MAX_MESSAGE_ROWS - 1);
            rows.drain(..hidden);
            rows.insert(0, format!("… {hidden} earlier lines hidden"));
        }
        let explanation_rows = super::info::wrap_to_width(EXPLANATION, text_width).len() as u16;
        // The message, a gap, the explanation, a gap and the buttons, inside border and margin.
        let dialog_height = (rows.len() as u16 + explanation_rows + 7).min(area.height);
        let block = super::toned_dialog_block(" Session setup failed ", theme.error, theme.error);
        let (_, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);

        let message_rows = rows.len() as u16;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(message_rows),
                Constraint::Length(1),
                Constraint::Length(explanation_rows),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);
        let lines: Vec<Line> = rows.into_iter().map(Line::from).collect();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(theme.text)),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(EXPLANATION)
                .style(Style::default().fg(theme.dimmed))
                .wrap(Wrap { trim: true }),
            chunks[2],
        );

        let style = |action| {
            if self.selected == action {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.dimmed)
            }
        };
        let gap = 3u16;
        let widths = [RETRY_LABEL, SKIP_LABEL, CANCEL_LABEL].map(|label| label.len() as u16);
        let total = widths.iter().sum::<u16>() + 2 * gap;
        let row = chunks[4];
        if row.width >= total {
            let x = row.x + (row.width - total) / 2;
            self.retry_area = Rect::new(x, row.y, widths[0], 1);
            self.skip_area = Rect::new(x + widths[0] + gap, row.y, widths[1], 1);
            self.cancel_area = Rect::new(x + widths[0] + widths[1] + 2 * gap, row.y, widths[2], 1);
        } else {
            self.retry_area = Rect::default();
            self.skip_area = Rect::default();
            self.cancel_area = Rect::default();
        }
        let spacer = " ".repeat(gap as usize);
        let buttons = Line::from(vec![
            Span::styled(RETRY_LABEL, style(CreateHookFailedAction::Retry)),
            Span::raw(spacer.clone()),
            Span::styled(
                SKIP_LABEL,
                style(CreateHookFailedAction::CreateWithoutSetup),
            ),
            Span::raw(spacer),
            Span::styled(CANCEL_LABEL, Style::default().fg(theme.dimmed)),
        ]);
        frame.render_widget(Paragraph::new(buttons).alignment(Alignment::Center), row);

        if let Some(rect) =
            self.hover
                .current_in(&[self.retry_area, self.skip_area, self.cancel_area])
        {
            paint_hover_bg(frame, rect, theme.selection);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn each_key_picks_one_choice() {
        use CreateHookFailedAction::*;
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // (keys, result of the last one)
        for (keys, want) in [
            (&[KeyCode::Enter][..], Some(Retry)),
            (&[KeyCode::Char('r')], Some(Retry)),
            (&[KeyCode::Char('s')], Some(CreateWithoutSetup)),
            (&[KeyCode::Right, KeyCode::Enter], Some(CreateWithoutSetup)),
            (&[KeyCode::Tab, KeyCode::Tab, KeyCode::Enter], Some(Retry)),
            (
                &[KeyCode::Right, KeyCode::Left, KeyCode::Enter],
                Some(Retry),
            ),
            (&[KeyCode::Esc], None),
        ] {
            let mut dialog = CreateHookFailedDialog::new("on_create hook failed: exit 1");
            let mut last = DialogResult::Continue;
            for code in keys {
                last = dialog.handle_key(key(*code));
            }
            match (last, want) {
                (DialogResult::Submit(got), Some(want)) => assert_eq!(got, want, "{keys:?}"),
                (DialogResult::Cancel, None) => {}
                _ => panic!("{keys:?}: unexpected result"),
            }
        }
    }
}
