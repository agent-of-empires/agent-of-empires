//! Stops what a sandboxed launch left running in its container. Stopping the pane only ends the
//! runtime's exec client; the Agent and everything it started keep running in the container.
//! A backgrounded process is reparented to the container's init, so membership comes from the
//! launch marker every sandboxed launch exports ([`crate::hooks::SESSION_SOURCE_ENV`]), not from
//! a process tree.

use std::time::Duration;

use anyhow::{ensure, Context, Result};

use super::ContainerExecutionSnapshot;

/// Bounds the stop exec: its TERM and KILL waits plus the scans between them.
pub(crate) const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// `$1` is the launch's id, `$2` the proc root (`/proc` unless a test passes one). Signals every
/// live process whose environment carries this launch's marker: TERM, then KILL for what is left
/// after about 2 s, reading `/proc` again on every pass. No environment value is printed. Prints
/// `survived <n>`.
pub(crate) const STOP_SCRIPT: &str = r#"l=$1 r=$2
pick() {
  for d in "$r"/[0-9]*; do
    grep -qzxF -- "AOE_SESSION_SOURCE=$l" "$d/environ" 2>/dev/null || continue
    s=$(cat "$d/stat" 2>/dev/null) || continue
    set -- ${s##*") "}
    [ "$1" != Z ] && [ "$1" != X ] && echo "${d##*/}"
  done
}
wait_gone() {
  i=0
  while [ $i -lt "$1" ] && [ -n "$(pick)" ]; do
    sleep 0.2 2>/dev/null || sleep 1
    i=$((i + 1))
  done
}
v=$(pick)
[ -n "$v" ] && kill -TERM $v 2>/dev/null
wait_gone 10
v=$(pick)
[ -n "$v" ] && kill -KILL $v 2>/dev/null
wait_gone 5
n=0
for p in $(pick); do n=$((n + 1)); done
printf 'survived %s\n' "$n"
"#;

/// Stops this launch's processes in the container. An error when the exec fails or any survive.
pub(crate) fn stop(container: &ContainerExecutionSnapshot, launch_id: &str) -> Result<()> {
    let args = ["/bin/sh", "-c", STOP_SCRIPT, "sh", launch_id, "/proc"].map(String::from);
    let output = container
        .runtime
        .exec_output(&container.id, &args, STOP_TIMEOUT)?;
    ensure!(
        output.status.success(),
        "container stop exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let survived: usize = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("survived "))
        .context("container stop gave no count")?
        .trim()
        .parse()
        .context("container stop count")?;
    ensure!(
        survived == 0,
        "{survived} processes of the old launch survived the container stop"
    );
    Ok(())
}

/// A runtime program for tests: it writes its argv, NUL-separated, to `dir/argv`, then runs
/// `tail` as shell with the same arguments.
#[cfg(test)]
pub(crate) fn fake_container(
    dir: &std::path::Path,
    name: &str,
    tail: &str,
) -> ContainerExecutionSnapshot {
    use std::os::unix::fs::PermissionsExt;

    let program = dir.join(name);
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > '{}'\n{tail}\n",
            dir.join("argv").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    ContainerExecutionSnapshot {
        runtime: super::RuntimeExecutionSnapshot {
            kind: crate::session::ContainerRuntimeName::Docker,
            program,
            cwd: dir.to_path_buf(),
            endpoint: String::new(),
            local_mounts: true,
            routing: Vec::new(),
            global_arguments: Vec::new(),
        },
        name: "aoe-test".into(),
        id: "container-id".into(),
        mounts: Vec::new(),
        shadow_mounts: Vec::new(),
    }
}

/// The argv [`fake_container`] last recorded in `dir`.
#[cfg(test)]
pub(crate) fn recorded_argv(dir: &std::path::Path) -> Vec<String> {
    std::fs::read(dir.join("argv"))
        .unwrap()
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kills on drop what a stop test started, so a failed assertion leaves nothing running.
    struct Started(Vec<(&'static str, std::process::Child)>);

    impl Drop for Started {
        fn drop(&mut self) {
            for (_, child) in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn spawn_marked(argv: &[&str], marker: Option<&str>) -> std::process::Child {
        let mut command = std::process::Command::new(argv[0]);
        command.args(&argv[1..]).env_remove("AOE_SESSION_SOURCE");
        if let Some(marker) = marker {
            command.env("AOE_SESSION_SOURCE", marker);
        }
        command.spawn().unwrap()
    }

    /// The stop, on real processes: the script's proc root holds only this test's processes, so
    /// nothing else on the machine is read or signalled. This launch's go, a TERM-ignoring one
    /// by KILL; an earlier launch's, a prefix of this launch's id, and an unmarked one stay.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_stop_takes_only_this_launch_s_processes() {
        use std::os::unix::process::ExitStatusExt;

        let current = uuid::Uuid::new_v4().to_string();
        let earlier = uuid::Uuid::new_v4().to_string();
        let prefix = current[..8].to_owned();
        let sleep = ["sleep", "600"];
        let mut started = Started(vec![
            ("current", spawn_marked(&sleep, Some(&current))),
            (
                "ignores TERM",
                spawn_marked(
                    &["sh", "-c", "trap '' TERM; exec sleep 600"],
                    Some(&current),
                ),
            ),
            ("earlier", spawn_marked(&sleep, Some(&earlier))),
            ("prefix", spawn_marked(&sleep, Some(&prefix))),
            ("unmarked", spawn_marked(&sleep, None)),
        ]);
        let trapped = started.0[1].1.id();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !std::fs::read(format!("/proc/{trapped}/cmdline"))
            .unwrap()
            .starts_with(b"sleep")
        {
            assert!(std::time::Instant::now() < deadline, "the trap never ran");
            std::thread::sleep(Duration::from_millis(10));
        }
        let root = tempfile::tempdir().unwrap();
        for (_, child) in &started.0 {
            let pid = child.id();
            std::os::unix::fs::symlink(format!("/proc/{pid}"), root.path().join(pid.to_string()))
                .unwrap();
        }

        let output = std::process::Command::new("/bin/sh")
            .args(["-c", STOP_SCRIPT, "sh", &current])
            .arg(root.path())
            .env_remove("AOE_SESSION_SOURCE")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "survived 0\n",
            "{output:?}"
        );
        let outcomes: Vec<_> = started
            .0
            .iter_mut()
            .map(|(name, child)| {
                let status = child.try_wait().unwrap();
                (*name, status.map(|status| status.signal()))
            })
            .collect();
        assert_eq!(
            outcomes,
            [
                ("current", Some(Some(15))),
                ("ignores TERM", Some(Some(9))),
                ("earlier", None),
                ("prefix", None),
                ("unmarked", None),
            ]
        );
    }

    /// The host side of the stop: the constant script with the launch id as its own argument,
    /// and only a reported count of zero is success.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_stop_succeeds_only_when_nothing_survives() {
        let bin = tempfile::tempdir().unwrap();
        for (name, tail, ok) in [
            ("stopped", "echo 'survived 0'", true),
            ("survivor", "echo 'survived 2'", false),
            ("silent", "true", false),
            ("fails", "echo 'survived 0'; exit 1", false),
        ] {
            let container = fake_container(bin.path(), name, tail);
            assert_eq!(stop(&container, "launch-2").is_ok(), ok, "{name}");
            let want = [
                "exec",
                "container-id",
                "/bin/sh",
                "-c",
                STOP_SCRIPT,
                "sh",
                "launch-2",
                "/proc",
            ];
            assert_eq!(recorded_argv(bin.path()), want, "{name}");
        }
    }
}
