//! macOS-specific process utilities.

pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = true;
use anyhow::Context as _;
use std::collections::HashMap;
use std::process::Command;

pub(super) use super::unix::{
    close_frozen_payload, configure_process_group, exec_command, kill_process_group,
    restore_environment_value, terminate_process_group, try_wait_status_hook,
};
/// Prove only the absence of the Linux scope backend on this kernel.
/// Native process, worker and tmux custody must still pass their own guards.
pub(super) fn legacy_scope_inventory_clear(
    known: &[crate::process::worker_registry::ScopeOwnership],
) -> anyhow::Result<bool> {
    use crate::process::worker_registry::ScopeOwnership;
    anyhow::ensure!(
        !known
            .iter()
            .any(|scope| matches!(scope, ScopeOwnership::Owned { .. })),
        "Captured Linux scope authority cannot be recovered on this platform"
    );
    let mut name = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: uname receives writable storage of the required size.
    if unsafe { libc::uname(name.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Reading the native kernel identity");
    }
    // SAFETY: uname succeeded and initializes NUL-terminated sysname.
    let name = unsafe { name.assume_init() };
    let kernel = unsafe { std::ffi::CStr::from_ptr(name.sysname.as_ptr()) };
    anyhow::ensure!(
        kernel.to_bytes() == b"Darwin",
        "Cannot prove the Linux scope backend absent on this kernel"
    );
    Ok(true)
}

#[cfg(test)]
mod legacy_scope_recovery_tests {
    use super::*;
    use crate::process::worker_registry::ScopeOwnership;

    #[test]
    fn native_darwin_proof_does_not_discard_a_captured_linux_unit() {
        assert!(legacy_scope_inventory_clear(&[]).unwrap());
        assert!(legacy_scope_inventory_clear(&[ScopeOwnership::Unscoped]).unwrap());
        assert!(legacy_scope_inventory_clear(&[ScopeOwnership::Owned {
            unit: "aoe-session-old-original.scope".into(),
        }])
        .is_err());
    }
}

pub(super) fn open_read_nonblocking(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

pub(super) fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}
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

pub(crate) fn unix_peer_uid(stream: &tokio::net::UnixStream) -> std::io::Result<u32> {
    Ok(stream.peer_cred()?.uid())
}
/// Run the child at background priority so a mass file delete does not saturate
/// `fseventsd` and stall the rest of the machine.
pub(super) fn throttle_child(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `setpriority` is async-signal-safe and the closure allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
            Ok(())
        });
    }
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

/// One `ps -A -ww -E -o command=` fork deciding, for each candidate `i`,
/// whether a live process belongs to it: a whitespace-delimited token exactly
/// equals `env_needles[i]` (anchored, matching the `KEY=VAL` env tokens `-E`
/// appends), and the line contains `cmdline_needles[i]` when both are supplied.
/// An executable needle matches an exact argv-token basename. A candidate with
/// one signal uses that one; otherwise every supplied signal must match.
/// `-ww` disables column
/// truncation; `-E` appends each owner-owned process's environment. If a `ps`
/// build rejects `-E`, the call fails closed to all `false` and recovery falls
/// back to the ledger. Best-effort: a failed `ps` yields all `false`.
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

pub(super) fn runner_group_executing(group: u32) -> anyhow::Result<bool> {
    let mut command = Command::new("ps");
    command
        .args(["-A", "-o", "pid=,pgid=,stat="])
        .env("LC_ALL", "C");
    let output = crate::session::capture::run_with_timeout_limit(
        command,
        std::time::Duration::from_secs(2),
        "runner group census",
        1024 * 1024,
    )?;
    for line in std::str::from_utf8(&output)?.lines() {
        let mut fields = line.split_ascii_whitespace();
        fields
            .next()
            .context("missing process PID")?
            .parse::<u32>()?;
        let process_group = fields
            .next()
            .context("missing process group")?
            .parse::<u32>()?;
        let state = fields.next().context("missing process state")?;
        if process_group == group && !state.starts_with('Z') {
            return Ok(true);
        }
    }
    Ok(false)
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct AuditToken {
    values: [u32; 8],
}

unsafe extern "C" {
    static mach_task_self_: libc::mach_port_t;
    fn task_name_for_pid(
        task: libc::mach_port_t,
        pid: libc::pid_t,
        name: *mut libc::mach_port_t,
    ) -> libc::kern_return_t;
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

fn audit_token(pid: u32) -> anyhow::Result<Option<AuditToken>> {
    let raw_pid = i32::try_from(pid)?;
    anyhow::ensure!(raw_pid > 0, "invalid owned process PID");
    let mut port = 0;
    let task = unsafe { mach_task_self_ };
    let result = unsafe { task_name_for_pid(task, raw_pid, &mut port) };
    if result != 0 {
        if !super::worker::is_pid_alive(pid) {
            return Ok(None);
        }
        if process_membership(pid)?.is_none_or(|info| info.pbi_status == libc::SZOMB) {
            return Ok(None);
        }
        anyhow::bail!("process audit token unavailable: Mach error {result}");
    }
    struct Port(libc::mach_port_t, libc::mach_port_t);
    impl Drop for Port {
        fn drop(&mut self) {
            unsafe {
                mach_port_deallocate(self.0, self.1);
            }
        }
    }
    let _port = Port(task, port);
    let mut token = AuditToken { values: [0; 8] };
    let mut count = 8;
    let result = unsafe { libc::task_info(port, 15, token.values.as_mut_ptr().cast(), &mut count) };
    if result != 0 || count != 8 || token.values[5] != pid {
        if !super::worker::is_pid_alive(pid)
            || process_membership(pid)?.is_none_or(|info| info.pbi_status == libc::SZOMB)
        {
            return Ok(None);
        }
        anyhow::bail!(
            "invalid process audit token: Mach error {result}, count {count}, PID {}",
            token.values[5]
        );
    }
    Ok(Some(token))
}

fn audit_start_id(token: &AuditToken) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(70);
    value.push_str("audit:");
    for word in token.values {
        write!(value, "{word:08x}").unwrap();
    }
    value
}

pub(super) fn process_start_id(pid: u32) -> anyhow::Result<Option<String>> {
    Ok(audit_token(pid)?.as_ref().map(audit_start_id))
}

pub(super) struct ProcessHandle(AuditToken);

impl ProcessHandle {
    pub(super) fn capture(identity: &super::PrimeProcessIdentity) -> anyhow::Result<Option<Self>> {
        anyhow::ensure!(
            identity.start_id.starts_with("audit:"),
            "process lacks incarnation-bound signal authority"
        );
        let Some(token) = audit_token(identity.pid)? else {
            return Ok(None);
        };
        if audit_start_id(&token) != identity.start_id {
            return Ok(None);
        }
        Ok(Some(Self(token)))
    }

    pub(super) fn exited(&self) -> anyhow::Result<bool> {
        Ok(audit_token(self.0.values[5])?
            .is_none_or(|current| current.values[7] != self.0.values[7]))
    }

    pub(super) fn signal(&self, signal: i32) -> anyhow::Result<()> {
        type Signal = unsafe extern "C" fn(*mut AuditToken, i32) -> i32;
        static FUNCTION: std::sync::OnceLock<Option<Signal>> = std::sync::OnceLock::new();
        let function = FUNCTION
            .get_or_init(|| unsafe {
                let library = libc::dlopen(
                    c"/usr/lib/libproc.dylib".as_ptr(),
                    libc::RTLD_LAZY | libc::RTLD_LOCAL,
                );
                if library.is_null() {
                    return None;
                }
                let function = libc::dlsym(library, c"proc_signal_with_audittoken".as_ptr());
                if function.is_null() {
                    libc::dlclose(library);
                    return None;
                }
                // The cached function keeps its library loaded for the process lifetime.
                Some(std::mem::transmute::<*mut libc::c_void, Signal>(function))
            })
            .context("incarnation-bound signalling unavailable on this macOS version")?;
        let mut token = self.0;
        let result = unsafe { function(&mut token, signal) };
        // The kernel reports EPERM when the target starts exiting during the call.
        if result != 0 && result != libc::ESRCH && !(result == libc::EPERM && self.exited()?) {
            return Err(std::io::Error::from_raw_os_error(result))
                .context("signalling owned audit token");
        }
        Ok(())
    }
}

pub(super) fn runner_group_identities(
    group: u32,
) -> anyhow::Result<Vec<super::PrimeProcessIdentity>> {
    let mut command = Command::new("ps");
    command.args(["-A", "-o", "pid=,pgid=,stat="]);
    let output = crate::session::capture::run_with_timeout_limit(
        command,
        std::time::Duration::from_secs(2),
        "runner group identities",
        1024 * 1024,
    )?;
    let mut identities = Vec::new();
    for line in std::str::from_utf8(&output)?.lines() {
        let mut fields = line.split_whitespace();
        let pid = fields.next().context("missing process PID")?.parse()?;
        let pgrp = fields
            .next()
            .context("missing process group")?
            .parse::<u32>()?;
        let zombie = fields
            .next()
            .context("missing process state")?
            .starts_with('Z');
        if pgrp == group && !zombie {
            if let Some(start_id) = process_start_id(pid)? {
                if process_group_id(pid)? == Some(group)
                    && process_start_id(pid)?.as_ref() == Some(&start_id)
                {
                    identities.push(super::PrimeProcessIdentity { pid, start_id });
                }
            }
        }
    }
    Ok(identities)
}
pub(super) fn process_group_id(pid: u32) -> anyhow::Result<Option<u32>> {
    let group = unsafe { libc::getpgid(i32::try_from(pid)?) };
    if group >= 0 {
        return Ok(Some(group as u32));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(None)
    } else {
        Err(error.into())
    }
}

fn process_membership(pid: u32) -> anyhow::Result<Option<libc::proc_bsdinfo>> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid)?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        anyhow::bail!("process membership unavailable: {error}");
    }
    let info = unsafe { info.assume_init() };
    anyhow::ensure!(info.pbi_pid == pid, "unexpected process membership PID");
    Ok(Some(info))
}

pub(super) fn foreign_runner_group_absent(pid: u32) -> anyhow::Result<bool> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid)?,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return Err(std::io::Error::last_os_error())
            .context("runner short process metadata unavailable");
    }
    let info = unsafe { info.assume_init() };
    anyhow::ensure!(info.pbsi_pid == pid, "unexpected runner metadata PID");
    let uid = unsafe { libc::geteuid() };
    let ruid = unsafe { libc::getuid() };
    Ok((info.pbsi_uid != uid || info.pbsi_ruid != ruid) && !runner_group_executing(pid)?)
}
pub(super) fn process_tree_samples(
    root: &super::PrimeProcessIdentity,
) -> anyhow::Result<Vec<super::PrimeProcessSample>> {
    let mut command = Command::new("ps");
    command.args(["-A", "-o", "pid=,ppid=,stat="]);
    let output = crate::session::capture::run_with_timeout_limit(
        command,
        std::time::Duration::from_secs(2),
        "tool process census",
        1024 * 1024,
    )?;
    let mut candidates = Vec::new();
    for line in std::str::from_utf8(&output)?.lines() {
        let mut fields = line.split_ascii_whitespace();
        let pid = fields.next().context("missing process PID")?.parse()?;
        let parent = fields.next().context("missing process parent")?.parse()?;
        if !fields
            .next()
            .context("missing process state")?
            .starts_with('Z')
        {
            candidates.push(super::PrimeProcessSample {
                identity: super::PrimeProcessIdentity {
                    pid,
                    start_id: if pid == root.pid {
                        root.start_id.clone()
                    } else {
                        String::new()
                    },
                },
                parent,
                marked: false,
            });
        }
    }
    let selected = super::prime_selection_mask(&candidates, std::slice::from_ref(root));
    let mut samples = Vec::new();
    for (mut sample, selected) in candidates.into_iter().zip(selected) {
        if !selected {
            continue;
        }
        let pid = sample.identity.pid;
        let Some(token) = audit_token(pid)? else {
            continue;
        };
        let membership = process_membership(pid)?.context("tool process membership unavailable")?;
        anyhow::ensure!(
            audit_token(pid)? == Some(token),
            "tool process incarnation changed during census"
        );
        sample.identity.start_id = audit_start_id(&token);
        sample.parent = membership.pbi_ppid;
        samples.push(sample);
    }
    anyhow::ensure!(
        samples.iter().any(|sample| sample.identity == *root),
        "tool root omitted from census"
    );
    Ok(samples)
}

pub(super) fn prime_processes(
    marker: &str,
    socket: &std::path::Path,
    known: &[super::PrimeProcessIdentity],
) -> anyhow::Result<Vec<super::PrimeProcessIdentity>> {
    let mut command = Command::new("ps");
    command
        .args(["-A", "-o", "pid=,ppid=,stat=,lstart="])
        .env("LC_ALL", "C")
        .env("LC_TIME", "C")
        .env("LANG", "C")
        .env("TZ", "UTC");
    let output = crate::session::capture::run_with_timeout_limit(
        command,
        std::time::Duration::from_secs(2),
        "Prime process census",
        1024 * 1024,
    )?;
    let mut samples = Vec::new();
    let mut bytes = Vec::new();
    let mut legacy_observed = Vec::new();
    let mut known_zombies = Vec::new();
    for line in std::str::from_utf8(&output)?.lines() {
        let mut rest = line;
        let mut take = || -> anyhow::Result<&str> {
            let (value, tail) = rest
                .trim_start()
                .split_once(char::is_whitespace)
                .context("incomplete process census row")?;
            rest = tail;
            Ok(value)
        };
        let pid = take()?.parse::<u32>()?;
        let parent = take()?.parse::<u32>()?;
        let zombie = take()?.starts_with('Z');
        if zombie && known.iter().any(|identity| identity.pid == pid) {
            known_zombies.push(pid);
        }
        if zombie || pid == std::process::id() {
            continue;
        }
        let start = rest.trim();
        anyhow::ensure!(!start.is_empty(), "missing process start identity");
        let marked = match prime_launch_marker(pid, marker, socket, &mut bytes) {
            Ok(marked) => marked,
            Err(error) if known.iter().any(|identity| identity.pid == pid) => return Err(error),
            Err(_) => false,
        };
        samples.push(super::PrimeProcessSample {
            identity: super::PrimeProcessIdentity {
                pid,
                start_id: format!("ps:{start}"),
            },
            parent,
            marked,
        });
    }
    let candidate_known: Vec<_> = samples
        .iter()
        .filter(|sample| {
            known
                .iter()
                .any(|identity| identity.pid == sample.identity.pid)
        })
        .map(|sample| sample.identity.clone())
        .collect();
    let candidates = super::prime_selection_mask(&samples, &candidate_known);
    let mut attested = Vec::new();
    for (mut sample, candidate) in samples.into_iter().zip(candidates) {
        if !candidate {
            continue;
        }
        let pid = sample.identity.pid;
        let Some(token) = audit_token(pid)? else {
            continue;
        };
        let Some(membership) = process_membership(pid)? else {
            continue;
        };
        let Some(after) = audit_token(pid)? else {
            continue;
        };
        anyhow::ensure!(
            after == token,
            "owned process incarnation changed during census"
        );
        if !sample.marked && known.iter().any(|known| known == &sample.identity) {
            legacy_observed.push(super::PrimeProcessIdentity {
                pid,
                start_id: audit_start_id(&token),
            });
        }
        sample.identity.start_id = audit_start_id(&token);
        sample.parent = membership.pbi_ppid;
        attested.push(sample);
    }
    let samples = attested;
    for identity in known {
        if !samples
            .iter()
            .any(|sample| sample.identity.pid == identity.pid)
        {
            anyhow::ensure!(
                known_zombies.contains(&identity.pid) || !super::worker::is_pid_alive(identity.pid),
                "owned Prime process {} omitted from census",
                identity.pid
            );
        }
    }
    let owned = super::select_stable_prime_processes(samples, known)?;
    for identity in legacy_observed {
        anyhow::ensure!(
            owned.contains(&identity)
                || process_start_id(identity.pid)?.as_deref() != Some(&identity.start_id),
            "legacy worker birth is unproven; retain ownership until external settlement"
        );
    }
    Ok(owned)
}

fn prime_launch_marker(
    pid: u32,
    marker: &str,
    socket: &std::path::Path,
    bytes: &mut Vec<u8>,
) -> anyhow::Result<bool> {
    let pid = i32::try_from(pid)?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut length = 0usize;
    let queried = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if queried != 0 {
        return Err(std::io::Error::last_os_error()).context("reading process argument size");
    }
    anyhow::ensure!(
        (4..=256 * 1024).contains(&length),
        "invalid process argument size"
    );
    bytes.clear();
    bytes.reserve(length);
    let capacity = bytes.capacity();
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return Err(std::io::Error::last_os_error()).context("reading process arguments");
    }
    anyhow::ensure!(
        (4..=capacity).contains(&length),
        "invalid process argument result"
    );
    // sysctl initialized exactly length bytes on success.
    unsafe {
        bytes.set_len(length);
    }
    let count = i32::from_ne_bytes(bytes[..4].try_into()?);
    anyhow::ensure!(
        (0..=4096).contains(&count),
        "invalid process argument count"
    );
    let mut tail = &bytes[4..];
    let executable_end = tail
        .iter()
        .position(|byte| *byte == 0)
        .context("unterminated process executable")?;
    tail = &tail[executable_end + 1..];
    while tail.first() == Some(&0) {
        tail = &tail[1..];
    }
    let mut routed = false;
    for _ in 0..count {
        let end = tail
            .iter()
            .position(|byte| *byte == 0)
            .context("unterminated process argument")?;
        routed |= &tail[..end] == socket.as_os_str().as_encoded_bytes();
        tail = &tail[end + 1..];
    }
    Ok(routed
        || tail
            .split(|byte| *byte == 0)
            .any(|entry| entry == marker.as_bytes()))
}

/// Sample system memory via `sysctl` and `vm_stat`, matching this module's
/// existing shell-out convention. Populates total/available RAM and the native
/// memory-pressure level; PSI has no macOS analogue and stays `None`.
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

pub(super) fn foreground_arguments(shell_pid: u32) -> Vec<super::ProcessArguments> {
    let Some(foreground) = get_foreground_pid(shell_pid) else {
        return Vec::new();
    };
    let group = unsafe { libc::getpgid(foreground as i32) };
    if group < 0 {
        return Vec::new();
    }
    collect_pid_tree(foreground)
        .into_iter()
        .filter_map(|pid| {
            if unsafe { libc::getpgid(pid as i32) } != group {
                return None;
            }
            let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
            let mut length = 0usize;
            if unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    3,
                    std::ptr::null_mut(),
                    &mut length,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0
                || length > 256 * 1024
            {
                return None;
            }
            let mut bytes = vec![0u8; length];
            if unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    3,
                    bytes.as_mut_ptr().cast(),
                    &mut length,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0
            {
                return None;
            }
            bytes.truncate(length);
            let count = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
            if !(1..=4096).contains(&count) {
                return None;
            }
            let tail = bytes.get(4..)?;
            let executable_end = tail.iter().position(|byte| *byte == 0)?;
            let padding = tail.get(executable_end + 1..)?;
            let start = padding.iter().position(|byte| *byte != 0)?;
            let mut arguments = padding[start..].split(|byte| *byte == 0);
            let argv: Vec<String> = arguments
                .by_ref()
                .take(count as usize)
                .map(|word| std::str::from_utf8(word).map(str::to_owned))
                .collect::<Result<_, _>>()
                .ok()?;

            Some(super::ProcessArguments {
                argv,
                stdin_is_tty: None,
            })
        })
        .collect()
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

/// Prevents user-idle system sleep by holding a `caffeinate` child. `-i`
/// inhibits system idle sleep only, so the display still sleeps normally.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_runner_probe_requires_an_empty_original_group_and_keeps_native_tokens_strict() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let available = Command::new("sudo")
            .args(["-n", "-u", "nobody", "true"])
            .status();
        if !available.is_ok_and(|status| status.success()) {
            eprintln!("skipping foreign-runner probe: passwordless sudo unavailable");
            return;
        }
        struct Foreign(std::process::Child);
        impl Drop for Foreign {
            fn drop(&mut self) {
                self.0.stdin.take();
                let _ = self.0.wait();
            }
        }
        for own_group in [false, true] {
            let script = format!(
                "import os,sys\npid=os.fork()\nif pid == 0:\n    {}\n    print(os.getpid(),flush=True)\n    sys.stdin.read()\n    os._exit(0)\nelse:\n    _,status=os.waitpid(pid,0)\n    sys.exit(os.waitstatus_to_exitcode(status))",
                if own_group { "os.setsid()" } else { "pass" }
            );
            let mut child = Foreign(
                Command::new("sudo")
                    .args(["-n", "-u", "nobody", "python3", "-c", &script])
                    .current_dir("/tmp")
                    .process_group(0)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let mut line = String::new();
            std::io::BufReader::new(child.0.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let pid = line.trim().parse::<u32>().unwrap();
            assert!(
                process_start_id(pid).is_err(),
                "generic native ownership must remain unproven"
            );
            assert_eq!(foreign_runner_group_absent(pid).unwrap(), !own_group);
            if own_group {
                assert!(super::super::runner_birth_identity(pid).is_err());
            } else {
                assert!(super::super::runner_birth_identity(pid).unwrap().is_none());
                let guard = super::super::RunnerProcessGuard::capture(pid, None).unwrap();
                let mut words = [0; 8];
                words[5] = pid;
                guard.members.lock().push((
                    super::super::PrimeProcessIdentity {
                        pid,
                        start_id: audit_start_id(&AuditToken { values: words }),
                    },
                    ProcessHandle(AuditToken { values: words }),
                ));
                let auxiliary = Command::new("cat")
                    .process_group(0)
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                struct Auxiliary(std::process::Child);
                impl Drop for Auxiliary {
                    fn drop(&mut self) {
                        let _ = self.0.kill();
                        let _ = self.0.wait();
                    }
                }
                let auxiliary_identity = super::super::PrimeProcessIdentity {
                    pid: auxiliary.id(),
                    start_id: process_start_id(auxiliary.id()).unwrap().unwrap(),
                };
                let auxiliary_handle = ProcessHandle::capture(&auxiliary_identity)
                    .unwrap()
                    .unwrap();
                let mut auxiliary = Auxiliary(auxiliary);
                guard
                    .members
                    .lock()
                    .push((auxiliary_identity, auxiliary_handle));
                assert!(
                    guard.executing().unwrap(),
                    "a known detached member must remain owned"
                );
                guard.signal(libc::SIGTERM).unwrap();
                assert!(!auxiliary.0.wait().unwrap().success());
                assert!(!guard.executing().unwrap());
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "the foreign execution was signalled"
            );
            drop(child.0.stdin.take());
            assert!(child.0.wait().unwrap().success());
        }
    }

    #[test]
    fn prime_quiescence_accepts_observed_owned_zombies() {
        use std::io::{BufRead as _, Write as _};
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let marker = format!("TMPDIR={}", root.path().display());
        let socket = root.path().join("daemon.sock");
        let mut child = OwnedChild(
            Command::new("sh")
                .args(["-c", "printf 'ready\n'; IFS= read -r line"])
                .env("TMPDIR", root.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let owned = prime_processes(&marker, &socket, &[])
            .unwrap()
            .into_iter()
            .find(|identity| identity.pid == child.0.id())
            .unwrap();
        child.0.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let mut probe = Command::new("ps");
            probe.args(["-p", &child.0.id().to_string(), "-o", "stat="]);
            let state = crate::session::capture::run_with_timeout_limit(
                probe,
                std::time::Duration::from_secs(1),
                "owned zombie observation",
                1024,
            )
            .unwrap();
            if std::str::from_utf8(&state).unwrap().trim().starts_with('Z') {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owned child did not become a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let remaining = prime_processes(&marker, &socket, std::slice::from_ref(&owned)).unwrap();
        assert!(!remaining.contains(&owned));
        assert!(child.0.wait().unwrap().success());
    }

    #[test]
    fn attested_runner_root_is_retained_once_across_refreshes() {
        use std::os::unix::process::CommandExt as _;
        struct Leader(std::process::Child);
        impl Drop for Leader {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let leader = Leader(
            Command::new("sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let pid = leader.0.id();
        let birth = super::super::runner_birth_identity(pid)
            .unwrap()
            .expect("live leader has a birth identity");
        let root = super::super::PrimeProcessIdentity {
            pid,
            start_id: birth.start_id.clone(),
        };
        assert!(
            runner_group_identities(pid).unwrap().contains(&root),
            "enumeration must include the live leader"
        );

        let guard =
            super::super::RunnerProcessGuard::capture(pid, Some(birth.fingerprint())).unwrap();
        let roots = |guard: &super::super::RunnerProcessGuard| {
            guard
                .members
                .lock()
                .iter()
                .filter(|(identity, _)| identity.pid == pid)
                .count()
        };
        assert_eq!(roots(&guard), 1, "root duplicated by capture");
        for _ in 0..3 {
            guard.refresh(&mut guard.members.lock()).unwrap();
            assert_eq!(roots(&guard), 1, "root duplicated by refresh");
        }
    }
}
