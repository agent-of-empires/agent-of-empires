//! Sessions held until the sandbox login they need exists, the one push per episode and the
//! login helper the dashboard runs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::session::config::container_config::{
    login_capable_agents, login_target_with_command, sandbox_login_status, SandboxLoginStatus,
};
use crate::session::sandbox_login::helper::{LoginError, LoginHelper, Retired};
use crate::session::SandboxLoginRequired;

const LOG_TARGET: &str = "session.sandbox_login";

/// Shutdown has a 5 s window in all; these stay well inside it.
const SHUTDOWN_CANCEL_WAIT: Duration = Duration::from_millis(1500);
const SHUTDOWN_SWEEP_WAIT: Duration = Duration::from_millis(1500);

/// How many ended logins keep their outcome for a client whose answer got lost.
const RETAINED_OUTCOMES: usize = 16;

/// How long an outcome read waits for a code exchange in progress.
const OUTCOME_WAIT: Duration = Duration::from_secs(5);

/// What a login id came to, for a client that lost the answer to its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginOutcome {
    Succeeded,
    Failed,
    /// The login is still there and its code exchange has not ended.
    Pending,
    /// No exchange ended under this id, or it is no longer remembered.
    Unknown,
}

#[cfg(test)]
type TestStarter = Box<
    dyn Fn(
            &str,
            &str,
            Option<&str>,
        )
            -> futures_util::future::BoxFuture<'static, Result<Arc<LoginHelper>, LoginError>>
        + Send
        + Sync,
>;

/// Who a refused launch was for: only an unattended one notifies the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchOrigin {
    Interactive,
    Unattended,
}

/// The one notice of an episode: which session waits, and for whose login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoginRequiredNotice {
    pub session_id: String,
    pub tool: String,
}

/// Hands each notice to `handle` until `shutdown` is cancelled. The receiver must come from
/// [`SandboxLoginService::subscribe`]. Notices queue until handled, since a lost one would
/// leave its episode never pushing again.
pub(crate) async fn forward_notices<F, Fut>(
    mut notices: mpsc::UnboundedReceiver<LoginRequiredNotice>,
    shutdown: &tokio_util::sync::CancellationToken,
    mut handle: F,
) where
    F: FnMut(LoginRequiredNotice) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    loop {
        let notice = tokio::select! {
            recv = notices.recv() => match recv {
                Some(notice) => notice,
                None => return,
            },
            _ = shutdown.cancelled() => return,
        };
        handle(notice).await;
    }
}

#[derive(Debug)]
struct Hold {
    origin: LaunchOrigin,
    credential: PathBuf,
}

#[derive(Debug, Default)]
struct State {
    held: HashMap<String, Hold>,
    /// Credential files whose episode already pushed.
    pushed: HashSet<PathBuf>,
}

/// An episode of a credential file runs from the first hold waiting on it until no held
/// session waits on it any more.
pub(crate) struct SandboxLoginService {
    state: Mutex<State>,
    notices: mpsc::UnboundedSender<LoginRequiredNotice>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<LoginRequiredNotice>>>,
    /// The one helper this daemon runs. Held only briefly, so a status read never waits
    /// for a start.
    helper: Mutex<Option<Arc<LoginHelper>>>,
    /// The outcomes of the latest logins that left the slot, oldest first.
    outcomes: Mutex<VecDeque<(String, bool)>>,
    /// Serializes starts: two requests spawn one helper.
    starting: tokio::sync::Mutex<()>,
    /// Set once by `shutdown`; no helper starts or registers after it.
    shutting_down: AtomicBool,
    #[cfg(test)]
    starter: Mutex<Option<TestStarter>>,
}

impl std::fmt::Debug for SandboxLoginService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxLoginService")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Default for SandboxLoginService {
    fn default() -> Self {
        let (notices, receiver) = mpsc::unbounded_channel();
        Self {
            state: Mutex::new(State::default()),
            notices,
            receiver: Mutex::new(Some(receiver)),
            helper: Mutex::new(None),
            outcomes: Mutex::new(VecDeque::new()),
            starting: tokio::sync::Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            #[cfg(test)]
            starter: Mutex::new(None),
        }
    }
}

impl State {
    /// Ends the episode of `credential` when no held session waits on it.
    fn end_episode_if_unused(&mut self, credential: &PathBuf) {
        if !self
            .held
            .values()
            .any(|hold| &hold.credential == credential)
        {
            self.pushed.remove(credential);
        }
    }
}

impl SandboxLoginService {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The running login helper, or a fresh one for `profile` and `tool`, with the lease of
    /// the dialog joining it. `command` is the one a session's launch runs for `tool` when
    /// it overrides the profile's.
    pub(crate) async fn start_login(
        &self,
        profile: &str,
        tool: &str,
        command: Option<&str>,
        force: bool,
    ) -> Result<(Arc<LoginHelper>, String), LoginError> {
        let _starting = self.starting.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(LoginError::Cancelled);
        }
        if let Some(running) = self.running() {
            // The one helper logs one file in: another tool's file keeps it busy. Compared
            // with the helper in hand, which may finish meanwhile.
            let wanted = login_target_with_command(profile, tool, command).map(|t| t.credential);
            if wanted.as_deref() != Some(running.shared()) {
                return Err(LoginError::Busy);
            }
            if let Some(lease) = running.join() {
                return Ok((running, lease));
            }
            // Its code is being exchanged or a cancel is taking it down: wait that out, never
            // kill an exchange, and start a fresh login once it has let go of the credential.
            running.settled_outcome().await;
            if running.cancel().await == Retired::Pending {
                return Err(LoginError::Busy);
            }
            self.forget_login(running.id());
        } else {
            // A finished helper may still be recording its exchange: replacing it first
            // would lose the outcome.
            let finished = self
                .helper
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(finished) = finished {
                finished.settled_outcome().await;
            }
        }
        let helper = self.spawn_helper(profile, tool, command, force).await?;
        {
            // Under the slot lock, so a shutdown either sees this helper or this sees its flag.
            let mut slot = self.helper.lock().unwrap_or_else(PoisonError::into_inner);
            if !self.shutting_down.load(Ordering::SeqCst) {
                self.retain_outcome(slot.take());
                *slot = Some(Arc::clone(&helper));
                let lease = helper.join().ok_or(LoginError::Cancelled)?;
                return Ok((helper, lease));
            }
        }
        helper.cancel().await;
        Err(LoginError::Cancelled)
    }

    async fn spawn_helper(
        &self,
        profile: &str,
        tool: &str,
        command: Option<&str>,
        force: bool,
    ) -> Result<Arc<LoginHelper>, LoginError> {
        #[cfg(test)]
        {
            let started = self
                .starter
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(|start| start(profile, tool, command));
            if let Some(started) = started {
                return started.await;
            }
        }
        LoginHelper::start(profile, tool, command, force).await
    }

    /// The helper still waiting for its code. A finished one stays in its slot, so its
    /// outcome can be read, until `forget_login` or a new start takes it out.
    pub(crate) fn running(&self) -> Option<Arc<LoginHelper>> {
        let slot = self.helper.lock().unwrap_or_else(PoisonError::into_inner);
        slot.clone().filter(|h| !h.is_finished())
    }

    /// The running helper logging in `credential`.
    pub(crate) fn running_on(&self, credential: &Path) -> Option<Arc<LoginHelper>> {
        self.running().filter(|h| h.shared() == credential)
    }

    /// The running helper named `login_id`.
    pub(crate) fn running_login(&self, login_id: &str) -> Option<Arc<LoginHelper>> {
        self.running().filter(|h| h.id() == login_id)
    }

    /// Drops `login_id`'s helper, leaving a newer one alone.
    pub(crate) fn forget_login(&self, login_id: &str) {
        let mut slot = self.helper.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.as_ref().is_some_and(|h| h.id() == login_id) {
            self.retain_outcome(slot.take());
        }
    }

    /// Remembers the outcome of a login leaving the slot.
    fn retain_outcome(&self, helper: Option<Arc<LoginHelper>>) {
        let Some(helper) = helper else { return };
        let Some(succeeded) = helper.outcome() else {
            return;
        };
        let mut outcomes = self.outcomes.lock().unwrap_or_else(PoisonError::into_inner);
        if outcomes.len() == RETAINED_OUTCOMES {
            outcomes.pop_front();
        }
        outcomes.push_back((helper.id().to_string(), succeeded));
    }

    /// What the code exchange of `login_id` came to. An exchange in progress is waited for,
    /// within `OUTCOME_WAIT`, so a retry that finds the login gone while its exchange is
    /// still cleaning up learns how it ended.
    pub(crate) async fn outcome_of(&self, login_id: &str) -> LoginOutcome {
        let in_slot = self
            .helper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|h| h.id() == login_id);
        let known = match in_slot {
            Some(helper) => {
                match tokio::time::timeout(OUTCOME_WAIT, helper.settled_outcome()).await {
                    Ok(Some(succeeded)) => Some(succeeded),
                    // A login that ended without an exchange never will have one.
                    Ok(None) if helper.is_finished() => None,
                    Ok(None) | Err(_) => return LoginOutcome::Pending,
                }
            }
            None => self
                .outcomes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .find(|(id, _)| id == login_id)
                .map(|(_, succeeded)| *succeeded),
        };
        match known {
            Some(true) => LoginOutcome::Succeeded,
            Some(false) => LoginOutcome::Failed,
            None => LoginOutcome::Unknown,
        }
    }

    /// Cancels this daemon's helper and sweeps what it may have left in the runtime, inside
    /// a short fixed budget.
    pub(crate) async fn shutdown(&self) {
        self.shutdown_within(SHUTDOWN_CANCEL_WAIT).await;
    }

    async fn shutdown_within(&self, cancel_wait: Duration) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let helper = self
            .helper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(helper) = helper else {
            return;
        };
        if tokio::time::timeout(cancel_wait, helper.cancel())
            .await
            .is_ok_and(|retired| retired == Retired::Removed)
        {
            // The cancel removed the container by name; nothing is left to sweep.
            return;
        }
        tracing::warn!(target: LOG_TARGET, "sandbox login helper cancel timed out at shutdown");
        let sweep = async {
            // The sweep needs the helper's login lock, which its exit releases.
            helper.ended().await;
            let Ok(runtime) =
                tokio::task::spawn_blocking(crate::containers::get_container_runtime).await
            else {
                return;
            };
            sweep_shared_file(&runtime, helper.shared()).await;
        };
        if tokio::time::timeout(SHUTDOWN_SWEEP_WAIT, sweep)
            .await
            .is_err()
        {
            tracing::warn!(target: LOG_TARGET, "sandbox login helper sweep timed out at shutdown");
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_starter(
        &self,
        starter: impl Fn(
                &str,
                &str,
                Option<&str>,
            )
                -> futures_util::future::BoxFuture<'static, Result<Arc<LoginHelper>, LoginError>>
            + Send
            + Sync
            + 'static,
    ) {
        *self.starter.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(starter));
    }

    /// Holds `session_id` for the login `blocked` names. Returns whether the session was
    /// not held before. An unattended hold pushes at most once per credential file episode.
    pub(crate) fn hold(
        &self,
        session_id: &str,
        origin: LaunchOrigin,
        blocked: &SandboxLoginRequired,
    ) -> bool {
        let mut state = self.state();
        let mut moved_from = None;
        let newly_held = match state.held.get_mut(session_id) {
            Some(hold) => {
                if origin == LaunchOrigin::Unattended {
                    hold.origin = origin;
                }
                if hold.credential != blocked.credential {
                    moved_from = Some(std::mem::replace(
                        &mut hold.credential,
                        blocked.credential.clone(),
                    ));
                }
                false
            }
            None => {
                state.held.insert(
                    session_id.to_string(),
                    Hold {
                        origin,
                        credential: blocked.credential.clone(),
                    },
                );
                true
            }
        };
        // A session that now waits on another file leaves the episode of the old one.
        if let Some(old) = moved_from {
            state.end_episode_if_unused(&old);
        }
        // Under the lock, so a concurrent `release` cannot reset the episode in between.
        let push =
            origin == LaunchOrigin::Unattended && state.pushed.insert(blocked.credential.clone());
        drop(state);
        if push {
            let _ = self.notices.send(LoginRequiredNotice {
                session_id: session_id.to_string(),
                tool: blocked.tool.clone(),
            });
        }
        newly_held
    }

    pub(crate) fn is_held(&self, session_id: &str) -> bool {
        self.state().held.contains_key(session_id)
    }

    #[cfg(test)]
    pub(crate) fn held_origin(&self, session_id: &str) -> Option<LaunchOrigin> {
        self.state().held.get(session_id).map(|hold| hold.origin)
    }

    pub(crate) fn held_ids(&self) -> Vec<String> {
        self.state().held.keys().cloned().collect()
    }

    /// How many held sessions wait on `credential`.
    pub(crate) fn held_for(&self, credential: &Path) -> usize {
        self.state()
            .held
            .values()
            .filter(|hold| hold.credential == credential)
            .count()
    }

    /// Ends the episode of the session's credential file when no other held session waits
    /// on it.
    pub(crate) fn release(&self, session_id: &str) {
        let mut state = self.state();
        let Some(released) = state.held.remove(session_id) else {
            return;
        };
        state.end_episode_if_unused(&released.credential);
    }

    /// The notice stream, holding every notice since the service was made. There is one
    /// consumer, so a second call is a bug.
    pub(crate) fn subscribe(&self) -> mpsc::UnboundedReceiver<LoginRequiredNotice> {
        self.receiver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("sandbox login notices have one subscriber")
    }
}

/// Whose shared credential files a sweep covers: a profile (every agent with a sandbox login
/// and every tool with its own `agent_config_dir`), or one session's tool with its own
/// command, which can point its launch at a file the profile's tools do not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum LoginScope {
    Profile(String),
    Session {
        profile: String,
        tool: String,
        command: Option<String>,
    },
}

impl LoginScope {
    pub(crate) fn profile(profile: String) -> Self {
        Self::Profile(profile)
    }

    /// The scopes of a session: its profile and its own tool and command under it.
    pub(crate) fn of_session(instance: &crate::session::Instance) -> [Self; 2] {
        let profile = instance.effective_profile();
        [
            Self::Profile(profile.clone()),
            Self::Session {
                profile,
                tool: instance.tool.clone(),
                command: Some(instance.command.clone()).filter(|c| !c.is_empty()),
            },
        ]
    }
}

/// The distinct shared credential files of the `separate_login` scopes among `scopes`, each
/// scope resolved once, as the gate resolves its target.
fn separate_login_shared_files(scopes: impl IntoIterator<Item = LoginScope>) -> Vec<PathBuf> {
    let distinct: HashSet<LoginScope> = scopes.into_iter().collect();
    let separate = |profile: &str| {
        crate::session::config::profile_config::resolve_config_or_warn(profile)
            .sandbox
            .shared_credential
            == crate::session::config::SharedCredentialMode::SeparateLogin
    };
    let mut files: Vec<PathBuf> = distinct
        .iter()
        .flat_map(|scope| match scope {
            LoginScope::Profile(p) if separate(p) => {
                let config = crate::session::config::profile_config::resolve_config_or_warn(p);
                login_capable_agents()
                    .map(str::to_string)
                    .chain(config.session.agent_config_dir.keys().cloned())
                    .filter_map(|tool| {
                        login_target_with_command(p, &tool, None).map(|target| target.credential)
                    })
                    .collect()
            }
            LoginScope::Session {
                profile,
                tool,
                command,
            } if separate(profile) => login_target_with_command(profile, tool, command.as_deref())
                .map(|target| target.credential)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Removes login helpers a killed daemon or CLI left behind, for every `separate_login`
/// shared file among `scopes`. A file whose login is running is left alone.
pub(crate) async fn sweep_stale_login_helpers(scopes: Vec<LoginScope>) {
    let Ok((files, runtime)) = tokio::task::spawn_blocking(move || {
        let files = separate_login_shared_files(scopes);
        let runtime = (!files.is_empty()).then(crate::containers::get_container_runtime);
        (files, runtime)
    })
    .await
    else {
        return;
    };
    let Some(runtime) = runtime else {
        return;
    };
    for file in files {
        sweep_shared_file(&runtime, &file).await;
    }
}

async fn sweep_shared_file(runtime: &crate::containers::ContainerRuntime, file: &Path) {
    match crate::session::sandbox_login::helper::sweep_stale_helpers(runtime, file).await {
        Ok(()) => {}
        Err(LoginError::Busy) => {
            tracing::debug!(target: LOG_TARGET, "sandbox login running, stale sweep skipped");
        }
        Err(e) => {
            tracing::warn!(target: LOG_TARGET, "sandbox login helper stale sweep failed: {e}");
        }
    }
}

/// Logs, per agent with a sandbox login, whether `profile`'s sandboxes need it; silent when
/// not applicable.
pub(crate) async fn log_login_status(profile: String) {
    let Ok(statuses) = tokio::task::spawn_blocking(move || {
        login_capable_agents()
            .map(|tool| (tool, sandbox_login_status(&profile, tool, None)))
            .collect::<Vec<_>>()
    })
    .await
    else {
        return;
    };
    for (tool, status) in statuses {
        let status_tag = match status {
            SandboxLoginStatus::NotApplicable => continue,
            SandboxLoginStatus::Unsupported => {
                tracing::warn!(
                    target: LOG_TARGET,
                    tool,
                    "{}",
                    crate::session::SandboxLoginUnsupported
                );
                continue;
            }
            SandboxLoginStatus::Needed => "needed",
            SandboxLoginStatus::Ok { .. } => "ok",
        };
        tracing::info!(target: LOG_TARGET, tool, status = ?status_tag, "Sandbox login status");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;

    /// A service whose helper is a fake process; `gate` holds each start until released and
    /// `entered` is told when one begins.
    #[cfg(unix)]
    fn fake_service(
        fake: &Arc<crate::session::sandbox_login::test_fake::Fake>,
        gate: Option<Arc<tokio::sync::Notify>>,
        entered: Arc<tokio::sync::Notify>,
    ) -> SandboxLoginService {
        use crate::session::sandbox_login::helper::HelperTimeouts;
        let service = SandboxLoginService::default();
        let fake = Arc::clone(fake);
        service.set_test_starter(move |_profile, _tool, _command| {
            let (fake, gate, entered) = (Arc::clone(&fake), gate.clone(), Arc::clone(&entered));
            Box::pin(async move {
                entered.notify_one();
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                fake.start_timed(true, HelperTimeouts::default()).await
            })
        });
        service
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_service_a_helper_still_ending_keeps_its_slot_and_blocks_a_new_start() {
        use crate::session::sandbox_login::helper::HelperTimeouts;
        let _home = crate::session::test_support::isolate_app_dir();
        // The file the login of `claude` resolves to, or a start would be `Busy` as another
        // tool's.
        let shared = login_target_with_command("default", "claude", None)
            .unwrap()
            .credential;
        let fake = Arc::new(crate::session::sandbox_login::test_fake::Fake::at(
            tempfile::TempDir::new().unwrap(),
            shared,
        ));
        let service = SandboxLoginService::default();
        let starter = Arc::clone(&fake);
        service.set_test_starter(move |_profile, _tool, _command| {
            let fake = Arc::clone(&starter);
            Box::pin(async move {
                LoginHelper::start_with(
                    &crate::session::sandbox_login::claude::LOGIN,
                    fake.argv(true),
                    fake.shared.clone(),
                    // The removal outlasts the cancel's wait.
                    Some(vec!["sleep".into(), "1".into()]),
                    HelperTimeouts {
                        retire: Duration::from_millis(200),
                        ..HelperTimeouts::default()
                    },
                )
                .await
            })
        });
        let (helper, lease) = service
            .start_login("default", "claude", None, false)
            .await
            .unwrap();
        let (joined, other) = service
            .start_login("default", "claude", None, false)
            .await
            .unwrap();
        assert_eq!(joined.id(), helper.id(), "a second dialog joins");
        assert_eq!(helper.release(Some(&other)).await, None);
        assert_eq!(helper.release(Some(&lease)).await, Some(Retired::Pending));

        let again = service.start_login("default", "claude", None, false).await;
        assert!(matches!(again, Err(LoginError::Busy)));
        assert!(
            service.running_login(helper.id()).is_some(),
            "the retiring owner is not forgotten"
        );

        // Once it let go, the next start replaces it.
        tokio::time::timeout(Duration::from_secs(10), helper.ended())
            .await
            .unwrap();
        let (fresh, _) = service
            .start_login("default", "claude", None, false)
            .await
            .unwrap();
        assert_ne!(fresh.id(), helper.id());
        fresh.cancel().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_service_shutdown_cancels_the_helper_and_skips_the_redundant_sweep() {
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, calls) =
            crate::session::test_support::probing_container_runtime(home.path());
        let fake = Arc::new(crate::session::sandbox_login::test_fake::Fake::new());
        let service = fake_service(&fake, None, Arc::default());

        let (helper, _lease) = service
            .start_login("default", "claude", None, false)
            .await
            .unwrap();
        assert!(!helper.is_finished());
        tokio::time::timeout(Duration::from_secs(10), service.shutdown())
            .await
            .expect("shutdown stays inside its budget");

        assert!(helper.is_finished(), "the helper was cancelled");
        assert!(fake.cleaned().exists(), "its container cleanup ran");
        assert!(service.running().is_none());
        let runtime_calls = std::fs::read_to_string(calls).unwrap_or_default();
        assert!(
            !runtime_calls.contains("ps -aq"),
            "a completed cancel leaves nothing to sweep: {runtime_calls}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_service_shutdown_sweeps_when_the_removal_failed() {
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, calls) =
            crate::session::test_support::probing_container_runtime(home.path());
        let fake = Arc::new(crate::session::sandbox_login::test_fake::Fake::new());
        let service = SandboxLoginService::default();
        let starter = Arc::clone(&fake);
        service.set_test_starter(move |_profile, _tool, _command| {
            let fake = Arc::clone(&starter);
            Box::pin(async move {
                LoginHelper::start_with(
                    &crate::session::sandbox_login::claude::LOGIN,
                    fake.argv(true),
                    fake.shared.clone(),
                    Some(vec!["false".into()]),
                    crate::session::sandbox_login::helper::HelperTimeouts::default(),
                )
                .await
            })
        });
        let (helper, _lease) = service
            .start_login("default", "claude", None, false)
            .await
            .unwrap();

        service.shutdown().await;

        assert!(helper.is_finished(), "the client ended");
        let runtime_calls = std::fs::read_to_string(calls).unwrap_or_default();
        assert!(
            runtime_calls.contains("ps -aq --filter label="),
            "a finished helper whose container removal failed is swept: {runtime_calls}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_service_shutdown_sweeps_when_the_cancel_times_out() {
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, calls) =
            crate::session::test_support::probing_container_runtime(home.path());
        let fake = Arc::new(crate::session::sandbox_login::test_fake::Fake::new());
        let service = fake_service(&fake, None, Arc::default());
        let _helper = service
            .start_login("default", "claude", None, false)
            .await
            .unwrap();

        service.shutdown_within(Duration::ZERO).await;

        let runtime_calls = std::fs::read_to_string(calls).unwrap_or_default();
        assert!(
            runtime_calls.contains("ps -aq --filter label="),
            "the helper's shared file was swept: {runtime_calls}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_service_shutdown_beats_a_start_in_flight() {
        let _home = crate::session::test_support::isolate_app_dir();
        let fake = Arc::new(crate::session::sandbox_login::test_fake::Fake::new());
        let gate = Arc::new(tokio::sync::Notify::new());
        let entered = Arc::new(tokio::sync::Notify::new());
        let service = Arc::new(fake_service(
            &fake,
            Some(Arc::clone(&gate)),
            Arc::clone(&entered),
        ));

        let starting = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.start_login("default", "claude", None, false).await }
        });
        entered.notified().await;
        service.shutdown().await;
        gate.notify_one();
        let started = tokio::time::timeout(Duration::from_secs(10), starting)
            .await
            .expect("the start ends")
            .unwrap();

        assert!(
            matches!(started, Err(LoginError::Cancelled)),
            "a start that finishes after shutdown is refused"
        );
        assert!(
            service.running().is_none(),
            "no helper registered after shutdown"
        );
        assert!(
            fake.cleaned().exists(),
            "the late helper was cancelled and cleaned up"
        );
        assert!(
            matches!(
                service.start_login("default", "claude", None, false).await,
                Err(LoginError::Cancelled)
            ),
            "and no later start gets through"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_service_forget_leaves_a_newer_helper_alone() {
        let _home = crate::session::test_support::isolate_app_dir();
        let fake = Arc::new(crate::session::sandbox_login::test_fake::Fake::new());
        let service = fake_service(&fake, None, Arc::default());

        let (old, _) = service
            .start_login("default", "claude", None, false)
            .await
            .unwrap();
        let old_id = old.id().to_string();
        old.cancel().await;
        let (newer, _) = service
            .start_login("default", "claude", None, false)
            .await
            .unwrap();
        assert_ne!(old_id, newer.id());

        service.forget_login(&old_id);
        assert!(service.running_login(newer.id()).is_some());
        service.forget_login(newer.id());
        assert!(service.running().is_none());
    }

    #[tokio::test]
    async fn sandbox_login_service_shutdown_without_a_helper_does_nothing() {
        tokio::time::timeout(
            Duration::from_secs(1),
            SandboxLoginService::default().shutdown(),
        )
        .await
        .expect("nothing to cancel, nothing to wait for");
    }

    fn blocked(tool: &str, credential: &str) -> SandboxLoginRequired {
        SandboxLoginRequired {
            tool: tool.to_string(),
            credential: PathBuf::from(credential),
            command: None,
            profile: None,
        }
    }

    fn notice(session_id: &str, tool: &str) -> LoginRequiredNotice {
        LoginRequiredNotice {
            session_id: session_id.to_string(),
            tool: tool.to_string(),
        }
    }

    /// A hold that runs while the consumer's task is not started yet (later startup work is
    /// still pending) reaches the consumer, because the receiver was subscribed first.
    #[tokio::test]
    async fn a_hold_before_the_consumer_runs_still_reaches_it() {
        let service = SandboxLoginService::default();
        let notices = service.subscribe();
        let main = "/h/.claude/sandbox-v2/.credentials.json";
        assert!(service.hold("a", LaunchOrigin::Unattended, &blocked("claude", main)));

        let shutdown = tokio_util::sync::CancellationToken::new();
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let consumer = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                forward_notices(notices, &shutdown, |notice| {
                    let seen_tx = seen_tx.clone();
                    async move { seen_tx.send(notice).unwrap() }
                })
                .await;
            }
        });
        assert_eq!(seen.recv().await, Some(notice("a", "claude")));
        shutdown.cancel();
        consumer.await.unwrap();
    }

    /// More notices than a bounded buffer holds queue up while the consumer is busy, and none
    /// is lost or repeated.
    #[tokio::test]
    async fn a_burst_of_holds_before_the_consumer_drains_loses_no_notice() {
        const CREDENTIALS: usize = 40;
        let service = SandboxLoginService::default();
        let notices = service.subscribe();
        let credential = |i: usize| format!("/h/.claude-{i}/sandbox-v2/.credentials.json");
        for i in 0..CREDENTIALS {
            assert!(service.hold(
                &format!("s{i}"),
                LaunchOrigin::Unattended,
                &blocked("claude", &credential(i))
            ));
        }

        let shutdown = tokio_util::sync::CancellationToken::new();
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let consumer = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                forward_notices(notices, &shutdown, |notice| {
                    let seen_tx = seen_tx.clone();
                    async move { seen_tx.send(notice).unwrap() }
                })
                .await;
            }
        });
        for i in 0..CREDENTIALS {
            assert_eq!(seen.recv().await, Some(notice(&format!("s{i}"), "claude")));
        }

        // The episodes are still running: re-holding pushes nothing.
        for i in 0..CREDENTIALS {
            assert!(!service.hold(
                &format!("s{i}"),
                LaunchOrigin::Unattended,
                &blocked("claude", &credential(i))
            ));
        }
        // Dropping the only sender lets the consumer drain what is queued before it stops.
        drop(service);
        consumer.await.unwrap();
        assert_eq!(seen.recv().await, None, "no duplicate notice");
    }

    /// A subscription made after the hold still sees its notice, once.
    #[test]
    fn a_subscription_made_after_the_hold_still_sees_its_notice() {
        let service = SandboxLoginService::default();
        let main = "/h/.claude/sandbox-v2/.credentials.json";
        assert!(service.hold("a", LaunchOrigin::Unattended, &blocked("claude", main)));

        let mut late = service.subscribe();
        assert_eq!(late.try_recv(), Ok(notice("a", "claude")));
        assert!(!service.hold("a", LaunchOrigin::Unattended, &blocked("claude", main)));
        assert_eq!(late.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn sandbox_login_episodes_are_keyed_by_credential_file() {
        let service = SandboxLoginService::default();
        let mut notices = service.subscribe();
        let main = "/h/.claude/sandbox-v2/.credentials.json";
        let work = "/h/.claude-work/sandbox-v2/.credentials.json";

        // Two sessions of the same tool waiting on different files: one push each.
        assert!(service.hold("a", LaunchOrigin::Unattended, &blocked("claude", main)));
        assert_eq!(notices.try_recv(), Ok(notice("a", "claude")));
        assert!(service.hold("w", LaunchOrigin::Unattended, &blocked("claude", work)));
        assert_eq!(notices.try_recv(), Ok(notice("w", "claude")));

        // Two tools sharing one file: the second hold is the same episode.
        assert!(service.hold("b", LaunchOrigin::Unattended, &blocked("wrapper", main)));
        assert_eq!(notices.try_recv(), Err(TryRecvError::Empty));
        assert!(!service.hold("a", LaunchOrigin::Unattended, &blocked("claude", main)));
        assert_eq!(notices.try_recv(), Err(TryRecvError::Empty));

        // An interactive hold never pushes and does not start an episode.
        let fresh = SandboxLoginService::default();
        let mut fresh_notices = fresh.subscribe();
        assert!(fresh.hold("c", LaunchOrigin::Interactive, &blocked("claude", main)));
        assert_eq!(fresh_notices.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(fresh.held_origin("c"), Some(LaunchOrigin::Interactive));
        assert!(fresh.hold("d", LaunchOrigin::Unattended, &blocked("claude", main)));
        assert_eq!(fresh_notices.try_recv(), Ok(notice("d", "claude")));

        // The episode of a file lasts while any session waits on it.
        service.release("a");
        assert!(service.hold("e", LaunchOrigin::Unattended, &blocked("claude", main)));
        assert_eq!(notices.try_recv(), Err(TryRecvError::Empty));
        service.release("e");
        service.release("b");
        assert!(service.hold("f", LaunchOrigin::Unattended, &blocked("claude", main)));
        assert_eq!(
            notices.try_recv(),
            Ok(notice("f", "claude")),
            "episode ended"
        );

        // A session re-held on another file leaves the old file's episode: the next hold
        // on the old file pushes again.
        service.release("f");
        assert!(service.hold("g", LaunchOrigin::Unattended, &blocked("claude", main)));
        assert_eq!(notices.try_recv(), Ok(notice("g", "claude")));
        assert!(!service.hold("g", LaunchOrigin::Unattended, &blocked("claude", work)));
        assert_eq!(
            notices.try_recv(),
            Err(TryRecvError::Empty),
            "work's episode runs"
        );
        assert!(service.hold("h", LaunchOrigin::Unattended, &blocked("claude", main)));
        assert_eq!(
            notices.try_recv(),
            Ok(notice("h", "claude")),
            "the old file's episode ended with the move"
        );
        service.release("g");
        service.release("h");
        service.release("w");
        assert!(service.held_ids().is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_login_startup_sweep_targets_only_separate_login_files() {
        let home = crate::session::test_support::isolate_app_dir();
        let profiles = || {
            vec![
                LoginScope::profile("default".to_string()),
                LoginScope::profile("default".to_string()),
            ]
        };
        assert!(
            separate_login_shared_files(profiles()).is_empty(),
            "host_seed"
        );

        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential =
                crate::session::config::SharedCredentialMode::SeparateLogin;
        })
        .unwrap();
        assert_eq!(
            separate_login_shared_files(profiles()),
            vec![home.path().join(".claude/sandbox-v2/.credentials.json")]
        );

        crate::session::config::update_config(|c| {
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
        assert_eq!(
            separate_login_shared_files(profiles()),
            vec![
                home.path().join(".claude/sandbox-v2/.credentials.json"),
                home.path()
                    .join(".claude-personal/sandbox-v2/.credentials.json"),
            ],
            "a wrapper agent's file is swept too"
        );

        // `personal` launches claude through the override, not the custom agent's codex.
        crate::session::config::update_config(|c| {
            c.session
                .custom_agents
                .insert("personal".to_string(), "codex".to_string());
            c.session
                .agent_command_override
                .insert("personal".to_string(), "claude".to_string());
            c.session
                .agent_config_dir
                .insert("personal".to_string(), "~/.claude-override".to_string());
        })
        .unwrap();
        assert_eq!(
            separate_login_shared_files(profiles()),
            vec![
                home.path().join(".claude/sandbox-v2/.credentials.json"),
                home.path()
                    .join(".claude-override/sandbox-v2/.credentials.json"),
                home.path()
                    .join(".claude-personal/sandbox-v2/.credentials.json"),
            ],
            "the sweep follows the launch command"
        );
    }

    /// A session's own command points its gate at a file the profile's tools do not resolve;
    /// the sweep covers the file the gate waits for, resolving each `(profile, tool,
    /// command)` once.
    #[test]
    #[serial_test::serial]
    fn sandbox_login_startup_sweep_covers_the_file_a_session_is_gated_on() {
        let home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential =
                crate::session::config::SharedCredentialMode::SeparateLogin;
            c.session
                .custom_agents
                .insert("personal".to_string(), "codex".to_string());
        })
        .unwrap();
        let plain = home.path().join(".claude/sandbox-v2/.credentials.json");
        let profile = crate::session::config::effective_profile("");
        let session = |command: &str| {
            let mut session = crate::session::Instance::new("sweep", "/tmp/aoe-sweep-none");
            session.tool = "personal".to_string();
            session.command = command.to_string();
            session
        };

        let codex = session("");
        assert_eq!(
            separate_login_shared_files(LoginScope::of_session(&codex)),
            vec![plain.clone()],
            "codex has no login: only the profile's login agents are covered"
        );

        // A session command of its own is resolved as its gate does: here into a login
        // agent whose directory only the profile declares.
        crate::session::config::update_config(|c| {
            c.session
                .agent_config_dir
                .insert("personal".to_string(), "~/.claude-personal".to_string());
        })
        .unwrap();
        let personal = home
            .path()
            .join(".claude-personal/sandbox-v2/.credentials.json");
        let overridden = session("claude");
        assert_eq!(
            separate_login_shared_files(LoginScope::of_session(&overridden)),
            vec![plain.clone(), personal.clone()]
        );
        assert_eq!(
            separate_login_shared_files([LoginScope::profile(profile)]),
            vec![plain],
            "the profile alone does not reach it"
        );
    }

    /// A session adds its profile scope next to its own `(profile, tool, command)` one, and
    /// sessions sharing the three resolve once.
    #[test]
    fn sandbox_login_scope_of_a_session_adds_its_profile_and_dedupes() {
        let session = |tool: &str, command: &str| {
            let mut session = crate::session::Instance::new("scope", "/tmp/aoe-scope");
            session.source_profile = "work".to_string();
            session.tool = tool.to_string();
            session.command = command.to_string();
            session
        };
        let scopes = LoginScope::of_session(&session("personal", "claude"));
        assert!(scopes.contains(&LoginScope::Profile("work".to_string())));
        assert!(scopes.contains(&LoginScope::Session {
            profile: "work".to_string(),
            tool: "personal".to_string(),
            command: Some("claude".to_string()),
        }));
        assert_eq!(
            LoginScope::of_session(&session("personal", ""))[1],
            LoginScope::Session {
                profile: "work".to_string(),
                tool: "personal".to_string(),
                command: None,
            },
            "no command of its own is no command"
        );

        let distinct: std::collections::HashSet<LoginScope> = [
            session("personal", "claude"),
            session("personal", "claude"),
            session("other", "claude"),
        ]
        .iter()
        .flat_map(LoginScope::of_session)
        .collect();
        assert_eq!(
            distinct.len(),
            3,
            "one profile scope, one per distinct (profile, tool, command)"
        );
    }
}
