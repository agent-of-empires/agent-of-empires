//! Suggested sessions browser. Items come from the daemon's suggestions API;
//! starting one opens the regular new-session dialog prefilled from `prepare`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use serde::Deserialize;

use crate::acp::client::{locate_daemon, HttpClient};
use crate::tui::styles::Theme;
use crate::tui::worker::Worker;

#[derive(Debug, Clone, Deserialize)]
struct Snapshot {
    providers: Vec<Provider>,
    items: Vec<Item>,
}

#[derive(Debug, Clone, Deserialize)]
struct Provider {
    name: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Item {
    provider_id: String,
    id: String,
    title: String,
    reason: String,
    source_ref: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    dismissed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct PreparedProject {
    pub path: String,
}

/// The wizard prefill the daemon resolves for one suggestion.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct Prepared {
    pub title: String,
    pub initial_prompt: String,
    pub tool: String,
    pub project: Option<PreparedProject>,
}

enum Req {
    Load,
    Refresh,
    Dismiss {
        provider_id: String,
        id: String,
        dismissed: bool,
    },
    Prepare {
        provider_id: String,
        id: String,
    },
}

enum Res {
    Snapshot(Result<Snapshot, String>),
    Prepared(Result<Prepared, String>),
}

fn failed(req: &Req, message: String) -> Res {
    match req {
        Req::Prepare { .. } => Res::Prepared(Err(message)),
        _ => Res::Snapshot(Err(message)),
    }
}

async fn run(req: &Req) -> Res {
    let client = match locate_daemon()
        .await
        .map_err(|e| e.to_string())
        .and_then(|endpoint| HttpClient::new(endpoint).map_err(|e| e.to_string()))
    {
        Ok(client) => client,
        Err(message) => return failed(req, message),
    };
    let to_string = |e: crate::acp::client::HttpError| e.to_string();
    match req {
        Req::Prepare { provider_id, id } => Res::Prepared(
            async {
                let value = client
                    .prepare_suggestion(provider_id, id)
                    .await
                    .map_err(to_string)?;
                serde_json::from_value(value).map_err(|e| e.to_string())
            }
            .await,
        ),
        _ => Res::Snapshot(
            async {
                match req {
                    Req::Refresh => client.refresh_suggestions().await.map_err(to_string)?,
                    Req::Dismiss {
                        provider_id,
                        id,
                        dismissed,
                    } => client
                        .set_suggestion_dismissed(provider_id, id, *dismissed)
                        .await
                        .map_err(to_string)?,
                    Req::Load | Req::Prepare { .. } => {}
                }
                let value = client.suggestions().await.map_err(to_string)?;
                serde_json::from_value(value).map_err(|e| e.to_string())
            }
            .await,
        ),
    }
}

fn spawn_worker() -> Worker<Req, Res> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    Worker::spawn("aoe-suggestions", move |req| match &runtime {
        Ok(runtime) => runtime.block_on(run(&req)),
        Err(e) => failed(&req, e.to_string()),
    })
}

pub(super) enum Poll {
    Idle,
    Changed,
    /// A start request resolved to a registered project.
    Prepared(Prepared),
}

pub(super) enum SuggestionsAction {
    Continue,
    Close,
}

pub(super) struct SuggestionsView {
    snapshot: Option<Snapshot>,
    error: Option<String>,
    selected: usize,
    show_dismissed: bool,
    busy: bool,
    worker: Worker<Req, Res>,
}

impl SuggestionsView {
    pub(super) fn new() -> Self {
        let mut view = Self::with_worker(spawn_worker());
        view.send(Req::Load);
        view
    }

    fn with_worker(worker: Worker<Req, Res>) -> Self {
        Self {
            snapshot: None,
            error: None,
            selected: 0,
            show_dismissed: false,
            busy: false,
            worker,
        }
    }

    fn send(&mut self, req: Req) {
        self.busy = true;
        self.error = None;
        self.worker.request(req);
    }

    fn visible(&self) -> Vec<&Item> {
        let mut items: Vec<&Item> = self
            .snapshot
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| self.show_dismissed || !i.dismissed)
            .collect();
        items.sort_by(|a, b| {
            a.project
                .cmp(&b.project)
                .then_with(|| a.source_ref.cmp(&b.source_ref))
        });
        items
    }

    /// `mutations_allowed` is false for a read-only or disconnected daemon.
    pub(super) fn handle_key(
        &mut self,
        key: KeyEvent,
        mutations_allowed: bool,
    ) -> SuggestionsAction {
        let len = self.visible().len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return SuggestionsAction::Close,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(len.saturating_sub(1))
            }
            KeyCode::Char('d') => {
                self.show_dismissed = !self.show_dismissed;
                self.selected = 0;
            }
            _ if self.busy => {}
            KeyCode::Char('r') if mutations_allowed => self.send(Req::Refresh),
            KeyCode::Char('x') if mutations_allowed => {
                if let Some(item) = self.visible().get(self.selected) {
                    let req = Req::Dismiss {
                        provider_id: item.provider_id.clone(),
                        id: item.id.clone(),
                        dismissed: !item.dismissed,
                    };
                    self.send(req);
                }
            }
            KeyCode::Char('r' | 'x') => {
                self.error = Some("The daemon is read-only or unavailable".into());
            }
            KeyCode::Enter => {
                if let Some(item) = self.visible().get(self.selected) {
                    let req = Req::Prepare {
                        provider_id: item.provider_id.clone(),
                        id: item.id.clone(),
                    };
                    self.send(req);
                }
            }
            _ => {}
        }
        SuggestionsAction::Continue
    }

    /// Applies a finished request, if any.
    pub(super) fn poll(&mut self) -> Poll {
        let Ok(res) = self.worker.try_recv() else {
            return Poll::Idle;
        };
        self.busy = false;
        match res {
            Res::Snapshot(Ok(snapshot)) => {
                self.snapshot = Some(snapshot);
                self.selected = self.selected.min(self.visible().len().saturating_sub(1));
            }
            Res::Snapshot(Err(e)) | Res::Prepared(Err(e)) => self.error = Some(e),
            Res::Prepared(Ok(prepared)) if prepared.project.is_none() => {
                self.error = Some("No registered project matches this suggestion".into());
            }
            Res::Prepared(Ok(prepared)) => return Poll::Prepared(prepared),
        }
        Poll::Changed
    }

    pub(super) fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .title(" Suggested sessions ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.background).fg(theme.text))
            .border_style(Style::default().fg(theme.border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let items = self.visible();
        let mut lines: Vec<Line> = Vec::new();
        let mut selected_line = 0;
        for (index, item) in items.iter().enumerate() {
            if index == self.selected {
                selected_line = lines.len();
            }
            let current = index == self.selected;
            let marker = if current { "› " } else { "  " };
            let state = if item.dismissed { " [dismissed]" } else { "" };
            let title_style = if current {
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            lines.push(Line::from(Span::styled(
                format!("{marker}{}{state}", item.title),
                title_style,
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "    {} · {} · {}",
                    item.reason,
                    item.source_ref,
                    item.project.as_deref().unwrap_or("no project")
                ),
                Style::default().fg(theme.dimmed),
            )));
        }
        if lines.is_empty() {
            let message = match (&self.snapshot, self.busy) {
                (None, true) => "Loading suggestions...",
                (None, false) => "No suggestions loaded. Press r to refresh.",
                (Some(_), _) => "No suggestions. Press r to ask providers to refresh.",
            };
            lines.push(Line::from(Span::styled(
                message,
                Style::default().fg(theme.dimmed),
            )));
        }

        let mut footer: Vec<Line> = Vec::new();
        if let Some(error) = &self.error {
            footer.push(Line::from(Span::styled(
                error.as_str(),
                Style::default().fg(theme.error),
            )));
        }
        for provider in self.snapshot.iter().flat_map(|s| &s.providers) {
            if let Some(error) = &provider.error {
                footer.push(Line::from(Span::styled(
                    format!("{}: {error}", provider.name),
                    Style::default().fg(theme.waiting),
                )));
            }
        }
        footer.push(Line::from(Span::styled(
            "j/k select · Enter start · x dismiss/restore · d show dismissed · r refresh · Esc back",
            Style::default().fg(theme.dimmed),
        )));
        let footer_height = (footer.len() as u16).min(inner.height.saturating_sub(1));
        let list_area = Rect {
            height: inner.height - footer_height,
            ..inner
        };
        let footer_area = Rect {
            y: inner.y + list_area.height,
            height: footer_height,
            ..inner
        };
        let scroll = (selected_line + 2).saturating_sub(list_area.height as usize);
        frame.render_widget(
            Paragraph::new(lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            list_area,
        );
        frame.render_widget(
            Paragraph::new(footer).wrap(Wrap { trim: true }),
            footer_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn item(id: &str, project: Option<&str>, dismissed: bool) -> Item {
        Item {
            provider_id: "p".into(),
            id: id.into(),
            title: id.into(),
            reason: "reason".into(),
            source_ref: format!("ref-{id}"),
            project: project.map(str::to_owned),
            dismissed,
        }
    }

    fn view_with(items: Vec<Item>, res: Option<Res>) -> SuggestionsView {
        let worker = match res {
            Some(res) => Worker::seeded_for_test("test-suggestions", res),
            None => Worker::spawn("test-suggestions", |_| unreachable!("no request expected")),
        };
        let mut view = SuggestionsView::with_worker(worker);
        view.snapshot = Some(Snapshot {
            providers: vec![],
            items,
        });
        view
    }

    fn press(view: &mut SuggestionsView, code: KeyCode, mutations: bool) {
        view.handle_key(KeyEvent::new(code, KeyModifiers::NONE), mutations);
    }

    fn prepared(project: Option<&str>) -> Prepared {
        Prepared {
            title: "t".into(),
            initial_prompt: "p".into(),
            tool: "claude".into(),
            project: project.map(|path| PreparedProject { path: path.into() }),
        }
    }

    #[test]
    fn dismissed_items_are_hidden_until_toggled_and_sorted_by_project() {
        let mut view = view_with(
            vec![
                item("b", Some("zeta"), false),
                item("a", Some("alpha"), false),
                item("gone", Some("alpha"), true),
            ],
            None,
        );
        let ids =
            |v: &SuggestionsView| v.visible().iter().map(|i| i.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&view), ["a", "b"]);
        press(&mut view, KeyCode::Char('d'), true);
        assert_eq!(ids(&view), ["a", "gone", "b"]);
    }

    #[test]
    fn mutating_keys_are_refused_when_read_only() {
        let mut view = view_with(vec![item("a", None, false)], None);
        press(&mut view, KeyCode::Char('x'), false);
        press(&mut view, KeyCode::Char('r'), false);
        assert!(!view.busy);
        assert!(view.error.is_some());
    }

    #[test]
    fn prepare_result_opens_only_with_a_registered_project() {
        let mut view = view_with(vec![], Some(Res::Prepared(Ok(prepared(Some("/repo"))))));
        view.busy = true;
        let Poll::Prepared(p) = poll_until_changed(&mut view) else {
            panic!("expected a prefill");
        };
        assert_eq!(p.project.unwrap().path, "/repo");
        assert!(!view.busy);

        let mut view = view_with(vec![], Some(Res::Prepared(Ok(prepared(None)))));
        assert!(matches!(poll_until_changed(&mut view), Poll::Changed));
        assert!(view
            .error
            .as_deref()
            .unwrap()
            .contains("No registered project"));
    }

    #[test]
    fn failed_request_keeps_the_last_snapshot() {
        let mut view = view_with(
            vec![item("a", None, false)],
            Some(Res::Snapshot(Err("daemon down".into()))),
        );
        assert!(matches!(poll_until_changed(&mut view), Poll::Changed));
        assert_eq!(view.error.as_deref(), Some("daemon down"));
        assert_eq!(view.visible().len(), 1);
    }

    /// The seeded worker's result is queued before the thread starts, so a
    /// bounded retry loop only covers scheduling.
    fn poll_until_changed(view: &mut SuggestionsView) -> Poll {
        for _ in 0..200 {
            match view.poll() {
                Poll::Idle => std::thread::sleep(std::time::Duration::from_millis(10)),
                other => return other,
            }
        }
        Poll::Idle
    }
}
