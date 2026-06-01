//! Trust confirmation dialog for repository hooks

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::{repo_config, HooksConfig};
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

pub struct HookTrustDialog {
    hooks: HooksConfig,
    /// Final merged hook set (repo hooks overlaid on global/profile) shown to the
    /// user. `hooks` stays the repo-only set so the trust hash and post-approval
    /// merge keep operating on what the repo actually defines.
    merged_hooks: HooksConfig,
    hooks_hash: String,
    project_path: String,
    selected: bool, // true = Trust, false = Skip
    scroll_offset: u16,
    trust_button_area: Rect,
    skip_button_area: Rect,
    cancel_button_area: Rect,
    /// Which button the mouse is over, for the hover highlight. Visual
    /// only; never changes `selected`.
    hover: HoverState,
}

/// Result from the hook trust dialog.
pub enum HookTrustAction {
    /// User trusts the hooks; proceed with execution.
    Trust {
        hooks: HooksConfig,
        hooks_hash: String,
        project_path: String,
    },
    /// User chose to skip hooks but still create the session.
    Skip,
}

impl HookTrustDialog {
    pub fn new(
        hooks: HooksConfig,
        merged_hooks: HooksConfig,
        hooks_hash: String,
        project_path: String,
    ) -> Self {
        Self {
            hooks,
            merged_hooks,
            hooks_hash,
            project_path,
            selected: false,
            scroll_offset: 0,
            trust_button_area: Rect::default(),
            skip_button_area: Rect::default(),
            cancel_button_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    pub fn handle_click(&self, col: u16, row: u16) -> Option<DialogResult<HookTrustAction>> {
        let pos = ratatui::layout::Position::from((col, row));
        if self.trust_button_area.contains(pos) {
            return Some(DialogResult::Submit(HookTrustAction::Trust {
                hooks: self.hooks.clone(),
                hooks_hash: self.hooks_hash.clone(),
                project_path: self.project_path.clone(),
            }));
        }
        if self.skip_button_area.contains(pos) {
            return Some(DialogResult::Submit(HookTrustAction::Skip));
        }
        if self.cancel_button_area.contains(pos) {
            return Some(DialogResult::Cancel);
        }
        None
    }

    /// Highlight the button under the cursor without changing the
    /// Trust/Skip selection. See `ConfirmDialog::handle_hover` for the
    /// rationale. Returns `true` when the highlighted button changed.
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

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<HookTrustAction> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Char('n') | KeyCode::Char('N') => DialogResult::Submit(HookTrustAction::Skip),
            KeyCode::Enter => {
                if self.selected {
                    DialogResult::Submit(HookTrustAction::Trust {
                        hooks: self.hooks.clone(),
                        hooks_hash: self.hooks_hash.clone(),
                        project_path: self.project_path.clone(),
                    })
                } else {
                    DialogResult::Submit(HookTrustAction::Skip)
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                DialogResult::Submit(HookTrustAction::Trust {
                    hooks: self.hooks.clone(),
                    hooks_hash: self.hooks_hash.clone(),
                    project_path: self.project_path.clone(),
                })
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
                let total_lines = self.build_hook_lines().len() as u16;
                if self.scroll_offset + 1 < total_lines {
                    self.scroll_offset += 1;
                }
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn build_hook_lines(&self) -> Vec<Line<'_>> {
        // Render the merged set (what actually runs) with per-type source labels,
        // sharing the grouping logic with the CLI trust prompt.
        let mut lines = Vec::new();
        for group in repo_config::hook_display_groups(&self.merged_hooks, &self.hooks, true) {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(vec![
                Span::styled(format!("{}:", group.name), Style::default().bold()),
                Span::styled(group.source_label(), Style::default().dim()),
            ]));
            for cmd in &group.commands {
                lines.push(Line::from(format!("  {}", cmd)));
            }
        }

        lines
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let hook_lines = self.build_hook_lines();
        let content_height = hook_lines.len() as u16 + 4; // +4 for header, spacing, buttons

        let dialog_width = 60.min(area.width.saturating_sub(4));
        let dialog_height = (content_height + 6).min(area.height.saturating_sub(4));
        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Repository Hooks ")
            .title_style(Style::default().fg(theme.accent).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // header
                Constraint::Min(1),    // hook commands
                Constraint::Length(2), // buttons
            ])
            .split(inner);

        // Header
        let header = Paragraph::new(
            "This repo defines hooks in .agent-of-empires/config.toml.\nThese commands will run (repo overrides global per type). Allow them?",
        )
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: true });
        frame.render_widget(header, chunks[0]);

        // Hook commands (scrollable)
        let visible_lines: Vec<Line> = hook_lines
            .into_iter()
            .skip(self.scroll_offset as usize)
            .collect();
        let hooks_paragraph = Paragraph::new(visible_lines)
            .style(Style::default().fg(theme.dimmed))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(hooks_paragraph, chunks[1]);

        // Buttons
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

        let trust_label = "[Trust & Run (y)]";
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
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn test_dialog() -> HookTrustDialog {
        let repo = HooksConfig {
            on_create: vec!["npm install".to_string()],
            on_launch: vec!["echo start".to_string()],
            ..Default::default()
        };
        HookTrustDialog::new(
            repo.clone(),
            repo,
            "abc123".to_string(),
            "/home/user/project".to_string(),
        )
    }

    #[test]
    fn test_default_selection_is_skip() {
        let dialog = test_dialog();
        assert!(!dialog.selected);
    }

    #[test]
    fn test_y_trusts() {
        let mut dialog = test_dialog();
        let result = dialog.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(
            result,
            DialogResult::Submit(HookTrustAction::Trust { .. })
        ));
    }

    #[test]
    fn test_n_skips() {
        let mut dialog = test_dialog();
        let result = dialog.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            result,
            DialogResult::Submit(HookTrustAction::Skip)
        ));
    }

    #[test]
    fn test_esc_cancels() {
        let mut dialog = test_dialog();
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_with_trust_selected() {
        let mut dialog = test_dialog();
        dialog.selected = true;
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(HookTrustAction::Trust { .. })
        ));
    }

    #[test]
    fn test_enter_with_skip_selected() {
        let mut dialog = test_dialog();
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(HookTrustAction::Skip)
        ));
    }

    #[test]
    fn test_tab_toggles() {
        let mut dialog = test_dialog();
        assert!(!dialog.selected);
        dialog.handle_key(key(KeyCode::Tab));
        assert!(dialog.selected);
        dialog.handle_key(key(KeyCode::Tab));
        assert!(!dialog.selected);
    }

    #[test]
    fn hover_highlights_button_without_changing_selection() {
        // Stage button rects manually; the real ones come from render().
        let mut dialog = test_dialog();
        dialog.trust_button_area = Rect::new(2, 5, 17, 1);
        dialog.skip_button_area = Rect::new(23, 5, 10, 1);
        dialog.cancel_button_area = Rect::new(37, 5, 14, 1);
        assert!(!dialog.selected);

        // Over Trust: highlight it, selection unchanged.
        assert!(dialog.handle_hover(3, 5));
        assert_eq!(dialog.hover.current(), Some(dialog.trust_button_area));
        assert!(
            !dialog.selected,
            "hover must not flip the Trust/Skip selection"
        );

        // Cancel is clickable, so it highlights too.
        assert!(dialog.handle_hover(38, 5));
        assert_eq!(dialog.hover.current(), Some(dialog.cancel_button_area));

        // Off all buttons clears the highlight.
        assert!(dialog.handle_hover(0, 0));
        assert_eq!(dialog.hover.current(), None);
    }

    #[test]
    fn test_empty_hooks_dialog() {
        let dialog = HookTrustDialog::new(
            HooksConfig::default(),
            HooksConfig::default(),
            "empty_hash".to_string(),
            "/some/path".to_string(),
        );
        // Should build with no lines
        let lines = dialog.build_hook_lines();
        assert!(lines.is_empty());
    }

    fn lines_text(dialog: &HookTrustDialog) -> String {
        dialog
            .build_hook_lines()
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
    fn test_merged_display_shows_global_and_repo_sources() {
        // Repo overrides on_create; on_launch comes only from global config.
        let repo = HooksConfig {
            on_create: vec!["repo-create".to_string()],
            ..Default::default()
        };
        let merged = HooksConfig {
            on_create: vec!["repo-create".to_string()],
            on_launch: vec!["global-launch".to_string()],
            ..Default::default()
        };
        let dialog = HookTrustDialog::new(
            repo,
            merged,
            "hash".to_string(),
            "/home/user/project".to_string(),
        );
        let text = lines_text(&dialog);
        assert!(text.contains("on_create:"), "missing on_create: {}", text);
        assert!(
            text.contains("(from repo)"),
            "missing repo source: {}",
            text
        );
        assert!(text.contains("repo-create"), "missing repo cmd: {}", text);
        assert!(
            text.contains("on_launch:"),
            "merged global on_launch should show: {}",
            text
        );
        assert!(
            text.contains("(from global config)"),
            "missing global source: {}",
            text
        );
        assert!(
            text.contains("global-launch"),
            "missing global cmd: {}",
            text
        );
    }

    #[test]
    fn test_merged_display_renders_on_destroy_with_source() {
        // on_create falls through to global; on_destroy is repo-defined. Both must
        // render with the correct source label, exercising the on_destroy path.
        let repo = HooksConfig {
            on_destroy: vec!["repo-destroy".to_string()],
            ..Default::default()
        };
        let merged = HooksConfig {
            on_create: vec!["global-create".to_string()],
            on_destroy: vec!["repo-destroy".to_string()],
            ..Default::default()
        };
        let dialog = HookTrustDialog::new(
            repo,
            merged,
            "hash".to_string(),
            "/home/user/project".to_string(),
        );
        let text = lines_text(&dialog);
        assert!(text.contains("on_create:"), "missing on_create: {}", text);
        assert!(
            text.contains("global-create"),
            "missing global cmd: {}",
            text
        );
        assert!(text.contains("on_destroy:"), "missing on_destroy: {}", text);
        assert!(
            text.contains("repo-destroy"),
            "missing destroy cmd: {}",
            text
        );
        assert!(
            text.contains("(from global config)"),
            "on_create should be labeled global: {}",
            text
        );
        assert!(
            text.contains("(from repo)"),
            "on_destroy should be labeled repo: {}",
            text
        );
    }
}
