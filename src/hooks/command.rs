//! Shell commands AoE installs as agent hooks, and recognition of them.

use crate::agents::{HookIdentityField, HookStatus};

use super::{dir_guard, HookInstallTarget};

/// `concat!` only accepts literals, so the marker is a macro shared by the constants below.
macro_rules! aoe_hook_marker {
    () => {
        "aoe-hooks"
    };
}

/// Fixed base inside the single-tenant sandbox; the host bind-mounts the
/// canonical `dir_guard::hook_base_path()/<id>` onto `<this>/<id>`.
pub(crate) const HOOK_STATUS_BASE_IN_CONTAINER: &str = concat!("/tmp/", aoe_hook_marker!());

const AOE_HOOK_MARKER: &str = aoe_hook_marker!();

/// Every emitter ends with `exit 0 # aoe-hooks`; the `0 ` binds the match to
/// that trailer rather than a `# aoe-hooks` inside user text.
const AOE_HOOK_TRAILING_SENTINEL: &str = concat!("0 # ", aoe_hook_marker!());

/// Legacy emitters without the trailer bake this path; a user script would
/// have expanded `$AOE_INSTANCE_ID`.
const AOE_HOOK_PATH_SENTINEL: &str = concat!(aoe_hook_marker!(), "/$AOE_INSTANCE_ID");

/// Whether `cmd` was emitted by AoE (current or legacy form).
pub(super) fn is_aoe_hook_command(cmd: &str) -> bool {
    let trimmed_tail = cmd.trim_end_matches(|c: char| c == '\'' || c == '"' || c.is_whitespace());
    trimmed_tail.ends_with(AOE_HOOK_TRAILING_SENTINEL) || cmd.contains(AOE_HOOK_PATH_SENTINEL)
}

/// Whether a JSON hook entry's `command` is AoE's.
pub(super) fn json_command_is_aoe(entry: &serde_json::Value) -> bool {
    entry
        .get("command")
        .and_then(serde_json::Value::as_str)
        .is_some_and(is_aoe_hook_command)
}

fn hook_base_for_target(target: HookInstallTarget) -> String {
    match target {
        HookInstallTarget::Host => dir_guard::hook_base_path().display().to_string(),
        HookInstallTarget::Sandbox => HOOK_STATUS_BASE_IN_CONTAINER.to_string(),
    }
}

/// Command writing `status` to the instance's status file. It must always
/// exit 0: a failing hook blocks the agent's tool calls.
pub(crate) fn hook_command(status: &str, target: HookInstallTarget) -> String {
    hook_command_with_base(status, &hook_base_for_target(target), target)
}

/// The tool-gated writer when `waiting_tools` can change the outcome, else the
/// plain writer (the gate only ever rewrites to `waiting`).
pub(crate) fn status_command_for_event(
    status: HookStatus,
    waiting_tools: &[String],
    target: HookInstallTarget,
) -> String {
    if waiting_tools.is_empty() || status == HookStatus::Waiting {
        hook_command(status.as_str(), target)
    } else {
        hook_command_waiting_tools_with_base(
            status.as_str(),
            waiting_tools,
            &hook_base_for_target(target),
            target,
        )
    }
}

/// Writes `waiting` when stdin names one of `waiting_tools`, else `default_status`.
/// Matches the compact `"tool_name":"X"` bytes, which an escaped mention inside
/// a JSON string value cannot produce.
fn hook_command_waiting_tools_with_base(
    default_status: &str,
    waiting_tools: &[String],
    base: &str,
    target: HookInstallTarget,
) -> String {
    let patterns: Vec<String> = waiting_tools
        .iter()
        .map(|tool| format!("*\\\"tool_name\\\":\\\"{tool}\\\"*"))
        .collect();
    let write = format!(
        "IN=$(cat 2>/dev/null); S={default_status}; \
         case \"$IN\" in {patterns}) S=waiting ;; esac; \
         printf %s \"$S\" > \"$D/status\" 2>/dev/null; ",
        patterns = patterns.join("|")
    );
    hook_command_with_write(&write, base, target)
}

fn hook_command_with_base(status: &str, base: &str, target: HookInstallTarget) -> String {
    hook_command_with_write(
        &format!("printf {status} > \"$D/status\" 2>/dev/null; "),
        base,
        target,
    )
}

/// Host commands check the base's mode and owner (the Rust `dir_guard` is the
/// authoritative gate); sandbox commands skip the uid check because the
/// container uid is unpredictable and the bind source was validated host-side.
fn hook_command_with_write(write: &str, base: &str, target: HookInstallTarget) -> String {
    let (parent_check, owner_recheck) = match target {
        // `mkdir -p $B` recovers from a /tmp reaper. It relies on /tmp's sticky
        // bit; re-audit if the base ever moves out of /tmp.
        HookInstallTarget::Host => (
            "\
             mkdir -p \"$B\" 2>/dev/null || exit 0; \
             LS=$(LC_ALL=C ls -ldn \"$B\" 2>/dev/null) || exit 0; \
             set -- $LS; M=\"$1\"; \
             case \"$M\" in drwx------|drwx------.|drwx------+|drwx------@) ;; *) exit 0 ;; esac; \
             ME=$(id -u 2>/dev/null) || exit 0; \
             [ \"$3\" = \"$ME\" ] || exit 0; ",
            "[ \"$3\" = \"$ME\" ] || exit 0; ",
        ),
        HookInstallTarget::Sandbox => ("", ""),
    };
    format!(
        "sh -c 'unset IFS; set -f; umask 077; \
         [ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
         case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
         B={base}; {parent_check}\
         D=\"$B/$AOE_INSTANCE_ID\"; \
         mkdir -p \"$D\" 2>/dev/null; \
         LS=$(LC_ALL=C ls -ldn \"$D\" 2>/dev/null) || exit 0; \
         set -- $LS; M=\"$1\"; \
         case \"$M\" in drwx------|drwx------.|drwx------+|drwx------@) ;; *) exit 0 ;; esac; \
         {owner_recheck}\
         {write}\
         exit 0 # {AOE_HOOK_MARKER}'"
    )
}

/// Command extracting the top-level session id from the hook's stdin JSON into
/// the `session_id` sidecar. The host calls the pinned `aoe` binary; the
/// sandbox image has no `aoe`, so it uses `jq` and silently skips without it.
pub(crate) fn hook_command_session_id(
    target: HookInstallTarget,
    field: HookIdentityField,
) -> String {
    match target {
        HookInstallTarget::Host => hook_command_session_id_host(field),
        HookInstallTarget::Sandbox => {
            hook_command_session_id_sandbox(HOOK_STATUS_BASE_IN_CONTAINER, field)
        }
    }
}

fn hook_command_session_id_host(field: HookIdentityField) -> String {
    let field = match field {
        HookIdentityField::SessionId => "session-id",
        HookIdentityField::ConversationIdOrSessionId => "conversation-id-or-session-id",
    };
    format!(
        "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
         [ -n \"$AOE_HOOK_BIN\" ] || exit 0; \
         [ -x \"$AOE_HOOK_BIN\" ] || exit 0; \
         \"$AOE_HOOK_BIN\" __extract-session-id --field {field} 2>/dev/null; exit 0 # {AOE_HOOK_MARKER}'"
    )
}

/// A second `AOE_AGENT_BIN` ancestor marks a nested agent, whose id must not
/// replace the pane's; with no launch pid in the container the walk runs to root.
fn hook_command_session_id_sandbox(base: &str, field: HookIdentityField) -> String {
    let selector = match field {
        HookIdentityField::SessionId => {
            r#"if (.session_id|type)=="string" then .session_id else empty end"#
        }
        HookIdentityField::ConversationIdOrSessionId => {
            r#"if (.conversation_id|type)=="string" then .conversation_id elif (.session_id|type)=="string" then .session_id else empty end"#
        }
    };
    format!(
        "sh -c 'unset IFS; set -f; umask 077; \
         [ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
         case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
         D=\"{base}/$AOE_INSTANCE_ID\"; mkdir -p \"$D\" 2>/dev/null; \
         LS=$(LC_ALL=C ls -ldn \"$D\" 2>/dev/null) || exit 0; \
         set -- $LS; M=\"$1\"; \
         case \"$M\" in drwx------|drwx------.|drwx------+|drwx------@) ;; *) exit 0 ;; esac; \
         B=\"${{AOE_AGENT_BIN:-}}\"; N=0; P=$PPID; \
         while [ -n \"$B\" ] && [ \"${{P:-0}}\" -gt 0 ]; do \
         A=$(tr \"\\0\" \"\\n\" < /proc/$P/cmdline 2>/dev/null | head -n 1); \
         [ \"${{A##*/}}\" = \"$B\" ] && N=$((N + 1)); \
         P=$(sed -n \"s/^PPid:[[:space:]]*//p\" /proc/$P/status 2>/dev/null); \
         done; \
         [ \"$N\" -le 1 ] || exit 0; \
         command -v jq >/dev/null 2>&1 || exit 0; \
         SID=$(jq -r '\\''{selector}'\\'' 2>/dev/null); \
         case \"$SID\" in \"\"|-*|*[!0-9a-zA-Z._-]*) exit 0 ;; esac; \
         [ \"${{#SID}}\" -le 256 ] || exit 0; \
         printf \"%s\" \"$SID\" > \"$D/.session_id.$$.tmp\" 2>/dev/null && mv \"$D/.session_id.$$.tmp\" \"$D/session_id\" 2>/dev/null; \
         exit 0 # {AOE_HOOK_MARKER}'"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use tempfile::TempDir;

    fn tight_dir(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path.to_path_buf()
    }

    fn host_status_command(base: &Path) -> String {
        hook_command_with_base("running", base.to_str().unwrap(), HookInstallTarget::Host)
    }

    /// Runs `script` under `shell` with `AOE_INSTANCE_ID=id`, feeding `stdin`.
    fn run_hook(
        shell: impl AsRef<std::ffi::OsStr>,
        script: &str,
        id: &str,
        stdin: &str,
        configure: impl FnOnce(&mut Command),
    ) -> Output {
        let mut command = Command::new(shell);
        command
            .args(["-c", script])
            .env("AOE_INSTANCE_ID", id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().expect("spawn shell");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "hook must exit 0: {output:?}");
        output
    }

    fn jq_available() -> bool {
        let present = Command::new("sh")
            .args(["-c", "command -v jq >/dev/null 2>&1"])
            .status()
            .is_ok_and(|s| s.success());
        if !present {
            eprintln!("skipping: jq not on PATH");
        }
        present
    }

    fn shell_available(shell: &str) -> bool {
        let present = Command::new(shell)
            .args(["-c", "exit 0"])
            .status()
            .is_ok_and(|s| s.success());
        if !present {
            eprintln!("skipping: {shell} not available");
        }
        present
    }

    #[test]
    fn commands_carry_their_guards() {
        let host = hook_command_with_base("running", "/tmp/aoe-hooks", HookInstallTarget::Host);
        let sandbox = hook_command("running", HookInstallTarget::Sandbox);
        let sid_host =
            hook_command_session_id(HookInstallTarget::Host, HookIdentityField::SessionId);
        let sid_sandbox =
            hook_command_session_id_sandbox("/tmp/aoe-hooks", HookIdentityField::SessionId);
        let common = [
            "unset IFS",
            "set -f",
            "umask 077",
            "case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac",
            "LC_ALL=C ls -ldn",
            "drwx------|drwx------.|drwx------+|drwx------@",
            "# aoe-hooks",
        ];
        let cases: [(&str, &str, Vec<&str>, Vec<&str>); 4] = [
            (
                "host status",
                &host,
                [
                    &common[..],
                    &[
                        "ME=$(id -u 2>/dev/null)",
                        "B=/tmp/aoe-hooks;",
                        "D=\"$B/$AOE_INSTANCE_ID\"",
                        "printf running > \"$D/status\"",
                    ],
                ]
                .concat(),
                vec![],
            ),
            (
                "sandbox status",
                &sandbox,
                [
                    &common[..],
                    &[
                        "B=/tmp/aoe-hooks;",
                        "D=\"$B/$AOE_INSTANCE_ID\"",
                        "printf running",
                    ],
                ]
                .concat(),
                vec!["ME=$(id -u", "[ \"$3\" = \"$ME\" ]", "/tmp/aoe-hooks-"],
            ),
            (
                "host session id",
                &sid_host,
                vec![
                    r#""$AOE_HOOK_BIN" __extract-session-id --field session-id"#,
                    r#"[ -n "$AOE_HOOK_BIN" ]"#,
                    r#"[ -x "$AOE_HOOK_BIN" ]"#,
                    "# aoe-hooks",
                ],
                vec!["command -v aoe", "grep -oE"],
            ),
            (
                "sandbox session id",
                &sid_sandbox,
                [
                    &common[..],
                    &[
                        "D=\"/tmp/aoe-hooks/$AOE_INSTANCE_ID\"",
                        "command -v jq >/dev/null 2>&1 || exit 0",
                        "jq -r ",
                        ".session_id|type",
                        // Refuses option-shaped ids like the host's `is_valid_session_id`.
                        "case \"$SID\" in \"\"|-*|*[!0-9a-zA-Z._-]*) exit 0 ;; esac",
                        "[ \"${#SID}\" -le 256 ] || exit 0",
                        ".session_id.$$.tmp",
                    ],
                ]
                .concat(),
                vec!["grep -oE", "__extract-session-id"],
            ),
        ];
        for (label, cmd, present, absent) in cases {
            for token in present {
                assert!(cmd.contains(token), "{label}: missing {token:?}: {cmd}");
            }
            for token in absent {
                assert!(!cmd.contains(token), "{label}: unexpected {token:?}: {cmd}");
            }
        }

        let host = hook_command_session_id(
            HookInstallTarget::Host,
            HookIdentityField::ConversationIdOrSessionId,
        );
        assert!(host.contains("--field conversation-id-or-session-id"));
        let sandbox = hook_command_session_id(
            HookInstallTarget::Sandbox,
            HookIdentityField::ConversationIdOrSessionId,
        );
        for token in [".conversation_id", ".session_id", "elif"] {
            assert!(sandbox.contains(token), "{sandbox}");
        }
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn host_status_command_bakes_per_user_base() {
        for euid in [1000u32, 65534, 0] {
            let _g = crate::hooks::test_support::BaseGuard::with_base(PathBuf::from(format!(
                "/tmp/aoe-hooks-{euid}"
            )));
            let cmd = hook_command("running", HookInstallTarget::Host);
            assert!(cmd.contains(&format!("B=/tmp/aoe-hooks-{euid};")), "{cmd}");
            assert!(cmd.contains("ME=$(id -u 2>/dev/null)"), "{cmd}");
        }
    }

    #[test]
    fn is_aoe_hook_command_distinguishes_ours_from_user_commands() {
        let ours = [
            hook_command("running", HookInstallTarget::Host),
            hook_command("idle", HookInstallTarget::Host),
            hook_command("waiting", HookInstallTarget::Sandbox),
            hook_command_session_id(HookInstallTarget::Host, HookIdentityField::SessionId),
            hook_command_session_id(HookInstallTarget::Sandbox, HookIdentityField::SessionId),
            // Legacy forms without the trailing marker match via the path sentinel.
            "sh -c 'unset IFS; set -f; umask 077; \
             [ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
             D=\"/tmp/aoe-hooks/$AOE_INSTANCE_ID\"; mkdir -p \"$D\" 2>/dev/null; \
             exit 0'"
                .to_string(),
            "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
             case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
             mkdir -p \"/tmp/aoe-hooks/$AOE_INSTANCE_ID\" 2>/dev/null; \
             printf running > \"/tmp/aoe-hooks/$AOE_INSTANCE_ID/status\" 2>/dev/null; \
             exit 0'"
                .to_string(),
        ];
        for cmd in &ours {
            assert!(is_aoe_hook_command(cmd), "must match: {cmd}");
        }
        for cmd in [
            "ls /tmp/aoe-hooks",
            "echo 'cleaning aoe-hooks dir'",
            "rm -rf /tmp/aoe-hooks-1000",
            "cat /var/log/aoe-hooks.log",
            "sh -c 'aoe-hooks stuff'",
            "aoe-hooks --foo",
            "echo aoe-hooks",
            "echo \" # aoe-hooks comment\"",
            "bash -c \"cd ~ && # aoe-hooks placeholder\nls\"",
            "# aoe-hooks: clean up tmp dir",
            "echo '# aoe-hooks'",
            "echo \"# aoe-hooks\"",
            "X='hidden # aoe-hooks'",
            "say 'task done: # aoe-hooks'",
        ] {
            assert!(!is_aoe_hook_command(cmd), "must not match: {cmd}");
        }
    }

    /// Hostile IFS, umask and a cwd full of glob bait must not stop the write
    /// or widen the instance dir.
    #[test]
    fn host_status_command_survives_hostile_environment() {
        let tmp = TempDir::new().unwrap();
        let base = tight_dir(&tmp.path().join("aoe-hooks"));
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        let decoys = ["glob-decoy-1", "drwxrwxrwx", "1000", "65534"];
        for name in decoys {
            std::fs::write(cwd.join(name), b"untouched").unwrap();
        }
        let cmd =
            hook_command_with_base("waiting", base.to_str().unwrap(), HookInstallTarget::Host);

        run_hook("sh", &format!("umask 022; {cmd}"), "hostile", "", |c| {
            c.env("IFS", "d").current_dir(&cwd);
        });

        let inst = base.join("hostile");
        assert_eq!(
            std::fs::read_to_string(inst.join("status")).unwrap(),
            "waiting"
        );
        let mode = std::fs::metadata(&inst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "got {mode:o}");
        for name in decoys {
            assert_eq!(
                std::fs::read_to_string(cwd.join(name)).unwrap(),
                "untouched"
            );
        }
    }

    #[test]
    fn host_status_command_exits_zero_when_it_cannot_write() {
        let tmp = TempDir::new().unwrap();

        // #1390: a base that cannot be created must not block the agent.
        let file_base = tmp.path().join("blocked");
        std::fs::write(&file_base, "not a dir").unwrap();
        run_hook(
            "sh",
            &host_status_command(&file_base),
            "blocked",
            "",
            |_| {},
        );

        let wide = tmp.path().join("wide");
        std::fs::create_dir(&wide).unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).unwrap();
        if shell_available("dash") {
            run_hook("dash", &host_status_command(&wide), "wide", "", |_| {});
            assert!(!wide.join("wide").exists(), "0o755 parent must be refused");
        }
    }

    #[test]
    fn host_status_command_rejects_traversal() {
        let tmp = TempDir::new().unwrap();
        let level1 = tmp.path().join("level1");
        let base = level1.join("base");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join(".canary"), b"keep").unwrap();
        let cmd = host_status_command(&base);

        for poisoned in ["..", "../../escape", "/etc", "foo/bar", "; rm -rf /;", ""] {
            run_hook("sh", &cmd, poisoned, "", |_| {});
        }

        for (dir, only) in [
            (&base, ".canary"),
            (&level1, "base"),
            (&tmp.path().to_path_buf(), "level1"),
        ] {
            let entries: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(
                entries,
                [std::ffi::OsString::from(only)],
                "{}",
                dir.display()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn privdrop_host_status_command_refuses_alien_uid() {
        use crate::hooks::test_support::{make_alien_owned, privdrop_test_enabled};
        if !privdrop_test_enabled() {
            return;
        }
        assert!(
            shell_available("dash"),
            "dedicated Linux privdrop job requires dash"
        );
        let tmp = TempDir::new().unwrap();
        let base = tight_dir(&tmp.path().join("aoe-hooks-alien"));
        make_alien_owned(&base);
        run_hook("dash", &host_status_command(&base), "alien", "", |_| {});
        assert!(!base.join("alien").exists());
    }

    /// `ls -l` appends `+` for an ACL; a tight one is accepted, a widening one
    /// changes the mode glyphs and is refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn host_status_command_acl_handling() {
        let euid = nix::unistd::geteuid().as_raw();
        let probe_uid = [65534u32, 65533, 1, 2]
            .into_iter()
            .find(|u| *u != euid)
            .unwrap();
        let setfacl = |path: &Path, spec: &str| match Command::new("setfacl")
            .args(["-m", spec])
            .arg(path)
            .output()
        {
            Ok(o) if o.status.success() => true,
            other => {
                eprintln!("skipping: setfacl failed: {other:?}");
                false
            }
        };
        let accepted = |mode: &str| {
            matches!(
                mode,
                "drwx------" | "drwx------." | "drwx------+" | "drwx------@"
            )
        };
        let ls_mode = |path: &Path| {
            let out = Command::new("ls")
                .arg("-ldn")
                .arg(path)
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            String::from_utf8(out.stdout)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        };

        let tight = format!("u::rwx,g::---,o::---,u:{probe_uid}:---,m::---");
        let wide = format!("u::rwx,g::---,o::---,u:{probe_uid}:r-x,m::r-x");
        for (id, spec, on_base, writes) in [
            ("acl_tight", &tight, true, true),
            ("acl_wide", &wide, false, false),
        ] {
            let tmp = TempDir::new().unwrap();
            let base = tight_dir(&tmp.path().join("base"));
            let inst = tight_dir(&base.join(id));
            if (on_base && !setfacl(&base, spec)) || !setfacl(&inst, spec) {
                return;
            }
            assert!(accepted(&ls_mode(&base)), "precondition: base accepted");
            let inst_mode = ls_mode(&inst);
            assert!(
                inst_mode.ends_with('+') && accepted(&inst_mode) == writes,
                "precondition: {inst_mode}"
            );

            run_hook("sh", &host_status_command(&base), id, "", |_| {});
            assert_eq!(inst.join("status").exists(), writes, "{id}");
        }
    }

    #[test]
    fn waiting_tools_command_gates_on_tool_name() {
        let cases = [
            (
                r#"{"session_id":"6cbahc1c","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_input":{"questions":[]}}"#,
                "waiting",
            ),
            (
                r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
                "running",
            ),
            // An escaped mention inside a string value has different bytes.
            (
                r#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"new_string":"match on \"tool_name\":\"AskUserQuestion\" here"}}"#,
                "running",
            ),
            ("", "running"),
        ];
        for (payload, want) in cases {
            let tmp = TempDir::new().unwrap();
            let base = tight_dir(&tmp.path().join("aoe-hooks"));
            let cmd = hook_command_waiting_tools_with_base(
                "running",
                &["AskUserQuestion".to_string()],
                base.to_str().unwrap(),
                HookInstallTarget::Host,
            );
            run_hook("sh", &cmd, "tools", payload, |_| {});
            assert_eq!(
                std::fs::read_to_string(base.join("tools/status")).unwrap(),
                want,
                "{payload}"
            );
        }
    }

    #[test]
    fn sandbox_session_id_command_extracts_top_level_id() {
        if !jq_available() {
            return;
        }
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let decoy = "11111111-2222-3333-4444-555555555555";
        let cases = [
            (format!(r#"{{"session_id":"{uuid}","cwd":"/x"}}"#), Some(uuid)),
            (format!("{{\n  \"session_id\":\"{uuid}\",\n  \"cwd\":\"/x\"\n}}"), Some(uuid)),
            (r#"{"session_id":"AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"}"#.to_string(), Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE")),
            (format!(r#"{{"session_id":"{uuid}","prompt":"\"session_id\":\"{decoy}\""}}"#), Some(uuid)),
            // #1760: a textually earlier nested id must not win.
            (
                format!(r#"{{"context":{{"session_id":"{decoy}"}},"session_id":"conversation_opaque.123"}}"#),
                Some("conversation_opaque.123"),
            ),
            (
                format!(
                    "{{\n  \"hook_event_name\": \"PreToolUse\",\n  \"tool_input\": {{\n    \"session_id\": \"{decoy}\"\n  }},\n  \"prompt\": \"please do \\\"session_id\\\":\\\"{decoy}\\\" thing\",\n  \"session_id\": \"{uuid}\"\n}}"
                ),
                Some(uuid),
            ),
            (r#"{"cwd":"/x","other":"value"}"#.to_string(), None),
        ];
        for (payload, want) in cases {
            let tmp = TempDir::new().unwrap();
            let cmd = hook_command_session_id_sandbox(
                tmp.path().to_str().unwrap(),
                HookIdentityField::SessionId,
            );
            run_hook("sh", &cmd, "sid", &payload, |_| {});
            let got = std::fs::read_to_string(tmp.path().join("sid/session_id")).ok();
            assert_eq!(got.as_deref(), want, "{payload}");
        }
    }

    #[test]
    fn sandbox_session_id_command_skips_nested_agent() {
        if !jq_available() || !Path::new("/proc/self/cmdline").exists() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let agent = tmp.path().join("aoe-fake-agent");
        std::os::unix::fs::symlink("/bin/sh", &agent).unwrap();
        let hook = hook_command_session_id_sandbox(
            tmp.path().to_str().unwrap(),
            HookIdentityField::SessionId,
        );
        let payload = r#"{"session_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"}"#;
        for (id, script, writes) in [
            ("pane_agent", r#"eval "$HOOK"; true"#, true),
            (
                "nested_agent",
                r#""$AGENT" -c 'eval "$HOOK"; true'; true"#,
                false,
            ),
        ] {
            run_hook(&agent, script, id, payload, |c| {
                c.env("AGENT", &agent)
                    .env("HOOK", &hook)
                    .env("AOE_AGENT_BIN", "aoe-fake-agent");
            });
            assert_eq!(
                tmp.path().join(id).join("session_id").exists(),
                writes,
                "{id}"
            );
        }
    }
}
