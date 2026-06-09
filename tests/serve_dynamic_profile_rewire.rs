//! Dynamic per-profile disk-watch rewire. Two layers of coverage:
//!
//! * Lower layer (`dynamic_profile_rewire_inserts_and_removes_entries`):
//!   drives `add_profile_disk_watch` / `remove_profile_disk_watch` / `rename_profile_disk_watch` directly against an in-process
//!   `AppState`, asserting `disk_watch_handles` insert/remove under the
//!   canonical drop-then-abort order. Observable only at this layer
//!   because the handles map is daemon-internal state that the HTTP
//!   surface intentionally does not expose.
//! * HTTP API layer (`dynamic_profile_create_via_http_api`,
//!   `dynamic_profile_delete_via_http_api`): spawns a real `aoe serve`
//!   subprocess against an isolated `HOME` and drives `POST /api/profiles`
//!   and `DELETE /api/profiles/{name}`. These are the entry points that
//!   trigger `add_profile_disk_watch` / `remove_profile_disk_watch` / `rename_profile_disk_watch` in production, so this layer guards
//!   the daemon-boot path.

#![cfg(feature = "serve")]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use agent_of_empires::server::test_support::{
    add_profile_disk_watch, build_test_app_state, disk_watch_handle_count, has_disk_watch_handle,
    remove_profile_disk_watch, rename_profile_disk_watch,
};
use serial_test::serial;
use tempfile::TempDir;

use common::{pick_free_port, wait_for_port};

#[tokio::test]
#[serial]
async fn dynamic_profile_rewire_inserts_and_removes_entries() {
    let temp = tempfile::tempdir().unwrap();
    isolate_home(temp.path());
    let _ = agent_of_empires::session::get_profile_dir("rewire-profile").expect("profile dir");

    let state = build_test_app_state(Vec::new());
    let live = agent_of_empires::file_watch::test_support::new_filewatch().expect("live svc");
    let mut state_mut = Arc::try_unwrap(state).map_err(|_| ()).expect("unique");
    agent_of_empires::server::test_support::replace_file_watch(&mut state_mut, live);
    let state = Arc::new(state_mut);

    add_profile_disk_watch(&state, "rewire-profile").await;
    {
        assert!(
            has_disk_watch_handle(&state, "rewire-profile").await,
            "add must insert the per-profile entry"
        );
    }

    remove_profile_disk_watch(&state, "rewire-profile").await;
    {
        assert!(
            !has_disk_watch_handle(&state, "rewire-profile").await,
            "remove must drop the per-profile entry"
        );
    }

    add_profile_disk_watch(&state, "rewire-profile").await;
    {
        assert!(
            has_disk_watch_handle(&state, "rewire-profile").await,
            "re-add after remove must converge back to one live entry"
        );
        assert_eq!(
            disk_watch_handle_count(&state).await,
            1,
            "re-add must not duplicate entries"
        );
    }
    assert_eq!(
        agent_of_empires::server::test_support::file_watch(&state).subscriber_count(),
        1,
        "re-add after remove must leave exactly one live subscription"
    );
}

#[tokio::test]
#[serial]
async fn dynamic_profile_rewire_overwrite_replaces_existing_subscription() {
    let temp = tempfile::tempdir().unwrap();
    isolate_home(temp.path());
    let _ = agent_of_empires::session::get_profile_dir("rewire-profile").expect("profile dir");

    let state = build_test_app_state(Vec::new());
    let live = agent_of_empires::file_watch::test_support::new_filewatch().expect("live svc");
    let mut state_mut = Arc::try_unwrap(state).map_err(|_| ()).expect("unique");
    agent_of_empires::server::test_support::replace_file_watch(&mut state_mut, live);
    let state = Arc::new(state_mut);

    add_profile_disk_watch(&state, "rewire-profile").await;
    assert_eq!(
        agent_of_empires::server::test_support::file_watch(&state).subscriber_count(),
        1,
        "first add must install one live subscription"
    );

    add_profile_disk_watch(&state, "rewire-profile").await;
    assert_eq!(
        agent_of_empires::server::test_support::file_watch(&state).subscriber_count(),
        1,
        "re-adding the same profile must replace, not leak, the prior subscription"
    );
    assert_eq!(
        disk_watch_handle_count(&state).await,
        1,
        "replace path must keep one map entry"
    );
    assert!(has_disk_watch_handle(&state, "rewire-profile").await);
}

#[tokio::test]
#[serial]
async fn rewire_after_rename_drops_old_subscribes_new() {
    // Locks the rewire pair invoked by `rename_profile` in
    // `src/server/api/system.rs`. Without these two calls the old
    // canonical dir's handle leaks and the renamed dir is unwatched.
    let temp = tempfile::tempdir().unwrap();
    isolate_home(temp.path());
    let _ = agent_of_empires::session::get_profile_dir("rename-old").expect("profile dir");
    let _ = agent_of_empires::session::get_profile_dir("rename-new").expect("profile dir");

    let state = build_test_app_state(Vec::new());
    let live = agent_of_empires::file_watch::test_support::new_filewatch().expect("live svc");
    let mut state_mut = Arc::try_unwrap(state).map_err(|_| ()).expect("unique");
    agent_of_empires::server::test_support::replace_file_watch(&mut state_mut, live);
    let state = Arc::new(state_mut);

    add_profile_disk_watch(&state, "rename-old").await;
    assert!(
        has_disk_watch_handle(&state, "rename-old").await,
        "precondition: old profile subscription must be present"
    );

    rename_profile_disk_watch(&state, "rename-old", "rename-new").await;

    assert!(
        !has_disk_watch_handle(&state, "rename-old").await,
        "old subscription must be removed; otherwise rename leaks the stale handle"
    );
    assert!(
        has_disk_watch_handle(&state, "rename-new").await,
        "new subscription must be installed; otherwise renamed profile loses live propagation"
    );
}

#[tokio::test]
#[serial]
async fn dynamic_profile_create_via_http_api() {
    let daemon = ServeDaemon::spawn();
    let client = reqwest::Client::new();

    let resp = client
        .post(daemon.url("/api/profiles"))
        .json(&serde_json::json!({"name": "alt"}))
        .send()
        .await
        .expect("POST /api/profiles");
    assert_eq!(
        resp.status().as_u16(),
        201,
        "POST /api/profiles must succeed (got {})",
        resp.status()
    );

    let profiles = list_profiles(&client, &daemon).await;
    assert!(
        profiles.iter().any(|name| name == "alt"),
        "GET /api/profiles must list the new profile, got {:?}",
        profiles
    );

    let profile_dir = daemon.app_dir().join("profiles").join("alt");
    assert!(
        profile_dir.is_dir(),
        "POST /api/profiles must create the on-disk profile dir at {}",
        profile_dir.display()
    );
}

#[tokio::test]
#[serial]
async fn dynamic_profile_delete_via_http_api() {
    let daemon = ServeDaemon::spawn();
    let client = reqwest::Client::new();

    let create = client
        .post(daemon.url("/api/profiles"))
        .json(&serde_json::json!({"name": "alt"}))
        .send()
        .await
        .expect("POST /api/profiles");
    assert_eq!(create.status().as_u16(), 201, "create must succeed");

    let delete = client
        .delete(daemon.url("/api/profiles/alt"))
        .send()
        .await
        .expect("DELETE /api/profiles/alt");
    assert_eq!(
        delete.status().as_u16(),
        200,
        "DELETE /api/profiles/alt must succeed (got {})",
        delete.status()
    );

    let profiles = list_profiles(&client, &daemon).await;
    assert!(
        !profiles.iter().any(|name| name == "alt"),
        "GET /api/profiles must NOT list a deleted profile, got {:?}",
        profiles
    );

    let profile_dir = daemon.app_dir().join("profiles").join("alt");
    assert!(
        !profile_dir.exists(),
        "DELETE /api/profiles/alt must remove the on-disk profile dir at {}",
        profile_dir.display()
    );
}

fn isolate_home(temp: &Path) {
    // SAFETY: env mutation; #[serial] guards cross-test races.
    unsafe { std::env::set_var("HOME", temp) };
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"))
    };
}

async fn list_profiles(client: &reqwest::Client, daemon: &ServeDaemon) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct ProfileInfo {
        name: String,
    }
    let resp = client
        .get(daemon.url("/api/profiles"))
        .send()
        .await
        .expect("GET /api/profiles");
    assert_eq!(resp.status().as_u16(), 200);
    let parsed: Vec<ProfileInfo> = resp.json().await.expect("decode profiles");
    parsed.into_iter().map(|p| p.name).collect()
}

/// RAII guard around a foreground `aoe serve` subprocess scoped to an
/// isolated `HOME`. `Drop` kills the child and waits, even on test panic.
struct ServeDaemon {
    child: Option<Child>,
    port: u16,
    home: TempDir,
}

impl ServeDaemon {
    /// Spawn `aoe serve --no-auth --host 127.0.0.1 --port <free>` against
    /// a fresh `HOME`. Panics on startup failure so the test gives a
    /// useful diagnostic. The whole file is `#![cfg(feature = "serve")]`,
    /// so the binary always has the feature enabled at this point.
    fn spawn() -> Self {
        let aoe = env!("CARGO_BIN_EXE_aoe");
        let home = tempfile::tempdir().expect("home tempdir");
        let port = pick_free_port();

        let mut cmd = Command::new(aoe);
        cmd.args([
            "serve",
            "--no-auth",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ]);
        cmd.env("HOME", home.path());
        #[cfg(target_os = "linux")]
        cmd.env("XDG_CONFIG_HOME", home.path().join(".config"));
        cmd.env_remove("AGENT_OF_EMPIRES_DEBUG");

        let mut child = cmd.spawn().expect("spawn aoe serve");
        if !wait_for_port(port, Duration::from_secs(15)) {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "aoe serve did not bind 127.0.0.1:{} within 15s; likely missing serve feature",
                port
            );
        }
        Self {
            child: Some(child),
            port,
            home,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    fn app_dir(&self) -> PathBuf {
        let home = self.home.path();
        if cfg!(target_os = "linux") {
            home.join(".config")
                .join(agent_of_empires::session::APP_DIR_NAME_XDG)
        } else {
            home.join(agent_of_empires::session::APP_DIR_NAME_OTHER)
        }
    }
}

impl Drop for ServeDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
