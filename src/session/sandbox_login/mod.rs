//! The sandbox login: how an agent whose sandboxes share one rotating credential gets that
//! credential without reading the host's.
//!
//! An agent opts in by declaring an [`AgentLogin`] on its config mount. Everything native to
//! the agent (credential format, login command, its output) stays behind that descriptor;
//! the helper container, the gate and the CLI only call through it.

use crate::containers::EnvEntry;

pub(crate) mod claude;
pub(crate) mod code;
pub(crate) mod helper;

/// What a credential file holds, as the agent's integration reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialState {
    /// A login the sandboxes can use; `refresh_expires_at` (ms since the epoch) when the
    /// agent records one.
    Usable { refresh_expires_at: Option<u64> },
    /// Absent, empty, signed out or unreadable.
    Unusable,
}

/// The auth route one launch runs with: the environment its sandbox starts with, which
/// already carries the session's provider pick.
#[derive(Debug, Default)]
pub(crate) struct LaunchAuth {
    env: Vec<(String, String)>,
}

impl LaunchAuth {
    pub(crate) fn from_env(entries: &[EnvEntry]) -> Self {
        Self {
            env: entries
                .iter()
                .map(|entry| (entry.key().to_string(), entry.value().to_string()))
                .collect(),
        }
    }

    /// The value of `key` when the launch sets it to something.
    pub(crate) fn var(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }
}

/// A file the login helper sees in its `HOME`, rewritten before every run.
#[derive(Debug)]
pub(crate) struct HomeSeed {
    /// Relative to the helper's `HOME`.
    pub path: &'static str,
    pub content: &'static str,
}

/// The login integration of one agent. Only the agent's own module builds one.
pub(crate) struct AgentLogin {
    /// The command run in the helper container.
    pub argv: &'static [&'static str],
    /// Mounted privately into the helper so the client skips its first-run flow.
    pub home_seed: HomeSeed,
    /// The expiry (ms since the epoch) of a credential a container could use, `None` when the
    /// content is not one. The second argument is the current time in ms. Seeding folds the
    /// freshest credential by it.
    pub credential_expires_at: fn(&str, u64) -> Option<u64>,
    /// `winner` with what `existing` holds beside the credential kept, so seeding does not
    /// drop what the agent stored in the file.
    pub merge_credential: fn(Option<&str>, &str) -> String,
    /// Reads the native credential file.
    pub credential_state: fn(&str) -> CredentialState,
    /// Whether the launch uses the shared credential at all: another auth route (a cloud
    /// provider, an API key) does not, so the launch is not held for the login.
    pub needs_credential: fn(&LaunchAuth) -> bool,
    /// The sign-in link in the client's output so far.
    pub login_url: fn(&str) -> Option<String>,
    /// The part of the client's output worth showing after a failure, with `secrets`
    /// redacted.
    pub failure_message: fn(&str, &[&str]) -> String,
    /// Whether the output shows the client rejected a code and kept running.
    pub rejected_code: fn(&str) -> bool,
}

impl std::fmt::Debug for AgentLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLogin")
            .field("argv", &self.argv)
            .finish()
    }
}

/// Milliseconds since the epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl AgentLogin {
    pub(crate) fn state(&self, content: Option<&str>) -> CredentialState {
        content.map_or(CredentialState::Unusable, |c| (self.credential_state)(c))
    }

    pub(crate) fn usable(&self, content: Option<&str>) -> bool {
        matches!(self.state(content), CredentialState::Usable { .. })
    }
}

/// A second integration with nothing in common with Claude's: another command, credential
/// format, output and auth route. The generic gate, seeding and helper run through it in
/// their tests without touching `claude.rs`.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    pub(crate) const CREDENTIAL_FILE: &str = "auth.token";

    /// `TOKEN:<expires_at>` is a login; anything else is none.
    fn credential_expires_at(content: &str, _now_ms: u64) -> Option<u64> {
        content.strip_prefix("TOKEN:")?.trim().parse().ok()
    }

    pub(crate) static LOGIN: AgentLogin = AgentLogin {
        argv: &["fake-agent", "signin", "--device"],
        home_seed: HomeSeed {
            path: ".fake-agent/seed.txt",
            content: "seeded",
        },
        credential_expires_at,
        merge_credential: |_existing, winner| winner.to_string(),
        credential_state: |content| match credential_expires_at(content, 0) {
            Some(expires) => CredentialState::Usable {
                refresh_expires_at: Some(expires),
            },
            None => CredentialState::Unusable,
        },
        needs_credential: |auth| auth.var("FAKE_AGENT_API_KEY").is_none(),
        login_url: |output| {
            output
                .lines()
                .find_map(|line| line.strip_prefix("SIGNIN "))
                .map(str::to_string)
        },
        failure_message: |output, _secrets| output.trim().to_string(),
        rejected_code: |output| output.contains("BAD CODE"),
    };
}
