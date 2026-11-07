//! Sessions held until the sandbox login they need exists, and the one push per episode.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use tokio::sync::broadcast;

use crate::session::config::container_config::{
    login_capable_agents, sandbox_login_status, SandboxLoginStatus,
};
use crate::session::SandboxLoginRequired;

const LOG_TARGET: &str = "session.sandbox_login";

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
#[derive(Debug)]
pub(crate) struct SandboxLoginService {
    state: Mutex<State>,
    notices: broadcast::Sender<LoginRequiredNotice>,
}

impl Default for SandboxLoginService {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            notices: broadcast::channel(16).0,
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

    /// Ends the episode of the session's credential file when no other held session waits
    /// on it.
    pub(crate) fn release(&self, session_id: &str) {
        let mut state = self.state();
        let Some(released) = state.held.remove(session_id) else {
            return;
        };
        state.end_episode_if_unused(&released.credential);
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<LoginRequiredNotice> {
        self.notices.subscribe()
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
    use crate::session::config::container_config::login_target_with_command;
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
        match crate::session::sandbox_login::helper::sweep_stale_helpers(&runtime, &file).await {
            Ok(()) => {}
            Err(crate::session::sandbox_login::helper::LoginError::Busy) => {
                tracing::debug!(target: LOG_TARGET, "sandbox login running, stale sweep skipped");
            }
            Err(e) => {
                tracing::warn!(target: LOG_TARGET, "sandbox login helper stale sweep failed: {e}");
            }
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
    use tokio::sync::broadcast::error::TryRecvError;

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
