//! Per-session HTTP completion callbacks for external work-queue dispatchers.
//!
//! A session created with `callback_url` set receives a fire-and-forget HTTP
//! POST when it transitions into Idle, Waiting, or Error, so a headless
//! dispatcher can react to completion without polling `GET /api/sessions`.
//! Subscribes to the same `state.status_tx` broadcast the web-push consumer
//! (`push.rs`) uses, but applies a short debounce instead of push's
//! dwell+cooldown: a legitimate second Idle a minute later is real signal a
//! dispatcher needs, not noise to suppress, so only sub-second tmux-scrape
//! flicker gets collapsed. The debounce mirrors `status_hooks.rs`'s
//! generation-counter pattern (the TUI's own status-command hooks), ported
//! to async. See #3156.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

use super::push::StatusChange;
use super::AppState;
use crate::session::Status;

/// Debounce window before firing: absorbs sub-second tmux-scrape flicker
/// (Waiting -> Running -> Waiting) without push's 60s cooldown, since a real
/// second Idle a minute later is signal a dispatcher needs, not noise.
const DEBOUNCE_MS: u64 = 500;

/// Bounded concurrency for outbound callback POSTs, mirrors `push.rs`'s
/// `SEND_CONCURRENCY` so a session with a slow/dead callback endpoint can't
/// let outstanding requests grow unbounded.
const DISPATCH_CONCURRENCY: usize = 8;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(10);

static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize)]
struct CallbackPayload {
    session_id: String,
    old_status: &'static str,
    new_status: &'static str,
    at: String,
    /// Per-process monotonic counter (resets on daemon restart) so a
    /// dispatcher can discard an out-of-order delivery caused by network
    /// jitter between two async POSTs.
    seq: u64,
}

#[derive(Debug, Clone, Copy)]
struct DebounceEntry {
    generation: u64,
}

fn debounce_state() -> &'static Mutex<HashMap<String, DebounceEntry>> {
    static STATE: OnceLock<Mutex<HashMap<String, DebounceEntry>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `Url::host_str()` returns an IPv6 literal bracketed (`"[::1]"`, matching
/// the URL's own syntax); `IpAddr::from_str` rejects the brackets, so strip
/// them before parsing.
fn strip_ipv6_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
}

/// Whether an IP is inside a range a callback must never reach: loopback,
/// private/link-local space, or unspecified/multicast. Applied both at
/// create-time (a literal-IP `callback_url`) and immediately before every
/// dispatch (the re-resolved hostname), to block SSRF against cloud
/// metadata endpoints (e.g. 169.254.169.254, link-local) and internal admin
/// surfaces. A DNS-rebinding attacker who flips resolution between the
/// pre-dispatch check and the actual `reqwest` connect has a small residual
/// window; fully closing it needs a custom resolver pinning the checked IP,
/// out of scope here.
fn is_forbidden_target(ip: IpAddr) -> bool {
    // Unwrap `::ffff:<v4>` before classifying. `Ipv6Addr::is_loopback()` only
    // matches `::1`, so a mapped loopback (`::ffff:127.0.0.1`) or a mapped
    // metadata address (`::ffff:169.254.169.254`) would otherwise clear every
    // check below while the OS still connects it to the v4 target, defeating
    // the whole guard. `to_canonical()` leaves a genuine v6 address untouched.
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
        }
    }
}

/// Create-time validation: rejects a bad scheme or a literal forbidden IP.
/// Does not resolve hostnames (that happens per-dispatch in
/// `resolve_is_safe`), so a hostname that *later* resolves to a private
/// address isn't caught here; the pre-dispatch check is the real gate for
/// that case.
pub fn validate_callback_url(raw: &str) -> Result<(), String> {
    let url =
        reqwest::Url::parse(raw).map_err(|e| format!("callback_url is not a valid URL: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("callback_url must be http or https".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "callback_url has no host".to_string())?;
    if let Ok(ip) = strip_ipv6_brackets(host).parse::<IpAddr>() {
        if is_forbidden_target(ip) {
            return Err(
                "callback_url resolves to a loopback/private/link-local address".to_string(),
            );
        }
    }
    Ok(())
}

/// Pre-dispatch guard: re-resolves the hostname and rejects if any resolved
/// address is forbidden. Fails closed (resolution error => reject).
async fn resolve_is_safe(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = strip_ipv6_brackets(host);
    let port = url.port_or_known_default().unwrap_or(80);
    match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            !addrs.is_empty() && addrs.iter().all(|a| !is_forbidden_target(a.ip()))
        }
        Err(_) => false,
    }
}

fn is_fire_worthy(status: Status) -> bool {
    matches!(status, Status::Idle | Status::Waiting | Status::Error)
}

/// Spawn the consumer task. Subscribes to `state.status_tx` and dispatches a
/// debounced HTTP POST to any instance's `callback_url` on a fire-worthy
/// transition. Runs for the lifetime of the server, mirroring
/// `push::spawn_consumer`.
pub fn spawn_consumer(state: Arc<AppState>) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(target: "http.middleware", error = %e, "callback: failed to build reqwest client");
                return;
            }
        };
        let semaphore = Arc::new(tokio::sync::Semaphore::new(DISPATCH_CONCURRENCY));
        let mut rx = state.status_tx.subscribe();
        loop {
            tokio::select! {
                recv = rx.recv() => {
                    match recv {
                        Ok(change) => {
                            handle_status_change(state.clone(), client.clone(), semaphore.clone(), change);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(target: "http.middleware", lagged = n, "callback: consumer lagged, skipped events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::info!(target: "http.middleware", "callback: status channel closed, consumer exiting");
                            return;
                        }
                    }
                }
                _ = state.shutdown.cancelled() => {
                    tracing::info!(target: "http.middleware", "callback: shutdown signaled, consumer exiting");
                    return;
                }
            }
        }
    });
}

fn handle_status_change(
    state: Arc<AppState>,
    client: reqwest::Client,
    semaphore: Arc<tokio::sync::Semaphore>,
    change: StatusChange,
) {
    if !is_fire_worthy(change.new) {
        return;
    }
    let session_id = change.instance_id.clone();
    let generation = {
        let mut guard = debounce_state().lock().unwrap();
        let entry = guard
            .entry(session_id.clone())
            .or_insert(DebounceEntry { generation: 0 });
        entry.generation = entry.generation.wrapping_add(1);
        entry.generation
    };

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
        {
            let guard = debounce_state().lock().unwrap();
            match guard.get(&session_id) {
                Some(entry) if entry.generation == generation => {}
                // Superseded by a later transition within the debounce
                // window; that later transition owns firing (or dropping).
                _ => return,
            }
        }

        let (callback_url, current_status) = {
            let instances = state.instances.read().await;
            match instances.iter().find(|i| i.id == session_id) {
                Some(inst) => (inst.callback_url.clone(), inst.status),
                None => return,
            }
        };
        let Some(callback_url) = callback_url else {
            return;
        };
        // Re-check the CURRENT status (not the event's `new`): the debounce
        // window may have let a further transition land, so only fire for
        // whatever fire-worthy status the session is actually in right now.
        if !is_fire_worthy(current_status) {
            return;
        }

        let Ok(url) = reqwest::Url::parse(&callback_url) else {
            return;
        };
        let Ok(_permit) = semaphore.acquire_owned().await else {
            return;
        };
        if !resolve_is_safe(&url).await {
            tracing::warn!(
                target: "http.middleware",
                session_id = %session_id,
                "callback: target resolved to a forbidden address, skipping dispatch"
            );
            return;
        }

        let payload = CallbackPayload {
            session_id: session_id.clone(),
            // PascalCase, matching the REST `status` field the same dispatcher
            // reads from `GET /api/sessions`; `as_str()` is the lowercase
            // CLI/hook vocabulary and would not compare equal. See #3187.
            old_status: change.old.wire_str(),
            new_status: current_status.wire_str(),
            at: change.at.to_rfc3339(),
            seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
        };
        if let Err(e) = client.post(url).json(&payload).send().await {
            tracing::warn!(
                target: "http.middleware",
                session_id = %session_id,
                error = %e,
                "callback: delivery failed"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_unsafe_callback_urls() {
        let cases = [
            "ftp://example.com/hook",                  // non-http scheme
            "http://127.0.0.1/hook",                   // v4 loopback
            "http://[::1]/hook",                       // v6 loopback
            "http://169.254.169.254/latest/meta-data", // cloud metadata (IMDS)
            "http://10.0.0.5/hook",                    // private
            "http://192.168.1.1/hook",                 // private
            "http://0.0.0.0/hook",                     // unspecified
            // IPv4-mapped IPv6 forms. `Ipv6Addr::is_loopback()` only matches
            // `::1`, so without canonicalization these cleared every check
            // while the OS still dialed the v4 target.
            "http://[::ffff:127.0.0.1]/hook",
            "http://[::ffff:169.254.169.254]/latest/meta-data",
            "http://[::ffff:10.0.0.5]/hook",
            "http://[::ffff:192.168.1.1]/hook",
        ];
        for url in cases {
            assert!(validate_callback_url(url).is_err(), "must reject {url:?}");
        }
    }

    #[test]
    fn validate_accepts_public_callback_urls() {
        let cases = [
            "https://dispatcher.example.com/hook",
            "http://203.0.113.5/hook",
            // A mapped *public* address stays allowed: canonicalization must
            // not over-block, only unwrap.
            "http://[::ffff:203.0.113.5]/hook",
        ];
        for url in cases {
            assert!(validate_callback_url(url).is_ok(), "must accept {url:?}");
        }
    }

    #[tokio::test]
    async fn resolve_is_safe_rejects_localhost_hostname() {
        let url = reqwest::Url::parse("http://localhost/hook").unwrap();
        assert!(!resolve_is_safe(&url).await);
    }

    #[test]
    fn is_fire_worthy_matches_idle_waiting_error_only() {
        assert!(is_fire_worthy(Status::Idle));
        assert!(is_fire_worthy(Status::Waiting));
        assert!(is_fire_worthy(Status::Error));
        assert!(!is_fire_worthy(Status::Running));
        assert!(!is_fire_worthy(Status::Starting));
        assert!(!is_fire_worthy(Status::Stopped));
    }

    #[tokio::test]
    async fn debounce_drops_stale_generation_after_flicker() {
        let session_id = "cb-debounce-flicker".to_string();
        {
            let mut guard = debounce_state().lock().unwrap();
            guard.remove(&session_id);
        }

        // First transition claims generation 1.
        let gen1 = {
            let mut guard = debounce_state().lock().unwrap();
            let entry = guard
                .entry(session_id.clone())
                .or_insert(DebounceEntry { generation: 0 });
            entry.generation = entry.generation.wrapping_add(1);
            entry.generation
        };
        // A second, later transition (e.g. Waiting -> Running -> Waiting)
        // bumps the generation again before the first debounce fires.
        let gen2 = {
            let mut guard = debounce_state().lock().unwrap();
            let entry = guard
                .entry(session_id.clone())
                .or_insert(DebounceEntry { generation: 0 });
            entry.generation = entry.generation.wrapping_add(1);
            entry.generation
        };
        assert_ne!(gen1, gen2);

        // The stale (first) generation must no longer match current state,
        // so its debounce timer would drop rather than fire.
        let guard = debounce_state().lock().unwrap();
        let current = guard.get(&session_id).unwrap();
        assert_eq!(current.generation, gen2);
        assert_ne!(current.generation, gen1);
    }
}
