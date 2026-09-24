//! macOS-specific process utilities.

pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = true;
use std::collections::HashMap;
use std::process::Command;

pub(super) use super::unix::{
    configure_process_group, kill_process_group, terminate_process_group,
};
pub(super) fn rename_exclusive(
    source_dir: &std::os::fd::OwnedFd,
    source: &std::ffi::OsStr,
    destination_dir: &std::os::fd::OwnedFd,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use nix::NixPath;
    use std::os::fd::AsRawFd;

    let result = source.with_nix_path(|source| {
        destination.with_nix_path(|destination| {
            // SAFETY: both descriptors remain owned for the call and NixPath
            // supplies live NUL-terminated names. RENAME_EXCL never replaces.
            nix::errno::Errno::result(unsafe {
                libc::renameatx_np(
                    source_dir.as_raw_fd(),
                    source.as_ptr(),
                    destination_dir.as_raw_fd(),
                    destination.as_ptr(),
                    libc::RENAME_EXCL,
                )
            })
            .map(|_| ())
        })
    })??;
    result.map_err(std::io::Error::from)
}

pub(super) fn collect_pid_tree(pid: u32) -> Vec<u32> {
    let children_map = build_children_map();
    let mut pids = vec![pid];
    super::collect_descendants_from_map(pid, &children_map, &mut pids);
    pids
}

pub(super) fn build_children_map() -> HashMap<u32, Vec<u32>> {
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

/// `-E` appends each owned process's environment; if `ps` rejects it, every candidate is `false`.
pub(super) fn processes_matching(
    env_needles: &[String],
    cmdline_needles: &[Option<String>],
    executable_needles: &[Option<String>],
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
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for i in 0..n {
            if found[i] {
                continue;
            }
            let env_position = (!env_needles[i].is_empty())
                .then(|| tokens.iter().position(|token| *token == env_needles[i]))
                .flatten();
            let env_hit = env_position.is_some();
            let cmd_hit = cmdline_needles[i]
                .as_deref()
                .is_some_and(|s| !s.is_empty() && line.contains(s));
            let command_end = env_position.unwrap_or(tokens.len());
            let executable_hit = executable_needles[i].as_deref().is_some_and(|needle| {
                !needle.is_empty()
                    && tokens[..command_end].iter().any(|token| {
                        std::path::Path::new(token)
                            .file_name()
                            .and_then(|value| value.to_str())
                            == Some(needle)
                    })
            });
            let has_env = !env_needles[i].is_empty();
            let has_cmd = cmdline_needles[i]
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let has_executable = executable_needles[i]
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let matched = (has_env || has_cmd || has_executable)
                && (!has_env || env_hit)
                && (!has_cmd || cmd_hit)
                && (!has_executable || executable_hit);
            if matched {
                found[i] = true;
            }
        }
    }
    found
}

pub(super) fn sample_memory() -> super::metrics::MemorySample {
    // A vm_stat failure alone would otherwise read as a false 100% used.
    let (total_bytes, available_bytes) = match (
        sysctl_u64("hw.memsize"),
        read_vm_stat().and_then(|s| parse_vm_stat_available(&s)),
    ) {
        (Some(total), Some(available)) => (total, available),
        _ => (0, 0),
    };

    super::metrics::MemorySample {
        total_bytes,
        available_bytes,
        psi_mem_some_avg10: None,
        psi_io_some_avg10: None,
        macos_pressure_level: sysctl_u64("kern.memorystatus_vm_pressure_level").map(|v| v as u8),
    }
}

pub(super) fn sample_system() -> super::metrics::SystemReading {
    let cpu = Command::new("ps")
        .args(["-A", "-o", "%cpu="])
        .output()
        .ok()
        .map(|output| {
            let total_percent: f64 = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<f64>().ok())
                .sum();
            let cpus = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1);
            (total_percent / 100.0 / cpus as f64).clamp(0.0, 1.0)
        });
    let load = sysctl_string("vm.loadavg").and_then(|value| {
        let values: Vec<f64> = value
            .replace(['{', '}'], "")
            .split_whitespace()
            .filter_map(|part| part.parse().ok())
            .collect();
        (values.len() >= 3).then(|| [values[0], values[1], values[2]])
    });
    let swap = sysctl_string("vm.swapusage")
        .map(|value| parse_swap(&value))
        .unwrap_or((0, 0));
    (None, cpu, load, swap)
}

pub(super) fn process_snapshot() -> Vec<super::metrics::ProcessRecord> {
    let Ok(output) = Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,time=,lstart="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_record)
        .collect()
}

fn parse_process_record(line: &str) -> Option<super::metrics::ProcessRecord> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let started_at = fields.get(4..)?;
    if started_at.is_empty() {
        return None;
    }
    Some(super::metrics::ProcessRecord {
        pid: fields.first()?.parse().ok()?,
        ppid: fields.get(1)?.parse().ok()?,
        rss_bytes: fields.get(2)?.parse::<u64>().ok()?.saturating_mul(1024),
        cpu_seconds: parse_ps_time(fields.get(3)?)?,
        start_id: stable_process_start_id(started_at),
    })
}

fn stable_process_start_id(fields: &[&str]) -> u64 {
    fields
        .iter()
        .flat_map(|field| field.bytes().chain(std::iter::once(0)))
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}

fn parse_swap(value: &str) -> (u64, u64) {
    let parse = |label: &str| {
        let raw = value
            .split_whitespace()
            .skip_while(|part| *part != label)
            .nth(2)?;
        let (number, multiplier) = if let Some(number) = raw.strip_suffix('G') {
            (number, 1u64 << 30)
        } else if let Some(number) = raw.strip_suffix('M') {
            (number, 1u64 << 20)
        } else if let Some(number) = raw.strip_suffix('K') {
            (number, 1u64 << 10)
        } else {
            (raw, 1)
        };
        Some((number.parse::<f64>().ok()? * multiplier as f64) as u64)
    };
    (parse("total").unwrap_or(0), parse("used").unwrap_or(0))
}

fn parse_ps_time(value: &str) -> Option<f64> {
    let (days, clock) = if let Some((days, clock)) = value.split_once('-') {
        (days.parse::<f64>().ok()?, clock)
    } else {
        (0.0, value)
    };
    let parts: Vec<f64> = clock
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] => minutes * 60.0 + seconds,
        [hours, minutes, seconds] => hours * 3600.0 + minutes * 60.0 + seconds,
        _ => return None,
    };
    Some(days * 86_400.0 + seconds)
}

fn sysctl_string(key: &str) -> Option<String> {
    let out = Command::new("sysctl").args(["-n", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn sysctl_u64(key: &str) -> Option<u64> {
    sysctl_string(key)?.parse().ok()
}

fn read_vm_stat() -> Option<String> {
    let out = Command::new("vm_stat").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// (free + inactive) pages; an approximation, so the band uses the native pressure level.
fn parse_vm_stat_available(vm_stat: &str) -> Option<u64> {
    let page_size = parse_vm_stat_page_size(vm_stat)?;
    let free = parse_vm_stat_pages(vm_stat, "Pages free")?;
    let inactive = parse_vm_stat_pages(vm_stat, "Pages inactive")?;
    Some((free + inactive).saturating_mul(page_size))
}

fn parse_vm_stat_page_size(vm_stat: &str) -> Option<u64> {
    let line = vm_stat.lines().next()?;
    let after = line.split("page size of").nth(1)?;
    after.split_whitespace().next()?.parse().ok()
}

fn parse_vm_stat_pages(vm_stat: &str, key: &str) -> Option<u64> {
    for line in vm_stat.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        return rest.trim().trim_end_matches('.').parse().ok();
    }
    None
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    const VM_STAT: &str = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                              123456.
Pages active:                            654321.
Pages inactive:                          200000.
Pages speculative:                        10000.
Pages wired down:                        300000.
";

    #[test]
    fn test_parse_vm_stat_available() {
        assert_eq!(parse_vm_stat_page_size(VM_STAT), Some(16384));
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages free"), Some(123456));
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages inactive"), Some(200000));
        assert_eq!(
            parse_vm_stat_available(VM_STAT),
            Some((123456 + 200000) * 16384)
        );
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages nonexistent"), None);
    }

    #[test]
    fn process_record_parses_stable_identity_and_cpu_time() {
        let time_cases = [
            ("03:04", Some(184.0)),
            ("02:03:04", Some(7_384.0)),
            ("1-02:03:04", Some(93_784.0)),
            ("1-02:x:04", None),
            ("1-02:03:bad", None),
        ];
        for (value, expected) in time_cases {
            assert_eq!(parse_ps_time(value), expected, "CPU time {value}");
        }

        let line = "42 1 512 1-02:03:04 Thu Aug 13 12:34:56 2026";
        let record = parse_process_record(line).unwrap();
        assert_eq!(record.cpu_seconds, 93_784.0);
        assert_ne!(record.start_id, 0);
        assert_eq!(
            record.start_id,
            parse_process_record(line).unwrap().start_id
        );
    }
}

/// `kern.boottime` shifts on clock steps; the session UUID does not.
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

pub(super) fn parent_and_argv0(pid: u32) -> Option<(u32, String)> {
    let output = Command::new("ps")
        .args(["-o", "ppid=,args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let ppid = fields.next()?.parse().ok()?;
    Some((ppid, fields.next().unwrap_or_default().to_string()))
}

pub fn get_foreground_pid(shell_pid: u32) -> Option<u32> {
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

    find_process_in_group(tpgid as u32).or(Some(shell_pid))
}

fn find_process_in_group(pgrp: u32) -> Option<u32> {
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

pub(super) struct CaffeinateInhibitor {
    child: Option<std::process::Child>,
}

impl CaffeinateInhibitor {
    pub(super) fn new() -> Self {
        Self { child: None }
    }
}

impl super::SleepInhibit for CaffeinateInhibitor {
    fn acquire(&mut self) -> anyhow::Result<()> {
        if super::sleep_inhibit_unavailable() {
            return Ok(());
        }
        // `-w <daemon_pid>` releases the assertion even when the daemon dies without `Drop`.
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
