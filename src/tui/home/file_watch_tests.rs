//! In-module tests for `HomeView` file-watch wiring, exercising
//! `HomeView::new` and `HomeView::rewire_disk_subscriptions` directly.
//! The integration-level tests under `tests/filewatch_tui_*.rs`
//! exercise the same wiring against the public `file_watch` API in
//! isolation.
//!
//! Async TUI tests are segregated to this module so the much larger
//! synchronous `tests.rs` file is not forced to mix sync `#[test]`
//! with `#[tokio::test]` runtime infrastructure.

#![cfg(test)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;

use super::HomeView;
use crate::file_watch::FileWatchService;
use crate::session::{Instance, Storage};

fn isolate_home(temp: &std::path::Path) {
    // SAFETY: env mutation; #[serial] guards cross-test races on HOME.
    unsafe { std::env::set_var("HOME", temp) };
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"))
    };
}

/// Poll `pred` every 25ms up to `deadline`. Avoids a fixed sleep that
/// would either flake on slow CI or pad the test runtime on fast paths.
async fn wait_until<F>(deadline: Duration, mut pred: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Locks the adapter-spawn contract: real watcher events must flip
/// `disk_dirty` through the `HomeView::new` wiring.
#[tokio::test]
#[serial]
async fn home_view_new_spawns_adapter_that_flips_disk_dirty() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-adapter").expect("seed dir");

    let view = HomeView::new(
        Some("hv-adapter".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    // Install a watcher subscription via the real rewire path so the
    // dispatcher routes peer writes through the adapter task that
    // HomeView::new just spawned.
    let mut view = view;
    view.rewire_disk_subscriptions(&["hv-adapter".to_string()])
        .expect("rewire");

    let writer = Storage::new("hv-adapter", live.clone()).expect("writer");
    writer
        .update(|i, _g| {
            *i = vec![Instance::new("peer-write", "/tmp/peer")];
            Ok(())
        })
        .expect("peer write");

    let flipped = wait_until(Duration::from_secs(2), || {
        view.disk_dirty.load(Ordering::Acquire)
    })
    .await;
    assert!(
        flipped,
        "HomeView::new must spawn the adapter task that flips disk_dirty on dispatcher events"
    );
}

/// Locks the canonical remove path in `rewire_disk_subscriptions`:
/// removing a profile must leave no stale `disk_watch_handles` entry
/// behind.
#[tokio::test]
#[serial]
async fn rewire_disk_subscriptions_drops_removed_profile_entry() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-keep").expect("dir");
    crate::session::get_profile_dir("hv-drop").expect("dir");

    let mut view = HomeView::new(
        Some("hv-keep".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.rewire_disk_subscriptions(&["hv-keep".to_string(), "hv-drop".to_string()])
        .expect("install both");
    assert!(
        view.disk_watch_handles.contains_key("hv-keep"),
        "precondition: hv-keep installed"
    );
    assert!(
        view.disk_watch_handles.contains_key("hv-drop"),
        "precondition: hv-drop installed"
    );

    view.rewire_disk_subscriptions(&["hv-keep".to_string()])
        .expect("remove hv-drop");

    assert!(
        view.disk_watch_handles.contains_key("hv-keep"),
        "rewire must keep entries for profiles still in the current set"
    );
    assert!(
        !view.disk_watch_handles.contains_key("hv-drop"),
        "rewire must drop+abort the entry for a removed profile"
    );
    assert_eq!(
        view.disk_watch_handles.len(),
        1,
        "exactly the surviving profile's disk_watch_handles entry remains; live `subscriber_count()` also includes config-watch subscriptions wired by `rewire_config_subscriptions` and is not the right invariant for the disk-only path"
    );
}

/// Locks the config-watch remove/recreate path: deleting a profile must
/// clear its typed key, and recreating it must restore the subscription
/// count back to baseline without leaking an extra entry.
#[tokio::test]
#[serial]
async fn config_subscriptions_remove_then_recreate_does_not_leak_or_double_subscribe() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("cfg-leak").expect("seed dir");

    let mut view = HomeView::new(
        Some("cfg-leak".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    use super::ConfigWatchKey;

    view.rewire_config_subscriptions(&["cfg-leak".to_string()])
        .expect("install profile sub");
    let baseline = live.subscriber_count();
    assert!(
        view.config_watch_handles
            .contains_key(&ConfigWatchKey::profile("cfg-leak")),
        "precondition: profile config sub installed"
    );

    view.rewire_config_subscriptions(&[]).expect("remove all");
    assert!(
        !view
            .config_watch_handles
            .contains_key(&ConfigWatchKey::profile("cfg-leak")),
        "remove must drop the per-profile entry"
    );

    view.rewire_config_subscriptions(&["cfg-leak".to_string()])
        .expect("recreate profile sub");
    assert!(
        view.config_watch_handles
            .contains_key(&ConfigWatchKey::profile("cfg-leak")),
        "recreate must reinstall the per-profile entry"
    );
    assert_eq!(
        live.subscriber_count(),
        baseline,
        "remove-then-recreate must converge to the same live subscription count, not double up"
    );
}

/// Locks the resurrection-prevention invariant for
/// `rewire_config_subscriptions`: the inode-invalidation pre-pass
/// resolves profile paths through the non-creating
/// `get_profile_dir_path`, so a deleted profile directory stays
/// deleted when a subsequent rewire iterates it in `prior_profiles`.
#[tokio::test]
#[serial]
async fn rewire_config_subscriptions_does_not_resurrect_deleted_profile_dir() {
    use super::ConfigWatchKey;
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    let profile_dir = crate::session::get_profile_dir("ghost").expect("seed dir");

    let mut view = HomeView::new(
        Some("ghost".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.rewire_config_subscriptions(&["ghost".to_string()])
        .expect("install profile sub");
    assert!(
        view.config_watch_handles
            .contains_key(&ConfigWatchKey::profile("ghost")),
        "precondition: profile config sub installed"
    );

    std::fs::remove_dir_all(&profile_dir).expect("delete profile dir");
    assert!(
        !profile_dir.exists(),
        "precondition: profile dir is gone before the rewire pre-pass runs"
    );

    view.rewire_config_subscriptions(&[])
        .expect("rewire after delete");

    assert!(
        !profile_dir.exists(),
        "the inode-invalidation pre-pass must use a non-creating resolver; \
         a deleted profile directory stays deleted across rewire"
    );
}

/// In single-profile mode, `reload_storage_only` keeps disk
/// subscriptions scoped to `self.storages.keys()` (just the active
/// profile) while config subscriptions cover the full on-disk
/// profile set. Widening disk wiring to `current_profiles` would
/// watch peer profiles' sessions.json/groups.json that the user
/// explicitly opted out of by passing `--profile X`.
#[tokio::test]
#[serial]
async fn reload_storage_only_keeps_disk_watch_scoped_in_single_profile_mode() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("active-only").expect("seed active dir");
    crate::session::get_profile_dir("peer-one").expect("seed peer 1 dir");
    crate::session::get_profile_dir("peer-two").expect("seed peer 2 dir");

    let mut view = HomeView::new(
        Some("active-only".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.reload_storage_only().expect("reload");

    use super::ConfigWatchKey;
    assert_eq!(
        view.disk_watch_handles.len(),
        1,
        "single-profile mode must keep disk watches scoped to the active profile only; \
         got {} entries: {:?}",
        view.disk_watch_handles.len(),
        view.disk_watch_handles.keys().collect::<Vec<_>>()
    );
    assert!(
        view.disk_watch_handles.contains_key("active-only"),
        "the active profile's disk watch must be present after reload"
    );
    assert!(
        view.config_watch_handles
            .contains_key(&ConfigWatchKey::profile("peer-one")),
        "peer profiles' CONFIG watches must be wired (asymmetric design): \
         peer config edits propagate to picker UI / status-hook cache"
    );
    assert!(
        view.config_watch_handles
            .contains_key(&ConfigWatchKey::profile("peer-two")),
        "all on-disk profiles must have config watches in single-profile mode"
    );
}

/// When `list_profiles()` fails after a successful create or delete,
/// `rewire_after_profile_mutation` must surface a Watcher Warning to
/// the user via `info_dialog` (in addition to logging a structured
/// warn). The test seam in `crate::session` injects the failure
/// without requiring a platform-fragile permission denial.
#[tokio::test]
#[serial]
async fn rewire_after_profile_mutation_surfaces_dialog_when_list_profiles_fails() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("seam-test").expect("seed dir");

    let mut view = HomeView::new(
        Some("seam-test".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    assert!(
        view.info_dialog.is_none(),
        "precondition: no dialog before the failure"
    );

    let _fail_guard = crate::session::FailNextListProfilesGuard::new();
    view.rewire_after_profile_mutation("seam-test", super::ProfileMutation::Create);

    assert!(
        view.info_dialog.is_some(),
        "list_profiles failure must surface a Watcher Warning dialog to the user; \
         silently swallowing the error would leave info_dialog None"
    );

    assert!(
        crate::session::list_profiles().is_ok(),
        "seam must auto-clear after firing once"
    );
}

#[tokio::test]
#[serial]
async fn reload_storage_only_survives_list_profiles_failure() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("reload-fallback").expect("seed dir");

    let mut view = HomeView::new(
        Some("reload-fallback".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let writer = Storage::new("reload-fallback", live.clone()).expect("writer");
    writer
        .update(|instances, _groups| {
            *instances = vec![Instance::new("fallback-row", "/tmp/fallback")];
            Ok(())
        })
        .expect("peer write");

    let _fail_guard = crate::session::FailNextListProfilesGuard::new();
    view.reload_storage_only()
        .expect("reload should degrade, not fail");

    assert!(
        view.instances
            .iter()
            .any(|inst| inst.title == "fallback-row"),
        "reload must still refresh storage-backed instances when list_profiles fails"
    );
    assert!(
        crate::session::list_profiles().is_ok(),
        "seam must auto-clear after firing once"
    );
}

#[tokio::test]
#[serial]
async fn rewire_after_profile_mutation_preserves_existing_info_dialog() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("dialog-guard").expect("seed dir");

    let mut view = HomeView::new(
        Some("dialog-guard".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
        "Existing dialog",
        "keep me",
    ));

    let _fail_guard = crate::session::FailNextListProfilesGuard::new();
    view.rewire_after_profile_mutation("dialog-guard", super::ProfileMutation::Delete);

    assert!(
        crate::session::list_profiles().is_ok(),
        "seam must auto-clear after firing once"
    );

    let mut dialog = view.info_dialog.expect("existing dialog should survive");
    let theme = crate::tui::styles::load_theme("empire");
    let backend = ratatui::backend::TestBackend::new(60, 12);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &theme))
        .expect("render dialog");
    let buf = terminal.backend().buffer().clone();
    let rendered: String = buf.content.iter().map(|cell| cell.symbol()).collect();
    assert!(
        rendered.contains("Existing dialog"),
        "rewire failure must not overwrite a pre-existing info dialog"
    );
}

/// The Watcher Warning dialog raised by `rewire_after_profile_mutation`
/// is intentionally outside `reload_failure_state`, so `has_any_failure()`
/// stays false. The recovery-edge cleanup keys off both the failure
/// state and the dialog title, and must not match `Watcher Warning`;
/// the dialog stays visible until the user dismisses it.
#[tokio::test]
#[serial]
async fn rewire_after_profile_mutation_watcher_warning_survives_recovery_edge() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("watcher-warning-edge").expect("seed dir");

    let mut view = HomeView::new(
        Some("watcher-warning-edge".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let _fail_guard = crate::session::FailNextListProfilesGuard::new();
    view.rewire_after_profile_mutation("watcher-warning-edge", super::ProfileMutation::Create);

    let dialog = view.info_dialog.as_ref().expect("watcher warning raised");
    assert_eq!(
        dialog.title(),
        "Watcher Warning",
        "rewire failure must raise the Watcher Warning dialog"
    );
    assert!(
        !view.reload_failure_state.has_any_failure(),
        "rewire_after_profile_mutation does not record into reload_failure_state; \
         the recovery-edge cleanup keys off has_any_failure() to protect tracked \
         failures, and the Watcher Warning relies on its title to stay visible"
    );

    let cleared = view.try_clear_recovered_reload_dialog();
    assert!(
        !cleared,
        "try_clear_recovered_reload_dialog must not match Watcher Warning"
    );
    let dialog = view
        .info_dialog
        .as_ref()
        .expect("watcher warning must persist past the recovery-edge check");
    assert_eq!(
        dialog.title(),
        "Watcher Warning",
        "the dialog promises the next reload will repair watcher state and \
         stays visible for the user to read and dismiss"
    );
}

/// Locks the no-op fast-path invariant: when `rewire_disk_subscriptions`
/// is called with an unchanged profile set and no inode invalidation, the
/// fast-path returns without running the install loop, and a previously
/// latched `disk_watcher_init_error` must be preserved (the install loop
/// is the only path that re-latches via `record_disk_watcher_init_failure`).
#[tokio::test]
#[serial]
async fn rewire_no_op_preserves_latched_disk_watcher_init_failure() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-noop").expect("seed dir");

    let mut view = HomeView::new(
        Some("hv-noop".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.rewire_disk_subscriptions(&["hv-noop".to_string()])
        .expect("install");
    assert!(
        view.disk_watch_handles.contains_key("hv-noop"),
        "precondition: hv-noop installed"
    );

    view.reload_failure_state
        .record_disk_watcher_init_failure("seed: simulated prior failure");
    assert!(
        view.reload_failure_state.has_any_failure(),
        "precondition: latch is set"
    );

    view.rewire_disk_subscriptions(&["hv-noop".to_string()])
        .expect("no-op rewire");

    assert!(
        view.reload_failure_state.has_any_failure(),
        "no-op rewire (unchanged set, no inode change) must preserve the disk_watcher_init_error latch"
    );
}

/// Locks the per-source independence invariant: a config init failure
/// recorded in `config_watcher_init_error` must survive a concurrent disk
/// rewire that clears `disk_watcher_init_error` (and vice-versa). The two
/// fields are independent slots; clearing one never touches the other.
#[tokio::test]
#[serial]
async fn config_init_failure_survives_concurrent_disk_rewire_clear() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-iso").expect("seed dir");

    let mut view = HomeView::new(
        Some("hv-iso".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.reload_failure_state
        .record_config_watcher_init_failure("seed: config init failed");
    assert!(
        view.reload_failure_state.has_any_failure(),
        "precondition: config latch is set"
    );

    view.rewire_disk_subscriptions(&["hv-iso".to_string()])
        .expect("disk rewire install");

    assert!(
        view.reload_failure_state.has_any_failure(),
        "disk rewire install must not clear the independent config_watcher_init_error latch"
    );
}

#[test]
fn reload_failure_state_record_storage_recovery_returns_true_and_clears_ack_latch() {
    let mut state = super::ReloadFailureState::default();
    let err: anyhow::Result<()> = Err(anyhow::anyhow!("disk unreadable"));
    let ok: anyhow::Result<()> = Ok(());

    assert!(
        !state.record_storage(&err),
        "first failure does not return true"
    );
    state.acknowledge_dialog();
    assert!(!state.has_unacknowledged_failure());

    assert!(
        state.record_storage(&ok),
        "failed-to-ok edge must return true so callers can emit an info log on recovery"
    );
    assert!(
        !state.has_any_failure(),
        "successful recovery clears the failure flag"
    );
    assert!(
        !state.has_unacknowledged_failure(),
        "recovery clears the ack latch so a fresh failure burst will surface a fresh dialog"
    );
}

#[test]
fn reload_failure_state_new_failure_during_acked_burst_re_arms_dialog() {
    let mut state = super::ReloadFailureState::default();
    let err1: anyhow::Result<()> = Err(anyhow::anyhow!("storage broken"));
    let err2: anyhow::Result<()> = Err(anyhow::anyhow!("config broken"));

    state.record_storage(&err1);
    state.acknowledge_dialog();
    assert!(
        !state.has_unacknowledged_failure(),
        "first failure acknowledged"
    );

    state.record_config(&err2);
    assert!(
        state.has_unacknowledged_failure(),
        "a NEW source failing during an already-acknowledged burst re-arms the dialog so the user is notified about the additional failure"
    );
}

#[test]
fn reload_failure_state_dialog_body_aggregates_all_four_sources() {
    let mut state = super::ReloadFailureState::default();
    state.record_storage(&Err::<(), _>(anyhow::anyhow!("storage err")));
    state.record_config(&Err::<(), _>(anyhow::anyhow!("config err")));
    state.record_disk_watcher_init_failure("disk subscribe denied");
    state.record_config_watcher_init_failure("config subscribe denied");

    let body = state.build_dialog_body();
    assert!(
        body.contains("- Storage: storage err"),
        "missing storage line: {body}"
    );
    assert!(
        body.contains("- Config: config err"),
        "missing config line: {body}"
    );
    assert!(
        body.contains("- Disk watcher init: disk subscribe denied"),
        "missing disk watcher-init line: {body}"
    );
    assert!(
        body.contains("- Config watcher init: config subscribe denied"),
        "missing config watcher-init line: {body}"
    );
}

#[test]
fn reload_failure_state_watcher_init_failure_lifecycle_is_per_source() {
    let mut state = super::ReloadFailureState::default();

    state.record_disk_watcher_init_failure("first disk install failed");
    assert!(
        state.has_unacknowledged_failure(),
        "disk_watcher_init_error contributes to has_any_failure"
    );

    state.record_config_watcher_init_failure("first config install failed");
    state.acknowledge_dialog();

    state.clear_disk_watcher_init_failure();
    assert!(
        state.has_any_failure(),
        "clearing only the disk slot leaves the config slot latched"
    );

    state.clear_config_watcher_init_failure();
    assert!(
        !state.has_any_failure(),
        "clearing the last failing source removes all latches"
    );
    assert!(
        !state.has_unacknowledged_failure(),
        "clearing the last failing source resets the ack latch"
    );
}
