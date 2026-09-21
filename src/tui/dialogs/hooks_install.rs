//! Acknowledgment dialog for first-time status hook installation.

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
    /// The hovered button. Visual only; never changes `selected`.
    hover: HoverState,
}

impl HooksInstallDialog {
    pub fn new(tool_name: &str) -> Self {
        Self::new_for_profile(tool_name, None)
    }

    pub fn new_for_profile(tool_name: &str, profile: Option<&str>) -> Self {
        let profile_config =
            profile.map(crate::session::config::profile_config::resolve_config_or_warn);
        let agent_name = crate::agents::get_agent(tool_name)
            .or_else(|| {
                profile_config
                    .as_ref()
                    .and_then(|config| config.session.agent_detect_as.get(tool_name))
                    .and_then(|detect_as| crate::agents::get_agent(detect_as))
            })
            .map_or(tool_name, |agent| agent.name);
        Self::new_for_profile_resolved(tool_name, agent_name, profile)
    }

    pub fn new_for_profile_resolved(
        tool_name: &str,
        agent_name: &str,
        profile: Option<&str>,
    ) -> Self {
        let mut settings_paths = Vec::new();
        let mut hook_commands = Vec::new();
        let mut needs_codex_trust_note = false;

        let profile_config =
            profile.map(crate::session::config::profile_config::resolve_config_or_warn);
        if let Some(agent) = crate::agents::get_agent(agent_name) {
            if let Some(hook_cfg) = &agent.hook_config {
                let host_env = profile_config
                    .as_ref()
                    .map(|config| config.environment.clone())
                    .unwrap_or_default();
                let profile_home =
                    crate::session::environment::resolve_host_environment_value(&host_env, "HOME")
                        .map(std::path::PathBuf::from)
                        .or_else(dirs::home_dir)
                        .unwrap_or_else(|| std::path::PathBuf::from("~"));
                let default_config = crate::session::config::SessionConfig::default();
                let session_config = profile_config
                    .as_ref()
                    .map(|config| &config.session)
                    .unwrap_or(&default_config);
                needs_codex_trust_note = hook_cfg.format == crate::agents::HookFormat::CodexJson;
                settings_paths.push(
                    crate::session::generic_host_config_path_for(
                        tool_name,
                        hook_cfg,
                        &profile_home,
                        session_config,
                        &host_env,
                    )
                    .to_string_lossy()
                    .into_owned(),
                );
                for event in hook_cfg.events {
                    let label = match event.status {
                        Some(status) => format!("writes \"{}\"", status),
                        None => "session lifecycle".to_string(),
                    };
                    hook_commands.push((event.name.to_string(), label));
                }
            } else if let Some(sidecar) = &agent.sidecar_hooks {
                let host_environment = profile_config
                    .as_ref()
                    .map(|config| config.environment.as_slice())
                    .unwrap_or_default();
                let home = crate::session::environment::resolve_host_environment_value(
                    host_environment,
                    "HOME",
                )
                .map(std::path::PathBuf::from)
                .or_else(dirs::home_dir)
                .unwrap_or_else(|| std::path::PathBuf::from("~"));
                let default_config = crate::session::config::SessionConfig::default();
                let session_config = profile_config
                    .as_ref()
                    .map(|config| &config.session)
                    .unwrap_or(&default_config);
                let path = crate::session::sidecar_host_config_path_for(
                    tool_name,
                    agent,
                    sidecar,
                    &home,
                    session_config,
                    host_environment,
                );
                settings_paths.push(path.to_string_lossy().into_owned());
                for event in sidecar.events {
                    hook_commands.push((
                        event.name.to_string(),
                        format!("writes \"{}\"", event.status),
                    ));
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

    /// Highlight the button under the cursor without changing the selection.
    /// True when the highlight changed.
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
        // The euid shown matches the runtime path baked into the hook command,
        // and is already exposed by `id -u`. A placeholder would mislead.
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
        let block = super::toned_dialog_block(" Agent Status Hooks ", theme.accent, theme.accent);
        let (_, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // header
                Constraint::Min(1),    // content
                Constraint::Length(2), // buttons
            ])
            .split(inner);

        let header = Paragraph::new(
            "AoE needs to install hooks into your agent's settings\nto detect session status (running/waiting/idle).",
        )
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: true });
        frame.render_widget(header, chunks[0]);

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
    use crate::tui::dialogs::test_keys::key;
    use tempfile::TempDir;

    fn content_text(dialog: &HooksInstallDialog) -> String {
        dialog
            .build_content_lines()
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_accept_and_cancel_keys_decide_the_dialog() {
        assert!(
            HooksInstallDialog::new("claude").selected,
            "Accept is focused by default"
        );
        assert!(matches!(
            HooksInstallDialog::new("claude").handle_key(key(KeyCode::Char('y'))),
            DialogResult::Submit(true)
        ));
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            assert!(
                matches!(
                    HooksInstallDialog::new("claude").handle_key(key(code)),
                    DialogResult::Cancel
                ),
                "{code:?}"
            );
        }

        // Tab flips which button Enter takes.
        let mut dialog = HooksInstallDialog::new("claude");
        for (selected, submits) in [(false, false), (true, true)] {
            dialog.handle_key(key(KeyCode::Tab));
            assert_eq!(dialog.selected, selected);
            let mut probe = HooksInstallDialog::new("claude");
            probe.selected = selected;
            assert_eq!(
                matches!(
                    probe.handle_key(key(KeyCode::Enter)),
                    DialogResult::Submit(true)
                ),
                submits
            );
        }
    }

    #[test]
    fn hover_highlights_a_button_without_changing_the_selection() {
        let mut dialog = HooksInstallDialog::new("claude");
        dialog.accept_button_area = Rect::new(2, 5, 12, 1);
        dialog.cancel_button_area = Rect::new(20, 5, 14, 1);
        for (col, want) in [
            (3, dialog.accept_button_area),
            (21, dialog.cancel_button_area),
        ] {
            assert!(dialog.handle_hover(col, 5));
            assert_eq!(dialog.hover.current(), Some(want));
            assert!(dialog.selected, "hover must not flip the selection");
        }
        assert!(dialog.handle_hover(0, 0));
        assert_eq!(dialog.hover.current(), None);
    }

    #[test]
    #[serial_test::serial]
    fn the_disclosure_names_the_files_and_events_it_will_touch() {
        // The dialog follows the resolved config root, so a developer with
        // `CLAUDE_CONFIG_DIR` exported would otherwise see their own path.
        let _overrides = EnvGuard::unset(&["CLAUDE_CONFIG_DIR", "CODEX_HOME"]);

        let claude = content_text(&HooksInstallDialog::new("claude"));
        assert!(claude.contains(".claude/settings.json"));
        for event in ["PreToolUse", "Stop", "Notification"] {
            assert!(claude.contains(event), "{event} missing from {claude}");
        }
        // The example command must use the per-user path, not the legacy
        // multi-tenant one or a bogus placeholder.
        assert!(claude.contains(&format!(
            "{}/$AOE_INSTANCE_ID/status",
            crate::hooks::hook_base_path().display()
        )));
        for legacy in [
            "/tmp/aoe-hooks/$ID/",
            "/tmp/aoe-hooks/$AOE_INSTANCE_ID/status",
        ] {
            assert!(!claude.contains(legacy), "{legacy} still referenced");
        }
        // The codex trust note belongs to codex alone.
        assert!(!claude.contains("trust these hooks in /hooks"));
        assert!(!claude.contains("pane-based status detection"));

        assert!(content_text(&HooksInstallDialog::new("cursor")).contains(".cursor/hooks.json"));

        let codex = content_text(&HooksInstallDialog::new("codex"));
        assert!(codex.contains(".codex/hooks.json"));
        assert!(!codex.contains(".codex/config.toml"));
        assert!(codex.contains("trust these hooks in /hooks"));
        assert!(codex.contains("pane-based status detection"));
    }

    #[test]
    #[serial_test::serial]
    fn codex_home_moves_the_hooks_file_without_dragging_the_config_along() {
        let tmp = TempDir::new().unwrap();
        let _guard = EnvGuard::set(&[("CODEX_HOME", tmp.path())]);
        let text = content_text(&HooksInstallDialog::new("codex"));
        assert!(text.contains(&tmp.path().join("hooks.json").display().to_string()));
        assert!(!text.contains(&tmp.path().join("config.toml").display().to_string()));
    }

    /// Write `contents` as the profile's `config.toml` and return its dir.
    fn write_profile(profile: &str, contents: String) {
        let dir = crate::session::get_profile_dir(profile).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), contents).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn a_profile_s_environment_and_declared_roots_move_the_disclosed_paths() {
        // `isolate_home` restores HOME/XDG on Drop and holds the shared env
        // lock; the `EnvGuard`s are same-thread re-entrant acquisitions.
        let temp = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());
        let _overrides = EnvGuard::unset(&["CODEX_HOME", "CLAUDE_CONFIG_DIR"]);

        // A profile that redirects HOME moves both agents' files with it.
        let profile_home = temp.path().join("profile-home");
        let _environment = EnvGuard::set(&[("AOE_TEST_DIALOG_HOME", profile_home.as_os_str())]);
        write_profile(
            "profile-home",
            "environment = [\"HOME=$AOE_TEST_DIALOG_HOME\"]\n".to_string(),
        );
        for (agent, relative) in [
            ("claude", ".claude/settings.json"),
            ("codex", ".codex/hooks.json"),
        ] {
            let dialog = HooksInstallDialog::new_for_profile(agent, Some("profile-home"));
            assert_eq!(
                dialog.settings_paths,
                vec![profile_home.join(relative).to_string_lossy()],
                "{agent}"
            );
        }

        // A declared `agent_config_dir` wins outright.
        let claude_root = temp.path().join("claude-custom");
        let codex_root = temp.path().join("codex-custom");
        write_profile(
            "declared-hook-roots",
            format!(
                "[session.agent_config_dir]\nclaude = \"{}\"\ncodex = \"{}\"\n",
                claude_root.display(),
                codex_root.display()
            ),
        );
        for (agent, root, file) in [
            ("claude", &claude_root, "settings.json"),
            ("codex", &codex_root, "hooks.json"),
        ] {
            let dialog = HooksInstallDialog::new_for_profile(agent, Some("declared-hook-roots"));
            assert_eq!(
                dialog.settings_paths,
                vec![root.join(file).to_string_lossy()],
                "{agent}"
            );
        }

        // A CODEX_HOME set by the profile's own environment reaches the
        // hooks file but not the config.
        let codex_home = temp.path().join("profile-codex-home");
        write_profile(
            "codex-profile",
            format!("environment = [\"CODEX_HOME={}\"]\n", codex_home.display()),
        );
        let text = content_text(&HooksInstallDialog::new_for_profile(
            "codex",
            Some("codex-profile"),
        ));
        assert!(text.contains(&codex_home.join("hooks.json").display().to_string()));
        assert!(!text.contains(&codex_home.join("config.toml").display().to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn an_aliased_agent_resolves_through_the_shared_inherited_path_resolver() {
        let temp = TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let custom = temp.path().join("cursor-custom");
        let _cursor = EnvGuard::set(&[("CURSOR_CONFIG_DIR", custom.as_os_str())]);
        write_profile(
            "work",
            "environment = [\"CURSOR_CONFIG_DIR\"]\n\n[session.agent_detect_as]\ncorp-cursor = \"cursor\"\n"
                .to_string(),
        );

        let dialog = HooksInstallDialog::new_for_profile("corp-cursor", Some("work"));
        assert_eq!(
            dialog.settings_paths,
            vec![custom.join("hooks.json").to_string_lossy()]
        );
        assert!(!dialog.hook_commands.is_empty());
    }
}
