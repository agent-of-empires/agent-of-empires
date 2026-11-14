//! Fixed plugin Git/build budget and incarnation-bound writer settlement.
use std::io::Read;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

/// Git progress and error text is diagnostic only; commit identities and ref
/// listings are never truncated, they feed the resolved commit.
pub(super) const DIAGNOSTIC_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub(super) struct Budget {
    deadline: Instant,
    #[cfg(test)]
    expired: Option<&'static std::sync::atomic::AtomicBool>,
}

#[derive(Debug, thiserror::Error)]
#[error("plugin Git/build execution exhausted its two-hour operation budget")]
pub(super) struct Exhausted;

#[derive(Debug, thiserror::Error)]
#[error("plugin recovery required: {0}; mutation exclusion and recovery evidence retained")]
pub(super) struct RecoveryRequired(pub String);

pub(super) fn catch_panic<T>(run: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(result) => result,
        Err(payload) => {
            // An arbitrary payload's destructor must not unwind past recovery.
            std::mem::forget(payload);
            Err(
                RecoveryRequired("Plugin job panicked; operation outcome unknown".to_owned())
                    .into(),
            )
        }
    }
}

impl Budget {
    pub(super) fn new() -> Self {
        Self {
            deadline: Instant::now() + super::EXECUTION_BUDGET,
            #[cfg(test)]
            expired: None,
        }
    }

    #[cfg(test)]
    pub(super) fn controlled_for_test(expired: &'static std::sync::atomic::AtomicBool) -> Self {
        Self {
            expired: Some(expired),
            ..Self::new()
        }
    }

    fn is_expired(self) -> bool {
        #[cfg(test)]
        if let Some(expired) = self.expired {
            return expired.load(std::sync::atomic::Ordering::SeqCst);
        }
        Instant::now() >= self.deadline
    }

    pub(super) fn run(self, command: &mut Command) -> Result<Output> {
        self.run_captured(command, true)
    }

    /// Clone and checkout stdout is discarded by every caller, so it must not
    /// be buffered. Diagnostics stay bounded either way.
    pub(super) fn run_discarding_stdout(self, command: &mut Command) -> Result<Output> {
        self.run_captured(command, false)
    }

    fn run_captured(self, command: &mut Command, keep_stdout: bool) -> Result<Output> {
        if self.is_expired() {
            return Err(Exhausted.into());
        }
        let mut child = crate::process::OwnedProcessGroup::spawn(command)
            .context("spawning plugin command process group")?;
        let pid = child.id();
        let stdout = child.take_stdout();
        let stderr = child.take_stderr();
        #[cfg(test)]
        if let Some(dir) = command.get_current_dir() {
            super::fetch::test_gate::reach(dir, "command-capture");
        }
        let mut descendants = match crate::process::OwnedProcessTree::capture(&child) {
            Ok(tree) => tree,
            Err(error) => {
                std::mem::forget((child, stdout, stderr));
                return Err(RecoveryRequired(format!(
                    "cannot attest descendants of command {pid}: {error:#}"
                ))
                .into());
            }
        };
        // Drain captured Git output concurrently. Build stdio remains attached to its log.
        fn reader<R: Read + Send + 'static>(
            pipe: Option<R>,
            keep: Option<usize>,
        ) -> Option<std::thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>> {
            pipe.map(|mut pipe| {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    let mut retained = Vec::new();
                    let mut truncated = false;
                    loop {
                        let read = pipe.read(&mut buf)?;
                        if read == 0 {
                            return Ok((retained, truncated));
                        }
                        let room = keep.map_or(0, |limit| limit.saturating_sub(retained.len()));
                        let take = room.min(read);
                        retained.extend_from_slice(&buf[..take]);
                        truncated |= take < read;
                    }
                })
            })
        }
        let stdout = reader(stdout, keep_stdout.then_some(usize::MAX));
        let stderr = reader(stderr, Some(DIAGNOSTIC_LIMIT));
        let mut timed_out = false;
        let mut next_census = Instant::now() + Duration::from_secs(1);
        loop {
            #[cfg(test)]
            if let Some(dir) = command.get_current_dir() {
                super::fetch::test_gate::reach(dir, "command-observe");
            }
            let expired = self.is_expired();
            if expired || Instant::now() >= next_census {
                if let Err(error) = descendants.refresh() {
                    std::mem::forget(descendants);
                    std::mem::forget(child);
                    return Err(RecoveryRequired(format!(
                        "cannot observe command {pid}: {error:#}"
                    ))
                    .into());
                }
                #[cfg(test)]
                if let Some(dir) = command.get_current_dir() {
                    super::fetch::test_gate::reach(dir, "command-census");
                }
                next_census = Instant::now() + Duration::from_secs(1);
            }
            if expired {
                timed_out = true;
                break;
            }
            #[cfg(test)]
            if let Some(dir) = command.get_current_dir() {
                super::fetch::test_gate::reach(dir, "leader-exit-observe");
            }
            match child.exited() {
                Ok(true) => break,
                Ok(false) => std::thread::sleep(Duration::from_millis(25)),
                Err(error) => {
                    std::mem::forget(descendants);
                    std::mem::forget(child);
                    return Err(RecoveryRequired(format!(
                        "cannot wait for command {pid}: {error}"
                    ))
                    .into());
                }
            }
        }
        // A completed leader can leave writers behind. Settle them on success too.
        if let Err(error) = descendants.settle(Duration::from_millis(250)) {
            std::mem::forget(descendants);
            std::mem::forget(child);
            return Err(RecoveryRequired(format!(
                "command {pid} descendant settlement unproven: {error:#}"
            ))
            .into());
        }
        drop(descendants);
        if let Err(error) = child.settle(Duration::from_millis(250)) {
            std::mem::forget(child);
            return Err(
                RecoveryRequired(format!("command {pid} settlement unproven: {error:#}")).into(),
            );
        }
        let status = child.reap().map_err(|error| {
            RecoveryRequired(format!("command {pid} final reap unproven: {error}"))
        })?;
        fn finish(
            reader: Option<std::thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>>,
        ) -> Result<(Vec<u8>, bool)> {
            match reader {
                Some(reader) => reader
                    .join()
                    .map_err(|_| anyhow!("plugin output reader panicked"))?
                    .map_err(Into::into),
                None => Ok((Vec::new(), false)),
            }
        }
        let stdout = finish(stdout)?.0;
        let (mut stderr, truncated) = finish(stderr)?;
        if truncated {
            stderr.extend_from_slice(
                format!("\n[plugin diagnostics truncated at {DIAGNOSTIC_LIMIT} bytes]").as_bytes(),
            );
        }
        if timed_out {
            return Err(Exhausted.into());
        }
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

pub(super) fn needs_recovery<T>(result: &Result<T>) -> bool {
    result
        .as_ref()
        .err()
        .is_some_and(|error| error.downcast_ref::<RecoveryRequired>().is_some())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn leader_exit_during_census_does_not_require_manual_recovery() {
        static EXPIRED: AtomicBool = AtomicBool::new(false);
        EXPIRED.store(false, Ordering::SeqCst);
        struct ExpireOnDrop;
        impl Drop for ExpireOnDrop {
            fn drop(&mut self) {
                EXPIRED.store(true, Ordering::SeqCst);
            }
        }
        let _expire = ExpireOnDrop;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("leader-pid");
        let release = dir.path().join("release-leader");
        let mut observe = super::super::fetch::test_gate::install(dir.path(), "command-observe");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "printf %s $$ > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done",
                "leader",
            ])
            .arg(&pid_file)
            .arg(&release)
            .current_dir(dir.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let task = tokio::task::spawn_blocking(move || {
            Budget::controlled_for_test(&EXPIRED).run(&mut command)
        });
        tokio::time::timeout(Duration::from_secs(5), &mut observe.entered)
            .await
            .unwrap()
            .unwrap();
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|text| text.parse::<u32>().ok())
                {
                    return pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let birth = crate::process::process_birth_identity(pid)
            .unwrap()
            .unwrap();
        let process = crate::process::CapturedProcess::capture(pid, &birth)
            .unwrap()
            .unwrap();
        struct Cleanup(crate::process::CapturedProcessTree);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.settle(Duration::ZERO);
            }
        }
        let _cleanup = Cleanup(crate::process::CapturedProcessTree::capture(pid).unwrap());
        EXPIRED.store(true, Ordering::SeqCst);
        std::fs::write(release, b"finish").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !process.exited().unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            crate::process::process_birth_identity(pid)
                .unwrap()
                .is_some(),
            "the command remains unreaped during the gated census"
        );
        observe.release();
        let error = task.await.unwrap().unwrap_err();
        assert!(
            error.downcast_ref::<Exhausted>().is_some(),
            "a completed leader needs ordinary deadline settlement, not manual recovery: {error:#}"
        );
    }

    #[tokio::test]
    async fn late_same_group_child_settles_before_normal_result() {
        static EXPIRED: AtomicBool = AtomicBool::new(false);
        EXPIRED.store(false, Ordering::SeqCst);
        struct ExpireOnDrop;
        impl Drop for ExpireOnDrop {
            fn drop(&mut self) {
                EXPIRED.store(true, Ordering::SeqCst);
            }
        }
        let _expire = ExpireOnDrop;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut observe = super::super::fetch::test_gate::install(&root, "leader-exit-observe");
        let mut command = Command::new("sh");
        command.args(["-c",
            "printf %s $$ > leader; while [ ! -f start ]; do sleep 0.01; done; sleep 300 & printf %s $! > late; while [ ! -f release ]; do sleep 0.01; done; exit 0"])
            .current_dir(&root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let task = tokio::task::spawn_blocking(move || {
            Budget::controlled_for_test(&EXPIRED).run(&mut command)
        });
        tokio::time::timeout(Duration::from_secs(5), &mut observe.entered)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(root.join("start"), b"").unwrap();
        async fn captured(path: &std::path::Path) -> (u32, crate::process::CapturedProcess) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(pid) = std::fs::read_to_string(path)
                        .ok()
                        .and_then(|s| s.parse().ok())
                    {
                        if let Some(birth) = crate::process::process_birth_identity(pid).unwrap() {
                            if let Some(process) =
                                crate::process::CapturedProcess::capture(pid, &birth).unwrap()
                            {
                                return (pid, process);
                            }
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap()
        }
        let (_, leader) = captured(&root.join("leader")).await;
        std::fs::write(root.join("release"), b"").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !leader.exited().unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // The leader publishes the background job's pid between fork and exec, and the
        // macOS process identity changes on exec, so capture only once the leader is
        // gone: the run is still parked at the gate and has not settled the group yet.
        let (late_pid, late) = captured(&root.join("late")).await;
        assert!(
            !late.exited().unwrap(),
            "late group child pid={late_pid} must survive until the settle pass"
        );
        observe.release();
        let output = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert!(
            late.exited().unwrap(),
            "late uncaptured group writer must physically exit"
        );
    }

    #[test]
    fn git_diagnostics_are_bounded_and_clone_stdout_is_discarded() {
        let mut loud = Command::new("sh");
        loud.args([
            "-c",
            "dd if=/dev/zero bs=1024 count=512 2>/dev/null | tr '\\0' 'e' >&2; printf stdout",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
        let output = Budget::new().run(&mut loud).unwrap();
        assert!(output.status.success());
        // Retained identity output stays complete.
        assert_eq!(output.stdout, b"stdout");
        assert!(
            output.stderr.len() <= DIAGNOSTIC_LIMIT + 128,
            "{}",
            output.stderr.len()
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("[plugin diagnostics truncated"));

        let mut noisy = Command::new("sh");
        noisy
            .args([
                "-c",
                "dd if=/dev/zero bs=1024 count=512 2>/dev/null | tr '\\0' 'x'",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = Budget::new().run_discarding_stdout(&mut noisy).unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
    }
}
