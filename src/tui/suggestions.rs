//! Suggested sessions browser. Items come from the daemon's suggestions API;
//! Enter starts one with its kind's preset through `start`, and `e` opens the
//! regular new-session dialog prefilled from `prepare`. Provider text is
//! untrusted: it is shown as plain text, control characters stripped, and the
//! prompt Enter will send is visible before it.

use std::sync::mpsc::TryRecvError;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

#[cfg(test)]
use crate::daemon::suggestions::{Draft, Prefs, PreparedProject, ResolvedProject};
use crate::daemon::suggestions::{Item, Prepared, Provider, Snapshot, Started, WirePreset};

#[cfg(not(test))]
use crate::acp::client::locate_daemon;
#[cfg(not(test))]
use crate::daemon::DaemonClient;
use crate::tui::components::text::{
    fixed_width, rendered_width, truncate_to_width, wrapped_line_count,
};
use crate::tui::session_feed::MutationBlock;
use crate::tui::styles::Theme;
use crate::tui::worker::Worker;

impl Item {
    /// `(key, name)` of the full view's group: the repo, named by its last
    /// segment, else the resolved Project.
    fn group(&self) -> Option<(&str, &str)> {
        if let Some(repo) = self.mapping_key.as_deref() {
            return Some((repo, repo.rsplit('/').next().unwrap_or(repo)));
        }
        self.resolved_project
            .as_ref()
            .map(|p| (p.path.as_str(), p.name.as_str()))
    }
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
        commands: Vec<String>,
        /// One-line summaries of the repository's MCP servers.
        mcp_summaries: Vec<String>,
        fingerprint: serde_json::Value,
    },
    /// The hooks changed after the review: ask again for a fresh one.
    TrustChanged,
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
        (409, "trust_changed") => StartAnswer::TrustChanged,
        (409, "hooks_need_trust") => {
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
            let mcp_summaries: Vec<String> = body["mcp_summaries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m.as_str())
                .map(plain_line)
                .collect();
            if mcp_summaries.is_empty() && body["needs_mcp_trust"].as_bool() == Some(true) {
                commands.push("project MCP servers".to_owned());
            }
            StartAnswer::NeedsTrust {
                commands,
                mcp_summaries,
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
pub(super) struct Origin {
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
    /// Turns on `pre_trust_agent_folders` through the daemon's settings API.
    PreTrust {
        origin: Option<Origin>,
    },
}

impl Req {
    fn origin(&self) -> Option<&Origin> {
        match self {
            Req::Dismiss { origin, .. }
            | Req::Prepare { origin, .. }
            | Req::Start { origin, .. }
            | Req::PreTrust { origin } => origin.as_ref(),
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
    PreTrusted(Result<(), String>),
}

#[cfg(not(test))]
impl Res {
    fn error(&self) -> Option<&str> {
        match self {
            Res::Snapshot(Err(e))
            | Res::DismissKept(e)
            | Res::Refreshed(Err(e))
            | Res::Prepared(Err(e))
            | Res::Started(Err(e))
            | Res::PreTrusted(Err(e)) => Some(e),
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
        Req::PreTrust { .. } => Res::PreTrusted(Err(message)),
        Req::Load | Req::Dismiss { .. } => Res::Snapshot(Err(message)),
    }
}

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[cfg(not(test))]
async fn load(client: &DaemonClient, daemon: &str) -> Result<Snapshot, String> {
    let mut snapshot = client.suggestions().await.map_err(text)?;
    snapshot.daemon = Some(daemon.to_owned());
    snapshot.pre_trust = client.settings().await.ok().and_then(|settings| {
        settings
            .get("session")?
            .get("pre_trust_agent_folders")?
            .as_bool()
    });
    Ok(snapshot)
}

/// Refuses a request bound to a daemon or profile other than the one answering.
#[cfg(not(test))]
async fn check_bound(
    client: &DaemonClient,
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
    let client = match located.daemon_client().map_err(text) {
        Ok(client) => client,
        Err(message) => return failed(req, message),
    };
    if let Err(message) = check_bound(&client, &daemon, req.origin()).await {
        return failed(req, message);
    }
    match req {
        Req::Load => Res::Snapshot(load(&client, &daemon).await),
        Req::Refresh => Res::Refreshed(client.refresh_suggestions().await.map_err(text)),
        Req::PreTrust { .. } => Res::PreTrusted(
            client
                .patch_settings(
                    &serde_json::json!({ "session": { "pre_trust_agent_folders": true } }),
                )
                .await
                .map_err(|e| format!("Couldn't turn on pre-trust: {e}")),
        ),
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
            client
                .prepare_suggestion(provider_id, id)
                .await
                .map_err(text),
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

/// The note for a daemon that refuses mutations.
pub(super) fn block_note(block: MutationBlock) -> &'static str {
    match block {
        MutationBlock::ReadOnly => "Read-only daemon: you can browse, not start or dismiss.",
        MutationBlock::Offline => {
            "Disconnected. Suggestions show from the last fetch; reconnect to start one."
        }
    }
}

const STALE_NOTE: &str = "No longer open at the source. Dismiss it, or wait for the next refresh.";

/// A working provider's status for the divider: when it last updated.
fn provider_status(provider: &Provider, any_items: bool) -> String {
    let name = &provider.name;
    if provider.refreshing {
        format!("⠙ {name} · refreshing…")
    } else if provider.updated_at.is_none() {
        format!("{name} · waiting for first results")
    } else if !any_items {
        format!("{name} · nothing new")
    } else {
        let ago = provider.updated_at.map_or_else(String::new, |at| {
            format!(
                " {}",
                ago_label(crate::time::Timestamp::now().as_unix_secs() - at)
            )
        });
        format!("{name} · updated{ago}")
    }
}

/// Key hints in rows of `width`, never splitting a key from its label.
fn key_rows(units: &[String], width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for unit in units {
        match rows.last_mut() {
            Some(row) if rendered_width(row) + 3 + rendered_width(unit) <= width => {
                row.push_str("   ");
                row.push_str(unit);
            }
            _ => rows.push(unit.clone()),
        }
    }
    rows
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

/// `initial_prompt_error_code` values from the daemon.
const FOLDER_NOT_TRUSTED: &str = "agent_folder_not_trusted";
const BYPASS_NOT_ACCEPTED: &str = "agent_bypass_not_accepted";

/// How long a dismissed row stays listed so `z` can undo it.
const UNDO_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) enum SuggestionsAction {
    Continue,
    Close,
    /// Jump to the Session an item was started as.
    OpenSession(String),
    /// Ask before turning on `pre_trust_agent_folders`.
    TurnOnPreTrust,
    /// Trash the item's Session, whose prompt was blocked, then start it again.
    TryAgain(ItemKey, String),
}

type ItemKey = (String, String);

pub(super) struct SuggestionsView {
    /// Listed items in display order (core interleaves providers), rebuilt
    /// when the store's snapshot or the dismissed filter changes.
    order: Vec<ItemKey>,
    pub(super) error: Option<String>,
    notice: Option<String>,
    /// The resolved prefill, for the caller to open the dialog with.
    prepared: Option<Start>,
    epoch: u64,
    selected: usize,
    detail_scroll: u16,
    show_dismissed: bool,
    /// The `?` key list replaces the list and detail.
    help: bool,
    /// The item a `prepare` is in flight for.
    preparing: Option<ItemKey>,
    /// The in-flight `prepare` is for "Start another", which does not claim the link.
    prepare_another: bool,
    /// Strict mode keys follow Home's rule: a bare letter moves to Shift, Shift to Ctrl.
    pub(super) strict: bool,
    /// `pre_trust_agent_folders` is on, so a Session blocked on folder trust can start again.
    pub(super) pretrusted: bool,
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
            help: false,
            preparing: None,
            prepare_another: false,
            strict: false,
            pretrusted: false,
        }
    }

    /// Shows a start the daemon answered with a failure on its row; Enter tries again.
    pub(super) fn set_failed(
        &mut self,
        store: &mut SuggestionsFeed,
        key: ItemKey,
        message: String,
    ) {
        self.notice = None;
        store.failed = Some((key, message));
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

    /// The group an item sits under; empty when it names no repo and resolved to no Project.
    fn group_key<'a>(&self, store: &'a SuggestionsFeed, index: usize) -> &'a str {
        self.item_at(store, index)
            .and_then(Item::group)
            .map_or("", |(key, _)| key)
    }

    /// Headings show only once some item has a group.
    fn has_groups(&self, store: &SuggestionsFeed) -> bool {
        (0..self.order.len()).any(|i| !self.group_key(store, i).is_empty())
    }

    /// Rebuilds `order`, grouped by repo in first-seen order with ungrouped
    /// last (snapshot order holds within a group), keeping the selection on
    /// `key` when it is still listed.
    fn reorder(&mut self, store: &SuggestionsFeed, key: Option<ItemKey>) {
        let listed: Vec<&Item> = store
            .snapshot
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| store.is_listed(i, self.show_dismissed))
            .collect();
        let mut seen: Vec<&str> = Vec::new();
        for (group, _) in listed.iter().filter_map(|i| i.group()) {
            if !seen.contains(&group) {
                seen.push(group);
            }
        }
        let mut ranked: Vec<(usize, &Item)> = listed
            .into_iter()
            .map(|i| {
                let rank = i
                    .group()
                    .and_then(|(g, _)| seen.iter().position(|s| *s == g))
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
        if self.help {
            self.help = false;
            return SuggestionsAction::Continue;
        }
        let Some(code) = self.unstrict(key) else {
            return SuggestionsAction::Continue;
        };
        match code {
            KeyCode::Char('?') => self.help = true,
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
            KeyCode::Char('t') => {
                let untrusted = self.item_at(store, self.selected).is_some_and(|i| {
                    store
                        .blocked_prompt(i)
                        .is_some_and(|(_, code)| code.as_deref() == Some(FOLDER_NOT_TRUSTED))
                });
                if untrusted && !self.pretrusted {
                    match block {
                        Some(block) => self.error = Some(block_note(block).to_string()),
                        None => return SuggestionsAction::TurnOnPreTrust,
                    }
                }
            }
            KeyCode::Char('z') if mutations_allowed => {
                if let Some(item) = self.item_at(store, self.selected) {
                    let (key, dismissed) =
                        ((item.provider_id.clone(), item.id.clone()), !item.dismissed);
                    self.begin();
                    store.dismiss(&key, dismissed);
                }
            }
            KeyCode::Char('z') => {
                self.error = block.map(|b| block_note(b).to_string());
            }
            KeyCode::Enter | KeyCode::Char('e' | 'N') => {
                let review = code != KeyCode::Enter;
                let another = code == KeyCode::Char('N');
                let Some(key) = self.selected_key() else {
                    return SuggestionsAction::Continue;
                };
                let item = self.item_at(store, self.selected);
                if let Some(started) = item
                    .and_then(|i| i.started_session.as_ref())
                    .filter(|_| !another)
                {
                    let untrusted = item
                        .and_then(|i| store.blocked_prompt(i))
                        .is_some_and(|(_, code)| code.as_deref() == Some(FOLDER_NOT_TRUSTED));
                    if untrusted && self.pretrusted && started.one_click() && block.is_none() {
                        return SuggestionsAction::TryAgain(key, started.id.clone());
                    }
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

    /// The non-strict key a key press stands for, or `None` for a bare letter
    /// strict mode reserves. Navigation keys and `q` do not move.
    fn unstrict(&self, key: KeyEvent) -> Option<KeyCode> {
        if !self.strict {
            return Some(key.code);
        }
        match key.code {
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(KeyCode::Char('N'))
            }
            KeyCode::Char(c @ ('E' | 'D' | 'R' | 'T' | 'Z')) => {
                Some(KeyCode::Char(c.to_ascii_lowercase()))
            }
            KeyCode::Char('e' | 'd' | 'r' | 't' | 'z' | 'N') => None,
            code => Some(code),
        }
    }

    /// How a key reads in the current mode: `z` shows as `Z`, `N` as `Ctrl+N` in strict mode.
    fn key_label(&self, c: char) -> String {
        match (self.strict, c) {
            (false, c) => c.to_string(),
            (true, 'N') => "Ctrl+N".to_string(),
            (true, c) => c.to_ascii_uppercase().to_string(),
        }
    }

    /// Asks for `key`'s prefill; the dialog opens when it arrives.
    pub(super) fn prepare(&mut self, key: ItemKey, another: bool, store: &mut SuggestionsFeed) {
        self.begin();
        store.failed = None;
        self.preparing = Some(key.clone());
        self.prepare_another = another;
        store.prepare_in_view(key, self.epoch);
    }

    /// The prefill a prepare resolved, for the caller to open the dialog with.
    pub(super) fn take_prepared(&mut self) -> Option<Start> {
        self.prepared.take()
    }

    /// Takes what a store poll holds for this view. True when state changed.
    pub(super) fn apply(&mut self, store: &mut SuggestionsFeed, polled: &mut FeedPoll) -> bool {
        let mut changed = false;
        if polled.changed {
            self.sync(store);
            if let Some(on) = store.daemon_pre_trust() {
                self.pretrusted = on;
            }
            changed = true;
        }
        if polled.refreshed {
            self.notice = Some("Refresh requested.".into());
            changed = true;
        }
        if polled.pretrusted {
            self.pretrusted = true;
            self.notice =
                Some("Pre-trust is on: new Sessions start with their folder trusted.".into());
            changed = true;
        }
        if let Some(error) = polled.error.take() {
            self.notice = None;
            match self.preparing.take() {
                Some(key) if polled.prepare_failed => store.failed = Some((key, error)),
                Some(key) => {
                    self.preparing = Some(key);
                    self.error = Some(error);
                }
                None => self.error = Some(error),
            }
            changed = true;
        }
        if matches!(polled.start, Some((_, _, PrepareTarget::View(e))) if e == self.epoch) {
            if let Some((_, mut start, _)) = polled.start.take() {
                self.preparing = None;
                if self.prepare_another {
                    start.suggestion = None;
                }
                self.prepared = Some(start);
                changed = true;
            }
        }
        changed
    }

    /// The keys that apply to the selected row; `?` lists them all.
    fn row_keys(&self, store: &SuggestionsFeed, block: Option<MutationBlock>) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some(item) = self.item_at(store, self.selected) {
            let key = (item.provider_id.clone(), item.id.clone());
            if let Some(started) = &item.started_session {
                let untrusted = store
                    .blocked_prompt(item)
                    .is_some_and(|(_, code)| code.as_deref() == Some(FOLDER_NOT_TRUSTED));
                let retry = untrusted && self.pretrusted && started.one_click() && block.is_none();
                keys.push(if retry { "↵ try again" } else { "↵ open" }.to_string());
            } else if block.is_none() && !item.stale {
                let retry = store.failed.as_ref().is_some_and(|(k, _)| *k == key);
                keys.push(if retry { "↵ try again" } else { "↵ start" }.to_string());
                keys.push(format!("{} change first", self.key_label('e')));
            }
            if block.is_none() {
                if !item.stale {
                    keys.push(format!("{} new", self.key_label('N')));
                }
                let verb = if item.dismissed { "restore" } else { "dismiss" };
                keys.push(format!("{} {verb}", self.key_label('z')));
            }
        }
        if block.is_some() {
            keys.push(format!("{} reload", self.key_label('r')));
        }
        keys.push("? all keys".to_string());
        keys.push("Esc back".to_string());
        keys
    }

    /// Every key of the view, for the `?` overlay.
    fn help_lines(&self) -> Vec<(String, &'static str)> {
        let dismissed = if self.show_dismissed {
            "hide dismissed"
        } else {
            "show dismissed"
        };
        vec![
            ("j/k".to_string(), "select"),
            (
                "↵".to_string(),
                "start with the preset, or open a started one",
            ),
            (
                self.key_label('e'),
                "change first: the New session dialog, prefilled",
            ),
            (
                self.key_label('N'),
                "a new session for the item, without its link",
            ),
            (self.key_label('z'), "dismiss or restore"),
            (self.key_label('d'), dismissed),
            (
                self.key_label('t'),
                "turn on pre-trust for a Session blocked on it",
            ),
            (self.key_label('r'), "refresh the providers, then reload"),
            ("PgUp/PgDn".to_string(), "scroll the detail"),
            ("Ctrl+K".to_string(), "open in the provider"),
            ("Esc".to_string(), "back"),
        ]
    }

    /// Lines above the list: why the list may be stale or partial.
    fn list_notices(
        &self,
        store: &SuggestionsFeed,
        block: Option<MutationBlock>,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(blocked) = block {
            lines.push(Line::from(Span::styled(
                block_note(blocked),
                Style::default().fg(theme.waiting),
            )));
        }
        if let Some(profile) = &store.foreign_profile {
            lines.push(Line::from(Span::styled(
                plain(&format!(
                    "Suggestions are published under profile {profile}. Switch to it to see them."
                )),
                Style::default().fg(theme.waiting),
            )));
        }
        let failures = store.provider_failures();
        for (name, error, fix) in &failures {
            lines.push(Line::from(Span::styled(
                format!("{name} couldn't refresh: {error}"),
                Style::default().fg(theme.error),
            )));
            if let Some(fix) = fix {
                lines.push(Line::from(format!("  {fix}")));
            }
        }
        if !failures.is_empty() {
            let shown = if self.order.is_empty() {
                ""
            } else {
                "Shown from the last refresh. "
            };
            lines.push(Line::from(Span::styled(
                format!("{shown}{} retries.", self.key_label('r')),
                Style::default().fg(theme.dimmed),
            )));
        }
        for provider in store
            .snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .filter(|p| !p.enabled)
        {
            lines.push(Line::from(Span::styled(
                plain(&format!(
                    "{0} suggestions are hidden because {0} is off in Settings › Suggestions.",
                    provider.name
                )),
                Style::default().fg(theme.dimmed),
            )));
        }
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines
    }

    /// The divider's text: each working provider's status.
    fn divider_status(store: &SuggestionsFeed) -> String {
        let items = store.snapshot.iter().flat_map(|s| &s.items);
        let statuses: Vec<String> = store
            .snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .filter(|p| p.enabled && p.error.is_none())
            .map(|p| {
                let any = items.clone().any(|i| i.provider_id == p.id);
                plain_line(&provider_status(p, any))
            })
            .collect();
        statuses.join(" · ")
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
        let bordered = block.inner(area);
        frame.render_widget(block, area);
        // A one-cell margin inside the border, above and at the sides.
        let inner = Rect {
            x: bordered.x + 1,
            y: bordered.y + 1,
            width: bordered.width.saturating_sub(2),
            height: bordered.height.saturating_sub(1),
        };
        let width = inner.width.max(1);
        let dim = Style::default().fg(theme.dimmed);

        let detail = self.detail_lines(store, theme);
        let mut units = if self.help {
            vec!["any key closes".to_string()]
        } else {
            self.row_keys(store, mutation_block)
        };
        let mut footer: Vec<Line> = Vec::new();
        if let Some(error) = &self.error {
            footer.push(Line::from(Span::styled(
                plain(error),
                Style::default().fg(theme.error),
            )));
        }
        if let Some(notice) = &self.notice {
            footer.push(Line::from(Span::styled(
                plain(notice),
                Style::default().fg(theme.hint),
            )));
        }
        let footer_rows = |footer: &[Line], units: &[String]| -> u16 {
            let messages: usize = footer.iter().map(|l| wrapped_line_count(l, width)).sum();
            (messages + key_rows(units, width as usize).len() + 1) as u16
        };
        let split = |units: &[String]| -> (Rect, Rect, Rect, Rect) {
            let footer_height = footer_rows(&footer, units).min(inner.height.saturating_sub(1));
            let [body, footer_area] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)])
                    .areas(inner);
            let [list_area, detail_area] =
                Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(body);
            (body, footer_area, list_area, detail_area)
        };
        let (mut body, mut footer_area, mut list_area, mut detail_area) = split(&units);
        let detail_rows: usize = detail.iter().map(|l| wrapped_line_count(l, width)).sum();
        let visible = detail_area.height.saturating_sub(1) as usize;
        if !self.help && detail_rows > visible + self.detail_scroll as usize {
            units.insert(units.len() - 2, "PgDn more".to_string());
            (body, footer_area, list_area, detail_area) = split(&units);
        }

        if self.help {
            let key_width = self
                .help_lines()
                .iter()
                .map(|(k, _)| rendered_width(k))
                .max()
                .unwrap_or(0);
            let mut lines = vec![Line::from(Span::styled(
                "Keys",
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            lines.extend(self.help_lines().into_iter().map(|(key, what)| {
                Line::from(vec![
                    Span::styled(
                        format!("  {}", fixed_width(&key, key_width)),
                        Style::default().fg(theme.accent),
                    ),
                    Span::raw(format!("   {what}")),
                ])
            }));
            frame.render_widget(Paragraph::new(lines), body);
        } else {
            self.render_list(frame, list_area, theme, store, mutation_block);
            // The divider joins the frame's border, with each provider's status on it.
            let status = Self::divider_status(store);
            let status = if status.is_empty() {
                String::new()
            } else {
                format!(
                    " {} ",
                    truncate_to_width(&status, area.width.saturating_sub(5) as usize)
                )
            };
            let rule = (area.width as usize).saturating_sub(3 + rendered_width(&status));
            let border = Style::default().fg(theme.border);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("├─", border),
                    Span::styled(status, dim),
                    Span::styled(format!("{}┤", "─".repeat(rule)), border),
                ])),
                Rect {
                    x: area.x,
                    width: area.width,
                    height: 1,
                    ..detail_area
                },
            );
            frame.render_widget(
                Paragraph::new(detail)
                    .wrap(Wrap { trim: false })
                    .scroll((self.detail_scroll, 0)),
                Rect {
                    y: detail_area.y + 1,
                    height: detail_area.height.saturating_sub(1),
                    ..detail_area
                },
            );
        }
        footer.insert(0, Line::from(""));
        footer.extend(
            key_rows(&units, width as usize)
                .into_iter()
                .map(|row| Line::from(Span::styled(row, dim))),
        );
        frame.render_widget(
            Paragraph::new(footer).wrap(Wrap { trim: true }),
            footer_area,
        );
    }

    fn render_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        store: &SuggestionsFeed,
        mutation_block: Option<MutationBlock>,
    ) {
        let width = area.width as usize;
        let dim = Style::default().fg(theme.dimmed);
        let mut lines = self.list_notices(store, mutation_block, theme);
        let headed = self.has_groups(store);
        let mut selected_rows = (0, 0);
        for index in 0..self.order.len() {
            let Some(item) = self.item_at(store, index) else {
                continue;
            };
            let item_key = (item.provider_id.clone(), item.id.clone());
            let first = lines.len();
            let group = self.group_key(store, index);
            if headed && (index == 0 || group != self.group_key(store, index - 1)) {
                let name = item.group().map_or("No project", |(_, name)| name);
                let repo = item
                    .mapping_key
                    .as_deref()
                    .filter(|repo| *repo != name)
                    .map(|repo| format!("  {repo}"))
                    .unwrap_or_default();
                let count = (0..self.order.len())
                    .filter(|&i| self.group_key(store, i) == group)
                    .count()
                    .to_string();
                let label_width = width.saturating_sub(count.len() + 1);
                let name = format!("▾ {}", plain_line(name));
                let repo = plain_line(&repo);
                let repo_width = label_width.saturating_sub(rendered_width(&name));
                lines.push(Line::from(vec![
                    Span::styled(
                        truncate_to_width(&name, label_width),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(fixed_width(&repo, repo_width), dim),
                    Span::styled(format!(" {count}"), dim),
                ]));
            }
            let current = index == self.selected;
            let marker = if current { "  › " } else { "    " };
            let state = match (item.dismissed, store.is_recent(&item_key)) {
                (false, _) => String::new(),
                (true, true) => format!(" [dismissed, {} to undo]", self.key_label('z')),
                (true, false) => " [dismissed]".to_string(),
            };
            let title_style = if item.dismissed || item.stale {
                dim
            } else if current {
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            // The heading names the repo, so "owner/repo#12" shows as "#12".
            let source_ref = store
                .source_ref(item)
                .map(|r| {
                    let short = item.mapping_key.as_deref().and_then(|k| r.strip_prefix(k));
                    plain_line(short.filter(|r| r.starts_with('#')).unwrap_or(r))
                })
                .unwrap_or_default();
            let ref_width = rendered_width(&source_ref);
            let title_width = width.saturating_sub(ref_width + usize::from(ref_width > 0));
            lines.push(Line::from(vec![
                Span::styled(
                    fixed_width(
                        &plain_line(&format!("{marker}{}{state}", item.title)),
                        title_width,
                    ),
                    title_style,
                ),
                Span::styled(
                    if ref_width > 0 {
                        format!(" {source_ref}")
                    } else {
                        String::new()
                    },
                    dim,
                ),
            ]));
            lines.push(self.subtitle(store, item, headed, theme));
            selected_rows = if current {
                (first, lines.len())
            } else {
                selected_rows
            };
            lines.push(Line::from(""));
        }
        if self.order.is_empty() {
            let message = match (&store.snapshot, store.busy) {
                (None, true) => "Loading suggestions…".to_string(),
                (None, false) => {
                    format!(
                        "No suggestions loaded. Press {} to reload.",
                        self.key_label('r')
                    )
                }
                (Some(_), _) => "Nothing to suggest right now.".to_string(),
            };
            lines.push(Line::from(Span::styled(message, dim)));
            if store.snapshot.is_some() {
                let status = Self::divider_status(store);
                let checked = if status.is_empty() {
                    String::new()
                } else {
                    format!("{status}. ")
                };
                lines.push(Line::from(Span::styled(
                    format!("{checked}{} checks again.", self.key_label('r')),
                    dim,
                )));
            }
        }
        let height = area.height as usize;
        let mut offset = selected_rows.1.saturating_sub(height);
        offset = offset.min(selected_rows.0);
        frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), area);
    }

    /// The line under an item's title: why it cannot go on, else its reason
    /// and Session.
    fn subtitle(
        &self,
        store: &SuggestionsFeed,
        item: &Item,
        headed: bool,
        theme: &Theme,
    ) -> Line<'static> {
        let error = Style::default().fg(theme.error);
        let item_key = (item.provider_id.clone(), item.id.clone());
        if let Some((_, message)) = store.failed.as_ref().filter(|(key, _)| *key == item_key) {
            return Line::from(Span::styled(
                plain_line(&format!(
                    "      Couldn't start: {message} · Enter tries again"
                )),
                error,
            ));
        }
        if let Some((_, code)) = store.blocked_prompt(item) {
            let one_click = item
                .started_session
                .as_ref()
                .is_some_and(Started::one_click);
            let next = match code.as_deref() {
                Some(FOLDER_NOT_TRUSTED) if self.pretrusted && one_click => {
                    "Enter tries again".to_string()
                }
                Some(FOLDER_NOT_TRUSTED) if self.pretrusted => {
                    "trust the folder in the Session · Enter opens it".to_string()
                }
                Some(FOLDER_NOT_TRUSTED) => format!("{} turn on pre-trust", self.key_label('t')),
                Some(BYPASS_NOT_ACCEPTED) => {
                    "accept Bypass Permissions once in the Session · Enter opens it".to_string()
                }
                _ => "Enter opens it".to_string(),
            };
            return Line::from(Span::styled(
                format!("      Prompt not typed · {next}"),
                error,
            ));
        }
        let mut parts = vec![item.reason.clone()];
        // A heading already names the Project.
        if !(headed && item.group().is_some()) {
            parts.push(
                item.resolved_project
                    .as_ref()
                    .map(|p| p.name.clone())
                    .or_else(|| item.project.clone())
                    .unwrap_or_else(|| "no project".to_string()),
            );
        }
        if let Some(s) = &item.started_session {
            parts.push(if s.archived {
                "Archived: Enter opens".to_string()
            } else {
                format!("Session {}: Enter opens", s.status.to_lowercase())
            });
        }
        Line::from(Span::styled(
            plain_line(&format!("      {}", parts.join(" · "))),
            Style::default().fg(theme.dimmed),
        ))
    }

    /// The detail under the divider: what stops the item, what Enter starts,
    /// the prompt, then the source text. The title is on the selected row.
    fn detail_lines(&self, store: &SuggestionsFeed, theme: &Theme) -> Vec<Line<'static>> {
        let Some(item) = self.item_at(store, self.selected) else {
            return Vec::new();
        };
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut detail: Vec<Line> = vec![Line::from("")];
        if let Some((_, message)) = store
            .failed
            .as_ref()
            .filter(|(key, _)| (&key.0, &key.1) == (&item.provider_id, &item.id))
        {
            detail.push(Line::from(Span::styled(
                plain(&format!("Couldn't start: {message} · Enter tries again")),
                Style::default().fg(theme.error),
            )));
        }
        if let Some((message, _)) = store.blocked_prompt(item) {
            detail.push(Line::from(Span::styled(
                plain(&format!("Prompt not typed: {message}")),
                Style::default().fg(theme.error),
            )));
        }
        if let Some(line) = store.preset_line(item) {
            detail.push(Line::from(Span::styled(
                plain(&line),
                Style::default().fg(theme.accent),
            )));
        }
        if detail.len() > 1 {
            detail.push(Line::from(""));
        }
        detail.push(Line::from(Span::styled(
            "Prompt (untrusted source text)",
            bold,
        )));
        detail.extend(
            plain(&store.effective_prompt(item))
                .lines()
                .map(|l| Line::from(format!("  {l}"))),
        );
        let preview = plain(&item.preview);
        if !preview.trim().is_empty() {
            detail.push(Line::from(""));
            detail.push(Line::from(Span::styled("Source text", bold)));
            detail.extend(preview.lines().map(|l| Line::from(format!("  {l}"))));
        }
        detail
    }

    #[cfg(test)]
    pub(super) fn idle_for_test() -> Self {
        Self::empty()
    }
}

/// How often the Home list reloads the suggestions snapshot.
const FEED_RELOAD: std::time::Duration = std::time::Duration::from_secs(60);
/// How often the feed reloads while a provider shows "refreshing…".
const REFRESHING_RELOAD: std::time::Duration = std::time::Duration::from_secs(2);

/// What the Home list shows for one suggestion; provider text is untrusted and
/// already stripped of control characters.
pub(super) struct Entry {
    pub source_ref: String,
    pub title: String,
    pub reason: String,
    pub provider: String,
    pub project: Option<String>,
    pub prompt: String,
    /// "Starts with Claude Code · Structured · worktree"; `None` before the daemon reports it.
    pub preset: Option<String>,
    pub provider_error: Option<String>,
    /// The item's `source_url` when it is an http(s) address; core validates
    /// it, this is a second check before anything is opened.
    pub source_url: Option<String>,
    /// The id of the live Session the item was started as.
    pub started_session: Option<String>,
    pub dismissed: bool,
    /// Why the last start failed, until the next try.
    pub failed: Option<String>,
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
    /// The daemon turned on pre-trust.
    pub pretrusted: bool,
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
    /// The last start or prepare that failed, shown on its row on Home and in
    /// the full view until the next try.
    pub(super) failed: Option<(ItemKey, String)>,
    /// Why a Session's first prompt was not typed, by Session id: the
    /// daemon's plain message and its `initial_prompt_error_code`, if any.
    pub(super) blocked_prompts: std::collections::HashMap<String, (String, Option<String>)>,
    /// The next hooks review follows a `trust_changed` answer.
    hooks_changed: bool,
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
            failed: None,
            blocked_prompts: std::collections::HashMap::new(),
            hooks_changed: false,
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
    pub(super) fn origin(&self) -> Option<Origin> {
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
                started_via: None,
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

    /// Whether Home shows the Suggested group: the daemon's "Show in the sidebar" pref.
    pub(super) fn shown_on_home(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|s| s.prefs.sidebar)
    }

    /// `(provider_id, id)` of every suggestion of a listed provider that is
    /// open, not started and undismissed, or dismissed within the undo window,
    /// in snapshot order. A started one is listed as its Session.
    pub(super) fn keys(&self) -> Vec<ItemKey> {
        self.snapshot
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| {
                (!i.dismissed || self.is_recent(&(i.provider_id.clone(), i.id.clone())))
                    && !i.stale
                    && i.started_session.is_none()
                    && self.provider(&i.provider_id).is_some()
            })
            .map(|i| (i.provider_id.clone(), i.id.clone()))
            .collect()
    }

    /// Enabled providers that failed their last refresh, as `(name, error,
    /// fix)`; `fix` is the advice of the first failing setup check that has one.
    pub(super) fn provider_failures(&self) -> Vec<(String, String, Option<String>)> {
        self.snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .filter(|p| p.enabled)
            .filter_map(|p| {
                let fix = p
                    .checks
                    .iter()
                    .find(|c| c.status == "fail" && !c.fix.trim().is_empty())
                    .map(|c| plain_line(&c.fix));
                Some((plain(&p.name), plain(p.error.as_deref()?), fix))
            })
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

    fn source_preset(&self, item: &Item) -> Option<&WirePreset> {
        self.provider(&item.provider_id)?
            .sources
            .iter()
            .find(|s| s.criterion == item.criterion)?
            .preset
            .as_ref()
    }

    /// The prompt Enter sends: the source's template expanded with the item's
    /// variables, or the plugin's draft when the preset has no template.
    fn effective_prompt(&self, item: &Item) -> String {
        self.source_preset(item)
            .and_then(|p| p.prompt.as_deref())
            .and_then(|t| crate::plugin::suggestions::expand(t, &item.variables).ok())
            .unwrap_or_else(|| item.draft.prompt.clone())
    }

    /// "Starts with Claude Code · Structured · worktree", from what Start will create.
    fn preset_line(&self, item: &Item) -> Option<String> {
        let start = item.start.as_ref()?;
        let agent =
            crate::acp::agent_registry::display_name_for(&start.agent).unwrap_or(&start.agent);
        let mut parts = vec![
            plain_line(agent),
            if start.view == "structured" {
                "Structured"
            } else {
                "Terminal"
            }
            .to_owned(),
        ];
        let yolo = self.source_preset(item).is_some_and(|p| p.yolo);
        for (on, label) in [
            (start.worktree, "worktree"),
            (start.sandbox, "sandbox"),
            (yolo, "yolo"),
        ] {
            if on {
                parts.push(label.to_owned());
            }
        }
        Some(format!("Starts with {}", parts.join(" · ")))
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
            prompt: plain(&self.effective_prompt(item)),
            preset: self.preset_line(item),
            provider_error: provider.error.as_deref().map(plain),
            source_url: crate::util::is_http_url(&item.source_url).then(|| item.source_url.clone()),
            started_session: item.started_session.as_ref().map(|s| s.id.clone()),
            dismissed: item.dismissed,
            failed: self
                .failed
                .as_ref()
                .filter(|(k, _)| k == key)
                .map(|(_, m)| plain(m)),
        })
    }

    /// Reloads when the snapshot is stale and nothing is in flight, sooner
    /// while a provider refreshes. Never while `connected` is false, and a
    /// failed load waits the same interval.
    pub(super) fn tick(&mut self, connected: bool) {
        let refreshing = self
            .snapshot
            .iter()
            .flat_map(|s| &s.providers)
            .any(|p| p.refreshing);
        let every = if refreshing {
            REFRESHING_RELOAD
        } else {
            FEED_RELOAD
        };
        let due = |at: Option<std::time::Instant>| at.is_none_or(|at| at.elapsed() >= every);
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
                Req::PreTrust { .. } => {
                    let _ = sink.send("pre-trust".into());
                    Res::PreTrusted(Ok(()))
                }
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
        self.start_from(key, origin);
    }

    /// Starts `key` bound to `origin`, which the worker refuses if the daemon
    /// or profile changed since.
    pub(super) fn start_from(&mut self, key: ItemKey, origin: Option<Origin>) {
        self.failed = None;
        self.submit_start(key, None, origin);
    }

    /// The daemon's `pre_trust_agent_folders` as of the last load, when it answered.
    pub(super) fn daemon_pre_trust(&self) -> Option<bool> {
        self.snapshot.as_ref()?.pre_trust
    }

    /// Asks the daemon to turn on pre-trust, bound to the list on screen.
    pub(super) fn turn_on_pre_trust(&mut self) {
        let origin = self.origin();
        self.submit(Req::PreTrust { origin });
    }

    /// Remembers the hooks review shown for `key`, bound to the list on screen.
    pub(super) fn hold_trust(&mut self, key: ItemKey, fingerprint: serde_json::Value) {
        self.held_trust = Some((key, fingerprint, self.origin()));
    }

    /// Asks for a fresh review after the daemon found the hooks changed.
    pub(super) fn restart_after_trust_change(&mut self, key: ItemKey) {
        self.hooks_changed = true;
        self.start(key);
    }

    pub(super) fn take_hooks_changed(&mut self) -> bool {
        std::mem::take(&mut self.hooks_changed)
    }

    /// Why the Session `item` was started as did not get its prompt.
    fn blocked_prompt(&self, item: &Item) -> Option<&(String, Option<String>)> {
        self.blocked_prompts.get(&item.started_session.as_ref()?.id)
    }

    pub(super) fn drop_held_trust(&mut self) {
        self.held_trust = None;
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
            pretrusted: false,
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
            | Res::Started(Err(e))
            | Res::PreTrusted(Err(e)) => {
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
            Res::PreTrusted(Ok(())) => out.pretrusted = true,
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
            mapping_key: None,
            criterion: String::new(),
            variables: Default::default(),
            start: None,
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
            pre_trust: None,
            profile: None,
            providers: vec![Provider {
                id: "p".into(),
                name: "GitHub".into(),
                error: None,
                enabled: true,
                updated_at: Some(crate::time::Timestamp::now().as_unix_secs() - 120),
                refreshing: false,
                sources: vec![],
                checks: vec![],
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
        render_sized(rig, block, 100, 24)
    }

    fn render_sized(rig: &Rig, block: Option<MutationBlock>, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
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
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The rendered row holding `needle`, trimmed of the border.
    fn row<'a>(text: &'a str, needle: &str) -> &'a str {
        text.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} in {text}"))
            .trim_matches(|c: char| c == '│' || c.is_whitespace())
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
            if rig.view.apply(&mut rig.store, &mut polled) {
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
            pre_trust: None,
            profile: None,
            providers: vec![Provider {
                id: "p".into(),
                name: hostile("GitHub"),
                error: None,
                enabled: true,
                updated_at: None,
                refreshing: false,
                sources: vec![],
                checks: vec![],
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
            pre_trust: None,
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
            started_via: None,
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
        assert!(row(&render_text(&rig), "▾ web").ends_with(" 2"));
        for _ in 0..3 {
            press(&mut rig, KeyCode::Char('j'), true);
        }
        let text = render_text(&rig);
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {text}"))
        };
        assert!(
            at("▾ api") < at("▾ No project"),
            "the window scrolled to the last group"
        );
        let flat = view_with(vec![item("x", None, false)], idle_worker());
        assert!(
            !render_text(&flat).contains("No project"),
            "no headings without a resolved project"
        );
    }

    #[test]
    fn an_unlinked_repo_gets_its_own_group_named_after_the_repo() {
        let in_repo = |mut item: Item, key: &str| {
            item.source_ref = format!("{key}#38");
            item.mapping_key = Some(key.into());
            item
        };
        let rig = view_with(
            vec![
                item("loose", None, false),
                in_repo(item("f1", None, false), "acme/freebuf"),
                in_repo(in_project(item("w1", None, false), "web"), "acme/web"),
            ],
            idle_worker(),
        );
        assert_eq!(ids(&rig), ["f1", "w1", "loose"]);
        let text = render_text(&rig);
        assert!(row(&text, "▾ freebuf  acme/freebuf").ends_with(" 1"));
        assert!(row(&text, "▾ web  acme/web").ends_with(" 1"));
        assert!(row(&text, "▾ No project").ends_with(" 1"));
        let f1 = row(&text, "› f1");
        assert!(
            f1.ends_with(" #38") && !f1.contains("acme/"),
            "the heading names the repo: {text}"
        );
    }

    fn blocked_rig(code: &str) -> Rig {
        let mut started = item("a", None, false);
        started.started_session = Some(Started {
            id: "s1".into(),
            status: "Idle".into(),
            archived: false,
            started_via: Some("start".into()),
        });
        let mut rig = view_with(vec![started], idle_worker());
        rig.store.blocked_prompts.insert(
            "s1".into(),
            (
                "Claude Code is asking whether to trust this folder.".into(),
                Some(code.into()),
            ),
        );
        rig
    }

    #[test]
    fn an_untrusted_folder_offers_pre_trust_then_try_again() {
        let mut rig = blocked_rig("agent_folder_not_trusted");
        let text = render_text(&rig);
        assert!(text.contains("Prompt not typed"), "{text}");
        assert!(text.contains("t turn on pre-trust"), "{text}");
        assert!(
            text.contains("asking whether to trust this folder"),
            "{text}"
        );
        assert!(matches!(
            press(&mut rig, KeyCode::Char('t'), false),
            SuggestionsAction::Continue
        ));
        assert!(matches!(
            press(&mut rig, KeyCode::Char('t'), true),
            SuggestionsAction::TurnOnPreTrust
        ));

        rig.view.pretrusted = true;
        assert!(render_text(&rig).contains("Enter tries again"));
        let key = ("p".to_string(), "a".to_string());
        assert!(matches!(
            press(&mut rig, KeyCode::Enter, true),
            SuggestionsAction::TryAgain(k, s) if k == key && s == "s1"
        ));
    }

    #[test]
    fn a_session_not_made_by_one_click_start_is_opened_not_retried() {
        let mut rig = blocked_rig("agent_folder_not_trusted");
        rig.view.pretrusted = true;
        for via in [None, Some("wizard")] {
            rig.store.snapshot.as_mut().unwrap().items[0]
                .started_session
                .as_mut()
                .unwrap()
                .started_via = via.map(Into::into);
            let text = render_text(&rig);
            assert!(text.contains("trust the folder in the Session"), "{text}");
            assert!(!text.contains("Enter tries again"), "{text}");
            assert!(matches!(
                press(&mut rig, KeyCode::Enter, true),
                SuggestionsAction::OpenSession(s) if s == "s1"
            ));
        }
    }

    #[test]
    fn a_bypass_acceptance_says_to_open_the_session() {
        let mut rig = blocked_rig("agent_bypass_not_accepted");
        let text = render_text(&rig);
        assert!(text.contains("accept Bypass Permissions"), "{text}");
        assert!(!text.contains("pre-trust"), "{text}");
        assert!(matches!(
            press(&mut rig, KeyCode::Enter, true),
            SuggestionsAction::OpenSession(s) if s == "s1"
        ));
    }

    fn dismissed_snapshot(rig: &mut Rig, dismissed: bool) {
        rig.store.snapshot = Some(Snapshot {
            daemon: None,
            pre_trust: None,
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
            pretrusted: false,
        }
    }

    #[test]
    fn a_dismissed_item_stays_dimmed_for_ten_seconds_and_z_undoes_it() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        let key = ("p".to_string(), "a".to_string());
        rig.store.dismissing.push(key.clone());
        dismissed_snapshot(&mut rig, true);
        rig.store.settle_dismissals();
        assert!(rig.view.apply(&mut rig.store, &mut changed()));
        assert_eq!(ids(&rig), ["a"], "kept while the undo window is open");
        assert!(render_text(&rig).contains("[dismissed, z to undo]"));

        let stale = std::time::Instant::now()
            .checked_sub(UNDO_WINDOW + std::time::Duration::from_secs(1))
            .expect("monotonic clock far enough from boot");
        rig.store.recent[0].1 = stale;
        assert!(rig.store.expire_recent());
        assert!(rig.view.apply(&mut rig.store, &mut changed()));
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
            pre_trust: None,
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
            pretrusted: false,
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
            "mcp_summaries": ["github: npx server"],
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
                    commands: vec!["on create: make setup".into()],
                    mcp_summaries: vec!["github: npx server".into()],
                    fingerprint: json!({ "project_path": "/repo", "hooks": "abc" }),
                },
            ),
            (
                409,
                json!({ "error": "trust_changed" }),
                StartAnswer::TrustChanged,
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
        assert!(rig.store.failed.is_some());
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
            rig.view.apply(&mut rig.store, &mut polled);
            error_seen = error_seen.or_else(|| rig.view.error.clone());
            if rig.view.prepared.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(error_seen.as_deref(), Some("reload boom"));
        assert!(
            rig.store.failed.is_none(),
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
                "Read-only daemon: you can browse, not start or dismiss.",
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
    fn each_provider_gets_a_status_on_the_divider_and_a_failure_leads_the_list() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        let text = render_text(&rig);
        assert!(text.contains("├─ GitHub · updated 2 min ago ─"), "{text}");
        let provider = &mut rig.store.snapshot.as_mut().unwrap().providers[0];
        provider.refreshing = true;
        assert!(render_text(&rig).contains("⠙ GitHub · refreshing…"));
        let provider = &mut rig.store.snapshot.as_mut().unwrap().providers[0];
        provider.refreshing = false;
        provider.error = Some("API key rejected".into());
        let text = render_text(&rig);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[2].contains("GitHub couldn't refresh: API key rejected"),
            "{text}"
        );
        assert!(
            lines[3].contains("Shown from the last refresh. r retries."),
            "{text}"
        );
        rig.store.snapshot.as_mut().unwrap().providers[0].error = None;
        rig.store.snapshot.as_mut().unwrap().items.clear();
        assert!(render_text(&rig).contains("GitHub · nothing new"));
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
    fn the_heading_prefers_the_resolved_project_over_the_hint() {
        let resolved = in_project(item("a", Some("hint-name"), false), "api");
        let hinted = item("b", Some("hint-name"), false);
        let rig = view_with(vec![resolved, hinted], idle_worker());
        let text = render_text(&rig);
        assert!(text.contains("▾ api"), "{text}");
        assert!(
            !text.contains("reason · api"),
            "the heading names it: {text}"
        );
        assert!(text.contains("reason · hint-name"), "{text}");
    }

    #[test]
    fn home_starts_only_fresh_items_while_the_view_lists_stale_ones_too() {
        let mut stale = item("old", None, false);
        stale.stale = true;
        let dismissed = item("gone", None, true);
        let rig = view_with(
            vec![item("fresh", None, false), stale, dismissed],
            idle_worker(),
        );
        assert_eq!(rig.store.keys().len(), 1, "Home starts only the fresh one");
        assert_eq!(rig.view.order.len(), 2, "the view lists the stale one too");
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
    fn a_refreshing_provider_is_reloaded_until_its_refresh_ends() {
        let (sink, requests) = std::sync::mpsc::channel();
        let mut feed = SuggestionsFeed::recording_for_test(
            serde_json::json!({ "providers": [], "items": [] }),
            sink,
        );
        let ago = std::time::Instant::now() - std::time::Duration::from_secs(3);
        (feed.loaded_at, feed.load_attempt_at) = (Some(ago), Some(ago));
        feed.set_snapshot_for_test(serde_json::json!({
            "providers": [{ "id": "gh", "name": "GitHub", "refreshing": true }],
            "items": []
        }));
        feed.tick(true);
        assert_eq!(
            requests
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "load"
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
        assert!(!rig.view.apply(&mut rig.store, &mut polled));
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
        rig.view.apply(&mut rig.store, &mut polled);
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
        assert!(row(&text, "› a").ends_with(" ref-a"), "{text}");
        for expected in [
            "reason · repo",
            "Source text",
            "preview text",
            "Prompt (untrusted source text)",
            "run \u{fffd}[31mrm -rf\u{fffd}[0m now",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text}");
        }
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn the_source_reference_follows_the_pref() {
        for (shown, entry_ref) in [(true, "ref-a"), (false, "")] {
            let mut rig = view_with(vec![item("a", None, false)], idle_worker());
            rig.store.snapshot.as_mut().unwrap().prefs.show_source_ref = shown;
            let text = render_text(&rig);
            assert_eq!(row(&text, "› a").ends_with(" ref-a"), shown, "{text}");
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
            pre_trust: None,
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
                    pre_trust: None,
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
    fn strict_mode_moves_each_view_letter_like_home_does() {
        let worker = Worker::spawn("t", |_| Res::Snapshot(Err("unused".into())));
        let mut rig = view_with(vec![item("a", None, false), item("b", None, true)], worker);
        rig.view.strict = true;
        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);

        for bare in ['d', 'e', 'N'] {
            press(&mut rig, KeyCode::Char(bare), true);
        }
        assert!(!rig.view.show_dismissed && rig.view.preparing.is_none());
        press(&mut rig, KeyCode::Char('D'), true);
        assert!(rig.view.show_dismissed, "D shows dismissed");
        rig.view.handle_key(ctrl_n, None, &mut rig.store);
        assert!(rig.view.preparing.is_some() && rig.view.prepare_another);
        let text = render_text(&rig);
        assert!(text.contains("Ctrl+N new"), "{text}");
    }

    /// At 80x24 a heading reads apart from its rows, the divider carries the
    /// provider status, and no key is split from its label.
    #[test]
    fn the_view_reads_at_80_by_24() {
        let mut long = in_project(item("b", None, false), "web");
        long.draft.prompt = (0..40).map(|i| format!("line {i}\n")).collect();
        let mut rig = view_with(
            vec![
                in_project(item("a", None, false), "web"),
                long,
                in_project(item("c", None, false), "cli"),
            ],
            idle_worker(),
        );
        press(&mut rig, KeyCode::Char('j'), true);
        let text = render_sized(&rig, None, 80, 24);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 24);
        assert!(lines[2].contains("▾ web") && lines[2].trim_end_matches(['│', ' ']).ends_with('2'));
        assert!(lines[3].contains("    a"), "{text}");
        assert!(
            lines[5].trim_matches(['│', ' ']).is_empty(),
            "a gap between items: {text}"
        );
        assert!(lines[6].contains("  › b"), "{text}");
        assert!(text.contains("├─ GitHub · updated 2 min ago ─"), "{text}");
        let keys = [
            "↵ start",
            "e change first",
            "N new",
            "z dismiss",
            "PgDn more",
            "? all keys",
            "Esc back",
        ];
        for key in keys {
            assert!(
                lines.iter().any(|l| l.contains(key)),
                "{key:?} split or missing: {text}"
            );
        }
    }

    #[test]
    fn question_mark_lists_every_key_and_any_key_closes_it() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        rig.view.strict = true;
        press(&mut rig, KeyCode::Char('?'), true);
        let text = render_sized(&rig, None, 80, 24);
        for (key, what) in [
            ("E", "change first"),
            ("Ctrl+N", "a new session"),
            ("T", "turn on pre-trust"),
        ] {
            assert!(
                row(&text, what).starts_with(&format!("{key} ")),
                "{key:?}: {text}"
            );
        }
        press(&mut rig, KeyCode::Char('D'), true);
        assert!(
            !rig.view.help && !rig.view.show_dismissed,
            "the key only closed the list"
        );
    }

    #[test]
    fn an_empty_list_says_when_it_was_checked_and_how_to_check_again() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        rig.store.snapshot.as_mut().unwrap().items.clear();
        rig.view.sync(&rig.store);
        let text = render_sized(&rig, None, 80, 24);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[2].contains("│ Nothing to suggest right now."),
            "{text}"
        );
        assert!(
            lines[3].contains("GitHub · nothing new. r checks again."),
            "{text}"
        );
    }

    #[test]
    fn x_and_l_no_longer_do_anything() {
        let mut rig = view_with(vec![item("a", None, false)], idle_worker());
        press(&mut rig, KeyCode::Char('x'), true);
        press(&mut rig, KeyCode::Char('l'), true);
        assert!(!rig.store.busy);
    }
}
