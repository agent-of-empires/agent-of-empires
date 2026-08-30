//! The `Status` enum, its wire form, and the passive-status patch that
//! peers apply to a row.

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
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Idle => "idle",
            Status::Unknown => "unknown",
            Status::Stopped => "stopped",
            Status::Error => "error",
            Status::Starting => "starting",
            Status::Deleting => "deleting",
            Status::Creating => "creating",
        }
    }

    /// Wire form for the HTTP API's `status` field: PascalCase, matching what
    /// `SessionResponse` has emitted since the endpoint shipped (the web
    /// dashboard and existing tests both key on it). Distinct from
    /// [`Status::as_str`], which is the lowercase CLI/hook form.
    ///
    /// Spelled out rather than leaning on `format!("{:?}")` so renaming a
    /// variant cannot silently change the public API;
    /// `status_api_wire_form_round_trips` pins the two together. See #3187.
    pub fn wire_str(self) -> &'static str {
        match self {
            Status::Running => "Running",
            Status::Waiting => "Waiting",
            Status::Idle => "Idle",
            Status::Unknown => "Unknown",
            Status::Stopped => "Stopped",
            Status::Error => "Error",
            Status::Starting => "Starting",
            Status::Deleting => "Deleting",
            Status::Creating => "Creating",
        }
    }

    /// Parse the form `/api/sessions` puts on the wire. That endpoint
    /// serializes with `format!("{:?}", inst.status)`, not serde, so the
    /// variant names are `CamelCase` rather than the `lowercase` rename
    /// `as_str` and `Deserialize` use. Kept next to `as_str` so both
    /// spellings of the same enum are read together;
    /// `status_api_wire_form_round_trips` locks the pairing against the
    /// server's formatter.
    ///
    /// `None` for anything unrecognized, which is how a newer daemon
    /// reaches an older client: the caller leaves the row's status alone
    /// rather than inventing one.
    pub fn from_api_str(s: &str) -> Option<Status> {
        match s {
            "Running" => Some(Status::Running),
            "Waiting" => Some(Status::Waiting),
            "Idle" => Some(Status::Idle),
            "Unknown" => Some(Status::Unknown),
            "Stopped" => Some(Status::Stopped),
            "Error" => Some(Status::Error),
            "Starting" => Some(Status::Starting),
            "Deleting" => Some(Status::Deleting),
            "Creating" => Some(Status::Creating),
            _ => None,
        }
    }

    /// Whether this status blocks an in-place worktree edit (move dir /
    /// rename branch). The worktree's checkout must be quiescent: an
    /// actively running agent, a session mid-start, or one being
    /// created/deleted can hold the directory or race the metadata write.
    /// Idle/Stopped/Error/Unknown sessions are safe to edit.
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

/// `last_error` the status poller stamps when a session's tmux pane is simply
/// absent (killed, exited, server reboot) and nothing more specific was
/// captured from the pane. The preview treats this as the calm "Stopped" case
/// rather than a red crash error, since it carries no diagnostic detail.
pub const TMUX_SESSION_GONE_ERROR: &str =
    "tmux session is gone. The agent process may have exited or been killed.";

/// `last_error` the status poller stamps when the tmux server itself could
/// not be reached for a sustained period (past `UNKNOWN_ERROR_WINDOW_*`),
/// as distinct from `TMUX_SESSION_GONE_ERROR`'s "session confirmed absent"
/// case. This is a connectivity failure, not evidence the session's pane
/// was actually torn down, so consumers that treat `TMUX_SESSION_GONE_ERROR`
/// as the calm "Stopped" case must not conflate the two.
pub const TMUX_SERVER_UNREACHABLE_ERROR: &str =
    "tmux server could not be reached. It may be busy or have crashed.";

/// How long a session that has never once been confirmed alive
/// (`Instance::ever_confirmed_present == false`) tolerates a continuous
/// `tmux::SessionExistence::Unknown` before `update_status_with_metadata_inner`
/// latches `Status::Error`. There is nothing that could be "blipping" for a
/// session nobody has ever seen alive (e.g. `aoe add` without `--launch`, or
/// a row whose tmux session failed to spawn), so this stays close to the
/// pre-fix immediate-Error behavior rather than the long grace period below;
/// a couple of `status_poll_loop` ticks (2s each) is enough to smooth over
/// boot jitter without stalling the case a genuinely-dead server needs to
/// surface quickly (see `web/tests/live/ensure-session-restart.spec.ts`,
/// which waits up to 10s for exactly this transition).
pub(super) const UNKNOWN_ERROR_WINDOW_NEVER_PRESENT: std::time::Duration =
    std::time::Duration::from_secs(4);

/// How long a session that HAS been confirmed alive tolerates a continuous
/// `tmux::SessionExistence::Unknown` before latching `Status::Error`. Sized
/// with real margin over the ~11s max tmux-server-unreachable blip observed
/// in production debug logs, so a transient hiccup on an actually-running
/// session never trips a false Error.
pub(super) const UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// A passively-detected status transition, queued for a batched disk write.
/// Produced by the TUI's and daemon's background pollers when a genuine
/// live status change is observed (see [`Instance::update_status_with_metadata`]
/// and its `live_status_baseline` field), consumed by
/// [`Instance::merge_passive_status_patch`]. `pub(crate)`: this is an
/// internal wire format between the pollers and `merge_passive_status_patch`,
/// not a stable type for out-of-tree consumers.
///
/// ## Poller vocabulary (#2690 follow-up)
///
/// - **passive status**: a status transition detected by a background
///   poller from tmux pane state or ACP overlay, not by an explicit user
///   action.
/// - **passive status patch**: a minimal `PassiveStatusPatch` carrying
///   the `status` / `idle_entered_at` writes plus the monotone
///   `last_accessed_at` carry-through (user-gesture-only since #3465),
///   applied on disk via [`Instance::merge_passive_status_patch`].
/// - **live status baseline**: the last `Status` a caller has actually
///   observed live for an in-memory `Instance`. Held on
///   `Instance::live_status_baseline` (`#[serde(skip)]`). `None` means
///   no live observation exists yet, so
///   [`Instance::update_status_with_metadata`] seeds it on the first
///   call without restamping.
/// - **detected status**: the `Status` a poller reads from tmux / ACP /
///   sandbox liveness on a single call. Distinct from the disk-loaded
///   `Instance::status`, which can be stale by up to one tick.
/// - **poller-authoritative status**: for plain-tmux sessions, the poller
///   owns `Instance::status`. For structured/ACP sessions,
///   `apply_acp_overlay_inplace` is the authority; see its docstring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PassiveStatusPatch {
    pub status: Status,
    pub lifecycle_generation: u64,
    pub idle_entered_at: Option<DateTime<Utc>>,
    /// `None` when the source `Instance` was never touched by a user
    /// (`last_accessed_at` itself `None`); must stay `None` in that case
    /// rather than fabricating a stamp, or a session that transitions
    /// status before anyone ever attaches would gain a spurious
    /// `last_accessed_at` and break the "`None` = never touched" contract
    /// that idle-reap and the freshness sort rely on.
    pub last_accessed_at: Option<DateTime<Utc>>,
}

impl PassiveStatusPatch {
    /// Build a patch from the current state of `inst`, as observed by a
    /// background poller. The `last_accessed_at` None-preservation
    /// contract is on [`Self::last_accessed_at`].
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

    /// `/api/sessions` puts `format!("{:?}", inst.status)` on the wire, so
    /// `from_api_str` has to speak `CamelCase` while `as_str` and serde speak
    /// `lowercase`. Two spellings of one enum drift silently unless the pairing
    /// is asserted against the actual formatter, which is what this does: a new
    /// variant that nobody teaches `from_api_str` fails here rather than
    /// showing up as a structured row whose status stops moving.
    #[test]
    fn status_api_wire_form_round_trips() {
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Idle,
            Status::Unknown,
            Status::Stopped,
            Status::Error,
            Status::Starting,
            Status::Deleting,
            Status::Creating,
        ] {
            let wire = format!("{status:?}");
            // `wire_str` is the explicit spelling every API surface emits;
            // pin it to `Debug` so a variant rename cannot silently change
            // the public wire format on one side only. See #3187.
            assert_eq!(
                status.wire_str(),
                wire,
                "wire_str must match the Debug spelling callers already receive"
            );
            assert_eq!(
                Status::from_api_str(&wire),
                Some(status),
                "wire form {wire} must parse back"
            );
            assert_eq!(
                Status::from_api_str(status.wire_str()),
                Some(status),
                "wire_str output must parse back through from_api_str"
            );
            // The lowercase serde/`as_str` spelling is a different vocabulary
            // and must NOT be accepted here, or a caller mixing the two would
            // silently work for `error` and fail for `Error`.
            assert_eq!(
                Status::from_api_str(status.as_str()),
                None,
                "from_api_str must not accept the lowercase spelling {}",
                status.as_str()
            );
        }
        assert_eq!(Status::from_api_str(""), None);
        assert_eq!(Status::from_api_str("Hibernating"), None);
    }

    // Tests for Status enum
    #[test]
    fn test_status_default() {
        let status = Status::default();
        assert_eq!(status, Status::Idle);
    }

    #[test]
    fn test_status_serialization() {
        let statuses = vec![
            Status::Running,
            Status::Waiting,
            Status::Idle,
            Status::Unknown,
            Status::Stopped,
            Status::Error,
            Status::Starting,
            Status::Deleting,
            Status::Creating,
        ];

        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: Status = serde_json::from_str(&json).unwrap();
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_status_unknown_serialization() {
        let status = Status::Unknown;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"unknown\"");
        let deserialized: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, Status::Unknown);
    }
}
