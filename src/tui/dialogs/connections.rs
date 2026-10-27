//! The Connections hub behind R: this computer's Web dashboard, plus its Daemon whenever the
//! Daemon needs attention. While the Daemon is offline it is also the home screen's takeover.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::styles::Theme;

/// Why the Daemon row is listed. A healthy Daemon has no row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonAttention {
    Offline,
    OutcomeUnknown,
    ReadOnly,
    NativeUnavailable,
}

impl DaemonAttention {
    /// The footer's one-state label for the R entry.
    pub fn footer_label(self) -> &'static str {
        match self {
            DaemonAttention::Offline => "Daemon offline",
            DaemonAttention::OutcomeUnknown => "Runtime outcome unknown",
            DaemonAttention::ReadOnly => "Unavailable",
            DaemonAttention::NativeUnavailable => "Native unavailable",
        }
    }

    /// A problem is red and outranks a limit, which is dim.
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            DaemonAttention::Offline | DaemonAttention::OutcomeUnknown
        )
    }

    fn status(self) -> &'static str {
        match self {
            DaemonAttention::Offline => "Not responding",
            DaemonAttention::OutcomeUnknown => "Outcome unknown",
            DaemonAttention::ReadOnly => "Read-only",
            DaemonAttention::NativeUnavailable => "Native unavailable",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            DaemonAttention::Offline => {
                "Your Sessions keep running in tmux, but the list can't update until it's back."
            }
            DaemonAttention::OutcomeUnknown => {
                "A change was sent, but whether it applied isn't known yet. Check the list before you retry it."
            }
            DaemonAttention::ReadOnly => {
                "This Daemon refuses changes, so creating, deleting and similar actions don't work here."
            }
            DaemonAttention::NativeUnavailable => {
                "Sessions can be listed and changed, but not attached to or typed into from this terminal."
            }
        }
    }
}

/// What the Web dashboard row says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebState {
    Off,
    /// Served, with how it's reachable ("Tailscale", "Cloudflare", "Local network").
    On(Option<&'static str>),
    Disabled,
    NeedsDaemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Daemon,
    Web,
}

pub struct ConnectionsView {
    host: Option<String>,
    daemon: Option<DaemonAttention>,
    /// Why the Daemon couldn't be reached, when offline.
    reason: Option<String>,
    web: WebState,
    selected: usize,
    row_rects: Vec<(Row, Rect)>,
}

impl ConnectionsView {
    pub fn new(daemon: Option<DaemonAttention>, web: WebState) -> Self {
        Self {
            host: host_name(),
            daemon,
            reason: None,
            web,
            selected: 0,
            row_rects: Vec::new(),
        }
    }

    /// The home screen's takeover while the Daemon can't be reached.
    pub fn offline(reason: Option<&str>) -> Self {
        Self {
            reason: reason.map(str::to_owned),
            ..Self::new(Some(DaemonAttention::Offline), WebState::NeedsDaemon)
        }
    }

    /// Follow the Daemon and Web dashboard while open, keeping the selected row.
    pub fn refresh(&mut self, daemon: Option<DaemonAttention>, web: WebState) {
        let selected = self.rows().get(self.selected).copied();
        self.daemon = daemon;
        self.web = web;
        self.selected = self
            .rows()
            .iter()
            .position(|row| Some(*row) == selected)
            .unwrap_or(0);
    }

    /// The Web dashboard row's state, so a refresh can keep a known transport.
    pub fn web(&self) -> &WebState {
        &self.web
    }

    fn rows(&self) -> Vec<Row> {
        self.daemon
            .map(|_| Row::Daemon)
            .into_iter()
            .chain([Row::Web])
            .collect()
    }

    fn offline_takeover(&self) -> bool {
        self.daemon == Some(DaemonAttention::Offline)
    }

    /// Enter on the Web dashboard row submits; the Daemon row has nothing to open.
    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        let rows = self.rows();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Cancel,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                DialogResult::Continue
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(rows.len() - 1);
                DialogResult::Continue
            }
            KeyCode::Enter if rows.get(self.selected) == Some(&Row::Web) => {
                DialogResult::Submit(())
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<()>> {
        let hit = super::hit(&self.row_rects, col, row)?;
        self.selected = self.rows().iter().position(|r| *r == hit).unwrap_or(0);
        Some(match hit {
            Row::Web => DialogResult::Submit(()),
            Row::Daemon => DialogResult::Continue,
        })
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        const NAME: usize = 18;
        // Narrow terminals shrink the dialog, so the details wrap to what's really there.
        let width = area.width.min(76);
        let inner_width = width.saturating_sub(4) as usize;
        let detail_width = inner_width.saturating_sub(NAME + 2);
        let dim = Style::default().fg(theme.dimmed);
        let key = Style::default().fg(theme.accent).bold();

        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                self.host
                    .as_deref()
                    .map_or("THIS COMPUTER".to_string(), |host| {
                        format!("THIS COMPUTER \u{00B7} {host}")
                    }),
                dim,
            )),
            Line::raw(""),
        ];
        // Line index of each row's first line, for click targets.
        let mut row_lines = Vec::new();
        for (i, row) in self.rows().into_iter().enumerate() {
            let selected = i == self.selected;
            let (dot, dot_color, name, status, status_style, details) = match row {
                Row::Daemon => {
                    let attention = self.daemon.expect("daemon row");
                    let color = if attention.is_problem() {
                        theme.error
                    } else {
                        theme.dimmed
                    };
                    let mut details = super::info::wrap_to_width(attention.detail(), detail_width);
                    if let Some(reason) = self.reason.as_deref() {
                        details.extend(super::info::wrap_to_width(reason, detail_width));
                    }
                    (
                        "\u{25CF}",
                        color,
                        "Daemon",
                        attention.status().to_string(),
                        Style::default().fg(theme.text),
                        details,
                    )
                }
                Row::Web => {
                    let name = if cfg!(feature = "web") {
                        "Web dashboard"
                    } else {
                        "API server"
                    };
                    let (dot, color, status, style) = match &self.web {
                        WebState::On(via) => (
                            "\u{25CF}",
                            theme.running,
                            via.map_or("On".to_string(), |via| format!("On \u{00B7} {via}")),
                            Style::default().fg(theme.text),
                        ),
                        WebState::Off => (
                            "\u{25CB}",
                            theme.dimmed,
                            if cfg!(feature = "web") {
                                "Off \u{00B7} use AoE in a browser or on your phone".to_string()
                            } else {
                                "Off".to_string()
                            },
                            Style::default().fg(theme.text),
                        ),
                        WebState::Disabled => (
                            "\u{25CB}",
                            theme.dimmed,
                            "Off \u{00B7} the aoe.web plugin is disabled".to_string(),
                            dim,
                        ),
                        WebState::NeedsDaemon => (
                            "\u{25CB}",
                            theme.dimmed,
                            "Needs the Daemon".to_string(),
                            dim,
                        ),
                    };
                    (dot, color, name, status, style, Vec::new())
                }
            };
            row_lines.push((row, lines.len()));
            let marker = if selected { ">" } else { " " };
            let name_style = if selected {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.text)
            };
            let mut line = Line::from(vec![
                Span::styled(marker, key),
                Span::styled(dot, Style::default().fg(dot_color)),
                Span::raw(" "),
                Span::styled(format!("{name:<NAME$}"), name_style),
                Span::styled(status, status_style),
            ]);
            if selected {
                line = line.style(Style::default().bg(theme.selection));
            }
            lines.push(line);
            let indent = " ".repeat(NAME + 3);
            for detail in details {
                lines.push(Line::from(Span::styled(format!("{indent}{detail}"), dim)));
            }
            if row == Row::Daemon && self.offline_takeover() {
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled("[ ", key),
                    Span::styled("r", key),
                    Span::styled(" Reconnect ]", key),
                ]));
                lines.push(Line::raw(""));
            }
        }
        lines.push(Line::raw(""));
        let hints: Vec<(&str, &str)> = if self.offline_takeover() {
            vec![("r", " reconnect  "), ("q", " quit")]
        } else if self.rows().len() > 1 {
            vec![
                ("\u{2191}\u{2193}", " move  "),
                ("Enter", " open  "),
                ("Esc", " close"),
            ]
        } else {
            vec![("Enter", " open  "), ("Esc", " close")]
        };
        lines.push(Line::from(
            hints
                .into_iter()
                .flat_map(|(k, d)| [Span::styled(k, key), Span::styled(d, dim)])
                .collect::<Vec<_>>(),
        ));

        let height = lines.len() as u16 + 4;
        let block = super::dialog_block(" Connections ", theme);
        let (_, inner) = super::render_dialog_frame(frame, area, width, height, block);
        let body = inner.inner(Margin::new(1, 1));
        self.row_rects = row_lines
            .into_iter()
            .filter(|(_, line)| (*line as u16) < body.height)
            .map(|(row, line)| (row, Rect::new(body.x, body.y + line as u16, body.width, 1)))
            .collect();
        frame.render_widget(Paragraph::new(lines), body);
    }
}

/// This computer's name, without its domain ("mac-studio", not "mac-studio.local").
fn host_name() -> Option<String> {
    let name = nix::unistd::gethostname().ok()?.into_string().ok()?;
    let short = name.split('.').next().unwrap_or_default();
    (!short.is_empty()).then(|| short.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn screen(view: &mut ConnectionsView) -> String {
        screen_at(view, 100)
    }

    fn screen_at(view: &mut ConnectionsView, width: u16) -> String {
        let buffer = crate::tui::dialogs::test_render::draw(width, 30, |frame, theme| {
            view.render(frame, frame.area(), theme)
        });
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn rows_say_what_the_daemon_and_web_dashboard_are_doing() {
        let web = if cfg!(feature = "web") {
            "Web dashboard"
        } else {
            "API server"
        };
        let offline = || ConnectionsView::offline(Some("connection refused"));
        // (view, text shown, text absent)
        let cases: [(ConnectionsView, &[&str], &[&str]); 6] = [
            // Healthy: no Daemon row and one row, so no move hint.
            (
                ConnectionsView::new(None, WebState::Off),
                &[web, "Off", "Enter open", "Esc close"],
                &["Daemon", "move"],
            ),
            (
                ConnectionsView::new(None, WebState::On(Some("Tailscale"))),
                &["On \u{00B7} Tailscale"],
                &["Daemon"],
            ),
            (
                ConnectionsView::new(Some(DaemonAttention::OutcomeUnknown), WebState::On(None)),
                &["Daemon", "Outcome unknown", "Check the list", "move"],
                &["Reconnect"],
            ),
            (
                ConnectionsView::new(Some(DaemonAttention::ReadOnly), WebState::Off),
                &["Read-only", "refuses changes"],
                &[],
            ),
            (
                ConnectionsView::new(None, WebState::Disabled),
                &["aoe.web plugin is disabled"],
                &[],
            ),
            // The offline takeover: what still works, the reason, and today's r and q.
            (
                offline(),
                &[
                    "Not responding",
                    "keep running in tmux",
                    "connection refused",
                    "Needs the Daemon",
                    "[ r Reconnect ]",
                    "r reconnect",
                    "q quit",
                ],
                &["Esc close", "Enter open"],
            ),
        ];
        for (mut view, shown, absent) in cases {
            let text = screen(&mut view);
            for needle in shown {
                assert!(text.contains(needle), "{needle:?} missing\n{text}");
            }
            for needle in absent {
                assert!(!text.contains(needle), "{needle:?} present\n{text}");
            }
        }

        // A narrow terminal shrinks the dialog and wraps its details rather than cutting them.
        let text = screen_at(&mut offline(), 50);
        for needle in ["back.", "connection refused", "[ r Reconnect ]"] {
            assert!(
                text.contains(needle),
                "{needle:?} missing at 50 columns\n{text}"
            );
        }
    }

    #[test]
    fn enter_opens_only_the_web_dashboard_row() {
        let press = |view: &mut ConnectionsView, code| {
            view.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        };
        let mut view = ConnectionsView::new(None, WebState::Off);
        assert!(matches!(
            press(&mut view, KeyCode::Enter),
            DialogResult::Submit(())
        ));
        assert!(matches!(
            press(&mut view, KeyCode::Esc),
            DialogResult::Cancel
        ));

        // With the Daemon row first, Enter on it does nothing; Down reaches the dashboard.
        let mut view = ConnectionsView::new(Some(DaemonAttention::ReadOnly), WebState::Off);
        assert!(matches!(
            press(&mut view, KeyCode::Enter),
            DialogResult::Continue
        ));
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Down);
        assert!(matches!(
            press(&mut view, KeyCode::Enter),
            DialogResult::Submit(())
        ));
        press(&mut view, KeyCode::Up);
        assert!(matches!(
            press(&mut view, KeyCode::Enter),
            DialogResult::Continue
        ));

        // A refresh follows the Daemon and keeps the selected row when the rows change.
        press(&mut view, KeyCode::Down);
        view.refresh(None, WebState::On(None));
        assert!(matches!(
            press(&mut view, KeyCode::Enter),
            DialogResult::Submit(())
        ));
        view.refresh(Some(DaemonAttention::OutcomeUnknown), WebState::On(None));
        assert!(matches!(
            press(&mut view, KeyCode::Enter),
            DialogResult::Submit(())
        ));
        assert!(screen(&mut view).contains("Outcome unknown"));
    }
}
