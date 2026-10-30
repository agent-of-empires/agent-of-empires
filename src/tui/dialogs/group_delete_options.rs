//! Group delete options dialog

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::checkbox::{checkbox_line, CheckboxStyle};
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroupDeleteAction {
    MoveToParent,
    #[default]
    MoveToDefault,
    DeleteSessions,
}

#[derive(Clone, Debug, Default)]
pub struct GroupDeleteOptions {
    pub action: GroupDeleteAction,
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
        let action = if group_path.contains('/') {
            GroupDeleteAction::MoveToParent
        } else {
            GroupDeleteAction::MoveToDefault
        };
        Self {
            group_path,
            session_count,
            has_managed_worktrees,
            has_containers,
            options: GroupDeleteOptions {
                action,
                ..Default::default()
            },
            focused_field: 0,
            focusable_rects: Vec::new(),
            hover: HoverState::default(),
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<GroupDeleteOptions>> {
        self.focused_field = super::hit(&self.focusable_rects, col, row)?;
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

    fn actions(&self) -> &[GroupDeleteAction] {
        if self.group_path.contains('/') {
            &[
                GroupDeleteAction::MoveToParent,
                GroupDeleteAction::MoveToDefault,
                GroupDeleteAction::DeleteSessions,
            ]
        } else {
            &[
                GroupDeleteAction::MoveToDefault,
                GroupDeleteAction::DeleteSessions,
            ]
        }
    }

    fn toggle_focused_field(&mut self) {
        if let Some(action) = self.actions().get(self.focused_field).copied() {
            if action == GroupDeleteAction::DeleteSessions {
                self.options.action = action;
            } else {
                self.options = GroupDeleteOptions {
                    action,
                    ..Default::default()
                };
            }
            return;
        }
        match self.focused_field {
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
        let mut count = self.actions().len();
        if self.options.action != GroupDeleteAction::DeleteSessions {
            return count;
        }
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
        if self.options.action == GroupDeleteAction::DeleteSessions && self.has_managed_worktrees {
            Some(self.actions().len())
        } else {
            None
        }
    }

    fn force_field_index(&self) -> Option<usize> {
        if self.options.action == GroupDeleteAction::DeleteSessions
            && self.has_managed_worktrees
            && self.options.delete_worktrees
        {
            Some(self.actions().len() + 1)
        } else {
            None
        }
    }

    fn branch_field_index(&self) -> Option<usize> {
        if self.options.action == GroupDeleteAction::DeleteSessions && self.has_managed_worktrees {
            Some(self.actions().len() + 1 + usize::from(self.options.delete_worktrees))
        } else {
            None
        }
    }

    fn container_field_index(&self) -> Option<usize> {
        if self.options.action == GroupDeleteAction::DeleteSessions && self.has_containers {
            let base = self.actions().len();
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
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => {
                self.focused_field =
                    super::cycle_focus(self.focused_field, self.max_field(), false);
                DialogResult::Continue
            }
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => {
                self.focused_field = super::cycle_focus(self.focused_field, self.max_field(), true);
                DialogResult::Continue
            }
            KeyCode::Char(' ') => {
                self.toggle_focused_field();
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.focusable_rects.clear();
        let deleting = self.options.action == GroupDeleteAction::DeleteSessions;
        let show_worktree_option = deleting && self.has_managed_worktrees;
        let show_force_option = show_worktree_option && self.options.delete_worktrees;
        let show_container_option = deleting && self.has_containers;
        let dialog_width = 50;
        let mut dialog_height = 9 + self.actions().len() as u16;
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
        ];
        constraints.extend(self.actions().iter().map(|_| Constraint::Length(1)));
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

        for (index, action) in self.actions().iter().enumerate() {
            let focused = self.focused_field == index;
            let selected = self.options.action == *action;
            let radio = if selected { "(•)" } else { "( )" };
            let color = if *action == GroupDeleteAction::DeleteSessions {
                theme.error
            } else {
                theme.accent
            };
            let style = if focused {
                Style::default().fg(color).underlined()
            } else if selected {
                Style::default().fg(color)
            } else {
                Style::default().fg(theme.dimmed)
            };
            let label = match action {
                GroupDeleteAction::MoveToParent => " Move sessions to parent group",
                GroupDeleteAction::MoveToDefault => " Move sessions to default group",
                GroupDeleteAction::DeleteSessions => " Delete all sessions",
            };
            let line = Line::from(vec![Span::styled(radio, style), Span::styled(label, style)]);
            frame.render_widget(Paragraph::new(line), chunks[index + 2]);
        }
        let action_count = self.actions().len();
        for index in 0..action_count {
            self.focusable_rects.push((index, chunks[index + 2]));
        }
        let mut next_chunk = action_count + 2;

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
        d.options.action = GroupDeleteAction::DeleteSessions;
        d
    }

    #[test]
    fn enter_submits_and_defaults_to_moving_the_sessions() {
        let mut d = dialog(false, false);
        assert!(matches!(
            d.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
        for (path, action) in [
            ("work", GroupDeleteAction::MoveToDefault),
            ("work/projects", GroupDeleteAction::MoveToParent),
            ("work/projects/feature", GroupDeleteAction::MoveToParent),
        ] {
            let mut d = GroupDeleteOptionsDialog::new(path.to_string(), 3, false, false);
            match d.handle_key(key(KeyCode::Enter)) {
                DialogResult::Submit(opts) => assert_eq!(opts.action, action),
                _ => panic!("expected Submit"),
            }
        }

        let mut d = deleting(true, true);
        d.options.delete_worktrees = true;
        d.options.force_delete_worktrees = true;
        d.options.delete_branches = true;
        d.options.delete_containers = true;
        match d.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(opts) => {
                assert_eq!(opts.action, GroupDeleteAction::DeleteSessions);
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
            d.options.action = GroupDeleteAction::DeleteSessions;
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
        assert_eq!(d.options.action, GroupDeleteAction::DeleteSessions);
        d.focused_field = 0;
        d.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(d.options.action, GroupDeleteAction::MoveToDefault);

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
        for (path, field, action) in [
            ("work", 0, GroupDeleteAction::MoveToDefault),
            ("work/projects", 0, GroupDeleteAction::MoveToParent),
            ("work/projects", 1, GroupDeleteAction::MoveToDefault),
        ] {
            let mut d = GroupDeleteOptionsDialog::new(path.to_string(), 3, true, true);
            d.options = GroupDeleteOptions {
                action: GroupDeleteAction::DeleteSessions,
                delete_worktrees: true,
                force_delete_worktrees: true,
                delete_branches: true,
                delete_containers: true,
            };
            d.focused_field = field;

            d.handle_key(key(KeyCode::Char(' ')));
            assert_eq!(d.options.action, action);
            assert!(!d.options.delete_worktrees);
            assert!(!d.options.force_delete_worktrees);
            assert!(!d.options.delete_branches);
            assert!(!d.options.delete_containers);
            assert_eq!(d.max_field(), d.actions().len());
        }
    }

    #[test]
    fn subgroup_actions_render_in_order_and_clicks_select_them() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut d = GroupDeleteOptionsDialog::new("work/projects".to_string(), 3, true, true);
        let theme = crate::tui::styles::load_theme("empire");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| d.render(f, f.area(), &theme)).unwrap();
        let buffer = terminal.backend().buffer();
        for (index, label) in [
            "(•) Move sessions to parent group",
            "( ) Move sessions to default group",
            "( ) Delete all sessions",
        ]
        .iter()
        .enumerate()
        {
            let (field, rect) = d.focusable_rects[index];
            assert_eq!(field, index);
            let line: String = (rect.x..rect.right())
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect();
            assert!(line.starts_with(label), "{line:?}");
            assert_eq!(rect.y, d.focusable_rects[0].1.y + index as u16);
        }

        for (index, action) in [
            (1, GroupDeleteAction::MoveToDefault),
            (2, GroupDeleteAction::DeleteSessions),
            (0, GroupDeleteAction::MoveToParent),
        ] {
            let (_, rect) = d.focusable_rects[index];
            assert!(matches!(
                d.handle_click(rect.x, rect.y),
                Some(DialogResult::Continue)
            ));
            match d.handle_key(key(KeyCode::Enter)) {
                DialogResult::Submit(options) => assert_eq!(options.action, action),
                _ => panic!("expected Submit"),
            }
        }
    }

    #[test]
    fn subgroup_navigation_and_resource_checkboxes_follow_the_three_actions() {
        for (worktrees, containers) in [(true, false), (false, true), (true, true)] {
            let mut d = GroupDeleteOptionsDialog::new(
                "work/projects".to_string(),
                3,
                worktrees,
                containers,
            );
            for (field, action) in [
                (1, GroupDeleteAction::MoveToDefault),
                (2, GroupDeleteAction::DeleteSessions),
            ] {
                d.handle_key(key(KeyCode::Tab));
                assert_eq!(d.focused_field, field);
                d.handle_key(key(KeyCode::Char(' ')));
                assert_eq!(d.options.action, action);
            }
            if worktrees {
                d.handle_key(key(KeyCode::Tab));
                d.handle_key(key(KeyCode::Char(' ')));
                assert!(d.options.delete_worktrees);
                assert_eq!(d.force_field_index(), Some(4));
                d.handle_key(key(KeyCode::Tab));
                d.handle_key(key(KeyCode::Char(' ')));
                assert!(d.options.force_delete_worktrees);
                d.handle_key(key(KeyCode::Tab));
                d.handle_key(key(KeyCode::Char(' ')));
                assert!(d.options.delete_branches);
            }
            if containers {
                d.handle_key(key(KeyCode::Tab));
                d.handle_key(key(KeyCode::Char(' ')));
                assert!(d.options.delete_containers);
            }
            d.handle_key(key(KeyCode::Tab));
            assert_eq!(d.focused_field, 0);
            d.handle_key(key(KeyCode::BackTab));
            assert_eq!(d.focused_field, d.max_field() - 1);
        }
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
