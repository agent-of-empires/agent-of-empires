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

#[cfg(not(test))]
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
    id: String,
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
    pub provider: String,
    pub source_ref: String,
    pub reason: String,
    /// The provider's `preview`, untrusted; `None` when it is blank.
    pub source_text: Option<String>,
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
        target: PrepareTarget,
    },
}

/// Which surface asked for a prepare, so its answer goes back to that surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrepareTarget {
    Home,
    View,
}

enum Res {
    Snapshot(Result<Snapshot, String>),
    Refreshed(Result<(), String>),
    Prepared(Result<Prepared, String>),
}

#[cfg(not(test))]
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

#[cfg(not(test))]
async fn load(client: &HttpClient) -> Result<Snapshot, String> {
    let value = client.suggestions().await.map_err(text)?;
    serde_json::from_value(value).map_err(text)
}

#[cfg(not(test))]
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
        Req::Prepare {
            provider_id, id, ..
        } => Res::Prepared(
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

#[cfg(not(test))]
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

const NO_PROJECT: &str = "No registered project matches this suggestion";

/// Resolves a prepared suggestion into a dialog `Start`; `None` when the
/// prepare response names no registered project.
fn start_for(snapshot: Option<&Snapshot>, key: &ItemKey, prepared: Prepared) -> Option<Start> {
    let project = prepared.project?;
    let item = snapshot
        .iter()
        .flat_map(|s| &s.items)
        .find(|i| i.provider_id == key.0 && i.id == key.1);
    let provider = snapshot
        .iter()
        .flat_map(|s| &s.providers)
        .find(|p| p.id == key.0)
        .map_or(key.0.as_str(), |p| p.name.as_str())
        .to_owned();
    Some(Start {
        provider,
        source_ref: item.map(|i| i.source_ref.clone()).unwrap_or_default(),
        reason: item.map(|i| i.reason.clone()).unwrap_or_default(),
        source_text: item
            .map(|i| i.preview.clone())
            .filter(|p| !p.trim().is_empty()),
        path: project.path,
        title: prepared.title,
        initial_prompt: prepared.initial_prompt,
        tool: prepared.tool,
    })
}

pub(super) enum SuggestionsAction {
    Continue,
    Close,
    /// The user confirmed a prepared suggestion.
    Start(Start),
}

type ItemKey = (String, String);

pub(super) struct SuggestionsView {
    /// Listed items in display order (core interleaves providers), rebuilt
    /// when the store's snapshot or the dismissed filter changes.
    order: Vec<ItemKey>,
    pub(super) error: Option<String>,
    notice: Option<String>,
    /// The resolved prefill, shown before the second Enter confirms it.
    prepared: Option<(ItemKey, Start)>,
    selected: usize,
    detail_scroll: u16,
    show_dismissed: bool,
}

impl SuggestionsView {
    pub(super) fn new(store: &SuggestionsFeed) -> Self {
        let mut view = Self::empty();
        view.reorder(store, None);
        view
    }

    fn empty() -> Self {
        Self {
            order: Vec::new(),
            error: None,
            notice: None,
            prepared: None,
            selected: 0,
            detail_scroll: 0,
            show_dismissed: false,
        }
    }

    pub(super) fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    /// Clears what the previous request left on screen.
    fn begin(&mut self) {
        self.error = None;
        self.notice = None;
        self.prepared = None;
    }

    fn item_at<'a>(&self, store: &'a SuggestionsFeed, index: usize) -> Option<&'a Item> {
        let key = self.order.get(index)?;
        store.item(key)
    }

    fn selected_key(&self) -> Option<ItemKey> {
        self.order.get(self.selected).cloned()
    }

    /// Rebuilds `order` in snapshot order, keeping the selection on `key`
    /// when it is still listed.
    fn reorder(&mut self, store: &SuggestionsFeed, key: Option<ItemKey>) {
        self.order = store
            .snapshot
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| self.show_dismissed || !i.dismissed)
            .map(|i| (i.provider_id.clone(), i.id.clone()))
            .collect();
        let kept = key.and_then(|key| self.order.iter().position(|k| *k == key));
        self.selected =
            kept.unwrap_or_else(|| self.selected.min(self.order.len().saturating_sub(1)));
    }

    fn sync(&mut self, store: &SuggestionsFeed) {
        self.reorder(store, self.selected_key());
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
        store: &mut SuggestionsFeed,
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
                self.reorder(store, key);
            }
            _ if store.busy => {}
            KeyCode::Char('l') => {
                self.begin();
                store.reload();
            }
            KeyCode::Char('r') if mutations_allowed => {
                self.begin();
                store.refresh();
            }
            KeyCode::Char('x') if mutations_allowed => {
                if let Some(item) = self.item_at(store, self.selected) {
                    let (key, dismissed) =
                        ((item.provider_id.clone(), item.id.clone()), !item.dismissed);
                    self.begin();
                    store.dismiss(&key, dismissed);
                }
            }
            KeyCode::Char('r' | 'x') => {
                self.error = Some("The daemon is read-only or unavailable".into());
            }
            KeyCode::Enter => {
                let Some(key) = self.selected_key() else {
                    return SuggestionsAction::Continue;
                };
                if self.prepared.as_ref().is_some_and(|(k, _)| *k == key) {
                    if let Some((_, start)) = self.prepared.take() {
                        return SuggestionsAction::Start(start);
                    }
                }
                self.begin();
                store.prepare_in_view(key);
            }
            _ => {}
        }
        SuggestionsAction::Continue
    }

    /// Takes what a store poll holds for this view. True when state changed.
    pub(super) fn apply(&mut self, store: &SuggestionsFeed, polled: &mut FeedPoll) -> bool {
        let mut changed = false;
        if polled.changed {
            self.sync(store);
            changed = true;
        }
        if polled.refreshed {
            self.notice = Some("Refresh requested. Press l to reload the list.".into());
            changed = true;
        }
        if let Some(error) = polled.error.take() {
            self.error = Some(error);
            changed = true;
        }
        if matches!(polled.start, Some((_, _, PrepareTarget::View))) {
            if let Some((key, start, _)) = polled.start.take() {
                self.notice = Some("Review the prompt below, then press Enter to start.".into());
                self.prepared = Some((key, start));
                changed = true;
            }
        }
        changed
    }

    pub(super) fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        store: &SuggestionsFeed,
    ) {
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
        for provider in store.snapshot.iter().flat_map(|s| &s.providers) {
            if let Some(error) = &provider.error {
                footer.push(Line::from(Span::styled(
                    plain(&format!("{}: {error}", provider.name)),
                    Style::default().fg(theme.waiting),
                )));
            }
        }
        footer.push(Line::from(Span::styled(
            "j/k select · Enter review/start · x dismiss/restore · d dismissed · r refresh · l reload · PgUp/PgDn prompt · Esc back",
            Style::default().fg(theme.dimmed),
        )));
        let width = (inner.width as usize).max(1);
        let wrapped_rows: usize = footer
            .iter()
            .map(|line| line.width().div_ceil(width).max(1))
            .sum();
        let footer_height = (wrapped_rows as u16).min(inner.height.saturating_sub(1));
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
            let Some(item) = self.item_at(store, index) else {
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
            lines.push(Line::from(vec![
                Span::styled(
                    plain(&format!(
                        "{marker}{} ",
                        store.provider_name(&item.provider_id)
                    )),
                    Style::default().fg(theme.dimmed),
                ),
                Span::styled(plain(&format!("{}{state}", item.title)), title_style),
            ]));
            lines.push(Line::from(Span::styled(
                plain(&format!(
                    "    {} · {}",
                    item.reason,
                    item.project.as_deref().unwrap_or("no project")
                )),
                Style::default().fg(theme.dimmed),
            )));
        }
        if lines.is_empty() {
            let message = match (&store.snapshot, store.busy) {
                (None, true) => "Loading suggestions…",
                (None, false) => "No suggestions loaded. Press l to reload.",
                (Some(_), _) => "Nothing to suggest right now.",
            };
            lines.push(Line::from(Span::styled(
                message,
                Style::default().fg(theme.dimmed),
            )));
        }
        frame.render_widget(Paragraph::new(lines), list_area);

        let mut detail: Vec<Line> = Vec::new();
        if let Some(item) = self.item_at(store, self.selected) {
            detail.push(Line::from(Span::styled(
                plain(&item.title),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            detail.push(Line::from(plain(&format!(
                "{} · {} · {}",
                store.provider_name(&item.provider_id),
                item.source_ref,
                item.source_url
            ))));
            detail.extend(
                plain(&item.preview)
                    .lines()
                    .map(|l| Line::from(l.to_owned())),
            );
            detail.push(Line::from(""));
            let final_prompt = self
                .prepared
                .as_ref()
                .filter(|(key, _)| *key == (item.provider_id.clone(), item.id.clone()))
                .map(|(_, start)| start.initial_prompt.as_str());
            let (header, prompt) = match final_prompt {
                Some(prompt) => ("Prompt sent to the agent (untrusted source text):", prompt),
                None => (
                    "Provider draft prompt (Enter resolves the final prompt first):",
                    item.draft.prompt.as_str(),
                ),
            };
            detail.push(Line::from(Span::styled(
                header,
                Style::default().fg(theme.accent),
            )));
            detail.extend(plain(prompt).lines().map(|l| Line::from(l.to_owned())));
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
    pub(super) fn idle_for_test() -> Self {
        Self::empty()
    }
}

/// How often the Home list reloads the suggestions snapshot.
const FEED_RELOAD: std::time::Duration = std::time::Duration::from_secs(60);
const SOURCE_LABEL_WIDTH: usize = 6;

/// What the Home list shows for one suggestion; provider text is untrusted and
/// already stripped of control characters.
pub(super) struct Entry {
    pub label: String,
    pub source_ref: String,
    pub title: String,
    pub reason: String,
    pub provider: String,
    pub project: Option<String>,
    pub prompt: String,
    pub provider_error: Option<String>,
}

pub(super) struct FeedPoll {
    pub changed: bool,
    /// The daemon accepted a refresh request.
    pub refreshed: bool,
    /// A prepared suggestion, with the key and the surface it was requested for.
    pub start: Option<(ItemKey, Start, PrepareTarget)>,
    /// Failure of a user-initiated request; background reloads only log.
    pub error: Option<String>,
}

/// The one suggestions store, owned by `HomeView`: snapshot, worker and
/// in-flight state behind both the Suggested group and the full view, which
/// keeps only its own UI state.
pub(super) struct SuggestionsFeed {
    snapshot: Option<Snapshot>,
    prepare_key: Option<ItemKey>,
    prepare_target: PrepareTarget,
    /// The in-flight request came from the user (prepare or dismiss), so its
    /// failure is worth showing.
    in_flight_user: bool,
    busy: bool,
    /// User requests that arrived while another request was in flight.
    queued: std::collections::VecDeque<Req>,
    loaded_at: Option<std::time::Instant>,
    load_attempt_at: Option<std::time::Instant>,
    worker: Worker<Req, Res>,
}

impl SuggestionsFeed {
    pub(super) fn new() -> Self {
        // Tests must not reach a developer's real daemon.
        #[cfg(test)]
        let worker = Worker::spawn("test-suggestions-feed", |req| {
            failed(&req, "no daemon in tests".into())
        });
        #[cfg(not(test))]
        let worker = spawn_worker();
        Self::with_worker(worker)
    }

    fn with_worker(worker: Worker<Req, Res>) -> Self {
        Self {
            snapshot: None,
            prepare_key: None,
            prepare_target: PrepareTarget::Home,
            in_flight_user: false,
            busy: false,
            queued: std::collections::VecDeque::new(),
            loaded_at: None,
            load_attempt_at: None,
            worker,
        }
    }

    fn send(&mut self, req: Req, user: bool) {
        self.busy = true;
        self.in_flight_user = user;
        match &req {
            Req::Load => self.load_attempt_at = Some(std::time::Instant::now()),
            Req::Prepare {
                provider_id,
                id,
                target,
            } => {
                self.prepare_key = Some((provider_id.clone(), id.clone()));
                self.prepare_target = *target;
            }
            _ => {}
        }
        self.worker.request(req);
    }

    /// Sends a user request now, or behind the request in flight.
    fn submit(&mut self, req: Req) {
        if self.busy {
            self.queued.push_back(req);
        } else {
            self.send(req, true);
        }
    }

    fn item(&self, key: &ItemKey) -> Option<&Item> {
        self.snapshot
            .iter()
            .flat_map(|s| &s.items)
            .find(|i| i.provider_id == key.0 && i.id == key.1)
    }

    fn provider_name<'a>(&'a self, id: &'a str) -> &'a str {
        self.snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .find(|p| p.id == id)
            .map_or(id, |p| p.name.as_str())
    }

    /// Whether the provider is listed: items of an unlisted provider are not
    /// shown anywhere.
    fn provider(&self, id: &str) -> Option<&Provider> {
        self.snapshot
            .as_ref()?
            .providers
            .iter()
            .find(|p| p.id == id)
    }

    /// `(provider_id, id)` of every undismissed suggestion of a listed
    /// provider, in snapshot order.
    pub(super) fn keys(&self) -> Vec<ItemKey> {
        self.snapshot
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| !i.dismissed && self.provider(&i.provider_id).is_some())
            .map(|i| (i.provider_id.clone(), i.id.clone()))
            .collect()
    }

    /// Provider names in snapshot order, for the group preview.
    pub(super) fn provider_names(&self) -> Vec<String> {
        self.snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .map(|p| plain(&p.name))
            .collect()
    }

    pub(super) fn entry(&self, key: &ItemKey) -> Option<Entry> {
        let item = self.item(key)?;
        let provider = self.provider(&key.0)?;
        let label: String = plain(&provider.name)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .chars()
            .take(SOURCE_LABEL_WIDTH)
            .collect();
        Some(Entry {
            label,
            source_ref: plain(&item.source_ref),
            title: plain(&item.title),
            reason: plain(&item.reason),
            provider: plain(&provider.name),
            project: item.project.as_deref().map(plain),
            prompt: plain(&item.draft.prompt),
            provider_error: provider.error.as_deref().map(plain),
        })
    }

    /// Reloads when the snapshot is stale and nothing is in flight. Never
    /// while `connected` is false, and a failed load waits a full interval.
    pub(super) fn tick(&mut self, connected: bool) {
        let due = |at: Option<std::time::Instant>| at.is_none_or(|at| at.elapsed() >= FEED_RELOAD);
        if connected && !self.busy && due(self.loaded_at) && due(self.load_attempt_at) {
            self.send(Req::Load, false);
        }
    }

    #[cfg(test)]
    pub(super) fn set_snapshot_for_test(&mut self, snapshot: serde_json::Value) {
        self.snapshot = Some(serde_json::from_value(snapshot).unwrap());
    }

    /// Test feed whose worker records each request as text on `sink` and
    /// answers it from `snapshot`; a prepare resolves to `/tmp`.
    #[cfg(test)]
    pub(super) fn recording_for_test(
        snapshot: serde_json::Value,
        sink: std::sync::mpsc::Sender<String>,
    ) -> Self {
        let worker = Worker::spawn("test-suggestions-feed", move |req| {
            let parse = || serde_json::from_value(snapshot.clone()).map_err(text);
            match req {
                Req::Load => {
                    let _ = sink.send("load".into());
                    Res::Snapshot(parse())
                }
                Req::Dismiss {
                    provider_id,
                    id,
                    dismissed,
                } => {
                    let _ = sink.send(format!("dismiss {provider_id}/{id} {dismissed}"));
                    Res::Snapshot(parse())
                }
                Req::Prepare {
                    provider_id, id, ..
                } => {
                    let _ = sink.send(format!("prepare {provider_id}/{id}"));
                    Res::Prepared(Ok(Prepared {
                        title: "Prepared title".into(),
                        initial_prompt: "Prepared prompt".into(),
                        tool: "claude".into(),
                        project: Some(PreparedProject {
                            path: "/tmp".into(),
                        }),
                    }))
                }
                Req::Refresh => Res::Refreshed(Ok(())),
            }
        });
        Self::with_worker(worker)
    }

    #[cfg(test)]
    pub(super) fn failed_load_for_test(message: &str) -> Self {
        let seeded = Res::Snapshot(Err(message.into()));
        Self::with_worker(Worker::seeded_for_test("test-suggestions-feed", seeded))
    }

    /// A reload the user asked for, so its failure is shown.
    pub(super) fn reload(&mut self) {
        self.submit(Req::Load);
    }

    pub(super) fn refresh(&mut self) {
        self.submit(Req::Refresh);
    }

    pub(super) fn dismiss(&mut self, key: &ItemKey, dismissed: bool) {
        self.submit(Req::Dismiss {
            provider_id: key.0.clone(),
            id: key.1.clone(),
            dismissed,
        });
    }

    pub(super) fn prepare(&mut self, key: ItemKey) {
        self.submit_prepare(key, PrepareTarget::Home);
    }

    fn prepare_in_view(&mut self, key: ItemKey) {
        self.submit_prepare(key, PrepareTarget::View);
    }

    fn submit_prepare(&mut self, key: ItemKey, target: PrepareTarget) {
        self.submit(Req::Prepare {
            provider_id: key.0,
            id: key.1,
            target,
        });
    }

    pub(super) fn poll(&mut self) -> FeedPoll {
        let mut out = FeedPoll {
            changed: false,
            refreshed: false,
            start: None,
            error: None,
        };
        let res = match self.worker.try_recv() {
            Ok(res) => res,
            Err(TryRecvError::Disconnected) if self.busy => {
                self.busy = false;
                self.queued.clear();
                out.error = Some("The suggestions worker stopped".into());
                return out;
            }
            Err(_) => return out,
        };
        self.busy = false;
        let user = std::mem::take(&mut self.in_flight_user);
        match res {
            Res::Snapshot(Ok(snapshot)) => {
                self.snapshot = Some(snapshot);
                self.loaded_at = Some(std::time::Instant::now());
                out.changed = true;
            }
            Res::Prepared(Ok(prepared)) => {
                let key = self.prepare_key.take();
                match key.and_then(|key| {
                    start_for(self.snapshot.as_ref(), &key, prepared).map(|start| (key, start))
                }) {
                    Some((key, start)) => out.start = Some((key, start, self.prepare_target)),
                    None => out.error = Some(NO_PROJECT.into()),
                }
            }
            Res::Snapshot(Err(e)) | Res::Refreshed(Err(e)) | Res::Prepared(Err(e)) => {
                self.prepare_key = None;
                if user {
                    out.error = Some(e);
                } else {
                    tracing::debug!(target: "tui.suggestions", error = %e, "suggestions reload failed");
                }
            }
            Res::Refreshed(Ok(())) => out.refreshed = true,
        }
        if let Some(next) = self.queued.pop_front() {
            self.send(next, true);
        }
        out
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

    struct Rig {
        view: SuggestionsView,
        store: SuggestionsFeed,
    }

    fn view_with(items: Vec<Item>, worker: Worker<Req, Res>) -> Rig {
        let mut store = SuggestionsFeed::with_worker(worker);
        store.snapshot = Some(Snapshot {
            providers: vec![Provider {
                id: "p".into(),
                name: "GitHub".into(),
                error: None,
            }],
            items,
        });
        let view = SuggestionsView::new(&store);
        Rig { view, store }
    }

    fn idle_worker() -> Worker<Req, Res> {
        Worker::spawn("test-suggestions", |_| unreachable!("no request expected"))
    }

    fn press(rig: &mut Rig, code: KeyCode, mutations: bool) -> SuggestionsAction {
        rig.view.handle_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            mutations,
            &mut rig.store,
        )
    }

    fn render_text(rig: &Rig) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                rig.view
                    .render(frame, frame.area(), &Theme::default(), &rig.store)
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(100)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ids(rig: &Rig) -> Vec<String> {
        rig.view.order.iter().map(|k| k.1.clone()).collect()
    }

    /// Seeded results are queued before the worker thread starts and a dead
    /// worker disconnects shortly after a request, so a bounded retry loop
    /// only covers scheduling.
    fn poll_until_settled(rig: &mut Rig) -> bool {
        for _ in 0..200 {
            let mut polled = rig.store.poll();
            if rig.view.apply(&rig.store, &mut polled) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    fn prepared(project: Option<&str>) -> Prepared {
        Prepared {
            title: "t".into(),
            initial_prompt: "final prompt".into(),
            tool: "claude".into(),
            project: project.map(|path| PreparedProject { path: path.into() }),
        }
    }

    #[test]
    fn dismissed_items_are_hidden_until_toggled_and_keep_snapshot_order() {
        let mut rig = view_with(
            vec![
                item("b", Some("zeta"), false),
                item("a", Some("alpha"), false),
                item("gone", Some("alpha"), true),
            ],
            idle_worker(),
        );
        assert_eq!(ids(&rig), ["b", "a"]);
        press(&mut rig, KeyCode::Char('d'), true);
        assert_eq!(ids(&rig), ["b", "a", "gone"]);
    }

    #[test]
    fn selection_follows_the_item_across_a_snapshot_replace() {
        let mut rig = view_with(
            vec![item("a", Some("p1"), false), item("b", Some("p2"), false)],
            idle_worker(),
        );
        press(&mut rig, KeyCode::Char('j'), true);
        assert_eq!(rig.view.selected_key().unwrap().1, "b");
        rig.store.snapshot = Some(Snapshot {
            providers: vec![],
            items: vec![
                item("new", Some("p0"), false),
                item("a", Some("p1"), false),
                item("b", Some("p2"), false),
            ],
        });
        rig.view.sync(&rig.store);
        assert_eq!(rig.view.selected_key().unwrap().1, "b");
    }

    #[test]
    fn mutating_keys_are_refused_when_read_only() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        press(&mut rig, KeyCode::Char('x'), false);
        press(&mut rig, KeyCode::Char('r'), false);
        assert!(!rig.store.busy);
        assert!(rig.view.error.is_some());
    }

    #[test]
    fn enter_resolves_the_final_prompt_and_a_second_enter_starts() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        assert!(matches!(
            press(&mut rig, KeyCode::Enter, true),
            SuggestionsAction::Continue
        ));
        assert!(poll_until_settled(&mut rig));
        assert!(rig.view.prepared.is_some());
        assert!(render_text(&rig).contains("final prompt"));
        let SuggestionsAction::Start(start) = press(&mut rig, KeyCode::Enter, true) else {
            panic!("expected a start");
        };
        assert_eq!(start.path, "/repo");
        assert_eq!(start.initial_prompt, "final prompt");
        assert!(rig.view.prepared.is_none());
    }

    #[test]
    fn prepare_without_a_registered_project_cannot_start() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(None))));
        let mut rig = view_with(vec![item("a", None, false)], worker);
        press(&mut rig, KeyCode::Enter, true);
        assert!(poll_until_settled(&mut rig));
        assert!(rig.view.prepared.is_none());
        assert!(rig
            .view
            .error
            .as_deref()
            .unwrap()
            .contains("No registered project"));
    }

    #[test]
    fn feed_loads_only_when_connected_and_backs_off_after_a_failure() {
        let (sink, requests) = std::sync::mpsc::channel();
        let mut feed = SuggestionsFeed::recording_for_test(
            serde_json::json!({ "providers": [], "items": [] }),
            sink,
        );
        feed.tick(false);
        feed.tick(true);
        assert_eq!(
            requests
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "load"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !feed.poll().changed {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Fresh snapshot: no second load is due; the prepare is the next request.
        feed.tick(true);
        feed.prepare(("p".into(), "a".into()));
        assert_eq!(
            requests
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "prepare p/a"
        );
    }

    #[test]
    fn failed_request_keeps_the_last_snapshot() {
        let worker = Worker::seeded_for_test("t", Res::Snapshot(Err("daemon down".into())));
        let mut rig = view_with(vec![item("a", None, false)], worker);
        rig.store.reload();
        assert!(poll_until_settled(&mut rig));
        assert_eq!(rig.view.error.as_deref(), Some("daemon down"));
        assert_eq!(ids(&rig), ["a"]);
    }

    #[test]
    fn refresh_notifies_and_asks_for_a_reload() {
        let worker = Worker::seeded_for_test("t", Res::Refreshed(Ok(())));
        let mut rig = view_with(vec![], worker);
        rig.store.refresh();
        assert!(poll_until_settled(&mut rig));
        assert!(rig.view.notice.as_deref().unwrap().contains("Press l"));
    }

    #[test]
    fn a_dead_worker_clears_busy_and_reports_an_error() {
        let worker = Worker::spawn("test-suggestions", |_: Req| -> Res {
            panic!("worker died")
        });
        let mut rig = view_with(vec![item("a", None, false)], worker);
        press(&mut rig, KeyCode::Char('l'), true);
        assert!(rig.store.busy);
        assert!(poll_until_settled(&mut rig));
        assert!(!rig.store.busy);
        assert!(rig.view.error.is_some());
        let mut polled = rig.store.poll();
        assert!(!rig.view.apply(&rig.store, &mut polled));
    }

    #[test]
    fn a_dismiss_in_the_view_reaches_the_home_keys_without_a_reload() {
        let (sink, requests) = std::sync::mpsc::channel();
        let snapshot = serde_json::json!({
            "providers": [{ "id": "p", "name": "GitHub" }],
            "items": [{
                "provider_id": "p", "id": "a", "title": "a", "reason": "r",
                "source_url": "https://example.test/a", "source_ref": "#a",
                "preview": "", "dismissed": true, "draft": { "prompt": "x" }
            }],
        });
        let mut store = SuggestionsFeed::recording_for_test(snapshot, sink);
        store.set_snapshot_for_test(serde_json::json!({
            "providers": [{ "id": "p", "name": "GitHub" }],
            "items": [{
                "provider_id": "p", "id": "a", "title": "a", "reason": "r",
                "source_url": "https://example.test/a", "source_ref": "#a",
                "preview": "", "draft": { "prompt": "x" }
            }],
        }));
        let mut view = SuggestionsView::new(&store);
        assert_eq!(store.keys().len(), 1);
        view.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            true,
            &mut store,
        );
        assert_eq!(
            requests
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "dismiss p/a true"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut polled = store.poll();
        while !polled.changed {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
            polled = store.poll();
        }
        assert!(store.keys().is_empty());
        assert!(requests.try_recv().is_err(), "no reload was sent");
    }

    #[test]
    fn a_view_prepare_answered_after_the_view_closed_is_left_for_the_caller_to_drop() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        press(&mut rig, KeyCode::Enter, true);
        let mut view_closed = rig.store.poll();
        for _ in 0..200 {
            if view_closed.start.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            view_closed = rig.store.poll();
        }
        assert!(matches!(
            view_closed.start,
            Some((_, _, PrepareTarget::View))
        ));
    }

    #[test]
    fn view_shows_prompt_and_preview_as_plain_text() {
        let mut hostile = item("a", Some("repo"), false);
        hostile.draft.prompt = "run \u{1b}[31mrm -rf\u{1b}[0m now".into();
        let rig = view_with(vec![hostile], idle_worker());
        let text = render_text(&rig);
        for expected in [
            "GitHub a",
            "reason · repo",
            "GitHub · ref-a · https://example.test/x",
            "preview text",
            "Provider draft prompt",
            "run \u{fffd}[31mrm -rf\u{fffd}[0m now",
        ] {
            assert!(text.contains(expected), "missing {expected:?}");
        }
        assert!(!text.contains('\u{1b}'));
    }
}
