//! Help overlay component.
//!
//! The overlay claims most of the viewport, flows the shortcut sections
//! across as many columns as the width allows, and scrolls vertically
//! when even a single column does not fit. Sizing is driven by viewport
//! dimensions so the same render path works on a 24-row phone and a
//! 60-row desktop.

use ratatui::prelude::*;
use ratatui::widgets::*;
use unicode_width::UnicodeWidthStr;

use crate::session::config::SortOrder;
use crate::tui::styles::Theme;

/// Width of the key column, in cells. 11 fits the longest key strings
/// in either keymap (`Home/End/G`, `Shift+drag` = 10 cells) with one
/// cell of padding before the description.
const KEY_FIELD_WIDTH: usize = 11;
/// Cells of left indent before each shortcut row.
const ROW_INDENT: usize = 2;
/// Space between adjacent columns.
const COLUMN_GAP: u16 = 3;
/// Horizontal padding inside the dialog border.
const INNER_PADDING_X: u16 = 1;
/// Below this viewport width the overlay drops its margins and uses the
/// whole area; otherwise a tiny phone viewport loses too many cells to
/// chrome.
const SMALL_VIEWPORT_WIDTH: u16 = 40;
/// Same idea for height: drop top/bottom margin below this.
const SMALL_VIEWPORT_HEIGHT: u16 = 16;

fn shortcuts(strict: bool, live_on_enter: bool) -> Vec<(&'static str, Vec<(String, String)>)> {
    use crate::tui::home::bindings::{self, HelpSection as Sec};

    let (enter_desc, tab_desc) = if live_on_enter {
        ("Live mode (send keys to agent)", "Attach to tmux session")
    } else {
        ("Attach to tmux session", "Live mode (send keys to agent)")
    };

    // Action rows are generated from the shared keybinding registry, bucketed
    // by section in table order, so the help labels can never drift from the
    // actual bindings. `label` formats each chord for the active mode; an
    // action with no binding in this mode (only NextWaiting, in strict) falls
    // back to its non-strict label so it stays discoverable.
    let mut actions: Vec<(String, String)> = Vec::new();
    let mut attention: Vec<(String, String)> = Vec::new();
    let mut views: Vec<(String, String)> = Vec::new();
    let mut other: Vec<(String, String)> = Vec::new();
    for b in bindings::BINDINGS {
        let Some(help) = &b.help else { continue };
        // The unread toggle is fully removed when the feature is off, so the
        // help overlay shouldn't advertise a dead key (unlike the sort-gated
        // Attention rows, which stay listed because that's a transient view
        // state, not a disabled feature).
        if b.id == bindings::ActionId::ToggleUnread && !crate::session::unread_enabled() {
            continue;
        }
        let mut label = bindings::label(b.id, strict);
        if label.is_empty() {
            label = bindings::label(b.id, false);
        }
        if label.is_empty() {
            continue;
        }
        let row = (label, help.desc.to_string());
        match help.section {
            Sec::Actions => actions.push(row),
            Sec::Attention => attention.push(row),
            Sec::Views => views.push(row),
            Sec::Other => other.push(row),
        }
    }

    // Enter / Tab are structural keys (not relocatable registry actions); they
    // lead the Actions section and swap descriptions with the attach mode.
    let mut actions_rows = vec![
        ("Enter".to_string(), enter_desc.to_string()),
        ("Tab".to_string(), tab_desc.to_string()),
    ];
    actions_rows.append(&mut actions);

    // Non-action rows with no single registry binding.
    views.push(("< >".to_string(), "Resize list panel".to_string()));
    other.push(("n/N".to_string(), "Next/prev match".to_string()));
    other.push((
        "Ctrl+x".to_string(),
        "Dismiss update bar (this session)".to_string(),
    ));
    other.push((
        "Shift+drag".to_string(),
        "Select text in preview".to_string(),
    ));
    other.push((
        "Drag".to_string(),
        "Select + copy preview (live mode)".to_string(),
    ));
    other.push(("Ctrl+K".to_string(), "Command palette".to_string()));
    // Tips has no global hotkey (it's palette / badge driven), so it isn't in
    // the registry-derived rows above; surface it here so `?` still documents it.
    other.push((
        "\u{1f4a1}".to_string(),
        "Tips (badge, or Ctrl+K \u{2192} \"tips\")".to_string(),
    ));

    // Navigation is mode-invariant except the collapse row: in non-strict mode
    // bare `h` is the contextual snooze key, so only `<-` is advertised for
    // collapse; in strict mode `h` always collapses.
    let nav_collapse = if strict { "h/\u{2190}" } else { "\u{2190}" };
    let navigation = vec![
        ("j/\u{2193}".to_string(), "Move down".to_string()),
        ("k/\u{2191}".to_string(), "Move up".to_string()),
        (nav_collapse.to_string(), "Collapse group".to_string()),
        ("l/\u{2192}".to_string(), "Expand group".to_string()),
        ("Home/End/G".to_string(), "Go to top / bottom".to_string()),
        (
            "PgUp/Dn".to_string(),
            "Move 10 (also Shift+\u{2191}/\u{2193}, { })".to_string(),
        ),
    ];

    let actions_title = if strict {
        "Actions (strict mode)"
    } else {
        "Actions"
    };
    vec![
        ("Navigation", navigation),
        (actions_title, actions_rows),
        ("Attention (Attention sort only, except Archive)", attention),
        ("Views", views),
        ("Other", other),
    ]
}

struct HelpSection {
    title: &'static str,
    rows: Vec<(String, String)>,
}

fn build_sections(strict: bool, sort_order: SortOrder, live_on_enter: bool) -> Vec<HelpSection> {
    let raw = shortcuts(strict, live_on_enter);
    let sort_label = format!("(current sort: {})", sort_order.label());
    raw.into_iter()
        .map(|(title, keys)| {
            let mut rows: Vec<(String, String)> = keys;
            if title == "Views" {
                rows.push((String::new(), sort_label.clone()));
            }
            HelpSection { title, rows }
        })
        .collect()
}

#[cfg(test)]
fn section_height(section: &HelpSection) -> usize {
    1 + section.rows.len()
}

/// Minimum width a column needs to render every row of every section
/// without truncating the description.
fn min_column_width(sections: &[HelpSection]) -> u16 {
    let row_width = sections
        .iter()
        .flat_map(|s| s.rows.iter().map(|(_, d)| d.width()))
        .max()
        .unwrap_or(0)
        + ROW_INDENT
        + KEY_FIELD_WIDTH;
    let title_width = sections.iter().map(|s| s.title.width()).max().unwrap_or(0);
    row_width.max(title_width) as u16
}

/// Pick a column count that fits `inner_width` while giving each column
/// at least `min_col` cells. Capped at `max_cols` so we never produce
/// more columns than there are sections.
fn pick_columns(inner_width: u16, min_col: u16, max_cols: usize) -> usize {
    if max_cols == 0 {
        return 0;
    }
    let mut n = 1usize;
    while n < max_cols {
        let candidate = (n + 1) as u16;
        let gaps = COLUMN_GAP.saturating_mul(candidate - 1);
        let total = candidate.saturating_mul(min_col).saturating_add(gaps);
        if total <= inner_width {
            n += 1;
        } else {
            break;
        }
    }
    n
}

/// Spread `n` sections across `n_cols` columns, preserving reading
/// order. Returns one Vec of section indices per column.
///
/// The split is by section count rather than by height: with the small
/// number of sections in the help (currently 4) and similar densities,
/// count-based balancing keeps left-to-right reading intact and
/// produces close-to-equal column heights in practice. Height-based
/// bin-packing would have to reorder sections to do better.
fn distribute_sections(n: usize, n_cols: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![Vec::new(); n_cols.max(1)];
    }
    let n_cols = n_cols.clamp(1, n);
    let per_col = n / n_cols;
    let extra = n % n_cols;
    let mut cols: Vec<Vec<usize>> = Vec::with_capacity(n_cols);
    let mut idx = 0;
    for c in 0..n_cols {
        let count = per_col + if c < extra { 1 } else { 0 };
        cols.push((idx..idx + count).collect());
        idx += count;
    }
    cols
}

fn render_column_lines(
    sections: &[HelpSection],
    indices: &[usize],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for (i, &si) in indices.iter().enumerate() {
        let section = &sections[si];
        lines.push(Line::from(Span::styled(
            section.title.to_string(),
            Style::default().fg(theme.accent).bold(),
        )));
        for (key, desc) in &section.rows {
            let pad = KEY_FIELD_WIDTH.saturating_sub(key.width());
            let key_cell = format!("{}{}{}", " ".repeat(ROW_INDENT), key, " ".repeat(pad));
            lines.push(Line::from(vec![
                Span::styled(key_cell, Style::default().fg(theme.waiting)),
                Span::styled(desc.clone(), Style::default().fg(theme.text)),
            ]));
        }
        if i + 1 != indices.len() {
            lines.push(Line::from(""));
        }
    }
    lines
}

fn compute_dialog_area(area: Rect) -> Rect {
    // Reserve a small constant margin so the overlay almost fills the
    // viewport but still leaves a visual frame around it on every
    // sensible terminal size. On a 100-cell-wide terminal that leaves
    // 96 cells for the dialog, which is enough for the canonical 2-col
    // layout (two ~43-cell columns plus borders and gap).
    let margin_x = if area.width < SMALL_VIEWPORT_WIDTH {
        0
    } else {
        (area.width / 40).clamp(1, 4)
    };
    let margin_y = if area.height < SMALL_VIEWPORT_HEIGHT {
        0
    } else {
        (area.height / 30).clamp(1, 2)
    };
    Rect {
        x: area.x + margin_x,
        y: area.y + margin_y,
        width: area.width.saturating_sub(2 * margin_x),
        height: area.height.saturating_sub(2 * margin_y),
    }
}

fn split_footer(inner: Rect) -> (Rect, Option<Rect>) {
    if inner.height < 3 {
        return (inner, None);
    }
    let lines_area = Rect {
        height: inner.height - 1,
        ..inner
    };
    let footer = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    (lines_area, Some(footer))
}

pub struct HelpOverlay;

impl HelpOverlay {
    /// Render the help overlay. `scroll` is mutated in place: it is
    /// clamped to the valid range so that overshoot from input handlers
    /// (for example, treating `End` as `u16::MAX`) is naturally
    /// corrected on the next paint.
    pub fn render(
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        sort_order: SortOrder,
        strict_hotkeys: bool,
        live_on_enter: bool,
        scroll: &mut u16,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let dialog_area = compute_dialog_area(area);
        frame.render_widget(Clear, dialog_area);

        let sections = build_sections(strict_hotkeys, sort_order, live_on_enter);

        let version = format!(" Agent of Empires v{} ", env!("CARGO_PKG_VERSION"));
        let block = Block::default()
            .style(Style::default().bg(theme.background))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .title(Line::styled(
                " Keyboard Shortcuts ",
                Style::default().fg(theme.title).bold(),
            ))
            .title_bottom(Line::styled(version, Style::default().fg(theme.dimmed)).right_aligned());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let (content_area, footer_area) = split_footer(inner);
        let usable_width = content_area.width.saturating_sub(2 * INNER_PADDING_X);

        let min_col = min_column_width(&sections);
        let n_cols = pick_columns(usable_width, min_col, sections.len()).max(1);
        let distribution = distribute_sections(sections.len(), n_cols);
        let columns: Vec<Vec<Line>> = distribution
            .iter()
            .map(|idxs| render_column_lines(&sections, idxs, theme))
            .collect();

        let max_col_height = columns.iter().map(|c| c.len()).max().unwrap_or(0) as u16;
        let max_scroll = max_col_height.saturating_sub(content_area.height);
        *scroll = (*scroll).min(max_scroll);
        let scroll_offset = *scroll;

        if n_cols > 0 && usable_width > 0 {
            let total_gap = COLUMN_GAP.saturating_mul(n_cols as u16 - 1);
            let base = usable_width.saturating_sub(total_gap) / n_cols as u16;
            let extra = usable_width.saturating_sub(total_gap) % n_cols as u16;

            let mut x = content_area.x + INNER_PADDING_X;
            for (i, lines) in columns.into_iter().enumerate() {
                let w = base + if (i as u16) < extra { 1 } else { 0 };
                if w == 0 {
                    continue;
                }
                let col_area = Rect {
                    x,
                    y: content_area.y,
                    width: w,
                    height: content_area.height,
                };
                let paragraph = Paragraph::new(lines).scroll((scroll_offset, 0));
                frame.render_widget(paragraph, col_area);
                x = x.saturating_add(w).saturating_add(COLUMN_GAP);
            }
        }

        if let Some(footer) = footer_area {
            let mut hint = String::from("?/q/Esc close");
            if max_scroll > 0 {
                hint.push_str(&format!(
                    "    ↑/↓ scroll  PgUp/PgDn page  g/G top/bottom  [{}/{}]",
                    scroll_offset, max_scroll
                ));
            }
            let footer_para = Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme.dimmed),
            )));
            let footer_inner = Rect {
                x: footer.x + INNER_PADDING_X,
                y: footer.y,
                width: footer.width.saturating_sub(2 * INNER_PADDING_X),
                height: footer.height,
            };
            frame.render_widget(footer_para, footer_inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_contains_resize_shortcut() {
        for strict in [false, true] {
            let all = shortcuts(strict, false);
            let views_section = all.iter().find(|(name, _)| *name == "Views");
            assert!(views_section.is_some(), "Views section should exist");
            let (_, keys) = views_section.unwrap();
            assert!(
                keys.iter().any(|(k, _)| *k == "< >"),
                "Views section should contain < > resize shortcut"
            );
        }
    }

    #[test]
    fn help_shows_corrected_labels_for_relocated_actions() {
        // The six strict-mode relocations must surface with their corrected
        // chords in the `?` overlay, not the pre-fix labels. (bindings.rs
        // `labels_match_mode` pins `label()`; this pins that the overlay
        // actually renders those labels in both modes.)
        // Format: (desc substring, non-strict key, strict key).
        let cases = [
            ("Diff view", "D", "Ctrl+D"),
            ("Serve", "R", "Ctrl+R"),
            ("Attach to terminal", "T", "Ctrl+T"),
            ("New from selection", "N", "Ctrl+N"),
            ("Projects", "p", "P"),
            ("Profiles", "P", "Ctrl+P"),
        ];
        for (desc_sub, non_strict_key, strict_key) in cases {
            for (strict, expected_key) in [(false, non_strict_key), (true, strict_key)] {
                let all = shortcuts(strict, false);
                let found = all.iter().any(|(_, keys)| {
                    keys.iter()
                        .any(|(k, desc)| k == expected_key && desc.contains(desc_sub))
                });
                assert!(
                    found,
                    "help overlay (strict={strict}) should list '{expected_key}' for '{desc_sub}'"
                );
            }
        }
    }

    #[test]
    fn help_lists_snooze() {
        // PR #1084 introduced the snooze primitive (H in strict mode, h in
        // non-strict) but did not advertise it in the help overlay. Lock the
        // listing in so a future binding rename keeps the docs honest.
        for (strict, expected_key) in [(false, "h"), (true, "H")] {
            let all = shortcuts(strict, false);
            let attention = all
                .iter()
                .find(|(name, _)| name.starts_with("Attention"))
                .expect("Attention section should exist");
            let (_, keys) = attention;
            assert!(
                keys.iter()
                    .any(|(k, desc)| *k == expected_key && desc.contains("Snooze")),
                "Attention section should contain {expected_key} Snooze entry (strict={strict})"
            );
        }
    }

    #[test]
    fn attention_section_groups_triage_bindings() {
        // The Attention section bundles snooze/archive/favorite/next-waiting
        // so users see them together instead of scattered across Navigation
        // and Actions.
        for strict in [false, true] {
            let all = shortcuts(strict, false);
            let attention = all
                .iter()
                .find(|(name, _)| name.starts_with("Attention"))
                .expect("Attention section should exist");
            let (_, keys) = attention;
            for needle in ["favorite", "Snooze", "Archive", "waiting"] {
                assert!(
                    keys.iter().any(|(_, desc)| desc.contains(needle)),
                    "Attention section should mention {needle} (strict={strict})"
                );
            }
            // And those should no longer live in Actions or Navigation.
            for other_section in ["Navigation", "Actions", "Actions (strict mode)"] {
                if let Some((_, other)) = all.iter().find(|(n, _)| *n == other_section) {
                    for needle in ["favorite", "Snooze", "Archive", "waiting"] {
                        assert!(
                            !other.iter().any(|(_, desc)| desc.contains(needle)),
                            "{other_section} should not still contain {needle} \
                             (strict={strict})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn help_lists_command_palette() {
        // Asserts both keymaps surface the Ctrl+K command palette entry in
        // their "Other" section so users can discover the palette from `?`.
        for strict in [false, true] {
            let all = shortcuts(strict, false);
            let other = all
                .iter()
                .find(|(name, _)| *name == "Other")
                .expect("Other section should exist");
            let (_, keys) = other;
            assert!(
                keys.iter()
                    .any(|(k, desc)| *k == "Ctrl+K" && desc.contains("Command palette")),
                "Other section should contain Ctrl+K Command palette (strict={strict})"
            );
        }
    }

    #[test]
    fn enter_and_tab_swap_descriptions_with_default_attach_mode() {
        // Enter and Tab are complements: whichever Enter doesn't do,
        // Tab does. The help overlay has to reflect the user's current
        // `default_attach_mode` so the two rows aren't lying.
        for strict in [false, true] {
            for live_on_enter in [false, true] {
                let all = shortcuts(strict, live_on_enter);
                let actions_name = if strict {
                    "Actions (strict mode)"
                } else {
                    "Actions"
                };
                let (_, keys) = all
                    .iter()
                    .find(|(n, _)| *n == actions_name)
                    .expect("Actions section should exist");
                let enter = keys
                    .iter()
                    .find(|(k, _)| *k == "Enter")
                    .expect("Enter entry");
                let tab = keys.iter().find(|(k, _)| *k == "Tab").expect("Tab entry");
                if live_on_enter {
                    assert!(
                        enter.1.contains("Live mode"),
                        "live_on_enter=true → Enter should say Live mode, got {:?}",
                        enter.1
                    );
                    assert!(
                        tab.1.contains("tmux"),
                        "live_on_enter=true → Tab should say tmux, got {:?}",
                        tab.1
                    );
                } else {
                    assert!(
                        enter.1.contains("tmux"),
                        "live_on_enter=false → Enter should say tmux, got {:?}",
                        enter.1
                    );
                    assert!(
                        tab.1.contains("Live mode"),
                        "live_on_enter=false → Tab should say Live mode, got {:?}",
                        tab.1
                    );
                }
            }
        }
    }

    #[test]
    fn pick_columns_grows_with_width() {
        // 40 cells per column is enough for the canonical help rows; verify
        // the picker scales with viewport width.
        let min = 40;
        assert_eq!(pick_columns(30, min, 4), 1);
        assert_eq!(pick_columns(80, min, 4), 1);
        assert_eq!(pick_columns(85, min, 4), 2);
        assert_eq!(pick_columns(130, min, 4), 3);
        assert_eq!(pick_columns(180, min, 4), 4);
        assert_eq!(pick_columns(300, min, 4), 4);
    }

    #[test]
    fn pick_columns_capped_at_section_count() {
        assert_eq!(pick_columns(1000, 10, 2), 2);
        assert_eq!(pick_columns(1000, 10, 0), 0);
    }

    #[test]
    fn distribute_preserves_reading_order() {
        // For 4 sections across 1..=4 columns the partition must be
        // contiguous so the user reads top-to-bottom, left-to-right.
        for n_cols in 1..=4 {
            let cols = distribute_sections(4, n_cols);
            let mut prev = -1i32;
            for c in &cols {
                for &i in c {
                    assert!(
                        (i as i32) > prev,
                        "section {i} out of order in {n_cols}-col layout"
                    );
                    prev = i as i32;
                }
            }
            let total: usize = cols.iter().map(|c| c.len()).sum();
            assert_eq!(total, 4, "every section must be placed");
        }
    }

    #[test]
    fn distribute_balances_section_counts() {
        // 4 sections into 3 columns should give [2, 1, 1] (extras at the
        // front), keeping column heights as close as possible.
        let cols = distribute_sections(4, 3);
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].len(), 2);
        assert_eq!(cols[1].len(), 1);
        assert_eq!(cols[2].len(), 1);
    }

    #[test]
    fn render_keeps_max_col_height_balanced() {
        // Sanity check: the chosen 4 → 3 split keeps max column height
        // below the naive [1, 3, 0] alternative that would happen if we
        // pushed all extras to one column.
        let sections = build_sections(false, SortOrder::Newest, false);
        let heights: Vec<usize> = sections.iter().map(section_height).collect();
        let cols = distribute_sections(sections.len(), 3);
        let max_h = cols
            .iter()
            .map(|c| c.iter().map(|&i| heights[i]).sum::<usize>())
            .max()
            .unwrap_or(0);
        // Sum of the two largest sections is an upper bound on the
        // tallest 3-column slot; assert we land at or below it.
        let mut sorted = heights.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        let bound = sorted[0] + sorted.get(1).copied().unwrap_or(0);
        assert!(
            max_h <= bound,
            "3-col layout produced max column {max_h} > top-two sum {bound}"
        );
    }

    fn render_to_buffer(width: u16, height: u16, scroll: &mut u16) -> ratatui::buffer::Buffer {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = crate::tui::styles::Theme::default();
        terminal
            .draw(|frame| {
                let area = frame.area();
                HelpOverlay::render(frame, area, &theme, SortOrder::Newest, false, false, scroll);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_contains(buf: &ratatui::buffer::Buffer, needle: &str) -> bool {
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            if row.contains(needle) {
                return true;
            }
        }
        false
    }

    #[test]
    fn render_fills_wide_viewport_with_multiple_columns() {
        // A 140x40 viewport leaves enough room for at least 2 side-by-side
        // sections; the Actions header must land on the same row as
        // Navigation if columns are working.
        let mut scroll = 0;
        let buf = render_to_buffer(140, 40, &mut scroll);
        // Find Navigation row and check that Actions / Views / Other
        // start on the same line in another column.
        let mut nav_row = None;
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            if row.contains("Navigation") {
                nav_row = Some((y, row));
                break;
            }
        }
        let (y, row) = nav_row.expect("Navigation header should render");
        let has_second_section = ["Actions", "Views", "Other"]
            .iter()
            .any(|s| row.contains(s));
        assert!(
            has_second_section,
            "expected a second section header on row {y}, got: {row:?}"
        );
        assert!(buf_dialog_takes_most_of_area(&buf, 140, 40));
    }

    fn buf_dialog_takes_most_of_area(buf: &ratatui::buffer::Buffer, w: u16, h: u16) -> bool {
        // The rounded border uses '╭'/'╮'/'╰'/'╯' corners; find the
        // top-left corner and assert it sits within the outer two cells
        // so the dialog is "nearly fullscreen".
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].symbol() == "╭" {
                    return x <= 4 && y <= 2 && x + 1 < w && y + 1 < h;
                }
            }
        }
        false
    }

    #[test]
    fn render_shows_scroll_hint_when_overflowing() {
        // 60x14 is too small to fit all rows; the footer should show the
        // scroll position.
        let mut scroll = 0;
        let buf = render_to_buffer(60, 14, &mut scroll);
        assert!(
            buffer_contains(&buf, "scroll"),
            "scroll hint should be visible when content overflows"
        );
    }

    #[test]
    fn render_clamps_scroll_to_max() {
        // u16::MAX overshoot must be clamped after render so 'g' /
        // 'Home' from that state actually lands on something sensible.
        let mut scroll = u16::MAX;
        let _buf = render_to_buffer(60, 14, &mut scroll);
        assert!(
            scroll < u16::MAX,
            "scroll should be clamped to the layout max"
        );
    }

    #[test]
    fn render_with_no_overflow_keeps_scroll_zero() {
        // A roomy viewport fits all content; scroll stays at 0 even if
        // we pass a positive value in (it gets clamped to max=0).
        let mut scroll = 5;
        let _buf = render_to_buffer(200, 60, &mut scroll);
        assert_eq!(scroll, 0, "no overflow should clamp scroll to 0");
    }
}
