//! Centralized logging configuration + runtime filter control.
//!
//! Single source of truth for env-var resolution, default-filter
//! construction, and the reloadable subscriber handle. Both the main
//! daemon and structured view runner subprocesses use this module so they
//! agree on what `AOE_LOG_LEVEL=debug` means.
//!
//! The process-global `FilterController` is exposed via free
//! functions (`set_filter`, `set_level`, `current_filter`). Tracing's
//! subscriber is already a process-wide singleton; we mirror that
//! design rather than threading a handle through application state.
//! `Mutex<Option<Arc<_>>>` (over `OnceLock`) so tests can reset.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fs2::FileExt;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Registry;

use crate::session::config::{LoggingConfig, RotationKind};

/// Which context the running process is in. Drives whether `[logging].output`
/// is honored or coerced to `File`. Contexts where the stdout sink would
/// corrupt the UI (TUI alt-screen) or get discarded (daemon child's
/// detached stdio, structured view runner) force the file sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessContext {
    Tui,
    ServeDaemonChild,
    ServeForeground,
    Runner,
    OneShotCli,
}

/// Output of `resolve_sink`. The `warning` is deferred until after subscriber
/// init so it can be emitted through tracing rather than dropped silently.
pub struct SinkResolution {
    pub target: SubscriberTarget,
    pub warning: Option<String>,
}

/// Resolve the configured log file path. Relative `file_path` values join
/// onto `app_dir`; absolute paths are used verbatim. Used by every site
/// that names the log file (main, runner, aoe logs, TUI serve dialog).
pub fn resolve_log_path(cfg: &LoggingConfig, app_dir: &Path) -> PathBuf {
    let p = Path::new(&cfg.file_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        app_dir.join(p)
    }
}

/// Pick the subscriber sink for this process. `output = "stdout"` is
/// honored only when the context can safely write to stdout; otherwise
/// the resolution carries a `warning` describing the coercion so the
/// caller can emit it through the now-live subscriber.
pub fn resolve_sink(cfg: &LoggingConfig, app_dir: &Path, ctx: ProcessContext) -> SinkResolution {
    use crate::session::config::SinkKind;

    let force_file = matches!(
        ctx,
        ProcessContext::Tui | ProcessContext::ServeDaemonChild | ProcessContext::Runner
    );
    let want_stdout = matches!(cfg.output, SinkKind::Stdout);

    if want_stdout && !force_file {
        SinkResolution {
            target: SubscriberTarget::Stdout,
            warning: None,
        }
    } else {
        let warning = if want_stdout && force_file {
            Some(format!(
                "[logging].output = \"stdout\" ignored for {:?} (file required to avoid output corruption)",
                ctx
            ))
        } else {
            None
        };
        SinkResolution {
            target: SubscriberTarget::File(
                resolve_log_path(cfg, app_dir),
                RotationPolicy::from(cfg),
            ),
            warning,
        }
    }
}

/// Rotation thresholds resolved from `[logging]`. Built once at subscriber
/// init and held by the `SizeRotatingWriter`; not hot-swappable (sink and
/// rotation knobs require restart, see `apply_persisted_config`).
#[derive(Debug, Clone, Copy)]
pub struct RotationPolicy {
    pub kind: RotationKind,
    pub max_size_bytes: u64,
    pub keep_count: u8,
}

impl From<&LoggingConfig> for RotationPolicy {
    fn from(cfg: &LoggingConfig) -> Self {
        // Defensive clamp: a hand-edited keep_count = 0 would otherwise yield
        // a rotation that deletes the only copy. UI fields enforce >= 1; this
        // makes the invariant hold for any path into the writer.
        Self {
            kind: cfg.rotation,
            max_size_bytes: cfg.max_size_mib.saturating_mul(1024 * 1024),
            keep_count: cfg.keep_count.max(1),
        }
    }
}

/// Top-level tracing target roots. The default filter expands a
/// single level (e.g. "debug") to one directive per root so user-defined
/// targets like `auth.token`, `process.signal`, `git.command` inherit
/// the same level.
pub const DEFAULT_TARGET_ROOTS: &[&str] = &[
    "agent_of_empires",
    "acp",
    "terminal",
    "auth",
    "process",
    "update",
    "containers",
    "git",
    "migrations",
    "web",
    // `log` is the meta-target prefix for filter-swap audit events
    // (`log.runtime`). Without this, `log.runtime` would be dropped
    // under any expanded-level filter that has no global default.
    "log",
    // User-facing surfaces that previously fell under `agent_of_empires`
    // and were therefore indistinguishable from generic library code.
    // Each is a separate ownership boundary the user can dial up/down.
    "cli",
    "tui",
    "session",
    "tmux",
    "http",
    "serve",
    "hooks",
    "sound",
    "telemetry",
];

/// Sub-targets users can tune individually from the settings UI.
/// Order is the UI ordering. Anything not in this list still works
/// in the runtime endpoint as a raw filter, but won't have a dropdown.
///
/// Kept intentionally short. The list is for the UI dropdown only;
/// callers can always set arbitrary EnvFilter directives via the
/// settings TUI's raw field or `PATCH /api/log-level`. Adding an
/// entry here is only worth it when we have evidence we'll want to
/// dial that area in isolation. Sub-targets emitted by code (e.g.
/// `http.request`, `cli.serve`, `tui.home`) work fine even when not
/// listed; they just won't have a one-click row in the settings UI.
pub const KNOWN_SUB_TARGETS: &[&str] = &[
    "acp.protocol",
    "acp.protocol.stderr",
    "acp.protocol.tool_dispatch",
    "acp.supervisor",
    "acp.event_store",
    "acp.runner",
    "terminal.ws",
    "terminal.ws.bytes",
    "auth.token",
    "auth.middleware",
    "auth.rate_limit",
    "auth.passphrase",
    "auth.device",
    "auth.ip",
    "process.signal",
    "process.tree",
    "process.reap",
    "process.ppid",
    "update.fetch",
    "update.cache",
    "update.parse",
    "containers.docker",
    "containers.image",
    "containers.runtime",
    "git.command",
    "web.client",
    "log.runtime",
];

/// Apply a persisted `LoggingConfig` to the running subscriber + persist
/// runtime_filter so structured view runners pick it up via the notify watcher.
/// Both the TUI save path and the web `PATCH /api/settings` path call
/// this after `save_config`, so settings changes take effect live
/// without a daemon restart.
///
/// Only the filter (default_level plus per-target overrides) hot-swaps.
/// Sink-shape knobs (output, file_path, rotation, max_size_mib, keep_count)
/// require a process restart: the tracing subscriber is a global singleton
/// installed once at startup, and the rotating writer holds its policy
/// and file handle for the life of the process. The settings UI surfaces
/// a restart hint when those fields change.
pub fn apply_persisted_config(
    default_level: &str,
    targets: &std::collections::BTreeMap<String, String>,
    app_dir: &std::path::Path,
) {
    let Some(filter) = build_filter_from_config(default_level, targets) else {
        return;
    };
    match set_filter(&filter) {
        Ok(swap) => {
            // Skip the log and the disk write on a no-op: persisting an
            // unchanged directive needlessly re-fires every runner's watcher
            // (#1894).
            if swap.changed {
                tracing::info!(
                    target: "log.runtime",
                    previous = %swap.previous,
                    current = %swap.current,
                    source = "settings",
                    "filter swapped"
                );
                persist_runtime_filter(&swap.current, app_dir);
            }
        }
        Err(LogFilterError::Unavailable) => {
            // No reload handle installed (e.g. TUI process). Still persist
            // so a runner watching the file gets the update.
            persist_runtime_filter(&filter, app_dir);
        }
        Err(e) => {
            tracing::warn!(
                target: "log.runtime",
                error = %e,
                filter = %filter,
                "settings-driven filter swap failed"
            );
        }
    }
}

/// Compose an EnvFilter directive from a baseline level + per-target overrides.
/// Used both at startup (when no env var is set) and by the settings write path
/// when a user updates `[logging]`.
///
/// Per-target overrides win over the baseline because EnvFilter is
/// last-wins-per-target: the override directives are emitted AFTER the roots.
pub fn build_filter_from_config(
    default_level: &str,
    targets: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let baseline_level = LogLevel::parse(default_level)?;
    let mut s = LogConfig::filter_for_level(baseline_level);
    for (target, lvl) in targets {
        if target.is_empty() {
            continue;
        }
        if LogLevel::parse(lvl).is_none() {
            continue;
        }
        s.push(',');
        s.push_str(target);
        s.push('=');
        s.push_str(lvl);
    }
    Some(s)
}

/// Load `[logging]` from `config.toml` and build an EnvFilter directive.
/// Returns `None` when no config file exists, when it fails to parse, or
/// when the level value is unrecognised. Callers fall back to
/// `serve_default_filter()` in that case.
pub fn load_persisted_filter() -> Option<String> {
    let config = crate::session::load_config().ok().flatten()?;
    build_filter_from_config(&config.logging.default_level, &config.logging.targets)
}

/// Info-baseline filter directive. Used as the universal fallback when
/// neither env nor config produce one — both `aoe serve` and the TUI
/// must come up with *some* filter so the subscriber can be installed.
pub fn serve_default_filter() -> String {
    LogConfig::serve_default()
        .filter_string()
        .expect("serve_default sets a level")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Resolved logging configuration. Pure data; env-touching lives only in
/// `from_env` so the rest of the module is unit-testable without env hacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    pub level: Option<LogLevel>,
    pub acp_trace: bool,
    pub terminal_trace: bool,
}

impl LogConfig {
    pub fn from_env() -> Self {
        let level = std::env::var("AOE_LOG_LEVEL")
            .ok()
            .and_then(|v| LogLevel::parse(&v))
            .or_else(|| {
                if std::env::var("AGENT_OF_EMPIRES_DEBUG").is_ok() {
                    Some(LogLevel::Debug)
                } else {
                    None
                }
            });
        Self {
            level,
            acp_trace: std::env::var("AOE_ACP_TRACE").is_ok(),
            terminal_trace: std::env::var("AOE_TERMINAL_TRACE").is_ok(),
        }
    }

    /// Default for foreground `aoe serve` (info level, no overlays).
    pub fn serve_default() -> Self {
        Self {
            level: Some(LogLevel::Info),
            acp_trace: false,
            terminal_trace: false,
        }
    }

    /// EnvFilter directive string. None when level unset.
    pub fn filter_string(&self) -> Option<String> {
        let level = self.level?;
        let mut s = Self::filter_for_level(level);
        if self.acp_trace {
            s.push_str(
                ",agent_client_protocol=debug,agent_client_protocol::jsonrpc::transport_actor=trace",
            );
        }
        if self.terminal_trace {
            s.push_str(",terminal=trace");
        }
        Some(s)
    }

    /// Expand a level to one directive per target root.
    pub fn filter_for_level(level: LogLevel) -> String {
        let lvl = level.as_str();
        DEFAULT_TARGET_ROOTS
            .iter()
            .map(|t| format!("{t}={lvl}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

pub enum SubscriberTarget {
    File(PathBuf, RotationPolicy),
    Stdout,
}

pub struct InitResult {
    pub controller: Option<Arc<FilterController>>,
    pub warning: Option<String>,
}

pub struct FilterController {
    inner: reload::Handle<EnvFilter, Registry>,
    current: Mutex<String>,
}

impl FilterController {
    pub fn current(&self) -> String {
        self.current.lock().unwrap().clone()
    }

    pub fn set_filter(&self, directive: &str) -> Result<SwapResult, LogFilterError> {
        let directive = directive.trim();
        if directive.is_empty() {
            return Err(LogFilterError::Invalid("empty filter".into()));
        }
        // Bare global levels would enable debug for hyper/rustls/tower etc.
        // Use set_level when you mean "everything we own at this level".
        if LogLevel::parse(directive).is_some() {
            return Err(LogFilterError::BareGlobalLevel);
        }
        let filter = EnvFilter::builder()
            .with_regex(false)
            .parse(directive)
            .map_err(|e| LogFilterError::Invalid(e.to_string()))?;
        self.swap(filter, directive.to_string())
    }

    pub fn set_level(&self, level: LogLevel) -> Result<SwapResult, LogFilterError> {
        let directive = LogConfig::filter_for_level(level);
        let filter = EnvFilter::builder()
            .with_regex(false)
            .parse(&directive)
            .map_err(|e| LogFilterError::Invalid(e.to_string()))?;
        self.swap(filter, directive)
    }

    fn swap(&self, filter: EnvFilter, directive: String) -> Result<SwapResult, LogFilterError> {
        // Hold the `current` lock across the compare and the modify so two
        // concurrent swaps cannot interleave and both observe `changed`.
        let mut current = self.current.lock().unwrap();
        let previous = current.clone();
        // No-op: the active directive already equals the requested one.
        // Skip the reload-handle modify entirely and report `changed=false`
        // so callers stay silent. A no-op swap that logged at INFO is what
        // fed the file-watch OOM loop in #1894: the log line landed in the
        // watched dir and re-triggered the watcher.
        if previous == directive {
            return Ok(SwapResult {
                previous,
                current: directive,
                changed: false,
            });
        }
        self.inner
            .modify(|f| *f = filter)
            .map_err(|e| LogFilterError::Invalid(e.to_string()))?;
        *current = directive.clone();
        Ok(SwapResult {
            previous,
            current: directive,
            changed: true,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SwapResult {
    pub previous: String,
    pub current: String,
    /// `false` when the requested directive already matched the active one,
    /// so the swap was a no-op. Callers gate logging and persistence on this
    /// to avoid the self-sustaining file-watch loop (#1894).
    pub changed: bool,
}

#[derive(Debug)]
pub enum LogFilterError {
    Invalid(String),
    BareGlobalLevel,
    Unavailable,
}

impl std::fmt::Display for LogFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "invalid filter: {msg}"),
            Self::BareGlobalLevel => write!(
                f,
                "bare global level not accepted in filter mode; use set_level instead"
            ),
            Self::Unavailable => write!(f, "log subscriber not initialized in reloadable mode"),
        }
    }
}

impl std::error::Error for LogFilterError {}

/// Optional top-of-stack layer injected at subscriber init. Only the
/// serve daemon supplies a real one (the per-session tee, see
/// `crate::acp::session_tee`); the acp module is `serve`-gated, so without
/// that feature the slot is the no-op `Identity` layer and callers always
/// pass `None`.
#[cfg(feature = "serve")]
pub type TeeLayer = crate::acp::session_tee::SessionTeeLayer;
#[cfg(not(feature = "serve"))]
pub type TeeLayer = tracing_subscriber::layer::Identity;

pub fn init_subscriber(target: SubscriberTarget, filter: String) -> InitResult {
    init_subscriber_with_options(target, filter, false, None)
}

/// Event formatter that mirrors the default Full output (RFC3339-ish
/// timestamp, level, target, fields, message) but omits the span chain
/// prefix. Used when `[logging].show_spans = false`, the project
/// default, so that idle polling requests do not flood the log with
/// `http_request{request_id=... method=GET path=...}` prefixes on every
/// downstream event. The full default formatter is still available when
/// the user opts in via the settings toggle.
struct NoSpanFormat;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for NoSpanFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        write!(
            writer,
            "{}  {} {}: ",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            meta.level(),
            meta.target(),
        )?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Initialize tracing with explicit formatter options. `show_spans = true`
/// prefixes every event with the span chain (e.g. `http_request{request_id=...}`),
/// which enables grep-correlation across async boundaries but adds noise on
/// idle polling endpoints. `false` (the default) drops the prefix.
pub fn init_subscriber_with_options(
    target: SubscriberTarget,
    filter: String,
    show_spans: bool,
    session_tee: Option<TeeLayer>,
) -> InitResult {
    let parsed = match EnvFilter::builder().with_regex(false).parse(&filter) {
        Ok(f) => f,
        Err(e) => {
            return InitResult {
                controller: None,
                warning: Some(format!("invalid initial filter {filter:?}: {e}")),
            };
        }
    };
    let (reload_layer, handle) = reload::Layer::new(parsed);

    // tracing-subscriber's default Full formatter hard-codes the span
    // chain prefix into the event line, and the `with_current_span` /
    // `with_span_list` toggles only exist on the JSON formatter (not on
    // `fmt::Layer` or `format::Format<Full, _>` for non-JSON output).
    // When `show_spans` is false we therefore install a small custom
    // FormatEvent that emits the same timestamp / level / target /
    // message but skips the span list. When true we install the default
    // Full formatter.
    let install_result = match target {
        SubscriberTarget::File(path, policy) => match SizeRotatingWriter::new(path.clone(), policy)
        {
            Ok(mut writer) => {
                // Raw marker is written before tracing takes ownership so it
                // appears in the file even when the user's filter would drop
                // an info-level event. Forensic boundary; not load-bearing
                // for the TUI dialog (which uses captured offset).
                write_raw_startup_marker(&mut writer);
                let mw = std::sync::Mutex::new(writer);
                if show_spans {
                    let fmt_layer = tracing_subscriber::fmt::layer()
                        .with_writer(mw)
                        .with_ansi(false);
                    Registry::default()
                        .with(reload_layer)
                        .with(fmt_layer)
                        .with(session_tee)
                        .try_init()
                        .map_err(|e| e.to_string())
                } else {
                    let fmt_layer = tracing_subscriber::fmt::layer()
                        .with_writer(mw)
                        .with_ansi(false)
                        .event_format(NoSpanFormat);
                    Registry::default()
                        .with(reload_layer)
                        .with(fmt_layer)
                        .with(session_tee)
                        .try_init()
                        .map_err(|e| e.to_string())
                }
            }
            Err(e) => Err(format!("open log file {}: {e}", path.display())),
        },
        SubscriberTarget::Stdout => {
            // Marker on stdout too so a piped foreground serve preserves the
            // boundary in tools that grep the captured output.
            write_raw_startup_marker(&mut std::io::stdout());
            if show_spans {
                let fmt_layer = tracing_subscriber::fmt::layer().with_ansi(false);
                Registry::default()
                    .with(reload_layer)
                    .with(fmt_layer)
                    .with(session_tee)
                    .try_init()
                    .map_err(|e| e.to_string())
            } else {
                let fmt_layer = tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .event_format(NoSpanFormat);
                Registry::default()
                    .with(reload_layer)
                    .with(fmt_layer)
                    .with(session_tee)
                    .try_init()
                    .map_err(|e| e.to_string())
            }
        }
    };

    match install_result {
        Ok(()) => {
            let controller = Arc::new(FilterController {
                inner: handle,
                current: Mutex::new(filter),
            });
            // Best-effort tracing marker (respects user filter). Cheap to
            // emit and useful for grep when filter allows info.
            tracing::info!(
                target: "log.runtime",
                version = env!("CARGO_PKG_VERSION"),
                pid = std::process::id(),
                "aoe started"
            );
            InitResult {
                controller: Some(controller),
                warning: None,
            }
        }
        Err(msg) => InitResult {
            controller: None,
            warning: Some(msg),
        },
    }
}

/// Append a one-line marker directly through the writer before the tracing
/// subscriber takes it over. Filter-immune so it survives any user level
/// setting and gives forensic readers a boundary between process runs.
fn write_raw_startup_marker(writer: &mut dyn Write) {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let line = format!(
        "{} INFO log.runtime [AOE_START_MARKER] version={} pid={} exe={}\n",
        chrono::Utc::now().to_rfc3339(),
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        exe,
    );
    let _ = writer.write_all(line.as_bytes());
    let _ = writer.flush();
}

/// Multi-process safe size-based rotating file writer.
///
/// - Buffers bytes until `\n` so a rotation never splits one tracing event
///   across `debug.log` and `debug.log.1`. A pathological 8 KiB without a
///   newline still flushes to bound memory.
/// - On every stat-on-tick (16 KiB written or any line >= 16 KiB), checks
///   whether another process rotated the file out from under us via inode
///   comparison, and reopens the current path if so.
/// - When this process needs to rotate (file size >= threshold), takes a
///   `fs2` advisory lock on `{path}.lock` and rotates under the lock. The
///   OS releases the lock on process exit, so a crashed rotater never
///   wedges future rotations. The lockfile is intentionally left on disk
///   between rotations.
pub struct SizeRotatingWriter {
    path: PathBuf,
    file: std::fs::File,
    policy: RotationPolicy,
    fd_inode: u64,
    bytes_since_stat: u64,
    line_buf: Vec<u8>,
}

const STAT_TICK_BYTES: u64 = 16 * 1024;
const MAX_BUFFERED_LINE: usize = 8 * 1024;

impl SizeRotatingWriter {
    pub fn new(path: PathBuf, policy: RotationPolicy) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let (file, fd_inode) = Self::open_and_stat(&path)?;
        let mut writer = Self {
            path,
            file,
            policy,
            fd_inode,
            bytes_since_stat: 0,
            line_buf: Vec::with_capacity(1024),
        };
        // Pre-existing oversized file gets rotated at startup so the first
        // event of a fresh run isn't appended to a stale 50 MiB file.
        let _ = writer.check_rotation();
        Ok(writer)
    }

    fn open_and_stat(path: &Path) -> std::io::Result<(std::fs::File, u64)> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        let meta = file.metadata()?;
        Ok((file, file_inode(&meta)))
    }

    fn emit_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        if line.is_empty() {
            return Ok(());
        }
        self.bytes_since_stat = self.bytes_since_stat.saturating_add(line.len() as u64);
        // Stat every STAT_TICK_BYTES OR when accumulated bytes start
        // approaching the threshold (so a small max_size_bytes doesn't get
        // overshot N× before the stat tick fires).
        let tick = STAT_TICK_BYTES.min(self.policy.max_size_bytes / 4).max(1);
        if self.bytes_since_stat >= tick || line.len() as u64 >= tick {
            let _ = self.check_rotation();
            self.bytes_since_stat = 0;
        }
        self.file.write_all(line)
    }

    fn check_rotation(&mut self) -> std::io::Result<()> {
        if matches!(self.policy.kind, RotationKind::Never) {
            return Ok(());
        }
        match std::fs::metadata(&self.path) {
            Ok(meta) => {
                let path_inode = file_inode(&meta);
                if path_inode != self.fd_inode {
                    // Another process rotated debug.log → debug.log.1 while we
                    // had it open. Reopen the new current; our fd would
                    // otherwise keep writing to the now-archived inode.
                    let (file, ino) = Self::open_and_stat(&self.path)?;
                    self.file = file;
                    self.fd_inode = ino;
                    return Ok(());
                }
                if meta.len() >= self.policy.max_size_bytes {
                    self.try_rotate()?;
                }
            }
            Err(_) => {
                // Path missing (deleted out from under us). Reopen.
                let (file, ino) = Self::open_and_stat(&self.path)?;
                self.file = file;
                self.fd_inode = ino;
            }
        }
        Ok(())
    }

    fn try_rotate(&mut self) -> std::io::Result<()> {
        let lock_path = path_with_suffix(&self.path, ".lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        match lock_file.try_lock_exclusive() {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Another process is rotating; let them. Bounded loss: our fd
                // still references the file that the winner is about to
                // rename, so events written before the next stat tick land in
                // `.1` rather than fresh `debug.log`. The next `check_rotation`
                // detects the inode mismatch and reopens.
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        // Re-stat under lock to avoid double-rotation when two processes race
        // through `check_rotation` simultaneously.
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size >= self.policy.max_size_bytes {
            let _ = self.file.sync_all();
            let keep = self.policy.keep_count.max(1);
            for i in (1..keep).rev() {
                let src = path_with_suffix(&self.path, &format!(".{i}"));
                let dst = path_with_suffix(&self.path, &format!(".{}", i + 1));
                let _ = std::fs::rename(&src, &dst);
            }
            let dst = path_with_suffix(&self.path, ".1");
            let _ = std::fs::rename(&self.path, &dst);
            // Sweep orphan files above the current keep_count: if the user
            // lowered the threshold between runs, the in-place rename chain
            // doesn't touch indices > keep. Walk upward until two consecutive
            // misses to keep the cost bounded.
            let mut misses = 0;
            let mut i = u32::from(keep) + 1;
            while misses < 2 {
                let p = path_with_suffix(&self.path, &format!(".{i}"));
                if p.exists() {
                    let _ = std::fs::remove_file(&p);
                    misses = 0;
                } else {
                    misses += 1;
                }
                i += 1;
            }
            let (file, ino) = Self::open_and_stat(&self.path)?;
            self.file = file;
            self.fd_inode = ino;
        }
        // fs2 releases the lock when `lock_file` drops. We deliberately
        // leave the lockfile on disk; removing it races with another
        // process about to open and lock it.
        Ok(())
    }
}

impl Write for SizeRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut consumed = 0;
        while consumed < buf.len() {
            let rest = &buf[consumed..];
            match rest.iter().position(|&b| b == b'\n') {
                Some(idx) => {
                    self.line_buf.extend_from_slice(&rest[..=idx]);
                    consumed += idx + 1;
                    let line = std::mem::take(&mut self.line_buf);
                    self.emit_line(&line)?;
                }
                None => {
                    self.line_buf.extend_from_slice(rest);
                    consumed = buf.len();
                    if self.line_buf.len() >= MAX_BUFFERED_LINE {
                        let line = std::mem::take(&mut self.line_buf);
                        self.emit_line(&line)?;
                    }
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            self.emit_line(&line)?;
        }
        self.file.flush()
    }
}

impl Drop for SizeRotatingWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(unix)]
fn file_inode(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}
#[cfg(windows)]
fn file_inode(meta: &std::fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    meta.file_index().unwrap_or(0)
}
#[cfg(not(any(unix, windows)))]
fn file_inode(_meta: &std::fs::Metadata) -> u64 {
    0
}

static CONTROLLER: Mutex<Option<Arc<FilterController>>> = Mutex::new(None);

pub fn install_controller(c: Arc<FilterController>) {
    *CONTROLLER.lock().unwrap() = Some(c);
}

pub fn controller() -> Option<Arc<FilterController>> {
    CONTROLLER.lock().unwrap().clone()
}

pub fn current_filter() -> Option<String> {
    controller().map(|c| c.current())
}

pub fn set_filter(directive: &str) -> Result<SwapResult, LogFilterError> {
    controller()
        .ok_or(LogFilterError::Unavailable)?
        .set_filter(directive)
}

pub fn set_level(level: LogLevel) -> Result<SwapResult, LogFilterError> {
    controller()
        .ok_or(LogFilterError::Unavailable)?
        .set_level(level)
}

/// Path of the shared runtime-filter file inside `app_dir`. Daemon writes
/// here on every successful swap; structured view runner subprocesses watch it
/// with `notify` and apply the same filter to their own subscribers.
pub fn runtime_filter_path(app_dir: &std::path::Path) -> std::path::PathBuf {
    app_dir.join("runtime_filter")
}

/// Atomically persist a filter directive to `<app_dir>/runtime_filter`.
/// Write-and-rename so concurrent readers never see a half-written file.
/// Owner-only permissions match the other `serve.*` artifacts.
pub fn persist_runtime_filter(directive: &str, app_dir: &std::path::Path) {
    if let Err(e) = std::fs::create_dir_all(app_dir) {
        tracing::warn!(target: "log.runtime", error = %e, "could not create app dir for runtime_filter");
        return;
    }
    let path = runtime_filter_path(app_dir);
    let tmp = app_dir.join("runtime_filter.tmp");
    if let Err(e) = std::fs::write(&tmp, directive) {
        tracing::warn!(target: "log.runtime", error = %e, "could not write runtime_filter.tmp");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!(target: "log.runtime", error = %e, "could not rename runtime_filter");
    }
}

/// Background task: watch `<app_dir>/runtime_filter` and apply changes
/// to this process's `FilterController`. Used by the structured view runner so
/// the daemon's `aoe log-level` propagates to runners without restart.
///
/// Subscribes via the shared [`crate::file_watch::FileWatchService`] (one
/// `notify::RecommendedWatcher` per process). The function holds the
/// returned `SubscriptionHandle` for its entire lifetime; dropping the
/// future deregisters the subscription and unwatches the directory if no
/// other consumer needs it.
pub async fn watch_runtime_filter(
    svc: std::sync::Arc<crate::file_watch::FileWatchService>,
    app_dir: std::path::PathBuf,
) {
    use crate::file_watch::{FileMatcher, WatchSpec};

    let target = runtime_filter_path(&app_dir);
    let result = svc.subscribe_channel(
        WatchSpec {
            dir: app_dir.clone(),
            matcher: FileMatcher::Exact(target.clone()),
            // `apply_filter_file` is idempotent; no debounce needed.
            debounce: None,
        },
        // Capacity 4: low-rate source.
        4,
    );
    let (mut rx, _handle) = match result {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(
                target: "log.runtime",
                error = %e,
                dir = %app_dir.display(),
                "notify watch failed; live propagation disabled"
            );
            return;
        }
    };

    // Apply once at startup if the file is already there. This matches the
    // pre-migration ordering: priming runs only AFTER subscribe succeeds.
    apply_filter_file(&target);

    // Drain the channel for the lifetime of the task. `_handle` keeps the
    // subscription alive; dropping it on function exit unsubscribes.
    while rx.recv().await.is_some() {
        apply_filter_file(&target);
    }
}

fn apply_filter_file(path: &std::path::Path) {
    let directive = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return,
    };
    let directive = directive.trim();
    if directive.is_empty() {
        return;
    }
    match set_filter(directive) {
        // No-op swaps stay silent: logging here would write into the watched
        // dir and re-trigger the watcher (#1894).
        Ok(swap) if swap.changed => tracing::info!(
            target: "log.runtime",
            previous = %swap.previous,
            current = %swap.current,
            source = "file-watch",
            "runner filter swapped"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            target: "log.runtime",
            error = %e,
            directive = %directive,
            "runner filter swap failed"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-touching tests must serialize.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn log_level_parse_accepts_known() {
        assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("INFO"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("trace "), Some(LogLevel::Trace));
        assert_eq!(LogLevel::parse("bogus"), None);
    }

    /// Build a `FilterController` backed by a reload handle, installed as
    /// the thread-local default subscriber so the reload handle's `modify`
    /// can upgrade its weak reference. The returned guard must outlive the
    /// controller; the default is thread-scoped, so it does not collide
    /// with the process-global subscriber other tests install.
    fn test_controller(initial: &str) -> (FilterController, tracing::subscriber::DefaultGuard) {
        let filter = EnvFilter::builder()
            .with_regex(false)
            .parse(initial)
            .expect("valid initial filter");
        let (layer, handle) = reload::Layer::new(filter);
        let guard = tracing::subscriber::set_default(Registry::default().with(layer));
        let controller = FilterController {
            inner: handle,
            current: Mutex::new(initial.to_string()),
        };
        (controller, guard)
    }

    #[test]
    fn swap_reports_changed_then_noop() {
        let (c, _guard) = test_controller("agent_of_empires=info");
        let first = c.set_filter("agent_of_empires=debug").expect("swap ok");
        assert!(
            first.changed,
            "first swap to a new directive must report changed"
        );
        assert_eq!(first.previous, "agent_of_empires=info");
        assert_eq!(first.current, "agent_of_empires=debug");

        // Re-applying the identical directive (the #1894 file-watch case)
        // must be a silent no-op so callers do not log or persist.
        let second = c.set_filter("agent_of_empires=debug").expect("swap ok");
        assert!(!second.changed, "identical re-apply must report no-op");
        assert_eq!(second.previous, second.current);
        assert_eq!(c.current(), "agent_of_empires=debug");
    }

    #[test]
    fn filter_for_level_expands_all_roots() {
        let s = LogConfig::filter_for_level(LogLevel::Debug);
        for root in DEFAULT_TARGET_ROOTS {
            assert!(
                s.contains(&format!("{root}=debug")),
                "missing {root} in {s}"
            );
        }
    }

    #[test]
    fn filter_string_overlay_acp() {
        let cfg = LogConfig {
            level: Some(LogLevel::Info),
            acp_trace: true,
            terminal_trace: false,
        };
        let s = cfg.filter_string().unwrap();
        assert!(s.contains("agent_client_protocol=debug"));
        assert!(s.contains("transport_actor=trace"));
    }

    #[test]
    fn filter_string_overlay_terminal() {
        let cfg = LogConfig {
            level: Some(LogLevel::Info),
            acp_trace: false,
            terminal_trace: true,
        };
        let s = cfg.filter_string().unwrap();
        assert!(s.ends_with(",terminal=trace"));
    }

    #[test]
    fn filter_string_none_when_level_unset() {
        let cfg = LogConfig {
            level: None,
            acp_trace: false,
            terminal_trace: false,
        };
        assert!(cfg.filter_string().is_none());
    }

    #[test]
    fn from_env_no_vars() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AOE_LOG_LEVEL");
        std::env::remove_var("AGENT_OF_EMPIRES_DEBUG");
        std::env::remove_var("AOE_ACP_TRACE");
        std::env::remove_var("AOE_TERMINAL_TRACE");
        let cfg = LogConfig::from_env();
        assert_eq!(cfg.level, None);
        assert!(!cfg.acp_trace);
        assert!(!cfg.terminal_trace);
    }

    #[test]
    fn from_env_aoe_log_level() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("AOE_LOG_LEVEL", "trace");
        std::env::remove_var("AGENT_OF_EMPIRES_DEBUG");
        let cfg = LogConfig::from_env();
        std::env::remove_var("AOE_LOG_LEVEL");
        assert_eq!(cfg.level, Some(LogLevel::Trace));
    }

    #[test]
    fn from_env_legacy_debug_flag() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AOE_LOG_LEVEL");
        std::env::set_var("AGENT_OF_EMPIRES_DEBUG", "1");
        let cfg = LogConfig::from_env();
        std::env::remove_var("AGENT_OF_EMPIRES_DEBUG");
        assert_eq!(cfg.level, Some(LogLevel::Debug));
    }

    fn with_test_controller<F>(initial: &str, f: F)
    where
        F: FnOnce(&FilterController),
    {
        let filter = EnvFilter::builder()
            .with_regex(false)
            .parse(initial)
            .unwrap();
        let (layer, handle) = reload::Layer::<EnvFilter, Registry>::new(filter);
        let subscriber = Registry::default().with(layer);
        let c = FilterController {
            inner: handle,
            current: Mutex::new(initial.to_string()),
        };
        tracing::subscriber::with_default(subscriber, || f(&c));
    }

    #[test]
    fn controller_swap_returns_previous() {
        with_test_controller("info", |c| {
            let r = c.set_level(LogLevel::Debug).unwrap();
            assert_eq!(r.previous, "info");
            assert!(r.current.contains("agent_of_empires=debug"));
            assert_eq!(c.current(), r.current);
        });
    }

    #[test]
    fn controller_rejects_bare_global_level() {
        with_test_controller("info", |c| {
            let err = c.set_filter("debug").unwrap_err();
            assert!(matches!(err, LogFilterError::BareGlobalLevel));
        });
    }

    #[test]
    fn controller_accepts_targeted_filter() {
        with_test_controller("info", |c| {
            c.set_filter("acp.protocol=trace,info").unwrap();
            assert_eq!(c.current(), "acp.protocol=trace,info");
        });
    }

    #[test]
    fn controller_rejects_empty_filter() {
        with_test_controller("info", |c| {
            assert!(matches!(
                c.set_filter("   ").unwrap_err(),
                LogFilterError::Invalid(_)
            ));
        });
    }

    #[test]
    fn controller_rejects_invalid_level() {
        with_test_controller("info", |c| {
            // Unknown level name; EnvFilter rejects.
            assert!(matches!(
                c.set_filter("acp=notalevel").unwrap_err(),
                LogFilterError::Invalid(_)
            ));
        });
    }

    fn make_cfg(rotation: RotationKind, max_mib: u64, keep: u8) -> LoggingConfig {
        LoggingConfig {
            default_level: "info".into(),
            targets: Default::default(),
            output: crate::session::config::SinkKind::File,
            file_path: "debug.log".into(),
            rotation,
            max_size_mib: max_mib,
            keep_count: keep,
            show_spans: false,
        }
    }

    #[test]
    fn resolve_log_path_relative_joins_app_dir() {
        let cfg = make_cfg(RotationKind::Size, 50, 5);
        let dir = std::path::PathBuf::from("/tmp/aoe-test");
        assert_eq!(resolve_log_path(&cfg, &dir), dir.join("debug.log"));
    }

    #[test]
    fn resolve_log_path_absolute_used_verbatim() {
        let mut cfg = make_cfg(RotationKind::Size, 50, 5);
        cfg.file_path = "/var/log/aoe.log".into();
        let dir = std::path::PathBuf::from("/tmp/aoe-test");
        assert_eq!(
            resolve_log_path(&cfg, &dir),
            std::path::PathBuf::from("/var/log/aoe.log")
        );
    }

    #[test]
    fn resolve_sink_tui_with_stdout_coerces_to_file_with_warning() {
        let mut cfg = make_cfg(RotationKind::Size, 50, 5);
        cfg.output = crate::session::config::SinkKind::Stdout;
        let dir = std::path::PathBuf::from("/tmp/aoe-test");
        let r = resolve_sink(&cfg, &dir, ProcessContext::Tui);
        assert!(matches!(r.target, SubscriberTarget::File(_, _)));
        assert!(r.warning.is_some(), "coercion should surface a warning");
    }

    #[test]
    fn resolve_sink_serve_foreground_honors_stdout() {
        let mut cfg = make_cfg(RotationKind::Size, 50, 5);
        cfg.output = crate::session::config::SinkKind::Stdout;
        let dir = std::path::PathBuf::from("/tmp/aoe-test");
        let r = resolve_sink(&cfg, &dir, ProcessContext::ServeForeground);
        assert!(matches!(r.target, SubscriberTarget::Stdout));
        assert!(r.warning.is_none());
    }

    #[test]
    fn rotation_writer_rotates_at_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");
        // 4 KiB threshold so the test runs fast.
        let policy = RotationPolicy {
            kind: RotationKind::Size,
            max_size_bytes: 4 * 1024,
            keep_count: 3,
        };
        let mut w = SizeRotatingWriter::new(path.clone(), policy).unwrap();
        // Write 5 KiB worth of one-line events; each line forces stat-on-tick
        // when crossing 16 KiB but actual size check uses metadata so the
        // 4 KiB threshold triggers on the first oversized stat.
        for i in 0..200 {
            writeln!(&mut w, "line {i:050}").unwrap();
        }
        w.flush().unwrap();
        drop(w);
        let rotated = path.with_extension("log.1");
        assert!(rotated.exists(), "expected rotated file at {:?}", rotated);
    }

    #[test]
    fn rotation_writer_keeps_at_most_keep_count() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");
        // Seed pre-existing rotated files to simulate prior rotations.
        std::fs::write(path.with_extension("log.1"), b"old 1").unwrap();
        std::fs::write(path.with_extension("log.2"), b"old 2").unwrap();
        std::fs::write(path.with_extension("log.3"), b"old 3").unwrap();
        // Threshold low enough to rotate on first emit.
        let policy = RotationPolicy {
            kind: RotationKind::Size,
            max_size_bytes: 64,
            keep_count: 3,
        };
        std::fs::write(&path, vec![b'x'; 200]).unwrap();
        let mut w = SizeRotatingWriter::new(path.clone(), policy).unwrap();
        writeln!(&mut w, "trigger rotation now padded out to be large enough").unwrap();
        w.flush().unwrap();
        drop(w);
        // .1, .2, .3 should exist; .4 should NOT.
        assert!(
            !path.with_extension("log.4").exists(),
            "keep_count=3 must drop .4"
        );
    }

    #[test]
    fn rotation_writer_never_keeps_file_alone_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");
        let policy = RotationPolicy {
            kind: RotationKind::Never,
            max_size_bytes: 64,
            keep_count: 3,
        };
        let mut w = SizeRotatingWriter::new(path.clone(), policy).unwrap();
        for _ in 0..100 {
            writeln!(&mut w, "filler line that pushes well past 64 bytes here").unwrap();
        }
        w.flush().unwrap();
        drop(w);
        assert!(
            !path.with_extension("log.1").exists(),
            "rotation=never must not produce .1"
        );
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 64, "file should have grown past threshold");
    }

    #[test]
    fn rotation_writer_line_buffers_across_partial_writes() {
        // Simulate the multi-write pattern from `tracing-subscriber::fmt`:
        // two write() calls for the same logical event. Ensure both halves
        // land in the same line (no rotation can split mid-line).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");
        let policy = RotationPolicy {
            kind: RotationKind::Size,
            max_size_bytes: 1024 * 1024,
            keep_count: 3,
        };
        let mut w = SizeRotatingWriter::new(path.clone(), policy).unwrap();
        w.write_all(b"first half ").unwrap();
        w.write_all(b"second half\n").unwrap();
        w.flush().unwrap();
        drop(w);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("first half second half\n"),
            "split write should re-coalesce in one line, got: {:?}",
            contents
        );
    }

    #[test]
    fn rotation_writer_startup_rotates_oversize_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");
        // Pre-seed oversized file.
        std::fs::write(&path, vec![b'x'; 200]).unwrap();
        let policy = RotationPolicy {
            kind: RotationKind::Size,
            max_size_bytes: 64,
            keep_count: 3,
        };
        let _w = SizeRotatingWriter::new(path.clone(), policy).unwrap();
        // .1 should now exist (startup rotation triggered in new()).
        assert!(path.with_extension("log.1").exists());
    }

    #[test]
    fn rotation_writer_sweeps_orphans_when_keep_count_reduced() {
        // User lowered keep_count from 5 to 2 between runs. The rename chain
        // only touches indices 1..=keep, so .3, .4, .5 would orphan forever
        // without the sweep.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");
        std::fs::write(path.with_extension("log.1"), b"old 1").unwrap();
        std::fs::write(path.with_extension("log.2"), b"old 2").unwrap();
        std::fs::write(path.with_extension("log.3"), b"old 3").unwrap();
        std::fs::write(path.with_extension("log.4"), b"old 4").unwrap();
        std::fs::write(path.with_extension("log.5"), b"old 5").unwrap();
        let policy = RotationPolicy {
            kind: RotationKind::Size,
            max_size_bytes: 64,
            keep_count: 2,
        };
        std::fs::write(&path, vec![b'x'; 200]).unwrap();
        let _w = SizeRotatingWriter::new(path.clone(), policy).unwrap();
        // After startup rotation: .1 (was current), .2 (was .1) survive.
        assert!(path.with_extension("log.1").exists(), ".1 must exist");
        assert!(path.with_extension("log.2").exists(), ".2 must exist");
        assert!(
            !path.with_extension("log.3").exists(),
            ".3 must be swept by orphan sweep"
        );
        assert!(
            !path.with_extension("log.4").exists(),
            ".4 must be swept by orphan sweep"
        );
        assert!(
            !path.with_extension("log.5").exists(),
            ".5 must be swept by orphan sweep"
        );
    }

    #[test]
    fn rotation_policy_clamps_keep_count_zero_to_one() {
        // Defense-in-depth: a hand-edited config with keep_count = 0 would
        // otherwise yield a writer that deletes the only copy on rotation.
        let mut cfg = make_cfg(RotationKind::Size, 50, 0);
        cfg.keep_count = 0;
        let policy = RotationPolicy::from(&cfg);
        assert_eq!(policy.keep_count, 1, "keep_count=0 must clamp to 1");
    }
}
