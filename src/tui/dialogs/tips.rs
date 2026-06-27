//! Tips overlay: a browsable list of the hints from [`crate::tips`].
//!
//! Opened from the command palette, the `?` help screen, or the tips badge.
//! Unseen tips lead the list; tips the user has already seen collapse into a
//! "Seen" section (so the overlay foregrounds what's new), which can be
//! expanded to re-read them. Focusing a tip counts as viewing it, so the
//! badge's unseen count ticks down as the user works through the collection.
//! A "don't show tips again" toggle disables the badge and earned pops without
//! hiding this list. Closing persists what was seen plus any toggle via
//! [`TipsOutcome`].
//!
//! The unseen/seen split is snapshotted when the overlay opens, so focusing a
//! tip marks it seen without making it jump sections mid-view; the move lands
//! on the next reopen.

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
    /// Final "don't show tips" preference if the user toggled it this session;
    /// `None` when untouched, so the caller leaves the stored value alone.
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
    /// Ids seen before this overlay opened. Drives both the unseen marker and
    /// the (stable) unseen/seen partition.
    initially_seen: Vec<String>,
    /// Ids first viewed during this session.
    newly_seen: Vec<String>,
    /// Current "don't show tips" state, and whether the user flipped it here.
    disabled: bool,
    disabled_touched: bool,
    /// Strict-hotkey mode, used to render keybinding placeholders with the live
    /// chord (e.g. `Ctrl+N` vs `Shift+N`).
    strict: bool,
    /// Whether the "Seen" section is collapsed.
    seen_collapsed: bool,
    /// Click rects for the visible rows, parallel to `visible_rows()`
    /// (zero-sized for rows outside the scroll window).
    row_rects: Vec<Rect>,
    /// The modal's outer rect; a click outside it closes the overlay.
    dialog_rect: Rect,
}

impl TipsDialog {
    pub fn new(tips: Vec<&'static Tip>, seen: Vec<String>, disabled: bool, strict: bool) -> Self {
        // Collapse the Seen section when there's something new to read, so the
        // overlay leads with unseen tips. If everything's already been seen,
        // expand it so the overlay isn't just a lone header.
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

    /// The rows shown right now, top to bottom: unseen tips, then (if any were
    /// already seen) the "Seen" header and, when expanded, the seen tips. The
    /// partition uses the seen-state captured at open, so focusing a tip marks
    /// it seen without reshuffling the list under the cursor.
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(title)
            .title_style(Style::default().fg(theme.accent).bold());
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
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // Synthetic fixtures so the overlay's behavior tests (sections, navigation,
    // clicks) don't depend on how many tips the real catalog happens to ship.
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

    fn ids(skip: usize) -> Vec<String> {
        all_tips()
            .iter()
            .skip(skip)
            .map(|t| t.id.to_string())
            .collect()
    }

    #[test]
    fn opening_marks_the_first_tip_seen() {
        let tips = all_tips();
        let first_id = tips[0].id.to_string();
        let dialog = TipsDialog::new(tips, vec![], false, false);
        assert_eq!(dialog.newly_seen, vec![first_id]);
    }

    #[test]
    fn navigating_marks_each_focused_tip_seen() {
        let tips = all_tips();
        let count = tips.len();
        let mut dialog = TipsDialog::new(tips, vec![], false, false);
        for _ in 0..count {
            dialog.handle_key(key(KeyCode::Down));
        }
        // No tip was initially seen, so there's no Seen header to land on;
        // every tip gets focused and recorded.
        assert_eq!(dialog.newly_seen.len(), count);
    }

    #[test]
    fn already_seen_tip_is_not_re_recorded() {
        let tips = all_tips();
        let first_id = tips[0].id.to_string();
        // Mark the first tip seen, so it starts in the (collapsed) Seen section
        // and the cursor opens on the first *unseen* tip instead.
        let dialog = TipsDialog::new(tips, vec![first_id.clone()], false, false);
        assert!(
            !dialog.newly_seen.contains(&first_id),
            "an already-seen tip is never re-recorded"
        );
    }

    #[test]
    fn esc_submits_outcome_with_seen_and_no_toggle() {
        let mut dialog = TipsDialog::new(all_tips(), vec![], false, false);
        match dialog.handle_key(key(KeyCode::Esc)) {
            DialogResult::Submit(outcome) => {
                assert!(!outcome.newly_seen.is_empty());
                assert_eq!(
                    outcome.disabled, None,
                    "no toggle => leave preference alone"
                );
            }
            _ => panic!("Esc should submit the tips outcome"),
        }
    }

    #[test]
    fn d_toggles_disabled_and_reports_it() {
        let mut dialog = TipsDialog::new(all_tips(), vec![], false, false);
        dialog.handle_key(key(KeyCode::Char('d')));
        match dialog.handle_key(key(KeyCode::Esc)) {
            DialogResult::Submit(outcome) => assert_eq!(outcome.disabled, Some(true)),
            _ => panic!("Esc should submit"),
        }
    }

    #[test]
    fn seen_tips_collapse_into_a_section_that_expands() {
        let tips = all_tips();
        let total = tips.len();
        // All but the first are already seen.
        let dialog_seen = ids(1);
        let mut dialog = TipsDialog::new(tips, dialog_seen, false, false);

        // Collapsed by default (there's an unseen tip): one unseen tip + header.
        assert!(dialog.seen_collapsed);
        assert_eq!(dialog.visible_rows().len(), 2);

        // Down onto the header, then expand: unseen tip + header + seen tips.
        dialog.handle_key(key(KeyCode::Down));
        assert!(dialog.on_seen_header());
        dialog.handle_key(key(KeyCode::Enter));
        assert!(!dialog.seen_collapsed);
        assert_eq!(dialog.visible_rows().len(), total + 1);
    }

    #[test]
    fn all_seen_expands_the_section_by_default() {
        let tips = all_tips();
        let total = tips.len();
        let dialog = TipsDialog::new(tips, ids(0), false, false);
        assert!(
            !dialog.seen_collapsed,
            "nothing new => show the seen tips rather than a lone header"
        );
        assert_eq!(dialog.visible_rows().len(), total + 1);
    }

    #[test]
    fn body_substitutes_the_new_from_selection_key_per_mode() {
        let normal = TipsDialog::new(all_tips(), vec![], false, false);
        let resolved = normal.resolve_body("Press {new_from_selection} now");
        assert!(
            !resolved.contains("{new_from_selection}"),
            "placeholder filled"
        );
        assert!(resolved.contains(&bindings::label(ActionId::NewFromSelection, false)));

        let strict = TipsDialog::new(all_tips(), vec![], false, true);
        let strict_resolved = strict.resolve_body("Press {new_from_selection} now");
        assert!(strict_resolved.contains(&bindings::label(ActionId::NewFromSelection, true)));
        // The two modes render different chords (Shift+N vs Ctrl+N).
        assert_ne!(resolved, strict_resolved);
    }

    #[test]
    fn body_substitutes_all_shortcut_placeholders() {
        let dialog = TipsDialog::new(all_tips(), vec![], false, false);
        let body = "{toggle_view} {diff} {settings} {help} {sort} {group} {archive} \
                    {snooze} {favorite} {serve} {tool_session}";
        let resolved = dialog.resolve_body(body);
        assert!(
            !resolved.contains('{'),
            "every placeholder filled: {resolved}"
        );
        assert!(resolved.contains(&bindings::label(ActionId::Diff, false)));
        assert!(resolved.contains(&bindings::label(ActionId::ToggleArchive, false)));
    }

    fn rendered_dialog(seen: Vec<String>) -> TipsDialog {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = load_theme("empire");
        let mut dialog = TipsDialog::new(all_tips(), seen, false, false);
        let backend = TestBackend::new(90, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dialog.render(f, f.area(), &theme))
            .unwrap();
        dialog
    }

    #[test]
    fn renders_title_focused_tip_and_footer() {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = load_theme("empire");
        let mut dialog = TipsDialog::new(all_tips(), vec![], false, false);
        let backend = TestBackend::new(90, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dialog.render(f, f.area(), &theme))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        assert!(out.contains("Tips"), "title renders\n{out}");
        assert!(
            out.contains(all_tips()[0].title),
            "focused tip title renders\n{out}"
        );
        assert!(out.contains("close"), "footer hint renders\n{out}");
    }

    #[test]
    fn clicking_a_row_focuses_and_marks_it_seen() {
        // Nothing seen => every row is an unseen tip, in catalog order.
        let mut dialog = rendered_dialog(vec![]);
        assert_eq!(dialog.cursor, 0);
        let target = dialog.row_rects[1];
        assert!(target.width > 0, "second row should be drawn");

        let result = dialog.handle_click(target.x + 1, target.y);
        assert!(matches!(result, Some(DialogResult::Continue)));
        assert_eq!(dialog.cursor, 1);
        let second_id = dialog.tips[1].id;
        assert!(dialog.is_seen(second_id), "clicked row is marked seen");
    }

    #[test]
    fn clicking_the_seen_header_expands_the_section() {
        // All but the first seen: row 0 = unseen tip, row 1 = collapsed header.
        let mut dialog = rendered_dialog(ids(1));
        assert!(dialog.seen_collapsed);
        let header = dialog.row_rects[1];
        assert!(header.width > 0);

        let result = dialog.handle_click(header.x + 1, header.y);
        assert!(matches!(result, Some(DialogResult::Continue)));
        assert!(
            !dialog.seen_collapsed,
            "clicking the header expands the section"
        );
    }

    #[test]
    fn empty_overlay_renders_a_message_and_stays_usable() {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = load_theme("empire");
        let mut dialog = TipsDialog::new(vec![], vec![], false, false);

        // Navigation and the toggle must not panic with an empty list.
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Up));
        dialog.handle_key(key(KeyCode::Char('d')));

        let backend = TestBackend::new(90, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dialog.render(f, f.area(), &theme))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        assert!(
            out.contains("No tips right now"),
            "empty-state message renders\n{out}"
        );

        // Esc still closes and reports the toggle the user flipped.
        match dialog.handle_key(key(KeyCode::Esc)) {
            DialogResult::Submit(outcome) => assert_eq!(outcome.disabled, Some(true)),
            _ => panic!("Esc should submit"),
        }
    }

    #[test]
    fn clicking_outside_the_modal_closes_it() {
        let mut dialog = rendered_dialog(vec![]);
        // (0, 0) is outside the centered modal.
        match dialog.handle_click(0, 0) {
            Some(DialogResult::Submit(outcome)) => {
                assert!(
                    !outcome.newly_seen.is_empty(),
                    "seen state is persisted on close"
                );
            }
            _ => panic!("outside click should close the overlay"),
        }
    }
}
