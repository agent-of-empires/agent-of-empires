//! Helper-equivalence regression for the `reload_state_instances_from_disk`
//! extraction.
//!
//! Verifies that the helper preserves the per-id ordering contract for both
//! `StatusSource` variants: `DiskOnly` keeps the prior in-memory `status` and
//! `idle_entered_at` while `TmuxApplied` trusts the caller-applied scrape.
//! Both paths must take the monotonic-max `last_accessed_at` and carry the
//! `#[serde(skip)]` runtime fields preserved by `merge_runtime_fields`.
//! `last_error` is the exception: it is preserved only while the merged status
//! is still `Error`, so a session that recovered to a healthy state drops the
//! stale string (issue #1271). The other runtime fields (here `last_error_check`)
//! carry over unconditionally.
//!
//! Drives the helper directly via the `crate::server::test_support` surface
//! exposed for this test (the merge invariants live below the HTTP API and
//! cannot be observed end-to-end through `GET /api/sessions`).

#![cfg(feature = "serve")]

use agent_of_empires::server::test_support::build_test_app_state;
use agent_of_empires::server::test_support::{
    reload_disk_only_for_test, reload_tmux_applied_for_test,
};
use agent_of_empires::session::{Instance, Status};
use chrono::TimeZone;

#[tokio::test]
async fn reload_state_instances_from_disk_disk_only_preserves_prior_status() {
    let probe = std::time::Instant::now();
    let mut prior = Instance::new("seed", "/tmp/seed");
    prior.status = Status::Running;
    prior.last_error_check = Some(probe);
    prior.last_error = Some("boom".to_string());
    prior.last_accessed_at = Some(chrono::Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap());
    let prior_id = prior.id.clone();
    let state = build_test_app_state(vec![prior]);

    let mut fresh = Instance::new("seed", "/tmp/seed");
    fresh.id = prior_id.clone();
    fresh.status = Status::Idle;
    fresh.last_accessed_at = Some(chrono::Utc.with_ymd_and_hms(2024, 5, 1, 0, 0, 0).unwrap());

    reload_disk_only_for_test(&state, vec![fresh], Vec::new()).await;

    let result = state.instances.read().await;
    assert_eq!(result.len(), 1);
    let row = &result[0];
    assert_eq!(row.id, prior_id);
    assert_eq!(
        row.status,
        Status::Running,
        "DiskOnly: prior in-memory status must win"
    );
    assert_eq!(
        row.last_error_check,
        Some(probe),
        "runtime field preserved unconditionally"
    );
    assert_eq!(
        row.last_error, None,
        "healthy merged status drops the stale last_error (#1271)"
    );
    assert_eq!(
        row.last_accessed_at.unwrap().timestamp(),
        chrono::Utc
            .with_ymd_and_hms(2024, 6, 1, 0, 0, 0)
            .unwrap()
            .timestamp(),
        "monotonic-max last_accessed_at",
    );
}

#[tokio::test]
async fn reload_state_instances_from_disk_tmux_applied_takes_fresh_status() {
    let probe = std::time::Instant::now();
    let mut prior = Instance::new("seed", "/tmp/seed");
    prior.status = Status::Idle;
    prior.last_error_check = Some(probe);
    prior.last_error = Some("prev".to_string());
    prior.last_accessed_at = Some(chrono::Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap());
    let prior_id = prior.id.clone();
    let state = build_test_app_state(vec![prior]);

    let mut fresh = Instance::new("seed", "/tmp/seed");
    fresh.id = prior_id.clone();
    fresh.status = Status::Running;
    fresh.last_accessed_at = Some(chrono::Utc.with_ymd_and_hms(2024, 5, 1, 0, 0, 0).unwrap());

    reload_tmux_applied_for_test(&state, vec![fresh], Vec::new()).await;

    let result = state.instances.read().await;
    assert_eq!(result.len(), 1);
    let row = &result[0];
    assert_eq!(
        row.status,
        Status::Running,
        "TmuxApplied: fresh status must win",
    );
    assert_eq!(
        row.last_error_check,
        Some(probe),
        "runtime field preserved unconditionally"
    );
    assert_eq!(
        row.last_error, None,
        "healthy merged status drops the stale last_error (#1271)"
    );
    assert_eq!(
        row.last_accessed_at.unwrap().timestamp(),
        chrono::Utc
            .with_ymd_and_hms(2024, 6, 1, 0, 0, 0)
            .unwrap()
            .timestamp(),
        "monotonic-max last_accessed_at",
    );
}

/// #2865: `status_poll_loop` calls `update_status_with_metadata` on the
/// `fresh` vector (after seeding it from the prior in-memory
/// `unknown_since`/`ever_confirmed_present`) BEFORE this helper runs, so by
/// the time `TmuxApplied` reaches here `fresh` already holds this tick's
/// authoritative escalation decision. If the merge instead restored the
/// prior in-memory snapshot (taken before that decision ran), it would wipe
/// out the just-computed `unknown_since` advancement every single tick,
/// permanently resetting the Unknown->Error escalation window and
/// reintroducing the bug this test guards against.
#[tokio::test]
async fn reload_state_instances_from_disk_tmux_applied_trusts_fresh_unknown_tracking() {
    let mut prior = Instance::new("seed", "/tmp/seed");
    prior.ever_confirmed_present = false;
    prior.unknown_since = None;
    let prior_id = prior.id.clone();
    let state = build_test_app_state(vec![prior]);

    // Simulates status_poll_loop having already run
    // `update_status_with_metadata` on a disk-fresh instance seeded from the
    // prior tick's tracking fields: this tick's tmux probe found the server
    // still unreachable, so `unknown_since` advanced from `None` to `Some`.
    let advanced_since = std::time::Instant::now();
    let mut fresh = Instance::new("seed", "/tmp/seed");
    fresh.id = prior_id.clone();
    fresh.ever_confirmed_present = false;
    fresh.unknown_since = Some(advanced_since);

    reload_tmux_applied_for_test(&state, vec![fresh], Vec::new()).await;

    let result = state.instances.read().await;
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0].unknown_since,
        Some(advanced_since),
        "TmuxApplied must trust fresh's just-computed unknown_since, not the \
         prior in-memory snapshot taken before this tick's status decision"
    );
    assert!(
        !result[0].ever_confirmed_present,
        "unrelated field unaffected by the advancement"
    );
}

/// #2865 counterpart: `disk_watcher_consumer`'s `DiskOnly` path never calls
/// `update_status_with_metadata` (no tmux scrape), so `fresh` here is a raw
/// disk load with both fields at their `#[serde(skip)]` defaults. The prior
/// in-memory tracking must still survive this reload.
#[tokio::test]
async fn reload_state_instances_from_disk_disk_only_preserves_prior_unknown_tracking() {
    let confirmed_since = std::time::Instant::now();
    let mut prior = Instance::new("seed", "/tmp/seed");
    prior.ever_confirmed_present = true;
    prior.unknown_since = Some(confirmed_since);
    let prior_id = prior.id.clone();
    let state = build_test_app_state(vec![prior]);

    let mut fresh = Instance::new("seed", "/tmp/seed");
    fresh.id = prior_id.clone();
    assert!(!fresh.ever_confirmed_present);
    assert_eq!(fresh.unknown_since, None);

    reload_disk_only_for_test(&state, vec![fresh], Vec::new()).await;

    let result = state.instances.read().await;
    assert_eq!(result.len(), 1);
    assert!(
        result[0].ever_confirmed_present,
        "DiskOnly must restore the prior in-memory ever_confirmed_present"
    );
    assert_eq!(
        result[0].unknown_since,
        Some(confirmed_since),
        "DiskOnly must restore the prior in-memory unknown_since"
    );
}

#[tokio::test]
async fn reload_state_instances_from_disk_new_ids_use_fresh() {
    let prior = Instance::new("seed", "/tmp/seed");
    let state = build_test_app_state(vec![prior]);
    let new_inst = Instance::new("new", "/tmp/new");
    let new_id = new_inst.id.clone();
    reload_disk_only_for_test(&state, vec![new_inst], Vec::new()).await;
    let result = state.instances.read().await;
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, new_id);
    assert!(
        result[0].last_error.is_none(),
        "new id has no prior runtime fields"
    );
}
