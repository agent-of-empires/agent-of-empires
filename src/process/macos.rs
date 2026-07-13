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
