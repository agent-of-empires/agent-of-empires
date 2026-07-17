//! Tests for HomeView

use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;
use tempfile::TempDir;
use tui_input::Input;

use super::{ConfigRefreshOrigin, ConfigWatchKey, HomeView, ViewMode};
use crate::session::{GroupTree, Instance, Item, Storage};
use crate::tmux::AvailableTools;
use crate::tui::app::Action;
use crate::tui::dialogs::{InfoDialog, NewSessionDialog};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn setup_test_home(temp: &TempDir) -> AppDirGuard {
    isolate_app_dir_at(temp.path())
}

fn seed_instances(view: &mut HomeView, insts: &[Instance]) {
    view.instances = insts.iter().cloned().map(|i| (i.id.clone(), i)).collect();
}

struct TestEnv {
    view: HomeView,
    _guard: AppDirGuard,
    _temp: TempDir,
}

fn create_test_env_empty() -> TestEnv {
    use crate::session::config::GroupByMode;
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap(); // ensure profile dir exists
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();
    TestEnv {
        view,
        _guard,
        _temp: temp,
    }
}

// #1897 / CodeRabbit follow-up: `add_instance` is the funnel for both the
// `Creating` placeholder stub (async creation flow) and the finalized session
// row. The opt-in create-trend counter must bump only for finalized inserts, or
// a successful background create double-counts (stub + real) and a cancelled one
// counts a session that never existed. Asserts deltas (not absolutes) since the
// counter is a process-global shared with the `telemetry_creates` serial group.
#[test]
#[serial]
#[serial_test::serial(telemetry_creates)]
fn add_instance_counts_only_finalized_creates() {
    use crate::session::Status;
    let mut env = create_test_env_empty();
    let before = crate::tui::app::session_create_count_for_test();

    let mut stub = Instance::new("stub", "/tmp/test");
    stub.source_profile = "test".to_string();
    stub.status = Status::Creating;
    env.view.add_instance(stub);
    assert_eq!(
        crate::tui::app::session_create_count_for_test(),
        before,
        "a Creating placeholder stub must not bump the create counter"
    );

    let mut real = Instance::new("real", "/tmp/test");
    real.source_profile = "test".to_string();
    env.view.add_instance(real);
    assert_eq!(
        crate::tui::app::session_create_count_for_test(),
        before + 1,
        "a finalized session insert must bump the create counter exactly once"
    );
}

#[test]
#[serial]
fn rewire_disk_subscriptions_is_noop_without_tokio_runtime() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    let current = vec!["test".to_string()];

    assert!(
        view.disk_watch.handles.is_empty(),
        "construction outside a tokio runtime must not prewire subscriptions"
    );
    view.rewire_disk_subscriptions(&current);
    assert!(
        view.disk_watch.handles.is_empty(),
        "rewire outside a tokio runtime must stay a no-op for lib tests"
    );
    assert!(
        !view
            .disk_watch
            .dirty
            .load(std::sync::atomic::Ordering::Acquire),
        "the noop branch must leave disk_dirty clear outside a runtime"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn config_watch_keys_distinguish_global_from_profile_named_global() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let profile_name = "<global>";
    let _storage = Storage::new_unwatched(profile_name).unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let view = HomeView::new(
        Some(profile_name.to_string()),
        tools,
        crate::file_watch::FileWatchService::new().unwrap(),
    )
    .unwrap();

    assert_eq!(view.config_watch.handles.len(), 2);
    assert!(view
        .config_watch
        .handles
        .contains_key(&ConfigWatchKey::Global));
    assert!(view
        .config_watch
        .handles
        .contains_key(&ConfigWatchKey::profile(profile_name)));
}

#[test]
#[serial]
fn watcher_refresh_does_not_reopen_hotkey_warning_dialog() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(
        &global_config,
        "[tools.alpha]\ncommand = \"alpha\"\nhotkey = \"Ctrl+g\"\n",
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["alpha"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert!(
        view.info_dialog.is_some(),
        "precondition: initial load shows warning dialog"
    );
    view.info_dialog = None;

    view.refresh_from_config(super::ConfigRefreshOrigin::Watcher);
    assert!(
        view.info_dialog.is_none(),
        "watcher-driven refresh must not reopen the hotkey warning dialog"
    );
}

#[test]
#[serial]
fn interactive_refresh_reopens_hotkey_warning_dialog() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(
        &global_config,
        "[tools.alpha]\ncommand = \"alpha\"\nhotkey = \"Ctrl+g\"\n",
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["alpha"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.info_dialog = None;

    view.refresh_from_config(super::ConfigRefreshOrigin::Interactive);
    assert!(
        view.info_dialog.is_some(),
        "interactive refresh must still surface the hotkey warning dialog"
    );
}

#[test]
#[serial]
fn watcher_refresh_stashes_pending_watcher_theme() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(&global_config, "[theme]\nname = \"dracula\"\n").unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert!(
        view.pending_watcher_theme.is_none(),
        "precondition: HomeView::new must not stash a pending watcher theme"
    );

    view.refresh_from_config(super::ConfigRefreshOrigin::Watcher);
    assert_eq!(
        view.pending_watcher_theme.as_deref(),
        Some("dracula"),
        "watcher-driven refresh must stash the resolved theme name so the tick loop can dispatch App::set_theme"
    );
}

#[test]
#[serial]
fn interactive_refresh_does_not_stash_pending_watcher_theme() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(&global_config, "[theme]\nname = \"dracula\"\n").unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert!(
        view.pending_watcher_theme.is_none(),
        "precondition: HomeView::new must not stash a pending watcher theme"
    );

    view.refresh_from_config(super::ConfigRefreshOrigin::Interactive);
    assert!(
        view.pending_watcher_theme.is_none(),
        "interactive refresh must not stash a pending theme; settings/intro input handlers dispatch Action::SetTheme directly"
    );
}

#[test]
#[serial]
fn take_pending_watcher_theme_clears_the_field() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.pending_watcher_theme = Some("zinc".to_string());

    let first = view.take_pending_watcher_theme();
    let second = view.take_pending_watcher_theme();
    assert_eq!(first.as_deref(), Some("zinc"));
    assert!(
        second.is_none(),
        "take must drain the pending field so a single watcher refresh dispatches at most one set_theme"
    );
}

#[test]
#[serial]
fn watcher_refresh_stashes_global_theme_not_profile_override() {
    use crate::session::profile_config::{save_profile_config, ProfileConfig};
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();

    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(&global_config, "[theme]\nname = \"dracula\"\n").unwrap();

    let profile_overrides: ProfileConfig =
        serde_json::from_value(serde_json::json!({"theme": {"name": "empire"}}))
            .expect("legacy hand-edited overrides may carry a theme key even though theme is global by contract");
    save_profile_config("test", &profile_overrides).unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert!(
        view.pending_watcher_theme.is_none(),
        "precondition: HomeView::new must not stash a pending watcher theme"
    );

    view.refresh_from_config(super::ConfigRefreshOrigin::Watcher);
    assert_eq!(
        view.pending_watcher_theme.as_deref(),
        Some("dracula"),
        "watcher path must stash the global theme name via resolve_theme_name; a stale per-profile theme override (legacy or hand-edited) must not mask the global value"
    );
}

#[test]
#[serial]
fn second_watcher_refresh_overwrites_stale_stash() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(&global_config, "[theme]\nname = \"dracula\"\n").unwrap();
    view.refresh_from_config(super::ConfigRefreshOrigin::Watcher);
    assert_eq!(view.pending_watcher_theme.as_deref(), Some("dracula"));

    std::fs::write(&global_config, "[theme]\nname = \"empire\"\n").unwrap();
    view.refresh_from_config(super::ConfigRefreshOrigin::Watcher);
    assert_eq!(
        view.pending_watcher_theme.as_deref(),
        Some("empire"),
        "second watcher refresh must overwrite the stale stash; first-write-wins would silently drop the latest theme change"
    );
}

/// Render the view once into an off-screen backend so geometry-dependent
/// fields (`list_inner_area`, `shelf_inner_area`, scroll offsets) reflect a
/// real layout. Needed by mouse tests that click rows in the pinned Trash /
/// Archived shelf, whose position can't be faked the way `setup_inner` fakes
/// the flat list rect.
fn render_geometry(view: &mut HomeView) {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let theme = load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
}

/// Screen row (0-indexed) of the shelf item at absolute `flat_items` index
/// `idx`, after `render_geometry` has populated `shelf_inner_area`. Assumes the
/// shelf isn't scrolled (true for the small fixtures these tests build).
fn shelf_row_for_idx(view: &HomeView, idx: usize) -> u16 {
    let list_len = view.shelf_start().expect("a shelf must be present");
    assert!(idx >= list_len, "idx {idx} is in the list, not the shelf");
    view.shelf_inner_area.y + (idx - list_len) as u16
}

fn create_test_env_with_sessions(count: usize) -> TestEnv {
    use crate::session::config::GroupByMode;
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut instances = Vec::new();
    for i in 0..count {
        instances.push(Instance::new(
            &format!("session{}", i),
            &format!("/tmp/{}", i),
        ));
    }
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();
    TestEnv {
        view,
        _guard,
        _temp: temp,
    }
}

/// Disable trash-first delete for tests that assert the permanent-delete
/// dialog opens on `d` / `Shift+D` / context-menu Delete. With the default
/// (`delete_to_trash = true`) those keys move the session to the trash
/// instead of opening the dialog; the trash-first path has its own coverage
/// (`trash_then_restore_round_trip`). Must run after `setup_test_home` so it
/// writes into the test HOME. See #2489.
fn disable_delete_to_trash() {
    crate::session::config::update_config(|config| {
        config.session.delete_to_trash = false;
    })
    .unwrap();
}

/// Turn on `session.confirm_delete` so `d` guards the trash with a
/// confirmation dialog instead of trashing on the keystroke. Must run after
/// `setup_test_home` so it writes into the test HOME. See #2583.
fn enable_confirm_delete() {
    crate::session::config::update_config(|config| {
        config.session.confirm_delete = true;
    })
    .unwrap();
}

fn create_test_env_with_groups() -> TestEnv {
    use crate::session::config::GroupByMode;
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut instances = Vec::new();

    let inst1 = Instance::new("ungrouped", "/tmp/u");
    instances.push(inst1);

    let mut inst2 = Instance::new("work-project", "/tmp/work");
    inst2.group_path = "work".to_string();
    instances.push(inst2);

    let mut inst3 = Instance::new("personal-project", "/tmp/personal");
    inst3.group_path = "personal".to_string();
    instances.push(inst3);

    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();
    TestEnv {
        view,
        _guard,
        _temp: temp,
    }
}

fn create_test_env_with_mixed_sessions() -> TestEnv {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut instances = Vec::new();

    let inst_ungrouped = Instance::new("Uncategorized", "/tmp/u");
    instances.push(inst_ungrouped);

    let mut inst1 = Instance::new("Zebra", "/tmp/z");
    inst1.group_path = "work".to_string();
    instances.push(inst1);

    let mut inst2 = Instance::new("Mango", "/tmp/m");
    inst2.group_path = "work".to_string();
    instances.push(inst2);

    let mut inst3 = Instance::new("Apple", "/tmp/a");
    inst3.group_path = "work".to_string();
    instances.push(inst3);

    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();
    TestEnv {
        view,
        _guard,
        _temp: temp,
    }
}

#[test]
#[serial]
fn test_initial_cursor_position() {
    let env = create_test_env_with_sessions(3);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn preview_info_follows_flag_and_never_auto_shows_in_live() {
    // Info-header visibility is purely the persisted `show_preview_info` toggle
    // (driven by `i` in the TUI). Live mode must NOT change it: if the user
    // hid the header, it stays hidden when they go live, and a shown header
    // stays shown. Nothing magically re-shows it.
    use super::live_send::{LiveSendState, LiveSendTarget};
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.select_session_by_id(&id);
    env.view.view_mode = ViewMode::Structured;
    let theme = load_theme("empire");

    let render_to_string = |view: &mut HomeView| {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    let live_state = || LiveSendState {
        session_id: id.clone(),
        title: "session0".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    };

    // Hidden via the toggle: gone outside live...
    env.view.show_preview_info = false;
    let hidden_not_live = render_to_string(&mut env.view);
    assert!(
        !hidden_not_live.contains("Profile:"),
        "header must be hidden when the flag is off.\n{hidden_not_live}"
    );
    // ...and STILL gone after going live (the regression the user reported:
    // it must never magically re-show).
    env.view.live_send = Some(live_state());
    let hidden_live = render_to_string(&mut env.view);
    assert!(
        !hidden_live.contains("Profile:"),
        "a hidden header must not re-appear in live mode.\n{hidden_live}"
    );

    // Shown via the toggle: present both outside and inside live mode.
    env.view.live_send = None;
    env.view.show_preview_info = true;
    let shown_not_live = render_to_string(&mut env.view);
    assert!(
        shown_not_live.contains("Profile:"),
        "header must render when the flag is on.\n{shown_not_live}"
    );
    env.view.live_send = Some(live_state());
    let shown_live = render_to_string(&mut env.view);
    assert!(
        shown_live.contains("Profile:"),
        "a shown header stays shown in live mode (flag, not mode, governs it).\n{shown_live}"
    );
}

/// The app-level global keybindings defer Ctrl+C to live-send via this
/// predicate (#2894). It is true only while live mode owns the keyboard: an
/// overlay opened on top of live mode takes focus back, and Ctrl+C should
/// then behave normally.
#[test]
#[serial]
fn is_live_send_capturing_tracks_state_and_overlays() {
    use super::live_send::{LiveSendState, LiveSendTarget};

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();

    assert!(
        !env.view.is_live_send_capturing(),
        "no live-send means the keyboard is not captured"
    );

    env.view.live_send = Some(LiveSendState {
        session_id: id.clone(),
        title: "session0".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });
    assert!(
        env.view.is_live_send_capturing(),
        "live-send with no overlay captures the keyboard"
    );

    // An overlay over live mode hands focus back to the overlay, so Ctrl+C
    // must stop being routed to the agent.
    env.view.info_dialog = Some(InfoDialog::new("t", "b"));
    assert!(
        !env.view.is_live_send_capturing(),
        "an overlay over live-send releases the capture"
    );
}

/// #2894: in live mode Ctrl+C is forwarded to the agent (an interrupt), not
/// treated as a quit, and each forward arms the footer reminder. Drives the
/// real `handle_key` routing with the default `C-q` exit chord present so the
/// test proves Ctrl+C is distinct from exiting.
#[test]
#[serial]
fn ctrl_c_in_live_mode_forwards_to_agent_and_flashes() {
    use super::live_send::{parse_chord_list, LiveSendState, LiveSendTarget};

    let mut env = create_test_env_with_sessions(1);
    let inst = env.view.instance_at(0).clone();

    // Match the generated tmux name so the drift guard doesn't tear live mode
    // down before the key is translated.
    let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    env.view.live_send = Some(LiveSendState {
        session_id: inst.id.clone(),
        title: inst.title.clone(),
        tmux_name,
        target: LiveSendTarget::Agent,
        exit_chords: parse_chord_list("C-q"),
        leader: None,
    });

    assert!(!env.view.live_send_ctrl_c_flash_active());

    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let action = env.view.handle_key(ctrl_c, None);

    assert!(
        action.is_none(),
        "Ctrl+C in live mode produces no home-view action"
    );
    assert!(
        env.view.live_send.is_some(),
        "Ctrl+C reaches the agent; it must not exit live mode"
    );
    assert!(
        env.view.live_send_ctrl_c_flash_active(),
        "forwarding Ctrl+C arms the footer reminder"
    );
}

/// The live-send footer renders the "Ctrl+C sent to agent" reminder only
/// while the flash window is open (#2894).
#[test]
#[serial]
fn ctrl_c_flash_renders_in_live_footer() {
    use super::live_send::{parse_chord_list, LiveSendState, LiveSendTarget};
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.select_session_by_id(&id);
    env.view.view_mode = ViewMode::Structured;
    let theme = load_theme("empire");

    let render_to_string = |view: &mut HomeView| -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    env.view.live_send = Some(LiveSendState {
        session_id: id.clone(),
        title: "session0".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: parse_chord_list("C-q"),
        leader: None,
    });

    let without = render_to_string(&mut env.view);
    assert!(
        !without.contains("Ctrl+C sent to agent"),
        "the reminder must be absent before any Ctrl+C\n{without}"
    );

    env.view.flash_ctrl_c_hint();
    let with = render_to_string(&mut env.view);
    assert!(
        with.contains("Ctrl+C sent to agent"),
        "the reminder must render while the flash window is open\n{with}"
    );
}

#[test]
#[serial]
fn preview_visible_rows_equal_output_area_with_info_shown() {
    // With the info header shown, the Agent branch sizes the pane to
    // `PreviewLayout::compute(..).output` (header + banner removed once) and the
    // renderer paints into the same rect. `preview_visible_rows` must equal
    // `preview_pane_area.height`; the historical bugs all came from a second,
    // drifting derivation of this number, now consolidated into one layout.
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.select_session_by_id(&id);
    env.view.view_mode = ViewMode::Structured;
    env.view.show_preview_info = true;

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();

    assert!(
        env.view.preview_pane_area.height > 0,
        "expected a non-empty output sub-rect at 120x40 (non-compact)"
    );
    assert_eq!(
        env.view.preview_visible_rows, env.view.preview_pane_area.height as usize,
        "visible rows must match the output area height, not be a row short"
    );
}

/// Precedence: unread paints only on resting (Idle/Unknown) rows. A live
/// status supersedes it, keeping its own spinner — so a Running session that
/// also carries an unread marker must NOT show the solid unread dot. See the
/// #2088 review note about jumbled precedence.
#[test]
#[serial]
fn unread_dot_yields_to_a_running_status() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let theme = load_theme("empire");

    let render = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| env.view.render(f, f.area(), &theme, None, None, None))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        out
    };

    // Idle + unread: the row shows the solid unread dot.
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
        inst.mark_unread();
    });
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        render(&mut env).contains('●'),
        "an idle unread row should paint the unread dot"
    );

    // Running + still unread: the live status wins; no unread dot.
    env.view
        .mutate_instance(&id, |inst| inst.status = crate::session::Status::Running);
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        !render(&mut env).contains('●'),
        "a running row must keep its spinner, not the unread dot"
    );
}

/// Sunk rows never paint the unread dot. Archiving or snoozing an unread
/// row dismisses it; surfacing it as unread contradicts that. The snooze
/// case must hold in every sort mode, not just Attention (#2571).
#[test]
#[serial]
fn unread_dot_suppressed_on_archived_and_snoozed() {
    use crate::session::config::SortOrder;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let theme = load_theme("empire");

    let render = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| env.view.render(f, f.area(), &theme, None, None, None))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        out
    };

    // Baseline: an idle unread row paints the dot.
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
        inst.mark_unread();
    });
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        render(&mut env).contains('●'),
        "an idle unread row should paint the unread dot"
    );

    // Snoozed, in a non-Attention sort: the dot must be gone even though the
    // snooze decoration itself is Attention-only.
    env.view.sort_order = SortOrder::Newest;
    env.view.mutate_instance(&id, |inst| inst.snooze(30));
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        !render(&mut env).contains('●'),
        "a snoozed unread row must not paint the unread dot outside Attention sort"
    );

    // Snoozed in Attention sort: still no dot.
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        !render(&mut env).contains('●'),
        "a snoozed unread row must not paint the unread dot in Attention sort"
    );

    // Archived: the archive override already mutes the glyph; guard it stays muted.
    env.view.mutate_instance(&id, |inst| {
        inst.unsnooze();
        inst.archive();
    });
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        !render(&mut env).contains('●'),
        "an archived unread row must not paint the unread dot"
    );
}

/// Unread is an Agent-view concept: the dot marks agent output the user hasn't
/// seen. The paired terminal has no such notion, so Terminal view must never
/// paint the unread dot even when the underlying instance carries the flag.
#[test]
#[serial]
fn unread_dot_never_paints_in_terminal_view() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let theme = load_theme("empire");

    let render = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| env.view.render(f, f.area(), &theme, None, None, None))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        out
    };

    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
        inst.mark_unread();
    });
    env.view.flat_items = env.view.build_flat_items();

    // Agent view: the idle unread row paints the dot (baseline sanity).
    env.view.view_mode = ViewMode::Structured;
    assert!(
        render(&mut env).contains('●'),
        "agent view should paint the unread dot for an idle unread row"
    );

    // Terminal view: same instance, no dot. The terminal pane isn't running
    // (no tmux session in the test env), so the row shows its idle glyph.
    env.view.view_mode = ViewMode::Terminal;
    assert!(
        !render(&mut env).contains('●'),
        "terminal view must not paint the unread dot"
    );
}

/// Stop targets what the user is looking at: Agent view arms the
/// `stop_session` confirm (agent + container stop), while Terminal and Tool
/// views route to their respective pane-kill paths and never arm an agent
/// stop. The routing decision lives in `stop_selected`.
#[test]
#[serial]
fn stop_in_terminal_view_does_not_target_agent_session() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view
        .mutate_instance(&id, |inst| inst.status = crate::session::Status::Idle);
    env.view.selected_session = Some(id.clone());

    // Agent view: Stop arms the agent-session stop and opens its confirm.
    env.view.view_mode = ViewMode::Structured;
    env.view.stop_selected();
    assert_eq!(
        env.view.pending_stop_session.as_deref(),
        Some(id.as_str()),
        "agent view stop must arm the agent-session stop"
    );
    assert_eq!(
        env.view.confirm_dialog.as_ref().map(|d| d.action()),
        Some("stop_session"),
        "agent view stop must open the stop-session confirm"
    );

    env.view.confirm_dialog = None;
    env.view.pending_stop_session = None;

    // Terminal view: Stop must never touch the agent session. With no live
    // terminal in the test env the terminal-kill path no-ops, but the critical
    // invariant is that no agent stop was armed and the stop-session confirm
    // never opened.
    env.view.view_mode = ViewMode::Terminal;
    env.view.stop_selected();
    assert!(
        env.view.pending_stop_session.is_none(),
        "terminal view stop must not arm an agent-session stop"
    );
    assert_ne!(
        env.view.confirm_dialog.as_ref().map(|d| d.action()),
        Some("stop_session"),
        "terminal view stop must not open the stop-session confirm"
    );

    // Tool view: same invariant, Stop routes to the tool-kill path and never
    // arms an agent stop.
    env.view.view_mode = ViewMode::Tool("lazygit".to_string());
    env.view.stop_selected();
    assert!(
        env.view.pending_stop_session.is_none(),
        "tool view stop must not arm an agent-session stop"
    );
    assert_ne!(
        env.view.confirm_dialog.as_ref().map(|d| d.action()),
        Some("stop_session"),
        "tool view stop must not open the stop-session confirm"
    );
}

/// Render suppression is cosmetic: archive/snooze leave the `unread` flag on
/// disk so unarchiving or unsnoozing brings the marker back (#2571).
#[test]
fn unread_flag_survives_sink_round_trip() {
    let mut inst = crate::session::Instance::new("rt", "/tmp/rt");
    inst.mark_unread();

    inst.archive();
    assert!(inst.is_unread(), "archive must not clear unread");
    inst.unarchive();
    assert!(inst.is_unread(), "unarchive must keep unread");

    inst.snooze(30);
    assert!(inst.is_unread(), "snooze must not clear unread");
    inst.unsnooze();
    assert!(inst.is_unread(), "unsnooze must keep unread");
}

/// Dwell-to-read: an unread row that stays selected past `UNREAD_DWELL`
/// (with the list in the foreground) is cleared, distinguishing "stopped to
/// read it" from "scrolled past."
#[test]
#[serial]
fn unread_dwell_clears_after_threshold() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
        inst.mark_unread();
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.select_session_by_id(&id);
    assert!(env.view.get_instance(&id).unwrap().is_unread());

    let t0 = Instant::now();
    // First tick arms the dwell clock; nothing cleared yet.
    assert!(!env.view.tick_unread_dwell(t0));
    assert!(env.view.get_instance(&id).unwrap().is_unread());
    // Below the threshold: still unread (this is the "scrolled past" guard).
    assert!(!env.view.tick_unread_dwell(t0 + Duration::from_millis(500)));
    assert!(env.view.get_instance(&id).unwrap().is_unread());
    // Past the threshold: cleared.
    assert!(env
        .view
        .tick_unread_dwell(t0 + super::UNREAD_DWELL + Duration::from_millis(1)));
    assert!(!env.view.get_instance(&id).unwrap().is_unread());
}

/// A fresh manual flag (`u`) is held for the current visit: marking a session
/// unread and keeping the cursor on it must not let dwell-to-read undo the
/// mark. Regression for the bug where staying on the row past `UNREAD_DWELL`
/// silently re-cleared it.
#[test]
#[serial]
fn manual_unread_survives_same_visit_dwell() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.select_session_by_id(&id);
    env.view
        .toggle_unread_at_cursor()
        .expect("manual toggle should succeed");
    assert!(env.view.get_instance(&id).unwrap().is_unread());

    let t0 = Instant::now();
    // Arm the clock, then sit well past the threshold without moving: the hold
    // keeps the mark.
    assert!(!env.view.tick_unread_dwell(t0));
    assert!(!env
        .view
        .tick_unread_dwell(t0 + super::UNREAD_DWELL + Duration::from_secs(5)));
    assert!(
        env.view.get_instance(&id).unwrap().is_unread(),
        "a freshly hand-flagged row must survive dwell while it stays selected"
    );
}

/// The manual hold is per-visit: after leaving a hand-flagged row and coming
/// back, dwelling on it clears the mark like any other unread row. This is the
/// behavior verified by hand (select A, mark unread, move to B, move back, sit
/// past the threshold -> it clears).
#[test]
#[serial]
fn manual_unread_clears_after_leave_and_return() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(2);
    let a = env.view.instance_at(0).id.clone();
    let b = env.view.instance_at(1).id.clone();
    for id in [&a, &b] {
        env.view.mutate_instance(id, |inst| {
            inst.status = crate::session::Status::Idle;
        });
    }
    env.view.flat_items = env.view.build_flat_items();

    // Select A and flag it unread by hand.
    env.view.select_session_by_id(&a);
    env.view.toggle_unread_at_cursor().expect("manual mark A");
    assert!(env.view.get_instance(&a).unwrap().is_unread());

    // Move to B with NO dwell tick in between (a quick hop, like real
    // navigation). The hold must release purely from the selection change;
    // otherwise returning to A would stay suppressed forever (the reported
    // bug, which an in-between tick would have masked).
    env.view.select_session_by_id(&b);
    assert!(
        env.view.manual_unread_hold.is_none(),
        "moving off the row must release the hold without needing a dwell tick"
    );

    // Come back to A: arm the clock, then sit past the threshold. Now it clears.
    let t0 = Instant::now();
    env.view.select_session_by_id(&a);
    assert!(!env.view.tick_unread_dwell(t0));
    let cleared = env
        .view
        .tick_unread_dwell(t0 + super::UNREAD_DWELL + Duration::from_secs(1));
    assert!(cleared, "revisiting and dwelling should clear the mark");
    assert!(
        !env.view.get_instance(&a).unwrap().is_unread(),
        "a hand-flagged row clears on revisit + dwell (per-visit hold)"
    );
}

/// Engaging with a hand-flagged row (open/attach, which clears it) also drops
/// the per-visit hold, so a *later* auto mark on that same still-selected row
/// is not wrongly suppressed and clears on dwell.
#[test]
#[serial]
fn manual_hold_released_on_engagement_lets_auto_clear() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.select_session_by_id(&id);

    // Hand-flag, then engage (the open/attach path), which clears it and ends
    // the hold even though the cursor never left the row.
    env.view.toggle_unread_at_cursor().expect("manual mark");
    env.view.clear_unread_on_view(&id);
    assert!(
        env.view.manual_unread_hold.is_none(),
        "engaging with the row must release the manual hold"
    );
    assert!(!env.view.get_instance(&id).unwrap().is_unread());

    // A later auto mark on the same (still-selected) row must clear on dwell.
    env.view.mutate_instance(&id, |inst| inst.mark_unread());
    let t0 = Instant::now();
    assert!(!env.view.tick_unread_dwell(t0));
    let cleared = env
        .view
        .tick_unread_dwell(t0 + super::UNREAD_DWELL + Duration::from_secs(1));
    assert!(
        cleared && !env.view.get_instance(&id).unwrap().is_unread(),
        "a later auto mark must not be suppressed by a stale hold"
    );
}

/// Moving the selection to a different row before the dwell completes spares
/// the first row: arrowing through a list doesn't read everything you pass.
#[test]
#[serial]
fn unread_dwell_resets_on_selection_change() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(2);
    let a = env.view.instance_at(0).id.clone();
    let b = env.view.instance_at(1).id.clone();
    for id in [&a, &b] {
        env.view.mutate_instance(id, |inst| {
            inst.status = crate::session::Status::Idle;
            inst.mark_unread();
        });
    }
    env.view.flat_items = env.view.build_flat_items();

    let t0 = Instant::now();
    // Arm the dwell clock on A.
    env.view.select_session_by_id(&a);
    assert!(!env.view.tick_unread_dwell(t0));
    // Move to B well before A's threshold; A's clock is dropped, B's arms.
    env.view.select_session_by_id(&b);
    assert!(!env.view.tick_unread_dwell(t0 + Duration::from_millis(500)));
    // Long after, B has now dwelled past the threshold and clears; A, which we
    // left early, is untouched.
    assert!(env
        .view
        .tick_unread_dwell(t0 + super::UNREAD_DWELL + Duration::from_secs(2)));
    assert!(
        env.view.get_instance(&a).unwrap().is_unread(),
        "row left before the threshold must stay unread"
    );
    assert!(
        !env.view.get_instance(&b).unwrap().is_unread(),
        "row dwelled past the threshold must be cleared"
    );
}

#[test]
#[serial]
fn test_q_returns_quit_action() {
    let mut env = create_test_env_empty();
    let action = env.view.handle_key(key(KeyCode::Char('q')), None);
    assert_eq!(action, Some(Action::Quit));
}

#[test]
#[serial]
fn test_ctrl_q_does_not_quit_home() {
    // #1569: Ctrl+Q is a live-mode-exit habit; on the home view it must
    // not quit aoe. (The app-level handler swallows it; the home view
    // itself must also never treat it as a quit.)
    let mut env = create_test_env_empty();
    let action = env.view.handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        None,
    );
    assert_eq!(action, None);
}

#[test]
#[serial]
fn test_quit_confirm_dont_ask_again_persists_opt_out() {
    let mut env = create_test_env_empty();
    env.view.confirm_before_quit = true;

    env.view.show_quit_confirm();
    assert!(env.view.confirm_dialog.is_some());

    // Tick "don't warn me again", then confirm.
    env.view.handle_key(key(KeyCode::Char(' ')), None);
    let action = env.view.handle_key(key(KeyCode::Char('y')), None);

    assert_eq!(action, Some(Action::Quit));
    assert!(!env.view.confirm_before_quit);
    // The opt-out is persisted so it survives a restart.
    let saved = crate::session::config::load_config()
        .unwrap()
        .expect("config should have been written");
    assert!(!saved.session.confirm_before_quit);
}

#[test]
#[serial]
fn test_quit_confirm_without_opt_out_keeps_flag() {
    let mut env = create_test_env_empty();
    env.view.confirm_before_quit = true;

    env.view.show_quit_confirm();
    // Confirm without ticking the checkbox.
    let action = env.view.handle_key(key(KeyCode::Char('y')), None);

    assert_eq!(action, Some(Action::Quit));
    assert!(env.view.confirm_before_quit);
}

#[test]
#[serial]
fn test_question_mark_opens_help() {
    let mut env = create_test_env_empty();
    assert!(!env.view.show_help);
    env.view.handle_key(key(KeyCode::Char('?')), None);
    assert!(env.view.show_help);
}

#[test]
#[serial]
fn test_help_closes_on_esc() {
    let mut env = create_test_env_empty();
    env.view.show_help = true;
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(!env.view.show_help);
}

#[test]
#[serial]
fn test_help_closes_on_question_mark() {
    let mut env = create_test_env_empty();
    env.view.show_help = true;
    env.view.handle_key(key(KeyCode::Char('?')), None);
    assert!(!env.view.show_help);
}

#[test]
#[serial]
fn test_help_closes_on_q() {
    let mut env = create_test_env_empty();
    env.view.show_help = true;
    env.view.handle_key(key(KeyCode::Char('q')), None);
    assert!(!env.view.show_help);
}

#[test]
#[serial]
fn test_help_closes_on_uppercase_q_for_strict_mode() {
    // Strict mode binds quit to uppercase Q; the help overlay must
    // accept it too so strict-mode users can dismiss the dialog with
    // the same key they use to quit.
    let mut env = create_test_env_empty();
    env.view.show_help = true;
    env.view.handle_key(key(KeyCode::Char('Q')), None);
    assert!(!env.view.show_help);
}

#[test]
#[serial]
fn test_has_dialog_returns_true_for_help() {
    let mut env = create_test_env_empty();
    assert!(!env.view.has_dialog());
    env.view.show_help = true;
    assert!(env.view.has_dialog());
}

#[test]
#[serial]
fn test_n_opens_new_dialog() {
    let mut env = create_test_env_empty();
    assert!(env.view.new_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert!(env.view.new_dialog.is_some());
}

#[test]
#[serial]
fn test_has_dialog_returns_true_for_new_dialog() {
    let mut env = create_test_env_empty();
    env.view.new_dialog = Some(NewSessionDialog::new(
        AvailableTools::with_tools(&["claude"]),
        Vec::new(),
        "default",
        vec!["default".to_string()],
    ));
    assert!(env.view.has_dialog());
}

#[test]
#[serial]
fn test_b_opens_project_session_picker_when_projects_exist() {
    use crate::session::projects::{self, Project, ProjectScope};
    let mut env = create_test_env_empty();
    let repo = env._temp.path().join("repoA");
    std::fs::create_dir_all(&repo).unwrap();
    projects::add(
        "test",
        ProjectScope::Profile,
        Project::new("repoA", repo.to_string_lossy(), ProjectScope::Profile),
        false,
    )
    .unwrap();

    assert!(env.view.project_session_picker_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(env.view.project_session_picker_dialog.is_some());
    assert!(env.view.info_dialog.is_none());
    // The picker captures filter chars, so it must register as a modal: an
    // unregistered picker lets the global `q` shortcut quit the app and the
    // paste-burst detector fire mid-filter (text gets stranded in handle_paste).
    assert!(env.view.has_dialog());
    assert!(!env.view.wants_paste_burst());
}

#[test]
#[serial]
fn test_b_opens_project_add_flow_when_no_projects() {
    let mut env = create_test_env_empty();
    let project_dir = env._temp.path().join("plain-project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let profile = env.view.config_profile();

    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(env.view.project_session_picker_dialog.is_none());
    assert!(env.view.info_dialog.is_none());
    assert!(env.view.projects_dialog.is_some());

    for ch in project_dir.to_string_lossy().chars() {
        env.view.handle_key(key(KeyCode::Char(ch)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);

    let canonical = project_dir
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let projects = crate::session::projects::load_merged(&profile).unwrap();
    assert!(
        projects.iter().any(|p| p.path == canonical),
        "b empty-state add flow should register the typed path"
    );
}

#[test]
#[serial]
fn test_b_empty_project_add_flow_escape_closes_dialog() {
    let mut env = create_test_env_empty();

    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(env.view.projects_dialog.is_some());

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.projects_dialog.is_none());
    assert!(env.view.info_dialog.is_none());
}

#[test]
#[serial]
fn test_b_submit_opens_new_dialog_with_prefilled_path() {
    use crate::session::projects::{self, Project, ProjectScope};
    let mut env = create_test_env_empty();
    let repo = env._temp.path().join("repoB");
    std::fs::create_dir_all(&repo).unwrap();
    projects::add(
        "test",
        ProjectScope::Profile,
        Project::new("repoB", repo.to_string_lossy(), ProjectScope::Profile),
        false,
    )
    .unwrap();
    let expected = projects::load_merged("test").unwrap()[0].path.clone();

    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(env.view.project_session_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.project_session_picker_dialog.is_none());
    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("new session dialog should open after picking a project");
    assert_eq!(dialog.path_value(), expected);
}

#[test]
#[serial]
fn test_cursor_down_j() {
    let mut env = create_test_env_with_sessions(5);
    assert_eq!(env.view.cursor, 0);
    env.view.handle_key(key(KeyCode::Char('j')), None);
    assert_eq!(env.view.cursor, 1);
}

#[test]
#[serial]
fn test_cursor_down_arrow() {
    let mut env = create_test_env_with_sessions(5);
    assert_eq!(env.view.cursor, 0);
    env.view.handle_key(key(KeyCode::Down), None);
    assert_eq!(env.view.cursor, 1);
}

#[test]
#[serial]
fn test_cursor_up_k() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::Char('k')), None);
    assert_eq!(env.view.cursor, 2);
}

#[test]
#[serial]
fn test_cursor_up_arrow() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::Up), None);
    assert_eq!(env.view.cursor, 2);
}

#[test]
#[serial]
fn test_cursor_bounds_at_top() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 0;
    env.view.handle_key(key(KeyCode::Up), None);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_cursor_bounds_at_bottom() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 4;
    env.view.handle_key(key(KeyCode::Down), None);
    assert_eq!(env.view.cursor, 4);
}

#[test]
#[serial]
fn test_page_down() {
    let mut env = create_test_env_with_sessions(20);
    env.view.cursor = 0;
    env.view.handle_key(key(KeyCode::PageDown), None);
    assert_eq!(env.view.cursor, 10);
}

#[test]
#[serial]
fn test_page_up() {
    let mut env = create_test_env_with_sessions(20);
    env.view.cursor = 15;
    env.view.handle_key(key(KeyCode::PageUp), None);
    assert_eq!(env.view.cursor, 5);
}

#[test]
#[serial]
fn test_page_down_clamps_to_end() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 0;
    env.view.handle_key(key(KeyCode::PageDown), None);
    assert_eq!(env.view.cursor, 4);
}

#[test]
#[serial]
fn test_page_up_clamps_to_start() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::PageUp), None);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_home_key() {
    let mut env = create_test_env_with_sessions(10);
    env.view.cursor = 7;
    env.view.handle_key(key(KeyCode::Home), None);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_end_key() {
    let mut env = create_test_env_with_sessions(10);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::End), None);
    assert_eq!(env.view.cursor, 9);
}

#[test]
#[serial]
fn test_g_key_opens_group_picker() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_with_sessions(3);
    env.view.group_by = GroupByMode::Manual;

    // 'g' opens the picker without changing the current mode.
    env.view.handle_key(key(KeyCode::Char('g')), None);
    assert!(env.view.group_picker_dialog.is_some());
    assert_eq!(env.view.group_by, GroupByMode::Manual);

    // Down + Enter selects the next option (Project).
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.group_picker_dialog.is_none());
    assert_eq!(env.view.group_by, GroupByMode::Project);

    // 'g' again, Esc cancels without changing mode.
    env.view.handle_key(key(KeyCode::Char('g')), None);
    assert!(env.view.group_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.group_picker_dialog.is_none());
    assert_eq!(env.view.group_by, GroupByMode::Project);
}

#[test]
#[serial]
fn test_uppercase_g_goes_to_end() {
    let mut env = create_test_env_with_sessions(10);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::Char('G')), None);
    assert_eq!(env.view.cursor, 9);
}

#[test]
#[serial]
fn test_cursor_movement_on_empty_list() {
    let mut env = create_test_env_empty();
    env.view.handle_key(key(KeyCode::Down), None);
    assert_eq!(env.view.cursor, 0);
    env.view.handle_key(key(KeyCode::Up), None);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_enter_on_session_returns_attach_action() {
    let mut env = create_test_env_with_sessions(3);
    env.view.cursor = 1;
    env.view.update_selected();
    let action = env.view.handle_key(key(KeyCode::Enter), None);
    assert!(matches!(action, Some(Action::AttachSession(_))));
}

#[cfg(feature = "serve")]
#[test]
#[serial]
fn test_enter_on_acp_session_opens_structured_view() {
    use crate::session::config::GroupByMode;
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut instances = vec![
        Instance::new("plain", "/tmp/0"),
        Instance::new("acp", "/tmp/1"),
        Instance::new("plain2", "/tmp/2"),
    ];
    instances[1].view = crate::session::View::Structured;
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.cursor = 1;
    view.update_selected();

    let action = view.handle_key(key(KeyCode::Enter), None);
    match action {
        Some(Action::OpenStructuredView(id)) => {
            // Should target the structured view instance, not the plain ones.
            assert!(
                id.contains("acp") || !id.is_empty(),
                "OpenStructuredView carried an empty session id"
            );
        }
        other => {
            panic!("expected Action::OpenStructuredView for structured view session, got {other:?}")
        }
    }
}

#[test]
#[serial]
fn test_slash_enters_search_mode() {
    let mut env = create_test_env_with_sessions(3);
    assert!(!env.view.search_active);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    assert!(env.view.search_active);
    assert!(env.view.search_query.value().is_empty());
}

#[test]
#[serial]
fn test_search_mode_captures_chars() {
    let mut env = create_test_env_with_sessions(3);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('t')), None);
    env.view.handle_key(key(KeyCode::Char('e')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(env.view.search_query.value(), "test");
}

#[test]
#[serial]
fn test_search_mode_backspace() {
    let mut env = create_test_env_with_sessions(3);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('a')), None);
    env.view.handle_key(key(KeyCode::Char('b')), None);
    env.view.handle_key(key(KeyCode::Backspace), None);
    assert_eq!(env.view.search_query.value(), "a");
}

#[test]
#[serial]
fn test_search_mode_esc_exits_and_clears() {
    let mut env = create_test_env_with_sessions(3);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('x')), None);
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(!env.view.search_active);
    assert!(env.view.search_query.value().is_empty());
    assert!(env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_mode_enter_commits_without_clearing_matches() {
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('e')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    assert!(env.view.search_active);
    let matches_before = env.view.search_matches.len();
    assert!(
        matches_before > 1,
        "test needs multiple matches to be meaningful"
    );

    env.view.handle_key(key(KeyCode::Enter), None);

    assert!(!env.view.search_active);
    assert_eq!(
        env.view.search_query.value(),
        "sess",
        "Enter must keep search_query so reloads re-score instead of wiping matches"
    );
    assert_eq!(
        env.view.search_matches.len(),
        matches_before,
        "Enter must not clear matches"
    );
    assert_eq!(env.view.search_match_index, 0);
}

#[test]
#[serial]
fn test_reload_after_enter_preserves_search_state() {
    // Regression guard for #2676: `refresh_search_matches` wipes matches
    // whenever the query is empty. If Enter cleared search_query, the very
    // next storage/config reload would destroy the matches Enter promised
    // to keep, silently breaking n/N cycling.
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('e')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    let matches_before = env.view.search_matches.len();
    assert!(
        matches_before >= 3,
        "test needs matches for meaningful assertions"
    );

    env.view.reload().unwrap();

    assert_eq!(
        env.view.search_matches.len(),
        matches_before,
        "reload after Enter must not wipe search_matches"
    );

    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert_eq!(
        env.view.search_match_index, 1,
        "n still cycles after a reload lands between Enter and the first press"
    );
}

#[test]
#[serial]
fn test_sort_order_change_after_enter_rescores_search_matches() {
    // Regression guard for #2676: paths that rebuild `flat_items`
    // (`apply_sort_order`, `apply_group_by`, `toggle_group_collapsed`)
    // must re-score `search_matches` against the new indices, or `n`/`N`
    // jumps to stale positions and row highlights land on wrong sessions.
    // Query "session0" matches only session0. With Newest sort it sits at
    // index 4; with Oldest at index 0. Without the fix the stale index 4
    // would land on session4 after the sort change.
    use crate::session::config::SortOrder;
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    for c in "session0".chars() {
        env.view.handle_key(key(KeyCode::Char(c)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);

    assert_eq!(env.view.search_matches.len(), 1);
    let matched_id_before = match &env.view.flat_items[env.view.search_matches[0]] {
        Item::Session { id, .. } => id.clone(),
        _ => panic!("initial match must be a Session"),
    };

    let new_order = if env.view.sort_order == SortOrder::Newest {
        SortOrder::Oldest
    } else {
        SortOrder::Newest
    };
    env.view.apply_sort_order(new_order);

    assert_eq!(
        env.view.search_matches.len(),
        1,
        "same session still matches after sort"
    );
    let matched_id_after = match &env.view.flat_items[env.view.search_matches[0]] {
        Item::Session { id, .. } => id.clone(),
        _ => panic!("match must still be a Session, not a stale non-Session index"),
    };
    assert_eq!(
        matched_id_after, matched_id_before,
        "sort change must not orphan search_matches to a stale index pointing at the wrong session"
    );
}

#[test]
#[serial]
fn test_search_mode_enter_keeps_matches_for_cycling() {
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('e')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Enter), None);

    let n_matches = env.view.search_matches.len();
    assert!(
        n_matches >= 3,
        "test needs at least 3 matches for wrap coverage"
    );
    assert_eq!(env.view.search_match_index, 0);

    for expected in 1..n_matches {
        env.view.handle_key(key(KeyCode::Char('n')), None);
        assert_eq!(env.view.search_match_index, expected);
    }

    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert_eq!(env.view.search_match_index, 0, "n wraps to first");

    env.view.handle_key(key(KeyCode::Char('N')), None);
    assert_eq!(
        env.view.search_match_index,
        n_matches - 1,
        "N wraps to last"
    );

    env.view.handle_key(key(KeyCode::Char('N')), None);
    assert_eq!(env.view.search_match_index, n_matches - 2);
}

#[test]
#[serial]
fn test_d_on_session_opens_delete_dialog() {
    let mut env = create_test_env_with_sessions(3);
    disable_delete_to_trash();
    env.view.update_selected();
    assert!(env.view.unified_delete_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('d')), None);
    assert!(env.view.unified_delete_dialog.is_some());
}

#[test]
#[serial]
fn test_d_on_group_with_sessions_opens_group_delete_options_dialog() {
    let mut env = create_test_env_with_groups();
    env.view.cursor = 1;
    env.view.update_selected();
    assert!(env.view.selected_group.is_some());
    assert!(env.view.group_delete_options_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('d')), None);
    assert!(env.view.group_delete_options_dialog.is_some());
}

#[test]
#[serial]
fn test_selected_session_updates_on_cursor_move() {
    let mut env = create_test_env_with_sessions(3);
    let first_id = env.view.selected_session.clone();
    env.view.handle_key(key(KeyCode::Down), None);
    assert_ne!(env.view.selected_session, first_id);
}

#[test]
#[serial]
fn test_selected_group_set_when_on_group() {
    let mut env = create_test_env_with_groups();
    for i in 0..env.view.flat_items.len() {
        env.view.cursor = i;
        env.view.update_selected();
        if matches!(env.view.flat_items.get(i), Some(Item::Group { .. })) {
            assert!(env.view.selected_group.is_some());
            assert!(env.view.selected_session.is_none());
            return;
        }
    }
    panic!("No group found in flat_items");
}

#[test]
#[serial]
fn test_search_matches_session_title() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("session2".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());
    // The best match should be session2
    let best_idx = env.view.search_matches[0];
    if let Item::Session { id, .. } = &env.view.flat_items[best_idx] {
        let inst = env.view.get_instance(id).unwrap();
        assert!(inst.title.contains("session2"));
    }
}

#[test]
#[serial]
fn test_search_case_insensitive() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("SESSION2".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_matches_path() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("/tmp/3".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_matches_group_name() {
    let mut env = create_test_env_with_groups();
    env.view.search_query = Input::new("work".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_empty_query_clears_matches() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());

    env.view.search_query = Input::default();
    env.view.update_search();
    assert!(env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_no_matches() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("zzzznonexistent".to_string());
    env.view.update_search();
    assert!(env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_jumps_to_best_match() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 0; // start at beginning
    env.view.search_active = true;
    env.view.search_query = Input::new("session0".to_string());
    env.view.update_search();
    // Cursor should jump to the best match
    // With default sort (Newest), session0 is at index 4 (last)
    assert_eq!(env.view.cursor, 4);
}

#[test]
#[serial]
fn test_search_keeps_full_list() {
    let mut env = create_test_env_with_sessions(5);
    let original_len = env.view.flat_items.len();
    env.view.search_query = Input::new("session2".to_string());
    env.view.update_search();
    // All items should still be in flat_items
    assert_eq!(env.view.flat_items.len(), original_len);
}

#[test]
#[serial]
fn test_search_n_cycles_forward() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();
    let match_count = env.view.search_matches.len();
    assert!(match_count > 1);

    let first_cursor = env.view.cursor;
    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert_eq!(env.view.search_match_index, 1);
    // Cursor should have moved
    assert_ne!(env.view.cursor, first_cursor);
}

#[test]
#[serial]
fn test_search_n_wraps_around() {
    let mut env = create_test_env_with_sessions(3);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();
    let match_count = env.view.search_matches.len();

    // Cycle through all matches to wrap
    for _ in 0..match_count {
        env.view.handle_key(key(KeyCode::Char('n')), None);
    }
    assert_eq!(env.view.search_match_index, 0);
}

#[test]
#[serial]
fn test_search_shift_n_cycles_backward() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();
    let match_count = env.view.search_matches.len();
    assert!(match_count > 1);

    // N from index 0 should wrap to last
    env.view.handle_key(key(KeyCode::Char('N')), None);
    assert_eq!(env.view.search_match_index, match_count - 1);
}

#[test]
#[serial]
fn test_esc_clears_search_matches() {
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    assert!(!env.view.search_matches.is_empty());
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.search_matches.is_empty());
    assert_eq!(env.view.search_match_index, 0);
}

#[test]
#[serial]
fn test_esc_clears_matches_so_n_opens_new_dialog() {
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(!env.view.search_active);
    assert!(env.view.search_matches.is_empty());

    assert!(env.view.new_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert!(env.view.new_dialog.is_some());
}

// The only catalog tip is earned, so it (and the badge) appears only after the
// `new_session_with_selection` counter crosses its threshold. Set that on disk
// and refresh the cached badge so a test starts with the tip eligible.
fn earn_tip(env: &mut TestEnv) {
    crate::session::config::update_app_state(|state| {
        state.new_session_with_selection_count = crate::tips::NEW_FROM_SELECTION_TIP_THRESHOLD;
    })
    .unwrap();
    let config = crate::session::config::load_config()
        .unwrap()
        .unwrap_or_default();
    env.view.tips_unseen = crate::tui::home::tips_unseen_count(&config);
}

#[test]
#[serial]
fn open_tips_dialog_opens_even_with_no_eligible_tips() {
    // No tip earned yet: "Show tips" still opens the overlay (an empty state)
    // rather than silently doing nothing.
    let mut env = create_test_env_empty();
    assert!(env.view.tips_dialog.is_none());
    env.view.open_tips_dialog();
    assert!(env.view.tips_dialog.is_some());
}

#[test]
#[serial]
fn persist_tips_outcome_merges_seen_sets_disabled_and_updates_badge() {
    use crate::tui::dialogs::TipsOutcome;

    let mut env = create_test_env_empty();
    earn_tip(&mut env);
    let before = env.view.tips_unseen;
    assert!(before > 0);

    env.view.persist_tips_outcome(TipsOutcome {
        newly_seen: vec!["new-from-selection".to_string()],
        disabled: Some(true),
    });

    let config = crate::session::config::load_config()
        .unwrap()
        .unwrap_or_default();
    assert!(config
        .app_state
        .tips_seen
        .iter()
        .any(|s| s == "new-from-selection"));
    assert!(!config.session.show_tips);
    // Disabling tips zeroes the cached badge count.
    assert_eq!(env.view.tips_unseen, 0);
}

#[test]
#[serial]
fn tips_badge_renders_with_count_and_hides_when_zero() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let theme = load_theme("empire");

    let render = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(200, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    // Earn the tip so the badge shows a count.
    earn_tip(&mut env);
    let n = env.view.tips_unseen;
    assert!(n > 0);
    let shown = render(&mut env);
    assert!(
        shown.contains(&format!("{n} tips")),
        "badge should show the unseen count\n{shown}"
    );

    // Zero unseen (or disabled) hides the badge entirely.
    env.view.tips_unseen = 0;
    let hidden = render(&mut env);
    assert!(
        !hidden.contains("tips"),
        "no badge when nothing is unseen\n{hidden}"
    );
}

#[test]
#[serial]
fn footer_hints_yield_to_tips_badge_when_thin() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let theme = load_theme("empire");
    earn_tip(&mut env);
    let n = env.view.tips_unseen;
    assert!(n > 0);
    let badge = format!("{n} tips");

    let render_at = |env: &mut TestEnv, w: u16| -> String {
        let backend = TestBackend::new(w, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    // Wide: the badge and even a low-priority hint (Diff) both fit.
    let wide = render_at(&mut env, 200);
    assert!(wide.contains(&badge), "badge shows when wide\n{wide}");
    assert!(
        wide.contains("Diff"),
        "low-priority hint present when wide\n{wide}"
    );

    // Thin: the badge still shows (it takes priority); the hints yield.
    let thin = render_at(&mut env, 30);
    assert!(
        thin.contains(&badge),
        "badge survives on a thin footer\n{thin}"
    );
    assert!(
        !thin.contains("Diff"),
        "low-priority hints drop to make room for the badge\n{thin}"
    );
}

#[test]
#[serial]
fn clicking_footer_tips_badge_opens_overlay() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let theme = load_theme("empire");
    earn_tip(&mut env);
    assert!(env.view.tips_unseen > 0);

    // Render once so the footer captures the badge's clickable rect.
    let backend = TestBackend::new(200, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let rect = env
        .view
        .tips_badge_rect
        .expect("badge rect should be captured when shown");

    assert!(env.view.tips_dialog.is_none());
    let handled = env.view.handle_tips_badge_click(rect.x, rect.y);
    assert!(handled, "click on the badge is handled");
    assert!(
        env.view.tips_dialog.is_some(),
        "clicking the badge opens the tips overlay"
    );
}

#[test]
#[serial]
fn hovering_footer_tips_badge_sets_hover_state() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let theme = load_theme("empire");
    earn_tip(&mut env);

    // Render once so the badge's rect is captured.
    let backend = TestBackend::new(200, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let rect = env.view.tips_badge_rect.expect("badge rect captured");

    assert!(!env.view.tips_badge_hovered);
    // Hovering the badge sets the highlight and reports a change.
    assert!(env.view.handle_hover(rect.x, rect.y));
    assert!(env.view.tips_badge_hovered);
    // Moving off clears it.
    assert!(env.view.handle_hover(0, 0));
    assert!(!env.view.tips_badge_hovered);
}

#[test]
#[serial]
fn earned_new_from_selection_tip_pops_after_repeated_n_with_selection() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id);
    let before = env.view.tips_unseen;

    // Open + cancel `n` with a selection enough times to earn the tip.
    for _ in 0..crate::tips::NEW_FROM_SELECTION_TIP_THRESHOLD {
        env.view.handle_key(key(KeyCode::Char('n')), None);
        assert!(
            env.view.new_dialog.is_some(),
            "n opens the new-session dialog"
        );
        env.view.handle_key(key(KeyCode::Esc), None);
    }

    // The earned tip is now in the badge and queued to pop.
    assert_eq!(
        env.view.tips_unseen,
        before + 1,
        "earned tip joins the badge"
    );
    assert!(
        env.view.pending_tip_pop.is_some(),
        "earned tip should be queued after the threshold"
    );

    // The next idle keystroke drains the queue into the tips overlay.
    assert!(env.view.tips_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('j')), None);
    assert!(
        env.view.tips_dialog.is_some(),
        "queued earned tip should pop on the next keystroke"
    );
    assert!(env.view.pending_tip_pop.is_none(), "pop is drained once");
}

#[test]
#[serial]
fn earned_tip_does_not_pop_when_tips_disabled() {
    use crate::tui::dialogs::TipsOutcome;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id);
    env.view.persist_tips_outcome(TipsOutcome {
        newly_seen: vec![],
        disabled: Some(true),
    });

    for _ in 0..crate::tips::NEW_FROM_SELECTION_TIP_THRESHOLD {
        env.view.handle_key(key(KeyCode::Char('n')), None);
        env.view.handle_key(key(KeyCode::Esc), None);
    }

    assert!(
        env.view.pending_tip_pop.is_none(),
        "disabled tips must not queue a pop"
    );
    assert_eq!(env.view.tips_unseen, 0, "disabled tips => empty badge");
}

#[test]
#[serial]
fn using_n_suppresses_the_earned_tip() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id);
    // Earn the tip (badge showing) without queueing a pop.
    earn_tip(&mut env);
    let earned = env.view.tips_unseen;
    assert!(earned > 0, "tip is earned and badged");

    // The user discovers N for themselves: open new-from-selection.
    env.view.handle_key(key(KeyCode::Char('N')), None);
    assert!(
        env.view.new_dialog.is_some(),
        "N opens the new-from-selection dialog"
    );
    // The earned tip drops from the badge (rotation tips, if any, remain).
    assert_eq!(
        env.view.tips_unseen,
        earned - 1,
        "using N suppresses the tip that teaches it"
    );

    let config = crate::session::config::load_config()
        .unwrap()
        .unwrap_or_default();
    assert!(
        config.app_state.used_new_from_selection,
        "N use is persisted"
    );
}

#[test]
#[serial]
fn test_reload_does_not_snap_cursor_after_enter() {
    let mut env = create_test_env_with_sessions(5);
    // Search and commit with Enter: matches stay non-empty so
    // `refresh_search_matches` fires on reload; the cursor must not snap.
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(!env.view.search_active);

    // Navigate away from the search result
    env.view.cursor = 4;
    env.view.update_selected();

    // Simulate periodic reload
    env.view.reload().unwrap();

    // Cursor should stay where the user put it, not snap back to best match
    assert_eq!(env.view.cursor, 4);
}

#[test]
#[serial]
fn test_cursor_moves_over_full_list_during_search() {
    let mut env = create_test_env_with_sessions(10);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();

    // Cursor should be able to move to last item in full list
    env.view.cursor = 0;
    for _ in 0..20 {
        env.view.move_cursor(1);
    }
    assert_eq!(env.view.cursor, 9); // last item in 10-item list
}

#[test]
#[serial]
fn test_r_opens_rename_dialog() {
    let mut env = create_test_env_with_sessions(3);
    env.view.update_selected();
    assert!(env.view.rename_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('r')), None);
    assert!(env.view.rename_dialog.is_some());
}

#[test]
#[serial]
fn test_rename_dialog_opened_on_group() {
    let mut env = create_test_env_with_groups();
    env.view.cursor = 1;
    env.view.update_selected();
    assert!(env.view.selected_group.is_some());
    assert!(env.view.rename_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('r')), None);
    assert!(env.view.rename_dialog.is_some());
    assert!(env.view.group_rename_context.is_some());
}

#[test]
#[serial]
fn test_has_dialog_returns_true_for_rename_dialog() {
    let mut env = create_test_env_with_sessions(1);
    env.view.update_selected();
    assert!(!env.view.has_dialog());
    env.view.handle_key(key(KeyCode::Char('r')), None);
    assert!(env.view.has_dialog());
}

#[test]
#[serial]
fn test_select_session_by_id() {
    let mut env = create_test_env_with_sessions(3);
    let session_id = env.view.instance_at(1).id.clone();

    assert_eq!(env.view.cursor, 0);

    env.view.select_session_by_id(&session_id);

    assert_eq!(env.view.cursor, 1);
    assert_eq!(env.view.selected_session, Some(session_id));
}

#[test]
#[serial]
fn test_select_session_by_id_nonexistent() {
    let mut env = create_test_env_with_sessions(3);

    assert_eq!(env.view.cursor, 0);
    env.view.select_session_by_id("nonexistent-id");
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_select_top_attention_lands_on_first_session() {
    let mut env = create_test_env_with_sessions(3);
    env.view.cursor = 2;
    env.view.update_selected();
    assert_eq!(env.view.cursor, 2);

    env.view.select_top_attention(None);

    assert_eq!(env.view.cursor, 0);
    if let Item::Session { id, .. } = &env.view.flat_items[0] {
        assert_eq!(env.view.selected_session.as_deref(), Some(id.as_str()));
    } else {
        panic!("expected first flat_items row to be a Session");
    }
}

#[test]
#[serial]
fn test_select_top_attention_skips_returning_session() {
    let mut env = create_test_env_with_sessions(3);

    // Grab id of first session (the one we're "returning from").
    let first_id = if let Item::Session { id, .. } = &env.view.flat_items[0] {
        id.clone()
    } else {
        panic!("expected first flat_items row to be a Session");
    };
    let second_id = if let Item::Session { id, .. } = &env.view.flat_items[1] {
        id.clone()
    } else {
        panic!("expected second flat_items row to be a Session");
    };

    env.view.cursor = 0;
    env.view.update_selected();

    // Simulate returning from `first_id`: skip it, land on the next session.
    env.view.select_top_attention(Some(&first_id));

    assert_eq!(env.view.cursor, 1);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(second_id.as_str())
    );
}

#[test]
#[serial]
fn test_select_top_attention_falls_back_to_returning_when_only_session() {
    let mut env = create_test_env_with_sessions(1);

    let only_id = if let Item::Session { id, .. } = &env.view.flat_items[0] {
        id.clone()
    } else {
        panic!("expected first flat_items row to be a Session");
    };

    env.view.cursor = 0;
    env.view.update_selected();

    // Only one session; skip would leave nothing; must fall back to it.
    env.view.select_top_attention(Some(&only_id));

    assert_eq!(env.view.cursor, 0);
    assert_eq!(env.view.selected_session.as_deref(), Some(only_id.as_str()));
}

#[test]
#[serial]
fn test_uppercase_p_opens_profile_picker() {
    let env = create_test_env_empty();
    let mut view = env.view;

    assert!(view.profile_picker_dialog.is_none());
    let action = view.handle_key(key(KeyCode::Char('P')), None);
    assert_eq!(action, None);
    assert!(view.profile_picker_dialog.is_some());
}

#[test]
#[serial]
fn test_uppercase_p_in_search_mode_does_not_open_picker() {
    let env = create_test_env_empty();
    let mut view = env.view;

    // Enter search mode
    view.handle_key(key(KeyCode::Char('/')), None);
    assert!(view.search_active);

    // P should be treated as search input, not open picker
    view.handle_key(key(KeyCode::Char('P')), None);
    assert!(view.profile_picker_dialog.is_none());
    assert_eq!(view.search_query.value(), "P");
}

#[test]
#[serial]
fn test_uppercase_p_picker_esc_closes() {
    let env = create_test_env_empty();
    let mut view = env.view;

    view.handle_key(key(KeyCode::Char('P')), None);
    assert!(view.profile_picker_dialog.is_some());

    view.handle_key(key(KeyCode::Esc), None);
    assert!(view.profile_picker_dialog.is_none());
}

#[test]
#[serial]
fn test_uppercase_p_picker_switch_profile() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    crate::session::create_profile("first").unwrap();
    crate::session::create_profile("second").unwrap();

    let _storage = Storage::new_unwatched("first").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("first".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Open picker
    view.handle_key(key(KeyCode::Char('P')), None);
    assert!(view.profile_picker_dialog.is_some());

    // In filtered mode, "all" is at top, then "first", "second", "test"
    // Navigate down to reach "second" and select it
    view.handle_key(key(KeyCode::Down), None);
    view.handle_key(key(KeyCode::Down), None);
    view.handle_key(key(KeyCode::Down), None);
    let action = view.handle_key(key(KeyCode::Enter), None);
    // Profile switch is handled internally, no Action returned
    assert_eq!(action, None);
    assert_eq!(view.active_profile, Some("second".to_string()));
    assert!(view.profile_picker_dialog.is_none());
}

#[test]
#[serial]
fn test_t_toggles_view_mode() {
    let env = create_test_env_empty();
    let mut view = env.view;

    assert_eq!(view.view_mode, ViewMode::Structured);

    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);

    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Structured);
}

#[test]
#[serial]
fn switching_view_retargets_capture_worker_pane() {
    // The preview's off-thread capture worker follows the displayed pane:
    // switching agent <-> terminal must resolve to different tmux sessions
    // so `sync_preview_capture_worker` respawns the worker against the new
    // pane (instead of the old agent-only behavior). Regression guard for
    // the responsiveness fix that moved every preview's `tmux capture-pane`
    // off the render thread.
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    let agent_pane = view.displayed_pane_tmux_name();
    assert!(
        agent_pane.is_some(),
        "a selected session must resolve a pane"
    );

    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);
    let terminal_pane = view.displayed_pane_tmux_name();
    assert!(terminal_pane.is_some());
    assert_ne!(
        agent_pane, terminal_pane,
        "agent and terminal panes must differ so the worker retargets on switch",
    );

    // The reconcile tracks the active target and is idempotent: a changed
    // pane updates it, the same pane leaves it in place.
    view.sync_preview_capture_worker(terminal_pane.clone());
    assert_eq!(view.preview_capture_target, terminal_pane);
    view.sync_preview_capture_worker(terminal_pane.clone());
    assert_eq!(view.preview_capture_target, terminal_pane);
    view.sync_preview_capture_worker(agent_pane.clone());
    assert_eq!(view.preview_capture_target, agent_pane);
}

#[test]
#[serial]
fn test_enter_returns_attach_terminal_in_terminal_view() {
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    // In Structured view, Enter returns AttachSession
    let action = view.handle_key(key(KeyCode::Enter), None);
    assert!(matches!(action, Some(Action::AttachSession(_))));

    // Switch to Terminal view
    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);

    // In Terminal view, Enter returns AttachTerminal
    let action = view.handle_key(key(KeyCode::Enter), None);
    assert!(matches!(action, Some(Action::AttachTerminal(_, _))));
}

#[test]
#[serial]
fn test_shift_t_attaches_terminal_from_structured_view() {
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    // Should be in Structured view by default
    assert_eq!(view.view_mode, ViewMode::Structured);

    // Shift+T should return AttachTerminal without switching view mode
    let action = view.handle_key(key(KeyCode::Char('T')), None);
    assert!(matches!(action, Some(Action::AttachTerminal(_, _))));
    assert_eq!(view.view_mode, ViewMode::Structured);
}

#[test]
#[serial]
fn test_shift_t_attaches_terminal_from_terminal_view() {
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    // Switch to Terminal view
    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);

    // Shift+T should also work from Terminal view
    let action = view.handle_key(key(KeyCode::Char('T')), None);
    assert!(matches!(action, Some(Action::AttachTerminal(_, _))));
}

#[test]
#[serial]
fn test_shift_t_noop_with_no_sessions() {
    let env = create_test_env_empty();
    let mut view = env.view;

    let action = view.handle_key(key(KeyCode::Char('T')), None);
    assert!(action.is_none());
}

#[test]
#[serial]
fn test_d_shows_info_dialog_in_terminal_view() {
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    // Switch to Terminal view
    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);

    // Press 'd' - should show info dialog, not delete dialog
    assert!(view.info_dialog.is_none());
    view.handle_key(key(KeyCode::Char('d')), None);
    assert!(view.info_dialog.is_some());
    assert!(view.unified_delete_dialog.is_none());
}

#[test]
#[serial]
fn test_has_dialog_includes_info_dialog() {
    let env = create_test_env_empty();
    let mut view = env.view;

    assert!(!view.has_dialog());

    view.info_dialog = Some(InfoDialog::new("Test", "Test message"));
    assert!(view.has_dialog());
}

#[test]
#[serial]
fn test_has_dialog_includes_settings_view() {
    use crate::tui::settings::SettingsView;

    let env = create_test_env_empty();
    let mut view = env.view;

    assert!(!view.has_dialog());

    view.settings_view = Some(SettingsView::new("test", None).unwrap());
    assert!(view.has_dialog());
}

#[test]
#[serial]
fn test_s_opens_settings_view() {
    let mut env = create_test_env_empty();
    assert!(env.view.settings_view.is_none());
    env.view.handle_key(key(KeyCode::Char('s')), None);
    assert!(env.view.settings_view.is_some());
}

// Group deletion tests

fn create_test_env_with_group_sessions() -> TestEnv {
    use crate::session::{GroupTree, SandboxInfo};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut instances = Vec::new();

    // Ungrouped session
    let inst1 = Instance::new("ungrouped", "/tmp/u");
    instances.push(inst1);

    // Sessions in "work" group
    let mut inst2 = Instance::new("work-session-1", "/tmp/work1");
    inst2.group_path = "work".to_string();
    instances.push(inst2);

    let mut inst3 = Instance::new("work-session-2", "/tmp/work2");
    inst3.group_path = "work".to_string();
    inst3.sandbox_info = Some(SandboxInfo {
        enabled: true,
        container_id: None,
        image: "ubuntu:latest".to_string(),
        container_name: "test-container".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: None,
    });
    instances.push(inst3);

    // Session in nested group
    let mut inst4 = Instance::new("work-nested", "/tmp/work/nested");
    inst4.group_path = "work/projects".to_string();
    instances.push(inst4);

    // Build group tree from instances and save with groups
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();
    TestEnv {
        view,
        _guard,
        _temp: temp,
    }
}

/// Trashing and restoring a session through the view's own actions keeps the
/// group header count in step with the rows, since both are rebuilt from the
/// same predicate. Guards against the count and the visible rows drifting.
#[test]
#[serial]
fn group_header_count_tracks_trash_and_restore() {
    let mut env = create_test_env_with_group_sessions();
    env.view.trashed_section_collapsed = false;

    let work_count = |env: &TestEnv| -> usize {
        env.view
            .flat_items
            .iter()
            .find_map(|i| match i {
                Item::Group {
                    path,
                    session_count,
                    ..
                } if path == "work" => Some(*session_count),
                _ => None,
            })
            .expect("work group header present")
    };

    // "work" holds two direct sessions plus one in the nested "work/projects".
    assert_eq!(work_count(&env), 3);

    let target = env
        .view
        .instances
        .values()
        .find(|i| i.group_path == "work")
        .map(|i| i.id.clone())
        .expect("a direct work session");

    env.view.trash_session_by_id(&target);
    assert_eq!(
        work_count(&env),
        2,
        "trashed session drops out of the count"
    );

    env.view.select_session_by_id(&target);
    env.view.toggle_archive_at_cursor().unwrap();
    assert_eq!(work_count(&env), 3, "restored session returns to the count");
}

#[test]
#[serial]
fn test_group_has_managed_worktrees() {
    use crate::session::WorktreeInfo;
    use chrono::Utc;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("work-session", "/tmp/work");
    inst1.group_path = "work".to_string();
    inst1.worktree_info = Some(WorktreeInfo {
        branch: "feature-branch".to_string(),
        main_repo_path: "/tmp/main".to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });

    let mut inst2 = Instance::new("other-session", "/tmp/other");
    inst2.group_path = "other".to_string();

    {
        let xs: Vec<Instance> = vec![inst1, inst2];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    assert!(view.group_has_managed_worktrees("work", "work/", None));
    assert!(!view.group_has_managed_worktrees("other", "other/", None));
}

#[test]
#[serial]
fn test_group_has_containers() {
    use crate::session::SandboxInfo;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("work-session", "/tmp/work");
    inst1.group_path = "work".to_string();
    inst1.sandbox_info = Some(SandboxInfo {
        enabled: true,
        container_id: None,
        image: "ubuntu:latest".to_string(),
        container_name: "test-container".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: None,
    });

    let mut inst2 = Instance::new("other-session", "/tmp/other");
    inst2.group_path = "other".to_string();

    {
        let xs: Vec<Instance> = vec![inst1, inst2];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    assert!(view.group_has_containers("work", "work/", None));
    assert!(!view.group_has_containers("other", "other/", None));
}

#[test]
#[serial]
fn test_delete_selected_group_updates_groups_field() {
    let mut env = create_test_env_with_group_sessions();

    // Select the "work" group
    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }

    assert!(env.view.selected_group.is_some());
    assert!(env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work"));

    // Delete the group (this moves sessions to default)
    env.view.delete_selected_group().unwrap();

    // Verify the group is removed from group_tree
    assert!(!env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work"));

    // Verify self.groups is updated (this is the bug fix)
    let all_groups = env.view.all_groups();
    let group_paths: Vec<_> = all_groups.iter().map(|g| g.path.as_str()).collect();
    assert!(!group_paths.contains(&"work"));
    assert!(!group_paths.contains(&"work/projects"));
}

/// Archiving a manual group archives every session under it, including
/// nested subgroups, and leaves sessions outside the group untouched.
#[test]
#[serial]
fn test_archive_selected_group_archives_all_members() {
    let mut env = create_test_env_with_group_sessions();

    // Select the "work" group.
    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }
    assert_eq!(env.view.selected_group.as_deref(), Some("work"));

    // "work" holds two direct sessions plus one in the nested "work/projects".
    assert_eq!(env.view.active_sessions_in_selected_group().len(), 3);

    env.view.archive_selected_group().unwrap();

    for inst in env.view.instances() {
        let in_work = inst.group_path == "work" || inst.group_path.starts_with("work/");
        assert_eq!(
            inst.is_archived(),
            in_work,
            "session {} (group {:?}) archived state should match group membership",
            inst.title,
            inst.group_path
        );
    }
}

/// Locks #1868: bulk archive persists synchronously even though tmux
/// teardown runs off-thread. Real tmux state asserted in
/// `tests/e2e/archive_restore.rs`.
#[test]
#[serial]
fn test_archive_selected_group_widened_teardown_persists_synchronously() {
    let mut env = create_test_env_with_group_sessions();

    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }
    assert_eq!(env.view.selected_group.as_deref(), Some("work"));
    let work_ids: Vec<String> = env.view.active_sessions_in_selected_group();
    assert_eq!(work_ids.len(), 3);

    let result = env.view.archive_selected_group();
    assert!(
        result.is_ok(),
        "archive_selected_group must return Ok even when the off-thread \
         teardown is fire-and-forget; got {:?}",
        result
    );

    for id in &work_ids {
        let inst = env
            .view
            .instances()
            .find(|i| &i.id == id)
            .expect("group member must still exist after archive");
        assert!(
            inst.is_archived(),
            "session {} ({}) must have archived_at set synchronously \
             on the input thread before archive_selected_group returns",
            inst.title,
            id
        );
    }
}

/// In project group-by mode, archiving a project header archives every live
/// session that maps to that repo, even though their stored `group_path`
/// values differ from the synthetic project name.
#[test]
#[serial]
fn test_archive_selected_group_project_mode() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    // Two sessions sharing one repo, one session in a different repo.
    let a1 = Instance::new("alpha-1", "/tmp/alpha");
    let a2 = Instance::new("alpha-2", "/tmp/alpha");
    let b1 = Instance::new("beta-1", "/tmp/beta");
    let instances = vec![a1, a2, b1];
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Project;
    view.flat_items = view.build_flat_items();

    // Select the "alpha" project header.
    for (i, item) in view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "alpha" {
                view.cursor = i;
                view.update_selected();
                break;
            }
        }
    }
    assert_eq!(view.selected_group.as_deref(), Some("alpha"));
    assert_eq!(view.active_sessions_in_selected_group().len(), 2);

    view.archive_selected_group().unwrap();

    for inst in view.instances() {
        let in_alpha = inst.project_path == "/tmp/alpha";
        assert_eq!(
            inst.is_archived(),
            in_alpha,
            "session {} (repo {}) archived state should match project membership",
            inst.title,
            inst.project_path
        );
    }
}

/// The group-level prompt opens a confirmation carrying the `archive_group`
/// action and counts only the active members, and no-ops without a prompt when
/// the group has nothing left to archive.
#[test]
#[serial]
fn test_prompt_archive_selected_group() {
    let mut env = create_test_env_with_group_sessions();

    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }

    env.view.prompt_archive_selected_group();
    assert_eq!(
        env.view.confirm_dialog.as_ref().map(|d| d.action()),
        Some("archive_group")
    );

    // Confirm, which archives the group and clears the prompt.
    env.view.confirm_dialog = None;
    env.view.archive_selected_group().unwrap();

    // With every member archived, a second prompt is a silent no-op.
    env.view.prompt_archive_selected_group();
    assert!(env.view.confirm_dialog.is_none());
}

#[test]
#[serial]
fn test_delete_group_with_sessions_updates_groups_field() {
    use crate::session::Status;
    use crate::tui::dialogs::GroupDeleteOptions;

    let mut env = create_test_env_with_group_sessions();

    // Select the "work" group
    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }

    assert!(env.view.selected_group.is_some());
    let initial_instance_count = env.view.instances().len();

    // Delete the group with all sessions
    let options = GroupDeleteOptions {
        delete_sessions: true,
        delete_worktrees: false,
        delete_branches: false,
        delete_containers: false,
        force_delete_worktrees: false,
    };
    env.view.delete_group_with_sessions(&options).unwrap();

    // Verify the group is removed from group_tree
    assert!(!env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work"));
    assert!(!env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work/projects"));

    // Verify self.groups is updated (this is the bug fix)
    let all_groups = env.view.all_groups();
    let group_paths: Vec<_> = all_groups.iter().map(|g| g.path.as_str()).collect();
    assert!(!group_paths.contains(&"work"));
    assert!(!group_paths.contains(&"work/projects"));

    // Verify sessions are marked as deleting
    let deleting_count = env
        .view
        .instances()
        .filter(|i| i.status == Status::Deleting)
        .count();
    // Should have 3 sessions in the work group marked as deleting
    assert_eq!(deleting_count, 3);

    // Instance count should remain the same (they're marked as deleting, not removed yet)
    assert_eq!(env.view.instances().len(), initial_instance_count);
}

#[test]
#[serial]
fn test_delete_group_with_sessions_respects_worktree_option() {
    use crate::session::WorktreeInfo;
    use crate::tui::dialogs::GroupDeleteOptions;
    use chrono::Utc;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("work-session", "/tmp/work");
    inst1.group_path = "work".to_string();
    inst1.worktree_info = Some(WorktreeInfo {
        branch: "feature".to_string(),
        main_repo_path: "/tmp/main".to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });

    {
        let xs: Vec<Instance> = vec![inst1];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Select the work group
    view.cursor = 0;
    view.update_selected();
    assert!(view.selected_group.is_some());

    // Delete with worktrees option enabled
    let options = GroupDeleteOptions {
        delete_sessions: true,
        delete_worktrees: true,
        delete_branches: false,
        delete_containers: false,
        force_delete_worktrees: false,
    };
    view.delete_group_with_sessions(&options).unwrap();

    // We can't easily verify the deletion request was sent with the right flags
    // without mocking, but we can verify the group was deleted
    assert!(!view.group_trees.get("test").unwrap().group_exists("work"));
}

#[test]
#[serial]
fn test_delete_group_with_sessions_respects_container_option() {
    use crate::session::SandboxInfo;
    use crate::tui::dialogs::GroupDeleteOptions;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("work-session", "/tmp/work");
    inst1.group_path = "work".to_string();
    inst1.sandbox_info = Some(SandboxInfo {
        enabled: true,
        container_id: None,
        image: "ubuntu:latest".to_string(),
        container_name: "test-container".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: None,
    });

    {
        let xs: Vec<Instance> = vec![inst1];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Select the work group
    view.cursor = 0;
    view.update_selected();
    assert!(view.selected_group.is_some());

    // Delete with containers option enabled
    let options = GroupDeleteOptions {
        delete_sessions: true,
        delete_worktrees: false,
        delete_branches: false,
        delete_containers: true,
        force_delete_worktrees: false,
    };
    view.delete_group_with_sessions(&options).unwrap();

    // Verify the group was deleted
    assert!(!view.group_trees.get("test").unwrap().group_exists("work"));
}

#[test]
#[serial]
fn test_delete_group_includes_nested_groups() {
    use crate::tui::dialogs::GroupDeleteOptions;

    let mut env = create_test_env_with_group_sessions();

    // Select the "work" group
    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }

    // Verify nested group exists
    assert!(env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work/projects"));

    // Delete the group with all sessions
    let options = GroupDeleteOptions {
        delete_sessions: true,
        delete_worktrees: false,
        delete_branches: false,
        delete_containers: false,
        force_delete_worktrees: false,
    };
    env.view.delete_group_with_sessions(&options).unwrap();

    // Verify both parent and nested groups are removed
    assert!(!env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work"));
    assert!(!env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work/projects"));
}

#[test]
#[serial]
fn test_groups_field_stays_in_sync_with_storage() {
    let mut env = create_test_env_with_group_sessions();

    // Get initial group count
    let initial_group_count = env.view.all_groups().len();
    assert!(initial_group_count > 0);

    // Select and delete the work group
    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }

    env.view.delete_selected_group().unwrap();

    // After deletion, groups field should be smaller
    assert!(env.view.all_groups().len() < initial_group_count);

    // Reload from storage and verify groups match
    env.view.reload().unwrap();
    let reloaded_groups: Vec<_> = env
        .view
        .all_groups()
        .iter()
        .map(|g| g.path.clone())
        .collect();
    let tree_groups: Vec<_> = env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .get_all_groups()
        .iter()
        .map(|g| g.path.clone())
        .collect();
    assert_eq!(reloaded_groups, tree_groups);
}

#[test]
#[serial]
fn test_group_collapsed_state_persists_across_reload() {
    let mut env = create_test_env_with_groups();

    // Find a group and verify it starts expanded
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("should have a group");

    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(!collapsed, "group should start expanded");
    }

    // Move cursor to group and collapse it with Enter
    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);

    // Verify it's collapsed
    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(*collapsed, "group should be collapsed after Enter");
    }

    // Reload (simulates the 5-second periodic refresh)
    env.view.reload().unwrap();

    // Find the group again (index may change after reload)
    let group_idx_after = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("should still have a group");

    // Verify it's still collapsed after reload
    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx_after] {
        assert!(*collapsed, "group should remain collapsed after reload");
    }
}

#[test]
#[serial]
fn test_group_collapsed_state_saved_to_storage() {
    use crate::session::GroupTree;

    let mut env = create_test_env_with_groups();

    // Find a group
    let group_path = env
        .view
        .flat_items
        .iter()
        .find_map(|item| {
            if let Item::Group { path, .. } = item {
                Some(path.clone())
            } else {
                None
            }
        })
        .expect("should have a group");

    // Move cursor to group and collapse it
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if path == &group_path))
        .unwrap();
    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);

    // Load fresh from storage to verify persistence
    let (_, groups) = env
        .view
        .storages
        .get("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let fresh_tree =
        GroupTree::new_with_groups(&env.view.instances().cloned().collect::<Vec<_>>(), &groups);
    let all_groups = fresh_tree.get_all_groups();

    let saved_group = all_groups
        .iter()
        .find(|g| g.path == group_path)
        .expect("group should exist in storage");

    assert!(
        saved_group.collapsed,
        "collapsed state should be persisted to storage"
    );
}

/// Project-mode folder collapse must survive a restart. Unlike group mode
/// (persisted on the per-profile GroupTree), project folders are auto-derived
/// and have no group record, so their collapse state is written to
/// `app_state.project_group_collapsed`. Regression for collapsed project
/// folders re-expanding on relaunch.
#[test]
#[serial]
fn test_project_group_collapsed_state_persists_to_config() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    // Find a project folder header and confirm it starts expanded.
    let (group_idx, group_path) = env
        .view
        .flat_items
        .iter()
        .enumerate()
        .find_map(|(idx, item)| match item {
            Item::Group {
                path, collapsed, ..
            } => {
                assert!(!collapsed, "project folder should start expanded");
                Some((idx, path.clone()))
            }
            _ => None,
        })
        .expect("project mode should have a folder header");

    // Collapse it via Enter, which routes through toggle_group_collapsed.
    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);

    // The collapsed path must be persisted to the on-disk config.
    let config = crate::session::config::load_config()
        .unwrap()
        .expect("config should exist after collapse");
    assert!(
        config
            .app_state
            .project_group_collapsed
            .contains(&group_path),
        "collapsed project folder path should be persisted to app_state"
    );

    // A freshly constructed HomeView (simulating relaunch) must restore it.
    let fresh = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert_eq!(
        fresh.project_group_collapsed.get(&group_path).copied(),
        Some(true),
        "relaunched HomeView should restore the collapsed project folder"
    );
}

/// A collapse entry for a project that no longer exists must be pruned on save
/// so the persisted set can't grow without bound as projects come and go. A
/// still-live folder collapsed in the same session must survive.
#[test]
#[serial]
fn test_project_group_collapsed_prunes_stale_paths() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    // A real folder the user collapsed this session.
    let live_path = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            Item::Group { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("project mode should have a folder header");

    env.view
        .project_group_collapsed
        .insert(live_path.clone(), true);
    // A stale entry for a project that isn't part of this session at all.
    env.view
        .project_group_collapsed
        .insert("/repos/deleted-ghost".to_string(), true);

    env.view.save_project_group_collapsed();

    let config = crate::session::config::load_config()
        .unwrap()
        .expect("config should exist after save");
    let saved = &config.app_state.project_group_collapsed;
    assert!(
        saved.contains(&live_path),
        "a live collapsed folder must be persisted"
    );
    assert!(
        !saved.iter().any(|p| p == "/repos/deleted-ghost"),
        "a collapse entry for a nonexistent project must be pruned"
    );
}

#[test]
#[serial]
fn test_list_width_default() {
    let env = create_test_env_empty();
    assert_eq!(env.view.list_width, 35);
}

#[test]
#[serial]
fn test_shrink_list() {
    let mut env = create_test_env_empty();
    env.view.shrink_list();
    assert_eq!(env.view.list_width, 30);
}

#[test]
#[serial]
fn test_grow_list() {
    let mut env = create_test_env_empty();
    env.view.grow_list();
    assert_eq!(env.view.list_width, 40);
}

#[test]
#[serial]
fn test_shrink_list_clamps_at_minimum() {
    let mut env = create_test_env_empty();
    env.view.list_width = 12;
    env.view.shrink_list();
    assert_eq!(env.view.list_width, 10);
    env.view.shrink_list();
    assert_eq!(env.view.list_width, 10);
}

#[test]
#[serial]
fn test_grow_list_clamps_at_maximum() {
    let mut env = create_test_env_empty();
    env.view.list_width = 78;
    env.view.grow_list();
    assert_eq!(env.view.list_width, 80);
    env.view.grow_list();
    assert_eq!(env.view.list_width, 80);
}

#[test]
#[serial]
fn test_lt_shrinks_list() {
    let mut env = create_test_env_empty();
    assert_eq!(env.view.list_width, 35);
    env.view.handle_key(key(KeyCode::Char('<')), None);
    assert_eq!(env.view.list_width, 30);
}

#[test]
#[serial]
fn test_gt_grows_list() {
    let mut env = create_test_env_empty();
    assert_eq!(env.view.list_width, 35);
    env.view.handle_key(key(KeyCode::Char('>')), None);
    assert_eq!(env.view.list_width, 40);
}

#[test]
#[serial]
fn test_sort_order_defaults_to_newest() {
    use crate::session::config::SortOrder;

    let env = create_test_env_with_mixed_sessions();
    assert_eq!(env.view.sort_order, SortOrder::Newest);
}

#[test]
#[serial]
fn test_o_key_opens_sort_picker() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // 'o' opens the picker; the current sort is unchanged until the user
    // confirms a selection.
    env.view.handle_key(key(KeyCode::Char('o')), None);
    assert!(env.view.sort_picker_dialog.is_some());
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // Walk to AZ (Newest -> Attention -> LastActivity -> Oldest -> AZ) and
    // confirm.
    for _ in 0..4 {
        env.view.handle_key(key(KeyCode::Down), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.sort_picker_dialog.is_none());
    assert_eq!(env.view.sort_order, SortOrder::AZ);
}

#[test]
#[serial]
fn test_shift_o_opens_sort_picker_in_strict_mode() {
    // Regression guard: the SortPicker binding lists Shift+O (Char('O')) for
    // strict mode, so it must resolve to the sort picker rather than falling
    // through to the typing-guard (capture_letter_to_compose).
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    env.view.strict_hotkeys = true;
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // Shift+O: opens the sort picker.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT), None);
    assert!(env.view.sort_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);

    // Some terminals drop the SHIFT modifier and send bare uppercase. Cover
    // that too.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::NONE), None);
    assert!(env.view.sort_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);

    // Ctrl+o also opens the picker in strict mode.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        None,
    );
    assert!(env.view.sort_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);

    // Sort order is unchanged because no selection was confirmed.
    assert_eq!(env.view.sort_order, SortOrder::Newest);
    // Sanity: message dialog must NOT have been opened as a side effect.
    assert!(env.view.send_message_dialog.is_none());
}

#[test]
#[serial]
fn test_bare_lowercase_o_does_not_cycle_sort_in_strict_mode() {
    // Regression guard (2026-04-22): in strict_hotkeys mode, plain lowercase 'o'
    // MUST NOT cycle sort; it must fall through to the typing-guard catch-all
    // (message dialog) per the "no destructive lowercase" rule. Only Shift+O
    // (Char('O')) and Ctrl+O should change sort order in strict mode.
    //
    // The previous implementation collapsed the two sort arms into a single
    // unguarded `Char('o') => cycle`, which fired for bare 'o' too, breaking
    // the contract and silently changing the user's sort order whenever they
    // tried to type 'o' as text input.
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    env.view.strict_hotkeys = true;
    let initial = env.view.sort_order;
    assert_eq!(initial, SortOrder::Newest);

    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), None);

    assert_eq!(
        env.view.sort_order, initial,
        "bare 'o' in strict mode must NOT cycle sort; expected it to stay at Newest"
    );
}

#[test]
#[serial]
fn test_strict_mode_h_collapses_group() {
    // Regression guard: the help overlay lists "h/←" for Collapse group in
    // strict mode. Bare lowercase `h` must walk through the dispatch and
    // collapse the cursor's group, mirroring `l`/Right for expand. Without
    // the explicit `Char('h')` arm next to `KeyCode::Left`, `h` would fall
    // into the strict-mode typing-guard catch-all and the advertised
    // navigation hotkey would silently open the compose dialog.
    let mut env = create_test_env_with_groups();
    env.view.strict_hotkeys = true;

    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("setup should produce a group");

    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(!collapsed, "group should start expanded");
    }
    env.view.cursor = group_idx;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('h')), None);

    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(
            *collapsed,
            "bare 'h' in strict mode must collapse the group"
        );
    }
    assert!(
        env.view.pending_paste.is_none(),
        "bare 'h' in strict mode must not leak into the typing-guard catch-all"
    );
}

#[test]
#[serial]
fn test_non_strict_h_snoozes_only_in_attention_sort() {
    // Snooze is Attention-mode-only: in Attention sort `h` toggles snooze on
    // the cursor's session and the group below the cursor stays expanded;
    // in every other sort mode the snooze arm declines, control falls
    // through to the unconditional `Left | Char('h')` collapse handler,
    // and the group collapses. Before the gating, snooze always caught
    // first in non-strict mode regardless of sort, which silently mutated
    // persisted state for users who weren't using Attention sort.
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_groups();
    env.view.strict_hotkeys = false;

    // Attention sort flattens groups out, so seed a cursor-on-session
    // scenario and assert that `h` opens the snooze duration dialog
    // (the actual snooze fires when the user picks a duration).
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    let session_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { .. }))
        .expect("setup should produce a session in Attention sort");
    env.view.cursor = session_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Char('h')), None);
    assert!(
        env.view.snooze_duration_dialog.is_some(),
        "`h` in Attention sort must open the snooze duration dialog"
    );
    // Tear the dialog back down before exercising the Newest case so the
    // next handle_key doesn't get swallowed by dialog input.
    env.view.snooze_duration_dialog = None;
    env.view.pending_snooze_session = None;

    // Now flip back to a non-Attention sort and confirm `h` falls
    // through to the collapse handler instead of snoozing.
    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("setup should produce a group in Newest sort");
    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Char('h')), None);
    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(
            *collapsed,
            "non-strict 'h' outside Attention sort must collapse the group, not snooze"
        );
    }
}

/// Build a flat list of one Running and one Waiting session in the given mode.
/// Returns the env plus the flat index of each so callers can park the cursor.
/// Statuses are seeded in storage before construction so `instances` and
/// what `get_instance`/`jump_to_next_waiting` read agree.
fn attention_env_running_then_waiting() -> (TestEnv, usize, usize) {
    use crate::session::config::{GroupByMode, SortOrder};
    use crate::session::Status;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut running = Instance::new("running", "/tmp/running");
    running.status = Status::Running;
    let mut waiting = Instance::new("waiting", "/tmp/waiting");
    waiting.status = Status::Waiting;
    let instances = vec![running, waiting];
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.strict_hotkeys = false;
    view.group_by = GroupByMode::Manual;
    view.sort_order = SortOrder::Attention;
    view.flat_items = view.build_flat_items();
    view.update_selected();
    let env = TestEnv {
        view,
        _guard,
        _temp: temp,
    };

    let status_at = |env: &TestEnv, idx: usize| match env.view.flat_items.get(idx) {
        Some(Item::Session { id, .. }) => env.view.get_instance(id).map(|i| i.status),
        _ => None,
    };
    let running = (0..env.view.flat_items.len())
        .find(|&i| status_at(&env, i) == Some(Status::Running))
        .expect("a Running session row");
    let waiting = (0..env.view.flat_items.len())
        .find(|&i| status_at(&env, i) == Some(Status::Waiting))
        .expect("a Waiting session row");
    (env, running, waiting)
}

fn attention_env_running_then_idle() -> (TestEnv, usize, usize) {
    use crate::session::config::{GroupByMode, SortOrder};
    use crate::session::Status;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut running = Instance::new("running", "/tmp/running");
    running.status = Status::Running;
    let mut idle = Instance::new("idle", "/tmp/idle");
    idle.status = Status::Idle;
    let instances = vec![running, idle];
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.strict_hotkeys = false;
    view.group_by = GroupByMode::Manual;
    view.sort_order = SortOrder::Attention;
    view.flat_items = view.build_flat_items();
    view.update_selected();
    let env = TestEnv {
        view,
        _guard,
        _temp: temp,
    };

    let status_at = |env: &TestEnv, idx: usize| match env.view.flat_items.get(idx) {
        Some(Item::Session { id, .. }) => env.view.get_instance(id).map(|i| i.status),
        _ => None,
    };
    let running = (0..env.view.flat_items.len())
        .find(|&i| status_at(&env, i) == Some(Status::Running))
        .expect("a Running session row");
    let idle = (0..env.view.flat_items.len())
        .find(|&i| status_at(&env, i) == Some(Status::Idle))
        .expect("an Idle session row");
    (env, running, idle)
}

#[test]
#[serial]
fn test_non_strict_w_jumps_to_next_waiting_in_attention_sort() {
    // Regression for #1524: in non-strict Attention sort, `w` must jump to the
    // next waiting/idle session (the #796 behavior) instead of snoozing the
    // cursor's session. Snooze lives on `h`/`H`; `w` is navigation. Previously
    // the snooze arm shadowed the jump arm in exactly the sort users triage in,
    // so `w` never felt like a navigation key.
    use crate::session::Status;

    let (mut env, running, _waiting) = attention_env_running_then_waiting();
    env.view.cursor = running;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('w')), None);

    assert!(
        env.view.snooze_duration_dialog.is_none(),
        "`w` in Attention sort must jump, not open the snooze dialog"
    );
    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => env.view.get_instance(id).map(|i| i.status),
        _ => None,
    };
    assert_eq!(
        landed,
        Some(Status::Waiting),
        "`w` should land the cursor on the Waiting session"
    );
}

#[test]
#[serial]
fn test_non_strict_w_on_running_jumps_to_idle_in_attention_sort() {
    use crate::session::Status;

    let (mut env, running, _idle) = attention_env_running_then_idle();
    env.view.cursor = running;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('w')), None);

    assert!(
        env.view.snooze_duration_dialog.is_none(),
        "`w` on a Running session must jump, not open the snooze dialog"
    );
    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => env.view.get_instance(id).map(|i| i.status),
        _ => None,
    };
    assert_eq!(
        landed,
        Some(Status::Idle),
        "`w` should fall back to the available Idle session"
    );
}

#[test]
#[serial]
fn test_non_strict_w_on_collapsed_project_group_reveals_idle_in_attention_sort() {
    use crate::session::config::{GroupByMode, SortOrder};
    use crate::session::Status;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut alpha_idle = Instance::new("alpha-idle", "/repos/alpha");
    alpha_idle.status = Status::Idle;
    let alpha_id = alpha_idle.id.clone();
    let mut beta_running = Instance::new("beta-running", "/repos/beta");
    beta_running.status = Status::Running;
    let instances = vec![alpha_idle, beta_running];
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.strict_hotkeys = false;
    view.group_by = GroupByMode::Project;
    view.sort_order = SortOrder::Attention;
    view.project_group_collapsed
        .insert("alpha".to_string(), true);
    view.flat_items = view.build_flat_items();

    let alpha_group = view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { name, collapsed, .. } if name == "alpha" && *collapsed))
        .expect("collapsed alpha project group");
    assert!(
        !view
            .flat_items
            .iter()
            .any(|item| matches!(item, Item::Session { id, .. } if id == &alpha_id)),
        "precondition: alpha idle session should be hidden by the collapsed group"
    );
    view.cursor = alpha_group;
    view.update_selected();

    view.handle_key(key(KeyCode::Char('w')), None);

    assert!(
        view.snooze_duration_dialog.is_none(),
        "`w` on a collapsed group must jump, not open the snooze dialog"
    );
    assert_eq!(view.selected_session.as_deref(), Some(alpha_id.as_str()));
    assert!(
        view.flat_items
            .iter()
            .any(|item| matches!(item, Item::Session { id, .. } if id == &alpha_id)),
        "jumping to a hidden idle session should reveal its project group"
    );
}

#[test]
#[serial]
fn test_strict_mode_ctrl_g_opens_group_picker() {
    // Regression guard: the GroupBy binding is Ctrl+G in strict mode. It must
    // open the group picker, while bare 'g' continues to fall into the
    // typing-guard catch-all (it lands in pending_paste).
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_with_sessions(3);
    env.view.strict_hotkeys = true;
    env.view.group_by = GroupByMode::Manual;

    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.group_picker_dialog.is_some(),
        "Ctrl+G in strict mode should open the group picker"
    );
    assert!(
        env.view.pending_paste.is_none(),
        "Ctrl+G must not leak into the typing-guard catch-all"
    );
    // Down + Enter switches to Project.
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.group_by, GroupByMode::Project);

    env.view.handle_key(key(KeyCode::Char('g')), None);
    assert!(
        env.view.group_picker_dialog.is_none(),
        "bare 'g' in strict mode must NOT open the group picker (typing-guard contract)"
    );
    assert_eq!(
        env.view.group_by,
        GroupByMode::Project,
        "bare 'g' in strict mode must NOT change group-by (typing-guard contract)"
    );
    assert_eq!(
        env.view.pending_paste.as_deref(),
        Some("g"),
        "bare 'g' in strict mode falls through to the typing-guard catch-all"
    );
}

#[test]
#[serial]
fn test_strict_mode_ctrl_t_and_ctrl_n_reach_secondary_actions() {
    // Regression guard (2026-05-29): in strict_hotkeys mode, normalize_strict_key
    // used to fold Ctrl+T -> 'T' and Ctrl+N -> 'N' (modifier stripped), which
    // collided with the Shift+T / Shift+N primary arms (toggle view, plain new
    // session) and left the Ctrl+T / Ctrl+N secondary arms (quick-attach
    // terminal, new-from-selection) as unreachable dead code. Both chords must
    // keep CTRL so the secondary arms fire.
    let mut env = create_test_env_with_sessions(1);
    env.view.strict_hotkeys = true;
    env.view.cursor = 0;
    env.view.update_selected();

    // Shift+T toggles the view (primary action), no terminal attach.
    assert_eq!(env.view.view_mode, ViewMode::Structured);
    let shift_t = env
        .view
        .handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT), None);
    assert_eq!(env.view.view_mode, ViewMode::Terminal);
    assert!(
        !matches!(shift_t, Some(Action::AttachTerminal(_, _))),
        "Shift+T must toggle view, not attach terminal"
    );
    // Reset to Structured view.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT), None);
    assert_eq!(env.view.view_mode, ViewMode::Structured);

    // Ctrl+T quick-attaches the paired terminal (secondary action) and must
    // NOT toggle the view.
    let ctrl_t = env.view.handle_key(
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        matches!(ctrl_t, Some(Action::AttachTerminal(_, _))),
        "Ctrl+T in strict mode must quick-attach the paired terminal"
    );
    assert_eq!(
        env.view.view_mode,
        ViewMode::Structured,
        "Ctrl+T must not toggle the view"
    );

    // Shift+N opens the plain new-session dialog (no prefill from selection).
    assert!(env.view.new_dialog.is_none());
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT), None);
    assert!(
        env.view.new_dialog.is_some(),
        "Shift+N must open the new-session dialog"
    );
    env.view.new_dialog = None;

    // Ctrl+N opens the new-from-selection dialog (secondary action). It also
    // routes through open_new_session_dialog, so assert it reaches the arm by
    // confirming the dialog opens with CTRL intact rather than being swallowed.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.new_dialog.is_some(),
        "Ctrl+N in strict mode must open the new-from-selection dialog"
    );
}

#[test]
#[serial]
fn test_strict_mode_ctrl_d_r_p_reach_secondary_actions() {
    // Regression guard (2026-05-29): normalize_strict_key used to fold
    // Ctrl+D/Ctrl+R/Ctrl+P to bare 'D'/'R'/'P', which collided with the
    // Shift+letter primary arms. In strict mode Shift+D=delete, Shift+R=rename,
    // Shift+P=profiles, so the folds made Ctrl+D fire delete (not diff), Ctrl+R
    // fire rename (not serve), and orphaned the diff/serve/projects arms. All
    // three Ctrl chords must keep CTRL so their secondary arms fire.
    let mut env = create_test_env_with_sessions(1);
    disable_delete_to_trash();
    env.view.strict_hotkeys = true;
    env.view.cursor = 0;
    env.view.update_selected();

    // Shift+D opens the delete confirmation (primary uppercase action).
    assert!(env.view.unified_delete_dialog.is_none());
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT), None);
    assert!(
        env.view.unified_delete_dialog.is_some(),
        "Shift+D must open the delete dialog"
    );
    env.view.unified_delete_dialog = None;

    // Ctrl+D routes to the diff arm, NOT delete. The test session's path is not
    // a real git worktree so the diff view may fail to open (info dialog) or
    // open empty; either way the regression is that Ctrl+D must never reach
    // open_delete_for_selected. Clear any takeover the diff arm leaves behind so
    // it doesn't swallow the next keypress.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.unified_delete_dialog.is_none(),
        "Ctrl+D in strict mode must NOT open the delete dialog (it targets diff)"
    );
    env.view.diff_view = None;
    env.view.info_dialog = None;

    // Shift+R opens the rename dialog (primary uppercase action).
    assert!(env.view.rename_dialog.is_none());
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT), None);
    assert!(
        env.view.rename_dialog.is_some(),
        "Shift+R must open the rename dialog"
    );
    env.view.rename_dialog = None;

    // Ctrl+R routes to the serve arm, NOT rename.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.rename_dialog.is_none(),
        "Ctrl+R in strict mode must NOT open the rename dialog (it targets serve)"
    );
    env.view.info_dialog = None;
    #[cfg(feature = "serve")]
    {
        env.view.serve_view = None;
    }

    // P follows the same relocation rule as D/R/T/N: the bare-`p` (primary)
    // action -> Shift+P, the Shift+P (secondary) action -> Ctrl+P. So in strict
    // mode Shift+P opens projects and Ctrl+P opens profiles.
    assert!(env.view.projects_dialog.is_none());
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT), None);
    assert!(
        env.view.projects_dialog.is_some(),
        "Shift+P in strict mode must open the projects dialog"
    );
    assert!(
        env.view.profile_picker_dialog.is_none(),
        "Shift+P must not open the profile picker"
    );
    env.view.projects_dialog = None;

    // Ctrl+P opens the profile picker, NOT projects.
    assert!(env.view.profile_picker_dialog.is_none());
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.profile_picker_dialog.is_some(),
        "Ctrl+P in strict mode must open the profile picker"
    );
    assert!(
        env.view.projects_dialog.is_none(),
        "Ctrl+P must not open the projects dialog"
    );
}

#[test]
#[serial]
fn test_command_palette_diff_invokes_diff_in_strict_mode() {
    // Regression guard for the palette half of the strict-mode bug: the palette
    // used to synthesize a keypress, so picking "Open diff view" in strict mode
    // routed through Shift+D and fired DELETE instead. Palette entries now carry
    // an ActionId and run the action directly, so the mode can't matter.
    let mut env = create_test_env_with_sessions(1);
    env.view.strict_hotkeys = true;
    env.view.cursor = 0;
    env.view.update_selected();

    // Open the palette and filter to the diff command.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.command_palette.is_some(),
        "Ctrl+K opens the palette"
    );
    for ch in "diff view".chars() {
        env.view.handle_key(key(KeyCode::Char(ch)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);

    // The diff action ran (opened the diff view, or raised an info dialog if the
    // temp path isn't a real git repo). Crucially, it did NOT delete.
    assert!(
        env.view.unified_delete_dialog.is_none(),
        "palette 'diff' in strict mode must not open the delete dialog"
    );
    assert!(
        env.view.diff_view.is_some() || env.view.info_dialog.is_some(),
        "palette 'diff' in strict mode must attempt to open the diff view"
    );
}

#[test]
#[serial]
fn test_f5_and_e_both_open_restart_dialog() {
    // Pin the equivalence: F5 and `e`/`E` all open the restart dialog. The
    // help overlay collapses them onto one row as "Restart session (also
    // F5)", which is only honest if both bindings keep hitting the same
    // dispatch (open_restart_dialog).
    let mut env = create_test_env_with_sessions(1);
    env.view.cursor = 0;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::F(5)), None);
    let f5_opened = env.view.restart_dialog.is_some();
    env.view.restart_dialog = None;

    env.view.strict_hotkeys = false;
    env.view.handle_key(key(KeyCode::Char('e')), None);
    let lower_e_opened = env.view.restart_dialog.is_some();
    env.view.restart_dialog = None;

    env.view.strict_hotkeys = true;
    env.view.handle_key(key(KeyCode::Char('E')), None);
    let upper_e_opened = env.view.restart_dialog.is_some();

    assert!(f5_opened, "F5 should open the restart dialog");
    assert!(
        lower_e_opened,
        "non-strict 'e' should open the restart dialog"
    );
    assert!(upper_e_opened, "strict 'E' should open the restart dialog");
}

#[test]
#[serial]
fn test_ctrl_o_key_opens_sort_picker() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // Ctrl+O opens the same modal picker. Pressing it on its own does not
    // change the current sort.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        None,
    );
    assert!(env.view.sort_picker_dialog.is_some());
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.sort_picker_dialog.is_none());
    assert_eq!(env.view.sort_order, SortOrder::Newest);
}

#[test]
#[serial]
fn test_o_key_flat_items_sorted_az() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // Open the sort picker and pick AZ.
    env.view.handle_key(key(KeyCode::Char('o')), None);
    for _ in 0..4 {
        env.view.handle_key(key(KeyCode::Down), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.sort_order, SortOrder::AZ);

    let mut session_titles: Vec<_> = Vec::new();
    let mut in_work_group = false;
    for item in &env.view.flat_items {
        match item {
            Item::Group { name, .. } => {
                in_work_group = name == "work";
            }
            Item::Session { id, .. } => {
                if in_work_group {
                    if let Some(inst) = env.view.get_instance(id) {
                        session_titles.push(inst.title.as_str());
                    }
                }
            }
        }
    }

    assert_eq!(session_titles, vec!["Apple", "Mango", "Zebra"]);
}

#[test]
#[serial]
fn test_o_key_flat_items_sorted_za() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();

    // Open the sort picker and pick ZA (5 entries down from Newest).
    env.view.handle_key(key(KeyCode::Char('o')), None);
    for _ in 0..5 {
        env.view.handle_key(key(KeyCode::Down), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.sort_order, SortOrder::ZA);

    let mut session_titles: Vec<_> = Vec::new();
    let mut in_work_group = false;
    for item in &env.view.flat_items {
        match item {
            Item::Group { name, .. } => {
                in_work_group = name == "work";
            }
            Item::Session { id, .. } => {
                if in_work_group {
                    if let Some(inst) = env.view.get_instance(id) {
                        session_titles.push(inst.title.as_str());
                    }
                }
            }
        }
    }

    assert_eq!(session_titles, vec!["Zebra", "Mango", "Apple"]);
}

#[test]
#[serial]
fn test_o_key_flat_items_newest_preserves_insertion_order() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();

    // Press 'o' six times to wrap back to Newest
    // (Newest -> Attention -> LastActivity -> Oldest -> AZ -> ZA -> Newest)
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    let mut session_titles: Vec<_> = Vec::new();
    let mut in_work_group = false;
    for item in &env.view.flat_items {
        match item {
            Item::Group { name, .. } => {
                in_work_group = name == "work";
            }
            Item::Session { id, .. } => {
                if in_work_group {
                    if let Some(inst) = env.view.get_instance(id) {
                        session_titles.push(inst.title.as_str());
                    }
                }
            }
        }
    }

    assert_eq!(session_titles, vec!["Apple", "Mango", "Zebra"]);
}

#[test]
#[serial]
fn test_o_key_clamps_cursor_when_list_shrinks() {
    use crate::session::config::SortOrder;
    use tui_input::Input;

    let mut env = create_test_env_with_mixed_sessions();
    let initial_items = env.view.flat_items.len();

    env.view.cursor = initial_items - 1;
    assert_eq!(env.view.cursor, initial_items - 1);

    // Set up a search query but don't activate search mode
    // (simulates having just exited search mode with matches)
    env.view.search_query = Input::new("work".to_string());
    env.view.update_search();
    let filtered_count = env.view.search_matches.len();
    assert!(filtered_count < initial_items);

    // Open the sort picker and pick Attention (one entry down from Newest).
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.sort_order, SortOrder::Attention);

    let valid_max = env.view.flat_items.len().saturating_sub(1);
    assert!(env.view.cursor <= valid_max);
}

#[test]
#[serial]
fn test_all_profiles_view_loads_from_multiple_profiles() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("Alpha Session", "/tmp/a")];
        storage_a
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let storage_b = Storage::new_unwatched("beta").unwrap();
    {
        let xs = vec![Instance::new("Beta Session", "/tmp/b")];
        storage_b
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    assert_eq!(view.instances().len(), 2);
    let profiles: Vec<&str> = view
        .instances()
        .map(|i| i.source_profile.as_str())
        .collect();
    assert!(profiles.contains(&"alpha"));
    assert!(profiles.contains(&"beta"));
}

#[test]
#[serial]
fn test_filtered_view_loads_single_profile() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("Alpha Session", "/tmp/a")];
        storage_a
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let storage_b = Storage::new_unwatched("beta").unwrap();
    {
        let xs = vec![Instance::new("Beta Session", "/tmp/b")];
        storage_b
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("alpha".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    assert_eq!(view.instances().len(), 1);
    assert_eq!(view.instance_at(0).title, "Alpha Session");
    assert_eq!(view.instance_at(0).source_profile, "alpha");
}

#[test]
#[serial]
fn test_all_profiles_view_has_no_profile_headers() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("A1", "/tmp/a")];
        storage_a
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let storage_b = Storage::new_unwatched("beta").unwrap();
    {
        let xs = vec![Instance::new("B1", "/tmp/b")];
        storage_b
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // All items should be sessions (no profile headers)
    let session_count = view
        .flat_items
        .iter()
        .filter(|i| matches!(i, Item::Session { .. }))
        .count();
    assert_eq!(session_count, 2);
    assert_eq!(view.flat_items.len(), 2);
}

#[test]
#[serial]
fn test_all_profiles_view_shows_all_sessions_flat() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("A1", "/tmp/a")];
        storage_a
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let storage_b = Storage::new_unwatched("beta").unwrap();
    {
        let xs = vec![Instance::new("B1", "/tmp/b")];
        storage_b
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // All sessions from all profiles should be visible at depth 0
    for item in &view.flat_items {
        if let Item::Session { depth, .. } = item {
            assert_eq!(*depth, 0, "sessions in all view should be at depth 0");
        }
    }
}

/// Flatten a rendered row into its plain text, dropping styling.
fn rendered_row_text(view: &HomeView, item: &Item) -> String {
    use crate::tui::styles::Theme;
    let theme = Theme::default();
    view.render_item_line(item, false, false, &theme, 200)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

fn rendered_single_session_text(
    inst: Instance,
    row_tag_mode: crate::session::config::RowTagMode,
) -> String {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage = Storage::new_unwatched("alpha").unwrap();
    let instances = vec![inst];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = row_tag_mode;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    view.flat_items
        .iter()
        .find_map(|item| {
            if let Item::Session { .. } = item {
                Some(rendered_row_text(&view, item))
            } else {
                None
            }
        })
        .expect("session row should render")
}

/// Default `RowTagMode::Branch` keeps worktree branch information visible.
#[test]
#[serial]
fn test_default_row_tag_mode_renders_branch_tag() {
    let mut inst = Instance::new("my-session", "/tmp/a");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/a-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::default());
    assert!(
        text.contains("[foo         ]"),
        "default row tag mode should show the compact branch tag: {text:?}"
    );
}

/// `RowTagMode::Auto` shows the profile short code in all-profiles view.
#[test]
#[serial]
fn test_row_tag_auto_renders_profile_in_all_profiles_view() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let instances_a = vec![Instance::new("A1", "/tmp/a")];
    let group_tree_a = GroupTree::new_with_groups(&instances_a, &[]);
    storage_a
        .update(|i, g| {
            *i = instances_a.to_vec();
            *g = group_tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    let storage_b = Storage::new_unwatched("beta").unwrap();
    let instances_b = vec![Instance::new("B1", "/tmp/b")];
    let group_tree_b = GroupTree::new_with_groups(&instances_b, &[]);
    storage_b
        .update(|i, g| {
            *i = instances_b.to_vec();
            *g = group_tree_b.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Auto;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let mut seen = 0;
    for item in &view.flat_items {
        if let Item::Session { id, .. } = item {
            let profile = view.get_instance(id).unwrap().source_profile.clone();
            let code = super::render::profile_short_code(&profile);
            let rendered = super::render::RowTag {
                content: code.clone(),
                max_width: 4,
            }
            .rendered();
            let text = rendered_row_text(&view, item);
            assert!(
                text.contains(&rendered),
                "all-view row for profile {profile} missing tag {rendered}: {text:?}"
            );
            seen += 1;
        }
    }
    assert_eq!(seen, 2, "expected both profile sessions to render");
}

/// `RowTagMode::Auto` does not render in a filtered view (profile already
/// in the list title).
#[test]
#[serial]
fn test_row_tag_auto_omits_tag_in_filtered_view() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let instances_a = vec![Instance::new("A1", "/tmp/a")];
    let group_tree_a = GroupTree::new_with_groups(&instances_a, &[]);
    storage_a
        .update(|i, g| {
            *i = instances_a.to_vec();
            *g = group_tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("alpha".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Auto;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let code = super::render::profile_short_code("alpha");
    let rendered = super::render::RowTag {
        content: code,
        max_width: 4,
    }
    .rendered();
    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                !text.contains(&rendered),
                "Auto in filtered view should omit the tag: {text:?}"
            );
        }
    }
}

/// `RowTagMode::Profile` renders the profile tag in BOTH views (unlike
/// Auto which gates on all-profiles view).
#[test]
#[serial]
fn test_row_tag_profile_renders_in_filtered_view() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let instances_a = vec![Instance::new("A1", "/tmp/a")];
    let group_tree_a = GroupTree::new_with_groups(&instances_a, &[]);
    storage_a
        .update(|i, g| {
            *i = instances_a.to_vec();
            *g = group_tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("alpha".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Profile;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let code = super::render::profile_short_code("alpha");
    let rendered = super::render::RowTag {
        content: code,
        max_width: 4,
    }
    .rendered();
    let mut seen = 0;
    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                text.contains(&rendered),
                "Profile mode should always render the tag: {text:?}"
            );
            seen += 1;
        }
    }
    assert!(seen > 0);
}

/// `RowTagMode::Branch` owns the branch suffix. It renders a compact tag even
/// when the title differs from the branch, with no raw hardcoded branch suffix.
#[test]
#[serial]
fn test_row_tag_branch_renders_when_branch_differs_from_title() {
    let mut inst = Instance::new("my-session", "/tmp/a");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/a-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::Branch);
    assert!(
        text.contains("[foo         ]"),
        "Branch mode should render the compact branch tag: {text:?}"
    );
    assert!(
        !text.contains("feature/foo"),
        "Branch mode should not render the old raw branch suffix: {text:?}"
    );
}

/// `RowTagMode::Branch` renders the tag when title matches branch.
#[test]
#[serial]
fn test_row_tag_branch_renders_when_title_matches_branch() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage = Storage::new_unwatched("alpha").unwrap();
    // Title and branch MATCH, so the divergence display stays quiet.
    let mut inst = Instance::new("feature/foo", "/tmp/a");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/a-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let instances = vec![inst];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Branch;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // The tag uses the last `/`-segment of the branch and pads to the branch
    // tag width so the row layout stays stable.
    let rendered = super::render::RowTag {
        content: "foo".to_string(),
        max_width: 12,
    }
    .rendered();
    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                text.contains(&rendered),
                "Branch mode must render the tag when divergence display is quiet: {text:?}"
            );
        }
    }
}

#[test]
#[serial]
fn test_row_tag_none_hides_worktree_branch_suffix() {
    let mut inst = Instance::new("my-session", "/tmp/a");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/a-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::None);
    assert!(
        !text.contains("feature/foo") && !text.contains("[foo") && !text.contains('['),
        "None mode should hide all worktree suffix metadata: {text:?}"
    );
}

#[test]
#[serial]
fn test_row_tag_none_hides_workspace_suffix() {
    let mut inst = Instance::new("workspace-session", "/tmp/workspace");
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/foo".to_string(),
        workspace_dir: "/tmp/workspace".to_string(),
        repos: vec![
            crate::session::WorkspaceRepo {
                name: "api".to_string(),
                source_path: "/src/api".to_string(),
                branch: "feature/foo".to_string(),
                worktree_path: "/tmp/workspace/api".to_string(),
                main_repo_path: "/src/api".to_string(),
                managed_by_aoe: true,
            },
            crate::session::WorkspaceRepo {
                name: "web".to_string(),
                source_path: "/src/web".to_string(),
                branch: "feature/foo".to_string(),
                worktree_path: "/tmp/workspace/web".to_string(),
                main_repo_path: "/src/web".to_string(),
                managed_by_aoe: true,
            },
        ],
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::None);
    assert!(
        !text.contains("feature/foo") && !text.contains("repos") && !text.contains('['),
        "None mode should hide all workspace suffix metadata: {text:?}"
    );
}

#[test]
#[serial]
fn test_row_tag_branch_renders_workspace_branch_repo_count() {
    let mut inst = Instance::new("workspace-session", "/tmp/workspace");
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/foo".to_string(),
        workspace_dir: "/tmp/workspace".to_string(),
        repos: vec![
            crate::session::WorkspaceRepo {
                name: "api".to_string(),
                source_path: "/src/api".to_string(),
                branch: "feature/foo".to_string(),
                worktree_path: "/tmp/workspace/api".to_string(),
                main_repo_path: "/src/api".to_string(),
                managed_by_aoe: true,
            },
            crate::session::WorkspaceRepo {
                name: "web".to_string(),
                source_path: "/src/web".to_string(),
                branch: "feature/foo".to_string(),
                worktree_path: "/tmp/workspace/web".to_string(),
                main_repo_path: "/src/web".to_string(),
                managed_by_aoe: true,
            },
        ],
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::Branch);
    assert!(
        text.contains("[foo+2       ]"),
        "Branch mode should render compact workspace branch and repo count: {text:?}"
    );
}

/// Legacy `Instance::new` left `source_profile` empty before the per-profile
/// plumbing landed. The render branch must skip the tag entirely in that
/// case rather than emit a literal `  []`.
#[test]
#[serial]
fn test_row_tag_auto_skips_for_empty_source_profile() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage = Storage::new_unwatched("legacy").unwrap();
    let mut inst = Instance::new("Legacy1", "/tmp/legacy");
    inst.source_profile = String::new();
    let instances = vec![inst];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Auto;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                !text.contains("[]"),
                "row with empty source_profile must not render a literal []: {text:?}"
            );
        }
    }
}

#[test]
#[serial]
fn test_create_session_in_all_mode_is_findable() {
    use crate::tui::dialogs::NewSessionData;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Create a profile so "all" mode has something
    let storage = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("Existing", "/tmp/a")];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let project_dir = temp.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let data = NewSessionData {
        profile: "alpha".to_string(),
        title: "New Session".to_string(),
        path: project_dir.to_str().unwrap().to_string(),
        group: String::new(),
        tool: "claude".to_string(),
        worktree_enabled: false,
        worktree_branch: None,
        create_new_branch: false,
        base_branch: None,
        extra_repo_paths: Vec::new(),
        sandbox: false,
        sandbox_image: String::new(),
        yolo_mode: false,
        extra_env: Vec::new(),
        extra_args: String::new(),
        command_override: String::new(),
        scratch: false,
        fork_seed: None,
        structured: false,
    };

    let session_id = view.create_session(data).unwrap();

    // In unified view, the session IS findable (fixes #419)
    assert!(
        view.get_instance(&session_id).is_some(),
        "session created in all-mode should be findable by get_instance"
    );
    assert_eq!(
        view.get_instance(&session_id).unwrap().source_profile,
        "alpha"
    );
}

#[test]
#[serial]
fn test_save_preserves_per_profile_collapsed_state() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Create alpha with group "work" (collapsed)
    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let mut inst_a = Instance::new("A1", "/tmp/a");
    inst_a.group_path = "work".to_string();
    let mut tree_a = GroupTree::new_with_groups(&[inst_a.clone()], &[]);
    tree_a.toggle_collapsed("work");
    storage_a
        .update(|i, g| {
            *i = [inst_a].to_vec();
            *g = tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    // Create beta with group "work" (expanded, the default)
    let storage_b = Storage::new_unwatched("beta").unwrap();
    let mut inst_b = Instance::new("B1", "/tmp/b");
    inst_b.group_path = "work".to_string();
    let tree_b = GroupTree::new_with_groups(&[inst_b.clone()], &[]);
    storage_b
        .update(|i, g| {
            *i = [inst_b].to_vec();
            *g = tree_b.get_all_groups();
            Ok(())
        })
        .unwrap();

    // Load unified view
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Verify per-profile collapsed state is preserved
    let alpha_tree = view.group_trees.get("alpha").unwrap();
    let alpha_work = alpha_tree
        .get_all_groups()
        .into_iter()
        .find(|g| g.path == "work")
        .expect("alpha should have work group");
    assert!(
        alpha_work.collapsed,
        "alpha's 'work' group should be collapsed"
    );

    let beta_tree = view.group_trees.get("beta").unwrap();
    let beta_work = beta_tree
        .get_all_groups()
        .into_iter()
        .find(|g| g.path == "work")
        .expect("beta should have work group");
    assert!(
        !beta_work.collapsed,
        "beta's 'work' group should be expanded"
    );

    // Save and reload to verify persistence
    view.save().unwrap();

    // Reload from disk and verify alpha's collapsed state survived
    let (_, groups_a) = storage_a.load_with_groups().unwrap();
    let saved_a = groups_a
        .iter()
        .find(|g| g.path == "work")
        .expect("alpha should still have work group on disk");
    assert!(
        saved_a.collapsed,
        "alpha's 'work' collapsed state should persist to disk"
    );

    let (_, groups_b) = storage_b.load_with_groups().unwrap();
    let saved_b = groups_b
        .iter()
        .find(|g| g.path == "work")
        .expect("beta should still have work group on disk");
    assert!(
        !saved_b.collapsed,
        "beta's 'work' expanded state should persist to disk"
    );
}

#[test]
#[serial]
fn test_create_profile_rejects_reserved_name_all() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("default").unwrap();

    let result = crate::session::create_profile("all");
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("reserved"),
        "error should mention 'reserved'"
    );

    // Case-insensitive
    let result = crate::session::create_profile("ALL");
    assert!(result.is_err());
}

#[test]
#[serial]
fn test_delete_group_scoped_to_owning_profile() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Create alpha with group "work"
    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let mut inst_a = Instance::new("A1", "/tmp/a");
    inst_a.group_path = "work".to_string();
    let tree_a = GroupTree::new_with_groups(&[inst_a.clone()], &[]);
    storage_a
        .update(|i, g| {
            *i = [inst_a].to_vec();
            *g = tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    // Create beta with the same group name "work"
    let storage_b = Storage::new_unwatched("beta").unwrap();
    let mut inst_b = Instance::new("B1", "/tmp/b");
    inst_b.group_path = "work".to_string();
    let tree_b = GroupTree::new_with_groups(&[inst_b.clone()], &[]);
    storage_b
        .update(|i, g| {
            *i = [inst_b].to_vec();
            *g = tree_b.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Both profiles should have a "work" group
    assert!(view.group_trees.get("alpha").unwrap().group_exists("work"));
    assert!(view.group_trees.get("beta").unwrap().group_exists("work"));

    // Find a "work" group item that belongs to alpha and select it.
    // Collect candidate indices first to avoid borrow conflicts.
    let work_indices: Vec<usize> = view
        .flat_items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| match item {
            Item::Group { path, .. } if path == "work" => Some(idx),
            _ => None,
        })
        .collect();

    for idx in work_indices {
        view.cursor = idx;
        view.update_selected();
        if view.selected_group_profile.as_deref() == Some("alpha") {
            break;
        }
    }

    assert_eq!(view.selected_group.as_deref(), Some("work"));
    assert_eq!(view.selected_group_profile.as_deref(), Some("alpha"));

    // Delete alpha's "work" group
    view.delete_selected_group().unwrap();

    // Alpha's "work" group should be gone, but beta's should remain
    assert!(
        !view.group_trees.get("alpha").unwrap().group_exists("work"),
        "alpha's 'work' group should be deleted"
    );
    assert!(
        view.group_trees.get("beta").unwrap().group_exists("work"),
        "beta's 'work' group should be untouched"
    );

    // Alpha's instance should be ungrouped, beta's should still be in "work"
    let alpha_inst = view
        .instances()
        .find(|i| i.source_profile == "alpha")
        .unwrap();
    assert_eq!(
        alpha_inst.group_path, "",
        "alpha's instance should be ungrouped"
    );
    let beta_inst = view
        .instances()
        .find(|i| i.source_profile == "beta")
        .unwrap();
    assert_eq!(
        beta_inst.group_path, "work",
        "beta's instance should still be in 'work'"
    );
}

/// Opening the group-delete dialog must scope its session count to the
/// selected group's profile. Two profiles can own a same-named group; an
/// empty group in one profile should open the simple confirm, not the
/// "delete N sessions" options dialog driven by its populated twin in
/// another profile. Regression for the group-key conflict where the empty
/// group was not the one the delete modal acted on.
#[test]
#[serial]
fn test_group_delete_dialog_scoped_to_owning_profile() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // alpha owns an EMPTY "work" group (group exists, no sessions).
    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let mut tree_a = GroupTree::new_with_groups(&[], &[]);
    tree_a.create_group("work");
    storage_a
        .update(|i, g| {
            *i = vec![];
            *g = tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    // beta owns a same-named "work" group that still has a session.
    let storage_b = Storage::new_unwatched("beta").unwrap();
    let mut inst_b = Instance::new("B1", "/tmp/b");
    inst_b.group_path = "work".to_string();
    let tree_b = GroupTree::new_with_groups(&[inst_b.clone()], &[]);
    storage_b
        .update(|i, g| {
            *i = [inst_b].to_vec();
            *g = tree_b.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Select alpha's (empty) "work" group.
    let work_indices: Vec<usize> = view
        .flat_items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| match item {
            Item::Group { path, .. } if path == "work" => Some(idx),
            _ => None,
        })
        .collect();
    for idx in work_indices {
        view.cursor = idx;
        view.update_selected();
        if view.selected_group_profile.as_deref() == Some("alpha") {
            break;
        }
    }
    assert_eq!(view.selected_group_profile.as_deref(), Some("alpha"));

    view.open_delete_for_selected();

    assert!(
        view.group_delete_options_dialog.is_none(),
        "empty group must not trigger the with-sessions options dialog from a same-named group in another profile"
    );
    assert!(
        view.confirm_dialog.is_some(),
        "empty group should open the simple delete-group confirm"
    );
}

/// Changing a session's profile via the rename dialog must prune the
/// source profile's now-empty group, just like the restart-with-edits
/// path does. Without the prune the source keeps an empty group with the
/// same name as the target's copy, which renders as a duplicate header and
/// collides on the shared group key.
#[test]
#[serial]
fn test_rename_profile_change_prunes_source_group() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // alpha has one session in "work"; beta exists but is empty.
    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let mut inst_a = Instance::new("A1", "/tmp/a");
    inst_a.group_path = "work".to_string();
    let id = inst_a.id.clone();
    let tree_a = GroupTree::new_with_groups(&[inst_a.clone()], &[]);
    storage_a
        .update(|i, g| {
            *i = [inst_a].to_vec();
            *g = tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();
    let _storage_b = Storage::new_unwatched("beta").unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.selected_session = Some(id.clone());

    // Move the session alpha -> beta, keeping the same group name.
    view.rename_selected("", None, Some("beta"), false).unwrap();

    let moved = view.get_instance(&id).unwrap();
    assert_eq!(moved.source_profile, "beta");
    assert_eq!(moved.group_path, "work");
    assert!(
        view.group_trees.get("beta").unwrap().group_exists("work"),
        "beta should own the 'work' group after the move"
    );
    assert!(
        !view
            .group_trees
            .get("alpha")
            .map(|t| t.group_exists("work"))
            .unwrap_or(false),
        "alpha's now-empty 'work' group should be pruned after the profile move"
    );
}

#[test]
#[serial]
fn test_shift_n_opens_prefilled_dialog_from_session() {
    let mut env = create_test_env_with_groups();
    assert!(env.view.new_dialog.is_none());

    // Move cursor to the "work-project" session (grouped under "work")
    // flat_items: [Group("personal"), Session("personal-project"), Group("work"), Session("work-project"), Session("ungrouped")]
    let work_session_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { id, .. } if env.view.get_instance(id).map(|i| i.title.as_str()) == Some("work-project")))
        .expect("work-project session should exist in flat_items");
    env.view.cursor = work_session_idx;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('N')), None);
    let dialog = env.view.new_dialog.as_ref().expect("N should open dialog");
    assert_eq!(dialog.path_value(), "/tmp/work");
    assert_eq!(dialog.group_value(), "work");
}

#[test]
#[serial]
fn test_shift_n_opens_prefilled_dialog_from_group() {
    let mut env = create_test_env_with_groups();

    // Move cursor to a group row
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if path == "work"))
        .expect("work group should exist in flat_items");
    env.view.cursor = group_idx;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('N')), None);
    let dialog = env.view.new_dialog.as_ref().expect("N should open dialog");
    assert_eq!(dialog.group_value(), "work");
    // The group has a member at "/tmp/work", so the path is borrowed from it
    // instead of being left on the default cwd (issue #2023).
    assert_eq!(dialog.path_value(), "/tmp/work");
}

#[test]
#[serial]
fn test_group_context_menu_new_session_prefills_path() {
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();

    // Move cursor to the "work" group row, as a right-click would.
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if path == "work"))
        .expect("work group should exist in flat_items");
    env.view.cursor = group_idx;
    env.view.update_selected();

    // The group right-click menu's "New Session" routes here.
    env.view
        .dispatch_context_menu_action(ContextMenuAction::NewFromSelection);
    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("NewFromSelection should open the new-session dialog");
    assert_eq!(dialog.path_value(), "/tmp/work");
    assert_eq!(dialog.group_value(), "work");
}

#[test]
#[serial]
fn test_group_context_menu_new_session_shows_no_agents_without_tools() {
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();
    env.view.available_tools = AvailableTools::with_tools(&[]);

    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if path == "work"))
        .expect("work group should exist in flat_items");
    env.view.cursor = group_idx;
    env.view.update_selected();

    env.view
        .dispatch_context_menu_action(ContextMenuAction::NewFromSelection);
    assert!(
        env.view.new_dialog.is_none(),
        "no agents means the new-session form must not open"
    );
    assert!(
        env.view.no_agents_dialog.is_some(),
        "should point the user at agent setup instead, like 'n'"
    );
}

#[test]
#[serial]
fn test_group_context_menu_new_session_prefills_path_in_project_mode() {
    use crate::session::config::GroupByMode;
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    // In project mode the group label is the repo basename ("work" from
    // "/tmp/work"), not the stored group_path.
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { name, .. } if name == "work"))
        .expect("work project group should exist in flat_items");
    env.view.cursor = group_idx;
    env.view.update_selected();

    env.view
        .dispatch_context_menu_action(ContextMenuAction::NewFromSelection);
    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("NewFromSelection should open the new-session dialog");
    assert_eq!(
        dialog.path_value(),
        "/tmp/work",
        "project-mode prefill should borrow the member repo path"
    );
}

#[test]
#[serial]
fn test_session_context_menu_new_session_prefills_from_session() {
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();

    // Move cursor onto the "work-project" session row, as a right-click would.
    let target_id = env
        .view
        .instances
        .values()
        .find(|i| i.repo_path() == "/tmp/work")
        .map(|i| i.id.clone())
        .expect("work-project instance should exist");
    let session_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { id, .. } if *id == target_id))
        .expect("work-project session row should exist in flat_items");
    env.view.cursor = session_idx;
    env.view.update_selected();

    // The session right-click menu's "New Session" routes here, prefilling the
    // dialog from the right-clicked session's repo path and group (issue #2023).
    env.view
        .dispatch_context_menu_action(ContextMenuAction::NewFromSelection);
    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("NewFromSelection should open the new-session dialog");
    assert_eq!(dialog.path_value(), "/tmp/work");
    assert_eq!(dialog.group_value(), "work");
}

#[test]
#[serial]
fn fork_from_selection_seeds_terminal_fork_and_inherits_parent_context() {
    let mut env = create_test_env_empty();
    let mut inst = Instance::new("parent", "/tmp/repo");
    inst.source_profile = "test".to_string();
    inst.tool = "claude".into();
    inst.agent_session_id = Some("parent-1111-2222-3333-444444444444".into());
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id);

    env.view.open_fork_from_selection();

    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("fork opens the new-session dialog");
    let seed = dialog.fork_seed().cloned().expect("fork seed present");
    match seed {
        crate::session::ForkSeed::Terminal {
            parent_agent_session_id,
            child_session_id,
        } => {
            assert_eq!(
                parent_agent_session_id,
                "parent-1111-2222-3333-444444444444"
            );
            assert_ne!(child_session_id, "parent-1111-2222-3333-444444444444");
            assert!(!child_session_id.is_empty());
        }
        other => panic!("expected Terminal fork seed, got {other:?}"),
    }
    assert_eq!(dialog.path_value(), "/tmp/repo");
}

#[test]
#[serial]
fn fork_denied_for_resume_only_agent_shows_info() {
    let mut env = create_test_env_empty();
    let mut inst = Instance::new("parent", "/tmp/repo");
    inst.source_profile = "test".to_string();
    inst.tool = "gemini".into();
    inst.agent_session_id = Some("parent-uuid".into());
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id);

    env.view.open_fork_from_selection();

    assert!(
        env.view.new_dialog.is_none(),
        "no dialog for an unforkable agent"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "an explanatory info dialog is shown instead"
    );
}

/// The fork seed forks the parent's agent, so the dialog must open preselected
/// on that agent rather than the configured default. A Codex parent forking
/// while the default tool is claude must land on codex, not claude (otherwise
/// the dialog's tool and the seed disagree).
#[test]
#[serial]
fn fork_from_selection_preselects_parent_tool() {
    let mut env = create_test_env_empty();
    env.view
        .set_available_tools(AvailableTools::with_tools(&["claude", "codex"]));
    let mut inst = Instance::new("parent", "/tmp/repo");
    inst.source_profile = "test".to_string();
    inst.tool = "codex".into();
    inst.agent_session_id = Some("parent-1111-2222-3333-444444444444".into());
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id);

    env.view.open_fork_from_selection();

    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("fork opens the new-session dialog");
    assert_eq!(
        dialog.selected_tool(),
        "codex",
        "fork dialog must preselect the parent's agent so it matches the seed"
    );
}

/// A structured (ACP) parent forks via the ACP `session/fork` handshake, so the
/// seed must be `Structured` carrying the parent's captured ACP session id, not
/// a terminal resume-with-fork-flag seed.
#[cfg(feature = "serve")]
#[test]
#[serial]
fn fork_from_selection_structured_parent_seeds_structured_fork() {
    let mut env = create_test_env_empty();
    let mut inst = Instance::new("parent", "/tmp/repo");
    inst.source_profile = "test".to_string();
    inst.tool = "claude".into();
    inst.view = crate::session::View::Structured;
    inst.acp_session_id = Some("acp-parent-9999".into());
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id);

    env.view.open_fork_from_selection();

    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("fork opens the new-session dialog for a structured parent");
    let seed = dialog.fork_seed().cloned().expect("fork seed present");
    assert_eq!(
        seed,
        crate::session::ForkSeed::Structured {
            parent_acp_session_id: "acp-parent-9999".into(),
        },
        "a structured parent must seed a structured fork from its ACP session id"
    );
}

/// A structured parent with no captured ACP session id yet has no conversation
/// to fork; the dialog must not open and an explanatory info dialog is shown.
#[cfg(feature = "serve")]
#[test]
#[serial]
fn fork_from_selection_structured_parent_without_acp_id_denies() {
    let mut env = create_test_env_empty();
    let mut inst = Instance::new("parent", "/tmp/repo");
    inst.source_profile = "test".to_string();
    inst.tool = "claude".into();
    inst.view = crate::session::View::Structured;
    inst.acp_session_id = None;
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id);

    env.view.open_fork_from_selection();

    assert!(
        env.view.new_dialog.is_none(),
        "no dialog for a structured parent with no captured ACP session"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "an explanatory info dialog is shown instead"
    );
}

/// A structured parent whose agent is resume-only (aoe-agent: ACP-capable but
/// no fork strategy) must be refused at the capability gate, BEFORE the
/// captured-conversation check, even when it has an acp_session_id. Otherwise
/// the fork would silently downgrade to session/new at the handshake. This is
/// the exact silent-downgrade the reviewer flagged; the gate mirrors the REST
/// create guard and the web `acp_can_fork` projection.
#[cfg(feature = "serve")]
#[test]
#[serial]
fn fork_from_selection_structured_unforkable_agent_denies() {
    let mut env = create_test_env_empty();
    let mut inst = Instance::new("parent", "/tmp/repo");
    inst.source_profile = "test".to_string();
    inst.tool = "aoe-agent".into();
    inst.view = crate::session::View::Structured;
    // A captured conversation IS present, so only the capability gate can
    // refuse (proving the gate runs before the acp-id check).
    inst.acp_session_id = Some("acp-parent-1234".into());
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id);

    env.view.open_fork_from_selection();

    assert!(
        env.view.new_dialog.is_none(),
        "no dialog for a structured parent whose agent cannot fork"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "an explanatory 'Fork not supported' info dialog is shown instead"
    );
}

#[test]
#[serial]
fn test_session_context_menu_snooze_opens_duration_dialog() {
    use crate::session::config::SortOrder;
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();
    // Snooze is offered in Attention sort (it mirrors the Attention-gated `h`
    // keybinding); dispatching it on an active session opens the duration
    // picker, the same path the keyboard takes.
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    let session_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { .. }))
        .expect("setup should produce a session");
    env.view.cursor = session_idx;
    env.view.update_selected();

    env.view
        .dispatch_context_menu_action(ContextMenuAction::ToggleSnooze);
    assert!(
        env.view.snooze_duration_dialog.is_some(),
        "context-menu Snooze on an active session must open the duration picker"
    );
}

#[test]
#[serial]
fn test_session_context_menu_snooze_wakes_snoozed_session() {
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();
    let session_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { .. }))
        .expect("setup should produce a session");
    env.view.cursor = session_idx;
    env.view.update_selected();
    let id = env
        .view
        .selected_session
        .clone()
        .expect("a session should be selected");

    // Pre-snooze the session so the toggle takes the wake path.
    env.view.snooze_session_for(&id, 60).unwrap();
    assert!(
        env.view.instances.get(&id).is_some_and(|i| i.is_snoozed()),
        "session should be snoozed before the toggle"
    );

    env.view
        .dispatch_context_menu_action(ContextMenuAction::ToggleSnooze);
    assert!(
        env.view.snooze_duration_dialog.is_none(),
        "waking a snoozed session must not open the duration picker"
    );
    assert!(
        !env.view.instances.get(&id).is_some_and(|i| i.is_snoozed()),
        "context-menu Snooze on a snoozed session must wake it immediately"
    );
}

#[test]
#[serial]
fn test_shift_n_does_nothing_with_no_selection() {
    let mut env = create_test_env_empty();
    env.view.handle_key(key(KeyCode::Char('N')), None);
    assert!(
        env.view.new_dialog.is_none(),
        "N should not open dialog when nothing is selected"
    );
}

#[test]
#[serial]
fn test_shift_n_prefills_main_repo_path_for_worktree_session() {
    use crate::session::WorktreeInfo;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst = Instance::new("worktree-session", "/tmp/repo-worktrees/feature-branch");
    inst.worktree_info = Some(WorktreeInfo {
        branch: "feature-branch".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    {
        let xs: Vec<Instance> = vec![inst];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();
    view.cursor = 0;
    view.update_selected();

    view.handle_key(key(KeyCode::Char('N')), None);
    let dialog = view.new_dialog.as_ref().expect("N should open dialog");
    assert_eq!(
        dialog.path_value(),
        "/tmp/repo",
        "Should pre-fill main_repo_path, not worktree path"
    );
}

#[test]
#[serial]
fn test_shift_n_prefills_session_path_for_ungrouped() {
    let mut env = create_test_env_with_groups();

    // Move cursor to the ungrouped session
    let ungrouped_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { id, .. } if env.view.get_instance(id).map(|i| i.title.as_str()) == Some("ungrouped")))
        .expect("ungrouped session should exist");
    env.view.cursor = ungrouped_idx;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('N')), None);
    let dialog = env.view.new_dialog.as_ref().expect("N should open dialog");
    assert_eq!(dialog.path_value(), "/tmp/u");
    assert_eq!(
        dialog.group_value(),
        "",
        "ungrouped session should not pre-fill group"
    );
}

#[test]
fn effective_list_width_clamps_on_small_screens() {
    // The formula: list_width.min(available.saturating_sub(40)).max(10)
    let clamp = |list_width: u16, available: u16| -> u16 {
        list_width.min(available.saturating_sub(40)).max(10)
    };

    // Normal screen (120 cols): list_width 35 fits fine
    assert_eq!(clamp(35, 120), 35);

    // Medium screen (80 cols): list_width 35 still fits (80-40=40 > 35)
    assert_eq!(clamp(35, 80), 35);

    // Small screen (60 cols): list capped to 20, leaving 40 for preview
    assert_eq!(clamp(35, 60), 20);

    // Very small screen (50 cols): list capped to 10 (minimum)
    assert_eq!(clamp(35, 50), 10);

    // Tiny screen (30 cols): list stays at minimum 10
    assert_eq!(clamp(35, 30), 10);

    // User-resized list to 50 on a 100-col screen: capped to 60, but 50 < 60
    assert_eq!(clamp(50, 100), 50);

    // User-resized list to 50 on a 70-col screen: capped to 30, but min 10
    assert_eq!(clamp(50, 70), 30);
}

#[test]
#[serial]
fn test_rename_selected_group_path() {
    let mut env = create_test_env_with_groups();

    // Set up rename context for the "work" group
    env.view.group_rename_context = Some(super::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "test".to_string(),
    });

    // Rename "work" -> "projects"
    env.view
        .rename_selected_group(Some("projects"), None)
        .unwrap();

    // Verify the session's group_path was updated
    let work_session = env
        .view
        .instances()
        .find(|i| i.title == "work-project")
        .unwrap();
    assert_eq!(work_session.group_path, "projects");
}

#[test]
#[serial]
fn test_rename_selected_group_with_children() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("parent-session", "/tmp/p");
    inst1.group_path = "work".to_string();
    let mut inst2 = Instance::new("child-session", "/tmp/c");
    inst2.group_path = "work/frontend".to_string();
    let instances = vec![inst1, inst2];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    view.group_rename_context = Some(super::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "test".to_string(),
    });

    view.rename_selected_group(Some("projects"), None).unwrap();

    let parent = view
        .instances()
        .find(|i| i.title == "parent-session")
        .unwrap();
    assert_eq!(parent.group_path, "projects");

    let child = view
        .instances()
        .find(|i| i.title == "child-session")
        .unwrap();
    assert_eq!(child.group_path, "projects/frontend");

    // Disk-state regression check: the rename must drop both old_path
    // and its descendant rows, leaving only the renamed paths on disk.
    let disk_groups: Vec<String> = storage
        .load_with_groups()
        .unwrap()
        .1
        .into_iter()
        .map(|g| g.path)
        .collect();
    assert!(
        !disk_groups.contains(&"work".to_string()),
        "old parent path must not survive on disk: {:?}",
        disk_groups
    );
    assert!(
        !disk_groups.contains(&"work/frontend".to_string()),
        "old descendant path must not survive on disk: {:?}",
        disk_groups
    );
    assert!(
        disk_groups.contains(&"projects".to_string()),
        "renamed parent must be on disk: {:?}",
        disk_groups
    );
    assert!(
        disk_groups.contains(&"projects/frontend".to_string()),
        "renamed descendant must be on disk: {:?}",
        disk_groups
    );
}

#[test]
#[serial]
fn test_rename_selected_group_noop_when_unchanged() {
    let mut env = create_test_env_with_groups();

    env.view.group_rename_context = Some(super::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "test".to_string(),
    });

    // Same path, no profile change -> noop
    env.view.rename_selected_group(Some("work"), None).unwrap();

    let work_session = env
        .view
        .instances()
        .find(|i| i.title == "work-project")
        .unwrap();
    assert_eq!(work_session.group_path, "work");
}

// --- Additional rename_selected_group operation tests ---

#[test]
#[serial]
fn test_rename_group_removes_old_path() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst = Instance::new("work-session", "/tmp/w");
    inst.group_path = "work".to_string();
    let instances = vec![inst];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    view.group_rename_context = Some(super::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "test".to_string(),
    });

    view.rename_selected_group(Some("projects"), None).unwrap();

    let tree = view.group_trees.get("test").unwrap();
    assert!(!tree.group_exists("work"), "old group path should be gone");
    assert!(tree.group_exists("projects"), "new group path should exist");
}

#[test]
#[serial]
fn test_rename_group_empty_group() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let instances: Vec<Instance> = vec![];
    let mut group_tree = GroupTree::new_with_groups(&instances, &[]);
    group_tree.create_group("empty-group");
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    view.group_rename_context = Some(super::GroupRenameContext {
        old_path: "empty-group".to_string(),
        old_profile: "test".to_string(),
    });

    view.rename_selected_group(Some("renamed-group"), None)
        .unwrap();

    let tree = view.group_trees.get("test").unwrap();
    assert!(
        !tree.group_exists("empty-group"),
        "old empty group path should be gone"
    );
    assert!(
        tree.group_exists("renamed-group"),
        "new group path should exist"
    );
}

#[test]
#[serial]
fn test_rename_group_duplicate_returns_error() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("work-session", "/tmp/w");
    inst1.group_path = "work".to_string();
    let mut inst2 = Instance::new("personal-session", "/tmp/p");
    inst2.group_path = "personal".to_string();
    let instances = vec![inst1, inst2];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    view.group_rename_context = Some(super::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "test".to_string(),
    });

    let result = view.rename_selected_group(Some("personal"), None);
    assert!(result.is_err(), "renaming to an existing group should fail");
}

#[test]
#[serial]
fn test_rename_group_resort_az() {
    use crate::session::config::SortOrder;
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    crate::session::config::update_app_state(|state| {
        state.sort_order = Some(SortOrder::AZ);
    })
    .unwrap();

    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("s1", "/tmp/1");
    inst1.group_path = "zzz".to_string();
    let mut inst2 = Instance::new("s2", "/tmp/2");
    inst2.group_path = "mmm".to_string();
    let instances = vec![inst1, inst2];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    view.group_rename_context = Some(super::GroupRenameContext {
        old_path: "zzz".to_string(),
        old_profile: "test".to_string(),
    });

    view.rename_selected_group(Some("aaa"), None).unwrap();

    let group_items: Vec<&str> = view
        .flat_items
        .iter()
        .filter_map(|item| {
            if let Item::Group { name, .. } = item {
                Some(name.as_str())
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        group_items,
        vec!["aaa", "mmm"],
        "groups should be sorted alphabetically after rename"
    );
}

#[test]
#[serial]
fn test_q_in_search_mode_types_q_not_quit() {
    let env = create_test_env_with_sessions(3);
    let mut view = env.view;

    view.handle_key(key(KeyCode::Char('/')), None);
    assert!(view.search_active);

    let action = view.handle_key(key(KeyCode::Char('q')), None);
    assert_eq!(action, None);
    assert!(view.search_active);
    assert_eq!(view.search_query.value(), "q");
}

#[test]
#[serial]
fn test_has_dialog_true_when_search_active() {
    let env = create_test_env_empty();
    let mut view = env.view;

    assert!(!view.has_dialog());
    view.handle_key(key(KeyCode::Char('/')), None);
    assert!(view.has_dialog());
}

/// Verify that the async CreationPoller path returns a session ID from
/// `apply_creation_results` once the background thread finishes. This is
/// the code path that was previously starved by continuous input events
/// in the tokio::select! event loop (see #633).
#[test]
#[serial]
fn test_apply_creation_results_returns_session_id() {
    use crate::tui::dialogs::NewSessionData;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let project_dir = temp.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("default".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let data = NewSessionData {
        profile: "default".to_string(),
        title: "Async Test".to_string(),
        path: project_dir.to_str().unwrap().to_string(),
        group: String::new(),
        tool: "claude".to_string(),
        worktree_enabled: false,
        worktree_branch: None,
        create_new_branch: false,
        base_branch: None,
        extra_repo_paths: Vec::new(),
        sandbox: false,
        sandbox_image: String::new(),
        yolo_mode: false,
        extra_env: Vec::new(),
        extra_args: String::new(),
        command_override: String::new(),
        scratch: false,
        fork_seed: None,
        structured: false,
    };

    // Use the async CreationPoller path (pass None hooks, non-sandbox,
    // but call request_creation directly to force the async path)
    view.request_creation(data, None);
    assert!(view.is_creation_pending());

    // Wait for the background thread to finish (should be near-instant
    // for non-sandbox, non-hook creation)
    let start = std::time::Instant::now();
    let mut session_id = None;
    while start.elapsed() < std::time::Duration::from_secs(5) {
        if let Some(id) = view.apply_creation_results() {
            session_id = Some(id);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let session_id = session_id.expect("apply_creation_results should return Some(session_id)");
    assert!(
        view.get_instance(&session_id).is_some(),
        "created session should be findable after apply_creation_results"
    );
}

#[test]
fn test_project_group_name_uses_last_path_segment() {
    use super::project_group_name;

    let inst = Instance::new("test", "/home/user/my-project");
    assert_eq!(project_group_name(&inst), "my-project");
}

#[test]
fn test_project_group_name_uses_main_repo_for_worktree() {
    use super::project_group_name;
    use crate::session::WorktreeInfo;
    use chrono::Utc;

    let mut inst = Instance::new("test", "/home/user/my-project/.worktrees/feature-abc");
    inst.worktree_info = Some(WorktreeInfo {
        branch: "feature-abc".to_string(),
        main_repo_path: "/home/user/my-project".to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });
    assert_eq!(project_group_name(&inst), "my-project");
}

#[test]
fn test_project_group_name_handles_trailing_slash() {
    use super::project_group_name;

    let inst = Instance::new("test", "/home/user/my-project/");
    assert_eq!(project_group_name(&inst), "my-project");
}

#[test]
fn test_project_group_name_groups_scratch_under_scratch() {
    use super::project_group_name;

    let mut inst = Instance::new(
        "test",
        "/home/user/.config/agent-of-empires/scratch/a4535853054b4096",
    );
    inst.scratch = true;
    assert_eq!(project_group_name(&inst), "scratch");
}

#[test]
#[serial]
fn test_cursor_follows_session_after_deletion() {
    let mut env = create_test_env_with_sessions(4);

    // Cursor starts at 0; move it to index 2 (session2)
    env.view.cursor = 2;
    env.view.update_selected();
    let tracked_id = env.view.selected_session.clone().unwrap();

    // Delete item at index 1 (a session above the cursor)
    let victim_id = match &env.view.flat_items[1] {
        Item::Session { id, .. } => id.clone(),
        _ => panic!("expected session at index 1"),
    };
    env.view.remove_instance(&victim_id);
    env.view.rebuild_group_trees();
    let _ = env.view.save();
    env.view.reload().unwrap();

    // Cursor should have followed the tracked session to its new position
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(tracked_id.as_str())
    );
    assert_eq!(env.view.cursor, 1);
}

#[test]
#[serial]
fn home_defaults_to_agent_when_config_unset() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert_eq!(view.view_mode, ViewMode::Structured);
}

#[test]
#[serial]
fn wants_text_selection_tracks_copy_friendly_surfaces() {
    use crate::tui::dialogs::ChangelogDialog;

    let mut env = create_test_env_empty();

    // Fresh dashboard: mouse capture should stay on (wheel-scroll works).
    assert!(!env.view.wants_text_selection());

    // info_dialog (e.g. an error message the user might want to copy).
    env.view.info_dialog = Some(InfoDialog::new("Error", "something went wrong"));
    assert!(env.view.wants_text_selection());
    env.view.info_dialog = None;
    assert!(!env.view.wants_text_selection());

    // changelog_dialog (release notes).
    env.view.changelog_dialog = Some(ChangelogDialog::new(Some("1.0.0".to_string())));
    assert!(env.view.wants_text_selection());
    env.view.changelog_dialog = None;
    assert!(!env.view.wants_text_selection());

    // serve_view is feature-gated; only assert it when the feature is on,
    // since the field isn't present otherwise.
    #[cfg(feature = "serve")]
    {
        use crate::tui::dialogs::ServeView;
        env.view.serve_view = Some(ServeView::new());
        assert!(env.view.wants_text_selection());
        env.view.serve_view = None;
        assert!(!env.view.wants_text_selection());
    }
}

// -- apply_one_status_update -------------------------------------------------
//
// These guard the bug discovered in #872: the polling loop runs
// `update_status_with_metadata` on a clone, then projects the result into
// a `StatusUpdate`. The first version of that struct dropped the
// freshly-set `idle_entered_at`, which meant the breathe rattle and
// fresh-idle color never fired in the TUI even though everything looked
// right via the API.

#[test]
#[serial]
fn apply_status_update_propagates_idle_entered_at_into_live_instance() {
    use crate::session::Status;
    use crate::tui::status_poller::{IdleIntent, StatusUpdate};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };

    // The instance was just created (Idle, no transition observed yet).
    assert_eq!(env.view.get_instance(&id).unwrap().idle_entered_at, None);

    // Simulate the poller observing a Stop hook: status stays Idle on
    // disk but the wrapper writes `idle_entered_at` on the polling
    // clone. The apply path must carry that timestamp into the live
    // instance, otherwise nothing downstream sees it.
    let now = chrono::Utc::now();
    env.view.apply_one_status_update(StatusUpdate {
        id: id.clone(),
        status: Status::Idle,
        last_error: None,
        idle_entered_at: IdleIntent::Set(now),
        last_accessed_at: None,
        pane_dead: false,
        live_status_baseline: None,
    });

    let inst = env.view.get_instance(&id).unwrap();
    assert_eq!(inst.status, Status::Idle);
    assert_eq!(inst.idle_entered_at, Some(now));
}

// #2690: `update_status_with_metadata` compares against
// `live_status_baseline`, but the background poller only ever mutates a
// *clone* of the real `Instance` (see `status_poller.rs`). If
// `StatusUpdate` doesn't carry the clone's freshly-seeded baseline back,
// the real `Instance` in `self.instances` keeps `live_status_baseline ==
// None` forever, so every poll looks like "no baseline yet" and no real
// transition after the first ever restamps again.
#[test]
#[serial]
fn apply_status_update_propagates_live_status_baseline_from_poller() {
    use crate::tui::status_poller::{poll_statuses_once, StatusPollState};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    assert_eq!(
        env.view.get_instance(&id).unwrap().live_status_baseline,
        None,
        "freshly loaded instance has no live baseline yet"
    );

    // Drive a real poll cycle through the same path the background thread
    // uses: clone -> poll_statuses_once -> project into StatusUpdate ->
    // apply back onto the real Instance.
    let mut poll_state = StatusPollState::new();
    let instances = env.view.pollable_instances();
    let updates = poll_statuses_once(instances, &mut poll_state);
    for update in updates {
        env.view.apply_one_status_update(update);
    }

    assert!(
        env.view
            .get_instance(&id)
            .unwrap()
            .live_status_baseline
            .is_some(),
        "the polling clone's seeded baseline must survive back into the \
         real Instance via StatusUpdate, or every future poll re-seeds \
         instead of restamping real transitions"
    );
}

// #2690: `IdleIntent::Keep` means the producer has no observation for
// `idle_entered_at`. The consumer must not touch the field, or an
// unseeded `attached_status_hooks` snapshot on attach exit would clobber
// a real value the main-thread poller wrote during attach. The other two
// variants (`Set(ts)`, `Clear`) are unambiguous and always apply.
#[test]
#[serial]
fn apply_status_update_preserves_idle_entered_at_on_keep() {
    use crate::session::Status;
    use crate::tui::status_poller::{IdleIntent, StatusUpdate};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };

    // Seed a real `idle_entered_at` on the live instance, as if the
    // main-thread poller had already observed an Idle transition.
    let seeded = chrono::Utc::now() - chrono::Duration::minutes(30);
    env.view.mutate_instance(&id, |inst| {
        inst.idle_entered_at = Some(seeded);
    });

    // Then apply a `Keep` update, mirroring an `attached_status_hooks`
    // snapshot from a watcher clone that never polled.
    env.view.apply_one_status_update(StatusUpdate {
        id: id.clone(),
        status: Status::Idle,
        last_error: None,
        idle_entered_at: IdleIntent::Keep,
        last_accessed_at: None,
        pane_dead: false,
        live_status_baseline: None,
    });

    assert_eq!(
        env.view.get_instance(&id).unwrap().idle_entered_at,
        Some(seeded),
        "`Keep` must not clobber an already-established `idle_entered_at`"
    );
}

// #2690: a passively-detected status transition must land on disk
// immediately, not just in memory, so the next reload (TUI relaunch, or
// a peer like `aoe serve`) finds disk already caught up instead of
// comparing against a stale snapshot and misreading it as a fresh
// transition.
#[test]
#[serial]
fn apply_status_update_persists_genuine_transition_to_disk() {
    use crate::session::{Status, Storage};
    use crate::tui::status_poller::{IdleIntent, StatusUpdate};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    assert_eq!(env.view.get_instance(&id).unwrap().status, Status::Idle);

    let now = chrono::Utc::now();
    env.view.apply_one_status_update(StatusUpdate {
        id: id.clone(),
        status: Status::Running,
        last_error: None,
        idle_entered_at: IdleIntent::Clear,
        last_accessed_at: Some(now),
        pane_dead: false,
        live_status_baseline: None,
    });

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    let row = reloaded.iter().find(|i| i.id == id).expect("row present");
    assert_eq!(
        row.status,
        Status::Running,
        "the genuine Idle -> Running transition must be persisted, not just in-memory"
    );
    assert_eq!(row.last_accessed_at, Some(now));
}

#[test]
#[serial]
fn apply_status_update_clears_idle_entered_at_on_idle_to_running() {
    use crate::session::Status;
    use crate::tui::status_poller::{IdleIntent, StatusUpdate};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };

    // Seed: session is Idle with a freshness timestamp set.
    let stop_time = chrono::Utc::now() - chrono::Duration::seconds(60);
    env.view.apply_one_status_update(StatusUpdate {
        id: id.clone(),
        status: Status::Idle,
        last_error: None,
        idle_entered_at: IdleIntent::Set(stop_time),
        last_accessed_at: None,
        pane_dead: false,
        live_status_baseline: None,
    });
    assert_eq!(
        env.view.get_instance(&id).unwrap().idle_entered_at,
        Some(stop_time)
    );

    // Transition Idle -> Running. The poller's wrapper clears
    // `idle_entered_at` on the clone for non-Idle states; the apply
    // path has to honor that, otherwise a Running session would still
    // claim a freshness age.
    env.view.apply_one_status_update(StatusUpdate {
        id: id.clone(),
        status: Status::Running,
        last_error: None,
        idle_entered_at: IdleIntent::Clear,
        last_accessed_at: None,
        pane_dead: false,
        live_status_baseline: None,
    });

    let inst = env.view.get_instance(&id).unwrap();
    assert_eq!(inst.status, Status::Running);
    assert_eq!(inst.idle_entered_at, None);
    // And `idle_age()` must not synthesize one out of stale state.
    assert_eq!(inst.idle_age(), None);
}

#[test]
#[serial]
fn archived_running_session_renders_stopped_icon_not_spinner() {
    // Regression for af711cb: pre-fix, archived/snoozed rows still cycled
    // through animated spinner frames driven by their underlying Running
    // status, making sunk rows read as "still alive" and pulling the eye
    // away from real attention items. Pin the icon to ICON_STOPPED for
    // archived rows even when status is Running.
    use super::render::agent_row_icon;
    use super::ICON_STOPPED;
    use crate::session::Status;

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected one session"),
    };

    // Archive the session AND keep its underlying status as Running so the
    // spinner branch would fire in the absence of the override.
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
        inst.archived_at = Some(chrono::Utc::now());
    });

    let inst = env.view.get_instance(&id).expect("session present");
    let icon = agent_row_icon(inst);

    assert_eq!(
        icon, ICON_STOPPED,
        "archived row must render stopped icon, not animated spinner"
    );

    // Same expectation for snooze: a row snoozed into the future must not
    // animate even if it's also Running underneath.
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
        inst.archived_at = None;
        inst.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::minutes(15));
    });
    let inst = env.view.get_instance(&id).expect("session present");
    assert_eq!(
        agent_row_icon(inst),
        ICON_STOPPED,
        "snoozed row must render stopped icon, not animated spinner"
    );

    // Sanity: a plain Running row (no archive, no snooze) must NOT collapse
    // to ICON_STOPPED; otherwise the test would pass trivially because the
    // helper always returned the stopped glyph.
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
        inst.archived_at = None;
        inst.snoozed_until = None;
    });
    let inst = env.view.get_instance(&id).expect("session present");
    assert_ne!(
        agent_row_icon(inst),
        ICON_STOPPED,
        "non-archived Running row should keep its spinner; helper would be a no-op otherwise"
    );
}

#[test]
#[serial]
fn apply_status_update_skips_terminal_states() {
    use crate::session::Status;
    use crate::tui::status_poller::{IdleIntent, StatusUpdate};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };

    // Move the session into a terminal state that the apply path is
    // supposed to leave alone.
    env.view
        .mutate_instance(&id, |inst| inst.status = Status::Deleting);
    let stale_ts = chrono::Utc::now() - chrono::Duration::seconds(10);

    env.view.apply_one_status_update(StatusUpdate {
        id: id.clone(),
        status: Status::Idle,
        last_error: None,
        idle_entered_at: IdleIntent::Set(stale_ts),
        last_accessed_at: None,
        pane_dead: false,
        live_status_baseline: None,
    });

    // Status and timestamp should both stay untouched.
    let inst = env.view.get_instance(&id).unwrap();
    assert_eq!(inst.status, Status::Deleting);
    assert_eq!(inst.idle_entered_at, None);
}

#[test]
#[serial]
fn apply_stop_results_transitions_instance_to_stopped() {
    use crate::session::Status;
    use crate::tui::stop_poller::StopRequest;

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };

    // Pretend the session is live, then dispatch the stop to the background
    // poller exactly as Action::StopSession does. The fixture instance has no
    // tmux pane or sandbox, so perform_stop returns success quickly.
    env.view
        .mutate_instance(&id, |inst| inst.status = Status::Running);
    let inst = env.view.get_instance(&id).unwrap().clone();
    env.view.stop_poller.request_stop(StopRequest {
        session_id: id.clone(),
        instance: inst,
    });

    // Poll the result-application path the main loop runs each frame.
    let mut applied = false;
    for _ in 0..50 {
        if env.view.apply_stop_results() {
            applied = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(applied, "apply_stop_results never observed the stop result");

    let inst = env.view.get_instance(&id).unwrap();
    assert_eq!(inst.status, Status::Stopped);
    assert_eq!(inst.last_error, None);
}

#[test]
#[serial]
fn apply_status_update_runs_status_hook_on_transition() {
    use crate::session::Status;
    use crate::status_hooks::{take_recorded_launches, StatusHookConfig};
    use crate::tui::status_poller::{IdleIntent, StatusUpdate};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    env.view.status_hook_config = StatusHookConfig {
        enabled: true,
        on_waiting: Some("notify-waiting".to_string()),
        on_change: Some("notify-change".to_string()),
        ..Default::default()
    };
    take_recorded_launches();

    env.view.apply_one_status_update(StatusUpdate {
        id: id.clone(),
        status: Status::Waiting,
        last_error: None,
        idle_entered_at: IdleIntent::Clear,
        last_accessed_at: None,
        pane_dead: false,
        live_status_baseline: None,
    });

    let launches = take_recorded_launches();
    assert_eq!(launches.len(), 2);
    assert_eq!(launches[0].command, "notify-waiting");
    assert_eq!(launches[1].command, "notify-change");
    assert_eq!(launches[0].context.session_id, id);
    assert_eq!(launches[0].context.old_status, Status::Idle);
    assert_eq!(launches[0].context.new_status, Status::Waiting);
}

#[test]
#[serial]
fn all_profiles_status_hook_lookup_uses_cache() {
    use crate::status_hooks::StatusHookConfig;

    let mut env = create_test_env_with_sessions(1);
    env.view.active_profile = None;
    env.view.status_hook_config = StatusHookConfig::default();
    env.view.status_hook_configs.clear();
    env.view.status_hook_configs.insert(
        "cached".to_string(),
        StatusHookConfig {
            enabled: true,
            on_waiting: Some("notify-cached".to_string()),
            ..Default::default()
        },
    );

    let mut instance = Instance::new("Cached profile", "/tmp/cached");
    instance.source_profile = "cached".to_string();

    let config = env.view.status_hook_config_for(&instance);
    assert!(config.enabled);
    assert_eq!(config.on_waiting.as_deref(), Some("notify-cached"));
}

#[test]
#[serial]
fn apply_status_update_does_not_run_status_hook_for_same_status() {
    use crate::session::Status;
    use crate::status_hooks::{take_recorded_launches, StatusHookConfig};
    use crate::tui::status_poller::{IdleIntent, StatusUpdate};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    env.view.status_hook_config = StatusHookConfig {
        enabled: true,
        on_change: Some("notify-change".to_string()),
        ..Default::default()
    };
    take_recorded_launches();

    env.view.apply_one_status_update(StatusUpdate {
        id,
        status: Status::Idle,
        last_error: None,
        idle_entered_at: IdleIntent::Keep,
        last_accessed_at: None,
        pane_dead: false,
        live_status_baseline: None,
    });

    assert!(take_recorded_launches().is_empty());
}

#[test]
#[serial]
fn apply_status_updates_without_hooks_does_not_run_status_hook() {
    use crate::session::Status;
    use crate::status_hooks::{take_recorded_launches, StatusHookConfig};
    use crate::tui::status_poller::{IdleIntent, StatusUpdate};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    env.view.status_hook_config = StatusHookConfig {
        enabled: true,
        on_waiting: Some("notify-waiting".to_string()),
        ..Default::default()
    };
    take_recorded_launches();

    env.view
        .apply_status_updates_without_hooks(vec![StatusUpdate {
            id: id.clone(),
            status: Status::Waiting,
            last_error: None,
            idle_entered_at: IdleIntent::Clear,
            last_accessed_at: None,
            pane_dead: false,
            live_status_baseline: None,
        }]);

    assert_eq!(env.view.get_instance(&id).unwrap().status, Status::Waiting);
    assert!(take_recorded_launches().is_empty());
}

#[test]
#[serial]
fn set_instance_status_runs_status_hook_on_transition() {
    use crate::session::Status;
    use crate::status_hooks::{take_recorded_launches, StatusHookConfig};

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    env.view.status_hook_config = StatusHookConfig {
        enabled: true,
        on_error: Some("notify-error".to_string()),
        ..Default::default()
    };
    take_recorded_launches();

    env.view.set_instance_status(&id, Status::Error);

    let launches = take_recorded_launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].command, "notify-error");
    assert_eq!(launches[0].context.old_status, Status::Idle);
    assert_eq!(launches[0].context.new_status, Status::Error);
}

/// Regression: paste over a group header must stash to `pending_paste`,
/// never open a compose dialog targeted at "the first running session".
/// Earlier behavior fell through to the first-running fallback whenever
/// `selected_session` was None — silently misrouting voice/dictation
/// across groups. With cursor on a group, `selected_session` is None and
/// `resolve_send_target` must return None unconditionally.
#[test]
#[serial]
fn paste_on_group_header_stashes_instead_of_misrouting() {
    let mut env = create_test_env_with_groups();

    // Find the cursor index of the first group header in flat_items.
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("fixture should produce at least one group header");
    env.view.cursor = group_idx;
    env.view.update_selected();

    // Cursor on a group sets selected_session to None.
    assert!(
        env.view.selected_session.is_none(),
        "cursor on a group header must clear selected_session"
    );

    env.view
        .handle_paste("voice dictation that must not misroute");

    assert!(
        env.view.send_message_dialog.is_none(),
        "paste over a group must NOT open a compose dialog against an unrelated session"
    );
    assert_eq!(
        env.view.pending_paste.as_deref(),
        Some("voice dictation that must not misroute"),
        "paste over a group must stash to pending_paste"
    );
}

/// Regression: a transient status toast must render even when no aoe update
/// is pending. Before the fix, the update-bar row was only laid out when
/// `update_info.is_some()`, so toasts produced by paths like the
/// restart-during-attach failure or `Action::SendMessage`'s "Reviving
/// session..." were silently dropped on the floor for the common-case user
/// with no update available.
#[test]
#[serial]
fn update_bar_renders_status_toast_without_update_info() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");

    let toast = "restart failed: tmux session unreachable";

    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, Some(toast), None);
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }

    assert!(
        out.contains("restart failed:"),
        "expected the toast to be rendered even when update_info is None.\n\
         Full buffer:\n{out}"
    );
    assert!(
        out.contains("[Ctrl+x] dismiss"),
        "expected the dismiss hint alongside the toast.\nFull buffer:\n{out}"
    );
}

/// The sandbox-image update banner renders (with its `[u] pull` /
/// `[Ctrl+x] dismiss` hints) when an `ImageUpdate` is present and no
/// higher-priority banner is up. Guards the lowest-priority slot in
/// `render_update_bar`.
#[test]
#[serial]
fn update_bar_renders_sandbox_image_banner() {
    use crate::containers::image_update::ImageUpdate;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");

    let image_update = ImageUpdate {
        image: "ghcr.io/agent-of-empires/aoe-sandbox:latest".to_string(),
        remote_digest: "sha256:abc".to_string(),
    };

    terminal
        .draw(|f| {
            let area = f.area();
            env.view
                .render(f, area, &theme, None, None, Some(&image_update));
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }

    assert!(
        out.contains("sandbox image update available"),
        "expected the sandbox image banner to render.\nFull buffer:\n{out}"
    );
    assert!(
        out.contains("[u] pull") && out.contains("[Ctrl+x] dismiss"),
        "expected the pull/dismiss hints alongside the image banner.\nFull buffer:\n{out}"
    );
}

/// The app-update banner wins the shared bottom row over a pending
/// sandbox-image update: only one shows at a time, so the lower-priority
/// image banner must stay hidden (and its `[u] pull` hint absent) while an
/// app update is up. This is what keeps the `u` / Ctrl+x keys unambiguous.
#[test]
#[serial]
fn app_update_banner_takes_precedence_over_image_banner() {
    use crate::containers::image_update::ImageUpdate;
    use crate::tui::styles::load_theme;
    use crate::update::UpdateInfo;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");

    let update_info = UpdateInfo {
        available: true,
        current_version: "1.0.0".to_string(),
        latest_version: "1.1.0".to_string(),
    };
    let image_update = ImageUpdate {
        image: "ghcr.io/agent-of-empires/aoe-sandbox:latest".to_string(),
        remote_digest: "sha256:abc".to_string(),
    };

    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(
                f,
                area,
                &theme,
                Some(&update_info),
                None,
                Some(&image_update),
            );
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }

    assert!(
        out.contains("update available 1.0.0"),
        "expected the app update banner to win the row.\nFull buffer:\n{out}"
    );
    assert!(
        !out.contains("sandbox image update available"),
        "image banner must stay hidden while an app update is shown.\nFull buffer:\n{out}"
    );
}

/// Issue #2220: the app-update banner reassures users that updating is safe
/// for running sessions. The reassurance must render alongside the version and
/// action keys so users know `u` won't tear down their work.
#[test]
#[serial]
fn app_update_banner_reassures_running_sessions_are_safe() {
    use crate::tui::styles::load_theme;
    use crate::update::UpdateInfo;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");

    let update_info = UpdateInfo {
        available: true,
        current_version: "1.0.0".to_string(),
        latest_version: "1.1.0".to_string(),
    };

    terminal
        .draw(|f| {
            let area = f.area();
            env.view
                .render(f, area, &theme, Some(&update_info), None, None);
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }

    assert!(
        out.contains("running sessions stay safe"),
        "expected the update banner to reassure that running sessions are safe.\nFull buffer:\n{out}"
    );
    assert!(
        out.contains("[u] update"),
        "the action key must still render alongside the reassurance.\nFull buffer:\n{out}"
    );

    // Narrow-terminal contract: the reassurance is appended after the keys
    // precisely so the action hints survive when the line is too narrow to
    // hold everything. At 72 columns the keys fit but the reassurance clips.
    let narrow = TestBackend::new(72, 30);
    let mut narrow_terminal = Terminal::new(narrow).unwrap();
    narrow_terminal
        .draw(|f| {
            let area = f.area();
            env.view
                .render(f, area, &theme, Some(&update_info), None, None);
        })
        .unwrap();

    let nbuf = narrow_terminal.backend().buffer();
    let mut nout = String::new();
    for y in 0..nbuf.area.height {
        for x in 0..nbuf.area.width {
            nout.push_str(nbuf[(x, y)].symbol());
        }
        nout.push('\n');
    }

    assert!(
        nout.contains("[u] update") && nout.contains("[Ctrl+x] dismiss"),
        "the action keys must survive clipping on a narrow terminal.\nFull buffer:\n{nout}"
    );
    assert!(
        !nout.contains("running sessions stay safe"),
        "the trailing reassurance is expected to clip first on a narrow terminal.\nFull buffer:\n{nout}"
    );
}

/// Regression for the e2e CI failure (job 76034901940):
/// `test_command_palette_fuzzy_search_settings` and
/// `test_profile_picker_create_new_profile` failed because the harness types
/// fast enough to trip the paste-burst detector, and the resulting "paste"
/// got stashed in `pending_paste` instead of reaching the dialog's input.
/// `wants_paste_burst` must be false for dialogs that capture keys via
/// `handle_key` but do not implement `handle_paste`.
#[test]
#[serial]
fn wants_paste_burst_only_for_paste_aware_dialogs() {
    let mut env = create_test_env_empty();

    // No dialog open: burst is needed (home shortcuts at risk).
    assert!(
        env.view.wants_paste_burst(),
        "burst must be enabled when no dialog is open"
    );

    // Command palette: captures keys, no handle_paste. Burst would
    // strand input in pending_paste — must be disabled.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.command_palette.is_some(),
        "Ctrl+K must open the command palette"
    );
    assert!(
        !env.view.wants_paste_burst(),
        "burst must be disabled when command palette is open"
    );
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.command_palette.is_none());
    assert!(
        env.view.wants_paste_burst(),
        "burst should re-enable after dialog closes"
    );
}

#[test]
#[serial]
fn pollable_instances_excludes_recovery_in_flight() {
    let mut env = create_test_env_with_sessions(3);
    let id_skipped = env.view.instance_at(1).id.clone();
    env.view.recovery_in_flight.insert(id_skipped.clone());

    let pollable = env.view.pollable_instances();

    assert_eq!(pollable.len(), 2);
    assert!(pollable.iter().all(|i| i.id != id_skipped));
}

#[test]
#[serial]
fn pollable_instances_recovers_after_inflight_clear() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.recovery_in_flight.insert(id.clone());
    assert!(env.view.pollable_instances().is_empty());

    env.view.recovery_in_flight.remove(&id);

    assert_eq!(env.view.pollable_instances().len(), 1);
}

/// Footer hides Archive/Fav/Snooze hints unless `sort_order` is Attention.
/// The underlying keybinds still work in any mode; only the discoverability
/// hints in `render_status_bar` adapt so the footer doesn't waste width on
/// shortcuts that don't visibly reorder the list in Newest/Created/LastAccessed.
#[test]
#[serial]
fn footer_hides_attention_workflow_hints_outside_attention_sort() {
    use crate::session::config::SortOrder;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let theme = load_theme("empire");

    let render_footer = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(200, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    // Newest sort: footer should NOT advertise attention-workflow shortcuts.
    env.view.sort_order = SortOrder::Newest;
    let newest_out = render_footer(&mut env);
    assert!(
        !newest_out.contains("Snooze"),
        "Snooze hint should be hidden in Newest sort.\n{newest_out}"
    );
    assert!(
        !newest_out.contains("Fav"),
        "Fav hint should be hidden in Newest sort.\n{newest_out}"
    );
    assert!(
        !newest_out.contains("Archive"),
        "Archive hint should be hidden in Newest sort.\n{newest_out}"
    );

    // Attention sort: footer should advertise them.
    env.view.sort_order = SortOrder::Attention;
    let attention_out = render_footer(&mut env);
    assert!(
        attention_out.contains("Snooze"),
        "Snooze hint should appear in Attention sort.\n{attention_out}"
    );
    assert!(
        attention_out.contains("Fav"),
        "Fav hint should appear in Attention sort.\n{attention_out}"
    );
    assert!(
        attention_out.contains("Archive"),
        "Archive hint should appear in Attention sort.\n{attention_out}"
    );
}

/// `toggle_favorite_at_cursor` flips the cursor's instance favorited state
/// and persists the change. No toast: the row's visual treatment (bold +
/// leading `* ` glyph) is the feedback.
#[test]
#[serial]
fn toggle_favorite_at_cursor_round_trip() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    // Initial state: not favorited.
    assert!(!env.view.instance_at(0).is_favorited());

    env.view.toggle_favorite_at_cursor().unwrap();
    assert!(env.view.instance_at(0).is_favorited());

    env.view.toggle_favorite_at_cursor().unwrap();
    assert!(!env.view.instance_at(0).is_favorited());
}

/// When no session is selected, the toggle is a silent no-op.
#[test]
#[serial]
fn toggle_favorite_at_cursor_noop_with_no_selection() {
    let mut env = create_test_env_empty();
    env.view.selected_session = None;
    env.view.toggle_favorite_at_cursor().unwrap();
}

/// `toggle_archive_at_cursor` flips the cursor's instance archived state
/// and persists the change. No toast: the row sinks to tier 99 and that
/// visible reordering is the feedback.
#[test]
#[serial]
fn toggle_archive_at_cursor_round_trip() {
    let mut env = create_test_env_with_sessions(1);
    // Keep the Archived section expanded so the archived row stays reachable.
    env.view.archived_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    // Initial state: not archived.
    assert!(!env.view.instance_at(0).is_archived());

    env.view.toggle_archive_at_cursor().unwrap();
    assert!(env.view.instance_at(0).is_archived());

    // Archiving moved the selection off the row (it advances to the next
    // active session; here there is none). Navigate back onto the archived
    // row, as a user would, before toggling it back.
    env.view.select_session_by_id(&id);
    env.view.toggle_archive_at_cursor().unwrap();
    assert!(!env.view.instance_at(0).is_archived());
}

/// Trashing a session hides it from the active list and surfaces it under
/// the synthetic Trash section; the shelve/unshelve key (`z`) restores it.
#[test]
#[serial]
fn trash_then_restore_round_trip() {
    let mut env = create_test_env_with_sessions(2);
    // Keep the Trash section expanded so the trashed row stays reachable.
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    assert!(!env.view.instance_at(0).is_trashed());

    env.view.trash_session_by_id(&id);
    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "session must be trashed"
    );

    // The Trash section header is present, and the trashed row is not in the
    // active flow (it renders under that header).
    let items = env.view.build_flat_items();
    assert!(
        items.iter().any(|it| matches!(
            it,
            Item::Group { path, .. } if crate::session::is_trash_section_path(path)
        )),
        "Trash section header must be present after trashing"
    );

    // Restore via the shelve/unshelve key.
    env.view.select_session_by_id(&id);
    env.view.toggle_archive_at_cursor().unwrap();
    assert!(
        !env.view.get_instance(&id).unwrap().is_trashed(),
        "session must be restored out of trash"
    );
}

/// Regression for #2489: trashing a session must not re-expand a Trash
/// section the user has collapsed. Like single-row archive, the section
/// header's count is the feedback; the collapse state is left untouched.
#[test]
#[serial]
fn trashing_leaves_collapsed_trash_section_collapsed() {
    let mut env = create_test_env_with_sessions(2);
    assert!(
        env.view.trashed_section_collapsed,
        "Trash section defaults to collapsed"
    );
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    env.view.trash_session_by_id(&id);

    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "session must be trashed"
    );
    assert!(
        env.view.trashed_section_collapsed,
        "trashing must not re-expand a collapsed Trash section"
    );
}

/// Regression: trashing must offload the blocking teardown (tmux kill, the
/// ~10s `docker stop`, and the worktree relocation) to the `TrashPoller`
/// instead of running it on the input thread, which froze the TUI while the
/// sandbox container stopped. The durable trash marker is still written inline
/// so the row flips to Trashed instantly; the teardown is merely queued.
#[test]
#[serial]
fn trash_offloads_blocking_teardown_to_poller() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    env.view.trash_session_by_id(&id);

    // Inline: the row is durably trashed the instant the key is handled.
    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "trash marker must be written inline for instant feedback"
    );
    // Off-thread: the blocking teardown is in flight on the worker, tracked in
    // its pending set until a result is drained. If trashing had run the
    // teardown inline (the frozen-TUI bug), nothing would be queued here.
    let pending = env.view.trash_poller.take_pending();
    assert_eq!(
        pending,
        vec![id],
        "trash teardown must be queued on the TrashPoller, not run on the input thread"
    );
}

/// Right-clicking the synthetic Trash section header opens the bulk menu
/// (Empty Trash / Restore All / Collapse), not the meaningless "Rename Group /
/// Delete Group" a real group would show.
#[test]
#[serial]
fn right_click_trash_header_shows_bulk_menu() {
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);

    let header_idx = env
        .view
        .flat_items
        .iter()
        .position(|it| {
            matches!(it, Item::Group { path, .. }
                if crate::session::is_trash_section_path(path))
        })
        .expect("Trash header must render");
    render_geometry(&mut env.view);
    let row = shelf_row_for_idx(&env.view, header_idx);
    assert!(env.view.handle_right_click(5, row));

    let labels: Vec<&str> = env
        .view
        .context_menu
        .as_ref()
        .unwrap()
        .items_for_test()
        .iter()
        .map(|(_, l)| *l)
        .collect();
    assert_eq!(labels, vec!["Empty Trash", "Restore All", "Collapse"]);
}

/// Right-clicking the synthetic Archived section header offers Restore All and
/// the collapse toggle, but no destructive "empty" action (archived rows are
/// never purged from there).
#[test]
#[serial]
fn right_click_archived_header_shows_restore_menu() {
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = false;
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.toggle_archive_at_cursor().unwrap();

    let header_idx = env
        .view
        .flat_items
        .iter()
        .position(|it| {
            matches!(it, Item::Group { path, .. }
                if crate::session::is_archived_section_path(path))
        })
        .expect("Archived header must render");
    render_geometry(&mut env.view);
    let row = shelf_row_for_idx(&env.view, header_idx);
    assert!(env.view.handle_right_click(5, row));

    let labels: Vec<&str> = env
        .view
        .context_menu
        .as_ref()
        .unwrap()
        .items_for_test()
        .iter()
        .map(|(_, l)| *l)
        .collect();
    assert_eq!(labels, vec!["Restore All", "Collapse"]);
}

/// "Empty Trash" routes through a destructive confirm carrying the count; the
/// confirmed action marks every trashed row for deletion (claimed + Deleting).
#[test]
#[serial]
fn empty_trash_confirm_purges_every_trashed_row() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(3);
    let a = env.view.instance_at(0).id.clone();
    let b = env.view.instance_at(1).id.clone();
    env.view.trash_session_by_id(&a);
    env.view.trash_session_by_id(&b);

    env.view.prompt_empty_trash();
    let dialog = env
        .view
        .confirm_dialog
        .as_ref()
        .expect("Empty Trash must open a confirm dialog");
    assert_eq!(dialog.action(), "empty_trash");

    env.view.dispatch_confirm_submit("empty_trash");
    for id in [&a, &b] {
        let inst = env
            .view
            .get_instance(id)
            .expect("row kept until purge lands");
        assert_eq!(
            inst.status,
            Status::Deleting,
            "each trashed row must be claimed and marked Deleting"
        );
        assert!(
            env.view.purge_claimed.contains(id),
            "each row's purge claim must be owned"
        );
    }
}

/// "Empty Trash" on an already-empty trash shows an info dialog instead of a
/// confirm that would delete nothing.
#[test]
#[serial]
fn empty_trash_on_empty_trash_is_a_noop_info() {
    let mut env = create_test_env_with_sessions(2);
    env.view.prompt_empty_trash();
    assert!(env.view.confirm_dialog.is_none());
    assert_eq!(
        env.view.info_dialog.as_ref().map(|d| d.title()),
        Some("Trash is empty")
    );
}

/// "Restore All" pulls every trashed session back out of the trash in one go.
#[test]
#[serial]
fn restore_all_from_trash_restores_every_row() {
    let mut env = create_test_env_with_sessions(3);
    let a = env.view.instance_at(0).id.clone();
    let b = env.view.instance_at(1).id.clone();
    env.view.trash_session_by_id(&a);
    env.view.trash_session_by_id(&b);
    assert_eq!(
        env.view
            .instances
            .values()
            .filter(|i| i.is_trashed())
            .count(),
        2
    );

    env.view.restore_all_from_trash();
    assert_eq!(
        env.view
            .instances
            .values()
            .filter(|i| i.is_trashed())
            .count(),
        0,
        "Restore All must un-trash every row"
    );
}

/// "Restore All" on the Archived section unarchives every archived session.
#[test]
#[serial]
fn unarchive_all_unarchives_every_row() {
    let mut env = create_test_env_with_sessions(3);
    for i in 0..2 {
        env.view.cursor = i;
        env.view.update_selected();
        env.view.toggle_archive_at_cursor().unwrap();
    }
    assert_eq!(
        env.view
            .instances
            .values()
            .filter(|i| i.is_archived())
            .count(),
        2
    );

    env.view.unarchive_all();
    assert_eq!(
        env.view
            .instances
            .values()
            .filter(|i| i.is_archived())
            .count(),
        0,
        "Restore All (archived) must unarchive every row"
    );
}

/// The Trash section renders in the pinned shelf with its distinct type glyph,
/// and the sort indicator moves onto the divider above it.
#[test]
#[serial]
fn shelf_renders_trash_with_glyph_and_divider_sort() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);

    let theme = load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut screen = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            screen.push_str(buf[(x, y)].symbol());
        }
        screen.push('\n');
    }

    assert!(
        screen.contains(super::ICON_TRASH_SECTION),
        "shelf must show the Trash type glyph"
    );
    assert!(
        screen.contains("Trash (1)"),
        "shelf must show the Trash count"
    );
    assert!(
        screen.contains("sort:"),
        "the sort indicator rides the shelf divider"
    );
    assert!(
        env.view.shelf_inner_area.height > 0,
        "a shelf rect must be populated when trash is present"
    );
}

/// Render the full home view into a TestBackend and dump the screen as one
/// string, for asserting on preview/list text.
fn render_home_to_string(view: &mut HomeView, width: u16, height: u16) -> String {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let theme = load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut screen = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            screen.push_str(buf[(x, y)].symbol());
        }
        screen.push('\n');
    }
    screen
}

/// A trashed row whose permanent delete failed carries `Status::Error` +
/// `last_error` (set by `apply_deletion_results`). The preview must surface
/// that failure instead of the calm "Trash" placeholder, and the shelf row
/// must show the error glyph instead of the uniform muted one.
#[test]
#[serial]
fn trashed_preview_surfaces_delete_failure() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);

    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Error;
        inst.last_error = Some("worktree removal failed: directory locked".to_string());
    });

    let screen = render_home_to_string(&mut env.view, 120, 40);
    assert!(
        screen.contains("worktree removal failed"),
        "a failed delete's error must show in the trashed preview.\n{screen}"
    );
    assert!(
        screen.contains(super::ICON_ERROR),
        "the shelf row must show the error glyph after a failed delete.\n{screen}"
    );
}

/// While a trashed row's permanent delete is running (`Status::Deleting`),
/// the preview placeholder must say so instead of advertising restore/delete
/// keys that would race the in-flight purge.
#[test]
#[serial]
fn trashed_preview_shows_deleting_status() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Deleting;
    });

    let screen = render_home_to_string(&mut env.view, 120, 40);
    assert!(
        screen.contains("Deleting"),
        "an in-flight delete must be visible in the trashed preview.\n{screen}"
    );
}

/// An archived row can also be deleted; a failed delete must surface in the
/// archived preview the same way.
#[test]
#[serial]
fn archived_preview_surfaces_delete_failure() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.select_session_by_id(&id);
    env.view.toggle_archive_at_cursor().unwrap();
    env.view.select_session_by_id(&id);
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Error;
        inst.last_error = Some("container teardown failed".to_string());
    });

    let screen = render_home_to_string(&mut env.view, 120, 40);
    assert!(
        screen.contains("container teardown failed"),
        "a failed delete's error must show in the archived preview.\n{screen}"
    );
}

/// Restart (`e`) on an archived or trashed row is intentionally refused, but
/// the refusal must be visible: an info dialog, not a silent no-op.
#[test]
#[serial]
fn restart_on_trashed_row_surfaces_refusal() {
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);

    env.view
        .restart_selected_session(None, None, None, None)
        .unwrap();
    assert!(
        env.view.info_dialog.is_some(),
        "restarting a trashed row must explain why nothing happened"
    );
}

/// While a trashed row's permanent delete is in flight (`Status::Deleting`),
/// restart must stay a silent drop: the "press z to restore it first" dialog
/// would race the purge (same rationale as the Deleting preview body, which
/// drops the restore/delete hints).
#[test]
#[serial]
fn restart_on_deleting_trashed_row_stays_silent() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Deleting;
    });

    env.view
        .restart_selected_session(None, None, None, None)
        .unwrap();
    assert!(
        env.view.info_dialog.is_none(),
        "a mid-purge row must not get a restore hint that races the delete"
    );
}

/// In compact layouts (< 80 cols) the preview hoists the session's status
/// icon into the block title. A trashed row must mask a stale persisted
/// Running status there (no spinner above the "Trash" placeholder body),
/// matching the archived treatment.
#[test]
#[serial]
fn compact_title_masks_stale_spinner_on_trashed_row() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);
    // Stale persisted live status; the pane was killed on trash.
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
    });

    let screen = render_home_to_string(&mut env.view, 70, 40);
    assert!(
        screen.contains("Trash"),
        "trashed placeholder should render.\n{screen}"
    );
    // The hoisted preview title starts at the block's top-left corner. With
    // the mask it carries ICON_STOPPED; unmasked, Running would paint a
    // time-varying `dots()` spinner frame there instead (a frame set that
    // never includes ICON_STOPPED, so this pin cannot pass by accident).
    let masked_title = format!("\u{256d} {} session0", super::ICON_STOPPED);
    assert!(
        screen.contains(&masked_title),
        "a trashed row's compact title must show the stopped icon, not a stale spinner.\n{screen}"
    );
}

/// Regression for #2489: `w` (jump to next needing-attention) must skip
/// trashed rows even when a stale unread flag survived the trash. A trashed
/// session is stopped and only lives under the Trash section, so it never
/// "needs attention".
#[test]
#[serial]
fn w_skips_unread_trashed_session() {
    use crate::session::Status;
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(2);
    // Non-strict so bare `w` routes to the jump handler, not the typing guard.
    env.view.strict_hotkeys = false;
    // Keep the Trash section expanded so the trashed row lands in `flat_items`;
    // that is the only way `w`'s walk could reach it.
    env.view.trashed_section_collapsed = false;

    let trashed = env.view.instance_at(0).id.clone();
    let active = env.view.instance_at(1).id.clone();
    // The surviving active row is a plain idle session (the pass-2 fallback);
    // the trashed row carries an unread flag, as it would after being trashed
    // while unread.
    env.view
        .mutate_instance(&active, |inst| inst.status = Status::Idle);
    env.view
        .mutate_instance(&trashed, |inst| inst.mark_unread());
    env.view.trash_session_by_id(&trashed);
    assert!(env.view.get_instance(&trashed).unwrap().is_trashed());
    assert!(
        env.view.get_instance(&trashed).unwrap().is_unread(),
        "the trashed row must still carry the unread flag for this regression"
    );

    env.view.select_session_by_id(&active);
    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(trashed.as_str()),
        "`w` must not land on a trashed session even when it is unread"
    );
}

/// Snooze is the same "don't bother me" sink state as trash/archive for
/// every other subsystem (see `Instance::is_snoozed`); `w`'s forward walk
/// (pass 1) must skip a snoozed session even though it is otherwise
/// `Status::Waiting`, exactly the state `w` is looking for. A third,
/// eligible `Status::Waiting` control session proves the walk actually
/// found and landed on a real target rather than merely failing to land
/// on the snoozed one (with only two sessions "landed != snoozed" would
/// hold trivially even if `w` did nothing at all).
#[test]
#[serial]
fn w_skips_snoozed_waiting_session() {
    use crate::session::Status;

    let mut env = create_test_env_with_sessions(3);
    env.view.strict_hotkeys = false;

    let snoozed = env.view.instances[0].id.clone();
    let active = env.view.instances[1].id.clone();
    let control = env.view.instances[2].id.clone();
    // The active row is a plain idle session the cursor starts on; the
    // control row is a legitimate, non-dismissed Waiting session `w` should
    // land on; the snoozed row is otherwise Waiting too, as it would be if
    // it started waiting on input before being snoozed.
    env.view
        .mutate_instance(&active, |inst| inst.status = Status::Idle);
    env.view
        .mutate_instance(&control, |inst| inst.status = Status::Waiting);
    env.view.mutate_instance(&snoozed, |inst| {
        inst.status = Status::Waiting;
        inst.snooze(30);
    });
    assert!(env.view.get_instance(&snoozed).unwrap().is_snoozed());

    env.view.select_session_by_id(&active);
    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(snoozed.as_str()),
        "`w` must not land on a snoozed session even though it is Waiting"
    );
    assert_eq!(
        landed.as_deref(),
        Some(control.as_str()),
        "`w` must land on the eligible Waiting session, proving the forward walk actually ran"
    );
}

/// Same contract as `w_skips_snoozed_waiting_session`, but for the pass-2
/// idle fallback: with the only Idle candidate snoozed, `w` must not treat
/// it as the "most-recently-accessed Idle session" fallback target.
#[test]
#[serial]
fn w_skips_snoozed_idle_session_in_fallback() {
    let (mut env, running, idle) = attention_env_running_then_idle();
    let running_id = match env.view.flat_items.get(running) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the running row to be a session item"),
    };
    let idle_id = match env.view.flat_items.get(idle) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the idle row to be a session item"),
    };
    env.view.mutate_instance(&idle_id, |inst| inst.snooze(30));
    assert!(env.view.get_instance(&idle_id).unwrap().is_snoozed());

    env.view.cursor = running;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(idle_id.as_str()),
        "`w` must not fall back to a snoozed session even when it is the only Idle candidate"
    );
    assert_eq!(
        landed.as_deref(),
        Some(running_id.as_str()),
        "with no eligible target, `w` must leave the cursor on the Running session"
    );
}

/// Archive is the same "don't bother me" sink state as trash/snooze (see
/// `Instance::archive`'s mutual-exclusion doc comment: archive is "the
/// strongest dismiss"); `w`'s forward walk (pass 1) must skip an archived
/// session even though it is otherwise `Status::Waiting`, exactly the state
/// `w` is looking for. A third, eligible `Status::Waiting` control session
/// proves the walk actually found and landed on a real target rather than
/// merely failing to land on the archived one (with only two sessions
/// "landed != archived" would hold trivially even if `w` did nothing at
/// all).
#[test]
#[serial]
fn w_skips_archived_waiting_session() {
    use crate::session::Status;

    let mut env = create_test_env_with_sessions(3);
    env.view.strict_hotkeys = false;
    // Keep the Archived section expanded so the archived row lands in
    // `flat_items`; that is the only way `w`'s walk could reach it.
    env.view.archived_section_collapsed = false;

    let archived = env.view.instances[0].id.clone();
    let active = env.view.instances[1].id.clone();
    let control = env.view.instances[2].id.clone();
    // The active row is a plain idle session the cursor starts on; the
    // control row is a legitimate, non-dismissed Waiting session `w` should
    // land on; the archived row is otherwise Waiting too, as it would be if
    // it started waiting on input before being archived.
    env.view
        .mutate_instance(&active, |inst| inst.status = Status::Idle);
    env.view
        .mutate_instance(&control, |inst| inst.status = Status::Waiting);
    env.view.mutate_instance(&archived, |inst| {
        inst.status = Status::Waiting;
        inst.archive();
    });
    assert!(env.view.get_instance(&archived).unwrap().is_archived());

    env.view.select_session_by_id(&active);
    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(archived.as_str()),
        "`w` must not land on an archived session even though it is Waiting"
    );
    assert_eq!(
        landed.as_deref(),
        Some(control.as_str()),
        "`w` must land on the eligible Waiting session, proving the forward walk actually ran"
    );
}

/// Same contract as `w_skips_archived_waiting_session`, but for the pass-2
/// idle fallback: with the only Idle candidate archived, `w` must not treat
/// it as the "most-recently-accessed Idle session" fallback target.
#[test]
#[serial]
fn w_skips_archived_idle_session_in_fallback() {
    let (mut env, running, idle) = attention_env_running_then_idle();
    let running_id = match env.view.flat_items.get(running) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the running row to be a session item"),
    };
    let idle_id = match env.view.flat_items.get(idle) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the idle row to be a session item"),
    };
    // `mutate_instance` updates the instance in place without rebuilding
    // `flat_items`, so the now-archived row stays at its original index
    // (pass 2 re-derives its dismissed state from the live instance, not
    // from `flat_items`, so this still exercises the real code path).
    env.view.mutate_instance(&idle_id, |inst| inst.archive());
    assert!(env.view.get_instance(&idle_id).unwrap().is_archived());

    env.view.cursor = running;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(idle_id.as_str()),
        "`w` must not fall back to an archived session even when it is the only Idle candidate"
    );
    assert_eq!(
        landed.as_deref(),
        Some(running_id.as_str()),
        "with no eligible target, `w` must leave the cursor on the Running session"
    );
}

#[test]
#[serial]
fn d_on_session_with_default_trash_persists_trash_marker() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.selected_session.clone().unwrap();

    env.view.handle_key(key(KeyCode::Char('d')), None);

    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "pressing d with default trash-first config must mark the row trashed in memory"
    );
    assert!(
        env.view.unified_delete_dialog.is_none(),
        "trash-first d must not open the permanent-delete dialog"
    );
    let disk_row = Storage::new_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|inst| inst.id == id)
        .expect("disk row present");
    assert!(
        disk_row.is_trashed(),
        "pressing d must persist trashed_at so a storage refresh cannot resurrect a killed session"
    );
}

/// With `session.confirm_delete` on, `d` opens a confirmation dialog and does
/// not trash until the dialog is accepted; accepting then runs the same trash
/// path as the instant flow. See #2583.
#[test]
#[serial]
fn d_with_confirm_delete_prompts_before_trashing() {
    let mut env = create_test_env_with_sessions(2);
    enable_confirm_delete();
    let id = env.view.selected_session.clone().unwrap();

    env.view.handle_key(key(KeyCode::Char('d')), None);

    assert!(
        !env.view.get_instance(&id).unwrap().is_trashed(),
        "confirm_delete on must not trash the session on the keystroke"
    );
    let dialog = env
        .view
        .confirm_dialog
        .as_ref()
        .expect("confirm_delete on must open a confirmation dialog");
    assert_eq!(dialog.action(), "trash_session");
    assert_eq!(
        env.view.pending_trash_session.as_deref(),
        Some(id.as_str()),
        "the pending trash target must be the selected session"
    );

    // Accepting the dialog trashes via the same trash_session_by_id path.
    env.view.dispatch_confirm_submit("trash_session");
    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "accepting the confirm dialog must trash the session"
    );
    assert!(
        env.view.pending_trash_session.is_none(),
        "the pending trash target must be cleared once consumed"
    );
}

/// Cancelling the `session.confirm_delete` dialog leaves the session untouched
/// and clears the pending target. See #2583.
#[test]
#[serial]
fn confirm_delete_dialog_cancel_leaves_session() {
    let mut env = create_test_env_with_sessions(2);
    enable_confirm_delete();
    let id = env.view.selected_session.clone().unwrap();

    env.view.handle_key(key(KeyCode::Char('d')), None);
    assert!(env.view.confirm_dialog.is_some());

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(
        env.view.confirm_dialog.is_none(),
        "Esc must dismiss the confirm dialog"
    );
    assert!(
        !env.view.get_instance(&id).unwrap().is_trashed(),
        "cancelling the confirm dialog must not trash the session"
    );
    assert!(
        env.view.pending_trash_session.is_none(),
        "cancelling must clear the pending trash target"
    );
}

/// When no session is selected, the toggle is a silent no-op.
#[test]
#[serial]
fn toggle_archive_at_cursor_noop_with_no_selection() {
    let mut env = create_test_env_empty();
    env.view.selected_session = None;
    env.view.toggle_archive_at_cursor().unwrap();
}

/// Archiving in the default (non-Attention) sort advances the cursor to the
/// next active session below instead of following the archived row into the
/// Archived section. The section is NOT auto-revealed; its header count is
/// the feedback. The preview follows the new selection through the normal
/// per-frame retarget (cache gates on session id, worker drops stale frames).
#[test]
#[serial]
fn archive_advances_cursor_to_next_session() {
    let mut env = create_test_env_with_sessions(3);
    // Start with the Archived section collapsed (the default).
    env.view.archived_section_collapsed = true;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();
    let next_id = match env.view.flat_items.get(1) {
        Some(Item::Session { id, .. }) => id.clone(),
        other => panic!("expected a session row below the cursor, got {other:?}"),
    };

    env.view.toggle_archive_at_cursor().unwrap();

    assert!(
        env.view.get_instance(&id).unwrap().is_archived(),
        "the session must be archived"
    );
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(next_id.as_str()),
        "selection must advance to the next active session"
    );
    assert!(
        env.view.archived_section_collapsed,
        "single-row archive must not auto-reveal the Archived section"
    );
    match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id: cur, .. }) => {
            assert_eq!(cur, &next_id, "cursor must sit on the next session's row")
        }
        _ => panic!("cursor should be on the next session row"),
    }
}

/// Archiving the bottom session has no row below to advance to, so the
/// cursor falls back to the nearest active session above.
#[test]
#[serial]
fn archive_bottom_row_falls_back_to_session_above() {
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = true;
    let last = env.view.flat_items.len() - 1;
    env.view.cursor = last;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();
    let above_id = match env.view.flat_items.get(last - 1) {
        Some(Item::Session { id, .. }) => id.clone(),
        other => panic!("expected a session row above the cursor, got {other:?}"),
    };

    env.view.toggle_archive_at_cursor().unwrap();

    assert!(env.view.get_instance(&id).unwrap().is_archived());
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(above_id.as_str()),
        "with nothing below, selection must land on the session above"
    );
}

/// Archiving the only active session leaves nothing to advance to: the
/// cursor clamps into the remaining list (the Archived section header) and
/// the selection clears instead of pointing at a vanished row.
#[test]
#[serial]
fn archive_last_active_session_clears_selection() {
    let mut env = create_test_env_with_sessions(1);
    env.view.archived_section_collapsed = true;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();

    env.view.toggle_archive_at_cursor().unwrap();

    assert!(env.view.get_instance(&id).unwrap().is_archived());
    assert_eq!(
        env.view.selected_session, None,
        "no active session remains, so nothing should be selected"
    );
    assert!(
        env.view.cursor < env.view.flat_items.len(),
        "cursor must stay clamped inside the rebuilt list"
    );
}

/// The successor scan must skip rows already parked under an EXPANDED
/// Archived section: archiving the last active row with an archived row
/// visible below clears the selection instead of advancing into the section.
#[test]
#[serial]
fn archive_successor_skips_archived_rows() {
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = false;

    // Park the second session first, so an archived row sits below.
    let parked_id = match env.view.flat_items.get(1) {
        Some(Item::Session { id, .. }) => id.clone(),
        other => panic!("expected a second session row, got {other:?}"),
    };
    env.view.select_session_by_id(&parked_id);
    env.view.toggle_archive_at_cursor().unwrap();
    assert!(env.view.get_instance(&parked_id).unwrap().is_archived());

    // Archive the remaining active session. The only session row left below
    // the cursor is the parked one, which must NOT become the selection.
    let id = env.view.selected_session.clone().unwrap();
    assert_ne!(
        id, parked_id,
        "selection must have fallen back to the active row"
    );
    env.view.toggle_archive_at_cursor().unwrap();

    assert!(env.view.get_instance(&id).unwrap().is_archived());
    assert_eq!(
        env.view.selected_session, None,
        "the cursor must not advance onto a row inside the Archived section"
    );
}

/// Attention sort: archiving the only active session with the Archived
/// section collapsed leaves no session row for `select_top_attention` to
/// land on. Selection must clear (not stay pinned to the invisible archived
/// row) and the cursor must clamp into the shrunken list.
#[test]
#[serial]
fn archive_last_active_session_attention_sort_clears_selection() {
    let mut env = create_test_env_with_sessions(1);
    env.view.sort_order = crate::session::config::SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    env.view.archived_section_collapsed = true;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();

    env.view.toggle_archive_at_cursor().unwrap();

    assert!(env.view.get_instance(&id).unwrap().is_archived());
    assert_eq!(
        env.view.selected_session, None,
        "selection must not point at the archived row hidden in the collapsed section"
    );
    assert!(
        env.view.cursor < env.view.flat_items.len(),
        "cursor must stay clamped inside the rebuilt list"
    );
}

/// Restoring with `z` unarchives the row and keeps it selected, following it
/// back to its real tier. Unarchive does not restart the agent: the row stays
/// Stopped (archive killed its pane) and the user restarts with `e`.
#[test]
#[serial]
fn unarchive_keeps_selection() {
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = false;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();

    env.view.toggle_archive_at_cursor().unwrap();
    assert!(env.view.get_instance(&id).unwrap().is_archived());

    // The archive advanced the cursor to the neighbor; navigate back onto
    // the archived row (visible because the section is expanded) to restore.
    env.view.select_session_by_id(&id);
    env.view.toggle_archive_at_cursor().unwrap();
    assert!(
        !env.view.get_instance(&id).unwrap().is_archived(),
        "second toggle unarchives"
    );
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(id.as_str()),
        "unarchived row stays selected"
    );
    match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id: cur, .. }) => {
            assert_eq!(cur, &id, "cursor follows the unarchived row")
        }
        _ => panic!("cursor should be on the unarchived session row"),
    }
}

/// `restart_selected_session` must drop the press silently when nothing is
/// selected. No restart_with_size call, no save, no cooldown insertion.
#[test]
#[serial]
fn restart_selected_session_noop_with_no_selection() {
    let mut env = create_test_env_empty();
    env.view.selected_session = None;
    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(env.view.restart_cooldown_at.is_empty());
}

/// Sunk rows (`archived` / `snoozed` / `pane_dead_observed`) and transient
/// lifecycle states (`Creating` / `Deleting`) must skip the restart path.
/// Archive's contract is "don't auto-revive"; restart should respect that.
#[test]
#[serial]
fn restart_selected_session_skips_archived_row() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.mutate_instance(&id, |inst| inst.archive());

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(
        env.view.instance_at(0).is_archived(),
        "archive bit should still be set: restart must not unarchive"
    );
    assert!(
        env.view.restart_cooldown_at.is_empty(),
        "cooldown should not be set on a skipped restart"
    );
}

#[test]
#[serial]
fn restart_selected_session_skips_snoozed_row_in_attention_sort() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.sort_order = SortOrder::Attention;
    env.view.mutate_instance(&id, |inst| inst.snooze(30));

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(
        env.view.instance_at(0).is_snoozed(),
        "Attention sort: snooze is the user's explicit `don't revive`; restart must not clear it"
    );
    assert!(
        env.view.restart_cooldown_at.is_empty(),
        "Attention sort: skipped restart should not set the cooldown"
    );
}

/// Outside Attention sort, the snooze badge / dim styling / `z ` prefix
/// are all invisible, so silently swallowing a restart press on a snoozed
/// row would leave the user staring at an apparently-restartable row that
/// doesn't restart. Wake the snooze and let the restart proceed instead.
#[test]
#[serial]
fn restart_selected_session_wakes_snooze_outside_attention_sort() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.sort_order = SortOrder::Newest;
    env.view.mutate_instance(&id, |inst| inst.snooze(30));
    assert!(env.view.instance_at(0).is_snoozed(), "pre-condition");

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(
        !env.view.instance_at(0).is_snoozed(),
        "Newest sort: restart on a snoozed row must clear the snooze so persisted state matches what's on screen"
    );
    // Restart cooldown gets set because the press wasn't dropped. Bare
    // `restart_selected_session` schedules the actual restart on a
    // worker; we only assert the synchronous bookkeeping here.
    assert!(
        env.view.restart_cooldown_at.contains_key(&id),
        "Newest sort: restart that proceeded must record the cooldown"
    );
}

#[test]
#[serial]
fn restart_selected_session_skips_creating_row() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view
        .mutate_instance(&id, |inst| inst.status = crate::session::Status::Creating);

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(env.view.restart_cooldown_at.is_empty());
}

/// The cooldown map debounces rapid presses. A second press within the
/// cooldown window must be dropped before the restart_with_size call
/// would otherwise tear down a still-booting tmux pane.
///
/// We cannot exercise the full restart path under unit tests (no tmux),
/// so this test confirms the cooldown bookkeeping: after the first call
/// inserts an entry, a second call with the same id within the window
/// returns immediately and does not overwrite the timestamp.
#[test]
#[serial]
fn restart_selected_session_debounces_via_cooldown_map() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    // Seed the cooldown so the next press is debounced. This stands in
    // for the "first restart already ran" precondition: we cannot run
    // restart_with_size in a unit test (no tmux), but the debounce check
    // happens before that, on the cooldown map.
    let now = std::time::Instant::now();
    env.view.restart_cooldown_at.insert(id.clone(), now);

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    let stored = env.view.restart_cooldown_at.get(&id).copied().unwrap();
    assert_eq!(
        stored, now,
        "cooldown timestamp must not be overwritten on a debounced press"
    );
}

#[test]
#[serial]
fn restart_selected_session_surfaces_resume_failed_after_async_restart() {
    if crate::tmux::tmux_command().arg("-V").output().is_err() {
        eprintln!("Skipping: tmux not available");
        return;
    }

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let profile = "restart-resume-failed";
    let storage = Storage::new_unwatched(profile).unwrap();
    let stale_sid = "11111111-2222-3333-4444-555555555555";

    let mut inst = Instance::new("restart-resume-failed", "/tmp/x");
    inst.source_profile = profile.to_string();
    inst.tool = "claude".to_string();
    inst.command = "/bin/false".to_string();
    inst.agent_session_id = Some(stale_sid.to_string());
    let id = inst.id.clone();
    let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    let _ = crate::tmux::tmux_command()
        .args(["kill-session", "-t", &tmux_name])
        .output();

    storage
        .update(|instances, groups| {
            *instances = vec![inst.clone()];
            *groups = GroupTree::new_with_groups(std::slice::from_ref(&inst), &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    // A real prior conversation on disk so the restart drives the --resume
    // cascade (and its ResumeFailed path). A stored sid with no transcript now
    // launches fresh-pinned (`--session-id`), which would not surface here.
    let claude_dir = temp.path().join(".claude").join("projects").join(
        crate::session::capture::encode_claude_project_path("/tmp/x"),
    );
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join(format!("{stale_sid}.jsonl")), "seed\n").unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some(profile.to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.update_selected();
    view.selected_session = Some(id.clone());

    let result = view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());

    let mut applied = false;
    for _ in 0..120 {
        if view.apply_restart_results() {
            applied = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = crate::tmux::tmux_command()
        .args(["kill-session", "-t", &tmux_name])
        .output();

    assert!(applied, "timed out waiting for async restart result");
    let dialog = view.info_dialog.as_ref().expect("resume failure dialog");
    assert_eq!(dialog.title(), "Restart Failed");
    assert!(dialog.message().contains(stale_sid));
    let row = view.get_instance(&id).expect("instance remains visible");
    assert_eq!(row.agent_session_id.as_deref(), Some(stale_sid));
    assert_eq!(row.resume_probe_failed_sid.as_deref(), Some(stale_sid));
    assert_eq!(row.status, crate::session::Status::Error);
    assert!(row.last_accessed_at.is_some());
}

#[test]
#[serial]
fn apply_restart_results_preserves_peer_sid_and_marker() {
    use crate::session::StartOutcome;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.restart_in_flight.insert(id.clone());
    env.view.instance_at_mut(0).agent_session_id = Some("peer-fresh-sid".to_string());
    env.view.instance_at_mut(0).resume_probe_failed_sid = Some("peer-fresh-sid".to_string());

    let mut worker = env.view.instance_at(0).clone();
    worker.status = crate::session::Status::Error;
    worker.agent_session_id = Some("phase1-stale-sid".to_string());
    worker.resume_probe_failed_sid = Some("phase1-stale-sid".to_string());
    worker.last_error =
        Some("resume failed for sid phase1-stale-sid; preserved for explicit retry".to_string());

    env.view.restart_poller = crate::tui::restart_poller::RestartPoller::with_result_for_test(
        crate::session::restart::RestartResult {
            session_id: id.clone(),
            before: Box::new(worker.clone()),
            instance: Box::new(worker),
            outcome: Ok(StartOutcome::ResumeFailed {
                sid: "phase1-stale-sid".to_string(),
            }),
        },
    );

    assert!(env.view.apply_restart_results());

    let row = env
        .view
        .get_instance(&id)
        .expect("instance remains visible");
    assert_eq!(row.status, crate::session::Status::Error);
    assert_eq!(row.agent_session_id.as_deref(), Some("peer-fresh-sid"));
    assert_eq!(
        row.resume_probe_failed_sid.as_deref(),
        Some("peer-fresh-sid")
    );
    assert!(env.view.restart_in_flight.is_empty());
    let dialog = env
        .view
        .info_dialog
        .as_ref()
        .expect("resume failure dialog");
    assert!(dialog.message().contains("phase1-stale-sid"));
}

#[test]
#[serial]
fn apply_restart_results_propagates_worker_sid_without_peer_write() {
    use crate::session::StartOutcome;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.restart_in_flight.insert(id.clone());
    env.view.instance_at_mut(0).agent_session_id = Some("sid-before".to_string());

    let before = env.view.instance_at(0).clone();
    let mut worker = before.clone();
    worker.agent_session_id = Some("sid-after".to_string());
    worker.status = crate::session::Status::Running;

    env.view.restart_poller = crate::tui::restart_poller::RestartPoller::with_result_for_test(
        crate::session::restart::RestartResult {
            session_id: id.clone(),
            before: Box::new(before),
            instance: Box::new(worker),
            outcome: Ok(StartOutcome::Resumed),
        },
    );

    assert!(env.view.apply_restart_results());

    let row = env
        .view
        .get_instance(&id)
        .expect("instance remains visible");
    assert_eq!(row.status, crate::session::Status::Running);
    assert_eq!(row.agent_session_id.as_deref(), Some("sid-after"));
    assert_eq!(row.resume_probe_failed_sid, None);
    assert!(env.view.restart_in_flight.is_empty());
}

#[test]
#[serial]
fn execute_send_message_missing_session_shows_send_failed() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.instances.shift_remove(&id);

    env.view.execute_send_message(&id, "hello");

    let dialog = env.view.info_dialog.as_ref().expect("send failure dialog");
    assert_eq!(dialog.title(), "Send Failed");
    assert_eq!(
        dialog.message(),
        "Session disappeared before the message could be sent."
    );
}

/// A second restart press while the first cascade is still running on the
/// poller worker must be dropped. The cascade is off the event loop, so the
/// 1.5s keyboard-repeat debounce does not cover a deliberate press during a
/// multi-second pull; without the in-flight guard the worker would enqueue a
/// duplicate request and restart the row twice.
#[test]
#[serial]
fn restart_selected_session_skips_when_already_in_flight() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.restart_in_flight.insert(id.clone());

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(
        env.view.restart_cooldown_at.is_empty(),
        "an in-flight restart must drop the press before any bookkeeping"
    );
    assert_ne!(
        env.view.instance_at(0).status,
        crate::session::Status::Starting,
        "the row must not be re-flipped to Starting by a dropped duplicate press"
    );
}

/// Deleting a row whose restart cascade is still running would fire docker
/// commands against the container the worker is mid-creating. The delete must
/// be refused (and surfaced) rather than racing the restart worker.
#[test]
#[serial]
fn delete_selected_refused_during_restart() {
    use crate::tui::dialogs::DeleteOptions;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.restart_in_flight.insert(id.clone());

    let result = env.view.delete_selected(&DeleteOptions::default());
    assert!(result.is_ok());
    assert_ne!(
        env.view.instance_at(0).status,
        crate::session::Status::Deleting,
        "delete must be refused while a restart is in flight"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "the refused delete must surface a dialog, not silently no-op"
    );
}

/// Build a HomeView seeded with two distinct projects, each containing
/// sessions with different attention statuses. Helper for the Project +
/// Attention combination tests below.
fn create_test_env_two_projects_mixed_attention() -> TestEnv {
    use crate::session::Status;
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut alpha_waiting = Instance::new("alpha-waiting", "/repos/alpha");
    alpha_waiting.status = Status::Waiting;
    let mut alpha_running = Instance::new("alpha-running", "/repos/alpha");
    alpha_running.status = Status::Running;

    let mut beta_running = Instance::new("beta-running", "/repos/beta");
    beta_running.status = Status::Running;
    let mut beta_error = Instance::new("beta-error", "/repos/beta");
    beta_error.status = Status::Error;

    let instances = vec![alpha_waiting, alpha_running, beta_running, beta_error];
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    TestEnv {
        view,
        _guard,
        _temp: temp,
    }
}

/// Project grouping must survive Attention sort. Previously `build_flat_items`
/// short-circuited on `SortOrder::Attention` before checking `GroupByMode`,
/// flattening the list and dropping project headers. The headers are the
/// whole point of project mode; users want attention triage WITHIN their
/// project boundaries, not a flat firehose across projects.
#[test]
#[serial]
fn project_grouping_survives_attention_sort() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();

    let group_count = env
        .view
        .flat_items
        .iter()
        .filter(|i| matches!(i, Item::Group { .. }))
        .count();
    assert_eq!(
        group_count, 2,
        "Project + Attention must keep both project headers (alpha, beta), \
         got flat_items: {:?}",
        env.view.flat_items
    );

    let group_names: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        group_names.iter().any(|n| n == "alpha") && group_names.iter().any(|n| n == "beta"),
        "expected alpha and beta project headers, got {group_names:?}"
    );
}

/// Within a project group under Attention sort, sessions must order by
/// attention tier: Waiting (tier 0) above Running (tier 4). Confirms that
/// the existing `sort_sessions` helper, already reached by the project
/// flatten path via `flatten_tree`, is doing its job once we stopped
/// short-circuiting it.
#[test]
#[serial]
fn project_grouping_sorts_sessions_by_attention_within_group() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();

    let mut current_group: Option<String> = None;
    let mut alpha_session_order: Vec<String> = Vec::new();
    for item in &env.view.flat_items {
        match item {
            Item::Group { name, .. } => current_group = Some(name.clone()),
            Item::Session { id, .. } => {
                if current_group.as_deref() == Some("alpha") {
                    if let Some(inst) = env.view.instances.get(id) {
                        alpha_session_order.push(inst.title.clone());
                    }
                }
            }
        }
    }
    assert_eq!(
        alpha_session_order,
        vec!["alpha-waiting".to_string(), "alpha-running".to_string()],
        "Waiting session must rank above Running within the alpha group"
    );
}

/// The most-attention-urgent project floats to the top. `attention_group_key`
/// scores groups by their best member's tier; beta has an Error (tier 1)
/// while alpha's best is Waiting (tier 0), so alpha sorts first. This
/// confirms that the existing group-sort path is reached for project mode
/// under Attention sort.
#[test]
#[serial]
fn project_groups_sort_by_top_attention_member() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();

    let group_order: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        group_order,
        vec!["alpha".to_string(), "beta".to_string()],
        "alpha (Waiting=tier 0) must sort above beta (Error=tier 1)"
    );
}

/// Archiving a project header while in Attention sort must remove the project
/// from the main flow once all of its live sessions are archived. The archived
/// rows still appear under the synthetic Archived section's project sub-header.
#[test]
#[serial]
fn project_attention_archive_selected_group_removes_empty_main_header() {
    use crate::session::{
        archived_project_sub_path,
        config::{GroupByMode, SortOrder},
        is_within_archived_section,
    };

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Attention;
    env.view.archived_section_collapsed = true;
    env.view.flat_items = env.view.build_flat_items();

    let beta_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| {
            matches!(
                i,
                Item::Group { name, path, .. }
                    if name == "beta" && !is_within_archived_section(path)
            )
        })
        .expect("beta project header present");
    env.view.cursor = beta_idx;
    env.view.update_selected();
    assert_eq!(env.view.selected_group.as_deref(), Some("beta"));

    env.view.archive_selected_group().unwrap();

    assert!(
        env.view
            .instances
            .values()
            .filter(|i| super::project_group_name(i) == "beta")
            .all(|i| i.is_archived()),
        "all beta sessions must be archived"
    );
    assert!(
        !env.view.flat_items.iter().any(|item| matches!(
            item,
            Item::Group { name, path, .. }
                if name == "beta" && !is_within_archived_section(path)
        )),
        "archived-only beta must not leave a main-flow project header; got flat_items: {:?}",
        env.view.flat_items
    );
    let archived_beta = archived_project_sub_path("beta");
    assert!(
        env.view.flat_items.iter().any(|item| matches!(
            item,
            Item::Group { path, name, session_count, .. }
                if path == &archived_beta && name == "beta" && *session_count == 2
        )),
        "archived beta sessions must stay reachable under the Archived section; got flat_items: {:?}",
        env.view.flat_items
    );
}

/// A registered (pinned) project with no sessions surfaces as an empty
/// header in project view, mirroring the WebUI where an empty project is just
/// a registry entry decoupled from sessions. This is the core of #2047.
#[test]
#[serial]
fn pinned_project_without_sessions_shows_empty_header() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let mut env = create_test_env_two_projects_mixed_attention();
    // Register a project that has no sessions at all. The path need not exist;
    // `add` falls back to the literal path when canonicalization fails.
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("gamma", "/repos/gamma", ProjectScope::Global).with_pinned(true),
        false,
    )
    .unwrap();

    env.view.group_by = GroupByMode::Project;
    env.view.refresh_registered_projects();
    env.view.flat_items = env.view.build_flat_items();

    let group_names: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        group_names.iter().any(|n| n == "gamma"),
        "pinned empty project gamma must show as a header, got {group_names:?}"
    );
    assert!(env.view.is_project_label_pinned("gamma"));
}

/// Pressing `p` on a project header pins it (registers the repo) instead of
/// opening the projects dialog; the pin toggle binding wins the shared chord
/// because a project header is selected.
#[test]
#[serial]
fn p_key_pins_project_on_header() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    let alpha_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "alpha"))
        .expect("alpha header present");
    env.view.cursor = alpha_idx;
    env.view.update_selected();

    assert!(!env.view.is_project_label_pinned("alpha"));
    env.view.handle_key(key(KeyCode::Char('p')), None);
    assert!(
        env.view.is_project_label_pinned("alpha"),
        "p on a project header should pin it"
    );
    // The pin path must not open the projects dialog (the chord is shared).
    assert!(env.view.projects_dialog.is_none());
    // A successful pin stays quiet: the header's pin icon is feedback enough,
    // so there is no dialog to dismiss.
    assert!(
        env.view.info_dialog.is_none(),
        "a successful pin must not raise an info dialog"
    );

    // Unpinning (a second toggle) clears the pin but KEEPS the saved project,
    // so the entry stays in the registry (only an explicit remove deletes it).
    // See #2208.
    env.view.toggle_project_pin_at_cursor();
    assert!(!env.view.is_project_label_pinned("alpha"));
    // A successful unpin is likewise quiet.
    assert!(
        env.view.info_dialog.is_none(),
        "a successful unpin must not raise an info dialog"
    );
    // The specific entry is kept (not just "registry non-empty") with its pin
    // flag cleared: unpin keeps the saved project, only Remove deletes it.
    let after = crate::session::projects::load_global().unwrap();
    assert_eq!(after.len(), 1, "unpin must keep the registry entry");
    assert_eq!(after[0].name, "alpha");
    assert!(!after[0].pinned, "unpin must clear the pin flag");
}

/// Off a project header (here: in Manual grouping), `p` keeps its original
/// meaning and opens the projects dialog, so the overload doesn't shadow the
/// global binding.
#[test]
#[serial]
fn p_key_opens_projects_dialog_off_project_header() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Manual;
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('p')), None);
    assert!(
        env.view.projects_dialog.is_some(),
        "p off a project header should open the projects dialog"
    );
}

/// Pin a project, archive its only session, then unpin: the empty header must
/// leave the main flow (the archived session stays under the Archived section).
#[test]
#[serial]
fn unpin_archived_only_project_leaves_main_flow() {
    use crate::session::{config::GroupByMode, is_within_archived_section};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    // Pin beta.
    let beta_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "beta"))
        .expect("beta header present");
    env.view.cursor = beta_idx;
    env.view.update_selected();
    env.view.toggle_project_pin_at_cursor();
    assert!(env.view.is_project_label_pinned("beta"));

    // Archive both beta sessions.
    let beta_ids: Vec<String> = env
        .view
        .instances
        .values()
        .filter(|i| super::project_group_name(i) == "beta")
        .map(|i| i.id.clone())
        .collect();
    for id in &beta_ids {
        env.view
            .apply_user_action(id, |inst| inst.archive())
            .unwrap();
    }
    env.view.flat_items = env.view.build_flat_items();

    // Now unpin via the cursor on the empty main-flow beta header.
    let beta_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, path, .. } if name == "beta" && !is_within_archived_section(path)))
        .expect("empty beta header present in main flow after archiving");
    env.view.cursor = beta_idx;
    env.view.update_selected();
    env.view.toggle_project_pin_at_cursor();

    assert!(
        !env.view.is_project_label_pinned("beta"),
        "beta must read as unpinned after the toggle; registry still has it"
    );

    // Count beta headers OUTSIDE the Archived section.
    let mut in_archived = false;
    let mut main_beta = 0;
    for item in &env.view.flat_items {
        if let Item::Group { path, name, .. } = item {
            if is_within_archived_section(path) {
                in_archived = true;
            } else if name == "beta" && !in_archived {
                main_beta += 1;
            }
        }
    }
    assert_eq!(
        main_beta, 0,
        "unpinned archived-only beta must not render in the main flow; got: {:?}",
        env.view.flat_items
    );
}

/// A registry entry whose path differs from an archived session's repo path
/// sharing the same basename must still read as pinned and be unpinnable.
/// The empty header is surfaced by LABEL match (`unpopulated_projects`), so
/// pin state and the unpin toggle must resolve by the same rule. Previously
/// `project_header_repo_path` let the archived row lend the header its path:
/// the path comparison failed (repo gone from disk, `canonical_key` compares
/// raw strings), the header read as unpinned, and `p` routed to the pin
/// branch and died on the name conflict, leaving a phantom header the user
/// could not clear.
#[test]
#[serial]
fn stale_registry_entry_with_mismatched_archived_path_stays_pinned_and_unpinnable() {
    use crate::session::config::GroupByMode;
    use crate::session::is_within_archived_section;
    use crate::session::projects::{self, Project, ProjectScope};
    use crate::session::Status;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    // A live session in another project, plus an ARCHIVED session whose repo
    // basename is "otari" but whose recorded path differs from the registry
    // entry below (repo deleted/moved, so neither canonicalizes).
    let mut alpha = Instance::new("alpha-running", "/repos/alpha");
    alpha.status = Status::Running;
    let mut orphan = Instance::new("otari-old", "/old/home/otari");
    orphan.status = Status::Stopped;
    orphan.archive();

    let instances = vec![alpha, orphan];
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    // Stale registry entry: same basename, different (nonexistent) path.
    // Pinned, so it surfaces as an empty header (#2208).
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("otari", "/repos/otari", ProjectScope::Global).with_pinned(true),
        false,
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    view.group_by = GroupByMode::Project;
    view.refresh_registered_projects();
    view.flat_items = view.build_flat_items();

    // 1. The phantom: an empty otari header renders in the main flow.
    let otari_idx = view.flat_items.iter().position(|i| {
        matches!(i, Item::Group { name, path, .. }
            if name == "otari" && !is_within_archived_section(path))
    });
    assert!(
        otari_idx.is_some(),
        "stale registry entry must surface an empty otari header; got {:?}",
        view.flat_items
    );

    // 2. With no live session populating the label, pin state resolves by
    //    label, the same rule that injected the header.
    assert!(
        view.is_project_label_pinned("otari"),
        "registry-backed empty header must read as pinned even when an \
         archived session recorded a different path for the same basename"
    );

    // 3. `p` on the header routes to the unpin branch and clears it.
    view.cursor = otari_idx.unwrap();
    view.update_selected();
    view.toggle_project_pin_at_cursor();

    let still_there = view.flat_items.iter().any(|i| {
        matches!(i, Item::Group { name, path, .. }
            if name == "otari" && !is_within_archived_section(path))
    });
    assert!(
        !still_there,
        "unpin must drop the empty header from the main flow; got {:?}",
        view.flat_items
    );
    // Unpin clears the flag but KEEPS the saved project (#2208): the entry
    // survives, now unpinned, so it stays in the Projects view / wizard.
    let after = projects::load_global().unwrap();
    assert_eq!(after.len(), 1, "unpin must keep the registry entry");
    assert!(!after[0].pinned, "unpin must clear the pin flag");
    // The archived session itself is untouched; it stays under Archived.
    assert!(
        view.flat_items
            .iter()
            .any(|i| { matches!(i, Item::Group { path, .. } if is_within_archived_section(path)) }),
        "archived section still present"
    );
}

/// The pin must persist a project across its last session leaving the view:
/// once pinned, the header remains even when no sessions reference it. This
/// is the user-visible promise of #2047.
#[test]
#[serial]
fn pinned_project_survives_losing_last_session() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    let alpha_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "alpha"))
        .expect("alpha header present");
    env.view.cursor = alpha_idx;
    env.view.update_selected();
    env.view.toggle_project_pin_at_cursor();
    assert!(env.view.is_project_label_pinned("alpha"));

    // Drop every alpha session, then rebuild as a reload would. The registry
    // entry keeps the header alive even with zero members.
    env.view
        .instances
        .retain(|_, i| super::project_group_name(i) != "alpha");
    env.view.flat_items = env.view.build_flat_items();

    let alpha_header = env.view.flat_items.iter().find_map(|i| match i {
        Item::Group {
            name,
            session_count,
            ..
        } if name == "alpha" => Some(*session_count),
        _ => None,
    });
    assert_eq!(
        alpha_header,
        Some(0),
        "pinned alpha must remain as an empty (0) header after losing its sessions"
    );
}

/// Two repos that share a basename are judged independently for pinning: a
/// header whose own repo is not registered must read as unpinned even when a
/// different same-basename repo is in the registry. Guards the path-keyed pin
/// identity (CodeRabbit #2055).
#[test]
#[serial]
fn same_basename_repos_pin_independently() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let mut env = create_test_env_empty();
    // Register `/work/api`, but the visible header's repo is `/other/api`.
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("api", "/work/api", ProjectScope::Global),
        false,
    )
    .unwrap();
    let mut sess = Instance::new("api-sess", "/other/api");
    sess.source_profile = "test".to_string();
    env.view.instances.insert(sess.id.clone(), sess);

    env.view.group_by = GroupByMode::Project;
    env.view.refresh_registered_projects();
    env.view.flat_items = env.view.build_flat_items();

    let api_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "api"))
        .expect("api header present");
    // The header's repo (/other/api) is not registered, so it is NOT pinned,
    // even though a same-basename repo (/work/api) is. The old basename match
    // would have reported pinned here.
    assert!(!env.view.is_project_label_pinned("api"));

    // Pinning this header would register under the basename "api", which the
    // registry already holds for /work/api, so the registry's name-uniqueness
    // surfaces a conflict rather than silently toggling the unrelated entry.
    env.view.cursor = api_idx;
    env.view.update_selected();
    env.view.toggle_project_pin_at_cursor();
    assert!(
        !env.view.is_project_label_pinned("api"),
        "the unrelated /work/api entry must not make this header read as pinned"
    );
    // The conflicting pin did not register the header's repo.
    let paths: Vec<String> = projects::load_global()
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    assert_eq!(paths, vec!["/work/api".to_string()]);
}

/// "New Session" on an empty pinned project (no member sessions) must prefill
/// the registered repo path, so the pin->launch loop works: the path can only
/// come from the registry fallback in `group_repo_path`.
#[test]
#[serial]
fn empty_pinned_project_new_session_uses_registered_path() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let mut env = create_test_env_empty();
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("lonely", "/repos/lonely", ProjectScope::Global),
        false,
    )
    .unwrap();
    env.view.group_by = GroupByMode::Project;
    env.view.refresh_registered_projects();
    env.view.flat_items = env.view.build_flat_items();

    assert_eq!(
        env.view.group_repo_path("lonely"),
        Some("/repos/lonely".to_string()),
        "empty pinned project must source its new-session path from the registry"
    );
}

/// In all-profiles mode the pin registry must include every loaded profile's
/// projects, not just the default profile's, so a profile-scoped pin keeps its
/// empty header (CodeRabbit #2055).
#[test]
#[serial]
fn all_profiles_view_includes_profile_scoped_pins() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Two discoverable profiles, each with a session.
    for (profile, title, path) in [
        ("alpha", "Alpha Session", "/tmp/a"),
        ("beta", "Beta Session", "/tmp/b"),
    ] {
        let storage = Storage::new_unwatched(profile).unwrap();
        let xs = vec![Instance::new(title, path)];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }
    // A profile-scoped pin in `beta` with no sessions of its own.
    projects::add(
        "beta",
        ProjectScope::Profile,
        Project::new("lonely", "/repos/lonely", ProjectScope::Profile).with_pinned(true),
        false,
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = GroupByMode::Project;
    view.flat_items = view.build_flat_items();

    let names: Vec<String> = view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        names.iter().any(|n| n == "lonely"),
        "beta's profile-scoped pin must show in all-profiles project view, got {names:?}"
    );
    assert!(view.is_project_label_pinned("lonely"));
}

/// Unpinning a PROFILE-scoped pin from all-profiles mode must actually clear
/// it. Regression for #2055: the empty header surfaced from a non-default
/// profile's registry, but the unpin removed against `config_profile()` (the
/// default profile) rather than the profile that owned the entry, so the
/// header never disappeared.
#[test]
#[serial]
fn unpin_profile_scoped_pin_from_all_profiles_clears_header() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Two discoverable profiles, each with a session, so all-profiles mode
    // loads both registries.
    for (profile, title, path) in [
        ("alpha", "Alpha Session", "/tmp/a"),
        ("beta", "Beta Session", "/tmp/b"),
    ] {
        let storage = Storage::new_unwatched(profile).unwrap();
        let xs = vec![Instance::new(title, path)];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }
    // A profile-scoped pin in `beta` (NOT the default profile) with no sessions.
    projects::add(
        "beta",
        ProjectScope::Profile,
        Project::new("lonely", "/repos/lonely", ProjectScope::Profile).with_pinned(true),
        false,
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = GroupByMode::Project;
    view.flat_items = view.build_flat_items();

    let idx = view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "lonely"))
        .expect("lonely header present in all-profiles project view");
    view.cursor = idx;
    view.update_selected();
    view.toggle_project_pin_at_cursor();

    assert!(
        !view.is_project_label_pinned("lonely"),
        "lonely must read as unpinned after the toggle"
    );
    // The unpin must clear the flag on beta's on-disk entry (not the default
    // profile's), but KEEP the entry: it stays a saved project. See #2208.
    let beta_after = projects::load_profile("beta").unwrap();
    assert_eq!(beta_after.len(), 1, "the profile-scoped entry must be kept");
    assert!(
        !beta_after[0].pinned,
        "its pin flag must be cleared on disk"
    );
    let still: Vec<String> = view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !still.iter().any(|n| n == "lonely"),
        "unpinned lonely must drop from the view, got {still:?}"
    );
}

/// A repo pinned in BOTH scopes (a profile entry shadowing a global one via
/// `--allow-override`) must fully unpin in a single press. `load_merged` only
/// surfaces the shadowing profile entry, so clearing just that one would
/// re-surface the global pin and leave the header pinned after a "success"
/// dialog. Unpin sweeps every scope for the path, clearing the flag while
/// keeping each entry. See #2208.
#[test]
#[serial]
fn unpin_clears_both_global_and_profile_entries_for_a_path() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let mut env = create_test_env_empty();
    // Same path pinned globally and profile-scoped (override allows the shadow).
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("dual-global", "/repos/dual", ProjectScope::Global).with_pinned(true),
        false,
    )
    .unwrap();
    projects::add(
        "test",
        ProjectScope::Profile,
        Project::new("dual-profile", "/repos/dual", ProjectScope::Profile).with_pinned(true),
        true,
    )
    .unwrap();

    env.view.group_by = GroupByMode::Project;
    env.view.refresh_registered_projects();
    env.view.flat_items = env.view.build_flat_items();

    let idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "dual"))
        .expect("dual header present");
    env.view.cursor = idx;
    env.view.update_selected();

    // One press must fully unpin.
    env.view.toggle_project_pin_at_cursor();

    assert!(
        !env.view.is_project_label_pinned("dual"),
        "dual must read as unpinned after a single press"
    );
    // Both entries are kept (saved projects), with the pin flag cleared in
    // each scope so the merged view no longer shows a pinned header. See #2208.
    let global_after = projects::load_global().unwrap();
    assert_eq!(global_after.len(), 1, "global entry must be kept");
    assert!(!global_after[0].pinned, "global pin flag must be cleared");
    let profile_after = projects::load_profile("test").unwrap();
    assert_eq!(profile_after.len(), 1, "profile entry must be kept");
    assert!(!profile_after[0].pinned, "profile pin flag must be cleared");
    let names: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !names.iter().any(|n| n == "dual"),
        "unpinned dual must drop from the view, got {names:?}"
    );
}

/// Pressing `g` to flip `group_by` keeps the cursor on the previously
/// selected session, even when the list reshapes (Manual flat list →
/// Project grouped list). Previously `apply_group_by` clamped by index,
/// which landed the cursor on whatever row slid into the old slot once
/// project headers got inserted. The fix seeks `selected_session` by id
/// after the rebuild.
#[test]
#[serial]
fn group_by_toggle_preserves_selected_session() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Manual;
    env.view.sort_order = crate::session::config::SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();

    // Pick the last session in the Manual flat list; that's the row whose
    // index is most likely to be invalidated when project headers get
    // inserted in front of it.
    let target_id = env
        .view
        .flat_items
        .iter()
        .rev()
        .find_map(|i| match i {
            Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("manual flat list should contain at least one session");
    env.view.select_session_by_id(&target_id);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(target_id.as_str())
    );

    env.view.handle_key(key(KeyCode::Char('g')), None);
    // 'g' opens the picker; pick Project to apply the flip.
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.group_by, GroupByMode::Project);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(target_id.as_str()),
        "cursor must stay on the same session after group_by flip"
    );
    let cursor_item = env
        .view
        .flat_items
        .get(env.view.cursor)
        .expect("cursor must point into flat_items");
    match cursor_item {
        Item::Session { id, .. } => assert_eq!(id, &target_id),
        Item::Group { .. } => panic!("cursor landed on a group header, not the session"),
    }
}

/// Pressing `o` to flip `sort_order` keeps the cursor on the previously
/// selected session. Most visible when going Newest → Attention with
/// Project grouping on, since Attention reorders both groups (by top
/// member) and sessions within each group, so the target session is very
/// unlikely to keep its index across the rebuild.
#[test]
#[serial]
fn sort_order_toggle_preserves_selected_session() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();

    // Pin the Running session inside alpha. Under Attention sort it sinks
    // below alpha-waiting, so its index will shift on the rebuild.
    let target_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .expect("fixture provides alpha-running");
    env.view.select_session_by_id(&target_id);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(target_id.as_str())
    );

    // Open the sort picker and pick Attention (one down from Newest).
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.sort_order, SortOrder::Attention);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(target_id.as_str()),
        "cursor must stay on the same session after sort_order flip"
    );
}

/// `reseat_cursor_after_rebuild` falls back to index clamping when there
/// is no prior session selection. Guards against the helper accidentally
/// regressing the empty-or-group-only path, where the original clamp
/// logic was correct.
#[test]
#[serial]
fn reseat_cursor_clamps_when_no_session_selected() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();
    env.view.selected_session = None;
    env.view.cursor = env.view.flat_items.len() + 50; // intentionally out of range

    env.view.reseat_cursor_after_rebuild();
    assert!(
        env.view.cursor < env.view.flat_items.len(),
        "cursor must be clamped into the flat_items range"
    );
}

/// Manual grouping + Attention sort must still flatten. The cross-cutting
/// flat priority view is the original Attention design and is the right
/// behavior when the user has not opted into project grouping. Guards
/// against an over-eager refactor flipping both modes to grouped.
#[test]
#[serial]
fn manual_grouping_attention_sort_stays_flat() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Manual;
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();

    let group_count = env
        .view
        .flat_items
        .iter()
        .filter(|i| matches!(i, Item::Group { .. }))
        .count();
    assert_eq!(
        group_count, 0,
        "Manual + Attention should produce a flat list, no group headers"
    );
}

/// `prune_empty_group` is the post-move cleanup that drops the source
/// profile's now-empty copy of a group after a session moves to a
/// different profile. Without it, both profiles end up with the same
/// group name in unified view, the source one empty and the target one
/// populated, which reads as a duplicate group header.
#[test]
#[serial]
fn prune_empty_group_drops_source_when_no_session_remains() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _ = Storage::new_unwatched("alpha").unwrap();
    let _ = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();

    // Pre-state: alpha has one session in group "work", beta is empty.
    let mut moved = Instance::new("moved", "/tmp/moved");
    moved.source_profile = "alpha".to_string();
    moved.group_path = "work".to_string();
    let insts = vec![moved];
    seed_instances(&mut view, &insts);
    view.group_trees.clear();
    view.group_trees
        .insert("alpha".to_string(), GroupTree::new_with_groups(&insts, &[]));
    view.group_trees
        .insert("beta".to_string(), GroupTree::new_with_groups(&[], &[]));
    assert!(view.group_trees["alpha"].group_exists("work"));

    // Simulate the move: re-tag source_profile, then prune the now-empty
    // source group.
    view.instance_at_mut(0).source_profile = "beta".to_string();
    view.prune_empty_group("alpha", "work");

    assert!(
        !view.group_trees["alpha"].group_exists("work"),
        "alpha should no longer own the now-empty 'work' group after the move"
    );
}

/// Prune must NOT drop the source group when the source profile still
/// has other sessions sitting at the same path (or nested under it).
/// Two sessions, only one moved → source profile keeps the group.
#[test]
#[serial]
fn prune_empty_group_keeps_source_when_sibling_session_remains() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _ = Storage::new_unwatched("alpha").unwrap();
    let _ = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();

    let mut moved = Instance::new("moved", "/tmp/moved");
    moved.source_profile = "alpha".to_string();
    moved.group_path = "work".to_string();
    let mut sibling = Instance::new("sibling", "/tmp/sibling");
    sibling.source_profile = "alpha".to_string();
    sibling.group_path = "work".to_string();
    let insts = vec![moved, sibling];
    seed_instances(&mut view, &insts);
    view.group_trees.clear();
    view.group_trees
        .insert("alpha".to_string(), GroupTree::new_with_groups(&insts, &[]));
    view.group_trees
        .insert("beta".to_string(), GroupTree::new_with_groups(&[], &[]));

    view.instance_at_mut(0).source_profile = "beta".to_string();
    view.prune_empty_group("alpha", "work");

    assert!(
        view.group_trees["alpha"].group_exists("work"),
        "alpha must keep 'work' because the sibling session still lives there"
    );
}

/// Prune must also keep the source group when a session sits in a
/// *descendant* path. Only the leaf moved out; the parent still has
/// rows under it.
#[test]
#[serial]
fn prune_empty_group_keeps_source_when_descendant_session_remains() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _ = Storage::new_unwatched("alpha").unwrap();
    let _ = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();

    let mut moved = Instance::new("moved", "/tmp/moved");
    moved.source_profile = "alpha".to_string();
    moved.group_path = "work".to_string();
    let mut nested = Instance::new("nested", "/tmp/nested");
    nested.source_profile = "alpha".to_string();
    nested.group_path = "work/frontend".to_string();
    let insts = vec![moved, nested];
    seed_instances(&mut view, &insts);
    view.group_trees.clear();
    view.group_trees
        .insert("alpha".to_string(), GroupTree::new_with_groups(&insts, &[]));
    view.group_trees
        .insert("beta".to_string(), GroupTree::new_with_groups(&[], &[]));

    view.instance_at_mut(0).source_profile = "beta".to_string();
    view.prune_empty_group("alpha", "work");

    assert!(
        view.group_trees["alpha"].group_exists("work"),
        "alpha must keep 'work' because the nested session still lives under it"
    );
}

/// Prune must keep the source group when the profile's tree carries a
/// descendant *group* (even with no session under it). Lets users keep
/// hand-built structure like `work/anchor` that survives moves of every
/// session out of the parent. Without this guard, `delete_group`'s
/// `starts_with(prefix)` cascade nukes the anchor sub-group too.
#[test]
#[serial]
fn prune_empty_group_keeps_source_when_descendant_group_remains() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _ = Storage::new_unwatched("alpha").unwrap();
    let _ = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();

    let mut moved = Instance::new("moved", "/tmp/moved");
    moved.source_profile = "alpha".to_string();
    moved.group_path = "work".to_string();
    let insts = vec![moved];
    seed_instances(&mut view, &insts);
    view.group_trees.clear();
    let mut alpha_tree = GroupTree::new_with_groups(&insts, &[]);
    alpha_tree.create_group("work/anchor");
    view.group_trees.insert("alpha".to_string(), alpha_tree);
    view.group_trees
        .insert("beta".to_string(), GroupTree::new_with_groups(&[], &[]));
    assert!(view.group_trees["alpha"].group_exists("work/anchor"));

    view.instance_at_mut(0).source_profile = "beta".to_string();
    view.prune_empty_group("alpha", "work");

    assert!(
        view.group_trees["alpha"].group_exists("work"),
        "alpha must keep 'work' because of the user-anchored 'work/anchor' sub-group"
    );
    assert!(
        view.group_trees["alpha"].group_exists("work/anchor"),
        "anchor sub-group must survive the no-op prune"
    );
}

/// The prune must persist through save+reload. Without tombstoning in
/// `pending_group_deletions`, the in-memory delete is reverted on next
/// startup because `HomeView::new` reloads `existing_groups` from disk
/// and reseeds the tree with the supposedly-pruned group. Mirrors the
/// restart_selected_session sequence: seed + persist, then move + prune
/// + persist.
#[test]
#[serial]
fn prune_empty_group_survives_save_and_reload() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _ = Storage::new_unwatched("alpha").unwrap();
    let _ = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);

    {
        let mut view = HomeView::new(
            None,
            tools.clone(),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        let moved = {
            let mut inst = Instance::new("moved", "/tmp/moved");
            inst.id = "moved".to_string();
            inst.source_profile = "alpha".to_string();
            inst.group_path = "work".to_string();
            inst
        };
        view.instances.insert(moved.id.clone(), moved);
        view.pending_added
            .entry("alpha".to_string())
            .or_default()
            .insert("moved".to_string());
        view.group_trees.insert(
            "alpha".to_string(),
            GroupTree::new_with_groups(&view.cloned_instances(), &[]),
        );
        view.save().unwrap();

        view.group_trees
            .entry("beta".to_string())
            .or_insert_with(|| GroupTree::new_with_groups(&[], &[]));
        let old_path = view.instances["moved"].group_path.clone();
        view.move_to_profile("moved", "beta", old_path.clone())
            .unwrap();
        view.prune_empty_group("alpha", &old_path);
        view.save().unwrap();
    }

    let reloaded = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    assert!(
        reloaded.group_trees.contains_key("alpha"),
        "alpha tree must still load after the move"
    );
    assert!(
        !reloaded.group_trees["alpha"].group_exists("work"),
        "pruned 'work' must stay gone after save+reload, not get re-seeded from disk"
    );
}

/// Favorite, snooze, and urgent decorations only render in Attention sort.
/// In Newest (or any other sort), the row paints with its plain title and
/// status-driven color even when the flags are set, so users who don't
/// triage in Attention don't see decoration for state they didn't opt into
/// managing.
#[test]
#[serial]
fn favorite_decoration_gated_to_attention_sort() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let title = env.view.instance_at(0).title.clone();
    env.view.mutate_instance(&id, |inst| inst.favorite());

    // In Newest: row should NOT have the `* ` prefix or the bold/
    // underlined favorite styling.
    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    let item = env
        .view
        .flat_items
        .iter()
        .find(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id))
        .cloned()
        .expect("session item present in Newest sort");
    let text_newest = rendered_row_text(&env.view, &item);
    assert!(
        !text_newest.contains("* "),
        "favorite prefix must be hidden outside Attention sort; got: {:?}",
        text_newest
    );
    assert!(
        text_newest.contains(&title),
        "row title must still render; got: {:?}",
        text_newest
    );

    // Flip to Attention: the prefix returns.
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    let item_attention = env
        .view
        .flat_items
        .iter()
        .find(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id))
        .cloned()
        .expect("session item present in Attention sort");
    let text_attention = rendered_row_text(&env.view, &item_attention);
    assert!(
        text_attention.contains("* "),
        "favorite prefix must surface in Attention sort; got: {:?}",
        text_attention
    );
}

/// Snoozed rows: prefix and remaining-time column only appear in Attention
/// sort. Outside Attention, the snooze flag persists silently and the row
/// paints with its underlying status.
#[test]
#[serial]
fn snooze_decoration_gated_to_attention_sort() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| inst.snooze(30));

    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    let item_newest = env
        .view
        .flat_items
        .iter()
        .find(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id))
        .cloned()
        .expect("session item present in Newest sort");
    let text_newest = rendered_row_text(&env.view, &item_newest);
    assert!(
        !text_newest.contains("z "),
        "snooze prefix must be hidden outside Attention sort; got: {:?}",
        text_newest
    );

    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    let item_attention = env
        .view
        .flat_items
        .iter()
        .find(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id))
        .cloned()
        .expect("session item present in Attention sort");
    let text_attention = rendered_row_text(&env.view, &item_attention);
    assert!(
        text_attention.contains("z "),
        "snooze prefix must surface in Attention sort; got: {:?}",
        text_attention
    );
}

/// Archived sessions live under the synthetic "Archived" section pinned to
/// the bottom of the sidebar in every sort mode, not inline at their
/// natural position. The section header carries the count; when collapsed
/// the archived rows themselves are hidden but the header still appears.
#[test]
#[serial]
fn archived_section_pinned_to_bottom_in_every_sort() {
    use crate::session::{config::SortOrder, is_archived_section_path, ARCHIVED_SECTION_NAME};

    let mut env = create_test_env_with_sessions(3);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| inst.archive());
    env.view.archived_section_collapsed = true;

    for sort in [SortOrder::Newest, SortOrder::Attention, SortOrder::AZ] {
        env.view.sort_order = sort;
        env.view.flat_items = env.view.build_flat_items();

        // Archived row must NOT appear inline among the active sessions.
        let archived_inline = env
            .view
            .flat_items
            .iter()
            .take_while(|i| {
                !matches!(
                    i,
                    Item::Group { path, .. } if is_archived_section_path(path)
                )
            })
            .any(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id));
        assert!(
            !archived_inline,
            "[{:?}] archived row must not appear before the Archived section",
            sort
        );

        // The synthetic section must sit at the bottom of the list.
        let last = env
            .view
            .flat_items
            .last()
            .expect("flat_items should be non-empty");
        match last {
            Item::Group {
                path,
                name,
                session_count,
                collapsed,
                ..
            } => {
                assert!(
                    is_archived_section_path(path),
                    "[{:?}] last item must be the Archived section header; got path {:?}",
                    sort,
                    path
                );
                assert_eq!(name, ARCHIVED_SECTION_NAME, "[{:?}] section name", sort);
                assert_eq!(*session_count, 1, "[{:?}] one archived row", sort);
                assert!(*collapsed, "[{:?}] section must default collapsed", sort);
            }
            other => panic!(
                "[{:?}] expected Archived section header, got {:?}",
                sort, other
            ),
        }
    }
}

/// In Project grouping mode, archived sessions must nest under per-project
/// sub-headers inside the Archived section instead of forming one flat list.
/// Layout: Archived (depth 0) > <project> (depth 1) > sessions (depth 2).
/// Sessions inside a sub-folder still sort most-recently-archived first.
#[test]
#[serial]
fn archived_section_nests_by_project_in_project_mode() {
    use crate::session::{
        archived_project_sub_path,
        config::{GroupByMode, SortOrder},
        is_archived_section_path, ARCHIVED_SECTION_NAME,
    };

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    // Pin to AZ so this test asserts only the depth-0/1/2 layout shape,
    // not the sort-order behavior. Sort_order coverage lives in
    // `archived_sub_folders_honor_sort_order` below.
    env.view.sort_order = SortOrder::AZ;
    // Archive one session from each project so we expect two sub-folders.
    let alpha_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-error")
        .map(|i| i.id.clone())
        .unwrap();
    env.view
        .apply_user_action(&alpha_id, |inst| inst.archive())
        .unwrap();
    env.view
        .apply_user_action(&beta_id, |inst| inst.archive())
        .unwrap();
    env.view.archived_section_collapsed = false;
    env.view.flat_items = env.view.build_flat_items();

    // Find the Archived section header and walk forward.
    let arch_idx = env
        .view
        .flat_items
        .iter()
        .position(|it| matches!(it, Item::Group { path, .. } if is_archived_section_path(path)))
        .expect("Archived section header must be present");

    // Header sanity: depth 0, count = 2, name = Archived.
    match &env.view.flat_items[arch_idx] {
        Item::Group {
            depth,
            session_count,
            name,
            ..
        } => {
            assert_eq!(*depth, 0, "Archived header depth");
            assert_eq!(*session_count, 2, "two archived sessions across projects");
            assert_eq!(name, ARCHIVED_SECTION_NAME);
        }
        _ => unreachable!(),
    }

    // The next two non-session items should be sub-folder headers at depth 1,
    // one for "alpha" and one for "beta", in alphabetical order. Between them
    // and after the second, the sessions at depth 2 belong to that sub-folder.
    let tail = &env.view.flat_items[arch_idx + 1..];

    let sub_alpha_path = archived_project_sub_path("alpha");
    let sub_beta_path = archived_project_sub_path("beta");

    // First sub-header must be alpha (AZ sort orders by name).
    match &tail[0] {
        Item::Group {
            path,
            name,
            depth,
            session_count,
            ..
        } => {
            assert_eq!(path, &sub_alpha_path);
            assert_eq!(name, "alpha");
            assert_eq!(*depth, 1);
            assert_eq!(*session_count, 1);
        }
        other => panic!("expected alpha sub-header at depth 1, got {:?}", other),
    }
    // Then alpha's archived session at depth 2.
    match &tail[1] {
        Item::Session { id, depth } => {
            assert_eq!(
                id, &alpha_id,
                "alpha sub-folder should contain alpha-running"
            );
            assert_eq!(*depth, 2);
        }
        other => panic!("expected alpha-running session row, got {:?}", other),
    }
    // Then the beta sub-header at depth 1.
    match &tail[2] {
        Item::Group {
            path,
            name,
            depth,
            session_count,
            ..
        } => {
            assert_eq!(path, &sub_beta_path);
            assert_eq!(name, "beta");
            assert_eq!(*depth, 1);
            assert_eq!(*session_count, 1);
        }
        other => panic!("expected beta sub-header at depth 1, got {:?}", other),
    }
    // Then beta's archived session at depth 2.
    match &tail[3] {
        Item::Session { id, depth } => {
            assert_eq!(id, &beta_id, "beta sub-folder should contain beta-error");
            assert_eq!(*depth, 2);
        }
        other => panic!("expected beta-error session row, got {:?}", other),
    }
}

/// A project whose only remaining member is archived must NOT leave an empty
/// phantom header in the main (non-archived) flow. The archived session shows
/// under the Archived section instead; an empty project header would be
/// undeletable in project mode ("Project groups are automatic").
#[test]
#[serial]
fn archived_only_project_leaves_no_phantom_header() {
    use crate::session::{config::GroupByMode, is_within_archived_section};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;

    // Drain beta down to a single ARCHIVED member: archive beta-error, then
    // delete beta-running (the "last visible session in the group").
    let beta_error = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-error")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_running = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-running")
        .map(|i| i.id.clone())
        .unwrap();
    env.view
        .apply_user_action(&beta_error, |inst| inst.archive())
        .unwrap();
    env.view.instances.shift_remove(&beta_running);
    env.view.flat_items = env.view.build_flat_items();

    // Count "beta" headers that live OUTSIDE the Archived section.
    let mut in_archived = false;
    let mut main_beta_headers = 0;
    for item in &env.view.flat_items {
        if let Item::Group { path, name, .. } = item {
            if is_within_archived_section(path) {
                in_archived = true;
            } else if name == "beta" && !in_archived {
                main_beta_headers += 1;
            }
        }
    }
    assert_eq!(
        main_beta_headers, 0,
        "archived-only project must not render a header in the main flow; got flat_items: {:?}",
        env.view.flat_items
    );
}

/// Collapsing the Archived umbrella in Project mode hides both sub-folder
/// headers and their session rows.
#[test]
#[serial]
fn archived_section_collapsed_hides_project_sub_folders() {
    use crate::session::{config::GroupByMode, is_within_archived_section};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    let alpha_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    env.view
        .apply_user_action(&alpha_id, |inst| inst.archive())
        .unwrap();
    env.view.archived_section_collapsed = true;
    env.view.flat_items = env.view.build_flat_items();

    let within_archive_items: Vec<&Item> = env
        .view
        .flat_items
        .iter()
        .filter(|it| match it {
            Item::Group { path, .. } => is_within_archived_section(path),
            Item::Session { .. } => false,
        })
        .collect();
    assert_eq!(
        within_archive_items.len(),
        1,
        "collapsed Archived must render only its top-level header, got {:?}",
        within_archive_items
    );
}

/// Collapsing a single project sub-folder under Archived hides its session
/// rows but leaves the sub-header (and any other sub-folders) intact. Uses
/// the same `project_group_collapsed` map that drives regular project mode
/// collapse, keyed by the synthetic `archived_project_sub_path`.
#[test]
#[serial]
fn archived_project_sub_folder_collapse_hides_only_its_sessions() {
    use crate::session::{archived_project_sub_path, config::GroupByMode};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    let alpha_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-error")
        .map(|i| i.id.clone())
        .unwrap();
    env.view
        .apply_user_action(&alpha_id, |inst| inst.archive())
        .unwrap();
    env.view
        .apply_user_action(&beta_id, |inst| inst.archive())
        .unwrap();
    env.view.archived_section_collapsed = false;
    // Collapse only alpha's archived sub-folder.
    env.view
        .project_group_collapsed
        .insert(archived_project_sub_path("alpha"), true);
    env.view.flat_items = env.view.build_flat_items();

    // alpha sub-folder must still appear as a header but with no session row
    // following it; beta sub-folder must still emit its session row.
    let has_alpha_session = env
        .view
        .flat_items
        .iter()
        .any(|it| matches!(it, Item::Session { id, .. } if id == &alpha_id));
    let has_beta_session = env
        .view
        .flat_items
        .iter()
        .any(|it| matches!(it, Item::Session { id, .. } if id == &beta_id));
    assert!(
        !has_alpha_session,
        "collapsed alpha sub-folder must hide its archived session"
    );
    assert!(
        has_beta_session,
        "expanded beta sub-folder must still surface its archived session"
    );
    let alpha_sub_path = archived_project_sub_path("alpha");
    assert!(
        env.view.flat_items.iter().any(
            |it| matches!(it, Item::Group { path, collapsed, .. } if path == &alpha_sub_path && *collapsed)
        ),
        "alpha sub-folder header must remain visible with collapsed=true"
    );
}

/// Archived project sub-folders honor `sort_order`, mirroring how active
/// project headers order in `flatten_tree`. AZ/ZA sort by project name;
/// recency sorts (Newest, LastActivity, Attention) bring the most-
/// recently-archived project to the top; Oldest does the inverse. Probes
/// AZ, ZA, and Newest as representatives; the Oldest/LastActivity/Attention
/// branches share the same `sort_archived_project_buckets` machinery.
#[test]
#[serial]
fn archived_sub_folders_honor_sort_order() {
    use crate::session::{
        archived_project_sub_path,
        config::{GroupByMode, SortOrder},
        is_archived_section_path,
    };

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    let alpha_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-error")
        .map(|i| i.id.clone())
        .unwrap();
    // Archive alpha first, then beta. archived_at is `Utc::now()` at the
    // moment of `archive()`, so beta is strictly more recent than alpha.
    env.view
        .apply_user_action(&alpha_id, |inst| inst.archive())
        .unwrap();
    env.view
        .apply_user_action(&beta_id, |inst| inst.archive())
        .unwrap();
    env.view.archived_section_collapsed = false;

    let first_sub_folder = |env: &TestEnv| -> Option<String> {
        let arch_idx = env.view.flat_items.iter().position(
            |it| matches!(it, Item::Group { path, .. } if is_archived_section_path(path)),
        )?;
        env.view
            .flat_items
            .get(arch_idx + 1)
            .and_then(|it| match it {
                Item::Group { path, .. } => Some(path.clone()),
                _ => None,
            })
    };

    let alpha_sub = archived_project_sub_path("alpha");
    let beta_sub = archived_project_sub_path("beta");

    env.view.sort_order = SortOrder::AZ;
    env.view.flat_items = env.view.build_flat_items();
    assert_eq!(
        first_sub_folder(&env).as_deref(),
        Some(alpha_sub.as_str()),
        "AZ: alphabetical, alpha first"
    );

    env.view.sort_order = SortOrder::ZA;
    env.view.flat_items = env.view.build_flat_items();
    assert_eq!(
        first_sub_folder(&env).as_deref(),
        Some(beta_sub.as_str()),
        "ZA: reverse alphabetical, beta first"
    );

    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    assert_eq!(
        first_sub_folder(&env).as_deref(),
        Some(beta_sub.as_str()),
        "Newest: most-recently-archived project first (beta archived after alpha)"
    );
}

mod scroll_pane_isolation {
    //! Wheel events are confined to whichever pane the mouse is over.
    //! In particular, a wheel over the preview pane never moves the list
    //! cursor: not when the preview is at its scroll boundary, and not
    //! when no session is selected. See issue #1361.

    use super::*;
    use ratatui::layout::Rect;

    fn setup_panes(env: &mut TestEnv) {
        env.view.list_area = Rect::new(0, 0, 30, 40);
        env.view.preview_area = Rect::new(30, 0, 100, 40);
    }

    /// Build a live-send env whose preview-capture worker reports the
    /// given cursor, so the alternate-screen wheel-forwarding branch can
    /// be exercised without a real full-screen pane.
    fn live_env_with_cursor(cursor: crate::tmux::PaneCursor) -> TestEnv {
        use crate::tui::home::live_send::{LiveSendState, LiveSendTarget, LiveSendWorker};
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 1;
        env.view.update_selected();
        env.view.preview_cache.dimensions = (80, 24);
        env.view.preview_cache.captured_lines = 200;
        env.view.preview_scroll_offset = 10;
        env.view.live_send = Some(LiveSendState {
            session_id: "fake".to_string(),
            title: "fake".to_string(),
            tmux_name: "fake".to_string(),
            target: LiveSendTarget::Agent,
            exit_chords: crate::tui::home::live_send::parse_chord_list(
                crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
            ),
            leader: None,
        });
        env.view.live_send_worker = Some(LiveSendWorker::spawn("fake".to_string(), None));
        // Spawn the capture worker, then inject the cursor (set_target
        // clears it, so the injection must come after).
        env.view
            .sync_preview_capture_worker(Some("fake".to_string()));
        env.view
            .preview_capture_worker
            .as_ref()
            .expect("capture worker spawned")
            .set_cursor_for_test(Some(cursor));
        env
    }

    /// Like `live_env_with_cursor` but WITHOUT entering live-send: the
    /// session is merely previewed (the common "hover the preview" case).
    /// No `live_send` / `live_send_worker`; the capture worker and its
    /// target are set so `forward_wheel_to_preview` can take the passive
    /// one-shot path.
    fn passive_env_with_cursor(cursor: crate::tmux::PaneCursor) -> TestEnv {
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 1;
        env.view.update_selected();
        env.view.preview_cache.dimensions = (80, 24);
        env.view.preview_cache.captured_lines = 200;
        env.view.preview_scroll_offset = 10;
        env.view
            .sync_preview_capture_worker(Some("fake".to_string()));
        env.view.preview_capture_target = Some("fake".to_string());
        env.view
            .preview_capture_worker
            .as_ref()
            .expect("capture worker spawned")
            .set_cursor_for_test(Some(cursor));
        env
    }

    fn alt_screen_cursor(
        alternate_on: bool,
        mouse_tracking: bool,
        mouse_sgr: bool,
    ) -> crate::tmux::PaneCursor {
        crate::tmux::PaneCursor {
            x: 0,
            y: 0,
            visible: true,
            pane_height: 24,
            history_size: 1800,
            pane_width: 80,
            alternate_on,
            mouse_tracking,
            mouse_sgr,
            mouse_all: false,
            position_reliable: true,
        }
    }

    /// Live-send target is a full-screen app with SGR mouse tracking on:
    /// the wheel is forwarded to the app (returns to the live edge) instead
    /// of growing the useless normal-buffer capture window. This is the fix
    /// for the "scroll up a little then snap to the very first part of the
    /// session" report on alternate-screen agents.
    #[test]
    #[serial]
    fn wheel_over_alt_screen_sgr_mouse_pane_forwards_instead_of_scrollback() {
        let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));

        let up = env.view.handle_scroll_up(50, 10);
        assert!(up, "wheel over a full-screen SGR-mouse pane is handled");
        assert_eq!(
            env.view.preview_scroll_offset, 0,
            "forwarding pins the preview to the live edge, never the normal-buffer history"
        );

        env.view.preview_scroll_offset = 10;
        let down = env.view.handle_scroll_down(50, 10);
        assert!(down);
        assert_eq!(env.view.preview_scroll_offset, 0);
    }

    /// A full-screen app WITHOUT mouse tracking (e.g. Claude Code's
    /// fullscreen renderer: `1049h` + `1007h`, no mouse) does not scroll on
    /// arrow keys (it reads them as cursor / input-history navigation), so we
    /// forward `PageUp`/`PageDown` named keys instead and pin the preview to
    /// the live edge, just like the mouse-tracking case. Regression for #2407.
    #[test]
    #[serial]
    fn wheel_over_alt_screen_without_mouse_forwards_page_keys() {
        let mut env = live_env_with_cursor(alt_screen_cursor(true, false, false));

        let up = env.view.handle_scroll_up(50, 10);
        assert!(up, "wheel over a full-screen no-mouse pane is handled");
        assert_eq!(
            env.view.preview_scroll_offset, 0,
            "arrow-key forwarding pins the preview to the live edge, never the normal-buffer history"
        );

        env.view.preview_scroll_offset = 10;
        let down = env.view.handle_scroll_down(50, 10);
        assert!(down);
        assert_eq!(env.view.preview_scroll_offset, 0);
    }

    /// Passive preview (NOT live-send) over a full-screen agent must ALSO
    /// forward the wheel. The alternate screen has no scrollback, so the
    /// capture-window scroll is inert; without forwarding, "hover the
    /// preview and scroll" does literally nothing (the reported regression
    /// after Claude Code's fullscreen renderer landed). Forwarding pins the
    /// preview to the live edge, exactly like the live-send path.
    #[test]
    #[serial]
    fn wheel_over_alt_screen_passive_preview_forwards() {
        let mut env = passive_env_with_cursor(alt_screen_cursor(true, true, true));
        assert!(
            env.view.live_send.is_none(),
            "this exercises passive preview, not live-send"
        );

        let up = env.view.handle_scroll_up(50, 10);
        assert!(
            up,
            "wheel over a full-screen pane in passive preview is forwarded"
        );
        assert_eq!(
            env.view.preview_scroll_offset, 0,
            "passive forwarding pins the preview to the live edge"
        );

        env.view.preview_scroll_offset = 10;
        let down = env.view.handle_scroll_down(50, 10);
        assert!(down);
        assert_eq!(env.view.preview_scroll_offset, 0);
    }

    /// In live-send over a mouse-tracking agent, a plain (no-Shift) left
    /// press/release is forwarded to the agent and consumed, and the held
    /// button is tracked so its release can't be stranded.
    #[test]
    #[serial]
    fn forward_mouse_to_preview_left_click_forwards() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
        assert!(env.view.forward_mouse_to_preview(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            50,
            10
        ));
        assert_eq!(env.view.mouse_forward_btn, Some(0));
        assert!(env.view.forward_mouse_to_preview(
            MouseEventKind::Up(MouseButton::Left),
            KeyModifiers::NONE,
            50,
            10
        ));
        assert_eq!(env.view.mouse_forward_btn, None);
        // Forwarding never starts an aoe text selection.
        assert!(env.view.drag_state.is_none());
        assert!(env.view.preview_selection.is_none());
    }

    /// Passive preview (NOT live-send) over a mouse-tracking agent ALSO forwards
    /// a plain press/drag/release, so hovering an agent and dragging drives its
    /// native selection / scroll, exactly like the live-send case (and like the
    /// passive wheel path). The one-shot send carries it with no live worker.
    #[test]
    #[serial]
    fn forward_mouse_to_preview_passive_preview_forwards() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut env = passive_env_with_cursor(alt_screen_cursor(true, true, true));
        assert!(
            env.view.live_send.is_none(),
            "this exercises passive preview, not live-send"
        );
        assert!(env.view.forward_mouse_to_preview(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            50,
            10
        ));
        assert_eq!(env.view.mouse_forward_btn, Some(0));
        assert!(env.view.forward_mouse_to_preview(
            MouseEventKind::Drag(MouseButton::Left),
            KeyModifiers::NONE,
            55,
            12
        ));
        assert!(env.view.forward_mouse_to_preview(
            MouseEventKind::Up(MouseButton::Left),
            KeyModifiers::NONE,
            55,
            12
        ));
        assert_eq!(env.view.mouse_forward_btn, None);
        // Forwarding never starts an aoe text selection, even passively.
        assert!(env.view.drag_state.is_none());
        assert!(env.view.preview_selection.is_none());
    }

    /// Bare motion over the preview is forwarded to an any-event-tracking
    /// (1003) agent so its hover UI (Claude Code's expandable-block
    /// highlight) works in live mode, deduped per pane cell, and re-armed
    /// when the pointer leaves the preview and comes back.
    #[test]
    #[serial]
    fn forward_hover_to_preview_reports_once_per_cell() {
        let mut cursor = alt_screen_cursor(true, true, true);
        cursor.mouse_all = true;
        let mut env = live_env_with_cursor(cursor);
        // The forward maps cells against the previewed pane's rect; give it
        // the preview area like a rendered frame would.
        env.view.preview_text_view.pane = Rect::new(30, 0, 100, 40);

        assert!(env.view.forward_hover_to_preview(50, 10));
        assert_eq!(env.view.hover_forward_cell, Some((21, 11)));
        // Same cell again: deduped, nothing sent.
        assert!(!env.view.forward_hover_to_preview(50, 10));
        // A different cell reports again.
        assert!(env.view.forward_hover_to_preview(51, 10));
        assert_eq!(env.view.hover_forward_cell, Some((22, 11)));
        // Leaving the preview clears the dedup cell (and forwards nothing)...
        assert!(!env.view.forward_hover_to_preview(1, 1));
        assert_eq!(env.view.hover_forward_cell, None);
        // ...so re-entering the same cell reports it to the agent again.
        assert!(env.view.forward_hover_to_preview(51, 10));
    }

    /// A button-tracking (1000/1002) agent never gets bare motion: it didn't
    /// ask for it and would misparse the report. Same for a non-mouse agent.
    #[test]
    #[serial]
    fn forward_hover_to_preview_requires_any_event_tracking() {
        let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
        env.view.preview_text_view.pane = Rect::new(30, 0, 100, 40);
        assert!(!env.view.forward_hover_to_preview(50, 10));
        assert_eq!(env.view.hover_forward_cell, None);

        let mut env = live_env_with_cursor(alt_screen_cursor(true, false, false));
        env.view.preview_text_view.pane = Rect::new(30, 0, 100, 40);
        assert!(!env.view.forward_hover_to_preview(50, 10));
    }

    /// Shift+press is NOT forwarded: it falls through so aoe's own preview
    /// text-selection (drag-to-copy) can run.
    #[test]
    #[serial]
    fn forward_mouse_to_preview_shift_falls_through() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
        assert!(!env.view.forward_mouse_to_preview(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::SHIFT,
            50,
            10
        ));
        assert_eq!(env.view.mouse_forward_btn, None);
    }

    /// A non-mouse agent never forwards; the event falls through to aoe.
    #[test]
    #[serial]
    fn forward_mouse_to_preview_non_mouse_agent_falls_through() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut env = live_env_with_cursor(alt_screen_cursor(true, false, false));
        assert!(!env.view.forward_mouse_to_preview(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            50,
            10
        ));
    }

    /// Once a press is forwarded, its drag and release keep forwarding even
    /// after the pointer leaves the preview rect, so the agent always sees the
    /// release (no stuck button).
    #[test]
    #[serial]
    fn forward_mouse_to_preview_drag_and_release_track_button() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
        assert!(env.view.forward_mouse_to_preview(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            50,
            10
        ));
        // (1, 1) is outside the preview rect, but the drag still forwards.
        assert!(env.view.forward_mouse_to_preview(
            MouseEventKind::Drag(MouseButton::Left),
            KeyModifiers::NONE,
            1,
            1
        ));
        assert_eq!(env.view.mouse_forward_btn, Some(0));
        assert!(env.view.forward_mouse_to_preview(
            MouseEventKind::Up(MouseButton::Left),
            KeyModifiers::NONE,
            1,
            1
        ));
        assert_eq!(env.view.mouse_forward_btn, None);
    }

    /// A drag or release with no forwarded press in flight is ignored (it must
    /// not start forwarding mid-gesture).
    #[test]
    #[serial]
    fn forward_mouse_to_preview_orphan_drag_ignored() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
        assert!(!env.view.forward_mouse_to_preview(
            MouseEventKind::Drag(MouseButton::Left),
            KeyModifiers::NONE,
            50,
            10
        ));
        assert!(!env.view.forward_mouse_to_preview(
            MouseEventKind::Up(MouseButton::Left),
            KeyModifiers::NONE,
            50,
            10
        ));
        assert_eq!(env.view.mouse_forward_btn, None);
    }

    /// Stage an in-flight Shift-selection drag held at the preview's top
    /// (`row == pane.y`) or bottom edge, plus a capture window with NO aoe-side
    /// scrollback, so `tick_preview_autoscroll` exercises the agent
    /// scroll-forward fallback rather than the capture-window line scroll.
    fn stage_edge_drag_no_scrollback(env: &mut TestEnv, at_top: bool) {
        use crate::tui::home::PreviewTextView;
        // Visible == captured: `scroll_preview_offset` has nowhere to go, the
        // alternate-screen reality the fallback exists for. The clamp reads
        // `preview_visible_rows`, so pin it to the captured-line count to make
        // the max offset zero (no scrollback to move into).
        env.view.preview_cache.captured_lines = 23;
        env.view.preview_visible_rows = 23;
        env.view.preview_cache.dimensions = (80, 24);
        env.view.preview_scroll_offset = 0;
        let pane = Rect::new(30, 0, 100, 5);
        env.view.preview_text_view = PreviewTextView {
            pane,
            first_line: 0,
            total_lines: 23,
        };
        // Anchor away from the held edge, then drag onto it.
        let (start_row, edge_row) = if at_top { (4, 0) } else { (0, 4) };
        assert!(env.view.handle_drag_start(40, start_row));
        assert!(env.view.handle_drag_move(40, edge_row));
    }

    /// Over a full-screen mouse-tracking agent the capture window has no
    /// scrollback, so an edge-held selection forwards the same scroll input the
    /// wheel does (a wheel-up/down mouse report, NOT PageUp, since the agent
    /// owns the mouse) to scroll its own transcript instead of moving the inert
    /// offset. The fallback delegates to `wheel_forward_key`, whose byte output
    /// per branch is asserted in `wheel_forward_key_*`; here we verify the tick
    /// forwards and pins the offset. Regression for the "autoscroll does nothing
    /// over a mouse-tracking agent" report (PageUp was a no-op there).
    #[test]
    #[serial]
    fn autoscroll_forwards_scroll_to_mouse_tracking_agent_at_top_edge() {
        let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
        stage_edge_drag_no_scrollback(&mut env, true);
        assert!(
            env.view.tick_preview_autoscroll(),
            "top-edge tick forwards a wheel notch to the agent"
        );
        // The inert capture-window offset never moved; the agent was scrolled.
        assert_eq!(env.view.preview_scroll_offset, 0);
    }

    /// Same as above at the bottom edge: a wheel-down notch is forwarded.
    #[test]
    #[serial]
    fn autoscroll_forwards_scroll_to_mouse_tracking_agent_at_bottom_edge() {
        let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
        stage_edge_drag_no_scrollback(&mut env, false);
        assert!(
            env.view.tick_preview_autoscroll(),
            "bottom-edge tick forwards a wheel notch to the agent"
        );
        assert_eq!(env.view.preview_scroll_offset, 0);
    }

    /// A full-screen agent WITHOUT mouse tracking (Claude Code's fullscreen
    /// renderer: `1049h`, no mouse) gets `PageUp`/`PageDown` from the fallback
    /// instead, matching the wheel path's no-mouse branch.
    #[test]
    #[serial]
    fn autoscroll_forwards_page_keys_to_no_mouse_agent_at_top_edge() {
        let mut env = live_env_with_cursor(alt_screen_cursor(true, false, false));
        stage_edge_drag_no_scrollback(&mut env, true);
        assert!(
            env.view.tick_preview_autoscroll(),
            "top-edge tick forwards a page key to the no-mouse agent"
        );
        assert_eq!(env.view.preview_scroll_offset, 0);
    }

    /// A normal-buffer pane (NOT alternate-screen) that has merely bottomed out
    /// its scrollback must NOT get scroll input injected into its shell: the
    /// tick is a no-op there.
    #[test]
    #[serial]
    fn autoscroll_does_not_forward_to_normal_pane() {
        let mut env = live_env_with_cursor(alt_screen_cursor(false, false, false));
        stage_edge_drag_no_scrollback(&mut env, true);
        assert!(
            !env.view.tick_preview_autoscroll(),
            "a non-alternate-screen pane never gets forwarded scroll input"
        );
        assert_eq!(env.view.preview_scroll_offset, 0);
    }

    /// A full-screen app with mouse tracking but in the LEGACY (non-SGR)
    /// encoding is still forwarded; the byte builder emits X10-encoded
    /// bytes for it instead of SGR (see `wheel_mouse_bytes_legacy_encodes_x10`).
    /// Forwarding pins the preview to the live edge like the SGR case.
    #[test]
    #[serial]
    fn wheel_over_alt_screen_legacy_mouse_forwards() {
        let mut env = live_env_with_cursor(alt_screen_cursor(true, true, false));

        let up = env.view.handle_scroll_up(50, 10);
        assert!(up, "wheel over a full-screen legacy-mouse pane is handled");
        assert_eq!(
            env.view.preview_scroll_offset, 0,
            "legacy mouse is forwarded too (X10 encoding), not dead-scrolled"
        );
    }

    /// And a normal-screen agent (no alternate screen) keeps the capture
    /// scroll even if it happens to have SGR mouse on: the preview's
    /// scrollback is genuinely useful there.
    #[test]
    #[serial]
    fn wheel_over_normal_screen_pane_uses_capture_scroll() {
        let mut env = live_env_with_cursor(alt_screen_cursor(false, true, true));

        let up = env.view.handle_scroll_up(50, 10);
        assert!(up);
        assert!(
            env.view.preview_scroll_offset > 10,
            "normal screen: capture-window scroll still drives the preview"
        );
    }

    /// Wheel-down over preview when offset is already at the bottom (0)
    /// must NOT advance the list cursor.
    #[test]
    #[serial]
    fn wheel_down_over_preview_at_bottom_does_not_move_list() {
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.preview_scroll_offset = 0;

        let handled = env.view.handle_scroll_down(50, 10);

        assert!(
            !handled,
            "expected no-op when preview is at bottom boundary"
        );
        assert_eq!(env.view.cursor, 0, "list cursor must not move");
        assert_eq!(env.view.preview_scroll_offset, 0);
    }

    /// Wheel-up over preview when there is nothing more to scroll into
    /// (no captured history) must NOT retreat the list cursor.
    #[test]
    #[serial]
    fn wheel_up_over_preview_at_top_does_not_move_list() {
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 1;
        env.view.update_selected();
        env.view.preview_scroll_offset = 0;
        env.view.preview_cache.dimensions = (80, 24);
        env.view.preview_cache.captured_lines = 10;

        let handled = env.view.handle_scroll_up(50, 10);

        assert!(
            !handled,
            "expected no-op when preview has no history to reveal"
        );
        assert_eq!(env.view.cursor, 1, "list cursor must not move");
        assert_eq!(env.view.preview_scroll_offset, 0);
    }

    /// Wheel over preview when no session is selected must NOT move the
    /// list cursor; scroll events stay in the preview pane.
    #[test]
    #[serial]
    fn wheel_over_preview_with_no_session_does_not_move_list() {
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 1;
        env.view.selected_session = None;

        let down_handled = env.view.handle_scroll_down(50, 10);
        assert!(!down_handled);
        assert_eq!(env.view.cursor, 1);

        let up_handled = env.view.handle_scroll_up(50, 10);
        assert!(!up_handled);
        assert_eq!(env.view.cursor, 1);
    }

    /// Wheel over preview with scrollable content moves the preview
    /// offset, not the list cursor.
    #[test]
    #[serial]
    fn wheel_over_preview_with_scrollable_content_moves_preview_only() {
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 1;
        env.view.update_selected();
        env.view.preview_cache.dimensions = (80, 24);
        env.view.preview_cache.captured_lines = 200;
        env.view.preview_scroll_offset = 10;

        let cursor_before = env.view.cursor;

        let up_handled = env.view.handle_scroll_up(50, 10);
        assert!(up_handled);
        assert_eq!(env.view.cursor, cursor_before, "list cursor must not move");
        assert!(
            env.view.preview_scroll_offset > 10,
            "preview should scroll back into history"
        );

        let offset_after_up = env.view.preview_scroll_offset;
        let down_handled = env.view.handle_scroll_down(50, 10);
        assert!(down_handled);
        assert_eq!(env.view.cursor, cursor_before, "list cursor must not move");
        assert!(
            env.view.preview_scroll_offset < offset_after_up,
            "preview should scroll forward"
        );
    }

    /// Wheel over the list pane still moves the list cursor (regression
    /// guard so the fix above doesn't accidentally kill list scrolling).
    #[test]
    #[serial]
    fn wheel_over_list_still_moves_list_cursor() {
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let handled = env.view.handle_scroll_down(5, 10);
        assert!(handled);
        assert_eq!(env.view.cursor, 1, "wheel over list should advance cursor");

        let handled = env.view.handle_scroll_up(5, 10);
        assert!(handled);
        assert_eq!(env.view.cursor, 0, "wheel over list should retreat cursor");
    }

    /// Live-send mode is meant to feel like an attach — users still need
    /// to scroll the preview to read agent history without exiting. The
    /// has_dialog() gate would otherwise swallow these events because
    /// live_send.is_some() participates in that predicate.
    #[test]
    #[serial]
    fn wheel_over_preview_in_live_mode_scrolls_preview() {
        use crate::tui::home::live_send::LiveSendState;
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 1;
        env.view.update_selected();
        env.view.preview_cache.dimensions = (80, 24);
        env.view.preview_cache.captured_lines = 200;
        env.view.preview_scroll_offset = 10;
        // Install live state directly so we don't have to stand up a
        // tmux session; the scroll handler only cares about
        // live_send.is_some().
        env.view.live_send = Some(LiveSendState {
            session_id: "fake".to_string(),
            title: "fake".to_string(),
            tmux_name: "fake".to_string(),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: crate::tui::home::live_send::parse_chord_list(
                crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
            ),
            leader: None,
        });

        let up_handled = env.view.handle_scroll_up(50, 10);
        assert!(up_handled, "preview scroll should work while in live mode");
        assert!(
            env.view.preview_scroll_offset > 10,
            "preview should scroll back into history"
        );
        // And we should still be in live mode (scroll doesn't exit).
        assert!(env.view.live_send.is_some());
    }

    /// List-pane wheel scroll stays suppressed in live mode: changing
    /// the selection mid-session would silently aim the next keystroke
    /// at a different pane than the preview is showing.
    #[test]
    #[serial]
    fn wheel_over_list_in_live_mode_does_not_change_selection() {
        use crate::tui::home::live_send::LiveSendState;
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 1;
        env.view.update_selected();
        env.view.live_send = Some(LiveSendState {
            session_id: "fake".to_string(),
            title: "fake".to_string(),
            tmux_name: "fake".to_string(),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: crate::tui::home::live_send::parse_chord_list(
                crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
            ),
            leader: None,
        });

        let handled = env.view.handle_scroll_down(5, 10);
        assert!(!handled, "list scroll must be a no-op in live mode");
        assert_eq!(env.view.cursor, 1, "selection must not change in live mode");
    }

    /// Build a live-send env with the default Ctrl+B leader armed and the
    /// cursor on a real session, so leader-menu keys route through
    /// `handle_live_send_key`.
    fn live_env_with_leader() -> TestEnv {
        use crate::tui::home::live_send::LiveSendState;
        let mut env = create_test_env_with_sessions(3);
        setup_panes(&mut env);
        env.view.cursor = 1;
        env.view.update_selected();
        let id = match env.view.flat_items.get(1) {
            Some(Item::Session { id, .. }) => id.clone(),
            _ => panic!("fixture should have a session at flat_items[1]"),
        };
        env.view.live_send = Some(LiveSendState {
            session_id: id,
            title: "session".to_string(),
            tmux_name: "fake".to_string(),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: crate::tui::home::live_send::parse_chord_list(
                crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
            ),
            leader: crate::tui::home::live_send::parse_chord(
                crate::tui::home::live_send::DEFAULT_LEADER,
            ),
        });
        env
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Pressing the leader arms the menu (swallowed, not forwarded);
    /// the follow-up `b` toggles the sidebar and disarms.
    #[test]
    #[serial]
    fn live_leader_b_toggles_sidebar() {
        let mut env = live_env_with_leader();
        assert!(!env.view.sidebar_collapsed);

        env.view.handle_key(ctrl('b'), None);
        assert!(
            env.view.live_send_pending_leader,
            "leader press should arm the menu"
        );
        assert!(
            !env.view.sidebar_collapsed,
            "leader alone must not toggle anything yet"
        );

        env.view.handle_key(key(KeyCode::Char('b')), None);
        assert!(!env.view.live_send_pending_leader, "menu should disarm");
        assert!(env.view.sidebar_collapsed, "leader+b hides the sidebar");

        // And again to reveal it.
        env.view.handle_key(ctrl('b'), None);
        env.view.handle_key(key(KeyCode::Char('b')), None);
        assert!(!env.view.sidebar_collapsed, "leader+b again shows it");
    }

    /// Leader + k opens the command palette over live mode.
    #[test]
    #[serial]
    fn live_leader_k_opens_palette() {
        let mut env = live_env_with_leader();
        env.view.handle_key(ctrl('b'), None);
        env.view.handle_key(key(KeyCode::Char('k')), None);
        assert!(!env.view.live_send_pending_leader);
        assert!(
            env.view.command_palette.is_some(),
            "leader+k should open the command palette"
        );
        // Live mode is still active underneath the palette overlay.
        assert!(env.view.live_send.is_some());
    }

    /// Leader + q exits live mode and disarms the leader menu. The sidebar
    /// collapse is now a persisted general state, so exiting live mode
    /// deliberately leaves it as the user set it (no force-reveal).
    #[test]
    #[serial]
    fn live_leader_q_exits() {
        let mut env = live_env_with_leader();
        env.view.sidebar_collapsed = true;
        env.view.handle_key(ctrl('b'), None);
        env.view.handle_key(key(KeyCode::Char('q')), None);
        assert!(env.view.live_send.is_none(), "leader+q exits live mode");
        assert!(
            env.view.sidebar_collapsed,
            "collapse is persisted, not reset on live exit"
        );
        assert!(!env.view.live_send_pending_leader);
    }

    /// An unbound key after the leader cancels the menu without exiting,
    /// toggling, or opening anything (it does not fall through to the
    /// agent either: the leader already swallowed it).
    #[test]
    #[serial]
    fn live_leader_unknown_key_cancels_menu() {
        let mut env = live_env_with_leader();
        env.view.handle_key(ctrl('b'), None);
        env.view.handle_key(key(KeyCode::Char('z')), None);
        assert!(!env.view.live_send_pending_leader, "menu disarms");
        assert!(env.view.live_send.is_some(), "still live");
        assert!(!env.view.sidebar_collapsed);
        assert!(env.view.command_palette.is_none());
    }

    /// The fast exit chord (Ctrl+Q) stays a single press, independent of
    /// the leader: it must not require arming the menu first.
    #[test]
    #[serial]
    fn live_ctrl_q_still_one_press_exit() {
        let mut env = live_env_with_leader();
        env.view.handle_key(ctrl('q'), None);
        assert!(
            env.view.live_send.is_none(),
            "Ctrl+Q exits in a single press"
        );
        assert!(!env.view.live_send_pending_leader);
    }

    /// A modified key after the leader (e.g. Ctrl+K) cancels the menu
    /// rather than firing a command: only the leader-again passthrough
    /// claims a modified form, so the user can't accidentally trigger the
    /// palette by holding Ctrl out of muscle memory.
    #[test]
    #[serial]
    fn live_leader_then_modified_key_cancels() {
        let mut env = live_env_with_leader();
        env.view.handle_key(ctrl('b'), None);
        env.view.handle_key(ctrl('k'), None);
        assert!(!env.view.live_send_pending_leader, "menu disarms");
        assert!(
            env.view.command_palette.is_none(),
            "leader + Ctrl+K must NOT open the palette"
        );
        assert!(env.view.live_send.is_some(), "still live");
    }

    /// Committing a palette command while live (here a jump) exits live
    /// mode first, so the preview can never show one session while
    /// keystrokes target another. Cancelling the palette is covered
    /// separately and must stay live.
    #[test]
    #[serial]
    fn palette_command_while_live_exits_live() {
        let mut env = live_env_with_leader();
        // Open the palette from within live mode via the leader.
        env.view.handle_key(ctrl('b'), None);
        env.view.handle_key(key(KeyCode::Char('k')), None);
        assert!(env.view.command_palette.is_some());
        assert!(env.view.live_send.is_some(), "palette opens over live mode");

        // Filter to a jump entry and commit it.
        for ch in "jump".chars() {
            env.view.handle_key(key(KeyCode::Char(ch)), None);
        }
        env.view.handle_key(key(KeyCode::Enter), None);

        assert!(
            env.view.live_send.is_none(),
            "committing a palette command must drop out of live mode"
        );
        assert!(env.view.command_palette.is_none());
        assert!(
            !env.view.sidebar_collapsed,
            "sidebar was never collapsed, so it stays expanded"
        );
    }

    /// Collapsing the sidebar in live mode hands the preview the full
    /// width: the preview sub-rect grows past the normal side-by-side
    /// width, and rendering the which-key banner doesn't panic.
    #[test]
    #[serial]
    fn collapsed_sidebar_gives_preview_full_width() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut env = live_env_with_leader();
        let theme = crate::tui::styles::load_theme("empire");

        let render = |env: &mut TestEnv| {
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    let area = f.area();
                    env.view.render(f, area, &theme, None, None, None);
                })
                .unwrap();
            env.view.preview_pane_area.width
        };

        let split_width = render(&mut env);
        env.view.sidebar_collapsed = true;
        let full_width = render(&mut env);
        assert!(
            full_width > split_width,
            "collapsed sidebar should widen the preview ({full_width} vs {split_width})"
        );
        // The list isn't drawn while collapsed, so its hit-test rects must
        // be cleared or a click in the preview area could resolve to a
        // hidden list row.
        assert!(
            env.view.list_inner_area.width == 0 && env.view.list_inner_area.height == 0,
            "collapsed sidebar must clear the list hit-test rect"
        );
        assert!(
            env.view.handle_click(2, 2).is_none(),
            "a click in collapsed live mode must not resolve to a list row"
        );

        // The which-key banner renders without panicking while armed.
        env.view.live_send_pending_leader = true;
        let _ = render(&mut env);
    }

    /// The collapse button (expanded) and the strip (collapsed) are
    /// click-toggle affordances: clicking the button collapses, clicking
    /// the strip re-expands, and each reports its hit rect while the other
    /// is cleared.
    #[test]
    #[serial]
    fn sidebar_collapse_button_and_strip_toggle() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        let mut env = create_test_env_with_sessions(3);
        let theme = crate::tui::styles::load_theme("empire");

        let render = |env: &mut TestEnv| {
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    let area = f.area();
                    env.view.render(f, area, &theme, None, None, None);
                })
                .unwrap();
        };

        // Expanded: the collapse button has a real rect; clicking it collapses.
        render(&mut env);
        assert!(!env.view.sidebar_collapsed);
        let btn = env.view.collapse_button_area;
        assert!(
            btn.width > 0 && btn.height > 0,
            "collapse button must have a hit rect while expanded"
        );
        assert!(
            env.view.handle_sidebar_collapse_click(btn.x, btn.y),
            "clicking the collapse button is consumed"
        );
        assert!(
            env.view.sidebar_collapsed,
            "collapse button click collapses the sidebar"
        );

        // Collapsed: the strip has a real rect, the button rect is cleared,
        // and clicking the strip re-expands.
        render(&mut env);
        let strip = env.view.expand_strip_area;
        assert!(
            strip.width > 0 && strip.height > 0,
            "collapsed strip must have a hit rect"
        );
        assert_eq!(
            env.view.collapse_button_area,
            Rect::default(),
            "collapse button rect cleared while collapsed"
        );
        assert!(
            env.view
                .handle_sidebar_collapse_click(strip.x + 1, strip.y + 1),
            "clicking the strip is consumed"
        );
        assert!(
            !env.view.sidebar_collapsed,
            "strip click re-expands the sidebar"
        );
    }

    /// A takeover view (settings/diff/serve) returns early in `render`
    /// before the home-view paths run, so the collapse/expand and footer
    /// hit rects must be cleared up front. Otherwise a stale rect from the
    /// prior home frame could swallow a click on the takeover surface (the
    /// collapse handler runs ahead of `hit_diff`).
    #[test]
    #[serial]
    fn takeover_view_clears_sidebar_hit_rects() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        let mut env = create_test_env_with_sessions(3);
        let theme = crate::tui::styles::load_theme("empire");
        let render = |env: &mut TestEnv| {
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    let area = f.area();
                    env.view.render(f, area, &theme, None, None, None);
                })
                .unwrap();
        };

        // Home view populates the collapse button + footer rects.
        render(&mut env);
        assert!(env.view.collapse_button_area.width > 0);
        assert!(!env.view.footer_buttons.is_empty());

        // Opening settings is a full-screen takeover; the next render must
        // clear the stale rects so a click can't toggle the hidden sidebar.
        env.view.settings_view =
            Some(crate::tui::settings::SettingsView::new("test", None).unwrap());
        render(&mut env);
        assert_eq!(env.view.collapse_button_area, Rect::default());
        assert_eq!(env.view.expand_strip_area, Rect::default());
        assert!(env.view.footer_buttons.is_empty());
        assert!(
            !env.view.handle_sidebar_collapse_click(0, 0),
            "no sidebar rect can be hit while a takeover view owns the screen"
        );
    }
}

mod footer_toolbar {
    //! The footer hint bar is a clickable toolbar: each shortcut renders a
    //! hit rect paired with the key it synthesizes, a click dispatches that
    //! key through the normal handler, and hover highlights the button.
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_at(env: &mut TestEnv, w: u16, h: u16) {
        let theme = crate::tui::styles::load_theme("empire");
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
    }

    fn button_key(env: &TestEnv, code: KeyCode) -> Option<KeyEvent> {
        env.view
            .footer_buttons
            .iter()
            .find(|(_, k)| k.code == code)
            .map(|(_, k)| *k)
    }

    /// Each rendered shortcut produces a hit rect carrying the equivalent
    /// key, and `footer_button_at` resolves a click inside one to that key.
    #[test]
    #[serial]
    fn buttons_map_clicks_to_shortcuts() {
        let mut env = create_test_env_with_sessions(3);
        render_at(&mut env, 120, 12);

        assert!(
            !env.view.footer_buttons.is_empty(),
            "footer should expose clickable buttons"
        );
        // New / View / Group / Cmds are always present in the home view.
        assert_eq!(
            button_key(&env, KeyCode::Char('n')).map(|k| k.modifiers),
            Some(KeyModifiers::NONE),
            "New maps to a plain 'n'"
        );
        let cmds = button_key(&env, KeyCode::Char('k')).expect("Cmds button present");
        assert_eq!(cmds.modifiers, KeyModifiers::CONTROL, "Cmds maps to Ctrl+K");

        // A click inside the New button's rect resolves to its key; a click
        // outside every button resolves to nothing.
        let (new_rect, _) = env
            .view
            .footer_buttons
            .iter()
            .find(|(_, k)| k.code == KeyCode::Char('n'))
            .cloned()
            .expect("New button rect");
        assert_eq!(
            env.view
                .footer_button_at(new_rect.x, new_rect.y)
                .map(|k| k.code),
            Some(KeyCode::Char('n'))
        );
        assert!(
            env.view
                .footer_button_at(new_rect.x, new_rect.y + 5)
                .is_none(),
            "a click off the footer row hits no button"
        );
    }

    /// Dispatching a footer button's key runs the same action as the
    /// keypress: the New button opens the new-session dialog.
    #[test]
    #[serial]
    fn clicking_new_opens_dialog() {
        let mut env = create_test_env_with_sessions(3);
        render_at(&mut env, 120, 12);
        let key = button_key(&env, KeyCode::Char('n')).expect("New button");
        assert!(env.view.new_dialog.is_none());
        env.view.handle_key(key, None);
        assert!(
            env.view.new_dialog.is_some(),
            "clicking New opens the new-session dialog"
        );
    }

    /// While a non-live overlay (here the help screen) is open, the footer
    /// is drawn underneath but the overlay owns clicks: `footer_button_at`
    /// and `handle_sidebar_collapse_click` must report no hit so a click
    /// can't fire a shortcut or toggle the sidebar behind the modal.
    #[test]
    #[serial]
    fn overlay_blocks_footer_and_sidebar_clicks() {
        let mut env = create_test_env_with_sessions(3);
        render_at(&mut env, 120, 12);
        let (rect, _) = env.view.footer_buttons[0];
        // No overlay: the button resolves.
        assert!(env.view.footer_button_at(rect.x, rect.y).is_some());

        env.view.show_help = true;
        assert!(
            env.view.has_non_live_send_overlay(),
            "help screen is a non-live overlay"
        );
        assert!(
            env.view.footer_button_at(rect.x, rect.y).is_none(),
            "footer click is blocked while an overlay owns the screen"
        );
        assert!(
            !env.view.handle_sidebar_collapse_click(0, 0),
            "sidebar toggle is blocked while an overlay owns the screen"
        );
    }

    /// Strict-hotkey mode shifts the chords: Diff becomes Ctrl+D and Delete
    /// becomes an uppercase 'D', and the buttons synthesize those exactly.
    #[test]
    #[serial]
    fn strict_mode_buttons_carry_shifted_chords() {
        let mut env = create_test_env_with_sessions(3);
        env.view.strict_hotkeys = true;
        render_at(&mut env, 120, 12);

        let diff = button_key(&env, KeyCode::Char('d')).expect("Diff button (strict ^D)");
        assert_eq!(
            diff.modifiers,
            KeyModifiers::CONTROL,
            "strict Diff is Ctrl+D"
        );
        assert!(
            button_key(&env, KeyCode::Char('D')).is_some(),
            "strict Delete is an uppercase D"
        );
    }

    /// Hover sets `footer_hover` to the button under the pointer, reports the
    /// change, and clears when the pointer leaves the footer.
    #[test]
    #[serial]
    fn hover_tracks_button_under_pointer() {
        let mut env = create_test_env_with_sessions(3);
        render_at(&mut env, 120, 12);
        let (rect, key) = env.view.footer_buttons[1];

        assert!(env.view.footer_hover.is_none());
        let changed = env.view.handle_hover(rect.x + 1, rect.y);
        assert!(changed, "moving onto a button is a hover change");
        assert_eq!(env.view.footer_hover, Some(key));

        // Same button, no change reported.
        assert!(!env.view.handle_hover(rect.x, rect.y));

        // Off the footer row clears the hover.
        let changed = env.view.handle_hover(rect.x, rect.y.saturating_sub(5));
        assert!(changed);
        assert!(env.view.footer_hover.is_none());
    }

    /// The footer is replaced by the live-send banner, so it exposes no
    /// clickable buttons while live mode owns the status bar.
    #[test]
    #[serial]
    fn no_buttons_during_live_send() {
        let mut env = create_test_env_with_sessions(3);
        render_at(&mut env, 120, 12);
        assert!(!env.view.footer_buttons.is_empty());

        env.view.cursor = 1;
        env.view.update_selected();
        let id = match env.view.flat_items.get(1) {
            Some(Item::Session { id, .. }) => id.clone(),
            _ => panic!("expected a session at flat_items[1]"),
        };
        env.view.live_send = Some(crate::tui::home::live_send::LiveSendState {
            session_id: id,
            title: "s".to_string(),
            tmux_name: "fake".to_string(),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: crate::tui::home::live_send::parse_chord_list(
                crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
            ),
            leader: None,
        });
        render_at(&mut env, 120, 12);
        assert!(
            env.view.footer_buttons.is_empty(),
            "live-send banner replaces the footer toolbar"
        );
        assert_eq!(env.view.footer_button_at(0, 11), None);
    }
}

mod click_to_select {
    //! Left-click on a session row in the list selects it (same effect as
    //! arrow-key navigation). Clicks outside the inner list rect, clicks on
    //! a row past the last item, and clicks while a dialog is open are
    //! no-ops.

    use super::*;
    use ratatui::layout::Rect;

    /// Inner rect chosen with comfortable headroom so all sessions fit
    /// without "[N more above/below]" indicators consuming a row.
    fn setup_inner(env: &mut TestEnv) {
        env.view.list_inner_area = Rect::new(1, 1, 28, 10);
    }

    #[test]
    #[serial]
    fn click_selects_session_at_clicked_row() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        // Click the third visible row (inner.y + 2 == 3) -> flat_items[2].
        // Single-click on a session row both selects it AND requests
        // live-send mode for that row.
        let action = env.view.handle_click(5, 3);
        let expected_id = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };
        assert_eq!(
            action,
            Some(crate::tui::app::Action::EnterLiveSend(expected_id)),
            "single click should select the row and request live mode"
        );
        assert_eq!(env.view.cursor, 2);
    }

    #[test]
    #[serial]
    fn select_only_click_moves_cursor_without_entering_live_mode() {
        // With `click_action = SelectOnly`, a single click must move the
        // cursor (so the preview pane updates) but NOT emit
        // EnterLiveSend. Double-click + Enter still activate the row,
        // but that path is gated by `default_attach_mode`, not this
        // setting, so it's exercised elsewhere.
        use crate::session::config::{update_config, ClickAction};
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        update_config(|config| {
            config.session.click_action = ClickAction::SelectOnly;
        })
        .unwrap();

        let action = env.view.handle_click(5, 3);
        assert_eq!(
            action, None,
            "SelectOnly must not emit EnterLiveSend on single click"
        );
        assert_eq!(
            env.view.cursor, 2,
            "SelectOnly must still move the cursor to the clicked row"
        );
    }

    #[test]
    #[serial]
    fn select_only_click_on_different_row_exits_live_mode() {
        // With `click_action = SelectOnly`, clicking a *different* row while
        // live-sending must leave live mode (otherwise keystrokes stay aimed at
        // the old session while the cursor/preview walk away). The click still
        // emits no action and still moves the cursor.
        use crate::session::config::{update_config, ClickAction};
        use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        update_config(|config| {
            config.session.click_action = ClickAction::SelectOnly;
        })
        .unwrap();

        let live_id = env.view.selected_session.clone().unwrap();
        env.view.live_send = Some(LiveSendState {
            session_id: live_id,
            title: "live".to_string(),
            tmux_name: "aoe_test_live".to_string(),
            target: LiveSendTarget::Agent,
            exit_chords: Vec::new(),
            leader: None,
        });

        let action = env.view.handle_click(5, 3);
        assert_eq!(action, None, "SelectOnly click never emits an action");
        assert_eq!(env.view.cursor, 2, "the click still moves the cursor");
        assert!(
            env.view.live_send.is_none(),
            "clicking a different row in SelectOnly mode must exit live mode"
        );
    }

    #[test]
    #[serial]
    fn select_only_click_on_live_row_stays_live() {
        // Clicking the row that's already live-sending is not a "leave" gesture:
        // the cursor is already there, so SelectOnly must not tear down live mode.
        use crate::session::config::{update_config, ClickAction};
        use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        // Row 3 resolves to index 2, so make index 2 the live row.
        env.view.cursor = 2;
        env.view.update_selected();

        update_config(|config| {
            config.session.click_action = ClickAction::SelectOnly;
        })
        .unwrap();

        let live_id = env.view.selected_session.clone().unwrap();
        env.view.live_send = Some(LiveSendState {
            session_id: live_id,
            title: "live".to_string(),
            tmux_name: "aoe_test_live".to_string(),
            target: LiveSendTarget::Agent,
            exit_chords: Vec::new(),
            leader: None,
        });

        let action = env.view.handle_click(5, 3);
        assert_eq!(action, None, "SelectOnly click never emits an action");
        assert!(
            env.view.live_send.is_some(),
            "clicking the already-live row must not exit live mode"
        );
    }

    #[test]
    #[serial]
    fn single_click_on_archived_row_selects_without_reviving() {
        // A parked (archived) session has had its pane killed. Single-clicking
        // it is a "let me look" gesture and must NOT resurrect it: no
        // EnterLiveSend (which would respawn the pane) and the session stays
        // archived (no auto-unarchive). This holds even under the default
        // `click_action = LiveSend`. Bringing it back stays explicit: `z`,
        // double-click, or Enter.
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        // Keep archived rows visible so the archived row is clickable.
        env.view.archived_section_collapsed = false;

        // Archive the row at cursor 0.
        env.view.cursor = 0;
        env.view.update_selected();
        let archived_id = env.view.selected_session.clone().unwrap();
        env.view.toggle_archive_at_cursor().unwrap();
        assert!(
            env.view.get_instance(&archived_id).unwrap().is_archived(),
            "precondition: the session must be archived"
        );

        // Locate the archived row in the flat list and click it.
        let idx = env
            .view
            .flat_items
            .iter()
            .position(|it| matches!(it, Item::Session { id, .. } if id == &archived_id))
            .expect("archived session must render under the expanded Archived section");
        // Archived rows now live in the pinned shelf, so render a real frame to
        // populate `shelf_inner_area` and click the shelf row, not the faked
        // list rect.
        render_geometry(&mut env.view);
        let row = shelf_row_for_idx(&env.view, idx);
        let action = env.view.handle_click(5, row);

        assert_eq!(
            action, None,
            "single click on an archived row must not request live-send"
        );
        assert!(
            env.view.live_send.is_none(),
            "single click on an archived row must not enter live-send mode"
        );
        assert!(
            env.view.get_instance(&archived_id).unwrap().is_archived(),
            "single click on an archived row must not unarchive it"
        );
        assert_eq!(
            env.view.selected_session.as_deref(),
            Some(archived_id.as_str()),
            "single click should still select the archived row"
        );
    }

    #[test]
    #[serial]
    fn select_only_click_honors_per_profile_override() {
        // Global stays LiveSend (default) but the test profile pins
        // SelectOnly via SessionConfigOverride. The resolver must
        // pick the profile override, not the global default, so a
        // single click returns None and the cursor still moves.
        use crate::session::profile_config::{save_profile_config, ProfileConfig};
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let profile_config: ProfileConfig =
            serde_json::from_value(serde_json::json!({"session": {"click_action": "select_only"}}))
                .unwrap();
        save_profile_config("test", &profile_config).unwrap();

        let action = env.view.handle_click(5, 3);
        assert_eq!(
            action, None,
            "per-profile SelectOnly must override the LiveSend global default"
        );
        assert_eq!(env.view.cursor, 2);
    }

    #[test]
    #[serial]
    fn double_click_still_attaches_under_select_only() {
        // Defensive: `SelectOnly` only changes single-click; double-click
        // must still activate the row via `default_attach_mode` (Tmux by
        // default, so we expect AttachSession). Locks down the
        // separation between the two settings so a future refactor
        // can't accidentally route double-click through `click_action`.
        use crate::session::config::{update_config, ClickAction};
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        update_config(|config| {
            config.session.click_action = ClickAction::SelectOnly;
        })
        .unwrap();

        let t0 = std::time::Instant::now();
        let first = env.view.handle_click_at(t0, 5, 3);
        assert_eq!(
            first, None,
            "first click under SelectOnly must not emit an action"
        );
        let t1 = t0 + std::time::Duration::from_millis(100);
        let second = env.view.handle_click_at(t1, 5, 3);
        let expected_id = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };
        assert_eq!(
            second,
            Some(crate::tui::app::Action::AttachSession(expected_id)),
            "double-click must still activate via default_attach_mode (Tmux)"
        );
    }

    #[test]
    #[serial]
    fn double_click_tears_down_live_send_before_tmux_attach() {
        // Regression for #2290: with `click_action = LiveSend` the first
        // click of a double-click enters live-send for the row, then the
        // second click resolves to a tmux attach via
        // `default_attach_mode = Tmux`. The attach must exit live mode
        // first, otherwise the worker is stranded against a pane we're
        // leaving and detaching drops the user back into live mode.
        use crate::session::config::{update_config, ClickAction};
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        update_config(|config| {
            config.session.click_action = ClickAction::LiveSend;
        })
        .unwrap();

        // Simulate the first click of the double having already entered
        // live-send (the real install runs in App::execute_action, which a
        // HomeView unit test can't drive): pin live_send to the row we are
        // about to double-click.
        let expected_id = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };
        env.view.live_send = Some(crate::tui::home::live_send::LiveSendState {
            session_id: expected_id.clone(),
            title: "row-2".to_string(),
            tmux_name: "aoe_test_2290".to_string(),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: Vec::new(),
            leader: None,
        });

        let t0 = std::time::Instant::now();
        // Seed last_click so the next click within the threshold is treated
        // as the second click of a double-click on the same row.
        env.view.last_click = Some((t0, 5, 3));
        let t1 = t0 + std::time::Duration::from_millis(100);
        let action = env.view.handle_click_at(t1, 5, 3);

        assert_eq!(
            action,
            Some(crate::tui::app::Action::AttachSession(expected_id)),
            "double-click must still resolve to a tmux attach under default_attach_mode (Tmux)"
        );
        assert!(
            env.view.live_send.is_none(),
            "the tmux attach path must exit live mode first, not strand the worker"
        );
    }

    #[test]
    #[serial]
    fn click_on_already_selected_row_does_not_move_cursor() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 1;
        env.view.update_selected();

        // Re-clicking the already-selected row still requests live mode
        // (the row is now eligible to be the live target); cursor stays
        // put.
        let action = env.view.handle_click(5, 2);
        let expected_id = match &env.view.flat_items[1] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[1] should be a session"),
        };
        assert_eq!(
            action,
            Some(crate::tui::app::Action::EnterLiveSend(expected_id))
        );
        assert_eq!(env.view.cursor, 1);
    }

    #[test]
    #[serial]
    fn click_below_last_item_is_noop() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        // inner.y=1, three items occupy rows 1..=3. Row 5 is inside the
        // inner rect but past the last item.
        let action = env.view.handle_click(5, 5);
        assert!(action.is_none());
        assert_eq!(env.view.cursor, 0);
    }

    #[test]
    #[serial]
    fn click_outside_inner_rect_is_noop() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        // Row 0 is above inner.y; column 50 is past inner.x + inner.width.
        assert!(env.view.handle_click(5, 0).is_none());
        assert!(env.view.handle_click(50, 2).is_none());
        assert_eq!(env.view.cursor, 0);
    }

    #[test]
    #[serial]
    fn click_with_dialog_open_is_noop() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.show_help = true;

        let action = env.view.handle_click(5, 3);
        assert!(action.is_none(), "dialog should swallow the click");
        assert_eq!(env.view.cursor, 0);
    }

    #[test]
    #[serial]
    fn double_click_on_session_returns_attach_action() {
        use std::time::{Duration, Instant};

        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();
        let expected_id = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };

        let t0 = Instant::now();
        let first = env.view.handle_click_at(t0, 5, 3);
        assert_eq!(
            first,
            Some(crate::tui::app::Action::EnterLiveSend(expected_id.clone())),
            "first click selects and requests live mode"
        );
        assert_eq!(env.view.cursor, 2);

        let t1 = t0 + Duration::from_millis(150);
        let second = env.view.handle_click_at(t1, 5, 3);
        assert_eq!(
            second,
            Some(crate::tui::app::Action::AttachSession(expected_id)),
            "second click within threshold should attach the session"
        );
    }

    /// A double-click on the preview pane produces the SAME activation Action a
    /// sidebar double-click would (parity gesture): the first press is a no-op
    /// that records timing, the second within threshold attaches the previewed
    /// session.
    #[test]
    #[serial]
    fn preview_double_click_attaches_like_sidebar() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        use std::time::{Duration, Instant};

        let mut env = create_test_env_with_sessions(3);
        env.view.preview_area = Rect::new(30, 0, 100, 40);
        env.view.cursor = 1;
        env.view.update_selected();
        let expected_id = env
            .view
            .selected_session
            .clone()
            .expect("a session is selected");

        // (50, 10) is inside preview_area (30, 0, 100, 40).
        let t0 = Instant::now();
        assert_eq!(
            env.view.preview_double_click_action_at(
                t0,
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                50,
                10
            ),
            None,
            "a single preview press does not activate"
        );
        let t1 = t0 + Duration::from_millis(150);
        assert_eq!(
            env.view.preview_double_click_action_at(
                t1,
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                50,
                10
            ),
            Some(crate::tui::app::Action::AttachSession(expected_id)),
            "a double-click on the preview attaches the session, same as the sidebar"
        );
    }

    /// Shift+press (aoe's own selection escape hatch) and presses outside the
    /// preview never activate, even repeated within the double-click window.
    #[test]
    #[serial]
    fn preview_shift_and_off_pane_presses_never_activate() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        use std::time::{Duration, Instant};

        let mut env = create_test_env_with_sessions(3);
        env.view.preview_area = Rect::new(30, 0, 100, 40);
        env.view.cursor = 1;
        env.view.update_selected();

        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(150);
        let down = MouseEventKind::Down(MouseButton::Left);
        // Shift falls through to aoe selection: never tracked, never activates.
        assert_eq!(
            env.view
                .preview_double_click_action_at(t0, down, KeyModifiers::SHIFT, 50, 10),
            None
        );
        assert_eq!(
            env.view
                .preview_double_click_action_at(t1, down, KeyModifiers::SHIFT, 50, 10),
            None
        );
        // A press in the list area (5, 3), outside the preview, is ignored too.
        assert_eq!(
            env.view
                .preview_double_click_action_at(t0, down, KeyModifiers::NONE, 5, 3),
            None
        );
        assert_eq!(
            env.view
                .preview_double_click_action_at(t1, down, KeyModifiers::NONE, 5, 3),
            None
        );
    }

    /// Two presses on the same preview row but different columns are unrelated
    /// clicks (e.g. tapping two different words), not a double-click: only a
    /// same-cell second press within the window activates.
    #[test]
    #[serial]
    fn preview_two_presses_on_same_row_different_col_do_not_activate() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        use std::time::{Duration, Instant};

        let mut env = create_test_env_with_sessions(3);
        env.view.preview_area = Rect::new(30, 0, 100, 40);
        env.view.cursor = 1;
        env.view.update_selected();

        let down = MouseEventKind::Down(MouseButton::Left);
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(150);
        // Same row 10, columns 40 then 70: within the time window but a
        // different cell, so the second press is a fresh single click.
        assert_eq!(
            env.view
                .preview_double_click_action_at(t0, down, KeyModifiers::NONE, 40, 10),
            None
        );
        assert_eq!(
            env.view
                .preview_double_click_action_at(t1, down, KeyModifiers::NONE, 70, 10),
            None,
            "a different-column second press on the same row must not activate"
        );
    }

    #[test]
    #[serial]
    fn two_clicks_on_different_rows_do_not_activate() {
        use std::time::{Duration, Instant};

        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let id_row2 = match &env.view.flat_items[1] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[1] should be a session"),
        };
        let id_row3 = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };

        let t0 = Instant::now();
        let first = env.view.handle_click_at(t0, 5, 2);
        assert_eq!(
            first,
            Some(crate::tui::app::Action::EnterLiveSend(id_row2)),
            "first click enters live mode for its row"
        );
        let t1 = t0 + Duration::from_millis(100);
        let second = env.view.handle_click_at(t1, 5, 3);
        assert_eq!(
            second,
            Some(crate::tui::app::Action::EnterLiveSend(id_row3)),
            "different-row second click is a fresh single click that switches the live target, not a double-click attach"
        );
        assert_eq!(env.view.cursor, 2);
    }

    #[test]
    #[serial]
    fn click_after_threshold_does_not_activate() {
        use std::time::{Duration, Instant};

        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();
        let id_row3 = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };

        let t0 = Instant::now();
        env.view.handle_click_at(t0, 5, 3);
        let t1 = t0 + Duration::from_millis(1500);
        let action = env.view.handle_click_at(t1, 5, 3);
        // Past the double-click threshold the second click is a fresh
        // single click that re-requests live mode for the row; it
        // never attaches.
        assert_eq!(
            action,
            Some(crate::tui::app::Action::EnterLiveSend(id_row3))
        );
    }

    #[test]
    #[serial]
    fn double_click_activates_clicked_row_even_if_cursor_moved_between_clicks() {
        use std::time::{Duration, Instant};

        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        // Capture the id at flat_items[2] so we know which session
        // the row-3 click is targeting.
        let clicked_id = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };

        let t0 = Instant::now();
        let first = env.view.handle_click_at(t0, 5, 3);
        assert_eq!(
            first,
            Some(crate::tui::app::Action::EnterLiveSend(clicked_id.clone()))
        );
        assert_eq!(env.view.cursor, 2);

        // Simulate the cursor drifting away between clicks (e.g., a
        // keyboard arrow press or an async list refresh that selected
        // a different row).
        env.view.cursor = 0;
        env.view.update_selected();

        let t1 = t0 + Duration::from_millis(150);
        let action = env.view.handle_click_at(t1, 5, 3);
        assert_eq!(
            action,
            Some(crate::tui::app::Action::AttachSession(clicked_id)),
            "double-click must activate the row that was clicked, \
             not whatever the cursor drifted to"
        );
        assert_eq!(
            env.view.cursor, 2,
            "double-click should also re-sync cursor onto the clicked row"
        );
    }

    #[test]
    #[serial]
    fn double_click_on_creating_session_returns_no_action() {
        use std::time::{Duration, Instant};

        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);

        // Force the target session into Creating; activation must bail.
        let target_id = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };
        env.view.mutate_instance(&target_id, |inst| {
            inst.status = crate::session::Status::Creating;
        });

        let t0 = Instant::now();
        env.view.handle_click_at(t0, 5, 3);
        let t1 = t0 + Duration::from_millis(150);
        let action = env.view.handle_click_at(t1, 5, 3);
        assert!(
            action.is_none(),
            "Creating sessions are not attachable; double-click should noop"
        );
    }

    /// Single click on a session row enters live-send mode for that
    /// session (the same `Action::EnterLiveSend` that Tab emits) in
    /// addition to selecting the row.
    #[test]
    #[serial]
    fn single_click_on_session_emits_enter_live_send() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let target_id = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };

        let action = env.view.handle_click(5, 3);
        assert_eq!(
            action,
            Some(crate::tui::app::Action::EnterLiveSend(target_id))
        );
        assert_eq!(env.view.cursor, 2);
    }

    /// Already in live mode for session A; clicking a different
    /// session row emits `EnterLiveSend(B)` so the caller can switch
    /// the live target.
    #[test]
    #[serial]
    fn click_on_other_session_while_live_switches_target() {
        use crate::tui::home::live_send::LiveSendState;

        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let id_a = match &env.view.flat_items[1] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[1] should be a session"),
        };
        let id_b = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };

        // Simulate already being in live mode for session A.
        env.view.live_send = Some(LiveSendState {
            session_id: id_a.clone(),
            title: "session1".to_string(),
            tmux_name: format!("aoe_test_{}", id_a),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: Vec::new(),
            leader: None,
        });

        // Click session B's row.
        let action = env.view.handle_click(5, 3);
        assert_eq!(
            action,
            Some(crate::tui::app::Action::EnterLiveSend(id_b)),
            "clicking a different session row while live must switch the live target"
        );
    }

    /// Clicking the row that is already the live-send target is a
    /// no-op: re-running `prepare_live_send` would drop the worker and
    /// re-do ensure_pane_ready for no reason.
    #[test]
    #[serial]
    fn click_on_already_live_session_is_noop() {
        use crate::tui::home::live_send::LiveSendState;

        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let id_a = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };

        env.view.live_send = Some(LiveSendState {
            session_id: id_a.clone(),
            title: "session2".to_string(),
            tmux_name: format!("aoe_test_{}", id_a),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: Vec::new(),
            leader: None,
        });

        let action = env.view.handle_click(5, 3);
        assert!(
            action.is_none(),
            "clicking the already-live session row should not re-enter live mode"
        );
        assert_eq!(env.view.cursor, 2, "selection still updates");
    }

    /// Creating/Deleting sessions can't host live mode, so a single
    /// click selects the row but emits no action.
    #[test]
    #[serial]
    fn single_click_on_creating_session_returns_no_action() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let target_id = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };
        env.view.mutate_instance(&target_id, |inst| {
            inst.status = crate::session::Status::Creating;
        });

        let action = env.view.handle_click(5, 3);
        assert!(
            action.is_none(),
            "Creating sessions can't enter live mode; click is a selection only"
        );
        assert_eq!(env.view.cursor, 2);
    }

    /// Acp-mode sessions are not tmux-backed, so click cannot
    /// enter live mode for them; selection still updates.
    #[cfg(feature = "serve")]
    #[test]
    #[serial]
    fn single_click_on_acp_session_returns_no_action() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let target_id = match &env.view.flat_items[2] {
            crate::session::Item::Session { id, .. } => id.clone(),
            _ => panic!("flat_items[2] should be a session"),
        };
        env.view.mutate_instance(&target_id, |inst| {
            inst.view = crate::session::View::Structured;
        });

        let action = env.view.handle_click(5, 3);
        assert!(
            action.is_none(),
            "Acp sessions can't enter live mode; click is a selection only"
        );
        assert_eq!(env.view.cursor, 2);
    }

    #[test]
    #[serial]
    fn hover_sets_resolved_index_for_row_under_mouse() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);

        let changed = env.view.handle_hover(5, 3);
        assert!(
            changed,
            "first hover over a fresh row should request redraw"
        );
        assert_eq!(env.view.hovered_index(), Some(2));
    }

    #[test]
    #[serial]
    fn hover_moving_to_a_new_row_requests_redraw() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);

        env.view.handle_hover(5, 1);
        let changed = env.view.handle_hover(5, 2);
        assert!(changed);
        assert_eq!(env.view.hovered_index(), Some(1));
    }

    #[test]
    #[serial]
    fn hover_pixel_twitch_on_same_row_is_noop() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);

        env.view.handle_hover(5, 2);
        let changed = env.view.handle_hover(6, 2);
        assert!(
            !changed,
            "same-row movement should not trigger a redraw request"
        );
        assert_eq!(env.view.hovered_index(), Some(1));
    }

    #[test]
    #[serial]
    fn hover_leaving_list_clears_resolved_index() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);

        env.view.handle_hover(5, 2);
        assert_eq!(env.view.hovered_index(), Some(1));

        // Row 0 is above the inner rect (inner.y = 1).
        let changed = env.view.handle_hover(5, 0);
        assert!(changed, "leaving the list should request a redraw");
        assert_eq!(env.view.hovered_index(), None);
    }

    #[test]
    #[serial]
    fn hover_resolves_to_none_when_dialog_open() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);

        env.view.show_help = true;
        env.view.handle_hover(5, 2);
        assert_eq!(env.view.hovered_index(), None);
    }

    #[test]
    #[serial]
    fn move_cursor_clears_hover() {
        // Repro for the keyboard-after-hover stuck-highlight bug: when
        // mosh (or any prediction layer) eats the off-list `Moved` event,
        // `mouse_pos` stays stuck on the row the mouse last touched while
        // the keyboard moves to a new row, painting two rows at once.
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);

        env.view.handle_hover(5, 2);
        assert_eq!(env.view.hovered_index(), Some(1));

        env.view.move_cursor(1);
        assert_eq!(
            env.view.hovered_index(),
            None,
            "keyboard nav must clear hover so only the selected row paints"
        );
    }

    #[test]
    #[serial]
    fn hover_below_last_item_resolves_to_none() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);

        env.view.handle_hover(5, 5);
        assert_eq!(env.view.hovered_index(), None);
    }

    #[test]
    #[serial]
    fn click_on_group_row_toggles_collapsed() {
        let mut env = create_test_env_with_mixed_sessions();
        setup_inner(&mut env);

        // Find the first group row in flat_items; record initial collapsed.
        let (group_idx, group_path) = env
            .view
            .flat_items
            .iter()
            .enumerate()
            .find_map(|(i, item)| match item {
                crate::session::Item::Group { path, .. } => Some((i, path.clone())),
                _ => None,
            })
            .expect("mixed env should produce at least one group row");

        let click_row = env.view.list_inner_area.y + group_idx as u16;
        let was_collapsed = env
            .view
            .flat_items
            .iter()
            .find_map(|item| match item {
                crate::session::Item::Group {
                    path, collapsed, ..
                } if path == &group_path => Some(*collapsed),
                _ => None,
            })
            .unwrap();

        let action = env.view.handle_click(5, click_row);
        assert!(
            action.is_none(),
            "single click on a group should not activate"
        );

        let now_collapsed = env
            .view
            .flat_items
            .iter()
            .find_map(|item| match item {
                crate::session::Item::Group {
                    path, collapsed, ..
                } if path == &group_path => Some(*collapsed),
                _ => None,
            })
            .expect("group row should still be present after toggle");
        assert_ne!(was_collapsed, now_collapsed, "group collapsed state flips");
    }
}

mod divider_drag {
    //! Click-and-drag on the list/preview divider resizes `list_width`.
    //! Persistence is checked via `load_config()` (the same path the
    //! keyboard `<`/`>` tests exercise indirectly via save_list_width).

    use super::*;
    use crate::session::config::load_config;
    use ratatui::layout::Rect;

    /// Stage the geometry a real side-by-side render would produce: a
    /// list at column 0, divider at column 35, terminal 100 wide. The
    /// list area mirrors what `render_list` would assign.
    fn stage_side_by_side(env: &mut TestEnv) {
        env.view.list_area = Rect::new(0, 0, 35, 20);
        env.view.divider_col = Some(35);
        env.view.main_area_width = 100;
        env.view.list_width = 35;
    }

    #[test]
    #[serial]
    fn hit_divider_matches_only_the_divider_column() {
        let mut env = create_test_env_empty();
        stage_side_by_side(&mut env);
        assert!(env.view.hit_divider(35, 5));
        assert!(!env.view.hit_divider(34, 5), "list inner shouldn't hit");
        assert!(!env.view.hit_divider(36, 5), "preview shouldn't hit");
        assert!(!env.view.hit_divider(35, 99), "row past list_area is out");
    }

    #[test]
    #[serial]
    fn hit_divider_is_false_in_stacked_mode() {
        let mut env = create_test_env_empty();
        stage_side_by_side(&mut env);
        // Stacked layout clears divider_col at render time; emulate.
        env.view.divider_col = None;
        assert!(!env.view.hit_divider(35, 5));
    }

    #[test]
    #[serial]
    fn drag_updates_list_width_relative_to_start() {
        let mut env = create_test_env_empty();
        stage_side_by_side(&mut env);
        assert!(
            env.view.handle_drag_start(35, 5),
            "divider click starts drag"
        );
        // Drag 10 cols right.
        assert!(env.view.handle_drag_move(45, 5));
        assert_eq!(env.view.list_width, 45);
        // Drag back 5 cols (from start).
        assert!(env.view.handle_drag_move(40, 5));
        assert_eq!(env.view.list_width, 40);
    }

    #[test]
    #[serial]
    fn drag_clamps_at_preview_min_width_ceiling() {
        let mut env = create_test_env_empty();
        stage_side_by_side(&mut env);
        // main_area_width=100, PREVIEW_MIN_WIDTH=40 -> ceiling=60.
        env.view.handle_drag_start(35, 5);
        env.view.handle_drag_move(200, 5);
        assert_eq!(env.view.list_width, 60);
    }

    #[test]
    #[serial]
    fn drag_clamps_at_floor_without_underflow() {
        let mut env = create_test_env_empty();
        stage_side_by_side(&mut env);
        env.view.handle_drag_start(35, 5);
        // Drag far to the left of column 0; the i32 math must absorb
        // the negative without wrapping u16.
        env.view.handle_drag_move(0, 5);
        assert_eq!(env.view.list_width, 10);
    }

    #[test]
    #[serial]
    fn dialog_opening_mid_drag_ends_drag_and_persists() {
        // If a modal opens while the user is still holding the mouse
        // (e.g. a hotkey was pressed mid-drag), further Drag events must
        // not keep updating list_width invisibly under the modal. The
        // width achieved up to that point is persisted so the user's
        // work isn't silently lost on Up.
        let mut env = create_test_env_empty();
        stage_side_by_side(&mut env);
        env.view.handle_drag_start(35, 5);
        env.view.handle_drag_move(50, 5);
        // Open a modal.
        env.view.info_dialog = Some(InfoDialog::new("title", "body"));
        // Next drag event sees the dialog and bails.
        let changed = env.view.handle_drag_move(60, 5);
        assert!(!changed);
        assert!(env.view.drag_state.is_none());
        assert_eq!(
            env.view.list_width, 50,
            "width frozen at last pre-dialog value"
        );
        let config = load_config().unwrap().expect("config saved");
        assert_eq!(config.app_state.home_list_width, Some(50));
        // Subsequent Up is now a no-op (drag_state was cleared early).
        assert!(!env.view.handle_drag_end());
    }

    #[test]
    #[serial]
    fn drag_end_persists_list_width_once() {
        let mut env = create_test_env_empty();
        stage_side_by_side(&mut env);
        env.view.handle_drag_start(35, 5);
        env.view.handle_drag_move(50, 5);
        assert!(env.view.handle_drag_end());
        let config = load_config().unwrap().expect("config saved");
        assert_eq!(config.app_state.home_list_width, Some(50));
        // Subsequent Up with no active drag is a no-op.
        assert!(!env.view.handle_drag_end());
    }

    #[test]
    #[serial]
    fn drag_move_without_drag_start_is_noop() {
        let mut env = create_test_env_empty();
        stage_side_by_side(&mut env);
        assert!(!env.view.handle_drag_move(50, 5));
        assert_eq!(env.view.list_width, 35);
    }

    #[test]
    #[serial]
    fn drag_start_misses_off_divider_column() {
        let mut env = create_test_env_empty();
        stage_side_by_side(&mut env);
        assert!(!env.view.handle_drag_start(34, 5));
        assert!(env.view.drag_state.is_none());
    }
}

mod preview_drag_select {
    //! Click-and-drag on the preview pane starts an in-app text
    //! selection whenever the pane is on screen (in or out of live
    //! mode). The selection is anchored to a distance from the newest
    //! line, so it survives a scroll (even as the captured window grows)
    //! and can span more than one page; the renderer re-derives the
    //! highlight rects each frame and release copies the full range
    //! through OSC 52. We need our own selection handler because the TUI
    //! captures mouse events to support wheel scroll, which keeps
    //! terminal-native drag-select from reaching the preview.

    use super::*;
    use crate::tui::home::{live_send::LiveSendState, DragKind, PreviewSelection, PreviewTextView};
    use ratatui::layout::Rect;
    use ratatui::text::{Line, Text};

    /// Absolute parsed-line index to the `from_bottom` distance the
    /// selection stores, for a pane of `total` lines. Tests express
    /// positions in absolute lines for readability and convert at the
    /// boundary.
    fn fb(total: usize, abs: usize) -> usize {
        total - 1 - abs
    }

    /// Read a stored `(col, from_bottom)` selection cell back as
    /// `(col, abs_line)` for a pane of `total` lines.
    fn to_abs(total: usize, cell: (u16, usize)) -> (u16, usize) {
        (cell.0, total - 1 - cell.1)
    }

    /// Stage the output-pane text-view snapshot the render path would set,
    /// plus the backing parsed cache, so the drag handlers can map screen
    /// cells to content lines and the copy can read them back. The scroll
    /// offset is derived so it agrees with `first_line` (the auto-scroll
    /// path projects `first_line` from the live offset, so the two must
    /// line up for the staged state to be self-consistent).
    fn stage_text(env: &mut TestEnv, pane: Rect, first_line: usize, lines: &[&str]) {
        let text: Text<'static> = lines.iter().map(|l| Line::from(l.to_string())).collect();
        let total_lines = text.lines.len();
        env.view.preview_cache.parsed_text = Some(text);
        env.view.preview_cache.captured_lines = total_lines;
        // `scroll_preview_offset` clamps the auto-scroll max offset against
        // `preview_visible_rows` (the rendered output-body height, same as the
        // per-frame `clamp_scroll_to_capture`); pin it to the pane height so the
        // max offset matches what `first_line` implies.
        env.view.preview_visible_rows = pane.height as usize;
        env.view.preview_cache.dimensions = (pane.width, pane.height + 1);
        env.view.preview_area = pane;
        env.view.preview_scroll_offset = total_lines
            .saturating_sub(pane.height as usize)
            .saturating_sub(first_line) as u16;
        env.view.preview_text_view = PreviewTextView {
            pane,
            first_line,
            total_lines,
        };
    }

    /// Stage a pane with `total_lines` of filler so coord-only tests don't
    /// have to spell out line contents.
    fn stage_pane(env: &mut TestEnv, pane: Rect, first_line: usize, total_lines: usize) {
        let filler: Vec<String> = (0..total_lines).map(|i| format!("line{i:04}")).collect();
        let refs: Vec<&str> = filler.iter().map(|s| s.as_str()).collect();
        stage_text(env, pane, first_line, &refs);
    }

    fn stage_live_send(env: &mut TestEnv) {
        // Live-send state cares only about session_id + tmux_name for
        // the parts of the home view these tests exercise (drag start
        // gate, key dismissal). The exit-chord list is unused here.
        env.view.live_send = Some(LiveSendState {
            session_id: "test-session".to_string(),
            title: "test".to_string(),
            tmux_name: "aoe_test_drag_select".to_string(),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: Vec::new(),
            leader: None,
        });
    }

    #[test]
    #[serial]
    fn drag_start_outside_live_mode_installs_selection() {
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
        // No live_send: a press on the preview pane still seeds a
        // PreviewSelect so users can copy from a regular session
        // preview without first entering live mode.
        assert!(env.view.handle_drag_start(50, 10));
        assert!(matches!(env.view.drag_state, Some(DragKind::PreviewSelect)));
        let sel = env.view.preview_selection.expect("selection installed");
        // col offset 50-40=10, content line first_line(0)+10=10.
        assert_eq!(to_abs(100, sel.anchor), (10, 10));
        assert_eq!(to_abs(100, sel.extent), (10, 10));
        assert!(!sel.finalized);
    }

    #[test]
    #[serial]
    fn drag_start_maps_screen_row_to_scrollback_line() {
        // With the pane scrolled into history, a press maps to the
        // absolute content line under the cursor, not the screen row.
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 100, 200);
        assert!(env.view.handle_drag_start(50, 10));
        let sel = env.view.preview_selection.expect("selection installed");
        // line first_line(100) + (row 10 - pane.y 0) = 110.
        assert_eq!(to_abs(200, sel.anchor), (10, 110));
    }

    #[test]
    #[serial]
    fn drag_start_blocked_by_non_live_overlay() {
        // A modal sitting over the preview must swallow the press
        // instead of seeding a hidden highlight behind the dialog.
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
        env.view.show_help = true;
        assert!(!env.view.handle_drag_start(50, 10));
        assert!(env.view.preview_selection.is_none());
        assert!(env.view.drag_state.is_none());
    }

    #[test]
    #[serial]
    fn drag_start_on_empty_pane_is_noop() {
        // A pane with no captured scrollback (total_lines == 0) has
        // nothing to select, so a press there must not seed a phantom
        // selection.
        let mut env = create_test_env_empty();
        env.view.preview_text_view = PreviewTextView {
            pane: Rect::new(40, 0, 60, 20),
            first_line: 0,
            total_lines: 0,
        };
        assert!(!env.view.handle_drag_start(50, 10));
        assert!(env.view.preview_selection.is_none());
        assert!(env.view.drag_state.is_none());
    }

    #[test]
    #[serial]
    fn drag_start_inside_live_mode_installs_selection() {
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
        stage_live_send(&mut env);
        assert!(env.view.handle_drag_start(50, 10));
        assert!(matches!(env.view.drag_state, Some(DragKind::PreviewSelect)));
        let sel = env.view.preview_selection.expect("selection installed");
        assert_eq!(to_abs(100, sel.anchor), (10, 10));
        assert_eq!(to_abs(100, sel.extent), (10, 10));
        assert!(!sel.finalized);
    }

    #[test]
    #[serial]
    fn drag_move_maps_to_content_and_clamps_to_pane() {
        let mut env = create_test_env_empty();
        // total_lines == pane height so there is no scroll room; a drag
        // far past the bottom-right clamps to the last visible cell.
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 20);
        env.view.handle_drag_start(50, 10);
        assert!(env.view.handle_drag_move(500, 500));
        let sel = env.view.preview_selection.expect("selection still live");
        // col offset clamps to width-1 = 59, content line to the last
        // visible line first_line(0)+height-1 = 19.
        assert_eq!(to_abs(20, sel.extent), (59, 19));
    }

    #[test]
    #[serial]
    fn bare_click_collapses_to_no_selection() {
        // Down + Up with no movement should not paint a 1x1 highlight
        // or copy a single character to the clipboard.
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
        stage_live_send(&mut env);
        env.view.handle_drag_start(50, 10);
        assert!(env.view.handle_drag_end());
        assert!(env.view.preview_selection.is_none());
        assert!(!env.view.preview_copy_pending);
    }

    #[test]
    #[serial]
    fn drag_end_finalizes_multi_cell_selection_and_arms_copy() {
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
        stage_live_send(&mut env);
        env.view.handle_drag_start(50, 10);
        env.view.handle_drag_move(55, 10);
        assert!(env.view.handle_drag_end());
        let sel = env.view.preview_selection.expect("finalized stays");
        assert!(sel.finalized);
        // The render that paints the finalized highlight is what
        // captures the text; handle_drag_end just arms the pending flag.
        assert!(env.view.preview_copy_pending);
        assert!(env.view.preview_copy_text.is_none());
    }

    #[test]
    #[serial]
    fn keypress_in_live_mode_dismisses_finalized_selection() {
        // After release, any keystroke clears the highlight so it
        // doesn't follow agent output as the live pane refreshes.
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
        stage_live_send(&mut env);
        env.view.handle_drag_start(50, 10);
        env.view.handle_drag_move(55, 10);
        env.view.handle_drag_end();
        assert!(env.view.preview_selection.is_some());
        // Send a stray key through the live-send path. The session
        // doesn't exist in tmux but the dismissal happens before the
        // translate step.
        env.view.handle_key(key(KeyCode::Char('x')), None);
        assert!(env.view.preview_selection.is_none());
    }

    #[test]
    #[serial]
    fn scroll_preserves_selection() {
        // The selection is anchored to scrollback lines, so a scroll no
        // longer drops it: the highlight tracks its text as the pane
        // moves. This is what lets the user scroll to verify a copy.
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
        stage_live_send(&mut env);
        env.view.handle_drag_start(50, 10);
        env.view.handle_drag_move(55, 10);
        env.view.handle_drag_end();
        assert!(env.view.preview_selection.is_some());
        env.view.handle_scroll_up(50, 10);
        assert!(
            env.view.preview_selection.is_some(),
            "scroll must not clear a scrollback-anchored selection"
        );
    }

    #[test]
    #[serial]
    fn drag_move_to_bottom_edge_extends_without_scrolling() {
        // The drag-move itself only records the edge position and extends
        // to the last visible line; the scroll is the ticker's job (so a
        // held-still cursor still advances). This keeps mouse movement
        // from lurching the scroll one line per event.
        let mut env = create_test_env_empty();
        // 50 lines, 5 visible, started scrolled 10 back (showing 35..40).
        stage_pane(&mut env, Rect::new(0, 0, 10, 5), 35, 50);
        stage_live_send(&mut env);
        assert_eq!(env.view.preview_scroll_offset, 10);
        env.view.handle_drag_start(0, 0); // anchor line 35
                                          // Drag held at the bottom edge (row 4 == pane.bottom()-1).
        assert!(env.view.handle_drag_move(0, 4));
        // No scroll yet, extent pinned to the last visible line (39).
        assert_eq!(env.view.preview_scroll_offset, 10);
        assert_eq!(
            to_abs(
                50,
                env.view.preview_selection.expect("live selection").extent
            ),
            (0, 39)
        );
    }

    #[test]
    #[serial]
    fn autoscroll_tick_at_bottom_edge_scrolls_and_extends_without_new_events() {
        // The core fix: with the cursor held at the bottom edge, plain
        // ticker ticks (no further mouse events) keep scrolling toward
        // newer output and growing the selection past one visible page.
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(0, 0, 10, 5), 35, 50);
        stage_live_send(&mut env);
        env.view.handle_drag_start(0, 0); // anchor line 35
        env.view.handle_drag_move(0, 4); // record edge position
        assert_eq!(env.view.preview_scroll_offset, 10);

        // First tick: scroll one line, extend to the new bottom (40).
        assert!(env.view.tick_preview_autoscroll());
        assert_eq!(env.view.preview_scroll_offset, 9);
        assert_eq!(
            to_abs(
                50,
                env.view.preview_selection.expect("live selection").extent
            ),
            (0, 40)
        );
        // Second tick, still no mouse event: advance again. (Clear the
        // pacing gate so the back-to-back call isn't throttled; the gate's
        // wall-clock interval is exercised in real use, not here.)
        env.view.preview_autoscroll_at = None;
        assert!(env.view.tick_preview_autoscroll());
        assert_eq!(env.view.preview_scroll_offset, 8);
        assert_eq!(
            to_abs(
                50,
                env.view.preview_selection.expect("live selection").extent
            ),
            (0, 41)
        );
    }

    #[test]
    #[serial]
    fn autoscroll_tick_at_top_edge_scrolls_into_history() {
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(0, 0, 10, 5), 35, 50);
        stage_live_send(&mut env);
        env.view.handle_drag_start(0, 4); // anchor line 39
        env.view.handle_drag_move(0, 0); // record top-edge position
        assert_eq!(env.view.preview_scroll_offset, 10);
        assert!(env.view.tick_preview_autoscroll());
        assert_eq!(env.view.preview_scroll_offset, 11);
        // New top line 34.
        assert_eq!(
            to_abs(
                50,
                env.view.preview_selection.expect("live selection").extent
            ),
            (0, 34)
        );
    }

    #[test]
    #[serial]
    fn autoscroll_tick_is_noop_off_edge_and_without_drag() {
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(0, 0, 10, 5), 35, 50);
        stage_live_send(&mut env);
        // No drag in progress yet.
        assert!(!env.view.tick_preview_autoscroll());
        // Drag held in the middle of the pane: nothing to auto-scroll.
        env.view.handle_drag_start(0, 2);
        env.view.handle_drag_move(0, 2);
        assert!(!env.view.tick_preview_autoscroll());
        assert_eq!(env.view.preview_scroll_offset, 10);
        // After release the tick must not resume scrolling.
        env.view.preview_drag_pos = Some((0, 4)); // pretend edge
        env.view.handle_drag_end();
        assert!(!env.view.tick_preview_autoscroll());
    }

    #[test]
    #[serial]
    fn extract_stays_locked_to_lines_when_capture_window_grows() {
        // Regression for the scroll-up-copies-wrong bug: live mode
        // re-captures a LARGER window as the user scrolls back, which
        // shifts every absolute line index. Because the selection is
        // anchored to the newest line, growing the window must NOT change
        // which physical lines the copy resolves to.
        let mut env = create_test_env_empty();
        // Small window: 6 lines, bottom two are E and F.
        stage_text(
            &mut env,
            Rect::new(0, 0, 5, 3),
            3,
            &["AAAAA", "BBBBB", "CCCCC", "DDDDD", "EEEEE", "FFFFF"],
        );
        // Select the bottom two lines (abs 4 and 5 in the small window).
        env.view.preview_selection = Some(PreviewSelection {
            anchor: (0, fb(6, 4)),
            extent: (4, fb(6, 5)),
            finalized: true,
        });
        assert_eq!(
            env.view.extract_preview_selection_text().as_deref(),
            Some("EEEEE\nFFFFF")
        );

        // The window grows by four older lines prepended at the top (a
        // scroll-back re-capture); the same physical bottom lines are now
        // at abs 8 and 9. The stored `from_bottom` distances are untouched.
        stage_text(
            &mut env,
            Rect::new(0, 0, 5, 3),
            7,
            &[
                "qqqqq", "rrrrr", "sssss", "ttttt", "AAAAA", "BBBBB", "CCCCC", "DDDDD", "EEEEE",
                "FFFFF",
            ],
        );
        // Same physical lines, even though their absolute indices moved.
        assert_eq!(
            env.view.extract_preview_selection_text().as_deref(),
            Some("EEEEE\nFFFFF")
        );
    }

    #[test]
    #[serial]
    fn extract_spans_full_scrollback_across_pages() {
        // The core multi-page guarantee: a selection whose start has
        // scrolled off the top of the visible window still copies the
        // off-screen lines, because extraction reads the parsed cache,
        // not the visible frame buffer.
        let mut env = create_test_env_empty();
        // Pane shows 3 rows; first visible line is 3, so lines 1 and 2
        // are above the fold.
        stage_text(
            &mut env,
            Rect::new(0, 0, 10, 3),
            3,
            &[
                "line0aaa", "line1bbb", "line2ccc", "line3ddd", "line4eee", "line5fff",
            ],
        );
        // Absolute lines 1..4 (col 0 of line 1 through col 6 of line 4).
        env.view.preview_selection = Some(PreviewSelection {
            anchor: (0, fb(6, 1)),
            extent: (6, fb(6, 4)),
            finalized: true,
        });
        let text = env
            .view
            .extract_preview_selection_text()
            .expect("non-empty text");
        assert_eq!(text, "line1bbb\nline2ccc\nline3ddd\nline4ee");
    }

    #[test]
    #[serial]
    fn extract_trims_trailing_whitespace_per_row() {
        let mut env = create_test_env_empty();
        stage_text(
            &mut env,
            Rect::new(0, 0, 10, 3),
            0,
            &["hello     ", "world     ", "          "],
        );
        env.view.preview_selection = Some(PreviewSelection {
            anchor: (0, fb(3, 0)),
            extent: (9, fb(3, 1)),
            finalized: true,
        });
        let text = env
            .view
            .extract_preview_selection_text()
            .expect("non-empty text");
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    #[serial]
    fn extract_returns_none_for_whitespace_only_selection() {
        let mut env = create_test_env_empty();
        stage_text(&mut env, Rect::new(0, 0, 5, 2), 0, &["     ", "     "]);
        env.view.preview_selection = Some(PreviewSelection {
            anchor: (0, fb(2, 0)),
            extent: (4, fb(2, 1)),
            finalized: true,
        });
        assert!(env.view.extract_preview_selection_text().is_none());
    }

    #[test]
    #[serial]
    fn take_preview_copy_text_drains_once() {
        // The app loop reads preview_copy_text after the post-drag
        // draw; the field must yield Some once and None thereafter so
        // a stable highlight doesn't write to the clipboard on every
        // subsequent frame.
        let mut env = create_test_env_empty();
        env.view.preview_copy_text = Some("clip me".to_string());
        assert_eq!(
            env.view.take_preview_copy_text().as_deref(),
            Some("clip me")
        );
        assert!(env.view.take_preview_copy_text().is_none());
    }

    #[test]
    #[serial]
    fn real_modal_during_preview_drag_cancels_selection() {
        // Live-send counts as a dialog under has_dialog() and is what
        // makes drag-select run in the first place; it must not cancel
        // the drag. But a real modal (info / confirm / etc.) popping up
        // mid-drag must drop the selection and stop mutating state
        // behind the overlay.
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
        stage_live_send(&mut env);
        assert!(env.view.handle_drag_start(50, 10));
        assert!(env.view.handle_drag_move(55, 10));
        assert!(env.view.preview_selection.is_some());

        // Open a real modal mid-drag (info dialog as a stand-in for
        // any of the non-live-send modals that has_dialog covers).
        env.view.info_dialog = Some(super::super::super::dialogs::InfoDialog::new(
            "title", "body",
        ));
        // Next drag-move should detect the modal and cancel.
        assert!(!env.view.handle_drag_move(60, 10));
        assert!(env.view.preview_selection.is_none());
        assert!(env.view.drag_state.is_none());
        assert!(!env.view.preview_copy_pending);
    }

    #[test]
    #[serial]
    fn clear_preview_selection_drops_pending_copy() {
        // Dismissing the highlight before the render fires (e.g. a
        // keystroke between Up(Left) and the next draw) must drop the
        // pending capture so it doesn't leak into the next drag.
        let mut env = create_test_env_empty();
        stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
        stage_live_send(&mut env);
        env.view.handle_drag_start(50, 10);
        env.view.handle_drag_move(55, 10);
        env.view.handle_drag_end();
        assert!(env.view.preview_copy_pending);
        env.view.clear_preview_selection();
        assert!(!env.view.preview_copy_pending);
        assert!(env.view.preview_copy_text.is_none());
    }

    #[test]
    #[serial]
    fn flow_extract_wraps_lines_with_partial_first_and_last_rows() {
        // Tmux-style flow: anchor partway into line 0, extent partway
        // into line 2. The middle line is pulled in full, the first from
        // the anchor col onward, and the last from the left up through
        // the extent col.
        let mut env = create_test_env_empty();
        stage_text(
            &mut env,
            Rect::new(0, 0, 10, 3),
            0,
            &["abcdefghij", "klmnopqrst", "uvwxyz0123"],
        );
        env.view.preview_selection = Some(PreviewSelection {
            anchor: (3, fb(3, 0)),
            extent: (5, fb(3, 2)),
            finalized: true,
        });
        let text = env
            .view
            .extract_preview_selection_text()
            .expect("non-empty text");
        assert_eq!(text, "defghij\nklmnopqrst\nuvwxyz");
    }

    #[test]
    #[serial]
    fn flow_extract_handles_reverse_drag() {
        // Drag from a later line up to an earlier one: anchor and extent
        // are swapped into reading order before the flow shape is built.
        let mut env = create_test_env_empty();
        stage_text(&mut env, Rect::new(0, 0, 5, 2), 0, &["abcde", "fghij"]);
        env.view.preview_selection = Some(PreviewSelection {
            anchor: (2, fb(2, 1)),
            extent: (1, fb(2, 0)),
            finalized: true,
        });
        let text = env
            .view
            .extract_preview_selection_text()
            .expect("non-empty text");
        assert_eq!(text, "bcde\nfgh");
    }

    /// Build a text-view snapshot for the screen-rect tests.
    fn view(pane: Rect, first_line: usize, total_lines: usize) -> PreviewTextView {
        PreviewTextView {
            pane,
            first_line,
            total_lines,
        }
    }

    #[test]
    #[serial]
    fn screen_flow_rects_single_row_returns_one_segment() {
        let sel = PreviewSelection {
            anchor: (10, fb(100, 5)),
            extent: (15, fb(100, 5)),
            finalized: false,
        };
        let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 20), 0, 100));
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], Rect::new(10, 5, 6, 1));
    }

    #[test]
    #[serial]
    fn screen_flow_rects_two_rows_returns_two_segments() {
        let sel = PreviewSelection {
            anchor: (10, fb(100, 5)),
            extent: (3, fb(100, 6)),
            finalized: false,
        };
        let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 20), 0, 100));
        assert_eq!(rects.len(), 2);
        // First line tail: cols 10..40 on row 5.
        assert_eq!(rects[0], Rect::new(10, 5, 30, 1));
        // Last line head: cols 0..=3 on row 6.
        assert_eq!(rects[1], Rect::new(0, 6, 4, 1));
    }

    #[test]
    #[serial]
    fn screen_flow_rects_three_rows_are_per_row_full_width_middles() {
        let sel = PreviewSelection {
            anchor: (10, fb(100, 5)),
            extent: (3, fb(100, 8)),
            finalized: false,
        };
        let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 20), 0, 100));
        // Per-row segments: tail, two full-width middles, head.
        assert_eq!(rects.len(), 4);
        assert_eq!(rects[0], Rect::new(10, 5, 30, 1));
        assert_eq!(rects[1], Rect::new(0, 6, 40, 1));
        assert_eq!(rects[2], Rect::new(0, 7, 40, 1));
        assert_eq!(rects[3], Rect::new(0, 8, 4, 1));
    }

    #[test]
    #[serial]
    fn screen_flow_rects_clips_rows_outside_visible_window() {
        // Selection runs from above the fold to below it; only the rows
        // inside the visible window paint, each full width since neither
        // the start nor the end line is in view.
        let sel = PreviewSelection {
            anchor: (2, fb(100, 8)),
            extent: (7, fb(100, 20)),
            finalized: false,
        };
        // Visible lines are 10..15 (first_line 10, height 5).
        let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 5), 10, 100));
        assert_eq!(rects.len(), 5);
        for (k, rect) in rects.iter().enumerate() {
            assert_eq!(*rect, Rect::new(0, k as u16, 40, 1));
        }
    }

    #[test]
    #[serial]
    fn screen_flow_rects_fully_offscreen_returns_empty() {
        let sel = PreviewSelection {
            anchor: (0, fb(100, 2)),
            extent: (5, fb(100, 4)),
            finalized: false,
        };
        // Selection lines 2..4 sit above the visible window 10..15.
        let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 5), 10, 100));
        assert!(rects.is_empty());
    }

    #[test]
    #[serial]
    fn full_render_pipeline_captures_copy_text_after_finalize() {
        // Drives the actual render path: render seeds the text-view
        // snapshot, the drag handlers map against it, and
        // paint_preview_selection captures the selected lines from the
        // parsed cache into `preview_copy_text` for the app loop to drain.
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut env = create_test_env_empty();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = load_theme("empire");

        // Stub the preview cache so render_preview has known content to
        // paint and to back the copy.
        env.view.preview_cache.content = "alpha beta gamma\nsecond line\nthird line\n".to_string();
        env.view.preview_cache.dimensions = (80, 24);
        env.view.preview_cache.captured_lines = 3;
        env.view.preview_cache.last_refresh = std::time::Instant::now();
        env.view.preview_cache.session_id = Some("fake-id".to_string());

        // First render seeds preview_text_view + paints content. We need
        // that before the drag handlers can map the cursor.
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();

        let pane = env.view.preview_text_view.pane;
        assert!(pane.width > 4, "preview pane was not set by render");
        assert!(
            env.view.preview_text_view.total_lines > 0,
            "render should have parsed scrollback into the text view"
        );

        // Install live-send so handle_drag_start claims the preview click.
        env.view.live_send = Some(LiveSendState {
            session_id: "fake-id".to_string(),
            title: "fake".to_string(),
            tmux_name: "aoe_test_full_pipeline".to_string(),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: Vec::new(),
            leader: None,
        });

        // Find a row in the output pane that actually carries text.
        let initial_buf = terminal.backend().buffer().clone();
        let mut content_row = None;
        for r in pane.y..pane.bottom() {
            let mut row_text = String::new();
            for c in pane.x..pane.right() {
                row_text.push_str(initial_buf[(c, r)].symbol());
            }
            if row_text.trim().chars().any(|ch| ch.is_alphabetic()) {
                content_row = Some(r);
                break;
            }
        }
        let row = content_row.expect("preview must paint some text");
        let start_col = pane.x;
        let end_col = pane.right() - 1;
        assert!(env.view.handle_drag_start(start_col, row));
        assert!(env.view.handle_drag_move(end_col, row));
        assert!(env.view.handle_drag_end());
        assert!(
            env.view.preview_copy_pending,
            "drag_end should arm a pending capture"
        );

        // The render that paints the finalized highlight is where the
        // capture happens: it reads the selected lines from the parsed
        // cache and stashes the string for the app loop to drain.
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();

        assert!(
            !env.view.preview_copy_pending,
            "render should consume the pending flag"
        );
        let copied = env
            .view
            .take_preview_copy_text()
            .expect("render should have captured selection text");
        assert!(
            copied.contains("alpha"),
            "captured text should include the first content row; got {copied:?}"
        );
    }
}

mod live_send_mode {
    //! Live-send wiring at the home view level. Translation correctness
    //! is covered by unit tests in src/tui/home/live_send.rs. Here we
    //! verify the integration points: keys are captured while live mode
    //! is active, Ctrl+q clears the state, the per-keystroke liveness
    //! check auto-exits on drift, and the predicate plumbing treats
    //! live mode like a modal capture so the rest of the TUI suspends
    //! underneath it.

    use super::super::live_send::LiveSendState;
    use super::*;

    /// Seed live-send state pointing at the first instance in the test
    /// env, with a matching tmux_name so the drift check passes. Tests
    /// that want to trigger drift either install pointing at a missing
    /// id or mutate the instance's title after installing.
    fn install_live_for_first_session(env: &mut TestEnv) -> String {
        let id = env
            .view
            .flat_items
            .iter()
            .find_map(|item| match item {
                crate::session::Item::Session { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("test env has no sessions; use install_live_orphan instead");
        let inst = env.view.get_instance(&id).unwrap().clone();
        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        // CI runs the e2e suite in the same `cargo test` invocation,
        // which populates the global tmux session cache. The drift
        // check then sees our fake test session name as "not in tmux"
        // (Some(false)) and clears live_send mid-test. Pre-inject the
        // name so the cache reports Some(true) for it; orphan tests
        // (install_live_orphan) deliberately skip this and let the
        // instance-missing branch fire instead.
        crate::tmux::test_inject_session_into_cache(&tmux_name);
        env.view.live_send = Some(LiveSendState {
            session_id: inst.id.clone(),
            title: inst.title,
            tmux_name,
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: crate::tui::home::live_send::parse_chord_list(
                crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
            ),
            leader: None,
        });
        id
    }

    /// Install live-send state pointing at a session id the env does
    /// NOT contain — used to verify the drift check fires (auto-exit
    /// + info dialog) when the underlying instance has vanished.
    fn install_live_orphan(env: &mut TestEnv) {
        env.view.live_send = Some(LiveSendState {
            session_id: "missing-id".to_string(),
            title: "missing-title".to_string(),
            tmux_name: "missing-tmux".to_string(),
            target: crate::tui::home::live_send::LiveSendTarget::Agent,
            exit_chords: crate::tui::home::live_send::parse_chord_list(
                crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
            ),
            leader: None,
        });
    }

    #[test]
    #[serial]
    fn ctrl_q_exits_live_mode() {
        let mut env = create_test_env_with_sessions(1);
        install_live_for_first_session(&mut env);
        assert!(env.view.live_send.is_some());

        env.view.handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            None,
        );

        assert!(env.view.live_send.is_none());
    }

    #[test]
    #[serial]
    fn ctrl_q_exits_even_when_session_has_drifted() {
        // Ctrl+q is the safety chord: it must always exit cleanly,
        // even if the underlying session went away (so the user can
        // recover from a stuck live mode without an extra dialog).
        let mut env = create_test_env_empty();
        install_live_orphan(&mut env);
        env.view.handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            None,
        );
        assert!(env.view.live_send.is_none());
        assert!(env.view.info_dialog.is_none());
    }

    #[test]
    #[serial]
    fn arbitrary_key_in_live_mode_does_not_emit_action() {
        // Live-send swallows the key (forwards it to tmux). The tmux
        // call will quietly fail because the test env doesn't have a
        // real tmux pane, but the home view must NOT bubble an
        // Action::* out (otherwise the action would race with the
        // live state). Use bare `x` so the test doesn't collide with
        // the Ctrl+q exit chord.
        let mut env = create_test_env_with_sessions(1);
        install_live_for_first_session(&mut env);
        let action = env
            .view
            .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), None);
        assert!(action.is_none());
        // Still in live mode; only Ctrl+q exits.
        assert!(env.view.live_send.is_some());
    }

    #[test]
    #[serial]
    fn drift_check_auto_exits_when_instance_missing() {
        // If the session is deleted while live mode is active, the
        // very next keystroke should auto-exit and surface an info
        // dialog explaining why (so the user isn't typing into the
        // void with no feedback).
        let mut env = create_test_env_empty();
        install_live_orphan(&mut env);
        env.view
            .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), None);
        assert!(env.view.live_send.is_none());
        assert!(env.view.info_dialog.is_some());
    }

    #[test]
    #[serial]
    fn shift_page_up_scrolls_preview_instead_of_sending_to_agent() {
        // Terminal-emulator convention: Shift+PageUp scrolls the outer
        // scrollback, not the inner program. Live mode honors that so
        // users can read agent history without exiting.
        let mut env = create_test_env_with_sessions(1);
        install_live_for_first_session(&mut env);
        env.view.preview_scroll_offset = 0;

        env.view
            .handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT), None);

        assert!(
            env.view.preview_scroll_offset > 0,
            "Shift+PageUp should scroll the preview back into history"
        );
        // Still in live mode — the intercept doesn't exit.
        assert!(env.view.live_send.is_some());
    }

    #[test]
    #[serial]
    fn shift_page_down_scrolls_preview_forward() {
        let mut env = create_test_env_with_sessions(1);
        install_live_for_first_session(&mut env);
        env.view.preview_scroll_offset = 50;

        env.view
            .handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT), None);

        assert!(
            env.view.preview_scroll_offset < 50,
            "Shift+PageDown should reduce the offset (scroll toward live)"
        );
        assert!(env.view.live_send.is_some());
    }

    #[test]
    #[serial]
    fn bare_page_up_still_passes_through_to_agent() {
        // Regression guard: only the Shift-modified Page chord is
        // intercepted. Bare PageUp must keep flowing to the agent so
        // agents that page their own UI (claude-code transcript, etc.)
        // keep responding.
        let mut env = create_test_env_with_sessions(1);
        install_live_for_first_session(&mut env);
        env.view.preview_scroll_offset = 25;

        env.view
            .handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), None);

        assert_eq!(
            env.view.preview_scroll_offset, 25,
            "bare PageUp must NOT change preview scroll offset"
        );
        assert!(env.view.live_send.is_some());
    }

    #[test]
    #[serial]
    fn drift_check_auto_exits_when_session_renamed() {
        // Title changes the generated tmux name. After a rename the
        // worker is targeting a stale name, so the next keystroke
        // should auto-exit. Simulate the rename by mutating the
        // instance title after installing live state.
        let mut env = create_test_env_with_sessions(1);
        let id = install_live_for_first_session(&mut env);
        env.view.mutate_instance(&id, |inst| {
            inst.title = "renamed-after-entry".to_string();
        });
        env.view
            .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), None);
        assert!(env.view.live_send.is_none());
        assert!(env.view.info_dialog.is_some());
    }

    #[test]
    #[serial]
    fn drift_check_does_not_exit_for_tool_target_named_via_tool_session() {
        // Regression guard: the Tool arm of the drift check must resolve
        // the current name the same way `prepare_live_send` computed
        // `tmux_name` at entry (via `ToolSession::new(..).session_name()`).
        // A prior bug instead re-derived the Tool arm's "current name"
        // through `Session::generate_name`, the agent-pane naming scheme,
        // which never matches a tool's own tmux name. That mismatch made
        // every Tool-view live-send look "renamed" on its very first
        // keystroke and auto-exit immediately.
        let mut env = create_test_env_with_sessions(1);
        let id = env
            .view
            .flat_items
            .iter()
            .find_map(|item| match item {
                crate::session::Item::Session { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("test env has one session");
        let inst = env.view.get_instance(&id).unwrap().clone();
        let tmux_name = crate::tmux::ToolSession::new(&inst.id, &inst.title, "lazygit")
            .session_name()
            .to_string();
        crate::tmux::test_inject_session_into_cache(&tmux_name);
        env.view.live_send = Some(LiveSendState {
            session_id: inst.id.clone(),
            title: inst.title,
            tmux_name,
            target: crate::tui::home::live_send::LiveSendTarget::Tool("lazygit".to_string()),
            exit_chords: crate::tui::home::live_send::parse_chord_list(
                crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
            ),
            leader: None,
        });

        env.view
            .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), None);

        assert!(
            env.view.live_send.is_some(),
            "first keystroke in a Tool-view live-send must not trip spurious drift"
        );
        assert!(env.view.info_dialog.is_none());
    }

    #[test]
    #[serial]
    fn live_mode_makes_has_dialog_true() {
        // Every dialog-gating predicate that already inspects has_dialog()
        // (mouse swallow, list nav suspend, palette skip) inherits live
        // mode for free via this single addition.
        let mut env = create_test_env_empty();
        assert!(!env.view.has_dialog());
        install_live_orphan(&mut env);
        assert!(env.view.has_dialog());
    }

    #[test]
    #[serial]
    fn live_mode_enables_paste_burst() {
        // wants_paste_burst is what tells the runtime to batch a stream
        // of Char events into a single Paste event when bracketed-paste
        // markers are missing (mosh, some SSH wrappers). Live mode wants
        // batching so a paste streams as one tmux call.
        let mut env = create_test_env_empty();
        install_live_orphan(&mut env);
        assert!(env.view.wants_paste_burst());
    }

    #[test]
    #[serial]
    fn tab_does_not_start_live_send_without_selection() {
        // No session selected (empty list, cursor on a group, etc.) →
        // Tab must silently no-op rather than emitting a deferred
        // action targeting nothing.
        let mut env = create_test_env_empty();
        let action = env
            .view
            .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), None);
        assert!(action.is_none());
        assert!(env.view.live_send.is_none());
    }

    #[test]
    #[serial]
    fn tab_emits_enter_live_send_for_stopped_session() {
        // start_live_send is intentionally permissive: it accepts any
        // non-Creating instance and defers ensure_pane_ready to
        // prepare_live_send. Without this, Tab would silently no-op on
        // stopped/dead-but-recoverable rows because the tmux session
        // doesn't exist yet.
        let mut env = create_test_env_with_sessions(1);
        env.view.cursor = 0;
        env.view.update_selected();
        // Pin the status explicitly so this regression guard doesn't
        // rely on the implicit Instance::new default surviving future
        // refactors. A real stopped session is what we're modeling.
        let id = env
            .view
            .flat_items
            .iter()
            .find_map(|item| match item {
                crate::session::Item::Session { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("test env has one session");
        env.view.mutate_instance(&id, |inst| {
            inst.status = crate::session::Status::Stopped;
        });
        let action = env
            .view
            .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), None);
        assert!(
            matches!(action, Some(Action::EnterLiveSend(_))),
            "Tab on a stopped session should emit Action::EnterLiveSend, got {:?}",
            action
        );
    }

    /// Acp-mode is a `serve` feature; the `structured_view` field on
    /// Instance only exists when that feature is compiled in. Without
    /// it, `is_structured()` is hard-coded to false and the gate is
    /// a no-op, so there's nothing meaningful to verify in the default
    /// build.
    #[cfg(feature = "serve")]
    #[test]
    #[serial]
    fn tab_does_not_start_live_send_for_acp_session() {
        // Acp sessions are not tmux-backed, so live-send has no
        // valid target. Tab must silently no-op rather than enqueue
        // an Action::EnterLiveSend that would fail downstream.
        let mut env = create_test_env_with_sessions(1);
        env.view.cursor = 0;
        env.view.update_selected();
        let id = env
            .view
            .flat_items
            .iter()
            .find_map(|item| match item {
                crate::session::Item::Session { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("test env has one session");
        env.view.mutate_instance(&id, |inst| {
            inst.status = crate::session::Status::Stopped;
            inst.view = crate::session::View::Structured;
        });
        let action = env
            .view
            .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), None);
        assert!(action.is_none(), "expected no action, got {:?}", action);
        assert!(env.view.live_send.is_none());
    }

    #[test]
    #[serial]
    fn has_non_live_send_overlay_false_in_pure_live_mode() {
        // Regression for the dead-fast-path bug: `has_dialog()` returns
        // true when live-send is active, which would gate off the
        // preview-only fast path (added in #1495) — the very thing it
        // was supposed to enable. `has_non_live_send_overlay()` is the
        // helper the fast-path gates use; in pure live mode with no
        // other dialog open, it must be false so the fast path can run.
        let mut env = create_test_env_with_sessions(1);
        install_live_for_first_session(&mut env);
        assert!(env.view.has_dialog(), "has_dialog includes live_send");
        assert!(
            !env.view.has_non_live_send_overlay(),
            "live-send alone must NOT count as an overlay for the fast-path gate"
        );
    }

    #[test]
    #[serial]
    fn has_non_live_send_overlay_true_when_dialog_also_open() {
        // The fast path still has to bail when any non-live overlay is
        // on top of the home view (settings, diff, info dialog, etc.),
        // because the snapshot it repaints doesn't include them.
        let mut env = create_test_env_with_sessions(1);
        install_live_for_first_session(&mut env);
        env.view.info_dialog = Some(InfoDialog::new("title", "body"));
        assert!(env.view.has_non_live_send_overlay());
    }

    #[test]
    #[serial]
    fn refresh_preserves_cache_when_live_capture_fails() {
        // Pin the kill-switch behavior (originally introduced in #1501,
        // re-implemented here against the fork-only capture path):
        // when live-send is active and the capture call fails (in this
        // unit fixture the backing tmux session doesn't exist, so the
        // fork returns Err), the previous capture's content must stay
        // in the cache. Pre-#1501 a single failed capture wiped
        // `preview_cache.content` to "" and the preview rendered
        // "No output available" until the user exited and re-entered
        // live mode.
        let mut env = create_test_env_with_sessions(1);
        let id = install_live_for_first_session(&mut env);
        env.view.selected_session = Some(id.clone());
        env.view.preview_cache.content = "hello from a successful capture".to_string();
        env.view.preview_cache.captured_lines = 1;
        env.view.preview_cache.dimensions = (80, 24);
        env.view.preview_cache.session_id = Some(id);

        env.view.refresh_preview_cache_if_needed(80, 24);

        assert_eq!(
            env.view.preview_cache.content, "hello from a successful capture",
            "cache must be preserved when the fork capture fails inside live mode"
        );
        assert_eq!(env.view.preview_cache.captured_lines, 1);
    }

    #[test]
    #[serial]
    fn refresh_terminal_cache_overwrites_on_empty_capture() {
        // Counterpart to `refresh_preserves_cache_when_live_capture_fails`:
        // the agent cache and the host-terminal cache now share
        // `refresh_preview_cache_core`, but only the agent wrapper carries the
        // live-send kill switch. The terminal path must keep its old semantics
        // (overwrite to empty so the preview surfaces "session looks gone")
        // even when the unit fixture's backing tmux session does not exist and
        // the capture comes back empty. Guards against the kill switch leaking
        // into the shared core for the non-agent wrappers.
        let mut env = create_test_env_with_sessions(1);
        let id = env
            .view
            .flat_items
            .iter()
            .find_map(|item| match item {
                crate::session::Item::Session { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("test env has one session");
        env.view.selected_session = Some(id.clone());
        env.view.terminal_preview_cache.content = "stale terminal output".to_string();
        env.view.terminal_preview_cache.captured_lines = 1;
        env.view.terminal_preview_cache.dimensions = (10, 10);
        env.view.terminal_preview_cache.session_id = Some(id.clone());

        env.view.refresh_terminal_preview_cache_if_needed(80, 24);

        assert_eq!(
            env.view.terminal_preview_cache.content, "",
            "terminal cache must overwrite stale content (no kill switch outside the agent path)"
        );
        assert_eq!(env.view.terminal_preview_cache.dimensions, (80, 24));
        assert_eq!(env.view.terminal_preview_cache.session_id, Some(id));
    }

    mod paste_splitting {
        //! `split_paste_for_live_send` decomposes a pasted string into
        //! tmux operations the live-send worker can actually deliver.
        //! Single-line pastes stay on the simple `Literal` + `Named("Tab")`
        //! path so raw shells and bracketed-paste-unaware agents keep
        //! working. Multi-line pastes get wrapped in xterm bracketed-
        //! paste markers (#1546) and dispatched as a single `HexBytes`
        //! payload so the receiving agent sees one paste instead of one
        //! `Enter` per line.

        use crate::tui::home::input::split_paste_for_live_send;
        use crate::tui::home::live_send::TmuxKey;

        fn lit(s: &str) -> TmuxKey {
            TmuxKey::Literal(s.to_string())
        }
        fn named(name: &str) -> TmuxKey {
            TmuxKey::Named(name.to_string())
        }

        /// xterm bracketed-paste start: `ESC [ 2 0 0 ~`.
        const BP_START: &[u8] = &[0x1b, b'[', b'2', b'0', b'0', b'~'];
        /// xterm bracketed-paste end: `ESC [ 2 0 1 ~`.
        const BP_END: &[u8] = &[0x1b, b'[', b'2', b'0', b'1', b'~'];

        /// Build the expected `HexBytes` payload for a multi-line
        /// paste: start marker, then the per-line `body` bytes, then
        /// end marker. Keeps each test focused on the *shape* of the
        /// paste content rather than on hand-rolled byte arithmetic.
        fn wrap(body: &[u8]) -> Vec<TmuxKey> {
            let mut out = Vec::with_capacity(BP_START.len() + body.len() + BP_END.len());
            out.extend_from_slice(BP_START);
            out.extend_from_slice(body);
            out.extend_from_slice(BP_END);
            vec![TmuxKey::HexBytes(out)]
        }

        #[test]
        fn printable_paste_stays_one_literal() {
            assert_eq!(
                split_paste_for_live_send("hello world"),
                vec![lit("hello world")],
            );
        }

        #[test]
        fn newline_wraps_in_bracketed_paste() {
            // Two-line paste must wrap in `\e[200~` / `\e[201~` markers,
            // with the interior newline riding as a raw CR. Without the
            // wrapping the agent treats the `\n` as Enter -> submit and
            // posts each line as its own user message (#1546).
            assert_eq!(
                split_paste_for_live_send("first\nsecond"),
                wrap(b"first\x0dsecond"),
            );
        }

        #[test]
        fn trailing_newline_stays_inside_bracketed_paste() {
            // A single line plus a trailing newline still wraps: the
            // user gets a paste with a trailing CR in the agent's input
            // buffer rather than a paste-then-submit. Lets the user
            // review before sending.
            assert_eq!(
                split_paste_for_live_send("only line\n"),
                wrap(b"only line\x0d"),
            );
        }

        #[test]
        fn leading_newline_stays_inside_bracketed_paste() {
            assert_eq!(split_paste_for_live_send("\nbody"), wrap(b"\x0dbody"));
        }

        #[test]
        fn crlf_coalesces_to_single_cr() {
            // Windows-style line endings collapse to one CR inside the
            // bracketed paste so the agent doesn't see a double newline.
            assert_eq!(split_paste_for_live_send("a\r\nb"), wrap(b"a\x0db"));
        }

        #[test]
        fn bare_cr_becomes_cr_inside_bracketed_paste() {
            assert_eq!(split_paste_for_live_send("a\rb"), wrap(b"a\x0db"));
        }

        #[test]
        fn tab_in_single_line_paste_emits_named_tab() {
            // Single-line tab pastes stay on the historical path.
            assert_eq!(
                split_paste_for_live_send("a\tb"),
                vec![lit("a"), named("Tab"), lit("b")],
            );
        }

        #[test]
        fn tab_in_multiline_paste_rides_as_raw_byte() {
            // Inside a bracketed paste, tab is a literal character of
            // the paste content, not a key event, so we send it as a
            // raw 0x09 byte alongside the rest of the payload.
            assert_eq!(split_paste_for_live_send("a\tb\nc"), wrap(b"a\x09b\x0dc"),);
        }

        #[test]
        fn other_control_bytes_are_dropped_in_single_line_path() {
            // BEL (0x07) and ESC (0x1b) have no safe paste mapping;
            // they're dropped to avoid surprising agent input cancels.
            assert_eq!(
                split_paste_for_live_send("a\x07b\x1bc"),
                vec![lit("a"), lit("b"), lit("c")],
            );
        }

        #[test]
        fn other_control_bytes_are_dropped_inside_bracketed_paste() {
            // Same drop policy applies inside the bracketed paste: an
            // embedded ESC could prematurely close the paste sequence
            // on the agent's side, so we strip it rather than forward.
            assert_eq!(
                split_paste_for_live_send("a\x07b\x1bc\nd"),
                wrap(b"abc\x0dd"),
            );
        }

        #[test]
        fn multiline_drag_select_paste_round_trip() {
            // Exact shape that comes back from drag-select copy: lines
            // joined with `\n` and no trailing newline. After the fix
            // for #1546 this wraps in bracketed-paste markers so the
            // agent sees one paste instead of three Enter keypresses.
            assert_eq!(
                split_paste_for_live_send("alpha beta\nsecond line\nthird"),
                wrap(b"alpha beta\x0dsecond line\x0dthird"),
            );
        }

        #[test]
        fn multiline_paste_dispatches_as_one_hex_payload() {
            // Single-fork dispatch: the entire paste (markers, content,
            // CRs) is one `HexBytes` so the worker fires exactly one
            // `tmux send-keys -H` subprocess. Verifies the length-of-1
            // invariant the worker relies on for paste latency.
            let out = split_paste_for_live_send("a\nb\nc\nd");
            assert_eq!(out.len(), 1, "multiline paste must be one TmuxKey");
            match &out[0] {
                TmuxKey::HexBytes(_) => {}
                other => panic!("expected HexBytes, got {other:?}"),
            }
        }

        #[test]
        fn multiline_paste_with_utf8_preserves_bytes() {
            // Non-ASCII chars (emoji, accented letters) ride as their
            // UTF-8 byte sequences so the agent receives the same text
            // the user copied. Regression guard for any future "ASCII
            // only" filter.
            assert_eq!(
                split_paste_for_live_send("café\n🚀"),
                wrap("café\x0d🚀".as_bytes()),
            );
        }

        #[test]
        fn empty_paste_is_empty() {
            // An empty paste should drop the entire bracketed-paste
            // wrapper too: pushing `\e[200~\e[201~` with no payload
            // would still flash through some agents' paste handlers.
            assert!(split_paste_for_live_send("").is_empty());
        }
    }
}

/// Tests for the post-create half of the `default_attach_mode` setting:
/// a freshly-created session enters tmux or live-send mode per the same
/// resolver as Enter/double-click. The unit under test is
/// `HomeView::default_attach_mode` as consumed by the post-create
/// dispatch, plus the invariant that the sync create path emits the
/// routed action variant (so it doesn't bypass the setting the way
/// `Action::AttachSession` would).
mod post_create_attach_mode {
    use super::*;
    use crate::session::config::{update_config, AttachMode};

    /// Add a session to the home view, return its id. The instance's
    /// `source_profile` is set to "test" so the resolver reads the
    /// test profile's config.
    fn add_session(view: &mut HomeView, title: &str) -> String {
        let mut inst = Instance::new(title, "/tmp/test");
        inst.source_profile = "test".to_string();
        let id = inst.id.clone();
        view.add_instance(inst);
        id
    }

    /// Write a global config.toml with the given attach mode so the
    /// resolver under test reads the user-configured value. Other
    /// fields stay at default.
    fn write_global_attach_mode(mode: AttachMode) {
        update_config(|config| {
            config.session.default_attach_mode = mode;
        })
        .unwrap();
    }

    #[test]
    #[serial]
    fn defaults_to_tmux_when_no_config_present() {
        // Fresh install: no config.toml exists, no profile override.
        // The setting must resolve to Tmux (historical behavior); a
        // None or LiveSend default would silently change every existing
        // user's UX on upgrade.
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "session-one");
        let mode = env.view.default_attach_mode(&id);
        assert_eq!(
            mode,
            Some(AttachMode::Tmux),
            "default must be Tmux to preserve existing UX"
        );
    }

    #[test]
    #[serial]
    fn returns_live_send_when_globally_configured() {
        // User saved `default_attach_mode = "live_send"` in their
        // global config. The resolver must pick it up so the dispatch
        // path in app.rs routes to live mode instead of tmux attach.
        let mut env = create_test_env_empty();
        write_global_attach_mode(AttachMode::LiveSend);
        let id = add_session(&mut env.view, "session-one");
        let mode = env.view.default_attach_mode(&id);
        assert_eq!(mode, Some(AttachMode::LiveSend));
    }

    #[test]
    #[serial]
    fn returns_none_for_missing_instance() {
        // Race: the apply_creation_results return reaches the dispatch
        // and the instance has been deleted in the meantime. `None`
        // signals the caller to fall back to the structured view-aware
        // attach_session path rather than try to attach to a ghost.
        let env = create_test_env_empty();
        let mode = env.view.default_attach_mode("nonexistent-id");
        assert!(mode.is_none());
    }

    #[cfg(feature = "serve")]
    #[test]
    #[serial]
    fn returns_none_for_acp_session() {
        // Acp sessions aren't tmux-backed; live mode has no target
        // and tmux attach is a no-op. The resolver returns None so the
        // dispatch picks the (no-op) fallback explicitly, regardless of
        // what the user configured globally.
        let mut env = create_test_env_empty();
        write_global_attach_mode(AttachMode::LiveSend);
        let id = add_session(&mut env.view, "acp-one");
        env.view.mutate_instance(&id, |inst| {
            inst.view = crate::session::View::Structured;
        });
        let mode = env.view.default_attach_mode(&id);
        assert!(mode.is_none(), "structured view sessions must return None");
    }

    /// Build a minimal `NewSessionData` for the sync create path: no
    /// sandbox, no hooks (caller passes `None`), no worktree. This is
    /// the combination that bypasses `creation_poller` and runs
    /// `create_session` inline, which is the path that originally
    /// emitted `Action::AttachSession` and bypassed the attach-mode
    /// setting.
    fn sync_path_session_data(project: &str) -> crate::tui::dialogs::NewSessionData {
        crate::tui::dialogs::NewSessionData {
            profile: "test".to_string(),
            title: "sync-path-test".to_string(),
            path: project.to_string(),
            group: String::new(),
            tool: "claude".to_string(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            extra_repo_paths: Vec::new(),
            sandbox: false,
            sandbox_image: String::new(),
            yolo_mode: false,
            extra_env: Vec::new(),
            extra_args: String::new(),
            command_override: String::new(),
            scratch: false,
            fork_seed: None,
            structured: false,
        }
    }

    #[test]
    #[serial]
    fn sync_create_path_emits_attach_after_create_not_attach_session() {
        // Regression guard for the original bug. `Action::AttachSession`
        // would skip the attach-mode dispatch; only
        // `Action::AttachAfterCreate` routes through it. If a future
        // refactor flips this back, the live-mode setting silently
        // stops working on no-sandbox/no-hooks/no-worktree creates and
        // the bug returns. e2e covers the live-mode end of the
        // dispatch; this unit test covers the action plumbing without
        // needing tmux.
        let mut env = create_test_env_empty();
        let project_dir = env._temp.path().join("sync-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let data = sync_path_session_data(project_dir.to_str().unwrap());
        let action = env.view.create_session_with_hooks(data, None);
        assert!(
            matches!(action, Some(Action::AttachAfterCreate(_))),
            "sync create path must emit AttachAfterCreate (route through attach-mode setting), got {:?}",
            action
        );
    }
}

/// Tests for the `default_attach_mode` setting that drives whether
/// pressing Enter (or double-clicking) on an existing session row in
/// Structured view attaches to tmux or enters live-send mode.
mod default_attach_mode {
    use super::*;
    use crate::session::config::{update_config, AttachMode};

    fn add_session(view: &mut HomeView, title: &str) -> String {
        let mut inst = Instance::new(title, "/tmp/test");
        inst.source_profile = "test".to_string();
        let id = inst.id.clone();
        view.add_instance(inst);
        id
    }

    fn write_global_default_attach_mode(mode: AttachMode) {
        update_config(|config| {
            config.session.default_attach_mode = mode;
        })
        .unwrap();
    }

    #[test]
    #[serial]
    fn defaults_to_tmux_when_no_config_present() {
        // Default Enter / double-click stays on AttachSession; flipping
        // it to LiveSend silently changes every existing user's muscle
        // memory on upgrade.
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "session-one");
        let mode = env.view.default_attach_mode(&id);
        assert_eq!(mode, Some(AttachMode::Tmux));
    }

    #[test]
    #[serial]
    fn enter_emits_attach_session_when_default_is_tmux() {
        // Sanity: with the historical Tmux default, Enter on a session
        // row produces Action::AttachSession.
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let action = env.view.activate_selected_session();
        assert_eq!(action, Some(Action::AttachSession(id)));
    }

    #[test]
    #[serial]
    fn enter_emits_enter_live_send_when_default_is_live_send() {
        // User opted into "Enter = live mode": activating an Agent-view
        // row must dispatch Action::EnterLiveSend instead of AttachSession.
        let mut env = create_test_env_empty();
        write_global_default_attach_mode(AttachMode::LiveSend);
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let action = env.view.activate_selected_session();
        assert_eq!(action, Some(Action::EnterLiveSend(id)));
    }

    #[test]
    #[serial]
    fn terminal_view_honors_default_attach_mode_live_send() {
        // The `default_attach_mode = LiveSend` setting applies to
        // Terminal view too: pressing Enter on a terminal-view row
        // dispatches `Action::EnterLiveSend` against the paired
        // terminal pane (the live-send target resolution happens in
        // `start_live_send` based on view_mode). Without this, the
        // user's "Enter = live mode" preference would silently flip
        // back to a full tmux attach whenever they were previewing a
        // terminal.
        let mut env = create_test_env_empty();
        write_global_default_attach_mode(AttachMode::LiveSend);
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.view_mode = crate::tui::home::ViewMode::Terminal;
        let action = env.view.activate_selected_session();
        assert_eq!(action, Some(Action::EnterLiveSend(id)));
    }

    #[test]
    #[serial]
    fn terminal_view_falls_back_to_attach_when_default_is_tmux() {
        // Inverse of the LiveSend case: with the historical Tmux
        // default, Enter on a terminal-view row keeps the historical
        // `Action::AttachTerminal` so users who haven't opted into
        // live mode see no change.
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.view_mode = crate::tui::home::ViewMode::Terminal;
        let action = env.view.activate_selected_session();
        assert!(
            matches!(&action, Some(Action::AttachTerminal(returned_id, _)) if returned_id == &id),
            "default Tmux mode must keep Terminal view on AttachTerminal, got {:?}",
            action
        );
    }

    #[test]
    #[serial]
    fn tab_swaps_to_attach_session_when_default_is_live_send() {
        // When `default_attach_mode = LiveSend`, Enter takes over the
        // live-send slot, so Tab swaps to a full tmux attach (the
        // escape hatch). Without this, the user would have no
        // single-key path to the underlying tmux session.
        let mut env = create_test_env_empty();
        write_global_default_attach_mode(AttachMode::LiveSend);
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let action = env.view.handle_key(key(KeyCode::Tab), None);
        assert_eq!(action, Some(Action::AttachSession(id)));
    }

    #[test]
    #[serial]
    fn tab_still_enters_live_send_when_default_is_tmux() {
        // With the historical Tmux default, Enter still attaches and
        // Tab keeps its historical live-send role.
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let action = env.view.handle_key(key(KeyCode::Tab), None);
        assert_eq!(action, Some(Action::EnterLiveSend(id)));
    }

    #[test]
    #[serial]
    fn tab_in_terminal_view_swaps_to_attach_terminal_when_default_is_live_send() {
        // Terminal-view counterpart of the swap: with Enter pinned to
        // live-send, Tab in Terminal view attaches the paired terminal
        // pane rather than the agent pane.
        let mut env = create_test_env_empty();
        write_global_default_attach_mode(AttachMode::LiveSend);
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.view_mode = crate::tui::home::ViewMode::Terminal;
        let action = env.view.handle_key(key(KeyCode::Tab), None);
        assert!(
            matches!(&action, Some(Action::AttachTerminal(returned_id, _)) if returned_id == &id),
            "Tab in Terminal view with LiveSend default must AttachTerminal, got {:?}",
            action
        );
    }

    #[test]
    #[serial]
    fn m_in_terminal_view_targets_terminal_pane() {
        // The 'm' bug from #1554: pressing 'm' from Terminal view used
        // to open a compose dialog that targeted the agent pane,
        // sending commands meant for the shell into the agent's input
        // box. The fix: `pending_send_target` reflects view_mode at
        // dialog open time so `execute_send_message` routes to the
        // paired terminal pane.
        let mut env = create_test_env_empty();
        let _id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.view_mode = crate::tui::home::ViewMode::Terminal;
        let _ = env.view.handle_key(key(KeyCode::Char('m')), None);
        assert!(
            env.view.send_message_dialog.is_some(),
            "Terminal view 'm' must open the compose dialog even when \
             the paired tmux pane hasn't spawned yet"
        );
        assert_eq!(
            env.view.pending_send_target,
            crate::tui::home::live_send::LiveSendTarget::Terminal,
            "compose dialog opened from Terminal view must target the terminal pane"
        );
    }

    #[test]
    #[serial]
    fn start_live_send_in_terminal_view_targets_terminal_pane() {
        // Direct check on the live-send target resolution: in Terminal
        // view, `start_live_send` stages the host terminal as the
        // pending target so `prepare_live_send` will dispatch
        // keystrokes to the paired terminal tmux pane.
        let mut env = create_test_env_empty();
        let _id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.view_mode = crate::tui::home::ViewMode::Terminal;
        let _ = env.view.start_live_send();
        assert_eq!(
            env.view.pending_live_send_target,
            crate::tui::home::live_send::LiveSendTarget::Terminal
        );
    }

    #[test]
    #[serial]
    fn refresh_tool_preview_cache_resizes_live_pane_when_targeted() {
        // Reviewer-requested fix (CodeRabbit + Seluj78 on #2777):
        // `refresh_tool_preview_cache_if_needed` must call
        // `resize_live_pane_if_target` up front, the same as the
        // Terminal/ContainerTerminal siblings, so a window resize while
        // live-sent to a Tool pane (lazygit, yazi) reflows it instead of
        // waiting for a live-mode re-enter. `resize_live_pane_if_target`
        // records the dedup in `live_send_last_resize` even without a
        // spawned worker, so that's the observable signal here.
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "session-one");
        let inst = env.view.get_instance(&id).unwrap().clone();
        let tmux_name = crate::tmux::ToolSession::new(&inst.id, &inst.title, "lazygit")
            .session_name()
            .to_string();
        env.view.live_send = Some(crate::tui::home::live_send::LiveSendState {
            session_id: id.clone(),
            title: inst.title.clone(),
            tmux_name,
            target: crate::tui::home::live_send::LiveSendTarget::Tool("lazygit".to_string()),
            exit_chords: Vec::new(),
            leader: None,
        });
        env.view.selected_session = Some(id);
        assert_eq!(env.view.live_send_last_resize, None);

        env.view
            .refresh_tool_preview_cache_if_needed(80, 24, "lazygit");

        assert_eq!(
            env.view.live_send_last_resize,
            Some((80, 24)),
            "resize_live_pane_if_target must fire for a targeted Tool pane"
        );
    }

    #[test]
    #[serial]
    fn start_live_send_in_tool_view_targets_tool_pane() {
        // Tool-view counterpart of `start_live_send_in_terminal_view_targets_terminal_pane`:
        // when previewing a named tool (lazygit, yazi, etc.), `start_live_send`
        // must resolve to that tool's own paired pane, not fall back to the
        // agent or bail out entirely.
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.view_mode = crate::tui::home::ViewMode::Tool("lazygit".to_string());
        let action = env.view.start_live_send();
        assert_eq!(action, Some(Action::EnterLiveSend(id)));
        assert_eq!(
            env.view.pending_live_send_target,
            crate::tui::home::live_send::LiveSendTarget::Tool("lazygit".to_string())
        );
    }

    fn write_live_send_on_view_switch(mode: AttachMode, on_view_switch: bool) {
        update_config(|config| {
            config.session.default_attach_mode = mode;
            config.session.live_send_on_view_switch = on_view_switch;
        })
        .unwrap();
    }

    #[test]
    #[serial]
    fn toggle_view_auto_starts_live_send_when_setting_enabled_and_default_is_live_send() {
        // With `live_send_on_view_switch` on and `default_attach_mode =
        // LiveSend`, pressing 't' (ToggleView) from Structured view must
        // not just flip the preview to Terminal; it must also enter
        // live-send immediately, without a separate Enter/Tab/click.
        let mut env = create_test_env_empty();
        write_live_send_on_view_switch(AttachMode::LiveSend, true);
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let action = env.view.handle_key(key(KeyCode::Char('t')), None);
        assert_eq!(
            env.view.view_mode,
            crate::tui::home::ViewMode::Terminal,
            "ToggleView must still flip the preview to Terminal"
        );
        assert_eq!(action, Some(Action::EnterLiveSend(id)));
    }

    #[test]
    #[serial]
    fn toggle_view_does_not_auto_start_live_send_when_setting_disabled() {
        // The setting defaults to off: even with `default_attach_mode =
        // LiveSend`, ToggleView must leave live-send alone and only
        // change the preview.
        let mut env = create_test_env_empty();
        write_live_send_on_view_switch(AttachMode::LiveSend, false);
        let _id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let action = env.view.handle_key(key(KeyCode::Char('t')), None);
        assert_eq!(env.view.view_mode, crate::tui::home::ViewMode::Terminal);
        assert_eq!(
            action, None,
            "auto live-send must stay off when the setting is disabled"
        );
        assert!(env.view.live_send.is_none());
    }

    #[test]
    #[serial]
    fn toggle_view_auto_starts_live_send_regardless_of_default_attach_mode() {
        // The setting is the only gate: with the historical Tmux
        // default, ToggleView still auto-enters live-send when
        // `live_send_on_view_switch` is enabled.
        let mut env = create_test_env_empty();
        write_live_send_on_view_switch(AttachMode::Tmux, true);
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let action = env.view.handle_key(key(KeyCode::Char('t')), None);
        assert_eq!(env.view.view_mode, crate::tui::home::ViewMode::Terminal);
        assert_eq!(
            action,
            Some(Action::EnterLiveSend(id)),
            "auto live-send must fire even when default_attach_mode is Tmux"
        );
    }

    #[test]
    #[serial]
    fn tool_hotkey_auto_starts_live_send_when_setting_enabled_and_default_is_live_send() {
        // Parallel case for the other explicit view-switch entry point:
        // opening a tool via its configured hotkey must apply the same
        // auto-entry check as ToggleView.
        let mut env = create_test_env_empty();
        write_live_send_on_view_switch(AttachMode::LiveSend, true);
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.tool_hotkey_cache =
            vec![("lazygit".to_string(), KeyCode::Char('g'), KeyModifiers::ALT)];
        let action = env
            .view
            .handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::ALT), None);
        assert_eq!(
            env.view.view_mode,
            crate::tui::home::ViewMode::Tool("lazygit".to_string())
        );
        assert_eq!(action, Some(Action::EnterLiveSend(id)));
    }

    #[test]
    #[serial]
    fn help_live_on_enter_returns_none_when_no_session_selected() {
        // Cursor parked off any session row: the help overlay shouldn't
        // claim a session-attach behavior, so `help_live_on_enter`
        // signals "no row" with None and the render path falls back to
        // the cached profile default.
        let env = create_test_env_empty();
        assert!(
            env.view.selected_session.is_none(),
            "fresh empty view should have no session selected"
        );
        assert_eq!(env.view.help_live_on_enter(), None);
    }

    #[test]
    #[serial]
    fn help_live_on_enter_returns_some_for_selected_session() {
        // With the historical Tmux default, a selected session row maps
        // to Some(false): Enter goes to tmux attach, Tab to live mode.
        let mut env = create_test_env_empty();
        let _id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        assert_eq!(env.view.help_live_on_enter(), Some(false));
    }

    #[test]
    #[serial]
    fn help_live_on_enter_reflects_live_send_setting() {
        // Flipping the user's default to LiveSend must propagate to
        // help_live_on_enter so the help overlay relabels Enter as
        // live mode and Tab as tmux attach.
        let mut env = create_test_env_empty();
        write_global_default_attach_mode(AttachMode::LiveSend);
        let _id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        assert_eq!(env.view.help_live_on_enter(), Some(true));
    }

    #[test]
    #[serial]
    fn profile_default_attach_mode_cache_refreshes_with_config() {
        // The render path falls back to `profile_default_attach_mode`
        // when no session is selected, so it has to track the saved
        // config without re-reading from disk per paint. Saving a new
        // mode + calling `refresh_from_config` must update the cache.
        let mut env = create_test_env_empty();
        assert_eq!(
            env.view.profile_default_attach_mode,
            AttachMode::Tmux,
            "cache should initialize to the historical Tmux default"
        );
        write_global_default_attach_mode(AttachMode::LiveSend);
        env.view
            .refresh_from_config(ConfigRefreshOrigin::Interactive);
        assert_eq!(
            env.view.profile_default_attach_mode,
            AttachMode::LiveSend,
            "refresh_from_config must pick up the saved LiveSend default"
        );
    }

    /// Acp sessions short-circuit before the setting is consulted
    /// (the structured view branch in `activate_selected_session` returns
    /// `OpenStructuredView`/transient-status before we get to the view-mode
    /// match), so the resolver also returns None for them; the setting
    /// must not be able to misroute a structured view row into live mode.
    #[cfg(feature = "serve")]
    #[test]
    #[serial]
    fn acp_session_ignores_default_attach_mode() {
        let mut env = create_test_env_empty();
        write_global_default_attach_mode(AttachMode::LiveSend);
        let id = add_session(&mut env.view, "acp-one");
        env.view.mutate_instance(&id, |inst| {
            inst.view = crate::session::View::Structured;
        });
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let action = env.view.activate_selected_session();
        assert!(
            matches!(&action, Some(Action::OpenStructuredView(returned_id)) if returned_id == &id),
            "structured view rows must route to OpenStructuredView regardless of default_attach_mode, got {:?}",
            action
        );
    }

    /// Render the whole home screen into a string for placeholder /
    /// badge assertions.
    fn render_home(env: &mut TestEnv) -> String {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = load_theme("empire");
        let backend = TestBackend::new(200, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn structured_session_env() -> (TestEnv, String) {
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "acp-one");
        env.view.mutate_instance(&id, |inst| {
            inst.view = crate::session::View::Structured;
        });
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        (env, id)
    }

    /// A selected structured session has no agent tmux pane; the preview
    /// must show the explanatory placeholder instead of a blank capture.
    #[test]
    #[serial]
    fn structured_session_preview_shows_placeholder() {
        let (mut env, _id) = structured_session_env();
        let screen = render_home(&mut env);
        assert!(
            screen.contains("Structured view"),
            "placeholder heading missing:\n{screen}"
        );
        assert!(
            screen.contains("structured transcript"),
            "placeholder body missing:\n{screen}"
        );
    }

    /// The switch-view context entry offers the opposite view: terminal for
    /// a structured row, structured for a terminal row whose tool is
    /// ACP-capable, and nothing for rows mid-lifecycle.
    #[cfg(feature = "serve")]
    #[test]
    #[serial]
    fn switch_view_target_gates_by_view_and_state() {
        let (mut env, id) = structured_session_env();
        assert_eq!(env.view.session_switch_view_target(&id), Some(true));
        // Terminal row with an ACP-capable tool (claude): offer structured.
        env.view.mutate_instance(&id, |inst| {
            inst.view = crate::session::View::Terminal;
        });
        assert_eq!(env.view.session_switch_view_target(&id), Some(false));
        // Mid-lifecycle rows are excluded.
        env.view.mutate_instance(&id, |inst| {
            inst.status = crate::session::Status::Creating;
        });
        assert_eq!(env.view.session_switch_view_target(&id), None);
    }

    /// Accepting the switch-view confirm emits the action with the stashed
    /// session id, mirroring the other confirm-carrying actions.
    #[cfg(feature = "serve")]
    #[test]
    #[serial]
    fn switch_view_confirm_dispatches_action_with_stashed_id() {
        let (mut env, id) = structured_session_env();
        env.view.prompt_switch_view_for_selected();
        assert!(
            env.view.confirm_dialog.is_some(),
            "switch must confirm first (history is destroyed)"
        );
        let action = env.view.dispatch_confirm_submit("switch_view");
        assert!(
            matches!(action, Some(Action::SwitchSessionView(ref sid)) if *sid == id),
            "expected SwitchSessionView({id}), got {action:?}"
        );
    }

    /// The `[structured]` badge marks structured rows in the Terminal home
    /// layout too (non-sandboxed rows have no container/host badge there),
    /// so Enter opening the structured view is never a surprise.
    #[test]
    #[serial]
    fn structured_badge_shows_in_terminal_view_mode() {
        let (mut env, _id) = structured_session_env();
        env.view.view_mode = crate::tui::home::ViewMode::Terminal;
        let screen = render_home(&mut env);
        assert!(
            screen.contains("[structured]"),
            "badge missing in Terminal view mode:\n{screen}"
        );
    }

    fn render_footer(env: &mut TestEnv) -> String {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = load_theme("empire");
        let backend = TestBackend::new(200, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Tab is Enter's complement on a session row: whichever of
    /// live-send / tmux-attach `default_attach_mode` doesn't route Enter
    /// to. The footer must surface that complement so it isn't only
    /// discoverable by reading the source or the `?` help overlay.
    #[test]
    #[serial]
    fn footer_advertises_tab_as_live_when_default_is_tmux() {
        let mut env = create_test_env_empty();
        let _id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let out = render_footer(&mut env);
        assert!(
            out.contains("↵  Attach"),
            "Enter hint should stay tmux attach under the default mode.\n{out}"
        );
        assert!(
            out.contains("⇥  Live"),
            "Tab hint should advertise Live mode when Enter is pinned to tmux attach.\n{out}"
        );
    }

    /// Inverse of the above: once `default_attach_mode = LiveSend` takes
    /// over Enter, the two hints swap rather than both claiming "Attach".
    /// Enter owns live-send and Tab becomes the tmux escape hatch, the
    /// same swap the `?` overlay does for this pairing.
    #[test]
    #[serial]
    fn footer_advertises_tab_as_attach_when_default_is_live_send() {
        let mut env = create_test_env_empty();
        write_global_default_attach_mode(AttachMode::LiveSend);
        let _id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let out = render_footer(&mut env);
        assert!(
            out.contains("↵  Live"),
            "Enter hint should say Live once it owns live-send.\n{out}"
        );
        assert!(
            out.contains("⇥  Attach"),
            "Tab hint should offer the tmux escape hatch once Enter owns live-send.\n{out}"
        );
    }

    /// Acp/structured rows ignore `default_attach_mode` entirely (Tab
    /// either mirrors Enter or no-ops), so the footer must not advertise
    /// a Tab complement that doesn't actually do anything different.
    #[test]
    #[serial]
    fn footer_hides_tab_hint_for_structured_sessions() {
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "acp-one");
        env.view.mutate_instance(&id, |inst| {
            inst.view = crate::session::View::Structured;
        });
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let out = render_footer(&mut env);
        assert!(
            !out.contains("⇥"),
            "structured view rows must not show a Tab hint at all.\n{out}"
        );
        assert!(
            out.contains("↵  Attach"),
            "structured rows keep the plain Enter attach label.\n{out}"
        );
    }
}

mod save_field_merge {
    use super::*;
    use chrono::Utc;

    fn boot_view_with_one_session(
        title: &str,
        path: &str,
    ) -> (TempDir, AppDirGuard, HomeView, String) {
        let temp = TempDir::new().unwrap();
        let guard = setup_test_home(&temp);
        let storage = Storage::new_unwatched("test").unwrap();
        let inst = Instance::new(title, path);
        let id = inst.id.clone();
        storage
            .update(|i, g| {
                i.push(inst.clone());
                *g = GroupTree::new_with_groups(&[inst], &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let tools = AvailableTools::with_tools(&["claude"]);
        let view = HomeView::new(
            Some("test".to_string()),
            tools,
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        (temp, guard, view, id)
    }

    #[test]
    #[serial]
    fn test_save_preserves_peer_field_update() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

        let peer_storage = Storage::new_unwatched("test").unwrap();
        let peer_archived_at = Utc::now();
        peer_storage
            .update(|insts, _| {
                if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                    inst.archived_at = Some(peer_archived_at);
                }
                Ok(())
            })
            .unwrap();

        view.save().expect("save must merge peer-owned field write");

        let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
        let row = reloaded.iter().find(|i| i.id == id).expect("row present");
        assert_eq!(
            row.archived_at,
            Some(peer_archived_at),
            "peer's archive must survive a TUI save with stale view"
        );
    }

    #[test]
    #[serial]
    fn test_save_preserves_peer_added_row() {
        let (_temp, _guard, mut view, _id) = boot_view_with_one_session("a", "/tmp/a");

        let peer_storage = Storage::new_unwatched("test").unwrap();
        peer_storage
            .update(|insts, _| {
                insts.push(Instance::new("peer-added", "/tmp/peer"));
                Ok(())
            })
            .unwrap();

        view.save()
            .expect("save must not delete rows the TUI does not know about");

        let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
        assert!(
            reloaded.iter().any(|i| i.title == "peer-added"),
            "peer-added row must survive TUI save"
        );
        assert!(
            reloaded.iter().any(|i| i.title == "a"),
            "TUI's known row must remain"
        );
    }

    #[test]
    #[serial]
    fn test_save_drops_explicitly_deleted_row() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/victim");

        view.remove_instance(&id);
        view.save().expect("save must propagate the delete");

        let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
        assert!(
            !reloaded.iter().any(|i| i.id == id),
            "tombstoned row must be removed from disk"
        );
    }

    #[test]
    #[serial]
    fn test_save_drains_pending_deletions_on_ok() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/victim");

        view.remove_instance(&id);
        assert!(
            view.pending_deletions
                .get("test")
                .is_some_and(|s| s.contains(&id)),
            "remove_instance must populate pending_deletions"
        );

        view.save().unwrap();

        assert!(
            !view.pending_deletions.contains_key("test"),
            "pending_deletions must drain on Ok save"
        );
    }

    #[test]
    #[serial]
    fn test_save_preserves_peer_added_group() {
        let (_temp, _guard, mut view, _id) = boot_view_with_one_session("a", "/tmp/a");

        let peer_storage = Storage::new_unwatched("test").unwrap();
        peer_storage
            .update(|_insts, groups| {
                groups.push(crate::session::Group::new("peer-grp", "peer-grp"));
                Ok(())
            })
            .unwrap();

        view.save()
            .expect("save must not clobber groups the TUI does not know about");

        let reloaded = Storage::new_unwatched("test")
            .unwrap()
            .load_with_groups()
            .unwrap()
            .1;
        assert!(
            reloaded.iter().any(|g| g.path == "peer-grp"),
            "peer-added group must survive TUI save"
        );
    }

    #[test]
    #[serial]
    fn test_apply_user_action_persists_atomically() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

        view.apply_user_action(&id, |inst| inst.archive())
            .expect("apply_user_action must persist");

        let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
        let row = reloaded.iter().find(|i| i.id == id).expect("row present");
        assert!(
            row.archived_at.is_some(),
            "apply_user_action must persist archived_at to disk"
        );
    }

    #[test]
    #[serial]
    fn test_apply_user_action_does_not_clobber_peer_field() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

        let peer_storage = Storage::new_unwatched("test").unwrap();
        peer_storage
            .update(|insts, _| {
                if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                    inst.notify_on_waiting = Some(true);
                }
                Ok(())
            })
            .unwrap();

        view.apply_user_action(&id, |inst| inst.archive())
            .expect("archive must persist");

        let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
        let row = reloaded.iter().find(|i| i.id == id).expect("row present");
        assert!(row.archived_at.is_some(), "TUI archive landed");
        assert_eq!(
            row.notify_on_waiting,
            Some(true),
            "peer's notify_on_waiting must survive an apply_user_action that does not touch it"
        );
    }

    #[test]
    #[serial]
    fn test_apply_user_action_disk_and_memory_share_one_timestamp() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

        view.apply_user_action(&id, |inst| inst.archive())
            .expect("apply_user_action must persist");

        let mem_ts = view
            .get_instance(&id)
            .expect("in-memory row present")
            .archived_at;
        let disk_ts = Storage::new_unwatched("test")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .expect("disk row present")
            .archived_at;
        assert_eq!(
            mem_ts, disk_ts,
            "single Utc::now() snapshot, no microsecond drift between memory and disk"
        );
    }

    #[test]
    #[serial]
    fn test_apply_user_action_archive_clears_peer_snooze() {
        // The web/TUI/CLI contract treats pinned / archived / snoozed
        // as mutually exclusive (see Instance::archive and the sidebar
        // tier comparator in #1581). When a peer snoozes a row that
        // the TUI then archives, archive wins because it is the
        // indefinite sink; leaving both flags persisted would surface
        // contradictory triage state on the next render.
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

        let peer_storage = Storage::new_unwatched("test").unwrap();
        peer_storage
            .update(|insts, _| {
                if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                    inst.snooze(30);
                }
                Ok(())
            })
            .unwrap();

        view.apply_user_action(&id, |inst| inst.archive())
            .expect("archive must persist");

        let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
        let row = reloaded.iter().find(|i| i.id == id).expect("row present");
        assert!(row.archived_at.is_some(), "TUI archive landed");
        assert!(
            row.snoozed_until.is_none(),
            "archive() invariant must clear a concurrent peer snooze",
        );
    }

    #[test]
    #[serial]
    fn test_apply_user_action_preserves_peer_user_action_field() {
        // Field-level merge regression: a TUI snooze must not clobber
        // an unrelated peer write (group_path here). Uses snooze
        // instead of archive so the snoozed_until field IS touched on
        // both sides and the test isolates the peer-field-survival
        // invariant from the archive XOR rules tested above.
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

        let peer_storage = Storage::new_unwatched("test").unwrap();
        peer_storage
            .update(|insts, _| {
                if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                    inst.group_path = "peer/group".to_string();
                }
                Ok(())
            })
            .unwrap();

        view.apply_user_action(&id, |inst| inst.snooze(30))
            .expect("snooze must persist");

        let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
        let row = reloaded.iter().find(|i| i.id == id).expect("row present");
        assert!(row.snoozed_until.is_some(), "TUI snooze landed");
        assert_eq!(
            row.group_path, "peer/group",
            "peer-written group_path must survive a TUI snooze that does not touch the field",
        );
    }

    #[test]
    #[serial]
    fn test_save_drops_peer_deleted_row_from_mirror() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/peer-rm");

        // Simulate `aoe session remove victim` from another process: peer
        // deletes the row from disk while TUI still has it in memory.
        Storage::new_unwatched("test")
            .unwrap()
            .update(|insts, _g| {
                insts.retain(|i| i.id != id);
                Ok(())
            })
            .unwrap();

        view.save()
            .expect("save must not error on peer-deleted rows");

        assert!(
            !view.instances().any(|i| i.id == id),
            "peer-deleted row must be dropped from in-memory instances"
        );
        assert!(
            view.get_instance(&id).is_none(),
            "peer-deleted row must be dropped from in-memory mirror"
        );
        let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
        assert!(
            !disk.iter().any(|i| i.id == id),
            "save() must not resurrect the peer-deleted row on disk"
        );
    }

    #[test]
    #[serial]
    fn test_save_pushes_tui_added_row_to_disk() {
        let (_temp, _guard, mut view, _) = boot_view_with_one_session("seed", "/tmp/seed");

        let mut new_inst = Instance::new("tui-added", "/tmp/added");
        new_inst.source_profile = "test".to_string();
        let new_id = new_inst.id.clone();
        view.add_instance(new_inst);

        view.save().expect("save must persist TUI-added row");

        let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
        assert!(
            disk.iter().any(|i| i.id == new_id),
            "TUI-added row must be persisted to disk"
        );
        assert!(
            !view.pending_added.contains_key("test"),
            "pending_added must drain on Ok save"
        );
    }

    #[test]
    #[serial]
    fn test_save_add_then_remove_in_same_cycle_does_not_persist() {
        let (_temp, _guard, mut view, _) = boot_view_with_one_session("seed", "/tmp/seed");

        let mut new_inst = Instance::new("ephemeral", "/tmp/ephemeral");
        new_inst.source_profile = "test".to_string();
        let new_id = new_inst.id.clone();
        view.add_instance(new_inst);
        view.remove_instance(&new_id);

        view.save().expect("save must succeed");

        let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
        assert!(
            !disk.iter().any(|i| i.id == new_id),
            "add+remove in same save cycle must not leak the row to disk"
        );
    }

    #[test]
    #[serial]
    fn test_move_to_profile_marks_tombstone_and_pending_added() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");
        view.storages.insert(
            "target".to_string(),
            Storage::new_unwatched("target").unwrap(),
        );

        view.move_to_profile(&id, "target", "moved/group".to_string())
            .unwrap();

        assert!(
            view.pending_deletions
                .get("test")
                .is_some_and(|s| s.contains(&id)),
            "old profile must have tombstone"
        );
        assert!(
            view.pending_added
                .get("target")
                .is_some_and(|s| s.contains(&id)),
            "new profile must have pending_added entry"
        );
        let inst = view.get_instance(&id).unwrap();
        assert_eq!(inst.source_profile, "target");
        assert_eq!(inst.group_path, "moved/group");
    }

    #[test]
    #[serial]
    fn test_move_to_profile_save_roundtrip_persists_under_target() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");
        view.storages.insert(
            "target".to_string(),
            Storage::new_unwatched("target").unwrap(),
        );

        view.move_to_profile(&id, "target", String::new()).unwrap();
        view.save().expect("save must succeed across profiles");

        let old_disk = Storage::new_unwatched("test").unwrap().load().unwrap();
        let new_disk = Storage::new_unwatched("target").unwrap().load().unwrap();
        assert!(
            !old_disk.iter().any(|i| i.id == id),
            "old profile disk must NOT contain the moved row"
        );
        assert!(
            new_disk.iter().any(|i| i.id == id),
            "new profile disk MUST contain the moved row"
        );
    }

    #[test]
    #[serial]
    fn test_move_to_profile_same_profile_only_updates_group_path() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");

        view.move_to_profile(&id, "test", "newgrp".to_string())
            .unwrap();

        assert!(
            !view.pending_deletions.contains_key("test")
                || !view.pending_deletions.get("test").unwrap().contains(&id),
            "same-profile move must NOT tombstone the row"
        );
        assert_eq!(view.get_instance(&id).unwrap().group_path, "newgrp");
    }

    #[test]
    #[serial]
    fn test_reload_honors_peer_cleared_session_id() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/sid");

        // Seed a stale sid via the in-memory mirror + persist.
        view.mutate_instance(&id, |inst| {
            inst.agent_session_id = Some("stale_X".to_string());
        });
        view.save().unwrap();

        // Peer clears the sid on disk (simulates `aoe session set-session-id ""`).
        Storage::new_unwatched("test")
            .unwrap()
            .update(|insts, _g| {
                if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                    inst.agent_session_id = None;
                }
                Ok(())
            })
            .unwrap();

        view.reload().unwrap();

        assert!(
            view.get_instance(&id)
                .and_then(|i| i.agent_session_id.clone())
                .is_none(),
            "reload must honor peer-cleared sid; carrying memory would re-pass --resume <stale>"
        );
    }

    /// `stamp_last_accessed` on a sunk row must auto-clear archived_at on
    /// BOTH memory and disk, and rebuild flat_items so the row leaves the
    /// synthetic Archived section on the same frame. Regression guard for
    /// the "re-entering an archived session left it stuck in the Archived
    /// section until the user pressed `z`" bug: the old implementation used
    /// mutate_instance + save, but merge_from_tui doesn't carry archived_at
    /// so the next reload resurrected the sink from disk.
    #[test]
    #[serial]
    fn stamp_last_accessed_on_archived_row_unsinks_persistently() {
        use crate::session::{is_archived_section_path, Item};

        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/grp");

        view.apply_user_action(&id, |inst| inst.archive())
            .expect("seed archive must persist");
        view.flat_items = view.build_flat_items();
        assert!(
            view.get_instance(&id).unwrap().is_archived(),
            "precondition: row archived in memory"
        );
        let archived_section_present = |items: &[Item]| {
            items.iter().any(|it| match it {
                Item::Group { path, .. } => is_archived_section_path(path),
                _ => false,
            })
        };
        assert!(
            archived_section_present(&view.flat_items),
            "precondition: Archived section header rendered"
        );

        view.stamp_last_accessed(&id);

        assert!(
            !view.get_instance(&id).unwrap().is_archived(),
            "stamp_last_accessed must clear archived_at in memory"
        );
        let disk_row = Storage::new_unwatched("test")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .expect("disk row present");
        assert!(
            disk_row.archived_at.is_none(),
            "stamp_last_accessed must persist the auto-unarchive (merge_from_tui drops archived_at)"
        );
        assert!(
            !archived_section_present(&view.flat_items),
            "Archived section must disappear once the only archived row is unsunk"
        );
    }

    /// Snoozed siblings of the archive case: `snoozed_until` is also cleared
    /// by `touch_last_accessed` and is also excluded from `merge_from_tui`,
    /// so the same persistence bug applied to snoozed rows. Same fix path
    /// (apply_user_action), same disk-versus-memory contract.
    #[test]
    #[serial]
    fn stamp_last_accessed_on_snoozed_row_persistently_clears_snooze() {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/grp");

        view.apply_user_action(&id, |inst| inst.snooze(30))
            .expect("seed snooze must persist");
        assert!(
            view.get_instance(&id).unwrap().is_snoozed(),
            "precondition: row snoozed in memory"
        );

        view.stamp_last_accessed(&id);

        assert!(
            !view.get_instance(&id).unwrap().is_snoozed(),
            "stamp_last_accessed must clear snoozed_until in memory"
        );
        let disk_row = Storage::new_unwatched("test")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .expect("disk row present");
        assert!(
            disk_row.snoozed_until.is_none(),
            "stamp_last_accessed must persist the auto-unsnooze (merge_from_tui drops snoozed_until)"
        );
    }
}

#[cfg(test)]
mod right_click_context_menu {
    //! Right-click on a sidebar row opens a small popup menu (Rename /
    //! Delete) anchored to the click. Picking Rename routes through the
    //! same helper as the `r` key, Delete through the same helper as
    //! `d`. Click-outside dismisses the menu.

    use super::*;
    use crate::session::config::SortOrder;
    use crate::session::Item;
    use crate::tui::dialogs::ContextMenuAction;
    use ratatui::layout::Rect;

    fn setup_inner(env: &mut TestEnv) {
        env.view.list_inner_area = Rect::new(1, 1, 28, 10);
        env.view.list_area = Rect::new(0, 0, 30, 12);
    }

    #[test]
    #[serial]
    fn right_click_on_session_opens_session_menu_and_moves_cursor() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        // Click the third visible row (inner.y + 2 == 3) -> flat_items[2].
        assert!(env.view.handle_right_click(5, 3));
        assert_eq!(env.view.cursor, 2, "cursor should move to clicked row");
        let menu = env
            .view
            .context_menu
            .as_ref()
            .expect("context_menu should be open");
        assert_eq!(menu.selected_action(), ContextMenuAction::NewFromSelection);
        // The selected item is a session, not a group.
        assert!(matches!(
            env.view.flat_items[env.view.cursor],
            Item::Session { .. }
        ));
    }

    #[test]
    #[serial]
    fn right_click_off_list_is_noop() {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        // Row 50 is well past list_inner_area.bottom.
        assert!(!env.view.handle_right_click(5, 50));
        assert!(env.view.context_menu.is_none());
    }

    #[test]
    #[serial]
    fn right_click_on_group_uses_group_menu() {
        let mut env = create_test_env_with_groups();
        setup_inner(&mut env);
        // Find a group row index in flat_items.
        let group_idx = env
            .view
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Group { .. }))
            .expect("manual-mode test env should have a group row");
        let click_row = env.view.list_inner_area.y + group_idx as u16;

        assert!(env.view.handle_right_click(5, click_row));
        assert_eq!(env.view.cursor, group_idx);
        assert!(env.view.context_menu.is_some());
        assert!(matches!(
            env.view.flat_items[env.view.cursor],
            Item::Group { .. }
        ));
    }

    #[test]
    #[serial]
    fn enter_rename_in_menu_opens_rename_dialog() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 1);
        assert!(env.view.context_menu.is_some());
        // First item is New Session; Rename is one Down away. Enter submits it.
        env.view.handle_key(key(KeyCode::Down), None);
        env.view.handle_key(key(KeyCode::Enter), None);
        assert!(
            env.view.context_menu.is_none(),
            "menu should close on submit"
        );
        assert!(
            env.view.rename_dialog.is_some(),
            "Rename should route to rename_dialog like the 'r' key"
        );
    }

    #[test]
    #[serial]
    fn down_then_enter_in_menu_opens_delete_dialog() {
        let mut env = create_test_env_with_sessions(2);
        disable_delete_to_trash();
        setup_inner(&mut env);
        // Attention sort surfaces the full session menu (New Session / Rename
        // / Archive / Snooze / Mark unread / Delete), so Delete is five Downs
        // away. (Unread defaults on, so the "Mark unread" row is present.)
        env.view.sort_order = SortOrder::Attention;
        env.view.flat_items = env.view.build_flat_items();
        env.view.handle_right_click(5, 1);
        env.view.handle_key(key(KeyCode::Down), None);
        env.view.handle_key(key(KeyCode::Down), None);
        env.view.handle_key(key(KeyCode::Down), None);
        env.view.handle_key(key(KeyCode::Down), None);
        env.view.handle_key(key(KeyCode::Down), None);
        env.view.handle_key(key(KeyCode::Enter), None);
        assert!(env.view.context_menu.is_none());
        assert!(
            env.view.unified_delete_dialog.is_some(),
            "Delete should route to unified_delete_dialog like the 'd' key"
        );
    }

    #[test]
    #[serial]
    fn esc_in_menu_cancels_without_dialog() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 1);
        env.view.handle_key(key(KeyCode::Esc), None);
        assert!(env.view.context_menu.is_none());
        assert!(env.view.rename_dialog.is_none());
        assert!(env.view.unified_delete_dialog.is_none());
    }

    /// Right-click a session, pick the Archive item (New Session -> Rename ->
    /// Archive is two Downs), and the row gets archived through the same `z`
    /// codepath. No follow-up dialog: archiving is immediate.
    #[test]
    #[serial]
    fn right_click_archive_action_archives_session() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 1);
        let id = env.view.selected_session.clone().unwrap();
        assert!(
            !env.view.get_instance(&id).unwrap().is_archived(),
            "precondition: session starts unarchived"
        );

        env.view.handle_key(key(KeyCode::Down), None); // New Session -> Rename
        env.view.handle_key(key(KeyCode::Down), None); // Rename -> Archive
        env.view.handle_key(key(KeyCode::Enter), None);

        assert!(env.view.context_menu.is_none(), "menu closes after archive");
        assert!(
            env.view.get_instance(&id).unwrap().is_archived(),
            "context-menu Archive must archive the session"
        );
    }

    /// An archived row's context menu offers Unarchive, and picking it restores
    /// the session.
    #[test]
    #[serial]
    fn right_click_unarchive_action_restores_session() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        // Reveal the section and archive the first row so it stays visible.
        env.view.archived_section_collapsed = false;
        env.view.cursor = 0;
        env.view.update_selected();
        let id = env.view.selected_session.clone().unwrap();
        env.view.toggle_archive_at_cursor().unwrap();
        assert!(env.view.get_instance(&id).unwrap().is_archived());

        // Right-click the archived row: its menu must read "Unarchive".
        let idx = env
            .view
            .flat_items
            .iter()
            .position(|it| matches!(it, Item::Session { id: i, .. } if i == &id))
            .expect("archived row must be visible");
        // The archived session row renders in the pinned shelf; render a real
        // frame so the shelf rect is populated, then right-click that row.
        render_geometry(&mut env.view);
        let row = shelf_row_for_idx(&env.view, idx);
        assert!(env.view.handle_right_click(5, row));
        let labels: Vec<&str> = env
            .view
            .context_menu
            .as_ref()
            .unwrap()
            .items_for_test()
            .iter()
            .map(|(_, l)| *l)
            .collect();
        // Default sort here is Newest, where Snooze is gated out. The unread
        // toggle is always-on (any sort) and defaults on. The default session
        // tool is claude (a forkable terminal agent), so the Fork row shows;
        // `right_click_session_menu_hides_fork_for_unforkable_agent` covers the
        // gated-off case. Menu is New Session / Rename / Unarchive / Mark unread
        // / Delete / Fork.
        assert_eq!(
            labels,
            vec![
                "New Session",
                "Rename",
                "Unarchive",
                "Mark unread",
                "Delete",
                "Fork session"
            ]
        );

        env.view.handle_key(key(KeyCode::Down), None); // New Session -> Rename
        env.view.handle_key(key(KeyCode::Down), None); // Rename -> Unarchive
        env.view.handle_key(key(KeyCode::Enter), None);
        assert!(
            !env.view.get_instance(&id).unwrap().is_archived(),
            "context-menu Unarchive must unarchive the session"
        );
    }

    /// A forkable agent (claude, the default test tool) shows the "Fork
    /// session" row so the mouse path matches the palette action.
    #[test]
    #[serial]
    fn right_click_session_menu_shows_fork_for_forkable_agent() {
        let mut env = create_test_env_with_sessions(1);
        setup_inner(&mut env);
        assert!(env.view.handle_right_click(5, 1));
        let actions: Vec<ContextMenuAction> = env
            .view
            .context_menu
            .as_ref()
            .unwrap()
            .items_for_test()
            .iter()
            .map(|(a, _)| *a)
            .collect();
        assert!(
            actions.contains(&ContextMenuAction::Fork),
            "a forkable agent (claude) must show the Fork row"
        );
    }

    /// A resume-only agent (gemini declares `ForkStrategy::Unsupported`) cannot
    /// fork, so the menu must omit the "Fork session" row rather than offer an
    /// action the palette would refuse.
    #[test]
    #[serial]
    fn right_click_session_menu_hides_fork_for_unforkable_agent() {
        let mut env = create_test_env_with_sessions(1);
        setup_inner(&mut env);
        let id = match &env.view.flat_items[0] {
            Item::Session { id, .. } => id.clone(),
            _ => panic!("expected a session row"),
        };
        env.view
            .apply_user_action(&id, |inst| inst.tool = "gemini".to_string())
            .unwrap();
        env.view.flat_items = env.view.build_flat_items();
        assert!(env.view.handle_right_click(5, 1));
        let actions: Vec<ContextMenuAction> = env
            .view
            .context_menu
            .as_ref()
            .unwrap()
            .items_for_test()
            .iter()
            .map(|(a, _)| *a)
            .collect();
        assert!(
            !actions.contains(&ContextMenuAction::Fork),
            "a resume-only agent (gemini) must not show the Fork row"
        );
    }

    /// The Snooze row mirrors the `'h'` keybinding, which only fires in
    /// Attention sort. So the right-click session menu must omit Snooze in
    /// every other sort and include it in Attention sort.
    #[test]
    #[serial]
    fn right_click_session_menu_gates_snooze_to_attention_sort() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);

        let menu_actions = |env: &TestEnv| -> Vec<ContextMenuAction> {
            env.view
                .context_menu
                .as_ref()
                .unwrap()
                .items_for_test()
                .iter()
                .map(|(a, _)| *a)
                .collect()
        };

        // Newest sort (the default): no Snooze row.
        env.view.sort_order = SortOrder::Newest;
        env.view.flat_items = env.view.build_flat_items();
        assert!(env.view.handle_right_click(5, 1));
        assert!(
            !menu_actions(&env).contains(&ContextMenuAction::ToggleSnooze),
            "Snooze must be hidden outside Attention sort"
        );
        env.view.context_menu = None;

        // Attention sort: Snooze row present.
        env.view.sort_order = SortOrder::Attention;
        env.view.flat_items = env.view.build_flat_items();
        assert!(env.view.handle_right_click(5, 1));
        assert!(
            menu_actions(&env).contains(&ContextMenuAction::ToggleSnooze),
            "Snooze must appear in Attention sort"
        );
    }

    /// For a forkable agent the Fork row is sort-independent: unlike Snooze
    /// (gated to Attention sort) it appears in every sort. Whether the row shows
    /// at all is gated on fork capability, covered by the
    /// `..._shows_fork_for_forkable_agent` / `..._hides_fork_for_unforkable_agent`
    /// pair; this test pins that the capability gate does not accidentally
    /// couple to sort order. The default test tool is claude (forkable).
    #[test]
    #[serial]
    fn right_click_session_menu_offers_fork_in_every_sort_for_forkable_agent() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);

        let has_fork = |env: &TestEnv| -> bool {
            env.view
                .context_menu
                .as_ref()
                .unwrap()
                .items_for_test()
                .iter()
                .any(|(a, _)| *a == ContextMenuAction::Fork)
        };

        for sort in [SortOrder::Newest, SortOrder::Attention] {
            env.view.sort_order = sort;
            env.view.flat_items = env.view.build_flat_items();
            assert!(env.view.handle_right_click(5, 1));
            assert!(
                has_fork(&env),
                "Fork must be offered for a forkable agent in {sort:?} sort"
            );
            env.view.context_menu = None;
        }
    }

    #[test]
    #[serial]
    fn right_click_is_gated_when_other_dialog_is_open() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.show_help = true;
        assert!(env.view.has_dialog());
        // resolve_row_to_index short-circuits on any non-live-send overlay,
        // so the right-click handler should bail without opening the menu.
        assert!(!env.view.handle_right_click(5, 1));
        assert!(env.view.context_menu.is_none());
    }

    #[test]
    #[serial]
    fn context_menu_counts_as_dialog() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        assert!(!env.view.has_dialog());
        env.view.handle_right_click(5, 1);
        assert!(env.view.has_dialog());
    }

    #[test]
    #[serial]
    fn left_click_outside_menu_dismisses_it() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 1);
        assert!(env.view.context_menu.is_some());
        // Before a render captures the menu's last_area, every click
        // reads as "outside", which is exactly the dismissal contract
        // we want here. (Item-row hit testing has its own unit coverage
        // in `dialogs::context_menu`.)
        let consumed = env.view.handle_context_menu_click(99, 99);
        assert!(consumed, "router should mark the click consumed");
        assert!(
            env.view.context_menu.is_none(),
            "outside click should dismiss the menu"
        );
    }

    #[test]
    #[serial]
    fn handle_context_menu_click_returns_false_when_no_menu() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        assert!(env.view.context_menu.is_none());
        assert!(!env.view.handle_context_menu_click(5, 5));
    }

    #[test]
    #[serial]
    fn left_click_on_empty_sidebar_outside_live_mode_is_noop() {
        // Left-click on empty sidebar space is intentionally low-stakes:
        // it does NOT open the new-session dialog anymore (right-click
        // owns that entry point) and it doesn't move selection. The
        // user can keep clicking the empty area to dismiss preview
        // selections without summoning modals.
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        // Sessions occupy inner rows 0 and 1 (y=1, y=2). Row 5 is well
        // past the last item but still inside list_inner_area.
        assert!(!env.view.handle_empty_list_click(5, 5));
        assert!(env.view.new_dialog.is_none());
        assert!(env.view.context_menu.is_none());
    }

    #[test]
    #[serial]
    fn left_click_on_empty_sidebar_in_live_mode_exits_live_mode() {
        // Quick-exit gesture: when live-send is active, a click on the
        // empty sidebar drops the user out of live mode. Mirrors the
        // Ctrl+Q chord but with the mouse, so a user who came in via
        // a left-click can also leave that way.
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        use crate::tui::home::live_send;
        env.view.live_send = Some(live_send::LiveSendState {
            session_id: "fake".to_string(),
            title: "fake".to_string(),
            tmux_name: "aoe_test_empty_click_exit_live".to_string(),
            target: live_send::LiveSendTarget::Agent,
            exit_chords: live_send::parse_chord_list(live_send::DEFAULT_EXIT_CHORD),
            leader: None,
        });
        assert!(env.view.live_send.is_some());
        assert!(env.view.handle_empty_list_click(5, 5));
        assert!(
            env.view.live_send.is_none(),
            "click on empty sidebar should exit live mode"
        );
        assert!(env.view.new_dialog.is_none());
    }

    #[test]
    #[serial]
    fn click_on_a_real_row_does_not_change_empty_click_state() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        // Row 1 resolves to flat_items[0], a real session row. The
        // empty-list click handler must defer to the regular click
        // path; it shouldn't open new-session or exit live mode here.
        assert!(!env.view.handle_empty_list_click(5, 1));
        assert!(env.view.new_dialog.is_none());
    }

    #[test]
    #[serial]
    fn empty_sidebar_click_is_gated_when_overlay_is_open() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.show_help = true;
        assert!(!env.view.handle_empty_list_click(5, 5));
        assert!(env.view.new_dialog.is_none());
    }

    #[test]
    #[serial]
    fn right_click_on_empty_sidebar_opens_empty_menu() {
        // Right-clicking the empty area of the sidebar (below the last
        // session) opens the dedicated 3-item menu so the mouse can
        // reach New / Sort / Grouping the same way `n`/`o`/`g` would
        // from the keyboard.
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        assert!(env.view.handle_right_click(5, 5));
        let menu = env.view.context_menu.as_ref().expect("menu opened");
        let labels: Vec<String> = menu
            .items_for_test()
            .iter()
            .map(|(_, label)| (*label).to_string())
            .collect();
        assert_eq!(
            labels,
            vec!["New Session", "Change Sort", "Change Grouping"]
        );
    }

    /// Helper: hit a key through the home view's handle_key path so
    /// the dispatch tests run the same wiring real input does. Both
    /// click and keyboard funnel through `dispatch_context_menu_action`,
    /// so this covers the dispatcher without having to mock the menu's
    /// `last_area` for hit-testing.
    fn send_key(env: &mut crate::tui::home::tests::TestEnv, code: crossterm::event::KeyCode) {
        env.view.handle_key(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            None,
        );
    }

    #[test]
    #[serial]
    fn empty_sidebar_menu_new_session_dispatches() {
        // First item (New Session) submits through the shared
        // dispatcher and opens the new-session dialog.
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 5);
        send_key(&mut env, crossterm::event::KeyCode::Enter);
        assert!(env.view.context_menu.is_none());
        assert!(env.view.new_dialog.is_some());
    }

    #[test]
    #[serial]
    fn empty_sidebar_menu_sort_dispatches() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 5);
        send_key(&mut env, crossterm::event::KeyCode::Down); // highlight "Change Sort"
        send_key(&mut env, crossterm::event::KeyCode::Enter);
        assert!(env.view.context_menu.is_none());
        assert!(env.view.sort_picker_dialog.is_some());
    }

    #[test]
    #[serial]
    fn empty_sidebar_menu_grouping_dispatches() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 5);
        send_key(&mut env, crossterm::event::KeyCode::Down);
        send_key(&mut env, crossterm::event::KeyCode::Down); // highlight "Change Grouping"
        send_key(&mut env, crossterm::event::KeyCode::Enter);
        assert!(env.view.context_menu.is_none());
        assert!(env.view.group_picker_dialog.is_some());
    }

    #[test]
    #[serial]
    fn empty_sidebar_menu_n_hotkey_opens_new_session() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 5);
        send_key(&mut env, crossterm::event::KeyCode::Char('n'));
        assert!(env.view.context_menu.is_none());
        assert!(env.view.new_dialog.is_some());
    }

    #[test]
    #[serial]
    fn empty_sidebar_menu_o_hotkey_opens_sort_picker() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 5);
        send_key(&mut env, crossterm::event::KeyCode::Char('o'));
        assert!(env.view.context_menu.is_none());
        assert!(env.view.sort_picker_dialog.is_some());
    }

    #[test]
    #[serial]
    fn empty_sidebar_menu_g_hotkey_opens_group_picker() {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 5);
        send_key(&mut env, crossterm::event::KeyCode::Char('g'));
        assert!(env.view.context_menu.is_none());
        assert!(env.view.group_picker_dialog.is_some());
    }

    #[test]
    #[serial]
    fn session_menu_n_hotkey_opens_new_session() {
        // The session-row menu now carries a New Session entry (issue #2023),
        // so 'n' submits NewFromSelection just like the group/project menus,
        // closing the menu and opening the new-session dialog prefilled from
        // the right-clicked session.
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 1); // row 1 = first session
        send_key(&mut env, crossterm::event::KeyCode::Char('n'));
        assert!(
            env.view.context_menu.is_none(),
            "menu should close on submit"
        );
        assert!(
            env.view.new_dialog.is_some(),
            "n on session menu must open the new-session dialog"
        );
    }
}

mod apply_session_id_updates {
    //! Post-CAS env publish: env mirrors the disk-confirmed sid
    //! (Applied) or reloaded peer value (Skipped); filter paths
    //! republish the memory mirror to clear `on_change`'s pre-CAS write.

    use super::*;
    use crate::session::poller::SessionPoller;
    use crate::session::ResumeIntent;
    use std::sync::{Arc, Mutex};

    const NEW_SID: &str = "019342ab-1111-7aaa-8bbb-cccdddeeefff";

    struct TmuxSession(String);

    impl TmuxSession {
        fn create(id: &str, title: &str) -> Self {
            Self::create_named(crate::tmux::Session::generate_name(id, title))
        }

        fn create_terminal(id: &str, title: &str) -> Self {
            Self::create_named(crate::tmux::TerminalSession::generate_name(id, title))
        }

        fn create_named(name: String) -> Self {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &name])
                .output();
            let status = crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", &name])
                .status()
                .expect("failed to spawn tmux");
            assert!(status.success(), "tmux new-session failed for {}", name);
            crate::tmux::refresh_session_cache();
            Self(name)
        }
        fn name(&self) -> &str {
            &self.0
        }
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &self.0])
                .output();
            crate::tmux::refresh_session_cache();
        }
    }

    fn skip_if_no_tmux() -> bool {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("Skipping: tmux not available");
            return true;
        }
        false
    }

    fn captured_env(name: &str) -> Option<String> {
        crate::tmux::env::get_hidden_env(name, crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY)
    }

    fn build_view_with_inst(profile: &str, inst: &Instance) -> HomeView {
        use crate::session::config::GroupByMode;
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|i, g| {
                *i = vec![inst.clone()];
                *g = GroupTree::new_with_groups(std::slice::from_ref(inst), &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
        let tools = AvailableTools::with_tools(&["claude"]);
        let mut view = HomeView::new(
            Some(profile.to_string()),
            tools,
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        view.group_by = GroupByMode::Manual;
        view.flat_items = view.build_flat_items();
        view.update_selected();
        view
    }

    fn attach_poller_with_update(view: &mut HomeView, instance_id: &str, sid: &str) {
        let poller = SessionPoller::new("test-session".to_string());
        poller.inject_test_update(instance_id, sid);
        let arc = Arc::new(Mutex::new(poller));
        if let Some(i) = view.instances.get_mut(instance_id) {
            i.session_id_poller = Some(arc);
        }
    }

    fn fresh_instance(profile: &str, title: &str) -> Instance {
        let mut inst = Instance::new(title, "/tmp/x");
        inst.tool = "claude".to_string();
        inst.source_profile = profile.to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Default;
        inst
    }

    #[test]
    #[serial]
    fn apply_session_id_updates_publishes_after_cas() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);

        let profile = "apply-publish";
        let inst = fresh_instance(profile, "apa");
        let mut view = build_view_with_inst(profile, &inst);

        let tmux = TmuxSession::create(&inst.id, &inst.title);

        attach_poller_with_update(&mut view, &inst.id, NEW_SID);

        let updated = view.apply_session_id_updates();
        assert!(updated, "Applied CAS must report a touch");
        assert_eq!(captured_env(tmux.name()).as_deref(), Some(NEW_SID));
    }

    #[test]
    #[serial]
    fn apply_session_id_updates_publishes_to_terminal_session() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);

        let profile = "apply-terminal-publish";
        let mut inst = fresh_instance(profile, "terminal-post-cas");
        inst.terminal_info = Some(crate::session::TerminalInfo { created: true });
        let mut view = build_view_with_inst(profile, &inst);

        let tmux = TmuxSession::create_terminal(&inst.id, &inst.title);
        let agent_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);

        attach_poller_with_update(&mut view, &inst.id, NEW_SID);

        let updated = view.apply_session_id_updates();
        assert!(updated, "Applied CAS must report a touch");
        assert!(captured_env(&agent_name).is_none());
        assert_eq!(captured_env(tmux.name()).as_deref(), Some(NEW_SID));
    }

    #[test]
    #[serial]
    fn apply_session_id_updates_skips_retroactive_excludes() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);

        let profile = "apply-excludes";
        let inst = fresh_instance(profile, "aer");
        let mut view = build_view_with_inst(profile, &inst);
        if let Some(i) = view.instances.get_mut(&inst.id) {
            i.retroactive_capture_excludes.insert(NEW_SID.to_string());
        }

        let tmux = TmuxSession::create(&inst.id, &inst.title);
        crate::tmux::env::set_hidden_env(
            tmux.name(),
            crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            "stale-untouched",
        )
        .unwrap();

        attach_poller_with_update(&mut view, &inst.id, NEW_SID);

        let updated = view.apply_session_id_updates();
        assert!(
            !updated,
            "filtered sid must not propagate to memory (returned bool tracks memory)"
        );
        let mem_sid = view
            .instances
            .get(&inst.id)
            .and_then(|i| i.agent_session_id.clone());
        assert!(
            mem_sid.is_none(),
            "filtered sid must not enter in-memory mirror"
        );
        assert!(
            captured_env(tmux.name()).is_none(),
            "filtered sid must not survive in tmux env: env converges on disk (None)"
        );
    }

    #[test]
    #[serial]
    fn apply_session_id_updates_skipped_publishes_disk_value() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);

        let profile = "apply-skipped";
        let peer_sid = "019342aa-3333-7eee-8fff-aaaabbbbcccc";
        let other_peer = "019342bb-4444-7fff-8000-111122223333";

        let mut inst = fresh_instance(profile, "ase");
        inst.agent_session_id = Some(peer_sid.to_string());
        let mut view = build_view_with_inst(profile, &inst);

        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|i, _g| {
                i[0].agent_session_id = Some(other_peer.to_string());
                Ok(())
            })
            .unwrap();

        let tmux = TmuxSession::create(&inst.id, &inst.title);
        crate::tmux::env::set_hidden_env(
            tmux.name(),
            crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            NEW_SID,
        )
        .unwrap();

        attach_poller_with_update(&mut view, &inst.id, NEW_SID);

        let updated = view.apply_session_id_updates();
        assert!(updated, "Skipped path still touches state");

        let mem_sid = view
            .instances
            .get(&inst.id)
            .and_then(|i| i.agent_session_id.clone());
        assert_eq!(
            mem_sid.as_deref(),
            Some(other_peer),
            "memory rolls back to disk after CAS skip"
        );
        assert_eq!(
            captured_env(tmux.name()).as_deref(),
            Some(other_peer),
            "env converges from poller's pre-published NEW_SID to disk's other_peer"
        );
    }

    #[test]
    #[serial]
    fn apply_session_id_updates_invalid_sid_corrects_env() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);

        let profile = "apply-invalid";
        let inst = fresh_instance(profile, "aiv");
        let mut view = build_view_with_inst(profile, &inst);

        let tmux = TmuxSession::create(&inst.id, &inst.title);
        crate::tmux::env::set_hidden_env(
            tmux.name(),
            crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            "bad sid!",
        )
        .unwrap();

        attach_poller_with_update(&mut view, &inst.id, "bad sid!");

        let updated = view.apply_session_id_updates();
        assert!(
            !updated,
            "validation-filtered sid must not propagate to memory"
        );
        assert!(
            captured_env(tmux.name()).is_none(),
            "env converges to disk-backed memory mirror (None) after validation failure"
        );
    }

    #[test]
    #[serial]
    fn apply_session_id_updates_no_tmux_session_skips_publish() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);

        let profile = "apply-pane-dead";
        let inst = fresh_instance(profile, "apds");
        let mut view = build_view_with_inst(profile, &inst);

        attach_poller_with_update(&mut view, &inst.id, NEW_SID);

        let updated = view.apply_session_id_updates();
        assert!(
            updated,
            "CAS still applies even when no tmux session exists"
        );
        let mem_sid = view
            .instances
            .get(&inst.id)
            .and_then(|i| i.agent_session_id.clone());
        assert_eq!(
            mem_sid.as_deref(),
            Some(NEW_SID),
            "memory still mirrors the CAS-applied sid",
        );
        let expected_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        assert!(
            captured_env(&expected_name).is_none(),
            "no tmux session means no publish target"
        );
    }

    /// Discarding unsaved Settings changes via a mouse click on the
    /// confirmation dialog's [Yes] button must revert a live theme preview,
    /// exactly like the keyboard discard path. Regression for the
    /// empire -> rose-pine flip where the click path closed Settings but
    /// never dispatched `SetTheme`, leaving the previewed theme applied until
    /// the next restart.
    #[test]
    #[serial]
    fn settings_mouse_discard_reverts_theme_preview() {
        use crate::tui::dialogs::ConfirmDialog;
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut env = create_test_env_empty();
        let view = &mut env.view;
        view.open_settings();
        assert!(view.settings_view.is_some(), "settings view should open");

        // Stand in the state reached after the user previewed a theme (so the
        // view has unsaved changes) and pressed Esc to close: the unsaved-
        // changes confirm dialog floats over the settings takeover.
        view.settings_close_confirm = true;
        view.confirm_dialog = Some(ConfirmDialog::new(
            "Unsaved Changes",
            "You have unsaved changes. Discard them?",
            "discard_settings",
        ));

        // Render once so the dialog's [Yes] button hit-rect is populated at the
        // exact coordinates it draws.
        let theme = load_theme("empire");
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                view.render(f, area, &theme, None, None, None);
            })
            .unwrap();

        let yes = view
            .confirm_dialog
            .as_ref()
            .unwrap()
            .yes_button_area_for_test();
        assert!(yes.width > 0, "render should populate the [Yes] hit-rect");

        // Click the center of [Yes] to discard.
        view.handle_dialog_click(yes.x + yes.width / 2, yes.y + yes.height / 2);

        // The click path must queue the same theme revert the keyboard path
        // returns. Before the fix this was `None` and the previewed theme stuck.
        assert!(
            matches!(view.pending_dialog_click_action, Some(Action::SetTheme(_))),
            "mouse discard should queue a SetTheme revert, got {:?}",
            view.pending_dialog_click_action
        );
        assert!(view.settings_view.is_none(), "settings should be closed");
        assert!(!view.settings_close_confirm, "confirm flag should reset");
    }
}

/// Live-send must boot a cold/dead agent pane at the visible preview size so it
/// does not start at tmux's 80x24 default and then depend on a single,
/// race-prone post-boot `resize-window` SIGWINCH to grow into the live area.
/// Regression guard for the "live mode opens at ~50% width" race: if this seed
/// regresses back to `None`/default, the agent boots narrow again.
mod live_send_boot_size_tests {
    use super::create_test_env_empty;
    use ratatui::layout::Rect;

    #[test]
    #[serial_test::serial]
    fn boots_agent_at_visible_preview_size() {
        let mut env = create_test_env_empty();
        // The rect `finalize_live_send_resize` would resize the pane to.
        env.view.preview_pane_area = Rect::new(35, 1, 123, 38);

        assert_eq!(
            env.view.live_send_boot_size(),
            Some((123, 38)),
            "cold agent must boot at the preview pane's visible size, not tmux's 80x24 default"
        );
    }

    #[test]
    #[serial_test::serial]
    fn never_seeds_a_zero_sized_pane() {
        let mut env = create_test_env_empty();
        // No preview has been drawn yet (attach-on-create style entry): the
        // cached rect is empty. The seed must fall back, never hand tmux a
        // degenerate 0x0 boot size.
        env.view.preview_pane_area = Rect::default();

        let seed = env.view.live_send_boot_size();
        assert!(
            !matches!(seed, Some((0, _)) | Some((_, 0))),
            "empty preview rect must fall back, not seed a 0-dimension size; got {seed:?}"
        );
    }
}

mod stacked_single_seam {
    //! Regression coverage for #2301: the list and preview panes must meet
    //! on a single shared border in every layout (DESIGN.md invariant). The
    //! stacked layout used to draw the list's BOTTOM and preview's TOP as
    //! two adjacent horizontal borders, producing a visible doubled seam.

    use super::*;
    use crate::tui::responsive;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Row indices whose glyphs are dominated by the horizontal box-drawing
    /// char used by ratatui's `BorderType::Rounded`. Includes both outer box
    /// borders and any internal horizontal dividers; the caller relies on
    /// adjacency to detect a doubled seam, not on the rows being borders
    /// exclusively.
    fn horizontal_dense_rows(buf: &ratatui::buffer::Buffer) -> Vec<u16> {
        const HORIZONTAL: &str = "─";
        let mut rows = Vec::new();
        for y in 0..buf.area.height {
            let mut count = 0u16;
            for x in 0..buf.area.width {
                if buf[(x, y)].symbol() == HORIZONTAL {
                    count += 1;
                }
            }
            // Half the row width is well above the noise floor (title text,
            // status glyphs, sort indicator) yet still catches the sparse
            // horizontal chars in a narrow preview.
            if count >= buf.area.width / 2 {
                rows.push(y);
            }
        }
        rows
    }

    fn render_home(env: &mut TestEnv, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let theme = load_theme("empire");
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    #[serial]
    fn stacked_layout_has_single_horizontal_seam() {
        let mut env = create_test_env_with_sessions(2);
        const { assert!(60 < responsive::STACKED_BREAKPOINT) };
        let buf = render_home(&mut env, 60, 40);

        let border_rows = horizontal_dense_rows(&buf);
        assert!(
            border_rows.len() >= 3,
            "expected at least list-top, shared-seam, and preview-bottom rows; got {border_rows:?}"
        );
        for pair in border_rows.windows(2) {
            assert!(
                pair[1] - pair[0] > 1,
                "doubled seam detected: horizontal borders on adjacent rows {} and {} \
                 (issue #2301). All horizontal-border rows: {:?}",
                pair[0],
                pair[1],
                border_rows,
            );
        }
    }

    #[test]
    #[serial]
    fn side_by_side_layout_preserves_horizontal_borders() {
        let mut env = create_test_env_with_sessions(2);
        const { assert!(120 >= responsive::STACKED_BREAKPOINT) };
        let buf = render_home(&mut env, 120, 40);

        let border_rows = horizontal_dense_rows(&buf);
        assert!(
            !border_rows.is_empty(),
            "side-by-side must still draw horizontal borders; got {border_rows:?}"
        );
        // The single-shared-separator invariant is a global rule, not a
        // stacked-only rule: if the enum threading ever grows an asymmetric
        // path that re-doubles a seam in side-by-side, this catches it.
        for pair in border_rows.windows(2) {
            assert!(
                pair[1] - pair[0] > 1,
                "unexpected horizontal seam doubling in side-by-side between rows {} and {}; \
                 all border rows: {:?}",
                pair[0],
                pair[1],
                border_rows,
            );
        }
    }
}

mod permission_response_dialog {
    use super::*;
    use crate::session::Status;

    fn add_session_with_tool(view: &mut HomeView, title: &str, tool: &str) -> String {
        let mut inst = Instance::new(title, "/tmp/test");
        inst.tool = tool.to_string();
        let id = inst.id.clone();
        view.add_instance(inst);
        id
    }

    #[test]
    #[serial]
    fn no_selected_session_is_a_no_op() {
        let mut env = create_test_env_empty();
        env.view.selected_session = None;
        let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
        assert!(env.view.permission_response_dialog.is_none());
        assert!(env.view.info_dialog.is_none());
    }

    #[test]
    #[serial]
    fn unsupported_agent_shows_info_dialog_no_send() {
        let mut env = create_test_env_empty();
        let id = add_session_with_tool(&mut env.view, "session-one", "some-unmapped-tool");
        env.view.selected_session = Some(id);
        let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
        assert!(
            env.view.permission_response_dialog.is_none(),
            "unsupported agent must not open the dialog"
        );
        assert!(
            env.view.info_dialog.is_some(),
            "unsupported agent must surface an info dialog"
        );
    }

    #[test]
    #[serial]
    fn supported_agent_opens_dialog_regardless_of_status() {
        let mut env = create_test_env_empty();
        let id = add_session_with_tool(&mut env.view, "session-one", "claude");
        env.view.selected_session = Some(id.clone());
        // Prove there's no Status::Waiting gate: explicitly set a
        // non-Waiting status before pressing the shortcut.
        env.view
            .mutate_instance(&id, |inst| inst.status = Status::Idle);
        let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
        assert!(
            env.view.permission_response_dialog.is_some(),
            "supported agent + valid target must open the dialog even when not Waiting"
        );
    }

    #[test]
    #[serial]
    fn structured_session_is_a_no_op() {
        let mut env = create_test_env_empty();
        let id = add_session_with_tool(&mut env.view, "session-one", "claude");
        env.view
            .mutate_instance(&id, |inst| inst.view = crate::session::View::Structured);
        env.view.selected_session = Some(id);
        let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
        assert!(
            env.view.permission_response_dialog.is_none(),
            "structured (ACP) session must not open the tmux-keystroke dialog"
        );
        assert!(
            env.view.info_dialog.is_none(),
            "structured session no-op must be silent, not surface an info dialog"
        );
    }
}
