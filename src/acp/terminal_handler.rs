//! Handlers for ACP `terminal/*` requests.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use thiserror::Error;
use tokio::process::Command;
use tracing::info;

use crate::acp::establishment::NativeRequestContext;
use crate::containers::container_interface::{docker_env_args, EnvEntry};

/// Routing target for a terminal command.
#[derive(Debug, Clone)]
pub struct TerminalSandbox {
    pub container_name: String,
    /// Resolved env entries to forward into the container for this command.
    pub env_entries: Vec<EnvEntry>,
}

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("terminal {0} does not exist")]
    UnknownTerminal(String),
    #[error("terminal authority refused: {0}")]
    Authority(String),
}

/// Identifier returned to the agent on `terminal/create`.
pub type TerminalId = Arc<str>;

/// One terminal's captured output and exit status.
#[derive(Debug, Clone)]
pub struct TerminalOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

/// Per-session terminal manager.
#[derive(Debug, Clone, Default)]
pub struct TerminalManager {
    inner: Arc<Mutex<TerminalManagerInner>>,
}

#[derive(Debug, Default)]
struct TerminalManagerInner {
    outputs: std::collections::HashMap<TerminalId, Arc<TerminalEntry>>,
}

#[derive(Debug)]
struct TerminalEntry {
    id: TerminalId,
    output: TerminalOutput,
    source: Option<Arc<NativeRequestContext>>,
}

/// Pins the actual allocation, not a terminal ID to look up after waiting.
#[derive(Debug, Clone)]
pub(crate) struct CapturedTerminal {
    entry: Arc<TerminalEntry>,
}

impl CapturedTerminal {
    pub(crate) fn with_output<T>(
        &self,
        read: impl FnOnce(&TerminalOutput) -> T,
    ) -> Result<T, TerminalError> {
        let _source = self
            .entry
            .source
            .as_deref()
            .map(NativeRequestContext::try_owned_resource_guard)
            .transpose()
            .map_err(|error| TerminalError::Authority(error.to_string()))?;
        Ok(read(&self.entry.output))
    }
}

/// Build the `docker exec` argv and inherit-env pairs for a sandboxed
/// `terminal/create` request.
pub(crate) fn build_sandbox_exec_args(
    sandbox: &TerminalSandbox,
    cwd: &std::path::Path,
    command: &str,
    args: &[String],
) -> (Vec<String>, Vec<(String, String)>) {
    let (env_argv, inherit_pairs) = docker_env_args(&sandbox.env_entries);
    let mut full_args: Vec<String> = vec![
        "exec".into(),
        "-w".into(),
        cwd.to_string_lossy().into_owned(),
    ];
    full_args.extend(env_argv);
    full_args.push(sandbox.container_name.clone());
    full_args.push(command.to_string());
    full_args.extend(args.iter().cloned());
    (full_args, inherit_pairs)
}

impl TerminalManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn a one-shot terminal: run a command, wait for exit, capture
    /// stdout/stderr.
    pub(crate) async fn create_and_run(
        &self,
        session_id: &str,
        command: (&str, Vec<String>),
        cwd: PathBuf,
        sandbox: Option<&TerminalSandbox>,
        source: Option<Arc<NativeRequestContext>>,
        spawn: impl FnOnce(&mut Command) -> Result<tokio::process::Child, TerminalError>,
    ) -> Result<TerminalId, TerminalError> {
        let (command, args) = command;
        let id: TerminalId = format!("term-{}", uuid::Uuid::new_v4().simple()).into();
        info!(
            target: "acp.terminal",
            session = %session_id,
            terminal = %id,
            command = %command,
            cwd = %cwd.display(),
            sandboxed = sandbox.is_some(),
            "terminal/create"
        );

        let child = {
            let _current = source
                .as_deref()
                .map(NativeRequestContext::try_original_io_guard)
                .transpose()
                .map_err(|error| TerminalError::Authority(error.to_string()))?;
            let mut command = match sandbox {
                Some(s) => {
                    let runtime = crate::containers::get_container_runtime();
                    let (full_args, inherit_pairs) =
                        build_sandbox_exec_args(s, &cwd, command, &args);
                    let mut command = Command::new(runtime.base.binary);
                    command.args(full_args);
                    for (key, value) in inherit_pairs {
                        command.env(key, value);
                    }
                    command
                }
                None => {
                    let mut command = Command::new(command);
                    command.args(args).current_dir(cwd);
                    command
                }
            };
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            // Original I/O and connection admission cover spawn, never the output await.
            spawn(&mut command)?
        };

        // Drain stdout, stderr, and wait() concurrently; sequential reads deadlock on a full pipe.
        let raw = child.wait_with_output().await?;
        let output = TerminalOutput {
            stdout: String::from_utf8_lossy(&raw.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&raw.stderr).into_owned(),
            exit_code: raw.status.code(),
        };

        let entry = Arc::new(TerminalEntry {
            id: Arc::clone(&id),
            output,
            source,
        });
        self.inner
            .lock()
            .expect("terminal manager lock")
            .outputs
            .insert(Arc::clone(&id), entry);
        Ok(id)
    }

    /// Capture the original entry before a callback can suspend.
    pub(crate) fn capture(
        &self,
        terminal_id: &str,
        caller: Option<&NativeRequestContext>,
    ) -> Result<CapturedTerminal, TerminalError> {
        let entry = self
            .inner
            .lock()
            .expect("terminal manager lock")
            .outputs
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| TerminalError::UnknownTerminal(terminal_id.into()))?;
        let owned = match (entry.source.as_deref(), caller) {
            (Some(source), Some(caller)) => source.same_callback_source(caller),
            (None, None) => true,
            _ => false,
        };
        if !owned {
            return Err(TerminalError::Authority(
                "terminal original source changed".into(),
            ));
        }
        Ok(CapturedTerminal { entry })
    }

    /// Remove only the captured allocation. Repeated cleanup stays idempotent.
    pub(crate) fn release(&self, terminal: &CapturedTerminal) -> Result<(), TerminalError> {
        let _source = terminal
            .entry
            .source
            .as_deref()
            .map(NativeRequestContext::try_owned_resource_guard)
            .transpose()
            .map_err(|error| TerminalError::Authority(error.to_string()))?;
        let mut inner = self.inner.lock().expect("terminal manager lock");
        if inner
            .outputs
            .get(terminal.entry.id.as_ref())
            .is_some_and(|entry| Arc::ptr_eq(entry, &terminal.entry))
        {
            inner.outputs.remove(terminal.entry.id.as_ref());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_captures_output_and_release_is_idempotent() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        let mgr = TerminalManager::new();
        let cwd = std::env::temp_dir();
        let id = mgr
            .create_and_run(
                "s-1",
                ("echo", vec!["hello".into()]),
                cwd,
                None,
                None,
                |command| Ok(command.spawn()?),
            )
            .await
            .unwrap();
        let terminal = mgr.capture(&id, None).unwrap();
        terminal
            .with_output(|out| {
                assert!(out.stdout.contains("hello"));
                assert_eq!(out.exit_code, Some(0));
            })
            .unwrap();

        mgr.release(&terminal).unwrap();
        mgr.release(&terminal).unwrap();
        assert!(matches!(
            mgr.capture("never-existed", None),
            Err(TerminalError::UnknownTerminal(_))
        ));
        assert!(matches!(
            mgr.capture(&id, None),
            Err(TerminalError::UnknownTerminal(_))
        ));
    }

    /// Both pipes must be drained concurrently or a writer fills its buffer
    /// and blocks forever.
    #[tokio::test]
    async fn large_pipes_drain_without_deadlocking() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        // (session, shell script, trimmed stdout len, stderr len)
        let cases: [(&str, &str, usize, usize); 2] = [
            (
                "s-large-stderr",
                "head -c 204800 /dev/zero | tr '\\0' 'x' >&2; echo done",
                4,
                204_800,
            ),
            (
                "s-both-pipes",
                "head -c 204800 /dev/zero | tr '\\0' 'o' \
                 & head -c 204800 /dev/zero | tr '\\0' 'e' >&2 \
                 & wait",
                204_800,
                204_800,
            ),
        ];
        for (session, script, stdout_len, stderr_len) in cases {
            let mgr = TerminalManager::new();
            let run = mgr.create_and_run(
                session,
                ("sh", vec!["-c".into(), script.into()]),
                std::env::temp_dir(),
                None,
                None,
                |command| Ok(command.spawn()?),
            );
            let id = tokio::time::timeout(std::time::Duration::from_secs(5), run)
                .await
                .unwrap_or_else(|_| {
                    panic!("{session}: create_and_run hung; pipe deadlock regressed")
                })
                .expect("create_and_run failed");
            mgr.capture(&id, None)
                .unwrap()
                .with_output(|out| {
                    assert_eq!(out.stdout.trim_end().len(), stdout_len, "{session}");
                    assert_eq!(out.stderr.len(), stderr_len, "{session}");
                    assert_eq!(out.exit_code, Some(0), "{session}");
                })
                .unwrap();
        }
    }

    #[test]
    fn sandbox_exec_args_place_env_flags_before_the_container() {
        let entries = vec![
            EnvEntry::Inherit {
                key: "GH_TOKEN".into(),
                value: "ghp_secret".into(),
            },
            EnvEntry::Literal {
                key: "TERM".into(),
                value: "xterm".into(),
            },
        ];
        // (env entries, command, args, expected argv tail after `exec -w /workspace`)
        type Case<'a> = (Vec<EnvEntry>, &'a str, Vec<String>, Vec<&'a str>);
        let cases: [Case; 2] = [
            (
                entries,
                "gh",
                vec!["pr".into(), "list".into()],
                vec![
                    "-e",
                    "GH_TOKEN",
                    "-e",
                    "TERM=xterm",
                    "aoe-sandbox-test",
                    "gh",
                    "pr",
                    "list",
                ],
            ),
            (
                vec![],
                "echo",
                vec!["hi".into()],
                vec!["aoe-sandbox-test", "echo", "hi"],
            ),
        ];
        for (env_entries, command, args, tail) in cases {
            let inherits = env_entries
                .iter()
                .filter_map(|e| match e {
                    EnvEntry::Inherit { key, value } => Some((key.clone(), value.clone())),
                    EnvEntry::Literal { .. } => None,
                })
                .collect::<Vec<_>>();
            let sandbox = TerminalSandbox {
                container_name: "aoe-sandbox-test".into(),
                env_entries,
            };
            let (argv, inherit) = build_sandbox_exec_args(
                &sandbox,
                std::path::Path::new("/workspace"),
                command,
                &args,
            );
            let want: Vec<String> = ["exec", "-w", "/workspace"]
                .into_iter()
                .chain(tail)
                .map(str::to_string)
                .collect();
            assert_eq!(argv, want, "{command}");
            // An inherited value reaches `cmd.env`, never argv.
            assert_eq!(inherit, inherits, "{command}");
            assert!(!argv.iter().any(|a| a.contains("ghp_secret")), "{command}");
        }
    }
}
