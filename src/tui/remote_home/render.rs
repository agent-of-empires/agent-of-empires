//! Render the remote-home session picker.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use unicode_width::UnicodeWidthStr;

use super::RemoteHomeState;
use crate::plugin::ui_state::Tone;
use crate::tui::components::truncate_to_width;
use crate::tui::plugin_ui;
use crate::tui::styles::{has_min_contrast, Theme};

const SELECTED_ROW_CONTRAST_RATIO: f32 = 3.0;

/// Widest a plugin's `row-column` cell may grow, in terminal cells, so a chatty
/// plugin cannot push the project path off the row. Matches the title column's
/// budget.
const ROW_COLUMN_MAX_WIDTH: usize = 24;

/// Gap between two plugins' cells inside the same column.
const ROW_COLUMN_GAP: &str = "  ";

/// One plugin's `row-column` text plus the tone to paint it in.
type RowColumnCell = (String, Option<Tone>);

/// A session's cells and the display width they occupy together.
type MeasuredCells = (Vec<RowColumnCell>, usize);

/// One session's `row-column` cells, truncated to the shared budget, plus the
/// display width they occupy. Separate from rendering so the column width can be
/// computed across every listed session before any row is painted: every row
/// pads to that one width, so a session with no cell leaves a blank of the same
/// size instead of shifting the path column (#2948). Returns width 0 when no
/// plugin pushed anything, which keeps the row identical to before this slot
/// rendered at all.
///
/// Budgets in terminal cells, not chars: plugin text is arbitrary, and a wide
/// glyph (an emoji status marker, CJK) paints two cells while counting as one
/// char, which would under-measure the column and shift the path after all.
fn row_column_cells(state: &RemoteHomeState, session_id: &str) -> MeasuredCells {
    let mut budget = ROW_COLUMN_MAX_WIDTH;
    let mut width = 0;
    let mut cells = Vec::new();
    for (text, tone) in plugin_ui::row_column_cells(&state.plugin_ui, session_id) {
        let gap = if cells.is_empty() {
            0
        } else {
            UnicodeWidthStr::width(ROW_COLUMN_GAP)
        };
        // Needs room for the gap plus at least one cell of text, else the
        // remaining plugins are dropped rather than rendered as a bare gap.
        if budget <= gap {
            break;
        }
        let text = truncate_to_width(&text, budget - gap);
        // Clamp rather than trust the helper: if it ever hands back more cells
        // than it was given, `budget -= gap + len` would underflow (panic in a
        // debug build) or wrap the budget wide open, voiding the cap this loop
        // exists to enforce.
        let len = UnicodeWidthStr::width(text.as_str()).min(budget - gap);
        width += gap + len;
        budget -= gap + len;
        cells.push((text, tone));
    }
    (cells, width)
}

fn selected_row_style(style: Style, theme: &Theme) -> Style {
    let Some(fg) = style.fg else {
        return style.fg(theme.text);
    };
    if has_min_contrast(fg, theme.session_selection, SELECTED_ROW_CONTRAST_RATIO) {
        style
    } else {
        style.fg(theme.text)
    }
}

pub fn render(frame: &mut Frame, area: Rect, theme: &Theme, state: &RemoteHomeState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
    render_header(frame, chunks[0], theme, state);
    render_list(frame, chunks[1], theme, state);
    render_footer(frame, chunks[2], theme, state);
}

fn render_header(frame: &mut Frame, area: Rect, theme: &Theme, state: &RemoteHomeState) {
    let spans = vec![
        Span::styled(
            " Remote agent sessions · ",
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            state.endpoint.base_url.clone(),
            Style::default().fg(theme.text),
        ),
        Span::raw(" "),
    ];
    let block = Block::default().borders(Borders::BOTTOM);
    let para = Paragraph::new(Line::from(spans)).block(block);
    frame.render_widget(para, area);
}

fn render_list(frame: &mut Frame, area: Rect, theme: &Theme, state: &RemoteHomeState) {
    if let Some(err) = &state.last_error {
        let para = Paragraph::new(format!(
            "Could not reach daemon at {}:\n\n{}\n\nPress r to retry, q to quit.",
            state.endpoint.base_url, err
        ))
        .style(Style::default().fg(theme.error));
        frame.render_widget(para, area);
        return;
    }
    if state.loading && state.sessions.is_empty() {
        let para =
            Paragraph::new("loading remote agent sessions…").style(Style::default().fg(theme.hint));
        frame.render_widget(para, area);
        return;
    }
    if state.sessions.is_empty() {
        let para = Paragraph::new(
            "No structured view sessions on this daemon.\n\nPress r to refresh, q to quit.\n\nAcp sessions are created via `aoe add --structured-view` on the host\n(or the web dashboard's New Session dialog).",
        )
        .style(Style::default().fg(theme.hint));
        frame.render_widget(para, area);
        return;
    }
    // Measure every row's plugin cells first so they all pad to one width.
    let plugin_cells: Vec<MeasuredCells> = state
        .sessions
        .iter()
        .map(|s| row_column_cells(state, &s.id))
        .collect();
    let plugin_width = plugin_cells.iter().map(|(_, w)| *w).max().unwrap_or(0);
    let items: Vec<ListItem> = state
        .sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let is_selected = idx == state.cursor;
            let title_style = Style::default().add_modifier(Modifier::BOLD);
            let status_style = Style::default().fg(theme.hint);
            let path_style = Style::default().fg(theme.dimmed);
            let readable = |style: Style| {
                if is_selected {
                    selected_row_style(style, theme)
                } else {
                    style
                }
            };
            let mut spans = vec![
                Span::styled(
                    format!(" {:<24}  ", truncate(&s.title, 24)),
                    readable(title_style),
                ),
                Span::styled(format!("{:<10}  ", s.status), readable(status_style)),
            ];
            if plugin_width > 0 {
                let (cells, width) = &plugin_cells[idx];
                for (i, (text, tone)) in cells.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::raw(ROW_COLUMN_GAP));
                    }
                    spans.push(Span::styled(
                        text.clone(),
                        readable(plugin_ui::tone_style(*tone, theme)),
                    ));
                }
                // Pad to the shared width, then the inter-column gap, so the
                // path starts at the same screen column on every row.
                spans.push(Span::raw(format!(
                    "{:width$}{ROW_COLUMN_GAP}",
                    "",
                    width = plugin_width - width
                )));
            }
            spans.push(Span::styled(s.project_path.clone(), readable(path_style)));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default())
        .highlight_style(
            Style::default()
                .bg(theme.session_selection)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    let mut list_state = ListState::default();
    list_state.select(Some(
        state.cursor.min(state.sessions.len().saturating_sub(1)),
    ));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme, state: &RemoteHomeState) {
    let mut spans: Vec<Span> = Vec::new();
    if let Some(text) = &state.status_text {
        spans.push(Span::styled(
            format!(" {text} · "),
            Style::default().fg(theme.hint),
        ));
    }
    spans.push(Span::styled(
        " j/k=navigate · Enter=open · r=refresh · q=quit ",
        Style::default().fg(theme.hint),
    ));
    let block = Block::default().borders(Borders::TOP);
    let para = Paragraph::new(Line::from(spans)).block(block);
    frame.render_widget(para, area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let take = max.saturating_sub(1);
        let truncated: String = s.chars().take(take).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::{DaemonEndpoint, Source};
    use crate::tui::remote_home::RemoteSession;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn state_with(sessions: &[&str], entries: serde_json::Value) -> RemoteHomeState {
        let mut state = RemoteHomeState::new(DaemonEndpoint::new(
            "http://127.0.0.1:8080".to_string(),
            None,
            Source::Env,
        ));
        state.loading = false;
        state.sessions = sessions
            .iter()
            .map(|id| RemoteSession {
                id: (*id).to_string(),
                title: format!("session {id}"),
                project_path: format!("/tmp/{id}"),
                status: "idle".to_string(),
                view: crate::session::View::Structured,
            })
            .collect();
        state.plugin_ui = serde_json::from_value(json!({
            "entries": entries,
            "notifications": [],
        }))
        .expect("snapshot deserializes");
        state
    }

    fn row_column(session_id: &str, text: &str) -> serde_json::Value {
        json!({"plugin_id": "gh", "slot": "row-column", "id": "st",
               "session_id": session_id, "payload": {"text": text}})
    }

    /// Screen column where `needle` starts on the first row carrying it. Indexes
    /// the per-cell strings from `rows`, one character per painted cell, so it is
    /// a real column: a byte offset would be skewed by the multi-byte `▸ `
    /// highlight symbol, and a character offset into the concatenated symbols
    /// would be skewed by any cell holding a multi-character cluster.
    fn column_of(painted: &[String], needle: &str) -> usize {
        let pat: Vec<char> = needle.chars().collect();
        painted
            .iter()
            .find_map(|line| {
                let chars: Vec<char> = line.chars().collect();
                chars.windows(pat.len()).position(|w| w == pat.as_slice())
            })
            .unwrap_or_else(|| panic!("{needle} missing from {painted:?}"))
    }

    /// The row text of every painted line, trailing blanks trimmed, with exactly
    /// one character per painted cell so a character index is a screen column.
    /// A cell can hold a multi-character cluster (an emoji with a presentation
    /// selector) and the trailing cell of a wide glyph holds an empty symbol, so
    /// both are folded to a single representative character.
    fn rows(state: &RemoteHomeState) -> Vec<String> {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).expect("terminal");
        terminal
            .draw(|f| render(f, f.area(), &theme, state))
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn row_shows_the_plugin_row_column_text() {
        let state = state_with(&["s1"], json!([row_column("s1", "CI failing")]));
        let painted = rows(&state);
        assert!(
            painted.iter().any(|l| l.contains("CI failing")),
            "{painted:?}"
        );
    }

    #[test]
    fn plugin_column_pads_so_the_path_stays_aligned() {
        // s2 has no cell; both rows must start the path at the same column.
        let state = state_with(
            &["s1", "s2"],
            json!([row_column("s1", "changes requested")]),
        );
        let painted = rows(&state);
        assert_eq!(
            column_of(&painted, "/tmp/s1"),
            column_of(&painted, "/tmp/s2")
        );
    }

    #[test]
    fn no_plugin_entries_reserve_no_width() {
        let painted = rows(&state_with(&["s1"], json!([])));
        // Highlight symbol (2) + title (1 + 24 + 2) + status (10 + 2), with no
        // plugin column and no gap for one.
        assert_eq!(column_of(&painted, "/tmp/s1"), 41);
    }

    #[test]
    fn long_plugin_text_is_capped_so_the_path_survives() {
        let long = "x".repeat(ROW_COLUMN_MAX_WIDTH + 20);
        let state = state_with(&["s1"], json!([row_column("s1", &long)]));
        let (cells, width) = row_column_cells(&state, "s1");
        assert_eq!(width, ROW_COLUMN_MAX_WIDTH);
        assert!(cells[0].0.ends_with('…'));
        let painted = rows(&state);
        assert!(painted.iter().any(|l| l.contains("/tmp/s1")), "{painted:?}");
    }

    #[test]
    fn wide_glyphs_are_budgeted_by_terminal_cells() {
        // 13 CJK chars paint 26 cells, over the 24-cell budget, though a char
        // count would have called it a comfortable fit.
        let wide = "検査失敗検査失敗検査失敗中";
        assert_eq!(wide.chars().count(), 13);
        let state = state_with(&["s1"], json!([row_column("s1", wide)]));
        let (cells, width) = row_column_cells(&state, "s1");
        assert!(width <= ROW_COLUMN_MAX_WIDTH, "{width} cells");
        assert!(cells[0].0.ends_with('…'));
        // And the painted column still lines up with a cell-less row. The four
        // CJK chars paint 8 cells, so the path starts 8 + 2 columns past the 41
        // it sits at with no plugin column; counting chars would have reserved 4
        // and left the two rows disagreeing.
        let both = state_with(&["s1", "s2"], json!([row_column("s1", "検査失敗")]));
        let painted = rows(&both);
        assert_eq!(column_of(&painted, "/tmp/s1"), 51);
        assert_eq!(column_of(&painted, "/tmp/s2"), 51);
    }

    #[test]
    fn emoji_presentation_status_text_stays_within_budget() {
        // "warning sign + VS16" is 2 cells as a cluster but its chars sum to 1,
        // which is exactly the text a CI-status plugin pushes. Budgeting per
        // char over-admitted, so this row underflowed the budget subtraction:
        // a panic in a debug build, and a wide-open cap in a release one.
        let state = state_with(
            &["s1", "s2"],
            json!([
                row_column("s1", "\u{26a0}\u{fe0f} CI failing on 5 checks"),
                row_column(
                    "s2",
                    "\u{2764}\u{fe0f}\u{2764}\u{fe0f} awaiting review from two people"
                )
            ]),
        );
        for id in ["s1", "s2"] {
            let (cells, width) = row_column_cells(&state, id);
            assert!(width <= ROW_COLUMN_MAX_WIDTH, "{id}: {width} cells");
            let painted: usize = cells
                .iter()
                .map(|(t, _)| UnicodeWidthStr::width(t.as_str()))
                .sum::<usize>()
                + ROW_COLUMN_GAP.len() * cells.len().saturating_sub(1);
            assert_eq!(painted, width, "{id}: measured width must match painted");
        }
        // The cap holds, so the path still renders and stays aligned.
        let painted = rows(&state);
        assert_eq!(
            column_of(&painted, "/tmp/s1"),
            column_of(&painted, "/tmp/s2")
        );
    }

    #[test]
    fn second_plugin_cell_is_dropped_when_the_budget_is_spent() {
        let state = state_with(
            &["s1"],
            json!([
                row_column("s1", &"y".repeat(ROW_COLUMN_MAX_WIDTH)),
                row_column("s1", "dropped")
            ]),
        );
        let (cells, width) = row_column_cells(&state, "s1");
        assert_eq!(cells.len(), 1);
        assert_eq!(width, ROW_COLUMN_MAX_WIDTH);
    }

    #[test]
    fn selected_row_style_preserves_readable_color() {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let style = Style::default().fg(theme.text);

        assert_eq!(selected_row_style(style, &theme).fg, Some(theme.text));
    }

    #[test]
    fn selected_row_style_sets_text_for_default_foreground() {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let style = Style::default();

        assert_eq!(selected_row_style(style, &theme).fg, Some(theme.text));
    }

    #[test]
    fn selected_row_style_falls_back_for_low_contrast_color() {
        let mut theme = crate::tui::styles::load_theme_with_mode("empire", false);
        theme.dimmed = theme.session_selection;
        let style = Style::default().fg(theme.dimmed);

        assert_eq!(selected_row_style(style, &theme).fg, Some(theme.text));
    }
}
