//! Opt-in account failover for rate-limit parks.
//!
//! A parked host session whose pool has an account with headroom swaps to it through the
//! restart path's account swap instead of waiting for the reset. The swap itself, its
//! duplicate-prompt guard and the interrupted-prompt continuation live in
//! `lifecycle::failover_account_swap`; this module decides when and where to.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;

use crate::acp::event_store::AccountState;
use crate::acp::state::RateLimitInfo;
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

/// Sessions with a failover running, so a later pass never starts a second one.
static IN_FLIGHT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// Sessions whose failover was refused or failed, with the event-log head it was decided on.
/// The same park is not retried every pass; any new event starts a fresh decision.
static REFUSED: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Mutex::default);

/// Pool entries already warned about, so a misconfigured name logs once.
static WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Failover settings one profile resolves to.
#[derive(Clone, Default)]
pub(super) struct Settings {
    pub(super) enabled: bool,
    pub(super) accounts: Vec<String>,
}

impl Settings {
    pub(super) fn of(config: &crate::session::config::Config) -> Self {
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

fn path_key(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The configured pool entries that run the same built-in agent as `row` and resolve to a root.
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
        let root: Option<PathBuf> =
            crate::session::conversation_carry::host_account_root(row, profile, tool);
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
                        "rate-limit failover: '{tool}' does not run the same agent as '{}' or has no resolvable config root; ignored",
                        row.tool
                    );
                }
            }
        }
    }
    pool
}

/// The first pool account other than the current one that is not known to be limited or
/// logged out. Accounts never seen have headroom.
pub(super) fn select_target<'a>(
    pool: &'a [PoolAccount],
    current_key: &str,
    now_ms: i64,
    state_of: impl Fn(&str) -> Option<AccountState>,
) -> Option<&'a PoolAccount> {
    pool.iter().find(|account| {
        account.key != current_key
            && state_of(&account.key).is_none_or(|state| state.has_headroom(now_ms))
    })
}

/// Writes what this park says about the session's current account.
pub(super) async fn record_limit(
    state: &Arc<AppState>,
    row: &Instance,
    info: &RateLimitInfo,
    limited_until: DateTime<Utc>,
    observed_at_ms: i64,
) {
    let Some(key) = account_key(row) else {
        return;
    };
    let store = Arc::clone(&state.acp_event_store);
    let kind = info.kind.clone();
    let _ = tokio::task::spawn_blocking(move || {
        store.record_account_limit(
            &key,
            limited_until.timestamp_millis(),
            &kind,
            observed_at_ms,
        );
    })
    .await;
}

/// Marks the session's current account unusable when its newest startup error is a failed login.
pub(super) async fn note_auth_failure(state: &Arc<AppState>, row: &Instance) {
    let Some(key) = account_key(row) else {
        return;
    };
    let store = Arc::clone(&state.acp_event_store);
    let id = row.id.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let Some((message, at_ms)) = store.unassigned_startup_error(&id) else {
            return;
        };
        if is_auth_failure(&message) {
            store.record_account_unusable(&key, at_ms + AUTH_FAILED_RETRY_MS, at_ms);
        }
    })
    .await;
}

fn is_auth_failure(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("authentication required")
}

struct InFlight(String);

impl InFlight {
    fn claim(id: &str) -> Option<Self> {
        lock(&IN_FLIGHT)
            .insert(id.to_owned())
            .then(|| Self(id.to_owned()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        lock(&IN_FLIGHT).remove(&self.0);
    }
}

pub(super) fn in_flight(id: &str) -> bool {
    lock(&IN_FLIGHT).contains(id)
}

/// Whether a failover for `id` is running or was just refused on this exact park, so the
/// caller must not fall through to the timed resume for it.
fn holds(id: &str, head_seq: u64) -> Held {
    if in_flight(id) {
        return Held::Running;
    }
    if lock(&REFUSED).get(id) == Some(&head_seq) {
        return Held::Refused;
    }
    Held::No
}

#[derive(Debug, PartialEq, Eq)]
enum Held {
    No,
    /// A failover is mid-flight: leave the session alone.
    Running,
    /// This park already failed to fail over: fall back to the timed resume.
    Refused,
}

/// Starts a failover for the parked `row` when the pool has an account with headroom.
/// Returns `true` when one is running for it (now or from an earlier pass), which means the
/// caller leaves the session alone. `false` falls back to the normal banner and timed resume.
pub(super) async fn maybe_fail_over(
    state: &Arc<AppState>,
    row: &Instance,
    accounts: &[String],
    head_seq: u64,
) -> bool {
    match holds(&row.id, head_seq) {
        Held::Running => return true,
        Held::Refused => return false,
        Held::No => {}
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
    let target = tokio::task::spawn_blocking(move || {
        select_target(&pool, &current_key, now_ms, |key| store.account_state(key))
            .map(|account| account.tool.clone())
    })
    .await
    .ok()
    .flatten();
    let Some(target) = target else {
        return false;
    };
    let Ok(permit) = Arc::clone(&PERMITS).try_acquire_owned() else {
        tracing::debug!(target: "acp.supervisor", session = %row.id, "rate-limit failover: waiting for a free restart slot");
        return false;
    };
    let Some(claim) = InFlight::claim(&row.id) else {
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
            Ok(()) => tracing::info!(
                target: "acp.supervisor",
                session = %id,
                from = %from,
                to = %target,
                "rate-limit failover: continued on another account"
            ),
            Err(error) => {
                lock(&REFUSED).insert(id.clone(), head_seq);
                tracing::warn!(
                    target: "acp.supervisor",
                    session = %id,
                    to = %target,
                    %error,
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
            limited_until_ms,
            window_kind: "usage".into(),
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
            select_target(&pool, current, now, |key| known.get(key).cloned())
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
            select_target(&[], "/a", 0, |_| None).map(|a| &a.tool),
            None,
            "an empty pool selects nothing"
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
    fn one_failover_per_session_and_a_refused_park_is_not_retried() {
        let first = InFlight::claim("s-dup").expect("first claim");
        assert!(
            InFlight::claim("s-dup").is_none(),
            "a second claim is refused"
        );
        assert_eq!(holds("s-dup", 7), Held::Running);
        drop(first);
        assert_eq!(holds("s-dup", 7), Held::No, "the claim is released on drop");

        lock(&REFUSED).insert("s-dup".into(), 7);
        assert_eq!(holds("s-dup", 7), Held::Refused);
        assert_eq!(
            holds("s-dup", 8),
            Held::No,
            "a new event starts a fresh decision"
        );
        lock(&REFUSED).remove("s-dup");
    }

    #[test]
    fn simultaneous_restarts_are_bounded() {
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_FAILOVERS));
        let held: Vec<_> = (0..MAX_CONCURRENT_FAILOVERS)
            .map(|_| {
                Arc::clone(&permits)
                    .try_acquire_owned()
                    .expect("within the bound")
            })
            .collect();
        assert!(Arc::clone(&permits).try_acquire_owned().is_err());
        drop(held);
        assert!(Arc::clone(&permits).try_acquire_owned().is_ok());
    }
}
