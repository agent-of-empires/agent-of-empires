//! Integration coverage for the daemon-side session-id drain wiring.

#![cfg(feature = "serve")]

use std::path::Path;

use agent_of_empires::server::test_support::{
    attach_session_id_update_for_test, build_test_app_state, drain_session_id_updates_for_test,
    load_instances_from_disk_for_test, seed_instances_on_disk_for_test,
};
use agent_of_empires::session::Instance;
use serial_test::serial;

struct EnvGuard {
    prev_home: Option<String>,
    prev_xdg: Option<String>,
}

impl EnvGuard {
    fn set(path: &Path) -> Self {
        let prev_home = std::env::var("HOME").ok();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: 2024-edition `set_var` is unsafe because env is process
        // global and racy across threads. The `#[serial]` annotation
        // serialises this test against any other test that mutates env.
        unsafe {
            std::env::set_var("HOME", path);
            std::env::set_var("XDG_CONFIG_HOME", path.join(".config"));
        }
        Self {
            prev_home,
            prev_xdg,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `set` above.
        unsafe {
            match self.prev_home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_xdg.take() {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }
}

#[tokio::test]
#[serial]
async fn daemon_session_id_drain_updates_state_and_sessions_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::set(temp.path());

    let profile = "daemon-sid-drain";
    let fresh_sid = "019342ab-1234-7def-8901-abcdef012345";
    let untouched_sid = "keep-me";

    let mut target = Instance::new("daemon sid drain", "/tmp/daemon-sid-drain");
    target.source_profile = profile.to_string();
    let target_id = target.id.clone();

    let mut untouched = Instance::new("daemon sid untouched", "/tmp/daemon-sid-untouched");
    untouched.source_profile = profile.to_string();
    untouched.agent_session_id = Some(untouched_sid.to_string());
    let untouched_id = untouched.id.clone();

    seed_instances_on_disk_for_test(profile, vec![target.clone(), untouched.clone()]);
    attach_session_id_update_for_test(&mut target, fresh_sid);

    let state = build_test_app_state(vec![target, untouched]);

    drain_session_id_updates_for_test(&state).await;

    {
        let instances = state.instances.read().await;
        let target_row = instances
            .iter()
            .find(|inst| inst.id == target_id)
            .expect("target row in state");
        assert_eq!(target_row.agent_session_id.as_deref(), Some(fresh_sid));

        let untouched_row = instances
            .iter()
            .find(|inst| inst.id == untouched_id)
            .expect("untouched row in state");
        assert_eq!(
            untouched_row.agent_session_id.as_deref(),
            Some(untouched_sid)
        );
    }

    let persisted = load_instances_from_disk_for_test(profile);
    let target_row = persisted
        .iter()
        .find(|inst| inst.id == target_id)
        .expect("target row on disk");
    assert_eq!(target_row.agent_session_id.as_deref(), Some(fresh_sid));

    let untouched_row = persisted
        .iter()
        .find(|inst| inst.id == untouched_id)
        .expect("untouched row on disk");
    assert_eq!(
        untouched_row.agent_session_id.as_deref(),
        Some(untouched_sid)
    );
}
