//! Group delete options dialog

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::checkbox::{checkbox_line, CheckboxStyle};
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

#[derive(Clone, Debug, Default)]
pub struct GroupDeleteOptions {
    pub delete_sessions: bool,
    pub delete_worktrees: bool,
    pub force_delete_worktrees: bool,
    pub delete_branches: bool,
    pub delete_containers: bool,
}

pub struct GroupDeleteOptionsDialog {
    group_path: String,
    session_count: usize,
    has_managed_worktrees: bool,
    has_containers: bool,
    options: GroupDeleteOptions,
    focused_field: usize,
    /// Captured rect per focusable field, populated by `render`.
    /// Drives both click (set focus + toggle) and the hover highlight.
    focusable_rects: Vec<(usize, Rect)>,
    /// Which field row the mouse is over, for the hover highlight.
    /// Visual only; never moves keyboard `focused_field`.
    hover: HoverState,
}

impl GroupDeleteOptionsDialog {
    pub fn new(
        group_path: String,
        session_count: usize,
        has_managed_worktrees: bool,
        has_containers: bool,
    ) -> Self {
        Self {
            group_path,
            session_count,
            has_managed_worktrees,
            has_containers,
            options: GroupDeleteOptions::default(),
            focused_field: 0,
            focusable_rects: Vec::new(),
            hover: HoverState::default(),
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<GroupDeleteOptions>> {
        let pos = ratatui::layout::Position::from((col, row));
        let hit = self
            .focusable_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(field, _)| *field)?;
        self.focused_field = hit;
        self.toggle_focused_field();
        Some(DialogResult::Continue)
    }

    /// Highlight the field row under the cursor without moving keyboard
    /// `focused_field`. See `ConfirmDialog::handle_hover` for the
    /// rationale; click still moves focus and toggles state. Returns
    /// `true` when the highlighted row changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let rects: Vec<Rect> = self.focusable_rects.iter().map(|(_, r)| *r).collect();
        self.hover.update(col, row, &rects)
    }

    /// Mirror the Space-key branch's per-field toggle / radio logic so a
    /// click produces byte-identical state changes.
    fn toggle_focused_field(&mut self) {
        match self.focused_field {
            0 => {
                self.options.delete_sessions = false;
                self.options.delete_worktrees = false;
                self.options.force_delete_worktrees = false;
                self.options.delete_branches = false;
                self.options.delete_containers = false;
            }
            1 => {
                self.options.delete_sessions = true;
            }
            f if Some(f) == self.worktree_field_index() => {
                self.options.delete_worktrees = !self.options.delete_worktrees;
                if !self.options.delete_worktrees {
                    self.options.force_delete_worktrees = false;
                }
            }
            f if Some(f) == self.force_field_index() => {
                self.options.force_delete_worktrees = !self.options.force_delete_worktrees;
            }
            f if Some(f) == self.branch_field_index() => {
                self.options.delete_branches = !self.options.delete_branches;
            }
            f if Some(f) == self.container_field_index() => {
                self.options.delete_containers = !self.options.delete_containers;
            }
            _ => {}
        }
    }

    fn max_field(&self) -> usize {
        if !self.options.delete_sessions {
            return 2; // move(0), delete(1)
        }
        let mut count = 2; // move(0), delete(1)
        if self.has_managed_worktrees {
            count += 2; // worktree checkbox + branch checkbox
            if self.options.delete_worktrees {
                count += 1; // force checkbox
            }
        }
        if self.has_containers {
            count += 1; // container checkbox
        }
        count
    }

    fn worktree_field_index(&self) -> Option<usize> {
        if self.options.delete_sessions && self.has_managed_worktrees {
            Some(2)
        } else {
            None
        }
    }

    fn force_field_index(&self) -> Option<usize> {
        if self.options.delete_sessions
            && self.has_managed_worktrees
            && self.options.delete_worktrees
        {
            Some(3)
        } else {
            None
        }
    }

    fn branch_field_index(&self) -> Option<usize> {
        if self.options.delete_sessions && self.has_managed_worktrees {
            if self.options.delete_worktrees {
                Some(4) // after force checkbox
            } else {
                Some(3)
            }
        } else {
            None
        }
    }

    fn container_field_index(&self) -> Option<usize> {
        if self.options.delete_sessions && self.has_containers {
            let base = 2;
            let mut offset = 0;
            if self.has_managed_worktrees {
                offset += 2; // worktree + branch
                if self.options.delete_worktrees {
                    offset += 1; // force
                }
            }
            Some(base + offset)
        } else {
            None
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<GroupDeleteOptions> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => DialogResult::Submit(self.options.clone()),
            KeyCode::Tab => {
                self.focused_field = (self.focused_field + 1) % self.max_field();
                DialogResult::Continue
            }
            KeyCode::BackTab => {
                let max = self.max_field();
                self.focused_field = if self.focused_field == 0 {
                    max - 1
                } else {
                    self.focused_field - 1
                };
                DialogResult::Continue
            }
            KeyCode::Char(' ') => {
                self.toggle_focused_field();
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let max = self.max_field();
                self.focused_field = if self.focused_field == 0 {
                    max - 1
                } else {
                    self.focused_field - 1
                };
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.focused_field = (self.focused_field + 1) % self.max_field();
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.focusable_rects.clear();
        let show_worktree_option = self.options.delete_sessions && self.has_managed_worktrees;
        let show_force_option = show_worktree_option && self.options.delete_worktrees;
        let show_container_option = self.options.delete_sessions && self.has_containers;
        let dialog_width = 50;
        let mut dialog_height = 11; // Base height
        if show_worktree_option {
            dialog_height += 2; // worktree + branch checkboxes
            if show_force_option {
                dialog_height += 1; // force checkbox
            }
        }
        if show_container_option {
            dialog_height += 1;
        }

        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.error))
            .title(" Delete Group ")
            .title_style(Style::default().fg(theme.error).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let mut constraints = vec![
            Constraint::Length(2), // Group info
            Constraint::Length(1), // Spacer
            Constraint::Length(1), // Move option
            Constraint::Length(1), // Delete option
        ];
        if show_worktree_option {
            constraints.push(Constraint::Length(1)); // Worktree checkbox
            if show_force_option {
                constraints.push(Constraint::Length(1)); // Force checkbox
            }
            constraints.push(Constraint::Length(1)); // Branch checkbox
        }
        if show_container_option {
            constraints.push(Constraint::Length(1)); // Container checkbox
        }
        constraints.push(Constraint::Min(1)); // Hints

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        // Group info
        let session_word = if self.session_count == 1 {
            "session"
        } else {
            "sessions"
        };
        let info_line = Line::from(vec![
            Span::styled("Group: ", Style::default().fg(theme.text)),
            Span::styled(
                format!("\"{}\"", self.group_path),
                Style::default().fg(theme.accent).bold(),
            ),
            Span::styled(
                format!(" ({} {})", self.session_count, session_word),
                Style::default().fg(theme.dimmed),
            ),
        ]);
        frame.render_widget(Paragraph::new(info_line), chunks[0]);

        // Move sessions option
        let move_focused = self.focused_field == 0;
        let move_selected = !self.options.delete_sessions;
        let move_radio = if move_selected { "(•)" } else { "( )" };
        let move_style = if move_focused {
            Style::default().fg(theme.accent).underlined()
        } else if move_selected {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.dimmed)
        };
        let move_line = Line::from(vec![
            Span::styled(move_radio, move_style),
            Span::styled(" Move sessions to default group", move_style),
        ]);
        frame.render_widget(Paragraph::new(move_line), chunks[2]);
        self.focusable_rects.push((0, chunks[2]));

        // Delete sessions option
        let delete_focused = self.focused_field == 1;
        let delete_selected = self.options.delete_sessions;
        let delete_radio = if delete_selected { "(•)" } else { "( )" };
        let delete_style = if delete_focused {
            Style::default().fg(theme.error).underlined()
        } else if delete_selected {
            Style::default().fg(theme.error)
        } else {
            Style::default().fg(theme.dimmed)
        };
        let delete_line = Line::from(vec![
            Span::styled(delete_radio, delete_style),
            Span::styled(" Delete all sessions", delete_style),
        ]);
        frame.render_widget(Paragraph::new(delete_line), chunks[3]);
        self.focusable_rects.push((1, chunks[3]));

        // Track current chunk index for optional checkboxes
        let mut next_chunk = 4;

        let style = CheckboxStyle::delete_group(theme);

        // Worktree checkbox (only shown when delete is selected and has managed worktrees)
        if show_worktree_option {
            let wt_focused = Some(self.focused_field) == self.worktree_field_index();
            let wt_line = checkbox_line(
                theme,
                "Also delete managed worktrees",
                None,
                4,
                self.options.delete_worktrees,
                wt_focused,
                style,
            );
            frame.render_widget(Paragraph::new(wt_line), chunks[next_chunk]);
            if let Some(idx) = self.worktree_field_index() {
                self.focusable_rects.push((idx, chunks[next_chunk]));
            }
            next_chunk += 1;

            if show_force_option {
                let fc_focused = Some(self.focused_field) == self.force_field_index();
                let fc_line = checkbox_line(
                    theme,
                    "Force delete",
                    None,
                    8,
                    self.options.force_delete_worktrees,
                    fc_focused,
                    style,
                );
                frame.render_widget(Paragraph::new(fc_line), chunks[next_chunk]);
                if let Some(idx) = self.force_field_index() {
                    self.focusable_rects.push((idx, chunks[next_chunk]));
                }
                next_chunk += 1;
            }

            // Branch checkbox (shown alongside worktree option)
            let br_focused = Some(self.focused_field) == self.branch_field_index();
            let br_line = checkbox_line(
                theme,
                "Also delete git branches",
                None,
                4,
                self.options.delete_branches,
                br_focused,
                style,
            );
            frame.render_widget(Paragraph::new(br_line), chunks[next_chunk]);
            if let Some(idx) = self.branch_field_index() {
                self.focusable_rects.push((idx, chunks[next_chunk]));
            }
            next_chunk += 1;
        }

        // Container checkbox (only shown when delete is selected and has containers)
        if show_container_option {
            let ct_focused = Some(self.focused_field) == self.container_field_index();
            let ct_line = checkbox_line(
                theme,
                "Also delete containers",
                None,
                4,
                self.options.delete_containers,
                ct_focused,
                style,
            );
            frame.render_widget(Paragraph::new(ct_line), chunks[next_chunk]);
            if let Some(idx) = self.container_field_index() {
                self.focusable_rects.push((idx, chunks[next_chunk]));
            }
            next_chunk += 1;
        }

        // Hints
        let hints = Line::from(vec![
            Span::styled("Tab", Style::default().fg(theme.hint)),
            Span::raw(" next  "),
            Span::styled("Space", Style::default().fg(theme.hint)),
            Span::raw(" select  "),
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" confirm  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" cancel"),
        ]);
        frame.render_widget(Paragraph::new(hints), chunks[next_chunk]);

        // Paint the hover highlight last so it sits behind a row that
        // still exists this frame (the row set shrinks when "Move" is
        // selected, so a stale rect from a previous layout is dropped).
        let rows: Vec<Rect> = self.focusable_rects.iter().map(|(_, r)| *r).collect();
        if let Some(rect) = self.hover.current_in(&rows) {
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

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn dialog() -> GroupDeleteOptionsDialog {
        GroupDeleteOptionsDialog::new("work".to_string(), 3, false, false)
    }

    fn dialog_with_worktrees() -> GroupDeleteOptionsDialog {
        GroupDeleteOptionsDialog::new("work".to_string(), 3, true, false)
    }

    fn dialog_with_containers() -> GroupDeleteOptionsDialog {
        GroupDeleteOptionsDialog::new("work".to_string(), 3, false, true)
    }

    fn dialog_with_both() -> GroupDeleteOptionsDialog {
        GroupDeleteOptionsDialog::new("work".to_string(), 3, true, true)
    }

    #[test]
    fn test_default_options() {
        let options = GroupDeleteOptions::default();
        assert!(!options.delete_sessions);
        assert!(!options.delete_worktrees);
        assert!(!options.force_delete_worktrees);
        assert!(!options.delete_branches);
        assert!(!options.delete_containers);
    }

    #[test]
    fn test_esc_cancels() {
        let mut dialog = dialog();
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_confirms() {
        let mut dialog = dialog();
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(_)));
    }

    #[test]
    fn test_default_is_move() {
        let mut dialog = dialog();
        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(opts) => {
                assert!(!opts.delete_sessions);
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_tab_cycles_fields() {
        let mut dialog = dialog();
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_backtab_cycles_reverse() {
        let mut dialog = dialog();
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(shift_key(KeyCode::BackTab));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(shift_key(KeyCode::BackTab));
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_space_selects_delete() {
        let mut dialog = dialog();
        dialog.handle_key(key(KeyCode::Tab)); // Move to delete option
        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(dialog.options.delete_sessions);
    }

    #[test]
    fn test_space_selects_move() {
        let mut dialog = dialog();
        dialog.options.delete_sessions = true;
        dialog.focused_field = 0;
        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.options.delete_sessions);
    }

    #[test]
    fn test_worktree_checkbox_appears_when_delete_selected() {
        let mut dialog = dialog_with_worktrees();
        assert_eq!(dialog.max_field(), 2); // No worktree option yet

        dialog.options.delete_sessions = true;
        assert_eq!(dialog.max_field(), 4); // Now worktree + branch options are available
    }

    #[test]
    fn test_worktree_checkbox_toggle() {
        let mut dialog = dialog_with_worktrees();
        dialog.options.delete_sessions = true;
        dialog.focused_field = 2;
        assert!(!dialog.options.delete_worktrees);

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(dialog.options.delete_worktrees);

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.options.delete_worktrees);
    }

    #[test]
    fn test_tab_includes_worktree_when_delete_selected() {
        let mut dialog = dialog_with_worktrees();
        dialog.options.delete_sessions = true;
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 2); // worktree checkbox

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 3); // branch checkbox

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_up_down_navigation() {
        let mut dialog = dialog();
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_jk_navigation() {
        let mut dialog = dialog();
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(key(KeyCode::Char('j')));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(key(KeyCode::Char('k')));
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_submit_with_delete_and_worktrees() {
        let mut dialog = dialog_with_worktrees();
        dialog.options.delete_sessions = true;
        dialog.options.delete_worktrees = true;

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(opts) => {
                assert!(opts.delete_sessions);
                assert!(opts.delete_worktrees);
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_selecting_move_clears_delete_worktrees() {
        let mut dialog = dialog_with_worktrees();
        dialog.options.delete_sessions = true;
        dialog.options.delete_worktrees = true;
        dialog.options.force_delete_worktrees = true;
        dialog.options.delete_branches = true;
        dialog.focused_field = 0;

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.options.delete_sessions);
        assert!(!dialog.options.delete_worktrees);
        assert!(!dialog.options.force_delete_worktrees);
        assert!(!dialog.options.delete_branches);
    }

    #[test]
    fn test_selecting_move_clears_delete_containers() {
        let mut dialog = dialog_with_containers();
        dialog.options.delete_sessions = true;
        dialog.options.delete_containers = true;
        dialog.focused_field = 0;

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.options.delete_sessions);
        assert!(!dialog.options.delete_containers);
    }

    #[test]
    fn test_container_checkbox_appears_when_delete_selected() {
        let mut dialog = dialog_with_containers();
        assert_eq!(dialog.max_field(), 2); // No container option yet

        dialog.options.delete_sessions = true;
        assert_eq!(dialog.max_field(), 3); // Now container option is available
    }

    #[test]
    fn test_container_checkbox_toggle() {
        let mut dialog = dialog_with_containers();
        dialog.options.delete_sessions = true;
        dialog.focused_field = 2; // Container is at index 2 when no worktrees
        assert!(!dialog.options.delete_containers);

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(dialog.options.delete_containers);

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.options.delete_containers);
    }

    #[test]
    fn test_tab_includes_container_when_delete_selected() {
        let mut dialog = dialog_with_containers();
        dialog.options.delete_sessions = true;
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 2); // Container checkbox

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_both_checkboxes_when_delete_selected() {
        let mut dialog = dialog_with_both();
        assert_eq!(dialog.max_field(), 2); // No checkboxes yet

        dialog.options.delete_sessions = true;
        assert_eq!(dialog.max_field(), 5); // worktree + branch + container checkboxes
    }

    #[test]
    fn test_tab_includes_both_checkboxes() {
        let mut dialog = dialog_with_both();
        dialog.options.delete_sessions = true;
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 1); // Delete option

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 2); // Worktree checkbox

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 3); // Branch checkbox

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 4); // Container checkbox

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 0); // Wrap around
    }

    #[test]
    fn test_container_checkbox_at_correct_index_with_worktrees() {
        let mut dialog = dialog_with_both();
        dialog.options.delete_sessions = true;

        // Worktree is at index 2
        assert_eq!(dialog.worktree_field_index(), Some(2));
        // Branch is at index 3 (after worktree)
        assert_eq!(dialog.branch_field_index(), Some(3));
        // Container is at index 4 (after branch)
        assert_eq!(dialog.container_field_index(), Some(4));
    }

    #[test]
    fn test_container_checkbox_at_correct_index_without_worktrees() {
        let mut dialog = dialog_with_containers();
        dialog.options.delete_sessions = true;

        // No worktree
        assert_eq!(dialog.worktree_field_index(), None);
        // Container is at index 2
        assert_eq!(dialog.container_field_index(), Some(2));
    }

    #[test]
    fn test_submit_with_all_options() {
        let mut dialog = dialog_with_both();
        dialog.options.delete_sessions = true;
        dialog.options.delete_worktrees = true;
        dialog.options.force_delete_worktrees = true;
        dialog.options.delete_branches = true;
        dialog.options.delete_containers = true;

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(opts) => {
                assert!(opts.delete_sessions);
                assert!(opts.delete_worktrees);
                assert!(opts.force_delete_worktrees);
                assert!(opts.delete_branches);
                assert!(opts.delete_containers);
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_selecting_move_clears_all_options() {
        let mut dialog = dialog_with_both();
        dialog.options.delete_sessions = true;
        dialog.options.delete_worktrees = true;
        dialog.options.force_delete_worktrees = true;
        dialog.options.delete_branches = true;
        dialog.options.delete_containers = true;
        dialog.focused_field = 0;

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.options.delete_sessions);
        assert!(!dialog.options.delete_worktrees);
        assert!(!dialog.options.force_delete_worktrees);
        assert!(!dialog.options.delete_branches);
        assert!(!dialog.options.delete_containers);
    }

    #[test]
    fn hover_highlights_row_without_moving_focus() {
        // Stage focusable rects manually; the real ones come from render().
        let mut dialog = dialog();
        dialog.focusable_rects = vec![(0, Rect::new(2, 4, 40, 1)), (1, Rect::new(2, 5, 40, 1))];
        dialog.focused_field = 0;

        // Over the delete row: highlight it, focus unchanged.
        assert!(dialog.handle_hover(5, 5));
        assert_eq!(dialog.hover.current(), Some(Rect::new(2, 5, 40, 1)));
        assert_eq!(dialog.focused_field, 0, "hover must not move focus");

        // Off all rows clears the highlight.
        assert!(dialog.handle_hover(99, 99));
        assert_eq!(dialog.hover.current(), None);
    }

    #[test]
    fn test_branch_checkbox_toggle() {
        let mut dialog = dialog_with_worktrees();
        dialog.options.delete_sessions = true;
        dialog.focused_field = 3; // Branch checkbox
        assert!(!dialog.options.delete_branches);

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(dialog.options.delete_branches);

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.options.delete_branches);
    }
}
