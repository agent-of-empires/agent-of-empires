//! End-to-end coverage for the OMP capture boundary.
//!
//! Core no longer captures OMP sessions itself, so the fake OMP processes
//! publish breadcrumbs and session stores that must never be adopted. A session
//! id that the user pins still drives resume.

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
    for key in [
        "NODE_ENV",
        "OMP_PROFILE",
        "PI_PROFILE",
        "PI_CODING_AGENT_DIR",
        "PI_CODING_AGENT_SESSION_DIR",
        "PI_CONFIG_DIR",
        "OMP_CODING_AGENT_DIR",
        "OMP_CODING_AGENT_SESSION_DIR",
        "OMP_CONFIG_DIR",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "AOE_INSTANCE_ID",
    ] {
        h.remove_env(key);
    }
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
         if [ ! -f \"$control/launched-metadata\" ]; then\n\
           : > \"$control/launched-metadata\"\n\
           slot=metadata; sid={first}; stale={metadata_stale}; store={metadata_store}\n\
         elif [ ! -f \"$control/launched-legacy\" ]; then\n\
           : > \"$control/launched-legacy\"\n\
           slot=legacy; sid={second}; stale={legacy_stale}; store={legacy_store}\n\
         else\n\
           : > \"$control/launched-resident\"\n\
           slot=resident; sid={third}; stale={metadata_stale}; store={metadata_store}\n\
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
        third = OMP_SID_THIRD,
        old_cwd = sh_quote(old_project),
        cwd = sh_quote(project),
    );
    write_executable(&bin.join("omp"), &script);
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
        // Same project, post-launch, current generation: what the removed
        // built-in capture would have adopted.
        ("resident", &metadata_store, "OmpResidentPublication"),
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
