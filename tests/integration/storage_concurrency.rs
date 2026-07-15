//! Integration tests for the two-layer per-profile lock (in-process mutex
//! + cross-process flock).
//!
//! These exercise the public locked API (`Storage::update`,
//! `update_workspace_ordering`) end-to-end from outside the crate, the same
//! surface a third-party consumer would see.

use agent_of_empires::session::{update_workspace_ordering, Group, GroupTree, Instance, Storage};
use anyhow::Result;
use serial_test::serial;
use std::sync::{Arc, Barrier};

#[cfg(unix)]
use crate::common::running_as_root;
use crate::common::setup_temp_home;

#[test]
#[serial]
fn test_concurrent_updates_no_lost_updates() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    storage.update(|i, g| {
        *i = [].to_vec();
        *g = GroupTree::new_with_groups(&[], &[]).get_all_groups();
        Ok(())
    })?;

    let n_threads = 16usize;
    let start = Arc::new(Barrier::new(n_threads));
    std::thread::scope(|scope| {
        for tid in 0..n_threads {
            let start = Arc::clone(&start);
            scope.spawn(move || {
                start.wait();
                let storage = Storage::new_unwatched("default").unwrap();
                storage
                    .update(|instances, _groups| {
                        instances.push(Instance::new(
                            &format!("worker-{tid}"),
                            &format!("/tmp/worker-{tid}"),
                        ));
                        Ok(())
                    })
                    .unwrap();
            });
        }
    });

    let loaded = storage.load()?;
    assert_eq!(loaded.len(), n_threads);
    let mut titles: Vec<_> = loaded.iter().map(|i| i.title.clone()).collect();
    titles.sort();
    for tid in 0..n_threads {
        assert!(titles.contains(&format!("worker-{tid}")));
    }
    Ok(())
}

#[test]
#[serial]
fn test_concurrent_workspace_ordering_merge() -> Result<()> {
    let _temp = setup_temp_home();

    update_workspace_ordering(|ord| {
        ord.order.clear();
        Ok(())
    })?;

    let n_threads = 12usize;
    let start = Arc::new(Barrier::new(n_threads));
    std::thread::scope(|scope| {
        for tid in 0..n_threads {
            let start = Arc::clone(&start);
            scope.spawn(move || {
                start.wait();
                update_workspace_ordering(|ord| {
                    ord.order.push(format!("/repo/{tid}::main"));
                    Ok(())
                })
                .unwrap();
            });
        }
    });

    let loaded = agent_of_empires::session::load_workspace_ordering()?;
    assert_eq!(loaded.order.len(), n_threads);
    for tid in 0..n_threads {
        assert!(loaded.order.contains(&format!("/repo/{tid}::main")));
    }
    Ok(())
}

#[test]
#[serial]
fn test_update_with_groups_and_instances_round_trip() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    storage.update(|i, g| {
        *i = [].to_vec();
        *g = GroupTree::new_with_groups(&[], &[]).get_all_groups();
        Ok(())
    })?;

    storage.update(|instances, groups| {
        let mut inst = Instance::new("project-a", "/tmp/a");
        inst.group_path = "work/clients".to_string();
        instances.push(inst);
        groups.push(Group::new("clients", "work/clients"));
        Ok(())
    })?;

    let (loaded_instances, loaded_groups) = storage.load_with_groups()?;
    assert_eq!(loaded_instances.len(), 1);
    assert_eq!(loaded_instances[0].group_path, "work/clients");
    assert_eq!(loaded_groups.len(), 1);
    assert_eq!(loaded_groups[0].name, "clients");
    Ok(())
}

/// Concurrent per-field updates on the same instance must not clobber each
/// other. Mirrors the structured view-handler / status-poll pattern: thread A
/// mutates one field (`title`, simulating status poll), thread B mutates
/// a different field (`notify_on_idle`, standing in for the structured view
/// handler's `structured_view`; the latter is `#[cfg(feature = "serve")]`,
/// `notify_on_idle` is always available and exhibits the same lost-update
/// class). Both should land. Regression guard for review #5: a
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

/// `Storage::update` must surface a disk-write failure as `Err` so the
/// `delete_session` handler can return HTTP 500 instead of silently
/// dropping the in-memory entry. Regression guard for review #7.
#[cfg(unix)]
#[test]
#[serial]
fn test_update_propagates_disk_write_failure() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Root bypasses the Unix DAC permission bits, so the read-only dir below
    // would not make the write fail and this regression guard would falsely
    // fail. Skip cleanly rather than assert a failure that cannot be injected.
    if running_as_root() {
        eprintln!(
            "test_update_propagates_disk_write_failure: skipping (running as root; \
             uid 0 bypasses the read-only dir bit, so the write cannot be made to fail)"
        );
        return Ok(());
    }

    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let seed = vec![Instance::new("session", "/tmp/s")];
    storage.update(|i, g| {
        *i = seed.to_vec();
        *g = GroupTree::new_with_groups(&seed, &[]).get_all_groups();
        Ok(())
    })?;

    let profile_dir = agent_of_empires::session::get_profile_dir("default")?;

    let original_perms = std::fs::metadata(&profile_dir)?.permissions();
    let mut readonly_perms = original_perms.clone();
    readonly_perms.set_mode(0o555);
    std::fs::set_permissions(&profile_dir, readonly_perms)?;

    let result: Result<()> = storage.update(|instances, _groups| {
        instances.clear();
        Ok(())
    });

    std::fs::set_permissions(&profile_dir, original_perms)?;

    assert!(
        result.is_err(),
        "update must Err when disk write fails (read-only parent dir)"
    );

    let reloaded = storage.load()?;
    assert_eq!(
        reloaded.len(),
        1,
        "disk state must be unchanged when atomic_write fails"
    );
    Ok(())
}

// Cross-process tests: spawn real `aoe` subprocesses that each go through
// the full Storage::update path (in-process Mutex + cross-process flock +
// atomic_write). They validate that two independent OS processes
// (e.g. TUI + daemon, or two CLI invocations, or daemon + a CLI invocation)
// cannot lose each other's updates when racing on the same profile.

fn aoe_bin() -> &'static str {
    env!("CARGO_BIN_EXE_aoe")
}

fn spawn_favorite(aoe: &str, home: &std::path::Path, id: &str) -> std::process::Child {
    let mut cmd = std::process::Command::new(aoe);
    cmd.args(["session", "favorite", id])
        .env("HOME", home)
        .env_remove("AGENT_OF_EMPIRES_DEBUG");
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("aoe binary failed to spawn")
}

#[test]
#[serial]
fn test_cross_process_no_lost_updates() -> Result<()> {
    let temp = setup_temp_home();
    let home = temp.path().to_path_buf();

    let storage = Storage::new_unwatched("default")?;
    let n = 8usize;
    storage.update(|instances, _groups| {
        for i in 0..n {
            instances.push(Instance::new(
                &format!("session-{i}"),
                &format!("/tmp/aoe-test-{i}"),
            ));
        }
        Ok(())
    })?;
    let ids: Vec<String> = storage.load()?.iter().map(|i| i.id.clone()).collect();
    assert_eq!(ids.len(), n);

    let aoe = aoe_bin();
    let children: Vec<_> = ids
        .iter()
        .map(|id| spawn_favorite(aoe, &home, id))
        .collect();
    for mut child in children {
        let status = child.wait()?;
        assert!(
            status.success(),
            "child `aoe session favorite` exited with {status:?}"
        );
    }

    let final_state = storage.load()?;
    let favorited = final_state
        .iter()
        .filter(|i| i.favorited_at.is_some())
        .count();
    assert_eq!(
        favorited, n,
        "every concurrent CLI favorite must persist (no lost updates)"
    );
    Ok(())
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

    let hold = std::time::Duration::from_millis(800);
    let parent_held = std::sync::Arc::new(std::sync::Barrier::new(2));
    let parent_held_in_thread = parent_held.clone();

    let storage_clone = Storage::new_unwatched("default")?;
    let parent_handle = std::thread::spawn(move || {
        storage_clone
            .update(|_instances, _groups| {
                parent_held_in_thread.wait();
                std::thread::sleep(hold);
                Ok(())
            })
            .unwrap();
    });

    parent_held.wait();
    let started = std::time::Instant::now();
    let mut child = spawn_favorite(aoe_bin(), &home, &id);
    let status = child.wait()?;
    let elapsed = started.elapsed();
    parent_handle.join().unwrap();

    assert!(status.success(), "child exit status: {status:?}");
    assert!(
        elapsed >= hold - std::time::Duration::from_millis(200),
        "child should have blocked on the flock for ~{:?}, observed {:?}",
        hold,
        elapsed
    );

    let final_state = storage.load()?;
    assert!(
        final_state
            .iter()
            .any(|i| i.id == id && i.favorited_at.is_some()),
        "child's favorite must land after parent releases"
    );
    Ok(())
}

#[test]
#[serial]
fn test_lock_released_on_panic_unwind() -> Result<()> {
    let temp = setup_temp_home();
    let home = temp.path().to_path_buf();

    let storage = Storage::new_unwatched("default")?;
    storage.update(|instances, _groups| {
        instances.push(Instance::new("victim", "/tmp/aoe-test-victim"));
        Ok(())
    })?;
    let id = storage.load()?[0].id.clone();

    let storage_clone = Storage::new_unwatched("default")?;
    let parent_handle = std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = storage_clone.update(|_, _| -> Result<()> {
                panic!("simulated abort while holding the lock");
            });
        }));
    });
    parent_handle.join().expect("thread joined");

    let started = std::time::Instant::now();
    let mut child = spawn_favorite(aoe_bin(), &home, &id);
    let status = child.wait()?;
    let elapsed = started.elapsed();

    assert!(status.success());
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "lock must be released after holder unwinds; observed {:?}",
        elapsed
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

#[test]
#[serial]
fn test_cross_process_independent_profiles_do_not_serialise() -> Result<()> {
    let temp = setup_temp_home();
    let home = temp.path().to_path_buf();

    let s_a = Storage::new_unwatched("profile-a")?;
    let s_b = Storage::new_unwatched("profile-b")?;
    s_a.update(|insts, _| {
        insts.push(Instance::new("a-1", "/tmp/aoe-test-a"));
        Ok(())
    })?;
    s_b.update(|insts, _| {
        insts.push(Instance::new("b-1", "/tmp/aoe-test-b"));
        Ok(())
    })?;
    let id_a = s_a.load()?[0].id.clone();
    let id_b = s_b.load()?[0].id.clone();

    let hold = std::time::Duration::from_secs(5);
    let storage_clone = Storage::new_unwatched("profile-a")?;
    let parent_held = Arc::new(Barrier::new(2));
    let parent_held_inner = parent_held.clone();
    let parent_handle = std::thread::spawn(move || {
        storage_clone
            .update(|_, _| {
                parent_held_inner.wait();
                std::thread::sleep(hold);
                Ok(())
            })
            .unwrap();
    });

    let started = std::time::Instant::now();
    parent_held.wait();

    // Cross-profile children must NOT be serialised by profile-a's flock.
    let aoe = aoe_bin();
    let mut cmd_b = std::process::Command::new(aoe);
    cmd_b
        .args(["session", "favorite", "--profile", "profile-b", &id_b])
        .env("HOME", &home);
    cmd_b.env("XDG_CONFIG_HOME", home.join(".config"));
    let status = cmd_b
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    let elapsed = started.elapsed();

    parent_handle.join().unwrap();
    assert!(status.success(), "profile-b favorite failed: {status:?}");
    assert!(
        elapsed < hold,
        "profile-b must not block on profile-a's flock; observed {:?} >= {:?}",
        elapsed,
        hold
    );
    let _ = id_a;
    Ok(())
}
