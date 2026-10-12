//! Per-session manual TODO list, shown as a panel over the preview and toggled
//! with `Ctrl+Y` from the terminal view (`Ctrl+T` is the strict-mode
//! quick-attach, so the panel takes the next free chord). The chord is listed in
//! the help overlay and in `docs/guides/live-mode.md`.
//!
//! The list is the user's own checklist for a session ("what's left / what's
//! done"), independent of anything the agent tracks. It is keyed by session id
//! and persisted to `<app_dir>/session-todos.json`, so it survives restarts and
//! is shared by every build pointed at the same config dir. Each edit is a
//! read-modify-write against that file, so two TUIs on one config dir do not
//! clobber each other's lists.
//!
//! Panel keys: `↑`/`↓` (or `j`/`k`) move, `space`/`Enter` toggle done, `a` adds
//! an item (type, `Enter` to confirm / `Esc` to cancel), `d` deletes, and `Esc`
//! or `Ctrl+Y` closes. The panel registers as an overlay (see
//! `has_non_live_send_overlay` / `has_dialog`), so it owns `q`, paste and the
//! live-send relay while it is open.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use fs2::FileExt;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use serde::{Deserialize, Serialize};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::tui::styles::Theme;

/// A stable id, so an edit finds the same item even if another TUI reordered
/// the list between the on-screen snapshot and the read-modify-write.
fn new_todo_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// One checklist entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TodoItem {
    /// Stable identity; older stores without one get a fresh id on load.
    #[serde(default = "new_todo_id")]
    pub(super) id: String,
    pub(super) text: String,
    #[serde(default)]
    pub(super) done: bool,
}

/// Open-panel state.
pub(super) struct TodoPanel {
    /// Session this panel edits, captured at open so a mouse click that moves
    /// `selected_session` underneath never redirects edits to another list.
    session: String,
    selected: usize,
    /// First item row shown, followed so the selection stays on screen.
    scroll: usize,
    /// `Some` while typing a new item; `None` in navigation mode.
    adding: Option<Input>,
}

impl TodoPanel {
    /// Whether the panel is in add-an-item (text entry) mode.
    pub(super) fn is_adding(&self) -> bool {
        self.adding.is_some()
    }
}

fn store_path() -> Option<PathBuf> {
    crate::session::get_app_dir()
        .ok()
        .map(|dir| dir.join("session-todos.json"))
}

/// Acquire an exclusive advisory lock over the whole load-edit-save transaction,
/// so two TUIs sharing one config dir serialize their edits instead of racing.
/// The returned file releases the lock when dropped. `None` leaves the caller to
/// proceed unserialized rather than block editing on a lock failure.
fn lock_store() -> Option<File> {
    let path = store_path()?.with_extension("lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    file.lock_exclusive().ok()?;
    Some(file)
}

/// Load the whole session→items map from disk. `Some(empty)` for a missing or
/// empty store; `Some(map)` on success; **`None`** when the store exists but
/// cannot be read or a malformed file cannot be quarantined — the caller must
/// then skip the edit rather than overwrite data it could not read.
pub(super) fn load_store() -> Option<HashMap<String, Vec<TodoItem>>> {
    let Some(path) = store_path() else {
        return Some(HashMap::new());
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Some(HashMap::new()),
        Err(err) => {
            tracing::warn!(target: "tui.home", "could not read {}: {err}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&raw) {
        Ok(map) => Some(map),
        Err(err) => {
            let aside = path.with_extension("corrupt");
            tracing::warn!(
                target: "tui.home",
                "session-todos.json is unreadable ({err}); moving it to {}",
                aside.display()
            );
            if std::fs::rename(&path, &aside).is_err() {
                return None;
            }
            Some(HashMap::new())
        }
    }
}

/// Persist the map by writing a uniquely named temp file and renaming it over the
/// target, so a crash mid-write cannot truncate the existing list and two writers
/// never share a temp path. Returns whether the write landed; failures are logged.
fn save_store(store: &HashMap<String, Vec<TodoItem>>) -> bool {
    let Some(path) = store_path() else {
        return false;
    };
    let json = match serde_json::to_string_pretty(store) {
        Ok(json) => json,
        Err(err) => {
            tracing::warn!(target: "tui.home", "could not serialize TODO store: {err}");
            return false;
        }
    };
    let tmp = path.with_file_name(format!("session-todos.{}.tmp", uuid::Uuid::new_v4()));
    if let Err(err) = std::fs::write(&tmp, &json) {
        tracing::warn!(target: "tui.home", "could not write {}: {err}", tmp.display());
        return false;
    }
    if let Err(err) = std::fs::rename(&tmp, &path) {
        tracing::warn!(target: "tui.home", "could not replace {}: {err}", path.display());
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
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
    /// session and refreshes the on-disk store so the newest items show.
    pub(super) fn toggle_todo_panel(&mut self) {
        if self.todo_panel.is_some() {
            self.todo_panel = None;
            return;
        }
        let Some(session) = self.selected_session.clone() else {
            return;
        };
        // Refresh from disk, but keep the in-memory list if the store is
        // unreadable rather than blanking it.
        if let Some(map) = load_store() {
            self.session_todos = map;
        }
        self.todo_panel = Some(TodoPanel {
            session,
            selected: 0,
            scroll: 0,
            adding: None,
        });
    }

    /// Number of items in the open panel's session.
    fn todo_len(&self) -> usize {
        self.todo_panel
            .as_ref()
            .and_then(|p| self.session_todos.get(&p.session))
            .map_or(0, Vec::len)
    }

    /// The id of the highlighted item, read from the on-screen snapshot before a
    /// read-modify-write so the edit targets the same item by identity.
    fn selected_todo_id(&self) -> Option<String> {
        let panel = self.todo_panel.as_ref()?;
        self.session_todos
            .get(&panel.session)?
            .get(panel.selected)
            .map(|item| item.id.clone())
    }

    /// Read-modify-write one edit against the on-disk store under an exclusive
    /// lock, then refresh the in-memory cache, so concurrent TUIs serialize and a
    /// store that cannot be read is left untouched rather than overwritten.
    fn edit_todos(&mut self, edit: impl FnOnce(&mut Vec<TodoItem>)) {
        let Some(session) = self.todo_panel.as_ref().map(|p| p.session.clone()) else {
            return;
        };
        let _lock = lock_store();
        let Some(mut map) = load_store() else {
            return;
        };
        edit(map.entry(session).or_default());
        if save_store(&map) {
            self.session_todos = map;
        }
    }

    /// Route a bracketed paste into the add field when the panel is adding.
    /// Returns `true` when consumed.
    pub(super) fn todo_panel_handle_paste(&mut self, text: &str) -> bool {
        if let Some(input) = self.todo_panel.as_mut().and_then(|p| p.adding.as_mut()) {
            let merged = format!("{}{text}", input.value());
            *input = Input::new(merged);
            true
        } else {
            false
        }
    }

    /// Handle a key while the panel is open. The panel owns the keyboard, so
    /// everything is consumed here.
    pub(super) fn handle_todo_key(&mut self, key: KeyEvent) {
        if self.todo_panel.is_none() {
            return;
        }
        let ctrl_y = key.code == KeyCode::Char('y') && key.modifiers == KeyModifiers::CONTROL;

        // Adding mode owns the keyboard: typing goes to the text field, Enter
        // commits, Esc / Ctrl+Y close.
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
                        self.edit_todos(|items| {
                            items.push(TodoItem {
                                id: new_todo_id(),
                                text,
                                done: false,
                            })
                        });
                        let last = self.todo_len().saturating_sub(1);
                        if let Some(panel) = self.todo_panel.as_mut() {
                            panel.selected = last;
                        }
                    }
                }
                KeyCode::Esc => {
                    if let Some(panel) = self.todo_panel.as_mut() {
                        panel.adding = None;
                    }
                }
                _ if ctrl_y => self.todo_panel = None,
                _ => {
                    if let Some(input) = self.todo_panel.as_mut().and_then(|p| p.adding.as_mut()) {
                        input.handle_event(&Event::Key(key));
                    }
                }
            }
            return;
        }

        let len = self.todo_len();
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => self.todo_panel = None,
            _ if ctrl_y => self.todo_panel = None,
            (KeyCode::Char('a'), m) if m.is_empty() => {
                if let Some(panel) = self.todo_panel.as_mut() {
                    panel.adding = Some(Input::default());
                }
            }
            (KeyCode::Down, m) | (KeyCode::Char('j'), m) if m.is_empty() && len > 0 => {
                if let Some(panel) = self.todo_panel.as_mut() {
                    panel.selected = (panel.selected + 1).min(len - 1);
                }
            }
            (KeyCode::Up, m) | (KeyCode::Char('k'), m) if m.is_empty() => {
                if let Some(panel) = self.todo_panel.as_mut() {
                    panel.selected = panel.selected.saturating_sub(1);
                }
            }
            (KeyCode::Char(' '), m) | (KeyCode::Enter, m) if m.is_empty() => {
                if let Some(id) = self.selected_todo_id() {
                    self.edit_todos(|items| {
                        if let Some(item) = items.iter_mut().find(|i| i.id == id) {
                            item.done = !item.done;
                        }
                    });
                }
            }
            (KeyCode::Char('d'), m) if m.is_empty() => {
                if let Some(id) = self.selected_todo_id() {
                    self.edit_todos(|items| items.retain(|i| i.id != id));
                }
                let new_len = self.todo_len();
                if let Some(panel) = self.todo_panel.as_mut() {
                    panel.selected = panel.selected.min(new_len.saturating_sub(1));
                }
            }
            _ => {}
        }
    }

    /// Render the TODO panel centered over `area` (the preview region), scrolling
    /// so the selection and the add field stay on screen.
    pub(super) fn render_todo_panel(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let Some(session) = self.todo_panel.as_ref().map(|p| p.session.clone()) else {
            return;
        };
        let len = self.session_todos.get(&session).map_or(0, Vec::len);
        let adding = self.todo_panel.as_ref().is_some_and(|p| p.adding.is_some());

        // Rows below the list: a blank spacer, the help line, and the add field
        // when active. The panel is capped to the area and scrolls inside.
        let footer_rows = 2 + u16::from(adding);
        let max_h = area.height.max(1);
        let desired = (len.min(u16::MAX as usize) as u16)
            .saturating_add(footer_rows)
            .saturating_add(2); // borders
        let height = desired.min(max_h).max(footer_rows + 2);
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

        let item_rows = inner.height.saturating_sub(footer_rows) as usize;

        // Follow the selection: keep it within [scroll, scroll + item_rows).
        if let Some(panel) = self.todo_panel.as_mut() {
            if panel.selected < panel.scroll {
                panel.scroll = panel.selected;
            } else if item_rows > 0 && panel.selected >= panel.scroll + item_rows {
                panel.scroll = panel.selected + 1 - item_rows;
            }
            if panel.scroll > len.saturating_sub(item_rows.max(1)) {
                panel.scroll = len.saturating_sub(item_rows.max(1));
            }
        }

        let panel = self.todo_panel.as_ref().expect("checked above");
        let items = self
            .session_todos
            .get(&session)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let mut lines: Vec<Line> = Vec::new();
        if items.is_empty() {
            lines.push(Line::from(Span::styled(
                "no items yet, press a to add",
                Style::default().fg(theme.dimmed),
            )));
        } else {
            let end = (panel.scroll + item_rows).min(items.len());
            for (i, item) in items.iter().enumerate().take(end).skip(panel.scroll) {
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
impl super::HomeView {
    pub(crate) fn todo_selected_for_test(&self) -> Option<usize> {
        self.todo_panel.as_ref().map(|p| p.selected)
    }

    pub(crate) fn set_todo_selected_for_test(&mut self, index: usize) {
        if let Some(panel) = self.todo_panel.as_mut() {
            panel.selected = index;
        }
    }

    pub(crate) fn seed_todos_for_test(&mut self, session: &str, texts: &[&str]) {
        let items = texts
            .iter()
            .map(|t| TodoItem {
                id: new_todo_id(),
                text: (*t).to_string(),
                done: false,
            })
            .collect();
        self.session_todos.insert(session.to_string(), items);
    }

    pub(crate) fn todo_items_for_test(&self, session: &str) -> Vec<(String, bool)> {
        self.session_todos
            .get(session)
            .map(|items| items.iter().map(|i| (i.text.clone(), i.done)).collect())
            .unwrap_or_default()
    }

    /// Write the on-disk store for `session` directly, simulating another TUI.
    /// Items are `(id, text, done)`.
    pub(crate) fn write_todo_store_for_test(&self, session: &str, items: &[(&str, &str, bool)]) {
        let mut map = load_store().unwrap_or_default();
        map.insert(
            session.to_string(),
            items
                .iter()
                .map(|(id, text, done)| TodoItem {
                    id: (*id).to_string(),
                    text: (*text).to_string(),
                    done: *done,
                })
                .collect(),
        );
        save_store(&map);
    }

    /// Read the on-disk store for `session` as `(text, done)` rows.
    pub(crate) fn read_todo_store_for_test(&self, session: &str) -> Vec<(String, bool)> {
        load_store()
            .unwrap_or_default()
            .get(session)
            .map(|items| items.iter().map(|i| (i.text.clone(), i.done)).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_item_round_trips_through_json() {
        let items = vec![
            TodoItem {
                id: new_todo_id(),
                text: "rebase on main".into(),
                done: true,
            },
            TodoItem {
                id: new_todo_id(),
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
    fn done_and_id_default_for_older_entries() {
        let back: Vec<TodoItem> = serde_json::from_str(r#"[{"text":"x"}]"#).unwrap();
        assert!(!back[0].done);
        assert!(!back[0].id.is_empty(), "a missing id is filled in on load");
    }
}
