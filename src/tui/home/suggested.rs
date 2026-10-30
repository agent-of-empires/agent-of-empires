//! Home-list behavior of the Suggested group and its rows.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

use super::*;
use crate::tui::styles::Theme;

/// Prompt lines shown in the preview before it is ellipsised.
const PREVIEW_PROMPT_LINES: usize = 4;

/// "A", "A and B", "A, B and C".
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
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

    /// Enter on a suggestion row prepares it and opens the prefilled dialog;
    /// on "View all" it opens the full view. False when the cursor is on
    /// neither.
    pub(super) fn activate_suggestion_row(&mut self) -> bool {
        if let Some(key) = self.cursor_suggestion() {
            self.suggestions_feed.prepare(key);
            return true;
        }
        if matches!(
            self.flat_items.get(self.cursor),
            Some(Item::SuggestionsViewAll { .. })
        ) {
            self.suggestions_view = Some(super::super::suggestions::SuggestionsView::new());
            return true;
        }
        false
    }

    /// `z` on a suggestion row dismisses it. False when the cursor is elsewhere.
    pub(super) fn dismiss_suggestion_row(&mut self) -> bool {
        let Some(key) = self.cursor_suggestion() else {
            return false;
        };
        if self.session_feed.mutations_available() {
            self.suggestions_feed.dismiss(&key);
        } else {
            self.flash_status("The daemon is read-only or unavailable");
        }
        true
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
                let total = self.suggestions_feed.keys().len();
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
                lines.push(Line::from(Span::styled("↵ start  z dismiss  ^K more", dim)));
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
}
