//! Approval dialog for the first agent hook install.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::path::PathBuf;

use super::DialogResult;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

pub struct HooksInstallDialog {
    status_hooks_enabled: bool,
    settings_paths: Vec<String>,
    hook_commands: Vec<(String, String)>,
    needs_codex_trust_note: bool,
    disabled_by_agent: Option<PathBuf>,
    extra_settings_paths: Vec<(String, String)>,
    selected: bool, // true = Accept, false = Cancel
    scroll_offset: u16,
    accept_button_area: Rect,
    cancel_button_area: Rect,
    /// The hovered button. Visual only; never changes `selected`.
    hover: HoverState,
}

impl HooksInstallDialog {
    /// `agent` is the one the gate resolved for this session, and `config` the
    /// one the gate read, so the dialog describes the same install the launch
    /// would install rather than a second derivation of it.
    pub fn new(
        tool_name: &str,
        agent: &'static crate::agents::AgentDef,
        config: &crate::session::config::Config,
    ) -> Self {
        let disclosure = crate::session::host_hook_disclosure(tool_name, agent, config);
        Self {
            settings_paths: disclosure.settings_paths,
            hook_commands: disclosure.hook_commands,
            needs_codex_trust_note: disclosure.needs_codex_trust_note,
            disabled_by_agent: disclosure.disabled_by_agent,
            status_hooks_enabled: disclosure.status_hooks_enabled,
            extra_settings_paths: disclosure.extra_settings_paths,
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
            "Files AoE targets:",
            Style::default().bold(),
        )));
        for path in &self.settings_paths {
            lines.push(Line::from(format!("  {path}")));
        }
        for (label, path) in &self.extra_settings_paths {
            lines.push(Line::from(format!("  {path}")));
            lines.push(Line::from(format!("    ({label})")));
        }
        if let Some(config) = &self.disabled_by_agent {
            lines.push(Line::from(format!(
                "  (this agent's own config turns its hooks off: {})",
                config.display()
            )));
        }

        if !self.hook_commands.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Hook events added:",
                Style::default().bold(),
            )));
            for (event, effect) in &self.hook_commands {
                lines.push(Line::from(format!("  {event} -> {effect}")));
            }
        }

        lines.push(Line::from(""));
        if self.status_hooks_enabled {
            lines.push(Line::from(Span::styled(
                "A status event runs:",
                Style::default().bold(),
            )));
            // The path is the runtime one baked into the hook command,
            // and a placeholder would mislead.
            lines.push(Line::from(format!(
                "  printf {{status}} > {}/$AOE_INSTANCE_ID/status",
                crate::hooks::hook_base_path().display()
            )));
        } else if self.hook_commands.is_empty() {
            lines.push(Line::from(
                "No status hook survives here, and this agent installs none:",
            ));
            lines.push(Line::from("its own config turns its hooks off."));
        } else {
            lines.push(Line::from(
                "No status hook survives here, so these only publish the id",
            ));
            lines.push(Line::from(
                "AoE resumes from. Each event's command is above.",
            ));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(
            "Hooks are guarded by $AOE_INSTANCE_ID and are a",
        ));
        lines.push(Line::from("no-op outside of AoE sessions."));
        lines.push(Line::from(""));
        lines.push(Line::from(
            "This is what the effective config resolves, not a manifest of",
        ));
        lines.push(Line::from(
            "every write a launch can make. A launch that routes through a",
        ));
        lines.push(Line::from(
            "native store, merges into a selected agent, or targets a",
        ));
        lines.push(Line::from(
            "selected or recorded Claude conversation store resolves that",
        ));
        lines.push(Line::from("target at launch time."));

        if self.needs_codex_trust_note && self.disabled_by_agent.is_none() {
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Codex may ask you to review and trust these hooks in /hooks.",
            ));
            if self.status_hooks_enabled {
                lines.push(Line::from(
                    "Until then, AoE falls back to pane-based status detection.",
                ));
            }
        }

        lines
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let content_lines = self.build_content_lines();
        // 8 rows of chrome: 2 block borders, 3 header, 2 buttons, 1 content top border.
        let content_height = content_lines.len() as u16 + 8;

        let dialog_width = 64.min(area.width.saturating_sub(4));
        let dialog_height = (content_height + 6).min(area.height.saturating_sub(4));
        let title = if self.status_hooks_enabled {
            " Agent Hooks "
        } else {
            " Agent Identity Hooks "
        };
        let block = super::toned_dialog_block(title, theme.accent, theme.accent);
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

        let header = Paragraph::new(if self.status_hooks_enabled {
            "AoE needs to install hooks into your agent's settings\nto detect session status (running/waiting/idle)."
        } else {
            "AoE needs to install identity hooks into your agent's settings\nfor native resume. No status hook survives for this config."
        })
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: true });
        frame.render_widget(header, chunks[0]);

        let visible_lines: Vec<Line> = content_lines
            .into_iter()
            .skip(self.scroll_offset as usize)
            .collect();
        let content_paragraph = Paragraph::new(visible_lines)
            .style(Style::default().fg(theme.dimmed))
            // A disclosed command is longer than the content width, so without
            // this the tail of every identity event is cut off.
            .wrap(Wrap { trim: true })
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

    /// Build the dialog the way the gate does: resolve the agent from the
    /// config, then describe it, so the tests stay on the production path
    /// instead of a second resolution.
    fn hook_dialog(tool_name: &str, profile: Option<&str>) -> HooksInstallDialog {
        let config = profile
            .map(crate::session::config::profile_config::resolve_config_or_warn)
            .unwrap_or_default();
        let agent = crate::session::host_hook_agent(
            tool_name,
            &config.session.launch_command_for(tool_name),
            &config.session,
        )
        .or_else(|| crate::agents::get_agent(tool_name))
        .unwrap_or_else(|| panic!("{tool_name} must name a known agent"));
        HooksInstallDialog::new(tool_name, agent, &config)
    }
    #[test]
    fn the_accept_and_cancel_keys_decide_the_dialog() {
        assert!(
            hook_dialog("claude", None).selected,
            "Accept is focused by default"
        );
        assert!(matches!(
            hook_dialog("claude", None).handle_key(key(KeyCode::Char('y'))),
            DialogResult::Submit(true)
        ));
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            assert!(
                matches!(
                    hook_dialog("claude", None).handle_key(key(code)),
                    DialogResult::Cancel
                ),
                "{code:?}"
            );
        }

        // Tab flips which button Enter takes.
        let mut dialog = hook_dialog("claude", None);
        for (selected, submits) in [(false, false), (true, true)] {
            dialog.handle_key(key(KeyCode::Tab));
            assert_eq!(dialog.selected, selected);
            let mut probe = hook_dialog("claude", None);
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
        let mut dialog = hook_dialog("claude", None);
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

        let claude = content_text(&hook_dialog("claude", None));
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

        assert!(content_text(&hook_dialog("cursor", None)).contains(".cursor/hooks.json"));

        let codex = content_text(&hook_dialog("codex", None));
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
        let text = content_text(&hook_dialog("codex", None));
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
            let dialog = hook_dialog(agent, Some("profile-home"));
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
            let dialog = hook_dialog(agent, Some("declared-hook-roots"));
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
        let text = content_text(&hook_dialog("codex", Some("codex-profile")));
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

        let dialog = hook_dialog("corp-cursor", Some("work"));
        assert_eq!(
            dialog.settings_paths,
            vec![custom.join("hooks.json").to_string_lossy()]
        );
        assert!(!dialog.hook_commands.is_empty());
    }
}
