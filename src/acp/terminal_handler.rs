//! Handlers for ACP `terminal/*` requests.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use thiserror::Error;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::info;

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
}

/// Identifier returned to the agent on `terminal/create`.
pub type TerminalId = String;

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
    outputs: std::collections::HashMap<TerminalId, TerminalOutput>,
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
    pub async fn create_and_run(
        &self,
        session_id: &str,
        command: &str,
        args: Vec<String>,
        cwd: PathBuf,
        sandbox: Option<&TerminalSandbox>,
    ) -> Result<TerminalId, TerminalError> {
        let id = format!("term-{}", uuid::Uuid::new_v4().simple());
        info!(
            target: "acp.terminal",
            session = %session_id,
            terminal = %id,
            command = %command,
            cwd = %cwd.display(),
            sandboxed = sandbox.is_some(),
            "terminal/create"
        );

        let child = match sandbox {
            Some(s) => {
                let runtime = crate::containers::get_container_runtime();
                let binary = runtime.base.binary;
                let (full_args, inherit_pairs) = build_sandbox_exec_args(s, &cwd, command, &args);
                let mut cmd = Command::new(binary);
                cmd.args(&full_args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                for (k, v) in inherit_pairs {
                    cmd.env(k, v);
                }
                cmd.spawn()?
            }
            None => Command::new(command)
                .args(&args)
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // Kill the child if the prompt task is dropped on a
                // force-stop / cancel-escalation worker teardown.
                .kill_on_drop(true)
                .spawn()?,
        };

        // Drain stdout, stderr, and wait() concurrently; sequential reads deadlock on a full pipe.
        let raw = child.wait_with_output().await?;
        let output = TerminalOutput {
            stdout: String::from_utf8_lossy(&raw.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&raw.stderr).into_owned(),
            exit_code: raw.status.code(),
        };

        self.inner.lock().await.outputs.insert(id.clone(), output);
        Ok(id)
    }

    /// Returns the captured output of a terminal.
    pub async fn output(&self, terminal_id: &str) -> Result<TerminalOutput, TerminalError> {
        let inner = self.inner.lock().await;
        inner
            .outputs
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| TerminalError::UnknownTerminal(terminal_id.into()))
    }

    /// Drop captured output.
    pub async fn release(&self, terminal_id: &str) {
        self.inner.lock().await.outputs.remove(terminal_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_runs_and_captures_output() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        let mgr = TerminalManager::new();
        let cwd = std::env::temp_dir();
        let id = mgr
            .create_and_run("s-1", "echo", vec!["hello".into()], cwd, None)
            .await
            .unwrap();
        let out = mgr.output(&id).await.unwrap();
        assert!(out.stdout.contains("hello"));
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn release_is_idempotent_cleanup() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        let mgr = TerminalManager::new();
        let id = mgr
            .create_and_run("s-1", "true", vec![], std::env::temp_dir(), None)
            .await
            .unwrap();
        mgr.release(&id).await;
        mgr.release(&id).await;
        mgr.release("never-existed").await;
        assert!(matches!(
            mgr.output(&id).await,
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
                "sh",
                vec!["-c".into(), script.into()],
                std::env::temp_dir(),
                None,
            );
            let id = tokio::time::timeout(std::time::Duration::from_secs(5), run)
                .await
                .unwrap_or_else(|_| {
                    panic!("{session}: create_and_run hung; pipe deadlock regressed")
                })
                .expect("create_and_run failed");
            let out = mgr.output(&id).await.unwrap();
            assert_eq!(out.stdout.trim_end().len(), stdout_len, "{session}");
            assert_eq!(out.stderr.len(), stderr_len, "{session}");
            assert_eq!(out.exit_code, Some(0), "{session}");
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
