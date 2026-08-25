//! Render the remote-home session picker.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use unicode_width::UnicodeWidthStr;

use super::RemoteHomeState;
use crate::plugin::ui_state::Tone;
use crate::server::api::sessions::{
    ContextResumeAvailability, ContextResumeIndeterminateReason, ContextResumeUnavailableReason,
};
use crate::tui::components::truncate_to_width;
use crate::tui::plugin_ui;
use crate::tui::styles::{has_min_contrast, Theme};

const SELECTED_ROW_CONTRAST_RATIO: f32 = 3.0;

/// Widest the whole plugin region may grow, in terminal cells: the `row-badge`
/// chips, the `row-column` cells, and the gap between the two groups. One shared
/// cap rather than one per slot because the row already spends 41 cells on the
/// highlight symbol, title, and status before any plugin content, so a second
/// independent 24-cell group would push the project path past the right edge of
/// an 80-column terminal instead of merely squeezing it.
const PLUGIN_REGION_MAX_WIDTH: usize = 24;

/// Slice of the region the `row-badge` chips may claim, leaving the rest to the
/// wordier `row-column` text. Badges are terse state markers ("draft",
/// "approved"), so they need far less room than a status sentence.
const ROW_BADGE_MAX_WIDTH: usize = 10;

/// Gap between two plugins' cells inside a group, and between the two groups.
const PLUGIN_COLUMN_GAP: &str = "  ";

/// One plugin's cell text plus the tone to paint it in.
type PluginCell = (String, Option<Tone>);

/// A session's cells and the display width they occupy together.
type MeasuredCells = (Vec<PluginCell>, usize);

/// One session's cells for a slot, truncated to `budget`, plus the display width
/// they occupy. Separate from rendering so a group's width can be computed
/// across every listed session before any row is painted: every row pads to that
/// one width, so a session with no cell leaves a blank of the same size instead
/// of shifting the path column (#2948). Returns width 0 when no plugin pushed
/// anything, which keeps the row identical to before these slots rendered at
/// all.
///
/// Budgets in terminal cells, not chars: plugin text is arbitrary, and a wide
/// glyph (an emoji status marker, CJK) paints two cells while counting as one
/// char, which would under-measure the group and shift the path after all.
fn measured_cells(cells: Vec<PluginCell>, mut budget: usize) -> MeasuredCells {
    let mut width = 0;
    let mut kept = Vec::new();
    for (text, tone) in cells {
        let gap = if kept.is_empty() {
            0
        } else {
            UnicodeWidthStr::width(PLUGIN_COLUMN_GAP)
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
        kept.push((text, tone));
    }
    (kept, width)
}

/// This session's `row-badge` chips, measured against the badge sub-cap.
fn row_badge_cells(state: &RemoteHomeState, session_id: &str) -> MeasuredCells {
    measured_cells(
        plugin_ui::row_badge_cells(&state.plugin_ui, session_id),
        ROW_BADGE_MAX_WIDTH,
    )
}

/// This session's `row-column` cells, measured against whatever the badges left
/// of the shared region.
fn row_column_cells(state: &RemoteHomeState, session_id: &str, budget: usize) -> MeasuredCells {
    measured_cells(
        plugin_ui::row_column_cells(&state.plugin_ui, session_id),
        budget,
    )
}

/// Paint one group's cells and pad them out to `group_width`, then the gap that
/// separates the group from what follows. A zero-width group paints nothing, so
/// a picker with no plugin entries keeps its original row layout.
fn push_group(
    spans: &mut Vec<Span<'static>>,
    measured: &MeasuredCells,
    group_width: usize,
    theme: &Theme,
    is_selected: bool,
) {
    if group_width == 0 {
        return;
    }
    let (cells, width) = measured;
    for (i, (text, tone)) in cells.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(PLUGIN_COLUMN_GAP));
        }
        let style = plugin_ui::tone_style(*tone, theme);
        spans.push(Span::styled(
            text.clone(),
            if is_selected {
                selected_row_style(style, theme)
            } else {
                style
            },
        ));
    }
    spans.push(Span::raw(format!(
        "{:pad$}{PLUGIN_COLUMN_GAP}",
        "",
        pad = group_width - width
    )));
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

/// The picker lists structured rows only, so the reachable values are
/// `agent_handshake_required`, `fork_pending`, `no_target`, and absent (older
/// daemon). The rest are terminal-only and stay unreachable here until the
/// daemon can answer `session/load` capability for a structured row the way
/// `structured_fork_capable` answers `session/fork`.
fn context_resume_summary(availability: Option<ContextResumeAvailability>) -> &'static str {
    match availability {
        Some(ContextResumeAvailability::Available) => "ctx:yes",
        Some(ContextResumeAvailability::Indeterminate { .. }) => "ctx:check",
        Some(ContextResumeAvailability::Unavailable { .. }) => "ctx:no",
        None => "ctx:?",
    }
}

fn context_resume_detail(availability: Option<ContextResumeAvailability>) -> &'static str {
    match availability {
        Some(ContextResumeAvailability::Available) => "available",
        Some(ContextResumeAvailability::Indeterminate {
            reason: ContextResumeIndeterminateReason::RuntimeCheckRequired,
        }) => "runtime check required",
        Some(ContextResumeAvailability::Indeterminate {
            reason: ContextResumeIndeterminateReason::AgentHandshakeRequired,
        }) => "agent handshake required",
        Some(ContextResumeAvailability::Unavailable { reason }) => match reason {
            ContextResumeUnavailableReason::AgentUnsupported => "agent unsupported",
            ContextResumeUnavailableReason::SandboxUnsupported => "sandbox unsupported",
            ContextResumeUnavailableReason::CommandUnsupported => "command unsupported",
            ContextResumeUnavailableReason::ForcedFresh => "forced fresh",
            ContextResumeUnavailableReason::InvalidTarget => "invalid target",
            ContextResumeUnavailableReason::ForkPending => "fork pending",
            ContextResumeUnavailableReason::PreviousFailure => "previous failure",
            ContextResumeUnavailableReason::NoTarget => "no target",
        },
        None => "not reported",
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
            " Remote sessions · ",
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
            "No structured view sessions on this daemon.

Press r to refresh, q to quit.",
        )
        .style(Style::default().fg(theme.hint));
        frame.render_widget(para, area);
        return;
    }
    // Measure every row's plugin cells first so they all pad to one width. The
    // badges go first and eat into the shared region, so the column's budget is
    // whatever they leave: with no badges anywhere the column still gets the
    // whole region and the row renders exactly as it did before #2947.
    let badge_cells: Vec<MeasuredCells> = state
        .sessions
        .iter()
        .map(|s| row_badge_cells(state, &s.id))
        .collect();
    let badge_width = badge_cells.iter().map(|(_, w)| *w).max().unwrap_or(0);
    let column_budget = if badge_width == 0 {
        PLUGIN_REGION_MAX_WIDTH
    } else {
        PLUGIN_REGION_MAX_WIDTH
            .saturating_sub(badge_width + UnicodeWidthStr::width(PLUGIN_COLUMN_GAP))
    };
    let column_cells: Vec<MeasuredCells> = state
        .sessions
        .iter()
        .map(|s| row_column_cells(state, &s.id, column_budget))
        .collect();
    let column_width = column_cells.iter().map(|(_, w)| *w).max().unwrap_or(0);
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
                Span::styled(
                    format!("{:<11}  ", context_resume_summary(s.context_resume)),
                    readable(status_style),
                ),
            ];
            // Each group pads to its shared width, then the gap, so the path
            // starts at the same screen column on every row.
            push_group(
                &mut spans,
                &badge_cells[idx],
                badge_width,
                theme,
                is_selected,
            );
            push_group(
                &mut spans,
                &column_cells[idx],
                column_width,
                theme,
                is_selected,
            );
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
    let selected = state.sessions.get(state.cursor);
    spans.push(Span::styled(
        " j/k=navigate · Enter=open · r=refresh · q=quit ",
        Style::default().fg(theme.hint),
    ));
    if let Some(session) = selected {
        spans.push(Span::styled(
            format!(
                " · context resume: {} ",
                context_resume_detail(session.context_resume),
            ),
            Style::default().fg(theme.dimmed),
        ));
    }
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
        ))
        .unwrap();
        state.loading = false;
        state.sessions = sessions
            .iter()
            .map(|id| RemoteSession {
                id: (*id).to_string(),
                title: format!("session {id}"),
                project_path: format!("/tmp/{id}"),
                status: "idle".to_string(),
                context_resume: Some(ContextResumeAvailability::Indeterminate {
                    reason: ContextResumeIndeterminateReason::AgentHandshakeRequired,
                }),
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

    fn row_badge(session_id: &str, payload: serde_json::Value) -> serde_json::Value {
        json!({"plugin_id": "gh", "slot": "row-badge", "id": "pr",
               "session_id": session_id, "payload": payload})
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
        // Highlight, title, status and context-resume columns occupy 54 cells
        // before the path when no plugin column is present.
        assert_eq!(column_of(&painted, "/tmp/s1"), 54);
    }

    #[test]
    fn long_plugin_text_is_capped_so_the_path_survives() {
        let long = "x".repeat(PLUGIN_REGION_MAX_WIDTH + 20);
        let state = state_with(&["s1"], json!([row_column("s1", &long)]));
        let (cells, width) = row_column_cells(&state, "s1", PLUGIN_REGION_MAX_WIDTH);
        assert_eq!(width, PLUGIN_REGION_MAX_WIDTH);
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
        let (cells, width) = row_column_cells(&state, "s1", PLUGIN_REGION_MAX_WIDTH);
        assert!(width <= PLUGIN_REGION_MAX_WIDTH, "{width} cells");
        assert!(cells[0].0.ends_with('…'));
        // The four CJK chars paint 8 cells, followed by the two-cell gap.
        let both = state_with(&["s1", "s2"], json!([row_column("s1", "検査失敗")]));
        let painted = rows(&both);
        assert_eq!(
            column_of(&painted, "/tmp/s1"),
            column_of(&painted, "/tmp/s2")
        );
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
            let (cells, width) = row_column_cells(&state, id, PLUGIN_REGION_MAX_WIDTH);
            assert!(width <= PLUGIN_REGION_MAX_WIDTH, "{id}: {width} cells");
            let painted: usize = cells
                .iter()
                .map(|(t, _)| UnicodeWidthStr::width(t.as_str()))
                .sum::<usize>()
                + PLUGIN_COLUMN_GAP.len() * cells.len().saturating_sub(1);
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
                row_column("s1", &"y".repeat(PLUGIN_REGION_MAX_WIDTH)),
                row_column("s1", "dropped")
            ]),
        );
        let (cells, width) = row_column_cells(&state, "s1", PLUGIN_REGION_MAX_WIDTH);
        assert_eq!(cells.len(), 1);
        assert_eq!(width, PLUGIN_REGION_MAX_WIDTH);
    }

    #[test]
    fn row_shows_the_plugin_row_badge_text_before_the_column() {
        let state = state_with(
            &["s1"],
            json!([
                row_column("s1", "CI failing"),
                row_badge("s1", json!({"text": "draft", "tone": "warn"}))
            ]),
        );
        let painted = rows(&state);
        assert!(painted.iter().any(|l| l.contains("draft")), "{painted:?}");
        // Badges sit left of the column, matching the web's row line.
        assert!(column_of(&painted, "draft") < column_of(&painted, "CI failing"));
    }

    #[test]
    fn badges_share_the_plugin_region_instead_of_widening_it() {
        // A badge plus a column wordy enough to have filled the region alone.
        let long = "x".repeat(PLUGIN_REGION_MAX_WIDTH + 10);
        let state = state_with(
            &["s1"],
            json!([
                row_badge("s1", json!({"text": "draft"})),
                row_column("s1", &long)
            ]),
        );
        let painted = rows(&state);
        assert!(painted.iter().any(|l| l.contains("draft")), "{painted:?}");
        // The 41-cell prefix, the whole region, and the gap before the path:
        // the badge took its cells from the column, not from the path, which
        // starts exactly where a badge-free capped column leaves it.
        assert_eq!(
            column_of(&painted, "/tmp/s1"),
            41 + PLUGIN_REGION_MAX_WIDTH + 2
        );
    }

    #[test]
    fn badge_free_rows_leave_the_whole_region_to_the_column() {
        let long = "x".repeat(PLUGIN_REGION_MAX_WIDTH + 10);
        let state = state_with(&["s1"], json!([row_column("s1", &long)]));
        let (_, width) = row_column_cells(&state, "s1", PLUGIN_REGION_MAX_WIDTH);
        assert_eq!(width, PLUGIN_REGION_MAX_WIDTH);
    }

    #[test]
    fn badge_column_pads_so_the_path_stays_aligned() {
        // s2 has no badge; both rows must start the path at the same column.
        let state = state_with(
            &["s1", "s2"],
            json!([row_badge("s1", json!({"text": "changes requested"}))]),
        );
        let painted = rows(&state);
        assert_eq!(
            column_of(&painted, "/tmp/s1"),
            column_of(&painted, "/tmp/s2")
        );
    }

    #[test]
    fn cleared_and_icon_only_badges_reserve_no_width() {
        // Both render nothing, so the path sits where it does with no entries.
        for payload in [json!({"items": []}), json!({"icon": "git-pull-request"})] {
            let state = state_with(&["s1"], json!([row_badge("s1", payload.clone())]));
            assert_eq!(column_of(&rows(&state), "/tmp/s1"), 41, "{payload}");
        }
    }

    #[test]
    fn long_badge_text_is_capped_to_the_badge_budget() {
        let long = "b".repeat(ROW_BADGE_MAX_WIDTH + 20);
        let state = state_with(&["s1"], json!([row_badge("s1", json!({"text": long}))]));
        let (cells, width) = row_badge_cells(&state, "s1");
        assert_eq!(width, ROW_BADGE_MAX_WIDTH);
        assert!(cells[0].0.ends_with('\u{2026}'));
        let painted = rows(&state);
        assert!(painted.iter().any(|l| l.contains("/tmp/s1")), "{painted:?}");
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
    #[test]
    fn missing_context_metadata_is_visible_without_disabling_enter() {
        let mut state = state_with(&["s1"], json!([]));
        state.sessions[0].context_resume = None;

        let painted = rows(&state);
        assert!(
            painted.iter().any(|line| line.contains("ctx:?")),
            "{painted:?}"
        );
        assert!(
            painted.iter().any(|line| line.contains("not reported")),
            "{painted:?}"
        );
        assert!(
            painted.iter().any(|line| line.contains("Enter=open")),
            "{painted:?}"
        );
    }
}
