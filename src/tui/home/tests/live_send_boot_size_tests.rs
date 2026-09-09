/// Live-send must boot a cold/dead agent pane at the visible preview size so it
/// does not start at tmux's 80x24 default and then depend on a single,
/// race-prone post-boot `resize-window` SIGWINCH to grow into the live area.
/// Regression guard for the "live mode opens at ~50% width" race: if this seed
/// regresses back to `None`/default, the agent boots narrow again.
use super::create_test_env_empty;
use ratatui::layout::Rect;

#[test]
#[serial_test::serial]
fn boots_agent_at_visible_preview_size() {
    let mut env = create_test_env_empty();
    // The visible rect the post-toast draw queues to LiveSendWorker.
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
