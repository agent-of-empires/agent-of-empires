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

/// Bounds hostname resolution. The reqwest timeouts above only start once the
/// client exists, so they do not cover the pre-dispatch lookup, and the
/// dispatch task already holds a `DISPATCH_CONCURRENCY` permit while resolving:
/// without this, callback hosts pointing at unresponsive resolvers would pin
/// every permit and starve callbacks for all other sessions.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Register a transition for `session_id` and return its generation. The
/// newest generation is the one allowed to fire, so a burst within the
/// debounce window collapses to its last member.
fn bump_debounce(session_id: &str) -> u64 {
    let mut guard = debounce_state().lock().unwrap();
    let entry = guard
        .entry(session_id.to_string())
        .or_insert(DebounceEntry { generation: 0 });
    entry.generation = entry.generation.wrapping_add(1);
    entry.generation
}

/// Whether the waking debounce task still owns firing for `session_id`, and
/// if so drop its entry: the map only needs to hold sessions with a debounce
/// window in flight, otherwise it would retain one entry per session id for
/// the daemon's lifetime.
///
/// Check and removal share one lock acquisition on purpose. Releasing between
/// them would let a transition arriving in that gap insert a fresh entry that
/// this call then deletes, stranding that newer task with nothing to claim.
/// A superseded task removes nothing: the newer generation's task owns the
/// entry and will clean it up when it fires.
fn claim_debounce(session_id: &str, generation: u64) -> bool {
    let mut guard = debounce_state().lock().unwrap();
    match guard.get(session_id) {
        Some(entry) if entry.generation == generation => {
            guard.remove(session_id);
            true
        }
        _ => false,
    }
}

/// `Url::host_str()` returns an IPv6 literal bracketed (`"[::1]"`, matching
/// the URL's own syntax); `IpAddr::from_str` rejects the brackets, so strip
/// them before parsing.
fn strip_ipv6_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
}

/// Carrier-grade NAT (RFC 6598). Not covered by `Ipv4Addr::is_private`, but
/// routable to other tenants on a CGNAT network, so it is not a safe callback
/// target. `Ipv4Addr::is_shared` would say this for us but is nightly-only.
fn is_cgnat_v4(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

/// Pull an embedded IPv4 address out of the v6 forms that carry one, so it can
/// be judged by the IPv4 rules instead of sliding past them:
///
/// - `::ffff:a.b.c.d` (IPv4-mapped, RFC 4291) via `to_canonical`
/// - `64:ff9b::a.b.c.d` (NAT64 well-known prefix, RFC 6052)
/// - `::a.b.c.d` (IPv4-compatible, deprecated but still parsed and routable)
///
/// Verified necessary: without this, `64:ff9b::169.254.169.254` and
/// `::169.254.169.254` both cleared every check while still reaching the
/// metadata service.
fn embedded_v4(ip: IpAddr) -> Option<std::net::Ipv4Addr> {
    if let IpAddr::V4(v4) = ip.to_canonical() {
        return Some(v4);
    }
    let IpAddr::V6(v6) = ip else { return None };
    let seg = v6.segments();
    let last_two_as_v4 = || {
        let [a, b] = [seg[6], seg[7]];
        std::net::Ipv4Addr::new(
            (a >> 8) as u8,
            (a & 0xff) as u8,
            (b >> 8) as u8,
            (b & 0xff) as u8,
        )
    };
    // NAT64 well-known prefix 64:ff9b::/96.
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6].iter().all(|s| *s == 0) {
        return Some(last_two_as_v4());
    }
    // IPv4-compatible ::a.b.c.d (excluding :: and ::1, handled as v6 already).
    if seg[..6].iter().all(|s| *s == 0) && (seg[6] != 0 || seg[7] > 1) {
        return Some(last_two_as_v4());
    }
    None
}

fn is_forbidden_v4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_multicast()
        || is_cgnat_v4(v4)
}

/// Whether an IP is inside a range a callback must never reach: loopback,
/// private/link-local/CGNAT space, or unspecified/multicast. Applied both at
/// create-time (a literal-IP `callback_url`) and immediately before every
/// dispatch (the resolved hostname), to block SSRF against cloud metadata
/// endpoints (e.g. 169.254.169.254, link-local) and internal admin surfaces.
///
/// The dispatch path does not re-resolve after this check: the approved
/// addresses are pinned onto the client (`build_pinned_client`), so a DNS
/// rebinding answer cannot redirect the connect to a target this never
/// approved.
fn is_forbidden_target(ip: IpAddr) -> bool {
    // Judge any embedded IPv4 by the IPv4 rules first. `Ipv6Addr::is_loopback()`
    // only matches `::1`, so a mapped/NAT64/compatible loopback or metadata
    // address would otherwise clear every v6 check below while the OS still
    // connects it to the v4 target, defeating the whole guard.
    if let Some(v4) = embedded_v4(ip) {
        return is_forbidden_v4(v4);
    }
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
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
/// `resolve_vetted_addrs`), so a hostname that *later* resolves to a private
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

/// Pre-dispatch guard: resolves the hostname and returns the vetted addresses,
/// or `None` if resolution failed or ANY resolved address is forbidden (fails
/// closed).
///
/// The caller must pin the returned addresses onto the client rather than
/// letting `reqwest` resolve the host again. Checking and then re-resolving is
/// a TOCTOU: hostile DNS can answer with a public address for this check and a
/// loopback/metadata address microseconds later for the connect, so the check
/// would approve one target and the request would reach another.
async fn resolve_vetted_addrs(url: &reqwest::Url) -> Option<Vec<std::net::SocketAddr>> {
    let host = strip_ipv6_brackets(url.host_str()?);
    let port = url.port_or_known_default().unwrap_or(80);
    // Bounded by RESOLVE_TIMEOUT: a hung resolver would otherwise hold this
    // task's dispatch permit indefinitely. Both the timeout and the lookup
    // itself fail closed to `None`.
    let addrs: Vec<std::net::SocketAddr> =
        tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .ok()?
            .ok()?
            .collect();
    if addrs.is_empty() || addrs.iter().any(|a| is_forbidden_target(a.ip())) {
        return None;
    }
    Some(addrs)
}

/// Build the outbound client for one callback dispatch, with DNS pinned to the
/// already-vetted addresses so the connect cannot land anywhere
/// `resolve_vetted_addrs` did not approve. Per-dispatch rather than shared
/// because `resolve_to_addrs` is a builder-level override; callbacks are
/// debounced and per-transition, so giving up pool reuse is far cheaper than
/// leaving the rebinding window open.
fn build_pinned_client(
    host: &str,
    addrs: &[std::net::SocketAddr],
) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, addrs)
        .build()
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
        let semaphore = Arc::new(tokio::sync::Semaphore::new(DISPATCH_CONCURRENCY));
        let mut rx = state.status_tx.subscribe();
        loop {
            tokio::select! {
                recv = rx.recv() => {
                    match recv {
                        Ok(change) => {
                            handle_status_change(state.clone(), semaphore.clone(), change);
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
    semaphore: Arc<tokio::sync::Semaphore>,
    change: StatusChange,
) {
    if !is_fire_worthy(change.new) {
        return;
    }
    let session_id = change.instance_id.clone();
    let generation = bump_debounce(&session_id);

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
        // Superseded by a later transition within the debounce window? That
        // later transition owns firing (or dropping).
        if !claim_debounce(&session_id, generation) {
            return;
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
        let Some(vetted) = resolve_vetted_addrs(&url).await else {
            tracing::warn!(
                target: "http.middleware",
                session_id = %session_id,
                "callback: target resolved to a forbidden address, skipping dispatch"
            );
            return;
        };
        // Pin the vetted addresses so the connect cannot re-resolve to a
        // different host than the one just approved (DNS rebinding).
        let Some(host) = url.host_str().map(strip_ipv6_brackets) else {
            return;
        };
        let client = match build_pinned_client(host, &vetted) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "http.middleware",
                    session_id = %session_id,
                    error = %e,
                    "callback: failed to build pinned client"
                );
                return;
            }
        };

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
            // Other v6 forms carrying an embedded v4 that also has to be
            // judged by the IPv4 rules, or the metadata service stays
            // reachable through them.
            "http://[64:ff9b::169.254.169.254]/latest/meta-data", // NAT64
            "http://[64:ff9b::127.0.0.1]/hook",                   // NAT64 loopback
            "http://[::169.254.169.254]/latest/meta-data",        // IPv4-compatible
            "http://[::10.0.0.5]/hook",                           // IPv4-compatible private
            // Carrier-grade NAT (RFC 6598): routable to other tenants.
            "http://100.64.0.1/hook",
            "http://100.127.255.254/hook",
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
            // A mapped *public* address stays allowed: unwrapping must not
            // over-block, only reclassify.
            "http://[::ffff:203.0.113.5]/hook",
            // 100.64.0.0/10 is CGNAT, but 100.63/100.128 are ordinary public
            // space: the mask must not swallow the neighbours.
            "http://100.63.255.255/hook",
            "http://100.128.0.1/hook",
            // A genuine global v6 address is untouched by the embedded-v4 paths.
            "http://[2606:4700:4700::1111]/hook",
        ];
        for url in cases {
            assert!(validate_callback_url(url).is_ok(), "must accept {url:?}");
        }
    }

    #[tokio::test]
    async fn resolve_vetted_addrs_rejects_localhost_hostname() {
        let url = reqwest::Url::parse("http://localhost/hook").unwrap();
        assert!(resolve_vetted_addrs(&url).await.is_none());
    }

    /// A public hostname yields addresses to pin, and pinning them builds a
    /// usable client. This is what closes the rebinding window: the connect
    /// uses these addresses instead of resolving the name a second time.
    #[tokio::test]
    async fn vetted_addrs_are_pinnable_onto_a_client() {
        let url = reqwest::Url::parse("http://203.0.113.5:8080/hook").unwrap();
        let addrs = resolve_vetted_addrs(&url)
            .await
            .expect("a public literal address must vet clean");
        assert!(!addrs.is_empty());
        let host = strip_ipv6_brackets(url.host_str().unwrap());
        assert!(build_pinned_client(host, &addrs).is_ok());
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

    fn debounce_contains(session_id: &str) -> bool {
        debounce_state().lock().unwrap().contains_key(session_id)
    }

    /// A flicker inside the debounce window collapses to its last transition:
    /// the superseded task must not fire, the newest one must, and the entry
    /// must not outlive the window (it used to be inserted and never removed
    /// outside tests, growing one entry per session id forever).
    #[test]
    fn debounce_collapses_flicker_and_leaves_no_entry() {
        let session_id = "cb-debounce-flicker";
        debounce_state().lock().unwrap().remove(session_id);

        let gen1 = bump_debounce(session_id);
        // A later transition (Waiting -> Running -> Waiting) arrives before
        // the first window elapses.
        let gen2 = bump_debounce(session_id);
        assert_ne!(gen1, gen2);

        // The superseded task loses and must leave the entry for the winner.
        assert!(
            !claim_debounce(session_id, gen1),
            "stale generation must not fire"
        );
        assert!(
            debounce_contains(session_id),
            "a losing task must not strand the winner by removing its entry"
        );

        // The newest task fires exactly once and cleans up after itself.
        assert!(
            claim_debounce(session_id, gen2),
            "newest generation must fire"
        );
        assert!(
            !debounce_contains(session_id),
            "the fired entry must be dropped, or the map grows for the daemon's lifetime"
        );
        assert!(
            !claim_debounce(session_id, gen2),
            "a fired generation must not be claimable twice"
        );
    }
}
