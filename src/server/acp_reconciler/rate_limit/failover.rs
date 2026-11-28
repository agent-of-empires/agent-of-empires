//! Opt-in account failover for rate-limit parks.
//!
//! A parked host session whose pool has an account with headroom swaps to it through the
//! restart path's account swap instead of waiting for the reset. The swap itself, its
//! duplicate-prompt guard and the interrupted-prompt continuation live in
//! `lifecycle::failover_account_swap`; this module decides when and where to.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::time::Timestamp;
use tokio::sync::Semaphore;

use crate::acp::acp_client::FIVE_HOUR_WINDOW_SECS;
use crate::acp::event_store::{AccountState, WindowLimit};
use crate::acp::state::RateLimitInfo;
use crate::server::api::sessions::FailoverError;
use crate::server::AppState;
use crate::session::Instance;

/// Failover restarts running at once. Each stops a worker and copies a transcript that can be
/// large, so a limit hitting many sessions on one account must not start them all together.
const MAX_CONCURRENT_FAILOVERS: usize = 2;

/// How long a failed login keeps an account out of the pool. The entry is derived from the
/// startup error's own time, so it lapses by itself once the session stops failing there.
const AUTH_FAILED_RETRY_MS: i64 = 3_600_000;

static PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_FAILOVERS)));

/// Sessions with a failover running, each with the account it moves to, so a later pass never
/// starts a second one and the list keeps naming the account the running one chose.
static IN_FLIGHT: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Mutex::default);

/// Attempts a park gets before the timed resume is left to handle it alone.
const MAX_FAILOVER_ATTEMPTS: u32 = 3;

/// Wait after the first failed attempt; doubled for each further one.
const FAILOVER_RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// How long after its last retry time a session's record is kept. Bounds the map for sessions
/// that were deleted or whose park ended without a success.
const RETRY_RETENTION: Duration = Duration::from_secs(24 * 3600);

/// What failed attempts have been made for each session's current park.
static RETRIES: LazyLock<Mutex<Retries>> = LazyLock::new(Mutex::default);

/// Pool entries already warned about, so a misconfigured name logs once.
static WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// Whether the account table is currently unreadable and already reported, so a persistent fault
/// logs once instead of on every pass for every parked session.
static STATE_UNREADABLE: AtomicBool = AtomicBool::new(false);

/// Marks the start of an outage; `true` only for the first failure since the last good read.
fn begin_outage(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Failover settings one profile resolves to.
#[derive(Clone, Default)]
pub(crate) struct Settings {
    pub(super) enabled: bool,
    pub(super) accounts: Vec<String>,
}

impl Settings {
    pub(crate) fn of(config: &crate::session::config::Config) -> Self {
        Self {
            enabled: config.acp.rate_limit_failover,
            accounts: config.acp.rate_limit_failover_accounts.clone(),
        }
    }
}

/// A pool member and the account it is, identified by its resolved config root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PoolAccount {
    pub(super) tool: String,
    pub(super) key: String,
}

/// The account a host session currently runs as. `None` for a sandboxed session or an
/// unresolvable root, which never take part in failover or the limit table.
pub(super) fn account_key(row: &Instance) -> Option<String> {
    crate::session::conversation_carry::host_account_root(row, &row.effective_profile(), &row.tool)
        .as_deref()
        .map(path_key)
}

/// Aliases of one directory (a symlink, `..`) are one account, including before the directory
/// exists: the deepest existing ancestor is resolved and the missing tail appended, so a config
/// root keys the same on the attempt that creates it and on every one after.
pub(super) fn path_key(path: &std::path::Path) -> String {
    let mut deepest = path.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&deepest) {
            let tail = path
                .strip_prefix(&deepest)
                .unwrap_or(std::path::Path::new(""));
            return fold_tail(real, tail).to_string_lossy().into_owned();
        }
        match deepest.parent() {
            Some(parent) => deepest = parent.to_path_buf(),
            None => return path.to_string_lossy().into_owned(),
        }
    }
}

/// Apply the unresolved tail to the resolved ancestor. Nothing there exists yet, so `..` can only
/// cancel the component before it, the way `canonicalize` folds it once the path does exist.
fn fold_tail(mut key: std::path::PathBuf, tail: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    for part in tail.components() {
        match part {
            Component::ParentDir => drop(key.pop()),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::Normal(name) => key.push(name),
        }
    }
    key
}

/// The configured pool entries that run the same built-in agent as `row` and declare a config
/// directory that resolves to a root.
/// Others are ignored with one warning each.
pub(super) fn resolve_pool(row: &Instance, accounts: &[String]) -> Vec<PoolAccount> {
    let profile = &row.source_profile;
    let mut pool = Vec::new();
    for tool in accounts {
        if *tool == row.tool {
            // The session's own account is the one that is limited.
            continue;
        }
        let same_agent = crate::session::conversation_carry::is_account_swap(
            &row.effective_profile(),
            &row.tool,
            &row.detect_as,
            profile,
            tool,
        );
        // Only a directory the config declares for the entry counts: without one the root would
        // fall back to the environment or the default, an account outside the pool contract.
        let root: Option<PathBuf> = crate::session::conversation_carry::declared_account_dir(
            profile, tool,
        )
        .and_then(|_| crate::session::conversation_carry::host_account_root(row, profile, tool));
        match (same_agent, root) {
            (true, Some(root)) => pool.push(PoolAccount {
                tool: tool.clone(),
                key: path_key(&root),
            }),
            _ => {
                if lock(&WARNED).insert(format!("{}\0{tool}", row.tool)) {
                    tracing::warn!(
                        target: "acp.supervisor",
                        session = %row.id,
                        account = %tool,
                        from = %row.tool,
                        "rate-limit failover: pool account does not run the same agent or has no declared, resolvable config directory; ignored"
                    );
                }
            }
        }
    }
    pool
}

/// The first pool account other than the current one that is not known to be limited or
/// logged out. Accounts never seen have headroom; one whose state cannot be read does not.
pub(super) fn select_target<'a>(
    pool: &'a [PoolAccount],
    current_key: &str,
    now_ms: i64,
    state_of: impl Fn(&str) -> rusqlite::Result<Option<AccountState>>,
) -> Option<&'a PoolAccount> {
    pool.iter().find(|account| {
        account.key != current_key
            && match state_of(&account.key) {
                Ok(state) => {
                    STATE_UNREADABLE.store(false, Ordering::Relaxed);
                    state.is_none_or(|state| state.has_headroom(now_ms))
                }
                Err(error) => {
                    if begin_outage(&STATE_UNREADABLE) {
                        tracing::error!(
                            target: "acp.supervisor",
                            account = %account.tool,
                            %error,
                            "rate-limit failover: account state unreadable; no account is a target until it reads again"
                        );
                    }
                    false
                }
            }
    })
}

/// The account a park observed through `row` belongs to. A swap's failed launch leaves the row
/// naming the target until the rollback, and a snapshot can outlive that rollback, so a running
/// swap or a row that no longer matches the live one yields `None`.
async fn observed_account_key(state: &Arc<AppState>, row: &Instance) -> Option<String> {
    let key = account_key(row)?;
    if in_flight(&row.id) {
        return None;
    }
    let live = {
        let rows = state.instances.read().await;
        rows.iter().find(|i| i.id == row.id).and_then(account_key)
    };
    (live.as_deref() == Some(key.as_str())).then_some(key)
}

/// Writes what this park says about the session's current account.
pub(super) async fn record_limit(
    state: &Arc<AppState>,
    row: &Instance,
    info: &RateLimitInfo,
    limited_until: Timestamp,
    observed_at_ms: i64,
) {
    let Some(key) = observed_account_key(state, row).await else {
        return;
    };
    let store = Arc::clone(&state.acp_event_store);
    let kind = info.kind.clone();
    let estimated = info.resets_at.is_none();
    let _ = tokio::task::spawn_blocking(move || {
        store.record_account_limit(
            &key,
            limited_until.as_millis(),
            &kind,
            observed_at_ms,
            estimated,
        );
    })
    .await;
}

/// The wake deadline chosen for a park and when it was observed.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Wake {
    pub(super) at_ms: i64,
    pub(super) observed_at_ms: i64,
    /// The park's own reset stands: no account report replaced it.
    pub(super) own: bool,
}

/// What the wake decision needs of a park.
pub(super) struct ParkView<'a> {
    pub(super) kind: &'a str,
    /// The park's own wake (its reset plus grace), when it carries a reset of its own.
    pub(super) own_ms: Option<i64>,
    /// When that reset was observed, or for a park without one when the park began.
    pub(super) observed_at_ms: i64,
    /// A reset the park inherited from a report that named no window, which stays generic
    /// evidence: it may extend the park's window, never stand in for it.
    pub(super) generic: Option<WindowLimit>,
}

/// The wake deadline for a park from the session's account table: a session that started after
/// the reset was announced never saw it, and a later report of the park's window supersedes the
/// reset the park carries. `None` leaves the park to its own schedule.
pub(super) async fn reported_wake(
    state: &Arc<AppState>,
    row: &Instance,
    park: &ParkView<'_>,
) -> Reported {
    let none = Reported {
        wake: None,
        generic_superseded: false,
    };
    let Some(key) = observed_account_key(state, row).await else {
        return none;
    };
    let store = Arc::clone(&state.acp_event_store);
    let Some(account) = tokio::task::spawn_blocking(move || store.account_state(&key))
        .await
        .ok()
        .flatten()
    else {
        return none;
    };
    Reported {
        wake: wake_deadline(park, &account.windows, Timestamp::now().as_millis()),
        generic_superseded: generic_superseded(park, &account.windows),
    }
}

/// What the account table says about a park: its wake, and whether a newer window-less report
/// invalidated the reset the park carries. No wake with the reset still standing means no
/// evidence (no account row yet), so the park keeps its own schedule.
#[derive(Default)]
pub(super) struct Reported {
    pub(super) wake: Option<Wake>,
    pub(super) generic_superseded: bool,
}

/// A window-less report observed after the reset the park inherited supersedes it, whether or
/// not that report still applies.
fn generic_superseded(park: &ParkView<'_>, windows: &[WindowLimit]) -> bool {
    let undated = park.own_ms.is_none();
    park.generic.as_ref().is_some_and(|generic| {
        windows.iter().any(|w| {
            !w.estimated
                && relation(park.kind, undated, w) == Some(Relation::Ambiguous)
                && (w.observed_at_ms, w.until_ms) > (generic.observed_at_ms, generic.until_ms)
        })
    })
}

/// How a report relates to a park's window.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Relation {
    /// The same window: its latest observation is the truth, either way it moves.
    Same,
    /// A window-less report: it may be another window of the account.
    Ambiguous,
    /// A named window under a window-less undated park: the park may be any of them.
    Any,
}

fn relation(park_kind: &str, undated: bool, w: &WindowLimit) -> Option<Relation> {
    // A billing deadline is its own: it matches by exact kind only, both ways.
    if park_kind == "billing_error" || w.kind == "billing_error" {
        return (w.kind == park_kind).then_some(Relation::Same);
    }
    if w.kind == park_kind {
        return Some(Relation::Same);
    }
    if w.kind == "rate_limit" {
        // `until_ms` is the scheduled wake, the reset plus grace: reach is the raw reset's.
        let grace_ms = i64::from(super::RATE_LIMIT_AUTO_RESUME_GRACE_SECS) * 1000;
        let unreachable = park_kind == "five_hour"
            && w.until_ms - grace_ms - w.observed_at_ms > FIVE_HOUR_WINDOW_SECS * 1000;
        return (!unreachable).then_some(Relation::Ambiguous);
    }
    (undated && park_kind == "rate_limit").then_some(Relation::Any)
}

/// The wake deadline of a park, from its own reset and the account's reported windows.
///
/// Reference rule. Reports are never estimated, and billing and quota never mix.
/// 1. A park with no reset of its own only sees a report that is applicable: observed since the
///    park, or not yet over when the park began (so the report in force then decides the wake
///    through its own deadline, however late the pass).
/// 2. A window-less undated park takes the longest applicable deadline of all.
/// 3. Otherwise the baseline is the latest observation of the park's own window: its reset or a
///    report naming it (the park's reset wins a tie), either way the deadline moves.
/// 4. Window-less reports are one window, so the latest observation of them supersedes the
///    others first, applicable or not: an elapsed correction invalidates the older reset the park
///    carries. That one may be another window of the account, so it only counts when applicable,
///    observed after the baseline and ending later: it extends the baseline, never shortens it,
///    and a superseded report never stands in for it.
/// 5. With no baseline at all, the latest observed window-less report stands if applicable.
/// 6. A reset the park inherited from a window-less report (`generic`) is such a report, always
///    applicable, whatever window the park names.
fn wake_deadline(park: &ParkView<'_>, windows: &[WindowLimit], now_ms: i64) -> Option<Wake> {
    let undated = park.own_ms.is_none();
    let applicable = |w: &&WindowLimit| {
        !undated
            || w.until_ms > now_ms
            || w.until_ms > park.observed_at_ms
            || w.observed_at_ms >= park.observed_at_ms
    };
    let related = || {
        windows
            .iter()
            .filter(|w| !w.estimated)
            .filter(applicable)
            .filter_map(|w| relation(park.kind, undated, w).map(|r| (r, w)))
    };
    let report = |w: &WindowLimit| Wake {
        at_ms: w.until_ms,
        observed_at_ms: w.observed_at_ms,
        own: false,
    };
    if undated && park.kind == "rate_limit" {
        return related()
            .map(|(_, w)| w)
            .max_by_key(|w| w.until_ms)
            .map(report);
    }
    let same = related()
        .filter(|(r, _)| *r == Relation::Same)
        .map(|(_, w)| w)
        .max_by_key(|w| (w.observed_at_ms, w.until_ms));
    let baseline = match (park.own_ms, same) {
        (Some(_), Some(w)) if w.observed_at_ms > park.observed_at_ms => Some(report(w)),
        (Some(own_ms), _) => Some(Wake {
            at_ms: own_ms,
            observed_at_ms: park.observed_at_ms,
            own: true,
        }),
        (None, Some(w)) => Some(report(w)),
        (None, None) => None,
    };
    // The latest window-less observation supersedes the others whether or not it is applicable:
    // an elapsed correction still invalidates the older copy the park carries.
    let ambiguous = windows
        .iter()
        .filter(|w| !w.estimated)
        .filter(|w| relation(park.kind, undated, w) == Some(Relation::Ambiguous))
        .map(|w| (w, applicable(&w)))
        .chain(park.generic.as_ref().map(|w| (w, true)))
        .max_by_key(|(w, usable)| (w.observed_at_ms, w.until_ms, *usable))
        .filter(|(_, usable)| *usable)
        .map(|(w, _)| w)
        .filter(|w| {
            baseline
                .as_ref()
                .is_none_or(|b| w.observed_at_ms > b.observed_at_ms && w.until_ms > b.at_ms)
        });
    ambiguous.map(report).or(baseline)
}

/// Marks the session's current account unusable when its newest startup error is a failed login.
pub(super) async fn note_auth_failure(state: &Arc<AppState>, row: &Instance) {
    let Some(key) = account_key(row) else {
        return;
    };
    let store = Arc::clone(&state.acp_event_store);
    let id = row.id.clone();
    let _ = tokio::task::spawn_blocking(move || record_auth_failure(&store, &key, &id, 0)).await;
}

/// What a failed attempt says about its target: the launch itself reported a missing login or a
/// rate limit, or an earlier startup error in the session's log was a failed login. The row is
/// back on its own account by now, so none of this can be read from the row.
fn note_failed_attempt(
    store: &crate::acp::event_store::EventStore,
    key: &str,
    id: &str,
    error: &FailoverError,
    attempt_ms: i64,
) {
    if let Some(info) = &error.target_rate_limit {
        let until = super::scheduled_resume_at(info, attempt_ms, 0);
        store.record_account_limit(
            key,
            until.as_millis(),
            &info.kind,
            attempt_ms,
            info.resets_at.is_none(),
        );
    } else if error.target_auth_failed {
        store.record_account_unusable(key, attempt_ms + AUTH_FAILED_RETRY_MS, attempt_ms);
    } else {
        record_auth_failure(store, key, id, attempt_ms);
    }
}

/// Marks `key` unusable when `id`'s newest startup error, observed at or after `since_ms`, is a
/// failed login.
fn record_auth_failure(
    store: &crate::acp::event_store::EventStore,
    key: &str,
    id: &str,
    since_ms: i64,
) {
    let Some((message, at_ms)) = store.unassigned_startup_error(id) else {
        return;
    };
    if at_ms >= since_ms && is_auth_failure(&message) {
        store.record_account_unusable(key, at_ms + AUTH_FAILED_RETRY_MS, at_ms);
    }
}

fn is_auth_failure(message: &str) -> bool {
    crate::acp::acp_client::is_authentication_failure(message)
}

struct InFlight(String);

impl InFlight {
    fn claim(id: &str, target: &str) -> Option<Self> {
        match lock(&IN_FLIGHT).entry(id.to_owned()) {
            std::collections::hash_map::Entry::Occupied(_) => None,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(target.to_owned());
                Some(Self(id.to_owned()))
            }
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        lock(&IN_FLIGHT).remove(&self.0);
    }
}

pub(crate) fn in_flight(id: &str) -> bool {
    lock(&IN_FLIGHT).contains_key(id)
}

fn in_flight_target(id: &str) -> Option<String> {
    lock(&IN_FLIGHT).get(id).cloned()
}

/// What a session's failed failover attempts allow next.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    /// Start (or restart) an attempt.
    Go,
    /// An attempt failed transiently; hold the park until its backoff passes.
    Wait,
    /// A refusal that cannot clear, or the attempts are spent: leave the park to the timed resume.
    GiveUp,
}

/// Failed attempts per session, for the park (identified by when its limit was recorded) they
/// were made on. A new park starts clean.
#[derive(Default)]
struct Retries(HashMap<String, Retry>);

struct Retry {
    park: i64,
    failures: u32,
    retry_at: Instant,
    permanent: bool,
}

impl Retries {
    fn gate(&self, id: &str, park: i64, now: Instant) -> Gate {
        match self.0.get(id) {
            Some(retry) if retry.park == park => {
                if retry.permanent || retry.failures >= MAX_FAILOVER_ATTEMPTS {
                    Gate::GiveUp
                } else if now < retry.retry_at {
                    Gate::Wait
                } else {
                    Gate::Go
                }
            }
            _ => Gate::Go,
        }
    }

    /// `backoff` is false when the target account is to blame and is now out of the pool: the next
    /// attempt goes to another account and may start at once. The attempt still counts.
    fn failed(&mut self, id: &str, park: i64, permanent: bool, backoff: bool, now: Instant) {
        let earlier = match self.0.get(id) {
            Some(retry) if retry.park == park => retry.failures,
            _ => 0,
        };
        let failures = earlier.saturating_add(1);
        self.0
            .retain(|_, retry| retry.retry_at + RETRY_RETENTION > now);
        let backoff = if backoff {
            FAILOVER_RETRY_BACKOFF * 2u32.pow((failures - 1).min(8))
        } else {
            Duration::ZERO
        };
        self.0.insert(
            id.to_owned(),
            Retry {
                park,
                failures,
                retry_at: now + backoff,
                permanent,
            },
        );
    }

    fn succeeded(&mut self, id: &str) {
        self.0.remove(id);
    }
}

/// Whether a failover for `id` is running, or must wait for a retry or give way, so the caller
/// knows whether to fall through to the timed resume.
fn holds(id: &str, park: i64, now: Instant) -> Gate {
    if in_flight(id) {
        return Gate::Wait;
    }
    lock(&RETRIES).gate(id, park, now)
}

/// The account a parked session is about to move to, or `None` when it will stay and show the
/// choice: failover off, the park already resumed or capped, the attempts spent or refused, or
/// no pool account with headroom. Mirrors what `maybe_fail_over` would decide at this moment.
pub(crate) fn pending_target(
    row: &Instance,
    settings: &Settings,
    park: &crate::acp::event_store::RateLimitPark,
    streak: i64,
    store: &crate::acp::event_store::EventStore,
) -> Option<String> {
    if !settings.enabled
        || park.cap_reached
        || park.last_resume_attempt_ms.is_some()
        || streak >= super::RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES
    {
        return None;
    }
    // A running swap is the one hold that still ends on the target account, and the restart has
    // already moved the row onto it, so the answer is the account it chose; a backoff or a
    // refusal leaves the choice showing.
    if let Some(target) = in_flight_target(&row.id) {
        return Some(target);
    }
    if lock(&RETRIES).gate(&row.id, park.recorded_at_ms, Instant::now()) != Gate::Go {
        return None;
    }
    let current_key = account_key(row)?;
    let pool = resolve_pool(row, &settings.accounts);
    let now_ms = Timestamp::now().as_millis();
    select_target(&pool, &current_key, now_ms, |key| {
        store.account_state_checked(key)
    })
    .map(|account| account.tool.clone())
}

/// Starts a failover for the parked `row` when the pool has an account with headroom.
/// Returns `true` when one is running for it (now or from an earlier pass) or waits for a
/// restart slot, which means the caller leaves the session alone. `false` falls back to the
/// normal banner and timed resume.
pub(super) async fn maybe_fail_over(
    state: &Arc<AppState>,
    row: &Instance,
    accounts: &[String],
    head_seq: u64,
    park: i64,
) -> bool {
    match holds(&row.id, park, Instant::now()) {
        Gate::Wait => return true,
        Gate::GiveUp => return false,
        Gate::Go => {}
    }
    let Some(current_key) = account_key(row) else {
        return false;
    };
    let pool = resolve_pool(row, accounts);
    if pool.is_empty() {
        return false;
    }
    let store = Arc::clone(&state.acp_event_store);
    let now_ms = Timestamp::now().as_millis();
    let target = {
        let store = Arc::clone(&store);
        tokio::task::spawn_blocking(move || {
            select_target(&pool, &current_key, now_ms, |key| {
                store.account_state_checked(key)
            })
            .map(|account| (account.tool.clone(), account.key.clone()))
        })
        .await
        .ok()
        .flatten()
    };
    let Some((target, target_key)) = target else {
        return false;
    };
    let Ok(permit) = Arc::clone(&PERMITS).try_acquire_owned() else {
        tracing::debug!(target: "acp.supervisor", session = %row.id, "rate-limit failover: waiting for a free restart slot");
        return true;
    };
    let Some(claim) = InFlight::claim(&row.id, &target) else {
        return true;
    };
    let state = Arc::clone(state);
    let id = row.id.clone();
    let from = row.tool.clone();
    let generation = row.lifecycle_generation;
    tokio::spawn(async move {
        let _permit = permit;
        let _claim = claim;
        match crate::server::api::sessions::failover_account_swap(
            &state, &id, &target, head_seq, generation,
        )
        .await
        {
            Ok(()) => {
                lock(&RETRIES).succeeded(&id);
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    from = %from,
                    to = %target,
                    "rate-limit failover: continued on another account"
                );
            }
            Err(error) => {
                let target_blamed = error.target_auth_failed || error.target_rate_limit.is_some();
                lock(&RETRIES).failed(&id, park, error.permanent, !target_blamed, Instant::now());
                // The restore puts the row back on its own account, so a login failure of the
                // target would otherwise go unrecorded and the next attempt reselect it.
                let session = id.clone();
                let noted = error.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    note_failed_attempt(&store, &target_key, &session, &noted, now_ms)
                })
                .await;
                tracing::warn!(
                    target: "acp.supervisor",
                    session = %id,
                    to = %target,
                    permanent = error.permanent,
                    error = %error.message,
                    "rate-limit failover: not performed; the normal rate-limit handling stays"
                );
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(tool: &str, key: &str) -> PoolAccount {
        PoolAccount {
            tool: tool.into(),
            key: key.into(),
        }
    }

    fn state(limited_until_ms: i64, unusable_until_ms: i64) -> AccountState {
        AccountState {
            windows: vec![WindowLimit {
                kind: "usage".into(),
                until_ms: limited_until_ms,
                estimated: false,
                observed_at_ms: 0,
            }],
            unusable_until_ms,
        }
    }

    #[test]
    fn selection_takes_the_first_other_account_with_headroom() {
        let pool = [account("b", "/b"), account("c", "/c"), account("d", "/d")];
        let known: HashMap<&str, AccountState> = [
            ("/b", state(2_000, 0)),
            ("/c", state(0, 2_000)),
            ("/d", state(900, 0)),
        ]
        .into();
        let pick = |current: &str, now: i64| {
            select_target(&pool, current, now, |key| Ok(known.get(key).cloned()))
                .map(|a| a.tool.as_str())
        };
        assert_eq!(
            pick("/a", 1_000),
            Some("d"),
            "limited and logged-out members are skipped"
        );
        assert_eq!(
            pick("/a", 2_000),
            Some("b"),
            "an elapsed limit is headroom again"
        );
        assert_eq!(
            pick("/d", 1_000),
            None,
            "the current account is never a target"
        );
        assert_eq!(pick("/b", 1_000), Some("d"));
        assert_eq!(
            select_target(&[], "/a", 0, |_| Ok(None)).map(|a| &a.tool),
            None,
            "an empty pool selects nothing"
        );
        let unreadable = |key: &str| {
            if key == "/b" {
                Err(rusqlite::Error::InvalidQuery)
            } else {
                Ok(None)
            }
        };
        assert_eq!(
            select_target(&pool, "/a", 0, unreadable).map(|a| a.tool.as_str()),
            Some("c"),
            "an account whose state cannot be read is skipped, not assumed free"
        );
        assert_eq!(
            select_target(&pool[..1], "/a", 0, unreadable),
            None,
            "a failed lookup alone never yields a target"
        );
    }

    #[test]
    fn an_unreadable_table_is_reported_once_per_outage() {
        let flag = AtomicBool::new(false);
        assert!(begin_outage(&flag));
        assert!(
            !begin_outage(&flag),
            "the same outage is not reported again"
        );
        flag.store(false, Ordering::Relaxed);
        assert!(begin_outage(&flag), "a later outage is");
    }

    /// Only an error after the attempt starts marks its target unusable.
    #[test]
    fn a_login_failure_after_the_attempt_marks_the_target_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::acp::event_store::EventStore::open(
            &dir.path().join("events.db"),
            100,
            Arc::default(),
        )
        .unwrap();
        let event = crate::acp::state::Event::startup_error("Authentication required");
        store
            .record(
                "s",
                1,
                &event,
                &crate::acp::event_sender::EventMetadata::host(&event),
            )
            .unwrap();
        let now = Timestamp::now().as_millis();

        record_auth_failure(&store, "/target", "s", now + 3_600_000);
        assert_eq!(
            store.account_state("/target"),
            None,
            "an earlier error is not this attempt's"
        );

        record_auth_failure(&store, "/target", "s", now - 3_600_000);
        let state = store.account_state("/target").unwrap();
        assert!(
            !state.has_headroom(now),
            "the target is out of the pool for a while"
        );
    }

    /// A login failure before session establishment still excludes that account.
    #[test]
    fn a_login_failure_the_launch_reported_sends_the_next_pass_to_the_next_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::acp::event_store::EventStore::open(
            &dir.path().join("events.db"),
            100,
            Arc::default(),
        )
        .unwrap();
        let pool = [account("b", "/b"), account("c", "/c")];
        let now = Timestamp::now().as_millis();
        let pick = || {
            select_target(&pool, "/a", now + 1_000, |key| {
                store.account_state_checked(key)
            })
            .map(|a| a.tool.clone())
        };
        assert_eq!(pick().as_deref(), Some("b"));

        let failure = |auth_failed, rate_limit| FailoverError {
            permanent: false,
            target_auth_failed: auth_failed,
            target_rate_limit: rate_limit,
            message: String::new(),
        };
        note_failed_attempt(&store, "/b", "s", &failure(true, None), now);

        assert_eq!(pick().as_deref(), Some("c"));
        let state = store.account_state("/b").unwrap();
        assert!(state.has_headroom(now + AUTH_FAILED_RETRY_MS), "it expires");
    }

    /// A target's launch limit excludes it without changing the original account.
    #[test]
    fn a_rate_limit_the_launch_reported_is_recorded_against_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::acp::event_store::EventStore::open(
            &dir.path().join("events.db"),
            100,
            Arc::default(),
        )
        .unwrap();
        let pool = [account("b", "/b"), account("c", "/c")];
        let now = Timestamp::now().as_millis();
        let limited = |kind: &str, resets_in_ms: Option<i64>| FailoverError {
            permanent: false,
            target_auth_failed: false,
            target_rate_limit: Some(RateLimitInfo {
                status: "limited".into(),
                resets_at: resets_in_ms.and_then(|ms| Timestamp::from_millis(now + ms)),
                kind: kind.into(),
            }),
            message: String::new(),
        };
        let pick = || {
            select_target(&pool, "/a", now + 1_000, |key| {
                store.account_state_checked(key)
            })
            .map(|a| a.tool.clone())
        };

        note_failed_attempt(&store, "/b", "s", &limited("usage", Some(3_600_000)), now);
        assert_eq!(pick().as_deref(), Some("c"));
        let on_b = store.account_state("/b").unwrap();
        assert!(!on_b.window("usage").unwrap().estimated);
        assert!(!on_b.has_headroom(now + 3_000_000) && on_b.has_headroom(now + 3_700_000));
        assert_eq!(store.account_state("/a"), None, "A is not B");

        note_failed_attempt(&store, "/c", "s", &limited("usage", None), now);
        assert_eq!(pick(), None, "every other account is limited");
        assert!(
            store
                .account_state("/c")
                .unwrap()
                .window("usage")
                .unwrap()
                .estimated
        );
    }

    #[test]
    fn aliases_of_one_directory_are_one_account() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(path_key(&link), path_key(&real));
        assert_eq!(path_key(&real.join("..").join("real")), path_key(&real));
        let missing = dir.path().join("not-yet");
        assert_eq!(
            path_key(&missing),
            format!("{}/not-yet", path_key(dir.path())),
            "a missing tail is appended to the resolved parent"
        );
        let relative = std::path::Path::new("no-such/account");
        assert_eq!(
            path_key(relative),
            relative.to_string_lossy(),
            "a path with no existing ancestor stays as written"
        );
    }

    #[test]
    fn an_account_keys_the_same_before_and_after_its_root_appears() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let root = link.join("account");
        let before = path_key(&root);
        assert_eq!(before, path_key(&real.join("account")));
        std::fs::create_dir(&root).unwrap();
        assert_eq!(
            before,
            path_key(&root),
            "creating the root must not rekey the account"
        );

        let scope = real.join("scope");
        std::fs::create_dir(&scope).unwrap();
        let through_parent = scope.join("missing").join("..").join("account");
        let before = path_key(&through_parent);
        assert_eq!(
            before,
            path_key(&scope.join("account")),
            "a `..` root names the directory it ends at"
        );
        std::fs::create_dir(scope.join("missing")).unwrap();
        std::fs::create_dir(scope.join("account")).unwrap();
        assert_eq!(
            before,
            path_key(&through_parent),
            "creating the `..` root must not rekey the account"
        );
    }

    fn win(kind: &str, until_ms: i64, observed_at_ms: i64) -> WindowLimit {
        WindowLimit {
            kind: kind.into(),
            until_ms,
            estimated: false,
            observed_at_ms,
        }
    }

    /// `(observed, until)` of a report in the table below.
    type Report = (i64, i64);

    /// The rule of `wake_deadline`, restated over the table's two kinds: a named park
    /// (`seven_day`, with `weekly` its own-window reports) or a window-less one (`rate_limit`),
    /// and `unnamed` window-less reports. Returns the wake and whether the park's own stands.
    fn reference(
        named: bool,
        own: Option<i64>,
        park_obs: i64,
        now: i64,
        weekly: Option<Report>,
        unnamed: &[Report],
        inherited: Option<Report>,
    ) -> Option<(i64, bool)> {
        let seen = |r: &&Report| own.is_some() || r.1 > now || r.1 > park_obs || r.0 >= park_obs;
        let weekly: Vec<Report> = weekly.iter().filter(seen).copied().collect();
        if !named && own.is_none() {
            let all = weekly.iter().chain(unnamed.iter().filter(seen));
            return all.map(|r| r.1).max().map(|at| (at, false));
        }
        // A reset the named park inherited from a window-less report is one more window-less
        // report, always applicable. The latest of them all supersedes the rest, applicable or
        // not, and stands only if it is applicable.
        let latest_unnamed = unnamed
            .iter()
            .map(|r| (*r, seen(&r)))
            .chain(inherited.map(|r| (r, true)))
            .max_by_key(|&((o, u), usable)| (o, u, usable))
            .filter(|&(_, usable)| usable)
            .map(|(r, _)| r);
        let unnamed: Vec<Report> = if named {
            latest_unnamed.into_iter().collect()
        } else {
            unnamed.iter().filter(seen).copied().collect()
        };
        // A window-less park's own window is the unnamed reports; it has no other.
        let (same, ambiguous) = if named {
            (weekly, unnamed)
        } else {
            (unnamed, vec![])
        };
        let base = match (own, same.iter().max()) {
            (Some(_), Some(&(o, u))) if o > park_obs => Some((u, o, false)),
            (Some(at), _) => Some((at, park_obs, true)),
            (None, Some(&(o, u))) => Some((u, o, false)),
            (None, None) => None,
        };
        let extension = ambiguous
            .iter()
            .max()
            .filter(|&&(o, u)| base.is_none_or(|(at, bo, _)| o > bo && u > at));
        match (extension, base) {
            (Some(&(_, u)), _) => Some((u, false)),
            (None, Some((at, _, own))) => Some((at, own)),
            (None, None) => None,
        }
    }

    /// Every ordering of observation time, deadline, named or window-less, before or after the
    /// park, before or after the deadline, against the reference rule and two properties that
    /// do not depend on it: a window-less report never ends a named park's baseline sooner, and
    /// an undated park never sees a named report that is over and predates it (a window-less
    /// one still supersedes the older window-less reports of a named park).
    #[test]
    fn wake_deadline_matches_the_reference_over_every_ordering() {
        const PARK_OBS: i64 = 200;
        // Observed before and after the park; deadlines over before the park, then ahead.
        let reports: Vec<Option<Report>> = std::iter::once(None)
            .chain(
                [50, 100, 300, 400]
                    .into_iter()
                    .flat_map(|o| [150, 350, 500, 800].into_iter().map(move |u| Some((o, u)))),
            )
            .collect();
        let mut rows = 0;
        for named in [true, false] {
            for own in [None, Some(450)] {
                for now in [250, 600, 1_000] {
                    for &weekly in &reports {
                        for &first in &reports {
                            for &second in &reports {
                                // Only a named park can have inherited a window-less reset.
                                let inherits: &[Option<Report>] =
                                    if named { &reports } else { &[None] };
                                for &inherited in inherits {
                                    let unnamed: Vec<Report> =
                                        [first, second].into_iter().flatten().collect();
                                    let kind = if named { "seven_day" } else { "rate_limit" };
                                    let windows_of =
                                        |weekly: Option<Report>, unnamed: &[Report]| {
                                            weekly
                                                .iter()
                                                .map(|&(o, u)| win("seven_day", u, o))
                                                .chain(
                                                    unnamed
                                                        .iter()
                                                        .map(|&(o, u)| win("rate_limit", u, o)),
                                                )
                                                .collect::<Vec<_>>()
                                        };
                                    let park = ParkView {
                                        kind,
                                        own_ms: own,
                                        observed_at_ms: PARK_OBS,
                                        generic: inherited.map(|(o, u)| win("rate_limit", u, o)),
                                    };
                                    let got = |weekly, unnamed: &[Report]| {
                                        wake_deadline(&park, &windows_of(weekly, unnamed), now)
                                            .map(|w| (w.at_ms, w.own))
                                    };
                                    let row = format!(
                                    "named {named} own {own:?} now {now} weekly {weekly:?} unnamed {unnamed:?} inherited {inherited:?}"
                                );
                                    let chosen = got(weekly, &unnamed);
                                    assert_eq!(
                                        chosen,
                                        reference(
                                            named, own, PARK_OBS, now, weekly, &unnamed, inherited
                                        ),
                                        "{row}"
                                    );
                                    // The carried reset is superseded only by a window-less
                                    // report observed after it; no report at all supersedes
                                    // nothing, so a missing account row keeps it.
                                    assert_eq!(
                                        generic_superseded(&park, &windows_of(weekly, &unnamed)),
                                        inherited.is_some_and(|(io, iu)| unnamed
                                            .iter()
                                            .any(|&(o, u)| (o, u) > (io, iu))),
                                        "superseded: {row}"
                                    );
                                    if named {
                                        let bare = ParkView {
                                            generic: None,
                                            ..park
                                        };
                                        if let Some((without, _)) =
                                            wake_deadline(&bare, &windows_of(weekly, &[]), now)
                                                .map(|w| (w.at_ms, w.own))
                                        {
                                            let (at, _) = chosen.expect(&row);
                                            assert!(
                                                at >= without,
                                                "window-less report shortened: {row}"
                                            );
                                        }
                                    }
                                    if own.is_none() {
                                        let relevant = |r: &Report| {
                                            r.1 > now || r.1 > PARK_OBS || r.0 >= PARK_OBS
                                        };
                                        // A named park still reads a newer, over window-less report: it
                                        // supersedes the older ones.
                                        let kept: Vec<Report> = unnamed
                                            .iter()
                                            .copied()
                                            .filter(|r| named || relevant(r))
                                            .collect();
                                        let weekly = weekly.filter(relevant);
                                        assert_eq!(
                                            chosen,
                                            got(weekly, &kept),
                                            "irrelevant report: {row}"
                                        );
                                    }
                                    rows += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        // Named parks also range over the inherited slot.
        assert_eq!(rows, 2 * 3 * 17 * 17 * 17 * (17 + 1));
    }

    /// The two orderings reviewers found by hand, then the bounds, as named rows.
    #[test]
    fn a_wake_keeps_the_applicable_correction_through_its_deadline() {
        let wake = |park: &ParkView<'_>, windows: Vec<WindowLimit>, now| {
            wake_deadline(park, &windows, now).map(|w| (w.at_ms, w.observed_at_ms, w.own))
        };
        // Weekly park due at noon (1_200), a weekly correction to 14:00 (1_400), then a newer
        // window-less report ending at 11:00 (1_100): the correction stands.
        let dated = ParkView {
            kind: "seven_day",
            own_ms: Some(1_200),
            observed_at_ms: 1_000,
            generic: None,
        };
        let windows = vec![
            win("seven_day", 1_400, 1_100),
            win("rate_limit", 1_100, 1_200),
        ];
        for now in [1_150, 1_300, 1_450] {
            assert_eq!(
                wake(&dated, windows.clone(), now),
                Some((1_400, 1_100, false)),
                "now {now}"
            );
        }
        // Undated weekly park: the weekly report in force when it began is kept once over; an
        // older window-less report reaching further does not postpone it.
        let undated = ParkView {
            kind: "seven_day",
            own_ms: None,
            observed_at_ms: 1_000,
            generic: None,
        };
        let windows = vec![win("seven_day", 1_300, 900), win("rate_limit", 2_000, 800)];
        for now in [1_200, 1_300, 1_400] {
            assert_eq!(
                wake(&undated, windows.clone(), now),
                Some((1_300, 900, false)),
                "now {now}"
            );
        }
        // A report that was already over when the park began stays excluded.
        assert_eq!(
            wake(&undated, vec![win("seven_day", 900, 500)], 1_400),
            None
        );
        // A window-less report observed later and ending later does extend it.
        assert_eq!(
            wake(
                &dated,
                vec![
                    win("seven_day", 1_400, 1_100),
                    win("rate_limit", 1_600, 1_200)
                ],
                1_300
            ),
            Some((1_600, 1_200, false))
        );
        // Window-less evidence is one window: a newer report ending sooner than the weekly
        // baseline supersedes the longer reset the park inherited, it does not leave it standing.
        // Weekly due 14:00 (1_400), inherited reset to 20:00 (2_000), newer report to 12:00 (1_200).
        let inherited = ParkView {
            generic: Some(win("rate_limit", 2_000, 1_050)),
            ..dated
        };
        assert_eq!(
            wake(
                &inherited,
                vec![
                    win("seven_day", 1_400, 1_020),
                    win("rate_limit", 1_200, 1_100)
                ],
                1_300
            ),
            Some((1_400, 1_020, false))
        );
        // An undated named park (began 12:00, 1_200) carries a reset to 20:00 (2_000) observed
        // at 10:00 (1_000). Another session corrected the account's window-less report at 11:00
        // (1_100) to 11:30 (1_150), already over: it predates the park, but it is still the
        // latest observation, so it invalidates the older copy and no deadline is left.
        let undated = ParkView {
            kind: "seven_day",
            own_ms: None,
            observed_at_ms: 1_200,
            generic: Some(win("rate_limit", 2_000, 1_000)),
        };
        for now in [1_250, 1_500, 2_100] {
            assert_eq!(
                wake(&undated, vec![win("rate_limit", 1_150, 1_100)], now),
                None,
                "elapsed correction at {now}"
            );
        }
        // A correction older than the carried reset does not supersede it.
        assert_eq!(
            wake(&undated, vec![win("rate_limit", 1_150, 900)], 1_250),
            Some((2_000, 1_000, false))
        );
    }

    #[test]
    fn only_a_reported_deadline_in_force_for_the_same_window_stands_in() {
        let reported = |until_ms, kind: &str, estimated| AccountState {
            windows: vec![WindowLimit {
                kind: kind.into(),
                until_ms,
                estimated,
                observed_at_ms: 0,
            }],
            unusable_until_ms: 0,
        };
        // A dated park's own wake is older than the report; an undated one began after it.
        let at_now = |account: &AccountState, kind: &str, any_window: bool, now_ms: i64| {
            let park = ParkView {
                kind,
                own_ms: (!any_window).then_some(0),
                observed_at_ms: if any_window { i64::MAX } else { -1 },
                generic: None,
            };
            wake_deadline(&park, &account.windows, now_ms)
                .filter(|w| !w.own)
                .map(|w| w.at_ms)
        };
        let at =
            |account: AccountState, kind, any_window| at_now(&account, kind, any_window, 1_000);
        assert_eq!(
            at(reported(5_000, "usage", false), "usage", false),
            Some(5_000)
        );
        assert_eq!(
            at(reported(5_000, "weekly", false), "usage", false),
            None,
            "another window"
        );
        assert_eq!(
            at(reported(5_000, "usage", true), "usage", false),
            None,
            "a guess is no deadline"
        );
        assert_eq!(
            at(reported(1_000, "usage", false), "usage", true),
            None,
            "already elapsed, for a park with no reset of its own"
        );
        assert_eq!(
            at(reported(1_000, "usage", false), "usage", false),
            Some(1_000),
            "a dated park still sees a correction that has elapsed"
        );
        let generic_multi_day = 1_000 + 6 * 3600 * 1000;
        assert_eq!(
            at(
                reported(generic_multi_day, "rate_limit", false),
                "five_hour",
                false
            ),
            None,
            "an unnamed deadline a five-hour window cannot reach is not its"
        );
        // The reach is fixed by when the report was observed: a later pass, within five hours
        // of the deadline, still does not take it for a five-hour window's.
        for now_ms in [1_000, generic_multi_day - 3_600_000] {
            assert!(
                at_now(
                    &reported(generic_multi_day, "rate_limit", false),
                    "five_hour",
                    false,
                    now_ms
                )
                .is_none(),
                "pass at {now_ms}"
            );
        }
        // The stored deadline is the raw reset plus the wake grace; the bound is on the reset.
        let grace_ms = i64::from(super::super::RATE_LIMIT_AUTO_RESUME_GRACE_SECS) * 1000;
        let five_hours_ms = FIVE_HOUR_WINDOW_SECS * 1000;
        assert_eq!(
            at(
                reported(five_hours_ms + grace_ms, "rate_limit", false),
                "five_hour",
                false
            ),
            Some(five_hours_ms + grace_ms),
            "a reset exactly five hours out is reachable"
        );
        assert_eq!(
            at(
                reported(five_hours_ms + grace_ms + 1_000, "rate_limit", false),
                "five_hour",
                false
            ),
            None,
            "a reset one second past five hours is not"
        );
        assert_eq!(
            at(
                reported(generic_multi_day, "rate_limit", false),
                "seven_day",
                false
            ),
            Some(generic_multi_day)
        );
        assert_eq!(
            at(reported(5_000, "seven_day", false), "rate_limit", true),
            Some(5_000),
            "a window-less report without a reset of its own takes the account's reported one"
        );
        assert_eq!(
            at(reported(5_000, "seven_day", false), "rate_limit", false),
            None,
            "a window-less report with its own reset stays apart from a named window"
        );
        assert_eq!(
            at(reported(5_000, "rate_limit", false), "seven_day", false),
            Some(5_000),
            "a window-less deadline is compatible with a later named report"
        );
        for (stored, lookup, any_window) in [
            ("rate_limit", "billing_error", true),
            ("seven_day", "billing_error", true),
            ("billing_error", "rate_limit", true),
            ("billing_error", "seven_day", false),
        ] {
            assert_eq!(
                at(reported(5_000, stored, false), lookup, any_window),
                None,
                "billing and quota deadlines stay apart: {stored} vs {lookup}"
            );
        }
        assert_eq!(
            at(
                reported(5_000, "billing_error", false),
                "billing_error",
                true
            ),
            Some(5_000)
        );
        assert_eq!(
            at(
                reported(253_402_300_800_000, "usage", false),
                "usage",
                false
            ),
            Some(253_402_300_800_000),
            "past year 9999"
        );
    }

    #[test]
    fn a_failed_login_is_recognised_in_a_startup_error() {
        assert!(is_auth_failure(
            "AgentStartupError: Authentication required"
        ));
        assert!(is_auth_failure(
            "authentication required: run claude auth login"
        ));
        assert!(!is_auth_failure("spawn failed: capacity full"));
    }

    #[test]
    fn one_failover_per_session() {
        let first = InFlight::claim("s-dup", "b").expect("first claim");
        assert!(
            InFlight::claim("s-dup", "b").is_none(),
            "a second claim is refused"
        );
        assert_eq!(holds("s-dup", 7, Instant::now()), Gate::Wait);
        drop(first);
        assert_eq!(
            holds("s-dup", 7, Instant::now()),
            Gate::Go,
            "the claim is released on drop"
        );
    }

    #[test]
    fn a_transient_failure_backs_off_then_retries_and_a_success_clears_it() {
        let mut retries = Retries::default();
        let t0 = Instant::now();
        let after = |secs| t0 + Duration::from_secs(secs);
        assert_eq!(retries.gate("s", 1, t0), Gate::Go);

        retries.failed("s", 1, false, true, t0);
        assert_eq!(
            retries.gate("s", 1, after(1)),
            Gate::Wait,
            "holds the park while it backs off"
        );
        assert_eq!(retries.gate("s", 1, after(59)), Gate::Wait);
        assert_eq!(
            retries.gate("s", 1, after(60)),
            Gate::Go,
            "retried once the backoff passes"
        );

        // The second failure waits twice as long.
        retries.failed("s", 1, false, true, after(60));
        assert_eq!(retries.gate("s", 1, after(60 + 119)), Gate::Wait);
        assert_eq!(retries.gate("s", 1, after(60 + 120)), Gate::Go);

        retries.succeeded("s");
        assert_eq!(
            retries.gate("s", 1, after(61)),
            Gate::Go,
            "success forgets the failures"
        );
    }

    #[test]
    fn attempts_are_bounded_and_then_the_timed_resume_takes_over() {
        let mut retries = Retries::default();
        let mut now = Instant::now();
        for attempt in 1..=MAX_FAILOVER_ATTEMPTS {
            assert_eq!(retries.gate("s", 1, now), Gate::Go, "attempt {attempt}");
            retries.failed("s", 1, false, true, now);
            now += Duration::from_secs(3600);
        }
        assert_eq!(retries.gate("s", 1, now), Gate::GiveUp);
    }

    #[test]
    fn a_permanent_refusal_stays_latched_for_its_park_only() {
        let mut retries = Retries::default();
        let now = Instant::now();
        retries.failed("s", 1, true, true, now);
        assert_eq!(retries.gate("s", 1, now), Gate::GiveUp);
        assert_eq!(
            retries.gate("s", 1, now + Duration::from_secs(86_400)),
            Gate::GiveUp,
            "time does not clear it"
        );
        assert_eq!(
            retries.gate("s", 2, now),
            Gate::Go,
            "a new park starts clean"
        );
        assert_eq!(
            retries.gate("other", 1, now),
            Gate::Go,
            "other sessions are unaffected"
        );
    }

    #[test]
    fn records_of_sessions_that_went_away_are_dropped() {
        let mut retries = Retries::default();
        let now = Instant::now();
        retries.failed("gone", 1, true, true, now);
        retries.failed(
            "live",
            1,
            false,
            true,
            now + RETRY_RETENTION + Duration::from_secs(61),
        );
        assert_eq!(retries.0.len(), 1);
        assert!(retries.0.contains_key("live"));
    }
}
