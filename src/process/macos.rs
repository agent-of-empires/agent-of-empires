//! macOS-specific process utilities

use std::collections::HashMap;
use std::process::Command;

/// Collect `pid` and every descendant by parsing `ps -A` once and walking the map.
pub(super) fn collect_pid_tree(pid: u32) -> Vec<u32> {
    let children_map = build_children_map();
    let mut pids = vec![pid];
    super::collect_descendants_from_map(pid, &children_map, &mut pids);
    pids
}

/// Build a map of parent PID -> list of child PIDs by parsing `ps` output once
fn build_children_map() -> HashMap<u32, Vec<u32>> {
    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();

    let Ok(output) = Command::new("ps").args(["-o", "pid=,ppid=", "-A"]).output() else {
        return children_map;
    };

    if !output.status.success() {
        return children_map;
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(child_pid), Ok(ppid)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                children_map.entry(ppid).or_default().push(child_pid);
            }
        }
    }

    children_map
}

/// One `ps -A -ww -E -o command=` fork deciding, for each candidate `i`,
/// whether a live process belongs to it: a whitespace-delimited token exactly
/// equals `env_needles[i]` (anchored, matching the `KEY=VAL` env tokens `-E`
/// appends), or the line contains `cmdline_needles[i]`. `-ww` disables column
/// truncation; `-E` appends each owner-owned process's environment. If a `ps`
/// build rejects `-E`, the call fails closed to all `false` and recovery falls
/// back to the ledger. Best-effort: a failed `ps` yields all `false`.
pub(super) fn processes_matching(
    env_needles: &[String],
    cmdline_needles: &[Option<String>],
) -> Vec<bool> {
    let n = env_needles.len();
    let mut found = vec![false; n];
    let Ok(output) = Command::new("ps")
        .args(["-A", "-ww", "-E", "-o", "command="])
        .output()
    else {
        return found;
    };
    if !output.status.success() {
        return found;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let tokens: std::collections::HashSet<&str> = line.split_whitespace().collect();
        for i in 0..n {
            if found[i] {
                continue;
            }
            let env_hit = !env_needles[i].is_empty() && tokens.contains(env_needles[i].as_str());
            let cmd_hit = cmdline_needles[i]
                .as_deref()
                .is_some_and(|s| !s.is_empty() && line.contains(s));
            if env_hit || cmd_hit {
                found[i] = true;
            }
        }
    }
    found
}

/// Per-boot identity from `kern.bootsessionuuid`: a UUID fixed for the boot's
/// lifetime. Preferred over `kern.boottime`, which is recomputed as
/// `now - uptime` and shifts on clock steps (NTP, sleep/wake), which would
/// silently rotate the ledger mid-boot.
pub(super) fn boot_id() -> Option<String> {
    let out = Command::new("sysctl")
        .args(["-n", "kern.bootsessionuuid"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Get the foreground process group leader for a shell PID
pub fn get_foreground_pid(shell_pid: u32) -> Option<u32> {
    // Use ps to get the foreground process group
    // ps -o tpgid= -p <pid> gives us the terminal foreground process group ID
    let output = Command::new("ps")
        .args(["-o", "tpgid=", "-p", &shell_pid.to_string()])
        .output()
        .ok()?;

    if !output.status.success() {
        return Some(shell_pid);
    }

    let tpgid: i32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;

    if tpgid <= 0 {
        return Some(shell_pid);
    }

    // Find a process in the foreground group
    find_process_in_group(tpgid as u32).or(Some(shell_pid))
}

/// Find a process belonging to the given process group
fn find_process_in_group(pgrp: u32) -> Option<u32> {
    // Use ps to find processes in this group
    // ps -o pid=,pgid= -A lists all processes with their PIDs and PGIDs
    let output = Command::new("ps")
        .args(["-o", "pid=,pgid=", "-A"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(pid), Ok(proc_pgrp)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                if proc_pgrp == pgrp {
                    return Some(pid);
                }
            }
        }
    }

    None
}

/// Prevents user-idle system sleep by holding a `caffeinate` child. `-i`
/// inhibits system idle sleep only, so the display still sleeps normally.
#[cfg(feature = "serve")]
pub(super) struct CaffeinateInhibitor {
    child: Option<std::process::Child>,
}

#[cfg(feature = "serve")]
impl CaffeinateInhibitor {
    pub(super) fn new() -> Self {
        Self { child: None }
    }
}

#[cfg(feature = "serve")]
impl super::SleepInhibit for CaffeinateInhibitor {
    fn acquire(&mut self) -> anyhow::Result<()> {
        if super::sleep_inhibit_unavailable() {
            return Ok(());
        }
        // `-w <daemon_pid>` makes caffeinate exit when the daemon exits, so
        // the assertion is released even on `std::process::exit`, a panic,
        // OOM, or `kill -9`, none of which run a `Drop`.
        let child = match Command::new("caffeinate")
            .args(["-i", "-w", &std::process::id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                super::latch_sleep_inhibit_unavailable(
                    "caffeinate not found; OS sleep will not be inhibited on this host",
                );
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        self.child = Some(child);
        Ok(())
    }

    fn release(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn is_held_alive(&mut self) -> bool {
        super::sleep_inhibit_child_held_alive(
            &mut self.child,
            "caffeinate exited unexpectedly; OS sleep will not be inhibited on this host",
        )
    }
}
