//! Tool picker dialog: quick list of configured tool sessions.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
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
    background: bool,
}

impl ToolPickerDialog {
    pub fn new(tools: &std::collections::HashMap<String, ToolSessionConfig>) -> Self {
        let mut items: Vec<ToolPickerEntry> = tools
            .iter()
            .map(|(name, config)| ToolPickerEntry {
                name: name.clone(),
                command: config.command.clone(),
                hotkey: config.hotkey.clone().unwrap_or_default(),
                background: config.background,
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

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<String> {
        if !super::contains(self.dialog_area, col, row) {
            return DialogResult::Cancel;
        }
        let Some(idx) = super::row_index(self.list_area, col, row, self.items.len()) else {
            return DialogResult::Continue;
        };
        self.cursor = idx;
        DialogResult::Submit(self.items[idx].name.clone())
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let hovered = super::row_index(self.list_area, col, row, self.items.len());
        super::hover_select(&mut self.cursor, hovered)
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
            code => {
                super::navigate_list(&mut self.cursor, self.items.len(), code);
                DialogResult::Continue
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let width = 50u16.min(area.width.saturating_sub(4));
        let height = (self.items.len() as u16 + 3).min(area.height.saturating_sub(4));
        let block = Block::default()
            .title(" Tool Sessions ")
            .title_style(Style::default().fg(theme.title))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.background));
        let (dialog_area, inner) = super::render_dialog_frame(frame, area, width, height, block);
        self.dialog_area = dialog_area;

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
                let background_part = if entry.background { "  [bg]" } else { "" };
                let line = Line::from(vec![
                    Span::styled(
                        &entry.name,
                        Style::default().fg(if i == self.cursor {
                            theme.accent
                        } else {
                            theme.text
                        }),
                    ),
                    Span::styled(background_part, Style::default().fg(theme.hint)),
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

        let footer = super::hint_line(
            theme,
            &[("↑↓", "navigate"), ("Enter", "open"), ("Esc", "close")],
        );
        frame.render_widget(Paragraph::new(footer), footer_area);
    }
}
