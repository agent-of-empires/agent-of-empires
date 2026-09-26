//! E2E coverage for the minimal plugin management core: `plugin list` shows the
//! bundled plugins with version and state, enable/disable round-trips through
//! config, `plugin info` prints the manifest details, `aoe serve` refuses to
//! start while the `aoe.web` plugin is disabled, and the command palette opens
//! the plugin manager listing the builtins.
//!
//! Compiled only under `web`: the sole bundled plugin is the `web`-gated
//! `aoe.web`, so without that feature the builtin set is empty and there is
//! nothing for these management-surface tests to exercise. The bare-core
//! `--no-default-features` e2e leg skips this module by design.
#![cfg(feature = "web")]

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

fn web_row(h: &TuiTestHarness) -> String {
    let stdout = h.run_cli_ok(&["plugin", "list"]);
    stdout
        .lines()
        .find(|l| l.contains("aoe.web"))
        .unwrap_or_else(|| panic!("missing aoe.web row:\n{stdout}"))
        .to_string()
}

/// `plugin list` shows the builtin `aoe.web` with version and state,
/// `plugin info` prints its manifest details, and disable/enable flip the
/// listed state. Unknown ids error and name the fix.
#[test]
#[parallel]
fn test_plugin_cli_lists_describes_and_toggles_builtins() {
    let h = TuiTestHarness::new("plugin_cli");
    let list = h.run_cli_ok(&["plugin", "list"]);
    assert!(
        list.contains("ID") && list.contains("VERSION") && list.contains("STATE"),
        "missing header line:\n{list}"
    );
    let row = web_row(&h);
    assert!(row.contains("1.0.0") && row.contains("enabled"), "{row}");

    let info = h.run_cli_ok(&["plugin", "info", "aoe.web"]);
    for needle in [
        "Web Dashboard (aoe.web)",
        "version:",
        "1.0.0",
        "state:",
        "enabled",
        "about:",
    ] {
        assert!(info.contains(needle), "{needle} missing:\n{info}");
    }

    h.run_cli_ok(&["plugin", "disable", "aoe.web"]);
    assert!(web_row(&h).contains("disabled"));
    h.run_cli_ok(&["plugin", "enable", "aoe.web"]);
    let row = web_row(&h);
    assert!(
        row.contains("enabled") && !row.contains("disabled"),
        "{row}"
    );

    let stderr = h.run_cli_err(&["plugin", "enable", "acme.nope"]);
    assert!(stderr.contains("unknown plugin"), "{stderr}");
}

/// `aoe.web` is a default plugin; disabling it must turn off the serve surface
/// at runtime. A fresh `aoe serve` start is then rejected as an unrecognized
/// subcommand before any daemon spawn, and re-enabling restores it.
#[test]
#[parallel]
fn test_serve_refuses_when_web_plugin_disabled() {
    let h = TuiTestHarness::new("plugin_serve_gate");
    let free_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();

    let disable = h.run_cli(&["plugin", "disable", "aoe.web"]);
    assert!(disable.status.success(), "disable aoe.web failed");

    let refused = h.run_cli(&["serve", "--daemon", "--port", &free_port, "--no-auth"]);
    assert!(
        !refused.status.success(),
        "serve must refuse while aoe.web is disabled"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("unrecognized subcommand 'serve'"),
        "refusal must read as an unrecognized subcommand:\n{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let enable = h.run_cli(&["plugin", "enable", "aoe.web"]);
    assert!(enable.status.success(), "enable aoe.web failed");

    let started = h.run_cli(&["serve", "--daemon", "--port", &free_port, "--no-auth"]);
    assert!(
        started.status.success(),
        "serve must start once aoe.web is enabled:\n{}",
        String::from_utf8_lossy(&started.stderr)
    );
    // `serve --daemon` returns once the child is spawned, before it has finished
    // binding the port and writing serve.pid. Stopping in that window races the
    // startup, so wait for the port to accept a connection first (mirrors the
    // serve.rs lifecycle test).
    let port: u16 = free_port.parse().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "daemon never bound port {port} after enabling aoe.web"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let stopped = h.run_cli(&["serve", "--stop"]);
    assert!(stopped.status.success(), "serve --stop must succeed");
}

/// Open the plugin manager through the palette (palette-only, no chord).
fn open_manager(h: &TuiTestHarness) {
    h.wait_for(" aoe ");
    h.send_keys("C-k");
    h.wait_for("Commands");
    h.type_text("plugins");
    h.wait_for("Manage plugins");
    h.send_keys("Enter");
    h.wait_for(" Plugins ");
}

/// The manager lists builtins by manifest name and version with their state.
/// Enter opens the details popup (`aoe plugin info`'s TUI twin) and Esc closes
/// it; Space toggles the plugin and reports where the write landed (no daemon
/// here, so the plain local message).
#[test]
#[parallel]
fn test_tui_manager_lists_details_and_toggles() {
    require_tmux!();

    let mut h = TuiTestHarness::new("plugin_manager");
    h.spawn_tui();
    open_manager(&h);
    h.assert_screen_contains("Web Dashboard v1.0.0");
    h.assert_screen_contains("enabled");

    h.send_keys("Enter");
    h.wait_for(" Plugin details ");
    h.assert_screen_contains("Web Dashboard v1.0.0 (aoe.web)");
    h.assert_screen_contains("Builtin plugin (compiled into aoe)");
    h.assert_screen_contains("No capabilities requested.");
    h.send_keys("Escape");
    h.wait_for_absent(" Plugin details ", std::time::Duration::from_secs(5));

    h.send_keys("Space");
    h.wait_for("Disabled aoe.web");
    h.assert_screen_contains("disabled");
    h.send_keys("Space");
    h.wait_for("Enabled aoe.web");
}

/// The full external-plugin loop the manager now owns: a locally installed
/// plugin whose manifest changed shows "needs approval"; `a` opens the
/// re-approval consent popup disclosing its capabilities, `y` re-grants; `x`
/// then uninstalls it behind a confirmation.
#[test]
#[parallel]
fn test_tui_manager_reapprove_and_uninstall_local_plugin() {
    require_tmux!();

    let mut h = TuiTestHarness::new("plugin_manager_lifecycle");

    // Install a local plugin, then grow its on-disk capability set so the
    // recorded grant no longer covers the manifest (needs re-approval).
    let src = h.home_path().join("src-plugin");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("aoe-plugin.toml"),
        r#"
id = "acme.tui"
name = "Tui Test"
version = "0.1.0"
api_version = 2
capabilities = ["net"]
"#,
    )
    .unwrap();
    let installed = h.run_cli(&["plugin", "install", src.to_str().unwrap(), "--yes"]);
    assert!(
        installed.status.success(),
        "local install failed: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let manifest = crate::harness::app_dir_in(h.home_path())
        .join("plugins")
        .join("acme.tui")
        .join("aoe-plugin.toml");
    let text = std::fs::read_to_string(&manifest).unwrap().replace(
        "capabilities = [\"net\"]",
        "capabilities = [\"net\", \"notifications\"]",
    );
    std::fs::write(&manifest, text).unwrap();

    h.spawn_tui();
    open_manager(&h);
    h.wait_for("Tui Test v0.1.0");
    h.assert_screen_contains("needs approval");

    // Row order is builtins first, then externals: step down to acme.tui.
    h.send_keys("j");
    h.send_keys("a");
    h.wait_for(" Approve plugin ");
    h.assert_screen_contains("notifications");
    // The decision keys are a pinned footer: they must be visible no matter
    // how tall the disclosure body is.
    h.assert_screen_contains("y approve");
    h.send_keys("y");
    h.wait_for("Approved acme.tui");
    h.wait_for_absent("needs approval", std::time::Duration::from_secs(5));

    // Uninstall the same row behind the confirmation popup.
    h.send_keys("x");
    h.wait_for(" Uninstall plugin ");
    h.assert_screen_contains("y uninstall");
    h.send_keys("y");
    h.wait_for("Uninstalled acme.tui");
    h.wait_for_absent("Tui Test", std::time::Duration::from_secs(5));
}
