use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::NewSessionDialog;
use crate::tui::components::longest_common_prefix;

pub(in crate::tui::dialogs) struct PathGhostCompletion {
    pub(super) input_snapshot: String,
    pub(super) cursor_snapshot: usize,
    pub(super) ghost_text: String,
}

fn char_to_byte_idx(value: &str, char_idx: usize) -> usize {
    value
        .char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

/// Expand a leading `~` or `~/...` to the user's home directory.
/// Cross-platform via `dirs::home_dir()` (handles `USERPROFILE` on Windows).
///
/// Returns the input unchanged when the path does not start with `~`, or
/// when no home directory is available.
pub(super) fn expand_tilde(path: &str) -> String {
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().to_string();
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    path.to_string()
}

fn path_completion_base(parent_prefix: &str) -> Option<PathBuf> {
    if parent_prefix.is_empty() {
        return Some(PathBuf::from("."));
    }

    let trimmed = parent_prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return Some(PathBuf::from("/"));
    }

    if trimmed == "~" {
        return dirs::home_dir();
    }

    if let Some(stripped) = trimmed.strip_prefix("~/") {
        return dirs::home_dir().map(|home| home.join(stripped));
    }

    Some(PathBuf::from(trimmed))
}

/// Compute a path ghost completion for any Input field.
/// Returns None if no completion is available.
pub(super) fn compute_path_ghost(input: &Input) -> Option<PathGhostCompletion> {
    let value = input.value().to_string();
    let char_len = value.chars().count();
    let cursor_char = input.cursor().min(char_len);

    if cursor_char < char_len {
        return None;
    }

    let cursor_byte = char_to_byte_idx(&value, cursor_char);
    let segment_start = value[..cursor_byte].rfind('/').map_or(0, |idx| idx + 1);
    let parent_prefix = &value[..segment_start];
    let current_segment = &value[segment_start..cursor_byte];

    let base_dir = path_completion_base(parent_prefix)?;
    let include_hidden = current_segment.starts_with('.');
    let mut matches = Vec::new();
    let entries = std::fs::read_dir(&base_dir).ok()?;

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        if name.starts_with(current_segment) {
            matches.push(name.to_string());
        }
    }

    if matches.is_empty() {
        return None;
    }
    matches.sort();

    let ghost_text = if matches.len() == 1 {
        let remainder = &matches[0][current_segment.len()..];
        format!("{}/", remainder)
    } else {
        let common_prefix = longest_common_prefix(&matches);
        if common_prefix.len() > current_segment.len() {
            common_prefix[current_segment.len()..].to_string()
        } else {
            let remainder = &matches[0][current_segment.len()..];
            format!("{}/", remainder)
        }
    };

    if ghost_text.is_empty() {
        return None;
    }

    Some(PathGhostCompletion {
        input_snapshot: value,
        cursor_snapshot: cursor_char,
        ghost_text,
    })
}

impl NewSessionDialog {
    pub(super) fn handle_path_shortcuts(&mut self, key: KeyEvent) -> bool {
        if self.focused_field != self.path_field() {
            return false;
        }

        // Right arrow at end of input with ghost: accept ghost text
        if key.code == KeyCode::Right && key.modifiers == KeyModifiers::NONE {
            let cursor = self.path.cursor();
            let char_len = self.path.value().chars().count();
            if cursor >= char_len && self.path_ghost.is_some() {
                self.accept_path_ghost();
                return true;
            }
            return false;
        }

        // End key at end of input with ghost: accept ghost text
        if key.code == KeyCode::End && key.modifiers == KeyModifiers::NONE {
            let cursor = self.path.cursor();
            let char_len = self.path.value().chars().count();
            if cursor >= char_len && self.path_ghost.is_some() {
                self.accept_path_ghost();
                return true;
            }
            return false;
        }

        if matches!(key.code, KeyCode::Home)
            || (key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.move_path_cursor_to(0);
            self.error_message = None;
            self.path_invalid_flash_until = None;
            self.recompute_path_ghost();
            return true;
        }

        if (key.code == KeyCode::Left && key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::ALT))
        {
            self.move_path_cursor_to_previous_segment();
            self.error_message = None;
            self.path_invalid_flash_until = None;
            self.recompute_path_ghost();
            return true;
        }

        false
    }

    fn move_path_cursor_to(&mut self, target_char_idx: usize) {
        let char_len = self.path.value().chars().count();
        let target = target_char_idx.min(char_len);
        let current = self.path.cursor().min(char_len);

        if target < current {
            for _ in 0..(current - target) {
                self.path
                    .handle_event(&crossterm::event::Event::Key(KeyEvent::new(
                        KeyCode::Left,
                        KeyModifiers::NONE,
                    )));
            }
        } else if target > current {
            for _ in 0..(target - current) {
                self.path
                    .handle_event(&crossterm::event::Event::Key(KeyEvent::new(
                        KeyCode::Right,
                        KeyModifiers::NONE,
                    )));
            }
        }
    }

    fn move_path_cursor_to_previous_segment(&mut self) {
        let chars: Vec<char> = self.path.value().chars().collect();
        let mut cursor = self.path.cursor().min(chars.len());
        if cursor == 0 {
            return;
        }

        while cursor > 0 && chars[cursor - 1] == '/' {
            cursor -= 1;
        }
        while cursor > 0 && chars[cursor - 1] != '/' {
            cursor -= 1;
        }

        self.move_path_cursor_to(cursor);
    }

    fn set_path_value_with_cursor(&mut self, value: String, cursor_char_idx: usize) {
        self.path = Input::new(value);
        let total_chars = self.path.value().chars().count();
        let target = cursor_char_idx.min(total_chars);
        let left_steps = total_chars.saturating_sub(target);

        for _ in 0..left_steps {
            self.path
                .handle_event(&crossterm::event::Event::Key(KeyEvent::new(
                    KeyCode::Left,
                    KeyModifiers::NONE,
                )));
        }
    }

    pub(super) fn recompute_path_ghost(&mut self) {
        self.path_ghost = compute_path_ghost(&self.path);
    }

    pub(super) fn accept_path_ghost(&mut self) -> bool {
        let ghost = match self.path_ghost.take() {
            Some(g) => g,
            None => return false,
        };

        let value = self.path.value().to_string();
        let cursor_char = self.path.cursor().min(value.chars().count());

        // Staleness check
        if ghost.input_snapshot != value || ghost.cursor_snapshot != cursor_char {
            return false;
        }

        let mut new_value = value;
        new_value.push_str(&ghost.ghost_text);
        let new_cursor = new_value.chars().count();
        self.set_path_value_with_cursor(new_value, new_cursor);
        self.error_message = None;
        self.path_invalid_flash_until = None;
        self.recompute_path_ghost();
        true
    }

    pub(super) fn clear_path_ghost(&mut self) {
        self.path_ghost = None;
    }

    pub(super) fn ghost_text(&self) -> Option<&str> {
        self.path_ghost.as_ref().map(|g| g.ghost_text.as_str())
    }

    pub(super) fn is_path_invalid_flash_active(&self) -> bool {
        self.path_invalid_flash_until.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_tilde_passthrough_for_absolute_path() {
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn test_expand_tilde_unrecognized_dollar_prefix_passthrough() {
        // `$HOME` is intentionally not handled: path inputs use `~`, not
        // shell-style variable references.
        assert_eq!(expand_tilde("$HOME/foo"), "$HOME/foo");
    }
}
