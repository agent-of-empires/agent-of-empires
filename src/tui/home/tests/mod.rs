//! Tests for HomeView

use super::watchers::ConfigWatchKey;
use super::{ConfigRefreshOrigin, HomeView, PreviewSelection, ViewMode};
use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
use crate::session::{
    Group, GroupTree, Instance, Item, LifecycleOperation, LifecycleReservation, Status, Storage,
};
use crate::tmux::AvailableTools;
use crate::tui::app::Action;
use crate::tui::dialogs::{InfoDialog, NewSessionData, NewSessionDialog};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;
use tempfile::TempDir;
use tui_input::Input;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

mod apply_session_id_updates;
mod archive_restart_grouping;
mod click_to_select;
mod daemon_status_apply_tests;
mod default_attach_mode;
mod divider_drag;
mod footer_toolbar;
mod fork_rename_dialogs;
mod keys_and_nav;
mod live_send_boot_size_tests;
mod live_send_mode;
mod permission_response_dialog;
mod pickers_groups_sort;
mod post_create_attach_mode;
mod preview_drag_select;
mod profile_duplicate_reconciliation;
mod render_and_save;
mod right_click_context_menu;
mod save_field_merge;
mod scroll_pane_isolation;
mod search;
mod settings_scroll_wiring;
mod stacked_single_seam;
mod status_rows_menu;
mod store_move;

fn setup_test_home(temp: &TempDir) -> AppDirGuard {
    isolate_app_dir_at(temp.path())
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

/// Turn off `session.confirm_delete` so `d` trashes on the keystroke instead
/// of opening the confirmation dialog. Must run after `setup_test_home` so it
/// writes into the test HOME. See #2583, #3364.
fn disable_confirm_delete() {
    crate::session::config::update_config(|config| {
        config.session.confirm_delete = false;
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

/// Shared fixture for the async-creation finalization tests: a fresh
/// single-commit git repo under a temp `$HOME`, a `HomeView` bound to the
/// `default` profile in manual-group mode, and an unwatched `Storage` handle
/// onto the same profile. Each test spawns a real background builder against
/// this repo, so the setup is factored out rather than duplicated.
struct CreationTestEnv {
    view: HomeView,
    storage: Storage,
    project_dir: std::path::PathBuf,
    _guard: AppDirGuard,
    _temp: TempDir,
}

fn setup_creation_test_env() -> CreationTestEnv {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let project_dir = temp.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    {
        let repo = git2::Repository::init(&project_dir).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        std::fs::write(project_dir.join("README.md"), "test\n").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("README.md")).unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();
    }

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

    let storage = Storage::new_unwatched("default").unwrap();
    CreationTestEnv {
        view,
        storage,
        project_dir,
        _guard,
        _temp: temp,
    }
}

/// Base new-session data targeting the shared repo. Callers tweak the
/// title/group and worktree fields per scenario.
fn creation_data(project_dir: &std::path::Path, title: &str, group: &str) -> NewSessionData {
    NewSessionData {
        profile: "default".to_string(),
        title: title.to_string(),
        path: project_dir.to_str().unwrap().to_string(),
        group: group.to_string(),
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

/// Pump `apply_creation_results` until the background builder delivers a
/// result, returning the finalized session id (`Some`) or the rollback outcome
/// (`None`). Consuming the result clears `is_creation_pending`, so this
/// terminates once a result lands; it fails the test on timeout rather than
/// looping forever. Centralizes the poll so the tests carry no bespoke timing
/// loops of their own.
fn drain_creation_result(view: &mut HomeView) -> Option<String> {
    let start = std::time::Instant::now();
    loop {
        if let Some(id) = view.apply_creation_results() {
            return Some(id);
        }
        if !view.is_creation_pending() {
            return None;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "background creation timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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

/// Build a HomeView seeded with three sessions: two live in real git repos
/// with distinct hosted `origin` remotes on different hosts entirely
/// (GitHub, GitLab) to prove owner resolution isn't GitHub-specific, and one
/// live in a real git repo with no `origin` remote at all. Helper for
/// `build_flat_items_by_org` grouping tests, which (unlike project mode)
/// need an actual `.git` directory since `get_remote_owner` reads the
/// on-disk remote configuration rather than parsing the path string.
fn create_test_env_two_orgs() -> TestEnv {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let repo_a = temp.path().join("repo-a");
    std::fs::create_dir_all(&repo_a).unwrap();
    git2::Repository::init(&repo_a)
        .unwrap()
        .remote("origin", "git@github.com:org-a/repo-a.git")
        .unwrap();

    let repo_b = temp.path().join("repo-b");
    std::fs::create_dir_all(&repo_b).unwrap();
    git2::Repository::init(&repo_b)
        .unwrap()
        .remote("origin", "git@gitlab.com:org-b/repo-b.git")
        .unwrap();

    let repo_no_remote = temp.path().join("repo-no-remote");
    std::fs::create_dir_all(&repo_no_remote).unwrap();
    git2::Repository::init(&repo_no_remote).unwrap();

    let inst_a = Instance::new("a-session", repo_a.to_str().unwrap());
    let inst_b = Instance::new("b-session", repo_b.to_str().unwrap());
    let inst_no_remote = Instance::new("no-remote-session", repo_no_remote.to_str().unwrap());

    let instances = vec![inst_a, inst_b, inst_no_remote];
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

/// Build a HomeView seeded with two sessions whose repos share the same
/// owner login ("acme") but live on different hosts (GitHub, GitLab).
/// Regression fixture for the Required #1 review fix: before it,
/// `org_group_key` returned the bare owner, so these two repos merged into
/// one org bucket and one bulk-archive scope despite having nothing to do
/// with each other.
fn create_test_env_same_owner_two_hosts() -> TestEnv {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let repo_gh = temp.path().join("repo-gh");
    std::fs::create_dir_all(&repo_gh).unwrap();
    git2::Repository::init(&repo_gh)
        .unwrap()
        .remote("origin", "git@github.com:acme/repo-gh.git")
        .unwrap();

    let repo_gl = temp.path().join("repo-gl");
    std::fs::create_dir_all(&repo_gl).unwrap();
    git2::Repository::init(&repo_gl)
        .unwrap()
        .remote("origin", "git@gitlab.com:acme/repo-gl.git")
        .unwrap();

    let inst_gh = Instance::new("gh-session", repo_gh.to_str().unwrap());
    let inst_gl = Instance::new("gl-session", repo_gl.to_str().unwrap());

    let instances = vec![inst_gh, inst_gl];
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
