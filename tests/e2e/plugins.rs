//! E2E coverage for the minimal plugin management core: `plugin list` shows the
//! bundled plugins with version and state, enable/disable round-trips through
//! config, `plugin info` prints the manifest details, `aoe serve` refuses to
//! start while the `aoe.web` plugin is disabled, and the command palette opens
//! the plugin manager listing the builtins.
//!
//! Compiled only under `serve`: the sole bundled plugin is the serve-gated
//! `aoe.web`, so without that feature the builtin set is empty and there is
//! nothing for these management-surface tests to exercise. The bare-core
//! `--no-default-features` e2e leg skips this module by design.
#![cfg(feature = "serve")]

use serial_test::serial;

use crate::harness::{require_tmux, TuiTestHarness};

/// `plugin list` prints a header then one row per builtin. The only bundled
/// plugin is `aoe.web`, which starts enabled.
#[test]
#[serial]
fn test_plugin_list_shows_builtins_with_state() {
    let h = TuiTestHarness::new("plugin_list");
    let output = h.run_cli(&["plugin", "list"]);
    assert!(
        output.status.success(),
        "aoe plugin list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ID") && stdout.contains("VERSION") && stdout.contains("STATE"),
        "missing header line:\n{stdout}"
    );
    let web_line = stdout
        .lines()
        .find(|l| l.contains("aoe.web"))
        .unwrap_or_else(|| panic!("missing aoe.web row:\n{stdout}"));
    assert!(
        web_line.contains("1.0.0"),
        "aoe.web row missing version:\n{web_line}"
    );
    assert!(
        web_line.contains("enabled"),
        "aoe.web should start enabled:\n{web_line}"
    );
}

/// Disabling then re-enabling `aoe.web` flips its state in the list. Unknown
/// ids error and name the fix.
#[test]
#[serial]
fn test_plugin_disable_enable_round_trip() {
    let h = TuiTestHarness::new("plugin_toggle");

    let disable = h.run_cli(&["plugin", "disable", "aoe.web"]);
    assert!(
        disable.status.success(),
        "disable failed: {}",
        String::from_utf8_lossy(&disable.stderr)
    );
    let list = h.run_cli(&["plugin", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    let web_line = stdout
        .lines()
        .find(|l| l.contains("aoe.web"))
        .unwrap_or_else(|| panic!("aoe.web missing from list:\n{stdout}"));
    assert!(
        web_line.contains("disabled"),
        "aoe.web should be disabled:\n{web_line}"
    );

    let enable = h.run_cli(&["plugin", "enable", "aoe.web"]);
    assert!(
        enable.status.success(),
        "enable failed: {}",
        String::from_utf8_lossy(&enable.stderr)
    );
    let list = h.run_cli(&["plugin", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    let web_line = stdout.lines().find(|l| l.contains("aoe.web")).unwrap();
    assert!(
        web_line.contains("enabled") && !web_line.contains("disabled"),
        "aoe.web should be enabled again:\n{web_line}"
    );

    // An unknown id errors and points at `plugin list`.
    let bad = h.run_cli(&["plugin", "enable", "acme.nope"]);
    assert!(
        !bad.status.success(),
        "enabling an unknown plugin must fail"
    );
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("unknown plugin"),
        "unknown-id error must name the problem:\n{}",
        String::from_utf8_lossy(&bad.stderr)
    );
}

/// `plugin info aoe.web` prints the manifest name/id header plus the version,
/// state, and about lines.
#[test]
#[serial]
fn test_plugin_info_prints_manifest_details() {
    let h = TuiTestHarness::new("plugin_info");
    let output = h.run_cli(&["plugin", "info", "aoe.web"]);
    assert!(
        output.status.success(),
        "info failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Web Dashboard (aoe.web)"),
        "name/id header missing:\n{stdout}"
    );
    assert!(
        stdout.contains("version:") && stdout.contains("1.0.0"),
        "version line missing:\n{stdout}"
    );
    assert!(
        stdout.contains("state:") && stdout.contains("enabled"),
        "state line missing:\n{stdout}"
    );
    assert!(
        stdout.contains("about:"),
        "about line missing for a plugin with a description:\n{stdout}"
    );
}

/// `aoe.web` is a default plugin; disabling it must turn off the serve surface
/// at runtime. The gate bails in the foreground invocation before any daemon
/// spawn, and re-enabling restores it.
#[test]
#[serial]
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
        String::from_utf8_lossy(&refused.stderr).contains("web dashboard plugin is disabled"),
        "refusal must name the fix:\n{}",
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

/// The command palette opens the plugin manager, which lists the bundled
/// plugins by manifest name + version with their state. Palette-only (no
/// default chord).
#[test]
#[serial]
fn test_palette_opens_plugin_manager_listing_builtins() {
    require_tmux!();

    let mut h = TuiTestHarness::new("plugin_manager_palette");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("C-k");
    h.wait_for("Commands");
    h.type_text("plugins");
    h.wait_for("Manage plugins");
    h.send_keys("Enter");

    // The manager lists builtins by their manifest name + version and state.
    h.wait_for(" Plugins ");
    h.assert_screen_contains("Web Dashboard v1.0.0");
    h.assert_screen_contains("enabled");
}

/// Open the plugin manager through the palette. Shared by the interactive
/// manager tests below.
fn open_manager(h: &TuiTestHarness) {
    h.wait_for(" aoe ");
    h.send_keys("C-k");
    h.wait_for("Commands");
    h.type_text("plugins");
    h.wait_for("Manage plugins");
    h.send_keys("Enter");
    h.wait_for(" Plugins ");
}

/// Space in the manager toggles the selected plugin and reports where the
/// write landed (no daemon here, so the plain local message).
#[test]
#[serial]
fn test_tui_manager_toggle_round_trip() {
    require_tmux!();

    let mut h = TuiTestHarness::new("plugin_manager_toggle");
    h.spawn_tui();
    open_manager(&h);
    h.assert_screen_contains("enabled");

    h.send_keys("Space");
    h.wait_for("Disabled aoe.web");
    h.assert_screen_contains("disabled");

    h.send_keys("Space");
    h.wait_for("Enabled aoe.web");
}

/// Enter opens the details popup: the full manifest disclosure for the
/// selected plugin (`aoe plugin info`'s TUI twin), Esc closes it.
#[test]
#[serial]
fn test_tui_manager_details_popup() {
    require_tmux!();

    let mut h = TuiTestHarness::new("plugin_manager_details");
    h.spawn_tui();
    open_manager(&h);

    h.send_keys("Enter");
    h.wait_for(" Plugin details ");
    h.assert_screen_contains("Web Dashboard v1.0.0 (aoe.web)");
    h.assert_screen_contains("Builtin plugin (compiled into aoe)");
    h.assert_screen_contains("No capabilities requested.");

    h.send_keys("Escape");
    h.wait_for_absent(" Plugin details ", std::time::Duration::from_secs(5));
}

/// The full external-plugin loop the manager now owns: a locally installed
/// plugin whose manifest changed shows "needs approval"; `a` opens the
/// re-approval consent popup disclosing its capabilities, `y` re-grants; `x`
/// then uninstalls it behind a confirmation.
#[test]
#[serial]
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
