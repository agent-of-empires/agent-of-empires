//! Tips overlay: a browsable list of the hints from [`crate::tips`].
//!
//! Unseen tips lead; seen ones collapse into an expandable "Seen" section.
//! Focusing a tip counts as viewing it, and a "don't show tips again" toggle
//! silences the badge without hiding this list. Both persist via
//! [`TipsOutcome`] on close.
//!
//! The unseen/seen split is snapshotted at open, so focusing a tip does not
//! make it jump sections mid-view; the move lands on the next reopen.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::{centered_rect, DialogResult};
use crate::tips::Tip;
use crate::tui::home::bindings::{self, ActionId};
use crate::tui::styles::Theme;

/// What the home view persists when the tips overlay closes.
pub struct TipsOutcome {
    /// Tip ids the user viewed this session; merge into `tips_seen`.
    pub newly_seen: Vec<String>,
    /// The "don't show tips" preference, `None` when untouched.
    pub disabled: Option<bool>,
}

/// A visible line in the overlay: a tip (by index into `tips`) or the
/// collapsible "already seen" section header.
enum Row {
    Tip(usize),
    SeenHeader,
}

pub struct TipsDialog {
    /// Eligible tips to browse, in catalog order.
    tips: Vec<&'static Tip>,
    /// Cursor into the currently visible rows (see `visible_rows`).
    cursor: usize,
    /// Ids seen before this overlay opened, fixing the partition.
    initially_seen: Vec<String>,
    /// Ids first viewed during this session.
    newly_seen: Vec<String>,
    /// Current "don't show tips" state, and whether the user flipped it here.
    disabled: bool,
    disabled_touched: bool,
    /// Strict-hotkey mode, so keybinding placeholders show the live chord.
    strict: bool,
    /// Whether the "Seen" section is collapsed.
    seen_collapsed: bool,
    /// Click rects parallel to `visible_rows()`, zero-sized when scrolled out.
    row_rects: Vec<Rect>,
    /// The modal's outer rect; a click outside it closes the overlay.
    dialog_rect: Rect,
}

impl TipsDialog {
    pub fn new(tips: Vec<&'static Tip>, seen: Vec<String>, disabled: bool, strict: bool) -> Self {
        // Lead with unseen tips, but expand Seen when there are none, so the
        // overlay is not a lone header.
        let has_unseen = tips.iter().any(|t| !seen.iter().any(|s| s == t.id));
        let mut dialog = Self {
            tips,
            cursor: 0,
            initially_seen: seen,
            newly_seen: Vec::new(),
            disabled,
            disabled_touched: false,
            strict,
            seen_collapsed: has_unseen,
            row_rects: Vec::new(),
            dialog_rect: Rect::default(),
        };
        // Focusing a tip counts as viewing it, so the one shown on open is seen.
        dialog.mark_current_seen();
        dialog
    }

    fn was_initially_seen(&self, id: &str) -> bool {
        self.initially_seen.iter().any(|s| s == id)
    }

    fn is_seen(&self, id: &str) -> bool {
        self.was_initially_seen(id) || self.newly_seen.iter().any(|s| s == id)
    }

    fn seen_count(&self) -> usize {
        self.tips
            .iter()
            .filter(|t| self.was_initially_seen(t.id))
            .count()
    }

    /// The visible rows: unseen tips, then the "Seen" header and, when
    /// expanded, the seen tips. Partitioned by the state captured at open, so
    /// focusing a tip does not reshuffle the list under the cursor.
    fn visible_rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .tips
            .iter()
            .enumerate()
            .filter(|(_, t)| !self.was_initially_seen(t.id))
            .map(|(i, _)| Row::Tip(i))
            .collect();
        let seen: Vec<usize> = self
            .tips
            .iter()
            .enumerate()
            .filter(|(_, t)| self.was_initially_seen(t.id))
            .map(|(i, _)| i)
            .collect();
        if !seen.is_empty() {
            rows.push(Row::SeenHeader);
            if !self.seen_collapsed {
                rows.extend(seen.into_iter().map(Row::Tip));
            }
        }
        rows
    }

    fn focused_tip(&self) -> Option<&'static Tip> {
        match self.visible_rows().get(self.cursor) {
            Some(Row::Tip(i)) => self.tips.get(*i).copied(),
            _ => None,
        }
    }

    fn on_seen_header(&self) -> bool {
        matches!(self.visible_rows().get(self.cursor), Some(Row::SeenHeader))
    }

    fn mark_current_seen(&mut self) {
        if let Some(tip) = self.focused_tip() {
            if !self.is_seen(tip.id) {
                self.newly_seen.push(tip.id.to_string());
            }
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.visible_rows().len();
        if len == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).rem_euclid(len as isize) as usize;
        self.mark_current_seen();
    }

    fn set_seen_collapsed(&mut self, collapsed: bool) {
        self.seen_collapsed = collapsed;
        // Park the cursor on the header so expand/collapse is a stable pivot
        // rather than dumping it into (or stranding it past) the section.
        let rows = self.visible_rows();
        if let Some(idx) = rows.iter().position(|r| matches!(r, Row::SeenHeader)) {
            self.cursor = idx;
        } else if self.cursor >= rows.len() {
            self.cursor = rows.len().saturating_sub(1);
        }
    }

    fn outcome(&self) -> TipsOutcome {
        TipsOutcome {
            newly_seen: self.newly_seen.clone(),
            disabled: if self.disabled_touched {
                Some(self.disabled)
            } else {
                None
            },
        }
    }

    /// Substitute keybinding placeholders so a tip body reflects the user's
    /// actual chord (correct in strict-hotkey mode too). Cheap no-op for the
    /// common case of a body with no placeholder.
    fn resolve_body(&self, body: &str) -> String {
        if !body.contains('{') {
            return body.to_string();
        }
        // (placeholder, action) pairs; each is replaced with the action's live
        // chord so the text is correct in strict-hotkey mode too.
        const PLACEHOLDERS: &[(&str, ActionId)] = &[
            ("{new_from_selection}", ActionId::NewFromSelection),
            ("{toggle_view}", ActionId::ToggleView),
            ("{diff}", ActionId::Diff),
            ("{settings}", ActionId::Settings),
            ("{help}", ActionId::Help),
            ("{sort}", ActionId::SortPicker),
            ("{group}", ActionId::GroupBy),
            ("{archive}", ActionId::ToggleArchive),
            ("{snooze}", ActionId::ToggleSnooze),
            ("{favorite}", ActionId::ToggleFavorite),
            ("{serve}", ActionId::Serve),
            ("{tool_session}", ActionId::ToolPicker),
        ];
        let mut out = body.to_string();
        for (placeholder, action) in PLACEHOLDERS {
            if out.contains(placeholder) {
                out = out.replace(placeholder, &bindings::label(*action, self.strict));
            }
        }
        out
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<TipsOutcome> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Submit(self.outcome()),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(-1);
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(1);
                DialogResult::Continue
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.on_seen_header() => {
                self.set_seen_collapsed(!self.seen_collapsed);
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char('l') if self.on_seen_header() => {
                self.set_seen_collapsed(false);
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Char('h') if self.on_seen_header() => {
                self.set_seen_collapsed(true);
                DialogResult::Continue
            }
            KeyCode::Char('d') => {
                self.disabled = !self.disabled;
                self.disabled_touched = true;
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    /// Route a left-click. A tip row focuses it (and marks it seen); the Seen
    /// header toggles the section; a click outside the modal closes the overlay
    /// (persisting what was seen); any other in-modal click is swallowed.
    /// Returns `None` only before the first render, when no rects are known.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<TipsOutcome>> {
        let pos = ratatui::layout::Position::from((col, row));
        if self.dialog_rect.width == 0 {
            return None;
        }
        if !self.dialog_rect.contains(pos) {
            return Some(DialogResult::Submit(self.outcome()));
        }
        let hit = self
            .row_rects
            .iter()
            .position(|r| r.width > 0 && r.contains(pos));
        if let Some(vis_idx) = hit {
            match self.visible_rows().get(vis_idx) {
                Some(Row::Tip(_)) if vis_idx != self.cursor => {
                    self.cursor = vis_idx;
                    self.mark_current_seen();
                }
                Some(Row::Tip(_)) => {}
                Some(Row::SeenHeader) => {
                    self.cursor = vis_idx;
                    self.set_seen_collapsed(!self.seen_collapsed);
                }
                None => {}
            }
        }
        Some(DialogResult::Continue)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_area = centered_rect(area, 74, 22);
        self.dialog_rect = dialog_area;
        frame.render_widget(Clear, dialog_area);

        let unseen = self.tips.iter().filter(|t| !self.is_seen(t.id)).count();
        let title = if unseen > 0 {
            format!(" Tips  ({unseen} new) ")
        } else {
            " Tips ".to_string()
        };
        let block = super::toned_dialog_block(title, theme.accent, theme.accent);
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        // List takes up to half the height; the focused tip's body fills the
        // rest. Footer pinned to the bottom.
        let list_max = ((inner.height.saturating_sub(4)) / 2).max(1);
        let row_count = self.visible_rows().len() as u16;
        // 0 when there are no tips, so the empty-state message gets the room.
        let list_height = row_count.min(list_max);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(list_height),
                Constraint::Length(1), // gap
                Constraint::Min(2),    // body
                Constraint::Length(1), // footer
            ])
            .split(inner);

        self.render_list(frame, chunks[0], theme);

        if self.tips.is_empty() {
            // Nothing eligible yet; give "Show tips" some feedback rather than
            // opening a blank box.
            frame.render_widget(
                Paragraph::new(
                    "No tips right now. As you use aoe, helpful tips will show up here.",
                )
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(theme.dimmed)),
                chunks[2],
            );
        } else if let Some(tip) = self.focused_tip() {
            let body = self.resolve_body(tip.body);
            frame.render_widget(
                Paragraph::new(body)
                    .wrap(Wrap { trim: true })
                    .style(Style::default().fg(theme.text)),
                chunks[2],
            );
        } else {
            // Cursor is on the Seen header.
            let hint = if self.seen_collapsed {
                format!(
                    "{} tips you've already seen are hidden. Press Enter to show them.",
                    self.seen_count()
                )
            } else {
                "Tips you've already seen. Press Enter to hide them again.".to_string()
            };
            frame.render_widget(
                Paragraph::new(hint)
                    .wrap(Wrap { trim: true })
                    .style(Style::default().fg(theme.dimmed)),
                chunks[2],
            );
        }

        let toggle_label = if self.disabled {
            " show me tips  "
        } else {
            " don't show me tips  "
        };
        let footer = Line::from(vec![
            Span::styled("↑/↓", Style::default().fg(theme.hint)),
            Span::styled(" browse  ", Style::default().fg(theme.dimmed)),
            Span::styled("d", Style::default().fg(theme.hint)),
            Span::styled(toggle_label, Style::default().fg(theme.dimmed)),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::styled(" close", Style::default().fg(theme.dimmed)),
        ]);
        frame.render_widget(Paragraph::new(footer), chunks[3]);
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let rows = self.visible_rows();
        // Rebuilt every frame; entries outside the scroll window stay
        // zero-sized so a click can't resolve to a row that isn't drawn.
        self.row_rects = vec![Rect::default(); rows.len()];
        if area.height == 0 || rows.is_empty() {
            return;
        }
        // Slide a window so the cursor stays visible when the list is taller
        // than the area (mirrors the intro theme picker).
        let visible = area.height as usize;
        let total = rows.len();
        let start = if total <= visible || self.cursor < visible / 2 {
            0
        } else if self.cursor + visible / 2 >= total {
            total.saturating_sub(visible)
        } else {
            self.cursor - visible / 2
        };
        let end = (start + visible).min(total);

        for (offset, vis_idx) in (start..end).enumerate() {
            let rect = Rect {
                x: area.x,
                y: area.y + offset as u16,
                width: area.width,
                height: 1,
            };
            self.row_rects[vis_idx] = rect;
            let is_focused = vis_idx == self.cursor;
            let line = match &rows[vis_idx] {
                Row::Tip(i) => {
                    let tip = self.tips[*i];
                    let seen = self.is_seen(tip.id);
                    let pointer = if is_focused { "▶ " } else { "  " };
                    let marker = if seen { "  " } else { "● " };
                    let title_style = if is_focused {
                        Style::default().fg(theme.accent).bold()
                    } else if seen {
                        Style::default().fg(theme.dimmed)
                    } else {
                        Style::default().fg(theme.text)
                    };
                    let marker_style = if seen {
                        Style::default().fg(theme.dimmed)
                    } else {
                        Style::default().fg(theme.accent)
                    };
                    Line::from(vec![
                        Span::styled(pointer, title_style),
                        Span::styled(marker, marker_style),
                        Span::styled(tip.title.to_string(), title_style),
                    ])
                }
                Row::SeenHeader => {
                    let arrow = if self.seen_collapsed { "▸" } else { "▾" };
                    let label = format!("{arrow} Seen ({})", self.seen_count());
                    let style = if is_focused {
                        Style::default().fg(theme.accent).bold()
                    } else {
                        Style::default().fg(theme.dimmed)
                    };
                    Line::from(Span::styled(label, style))
                }
            };
            frame.render_widget(Paragraph::new(line), rect);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::key;

    /// Synthetic tips, so behavior does not depend on how many the real
    /// catalog ships.
    static TEST_TIPS: &[Tip] = &[
        Tip {
            id: "alpha",
            title: "Alpha tip",
            body: "Body of the alpha tip.",
            trigger: crate::tips::TipTrigger::Rotation,
            surfaces: &[crate::tips::TipSurface::Tui],
        },
        Tip {
            id: "beta",
            title: "Beta tip",
            body: "Body of the beta tip.",
            trigger: crate::tips::TipTrigger::Rotation,
            surfaces: &[crate::tips::TipSurface::Tui],
        },
        Tip {
            id: "gamma",
            title: "Gamma tip",
            body: "Body of the gamma tip.",
            trigger: crate::tips::TipTrigger::Rotation,
            surfaces: &[crate::tips::TipSurface::Tui],
        },
    ];

    fn all_tips() -> Vec<&'static Tip> {
        TEST_TIPS.iter().collect()
    }

    /// Ids of every tip from `skip` onward, for seeding the seen set.
    fn ids(skip: usize) -> Vec<String> {
        TEST_TIPS
            .iter()
            .skip(skip)
            .map(|t| t.id.to_string())
            .collect()
    }

    fn dialog(seen: Vec<String>) -> TipsDialog {
        TipsDialog::new(all_tips(), seen, false, false)
    }

    /// Draw the dialog once so its row rects exist, and return the screen.
    fn render_to(dialog: &mut TipsDialog) -> String {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = load_theme("empire");
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal
            .draw(|f| dialog.render(f, f.area(), &theme))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    fn submitted(result: DialogResult<TipsOutcome>) -> TipsOutcome {
        match result {
            DialogResult::Submit(outcome) => outcome,
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn focusing_a_tip_records_it_as_seen_once() {
        // The tip shown on open counts as viewed.
        assert_eq!(dialog(vec![]).newly_seen, vec![TEST_TIPS[0].id.to_string()]);

        // Walking the whole list records every tip; with nothing previously
        // seen there is no Seen header to land on.
        let mut d = dialog(vec![]);
        for _ in 0..TEST_TIPS.len() {
            d.handle_key(key(KeyCode::Down));
        }
        assert_eq!(d.newly_seen.len(), TEST_TIPS.len());

        // A tip already seen starts in the Seen section and is not re-recorded.
        let first = TEST_TIPS[0].id.to_string();
        assert!(!dialog(vec![first.clone()]).newly_seen.contains(&first));
    }

    #[test]
    fn esc_reports_what_was_seen_and_whether_tips_were_disabled() {
        let outcome = submitted(dialog(vec![]).handle_key(key(KeyCode::Esc)));
        assert!(!outcome.newly_seen.is_empty());
        assert_eq!(outcome.disabled, None, "no toggle leaves the preference");

        let mut d = dialog(vec![]);
        d.handle_key(key(KeyCode::Char('d')));
        assert_eq!(
            submitted(d.handle_key(key(KeyCode::Esc))).disabled,
            Some(true)
        );
    }

    #[test]
    fn the_seen_section_collapses_only_while_something_is_unseen() {
        // One unseen tip: a collapsed header holds the rest.
        let mut d = dialog(ids(1));
        assert!(d.seen_collapsed);
        assert_eq!(d.visible_rows().len(), 2);
        d.handle_key(key(KeyCode::Down));
        assert!(d.on_seen_header());
        d.handle_key(key(KeyCode::Enter));
        assert!(!d.seen_collapsed);
        assert_eq!(d.visible_rows().len(), TEST_TIPS.len() + 1);

        // Nothing new to read: show the seen tips rather than a lone header.
        let d = dialog(ids(0));
        assert!(!d.seen_collapsed);
        assert_eq!(d.visible_rows().len(), TEST_TIPS.len() + 1);
    }

    #[test]
    fn tip_bodies_substitute_the_live_keybindings() {
        let dialog = dialog(vec![]);
        let body = "{toggle_view} {diff} {settings} {help} {sort} {group} {archive} \
                    {snooze} {favorite} {serve} {tool_session}";
        let resolved = dialog.resolve_body(body);
        assert!(
            !resolved.contains('{'),
            "every placeholder filled: {resolved}"
        );
        assert!(resolved.contains(&bindings::label(ActionId::Diff, false)));
        assert!(resolved.contains(&bindings::label(ActionId::ToggleArchive, false)));

        // Strict mode renders a different chord for the same action.
        let normal = dialog.resolve_body("Press {new_from_selection} now");
        let strict = TipsDialog::new(all_tips(), vec![], false, true)
            .resolve_body("Press {new_from_selection} now");
        assert!(normal.contains(&bindings::label(ActionId::NewFromSelection, false)));
        assert!(strict.contains(&bindings::label(ActionId::NewFromSelection, true)));
        assert_ne!(normal, strict);
    }

    #[test]
    fn clicks_focus_a_row_expand_the_header_and_close_from_outside() {
        let mut d = dialog(vec![]);
        let screen = render_to(&mut d);
        assert!(screen.contains("Tips"), "title renders\n{screen}");
        assert!(screen.contains(TEST_TIPS[0].title), "{screen}");
        assert!(screen.contains("close"), "footer hint renders\n{screen}");

        let target = d.row_rects[1];
        assert!(target.width > 0, "second row should be drawn");
        assert!(matches!(
            d.handle_click(target.x + 1, target.y),
            Some(DialogResult::Continue)
        ));
        assert_eq!(d.cursor, 1);
        assert!(d.is_seen(d.tips[1].id), "the clicked row is marked seen");

        // With one unseen tip, row 1 is the collapsed header instead.
        let mut d = dialog(ids(1));
        render_to(&mut d);
        assert!(d.seen_collapsed);
        let header = d.row_rects[1];
        assert!(header.width > 0);
        assert!(matches!(
            d.handle_click(header.x + 1, header.y),
            Some(DialogResult::Continue)
        ));
        assert!(!d.seen_collapsed);

        // (0, 0) is outside the centered modal, so it closes and persists.
        let mut d = dialog(vec![]);
        render_to(&mut d);
        let outcome = match d.handle_click(0, 0) {
            Some(result) => submitted(result),
            None => panic!("an outside click should close the overlay"),
        };
        assert!(!outcome.newly_seen.is_empty());
    }

    #[test]
    fn an_empty_catalog_still_renders_and_closes() {
        let mut d = TipsDialog::new(vec![], vec![], false, false);
        // Navigation and the toggle must not panic with no rows.
        d.handle_key(key(KeyCode::Down));
        d.handle_key(key(KeyCode::Up));
        d.handle_key(key(KeyCode::Char('d')));

        let screen = render_to(&mut d);
        assert!(screen.contains("No tips right now"), "{screen}");
        assert_eq!(
            submitted(d.handle_key(key(KeyCode::Esc))).disabled,
            Some(true)
        );
    }
}
