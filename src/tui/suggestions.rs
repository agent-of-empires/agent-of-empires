//! Suggested sessions browser. Items come from the daemon's suggestions API;
//! starting one opens the regular new-session dialog prefilled from `prepare`.
//! Provider text is untrusted: it is shown as plain text, control characters
//! stripped, and the prompt is visible before Enter.

use std::sync::mpsc::TryRecvError;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use serde::Deserialize;

#[cfg(not(test))]
use crate::acp::client::{locate_daemon, HttpClient};
use crate::tui::session_feed::MutationBlock;
use crate::tui::styles::Theme;
use crate::tui::worker::Worker;

#[derive(Debug, Clone, Deserialize)]
struct Snapshot {
    providers: Vec<Provider>,
    items: Vec<Item>,
    #[serde(default)]
    prefs: Prefs,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Prefs {
    show_source_ref: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            show_source_ref: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Provider {
    id: String,
    name: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    /// Unix seconds of the last report.
    #[serde(default)]
    updated_at: Option<i64>,
    #[serde(default)]
    refreshing: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// The registered Project an item resolves to.
#[derive(Debug, Clone, Deserialize)]
struct ResolvedProject {
    name: String,
    path: String,
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
    #[serde(default)]
    started_session: Option<Started>,
    #[serde(default)]
    resolved_project: Option<ResolvedProject>,
    /// The source closed or changed; the item cannot start.
    #[serde(default)]
    stale: bool,
}

/// The live Session an item was started as.
#[derive(Debug, Clone, Deserialize)]
struct Started {
    id: String,
    status: String,
    #[serde(default)]
    archived: bool,
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
    /// Links the created Session to the suggestion; `None` for "Start another".
    pub suggestion: Option<crate::session::SuggestionOrigin>,
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
    /// The view with this epoch; a closed view's answer matches no later view.
    View(u64),
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

/// `plain` for text shown on a single line: line breaks cannot start a second one.
fn plain_line(s: &str) -> String {
    plain(s).replace('\n', "\u{fffd}")
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
        .map_or(key.0.as_str(), |p| p.name.as_str());
    // The dialog is where the user confirms this text, so it is sanitised
    // here, once, and what they see is what is sent.
    Some(Start {
        provider: plain_line(provider),
        source_ref: item.map(|i| plain_line(&i.source_ref)).unwrap_or_default(),
        reason: item.map(|i| plain_line(&i.reason)).unwrap_or_default(),
        source_text: item
            .filter(|i| !i.preview.trim().is_empty())
            .map(|i| plain(&i.preview)),
        path: project.path,
        title: plain_line(&prepared.title),
        initial_prompt: plain(&prepared.initial_prompt),
        tool: prepared.tool,
        suggestion: Some(crate::session::SuggestionOrigin {
            provider_id: key.0.clone(),
            id: key.1.clone(),
        }),
    })
}

/// The note for a daemon that refuses mutations, in the web dashboard's words.
fn block_note(block: MutationBlock) -> &'static str {
    match block {
        MutationBlock::ReadOnly => {
            "This dashboard is read-only. Start, Dismiss and Refresh are off."
        }
        MutationBlock::Offline => {
            "Disconnected. Suggestions show from the last fetch; reconnect to start one."
        }
    }
}

const STALE_NOTE: &str = "No longer open at the source. Dismiss it, or wait for the next refresh.";

/// One status line per provider: when it last updated, or why it did not.
fn provider_status(provider: &Provider, any_items: bool, theme: &Theme) -> (String, Color) {
    let name = &provider.name;
    if provider.refreshing {
        (format!("⠙ {name}: refreshing…"), theme.dimmed)
    } else if let Some(error) = &provider.error {
        (format!("{name}: couldn't refresh, {error}"), theme.error)
    } else if provider.updated_at.is_none() {
        (format!("{name}: waiting for first results"), theme.dimmed)
    } else if !any_items {
        (format!("{name}: nothing new"), theme.dimmed)
    } else {
        let ago = provider.updated_at.map_or_else(String::new, |at| {
            format!(" {}", ago_label(chrono::Utc::now().timestamp() - at))
        });
        (format!("{name}: updated{ago}"), theme.text)
    }
}

/// "just now", "5 min ago", "3h ago", "2d ago".
fn ago_label(secs: i64) -> String {
    match secs.max(0) / 60 {
        0 => "just now".to_string(),
        m if m < 60 => format!("{m} min ago"),
        m if m < 60 * 24 => format!("{}h ago", m / 60),
        m => format!("{}d ago", m / 60 / 24),
    }
}

/// How long a dismissed row stays listed so `x` can undo it.
const UNDO_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) enum SuggestionsAction {
    Continue,
    Close,
    /// The user confirmed a prepared suggestion.
    Start(Box<Start>),
    /// Jump to the Session an item was started as.
    OpenSession(String),
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
    epoch: u64,
    selected: usize,
    detail_scroll: u16,
    show_dismissed: bool,
    /// The item a `prepare` is in flight for.
    preparing: Option<ItemKey>,
    /// The in-flight `prepare` is for "Start another", which does not claim the link.
    prepare_another: bool,
    /// The last start that failed, shown on its row; Enter tries again.
    failed: Option<(ItemKey, String)>,
    /// The item a dismiss request is in flight for.
    dismissing: Option<ItemKey>,
    /// The item a restore request is in flight for; it leaves `recent` only once that succeeds.
    restoring: Option<ItemKey>,
    /// Items dismissed in the last [`UNDO_WINDOW`], kept listed so `x` undoes them.
    recent: Vec<(ItemKey, std::time::Instant)>,
}

impl SuggestionsView {
    pub(super) fn new(store: &mut SuggestionsFeed) -> Self {
        store.view_epoch += 1;
        let mut view = Self::empty();
        view.epoch = store.view_epoch;
        view.reorder(store, None);
        view
    }

    fn empty() -> Self {
        Self {
            order: Vec::new(),
            error: None,
            notice: None,
            prepared: None,
            epoch: 0,
            selected: 0,
            detail_scroll: 0,
            show_dismissed: false,
            preparing: None,
            prepare_another: false,
            failed: None,
            dismissing: None,
            restoring: None,
            recent: Vec::new(),
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

    pub(super) fn selected_key(&self) -> Option<ItemKey> {
        self.order.get(self.selected).cloned()
    }

    fn is_recent(&self, key: &ItemKey) -> bool {
        self.recent.iter().any(|(k, _)| k == key)
    }

    /// The Project path an item sits under; empty when unresolved.
    fn group_key<'a>(&self, store: &'a SuggestionsFeed, index: usize) -> &'a str {
        self.item_at(store, index)
            .and_then(|i| i.resolved_project.as_ref())
            .map_or("", |p| p.path.as_str())
    }

    /// Headings show only once some item resolved to a Project.
    fn has_project_groups(&self, store: &SuggestionsFeed) -> bool {
        (0..self.order.len()).any(|i| !self.group_key(store, i).is_empty())
    }

    /// Rebuilds `order`, grouped by resolved Project in first-seen order with
    /// unresolved last (snapshot order holds within a group), keeping the
    /// selection on `key` when it is still listed.
    fn reorder(&mut self, store: &SuggestionsFeed, key: Option<ItemKey>) {
        let listed: Vec<&Item> = store
            .snapshot
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| {
                self.show_dismissed
                    || !i.dismissed
                    || self.is_recent(&(i.provider_id.clone(), i.id.clone()))
            })
            .collect();
        let mut seen: Vec<&str> = Vec::new();
        for item in &listed {
            if let Some(project) = &item.resolved_project {
                if !seen.contains(&project.path.as_str()) {
                    seen.push(&project.path);
                }
            }
        }
        let mut ranked: Vec<(usize, &Item)> = listed
            .into_iter()
            .map(|i| {
                let rank = i
                    .resolved_project
                    .as_ref()
                    .and_then(|p| seen.iter().position(|s| *s == p.path))
                    .unwrap_or(usize::MAX);
                (rank, i)
            })
            .collect();
        ranked.sort_by_key(|(rank, _)| *rank);
        self.order = ranked
            .into_iter()
            .map(|(_, i)| (i.provider_id.clone(), i.id.clone()))
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

    /// `block` says why the daemon refuses mutations, or `None` when it allows them.
    pub(super) fn handle_key(
        &mut self,
        key: KeyEvent,
        block: Option<MutationBlock>,
        store: &mut SuggestionsFeed,
    ) -> SuggestionsAction {
        let mutations_allowed = block.is_none();
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
                    if dismissed {
                        self.dismissing = Some(key.clone());
                    } else {
                        self.restoring = Some(key.clone());
                    }
                    store.dismiss(&key, dismissed);
                }
            }
            KeyCode::Char('r' | 'x') => {
                self.error = block.map(|b| block_note(b).to_string());
            }
            KeyCode::Enter | KeyCode::Char('N') => {
                let another = key.code == KeyCode::Char('N');
                let Some(key) = self.selected_key() else {
                    return SuggestionsAction::Continue;
                };
                let item = self.item_at(store, self.selected);
                if let Some(started) = item
                    .and_then(|i| i.started_session.as_ref())
                    .filter(|_| !another)
                {
                    return SuggestionsAction::OpenSession(started.id.clone());
                }
                if let Some(block) = block {
                    self.error = Some(block_note(block).to_string());
                    return SuggestionsAction::Continue;
                }
                if item.is_some_and(|i| i.stale) {
                    self.notice = Some(STALE_NOTE.to_string());
                    return SuggestionsAction::Continue;
                }
                if self.prepare_another == another
                    && self.prepared.as_ref().is_some_and(|(k, _)| *k == key)
                {
                    if let Some((_, start)) = self.prepared.take() {
                        return SuggestionsAction::Start(Box::new(start));
                    }
                }
                self.begin();
                self.failed = None;
                self.preparing = Some(key.clone());
                self.prepare_another = another;
                store.prepare_in_view(key, self.epoch);
            }
            _ => {}
        }
        SuggestionsAction::Continue
    }

    /// Takes what a store poll holds for this view. True when state changed.
    pub(super) fn apply(&mut self, store: &SuggestionsFeed, polled: &mut FeedPoll) -> bool {
        let mut changed = false;
        let before = self.recent.len();
        self.recent.retain(|(_, at)| at.elapsed() < UNDO_WINDOW);
        if self.recent.len() != before {
            self.sync(store);
            changed = true;
        }
        if polled.changed {
            // The reply to a dismiss or restore is the snapshot that shows it.
            if let Some(key) = self.dismissing.clone() {
                match store.item(&key) {
                    Some(item) if item.dismissed => {
                        self.recent.retain(|(k, _)| *k != key);
                        self.recent.push((key, std::time::Instant::now()));
                        self.dismissing = None;
                    }
                    Some(_) => {}
                    None => self.dismissing = None,
                }
            }
            if let Some(key) = self.restoring.clone() {
                if store.item(&key).is_none_or(|i| !i.dismissed) {
                    self.recent.retain(|(k, _)| *k != key);
                    self.restoring = None;
                }
            }
            self.sync(store);
            changed = true;
        }
        if polled.refreshed {
            self.notice = Some("Refresh requested. Press l to reload the list.".into());
            changed = true;
        }
        if let Some(error) = polled.error.take() {
            // A failed request leaves the undo window as it was.
            self.dismissing = None;
            self.restoring = None;
            match self.preparing.take() {
                Some(key) => self.failed = Some((key, error)),
                None => self.error = Some(error),
            }
            changed = true;
        }
        if matches!(polled.start, Some((_, _, PrepareTarget::View(e))) if e == self.epoch) {
            if let Some((key, mut start, _)) = polled.start.take() {
                self.preparing = None;
                if self.prepare_another {
                    start.suggestion = None;
                }
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
        mutation_block: Option<MutationBlock>,
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
        if let Some(blocked) = mutation_block {
            footer.push(Line::from(Span::styled(
                block_note(blocked),
                Style::default().fg(theme.waiting),
            )));
        }
        for provider in store
            .snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .filter(|p| !p.enabled)
        {
            footer.push(Line::from(Span::styled(
                plain(&format!(
                    "{0} suggestions are hidden because {0} is off in Settings › Suggestions.",
                    provider.name
                )),
                Style::default().fg(theme.dimmed),
            )));
        }
        for provider in store
            .snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .filter(|p| p.enabled)
        {
            let any_items = store
                .snapshot
                .iter()
                .flat_map(|s| &s.items)
                .any(|i| i.provider_id == provider.id);
            let (status, color) = provider_status(provider, any_items, theme);
            footer.push(Line::from(Span::styled(
                plain(&status),
                Style::default().fg(color),
            )));
        }
        footer.push(Line::from(Span::styled(
            "j/k select · Enter review/start/open · N start another · x dismiss/restore · d dismissed · r refresh · l reload · PgUp/PgDn prompt · Esc back",
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

        let height = list_area.height as usize;
        let headed = self.has_project_groups(store);
        // A heading opens the window and every group in it.
        let heading_at = |first: usize, index: usize| {
            headed
                && (index == first
                    || self.group_key(store, index) != self.group_key(store, index - 1))
        };
        let window = |first: usize| -> usize {
            (first..=self.selected)
                .map(|i| 2 + usize::from(heading_at(first, i)))
                .sum()
        };
        let mut start = self.selected.min(self.order.len().saturating_sub(1));
        while start > 0 && window(start - 1) <= height {
            start -= 1;
        }
        let mut lines: Vec<Line> = Vec::new();
        for index in start..self.order.len() {
            if lines.len() >= height {
                break;
            }
            let Some(item) = self.item_at(store, index) else {
                continue;
            };
            let item_key = (item.provider_id.clone(), item.id.clone());
            if heading_at(start, index) {
                let key = self.group_key(store, index);
                let name = item
                    .resolved_project
                    .as_ref()
                    .map_or("No project", |p| p.name.as_str());
                let count = (0..self.order.len())
                    .filter(|&i| self.group_key(store, i) == key)
                    .count();
                lines.push(Line::from(vec![
                    Span::styled(
                        plain(name),
                        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {count}"), Style::default().fg(theme.dimmed)),
                ]));
            }
            let current = index == self.selected;
            let marker = if current { "› " } else { "  " };
            let state = match (item.dismissed, self.is_recent(&item_key)) {
                (false, _) => "",
                (true, true) => " [dismissed, x to undo]",
                (true, false) => " [dismissed]",
            };
            let title_style = if item.dismissed || item.stale {
                Style::default().fg(theme.dimmed)
            } else if current {
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    plain(&format!(
                        "{marker}{} {}",
                        store.provider_name(&item.provider_id),
                        store
                            .source_ref(item)
                            .map(|r| format!("{r} "))
                            .unwrap_or_default()
                    )),
                    Style::default().fg(theme.dimmed),
                ),
                Span::styled(plain(&format!("{}{state}", item.title)), title_style),
            ]));
            let failure = self.failed.as_ref().filter(|(key, _)| *key == item_key);
            if let Some((_, message)) = failure {
                lines.push(Line::from(Span::styled(
                    plain(&format!(
                        "    Couldn't start: {message} · Enter tries again"
                    )),
                    Style::default().fg(theme.error),
                )));
                continue;
            }
            lines.push(Line::from(Span::styled(
                plain(&format!(
                    "    {} · {}{}",
                    item.reason,
                    item.resolved_project
                        .as_ref()
                        .map(|p| p.name.as_str())
                        .or(item.project.as_deref())
                        .unwrap_or("no project"),
                    item.started_session
                        .as_ref()
                        .map(|s| if s.archived {
                            " · Archived: Enter opens".to_string()
                        } else {
                            format!(" · Session {}: Enter opens", s.status.to_lowercase())
                        })
                        .unwrap_or_default()
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
            let mut source = vec![store.provider_name(&item.provider_id)];
            source.extend(store.source_ref(item));
            source.push(&item.source_url);
            detail.push(Line::from(plain(&source.join(" · "))));
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
    /// The item's `source_url` when it is an http(s) address; core validates
    /// it, this is a second check before anything is opened.
    pub source_url: Option<String>,
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
    /// Epoch of the most recently opened full view.
    view_epoch: u64,
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
            view_epoch: 0,
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

    /// The item's `source_ref`, unless it is blank or the user hid references.
    fn source_ref<'a>(&self, item: &'a Item) -> Option<&'a str> {
        let shown = self
            .snapshot
            .as_ref()
            .is_none_or(|s| s.prefs.show_source_ref);
        (shown && !item.source_ref.is_empty()).then_some(item.source_ref.as_str())
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
            .filter(|i| !i.dismissed && !i.stale && self.provider(&i.provider_id).is_some())
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
            source_ref: self.source_ref(item).map(plain).unwrap_or_default(),
            title: plain(&item.title),
            reason: plain(&item.reason),
            provider: plain(&provider.name),
            project: item.project.as_deref().map(plain),
            prompt: plain(&item.draft.prompt),
            provider_error: provider.error.as_deref().map(plain),
            source_url: crate::util::is_http_url(&item.source_url).then(|| item.source_url.clone()),
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

    fn prepare_in_view(&mut self, key: ItemKey, epoch: u64) {
        self.submit_prepare(key, PrepareTarget::View(epoch));
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
            started_session: None,
            resolved_project: None,
            stale: false,
        }
    }

    fn in_project(mut item: Item, name: &str) -> Item {
        item.resolved_project = Some(ResolvedProject {
            name: name.into(),
            path: format!("/work/{name}"),
        });
        item
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
                enabled: true,
                updated_at: Some(chrono::Utc::now().timestamp() - 120),
                refreshing: false,
            }],
            items,
            prefs: Prefs::default(),
        });
        let view = SuggestionsView::new(&mut store);
        Rig { view, store }
    }

    fn idle_worker() -> Worker<Req, Res> {
        Worker::spawn("test-suggestions", |_| unreachable!("no request expected"))
    }

    fn press(rig: &mut Rig, code: KeyCode, mutations: bool) -> SuggestionsAction {
        press_blocked(rig, code, (!mutations).then_some(MutationBlock::ReadOnly))
    }

    fn press_blocked(
        rig: &mut Rig,
        code: KeyCode,
        block: Option<MutationBlock>,
    ) -> SuggestionsAction {
        rig.view.handle_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            block,
            &mut rig.store,
        )
    }

    fn render_text(rig: &Rig) -> String {
        render_text_blocked(rig, None)
    }

    fn render_text_blocked(rig: &Rig, block: Option<MutationBlock>) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                rig.view
                    .render(frame, frame.area(), &Theme::default(), &rig.store, block)
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
    fn start_sanitises_every_provider_string_but_keeps_prompt_newlines() {
        let hostile = |s: &str| format!("{s}\u{1b}[31m\u{202e}x");
        let mut it = item("a", Some("repo"), false);
        it.source_ref = hostile("ref");
        it.reason = hostile("why\nmore");
        it.preview = hostile("body");
        let snapshot = Snapshot {
            providers: vec![Provider {
                id: "p".into(),
                name: hostile("GitHub"),
                error: None,
                enabled: true,
                updated_at: None,
                refreshing: false,
            }],
            items: vec![it],
            prefs: Prefs::default(),
        };
        let mut prep = prepared(Some("/repo"));
        prep.title = hostile("a\nb");
        prep.initial_prompt = format!("{}\nsecond line", hostile("go"));

        let start = start_for(Some(&snapshot), &("p".into(), "a".into()), prep).unwrap();
        let clean = |s: &str| format!("{s}\u{fffd}[31m\u{fffd}x");
        assert_eq!(start.provider, clean("GitHub"));
        assert_eq!(start.source_ref, clean("ref"));
        assert_eq!(start.reason, clean("why\u{fffd}more"));
        assert_eq!(start.source_text.as_deref(), Some(clean("body").as_str()));
        assert_eq!(start.title, clean("a\u{fffd}b"));
        assert_eq!(
            start.initial_prompt,
            format!("{}\nsecond line", clean("go"))
        );
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
            prefs: Prefs::default(),
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
        assert!(render_text(&rig).contains("Couldn't start: No registered project"));
    }

    fn started(id: &str, status: &str) -> Option<Started> {
        Some(Started {
            id: id.into(),
            status: status.into(),
            archived: false,
        })
    }

    #[test]
    fn a_started_item_opens_its_session_and_start_another_does_not_link() {
        let mut linked = item("a", Some("repo"), false);
        linked.started_session = started("sess-1", "Running");
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut rig = view_with(vec![linked], worker);
        assert!(render_text(&rig).contains("Session running"));
        let SuggestionsAction::OpenSession(id) = press(&mut rig, KeyCode::Enter, true) else {
            panic!("expected the linked session to open");
        };
        assert_eq!(id, "sess-1");

        press(&mut rig, KeyCode::Char('N'), true);
        assert!(poll_until_settled(&mut rig));
        let SuggestionsAction::Start(start) = press(&mut rig, KeyCode::Char('N'), true) else {
            panic!("expected a start");
        };
        assert!(start.suggestion.is_none());
    }

    #[test]
    fn a_plain_start_carries_the_suggestion_reference() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        press(&mut rig, KeyCode::Enter, true);
        assert!(poll_until_settled(&mut rig));
        let SuggestionsAction::Start(start) = press(&mut rig, KeyCode::Enter, true) else {
            panic!("expected a start");
        };
        let origin = start.suggestion.unwrap();
        assert_eq!(
            (origin.provider_id.as_str(), origin.id.as_str()),
            ("p", "a")
        );
    }

    #[test]
    fn items_group_by_resolved_project_with_unresolved_last() {
        let mut rig = view_with(
            vec![
                item("loose", None, false),
                in_project(item("b1", None, false), "web"),
                in_project(item("a1", None, false), "api"),
                in_project(item("b2", None, false), "web"),
            ],
            idle_worker(),
        );
        assert_eq!(ids(&rig), ["b1", "b2", "a1", "loose"]);
        assert!(render_text(&rig).contains("web 2"));
        for _ in 0..3 {
            press(&mut rig, KeyCode::Char('j'), true);
        }
        let text = render_text(&rig);
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {text}"))
        };
        assert!(
            at("api 1") < at("No project 1"),
            "the window scrolled to the last group"
        );
        let flat = view_with(vec![item("x", None, false)], idle_worker());
        assert!(
            !render_text(&flat).contains("No project"),
            "no headings without a resolved project"
        );
    }

    fn dismissed_snapshot(rig: &mut Rig, dismissed: bool) {
        rig.store.snapshot = Some(Snapshot {
            providers: vec![],
            items: vec![item("a", None, dismissed)],
            prefs: Prefs::default(),
        });
    }

    #[test]
    fn a_dismissed_item_stays_dimmed_for_ten_seconds_and_x_undoes_it() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        let key = ("p".to_string(), "a".to_string());
        rig.view.dismissing = Some(key.clone());
        dismissed_snapshot(&mut rig, true);
        let mut polled = FeedPoll {
            changed: true,
            refreshed: false,
            start: None,
            error: None,
        };
        assert!(rig.view.apply(&rig.store, &mut polled));
        assert_eq!(ids(&rig), ["a"], "kept while the undo window is open");
        assert!(render_text(&rig).contains("[dismissed, x to undo]"));

        let stale = std::time::Instant::now()
            .checked_sub(UNDO_WINDOW + std::time::Duration::from_secs(1))
            .expect("monotonic clock far enough from boot");
        rig.view.recent[0].1 = stale;
        let mut polled = FeedPoll {
            changed: false,
            refreshed: false,
            start: None,
            error: None,
        };
        assert!(rig.view.apply(&rig.store, &mut polled));
        assert!(
            ids(&rig).is_empty(),
            "then only the dismissed list shows it"
        );
        press(&mut rig, KeyCode::Char('d'), true);
        assert_eq!(ids(&rig), ["a"]);
    }

    #[test]
    fn a_failed_restore_keeps_the_undo_window() {
        let mut rig = view_with(vec![item("a", None, true)], idle_worker());
        let key = ("p".to_string(), "a".to_string());
        rig.view
            .recent
            .push((key.clone(), std::time::Instant::now()));
        rig.view.sync(&rig.store);
        rig.view.restoring = Some(key.clone());
        let mut failed = FeedPoll {
            changed: false,
            refreshed: false,
            start: None,
            error: Some("boom".into()),
        };
        rig.view.apply(&rig.store, &mut failed);
        assert_eq!(
            rig.view.recent.len(),
            1,
            "an API hiccup does not lose the undo"
        );
        assert_eq!(ids(&rig), ["a"]);

        rig.view.restoring = Some(key);
        dismissed_snapshot(&mut rig, false);
        let mut ok = FeedPoll {
            changed: true,
            refreshed: false,
            start: None,
            error: None,
        };
        rig.view.apply(&rig.store, &mut ok);
        assert!(
            rig.view.recent.is_empty(),
            "dropped once the restore succeeded"
        );
    }

    #[test]
    fn a_stale_item_cannot_start_and_says_why() {
        let mut stale = item("a", None, false);
        stale.stale = true;
        let mut rig = view_with(vec![stale], idle_worker());
        assert!(matches!(
            press(&mut rig, KeyCode::Enter, true),
            SuggestionsAction::Continue
        ));
        assert!(!rig.store.busy, "no prepare was sent");
        assert!(render_text(&rig).contains("No longer open at the source"));
    }

    #[test]
    fn a_failed_start_stays_on_its_row_and_enter_tries_again() {
        let worker =
            Worker::seeded_for_test("t", Res::Prepared(Err("branch already exists".into())));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        rig.store.in_flight_user = true;
        press(&mut rig, KeyCode::Enter, true);
        assert!(poll_until_settled(&mut rig));
        assert!(
            rig.view.error.is_none(),
            "the failure is on the row, not the footer"
        );
        assert!(render_text(&rig).contains("Couldn't start: branch already exists"));
        assert!(rig.view.failed.is_some());
    }

    #[test]
    fn read_only_and_offline_explain_themselves_and_still_allow_open() {
        let mut linked = item("a", None, false);
        linked.started_session = started("sess-1", "Idle");
        let mut rig = view_with(vec![item("b", None, false), linked], idle_worker());
        for (block, note) in [
            (
                MutationBlock::ReadOnly,
                "This dashboard is read-only. Start, Dismiss and Refresh are off.",
            ),
            (
                MutationBlock::Offline,
                "Disconnected. Suggestions show from the last fetch; reconnect to start one.",
            ),
        ] {
            assert!(render_text_blocked(&rig, Some(block)).contains(note));
            let SuggestionsAction::Continue = press_blocked(&mut rig, KeyCode::Enter, Some(block))
            else {
                panic!("Start must be refused");
            };
            assert_eq!(rig.view.error.as_deref(), Some(note));
            assert!(!rig.store.busy);
        }
        press(&mut rig, KeyCode::Char('j'), true);
        let SuggestionsAction::OpenSession(id) =
            press_blocked(&mut rig, KeyCode::Enter, Some(MutationBlock::Offline))
        else {
            panic!("a linked session still opens");
        };
        assert_eq!(id, "sess-1");
    }

    #[test]
    fn a_disabled_provider_is_explained() {
        let mut rig = view_with(vec![], idle_worker());
        rig.store.snapshot.as_mut().unwrap().providers[0].enabled = false;
        let text = render_text(&rig);
        assert!(text.contains("GitHub suggestions are hidden because GitHub is off in Settings"));
        assert!(
            !text.contains("GitHub: nothing new"),
            "no status line for a disabled provider"
        );
    }

    #[test]
    fn each_provider_gets_a_status_line() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        assert!(render_text(&rig).contains("GitHub: updated 2 min ago"));
        let provider = &mut rig.store.snapshot.as_mut().unwrap().providers[0];
        provider.refreshing = true;
        assert!(render_text(&rig).contains("⠙ GitHub: refreshing…"));
        let provider = &mut rig.store.snapshot.as_mut().unwrap().providers[0];
        provider.refreshing = false;
        provider.error = Some("API key rejected".into());
        assert!(render_text(&rig).contains("GitHub: couldn't refresh, API key rejected"));
        rig.store.snapshot.as_mut().unwrap().providers[0].error = None;
        rig.store.snapshot.as_mut().unwrap().items.clear();
        assert!(render_text(&rig).contains("GitHub: nothing new"));
    }

    #[test]
    fn provider_text_is_plain_in_every_row_and_line() {
        let hostile = "\u{1b}[31mred\u{1b}[0m\u{202e}live\u{7}";
        let mut bad = item("a", Some("repo"), false);
        bad.title = format!("T {hostile}");
        bad.reason = format!("R {hostile}");
        bad.project = Some(format!("P {hostile}"));
        let mut rig = view_with(vec![bad], idle_worker());
        let provider = &mut rig.store.snapshot.as_mut().unwrap().providers[0];
        provider.name = format!("N {hostile}");
        provider.error = Some(format!("E {hostile}"));
        let text = render_text(&rig);
        assert!(text.contains("T \u{fffd}[31mred"), "{text}");
        for bad in ['\u{1b}', '\u{202e}', '\u{7}'] {
            assert!(!text.contains(bad), "{bad:?} reached the terminal: {text}");
        }
    }

    #[test]
    fn the_subtitle_prefers_the_resolved_project_over_the_hint() {
        let resolved = in_project(item("a", Some("hint-name"), false), "api");
        let hinted = item("b", Some("hint-name"), false);
        let rig = view_with(vec![resolved, hinted], idle_worker());
        let text = render_text(&rig);
        assert!(text.contains("reason · api"), "{text}");
        assert!(text.contains("reason · hint-name"), "{text}");
    }

    #[test]
    fn the_home_feed_never_offers_a_stale_item() {
        let mut stale = item("old", None, false);
        stale.stale = true;
        let rig = view_with(vec![item("fresh", None, false), stale], idle_worker());
        assert_eq!(rig.store.keys(), [("p".to_string(), "fresh".to_string())]);
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
        let mut view = SuggestionsView::new(&mut store);
        assert_eq!(store.keys().len(), 1);
        view.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            None,
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
            Some((_, _, PrepareTarget::View(_)))
        ));
    }

    #[test]
    fn a_view_prepare_does_not_land_in_a_reopened_view() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        press(&mut rig, KeyCode::Enter, true);
        rig.view = SuggestionsView::new(&mut rig.store);
        let mut polled = rig.store.poll();
        for _ in 0..200 {
            if polled.start.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            polled = rig.store.poll();
        }
        assert!(polled.start.is_some());
        rig.view.apply(&rig.store, &mut polled);
        assert!(rig.view.prepared.is_none());
    }

    #[test]
    fn keys_are_not_swallowed_while_a_background_load_is_in_flight() {
        let mut rig = view_with(vec![item("a", Some("repo"), false)], idle_worker());
        rig.store.busy = true;
        let key = ("p".to_string(), "a".to_string());
        let start = start_for(rig.store.snapshot.as_ref(), &key, prepared(Some("/repo")));
        rig.view.prepared = Some((key, start.unwrap()));
        assert!(matches!(
            press(&mut rig, KeyCode::Enter, true),
            SuggestionsAction::Start(_)
        ));

        press(&mut rig, KeyCode::Char('l'), true);
        assert_eq!(rig.store.queued.len(), 1, "the reload waits its turn");
    }

    #[test]
    fn view_shows_prompt_and_preview_as_plain_text() {
        let mut hostile = item("a", Some("repo"), false);
        hostile.draft.prompt = "run \u{1b}[31mrm -rf\u{1b}[0m now".into();
        let rig = view_with(vec![hostile], idle_worker());
        let text = render_text(&rig);
        for expected in [
            "GitHub ref-a a",
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

    #[test]
    fn the_source_reference_follows_the_pref() {
        for (shown, row, detail, entry_ref) in [
            (true, "GitHub ref-a a", "GitHub · ref-a · https://", "ref-a"),
            (false, "GitHub a", "GitHub · https://", ""),
        ] {
            let mut rig = view_with(vec![item("a", None, false)], idle_worker());
            rig.store.snapshot.as_mut().unwrap().prefs.show_source_ref = shown;
            let text = render_text(&rig);
            assert!(text.contains(row), "{shown}: {text}");
            assert!(text.contains(detail), "{shown}: {text}");
            assert_eq!(text.contains("ref-a"), shown, "{text}");
            let key = ("p".to_string(), "a".to_string());
            assert_eq!(rig.store.entry(&key).unwrap().source_ref, entry_ref);
        }
    }
}
