//! Unix daemon notifications and Linux transient session scopes.
//! New scopes are best effort and may fall back to unscoped launch.
//! Retirement requires exact captured units and proven empty cgroups.
//! Unknown ownership or manager errors retain owned resources.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::session::config::SystemdSettings;

#[cfg(unix)]
pub(crate) mod notify;

pub(crate) const DEFAULT_SLICE_TEMPLATE: &str = "background-aoe-{profile}.slice";
const UNIT_PREFIX: &str = "aoe-session-";
/// `systemd-run --scope` sets this for the command; wrappers restore the
/// caller's value so the agent environment is unchanged.
pub(crate) const INJECTED_VAR: &str = "INVOCATION_ID";
/// systemd's limit for a unit name.
const UNIT_NAME_MAX: usize = 255;

/// Escapes every character outside `[A-Za-z0-9:.]` as `_xx` per byte. `-`
/// and `_` are escaped too, so the encoding is injective, never nests a slice
/// and keeps one id's glob from matching another id.
fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, ':' | '.') {
            out.push(c);
        } else {
            for b in c.to_string().bytes() {
                out.push_str(&format!("_{b:02x}"));
            }
        }
    }
    out
}

/// Profile name as a slice-name component; an empty profile becomes `_`.
fn escape_profile(profile: &str) -> String {
    if profile.is_empty() {
        "_".to_string()
    } else {
        escape(profile)
    }
}

fn escaped_id(session_id: &str) -> String {
    format!("{UNIT_PREFIX}{}", escape(session_id))
}

pub(crate) fn unit_name(session_id: &str, start_ms: u64, namespace: &str) -> String {
    format!(
        "{}-{namespace}-{start_ms}-{}.scope",
        escaped_id(session_id),
        uuid::Uuid::new_v4()
    )
}

pub(crate) fn unit_glob(session_id: &str) -> String {
    format!("{}-*.scope", escaped_id(session_id))
}

/// Application-directory consistency, not instance authentication or attestation.
pub(crate) fn scope_namespace() -> anyhow::Result<String> {
    scope_namespace_for(&crate::session::get_app_dir()?)
}

fn scope_namespace_for(app: &Path) -> anyhow::Result<String> {
    use sha2::Digest;
    let app = app.canonicalize()?;
    let digest = sha2::Sha256::digest(app.as_os_str().as_encoded_bytes());
    let mut namespace = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        namespace.push(char::from(HEX[usize::from(byte >> 4)]));
        namespace.push(char::from(HEX[usize::from(byte & 15)]));
    }
    Ok(namespace)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// systemd's rules for a slice name: unit-name characters, at most 255
/// bytes, and `-` only between non-empty nesting levels.
pub(crate) fn valid_slice(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".slice") else {
        return false;
    };
    name.len() <= UNIT_NAME_MAX
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_'))
        && stem.split('-').all(|level| !level.is_empty())
}

/// Bus-free guess that the user manager is reachable: the D-Bus address
/// names an existing unix socket, or `$XDG_RUNTIME_DIR` holds the user bus or
/// the manager's private socket (which `systemd-run --user` tries first, so
/// no user D-Bus is needed). Cheap, no subprocess; for `aoe list` only.
fn user_manager_reachable(runtime_dir: Option<&Path>, bus_addr: Option<&str>) -> bool {
    let addr_exists = bus_addr.is_some_and(|addr| {
        addr.split(';').any(|a| {
            a.strip_prefix("unix:")
                .and_then(|params| params.split(',').find_map(|kv| kv.strip_prefix("path=")))
                .is_some_and(|p| Path::new(p).exists())
        })
    });
    addr_exists
        || runtime_dir
            .is_some_and(|d| d.join("bus").exists() || d.join("systemd").join("private").exists())
}

/// Cheap guess that scopes are in use, without calling systemd: Linux,
/// `enabled` and a reachable user manager per the process environment. Exists
/// for `aoe list`, which must not run the capability probe.
pub(crate) fn scopes_expected(enabled: bool) -> bool {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let bus_addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
    cfg!(target_os = "linux")
        && enabled
        && user_manager_reachable(runtime_dir.as_deref(), bus_addr.as_deref())
}

pub(crate) fn description(session_id: &str) -> String {
    let short: String = session_id.chars().take(8).collect();
    format!("aoe session {short}")
}

/// True exactly once per `flag`, so a repeated condition logs once.
fn first_time(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

/// Expands `{profile}` in the configured template; an invalid result falls
/// back to the default template. Warns once per process.
pub(crate) fn resolve_slice(template: &str, profile: &str) -> String {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let slice = template.replace("{profile}", &escape_profile(profile));
    if valid_slice(&slice) {
        return slice;
    }
    if first_time(&WARNED) {
        tracing::warn!(
            target: "process.systemd",
            slice = template,
            "invalid systemd.slice; using {DEFAULT_SLICE_TEMPLATE}"
        );
    }
    DEFAULT_SLICE_TEMPLATE.replace("{profile}", &escape_profile(profile))
}

/// Parses exactly what the `memory_max` settings validator accepts: digits
/// followed by one of `b`, `k`, `m`, `g` (either case).
pub(crate) fn parse_memory_bytes(s: &str) -> Option<u64> {
    let shift = match s.chars().last()? {
        'b' | 'B' => 0,
        'k' | 'K' => 10,
        'm' | 'M' => 20,
        'g' | 'G' => 30,
        _ => return None,
    };
    let digits = &s[..s.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(1u64 << shift)
}

/// The `.service` unit a process runs in, from `/proc/<pid>/cgroup` contents.
/// Uses the cgroup v2 line; the deepest unit component wins, so a sub-cgroup
/// of a service still resolves to that service.
pub(crate) fn unit_from_cgroup(contents: &str) -> Option<String> {
    let path = contents.lines().find_map(|l| l.strip_prefix("0::"))?;
    let path = path.trim_end().trim_end_matches(" (deleted)");
    let unit = path.split('/').rev().find(|c| {
        [".service", ".scope", ".slice"]
            .iter()
            .any(|s| c.ends_with(s))
    })?;
    unit.ends_with(".service").then(|| unit.to_string())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Limits {
    pub memory_max: Option<u64>,
    pub tasks_max: Option<u64>,
}

impl Limits {
    /// The configured limits as systemd properties, e.g. `MemoryMax=1024`.
    fn properties(&self) -> Vec<String> {
        let memory = self.memory_max.map(|m| format!("MemoryMax={m}"));
        let tasks = self.tasks_max.map(|t| format!("TasksMax={t}"));
        memory.into_iter().chain(tasks).collect()
    }
}

/// Whether a session in `status` has a live process, and so may have a scope.
pub(crate) fn has_process(status: crate::session::Status) -> bool {
    use crate::session::Status;
    !matches!(
        status,
        Status::Stopped | Status::Deleting | Status::Creating | Status::Error
    )
}

pub(crate) fn status_line(running: usize, waiting: usize, idle: usize) -> String {
    format!("{running} running, {waiting} waiting, {idle} idle")
}

/// Scope settings resolved from config; `slice_template` is the raw setting.
#[derive(Debug, Clone)]
pub(crate) struct ScopeConfig {
    pub slice_template: String,
    pub limits: Limits,
    pub part_of: Option<String>,
}

static IS_DAEMON: AtomicBool = AtomicBool::new(false);

/// Marks this process as the `aoe serve` daemon. Only the daemon binds scopes
/// to its own service with `PartOf=`; a CLI or TUI would bind them to whatever
/// service happens to run it. Read only by the scope setup, which is inert
/// outside Linux.
pub(crate) fn mark_daemon() {
    IS_DAEMON.store(true, Ordering::Relaxed);
}

fn is_daemon() -> bool {
    IS_DAEMON.load(Ordering::Relaxed)
}

/// The unit to bind scopes to, read from `cgroup` only for a daemon that
/// should not outlive its sessions.
fn part_of(
    is_daemon: bool,
    survive: bool,
    cgroup: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if survive || !is_daemon {
        return None;
    }
    let unit = cgroup().and_then(|c| unit_from_cgroup(&c));
    if unit.is_none() {
        tracing::warn!(
            target: "process.systemd",
            "survive_daemon_restart=false but the daemon is not a service; ignoring"
        );
    }
    unit
}

/// An empty or blank value means no limit, like an unset one.
fn memory_limit(raw: Option<&str>) -> Option<u64> {
    let raw = raw.filter(|raw| !raw.trim().is_empty())?;
    let parsed = parse_memory_bytes(raw);
    if parsed.is_none() {
        tracing::warn!(
            target: "process.systemd",
            memory_max = raw,
            "invalid systemd.memory_max; sessions get no memory limit"
        );
    }
    // Zero means unlimited; a zero MemoryMax would kill the session.
    parsed.filter(|&m| m > 0)
}

impl ScopeConfig {
    pub(crate) fn from_config(cfg: &SystemdSettings) -> Self {
        let part_of = part_of(is_daemon(), cfg.survive_daemon_restart, || {
            std::fs::read_to_string("/proc/self/cgroup").ok()
        });
        Self {
            slice_template: cfg.slice.clone(),
            limits: Limits {
                memory_max: memory_limit(cfg.memory_max.as_deref()),
                tasks_max: (cfg.tasks_max > 0).then_some(u64::from(cfg.tasks_max)),
            },
            part_of,
        }
    }
}

/// `--expand-environment=no` keeps `$` in the command literal.
pub(crate) fn scope_args(
    cfg: &ScopeConfig,
    unit: &str,
    description: &str,
    profile: &str,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        "--no-ask-password",
        "--expand-environment=no",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(format!(
        "--slice={}",
        resolve_slice(&cfg.slice_template, profile)
    ));
    a.push(format!("--unit={unit}"));
    a.push(format!("--description={description}"));
    let part_of = cfg.part_of.iter().map(|u| format!("PartOf={u}"));
    for property in cfg.limits.properties().into_iter().chain(part_of) {
        a.push("-p".into());
        a.push(property);
    }
    a.push("--".into());
    a
}

/// Why the probe failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeFailure {
    Rejected(String),
    TimedOut,
    SpawnFailed(String),
}

impl ProbeFailure {
    fn detail(&self) -> &str {
        match self {
            Self::Rejected(stderr) | Self::SpawnFailed(stderr) => stderr,
            Self::TimedOut => "",
        }
    }
}

/// Runs `systemd-run ... -- true` once through `run` (argv includes the
/// program), with the same flags and limits as a real start.
fn probe_with(
    cfg: &ScopeConfig,
    profile: &str,
    run: &dyn Fn(&[String]) -> Result<(), ProbeFailure>,
) -> Result<(), ProbeFailure> {
    let unit = format!("aoe-probe-{}-{}.scope", std::process::id(), now_ms());
    let mut argv = vec!["systemd-run".to_string()];
    argv.extend(scope_args(cfg, &unit, "aoe capability probe", profile));
    argv.push("true".into());
    run(&argv)
}

/// The warning for a probe the manager rejected while limits were configured
/// or with stderr naming a property; `None` means an info line suffices.
fn limits_warning(limits: &Limits, failure: &ProbeFailure) -> Option<String> {
    let ProbeFailure::Rejected(stderr) = failure else {
        return None;
    };
    let properties = limits.properties();
    let names_property = ["MemoryMax", "TasksMax", "PartOf", "property"]
        .iter()
        .any(|p| stderr.contains(p));
    if properties.is_empty() && !names_property {
        return None;
    }
    let what = if properties.is_empty() {
        "a scope property".to_string()
    } else {
        properties.join(", ")
    };
    Some(format!(
        "systemd-run rejected {what}; sessions start unscoped"
    ))
}

/// The `[systemd]` settings, read once per process.
fn settings() -> &'static SystemdSettings {
    static SETTINGS: OnceLock<SystemdSettings> = OnceLock::new();
    SETTINGS.get_or_init(|| crate::session::Config::load_or_warn().systemd)
}

struct Capability {
    systemd_run: PathBuf,
    env_bin: PathBuf,
    cfg: ScopeConfig,
}

fn log_probe_failure(limits: &Limits, failure: &ProbeFailure) {
    let detail = failure.detail();
    if let Some(warning) = limits_warning(limits, failure) {
        tracing::warn!(target: "process.systemd", stderr = detail, "{warning}");
        return;
    }
    let reason = match failure {
        ProbeFailure::TimedOut => "systemd-run timed out",
        ProbeFailure::SpawnFailed(_) => "systemd-run could not be started",
        ProbeFailure::Rejected(_) => "systemd-run could not reach the user manager",
    };
    tracing::info!(target: "process.systemd", "{reason}; sessions start unscoped");
    if !detail.is_empty() {
        tracing::debug!(target: "process.systemd", stderr = detail, "systemd-run probe output");
    }
}

/// `None` outside Linux, under `cfg(test)`, when disabled, or when the probe
/// fails.
fn capability() -> Option<&'static Capability> {
    static CAP: OnceLock<Option<Capability>> = OnceLock::new();
    CAP.get_or_init(|| {
        if cfg!(test) || !cfg!(target_os = "linux") {
            return None;
        }
        if !settings().enabled {
            tracing::info!(target: "process.systemd", "systemd.enabled=false; sessions start unscoped");
            return None;
        }
        let tools = which::which("systemd-run").and_then(|run| Ok((run, which::which("env")?)));
        let Ok((systemd_run, env_bin)) = tools else {
            tracing::info!(
                target: "process.systemd",
                "systemd-run or env not found; sessions start unscoped"
            );
            return None;
        };
        let cfg = ScopeConfig::from_config(settings());
        let real = |argv: &[String]| -> Result<(), ProbeFailure> {
            let mut cmd = Command::new(&systemd_run);
            cmd.args(&argv[1..]);
            match crate::process::run_with_timeout(&mut cmd, Duration::from_secs(5)) {
                Ok(Some(o)) if o.status.success() => Ok(()),
                Ok(Some(o)) => Err(ProbeFailure::Rejected(
                    String::from_utf8_lossy(&o.stderr).trim().to_string(),
                )),
                Ok(None) => Err(ProbeFailure::TimedOut),
                Err(e) => Err(ProbeFailure::SpawnFailed(e.to_string())),
            }
        };
        // The real profile, so the probe creates no slice a session would not.
        let profile = crate::session::config::effective_profile("");
        match probe_with(&cfg, &profile, &real) {
            Ok(()) => {
                tracing::info!(target: "process.systemd", "systemd scopes enabled");
                Some(Capability {
                    systemd_run,
                    env_bin,
                    cfg,
                })
            }
            Err(failure) => {
                log_probe_failure(&cfg.limits, &failure);
                None
            }
        }
    })
    .as_ref()
}

/// The words to put in front of a session command.
#[derive(Debug, Clone)]
pub(crate) struct ScopePrefix {
    /// Absolute `systemd-run` and its flags, ending in `--`.
    pub argv: Vec<String>,
    /// Absolute `env`, which restores what `systemd-run` changed.
    pub env_bin: String,
}

impl ScopePrefix {
    /// The only scope this prefix can create; its exact name survives SID reuse.
    pub(crate) fn ownership(&self) -> crate::process::worker_registry::ScopeOwnership {
        self.argv
            .iter()
            .find_map(|arg| arg.strip_prefix("--unit="))
            .map(
                |unit| crate::process::worker_registry::ScopeOwnership::Owned {
                    unit: unit.to_owned(),
                },
            )
            .unwrap_or(crate::process::worker_registry::ScopeOwnership::Unscoped)
    }

    /// `argv`, then `<env> -u INVOCATION_ID`; callers append their own
    /// restore words and the command.
    pub(crate) fn with_env_restore(&self) -> Vec<String> {
        let mut words = self.argv.clone();
        words.extend([self.env_bin.clone(), "-u".into(), INJECTED_VAR.into()]);
        words
    }
}

/// The prefix to run a session command in a fresh scope, or `None` when
/// scopes are unavailable. An empty `profile` resolves to the effective
/// profile. Blocking: the first call in a process runs the capability probe
/// (one attempt, 5 s at most), so call it from `spawn_blocking` in async code.
pub(crate) fn scope_prefix_for(session_id: &str, profile: &str) -> Option<ScopePrefix> {
    let cap = capability()?;
    let profile = if profile.is_empty() {
        crate::session::config::effective_profile("")
    } else {
        profile.to_string()
    };
    let namespace = scope_namespace().ok()?;
    let unit = unit_name(session_id, now_ms(), &namespace);
    let mut argv = vec![cap.systemd_run.to_string_lossy().into_owned()];
    argv.extend(scope_args(
        &cap.cfg,
        &unit,
        &description(session_id),
        &profile,
    ));
    Some(ScopePrefix {
        argv,
        env_bin: cap.env_bin.to_string_lossy().into_owned(),
    })
}

/// Retire only frozen owned units, including scopes created before configuration was disabled.
pub(crate) fn retire_scopes(
    session_id: &str,
    scopes: &[crate::process::worker_registry::ScopeOwnership],
) -> anyhow::Result<()> {
    #[cfg(test)]
    release_probe::observe(session_id);
    scopes
        .iter()
        .try_for_each(|scope| retire_scope(session_id, scope))
}
/// Prove a captured scope empty without selecting any replacement by SID.
pub(crate) fn scope_stopped(
    session_id: &str,
    scope: &crate::process::worker_registry::ScopeOwnership,
) -> anyhow::Result<bool> {
    use crate::process::worker_registry::ScopeOwnership;
    let unit = match scope {
        ScopeOwnership::Unscoped => return Ok(true),
        ScopeOwnership::Unresolved => anyhow::bail!("runner scope ownership is unresolved"),
        ScopeOwnership::Owned { unit } => unit,
    };
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "captured Linux scope cannot be checked on this platform"
    );
    anyhow::ensure!(
        unit.starts_with(&format!("{}-", escaped_id(session_id)))
            && unit.ends_with(".scope")
            && unit
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b':')),
        "invalid captured scope unit"
    );
    let mut command = Command::new(which::which("systemctl")?);
    command.args([
        "--user",
        "show",
        "--property=LoadState",
        "--property=ActiveState",
        "--property=ControlGroup",
        unit,
    ]);
    let output = crate::process::run_with_timeout(&mut command, Duration::from_secs(3))?
        .ok_or_else(|| anyhow::anyhow!("scope status timed out"))?;
    let text = std::str::from_utf8(&output.stdout)?;
    let property = |name: &str| text.lines().find_map(|line| line.strip_prefix(name));
    let load =
        property("LoadState=").ok_or_else(|| anyhow::anyhow!("scope load state unavailable"))?;
    let active = property("ActiveState=")
        .ok_or_else(|| anyhow::anyhow!("scope active state unavailable"))?;
    let cgroup =
        property("ControlGroup=").ok_or_else(|| anyhow::anyhow!("scope cgroup unavailable"))?;
    anyhow::ensure!(
        output.status.success() || load == "not-found",
        "scope status rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(matches!(active, "inactive" | "failed") && cgroup.is_empty())
}
pub(crate) fn legacy_unknown_scope_absent(
    session_id: &str,
    known: &[crate::process::worker_registry::ScopeOwnership],
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        crate::session::is_valid_session_id(session_id),
        "Invalid legacy session identity"
    );
    #[cfg(target_os = "linux")]
    {
        super::linux::legacy_scope_inventory_clear(&format!("{}-", escaped_id(session_id)), known)
    }
    #[cfg(target_os = "macos")]
    {
        super::macos::legacy_scope_inventory_clear(known)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = known;
        anyhow::bail!("Legacy scope authority recovery is unsupported on this platform")
    }
}

/// Blocking exact-unit retirement; only completed, empty scope exit is success.
pub(crate) fn retire_scope(
    session_id: &str,
    scope: &crate::process::worker_registry::ScopeOwnership,
) -> anyhow::Result<()> {
    if scope_stopped(session_id, scope)? {
        return Ok(());
    }
    let crate::process::worker_registry::ScopeOwnership::Owned { unit } = scope else {
        unreachable!()
    };
    let mut command = Command::new(which::which("systemctl")?);
    command.args(["--user", "stop", unit]);
    let output = crate::process::run_with_timeout(&mut command, Duration::from_secs(3))?
        .ok_or_else(|| anyhow::anyhow!("scope stop timed out"))?;
    let proven_empty = scope_stopped(session_id, scope)?;
    anyhow::ensure!(
        output.status.success() || proven_empty,
        "scope stop rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    anyhow::ensure!(proven_empty, "captured scope is not empty after stop");
    Ok(())
}

/// Observes this thread's `release` calls, so tests can check when one runs.
#[cfg(test)]
pub(crate) mod release_probe {
    use std::cell::RefCell;

    type Probe = Box<dyn Fn(&str)>;

    thread_local! {
        static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
    }

    /// Uninstalls the probe on drop.
    pub(crate) struct Installed;

    impl Drop for Installed {
        fn drop(&mut self) {
            PROBE.with(|probe| probe.borrow_mut().take());
        }
    }

    pub(crate) fn install(probe: impl Fn(&str) + 'static) -> Installed {
        PROBE.with(|slot| *slot.borrow_mut() = Some(Box::new(probe)));
        Installed
    }

    pub(super) fn observe(session_id: &str) {
        PROBE.with(|probe| {
            if let Some(probe) = probe.borrow().as_ref() {
                probe(session_id);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_manager_detection() {
        let with_bus = tempfile::tempdir().unwrap();
        let bus = with_bus.path().join("bus");
        std::fs::write(&bus, "").unwrap();
        let private_only = tempfile::tempdir().unwrap();
        std::fs::create_dir(private_only.path().join("systemd")).unwrap();
        std::fs::write(private_only.path().join("systemd/private"), "").unwrap();
        let empty = tempfile::tempdir().unwrap();
        let (with, private, none) = (with_bus.path(), private_only.path(), empty.path());
        let live = format!("unix:path={},guid=0123", bus.display());
        let gone = format!("unix:path={}", none.join("bus").display());
        let fallback = format!("{gone};{live}");
        let cases = [
            (Some(with), None, true),
            // No user D-Bus: `systemd-run --user` uses the private socket.
            (Some(private), None, true),
            (Some(private), Some(gone.as_str()), true),
            (Some(none), None, false),
            (None, None, false),
            (Some(with), Some(""), true),
            (None, Some(live.as_str()), true),
            // The runtime dir the address points into is gone.
            (Some(none), Some(gone.as_str()), false),
            (None, Some(fallback.as_str()), true),
            (None, Some("unix:abstract=/tmp/dbus-x"), false),
            (None, Some("tcp:host=localhost,port=1"), false),
        ];
        for (dir, addr, want) in cases {
            assert_eq!(
                user_manager_reachable(dir, addr),
                want,
                "dir={dir:?} addr={addr:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn scopes_expected_reads_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("systemd")).unwrap();
        std::fs::write(dir.path().join("systemd/private"), "").unwrap();
        let path = dir.path().to_str().unwrap();
        let _env = crate::session::test_support::EnvGuard::unset(&["DBUS_SESSION_BUS_ADDRESS"])
            .and_set("XDG_RUNTIME_DIR", path);
        assert_eq!(scopes_expected(true), cfg!(target_os = "linux"));
        assert!(!scopes_expected(false));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[serial_test::serial]
    fn retire_scope_requires_positive_empty_proof_after_rejected_stop() {
        use crate::process::worker_registry::ScopeOwnership;
        use std::os::unix::fs::PermissionsExt;

        let manager = tempfile::tempdir().unwrap();
        let executable = manager.path().join("systemctl");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
root=${0%/*}
for unit in "$@"; do :; done
case "$2" in
  show)
    IFS= read -r state < "$root/$unit" || exit 2
    case "$state" in
      active) printf 'LoadState=loaded\nActiveState=active\nControlGroup=/owned\n' ;;
      collected) printf 'LoadState=not-found\nActiveState=inactive\nControlGroup=\n' ;;
      nonempty) printf 'LoadState=loaded\nActiveState=inactive\nControlGroup=/owned\n' ;;
      unknown) printf 'LoadState=loaded\nControlGroup=\n' ;;
      unavailable) exit 7 ;;
      *) exit 2 ;;
    esac
    ;;
  stop)
    IFS= read -r mode < "$root/mode" || exit 2
    case "$mode" in
      collected|unknown|unavailable)
        printf '%s\n' "$mode" > "$root/$unit"
        exit 1
        ;;
      nonempty)
        printf 'nonempty\n' > "$root/$unit"
        exit 0
        ;;
      active) exit 1 ;;
      *) exit 2 ;;
    esac
    ;;
  *) exit 2 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _env = crate::session::test_support::path_prepended(manager.path());
        let id = "retire-race";
        let unit = format!("{}-original.scope", escaped_id(id));
        let unrelated = format!("{}-replacement.scope", escaped_id(id));
        let scope = ScopeOwnership::Owned { unit: unit.clone() };
        for (mode, succeeds, expected) in [
            ("collected", true, "collected\n"),
            ("active", false, "active\n"),
            ("nonempty", false, "nonempty\n"),
            ("unknown", false, "unknown\n"),
            ("unavailable", false, "unavailable\n"),
        ] {
            std::fs::write(manager.path().join("mode"), format!("{mode}\n")).unwrap();
            std::fs::write(manager.path().join(&unit), b"active\n").unwrap();
            std::fs::write(manager.path().join(&unrelated), b"active\n").unwrap();
            assert!(!scope_stopped(id, &scope).unwrap(), "{mode}: starts active");
            let result = retire_scope(id, &scope);
            assert_eq!(result.is_ok(), succeeds, "{mode}: {result:?}");
            assert_eq!(
                std::fs::read(manager.path().join(&unit)).unwrap(),
                expected.as_bytes(),
                "{mode}: owned manager state"
            );
            assert_eq!(
                std::fs::read(manager.path().join(&unrelated)).unwrap(),
                b"active\n",
                "{mode}: unrelated unit must remain untouched"
            );
            if succeeds {
                assert!(scope_stopped(id, &scope).unwrap());
            }
        }
    }

    #[test]
    fn profile_escaping_is_injective_and_never_nests() {
        assert_eq!(escape_profile("main"), "main");
        assert_eq!(escape_profile("my-profile"), "my_2dprofile");
        assert_eq!(escape_profile("a_b"), "a_5fb");
        assert_eq!(escape_profile("Ünï"), "_c3_9cn_c3_af");
        assert_eq!(escape_profile(""), "_");
        assert!(!escape_profile("x-y z/é").contains('-'));
    }

    #[test]
    fn unit_names_carry_id_and_start_number() {
        let first = unit_name("abc123", 1_700_000_000_000, "namespace");
        assert!(first.starts_with("aoe-session-abc123-namespace-1700000000000-"));
        let nonce = first
            .strip_prefix("aoe-session-abc123-namespace-1700000000000-")
            .unwrap()
            .strip_suffix(".scope")
            .unwrap();
        uuid::Uuid::parse_str(nonce).unwrap();
        assert_ne!(
            first,
            unit_name("abc123", 1_700_000_000_000, "namespace"),
            "concurrent same-SID starts cannot share scope authority"
        );
        assert_eq!(unit_glob("abc123"), "aoe-session-abc123-*.scope");
        assert!(unit_name("a b", 5, "namespace").starts_with("aoe-session-a_20b-namespace-5-"));
        assert!(
            unit_name("a_b/c", 5, "namespace").starts_with("aoe-session-a_5fb_2fc-namespace-5-")
        );
        assert_eq!(unit_glob("a-b"), "aoe-session-a_2db-*.scope");
        assert!(!unit_name("a-b", 5, "namespace").starts_with("aoe-session-a-"));
        assert_eq!(description("abc123def4567890"), "aoe session abc123de");
    }

    #[test]
    fn slice_names_follow_systemd_rules() {
        let long_ok = format!("{}.slice", "a".repeat(UNIT_NAME_MAX - 6));
        let too_long = format!("{}.slice", "a".repeat(UNIT_NAME_MAX - 5));
        for good in [
            "aoe.slice",
            "background-aoe-main.slice",
            "a_b:c.d.slice",
            &long_ok,
        ] {
            assert!(valid_slice(good), "{good:?}");
        }
        for bad in [
            "",
            ".slice",
            "-.slice",
            "-a.slice",
            "a-.slice",
            "a--b.slice",
            "a b.slice",
            "x.service",
            "{profile}.slice",
            &too_long,
        ] {
            assert!(!valid_slice(bad), "{bad:?}");
        }
    }

    #[test]
    fn slice_template_substitutes_escaped_profile() {
        let t = "background-aoe-{profile}.slice";
        assert_eq!(resolve_slice(t, "main"), "background-aoe-main.slice");
        assert_eq!(
            resolve_slice(t, "my-profile"),
            "background-aoe-my_2dprofile.slice"
        );
        assert_eq!(resolve_slice(t, ""), "background-aoe-_.slice");
        assert_eq!(resolve_slice("aoe.slice", "main"), "aoe.slice");
        for bad in [
            "",
            "foo",
            "x.service",
            "a b.slice",
            ".slice",
            "{profile}",
            "a--b.slice",
        ] {
            assert_eq!(
                resolve_slice(bad, "main"),
                "background-aoe-main.slice",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn bad_slice_falls_back_every_time_and_the_log_gate_fires_once() {
        for _ in 0..5 {
            assert_eq!(resolve_slice("bogus", "main"), "background-aoe-main.slice");
        }
        let flag = AtomicBool::new(false);
        let fired = (0..5).filter(|_| first_time(&flag)).count();
        assert_eq!(fired, 1);
    }

    #[test]
    fn scope_args_shape() {
        let cfg = ScopeConfig {
            slice_template: "background-aoe-{profile}.slice".into(),
            limits: Limits {
                memory_max: Some(1 << 30),
                tasks_max: Some(100),
            },
            part_of: Some("aoe.service".into()),
        };
        let want = [
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "--no-ask-password",
            "--expand-environment=no",
            "--slice=background-aoe-main.slice",
            "--unit=aoe-session-abc123-1.scope",
            "--description=aoe session abc123de",
            "-p",
            "MemoryMax=1073741824",
            "-p",
            "TasksMax=100",
            "-p",
            "PartOf=aoe.service",
            "--",
        ];
        let args = scope_args(
            &cfg,
            "aoe-session-abc123-1.scope",
            "aoe session abc123de",
            "main",
        );
        assert_eq!(args, want);
        let bare = ScopeConfig {
            limits: Limits::default(),
            part_of: None,
            ..cfg
        };
        assert_eq!(scope_args(&bare, "u.scope", "d", "main").len(), 10);
    }

    #[test]
    fn probe_makes_one_attempt_with_the_real_flags() {
        let cfg = ScopeConfig {
            slice_template: DEFAULT_SLICE_TEMPLATE.into(),
            limits: Limits {
                memory_max: Some(1 << 30),
                tasks_max: None,
            },
            part_of: None,
        };
        let cases = [
            Ok(()),
            Err(ProbeFailure::Rejected("no bus".into())),
            Err(ProbeFailure::TimedOut),
            Err(ProbeFailure::SpawnFailed("no systemd-run".into())),
        ];
        for want in cases {
            let seen = std::cell::RefCell::new(Vec::new());
            let got = probe_with(&cfg, "main", &|argv| {
                seen.borrow_mut().push(argv.to_vec());
                want.clone()
            });
            assert_eq!(got, want);
            let seen = seen.into_inner();
            assert_eq!(seen.len(), 1, "{want:?}");
            let argv = &seen[0];
            for flag in ["--scope", "--expand-environment=no", "MemoryMax=1073741824"] {
                assert!(argv.iter().any(|a| a == flag), "{flag}");
            }
            assert!(argv.iter().any(|a| a.starts_with("--unit=aoe-probe-")));
            assert_eq!(argv.last().map(String::as_str), Some("true"));
        }
    }

    #[test]
    fn probe_failure_warns_when_limits_were_the_likely_reason() {
        let limits = Limits {
            memory_max: Some(1 << 30),
            tasks_max: Some(64),
        };
        let none = Limits::default();
        let rejected = |s: &str| ProbeFailure::Rejected(s.into());
        let cases = [
            (
                limits,
                rejected("Failed to start transient scope unit"),
                Some("systemd-run rejected MemoryMax=1073741824, TasksMax=64; sessions start unscoped"),
            ),
            (
                none,
                rejected("Unknown assignment: PartOf=x"),
                Some("systemd-run rejected a scope property; sessions start unscoped"),
            ),
            (none, rejected("Failed to connect to bus"), None),
            (limits, ProbeFailure::TimedOut, None),
            (limits, ProbeFailure::SpawnFailed("not found".into()), None),
        ];
        for (limits, failure, want) in cases {
            assert_eq!(
                limits_warning(&limits, &failure).as_deref(),
                want,
                "{limits:?} {failure:?}"
            );
        }
    }

    #[test]
    fn prefix_restores_the_injected_variable_with_env() {
        let prefix = ScopePrefix {
            argv: vec!["/usr/bin/systemd-run".into(), "--".into()],
            env_bin: "/usr/bin/env".into(),
        };
        assert_eq!(
            prefix.with_env_restore(),
            [
                "/usr/bin/systemd-run",
                "--",
                "/usr/bin/env",
                "-u",
                "INVOCATION_ID"
            ]
        );
    }

    #[test]
    fn memory_parsing() {
        assert_eq!(parse_memory_bytes("512b"), Some(512));
        assert_eq!(parse_memory_bytes("1k"), Some(1024));
        assert_eq!(parse_memory_bytes("4G"), Some(4 << 30));
        assert_eq!(parse_memory_bytes("512m"), Some(512 << 20));
        for bad in ["", "512", "4GB", "512mb", "x", "-1k", "+1k", " 1k", "k"] {
            assert_eq!(parse_memory_bytes(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn empty_memory_limit_means_unlimited() {
        for (raw, want) in [
            (None, None),
            (Some(""), None),
            (Some("  "), None),
            (Some("0m"), None),
            (Some("4G"), Some(4 << 30)),
            (Some("bogus"), None),
        ] {
            assert_eq!(memory_limit(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn memory_parser_agrees_with_the_settings_validator() {
        use crate::session::config::profile_config::validate_memory_limit;
        let suffixes = ["", "b", "B", "k", "K", "m", "M", "g", "G", "mb", "GB", "x"];
        for d in ["", "0", "1", "512", "-1"] {
            for s in suffixes {
                let value = format!("{d}{s}");
                assert_eq!(
                    parse_memory_bytes(&value).is_some(),
                    validate_memory_limit(&value).is_ok() && !value.is_empty(),
                    "{value:?}"
                );
            }
        }
    }

    #[test]
    fn cgroup_unit_detection() {
        let svc = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/agent-ui.service\n";
        assert_eq!(unit_from_cgroup(svc).as_deref(), Some("agent-ui.service"));
        let scope = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-x.scope\n";
        assert_eq!(unit_from_cgroup(scope), None);
        assert_eq!(unit_from_cgroup(""), None);
        let sub = "0::/user.slice/user@1000.service/app.slice/agent-ui.service/sub\n";
        assert_eq!(unit_from_cgroup(sub).as_deref(), Some("agent-ui.service"));
        let hybrid = "12:pids:/user.slice/x.service\n1:name=systemd:/a.service\n\
                      0::/user.slice/user@1000.service/app.slice/aoe.service (deleted)\n";
        assert_eq!(unit_from_cgroup(hybrid).as_deref(), Some("aoe.service"));
        let none = "0::/user.slice/user-1000.slice/session-2.scope\n";
        assert_eq!(unit_from_cgroup(none), None);
    }

    #[test]
    fn part_of_only_for_a_daemon_that_does_not_survive() {
        let svc = || Some("0::/user.slice/user@1000.service/app.slice/aoe.service\n".to_string());
        let read_never = || -> Option<String> { panic!("cgroup read outside the daemon") };
        assert_eq!(part_of(false, false, read_never), None);
        assert_eq!(part_of(false, true, read_never), None);
        assert_eq!(part_of(true, true, read_never), None);
        assert_eq!(part_of(true, false, svc).as_deref(), Some("aoe.service"));
        let scope = || Some("0::/user.slice/user-1000.slice/session-2.scope\n".to_string());
        assert_eq!(part_of(true, false, scope), None);
    }

    #[test]
    fn status_text() {
        assert_eq!(status_line(3, 1, 2), "3 running, 1 waiting, 2 idle");
    }
}
