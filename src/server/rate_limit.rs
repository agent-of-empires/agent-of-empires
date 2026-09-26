//! IP-based auth failure rate limiting.

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
const COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// Which failed-auth budget an attempt spends. Each budget is cleared by the
/// credential it guards: a valid token or session clears the token budget, a
/// verified passphrase clears the passphrase one. Sharing a counter would let
/// a bound device reset an attacker's passphrase budget on a shared IP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthBudget {
    /// Rejected or missing token / login session.
    Token,
    /// Rejected passphrase on login or step-up elevation.
    Passphrase,
}

struct FailureRecord {
    count: u32,
    first_failure: Instant,
    last_failure: Instant,
    locked_until: Option<Instant>,
}

pub struct RateLimiter {
    failures: RwLock<HashMap<(IpAddr, AuthBudget), FailureRecord>>,
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

    /// Check if an IP is currently locked out of a budget. Returns
    /// remaining seconds if locked.
    pub async fn check_locked(&self, ip: IpAddr, budget: AuthBudget) -> Option<u64> {
        let failures = self.failures.read().await;
        if let Some(record) = failures.get(&(ip, budget)) {
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

    /// Check every budget this IP spends: a request authenticated by
    /// neither credential is rejected once either budget is exhausted.
    pub async fn check_locked_any(&self, ip: IpAddr) -> Option<u64> {
        let failures = self.failures.read().await;
        let now = Instant::now();
        failures
            .iter()
            .filter(|((budget_ip, _), _)| *budget_ip == ip)
            .filter_map(|(_, record)| record.locked_until)
            .filter(|locked_until| now < *locked_until)
            .map(|locked_until| locked_until.duration_since(now).as_secs().max(1))
            .max()
    }

    /// Record a failed auth attempt. Returns true if this failure triggered a lockout.
    pub async fn record_failure(&self, ip: IpAddr, budget: AuthBudget) -> bool {
        let mut failures = self.failures.write().await;
        Self::record_failure_at(&mut failures, (ip, budget), Instant::now())
    }

    fn record_failure_at(
        failures: &mut HashMap<(IpAddr, AuthBudget), FailureRecord>,
        key: (IpAddr, AuthBudget),
        now: Instant,
    ) -> bool {
        let ip = key.0;
        if failures.len() >= MAX_TRACKED_IPS && !failures.contains_key(&key) {
            // The table is bounded, so a new IP has to displace one. Dropping
            // the newcomer instead (the previous behavior) left it entirely
            // untracked: an attacker who filled the table with distinct
            // source addresses then got unlimited auth attempts from any
            // address not already in it. Evict instead — a locked record is
            // chosen only once every other entry is locked too, so an active
            // lockout is never lifted while there is any other room.
            let victim = failures
                .iter()
                .min_by_key(|(_, record)| (record.locked_until.is_some(), record.last_failure))
                .map(|(victim, _)| *victim);
            match victim {
                Some(victim) => {
                    failures.remove(&victim);
                }
                None => return false,
            }
        }

        let record = failures.entry(key).or_insert(FailureRecord {
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

        // Coalesce bursts.
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

    /// Clear the failure count an IP spent on one budget after the
    /// credential that budget guards was verified. A budget is only ever
    /// cleared by its own success: clearing the token budget proves
    /// nothing about a passphrase guess.
    pub async fn record_success(&self, ip: IpAddr, budget: AuthBudget) {
        let mut failures = self.failures.write().await;
        failures.remove(&(ip, budget));
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

    /// Record the `n`th failure `n` coalesce windows after `base`, so each counts
    /// separately.
    async fn record_spaced(
        limiter: &RateLimiter,
        ip: IpAddr,
        budget: AuthBudget,
        base: Instant,
        n: u32,
    ) -> bool {
        let mut failures = limiter.failures.write().await;
        RateLimiter::record_failure_at(&mut failures, (ip, budget), base + COALESCE_WINDOW * 2 * n)
    }

    const TOKEN: AuthBudget = AuthBudget::Token;
    const PASSPHRASE: AuthBudget = AuthBudget::Passphrase;

    /// The lockout arms on the `MAX_FAILURES`'th spaced failure and not before; further
    /// failures while locked arm nothing new, and another peer is untouched.
    #[tokio::test]
    async fn lockout_arms_at_the_threshold_and_stays_per_ip() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        let other: IpAddr = "5.6.7.8".parse().unwrap();
        let base = Instant::now();
        assert!(limiter.check_locked(ip, TOKEN).await.is_none());

        for n in 0..MAX_FAILURES - 1 {
            assert!(!record_spaced(&limiter, ip, TOKEN, base, n).await);
        }
        assert!(limiter.check_locked(ip, TOKEN).await.is_none());
        assert!(
            record_spaced(&limiter, ip, TOKEN, base, MAX_FAILURES).await,
            "the threshold arms it"
        );
        assert!(limiter.check_locked(ip, TOKEN).await.is_some());
        assert!(!limiter.record_failure(ip, TOKEN).await, "already locked");
        assert!(limiter.check_locked(other, TOKEN).await.is_none());
        assert!(
            limiter.check_locked(other, PASSPHRASE).await.is_none(),
            "another peer's passphrase budget is separate"
        );
    }

    #[tokio::test]
    async fn success_clears_only_its_own_budget() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        let spent = 3;
        let base = Instant::now();

        for n in 0..spent {
            record_spaced(&limiter, ip, TOKEN, base, n).await;
            record_spaced(&limiter, ip, PASSPHRASE, base, n).await;
        }
        // A valid token or an authenticated session proves nothing about a
        // passphrase guess, so it must not reset that budget.
        limiter.record_success(ip, TOKEN).await;
        for n in spent..MAX_FAILURES - 1 {
            assert!(
                !record_spaced(&limiter, ip, PASSPHRASE, base, n).await,
                "the passphrase budget survived a token success"
            );
        }
        assert!(
            record_spaced(&limiter, ip, PASSPHRASE, base, MAX_FAILURES - 1).await,
            "the passphrase budget still reaches its own threshold"
        );
        assert!(limiter.check_locked(ip, PASSPHRASE).await.is_some());
        assert!(
            limiter.check_locked_any(ip).await.is_some(),
            "a spent budget locks the IP out of every credential"
        );

        // A verified passphrase is the one success that clears that budget.
        limiter.record_success(ip, PASSPHRASE).await;
        assert!(limiter.check_locked(ip, PASSPHRASE).await.is_none());
        assert!(limiter.check_locked_any(ip).await.is_none());
        for n in MAX_FAILURES..MAX_FAILURES * 2 - 1 {
            assert!(
                !record_spaced(&limiter, ip, PASSPHRASE, base, n).await,
                "a cleared budget counts again from zero"
            );
        }
    }

    #[tokio::test]
    async fn burst_failures_coalesce() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        let now = Instant::now();
        let mut failures = limiter.failures.write().await;
        for _ in 0..20 {
            assert!(!RateLimiter::record_failure_at(
                &mut failures,
                (ip, TOKEN),
                now
            ));
        }
        for attempt in 1..MAX_FAILURES {
            assert_eq!(
                RateLimiter::record_failure_at(
                    &mut failures,
                    (ip, TOKEN),
                    now + COALESCE_WINDOW * attempt,
                ),
                attempt == MAX_FAILURES - 1,
                "the burst consumes exactly one attempt, not zero or twenty",
            );
        }
    }

    /// A full table must not turn a new source address into an unlimited one:
    /// the entry is admitted by displacing the least recently active record.
    ///
    /// Driven through `record_failure_at` with synthetic instants so the
    /// 10 000-entry table costs no wall-clock time.
    #[test]
    fn a_full_table_still_tracks_a_new_ip() {
        let mut failures = HashMap::new();
        let base = Instant::now();
        let ip_at = |n: usize| -> IpAddr {
            format!("10.{}.{}.{}", (n / 65536) % 256, (n / 256) % 256, n % 256)
                .parse()
                .unwrap()
        };
        for n in 0..MAX_TRACKED_IPS {
            RateLimiter::record_failure_at(
                &mut failures,
                (ip_at(n), TOKEN),
                base + COALESCE_WINDOW * n as u32,
            );
        }
        let fresh: IpAddr = "203.0.113.9".parse().unwrap();
        RateLimiter::record_failure_at(&mut failures, (fresh, TOKEN), base);

        assert!(
            failures.contains_key(&(fresh, TOKEN)),
            "a new IP must be tracked, not silently dropped"
        );
        assert!(
            failures.len() <= MAX_TRACKED_IPS,
            "the table must stay bounded, got {}",
            failures.len()
        );
    }

    /// Eviction must not lift an active lockout while any unlocked record can
    /// be displaced instead.
    #[test]
    fn a_saturated_table_evicts_an_idle_record_before_a_locked_one() {
        let mut failures = HashMap::new();
        let base = Instant::now();
        let locked: IpAddr = "198.51.100.7".parse().unwrap();
        for attempt in 0..MAX_FAILURES {
            RateLimiter::record_failure_at(
                &mut failures,
                (locked, TOKEN),
                base + COALESCE_WINDOW * attempt,
            );
        }
        assert_eq!(
            failures.get(&(locked, TOKEN)).and_then(|r| r.locked_until),
            Some(base + COALESCE_WINDOW * (MAX_FAILURES - 1) + LOCKOUT_DURATION),
            "the fixture IP must be locked out before the table fills"
        );
        for n in 0..MAX_TRACKED_IPS {
            RateLimiter::record_failure_at(
                &mut failures,
                (
                    format!("10.{}.{}.{}", (n / 65536) % 256, (n / 256) % 256, n % 256)
                        .parse()
                        .unwrap(),
                    TOKEN,
                ),
                base + COALESCE_WINDOW * (n as u32 + 1),
            );
        }
        let fresh: IpAddr = "203.0.113.10".parse().unwrap();
        RateLimiter::record_failure_at(&mut failures, (fresh, TOKEN), base);

        assert!(
            failures.contains_key(&(locked, TOKEN)),
            "an active lockout must survive an eviction"
        );
        assert!(failures.contains_key(&(fresh, TOKEN)));
    }
}
