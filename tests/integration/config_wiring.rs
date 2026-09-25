//! The delete dialog's cleanup checkboxes default from the saved
//! `worktree.auto_cleanup` and `sandbox.auto_cleanup` settings.

use agent_of_empires::session::update_config;
use agent_of_empires::tui::dialogs::{DeleteDialogConfig, UnifiedDeleteDialog};
use serial_test::serial;

use crate::common::setup_temp_home;

#[test]
#[serial]
fn delete_dialog_cleanup_defaults_follow_saved_config() {
    for auto_cleanup in [true, false] {
        let _temp = setup_temp_home();
        update_config(|config| {
            config.worktree.auto_cleanup = auto_cleanup;
            config.sandbox.auto_cleanup = auto_cleanup;
        })
        .unwrap();

        let dialog = UnifiedDeleteDialog::new(
            "Test".to_string(),
            DeleteDialogConfig {
                worktree_branch: Some("main".to_string()),
                has_sandbox: true,
                project_path: None,
                is_scratch: false,
            },
            "default",
        );
        assert_eq!(dialog.options().delete_worktree, auto_cleanup);
        assert_eq!(dialog.options().delete_sandbox, auto_cleanup);
    }
}
