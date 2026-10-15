//! `HomeView` file-watch wiring tests against a live `FileWatchService`.

#![cfg(test)]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;

use super::tests::live_send_state;
use super::watchers::{ConfigWatchKey, ReloadFailureState, WatcherInitError, WatcherInitErrorKind};
use super::HomeView;
use crate::file_watch::{FileWatchService, WatchErrorKind};
use crate::session::test_support::{isolate_home, HomeGuard};
use crate::session::{Instance, Item, Storage};

fn init_err(profile: Option<&str>, kind: WatcherInitErrorKind, message: &str) -> WatcherInitError {
    WatcherInitError {
        profile: profile.map(str::to_owned),
        kind,
        message: message.to_owned(),
    }
}

fn watcher_err(profile: Option<&str>, message: &str) -> WatcherInitError {
    init_err(
        profile,
        WatcherInitErrorKind::Watch(WatchErrorKind::Other),
        message,
    )
}

struct Env {
    view: HomeView,
    live: Arc<FileWatchService>,
    temp: TempDir,
    _home: HomeGuard,
}

/// Isolated home with `seed_dirs` created, and a live-watch view on `profile`.
fn env(profile: &str, seed_dirs: &[&str]) -> Env {
    env_seeded(profile, seed_dirs, |_| {})
}

/// Like [`env`], running `seed` before the view loads.
fn env_seeded(profile: &str, seed_dirs: &[&str], seed: impl FnOnce(&Arc<FileWatchService>)) -> Env {
    let temp = TempDir::new().expect("tempdir");
    let _home = isolate_home(temp.path());
    let live = FileWatchService::new().expect("live svc");
    for dir in seed_dirs {
        crate::session::get_profile_dir(dir).expect("seed dir");
    }
    seed(&live);
    let view = HomeView::new_for_test(
        Some(profile.to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");
    Env {
        view,
        live,
        temp,
        _home,
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| n.to_string()).collect()
}

fn has_config_watch(view: &HomeView, profile: &str) -> bool {
    view.config_watch
        .handles
        .contains_key(&ConfigWatchKey::profile(profile))
}

fn session_row(view: &HomeView, session_id: &str) -> Option<usize> {
    view.flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { id, .. } if id == session_id))
}

#[tokio::test]
#[serial]
async fn peer_write_flips_disk_dirty() {
    let mut e = env("hv-adapter", &["hv-adapter"]);
    e.view.rewire_disk_subscriptions(&names(&["hv-adapter"]));

    Storage::new("hv-adapter", e.live.clone())
        .expect("writer")
        .update(|i, _g| {
            *i = vec![Instance::new("peer-write", "/tmp/peer")];
            Ok(())
        })
        .expect("peer write");

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !e.view.disk_watch.dirty.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "peer write must flip disk_dirty"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Dropping a profile from either rewire removes its entry, and re-adding a config watch
/// reuses the subscription slot instead of leaking or double-subscribing.
#[tokio::test]
#[serial]
async fn rewire_drops_removed_profiles_without_leaking_subscriptions() {
    let mut e = env("hv-keep", &["hv-keep", "hv-drop"]);
    e.view
        .rewire_disk_subscriptions(&names(&["hv-keep", "hv-drop"]));
    assert!(e.view.disk_watch.handles.contains_key("hv-drop"));
    e.view.rewire_disk_subscriptions(&names(&["hv-keep"]));
    let keys: Vec<_> = e.view.disk_watch.handles.keys().collect();
    assert_eq!(keys, ["hv-keep"]);

    e.view.rewire_config_subscriptions(&names(&["hv-drop"]));
    let baseline = e.live.subscriber_count();
    assert!(has_config_watch(&e.view, "hv-drop"));
    e.view.rewire_config_subscriptions(&[]);
    assert!(!has_config_watch(&e.view, "hv-drop"));
    e.view.rewire_config_subscriptions(&names(&["hv-drop"]));
    assert!(has_config_watch(&e.view, "hv-drop"));
    assert_eq!(e.live.subscriber_count(), baseline);
}

/// Rewires resolve profile dirs without creating them, in both the invalidation pass and install loop.
#[tokio::test]
#[serial]
async fn rewire_never_resurrects_missing_profile_dirs() {
    let mut e = env("ghost", &["ghost"]);
    let ghost_dir = crate::session::get_profile_dir_path("ghost").unwrap();
    e.view.rewire_config_subscriptions(&names(&["ghost"]));
    assert!(has_config_watch(&e.view, "ghost"));
    std::fs::remove_dir_all(&ghost_dir).expect("delete profile dir");
    e.view.rewire_config_subscriptions(&[]);
    assert!(!ghost_dir.exists());

    // A stale snapshot listing a deleted profile is skipped by both install loops.
    let stale = "stale-deleted";
    let stale_dir = crate::session::get_profile_dir_path(stale).unwrap();
    crate::session::get_profile_dir("active").unwrap();
    e.view
        .rewire_config_subscriptions(&names(&["active", stale]));
    e.view.rewire_disk_subscriptions(&names(&["active", stale]));
    assert!(!stale_dir.exists());
    assert!(!has_config_watch(&e.view, stale));
    assert!(has_config_watch(&e.view, "active"));
    assert!(!e.view.disk_watch.handles.contains_key(stale));
    assert!(e.view.disk_watch.handles.contains_key("active"));
}

/// `--profile X` scopes disk watches to X; config watches cover every profile on disk.
#[tokio::test]
#[serial]
async fn single_profile_mode_scopes_disk_watch_but_not_config_watch() {
    let mut e = env(
        "active-only",
        &["active-only", "peer-one", "peer-two", "peer-deleted"],
    );
    e.view.reload_storage_only().expect("reload");
    let keys: Vec<_> = e.view.disk_watch.handles.keys().collect();
    assert_eq!(keys, ["active-only"]);
    assert!(has_config_watch(&e.view, "peer-one"));
    assert!(has_config_watch(&e.view, "peer-two"));

    let deleted_dir = crate::session::get_profile_dir_path("peer-deleted").unwrap();
    std::fs::remove_dir_all(&deleted_dir).expect("remove peer-deleted");
    e.view.rewire_after_profile_delete("peer-deleted");
    let keys: Vec<_> = e.view.disk_watch.handles.keys().collect();
    assert_eq!(keys, ["active-only"]);
    assert!(has_config_watch(&e.view, "peer-one"));
    assert!(!has_config_watch(&e.view, "peer-deleted"));
}

#[tokio::test]
#[serial]
async fn reload_storage_only_preserves_live_send_state_while_adding_peer_row() {
    let mut active = Instance::new("active-live", "/tmp/active-live");
    active.source_profile = "live-refresh".to_string();
    let active_id = active.id.clone();
    let mut e = env_seeded("live-refresh", &[], |live| {
        Storage::new("live-refresh", live.clone())
            .expect("writer")
            .update(|instances, _groups| {
                instances.push(active);
                Ok(())
            })
            .expect("seed active row");
    });
    let writer = Storage::new("live-refresh", e.live.clone()).expect("writer");
    let view = &mut e.view;
    view.cursor = session_row(view, &active_id).expect("active row in flat items");
    view.update_selected();

    let tmux_name = crate::tmux::Session::resolve_name(&active_id, "active-live");
    view.live_send = Some(live_send_state(&active_id, "active-live", &tmux_name));
    view.pending_paste = Some("queued paste".to_string());
    view.preview_capture_target = Some(tmux_name.clone());
    view.live_send_last_resize = Some((100, 30));
    view.trashed_section_collapsed = true;
    view.search_active = true;
    view.search_query = tui_input::Input::new("peer-added".to_string());

    writer
        .update(|instances, _groups| {
            instances
                .iter_mut()
                .find(|inst| inst.id == active_id)
                .expect("active row on disk")
                .trash();
            let mut peer = Instance::new("peer-added", "/tmp/peer-added");
            peer.source_profile = "live-refresh".to_string();
            instances.push(peer);
            Ok(())
        })
        .expect("peer write");

    view.reload_storage_only().expect("storage-only reload");

    assert!(view
        .get_instance(&active_id)
        .is_some_and(Instance::is_trashed));
    assert!(
        session_row(view, &active_id).is_none(),
        "precondition: the active row moved under the collapsed trash shelf"
    );
    assert_eq!(view.selected_session.as_deref(), Some(active_id.as_str()));
    assert_eq!(view.pending_paste.as_deref(), Some("queued paste"));
    assert_eq!(
        view.preview_capture_target.as_deref(),
        Some(tmux_name.as_str())
    );
    assert_eq!(view.live_send_last_resize, Some((100, 30)));
    assert_eq!(
        view.displayed_pane_tmux_name().as_deref(),
        Some(tmux_name.as_str()),
        "preview capture must stay pinned to the live-send pane"
    );
    let peer_index = view
        .flat_items
        .iter()
        .position(|item| {
            matches!(item, Item::Session { id, .. }
                if view.get_instance(id).is_some_and(|inst| inst.title == "peer-added"))
        })
        .expect("peer row remains visible");
    assert!(view.search_matches.contains(&peer_index));
    assert!(
        !view.is_sidebar_item_selected(&view.flat_items[peer_index], peer_index),
        "cursor fallback must not visibly retarget selection during live-send"
    );
    let state = view.live_send.as_ref().expect("live-send remains active");
    assert_eq!(
        (
            state.session_id.as_str(),
            state.title.as_str(),
            state.tmux_name.as_str()
        ),
        (active_id.as_str(), "active-live", tmux_name.as_str())
    );
    assert_eq!(state.target, super::live_send::LiveSendTarget::Agent);
}

#[tokio::test]
#[serial]
async fn reload_storage_only_ends_live_send_when_active_row_is_removed() {
    let active = Instance::new("active-live", "/tmp/active-live");
    let active_id = active.id.clone();
    let peer = Instance::new("peer", "/tmp/peer");
    let peer_id = peer.id.clone();
    let mut e = env_seeded("live-refresh", &[], |live| {
        Storage::new("live-refresh", live.clone())
            .expect("writer")
            .update(|instances, _groups| {
                instances.extend([active, peer]);
                Ok(())
            })
            .expect("seed rows");
    });
    let writer = Storage::new("live-refresh", e.live.clone()).expect("writer");
    let view = &mut e.view;
    view.cursor = session_row(view, &active_id).expect("active row in flat items");
    view.update_selected();
    view.live_send = Some(live_send_state(
        &active_id,
        "active-live",
        "aoe_test_removed_live_refresh",
    ));

    writer
        .update(|instances, _groups| {
            instances.retain(|instance| instance.id != active_id);
            Ok(())
        })
        .expect("remove active row");
    view.reload_storage_only().expect("storage-only reload");

    assert!(view.get_instance(&active_id).is_none());
    assert!(
        view.live_send.is_none(),
        "deleted target must end live-send"
    );
    assert_eq!(
        view.info_dialog.as_ref().map(|dialog| dialog.title()),
        Some("Live send ended")
    );
    assert_eq!(view.selected_session.as_deref(), Some(peer_id.as_str()));
}

#[tokio::test]
#[serial]
async fn reload_failure_dialog_waits_until_live_send_exits() {
    let mut e = env("live-failure", &[]);
    let view = &mut e.view;
    view.live_send = Some(live_send_state("active", "active", "aoe_test_live_failure"));
    view.reload_failure_state
        .record_storage(&Err(anyhow::anyhow!("disk unreadable")));

    assert!(!view.try_present_reload_failure_dialog());
    assert!(view.info_dialog.is_none());
    assert!(view.reload_failure_state.has_unacknowledged_failure());

    view.live_send = None;
    assert!(view.try_present_reload_failure_dialog());
    assert_eq!(
        view.info_dialog.as_ref().map(|dialog| dialog.title()),
        Some("Reload Failed")
    );
}

/// A `list_profiles` failure degrades a storage-only reload instead of failing it. After a
/// profile delete it raises a Watcher Warning that sits outside `reload_failure_state`,
/// survives the recovery-edge cleanup, and never replaces another dialog.
#[tokio::test]
#[serial]
async fn list_profiles_failure_degrades_reload_and_warns_on_rewire() {
    let mut e = env("seam-test", &["seam-test"]);
    Storage::new("seam-test", e.live.clone())
        .expect("writer")
        .update(|instances, _groups| {
            *instances = vec![Instance::new("fallback-row", "/tmp/fallback")];
            Ok(())
        })
        .expect("peer write");
    let _fail_guard = crate::session::FailNextListProfilesGuard::new();
    e.view
        .reload_storage_only()
        .expect("reload should degrade, not fail");
    assert!(e
        .view
        .instances
        .values()
        .any(|inst| inst.title == "fallback-row"));
    assert!(crate::session::list_profiles().is_ok());

    let _fail_guard = crate::session::FailNextListProfilesGuard::new();
    e.view.rewire_after_profile_delete("seam-test");
    assert!(
        crate::session::list_profiles().is_ok(),
        "seam must auto-clear after firing once"
    );
    assert_eq!(
        e.view.info_dialog.as_ref().map(|d| d.title()),
        Some("Watcher Warning")
    );
    assert!(!e.view.reload_failure_state.has_any_failure());
    assert!(!e.view.try_clear_recovered_reload_dialog());
    assert_eq!(
        e.view.info_dialog.as_ref().map(|d| d.title()),
        Some("Watcher Warning")
    );

    e.view.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
        "Existing dialog",
        "keep me",
    ));
    let _fail_guard = crate::session::FailNextListProfilesGuard::new();
    e.view.rewire_after_profile_delete("seam-test");
    assert_eq!(
        e.view.info_dialog.as_ref().map(|d| d.title()),
        Some("Existing dialog")
    );
}

/// The rewire fast path keeps a latched init failure only while its profile is still current.
#[tokio::test]
#[serial]
async fn rewire_fast_path_preserves_relevant_latch_and_clears_stale_one() {
    let mut e = env("hv-noop", &["hv-noop"]);
    let current = names(&["hv-noop"]);
    e.view.rewire_disk_subscriptions(&current);
    assert!(e.view.disk_watch.handles.contains_key("hv-noop"));

    let state = &mut e.view.reload_failure_state;
    state.apply_disk_watcher_init_pass(Some(watcher_err(Some("hv-noop"), "prior failure")));
    e.view.rewire_disk_subscriptions(&current);
    assert!(e
        .view
        .reload_failure_state
        .disk_watcher_init_error
        .is_some());

    // A config latch is an independent slot: a disk rewire never clears it.
    e.view
        .reload_failure_state
        .apply_config_watcher_init_pass(Some(watcher_err(None, "config init failed")));
    e.view
        .reload_failure_state
        .apply_disk_watcher_init_pass(Some(watcher_err(Some("ghost"), "stale")));
    e.view.rewire_disk_subscriptions(&current);
    assert!(e
        .view
        .reload_failure_state
        .disk_watcher_init_error
        .is_none());
    assert!(e
        .view
        .reload_failure_state
        .config_watcher_init_error
        .is_some());

    e.view
        .reload_failure_state
        .apply_config_watcher_init_pass(Some(watcher_err(Some("ghost"), "stale")));
    e.view.rewire_config_subscriptions(&current);
    assert!(e
        .view
        .reload_failure_state
        .config_watcher_init_error
        .is_none());
}

#[derive(Clone, Copy)]
enum Slot {
    Storage,
    Config,
    DiskInit,
    ConfigInit,
}

enum Step {
    Fail(Slot),
    Heal(Slot),
    InitErr(Slot, WatcherInitError),
    Ack,
    /// `(unacknowledged, any failure)` expected at this point.
    Expect(bool, bool),
}

fn run_steps(name: &str, steps: Vec<Step>) -> ReloadFailureState {
    let mut s = ReloadFailureState::default();
    for (i, step) in steps.into_iter().enumerate() {
        match step {
            Step::Fail(Slot::Storage) => {
                assert!(!s.record_storage(&Err(anyhow::anyhow!("storage err"))));
            }
            Step::Fail(Slot::Config) => {
                assert!(!s.record_config(&Err(anyhow::anyhow!("config err"))));
            }
            Step::Heal(Slot::Storage) => {
                s.record_storage(&Ok(()));
            }
            Step::Heal(Slot::Config) => {
                s.record_config(&Ok(()));
            }
            Step::Heal(Slot::DiskInit) => s.apply_disk_watcher_init_pass(None),
            Step::Heal(Slot::ConfigInit) => s.apply_config_watcher_init_pass(None),
            Step::InitErr(Slot::DiskInit, e) => s.apply_disk_watcher_init_pass(Some(e)),
            Step::InitErr(Slot::ConfigInit, e) => s.apply_config_watcher_init_pass(Some(e)),
            Step::Fail(_) | Step::InitErr(..) => unreachable!("invalid step"),
            Step::Ack => s.acknowledge_dialog(),
            Step::Expect(unacked, any) => assert_eq!(
                (s.has_unacknowledged_failure(), s.has_any_failure()),
                (unacked, any),
                "{name}: step {i}"
            ),
        }
    }
    s
}

#[test]
fn reload_failure_ack_latch() {
    use Slot::*;
    use Step::*;
    let resource = WatcherInitErrorKind::Watch(WatchErrorKind::ResourceExhausted);
    let cases: Vec<(&str, Vec<Step>)> = vec![
        (
            "recovery clears the failure and the ack latch",
            vec![
                Fail(Storage),
                Ack,
                Expect(false, true),
                Heal(Storage),
                Expect(false, false),
            ],
        ),
        (
            "a new source failing in an acked burst re-arms",
            vec![Fail(Storage), Ack, Fail(Config), Expect(true, true)],
        ),
        (
            "init slots clear independently",
            vec![
                InitErr(DiskInit, watcher_err(Some("d"), "disk")),
                Expect(true, true),
                InitErr(ConfigInit, watcher_err(None, "config")),
                Ack,
                Heal(DiskInit),
                Expect(false, true),
                Heal(ConfigInit),
                Expect(false, false),
            ],
        ),
        (
            "identical failure across passes stays acked (#2112)",
            vec![
                InitErr(DiskInit, watcher_err(Some("p"), "denied")),
                Ack,
                InitErr(DiskInit, watcher_err(Some("p"), "denied")),
                Expect(false, true),
            ],
        ),
        (
            "message drift with a stable kind stays acked",
            vec![
                InitErr(
                    DiskInit,
                    init_err(Some("p"), resource, "Too many open files"),
                ),
                Ack,
                InitErr(
                    DiskInit,
                    init_err(Some("p"), resource, "EMFILE: descriptor 8192"),
                ),
                Expect(false, true),
            ],
        ),
        (
            "a changed kind re-arms",
            vec![
                InitErr(DiskInit, init_err(Some("p"), resource, "EMFILE")),
                Ack,
                InitErr(
                    DiskInit,
                    init_err(
                        Some("p"),
                        WatcherInitErrorKind::Watch(WatchErrorKind::Permission),
                        "EACCES",
                    ),
                ),
                Expect(true, true),
            ],
        ),
        (
            "crossing Watch and Resolution re-arms",
            vec![
                InitErr(
                    ConfigInit,
                    init_err(
                        None,
                        WatcherInitErrorKind::Watch(WatchErrorKind::Backend),
                        "x",
                    ),
                ),
                Ack,
                InitErr(
                    ConfigInit,
                    init_err(None, WatcherInitErrorKind::Resolution, "x"),
                ),
                Expect(true, true),
            ],
        ),
        (
            "a changed profile re-arms",
            vec![
                InitErr(DiskInit, watcher_err(Some("a"), "denied")),
                Ack,
                InitErr(DiskInit, watcher_err(Some("b"), "denied")),
                Expect(true, true),
            ],
        ),
        (
            "a new init source joining an acked burst re-arms",
            vec![
                InitErr(DiskInit, watcher_err(Some("p"), "denied")),
                Ack,
                InitErr(ConfigInit, watcher_err(None, "denied")),
                Expect(true, true),
            ],
        ),
        (
            "an unchanged pass on the other slot stays acked",
            vec![
                InitErr(DiskInit, watcher_err(Some("p"), "disk denied")),
                InitErr(ConfigInit, watcher_err(None, "config denied")),
                Ack,
                InitErr(ConfigInit, watcher_err(None, "config denied")),
                Expect(false, true),
            ],
        ),
        (
            "a failure that fully cleared and returned re-arms",
            vec![
                InitErr(DiskInit, watcher_err(Some("p"), "EMFILE")),
                Ack,
                Heal(DiskInit),
                Expect(false, false),
                InitErr(DiskInit, watcher_err(Some("p"), "EMFILE")),
                Expect(true, true),
            ],
        ),
    ];
    for (name, steps) in cases {
        run_steps(name, steps);
    }

    let mut s = ReloadFailureState::default();
    assert!(!s.record_storage(&Err(anyhow::anyhow!("x"))));
    assert!(s.record_storage(&Ok(())), "failed-to-ok edge returns true");
}

#[test]
fn reload_failure_state_dialog_body_aggregates_all_four_sources() {
    use Slot::*;
    use Step::*;
    let state = run_steps(
        "aggregate",
        vec![
            Fail(Storage),
            Fail(Config),
            InitErr(
                DiskInit,
                watcher_err(Some("agg-disk"), "disk subscribe denied"),
            ),
            InitErr(ConfigInit, watcher_err(None, "config subscribe denied")),
        ],
    );
    let body = state.build_dialog_body();
    for line in [
        "- Storage: storage err",
        "- Config: config err",
        "- Disk watcher init: agg-disk: disk subscribe denied",
        "- Config watcher init: global config: config subscribe denied",
    ] {
        assert!(body.contains(line), "missing {line:?}: {body}");
    }
}

/// A Reload Failed dialog waits while a foreign dialog occupies the slot, then rebuilds its
/// body on screen as failing sources come and go.
#[tokio::test]
#[serial]
async fn reload_failure_dialog_body_tracks_failing_sources() {
    let mut e = env("body-refresh", &["body-refresh"]);
    let view = &mut e.view;
    let message = |view: &HomeView| {
        view.info_dialog
            .as_ref()
            .expect("dialog presented")
            .message()
            .to_string()
    };

    view.reload_failure_state
        .record_storage(&Err(anyhow::anyhow!("storage broken")));
    view.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
        "Watcher Warning",
        "unrelated message",
    ));
    assert!(!view.try_present_reload_failure_dialog());
    assert_eq!(
        view.info_dialog.as_ref().unwrap().title(),
        "Watcher Warning"
    );
    assert!(view.reload_failure_state.has_unacknowledged_failure());
    view.info_dialog = None;
    assert!(view.try_present_reload_failure_dialog());
    assert_eq!(view.info_dialog.as_ref().unwrap().title(), "Reload Failed");

    view.reload_failure_state
        .record_config(&Err(anyhow::anyhow!("config broken")));
    assert!(view.reload_failure_state.has_unacknowledged_failure());
    assert!(view.try_present_reload_failure_dialog());
    let body = message(view);
    assert!(body.contains("storage broken") && body.contains("config broken"));
    assert!(!view.reload_failure_state.has_unacknowledged_failure());

    view.reload_failure_state.record_storage(&Ok(()));
    assert!(view.reload_failure_state.has_any_failure());
    assert!(!view.reload_failure_state.has_unacknowledged_failure());
    assert!(view.try_present_reload_failure_dialog());
    let body = message(view);
    assert!(body.contains("config broken") && !body.contains("storage broken"));
}

/// A same-path dir recreated with a new inode forces both rewires to rebuild the entry.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn rewire_invalidates_on_inode_change_with_same_canonical_path() {
    let profile = "inode-drift";
    let mut e = env(profile, &[profile]);
    let config_key = ConfigWatchKey::profile(profile);
    let identities = |view: &HomeView| {
        (
            view.config_watch.handles[&config_key].installed_identity,
            view.disk_watch.handles[profile].installed_identity,
        )
    };
    let (config_before, disk_before) = identities(&e.view);

    let profile_dir = crate::session::get_profile_dir_path(profile).unwrap();
    // Keep the old inode alive so the replacement cannot reuse its identity.
    std::fs::rename(&profile_dir, e.temp.path().join("retired-profile")).unwrap();
    std::fs::create_dir_all(&profile_dir).unwrap();
    let replacement = crate::file_watch::capture_watch_identity(&profile_dir).unwrap();
    assert_ne!(config_before, replacement);
    assert_ne!(disk_before, replacement);

    e.view.rewire_config_subscriptions(&names(&[profile]));
    e.view.rewire_disk_subscriptions(&names(&[profile]));
    assert_eq!(identities(&e.view), (replacement, replacement));
}
