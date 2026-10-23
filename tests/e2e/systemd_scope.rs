//! Per-session systemd scopes against the real user manager. The manager is
//! shared with the host, so the tests opt back in to scopes with a slice of
//! their own and only touch their own units.

use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{
    app_dir_in, require_tmux, wait_until, write_executable, TuiTestHarness, PROFILE, SYSTEMD_OFF,
};

/// Replaces the harness's `[systemd]` section, with a slice no real session
/// uses: directly under `background.slice`, named after this test run.
/// Returns the slice the harness profile resolves to.
fn configure_scopes(h: &mut TuiTestHarness, enabled: bool) -> String {
    for name in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Ok(value) = std::env::var(name) {
            h.set_env(name, &value);
        } else {
            h.remove_env(name);
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let template = format!(
        "background-aoe_e2e_{}_{nanos}_{{profile}}.slice",
        std::process::id()
    );
    let path = app_dir_in(h.home_path()).join("config.toml");
    let seeded = std::fs::read_to_string(&path).expect("read seeded config");
    assert!(seeded.contains(SYSTEMD_OFF), "harness config: {seeded}");
    let section = format!("[systemd]\nenabled = {enabled}\nslice = \"{template}\"\n");
    std::fs::write(&path, seeded.replace(SYSTEMD_OFF, &section)).expect("write config.toml");
    // The harness profile name needs no escaping.
    template.replace("{profile}", PROFILE)
}

/// `degraded` still runs scopes; only an unreachable manager skips.
fn user_manager_reachable() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
        .is_ok_and(|o| {
            matches!(
                String::from_utf8_lossy(&o.stdout).trim(),
                "running" | "degraded"
            )
        })
}

/// Loaded units matching `glob`, in any state.
fn units_matching(glob: &str) -> Vec<String> {
    let out = Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            "--all",
            "--plain",
            "--no-legend",
            glob,
        ])
        .output()
        .expect("systemctl list-units");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

/// Stops this test's scopes and its slice even when an assertion fails first.
struct ScopeGuard {
    glob: String,
    slice: String,
    launcher: Option<std::process::Child>,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        // Blocking stop: the units are gone before the harness removes its temp home.
        match Command::new("systemctl")
            .args(["--user", "stop", &self.slice])
            .output()
        {
            Ok(out) if !out.status.success() => {
                eprintln!(
                    "systemctl stop {} failed: {}",
                    self.slice,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Err(e) => eprintln!("systemctl stop {} failed: {e}", self.slice),
            _ => {}
        }
        if let Some(mut child) = self.launcher.take() {
            let _ = child.wait();
        }
    }
}

const TITLE: &str = "ScopeProbe";

/// The double-forked `setsid` sleep leaves the pane's process tree and session,
/// so neither aoe's tree kill nor the tmux hangup reaches it; only a scope
/// release can stop it.
const ESCAPEE_AGENT: &str = "#!/bin/sh\n(setsid sleep 601 &)\nexec sleep 600\n";

fn start_session(h: &TuiTestHarness, command: &str, slice: &str) -> (String, ScopeGuard) {
    let project = h.project_path();
    let id = h.add_session(&[
        project.to_str().unwrap(),
        "-t",
        TITLE,
        "--cmd-override",
        command,
    ]);
    let guard = ScopeGuard {
        glob: format!("aoe-session-{id}-*.scope"),
        slice: slice.to_string(),
        launcher: None,
    };
    h.run_cli_ok(&["session", "start", &id]);
    (id, guard)
}

fn pane_pid(h: &TuiTestHarness, id: &str) -> String {
    let tmux_name = agent_of_empires::tmux::Session::generate_name(id, TITLE);
    let out = h
        .tmux()
        .args(["display-message", "-p", "-t", &tmux_name, "#{pane_pid}"])
        .output()
        .expect("tmux display-message");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Waits for a descendant of the pane with exactly `cmdline`.
fn pane_descendant(pane: &str, cmdline: &[u8]) -> String {
    wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let mut queue = vec![pane.to_string()];
        while let Some(pid) = queue.pop() {
            if pid != pane
                && std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c == cmdline)
            {
                return Ok(pid);
            }
            let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                .unwrap_or_default();
            queue.extend(children.split_whitespace().map(str::to_string));
        }
        Err(format!("no {cmdline:?} under pane {pane}"))
    })
}

/// The agent runs as a child of the pane shell; returns the pane pid and the
/// agent's cgroup once `ready` accepts it.
fn agent_cgroup(h: &TuiTestHarness, id: &str, ready: impl Fn(&str) -> bool) -> (String, String) {
    let pane = pane_pid(h, id);
    wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let children = std::fs::read_to_string(format!("/proc/{pane}/task/{pane}/children"))
            .map_err(|e| format!("pane pid {pane:?}: {e}"))?;
        let pid: u32 = children
            .split_whitespace()
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| format!("pane {pane} has no child yet"))?;
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .map_err(|e| format!("agent pid {pid}: {e}"))?;
        if ready(&cgroup) {
            Ok((pane.clone(), cgroup))
        } else {
            Err(format!("agent {pid} cgroup: {cgroup}"))
        }
    })
}

/// Waits for a process with exactly `cmdline` in the cgroup.
fn find_in_cgroup(cgroup: &str, cmdline: &[u8]) -> u32 {
    let procs_path = format!(
        "/sys/fs/cgroup{}/cgroup.procs",
        cgroup.trim().trim_start_matches("0::")
    );
    wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let procs = std::fs::read_to_string(&procs_path).map_err(|e| e.to_string())?;
        procs
            .lines()
            .filter_map(|p| p.parse::<u32>().ok())
            .find(|p| std::fs::read(format!("/proc/{p}/cmdline")).is_ok_and(|c| c == cmdline))
            .ok_or_else(|| format!("no {cmdline:?} in {procs_path}: {procs}"))
    })
}

fn env_var(pid: &str, key: &str) -> Option<String> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).expect("read environ");
    environ.split(|b| *b == 0).find_map(|kv| {
        let kv = String::from_utf8_lossy(kv);
        kv.strip_prefix(&format!("{key}=")).map(str::to_string)
    })
}

fn systemd_unit_in_list(h: &TuiTestHarness, id: &str) -> Value {
    let list: Value = serde_json::from_str(&h.run_cli_ok(&["list", "--json"])).unwrap();
    list.as_array()
        .and_then(|rows| rows.iter().find(|r| r["id"] == id))
        .unwrap_or_else(|| panic!("session {id} missing from aoe list: {list}"))["systemd_unit"]
        .clone()
}

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
#[parallel]
fn tmux_session_runs_in_its_own_scope_until_removed() {
    require_tmux!();
    if !user_manager_reachable() {
        eprintln!("Skipping test: systemd user manager not reachable");
        return;
    }
    let mut h = TuiTestHarness::new("systemd_scope");
    let slice = configure_scopes(&mut h, true);
    let agent = h.home_path().join("escapee-agent");
    write_executable(&agent, ESCAPEE_AGENT);
    let (id, mut guard) = start_session(&h, agent.to_str().unwrap(), &slice);

    let scope_marker = format!("/background.slice/{slice}/aoe-session-{id}-");
    let (pane, cgroup) = agent_cgroup(&h, &id, |c| c.contains(&scope_marker));
    let escapee = find_in_cgroup(&cgroup, b"sleep\x00601\x00");
    // `systemd-run` must neither expand the launch's `$$` nor change the env.
    let agent = find_in_cgroup(&cgroup, b"sleep\x00600\x00").to_string();
    let agent_pid = env_var(&agent, "AOE_AGENT_PID");
    assert!(
        agent_pid
            .as_deref()
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "AOE_AGENT_PID={agent_pid:?}"
    );
    assert_eq!(
        env_var(&agent, "INVOCATION_ID"),
        env_var(&pane, "INVOCATION_ID")
    );
    let listed = systemd_unit_in_list(&h, &id);
    let listed = listed
        .as_str()
        .expect("a live scoped session exposes its expected namespace pattern");
    assert!(
        cgroup.contains(listed.strip_suffix("*.scope").unwrap()),
        "listed {listed}, actual cgroup {cgroup}"
    );
    // A second namespace can use the same SID; the first row never owns its unit.
    let replacement_unit = format!(
        "aoe-session-{id}-another-namespace-1-{}.scope",
        uuid::Uuid::new_v4()
    );
    guard.launcher = Some(
        Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--expand-environment=no",
                "--unit",
                &replacement_unit,
                "--slice",
                &slice,
                "/bin/sh",
                "-c",
                "exec sleep 602",
            ])
            .spawn()
            .expect("start this test's independent same-SID scope"),
    );
    let replacement = guard.launcher.as_mut().unwrap();
    let replacement_cgroup = wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        if let Some(status) = replacement.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("independent scope launcher exited: {status}"));
        }
        let out = Command::new("systemctl")
            .args([
                "--user",
                "show",
                "--property=ControlGroup",
                "--value",
                &replacement_unit,
            ])
            .output()
            .map_err(|error| error.to_string())?;
        let group = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !group.is_empty() {
            Ok(format!("0::{group}"))
        } else {
            Err(format!(
                "independent scope has no cgroup yet: {}",
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    });
    let replacement_pid = find_in_cgroup(&replacement_cgroup, b"sleep\x00602\x00");

    h.run_cli_ok(&["rm", &id, "--force"]);
    let owned_unit = cgroup.trim().rsplit('/').next().unwrap();
    wait_for_release(owned_unit, escapee);
    assert!(
        pid_alive(replacement_pid),
        "owned cleanup killed a same-SID unit from a different namespace"
    );
    assert_eq!(
        std::fs::read_to_string(format!("/proc/{replacement_pid}/cgroup"))
            .unwrap()
            .trim(),
        replacement_cgroup
    );
    assert!(
        replacement.try_wait().unwrap().is_none(),
        "the independent scope must still be running"
    );
    let stopped = Command::new("systemctl")
        .args(["--user", "stop", &replacement_unit])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let _ = replacement.wait().unwrap();
}

/// The escapee keeps the scope alive unless `aoe rm` released it.
fn wait_for_release(glob: &str, escapee: u32) {
    wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
        let units = units_matching(glob);
        if units.is_empty() && !pid_alive(escapee) {
            Ok(())
        } else {
            Err(format!(
                "units {units:?} escapee {escapee} alive={}",
                pid_alive(escapee)
            ))
        }
    });
}

/// `enabled = false` stops new scopes only: a later `aoe rm`, in a process
/// that reads the new setting, still releases the scope started before.
#[test]
#[parallel]
fn rm_releases_an_existing_scope_after_scopes_are_disabled() {
    require_tmux!();
    if !user_manager_reachable() {
        eprintln!("Skipping test: systemd user manager not reachable");
        return;
    }
    let mut h = TuiTestHarness::new("systemd_scope_disabled_later");
    let slice = configure_scopes(&mut h, true);
    let agent = h.home_path().join("escapee-agent");
    write_executable(&agent, ESCAPEE_AGENT);
    let (id, guard) = start_session(&h, agent.to_str().unwrap(), &slice);

    let scope_marker = format!("/background.slice/{slice}/aoe-session-{id}-");
    let (_, cgroup) = agent_cgroup(&h, &id, |c| c.contains(&scope_marker));
    let escapee = find_in_cgroup(&cgroup, b"sleep\x00601\x00");

    let path = app_dir_in(h.home_path()).join("config.toml");
    let config = std::fs::read_to_string(&path).expect("read config.toml");
    let disabled = config.replace(
        "[systemd]\nenabled = true\n",
        "[systemd]\nenabled = false\n",
    );
    assert_ne!(disabled, config, "config.toml: {config}");
    std::fs::write(&path, disabled).expect("write config.toml");

    h.run_cli_ok(&["rm", &id, "--force"]);
    wait_for_release(&guard.glob, escapee);
}

/// Negative control: with scopes disabled the agent stays where tmux put it.
#[test]
#[parallel]
fn tmux_session_stays_unscoped_when_disabled() {
    require_tmux!();
    if !user_manager_reachable() {
        eprintln!("Skipping test: systemd user manager not reachable");
        return;
    }
    let mut h = TuiTestHarness::new("systemd_scope_off");
    let slice = configure_scopes(&mut h, false);
    let (id, guard) = start_session(&h, "sleep 600", &slice);

    // The agent itself, so the check cannot race a `systemd-run` still starting.
    let agent = pane_descendant(&pane_pid(&h, &id), b"sleep\x00600\x00");
    let cgroup = std::fs::read_to_string(format!("/proc/{agent}/cgroup")).unwrap();
    assert!(
        !cgroup.contains(&format!("/aoe-session-{id}-")) && !cgroup.contains(&slice),
        "disabled scopes must leave the agent alone: {cgroup}"
    );
    assert!(units_matching(&guard.glob).is_empty());
    assert!(units_matching(&slice).is_empty());
    assert_eq!(systemd_unit_in_list(&h, &id), Value::Null);
    h.run_cli_ok(&["rm", &id, "--force"]);
}
