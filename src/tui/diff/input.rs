//! Input handling for the diff view

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::DiffView;
use crate::tui::dialogs::DialogResult;

/// Result of handling a key event in the diff view
pub enum DiffAction {
    /// Continue showing the diff view
    Continue,
    /// Close the diff view
    Close,
    /// Launch external editor for a file
    EditFile(PathBuf),
}

impl DiffView {
    /// Handle a key event
    pub fn handle_key(&mut self, key: KeyEvent) -> DiffAction {
        // Handle warning dialog first (modal)
        if let Some(ref mut dialog) = self.warning_dialog {
            match dialog.handle_key(key) {
                DialogResult::Cancel | DialogResult::Submit(_) => {
                    self.warning_dialog = None;
                }
                DialogResult::Continue => {}
            }
            return DiffAction::Continue;
        }

        // Clear transient messages on any key
        self.success_message = None;

        // Handle help overlay
        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {
                    self.show_help = false;
                }
                _ => {}
            }
            return DiffAction::Continue;
        }

        // Handle branch selection dialog
        if self.branch_select.is_some() {
            return self.handle_branch_select_key(key);
        }

        // Normal diff view mode
        self.handle_normal_key(key)
    }

    /// Route a left-click. Currently only the file-list panel accepts
    /// click input (select the clicked file). Clicks elsewhere are
    /// swallowed by the modal but no-op.
    pub fn handle_click(&mut self, col: u16, row: u16) {
        let pos = ratatui::layout::Position::from((col, row));
        if self.file_list_inner.contains(pos) {
            let row_in_list = (row - self.file_list_inner.y) as usize;
            let file_index = self.file_list_scroll_offset + row_in_list;
            if file_index < self.files.len() && self.selected_file != file_index {
                self.selected_file = file_index;
                self.scroll_offset = 0;
            }
        }
    }

    /// Hover does not move the file-list selection. Otherwise pressing
    /// j/k after a stray mouse drift would jump to whichever file the
    /// cursor last crossed instead of advancing from the actually
    /// selected one. Click still selects.
    pub fn handle_hover(&mut self, _col: u16, _row: u16) -> bool {
        false
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> DiffAction {
        match (key.code, key.modifiers) {
            // Close view
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => DiffAction::Close,

            // File navigation (j/k always navigate between files)
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                self.prev_file();
                DiffAction::Continue
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                self.next_file();
                DiffAction::Continue
            }

            // Diff scrolling
            (KeyCode::PageUp, _) => {
                self.page_up();
                DiffAction::Continue
            }
            (KeyCode::PageDown, _) => {
                self.page_down();
                DiffAction::Continue
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.half_page_up();
                DiffAction::Continue
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.half_page_down();
                DiffAction::Continue
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
                self.scroll_offset = 0;
                DiffAction::Continue
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                self.scroll_offset = self.total_lines.saturating_sub(self.visible_lines);
                DiffAction::Continue
            }

            // Open external editor
            (KeyCode::Char('e'), _) | (KeyCode::Enter, _) => {
                if let Some(file) = self.selected_file() {
                    let full_path = self.repo_path.join(&file.path);
                    return DiffAction::EditFile(full_path);
                }
                DiffAction::Continue
            }

            // Branch selection
            (KeyCode::Char('b'), _) => {
                self.open_branch_select();
                DiffAction::Continue
            }

            // Refresh
            (KeyCode::Char('r'), _) => {
                if let Err(e) = self.refresh_files() {
                    self.error_message = Some(format!("Failed to refresh: {}", e));
                }
                DiffAction::Continue
            }

            // Copy the selected file's repo-relative path to the clipboard
            (KeyCode::Char('y'), _) => {
                self.copy_selected_path();
                DiffAction::Continue
            }

            // Toggle side-by-side (split) layout
            (KeyCode::Char('s'), _) => {
                self.split_view = !self.split_view;
                self.persist_split_view();
                DiffAction::Continue
            }

            // Resize file list panel
            (KeyCode::Char('h'), _) | (KeyCode::Left, _) => {
                self.shrink_file_list();
                DiffAction::Continue
            }
            (KeyCode::Char('l'), _) | (KeyCode::Right, _) => {
                self.grow_file_list();
                DiffAction::Continue
            }

            // Help
            (KeyCode::Char('?'), _) => {
                self.show_help = true;
                DiffAction::Continue
            }

            _ => DiffAction::Continue,
        }
    }

    fn handle_branch_select_key(&mut self, key: KeyEvent) -> DiffAction {
        let Some(state) = &mut self.branch_select else {
            return DiffAction::Continue;
        };

        match key.code {
            KeyCode::Esc => {
                self.branch_select = None;
            }
            KeyCode::Enter => {
                let branch = state.branches.get(state.selected).cloned();
                if let Some(branch) = branch {
                    self.select_branch(branch);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if state.selected > 0 => {
                state.selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j')
                if state.selected < state.branches.len().saturating_sub(1) =>
            {
                state.selected += 1;
            }
            _ => {}
        }
        DiffAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::InfoDialog;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_diff_view_with_warning() -> DiffView {
        let mut view = DiffView::test_default();
        view.warning_dialog = Some(InfoDialog::new("Warning", "Test warning"));
        view
    }

    fn make_diff_view_no_warning() -> DiffView {
        DiffView::test_default()
    }

    #[test]
    fn test_warning_dialog_blocks_normal_keys() {
        let mut view = make_diff_view_with_warning();
        // 'q' would normally close the view, but with warning dialog open it should not
        let action = view.handle_key(key(KeyCode::Char('q')));
        assert!(matches!(action, DiffAction::Continue));
        // Dialog should still be there (q doesn't dismiss InfoDialog)
        assert!(view.warning_dialog.is_some());
    }

    #[test]
    fn test_warning_dialog_dismissed_by_enter() {
        let mut view = make_diff_view_with_warning();
        let action = view.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, DiffAction::Continue));
        assert!(view.warning_dialog.is_none());
    }

    #[test]
    fn test_warning_dialog_dismissed_by_esc() {
        let mut view = make_diff_view_with_warning();
        let action = view.handle_key(key(KeyCode::Esc));
        assert!(matches!(action, DiffAction::Continue));
        assert!(view.warning_dialog.is_none());
    }

    #[test]
    fn test_warning_dialog_dismissed_by_space() {
        let mut view = make_diff_view_with_warning();
        let action = view.handle_key(key(KeyCode::Char(' ')));
        assert!(matches!(action, DiffAction::Continue));
        assert!(view.warning_dialog.is_none());
    }

    #[test]
    fn test_normal_keys_work_without_warning() {
        let mut view = make_diff_view_no_warning();
        // 'q' should close the view when no dialog
        let action = view.handle_key(key(KeyCode::Char('q')));
        assert!(matches!(action, DiffAction::Close));
    }

    fn diff_file(path: &str) -> crate::git::diff::DiffFile {
        crate::git::diff::DiffFile {
            path: std::path::PathBuf::from(path),
            old_path: None,
            status: crate::git::diff::FileStatus::Modified,
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn selected_path_string_returns_the_selected_file_path() {
        let mut view = make_diff_view_no_warning();
        view.files = vec![diff_file("src/app/foo.rs"), diff_file("README.md")];
        view.selected_file = 1;
        assert_eq!(view.selected_path_string().as_deref(), Some("README.md"));
    }

    #[test]
    fn selected_path_string_is_none_without_files() {
        let view = make_diff_view_no_warning();
        assert_eq!(view.selected_path_string(), None);
    }

    #[test]
    fn y_key_is_wired_and_continues() {
        // Empty file list: the handler short-circuits before touching the
        // clipboard, so this asserts the binding without a real clipboard write.
        let mut view = make_diff_view_no_warning();
        let action = view.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(action, DiffAction::Continue));
    }

    #[test]
    fn y_key_sets_copied_confirmation() {
        // Exercises the message path the empty-list test skips. This goes
        // through the best-effort clipboard helper: a no-op in CI without a
        // clipboard tool, and a local `cargo test` briefly writes the path to
        // the system clipboard.
        let mut view = make_diff_view_no_warning();
        view.files = vec![diff_file("src/app/foo.rs")];
        view.selected_file = 0;
        let action = view.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(action, DiffAction::Continue));
        assert_eq!(
            view.success_message.as_deref(),
            Some("Copied src/app/foo.rs")
        );
    }

    #[test]
    fn file_list_scroll_tracks_the_selected_file() {
        let mut view = make_diff_view_no_warning();
        view.files = (0..20)
            .map(|i| diff_file(&format!("src/file_{i}.rs")))
            .collect();

        view.selected_file = 0;
        view.ensure_selected_file_visible_in_list(5);
        assert_eq!(view.file_list_scroll_offset, 0);

        view.selected_file = 5;
        view.ensure_selected_file_visible_in_list(5);
        assert_eq!(view.file_list_scroll_offset, 1);

        view.selected_file = 19;
        view.ensure_selected_file_visible_in_list(5);
        assert_eq!(view.file_list_scroll_offset, 15);

        view.selected_file = 14;
        view.ensure_selected_file_visible_in_list(5);
        assert_eq!(view.file_list_scroll_offset, 14);
    }

    #[test]
    fn file_list_click_uses_the_visible_scroll_offset() {
        let mut view = make_diff_view_no_warning();
        view.files = (0..20)
            .map(|i| diff_file(&format!("src/file_{i}.rs")))
            .collect();
        view.file_list_scroll_offset = 10;
        view.file_list_inner = ratatui::layout::Rect::new(0, 5, 20, 5);

        view.handle_click(1, 7);

        assert_eq!(view.selected_file, 12);
        assert_eq!(view.scroll_offset, 0);
    }

    #[test]
    #[serial_test::serial]
    fn s_key_toggles_split_view() {
        // Restore HOME/XDG on drop (even on panic) so later serial tests don't
        // inherit a temp HOME pointing at a since-deleted directory.
        struct EnvGuard {
            home: Option<std::ffi::OsString>,
            xdg: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match self.xdg.take() {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
        let _env = EnvGuard {
            home: std::env::var_os("HOME"),
            xdg: std::env::var_os("XDG_CONFIG_HOME"),
        };

        let temp_home = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let mut view = make_diff_view_no_warning();
        let before = view.split_view;
        let action = view.handle_key(key(KeyCode::Char('s')));
        assert!(matches!(action, DiffAction::Continue));
        assert_eq!(view.split_view, !before);
    }
}
