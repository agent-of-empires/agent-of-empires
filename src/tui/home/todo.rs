//! Per-session manual TODO list, shown as a panel over the preview and toggled
//! with `Ctrl+Y` from the terminal view (`Ctrl+T` is the strict-mode
//! quick-attach, so the panel takes the next free chord).
//!
//! The list is the user's own checklist for a session ("what's left / what's
//! done"), independent of anything the agent tracks. It is keyed by session id
//! and persisted to `<app_dir>/session-todos.json`, so it survives restarts and
//! is shared by every aoe build pointed at the same config dir.
//!
//! Panel keys: `↑`/`↓` (or `j`/`k`) move, `space`/`Enter` toggle done, `a` adds
//! an item (type, `Enter` to confirm / `Esc` to cancel), `d` deletes, and `Esc`
//! or `Ctrl+Y` closes.

use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use serde::{Deserialize, Serialize};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::tui::styles::Theme;

/// One checklist entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TodoItem {
    pub(super) text: String,
    #[serde(default)]
    pub(super) done: bool,
}

/// Open-panel state: the highlighted row, and the text field while adding.
pub(super) struct TodoPanel {
    selected: usize,
    /// `Some` while typing a new item; `None` in navigation mode.
    adding: Option<Input>,
}

fn store_path() -> Option<PathBuf> {
    crate::session::get_app_dir()
        .ok()
        .map(|dir| dir.join("session-todos.json"))
}

/// Load the whole session→items map from disk, or an empty map on any error
/// (missing file, bad JSON): the TODO list is best-effort, never fatal.
pub(super) fn load_store() -> HashMap<String, Vec<TodoItem>> {
    let Some(path) = store_path() else {
        return HashMap::new();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_store(store: &HashMap<String, Vec<TodoItem>>) {
    let Some(path) = store_path() else {
        return;
    };
    if let Ok(json) = serde_json::to_string_pretty(store) {
        let _ = std::fs::write(path, json);
    }
}

/// A rect of size `w`×`h` centered in `area` (clamped to it).
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

impl super::HomeView {
    /// Toggle the TODO panel for the selected session. Opening needs a selected
    /// session; otherwise it is a no-op.
    pub(super) fn toggle_todo_panel(&mut self) {
        if self.todo_panel.is_some() {
            self.todo_panel = None;
            return;
        }
        if self.selected_session.is_none() {
            return;
        }
        self.todo_panel = Some(TodoPanel {
            selected: 0,
            adding: None,
        });
    }

    /// Handle a key while the panel is open. Returns `true` when the panel
    /// consumed the key (always, while open), so the caller stops routing it.
    pub(super) fn handle_todo_key(&mut self, key: KeyEvent) -> bool {
        if self.todo_panel.is_none() {
            return false;
        }
        let Some(session_id) = self.selected_session.clone() else {
            self.todo_panel = None;
            return true;
        };

        // Adding mode owns the keyboard: typing goes to the text field.
        if self.todo_panel.as_ref().is_some_and(|p| p.adding.is_some()) {
            match key.code {
                KeyCode::Enter => {
                    let text = self
                        .todo_panel
                        .as_mut()
                        .and_then(|p| p.adding.take())
                        .map(|input| input.value().trim().to_string())
                        .unwrap_or_default();
                    if !text.is_empty() {
                        let items = self.session_todos.entry(session_id).or_default();
                        items.push(TodoItem { text, done: false });
                        let last = items.len() - 1;
                        if let Some(panel) = self.todo_panel.as_mut() {
                            panel.selected = last;
                        }
                        self.save_todos();
                    }
                }
                KeyCode::Esc => {
                    if let Some(panel) = self.todo_panel.as_mut() {
                        panel.adding = None;
                    }
                }
                _ => {
                    if let Some(input) = self.todo_panel.as_mut().and_then(|p| p.adding.as_mut()) {
                        input.handle_event(&Event::Key(key));
                    }
                }
            }
            return true;
        }

        let len = self.session_todos.get(&session_id).map_or(0, Vec::len);
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => self.todo_panel = None,
            (KeyCode::Char('y'), m) if m.contains(KeyModifiers::CONTROL) => self.todo_panel = None,
            (KeyCode::Char('a'), _) => {
                if let Some(panel) = self.todo_panel.as_mut() {
                    panel.adding = Some(Input::default());
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) if len > 0 => {
                if let Some(panel) = self.todo_panel.as_mut() {
                    panel.selected = (panel.selected + 1).min(len - 1);
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                if let Some(panel) = self.todo_panel.as_mut() {
                    panel.selected = panel.selected.saturating_sub(1);
                }
            }
            (KeyCode::Char(' '), _) | (KeyCode::Enter, _) => {
                let sel = self.todo_panel.as_ref().map_or(0, |p| p.selected);
                if let Some(item) = self
                    .session_todos
                    .get_mut(&session_id)
                    .and_then(|items| items.get_mut(sel))
                {
                    item.done = !item.done;
                    self.save_todos();
                }
            }
            (KeyCode::Char('d'), _) => {
                let sel = self.todo_panel.as_ref().map_or(0, |p| p.selected);
                if let Some(items) = self.session_todos.get_mut(&session_id) {
                    if sel < items.len() {
                        items.remove(sel);
                    }
                    let new_len = items.len();
                    if let Some(panel) = self.todo_panel.as_mut() {
                        panel.selected = panel.selected.min(new_len.saturating_sub(1));
                    }
                    self.save_todos();
                }
            }
            _ => {}
        }
        true
    }

    fn save_todos(&self) {
        save_store(&self.session_todos);
    }

    /// Render the TODO panel centered over `area` (the preview region).
    pub(super) fn render_todo_panel(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let Some(panel) = self.todo_panel.as_ref() else {
            return;
        };
        let items = self
            .selected_session
            .as_ref()
            .and_then(|id| self.session_todos.get(id))
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let adding = panel.adding.is_some();
        // borders (2) + help row + optional input row, plus one row per item (at
        // least one so an empty list still shows a hint line).
        let body_rows = items.len().max(1) as u16 + u16::from(adding);
        let height = body_rows.saturating_add(4);
        let rect = centered(area, 60, height);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .padding(Padding::horizontal(1))
            .title(" TODO ");
        let inner = block.inner(rect);
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let mut lines: Vec<Line> = Vec::new();
        if items.is_empty() {
            lines.push(Line::from(Span::styled(
                "no items yet — press a to add",
                Style::default().fg(theme.dimmed),
            )));
        } else {
            for (i, item) in items.iter().enumerate() {
                let box_ = if item.done { "[x] " } else { "[ ] " };
                let mut style = if item.done {
                    Style::default()
                        .fg(theme.dimmed)
                        .add_modifier(Modifier::CROSSED_OUT)
                } else {
                    Style::default().fg(theme.text)
                };
                if i == panel.selected && !adding {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                lines.push(Line::from(Span::styled(
                    format!("{box_}{}", item.text),
                    style,
                )));
            }
        }
        if let Some(input) = panel.adding.as_ref() {
            lines.push(Line::from(vec![
                Span::styled("> ", Style::default().fg(theme.accent)),
                Span::styled(input.value().to_string(), Style::default().fg(theme.text)),
            ]));
        }
        lines.push(Line::from(""));
        let help = if adding {
            "Enter add · Esc cancel"
        } else {
            "a add · space done · d del · Esc close"
        };
        lines.push(Line::from(Span::styled(
            help,
            Style::default().fg(theme.dimmed),
        )));

        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_item_round_trips_through_json() {
        let items = vec![
            TodoItem {
                text: "rebase on main".into(),
                done: true,
            },
            TodoItem {
                text: "record a gif".into(),
                done: false,
            },
        ];
        let json = serde_json::to_string(&items).unwrap();
        let back: Vec<TodoItem> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
        assert!(back[0].done);
        assert_eq!(back[1].text, "record a gif");
    }

    #[test]
    fn done_defaults_to_false_for_older_entries() {
        let back: Vec<TodoItem> = serde_json::from_str(r#"[{"text":"x"}]"#).unwrap();
        assert!(!back[0].done);
    }
}
