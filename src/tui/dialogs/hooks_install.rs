//! Acknowledgment dialog for first-time agent status hook installation

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::hook_disclosure::HookDisclosure;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

pub struct HooksInstallDialog {
    disclosure: HookDisclosure,
    /// The remote the paths belong to; `None` for this machine.
    machine: Option<String>,
    selected: bool, // true = Accept, false = Cancel
    scroll_offset: u16,
    accept_button_area: Rect,
    cancel_button_area: Rect,
    /// Which button the mouse is over, for the hover highlight. Visual
    /// only; never changes `selected`.
    hover: HoverState,
}

impl HooksInstallDialog {
    /// Approve hook installation on this machine.
    pub fn local(tool_name: &str, agent_name: &str, profile: Option<&str>) -> Self {
        Self::new(
            crate::session::hook_disclosure::hook_disclosure(tool_name, agent_name, profile),
            None,
        )
    }

    /// `machine` names the remote whose paths `disclosure` describes.
    pub fn new(disclosure: HookDisclosure, machine: Option<String>) -> Self {
        Self {
            disclosure,
            machine,
            selected: true,
            scroll_offset: 0,
            accept_button_area: Rect::default(),
            cancel_button_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    pub fn handle_click(&self, col: u16, row: u16) -> Option<DialogResult<bool>> {
        let pos = ratatui::layout::Position::from((col, row));
        if self.accept_button_area.contains(pos) {
            return Some(DialogResult::Submit(true));
        }
        if self.cancel_button_area.contains(pos) {
            return Some(DialogResult::Cancel);
        }
        None
    }

    /// Highlight the button under the cursor without changing the
    /// Accept / Cancel selection. See `ConfirmDialog::handle_hover` for
    /// the rationale. Returns `true` when the highlighted button changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.hover.update(
            col,
            row,
            &[self.accept_button_area, self.cancel_button_area],
        )
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<bool> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Char('y') | KeyCode::Char('Y') => DialogResult::Submit(true),
            KeyCode::Char('n') | KeyCode::Char('N') => DialogResult::Cancel,
            KeyCode::Enter => {
                if self.selected {
                    DialogResult::Submit(true)
                } else {
                    DialogResult::Cancel
                }
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
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let total_lines = self.build_content_lines().len() as u16;
                if self.scroll_offset + 1 < total_lines {
                    self.scroll_offset += 1;
                }
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn build_content_lines(&self) -> Vec<Line<'_>> {
        let mut lines = Vec::new();

        lines.push(Line::from(Span::styled(
            match &self.machine {
                Some(machine) => format!("Modified files on {machine}:"),
                None => "Modified files:".to_string(),
            },
            Style::default().bold(),
        )));
        for path in &self.disclosure.settings_paths {
            lines.push(Line::from(format!("  {}", path)));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Hook events added:",
            Style::default().bold(),
        )));
        for command in &self.disclosure.hook_commands {
            lines.push(Line::from(format!(
                "  {} -> {}",
                command.event, command.writes
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Each hook runs:",
            Style::default().bold(),
        )));
        lines.push(Line::from(format!(
            "  {}",
            self.disclosure.status_write_command
        )));

        lines.push(Line::from(""));
        lines.push(Line::from(
            "Hooks are guarded by $AOE_INSTANCE_ID and are a",
        ));
        lines.push(Line::from("no-op outside of AoE sessions."));

        if self.disclosure.needs_codex_trust_note {
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Codex may ask you to review and trust these hooks in /hooks.",
            ));
            lines.push(Line::from(
                "Until then, AoE falls back to pane-based status detection.",
            ));
        }

        lines
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let content_lines = self.build_content_lines();
        let content_height = content_lines.len() as u16 + 6; // header + spacing + buttons

        let dialog_width = 64.min(area.width.saturating_sub(4));
        let dialog_height = (content_height + 6).min(area.height.saturating_sub(4));
        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(match &self.machine {
                Some(machine) => format!(" Agent Status Hooks on {machine} "),
                None => " Agent Status Hooks ".to_string(),
            })
            .title_style(Style::default().fg(theme.accent).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // header
                Constraint::Min(1),    // content
                Constraint::Length(2), // buttons
            ])
            .split(inner);

        // Header
        let header = Paragraph::new(
            "AoE needs to install hooks into your agent's settings\nto detect session status (running/waiting/idle).",
        )
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: true });
        frame.render_widget(header, chunks[0]);

        // Scrollable content
        let visible_lines: Vec<Line> = content_lines
            .into_iter()
            .skip(self.scroll_offset as usize)
            .collect();
        let content_paragraph = Paragraph::new(visible_lines)
            .style(Style::default().fg(theme.dimmed))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(content_paragraph, chunks[1]);

        // Buttons
        let accept_style = if self.selected {
            Style::default().fg(theme.running).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };
        let cancel_style = if !self.selected {
            Style::default().fg(theme.accent).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };

        let accept_label = "[Accept (y)]";
        let cancel_label = "[Cancel (Esc)]";
        let gap: u16 = 4;
        let prefix: u16 = 2;
        let accept_w = accept_label.chars().count() as u16;
        let cancel_w = cancel_label.chars().count() as u16;
        let total = prefix + accept_w + gap + cancel_w;
        let button_area = chunks[2];
        if button_area.width >= total {
            let left_pad = (button_area.width - total) / 2;
            let accept_x = button_area.x + left_pad + prefix;
            let cancel_x = accept_x + accept_w + gap;
            self.accept_button_area = Rect::new(accept_x, button_area.y, accept_w, 1);
            self.cancel_button_area = Rect::new(cancel_x, button_area.y, cancel_w, 1);
        } else {
            self.accept_button_area = Rect::default();
            self.cancel_button_area = Rect::default();
        }

        let buttons = Line::from(vec![
            Span::raw("  "),
            Span::styled(accept_label, accept_style),
            Span::raw("    "),
            Span::styled(cancel_label, cancel_style),
        ]);

        frame.render_widget(
            Paragraph::new(buttons).alignment(Alignment::Center),
            button_area,
        );

        if let Some(rect) = self
            .hover
            .current_in(&[self.accept_button_area, self.cancel_button_area])
        {
            paint_hover_bg(frame, rect, theme.selection);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::hook_disclosure::HookCommand;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn dialog(machine: Option<&str>) -> HooksInstallDialog {
        HooksInstallDialog::new(
            HookDisclosure {
                settings_paths: vec!["/home/ada/.claude/settings.json".into()],
                hook_commands: vec![HookCommand {
                    event: "Stop".into(),
                    writes: "writes \"idle\"".into(),
                }],
                status_write_command: "printf {status} > /run/aoe-1000/$AOE_INSTANCE_ID/status"
                    .into(),
                needs_codex_trust_note: false,
            },
            machine.map(str::to_string),
        )
    }

    fn content_text(dialog: &HooksInstallDialog) -> String {
        dialog
            .build_content_lines()
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn keys_accept_cancel_and_move_the_selection() {
        assert!(dialog(None).selected, "Accept is preselected");
        for (code, expected_submit) in [
            (KeyCode::Char('y'), true),
            (KeyCode::Char('n'), false),
            (KeyCode::Esc, false),
        ] {
            let result = dialog(None).handle_key(key(code));
            assert_eq!(
                matches!(result, DialogResult::Submit(true)),
                expected_submit
            );
        }

        let mut d = dialog(None);
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(true)
        ));
        d.handle_key(key(KeyCode::Tab));
        assert!(!d.selected);
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Cancel
        ));
        d.handle_key(key(KeyCode::Tab));
        assert!(d.selected);
    }

    #[test]
    fn hover_highlights_button_without_changing_selection() {
        let mut dialog = dialog(None);
        dialog.accept_button_area = Rect::new(2, 5, 12, 1);
        dialog.cancel_button_area = Rect::new(20, 5, 14, 1);
        assert!(dialog.selected);

        // Over Accept: highlight it, selection unchanged.
        assert!(dialog.handle_hover(3, 5));
        assert_eq!(dialog.hover.current(), Some(dialog.accept_button_area));
        assert!(dialog.selected, "hover must not flip the selection");

        // Over Cancel.
        assert!(dialog.handle_hover(21, 5));
        assert_eq!(dialog.hover.current(), Some(dialog.cancel_button_area));

        // Off the buttons clears.
        assert!(dialog.handle_hover(0, 0));
        assert_eq!(dialog.hover.current(), None);
    }

    #[test]
    fn content_shows_the_disclosure_it_was_built_from() {
        let text = content_text(&dialog(None));
        assert!(text.contains("/home/ada/.claude/settings.json"), "{text}");
        assert!(text.contains("Stop -> writes \"idle\""), "{text}");
        assert!(
            text.contains("printf {status} > /run/aoe-1000/$AOE_INSTANCE_ID/status"),
            "{text}"
        );
        assert!(!text.contains("trust these hooks in /hooks"), "{text}");
    }

    #[test]
    fn a_codex_disclosure_adds_the_trust_note() {
        let mut dialog = dialog(None);
        dialog.disclosure.needs_codex_trust_note = true;
        let text = content_text(&dialog);
        assert!(text.contains("trust these hooks in /hooks"), "{text}");
        assert!(text.contains("pane-based status detection"), "{text}");
    }

    #[test]
    fn a_remote_disclosure_names_the_machine_the_paths_belong_to() {
        let text = content_text(&dialog(Some("mini")));
        assert!(text.contains("Modified files on mini:"), "{text}");
        assert!(!content_text(&dialog(None)).contains("on mini"));
    }
}
