//! tmux integration module

pub(crate) mod composite;
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

pub use session::{PaneCursor, PaneEnvMutation, Session, SIZE_OWNER_HEARTBEAT, SIZE_OWNER_TTL};
pub use status_bar::{get_session_info_for_current, get_status_for_current_session};
pub(crate) use status_detection::{
    claude_pane_is_ambiguous_typed_prompt, claude_pane_marker_fingerprint,
    reconcile_claude_hook_status, reconcile_claude_idle_hook_status, reconcile_codex_hook_status,
    reconcile_waiting_hook,
};
pub use status_detection::{detect_status_from_content, detect_status_from_content_in};
pub use terminal_session::{kill_all_terminals_for_id, ContainerTerminalSession, TerminalSession};
pub use tool_session::{kill_all_tool_sessions_for_id, ToolSession};
pub use utils::tmux_prefix_display;

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
use std::process::Command;
use std::sync::{OnceLock, RwLock};
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
    crate::session::profile_config::resolve_config_or_warn(profile)
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

/// Like [`tmux_command`], but pins `LC_ALL=C` so tmux's connection-failure
/// messages on stderr stay stable English for callers that match them. tmux's
/// `client.c` prints `error connecting to <socket> (strerror(errno))` for a
/// non-`ECONNREFUSED` connect failure, and glibc localizes `strerror` by
/// `LC_MESSAGES`, so on a non-English host the `(No such file or directory)`
/// ENOENT marker for an absent socket (#3337) would not match. Used by the
/// status-query callers (which classify via [`tmux_no_server_running`]) and by
/// `kill_session_if_present`. NOT folded into [`tmux_command`]: the
/// interactive attach/switch-client/capture-pane paths must keep the user's
/// locale for UTF-8 and status-bar rendering.
pub(crate) fn tmux_query_command() -> Command {
    let mut cmd = tmux_command();
    cmd.env("LC_ALL", "C");
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
}

static SESSION_CACHE: RwLock<SessionCache> = RwLock::new(SessionCache {
    data: None,
    time: None,
});

struct SessionCache {
    data: Option<HashMap<String, i64>>,
    time: Option<Instant>,
}

// Field separator for multi-field tmux `-F` format strings. Must be a
// printable ASCII byte that does not appear in `sanitize_session_name` output
// (which preserves `[A-Za-z0-9_-]` and replaces everything else with `_`).
// tmux 3.4 mangles whitespace (tab, newline become `_`) and octal-escapes
// control bytes (ASCII 0x1F is emitted as the literal 4-char sequence
// `\037`), so anything non-printable is unreliable. Pipe is safe.
const FIELD_SEP: char = '|';

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

pub fn refresh_session_cache() {
    let start = Instant::now();
    let output = tmux_query_command()
        .args(["list-sessions", "-F", "#{session_name}|#{session_activity}"])
        .output();

    let new_data = match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut map = HashMap::new();
            for line in stdout.lines() {
                if let Some((name, activity)) = line.split_once(FIELD_SEP) {
                    let activity: i64 = activity.parse().unwrap_or(0);
                    map.insert(name.to_string(), activity);
                }
            }
            Some(map)
        }
        Ok(out) => {
            if tmux_no_server_running(&out.stderr) {
                tracing::trace!(target: "tmux.cache", "no tmux server running; cache cleared");
            } else {
                tracing::warn!(
                    target: "tmux.cache",
                    status = ?out.status,
                    stderr_bytes = out.stderr.len(),
                    "list-sessions returned non-zero; cache cleared",
                );
            }
            None
        }
        Err(e) => {
            tracing::warn!(target: "tmux.cache", error = %e, "list-sessions spawn failed; cache cleared");
            None
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

    if let Ok(mut cache) = SESSION_CACHE.write() {
        cache.data = new_data;
        cache.time = Some(Instant::now());
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

/// `live_session_name` for the agent pane.
pub fn live_agent_session_name(session_id: &str, derived: &str) -> String {
    let suffix = id_suffix(session_id);
    live_session_name(derived, &NameShape::agent(&suffix))
}

/// Resolve from the current cache snapshot without spawning. `None` only when
/// the snapshot is stale or the lock is poisoned, so the caller knows a refresh
/// could still change the answer.
fn session_name_from_cache(derived: &str, shape: &NameShape) -> Option<String> {
    let cache = SESSION_CACHE.read().ok()?;
    let fresh = cache
        .time
        .map(|t| t.elapsed() <= CACHE_TTL)
        .unwrap_or(false);
    if !fresh {
        return None;
    }
    // A fresh snapshot with no data means the last `list-sessions` itself
    // failed (no server running, unreachable socket). That is not evidence
    // about any name, and it is an answer, not a stale snapshot: returning
    // `None` here would make every caller re-refresh into the same failure,
    // one subprocess per call from render loops that never had a session.
    let Some(names) = cache.data.as_ref() else {
        return Some(derived.to_string());
    };
    // Fast path: the derived name is live, which is the overwhelmingly common
    // case, so skip the scan.
    if names.contains_key(derived) {
        return Some(derived.to_string());
    }
    Some(resolve_session_name(
        names.keys().map(String::as_str),
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
    let output = tmux_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .map_err(|e| anyhow::anyhow!("tmux list-sessions spawn failed: {e}"))?;

    let mut killed = 0;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if is_aoe_session(line) {
                if let Some(pid) = crate::process::get_pane_pid(line) {
                    crate::process::kill_process_tree(pid);
                }
                let _ = tmux_command().args(["kill-session", "-t", line]).output();
                killed += 1;
            }
        }
    }

    if killed > 0 {
        refresh_session_cache();
    }
    Ok(killed)
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
    let output = tmux_query_command()
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}|#{pane_index}|#{pane_dead}|#{pane_current_command}|#{pane_start_command}",
        ])
        .output();

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

/// Parse the output of `tmux list-panes -a` into a map of session name to pane metadata.
/// Filters to aoe sessions, pane index 0, and takes only the first window per session.
fn parse_pane_metadata(output: &str) -> HashMap<String, PaneMetadata> {
    let mut map = HashMap::new();

    for line in output.lines() {
        let mut parts = line.splitn(5, FIELD_SEP);
        let (
            Some(session_name),
            Some(pane_index),
            Some(pane_dead),
            Some(pane_current_command),
            Some(pane_start_command),
        ) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        )
        else {
            continue;
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
                pane_current_command: if pane_current_command.is_empty() {
                    None
                } else {
                    Some(pane_current_command.to_string())
                },
                pane_start_command_is_protected: pane_start_command
                    .contains(utils::PANE_ENV_FILE_PREFIX),
            },
        );
    }

    map
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
}

#[cfg(test)]
impl SessionCacheGuard {
    pub(crate) fn capture() -> Self {
        let cache = SESSION_CACHE.read().expect("session cache lock");
        Self {
            prev_data: cache.data.clone(),
            prev_time: cache.time,
        }
    }

    /// Force a fresh "server unreachable" snapshot: mirrors what
    /// `refresh_session_cache` writes when `list-sessions` fails.
    pub(crate) fn force_unreachable(&self) {
        if let Ok(mut cache) = SESSION_CACHE.write() {
            cache.data = None;
            cache.time = Some(Instant::now());
        }
    }

    /// Force a fresh "server reachable" snapshot containing exactly `names`.
    pub(crate) fn force_present(&self, names: &[&str]) {
        if let Ok(mut cache) = SESSION_CACHE.write() {
            cache.data = Some(names.iter().map(|n| (n.to_string(), 0)).collect());
            cache.time = Some(Instant::now());
        }
    }
}

#[cfg(test)]
impl Drop for SessionCacheGuard {
    fn drop(&mut self) {
        if let Ok(mut cache) = SESSION_CACHE.write() {
            cache.data = self.prev_data.take();
            cache.time = self.prev_time;
        }
    }
}

/// How long a [`SESSION_CACHE`] snapshot is trusted before a lookup must
/// force a fresh `refresh_session_cache()` call.
const CACHE_TTL: Duration = Duration::from_secs(2);

pub fn session_exists_from_cache(name: &str) -> Option<bool> {
    let cache = SESSION_CACHE.read().ok()?;

    if cache.time.map(|t| t.elapsed() > CACHE_TTL).unwrap_or(true) {
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
    /// The tmux server could not be reached (refused connection, stale
    /// socket, spawn failure). This is NOT evidence the session is gone.
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

    Some(match &cache.data {
        Some(map) if map.contains_key(name) => SessionExistence::Present,
        Some(_) => SessionExistence::Absent,
        // The last refresh's `list-sessions` call itself failed (non-zero
        // exit or spawn error): a definitive "can't tell", not "absent".
        // Do not fall back to a fresh `has-session` probe here; during a
        // real outage that call fails the same way and just burns a
        // subprocess per session per poll for no new information.
        //
        // This is also why a fully-down server can never resolve to
        // `Absent` here: aoe's tmux sessions run with `remain-on-exit on`,
        // so a dying agent leaves its pane dead but the session itself
        // `Present` in `list-sessions`. The only way `list-sessions` fails
        // is the server process itself being gone (crash, `kill-server`,
        // or the last session in it being killed), and that case is
        // indistinguishable from a transient connectivity blip from here.
        // Resolving it to `Unknown` freezes every polled instance at its
        // prior status until the bounded-window escalation in
        // `update_status_with_metadata_inner` kicks in; do not "fix" this
        // arm back to `Absent`, that is the false-Error-latch bug this
        // tri-state exists to prevent.
        None => SessionExistence::Unknown,
    })
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

    tmux_command()
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn get_current_session_name() -> Option<String> {
    let output = tmux_command()
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
        // Simulates the last `list-sessions` call failing (stale socket,
        // refused connection): the cache is fresh but has no data. This must
        // resolve straight from the cache, without falling back to a fresh
        // `has-session` subprocess call (which would just fail the same way
        // during a real outage).
        guard.force_unreachable();
        assert_eq!(probe_session_existence(&name), SessionExistence::Unknown);
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
        let output = format!("{P}my_proj_abc12345|0|0|claude|claude\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        let meta = map.get(&format!("{P}my_proj_abc12345")).unwrap();
        assert!(!meta.pane_dead);
        assert_eq!(meta.pane_current_command.as_deref(), Some("claude"));
        assert!(!meta.pane_start_command_is_protected);
    }

    #[test]
    fn test_parse_pane_metadata_protected_wrapper_shell_is_not_stale() {
        let output = format!(
            "{P}protected_abc12345|0|0|sh|/bin/sh -c 'prepare | . /tmp/aoe-pane-env-123 | exec claude'\n\
             {P}interactive_def67890|0|0|sh|sh\n"
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
        let output = format!("{P}proj_abc12345|0|1|bash|bash\n");
        let map = parse_pane_metadata(&output);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert!(meta.pane_dead);
    }

    #[test]
    fn test_parse_pane_metadata_filters_non_aoe_sessions() {
        let output = format!(
            "user_session|0|0|bash|bash\n{P}proj_abc12345|0|0|claude|claude\nmy_tmux|0|0|vim|vim\n"
        );
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&format!("{P}proj_abc12345")));
    }

    #[test]
    fn test_parse_pane_metadata_filters_non_zero_panes() {
        let output =
            format!("{P}proj_abc12345|0|0|claude|claude\n{P}proj_abc12345|1|0|bash|bash\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert_eq!(meta.pane_current_command.as_deref(), Some("claude"));
    }

    #[test]
    fn test_parse_pane_metadata_first_window_wins() {
        // Two windows both have pane 0, first window's data should be kept
        let output =
            format!("{P}proj_abc12345|0|0|claude|claude\n{P}proj_abc12345|0|1|bash|bash\n");
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
        let output = format!("too|few|fields\n{P}proj_abc12345|0|0|claude|claude\n\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_parse_pane_metadata_empty_command() {
        let output = format!("{P}proj_abc12345|0|0||sh\n");
        let map = parse_pane_metadata(&output);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert!(meta.pane_current_command.is_none());
    }

    #[test]
    fn test_parse_pane_metadata_multiple_sessions() {
        let output = format!(
            "{P}proj_a_abc12345|0|0|claude|claude\n{P}proj_b_def67890|0|0|opencode|opencode\n{P}proj_c_ghi11111|0|1|bash|bash\n"
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

        // Wait for printenv to run and exit
        std::thread::sleep(std::time::Duration::from_millis(1000));

        // Capture pane output: should contain the secret value
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
        let pane_content = String::from_utf8_lossy(&capture.stdout);
        assert!(
            pane_content.contains(secret_value),
            "Expected secret value in pane output (proves export reached exec'd process).\nPane:\n{}",
            pane_content
        );

        // Pane should be dead (exec replaced the shell, printenv exited)
        let dead_check = tmux_command()
            .args(["display-message", "-t", &session_name, "-p", "#{pane_dead}"])
            .output()
            .expect("pane dead check");
        let is_dead = String::from_utf8_lossy(&dead_check.stdout).trim().eq("1");
        assert!(
            is_dead,
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
