//! IP-based auth failure rate limiting.
//!
//! After 5 failed authentication attempts from an IP within 15 minutes,
//! subsequent requests from that IP are locked out for 15 minutes.
//! State is in-memory only; restarting the server clears all lockouts.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const MAX_FAILURES: u32 = 5;
const LOCKOUT_DURATION: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const WINDOW_DURATION: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_TRACKED_IPS: usize = 10_000;
// Failures within this window of the last recorded failure collapse into one.
// Prevents a single page load's parallel API calls from burning the whole budget
// while still blocking serial brute-force attempts.
//
// Chosen for 500ms because:
// - A browser's parallel fetches on mount land within ~10-50ms, so 500ms is
//   well above the burst window.
// - A scripted serial attacker still hits lockout in 5 * 500ms = 2.5s, which
//   is fast enough that brute-force remains impractical.
// - A human mistyping a passphrase in the login flow waits >500ms between
//   attempts, so each of their failures counts.
const COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

struct FailureRecord {
    count: u32,
    first_failure: Instant,
    last_failure: Instant,
    locked_until: Option<Instant>,
}

pub struct RateLimiter {
    failures: RwLock<HashMap<IpAddr, FailureRecord>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            failures: RwLock::new(HashMap::new()),
        }
    }

    /// Check if an IP is currently locked out. Returns remaining seconds if locked.
    pub async fn check_locked(&self, ip: IpAddr) -> Option<u64> {
        let failures = self.failures.read().await;
        if let Some(record) = failures.get(&ip) {
            if let Some(locked_until) = record.locked_until {
                let now = Instant::now();
                if now < locked_until {
                    let remaining = locked_until.duration_since(now);
                    return Some(remaining.as_secs().max(1));
                }
            }
        }
        None
    }

    /// Record a failed auth attempt. Returns true if this failure triggered a lockout.
    pub async fn record_failure(&self, ip: IpAddr) -> bool {
        let mut failures = self.failures.write().await;
        let now = Instant::now();

        if failures.len() >= MAX_TRACKED_IPS && !failures.contains_key(&ip) {
            return false;
        }

        let record = failures.entry(ip).or_insert(FailureRecord {
            count: 0,
            first_failure: now,
            last_failure: now,
            locked_until: None,
        });

        // If already locked, no-op
        if let Some(locked_until) = record.locked_until {
            if now < locked_until {
                return false;
            }
            // Lockout expired, reset
            record.count = 0;
            record.first_failure = now;
            record.last_failure = now;
            record.locked_until = None;
        }

        // If the failure window expired, reset counter
        if now.duration_since(record.first_failure) > WINDOW_DURATION {
            record.count = 0;
            record.first_failure = now;
        }

        // Coalesce bursts: failures landing within COALESCE_WINDOW of the last
        // recorded failure count as the same attempt. A single page load fires
        // many parallel API calls; without this, one user burns all 5 slots
        // instantly. Serial brute-force is unaffected (attackers pace slower
        // than 500ms/attempt would be pointless).
        if record.count > 0 && now.duration_since(record.last_failure) < COALESCE_WINDOW {
            record.last_failure = now;
            return false;
        }

        record.count += 1;
        record.last_failure = now;

        if record.count >= MAX_FAILURES {
            record.locked_until = Some(now + LOCKOUT_DURATION);
            tracing::warn!(
                target: "auth.rate_limit",
                ip = %ip,
                failures = record.count,
                lockout_secs = LOCKOUT_DURATION.as_secs(),
                "ip locked out after failed auth threshold"
            );
            return true;
        }

        if record.count >= 3 {
            tracing::info!(
                target: "auth.rate_limit",
                ip = %ip,
                failures = record.count,
                max = MAX_FAILURES,
                "auth failures approaching lockout threshold"
            );
        }

        false
    }

    /// Clear failure count for an IP after successful auth.
    pub async fn record_success(&self, ip: IpAddr) {
        let mut failures = self.failures.write().await;
        failures.remove(&ip);
    }

    /// Spawn periodic cleanup task to evict expired entries. The task
    /// exits cleanly when `shutdown` is cancelled, so `aoe serve --stop`
    /// drains the loop within one tick instead of waiting for the
    /// 5 s force exit safety net.
    pub fn spawn_cleanup_task(self: &Arc<Self>, shutdown: CancellationToken) {
        let limiter = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let mut failures = limiter.failures.write().await;
                        let now = Instant::now();
                        failures.retain(|_, record| {
                            // Keep entries that are still locked
                            if let Some(locked_until) = record.locked_until {
                                if now < locked_until {
                                    return true;
                                }
                            }
                            // Keep entries with recent failures (within window)
                            now.duration_since(record.first_failure) < WINDOW_DURATION
                        });
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Record failures with spacing > COALESCE_WINDOW so each counts separately.
    async fn record_spaced(limiter: &RateLimiter, ip: IpAddr) -> bool {
        let result = limiter.record_failure(ip).await;
        tokio::time::sleep(COALESCE_WINDOW + std::time::Duration::from_millis(50)).await;
        result
    }

    #[tokio::test]
    async fn allows_under_limit() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        for _ in 0..4 {
            assert!(!record_spaced(&limiter, ip).await);
        }
        assert!(limiter.check_locked(ip).await.is_none());
    }

    #[tokio::test]
    async fn locks_at_threshold() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        for _ in 0..4 {
            record_spaced(&limiter, ip).await;
        }
        // 5th spaced failure triggers lockout
        assert!(record_spaced(&limiter, ip).await);
        assert!(limiter.check_locked(ip).await.is_some());
    }

    #[tokio::test]
    async fn success_clears_failures() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        for _ in 0..3 {
            record_spaced(&limiter, ip).await;
        }
        limiter.record_success(ip).await;

        // After success, should be able to fail again without lockout
        for _ in 0..4 {
            assert!(!record_spaced(&limiter, ip).await);
        }
    }

    #[tokio::test]
    async fn independent_ips() {
        let limiter = RateLimiter::new();
        let ip_a: IpAddr = "1.2.3.4".parse().unwrap();
        let ip_b: IpAddr = "5.6.7.8".parse().unwrap();

        // Lock out IP A
        for _ in 0..5 {
            record_spaced(&limiter, ip_a).await;
        }
        assert!(limiter.check_locked(ip_a).await.is_some());
        // IP B is unaffected
        assert!(limiter.check_locked(ip_b).await.is_none());
    }

    #[tokio::test]
    async fn burst_failures_coalesce() {
        // A single page load's parallel API calls (all firing within ~milliseconds)
        // must not exhaust the failure budget.
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        // 20 failures in a tight loop should count as 1
        for _ in 0..20 {
            assert!(!limiter.record_failure(ip).await);
        }
        assert!(limiter.check_locked(ip).await.is_none());
    }

    #[tokio::test]
    async fn unlocked_ip_returns_none() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(limiter.check_locked(ip).await.is_none());
    }

    #[tokio::test]
    async fn already_locked_failure_is_noop() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        for _ in 0..5 {
            record_spaced(&limiter, ip).await;
        }
        // Additional failures while locked return false (no new lockout triggered)
        assert!(!limiter.record_failure(ip).await);
    }
}
