//! Handlers for ACP `terminal/*` requests.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;
use tokio::time::{sleep_until, Instant};
use tracing::info;

use crate::acp::establishment::NativeRequestContext;
use crate::containers::container_interface::{
    docker_env_args, EnvEntry, RuntimeEnvArgs, TargetEnvFile,
};
use crate::process::worker_registry::RegistryBusy;

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
    #[error("terminal cwd outside session roots: {0}")]
    CwdOutsideRoots(crate::acp::fs_handler::FsError),
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

/// Retry only lock contention before an effect. The callback owns cancellation,
/// and no lifecycle or registry guard survives a sleep or child wait.
async fn with_terminal_authority<T, G>(
    try_guard: impl Fn() -> anyhow::Result<G>,
    effect: impl FnOnce() -> T,
) -> Result<T, TerminalError> {
    let mut deadline = None;
    loop {
        let error = match try_guard() {
            Ok(_guard) => return Ok(effect()),
            Err(error) => error,
        };
        if !error.is::<RegistryBusy>() {
            return Err(TerminalError::Authority(error.to_string()));
        }
        let now = Instant::now();
        let until = *deadline.get_or_insert_with(|| now + Duration::from_secs(3));
        sleep_until((now + Duration::from_millis(10)).min(until)).await;
        if Instant::now() >= until {
            return Err(TerminalError::Authority(error.to_string()));
        }
    }
}

/// Pins the actual allocation, not a terminal ID to look up after waiting.
#[derive(Debug, Clone)]
pub(crate) struct CapturedTerminal {
    entry: Arc<TerminalEntry>,
}

impl CapturedTerminal {
    async fn with_authority<T>(&self, effect: impl FnOnce() -> T) -> Result<T, TerminalError> {
        with_terminal_authority(
            || {
                self.entry
                    .source
                    .as_deref()
                    .map(NativeRequestContext::try_owned_resource_guard)
                    .transpose()
            },
            effect,
        )
        .await
    }

    pub(crate) async fn with_output<T>(
        &self,
        read: impl FnOnce(&TerminalOutput) -> T,
    ) -> Result<T, TerminalError> {
        self.with_authority(|| read(&self.entry.output)).await
    }
}

/// Build the `docker exec` argv and environment channels for a sandboxed
/// `terminal/create` request.
pub(crate) fn build_sandbox_exec_args(
    sandbox: &TerminalSandbox,
    cwd: &std::path::Path,
    command: &str,
    args: &[String],
) -> RuntimeEnvArgs {
    let env = docker_env_args(&sandbox.env_entries);
    let mut full_args: Vec<String> = vec![
        "exec".into(),
        "-w".into(),
        cwd.to_string_lossy().into_owned(),
    ];
    full_args.extend(env.argv);
    full_args.push(sandbox.container_name.clone());
    full_args.push(command.to_string());
    full_args.extend(args.iter().cloned());
    RuntimeEnvArgs {
        argv: full_args,
        ..env
    }
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
        source: Option<Arc<NativeRequestContext>>,
        prepare_sandbox: impl FnOnce(&Path) -> Result<Option<TerminalSandbox>, TerminalError>,
        spawn: impl FnOnce(&mut Command) -> Result<tokio::process::Child, TerminalError>,
    ) -> Result<TerminalId, TerminalError> {
        let (command, args) = command;
        let id: TerminalId = format!("term-{}", uuid::Uuid::new_v4().simple()).into();
        let child = with_terminal_authority(
            || {
                source
                    .as_deref()
                    .map(NativeRequestContext::try_original_io_guard)
                    .transpose()
            },
            || {
                let sandbox = prepare_sandbox(&cwd)?;
                info!(
                    target: "acp.terminal",
                    session = %session_id,
                    terminal = %id,
                    command = %command,
                    cwd = %cwd.display(),
                    sandboxed = sandbox.is_some(),
                    "terminal/create"
                );
                let mut command = match sandbox.as_ref() {
                    Some(s) => {
                        let runtime = crate::containers::get_container_runtime();
                        let env = build_sandbox_exec_args(s, &cwd, command, &args);
                        let mut command = Command::new(runtime.base.binary);
                        command.args(&env.argv);
                        for (key, value) in &env.inherit {
                            command.env(key, value);
                        }
                        if let Some(file) = TargetEnvFile::create(&env.target_env)? {
                            file.attach(command.as_std_mut())?;
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
                spawn(&mut command)
            },
        )
        .await??;

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
    pub(crate) async fn release(&self, terminal: &CapturedTerminal) -> Result<(), TerminalError> {
        terminal
            .with_authority(|| {
                let mut inner = self.inner.lock().expect("terminal manager lock");
                if inner
                    .outputs
                    .get(terminal.entry.id.as_ref())
                    .is_some_and(|entry| Arc::ptr_eq(entry, &terminal.entry))
                {
                    inner.outputs.remove(terminal.entry.id.as_ref());
                }
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn terminal_authority_waits_only_before_the_effect() {
        use std::cell::Cell;

        let lock = tokio::sync::Mutex::new(());
        let held = lock.try_lock().unwrap();
        let attempts = Cell::new(0);
        let effects = Cell::new(0);
        let (_, result) = tokio::join!(
            async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                assert_eq!(attempts.get(), 1);
                assert_eq!(effects.get(), 0);
                drop(held);
            },
            with_terminal_authority(
                || {
                    attempts.set(attempts.get() + 1);
                    lock.try_lock().map_err(|_| RegistryBusy.into())
                },
                || {
                    effects.set(effects.get() + 1);
                    Err::<(), _>(TerminalError::Io(std::io::Error::other("spawn failed")))
                },
            ),
        );
        assert!(matches!(result, Ok(Err(TerminalError::Io(_)))));
        assert_eq!(attempts.get(), 2);
        assert_eq!(effects.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_authority_times_out_but_does_not_retry_refusal() {
        use std::cell::Cell;

        let effects = Cell::new(0);
        let start = Instant::now();
        let result = with_terminal_authority(
            || Err::<(), _>(RegistryBusy.into()),
            || effects.set(effects.get() + 1),
        )
        .await;
        assert!(matches!(result, Err(TerminalError::Authority(_))));
        let elapsed = start.elapsed();
        assert!((Duration::from_secs(3)..Duration::from_millis(3010)).contains(&elapsed));

        let attempts = Cell::new(0);
        let result = with_terminal_authority(
            || {
                attempts.set(attempts.get() + 1);
                Err::<(), _>(crate::acp::establishment::EstablishmentRefusal::Superseded.into())
            },
            || effects.set(effects.get() + 1),
        )
        .await;
        assert!(matches!(result, Err(TerminalError::Authority(_))));
        assert_eq!(attempts.get(), 1);
        assert_eq!(effects.get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_busy_terminal_authority_does_not_run_the_effect() {
        use std::cell::Cell;

        let busy = Cell::new(true);
        let effects = Cell::new(0);
        let attempt = with_terminal_authority(
            || {
                if busy.get() {
                    Err(RegistryBusy.into())
                } else {
                    Ok(())
                }
            },
            || effects.set(effects.get() + 1),
        );
        let result = tokio::time::timeout(Duration::from_millis(5), attempt).await;
        assert!(result.is_err());
        busy.set(false);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(effects.get(), 0);
    }

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
                |_| Ok(None),
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
            .await
            .unwrap();

        mgr.release(&terminal).await.unwrap();
        mgr.release(&terminal).await.unwrap();
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
                |_| Ok(None),
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
                .await
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
                    "--env-file",
                    "/dev/fd/9",
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
            let (inherits, targets): (Vec<_>, Vec<_>) = env_entries
                .iter()
                .partition(|e| matches!(e, EnvEntry::Inherit { .. }));
            let pairs = |entries: Vec<&EnvEntry>| {
                entries
                    .into_iter()
                    .map(|e| (e.key().to_string(), e.value().to_string()))
                    .collect::<Vec<_>>()
            };
            let (inherits, targets) = (pairs(inherits), pairs(targets));
            let sandbox = TerminalSandbox {
                container_name: "aoe-sandbox-test".into(),
                env_entries,
            };
            let RuntimeEnvArgs {
                argv,
                inherit,
                target_env,
            } = build_sandbox_exec_args(
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
            // An inherited value reaches `cmd.env`, a literal only the env-file.
            assert_eq!(inherit, inherits, "{command}");
            assert_eq!(target_env, targets, "{command}");
            assert!(!argv.iter().any(|a| a.contains("ghp_secret")), "{command}");
        }
    }
}
