//! Unified delete dialog

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::buttons::render_yes_no;
use crate::tui::components::checkbox::{checkbox_line, CheckboxStyle};
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

/// Options for what to clean up when deleting a session
#[derive(Clone, Debug, Default)]
pub struct DeleteOptions {
    pub delete_worktree: bool,
    pub force_delete: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
    /// For scratch sessions: keep the scratch directory on disk instead of
    /// removing it. No effect when `DeleteDialogConfig.is_scratch` is false.
    pub keep_scratch: bool,
}

/// Configuration for what cleanup options to show in the dialog
#[derive(Clone, Debug, Default)]
pub struct DeleteDialogConfig {
    pub worktree_branch: Option<String>,
    pub has_sandbox: bool,
    /// Project path used to load repo-level config overrides.
    pub project_path: Option<String>,
    /// True iff the session being deleted is a scratch session. Surfaces a
    /// "Keep scratch directory" opt-in checkbox so users can rescue files
    /// mid-delete; defaults off so the normal flow stays one-keystroke.
    pub is_scratch: bool,
}

/// Focus states for navigation
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

/// Unified delete dialog that adapts based on available cleanup options
pub struct UnifiedDeleteDialog {
    session_title: String,
    config: DeleteDialogConfig,
    options: DeleteOptions,
    focus: FocusElement,
    focusable_elements: Vec<FocusElement>,
    /// Screen rect of the rendered `[Yes]` button. Captured during
    /// `render` so `handle_click` can hit-test the same cells the user
    /// sees. `Rect::default()` until the dialog has been rendered at
    /// least once.
    yes_button_area: Rect,
    /// Screen rect of the rendered `[No]` button, paired with
    /// `yes_button_area`.
    no_button_area: Rect,
    /// Per-focusable hit rect captured during `render`. Drives both
    /// hover (move focus) and click (toggle / submit). Yes/No still
    /// have dedicated fields above because the renderer returns them
    /// from `render_yes_no` already; everything else lives here.
    focusable_rects: Vec<(FocusElement, Rect)>,
    /// Which Yes/No button the mouse is over, for the hover highlight.
    /// Visual only; never moves keyboard `focus`.
    hover: HoverState,
}

impl UnifiedDeleteDialog {
    pub fn new(session_title: String, config: DeleteDialogConfig, profile: &str) -> Self {
        let user_config = match config.project_path.as_ref() {
            Some(p) => crate::session::repo_config::resolve_config_with_repo_or_warn(
                profile,
                std::path::Path::new(p),
            ),
            None => crate::session::profile_config::resolve_config_or_warn(profile),
        };

        let options = DeleteOptions {
            delete_worktree: config.worktree_branch.is_some() && user_config.worktree.auto_cleanup,
            force_delete: false,
            delete_branch: config.worktree_branch.is_some()
                && user_config.worktree.delete_branch_on_cleanup,
            delete_sandbox: config.has_sandbox && user_config.sandbox.auto_cleanup,
            // Scratch sessions default to remove. The user has to explicitly
            // opt in to keep the directory.
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

    /// Route a left-click. Returns `Some(Submit)` for `[Yes]`,
    /// `Some(Cancel)` for `[No]`, `Some(Continue)` for a click on a
    /// checkbox row (which is treated as a focus-then-toggle), and
    /// `None` for clicks that landed elsewhere inside the dialog
    /// (those are silently absorbed by the modal, no fall-through to
    /// the list). Rects are written during `render`; before the first
    /// render every rect is zero-sized so `contains()` returns false.
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

    /// Highlight the Yes/No button or checkbox row under the cursor.
    /// Hover never moves keyboard `focus` (mouse drift between reading the
    /// dialog and pressing Enter would otherwise silently flip which
    /// action fires); it only drives the visual highlight. Click still
    /// moves focus and (for checkboxes) toggles state. Returns `true` when
    /// the highlighted target changed so the caller can redraw.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let mut rects = vec![self.yes_button_area, self.no_button_area];
        rects.extend(self.focusable_rects.iter().map(|(_, r)| *r));
        self.hover.update(col, row, &rects)
    }

    /// Rects of the checkbox rows captured this frame, for the hover
    /// highlight. Yes/No are excluded; they're painted by `render_yes_no`.
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

        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.error))
            .title(" Delete Session ")
            .title_style(Style::default().fg(theme.error).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

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
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn simple_dialog() -> UnifiedDeleteDialog {
        UnifiedDeleteDialog::new(
            "Test Session".to_string(),
            DeleteDialogConfig::default(),
            "default",
        )
    }

    fn full_dialog() -> UnifiedDeleteDialog {
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

    #[test]
    fn test_default_options() {
        let options = DeleteOptions::default();
        assert!(!options.delete_worktree);
        assert!(!options.force_delete);
        assert!(!options.delete_branch);
        assert!(!options.delete_sandbox);
    }

    #[test]
    fn test_simple_dialog_focuses_no_button() {
        let dialog = simple_dialog();
        assert_eq!(dialog.focus, FocusElement::NoButton);
    }

    #[test]
    fn test_full_dialog_focuses_first_checkbox() {
        let dialog = full_dialog();
        assert_eq!(dialog.focus, FocusElement::WorktreeCheckbox);
    }

    #[test]
    fn test_full_dialog_respects_config_defaults() {
        let dialog = full_dialog();
        assert!(
            dialog.options.delete_worktree,
            "With default config (auto_cleanup: true), delete_worktree should be true"
        );
        assert!(
            !dialog.options.delete_branch,
            "With default config (delete_branch_on_cleanup: false), delete_branch should be false"
        );
        assert!(
            dialog.options.delete_sandbox,
            "With default config (auto_cleanup: true), delete_sandbox should be true"
        );
    }

    #[test]
    fn test_tab_cycles_through_elements() {
        let mut dialog = full_dialog();
        assert_eq!(dialog.focus, FocusElement::WorktreeCheckbox);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focus, FocusElement::ForceCheckbox);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focus, FocusElement::BranchCheckbox);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focus, FocusElement::SandboxCheckbox);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focus, FocusElement::YesButton);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focus, FocusElement::NoButton);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focus, FocusElement::WorktreeCheckbox);
    }

    #[test]
    fn test_branch_checkbox_toggle() {
        let mut dialog = full_dialog();
        dialog.focus = FocusElement::BranchCheckbox;
        let initial = dialog.options.delete_branch;

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(dialog.options.delete_branch, !initial);

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(dialog.options.delete_branch, initial);
    }

    #[test]
    fn test_space_toggles_checkbox() {
        let mut dialog = full_dialog();
        let initial = dialog.options.delete_worktree;

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(dialog.options.delete_worktree, !initial);

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(dialog.options.delete_worktree, initial);
    }

    #[test]
    fn test_esc_cancels() {
        let mut dialog = full_dialog();
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_n_cancels() {
        let mut dialog = full_dialog();
        let result = dialog.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_y_confirms() {
        let mut dialog = full_dialog();
        let result = dialog.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(result, DialogResult::Submit(_)));
    }

    #[test]
    fn test_enter_on_no_cancels() {
        let mut dialog = simple_dialog();
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_on_yes_submits() {
        let mut dialog = simple_dialog();
        dialog.focus = FocusElement::YesButton;
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(_)));
    }

    #[test]
    fn test_left_focuses_yes() {
        let mut dialog = simple_dialog();
        dialog.handle_key(key(KeyCode::Left));
        assert_eq!(dialog.focus, FocusElement::YesButton);
    }

    #[test]
    fn test_right_focuses_no() {
        let mut dialog = simple_dialog();
        dialog.focus = FocusElement::YesButton;
        dialog.handle_key(key(KeyCode::Right));
        assert_eq!(dialog.focus, FocusElement::NoButton);
    }

    #[test]
    fn test_click_before_render_is_noop() {
        // Both button rects default to Rect::default() (zero-sized) so
        // the contains() check returns false until the dialog has been
        // painted at least once.
        let mut dialog = simple_dialog();
        assert!(dialog.handle_click(5, 5).is_none());
    }

    #[test]
    fn test_click_on_yes_button_submits() {
        let mut dialog = simple_dialog();
        // Stage the button rects manually since the real coordinates
        // come from render(), which a unit test can't easily invoke.
        dialog.yes_button_area = Rect::new(10, 8, 5, 1);
        dialog.no_button_area = Rect::new(19, 8, 4, 1);

        let result = dialog.handle_click(12, 8).expect("yes hit");
        assert!(matches!(result, DialogResult::Submit(_)));
    }

    #[test]
    fn test_click_on_no_button_cancels() {
        let mut dialog = simple_dialog();
        dialog.yes_button_area = Rect::new(10, 8, 5, 1);
        dialog.no_button_area = Rect::new(19, 8, 4, 1);

        let result = dialog.handle_click(20, 8).expect("no hit");
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_click_between_buttons_misses() {
        let mut dialog = simple_dialog();
        dialog.yes_button_area = Rect::new(10, 8, 5, 1);
        dialog.no_button_area = Rect::new(19, 8, 4, 1);
        // The four-space gap between "[Yes]" and "[No]" is dead space.
        assert!(dialog.handle_click(16, 8).is_none());
    }

    #[test]
    fn test_scratch_dialog_focuses_keep_scratch_checkbox() {
        let dialog = scratch_dialog();
        assert_eq!(dialog.focus, FocusElement::KeepScratchCheckbox);
        assert!(!dialog.options.keep_scratch, "default must be off");
    }

    #[test]
    fn test_scratch_dialog_toggles_keep_scratch_on_space() {
        let mut dialog = scratch_dialog();
        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(dialog.options.keep_scratch);
        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(!dialog.options.keep_scratch);
    }

    #[test]
    fn test_submit_returns_options() {
        let mut dialog = full_dialog();
        dialog.options.delete_worktree = true;
        dialog.options.force_delete = true;
        dialog.options.delete_branch = true;
        dialog.options.delete_sandbox = true;

        let result = dialog.handle_key(key(KeyCode::Char('y')));
        match result {
            DialogResult::Submit(opts) => {
                assert!(opts.delete_worktree);
                assert!(opts.force_delete);
                assert!(opts.delete_branch);
                assert!(opts.delete_sandbox);
            }
            _ => panic!("Expected Submit"),
        }
    }

    /// Stage button + checkbox rects manually (the real ones come from
    /// `render`, which is impractical to invoke in a unit test).
    fn stage_rects_for_simple(dialog: &mut UnifiedDeleteDialog) {
        dialog.yes_button_area = Rect::new(10, 8, 5, 1);
        dialog.no_button_area = Rect::new(19, 8, 4, 1);
        // simple_dialog has no worktree/sandbox/scratch, so the only
        // focusable elements are the two buttons; no checkbox rects.
    }

    fn stage_rects_for_full(dialog: &mut UnifiedDeleteDialog) {
        dialog.focusable_rects.clear();
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
        dialog.yes_button_area = Rect::new(10, 10, 5, 1);
        dialog.no_button_area = Rect::new(19, 10, 4, 1);
    }

    #[test]
    fn hover_highlights_button_without_changing_focus() {
        // Hover drives the visual highlight only; it must leave focus
        // alone, otherwise mouse drift between reading the dialog and
        // pressing Enter / Space silently shifts which element the next
        // keystroke targets.
        let mut dialog = simple_dialog();
        stage_rects_for_simple(&mut dialog);
        dialog.focus = FocusElement::NoButton;
        let yes = dialog.yes_button_area;
        let no = dialog.no_button_area;

        // Over Yes: highlight Yes, focus stays put.
        assert!(dialog.handle_hover(12, 8));
        assert_eq!(dialog.hover.current(), Some(yes));
        assert_eq!(dialog.focus, FocusElement::NoButton);

        // Over No: highlight follows, focus still unchanged.
        assert!(dialog.handle_hover(20, 8));
        assert_eq!(dialog.hover.current(), Some(no));
        assert_eq!(dialog.focus, FocusElement::NoButton);

        // Same cell again: nothing changed, so no redraw is requested.
        assert!(!dialog.handle_hover(20, 8));

        // Off the buttons: highlight clears, focus unchanged.
        assert!(dialog.handle_hover(50, 50));
        assert_eq!(dialog.hover.current(), None);
        assert_eq!(dialog.focus, FocusElement::NoButton);
    }

    #[test]
    fn hover_on_checkbox_row_highlights_without_stealing_focus() {
        let mut dialog = full_dialog();
        stage_rects_for_full(&mut dialog);
        dialog.focus = FocusElement::YesButton;
        // (10, 5) lands on the branch checkbox row staged by the helper.
        assert!(dialog.handle_hover(10, 5));
        assert_eq!(dialog.hover.current(), Some(Rect::new(5, 5, 30, 1)));
        assert_eq!(
            dialog.focus,
            FocusElement::YesButton,
            "hover must not move keyboard focus"
        );
    }

    #[test]
    fn click_on_checkbox_toggles_and_focuses() {
        let mut dialog = full_dialog();
        stage_rects_for_full(&mut dialog);
        let before = dialog.options.delete_branch;
        let result = dialog
            .handle_click(10, 5)
            .expect("checkbox click should return Continue");
        assert!(matches!(result, DialogResult::Continue));
        assert_eq!(dialog.focus, FocusElement::BranchCheckbox);
        assert_eq!(dialog.options.delete_branch, !before);
    }

    #[test]
    fn click_on_worktree_checkbox_toggles_and_rebuilds_focusables() {
        let mut dialog = full_dialog();
        // Force a known starting state so the test doesn't depend on
        // whatever the default config's `worktree.auto_cleanup` is.
        dialog.options.delete_worktree = true;
        dialog.options.force_delete = true;
        dialog.rebuild_focusable_elements();
        stage_rects_for_full(&mut dialog);

        let before_focusables = dialog.focusable_elements.len();
        let result = dialog
            .handle_click(10, 3)
            .expect("worktree click should return Continue");
        assert!(matches!(result, DialogResult::Continue));
        assert!(
            !dialog.options.delete_worktree,
            "worktree click should toggle the option off"
        );
        assert!(
            !dialog.options.force_delete,
            "turning worktree off also clears force_delete"
        );
        assert!(
            dialog.focusable_elements.len() < before_focusables,
            "ForceCheckbox should drop out of focusables when worktree is off"
        );
    }
}
