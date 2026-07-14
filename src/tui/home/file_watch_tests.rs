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
use crate::session::test_support::isolate_home;
use crate::session::{Instance, Storage};

fn watcher_err(profile: Option<&str>, message: &str) -> super::WatcherInitError {
    super::WatcherInitError {
        profile: profile.map(str::to_owned),
        kind: super::WatcherInitErrorKind::Watch(crate::file_watch::WatchErrorKind::Other),
        message: message.to_owned(),
    }
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

/// RAII guard that sets `AOE_E2E_DEBUG=1` for the scope and clears it
/// on drop. Mirrors `FailNextListProfilesGuard`'s shape so a panic
/// between assertions cannot leak the env var to subsequent tests in
/// the same `cargo test` process.
struct E2eDebugGuard;

impl E2eDebugGuard {
    fn enable() -> Self {
        // SAFETY: env mutation; the Drop impl pairs with this set_var.
        // #[serial] guards cross-test races on the process env.
        unsafe { std::env::set_var("AOE_E2E_DEBUG", "1") };
        Self
    }
}

impl Drop for E2eDebugGuard {
    fn drop(&mut self) {
        // SAFETY: env cleanup; matches set_var in E2eDebugGuard::enable.
        unsafe { std::env::remove_var("AOE_E2E_DEBUG") };
    }
}

/// Locks the adapter-spawn contract: real watcher events must flip
/// `disk_watch.dirty` through the `HomeView::new` wiring.
#[tokio::test]
#[serial]
async fn home_view_new_spawns_adapter_that_flips_disk_dirty() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

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
    view.rewire_disk_subscriptions(&["hv-adapter".to_string()]);

    let writer = Storage::new("hv-adapter", live.clone()).expect("writer");
    writer
        .update(|i, _g| {
            *i = vec![Instance::new("peer-write", "/tmp/peer")];
            Ok(())
        })
        .expect("peer write");

    let flipped = wait_until(Duration::from_secs(2), || {
        view.disk_watch.dirty.load(Ordering::Acquire)
    })
    .await;
    assert!(
        flipped,
        "HomeView::new must spawn the adapter task that flips disk_dirty on dispatcher events"
    );
}

/// Locks the canonical remove path in `rewire_disk_subscriptions`:
/// removing a profile must leave no stale `disk_watch.handles` entry
/// behind.
#[tokio::test]
#[serial]
async fn rewire_disk_subscriptions_drops_removed_profile_entry() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-keep").expect("dir");
    crate::session::get_profile_dir("hv-drop").expect("dir");

    let mut view = HomeView::new(
        Some("hv-keep".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.rewire_disk_subscriptions(&["hv-keep".to_string(), "hv-drop".to_string()]);
    assert!(
        view.disk_watch.handles.contains_key("hv-keep"),
        "precondition: hv-keep installed"
    );
    assert!(
        view.disk_watch.handles.contains_key("hv-drop"),
        "precondition: hv-drop installed"
    );

    view.rewire_disk_subscriptions(&["hv-keep".to_string()]);

    assert!(
        view.disk_watch.handles.contains_key("hv-keep"),
        "rewire must keep entries for profiles still in the current set"
    );
    assert!(
        !view.disk_watch.handles.contains_key("hv-drop"),
        "rewire must drop+abort the entry for a removed profile"
    );
    assert_eq!(
        view.disk_watch.handles.len(),
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
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("cfg-leak").expect("seed dir");

    let mut view = HomeView::new(
        Some("cfg-leak".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    use super::ConfigWatchKey;

    view.rewire_config_subscriptions(&["cfg-leak".to_string()]);
    let baseline = live.subscriber_count();
    assert!(
        view.config_watch
            .handles
            .contains_key(&ConfigWatchKey::profile("cfg-leak")),
        "precondition: profile config sub installed"
    );

    view.rewire_config_subscriptions(&[]);
    assert!(
        !view
            .config_watch
            .handles
            .contains_key(&ConfigWatchKey::profile("cfg-leak")),
        "remove must drop the per-profile entry"
    );

    view.rewire_config_subscriptions(&["cfg-leak".to_string()]);
    assert!(
        view.config_watch
            .handles
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
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    let profile_dir = crate::session::get_profile_dir("ghost").expect("seed dir");

    let mut view = HomeView::new(
        Some("ghost".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.rewire_config_subscriptions(&["ghost".to_string()]);
    assert!(
        view.config_watch
            .handles
            .contains_key(&ConfigWatchKey::profile("ghost")),
        "precondition: profile config sub installed"
    );

    std::fs::remove_dir_all(&profile_dir).expect("delete profile dir");
    assert!(
        !profile_dir.exists(),
        "precondition: profile dir is gone before the rewire pre-pass runs"
    );

    view.rewire_config_subscriptions(&[]);

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
    let _home = isolate_home(temp.path());

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
        view.disk_watch.handles.len(),
        1,
        "single-profile mode must keep disk watches scoped to the active profile only; \
         got {} entries: {:?}",
        view.disk_watch.handles.len(),
        view.disk_watch.handles.keys().collect::<Vec<_>>()
    );
    assert!(
        view.disk_watch.handles.contains_key("active-only"),
        "the active profile's disk watch must be present after reload"
    );
    assert!(
        view.config_watch
            .handles
            .contains_key(&ConfigWatchKey::profile("peer-one")),
        "peer profiles' CONFIG watches must be wired (asymmetric design): \
         peer config edits propagate to picker UI / status-hook cache"
    );
    assert!(
        view.config_watch
            .handles
            .contains_key(&ConfigWatchKey::profile("peer-two")),
        "all on-disk profiles must have config watches in single-profile mode"
    );
}

/// Locks the single-profile-mode scoping invariant for
/// `rewire_after_profile_delete`. In `aoe --profile X` mode
/// `can_delete_selected` requires `!p.is_active`, so the deleted
/// profile is always a peer; the post-delete `list_profiles()`
/// snapshot must not be passed verbatim to
/// `rewire_disk_subscriptions` because peer-profile disk watches
/// violate the single-profile-mode contract that the user opted
/// into with `--profile X`. Disk targets are scoped to
/// `self.storages.keys()` (the active profile), using the same
/// shape `reload_storage_only` uses. Config watches stay full-set
/// because peer config edits must propagate to the picker UI even
/// in single-profile mode.
#[tokio::test]
#[serial]
async fn rewire_after_profile_delete_keeps_disk_watch_scoped_in_single_profile_mode() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("active-scoped").expect("seed active");
    crate::session::get_profile_dir("peer-stays").expect("seed peer that stays");
    crate::session::get_profile_dir("peer-deleted").expect("seed peer to delete");

    let mut view = HomeView::new(
        Some("active-scoped".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let deleted_dir =
        crate::session::get_profile_dir_path("peer-deleted").expect("peer-deleted path");
    std::fs::remove_dir_all(&deleted_dir).expect("remove peer-deleted");

    view.rewire_after_profile_delete("peer-deleted");

    use super::ConfigWatchKey;
    assert_eq!(
        view.disk_watch.handles.len(),
        1,
        "single-profile mode must keep disk watches scoped to the active profile after a peer delete; \
         got {} entries: {:?}",
        view.disk_watch.handles.len(),
        view.disk_watch.handles.keys().collect::<Vec<_>>()
    );
    assert!(
        view.disk_watch.handles.contains_key("active-scoped"),
        "the active profile's disk watch must remain installed"
    );
    assert!(
        view.config_watch
            .handles
            .contains_key(&ConfigWatchKey::profile("peer-stays")),
        "remaining peer profiles' CONFIG watches must stay wired (asymmetric design)"
    );
    assert!(
        !view
            .config_watch
            .handles
            .contains_key(&ConfigWatchKey::profile("peer-deleted")),
        "the deleted peer's config watch must be torn down"
    );
}

/// When `list_profiles()` fails after a successful create or delete,
/// `rewire_after_profile_delete` must surface a Watcher Warning to
/// the user via `info_dialog` (in addition to logging a structured
/// warn). The test seam in `crate::session` injects the failure
/// without requiring a platform-fragile permission denial.
#[tokio::test]
#[serial]
async fn rewire_after_profile_delete_surfaces_dialog_when_list_profiles_fails() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

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
    view.rewire_after_profile_delete("seam-test");

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
    let _home = isolate_home(temp.path());

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
            .values()
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
async fn rewire_after_profile_delete_preserves_existing_info_dialog() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

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
    view.rewire_after_profile_delete("dialog-guard");

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

/// The Watcher Warning dialog raised by `rewire_after_profile_delete`
/// is intentionally outside `reload_failure_state`, so `has_any_failure()`
/// stays false. The recovery-edge cleanup keys off both the failure
/// state and the dialog title, and must not match `Watcher Warning`;
/// the dialog stays visible until the user dismisses it.
#[tokio::test]
#[serial]
async fn rewire_after_profile_delete_watcher_warning_survives_recovery_edge() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("watcher-warning-edge").expect("seed dir");

    let mut view = HomeView::new(
        Some("watcher-warning-edge".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let _fail_guard = crate::session::FailNextListProfilesGuard::new();
    view.rewire_after_profile_delete("watcher-warning-edge");

    let dialog = view.info_dialog.as_ref().expect("watcher warning raised");
    assert_eq!(
        dialog.title(),
        "Watcher Warning",
        "rewire failure must raise the Watcher Warning dialog"
    );
    assert!(
        !view.reload_failure_state.has_any_failure(),
        "rewire_after_profile_delete does not record into reload_failure_state; \
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
/// is the only path that re-latches via `apply_disk_watcher_init_pass`).
#[tokio::test]
#[serial]
async fn rewire_no_op_preserves_latched_disk_watcher_init_failure() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-noop").expect("seed dir");

    let mut view = HomeView::new(
        Some("hv-noop".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.rewire_disk_subscriptions(&["hv-noop".to_string()]);
    assert!(
        view.disk_watch.handles.contains_key("hv-noop"),
        "precondition: hv-noop installed"
    );

    view.reload_failure_state
        .apply_disk_watcher_init_pass(Some(watcher_err(
            Some("hv-noop"),
            "simulated prior failure",
        )));
    assert!(
        view.reload_failure_state.has_any_failure(),
        "precondition: latch is set"
    );

    view.rewire_disk_subscriptions(&["hv-noop".to_string()]);

    assert!(
        view.reload_failure_state.has_any_failure(),
        "no-op rewire (unchanged set, no inode change) must preserve the disk_watcher_init_error latch"
    );
}

/// Locks the stale-latch detection invariant for
/// `rewire_disk_subscriptions`: when the latch records a failure for
/// profile X but X is no longer in `current` (subscribe_channel Err
/// for X then user deletes X), the rewire pass clears the latch
/// even when the installed-profile set is otherwise unchanged.
/// Companion to `rewire_no_op_preserves_latched_disk_watcher_init_failure`:
/// no-op preserves the latch only when the latch is still relevant.
#[tokio::test]
#[serial]
async fn rewire_disk_clears_stale_latch_when_failing_profile_is_removed() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("active-stale").expect("seed active");

    let mut view = HomeView::new(
        Some("active-stale".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.reload_failure_state
        .apply_disk_watcher_init_pass(Some(watcher_err(
            Some("ghost"),
            "simulated subscribe failure",
        )));
    assert!(
        view.reload_failure_state.has_any_failure(),
        "precondition: latch is set, references the now-deleted profile"
    );

    view.rewire_disk_subscriptions(&["active-stale".to_string()]);

    assert!(
        view.reload_failure_state.disk_watcher_init_error.is_none(),
        "stale latch must clear when its referenced profile is no longer in current; \
         the early-return fast-path must consider latch staleness"
    );
}

/// Locks the stale-latch detection invariant for
/// `rewire_config_subscriptions`. Sibling to
/// `rewire_disk_clears_stale_latch_when_failing_profile_is_removed`;
/// the per-profile config error format is `"profile {name} config: ..."`.
#[tokio::test]
#[serial]
async fn rewire_config_clears_stale_latch_when_failing_profile_is_removed() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("active-stale-cfg").expect("seed active");

    let mut view = HomeView::new(
        Some("active-stale-cfg".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.reload_failure_state
        .apply_config_watcher_init_pass(Some(watcher_err(
            Some("ghost"),
            "simulated subscribe failure",
        )));
    assert!(
        view.reload_failure_state.has_any_failure(),
        "precondition: latch is set, references the now-deleted profile"
    );

    view.rewire_config_subscriptions(&["active-stale-cfg".to_string()]);

    assert!(
        view.reload_failure_state
            .config_watcher_init_error
            .is_none(),
        "stale latch must clear when its referenced profile is no longer in current; \
         the early-return fast-path must consider latch staleness"
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
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-iso").expect("seed dir");

    let mut view = HomeView::new(
        Some("hv-iso".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.reload_failure_state
        .apply_config_watcher_init_pass(Some(watcher_err(None, "seed: config init failed")));
    assert!(
        view.reload_failure_state.has_any_failure(),
        "precondition: config latch is set"
    );

    view.rewire_disk_subscriptions(&["hv-iso".to_string()]);

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
    state
        .apply_disk_watcher_init_pass(Some(watcher_err(Some("agg-disk"), "disk subscribe denied")));
    state.apply_config_watcher_init_pass(Some(watcher_err(None, "config subscribe denied")));

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
        body.contains("- Disk watcher init: agg-disk: disk subscribe denied"),
        "missing disk watcher-init line: {body}"
    );
    assert!(
        body.contains("- Config watcher init: global config: config subscribe denied"),
        "missing config watcher-init line: {body}"
    );
}

#[test]
fn reload_failure_state_watcher_init_failure_lifecycle_is_per_source() {
    let mut state = super::ReloadFailureState::default();

    state.apply_disk_watcher_init_pass(Some(watcher_err(
        Some("life-disk"),
        "first disk install failed",
    )));
    assert!(
        state.has_unacknowledged_failure(),
        "disk_watcher_init_error contributes to has_any_failure"
    );

    state.apply_config_watcher_init_pass(Some(watcher_err(None, "first config install failed")));
    state.acknowledge_dialog();

    state.apply_disk_watcher_init_pass(None);
    assert!(
        state.has_any_failure(),
        "clearing only the disk slot leaves the config slot latched"
    );

    state.apply_config_watcher_init_pass(None);
    assert!(
        !state.has_any_failure(),
        "clearing the last failing source removes all latches"
    );
    assert!(
        !state.has_unacknowledged_failure(),
        "clearing the last failing source resets the ack latch"
    );
}

/// Regression test for #2112: identical disk-watcher-init failures
/// across rewire passes must not re-arm the ack latch.
#[test]
fn reload_failure_ack_persists_across_identical_rewire_failures() {
    let mut state = super::ReloadFailureState::default();

    state.apply_disk_watcher_init_pass(Some(watcher_err(Some("p"), "permission denied")));
    state.acknowledge_dialog();
    assert!(!state.has_unacknowledged_failure());

    state.apply_disk_watcher_init_pass(Some(watcher_err(Some("p"), "permission denied")));
    assert!(
        !state.has_unacknowledged_failure(),
        "identical failure across rewire passes must not re-arm the ack latch (#2112)"
    );
}

/// A different classified error kind (new root cause with different
/// remediation) is treated as a fresh failure and re-arms the dialog
/// even when the source slot is unchanged.
#[test]
fn reload_failure_re_arms_when_error_kind_changes() {
    use crate::file_watch::WatchErrorKind;
    let mut state = super::ReloadFailureState::default();

    state.apply_disk_watcher_init_pass(Some(super::WatcherInitError {
        profile: Some("p".to_string()),
        kind: super::WatcherInitErrorKind::Watch(WatchErrorKind::ResourceExhausted),
        message: "EMFILE".to_string(),
    }));
    state.acknowledge_dialog();

    state.apply_disk_watcher_init_pass(Some(super::WatcherInitError {
        profile: Some("p".to_string()),
        kind: super::WatcherInitErrorKind::Watch(WatchErrorKind::Permission),
        message: "EACCES".to_string(),
    }));
    assert!(
        state.has_unacknowledged_failure(),
        "a changed error kind surfaces a fresh notification"
    );
}

/// Defense-in-depth against `notify`-rs Display drift: the same
/// classified failure on the same profile is ack-equal even when the
/// formatted message string differs. Without this, a future
/// `notify::Error` Display change (e.g. an added watch descriptor)
/// would re-arm the dialog on the same persistent failure and the
/// #2112 fix would silently regress.
#[test]
fn reload_failure_ack_persists_when_message_drifts_but_kind_is_stable() {
    use crate::file_watch::WatchErrorKind;
    let mut state = super::ReloadFailureState::default();

    state.apply_disk_watcher_init_pass(Some(super::WatcherInitError {
        profile: Some("p".to_string()),
        kind: super::WatcherInitErrorKind::Watch(WatchErrorKind::ResourceExhausted),
        message: "Too many open files (os error 24)".to_string(),
    }));
    state.acknowledge_dialog();

    state.apply_disk_watcher_init_pass(Some(super::WatcherInitError {
        profile: Some("p".to_string()),
        kind: super::WatcherInitErrorKind::Watch(WatchErrorKind::ResourceExhausted),
        message: "EMFILE: watch descriptor 8192 exhausted on /tmp/p".to_string(),
    }));
    assert!(
        !state.has_unacknowledged_failure(),
        "a drifted message with stable kind must not re-arm the ack latch"
    );
}

/// A failure crossing the `Watch` <-> `Resolution` variant boundary
/// is a genuine root-cause change (kernel-backend failure vs app-dir
/// resolution failure have disjoint remediations) and re-arms the
/// dialog. Locks the cross-variant arm of the ack-equality contract.
#[test]
fn reload_failure_re_arms_when_kind_crosses_watch_resolution_boundary() {
    use crate::file_watch::WatchErrorKind;
    let mut state = super::ReloadFailureState::default();

    state.apply_config_watcher_init_pass(Some(super::WatcherInitError {
        profile: None,
        kind: super::WatcherInitErrorKind::Watch(WatchErrorKind::Backend),
        message: "subscribe failed".to_string(),
    }));
    state.acknowledge_dialog();

    state.apply_config_watcher_init_pass(Some(super::WatcherInitError {
        profile: None,
        kind: super::WatcherInitErrorKind::Resolution,
        message: "app dir resolution failed".to_string(),
    }));
    assert!(
        state.has_unacknowledged_failure(),
        "a kind transition between Watch and Resolution surfaces a fresh notification"
    );
}

/// Profile name is part of the failure's ack-identity, so a relocation
/// re-arms the dialog under stable error content.
#[test]
fn reload_failure_re_arms_when_profile_changes() {
    let mut state = super::ReloadFailureState::default();

    state.apply_disk_watcher_init_pass(Some(watcher_err(Some("a"), "denied")));
    state.acknowledge_dialog();

    state.apply_disk_watcher_init_pass(Some(watcher_err(Some("b"), "denied")));
    assert!(
        state.has_unacknowledged_failure(),
        "a failure relocating to a different profile re-arms the dialog"
    );
}

/// A new failing source appearing during an already-acknowledged
/// burst re-arms the dialog so the additional failure is surfaced.
#[test]
fn reload_failure_re_arms_when_new_source_joins_burst() {
    let mut state = super::ReloadFailureState::default();

    state.apply_disk_watcher_init_pass(Some(watcher_err(Some("p"), "denied")));
    state.acknowledge_dialog();
    assert!(!state.has_unacknowledged_failure());

    state.apply_config_watcher_init_pass(Some(watcher_err(None, "denied")));
    assert!(
        state.has_unacknowledged_failure(),
        "a NEW failing source during an acked burst surfaces a fresh notification"
    );
}

/// When two sources are failing and the user has acknowledged, a
/// same-content `apply_*_init_pass` on the unrelated slot must not
/// re-arm the dialog. Locks per-source isolation of the ack semantics.
#[test]
fn reload_failure_ack_persists_when_only_other_source_clears_and_rerecords() {
    let mut state = super::ReloadFailureState::default();

    state.apply_disk_watcher_init_pass(Some(watcher_err(Some("p"), "disk denied")));
    state.apply_config_watcher_init_pass(Some(watcher_err(None, "config denied")));
    state.acknowledge_dialog();
    assert!(!state.has_unacknowledged_failure());

    state.apply_config_watcher_init_pass(Some(watcher_err(None, "config denied")));
    assert!(
        !state.has_unacknowledged_failure(),
        "an unchanged adjacent-source rewire pass must not re-arm the ack latch"
    );
}

/// A failure that fully clears and then later returns produces a fresh
/// dialog even if its content is byte-identical to the previously
/// acknowledged failure. Locks the recovery-then-refailure UX.
#[test]
fn reload_failure_re_arms_when_failure_fully_clears_then_returns() {
    let mut state = super::ReloadFailureState::default();

    state.apply_disk_watcher_init_pass(Some(watcher_err(Some("p"), "EMFILE")));
    state.acknowledge_dialog();
    assert!(!state.has_unacknowledged_failure());

    state.apply_disk_watcher_init_pass(None);
    assert!(!state.has_any_failure(), "all sources cleared");

    state.apply_disk_watcher_init_pass(Some(watcher_err(Some("p"), "EMFILE")));
    assert!(
        state.has_unacknowledged_failure(),
        "a failure that fully cleared and then returned must surface a fresh dialog"
    );
}

/// Locks the body-refresh invariant: a `Reload Failed` dialog already
/// on screen must rebuild its body when a new failure source is
/// recorded, so the user sees every failed source without dismissing
/// and reopening the dialog.
#[tokio::test]
#[serial]
async fn try_present_reload_failure_dialog_refreshes_body_for_new_source() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("body-refresh").expect("seed dir");

    let mut view = HomeView::new(
        Some("body-refresh".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let storage_err: anyhow::Result<()> = Err(anyhow::anyhow!("first source"));
    view.reload_failure_state.record_storage(&storage_err);

    assert!(
        view.try_present_reload_failure_dialog(),
        "first call presents the dialog"
    );
    let dialog = view.info_dialog.as_ref().expect("dialog presented");
    assert_eq!(dialog.title(), "Reload Failed");

    let config_err: anyhow::Result<()> = Err(anyhow::anyhow!("second source"));
    view.reload_failure_state.record_config(&config_err);
    assert!(
        view.reload_failure_state.has_unacknowledged_failure(),
        "recording a new source while a dialog is acked re-arms the latch"
    );

    assert!(
        view.try_present_reload_failure_dialog(),
        "second call refreshes the body and re-acknowledges"
    );

    let body = view.reload_failure_state.build_dialog_body();
    assert!(
        body.contains("first source"),
        "refreshed body must keep the original failure source: {body}"
    );
    assert!(
        body.contains("second source"),
        "refreshed body must include the newly recorded source: {body}"
    );
}

/// Locks the foreign-dialog skip invariant: when an unrelated dialog
/// (a `Watcher Warning` from `rewire_after_profile_delete`, or an
/// `Error` from a profile create/delete failure) occupies the slot,
/// `try_present_reload_failure_dialog` returns `false` and leaves the
/// ack latch untouched, so the next tick can present once the
/// foreign dialog is dismissed.
#[tokio::test]
#[serial]
async fn try_present_reload_failure_dialog_skips_while_foreign_dialog_occupies_slot() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("foreign-skip").expect("seed dir");

    let mut view = HomeView::new(
        Some("foreign-skip".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let storage_err: anyhow::Result<()> = Err(anyhow::anyhow!("storage broken"));
    view.reload_failure_state.record_storage(&storage_err);
    view.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
        "Watcher Warning",
        "unrelated message",
    ));

    assert!(
        !view.try_present_reload_failure_dialog(),
        "presentation must skip while a foreign dialog occupies the slot"
    );
    assert_eq!(
        view.info_dialog
            .as_ref()
            .expect("foreign dialog still up")
            .title(),
        "Watcher Warning",
        "the foreign dialog stays in place untouched"
    );
    assert!(
        view.reload_failure_state.has_unacknowledged_failure(),
        "the ack latch stays armed so the next tick re-tries presentation \
         after the foreign dialog is dismissed"
    );

    view.info_dialog = None;
    assert!(
        view.try_present_reload_failure_dialog(),
        "after the foreign dialog is dismissed the next call presents"
    );
    assert_eq!(
        view.info_dialog.as_ref().expect("dialog presented").title(),
        "Reload Failed"
    );
}

/// Locks the install-loop resurrection-prevention invariant for
/// `rewire_config_subscriptions`: the install loop resolves each
/// `to_add` profile through the non-creating `get_profile_dir_path`
/// and skips when the directory is absent, so a peer-process delete
/// that races the `list_profiles()` snapshot does not recreate the
/// profile directory as an empty stub via the install path.
#[tokio::test]
#[serial]
async fn rewire_config_subscriptions_install_loop_skips_missing_profile_dir() {
    use super::ConfigWatchKey;
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("active").expect("seed active");

    let mut view = HomeView::new(
        Some("active".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let stale_name = "stale-deleted";
    let stale_path = crate::session::get_profile_dir_path(stale_name).expect("path");
    assert!(
        !stale_path.exists(),
        "precondition: stale profile dir is absent (peer delete raced the snapshot)"
    );

    view.rewire_config_subscriptions(&["active".to_string(), stale_name.to_string()]);

    assert!(
        !stale_path.exists(),
        "the install loop must not recreate a deleted profile dir; \
         a stale snapshot listing a missing profile is skipped, not resurrected"
    );
    assert!(
        !view
            .config_watch
            .handles
            .contains_key(&ConfigWatchKey::profile(stale_name)),
        "no config-watch handle is installed for a missing profile dir"
    );
    assert!(
        view.config_watch
            .handles
            .contains_key(&ConfigWatchKey::profile("active")),
        "the active profile (which exists) is still subscribed"
    );
}

/// Locks the install-loop resurrection-prevention invariant for
/// `rewire_disk_subscriptions`: same shape as the config-watch
/// sibling. A stale snapshot that lists a missing profile is
/// skipped, not resurrected via `fs::create_dir_all`.
#[tokio::test]
#[serial]
async fn rewire_disk_subscriptions_install_loop_skips_missing_profile_dir() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("disk-active").expect("seed active");

    let mut view = HomeView::new(
        Some("disk-active".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let stale_name = "disk-stale-deleted";
    let stale_path = crate::session::get_profile_dir_path(stale_name).expect("path");
    assert!(
        !stale_path.exists(),
        "precondition: stale profile dir is absent"
    );

    view.rewire_disk_subscriptions(&["disk-active".to_string(), stale_name.to_string()]);

    assert!(
        !stale_path.exists(),
        "the disk-watch install loop must not recreate a deleted profile dir"
    );
    assert!(
        !view.disk_watch.handles.contains_key(stale_name),
        "no disk-watch handle is installed for a missing profile dir"
    );
    assert!(
        view.disk_watch.handles.contains_key("disk-active"),
        "the active profile (which exists) is still subscribed"
    );
}

/// Locks the partial-recovery body-refresh invariant: when one
/// failing source recovers while another stays failed, the
/// `Reload Failed` dialog body rebuilds in place to drop the
/// recovered source's line. The ack latch stays in place so the
/// user is not re-notified for the same ongoing burst.
#[tokio::test]
#[serial]
async fn try_present_reload_failure_dialog_refreshes_body_on_partial_recovery() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("partial-recovery").expect("seed dir");

    let mut view = HomeView::new(
        Some("partial-recovery".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let storage_err: anyhow::Result<()> = Err(anyhow::anyhow!("storage broken"));
    let config_err: anyhow::Result<()> = Err(anyhow::anyhow!("config broken"));
    view.reload_failure_state.record_storage(&storage_err);
    view.reload_failure_state.record_config(&config_err);

    assert!(
        view.try_present_reload_failure_dialog(),
        "first call presents the dialog"
    );
    let initial_body = view
        .info_dialog
        .as_ref()
        .expect("dialog presented")
        .message()
        .to_string();
    assert!(
        initial_body.contains("storage broken") && initial_body.contains("config broken"),
        "initial body lists both failing sources: {initial_body}"
    );
    assert!(
        !view.reload_failure_state.has_unacknowledged_failure(),
        "presentation consumed the ack latch"
    );

    let storage_ok: anyhow::Result<()> = Ok(());
    view.reload_failure_state.record_storage(&storage_ok);
    assert!(
        view.reload_failure_state.has_any_failure(),
        "config still failing keeps has_any_failure true"
    );
    assert!(
        !view.reload_failure_state.has_unacknowledged_failure(),
        "partial recovery does not re-arm the ack latch"
    );

    assert!(
        view.try_present_reload_failure_dialog(),
        "the body-refresh path must update an on-screen dialog when the failing-source set shifts"
    );

    let refreshed_body = view
        .info_dialog
        .as_ref()
        .expect("dialog still on screen")
        .message();
    assert!(
        refreshed_body.contains("config broken"),
        "refreshed body keeps the still-failing source: {refreshed_body}"
    );
    assert!(
        !refreshed_body.contains("storage broken"),
        "refreshed body drops the recovered source's line: {refreshed_body}"
    );
}

/// Locks the e2e debug-counter export contract:
/// `try_refresh_from_config_watcher` increments
/// `watcher_config_refresh_count` on every invocation and, when
/// `AOE_E2E_DEBUG=1` is set on the process, writes the new count to
/// `<app_dir>/.aoe_e2e_refresh_count` so the e2e harness polls a
/// deterministic completion signal for the watcher path.
#[tokio::test]
#[serial]
async fn watcher_config_refresh_count_exports_to_e2e_debug_file() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("e2e-debug").expect("seed dir");

    let mut view = HomeView::new(
        Some("e2e-debug".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let app_dir = crate::session::get_app_dir().expect("app_dir");
    let counter_path = app_dir.join(".aoe_e2e_refresh_count");

    assert!(
        !counter_path.exists(),
        "precondition: counter file does not exist on a fresh test app dir"
    );
    let _ = view.try_refresh_from_config_watcher();
    assert_eq!(
        view.watcher_config_refresh_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the counter increments on every watcher refresh attempt"
    );
    assert!(
        !counter_path.exists(),
        "AOE_E2E_DEBUG unset keeps the export file absent so production \
         builds and unrelated test runs leave the disk untouched"
    );

    let _guard = E2eDebugGuard::enable();

    let _ = view.try_refresh_from_config_watcher();
    let exported = std::fs::read_to_string(&counter_path)
        .expect("counter file is written once AOE_E2E_DEBUG=1");
    assert_eq!(
        exported.trim(),
        "2",
        "exported value matches the post-increment counter"
    );

    let _ = view.try_refresh_from_config_watcher();
    let exported = std::fs::read_to_string(&counter_path)
        .expect("counter file refreshes on subsequent attempts");
    assert_eq!(
        exported.trim(),
        "3",
        "subsequent attempts re-export the latest counter value"
    );
}

/// Locks consumer-side identity invalidation for the config rewire:
/// when a peer recreates a profile dir between heartbeats, the
/// canonical path string is unchanged but the inode is fresh, and
/// `rewire_config_subscriptions` MUST rebuild the entry; the
/// early-return fast path on an unchanged name set must not skip
/// the install loop in this case.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn rewire_config_invalidates_on_inode_change_with_same_canonical_path() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    let _seed = crate::session::get_profile_dir("inode-drift-cfg").expect("seed dir");

    let mut view = HomeView::new(
        Some("inode-drift-cfg".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    use super::ConfigWatchKey;
    let identity_before = view
        .config_watch
        .handles
        .get(&ConfigWatchKey::profile("inode-drift-cfg"))
        .expect("initial install populated config entry")
        .installed_identity;

    let profile_dir =
        crate::session::get_profile_dir_path("inode-drift-cfg").expect("resolve profile dir");
    // Cross a kernel coarse-clock tick before the recreate so the new
    // dir's btime cannot tie with the seed dir's even when the inode
    // number is recycled (see the sibling primitive test for details).
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::remove_dir_all(&profile_dir).expect("remove first incarnation");
    std::fs::create_dir_all(&profile_dir).expect("recreate same-name dir");

    view.rewire_config_subscriptions(&["inode-drift-cfg".to_string()]);

    let identity_after = view
        .config_watch
        .handles
        .get(&ConfigWatchKey::profile("inode-drift-cfg"))
        .expect("entry rebuilt after inode drift")
        .installed_identity;

    assert_ne!(
        identity_before, identity_after,
        "rewire must rebuild the entry when the inode drifts even though the canonical path string is unchanged"
    );
}

/// Disk-side mirror of the config invalidation test. The disk
/// rewire path shares the same identity-tracking field on
/// `DiskWatchEntry`, so the regression locks both consumers.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn rewire_disk_invalidates_on_inode_change_with_same_canonical_path() {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    let _seed = crate::session::get_profile_dir("inode-drift-disk").expect("seed dir");

    let mut view = HomeView::new(
        Some("inode-drift-disk".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    let identity_before = view
        .disk_watch
        .handles
        .get("inode-drift-disk")
        .expect("initial install populated disk entry")
        .installed_identity;

    let profile_dir =
        crate::session::get_profile_dir_path("inode-drift-disk").expect("resolve profile dir");
    // Cross a kernel coarse-clock tick before the recreate so the new
    // dir's btime cannot tie with the seed dir's even when the inode
    // number is recycled (see the sibling primitive test for details).
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::remove_dir_all(&profile_dir).expect("remove first incarnation");
    std::fs::create_dir_all(&profile_dir).expect("recreate same-name dir");

    view.rewire_disk_subscriptions(&["inode-drift-disk".to_string()]);

    let identity_after = view
        .disk_watch
        .handles
        .get("inode-drift-disk")
        .expect("entry rebuilt after inode drift")
        .installed_identity;

    assert_ne!(
        identity_before, identity_after,
        "rewire must rebuild the entry when the inode drifts even though the canonical path string is unchanged"
    );
}
