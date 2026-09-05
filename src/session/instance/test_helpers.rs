//! Helpers shared by the tests of more than one `instance` submodule.

use super::*;

/// Force the tmux session cache into a fresh "server reachable, but this
/// session is not in its list" snapshot so `Session::existence()` resolves
/// to `Absent` regardless of whether a real tmux server happens to be up on
/// the per-process test socket. Tests that assert detection latches `Error`
/// must call this, otherwise their outcome depends on test scheduling
/// (#2936). Returns the RAII guard; keep it bound for the test's duration
/// (`let _cache = ...`) so it restores the prior cache on drop, and mark
/// the test `#[serial_test::serial]` since the cache is process-global.
#[must_use]
pub(super) fn force_session_absent() -> crate::tmux::SessionCacheGuard {
    let guard = crate::tmux::SessionCacheGuard::capture();
    guard.force_present(&["aoe_some_other_session"]);
    guard
}

/// Seed the process-global `agent_detect_as` registry for one profile
/// and return a guard that restores the profile's prior entries on drop:
/// `install_from_config` replaces the whole profile's state and the
/// registry outlives every test, so the caller must keep the returned
/// guard alive for the duration of its reads.
pub(crate) fn install_aliases(
    profile: &str,
    aliases: &[(&str, &str)],
) -> crate::tmux::status_rules::ProfileRegistryGuard {
    let guard = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);
    let mut config = crate::session::Config::default();
    for (agent, target) in aliases {
        config
            .session
            .agent_detect_as
            .insert(agent.to_string(), target.to_string());
    }
    crate::tmux::status_rules::install_from_config(profile, &config);
    guard
}

pub(super) fn write_sidecar(instance_id: &str, sid: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let base = crate::hooks::hook_base_path();
    if !base.exists() {
        std::fs::create_dir_all(&base).expect("create hook base dir");
    }
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
        .expect("set hook base mode 0700");
    let dir = crate::hooks::hook_status_dir(instance_id).expect("test id must be allowlist-safe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("set hook instance mode 0700");
    std::fs::write(dir.join("session_id"), sid).unwrap();
    dir
}

pub(super) fn seed_disk_for_sidecar_test(profile: &str, inst: &Instance) {
    let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
    let snapshot = inst.clone();
    storage
        .update(|i, g| {
            *i = vec![snapshot.clone()];
            *g = crate::session::GroupTree::new_with_groups(std::slice::from_ref(&snapshot), &[])
                .get_all_groups();
            Ok(())
        })
        .unwrap();
}

pub(super) const SIDECAR_TEST_FRESH_UUID: &str = "11111111-2222-4333-8444-555555555555";
