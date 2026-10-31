//! Logging configuration, subscriber init, and the process-wide runtime filter controller.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use fs2::FileExt;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Registry;

use crate::session::config::{LoggingConfig, RotationKind};

/// Contexts whose stdout is the UI or discarded force the file sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessContext {
    Tui,
    ServeDaemonChild,
    ServeForeground,
    Runner,
    OneShotCli,
}

pub struct SinkResolution {
    pub target: SubscriberTarget,
    pub warning: Option<String>,
}

pub fn resolve_log_path(cfg: &LoggingConfig, app_dir: &Path) -> PathBuf {
    let p = Path::new(&cfg.file_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        app_dir.join(p)
    }
}

pub fn resolve_sink(cfg: &LoggingConfig, app_dir: &Path, ctx: ProcessContext) -> SinkResolution {
    use crate::session::config::SinkKind;

    let force_file = matches!(
        ctx,
        ProcessContext::Tui
            | ProcessContext::ServeDaemonChild
            | ProcessContext::Runner
            | ProcessContext::OneShotCli
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

#[derive(Debug, Clone, Copy)]
pub struct RotationPolicy {
    pub kind: RotationKind,
    pub max_size_bytes: u64,
    pub keep_count: u8,
}

impl From<&LoggingConfig> for RotationPolicy {
    fn from(cfg: &LoggingConfig) -> Self {
        // keep_count = 0 would rotate away the only copy.
        Self {
            kind: cfg.rotation,
            max_size_bytes: cfg.max_size_mib.saturating_mul(1024 * 1024),
            keep_count: cfg.keep_count.max(1),
        }
    }
}

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
    "plugin",
    "web",
    // `log` carries filter-swap audit events (`log.runtime`).
    "log",
    "cli",
    "tui",
    "session",
    "tmux",
    "http",
    "serve",
    "server",
    "runtime",
    "hooks",
    "sound",
    "telemetry",
    "smart_rename",
    "events",
    "conversation_summary",
    "shutdown",
    "file_watch",
    "push",
    "task",
    "daemon",
];

pub const KNOWN_SUB_TARGETS: &[&str] = &[
    "acp.protocol",
    "acp.protocol.stderr",
    "acp.protocol.tool_dispatch",
    "acp.supervisor",
    "acp.event_store",
    "acp.runner",
    "plugin.host",
    "terminal.ws",
    "auth.token",
    "auth.middleware",
    "auth.rate_limit",
    "auth.passphrase",
    "auth.passkey",
    "process.signal",
    "process.tree",
    "process.reap",
    "process.ppid",
    "process.systemd",
    "update.fetch",
    "update.cache",
    "update.parse",
    "containers.image",
    "containers.runtime",
    "git.command",
    "git.worktree",
    "web.client",
    "log.runtime",
    "migrations.v036",
    "migrations.v037",
    "migrations.v038",
    "migrations.v039",
    "migrations.v040",
    "migrations.v041",
    "migrations.v043",
    "migrations.v044",
    "migrations.v045",
    "migrations.v046",
    "migrations.v047",
    "migrations.v048",
    "migrations.v052",
    "migrations.v053",
    "server.file_watch",
    "server.lifecycle",
    "server.idle_reap",
    "server.runtime",
    "server.session_service",
    "server.session_store",
    "server.sleep_inhibit",
    "server.maintenance",
    "server.status",
    "server.sessions",
    "runtime.ws",
    "file_watch.service",
    "file_watch.subscriber",
    "push.wake_fired",
    "task.panic",
    "session.status_change",
    "session.create",
    "session.delete",
    "session.trash",
    "session.restart",
    "session.start",
    "session.restore",
    "session.rename",
    "session.sync",
    "serve.lifecycle",
    "serve.shutdown",
    "daemon.client",
    "acp.queue",
    "plugin.install",
    "http.api.telemetry",
    "http.api.system",
];

/// Only the filter hot-swaps; sink and rotation settings apply on restart.
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
            // Persisting an unchanged directive would re-fire every runner's watcher.
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

/// Overrides follow the roots because EnvFilter is last-wins per target.
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

pub fn load_persisted_filter() -> Option<String> {
    let config = crate::session::load_config().ok().flatten()?;
    build_filter_from_config(&config.logging.default_level, &config.logging.targets)
}

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

    pub fn serve_default() -> Self {
        Self {
            level: Some(LogLevel::Info),
            acp_trace: false,
            terminal_trace: false,
        }
    }

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

    /// The `warn` catch-all keeps dependency crates to warn/error at any level.
    pub fn filter_for_level(level: LogLevel) -> String {
        let lvl = level.as_str();
        let mut s = String::from("warn");
        for t in DEFAULT_TARGET_ROOTS {
            s.push_str(&format!(",{t}={lvl}"));
        }
        s
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
        // Bare global levels would enable debug for hyper/rustls/tower.
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
        let mut current = self.current.lock().unwrap();
        let previous = current.clone();
        // A logged no-op swap lands in the watched dir and re-triggers the watcher.
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

pub type TeeLayer = crate::acp::session_tee::SessionTeeLayer;

pub fn init_subscriber(target: SubscriberTarget, filter: String) -> InitResult {
    init_subscriber_with_options(target, filter, false, None, true)
}

/// Default Full output without the span chain prefix. The enclosing
/// `acp_session` label is added unless the event declares its own `session`.
pub(crate) struct NoSpanFormat;

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
        if meta.fields().field("session").is_none() {
            if let Some(session) = acp_session_label(ctx) {
                write!(writer, "session={session} ")?;
            }
        }
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// The `session` field of the enclosing `acp_session` span, which the span
/// chain would otherwise carry.
fn acp_session_label<S, N>(ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>) -> Option<String>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    let span = ctx
        .event_scope()?
        .find(|span| span.name() == "acp_session")?;
    let extensions = span.extensions();
    let fields = extensions
        .get::<tracing_subscriber::fmt::FormattedFields<N>>()?
        .fields
        .as_str();
    let start = if fields.starts_with("session=") {
        0
    } else {
        fields.find(" session=")? + 1
    };
    let value = fields[start + "session=".len()..].split(' ').next()?;
    (!value.is_empty()).then(|| value.to_string())
}

/// `start_marker` writes the filter-immune `[AOE_START_MARKER]` line that
/// delimits daemon and TUI runs; one-shot CLI commands skip it so the last
/// marker in the shared file stays the daemon's.
pub fn init_subscriber_with_options(
    target: SubscriberTarget,
    filter: String,
    show_spans: bool,
    session_tee: Option<TeeLayer>,
    start_marker: bool,
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
    let sink = match &target {
        SubscriberTarget::File(path, _) => ActiveSink::File(path.clone()),
        SubscriberTarget::Stdout => ActiveSink::Stdout,
    };

    // Only the JSON formatter can drop spans, so a custom FormatEvent does it here.
    let install_result = match target {
        SubscriberTarget::File(path, policy) => match SizeRotatingWriter::new(path.clone(), policy)
        {
            Ok(mut writer) => {
                if start_marker {
                    write_raw_startup_marker(&mut writer);
                }
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
            if start_marker {
                write_raw_startup_marker(&mut std::io::stdout());
            }
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
            let _ = ACTIVE_SINK.set(sink);
            let controller = Arc::new(FilterController {
                inner: handle,
                current: Mutex::new(filter),
            });
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

/// Size-based rotating writer safe across processes. Buffers to `\n` so rotation never
/// splits an event, detects another process's rotation by inode, and rotates under an
/// `fs2` lock on `{path}.lock`, which stays on disk.
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
        // Also stat near a small threshold so it is not overshot before the tick.
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
                // Another process is rotating; the next check sees the inode change and reopens.
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        // Re-stat under the lock so two racing processes do not both rotate.
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
            // Sweep indices above keep_count left behind when the user lowered it.
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
        // Removing the lockfile would race another process about to lock it.
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

/// The sink a successful `init_subscriber_with_options` installed in this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActiveSink {
    File(PathBuf),
    Stdout,
}

static ACTIVE_SINK: OnceLock<ActiveSink> = OnceLock::new();

/// `None` when no subscriber was installed, including after a failed initialization.
pub fn active_sink() -> Option<ActiveSink> {
    ACTIVE_SINK.get().cloned()
}

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

pub fn runtime_filter_path(app_dir: &std::path::Path) -> std::path::PathBuf {
    app_dir.join("runtime_filter")
}

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
            debounce: None,
        },
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

    apply_filter_file(&target);

    while rx.recv().await.is_some() {
        apply_filter_file(&target);
    }
}

/// Crate targets a filter may name besides aoe's own roots.
const EXTERNAL_TARGET_ROOTS: &[&str] = &["agent_client_protocol"];

/// Directive roots outside aoe's targets, such as one renamed since the filter
/// was saved, whose directives then match no aoe log (#4991).
fn unknown_target_roots(directive: &str) -> Vec<&str> {
    directive
        .split(',')
        .filter_map(|d| {
            let d = d.trim();
            // A bare target means `target=trace`; a bare level names no target.
            if d.is_empty() || LogLevel::parse(d).is_some() {
                return None;
            }
            let root = d.split(['.', ':', '[', '=']).next()?.trim();
            (!root.is_empty()
                && !DEFAULT_TARGET_ROOTS.contains(&root)
                && !EXTERNAL_TARGET_ROOTS.contains(&root))
            .then_some(root)
        })
        .collect()
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
    let unknown = unknown_target_roots(directive);
    if !unknown.is_empty() {
        tracing::warn!(
            target: "log.runtime",
            roots = ?unknown,
            "runtime filter names log targets outside aoe; a renamed aoe target there matches nothing"
        );
    }
    match set_filter(directive) {
        // Logging here would write into the watched dir and re-trigger the watcher.
        Ok(swap) if swap.changed => tracing::info!(
            target: "log.runtime",
            previous = %swap.previous,
            current = %swap.current,
            source = "file-watch",
            "filter swapped"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            target: "log.runtime",
            error = %e,
            directive = %directive,
            "filter swap failed"
        ),
    }
    #[cfg(debug_assertions)]
    if path.with_extension("observe").exists() {
        std::fs::write(path.with_extension("applied"), directive)
            .expect("publish e2e filter application");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use crate::session::test_support::EnvGuard;

    #[test]
    fn unknown_target_roots_cases() {
        let cases = [
            (
                "agent_of_empires=trace,cockpit=trace,cockpit.event_store=debug",
                vec!["cockpit", "cockpit"],
            ),
            (
                "acp=trace,acp[turn]=debug,agent_client_protocol::jsonrpc::transport_actor=trace",
                vec![],
            ),
            ("agent_of_empires::tui=debug,hyper=debug", vec!["hyper"]),
            ("info,cockpit", vec!["cockpit"]),
        ];
        for (directive, want) in cases {
            assert_eq!(unknown_target_roots(directive), want, "{directive}");
        }
    }

    #[test]
    fn log_level_parse_accepts_known() {
        assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("INFO"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("trace "), Some(LogLevel::Trace));
        assert_eq!(LogLevel::parse("bogus"), None);
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
    fn swap_reports_previous_then_noop() {
        with_test_controller("agent_of_empires=info", |c| {
            let first = c.set_level(LogLevel::Debug).expect("swap ok");
            assert!(first.changed, "a new directive must report changed");
            assert_eq!(first.previous, "agent_of_empires=info");
            assert!(first.current.contains("agent_of_empires=debug"));
            assert_eq!(c.current(), first.current);

            let second = c.set_filter(&first.current).expect("swap ok");
            assert!(!second.changed, "identical re-apply must report no-op");
            assert_eq!(second.previous, second.current);
        });
    }

    #[test]
    fn set_filter_validates_directives() {
        with_test_controller("info", |c| {
            assert!(matches!(
                c.set_filter("debug").unwrap_err(),
                LogFilterError::BareGlobalLevel
            ));
            for bad in ["   ", "acp=notalevel"] {
                assert!(
                    matches!(c.set_filter(bad).unwrap_err(), LogFilterError::Invalid(_)),
                    "{bad:?}"
                );
            }
            assert_eq!(c.current(), "info", "rejected filters must not apply");
            c.set_filter("acp.protocol=trace,info").unwrap();
            assert_eq!(c.current(), "acp.protocol=trace,info");
        });
    }

    #[test]
    fn filter_string_applies_trace_overlays() {
        let cfg = |level, acp_trace, terminal_trace| LogConfig {
            level,
            acp_trace,
            terminal_trace,
        };
        assert!(cfg(None, true, true).filter_string().is_none());
        let acp = cfg(Some(LogLevel::Info), true, false)
            .filter_string()
            .unwrap();
        assert!(acp.contains("agent_client_protocol=debug"));
        assert!(acp.contains("transport_actor=trace"));
        let terminal = cfg(Some(LogLevel::Info), false, true)
            .filter_string()
            .unwrap();
        assert!(terminal.ends_with(",terminal=trace"));
    }

    #[test]
    fn default_filter_keeps_dependencies_at_warn_and_lets_overrides_win() {
        let directive = format!(
            "{},http.request=info",
            LogConfig::filter_for_level(LogLevel::Trace)
        );
        let filter = EnvFilter::builder()
            .with_regex(false)
            .parse(&directive)
            .unwrap();
        // Consts keep these fixtures out of the source scan below.
        const DEPENDENCY: &str = "hyper_util";
        const OWN: &str = "session.create";
        const OVERRIDDEN: &str = "http.request";
        tracing::subscriber::with_default(Registry::default().with(filter), || {
            assert!(tracing::enabled!(target: DEPENDENCY, tracing::Level::WARN));
            assert!(!tracing::enabled!(target: DEPENDENCY, tracing::Level::INFO));
            assert!(tracing::enabled!(target: OWN, tracing::Level::TRACE));
            assert!(!tracing::enabled!(target: OVERRIDDEN, tracing::Level::DEBUG));
        });
    }

    /// Every `target:` used in `src/` as `(file, target)`, excluding test-only targets.
    fn source_targets() -> Vec<(PathBuf, String)> {
        fn walk(dir: &Path, pattern: &regex::Regex, out: &mut Vec<(PathBuf, String)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, pattern, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let src = std::fs::read_to_string(&path).unwrap();
                    for captures in pattern.captures_iter(&src) {
                        let target = captures.get(1).or(captures.get(2)).unwrap().as_str();
                        if !target.starts_with("aoe_test_") {
                            out.push((path.clone(), target.to_string()));
                        }
                    }
                }
            }
        }
        // The trailing comma excludes struct fields such as `target: "codex".to_string()`.
        let pattern = regex::Regex::new(
            r#"target: "([A-Za-z_][A-Za-z0-9_.]*)"\s*,|target = "([A-Za-z_][A-Za-z0-9_.]*)""#,
        )
        .unwrap();
        let mut out = Vec::new();
        walk(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &pattern,
            &mut out,
        );
        out
    }

    #[test]
    fn every_source_target_root_is_filtered_and_known_sub_targets_exist() {
        let targets = source_targets();
        assert!(!targets.is_empty(), "the source scan found no targets");
        let unlisted: Vec<String> = targets
            .iter()
            .filter(|(_, target)| {
                let root = target.split('.').next().unwrap();
                !DEFAULT_TARGET_ROOTS.contains(&root) && !EXTERNAL_TARGET_ROOTS.contains(&root)
            })
            .map(|(file, target)| format!("{}: {target}", file.display()))
            .collect();
        assert!(
            unlisted.is_empty(),
            "targets whose root is missing from DEFAULT_TARGET_ROOTS (their events are dropped at every level):\n{}",
            unlisted.join("\n")
        );
        let used: BTreeSet<&str> = targets.iter().map(|(_, t)| t.as_str()).collect();
        let unused: Vec<&str> = KNOWN_SUB_TARGETS
            .iter()
            .copied()
            .filter(|t| !used.contains(t))
            .collect();
        assert!(
            unused.is_empty(),
            "KNOWN_SUB_TARGETS entries no longer used in src/: {unused:?}"
        );
    }

    #[test]
    fn known_sub_targets_stay_synced_with_ts_mirror() {
        let mirror_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("web/src/components/settings/customWidgets.tsx");
        let Ok(mirror) = std::fs::read_to_string(&mirror_path) else {
            // Nix-filtered sources omit web/.
            eprintln!(
                "skipping TS logging target mirror check: {} is absent",
                mirror_path.display()
            );
            return;
        };
        let start = mirror
            .find("const KNOWN_TARGETS")
            .expect("customWidgets.tsx declares `const KNOWN_TARGETS`");
        let body = &mirror[start..];
        let body = &body[..body
            .find("];")
            .expect("KNOWN_TARGETS array closes with `];`")];
        let mirrored: BTreeSet<&str> = body
            .split("value: \"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        let known: BTreeSet<&str> = KNOWN_SUB_TARGETS.iter().copied().collect();
        let missing_in_ts: Vec<_> = known.difference(&mirrored).collect();
        let missing_in_rust: Vec<_> = mirrored.difference(&known).collect();
        assert!(
            missing_in_ts.is_empty() && missing_in_rust.is_empty(),
            "KNOWN_TARGETS in customWidgets.tsx is out of sync with KNOWN_SUB_TARGETS: \
             missing in TS {missing_in_ts:?}, missing in Rust {missing_in_rust:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn from_env_reads_level_and_legacy_debug_flag() {
        let vars = [
            "AOE_LOG_LEVEL",
            "AGENT_OF_EMPIRES_DEBUG",
            "AOE_ACP_TRACE",
            "AOE_TERMINAL_TRACE",
        ];
        let cases: [(&[(&str, &str)], Option<LogLevel>); 3] = [
            (&[], None),
            (&[("AOE_LOG_LEVEL", "trace")], Some(LogLevel::Trace)),
            (&[("AGENT_OF_EMPIRES_DEBUG", "1")], Some(LogLevel::Debug)),
        ];
        for (set, expected) in cases {
            let mut env = EnvGuard::unset(&vars);
            for (key, value) in set {
                env = env.and_set(key, value);
            }
            let cfg = LogConfig::from_env();
            assert_eq!(cfg.level, expected, "{set:?}");
            assert!(!cfg.acp_trace && !cfg.terminal_trace);
        }
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
    fn rotation_writer_rotates_at_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");
        let policy = RotationPolicy {
            kind: RotationKind::Size,
            max_size_bytes: 4 * 1024,
            keep_count: 3,
        };
        let mut w = SizeRotatingWriter::new(path.clone(), policy).unwrap();
        w.write_all(b"first half ").unwrap();
        w.write_all(b"second half\n").unwrap();
        w.flush().unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("first half second half\n"),
            "split write should re-coalesce in one line, got: {contents:?}"
        );
        for i in 0..200 {
            writeln!(&mut w, "line {i:050}").unwrap();
        }
        w.flush().unwrap();
        drop(w);
        assert!(path.with_extension("log.1").exists());
    }

    #[test]
    fn rotation_writer_startup_rotates_and_sweeps_past_keep_count() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");
        for n in 1..=5 {
            std::fs::write(path.with_extension(format!("log.{n}")), b"old").unwrap();
        }
        std::fs::write(&path, vec![b'x'; 200]).unwrap();
        let policy = RotationPolicy {
            kind: RotationKind::Size,
            max_size_bytes: 64,
            keep_count: 2,
        };
        let _w = SizeRotatingWriter::new(path.clone(), policy).unwrap();
        for n in 1..=5 {
            assert_eq!(
                path.with_extension(format!("log.{n}")).exists(),
                n <= 2,
                ".{n} with keep_count=2"
            );
        }
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
    fn resolve_log_path_joins_only_relative_paths() {
        let dir = std::path::PathBuf::from("/tmp/aoe-test");
        for (file_path, expected) in [
            ("debug.log", dir.join("debug.log")),
            (
                "/var/log/aoe.log",
                std::path::PathBuf::from("/var/log/aoe.log"),
            ),
        ] {
            let mut cfg = make_cfg(RotationKind::Size, 50, 5);
            cfg.file_path = file_path.into();
            assert_eq!(resolve_log_path(&cfg, &dir), expected);
        }
    }

    /// Only a foreground serve may log to stdout; the TUI owns the terminal,
    /// so it is coerced to the file with a warning.
    #[test]
    fn failed_init_leaves_the_active_sink_unset() {
        let init = init_subscriber_with_options(
            SubscriberTarget::Stdout,
            "aoe=notalevel".into(),
            false,
            None,
            true,
        );
        assert!(init.controller.is_none());
        assert_eq!(active_sink(), None);
    }

    #[test]
    fn resolve_sink_honors_stdout_only_outside_the_tui() {
        let mut cfg = make_cfg(RotationKind::Size, 50, 5);
        cfg.output = crate::session::config::SinkKind::Stdout;
        let dir = std::path::PathBuf::from("/tmp/aoe-test");
        for (context, to_stdout) in [
            (ProcessContext::Tui, false),
            (ProcessContext::ServeForeground, true),
        ] {
            let r = resolve_sink(&cfg, &dir, context);
            assert_eq!(matches!(r.target, SubscriberTarget::Stdout), to_stdout);
            assert_eq!(
                r.warning.is_some(),
                !to_stdout,
                "coercion surfaces a warning"
            );
        }
    }

    struct TestBufWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for TestBufWriter {
        fn write(&mut self, src: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(src);
            Ok(src.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn no_span_format_prefixes_the_acp_session_label() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer_buf = buf.clone();
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(move || TestBufWriter(writer_buf.clone()))
            .with_ansi(false)
            .event_format(NoSpanFormat);
        tracing::subscriber::with_default(Registry::default().with(layer), || {
            tracing::info!(target: "acp.protocol", "outside");
            let span = tracing::info_span!("acp_session", session = %"abc123", turn = 7);
            span.in_scope(|| {
                tracing::info!(target: "acp.protocol", "inside");
                tracing::info_span!("inner")
                    .in_scope(|| tracing::warn!(target: "acp.protocol", "nested"));
                tracing::info!(target: "acp.protocol", session = "x", "own");
            });
        });
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert!(lines[0].ends_with(" INFO acp.protocol: outside"), "{out}");
        assert!(
            lines[1].ends_with(" INFO acp.protocol: session=abc123 inside"),
            "{out}"
        );
        assert!(
            lines[2].ends_with(" WARN acp.protocol: session=abc123 nested"),
            "{out}"
        );
        assert!(
            lines[3].ends_with(" INFO acp.protocol: own session=\"x\""),
            "{out}"
        );
        assert_eq!(lines[3].matches("session=").count(), 1, "{out}");
    }

    #[test]
    fn rotation_policy_clamps_keep_count_zero_to_one() {
        let mut cfg = make_cfg(RotationKind::Size, 50, 0);
        cfg.keep_count = 0;
        let policy = RotationPolicy::from(&cfg);
        assert_eq!(policy.keep_count, 1, "keep_count=0 must clamp to 1");
    }
}
