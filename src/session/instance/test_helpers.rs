//! Helpers shared by the tests of more than one `instance` submodule.

use super::*;

/// Makes `Session::existence()` resolve to `Absent` whatever tmux server is up
/// (#2936). Keep the guard bound and mark the test serial: the cache is global.
#[must_use]
pub(super) fn force_session_absent() -> crate::tmux::SessionCacheGuard {
    let guard = crate::tmux::SessionCacheGuard::capture();
    guard.force_present(&["aoe_some_other_session"]);
    guard
}

/// Seeds the global `agent_detect_as` registry for one profile; the guard
/// restores the prior entries on drop.
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

pub(super) fn test_sandbox(name: &str, workdir: Option<&str>) -> SandboxInfo {
    SandboxInfo {
        enabled: true,
        container_id: None,
        image: "test-image".to_string(),
        container_name: name.to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: workdir.map(str::to_string),
    }
}

pub(super) fn tool_instance(tool: &str, path: &str) -> Instance {
    let mut inst = Instance::new(tool, path);
    inst.tool = tool.to_string();
    inst
}
