//! The `Status` enum, its wire forms, and the passive-status patch peers apply to a row.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Waiting,
    #[default]
    Idle,
    Unknown,
    Stopped,
    Error,
    Starting,
    Deleting,
    Creating,
}

impl Status {
    const ALL: [(Status, &'static str, &'static str); 9] = [
        (Status::Running, "running", "Running"),
        (Status::Waiting, "waiting", "Waiting"),
        (Status::Idle, "idle", "Idle"),
        (Status::Unknown, "unknown", "Unknown"),
        (Status::Stopped, "stopped", "Stopped"),
        (Status::Error, "error", "Error"),
        (Status::Starting, "starting", "Starting"),
        (Status::Deleting, "deleting", "Deleting"),
        (Status::Creating, "creating", "Creating"),
    ];

    fn spellings(self) -> (&'static str, &'static str) {
        let (_, lower, pascal) = Self::ALL[self as usize];
        (lower, pascal)
    }

    /// Lowercase CLI/hook form.
    pub fn as_str(self) -> &'static str {
        self.spellings().0
    }

    /// PascalCase HTTP API form, spelled out so a variant rename cannot change the API.
    pub fn wire_str(self) -> &'static str {
        self.spellings().1
    }

    /// Parses [`Self::wire_str`]; `None` for anything unrecognized, such as a newer daemon's status.
    pub fn from_api_str(s: &str) -> Option<Status> {
        Self::ALL
            .iter()
            .find(|(_, _, pascal)| *pascal == s)
            .map(|(status, _, _)| *status)
    }

    /// Whether the worktree may be mid-use, so an in-place worktree edit must wait.
    pub fn blocks_worktree_edit(self) -> bool {
        matches!(
            self,
            Status::Running
                | Status::Waiting
                | Status::Starting
                | Status::Creating
                | Status::Deleting
        )
    }
}

/// `last_error` for a confirmed-absent pane; rendered as the calm "Stopped" case.
pub const TMUX_SESSION_GONE_ERROR: &str =
    "tmux session is gone. The agent process may have exited or been killed.";

/// `last_error` for a sustained unreachable tmux server; not evidence the pane is gone.
pub const TMUX_SERVER_UNREACHABLE_ERROR: &str =
    "tmux server could not be reached. It may be busy or have crashed.";

/// `Unknown` tolerance for a session never confirmed alive; nothing can be blipping.
pub(super) const UNKNOWN_ERROR_WINDOW_NEVER_PRESENT: std::time::Duration =
    std::time::Duration::from_secs(4);

/// `Unknown` tolerance for a confirmed-alive session, above the ~11s blips seen in production.
pub(super) const UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// A passively detected status transition queued for a batched disk write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PassiveStatusPatch {
    pub status: Status,
    pub lifecycle_generation: u64,
    pub idle_entered_at: Option<DateTime<Utc>>,
    /// Stays `None` for a never-touched row; idle-reap and freshness sort rely on it.
    pub last_accessed_at: Option<DateTime<Utc>>,
}

impl PassiveStatusPatch {
    pub(crate) fn from_instance(inst: &Instance) -> Self {
        Self {
            status: inst.status,
            lifecycle_generation: inst.lifecycle_generation,
            idle_entered_at: inst.idle_entered_at,
            last_accessed_at: inst.last_accessed_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_spellings_match_serde_and_debug() {
        for (status, _, _) in Status::ALL {
            let wire = format!("{status:?}");
            assert_eq!(status.wire_str(), wire);
            assert_eq!(Status::from_api_str(&wire), Some(status));
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{}\"", status.as_str())
            );
            assert_eq!(
                serde_json::from_str::<Status>(&format!("\"{}\"", status.as_str())).unwrap(),
                status
            );
            assert_eq!(Status::from_api_str(status.as_str()), None);
        }
        assert_eq!(Status::from_api_str(""), None);
        assert_eq!(Status::from_api_str("Hibernating"), None);
    }
}
