//! E2E coverage for the native in-TUI skills manager: the command palette
//! opens it, a host-discovered skill shows its agent source label, editing or
//! deleting a host row is refused until it is adopted into AoE's managed
//! store, and a managed row can then be deleted behind a confirmation.
use std::time::Duration;

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

/// Open the skills manager through the palette, mirroring
/// `plugins.rs::open_manager`.
fn open_manager(h: &TuiTestHarness) {
    h.wait_for(" aoe ");
    h.send_keys("C-k");
    h.wait_for("Commands");
    h.type_text("skills");
    h.wait_for("Manage skills");
    h.send_keys("Enter");
    h.wait_for(" Skills ");
}

/// The full lifecycle a skill goes through in the manager: discovered as a
/// read-only host row, refused for edit/delete until adopted, then deletable
/// as a managed row.
#[test]
#[parallel]
fn test_tui_skills_manager_adopt_and_delete_lifecycle() {
    require_tmux!();

    let mut h = TuiTestHarness::new("skills_manager_lifecycle");

    let skill_dir = h.home_path().join(".claude/skills/review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: review\ndescription: Code review helper\n---\n\n# review\n\nbody\n",
    )
    .unwrap();

    h.spawn_tui();
    open_manager(&h);

    // The host skill shows up with its agent's source label.
    h.assert_screen_contains("review");
    h.assert_screen_contains("Claude");

    // Editing a host row is refused: it must be adopted first.
    h.send_keys("e");
    h.wait_for("adopt it into AoE first");

    // Adopting copies it into the managed store; the new row shows the AoE
    // source label alongside the untouched original. Waiting on the adopt
    // confirmation rather than on "AoE" matters: the refusal above still says
    // "adopt it into AoE first", so a bare "AoE" would match the stale line.
    h.send_keys("a");
    h.wait_for("Adopted review");
    h.assert_screen_contains("AoE");

    // The managed (AoE) row sorts first, so the cursor already sits on it;
    // deleting goes behind a confirmation popup.
    h.send_keys("x");
    h.wait_for(" Delete skill ");
    h.assert_screen_contains("y delete");
    h.send_keys("y");
    h.wait_for("Deleted review");
    h.wait_for_absent("AoE", Duration::from_secs(5));

    // The original host skill is untouched by the delete.
    h.assert_screen_contains("Claude");
}
