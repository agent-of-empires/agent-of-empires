//! Suggested sessions browser. Items come from the daemon's suggestions API;
//! starting one opens the regular new-session dialog prefilled from `prepare`.
//! Provider text is untrusted: it is shown as plain text, control characters
//! stripped, and the prompt is visible before Enter.

use std::sync::mpsc::TryRecvError;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
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
    source_url: String,
    source_ref: String,
    preview: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    dismissed: bool,
    draft: Draft,
}

#[derive(Debug, Clone, Deserialize)]
struct Draft {
    prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PreparedProject {
    path: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Prepared {
    title: String,
    initial_prompt: String,
    tool: String,
    project: Option<PreparedProject>,
}

/// A suggestion resolved to a registered project, ready for the dialog.
pub(super) struct Start {
    pub path: String,
    pub title: String,
    pub initial_prompt: String,
    pub tool: String,
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
    Refreshed(Result<(), String>),
    Prepared(Result<Prepared, String>),
}

impl Res {
    fn error(&self) -> Option<&str> {
        match self {
            Res::Snapshot(Err(e)) | Res::Refreshed(Err(e)) | Res::Prepared(Err(e)) => Some(e),
            _ => None,
        }
    }
}

fn failed(req: &Req, message: String) -> Res {
    match req {
        Req::Prepare { .. } => Res::Prepared(Err(message)),
        Req::Refresh => Res::Refreshed(Err(message)),
        Req::Load | Req::Dismiss { .. } => Res::Snapshot(Err(message)),
    }
}

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

async fn load(client: &HttpClient) -> Result<Snapshot, String> {
    let value = client.suggestions().await.map_err(text)?;
    serde_json::from_value(value).map_err(text)
}

async fn run(req: &Req) -> Res {
    let client = match locate_daemon()
        .await
        .map_err(text)
        .and_then(|endpoint| HttpClient::new(endpoint).map_err(text))
    {
        Ok(client) => client,
        Err(message) => return failed(req, message),
    };
    match req {
        Req::Load => Res::Snapshot(load(&client).await),
        Req::Refresh => Res::Refreshed(client.refresh_suggestions().await.map_err(text)),
        Req::Dismiss {
            provider_id,
            id,
            dismissed,
        } => Res::Snapshot(
            async {
                client
                    .set_suggestion_dismissed(provider_id, id, *dismissed)
                    .await
                    .map_err(text)?;
                load(&client).await
            }
            .await,
        ),
        Req::Prepare { provider_id, id } => Res::Prepared(
            async {
                let value = client
                    .prepare_suggestion(provider_id, id)
                    .await
                    .map_err(text)?;
                serde_json::from_value(value).map_err(text)
            }
            .await,
        ),
    }
}

fn spawn_worker() -> Worker<Req, Res> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    Worker::spawn("aoe-suggestions", move |req| {
        let res = match &runtime {
            Ok(runtime) => runtime.block_on(run(&req)),
            Err(e) => failed(&req, e.to_string()),
        };
        if let Some(error) = res.error() {
            tracing::warn!(target: "tui.suggestions", error = %error, "suggestions request failed");
        }
        res
    })
}

/// Provider text is untrusted: drop control characters (escape sequences) and
/// bidi overrides, keeping newlines.
fn plain(s: &str) -> String {
    s.chars()
        .map(|c| {
            let hostile = (c.is_control() && c != '\n')
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
            if hostile {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

pub(super) enum Poll {
    Idle,
    Changed,
    Start(Start),
}

pub(super) enum SuggestionsAction {
    Continue,
    Close,
}

pub(super) struct SuggestionsView {
    snapshot: Option<Snapshot>,
    /// Indices into `snapshot.items` in display order, rebuilt when the
    /// snapshot or the dismissed filter changes.
    order: Vec<usize>,
    pub(super) error: Option<String>,
    notice: Option<String>,
    selected: usize,
    detail_scroll: u16,
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
            order: Vec::new(),
            error: None,
            notice: None,
            selected: 0,
            detail_scroll: 0,
            show_dismissed: false,
            busy: false,
            worker,
        }
    }

    pub(super) fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    fn send(&mut self, req: Req) {
        self.busy = true;
        self.error = None;
        self.notice = None;
        self.worker.request(req);
    }

    fn item_at(&self, index: usize) -> Option<&Item> {
        let snapshot = self.snapshot.as_ref()?;
        snapshot.items.get(*self.order.get(index)?)
    }

    fn selected_key(&self) -> Option<(String, String)> {
        self.item_at(self.selected)
            .map(|i| (i.provider_id.clone(), i.id.clone()))
    }

    /// Rebuilds `order`, keeping the selection on `key` when it is still listed.
    fn reorder(&mut self, key: Option<(String, String)>) {
        let items = self.snapshot.iter().flat_map(|s| &s.items);
        let mut order: Vec<usize> = items
            .clone()
            .enumerate()
            .filter(|(_, i)| self.show_dismissed || !i.dismissed)
            .map(|(index, _)| index)
            .collect();
        let items: Vec<&Item> = items.collect();
        order.sort_by(|&a, &b| {
            items[a]
                .project
                .cmp(&items[b].project)
                .then_with(|| items[a].source_ref.cmp(&items[b].source_ref))
        });
        self.order = order;
        let kept = key.and_then(|(provider_id, id)| {
            (0..self.order.len()).find(|&index| {
                self.item_at(index)
                    .is_some_and(|i| i.provider_id == provider_id && i.id == id)
            })
        });
        self.selected =
            kept.unwrap_or_else(|| self.selected.min(self.order.len().saturating_sub(1)));
    }

    fn replace(&mut self, snapshot: Snapshot) {
        let key = self.selected_key();
        self.snapshot = Some(snapshot);
        self.reorder(key);
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.order.len().saturating_sub(1));
        self.detail_scroll = 0;
    }

    /// `mutations_allowed` is false for a read-only or disconnected daemon.
    pub(super) fn handle_key(
        &mut self,
        key: KeyEvent,
        mutations_allowed: bool,
    ) -> SuggestionsAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return SuggestionsAction::Close,
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected + 1),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(8),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(8),
            KeyCode::Char('d') => {
                let key = self.selected_key();
                self.show_dismissed = !self.show_dismissed;
                self.reorder(key);
            }
            _ if self.busy => {}
            KeyCode::Char('l') => self.send(Req::Load),
            KeyCode::Char('r') if mutations_allowed => self.send(Req::Refresh),
            KeyCode::Char('x') if mutations_allowed => {
                if let Some(item) = self.item_at(self.selected) {
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
                if let Some(item) = self.item_at(self.selected) {
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
        let res = match self.worker.try_recv() {
            Ok(res) => res,
            Err(TryRecvError::Disconnected) if self.busy => {
                self.busy = false;
                self.error = Some("The suggestions worker stopped".into());
                return Poll::Changed;
            }
            Err(_) => return Poll::Idle,
        };
        self.busy = false;
        match res {
            Res::Snapshot(Ok(snapshot)) => self.replace(snapshot),
            Res::Refreshed(Ok(())) => {
                self.notice = Some("Refresh requested. Press l to reload the list.".into());
            }
            Res::Snapshot(Err(e)) | Res::Refreshed(Err(e)) | Res::Prepared(Err(e)) => {
                self.error = Some(e);
            }
            Res::Prepared(Ok(prepared)) => match prepared.project {
                None => {
                    self.error = Some("No registered project matches this suggestion".into());
                }
                Some(project) => {
                    return Poll::Start(Start {
                        path: project.path,
                        title: prepared.title,
                        initial_prompt: prepared.initial_prompt,
                        tool: prepared.tool,
                    });
                }
            },
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

        let mut footer: Vec<Line> = Vec::new();
        if let Some(error) = &self.error {
            footer.push(Line::from(Span::styled(
                plain(error),
                Style::default().fg(theme.error),
            )));
        }
        if let Some(notice) = &self.notice {
            footer.push(Line::from(Span::styled(
                notice.as_str(),
                Style::default().fg(theme.hint),
            )));
        }
        for provider in self.snapshot.iter().flat_map(|s| &s.providers) {
            if let Some(error) = &provider.error {
                footer.push(Line::from(Span::styled(
                    plain(&format!("{}: {error}", provider.name)),
                    Style::default().fg(theme.waiting),
                )));
            }
        }
        footer.push(Line::from(Span::styled(
            "j/k select · Enter start · x dismiss/restore · d dismissed · r refresh · l reload · PgUp/PgDn prompt · Esc back",
            Style::default().fg(theme.dimmed),
        )));
        let footer_height = (footer.len() as u16).min(inner.height.saturating_sub(1));
        let [body, footer_area] = *Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(footer_height)])
            .split(inner)
        else {
            return;
        };
        let [list_area, detail_area] = *Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(body)
        else {
            return;
        };

        let per_page = (list_area.height as usize / 2).max(1);
        let start = (self.selected + 1).saturating_sub(per_page);
        let mut lines: Vec<Line> = Vec::new();
        for index in start..self.order.len().min(start + per_page) {
            let Some(item) = self.item_at(index) else {
                continue;
            };
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
                plain(&format!("{marker}{}{state}", item.title)),
                title_style,
            )));
            lines.push(Line::from(Span::styled(
                plain(&format!(
                    "    {} · {} · {}",
                    item.reason,
                    item.source_ref,
                    item.project.as_deref().unwrap_or("no project")
                )),
                Style::default().fg(theme.dimmed),
            )));
        }
        if lines.is_empty() {
            let message = match (&self.snapshot, self.busy) {
                (None, true) => "Loading suggestions...",
                (None, false) => "No suggestions loaded. Press l to reload.",
                (Some(_), _) => "No suggestions. Press r to ask providers to refresh.",
            };
            lines.push(Line::from(Span::styled(
                message,
                Style::default().fg(theme.dimmed),
            )));
        }
        frame.render_widget(Paragraph::new(lines), list_area);

        let mut detail: Vec<Line> = Vec::new();
        if let Some(item) = self.item_at(self.selected) {
            detail.push(Line::from(Span::styled(
                plain(&item.title),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            detail.push(Line::from(plain(&format!(
                "Provider: {} · Source: {}",
                item.provider_id, item.source_url
            ))));
            detail.extend(
                plain(&item.preview)
                    .lines()
                    .map(|l| Line::from(l.to_owned())),
            );
            detail.push(Line::from(""));
            detail.push(Line::from(Span::styled(
                "Prompt sent to the agent (untrusted source text):",
                Style::default().fg(theme.accent),
            )));
            detail.extend(
                plain(&item.draft.prompt)
                    .lines()
                    .map(|l| Line::from(l.to_owned())),
            );
        }
        frame.render_widget(
            Paragraph::new(detail)
                .block(Block::default().borders(Borders::TOP))
                .wrap(Wrap { trim: false })
                .scroll((self.detail_scroll, 0)),
            detail_area,
        );
        frame.render_widget(
            Paragraph::new(footer).wrap(Wrap { trim: true }),
            footer_area,
        );
    }

    #[cfg(test)]
    pub(super) fn with_start_for_test(start: Start) -> Self {
        let prepared = Prepared {
            title: start.title,
            initial_prompt: start.initial_prompt,
            tool: start.tool,
            project: Some(PreparedProject { path: start.path }),
        };
        Self::with_worker(Worker::seeded_for_test(
            "test-suggestions",
            Res::Prepared(Ok(prepared)),
        ))
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
            source_url: "https://example.test/x".into(),
            source_ref: format!("ref-{id}"),
            preview: "preview text".into(),
            project: project.map(str::to_owned),
            dismissed,
            draft: Draft {
                prompt: "do the thing".into(),
            },
        }
    }

    fn view_with(items: Vec<Item>, worker: Worker<Req, Res>) -> SuggestionsView {
        let mut view = SuggestionsView::with_worker(worker);
        view.replace(Snapshot {
            providers: vec![],
            items,
        });
        view
    }

    fn idle_worker() -> Worker<Req, Res> {
        Worker::spawn("test-suggestions", |_| unreachable!("no request expected"))
    }

    fn press(view: &mut SuggestionsView, code: KeyCode, mutations: bool) {
        view.handle_key(KeyEvent::new(code, KeyModifiers::NONE), mutations);
    }

    fn ids(view: &SuggestionsView) -> Vec<String> {
        (0..view.order.len())
            .filter_map(|i| view.item_at(i))
            .map(|i| i.id.clone())
            .collect()
    }

    /// Seeded results are queued before the worker thread starts and a dead
    /// worker disconnects shortly after a request, so a bounded retry loop
    /// only covers scheduling.
    fn poll_until_settled(view: &mut SuggestionsView) -> Poll {
        for _ in 0..200 {
            match view.poll() {
                Poll::Idle => std::thread::sleep(std::time::Duration::from_millis(10)),
                other => return other,
            }
        }
        Poll::Idle
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
            idle_worker(),
        );
        assert_eq!(ids(&view), ["a", "b"]);
        press(&mut view, KeyCode::Char('d'), true);
        assert_eq!(ids(&view), ["a", "gone", "b"]);
    }

    #[test]
    fn selection_follows_the_item_across_a_snapshot_replace() {
        let mut view = view_with(
            vec![item("a", Some("p1"), false), item("b", Some("p2"), false)],
            idle_worker(),
        );
        press(&mut view, KeyCode::Char('j'), true);
        assert_eq!(view.item_at(view.selected).unwrap().id, "b");
        view.replace(Snapshot {
            providers: vec![],
            items: vec![
                item("new", Some("p0"), false),
                item("a", Some("p1"), false),
                item("b", Some("p2"), false),
            ],
        });
        assert_eq!(view.item_at(view.selected).unwrap().id, "b");
    }

    #[test]
    fn mutating_keys_are_refused_when_read_only() {
        let mut view = view_with(vec![item("a", None, false)], idle_worker());
        press(&mut view, KeyCode::Char('x'), false);
        press(&mut view, KeyCode::Char('r'), false);
        assert!(!view.busy);
        assert!(view.error.is_some());
    }

    #[test]
    fn prepare_result_starts_only_with_a_registered_project() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut view = view_with(vec![], worker);
        let Poll::Start(start) = poll_until_settled(&mut view) else {
            panic!("expected a start");
        };
        assert_eq!(start.path, "/repo");

        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(None))));
        let mut view = view_with(vec![], worker);
        assert!(matches!(poll_until_settled(&mut view), Poll::Changed));
        assert!(view
            .error
            .as_deref()
            .unwrap()
            .contains("No registered project"));
    }

    #[test]
    fn failed_request_keeps_the_last_snapshot() {
        let worker = Worker::seeded_for_test("t", Res::Snapshot(Err("daemon down".into())));
        let mut view = view_with(vec![item("a", None, false)], worker);
        assert!(matches!(poll_until_settled(&mut view), Poll::Changed));
        assert_eq!(view.error.as_deref(), Some("daemon down"));
        assert_eq!(ids(&view), ["a"]);
    }

    #[test]
    fn refresh_notifies_and_asks_for_a_reload() {
        let worker = Worker::seeded_for_test("t", Res::Refreshed(Ok(())));
        let mut view = view_with(vec![], worker);
        assert!(matches!(poll_until_settled(&mut view), Poll::Changed));
        assert!(view.notice.as_deref().unwrap().contains("Press l"));
    }

    #[test]
    fn a_dead_worker_clears_busy_and_reports_an_error() {
        let worker = Worker::spawn("test-suggestions", |_: Req| -> Res {
            panic!("worker died")
        });
        let mut view = view_with(vec![item("a", None, false)], worker);
        press(&mut view, KeyCode::Char('l'), true);
        assert!(view.busy);
        assert!(matches!(poll_until_settled(&mut view), Poll::Changed));
        assert!(!view.busy);
        assert!(view.error.is_some());
        assert!(matches!(view.poll(), Poll::Idle));
    }

    #[test]
    fn view_shows_prompt_and_preview_as_plain_text() {
        let mut hostile = item("a", Some("repo"), false);
        hostile.draft.prompt = "run \u{1b}[31mrm -rf\u{1b}[0m now".into();
        let view = view_with(vec![hostile], idle_worker());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(100)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        let text = rows.join("\n");
        println!("{text}");
        for expected in [
            "reason · ref-a · repo",
            "preview text",
            "Prompt sent to the agent",
            "run \u{fffd}[31mrm -rf\u{fffd}[0m now",
        ] {
            assert!(text.contains(expected), "missing {expected:?}");
        }
        assert!(!text.contains('\u{1b}'));
    }
}
