//! Claude subagents nested under their terminal session in the sidebar.
//!
//! A subagent has no pane, so its row is read-only: the preview shows its
//! recent transcript and Enter opens the parent session.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

use super::*;
use crate::session::subagents::{Subagent, SubagentActivity, SubagentFocus, SubagentState};
use crate::tui::components::format_scroll_indicator;
use crate::tui::styles::Theme;

const ICON_SUBAGENT_DONE: &str = "✓";

impl HomeView {
    /// Subagent rows tolerate a few seconds of lag; an open preview refreshes faster.
    pub fn subagent_refresh_interval(&self) -> std::time::Duration {
        if self.selected_subagent.is_some() {
            std::time::Duration::from_secs(1)
        } else {
            std::time::Duration::from_secs(3)
        }
    }

    pub fn request_subagent_refresh(&mut self) {
        if self.pending_subagent_refresh {
            return;
        }
        // Filtered before cloning: only live Claude terminal sessions are scanned.
        let watched: Vec<Instance> = self
            .instances
            .values()
            .filter(|inst| {
                super::super::subagent_poller::SubagentPoller::watches(inst)
                    && !self.recovery_in_flight.contains(&inst.id)
                    && !self.restart_in_flight.contains(&inst.id)
            })
            .cloned()
            .collect();
        if watched.is_empty() && self.subagents.is_empty() {
            return;
        }
        self.subagent_poller
            .request_refresh(watched, self.selected_subagent.clone());
        self.pending_subagent_refresh = true;
    }

    /// Apply the latest scan. Returns true when anything on screen changed.
    /// Rows are rebuilt only when a subagent appears, leaves, or changes state;
    /// new activity only repaints the open preview.
    pub fn apply_subagent_updates(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        match self.subagent_poller.try_recv_updates() {
            Ok(scan) => {
                self.pending_subagent_refresh = false;
                let mut changed = false;
                if scan.focus != self.subagent_activity {
                    self.subagent_activity = scan.focus;
                    self.subagent_activity_generation += 1;
                    changed = true;
                }
                if scan.subagents != self.subagents {
                    self.subagents = scan.subagents;
                    self.expanded_subagents
                        .retain(|id| self.subagents.contains_key(id));
                    self.rebuild_flat_items_keeping_cursor();
                    changed = true;
                }
                // The scan answered an older focus; ask again so a newly opened
                // preview fills without waiting for the next tick.
                let answered = self.subagent_activity.as_ref().map(|(focus, _)| focus);
                if self.selected_subagent.is_some() && self.selected_subagent.as_ref() != answered {
                    self.request_subagent_refresh();
                }
                changed
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                tracing::error!(
                    target: "tui.home",
                    "subagent poller worker gone; respawning a fresh poller",
                );
                self.subagent_poller = crate::tui::subagent_poller::SubagentPoller::new();
                self.pending_subagent_refresh = false;
                false
            }
        }
    }

    /// Show or hide `session_id`'s subagent rows. Returns false when it has none.
    pub(super) fn set_subagents_expanded(&mut self, session_id: &str, expanded: bool) -> bool {
        if !self.subagents.contains_key(session_id) {
            return false;
        }
        if expanded {
            self.expanded_subagents.insert(session_id.to_string());
        } else {
            self.expanded_subagents.remove(session_id);
        }
        self.rebuild_flat_items_keeping_cursor();
        true
    }

    /// Insert each expanded session's subagent rows directly below it.
    pub(super) fn with_subagent_rows(&self, items: Vec<Item>) -> Vec<Item> {
        if self.expanded_subagents.is_empty() {
            return items;
        }
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let children: Vec<Item> = match &item {
                Item::Session { id, depth } if self.expanded_subagents.contains(id) => self
                    .subagents
                    .get(id)
                    .into_iter()
                    .flatten()
                    .map(|subagent| Item::Subagent {
                        parent_id: id.clone(),
                        agent_id: subagent.agent_id.clone(),
                        depth: depth + 1,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            out.push(item);
            out.extend(children);
        }
        out
    }

    pub(super) fn subagent(&self, parent_id: &str, agent_id: &str) -> Option<&Subagent> {
        self.subagents
            .get(parent_id)?
            .iter()
            .find(|subagent| subagent.agent_id == agent_id)
    }

    pub(super) fn subagent_row(&self, parent_id: &str, agent_id: &str) -> Option<usize> {
        self.flat_items.iter().position(|item| {
            matches!(item, Item::Subagent { parent_id: p, agent_id: a, .. }
                if p == parent_id && a == agent_id)
        })
    }

    /// After a rebuild, put the cursor back on the selected subagent row, or on
    /// its parent once a finished subagent ages out, or clamp it when the
    /// parent is gone too. False when no subagent was selected.
    pub(super) fn reseat_subagent_cursor(&mut self) -> bool {
        let Some((parent_id, agent_id)) = self.selected_subagent.clone() else {
            return false;
        };
        if let Some(idx) = self.subagent_row(&parent_id, &agent_id) {
            self.cursor = idx;
            return true;
        }
        self.select_session_by_id(&parent_id);
        if self.selected_subagent.is_some() {
            self.selected_subagent = None;
            self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
            self.update_selected();
        }
        true
    }

    /// Move the cursor from a subagent row to its parent session.
    pub(super) fn select_subagent_parent(&mut self) -> bool {
        let Some((parent_id, _)) = self.selected_subagent.clone() else {
            return false;
        };
        self.select_session_by_id(&parent_id);
        true
    }

    /// The disclosure badge after a parent row's title, e.g. `▶2`.
    pub(super) fn subagent_badge(&self, session_id: &str, theme: &Theme) -> Option<Span<'static>> {
        let subagents = self.subagents.get(session_id)?;
        let glyph = if self.expanded_subagents.contains(session_id) {
            ICON_EXPANDED
        } else {
            ICON_COLLAPSED
        };
        let running = subagents
            .iter()
            .any(|subagent| subagent.state == SubagentState::Running);
        Some(Span::styled(
            format!(" {glyph}{}", subagents.len()),
            Style::default().fg(if running { theme.running } else { theme.dimmed }),
        ))
    }

    /// Icon, label, and style of a subagent row.
    pub(super) fn subagent_row_parts(
        &self,
        parent_id: &str,
        agent_id: &str,
        theme: &Theme,
    ) -> (&'static str, String, Style) {
        let Some(subagent) = self.subagent(parent_id, agent_id) else {
            return ("?", agent_id.to_string(), Style::default().fg(theme.dimmed));
        };
        let (icon, color) = match subagent.state {
            SubagentState::Running => (
                self.get_instance(parent_id).map_or(ICON_IDLE, |inst| {
                    super::render::spinner_running(&inst.created_at)
                }),
                theme.running,
            ),
            SubagentState::Done => (ICON_SUBAGENT_DONE, theme.dimmed),
            SubagentState::Stopped => (ICON_STOPPED, theme.dimmed),
            SubagentState::Failed => (ICON_ERROR, theme.error),
        };
        // The description tells siblings apart in a narrow sidebar; the type is in the preview.
        let label = if subagent.description.is_empty() {
            subagent.agent_type.clone()
        } else {
            subagent.description.clone()
        };
        (icon, label, Style::default().fg(color))
    }

    /// Paint the selected subagent's preview: a header and the end of its
    /// transcript. Returns false when no subagent is selected.
    pub(super) fn render_subagent_preview(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
    ) -> bool {
        let Some((parent_id, agent_id)) = self.selected_subagent.clone() else {
            return false;
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .padding(Padding::horizontal(1))
            .title(" Subagent ")
            .title_style(Style::default().fg(theme.title));
        let inner = block.inner(area);
        self.preview_outer_area = area;
        self.preview_area = inner;
        self.preview_pane_area = inner;
        self.preview_visible_rows = inner.height as usize;
        self.diff_area = Rect::default();

        let parent_title = self
            .get_instance(&parent_id)
            .map(|inst| inst.title.clone())
            .unwrap_or_default();
        let Some(subagent) = self.subagent(&parent_id, &agent_id) else {
            self.subagent_preview_layout = None;
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new("This subagent is no longer listed.")
                    .style(Style::default().fg(theme.dimmed)),
                inner,
            );
            return true;
        };
        let focus: SubagentFocus = (parent_id, agent_id.clone());
        let key = PreviewKey {
            generation: self.subagent_activity_generation,
            width: inner.width,
            subagent: subagent.clone(),
            parent_title,
            colors: [
                theme.title,
                theme.text,
                theme.dimmed,
                theme.running,
                theme.error,
                theme.accent,
            ],
        };
        if self.subagent_preview.as_ref().map(|(cached, _)| cached) != Some(&key) {
            let activity = self
                .subagent_activity
                .as_ref()
                .filter(|(answered, _)| *answered == focus)
                .map(|(_, activity)| activity.as_slice());
            let lines =
                subagent_preview_lines(subagent, activity, &key.parent_title, theme, inner.width);
            self.subagent_preview = Some((key, lines));
        }
        let lines = self
            .subagent_preview
            .as_ref()
            .map_or(&[][..], |(_, lines)| lines.as_slice());
        let layout = (agent_id, inner.width, lines.len());
        let grew_by = match &self.subagent_preview_layout {
            Some((agent, width, total)) if *agent == layout.0 && *width == layout.1 => {
                layout.2.saturating_sub(*total)
            }
            _ => 0,
        };
        self.subagent_preview_layout = Some(layout);
        let height = inner.height as usize;
        let (skip, offset) =
            scroll_window(lines.len(), height, self.preview_scroll_offset, grew_by);
        self.preview_scroll_offset = offset;

        let mut hint = vec![Span::styled(
            " read-only · Enter opens the session ",
            Style::default().fg(theme.dimmed).italic(),
        )];
        if let Some(indicator) = format_scroll_indicator(lines.len(), height, offset) {
            hint.push(Span::styled(
                indicator,
                Style::default().fg(theme.dimmed).italic(),
            ));
        }
        let visible: Vec<Line<'static>> = lines.iter().skip(skip).take(height).cloned().collect();
        frame.render_widget(block.title_top(Line::from(hint).right_aligned()), area);
        frame.render_widget(Paragraph::new(visible), inner);
        true
    }

    /// Wheel scroll over a subagent preview; `delta` is lines toward the top.
    pub(super) fn scroll_subagent_preview(&mut self, delta: i32) -> bool {
        let total = self
            .subagent_preview_layout
            .as_ref()
            .map_or(0, |(_, _, total)| *total);
        let max = total.saturating_sub(self.preview_visible_rows) as i32;
        let next = (i32::from(self.preview_scroll_offset) + delta).clamp(0, max) as u16;
        if next == self.preview_scroll_offset {
            return false;
        }
        self.preview_scroll_offset = next;
        true
    }

    /// Open the selected subagent's parent session as Enter on its row would.
    pub(super) fn activate_subagent_parent(&mut self) -> Option<crate::tui::app::Action> {
        self.select_subagent_parent()
            .then(|| self.activate_selected_session())
            .flatten()
    }
}

/// Rows to skip and the clamped offset for a preview of `total` lines in
/// `height` rows, where `offset` counts lines up from the bottom. While
/// scrolled up, `grew_by` new lines raise the offset so the view stays put.
fn scroll_window(total: usize, height: usize, offset: u16, grew_by: usize) -> (usize, u16) {
    let max = total.saturating_sub(height);
    let offset = if offset > 0 {
        usize::from(offset).saturating_add(grew_by)
    } else {
        0
    };
    let offset = offset.min(max);
    (max - offset, u16::try_from(offset).unwrap_or(u16::MAX))
}

/// Everything the wrapped preview lines depend on; an equal key reuses them.
#[derive(PartialEq)]
pub(super) struct PreviewKey {
    /// Bumped whenever the focused activity changes.
    generation: u64,
    width: u16,
    subagent: Subagent,
    parent_title: String,
    colors: [Color; 6],
}

/// `activity` is `None` until the first scan for this subagent answers.
fn subagent_preview_lines(
    subagent: &Subagent,
    activity: Option<&[SubagentActivity]>,
    parent_title: &str,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let (state, state_color) = match subagent.state {
        SubagentState::Running => ("running", theme.running),
        SubagentState::Done => ("done", theme.dimmed),
        SubagentState::Stopped => ("stopped", theme.dimmed),
        SubagentState::Failed => ("failed", theme.error),
    };
    let dim = Style::default().fg(theme.dimmed);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            subagent.agent_type.clone(),
            Style::default().fg(theme.title).bold(),
        ),
        Span::styled(" · ", dim),
        Span::styled(state, Style::default().fg(state_color)),
        Span::styled(format!(" · in {parent_title}"), dim),
    ])];
    if !subagent.description.is_empty() {
        crate::tui::markdown::wrap_line_into(
            Line::styled(
                subagent.description.clone(),
                Style::default().fg(theme.text),
            ),
            width,
            &mut lines,
        );
    }
    let Some(activity) = activity else {
        lines.push(Line::raw(""));
        lines.push(Line::styled("Loading…", dim));
        return lines;
    };
    for entry in activity {
        lines.push(Line::raw(""));
        match entry {
            SubagentActivity::Prompt(text) => {
                lines.push(Line::styled("Prompt", dim.bold()));
                for line in text.lines() {
                    crate::tui::markdown::wrap_line_into(
                        Line::styled(line.to_string(), dim),
                        width,
                        &mut lines,
                    );
                }
            }
            SubagentActivity::Text(text) => {
                lines.extend(crate::tui::markdown::render_wrapped(text, width));
            }
            SubagentActivity::Tool { name, detail } => {
                let mut spans = vec![Span::styled(
                    format!("● {name}"),
                    Style::default().fg(theme.accent),
                )];
                if !detail.is_empty() {
                    spans.push(Span::styled(format!(" {detail}"), dim));
                }
                crate::tui::markdown::wrap_line_into(Line::from(spans), width, &mut lines);
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::scroll_window;

    #[test]
    fn scroll_window_clamps_and_holds_position() {
        // (total, height, offset, grew_by) -> (skip, offset)
        for (input, expected) in [
            ((50, 10, 0, 5), (40, 0)),
            ((50, 10, 5, 0), (35, 5)),
            ((50, 10, 5, 3), (32, 8)),
            ((50, 10, 99, 0), (0, 40)),
            ((5, 10, 3, 0), (0, 0)),
        ] {
            let (total, height, offset, grew_by) = input;
            assert_eq!(
                scroll_window(total, height, offset, grew_by),
                expected,
                "{input:?}"
            );
        }
    }
}
