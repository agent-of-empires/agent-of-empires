//! tmux integration module

pub(crate) mod composite;
pub(crate) mod detect;
pub(crate) mod env;
mod session;
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
pub use status_detection::{
    detect_claude, detect_status_from_content, detect_status_from_content_in, detect_via_manifest,
    detect_with_rules,
};
pub use terminal_session::{kill_all_terminals_for_id, ContainerTerminalSession, TerminalSession};
pub use tool_session::{kill_all_tool_sessions_for_id, ToolSession};
pub use utils::{attach_return_key, tmux_prefix_display};

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

/// Environment variable that overrides the tmux socket path. Set by the e2e
/// harness (and available for opt-in isolation) so a spawned `aoe` routes all
/// tmux calls to a known per-test socket instead of relying on `$TMUX`.
pub const TMUX_SOCKET_ENV: &str = "AOE_TMUX_SOCKET";

/// Resolve the config layer that governs a session's `[tmux]` options.
///
/// `[tmux]` is profile-overridable like any other section, so every consumer of
/// [`crate::session::config::resolve_tmux_setting`] resolves
/// through here rather than reading the global `config.toml`: doing the latter
/// made a profile's `[tmux]` block silently inert (issue #3207). An empty
/// profile name resolves to the default profile, matching every other
/// profile-scoped read.
pub(crate) fn tmux_option_config(profile: &str) -> crate::session::Config {
    crate::session::config::profile_config::resolve_config_or_warn(profile)
}

/// How aoe points tmux at a specific server, if at all.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TmuxSocket {
    /// A full socket path, passed as `tmux -S <path>`. Used for build/test
    /// isolation and the `AOE_TMUX_SOCKET` override, where aoe owns the exact
    /// path.
    Path(PathBuf),
    /// A socket name, passed as `tmux -L <name>`. Used for the user-facing
    /// segmentation setting (#2267); tmux owns the socket directory
    /// (`$TMUX_TMPDIR`, else `/tmp/tmux-<UID>/`) and its `0700` perms.
    Name(String),
}

/// Resolve which tmux server this build talks to, or `None` to use tmux's
/// default per-user socket. Cached: neither the process env nor the config are
/// re-read at runtime (moving live sessions across servers is not meaningful).
///
/// - `AOE_TMUX_SOCKET` set -> that path via `-S` (e2e / opt-in isolation).
/// - unit tests            -> a shared temp socket, so `cargo test` never
///   touches the developer's real tmux server.
/// - debug builds          -> `<app_dir>/tmux.sock`, giving `cargo run` and
///   e2e their own tmux server so they can never poison an installed release
///   build's shared server (#2608); the app dir is already namespaced
///   (`~/.agent-of-empires-dev`).
/// - `tmux.socket_name` config set -> that name via `-L` (#2267): the user
///   opts into a private tmux server so their hand-managed `tmux ls` no longer
///   lists aoe's sessions. Release builds only; debug/test already isolate onto
///   their own socket above.
/// - release builds        -> `None`: keep tmux's default socket so upgrading
///   does not orphan the release build's live sessions.
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

/// The build-specific isolation socket path, if this build forces one. Test
/// and debug builds get their own server so they can never poison an installed
/// release build's shared tmux server (#2608). Release builds return `None` so
/// the user's `tmux.socket_name` setting (or the default socket) applies.
fn build_isolation_socket() -> Option<PathBuf> {
    #[cfg(test)]
    {
        // Per-process socket, not a fixed name. The resolution is cached once
        // per process so the path stays stable for this test binary (a later
        // test must not have the socket pulled from under it), while the pid
        // keeps it from colliding with a concurrent unit-test process (a second
        // `cargo test`, a serve-vs-default shard, or a server left over from a
        // prior run) that would otherwise share one tmux server and interfere.
        // The collision bites hardest as root, where `/tmp` is shared across
        // every same-uid run.
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

/// The user-configured tmux socket name (`tmux.socket_name`), if any.
fn configured_socket_name() -> Option<String> {
    crate::session::config::Config::load()
        .ok()
        .and_then(|c| c.tmux.socket_name)
}

/// Turn a configured socket name into a `-L` socket, or `None` to fall back to
/// the default socket. A name containing a path separator is rejected (tmux
/// `-L` takes a bare name and owns the directory itself) so a stray `/` cannot
/// silently redirect the server; use `AOE_TMUX_SOCKET` for a full path.
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

/// A `tmux` [`Command`] preconfigured with this build's socket flag (`-S` for a
/// path, `-L` for a name) when one applies. Every tmux invocation in aoe MUST
/// go through this so all commands hit the same server; a raw
/// `Command::new("tmux")` would fall back to the default socket and split state
/// across two servers.
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
    // Attach/switch-client calls run from inside `IgnoreSignalsGuard`'s
    // window (`src/tui/app.rs`), which ignores SIGINT/SIGQUIT on aoe
    // itself while the terminal is handed to tmux. `SIG_IGN` survives
    // exec, so without this every `tmux` child would silently inherit
    // that ignore too, leaving no way to Ctrl+C out of a hung attach.
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut cmd);
    cmd
}

/// Like [`tmux_command`], but pins `LC_MESSAGES=C` so tmux's connection-failure
/// messages on stderr stay stable English for callers that match them. tmux's
/// `client.c` prints `error connecting to <socket> (strerror(errno))` for a
/// non-`ECONNREFUSED` connect failure, and glibc localizes `strerror` by
/// `LC_MESSAGES`, so on a non-English host the `(No such file or directory)`
/// ENOENT marker for an absent socket (#3337) would not match. `LC_ALL` is
/// removed so it cannot override that. Global `-u` forces UTF-8 session names
/// even when the caller has `LC_CTYPE=C` or when `LC_ALL` was the only UTF-8
/// locale source. Used by the status-query callers (which classify via
/// [`tmux_no_server_running`]) and by `kill_session_if_present`. NOT folded into
/// [`tmux_command`]: the interactive attach/switch-client/capture-pane paths must
/// keep the user's locale for UTF-8 and status-bar rendering, and `-u` would
/// assert UTF-8 to a terminal that may not be.
pub(crate) fn tmux_query_command() -> Command {
    let mut cmd = tmux_command();
    cmd.arg("-u");
    cmd.env_remove("LC_ALL");
    cmd.env("LC_MESSAGES", "C");
    cmd
}

// Debug builds use `aoe_dev_*` prefixes so `cargo run` and an installed
// release `aoe` never mistake each other's sessions. Debug builds also run on
// their own tmux socket (see `tmux_socket`), so the two builds no longer
// share a server at all; the prefix split is kept as defence in depth and to
// keep dev/release session names visually distinct.
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

/// Pre-fetched pane metadata from a single `tmux list-panes -a` call.
#[derive(Debug, Clone)]
pub struct PaneMetadata {
    pub pane_dead: bool,
    pub pane_current_command: Option<String>,
    pub pane_start_command_is_protected: bool,
    pub pane_pid: Option<u32>,
    /// The terminal title the pane's program published over OSC 0/2. Several
    /// agent CLIs put their own state in it, which is the one signal that does
    /// not depend on what the transcript happens to contain.
    pub pane_title: Option<String>,
    /// tmux's last-output timestamp for the pane's window, used to skip a
    /// capture when nothing has been drawn since the last one.
    pub window_activity: Option<i64>,
    /// Observed `(window_width, window_height)`. Window, not pane: a resize
    /// by another client changes the window, while pane splits and status-bar
    /// chrome only redistribute rows inside an unchanged window, so this is
    /// the signal the passive-resize reconcile can compare against what it
    /// applied without false mismatches.
    pub window_size: Option<(u16, u16)>,
}

static SESSION_REFRESH_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static FORCED_SESSION_CACHE_GUARDS: AtomicUsize = AtomicUsize::new(0);

/// Whether a test owns SESSION_CACHE through a live SessionCacheGuard, in
/// which case refresh_session_cache must leave the forced snapshot alone.
/// serial_test only orders serial tests against each other, so a parallel test
/// can already be past its staleness check and blocked on the list-sessions
/// fork when the guard forces a snapshot, then land its write mid-test. On a
/// host with no tmux server that write is data: None, which reads back as
/// SessionExistence::Unknown and flips the assertion the guard meant to pin.
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

struct SessionCache {
    data: Option<HashMap<String, i64>>,
    time: Option<Instant>,
    refresh_id: u64,
    outcome: SessionCacheRefresh,
}

/// Shared tmux list-panes snapshot behind pane_dead_for_display, mirroring
/// SESSION_CACHE's TTL and its data: None "the server could not answer" state.
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

/// One wall-clock budget shared by every tmux subprocess in a logical
/// operation. Composite capture uses this so its layout fallback cannot turn
/// one stalled preview sample into two consecutive full timeouts.
pub(crate) struct TmuxCommandDeadline {
    deadline: Instant,
    #[cfg(test)]
    budget: Option<CommandBudget>,
}

/// Test-only stand-in for the wall clock: the first `commands` runs get the
/// standard timeout and every later one reports the budget as spent. Lets a
/// test expire a deadline at a chosen command instead of racing the tmux
/// forks that precede it.
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
fn run_tmux_command_with_timeout_inner(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<Output> {
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

/// Result of the authoritative `list-sessions` scan performed by
/// [`refresh_session_cache`]. The shared cache intentionally keeps both
/// no-server and unexpected failures as `data: None` so status pollers retain
/// their existing conservative `Unknown` behavior; rekeying uses this outcome
/// to suppress a warning only for the recognized no-server case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCacheRefresh {
    Populated,
    NoServer,
    Unknown,
}

// Field separator for the fixed tmux -F head. Must be printable ASCII and
// absent from sanitize_session_name output (which preserves [A-Za-z0-9_-]
// and replaces everything else with _). C0 bytes are reserved for the tail,
// whose parser handles tmux 3.4's octal escaping explicitly.
const FIELD_SEP: char = '|';
/// Separator for the two trailing fields. pane_start_command may itself
/// contain FIELD_SEP, which is why it was last in the original format. A C0
/// control byte cannot appear in a shell command or terminal title. tmux 3.4
/// escapes it as ESCAPED_TAIL_SEP, while newer versions emit it raw.
const TAIL_SEP: char = '\x1f';
const ESCAPED_TAIL_SEP: &str = r"\037";

/// tmux exits non-zero with `no server running on <socket>` on stderr when
/// there is no server on the resolved socket (zero sessions, or the socket's
/// server has died): the normal state for a structured-view user who never
/// opens a terminal. It also exits non-zero with
/// `error connecting to <socket> (No such file or directory)` when the socket
/// file itself is absent (issue #3337), which is likewise the empty case, not
/// an error. Both are treated as empty: callers log at trace and return an
/// empty result, reserving warn for a genuinely unexpected non-zero exit.
///
/// A transient glitch on an existing socket stays on the error path: tmux
/// (`client.c`) emits `error connecting to <socket> (<strerror>)` for a
/// non-`ECONNREFUSED` connect failure, so `(Permission denied)` (EACCES) and
/// `(Socket operation on non-socket)` (ENOTSOCK) do NOT match. The ENOENT
/// marker (and the `no server running` marker) is matched anchored per line,
/// so a socket path that happens to contain either phrase cannot fake the
/// empty case on a different errno. Callers MUST use [`tmux_query_command`] so
/// the `strerror` text is stable English (see #3327/#3328).
fn tmux_no_server_running(stderr: &[u8]) -> bool {
    let s = String::from_utf8_lossy(stderr);
    // tmux (`client.c`) prints both markers at the start of their own line
    // (`no server running on <socket>` / `error connecting to <socket>
    // (<strerror>)`), so anchor to the line rather than scanning the whole
    // buffer, where an arbitrary socket path could otherwise spoof a match.
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
    data: Option<HashMap<String, i64>>,
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
    // An unexpected refresh failure says nothing about the last successful
    // session list. Keep that list for display-only lookups while exposing the
    // failed outcome to authoritative lifecycle callers. A populated response
    // replaces it, and a recognized no-server response clears it.
    if outcome != SessionCacheRefresh::Unknown {
        cache.data = data;
    }
    cache.time = Some(Instant::now());
    cache.refresh_id = refresh_id;
    cache.outcome = outcome;
    outcome
}
pub fn refresh_session_cache() -> SessionCacheRefresh {
    let refresh_id = next_refresh_id(&SESSION_REFRESH_ID);
    let start = Instant::now();
    let mut command = tmux_query_command();
    command.args(["list-sessions", "-F", "#{session_name}|#{session_activity}"]);
    let output = run_tmux_command_with_timeout(&mut command);
    let (new_data, outcome) = match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut map = HashMap::new();
            for line in stdout.lines() {
                if let Some((name, activity)) = line.split_once(FIELD_SEP) {
                    let activity: i64 = activity.parse().unwrap_or(0);
                    map.insert(name.to_string(), activity);
                }
            }
            (Some(map), SessionCacheRefresh::Populated)
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

    // Trace, not debug: the TUI status poller calls this every ~2s, so
    // at debug it dominates the idle log. Errors above still log at warn.
    let sessions = new_data.as_ref().map(|m| m.len()).unwrap_or(0);
    tracing::trace!(
        target: "tmux.cache",
        sessions,
        duration_ms = start.elapsed().as_millis() as u64,
        "session cache refreshed",
    );

    publish_session_cache(refresh_id, new_data, outcome, true)
}

/// Classify the currently selected agent session without letting an
/// ambiguous title-derived resolution turn "another live name carries this
/// id" into confirmed absence.
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
    // Multiple id-shaped candidates make `Session::new` deliberately retain
    // the derived name. That is unresolved, not absence.
    SessionExistence::Unknown
}

/// Rekey a live title-derived tmux session after its new title is durable.
///
/// Returns `Ok(false)` only when tmux authoritatively confirms that no live
/// session exists. Title writers must persist first while holding the
/// per-session title and lifecycle locks through this call; rekeying before
/// commit can strand the pane when persistence fails.
pub(crate) fn rekey_session(id: &str, old_title: &str, new_title: &str) -> anyhow::Result<bool> {
    let renamed = rekey_session_name(id, old_title, new_title)?;
    if renamed {
        // Every `Ok(true)` path leaves the session under the new derived name.
        status_bar::refresh_session_title(&Session::generate_name(id, new_title), new_title);
    }
    Ok(renamed)
}

/// The rename half of [`rekey_session`]: resolves the live session for `id`
/// and moves it to the name derived from `new_title`.
fn rekey_session_name(id: &str, old_title: &str, new_title: &str) -> anyhow::Result<bool> {
    // Name resolution is cache-backed. Force an authoritative scan first so a
    // process-local snapshot from before another writer's rename cannot point
    // this mutation at the old title-derived name.
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

    // Another process may have rekeyed this id between our scan and
    // rename-session. Refresh and resolve by the immutable id suffix, then
    // retry once only when that same live session is confirmed under a newer
    // name. A transient query failure is not evidence the pane disappeared,
    // so preserve the original rename error in that case.
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

/// True for any tmux session name owned by this aoe namespace. Every session
/// kind (agent, terminal, container terminal, tool) is prefixed with
/// `SESSION_PREFIX` (`aoe_` in release, `aoe_dev_` in debug), so the single
/// root prefix matches all of them and never a release session from a debug
/// build (or vice versa).
fn is_aoe_session(name: &str) -> bool {
    name.starts_with(SESSION_PREFIX)
}

/// The `_<id8>` tail every tmux session name aoe derives for a session id
/// carries. Immutable across renames: only the title portion of the name
/// moves, so this is the durable handle from a session row to its panes.
fn id_suffix(session_id: &str) -> String {
    format!("_{}", crate::cli::truncate_id(session_id, 8))
}

/// Auxiliary kinds whose prefixes nest under `SESSION_PREFIX`, so the agent
/// shape has to exclude them explicitly.
const AGENT_EXCLUDED_PREFIXES: &[&str] = &[TERMINAL_PREFIX, CONTAINER_TERMINAL_PREFIX, TOOL_PREFIX];

/// How one kind of aoe tmux session's name is shaped for one session id, so a
/// live session can still be found after the title embedded in the name has
/// gone stale. Every name of a given kind is
/// `<prefix><sanitized title><suffix>`, and only the title in the middle moves.
///
/// - agent: prefix `aoe_`, suffix `_<id8>`, excluding the auxiliary prefixes
/// - paired terminal: prefix `aoe_term_`, suffix `_<id8>` (or `_<id8>_t<N>`)
/// - container terminal: prefix `aoe_cterm_`, same suffixes
/// - tool: prefix `aoe_tool_<tool>_`, suffix `_<id8>`
pub(crate) struct NameShape<'a> {
    pub prefix: &'a str,
    pub suffix: &'a str,
    /// Prefixes nesting under `prefix` that must never be adopted. Empty for
    /// every kind but the agent, whose `aoe_` prefixes all the others.
    pub excluded_prefixes: &'a [&'a str],
}

impl NameShape<'_> {
    /// The agent shape for a session id. The suffix must outlive the shape, so
    /// the caller owns it (see [`id_suffix`]).
    pub(crate) fn agent<'a>(suffix: &'a str) -> NameShape<'a> {
        NameShape {
            prefix: SESSION_PREFIX,
            suffix,
            excluded_prefixes: AGENT_EXCLUDED_PREFIXES,
        }
    }

    /// The paired-terminal shape for a session id. `TERMINAL_PREFIX` does not
    /// nest under any other kind's prefix, so nothing is excluded.
    pub(crate) fn terminal<'a>(suffix: &'a str) -> NameShape<'a> {
        NameShape {
            prefix: TERMINAL_PREFIX,
            suffix,
            excluded_prefixes: &[],
        }
    }

    /// The container-terminal shape for a session id. `CONTAINER_TERMINAL_PREFIX`
    /// does not nest under any other kind's prefix, so nothing is excluded.
    pub(crate) fn container<'a>(suffix: &'a str) -> NameShape<'a> {
        NameShape {
            prefix: CONTAINER_TERMINAL_PREFIX,
            suffix,
            excluded_prefixes: &[],
        }
    }

    /// True when `name` has this shape. A name whose sanitized title pushes it
    /// under an excluded prefix fails here, so it never resolves and callers
    /// keep their title-derived name: mistaking a paired terminal for the agent
    /// pane would be worse than not resolving at all.
    fn matches(&self, name: &str) -> bool {
        name.starts_with(self.prefix)
            && name.ends_with(self.suffix)
            && !self.excluded_prefixes.iter().any(|p| name.starts_with(p))
    }
}

/// True when `tmux_name` is the agent tmux session belonging to `session_id`,
/// whatever title was embedded in it when it was created. Use this instead of
/// comparing against `Session::generate_name`: the stored title moves under a
/// rename (smart rename, or a manual one whose tmux rename failed) while the
/// live session keeps the name it was created with, so an equality check
/// against the freshly derived name misses the very session it is looking for.
pub fn agent_session_belongs_to(tmux_name: &str, session_id: &str) -> bool {
    NameShape::agent(&id_suffix(session_id)).matches(tmux_name)
}

/// One tmux observation shared by a batch of per-instance liveness lookups.
///
/// A pass that asks "is this instance's pane live?" once per stored session
/// otherwise pays a `list-sessions` fork per instance, plus a `pane_dead`
/// fork per match. `compose_exclusion_with_persisted_peers` walks every
/// session sharing the project path, trashed ones included, so on a store of
/// a few hundred that is a few hundred `fork`+`exec` round-trips per pass, on
/// the thread that also serves input.
///
/// The observations are *fresh*, not cached: the session cache's answers are
/// asymmetric (a hit proves existence, a miss only means "not seen at the last
/// scan"), and liveness here decides both peer exclusion and env publication,
/// where a false negative and a false positive are each harmful. One live
/// observation per pass leaves the decision exactly as authoritative as the
/// per-item probe it replaces.
///
/// Each observation is taken on first use, and only if used: a pass that ends
/// up asking nothing, because no stored peer shares the project path or every
/// row short-circuits before the liveness clause, forks nothing, and a caller
/// that only needs session names never forks `list-panes`.
///
/// An unreachable server is preserved rather than collapsed into "absent":
/// [`Self::names`] returns `None`, so a one-shot caller that cannot retry can
/// tell Unknown from Absent and probe per row instead (see
/// `Instance::tmux_env_session_name_in_or_probe`).
#[derive(Default)]
pub(crate) struct LiveSessionSnapshot {
    names: OnceLock<Option<Vec<String>>>,
    panes: OnceLock<Option<HashMap<String, PaneMetadata>>>,
}

impl LiveSessionSnapshot {
    /// A snapshot for one pass: at most one `list-sessions` and at most one
    /// `list-panes -a`, however many instances are then looked up.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Build a snapshot from already-known parts, for tests that must not
    /// depend on a live tmux server.
    #[cfg(test)]
    pub(crate) fn from_parts(
        names: Option<Vec<String>>,
        panes: Option<HashMap<String, PaneMetadata>>,
    ) -> Self {
        let snapshot = Self::new();
        let _ = snapshot.names.set(names);
        let _ = snapshot.panes.set(panes);
        snapshot
    }

    /// Live session names, or None when the tmux server could not be reached.
    /// The fresh observation also warms the display cache, so TUI startup can
    /// reuse this pass instead of issuing another list-sessions command.
    pub(crate) fn names(&self) -> Option<&[String]> {
        self.names
            .get_or_init(|| {
                if refresh_session_cache() != SessionCacheRefresh::Populated {
                    return None;
                }
                SESSION_CACHE.read().ok().and_then(|cache| {
                    cache
                        .data
                        .as_ref()
                        .map(|sessions| sessions.keys().cloned().collect())
                })
            })
            .as_deref()
    }

    /// Whether `name`'s first pane is dead, mirroring `utils::is_pane_dead`
    /// but read from the batched metadata. An absent entry is reported alive,
    /// matching the per-item probe's `unwrap_or(false)` on a failed query.
    ///
    /// Only reachable once [`Self::names`] has produced a candidate, so an
    /// unreachable server never pays for this observation.
    pub(crate) fn pane_dead(&self, name: &str) -> bool {
        self.panes
            .get_or_init(|| batch_pane_metadata().ok())
            .as_ref()
            .and_then(|panes| panes.get(name))
            .map(|meta| meta.pane_dead)
            .unwrap_or(false)
    }
}

/// [`live_any_kind_name_for_id`] against an already-taken snapshot, so a batch
/// of lookups costs one observation instead of one per instance.
pub(crate) fn live_any_kind_name_for_id_in(
    snapshot: &LiveSessionSnapshot,
    session_id: &str,
) -> Option<String> {
    let names = snapshot.names()?;
    let suffix = id_suffix(session_id);
    let agent = NameShape::agent(&suffix);
    let terminal = NameShape::terminal(&suffix);
    let container = NameShape::container(&suffix);
    let (mut agent_hit, mut terminal_hit, mut container_hit) = (None, None, None);
    for name in names {
        let name = name.as_str();
        let bucket = if agent.matches(name) {
            &mut agent_hit
        } else if terminal.matches(name) {
            &mut terminal_hit
        } else if container.matches(name) {
            &mut container_hit
        } else {
            continue;
        };
        if bucket.is_none() && !snapshot.pane_dead(name) {
            *bucket = Some(name.to_string());
        }
    }
    agent_hit.or(terminal_hit).or(container_hit)
}

/// The live tmux session name carrying `session_id`'s `_<id8>` tail, preferring
/// the agent pane, then a paired terminal, then a container terminal, skipping
/// dead panes (tool sub-sessions never match any of these shapes). Unlike
/// [`resolve_agent_session_name`] this takes no title-derived name: it is an
/// id -> live-name lookup for liveness checks and poller-spawn resolution,
/// where any live pane for the id is evidence the session exists. Matching runs
/// through [`NameShape`] so the name shapes stay the single source of truth.
pub(crate) fn live_any_kind_name_for_id<'a>(
    live_names: impl IntoIterator<Item = &'a str>,
    session_id: &str,
) -> Option<String> {
    let suffix = id_suffix(session_id);
    let agent = NameShape::agent(&suffix);
    let terminal = NameShape::terminal(&suffix);
    let container = NameShape::container(&suffix);
    let (mut agent_hit, mut terminal_hit, mut container_hit) = (None, None, None);
    for name in live_names {
        let bucket = if agent.matches(name) {
            &mut agent_hit
        } else if terminal.matches(name) {
            &mut terminal_hit
        } else if container.matches(name) {
            &mut container_hit
        } else {
            continue;
        };
        if bucket.is_none() && !utils::is_pane_dead(name) {
            *bucket = Some(name.to_string());
        }
    }
    agent_hit.or(terminal_hit).or(container_hit)
}

/// The tmux session name to act on for one of a session's panes, resolved
/// against `live_names` (any iterator of live tmux session names).
///
/// `derived` is the title-derived name and stays the answer unless it is absent
/// from `live_names` while exactly one other live session fits `shape`. That
/// one case is a session whose stored title moved without its tmux session
/// being renamed: adopting the live name keeps stop / archive / trash / attach
/// / status pointed at the running pane instead of a name that never existed,
/// and keeps `create` from spawning a second pane beside it. Two candidates are
/// ambiguous, so `derived` wins there as well.
pub(crate) fn resolve_session_name<'a>(
    live_names: impl IntoIterator<Item = &'a str>,
    derived: &str,
    shape: &NameShape,
) -> String {
    let mut adopted: Option<&str> = None;
    let mut ambiguous = false;
    let mut derived_is_live = false;
    for name in live_names {
        // Test `derived` on its own rather than through the shape: a title that
        // sanitizes under an excluded prefix makes the derived name fail
        // `matches`, and a live derived name must still win over an older
        // session rather than be filtered out of its own match.
        if name == derived {
            derived_is_live = true;
            continue;
        }
        if !shape.matches(name) {
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

/// `resolve_session_name` for the agent pane, against `live_names`.
pub fn resolve_agent_session_name<'a>(
    live_names: impl IntoIterator<Item = &'a str>,
    session_id: &str,
    derived: &str,
) -> String {
    let suffix = id_suffix(session_id);
    resolve_session_name(live_names, derived, &NameShape::agent(&suffix))
}

/// [`resolve_agent_session_name`] against a [`batch_pane_metadata`] snapshot
/// the caller is about to index, with an O(1) fast path for the overwhelmingly
/// common case where the derived name is live. Without it the per-instance poll
/// loops would each scan every live session on every pass.
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

/// [`resolve_session_name`] against the shared session cache, refreshing a
/// stale snapshot once. Falls back to `derived` when the tmux server cannot be
/// reached, matching every other lookup here: an unreachable server is not
/// evidence about any name.
///
/// Every session kind's `resolve_name` goes through this, so a retitled
/// session's agent pane, paired terminals, and tool sub-sessions all stay
/// reachable under their original names.
pub(crate) fn live_session_name(derived: &str, shape: &NameShape) -> String {
    if let Some(name) = session_name_from_cache(derived, shape) {
        return name;
    }
    refresh_session_cache();
    session_name_from_cache(derived, shape).unwrap_or_else(|| derived.to_string())
}

/// Display variant of live_session_name, answered from the last successful
/// snapshot only and never refreshing. Display keeps using that map while it
/// is stale or an unexpected refresh fails; only a populated miss or recognized
/// no-server response changes visible liveness. Paint must never wait on tmux,
/// so this path has no synchronous fallback.
pub(crate) fn session_name_for_display(derived: &str, shape: &NameShape) -> String {
    let Ok(cache) = SESSION_CACHE.read() else {
        return derived.to_string();
    };
    resolve_session_name_from_snapshot(cache.data.as_ref(), derived, shape)
}

/// `session_name_for_display` for the agent pane.
pub(crate) fn agent_session_name_for_display(session_id: &str, derived: &str) -> String {
    let suffix = id_suffix(session_id);
    session_name_for_display(derived, &NameShape::agent(&suffix))
}

/// `live_session_name` for the agent pane.
pub fn live_agent_session_name(session_id: &str, derived: &str) -> String {
    let suffix = id_suffix(session_id);
    live_session_name(derived, &NameShape::agent(&suffix))
}

fn resolve_session_name_from_snapshot(
    names: Option<&HashMap<String, i64>>,
    derived: &str,
    shape: &NameShape,
) -> String {
    let Some(names) = names else {
        return derived.to_string();
    };
    if names.contains_key(derived) {
        return derived.to_string();
    }
    resolve_session_name(names.keys().map(String::as_str), derived, shape)
}

/// Resolve from the current authoritative cache snapshot without spawning.
/// Returns None only when the snapshot is stale or the lock is poisoned, so
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

/// Force-stop every aoe-owned tmux session (agent, terminal, container
/// terminal, tool) in this namespace. Mirrors `kill_all_tool_sessions_for_id`
/// but sweeps the whole `SESSION_PREFIX` namespace. Returns the number of
/// sessions killed. Refreshes the session cache once at the end.
///
/// `Err` means the `tmux list-sessions` process could not be spawned (e.g.
/// tmux is not installed), which callers should treat as a failed surface. A
/// non-zero exit (no server running, hence no sessions) is `Ok(0)`, and
/// per-session kills stay best-effort.
///
/// ponytail: per-session `kill_process_tree` is sequential and each does a
/// fixed 100ms SIGTERM grace, so a sweep of N sessions blocks ~N*100ms. Fine
/// for a panic button with a handful of sessions; if counts grow, batch the
/// SIGTERM across all pids, wait once, then SIGKILL survivors.
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

/// Batch-fetch pane metadata for all aoe sessions in a single tmux subprocess call.
/// Returns a map from session name to metadata for the first window's first pane.
///
/// Returns `Err` when the underlying `tmux list-panes` call fails to spawn or
/// exits non-zero. Callers MUST distinguish this from `Ok(map)` where a missing
/// key means the session is genuinely absent: `Err` means we don't know.
/// Startup recovery and status pollers treat `Err` as "skip this pass" to
/// avoid acting on a possibly-live pane during a transient tmux glitch. A
/// successful empty map is authoritative and means there are no panes.
pub fn batch_pane_metadata() -> anyhow::Result<HashMap<String, PaneMetadata>> {
    let start = Instant::now();
    let mut command = tmux_query_command();
    command.args([
        "list-panes",
        "-a",
        "-F",
        // `pane_pid` stays at the end of the pipe-separated head, where
        // the parser splits it back off the start command's tail; the two
        // fields after it ride [`TAIL_SEP`], because a start command or a
        // title may carry a pipe of its own.
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

    // Trace, not debug: paired with refresh_session_cache in the TUI
    // status poll loop (~every 2s). Debug-level here would dominate the
    // idle log.
    tracing::trace!(
        target: "tmux.pane",
        sessions = result.as_ref().map(|m| m.len()).unwrap_or(0),
        duration_ms = start.elapsed().as_millis() as u64,
        "batch pane metadata fetched",
    );
    result
}

/// Names of aoe tmux sessions that currently have at least one attached
/// client, from a single `tmux list-sessions` call.
///
/// Used by the idle auto-stop reapers (#1690) to spare a session the user is
/// reading. Returns `Err` when the underlying tmux call fails to spawn or
/// exits non-zero: callers MUST treat `Err` as "don't know, skip this reap
/// pass" rather than "nothing attached", so a transient tmux glitch cannot
/// kill a pane the user is sitting in.
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
                    // `#{session_attached}` is the attached client count; any
                    // non-zero value means a client is attached.
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

/// Parse the output of `tmux list-panes -a` into a map of session name to pane metadata.
/// Filters to aoe sessions, pane index 0, and takes only the first window per session.
fn parse_pane_metadata(output: &str) -> HashMap<String, PaneMetadata> {
    let mut map = HashMap::new();

    for line in output.lines() {
        // The two trailing fields ride their own separator (see TAIL_SEP), so
        // the pipe-separated head parses exactly as it did before them; a line
        // with no tail is all head. Accept tmux 3.4's octal rendering as well
        // as the raw byte emitted by newer versions.
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
        // The start command may itself contain the separator, so the pid is
        // split off the tail rather than the command off the head.
        let (pane_start_command, pane_pid) = match rest.rsplit_once(FIELD_SEP) {
            Some((command, pid)) => (command, pid.trim().parse().ok()),
            None => (rest, None),
        };
        if !session_name.starts_with(SESSION_PREFIX) {
            continue;
        }

        // Only take pane 0 (the agent pane). aoe pins pane-base-index to 0.
        if pane_index != "0" {
            continue;
        }

        // First occurrence per session = first window's pane 0 (list-panes
        // returns windows in index order).
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

/// Observed window geometry for `session_name` from the shared list-panes
/// snapshot, with the snapshot's observation time: the instant captured
/// before the `list-panes` fork, not when the result was published. `None`
/// when the snapshot is absent, failed, or does not include the session.
pub(crate) fn observed_window_size_from_cache(session_name: &str) -> Option<((u16, u16), Instant)> {
    let cache = PANE_META_CACHE.read().ok()?;
    let time = cache.time?;
    let size = cache.data.as_ref()?.get(session_name)?.window_size?;
    Some((size, time))
}

/// Test-only: inject a synthetic session name into the cache so
/// callers of `session_exists_from_cache` see it as present. Used
/// by live-send tests that install a fake `LiveSendState` without a
/// real tmux pane; without this the per-keystroke drift check
/// (which calls `session_exists_from_cache`) trips in CI runs that
/// have already populated the cache via the e2e suite, causing the
/// drift detector to flag the fake session as gone.
#[cfg(test)]
pub fn test_inject_session_into_cache(name: &str) {
    if let Ok(mut cache) = SESSION_CACHE.write() {
        let map = cache.data.get_or_insert_with(HashMap::new);
        map.insert(name.to_string(), 0);
        cache.time = Some(Instant::now());
    }
}

/// Test-only: publish a pane snapshot carrying a window size for `name`, so
/// the render-side observed-size invalidation can be exercised without a
/// real tmux server. Observed "now", the common case.
#[cfg(test)]
pub fn test_inject_pane_window_size(name: &str, size: (u16, u16)) {
    test_inject_pane_window_size_at(name, size, Instant::now());
}

/// Test-only: like [`test_inject_pane_window_size`], but with an explicit
/// observation time. Routes through [`publish_pane_meta_cache`], the real
/// publication path, so a regression that re-stamped `cache.time` at publish
/// instead of observation is caught by the tests using this.
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

/// Test-only instrumentation at the process's single tmux entry point.
///
/// `tmux_command()` records one hit per invocation on the *current* thread
/// while that thread is armed, so a paint-path regression test can assert
/// zero forks from the render thread while worker threads (capture, live
/// send) fork freely. Never compiled outside `cfg(test)`.
#[cfg(test)]
pub(crate) mod fork_probe {
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    /// Arms fork counting for the calling thread until the guard drops.
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

    /// Returns the armed thread's fork count since the last call.
    pub(crate) fn take() -> u64 {
        COUNT.with(|c| {
            let n = c.get();
            c.set(0);
            n
        })
    }
}

/// Test-only RAII guard for tests that force [`SESSION_CACHE`] into a known
/// state (e.g. simulating a server-unreachable snapshot for
/// [`probe_session_existence`]). Captures the prior cache on construction and
/// restores it on `Drop`, so a mid-test panic can never leak a forced cache
/// state into a later test; pair with `#[serial_test::serial]` since the
/// cache is process-global.
#[cfg(test)]
pub(crate) struct SessionCacheGuard {
    prev_data: Option<HashMap<String, i64>>,
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

    /// Save and restore the cache without suppressing refresh publication.
    pub(crate) fn capture_restore_only() -> Self {
        Self::capture_inner(false)
    }

    fn capture_inner(forced_snapshot: bool) -> Self {
        // The lock makes guard registration and the state snapshot atomic
        // against a concurrent refresh publisher.
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

    /// Force a fresh "server unreachable" snapshot: mirrors what
    /// `refresh_session_cache` writes when `list-sessions` fails.
    pub(crate) fn force_unreachable(&self) {
        if let Ok(mut cache) = SESSION_CACHE.write() {
            cache.data = None;
            cache.time = Some(Instant::now());
            cache.outcome = SessionCacheRefresh::Unknown;
        }
    }

    /// Force a fresh "server reachable" snapshot containing exactly `names`.
    pub(crate) fn force_present(&self, names: &[&str]) {
        if let Ok(mut cache) = SESSION_CACHE.write() {
            cache.data = Some(names.iter().map(|n| (n.to_string(), 0)).collect());
            cache.time = Some(Instant::now());
            cache.outcome = SessionCacheRefresh::Populated;
        }
    }

    /// Force an EXPIRED snapshot with data intact: what the shared cache
    /// looks like just past [`CACHE_TTL`] after a successful refresh. Paint
    /// must answer from it anyway instead of re-forking.
    pub(crate) fn force_stale(&self) {
        if let Ok(mut cache) = SESSION_CACHE.write() {
            cache.time = Some(Instant::now() - CACHE_TTL - Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
impl Drop for SessionCacheGuard {
    fn drop(&mut self) {
        // Deregistered under the same lock the restore takes, mirroring
        // `capture`: a refresh arriving mid-drop is either suppressed or lands
        // on top of the restored snapshot, never dropped on the floor.
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

/// [`SessionCacheGuard`] for [`PANE_META_CACHE`]: captures the prior snapshot
/// and restores it on `Drop` so a mid-test panic cannot leak a forced state
/// into a later test. Pair with `#[serial_test::serial]`.
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

    /// Force a fresh snapshot that carries no data: what `refresh_pane_meta_cache`
    /// writes when `batch_pane_metadata` fails.
    pub(crate) fn force_failed_refresh(&self) {
        if let Ok(mut cache) = PANE_META_CACHE.write() {
            cache.data = None;
            cache.time = Some(Instant::now());
        }
    }
    /// Force an EXPIRED snapshot: the past-`CACHE_TTL` state that paint must
    /// answer from without re-forking once display helpers are cache-only.
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

/// How long a [`SESSION_CACHE`] snapshot is trusted before a lookup must
/// force a fresh `refresh_session_cache()` call.
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

/// Cached tmux `#{session_activity}` epoch (seconds) for `name`, else `None`.
/// Read-only view over the private `SESSION_CACHE`; caller refreshes first if needed.
/// Ignores the snapshot TTL on purpose: this is a best-effort AGE hint for
/// `aoe ps`, not a liveness decision.
pub fn session_activity(name: &str) -> Option<i64> {
    let cache = SESSION_CACHE.read().ok()?;
    cache.data.as_ref()?.get(name).copied()
}

/// Tri-state result of probing whether an aoe tmux session exists, per
/// [`probe_session_existence`]. Unlike a plain `bool`, this keeps "the tmux
/// server itself was unreachable" distinct from "the server answered and the
/// session is not in its list": callers must treat `Unknown` as "don't know,
/// don't act" rather than collapsing it into `Absent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionExistence {
    /// The tmux server answered and the session is in its list.
    Present,
    /// The tmux server answered and the session is not in its list.
    Absent,
    /// The shared cache cannot establish liveness (including a recognized
    /// no-server response or an unexpected query failure). This is NOT
    /// evidence the session is gone.
    Unknown,
}

/// Derive a [`SessionExistence`] from the current cache snapshot, without
/// spawning anything. Returns `None` when the snapshot is stale (older than
/// [`CACHE_TTL`]) or the cache lock is poisoned, meaning the caller must
/// refresh before it can say anything.
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
        // A recognized no-server response is conservative for lifecycle
        // callers: it still means the session cannot be proven absent.
        None => SessionExistence::Unknown,
    })
}
/// Read the current session cache without refreshing it. A stale or poisoned
/// snapshot remains unknown so async request handlers never spawn tmux.
pub(crate) fn cached_session_existence(name: &str) -> SessionExistence {
    session_existence_from_cache(name).unwrap_or(SessionExistence::Unknown)
}
/// Probe whether an aoe tmux session exists, distinguishing "confirmed
/// absent" from "couldn't tell because the tmux server was unreachable".
///
/// Reuses `SESSION_CACHE`: a fresh snapshot answers immediately, a stale
/// one triggers a single [`refresh_session_cache`] call and re-derives from
/// the result. Callers that only care about "known-live" (never latch a
/// destructive action on an `Unknown`) should treat `Unknown` the same as a
/// skipped pass, mirroring [`batch_pane_metadata`] and
/// [`attached_session_names`]'s `Err` convention.
pub fn probe_session_existence(name: &str) -> SessionExistence {
    if let Some(existence) = session_existence_from_cache(name) {
        return existence;
    }
    refresh_session_cache();
    session_existence_from_cache(name).unwrap_or(SessionExistence::Unknown)
}

/// Authoritative session existence, with a cache fast-path for the positive
/// case only. The session cache is a snapshot refreshed on a ~2s cadence, so
/// its answers are asymmetric: a HIT proves the session existed as of the last
/// scan (trust it), but a MISS is unreliable, a session created since the scan
/// reads as absent. Trusting a cached miss is what made teardown and drift
/// decisions racy; here a miss (or a stale/absent cache) falls through to a
/// live `has-session`, keeping existence checks free of false negatives while
/// preserving the fast path for sessions that do exist.
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

/// Session liveness for a **render path**, answered from the shared snapshot
/// only: never a per-name probe, never a synchronous refresh.
///
/// [`session_exists`] falls through to a live `has-session` on a cache miss so
/// teardown and drift decisions can't act on a cached false negative. A row
/// glyph is neither of those, and that fallback costs one fork per call: the
/// Terminal-view list called it once per visible row per frame, which measured
/// 52ms/frame at 30 rows (1.7ms/row) against 47us for the agent view, whose
/// rows read `Instance.status` straight from the poller's batched snapshot.
///
/// The trade is that a session created since the last scan reads as absent for
/// up to `CACHE_TTL`, and an expired snapshot reads as absent until the
/// background [`spawn_snapshot_poller`] refreshes it. Call sites that start or
/// kill a pane already force a [`refresh_session_cache`], so the glyph flips
/// immediately there; the poller covers panes created behind this process's
/// back. Paint must never wait on tmux, so there is no synchronous fallback.
/// An expired or unexpectedly failed refresh retains the last successful map;
/// a populated miss or recognized no-server response removes the session.
pub fn session_exists_for_display(name: &str) -> bool {
    SESSION_CACHE
        .read()
        .ok()
        .and_then(|cache| cache.data.as_ref().map(|map| map.contains_key(name)))
        .unwrap_or(false)
}

/// Pane-dead state for a **render path**, from a shared `list-panes -a`
/// snapshot refreshed at most once per `CACHE_TTL`.
///
/// The per-name `utils::is_pane_dead` forks a `display-message` on every
/// call, so the Tool view paid two forks per row per frame (this plus
/// existence). See [`session_exists_for_display`] for the measurement and the
/// staleness trade.
///
/// Returns `false` ("not known to be dead") for a session missing from the
/// snapshot and whenever the snapshot could not be produced, matching
/// [`batch_pane_metadata`]'s contract that an `Err` means "don't know" rather
/// than "everything is dead". Callers gate on existence first, so a missing
/// key is an absent session rather than a live pane.
pub fn pane_dead_for_display(name: &str) -> bool {
    pane_dead_from_cache(name).unwrap_or(false)
}

/// Resolve from the current pane snapshot without spawning. `None` only when
/// the snapshot is stale or the lock is poisoned, so the caller knows a
/// refresh could still change the answer. A fresh snapshot with no data is an
/// answer (`Some(false)`, "can't tell, don't claim dead"), not a stale one, or
/// every row would re-refresh into the same failure.
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

/// Repopulate [`PANE_META_CACHE`]. The timestamp is stamped even when the
/// query fails, so a tmux outage costs one fork per poller cycle
/// ([`CACHE_TTL`] / 2) instead of one per row per frame.
///
/// `taken_at` must be captured BEFORE the `list-panes` fork: consumers
/// compare it against their own write times (`passive_synced_contradicted`),
/// and a publish-time stamp would let a listing that read pre-resize sizes,
/// then stalled past the resize's adoption, masquerade as a fresher
/// observation.
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
/// One queued passive preview resize, pushed by the render thread when its
/// debounce fires and executed by the dedicated passive-resize worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PassiveResizeIntent {
    pub session_id: String,
    pub session_name: String,
    pub cols: u16,
    pub rows: u16,
    /// Resize before any queued non-priority work: this is the session the
    /// user is currently viewing, so its pane must be correct first.
    pub priority: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PassiveResizeWork {
    intent: PassiveResizeIntent,
    generation: u64,
}

/// A passive resize the worker finished. The render thread consumes these to
/// adopt the per-session (cols, rows) dedup on success, or to park a declined
/// geometry so background sessions get one attempt per geometry change
/// instead of a per-frame retry loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PassiveResizeDone {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
    /// The window row count actually applied (`rows` plus status-bar chrome)
    /// so render can later spot an external resize by comparing the observed
    /// window size against `(cols, this)`. `None` when the resize did not
    /// happen: the session is missing, a client is attached, a size owner is
    /// active, or tmux errored.
    pub applied_window_rows: Option<u16>,
    generation: u64,
}

/// Geometry the worker is executing (or has finished, pending render
/// adoption). Suppresses identical re-queues until the completion is adopted.
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

/// Replace any queued resize for the same session with the latest geometry.
/// Paint can queue every frame while the pending slot stays armed; an exact
/// in-flight geometry is ignored until render consumes its completion. A
/// different geometry supersedes the pending slot. This bounds the queue by
/// the number of sessions even if the worker is delayed or restarts.
fn queue_latest_passive_resize(
    queue: &mut Vec<PassiveResizeWork>,
    in_flight: &[PassiveResizeTicket],
    work: PassiveResizeWork,
) {
    // Any newly wanted geometry supersedes the queued one for this session,
    // even when it returns to the currently in-flight geometry.
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

/// Queue a passive preview resize for its worker. Non-blocking by contract:
/// this is called from paint. Spawns the worker on first use (a fresh worker
/// drains the queue before its first park, so no wakeup is lost).
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

/// Cancel queued geometry once render observes that the completed geometry is
/// already the one wanted. Any other pending size for this session is stale.
pub(crate) fn cancel_pending_passive_resize(session_id: &str) {
    let mut queue = PASSIVE_RESIZE_INTENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    remove_pending_passive_resize(&mut queue, session_id);
}

/// Drain completions and release matching in-flight geometry only when render
/// can adopt the dedup. A newer geometry for the same session remains active.
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

/// Execute one queued passive resize under an atomic final tmux guard. The
/// worker first rejects a missing session; the Session helper then fences both
/// a newly attached client and a size-owner takeover at resize execution. A
/// resize the guard refuses (or that errors) still completes, as declined, so
/// render can park the geometry instead of retrying it every frame.
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

/// Pop the head of the queue. One item at a time, not a batch snapshot: a
/// priority intent (the session the user is viewing) queued mid-drain is
/// front-inserted and picked on the very next iteration instead of waiting
/// out a fleet-sized batch.
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
/// Background poller that keeps the session and pane metadata snapshots fresh
/// so every cache-only display helper can answer without forking from paint.
/// Commands run under the shared timeout; a tmux outage costs two bounded forks
/// per cycle, never per row or per frame. A panicking cycle is logged and
/// retried by the same thread; a failed thread spawn clears the latch so a
/// later call retries.
///
/// Idempotent while the poller is running. The daemon thread dies with the
/// process.
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
            // Half the TTL, not the TTL: the refresh work itself takes time
            // and the timestamps are stamped when each query lands, so a
            // full-TTL period would guarantee an expired-snapshot window
            // every cycle. Half keeps each snapshot fresh across the whole
            // cycle at one extra bounded fork pair per ~1s.
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

/// True when `binary` resolves on the user's PATH. An absolute or relative
/// path is checked for existence; a bare name is looked up with `which`,
/// falling back to a login shell so version-manager PATHs (NVM, etc.) are
/// loaded. Used by the `aoe add` override availability check; agent
/// detection routes through `agent_available_direct` + `login_shell_probe`
/// so a multi-agent scan shares one login shell. See #1910.
pub(crate) fn is_binary_on_path(binary: &str) -> bool {
    if binary.contains('/') || binary.contains('\\') {
        return std::path::Path::new(binary).exists();
    }
    // First try direct `which` (fast path).
    let direct = Command::new("which")
        .arg(binary)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if direct {
        return true;
    }
    // Fall back to a login shell so version-manager PATHs (NVM, etc.) are loaded.
    let shell = crate::session::user_shell();
    Command::new(&shell)
        .args(["-lc", &format!("which {}", shell_words::quote(binary))])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Cheap availability probe without a login shell. `Some(_)` is definitive:
/// an explicit path either exists or it doesn't, and a direct `which` /
/// version-run hit proves the agent is present. `None` means "not found on
/// the inherited PATH", which is inconclusive because version-manager PATHs
/// (NVM, etc.) only materialize inside a login shell; the caller decides
/// whether to pay for that fallback.
fn agent_available_direct(agent: &crate::agents::AgentDef) -> Option<bool> {
    use crate::agents::DetectionMethod;
    match &agent.detection {
        DetectionMethod::Which(binary) => {
            if binary.contains('/') || binary.contains('\\') {
                return Some(std::path::Path::new(binary).exists());
            }
            let found = Command::new("which")
                .arg(binary)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if found {
                Some(true)
            } else {
                None
            }
        }
        DetectionMethod::RunWithArg(binary, arg) => {
            let ok = Command::new(binary)
                .arg(arg)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                Some(true)
            } else {
                None
            }
        }
    }
}

/// One probe command per agent, chained with `;` so every probe runs
/// regardless of earlier results. Each hit prints a `AOE_AGENT_OK <name>`
/// marker line that [`parse_login_shell_probe`] picks out of whatever else
/// the user's login shell prints (motd, nvm chatter, ...).
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

/// Probe every agent in `agents` inside ONE login shell, returning the agent
/// names that resolved. The login shell itself is the expensive part (it
/// re-runs the user's whole profile: nvm, rbenv, ...; 0.5-2.5s is common),
/// so the cost must stay one shell per call regardless of how many agents
/// need the fallback. Probing each missing agent in its own login shell made
/// TUI startup hang for 5-10s once the built-in agent roster grew.
fn login_shell_probe(agents: &[&crate::agents::AgentDef]) -> std::collections::HashSet<String> {
    if agents.is_empty() {
        return std::collections::HashSet::new();
    }
    let shell = crate::session::user_shell();
    Command::new(&shell)
        .args(["-lc", &login_shell_probe_script(agents)])
        .output()
        .map(|o| parse_login_shell_probe(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

pub(crate) fn is_agent_available(agent: &crate::agents::AgentDef) -> bool {
    match agent_available_direct(agent) {
        Some(available) => available,
        None => login_shell_probe(&[agent]).contains(agent.name),
    }
}

#[derive(Debug, Clone)]
pub struct AvailableTools {
    available: Vec<String>,
}

impl AvailableTools {
    pub fn detect() -> Self {
        // Two passes so the whole roster costs at most ONE login shell.
        // Pass 1 is cheap per agent (`which` / a version run on the
        // inherited PATH); only the inconclusive rest goes to the batched
        // login-shell probe. The previous per-agent login shells made TUI
        // startup scale at ~1-2.5s per not-installed agent.
        let agents = crate::agents::AGENTS;
        let mut direct_ok = vec![false; agents.len()];
        let mut needs_shell: Vec<&crate::agents::AgentDef> = Vec::new();
        for (i, agent) in agents.iter().enumerate() {
            match agent_available_direct(agent) {
                Some(ok) => direct_ok[i] = ok,
                None => needs_shell.push(agent),
            }
        }
        let shell_found = login_shell_probe(&needs_shell);
        let mut available: Vec<String> = agents
            .iter()
            .enumerate()
            .filter(|(i, a)| direct_ok[*i] || shell_found.contains(a.name))
            .map(|(_, a)| a.name.to_string())
            .collect();

        // Append user-defined custom agents (always considered available since the
        // command may target a remote host or a wrapper script).
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
    use super::test_helpers::TmuxTestSession;
    use super::*;

    // Session names embed `SESSION_PREFIX`, which differs between release
    // (`aoe_`) and debug (`aoe_dev_`) builds. Use the constant so the same
    // test bodies cover both.
    const P: &str = SESSION_PREFIX;

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

        // A priority intent (the viewed session) is inserted ahead of queued
        // non-priority work so the worker resizes it first.
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
        // If the desired geometry returns to the in-flight one before G2 is
        // drained, the now-stale queued G2 must be removed as well.
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
        // Under `cfg(test)` the socket resolves to a shared temp path, so the
        // command must lead with `-S <path>` before any subcommand. This is
        // the isolation mechanism (#2608): every tmux call routes through the
        // same explicit socket instead of the default.
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
                Some(HashMap::from([("new-session".to_string(), 0)])),
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
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let error = run_tmux_command_with_timeout_inner(&mut command, Duration::from_millis(10))
            .expect_err("stalled client must time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn test_tmux_socket_resolves_under_test() {
        assert!(
            matches!(tmux_socket(), Some(TmuxSocket::Path(_))),
            "unit tests must isolate onto an explicit socket path, not the default socket"
        );
    }

    #[test]
    fn socket_from_config_name_maps_bare_name_to_dash_l() {
        assert_eq!(
            socket_from_config_name(Some("aoe_work".to_string())),
            Some(TmuxSocket::Name("aoe_work".to_string())),
        );
        // Surrounding whitespace is trimmed.
        assert_eq!(
            socket_from_config_name(Some("  aoe_work  ".to_string())),
            Some(TmuxSocket::Name("aoe_work".to_string())),
        );
    }

    #[test]
    fn socket_from_config_name_falls_back_for_empty_or_unset() {
        assert_eq!(socket_from_config_name(None), None);
        assert_eq!(socket_from_config_name(Some(String::new())), None);
        assert_eq!(socket_from_config_name(Some("   ".to_string())), None);
    }

    #[test]
    fn socket_from_config_name_rejects_path_separators() {
        // `-L` takes a bare name; a `/` or `\` must not silently redirect the
        // server, so these fall back to the default socket.
        assert_eq!(
            socket_from_config_name(Some("/tmp/foo.sock".to_string())),
            None
        );
        assert_eq!(socket_from_config_name(Some("a/b".to_string())), None);
        assert_eq!(socket_from_config_name(Some("a\\b".to_string())), None);
    }

    #[test]
    #[serial_test::serial]
    fn probe_session_existence_returns_present_when_fresh_cache_has_name() {
        let guard = SessionCacheGuard::capture();
        let name = format!("{P}probe_present_abc12345");
        guard.force_present(&[&name]);
        assert_eq!(probe_session_existence(&name), SessionExistence::Present);
    }

    #[test]
    #[serial_test::serial]
    fn probe_session_existence_returns_absent_when_fresh_cache_lacks_name() {
        let guard = SessionCacheGuard::capture();
        let name = format!("{P}probe_absent_abc12345");
        // Populated map, but not containing `name`: the server answered and
        // confirmed this session is not in its list.
        guard.force_present(&[&format!("{P}some_other_session")]);
        assert_eq!(probe_session_existence(&name), SessionExistence::Absent);
    }

    #[test]
    #[serial_test::serial]
    fn probe_session_existence_returns_unknown_when_server_unreachable() {
        let guard = SessionCacheGuard::capture();
        let name = format!("{P}probe_unknown_abc12345");
        // Simulates `list-sessions` failing unexpectedly (permission denied,
        // malformed socket, or spawn failure): the cache is fresh but has no
        // data. This must resolve straight from the cache, without falling
        // back to a fresh `has-session` subprocess call.
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

    /// A session id long enough that `truncate_id(.., 8)` actually truncates,
    /// so the tests exercise the real `_<id8>` tail.
    const ID: &str = "abc12345deadbeef";
    const ID8: &str = "abc12345";

    #[test]
    fn resolve_agent_session_name_prefers_the_derived_name_when_it_is_live() {
        let derived = format!("{P}Refactor_billing_{ID8}");
        let stale = format!("{P}Vikings_{ID8}");
        // Both live (a rename that created rather than renamed): the derived
        // name is the one the current title points at, so it wins.
        let names = [derived.as_str(), stale.as_str()];
        assert_eq!(
            resolve_agent_session_name(names, ID, &derived),
            derived,
            "a live derived name is never overridden"
        );
    }

    #[test]
    fn resolve_agent_session_name_adopts_the_stale_name_after_a_retitle() {
        // The reported bug: smart_rename moved the title, the tmux session
        // kept the name it was created under, so the derived name matches
        // nothing while the agent runs on under the old codename.
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
            // Same id, but the paired terminal / container terminal / tool
            // sub-sessions are not the agent pane.
            format!("{TERMINAL_PREFIX}Vikings_{ID8}"),
            format!("{CONTAINER_TERMINAL_PREFIX}Vikings_{ID8}"),
            format!("{TOOL_PREFIX}lazygit_Vikings_{ID8}"),
            // Agent-shaped, but a different session's id.
            format!("{P}Vikings_99999999"),
            // Not ours at all.
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
        // Two stale agent-shaped sessions for one id, the duplicate state a
        // pre-fix unarchive could leave behind: there is no basis to pick one,
        // so keep the derived name rather than guess which pane to kill.
        let derived = format!("{P}Refactor_{ID8}");
        let names = [format!("{P}Vikings_{ID8}"), format!("{P}Aztecs_{ID8}")];
        assert_eq!(
            resolve_agent_session_name(names.iter().map(String::as_str), ID, &derived),
            derived,
        );
    }

    #[test]
    fn resolve_agent_session_name_in_agrees_with_the_scan_on_both_paths() {
        // The poll loops go through the map wrapper for its O(1) hit path; it
        // must not diverge from the scan it short-circuits.
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
        // A title sanitizing to `term_...` collides with TERMINAL_PREFIX, so
        // the derived name fails the shape filter. Both directions must still
        // behave: adopt the stale name when only it is live, and keep the
        // derived name when it is live, rather than losing its own match to the
        // shape filter and killing the older pane.
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
        // The inverse lookup (`aoe session current` and friends): map a live
        // tmux session name back to its row without knowing the title it was
        // created under.
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
        // Unknown collapses to "not live" for the exclusion walk, which is what
        // the per-item probe did when its own `list-sessions` failed, and the
        // walk re-runs. A one-shot caller must not collapse it; that rule is
        // covered by
        // `instance::tests::one_shot_name_probes_when_the_snapshot_missed_tmux`.
        let snapshot = LiveSessionSnapshot::from_parts(None, None);
        assert_eq!(live_any_kind_name_for_id_in(&snapshot, ID), None);
    }

    #[test]
    #[serial_test::serial]
    fn live_any_kind_name_for_id_prefers_agent_then_terminal_then_container() {
        // None of these fake names is a live tmux session, so the internal
        // `is_pane_dead` probe returns false for all of them and the ordering
        // under test is the kind-preference, not liveness.
        let agent = format!("{P}Refactor_{ID8}");
        let terminal = format!("{TERMINAL_PREFIX}Refactor_{ID8}");
        let container = format!("{CONTAINER_TERMINAL_PREFIX}Refactor_{ID8}");

        let all = [agent.as_str(), terminal.as_str(), container.as_str()];
        assert_eq!(
            live_any_kind_name_for_id(all, ID).as_deref(),
            Some(agent.as_str()),
            "the agent pane wins when present"
        );
        assert_eq!(
            live_any_kind_name_for_id([terminal.as_str(), container.as_str()], ID).as_deref(),
            Some(terminal.as_str()),
            "the paired terminal is preferred over the container terminal"
        );
        assert_eq!(
            live_any_kind_name_for_id([container.as_str()], ID).as_deref(),
            Some(container.as_str()),
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
            live_any_kind_name_for_id(names.iter().map(String::as_str), ID),
            None,
            "a tool sub-session and other ids are never this session's pane"
        );
    }

    #[test]
    #[serial_test::serial]
    fn session_new_resolves_onto_a_retitled_sessions_live_name() {
        // End to end through the constructor every lifecycle op goes through
        // (`Instance::tmux_session`): with only the pre-rename session live,
        // `Session::new` under the NEW title must target it, so trash/archive
        // stop the running agent and `create` adopts it instead of spawning a
        // second one.
        let guard = SessionCacheGuard::capture();
        let stale = Session::generate_name(ID, "Vikings");
        guard.force_present(&[stale.as_str()]);

        let session = Session::new(ID, "Refactor billing module").expect("session");
        assert_eq!(session.name(), stale);
    }

    #[test]
    #[serial_test::serial]
    fn live_agent_session_name_answers_from_an_unreachable_snapshot_without_refreshing() {
        // No tmux server (the common state for a user who has not opened a
        // session yet) is an answer, not a stale snapshot: resolution must
        // return the derived name straight from the cache rather than spawn a
        // doomed `list-sessions` on every call from a render loop.
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
        // The creation path: no session for this id yet, so the name must be
        // the title-derived one `create` will spawn under.
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
        // A cached hit proves recent existence; session_exists must return
        // true from the fast path without a live query.
        //
        // Serial + guard: this writes the process-global SESSION_CACHE, and
        // running it in parallel with the serial probe_session_existence
        // tests turns their carefully-forced cache states into flakes (a
        // mid-test injection makes an "unreachable" cache look populated).
        let _guard = SessionCacheGuard::capture();
        let name = format!("{P}exists_probe_cache_hit");
        test_inject_session_into_cache(&name);
        assert!(session_exists(&name));
    }

    #[test]
    #[serial_test::serial]
    fn a_forced_cache_snapshot_survives_a_concurrent_refresh() {
        // See `forced_session_cache_active`: a refresh a parallel test
        // started must not land inside a guarded window.
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
        // tmux exits non-zero with this exact stderr when zero sessions exist.
        assert!(tmux_no_server_running(
            b"no server running on /tmp/tmux-501/default\n"
        ));
        assert!(tmux_no_server_running(b"no server running on /path.sock"));
        // The socket file itself is absent (issue #3337): also the empty case.
        assert!(tmux_no_server_running(
            b"error connecting to /path.sock (No such file or directory)"
        ));
        // The ENOENT marker is anchored to the line end, so it is still
        // detected when the socket path itself contains the phrase (#3337 F4).
        assert!(tmux_no_server_running(
            b"error connecting to /tmp/No such file or directory.sock (No such file or directory)"
        ));
    }

    #[test]
    fn tmux_no_server_running_rejects_other_errors_and_empty() {
        // A genuine tmux error must stay on the warn path.
        assert!(!tmux_no_server_running(b"can't find session: aoe_foo"));
        assert!(!tmux_no_server_running(b"usage: list-sessions"));
        assert!(!tmux_no_server_running(b""));
        // Transient strerrors reaching the error-connecting branch (tmux
        // client.c, non-ECONNREFUSED) must stay on the error path (#3327/#3328).
        // ECONNREFUSED is NOT here: tmux emits `no server running` for a dead
        // server, which is the empty case above.
        assert!(!tmux_no_server_running(
            b"error connecting to /path.sock (Permission denied)"
        ));
        assert!(!tmux_no_server_running(
            b"error connecting to /path.sock (Socket operation on non-socket)"
        ));
        // A socket path containing either marker phrase must not fake the empty
        // case on a different errno; both markers are anchored per line.
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
        // Built from TAIL_SEP itself, so a drift between the constant and the
        // `list-panes` format literal fails here instead of degrading silently
        // (no activity gate, every title rule dark).
        let output = format!(
            "{P}proj_abc12345|0|0|190|52|claude|claude{TAIL_SEP}1770000000{TAIL_SEP}✶ Working\n"
        );
        let meta = parse_pane_metadata(&output)
            .remove(&format!("{P}proj_abc12345"))
            .unwrap();
        assert_eq!(meta.window_activity, Some(1770000000));
        assert_eq!(meta.pane_title.as_deref(), Some("✶ Working"));

        // tmux 3.4 renders the control separators as unescaped octal tokens.
        // A doubled backslash belongs to the title and must not split it.
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

        // An unparsable activity or window size reads as absent, an empty
        // title as `None`, and a head with no tail at all parses as it did
        // before the fields.
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

    #[test]
    fn test_parse_pane_metadata_dead_pane() {
        let output = format!("{P}proj_abc12345|0|1|190|52|bash|bash\n");
        let map = parse_pane_metadata(&output);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert!(meta.pane_dead);
    }

    #[test]
    fn test_parse_pane_metadata_filters_non_aoe_sessions() {
        let output = format!(
            "user_session|0|0|190|52|bash|bash\n{P}proj_abc12345|0|0|190|52|claude|claude\nmy_tmux|0|0|190|52|vim|vim\n"
        );
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&format!("{P}proj_abc12345")));
    }

    #[test]
    fn test_parse_pane_metadata_filters_non_zero_panes() {
        let output = format!(
            "{P}proj_abc12345|0|0|190|52|claude|claude\n{P}proj_abc12345|1|0|190|52|bash|bash\n"
        );
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert_eq!(meta.pane_current_command.as_deref(), Some("claude"));
    }

    #[test]
    fn test_parse_pane_metadata_first_window_wins() {
        // Two windows both have pane 0, first window's data should be kept
        let output = format!(
            "{P}proj_abc12345|0|0|190|52|claude|claude\n{P}proj_abc12345|0|1|190|52|bash|bash\n"
        );
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert!(!meta.pane_dead);
        assert_eq!(meta.pane_current_command.as_deref(), Some("claude"));
    }

    #[test]
    fn test_parse_pane_metadata_empty_output() {
        assert!(parse_pane_metadata("").is_empty());
    }

    #[test]
    fn test_parse_pane_metadata_malformed_lines() {
        let output = format!("too|few|fields\n{P}proj_abc12345|0|0|190|52|claude|claude\n\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_parse_pane_metadata_empty_command() {
        let output = format!("{P}proj_abc12345|0|0|190|52||sh\n");
        let map = parse_pane_metadata(&output);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert!(meta.pane_current_command.is_none());
    }

    #[test]
    fn test_parse_pane_metadata_multiple_sessions() {
        let output = format!(
            "{P}proj_a_abc12345|0|0|190|52|claude|claude\n{P}proj_b_def67890|0|0|190|52|opencode|opencode\n{P}proj_c_ghi11111|0|1|190|52|bash|bash\n"
        );
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 3);
        assert_eq!(
            map.get(&format!("{P}proj_a_abc12345"))
                .unwrap()
                .pane_current_command
                .as_deref(),
            Some("claude")
        );
        assert_eq!(
            map.get(&format!("{P}proj_b_def67890"))
                .unwrap()
                .pane_current_command
                .as_deref(),
            Some("opencode")
        );
        assert!(map.get(&format!("{P}proj_c_ghi11111")).unwrap().pane_dead);
    }

    fn tmux_available() -> bool {
        tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    #[serial_test::serial]
    fn a_failed_pane_snapshot_is_an_answer_so_rows_do_not_re_fork() {
        // `refresh_pane_meta_cache` stamps `time` even when `batch_pane_metadata`
        // fails, and `pane_dead_from_cache` gates on `time` alone. That pairing is
        // what bounds a tmux outage to one fork per poller cycle (CACHE_TTL / 2)
        // instead of one per row per frame: if a fresh-but-empty snapshot
        // resolved to `None`, every Tool row would drive another doomed refresh,
        // which is the per-row fork this whole change removes.
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
        // The render path's contract: `session_exists_for_display` reads the
        // shared snapshot, where `session_exists` falls through to a live
        // `has-session` on a miss. Only a snapshot that DISAGREES with tmux
        // separates them, so force one that says "server reachable, zero
        // sessions" while a real pane is live. Getting this wrong costs one
        // fork per row per frame (~1.7ms each), which is what stalled the
        // Terminal-view list at ~19fps.
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = SessionCacheGuard::capture();
        let session = test_helpers::TmuxTestSession::new(&format!("{SESSION_PREFIX}display_probe"));
        let created = tmux_command()
            .args(["new-session", "-d", "-s", session.name(), "sleep 60"])
            .output()
            .expect("tmux new-session");
        assert!(created.status.success());

        // No fork between forcing the snapshot and reading it, so the TTL
        // cannot expire out from under the assertions.
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

        // And a snapshot that lists the session resolves live without tmux
        // being consulted at all.
        guard.force_present(&[session.name()]);
        assert!(session_exists_for_display(session.name()));
    }

    #[test]
    #[serial_test::serial]
    fn rekey_session_adopts_peer_renamed_pane() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
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

        // A sibling process renames the live pane without refreshing this
        // process's cache. `rekey_session` must scan first, adopt the
        // id-matching peer name, and move that same pane to the destination.
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
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let start_name = Session::generate_name(ID, "Britons");
        let final_name = Session::generate_name(ID, "Fix detach hint");
        let start_guard = TmuxTestSession::from_name(start_name.clone());
        let final_guard = TmuxTestSession::from_name(final_name.clone());
        let created = tmux_command()
            .args(["new-session", "-d", "-s", start_guard.name(), "sleep 60"])
            .output()
            .expect("tmux new-session");
        assert!(created.status.success());
        // Seed the option the way `apply_status_bar` does at session start.
        let seeded = tmux_command()
            .args(["set-option", "-t", &start_name, "@aoe_title", "Britons"])
            .output()
            .expect("tmux set-option @aoe_title");
        assert!(seeded.status.success());
        refresh_session_cache();

        assert!(rekey_session(ID, "Britons", "Fix detach hint").unwrap());

        // `status-right` renders `#{@aoe_title}`, so a stale value keeps the
        // pre-rename title on the bar until the session is restarted.
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
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        // Keep the isolated tmux server alive after the target is killed, so
        // the assertion distinguishes an absent session from a vanished server.
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
        // Populate a positive cache entry, then remove the target without
        // refreshing it. The authoritative refresh inside `rekey_session` must
        // classify the vanished pane as `Ok(false)`, keeping API/TUI callers
        // from showing a warning.
        refresh_session_cache();
        let killed = tmux_command()
            .args(["kill-session", "-t", &name])
            .output()
            .expect("tmux kill-session");
        assert!(killed.status.success());
        assert!(!rekey_session(ID, "Final rename", "No live pane").unwrap());
        drop((guard, dummy_guard));
    }

    /// Verify that the compound-command approach (export + exec) correctly
    /// passes env vars to the exec'd process while keeping secret values
    /// out of all long-lived process argv.
    ///
    /// This simulates the tmux session command:
    ///   export KEY='secret'; exec printenv KEY
    /// and verifies the secret reaches the exec'd process.
    #[test]
    #[serial_test::serial]
    fn test_export_exec_compound_command_passes_env() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        // Ensure the tmux server is already running so the test session's
        // command string doesn't end up in the server process's argv.
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

        // Simulate the compound command approach: export + exec as the session command
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

        // Poll rather than sleep a fixed interval. On a loaded runner the
        // pane can take longer than any one sleep to spawn, exec, and render,
        // and the blank capture that follows reads as a failed export rather
        // than as "not yet". `remain-on-exit on` holds the output after the
        // process dies, so waiting past the exit never loses it.
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
        // Pane should be dead (exec replaced the shell, printenv exited)
        assert!(
            pane_is_dead(),
            "Pane should be dead after exec'd command exits (lifecycle preserved)"
        );
    }

    /// Verify that after `exec` replaces the outer shell, the secret
    /// values from export statements are NOT visible in `ps` output.
    ///
    /// Note: the tmux server must already be running before this test.
    /// If the test session is the FIRST tmux process, the `tmux new-session`
    /// process becomes the server and its argv (which contains the command
    /// string with the secret) persists. In real aoe usage the server is
    /// always already running. We start a dummy session first to ensure this.
    #[test]
    #[serial_test::serial]
    fn test_export_exec_secrets_not_in_ps_after_exec() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        // Ensure the tmux server is already running so our test session's
        // command string doesn't end up in the server process's argv.
        let dummy_guard = TmuxTestSession::new("aoe_test_ps_dummy");
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

        let session_guard = TmuxTestSession::new("aoe_test_ps");
        let session_name = session_guard.name().to_string();
        let secret_value = format!("UNIQUE_SECRET_{}_xyzzy", std::process::id());

        // Simulate: export SECRET='val'; exec sleep 30
        // After exec, the shell process (whose argv contained the export) is
        // replaced by sleep, whose argv is just "sleep 30" (no secret).
        let compound_cmd = format!("export AOE_PS_TEST='{}'; exec sleep 30", secret_value);

        let output = tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                &compound_cmd,
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        // Wait for exec to complete
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Check ps output for the secret value
        let ps_output = Command::new("ps")
            .args(["auxww"])
            .output()
            .expect("ps auxww");
        let ps_text = String::from_utf8_lossy(&ps_output.stdout);

        assert!(
            !ps_text.contains(&secret_value),
            "Secret value must NOT appear in ps output after exec.\nFound '{}' in ps:\n{}",
            secret_value,
            ps_text
                .lines()
                .filter(|l| l.contains(&secret_value))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Regression guard for the 5-10s TUI startup hang: the login-shell
    /// fallback for agent detection must batch every pending agent into a
    /// single script (one login shell), not one shell per agent. A login
    /// shell re-runs the user's whole profile (nvm etc., 0.5-2.5s), so the
    /// per-launch cost has to stay O(1) in the number of missing agents.
    #[test]
    fn login_shell_probe_script_batches_all_probes_into_one_script() {
        let claude = crate::agents::get_agent("claude").unwrap();
        let vibe = crate::agents::get_agent("vibe").unwrap();
        assert!(
            matches!(
                vibe.detection,
                crate::agents::DetectionMethod::RunWithArg(_, _)
            ),
            "test premise: vibe uses RunWithArg so both detection arms are covered"
        );

        let script = login_shell_probe_script(&[claude, vibe]);

        assert!(script.contains("which claude"));
        assert!(script.contains("vibe --version"));
        assert_eq!(
            script.matches(LOGIN_PROBE_MARKER).count(),
            2,
            "one marker echo per agent, all inside the one script: {script}"
        );
        // Chained with `;` so a failed probe never short-circuits the rest.
        assert!(
            script.contains("; "),
            "probes must be `;`-chained: {script}"
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
