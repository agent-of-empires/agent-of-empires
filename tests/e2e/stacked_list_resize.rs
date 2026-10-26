use crate::harness::{app_dir_in, require_tmux, wait_until, TuiTestHarness};
use serial_test::parallel;
use std::time::Duration;

fn seam_row(h: &TuiTestHarness) -> u16 {
    h.capture_screen()
        .lines()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.starts_with('╭'))
        .map(|(row, _)| row as u16 + 1)
        .expect("stacked preview border")
}

#[test]
#[parallel]
fn test_stacked_list_resize_persists_and_restores_after_window_resize() {
    require_tmux!();
    let mut h = TuiTestHarness::new("stacked_list_resize");
    h.append_config("[session]\ndefault_attach_mode = \"live_send\"");
    let bin = h.install_path_command("claude");
    std::fs::write(
        bin.join("claude"),
        "#!/bin/sh\nstty -echo -icanon min 1 time 0\nprintf '\\033[?1049h\\033[?1002h\\033[?1006hDEMO READY\\r\\n'\nexec cat > \"$HOME/agent-input\"\n",
    )
    .unwrap();
    h.run_cli_ok(&["add", h.project_path().to_str().unwrap(), "-t", "Demo"]);
    h.spawn_tui();
    h.wait_for("Demo");
    h.send_keys("Enter");
    h.wait_for("LIVE");
    h.wait_for("DEMO READY");
    h.resize_window(60, 40);
    wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let screen = h.capture_screen();
        (screen.lines().filter(|l| l.starts_with('╭')).count() == 2)
            .then_some(())
            .ok_or(screen)
    });
    let original = seam_row(&h);
    h.send_mouse_drag((30, original), (30, 20));
    assert_eq!(seam_row(&h), 20);
    h.send_paste("input-fence\n");
    wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let input = std::fs::read_to_string(h.home_path().join("agent-input")).unwrap_or_default();
        if !input.contains("input-fence") {
            return Err(input);
        }
        assert!(
            !input.contains('\x1b'),
            "divider gesture reached agent: {input:?}"
        );
        Ok(())
    });
    let state_path = app_dir_in(h.home_path()).join("state.toml");
    let state: toml::Value = toml::from_str(&std::fs::read_to_string(state_path).unwrap()).unwrap();
    assert_eq!(state["home_list_height"].as_integer(), Some(19));

    h.resize_window(60, 18);
    wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let row = seam_row(&h);
        (row < 20).then_some(()).ok_or(format!("border at {row}"))
    });
    h.resize_window(60, 40);
    wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let row = seam_row(&h);
        (row == 20).then_some(()).ok_or(format!("border at {row}"))
    });
}
