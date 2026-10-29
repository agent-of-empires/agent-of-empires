//! `aoe serve` must take `serve.lock` before its startup migrations, so it cannot
//! write migrations while `aoe migrate` holds the lock.

use fs2::FileExt;
use serial_test::parallel;

use crate::harness::{app_dir_in, TuiTestHarness};

const VERSION_FILE: &str = ".schema_version";

#[test]
#[parallel]
fn serve_is_refused_before_it_migrates_while_the_lock_is_held() {
    let control = TuiTestHarness::new("serve_lock_control");
    control.run_cli_ok(&["list"]);
    assert!(
        app_dir_in(control.home_path()).join(VERSION_FILE).exists(),
        "an ordinary command must migrate on a fresh home"
    );

    let h = TuiTestHarness::new("serve_lock_held");
    let app_dir = app_dir_in(h.home_path());
    std::fs::create_dir_all(&app_dir).unwrap();
    let lock = std::fs::File::create(app_dir.join("serve.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();

    let stderr = h.run_cli_err(&["serve", "--port", "0"]);
    assert!(stderr.contains("starting or running"), "{stderr}");
    assert!(
        !app_dir.join(VERSION_FILE).exists(),
        "serve ran migrations while the lock was held"
    );
}

#[test]
#[parallel]
fn direct_serve_upgrades_legacy_policy_under_its_existing_transaction() {
    struct Foreground(std::process::Child);
    impl Drop for Foreground {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for detached in [false, true] {
        let mut h = TuiTestHarness::new(if detached {
            "serve_legacy_daemon"
        } else {
            "serve_legacy_foreground"
        });
        h.stop_daemon_on_drop();
        h.run_cli_ok(&["migrate"]);
        let app = app_dir_in(h.home_path());
        std::fs::write(app.join(VERSION_FILE), "43").unwrap();
        std::fs::write(
            app.join("pending-purge-owners.json"),
            br#"{"version":2,"owners":[]}"#,
        )
        .unwrap();
        let legacy = serde_json::json!({
            "schema":2, "pid":i32::MAX, "instance_id":"legacy-instance",
            "auth_mode":"token", "remote":false, "opaque":{"retained":true}
        });
        for name in ["serve.launch", "serve.rollback.launch"] {
            std::fs::write(app.join(name), serde_json::to_vec(&legacy).unwrap()).unwrap();
        }
        std::fs::write(app.join("serve.passphrase"), "retained rollback secret").unwrap();
        let port = crate::harness::pick_free_port();
        let port_arg = port.to_string();
        let foreground = if detached {
            h.run_cli_ok(&["serve", "--daemon", "--port", &port_arg, "--no-auth"]);
            None
        } else {
            Some(Foreground(h.spawn_cli(&[
                "serve",
                "--port",
                &port_arg,
                "--no-auth",
            ])))
        };
        assert!(
            crate::harness::wait_for_port(port, std::time::Duration::from_secs(10)),
            "direct serve failed to finish its legacy migration (detached={detached})"
        );
        assert_eq!(
            std::fs::read_to_string(app.join(VERSION_FILE)).unwrap(),
            agent_of_empires::migrations::current_schema_version().to_string()
        );
        let rollback: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.rollback.launch")).unwrap())
                .unwrap();
        assert_eq!(rollback["has_passphrase"], true);
        assert_eq!(rollback["auth_mode"], "token");
        assert_eq!(rollback["remote"], false);
        assert_eq!(rollback["pid"], legacy["pid"]);
        assert_eq!(rollback["instance_id"], legacy["instance_id"]);
        assert_eq!(rollback["opaque"], legacy["opaque"]);
        let credential: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.rollback.passphrase")).unwrap())
                .unwrap();
        assert_eq!(credential["pid"], legacy["pid"]);
        assert_eq!(credential["instance_id"], legacy["instance_id"]);
        assert_eq!(credential["passphrase"], "retained rollback secret");
        let status = h.run_cli(&["serve", "--status"]);
        assert!(
            status.status.success(),
            "upgraded daemon must remain discoverable: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(status.contains("Daemon: running (PID "), "{status}");
        assert!(status.contains(&format!(":{port}")), "{status}");
        h.run_cli_ok(&["serve", "--stop"]);
        drop(foreground);
    }
}

#[test]
#[parallel]
fn explicit_profile_serve_preserves_bootstrap_and_config_recovery() {
    for (core_only, empty) in [(false, false), (false, true), (true, false), (true, true)] {
        for (config, valid) in [
            ("default_profile = [\n", false),
            ("default_profile = \"main\"\n", true),
            ("default_profile = \"missing\"\n", true),
        ] {
            let mut h = TuiTestHarness::new_in_tmp("serve_profile_bootstrap");
            h.stop_daemon_on_drop();
            h.run_cli_ok(&["migrate"]);
            let app = app_dir_in(h.home_path());
            let profiles = app.join("profiles");
            std::fs::remove_dir_all(&profiles).unwrap();
            std::fs::create_dir(&profiles).unwrap();
            if !empty {
                std::fs::create_dir(profiles.join("main")).unwrap();
            }
            std::fs::write(app.join("config.toml"), config).unwrap();
            if core_only {
                h.run_cli_ok(&["-p", "main", "serve", "--daemon", "--core-only"]);
            } else {
                let port = crate::harness::pick_free_port().to_string();
                h.run_cli_ok(&[
                    "-p",
                    "main",
                    "serve",
                    "--daemon",
                    "--port",
                    &port,
                    "--no-auth",
                ]);
            }
            assert!(
                profiles.join("main").is_dir(),
                "core={core_only}, empty={empty}, {config}"
            );
            let status = h.run_cli_ok(&["serve", "--status"]);
            assert!(status.contains("Daemon: running (PID "), "{status}");
            if valid {
                let listed = h.run_cli_ok(&["-p", "main", "list", "--json"]);
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&listed).unwrap(),
                    serde_json::json!([])
                );
            }
            h.run_cli_ok(&["serve", "--stop"]);
        }
    }
}

#[test]
#[parallel]
fn managed_controls_migrate_historical_launch_before_replay() {
    for (rollback, protected) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut h = TuiTestHarness::new(&format!("legacy_control_{rollback}_{protected}"));
        h.stop_daemon_on_drop();
        let port = crate::harness::pick_free_port();
        let port_text = port.to_string();
        let mut args = vec!["serve", "--daemon", "--port", &port_text];
        if protected {
            args.extend(["--passphrase", "legacy-auth"]);
        } else {
            args.push("--no-auth");
        }
        h.run_cli_ok(&args);
        let app = app_dir_in(h.home_path());
        let mut launch: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.launch")).unwrap()).unwrap();
        let old_pid = launch["pid"].clone();
        launch["schema"] = 2.into();
        launch.as_object_mut().unwrap().remove("has_passphrase");
        let path = if rollback {
            "serve.rollback.launch"
        } else {
            "serve.launch"
        };
        std::fs::write(app.join(path), serde_json::to_vec(&launch).unwrap()).unwrap();
        if rollback {
            std::fs::write(
                app.join("serve.rollback.passphrase"),
                serde_json::to_vec(&serde_json::json!({
                    "pid": launch["pid"], "instance_id": launch["instance_id"],
                    "passphrase": if protected { Some("legacy-auth") } else { None },
                }))
                .unwrap(),
            )
            .unwrap();
        }
        std::fs::write(app.join(VERSION_FILE), "43").unwrap();
        h.run_cli_ok(&["serve", if rollback { "--rollback" } else { "--restart" }]);
        let current: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.launch")).unwrap()).unwrap();
        assert_ne!(current["pid"], old_pid);
        assert_eq!(current["schema"], 4);
        assert_eq!(current["has_passphrase"], protected);
        assert_eq!(current["port"], launch["port"]);
        assert_eq!(current["profile"], launch["profile"]);
        assert_eq!(
            std::fs::read_to_string(app.join(VERSION_FILE)).unwrap(),
            agent_of_empires::migrations::current_schema_version().to_string()
        );
        assert!(crate::harness::wait_for_port(
            port,
            std::time::Duration::from_secs(10)
        ));
        h.run_cli_ok(&["serve", "--stop"]);
    }
}

#[test]
#[parallel]
fn managed_controls_refuse_future_launch_schema_without_stopping_predecessor() {
    for control in ["restart", "rollback", "promotion"] {
        let mut h = TuiTestHarness::new(&format!("future_launch_{control}"));
        h.stop_daemon_on_drop();
        let port = crate::harness::pick_free_port();
        if control == "promotion" {
            h.run_cli_ok(&["serve", "--core-only", "--daemon"]);
        } else {
            h.run_cli_ok(&[
                "serve",
                "--daemon",
                "--port",
                &port.to_string(),
                "--no-auth",
            ]);
        }
        let app = app_dir_in(h.home_path());
        let mut launch: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.launch")).unwrap()).unwrap();
        let original = launch.clone();
        let pid = launch["pid"].as_u64().unwrap();
        launch["schema"] = 99.into();
        let record = if control == "rollback" {
            "serve.rollback.launch"
        } else {
            "serve.launch"
        };
        let bytes = serde_json::to_vec(&launch).unwrap();
        std::fs::write(app.join(record), &bytes).unwrap();
        if control == "rollback" {
            std::fs::write(
                app.join("serve.rollback.passphrase"),
                serde_json::to_vec(&serde_json::json!({
                    "pid": launch["pid"], "instance_id": launch["instance_id"], "passphrase": null,
                }))
                .unwrap(),
            )
            .unwrap();
        }
        std::fs::write(app.join(VERSION_FILE), "43").unwrap();
        let refused = match control {
            "restart" => h.run_cli(&["serve", "--restart"]),
            "rollback" => h.run_cli(&["serve", "--rollback"]),
            _ => h.run_cli(&[
                "serve",
                "--daemon",
                "--port",
                &port.to_string(),
                "--no-auth",
            ]),
        };
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("Unsupported daemon launch schema"),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        assert_eq!(std::fs::read(app.join(record)).unwrap(), bytes);
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok(),
            "unsupported schema must not stop the verified predecessor"
        );
        let status = h.run_cli(&["serve", "--status"]);
        assert!(status.status.success());
        assert!(
            String::from_utf8_lossy(&status.stdout)
                .contains(&format!("Daemon: running (PID {pid})")),
            "{}",
            String::from_utf8_lossy(&status.stdout)
        );
        if control == "rollback" {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(
                    &std::fs::read(app.join("serve.launch")).unwrap()
                )
                .unwrap(),
                original
            );
        }
        h.run_cli_ok(&["serve", "--stop"]);
    }
}

#[test]
#[parallel]
fn managed_core_promotion_migrates_historical_launch_before_replay() {
    let mut h = TuiTestHarness::new("legacy_core_promotion");
    h.stop_daemon_on_drop();
    h.run_cli_ok(&["serve", "--core-only", "--daemon"]);
    let app = app_dir_in(h.home_path());
    let mut launch: serde_json::Value =
        serde_json::from_slice(&std::fs::read(app.join("serve.launch")).unwrap()).unwrap();
    let old_pid = launch["pid"].clone();
    launch["schema"] = 2.into();
    launch.as_object_mut().unwrap().remove("has_passphrase");
    std::fs::write(
        app.join("serve.launch"),
        serde_json::to_vec(&launch).unwrap(),
    )
    .unwrap();
    std::fs::write(app.join(VERSION_FILE), "43").unwrap();
    let port = crate::harness::pick_free_port();
    h.run_cli_ok(&[
        "serve",
        "--daemon",
        "--port",
        &port.to_string(),
        "--no-auth",
    ]);
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(app.join("serve.launch")).unwrap()).unwrap();
    assert_ne!(current["pid"], old_pid);
    assert_eq!(current["schema"], 4);
    assert_eq!(current["core_only"], false);
    assert_eq!(current["has_passphrase"], false);
    assert_eq!(current["profile"], launch["profile"]);
    assert_eq!(current["port"], port);
    assert_eq!(
        std::fs::read_to_string(app.join(VERSION_FILE)).unwrap(),
        agent_of_empires::migrations::current_schema_version().to_string()
    );
    assert!(crate::harness::wait_for_port(
        port,
        std::time::Duration::from_secs(10)
    ));
    h.run_cli_ok(&["serve", "--stop"]);
}

#[test]
#[parallel]
fn tui_restart_failure_retains_predecessor_credential_for_rotation_and_removal() {
    use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};
    use std::net::TcpListener;
    use std::time::Duration;
    for rotate in [false, true] {
        require_tmux!();
        let mut h = TuiTestHarness::new(&format!("tui_rollback_credential_{rotate}"));
        h.stop_daemon_on_drop();
        let port = crate::harness::pick_free_port();
        let port_text = port.to_string();
        let predecessor_secret = "predecessor-secret-only-for-this-fixture";
        h.run_cli_ok(&[
            "serve",
            "--daemon",
            "--port",
            &port_text,
            "--passphrase",
            predecessor_secret,
        ]);
        let app = app_dir_in(h.home_path());
        let predecessor: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.launch")).unwrap()).unwrap();
        let blocker = (49152..65535)
            .find_map(|candidate| TcpListener::bind(("0.0.0.0", candidate)).ok())
            .expect("fixture high port");
        let blocked_port = blocker.local_addr().unwrap().port();
        std::fs::write(app.join("serve.last_port"), blocked_port.to_string()).unwrap();
        if rotate {
            let tailscale = h.install_path_command("tailscale");
            std::fs::write(tailscale.join("tailscale"), "#!/bin/sh\nexit 1\n").unwrap();
            let cloudflared = h.install_path_command("cloudflared");
            std::fs::write(cloudflared.join("cloudflared"), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'cloudflared version fixture'; exit 0; fi\nexit 7\n").unwrap();
            // Drive the actual G rotation path without exposing a public fixture tunnel.
            std::fs::write(app.join("serve.mode"), "tunnel").unwrap();
        }
        h.spawn_tui();
        h.wait_for_runtime_ready();
        h.send_keys("R");
        h.wait_for("Connections");
        h.send_keys("Enter");
        h.wait_for(if rotate { "tunnel" } else { "local" });
        let key = if rotate { "g" } else { "r" };
        h.send_keys(key);
        h.send_keys(key);
        h.wait_for_timeout("Dashboard start failed", Duration::from_secs(75));
        let retained: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.rollback.launch")).unwrap())
                .unwrap();
        let credential: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.rollback.passphrase")).unwrap())
                .unwrap();
        assert_eq!(retained["instance_id"], predecessor["instance_id"]);
        assert_eq!(credential["passphrase"], predecessor_secret);
        assert_eq!(credential["instance_id"], predecessor["instance_id"]);
        drop(blocker);
        h.run_cli_ok(&["serve", "--rollback"]);
        let restored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.launch")).unwrap()).unwrap();
        assert_eq!(restored["port"], predecessor["port"]);
        assert_eq!(restored["has_passphrase"], true);
        assert_eq!(restored["remote"], false);
        assert_eq!(
            std::fs::read_to_string(app.join("serve.passphrase")).unwrap(),
            predecessor_secret
        );
    }
}

#[test]
#[parallel]
fn pending_schema_startup_gates_cli_tui_and_explicit_migrate_before_daemon_writes() {
    let mut h = TuiTestHarness::new("pending_schema_active_writer");
    h.stop_daemon_on_drop();
    h.run_cli_ok(&["serve", "--core-only", "--daemon"]);
    let app = app_dir_in(h.home_path());
    let marker = app.join(VERSION_FILE);
    std::fs::write(
        &marker,
        (agent_of_empires::migrations::current_schema_version() - 1).to_string(),
    )
    .unwrap();
    let original_marker = std::fs::read(&marker).unwrap();
    let original_launch = std::fs::read(app.join("serve.launch")).unwrap();
    for args in [&["list"][..], &[][..], &["migrate"][..]] {
        let result = h.run_cli(args);
        assert!(
            !result.status.success(),
            "startup must gate pending migrations"
        );
        assert_eq!(std::fs::read(&marker).unwrap(), original_marker);
        assert_eq!(
            std::fs::read(app.join("serve.launch")).unwrap(),
            original_launch
        );
    }
    h.run_cli_ok(&["serve", "--restart"]);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        agent_of_empires::migrations::current_schema_version().to_string()
    );
    h.run_cli_ok(&["serve", "--stop"]);
}
