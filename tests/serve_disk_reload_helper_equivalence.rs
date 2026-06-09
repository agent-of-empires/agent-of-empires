//! Helper-equivalence regression for the `reload_state_instances_from_disk`
//! extraction.
//!
//! Verifies that the helper preserves the per-id ordering contract for both
//! `StatusSource` variants: `DiskOnly` keeps the prior in-memory `status` and
//! `idle_entered_at` while `TmuxApplied` trusts the caller-applied scrape.
//! Both paths must take the monotonic-max `last_accessed_at` and carry the
//! five `#[serde(skip)]` runtime fields preserved by `merge_runtime_fields`.
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
    let mut prior = Instance::new("seed", "/tmp/seed");
    prior.status = Status::Running;
    prior.last_error = Some("boom".to_string());
    prior.last_accessed_at = Some(chrono::Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap());
    let prior_id = prior.id.clone();
    let state = build_test_app_state(vec![prior]);

    let mut fresh = Instance::new("seed", "/tmp/seed");
    fresh.id = prior_id.clone();
    fresh.status = Status::Idle;
    fresh.last_accessed_at = Some(chrono::Utc.with_ymd_and_hms(2024, 5, 1, 0, 0, 0).unwrap());

    reload_disk_only_for_test(&state, vec![fresh]).await;

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
        row.last_error.as_deref(),
        Some("boom"),
        "runtime field preserved"
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
    let mut prior = Instance::new("seed", "/tmp/seed");
    prior.status = Status::Idle;
    prior.last_error = Some("prev".to_string());
    prior.last_accessed_at = Some(chrono::Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap());
    let prior_id = prior.id.clone();
    let state = build_test_app_state(vec![prior]);

    let mut fresh = Instance::new("seed", "/tmp/seed");
    fresh.id = prior_id.clone();
    fresh.status = Status::Running;
    fresh.last_accessed_at = Some(chrono::Utc.with_ymd_and_hms(2024, 5, 1, 0, 0, 0).unwrap());

    reload_tmux_applied_for_test(&state, vec![fresh]).await;

    let result = state.instances.read().await;
    assert_eq!(result.len(), 1);
    let row = &result[0];
    assert_eq!(
        row.status,
        Status::Running,
        "TmuxApplied: fresh status must win",
    );
    assert_eq!(
        row.last_error.as_deref(),
        Some("prev"),
        "runtime field preserved"
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
async fn reload_state_instances_from_disk_new_ids_use_fresh() {
    let prior = Instance::new("seed", "/tmp/seed");
    let state = build_test_app_state(vec![prior]);
    let new_inst = Instance::new("new", "/tmp/new");
    let new_id = new_inst.id.clone();
    reload_disk_only_for_test(&state, vec![new_inst]).await;
    let result = state.instances.read().await;
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, new_id);
    assert!(
        result[0].last_error.is_none(),
        "new id has no prior runtime fields"
    );
}
