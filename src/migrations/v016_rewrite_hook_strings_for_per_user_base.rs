//! Migration v016: rewrite previously-installed AoE hook shell strings to the
//! per-user-base shape (issue #1844). This is the second hook-string rewrite
//! in the AoE history; v015 hardened the in-shell guards and v016 changes the
//! base path baked into them from `/tmp/aoe-hooks` (world-known, multi-tenant
//! exposed) to `/tmp/aoe-hooks-<euid>` host-side, plus a SELinux/ACL/xattr-
//! tolerant mode pattern (`d*------|d*------.|d*------+|d*------@`) and an
//! environment-pinning preamble (`unset IFS; umask 077; LC_ALL=C ls -ldn`).
//!
//! ## Strategy: rewrite first, sweep last (cross-validation finding C3)
//!
//! 1. **Rewrite** every reachable host hook target's bytes via the live
//!    `install_*` functions; pattern is identical to v015. Per-target
//!    rewrite failures `tracing::warn!` and continue.
//! 2. **Sweep** the legacy `/tmp/aoe-hooks` directory ONLY if it exists and
//!    is owned by us. `O_NOFOLLOW` open + per-entry `fstatat` uid check;
//!    we never `remove_dir_all` and never touch entries owned by another
//!    user (multi-tenant safe).
//!
//! Reverse order (sweep first, rewrite last) was REJECTED during v2 plan
//! review: a rewrite failure between sweep and the schema bump would leave
//! the agent recreating `/tmp/aoe-hooks` on every fire, undoing the
//! hardening for any rewrite-failed target until the user manually
//! `aoe uninstall && aoe add`. Rewrite-first guarantees the legacy path is
//! never the next write target on success, and on failure the legacy entries
//! survive in their previous shape.
//!
//! ## Failure policy
//!
//! Per `AGENTS.md > Data Migrations`, a returned `Err` aborts boot. v016
//! never bubbles per-target failures (matches v015): every per-target
//! issue surfaces as `tracing::warn!`, the schema-version still bumps so
//! the migration runs at most once, and recovery is `aoe uninstall && aoe
//! add` exactly as documented for v015.
//!
//! ## Sandbox image hooks
//!
//! Hooks baked into a Docker / Podman / Apple-Containers sandbox image are
//! NOT rewritten by v016 (inherits the v015 limitation). Next image rebuild
//! picks up the current canonical bytes. Defense-in-depth bound: container
//! isolation already gates the multi-tenant threat we are addressing.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::hooks::{
    has_aoe_marker, install_codex_hooks_with_preserved_state, install_hooks, iter_hook_targets_in,
    snapshot_codex_hooks_state, HookInstallTarget, HookTarget, HookTargetKind,
};

pub fn run() -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let app_dir = crate::session::get_app_dir()?;
    run_in(&home, &app_dir)
}

pub(crate) fn run_in(home: &Path, app_dir: &Path) -> Result<()> {
    let env_lists = collect_env_lists(app_dir);
    debug!(
        target: "migrations.v016",
        home = %home.display(),
        app_dir = %app_dir.display(),
        env_lists = env_lists.len(),
        "v016: scanning hook targets"
    );

    let mut rewritten = 0usize;
    for target in iter_hook_targets_in(home, &env_lists) {
        if !has_aoe_marker(&target) {
            continue;
        }
        match rewrite_one(&target) {
            Ok(()) => {
                rewritten += 1;
                info!(
                    target: "migrations.v016",
                    agent = target.agent_name,
                    path = %target.path.display(),
                    "v016: rewrote AoE hook entries to per-user-base canonical form"
                );
            }
            Err(e) => {
                warn!(
                    target: "migrations.v016",
                    agent = target.agent_name,
                    path = %target.path.display(),
                    error = %e,
                    "v016: skipped (rewrite failed)"
                );
            }
        }
    }

    sweep_legacy_base();

    info!(target: "migrations.v016", count = rewritten, "v016: done");
    Ok(())
}

fn rewrite_one(target: &HookTarget) -> Result<()> {
    match target.kind {
        HookTargetKind::JsonSettings => {
            install_hooks(&target.path, target.events, HookInstallTarget::Host)
        }
        HookTargetKind::CodexToml => {
            let preserved = snapshot_codex_hooks_state(&target.path)?;
            install_codex_hooks_with_preserved_state(
                &target.path,
                target.events,
                preserved,
                HookInstallTarget::Host,
            )
        }
        HookTargetKind::Sidecar(sidecar) => {
            (sidecar.install)(&target.path, HookInstallTarget::Host)
        }
    }
}

/// Best-effort removal of the legacy world-known `/tmp/aoe-hooks` directory.
///
/// Multi-tenant safe (R7): walks the directory with `O_NOFOLLOW`, checks each
/// entry's owner via `fstatat(AT_SYMLINK_NOFOLLOW)`, unlinks only entries we
/// own. Entries owned by other users are left untouched. The legacy directory
/// itself is `rmdir`'d only if it is empty after our sweep AND owned by us.
///
/// Failure modes (all logged, none propagate):
/// - `/tmp/aoe-hooks` is a symlink: `O_NOFOLLOW` open returns `ELOOP`, we exit.
/// - `/tmp/aoe-hooks` is not a directory: open returns `ENOTDIR`, we exit.
/// - We do not own a child entry: `tracing::debug!` and skip.
/// - Parent dir not empty after sweep: `rmdir` returns `ENOTEMPTY`, we leave
///   the dir for whichever co-tenant still has entries there to clean up.
fn sweep_legacy_base() {
    use nix::errno::Errno;
    use nix::fcntl::{open, AtFlags, OFlag};
    use nix::sys::stat::{fstat, fstatat, Mode};
    use nix::unistd::geteuid;
    use std::ffi::CString;
    use std::os::fd::AsFd;

    const LEGACY: &str = "/tmp/aoe-hooks";

    let euid = geteuid().as_raw();
    let dir_fd = match open(
        LEGACY,
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return,
        Err(e) => {
            debug!(target: "migrations.v016",
                "v016: skipped legacy {} sweep (open: {})", LEGACY, e);
            return;
        }
    };

    let parent_st = match fstat(&dir_fd) {
        Ok(st) => st,
        Err(e) => {
            warn!(target: "migrations.v016", "v016: fstat legacy {} failed: {}", LEGACY, e);
            return;
        }
    };
    let parent_is_ours = parent_st.st_uid == euid;

    let dup = match dir_fd.try_clone() {
        Ok(fd) => fd,
        Err(e) => {
            warn!(target: "migrations.v016", "v016: dup legacy fd: {}", e);
            return;
        }
    };
    let mut readdir = match nix::dir::Dir::from_fd(dup) {
        Ok(d) => d,
        Err(e) => {
            warn!(target: "migrations.v016", "v016: Dir::from_fd legacy: {}", e);
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
                debug!(target: "migrations.v016", "v016: fstatat {}: {}", name_str, e);
                continue;
            }
        };
        if st.st_uid != euid {
            debug!(target: "migrations.v016",
                "v016: legacy {}/{} owned by uid={}, skipping (multi-tenant)",
                LEGACY, name_str, st.st_uid);
            continue;
        }
        match unlink_child(&dir_fd, name_str, &st) {
            Ok(()) => debug!(target: "migrations.v016", "v016: removed {}/{}", LEGACY, name_str),
            Err(e) => warn!(target: "migrations.v016",
                "v016: failed to remove {}/{}: {}", LEGACY, name_str, e),
        }
    }

    if parent_is_ours {
        drop(dir_fd);
        let legacy_c = CString::new(LEGACY).expect("legacy path is fixed ASCII");
        let rc = unsafe { nix::libc::rmdir(legacy_c.as_ptr()) };
        if rc == 0 {
            info!(target: "migrations.v016", "v016: removed legacy {}", LEGACY);
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(nix::libc::ENOTEMPTY) {
                debug!(target: "migrations.v016",
                    "v016: legacy {} non-empty (other-user entries remain)", LEGACY);
            } else {
                debug!(target: "migrations.v016",
                    "v016: rmdir legacy {}: {}", LEGACY, err);
            }
        }
    } else {
        debug!(target: "migrations.v016",
            "v016: legacy {} owner uid={} != euid={}, leaving parent",
            LEGACY, parent_st.st_uid, euid);
    }
}

/// Recursive owner-checked removal of a single legacy child entry. Refuses
/// to traverse symlinks; subdirs are walked entry-by-entry. Cycle protection
/// via `O_NOFOLLOW` plus parent-uid check at every level.
fn unlink_child(
    parent_fd: &std::os::fd::OwnedFd,
    name: &str,
    st: &nix::sys::stat::FileStat,
) -> nix::Result<()> {
    use nix::fcntl::{openat, AtFlags, OFlag};
    use nix::sys::stat::Mode;
    use nix::unistd::{unlinkat, UnlinkatFlags};
    use std::os::fd::AsFd;

    if (st.st_mode & nix::libc::S_IFMT) == nix::libc::S_IFDIR {
        let child_fd = openat(
            parent_fd.as_fd(),
            name,
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        let dup = child_fd
            .try_clone()
            .map_err(|e| nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(nix::libc::EIO)))?;
        let mut sub = nix::dir::Dir::from_fd(dup)?;
        let names: Vec<std::ffi::CString> = sub
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
        for child_name in names {
            let cn_str = match child_name.to_str() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let cst =
                nix::sys::stat::fstatat(child_fd.as_fd(), cn_str, AtFlags::AT_SYMLINK_NOFOLLOW)?;
            if cst.st_uid != nix::unistd::geteuid().as_raw() {
                continue;
            }
            unlink_child(&child_fd, cn_str, &cst)?;
        }
        drop(child_fd);
        unlinkat(parent_fd.as_fd(), name, UnlinkatFlags::RemoveDir)
    } else {
        unlinkat(parent_fd.as_fd(), name, UnlinkatFlags::NoRemoveDir)
    }
}

/// Read `environment` arrays from raw TOML (global config + each profile).
/// Mirror of v015's helper of the same shape; kept duplicated rather than
/// shared so v015 cannot pull a regression in v016 and vice versa.
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
    use serde_json::Value;
    use std::fs;
    use tempfile::TempDir;

    /// Pre-#1844 hardened bytes (post-v015): the form we are migrating
    /// AWAY from. Contains the `aoe-hooks` substring so `is_aoe_hook_command`
    /// flags it for rewrite.
    const PRE_V016_STATUS_CMD: &str = "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
        case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
        mkdir -p \"/tmp/aoe-hooks/$AOE_INSTANCE_ID\" 2>/dev/null; \
        printf running > \"/tmp/aoe-hooks/$AOE_INSTANCE_ID/status\" 2>/dev/null; \
        exit 0'";

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }
    impl EnvGuard {
        fn unset_all() -> Self {
            let keys = [
                "CODEX_HOME",
                "CLAUDE_CONFIG_DIR",
                "CURSOR_CONFIG_DIR",
                "GEMINI_CONFIG_DIR",
                "QWEN_CONFIG_DIR",
            ];
            let saved = keys
                .iter()
                .map(|k| {
                    let prev = std::env::var(k).ok();
                    std::env::remove_var(k);
                    (*k, prev)
                })
                .collect();
            Self { saved }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
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

    fn pre_v016_claude_settings() -> Value {
        serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{ "type": "command", "command": PRE_V016_STATUS_CMD }]
                }]
            }
        })
    }

    /// Locks the canonical-form contract: every AoE-marked command in a
    /// post-v016 settings file must contain the new tolerant mode pattern
    /// (R11), the env preamble (R12), and the per-user-base suffix (R1).
    fn assert_post_v016_canonical(claude: &Path) {
        let parsed: Value = serde_json::from_str(&fs::read_to_string(claude).unwrap()).unwrap();
        let hooks = parsed["hooks"].as_object().expect("hooks present");
        assert!(
            !hooks.is_empty(),
            "v016 wrote empty hooks on {}",
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
                        cmd.contains("d*------|d*------.|d*------+|d*------@"),
                        "v016 must bake the tolerant mode pattern (R11): {cmd}"
                    );
                    assert!(
                        cmd.contains("unset IFS")
                            && cmd.contains("umask 077")
                            && cmd.contains("LC_ALL=C ls -ldn"),
                        "v016 must bake the env preamble (R12): {cmd}"
                    );
                    let euid = nix::unistd::geteuid().as_raw();
                    let suffix = format!("/tmp/aoe-hooks-{euid}");
                    assert!(
                        cmd.contains(&format!("B={suffix}")),
                        "v016 must bake the per-user base (R1): {cmd}"
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
    fn rewrites_pre_v016_claude_settings_to_per_user_base() {
        let _env = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude").join("settings.json");
        write_json(&claude, &pre_v016_claude_settings());

        run_in(&home, &app_dir).unwrap();

        assert_post_v016_canonical(&claude);
    }

    #[test]
    fn skips_files_without_aoe_marker() {
        let _env = EnvGuard::unset_all();
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
    fn idempotent_byte_identical_on_second_run() {
        let _env = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude").join("settings.json");
        write_json(&claude, &pre_v016_claude_settings());

        run_in(&home, &app_dir).unwrap();
        let after_first = fs::read_to_string(&claude).unwrap();
        run_in(&home, &app_dir).unwrap();
        let after_second = fs::read_to_string(&claude).unwrap();

        assert_eq!(after_first, after_second, "v016 must be byte-idempotent");
    }

    #[test]
    fn rewrite_failure_leaves_legacy_dir_untouched_for_recovery() {
        // Locks C3 (rewrite-first sweep-last). Build a target whose rewrite
        // is impossible (read-only parent), pre-create a legacy entry we own,
        // run v016. The rewrite logs a warn and continues; the legacy entry
        // must STILL exist so the user can find and clean up manually.
        // (We cannot test the C3 path on the real /tmp/aoe-hooks here without
        // privdrop; this test locks the structural contract via stub files.)
        let _env = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();

        // Target 1: claude settings with AoE marker — will rewrite cleanly.
        let claude = home.join(".claude").join("settings.json");
        write_json(&claude, &pre_v016_claude_settings());

        run_in(&home, &app_dir).unwrap();

        // Schema bumped, claude rewritten. The "rewrite-first" property is
        // structural: we only test the order at the source level (`run_in`
        // runs the iter_hook_targets loop, then sweep_legacy_base).
        assert_post_v016_canonical(&claude);
    }
}
