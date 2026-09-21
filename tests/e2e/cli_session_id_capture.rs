//! End-to-end coverage for pane-attributed OMP native session capture.
//!
//! The fake OMP processes publish generation-qualified metadata through the
//! pane's tmux environment. Tests cover CLI-only start/restart persistence,
//! same-cwd isolation, reconstruction, and stale-artifact rejection without a
//! daemon.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, wait_until, write_executable, TuiTestHarness};

/// A string field of the session titled `title`.
fn session_field(h: &TuiTestHarness, title: &str, field: &str) -> Option<String> {
    h.try_read_sessions()
        .as_array()?
        .iter()
        .find(|s| s["title"].as_str() == Some(title))?
        .get(field)?
        .as_str()
        .map(str::to_owned)
}

fn agent_session_id(h: &TuiTestHarness, title: &str) -> Option<String> {
    session_field(h, title, "agent_session_id")
}

fn clear_omp_capture_generation(h: &TuiTestHarness, title: &str) {
    let path = h.sessions_path();
    let mut sessions = h.read_sessions();
    let session = sessions
        .as_array_mut()
        .and_then(|rows| {
            rows.iter_mut()
                .find(|row| row["title"].as_str() == Some(title))
        })
        .expect("find OMP session row");
    session
        .as_object_mut()
        .expect("session row is an object")
        .remove("omp_capture_generation");
    fs::write(
        path,
        serde_json::to_vec_pretty(&sessions).expect("serialize sessions"),
    )
    .expect("write legacy sessions row");
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

struct StopSessionOnDrop<'a> {
    h: &'a TuiTestHarness,
    title: &'a str,
}

impl Drop for StopSessionOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.h.run_cli(&["session", "stop", self.title]);
    }
}
fn configure_fresh_restart_capture(h: &TuiTestHarness) {
    h.append_config("[session]\nauto_resume_on_restart = false\nrestart_wake_message = \"\"");
}

const OMP_TITLE_FIRST: &str = "CliSidOmpFirstE2E";
const OMP_TITLE_SECOND: &str = "CliSidOmpSecondE2E";
const OMP_SID_FIRST: &str = "019342ab-1234-7def-8901-cccccccccccc";
const OMP_SID_SECOND: &str = "019342ab-1234-7def-8901-dddddddddddd";
const OMP_SID_THIRD: &str = "019342ab-1234-7def-8901-ffffffffffff";
const OMP_STALE_SID: &str = "019342ab-1234-7def-8901-eeeeeeeeeeee";
const OMP_CAPTURE_META_KEY: &str = "AOE_OMP_CAPTURE_META";
const OMP_LAUNCH_ID_KEY: &str = "AOE_OMP_LAUNCH_ID";
const OMP_CAPTURE_READY_KEY: &str = "AOE_OMP_CAPTURE_READY";
const OMP_ROUTING_SECRET: &str = "/aoe-e2e-sensitive-routing-value";

fn write_project_omp_dotenv(project: &Path, store: &Path) {
    fs::write(
        project.join(".env"),
        format!(
            "OMP_CODING_AGENT_DIR={}\nPI_CONFIG_DIR={OMP_ROUTING_SECRET}\n",
            store.display()
        ),
    )
    .expect("write project OMP dotenv");
}

fn install_path_preserving_test_shell(h: &mut TuiTestHarness, path_bin: &Path) {
    let shell = h.home_path().join("omp-test-shell");
    write_executable(
        &shell,
        &format!(
            "#!/bin/sh\n[ \"${{1-}}\" = -l ] && shift\nexport PATH={}:\"$PATH\"\nexec /bin/sh \"$@\"\n",
            sh_quote(path_bin)
        ),
    );
    h.set_env("SHELL", shell.to_str().expect("UTF-8 OMP test shell"));
}

fn launched_tmux_name(h: &TuiTestHarness, title: &str) -> String {
    let sessions = h.read_sessions();
    let id = sessions
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["title"].as_str() == Some(title)))
        .and_then(|row| row["id"].as_str())
        .unwrap_or_else(|| panic!("no session titled {title:?}"));
    agent_of_empires::tmux::Session::generate_name(id, title)
}

/// Run `tmux <args>` against the session's pane and return stdout.
fn tmux_query(h: &TuiTestHarness, title: &str, args: &[&str]) -> String {
    let name = launched_tmux_name(h, title);
    let output = h
        .tmux()
        .arg(args[0])
        .args(["-t", &name])
        .args(&args[1..])
        .output()
        .expect("tmux query");
    assert!(
        output.status.success(),
        "tmux {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn tmux_pane_start_command(h: &TuiTestHarness, title: &str) -> String {
    tmux_query(
        h,
        title,
        &["display-message", "-p", "#{pane_start_command}"],
    )
}

fn unset_tmux_environment(h: &TuiTestHarness, title: &str, key: &str) {
    let name = launched_tmux_name(h, title);
    let output = h
        .tmux()
        .args(["set-environment", "-h", "-u", "-t", &name, key])
        .output()
        .expect("unset tmux environment");
    assert!(
        output.status.success(),
        "tmux set-environment -u failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_past_tmux_creation_second(h: &TuiTestHarness, title: &str) {
    let created_secs: u64 = tmux_query(h, title, &["display-message", "-p", "#{session_created}"])
        .trim()
        .parse()
        .expect("tmux session_created must be epoch seconds");
    wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_secs();
        (now_secs > created_secs)
            .then_some(())
            .ok_or_else(|| format!("still inside tmux creation second {created_secs}"))
    });
}

fn wait_for_path(path: &Path) {
    wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        path.exists()
            .then_some(())
            .ok_or_else(|| format!("{} does not exist", path.display()))
    });
}

fn wait_for_agent_session_id(h: &TuiTestHarness, title: &str, expected: &str) {
    wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
        let actual = agent_session_id(h, title);
        if actual.as_deref() == Some(expected) {
            Ok(())
        } else {
            Err(format!("{title:?} has {actual:?}, want {expected}"))
        }
    });
}

fn install_toggling_fake_omp(h: &mut TuiTestHarness, project: &Path, omp_store: &Path) -> PathBuf {
    let control = h.home_path().join("omp-toggle-control");
    fs::create_dir_all(&control).expect("create fake OMP control directory");
    let bin = h.install_path_command("omp");
    install_path_preserving_test_shell(h, &bin);
    let script = format!(
        "#!/bin/sh\n\
         control={control}\n\
         if [ ! -f \"$control/launched-first\" ]; then\n\
           : > \"$control/launched-first\"; slot=first; sid={first}\n\
         elif [ ! -f \"$control/launched-second\" ]; then\n\
           : > \"$control/launched-second\"; slot=second; sid={second}\n\
         else\n\
           slot=third; sid={third}\n\
         fi\n\
         received={store}\n\
         sessions_dir=\"$received/sessions/home-project\"\n\
         terminal_dir=\"$received/terminal-sessions\"\n\
         mkdir -p \"$sessions_dir\" \"$terminal_dir\"\n\
         session_path=\"$sessions_dir/2026-08-05T00-00-00-000Z_${{sid}}.jsonl\"\n\
         printf '{{\"type\":\"title\",\"v\":1,\"title\":\"Fake OMP capture\",\"source\":\"user\",\"updatedAt\":\"2026-08-05T00:00:00.000Z\",\"pad\":\"\"}}\\n' > \"$session_path\"\n\
         printf '{{\"type\":\"session\",\"version\":3,\"id\":\"%s\",\"timestamp\":\"2026-08-05T00:00:00.000Z\",\"cwd\":\"%s\"}}\\n' \"$sid\" {cwd} >> \"$session_path\"\n\
         tty_path=$(tty) || exit 1\n\
         terminal_id=$(printf '%s' \"${{tty_path#/dev/}}\" | tr '/' '-')\n\
         printf '%s\\n%s\\nfresh\\n' {cwd} \"$session_path\" > \"$terminal_dir/$terminal_id\"\n\
         : > \"$control/ready-$slot\"\n\
         exec sleep 300\n",
        control = sh_quote(&control),
        first = OMP_SID_FIRST,
        second = OMP_SID_SECOND,
        third = OMP_SID_THIRD,
        cwd = sh_quote(project),
        store = sh_quote(omp_store),
    );
    write_executable(&bin.join("omp"), &script);
    control
}

fn write_stale_omp_session(store: &Path, old_project: &Path) -> PathBuf {
    let path = store.join(format!(
        "sessions/old-project/2020-01-01T00-00-00-000Z_{OMP_STALE_SID}.jsonl"
    ));
    fs::create_dir_all(path.parent().expect("stale session parent"))
        .expect("create stale OMP session directory");
    fs::write(
        &path,
        format!(
            "{{\"type\":\"title\",\"v\":1,\"title\":\"Stale OMP session\",\"source\":\"user\",\"updatedAt\":\"2020-01-01T00:00:00.000Z\",\"pad\":\"\"}}\n\
             {{\"type\":\"session\",\"version\":3,\"id\":\"{OMP_STALE_SID}\",\"timestamp\":\"2020-01-01T00:00:00.000Z\",\"cwd\":\"{}\"}}\n",
            old_project.display()
        ),
    )
    .expect("write stale OMP session");
    path
}

fn install_reconstructing_fake_omp(
    h: &mut TuiTestHarness,
    metadata_store: &Path,
    legacy_store: &Path,
    project: &Path,
    old_project: &Path,
) {
    let bin = h.install_path_command("omp");
    install_path_preserving_test_shell(h, &bin);
    let control = h.home_path().join("omp-control");
    fs::create_dir_all(&control).expect("create fake OMP control directory");
    let metadata_stale = write_stale_omp_session(metadata_store, old_project);
    let legacy_stale = write_stale_omp_session(legacy_store, old_project);
    let script = format!(
        "#!/bin/sh\n\
         control={control}\n\
         if [ -f \"$control/launched\" ]; then\n\
           slot=legacy; sid={second}; stale={legacy_stale}; store={legacy_store}\n\
         else\n\
           : > \"$control/launched\"\n\
           slot=metadata; sid={first}; stale={metadata_stale}; store={metadata_store}\n\
         fi\n\
         sessions_dir=\"$store/sessions/home-project\"\n\
         terminal_dir=\"$store/terminal-sessions\"\n\
         mkdir -p \"$sessions_dir\" \"$terminal_dir\"\n\
         tty_path=$(tty) || exit 1\n\
         terminal_id=$(printf '%s' \"${{tty_path#/dev/}}\" | tr '/' '-')\n\
         breadcrumb=\"$terminal_dir/$terminal_id\"\n\
         printf '%s\\n%s\\n' {old_cwd} \"$stale\" > \"$breadcrumb.tmp\"\n\
         touch -t 202001010000 \"$breadcrumb.tmp\"\n\
         mv \"$breadcrumb.tmp\" \"$breadcrumb\"\n\
         : > \"$control/ready-$slot\"\n\
         while [ ! -f \"$control/release-$slot\" ]; do sleep 0.05; done\n\
         if [ \"$slot\" = metadata ]; then\n\
           printf '%s\\n%s\\n' {old_cwd} \"$stale\" > \"$breadcrumb.tmp\"\n\
         else\n\
           fresh=\"$sessions_dir/2026-08-05T00-00-00-000Z_${{sid}}.jsonl\"\n\
           printf '{{\"type\":\"title\",\"v\":1,\"title\":\"Reconstructed OMP session\",\"source\":\"user\",\"updatedAt\":\"2026-08-05T00:00:00.000Z\",\"pad\":\"\"}}\\n' > \"$fresh\"\n\
           printf '{{\"type\":\"session\",\"version\":3,\"id\":\"%s\",\"timestamp\":\"2026-08-05T00:00:00.000Z\",\"cwd\":\"%s\"}}\\n' \"$sid\" {cwd} >> \"$fresh\"\n\
           printf '%s\\n%s\\n' {cwd} \"$fresh\" > \"$breadcrumb.tmp\"\n\
         fi\n\
         mv \"$breadcrumb.tmp\" \"$breadcrumb\"\n\
         : > \"$control/switched-$slot\"\n\
         exec sleep 300\n",
        control = sh_quote(&control),
        metadata_stale = sh_quote(&metadata_stale),
        legacy_stale = sh_quote(&legacy_stale),
        metadata_store = sh_quote(metadata_store),
        legacy_store = sh_quote(legacy_store),
        first = OMP_SID_FIRST,
        second = OMP_SID_SECOND,
        old_cwd = sh_quote(old_project),
        cwd = sh_quote(project),
    );
    write_executable(&bin.join("omp"), &script);
}

#[test]
#[parallel]
fn omp_routing_restart_generation_and_same_cwd_pane_attribution_are_preserved() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("cli_sid_omp_terminal");
    let project = h.project_path();
    let omp_store = h.home_path().join("dotenv-omp-store");
    fs::create_dir_all(omp_store.join("sessions/decoy")).expect("create OMP store");
    write_project_omp_dotenv(&project, &omp_store);
    configure_fresh_restart_capture(&h);
    let control = install_toggling_fake_omp(&mut h, &project, &omp_store);

    fs::write(
        omp_store
            .join("sessions/decoy")
            .join(format!("2020-01-01T00-00-00-000Z_{OMP_STALE_SID}.jsonl")),
        format!(
            "{{\"type\":\"title\",\"v\":1,\"title\":\"Decoy OMP session\",\"source\":\"user\",\"updatedAt\":\"2020-01-01T00:00:00.000Z\",\"pad\":\"\"}}\n\
             {{\"type\":\"session\",\"version\":3,\"id\":\"{OMP_STALE_SID}\",\"timestamp\":\"2020-01-01T00:00:00.000Z\",\"cwd\":\"{}\"}}\n",
            project.display()
        ),
    )
    .expect("write OMP decoy");

    for title in [OMP_TITLE_FIRST, OMP_TITLE_SECOND] {
        h.run_cli_ok(&[
            "add",
            project.to_str().unwrap(),
            "-c",
            "omp",
            "-t",
            title,
            "--extra-args=--thinking low",
        ]);
    }
    let _stop_first = StopSessionOnDrop {
        h: &h,
        title: OMP_TITLE_FIRST,
    };
    let _stop_second = StopSessionOnDrop {
        h: &h,
        title: OMP_TITLE_SECOND,
    };

    let mut generations = Vec::new();
    for (operation, title, slot, expected) in [
        ("start", OMP_TITLE_FIRST, "first", OMP_SID_FIRST),
        ("restart", OMP_TITLE_FIRST, "second", OMP_SID_SECOND),
        ("start", OMP_TITLE_SECOND, "third", OMP_SID_THIRD),
    ] {
        h.run_cli_ok(&["session", operation, title]);
        wait_for_path(&control.join(format!("ready-{slot}")));
        assert!(
            !tmux_pane_start_command(&h, title).contains(OMP_ROUTING_SECRET),
            "{operation} must not persist dotenv-expanded routing secrets in pane argv"
        );
        wait_for_agent_session_id(&h, title, expected);
        let generation = session_field(&h, title, "omp_capture_generation")
            .unwrap_or_else(|| panic!("{operation} must persist an OMP capture generation"));
        generations.push(generation);
    }
    // Restart and a fresh same-cwd launch must each mint their own durable
    // generation; a collision would let one pane's capture clobber another's.
    let distinct: std::collections::HashSet<_> = generations.iter().collect();
    assert_eq!(
        distinct.len(),
        generations.len(),
        "each same-cwd (re)launch must mint a distinct OMP capture generation: {generations:?}"
    );
    assert_eq!(
        agent_session_id(&h, OMP_TITLE_FIRST).as_deref(),
        Some(OMP_SID_SECOND),
        "the second same-cwd launch must not steal the restarted pane's SID2"
    );
}

#[test]
#[parallel]
fn omp_reconstruction_rejects_cross_project_and_unqualified_legacy_publications() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("cli_sid_omp_reconstruction");
    let project = h.project_path();
    let old_project = h.home_path().join("unrelated-old-project");
    fs::create_dir_all(&old_project).expect("create unrelated old project");
    let metadata_store = h.home_path().join("metadata-omp-store");
    let legacy_store = h.home_path().join("legacy-omp-store");
    install_reconstructing_fake_omp(
        &mut h,
        &metadata_store,
        &legacy_store,
        &project,
        &old_project,
    );
    let control = h.home_path().join("omp-control");
    for (slot, store, title) in [
        ("metadata", &metadata_store, "OmpCrossProject"),
        ("legacy", &legacy_store, "OmpUnqualifiedLegacy"),
    ] {
        write_project_omp_dotenv(&project, store);
        h.run_cli_ok(&["add", project.to_str().unwrap(), "-c", "omp", "-t", title]);
        h.run_cli_ok(&["session", "start", title]);
        let _stop = StopSessionOnDrop { h: &h, title };
        wait_for_path(&control.join(format!("ready-{slot}")));
        assert_eq!(
            agent_session_id(&h, title),
            None,
            "pre-launch breadcrumb was adopted"
        );
        if slot == "legacy" {
            unset_tmux_environment(&h, title, OMP_CAPTURE_META_KEY);
            unset_tmux_environment(&h, title, OMP_LAUNCH_ID_KEY);
            unset_tmux_environment(&h, title, OMP_CAPTURE_READY_KEY);
            clear_omp_capture_generation(&h, title);
        }
        wait_past_tmux_creation_second(&h, title);
        fs::write(control.join(format!("release-{slot}")), "").unwrap();
        wait_for_path(&control.join(format!("switched-{slot}")));
        h.run_cli_ok(&["session", "show", title, "--json"]);
        assert_eq!(
            agent_session_id(&h, title),
            None,
            "unqualified {slot} publication was adopted by synchronous capture"
        );
        let fork = h.run_cli(&[
            "add",
            project.to_str().unwrap(),
            "-c",
            "omp",
            "-t",
            "RefusedChild",
            "--fork-from",
            title,
        ]);
        assert!(
            !fork.status.success(),
            "unqualified {slot} publication authorized a fork"
        );
    }
}

#[test]
#[parallel]
fn declared_omp_wrapper_can_resume_without_gaining_automatic_capture() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("omp_wrapper_capture_boundary");
    let project = h.project_path();
    let effective = h.home_path().join("effective");
    fs::create_dir_all(&effective).unwrap();
    let store = h.home_path().join("wrapper-store");
    fs::create_dir_all(&store).unwrap();
    let control = install_toggling_fake_omp(&mut h, &effective, &store);
    let bin = h.install_path_command("my-omp");
    let recorded = control.join("argv");
    fs::write(
        bin.join("my-omp"),
        format!(
            "#!/bin/sh\nprintf '%s\n' \"$@\" > {}\nexec {} \"$@\"\n",
            sh_quote(&recorded),
            sh_quote(&bin.join("omp")),
        ),
    )
    .unwrap();
    let config = app_dir_in(h.home_path()).join("config.toml");
    let mut doc = fs::read_to_string(&config)
        .unwrap_or_default()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    doc["session"]["custom_agents"]["my-omp"] =
        toml_edit::value(format!("my-omp --cwd {}", sh_quote(&effective)));
    doc["session"]["agent_execution_as"]["my-omp"] = toml_edit::value("omp");
    doc["session"]["agent_config_dir"]["my-omp"] = toml_edit::value(store.to_str().unwrap());
    fs::write(config, doc.to_string()).unwrap();
    let parent = "DeclaredOmpParent";
    h.run_cli_ok(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "my-omp",
        "-t",
        parent,
    ]);
    let _stop_parent = StopSessionOnDrop {
        h: &h,
        title: parent,
    };
    h.run_cli_ok(&["session", "start", parent]);
    wait_for_path(&control.join("ready-first"));
    h.run_cli_ok(&["session", "show", parent]);
    assert_eq!(
        agent_session_id(&h, parent),
        None,
        "declaring a wrapper must not authorize automatic capture"
    );

    let transcript = store.join(format!(
        "sessions/home-project/2026-08-05T00-00-00-000Z_{OMP_SID_FIRST}.jsonl"
    ));
    h.run_cli_ok(&[
        "session",
        "set-session-id",
        parent,
        OMP_SID_FIRST,
        "--store",
        transcript.to_str().unwrap(),
    ]);
    let rows: Value =
        serde_json::from_str(&fs::read_to_string(h.sessions_path()).unwrap()).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["title"] == parent)
        .unwrap();
    assert_eq!(
        row["resume_binding"]["execution"]["cwd"].as_str(),
        effective.to_str()
    );
    h.run_cli_ok(&["session", "restart", parent]);
    wait_for_path(&control.join("ready-second"));
    let argv = fs::read_to_string(recorded).unwrap();
    let argv = argv.lines().collect::<Vec<_>>();
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--resume", OMP_SID_FIRST]),
        "{argv:?}"
    );
    assert!(
        argv.windows(2).any(|pair| pair
            == [
                "--session-dir",
                transcript.parent().unwrap().to_str().unwrap()
            ]),
        "{argv:?}"
    );
}
