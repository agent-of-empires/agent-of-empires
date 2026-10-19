//! Picker for configured tool sessions and daemon-provided plugin links.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::config::ToolSessionConfig;
use crate::tui::components::hint_buttons::HintButtons;
use crate::tui::styles::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPickerAction {
    Session(String),
    #[cfg(feature = "serve")]
    OpenPluginLink {
        command_id: String,
        session_id: String,
        href: String,
    },
}

pub struct ToolPickerDialog {
    items: Vec<ToolPickerEntry>,
    cursor: usize,
    dialog_area: Rect,
    list_area: Rect,
    footer: HintButtons,
}

struct ToolPickerEntry {
    name: String,
    command: String,
    hotkey: String,
    background: bool,
    action: ToolPickerAction,
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
                action: ToolPickerAction::Session(name.clone()),
            })
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            items,
            cursor: 0,
            dialog_area: Rect::default(),
            list_area: Rect::default(),
            footer: HintButtons::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(feature = "serve")]
    pub fn add_plugin_link(
        &mut self,
        name: String,
        command_id: String,
        session_id: String,
        href: String,
    ) {
        self.items.push(ToolPickerEntry {
            name,
            command: href.clone(),
            hotkey: String::new(),
            background: false,
            action: ToolPickerAction::OpenPluginLink {
                command_id,
                session_id,
                href,
            },
        });
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<ToolPickerAction> {
        if !super::contains(self.dialog_area, col, row) {
            return DialogResult::Cancel;
        }
        if let Some(key) = self.footer.key_at(col, row) {
            return self.handle_key(key);
        }
        let Some(idx) = super::row_index(self.list_area, col, row, self.items.len()) else {
            return DialogResult::Continue;
        };
        self.cursor = idx;
        DialogResult::Submit(self.items[idx].action.clone())
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let hovered = super::row_index(self.list_area, col, row, self.items.len());
        self.footer.handle_hover(col, row) | super::hover_select(&mut self.cursor, hovered)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<ToolPickerAction> {
        match key.code {
            KeyCode::Esc | KeyCode::Char(';') => DialogResult::Cancel,
            KeyCode::Enter => {
                if let Some(entry) = self.items.get(self.cursor) {
                    DialogResult::Submit(entry.action.clone())
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
            .title(" Tools ")
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

        self.footer.render(
            frame,
            footer_area,
            theme,
            &[
                ("↑↓", "navigate", KeyCode::Null),
                ("Enter", "open", KeyCode::Null),
                ("Esc", "close", KeyCode::Esc),
            ],
            Alignment::Left,
        );
    }
}

#[cfg(all(test, feature = "serve"))]
mod tests {
    use super::*;

    #[test]
    fn enter_selects_plugin_browser_resource() {
        let mut picker = ToolPickerDialog::new(&std::collections::HashMap::new());
        picker.add_plugin_link(
            "GitHub: #42".into(),
            "plugin.github.open_pr".into(),
            "session-1".into(),
            "https://github.com/org/repo/pull/42".into(),
        );

        let DialogResult::Submit(ToolPickerAction::OpenPluginLink {
            command_id,
            session_id,
            href,
        }) = picker.handle_key(KeyEvent::from(KeyCode::Enter))
        else {
            panic!("Enter did not select the plugin browser resource");
        };
        assert_eq!(command_id, "plugin.github.open_pr");
        assert_eq!(session_id, "session-1");
        assert_eq!(href, "https://github.com/org/repo/pull/42");
    }
}
