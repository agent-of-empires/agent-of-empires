//! Fixed plugin Git/build budget and incarnation-bound writer settlement.
use std::io::Read;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

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
        if self.is_expired() {
            return Err(Exhausted.into());
        }
        let mut child = crate::process::OwnedProcessGroup::spawn(command)
            .context("spawning plugin command process group")?;
        let pid = child.id();
        let mut descendants = match crate::process::CapturedProcessTree::capture(pid) {
            Ok(tree) => Some(tree),
            Err(error) => match child.exited() {
                Ok(true) => None,
                outcome => {
                    std::mem::forget(child);
                    return Err(RecoveryRequired(format!(
                        "cannot attest descendants of command {pid}: {error:#}; leader status: {outcome:?}"
                    )).into());
                }
            },
        };
        // Drain captured Git output concurrently. Build stdio remains attached to its log.
        fn reader<R: Read + Send + 'static>(
            pipe: Option<R>,
        ) -> Option<std::thread::JoinHandle<std::io::Result<Vec<u8>>>> {
            pipe.map(|mut pipe| {
                std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    pipe.read_to_end(&mut bytes)?;
                    Ok(bytes)
                })
            })
        }
        let stdout = reader(child.take_stdout());
        let stderr = reader(child.take_stderr());
        let mut timed_out = false;
        let mut next_census = Instant::now() + Duration::from_secs(1);
        loop {
            #[cfg(test)]
            if let Some(dir) = command.get_current_dir() {
                super::fetch::test_gate::reach(dir, "command-observe");
            }
            let expired = self.is_expired();
            if expired || Instant::now() >= next_census {
                let observed = descendants.as_mut().map_or(Ok(()), |tree| tree.refresh());
                if let Err(error) = observed {
                    match child.exited() {
                        Ok(true) => {
                            timed_out = expired;
                            break;
                        }
                        outcome => {
                            std::mem::forget((descendants, child));
                            return Err(RecoveryRequired(format!(
                                "cannot observe command {pid}: {error:#}; leader status: {outcome:?}"
                            )).into());
                        }
                    }
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
                    std::mem::forget((descendants, child));
                    return Err(RecoveryRequired(format!(
                        "cannot wait for command {pid}: {error}"
                    ))
                    .into());
                }
            }
        }
        // A completed leader can leave writers behind. Settle them on success too.
        if let Some(tree) = &descendants {
            if let Err(error) = tree.settle(Duration::from_millis(250)) {
                std::mem::forget((descendants, child));
                return Err(RecoveryRequired(format!(
                    "command {pid} descendant settlement unproven: {error:#}"
                ))
                .into());
            }
        }
        if let Err(error) = child.settle(Duration::from_millis(250)) {
            std::mem::forget((descendants, child));
            return Err(
                RecoveryRequired(format!("command {pid} settlement unproven: {error:#}")).into(),
            );
        }
        let status = child.reap().map_err(|error| {
            RecoveryRequired(format!("command {pid} final reap unproven: {error}"))
        })?;
        fn finish(
            reader: Option<std::thread::JoinHandle<std::io::Result<Vec<u8>>>>,
        ) -> Result<Vec<u8>> {
            match reader {
                Some(reader) => reader
                    .join()
                    .map_err(|_| anyhow!("plugin output reader panicked"))?
                    .map_err(Into::into),
                None => Ok(Vec::new()),
            }
        }
        let stdout = finish(stdout)?;
        let stderr = finish(stderr)?;
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
        async fn captured(path: &std::path::Path) -> crate::process::CapturedProcess {
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
                                return process;
                            }
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap()
        }
        let leader = captured(&root.join("leader")).await;
        let late = captured(&root.join("late")).await;
        std::fs::write(root.join("release"), b"").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !leader.exited().unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!late.exited().unwrap());
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
}
