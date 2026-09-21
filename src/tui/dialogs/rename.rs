//! Rename session / group dialog

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::DialogResult;
use crate::tui::components::{
    render_text_field, render_text_field_with_ghost, GroupGhostCompletion, ListPicker,
    ListPickerResult,
};
use crate::tui::styles::Theme;

/// Data returned when the rename dialog is submitted
#[derive(Debug, Clone)]
pub struct RenameData {
    /// New title (empty string means keep current)
    pub title: String,
    /// New group path (None means keep current, Some("") means remove from group)
    pub group: Option<String>,
    /// New profile (None means keep current, Some(name) means move to that profile)
    pub profile: Option<String>,
    /// Whether to also rename the git branch to match the title. Only ever
    /// true for a tied aoe-managed worktree session that opted into the
    /// branch toggle; always false otherwise.
    pub rename_branch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    Session,
    Group,
}

pub struct RenameDialog {
    mode: RenameMode,
    current_title: String,
    current_group: String,
    current_profile: String,
    available_profiles: Vec<String>,
    new_title: Input,
    new_group: Input,
    profile_index: usize,
    focused_field: usize, // Session: 0=title, 1=group, 2=profile; Group: 0=group, 1=profile
    existing_groups: Vec<String>,
    group_picker: ListPicker,
    group_ghost: Option<GroupGhostCompletion>,
    /// Inline validation error shown in Group mode when a duplicate name is entered.
    validation_error: Option<String>,
    /// Hit rect per focusable field (title / group / profile), set by
    /// `render`. Drives click + hover routing.
    focusable_rects: Vec<(usize, Rect)>,
    /// Set for a tied aoe-managed worktree session via
    /// [`Self::with_worktree_branch`]. When present, the dialog grows a
    /// fourth focusable field: an "Also rename git branch" toggle. The
    /// payload is `(current_branch, upstream)`; `upstream` drives the
    /// remote-orphan warning when the toggle is on.
    worktree_branch: Option<WorktreeBranch>,
    /// State of the branch toggle. Meaningless unless `worktree_branch` is set.
    rename_branch: bool,
}

/// Branch context for a tied worktree session's rename toggle.
struct WorktreeBranch {
    /// The session's current git branch (shown in the toggle row).
    current: String,
    /// Short upstream ref (e.g. `origin/hi`) when the branch tracks a remote,
    /// else `None`. Drives the "remote branch won't follow" warning.
    upstream: Option<String>,
}

impl RenameDialog {
    pub fn mode(&self) -> RenameMode {
        self.mode
    }

    #[cfg(test)]
    pub fn title_value(&self) -> &str {
        self.new_title.value()
    }

    pub fn new(
        current_title: &str,
        current_group: &str,
        current_profile: &str,
        available_profiles: Vec<String>,
        existing_groups: Vec<String>,
    ) -> Self {
        let profile_index = available_profiles
            .iter()
            .position(|p| p == current_profile)
            .unwrap_or(0);

        Self {
            mode: RenameMode::Session,
            current_title: current_title.to_string(),
            current_group: current_group.to_string(),
            current_profile: current_profile.to_string(),
            available_profiles,
            new_title: Input::default(),
            new_group: Input::new(current_group.to_string()),
            profile_index,
            focused_field: 0,
            existing_groups,
            group_picker: ListPicker::new("Select Group"),
            group_ghost: None,
            validation_error: None,
            focusable_rects: Vec::new(),
            worktree_branch: None,
            rename_branch: false,
        }
    }

    /// Attach tied-worktree branch context, enabling the "Also rename git
    /// branch" toggle. Call only for a Session-mode dialog whose session is a
    /// tied aoe-managed worktree. `upstream` is the short tracking ref
    /// (`origin/hi`) when the branch tracks a remote, used to warn that a
    /// rename leaves that remote branch behind.
    pub fn with_worktree_branch(mut self, current_branch: &str, upstream: Option<String>) -> Self {
        self.worktree_branch = Some(WorktreeBranch {
            current: current_branch.to_string(),
            upstream,
        });
        self
    }

    pub fn new_for_group(
        current_group: &str,
        current_profile: &str,
        available_profiles: Vec<String>,
        existing_groups: Vec<String>,
    ) -> Self {
        let profile_index = available_profiles
            .iter()
            .position(|p| p == current_profile)
            .unwrap_or(0);

        Self {
            mode: RenameMode::Group,
            current_title: String::new(),
            current_group: current_group.to_string(),
            current_profile: current_profile.to_string(),
            available_profiles,
            new_title: Input::default(),
            new_group: Input::new(current_group.to_string()),
            profile_index,
            focused_field: 0,
            existing_groups,
            group_picker: ListPicker::new("Select Group"),
            group_ghost: None,
            validation_error: None,
            focusable_rects: Vec::new(),
            worktree_branch: None,
            rename_branch: false,
        }
    }

    /// Whether the "Also rename git branch" toggle is present (tied worktree
    /// session only). When true it occupies focusable field index 3.
    fn shows_branch_toggle(&self) -> bool {
        self.mode == RenameMode::Session && self.worktree_branch.is_some()
    }

    fn is_branch_toggle_field(&self) -> bool {
        self.shows_branch_toggle() && self.focused_field == 3
    }

    fn field_count(&self) -> usize {
        match self.mode {
            // title, group, profile, and the branch toggle when present.
            RenameMode::Session => {
                if self.shows_branch_toggle() {
                    4
                } else {
                    3
                }
            }
            RenameMode::Group => 2, // group, profile
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<RenameData>> {
        // Group picker overlay wins when active so a click can pick a
        // group row without dropping the dialog underneath.
        if self.group_picker.is_active() {
            match self.group_picker.handle_click(col, row) {
                ListPickerResult::Continue => return Some(DialogResult::Continue),
                ListPickerResult::Cancelled => return Some(DialogResult::Continue),
                ListPickerResult::Selected(value) => {
                    self.new_group = Input::new(value);
                    // Mirror the keyboard picker path: the ghost
                    // autocomplete state goes stale once the user
                    // commits to a value via the picker, so drop it.
                    self.group_ghost = None;
                    return Some(DialogResult::Continue);
                }
            }
        }
        let pos = ratatui::layout::Position::from((col, row));
        let hit = self
            .focusable_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(f, _)| *f)?;
        self.focused_field = hit;
        // Cycle the profile chip on click; flip the branch toggle on click;
        // text fields just take focus.
        if self.is_profile_field() && !self.available_profiles.is_empty() {
            self.profile_index = (self.profile_index + 1) % self.available_profiles.len();
        } else if self.is_branch_toggle_field() {
            self.rename_branch = !self.rename_branch;
        }
        Some(DialogResult::Continue)
    }

    /// Hover only updates the group-picker overlay highlight (menu-style
    /// behavior the user expects). It deliberately does NOT move focus
    /// between the title / group / profile rows: stealing focus from the
    /// field the user is typing into just because the mouse cursor
    /// drifts across the dialog is jarring. Click still sets focus.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        if self.group_picker.is_active() {
            return self.group_picker.handle_hover(col, row);
        }
        false
    }

    fn is_profile_field(&self) -> bool {
        match self.mode {
            RenameMode::Session => self.focused_field == 2,
            RenameMode::Group => self.focused_field == 1,
        }
    }

    fn focused_input(&mut self) -> Option<&mut Input> {
        match self.mode {
            RenameMode::Session => match self.focused_field {
                0 => Some(&mut self.new_title),
                1 => Some(&mut self.new_group),
                _ => None,
            },
            RenameMode::Group => match self.focused_field {
                0 => Some(&mut self.new_group),
                _ => None,
            },
        }
    }

    fn is_group_field(&self) -> bool {
        match self.mode {
            RenameMode::Session => self.focused_field == 1,
            RenameMode::Group => self.focused_field == 0,
        }
    }

    fn next_field(&mut self) {
        self.focused_field = (self.focused_field + 1) % self.field_count();
    }

    fn prev_field(&mut self) {
        let count = self.field_count();
        self.focused_field = if self.focused_field == 0 {
            count - 1
        } else {
            self.focused_field - 1
        };
    }

    fn recompute_group_ghost(&mut self) {
        self.group_ghost = GroupGhostCompletion::compute(&self.new_group, &self.existing_groups);
    }

    fn accept_group_ghost(&mut self) {
        if let Some(ghost) = self.group_ghost.take() {
            if let Some(new_value) = ghost.accept(&self.new_group) {
                self.new_group = Input::new(new_value);
                self.recompute_group_ghost();
            }
        }
    }

    fn group_ghost_text(&self) -> Option<&str> {
        self.group_ghost.as_ref().map(|g| g.ghost_text())
    }

    fn selected_profile(&self) -> &str {
        &self.available_profiles[self.profile_index]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<RenameData> {
        // Handle group picker if active
        if self.group_picker.is_active() {
            if let ListPickerResult::Selected(value) = self.group_picker.handle_key(key) {
                self.new_group = Input::new(value);
                self.group_ghost = None;
            }
            return DialogResult::Continue;
        }

        // Ctrl+P opens group picker on group field
        if key.code == KeyCode::Char('p')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.is_group_field()
            && !self.existing_groups.is_empty()
        {
            self.group_picker.activate(self.existing_groups.clone());
            return DialogResult::Continue;
        }

        // Right/End arrow at end of group input with ghost: accept ghost text
        if self.is_group_field()
            && matches!(key.code, KeyCode::Right | KeyCode::End)
            && key.modifiers == KeyModifiers::NONE
            && self.group_ghost.is_some()
        {
            let cursor = self.new_group.cursor();
            let char_len = self.new_group.value().chars().count();
            if cursor >= char_len {
                self.accept_group_ghost();
                return DialogResult::Continue;
            }
        }

        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => {
                let title_value = self.new_title.value().trim().to_string();
                let group_value = self.new_group.value().trim();
                let selected_profile = self.selected_profile();
                let profile_changed = selected_profile != self.current_profile;

                // If nothing has changed, cancel. Arming the branch toggle
                // counts as a change even when title/group/profile are
                // untouched (rename a drifted branch in place).
                let branch_rename = self.shows_branch_toggle() && self.rename_branch;
                if title_value.is_empty()
                    && group_value == self.current_group
                    && !profile_changed
                    && !branch_rename
                {
                    return DialogResult::Cancel;
                }

                // Validate that the new group name does not already exist
                if self.mode == RenameMode::Group
                    && !group_value.is_empty()
                    && group_value != self.current_group
                    && self.existing_groups.iter().any(|g| g == group_value)
                {
                    self.validation_error = Some(
                        "A group with this name already exists.\nEnter a different name."
                            .to_string(),
                    );
                    return DialogResult::Continue;
                }

                // Determine the group value:
                // - Same as current means keep current group (None)
                // - Empty (and was non-empty) means remove from group (Some(""))
                // - Any other changed value means set new group
                let group = if group_value == self.current_group {
                    None
                } else if group_value.is_empty() {
                    Some(String::new())
                } else {
                    Some(group_value.to_string())
                };

                // Determine profile value
                let profile = if profile_changed {
                    Some(selected_profile.to_string())
                } else {
                    None
                };

                DialogResult::Submit(RenameData {
                    title: title_value,
                    group,
                    profile,
                    rename_branch: self.shows_branch_toggle() && self.rename_branch,
                })
            }
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.prev_field();
                } else {
                    self.next_field();
                }
                if self.is_group_field() {
                    self.recompute_group_ghost();
                } else {
                    self.group_ghost = None;
                }
                DialogResult::Continue
            }
            KeyCode::Down => {
                self.next_field();
                if self.is_group_field() {
                    self.recompute_group_ghost();
                } else {
                    self.group_ghost = None;
                }
                DialogResult::Continue
            }
            KeyCode::Up => {
                self.prev_field();
                if self.is_group_field() {
                    self.recompute_group_ghost();
                } else {
                    self.group_ghost = None;
                }
                DialogResult::Continue
            }
            KeyCode::Char(' ') if self.is_branch_toggle_field() => {
                self.rename_branch = !self.rename_branch;
                DialogResult::Continue
            }
            KeyCode::Left if self.is_profile_field() => {
                // Cycle profile backwards
                if self.profile_index == 0 {
                    self.profile_index = self.available_profiles.len().saturating_sub(1);
                } else {
                    self.profile_index -= 1;
                }
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char(' ') if self.is_profile_field() => {
                // Cycle profile forwards
                self.profile_index = (self.profile_index + 1) % self.available_profiles.len();
                DialogResult::Continue
            }
            _ => {
                if let Some(input) = self.focused_input() {
                    input.handle_event(&crossterm::event::Event::Key(key));
                }
                if self.is_group_field() {
                    self.recompute_group_ghost();
                    self.validation_error = None;
                }
                DialogResult::Continue
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if let Some(input) = self.focused_input() {
            super::paste_into_input(input, text);
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        match self.mode {
            RenameMode::Session => self.render_session(frame, area, theme),
            RenameMode::Group => self.render_group(frame, area, theme),
        }
    }

    fn render_session(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.focusable_rects.clear();
        let show_toggle = self.shows_branch_toggle();
        // The remote-orphan warning only matters once the toggle is armed and
        // the branch actually tracks a remote.
        let show_warning = show_toggle
            && self.rename_branch
            && self
                .worktree_branch
                .as_ref()
                .is_some_and(|w| w.upstream.is_some());

        let dialog_width = 50;
        let height = 15 + if show_toggle { 1 } else { 0 } + if show_warning { 2 } else { 0 };
        let block = super::dialog_block(" Edit Session ", theme);
        let (_, inner) = super::render_dialog_frame(frame, area, dialog_width, height, block);

        // Fixed rows first (current values, spacer, the three input fields),
        // then the optional branch toggle / warning, then spacer + hint. The
        // dynamic indices are tracked so the wiring below stays in sync.
        let mut constraints = vec![
            Constraint::Length(1), // 0 Current title
            Constraint::Length(1), // 1 Current group
            Constraint::Length(1), // 2 Current profile
            Constraint::Length(1), // 3 Spacer
            Constraint::Length(1), // 4 New title field
            Constraint::Length(1), // 5 New group field
            Constraint::Length(1), // 6 Profile selector
        ];
        let toggle_idx = show_toggle.then(|| {
            constraints.push(Constraint::Length(1));
            constraints.len() - 1
        });
        let warning_idx = show_warning.then(|| {
            constraints.push(Constraint::Length(2));
            constraints.len() - 1
        });
        constraints.push(Constraint::Length(1)); // Spacer
        constraints.push(Constraint::Min(1)); // Hint
        let hint_idx = constraints.len() - 1;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        // Current title
        let current_title_line = Line::from(vec![
            Span::styled("Current title: ", Style::default().fg(theme.dimmed)),
            Span::styled(&self.current_title, Style::default().fg(theme.text)),
        ]);
        frame.render_widget(Paragraph::new(current_title_line), chunks[0]);

        // Current group
        self.render_current_group(frame, chunks[1], theme);

        // Current profile
        self.render_current_profile(frame, chunks[2], theme);

        // New title field
        render_text_field(
            frame,
            chunks[4],
            "New title:",
            &self.new_title,
            self.focused_field == 0,
            None,
            theme,
        );
        self.focusable_rects.push((0, chunks[4]));

        // New group field
        self.render_group_field(frame, chunks[5], theme);
        self.focusable_rects.push((1, chunks[5]));

        // Profile selector
        self.render_profile_selector(frame, chunks[6], theme);
        self.focusable_rects.push((2, chunks[6]));

        // Branch toggle + remote-orphan warning (tied worktree only)
        if let Some(idx) = toggle_idx {
            self.render_branch_toggle(frame, chunks[idx], theme);
            self.focusable_rects.push((3, chunks[idx]));
        }
        if let Some(idx) = warning_idx {
            self.render_branch_warning(frame, chunks[idx], theme);
        }

        // Hint
        self.render_hints(frame, chunks[hint_idx], theme);

        // Render group picker overlay
        if self.group_picker.is_active() {
            self.group_picker.render(frame, area, theme);
        }
    }

    fn render_branch_toggle(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let focused = self.is_branch_toggle_field();
        let checkbox = if self.rename_branch { "[x]" } else { "[ ]" };
        let style = if focused {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.text)
        };
        let mut spans = vec![
            Span::styled(format!("{checkbox} "), style),
            Span::styled("Also rename git branch", style),
        ];
        // Show the current branch dimmed so the user knows what is being
        // renamed (and from what), since the title may already match the dir.
        if let Some(wt) = &self.worktree_branch {
            spans.push(Span::styled(
                format!("  ({})", wt.current),
                Style::default().fg(theme.dimmed),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_branch_warning(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let Some(wt) = &self.worktree_branch else {
            return;
        };
        let Some(upstream) = &wt.upstream else {
            return;
        };
        let lines = vec![
            Line::from(Span::styled(
                format!("! branch '{}' tracks {};", wt.current, upstream),
                Style::default().fg(theme.error),
            )),
            Line::from(Span::styled(
                "  the remote branch won't follow",
                Style::default().fg(theme.error),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_group(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.focusable_rects.clear();
        let dialog_width = 50;
        let has_error = self.validation_error.is_some();
        let dialog_height = if has_error { 16 } else { 13 };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Rename Group ")
            .title_style(Style::default().fg(theme.title).bold());
        let (_, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);

        let mut constraints = vec![
            Constraint::Length(1), // Current group
            Constraint::Length(1), // Current profile
            Constraint::Length(1), // Spacer
            Constraint::Length(1), // New group field
            Constraint::Length(1), // Profile selector
            Constraint::Length(1), // Spacer
            Constraint::Min(1),    // Hint
        ];
        if has_error {
            constraints.insert(5, Constraint::Length(2)); // Validation error (2 lines)
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        // Current group
        self.render_current_group(frame, chunks[0], theme);

        // Current profile
        self.render_current_profile(frame, chunks[1], theme);

        // New group field
        self.render_group_field(frame, chunks[3], theme);
        self.focusable_rects.push((0, chunks[3]));

        // Profile selector
        self.render_profile_selector(frame, chunks[4], theme);
        self.focusable_rects.push((1, chunks[4]));

        if has_error {
            // Validation error (two lines, one sentence each)
            let error_text: Vec<Line> = self
                .validation_error
                .as_deref()
                .unwrap_or("")
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(theme.error),
                    ))
                })
                .collect();
            frame.render_widget(Paragraph::new(error_text), chunks[5]);
            // Hint is shifted one index further
            self.render_hints(frame, chunks[7], theme);
        } else {
            // Hint
            self.render_hints(frame, chunks[6], theme);
        }

        // Render group picker overlay
        if self.group_picker.is_active() {
            self.group_picker.render(frame, area, theme);
        }
    }

    fn render_current_group(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let group_display = if self.current_group.is_empty() {
            "(none)".to_string()
        } else {
            self.current_group.clone()
        };
        let line = Line::from(vec![
            Span::styled("Current group: ", Style::default().fg(theme.dimmed)),
            Span::styled(group_display, Style::default().fg(theme.text)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_current_profile(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let line = Line::from(vec![
            Span::styled("Current profile: ", Style::default().fg(theme.dimmed)),
            Span::styled(&self.current_profile, Style::default().fg(theme.text)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_group_field(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let group_hint = if self.is_group_field() && !self.existing_groups.is_empty() {
            Some("Ctrl+P to browse")
        } else {
            None
        };
        render_text_field_with_ghost(
            frame,
            area,
            "New group:",
            &self.new_group,
            self.is_group_field(),
            group_hint,
            self.group_ghost_text(),
            theme,
        );
    }

    fn render_profile_selector(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let profile_focused = self.is_profile_field();
        let selected_profile = self.selected_profile();
        let profile_style = if profile_focused {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.text)
        };

        let profile_line = Line::from(vec![
            Span::styled(
                "Profile:    ",
                if profile_focused {
                    Style::default().fg(theme.accent)
                } else {
                    Style::default().fg(theme.dimmed)
                },
            ),
            Span::styled("< ", Style::default().fg(theme.dimmed)),
            Span::styled(selected_profile, profile_style),
            Span::styled(" >", Style::default().fg(theme.dimmed)),
        ]);
        frame.render_widget(Paragraph::new(profile_line), area);
    }

    fn render_hints(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let mut hint_spans = vec![
            Span::styled("Tab", Style::default().fg(theme.hint)),
            Span::raw(" switch  "),
        ];
        if self.is_branch_toggle_field() {
            hint_spans.push(Span::styled("Space", Style::default().fg(theme.hint)));
            hint_spans.push(Span::raw(" toggle  "));
        }
        if self.is_group_field() && !self.existing_groups.is_empty() {
            if self.group_ghost_text().is_some() {
                hint_spans.push(Span::styled("→", Style::default().fg(theme.hint)));
                hint_spans.push(Span::raw(" accept  "));
            }
            hint_spans.push(Span::styled("C-p", Style::default().fg(theme.hint)));
            hint_spans.push(Span::raw(" groups  "));
        }
        hint_spans.push(Span::styled("Enter", Style::default().fg(theme.hint)));
        hint_spans.push(Span::raw(" save  "));
        hint_spans.push(Span::styled("Esc", Style::default().fg(theme.hint)));
        hint_spans.push(Span::raw(" cancel"));
        let hint = Line::from(hint_spans);
        frame.render_widget(Paragraph::new(hint), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::{ctrl_key, key, shift_key};

    const ONE_PROFILE: &[&str] = &["default"];
    const MULTI_PROFILES: &[&str] = &["default", "work", "personal"];
    const GROUPS: &[&str] = &["work", "work/frontend", "personal"];

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    fn dlg(
        title: &str,
        group: &str,
        profile: &str,
        profiles: &[&str],
        groups: &[&str],
    ) -> RenameDialog {
        RenameDialog::new(title, group, profile, owned(profiles), owned(groups))
    }

    /// Session dialog over "Old Title" / "old-group" with three profiles.
    fn fixture() -> RenameDialog {
        dlg("Old Title", "old-group", "default", MULTI_PROFILES, &[])
    }

    fn group_dlg() -> RenameDialog {
        RenameDialog::new_for_group("work", "default", owned(ONE_PROFILE), owned(GROUPS))
    }

    fn tied(upstream: Option<&str>) -> RenameDialog {
        dlg("hi", "", "default", ONE_PROFILE, &[])
            .with_worktree_branch("thing", upstream.map(|s| s.to_string()))
    }

    fn type_str(d: &mut RenameDialog, text: &str) {
        for c in text.chars() {
            d.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn press(d: &mut RenameDialog, code: KeyCode, times: usize) {
        for _ in 0..times {
            d.handle_key(key(code));
        }
    }

    /// Focus the group field, clear its pre-filled value and type `text`.
    fn retype_group(d: &mut RenameDialog, text: &str) {
        d.handle_key(key(KeyCode::Tab));
        press(d, KeyCode::Backspace, "old-group".len());
        type_str(d, text);
    }

    fn submitted(result: DialogResult<RenameData>) -> RenameData {
        match result {
            DialogResult::Submit(data) => data,
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn new_seeds_current_values_and_prefills_group() {
        let d = dlg("Original", "work/frontend", "default", ONE_PROFILE, &[]);
        assert_eq!(d.current_title, "Original");
        assert_eq!(d.current_group, "work/frontend");
        assert_eq!(d.current_profile, "default");
        assert_eq!(d.new_title.value(), "");
        assert_eq!(d.new_group.value(), "work/frontend");
        assert_eq!((d.profile_index, d.focused_field), (0, 0));

        assert_eq!(dlg("t", "", "default", ONE_PROFILE, &[]).current_group, "");
        assert_eq!(dlg("t", "g", "work", MULTI_PROFILES, &[]).profile_index, 1);
    }

    #[test]
    fn submit_reports_only_changed_fields() {
        type Edit = fn(&mut RenameDialog);
        let cases: &[(&str, Edit, (&str, Option<&str>, Option<&str>))] = &[
            ("title only", |d| type_str(d, "New"), ("New", None, None)),
            (
                "group only",
                |d| retype_group(d, "new-group"),
                ("", Some("new-group"), None),
            ),
            (
                "title and group",
                |d| {
                    type_str(d, "New Title");
                    retype_group(d, "new-group");
                },
                ("New Title", Some("new-group"), None),
            ),
            (
                "cleared group ungroups",
                |d| retype_group(d, ""),
                ("", Some(""), None),
            ),
            (
                "profile only",
                |d| {
                    press(d, KeyCode::Tab, 2);
                    d.handle_key(key(KeyCode::Right));
                },
                ("", None, Some("work")),
            ),
            (
                "all three",
                |d| {
                    type_str(d, "New Title");
                    retype_group(d, "new-group");
                    d.handle_key(key(KeyCode::Tab));
                    d.handle_key(key(KeyCode::Right));
                },
                ("New Title", Some("new-group"), Some("work")),
            ),
            (
                "values are trimmed",
                |d| {
                    type_str(d, "  New Title  ");
                    retype_group(d, "  new-group  ");
                },
                ("New Title", Some("new-group"), None),
            ),
        ];

        for (name, edit, (title, group, profile)) in cases {
            let mut d = fixture();
            edit(&mut d);
            let data = submitted(d.handle_key(key(KeyCode::Enter)));
            assert_eq!(data.title, *title, "{name}");
            assert_eq!(data.group.as_deref(), *group, "{name}");
            assert_eq!(data.profile.as_deref(), *profile, "{name}");
            assert!(!data.rename_branch, "{name}");
        }

        // An unchanged profile stays None even when it is not the first one.
        let mut d = dlg("t", "g", "work", MULTI_PROFILES, &[]);
        type_str(&mut d, "X");
        assert_eq!(submitted(d.handle_key(key(KeyCode::Enter))).profile, None);
    }

    #[test]
    fn esc_cancels_and_unchanged_enter_cancels() {
        let mut d = fixture();
        type_str(&mut d, "New");
        assert!(matches!(
            d.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));

        let mut d = fixture();
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Cancel
        ));
        // Current values survive editing without submitting.
        assert_eq!(d.current_title, "Old Title");
        assert_eq!(d.current_group, "old-group");
    }

    #[test]
    fn tab_and_arrows_cycle_focus() {
        let mut d = fixture();
        for expected in [1, 2, 0] {
            d.handle_key(key(KeyCode::Tab));
            assert_eq!(d.focused_field, expected);
        }
        for expected in [2, 1, 0] {
            d.handle_key(shift_key(KeyCode::Tab));
            assert_eq!(d.focused_field, expected);
        }
        for expected in [1, 2] {
            d.handle_key(key(KeyCode::Down));
            assert_eq!(d.focused_field, expected);
        }
        for expected in [1, 0] {
            d.handle_key(key(KeyCode::Up));
            assert_eq!(d.focused_field, expected);
        }
    }

    #[test]
    fn text_keys_edit_the_focused_field_only() {
        let mut d = fixture();
        type_str(&mut d, "abc");
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(d.new_title.value(), "ab");
        assert_eq!(d.new_group.value(), "old-group");

        // Left moves the cursor inside the focused input.
        d.handle_key(key(KeyCode::Left));
        d.handle_key(key(KeyCode::Char('X')));
        assert_eq!(d.new_title.value(), "aXb");

        // Group field appends to its pre-filled value.
        d.handle_key(key(KeyCode::Tab));
        d.handle_key(key(KeyCode::Char('z')));
        assert_eq!(d.new_group.value(), "old-groupz");

        // The profile chip takes no text.
        d.handle_key(key(KeyCode::Tab));
        d.handle_key(key(KeyCode::Char('q')));
        assert_eq!(d.new_title.value(), "aXb");
        assert_eq!(d.new_group.value(), "old-groupz");
        assert_eq!(d.profile_index, 0);
    }

    #[test]
    fn profile_chip_cycles_both_ways_and_wraps() {
        for code in [KeyCode::Right, KeyCode::Char(' ')] {
            let mut d = fixture();
            d.focused_field = 2;
            for expected in ["work", "personal", "default"] {
                d.handle_key(key(code));
                assert_eq!(d.selected_profile(), expected);
            }
        }

        let mut d = fixture();
        d.focused_field = 2;
        for expected in ["personal", "work", "default"] {
            d.handle_key(key(KeyCode::Left));
            assert_eq!(d.selected_profile(), expected);
        }

        // Arrows on a text field move the cursor instead of the chip.
        let mut d = fixture();
        type_str(&mut d, "ab");
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.profile_index, 0);
    }

    #[test]
    fn group_picker_opens_on_the_group_field_only() {
        let mut d = dlg("t", "old-group", "default", ONE_PROFILE, GROUPS);
        d.handle_key(ctrl_key(KeyCode::Char('p')));
        assert!(!d.group_picker.is_active(), "title field");
        d.focused_field = 2;
        d.handle_key(ctrl_key(KeyCode::Char('p')));
        assert!(!d.group_picker.is_active(), "profile field");

        let mut d = dlg("t", "old-group", "default", ONE_PROFILE, &[]);
        d.handle_key(key(KeyCode::Tab));
        d.handle_key(ctrl_key(KeyCode::Char('p')));
        assert!(!d.group_picker.is_active(), "no groups to pick");
    }

    #[test]
    fn group_picker_selection_fills_the_group_field() {
        let open = || {
            let mut d = dlg("t", "old-group", "default", ONE_PROFILE, GROUPS);
            d.handle_key(key(KeyCode::Tab));
            d.handle_key(ctrl_key(KeyCode::Char('p')));
            assert!(d.group_picker.is_active());
            d
        };

        let mut d = open();
        d.handle_key(key(KeyCode::Esc));
        assert!(!d.group_picker.is_active());
        assert_eq!(d.new_group.value(), "old-group");

        let mut d = open();
        d.handle_key(key(KeyCode::Down));
        d.handle_key(key(KeyCode::Enter));
        assert_eq!(d.new_group.value(), "work/frontend");

        let mut d = open();
        d.handle_key(key(KeyCode::Enter));
        assert!(!d.group_picker.is_active());
        assert_eq!(d.new_group.value(), "work");
        let data = submitted(d.handle_key(key(KeyCode::Enter)));
        assert_eq!(data.group.as_deref(), Some("work"));
    }

    #[test]
    fn group_ghost_completes_the_typed_prefix() {
        let typed = |text: &str| {
            let mut d = dlg("t", "", "default", ONE_PROFILE, GROUPS);
            d.handle_key(key(KeyCode::Tab));
            type_str(&mut d, text);
            d
        };

        assert_eq!(typed("p").group_ghost_text(), Some("ersonal"));
        assert!(typed("z").group_ghost_text().is_none());
        // "work" and "work/frontend" share the prefix "work".
        assert_eq!(typed("w").group_ghost_text(), Some("ork"));

        for accept in [KeyCode::Right, KeyCode::End] {
            let mut d = typed("p");
            d.handle_key(key(accept));
            assert_eq!(d.new_group.value(), "personal");
        }

        let mut d = typed("p");
        d.handle_key(key(KeyCode::Tab));
        assert!(d.group_ghost_text().is_none(), "cleared on field switch");

        let mut d = typed("w");
        d.handle_key(ctrl_key(KeyCode::Char('p')));
        d.handle_key(key(KeyCode::Enter));
        assert!(d.group_ghost_text().is_none(), "cleared on picker select");
        assert_eq!(d.new_group.value(), "work");
    }

    #[test]
    fn group_rename_rejects_a_duplicate_name() {
        let retype = |text: &str| {
            let mut d = group_dlg();
            press(&mut d, KeyCode::Backspace, "work".len());
            type_str(&mut d, text);
            d
        };

        let mut d = retype("personal");
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert!(d.validation_error.is_some());
        // Editing the field clears the error.
        d.handle_key(key(KeyCode::Backspace));
        assert!(d.validation_error.is_none());

        let mut d = retype("projects");
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(_)
        ));
        assert!(d.validation_error.is_none());

        // The group's own name is not a duplicate; unchanged just cancels.
        let mut d = group_dlg();
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Cancel
        ));
        assert!(d.validation_error.is_none());
    }

    #[test]
    fn branch_toggle_exists_only_for_a_tied_worktree() {
        let mut d = dlg("hi", "", "default", ONE_PROFILE, &[]);
        assert!(!d.shows_branch_toggle());
        assert_eq!(d.field_count(), 3);
        type_str(&mut d, "x");
        assert!(!submitted(d.handle_key(key(KeyCode::Enter))).rename_branch);

        let d = tied(Some("origin/thing"));
        assert!(d.shows_branch_toggle());
        assert_eq!(d.field_count(), 4);
    }

    #[test]
    fn branch_toggle_flips_with_space_and_rides_along_on_submit() {
        let mut d = tied(None);
        press(&mut d, KeyCode::Tab, 3);
        assert!(d.is_branch_toggle_field());
        assert!(!d.rename_branch);
        d.handle_key(key(KeyCode::Char(' ')));
        assert!(d.rename_branch);
        d.handle_key(key(KeyCode::Char(' ')));
        assert!(!d.rename_branch);

        // Arming the toggle alone submits, so a drifted branch can be brought
        // in line with an unchanged title.
        d.handle_key(key(KeyCode::Char(' ')));
        let data = submitted(d.handle_key(key(KeyCode::Enter)));
        assert_eq!(data.title, "");
        assert!(data.rename_branch);

        let mut d = tied(Some("origin/thing"));
        type_str(&mut d, "x");
        press(&mut d, KeyCode::Tab, 3);
        d.handle_key(key(KeyCode::Char(' ')));
        let data = submitted(d.handle_key(key(KeyCode::Enter)));
        assert_eq!(data.title, "x");
        assert!(data.rename_branch);

        // Space on the profile chip still cycles it.
        let mut d =
            dlg("hi", "", "default", MULTI_PROFILES, &[]).with_worktree_branch("thing", None);
        press(&mut d, KeyCode::Tab, 2);
        assert!(d.is_profile_field());
        d.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(d.profile_index, 1);
        assert!(!d.rename_branch);
    }
}
