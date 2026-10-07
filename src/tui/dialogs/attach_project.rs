//! Attach-project picker: add a registered project to an existing session.
//! Drawing on the registry (as the new-session extra-repo picker does) means a
//! repo you can start a session on is one you can attach. An unregistered path
//! goes through `aoe session add-project <path>`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::Project;
use crate::tui::styles::Theme;

pub struct AttachProjectDialog {
    /// Registered projects, minus the ones this session already has.
    options: Vec<Project>,
    selected: usize,
    /// Session the pick applies to, so the caller re-resolves nothing.
    session_id: String,
    session_title: String,
    list_area: Rect,
    dialog_area: Rect,
}

impl AttachProjectDialog {
    pub fn new(session_id: String, session_title: String, options: Vec<Project>) -> Self {
        Self {
            options,
            selected: 0,
            session_id,
            session_title,
            list_area: Rect::default(),
            dialog_area: Rect::default(),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Whether there is anything to pick; if not, the dialog shows guidance.
    pub fn is_empty(&self) -> bool {
        self.options.is_empty()
    }

    fn row_to_idx(&self, col: u16, row: u16) -> Option<usize> {
        if !self
            .list_area
            .contains(ratatui::layout::Position::from((col, row)))
        {
            return None;
        }
        let i = (row - self.list_area.y) as usize;
        if i >= self.options.len() {
            return None;
        }
        Some(i)
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<Project> {
        if !self
            .dialog_area
            .contains(ratatui::layout::Position::from((col, row)))
        {
            return DialogResult::Cancel;
        }
        let Some(idx) = self.row_to_idx(col, row) else {
            return DialogResult::Continue;
        };
        self.selected = idx;
        DialogResult::Submit(self.options[idx].clone())
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let Some(idx) = self.row_to_idx(col, row) else {
            return false;
        };
        if self.selected == idx {
            return false;
        }
        self.selected = idx;
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<Project> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.options.len() {
                    self.selected += 1;
                }
                DialogResult::Continue
            }
            KeyCode::Enter => match self.options.get(self.selected) {
                Some(project) => DialogResult::Submit(project.clone()),
                None => DialogResult::Cancel,
            },
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let widest = self
            .options
            .iter()
            .map(|p| p.name.chars().count() + p.path.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let dialog_width: u16 = (widest + 10).clamp(40, 72);
        // Two extra rows over the list: the restart warning and the key hint.
        let dialog_height: u16 = (self.options.len().max(3) as u16 + 6).min(20);

        let block = super::dialog_block(format!(" Add Project to {} ", self.session_title), theme);
        let (dialog_area, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);
        self.dialog_area = dialog_area;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);
        self.list_area = chunks[0];

        if self.options.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No other registered projects. Add one with 'p' on the home screen, or run \
                     'aoe session add-project <session> <path>'.",
                )
                .style(Style::default().fg(theme.text))
                .wrap(Wrap { trim: true }),
                chunks[0],
            );
            // No rows, so clicks must not resolve to an index.
            self.list_area = Rect::default();
        } else {
            let mut lines: Vec<Line> = Vec::new();
            for (i, p) in self.options.iter().enumerate() {
                let is_selected = i == self.selected;
                let prefix = if is_selected { "> " } else { "  " };
                let name_style = if is_selected {
                    Style::default().fg(theme.accent).bold()
                } else {
                    Style::default().fg(theme.text)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, name_style),
                    Span::styled(p.name.clone(), name_style),
                    Span::styled(format!("  {}", p.path), Style::default().fg(theme.hint)),
                ]));
            }
            frame.render_widget(Paragraph::new(lines), chunks[0]);
        }

        frame.render_widget(
            Paragraph::new("May restart; resume not guaranteed")
                .style(Style::default().fg(theme.waiting)),
            chunks[1],
        );

        let hint = Line::from(vec![
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" attach  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" close"),
        ]);
        frame.render_widget(Paragraph::new(hint), chunks[2]);
    }
}
