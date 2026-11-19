//! Suggested sessions browser. Items come from the daemon's suggestions API;
//! Enter starts one with its kind's preset through `start`, and `e` opens the
//! regular new-session dialog prefilled from `prepare`. Provider text is
//! untrusted: it is shown as plain text, control characters stripped, and the
//! prompt Enter will send is visible before it.

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
    /// The profile the daemon publishes suggestions under.
    #[serde(default)]
    profile: Option<String>,
    /// The daemon that served this snapshot; set by the loader, not the wire.
    #[serde(skip)]
    daemon: Option<String>,
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
    #[serde(default)]
    view: Option<String>,
    #[serde(default)]
    worktree: bool,
    #[serde(default)]
    sandbox: bool,
    #[serde(default)]
    yolo: bool,
}

/// The kind's preset rows the New Session dialog starts from.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Preset {
    /// `None` leaves the dialog's own default view.
    pub structured: Option<bool>,
    pub worktree: bool,
    pub sandbox: bool,
    pub yolo: bool,
}

/// A suggestion resolved to a registered project, ready for the dialog.
pub(super) struct Start {
    pub provider: String,
    pub source_ref: String,
    pub reason: String,
    /// The provider's `preview`, untrusted; `None` when it is blank.
    pub source_text: Option<String>,
    /// `None` when no single registered project matched; the dialog asks for one.
    pub path: Option<String>,
    pub title: String,
    pub initial_prompt: String,
    pub tool: String,
    pub preset: Preset,
    /// Links the created Session to the suggestion; `None` for "Start another".
    pub suggestion: Option<crate::session::SuggestionOrigin>,
}

/// What `POST /api/suggestions/start` answered, as the TUI acts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StartAnswer {
    Started {
        session_id: String,
        title: String,
    },
    /// The item has a Session already; `None` while that Session is still being created.
    AlreadyStarted(Option<String>),
    /// Repository hooks need approval first: the commands to show, and the
    /// fingerprint the approval must quote.
    NeedsTrust {
        /// The hooks changed after the user reviewed them, so the review shows again.
        changed: bool,
        commands: Vec<String>,
        fingerprint: serde_json::Value,
    },
    /// The item's repo is linked to no Project, so the wizard has to ask for one.
    NoProject,
    Failed(String),
}

fn start_answer(status: u16, body: &serde_json::Value) -> StartAnswer {
    let field = |key: &str| body[key].as_str().unwrap_or_default().to_owned();
    match (status, body["error"].as_str().unwrap_or_default()) {
        (200..=299, _) => StartAnswer::Started {
            session_id: field("session_id"),
            title: field("title"),
        },
        (409, "already_started") => {
            StartAnswer::AlreadyStarted(body["session_id"].as_str().map(str::to_owned))
        }
        (409, code @ ("hooks_need_trust" | "trust_changed")) => {
            let mut commands = Vec::new();
            for (stage, label) in [
                ("on_create", "on create"),
                ("on_launch", "on launch"),
                ("on_destroy", "on destroy"),
            ] {
                let list = body["commands"][stage].as_array().into_iter().flatten();
                commands.extend(
                    list.filter_map(|c| c.as_str())
                        .map(|c| plain_line(&format!("{label}: {c}"))),
                );
            }
            if body["needs_mcp_trust"].as_bool() == Some(true) {
                commands.push("project MCP servers".to_owned());
            }
            StartAnswer::NeedsTrust {
                changed: code == "trust_changed",
                commands,
                fingerprint: body["review_fingerprint"].clone(),
            }
        }
        (400, "no_project") => StartAnswer::NoProject,
        (_, "stale" | "stale_suggestion") => {
            StartAnswer::Failed("That suggestion is no longer open. Reload with r.".into())
        }
        (422, "start_failed") => {
            let hint = field("hint");
            StartAnswer::Failed(plain_line(format!("{} {hint}", field("reason")).trim()))
        }
        _ => StartAnswer::Failed(plain_line(&field("message"))),
    }
}

/// The daemon and profile a list was read from. A mutation is bound to it, so
/// a replaced daemon or another profile never receives work meant for the list shown.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    daemon: Option<String>,
    profile: Option<String>,
}

enum Req {
    Load,
    Refresh,
    Dismiss {
        provider_id: String,
        id: String,
        dismissed: bool,
        origin: Option<Origin>,
    },
    Prepare {
        provider_id: String,
        id: String,
        target: PrepareTarget,
        origin: Option<Origin>,
    },
    /// Starts the item with its preset; `review_fingerprint` approves the repo hooks it names.
    Start {
        provider_id: String,
        id: String,
        review_fingerprint: Option<serde_json::Value>,
        origin: Option<Origin>,
    },
}

impl Req {
    fn origin(&self) -> Option<&Origin> {
        match self {
            Req::Dismiss { origin, .. }
            | Req::Prepare { origin, .. }
            | Req::Start { origin, .. } => origin.as_ref(),
            _ => None,
        }
    }
}

/// Why a request bound to `origin` must not go to `daemon`, which publishes `published`.
fn origin_error(origin: &Origin, daemon: &str, published: Option<&str>) -> Option<String> {
    let moved = origin.daemon.as_deref().is_some_and(|d| d != daemon);
    let switched = origin
        .profile
        .as_deref()
        .zip(published)
        .is_some_and(|(shown, now)| shown != now);
    (moved || switched).then(|| {
        "The daemon or profile changed since this list loaded. Reload and try again.".to_string()
    })
}

/// Which surface asked for a prepare, so its answer goes back to that surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrepareTarget {
    Home,
    /// Home's "start another": the new Session does not claim the suggestion's link.
    HomeAnother,
    /// The view with this epoch; a closed view's answer matches no later view.
    View(u64),
}

enum Res {
    Snapshot(Result<Snapshot, String>),
    /// A dismiss or restore the daemon accepted, but the reload after it failed.
    DismissKept(String),
    Refreshed(Result<(), String>),
    Prepared(Result<Prepared, String>),
    Started(Result<StartAnswer, String>),
}

#[cfg(not(test))]
impl Res {
    fn error(&self) -> Option<&str> {
        match self {
            Res::Snapshot(Err(e))
            | Res::DismissKept(e)
            | Res::Refreshed(Err(e))
            | Res::Prepared(Err(e))
            | Res::Started(Err(e)) => Some(e),
            _ => None,
        }
    }
}

enum ResKind {
    Snapshot,
    Prepare,
    Start,
    Other,
}

fn failed(req: &Req, message: String) -> Res {
    match req {
        Req::Prepare { .. } => Res::Prepared(Err(message)),
        Req::Start { .. } => Res::Started(Err(message)),
        Req::Refresh => Res::Refreshed(Err(message)),
        Req::Load | Req::Dismiss { .. } => Res::Snapshot(Err(message)),
    }
}

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[cfg(not(test))]
async fn load(client: &HttpClient, daemon: &str) -> Result<Snapshot, String> {
    let value = client.suggestions().await.map_err(text)?;
    let mut snapshot: Snapshot = serde_json::from_value(value).map_err(text)?;
    snapshot.daemon = Some(daemon.to_owned());
    Ok(snapshot)
}

/// Refuses a request bound to a daemon or profile other than the one answering.
#[cfg(not(test))]
async fn check_bound(
    client: &HttpClient,
    daemon: &str,
    origin: Option<&Origin>,
) -> Result<(), String> {
    let Some(origin) = origin else { return Ok(()) };
    let published = match &origin.profile {
        Some(_) => load(client, daemon).await?.profile,
        None => None,
    };
    origin_error(origin, daemon, published.as_deref()).map_or(Ok(()), Err)
}

#[cfg(not(test))]
async fn run(req: &Req) -> Res {
    let located = match locate_daemon().await.map_err(text) {
        Ok(endpoint) => endpoint,
        Err(message) => return failed(req, message),
    };
    let daemon = located.base_url.clone();
    let client = match HttpClient::new(located).map_err(text) {
        Ok(client) => client,
        Err(message) => return failed(req, message),
    };
    if let Err(message) = check_bound(&client, &daemon, req.origin()).await {
        return failed(req, message);
    }
    match req {
        Req::Load => Res::Snapshot(load(&client, &daemon).await),
        Req::Refresh => Res::Refreshed(client.refresh_suggestions().await.map_err(text)),
        Req::Dismiss {
            provider_id,
            id,
            dismissed,
            ..
        } => match client
            .set_suggestion_dismissed(provider_id, id, *dismissed)
            .await
        {
            Ok(()) => match load(&client, &daemon).await {
                Ok(snapshot) => Res::Snapshot(Ok(snapshot)),
                Err(e) => Res::DismissKept(e),
            },
            Err(e) => Res::Snapshot(Err(text(e))),
        },
        Req::Start {
            provider_id,
            id,
            review_fingerprint,
            ..
        } => {
            let mut body = serde_json::json!({ "provider_id": provider_id, "id": id });
            if let Some(fingerprint) = review_fingerprint {
                body["approve_hooks"] = true.into();
                body["review_fingerprint"] = fingerprint.clone();
            }
            Res::Started(
                client
                    .start_suggestion(&body)
                    .await
                    .map(|(status, body)| start_answer(status, &body))
                    .map_err(text),
            )
        }
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
/// bidi overrides, keeping newlines and tabs. A carriage return is dropped, so
/// CRLF text keeps its line breaks.
fn plain(s: &str) -> String {
    s.chars()
        .filter(|&c| c != '\r')
        .map(|c| {
            let hostile = (c.is_control() && c != '\n' && c != '\t')
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
            if hostile {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// `plain` for text shown on a single line: line breaks cannot start a second
/// one, and a tab becomes a space.
fn plain_line(s: &str) -> String {
    plain(s).replace('\n', "\u{fffd}").replace('\t', " ")
}

/// Resolves a prepared suggestion into a dialog `Start`. A prepare response
/// without a project (none mapped, or several) leaves the path for the user.
fn start_for(snapshot: Option<&Snapshot>, key: &ItemKey, prepared: Prepared) -> Start {
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
    Start {
        provider: plain_line(provider),
        source_ref: item.map(|i| plain_line(&i.source_ref)).unwrap_or_default(),
        reason: item.map(|i| plain_line(&i.reason)).unwrap_or_default(),
        source_text: item
            .filter(|i| !i.preview.trim().is_empty())
            .map(|i| plain(&i.preview)),
        path: prepared.project.map(|p| p.path),
        title: plain_line(&prepared.title),
        initial_prompt: plain(&prepared.initial_prompt),
        tool: prepared.tool,
        preset: Preset {
            structured: prepared.view.as_deref().map(|v| v == "structured"),
            worktree: prepared.worktree,
            sandbox: prepared.sandbox,
            yolo: prepared.yolo,
        },
        suggestion: Some(crate::session::SuggestionOrigin {
            provider_id: key.0.clone(),
            id: key.1.clone(),
            profile: None,
        }),
    }
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

/// How long a dismissed row stays listed so `z` can undo it.
const UNDO_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) enum SuggestionsAction {
    Continue,
    Close,
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
    /// The resolved prefill, shown before the second `e` confirms it.
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
    pub(super) failed: Option<(ItemKey, String)>,
    /// Strict mode keys: dismiss is `Z`, not `z`.
    pub(super) strict: bool,
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
            strict: false,
        }
    }

    /// Shows a start the daemon answered with a failure on its row; Enter tries again.
    pub(super) fn set_failed(&mut self, key: ItemKey, message: String) {
        self.notice = None;
        self.failed = Some((key, message));
    }

    pub(super) fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some(message.into());
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
            .filter(|i| store.is_listed(i, self.show_dismissed))
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
            KeyCode::Char('r') => {
                self.begin();
                if mutations_allowed {
                    store.refresh();
                }
                store.reload();
            }
            KeyCode::Char(c) if c == self.dismiss_key() && mutations_allowed => {
                if let Some(item) = self.item_at(store, self.selected) {
                    let (key, dismissed) =
                        ((item.provider_id.clone(), item.id.clone()), !item.dismissed);
                    self.begin();
                    store.dismiss(&key, dismissed);
                }
            }
            KeyCode::Char(c) if c == self.dismiss_key() => {
                self.error = block.map(|b| block_note(b).to_string());
            }
            KeyCode::Enter | KeyCode::Char('e' | 'N') => {
                let review = key.code != KeyCode::Enter;
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
                if !review {
                    self.begin();
                    self.failed = None;
                    self.notice = Some("Starting…".into());
                    store.start(key);
                    return SuggestionsAction::Continue;
                }
                self.prepare(key, another, store);
            }
            _ => {}
        }
        SuggestionsAction::Continue
    }

    fn dismiss_key(&self) -> char {
        if self.strict {
            'Z'
        } else {
            'z'
        }
    }

    /// Asks for `key`'s prefill; the dialog opens when it arrives.
    pub(super) fn prepare(&mut self, key: ItemKey, another: bool, store: &mut SuggestionsFeed) {
        self.begin();
        self.failed = None;
        self.preparing = Some(key.clone());
        self.prepare_another = another;
        store.prepare_in_view(key, self.epoch);
    }

    /// The prefill a prepare resolved, for the caller to open the dialog with.
    pub(super) fn take_prepared(&mut self) -> Option<Start> {
        self.prepared.take().map(|(_, start)| start)
    }

    /// Takes what a store poll holds for this view. True when state changed.
    pub(super) fn apply(&mut self, store: &SuggestionsFeed, polled: &mut FeedPoll) -> bool {
        let mut changed = false;
        if polled.changed {
            self.sync(store);
            changed = true;
        }
        if polled.refreshed {
            self.notice = Some("Refresh requested.".into());
            changed = true;
        }
        if let Some(error) = polled.error.take() {
            self.notice = None;
            match self.preparing.take() {
                Some(key) if polled.prepare_failed => self.failed = Some((key, error)),
                Some(key) => {
                    self.preparing = Some(key);
                    self.error = Some(error);
                }
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
        if let Some(profile) = &store.foreign_profile {
            footer.push(Line::from(Span::styled(
                plain(&format!(
                    "Suggestions are published under profile {profile}. Switch to it to see them."
                )),
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
            format!(
                "j/k select · Enter start/open · e start with changes · N new · {} dismiss/restore · d dismissed · r refresh · PgUp/PgDn prompt · Esc back",
                self.dismiss_key()
            ),
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
            let state = match (item.dismissed, store.is_recent(&item_key)) {
                (false, _) => "",
                (true, true) => " [dismissed, z to undo]",
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
                (None, false) => "No suggestions loaded. Press r to reload.",
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
                    "Provider draft prompt (e resolves the final prompt first):",
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

/// What the Home list shows for one suggestion; provider text is untrusted and
/// already stripped of control characters.
pub(super) struct Entry {
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
    /// The id of the live Session the item was started as.
    pub started_session: Option<String>,
    pub dismissed: bool,
}

pub(super) struct FeedPoll {
    pub changed: bool,
    /// The daemon accepted a refresh request.
    pub refreshed: bool,
    /// A prepared suggestion, with the key and the surface it was requested for.
    pub start: Option<(ItemKey, Start, PrepareTarget)>,
    /// What the daemon answered to a start, with the item it was for.
    pub started: Option<(ItemKey, StartAnswer)>,
    /// Failure of a user-initiated request; background reloads only log.
    pub error: Option<String>,
    /// `error` belongs to a prepare, not to a reload, refresh or dismiss.
    pub prepare_failed: bool,
}

/// The one suggestions store, owned by `HomeView`: snapshot, worker and
/// in-flight state behind both the Suggested group and the full view, which
/// keeps only its own UI state.
pub(super) struct SuggestionsFeed {
    snapshot: Option<Snapshot>,
    /// The profile the TUI shows; snapshots of another profile are not shown.
    profile: Option<String>,
    /// The other profile the daemon's suggestions belong to, while it differs.
    foreign_profile: Option<String>,
    prepare_key: Option<ItemKey>,
    start_key: Option<ItemKey>,
    /// The hooks review on screen: its item, the fingerprint it shows and the origin it was issued for.
    held_trust: Option<(ItemKey, serde_json::Value, Option<Origin>)>,
    prepare_target: PrepareTarget,
    /// Epoch of the most recently opened full view.
    view_epoch: u64,
    /// The in-flight request came from the user (prepare or dismiss), so its
    /// failure is worth showing.
    in_flight_user: bool,
    busy: bool,
    /// User requests that arrived while another request was in flight.
    queued: std::collections::VecDeque<Req>,
    /// The dismiss or restore request in flight, to clear only its own pending entry on failure.
    in_flight_dismiss: Option<(ItemKey, bool)>,
    /// Dismiss requests not yet answered by a snapshot showing them.
    dismissing: Vec<ItemKey>,
    /// Restore requests not yet answered by a snapshot showing them.
    restoring: Vec<ItemKey>,
    /// Items dismissed in the last [`UNDO_WINDOW`], kept listed so `z` undoes them.
    recent: Vec<(ItemKey, std::time::Instant)>,
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
            profile: None,
            foreign_profile: None,
            prepare_key: None,
            start_key: None,
            held_trust: None,
            prepare_target: PrepareTarget::Home,
            view_epoch: 0,
            in_flight_user: false,
            busy: false,
            queued: std::collections::VecDeque::new(),
            in_flight_dismiss: None,
            dismissing: Vec::new(),
            restoring: Vec::new(),
            recent: Vec::new(),
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
                ..
            } => {
                self.prepare_key = Some((provider_id.clone(), id.clone()));
                self.prepare_target = *target;
            }
            Req::Start {
                provider_id, id, ..
            } => self.start_key = Some((provider_id.clone(), id.clone())),
            Req::Dismiss {
                provider_id,
                id,
                dismissed,
                ..
            } => self.in_flight_dismiss = Some(((provider_id.clone(), id.clone()), *dismissed)),
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

    /// Where the list on screen came from; `None` before the first load.
    fn origin(&self) -> Option<Origin> {
        self.snapshot.as_ref().map(|s| Origin {
            daemon: s.daemon.clone(),
            profile: s.profile.clone(),
        })
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

    /// Shows an item as started by session `session_id` before the next
    /// snapshot confirms it, so returning to it offers Open, not a second Start.
    pub(super) fn link_started(&mut self, key: &ItemKey, session_id: &str) {
        if let Some(item) = self.snapshot.as_mut().and_then(|s| {
            s.items
                .iter_mut()
                .find(|i| (&i.provider_id, &i.id) == (&key.0, &key.1))
        }) {
            item.started_session = Some(Started {
                id: session_id.to_owned(),
                status: "Starting".into(),
                archived: false,
            });
        }
        self.reload();
    }

    /// Sets the profile the TUI shows, `None` while it shows every profile
    /// (nothing to compare). Any change drops the snapshot and a pending
    /// prepare and reloads at once.
    pub(super) fn set_profile(&mut self, profile: Option<String>) {
        let switched = self.profile != profile;
        let left_a_profile = self.profile.is_some();
        self.profile = profile;
        if switched {
            // A snapshot that names no profile cannot be told apart, so it stays.
            let foreign = self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.profile.is_some() && s.profile != self.profile);
            if foreign {
                self.snapshot = None;
            }
            self.foreign_profile = None;
            self.held_trust = None;
            self.loaded_at = None;
            self.load_attempt_at = None;
            // Adopting the first profile keeps a prepare in flight unless its snapshot was foreign.
            if left_a_profile || foreign {
                self.prepare_key = None;
            }
        }
    }

    pub(super) fn is_recent(&self, key: &ItemKey) -> bool {
        self.recent.iter().any(|(k, _)| k == key)
    }

    /// Moves each dismissal the snapshot now shows into the undo window, and
    /// drops each restore it now shows. Others wait for their own answer.
    fn settle_dismissals(&mut self) {
        let dismissing = std::mem::take(&mut self.dismissing);
        for key in dismissing {
            match self.item(&key).map(|i| i.dismissed) {
                Some(true) => {
                    self.recent.retain(|(k, _)| *k != key);
                    self.recent.push((key, std::time::Instant::now()));
                }
                Some(false) => self.dismissing.push(key),
                None => {}
            }
        }
        let restoring = std::mem::take(&mut self.restoring);
        for key in restoring {
            if self.item(&key).is_some_and(|i| i.dismissed) {
                self.restoring.push(key);
            } else {
                self.recent.retain(|(k, _)| *k != key);
            }
        }
    }

    /// Ends undo windows that ran out. True when one did.
    fn expire_recent(&mut self) -> bool {
        let before = self.recent.len();
        self.recent.retain(|(_, at)| at.elapsed() < UNDO_WINDOW);
        self.recent.len() != before
    }

    /// Whether the full view lists `item`: undismissed, dismissed within the
    /// undo window, or any when dismissed ones are shown. Stale items are listed.
    fn is_listed(&self, item: &Item, show_dismissed: bool) -> bool {
        show_dismissed
            || !item.dismissed
            || self.is_recent(&(item.provider_id.clone(), item.id.clone()))
    }

    /// How many suggestions the full view lists by default, stale ones included.
    pub(super) fn listed_count(&self) -> usize {
        self.snapshot
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| self.is_listed(i, false))
            .count()
    }

    /// `(provider_id, id)` of every suggestion of a listed provider that is
    /// open and undismissed, or dismissed within the undo window, in snapshot order.
    pub(super) fn keys(&self) -> Vec<ItemKey> {
        self.snapshot
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| {
                (!i.dismissed || self.is_recent(&(i.provider_id.clone(), i.id.clone())))
                    && !i.stale
                    && self.provider(&i.provider_id).is_some()
            })
            .map(|i| (i.provider_id.clone(), i.id.clone()))
            .collect()
    }

    /// Enabled providers that failed their last refresh, as `(name, error)`.
    pub(super) fn provider_failures(&self) -> Vec<(String, String)> {
        self.snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .filter(|p| p.enabled)
            .filter_map(|p| Some((plain(&p.name), plain(p.error.as_deref()?))))
            .collect()
    }

    pub(super) fn has_provider_failure(&self) -> bool {
        !self.provider_failures().is_empty()
    }

    /// Enabled provider names in snapshot order, for the group preview.
    pub(super) fn provider_names(&self) -> Vec<String> {
        self.snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .filter(|p| p.enabled)
            .map(|p| plain(&p.name))
            .collect()
    }

    pub(super) fn entry(&self, key: &ItemKey) -> Option<Entry> {
        let item = self.item(key)?;
        let provider = self.provider(&key.0)?;
        Some(Entry {
            source_ref: self.source_ref(item).map(plain).unwrap_or_default(),
            title: plain(&item.title),
            reason: plain(&item.reason),
            provider: plain(&provider.name),
            project: item.project.as_deref().map(plain),
            prompt: plain(&item.draft.prompt),
            provider_error: provider.error.as_deref().map(plain),
            source_url: crate::util::is_http_url(&item.source_url).then(|| item.source_url.clone()),
            started_session: item.started_session.as_ref().map(|s| s.id.clone()),
            dismissed: item.dismissed,
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

    /// True while a request is in flight: its answer is not yet processed.
    #[cfg(test)]
    pub(super) fn is_busy_for_test(&self) -> bool {
        self.busy
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
        let started = StartAnswer::Started {
            session_id: "s-new".into(),
            title: "Prepared title".into(),
        };
        Self::recording_start_for_test(snapshot, sink, started)
    }

    /// [`recording_for_test`](Self::recording_for_test) whose start answers `started`.
    #[cfg(test)]
    pub(super) fn recording_start_for_test(
        snapshot: serde_json::Value,
        sink: std::sync::mpsc::Sender<String>,
        started: StartAnswer,
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
                    ..
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
                        view: Some("terminal".into()),
                        worktree: true,
                        sandbox: false,
                        yolo: true,
                    }))
                }
                Req::Start {
                    provider_id,
                    id,
                    review_fingerprint,
                    ..
                } => {
                    let approved = review_fingerprint.map_or(String::new(), |f| format!(" {f}"));
                    let _ = sink.send(format!("start {provider_id}/{id}{approved}"));
                    Res::Started(Ok(started.clone()))
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
        let pending = if dismissed {
            &mut self.dismissing
        } else {
            &mut self.restoring
        };
        if !pending.contains(key) {
            pending.push(key.clone());
        }
        self.submit(Req::Dismiss {
            provider_id: key.0.clone(),
            id: key.1.clone(),
            dismissed,
            origin: self.origin(),
        });
    }

    /// Starts the item with its preset, behind the request in flight.
    pub(super) fn start(&mut self, key: ItemKey) {
        let origin = self.origin();
        self.submit_start(key, None, origin);
    }

    /// Remembers the hooks review shown for `key`, bound to the list on screen.
    pub(super) fn hold_trust(&mut self, key: ItemKey, fingerprint: serde_json::Value) {
        self.held_trust = Some((key, fingerprint, self.origin()));
    }

    pub(super) fn has_held_trust(&self) -> bool {
        self.held_trust.is_some()
    }

    /// Starts again with the hooks the user approved, to the daemon and profile the review came from.
    pub(super) fn approve_held_trust(&mut self) {
        if let Some((key, fingerprint, origin)) = self.held_trust.take() {
            self.submit_start(key, Some(fingerprint), origin);
        }
    }

    fn submit_start(
        &mut self,
        key: ItemKey,
        review_fingerprint: Option<serde_json::Value>,
        origin: Option<Origin>,
    ) {
        let pending = (self.busy && self.start_key.as_ref() == Some(&key))
            || self.queued.iter().any(|req| {
                matches!(req, Req::Start { provider_id, id, .. }
                    if (provider_id, id) == (&key.0, &key.1))
            });
        // A second Enter while the first answer is pending must not start twice.
        if pending {
            return;
        }
        self.submit(Req::Start {
            provider_id: key.0,
            id: key.1,
            review_fingerprint,
            origin,
        });
    }

    pub(super) fn prepare(&mut self, key: ItemKey) {
        self.submit_prepare(key, PrepareTarget::Home);
    }

    pub(super) fn prepare_another(&mut self, key: ItemKey) {
        self.submit_prepare(key, PrepareTarget::HomeAnother);
    }

    fn prepare_in_view(&mut self, key: ItemKey, epoch: u64) {
        self.submit_prepare(key, PrepareTarget::View(epoch));
    }

    /// Whether a prepare for this item and surface is in flight or queued.
    fn prepare_pending(&self, key: &ItemKey, target: PrepareTarget) -> bool {
        let in_flight =
            self.busy && self.prepare_key.as_ref() == Some(key) && self.prepare_target == target;
        in_flight
            || self.queued.iter().any(|req| {
                matches!(req, Req::Prepare { provider_id, id, target: t, .. }
                    if (provider_id, id) == (&key.0, &key.1) && *t == target)
            })
    }

    fn submit_prepare(&mut self, key: ItemKey, target: PrepareTarget) {
        // A second Enter while the first answer is pending must not queue a second dialog.
        if self.prepare_pending(&key, target) {
            return;
        }
        let origin = self.origin();
        self.submit(Req::Prepare {
            provider_id: key.0,
            id: key.1,
            target,
            origin,
        });
    }

    pub(super) fn poll(&mut self) -> FeedPoll {
        let mut out = FeedPoll {
            changed: self.expire_recent(),
            refreshed: false,
            start: None,
            started: None,
            error: None,
            prepare_failed: false,
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
        let in_flight_dismiss = self.in_flight_dismiss.take();
        let req_kind = match &res {
            Res::Snapshot(_) => ResKind::Snapshot,
            Res::Prepared(_) => ResKind::Prepare,
            Res::Started(_) => ResKind::Start,
            _ => ResKind::Other,
        };
        match res {
            Res::Snapshot(Ok(snapshot))
                if self
                    .profile
                    .as_deref()
                    .zip(snapshot.profile.as_deref())
                    .is_some_and(|(shown, published)| shown != published) =>
            {
                // The daemon's store belongs to another profile: showing,
                // dismissing or preparing from it would act on the wrong one.
                self.foreign_profile = snapshot.profile;
                self.snapshot = None;
                self.loaded_at = Some(std::time::Instant::now());
                out.changed = true;
            }
            Res::Snapshot(Ok(snapshot)) => {
                self.foreign_profile = None;
                let before = self.origin();
                self.snapshot = Some(snapshot);
                if before.is_some() && before != self.origin() {
                    // Work queued for the list that was replaced must not reach the new one.
                    let now = self.origin();
                    self.queued
                        .retain(|req| req.origin().is_none_or(|o| Some(o) == now.as_ref()));
                    self.dismissing.clear();
                    self.restoring.clear();
                    self.held_trust = None;
                }
                self.settle_dismissals();
                self.loaded_at = Some(std::time::Instant::now());
                out.changed = true;
            }
            Res::Prepared(Ok(prepared)) => {
                if let Some(key) = self.prepare_key.take() {
                    let start = start_for(self.snapshot.as_ref(), &key, prepared);
                    out.start = Some((key, start, self.prepare_target));
                } else {
                    // A profile switch dropped the request this answers.
                    out.error = Some("Profile changed, suggestion not started".into());
                    out.prepare_failed = true;
                }
            }
            Res::Started(Ok(answer)) => match self.start_key.take() {
                Some(key) => out.started = Some((key, answer)),
                None => out.error = Some("Profile changed, suggestion not started".into()),
            },
            Res::Snapshot(Err(e))
            | Res::Refreshed(Err(e))
            | Res::Prepared(Err(e))
            | Res::Started(Err(e)) => {
                if matches!(req_kind, ResKind::Prepare) {
                    self.prepare_key = None;
                    out.prepare_failed = true;
                }
                if matches!(req_kind, ResKind::Start) {
                    self.start_key = None;
                }
                if let (ResKind::Snapshot, Some((key, dismissed))) = (req_kind, in_flight_dismiss) {
                    // A failed dismiss or restore leaves the undo window as it was.
                    let pending = if dismissed {
                        &mut self.dismissing
                    } else {
                        &mut self.restoring
                    };
                    pending.retain(|k| *k != key);
                }
                if user {
                    out.error = Some(e);
                } else {
                    tracing::debug!(target: "tui.suggestions", error = %e, "suggestions reload failed");
                }
            }
            Res::DismissKept(e) => {
                // The daemon accepted it: show it from the snapshot we have and
                // open the undo window, even though the reload failed.
                if let (Some((key, dismissed)), Some(snapshot)) =
                    (&in_flight_dismiss, self.snapshot.as_mut())
                {
                    if let Some(item) = snapshot
                        .items
                        .iter_mut()
                        .find(|i| (&i.provider_id, &i.id) == (&key.0, &key.1))
                    {
                        item.dismissed = *dismissed;
                    }
                    self.settle_dismissals();
                    out.changed = true;
                }
                if user {
                    out.error = Some(format!("Saved, but the list did not reload: {e}"));
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
            daemon: None,
            profile: None,
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
            view: None,
            worktree: false,
            sandbox: false,
            yolo: false,
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
            daemon: None,
            profile: None,
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

        let start = start_for(Some(&snapshot), &("p".into(), "a".into()), prep);
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
            daemon: None,
            profile: None,
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
        press(&mut rig, KeyCode::Char('z'), false);
        assert!(!rig.store.busy);
        assert!(rig.view.error.is_some());
    }

    #[test]
    fn e_resolves_the_final_prompt_for_the_dialog() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        assert!(matches!(
            press(&mut rig, KeyCode::Char('e'), true),
            SuggestionsAction::Continue
        ));
        assert!(poll_until_settled(&mut rig));
        let start = rig.view.take_prepared().expect("the prefill is ready");
        assert_eq!(start.path.as_deref(), Some("/repo"));
        assert_eq!(start.initial_prompt, "final prompt");
        assert!(rig.view.prepared.is_none());
    }

    #[test]
    fn prepare_without_a_registered_project_still_starts_with_the_path_left_to_pick() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(None))));
        let mut rig = view_with(vec![item("a", None, false)], worker);
        press(&mut rig, KeyCode::Char('e'), true);
        assert!(poll_until_settled(&mut rig));
        let start = rig.view.take_prepared().expect("the prefill is ready");
        assert_eq!(start.path, None);
        assert_eq!(start.initial_prompt, "final prompt");
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
        let start = rig.view.take_prepared().expect("the prefill is ready");
        assert!(start.suggestion.is_none());
    }

    #[test]
    fn a_plain_start_carries_the_suggestion_reference() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        press(&mut rig, KeyCode::Char('e'), true);
        assert!(poll_until_settled(&mut rig));
        let start = rig.view.take_prepared().expect("the prefill is ready");
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
            daemon: None,
            profile: None,
            providers: vec![],
            items: vec![item("a", None, dismissed)],
            prefs: Prefs::default(),
        });
    }

    fn changed() -> FeedPoll {
        FeedPoll {
            changed: true,
            refreshed: false,
            start: None,
            started: None,
            error: None,
            prepare_failed: false,
        }
    }

    #[test]
    fn a_dismissed_item_stays_dimmed_for_ten_seconds_and_z_undoes_it() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        let key = ("p".to_string(), "a".to_string());
        rig.store.dismissing.push(key.clone());
        dismissed_snapshot(&mut rig, true);
        rig.store.settle_dismissals();
        assert!(rig.view.apply(&rig.store, &mut changed()));
        assert_eq!(ids(&rig), ["a"], "kept while the undo window is open");
        assert!(render_text(&rig).contains("[dismissed, z to undo]"));

        let stale = std::time::Instant::now()
            .checked_sub(UNDO_WINDOW + std::time::Duration::from_secs(1))
            .expect("monotonic clock far enough from boot");
        rig.store.recent[0].1 = stale;
        assert!(rig.store.expire_recent());
        assert!(rig.view.apply(&rig.store, &mut changed()));
        assert!(
            ids(&rig).is_empty(),
            "then only the dismissed list shows it"
        );
        press(&mut rig, KeyCode::Char('d'), true);
        assert_eq!(ids(&rig), ["a"]);
    }

    #[test]
    fn overlapping_dismissals_each_keep_their_undo_window() {
        let mut rig = view_with(
            vec![item("a", None, false), item("b", None, false)],
            idle_worker(),
        );
        let key = |id: &str| ("p".to_string(), id.to_string());
        rig.store.dismissing = vec![key("a"), key("b")];
        let snapshot = |a: bool, b: bool| Snapshot {
            daemon: None,
            profile: None,
            providers: vec![],
            items: vec![item("a", None, a), item("b", None, b)],
            prefs: Prefs::default(),
        };
        // The first answer shows only A; B still waits for its own.
        rig.store.snapshot = Some(snapshot(true, false));
        rig.store.settle_dismissals();
        assert!(rig.store.is_recent(&key("a")));
        assert_eq!(rig.store.dismissing, [key("b")]);
        rig.store.snapshot = Some(snapshot(true, true));
        rig.store.settle_dismissals();
        assert!(rig.store.is_recent(&key("a")) && rig.store.is_recent(&key("b")));
        assert!(rig.store.dismissing.is_empty());
    }

    #[test]
    fn a_failed_restore_keeps_the_undo_window() {
        let worker = Worker::seeded_for_test("t", Res::Snapshot(Err("boom".into())));
        let mut rig = view_with(vec![item("a", None, true)], worker);
        let key = ("p".to_string(), "a".to_string());
        rig.store
            .recent
            .push((key.clone(), std::time::Instant::now()));
        rig.view.sync(&rig.store);
        rig.store.restoring.push(key.clone());
        rig.store.busy = true;
        rig.store.in_flight_user = true;
        rig.store.in_flight_dismiss = Some((key.clone(), false));
        let mut failed = FeedPoll {
            changed: false,
            refreshed: false,
            start: None,
            started: None,
            error: None,
            prepare_failed: false,
        };
        for _ in 0..200 {
            failed = rig.store.poll();
            if failed.error.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert_eq!(
            rig.store.recent.len(),
            1,
            "an API hiccup does not lose the undo"
        );
        assert!(rig.store.restoring.is_empty());
        assert_eq!(ids(&rig), ["a"]);

        rig.store.restoring.push(key);
        dismissed_snapshot(&mut rig, false);
        rig.store.settle_dismissals();
        assert!(
            rig.store.recent.is_empty(),
            "dropped once the restore succeeded"
        );
    }

    #[test]
    fn an_accepted_dismiss_enters_the_undo_window_even_if_the_reload_fails() {
        let worker = Worker::seeded_for_test("t", Res::DismissKept("offline".into()));
        let mut rig = view_with(vec![item("a", None, false)], worker);
        let key = ("p".to_string(), "a".to_string());
        rig.store.dismissing.push(key.clone());
        rig.store.busy = true;
        rig.store.in_flight_user = true;
        rig.store.in_flight_dismiss = Some((key.clone(), true));
        let mut polled = rig.store.poll();
        for _ in 0..200 {
            if polled.error.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            polled = rig.store.poll();
        }
        assert!(polled.changed);
        assert!(polled.error.unwrap().contains("did not reload: offline"));
        assert!(rig.store.is_recent(&key), "the undo is offered");
        assert!(rig.store.dismissing.is_empty());
        rig.view.sync(&rig.store);
        assert_eq!(ids(&rig), ["a"], "listed while the undo window runs");
    }

    #[test]
    fn start_answers_follow_the_daemons_typed_outcomes() {
        use serde_json::json;
        let hooks = json!({
            "error": "hooks_need_trust",
            "commands": { "on_create": ["make setup"], "on_launch": [], "on_destroy": [] },
            "needs_mcp_trust": true,
            "review_fingerprint": { "project_path": "/repo", "hooks": "abc" },
        });
        let cases = [
            (
                201,
                json!({ "session_id": "s1", "title": "T" }),
                StartAnswer::Started {
                    session_id: "s1".into(),
                    title: "T".into(),
                },
            ),
            (
                409,
                json!({ "error": "already_started", "session_id": "s2" }),
                StartAnswer::AlreadyStarted(Some("s2".into())),
            ),
            (
                409,
                json!({ "error": "already_started", "session_id": null }),
                StartAnswer::AlreadyStarted(None),
            ),
            (
                409,
                hooks,
                StartAnswer::NeedsTrust {
                    changed: false,
                    commands: vec!["on create: make setup".into(), "project MCP servers".into()],
                    fingerprint: json!({ "project_path": "/repo", "hooks": "abc" }),
                },
            ),
            (
                409,
                json!({ "error": "trust_changed", "commands": {}, "review_fingerprint": { "n": 2 } }),
                StartAnswer::NeedsTrust {
                    changed: true,
                    commands: vec![],
                    fingerprint: json!({ "n": 2 }),
                },
            ),
            (
                400,
                json!({ "error": "no_project" }),
                StartAnswer::NoProject,
            ),
            (
                422,
                json!({ "error": "start_failed", "reason": "Docker isn't running", "hint": "Start it." }),
                StartAnswer::Failed("Docker isn't running Start it.".into()),
            ),
            (
                400,
                json!({ "error": "stale" }),
                StartAnswer::Failed("That suggestion is no longer open. Reload with r.".into()),
            ),
        ];
        for (status, body, expected) in cases {
            assert_eq!(start_answer(status, &body), expected, "{body}");
        }
    }

    #[test]
    fn provider_text_keeps_tabs_and_crlf_line_breaks() {
        assert_eq!(plain("fn a() {\r\n\tx\r\n}"), "fn a() {\n\tx\n}");
        assert_eq!(plain_line("a\tb\nc"), "a b\u{fffd}c");
        assert_eq!(plain("a\u{1b}b"), "a\u{fffd}b");
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
    fn a_failed_prepare_stays_on_its_row() {
        let worker =
            Worker::seeded_for_test("t", Res::Prepared(Err("branch already exists".into())));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        rig.store.in_flight_user = true;
        press(&mut rig, KeyCode::Char('e'), true);
        assert!(poll_until_settled(&mut rig));
        assert!(
            rig.view.error.is_none(),
            "the failure is on the row, not the footer"
        );
        assert!(render_text(&rig).contains("Couldn't start: branch already exists"));
        assert!(rig.view.failed.is_some());
    }

    #[test]
    fn a_reload_failure_during_a_queued_prepare_stays_off_the_row() {
        let worker = Worker::spawn("t", |req| match req {
            Req::Load => Res::Snapshot(Err("reload boom".into())),
            Req::Prepare { .. } => Res::Prepared(Ok(prepared(Some("/repo")))),
            _ => unreachable!("no other request expected"),
        });
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        rig.store.reload();
        press(&mut rig, KeyCode::Char('e'), true);
        let mut error_seen = None;
        for _ in 0..200 {
            let mut polled = rig.store.poll();
            rig.view.apply(&rig.store, &mut polled);
            error_seen = error_seen.or_else(|| rig.view.error.clone());
            if rig.view.prepared.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(error_seen.as_deref(), Some("reload boom"));
        assert!(
            rig.view.failed.is_none(),
            "the reload error is not the row's"
        );
        assert!(rig.view.prepared.is_some(), "the prepare still completes");
    }

    #[test]
    fn a_prepare_answered_after_a_profile_switch_reports_instead_of_vanishing() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        rig.store.profile = Some("default".into());
        press(&mut rig, KeyCode::Enter, true);
        rig.store.set_profile(Some("work".into()));
        assert!(poll_until_settled(&mut rig));
        assert!(rig.view.prepared.is_none());
        assert!(rig.view.preparing.is_none());
        assert!(render_text(&rig).contains("Profile changed, suggestion not started"));
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
    fn the_group_preview_names_only_enabled_providers() {
        let mut rig = view_with(vec![], idle_worker());
        let snapshot = rig.store.snapshot.as_mut().unwrap();
        let mut off = snapshot.providers[0].clone();
        off.id = "q".into();
        off.name = "Linear".into();
        off.enabled = false;
        snapshot.providers.push(off);
        assert_eq!(rig.store.provider_names(), ["GitHub"]);
        rig.store.snapshot.as_mut().unwrap().providers[0].enabled = false;
        assert!(rig.store.provider_names().is_empty());
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
    fn the_listed_count_matches_the_rows_the_full_view_shows() {
        let mut stale = item("old", None, false);
        stale.stale = true;
        let dismissed = item("gone", None, true);
        let rig = view_with(
            vec![item("fresh", None, false), stale, dismissed],
            idle_worker(),
        );
        assert_eq!(rig.store.keys().len(), 1, "Home starts only the fresh one");
        assert_eq!(rig.view.order.len(), 2, "the view lists the stale one too");
        assert_eq!(rig.store.listed_count(), rig.view.order.len());
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
        assert_eq!(rig.view.notice.as_deref(), Some("Refresh requested."));
    }

    #[test]
    fn a_dead_worker_clears_busy_and_reports_an_error() {
        let worker = Worker::spawn("test-suggestions", |_: Req| -> Res {
            panic!("worker died")
        });
        let mut rig = view_with(vec![item("a", None, false)], worker);
        rig.store.reload();
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
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
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
        let key = ("p".to_string(), "a".to_string());
        assert_eq!(
            store.keys(),
            std::slice::from_ref(&key),
            "listed while the undo window is open"
        );
        store.recent[0].1 = std::time::Instant::now()
            .checked_sub(UNDO_WINDOW + std::time::Duration::from_secs(1))
            .expect("monotonic clock far enough from boot");
        assert!(store.expire_recent());
        assert!(store.keys().is_empty());
        assert!(requests.try_recv().is_err(), "no reload was sent");
    }

    #[test]
    fn a_view_prepare_answered_after_the_view_closed_is_left_for_the_caller_to_drop() {
        let worker = Worker::seeded_for_test("t", Res::Prepared(Ok(prepared(Some("/repo")))));
        let mut rig = view_with(vec![item("a", Some("repo"), false)], worker);
        press(&mut rig, KeyCode::Char('e'), true);
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
        press(&mut rig, KeyCode::Char('e'), true);
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
        press(&mut rig, KeyCode::Char('e'), true);
        assert_eq!(rig.store.queued.len(), 1, "the prepare waits its turn");

        rig.store.reload();
        assert_eq!(rig.store.queued.len(), 2, "the reload waits its turn");
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

    #[test]
    fn a_created_session_links_its_item_at_once_and_reloads() {
        let (sink, requests) = std::sync::mpsc::channel();
        let mut store = SuggestionsFeed::recording_for_test(
            serde_json::json!({
                "providers": [{ "id": "p", "name": "GitHub" }],
                "items": [{
                    "provider_id": "p", "id": "a", "title": "A", "reason": "r",
                    "source_url": "https://example.test/a", "source_ref": "#a",
                    "preview": "", "draft": { "prompt": "x" }
                }],
            }),
            sink,
        );
        store.set_snapshot_for_test(serde_json::json!({
            "providers": [{ "id": "p", "name": "GitHub" }],
            "items": [{
                "provider_id": "p", "id": "a", "title": "A", "reason": "r",
                "source_url": "https://example.test/a", "source_ref": "#a",
                "preview": "", "draft": { "prompt": "x" }
            }],
        }));
        let key = ("p".to_string(), "a".to_string());
        assert_eq!(store.entry(&key).unwrap().started_session, None);
        store.link_started(&key, "sess-1");
        assert_eq!(
            store.entry(&key).unwrap().started_session.as_deref(),
            Some("sess-1")
        );
        assert_eq!(
            requests
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "load"
        );
    }

    #[test]
    fn a_snapshot_of_another_profile_is_not_shown_and_says_so() {
        let published = |profile: &str| Snapshot {
            daemon: None,
            profile: Some(profile.into()),
            providers: vec![],
            items: vec![item("a", None, false)],
            prefs: Prefs::default(),
        };
        let worker = Worker::seeded_for_test("t", Res::Snapshot(Ok(published("default"))));
        let mut rig = view_with(vec![], worker);
        rig.store.snapshot = None;
        rig.store.set_profile(Some("work".into()));
        rig.store.busy = true;
        let mut polled = rig.store.poll();
        for _ in 0..200 {
            if polled.changed {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            polled = rig.store.poll();
        }
        assert!(
            rig.store.snapshot.is_none(),
            "the other profile's items are not kept"
        );
        assert!(render_text(&rig).contains("published under profile default"));

        // The same profile is shown.
        rig.store.busy = false;
        rig.store.set_profile(Some("default".into()));
        assert!(rig.store.foreign_profile.is_none());
    }

    #[test]
    fn any_profile_change_drops_the_snapshot_and_a_pending_prepare() {
        let mut rig = view_with(
            vec![item("a", None, false)],
            Worker::seeded_for_test("t", Res::Snapshot(Err("x".into()))),
        );
        rig.store.prepare_key = Some(("t".into(), "a".into()));
        rig.store.snapshot.as_mut().unwrap().profile = Some("default".into());
        rig.store.profile = Some("default".into());
        rig.store.set_profile(Some("work".into()));
        assert!(rig.store.snapshot.is_none());
        assert!(rig.store.prepare_key.is_none());
    }

    #[test]
    fn a_foreign_labelled_snapshot_cancels_a_prepare_on_first_profile_adoption() {
        let mut rig = view_with(
            vec![item("a", None, false)],
            Worker::seeded_for_test("t", Res::Snapshot(Err("x".into()))),
        );
        rig.store.snapshot.as_mut().unwrap().profile = Some("default".into());
        rig.store.prepare_key = Some(("t".into(), "a".into()));
        rig.store.set_profile(Some("work".into()));
        assert!(rig.store.prepare_key.is_none());
    }
    #[test]
    fn a_queued_dismiss_is_dropped_when_the_list_reloads_from_another_origin() {
        let (sink, seen) = std::sync::mpsc::channel();
        let worker = Worker::spawn("t", move |req| match req {
            Req::Load => {
                let _ = sink.send("load".to_string());
                Res::Snapshot(Ok(Snapshot {
                    daemon: Some("http://b".into()),
                    profile: Some("B".into()),
                    providers: Vec::new(),
                    items: vec![item("a", None, false)],
                    prefs: Prefs::default(),
                }))
            }
            _ => {
                let _ = sink.send("mutation".to_string());
                Res::Refreshed(Ok(()))
            }
        });
        let mut rig = view_with(vec![item("a", None, false)], worker);
        let snapshot = rig.store.snapshot.as_mut().unwrap();
        snapshot.daemon = Some("http://a".into());
        snapshot.profile = Some("A".into());
        rig.store.reload();
        press(&mut rig, KeyCode::Char('z'), true);
        assert!(poll_until_settled(&mut rig));
        assert!(rig.store.queued.is_empty());
        assert!(rig.store.dismissing.is_empty());
        assert!(!rig.store.is_busy_for_test());
        assert_eq!(seen.try_iter().collect::<Vec<_>>(), ["load"]);
    }

    #[test]
    fn a_request_is_refused_by_another_daemon_or_profile_but_not_by_its_own() {
        let origin = Origin {
            daemon: Some("http://a".into()),
            profile: Some("A".into()),
        };
        assert!(origin_error(&origin, "http://a", Some("A")).is_none());
        assert!(origin_error(&origin, "http://b", Some("A")).is_some());
        assert!(origin_error(&origin, "http://a", Some("B")).is_some());
        let unbound = Origin {
            daemon: None,
            profile: None,
        };
        assert!(origin_error(&unbound, "http://b", Some("B")).is_none());
    }
    #[test]
    fn r_reloads_on_a_read_only_dashboard_and_refreshes_first_otherwise() {
        let (sink, seen) = std::sync::mpsc::channel();
        let worker = Worker::spawn("t", move |req| match req {
            Req::Load => {
                let _ = sink.send("load");
                Res::Snapshot(Err("boom".into()))
            }
            Req::Refresh => {
                let _ = sink.send("refresh");
                Res::Refreshed(Ok(()))
            }
            _ => unreachable!("no other request expected"),
        });
        let mut rig = view_with(vec![item("a", None, false)], worker);
        press(&mut rig, KeyCode::Char('r'), false);
        assert!(poll_until_settled(&mut rig));
        assert_eq!(seen.try_iter().collect::<Vec<_>>(), ["load"]);

        press(&mut rig, KeyCode::Char('r'), true);
        for _ in 0..2 {
            assert!(poll_until_settled(&mut rig));
        }
        assert_eq!(seen.try_iter().collect::<Vec<_>>(), ["refresh", "load"]);
    }

    #[test]
    fn x_and_l_no_longer_do_anything() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        press(&mut rig, KeyCode::Char('x'), true);
        press(&mut rig, KeyCode::Char('l'), true);
        assert!(!rig.store.busy);
    }
}
