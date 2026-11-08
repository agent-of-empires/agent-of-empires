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
            deadline: Instant::now() + Duration::from_secs(2 * 60 * 60),
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
        let mut child = crate::process::spawn_process_group(command)
            .context("spawning plugin command process group")?;
        let pid = child.id();
        let guard = (|| {
            let birth = crate::process::process_birth_identity(pid)?;
            crate::process::RunnerProcessGuard::capture(pid, birth.map(|b| b.fingerprint()))
        })();
        let guard = match guard {
            Ok(guard) => guard,
            Err(error) => {
                // Do not let a failed identity probe release ownership of a writer.
                let settlement = crate::process::terminate_and_reap_process_group(
                    &mut child,
                    Duration::from_millis(250),
                );
                std::mem::forget(child);
                return Err(RecoveryRequired(format!(
                    "cannot attest command {pid}: {error:#}; leader settlement: {settlement:?}"
                ))
                .into());
            }
        };
        let mut descendants = match crate::process::CapturedProcessTree::capture(pid) {
            Ok(tree) => Some(tree),
            Err(error) => match child.try_wait() {
                Ok(Some(_)) => None,
                outcome => {
                    std::mem::forget((guard, child));
                    return Err(RecoveryRequired(format!(
                        "cannot attest descendants of command {pid}: {error:#}; leader status: {outcome:?}"
                    ))
                    .into());
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
        let stdout = reader(child.stdout.take());
        let stderr = reader(child.stderr.take());
        let mut timed_out = false;
        let mut next_census = Instant::now() + Duration::from_secs(1);
        let status = loop {
            let expired = self.is_expired();
            if expired || Instant::now() >= next_census {
                let observed = descendants
                    .as_mut()
                    .map_or(Ok(()), |tree| tree.refresh())
                    .and_then(|()| guard.executing());
                if let Err(error) = observed {
                    std::mem::forget((descendants, guard, child));
                    return Err(RecoveryRequired(format!(
                        "cannot observe command {pid}: {error:#}"
                    ))
                    .into());
                }
                next_census = Instant::now() + Duration::from_secs(1);
            }
            if expired {
                timed_out = true;
                break None;
            }
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(error) => {
                    std::mem::forget((descendants, guard, child));
                    return Err(RecoveryRequired(format!(
                        "cannot wait for command {pid}: {error}"
                    ))
                    .into());
                }
            }
        };
        // A completed leader can leave writers behind. Settle them on success too.
        if let Some(tree) = &descendants {
            if let Err(error) = tree.settle(Duration::from_millis(250)) {
                std::mem::forget((descendants, guard, child));
                return Err(RecoveryRequired(format!(
                    "command {pid} descendant settlement unproven: {error:#}"
                ))
                .into());
            }
        }
        if let Err(error) = guard.settle(Duration::from_millis(250)) {
            std::mem::forget((descendants, guard, child));
            return Err(
                RecoveryRequired(format!("command {pid} settlement unproven: {error:#}")).into(),
            );
        }
        let status = match status {
            Some(status) => status,
            None => child.wait().context("reaping settled plugin command")?,
        };
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
