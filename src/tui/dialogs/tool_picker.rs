//! Tool picker dialog: quick list of configured tool sessions.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::{centered_rect, DialogResult};
use crate::session::config::ToolSessionConfig;
use crate::tui::styles::Theme;

pub struct ToolPickerDialog {
    items: Vec<ToolPickerEntry>,
    cursor: usize,
    dialog_area: Rect,
    list_area: Rect,
}

struct ToolPickerEntry {
    name: String,
    command: String,
    hotkey: String,
}

impl ToolPickerDialog {
    pub fn new(tools: &std::collections::HashMap<String, ToolSessionConfig>) -> Self {
        let mut items: Vec<ToolPickerEntry> = tools
            .iter()
            .map(|(name, config)| ToolPickerEntry {
                name: name.clone(),
                command: config.command.clone(),
                hotkey: config.hotkey.clone().unwrap_or_default(),
            })
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            items,
            cursor: 0,
            dialog_area: Rect::default(),
            list_area: Rect::default(),
        }
    }

    fn row_to_idx(&self, col: u16, row: u16) -> Option<usize> {
        let pos = ratatui::layout::Position::from((col, row));
        if !self.list_area.contains(pos) {
            return None;
        }
        let row_in_list = (row - self.list_area.y) as usize;
        if row_in_list >= self.items.len() {
            return None;
        }
        Some(row_in_list)
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<String> {
        if !self
            .dialog_area
            .contains(ratatui::layout::Position::from((col, row)))
        {
            return DialogResult::Cancel;
        }
        let Some(idx) = self.row_to_idx(col, row) else {
            return DialogResult::Continue;
        };
        self.cursor = idx;
        DialogResult::Submit(self.items[idx].name.clone())
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let Some(idx) = self.row_to_idx(col, row) else {
            return false;
        };
        if self.cursor == idx {
            return false;
        }
        self.cursor = idx;
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<String> {
        match key.code {
            KeyCode::Esc | KeyCode::Char(';') => DialogResult::Cancel,
            KeyCode::Enter => {
                if let Some(entry) = self.items.get(self.cursor) {
                    DialogResult::Submit(entry.name.clone())
                } else {
                    DialogResult::Cancel
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.cursor + 1 < self.items.len() {
                    self.cursor += 1;
                }
                DialogResult::Continue
            }
            KeyCode::Home => {
                self.cursor = 0;
                DialogResult::Continue
            }
            KeyCode::End => {
                self.cursor = self.items.len().saturating_sub(1);
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let width = 50u16.min(area.width.saturating_sub(4));
        // +2 for borders, +1 for the footer hint row.
        let height = (self.items.len() as u16 + 3).min(area.height.saturating_sub(4));
        let dialog_area = centered_rect(area, width, height);
        self.dialog_area = dialog_area;

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .title(" Tool Sessions ")
            .title_style(Style::default().fg(theme.title))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.background));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        let list_area = chunks[0];
        let footer_area = chunks[1];
        self.list_area = list_area;

        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let hotkey_part = if entry.hotkey.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", entry.hotkey)
                };
                let line = Line::from(vec![
                    Span::styled(
                        &entry.name,
                        Style::default().fg(if i == self.cursor {
                            theme.accent
                        } else {
                            theme.text
                        }),
                    ),
                    Span::styled(
                        format!("  {}", entry.command),
                        Style::default().fg(theme.dimmed),
                    ),
                    Span::styled(hotkey_part, Style::default().fg(theme.hint)),
                ]);
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items).highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );

        let mut state = ListState::default();
        state.select(Some(self.cursor));
        frame.render_stateful_widget(list, list_area, &mut state);

        let footer = Line::from(vec![
            Span::styled("↑↓", Style::default().fg(theme.hint)),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" open  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" close"),
        ]);
        frame.render_widget(Paragraph::new(footer), footer_area);
    }
}
