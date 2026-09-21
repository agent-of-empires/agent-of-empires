//! tmux integration module

pub(crate) mod composite;
pub(crate) mod detect;
pub(crate) mod env;
pub(crate) mod osc8;
mod session;
mod session_kind;
pub mod status_bar;
pub(crate) mod status_detection;
pub(crate) mod status_rules;
mod terminal_session;
#[cfg(test)]
pub(crate) mod test_helpers;
mod tool_session;
pub(crate) mod utils;
#[cfg(unix)]
pub(crate) mod vt;

pub use composite::PaneGeom;
pub use session::{PaneCursor, PaneEnvMutation, Session, SIZE_OWNER_HEARTBEAT, SIZE_OWNER_TTL};
pub use status_bar::{get_session_info_for_current, get_status_for_current_session};
pub use status_detection::{detect_status_from_content_in, detect_with_rules};
pub use terminal_session::{kill_all_terminals_for_id, ContainerTerminalSession, TerminalSession};
pub use tool_session::{kill_all_tool_sessions_for_id, ToolSession};
pub use utils::{attach_return_hint, tmux_prefix_display};

pub(crate) use session_kind::{append_session_kind_args, SessionKind};

/// Change count of `session`'s advertised OSC 8 links; always 0 off unix.
pub(crate) fn pane_links_generation(session: &str) -> u64 {
    #[cfg(unix)]
    {
        vt::pane_links_generation(session)
    }
    #[cfg(not(unix))]
    {
        let _ = session;
        0
    }
}

pub(crate) fn pane_links(session: &str) -> Vec<osc8::PaneLink> {
    #[cfg(unix)]
    {
        vt::pane_links(session)
    }
    #[cfg(not(unix))]
    {
        let _ = session;
        Vec::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    pub use super::env::{
        get_hidden_env, get_hidden_env_batch, remove_hidden_env, set_hidden_env,
        set_hidden_env_batch, AOE_CAPTURED_SESSION_ID_KEY, AOE_INSTANCE_ID_KEY,
    };
}

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// Overrides the tmux socket path (the e2e harness sets it).
pub const TMUX_SOCKET_ENV: &str = "AOE_TMUX_SOCKET";

/// The profile-merged config governing a session's `[tmux]` options.
pub(crate) fn tmux_option_config(profile: &str) -> crate::session::Config {
    crate::session::config::profile_config::resolve_config_or_warn(profile)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TmuxSocket {
    /// `tmux -S <path>`: build/test isolation and `AOE_TMUX_SOCKET`.
    Path(PathBuf),
    /// `tmux -L <name>`: the user's `tmux.socket_name` setting.
    Name(String),
}

/// Which tmux server this build talks to, cached for the process:
/// `AOE_TMUX_SOCKET`, else a test/debug isolation socket (so dev builds never
/// poison the release server), else `tmux.socket_name` in release builds, else
/// tmux's default socket.
fn tmux_socket() -> Option<TmuxSocket> {
    static SOCKET: OnceLock<Option<TmuxSocket>> = OnceLock::new();
    SOCKET
        .get_or_init(|| {
            if let Some(explicit) = std::env::var_os(TMUX_SOCKET_ENV) {
                if !explicit.is_empty() {
                    return Some(TmuxSocket::Path(PathBuf::from(explicit)));
                }
            }
            if let Some(path) = build_isolation_socket() {
                return Some(TmuxSocket::Path(path));
            }
            socket_from_config_name(configured_socket_name())
        })
        .clone()
}

fn build_isolation_socket() -> Option<PathBuf> {
    #[cfg(test)]
    {
        // Per-process so concurrent test binaries never share a server.
        return Some(
            std::env::temp_dir().join(format!("aoe-unit-test-tmux-{}.sock", std::process::id())),
        );
    }
    #[cfg(all(not(test), debug_assertions))]
    {
        match crate::session::get_app_dir() {
            Ok(dir) => return Some(dir.join("tmux.sock")),
            Err(e) => tracing::warn!(
                target: "tmux.socket",
                error = %e,
                "get_app_dir() failed; debug build falling back to tmux's default socket, \
                 which a dev build can share with (and poison for) release (#2608)"
            ),
        }
    }
    #[allow(unreachable_code)]
    None
}

fn configured_socket_name() -> Option<String> {
    crate::session::config::Config::load()
        .ok()
        .and_then(|c| c.tmux.socket_name)
}

/// `None` for an empty name or one containing a path separator (`-L` takes a
/// bare name); `AOE_TMUX_SOCKET` is the way to pass a path.
fn socket_from_config_name(name: Option<String>) -> Option<TmuxSocket> {
    let trimmed = name?.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        tracing::warn!(
            target: "tmux.socket",
            socket_name = %trimmed,
            "tmux.socket_name must be a bare name (no path separators); ignoring and using the default socket"
        );
        return None;
    }
    Some(TmuxSocket::Name(trimmed))
}

/// A `tmux` command on this build's socket. Every tmux invocation must use this
/// (or [`tmux_query_command`]) so all commands hit the same server.
pub(crate) fn tmux_command() -> Command {
    #[cfg(test)]
    fork_probe::record();
    let mut cmd = Command::new("tmux");
    match tmux_socket() {
        Some(TmuxSocket::Path(path)) => {
            cmd.arg("-S").arg(path);
        }
        Some(TmuxSocket::Name(name)) => {
            cmd.arg("-L").arg(name);
        }
        None => {}
    }
    // Attach runs while aoe ignores SIGINT/SIGQUIT, and SIG_IGN survives exec;
    // restore defaults so Ctrl+C still works in a hung tmux child.
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut cmd);
    cmd
}

/// [`tmux_command`] with `LC_MESSAGES=C` (and no `LC_ALL`) so stderr stays
/// matchable English, plus `-u` for UTF-8 session names. Not for interactive
/// paths, which must keep the user's locale.
pub(crate) fn tmux_query_command() -> Command {
    let mut cmd = tmux_command();
    cmd.arg("-u");
    cmd.env_remove("LC_ALL");
    cmd.env("LC_MESSAGES", "C");
    cmd
}

// Debug builds use `aoe_dev_*` so dev and release sessions never mix.
pub const SESSION_PREFIX: &str = if cfg!(debug_assertions) {
    "aoe_dev_"
} else {
    "aoe_"
};
pub const TERMINAL_PREFIX: &str = if cfg!(debug_assertions) {
    "aoe_dev_term_"
} else {
    "aoe_term_"
};
pub const CONTAINER_TERMINAL_PREFIX: &str = if cfg!(debug_assertions) {
    "aoe_dev_cterm_"
} else {
    "aoe_cterm_"
};
pub const TOOL_PREFIX: &str = if cfg!(debug_assertions) {
    "aoe_dev_tool_"
} else {
    "aoe_tool_"
};

#[derive(Debug, Clone)]
pub struct PaneMetadata {
    pub pane_dead: bool,
    pub pane_current_command: Option<String>,
    pub pane_start_command_is_protected: bool,
    pub pane_pid: Option<u32>,
    /// The OSC 0/2 terminal title, which several agents use for their state.
    pub pane_title: Option<String>,
    /// The window's last-output time, used to skip unchanged captures.
    pub window_activity: Option<i64>,
    /// Observed window (not pane) size: splits and chrome only move rows inside
    /// the window, so this is what passive resize compares against.
    pub window_size: Option<(u16, u16)>,
}

static SESSION_REFRESH_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static FORCED_SESSION_CACHE_GUARDS: AtomicUsize = AtomicUsize::new(0);

/// Whether a test's [`SessionCacheGuard`] owns the cache, in which case refreshes
/// must not overwrite its forced snapshot.
#[cfg(test)]
fn forced_session_cache_active() -> bool {
    FORCED_SESSION_CACHE_GUARDS.load(Ordering::SeqCst) > 0
}

#[cfg(not(test))]
fn forced_session_cache_active() -> bool {
    false
}
static SESSION_CACHE: RwLock<SessionCache> = RwLock::new(SessionCache {
    data: None,
    time: None,
    refresh_id: 0,
    outcome: SessionCacheRefresh::Unknown,
});

#[derive(Debug, Clone)]
pub(crate) struct LiveSession {
    activity: i64,
    /// Absent for sessions created before [`session_kind::KIND_OPTION`].
    kind: Option<SessionKind>,
}

#[cfg(test)]
impl LiveSession {
    fn unmarked() -> Self {
        Self {
            activity: 0,
            kind: None,
        }
    }
}

struct SessionCache {
    data: Option<HashMap<String, LiveSession>>,
    time: Option<Instant>,
    refresh_id: u64,
    outcome: SessionCacheRefresh,
}

/// Shared `list-panes` snapshot, mirroring `SESSION_CACHE`.
static PANE_META_REFRESH_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PANE_META_CACHE: RwLock<PaneMetaCache> = RwLock::new(PaneMetaCache {
    data: None,
    time: None,
    refresh_id: 0,
});

struct PaneMetaCache {
    data: Option<std::sync::Arc<HashMap<String, PaneMetadata>>>,
    time: Option<Instant>,
    refresh_id: u64,
}
pub(crate) const TMUX_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// One wall-clock budget shared by every tmux subprocess in an operation.
pub(crate) struct TmuxCommandDeadline {
    deadline: Instant,
    #[cfg(test)]
    budget: Option<CommandBudget>,
}

/// Test stand-in clock: the first `commands` runs get the full timeout, then
/// the budget reads as spent.
#[cfg(test)]
struct CommandBudget(std::sync::atomic::AtomicI64);

impl TmuxCommandDeadline {
    pub(crate) fn new() -> Self {
        Self {
            deadline: Instant::now() + TMUX_COMMAND_TIMEOUT,
            #[cfg(test)]
            budget: None,
        }
    }
    #[cfg(test)]
    fn with_timeout(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            budget: None,
        }
    }

    #[cfg(test)]
    fn expiring_after_commands(commands: i64) -> Self {
        Self {
            deadline: Instant::now() + TMUX_COMMAND_TIMEOUT,
            budget: Some(CommandBudget(std::sync::atomic::AtomicI64::new(commands))),
        }
    }

    fn remaining(&self) -> Duration {
        #[cfg(test)]
        if let Some(budget) = &self.budget {
            return if budget.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) > 0 {
                TMUX_COMMAND_TIMEOUT
            } else {
                Duration::ZERO
            };
        }
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub(crate) fn run(&self, cmd: &mut Command) -> std::io::Result<Output> {
        let remaining = self.remaining();
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "tmux operation deadline elapsed",
            ));
        }
        run_tmux_command_with_timeout_inner(cmd, remaining)
    }
}
#[cfg(test)]
thread_local! {
    static TMUX_COMMAND_EXECUTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn run_tmux_command_with_timeout_inner(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<Output> {
    #[cfg(test)]
    TMUX_COMMAND_EXECUTIONS.with(|count| count.set(count.get() + 1));
    cmd.stdin(Stdio::null());
    match crate::process::run_with_timeout(cmd, timeout)? {
        Some(output) => Ok(output),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("tmux command timed out after {}s", timeout.as_secs_f64()),
        )),
    }
}

pub(crate) fn run_tmux_command_with_timeout(cmd: &mut Command) -> std::io::Result<Output> {
    TmuxCommandDeadline::new().run(cmd)
}

/// Outcome of the `list-sessions` scan; both failures leave `data: None`, but
/// rekeying stays quiet only for the recognized no-server case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCacheRefresh {
    Populated,
    NoServer,
    Unknown,
}

// Printable and absent from sanitized names; C0 bytes are reserved for the tail.
const FIELD_SEP: char = '|';
/// Separates the trailing fields, which may contain `FIELD_SEP`. tmux 3.4
/// escapes it as `ESCAPED_TAIL_SEP`; newer versions emit it raw.
const TAIL_SEP: char = '\x1f';
const ESCAPED_TAIL_SEP: &str = r"\037";

/// Whether stderr says there is no server: `no server running` or an ENOENT
/// connect failure. Other connect errnos stay errors. Markers are anchored per
/// line so a socket path cannot spoof them; callers must use
/// [`tmux_query_command`] for stable English.
pub(super) fn tmux_no_server_running(stderr: &[u8]) -> bool {
    let s = String::from_utf8_lossy(stderr);
    s.lines().any(|line| {
        let line = line.trim();
        line.starts_with("no server running")
            || (line.starts_with("error connecting to ")
                && line.ends_with("(No such file or directory)"))
    })
}

fn next_refresh_id(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

fn publish_session_cache(
    refresh_id: u64,
    data: Option<HashMap<String, LiveSession>>,
    outcome: SessionCacheRefresh,
    respect_forced_guard: bool,
) -> SessionCacheRefresh {
    let Ok(mut cache) = SESSION_CACHE.write() else {
        return SessionCacheRefresh::Unknown;
    };
    if respect_forced_guard && forced_session_cache_active() {
        return outcome;
    }
    if refresh_id <= cache.refresh_id {
        return cache.outcome;
    }
    // An unexpected failure keeps the last good list for display lookups.
    if outcome != SessionCacheRefresh::Unknown {
        cache.data = data;
    }
    cache.time = Some(Instant::now());
    cache.refresh_id = refresh_id;
    cache.outcome = outcome;
    outcome
}
/// A fresh scan with the cache's parser; `None` when tmux is unreachable.
pub(crate) fn probe_live_sessions() -> Option<HashMap<String, LiveSession>> {
    let output = run_tmux_command_with_timeout(&mut session_scan_command()).ok()?;
    output
        .status
        .success()
        .then(|| parse_session_scan(&String::from_utf8_lossy(&output.stdout)))
}

pub(crate) fn marked_names(
    sessions: &HashMap<String, LiveSession>,
) -> impl Iterator<Item = (&str, Option<&str>)> {
    sessions
        .iter()
        .map(|(name, session)| (name.as_str(), session.kind.map(SessionKind::as_marker)))
}

fn session_scan_command() -> Command {
    let mut command = tmux_query_command();
    for flags in INHERITABLE_KIND_SCOPES {
        command.args(["show-options", flags, session_kind::KIND_OPTION, ";"]);
    }
    command.args(["list-sessions", "-F", SESSION_SCAN_FORMAT]);
    command
}

/// Server-wide scopes that also answer `#{@aoe_kind}`, read back so the scan
/// can subtract them. Measured on tmux 3.6, all but `-g` override a session's
/// own mark, so subtracting them never discards a visible legitimate mark.
const INHERITABLE_KIND_SCOPES: [&str; 3] = ["-gqv", "-sqv", "-gwqv"];

const SESSION_SCAN_FORMAT: &str = "#{session_name}|#{session_activity}|#{@aoe_kind}";

/// Session lines are `<name>|<activity>|<marker>`; the integer activity field
/// tells them apart from scope values that may contain `|`.
fn is_session_line(line: &str) -> bool {
    let mut fields = line.split(FIELD_SEP);
    fields.next();
    fields
        .next()
        .is_some_and(|activity| activity.parse::<i64>().is_ok())
}

/// Parse [`session_scan_command`] output. A session whose marker equals one of
/// the leading scope values is treated as unmarked (name-shape fallback).
fn parse_session_scan(stdout: &str) -> HashMap<String, LiveSession> {
    let mut lines = stdout.lines().peekable();
    let mut inherited: Vec<&str> = Vec::new();
    while let Some(line) = lines.next_if(|line| !is_session_line(line)) {
        if !line.is_empty() {
            inherited.push(line);
        }
    }

    let mut map = HashMap::new();
    for line in lines {
        let Some((name, rest)) = line.split_once(FIELD_SEP) else {
            continue;
        };
        let (activity, marker) = match rest.split_once(FIELD_SEP) {
            Some((activity, marker)) => (activity, Some(marker)),
            None => (rest, None),
        };
        map.insert(
            name.to_string(),
            LiveSession {
                activity: activity.parse().unwrap_or(0),
                kind: marker
                    .filter(|marker| !inherited.contains(marker))
                    .and_then(SessionKind::from_marker),
            },
        );
    }
    map
}

pub fn refresh_session_cache() -> SessionCacheRefresh {
    let refresh_id = next_refresh_id(&SESSION_REFRESH_ID);
    let start = Instant::now();
    let mut command = session_scan_command();
    let output = run_tmux_command_with_timeout(&mut command);
    let (new_data, outcome) = match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            (
                Some(parse_session_scan(&stdout)),
                SessionCacheRefresh::Populated,
            )
        }
        Ok(out) if tmux_no_server_running(&out.stderr) => {
            tracing::trace!(target: "tmux.cache", "no tmux server running; cache cleared");
            (None, SessionCacheRefresh::NoServer)
        }
        Ok(out) => {
            tracing::warn!(
                target: "tmux.cache",
                status = ?out.status,
                stderr_bytes = out.stderr.len(),
                "list-sessions returned non-zero; cache state unknown",
            );
            (None, SessionCacheRefresh::Unknown)
        }
        Err(e) => {
            tracing::warn!(target: "tmux.cache", error = %e, "list-sessions spawn failed; cache state unknown");
            (None, SessionCacheRefresh::Unknown)
        }
    };

    // Trace: the TUI polls this every ~2s.
    let sessions = new_data.as_ref().map(|m| m.len()).unwrap_or(0);
    tracing::trace!(
        target: "tmux.cache",
        sessions,
        duration_ms = start.elapsed().as_millis() as u64,
        "session cache refreshed",
    );

    publish_session_cache(refresh_id, new_data, outcome, true)
}

/// Existence of the selected agent session, where an ambiguous resolution is
/// Unknown rather than Absent.
fn resolved_agent_existence(
    id: &str,
    session: &Session,
    refresh: SessionCacheRefresh,
) -> SessionExistence {
    match refresh {
        SessionCacheRefresh::NoServer => return SessionExistence::Absent,
        SessionCacheRefresh::Unknown => return SessionExistence::Unknown,
        SessionCacheRefresh::Populated => {}
    }
    let cache = match SESSION_CACHE.read() {
        Ok(cache) if cache.time.is_some_and(|time| time.elapsed() <= CACHE_TTL) => cache,
        _ => return SessionExistence::Unknown,
    };
    let Some(names) = cache.data.as_ref() else {
        return SessionExistence::Unknown;
    };
    if names.contains_key(session.name()) {
        return SessionExistence::Present;
    }
    let suffix = id_suffix(id);
    let shape = NameShape::agent(&suffix);
    if !names.keys().any(|name| shape.matches(name)) {
        return SessionExistence::Absent;
    }
    SessionExistence::Unknown
}

/// Rekey a live tmux session after its new title is persisted. `Ok(false)` only
/// when tmux confirms no live session. Callers hold the title and lifecycle
/// locks and persist first.
pub(crate) fn rekey_session(id: &str, old_title: &str, new_title: &str) -> anyhow::Result<bool> {
    let renamed = rekey_session_name(id, old_title, new_title)?;
    if renamed {
        status_bar::refresh_session_title(&Session::generate_name(id, new_title), new_title);
    }
    Ok(renamed)
}

fn rekey_session_name(id: &str, old_title: &str, new_title: &str) -> anyhow::Result<bool> {
    // Force a fresh scan so a stale snapshot cannot target the old name.
    let initial_refresh = refresh_session_cache();
    let session = Session::new(id, old_title)?;
    match resolved_agent_existence(id, &session, initial_refresh) {
        SessionExistence::Present => {}
        SessionExistence::Absent => return Ok(false),
        SessionExistence::Unknown => {
            anyhow::bail!("Could not determine whether the tmux session exists")
        }
    }

    let new_name = Session::generate_name(id, new_title);
    let original_name = session.name().to_string();
    let original_error = match session.rename(&new_name) {
        Ok(()) => {
            refresh_session_cache();
            return Ok(true);
        }
        Err(error) => error,
    };

    // Another process may have rekeyed meanwhile: re-resolve by id suffix and
    // retry once. A failed query keeps the original rename error.
    let retry_refresh = refresh_session_cache();
    let refreshed = Session::new(id, old_title)?;
    match resolved_agent_existence(id, &refreshed, retry_refresh) {
        SessionExistence::Absent => return Ok(false),
        SessionExistence::Unknown => return Err(original_error),
        SessionExistence::Present => {}
    }
    if refreshed.name() == new_name {
        return Ok(true);
    }
    if refreshed.name() == original_name {
        return Err(original_error);
    }

    let retry_error = match refreshed.rename(&new_name) {
        Ok(()) => {
            refresh_session_cache();
            return Ok(true);
        }
        Err(error) => error,
    };
    let final_refresh = refresh_session_cache();
    let final_session = Session::new(id, old_title)?;
    match resolved_agent_existence(id, &final_session, final_refresh) {
        SessionExistence::Absent => Ok(false),
        SessionExistence::Unknown => Err(original_error),
        SessionExistence::Present if final_session.name() == new_name => Ok(true),
        SessionExistence::Present => Err(retry_error),
    }
}

/// Every session kind nests under `SESSION_PREFIX` for this build.
fn is_aoe_session(name: &str) -> bool {
    name.starts_with(SESSION_PREFIX)
}

/// The `_<id8>` tail, immutable across renames.
fn id_suffix(session_id: &str) -> String {
    format!("_{}", crate::cli::truncate_id(session_id, 8))
}

/// `<prefix><sanitized title><suffix>` for one kind and session id; only the
/// title moves.
pub(crate) struct NameShape<'a> {
    pub prefix: &'a str,
    pub suffix: &'a str,
    /// Needed because auxiliary prefixes nest under `SESSION_PREFIX` and titles can
    /// sanitize into another kind's shape.
    pub kind: SessionKind,
}

impl NameShape<'_> {
    pub(crate) fn agent<'a>(suffix: &'a str) -> NameShape<'a> {
        NameShape {
            prefix: SESSION_PREFIX,
            suffix,
            kind: SessionKind::Agent,
        }
    }

    pub(crate) fn terminal<'a>(suffix: &'a str) -> NameShape<'a> {
        NameShape {
            prefix: TERMINAL_PREFIX,
            suffix,
            kind: SessionKind::Terminal,
        }
    }

    pub(crate) fn container<'a>(suffix: &'a str) -> NameShape<'a> {
        NameShape {
            prefix: CONTAINER_TERMINAL_PREFIX,
            suffix,
            kind: SessionKind::ContainerTerminal,
        }
    }

    /// `marker` is `None` for pre-marker sessions, which classify by name shape.
    fn matches_marked(&self, name: &str, marker: Option<&str>) -> bool {
        name.starts_with(self.prefix)
            && name.ends_with(self.suffix)
            && SessionKind::of(name, marker) == Some(self.kind)
    }

    fn matches(&self, name: &str) -> bool {
        self.matches_marked(name, None)
    }
}

/// Whether `tmux_name` is `session_id`'s agent session, whatever title it was
/// created with.
pub fn agent_session_belongs_to(tmux_name: &str, session_id: &str) -> bool {
    NameShape::agent(&id_suffix(session_id)).matches(tmux_name)
}

pub(crate) type MarkedSessionName = (String, Option<SessionKind>);

/// Fresh (not cached) tmux observations shared by a batch of liveness lookups:
/// at most one `list-sessions` and one `list-panes -a`, each taken lazily. An
/// unreachable server stays distinguishable from absence.
#[derive(Default)]
pub(crate) struct LiveSessionSnapshot {
    sessions: OnceLock<Option<Vec<MarkedSessionName>>>,
    panes: OnceLock<Option<HashMap<String, PaneMetadata>>>,
}

impl LiveSessionSnapshot {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn from_parts(
        names: Option<Vec<String>>,
        panes: Option<HashMap<String, PaneMetadata>>,
    ) -> Self {
        Self::from_marked_parts(
            names.map(|names| names.into_iter().map(|name| (name, None)).collect()),
            panes,
        )
    }

    #[cfg(test)]
    pub(crate) fn from_marked_parts(
        names: Option<Vec<MarkedSessionName>>,
        panes: Option<HashMap<String, PaneMetadata>>,
    ) -> Self {
        let snapshot = Self::new();
        let _ = snapshot.sessions.set(names);
        let _ = snapshot.panes.set(panes);
        snapshot
    }

    /// `None` when tmux is unreachable. Also warms the display cache.
    pub(crate) fn sessions(&self) -> Option<&[MarkedSessionName]> {
        self.sessions
            .get_or_init(|| {
                if refresh_session_cache() != SessionCacheRefresh::Populated {
                    return None;
                }
                SESSION_CACHE.read().ok().and_then(|cache| {
                    cache.data.as_ref().map(|sessions| {
                        sessions
                            .iter()
                            .map(|(name, session)| (name.clone(), session.kind))
                            .collect()
                    })
                })
            })
            .as_deref()
    }

    pub(crate) fn names(&self) -> Option<impl Iterator<Item = &str>> {
        Some(self.sessions()?.iter().map(|(name, _)| name.as_str()))
    }

    /// From the batched metadata; an absent entry reads as alive.
    pub(crate) fn pane_dead(&self, name: &str) -> bool {
        self.panes
            .get_or_init(|| batch_pane_metadata().ok())
            .as_ref()
            .and_then(|panes| panes.get(name))
            .map(|meta| meta.pane_dead)
            .unwrap_or(false)
    }
}

/// The unique live agent pane for a poller seed; multiple matches are ambiguous.
pub(crate) fn live_agent_name_for_id_in(
    snapshot: &LiveSessionSnapshot,
    session_id: &str,
) -> Option<String> {
    live_agent_name_for_id(
        snapshot
            .sessions()?
            .iter()
            .map(|(name, kind)| (name.as_str(), kind.map(SessionKind::as_marker))),
        session_id,
        |name| snapshot.pane_dead(name),
    )
}

pub(crate) fn live_agent_name_for_id<'a>(
    live: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    session_id: &str,
    pane_dead: impl Fn(&str) -> bool,
) -> Option<String> {
    let suffix = id_suffix(session_id);
    let agent = NameShape::agent(&suffix);
    let mut matches = live
        .into_iter()
        .filter(|(name, marker)| agent.matches_marked(name, *marker) && !pane_dead(name));
    let (name, _) = matches.next()?;
    matches.next().is_none().then(|| name.to_owned())
}

pub(crate) fn live_any_kind_name_for_id_in(
    snapshot: &LiveSessionSnapshot,
    session_id: &str,
) -> Option<String> {
    let sessions = snapshot.sessions()?;
    live_any_kind_name_for_id(
        sessions
            .iter()
            .map(|(name, kind)| (name.as_str(), kind.map(SessionKind::as_marker))),
        session_id,
        |name| snapshot.pane_dead(name),
    )
}

/// The live session carrying `session_id`'s tail, preferring agent, then
/// terminal, then container terminal, skipping dead panes.
pub(crate) fn live_any_kind_name_for_id<'a>(
    live: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    session_id: &str,
    pane_dead: impl Fn(&str) -> bool,
) -> Option<String> {
    let suffix = id_suffix(session_id);
    let agent = NameShape::agent(&suffix);
    let terminal = NameShape::terminal(&suffix);
    let container = NameShape::container(&suffix);
    let (mut agent_hit, mut terminal_hit, mut container_hit) = (None, None, None);
    for (name, marker) in live {
        let bucket = if agent.matches_marked(name, marker) {
            &mut agent_hit
        } else if terminal.matches_marked(name, marker) {
            &mut terminal_hit
        } else if container.matches_marked(name, marker) {
            &mut container_hit
        } else {
            continue;
        };
        if bucket.is_none() && !pane_dead(name) {
            *bucket = Some(name.to_string());
        }
    }
    agent_hit.or(terminal_hit).or(container_hit)
}

#[cfg(test)]
fn unmarked<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> impl Iterator<Item = (&'a str, Option<&'a str>)> {
    names.into_iter().map(|name| (name, None))
}

/// The name to act on: `derived` unless it is not live and exactly one other
/// live session fits `shape` (a retitle without a tmux rename).
pub(crate) fn resolve_session_name<'a>(
    live: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    derived: &str,
    shape: &NameShape,
) -> String {
    let mut adopted: Option<&str> = None;
    let mut ambiguous = false;
    let mut derived_is_live = false;
    for (name, marker) in live {
        // A live derived name wins even if it fails the shape, unless it is marked
        // as another kind.
        if name == derived {
            derived_is_live = SessionKind::from_marker(marker.unwrap_or_default())
                .is_none_or(|kind| kind == shape.kind);
            continue;
        }
        if !shape.matches_marked(name, marker) {
            continue;
        }
        if adopted.replace(name).is_some() {
            ambiguous = true;
        }
    }
    match adopted {
        Some(name) if !derived_is_live && !ambiguous => name.to_string(),
        _ => derived.to_string(),
    }
}

/// Names-only: without markers, shapes cannot tell kinds apart.
pub fn resolve_agent_session_name<'a>(
    live_names: impl IntoIterator<Item = &'a str>,
    session_id: &str,
    derived: &str,
) -> String {
    let suffix = id_suffix(session_id);
    resolve_session_name(
        live_names.into_iter().map(|name| (name, None)),
        derived,
        &NameShape::agent(&suffix),
    )
}

/// Against a [`batch_pane_metadata`] snapshot, O(1) when the derived name is live.
pub fn resolve_agent_session_name_in(
    pane_metadata: &HashMap<String, PaneMetadata>,
    session_id: &str,
    derived: &str,
) -> String {
    if pane_metadata.contains_key(derived) {
        return derived.to_string();
    }
    resolve_agent_session_name(
        pane_metadata.keys().map(String::as_str),
        session_id,
        derived,
    )
}

/// [`resolve_session_name`] against the shared cache, refreshing a stale
/// snapshot once; `derived` when tmux is unreachable.
pub(crate) fn live_session_name(derived: &str, shape: &NameShape) -> String {
    if let Some(name) = session_name_from_cache(derived, shape) {
        return name;
    }
    refresh_session_cache();
    session_name_from_cache(derived, shape).unwrap_or_else(|| derived.to_string())
}

/// Snapshot-only [`live_session_name`] for paint, which never waits on tmux.
pub(crate) fn session_name_for_display(derived: &str, shape: &NameShape) -> String {
    let Ok(cache) = SESSION_CACHE.read() else {
        return derived.to_string();
    };
    resolve_session_name_from_snapshot(cache.data.as_ref(), derived, shape)
}

pub(crate) fn agent_session_name_for_display(session_id: &str, derived: &str) -> String {
    let suffix = id_suffix(session_id);
    session_name_for_display(derived, &NameShape::agent(&suffix))
}

pub fn live_agent_session_name(session_id: &str, derived: &str) -> String {
    let suffix = id_suffix(session_id);
    live_session_name(derived, &NameShape::agent(&suffix))
}

fn resolve_session_name_from_snapshot(
    sessions: Option<&HashMap<String, LiveSession>>,
    derived: &str,
    shape: &NameShape,
) -> String {
    let Some(sessions) = sessions else {
        return derived.to_string();
    };
    // The fast path requires the live derived name to be this kind.
    if sessions
        .get(derived)
        .is_some_and(|session| session.kind.is_none_or(|kind| kind == shape.kind))
    {
        return derived.to_string();
    }
    resolve_session_name(
        sessions
            .iter()
            .map(|(name, session)| (name.as_str(), session.kind.map(SessionKind::as_marker))),
        derived,
        shape,
    )
}

/// `None` when the snapshot is stale or the lock poisoned.
fn session_name_from_cache(derived: &str, shape: &NameShape) -> Option<String> {
    let cache = SESSION_CACHE.read().ok()?;
    let fresh = cache
        .time
        .map(|t| t.elapsed() <= CACHE_TTL)
        .unwrap_or(false);
    if !fresh {
        return None;
    }
    if cache.outcome == SessionCacheRefresh::Unknown {
        return Some(derived.to_string());
    }
    Some(resolve_session_name_from_snapshot(
        cache.data.as_ref(),
        derived,
        shape,
    ))
}

/// Kill every aoe tmux session in this namespace and return the count. `Err`
/// only when `list-sessions` cannot spawn; no server is `Ok(0)`.
pub fn stop_all_sessions() -> anyhow::Result<usize> {
    let output = tmux_query_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .map_err(|e| anyhow::anyhow!("tmux list-sessions spawn failed: {e}"))?;

    let mut matched = false;
    let killed = if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        stop_aoe_sessions(stdout.lines(), |name| {
            matched = true;
            if let Some(pid) = crate::process::get_pane_pid(name) {
                crate::process::kill_process_tree(pid);
            }
            utils::kill_session_if_present(name).is_ok()
        })
    } else {
        0
    };

    if matched {
        refresh_session_cache();
    }
    Ok(killed)
}

fn stop_aoe_sessions<'a>(
    names: impl Iterator<Item = &'a str>,
    mut stop: impl FnMut(&str) -> bool,
) -> usize {
    names
        .filter(|name| is_aoe_session(name))
        .filter(|name| stop(name))
        .count()
}

/// Pane metadata for every aoe session's first pane in one call. `Err` means
/// "don't know" and must not be read as absence.
pub fn batch_pane_metadata() -> anyhow::Result<HashMap<String, PaneMetadata>> {
    let start = Instant::now();
    let mut command = tmux_query_command();
    command.args([
        "list-panes",
        "-a",
        "-F",
        // Fields that may contain `|` ride `TAIL_SEP` after `pane_pid`.
        concat!(
            "#{session_name}|#{pane_index}|#{pane_dead}|#{window_width}|#{window_height}",
            "|#{pane_current_command}",
            "|#{pane_start_command}|#{pane_pid}\x1f#{window_activity}\x1f#{pane_title}"
        ),
    ]);
    let output = run_tmux_command_with_timeout(&mut command);

    let result: anyhow::Result<HashMap<String, PaneMetadata>> = match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            Ok(parse_pane_metadata(&stdout))
        }
        Ok(out) => {
            if tmux_no_server_running(&out.stderr) {
                tracing::trace!(target: "tmux.pane", "no tmux server running; no panes");
                Ok(HashMap::new())
            } else {
                tracing::warn!(
                    target: "tmux.pane",
                    status = ?out.status,
                    stderr_bytes = out.stderr.len(),
                    "list-panes returned non-zero",
                );
                Err(anyhow::anyhow!(
                    "tmux list-panes returned non-zero status: {:?}",
                    out.status
                ))
            }
        }
        Err(e) => {
            tracing::warn!(target: "tmux.pane", error = %e, "list-panes spawn failed");
            Err(anyhow::anyhow!("tmux list-panes spawn failed: {}", e))
        }
    };

    // Trace: polled every ~2s.
    tracing::trace!(
        target: "tmux.pane",
        sessions = result.as_ref().map(|m| m.len()).unwrap_or(0),
        duration_ms = start.elapsed().as_millis() as u64,
        "batch pane metadata fetched",
    );
    result
}

/// aoe sessions with an attached client. `Err` means "don't know, skip".
pub fn attached_session_names() -> anyhow::Result<HashSet<String>> {
    let output = tmux_query_command()
        .args(["list-sessions", "-F", "#{session_name}|#{session_attached}"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut attached = HashSet::new();
            for line in stdout.lines() {
                if let Some((name, flag)) = line.split_once(FIELD_SEP) {
                    if name.starts_with(SESSION_PREFIX) && flag.trim() != "0" {
                        attached.insert(name.to_string());
                    }
                }
            }
            Ok(attached)
        }
        Ok(out) => {
            if tmux_no_server_running(&out.stderr) {
                tracing::trace!(target: "tmux.cache", "no tmux server running; nothing attached");
                Ok(HashSet::new())
            } else {
                tracing::warn!(
                    target: "tmux.cache",
                    status = ?out.status,
                    "list-sessions (attached) returned non-zero",
                );
                Err(anyhow::anyhow!(
                    "tmux list-sessions returned non-zero status: {:?}",
                    out.status
                ))
            }
        }
        Err(e) => {
            tracing::warn!(target: "tmux.cache", error = %e, "list-sessions (attached) spawn failed");
            Err(anyhow::anyhow!("tmux list-sessions spawn failed: {}", e))
        }
    }
}

fn find_escaped_tail_sep(line: &str, from: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let separator = ESCAPED_TAIL_SEP.as_bytes();
    let mut offset = from;
    while offset + separator.len() <= bytes.len() {
        if bytes[offset..].starts_with(separator) {
            let preceding_slashes = bytes[..offset]
                .iter()
                .rev()
                .take_while(|&&byte| byte == b'\\')
                .count();
            if preceding_slashes % 2 == 0 {
                return Some(offset);
            }
        }
        offset += 1;
    }
    None
}

fn split_pane_metadata_tail(line: &str) -> (&str, Option<&str>, Option<&str>) {
    if let Some(first) = line.find(TAIL_SEP) {
        let rest = &line[first + TAIL_SEP.len_utf8()..];
        return match rest.find(TAIL_SEP) {
            Some(second) => (
                &line[..first],
                Some(&rest[..second]),
                Some(&rest[second + 1..]),
            ),
            None => (&line[..first], Some(rest), None),
        };
    }

    let Some(first) = find_escaped_tail_sep(line, 0) else {
        return (line, None, None);
    };
    let rest_start = first + ESCAPED_TAIL_SEP.len();
    match find_escaped_tail_sep(line, rest_start) {
        Some(second) => (
            &line[..first],
            Some(&line[rest_start..second]),
            Some(&line[second + ESCAPED_TAIL_SEP.len()..]),
        ),
        None => (&line[..first], Some(&line[rest_start..]), None),
    }
}

/// Filters to aoe sessions, pane index 0, first window per session.
fn parse_pane_metadata(output: &str) -> HashMap<String, PaneMetadata> {
    let mut map = HashMap::new();

    for line in output.lines() {
        let (line, activity, pane_title) = split_pane_metadata_tail(line);
        let window_activity = activity.and_then(|a| a.trim().parse::<i64>().ok());
        let pane_title = pane_title.unwrap_or("");
        let mut parts = line.splitn(7, FIELD_SEP);
        let (
            Some(session_name),
            Some(pane_index),
            Some(pane_dead),
            Some(window_width),
            Some(window_height),
            Some(pane_current_command),
            Some(rest),
        ) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        )
        else {
            continue;
        };
        let window_size = window_width
            .parse::<u16>()
            .ok()
            .zip(window_height.parse::<u16>().ok());
        // The start command may contain the separator, so split the pid off the end.
        let (pane_start_command, pane_pid) = match rest.rsplit_once(FIELD_SEP) {
            Some((command, pid)) => (command, pid.trim().parse().ok()),
            None => (rest, None),
        };
        if !session_name.starts_with(SESSION_PREFIX) {
            continue;
        }

        if pane_index != "0" {
            continue;
        }

        if map.contains_key(session_name) {
            continue;
        }

        map.insert(
            session_name.to_string(),
            PaneMetadata {
                pane_dead: pane_dead == "1",
                pane_pid,
                pane_current_command: if pane_current_command.is_empty() {
                    None
                } else {
                    Some(pane_current_command.to_string())
                },
                pane_start_command_is_protected: pane_start_command
                    .contains(utils::PANE_ENV_FILE_PREFIX),
                pane_title: (!pane_title.is_empty()).then(|| pane_title.to_string()),
                window_activity,
                window_size,
            },
        );
    }

    map
}

/// Observed window size and the instant taken before the `list-panes` fork.
pub(crate) fn observed_window_size_from_cache(session_name: &str) -> Option<((u16, u16), Instant)> {
    let cache = PANE_META_CACHE.read().ok()?;
    let time = cache.time?;
    let size = cache.data.as_ref()?.get(session_name)?.window_size?;
    Some((size, time))
}

/// Test-only: make `session_exists_from_cache` see `name`.
#[cfg(test)]
pub fn test_inject_session_into_cache(name: &str) {
    if let Ok(mut cache) = SESSION_CACHE.write() {
        let map = cache.data.get_or_insert_with(HashMap::new);
        map.insert(name.to_string(), LiveSession::unmarked());
        cache.time = Some(Instant::now());
    }
}

#[cfg(test)]
pub fn test_inject_pane_window_size(name: &str, size: (u16, u16)) {
    test_inject_pane_window_size_at(name, size, Instant::now());
}

/// Test-only: publish a pane snapshot through the real publication path with an
/// explicit observation time.
#[cfg(test)]
pub fn test_inject_pane_window_size_at(name: &str, size: (u16, u16), taken_at: Instant) {
    let map = {
        let Ok(cache) = PANE_META_CACHE.read() else {
            return;
        };
        let mut map = cache.data.as_deref().cloned().unwrap_or_default();
        map.insert(
            name.to_string(),
            PaneMetadata {
                pane_dead: false,
                pane_current_command: None,
                pane_start_command_is_protected: false,
                pane_pid: None,
                pane_title: None,
                window_activity: None,
                window_size: Some(size),
            },
        );
        map
    };
    publish_pane_meta_cache(
        next_refresh_id(&PANE_META_REFRESH_ID),
        Some(std::sync::Arc::new(map)),
        taken_at,
    );
}

/// Test-only per-thread fork counter at `tmux_command()`, so paint-path tests
/// can assert zero forks.
#[cfg(test)]
pub(crate) mod fork_probe {
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) struct Guard;

    pub(crate) fn arm() -> Guard {
        ARMED.with(|a| a.set(true));
        Guard
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ARMED.with(|a| a.set(false));
        }
    }

    pub(crate) fn record() {
        if ARMED.with(Cell::get) {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    }

    pub(crate) fn take() -> u64 {
        COUNT.with(|c| {
            let n = c.get();
            c.set(0);
            n
        })
    }
}

/// Test-only guard that restores [`SESSION_CACHE`] on drop. Pair with
/// `#[serial_test::serial]`.
#[cfg(test)]
pub(crate) struct SessionCacheGuard {
    prev_data: Option<HashMap<String, LiveSession>>,
    prev_time: Option<Instant>,
    prev_refresh_id: u64,
    prev_outcome: SessionCacheRefresh,
    forced_snapshot: bool,
}

#[cfg(test)]
impl SessionCacheGuard {
    pub(crate) fn capture() -> Self {
        Self::capture_inner(true)
    }

    pub(crate) fn capture_restore_only() -> Self {
        Self::capture_inner(false)
    }

    fn capture_inner(forced_snapshot: bool) -> Self {
        let cache = SESSION_CACHE.write().expect("session cache lock");
        if forced_snapshot {
            FORCED_SESSION_CACHE_GUARDS.fetch_add(1, Ordering::SeqCst);
        }
        Self {
            prev_data: cache.data.clone(),
            prev_time: cache.time,
            prev_refresh_id: cache.refresh_id,
            prev_outcome: cache.outcome,
            forced_snapshot,
        }
    }

    pub(crate) fn force_unreachable(&self) {
        if let Ok(mut cache) = SESSION_CACHE.write() {
            cache.data = None;
            cache.time = Some(Instant::now());
            cache.outcome = SessionCacheRefresh::Unknown;
        }
    }

    pub(crate) fn force_present(&self, names: &[&str]) {
        if let Ok(mut cache) = SESSION_CACHE.write() {
            cache.data = Some(
                names
                    .iter()
                    .map(|n| (n.to_string(), LiveSession::unmarked()))
                    .collect(),
            );
            cache.time = Some(Instant::now());
            cache.outcome = SessionCacheRefresh::Populated;
        }
    }

    /// An expired snapshot with data intact; paint must still answer from it.
    pub(crate) fn force_stale(&self) {
        if let Ok(mut cache) = SESSION_CACHE.write() {
            cache.time = Some(Instant::now() - CACHE_TTL - Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
impl Drop for SessionCacheGuard {
    fn drop(&mut self) {
        let mut cache = SESSION_CACHE.write();
        if let Ok(cache) = cache.as_mut() {
            cache.data = self.prev_data.take();
            cache.time = self.prev_time;
            cache.refresh_id = self.prev_refresh_id;
            cache.outcome = self.prev_outcome;
        }
        if self.forced_snapshot {
            FORCED_SESSION_CACHE_GUARDS.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// [`SessionCacheGuard`] for [`PANE_META_CACHE`].
#[cfg(test)]
pub(crate) struct PaneMetaCacheGuard {
    prev_data: Option<std::sync::Arc<HashMap<String, PaneMetadata>>>,
    prev_time: Option<Instant>,
    prev_refresh_id: u64,
}

#[cfg(test)]
impl PaneMetaCacheGuard {
    pub(crate) fn capture() -> Self {
        let cache = PANE_META_CACHE.read().expect("pane meta cache lock");
        Self {
            prev_data: cache.data.clone(),
            prev_time: cache.time,
            prev_refresh_id: cache.refresh_id,
        }
    }

    pub(crate) fn force_failed_refresh(&self) {
        if let Ok(mut cache) = PANE_META_CACHE.write() {
            cache.data = None;
            cache.time = Some(Instant::now());
        }
    }
    pub(crate) fn force_stale(&self) {
        if let Ok(mut cache) = PANE_META_CACHE.write() {
            cache.time = Some(Instant::now() - CACHE_TTL - Duration::from_secs(1));
        }
    }
}
#[cfg(test)]
impl Drop for PaneMetaCacheGuard {
    fn drop(&mut self) {
        if let Ok(mut cache) = PANE_META_CACHE.write() {
            cache.data = self.prev_data.take();
            cache.time = self.prev_time;
            cache.refresh_id = self.prev_refresh_id;
        }
    }
}

/// Test-only holder of [`AGENT_PROBE_LOCK`] plus the worker blocked on it; drop
/// releases then joins. Declare after [`AgentAvailabilityGuard`].
#[cfg(test)]
pub(crate) struct BlockedProbeWorker<'a> {
    guard: Option<std::sync::MutexGuard<'a, ()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(test)]
impl<'a> BlockedProbeWorker<'a> {
    pub(crate) fn new(
        guard: std::sync::MutexGuard<'a, ()>,
        handle: std::thread::JoinHandle<()>,
    ) -> Self {
        Self {
            guard: Some(guard),
            handle: Some(handle),
        }
    }

    pub(crate) fn release_and_join(&mut self) {
        drop(self.guard.take());
        if let Some(handle) = self.handle.take() {
            handle.join().expect("probe worker");
        }
    }
}

#[cfg(test)]
impl Drop for BlockedProbeWorker<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Test-only guard that restores [`AGENT_AVAILABILITY`] on drop.
#[cfg(test)]
pub(crate) struct AgentAvailabilityGuard {
    prev: Option<HashMap<String, (bool, std::time::Instant)>>,
}

#[cfg(test)]
impl AgentAvailabilityGuard {
    pub(crate) fn capture() -> Self {
        Self {
            prev: AGENT_AVAILABILITY
                .read()
                .expect("agent availability lock")
                .clone(),
        }
    }

    pub(crate) fn seed(&self, agent: &str, available: bool) {
        if let Ok(mut cache) = AGENT_AVAILABILITY.write() {
            cache
                .get_or_insert_with(HashMap::new)
                .insert(agent.to_string(), (available, std::time::Instant::now()));
        }
    }

    pub(crate) fn age_past_ttl(&self, agent: &str) {
        use std::time::{Duration, Instant};
        if let Ok(mut cache) = AGENT_AVAILABILITY.write() {
            if let Some(map) = cache.as_mut() {
                if let Some(entry) = map.get_mut(agent) {
                    entry.1 = Instant::now()
                        .checked_sub(AGENT_AVAILABILITY_TTL + Duration::from_secs(1))
                        .unwrap_or(Instant::now());
                }
            }
        }
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut cache) = AGENT_AVAILABILITY.write() {
            *cache = None;
        }
    }

    pub(crate) fn is_populated(&self) -> bool {
        AGENT_AVAILABILITY
            .read()
            .ok()
            .and_then(|c| c.as_ref().map(|m| !m.is_empty()))
            .unwrap_or(false)
    }
}

#[cfg(test)]
impl Drop for AgentAvailabilityGuard {
    fn drop(&mut self) {
        if let Ok(mut cache) = AGENT_AVAILABILITY.write() {
            *cache = self.prev.take();
        }
    }
}

/// How long a [`SESSION_CACHE`] snapshot is trusted.
const CACHE_TTL: Duration = Duration::from_secs(2);

pub fn session_exists_from_cache(name: &str) -> Option<bool> {
    let cache = SESSION_CACHE.read().ok()?;

    if cache.time.map(|t| t.elapsed() > CACHE_TTL).unwrap_or(true)
        || cache.outcome == SessionCacheRefresh::Unknown
    {
        return None;
    }

    cache.data.as_ref().map(|m| m.contains_key(name))
}

/// Cached `#{session_activity}` for `name`, ignoring the TTL (an age hint only).
pub fn session_activity(name: &str) -> Option<i64> {
    let cache = SESSION_CACHE.read().ok()?;
    cache
        .data
        .as_ref()?
        .get(name)
        .map(|session| session.activity)
}

/// Session existence that keeps "tmux unreachable" apart from "absent";
/// `Unknown` means don't act.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionExistence {
    Present,
    Absent,
    /// No-server responses and query failures; not evidence of absence.
    Unknown,
}

/// `None` when the snapshot is stale or the lock poisoned.
fn session_existence_from_cache(name: &str) -> Option<SessionExistence> {
    let cache = SESSION_CACHE.read().ok()?;

    let fresh = cache
        .time
        .map(|t| t.elapsed() <= CACHE_TTL)
        .unwrap_or(false);
    if !fresh {
        return None;
    }
    if cache.outcome == SessionCacheRefresh::Unknown {
        return Some(SessionExistence::Unknown);
    }

    Some(match &cache.data {
        Some(map) if map.contains_key(name) => SessionExistence::Present,
        Some(_) => SessionExistence::Absent,
        None => SessionExistence::Unknown,
    })
}
pub(crate) fn cached_session_existence(name: &str) -> SessionExistence {
    session_existence_from_cache(name).unwrap_or(SessionExistence::Unknown)
}
/// Existence from the cache, refreshing a stale snapshot once.
pub fn probe_session_existence(name: &str) -> SessionExistence {
    if let Some(existence) = session_existence_from_cache(name) {
        return existence;
    }
    refresh_session_cache();
    session_existence_from_cache(name).unwrap_or(SessionExistence::Unknown)
}

/// Trusts a cache hit, but a miss (a session may be newer than the scan) falls
/// through to a live `has-session`.
pub fn session_exists(name: &str) -> bool {
    if session_exists_from_cache(name) == Some(true) {
        return true;
    }

    let mut command = tmux_command();
    command.args(["has-session", "-t", name]);
    run_tmux_command_with_timeout(&mut command)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Render-path liveness from the snapshot only, never forking. A new session
/// may read absent until the background poller refreshes.
pub fn session_exists_for_display(name: &str) -> bool {
    SESSION_CACHE
        .read()
        .ok()
        .and_then(|cache| cache.data.as_ref().map(|map| map.contains_key(name)))
        .unwrap_or(false)
}

/// Render-path pane-dead from the `list-panes` snapshot; unknown reads as not
/// dead.
pub fn pane_dead_for_display(name: &str) -> bool {
    pane_dead_from_cache(name).unwrap_or(false)
}

/// `None` only when stale or poisoned; a fresh failed snapshot answers `false`.
fn pane_dead_from_cache(name: &str) -> Option<bool> {
    let cache = PANE_META_CACHE.read().ok()?;
    if cache.time.map(|t| t.elapsed() > CACHE_TTL).unwrap_or(true) {
        return None;
    }
    Some(
        cache
            .data
            .as_ref()
            .and_then(|map| map.get(name))
            .is_some_and(|meta| meta.pane_dead),
    )
}

/// Stamped even on failure so an outage costs one fork per cycle. `taken_at`
/// must predate the fork so a stalled listing cannot pose as fresher.
fn publish_pane_meta_cache(
    refresh_id: u64,
    data: Option<std::sync::Arc<HashMap<String, PaneMetadata>>>,
    taken_at: Instant,
) -> bool {
    let Ok(mut cache) = PANE_META_CACHE.write() else {
        return false;
    };
    if refresh_id <= cache.refresh_id {
        return false;
    }
    cache.data = data;
    cache.time = Some(taken_at);
    cache.refresh_id = refresh_id;
    true
}

pub(crate) fn refresh_pane_meta_cache(
) -> anyhow::Result<std::sync::Arc<HashMap<String, PaneMetadata>>> {
    let refresh_id = next_refresh_id(&PANE_META_REFRESH_ID);
    let taken_at = Instant::now();
    let result = batch_pane_metadata().map(std::sync::Arc::new);
    if publish_pane_meta_cache(refresh_id, result.as_ref().ok().cloned(), taken_at) {
        return result;
    }
    PANE_META_CACHE
        .read()
        .ok()
        .and_then(|cache| cache.data.clone())
        .ok_or_else(|| anyhow::anyhow!("a newer pane metadata refresh is unavailable"))
}

fn snapshot_refresh_due(last_refresh: Option<Instant>) -> bool {
    last_refresh.is_none_or(|at| at.elapsed() >= CACHE_TTL / 2)
}

fn session_snapshot_refresh_due() -> bool {
    SESSION_CACHE
        .read()
        .map_or(true, |cache| snapshot_refresh_due(cache.time))
}

pub(crate) fn refresh_session_cache_if_due() {
    if session_snapshot_refresh_due() {
        refresh_session_cache();
    }
}

fn pane_snapshot_refresh_due() -> bool {
    PANE_META_CACHE
        .read()
        .map_or(true, |cache| snapshot_refresh_due(cache.time))
}
/// A passive preview resize queued by paint, run by the passive-resize worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PassiveResizeIntent {
    pub session_id: String,
    pub session_name: String,
    pub cols: u16,
    pub rows: u16,
    /// The session the user is viewing; resized before other work.
    pub priority: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PassiveResizeWork {
    intent: PassiveResizeIntent,
    generation: u64,
}

/// A finished passive resize; render adopts the geometry or parks a decline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PassiveResizeDone {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
    /// Applied window rows (including chrome); `None` when declined or failed.
    pub applied_window_rows: Option<u16>,
    generation: u64,
}

/// Geometry in flight or awaiting adoption; suppresses identical re-queues.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PassiveResizeTicket {
    session_id: String,
    cols: u16,
    rows: u16,
    generation: u64,
}

static PASSIVE_RESIZE_INTENTS: Mutex<Vec<PassiveResizeWork>> = Mutex::new(Vec::new());
static PASSIVE_RESIZE_DONES: Mutex<Vec<PassiveResizeDone>> = Mutex::new(Vec::new());
static PASSIVE_RESIZE_IN_FLIGHT: Mutex<Vec<PassiveResizeTicket>> = Mutex::new(Vec::new());
static PASSIVE_RESIZE_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);
static PASSIVE_RESIZE_WORKER_THREAD: OnceLock<std::thread::Thread> = OnceLock::new();
#[cfg(test)]
thread_local! {
    static PASSIVE_RESIZE_EXECUTION_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Keep one queued resize per session (the latest); an identical in-flight
/// geometry is ignored.
fn queue_latest_passive_resize(
    queue: &mut Vec<PassiveResizeWork>,
    in_flight: &[PassiveResizeTicket],
    work: PassiveResizeWork,
) {
    let intent = &work.intent;
    queue.retain(|prev| prev.intent.session_id != intent.session_id);
    if in_flight.iter().any(|active| {
        active.session_id == intent.session_id
            && active.cols == intent.cols
            && active.rows == intent.rows
    }) {
        return;
    }
    if work.intent.priority {
        let at = queue
            .iter()
            .position(|prev| !prev.intent.priority)
            .unwrap_or(queue.len());
        queue.insert(at, work);
    } else {
        queue.push(work);
    }
}

/// Non-blocking (called from paint); spawns the worker on first use.
pub(crate) fn queue_passive_resize(intent: PassiveResizeIntent) {
    spawn_passive_resize_worker();
    {
        let mut queue = PASSIVE_RESIZE_INTENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let work = PassiveResizeWork {
            intent,
            generation: PASSIVE_RESIZE_GENERATION
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        };
        let in_flight = PASSIVE_RESIZE_IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue_latest_passive_resize(&mut queue, &in_flight, work);
    }
    if let Some(thread) = PASSIVE_RESIZE_WORKER_THREAD.get() {
        thread.unpark();
    }
}

fn remove_pending_passive_resize(queue: &mut Vec<PassiveResizeWork>, session_id: &str) {
    queue.retain(|work| work.intent.session_id != session_id);
}

/// Drop queued geometry once render sees the wanted size already applied.
pub(crate) fn cancel_pending_passive_resize(session_id: &str) {
    let mut queue = PASSIVE_RESIZE_INTENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    remove_pending_passive_resize(&mut queue, session_id);
}

/// Drain completions, releasing in-flight entries only when still current.
fn take_current_passive_completions(
    in_flight: &mut Vec<PassiveResizeTicket>,
    dones: Vec<PassiveResizeDone>,
) -> Vec<PassiveResizeDone> {
    let current: Vec<_> = dones
        .into_iter()
        .filter(|done| {
            in_flight
                .iter()
                .any(|active| active.generation == done.generation)
        })
        .collect();
    in_flight.retain(|active| {
        !current
            .iter()
            .any(|done| active.generation == done.generation)
    });
    current
}

pub(crate) fn take_passive_resize_dones() -> Vec<PassiveResizeDone> {
    let dones = {
        let mut slot = PASSIVE_RESIZE_DONES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *slot)
    };
    let mut in_flight = PASSIVE_RESIZE_IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    take_current_passive_completions(&mut in_flight, dones)
}

/// Run one resize under tmux's atomic guard; a refusal completes as declined
/// so render parks it.
fn execute_passive_resize(work: &PassiveResizeWork) -> PassiveResizeDone {
    let intent = &work.intent;
    let deadline = TmuxCommandDeadline::new();
    let session = Session::from_name(&intent.session_name);
    let applied_window_rows = if session.exists_with_deadline(&deadline) {
        session.resize_window_if_detached_without_active_owner_after_exists_with_deadline(
            intent.cols,
            intent.rows,
            &deadline,
        )
    } else {
        None
    };
    PassiveResizeDone {
        session_id: intent.session_id.clone(),
        cols: intent.cols,
        rows: intent.rows,
        applied_window_rows,
        generation: work.generation,
    }
}

fn publish_latest_passive_resize_done(dones: &mut Vec<PassiveResizeDone>, done: PassiveResizeDone) {
    dones.retain(|previous| previous.session_id != done.session_id);
    dones.push(done);
}

/// One at a time, so a priority intent queued mid-drain goes next.
fn take_next_passive_resize() -> Option<PassiveResizeWork> {
    let mut queue = PASSIVE_RESIZE_INTENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if queue.is_empty() {
        None
    } else {
        Some(queue.remove(0))
    }
}

fn execute_passive_resizes() {
    #[cfg(test)]
    PASSIVE_RESIZE_EXECUTION_COUNT.with(|count| count.set(count.get() + 1));
    while let Some(work) = take_next_passive_resize() {
        {
            let intent = &work.intent;
            let mut in_flight = PASSIVE_RESIZE_IN_FLIGHT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            in_flight.retain(|active| active.session_id != intent.session_id);
            in_flight.push(PassiveResizeTicket {
                session_id: intent.session_id.clone(),
                cols: intent.cols,
                rows: intent.rows,
                generation: work.generation,
            });
        }
        let done = execute_passive_resize(&work);
        let mut dones = PASSIVE_RESIZE_DONES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publish_latest_passive_resize_done(&mut dones, done);
    }
}
fn clear_all_passive_resizes_in_flight() {
    PASSIVE_RESIZE_IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

fn spawn_passive_resize_worker() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    let spawn_result = std::thread::Builder::new()
        .name("aoe-passive-resize".to_string())
        .spawn(|| {
            let _ = PASSIVE_RESIZE_WORKER_THREAD.set(std::thread::current());
            loop {
                if std::panic::catch_unwind(execute_passive_resizes).is_err() {
                    clear_all_passive_resizes_in_flight();
                    tracing::error!(
                        target: "tmux.cache",
                        "passive resize worker cycle panicked; retrying"
                    );
                }
                std::thread::park();
            }
        });
    if let Err(error) = spawn_result {
        STARTED.store(false, std::sync::atomic::Ordering::Release);
        tracing::warn!(
            target: "tmux.cache",
            %error,
            "failed to spawn passive resize worker; a later call may retry"
        );
    }
}

fn refresh_display_snapshots() {
    refresh_session_cache_if_due();
    if pane_snapshot_refresh_due() {
        let _ = refresh_pane_meta_cache();
    }
}
/// Keep the session and pane snapshots fresh so display helpers never fork
/// from paint. Idempotent; a panicking cycle is logged and retried.
pub fn spawn_snapshot_poller() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    let spawn_result = std::thread::Builder::new()
        .name("aoe-display-snapshot".to_string())
        .spawn(|| loop {
            let cycle = std::panic::catch_unwind(refresh_display_snapshots);
            if cycle.is_err() {
                tracing::error!(
                    target: "tmux.cache",
                    "display snapshot poller cycle panicked; retrying"
                );
            }
            // Half the TTL so a snapshot never expires within a cycle.
            std::thread::park_timeout(CACHE_TTL / 2);
        });
    if let Err(error) = spawn_result {
        STARTED.store(false, std::sync::atomic::Ordering::Release);
        tracing::warn!(
            target: "tmux.cache",
            %error,
            "failed to spawn display snapshot poller; a later call may retry"
        );
    }
}
pub fn get_current_session_name() -> Option<String> {
    let output = tmux_query_command()
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .ok()?;

    if output.status.success() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

pub fn is_tmux_available() -> bool {
    tmux_command().arg("-V").output().is_ok()
}

/// Whether `binary` resolves on PATH, falling back to a login shell for
/// version-manager PATHs.
pub(crate) fn is_binary_on_path(binary: &str) -> bool {
    if binary.contains('/') || binary.contains('\\') {
        return std::path::Path::new(binary).exists();
    }
    let direct = Command::new("which")
        .arg(binary)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if direct {
        return true;
    }
    let shell = crate::session::user_shell();
    Command::new(&shell)
        .args(["-lc", &format!("which {}", shell_words::quote(binary))])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

const AGENT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LOGIN_SHELL_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn agent_probe_output(
    command: &mut Command,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    match crate::process::run_with_timeout_process_group(
        command.stdin(std::process::Stdio::null()),
        timeout,
    ) {
        Ok(output) => {
            if output.is_none() {
                tracing::warn!(
                    program = ?command.get_program(),
                    timeout_s = timeout.as_secs(),
                    "agent availability probe timed out; process group terminated"
                );
            }
            output
        }
        Err(_) => None,
    }
}

fn agent_available_direct(agent: &crate::agents::AgentDef) -> Option<bool> {
    use crate::agents::DetectionMethod;
    match &agent.detection {
        DetectionMethod::Which(binary) => {
            if binary.contains('/') || binary.contains('\\') {
                return Some(std::path::Path::new(binary).exists());
            }
            let found = agent_probe_output(Command::new("which").arg(binary), AGENT_PROBE_TIMEOUT)
                .is_some_and(|output| output.status.success());
            if found {
                Some(true)
            } else {
                None
            }
        }
        DetectionMethod::RunWithArg(binary, arg) => {
            let ok = agent_probe_output(Command::new(binary).arg(arg), AGENT_PROBE_TIMEOUT)
                .is_some_and(|output| output.status.success());
            if ok {
                Some(true)
            } else {
                None
            }
        }
    }
}

/// Chained per-agent probes; hits print `AOE_AGENT_OK <name>` among whatever
/// the login shell prints.
fn login_shell_probe_script(agents: &[&crate::agents::AgentDef]) -> String {
    use crate::agents::DetectionMethod;
    agents
        .iter()
        .map(|agent| {
            let probe = match &agent.detection {
                DetectionMethod::Which(binary) => {
                    format!("which {}", shell_words::quote(binary))
                }
                DetectionMethod::RunWithArg(binary, arg) => {
                    format!("{} {}", shell_words::quote(binary), shell_words::quote(arg))
                }
            };
            format!(
                "{} >/dev/null 2>&1 && echo {} {}",
                probe,
                LOGIN_PROBE_MARKER,
                shell_words::quote(agent.name)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

const LOGIN_PROBE_MARKER: &str = "AOE_AGENT_OK";

fn parse_login_shell_probe(stdout: &str) -> std::collections::HashSet<String> {
    stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix(LOGIN_PROBE_MARKER))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// One login shell for all agents, since profile startup is slow.
fn login_shell_probe(agents: &[&crate::agents::AgentDef]) -> std::collections::HashSet<String> {
    if agents.is_empty() {
        return std::collections::HashSet::new();
    }
    let shell = crate::session::user_shell();
    agent_probe_output(
        Command::new(&shell).args(["-lc", &login_shell_probe_script(agents)]),
        LOGIN_SHELL_PROBE_TIMEOUT,
    )
    .map(|o| parse_login_shell_probe(&String::from_utf8_lossy(&o.stdout)))
    .unwrap_or_default()
}

/// Process-wide positive and negative availability cache. Startup warms it for
/// settings and API callers; TTL expiry and Recheck refresh external installations.
static AGENT_AVAILABILITY: RwLock<Option<HashMap<String, (bool, std::time::Instant)>>> =
    RwLock::new(None);

const AGENT_AVAILABILITY_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Serialize population across concurrent cold callers, without blocking fresh cache hits.
static AGENT_PROBE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
thread_local! {
    static AGENT_PROBE_MISS_GATE: std::cell::RefCell<Option<(
        std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>,
    )>> = const { std::cell::RefCell::new(None) };
    static AGENT_PROBE_LOCK_CONTENDED: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

fn lock_agent_probe() -> std::sync::MutexGuard<'static, ()> {
    #[cfg(test)]
    if let Some(contended) = AGENT_PROBE_LOCK_CONTENDED.with(|slot| slot.borrow_mut().take()) {
        return crate::session::test_support::lock_reporting_contention(&AGENT_PROBE_LOCK, || {
            contended.send(()).expect("contention observer alive")
        })
        .unwrap_or_else(|e| e.into_inner());
    }
    AGENT_PROBE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clear the memo after any in-flight probe finishes (probe lock, then memo).
pub(crate) fn invalidate_agent_availability() {
    let _probe_guard = lock_agent_probe();
    if let Ok(mut cache) = AGENT_AVAILABILITY.write() {
        *cache = None;
    }
}

fn partition_cached_agents<'a>(
    agents: &[&'a crate::agents::AgentDef],
) -> (HashSet<String>, Vec<&'a crate::agents::AgentDef>) {
    let mut found = HashSet::new();
    let mut uncached = Vec::new();
    let cache = AGENT_AVAILABILITY.read().ok();
    let cached = cache.as_ref().and_then(|c| c.as_ref());
    for agent in agents {
        match cached.and_then(|c| c.get(agent.name)) {
            Some((true, at)) if at.elapsed() < AGENT_AVAILABILITY_TTL => {
                found.insert(agent.name.to_string());
            }
            Some((false, at)) if at.elapsed() < AGENT_AVAILABILITY_TTL => {}
            _ => uncached.push(*agent),
        }
    }
    (found, uncached)
}

/// Memoized availability; the uncached set costs at most one login shell.
pub(crate) fn probe_agents_available(
    agents: &[&crate::agents::AgentDef],
) -> std::collections::HashSet<String> {
    let (mut found, uncached) = partition_cached_agents(agents);
    if uncached.is_empty() {
        return found;
    }

    #[cfg(test)]
    if let Some((entered, resume)) = AGENT_PROBE_MISS_GATE.with(|gate| gate.borrow_mut().take()) {
        let _ = entered.send(());
        let _ = resume.recv();
    }

    // A queued caller must consume the preceding probe's publication before starting another.
    let _probe_guard = AGENT_PROBE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (already_found, uncached) = partition_cached_agents(&uncached);
    found.extend(already_found);
    if uncached.is_empty() {
        return found;
    }

    let mut results: Vec<(&str, bool)> = Vec::new();
    let mut needs_shell: Vec<&crate::agents::AgentDef> = Vec::new();
    for agent in uncached {
        match agent_available_direct(agent) {
            Some(ok) => results.push((agent.name, ok)),
            None => needs_shell.push(agent),
        }
    }
    let shell_found = login_shell_probe(&needs_shell);
    for agent in needs_shell {
        results.push((agent.name, shell_found.contains(agent.name)));
    }

    if let Ok(mut cache) = AGENT_AVAILABILITY.write() {
        let map = cache.get_or_insert_with(HashMap::new);
        let now = std::time::Instant::now();
        for (name, ok) in &results {
            map.insert((*name).to_string(), (*ok, now));
        }
    }
    for (name, ok) in results {
        if ok {
            found.insert(name.to_string());
        }
    }
    found
}

pub(crate) fn is_agent_available(agent: &crate::agents::AgentDef) -> bool {
    probe_agents_available(&[agent]).contains(agent.name)
}

#[derive(Debug, Clone)]
pub struct AvailableTools {
    available: Vec<String>,
}

impl AvailableTools {
    pub fn detect() -> Self {
        // One batched probe warms the memo for later per-agent callers.
        let agents = crate::agents::AGENTS;
        let refs: Vec<&crate::agents::AgentDef> = agents.iter().collect();
        let found = probe_agents_available(&refs);
        let mut available: Vec<String> = agents
            .iter()
            .filter(|a| found.contains(a.name))
            .map(|a| a.name.to_string())
            .collect();

        // Custom agents always count as available (they may target a wrapper).
        if let Ok(config) = crate::session::config::Config::load() {
            config.session.warn_custom_agent_issues();
            let mut custom: Vec<_> = config
                .session
                .custom_agents
                .keys()
                .filter(|name| !name.is_empty() && !available.iter().any(|n| n == *name))
                .cloned()
                .collect();
            custom.sort();
            available.extend(custom);
        }

        Self { available }
    }

    pub fn any_available(&self) -> bool {
        !self.available.is_empty()
    }

    pub fn available_list(&self) -> &[String] {
        &self.available
    }

    #[cfg(test)]
    pub fn with_tools(tools: &[&str]) -> Self {
        Self {
            available: tools.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tmux::test_helpers::require_tmux;
    #[test]
    #[serial_test::serial]
    fn invalidate_agent_availability_waits_for_an_in_flight_probe() {
        let memo = AgentAvailabilityGuard::capture();
        memo.seed(crate::agents::AGENTS[0].name, false);
        let (contended_tx, contended_rx) = std::sync::mpsc::channel();
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let mut worker = BlockedProbeWorker::new(
            AGENT_PROBE_LOCK.lock().unwrap_or_else(|e| e.into_inner()),
            std::thread::spawn(move || {
                AGENT_PROBE_LOCK_CONTENDED.with(|slot| *slot.borrow_mut() = Some(contended_tx));
                invalidate_agent_availability();
                returned_tx.send(()).expect("test receiver alive");
            }),
        );

        contended_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("invalidator must observe the held probe lock");
        assert!(matches!(
            returned_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(
            memo.is_populated(),
            "memo survives the contested lock decision"
        );

        worker.release_and_join();
        returned_rx.recv().expect("invalidate completes");
        assert!(
            !memo.is_populated(),
            "invalidate clears after the probe releases its lock"
        );
    }

    #[cfg(unix)]
    fn run_probe_test_in_subprocess() -> bool {
        const CHILD_ENV: &str = "AOE_AGENT_PROBE_TEST_CHILD";
        let thread = std::thread::current();
        let test = thread.name().expect("named test thread");
        if std::env::var_os(CHILD_ENV).as_deref() == Some(std::ffi::OsStr::new(test)) {
            return false;
        }
        let home = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env_clear()
            .env(CHILD_ENV, test)
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::null());
        let output = crate::process::run_with_timeout_process_group(
            &mut command,
            std::time::Duration::from_secs(60),
        )
        .unwrap()
        .expect("isolated probe test timed out");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    #[cfg(unix)]
    fn probe_environment(home: &std::path::Path) -> crate::session::test_support::EnvGuard {
        use std::os::unix::fs::PermissionsExt;
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = shell_words::quote(home.join("probes").to_str().unwrap()).into_owned();
        for (name, script) in [
            (
                "which",
                format!("#!/bin/sh\nprintf 'which:%s\\n' \"$1\" >> {log}\n[ \"$1\" = claude ]\n"),
            ),
            (
                "vibe",
                format!("#!/bin/sh\nprintf 'version\\n' >> {log}\nexit 1\n"),
            ),
            (
                "login-shell",
                format!("#!/bin/sh\nprintf 'login\\n' >> {log}\nexit 0\n"),
            ),
        ] {
            let path = bin.join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        crate::session::test_support::EnvGuard::set(&[
            ("HOME", home.to_path_buf()),
            ("XDG_CONFIG_HOME", home.join(".config")),
            ("XDG_DATA_HOME", home.join(".local/share")),
            ("PATH", bin.clone()),
            ("SHELL", bin.join("login-shell")),
        ])
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn queued_agent_probe_reuses_results_published_after_its_cache_miss() {
        if run_probe_test_in_subprocess() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let _env = probe_environment(home.path());
        let memo = AgentAvailabilityGuard::capture();
        memo.clear();
        let agents = [
            crate::agents::get_agent("claude").unwrap(),
            crate::agents::get_agent("vibe").unwrap(),
        ];
        std::thread::scope(|scope| {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            let queued = scope.spawn(move || {
                AGENT_PROBE_MISS_GATE
                    .with(|gate| *gate.borrow_mut() = Some((entered_tx, resume_rx)));
                probe_agents_available(&agents)
            });
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let found = probe_agents_available(&agents);
            assert_eq!(found, HashSet::from(["claude".to_owned()]));
            let completed_probes = std::fs::read_to_string(home.path().join("probes")).unwrap();
            drop(resume_tx);
            assert_eq!(queued.join().unwrap(), found);
            assert_eq!(
                std::fs::read_to_string(home.path().join("probes")).unwrap(),
                completed_probes,
                "the queued caller must start no redundant external probes"
            );
        });
    }

    use super::test_helpers::TmuxTestSession;
    use super::*;

    const P: &str = SESSION_PREFIX;

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn agent_availability_reuses_warm_results_and_expires_both_polarities() {
        if run_probe_test_in_subprocess() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let _env = probe_environment(home.path());
        let memo = AgentAvailabilityGuard::capture();
        memo.clear();
        let agents = [
            crate::agents::get_agent("claude").unwrap(),
            crate::agents::get_agent("vibe").unwrap(),
        ];
        let expected = HashSet::from(["claude".to_owned()]);
        assert_eq!(probe_agents_available(&agents), expected);
        let mut probes = std::fs::read_to_string(home.path().join("probes")).unwrap();
        assert_eq!(probes, "which:claude\nversion\nlogin\n");
        assert_eq!(probe_agents_available(&agents), expected);
        assert_eq!(
            std::fs::read_to_string(home.path().join("probes")).unwrap(),
            probes
        );
        for (name, added) in [("vibe", "version\nlogin\n"), ("claude", "which:claude\n")] {
            memo.age_past_ttl(name);
            assert_eq!(probe_agents_available(&agents), expected);
            probes.push_str(added);
            assert_eq!(
                std::fs::read_to_string(home.path().join("probes")).unwrap(),
                probes
            );
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[serial_test::serial]
    fn timed_out_agent_probes_release_waiters_and_invalidation() {
        if run_probe_test_in_subprocess() {
            return;
        }
        use std::time::{Duration, Instant};
        let diagnostics = tempfile::tempdir().unwrap();
        let diagnostics_path = diagnostics.path().join("timeouts.log");
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(std::fs::File::create(&diagnostics_path).unwrap())
            .try_init()
            .unwrap();
        struct ReleaseProbe(std::path::PathBuf);
        impl Drop for ReleaseProbe {
            fn drop(&mut self) {
                let _ = std::fs::write(&self.0, "release");
            }
        }
        for (executable, timeout) in [
            ("vibe", AGENT_PROBE_TIMEOUT),
            ("login-shell", LOGIN_SHELL_PROBE_TIMEOUT),
        ] {
            let home = tempfile::tempdir().unwrap();
            let _env = probe_environment(home.path());
            let memo = AgentAvailabilityGuard::capture();
            memo.clear();
            let release = home.path().join("release");
            let ready = home.path().join("ready");
            let wait = format!(
                "while [ ! -e {} ]; do /bin/sleep 0.02; done",
                shell_words::quote(release.to_str().unwrap())
            );
            let script = format!(
                "#!/bin/sh\n/bin/sh -c {} &\nchild=$!\ntrap 'kill \"$child\" 2>/dev/null; wait \"$child\" 2>/dev/null; exit 1' TERM\nprintf '%s %s\\n' \"$$\" \"$child\" > {}\nwait \"$child\"\nexit 1\n",
                shell_words::quote(&wait), shell_words::quote(ready.to_str().unwrap()),
            );
            std::fs::write(home.path().join("bin").join(executable), script).unwrap();
            std::thread::scope(|scope| {
                let _release = ReleaseProbe(release);
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let holder_tx = done_tx.clone();
                scope.spawn(move || {
                    let found =
                        probe_agents_available(&[crate::agents::get_agent("vibe").unwrap()]);
                    let _ = holder_tx.send(("holder", Some(found)));
                });
                let deadline = Instant::now() + Duration::from_secs(5);
                while !ready.exists() {
                    assert!(Instant::now() < deadline, "probe child never started");
                    std::thread::sleep(Duration::from_millis(5));
                }
                let waiter_tx = done_tx.clone();
                scope.spawn(move || {
                    let found =
                        probe_agents_available(&[crate::agents::get_agent("claude").unwrap()]);
                    let _ = waiter_tx.send(("waiter", Some(found)));
                });
                scope.spawn(move || {
                    invalidate_agent_availability();
                    let _ = done_tx.send(("invalidator", None));
                });
                let deadline = Instant::now() + timeout * 2 + Duration::from_secs(2);
                let mut completed = Vec::new();
                for _ in 0..3 {
                    let (name, found) = done_rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .expect("a hung probe must not indefinitely hold callers or Recheck");
                    match name {
                        "holder" => assert!(found.unwrap().is_empty()),
                        "waiter" => {
                            assert_eq!(found.unwrap(), HashSet::from(["claude".to_owned()]))
                        }
                        "invalidator" => {}
                        _ => unreachable!(),
                    }
                    completed.push(name);
                }
                completed.sort_unstable();
                assert_eq!(completed, ["holder", "invalidator", "waiter"]);
                for (index, pid) in std::fs::read_to_string(&ready)
                    .unwrap()
                    .split_whitespace()
                    .enumerate()
                {
                    let output = Command::new("/bin/ps")
                        .args(["-o", "stat=", "-p", pid])
                        .output()
                        .unwrap();
                    let state = String::from_utf8_lossy(&output.stdout);
                    assert!(
                        state.trim().is_empty()
                            || (index > 0 && state.trim_start().starts_with('Z')),
                        "probe process {pid} remains alive: {state}"
                    );
                }
            });
        }
        let diagnostics = std::fs::read_to_string(diagnostics_path).unwrap();
        for timeout in [AGENT_PROBE_TIMEOUT, LOGIN_SHELL_PROBE_TIMEOUT] {
            assert!(
                diagnostics.lines().any(|line| {
                    line.contains("WARN")
                        && line.contains("program=")
                        && line.contains(&format!("timeout_s={}", timeout.as_secs()))
                }),
                "missing timeout diagnostic: {diagnostics}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn snapshot_refresh_cycle_excludes_passive_resizes() {
        let before = PASSIVE_RESIZE_EXECUTION_COUNT.with(std::cell::Cell::get);
        refresh_display_snapshots();
        let after = PASSIVE_RESIZE_EXECUTION_COUNT.with(std::cell::Cell::get);
        assert_eq!(
            after, before,
            "snapshot refresh must not execute deadline-bound passive work",
        );
    }
    #[test]
    fn passive_resize_queue_keeps_latest_intent_per_session() {
        let work = |generation, session_id: &str, cols, rows| PassiveResizeWork {
            intent: PassiveResizeIntent {
                session_id: session_id.to_string(),
                session_name: format!("aoe_test_{session_id}"),
                cols,
                rows,
                priority: false,
            },
            generation,
        };
        let mut queue = Vec::new();
        queue_latest_passive_resize(&mut queue, &[], work(1, "a", 80, 24));
        queue_latest_passive_resize(&mut queue, &[], work(2, "b", 90, 30));
        queue_latest_passive_resize(&mut queue, &[], work(3, "a", 120, 40));

        assert_eq!(queue.len(), 2, "one bounded slot per session");
        assert_eq!(
            (queue[0].intent.session_id.as_str(), queue[0].intent.cols),
            ("b", 90)
        );
        assert_eq!(
            (queue[1].intent.session_id.as_str(), queue[1].intent.cols),
            ("a", 120)
        );

        let mut viewed = work(4, "sel", 100, 30);
        viewed.intent.priority = true;
        queue_latest_passive_resize(&mut queue, &[], viewed);
        assert_eq!(queue[0].intent.session_id, "sel");
        assert_eq!(queue.len(), 3);

        let in_flight = vec![PassiveResizeTicket {
            session_id: "a".to_string(),
            cols: 120,
            rows: 40,
            generation: 4,
        }];
        let mut while_running = Vec::new();
        queue_latest_passive_resize(&mut while_running, &in_flight, work(5, "a", 120, 40));
        let mut completed_then_in_sync = vec![work(6, "a", 140, 50)];
        remove_pending_passive_resize(&mut completed_then_in_sync, "a");
        assert!(
            completed_then_in_sync.is_empty(),
            "adopting the in-sync completion cancels stale queued geometry"
        );
        assert!(
            while_running.is_empty(),
            "identical in-flight resize is suppressed"
        );
        queue_latest_passive_resize(&mut while_running, &in_flight, work(7, "a", 140, 50));
        assert_eq!(
            (while_running[0].intent.cols, while_running[0].intent.rows),
            (140, 50)
        );
        queue_latest_passive_resize(&mut while_running, &in_flight, work(8, "a", 120, 40));
        assert!(
            while_running.is_empty(),
            "returning to the in-flight geometry drops stale queued geometry"
        );

        let old_done = PassiveResizeDone {
            session_id: "a".to_string(),
            cols: 120,
            rows: 40,
            applied_window_rows: Some(40),
            generation: 9,
        };
        let mut newer_same_geometry = vec![PassiveResizeTicket {
            session_id: "a".to_string(),
            cols: 120,
            rows: 40,
            generation: 10,
        }];
        let stale = take_current_passive_completions(&mut newer_same_geometry, vec![old_done]);
        assert!(stale.is_empty(), "stale completion must not reach render");
        assert_eq!(
            newer_same_geometry[0].generation, 10,
            "an old identical completion must not clear newer in-flight work"
        );
        let current_done = PassiveResizeDone {
            session_id: "a".to_string(),
            cols: 120,
            rows: 40,
            applied_window_rows: Some(40),
            generation: 10,
        };
        let current =
            take_current_passive_completions(&mut newer_same_geometry, vec![current_done]);
        assert_eq!(current[0].generation, 10);
        assert!(newer_same_geometry.is_empty());

        let mut published = Vec::new();
        publish_latest_passive_resize_done(&mut published, current[0].clone());
        publish_latest_passive_resize_done(
            &mut published,
            PassiveResizeDone {
                generation: 11,
                ..current[0].clone()
            },
        );
        publish_latest_passive_resize_done(
            &mut published,
            PassiveResizeDone {
                session_id: "b".to_string(),
                cols: 90,
                rows: 30,
                applied_window_rows: Some(30),
                generation: 12,
            },
        );
        assert_eq!(published.len(), 2, "one completion slot per session");
        assert_eq!(published[0].generation, 11);
    }

    #[test]
    #[serial_test::serial]
    fn failed_passive_resize_publishes_declined_completion() {
        const ID: &str = "resize_failure_id";
        const TITLE: &str = "Missing resize target";
        let name = Session::generate_name(ID, TITLE);
        let cache = SessionCacheGuard::capture();
        cache.force_stale();
        let intent = PassiveResizeWork {
            intent: PassiveResizeIntent {
                session_id: ID.to_string(),
                session_name: name.clone(),
                cols: 100,
                rows: 30,
                priority: false,
            },
            generation: 1,
        };
        let _ = fork_probe::take();
        let probe = fork_probe::arm();

        assert!(
            execute_passive_resize(&intent)
                .applied_window_rows
                .is_none(),
            "a failed or timed-out resize must complete as declined"
        );
        drop(probe);
        assert_eq!(
            fork_probe::take(),
            1,
            "a missing session must short-circuit before attachment and ownership probes",
        );
    }
    #[test]
    fn test_tmux_command_carries_socket_flag() {
        let cmd = tmux_command();
        let args: Vec<_> = cmd.get_args().map(|a| a.to_owned()).collect();
        assert_eq!(args.first().map(|a| a.to_str().unwrap()), Some("-S"));
        assert!(args.get(1).is_some(), "socket path arg present");
        assert_eq!(cmd.get_program().to_str(), Some("tmux"));
    }

    #[test]
    fn shared_snapshot_refresh_skips_fresh_scans() {
        assert!(snapshot_refresh_due(None));
        assert!(!snapshot_refresh_due(Some(Instant::now())));
        assert!(snapshot_refresh_due(Some(
            Instant::now() - CACHE_TTL / 2 - Duration::from_millis(1)
        )));
    }

    #[test]
    #[serial_test::serial]
    fn later_started_snapshot_publication_wins() {
        let _session_guard = SessionCacheGuard::capture_restore_only();
        let _pane_guard = PaneMetaCacheGuard::capture();

        let session_older = next_refresh_id(&SESSION_REFRESH_ID);
        let session_newer = next_refresh_id(&SESSION_REFRESH_ID);
        assert_eq!(
            publish_session_cache(
                session_newer,
                Some(HashMap::from([(
                    "new-session".to_string(),
                    LiveSession::unmarked(),
                )])),
                SessionCacheRefresh::Populated,
                false,
            ),
            SessionCacheRefresh::Populated,
        );
        assert_eq!(
            publish_session_cache(session_older, None, SessionCacheRefresh::NoServer, false,),
            SessionCacheRefresh::Populated,
            "the superseded caller must observe the newer committed outcome",
        );
        assert_eq!(session_exists_from_cache("new-session"), Some(true));
        assert_eq!(session_exists_from_cache("old-session"), Some(false));

        let pane_older = next_refresh_id(&PANE_META_REFRESH_ID);
        let pane_newer = next_refresh_id(&PANE_META_REFRESH_ID);
        assert!(publish_pane_meta_cache(
            pane_newer,
            Some(std::sync::Arc::new(HashMap::from([(
                "new-pane".to_string(),
                dead_pane_meta(true),
            )]))),
            Instant::now(),
        ));
        assert!(!publish_pane_meta_cache(
            pane_older,
            Some(std::sync::Arc::new(HashMap::from([(
                "old-pane".to_string(),
                dead_pane_meta(true),
            )]))),
            Instant::now(),
        ));
        assert_eq!(pane_dead_from_cache("new-pane"), Some(true));
        assert_eq!(pane_dead_from_cache("old-pane"), Some(false));
    }

    #[cfg(unix)]
    #[test]
    fn tmux_operation_deadline_rejects_a_second_budget() {
        let deadline = TmuxCommandDeadline::with_timeout(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        let mut command = Command::new("true");
        let error = deadline
            .run(&mut command)
            .expect_err("an expired operation must not start another command budget");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn tmux_query_command_preserves_ctype_for_session_names() {
        let command = tmux_query_command();
        let args: Vec<_> = command.get_args().map(|a| a.to_owned()).collect();
        assert!(
            args.iter().any(|a| a.to_str() == Some("-u")),
            "tmux -u forces UTF-8 names independently of inherited LC_CTYPE"
        );
        let message_locale = command
            .get_envs()
            .find(|(key, _)| key.to_str() == Some("LC_MESSAGES"))
            .and_then(|(_, value)| value.and_then(|value| value.to_str()));
        assert_eq!(message_locale, Some("C"));
        assert!(
            command
                .get_envs()
                .find(|(key, _)| key.to_str() == Some("LC_ALL"))
                .is_some_and(|(_, value)| value.is_none()),
            "LC_ALL must not override LC_MESSAGES=C"
        );
    }

    #[test]
    #[serial_test::serial]
    fn live_snapshot_warms_display_cache_without_second_fork() {
        let cache = SessionCacheGuard::capture_restore_only();
        cache.force_stale();
        let _ = fork_probe::take();
        let probe = fork_probe::arm();

        let snapshot = LiveSessionSnapshot::new();
        let _ = snapshot.names();
        refresh_session_cache_if_due();

        drop(probe);
        assert_eq!(
            fork_probe::take(),
            1,
            "startup liveness and display warmup must share one list-sessions observation",
        );
    }

    #[test]
    fn stop_aoe_sessions_counts_only_successful_kills() {
        let successful = format!("{P}unicode_会话");
        let failed = format!("{P}failed");
        let names = [successful.as_str(), "unrelated", failed.as_str()];
        let mut attempted = Vec::new();

        let killed = stop_aoe_sessions(names.into_iter(), |name| {
            attempted.push(name.to_string());
            name == successful
        });

        assert_eq!(attempted, [successful, failed]);
        assert_eq!(killed, 1);
    }

    #[cfg(unix)]
    #[test]
    fn tmux_command_timeout_kills_a_stalled_client() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;

        struct ClientCleanup(libc::pid_t);
        impl Drop for ClientCleanup {
            fn drop(&mut self) {
                unsafe {
                    if libc::waitpid(self.0, std::ptr::null_mut(), libc::WNOHANG) == 0 {
                        libc::kill(self.0, libc::SIGKILL);
                        libc::waitpid(self.0, std::ptr::null_mut(), 0);
                    }
                }
            }
        }

        let _env = crate::session::test_support::EnvGuard::read_lock();
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        unsafe {
            command.pre_exec(move || {
                let pid = libc::getpid().to_ne_bytes();
                if libc::write(writer.as_raw_fd(), pid.as_ptr().cast(), pid.len())
                    != pid.len() as isize
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let result = run_tmux_command_with_timeout_inner(&mut command, Duration::from_millis(10));
        let mut pid = [0; std::mem::size_of::<libc::pid_t>()];
        reader
            .read_exact(&mut pid)
            .expect("spawned client's identity");
        let client = ClientCleanup(libc::pid_t::from_ne_bytes(pid));
        let error = result.expect_err("stalled client must time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            unsafe { libc::kill(client.0, 0) },
            -1,
            "timed-out client is still alive or a zombie"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert_eq!(
            unsafe { libc::waitpid(client.0, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn test_tmux_socket_resolves_under_test() {
        assert!(
            matches!(tmux_socket(), Some(TmuxSocket::Path(_))),
            "unit tests must isolate onto an explicit socket path, not the default socket"
        );
    }

    #[test]
    fn socket_from_config_name_accepts_bare_names_only() {
        let named = |n: &str| Some(TmuxSocket::Name(n.to_string()));
        for (configured, want) in [
            (Some("aoe_work"), named("aoe_work")),
            (Some("  aoe_work  "), named("aoe_work")),
            (None, None),
            (Some(""), None),
            (Some("   "), None),
            (Some("/tmp/foo.sock"), None),
            (Some("a/b"), None),
            (Some("a\\b"), None),
        ] {
            let got = socket_from_config_name(configured.map(str::to_string));
            assert_eq!(got, want, "{configured:?}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn probe_session_existence_answers_from_the_fresh_cache() {
        let guard = SessionCacheGuard::capture();
        let name = format!("{P}probe_abc12345");

        guard.force_present(&[&name]);
        assert_eq!(probe_session_existence(&name), SessionExistence::Present);

        guard.force_present(&[&format!("{P}some_other_session")]);
        assert_eq!(probe_session_existence(&name), SessionExistence::Absent);

        guard.force_unreachable();
        assert_eq!(probe_session_existence(&name), SessionExistence::Unknown);
    }

    #[test]
    #[serial_test::serial]
    fn cached_session_existence_keeps_stale_snapshot_unknown() {
        let guard = SessionCacheGuard::capture();
        let name = format!("{P}cached_stale_abc12345");
        guard.force_present(&[&name]);
        guard.force_stale();

        assert_eq!(cached_session_existence(&name), SessionExistence::Unknown);
    }
    #[test]
    #[serial_test::serial]
    fn rekey_classification_treats_confirmed_no_server_as_absent() {
        let guard = SessionCacheGuard::capture();
        let id = "noserverdeadbeef";
        guard.force_unreachable();
        let session = Session::new(id, "derived").unwrap();
        assert_eq!(
            resolved_agent_existence(id, &session, SessionCacheRefresh::NoServer),
            SessionExistence::Absent
        );
        assert_eq!(
            resolved_agent_existence(id, &session, SessionCacheRefresh::Unknown),
            SessionExistence::Unknown
        );
    }

    #[test]
    #[serial_test::serial]
    fn ambiguous_live_names_are_unknown_not_confirmed_absence() {
        let guard = SessionCacheGuard::capture();
        let id = "ambig123deadbeef";
        let first = format!("{P}first_ambig123");
        let second = format!("{P}second_ambig123");
        guard.force_present(&[&first, &second]);
        let session = Session::new(id, "derived").unwrap();
        assert_eq!(
            resolved_agent_existence(id, &session, SessionCacheRefresh::Populated),
            SessionExistence::Unknown
        );
    }

    #[test]
    #[serial_test::serial]
    fn aux_shaped_live_derived_name_is_present_not_absent() {
        let guard = SessionCacheGuard::capture();
        let id = "auxshapedeadbeef";
        let session = Session::new(id, "term rewriting").unwrap();
        guard.force_present(&[session.name()]);
        assert_eq!(
            resolved_agent_existence(id, &session, SessionCacheRefresh::Populated),
            SessionExistence::Present
        );
    }

    const ID: &str = "abc12345deadbeef";
    const ID8: &str = "abc12345";

    #[test]
    fn resolve_agent_session_name_prefers_the_derived_name_when_it_is_live() {
        let derived = format!("{P}Refactor_billing_{ID8}");
        let stale = format!("{P}Vikings_{ID8}");
        let names = [derived.as_str(), stale.as_str()];
        assert_eq!(
            resolve_agent_session_name(names, ID, &derived),
            derived,
            "a live derived name is never overridden"
        );
    }

    #[test]
    fn resolve_agent_session_name_adopts_the_stale_name_after_a_retitle() {
        let derived = format!("{P}Refactor_billing_mod_{ID8}");
        let stale = format!("{P}Vikings_{ID8}");
        assert_eq!(
            resolve_agent_session_name([stale.as_str()], ID, &derived),
            stale,
            "lifecycle ops must follow the live session, not the derived name"
        );
    }

    #[test]
    fn resolve_agent_session_name_ignores_other_kinds_and_other_ids() {
        let derived = format!("{P}Refactor_{ID8}");
        let names = [
            format!("{TERMINAL_PREFIX}Vikings_{ID8}"),
            format!("{CONTAINER_TERMINAL_PREFIX}Vikings_{ID8}"),
            format!("{TOOL_PREFIX}lazygit_Vikings_{ID8}"),
            format!("{P}Vikings_99999999"),
            "vim".to_string(),
        ];
        assert_eq!(
            resolve_agent_session_name(names.iter().map(String::as_str), ID, &derived),
            derived,
            "nothing here is this session's agent pane"
        );
    }

    #[test]
    fn resolve_agent_session_name_falls_back_when_two_candidates_are_ambiguous() {
        let derived = format!("{P}Refactor_{ID8}");
        let names = [format!("{P}Vikings_{ID8}"), format!("{P}Aztecs_{ID8}")];
        assert_eq!(
            resolve_agent_session_name(names.iter().map(String::as_str), ID, &derived),
            derived,
        );
    }

    #[test]
    fn resolve_agent_session_name_in_agrees_with_the_scan_on_both_paths() {
        let meta = |names: &[&str]| -> HashMap<String, PaneMetadata> {
            names
                .iter()
                .map(|n| {
                    (
                        n.to_string(),
                        PaneMetadata {
                            pane_dead: false,
                            pane_current_command: None,
                            pane_start_command_is_protected: false,
                            pane_pid: None,
                            pane_title: None,
                            window_activity: None,
                            window_size: None,
                        },
                    )
                })
                .collect()
        };
        let derived = format!("{P}Refactor_{ID8}");
        let stale = format!("{P}Vikings_{ID8}");

        for names in [
            vec![derived.as_str()],
            vec![stale.as_str()],
            vec![derived.as_str(), stale.as_str()],
            vec![],
        ] {
            let map = meta(&names);
            assert_eq!(
                resolve_agent_session_name_in(&map, ID, &derived),
                resolve_agent_session_name(names.iter().copied(), ID, &derived),
                "fast path and scan disagree for {names:?}"
            );
        }
    }

    #[test]
    fn resolve_agent_session_name_handles_a_title_shaped_like_an_aux_prefix() {
        let derived = format!("{P}term_rewriting_{ID8}");
        let stale = format!("{P}Vikings_{ID8}");
        assert_eq!(
            resolve_agent_session_name([stale.as_str()], ID, &derived),
            stale,
            "retitled INTO an aux-shaped title still resolves onto the live pane"
        );
        assert_eq!(
            resolve_agent_session_name([stale.as_str(), derived.as_str()], ID, &derived),
            derived,
            "a live derived name wins even when the shape filter excludes it"
        );
    }

    #[test]
    fn agent_session_belongs_to_matches_by_id_not_title() {
        assert!(agent_session_belongs_to(&format!("{P}Vikings_{ID8}"), ID));
        assert!(agent_session_belongs_to(&format!("{P}Anything_{ID8}"), ID));
        assert!(!agent_session_belongs_to(
            &format!("{TERMINAL_PREFIX}Vikings_{ID8}"),
            ID
        ));
        assert!(!agent_session_belongs_to(
            &format!("{P}Vikings_99999999"),
            ID
        ));
        assert!(!agent_session_belongs_to("vim", ID));
    }

    fn dead_pane_meta(dead: bool) -> PaneMetadata {
        PaneMetadata {
            pane_dead: dead,
            pane_current_command: None,
            pane_start_command_is_protected: false,
            pane_pid: None,
            pane_title: None,
            window_activity: None,
            window_size: None,
        }
    }

    #[test]
    fn snapshot_lookup_matches_the_per_item_probe() {
        let agent = format!("{P}Refactor_{ID8}");
        let cases = [(false, Some(agent.as_str())), (true, None)];
        for (pane_dead, expected) in cases {
            let snapshot = LiveSessionSnapshot::from_parts(
                Some(vec![agent.clone()]),
                Some(HashMap::from([(agent.clone(), dead_pane_meta(pane_dead))])),
            );
            assert_eq!(
                live_any_kind_name_for_id_in(&snapshot, ID).as_deref(),
                expected,
                "pane_dead = {pane_dead}"
            );
        }
    }

    #[test]
    fn snapshot_lookup_reports_not_live_when_server_unreachable() {
        let snapshot = LiveSessionSnapshot::from_parts(None, None);
        assert_eq!(live_any_kind_name_for_id_in(&snapshot, ID), None);
    }

    #[test]
    #[serial_test::serial]
    fn live_any_kind_name_for_id_prefers_agent_then_terminal_then_container() {
        let agent = format!("{P}Refactor_{ID8}");
        let terminal = format!("{TERMINAL_PREFIX}Refactor_{ID8}");
        let container = format!("{CONTAINER_TERMINAL_PREFIX}Refactor_{ID8}");

        let all = [agent.as_str(), terminal.as_str(), container.as_str()];
        assert_eq!(
            live_any_kind_name_for_id(unmarked(all), ID, utils::is_pane_dead).as_deref(),
            Some(agent.as_str()),
            "the agent pane wins when present"
        );
        assert_eq!(
            live_any_kind_name_for_id(
                unmarked([terminal.as_str(), container.as_str()]),
                ID,
                utils::is_pane_dead
            )
            .as_deref(),
            Some(terminal.as_str()),
            "the paired terminal is preferred over the container terminal"
        );
        assert_eq!(
            live_any_kind_name_for_id(unmarked([container.as_str()]), ID, utils::is_pane_dead)
                .as_deref(),
            Some(container.as_str()),
        );
    }

    #[test]
    #[serial_test::serial]
    fn live_agent_name_for_id_ignores_terminal_and_container_panes() {
        let agent = format!("{P}Refactor_{ID8}");
        let terminal = format!("{TERMINAL_PREFIX}Refactor_{ID8}");
        let container = format!("{CONTAINER_TERMINAL_PREFIX}Refactor_{ID8}");

        let snapshot = LiveSessionSnapshot::from_parts(
            Some(vec![agent.clone(), terminal.clone(), container.clone()]),
            Some(HashMap::new()),
        );
        assert_eq!(
            live_agent_name_for_id_in(&snapshot, ID).as_deref(),
            Some(agent.as_str()),
        );
        let snapshot = LiveSessionSnapshot::from_parts(
            Some(vec![terminal.clone(), container.clone()]),
            Some(HashMap::new()),
        );
        assert_eq!(
            live_agent_name_for_id_in(&snapshot, ID),
            None,
            "a surviving terminal is not a live agent pane"
        );
        assert_eq!(
            live_any_kind_name_for_id(
                unmarked([terminal.as_str(), container.as_str()]),
                ID,
                utils::is_pane_dead
            )
            .as_deref(),
            Some(terminal.as_str()),
        );

        let shadowed = format!("{TERMINAL_PREFIX}rewriting_{ID8}");
        let snapshot =
            LiveSessionSnapshot::from_parts(Some(vec![shadowed.clone()]), Some(HashMap::new()));
        assert_eq!(live_agent_name_for_id_in(&snapshot, ID), None);
    }

    #[test]
    fn session_scan_reads_the_kind_field_and_tolerates_its_absence() {
        let parsed = parse_session_scan(
            "aoe_Vikings_abcd1234|1789065184|agent\n\
             aoe_term_Vikings_abcd1234|1789065184|term\n\
             unmarked_session|1789065184|\n\
             short_line|1789065184\n\
             aoe_Weird_abcd1234|1789065184|from-a-newer-build\n\
             garbage-with-no-separator",
        );

        assert_eq!(
            parsed.get("aoe_Vikings_abcd1234").unwrap().kind,
            Some(SessionKind::Agent)
        );
        assert_eq!(
            parsed.get("aoe_term_Vikings_abcd1234").unwrap().kind,
            Some(SessionKind::Terminal)
        );
        assert_eq!(
            parsed.get("unmarked_session").unwrap().activity,
            1789065184,
            "an empty kind field still carries the session and its activity"
        );
        assert_eq!(parsed.get("unmarked_session").unwrap().kind, None);
        assert_eq!(parsed.get("short_line").unwrap().kind, None);
        assert_eq!(
            parsed.get("aoe_Weird_abcd1234").unwrap().kind,
            None,
            "a marker this build does not know is not evidence of a kind"
        );
        assert!(!parsed.contains_key("garbage-with-no-separator"));
    }

    #[test]
    fn an_inherited_kind_option_marks_nothing() {
        let agent = format!("{P}Vikings{ID8}");
        let terminal = format!("{TERMINAL_PREFIX}Vikings{ID8}");
        let scan = format!(
            "agent\n\
             {agent}|1789065184|agent\n\
             {terminal}|1789065184|agent\n\
             {terminal}_t1|1789065184|term"
        );

        let parsed = parse_session_scan(&scan);
        assert_eq!(
            parsed.get(&terminal).unwrap().kind,
            None,
            "a value the global could have produced is not a mark"
        );
        assert_eq!(
            parsed.get(&agent).unwrap().kind,
            None,
            "including on a session that really is an agent: unmarked falls \
             back to the name shape, which is right for it"
        );
        assert_eq!(
            parsed.get(&format!("{terminal}_t1")).unwrap().kind,
            Some(SessionKind::Terminal),
            "a value the global cannot explain is still a mark"
        );

        let parsed = parse_session_scan(&format!("{terminal}|1789065184|agent"));
        assert_eq!(
            parsed.get(&terminal).unwrap().kind,
            Some(SessionKind::Agent)
        );

        let parsed = parse_session_scan(&format!(
            "term\n\
             agent\n\
             {agent}|1789065184|agent\n\
             {terminal}|1789065184|term\n\
             {terminal}_t1|1789065184|tool"
        ));
        assert_eq!(parsed.get(&agent).unwrap().kind, None);
        assert_eq!(parsed.get(&terminal).unwrap().kind, None);
        assert_eq!(
            parsed.get(&format!("{terminal}_t1")).unwrap().kind,
            Some(SessionKind::Tool),
            "a value no scope could have produced is still a mark"
        );

        let parsed = parse_session_scan(&format!(
            "a|b\n\
             agent\n\
             {terminal}|1789065184|agent"
        ));
        assert_eq!(
            parsed.get(&terminal).unwrap().kind,
            None,
            "a later scope is still subtracted when an earlier one holds a separator"
        );
        assert!(!parsed.contains_key("a"), "a scope value is not a session");
    }

    #[test]
    #[serial_test::serial]
    fn live_agent_lookup_rejects_multiple_live_matches() {
        let first = format!("{P}first_{ID8}");
        let second = format!("{TERMINAL_PREFIX}second_{ID8}");
        let names = vec![
            (first.clone(), Some(SessionKind::Agent)),
            (second.clone(), Some(SessionKind::Agent)),
        ];
        let snapshot =
            LiveSessionSnapshot::from_marked_parts(Some(names.clone()), Some(HashMap::new()));
        assert_eq!(live_agent_name_for_id_in(&snapshot, ID), None);
        assert_eq!(
            live_agent_name_for_id(
                names
                    .iter()
                    .map(|(name, kind)| (name.as_str(), kind.map(SessionKind::as_marker))),
                ID,
                |name| name == second,
            ),
            Some(first),
            "a dead duplicate must not disqualify the only live agent",
        );
    }

    #[test]
    #[serial_test::serial]
    fn live_agent_lookup_follows_the_marker_over_the_name_shape() {
        let ambiguous = format!("{TERMINAL_PREFIX}rewriting_{ID8}");

        let as_agent = LiveSessionSnapshot::from_marked_parts(
            Some(vec![(ambiguous.clone(), Some(SessionKind::Agent))]),
            Some(HashMap::new()),
        );
        assert_eq!(
            live_agent_name_for_id_in(&as_agent, ID).as_deref(),
            Some(ambiguous.as_str()),
            "a marked agent is the row's agent pane whatever its title sanitized to"
        );

        let as_terminal = LiveSessionSnapshot::from_marked_parts(
            Some(vec![(ambiguous.clone(), Some(SessionKind::Terminal))]),
            Some(HashMap::new()),
        );
        assert_eq!(
            live_agent_name_for_id_in(&as_terminal, ID),
            None,
            "a marked terminal never passes as the agent pane"
        );

        let unmarked_snapshot =
            LiveSessionSnapshot::from_parts(Some(vec![ambiguous.clone()]), Some(HashMap::new()));
        assert_eq!(
            live_agent_name_for_id_in(&unmarked_snapshot, ID),
            None,
            "a session created before the marker keeps the old, ambiguous guess"
        );

        let terminal = format!("{TERMINAL_PREFIX}other_{ID8}");
        assert_eq!(
            live_any_kind_name_for_id(
                [
                    (terminal.as_str(), Some("term")),
                    (ambiguous.as_str(), Some("agent")),
                ],
                ID,
                |_| false
            )
            .as_deref(),
            Some(ambiguous.as_str()),
        );
    }

    #[test]
    fn a_session_marked_another_kind_is_not_the_live_derived_name() {
        let derived = format!("{TERMINAL_PREFIX}Foo_{ID8}");
        let agent = format!("{P}Foo_{ID8}");

        assert_eq!(
            resolve_session_name(
                [
                    (derived.as_str(), Some("term")),
                    (agent.as_str(), Some("agent")),
                ],
                &derived,
                &NameShape::agent(&id_suffix(ID))
            ),
            agent,
            "the marked agent wins over a terminal wearing the derived name"
        );
        assert_eq!(
            resolve_agent_session_name([derived.as_str(), agent.as_str()], ID, &derived),
            derived,
            "unmarked keeps the pre-marker answer: a live derived name wins"
        );
    }

    #[test]
    #[serial_test::serial]
    fn live_any_kind_name_for_id_excludes_tool_subsessions_and_other_ids() {
        let names = [
            format!("{TOOL_PREFIX}lazygit_Refactor_{ID8}"),
            format!("{P}Refactor_99999999"),
            format!("{TERMINAL_PREFIX}Refactor_99999999"),
            "vim".to_string(),
        ];
        assert_eq!(
            live_any_kind_name_for_id(
                unmarked(names.iter().map(String::as_str)),
                ID,
                utils::is_pane_dead
            ),
            None,
            "a tool sub-session and other ids are never this session's pane"
        );
    }

    #[test]
    #[serial_test::serial]
    fn session_new_resolves_onto_a_retitled_sessions_live_name() {
        let guard = SessionCacheGuard::capture();
        let stale = Session::generate_name(ID, "Vikings");
        guard.force_present(&[stale.as_str()]);

        let session = Session::new(ID, "Refactor billing module").expect("session");
        assert_eq!(session.name(), stale);
    }

    #[test]
    #[serial_test::serial]
    fn live_agent_session_name_answers_from_an_unreachable_snapshot_without_refreshing() {
        let guard = SessionCacheGuard::capture();
        guard.force_unreachable();
        let derived = format!("{P}Vikings_{ID8}");
        assert_eq!(live_agent_session_name(ID, &derived), derived);
        assert_eq!(
            session_name_from_cache(&derived, &NameShape::agent(&id_suffix(ID))),
            Some(derived),
            "the snapshot must satisfy the lookup, so no refresh is attempted"
        );
    }

    #[test]
    #[serial_test::serial]
    fn session_new_keeps_the_derived_name_when_nothing_is_live() {
        let guard = SessionCacheGuard::capture();
        guard.force_present(&[]);

        let derived = Session::generate_name(ID, "Refactor billing module");
        let session = Session::new(ID, "Refactor billing module").expect("session");
        assert_eq!(session.name(), derived);
    }

    #[test]
    fn is_aoe_session_matches_every_kind_and_rejects_foreign() {
        assert!(is_aoe_session(&format!("{P}my_proj_abc12345")));
        assert!(is_aoe_session(&format!("{TERMINAL_PREFIX}x")));
        assert!(is_aoe_session(&format!("{CONTAINER_TERMINAL_PREFIX}x")));
        assert!(is_aoe_session(&format!("{TOOL_PREFIX}x")));
        assert!(!is_aoe_session("vim"));
        assert!(!is_aoe_session("my_aoe_session"));
    }

    #[test]
    #[serial_test::serial]
    fn session_exists_trusts_a_cache_hit_without_tmux() {
        let _guard = SessionCacheGuard::capture();
        let name = format!("{P}exists_probe_cache_hit");
        test_inject_session_into_cache(&name);
        assert!(session_exists(&name));
    }

    #[test]
    #[serial_test::serial]
    fn a_forced_cache_snapshot_survives_a_concurrent_refresh() {
        let guard = SessionCacheGuard::capture();
        let name = format!("{P}forced_snapshot_survives_refresh");
        guard.force_present(&[name.as_str()]);

        refresh_session_cache();

        assert_eq!(
            probe_session_existence(&name),
            SessionExistence::Present,
            "a live SessionCacheGuard must own the snapshot"
        );
    }

    #[test]
    fn tmux_no_server_running_detects_empty_case() {
        assert!(tmux_no_server_running(
            b"no server running on /tmp/tmux-501/default\n"
        ));
        assert!(tmux_no_server_running(b"no server running on /path.sock"));
        assert!(tmux_no_server_running(
            b"error connecting to /path.sock (No such file or directory)"
        ));
        assert!(tmux_no_server_running(
            b"error connecting to /tmp/No such file or directory.sock (No such file or directory)"
        ));
    }

    #[test]
    fn tmux_no_server_running_rejects_other_errors_and_empty() {
        assert!(!tmux_no_server_running(b"can't find session: aoe_foo"));
        assert!(!tmux_no_server_running(b"usage: list-sessions"));
        assert!(!tmux_no_server_running(b""));
        assert!(!tmux_no_server_running(
            b"error connecting to /path.sock (Permission denied)"
        ));
        assert!(!tmux_no_server_running(
            b"error connecting to /path.sock (Socket operation on non-socket)"
        ));
        assert!(!tmux_no_server_running(
            b"error connecting to /tmp/No such file or directory.sock (Permission denied)"
        ));
        assert!(!tmux_no_server_running(
            b"error connecting to /tmp/no server running.sock (Permission denied)"
        ));
    }

    #[test]
    fn test_parse_pane_metadata_basic() {
        let output = format!("{P}my_proj_abc12345|0|0|190|52|claude|claude|4242\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        let meta = map.get(&format!("{P}my_proj_abc12345")).unwrap();
        assert!(!meta.pane_dead);
        assert_eq!(meta.pane_current_command.as_deref(), Some("claude"));
        assert!(!meta.pane_start_command_is_protected);
        assert_eq!(meta.pane_pid, Some(4242));
        assert_eq!(meta.window_size, Some((190, 52)));
    }

    #[test]
    fn test_parse_pane_metadata_reads_the_tail_fields() {
        let output = format!(
            "{P}proj_abc12345|0|0|190|52|claude|claude{TAIL_SEP}1770000000{TAIL_SEP}✶ Working\n"
        );
        let meta = parse_pane_metadata(&output)
            .remove(&format!("{P}proj_abc12345"))
            .unwrap();
        assert_eq!(meta.window_activity, Some(1770000000));
        assert_eq!(meta.pane_title.as_deref(), Some("✶ Working"));

        let escaped_output = format!(
            "{P}proj_escaped_abc12345|0|0|190|52|claude|claude literal{}{ESCAPED_TAIL_SEP}|4242{ESCAPED_TAIL_SEP}1770000001{ESCAPED_TAIL_SEP}literal{}{ESCAPED_TAIL_SEP}title{}",
            char::from(92),
            char::from(92),
            char::from(10)
        );
        let escaped_meta = parse_pane_metadata(&escaped_output)
            .remove(&format!("{P}proj_escaped_abc12345"))
            .unwrap();
        assert_eq!(escaped_meta.pane_pid, Some(4242));
        assert_eq!(escaped_meta.window_activity, Some(1770000001));
        assert_eq!(
            escaped_meta.pane_title,
            Some(format!("literal{}{ESCAPED_TAIL_SEP}title", char::from(92)))
        );

        let odd = format!("{P}proj_def67890|0|0|||claude|claude{TAIL_SEP}{TAIL_SEP}\n");
        let meta = parse_pane_metadata(&odd)
            .remove(&format!("{P}proj_def67890"))
            .unwrap();
        assert_eq!(meta.window_activity, None);
        assert_eq!(meta.pane_title, None);
        assert_eq!(meta.window_size, None);
    }

    #[test]
    fn test_parse_pane_metadata_protected_wrapper_shell_is_not_stale() {
        let output = format!(
            "{P}protected_abc12345|0|0|190|52|sh|/bin/sh -c 'prepare | . /tmp/aoe-pane-env-123 | exec claude'\n\
             {P}interactive_def67890|0|0|190|52|sh|sh\n"
        );
        let map = parse_pane_metadata(&output);

        let cases = [
            (format!("{P}protected_abc12345"), false),
            (format!("{P}interactive_def67890"), true),
        ];
        for (name, expected_shell_stale) in cases {
            let meta = map.get(&name).unwrap();
            assert_eq!(
                utils::is_pane_running_shell_command(
                    meta.pane_current_command.as_deref().unwrap(),
                    meta.pane_start_command_is_protected,
                ),
                expected_shell_stale,
                "{name}"
            );
        }
    }

    /// One row per aoe session: pane and window zero, first line wins.
    #[test]
    fn parse_pane_metadata_row_selection_table() {
        type Want<'a> = &'a [(&'a str, Option<&'a str>, bool)];
        let cases: &[(&str, String, Want<'_>)] = &[
            (
                "dead pane",
                format!("{P}proj_abc12345|0|1|190|52|bash|bash\n"),
                &[("proj_abc12345", Some("bash"), true)],
            ),
            (
                "non-aoe sessions filtered",
                format!(
                    "user_session|0|0|190|52|bash|bash\n{P}proj_abc12345|0|0|190|52|claude|claude\nmy_tmux|0|0|190|52|vim|vim\n"
                ),
                &[("proj_abc12345", Some("claude"), false)],
            ),
            (
                "non-zero panes filtered",
                format!(
                    "{P}proj_abc12345|0|0|190|52|claude|claude\n{P}proj_abc12345|1|0|190|52|bash|bash\n"
                ),
                &[("proj_abc12345", Some("claude"), false)],
            ),
            (
                "first window wins",
                format!(
                    "{P}proj_abc12345|0|0|190|52|claude|claude\n{P}proj_abc12345|0|1|190|52|bash|bash\n"
                ),
                &[("proj_abc12345", Some("claude"), false)],
            ),
            ("empty output", String::new(), &[]),
            (
                "malformed and blank lines skipped",
                format!("too|few|fields\n{P}proj_abc12345|0|0|190|52|claude|claude\n\n"),
                &[("proj_abc12345", Some("claude"), false)],
            ),
            (
                "empty command",
                format!("{P}proj_abc12345|0|0|190|52||sh\n"),
                &[("proj_abc12345", None, false)],
            ),
            (
                "several sessions",
                format!(
                    "{P}proj_a_abc12345|0|0|190|52|claude|claude\n{P}proj_b_def67890|0|0|190|52|opencode|opencode\n{P}proj_c_ghi11111|0|1|190|52|bash|bash\n"
                ),
                &[
                    ("proj_a_abc12345", Some("claude"), false),
                    ("proj_b_def67890", Some("opencode"), false),
                    ("proj_c_ghi11111", Some("bash"), true),
                ],
            ),
        ];
        for (label, output, want) in cases {
            let map = parse_pane_metadata(output);
            assert_eq!(map.len(), want.len(), "{label}");
            for (suffix, command, dead) in *want {
                let name = format!("{P}{suffix}");
                let meta = map.get(&name).unwrap_or_else(|| panic!("{label}: {name}"));
                assert_eq!(meta.pane_current_command.as_deref(), *command, "{label}");
                assert_eq!(meta.pane_dead, *dead, "{label}");
            }
        }
    }
    #[test]
    #[serial_test::serial]
    fn a_failed_pane_snapshot_is_an_answer_so_rows_do_not_re_fork() {
        let guard = PaneMetaCacheGuard::capture();
        guard.force_failed_refresh();

        assert_eq!(
            pane_dead_from_cache("aoe_tool_absent_00000000"),
            Some(false),
            "a fresh snapshot with no data must answer \"can't tell, not dead\", \
             not report itself stale"
        );
        assert!(
            !pane_dead_for_display("aoe_tool_absent_00000000"),
            "and the display helper must not claim a pane it cannot see is dead"
        );
    }

    #[test]
    #[serial_test::serial]
    fn display_lookups_keep_last_good_snapshot_until_authoritative_absence() {
        let guard = SessionCacheGuard::capture();
        let derived = format!("{P}Current_{ID8}");
        let last_good = format!("{P}Previous_{ID8}");
        let suffix = id_suffix(ID);
        let shape = NameShape::agent(&suffix);

        guard.force_present(&[last_good.as_str()]);
        guard.force_stale();
        assert!(session_exists_for_display(&last_good));
        assert_eq!(session_name_for_display(&derived, &shape), last_good);
        let unknown_refresh_id = SESSION_CACHE.read().expect("session cache").refresh_id + 1;
        assert_eq!(
            publish_session_cache(
                unknown_refresh_id,
                None,
                SessionCacheRefresh::Unknown,
                false,
            ),
            SessionCacheRefresh::Unknown,
        );
        assert_eq!(
            session_existence_from_cache(&last_good),
            Some(SessionExistence::Unknown),
        );
        assert_eq!(session_exists_from_cache(&last_good), None);
        assert_eq!(
            session_name_from_cache(&derived, &shape),
            Some(derived.clone())
        );
        assert!(session_exists_for_display(&last_good));
        assert_eq!(session_name_for_display(&derived, &shape), last_good);

        guard.force_present(&[]);
        assert!(!session_exists_for_display(&last_good));
        assert_eq!(session_name_for_display(&derived, &shape), derived);

        guard.force_present(&[last_good.as_str()]);
        let no_server_refresh_id = SESSION_CACHE.read().expect("session cache").refresh_id + 1;
        publish_session_cache(
            no_server_refresh_id,
            None,
            SessionCacheRefresh::NoServer,
            false,
        );
        assert!(!session_exists_for_display(&last_good));
        assert_eq!(session_name_for_display(&derived, &shape), derived);
    }

    #[test]
    #[serial_test::serial]
    fn display_liveness_answers_from_the_snapshot_instead_of_probing_per_name() {
        require_tmux!();
        let guard = SessionCacheGuard::capture();
        let session = test_helpers::TmuxTestSession::new(&format!("{SESSION_PREFIX}display_probe"));
        let created = tmux_command()
            .args(["new-session", "-d", "-s", session.name(), "sleep 60"])
            .output()
            .expect("tmux new-session");
        assert!(created.status.success());

        guard.force_present(&[]);
        assert!(
            !session_exists_for_display(session.name()),
            "display path must answer from the snapshot, not probe tmux"
        );
        assert!(
            session_exists(session.name()),
            "the probing path must see the live pane the snapshot missed; \
             without this the test would pass on a broken snapshot too"
        );

        guard.force_present(&[session.name()]);
        assert!(session_exists_for_display(session.name()));
    }

    #[test]
    #[serial_test::serial]
    fn rekey_session_adopts_peer_renamed_pane() {
        require_tmux!();
        let start_name = Session::generate_name(ID, "Fix login bug");
        let peer_name = Session::generate_name(ID, "Peer rename");
        let final_name = Session::generate_name(ID, "Final rename");
        let start_guard = TmuxTestSession::from_name(start_name.clone());
        let peer_guard = TmuxTestSession::from_name(peer_name.clone());
        let final_guard = TmuxTestSession::from_name(final_name.clone());
        let created = tmux_command()
            .args(["new-session", "-d", "-s", start_guard.name(), "sleep 60"])
            .output()
            .expect("tmux new-session");
        assert!(created.status.success());
        refresh_session_cache();

        let peer_rename = tmux_command()
            .args(["rename-session", "-t", &start_name, &peer_name])
            .output()
            .expect("peer tmux rename");
        assert!(peer_rename.status.success());
        assert!(rekey_session(ID, "Fix login bug", "Final rename").unwrap());
        assert!(Session::from_name(&final_name).exists());
        drop((start_guard, peer_guard, final_guard));
    }

    #[test]
    #[serial_test::serial]
    fn rekey_session_refreshes_the_status_bar_title() {
        require_tmux!();
        let start_name = Session::generate_name(ID, "Britons");
        let final_name = Session::generate_name(ID, "Fix detach hint");
        let start_guard = TmuxTestSession::from_name(start_name.clone());
        let final_guard = TmuxTestSession::from_name(final_name.clone());
        let created = tmux_command()
            .args(["new-session", "-d", "-s", start_guard.name(), "sleep 60"])
            .output()
            .expect("tmux new-session");
        assert!(created.status.success());
        let seeded = tmux_command()
            .args(["set-option", "-t", &start_name, "@aoe_title", "Britons"])
            .output()
            .expect("tmux set-option @aoe_title");
        assert!(seeded.status.success());
        refresh_session_cache();

        assert!(rekey_session(ID, "Britons", "Fix detach hint").unwrap());

        let shown = tmux_command()
            .args(["show-options", "-t", &final_name, "-v", "@aoe_title"])
            .output()
            .expect("tmux show-options @aoe_title");
        assert_eq!(
            String::from_utf8_lossy(&shown.stdout).trim(),
            "Fix detach hint"
        );
        drop((start_guard, final_guard));
    }

    #[test]
    #[serial_test::serial]
    fn rekey_session_reports_false_for_vanished_pane() {
        require_tmux!();
        let dummy_guard = TmuxTestSession::new("aoe_test_rekey_dummy");
        let dummy_created = tmux_command()
            .args(["new-session", "-d", "-s", dummy_guard.name(), "sleep 60"])
            .output()
            .expect("dummy tmux new-session");
        assert!(dummy_created.status.success());
        let name = Session::generate_name(ID, "Final rename");
        let guard = TmuxTestSession::from_name(name.clone());
        let created = tmux_command()
            .args(["new-session", "-d", "-s", guard.name(), "sleep 60"])
            .output()
            .expect("tmux new-session");
        assert!(created.status.success());
        refresh_session_cache();
        let killed = tmux_command()
            .args(["kill-session", "-t", &name])
            .output()
            .expect("tmux kill-session");
        assert!(killed.status.success());
        assert!(!rekey_session(ID, "Final rename", "No live pane").unwrap());
        drop((guard, dummy_guard));
    }

    #[test]
    #[serial_test::serial]
    fn test_export_exec_compound_command_passes_env() {
        require_tmux!();

        let dummy_guard = TmuxTestSession::new("aoe_test_compound_dummy");
        let dummy = dummy_guard.name().to_string();
        let _ = tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &dummy,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 120",
            ])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(200));

        let session_guard = TmuxTestSession::new("aoe_test_compound");
        let session_name = session_guard.name().to_string();
        let marker = format!("AOE_COMPOUND_TEST_{}", std::process::id());
        let secret_value = "s3cret_val!@#";

        let compound_cmd = format!(
            "export {}='{}'; exec printenv {}",
            marker,
            secret_value.replace('\'', "'\\''"),
            marker
        );

        let output = tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "120",
                "-y",
                "24",
                &compound_cmd,
                ";",
                "set-option",
                "-t",
                &session_name,
                "pane-base-index",
                "0",
                ";",
                "set-option",
                "-t",
                &session_name,
                "pane-base-index",
                "0",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success(), "Failed to create tmux session");

        let capture_pane = || {
            let capture = tmux_command()
                .args([
                    "capture-pane",
                    "-t",
                    &format!("{}:^.0", session_name),
                    "-p",
                    "-S",
                    "-10",
                ])
                .output()
                .expect("capture-pane");
            String::from_utf8_lossy(&capture.stdout).into_owned()
        };
        let pane_is_dead = || {
            let dead_check = tmux_command()
                .args(["display-message", "-t", &session_name, "-p", "#{pane_dead}"])
                .output()
                .expect("pane dead check");
            String::from_utf8_lossy(&dead_check.stdout).trim() == "1"
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut pane_content = capture_pane();
        while std::time::Instant::now() < deadline
            && !(pane_content.contains(secret_value) && pane_is_dead())
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
            pane_content = capture_pane();
        }

        assert!(
            pane_content.contains(secret_value),
            "Expected secret value in pane output (proves export reached exec'd process).\nPane:\n{pane_content}"
        );
        assert!(
            pane_is_dead(),
            "Pane should be dead after exec'd command exits (lifecycle preserved)"
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn login_shell_probe_batches_agents_and_continues_after_a_miss() {
        if run_probe_test_in_subprocess() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let _env = probe_environment(home.path());
        let log = shell_words::quote(home.path().join("probes").to_str().unwrap()).into_owned();
        std::fs::write(
            home.path().join("bin/login-shell"),
            format!("#!/bin/sh\nprintf 'login\n' >> {log}\n/bin/sleep 6\nexec /bin/sh -c \"$2\"\n"),
        )
        .unwrap();
        let found = login_shell_probe(&[
            crate::agents::get_agent("vibe").unwrap(),
            crate::agents::get_agent("claude").unwrap(),
        ]);
        assert_eq!(found, HashSet::from(["claude".to_owned()]));
        assert_eq!(
            std::fs::read_to_string(home.path().join("probes")).unwrap(),
            "login\nversion\nwhich:claude\n"
        );
    }

    #[test]
    fn parse_login_shell_probe_extracts_markers_amid_login_noise() {
        let stdout = "\
Welcome to zsh!\n\
nvm is lazily loading node v22.1.0...\n\
AOE_AGENT_OK kimi\n\
some other banner AOE_AGENT_OK not-a-marker-line\n\
  AOE_AGENT_OK omp  \n\
AOE_AGENT_OK\n";
        let found = parse_login_shell_probe(stdout);
        assert_eq!(
            found,
            ["kimi", "omp"].iter().map(|s| s.to_string()).collect(),
            "markers parse through profile noise; mid-line and empty markers are ignored"
        );
    }
}
