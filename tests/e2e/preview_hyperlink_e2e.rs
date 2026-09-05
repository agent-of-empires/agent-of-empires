//! e2e coverage for opening an OSC 8 hyperlink from the preview pane.
//!
//! A hyperlink's target cannot survive the render path: vt100 routes OSC 8 to
//! its unhandled-sequence hook and keeps nothing, and a ratatui cell has no
//! attribute to hold a URI, so a link whose visible text is not itself a URL
//! was inert (#3735). The targets are kept beside the grid and matched back
//! onto the row that shows them.
//!
//! Unit tests cover the scanner and the column mapping against real tmux and
//! real Claude Code bytes. Only an e2e can prove the rest of the chain: a live
//! pane emitting the sequence, the worker's transport, the painted row, and a
//! real mouse click resolving it. `AOE_OPEN_URL_TO` stands in for the browser
//! so the resolved URL is asserted exactly.

use serial_test::parallel;
use std::time::Duration;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

const LINK_TEXT: &str = "OPENME-LINK";
const LINK_URL: &str = "https://example.com/aoe-e2e";
/// Custom agent name, so the Agent-view pane runs a script we control instead
/// of a real agent.
const LINK_AGENT: &str = "linkagent";

/// The `printf` that emits one OSC 8 hyperlink whose visible text hides its
/// target, as a `/bin/sh` line.
fn emit_link_sh() -> String {
    format!("printf 'before \\033]8;;{LINK_URL}\\033\\\\{LINK_TEXT}\\033]8;;\\033\\\\ after\\n'")
}

/// Assert the link row is underlined, then click it and wait for the URL to
/// reach the browser seam. Shared by both transports.
fn assert_link_underlined_and_opens(h: &TuiTestHarness, opened: &std::path::Path) {
    let screen = h.capture_screen();
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains(LINK_TEXT))
        .map(|(i, l)| (i as u16 + 1, l.to_string()))
        .expect("link text on screen");
    let byte_offset = line.find(LINK_TEXT).expect("link text in row");
    // Two cells into the text, comfortably inside the span.
    let col = line[..byte_offset].chars().count() as u16 + 3;

    let styled_row = h
        .capture_screen_styled()
        .lines()
        .find(|l| l.contains(LINK_TEXT))
        .unwrap_or_default()
        .to_string();
    assert!(
        styled_row.contains("\u{1b}[4m"),
        "link row must carry an underline SGR, got: {styled_row:?}"
    );
    // The backend re-emits the target, so the terminal aoe runs in gets a real
    // hyperlink rather than styled text it cannot act on. tmux stores what it
    // receives, so a capture of aoe's OWN pane is the proof.
    assert!(
        styled_row.contains(&format!("\x1b]8;;{LINK_URL}\x1b\\")),
        "link row must carry OSC 8 through to the host terminal, got: {styled_row:?}"
    );

    h.send_mouse_click(0, col, row);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::read_to_string(opened)
            .map(|s| s.contains(LINK_URL))
            .unwrap_or(false)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "click at ({col},{row}) never opened {LINK_URL}; opened file: {:?}, screen:\n{}",
            std::fs::read_to_string(opened).ok(),
            h.capture_screen()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Route activation straight into live-send so the preview paints the pane,
/// mirroring `live_send_paste_e2e`.
fn write_live_send_config(h: &TuiTestHarness) {
    let config_dir = app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
has_responded_to_telemetry = true
last_seen_version = "{version}"
has_acknowledged_agent_hooks = true

[session]
default_attach_mode = "live_send"
"#,
        version = env!("CARGO_PKG_VERSION"),
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write live-send config");
}

/// A hyperlink whose visible text hides its target must render underlined in
/// the preview, and a plain left-click on it must open the target. The shape
/// from the issue: before the fix the text was indistinguishable from ordinary
/// output and the click did nothing.
///
/// Driven through Terminal view because a shell is a hyperlink source that
/// needs no agent installed, which also keeps the test off the network.
#[test]
#[parallel]
fn test_preview_click_opens_osc8_hyperlink() {
    require_tmux!();
    if !TuiTestHarness::tmux_reemits_hyperlinks() {
        eprintln!("Skipping test: tmux older than 3.4 (no OSC 8)");
        return;
    }

    let mut h = TuiTestHarness::new("preview_hyperlink");
    write_live_send_config(&h);

    // The link is emitted by a script rather than a typed command so the echoed
    // command line cannot itself contain the link text and be mistaken for the
    // rendered link.
    let script = h.home_path().join("emit-link.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf 'before \\033]8;;{LINK_URL}\\033\\\\{LINK_TEXT}\\033]8;;\\033\\\\ after\\n'\n"
        ),
    )
    .expect("write link script");

    // Redirect the browser open to a file so the resolved URL is assertable.
    let opened = h.home_path().join("opened-urls.txt");
    h.set_env("AOE_OPEN_URL_TO", &opened.display().to_string());

    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Linky"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    h.wait_for("Linky");
    // `t` switches to Terminal view, whose pane is a plain shell.
    h.send_keys("t");
    h.send_keys("Enter");
    h.wait_for_timeout("LIVE", Duration::from_secs(15));

    // `clear` first so the echoed command scrolls away and the only row left
    // holding the link text is the one the shell painted from the sequence.
    h.type_text(&format!("clear; sh {}", script.display()));
    h.send_keys("Enter");
    h.wait_for_timeout(LINK_TEXT, Duration::from_secs(20));

    assert_link_underlined_and_opens(&h, &opened);
}

/// A URL that is merely visible in the output, carrying no sequence at all,
/// must be clickable on the same plain gesture. Without this the host terminal
/// is the only thing that can open it, and only behind a modifier, because aoe
/// holds the mouse.
#[test]
#[parallel]
fn test_preview_click_opens_a_bare_url_with_no_osc8() {
    require_tmux!();

    let mut h = TuiTestHarness::new("preview_bare_url");
    write_live_send_config(&h);

    let opened = h.home_path().join("opened-urls.txt");
    h.set_env("AOE_OPEN_URL_TO", &opened.display().to_string());

    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Linky"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    h.wait_for("Linky");
    h.send_keys("t");
    h.send_keys("Enter");
    h.wait_for_timeout("LIVE", Duration::from_secs(15));

    // Trailing punctuation is sentence structure, not part of the URL, so the
    // resolved target must not include the full stop.
    const BARE: &str = "https://example.com/bare-e2e";
    // Printed from a script rather than a typed command, so the echoed command
    // line cannot itself hold the URL: a narrow preview wraps that echo and
    // leaves a row carrying only the tail of the address, which the row search
    // below would then pick up as the link row.
    let script = h.home_path().join("emit-bare.sh");
    std::fs::write(&script, format!("#!/bin/sh\necho 'see {BARE}.'\n"))
        .expect("write bare url script");
    h.type_text(&format!("clear; sh {}", script.display()));
    h.send_keys("Enter");
    h.wait_for_timeout(BARE, Duration::from_secs(20));

    let screen = h.capture_screen();
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains(BARE))
        .map(|(i, l)| (i as u16 + 1, l.to_string()))
        .expect("url on screen");
    let col = line[..line.find(BARE).expect("url in row")].chars().count() as u16 + 3;
    h.send_mouse_click(0, col, row);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let got = std::fs::read_to_string(&opened).unwrap_or_default();
        if got.contains(BARE) {
            assert!(
                !got.contains(&format!("{BARE}.")),
                "trailing punctuation must not be part of the target: {got:?}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "click at ({col},{row}) never opened {BARE}; opened: {got:?}, screen:\n{}",
            h.capture_screen()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The same contract in Agent view, which is where the issue was reported and
/// which renders through the in-process vt100 grid rather than `capture-pane`.
/// That transport drops OSC 8 during parsing, so the target can only come from
/// the channel's own tap on the pane's byte stream; the debug trace asserts it
/// did, rather than the test passing on a silent fall back to capture.
///
/// The pane runs a custom agent so it emits a known sequence without any real
/// agent installed.
#[test]
#[parallel]
fn test_agent_view_click_opens_osc8_hyperlink_over_vt() {
    require_tmux!();
    if !TuiTestHarness::tmux_reemits_hyperlinks() {
        eprintln!("Skipping test: tmux older than 3.4 (no OSC 8)");
        return;
    }

    let mut h = TuiTestHarness::new("preview_hyperlink_vt");

    let bin = h.install_path_command(LINK_AGENT);
    let stub = bin.join(LINK_AGENT);
    std::fs::write(
        &stub,
        format!("#!/bin/sh\n{}\nexec sleep 600\n", emit_link_sh()),
    )
    .expect("write link agent");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    let config_dir = app_dir_in(h.home_path());
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
has_responded_to_telemetry = true
last_seen_version = "{version}"
has_acknowledged_agent_hooks = true

[session]
default_attach_mode = "live_send"
custom_agents = {{ "{LINK_AGENT}" = "{LINK_AGENT}" }}
"#,
            version = env!("CARGO_PKG_VERSION"),
        ),
    )
    .expect("write config");

    let opened = h.home_path().join("opened-urls.txt");
    h.set_env("AOE_OPEN_URL_TO", &opened.display().to_string());
    // The transport assertion below reads the debug trace.
    h.set_env("AGENT_OF_EMPIRES_DEBUG", "1");

    let project = h.project_path();
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "Linky",
        "--tool",
        LINK_AGENT,
    ]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    h.wait_for("Linky");
    // Agent view is the default; Enter drops straight into live-send on it.
    h.send_keys("Enter");
    h.wait_for_timeout("LIVE", Duration::from_secs(15));
    h.wait_for_timeout(LINK_TEXT, Duration::from_secs(20));

    assert_link_underlined_and_opens(&h, &opened);

    // vt100 keeps no hyperlink, so a target reaching the row through the VT
    // channel can only have come from the raw-stream tap. The channel arms on a
    // throttle after the pane settles, so the preview can legitimately serve a
    // frame or two from the capture fallback first; poll for the handover
    // rather than sampling once and racing it.
    let log_path = config_dir.join("debug.log");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let collected: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("collected pane hyperlinks"))
            .collect();
        if collected.iter().any(|l| l.contains("from_channel=1")) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Agent view never sourced the target from the VT channel; traces were:\n{}",
            collected.join("\n")
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}
