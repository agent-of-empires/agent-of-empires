//! Trust confirmation dialog for repository hooks and project-local MCP servers.
//!
//! A repo's `.agent-of-empires/config.toml` hooks and its `.mcp.json` MCP
//! servers both run code on the user's behalf (a stdio MCP server launches its
//! `command` when a session spawns), so both sit behind one approval (#1985).
//! The dialog displays whichever surfaces are present, redacting MCP env and
//! header VALUES (names only), and records trust for each on approval.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::project_mcp::ProjectMcpServer;
use crate::session::{repo_config, HooksConfig};
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

pub struct RepoTrustDialog {
    /// Final merged hook set (repo hooks overlaid on global/profile) shown to
    /// the user. Empty when the repo defines no hooks.
    merged_hooks: HooksConfig,
    /// Repo-only hooks, used to label each displayed hook by source.
    repo_hooks: HooksConfig,
    /// Project MCP servers to display (redacted). Empty when no `.mcp.json`.
    mcp_servers: Vec<ProjectMcpServer>,
    /// Precomputed merged hooks to run if the user trusts.
    hooks_on_trust: Option<HooksConfig>,
    /// Precomputed merged hooks to run if the user skips (already-trusted hooks
    /// still run; newly-prompted hooks are dropped).
    hooks_on_skip: Option<HooksConfig>,
    /// Hashes to record on approval; `Some` only for a surface needing trust.
    hooks_hash: Option<String>,
    mcp_hash: Option<String>,
    project_path: String,
    selected: bool, // true = Trust, false = Skip
    scroll_offset: u16,
    trust_button_area: Rect,
    skip_button_area: Rect,
    cancel_button_area: Rect,
    /// Which button the mouse is over, for the hover highlight. Visual only;
    /// never changes `selected`.
    hover: HoverState,
}

/// Result from the repo trust dialog.
pub enum RepoTrustAction {
    /// User trusts the repo; record the surface hashes and run `hooks`.
    Trust {
        hooks_hash: Option<String>,
        mcp_hash: Option<String>,
        project_path: String,
        hooks: Option<HooksConfig>,
    },
    /// User declined; create the session running only `hooks` (the skip set).
    Skip { hooks: Option<HooksConfig> },
}

impl RepoTrustDialog {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        merged_hooks: HooksConfig,
        repo_hooks: HooksConfig,
        mcp_servers: Vec<ProjectMcpServer>,
        hooks_on_trust: Option<HooksConfig>,
        hooks_on_skip: Option<HooksConfig>,
        hooks_hash: Option<String>,
        mcp_hash: Option<String>,
        project_path: String,
    ) -> Self {
        Self {
            merged_hooks,
            repo_hooks,
            mcp_servers,
            hooks_on_trust,
            hooks_on_skip,
            hooks_hash,
            mcp_hash,
            project_path,
            selected: false,
            scroll_offset: 0,
            trust_button_area: Rect::default(),
            skip_button_area: Rect::default(),
            cancel_button_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    fn trust_action(&self) -> RepoTrustAction {
        RepoTrustAction::Trust {
            hooks_hash: self.hooks_hash.clone(),
            mcp_hash: self.mcp_hash.clone(),
            project_path: self.project_path.clone(),
            hooks: self.hooks_on_trust.clone(),
        }
    }

    fn skip_action(&self) -> RepoTrustAction {
        RepoTrustAction::Skip {
            hooks: self.hooks_on_skip.clone(),
        }
    }

    pub fn handle_click(&self, col: u16, row: u16) -> Option<DialogResult<RepoTrustAction>> {
        let pos = ratatui::layout::Position::from((col, row));
        if self.trust_button_area.contains(pos) {
            return Some(DialogResult::Submit(self.trust_action()));
        }
        if self.skip_button_area.contains(pos) {
            return Some(DialogResult::Submit(self.skip_action()));
        }
        if self.cancel_button_area.contains(pos) {
            return Some(DialogResult::Cancel);
        }
        None
    }

    /// Highlight the button under the cursor without changing the Trust/Skip
    /// selection. Returns `true` when the highlighted button changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.hover.update(
            col,
            row,
            &[
                self.trust_button_area,
                self.skip_button_area,
                self.cancel_button_area,
            ],
        )
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<RepoTrustAction> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Char('n') | KeyCode::Char('N') => DialogResult::Submit(self.skip_action()),
            KeyCode::Enter => {
                if self.selected {
                    DialogResult::Submit(self.trust_action())
                } else {
                    DialogResult::Submit(self.skip_action())
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => DialogResult::Submit(self.trust_action()),
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
                let total_lines = self.build_lines().len() as u16;
                if self.scroll_offset + 1 < total_lines {
                    self.scroll_offset += 1;
                }
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn build_lines(&self) -> Vec<Line<'_>> {
        let mut lines = Vec::new();

        // Hooks section: merged set with per-type source labels, sharing the
        // grouping logic with the CLI trust prompt.
        let groups = repo_config::hook_display_groups(&self.merged_hooks, &self.repo_hooks, true);
        if !groups.is_empty() {
            lines.push(Line::from(Span::styled(
                "Hooks",
                Style::default().bold().underlined(),
            )));
            for group in groups {
                lines.push(Line::from(vec![
                    Span::styled(format!("{}:", group.name), Style::default().bold()),
                    Span::styled(group.source_label(), Style::default().dim()),
                ]));
                for cmd in &group.commands {
                    lines.push(Line::from(format!("  {}", cmd)));
                }
            }
        }

        // Project MCP section: redacted (env / header VALUES never shown).
        if !self.mcp_servers.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                "Project MCP servers (.mcp.json, values redacted)",
                Style::default().bold().underlined(),
            )));
            for server in &self.mcp_servers {
                lines.push(Line::from(format!("  {}", server.redacted_summary())));
            }
        }

        lines
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let body_lines = self.build_lines();
        let content_height = body_lines.len() as u16 + 4;

        let dialog_width = 64.min(area.width.saturating_sub(4));
        let dialog_height = (content_height + 6).min(area.height.saturating_sub(4));
        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Repository Trust ")
            .title_style(Style::default().fg(theme.accent).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // header
                Constraint::Min(1),    // hooks + mcp
                Constraint::Length(2), // buttons
            ])
            .split(inner);

        let header = Paragraph::new(
            "This repo defines hooks and/or project MCP servers that run code on your behalf. Trust and use them?",
        )
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: true });
        frame.render_widget(header, chunks[0]);

        let visible_lines: Vec<Line> = body_lines
            .into_iter()
            .skip(self.scroll_offset as usize)
            .collect();
        let body_paragraph = Paragraph::new(visible_lines)
            .style(Style::default().fg(theme.dimmed))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(body_paragraph, chunks[1]);

        let trust_style = if self.selected {
            Style::default().fg(theme.running).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };
        let skip_style = if !self.selected {
            Style::default().fg(theme.accent).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };

        let trust_label = "[Trust & Use (y)]";
        let skip_label = "[Skip (n)]";
        let cancel_label = "[Cancel (Esc)]";
        let gap: u16 = 4;
        let prefix: u16 = 2;
        let trust_w = trust_label.chars().count() as u16;
        let skip_w = skip_label.chars().count() as u16;
        let cancel_w = cancel_label.chars().count() as u16;
        let total = prefix + trust_w + gap + skip_w + gap + cancel_w;
        let button_area = chunks[2];
        if button_area.width >= total {
            let left_pad = (button_area.width - total) / 2;
            let trust_x = button_area.x + left_pad + prefix;
            let skip_x = trust_x + trust_w + gap;
            let cancel_x = skip_x + skip_w + gap;
            self.trust_button_area = Rect::new(trust_x, button_area.y, trust_w, 1);
            self.skip_button_area = Rect::new(skip_x, button_area.y, skip_w, 1);
            self.cancel_button_area = Rect::new(cancel_x, button_area.y, cancel_w, 1);
        } else {
            self.trust_button_area = Rect::default();
            self.skip_button_area = Rect::default();
            self.cancel_button_area = Rect::default();
        }

        let buttons = Line::from(vec![
            Span::raw("  "),
            Span::styled(trust_label, trust_style),
            Span::raw("    "),
            Span::styled(skip_label, skip_style),
            Span::raw("    "),
            Span::styled(cancel_label, Style::default().fg(theme.dimmed)),
        ]);

        frame.render_widget(
            Paragraph::new(buttons).alignment(Alignment::Center),
            button_area,
        );

        if let Some(rect) = self.hover.current_in(&[
            self.trust_button_area,
            self.skip_button_area,
            self.cancel_button_area,
        ]) {
            paint_hover_bg(frame, rect, theme.selection);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::project_mcp::load_project_mcp_servers;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn dialog_with(hooks: HooksConfig, mcp: Vec<ProjectMcpServer>) -> RepoTrustDialog {
        RepoTrustDialog::new(
            hooks.clone(),
            hooks,
            mcp,
            None,
            None,
            Some("hh".to_string()),
            Some("mh".to_string()),
            "/home/user/project".to_string(),
        )
    }

    fn sample_mcp() -> Vec<ProjectMcpServer> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{ "mcpServers": { "fs": { "command": "mcp-fs", "env": { "TOKEN": "supersecret" } } } }"#,
        )
        .unwrap();
        load_project_mcp_servers(dir.path()).unwrap()
    }

    #[test]
    fn y_trusts_with_both_hashes() {
        let mut dialog = dialog_with(HooksConfig::default(), sample_mcp());
        match dialog.handle_key(key(KeyCode::Char('y'))) {
            DialogResult::Submit(RepoTrustAction::Trust {
                hooks_hash,
                mcp_hash,
                ..
            }) => {
                assert_eq!(hooks_hash.as_deref(), Some("hh"));
                assert_eq!(mcp_hash.as_deref(), Some("mh"));
            }
            _ => panic!("expected Trust"),
        }
    }

    #[test]
    fn n_skips() {
        let mut dialog = dialog_with(HooksConfig::default(), sample_mcp());
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Char('n'))),
            DialogResult::Submit(RepoTrustAction::Skip { .. })
        ));
    }

    #[test]
    fn esc_cancels() {
        let mut dialog = dialog_with(HooksConfig::default(), sample_mcp());
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    fn lines_text(dialog: &RepoTrustDialog) -> String {
        dialog
            .build_lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn mcp_section_redacts_values_shows_names() {
        let dialog = dialog_with(HooksConfig::default(), sample_mcp());
        let text = lines_text(&dialog);
        assert!(
            text.contains("Project MCP servers"),
            "missing mcp header: {text}"
        );
        assert!(
            text.contains("mcp-fs") && text.contains("TOKEN"),
            "missing mcp detail: {text}"
        );
        assert!(!text.contains("supersecret"), "env value leaked: {text}");
    }

    #[test]
    fn hooks_section_renders_when_present() {
        let hooks = HooksConfig {
            on_create: vec!["npm install".to_string()],
            ..Default::default()
        };
        let dialog = dialog_with(hooks, Vec::new());
        let text = lines_text(&dialog);
        assert!(text.contains("Hooks"), "missing hooks header: {text}");
        assert!(text.contains("on_create:"), "missing hook type: {text}");
        assert!(text.contains("npm install"), "missing hook cmd: {text}");
    }
}
