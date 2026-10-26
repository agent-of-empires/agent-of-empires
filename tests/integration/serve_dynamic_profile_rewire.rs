//! Dynamic per-profile disk-watch rewire. Two layers of coverage:
//!
//! * Lower layer (the overwrite and rename tests below):
//!   drives `add_profile_disk_watch` / `rename_profile_disk_watch` directly against an in-process
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

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use agent_of_empires::server::test_support::{
    add_profile_disk_watch, build_test_app_state_configured, disk_watch_handle_count,
    has_disk_watch_handle, rename_profile_disk_watch,
};
use serial_test::serial;
use tempfile::TempDir;

use crate::common::set_temp_home;
use crate::common::{pick_free_port, wait_for_port};

#[tokio::test]
#[serial]
async fn dynamic_profile_rewire_overwrite_replaces_existing_subscription() {
    let temp = tempfile::tempdir().unwrap();
    let _home = set_temp_home(temp.path());
    let _ = agent_of_empires::session::get_profile_dir("rewire-profile").expect("profile dir");

    let live = agent_of_empires::file_watch::test_support::new_filewatch().expect("live svc");
    let state = build_test_app_state_configured(Vec::new(), |state| {
        agent_of_empires::server::test_support::replace_file_watch(state, live);
    });

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
    let _home = set_temp_home(temp.path());
    let _ = agent_of_empires::session::get_profile_dir("rename-old").expect("profile dir");
    let _ = agent_of_empires::session::get_profile_dir("rename-new").expect("profile dir");

    let live = agent_of_empires::file_watch::test_support::new_filewatch().expect("live svc");
    let state = build_test_app_state_configured(Vec::new(), |state| {
        agent_of_empires::server::test_support::replace_file_watch(state, live);
    });

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

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn permanent_scratch_delete_cancels_an_unanswered_initialize() {
    assert_scratch_delete_cancels_initialize(ServeDaemon::spawn()).await;
    #[cfg(target_os = "linux")]
    if let Some(daemon) = ServeDaemon::spawn_with_scopes(true) {
        assert_scratch_delete_cancels_initialize(daemon).await;
    }
}

#[cfg(unix)]
async fn assert_scratch_delete_cancels_initialize(daemon: ServeDaemon) {
    use std::os::unix::fs::PermissionsExt;
    let scoped = daemon.scope_slice.is_some();
    let _home = set_temp_home(daemon.home.path());
    agent_of_empires::session::config::update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })
    .unwrap();
    let marker = daemon.home.path().join("initialize-observed");
    let escaped_marker = daemon.home.path().join("escapee-pid");
    let adapter = daemon.home.path().join("silent-acp-adapter");
    std::fs::write(
        &adapter,
        format!(
            "#!/bin/sh\nIFS= read -r request\nif [ '{}' = true ]; then setsid sleep 120 & printf '%s' \"$!\" > '{}'; fi\nprintf '%s\n' \"$request\" > '{}.pending'\nmv '{}.pending' '{}'\nexec sleep 120\n",
            scoped, escaped_marker.display(), marker.display(), marker.display(), marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o700)).unwrap();
    let client = reqwest::Client::new();
    let profile = client
        .post(daemon.url("/api/profiles"))
        .json(&serde_json::json!({"name": "scratch-cancellation"}))
        .send()
        .await
        .unwrap();
    assert_eq!(profile.status().as_u16(), 201);
    let created = client
        .post(daemon.url("/api/sessions"))
        .json(&serde_json::json!({
            "title": "unanswered initialize", "path": "", "tool": "gemini", "view": "structured",
            "scratch": true, "profile": "scratch-cancellation", "command_override": adapter,
        }))
        .send()
        .await
        .unwrap();
    let status = created.status();
    let created: serde_json::Value = created.json().await.unwrap();
    assert_eq!(status.as_u16(), 201, "{created}");
    let id = created["id"].as_str().unwrap();
    let scratch = daemon.app_dir().join("scratch").join(id);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the actual adapter must receive initialize");
    let request = std::fs::read_to_string(&marker).unwrap();
    assert!(request.contains("initialize"), "{request}");
    let record = agent_of_empires::process::worker_registry::load(id)
        .unwrap()
        .unwrap();
    struct RetireOnDrop {
        pid: u32,
        unit: Option<String>,
        slice: Option<String>,
        runtime: PathBuf,
    }
    impl Drop for RetireOnDrop {
        fn drop(&mut self) {
            agent_of_empires::process::worker::kill_process_group(self.pid);
            if let Some(unit) = &self.unit {
                let mut stop = manager_command("systemctl", &self.runtime, true);
                stop.args(["--user", "stop", unit]);
                if let Some(slice) = &self.slice {
                    stop.arg(slice);
                }
                let _ = stop.output();
            }
        }
    }
    let unit = match &record.scope {
        agent_of_empires::process::worker_registry::ScopeOwnership::Owned { unit } => {
            Some(unit.clone())
        }
        _ => None,
    };
    let _runner = RetireOnDrop {
        pid: record.pid,
        unit: unit.clone(),
        slice: daemon.scope_slice.clone(),
        runtime: daemon._runtime.path().to_path_buf(),
    };
    let escaped = if scoped {
        let unit = unit
            .as_ref()
            .expect("available user manager must actually scope this native runner");
        let escaped: u32 = std::fs::read_to_string(&escaped_marker)
            .unwrap()
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(escaped as i32)))
                .unwrap()
                .as_raw()
                == record.pid as i32
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the real child must actually escape its runner group");
        for pid in [record.pid, escaped] {
            let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
            assert!(
                cgroup.contains(unit) && cgroup.contains(daemon.scope_slice.as_ref().unwrap()),
                "{pid}: {cgroup}"
            );
        }
        Some(escaped)
    } else {
        None
    };
    assert!(scratch.is_dir());
    assert!(agent_of_empires::process::worker::is_process_group_alive(
        record.pid
    ));
    let response = client
        .delete(daemon.url(&format!("/api/sessions/{id}")))
        .json(&serde_json::json!({"force_delete": true, "keep_scratch": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let outcome: serde_json::Value = response.json().await.unwrap();
    assert!(
        outcome["cleanup_errors"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{outcome}"
    );
    assert!(
        !scratch.exists(),
        "actual permanent DELETE retained scratch: {outcome}"
    );
    assert!(!agent_of_empires::process::worker::is_process_group_alive(
        record.pid
    ));
    assert!(agent_of_empires::process::worker_registry::load(id)
        .unwrap()
        .is_none());
    assert!(!record.socket_path.exists());
    assert!(
        !agent_of_empires::process::worker::control_socket_sibling(&record.socket_path).exists()
    );
    if let Some(escaped) = escaped {
        assert!(
            !agent_of_empires::process::worker::is_process_group_alive(escaped),
            "scope escapee remained after permanent DELETE"
        );
        let output = manager_command("systemctl", daemon._runtime.path(), true)
            .args([
                "--user",
                "show",
                "--property=ActiveState",
                "--property=ControlGroup",
                unit.as_ref().unwrap(),
            ])
            .output()
            .unwrap();
        let state = String::from_utf8(output.stdout).unwrap();
        assert!(
            state.lines().any(|line| line == "ActiveState=inactive")
                && state.lines().any(|line| line == "ControlGroup="),
            "{state}"
        );
    }
    let rows: serde_json::Value = client
        .get(daemon.url("/api/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(rows["sessions"].as_array().unwrap().is_empty());
    assert!(
        agent_of_empires::session::Storage::open_unwatched("scratch-cancellation")
            .unwrap()
            .load()
            .unwrap()
            .is_empty()
    );
    let journal: serde_json::Value = serde_json::from_slice(
        &std::fs::read(daemon.app_dir().join("pending-purge-owners.json")).unwrap(),
    )
    .unwrap();
    assert!(journal["owners"].as_array().unwrap().is_empty());
}

/// RAII guard around a foreground `aoe serve` subprocess scoped to an
/// isolated `HOME`. `Drop` kills the child and waits, even on test panic.
struct ServeDaemon {
    child: Option<Child>,
    port: u16,
    home: TempDir,
    scope_slice: Option<String>,
    _runtime: TempDir,
}

impl ServeDaemon {
    /// Spawn `aoe serve --no-auth --host 127.0.0.1 --port <free>` against
    /// a fresh `HOME`. Panics on startup failure so the test gives a
    /// useful diagnostic.
    fn spawn() -> Self {
        Self::spawn_with_scopes(false).expect("unscoped fixture")
    }

    fn spawn_with_scopes(scoped: bool) -> Option<Self> {
        let aoe = env!("CARGO_BIN_EXE_aoe");
        let home = tempfile::tempdir().expect("home tempdir");
        let runtime = if cfg!(unix) {
            tempfile::tempdir_in("/tmp")
        } else {
            tempfile::tempdir()
        }
        .expect("short private runtime");
        let port = pick_free_port();
        let app = if cfg!(any(target_os = "linux", target_os = "macos")) {
            home.path()
                .join(".config")
                .join(agent_of_empires::session::APP_DIR_NAME_XDG)
        } else {
            home.path()
                .join(agent_of_empires::session::APP_DIR_NAME_OTHER)
        };
        std::fs::create_dir_all(&app).unwrap();
        let scope_slice = scoped.then(|| {
            format!(
                "background-aoe_integration_{}.slice",
                uuid::Uuid::new_v4().simple()
            )
        });
        #[cfg(target_os = "linux")]
        if let Some(slice) = &scope_slice {
            if !scope_available(runtime.path(), slice) {
                return None;
            }
        }
        let mut daemon = Self {
            child: None,
            port,
            home,
            scope_slice,
            _runtime: runtime,
        };
        let scope_config = match &daemon.scope_slice {
            Some(slice) => format!("[systemd]\nenabled = true\nslice = '{slice}'\n"),
            None => "[systemd]\nenabled = false\n".to_string(),
        };
        std::fs::write(app.join("config.toml"), scope_config).unwrap();

        let mut cmd = manager_command(aoe, daemon._runtime.path(), daemon.scope_slice.is_some());
        cmd.args([
            "serve",
            "--no-auth",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ]);
        cmd.env("HOME", daemon.home.path());
        cmd.env("XDG_CONFIG_HOME", daemon.home.path().join(".config"));
        cmd.env_remove("AOE_SERVE_PASSPHRASE");
        cmd.env_remove("AOE_CITYHALL_MODE");
        cmd.env_remove("AGENT_OF_EMPIRES_DEBUG");

        daemon.child = Some(cmd.spawn().expect("spawn aoe serve"));
        assert!(
            wait_for_port(port, Duration::from_secs(15)),
            "aoe serve did not bind 127.0.0.1:{port} within 15s"
        );
        Some(daemon)
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    fn app_dir(&self) -> PathBuf {
        let home = self.home.path();
        // Mirrors `get_app_dir_path`'s per-platform resolution: Linux and
        // macOS both resolve via `XDG_CONFIG_HOME` (forwarded to the child
        // unconditionally above), while Windows ignores that env var
        // entirely and falls back to the legacy home-dotfile path.
        if cfg!(any(target_os = "linux", target_os = "macos")) {
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
        if let Some(slice) = &self.scope_slice {
            let _ = manager_command("systemctl", self._runtime.path(), true)
                .args(["--user", "stop", slice])
                .output();
        }
    }
}

/// Probe the same manager transport and scope flags the fixture daemon uses.
#[cfg(target_os = "linux")]
fn scope_available(runtime: &std::path::Path, slice: &str) -> bool {
    let manager = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", nix::unistd::geteuid())));
    for name in ["bus", "systemd"] {
        let source = manager.join(name);
        if source.exists() {
            std::os::unix::fs::symlink(source, runtime.join(name)).unwrap();
        }
    }
    let mut probe = manager_command("systemd-run", runtime, true);
    probe
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "--no-ask-password",
            "--expand-environment=no",
        ])
        .arg(format!("--slice={slice}"))
        .arg(format!(
            "--unit=aoe-integration-probe-{}.scope",
            uuid::Uuid::new_v4().simple()
        ))
        .args([
            "--description=aoe integration capability probe",
            "--",
            "true",
        ]);
    match agent_of_empires::process::run_with_timeout(&mut probe, Duration::from_secs(5)) {
        Ok(Some(output)) if output.status.success() => true,
        result => {
            eprintln!("Scoped escapee coverage unavailable; unscoped cancellation was exercised: {result:?}");
            let _ = manager_command("systemctl", runtime, true)
                .args(["--user", "stop", slice])
                .output();
            assert!(
                std::env::var_os("AOE_REQUIRE_SCOPED_TESTS").is_none(),
                "Required scoped cancellation coverage unavailable: {result:?}"
            );
            false
        }
    }
}

fn manager_command(program: &str, runtime: &std::path::Path, scoped: bool) -> Command {
    let bus = if !scoped {
        "unix:path=/dev/null".to_string()
    } else if runtime.join("bus").exists() {
        format!("unix:path={}/bus", runtime.display())
    } else {
        std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .unwrap_or_else(|_| "unix:path=/dev/null".to_string())
    };
    let mut command = Command::new(program);
    command.env("XDG_RUNTIME_DIR", runtime);
    command.env("DBUS_SESSION_BUS_ADDRESS", bus);
    command
}
