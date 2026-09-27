//! The local-write gate: once a daemon snapshot has been applied, the runtime
//! owns the session and group rows, so this process must not write them
//! straight to disk behind the runtime's back.

use super::super::GroupRenameContext;
use super::{setup_test_home, HomeView};
use crate::session::{GroupTree, Instance, Storage};
use crate::tmux::AvailableTools;
use crate::tui::dialogs::InfoDialog;
use crossterm::event::{KeyCode, KeyEvent};
use serial_test::serial;
use tempfile::TempDir;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

/// A view over one profile holding the group under test, with the runtime
/// already attached: `set_sidebar_source(Daemon)` is what a feed's first
/// snapshot does.
fn view_with_attached_runtime() -> HomeView {
    let storage = Storage::new_unwatched("test").unwrap();
    let instances = [Instance::new("alpha", "/tmp/work")];
    let mut tree = GroupTree::new_with_groups(&instances, &[]);
    tree.create_group("work");
    let groups = tree.get_all_groups();
    assert!(
        groups.iter().any(|group| group.path == "work"),
        "the fixture must start with the group under test"
    );
    storage
        .update(|disk_instances, disk_groups| {
            *disk_instances = instances.to_vec();
            *disk_groups = groups;
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new_for_test(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    // `new_for_test` labels the source Daemon without going through the
    // transition, so drive the transition a real connection drives.
    view.set_sidebar_source(
        crate::tui::session_feed::SidebarSource::Disconnected,
        Some("fixture"),
    );
    view.set_sidebar_source(crate::tui::session_feed::SidebarSource::Daemon, None);
    view
}

fn disk_group_paths() -> Vec<String> {
    Storage::new_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap()
        .1
        .into_iter()
        .map(|group| group.path)
        .collect()
}

/// Rename the selected group through the public entry point, the way a
/// confirmed rename dialog does.
fn rename_selected_group_to(view: &mut HomeView, new_path: &str) {
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.selected_group = Some("work".to_string());
    view.group_rename_context = Some(GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "test".to_string(),
    });
    view.rename_selected_group(Some(new_path), None).unwrap();
}

fn info_title(view: &HomeView) -> Option<&str> {
    view.info_dialog.as_ref().map(InfoDialog::title)
}

/// A runtime that does not own mutations must leave `groups.json` untouched
/// rather than diverge from the canonical snapshot it published.
#[test]
#[serial]
fn read_only_runtime_refuses_a_local_group_write() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let mut view = view_with_attached_runtime();
    let before = disk_group_paths();

    rename_selected_group_to(&mut view, "renamed");

    assert_eq!(disk_group_paths(), before, "disk was rewritten");
    assert!(
        info_title(&view).is_some(),
        "the refusal must be visible to the operator"
    );
}

/// A CityHall client keeps its structured sessions mutable, but its policy does
/// not extend to this process writing group data on its behalf.
#[test]
#[serial]
fn cityhall_runtime_refuses_a_local_group_write() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let mut view = view_with_attached_runtime();
    let _driver = view.session_feed.command_driver_for_test();
    view.session_feed.set_cityhall_for_test(true);
    let before = disk_group_paths();

    rename_selected_group_to(&mut view, "renamed");

    // The runtime itself still grants mutations: the refusal comes from the
    // policy this process must honor, not from a degraded runtime.
    assert!(view.session_feed.mutations_available());
    assert_eq!(disk_group_paths(), before, "disk was rewritten");
    assert_eq!(info_title(&view), Some("Read-only"));
}

/// A runtime that goes away still owns the rows it published: the mirror must
/// not become a second writer just because the connection dropped.
#[test]
#[serial]
fn lost_runtime_refuses_the_mirror_save() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let mut view = view_with_attached_runtime();
    {
        let _driver = view.session_feed.command_driver_for_test();
        assert!(view.session_feed.mutations_available());
    }
    assert!(!view.session_feed.mutations_available());
    let before = disk_group_paths();

    rename_selected_group_to(&mut view, "renamed");
    view.save().unwrap();

    assert_eq!(disk_group_paths(), before, "disk was rewritten");
}

/// The gate must not fire for the local process that owns the files: a view
/// that never received a runtime snapshot writes them itself.
#[test]
#[serial]
fn local_owner_still_writes() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let mut view = HomeView::new_for_test(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    let storage = Storage::new_unwatched("test").unwrap();
    storage
        .update(|_, disk_groups| {
            disk_groups.push(crate::session::Group::new("work", "work"));
            Ok(())
        })
        .unwrap();
    view.reload().unwrap();

    rename_selected_group_to(&mut view, "renamed");
    view.save().unwrap();

    let after = disk_group_paths();
    assert!(
        after.contains(&"renamed".to_string()) && !after.contains(&"work".to_string()),
        "{after:?}"
    );
    assert!(view.info_dialog.is_none());
}

/// With a healthy runtime attached, the same rename goes through: the gate
/// costs nothing when the runtime does allow local writes.
#[test]
#[serial]
fn healthy_runtime_still_writes() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let mut view = view_with_attached_runtime();
    let _driver = view.session_feed.command_driver_for_test();

    rename_selected_group_to(&mut view, "renamed");
    view.save().unwrap();

    let after = disk_group_paths();
    assert!(
        after.contains(&"renamed".to_string()) && !after.contains(&"work".to_string()),
        "{after:?}"
    );
    assert!(view.info_dialog.is_none());
}

/// Profile creation is not in the runtime's CityHall mutation policy, so the
/// TUI must not perform it on disk for such a client.
#[test]
#[serial]
fn cityhall_refuses_profile_creation_on_disk() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let mut view = view_with_attached_runtime();
    let _driver = view.session_feed.command_driver_for_test();
    view.session_feed.set_cityhall_for_test(true);
    view.show_profile_picker();

    view.handle_key(key(KeyCode::Char('n')), None);
    for character in "cityhall-target".chars() {
        view.handle_key(key(KeyCode::Char(character)), None);
    }
    view.handle_key(key(KeyCode::Enter), None);

    assert_eq!(
        info_title(&view),
        Some("Read-only"),
        "a refused creation is a policy refusal, not a filesystem error"
    );
    assert!(view.profile_picker_dialog.is_none());
    assert!(!crate::session::list_profiles()
        .unwrap()
        .contains(&"cityhall-target".to_string()));
    assert!(!crate::session::get_app_dir()
        .unwrap()
        .join("profiles")
        .join("cityhall-target")
        .exists());
}
