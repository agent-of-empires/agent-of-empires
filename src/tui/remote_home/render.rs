//! Render the remote-home session picker.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use super::RemoteHomeState;
use crate::tui::styles::{has_min_contrast, Theme};

const SELECTED_ROW_CONTRAST_RATIO: f32 = 3.0;

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
    let items: Vec<ListItem> = state
        .sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let is_selected = idx == state.cursor;
            let title_style = Style::default().add_modifier(Modifier::BOLD);
            let status_style = Style::default().fg(theme.hint);
            let path_style = Style::default().fg(theme.dimmed);
            let line = Line::from(vec![
                Span::styled(
                    format!(" {:<24}  ", truncate(&s.title, 24)),
                    if is_selected {
                        selected_row_style(title_style, theme)
                    } else {
                        title_style
                    },
                ),
                Span::styled(
                    format!("{:<10}  ", s.status),
                    if is_selected {
                        selected_row_style(status_style, theme)
                    } else {
                        status_style
                    },
                ),
                Span::styled(
                    s.project_path.clone(),
                    if is_selected {
                        selected_row_style(path_style, theme)
                    } else {
                        path_style
                    },
                ),
            ]);
            ListItem::new(line)
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
