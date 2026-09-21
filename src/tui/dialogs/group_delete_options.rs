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

        let block = super::toned_dialog_block(" Delete Group ", theme.error, theme.error);
        let (_, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);

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
    use crate::tui::dialogs::test_keys::{key, shift_key};

    /// A dialog over a 3-session group, with or without worktrees and
    /// containers among those sessions.
    fn dialog(worktrees: bool, containers: bool) -> GroupDeleteOptionsDialog {
        GroupDeleteOptionsDialog::new("work".to_string(), 3, worktrees, containers)
    }

    /// The same, already switched to delete (the mode that reveals the
    /// per-resource checkboxes).
    fn deleting(worktrees: bool, containers: bool) -> GroupDeleteOptionsDialog {
        let mut d = dialog(worktrees, containers);
        d.options.delete_sessions = true;
        d
    }

    #[test]
    fn enter_submits_and_defaults_to_moving_the_sessions() {
        let mut d = dialog(false, false);
        assert!(matches!(
            d.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
        match dialog(false, false).handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(opts) => assert!(!opts.delete_sessions),
            _ => panic!("expected Submit"),
        }

        let mut d = deleting(true, true);
        d.options.delete_worktrees = true;
        d.options.force_delete_worktrees = true;
        d.options.delete_branches = true;
        d.options.delete_containers = true;
        match d.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(opts) => {
                assert!(opts.delete_sessions);
                assert!(opts.delete_worktrees);
                assert!(opts.force_delete_worktrees);
                assert!(opts.delete_branches);
                assert!(opts.delete_containers);
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn every_navigation_key_moves_between_the_visible_rows() {
        for (forward, back) in [
            (key(KeyCode::Tab), shift_key(KeyCode::BackTab)),
            (key(KeyCode::Down), key(KeyCode::Up)),
            (key(KeyCode::Char('j')), key(KeyCode::Char('k'))),
        ] {
            let mut d = dialog(false, false);
            d.handle_key(forward);
            assert_eq!(d.focused_field, 1);
            d.handle_key(back);
            assert_eq!(d.focused_field, 0);
        }

        // Tab wraps through however many checkbox rows the group's resources
        // and the delete mode expose.
        for (worktrees, containers, last) in [(true, false, 3), (false, true, 2), (true, true, 4)] {
            let mut d = deleting(worktrees, containers);
            for expected in 1..=last {
                d.handle_key(key(KeyCode::Tab));
                assert_eq!(d.focused_field, expected, "{worktrees} {containers}");
            }
            d.handle_key(key(KeyCode::Tab));
            assert_eq!(d.focused_field, 0, "wraps");
        }
    }

    #[test]
    fn checkbox_rows_appear_only_while_deleting_and_keep_their_order() {
        // Move mode offers the two mode rows alone.
        for (worktrees, containers, deleting_max) in
            [(true, false, 4), (false, true, 3), (true, true, 5)]
        {
            let mut d = dialog(worktrees, containers);
            assert_eq!(d.max_field(), 2, "move mode has no checkboxes");
            d.options.delete_sessions = true;
            assert_eq!(d.max_field(), deleting_max);
        }

        let d = deleting(true, true);
        assert_eq!(d.worktree_field_index(), Some(2));
        assert_eq!(d.branch_field_index(), Some(3));
        assert_eq!(d.container_field_index(), Some(4));

        let d = deleting(false, true);
        assert_eq!(d.worktree_field_index(), None);
        assert_eq!(d.container_field_index(), Some(2));
    }

    #[test]
    fn space_toggles_the_focused_row() {
        // Mode row 1 switches to delete; row 0 switches back.
        let mut d = dialog(false, false);
        d.handle_key(key(KeyCode::Tab));
        d.handle_key(key(KeyCode::Char(' ')));
        assert!(d.options.delete_sessions);
        d.focused_field = 0;
        d.handle_key(key(KeyCode::Char(' ')));
        assert!(!d.options.delete_sessions);

        // (worktrees, containers, focused row, the option it owns)
        type ToggleCase = (bool, bool, usize, fn(&GroupDeleteOptions) -> bool);
        let rows: &[ToggleCase] = &[
            (true, false, 2, |o| o.delete_worktrees),
            (true, false, 3, |o| o.delete_branches),
            (false, true, 2, |o| o.delete_containers),
        ];
        for (worktrees, containers, field, read) in rows {
            let mut d = deleting(*worktrees, *containers);
            d.focused_field = *field;
            assert!(!read(&d.options));
            d.handle_key(key(KeyCode::Char(' ')));
            assert!(read(&d.options), "row {field}");
            d.handle_key(key(KeyCode::Char(' ')));
            assert!(!read(&d.options), "row {field}");
        }
    }

    #[test]
    fn switching_back_to_move_clears_every_delete_option() {
        let mut d = deleting(true, true);
        d.options.delete_worktrees = true;
        d.options.force_delete_worktrees = true;
        d.options.delete_branches = true;
        d.options.delete_containers = true;
        d.focused_field = 0;

        d.handle_key(key(KeyCode::Char(' ')));
        assert!(!d.options.delete_sessions);
        assert!(!d.options.delete_worktrees);
        assert!(!d.options.force_delete_worktrees);
        assert!(!d.options.delete_branches);
        assert!(!d.options.delete_containers);
    }

    #[test]
    fn hover_highlights_a_row_without_moving_focus() {
        let mut d = dialog(false, false);
        // Staged manually; the real rects come from render().
        d.focusable_rects = vec![(0, Rect::new(2, 4, 40, 1)), (1, Rect::new(2, 5, 40, 1))];
        assert!(d.handle_hover(5, 5));
        assert_eq!(d.hover.current(), Some(Rect::new(2, 5, 40, 1)));
        assert_eq!(d.focused_field, 0, "hover must not move focus");
        assert!(d.handle_hover(99, 99));
        assert_eq!(d.hover.current(), None);
    }
}
