//! Migration v017: rewrite previously-installed AoE hook shell strings to the
//! per-user-base shape (issue #1844). This is the second hook-string rewrite
//! in the AoE history; v015 hardened the in-shell guards and v017 changes the
//! base path baked into them from `/tmp/aoe-hooks` (world-known, multi-tenant
//! exposed) to `/tmp/aoe-hooks-<euid>` host-side, plus a SELinux/ACL/xattr-
//! tolerant mode pattern (`d*------|d*------.|d*------+|d*------@`) and an
//! environment-pinning preamble (`unset IFS; umask 077; LC_ALL=C ls -ldn`).
//!
//! ## Strategy: rewrite first, sweep last
//!
//! 1. **Rewrite** every reachable host hook target's bytes via the live
//!    `install_*` functions. Per-target rewrite failures `tracing::warn!`
//!    and continue.
//! 2. **Sweep** the legacy `/tmp/aoe-hooks` directory ONLY if every
//!    rewrite succeeded AND it exists owned by us. `O_NOFOLLOW` open +
//!    per-entry `fstatat` uid check; we never `remove_dir_all` and never
//!    touch entries owned by another user (multi-tenant safe).
//!
//! Reverse order (sweep first, rewrite last) was rejected: a rewrite
//! failure between sweep and the schema bump would leave the agent
//! recreating `/tmp/aoe-hooks` on every fire, undoing the hardening for
//! any rewrite-failed target until the user manually runs
//! `aoe uninstall && aoe add`. With rewrite-first, a partial-failure
//! state keeps legacy entries discoverable for manual cleanup.
//!
//! ## Failure policy
//!
//! Per `AGENTS.md > Data Migrations`, a returned `Err` aborts boot. v017
//! never bubbles per-target failures (matches v015): every per-target
//! issue surfaces as `tracing::warn!`, the schema-version still bumps so
//! the migration runs at most once, and recovery is `aoe uninstall && aoe
//! add` exactly as documented for v015.
//!
//! ## Sandbox image hooks
//!
//! Hooks baked into a Docker / Podman / Apple-Containers sandbox image are
//! NOT rewritten by v017 (inherits the v015 limitation). Next image rebuild
//! picks up the current canonical bytes. Defense-in-depth bound: container
//! isolation already gates the multi-tenant threat we are addressing.

use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use crate::hooks::{
    has_aoe_marker, install_codex_hooks_with_preserved_state, install_hooks, iter_hook_targets_in,
    snapshot_codex_hooks_state, HookInstallTarget, HookTarget, HookTargetKind,
};

/// Path of the legacy world-known hook directory swept by this migration.
/// Production callers always reach `/tmp/aoe-hooks`; tests substitute a
/// tempdir via `override_legacy_for_test`.
const PRODUCTION_LEGACY_PATH: &str = "/tmp/aoe-hooks";

#[cfg(test)]
thread_local! {
    static LEGACY_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
    static FORCE_REWRITE_FAILURE_FOR: std::cell::RefCell<Option<&'static str>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn override_legacy_for_test(p: PathBuf) {
    LEGACY_OVERRIDE.with(|c| *c.borrow_mut() = Some(p));
}

#[cfg(test)]
pub(crate) fn clear_legacy_override_for_test() {
    LEGACY_OVERRIDE.with(|c| *c.borrow_mut() = None);
}

#[cfg(test)]
pub(crate) fn force_rewrite_failure_for_test(agent: &'static str) {
    FORCE_REWRITE_FAILURE_FOR.with(|c| *c.borrow_mut() = Some(agent));
}

#[cfg(test)]
pub(crate) fn clear_rewrite_failure_for_test() {
    FORCE_REWRITE_FAILURE_FOR.with(|c| *c.borrow_mut() = None);
}

fn legacy_path() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = LEGACY_OVERRIDE.with(|c| c.borrow().clone()) {
        return p;
    }
    PathBuf::from(PRODUCTION_LEGACY_PATH)
}

pub fn run() -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let app_dir = crate::session::get_app_dir()?;
    run_in(&home, &app_dir)
}

pub(crate) fn run_in(home: &Path, app_dir: &Path) -> Result<()> {
    let env_lists = collect_env_lists(app_dir);
    debug!(
        target: "migrations.v017",
        home = %home.display(),
        app_dir = %app_dir.display(),
        env_lists = env_lists.len(),
        "v017: scanning hook targets"
    );

    let mut rewritten = 0usize;
    let mut any_failure = false;
    for target in iter_hook_targets_in(home, &env_lists) {
        if !has_aoe_marker(&target) {
            continue;
        }
        match rewrite_one(&target) {
            Ok(()) => {
                rewritten += 1;
                info!(
                    target: "migrations.v017",
                    agent = target.agent_name,
                    path = %target.path.display(),
                    "v017: rewrote AoE hook entries to per-user-base canonical form"
                );
            }
            Err(e) => {
                any_failure = true;
                warn!(
                    target: "migrations.v017",
                    agent = target.agent_name,
                    path = %target.path.display(),
                    error = %e,
                    "v017: skipped (rewrite failed)"
                );
            }
        }
    }

    // Gate the legacy sweep on full rewrite success: if any target's hook
    // bytes still point at `/tmp/aoe-hooks/<id>` (because its rewrite
    // failed), the agent will recreate that path on its next hook fire.
    // Sweeping it now strands the user with no recoverable diagnostic
    // state for the failed targets.
    if any_failure {
        info!(
            target: "migrations.v017",
            "skipped legacy /tmp/aoe-hooks sweep: at least one rewrite failed; \
             the legacy directory is left for manual recovery"
        );
    } else {
        sweep_legacy_base_in(&legacy_path());
    }

    info!(target: "migrations.v017", count = rewritten, "v017: done");
    Ok(())
}

fn rewrite_one(target: &HookTarget) -> Result<()> {
    #[cfg(test)]
    if FORCE_REWRITE_FAILURE_FOR.with(|c| c.borrow().as_deref() == Some(target.agent_name)) {
        anyhow::bail!("test-injected rewrite failure for {}", target.agent_name);
    }
    match target.kind {
        HookTargetKind::JsonSettings | HookTargetKind::CodexJson => {
            install_hooks(&target.path, &target.events, HookInstallTarget::Host)
        }
        HookTargetKind::CodexToml => {
            let preserved = snapshot_codex_hooks_state(&target.path)?;
            install_codex_hooks_with_preserved_state(
                &target.path,
                &target.events,
                preserved,
                HookInstallTarget::Host,
            )
        }
        HookTargetKind::Sidecar(sidecar) => {
            (sidecar.install)(&target.path, HookInstallTarget::Host, &target.events)
        }
    }
}

/// Best-effort removal of the legacy world-known hook directory.
///
/// Multi-tenant safe: walks the directory with `O_NOFOLLOW`, checks each
/// entry's owner via `fstatat(AT_SYMLINK_NOFOLLOW)`, unlinks only entries we
/// own. Entries owned by other users are left untouched. The legacy directory
/// itself is `rmdir`'d only if it is empty after our sweep AND owned by us.
///
/// Failure modes (all logged, none propagate):
/// - legacy is a symlink: `O_NOFOLLOW` open returns `ELOOP`, we exit.
/// - legacy is not a directory: open returns `ENOTDIR`, we exit.
/// - We do not own a child entry: `tracing::debug!` and skip.
/// - Parent dir not empty after sweep: `rmdir` returns `ENOTEMPTY`, we leave
///   the dir for whichever co-tenant still has entries there to clean up.
fn sweep_legacy_base_in(legacy: &Path) {
    use nix::errno::Errno;
    use nix::fcntl::{open, AtFlags, OFlag};
    use nix::sys::stat::{fstat, fstatat, Mode};
    use nix::unistd::geteuid;
    use std::ffi::CString;
    use std::os::fd::AsFd;

    let euid = geteuid().as_raw();
    let dir_fd = match open(
        legacy,
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return,
        Err(e) => {
            debug!(target: "migrations.v017",
                "v017: skipped legacy {} sweep (open: {})", legacy.display(), e);
            return;
        }
    };

    let parent_st = match fstat(&dir_fd) {
        Ok(st) => st,
        Err(e) => {
            warn!(target: "migrations.v017",
                "v017: fstat legacy {} failed: {}", legacy.display(), e);
            return;
        }
    };
    let parent_is_ours = parent_st.st_uid == euid;

    let dup = match dir_fd.try_clone() {
        Ok(fd) => fd,
        Err(e) => {
            warn!(target: "migrations.v017", "v017: dup legacy fd: {}", e);
            return;
        }
    };
    let mut readdir = match nix::dir::Dir::from_fd(dup) {
        Ok(d) => d,
        Err(e) => {
            warn!(target: "migrations.v017", "v017: Dir::from_fd legacy: {}", e);
            return;
        }
    };

    let mut child_names: Vec<std::ffi::CString> = Vec::new();
    for entry in readdir.iter().flatten() {
        let name = entry.file_name().to_owned();
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        child_names.push(name);
    }
    drop(readdir);

    for name in child_names {
        let name_str = match name.to_str() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let st = match fstatat(dir_fd.as_fd(), name_str, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(e) => {
                debug!(target: "migrations.v017", "v017: fstatat {}: {}", name_str, e);
                continue;
            }
        };
        if st.st_uid != euid {
            debug!(target: "migrations.v017",
                "v017: legacy {}/{} owned by uid={}, skipping (multi-tenant)",
                legacy.display(), name_str, st.st_uid);
            continue;
        }
        match unlink_subtree(&dir_fd, name_str, &st) {
            Ok(()) => debug!(target: "migrations.v017",
                "v017: removed {}/{}", legacy.display(), name_str),
            Err(e) => warn!(target: "migrations.v017",
                "v017: failed to remove {}/{}: {}", legacy.display(), name_str, e),
        }
    }

    if parent_is_ours {
        drop(dir_fd);
        let legacy_str = legacy
            .to_str()
            .expect("legacy path must be UTF-8 for libc::rmdir");
        let legacy_c = CString::new(legacy_str).expect("legacy path must not contain NUL");
        let rc = unsafe { nix::libc::rmdir(legacy_c.as_ptr()) };
        if rc == 0 {
            info!(target: "migrations.v017", "v017: removed legacy {}", legacy.display());
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(nix::libc::ENOTEMPTY) {
                debug!(target: "migrations.v017",
                    "v017: legacy {} non-empty (other-user entries remain)",
                    legacy.display());
            } else {
                debug!(target: "migrations.v017",
                    "v017: rmdir legacy {}: {}", legacy.display(), err);
            }
        }
    } else {
        debug!(target: "migrations.v017",
            "v017: legacy {} owner uid={} != euid={}, leaving parent",
            legacy.display(), parent_st.st_uid, euid);
    }
}

/// Iterative owner-checked subtree removal under a verified parent fd.
///
/// Replaces a recursive walker. Bounded depth (`MAX_DEPTH = 32`) defends
/// against an attacker-planted deep tree under a same-uid entry; AoE itself
/// never creates nested subdirs, so 32 is generous in practice. On overflow
/// we leave the remaining subtree intact and warn so the operator can
/// inspect manually.
fn unlink_subtree(
    parent_fd: &std::os::fd::OwnedFd,
    name: &str,
    st: &nix::sys::stat::FileStat,
) -> nix::Result<()> {
    use nix::fcntl::{openat, AtFlags, OFlag};
    use nix::sys::stat::Mode;
    use nix::unistd::{unlinkat, UnlinkatFlags};
    use std::os::fd::AsFd;

    const MAX_DEPTH: usize = 32;

    if (st.st_mode & nix::libc::S_IFMT) != nix::libc::S_IFDIR {
        return unlinkat(parent_fd.as_fd(), name, UnlinkatFlags::NoRemoveDir);
    }

    enum Phase {
        Enter,
        Exit,
    }
    struct Frame {
        parent: std::os::fd::OwnedFd,
        name: std::ffi::CString,
        depth: usize,
        phase: Phase,
    }

    let initial_parent = parent_fd
        .try_clone()
        .map_err(|e| nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(nix::libc::EIO)))?;
    let initial_name = std::ffi::CString::new(name).map_err(|_| nix::errno::Errno::EINVAL)?;
    let mut stack: Vec<Frame> = Vec::with_capacity(8);
    stack.push(Frame {
        parent: initial_parent,
        name: initial_name,
        depth: 0,
        phase: Phase::Enter,
    });

    while let Some(frame) = stack.pop() {
        match frame.phase {
            Phase::Enter => {
                if frame.depth >= MAX_DEPTH {
                    warn!(
                        target: "migrations.v017",
                        depth = frame.depth,
                        "v017: depth cap reached, leaving subtree intact"
                    );
                    continue;
                }
                let name_str = match frame.name.to_str() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let child_fd = openat(
                    frame.parent.as_fd(),
                    name_str,
                    OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
                    Mode::empty(),
                )?;
                let dup = child_fd.try_clone().map_err(|e| {
                    nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(nix::libc::EIO))
                })?;
                let mut sub = nix::dir::Dir::from_fd(dup)?;
                let entries: Vec<std::ffi::CString> = sub
                    .iter()
                    .flatten()
                    .filter_map(|entry| {
                        let n = entry.file_name().to_owned();
                        let b = n.to_bytes();
                        if b == b"." || b == b".." {
                            None
                        } else {
                            Some(n)
                        }
                    })
                    .collect();
                drop(sub);
                let exit_parent = frame.parent.try_clone().map_err(|e| {
                    nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(nix::libc::EIO))
                })?;
                stack.push(Frame {
                    parent: exit_parent,
                    name: frame.name,
                    depth: frame.depth,
                    phase: Phase::Exit,
                });
                let euid = nix::unistd::geteuid().as_raw();
                for entry_name in entries {
                    let en_str = match entry_name.to_str() {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let cst = nix::sys::stat::fstatat(
                        child_fd.as_fd(),
                        en_str,
                        AtFlags::AT_SYMLINK_NOFOLLOW,
                    )?;
                    if cst.st_uid != euid {
                        continue;
                    }
                    if (cst.st_mode & nix::libc::S_IFMT) == nix::libc::S_IFDIR {
                        let new_parent = child_fd.try_clone().map_err(|e| {
                            nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(nix::libc::EIO))
                        })?;
                        stack.push(Frame {
                            parent: new_parent,
                            name: entry_name,
                            depth: frame.depth + 1,
                            phase: Phase::Enter,
                        });
                    } else {
                        unlinkat(child_fd.as_fd(), en_str, UnlinkatFlags::NoRemoveDir)?;
                    }
                }
                drop(child_fd);
            }
            Phase::Exit => {
                let name_str = match frame.name.to_str() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                unlinkat(frame.parent.as_fd(), name_str, UnlinkatFlags::RemoveDir)?;
            }
        }
    }
    Ok(())
}

/// Read `environment` arrays from raw TOML (global config + each profile).
/// Mirror of v015's helper of the same shape; kept duplicated rather than
/// shared so v015 cannot pull a regression in v017 and vice versa.
fn collect_env_lists(app_dir: &Path) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    if let Some(env) = read_environment_from_toml(&app_dir.join("config.toml")) {
        out.push(env);
    }
    let profiles_dir = app_dir.join("profiles");
    let Ok(entries) = fs::read_dir(&profiles_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if let Some(env) = read_environment_from_toml(&entry.path().join("config.toml")) {
                out.push(env);
            }
        }
    }
    out
}

fn read_environment_from_toml(path: &Path) -> Option<Vec<String>> {
    let content = fs::read_to_string(path).ok()?;
    let table: toml::Value = toml::from_str(&content).ok()?;
    let env = table.get("environment")?.as_array()?;
    Some(
        env.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::EnvGuard;
    use serde_json::Value;
    use std::fs;
    use tempfile::TempDir;

    /// Pre-#1844 hardened bytes (post-v015): the form we are migrating
    /// AWAY from. Contains the `aoe-hooks` substring so `is_aoe_hook_command`
    /// flags it for rewrite.
    const PRE_V017_STATUS_CMD: &str = "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
        case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
        mkdir -p \"/tmp/aoe-hooks/$AOE_INSTANCE_ID\" 2>/dev/null; \
        printf running > \"/tmp/aoe-hooks/$AOE_INSTANCE_ID/status\" 2>/dev/null; \
        exit 0'";

    /// Clears CODEX_HOME, CLAUDE_CONFIG_DIR, etc. for the test duration so
    /// the migration's path resolution sees only the explicit fixtures in
    /// `home` / `app_dir`.
    fn unset_agent_home_env() -> EnvGuard {
        EnvGuard::unset(&[
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "CURSOR_CONFIG_DIR",
            "GEMINI_CONFIG_DIR",
            "QWEN_CONFIG_DIR",
        ])
    }

    fn setup_dirs() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let app_dir = tmp.path().join("app");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&app_dir).unwrap();
        (tmp, home, app_dir)
    }

    fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    fn pre_v017_claude_settings() -> Value {
        serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{ "type": "command", "command": PRE_V017_STATUS_CMD }]
                }]
            }
        })
    }

    /// Locks the canonical-form contract: every AoE-marked command in a
    /// post-v017 settings file must contain the SELinux/ACL/xattr-tolerant
    /// mode pattern, the env-pinning preamble, and the per-user-base suffix.
    fn assert_post_v017_canonical(claude: &Path) {
        let parsed: Value = serde_json::from_str(&fs::read_to_string(claude).unwrap()).unwrap();
        let hooks = parsed["hooks"].as_object().expect("hooks present");
        assert!(
            !hooks.is_empty(),
            "v017 wrote empty hooks on {}",
            claude.display()
        );
        let mut status_writers = 0;
        for (_, matchers) in hooks {
            let arr = matchers.as_array().unwrap();
            for matcher in arr {
                for hook in matcher["hooks"].as_array().unwrap() {
                    let cmd = hook["command"].as_str().unwrap_or_default();
                    if !cmd.contains("aoe-hooks") {
                        continue;
                    }
                    if cmd.contains("aoe __extract-session-id") {
                        continue;
                    }
                    status_writers += 1;
                    assert!(
                        cmd.contains("drwx------|drwx------.|drwx------+|drwx------@"),
                        "v017 must bake the strict 0700 mode pattern: {cmd}"
                    );
                    assert!(
                        cmd.contains("unset IFS")
                            && cmd.contains("umask 077")
                            && cmd.contains("LC_ALL=C ls -ldn"),
                        "v017 must bake the env preamble: {cmd}"
                    );
                    let euid = nix::unistd::geteuid().as_raw();
                    let suffix = format!("/tmp/aoe-hooks-{euid}");
                    assert!(
                        cmd.contains(&format!("B={suffix}")),
                        "v017 must bake the per-user base: {cmd}"
                    );
                }
            }
        }
        assert!(
            status_writers > 0,
            "no AoE status writer found in {}; canonical assertion would be vacuous",
            claude.display()
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn rewrites_pre_v017_claude_settings_to_per_user_base() {
        let _env = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude").join("settings.json");
        write_json(&claude, &pre_v017_claude_settings());

        run_in(&home, &app_dir).unwrap();

        assert_post_v017_canonical(&claude);
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn skips_files_without_aoe_marker() {
        let _env = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude").join("settings.json");
        let user_settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{ "type": "command", "command": "echo user-only" }]
                }]
            }
        });
        write_json(&claude, &user_settings);

        run_in(&home, &app_dir).unwrap();

        let parsed: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(
            parsed["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap(),
            "echo user-only",
            "non-AoE file must be byte-untouched"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn idempotent_byte_identical_on_second_run() {
        let _env = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude").join("settings.json");
        write_json(&claude, &pre_v017_claude_settings());

        run_in(&home, &app_dir).unwrap();
        let after_first = fs::read_to_string(&claude).unwrap();
        run_in(&home, &app_dir).unwrap();
        let after_second = fs::read_to_string(&claude).unwrap();

        assert_eq!(after_first, after_second, "v017 must be byte-idempotent");
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn rewrite_failure_keeps_legacy_dir_intact_for_manual_recovery() {
        use std::os::unix::fs::PermissionsExt;
        let _env = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();

        let claude = home.join(".claude").join("settings.json");
        write_json(&claude, &pre_v017_claude_settings());

        let legacy = _tmp.path().join("legacy-aoe-hooks");
        fs::create_dir(&legacy).unwrap();
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o700)).unwrap();
        let preserved_inst = legacy.join("inst-must-survive");
        fs::create_dir(&preserved_inst).unwrap();
        fs::write(preserved_inst.join("status"), b"running").unwrap();

        super::override_legacy_for_test(legacy.clone());
        super::force_rewrite_failure_for_test("claude");
        let result = run_in(&home, &app_dir);
        super::clear_rewrite_failure_for_test();
        super::clear_legacy_override_for_test();
        result.unwrap();

        assert!(
            legacy.exists(),
            "legacy directory must remain when any rewrite failed"
        );
        assert!(
            preserved_inst.exists(),
            "owned legacy entries must remain so the operator can recover them"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn legacy_sweep_full_success_removes_owned_dir() {
        use std::os::unix::fs::PermissionsExt;
        let _env = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();

        let claude = home.join(".claude").join("settings.json");
        write_json(&claude, &pre_v017_claude_settings());

        let legacy = _tmp.path().join("legacy-aoe-hooks-clean");
        fs::create_dir(&legacy).unwrap();
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o700)).unwrap();
        let inst_dir = legacy.join("inst-to-sweep");
        fs::create_dir(&inst_dir).unwrap();
        fs::write(inst_dir.join("status"), b"idle").unwrap();
        fs::write(inst_dir.join("session_id"), b"deadbeef").unwrap();

        super::override_legacy_for_test(legacy.clone());
        let result = run_in(&home, &app_dir);
        super::clear_legacy_override_for_test();
        result.unwrap();

        assert!(
            !legacy.exists(),
            "owned legacy directory must be swept on full rewrite success"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn legacy_sweep_handles_symlink_at_legacy_path() {
        use std::os::unix::fs::PermissionsExt;
        let _env = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();

        let canary = _tmp.path().join("canary");
        fs::create_dir(&canary).unwrap();
        fs::write(canary.join("file"), b"do not delete").unwrap();

        let legacy_link = _tmp.path().join("legacy-aoe-hooks-link");
        std::os::unix::fs::symlink(&canary, &legacy_link).unwrap();

        let claude = home.join(".claude").join("settings.json");
        write_json(&claude, &pre_v017_claude_settings());
        fs::set_permissions(_tmp.path().join("home"), fs::Permissions::from_mode(0o755)).ok();

        super::override_legacy_for_test(legacy_link.clone());
        let result = run_in(&home, &app_dir);
        super::clear_legacy_override_for_test();
        result.unwrap();

        assert!(
            canary.join("file").exists(),
            "symlink target must be untouched"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn sandbox_baked_hooks_under_aoe_sandbox_subpath_are_untouched() {
        let _env = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();

        let claude = home.join(".claude").join("settings.json");
        write_json(&claude, &pre_v017_claude_settings());

        let sandbox_settings = home.join(".claude").join("sandbox").join("settings.json");
        let sandbox_baked = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{
                        "type": "command",
                        "command": PRE_V017_STATUS_CMD
                    }]
                }]
            }
        });
        write_json(&sandbox_settings, &sandbox_baked);
        let sandbox_before = fs::read_to_string(&sandbox_settings).unwrap();

        run_in(&home, &app_dir).unwrap();

        let sandbox_after = fs::read_to_string(&sandbox_settings).unwrap();
        assert_eq!(
            sandbox_before, sandbox_after,
            "v017 must not touch settings under .claude/sandbox/ (baked into image)"
        );
    }
}
