//! Home-list behavior of the Suggested group and its rows.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

use super::*;
use crate::tui::components::rendered_width;
use crate::tui::dialogs::{PaletteAction, PaletteCommand, PaletteGroup};
use crate::tui::styles::Theme;

/// Prompt lines shown in the preview before it is ellipsised.
const PREVIEW_PROMPT_LINES: usize = 4;

/// Terminal width from which the key line names the ^K action.
const WIDE_KEY_LINE_COLS: u16 = 120;

/// "A", "A and B", "A, B and C".
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A row of the Suggested group, identified independently of its index.
pub(super) enum SuggestedRow {
    Header,
    Item((String, String)),
    ViewAll,
}

impl HomeView {
    pub fn toggle_suggested_section(&mut self) {
        self.suggested_section_collapsed = !self.suggested_section_collapsed;
        self.rebuild_flat_items();
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
    }

    /// The suggestion under the cursor, when the cursor is on one.
    pub(super) fn cursor_suggestion(&self) -> Option<(String, String)> {
        match self.flat_items.get(self.cursor) {
            Some(Item::Suggestion {
                provider_id, id, ..
            }) => Some((provider_id.clone(), id.clone())),
            _ => None,
        }
    }

    /// What the cursor is on within the Suggested group, to find it again after a rebuild.
    pub(super) fn suggested_row_at_cursor(&self) -> Option<SuggestedRow> {
        match self.flat_items.get(self.cursor)? {
            Item::Group { path, .. } if crate::session::is_suggested_section_path(path) => {
                Some(SuggestedRow::Header)
            }
            Item::Suggestion {
                provider_id, id, ..
            } => Some(SuggestedRow::Item((provider_id.clone(), id.clone()))),
            Item::SuggestionsViewAll { .. } => Some(SuggestedRow::ViewAll),
            _ => None,
        }
    }

    pub(super) fn suggested_row_position(&self, row: Option<&SuggestedRow>) -> Option<usize> {
        let row = row?;
        self.flat_items.iter().position(|item| match (row, item) {
            (SuggestedRow::Header, Item::Group { path, .. }) => {
                crate::session::is_suggested_section_path(path)
            }
            (
                SuggestedRow::Item((p, i)),
                Item::Suggestion {
                    provider_id, id, ..
                },
            ) => p == provider_id && i == id,
            (SuggestedRow::ViewAll, Item::SuggestionsViewAll { .. }) => true,
            _ => false,
        })
    }

    /// Enter on a suggestion row opens its started Session, or prepares it
    /// and opens the prefilled dialog; on "View all" it opens the full view.
    /// False when the cursor is on neither.
    pub(super) fn activate_suggestion_row(&mut self) -> bool {
        if let Some(key) = self.cursor_suggestion() {
            match self
                .suggestions_feed
                .entry(&key)
                .and_then(|e| e.started_session)
            {
                Some(id) => {
                    self.open_linked_session(&id);
                }
                None if self.mutations_blocked() => {}
                None => self.suggestions_feed.prepare(key),
            }
            return true;
        }
        if matches!(
            self.flat_items.get(self.cursor),
            Some(Item::SuggestionsViewAll { .. })
        ) {
            self.open_suggestions_view();
            return true;
        }
        false
    }

    /// Selects the Session a suggestion was started as, expanding the Archived
    /// or Trash section when that is where it sits. False, with the reason
    /// flashed, when it cannot be shown (including another profile's).
    pub(super) fn open_linked_session(&mut self, id: &str) -> bool {
        let Some(row) = self.session_feed.applied_session(id) else {
            self.flash_status("That session no longer exists");
            return false;
        };
        let (profile, trashed, archived) = (
            row.profile.clone(),
            row.trashed_at.is_some(),
            row.archived_at.is_some(),
        );
        if self.active_profile.as_deref().is_some_and(|p| p != profile) {
            self.flash_status(format!(
                "That session is in profile {profile}. Switch to it to open it"
            ));
            return false;
        }
        let sections = (
            self.trashed_section_collapsed,
            self.archived_section_collapsed,
        );
        if trashed {
            self.trashed_section_collapsed = false;
        } else if archived {
            self.archived_section_collapsed = false;
        }
        self.rebuild_flat_items();
        self.jump_to_session_id(id);
        let shown = self.selected_session.as_deref() == Some(id);
        if !shown {
            (
                self.trashed_section_collapsed,
                self.archived_section_collapsed,
            ) = sections;
            self.rebuild_flat_items();
            self.flash_status("That session could not be shown");
        }
        shown
    }

    /// Flashes why the daemon refuses mutations; true when it does.
    fn mutations_blocked(&mut self) -> bool {
        if self.session_feed.mutations_available() {
            return false;
        }
        self.flash_status("The daemon is read-only or unavailable");
        true
    }

    /// `z` on a suggestion row dismisses it, or restores one still in its undo
    /// window. False when the cursor is elsewhere.
    pub(super) fn dismiss_suggestion_row(&mut self) -> bool {
        let Some(key) = self.cursor_suggestion() else {
            return false;
        };
        if !self.mutations_blocked() {
            let dismissed = self
                .suggestions_feed
                .entry(&key)
                .is_some_and(|e| e.dismissed);
            self.suggestions_feed.dismiss(&key, !dismissed);
        }
        true
    }

    /// `N` (Ctrl+N in strict mode) on a suggestion row prepares a second session for the same item;
    /// the new session does not claim the suggestion's link.
    pub(super) fn start_another_from_suggestion(&mut self) {
        if let Some(key) = self.cursor_suggestion() {
            if !self.mutations_blocked() {
                self.suggestions_feed.prepare_another(key);
            }
        }
    }
    /// "Open in <provider>" for a suggestion, when its `source_url` is openable.
    pub(super) fn open_source_command(&self, key: &(String, String)) -> Option<PaletteCommand> {
        let entry = self.suggestions_feed.entry(key)?;
        Some(PaletteCommand {
            id: "open-suggestion-source",
            title: format!("Open in {}", entry.provider),
            group: PaletteGroup::Actions,
            keywords: vec!["open", "link", "source", "browser"],
            hotkey: String::new(),
            payload: PaletteAction::OpenLink(entry.source_url?),
        })
    }

    /// Paints the preview for the Suggested group or a suggestion row. False
    /// when the cursor is on neither, so the regular preview runs.
    pub(super) fn render_suggestion_preview(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
    ) -> bool {
        let dim = Style::default().fg(theme.dimmed);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let (title, lines): (String, Vec<Line>) = match self.flat_items.get(self.cursor) {
            Some(Item::Group { path, .. }) if crate::session::is_suggested_section_path(path) => {
                let total = self.suggestions_feed.listed_count();
                (
                    format!("Suggested ({total})"),
                    vec![
                        Line::from(format!(
                            "From {} suggestions.",
                            join_names(&self.suggestions_feed.provider_names())
                        )),
                        Line::from(""),
                        Line::from(Span::styled(
                            "Enter to expand, then Enter on one to start a session.",
                            dim,
                        )),
                    ],
                )
            }
            Some(Item::SuggestionsViewAll { total, .. }) => (
                format!("Suggested ({total})"),
                vec![Line::from(Span::styled(
                    "Enter to open all suggestions.",
                    dim,
                ))],
            ),
            Some(Item::Suggestion {
                provider_id, id, ..
            }) => {
                let Some(entry) = self
                    .suggestions_feed
                    .entry(&(provider_id.clone(), id.clone()))
                else {
                    return false;
                };
                let mut source = vec![entry.provider.clone()];
                if !entry.source_ref.is_empty() {
                    source.push(entry.source_ref.clone());
                }
                source.extend(entry.project.clone());
                source.push(entry.reason.clone());
                let mut lines = vec![
                    Line::from(Span::styled(entry.title.clone(), bold)),
                    Line::from(source.join(" · ")),
                ];
                if entry.provider_error.is_some() {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{} couldn't refresh. Shown from the last fetch.",
                            entry.provider
                        ),
                        Style::default().fg(theme.waiting),
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("Prompt", bold)));
                let prompt: Vec<&str> = entry.prompt.lines().collect();
                lines.extend(
                    prompt
                        .iter()
                        .take(PREVIEW_PROMPT_LINES)
                        .map(|l| Line::from(format!("  {l}"))),
                );
                if prompt.len() > PREVIEW_PROMPT_LINES {
                    lines.push(Line::from("  …"));
                }
                lines.push(Line::from(""));
                let dismiss = super::bindings::label(
                    super::bindings::ActionId::ToggleArchive,
                    self.strict_hotkeys,
                );
                let enter = if entry.started_session.is_some() {
                    "open"
                } else {
                    "start"
                };
                let dismiss_verb = if entry.dismissed {
                    "restore"
                } else {
                    "dismiss"
                };
                let keys = if frame.area().width >= WIDE_KEY_LINE_COLS {
                    format!(
                        "↵ {enter}  {dismiss} {dismiss_verb}  ^K open in {}",
                        entry.provider
                    )
                } else {
                    format!("↵ {enter}  {dismiss} {dismiss_verb}  ^K more")
                };
                lines.push(Line::from(Span::styled(keys, dim)));
                ("Suggestion".to_string(), lines)
            }
            _ => return false,
        };
        self.preview_outer_area = area;
        self.preview_area = area;
        self.preview_pane_area = area;
        self.preview_visible_rows = area.height as usize;
        self.sync_preview_capture_worker(None);
        let block = Block::default()
            .title(format!(" {title} "))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .title_style(Style::default().fg(theme.title))
            .padding(Padding::horizontal(1));
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        true
    }

    /// Muted `source ref ` prefix and the title of a suggestion row.
    pub(super) fn suggestion_row_text(
        &self,
        provider_id: &str,
        id: &str,
    ) -> Option<(String, String)> {
        let entry = self
            .suggestions_feed
            .entry(&(provider_id.to_owned(), id.to_owned()))?;
        let pad = " ".repeat(6usize.saturating_sub(rendered_width(&entry.label)));
        let prefix = if entry.source_ref.is_empty() {
            format!("{}{pad} ", entry.label)
        } else {
            format!("{}{pad} {} ", entry.label, entry.source_ref)
        };
        Some((prefix, entry.title))
    }
}
