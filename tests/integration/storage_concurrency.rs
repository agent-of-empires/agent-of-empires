//! Integration tests for the two-layer per-profile lock (in-process mutex
//! + cross-process flock).
//!
//! In-process lock layers are unit tested in `session::storage`; these cover
//! `Storage::update` from outside the crate and across real `aoe` processes.

use agent_of_empires::session::{GroupTree, Instance, Storage};
use anyhow::Result;
use serial_test::serial;
use std::sync::{Arc, Barrier};

use crate::common::setup_temp_home;

/// Concurrent per-field updates on the same instance must not clobber each
/// other. Mirrors the structured view-handler / status-poll pattern: thread A
/// mutates one field (`title`, simulating status poll), thread B mutates
/// a different field (`notify_on_idle`, standing in for the structured view
/// handler's `structured_view`). Both should land. Regression guard for
/// review #5: a
/// wholesale-replace closure (using a pre-lock snapshot) would lose one
/// of the writes.
#[test]
#[serial]
fn test_concurrent_per_field_updates_no_clobber() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let seed = vec![Instance::new("session", "/tmp/session")];
    storage.update(|i, g| {
        *i = seed.to_vec();
        *g = GroupTree::new_with_groups(&seed, &[]).get_all_groups();
        Ok(())
    })?;
    let target_id = storage.load()?[0].id.clone();

    let n_iterations = 16usize;
    let start = Arc::new(Barrier::new(2));
    let id_for_a = target_id.clone();
    let id_for_b = target_id.clone();
    let start_a = Arc::clone(&start);
    let start_b = Arc::clone(&start);

    let thread_a = std::thread::spawn(move || -> Result<()> {
        let storage = Storage::new_unwatched("default")?;
        start_a.wait();
        for i in 0..n_iterations {
            storage.update(|all, _groups| {
                if let Some(slot) = all.iter_mut().find(|i| i.id == id_for_a) {
                    slot.title = format!("from-A-{i}");
                }
                Ok(())
            })?;
        }
        Ok(())
    });

    let thread_b = std::thread::spawn(move || -> Result<()> {
        let storage = Storage::new_unwatched("default")?;
        start_b.wait();
        for _ in 0..n_iterations {
            storage.update(|all, _groups| {
                if let Some(slot) = all.iter_mut().find(|i| i.id == id_for_b) {
                    slot.notify_on_idle = Some(true);
                }
                Ok(())
            })?;
        }
        Ok(())
    });

    thread_a.join().unwrap()?;
    thread_b.join().unwrap()?;

    let loaded = storage.load()?;
    assert_eq!(loaded.len(), 1);
    assert!(
        loaded[0].title.starts_with("from-A-"),
        "thread A's title write must be preserved (got: {})",
        loaded[0].title
    );
    assert_eq!(
        loaded[0].notify_on_idle,
        Some(true),
        "thread B's notify_on_idle write must be preserved"
    );
    Ok(())
}

// Cross-process tests: real `aoe` subprocesses contend for the profile flock.

fn aoe_bin() -> &'static str {
    env!("CARGO_BIN_EXE_aoe")
}

struct CliChild(std::process::Child);

impl Drop for CliChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct HeldStorageUpdate {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<Result<()>>>,
}

impl HeldStorageUpdate {
    fn new(storage: Storage) -> Self {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (held, ready) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            storage.update(|_, _| {
                let _ = held.send(());
                let _ = released.recv();
                Ok(())
            })
        });
        let guard = Self {
            release: Some(release),
            thread: Some(thread),
        };
        ready
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("parent storage lock acquired");
        guard
    }
}

impl Drop for HeldStorageUpdate {
    fn drop(&mut self) {
        self.release.take();
        let result = self.thread.take().expect("owned storage thread").join();
        if !std::thread::panicking() {
            result
                .expect("storage holder thread panicked")
                .expect("held storage update failed");
        }
    }
}

#[test]
#[serial]
fn test_cross_process_blocking_acquire() -> Result<()> {
    let temp = setup_temp_home();
    let home = temp.path().to_path_buf();

    let storage = Storage::new_unwatched("default")?;
    storage.update(|instances, _groups| {
        instances.push(Instance::new("blocked", "/tmp/aoe-test-blocked"));
        Ok(())
    })?;
    let id = storage.load()?[0].id.clone();

    let parent = HeldStorageUpdate::new(Storage::new_unwatched("default")?);
    let contended = home.join("child-contended");
    let mut child = CliChild(
        std::process::Command::new(aoe_bin())
            .args(["session", "favorite", &id])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("AOE_E2E_STORAGE_LOCK_CONTENDED", &contended)
            .spawn()?,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !contended.exists() {
        assert!(
            child.0.try_wait()?.is_none(),
            "child bypassed held storage lock"
        );
        if std::time::Instant::now() >= deadline {
            panic!("child never observed the held storage lock");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        child.0.try_wait()?.is_none(),
        "contending child cannot finish before release"
    );
    drop(parent);
    let status = child.0.wait()?;
    assert!(status.success(), "child exit status: {status:?}");

    let final_state = storage.load()?;
    assert!(
        final_state
            .iter()
            .any(|i| i.id == id && i.favorited_at.is_some()),
        "child's favorite must land after parent releases"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn test_cross_process_lock_released_on_child_kill() -> Result<()> {
    use fs2::FileExt;
    use nix::sys::signal::{self, Signal};
    use nix::sys::wait::waitpid;
    use nix::unistd::{fork, ForkResult, Pid};

    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    storage.update(|insts, _| {
        insts.push(Instance::new("victim-kill", "/tmp/aoe-test-victim-kill"));
        Ok(())
    })?;

    let lock_path = agent_of_empires::session::get_profile_dir("default")?.join(".storage.lock");
    let path_c =
        std::ffi::CString::new(lock_path.to_str().expect("utf8 path")).expect("path has no NUL");

    // SAFETY: between fork() and _exit() the child only calls
    // async-signal-safe libc routines (open, flock, pause, _exit). Cargo
    // test runs each test on a worker thread, so the post-fork process
    // is multithreaded; non-async-signal-safe code (allocator, std::fs)
    // would be UB here.
    let child = match unsafe { fork() }? {
        ForkResult::Parent { child } => child,
        ForkResult::Child => unsafe {
            let fd = nix::libc::open(
                path_c.as_ptr(),
                nix::libc::O_RDWR | nix::libc::O_CREAT,
                0o600,
            );
            if fd < 0 {
                nix::libc::_exit(2);
            }
            if nix::libc::flock(fd, nix::libc::LOCK_EX) != 0 {
                nix::libc::_exit(3);
            }
            nix::libc::pause();
            nix::libc::_exit(0);
        },
    };

    struct ChildGuard(Pid);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = signal::kill(self.0, Signal::SIGKILL);
            let _ = waitpid(self.0, None);
        }
    }
    let _g = ChildGuard(child);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        if FileExt::try_lock_exclusive(&probe).is_err() {
            break;
        }
        let _ = FileExt::unlock(&probe);
        drop(probe);
        if std::time::Instant::now() > deadline {
            anyhow::bail!("child did not acquire flock within deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    signal::kill(child, Signal::SIGKILL)?;
    waitpid(child, None)?;

    let started = std::time::Instant::now();
    storage.update(|_, _| Ok(()))?;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "lock must release after SIGKILL; observed {:?}",
        started.elapsed()
    );
    Ok(())
}
