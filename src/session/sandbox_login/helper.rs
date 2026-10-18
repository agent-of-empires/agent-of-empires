//! Runs an agent's login command in a short-lived helper container over plain pipes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::code::LoginCode;
use super::AgentLogin;
use crate::containers::RuntimeKind;
use crate::session::config::container_config::{
    ensure_shared_credential_file, login_target_with_command, read_shared_credential,
    CONTAINER_HOME,
};
use crate::session::config::SharedCredentialMode;

const LOG_TARGET: &str = "session.sandbox_login";
const HELPER_LABEL_KEY: &str = "com.agent-of-empires.sandbox-login-helper";
const HELPER_NAME_PREFIX: &str = "aoe-sandbox-login-";
const OUTPUT_CAP: usize = 256 * 1024;
const KILL_WAIT: Duration = Duration::from_secs(10);
const DRAIN_WAIT: Duration = Duration::from_secs(2);
const CLEANUP_WAIT: Duration = Duration::from_secs(30);
const SUCCESS_WITHOUT_CREDENTIAL: &str =
    "the client reported success but the shared credential holds none";
const SUCCESS_WITHOUT_NEW_CREDENTIAL: &str =
    "the client reported success but the shared credential did not change";

#[derive(Debug, Clone, Copy)]
pub(crate) struct HelperTimeouts {
    pub url: Duration,
    pub after_code: Duration,
    pub total: Duration,
}

impl Default for HelperTimeouts {
    fn default() -> Self {
        Self {
            url: Duration::from_secs(90),
            after_code: Duration::from_secs(120),
            total: Duration::from_secs(600),
        }
    }
}

/// Host files the helper mounts, and where it sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelperMounts {
    pub credential: PathBuf,
    pub credential_in_container: String,
    pub home_seed: PathBuf,
    pub home_seed_in_container: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LoginError {
    #[error("a sandbox login is already running")]
    Busy,
    #[error("sandbox login needs sandbox.shared_credential = \"separate_login\"")]
    NotSeparateLogin,
    #[error("the sandbox credential is already usable; pass --force to log in again")]
    AlreadyLoggedIn,
    #[error(transparent)]
    Unsupported(#[from] crate::session::SandboxLoginUnsupported),
    #[error("the login client did not print a sign-in link:\n{output}")]
    NoUrl { output: String },
    #[error("the sandbox login timed out")]
    Timeout,
    #[error("the sandbox login was cancelled")]
    Cancelled,
    #[error("the sandbox login failed:\n{message}")]
    Failed { message: String },
    #[error("this login is no longer running")]
    Gone,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// `<key>=<16 hex>` from the shared file's path: helpers of different shared files
/// (separate login locks) never sweep each other.
fn helper_label(shared: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(shared.as_os_str().as_encoded_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{HELPER_LABEL_KEY}={hex}")
}

/// Whether the helper container can run on `kind`. A runtime without it cannot log the
/// sandboxes in, so `separate_login` is refused there instead of holding launches for a
/// login nothing could perform.
pub(crate) fn supports_runtime(kind: RuntimeKind) -> bool {
    kind != RuntimeKind::AppleContainer
}

/// `<binary> run --rm -i ... <image> <login command>`: only the credential and the
/// helper-private seed are mounted, and only `HOME` is set.
pub(crate) fn helper_argv(
    kind: RuntimeKind,
    binary: &str,
    image: &str,
    name: &str,
    mounts: &HelperMounts,
    login: &AgentLogin,
    relabel: bool,
) -> Result<Vec<String>, LoginError> {
    if !supports_runtime(kind) {
        return Err(crate::session::SandboxLoginUnsupported.into());
    }
    if image.starts_with('-') {
        return Err(anyhow::anyhow!(
            "invalid sandbox image {image:?}: an image name cannot start with '-'"
        )
        .into());
    }
    for path in [&mounts.credential, &mounts.home_seed] {
        if path.as_os_str().as_encoded_bytes().contains(&b':') {
            return Err(anyhow::anyhow!(
                "cannot mount {} into the login helper: the path contains ':'",
                path.display()
            )
            .into());
        }
    }
    let z = if relabel { ":z" } else { "" };
    let label = helper_label(&mounts.credential);
    let mut argv: Vec<String> = [
        binary,
        "run",
        "--rm",
        "-i",
        "--name",
        name,
        "--label",
        &label,
        "-v",
        &format!(
            "{}:{}{z}",
            mounts.credential.display(),
            mounts.credential_in_container
        ),
        "-v",
        &format!(
            "{}:{}{z}",
            mounts.home_seed.display(),
            mounts.home_seed_in_container
        ),
        "-e",
        &format!("HOME={CONTAINER_HOME}"),
        image,
    ]
    .map(str::to_string)
    .to_vec();
    argv.extend(login.argv.iter().map(|arg| (*arg).to_string()));
    Ok(argv)
}

/// `text` keeps the newest bytes; `dropped` counts bytes cut from the front so marks stay valid.
#[derive(Default)]
struct Output {
    text: String,
    dropped: usize,
}

impl Output {
    fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        if self.text.len() > OUTPUT_CAP {
            let mut cut = self.text.len() - OUTPUT_CAP;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut;
        }
    }

    fn mark(&self) -> usize {
        self.dropped + self.text.len()
    }

    fn since(&self, mark: usize) -> &str {
        self.text
            .get(mark.saturating_sub(self.dropped)..)
            .unwrap_or(&self.text)
    }
}

/// Non-blocking lock beside the shared file. Not `.credentials.lock`: come-ups take that one blocking.
struct LoginLock {
    _file: std::fs::File,
}

impl LoginLock {
    fn try_acquire(shared: &Path) -> Result<Self, LoginError> {
        use std::os::unix::fs::OpenOptionsExt;
        let root = create_private_root(shared)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join(".credentials.login-lock"))
            .context("opening the sandbox login lock")?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|_| LoginError::Busy)?;
        Ok(Self { _file: file })
    }
}

/// The shared file's directory; created 0700 when absent (an existing one is left as is).
fn create_private_root(shared: &Path) -> Result<&Path, LoginError> {
    use std::os::unix::fs::DirBuilderExt;
    let root = shared.parent().context("shared credential has no parent")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .context("creating the sandbox credential dir")?;
    Ok(root)
}

/// Why the client ended. `Exited` is the client's own exit, `Killed` is ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperExit {
    Exited(std::process::ExitStatus),
    Killed(KillReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillReason {
    TimedOut,
    Cancelled,
}

impl HelperExit {
    fn success(self) -> bool {
        matches!(self, Self::Exited(status) if status.success())
    }

    /// `Err` for a kill; otherwise a note like `exit 1` or `signal 9` (no output, no secrets).
    fn note(self) -> Result<String, LoginError> {
        use std::os::unix::process::ExitStatusExt;
        match self {
            Self::Killed(KillReason::TimedOut) => Err(LoginError::Timeout),
            Self::Killed(KillReason::Cancelled) => Err(LoginError::Cancelled),
            Self::Exited(status) => Ok(match (status.code(), status.signal()) {
                (Some(code), _) => format!("exit {code}"),
                (None, Some(signal)) => format!("signal {signal}"),
                (None, None) => "unknown exit".to_string(),
            }),
        }
    }
}

pub(crate) struct LoginHelper {
    /// 16 hex, random, not derived from any secret.
    id: String,
    login: &'static AgentLogin,
    url: OnceLock<String>,
    shared: PathBuf,
    timeouts: HelperTimeouts,
    /// stdout and stderr in arrival order, for failure text.
    output: Arc<std::sync::Mutex<Output>>,
    /// stdout alone, the only place the link is looked for.
    stdout: Arc<std::sync::Mutex<Output>>,
    output_rev: watch::Receiver<u64>,
    /// `Some` once the client ended, its output was drained, cleanup ran and the login
    /// lock was released.
    exit: watch::Receiver<Option<HelperExit>>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    kill: mpsc::Sender<()>,
    submit: tokio::sync::Mutex<()>,
    #[cfg(test)]
    pid: Option<u32>,
}

fn random_id() -> String {
    use rand::RngExt;
    format!("{:016x}", rand::rng().random::<u64>())
}

impl LoginHelper {
    /// Logs in the file the sandboxes of `tool` are gated on, as
    /// [`sandbox_login_status`](crate::session::config::container_config::sandbox_login_status) reads it.
    /// `command` is the one a session's launch runs for `tool` when it overrides the
    /// profile's; `None` is the profile's.
    pub(crate) async fn start(
        profile: &str,
        tool: &str,
        command: Option<&str>,
        force: bool,
    ) -> Result<Arc<Self>, LoginError> {
        let config = crate::session::config::profile_config::resolve_config_or_warn(profile);
        if config.sandbox.shared_credential != SharedCredentialMode::SeparateLogin {
            return Err(LoginError::NotSeparateLogin);
        }
        // The target first, as `sandbox_login_status` resolves it: an agent without a
        // sandbox login is told so on every runtime.
        let target = login_target_with_command(profile, tool, command)
            .with_context(|| format!("{tool} has no sandbox login"))?;
        // Before the credential file is created: nothing can be logged in on this runtime.
        let runtime = crate::containers::get_container_runtime();
        if !supports_runtime(runtime.kind) {
            return Err(crate::session::SandboxLoginUnsupported.into());
        }
        let login = target.login;
        let shared = target.credential;
        create_private_root(&shared)?;
        ensure_shared_credential_file(&shared)?;
        // The lock first: a login that finishes meanwhile is seen by the check below
        // instead of racing a second one.
        let lock = LoginLock::try_acquire(&shared)?;
        let content = read_shared_credential(&shared)?;
        if !force && login.usable(content.as_deref()) {
            return Err(LoginError::AlreadyLoggedIn);
        }
        let binary = runtime.base.binary;
        let image = if config.sandbox.default_image.is_empty() {
            runtime.default_sandbox_image().to_string()
        } else {
            config.sandbox.default_image.clone()
        };
        let root = shared.parent().context("shared credential has no parent")?;
        let id = random_id();
        let name = format!("{HELPER_NAME_PREFIX}{id}");
        let mounts = HelperMounts {
            credential: shared.clone(),
            credential_in_container: target.credential_in_container,
            home_seed: root
                .join(".login-helper")
                .join(login.home_seed.path.trim_start_matches('.')),
            home_seed_in_container: format!("{CONTAINER_HOME}/{}", login.home_seed.path),
        };
        let relabel = config.sandbox.selinux_relabel && runtime.base.supports_selinux_relabel;
        let argv = helper_argv(runtime.kind, binary, &image, &name, &mounts, login, relabel)?;
        let kind = runtime.kind;
        sweep_root(kind, binary, &shared).await;
        tokio::task::spawn_blocking(move || {
            runtime
                .ensure_image(&image, &tokio_util::sync::CancellationToken::new())
                .map_err(|e| anyhow::anyhow!("pulling the sandbox image: {e}"))
        })
        .await
        .context("the sandbox image task failed")??;
        write_home_seed(&mounts.home_seed, login.home_seed.content)?;
        let cleanup = Some(rm_argv(kind, binary, &[name]));
        Self::start_inner(
            id,
            login,
            lock,
            argv,
            shared,
            cleanup,
            HelperTimeouts::default(),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn start_with(
        login: &'static AgentLogin,
        argv: Vec<String>,
        shared: PathBuf,
        cleanup: Option<Vec<String>>,
        timeouts: HelperTimeouts,
    ) -> Result<Arc<Self>, LoginError> {
        let lock = LoginLock::try_acquire(&shared)?;
        Self::start_inner(random_id(), login, lock, argv, shared, cleanup, timeouts).await
    }

    async fn start_inner(
        id: String,
        login: &'static AgentLogin,
        lock: LoginLock,
        argv: Vec<String>,
        shared: PathBuf,
        cleanup: Option<Vec<String>>,
        timeouts: HelperTimeouts,
    ) -> Result<Arc<Self>, LoginError> {
        let (program, args) = argv.split_first().context("empty helper command")?;
        let spawned = tokio::time::Instant::now();
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .env_remove("INVOCATION_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting the sandbox login helper")?;
        let name = format!("{HELPER_NAME_PREFIX}{id}");
        tracing::info!(target: LOG_TARGET, helper = %name, "Sandbox login helper started");
        #[cfg(test)]
        let pid = child.id();
        let stdin = child.stdin.take();
        let output = Arc::new(std::sync::Mutex::new(Output::default()));
        let stdout = Arc::new(std::sync::Mutex::new(Output::default()));
        let (rev_tx, output_rev) = watch::channel(0u64);
        let rev_tx = Arc::new(rev_tx);
        let readers = [
            child
                .stdout
                .take()
                .map(|r| spawn_reader(r, vec![Arc::clone(&output), Arc::clone(&stdout)], &rev_tx)),
            child
                .stderr
                .take()
                .map(|r| spawn_reader(r, vec![Arc::clone(&output)], &rev_tx)),
        ];
        drop(rev_tx);
        let (exit_tx, exit) = watch::channel(None);
        let (kill, kill_rx) = mpsc::channel(1);
        tokio::spawn(supervise(Supervised {
            name,
            child,
            readers,
            kill_rx,
            total: spawned + timeouts.total,
            cleanup,
            lock,
            exit_tx,
        }));
        let helper = Arc::new(Self {
            id,
            login,
            url: OnceLock::new(),
            shared,
            timeouts,
            output,
            stdout,
            output_rev,
            exit,
            stdin: tokio::sync::Mutex::new(stdin),
            kill,
            submit: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            pid,
        });
        helper.await_url(spawned + timeouts.url).await?;
        Ok(helper)
    }

    async fn await_url(&self, deadline: tokio::time::Instant) -> Result<(), LoginError> {
        let mut rev = self.output_rev.clone();
        let mut rev_open = true;
        let mut exit = self.exit.clone();
        loop {
            // Read the exit before the text: once exited, the drained text is final.
            let exited = *exit.borrow_and_update();
            let stdout = lock(&self.stdout).text.clone();
            if let Some(url) = (self.login.login_url)(&stdout) {
                let _ = self.url.set(url);
                return Ok(());
            }
            if let Some(exited) = exited {
                let note = exited.note()?;
                let text = (self.login.failure_message)(&lock(&self.output).text, &[]);
                return Err(LoginError::NoUrl {
                    output: format!("{text} ({note})"),
                });
            }
            tokio::select! {
                changed = rev.changed(), if rev_open => rev_open = changed.is_ok(),
                changed = exit.changed() => if changed.is_err() {
                    return Err(LoginError::Gone);
                },
                () = tokio::time::sleep_until(deadline) => {
                    tracing::info!(target: LOG_TARGET, helper = %self.name(), "Sandbox login helper timed out");
                    self.kill_and_cleanup().await;
                    return Err(LoginError::Timeout);
                }
            }
        }
    }

    fn name(&self) -> String {
        format!("{HELPER_NAME_PREFIX}{}", self.id)
    }

    pub(crate) fn url(&self) -> &str {
        self.url.get().map_or("", String::as_str)
    }

    /// One code per helper: `Ok(())` means logged in; any `Err` ends this helper
    /// (a wrong code is `Failed { message }`; trying again needs a new `start`).
    pub(crate) async fn submit_code(&self, code: LoginCode) -> Result<(), LoginError> {
        let _one = self.submit.lock().await;
        let Some(mut stdin) = self.stdin.lock().await.take() else {
            return Err(LoginError::Gone);
        };
        if self.is_finished() {
            return Err(LoginError::Gone);
        }
        let mark = lock(&self.output).mark();
        // Only a credential that differs from this one counts as the new login (`--force`).
        let before = read_shared_credential(&self.shared).ok().flatten();
        // A write error means the client is already gone; its exit decides below.
        let _ = async {
            stdin.write_all(code.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        }
        .await;
        drop(stdin);
        let mut exit = self.exit.clone();
        let mut rev = self.output_rev.clone();
        let mut rev_open = true;
        let deadline = tokio::time::Instant::now() + self.timeouts.after_code;
        let exited = loop {
            // Measured: a rejected code is reported on the terminal and the client then
            // keeps running without ever exiting, so the text has to end the wait.
            let since = lock(&self.output).since(mark).to_string();
            if (self.login.rejected_code)(&since) {
                self.kill_and_cleanup().await;
                let text = (self.login.failure_message)(&since, &[code.as_str(), self.url()]);
                return Err(LoginError::Failed { message: text });
            }
            tokio::select! {
                exited = exit.wait_for(Option::is_some) => break exited.ok().and_then(|e| *e),
                changed = rev.changed(), if rev_open => rev_open = changed.is_ok(),
                () = tokio::time::sleep_until(deadline) => break *self.exit.borrow(),
            }
        };
        let Some(exited) = exited else {
            // The client may have written the credential and still not exited.
            let landed = read_shared_credential(&self.shared).ok().flatten();
            let logged_in = landed != before && self.login.usable(landed.as_deref());
            tracing::info!(target: LOG_TARGET, helper = %self.name(), logged_in, "Sandbox login helper timed out");
            self.kill_and_cleanup().await;
            return if logged_in {
                Ok(())
            } else {
                Err(LoginError::Timeout)
            };
        };
        let note = exited.note()?;
        if !exited.success() {
            let since = lock(&self.output).since(mark).to_string();
            let text = (self.login.failure_message)(&since, &[code.as_str(), self.url()]);
            return Err(LoginError::Failed {
                message: format!("{text} ({note})"),
            });
        }
        match read_shared_credential(&self.shared) {
            Ok(content) if !self.login.usable(content.as_deref()) => Err(LoginError::Failed {
                message: SUCCESS_WITHOUT_CREDENTIAL.to_string(),
            }),
            Ok(content) if content == before => Err(LoginError::Failed {
                message: SUCCESS_WITHOUT_NEW_CREDENTIAL.to_string(),
            }),
            Ok(_) => Ok(()),
            Err(e) => Err(LoginError::Failed {
                message: format!("could not verify the shared credential after the login: {e:#}"),
            }),
        }
    }

    pub(crate) async fn cancel(&self) {
        self.kill_and_cleanup().await;
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.exit.borrow().is_some()
    }

    async fn kill_and_cleanup(&self) {
        let _ = self.kill.try_send(());
        drop(self.stdin.lock().await.take());
        let mut exit = self.exit.clone();
        let _ = tokio::time::timeout(
            KILL_WAIT + DRAIN_WAIT + CLEANUP_WAIT,
            exit.wait_for(Option::is_some),
        )
        .await;
    }

    /// Resolves once the helper ended on its own (timeout, kill, client exit), with why.
    /// Not for a submitted code: `submit_code` gives that outcome.
    pub(crate) async fn ended(&self) -> LoginError {
        let mut exit = self.exit.clone();
        let exited = exit.wait_for(Option::is_some).await.ok().and_then(|e| *e);
        let Some(exited) = exited else {
            return LoginError::Gone;
        };
        match exited.note() {
            Err(reason) => reason,
            Ok(note) => {
                let text = (self.login.failure_message)(&lock(&self.output).text, &[self.url()]);
                LoginError::Failed {
                    message: format!("{text} ({note})"),
                }
            }
        }
    }
}

fn lock(output: &std::sync::Mutex<Output>) -> std::sync::MutexGuard<'_, Output> {
    output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_reader(
    mut reader: impl AsyncRead + Unpin + Send + 'static,
    outputs: Vec<Arc<std::sync::Mutex<Output>>>,
    rev: &Arc<watch::Sender<u64>>,
) -> JoinHandle<()> {
    let rev = Arc::clone(rev);
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    for output in &outputs {
                        lock(output).push(&chunk);
                    }
                    rev.send_modify(|r| *r += 1);
                }
            }
        }
    })
}

struct Supervised {
    name: String,
    child: Child,
    readers: [Option<JoinHandle<()>>; 2],
    kill_rx: mpsc::Receiver<()>,
    total: tokio::time::Instant,
    cleanup: Option<Vec<String>>,
    lock: LoginLock,
    exit_tx: watch::Sender<Option<HelperExit>>,
}

/// Owns the client: ends it on a kill request, a dropped helper or the total deadline,
/// then drains its output, removes the container and releases the login lock.
async fn supervise(mut s: Supervised) {
    let exited = tokio::select! {
        status = s.child.wait() => match status {
            Ok(status) => HelperExit::Exited(status),
            Err(_) => HelperExit::Killed(KillReason::Cancelled),
        },
        // A closed channel means the helper was dropped: end the client with it.
        _ = s.kill_rx.recv() => {
            kill(&mut s.child).await;
            HelperExit::Killed(KillReason::Cancelled)
        }
        () = tokio::time::sleep_until(s.total) => {
            tracing::info!(target: LOG_TARGET, helper = %s.name, "Sandbox login helper timed out");
            kill(&mut s.child).await;
            HelperExit::Killed(KillReason::TimedOut)
        }
    };
    match exited.note() {
        Ok(note) => {
            tracing::info!(target: LOG_TARGET, helper = %s.name, exit = %note, "Sandbox login helper exited")
        }
        Err(reason) => {
            tracing::info!(target: LOG_TARGET, helper = %s.name, reason = %reason, "Sandbox login helper killed")
        }
    }
    for reader in s.readers.iter_mut().flatten() {
        if tokio::time::timeout(DRAIN_WAIT, &mut *reader)
            .await
            .is_err()
        {
            reader.abort();
        }
    }
    // `--rm` normally removes the container; killing the client alone may leave it running.
    if let Some(argv) = &s.cleanup {
        run_cleanup(argv).await;
    }
    drop(s.lock);
    let _ = s.exit_tx.send(Some(exited));
}

async fn kill(child: &mut Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(KILL_WAIT, child.wait()).await;
}

async fn run_cleanup(argv: &[String]) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let status = tokio::process::Command::new(program)
        .args(args)
        .env_remove("INVOCATION_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    let _ = tokio::time::timeout(CLEANUP_WAIT, status).await;
}

/// `rm -f` without podman's 10 s stop grace (`-t 0`); Docker's `rm -f` kills at once
/// and has no `-t`.
fn rm_argv(kind: RuntimeKind, binary: &str, ids: &[String]) -> Vec<String> {
    let mut argv = vec![binary.to_string(), "rm".to_string(), "-f".to_string()];
    if kind == RuntimeKind::Podman {
        argv.extend(["-t".to_string(), "0".to_string()]);
    }
    argv.extend(ids.iter().cloned());
    argv
}

/// Removes leftover helpers of `shared`'s file: a dropped runtime (daemon stop, killed
/// CLI) kills only the client, and the container keeps waiting on its closed stdin.
/// The daemon calls this at startup. `Busy` while a live helper of this file holds
/// the login lock, in this or another process.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unused until the daemon sweeps at startup")
)]
pub(crate) async fn sweep_stale_helpers(
    runtime: &crate::containers::ContainerRuntime,
    shared: &Path,
) -> Result<(), LoginError> {
    if runtime.kind == RuntimeKind::AppleContainer {
        return Ok(());
    }
    let _lock = LoginLock::try_acquire(shared)?;
    sweep_root(runtime.kind, runtime.base.binary, shared).await;
    Ok(())
}

/// Only called with `shared`'s login lock held, so no live helper of it can match.
async fn sweep_root(kind: RuntimeKind, binary: &str, shared: &Path) {
    let label = format!("label={}", helper_label(shared));
    let ps = tokio::process::Command::new(binary)
        .args(["ps", "-aq", "--filter", &label])
        .env_remove("INVOCATION_ID")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let ids: Vec<String> = match tokio::time::timeout(CLEANUP_WAIT, ps).await {
        Ok(Ok(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        _ => {
            tracing::warn!(target: LOG_TARGET, "Sandbox login helper stale sweep failed");
            return;
        }
    };
    if ids.is_empty() {
        return;
    }
    tracing::info!(target: LOG_TARGET, count = ids.len(), "Removing stale sandbox login helpers");
    run_cleanup(&rm_argv(kind, binary, &ids)).await;
}

/// The helper-private seed file, rewritten before each run.
fn write_home_seed(path: &Path, content: &str) -> Result<(), LoginError> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let dir = path.parent().context("login helper seed has no parent")?;
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(anyhow::anyhow!(
                "{} is not a directory (a symlink or a file)",
                dir.display()
            )
            .into())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .context("creating the login helper dir")?,
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context("inspecting the login helper dir")
                .into())
        }
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .context("restricting the login helper dir")?;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .and_then(|mut file| {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(content.as_bytes())
        })
        .context("writing the login helper seed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::claude::LOGIN;
    use super::*;
    use crate::session::config::container_config::login_target;
    use tempfile::TempDir;

    // Measured shape, `state`/`code_challenge` replaced by SAMPLE values.
    const MEASURED_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code+user%3Amcp_servers+user%3Afile_upload+user%3Aplugins&code_challenge=SAMPLEchallenge&code_challenge_method=S256&state=SAMPLEstate";
    const DEADLINE: Duration = Duration::from_secs(10);
    const WRONG_LINE: &str = "Login failed: Request failed with status code 400";

    /// Imitates the measured plain output: the URL line over two writes, the prompt
    /// without a newline, one `read`. `HANG_ON_EOF=1` ignores EOF like the real client.
    const FAKE_HELPER: &str = r#"#!/bin/sh
printf 'Opening browser to sign in\342\200\246\n'
half=${#URL}; half=$((half / 2))
printf "If the browser didn't open, visit: %s" "$(printf %s "$URL" | cut -c1-$half)"
printf '%s\n' "$(printf %s "$URL" | cut -c$((half + 1))-)"
printf 'Paste code here if prompted > '
if ! IFS= read -r code; then
  [ -n "$HANG_ON_EOF" ] && exec sleep 3600
  exit 3
fi
case "$code" in
  good-code#good-state)
    printf '{"claudeAiOauth":{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":%s}}' "$(( $(date +%s) * 1000 + 3600000 ))" > "$CRED"
    printf 'Login successful.\n'; exit 0 ;;
  lie*) printf 'Login successful.\n'; exit 0 ;;
  late*)
    printf '{"claudeAiOauth":{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":%s}}' "$(( $(date +%s) * 1000 + 3600000 ))" > "$CRED"
    exec sleep 3600 ;;
  hang*) exec sleep 3600 ;;
  invalid*) printf 'Invalid code. Please make sure the full code was copied.\n'; exec sleep 3600 ;;
  echo*) printf '%s\n%s\n' "$code" "$URL"; printf 'Login failed: Request failed with status code 400\n' >&2; exit 1 ;;
  *) printf 'Login failed: Request failed with status code 400\n'; exit 1 ;;
esac
"#;

    struct Fake {
        tmp: TempDir,
        shared: PathBuf,
    }

    impl Fake {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let shared = tmp.path().join("sandbox-v2").join(".credentials.json");
            Self::at(tmp, shared)
        }

        /// A fake helper filling `shared`, the file a real start would mount.
        fn at(tmp: TempDir, shared: PathBuf) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let script = tmp.path().join("fake.sh");
            std::fs::write(&script, FAKE_HELPER).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
            std::fs::write(&shared, "{}").unwrap();
            Self { tmp, shared }
        }

        fn argv(&self, hang_on_eof: bool) -> Vec<String> {
            let mut argv = vec![
                "/usr/bin/env".to_string(),
                format!("URL={MEASURED_URL}"),
                format!("CRED={}", self.shared.display()),
            ];
            if hang_on_eof {
                argv.push("HANG_ON_EOF=1".to_string());
            }
            argv.push("/bin/sh".to_string());
            argv.push(self.tmp.path().join("fake.sh").display().to_string());
            argv
        }

        fn cleaned(&self) -> PathBuf {
            self.tmp.path().join("cleaned")
        }

        fn cleanup(&self) -> Option<Vec<String>> {
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("touch '{}'", self.cleaned().display()),
            ])
        }

        async fn start(&self, hang_on_eof: bool) -> Result<Arc<LoginHelper>, LoginError> {
            self.start_timed(hang_on_eof, HelperTimeouts::default())
                .await
        }

        async fn start_timed(
            &self,
            hang_on_eof: bool,
            timeouts: HelperTimeouts,
        ) -> Result<Arc<LoginHelper>, LoginError> {
            bounded(LoginHelper::start_with(
                &LOGIN,
                self.argv(hang_on_eof),
                self.shared.clone(),
                self.cleanup(),
                timeouts,
            ))
            .await
        }
    }

    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(DEADLINE, future)
            .await
            .expect("deadline exceeded")
    }

    fn code(raw: &str) -> LoginCode {
        LoginCode::parse(raw.to_string()).unwrap()
    }

    fn assert_gone(pid: u32) {
        let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None);
        assert_eq!(
            alive,
            Err(nix::errno::Errno::ESRCH),
            "pid {pid} still exists"
        );
    }

    fn mounts() -> HelperMounts {
        HelperMounts {
            credential: PathBuf::from("/h/.claude/sandbox-v2/.credentials.json"),
            credential_in_container: "/root/.claude/.credentials.json".to_string(),
            home_seed: PathBuf::from("/h/.claude/sandbox-v2/.login-helper/claude.json"),
            home_seed_in_container: "/root/.claude.json".to_string(),
        }
    }

    #[test]
    fn sandbox_login_helper_argv_is_minimal() {
        let name = "aoe-sandbox-login-0123456789abcdef";
        let argv = helper_argv(
            RuntimeKind::Podman,
            "podman",
            "img:tag",
            name,
            &mounts(),
            &LOGIN,
            false,
        )
        .unwrap();
        let label = helper_label(&mounts().credential);
        let hash = label
            .strip_prefix("com.agent-of-empires.sandbox-login-helper=")
            .unwrap();
        assert!(
            hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "{label}"
        );
        assert_ne!(
            label,
            helper_label(Path::new("/h/.claude-work/sandbox-v2/.credentials.json")),
            "each shared file has its own label"
        );
        assert_eq!(
            argv,
            [
                "podman",
                "run",
                "--rm",
                "-i",
                "--name",
                name,
                "--label",
                label.as_str(),
                "-v",
                "/h/.claude/sandbox-v2/.credentials.json:/root/.claude/.credentials.json",
                "-v",
                "/h/.claude/sandbox-v2/.login-helper/claude.json:/root/.claude.json",
                "-e",
                "HOME=/root",
                "img:tag",
                "claude",
                "auth",
                "login",
            ]
        );
        for flag in ["-t", "-w", "--network", "-p", "--env-file", "-it"] {
            assert!(!argv.iter().any(|a| a == flag), "{flag}");
        }
        assert_eq!(argv.iter().filter(|a| *a == "-e").count(), 1);
        assert_eq!(argv.iter().filter(|a| *a == "-v").count(), 2);

        let relabeled = helper_argv(
            RuntimeKind::Docker,
            "docker",
            "img:tag",
            name,
            &mounts(),
            &LOGIN,
            true,
        )
        .unwrap();
        let volumes: Vec<&String> = relabeled
            .iter()
            .zip(relabeled.iter().skip(1))
            .filter(|(flag, _)| *flag == "-v")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(volumes.len(), 2);
        assert!(volumes.iter().all(|v| v.ends_with(":z")), "{volumes:?}");

        assert!(matches!(
            helper_argv(
                RuntimeKind::AppleContainer,
                "container",
                "img:tag",
                name,
                &mounts(),
                &LOGIN,
                false
            ),
            Err(LoginError::Unsupported(
                crate::session::SandboxLoginUnsupported
            ))
        ));
        match helper_argv(
            RuntimeKind::Podman,
            "podman",
            "--privileged",
            name,
            &mounts(),
            &LOGIN,
            false,
        ) {
            Err(LoginError::Other(e)) => {
                assert!(e.to_string().contains("cannot start with '-'"), "{e}")
            }
            other => panic!("{other:?}"),
        }
        // `-v` splits on ':', so such a path would mount the wrong place.
        for (credential, home_seed) in [
            ("/h/a:b/sandbox-v2/.credentials.json", "/h/seed/claude.json"),
            ("/h/.credentials.json", "/h/se:ed/claude.json"),
        ] {
            let mounts = HelperMounts {
                credential: PathBuf::from(credential),
                home_seed: PathBuf::from(home_seed),
                ..mounts()
            };
            match helper_argv(
                RuntimeKind::Docker,
                "docker",
                "img:tag",
                name,
                &mounts,
                &LOGIN,
                false,
            ) {
                Err(LoginError::Other(e)) => {
                    assert!(e.to_string().contains("contains ':'"), "{e}")
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            rm_argv(RuntimeKind::Podman, "podman", &[name.to_string()]),
            ["podman", "rm", "-f", "-t", "0", name]
        );
        assert_eq!(
            rm_argv(RuntimeKind::Docker, "docker", &[name.to_string()]),
            ["docker", "rm", "-f", name]
        );
    }

    #[test]
    fn sandbox_login_helper_home_seed_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join(".login-helper").join("claude.json");
        write_home_seed(&seed, LOGIN.home_seed.content).unwrap();
        assert_eq!(
            std::fs::read_to_string(&seed).unwrap(),
            r#"{"hasCompletedOnboarding":true}"#
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&seed), 0o600);
        assert_eq!(mode(seed.parent().unwrap()), 0o700);

        let planted = TempDir::new().unwrap();
        let linked = tmp.path().join("linked");
        std::os::unix::fs::symlink(planted.path(), &linked).unwrap();
        assert!(write_home_seed(&linked.join("claude.json"), LOGIN.home_seed.content).is_err());
        assert!(!planted.path().join("claude.json").exists());
    }

    /// A second integration (another command, output and credential format) runs the helper
    /// end to end through its descriptor.
    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_runs_a_second_integration() {
        use super::super::fake;
        let tmp = TempDir::new().unwrap();
        let shared = tmp.path().join("sandbox-v2").join(fake::CREDENTIAL_FILE);
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(&shared, "{}").unwrap();
        let script = r#"echo "starting"; echo "SIGNIN https://example.test/device"; IFS= read -r code; case "$code" in good*) printf TOKEN:9999999999999 > "$CRED"; exit 0 ;; *) echo "BAD CODE"; exec sleep 3600 ;; esac"#;
        let argv = |script: &str| {
            vec![
                "/usr/bin/env".to_string(),
                format!("CRED={}", shared.display()),
                "/bin/sh".to_string(),
                "-c".to_string(),
                script.to_string(),
            ]
        };

        let helper = bounded(LoginHelper::start_with(
            &fake::LOGIN,
            argv(script),
            shared.clone(),
            None,
            HelperTimeouts::default(),
        ))
        .await
        .unwrap();
        assert_eq!(helper.url(), "https://example.test/device");
        bounded(helper.submit_code(code("good-code")))
            .await
            .unwrap();
        let content = read_shared_credential(&shared).unwrap();
        assert_eq!(content.as_deref(), Some("TOKEN:9999999999999"));
        assert!(fake::LOGIN.usable(content.as_deref()));
        assert!(
            !LOGIN.usable(content.as_deref()),
            "not a login of the other integration"
        );

        std::fs::write(&shared, "{}").unwrap();
        let helper = bounded(LoginHelper::start_with(
            &fake::LOGIN,
            argv(script),
            shared.clone(),
            None,
            HelperTimeouts::default(),
        ))
        .await
        .unwrap();
        match bounded(helper.submit_code(code("wrong"))).await {
            Err(LoginError::Failed { message }) => assert_eq!(message, "BAD CODE"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_happy_path() {
        let fake = Fake::new();
        let helper = fake.start(false).await.unwrap();
        assert_eq!(helper.url(), MEASURED_URL);
        assert_eq!(helper.id.len(), 16);
        assert!(helper.id.bytes().all(|b| b.is_ascii_hexdigit()));
        bounded(helper.submit_code(code("good-code#good-state")))
            .await
            .unwrap();
        let content = read_shared_credential(&fake.shared).unwrap();
        assert!(LOGIN.usable(content.as_deref()));
        assert!(helper.is_finished());
        assert!(fake.cleaned().exists(), "cleanup runs after every exit");
        assert!(matches!(
            bounded(helper.submit_code(code("good-code#good-state"))).await,
            Err(LoginError::Gone)
        ));
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_wrong_code_ends_the_helper() {
        let fake = Fake::new();
        let helper = fake.start(false).await.unwrap();
        match bounded(helper.submit_code(code("wrong-AAAA#BBBB"))).await {
            Err(LoginError::Failed { message }) => {
                assert!(message.contains(WRONG_LINE), "{message}");
                assert!(message.ends_with("(exit 1)"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(helper.is_finished());
        assert!(matches!(
            bounded(helper.submit_code(code("wrong-AAAA#BBBB"))).await,
            Err(LoginError::Gone)
        ));
    }

    /// The fake prints the code and the URL back on stdout, the error on stderr.
    async fn echo_flow(fake: &Fake) {
        let helper = fake.start(false).await.unwrap();
        let message = match bounded(helper.submit_code(code("echo-AAAA#BBBB"))).await {
            Err(LoginError::Failed { message }) => message,
            other => panic!("{other:?}"),
        };
        assert!(message.contains(WRONG_LINE), "{message}");
        for leak in [
            "echo-AAAA",
            "BBBB",
            MEASURED_URL,
            "SAMPLEstate",
            "SAMPLEchallenge",
        ] {
            assert!(!message.contains(leak), "{leak} leaked: {message}");
        }
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_failure_message_never_contains_the_code() {
        echo_flow(&Fake::new()).await;
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_exit_without_credential_fails() {
        let fake = Fake::new();
        let helper = fake.start(false).await.unwrap();
        match bounded(helper.submit_code(code("lie"))).await {
            Err(LoginError::Failed { message }) => {
                assert_eq!(message, SUCCESS_WITHOUT_CREDENTIAL)
            }
            other => panic!("{other:?}"),
        }

        // `--force` over a usable credential: exit 0 without a write is no new login.
        let fake = Fake::new();
        let expires = chrono::Utc::now().timestamp_millis() + 3_600_000;
        std::fs::write(
            &fake.shared,
            crate::session::sandbox_login::claude::fixtures::usable(expires as u64),
        )
        .unwrap();
        let helper = fake.start(false).await.unwrap();
        match bounded(helper.submit_code(code("lie"))).await {
            Err(LoginError::Failed { message }) => {
                assert_eq!(message, SUCCESS_WITHOUT_NEW_CREDENTIAL)
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_no_url() {
        let fake = Fake::new();
        let argv = [
            "sh",
            "-c",
            "printf 'Error: getaddrinfo EAI_AGAIN claude.ai\\n' >&2; exit 1",
        ]
        .map(str::to_string)
        .to_vec();
        let result = bounded(LoginHelper::start_with(
            &LOGIN,
            argv,
            fake.shared.clone(),
            fake.cleanup(),
            HelperTimeouts::default(),
        ))
        .await;
        match result {
            Err(LoginError::NoUrl { output }) => {
                assert!(
                    output.contains("Error: getaddrinfo EAI_AGAIN claude.ai"),
                    "{output}"
                );
                assert!(output.ends_with("(exit 1)"), "{output}");
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("a URL was found"),
        }
        assert!(fake.cleaned().exists());
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_total_timeout_kills_and_cleans_up() {
        let fake = Fake::new();
        let helper = fake
            .start_timed(
                true,
                HelperTimeouts {
                    url: Duration::from_secs(5),
                    after_code: Duration::from_secs(5),
                    total: Duration::from_secs(1),
                },
            )
            .await
            .unwrap();
        assert!(matches!(bounded(helper.ended()).await, LoginError::Timeout));
        assert!(fake.cleaned().exists());
        assert_gone(helper.pid.unwrap());
        assert!(matches!(
            bounded(helper.submit_code(code("good-code#good-state"))).await,
            Err(LoginError::Gone)
        ));
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_url_timeout() {
        let url_after = |url: u64, total: u64| HelperTimeouts {
            url: Duration::from_millis(url),
            total: Duration::from_millis(total),
            ..HelperTimeouts::default()
        };
        // (case, output before hanging, timeouts): a link without its line end is never taken.
        for (case, prints, timeouts) in [
            ("silent", "", url_after(200, 600_000)),
            ("total deadline first", "", url_after(5_000, 300)),
            (
                "link without newline",
                r#"printf "visit: %s" "$URL";"#,
                url_after(300, 600_000),
            ),
            (
                "half a link",
                r#"printf "visit: %s" "$(printf %s "$URL" | cut -c1-200)";"#,
                url_after(300, 600_000),
            ),
        ] {
            let fake = Fake::new();
            let pid_file = fake.tmp.path().join("pid");
            let argv = vec![
                "/usr/bin/env".to_string(),
                format!("URL={MEASURED_URL}"),
                "sh".to_string(),
                "-c".to_string(),
                format!(
                    "echo $$ > '{}'; {prints} exec sleep 3600",
                    pid_file.display()
                ),
            ];
            let result = bounded(LoginHelper::start_with(
                &LOGIN,
                argv,
                fake.shared.clone(),
                fake.cleanup(),
                timeouts,
            ))
            .await;
            assert!(
                matches!(result, Err(LoginError::Timeout)),
                "{case}: {:?}",
                result.err()
            );
            assert!(fake.cleaned().exists(), "{case}");
            let pid = std::fs::read_to_string(&pid_file).unwrap();
            assert_gone(pid.trim().parse().unwrap());
        }
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_a_rejected_code_fails_at_once_and_kills_the_client() {
        // The long after-code timeout must not be what ends this: the client keeps running.
        let timeouts = HelperTimeouts {
            after_code: Duration::from_secs(60),
            ..HelperTimeouts::default()
        };
        let fake = Fake::new();
        let helper = fake.start_timed(true, timeouts).await.unwrap();
        let pid = helper.pid.unwrap();
        let result = bounded(helper.submit_code(code("invalid-AAAA#BBBB"))).await;
        match result {
            Err(LoginError::Failed { message }) => {
                assert!(message.contains("Invalid code"), "{message}");
                assert!(!message.contains("invalid-AAAA"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(fake.cleaned().exists());
        assert_gone(pid);
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_after_code_timeout_checks_the_credential() {
        let timeouts = HelperTimeouts {
            after_code: Duration::from_millis(300),
            ..HelperTimeouts::default()
        };
        // (code, expected): the client wrote a credential but never exited, or wrote none.
        for (raw, logged_in) in [("late-AAAA#BBBB", true), ("hang", false)] {
            let fake = Fake::new();
            let helper = fake.start_timed(true, timeouts).await.unwrap();
            let pid = helper.pid.unwrap();
            let result = bounded(helper.submit_code(code(raw))).await;
            if logged_in {
                result.unwrap();
            } else {
                assert!(matches!(result, Err(LoginError::Timeout)), "{result:?}");
            }
            assert!(fake.cleaned().exists(), "{raw}");
            assert_gone(pid);
        }
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_dropped_while_waiting_kills_and_cleans_up() {
        let fake = Fake::new();
        let helper = fake.start(true).await.unwrap();
        let pid = helper.pid.unwrap();
        drop(helper);
        // The lock is released only after the kill and the cleanup.
        let next = bounded(async {
            loop {
                match LoginHelper::start_with(
                    &LOGIN,
                    fake.argv(true),
                    fake.shared.clone(),
                    None,
                    HelperTimeouts::default(),
                )
                .await
                {
                    Err(LoginError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
                    other => break other,
                }
            }
        })
        .await
        .unwrap();
        assert!(fake.cleaned().exists());
        assert_gone(pid);
        bounded(next.cancel()).await;
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_sweep_stays_in_its_root() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let shared = |dir: &str| tmp.path().join(dir).join("sandbox-v2/.credentials.json");
        let (a, b) = (shared(".claude"), shared(".claude-work"));
        let hash = |p: &Path| helper_label(p).rsplit('=').next().unwrap().to_string();
        // A fake runtime: `ps --filter label=<key>=<hash>` lists the ids stored under the
        // hash, a bare `label=<key>` filter matches every helper, `rm` records its arguments.
        let runtime_dir = tmp.path().join("runtime");
        let ids = runtime_dir.join("ids");
        std::fs::create_dir_all(&ids).unwrap();
        std::fs::write(ids.join(hash(&a)), "helper-a\n").unwrap();
        std::fs::write(ids.join(hash(&b)), "helper-b\n").unwrap();
        let log = runtime_dir.join("rm.log");
        let script = runtime_dir.join("podman");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
case "$1" in
  ps) filter=${{4#label=}}
      case "$filter" in
        *=*) cat '{ids}'/"${{filter##*=}}" 2>/dev/null ;;
        *) cat '{ids}'/* ;;
      esac ;;
  rm) shift; echo "$@" >> '{log}' ;;
esac
"#,
                ids = ids.display(),
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let binary: &'static str = Box::leak(script.display().to_string().into_boxed_str());
        let runtime = crate::containers::ContainerRuntime {
            base: crate::containers::runtime_base::RuntimeBase {
                binary,
                ..crate::containers::runtime_base::RuntimeBase::PODMAN
            },
            kind: RuntimeKind::Podman,
        };

        // A's helper is live: its login lock is held.
        let _live_a = LoginLock::try_acquire(&a).unwrap();
        assert!(matches!(
            sweep_stale_helpers(&runtime, &a).await,
            Err(LoginError::Busy)
        ));
        sweep_stale_helpers(&runtime, &b).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "-f -t 0 helper-b\n",
            "only B's helper is removed"
        );
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_cancel_kills() {
        let fake = Fake::new();
        let helper = fake.start(true).await.unwrap();
        assert!(!helper.is_finished());
        bounded(helper.cancel()).await;
        assert!(helper.is_finished());
        assert!(fake.cleaned().exists());
        assert_gone(helper.pid.unwrap());
        assert!(matches!(
            bounded(helper.ended()).await,
            LoginError::Cancelled
        ));
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_ended_by_client_exit_redacts_the_link() {
        let fake = Fake::new();
        let script = r#"printf 'visit: %s\n' "$URL"; printf 'crashed\n' >&2; exit 4"#;
        let argv = [
            "/usr/bin/env",
            &format!("URL={MEASURED_URL}"),
            "/bin/sh",
            "-c",
            script,
        ]
        .map(str::to_string)
        .to_vec();
        let helper = bounded(LoginHelper::start_with(
            &LOGIN,
            argv,
            fake.shared.clone(),
            fake.cleanup(),
            HelperTimeouts::default(),
        ))
        .await
        .unwrap();
        match bounded(helper.ended()).await {
            LoginError::Failed { message } => {
                assert!(message.contains("crashed"), "{message}");
                assert!(message.ends_with("(exit 4)"), "{message}");
                for leak in [MEASURED_URL, "SAMPLEstate", "SAMPLEchallenge"] {
                    assert!(!message.contains(leak), "{leak} leaked: {message}");
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_second_start_is_busy() {
        let fake = Fake::new();
        let first = fake.start(false).await.unwrap();
        assert!(matches!(fake.start(false).await, Err(LoginError::Busy)));
        bounded(first.submit_code(code("good-code#good-state")))
            .await
            .unwrap();
        let next = fake.start(true).await.unwrap();
        bounded(next.cancel()).await;
    }

    #[tokio::test]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_does_not_hold_the_config_lock() {
        let fake = Fake::new();
        let helper = fake.start(true).await.unwrap();
        let shared = fake.shared.clone();
        let locked = tokio::task::spawn_blocking(move || {
            crate::hooks::with_config_lock_policy(
                &shared,
                "lock",
                crate::hooks::SymlinkPolicy::Never,
                || Ok(()),
            )
        });
        tokio::time::timeout(Duration::from_secs(2), locked)
            .await
            .expect("the config lock is free while a helper waits")
            .unwrap()
            .unwrap();
        bounded(helper.cancel()).await;
    }

    /// Every helper test shares the `sandbox_login_helper` key: helper events emitted on
    /// other test threads while this capture runs can leave its callsites disabled.
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_helper_logs_no_secrets() {
        let capture = crate::session::test_support::LogCapture::start();
        let fake = Fake::new();
        echo_flow(&fake).await;
        let helper = fake.start(false).await.unwrap();
        bounded(helper.submit_code(code("good-code#good-state")))
            .await
            .unwrap();
        let logs = capture.contents();
        assert!(logs.contains("Sandbox login helper started"), "{logs}");
        assert!(logs.contains("Sandbox login helper exited"), "{logs}");
        for leak in [
            "echo-AAAA",
            "good-code",
            "good-state",
            "BBBB",
            MEASURED_URL,
            "oauth/authorize",
            "SAMPLEstate",
            "SAMPLEchallenge",
        ] {
            assert!(!logs.contains(leak), "{leak} leaked: {logs}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_helper_start_refuses_outside_separate_login() {
        let home = crate::session::test_support::isolate_app_dir();
        let profile = crate::session::config::effective_profile("");
        assert!(matches!(
            LoginHelper::start(&profile, "claude", None, false).await,
            Err(LoginError::NotSeparateLogin)
        ));
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
        })
        .unwrap();
        let shared = login_target(&profile, "claude").unwrap().credential;
        assert!(shared.starts_with(home.path()));
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        let expires_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 3_600_000;
        std::fs::write(
            &shared,
            crate::session::sandbox_login::claude::fixtures::usable(expires_at as u64),
        )
        .unwrap();
        assert!(matches!(
            LoginHelper::start(&profile, "claude", None, false).await,
            Err(LoginError::AlreadyLoggedIn)
        ));
    }

    /// The already-logged-in check reads the credential under the login lock: while a login
    /// holds the lock the credential is still changing, so a second start is `Busy` and never
    /// judges (or begins a login on) a file that login is about to write.
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_helper_start_checks_the_credential_under_the_login_lock() {
        let _home = crate::session::test_support::isolate_app_dir();
        let profile = crate::session::config::effective_profile("");
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
        })
        .unwrap();
        let shared = login_target(&profile, "claude").unwrap().credential;
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        let expires_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 3_600_000;
        std::fs::write(
            &shared,
            crate::session::sandbox_login::claude::fixtures::usable(expires_at as u64),
        )
        .unwrap();

        let held = LoginLock::try_acquire(&shared).unwrap();
        for force in [false, true] {
            assert!(
                matches!(
                    LoginHelper::start(&profile, "claude", None, force).await,
                    Err(LoginError::Busy)
                ),
                "force={force}: a login in progress holds the lock"
            );
        }
        drop(held);
        assert!(matches!(
            LoginHelper::start(&profile, "claude", None, false).await,
            Err(LoginError::AlreadyLoggedIn)
        ));
    }

    /// Apple Container cannot run the helper: the login refuses with the setting's message
    /// before it creates or reads anything, even with a usable credential or `--force`.
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_helper_start_refuses_on_apple_container() {
        let home = crate::session::test_support::isolate_app_dir();
        let profile = crate::session::config::effective_profile("");
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
            c.sandbox.container_runtime =
                crate::session::config::ContainerRuntimeName::AppleContainer;
        })
        .unwrap();
        for force in [false, true] {
            let error = LoginHelper::start(&profile, "claude", None, force)
                .await
                .err()
                .expect("refused");
            assert!(
                matches!(error, LoginError::Unsupported(_)),
                "{force}: {error:?}"
            );
            assert_eq!(
                error.to_string(),
                "sandbox.shared_credential is separate_login, which is not supported with Apple Container; use host_seed or Docker/Podman"
            );
        }
        let shared = login_target(&profile, "claude").unwrap().credential;
        assert!(shared.starts_with(home.path()));
        assert!(!shared.exists(), "nothing was created");
    }

    /// The target is resolved before the runtime is checked: an agent without a sandbox
    /// login is told so on Apple Container too, not given the Apple refusal.
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_helper_start_names_a_tool_without_login_on_apple_container() {
        let _home = crate::session::test_support::isolate_app_dir();
        let profile = crate::session::config::effective_profile("");
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
            c.sandbox.container_runtime =
                crate::session::config::ContainerRuntimeName::AppleContainer;
        })
        .unwrap();
        let error = LoginHelper::start(&profile, "codex", None, false)
            .await
            .err()
            .expect("refused");
        assert!(
            matches!(error, LoginError::Other(_)),
            "not the Apple refusal: {error:?}"
        );
        assert_eq!(error.to_string(), "codex has no sandbox login");
    }

    /// A wrapper agent (own `agent_config_dir`, detected as claude) is gated on its own
    /// file: status and start read that file, and the login of that tool releases it.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_wrapper_agent_gate_status_and_login_share_one_file() {
        use crate::session::config::container_config::{sandbox_login_status, SandboxLoginStatus};

        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
            c.session
                .custom_agents
                .insert("claude-personal".to_string(), "claude".to_string());
            c.session
                .agent_detect_as
                .insert("claude-personal".to_string(), "claude".to_string());
            c.session.agent_config_dir.insert(
                "claude-personal".to_string(),
                "~/.claude-personal".to_string(),
            );
        })
        .unwrap();
        let profile = crate::session::config::effective_profile("");
        let wrapper = login_target(&profile, "claude-personal")
            .unwrap()
            .credential;
        let plain = login_target(&profile, "claude").unwrap().credential;
        assert_eq!(
            wrapper,
            home.path()
                .join(".claude-personal/sandbox-v2/.credentials.json")
        );
        assert_ne!(wrapper, plain);

        let mut session = crate::session::Instance::new("wrapper", "/tmp/aoe-login-wrapper");
        session.tool = "claude-personal".to_string();
        session.command = "claude".to_string();
        session.sandbox_info = Some(crate::session::SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-login-wrapper".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            container_workdir: None,
        });
        assert!(session.ensure_sandbox_login().is_err(), "held");
        assert_eq!(
            sandbox_login_status(&profile, "claude-personal", None),
            SandboxLoginStatus::Needed
        );

        // A usable login for plain claude does not release, nor does it answer `--check`.
        std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
        let expires_at = chrono::Utc::now().timestamp_millis() + 3_600_000;
        std::fs::write(
            &plain,
            crate::session::sandbox_login::claude::fixtures::usable(expires_at as u64),
        )
        .unwrap();
        assert_eq!(
            sandbox_login_status(&profile, "claude", None),
            SandboxLoginStatus::Ok {
                refresh_expires_at: None
            }
        );
        assert_eq!(
            sandbox_login_status(&profile, "claude-personal", None),
            SandboxLoginStatus::Needed
        );
        assert!(session.ensure_sandbox_login().is_err(), "still held");

        // The helper fills the file a start for that tool resolves.
        let fake = Fake::at(TempDir::new().unwrap(), wrapper.clone());
        let helper = fake.start(false).await.unwrap();
        bounded(helper.submit_code(code("good-code#good-state")))
            .await
            .unwrap();
        assert_eq!(
            sandbox_login_status(&profile, "claude-personal", None),
            SandboxLoginStatus::Ok {
                refresh_expires_at: None
            }
        );
        assert!(session.ensure_sandbox_login().is_ok(), "released");
        // `start` reads the wrapper's file, not plain claude's, now emptied.
        std::fs::write(&plain, "{}").unwrap();
        assert!(matches!(
            LoginHelper::start(&profile, "claude-personal", None, false).await,
            Err(LoginError::AlreadyLoggedIn)
        ));
    }

    /// `custom_agents.personal = "codex"` with `agent_command_override.personal = "claude"`
    /// launches claude: the gate, `--check`, `start` and the sweep all read that tool's file.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_command_override_target_is_the_one_the_gate_reads() {
        use crate::session::config::container_config::{sandbox_login_status, SandboxLoginStatus};

        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
            c.session
                .custom_agents
                .insert("personal".to_string(), "codex".to_string());
            c.session
                .agent_command_override
                .insert("personal".to_string(), "claude".to_string());
            c.session
                .agent_config_dir
                .insert("personal".to_string(), "~/.claude-personal".to_string());
        })
        .unwrap();
        let profile = crate::session::config::effective_profile("");
        let target = home
            .path()
            .join(".claude-personal/sandbox-v2/.credentials.json");

        // What the builder gives a session of this tool: the override as its command.
        let mut session = crate::session::Instance::new("override", "/tmp/aoe-login-override");
        session.tool = "personal".to_string();
        session.command = "claude".to_string();
        session.sandbox_info = Some(crate::session::SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-login-override".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            container_workdir: None,
        });
        assert!(session.ensure_sandbox_login().is_err(), "held");

        assert_eq!(
            login_target(&profile, "personal").map(|t| t.credential),
            Some(target.clone())
        );
        assert_eq!(
            sandbox_login_status(&profile, "personal", None),
            SandboxLoginStatus::Needed,
            "not `not applicable`"
        );

        let fake = Fake::at(TempDir::new().unwrap(), target.clone());
        let helper = fake.start(false).await.unwrap();
        bounded(helper.submit_code(code("good-code#good-state")))
            .await
            .unwrap();
        assert!(session.ensure_sandbox_login().is_ok(), "released");
        assert!(matches!(
            LoginHelper::start(&profile, "personal", None, false).await,
            Err(LoginError::AlreadyLoggedIn)
        ));
    }

    /// A tool the profile runs as Codex, with its own `agent_config_dir`, whose session was
    /// created with `command_override = "claude"`: the gate reads the file of that launch.
    /// `login --tool personal --command claude` reaches the same file, `--tool personal`
    /// alone still resolves the profile's command.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    #[serial_test::serial(sandbox_login_helper)]
    async fn sandbox_login_command_flag_targets_the_file_of_a_session_command_override() {
        use crate::session::config::container_config::{
            login_target_with_command, sandbox_login_status, SandboxLoginStatus,
        };

        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
            c.session
                .custom_agents
                .insert("personal".to_string(), "codex".to_string());
            c.session
                .agent_config_dir
                .insert("personal".to_string(), "~/.codex-personal".to_string());
        })
        .unwrap();
        let profile = crate::session::config::effective_profile("");
        let target = home
            .path()
            .join(".codex-personal/sandbox-v2/.credentials.json");

        let mut session = crate::session::Instance::new("override", "/tmp/aoe-login-command");
        session.tool = "personal".to_string();
        session.command = "claude".to_string();
        session.sandbox_info = Some(crate::session::SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-login-command".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            container_workdir: None,
        });
        let held = session.ensure_sandbox_login().unwrap_err();
        assert_eq!(held.command.as_deref(), Some("claude"));

        assert_eq!(
            login_target_with_command(&profile, "personal", Some("claude")).map(|t| t.credential),
            Some(target.clone()),
            "--command resolves the file of the gate"
        );
        assert!(
            login_target(&profile, "personal").is_none(),
            "--tool alone resolves the profile's command, Codex, which has no login"
        );
        assert_eq!(
            sandbox_login_status(&profile, "personal", Some("claude")),
            SandboxLoginStatus::Needed
        );
        assert_eq!(
            sandbox_login_status(&profile, "personal", None),
            SandboxLoginStatus::NotApplicable
        );
        assert!(matches!(
            LoginHelper::start(&profile, "personal", None, false).await,
            Err(LoginError::Other(_))
        ));

        let fake = Fake::at(TempDir::new().unwrap(), target.clone());
        let helper = fake.start(false).await.unwrap();
        bounded(helper.submit_code(code("good-code#good-state")))
            .await
            .unwrap();
        assert!(session.ensure_sandbox_login().is_ok(), "released");
        assert!(matches!(
            LoginHelper::start(&profile, "personal", Some("claude"), false).await,
            Err(LoginError::AlreadyLoggedIn)
        ));
    }
}
