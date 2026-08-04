//! Skills manager: list every discovered skill (the AoE-managed store plus
//! every host-discovered agent skills directory), view a skill's `SKILL.md`,
//! and run the managed-skill lifecycle in-TUI: create, edit, delete, adopt a
//! host skill into the managed store, and share every managed skill out to
//! every agent's skills directory. The TUI twin of `aoe skill` and the
//! backend model in `crate::session::skills_model`.
//!
//! `home`/`app_dir` are resolved once at construction and reused for every
//! action rather than re-resolved per keypress, so the mutating helpers below
//! take no I/O-resolution path of their own (and so a test can hand them a
//! tempdir directly).

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use ratatui_textarea::TextArea;

use super::{centered_rect, DialogResult};
use crate::session::skills_model::{self, DiscoveredSkill, SkillError, SyncOutcome, SyncStatus};
use crate::tui::styles::Theme;
use crate::tui::worker::Worker;

/// One share request handed to the worker thread.
struct SyncRequest {
    home: PathBuf,
    app_dir: PathBuf,
}

/// The floating popup owning the keyboard; at most one at a time.
enum Popup {
    /// Read-only view of a skill's `SKILL.md` (Enter on a row).
    View { content: String, scroll: u16 },
    /// Editing a managed skill's `SKILL.md`.
    Edit {
        directory: String,
        text_area: Box<TextArea<'static>>,
    },
    /// Creating a new managed skill: the directory name being typed.
    Create { name: String },
    /// Confirming deletion of a managed skill.
    ConfirmDelete { directory: String },
}

pub struct SkillsManagerDialog {
    rows: Vec<DiscoveredSkill>,
    selected: usize,
    info: Option<String>,
    popup: Option<Popup>,
    home: PathBuf,
    app_dir: PathBuf,
    /// Spawned on the first share, so a panel that is only browsed never
    /// starts a thread. Reconciling every skill against every root walks and
    /// digests whole packages, so it cannot run on the thread that draws
    /// frames and reads keys without freezing both.
    sync_worker: Option<Worker<SyncRequest, Vec<SyncOutcome>>>,
    syncing: bool,
}

impl Default for SkillsManagerDialog {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a [`SkillError`] to the message shown in `info`, the same wording the
/// CLI and the web API surface for the same failures.
fn describe_skill_error(error: SkillError) -> String {
    match error {
        SkillError::InvalidInput(m)
        | SkillError::NotFound(m)
        | SkillError::Collision(m)
        | SkillError::ReadOnly(m) => m,
        SkillError::Io(e) => format!("{e:#}"),
    }
}

/// Compact counts line for a share, e.g. "Shared: 3 created, 1 conflict(s)."
fn summarize_sync(outcomes: &[SyncOutcome]) -> String {
    let mut counts = [0usize; 6];
    for outcome in outcomes {
        let idx = match outcome.status {
            SyncStatus::Created => 0,
            SyncStatus::Updated => 1,
            SyncStatus::Unchanged => 2,
            SyncStatus::Removed => 3,
            SyncStatus::Conflict => 4,
            SyncStatus::Error => 5,
        };
        counts[idx] += 1;
    }
    let labels = [
        "created",
        "updated",
        "unchanged",
        "removed",
        "conflict(s)",
        "error(s)",
    ];
    let parts: Vec<String> = counts
        .iter()
        .zip(labels)
        .filter(|(count, _)| **count > 0)
        .map(|(count, label)| format!("{count} {label}"))
        .collect();
    if parts.is_empty() {
        "Nothing to share.".to_string()
    } else {
        format!("Shared: {}.", parts.join(", "))
    }
}

impl SkillsManagerDialog {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let app_dir = crate::session::get_app_dir().unwrap_or_default();
        let mut dialog = Self {
            rows: Vec::new(),
            selected: 0,
            info: None,
            popup: None,
            home,
            app_dir,
            sync_worker: None,
            syncing: false,
        };
        dialog.reload();
        dialog
    }

    /// Drain a finished share. Returns whether anything changed, so the caller
    /// only redraws when it must, matching the other pollers.
    pub fn tick(&mut self) -> bool {
        let Some(worker) = &self.sync_worker else {
            return false;
        };
        let Ok(outcomes) = worker.try_recv() else {
            return false;
        };
        self.syncing = false;
        self.reload_after(summarize_sync(&outcomes));
        true
    }

    fn reload(&mut self) {
        self.rows = skills_model::discover(&self.home, &self.app_dir);
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    fn reload_after(&mut self, message: String) {
        self.reload();
        self.info = Some(message);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        self.info = None;
        if self.popup.is_some() {
            return self.handle_popup_key(key);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.rows.is_empty() {
                    self.selected = (self.selected + 1).min(self.rows.len() - 1);
                }
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                DialogResult::Continue
            }
            KeyCode::Enter => {
                self.open_view();
                DialogResult::Continue
            }
            KeyCode::Char('e') => {
                self.start_edit();
                DialogResult::Continue
            }
            KeyCode::Char('a') => {
                self.start_adopt();
                DialogResult::Continue
            }
            KeyCode::Char('n') => {
                self.popup = Some(Popup::Create {
                    name: String::new(),
                });
                DialogResult::Continue
            }
            KeyCode::Char('x') => {
                self.start_delete();
                DialogResult::Continue
            }
            KeyCode::Char('s') => {
                self.share_all();
                DialogResult::Continue
            }
            KeyCode::Char('r') => {
                self.reload();
                self.info = Some("Refreshed.".to_string());
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    /// Take a bracketed paste. Only the editor and the create prompt accept
    /// text; anywhere else in the panel a paste is a no-op, which still has to
    /// be swallowed here rather than falling through to the home view's other
    /// dialogs while this one is open.
    pub fn handle_paste(&mut self, text: &str) {
        match &mut self.popup {
            Some(Popup::Edit { text_area, .. }) => {
                text_area.insert_str(text);
            }
            Some(Popup::Create { name }) => {
                // A directory name is one line, so a multi-line paste takes its
                // first line rather than smuggling newlines into a path.
                name.push_str(text.lines().next().unwrap_or_default());
            }
            _ => {}
        }
    }

    fn handle_popup_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        let Some(popup) = self.popup.take() else {
            return DialogResult::Continue;
        };
        match popup {
            Popup::View { content, scroll } => self.handle_view_key(key, content, scroll),
            Popup::Edit {
                directory,
                text_area,
            } => self.handle_edit_key(key, directory, text_area),
            Popup::Create { name } => self.handle_create_key(key, name),
            Popup::ConfirmDelete { directory } => self.handle_confirm_delete_key(key, directory),
        }
    }

    fn handle_view_key(&mut self, key: KeyEvent, content: String, scroll: u16) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {}
            KeyCode::Down | KeyCode::Char('j') => {
                let max = content.lines().count().saturating_sub(1) as u16;
                self.popup = Some(Popup::View {
                    scroll: scroll.saturating_add(1).min(max),
                    content,
                });
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.popup = Some(Popup::View {
                    scroll: scroll.saturating_sub(1),
                    content,
                });
            }
            _ => {
                self.popup = Some(Popup::View { content, scroll });
            }
        }
        DialogResult::Continue
    }

    fn handle_edit_key(
        &mut self,
        key: KeyEvent,
        directory: String,
        mut text_area: Box<TextArea<'static>>,
    ) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let content = text_area.lines().join("\n");
                match skills_model::edit_skill(&self.home, &self.app_dir, &directory, &content) {
                    Ok(()) => self.reload_after(format!("Saved {directory}.")),
                    Err(e) => {
                        self.info = Some(describe_skill_error(e));
                        self.popup = Some(Popup::Edit {
                            directory,
                            text_area,
                        });
                    }
                }
            }
            _ => {
                text_area.input(key);
                self.popup = Some(Popup::Edit {
                    directory,
                    text_area,
                });
            }
        }
        DialogResult::Continue
    }

    fn handle_create_key(&mut self, key: KeyEvent, mut name: String) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let directory = name.trim().to_string();
                if directory.is_empty() {
                    self.info = Some("Enter a directory name.".to_string());
                    self.popup = Some(Popup::Create { name });
                    return DialogResult::Continue;
                }
                match skills_model::create_skill(&self.app_dir, &directory, None) {
                    Ok(()) => self.reload_after(format!("Created {directory}.")),
                    Err(e) => {
                        self.info = Some(describe_skill_error(e));
                        self.popup = Some(Popup::Create { name });
                    }
                }
            }
            KeyCode::Backspace => {
                name.pop();
                self.popup = Some(Popup::Create { name });
            }
            // Only a plain keypress is text. Without this, a chord like Ctrl+U
            // types its letter into the name instead of being ignored.
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                name.push(c);
                self.popup = Some(Popup::Create { name });
            }
            _ => {
                self.popup = Some(Popup::Create { name });
            }
        }
        DialogResult::Continue
    }

    fn handle_confirm_delete_key(&mut self, key: KeyEvent, directory: String) -> DialogResult<()> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                match skills_model::delete_skill(&self.home, &self.app_dir, &directory) {
                    Ok(()) => self.reload_after(format!("Deleted {directory}.")),
                    Err(e) => self.info = Some(describe_skill_error(e)),
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {}
            _ => {
                self.popup = Some(Popup::ConfirmDelete { directory });
            }
        }
        DialogResult::Continue
    }

    /// Open the read-only view popup for the selected row (any provenance).
    fn open_view(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let provenance = row.provenance.clone();
        let directory = row.directory.clone();
        match skills_model::read_skill(&self.home, &self.app_dir, &provenance, &directory) {
            Ok(skill) => {
                self.popup = Some(Popup::View {
                    content: skill.content,
                    scroll: 0,
                });
            }
            Err(e) => self.info = Some(describe_skill_error(e)),
        }
    }

    /// Open the edit popup for the selected row. Only an AoE-managed row is
    /// writable; a host row must be adopted first.
    fn start_edit(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.provenance.is_writable() {
            self.info = Some(format!(
                "{} is {}-managed; adopt it into AoE first.",
                row.directory,
                row.provenance.source_label()
            ));
            return;
        }
        let provenance = row.provenance.clone();
        let directory = row.directory.clone();
        match skills_model::read_skill(&self.home, &self.app_dir, &provenance, &directory) {
            Ok(skill) => {
                let lines: Vec<String> = if skill.content.is_empty() {
                    vec![String::new()]
                } else {
                    skill.content.lines().map(str::to_string).collect()
                };
                let mut text_area = Box::new(TextArea::new(lines));
                text_area.set_cursor_line_style(Style::default());
                self.popup = Some(Popup::Edit {
                    directory,
                    text_area,
                });
            }
            Err(e) => self.info = Some(describe_skill_error(e)),
        }
    }

    /// Adopt the selected row into the managed store. Only a host row can be
    /// adopted; an already-managed row is refused with an explanation.
    fn start_adopt(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.provenance.is_writable() {
            self.info = Some(format!("{} is already AoE-managed.", row.directory));
            return;
        }
        let provenance = row.provenance.clone();
        let directory = row.directory.clone();
        match skills_model::adopt_skill(&self.home, &self.app_dir, &provenance, &directory, None) {
            Ok(dest) => self.reload_after(format!("Adopted {directory} as {dest}.")),
            Err(e) => self.info = Some(describe_skill_error(e)),
        }
    }

    /// Open the delete confirmation for the selected row. Only an AoE-managed
    /// row can be deleted.
    fn start_delete(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.provenance.is_writable() {
            self.info = Some(format!(
                "AoE does not manage {} ({}); nothing to delete.",
                row.directory,
                row.provenance.source_label()
            ));
            return;
        }
        let directory = row.directory.clone();
        self.popup = Some(Popup::ConfirmDelete { directory });
    }

    /// Reconcile every managed skill into every agent's skills directory and
    /// summarize the outcome counts.
    fn share_all(&mut self) {
        if self.syncing {
            self.info = Some("Already sharing.".to_string());
            return;
        }
        let worker = self.sync_worker.get_or_insert_with(|| {
            Worker::spawn("aoe-skills-sync", |request: SyncRequest| {
                skills_model::sync_all_roots(
                    &request.home,
                    &request.app_dir,
                    &skills_model::SyncOptions::default(),
                )
            })
        });
        worker.request(SyncRequest {
            home: self.home.clone(),
            app_dir: self.app_dir.clone(),
        });
        self.syncing = true;
        self.info = Some("Sharing with all agents...".to_string());
    }

    pub fn render(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width.clamp(40, 100);
        let height = area.height.clamp(12, 28);
        let rect = centered_rect(area, width, height);
        f.render_widget(Clear, rect);
        let block = Block::default()
            .title(" Skills ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .padding(Padding::horizontal(1));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.render_list(f, inner, theme);
        match &self.popup {
            Some(Popup::View { content, scroll }) => {
                self.render_view(f, rect, theme, content, *scroll)
            }
            Some(Popup::Edit {
                directory,
                text_area,
            }) => self.render_edit(f, rect, theme, directory, text_area),
            Some(Popup::Create { name }) => self.render_create(f, rect, theme, name),
            Some(Popup::ConfirmDelete { directory }) => {
                self.render_confirm_delete(f, rect, theme, directory)
            }
            None => {}
        }
    }

    fn render_list(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(2)])
            .split(area);

        if self.rows.is_empty() {
            f.render_widget(
                Paragraph::new("No skills found.").style(Style::default().fg(theme.dimmed)),
                chunks[0],
            );
        } else {
            let items: Vec<ListItem> = self
                .rows
                .iter()
                .map(|row| {
                    let spans = vec![
                        Span::styled(
                            format!("{:<24}", row.directory),
                            Style::default().fg(theme.text),
                        ),
                        // AoE's own skills carry the accent so they are
                        // pickable out of the list at a glance, matching the
                        // branded badge the dashboard gives them; every other
                        // source stays dim so the managed ones stand out.
                        Span::styled(
                            format!("{:<10}", row.provenance.source_label()),
                            Style::default().fg(if row.provenance.is_writable() {
                                theme.accent
                            } else {
                                theme.dimmed
                            }),
                        ),
                        Span::styled(format!("{:<24}", row.name), Style::default().fg(theme.text)),
                        Span::styled(row.description.clone(), Style::default().fg(theme.dimmed)),
                    ];
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(items)
                .highlight_style(
                    Style::default()
                        .bg(theme.selection)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ");
            let mut state = ListState::default();
            state.select(Some(self.selected));
            f.render_stateful_widget(list, chunks[0], &mut state);
        }

        self.render_footer(f, chunks[1], theme);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let (text, color) = if let Some(i) = self.info.as_deref() {
            (i.to_string(), theme.waiting)
        } else {
            let mut hints = vec!["enter view", "n new", "s share", "r refresh"];
            if let Some(row) = self.rows.get(self.selected) {
                if row.provenance.is_writable() {
                    hints.push("e edit");
                    hints.push("x delete");
                } else {
                    hints.push("a adopt");
                }
            }
            hints.push("esc close");
            (hints.join(" · "), theme.dimmed)
        };
        let footer = Paragraph::new(text)
            .style(Style::default().fg(color))
            .wrap(Wrap { trim: true });
        f.render_widget(footer, area);
    }

    fn render_view(&self, f: &mut Frame, area: Rect, theme: &Theme, content: &str, scroll: u16) {
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" SKILL.md ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        let body = Paragraph::new(content.to_string())
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        f.render_widget(body, chunks[0]);
        f.render_widget(
            Paragraph::new("j/k scroll · esc close").style(Style::default().fg(theme.dimmed)),
            chunks[1],
        );
    }

    fn render_edit(
        &self,
        f: &mut Frame,
        area: Rect,
        theme: &Theme,
        directory: &str,
        text_area: &TextArea<'static>,
    ) {
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(format!(" Edit {directory} "))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        let mut clone = text_area.clone();
        clone.set_style(Style::default().fg(theme.text));
        clone.set_cursor_style(Style::default().fg(theme.background).bg(theme.accent));
        f.render_widget(&clone, chunks[0]);
        if chunks[0].width > 0 && chunks[0].height > 0 {
            let cursor = clone.screen_cursor();
            let max_x = chunks[0]
                .x
                .saturating_add(chunks[0].width.saturating_sub(1));
            let max_y = chunks[0]
                .y
                .saturating_add(chunks[0].height.saturating_sub(1));
            let cursor_x = chunks[0].x.saturating_add(cursor.col as u16).min(max_x);
            let cursor_y = chunks[0].y.saturating_add(cursor.row as u16).min(max_y);
            f.set_cursor_position(Position::new(cursor_x, cursor_y));
        }
        f.render_widget(
            Paragraph::new("ctrl+s save · esc cancel").style(Style::default().fg(theme.dimmed)),
            chunks[1],
        );
    }

    fn render_create(&self, f: &mut Frame, area: Rect, theme: &Theme, name: &str) {
        let lines = vec![
            Line::from(Span::styled(
                "New skill directory name:",
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("{name}▌"),
                Style::default().fg(theme.accent),
            )),
        ];
        self.draw_notice(
            f,
            area,
            theme,
            " New skill ",
            lines,
            "enter create · esc cancel",
        );
    }

    fn render_confirm_delete(&self, f: &mut Frame, area: Rect, theme: &Theme, directory: &str) {
        let lines = vec![
            Line::from(Span::styled(
                format!("Delete {directory}?"),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Removes it from AoE's managed store.",
                Style::default().fg(theme.dimmed),
            )),
        ];
        self.draw_notice(
            f,
            area,
            theme,
            " Delete skill ",
            lines,
            "y delete · esc cancel",
        );
    }

    /// A small fixed-size centered notice: a couple of body lines plus a
    /// pinned decision-key footer line, for the two popups short enough to
    /// never need scrolling.
    fn draw_notice(
        &self,
        f: &mut Frame,
        area: Rect,
        theme: &Theme,
        title: &str,
        mut lines: Vec<Line>,
        footer: &'static str,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            footer,
            Style::default().fg(theme.dimmed),
        )));
        let width = area.width.clamp(1, 60);
        let height = ((lines.len() as u16).saturating_add(2)).min(area.height);
        let rect = centered_rect(area, width, height);
        f.render_widget(Clear, rect);
        let block = Block::default()
            .title(title.to_string())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .padding(Padding::horizontal(1));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn write_host_skill(home: &std::path::Path, directory: &str) {
        let dir = home.join(".claude/skills").join(directory);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {directory}\ndescription: d\n---\n\nbody\n"),
        )
        .unwrap();
    }

    /// Build a dialog over a fresh tempdir seeded with one AoE-managed skill
    /// (`managed1`) and one host-discovered skill (`host1`), select
    /// `directory`'s row, and press `code`. Returns the resulting `info`
    /// message and whether a popup opened.
    fn press_on(directory: &str, code: KeyCode) -> (Option<String>, bool) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app_dir = tmp.path().join("app");
        skills_model::create_skill(&app_dir, "managed1", Some("d")).unwrap();
        write_host_skill(&home, "host1");

        let mut dialog = SkillsManagerDialog {
            rows: Vec::new(),
            selected: 0,
            info: None,
            popup: None,
            home,
            app_dir,
            sync_worker: None,
            syncing: false,
        };
        dialog.reload();
        dialog.selected = dialog
            .rows
            .iter()
            .position(|r| r.directory == directory)
            .unwrap_or_else(|| panic!("row {directory:?} not discovered"));

        dialog.handle_key(key(code));
        (dialog.info, dialog.popup.is_some())
    }

    /// `e`/`x` are writable-only (AoE-managed rows open a popup; host rows are
    /// refused with an explanation), `a` is the mirror image (host rows adopt
    /// straight through; a managed row is refused as already-managed).
    /// A paste belongs to whatever the panel currently has open, and nowhere
    /// else: with no popup it must be swallowed rather than leaking to the
    /// home view's other dialogs.
    #[test]
    fn paste_lands_in_the_open_popup_only() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app_dir = tmp.path().join("app");
        skills_model::create_skill(&app_dir, "managed1", Some("d")).unwrap();
        let mut dialog = SkillsManagerDialog {
            rows: Vec::new(),
            selected: 0,
            info: None,
            popup: None,
            home,
            app_dir,
            sync_worker: None,
            syncing: false,
        };
        dialog.reload();

        dialog.handle_paste("ignored");
        assert!(dialog.popup.is_none(), "a paste must not open a popup");

        // A directory name is one line, so a multi-line paste is truncated
        // rather than smuggling a newline into a path.
        dialog.popup = Some(Popup::Create {
            name: String::new(),
        });
        dialog.handle_paste("my-skill\nsecond line");
        match &dialog.popup {
            Some(Popup::Create { name }) => assert_eq!(name, "my-skill"),
            _ => panic!("expected the create popup to still be open"),
        }

        dialog.popup = Some(Popup::Edit {
            directory: "managed1".to_string(),
            text_area: Box::new(TextArea::default()),
        });
        dialog.handle_paste("pasted body");
        match &dialog.popup {
            Some(Popup::Edit { text_area, .. }) => {
                assert!(text_area.lines().join("\n").contains("pasted body"));
            }
            _ => panic!("expected the edit popup to still be open"),
        }
    }

    #[test]
    fn provenance_gates_edit_delete_and_adopt() {
        let cases = [
            ('e', "managed1", true, None),
            ('e', "host1", false, Some("adopt it into AoE first")),
            ('x', "managed1", true, None),
            ('x', "host1", false, Some("does not manage")),
            ('a', "host1", false, Some("Adopted")),
            ('a', "managed1", false, Some("already AoE-managed")),
        ];
        for (key_char, directory, expect_popup, info_contains) in cases {
            let (info, popup_open) = press_on(directory, KeyCode::Char(key_char));
            assert_eq!(
                popup_open, expect_popup,
                "key {key_char:?} on {directory:?}: expected popup_open={expect_popup}, got {popup_open}"
            );
            if let Some(expected) = info_contains {
                let info = info.unwrap_or_else(|| {
                    panic!("key {key_char:?} on {directory:?}: expected info containing {expected:?}, got none")
                });
                assert!(
                    info.contains(expected),
                    "key {key_char:?} on {directory:?}: info {info:?} does not contain {expected:?}"
                );
            }
        }
    }
}
