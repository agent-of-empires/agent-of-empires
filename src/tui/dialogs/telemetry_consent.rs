//! Standalone telemetry opt-in popup.
//!
//! Shown once to users who completed the first-run walkthrough before
//! telemetry existed (the walkthrough itself carries the prompt as its second
//! pane for new users). `Submit(true)` opts in, `Submit(false)` / `Cancel`
//! declines; the caller marks the prompt answered in either case so it never
//! re-appears. Startup gating ensures it never renders on top of the changelog
//! or the version update modal.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::styles::Theme;

#[derive(Default)]
pub struct TelemetryConsentDialog {
    /// Focused option: `Some(true)` = Enable, `Some(false)` = Decline,
    /// `None` = nothing focused yet. There is no default focus on purpose:
    /// a reflexive Enter does nothing until the user makes an explicit
    /// left/right choice, so the prompt can't be dismissed without reading.
    selected: Option<bool>,
    enable_button_area: Rect,
    decline_button_area: Rect,
    /// Which button the mouse is over (0 = Enable, 1 = Decline). Visual only.
    hovered: Option<usize>,
}

impl TelemetryConsentDialog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<bool> {
        match key.code {
            // Esc is the standard cancel; treat it as a decline.
            KeyCode::Esc => DialogResult::Submit(false),
            // Enter confirms a chosen side. With no choice it is a no-op so the
            // user can't blow past the prompt, EXCEPT under DO_NOT_TRACK, where
            // the dialog shows no buttons and explicitly says "Press Enter or
            // Esc to dismiss", so Enter must dismiss (as a decline).
            KeyCode::Enter | KeyCode::Char(' ') => match self.selected {
                Some(choice) => DialogResult::Submit(choice),
                None if crate::telemetry::do_not_track() => DialogResult::Submit(false),
                None => DialogResult::Continue,
            },
            KeyCode::Left | KeyCode::Char('h') => {
                self.selected = Some(true);
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.selected = Some(false);
                DialogResult::Continue
            }
            KeyCode::Tab => {
                self.selected = Some(!self.selected.unwrap_or(false));
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn handle_click(&self, col: u16, row: u16) -> Option<DialogResult<bool>> {
        let pos = Position::from((col, row));
        if self.enable_button_area.contains(pos) {
            return Some(DialogResult::Submit(true));
        }
        if self.decline_button_area.contains(pos) {
            return Some(DialogResult::Submit(false));
        }
        None
    }

    /// Update the hover highlight. Returns true when it changed (so the caller
    /// can skip redrawing on every pixel of mouse drift).
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let pos = Position::from((col, row));
        let new = if self.enable_button_area.contains(pos) {
            Some(0)
        } else if self.decline_button_area.contains(pos) {
            Some(1)
        } else {
            None
        };
        let changed = self.hovered != new;
        self.hovered = new;
        changed
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dnt = crate::telemetry::do_not_track();
        let dialog_area = super::centered_rect(area, 76, if dnt { 13 } else { 17 });
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Usage telemetry ")
            .title_style(Style::default().fg(theme.accent).bold());
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        if dnt {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(inner);
            self.render_dnt(frame, chunks[0], chunks[1], theme);
            return;
        }

        // body (top) · [Enable] [Not now] · "change any time" hint (bottom).
        // The hint sits under the buttons so the action is what the eye lands
        // on, not the fine print.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);

        let body = Paragraph::new(vec![
            Line::from(Span::styled(
                "Help improve aoe with anonymous usage telemetry?",
                Style::default().fg(theme.title).bold(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "It shows us how aoe is actually used, so we can prioritize the",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "features that matter most. Off by default.",
                Style::default().fg(theme.text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "When on, aoe sends anonymous counts only: sessions, agents and",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "models, your aoe version, and OS. Never prompts, paths, names,",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "branch names, or commands.",
                Style::default().fg(theme.text),
            )),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(body, chunks[0]);

        // chunks[1] is a one-row gap between the body and the buttons.
        self.render_buttons(frame, chunks[2], theme);

        // No default focus: tell the user how to choose so a bare Enter (which
        // is intentionally inert here) isn't a dead end.
        let keys = Paragraph::new(Span::styled(
            "←/→ or Tab to choose, then Enter to confirm",
            Style::default().fg(theme.hint).italic(),
        ))
        .alignment(Alignment::Center);
        frame.render_widget(keys, chunks[3]);

        let hint = Paragraph::new(Span::styled(
            "Change it any time under Settings, or with `aoe telemetry`.",
            Style::default().fg(theme.dimmed),
        ))
        .alignment(Alignment::Center);
        frame.render_widget(hint, chunks[4]);
    }

    fn render_dnt(&mut self, frame: &mut Frame, body_area: Rect, footer: Rect, theme: &Theme) {
        // DO_NOT_TRACK forces telemetry off; surface that rather than offering
        // a toggle that would do nothing.
        self.enable_button_area = Rect::default();
        self.decline_button_area = Rect::default();
        let body = Paragraph::new(vec![
            Line::from(Span::styled(
                "DO_NOT_TRACK is set in your environment.",
                Style::default().fg(theme.accent).bold(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Telemetry stays off and no install id is generated. You can",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "unset DO_NOT_TRACK and opt in under Settings if you change",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled("your mind.", Style::default().fg(theme.text))),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(body, body_area);
        let hint = Paragraph::new(Span::styled(
            "Press Enter or Esc to dismiss",
            Style::default().fg(theme.hint).italic(),
        ))
        .alignment(Alignment::Center);
        frame.render_widget(hint, footer);
    }

    fn render_buttons(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let enable_label = "[Enable]";
        let decline_label = "[Not now]";
        let gap = "    ";
        let row_width = (enable_label.len() + gap.len() + decline_label.len()) as u16;

        // A side lights up in its active color when it's the keyboard-focused
        // choice OR the mouse is hovering it; otherwise it stays dimmed. With
        // no choice and no hover, both are dimmed so nothing reads as
        // preselected. Hover drives the highlight only (like the other
        // dialogs), so a click is what commits, not a stray mouse drift.
        let enable_active = self.selected == Some(true) || self.hovered == Some(0);
        let decline_active = self.selected == Some(false) || self.hovered == Some(1);
        let enable_style = if enable_active {
            Style::default().fg(theme.accent).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };
        let decline_style = if decline_active {
            Style::default().fg(theme.running).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };
        let line = Line::from(vec![
            Span::styled(enable_label, enable_style),
            Span::raw(gap),
            Span::styled(decline_label, decline_style),
        ]);
        frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);

        if area.width < row_width || area.height == 0 {
            self.enable_button_area = Rect::default();
            self.decline_button_area = Rect::default();
            return;
        }
        let left_pad = (area.width - row_width) / 2;
        let enable_x = area.x + left_pad;
        let decline_x = enable_x + (enable_label.len() + gap.len()) as u16;
        self.enable_button_area = Rect::new(enable_x, area.y, enable_label.len() as u16, 1);
        self.decline_button_area = Rect::new(decline_x, area.y, decline_label.len() as u16, 1);

        if let Some(idx) = self.hovered {
            let rect = if idx == 0 {
                self.enable_button_area
            } else {
                self.decline_button_area
            };
            crate::tui::components::hover::paint_hover_bg(frame, rect, theme.selection);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use serial_test::serial;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // `#[serial]` because `render` reads `DO_NOT_TRACK`, which other telemetry
    // tests mutate. With it set, `render_dnt` zeroes the button areas and the
    // click lands on nothing, so a parallel run flakes this assertion.
    #[test]
    #[serial]
    fn click_after_render_submits_the_hit_button() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        unsafe { std::env::remove_var("DO_NOT_TRACK") };
        let theme = crate::tui::styles::load_theme("zinc");
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut d = TelemetryConsentDialog::new();
        term.draw(|f| d.render(f, f.area(), &theme)).unwrap();
        // Clicking the rendered [Enable] / [Not now] glyphs commits directly.
        let enable = d.enable_button_area;
        let decline = d.decline_button_area;
        assert!(matches!(
            d.handle_click(enable.x, enable.y),
            Some(DialogResult::Submit(true))
        ));
        assert!(matches!(
            d.handle_click(decline.x, decline.y),
            Some(DialogResult::Submit(false))
        ));
    }

    #[test]
    fn no_default_focus() {
        assert_eq!(TelemetryConsentDialog::new().selected, None);
    }

    // `#[serial]` because `handle_key` reads the `DO_NOT_TRACK` env var, which
    // other telemetry tests mutate; serializing keeps it deterministic.
    #[test]
    #[serial]
    fn enter_with_no_focus_is_inert() {
        // The whole point of no default focus: a reflexive Enter must not
        // dismiss the prompt until the user has chosen a side.
        unsafe { std::env::remove_var("DO_NOT_TRACK") };
        let mut d = TelemetryConsentDialog::new();
        assert!(matches!(
            d.handle_key(k(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert!(matches!(
            d.handle_key(k(KeyCode::Char(' '))),
            DialogResult::Continue
        ));
    }

    #[test]
    #[serial]
    fn enter_dismisses_under_do_not_track() {
        // Under DO_NOT_TRACK the popup shows no buttons and says "Press Enter
        // or Esc to dismiss", so Enter with no selection must decline-dismiss.
        unsafe { std::env::set_var("DO_NOT_TRACK", "1") };
        let mut d = TelemetryConsentDialog::new();
        let result = d.handle_key(k(KeyCode::Enter));
        unsafe { std::env::remove_var("DO_NOT_TRACK") };
        assert!(matches!(result, DialogResult::Submit(false)));
    }

    #[test]
    fn tab_focuses_then_enter_confirms() {
        let mut d = TelemetryConsentDialog::new();
        // Tab from no focus lands on Enable; a second Tab flips to Decline.
        assert!(matches!(
            d.handle_key(k(KeyCode::Tab)),
            DialogResult::Continue
        ));
        assert_eq!(d.selected, Some(true));
        assert!(matches!(
            d.handle_key(k(KeyCode::Tab)),
            DialogResult::Continue
        ));
        assert_eq!(d.selected, Some(false));
    }

    #[test]
    fn esc_declines() {
        assert!(matches!(
            TelemetryConsentDialog::new().handle_key(k(KeyCode::Esc)),
            DialogResult::Submit(false)
        ));
    }

    #[test]
    fn left_focuses_enable_then_enter_opts_in() {
        let mut d = TelemetryConsentDialog::new();
        assert!(matches!(
            d.handle_key(k(KeyCode::Left)),
            DialogResult::Continue
        ));
        assert_eq!(d.selected, Some(true));
        assert!(matches!(
            d.handle_key(k(KeyCode::Enter)),
            DialogResult::Submit(true)
        ));
    }

    #[test]
    fn right_focuses_decline_then_enter_declines() {
        let mut d = TelemetryConsentDialog::new();
        assert!(matches!(
            d.handle_key(k(KeyCode::Right)),
            DialogResult::Continue
        ));
        assert_eq!(d.selected, Some(false));
        assert!(matches!(
            d.handle_key(k(KeyCode::Enter)),
            DialogResult::Submit(false)
        ));
    }
}
