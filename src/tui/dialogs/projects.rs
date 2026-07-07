//! Projects panel: list/add/remove the project registry from the TUI home screen.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::{DialogResult, InfoDialog};
use crate::session::config::{save_config, Config};
use crate::session::projects;
use crate::session::{Project, ProjectScope};
use crate::tui::components::set_prefixed_input_cursor_position;
use crate::tui::styles::Theme;

#[derive(Copy, Clone, PartialEq, Eq)]
enum Mode {
    Browse,
    Adding,
}

pub struct ProjectsDialog {
    profile: String,
    items: Vec<Project>,
    selected: usize,
    mode: Mode,
    /// Path input when adding
    add_input: Input,
    /// Optional default base branch input when adding
    add_base_branch: Input,
    /// Scope selection when adding (Global vs Profile)
    add_scope: ProjectScope,
    /// Allow registering even if path is already in the other scope.
    add_allow_override: bool,
    /// Cursor field while adding: 0=path, 1=base-branch, 2=scope, 3=allow-override
    add_focused: usize,
    error: Option<String>,
    info: Option<String>,
    /// One-time notice shown on top of the dialog after registering a non-git
    /// directory, explaining that git features are unavailable. Gated by
    /// `app_state.has_seen_non_git_project_warning` so it appears once.
    non_git_notice: Option<InfoDialog>,
    /// Close the dialog when Esc cancels the add form opened from a direct flow.
    close_on_add_cancel: bool,
}

impl ProjectsDialog {
    pub fn new(profile: &str) -> Self {
        let mut dialog = Self {
            profile: profile.to_string(),
            items: Vec::new(),
            selected: 0,
            mode: Mode::Browse,
            add_input: Input::default(),
            add_base_branch: Input::default(),
            add_scope: ProjectScope::Global,
            add_allow_override: false,
            add_focused: 0,
            error: None,
            info: None,
            non_git_notice: None,
            close_on_add_cancel: false,
        };
        dialog.reload();
        dialog
    }

    pub fn new_adding(profile: &str) -> Self {
        let mut dialog = Self::new(profile);
        dialog.enter_add_mode(true);
        dialog
    }

    fn enter_add_mode(&mut self, close_on_cancel: bool) {
        self.mode = Mode::Adding;
        self.add_input = Input::default();
        self.add_base_branch = Input::default();
        self.add_scope = ProjectScope::Global;
        self.add_allow_override = false;
        self.add_focused = 0;
        self.error = None;
        self.close_on_add_cancel = close_on_cancel;
    }

    fn reload(&mut self) {
        match projects::load_merged(&self.profile) {
            Ok(items) => {
                self.items = items;
                if self.selected >= self.items.len() {
                    self.selected = self.items.len().saturating_sub(1);
                }
                self.error = None;
            }
            Err(e) => {
                self.error = Some(format!("Failed to load projects: {}", e));
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        // The one-time non-git notice sits on top of the dialog; while it is up,
        // keys dismiss it rather than driving the list or add form.
        if let Some(notice) = &mut self.non_git_notice {
            if matches!(notice.handle_key(key), DialogResult::Cancel) {
                self.non_git_notice = None;
            }
            return DialogResult::Continue;
        }
        self.info = None;
        match self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::Adding => self.handle_add_key(key),
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.items.is_empty() {
                    self.selected = (self.selected + 1).min(self.items.len() - 1);
                }
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                DialogResult::Continue
            }
            KeyCode::Char('a') => {
                self.enter_add_mode(false);
                DialogResult::Continue
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(project) = self.items.get(self.selected).cloned() {
                    match projects::remove(&self.profile, project.scope, &project.name) {
                        Ok(_) => {
                            self.info = Some(format!("Removed '{}'", project.name));
                            self.reload();
                        }
                        Err(e) => self.error = Some(format!("Remove failed: {}", e)),
                    }
                }
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn handle_add_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc => {
                if self.close_on_add_cancel {
                    return DialogResult::Cancel;
                }
                self.mode = Mode::Browse;
                self.error = None;
                self.close_on_add_cancel = false;
                DialogResult::Continue
            }
            KeyCode::Tab => {
                self.add_focused = (self.add_focused + 1) % 4;
                DialogResult::Continue
            }
            KeyCode::BackTab => {
                self.add_focused = (self.add_focused + 3) % 4;
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.add_focused == 2 => {
                self.add_scope = match self.add_scope {
                    ProjectScope::Global => ProjectScope::Profile,
                    ProjectScope::Profile => ProjectScope::Global,
                };
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.add_focused == 3 => {
                self.add_allow_override = !self.add_allow_override;
                DialogResult::Continue
            }
            KeyCode::Enter => {
                let path = self.add_input.value().trim().to_string();
                if path.is_empty() {
                    self.error = Some("Path required".into());
                    return DialogResult::Continue;
                }
                let path_buf = std::path::PathBuf::from(&path);
                let canonical = path_buf.canonicalize().unwrap_or_else(|_| path_buf.clone());
                // Non-git directories are allowed (sessions run in place); only
                // reject paths that don't resolve to a directory.
                if !canonical.is_dir() {
                    self.error = Some(format!(
                        "Path does not exist or is not a directory: {}",
                        canonical.display()
                    ));
                    return DialogResult::Continue;
                }
                let name = canonical
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "project".to_string());
                let base_branch = {
                    let b = self.add_base_branch.value().trim();
                    if b.is_empty() {
                        None
                    } else {
                        Some(b.to_string())
                    }
                };
                let project =
                    Project::new(name.clone(), canonical.to_string_lossy(), self.add_scope)
                        .with_base_branch(base_branch);
                let is_git = project.is_git();
                match projects::add(
                    &self.profile,
                    self.add_scope,
                    project,
                    self.add_allow_override,
                ) {
                    Ok(saved) => {
                        let saved_name = saved.name.clone();
                        self.info = Some(format!(
                            "Added '{}' [{}]",
                            saved.name,
                            self.add_scope.as_str()
                        ));
                        self.mode = Mode::Browse;
                        self.add_input = Input::default();
                        self.add_base_branch = Input::default();
                        self.close_on_add_cancel = false;
                        self.reload();
                        if !is_git {
                            self.maybe_warn_non_git(&saved_name);
                        }
                    }
                    Err(e) => self.error = Some(format!("Add failed: {}", e)),
                }
                DialogResult::Continue
            }
            _ => {
                if !key.modifiers.contains(KeyModifiers::CONTROL) {
                    match self.add_focused {
                        0 => self
                            .add_input
                            .handle_event(&crossterm::event::Event::Key(key)),
                        1 => self
                            .add_base_branch
                            .handle_event(&crossterm::event::Event::Key(key)),
                        _ => None,
                    };
                }
                DialogResult::Continue
            }
        }
    }

    /// Show the one-time "not a git repository" notice, unless the user has
    /// already seen it. Latches `app_state.has_seen_non_git_project_warning` so
    /// it never repeats.
    ///
    /// Reads the latch with `Config::load()` rather than `load_or_warn()`: the
    /// latter falls back to a default `Config` when an existing file fails to
    /// parse, and persisting that would atomically overwrite the user's real
    /// config with defaults. On a load failure we show the notice (harmless to
    /// repeat) but skip persistence entirely.
    fn maybe_warn_non_git(&mut self, project_name: &str) {
        let config = Config::load().ok();
        if config
            .as_ref()
            .is_some_and(|c| c.app_state.has_seen_non_git_project_warning)
        {
            return;
        }
        self.non_git_notice = Some(InfoDialog::sized_to_fit(
            "Not a Git Repository",
            &format!(
                "'{project_name}' isn't a git repository. Agent sessions will open \
                 directly in this folder. Git features (a separate worktree per \
                 session, branches, and the diff view) won't be available here."
            ),
        ));
        if let Some(mut config) = config {
            config.app_state.has_seen_non_git_project_warning = true;
            let _ = save_config(&config);
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_width: u16 = 76;
        let list_height: u16 = (self.items.len() as u16).clamp(3, 12);
        let adding_extra: u16 = if matches!(self.mode, Mode::Adding) {
            3
        } else {
            0
        };
        let dialog_height: u16 = list_height + 9 + adding_extra;
        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Projects ")
            .title_style(Style::default().fg(theme.title).bold());
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let constraints = vec![
            Constraint::Length(list_height),
            Constraint::Length(1),
            Constraint::Length(if matches!(self.mode, Mode::Adding) {
                7
            } else {
                1
            }),
            Constraint::Min(1),
        ];
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        // Project list
        if self.items.is_empty() {
            let p = Paragraph::new("No registered projects. Press 'a' to add one.")
                .style(Style::default().fg(theme.dimmed));
            frame.render_widget(p, chunks[0]);
        } else {
            let lines: Vec<Line> = self
                .items
                .iter()
                .enumerate()
                .map(|(idx, project)| {
                    let style = if idx == self.selected {
                        Style::default().fg(theme.accent).bold()
                    } else {
                        Style::default().fg(theme.text)
                    };
                    let scope_style = if idx == self.selected {
                        Style::default().fg(theme.accent)
                    } else {
                        Style::default().fg(theme.dimmed)
                    };
                    let mut spans = vec![
                        Span::styled(if idx == self.selected { "› " } else { "  " }, style),
                        Span::styled(project.name.clone(), style),
                        Span::raw(" "),
                        Span::styled(format!("[{}]", project.scope.as_str()), scope_style),
                        Span::raw("  "),
                        Span::styled(project.path.clone(), Style::default().fg(theme.dimmed)),
                    ];
                    if let Some(base) = &project.default_base_branch {
                        spans.push(Span::styled(
                            format!("  base:{}", base),
                            Style::default().fg(theme.dimmed),
                        ));
                    }
                    Line::from(spans)
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), chunks[0]);
        }

        // Separator
        frame.render_widget(
            Paragraph::new("─".repeat(inner.width as usize))
                .style(Style::default().fg(theme.dimmed)),
            chunks[1],
        );

        // Add form or status line
        match self.mode {
            Mode::Browse => {
                let mut spans = vec![];
                if let Some(err) = &self.error {
                    spans.push(Span::styled(err.clone(), Style::default().fg(theme.error)));
                } else if let Some(info) = &self.info {
                    spans.push(Span::styled(
                        info.clone(),
                        Style::default().fg(theme.accent),
                    ));
                }
                frame.render_widget(Paragraph::new(Line::from(spans)), chunks[2]);
            }
            Mode::Adding => {
                let path_label_style = if self.add_focused == 0 {
                    Style::default().fg(theme.accent).underlined()
                } else {
                    Style::default().fg(theme.text)
                };
                let path_line = Line::from(vec![
                    Span::styled("Path: ", path_label_style),
                    Span::styled(
                        self.add_input.value().to_string(),
                        Style::default().fg(theme.text),
                    ),
                    if self.add_focused == 0 {
                        Span::styled("█", Style::default().fg(theme.accent))
                    } else {
                        Span::raw("")
                    },
                ]);
                let base_label_style = if self.add_focused == 1 {
                    Style::default().fg(theme.accent).underlined()
                } else {
                    Style::default().fg(theme.text)
                };
                let base_line = Line::from(vec![
                    Span::styled("Base branch: ", base_label_style),
                    Span::styled(
                        self.add_base_branch.value().to_string(),
                        Style::default().fg(theme.text),
                    ),
                    if self.add_focused == 1 {
                        Span::styled("█", Style::default().fg(theme.accent))
                    } else if self.add_base_branch.value().is_empty() {
                        Span::styled("(auto-detect)", Style::default().fg(theme.dimmed))
                    } else {
                        Span::raw("")
                    },
                ]);
                let scope_label_style = if self.add_focused == 2 {
                    Style::default().fg(theme.accent).underlined()
                } else {
                    Style::default().fg(theme.text)
                };
                let scope_value = match self.add_scope {
                    ProjectScope::Global => "global (all profiles)",
                    ProjectScope::Profile => "profile-only",
                };
                let scope_line = Line::from(vec![
                    Span::styled("Scope: ", scope_label_style),
                    Span::styled(
                        format!("< {} >", scope_value),
                        Style::default().fg(theme.accent).bold(),
                    ),
                ]);
                let override_label_style = if self.add_focused == 3 {
                    Style::default().fg(theme.accent).underlined()
                } else {
                    Style::default().fg(theme.text)
                };
                let override_box = if self.add_allow_override {
                    "[x]"
                } else {
                    "[ ]"
                };
                let override_line = Line::from(vec![
                    Span::styled("Override: ", override_label_style),
                    Span::styled(
                        format!("{} allow shadowing other scope", override_box),
                        Style::default().fg(theme.accent).bold(),
                    ),
                ]);
                let mut lines = vec![path_line, base_line, scope_line, override_line];
                if let Some(err) = &self.error {
                    lines.push(Line::from(Span::styled(
                        err.clone(),
                        Style::default().fg(theme.error),
                    )));
                }
                frame.render_widget(Paragraph::new(lines), chunks[2]);
                // The real terminal cursor follows the focused text field. Each
                // field renders on its own row within chunks[2], so offset the
                // 1-row cursor rect by the field's line index.
                let row = |offset: u16| Rect {
                    y: chunks[2].y.saturating_add(offset),
                    height: 1,
                    ..chunks[2]
                };
                if self.add_focused == 0 {
                    set_prefixed_input_cursor_position(frame, row(0), "Path: ", &self.add_input);
                } else if self.add_focused == 1 {
                    set_prefixed_input_cursor_position(
                        frame,
                        row(1),
                        "Base branch: ",
                        &self.add_base_branch,
                    );
                }
            }
        }

        // Hints
        let hint_spans: Vec<Span> = match self.mode {
            Mode::Browse => vec![
                Span::styled("a", Style::default().fg(theme.hint)),
                Span::raw(" add  "),
                Span::styled("d", Style::default().fg(theme.hint)),
                Span::raw(" remove  "),
                Span::styled("j/k", Style::default().fg(theme.hint)),
                Span::raw(" move  "),
                Span::styled("q/Esc", Style::default().fg(theme.hint)),
                Span::raw(" close"),
            ],
            Mode::Adding => vec![
                Span::styled("Tab", Style::default().fg(theme.hint)),
                Span::raw(" next  "),
                Span::styled("Space/←/→", Style::default().fg(theme.hint)),
                Span::raw(" toggle  "),
                Span::styled("Enter", Style::default().fg(theme.hint)),
                Span::raw(" save  "),
                Span::styled("Esc", Style::default().fg(theme.hint)),
                Span::raw(" cancel"),
            ],
        };
        frame.render_widget(Paragraph::new(Line::from(hint_spans)), chunks[3]);

        // The one-time non-git notice renders last so it sits on top of the
        // projects dialog body.
        if let Some(notice) = &mut self.non_git_notice {
            notice.render(frame, area, theme);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use serial_test::serial;
    use tempfile::tempdir;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn isolate_home(temp: &std::path::Path) {
        std::env::set_var("HOME", temp);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"));
    }

    /// Drive the dialog through an add of `dir`: enter add mode, set the path
    /// input directly (typing char-by-char is unnecessary for this logic), and
    /// submit.
    fn add_dir(dialog: &mut ProjectsDialog, dir: &std::path::Path) {
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.add_input = Input::new(dir.to_string_lossy().to_string());
        dialog.handle_key(key(KeyCode::Enter));
    }

    #[test]
    #[serial]
    fn non_git_add_shows_notice_once_then_latches() {
        let temp = tempdir().unwrap();
        isolate_home(temp.path());

        let mut dialog = ProjectsDialog::new("test");

        // First non-git add pops the one-time notice.
        let plain = temp.path().join("plain-one");
        std::fs::create_dir_all(&plain).unwrap();
        add_dir(&mut dialog, &plain);
        let notice = dialog
            .non_git_notice
            .as_ref()
            .expect("non-git add should show the notice");
        assert_eq!(notice.title(), "Not a Git Repository");

        // Enter dismisses it.
        dialog.handle_key(key(KeyCode::Enter));
        assert!(dialog.non_git_notice.is_none(), "Enter should dismiss");

        // A second non-git add does not re-show it: the persisted flag latched.
        let plain2 = temp.path().join("plain-two");
        std::fs::create_dir_all(&plain2).unwrap();
        add_dir(&mut dialog, &plain2);
        assert!(
            dialog.non_git_notice.is_none(),
            "notice must not repeat once seen"
        );
    }

    /// A config file that fails to parse must not be clobbered with defaults
    /// when a non-git project is added: persistence is skipped on load failure,
    /// and the notice still shows (harmless to repeat).
    #[test]
    #[serial]
    fn malformed_config_is_not_clobbered_on_non_git_add() {
        let temp = tempdir().unwrap();
        isolate_home(temp.path());

        // First add creates a real config.toml (with the latch set).
        let mut dialog = ProjectsDialog::new("test");
        let first = temp.path().join("first");
        std::fs::create_dir_all(&first).unwrap();
        add_dir(&mut dialog, &first);
        let cfg_path = crate::session::config::config_path().expect("config path");
        assert!(cfg_path.exists(), "first add should write a config");

        // Corrupt it so Config::load() returns Err.
        let garbage = "this is = not ] valid [[ toml";
        std::fs::write(&cfg_path, garbage).unwrap();

        // A second non-git add must NOT overwrite the corrupt file...
        let mut dialog = ProjectsDialog::new("test");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&second).unwrap();
        add_dir(&mut dialog, &second);
        assert_eq!(
            std::fs::read_to_string(&cfg_path).unwrap(),
            garbage,
            "malformed config must be left untouched"
        );
        // ...and, unable to confirm the latch, it shows the notice.
        assert!(
            dialog.non_git_notice.is_some(),
            "notice should show when the latch can't be read"
        );
    }

    #[test]
    #[serial]
    fn git_add_shows_no_notice() {
        let temp = tempdir().unwrap();
        isolate_home(temp.path());

        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git2::Repository::init(&repo).expect("git init");

        let mut dialog = ProjectsDialog::new("test");
        add_dir(&mut dialog, &repo);
        assert!(
            dialog.non_git_notice.is_none(),
            "a git repo add should not warn"
        );
    }
}
