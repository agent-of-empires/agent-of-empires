//! E2E coverage for the minimal plugin management core: `plugin list` shows the
//! bundled plugins with version and state, enable/disable round-trips through
//! config, `plugin info` prints the manifest details, `aoe serve` refuses to
//! start while the `aoe.web` plugin is disabled, and the command palette opens
//! the plugin manager listing the builtins.

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
