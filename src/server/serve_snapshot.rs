//! The periodic telemetry snapshot: what the daemon counts while it serves
//! and how those counters are cleared once reported.

use std::sync::Arc;

use super::state::AppState;

/// Background task: emit an opt-in telemetry `usage_snapshot` immediately and
/// every ~4 hours (jittered), plus a final one on graceful shutdown. The boot
/// `process_start` is emitted separately by the caller before transport setup.
/// All sends are best-effort and swallow errors; nothing leaves the box unless
/// the user opted in and an endpoint is configured.
pub(super) fn spawn_serve_snapshot_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        // Jittered period (4h + up to 30m) so installs that boot together don't
        // snapshot in lockstep; the first tick is still immediate (boot
        // snapshot). `Delay` avoids a burst of catch-up ticks after a stall.
        let mut interval = tokio::time::interval(crate::telemetry::snapshot_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Sample the live session list more often than we send, folding each
        // sample into a window aggregate so short-lived sessions' agent/model
        // mix and the concurrency peak survive into the periodic snapshot (#1870).
        // Both tickers share this one task, so a sample tick and a flush tick
        // never run concurrently: the aggregate needs no locking and a plain
        // reset after a confirmed send is race-free. `Skip` so a long suspend
        // does not fire a run of catch-up samples on wake.
        let mut sample = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
        sample.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut aggregator = crate::telemetry::aggregate::UsageAggregator::default();
        loop {
            tokio::select! {
                _ = state.shutdown.cancelled() => {
                    // Deduped: a serve process that starts and stops between
                    // periodic ticks would otherwise emit the initial first-tick
                    // snapshot and an identical shutdown snapshot seconds apart.
                    // We exit after this, so the aggregate is dropped; no reset.
                    if let Some(snapshot) = build_serve_snapshot(&state, &mut aggregator).await {
                        let outcome = crate::telemetry::flush_snapshot_if_changed(snapshot).await;
                        clear_reported_serve_signals(&state, outcome);
                    }
                    break;
                }
                _ = interval.tick() => {
                    if let Some(snapshot) = build_serve_snapshot(&state, &mut aggregator).await {
                        // Awaited (not detached) so the reported signals are
                        // cleared only after a confirmed send. A failed send
                        // retains the usage_seen counts / the create counter
                        // for the next snapshot instead of dropping them.
                        let outcome = if crate::telemetry::send_snapshot(snapshot).await {
                            crate::telemetry::SendOutcome::Sent
                        } else {
                            crate::telemetry::SendOutcome::Failed
                        };
                        clear_reported_serve_signals(&state, outcome);
                        // Reset the window only after a confirmed send, mirroring
                        // the signal-clear discipline: a failed send keeps the
                        // aggregate so the next flush re-reports the full window.
                        if outcome == crate::telemetry::SendOutcome::Sent {
                            aggregator = crate::telemetry::aggregate::UsageAggregator::default();
                        }
                    }
                }
                _ = sample.tick() => {
                    let instances = state.instances.read().await.clone();
                    aggregator.sample(&instances);
                }
            }
        }
    });
}

/// Per-form-factor open counters for one web surface (dashboard or acp).
/// A fixed, lock-free set over the closed [`crate::telemetry::WebClientFormFactor`]
/// allowlist: the seen endpoint increments the matching class, the snapshot
/// reads exact counts, and a confirmed send decrements by exactly what it
/// reported (so an open landing during an in-flight send survives, mirroring
/// the coarse `telemetry_web_seen` counter). Named fields rather than a map so
/// no free-form string key can ever enter daemon state.
#[derive(Default)]
pub struct FormFactorCounters {
    desktop: std::sync::atomic::AtomicU32,
    desktop_pwa: std::sync::atomic::AtomicU32,
    mobile: std::sync::atomic::AtomicU32,
    mobile_pwa: std::sync::atomic::AtomicU32,
}

impl FormFactorCounters {
    fn field(&self, ff: crate::telemetry::WebClientFormFactor) -> &std::sync::atomic::AtomicU32 {
        use crate::telemetry::WebClientFormFactor::*;
        match ff {
            Desktop => &self.desktop,
            DesktopPwa => &self.desktop_pwa,
            Mobile => &self.mobile,
            MobilePwa => &self.mobile_pwa,
        }
    }

    /// Record one classified open of the given client class.
    pub fn increment(&self, ff: crate::telemetry::WebClientFormFactor) {
        self.field(ff)
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Point-in-time read of every class count, so the snapshot loop can later
    /// decrement by exactly the values it reported.
    fn read(&self) -> FormFactorCounts {
        use std::sync::atomic::Ordering;
        let mut counts = FormFactorCounts::default();
        for ff in crate::telemetry::WebClientFormFactor::ALL {
            counts.set(ff, self.field(ff).load(Ordering::Relaxed));
        }
        counts
    }

    /// Subtract exactly the reported counts after a confirmed send. Never zeroes,
    /// so an open that landed mid-send rolls into the next snapshot.
    fn decrement(&self, reported: &FormFactorCounts) {
        for ff in crate::telemetry::WebClientFormFactor::ALL {
            let n = reported.get(ff);
            if n > 0 {
                self.field(ff)
                    .fetch_sub(n, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// A snapshot's reported per-class counts. Plain values (not atomics) so they
/// can be stashed in [`ReportedServeSignals`] and replayed on confirm.
#[derive(Default, Clone, Copy)]
pub(super) struct FormFactorCounts {
    desktop: u32,
    desktop_pwa: u32,
    mobile: u32,
    mobile_pwa: u32,
}

impl FormFactorCounts {
    fn slot(&mut self, ff: crate::telemetry::WebClientFormFactor) -> &mut u32 {
        use crate::telemetry::WebClientFormFactor::*;
        match ff {
            Desktop => &mut self.desktop,
            DesktopPwa => &mut self.desktop_pwa,
            Mobile => &mut self.mobile,
            MobilePwa => &mut self.mobile_pwa,
        }
    }

    fn set(&mut self, ff: crate::telemetry::WebClientFormFactor, n: u32) {
        *self.slot(ff) = n;
    }

    fn get(&self, ff: crate::telemetry::WebClientFormFactor) -> u32 {
        match ff {
            crate::telemetry::WebClientFormFactor::Desktop => self.desktop,
            crate::telemetry::WebClientFormFactor::DesktopPwa => self.desktop_pwa,
            crate::telemetry::WebClientFormFactor::Mobile => self.mobile,
            crate::telemetry::WebClientFormFactor::MobilePwa => self.mobile_pwa,
        }
    }

    /// Per-class was-seen map for the snapshot wire: only classes with a
    /// positive count appear, each as `true`. Empty (and so omitted) when no
    /// classified client opened the surface.
    fn seen_map(&self) -> std::collections::BTreeMap<String, bool> {
        let mut map = std::collections::BTreeMap::new();
        for ff in crate::telemetry::WebClientFormFactor::ALL {
            if self.get(ff) > 0 {
                map.insert(ff.key().to_string(), true);
            }
        }
        map
    }
}

/// Daemon-side structured-interaction tallies for the next opt-in snapshot. Each
/// is a monotonic `AtomicU32` consumed with the same decrement-by-reported
/// discipline as `telemetry_*_seen`, so an interaction that lands during an
/// in-flight send rolls into the next snapshot instead of being dropped.
///
/// `plan_mode_seen` is a counter rather than a flag for the same reason: the
/// snapshot reports the boolean `count > 0`, but consuming it by subtracting
/// the reported amount keeps a plan-mode entry that arrived mid-send.
#[derive(Default)]
pub struct StructuredTelemetryCounters {
    pub approvals_allow: std::sync::atomic::AtomicU32,
    pub approvals_allow_always: std::sync::atomic::AtomicU32,
    pub approvals_deny: std::sync::atomic::AtomicU32,
    pub agent_switches: std::sync::atomic::AtomicU32,
    pub plan_mode_seen: std::sync::atomic::AtomicU32,
    pub prompts_queued: std::sync::atomic::AtomicU32,
}

/// What a serve snapshot reported, so the originating signals can be cleared
/// only after the send is confirmed. The clear is deferred (rather than reset at
/// build time) so a failed send retains the signals for the next snapshot.
pub(super) struct ReportedServeSignals {
    usage_seen: std::collections::BTreeMap<String, u32>,
    web_clients: FormFactorCounts,
    structured_clients: FormFactorCounts,
    session_creates: u32,
    acp: ReportedAcpCounts,
}

/// The raw `AtomicU32` values a snapshot folded in, kept so each can be
/// decremented by exactly the reported amount on a confirmed send. `plan_mode`
/// is the raw count (not the reported boolean) so a plan-mode entry that
/// arrived mid-send is preserved rather than wiped.
#[derive(Default, Clone, Copy)]
pub(super) struct ReportedAcpCounts {
    approvals_allow: u32,
    approvals_allow_always: u32,
    approvals_deny: u32,
    agent_switches: u32,
    plan_mode: u32,
    prompts_queued: u32,
}

/// Build a serve `usage_snapshot` from the live session list, folding in the
/// `usage_seen` open counts and the session-create trend counter *without
/// resetting them*. The reported counts are stashed in `AppState` so
/// [`clear_reported_serve_signals`] can subtract exactly what was reported once
/// the send is confirmed. Returns `None` when telemetry is not opted in.
///
/// The live read is also folded into `aggregator` as the flush-moment sample,
/// then the window's peak concurrency and distinct-sessions-seen maps override
/// the point-in-time defaults `build_usage_snapshot` produced (#1870). The
/// point-in-time `session_total` and status/sandbox/yolo/acp counts keep
/// their instant-of-flush meaning.
pub(super) async fn build_serve_snapshot(
    state: &AppState,
    aggregator: &mut crate::telemetry::aggregate::UsageAggregator,
) -> Option<crate::telemetry::UsageSnapshot> {
    use std::sync::atomic::Ordering;
    let usage_seen = state.telemetry_usage_seen.snapshot();
    let web_clients = state.telemetry_web_clients.read();
    let structured_clients = state.telemetry_structured_clients.read();
    let session_creates = state.telemetry_session_creates.load(Ordering::Relaxed);
    let c = &state.telemetry_structured;
    let reported_acp = ReportedAcpCounts {
        approvals_allow: c.approvals_allow.load(Ordering::Relaxed),
        approvals_allow_always: c.approvals_allow_always.load(Ordering::Relaxed),
        approvals_deny: c.approvals_deny.load(Ordering::Relaxed),
        agent_switches: c.agent_switches.load(Ordering::Relaxed),
        plan_mode: c.plan_mode_seen.load(Ordering::Relaxed),
        prompts_queued: c.prompts_queued.load(Ordering::Relaxed),
    };
    let acp = crate::telemetry::StructuredInteractionCounts {
        approvals_allow: reported_acp.approvals_allow,
        approvals_allow_always: reported_acp.approvals_allow_always,
        approvals_deny: reported_acp.approvals_deny,
        agent_switches: reported_acp.agent_switches,
        plan_mode_seen: reported_acp.plan_mode > 0,
        prompts_queued: reported_acp.prompts_queued,
    };
    let instances = state.instances.read().await.clone();
    aggregator.sample(&instances);
    let mut snapshot = crate::telemetry::build_usage_snapshot(
        crate::telemetry::Surface::Serve,
        &instances,
        usage_seen.clone(),
        session_creates,
        Some(state.auth_mode),
        Some(state.serve_mode),
        &acp,
    )?;
    // Layer the per-form-factor was-seen maps onto the snapshot. They are serve
    // only (the browser surfaces), so the pure builder leaves them empty and the
    // daemon fills them here from its client counters.
    snapshot.web_clients_seen = web_clients.seen_map();
    snapshot.structured_clients_seen = structured_clients.seen_map();
    snapshot.peak_concurrent_sessions = aggregator.peak_concurrent_sessions();
    snapshot.distinct_sessions_by_agent = aggregator.distinct_by_agent();
    snapshot.distinct_sessions_by_model_bucket = aggregator.distinct_by_model();
    *state.telemetry_last_reported.lock().unwrap() = Some(ReportedServeSignals {
        usage_seen,
        web_clients,
        structured_clients,
        session_creates,
        acp: reported_acp,
    });
    Some(snapshot)
}

/// Clear the signals a serve snapshot reported, but only when the send was
/// confirmed (`SendOutcome::Sent`). On `Deduped` the prior confirmed send
/// already cleared them; on `Failed` they are retained so the next snapshot
/// re-reports them. Every signal (the `usage_seen` open counts and the create
/// counter) is decremented by exactly what was reported, not reset to 0, so an
/// open or a create that landed during the in-flight send survives into the
/// next snapshot instead of being cleared away.
pub(super) fn clear_reported_serve_signals(
    state: &AppState,
    outcome: crate::telemetry::SendOutcome,
) {
    let Some(reported) = state.telemetry_last_reported.lock().unwrap().take() else {
        return;
    };
    if outcome != crate::telemetry::SendOutcome::Sent {
        return;
    }
    state.telemetry_usage_seen.decrement(&reported.usage_seen);
    state.telemetry_web_clients.decrement(&reported.web_clients);
    state
        .telemetry_structured_clients
        .decrement(&reported.structured_clients);
    decrement_reported_count(&state.telemetry_session_creates, reported.session_creates);
    let c = &state.telemetry_structured;
    let rc = reported.acp;
    decrement_reported_count(&c.approvals_allow, rc.approvals_allow);
    decrement_reported_count(&c.approvals_allow_always, rc.approvals_allow_always);
    decrement_reported_count(&c.approvals_deny, rc.approvals_deny);
    decrement_reported_count(&c.agent_switches, rc.agent_switches);
    decrement_reported_count(&c.plan_mode_seen, rc.plan_mode);
    decrement_reported_count(&c.prompts_queued, rc.prompts_queued);
}

/// Decrement a reported telemetry counter by exactly `reported`, never by more.
/// Subtracting the reported amount rather than `swap(0)` preserves any
/// increments (a create, or a web/acp open, or an acp interaction) that
/// landed between the snapshot build and the confirmed send, so they roll into
/// the next snapshot instead of being dropped. A no-op when nothing was
/// reported.
///
/// The snapshot loop is the sole consumer and runs strictly sequentially (each
/// send is awaited, then cleared, before the next build), so the counter can
/// never go below `reported`. The subtraction saturates anyway as cheap
/// insurance against a future refactor that detaches sends, which would
/// otherwise be able to underflow-wrap the `AtomicU32`.
pub(super) fn decrement_reported_count(counter: &std::sync::atomic::AtomicU32, reported: u32) {
    if reported == 0 {
        return;
    }
    use std::sync::atomic::Ordering;
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(reported))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // #1874 / #1875: a confirmed snapshot clears a reported telemetry counter
    // (the create counter and the web/acp open counts all share this path)
    // by exactly the value it reported, so an increment that arrives during the
    // in-flight send survives into the next snapshot instead of being reset away.
    #[test]
    fn reported_count_decrement_preserves_concurrent_increments() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(5);
        // The snapshot reported the 5 increments seen at build time.
        let reported = counter.load(Ordering::Relaxed);
        // One more lands while the snapshot is in flight.
        counter.fetch_add(1, Ordering::Relaxed);
        // The confirmed send clears only what it reported.
        decrement_reported_count(&counter, reported);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "the increment that arrived during the send must be retained"
        );
    }

    #[test]
    fn reported_count_decrement_is_noop_for_zero() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(3);
        decrement_reported_count(&counter, 0);
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    // #1883: the per-form-factor counters dedup repeated same-class opens to a
    // single was-seen entry, and the confirmed-send decrement subtracts exactly
    // what was reported so a class opened during an in-flight send survives.
    #[test]
    fn form_factor_counters_dedup_and_preserve_in_flight_opens() {
        use crate::telemetry::WebClientFormFactor::{Desktop, MobilePwa};

        let counters = FormFactorCounters::default();
        // Two desktop opens and one mobile-PWA open before the snapshot builds.
        counters.increment(Desktop);
        counters.increment(Desktop);
        counters.increment(MobilePwa);

        let reported = counters.read();
        // Repeated same-class pings collapse to one was-seen entry on the wire.
        let map = reported.seen_map();
        assert_eq!(map.get("desktop"), Some(&true));
        assert_eq!(map.get("mobile_pwa"), Some(&true));
        assert_eq!(map.get("mobile"), None, "unseen classes are absent");
        assert_eq!(map.len(), 2);

        // A mobile-PWA open lands while the snapshot is in flight.
        counters.increment(MobilePwa);
        // The confirmed send clears only the reported counts.
        counters.decrement(&reported);

        let after = counters.read();
        assert_eq!(after.get(Desktop), 0, "reported desktop opens cleared");
        assert_eq!(
            after.get(MobilePwa),
            1,
            "the open that arrived during the send must be retained"
        );
    }

    // #1888: the same decrement path carries the structured-interaction counters,
    // so an interaction that lands mid-send must survive the clear (the plan
    // mode counter shown here, which the snapshot reports as the bool count>0).
    #[test]
    fn reported_count_decrement_preserves_concurrent_structured_interaction() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let plan_mode = AtomicU32::new(2);
        let reported = plan_mode.load(Ordering::Relaxed);
        plan_mode.fetch_add(1, Ordering::Relaxed);
        decrement_reported_count(&plan_mode, reported);
        assert_eq!(plan_mode.load(Ordering::Relaxed), 1);
    }

    // The decrement saturates rather than underflow-wrapping the AtomicU32, so a
    // hypothetical future refactor that detaches sends (double-clearing a
    // counter) degrades to zero instead of jumping to u32::MAX.
    #[test]
    fn reported_count_decrement_saturates_below_zero() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(2);
        decrement_reported_count(&counter, 5);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }
}
