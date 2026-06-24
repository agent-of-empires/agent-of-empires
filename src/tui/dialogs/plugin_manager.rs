//! Plugin manager: list builtin plugins and enable/disable them from the TUI.
//! The TUI twin of `aoe plugin list` and the web Plugins tab. This is the
//! minimal core: a browse list with an enable/disable toggle. Install, update,
//! uninstall, discovery, and capability approval return in follow-up PRs.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::{centered_rect, DialogResult};
use crate::tui::styles::Theme;

pub struct PluginManagerDialog {
    /// The shared manager view-model, the same shape the web dashboard renders
    /// from (`crate::plugin::view`). Built straight off the registry, so the
    /// TUI never re-derives plugin fields.
    rows: Vec<crate::plugin::PluginView>,
    load_errors: Vec<String>,
    selected: usize,
    error: Option<String>,
    info: Option<String>,
    /// Set whenever the on-disk plugin config changed (enable/disable). An
    /// embedding surface drains it via [`take_mutated`] to re-sync its own
    /// config view; the standalone modal ignores it.
    mutated: bool,
    /// True when hosted inside the settings screen (vs the command-palette
    /// modal). Only changes the footer hint: Esc returns to the category list.
    embedded: bool,
}

impl Default for PluginManagerDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginManagerDialog {
    pub fn new() -> Self {
        let mut dialog = Self {
            rows: Vec::new(),
            load_errors: Vec::new(),
            selected: 0,
            error: None,
            info: None,
            mutated: false,
            embedded: false,
        };
        dialog.reload();
        dialog.mutated = false; // Initial load is not a user mutation.
        dialog
    }

    /// A manager hosted inside the settings screen rather than the command
    /// palette. Only the footer differs: Esc returns to the category list.
    pub fn embedded() -> Self {
        let mut dialog = Self::new();
        dialog.embedded = true;
        dialog
    }

    /// Take and clear the "config mutated" flag (enable/disable wrote to disk
    /// and reloaded the registry).
    pub fn take_mutated(&mut self) -> bool {
        std::mem::take(&mut self.mutated)
    }

    fn reload(&mut self) {
        // reload() runs only after a config-mutating action (and once at
        // construction), so it is the single place to flag a mutation.
        self.mutated = true;
        let registry = crate::plugin::reload_registry();
        self.rows = registry.all().iter().map(|p| p.view()).collect();
        self.load_errors = registry.load_errors().to_vec();
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        self.info = None;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.rows.is_empty() {
                    self.selected = (self.selected + 1).min(self.rows.len() - 1);
                }
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                DialogResult::Continue
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                if let Some(row) = self.rows.get(self.selected) {
                    let (id, enabled) = (row.id.clone(), row.enabled);
                    match crate::plugin::install::set_enabled(&id, !enabled) {
                        Ok(()) => {
                            self.info = Some(format!(
                                "{} {id}",
                                if enabled { "Disabled" } else { "Enabled" }
                            ));
                            self.error = None;
                            self.reload();
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    /// The currently selected plugin row, if any. Lets an embedding surface
    /// (the settings Plugins tab) read the selection.
    pub fn selected(&self) -> Option<&crate::plugin::PluginView> {
        self.rows.get(self.selected)
    }

    /// Reflect a staged enable/disable in the displayed list without touching
    /// disk or the registry. The settings host stages the change in its own
    /// config and persists it on save, so the row shows the pending state
    /// immediately while still following the normal save flow.
    pub fn set_row_enabled(&mut self, id: &str, enabled: bool) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
            row.enabled = enabled;
        }
    }

    /// Render as a centered modal (the command-palette surface): clears a
    /// clamped sub-rect and draws into it.
    pub fn render(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width.clamp(40, 100);
        let height = area.height.clamp(12, 28);
        let rect = centered_rect(area, width, height);
        f.render_widget(Clear, rect);
        // A modal always owns the keyboard, so its border is always accent.
        self.render_into(f, rect, theme, true);
    }

    /// Render directly into the given rect, no centering or clearing, for
    /// embedding in the settings screen's Plugins category. Same manager, same
    /// state, same key handler; only the framing differs. `focused` mirrors the
    /// settings fields-pane focus so the border matches every other pane.
    pub fn render_inline(&self, f: &mut Frame, area: Rect, theme: &Theme, focused: bool) {
        self.render_into(f, area, theme, focused);
    }

    fn render_into(&self, f: &mut Frame, rect: Rect, theme: &Theme, focused: bool) {
        // Focus-aware border, matching the settings fields pane: accent when
        // the pane holds the keyboard, dim border otherwise.
        let border_color = if focused { theme.accent } else { theme.border };
        let block = Block::default()
            .title(" Plugins ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.render_browse(f, inner, theme);
    }

    fn render_browse(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(if self.load_errors.is_empty() { 0 } else { 2 }),
                Constraint::Length(2),
            ])
            .split(area);

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| {
                let state = if row.enabled {
                    ("enabled", theme.running)
                } else {
                    ("disabled", theme.dimmed)
                };
                let spans = vec![
                    Span::styled(
                        format!("{:<28}", format!("{} v{}", row.name, row.version)),
                        Style::default().fg(theme.text),
                    ),
                    Span::styled(format!("{:<12}", state.0), Style::default().fg(state.1)),
                ];
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        let mut state = ListState::default();
        state.select(if self.rows.is_empty() {
            None
        } else {
            Some(self.selected)
        });
        f.render_stateful_widget(list, chunks[0], &mut state);

        if !self.load_errors.is_empty() {
            let errors = Paragraph::new(self.load_errors.join("; "))
                .style(Style::default().fg(theme.error))
                .wrap(Wrap { trim: true });
            f.render_widget(errors, chunks[1]);
        }

        let status = self
            .error
            .as_deref()
            .map(|e| (e, theme.error))
            .or(self.info.as_deref().map(|i| (i, theme.running)));
        let footer = match status {
            Some((message, color)) => Paragraph::new(message.to_string())
                .style(Style::default().fg(color))
                .wrap(Wrap { trim: true }),
            None => Paragraph::new(if self.embedded {
                "space/enter toggle · esc back"
            } else {
                "space/enter toggle · esc close"
            })
            .style(Style::default().fg(theme.dimmed)),
        };
        f.render_widget(footer, chunks[2]);
    }
}
