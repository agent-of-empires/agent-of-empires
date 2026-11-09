use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;

pub(crate) struct Transaction(File);

#[derive(Debug, thiserror::Error)]
#[error("Daemon lifecycle is busy; retry after the current transition finishes")]
pub(crate) struct TransactionBusy;

pub(crate) struct Lifetime {
    _file: File,
}

fn lock_path(name: &str) -> Result<std::path::PathBuf> {
    let socket = super::transport::local_socket_path()?;
    let parent = socket.parent().context("Daemon socket has no directory")?;
    super::transport::prepare_runtime_directory(parent)?;
    Ok(parent.join(name))
}

fn open_lock(name: &str) -> Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(lock_path(name)?)?;
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.is_file()
            && meta.uid() == nix::unistd::geteuid().as_raw()
            && meta.mode() & 0o777 == 0o600
            && meta.nlink() == 1,
        "Unsafe daemon lock file"
    );
    Ok(file)
}

impl Transaction {
    /// Daemon mutations must not wait here: the transition may be waiting for
    /// that same daemon to exit.
    pub(crate) fn try_acquire() -> Result<Self> {
        let file = open_lock("transaction.lock")?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self(file)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(TransactionBusy.into())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Bound synchronous TUI actions so lifecycle transitions cannot freeze the UI.
    pub(crate) fn acquire_blocking_for(wait: Duration) -> Result<Self> {
        let file = open_lock("transaction.lock")?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self(file)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "the aoe serve daemon is starting, restarting or stopping and holds the \
                         daemon lifecycle lock; retry this command in a moment"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(crate) async fn acquire() -> Result<Self> {
        let file = open_lock("transaction.lock")?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self(file)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "Daemon lifecycle transaction timed out"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(crate) fn receive() -> Result<Self> {
        let file = File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
        let expected = open_lock("transaction.lock")?.metadata()?;
        let received = file.metadata()?;
        anyhow::ensure!(
            received.dev() == expected.dev() && received.ino() == expected.ino(),
            "Missing daemon transaction handoff"
        );
        file.try_lock_exclusive()
            .context("Invalid daemon transaction handoff")?;
        crate::process::detach_daemon_stdin()?;
        Ok(Self(file))
    }

    pub(crate) fn handoff(&self) -> Result<std::process::Stdio> {
        Ok(self.0.try_clone()?.into())
    }

    pub(crate) fn lifetime(&self) -> Result<Lifetime> {
        self.try_lifetime()?
            .context("A daemon already owns this namespace")
    }

    pub(crate) fn try_lifetime(&self) -> Result<Option<Lifetime>> {
        let file = open_lock("lifetime.lock")?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Lifetime { _file: file })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}
