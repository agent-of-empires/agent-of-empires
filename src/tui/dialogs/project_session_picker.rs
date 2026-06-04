//! Picker that lets the user start a brand-new session from a saved project.
//! Wraps the reusable `ListPicker` over the merged project registry; the
//! chosen entry resolves to its on-disk path, which the home view feeds into
//! a pre-filled new-session dialog.

use crossterm::event::KeyEvent;
use ratatui::prelude::*;

use super::project_picker_label;
use super::DialogResult;
use crate::session::Project;
use crate::tui::components::{ListPicker, ListPickerResult};
use crate::tui::styles::Theme;

pub struct ProjectSessionPickerDialog {
    picker: ListPicker,
    projects: Vec<Project>,
}

impl ProjectSessionPickerDialog {
    pub fn new(projects: Vec<Project>) -> Self {
        let labels: Vec<String> = projects.iter().map(project_picker_label).collect();
        let mut picker = ListPicker::new("New Session from Project");
        picker.activate(labels);
        Self { picker, projects }
    }

    /// Resolve a label chosen from the picker back to the project's path.
    /// Labels embed the path so they are unique; matching on the label is
    /// safe even when two scopes share a name.
    fn path_for_label(&self, label: &str) -> Option<String> {
        self.projects
            .iter()
            .find(|p| project_picker_label(p) == label)
            .map(|p| p.path.clone())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<String> {
        match self.picker.handle_key(key) {
            ListPickerResult::Continue => DialogResult::Continue,
            ListPickerResult::Cancelled => DialogResult::Cancel,
            ListPickerResult::Selected(label) => match self.path_for_label(&label) {
                Some(path) => DialogResult::Submit(path),
                None => DialogResult::Cancel,
            },
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<String> {
        match self.picker.handle_click(col, row) {
            ListPickerResult::Continue => DialogResult::Continue,
            ListPickerResult::Cancelled => DialogResult::Cancel,
            ListPickerResult::Selected(label) => match self.path_for_label(&label) {
                Some(path) => DialogResult::Submit(path),
                None => DialogResult::Cancel,
            },
        }
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.picker.handle_hover(col, row)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.picker.render(frame, area, theme);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::projects::ProjectScope;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample_projects() -> Vec<Project> {
        vec![
            Project::new("alpha", "/tmp/alpha", ProjectScope::Global),
            Project::new("beta", "/tmp/beta", ProjectScope::Profile),
        ]
    }

    #[test]
    fn enter_submits_path_not_label() {
        let mut dialog = ProjectSessionPickerDialog::new(sample_projects());
        match dialog.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(path) => assert_eq!(path, "/tmp/alpha"),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut dialog = ProjectSessionPickerDialog::new(sample_projects());
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn filter_then_select_resolves_correct_path() {
        let mut dialog = ProjectSessionPickerDialog::new(sample_projects());
        // Filter down to "beta" then select.
        dialog.handle_key(key(KeyCode::Char('b')));
        dialog.handle_key(key(KeyCode::Char('e')));
        dialog.handle_key(key(KeyCode::Char('t')));
        match dialog.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(path) => assert_eq!(path, "/tmp/beta"),
            _ => panic!("expected Submit"),
        }
    }
}
