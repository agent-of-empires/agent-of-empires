//! Snooze duration picker. Opens when the user presses `h`/`H`/`w`/`W`
//! on a non-snoozed session; single-key shortcuts so the choice is
//! one keystroke after the trigger.
//!
//! Mapping principle: the digit IS the duration where it can be.
//!   1..6 → that many hours
//!   8    → 24 hours (one day)
//!   0    → 1 week
//!   7,9  → unbound (no preset, fall through; reserved for future)

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::styles::Theme;

const ONE_HOUR: u32 = 60;
const TWO_HOURS: u32 = 2 * 60;
const THREE_HOURS: u32 = 3 * 60;
const FOUR_HOURS: u32 = 4 * 60;
const FIVE_HOURS: u32 = 5 * 60;
const SIX_HOURS: u32 = 6 * 60;
const ONE_DAY: u32 = 24 * 60;
const ONE_WEEK: u32 = 7 * 24 * 60;

pub struct SnoozeDurationDialog {
    title: String,
    /// Hit rect per preset row with the minutes it submits, so a click
    /// matches its digit key.
    row_rects: Vec<(u32, Rect)>,
    /// Hovered row. Drives the highlight only; a click submits.
    hovered_row: Option<usize>,
    selected_minutes: Option<u32>,
    error: Option<super::InfoDialog>,
}

impl SnoozeDurationDialog {
    pub fn new(session_title: &str) -> Self {
        Self {
            title: session_title.to_string(),
            row_rects: Vec::new(),
            hovered_row: None,
            selected_minutes: None,
            error: None,
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<u32>> {
        if let Some(error) = &self.error {
            if error.handle_click(col, row).is_some() {
                self.error = None;
            }
            return Some(DialogResult::Continue);
        }
        super::hit(&self.row_rects, col, row).map(DialogResult::Submit)
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        if let Some(error) = &mut self.error {
            return error.handle_hover(col, row);
        }
        let pos = ratatui::layout::Position::from((col, row));
        let new_hover = self
            .row_rects
            .iter()
            .position(|(_, rect)| rect.contains(pos));
        if self.hovered_row == new_hover {
            return false;
        }
        self.hovered_row = new_hover;
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<u32> {
        if let Some(error) = &mut self.error {
            if !matches!(error.handle_key(key), DialogResult::Continue) {
                self.error = None;
            }
            return DialogResult::Continue;
        }
        if key.code == KeyCode::Enter {
            return self
                .selected_minutes
                .map_or(DialogResult::Continue, DialogResult::Submit);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => DialogResult::Cancel,
            KeyCode::Char('1') => DialogResult::Submit(ONE_HOUR),
            KeyCode::Char('2') => DialogResult::Submit(TWO_HOURS),
            KeyCode::Char('3') => DialogResult::Submit(THREE_HOURS),
            KeyCode::Char('4') => DialogResult::Submit(FOUR_HOURS),
            KeyCode::Char('5') => DialogResult::Submit(FIVE_HOURS),
            KeyCode::Char('6') => DialogResult::Submit(SIX_HOURS),
            KeyCode::Char('8') => DialogResult::Submit(ONE_DAY),
            KeyCode::Char('0') => DialogResult::Submit(ONE_WEEK),
            _ => DialogResult::Continue,
        }
    }
    pub(crate) fn reject(&mut self, minutes: u32, message: &str) {
        self.selected_minutes = Some(minutes);
        self.error = Some(super::InfoDialog::sized_to_fit(
            "Snooze Not Changed",
            message,
        ));
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.row_rects.clear();
        let block = super::toned_dialog_block(" Snooze ", theme.waiting, theme.waiting);
        let (_, inner) = super::render_dialog_frame(frame, area, 52, 14, block);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // session title
                Constraint::Length(1), // spacer
                Constraint::Length(1), // 1
                Constraint::Length(1), // 2
                Constraint::Length(1), // 3
                Constraint::Length(1), // 4
                Constraint::Length(1), // 5
                Constraint::Length(1), // 6
                Constraint::Length(1), // 8
                Constraint::Length(1), // 0
            ])
            .split(inner);

        let subject = Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{}  ", self.title),
                Style::default().fg(theme.text).bold(),
            ),
            Span::styled("how long?", Style::default().fg(theme.dimmed)),
        ]))
        .alignment(Alignment::Center);
        frame.render_widget(subject, chunks[0]);

        let key_style = Style::default().fg(theme.waiting).bold();
        let text_style = Style::default().fg(theme.text);
        let hover_text_style = Style::default().fg(theme.accent).bold();
        let presets: &[(u32, &str, &str, usize)] = &[
            (ONE_HOUR, "1", "1 hour", 2),
            (TWO_HOURS, "2", "2 hours", 3),
            (THREE_HOURS, "3", "3 hours", 4),
            (FOUR_HOURS, "4", "4 hours", 5),
            (FIVE_HOURS, "5", "5 hours", 6),
            (SIX_HOURS, "6", "6 hours", 7),
            (ONE_DAY, "8", "24 hours (1 day)", 8),
            (ONE_WEEK, "0", "1 week", 9),
        ];
        for (idx, (minutes, k, label, ci)) in presets.iter().enumerate() {
            let area = chunks[*ci];
            let label_style = if self.hovered_row == Some(idx) {
                hover_text_style
            } else {
                text_style
            };
            let line = Paragraph::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("[{}]", k), key_style),
                Span::raw("  "),
                Span::styled(*label, label_style),
            ]));
            frame.render_widget(line, area);
            self.row_rects.push((*minutes, area));
        }
        if let Some(error) = &mut self.error {
            error.render(frame, area, theme);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn keys_pick_a_preset_or_cancel() {
        let cases = [
            ('1', DialogResult::Submit(60)),
            ('2', DialogResult::Submit(120)),
            ('3', DialogResult::Submit(180)),
            ('4', DialogResult::Submit(240)),
            ('5', DialogResult::Submit(300)),
            ('6', DialogResult::Submit(360)),
            ('8', DialogResult::Submit(1440)),
            ('0', DialogResult::Submit(10080)),
            ('7', DialogResult::Continue),
            ('9', DialogResult::Continue),
            ('x', DialogResult::Continue),
            ('q', DialogResult::Cancel),
        ];
        for (ch, want) in cases {
            let mut d = SnoozeDurationDialog::new("sess");
            assert_eq!(d.handle_key(k(KeyCode::Char(ch))), want, "{ch}");
        }
        let mut d = SnoozeDurationDialog::new("sess");
        assert_eq!(d.handle_key(k(KeyCode::Esc)), DialogResult::Cancel);
    }
}
