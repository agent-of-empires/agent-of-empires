//! Daemon-only CLI reads against isolated homes and owned endpoints.

use std::path::PathBuf;
use std::process::{Command, Output};

use agent_of_empires::session::{Instance, View, APP_DIR_NAME_XDG};
use serial_test::parallel;

struct Fixture {
    home: tempfile::TempDir,
    app: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::TempDir::new_in("/tmp").unwrap();
        let app = home.path().join(".config").join(APP_DIR_NAME_XDG);
        let profile = app.join("profiles/main");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            app.join("config.toml"),
            "default_profile = \"main\"\n[updates]\nupdate_check_mode = \"off\"\n[systemd]\nenabled = false\n",
        )
        .unwrap();
        let mut row = Instance::new("Disk must not answer", "/isolated/project");
        row.id = "daemon-only-row".into();
        row.view = View::Structured;
        std::fs::write(
            profile.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        Self { home, app }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aoe"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            .env("XDG_RUNTIME_DIR", self.home.path().join("run"))
            .env("AOE_TMUX_SOCKET", self.home.path().join("tmux.sock"))
            .env("NO_COLOR", "1")
            .current_dir(self.home.path())
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn assert_not_started(&self) {
        for path in [
            "serve.pid",
            "serve.launch",
            "daemon/api.sock",
            "runtime.sock",
        ] {
            assert!(!self.app.join(path).exists(), "unexpected {path}");
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.app.join("serve.pid").exists() {
            let _ = self.run(&["serve", "--stop"]);
        }
        let _ = Command::new("tmux")
            .args(["-S"])
            .arg(self.home.path().join("tmux.sock"))
            .arg("kill-server")
            .output();
    }
}

fn assert_refused(output: &Output, code: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unexpected success: {output:?}");
    assert!(output.stdout.is_empty(), "unexpected answer: {output:?}");
    assert!(stderr.contains(code), "{stderr}");
    assert!(!stderr.contains("local store"), "{stderr}");
    assert!(!stderr.contains("Started aoe daemon"), "{stderr}");
}

#[tokio::test]
#[parallel]
async fn unreachable_selected_targets_never_answer_from_disk() {
    let fixture = Fixture::new();
    // Retain the bound, non-listening socket so no other process can take the port.
    let unused = tokio::net::TcpSocket::new_v4().unwrap();
    unused.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let url = format!("http://{}", unused.local_addr().unwrap());
    for explicit in [false, true] {
        for disabled in [false, true] {
            let mut command = fixture.command(&["list", "--json"]);
            if explicit {
                command.args(["--daemon-url", &url]);
            } else {
                command.env("AOE_DAEMON_URL", &url);
            }
            if disabled {
                command.env("AOE_NO_AUTOSTART", "1");
            }
            let output = command.output().unwrap();
            assert_refused(&output, "daemon read:");
            let stderr = String::from_utf8_lossy(&output.stderr);
            for option in ["--daemon-url", "AOE_DAEMON_URL"] {
                assert!(
                    stderr.contains(option),
                    "missing reconnect guidance: {stderr}"
                );
            }
            fixture.assert_not_started();
        }
    }
}

#[test]
#[parallel]
fn absent_local_reads_honor_both_autostart_opt_outs() {
    let fixture = Fixture::new();
    let commands: &[&[&str]] = &[
        &["list", "--json"],
        &["list", "--all", "--json", "-p", "unknown"],
        &["status", "--json"],
        &["session", "show", "daemon-only-row", "--json"],
        &["session", "list-trash"],
        &["group", "list", "--json"],
        &["profile"],
        &["project", "list", "--scope", "global", "--json"],
    ];
    for args in commands {
        for flag in [false, true] {
            let mut command = fixture.command(args);
            if flag {
                command.arg("--no-autostart");
            } else {
                command.env("AOE_NO_AUTOSTART", "1");
            }
            let output = command.output().unwrap();
            assert_refused(&output, "daemon read:");
            assert!(String::from_utf8_lossy(&output.stderr).contains("aoe serve"));
            fixture.assert_not_started();
        }
    }
}

#[test]
#[parallel]
fn concurrent_reads_bootstrap_once_and_reuse_the_core_daemon() {
    let fixture = Fixture::new();
    let children: Vec<_> = (0..4)
        .map(|_| {
            fixture
                .command(&["list", "--json"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut announcements = Vec::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(rows[0]["id"], "daemon-only-row");
        let stderr = String::from_utf8(output.stderr).unwrap();
        announcements.extend(
            stderr
                .lines()
                .filter(|line| line.starts_with("Started aoe daemon (pid "))
                .map(str::to_owned),
        );
    }
    let pid = std::fs::read_to_string(fixture.app.join("serve.pid")).unwrap();
    assert_eq!(
        announcements,
        [format!("Started aoe daemon (pid {})", pid.trim())]
    );
    let launch: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.app.join("serve.launch")).unwrap()).unwrap();
    assert_eq!(launch["core_only"], true);
    assert!(!fixture.app.join("serve.url").exists());
    for flag in [false, true] {
        let mut command = fixture.command(&["list", "--json"]);
        if flag {
            command.arg("--no-autostart");
        } else {
            command.env("AOE_NO_AUTOSTART", "1");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(rows[0]["id"], "daemon-only-row");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("Started aoe daemon"));
        assert_eq!(
            std::fs::read_to_string(fixture.app.join("serve.pid")).unwrap(),
            pid
        );
    }
    let all = fixture.run(&["list", "--all", "--json", "-p", "unknown"]);
    assert!(all.status.success(), "{all:?}");
    let rows: serde_json::Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(rows[0]["id"], "daemon-only-row");
    assert_eq!(rows[0]["profile"], "main");
    let missing = fixture.run(&["list", "--json", "-p", "unknown"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("Profile 'unknown' does not exist"));
    assert!(!fixture.app.join("profiles/unknown").exists());
}

#[test]
#[parallel]
fn failed_local_acquisition_never_uses_the_store() {
    let fixture = Fixture::new();
    let migrated = fixture.run(&["migrate"]);
    assert!(migrated.status.success(), "{migrated:?}");
    std::fs::write(fixture.app.join("serve.pid"), b"unverifiable owner").unwrap();
    let output = fixture.run(&["list", "--json"]);
    assert_refused(&output, "daemon read: unavailable");
    assert!(String::from_utf8_lossy(&output.stderr).contains("Cannot verify"));
    assert_eq!(
        std::fs::read(fixture.app.join("serve.pid")).unwrap(),
        b"unverifiable owner"
    );
    assert!(!fixture.app.join("serve.launch").exists());
    assert!(!fixture.app.join("daemon/api.sock").exists());
}

#[test]
#[parallel]
fn legacy_publication_artifacts_are_ignored() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(&fixture.app).unwrap();
    let artifacts = [
        (
            "runtime.prebind.json",
            br#"{"schema":255,"future_field":true}"#.as_slice(),
        ),
        ("runtime.postbind.json", b"{]".as_slice()),
        (
            "runtime.postbind.json.tmp.11111111-2222-3333-4444-555555555555",
            b"{]".as_slice(),
        ),
    ];
    for (name, bytes) in artifacts {
        std::fs::write(fixture.app.join(name), bytes).unwrap();
    }
    let output = fixture.run(&["list", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows[0]["id"], "daemon-only-row");
    for (name, bytes) in artifacts {
        assert_eq!(
            std::fs::read(fixture.app.join(name)).unwrap(),
            bytes,
            "{name}"
        );
    }
}

#[tokio::test]
#[parallel]
async fn a_cityhall_daemon_refuses_reads_with_the_typed_cityhall_mode() {
    use agent_of_empires::daemon::{DaemonClient, RUNTIME_PROTOCOL_VERSION};
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::net::UnixDatagram;

    struct Foreground(std::process::Child);
    impl Drop for Foreground {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let fixture = Fixture::new();
    let notify_path = fixture.home.path().join("ready.sock");
    let notify = UnixDatagram::bind(&notify_path).unwrap();
    let log_path = fixture.home.path().join("serve.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut foreground = Foreground(
        fixture
            .command(&["serve", "--core-only", "--cityhall"])
            .env("NOTIFY_SOCKET", &notify_path)
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut message = [0; 1024];
        loop {
            let size = notify.recv(&mut message).await.unwrap();
            if message[..size]
                .split(|byte| *byte == b'\n')
                .any(|field| field == b"READY=1")
            {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "Daemon did not become ready: {error}; {}",
            std::fs::read_to_string(log_path).unwrap()
        )
    });
    let client = DaemonClient::new_unix(fixture.app.join("daemon/api.sock")).unwrap();
    let ready = client.runtime_info().await.unwrap();
    assert_eq!(ready.protocol_version, RUNTIME_PROTOCOL_VERSION);
    assert_eq!(
        ready.namespace,
        fixture.app.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(ready.local_owner && ready.cityhall_mode);
    assert!(!ready.epoch.is_empty());
    let pid = std::fs::read_to_string(fixture.app.join("serve.pid")).unwrap();
    assert_eq!(pid.trim().parse::<u32>().unwrap(), foreground.0.id());
    let output = fixture.run(&["list", "--json", "--no-autostart"]);
    assert_refused(&output, "daemon read: cityhall_mode");
    assert_eq!(output.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("--daemon-url"), "{stderr}");
    assert!(!stderr.contains("AOE_DAEMON_URL"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(fixture.app.join("serve.pid")).unwrap(),
        pid
    );
    assert!(foreground.0.try_wait().unwrap().is_none());
}

#[tokio::test]
#[parallel]
async fn peer_refusals_never_switch_to_a_live_local_daemon() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{
        handshake::server::{Request, Response},
        Message,
    };

    let fixture = Fixture::new();
    let local = fixture.run(&["list", "--json"]);
    assert!(local.status.success(), "{local:?}");
    let local_rows: serde_json::Value = serde_json::from_slice(&local.stdout).unwrap();
    assert_eq!(local_rows[0]["id"], "daemon-only-row");
    let pid = std::fs::read(fixture.app.join("serve.pid")).unwrap();
    let cases: &[(u16, &[&str], &str)] = &[
        (401, &[], "unauthorized"),
        (403, &[], "unauthorized"),
        (403, &["cityhall_mode"], "cityhall_mode"),
        (403, &["cityhall_mode", "cityhall_mode"], "unauthorized"),
        (401, &["cityhall_mode"], "unauthorized"),
        (403, &["unknown_future_code"], "unauthorized"),
        (404, &["cityhall_mode"], "server_error"),
        (101, &[], "protocol_mismatch"),
    ];
    for &(status, headers, code) in cases {
        for explicit in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let peer = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let socket = tokio_tungstenite::accept_hdr_async(
                    stream,
                    move |_: &Request, response: Response| {
                        if status == 101 {
                            Ok(response)
                        } else {
                            let mut refusal =
                                tokio_tungstenite::tungstenite::http::Response::builder()
                                    .status(status);
                            for header in headers {
                                refusal = refusal.header("AoE-Error-Code", *header);
                            }
                            Err(refusal.body(None).unwrap())
                        }
                    },
                )
                .await;
                if status == 101 {
                    let mut socket = socket.unwrap();
                    socket
                        .send(Message::Text(
                            r#"{"kind":"hello","data":{"protocol_version":65535}}"#.into(),
                        ))
                        .await
                        .unwrap();
                    let _ = socket.next().await;
                } else {
                    assert!(socket.is_err());
                }
            });
            let mut command = fixture.command(&["list", "--json"]);
            if explicit {
                command.args(["--daemon-url", &url]);
                command.env("AOE_DAEMON_URL", "not the selected endpoint");
            } else {
                command.env("AOE_DAEMON_URL", &url);
            }
            let output = tokio::task::spawn_blocking(move || command.output().unwrap())
                .await
                .unwrap();
            assert_refused(&output, &format!("daemon read: {code}"));
            assert_eq!(output.status.code(), Some(4));
            assert_eq!(std::fs::read(fixture.app.join("serve.pid")).unwrap(), pid);
            tokio::time::timeout(std::time::Duration::from_secs(5), peer)
                .await
                .unwrap()
                .unwrap();
        }
    }
}

#[test]
#[parallel]
fn invalid_selected_targets_never_acquire_a_local_daemon() {
    use std::os::unix::ffi::OsStringExt;

    let fixture = Fixture::new();
    let cases = [
        (
            None,
            std::ffi::OsString::from("not a URL"),
            None,
            "invalid_endpoint",
        ),
        (
            None,
            std::ffi::OsString::from_vec(vec![0xff]),
            None,
            "invalid_endpoint",
        ),
        (
            None,
            std::ffi::OsString::from("http://127.0.0.1:1"),
            Some("bad token"),
            "invalid_token",
        ),
        (
            Some(""),
            std::ffi::OsString::from("http://127.0.0.1:1"),
            None,
            "invalid_endpoint",
        ),
    ];
    for (flag, url, token, code) in cases {
        let mut command = fixture.command(&["list", "--json"]);
        command.env("AOE_DAEMON_URL", url);
        if let Some(flag) = flag {
            command.args(["--daemon-url", flag]);
        }
        if let Some(token) = token {
            command.env("AOE_DAEMON_TOKEN", token);
        }
        let output = command.output().unwrap();
        assert_refused(&output, code);
        assert_eq!(output.status.code(), Some(2));
        fixture.assert_not_started();
    }
}

#[test]
#[parallel]
fn reads_do_not_need_an_unrelated_missing_default_to_bootstrap() {
    for args in [
        &["-p", "main", "list", "--json"][..],
        &["list", "--all", "--json", "-p", "unknown"],
        &["profile", "list"],
        &[
            "project", "list", "--scope", "global", "--json", "-p", "unknown",
        ],
    ] {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.app.join("config.toml"),
            "default_profile = \"missing\"\n[updates]\nupdate_check_mode = \"off\"\n[systemd]\nenabled = false\n",
        ).unwrap();
        let output = fixture.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        if args.contains(&"profile") {
            assert!(String::from_utf8_lossy(&output.stdout).contains("main"));
        } else {
            let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if args.contains(&"list") && !args.contains(&"project") {
                assert_eq!(rows[0]["id"], "daemon-only-row");
            } else {
                assert_eq!(rows, serde_json::json!([]));
            }
        }
        let default_read = fixture.run(&["list", "--json"]);
        assert_eq!(default_read.status.code(), Some(1), "{default_read:?}");
        assert!(default_read.stdout.is_empty());
        assert!(String::from_utf8_lossy(&default_read.stderr).contains("missing"));
        assert!(!fixture.app.join("profiles/missing").exists());
        assert!(!fixture.app.join("profiles/unknown").exists());
    }
}

#[test]
#[parallel]
fn empty_registry_reads_preserve_a_missing_configured_default() {
    for default in ["missing", "main"] {
        for args in [
            &["list", "--json"][..],
            &["profile", "list"],
            &["list", "--all", "--json"],
            &["project", "list", "--scope", "global", "--json"],
        ] {
            let fixture = Fixture::new();
            std::fs::remove_dir_all(fixture.app.join("profiles/main")).unwrap();
            std::fs::write(
                fixture.app.join("config.toml"),
                format!(
                    "default_profile = \"{default}\"\n[updates]\nupdate_check_mode = \"off\"\n[systemd]\nenabled = false\n"
                ),
            )
            .unwrap();
            let output = fixture.run(args);
            if args == ["list", "--json"] {
                assert_eq!(output.status.code(), Some(1), "{output:?}");
                assert!(output.stdout.is_empty(), "{output:?}");
                assert!(String::from_utf8_lossy(&output.stderr)
                    .contains(&format!("Profile '{default}' does not exist")));
            } else {
                assert!(output.status.success(), "{args:?}: {output:?}");
                if args.contains(&"project") {
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
                        serde_json::json!([])
                    );
                } else {
                    assert!(
                        String::from_utf8_lossy(&output.stdout).starts_with("No profiles found.")
                    );
                }
            }
            assert!(fixture.app.join("serve.pid").exists());
            assert_eq!(
                std::fs::read_dir(fixture.app.join("profiles"))
                    .unwrap()
                    .count(),
                0,
                "read created a profile: {args:?}"
            );
            let reused = fixture.run(&["--no-autostart", "profile", "list"]);
            assert!(reused.status.success(), "{reused:?}");
            assert!(String::from_utf8_lossy(&reused.stdout).starts_with("No profiles found."));
        }
    }
}

#[test]
#[parallel]
fn configured_alias_does_not_choose_an_unlisted_startup_profile() {
    for args in [&["-p", "main", "list", "--json"][..], &["list", "--json"]] {
        let fixture = Fixture::new();
        let migrated = fixture.run(&["migrate"]);
        assert!(migrated.status.success(), "{migrated:?}");
        std::os::unix::fs::symlink("main", fixture.app.join("profiles/account")).unwrap();
        std::fs::write(
            fixture.app.join("config.toml"),
            "default_profile = \"account\"\n[updates]\nupdate_check_mode = \"off\"\n[systemd]\nenabled = false\n",
        ).unwrap();
        let output = fixture.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(rows[0]["id"], "daemon-only-row");
        let all = fixture.run(&["list", "--all", "--json"]);
        assert!(all.status.success(), "{all:?}");
        let rows: serde_json::Value = serde_json::from_slice(&all.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["id"], "daemon-only-row");
        assert_eq!(rows[0]["profile"], "main");
        assert!(
            std::fs::symlink_metadata(fixture.app.join("profiles/account"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[tokio::test]
#[parallel]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "macOS local reads exercise shared acquisition during reuse"
)]
async fn read_reuse_does_not_require_an_unadopted_startup_profile() {
    use agent_of_empires::daemon::{DaemonClient, RuntimeHealth};
    use fs2::FileExt;

    let fixture = Fixture::new();
    std::fs::write(
        fixture.app.join("config.toml"),
        "[updates]\nupdate_check_mode = \"off\"\n[systemd]\nenabled = false\n",
    )
    .unwrap();
    let started = fixture.run(&["list", "--json"]);
    assert!(started.status.success(), "{started:?}");
    let client = DaemonClient::new_unix(fixture.app.join("daemon/api.sock")).unwrap();
    let before = client.runtime_info().await.unwrap();
    assert_eq!(before.health, RuntimeHealth::Healthy);
    assert_eq!(before.profiles, ["main"]);
    let pid = std::fs::read(fixture.app.join("serve.pid")).unwrap();
    let launch = std::fs::read(fixture.app.join("serve.launch")).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.app.join("profiles/main/.storage.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    std::fs::create_dir(fixture.app.join("profiles/aaa")).unwrap();
    let held = client.runtime_info().await.unwrap();
    assert_eq!(held.health, RuntimeHealth::Healthy);
    assert_eq!(held.profiles, ["main"]);
    for args in [
        &["-p", "main", "list", "--json"][..],
        &["profile", "list"],
        &["project", "list", "--scope", "global", "--json"],
    ] {
        let output = fixture.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("Started aoe daemon"));
        if args.contains(&"profile") {
            let profiles = String::from_utf8_lossy(&output.stdout);
            assert!(profiles.contains("main"), "{profiles}");
            assert!(profiles.contains("aaa"), "{profiles}");
        } else {
            let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if args.contains(&"project") {
                assert_eq!(rows, serde_json::json!([]));
            } else {
                assert_eq!(rows[0]["id"], "daemon-only-row");
            }
        }
    }
    assert_eq!(std::fs::read(fixture.app.join("serve.pid")).unwrap(), pid);
    assert_eq!(
        std::fs::read(fixture.app.join("serve.launch")).unwrap(),
        launch
    );
    let held = client.runtime_info().await.unwrap();
    assert_eq!(held.health, RuntimeHealth::Healthy);
    assert_eq!(held.profiles, ["main"]);
}
