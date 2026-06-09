//! Tests for HomeView

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;
use tempfile::TempDir;
use tui_input::Input;

use super::{HomeView, ViewMode};
use crate::session::{GroupTree, Instance, Item, Storage};
use crate::tmux::AvailableTools;
use crate::tui::app::Action;
use crate::tui::dialogs::{InfoDialog, NewSessionDialog};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn setup_test_home(temp: &TempDir) {
    std::env::set_var("HOME", temp.path());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
}

struct TestEnv {
    _temp: TempDir,
    view: HomeView,
}

fn create_test_env_empty() -> TestEnv {
    use crate::session::config::GroupByMode;
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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
    TestEnv { _temp: temp, view }
}

#[test]
#[serial]
fn rewire_disk_subscriptions_is_noop_without_tokio_runtime() {
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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
        view.disk_watch_handles.is_empty(),
        "construction outside a tokio runtime must not prewire subscriptions"
    );
    view.rewire_disk_subscriptions(&current).unwrap();
    assert!(
        view.disk_watch_handles.is_empty(),
        "rewire outside a tokio runtime must stay a no-op for lib tests"
    );
    assert!(
        !view.disk_dirty.load(std::sync::atomic::Ordering::Acquire),
        "the noop branch must leave disk_dirty clear outside a runtime"
    );
}

fn create_test_env_with_sessions(count: usize) -> TestEnv {
    use crate::session::config::GroupByMode;
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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
    TestEnv { _temp: temp, view }
}

fn create_test_env_with_groups() -> TestEnv {
    use crate::session::config::GroupByMode;
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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
    TestEnv { _temp: temp, view }
}

fn create_test_env_with_mixed_sessions() -> TestEnv {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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
    TestEnv { _temp: temp, view }
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
    let id = env.view.instances()[0].id.clone();
    env.view.select_session_by_id(&id);
    env.view.view_mode = ViewMode::Structured;
    let theme = load_theme("empire");

    let render_to_string = |view: &mut HomeView| {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                view.render(f, area, &theme, None, None);
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
    let id = env.view.instances()[0].id.clone();
    env.view.select_session_by_id(&id);
    env.view.view_mode = ViewMode::Structured;
    env.view.show_preview_info = true;

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None);
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
fn test_b_shows_info_dialog_when_no_projects() {
    let mut env = create_test_env_empty();
    assert!(env.view.info_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(env.view.info_dialog.is_some());
    assert!(env.view.project_session_picker_dialog.is_none());
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
    setup_test_home(&temp);
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
fn test_search_mode_enter_exits_and_clears_state() {
    let mut env = create_test_env_with_sessions(3);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(!env.view.search_active);
    assert_eq!(env.view.search_query.value(), "");
    assert!(env.view.search_matches.is_empty());
    assert_eq!(env.view.search_match_index, 0);
}

#[test]
#[serial]
fn test_d_on_session_opens_delete_dialog() {
    let mut env = create_test_env_with_sessions(3);
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
fn test_enter_clears_matches_so_n_opens_new_dialog() {
    let mut env = create_test_env_with_sessions(5);
    // Search, then Enter to exit search mode
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(!env.view.search_active);
    // Enter should have cleared matches
    assert!(env.view.search_matches.is_empty());

    // n should now open new session dialog (not cycle matches)
    assert!(env.view.new_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert!(env.view.new_dialog.is_some());
}

#[test]
#[serial]
fn test_reload_does_not_snap_cursor_after_enter() {
    let mut env = create_test_env_with_sessions(5);
    // Search and exit with Enter
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
fn test_enter_clears_matches_and_resets_index() {
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    let match_count = env.view.search_matches.len();
    assert!(match_count > 0);

    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(!env.view.search_active);
    // Enter should clear matches so normal keybindings work
    assert!(env.view.search_matches.is_empty());
    assert_eq!(env.view.search_match_index, 0);
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
    let session_id = env.view.instances()[1].id.clone();

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
    setup_test_home(&temp);

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
    setup_test_home(&temp);
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
    TestEnv { _temp: temp, view }
}

#[test]
#[serial]
fn test_group_has_managed_worktrees() {
    use crate::session::WorktreeInfo;
    use chrono::Utc;

    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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

    assert!(view.group_has_managed_worktrees("work", "work/"));
    assert!(!view.group_has_managed_worktrees("other", "other/"));
}

#[test]
#[serial]
fn test_group_has_containers() {
    use crate::session::SandboxInfo;

    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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

    assert!(view.group_has_containers("work", "work/"));
    assert!(!view.group_has_containers("other", "other/"));
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
        .iter()
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
    setup_test_home(&temp);
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
    setup_test_home(&temp);
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
    let fresh_tree = GroupTree::new_with_groups(env.view.instances(), &groups);
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
/// Statuses are seeded in storage before construction so both `instances` and
/// the `instance_map` that `get_instance`/`jump_to_next_waiting` read agree.
fn attention_env_running_then_waiting() -> (TestEnv, usize, usize) {
    use crate::session::config::{GroupByMode, SortOrder};
    use crate::session::Status;

    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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
    let env = TestEnv { _temp: temp, view };

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
    setup_test_home(&temp);

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
        .iter()
        .map(|i| i.source_profile.as_str())
        .collect();
    assert!(profiles.contains(&"alpha"));
    assert!(profiles.contains(&"beta"));
}

#[test]
#[serial]
fn test_filtered_view_loads_single_profile() {
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);

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
    assert_eq!(view.instances()[0].title, "Alpha Session");
    assert_eq!(view.instances()[0].source_profile, "alpha");
}

#[test]
#[serial]
fn test_all_profiles_view_has_no_profile_headers() {
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);

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
    setup_test_home(&temp);

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

/// Default `RowTagMode::None` renders no tag in any view; existing users
/// see no change from the row-tag feature being added.
#[test]
#[serial]
fn test_default_row_tag_mode_renders_no_tag() {
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);

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
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Default `row_tag_mode` is `None`; no row should carry a bracketed tag.
    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                !text.contains('['),
                "default RowTagMode::None must render no tag: {text:?}"
            );
        }
    }
}

/// `RowTagMode::Auto` shows the profile short code in all-profiles view.
#[test]
#[serial]
fn test_row_tag_auto_renders_profile_in_all_profiles_view() {
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);

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
    setup_test_home(&temp);

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
    setup_test_home(&temp);

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

/// `RowTagMode::Branch` complements the existing branch-on-divergence
/// display rather than duplicating it: when `worktree.branch != title`
/// the divergence display already shows the branch (in `theme.branch`
/// color, earlier in the row), so the Branch tag suppresses itself to
/// avoid showing the same information twice.
#[test]
#[serial]
fn test_row_tag_branch_dedups_with_divergence_display() {
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);

    let storage = Storage::new_unwatched("alpha").unwrap();
    // Title and branch DIFFER, so the existing divergence display
    // would render the branch.
    let mut inst = Instance::new("my-session", "/tmp/a");
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

    // No bracketed `[...]` tag on this row: divergence display owns the
    // branch label here. The plain `feature/foo` from the divergence
    // display is still expected in the rendered text.
    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                !text.contains('['),
                "Branch mode must suppress its tag when divergence display already shows the branch: {text:?}"
            );
            assert!(
                text.contains("feature/foo"),
                "the existing divergence display should still render: {text:?}"
            );
        }
    }
}

/// `RowTagMode::Branch` DOES render the tag when title matches branch
/// (the divergence display stays quiet, so the user would otherwise not
/// know which branch this session is on).
#[test]
#[serial]
fn test_row_tag_branch_renders_when_title_matches_branch() {
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);

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

    // The tag uses the last `/`-segment of the branch, truncated to 8
    // chars, so `feature/foo` becomes `foo` padded to width 8.
    let rendered = super::render::RowTag {
        content: "foo".to_string(),
        max_width: 8,
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

/// Legacy `Instance::new` left `source_profile` empty before the per-profile
/// plumbing landed. The render branch must skip the tag entirely in that
/// case rather than emit a literal `  []`.
#[test]
#[serial]
fn test_row_tag_auto_skips_for_empty_source_profile() {
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);

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
    setup_test_home(&temp);

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
    setup_test_home(&temp);

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
    setup_test_home(&temp);
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
    setup_test_home(&temp);

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
        .iter()
        .find(|i| i.source_profile == "alpha")
        .unwrap();
    assert_eq!(
        alpha_inst.group_path, "",
        "alpha's instance should be ungrouped"
    );
    let beta_inst = view
        .instances()
        .iter()
        .find(|i| i.source_profile == "beta")
        .unwrap();
    assert_eq!(
        beta_inst.group_path, "work",
        "beta's instance should still be in 'work'"
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
    setup_test_home(&temp);
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
        .iter()
        .find(|i| i.title == "work-project")
        .unwrap();
    assert_eq!(work_session.group_path, "projects");
}

#[test]
#[serial]
fn test_rename_selected_group_with_children() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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
        .iter()
        .find(|i| i.title == "parent-session")
        .unwrap();
    assert_eq!(parent.group_path, "projects");

    let child = view
        .instances()
        .iter()
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
        .iter()
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
    setup_test_home(&temp);
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
    setup_test_home(&temp);
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
    setup_test_home(&temp);
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
    use crate::session::config::{save_config, Config, SortOrder};
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);

    let mut config = Config::default();
    config.app_state.sort_order = Some(SortOrder::AZ);
    save_config(&config).unwrap();

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
    setup_test_home(&temp);

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
    setup_test_home(&temp);
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
    use crate::tui::status_poller::StatusUpdate;

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
        idle_entered_at: Some(now),
        last_accessed_at: None,
        pane_dead: false,
    });

    let inst = env.view.get_instance(&id).unwrap();
    assert_eq!(inst.status, Status::Idle);
    assert_eq!(inst.idle_entered_at, Some(now));
}

#[test]
#[serial]
fn apply_status_update_clears_idle_entered_at_on_idle_to_running() {
    use crate::session::Status;
    use crate::tui::status_poller::StatusUpdate;

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
        idle_entered_at: Some(stop_time),
        last_accessed_at: None,
        pane_dead: false,
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
        idle_entered_at: None,
        last_accessed_at: None,
        pane_dead: false,
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
    use crate::tui::status_poller::StatusUpdate;

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
        idle_entered_at: Some(stale_ts),
        last_accessed_at: None,
        pane_dead: false,
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
    use crate::tui::status_poller::StatusUpdate;

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    env.view.status_hook_config = StatusHookConfig {
        enabled: true,
        debounce_ms: 0,
        on_waiting: Some("notify-waiting".to_string()),
        on_change: Some("notify-change".to_string()),
        ..Default::default()
    };
    take_recorded_launches();

    env.view.apply_one_status_update(StatusUpdate {
        id: id.clone(),
        status: Status::Waiting,
        last_error: None,
        idle_entered_at: None,
        last_accessed_at: None,
        pane_dead: false,
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
            debounce_ms: 0,
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
    use crate::tui::status_poller::StatusUpdate;

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    env.view.status_hook_config = StatusHookConfig {
        enabled: true,
        debounce_ms: 0,
        on_change: Some("notify-change".to_string()),
        ..Default::default()
    };
    take_recorded_launches();

    env.view.apply_one_status_update(StatusUpdate {
        id,
        status: Status::Idle,
        last_error: None,
        idle_entered_at: None,
        last_accessed_at: None,
        pane_dead: false,
    });

    assert!(take_recorded_launches().is_empty());
}

#[test]
#[serial]
fn apply_status_updates_without_hooks_does_not_run_status_hook() {
    use crate::session::Status;
    use crate::status_hooks::{take_recorded_launches, StatusHookConfig};
    use crate::tui::status_poller::StatusUpdate;

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    env.view.status_hook_config = StatusHookConfig {
        enabled: true,
        debounce_ms: 0,
        on_waiting: Some("notify-waiting".to_string()),
        ..Default::default()
    };
    take_recorded_launches();

    env.view
        .apply_status_updates_without_hooks(vec![StatusUpdate {
            id: id.clone(),
            status: Status::Waiting,
            last_error: None,
            idle_entered_at: None,
            last_accessed_at: None,
            pane_dead: false,
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
        debounce_ms: 0,
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
            env.view.render(f, area, &theme, None, Some(toast));
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
    let id_skipped = env.view.instances[1].id.clone();
    env.view.recovery_in_flight.insert(id_skipped.clone());

    let pollable = env.view.pollable_instances();

    assert_eq!(pollable.len(), 2);
    assert!(pollable.iter().all(|i| i.id != id_skipped));
}

#[test]
#[serial]
fn pollable_instances_recovers_after_inflight_clear() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instances[0].id.clone();
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
                env.view.render(f, area, &theme, None, None);
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
    let id = env.view.instances[0].id.clone();
    env.view.selected_session = Some(id.clone());

    // Initial state: not favorited.
    assert!(!env.view.instances[0].is_favorited());

    env.view.toggle_favorite_at_cursor().unwrap();
    assert!(env.view.instances[0].is_favorited());

    env.view.toggle_favorite_at_cursor().unwrap();
    assert!(!env.view.instances[0].is_favorited());
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
    let id = env.view.instances[0].id.clone();
    env.view.selected_session = Some(id.clone());

    // Initial state: not archived.
    assert!(!env.view.instances[0].is_archived());

    env.view.toggle_archive_at_cursor().unwrap();
    assert!(env.view.instances[0].is_archived());

    env.view.toggle_archive_at_cursor().unwrap();
    assert!(!env.view.instances[0].is_archived());
}

/// When no session is selected, the toggle is a silent no-op.
#[test]
#[serial]
fn toggle_archive_at_cursor_noop_with_no_selection() {
    let mut env = create_test_env_empty();
    env.view.selected_session = None;
    env.view.toggle_archive_at_cursor().unwrap();
}

/// Approach 2: archiving in the default (non-Attention) sort keeps the cursor
/// AND selection on the just-archived session instead of swapping to a
/// neighbor, and reveals the Archived section so the row stays visible. This is
/// what lets the preview render the calm "Archived" placeholder for the same
/// row the user is looking at, rather than flashing a dead-pane warning and
/// then snapping selection to the session below.
#[test]
#[serial]
fn archive_keeps_selection_and_reveals_section() {
    let mut env = create_test_env_with_sessions(2);
    // Start with the Archived section collapsed (the default / reported repro).
    env.view.archived_section_collapsed = true;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();

    env.view.toggle_archive_at_cursor().unwrap();

    assert!(
        env.view.get_instance(&id).unwrap().is_archived(),
        "the session must be archived"
    );
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(id.as_str()),
        "selection must stay on the archived session, not swap to a neighbor"
    );
    assert!(
        !env.view.archived_section_collapsed,
        "archiving must reveal the Archived section so the row stays visible"
    );
    match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id: cur, .. }) => {
            assert_eq!(cur, &id, "cursor must land on the archived row")
        }
        _ => panic!("cursor should be on the archived session row"),
    }
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
    let result = env.view.restart_selected_session(None, None);
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
    let id = env.view.instances[0].id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.mutate_instance(&id, |inst| inst.archive());

    let result = env.view.restart_selected_session(None, None);
    assert!(result.is_ok());
    assert!(
        env.view.instances[0].is_archived(),
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
    let id = env.view.instances[0].id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.sort_order = SortOrder::Attention;
    env.view.mutate_instance(&id, |inst| inst.snooze(30));

    let result = env.view.restart_selected_session(None, None);
    assert!(result.is_ok());
    assert!(
        env.view.instances[0].is_snoozed(),
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
    let id = env.view.instances[0].id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.sort_order = SortOrder::Newest;
    env.view.mutate_instance(&id, |inst| inst.snooze(30));
    assert!(env.view.instances[0].is_snoozed(), "pre-condition");

    let result = env.view.restart_selected_session(None, None);
    assert!(result.is_ok());
    assert!(
        !env.view.instances[0].is_snoozed(),
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
    let id = env.view.instances[0].id.clone();
    env.view.selected_session = Some(id.clone());
    env.view
        .mutate_instance(&id, |inst| inst.status = crate::session::Status::Creating);

    let result = env.view.restart_selected_session(None, None);
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
    let id = env.view.instances[0].id.clone();
    env.view.selected_session = Some(id.clone());

    // Seed the cooldown so the next press is debounced. This stands in
    // for the "first restart already ran" precondition: we cannot run
    // restart_with_size in a unit test (no tmux), but the debounce check
    // happens before that, on the cooldown map.
    let now = std::time::Instant::now();
    env.view.restart_cooldown_at.insert(id.clone(), now);

    let result = env.view.restart_selected_session(None, None);
    assert!(result.is_ok());
    let stored = env.view.restart_cooldown_at.get(&id).copied().unwrap();
    assert_eq!(
        stored, now,
        "cooldown timestamp must not be overwritten on a debounced press"
    );
}

/// Build a HomeView seeded with two distinct projects, each containing
/// sessions with different attention statuses. Helper for the Project +
/// Attention combination tests below.
fn create_test_env_two_projects_mixed_attention() -> TestEnv {
    use crate::session::Status;
    let temp = TempDir::new().unwrap();
    setup_test_home(&temp);
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
    TestEnv { _temp: temp, view }
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
                    if let Some(inst) = env.view.instances.iter().find(|i| &i.id == id) {
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
        .iter()
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
    setup_test_home(&temp);
    let _ = Storage::new_unwatched("alpha").unwrap();
    let _ = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();

    // Pre-state: alpha has one session in group "work", beta is empty.
    let mut moved = Instance::new("moved", "/tmp/moved");
    moved.source_profile = "alpha".to_string();
    moved.group_path = "work".to_string();
    view.instances = vec![moved];
    view.group_trees.clear();
    view.group_trees.insert(
        "alpha".to_string(),
        GroupTree::new_with_groups(&view.instances, &[]),
    );
    view.group_trees
        .insert("beta".to_string(), GroupTree::new_with_groups(&[], &[]));
    assert!(view.group_trees["alpha"].group_exists("work"));

    // Simulate the move: re-tag source_profile, then prune the now-empty
    // source group.
    view.instances[0].source_profile = "beta".to_string();
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
    setup_test_home(&temp);
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
    view.instances = vec![moved, sibling];
    view.group_trees.clear();
    view.group_trees.insert(
        "alpha".to_string(),
        GroupTree::new_with_groups(&view.instances, &[]),
    );
    view.group_trees
        .insert("beta".to_string(), GroupTree::new_with_groups(&[], &[]));

    view.instances[0].source_profile = "beta".to_string();
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
    setup_test_home(&temp);
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
    view.instances = vec![moved, nested];
    view.group_trees.clear();
    view.group_trees.insert(
        "alpha".to_string(),
        GroupTree::new_with_groups(&view.instances, &[]),
    );
    view.group_trees
        .insert("beta".to_string(), GroupTree::new_with_groups(&[], &[]));

    view.instances[0].source_profile = "beta".to_string();
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
    setup_test_home(&temp);
    let _ = Storage::new_unwatched("alpha").unwrap();
    let _ = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();

    let mut moved = Instance::new("moved", "/tmp/moved");
    moved.source_profile = "alpha".to_string();
    moved.group_path = "work".to_string();
    view.instances = vec![moved];
    view.group_trees.clear();
    let mut alpha_tree = GroupTree::new_with_groups(&view.instances, &[]);
    alpha_tree.create_group("work/anchor");
    view.group_trees.insert("alpha".to_string(), alpha_tree);
    view.group_trees
        .insert("beta".to_string(), GroupTree::new_with_groups(&[], &[]));
    assert!(view.group_trees["alpha"].group_exists("work/anchor"));

    view.instances[0].source_profile = "beta".to_string();
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
    setup_test_home(&temp);
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
            inst.source_profile = "alpha".to_string();
            inst.group_path = "work".to_string();
            inst
        };
        view.instance_map.insert("moved".to_string(), moved.clone());
        view.instances.push(moved);
        view.pending_added
            .entry("alpha".to_string())
            .or_default()
            .insert("moved".to_string());
        view.group_trees.insert(
            "alpha".to_string(),
            GroupTree::new_with_groups(&view.instances, &[]),
        );
        view.save().unwrap();

        view.group_trees
            .entry("beta".to_string())
            .or_insert_with(|| GroupTree::new_with_groups(&[], &[]));
        let old_path = view.instance_map["moved"].group_path.clone();
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
    let id = env.view.instances[0].id.clone();
    let title = env.view.instances[0].title.clone();
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
    let id = env.view.instances[0].id.clone();
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
    let id = env.view.instances[0].id.clone();
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
        .iter()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_id = env
        .view
        .instances
        .iter()
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
        .iter()
        .find(|i| i.title == "beta-error")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_running = env
        .view
        .instances
        .iter()
        .find(|i| i.title == "beta-running")
        .map(|i| i.id.clone())
        .unwrap();
    env.view
        .apply_user_action(&beta_error, |inst| inst.archive())
        .unwrap();
    env.view.instances.retain(|i| i.id != beta_running);
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
        .iter()
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
        .iter()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_id = env
        .view
        .instances
        .iter()
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
        .iter()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_id = env
        .view
        .instances
        .iter()
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

    /// Leader + q exits live mode and resets the live-only UI state.
    #[test]
    #[serial]
    fn live_leader_q_exits() {
        let mut env = live_env_with_leader();
        env.view.sidebar_collapsed = true;
        env.view.handle_key(ctrl('b'), None);
        env.view.handle_key(key(KeyCode::Char('q')), None);
        assert!(env.view.live_send.is_none(), "leader+q exits live mode");
        assert!(
            !env.view.sidebar_collapsed,
            "exiting must re-reveal the sidebar"
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
        assert!(!env.view.sidebar_collapsed, "live-only state is reset");
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
                    env.view.render(f, area, &theme, None, None);
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
        use crate::session::config::{save_config, ClickAction, Config};
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let mut config = Config::default();
        config.session.click_action = ClickAction::SelectOnly;
        save_config(&config).unwrap();

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
        let row = env.view.list_inner_area.y + idx as u16;
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
        use crate::session::config::{save_config, ClickAction, Config};
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();

        let mut config = Config::default();
        config.session.click_action = ClickAction::SelectOnly;
        save_config(&config).unwrap();

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
        // `dimensions.1 - 1` is the visible-height the scroll clamp uses;
        // pin it to the pane height so the auto-scroll max offset matches
        // what `first_line` implies.
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
                env.view.render(f, area, &theme, None, None);
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
                env.view.render(f, area, &theme, None, None);
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

/// Tests for the `new_session_attach_mode` setting that drives whether
/// a freshly-created session enters tmux or live-send mode. The unit
/// under test is `HomeView::new_session_attach_mode`, plus the
/// invariant that the sync create path emits the routed action variant
/// (so it doesn't bypass the setting the way `Action::AttachSession`
/// would).
mod new_session_attach_mode {
    use super::*;
    use crate::session::config::{save_config, Config, NewSessionAttachMode};

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
    fn write_global_attach_mode(mode: NewSessionAttachMode) {
        let mut config = Config::default();
        config.session.new_session_attach_mode = mode;
        save_config(&config).unwrap();
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
        let mode = env.view.new_session_attach_mode(&id);
        assert_eq!(
            mode,
            Some(NewSessionAttachMode::Tmux),
            "default must be Tmux to preserve existing UX"
        );
    }

    #[test]
    #[serial]
    fn returns_live_send_when_globally_configured() {
        // User saved `new_session_attach_mode = "live_send"` in their
        // global config. The resolver must pick it up so the dispatch
        // path in app.rs routes to live mode instead of tmux attach.
        let mut env = create_test_env_empty();
        write_global_attach_mode(NewSessionAttachMode::LiveSend);
        let id = add_session(&mut env.view, "session-one");
        let mode = env.view.new_session_attach_mode(&id);
        assert_eq!(mode, Some(NewSessionAttachMode::LiveSend));
    }

    #[test]
    #[serial]
    fn returns_none_for_missing_instance() {
        // Race: the apply_creation_results return reaches the dispatch
        // and the instance has been deleted in the meantime. `None`
        // signals the caller to fall back to the structured view-aware
        // attach_session path rather than try to attach to a ghost.
        let env = create_test_env_empty();
        let mode = env.view.new_session_attach_mode("nonexistent-id");
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
        write_global_attach_mode(NewSessionAttachMode::LiveSend);
        let id = add_session(&mut env.view, "acp-one");
        env.view.mutate_instance(&id, |inst| {
            inst.view = crate::session::View::Structured;
        });
        let mode = env.view.new_session_attach_mode(&id);
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
        }
    }

    #[test]
    #[serial]
    fn sync_create_path_emits_attach_after_create_not_attach_session() {
        // Regression guard for the original bug. `Action::AttachSession`
        // would skip the `new_session_attach_mode` dispatch; only
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
    use crate::session::config::{save_config, Config, NewSessionAttachMode};

    fn add_session(view: &mut HomeView, title: &str) -> String {
        let mut inst = Instance::new(title, "/tmp/test");
        inst.source_profile = "test".to_string();
        let id = inst.id.clone();
        view.add_instance(inst);
        id
    }

    fn write_global_default_attach_mode(mode: NewSessionAttachMode) {
        let mut config = Config::default();
        config.session.default_attach_mode = mode;
        save_config(&config).unwrap();
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
        assert_eq!(mode, Some(NewSessionAttachMode::Tmux));
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
        write_global_default_attach_mode(NewSessionAttachMode::LiveSend);
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
        write_global_default_attach_mode(NewSessionAttachMode::LiveSend);
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
        write_global_default_attach_mode(NewSessionAttachMode::LiveSend);
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
        write_global_default_attach_mode(NewSessionAttachMode::LiveSend);
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
        write_global_default_attach_mode(NewSessionAttachMode::LiveSend);
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
            NewSessionAttachMode::Tmux,
            "cache should initialize to the historical Tmux default"
        );
        write_global_default_attach_mode(NewSessionAttachMode::LiveSend);
        env.view.refresh_from_config();
        assert_eq!(
            env.view.profile_default_attach_mode,
            NewSessionAttachMode::LiveSend,
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
        write_global_default_attach_mode(NewSessionAttachMode::LiveSend);
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
}

mod save_field_merge {
    use super::*;
    use chrono::Utc;

    fn boot_view_with_one_session(title: &str, path: &str) -> (TempDir, HomeView, String) {
        let temp = TempDir::new().unwrap();
        setup_test_home(&temp);
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
        (temp, view, id)
    }

    #[test]
    #[serial]
    fn test_save_preserves_peer_field_update() {
        let (_temp, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

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
        let (_temp, mut view, _id) = boot_view_with_one_session("a", "/tmp/a");

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
        let (_temp, mut view, id) = boot_view_with_one_session("victim", "/tmp/victim");

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
        let (_temp, mut view, id) = boot_view_with_one_session("victim", "/tmp/victim");

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
        let (_temp, mut view, _id) = boot_view_with_one_session("a", "/tmp/a");

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
        let (_temp, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

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
        let (_temp, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

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
        let (_temp, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

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
        let (_temp, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

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
        let (_temp, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

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
        let (_temp, mut view, id) = boot_view_with_one_session("victim", "/tmp/peer-rm");

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
            !view.instances().iter().any(|i| i.id == id),
            "peer-deleted row must be dropped from in-memory instances"
        );
        assert!(
            view.get_instance(&id).is_none(),
            "peer-deleted row must be dropped from instance_map"
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
        let (_temp, mut view, _) = boot_view_with_one_session("seed", "/tmp/seed");

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
        let (_temp, mut view, _) = boot_view_with_one_session("seed", "/tmp/seed");

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
        let (_temp, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");
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
        let (_temp, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");
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
        let (_temp, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");

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
        let (_temp, mut view, id) = boot_view_with_one_session("session", "/tmp/sid");

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

        let (_temp, mut view, id) = boot_view_with_one_session("session", "/tmp/grp");

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
        let (_temp, mut view, id) = boot_view_with_one_session("session", "/tmp/grp");

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
        assert_eq!(menu.selected_action(), ContextMenuAction::Rename);
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
        // First item is Rename; Enter submits it.
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
        setup_inner(&mut env);
        env.view.handle_right_click(5, 1);
        // Session menu is Rename / Archive / Delete; Delete is two Downs away.
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

    /// Right-click a session, pick the Archive item (Rename -> Archive is one
    /// Down), and the row gets archived through the same `z` codepath. No
    /// follow-up dialog: archiving is immediate.
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
        let row = env.view.list_inner_area.y + idx as u16;
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
        assert_eq!(labels, vec!["Rename", "Unarchive", "Delete"]);

        env.view.handle_key(key(KeyCode::Down), None); // Rename -> Unarchive
        env.view.handle_key(key(KeyCode::Enter), None);
        assert!(
            !env.view.get_instance(&id).unwrap().is_archived(),
            "context-menu Unarchive must unarchive the session"
        );
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
    fn session_menu_n_hotkey_is_inert() {
        // Sanity: the session-row menu only has Rename/Delete actions,
        // so 'n' must NOT submit NewSession when the wrong menu is open.
        // This proves the hotkey gate (action must be in items) holds.
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 1); // row 1 = first session
        send_key(&mut env, crossterm::event::KeyCode::Char('n'));
        assert!(env.view.context_menu.is_some(), "menu should stay open");
        assert!(
            env.view.new_dialog.is_none(),
            "n on session menu must not open new-session"
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
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    const NEW_SID: &str = "019342ab-1111-7aaa-8bbb-cccdddeeefff";

    struct TmuxSession(String);

    impl TmuxSession {
        fn create(id: &str, title: &str) -> Self {
            let name = crate::tmux::Session::generate_name(id, title);
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &name])
                .output();
            let status = Command::new("tmux")
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
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.0])
                .output();
            crate::tmux::refresh_session_cache();
        }
    }

    fn skip_if_no_tmux() -> bool {
        if Command::new("tmux").arg("-V").output().is_err() {
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
        for i in &mut view.instances {
            if i.id == instance_id {
                i.session_id_poller = Some(arc.clone());
            }
        }
        if let Some(i) = view.instance_map.get_mut(instance_id) {
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
        setup_test_home(&temp);

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
    fn apply_session_id_updates_skips_retroactive_excludes() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        setup_test_home(&temp);

        let profile = "apply-excludes";
        let inst = fresh_instance(profile, "aer");
        let mut view = build_view_with_inst(profile, &inst);
        for i in &mut view.instances {
            if i.id == inst.id {
                i.retroactive_capture_excludes.insert(NEW_SID.to_string());
            }
        }
        if let Some(i) = view.instance_map.get_mut(&inst.id) {
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
            .iter()
            .find(|i| i.id == inst.id)
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
        setup_test_home(&temp);

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
            .iter()
            .find(|i| i.id == inst.id)
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
        setup_test_home(&temp);

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
        setup_test_home(&temp);

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
            .iter()
            .find(|i| i.id == inst.id)
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
                view.render(f, area, &theme, None, None);
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
