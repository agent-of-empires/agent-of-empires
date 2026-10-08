//! Tests for HomeView

use super::watchers::ConfigWatchKey;
use super::{ConfigRefreshOrigin, HomeView, PreviewSelection, ViewMode};
use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
use crate::session::{
    Group, GroupTree, Instance, Item, LifecycleOperation, LifecycleReservation, Status, Storage,
};
use crate::tmux::AvailableTools;
use crate::tui::app::Action;
use crate::tui::dialogs::{InfoDialog, NewSessionData};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;
use tempfile::TempDir;
use tui_input::Input;

fn remove_test_instance(view: &mut HomeView, id: &str) {
    if let Some(instance) = view.instances.shift_remove(id) {
        if let Some(pending) = view.pending_added.get_mut(&instance.source_profile) {
            pending.remove(id);
        }
        view.pending_deletions
            .entry(instance.source_profile)
            .or_default()
            .insert(id.to_owned());
    }
}

fn observed_fork_parent(agent: &str) -> Instance {
    let mut instance = Instance::new("parent", "/tmp/repo");
    instance.source_profile = "test".into();
    instance.tool = agent.into();
    let sid = "parent-1111-2222-3333-444444444444";
    instance.set_agent_conversation(
        Some(sid.into()),
        Some(crate::session::ConversationBinding {
            session_id: sid.into(),
            execution: Some(crate::session::ExecutionBinding {
                agent: agent.into(),
                stores: vec!["/native-store".into()],
                configuration: Vec::new(),
                cwd: "/tmp/repo".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: crate::session::ConversationProvenance::Observed,
            transcript_path: None,
        }),
        None,
    );
    instance
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

mod apply_session_id_updates;
mod archive_restart_grouping;
mod click_to_select;
mod default_attach_mode;
mod dialog_mouse;
mod divider_drag;
mod footer_toolbar;
mod fork_rename_dialogs;
mod hide_stopped;
mod keys_and_nav;
mod live_send_boot_size_tests;
mod live_send_mode;
mod permission_response_dialog;
mod pickers_groups_sort;
mod post_create_attach_mode;
mod preview_drag_select;
mod preview_links;
mod profile_duplicate_reconciliation;
mod render_and_save;
mod right_click_context_menu;
mod save_field_merge;
mod scroll_pane_isolation;
mod search;
mod session_feed_tests;
mod settings_scroll_wiring;
mod sidebar_position;
mod stacked_single_seam;
mod status_rows_menu;
mod store_move;

fn setup_test_home(temp: &TempDir) -> AppDirGuard {
    isolate_app_dir_at(temp.path())
}

struct TestEnv {
    view: HomeView,
    native_input: Option<NativePreviewInput>,
    _guard: AppDirGuard,
    _temp: TempDir,
}

struct NativePreviewInput {
    _guard: crate::tmux::test_helpers::TmuxTestSession,
    input: std::path::PathBuf,
    actor: std::sync::Arc<crate::tmux::Session>,
    effects: std::sync::Arc<std::sync::Mutex<()>>,
    receipt_offset: std::cell::Cell<usize>,
    observed: std::cell::RefCell<Vec<u8>>,
}

fn wait_for_native_fixture<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn finish_runner_settlements(view: &mut HomeView) {
    wait_for_native_fixture("runner settlement callbacks", || {
        view.apply_settlement_results();
        view.settlement_in_flight.is_empty().then_some(())
    });
}
/// State-only fixtures use this without claiming a prepared input transport.
fn live_state_for_instance(
    inst: &Instance,
    target: super::live_send::LiveSendTarget,
) -> super::live_send::LiveSendState {
    use super::live_send::{parse_chord_list, LiveSendState, LiveSendTarget, DEFAULT_EXIT_CHORD};
    let tmux_name = match &target {
        LiveSendTarget::Agent => crate::tmux::Session::resolve_name(&inst.id, &inst.title),
        LiveSendTarget::Terminal => {
            crate::tmux::TerminalSession::resolve_name(&inst.id, &inst.title)
        }
        LiveSendTarget::ContainerTerminal => {
            crate::tmux::ContainerTerminalSession::resolve_name(&inst.id, &inst.title)
        }
        LiveSendTarget::Tool(name) => crate::tmux::ToolSession::new(&inst.id, &inst.title, name)
            .session_name()
            .to_owned(),
    };
    LiveSendState {
        session_id: inst.id.clone(),
        title: inst.title.clone(),
        tmux_name,
        target,
        exit_chords: parse_chord_list(DEFAULT_EXIT_CHORD),
        leader: None,
    }
}

impl TestEnv {
    /// One raw native receiver, shared by the capture actor and live/passive admission.
    fn install_native_preview_input(
        &mut self,
        id: &str,
        target: super::live_send::LiveSendTarget,
        live: bool,
    ) {
        use super::live_send::LiveSendWorker;
        assert!(
            self.native_input.is_none(),
            "native receiver already installed"
        );
        let inst = self
            .view
            .get_instance(id)
            .expect("native fixture instance")
            .clone();
        let state = live_state_for_instance(&inst, target);
        let guard = crate::tmux::test_helpers::TmuxTestSession::from_name(state.tmux_name.clone());
        let input = self._temp.path().join("native-preview-input");
        let receiver = format!(
            "stty raw -echo; printf 'NATIVE_INPUT_READY'; cat > {}",
            crate::session::environment::shell_escape_script_word(&input.to_string_lossy()),
        );
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                guard.name(),
                "-x",
                "80",
                "-y",
                "24",
                &receiver,
            ])
            .output()
            .expect("create native preview receiver");
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        crate::tmux::refresh_session_cache();
        let primary = crate::tmux::utils::resolve_primary(
            guard.name(),
            &crate::tmux::TmuxCommandDeadline::new(),
        )
        .expect("bind native preview actor");
        let session =
            std::sync::Arc::new(crate::tmux::Session::with_primary(guard.name(), primary));
        self.view.selected_session = Some(id.to_owned());
        self.view.live_send = live.then_some(state);
        self.view.vt_live_enabled = false;
        self.view.agent_clipboard_forward = false;
        self.view
            .sync_preview_capture_worker(Some(session.name().to_owned()));
        let capture = self
            .view
            .preview_capture_worker
            .as_ref()
            .expect("capture worker");
        if live {
            let admission = capture.begin_live_input(session.clone());
            self.view.live_send_worker =
                Some(LiveSendWorker::spawn(admission, Some(capture.waker())));
        }
        capture.set_capture_lines(40);
        let frame = wait_for_native_fixture("current native preview frame", || {
            capture.take_latest().filter(|frame| {
                capture.frame_is_current(frame)
                    && frame.content.contains("NATIVE_INPUT_READY")
                    && frame
                        .session
                        .as_ref()
                        .is_some_and(|actor| actor.captured_primary() == session.captured_primary())
            })
        });
        self.view
            .preview_cache
            .store_capture(frame, id.to_owned(), (80, 24));
        self.native_input = Some(NativePreviewInput {
            _guard: guard,
            input,
            actor: session,
            effects: capture.effects_for_test(),
            receipt_offset: std::cell::Cell::new(0),
            observed: std::cell::RefCell::new(Vec::new()),
        });
        wait_for_native_fixture("raw receiver ready", || {
            self.native_input
                .as_ref()
                .unwrap()
                .input
                .exists()
                .then_some(())
        });
    }

    fn assert_native_input(&self, expected: &[u8]) {
        use super::live_send::{oneshot_idle_for_test, TmuxKey};
        let native = self
            .native_input
            .as_ref()
            .expect("native receiver installed");
        wait_for_native_fixture("passive dispatch completion", || {
            oneshot_idle_for_test().then_some(())
        });
        let marker = format!("\x1b[4107;{}~", native.receipt_offset.get());
        if let Some(worker) = self.view.live_send_worker.as_ref() {
            assert!(worker.send(TmuxKey::HexBytes(marker.as_bytes().to_vec())));
        } else {
            // Cancelled live work and completed passive work precede this native receipt.
            let _fence = native.effects.lock().unwrap();
            let deadline = crate::tmux::TmuxCommandDeadline::new();
            let mut send = native
                .actor
                .commands_with_deadline(
                    [[
                        "send-keys",
                        "-t",
                        native.actor.captured_primary().pane_id.as_str(),
                        "-l",
                        marker.as_str(),
                    ]],
                    &deadline,
                )
                .expect("fenced native receipt");
            assert!(deadline
                .run(&mut send)
                .expect("send native receipt")
                .status
                .success());
        }
        let bytes = wait_for_native_fixture("ordered native input receipt", || {
            std::fs::read(&native.input)
                .ok()
                .filter(|bytes| bytes.ends_with(marker.as_bytes()))
        });
        let mut observed = native.observed.borrow_mut();
        observed.extend_from_slice(&bytes[native.receipt_offset.get()..bytes.len() - marker.len()]);
        native.receipt_offset.set(bytes.len());
        assert_eq!(
            observed.as_slice(),
            expected,
            "complete input before its ordered receipt"
        );
    }
}

/// An isolated app dir for a fixture; the guard must outlive every storage write.
fn test_home() -> (TempDir, AppDirGuard) {
    let temp = TempDir::new().unwrap();
    let guard = setup_test_home(&temp);
    (temp, guard)
}

fn test_view(profile: Option<&str>) -> HomeView {
    HomeView::new_for_test(
        profile.map(str::to_string),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap()
}

/// Persist `instances` (with derived groups) to `profile`.
fn seed_profile(profile: &str, instances: &[Instance]) {
    Storage::new_unwatched(profile)
        .unwrap()
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();
}

/// A view over profile "test" seeded with `instances`. `manual` switches to manual
/// grouping and rebuilds the rows, which most fixtures want.
fn seeded_env(
    (temp, guard): (TempDir, AppDirGuard),
    instances: &[Instance],
    manual: bool,
) -> TestEnv {
    seed_profile("test", instances);
    let mut view = test_view(Some("test"));
    if manual {
        view.group_by = crate::session::config::GroupByMode::Manual;
        view.flat_items = view.build_flat_items();
        view.update_selected();
    }
    TestEnv {
        view,
        native_input: None,
        _guard: guard,
        _temp: temp,
    }
}

/// Session id of the `flat_items` row at `idx`, or `None` when it is not a session row.
fn session_id_at(view: &HomeView, idx: usize) -> Option<String> {
    match view.flat_items.get(idx) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    }
}

/// Session id under the cursor, or `None` when the cursor is not on a session row.
fn cursor_session_id(view: &HomeView) -> Option<String> {
    session_id_at(view, view.cursor)
}

/// A `StatusUpdate` carrying the three fields the apply-path tests vary; everything else
/// takes the "producer had nothing to say" value.
fn status_update(
    id: &str,
    status: Status,
    idle_entered_at: crate::tui::status_poller::IdleIntent,
) -> crate::tui::status_poller::StatusUpdate {
    crate::tui::status_poller::StatusUpdate {
        id: id.to_string(),
        status,
        last_error: None,
        idle_entered_at,
        last_accessed_at: None,
        pane_dead: false,
        live_status_baseline: None,
        detection: None,
    }
}

fn instance_in(title: &str, path: &str, group: &str) -> Instance {
    let mut inst = Instance::new(title, path);
    inst.group_path = group.to_string();
    inst
}

fn instance_with_status(title: &str, path: &str, status: Status) -> Instance {
    let mut inst = Instance::new(title, path);
    inst.status = status;
    inst
}

fn create_test_env_empty() -> TestEnv {
    seeded_env(test_home(), &[], true)
}

fn create_test_env_with_sessions(count: usize) -> TestEnv {
    let instances: Vec<_> = (0..count)
        .map(|i| Instance::new(&format!("session{i}"), &format!("/tmp/{i}")))
        .collect();
    seeded_env(test_home(), &instances, true)
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn config_watch_keys_distinguish_global_from_profile_named_global() {
    let (_temp, _guard) = test_home();
    let profile_name = "<global>";
    // Outside the create grammar, so lay the legacy directory down directly.
    let profile_dir = crate::session::get_app_dir()
        .unwrap()
        .join("profiles")
        .join(profile_name);
    std::fs::create_dir_all(&profile_dir).unwrap();
    let _storage = Storage::open_unwatched(profile_name).unwrap();
    let view = HomeView::new_for_test(
        Some(profile_name.to_string()),
        AvailableTools::with_tools(&["claude"]),
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

/// Render once off-screen so geometry fields (`list_inner_area`, `shelf_inner_area`) are real.
fn render_geometry(view: &mut HomeView) {
    render_home_to_string(view, 120, 40);
}

/// Screen row of the shelf item at `flat_items` index `idx`, assuming an unscrolled shelf.
fn shelf_row_for_idx(view: &HomeView, idx: usize) -> u16 {
    let list_len = view.shelf_start().expect("a shelf must be present");
    assert!(idx >= list_len, "idx {idx} is in the list, not the shelf");
    view.shelf_inner_area.y + (idx - list_len) as u16
}

/// With trash-first delete on, `d` trashes instead of opening the delete dialog.
fn disable_delete_to_trash() {
    crate::session::config::update_config(|config| {
        config.session.delete_to_trash = false;
    })
    .unwrap();
}

/// Makes `d` trash on the keystroke instead of opening the confirmation dialog.
fn disable_confirm_delete() {
    crate::session::config::update_config(|config| {
        config.session.confirm_delete = false;
    })
    .unwrap();
}

fn create_test_env_with_groups() -> TestEnv {
    let instances = [
        Instance::new("ungrouped", "/tmp/u"),
        instance_in("work-project", "/tmp/work", "work"),
        instance_in("personal-project", "/tmp/personal", "personal"),
    ];
    seeded_env(test_home(), &instances, true)
}

fn create_test_env_with_mixed_sessions() -> TestEnv {
    let instances = [
        Instance::new("Uncategorized", "/tmp/u"),
        instance_in("Zebra", "/tmp/z", "work"),
        instance_in("Mango", "/tmp/m", "work"),
        instance_in("Apple", "/tmp/a", "work"),
    ];
    seeded_env(test_home(), &instances, true)
}

/// The only catalog tip is earned: cross its threshold on disk and refresh the badge.
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

fn create_test_env_with_group_sessions() -> TestEnv {
    let mut sandboxed = instance_in("work-session-2", "/tmp/work2", "work");
    sandboxed.sandbox_info = Some(crate::session::SandboxInfo {
        provider: None,
        enabled: true,
        container_id: None,
        image: "ubuntu:latest".to_string(),
        container_name: "test-container".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: None,
    });
    let instances = [
        Instance::new("ungrouped", "/tmp/u"),
        instance_in("work-session-1", "/tmp/work1", "work"),
        sandboxed,
        instance_in("work-nested", "/tmp/work/nested", "work/projects"),
    ];
    seeded_env(test_home(), &instances, true)
}

/// Attention-sorted view of a Running session and one with `other` status, plus both row indices.
fn attention_env_running_then(other: Status) -> (TestEnv, usize, usize) {
    use crate::session::config::SortOrder;
    let instances = [
        instance_with_status("running", "/tmp/running", Status::Running),
        instance_with_status("other", "/tmp/other", other),
    ];
    let mut env = seeded_env(test_home(), &instances, false);
    env.view.strict_hotkeys = false;
    env.view.group_by = crate::session::config::GroupByMode::Manual;
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    env.view.update_selected();

    let row_with = |status: Status| {
        env.view
            .flat_items
            .iter()
            .position(|item| match item {
                Item::Session { id, .. } => {
                    env.view.get_instance(id).map(|i| i.status) == Some(status)
                }
                _ => false,
            })
            .expect("a session row with the requested status")
    };
    let (running, other) = (row_with(Status::Running), row_with(other));
    (env, running, other)
}

fn attention_env_running_then_waiting() -> (TestEnv, usize, usize) {
    attention_env_running_then(Status::Waiting)
}

fn attention_env_running_then_idle() -> (TestEnv, usize, usize) {
    attention_env_running_then(Status::Idle)
}

/// Flatten a rendered row into its plain text, dropping styling.
fn rendered_row_text(view: &HomeView, item: &Item) -> String {
    let theme = crate::tui::styles::Theme::default();
    view.render_item_line(item, false, false, &theme, 200, view.favorite_gutter())
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

fn rendered_single_session_text(
    inst: Instance,
    row_tag_mode: crate::session::config::RowTagMode,
) -> String {
    let (_temp, _guard) = test_home();
    seed_profile("alpha", &[inst]);
    let mut view = test_view(None);
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = row_tag_mode;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    view.flat_items
        .iter()
        .find(|item| matches!(item, Item::Session { .. }))
        .map(|item| rendered_row_text(&view, item))
        .expect("session row should render")
}

/// Fixture for async-creation finalization: a single-commit git repo and a `default` profile view.
struct CreationTestEnv {
    view: HomeView,
    storage: Storage,
    project_dir: std::path::PathBuf,
    _guard: AppDirGuard,
    _temp: TempDir,
}

fn setup_creation_test_env() -> CreationTestEnv {
    let (temp, guard) = test_home();
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

    let storage = Storage::new_unwatched("default").unwrap();
    let mut view = test_view(Some("default"));
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    CreationTestEnv {
        view,
        storage,
        project_dir,
        _guard: guard,
        _temp: temp,
    }
}

fn creation_data(project_dir: &std::path::Path, title: &str, group: &str) -> NewSessionData {
    NewSessionData {
        profile: "default".to_string(),
        title: title.to_string(),
        title_typed: false,
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

/// Pump `apply_creation_results` until the builder delivers: `Some(id)` on success, `None` on rollback.
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

/// Render the full home view into a TestBackend and dump the screen as one string.
fn render_home_to_string(view: &mut HomeView, width: u16, height: u16) -> String {
    let theme = crate::tui::styles::load_theme("empire");
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let mut screen = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            screen.push_str(buf[(x, y)].symbol());
        }
        screen.push('\n');
    }
    screen
}

/// Two projects, each with sessions of different attention statuses.
fn create_test_env_two_projects_mixed_attention() -> TestEnv {
    let instances = [
        instance_with_status("alpha-waiting", "/repos/alpha", Status::Waiting),
        instance_with_status("alpha-running", "/repos/alpha", Status::Running),
        instance_with_status("beta-running", "/repos/beta", Status::Running),
        instance_with_status("beta-error", "/repos/beta", Status::Error),
    ];
    seeded_env(test_home(), &instances, false)
}

/// Init a git repo at `temp/name`, with an `origin` remote when given.
fn git_repo(temp: &TempDir, name: &str, origin: Option<&str>) -> String {
    let dir = temp.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let repo = git2::Repository::init(&dir).unwrap();
    if let Some(url) = origin {
        repo.remote("origin", url).unwrap();
    }
    dir.to_str().unwrap().to_string()
}

/// Sessions in repos with GitHub and GitLab `origin`s plus one with no remote, for org grouping.
fn create_test_env_two_orgs() -> TestEnv {
    let home = test_home();
    let instances = [
        Instance::new(
            "a-session",
            &git_repo(&home.0, "repo-a", Some("git@github.com:org-a/repo-a.git")),
        ),
        Instance::new(
            "b-session",
            &git_repo(&home.0, "repo-b", Some("git@gitlab.com:org-b/repo-b.git")),
        ),
        Instance::new(
            "no-remote-session",
            &git_repo(&home.0, "repo-no-remote", None),
        ),
    ];
    seeded_env(home, &instances, false)
}

/// Same owner login on two hosts: org grouping must keep them apart.
fn create_test_env_same_owner_two_hosts() -> TestEnv {
    let home = test_home();
    let instances = [
        Instance::new(
            "gh-session",
            &git_repo(&home.0, "repo-gh", Some("git@github.com:acme/repo-gh.git")),
        ),
        Instance::new(
            "gl-session",
            &git_repo(&home.0, "repo-gl", Some("git@gitlab.com:acme/repo-gl.git")),
        ),
    ];
    seeded_env(home, &instances, false)
}

/// Live-send state targeting the agent pane with the default exit chord and no leader.
pub(super) fn live_send_state(
    session_id: &str,
    title: &str,
    tmux_name: &str,
) -> super::live_send::LiveSendState {
    super::live_send::LiveSendState {
        session_id: session_id.to_string(),
        title: title.to_string(),
        tmux_name: tmux_name.to_string(),
        target: super::live_send::LiveSendTarget::Agent,
        exit_chords: super::live_send::parse_chord_list(super::live_send::DEFAULT_EXIT_CHORD),
        leader: None,
    }
}
