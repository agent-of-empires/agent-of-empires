//! Acknowledgment dialog for first-time agent status hook installation

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

pub struct HooksInstallDialog {
    settings_paths: Vec<String>,
    hook_commands: Vec<(String, String)>,
    needs_codex_trust_note: bool,
    selected: bool, // true = Accept, false = Cancel
    scroll_offset: u16,
    accept_button_area: Rect,
    cancel_button_area: Rect,
    /// Which button the mouse is over, for the hover highlight. Visual
    /// only; never changes `selected`.
    hover: HoverState,
}

impl HooksInstallDialog {
    pub fn new(tool_name: &str) -> Self {
        Self::new_for_profile(tool_name, None)
    }

    pub fn new_for_profile(tool_name: &str, profile: Option<&str>) -> Self {
        let mut settings_paths = Vec::new();
        let mut hook_commands = Vec::new();
        let mut needs_codex_trust_note = false;

        if let Some(agent) = crate::agents::get_agent(tool_name) {
            if let Some(hook_cfg) = &agent.hook_config {
                let host_env = profile
                    .map(crate::session::profile_config::resolve_config_or_warn)
                    .map(|config| config.environment)
                    .unwrap_or_default();
                match hook_cfg.format {
                    crate::agents::HookFormat::CodexJson => {
                        needs_codex_trust_note = true;
                        settings_paths.push(
                            crate::hooks::codex_hooks_json_path_display_for_host_environment(
                                &host_env,
                            ),
                        );
                    }
                    crate::agents::HookFormat::JsonSettings => {
                        settings_paths.push(
                            crate::hooks::agent_settings_path_display_for_host_environment(
                                hook_cfg, &host_env,
                            ),
                        );
                    }
                }
                for event in hook_cfg.events {
                    let label = match event.status {
                        Some(s) => format!("writes \"{}\"", s),
                        None => "session lifecycle".to_string(),
                    };
                    hook_commands.push((event.name.to_string(), label));
                }
            }
        }

        Self {
            settings_paths,
            hook_commands,
            needs_codex_trust_note,
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
            "Modified files:",
            Style::default().bold(),
        )));
        for path in &self.settings_paths {
            lines.push(Line::from(format!("  {}", path)));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Hook events added:",
            Style::default().bold(),
        )));
        for (event, status) in &self.hook_commands {
            lines.push(Line::from(format!("  {} -> {}", event, status)));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Each hook runs:",
            Style::default().bold(),
        )));
        // The euid in the displayed path matches the runtime path baked into
        // the hook command and is already exposed via `id -u` and `ps`. The
        // alternative (a placeholder) would mislead users about what is
        // actually installed.
        lines.push(Line::from(format!(
            "  printf {{status}} > {}/$AOE_INSTANCE_ID/status",
            crate::hooks::hook_base_path().display()
        )));

        lines.push(Line::from(""));
        lines.push(Line::from(
            "Hooks are guarded by $AOE_INSTANCE_ID and are a",
        ));
        lines.push(Line::from("no-op outside of AoE sessions."));

        if self.needs_codex_trust_note {
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
            .title(" Agent Status Hooks ")
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
    use crate::session::test_support::EnvGuard;
    use crossterm::event::KeyModifiers;
    use tempfile::TempDir;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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
    fn test_default_selection_is_accept() {
        let dialog = HooksInstallDialog::new("claude");
        assert!(dialog.selected);
    }

    #[test]
    fn test_y_accepts() {
        let mut dialog = HooksInstallDialog::new("claude");
        let result = dialog.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(result, DialogResult::Submit(true)));
    }

    #[test]
    fn test_n_cancels() {
        let mut dialog = HooksInstallDialog::new("claude");
        let result = dialog.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_esc_cancels() {
        let mut dialog = HooksInstallDialog::new("claude");
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_with_accept_selected() {
        let mut dialog = HooksInstallDialog::new("claude");
        dialog.selected = true;
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(true)));
    }

    #[test]
    fn test_enter_with_cancel_selected() {
        let mut dialog = HooksInstallDialog::new("claude");
        dialog.selected = false;
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_tab_toggles() {
        let mut dialog = HooksInstallDialog::new("claude");
        assert!(dialog.selected);
        dialog.handle_key(key(KeyCode::Tab));
        assert!(!dialog.selected);
        dialog.handle_key(key(KeyCode::Tab));
        assert!(dialog.selected);
    }

    #[test]
    fn hover_highlights_button_without_changing_selection() {
        let mut dialog = HooksInstallDialog::new("claude");
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
    fn test_content_shows_settings_path() {
        let dialog = HooksInstallDialog::new("claude");
        let lines = dialog.build_content_lines();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(".claude/settings.json"));
    }

    #[test]
    fn test_content_shows_hook_events() {
        let dialog = HooksInstallDialog::new("claude");
        let lines = dialog.build_content_lines();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("PreToolUse"));
        assert!(text.contains("Stop"));
        assert!(text.contains("Notification"));
    }

    #[test]
    fn test_content_uses_aoe_instance_id_in_example() {
        let dialog = HooksInstallDialog::new("claude");
        let lines = dialog.build_content_lines();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains(&format!(
                "{}/$AOE_INSTANCE_ID/status",
                crate::hooks::hook_base_path().display()
            )),
            "example command must reference the per-user (issue #1844) path: {text}"
        );
        assert!(
            !text.contains("/tmp/aoe-hooks/$ID/"),
            "example command must not use the bogus $ID placeholder: {text}"
        );
        assert!(
            !text.contains("/tmp/aoe-hooks/$AOE_INSTANCE_ID/status"),
            "example must not use the legacy multi-tenant-vulnerable path: {text}"
        );
    }

    #[test]
    fn test_cursor_agent_shows_cursor_path() {
        let dialog = HooksInstallDialog::new("cursor");
        let lines = dialog.build_content_lines();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(".cursor/settings.json"));
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_agent_shows_hooks_and_config_paths() {
        let _guard = EnvGuard::unset(&["CODEX_HOME"]);
        let dialog = HooksInstallDialog::new("codex");
        let lines = dialog.build_content_lines();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(".codex/hooks.json"));
        assert!(!text.contains(".codex/config.toml"));
        assert!(text.contains("trust these hooks in /hooks"));
        assert!(text.contains("pane-based status detection"));
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_agent_shows_codex_home_config_path() {
        let tmp = TempDir::new().unwrap();
        let _guard = EnvGuard::set(&[("CODEX_HOME", tmp.path())]);

        let dialog = HooksInstallDialog::new("codex");
        let text = content_text(&dialog);

        assert!(text.contains(&tmp.path().join("hooks.json").display().to_string()));
        assert!(!text.contains(&tmp.path().join("config.toml").display().to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_agent_shows_profile_codex_home_config_path() {
        let tmp = TempDir::new().unwrap();
        let _guard = EnvGuard::unset(&["CODEX_HOME"]);
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        let codex_home = tmp.path().join("profile-codex-home");
        let profile_dir = crate::session::get_profile_dir("codex-profile").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            format!("environment = [\"CODEX_HOME={}\"]\n", codex_home.display()),
        )
        .unwrap();

        let dialog = HooksInstallDialog::new_for_profile("codex", Some("codex-profile"));
        let text = content_text(&dialog);

        assert!(text.contains(&codex_home.join("hooks.json").display().to_string()));
        assert!(!text.contains(&codex_home.join("config.toml").display().to_string()));
    }

    #[test]
    fn test_non_codex_agents_do_not_show_codex_trust_note() {
        let dialog = HooksInstallDialog::new("claude");
        let lines = dialog.build_content_lines();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!text.contains("trust these hooks in /hooks"));
        assert!(!text.contains("pane-based status detection"));
    }
}
