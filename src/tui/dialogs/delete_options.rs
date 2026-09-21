//! Unified delete dialog

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::buttons::render_yes_no;
use crate::tui::components::checkbox::{checkbox_line, CheckboxStyle};
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

#[derive(Clone, Debug, Default)]
pub struct DeleteOptions {
    pub delete_worktree: bool,
    pub force_delete: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
    /// Keep the scratch directory on disk. Only read when `is_scratch`.
    pub keep_scratch: bool,
}

#[derive(Clone, Debug, Default)]
pub struct DeleteDialogConfig {
    pub worktree_branch: Option<String>,
    pub has_sandbox: bool,
    pub project_path: Option<String>,
    /// Surfaces the "Keep scratch directory" opt-in, off by default so the
    /// normal flow stays one keystroke.
    pub is_scratch: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusElement {
    WorktreeCheckbox,
    ForceCheckbox,
    BranchCheckbox,
    SandboxCheckbox,
    KeepScratchCheckbox,
    YesButton,
    NoButton,
}

pub struct UnifiedDeleteDialog {
    session_title: String,
    config: DeleteDialogConfig,
    options: DeleteOptions,
    focus: FocusElement,
    focusable_elements: Vec<FocusElement>,
    /// The `[Yes]` button, zero-sized until the first render.
    yes_button_area: Rect,
    no_button_area: Rect,
    /// Hit rect per focusable row. Yes/No keep their own fields above,
    /// since `render_yes_no` already returns them.
    focusable_rects: Vec<(FocusElement, Rect)>,
    /// The hovered button. Visual only; never moves keyboard `focus`.
    hover: HoverState,
}

impl UnifiedDeleteDialog {
    pub fn new(session_title: String, config: DeleteDialogConfig, profile: &str) -> Self {
        let user_config = match config.project_path.as_ref() {
            Some(p) => crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                profile,
                std::path::Path::new(p),
            ),
            None => crate::session::config::profile_config::resolve_config_or_warn(profile),
        };

        let options = DeleteOptions {
            delete_worktree: config.worktree_branch.is_some() && user_config.worktree.auto_cleanup,
            force_delete: false,
            delete_branch: config.worktree_branch.is_some()
                && user_config.worktree.delete_branch_on_cleanup,
            delete_sandbox: config.has_sandbox && user_config.sandbox.auto_cleanup,
            keep_scratch: false,
        };

        let initial_focus = if config.worktree_branch.is_some() {
            FocusElement::WorktreeCheckbox
        } else if config.has_sandbox {
            FocusElement::SandboxCheckbox
        } else if config.is_scratch {
            FocusElement::KeepScratchCheckbox
        } else {
            FocusElement::NoButton
        };

        let focusable_elements = Self::build_focusable_elements(&config, &options);

        Self {
            session_title,
            config,
            options,
            focus: initial_focus,
            focusable_elements,
            yes_button_area: Rect::default(),
            no_button_area: Rect::default(),
            focusable_rects: Vec::new(),
            hover: HoverState::default(),
        }
    }

    /// Route a left-click: `Submit` on `[Yes]`, `Cancel` on `[No]`,
    /// `Continue` on a checkbox row (focus then toggle), `None` elsewhere
    /// inside the dialog, which the modal absorbs.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<DeleteOptions>> {
        let pos = ratatui::layout::Position::from((col, row));
        if self.yes_button_area.contains(pos) {
            return Some(DialogResult::Submit(self.options.clone()));
        }
        if self.no_button_area.contains(pos) {
            return Some(DialogResult::Cancel);
        }
        if let Some(element) = self.hit_focusable(col, row) {
            self.focus = element;
            self.toggle_focused_checkbox();
            return Some(DialogResult::Continue);
        }
        None
    }

    /// Highlight the row under the cursor without moving keyboard `focus`:
    /// a drift between reading the dialog and pressing Enter must not flip
    /// which action fires. True when the highlight changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let mut rects = vec![self.yes_button_area, self.no_button_area];
        rects.extend(self.focusable_rects.iter().map(|(_, r)| *r));
        self.hover.update(col, row, &rects)
    }

    /// Checkbox rows captured this frame; Yes/No are painted elsewhere.
    fn checkbox_rects(&self) -> Vec<Rect> {
        self.focusable_rects.iter().map(|(_, r)| *r).collect()
    }

    fn hit_focusable(&self, col: u16, row: u16) -> Option<FocusElement> {
        let pos = ratatui::layout::Position::from((col, row));
        self.focusable_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(element, _)| *element)
    }

    /// Toggle whichever checkbox the focus is currently on. No-op for
    /// the Yes/No buttons, which use Submit/Cancel from `handle_key`
    /// instead. Mirrors the Space key handler so click and Space
    /// produce byte-identical state changes.
    fn toggle_focused_checkbox(&mut self) {
        match self.focus {
            FocusElement::WorktreeCheckbox => {
                self.options.delete_worktree = !self.options.delete_worktree;
                if !self.options.delete_worktree {
                    self.options.force_delete = false;
                }
                self.rebuild_focusable_elements();
            }
            FocusElement::ForceCheckbox => {
                self.options.force_delete = !self.options.force_delete;
            }
            FocusElement::BranchCheckbox => {
                self.options.delete_branch = !self.options.delete_branch;
            }
            FocusElement::SandboxCheckbox => {
                self.options.delete_sandbox = !self.options.delete_sandbox;
            }
            FocusElement::KeepScratchCheckbox => {
                self.options.keep_scratch = !self.options.keep_scratch;
            }
            FocusElement::YesButton | FocusElement::NoButton => {}
        }
    }

    fn build_focusable_elements(
        config: &DeleteDialogConfig,
        options: &DeleteOptions,
    ) -> Vec<FocusElement> {
        let mut elements = Vec::new();
        if config.worktree_branch.is_some() {
            elements.push(FocusElement::WorktreeCheckbox);
            if options.delete_worktree {
                elements.push(FocusElement::ForceCheckbox);
            }
            elements.push(FocusElement::BranchCheckbox);
        }
        if config.has_sandbox {
            elements.push(FocusElement::SandboxCheckbox);
        }
        if config.is_scratch {
            elements.push(FocusElement::KeepScratchCheckbox);
        }
        elements.push(FocusElement::YesButton);
        elements.push(FocusElement::NoButton);
        elements
    }

    fn rebuild_focusable_elements(&mut self) {
        let old_focus = self.focus;
        self.focusable_elements = Self::build_focusable_elements(&self.config, &self.options);
        if !self.focusable_elements.contains(&old_focus) {
            self.focus = self.focusable_elements[0];
        }
    }

    pub fn options(&self) -> &DeleteOptions {
        &self.options
    }

    fn focus_index(&self) -> usize {
        self.focusable_elements
            .iter()
            .position(|&e| e == self.focus)
            .unwrap_or(0)
    }

    fn focus_next(&mut self) {
        let idx = self.focus_index();
        let next_idx = (idx + 1) % self.focusable_elements.len();
        self.focus = self.focusable_elements[next_idx];
    }

    fn focus_prev(&mut self) {
        let idx = self.focus_index();
        let prev_idx = if idx == 0 {
            self.focusable_elements.len() - 1
        } else {
            idx - 1
        };
        self.focus = self.focusable_elements[prev_idx];
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<DeleteOptions> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => DialogResult::Cancel,

            KeyCode::Char('y') | KeyCode::Char('Y') => DialogResult::Submit(self.options.clone()),

            KeyCode::Enter => match self.focus {
                FocusElement::YesButton => DialogResult::Submit(self.options.clone()),
                FocusElement::NoButton => DialogResult::Cancel,
                // Enter on a checkbox toggles it (same as Space) rather
                // than submitting; share the toggle logic with the
                // Space key handler and mouse click handler.
                _ => {
                    self.toggle_focused_checkbox();
                    DialogResult::Continue
                }
            },

            KeyCode::Char(' ') => {
                self.toggle_focused_checkbox();
                DialogResult::Continue
            }

            KeyCode::Tab => {
                self.focus_next();
                DialogResult::Continue
            }

            KeyCode::BackTab => {
                self.focus_prev();
                DialogResult::Continue
            }

            KeyCode::Up | KeyCode::Char('k') => {
                self.focus_prev();
                DialogResult::Continue
            }

            KeyCode::Down | KeyCode::Char('j') => {
                self.focus_next();
                DialogResult::Continue
            }

            KeyCode::Left | KeyCode::Char('h') => {
                self.focus = FocusElement::YesButton;
                DialogResult::Continue
            }

            KeyCode::Right | KeyCode::Char('l') => {
                self.focus = FocusElement::NoButton;
                DialogResult::Continue
            }

            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Rebuilt every frame so a layout change (e.g. focusing the
        // worktree checkbox unhides the force-delete row) doesn't leave
        // stale hit rects pointing at the wrong cells.
        self.focusable_rects.clear();
        let has_worktree = self.config.worktree_branch.is_some();
        let has_sandbox = self.config.has_sandbox;
        let is_scratch = self.config.is_scratch;
        let show_force = has_worktree && self.options.delete_worktree;
        // Count checkbox rows: worktree + force (if worktree checked) +
        // branch (if worktree exists) + sandbox + keep-scratch (if scratch).
        let checkbox_count = if has_worktree { 2 } else { 0 }
            + (show_force as u16)
            + (has_sandbox as u16)
            + (is_scratch as u16);

        let dialog_width = 55;
        let dialog_height = if checkbox_count > 0 {
            8 + checkbox_count
        } else {
            7
        };

        let block = super::toned_dialog_block(" Delete Session ", theme.error, theme.error);
        let (_, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);

        let mut constraints = vec![
            Constraint::Length(1), // message
            Constraint::Length(1), // spacer after message
        ];

        if checkbox_count > 0 {
            for _ in 0..checkbox_count {
                constraints.push(Constraint::Length(1)); // each checkbox
            }
            constraints.push(Constraint::Length(1)); // spacer after checkboxes
        }

        constraints.push(Constraint::Length(1)); // buttons
        constraints.push(Constraint::Length(1)); // spacer before hints
        constraints.push(Constraint::Length(1)); // hints

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);

        let mut chunk_idx = 0;

        let message = format!("Delete \"{}\"?", self.session_title);
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(theme.text))
                .alignment(Alignment::Center),
            chunks[chunk_idx],
        );
        chunk_idx += 1;
        chunk_idx += 1; // skip spacer

        if checkbox_count > 0 {
            if let Some(branch) = &self.config.worktree_branch {
                let area = chunks[chunk_idx];
                let focused = self.focus == FocusElement::WorktreeCheckbox;
                self.render_checkbox(
                    frame,
                    area,
                    theme,
                    "Delete worktree",
                    Some(branch),
                    self.options.delete_worktree,
                    focused,
                );
                self.focusable_rects
                    .push((FocusElement::WorktreeCheckbox, area));
                chunk_idx += 1;

                if show_force {
                    let area = chunks[chunk_idx];
                    let force_focused = self.focus == FocusElement::ForceCheckbox;
                    self.render_indented_checkbox(
                        frame,
                        area,
                        theme,
                        "Force delete",
                        self.options.force_delete,
                        force_focused,
                    );
                    self.focusable_rects
                        .push((FocusElement::ForceCheckbox, area));
                    chunk_idx += 1;
                }

                let area = chunks[chunk_idx];
                let branch_focused = self.focus == FocusElement::BranchCheckbox;
                self.render_checkbox(
                    frame,
                    area,
                    theme,
                    "Delete branch",
                    Some(branch),
                    self.options.delete_branch,
                    branch_focused,
                );
                self.focusable_rects
                    .push((FocusElement::BranchCheckbox, area));
                chunk_idx += 1;
            }

            if has_sandbox {
                let area = chunks[chunk_idx];
                let focused = self.focus == FocusElement::SandboxCheckbox;
                self.render_checkbox(
                    frame,
                    area,
                    theme,
                    "Delete container",
                    None,
                    self.options.delete_sandbox,
                    focused,
                );
                self.focusable_rects
                    .push((FocusElement::SandboxCheckbox, area));
                chunk_idx += 1;
            }

            if is_scratch {
                let area = chunks[chunk_idx];
                let focused = self.focus == FocusElement::KeepScratchCheckbox;
                self.render_checkbox(
                    frame,
                    area,
                    theme,
                    "Keep scratch directory",
                    None,
                    self.options.keep_scratch,
                    focused,
                );
                self.focusable_rects
                    .push((FocusElement::KeepScratchCheckbox, area));
                chunk_idx += 1;
            }

            chunk_idx += 1; // skip spacer
        }

        self.render_buttons(frame, chunks[chunk_idx], theme);
        chunk_idx += 1;
        chunk_idx += 1; // skip spacer

        self.render_hints(frame, chunks[chunk_idx], theme, checkbox_count > 0);

        // Yes/No are highlighted by `render_yes_no`; a hovered checkbox
        // row is painted here, guarded to this frame's rows so a stale
        // rect (layout shrank between mouse moves) is dropped.
        if let Some(rect) = self.hover.current_in(&self.checkbox_rects()) {
            paint_hover_bg(frame, rect, theme.selection);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_checkbox(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        label: &str,
        detail: Option<&str>,
        checked: bool,
        focused: bool,
    ) {
        let line = checkbox_line(
            theme,
            label,
            detail,
            0,
            checked,
            focused,
            CheckboxStyle::delete_session(theme),
        );
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_indented_checkbox(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        label: &str,
        checked: bool,
        focused: bool,
    ) {
        let line = checkbox_line(
            theme,
            label,
            None,
            4,
            checked,
            focused,
            CheckboxStyle::delete_session(theme),
        );
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_buttons(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let (yes, no) = render_yes_no(
            frame,
            area,
            theme,
            self.focus == FocusElement::YesButton,
            self.hover.current(),
        );
        self.yes_button_area = yes;
        self.no_button_area = no;
    }

    fn render_hints(&self, frame: &mut Frame, area: Rect, theme: &Theme, has_checkboxes: bool) {
        let mut hints = vec![
            Span::styled("Tab", Style::default().fg(theme.hint)),
            Span::raw(" navigate  "),
        ];

        if has_checkboxes {
            hints.extend([
                Span::styled("Space", Style::default().fg(theme.hint)),
                Span::raw(" toggle  "),
            ]);
        }

        hints.extend([
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" confirm  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" cancel"),
        ]);

        frame.render_widget(Paragraph::new(Line::from(hints)), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::key;

    /// No worktree, sandbox or scratch, so only the two buttons focus.
    fn simple_dialog() -> UnifiedDeleteDialog {
        UnifiedDeleteDialog::new(
            "Test Session".to_string(),
            DeleteDialogConfig::default(),
            "default",
        )
    }

    /// Every checkbox present, over a config that turns both auto-cleanups on
    /// and branch deletion off.
    fn full_dialog() -> UnifiedDeleteDialog {
        let _home = crate::session::test_support::isolate_app_dir();
        std::fs::write(
            crate::session::config::config_path().unwrap(),
            "[worktree]\nauto_cleanup = true\ndelete_branch_on_cleanup = false\n[sandbox]\nauto_cleanup = true\n",
        )
        .unwrap();
        UnifiedDeleteDialog::new(
            "Test Session".to_string(),
            DeleteDialogConfig {
                worktree_branch: Some("feature-branch".to_string()),
                has_sandbox: true,
                project_path: None,
                is_scratch: false,
            },
            "default",
        )
    }

    fn scratch_dialog() -> UnifiedDeleteDialog {
        UnifiedDeleteDialog::new(
            "Scratch Session".to_string(),
            DeleteDialogConfig {
                worktree_branch: None,
                has_sandbox: false,
                project_path: None,
                is_scratch: true,
            },
            "default",
        )
    }

    /// Stage the rects `render` would capture: the two buttons, plus a row per
    /// checkbox for `full_dialog`.
    fn stage_rects(dialog: &mut UnifiedDeleteDialog, checkboxes: bool) {
        dialog.focusable_rects.clear();
        if checkboxes {
            dialog
                .focusable_rects
                .push((FocusElement::WorktreeCheckbox, Rect::new(5, 3, 30, 1)));
            if dialog.options.delete_worktree {
                dialog
                    .focusable_rects
                    .push((FocusElement::ForceCheckbox, Rect::new(5, 4, 30, 1)));
            }
            dialog
                .focusable_rects
                .push((FocusElement::BranchCheckbox, Rect::new(5, 5, 30, 1)));
            dialog
                .focusable_rects
                .push((FocusElement::SandboxCheckbox, Rect::new(5, 6, 30, 1)));
        }
        dialog.yes_button_area = Rect::new(10, 8, 5, 1);
        dialog.no_button_area = Rect::new(19, 8, 4, 1);
    }

    #[test]
    #[serial_test::serial]
    fn opening_focus_and_checkbox_defaults_follow_the_config() {
        assert_eq!(simple_dialog().focus, FocusElement::NoButton);

        let dialog = full_dialog();
        assert_eq!(dialog.focus, FocusElement::WorktreeCheckbox);
        assert!(dialog.options.delete_worktree, "worktree.auto_cleanup");
        assert!(!dialog.options.delete_branch, "delete_branch_on_cleanup");
        assert!(dialog.options.delete_sandbox, "sandbox.auto_cleanup");

        let dialog = scratch_dialog();
        assert_eq!(dialog.focus, FocusElement::KeepScratchCheckbox);
        assert!(!dialog.options.keep_scratch, "keeping is opt-in");
    }

    #[test]
    #[serial_test::serial]
    fn tab_cycles_every_focusable_and_arrows_pick_a_button() {
        let mut dialog = full_dialog();
        for expected in [
            FocusElement::ForceCheckbox,
            FocusElement::BranchCheckbox,
            FocusElement::SandboxCheckbox,
            FocusElement::YesButton,
            FocusElement::NoButton,
            FocusElement::WorktreeCheckbox,
        ] {
            dialog.handle_key(key(KeyCode::Tab));
            assert_eq!(dialog.focus, expected);
        }

        let mut dialog = simple_dialog();
        dialog.handle_key(key(KeyCode::Left));
        assert_eq!(dialog.focus, FocusElement::YesButton);
        dialog.handle_key(key(KeyCode::Right));
        assert_eq!(dialog.focus, FocusElement::NoButton);
    }

    #[test]
    #[serial_test::serial]
    fn space_toggles_the_focused_checkbox() {
        let mut dialog = full_dialog();
        for focus in [FocusElement::WorktreeCheckbox, FocusElement::BranchCheckbox] {
            dialog.focus = focus;
            let read = |d: &UnifiedDeleteDialog| match focus {
                FocusElement::WorktreeCheckbox => d.options.delete_worktree,
                _ => d.options.delete_branch,
            };
            let before = read(&dialog);
            dialog.handle_key(key(KeyCode::Char(' ')));
            assert_eq!(read(&dialog), !before, "{focus:?}");
            dialog.handle_key(key(KeyCode::Char(' ')));
            assert_eq!(read(&dialog), before, "{focus:?}");
        }

        let mut dialog = scratch_dialog();
        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(dialog.options.keep_scratch);
        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.options.keep_scratch);
    }

    #[test]
    #[serial_test::serial]
    fn confirm_and_cancel_keys_decide_the_dialog() {
        for code in [KeyCode::Esc, KeyCode::Char('n')] {
            assert!(matches!(
                full_dialog().handle_key(key(code)),
                DialogResult::Cancel
            ));
        }
        assert!(matches!(
            simple_dialog().handle_key(key(KeyCode::Enter)),
            DialogResult::Cancel
        ));

        let mut dialog = simple_dialog();
        dialog.focus = FocusElement::YesButton;
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(_)
        ));

        // Submitting carries the staged options out.
        let mut dialog = full_dialog();
        dialog.options.delete_worktree = true;
        dialog.options.force_delete = true;
        dialog.options.delete_branch = true;
        dialog.options.delete_sandbox = true;
        match dialog.handle_key(key(KeyCode::Char('y'))) {
            DialogResult::Submit(opts) => {
                assert!(opts.delete_worktree);
                assert!(opts.force_delete);
                assert!(opts.delete_branch);
                assert!(opts.delete_sandbox);
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn clicks_hit_the_buttons_and_the_checkbox_rows() {
        // Every rect is zero-sized before the first render, so nothing hits.
        assert!(simple_dialog().handle_click(5, 5).is_none());

        let mut dialog = simple_dialog();
        stage_rects(&mut dialog, false);
        assert!(matches!(
            dialog.handle_click(12, 8),
            Some(DialogResult::Submit(_))
        ));
        assert!(matches!(
            dialog.handle_click(20, 8),
            Some(DialogResult::Cancel)
        ));
        assert!(
            dialog.handle_click(16, 8).is_none(),
            "the gap between the buttons is dead space"
        );

        let mut dialog = full_dialog();
        stage_rects(&mut dialog, true);
        let before = dialog.options.delete_branch;
        assert!(matches!(
            dialog.handle_click(10, 5),
            Some(DialogResult::Continue)
        ));
        assert_eq!(dialog.focus, FocusElement::BranchCheckbox);
        assert_eq!(dialog.options.delete_branch, !before);

        // Turning worktree off clears force_delete and drops its row from the
        // focusables.
        let mut dialog = full_dialog();
        dialog.options.delete_worktree = true;
        dialog.options.force_delete = true;
        dialog.rebuild_focusable_elements();
        stage_rects(&mut dialog, true);
        let focusables = dialog.focusable_elements.len();
        assert!(matches!(
            dialog.handle_click(10, 3),
            Some(DialogResult::Continue)
        ));
        assert!(!dialog.options.delete_worktree);
        assert!(!dialog.options.force_delete);
        assert!(dialog.focusable_elements.len() < focusables);
    }

    #[test]
    #[serial_test::serial]
    fn hover_highlights_without_moving_keyboard_focus() {
        // Drift between reading the dialog and pressing Enter or Space must
        // not shift which element the next keystroke targets.
        let mut dialog = simple_dialog();
        stage_rects(&mut dialog, false);
        dialog.focus = FocusElement::NoButton;
        for (col, want) in [(12, dialog.yes_button_area), (20, dialog.no_button_area)] {
            assert!(dialog.handle_hover(col, 8));
            assert_eq!(dialog.hover.current(), Some(want));
            assert_eq!(dialog.focus, FocusElement::NoButton);
        }
        assert!(!dialog.handle_hover(20, 8), "same cell is no redraw");
        assert!(dialog.handle_hover(50, 50));
        assert_eq!(dialog.hover.current(), None);
        assert_eq!(dialog.focus, FocusElement::NoButton);

        let mut dialog = full_dialog();
        stage_rects(&mut dialog, true);
        dialog.focus = FocusElement::YesButton;
        assert!(dialog.handle_hover(10, 5));
        assert_eq!(dialog.hover.current(), Some(Rect::new(5, 5, 30, 1)));
        assert_eq!(dialog.focus, FocusElement::YesButton);
    }
}
