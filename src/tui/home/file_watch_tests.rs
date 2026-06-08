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

/// Locks the no-op fast-path invariant: when `rewire_disk_subscriptions`
/// is called with an unchanged profile set and no inode invalidation, the
/// fast-path returns without running the install loop, and a previously
/// latched `watcher_init_error` must be preserved (the install loop is
/// the only path that re-latches via `record_watcher_init_failure`).
#[tokio::test]
#[serial]
async fn rewire_no_op_preserves_latched_watcher_init_failure() {
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
        .record_watcher_init_failure("seed: simulated prior failure");
    assert!(
        view.reload_failure_state.has_any_failure(),
        "precondition: latch is set"
    );

    view.rewire_disk_subscriptions(&["hv-noop".to_string()])
        .expect("no-op rewire");

    assert!(
        view.reload_failure_state.has_any_failure(),
        "no-op rewire (unchanged set, no inode change) must preserve the watcher_init_error latch"
    );
}
