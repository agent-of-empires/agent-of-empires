/// Live-send must boot a cold/dead agent pane at the visible preview size so it
/// does not start at tmux's 80x24 default and then depend on a single,
/// race-prone post-boot `resize-window` SIGWINCH to grow into the live area.
/// Regression guard for the "live mode opens at ~50% width" race: if this seed
/// regresses back to `None`/default, the agent boots narrow again.
use super::create_test_env_empty;
use ratatui::layout::Rect;

/// A drawn preview seeds the boot size; before any draw the empty rect must fall back
/// rather than hand tmux a degenerate 0x0 size.
#[test]
#[serial_test::serial]
fn boot_size_follows_visible_preview_and_never_seeds_zero() {
    let mut env = create_test_env_empty();
    // The visible rect the post-toast draw queues to LiveSendWorker.
    env.view.preview_pane_area = Rect::new(35, 1, 123, 38);
    assert_eq!(
        env.view.live_send_boot_size(),
        Some((123, 38)),
        "cold agent must boot at the preview pane's visible size, not tmux's 80x24 default"
    );

    // No preview drawn yet (attach-on-create style entry).
    env.view.preview_pane_area = Rect::default();
    let seed = env.view.live_send_boot_size();
    assert!(
        !matches!(seed, Some((0, _)) | Some((_, 0))),
        "empty preview rect must fall back, not seed a 0-dimension size; got {seed:?}"
    );
}
