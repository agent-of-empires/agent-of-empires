//! Opt-in account failover for rate-limit parks.
//!
//! A parked host session whose pool has an account with headroom swaps to it through the
//! restart path's account swap instead of waiting for the reset. The swap itself, its
//! duplicate-prompt guard and the interrupted-prompt continuation live in
//! `lifecycle::failover_account_swap`; this module decides when and where to.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;

use crate::acp::event_store::AccountState;
#[cfg(test)]
use crate::acp::event_store::WindowLimit;
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

/// Aliases of one directory (a symlink, `..`) are one account; a directory that does not exist
/// yet keeps its path as written.
fn path_key(path: &std::path::Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
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
                Ok(state) => state.is_none_or(|state| state.has_headroom(now_ms)),
                Err(error) => {
                    tracing::warn!(
                        target: "acp.supervisor",
                        account = %account.tool,
                        %error,
                        "rate-limit failover: account state unreadable; not a target"
                    );
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
    limited_until: DateTime<Utc>,
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
            limited_until.timestamp_millis(),
            &kind,
            observed_at_ms,
            estimated,
        );
    })
    .await;
}

/// The deadline already reported for the session's account and this park's window, when the
/// park itself carries none: a session that started after the reset was announced never saw it.
/// Only a reported (not estimated) deadline still in the future counts.
pub(super) async fn reported_deadline(
    state: &Arc<AppState>,
    row: &Instance,
    info: &RateLimitInfo,
) -> Option<DateTime<Utc>> {
    if info.resets_at.is_some() {
        return None;
    }
    let key = observed_account_key(state, row).await?;
    let store = Arc::clone(&state.acp_event_store);
    let account = tokio::task::spawn_blocking(move || store.account_state(&key))
        .await
        .ok()
        .flatten()?;
    in_force_deadline(&account, &info.kind, Utc::now().timestamp_millis())
}

fn in_force_deadline(account: &AccountState, kind: &str, now_ms: i64) -> Option<DateTime<Utc>> {
    let window = account.window(kind)?;
    (!window.estimated && window.until_ms > now_ms)
        .then(|| DateTime::from_timestamp_millis(window.until_ms))
        .flatten()
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
            until.timestamp_millis(),
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

/// Marks a failover to `target` as running for `id` until the returned guard drops.
#[cfg(test)]
pub(super) fn hold_in_flight(id: &str, target: &str) -> impl Sized {
    InFlight::claim(id, target).expect("no failover is running for the session")
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

/// The failed attempts recorded for `id`'s park, if any were made.
#[cfg(test)]
pub(super) fn attempts_made(id: &str, park: i64) -> Option<u32> {
    lock(&RETRIES)
        .0
        .get(id)
        .filter(|retry| retry.park == park)
        .map(|retry| retry.failures)
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
    let now_ms = Utc::now().timestamp_millis();
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
    let now_ms = Utc::now().timestamp_millis();
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
    tokio::spawn(async move {
        let _permit = permit;
        let _claim = claim;
        match crate::server::api::sessions::failover_account_swap(&state, &id, &target, head_seq)
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

    /// A login failure the target's launch reported is recorded against the target, but only
    /// when it came after the attempt started, not from an earlier launch.
    #[test]
    fn a_login_failure_after_the_attempt_marks_the_target_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            crate::acp::event_store::EventStore::open(&dir.path().join("events.db"), 100).unwrap();
        store
            .record(
                "s",
                1,
                &crate::acp::state::Event::AgentStartupError {
                    message: "Authentication required".into(),
                },
            )
            .unwrap();
        let now = Utc::now().timestamp_millis();

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

    /// B failed to log in at `initialize`, which leaves no startup error in the session's log:
    /// the next pass must still pass over B and take C.
    #[test]
    fn a_login_failure_the_launch_reported_sends_the_next_pass_to_the_next_account() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            crate::acp::event_store::EventStore::open(&dir.path().join("events.db"), 100).unwrap();
        let pool = [account("b", "/b"), account("c", "/c")];
        let now = Utc::now().timestamp_millis();
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

    /// A limit B's launch reported is B's: it holds B for the window, leaves A untouched, and the
    /// next pass takes C; with C limited too nothing is left.
    #[test]
    fn a_rate_limit_the_launch_reported_is_recorded_against_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            crate::acp::event_store::EventStore::open(&dir.path().join("events.db"), 100).unwrap();
        let pool = [account("b", "/b"), account("c", "/c")];
        let now = Utc::now().timestamp_millis();
        let limited = |kind: &str, resets_in_ms: Option<i64>| FailoverError {
            permanent: false,
            target_auth_failed: false,
            target_rate_limit: Some(RateLimitInfo {
                status: "limited".into(),
                resets_at: resets_in_ms.and_then(|ms| DateTime::from_timestamp_millis(now + ms)),
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
            missing.to_string_lossy(),
            "a directory that does not exist keeps its path"
        );
    }

    #[test]
    fn only_a_reported_deadline_in_force_for_the_same_window_stands_in() {
        let reported = |until_ms, kind: &str, estimated| AccountState {
            windows: vec![WindowLimit {
                kind: kind.into(),
                until_ms,
                estimated,
            }],
            unusable_until_ms: 0,
        };
        let at = |account: AccountState, kind| in_force_deadline(&account, kind, 1_000);
        assert_eq!(
            at(reported(5_000, "usage", false), "usage").map(|d| d.timestamp_millis()),
            Some(5_000)
        );
        assert_eq!(
            at(reported(5_000, "weekly", false), "usage"),
            None,
            "another window"
        );
        assert_eq!(
            at(reported(5_000, "usage", true), "usage"),
            None,
            "a guess is no deadline"
        );
        assert_eq!(
            at(reported(1_000, "usage", false), "usage"),
            None,
            "already elapsed"
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
