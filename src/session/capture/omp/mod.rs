//! Oh My Pi (OMP) session capture, attributed only through terminal breadcrumbs.
//!
//! Store layout and launch identity live in tmux and are reloaded every poll so
//! a restarted process cannot attach to a superseded pane generation.
//!
//! Wire formats with no shared serializer: `wrap_omp_launch` (sh), OMP itself,
//! the Rust readers and `CONTAINER_BREADCRUMB_SCRIPT` must change together.
//!
//! Launch marker, exactly 4 lines: terminal id (tty leaf, `/` as `-`), launch id,
//! non-empty pending pre-launch session path, routing fingerprint (64 hex).
//! Terminal breadcrumb, 2 to 4 lines: absolute cwd, session path, zero, one, or
//! both extras in either order, each at most once: literal `fresh` or
//! `cwdstat <dev> <ino>` with decimal device and inode values.
//!
//! The marker's launch id and fingerprint prove the generation, not that the
//! breadcrumb was written after launch; that also requires freshness (host:
//! mtime > `launched_at_ms`; container: breadcrumb `-nt` the marker).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod container;
mod layout;
mod options;

use container::*;
use layout::*;

pub(crate) use container::{omp_poll_fn_sandboxed, try_capture_omp_session_id_in_container};
pub(crate) use layout::{
    host_launcher_environment, omp_host_routing_environment, read_container_environment,
    resolve_omp_store_layout, resolve_omp_store_layout_in_container_with_environment,
    resolve_omp_store_layout_with_environment,
};
pub(crate) use options::{reject_omp_secret_args, OmpCliCaptureOptions};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DOTENV_BYTES: usize = 1024 * 1024;
const MAX_CONTAINER_ENV_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONTAINER_PROBE_BYTES: usize = 16 * 1024;
const MAX_BREADCRUMB_BYTES: usize = 16 * 1024;
const MAX_LAUNCH_MARKER_BYTES: usize = MAX_BREADCRUMB_BYTES + 1024;
// Header window plus breadcrumb fields, so the container accepts every header the host scan does.
const MAX_CONTAINER_CAPTURE_BYTES: usize = super::pi::PI_HEADER_SCAN_BYTES + MAX_BREADCRUMB_BYTES;
pub(crate) const OMP_STORE_ENV_KEYS: [&str; 9] = [
    "HOME",
    "NODE_ENV",
    "OMP_PROFILE",
    "PI_PROFILE",
    "PI_CODING_AGENT_DIR",
    "PI_CODING_AGENT_SESSION_DIR",
    "PI_CONFIG_DIR",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
];

/// Shape of the effective OMP session store.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OmpStoreKind {
    /// OMP's normal bucket-per-cwd `sessions/<bucket>/<session>.jsonl` layout.
    Managed,
    /// An explicit flat `--session-dir` / `PI_CODING_AGENT_SESSION_DIR` layout.
    Custom,
}

/// Absolute roots used by one OMP process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OmpStoreLayout {
    pub sessions: PathBuf,
    /// Managed store retained even when `sessions` is an explicit custom store.
    pub managed_sessions: PathBuf,
    pub terminal_sessions: PathBuf,
    pub kind: OmpStoreKind,
}

#[derive(Debug)]
pub(crate) struct OmpResolvedContext {
    pub(crate) layout: OmpStoreLayout,
    pub(crate) routing_fingerprint: String,
    pub(crate) launcher_routing: Vec<(String, Option<String>)>,
    pub(crate) profile: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) agent_dir: PathBuf,
}

fn omp_routing_values(environment: &HashMap<String, String>) -> Vec<(String, Option<String>)> {
    OMP_STORE_ENV_KEYS
        .iter()
        .map(|key| ((*key).to_owned(), environment.get(*key).cloned()))
        .collect()
}

/// Transient launch snapshot; only the routing fingerprint reaches pane metadata.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct OmpCapturePlan {
    pub layout: OmpStoreLayout,
    pub routing_fingerprint: String,
    pub launch_id: String,
    pub launch_marker: String,
    pub container_runtime: Option<crate::session::config::ContainerRuntimeName>,
}

/// Stable capture inputs persisted with a tmux session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OmpCaptureMetadata {
    pub layout: OmpStoreLayout,
    pub launched_at_ms: u64,
    #[serde(default)]
    pub launch_id: String,
    #[serde(default)]
    pub launch_marker: String,
    #[serde(default)]
    pub routing_fingerprint: String,
    #[serde(default)]
    pub container_runtime: Option<crate::session::config::ContainerRuntimeName>,
}

impl OmpCaptureMetadata {
    /// Legacy (pre-marker) generations persist unguarded; marked ones carry the launch id CAS.
    pub(crate) fn session_observation(
        &self,
        sid: String,
    ) -> crate::session::poller::SessionIdObservation {
        if self.launch_marker.is_empty() {
            crate::session::poller::SessionIdObservation::omp_legacy(sid)
        } else {
            crate::session::poller::SessionIdObservation::omp(sid, self.launch_id.clone())
        }
    }
}

/// The per-instance sandbox launch marker path, reused across relaunches and
/// never unlinked. Reuse is safe only because stale markers fail the launch id
/// and fingerprint checks and a marked generation needs its READY key; do not
/// weaken those to an existence test.
pub(crate) fn omp_sandbox_launch_marker(instance_id: &str) -> String {
    let safe = instance_id
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        .map(char::from)
        .collect::<String>();
    format!("/tmp/aoe-omp-launch-{safe}")
}

fn valid_omp_terminal_id(terminal_id: &str) -> bool {
    !terminal_id.is_empty()
        && !matches!(terminal_id, "." | "..")
        && terminal_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn omp_terminal_id_from_tty(tty: &str) -> Option<String> {
    let device = tty.strip_prefix("/dev/")?;
    let terminal_id = device.replace('/', "-");
    valid_omp_terminal_id(&terminal_id).then_some(terminal_id)
}

fn tty_and_terminal_id_for_tmux(tmux_session_name: &str) -> Result<(String, String)> {
    let tty = crate::tmux::Session::from_name(tmux_session_name).pane_tty()?;
    let terminal_id = omp_terminal_id_from_tty(&tty)
        .ok_or_else(|| anyhow::anyhow!("Unsupported OMP pane TTY: {tty:?}"))?;
    Ok((tty, terminal_id))
}

fn load_omp_capture_metadata(tmux_session_name: &str) -> Result<OmpCaptureMetadata> {
    let key = crate::tmux::env::AOE_OMP_CAPTURE_META_KEY;
    let output = crate::tmux::tmux_command()
        .args(["show-environment", "-h", "-t", tmux_session_name, key])
        .output()
        .context("Failed to read OMP capture metadata from tmux")?;
    if !output.status.success() {
        anyhow::bail!("OMP capture metadata is unavailable in tmux");
    }
    let encoded = String::from_utf8(output.stdout).context("OMP capture metadata is not UTF-8")?;
    let encoded = encoded
        .strip_suffix("\r\n")
        .or_else(|| encoded.strip_suffix('\n'))
        .context("tmux returned unterminated OMP capture metadata")?;
    let encoded = encoded
        .strip_prefix(key)
        .and_then(|value| value.strip_prefix('='))
        .context("tmux returned malformed OMP capture metadata")?;
    if encoded.contains('\r') || encoded.contains('\n') {
        anyhow::bail!("tmux returned trailing OMP capture metadata");
    }
    let metadata: OmpCaptureMetadata =
        serde_json::from_str(encoded).context("tmux returned invalid OMP capture metadata")?;
    validate_omp_capture_metadata(&metadata)?;
    if !metadata.launch_marker.is_empty() {
        let ready = crate::tmux::env::get_hidden_env(
            tmux_session_name,
            crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
        );
        anyhow::ensure!(
            ready.as_deref() == Some(metadata.launch_id.as_str()),
            "OMP capture metadata generation is not ready"
        );
    }
    Ok(metadata)
}

pub(crate) fn validate_omp_capture_metadata(metadata: &OmpCaptureMetadata) -> Result<()> {
    validate_layout(&metadata.layout)?;
    if metadata.launched_at_ms == 0
        || metadata.launch_id.is_empty()
        || metadata.launch_id.contains('\r')
        || metadata.launch_id.contains('\n')
        || (!metadata.launch_marker.is_empty()
            && (!Path::new(&metadata.launch_marker).is_absolute()
                || metadata.launch_marker.contains('\r')
                || metadata.launch_marker.contains('\n')
                || metadata.routing_fingerprint.len() != 64
                || !metadata
                    .routing_fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())))
    {
        anyhow::bail!("OMP capture metadata has an invalid generation");
    }
    Ok(())
}

#[derive(Debug)]
struct Breadcrumb<'a> {
    cwd: &'a str,
    session_path: &'a str,
    fresh: bool,
}

fn parse_breadcrumb_extras<'a>(extras: impl Iterator<Item = &'a str>) -> Result<bool> {
    let mut fresh = false;
    let mut cwdstat = false;
    for extra in extras {
        if extra == "fresh" {
            if fresh {
                anyhow::bail!("OMP terminal breadcrumb has duplicate fresh metadata");
            }
            fresh = true;
            continue;
        }

        let Some(identity) = extra.strip_prefix("cwdstat ") else {
            anyhow::bail!("OMP terminal breadcrumb has unknown metadata");
        };
        if cwdstat {
            anyhow::bail!("OMP terminal breadcrumb has duplicate cwdstat metadata");
        }
        let mut fields = identity.split(' ');
        let valid = matches!(fields.next(), Some(dev) if !dev.is_empty() && dev.bytes().all(|byte| byte.is_ascii_digit()))
            && matches!(fields.next(), Some(ino) if !ino.is_empty() && ino.bytes().all(|byte| byte.is_ascii_digit()))
            && fields.next().is_none();
        if !valid {
            anyhow::bail!("OMP terminal breadcrumb has invalid cwdstat metadata");
        }
        cwdstat = true;
    }
    Ok(fresh)
}

fn parse_breadcrumb(content: &str) -> Result<Breadcrumb<'_>> {
    let mut lines = content.lines();
    let cwd = lines
        .next()
        .filter(|value| !value.is_empty())
        .context("OMP terminal breadcrumb has no cwd")?;
    let session_path = lines
        .next()
        .filter(|value| !value.is_empty())
        .context("OMP terminal breadcrumb has no session path")?;
    let fresh = parse_breadcrumb_extras(lines)?;
    Ok(Breadcrumb {
        cwd,
        session_path,
        fresh,
    })
}

fn validate_layout(layout: &OmpStoreLayout) -> Result<()> {
    if !layout.sessions.is_absolute()
        || !layout.managed_sessions.is_absolute()
        || !layout.terminal_sessions.is_absolute()
    {
        anyhow::bail!("OMP capture layout roots must be absolute");
    }
    Ok(())
}

/// `path` sits exactly `components` levels under `root` (managed 2, custom 1).
/// Mirrored in `CONTAINER_BREADCRUMB_SCRIPT`; the parity test locks them together.
fn has_store_shape(path: &Path, root: &Path, components: usize) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|relative| relative.components().count() == components)
}

/// Lexically resolves the breadcrumb target and rejects anything outside the
/// store's session-file depth, before anything opens it.
fn lexical_store_session_path(
    layout: &OmpStoreLayout,
    breadcrumb: &Breadcrumb<'_>,
) -> Result<PathBuf> {
    validate_layout(layout)?;
    let raw_path = Path::new(breadcrumb.session_path);
    let session_path = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else if layout.kind == OmpStoreKind::Custom {
        absolute_path(Path::new(breadcrumb.cwd), raw_path)
    } else {
        anyhow::bail!("Managed OMP breadcrumb session path is not absolute");
    };
    let normalized_path = crate::git::template::lexical_normalize(&session_path);
    let normalized_active = crate::git::template::lexical_normalize(&layout.sessions);
    let normalized_managed = crate::git::template::lexical_normalize(&layout.managed_sessions);
    let active_components = match layout.kind {
        OmpStoreKind::Managed => 2,
        OmpStoreKind::Custom => 1,
    };
    let valid_store = has_store_shape(&normalized_path, &normalized_active, active_components)
        || has_store_shape(&normalized_path, &normalized_managed, 2);
    if !valid_store
        || session_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("jsonl")
    {
        anyhow::bail!("OMP breadcrumb does not point to an allowed session JSONL");
    }
    Ok(session_path)
}

/// Re-checks store membership on the canonical path before the host opens it,
/// so an in-store directory symlink cannot redirect the read.
fn ensure_canonical_store(layout: &OmpStoreLayout, session_path: &Path) -> Result<PathBuf> {
    let active_components = match layout.kind {
        OmpStoreKind::Managed => 2,
        OmpStoreKind::Custom => 1,
    };
    let canonical_path = session_path
        .canonicalize()
        .context("Failed to canonicalize OMP session JSONL")?;
    let canonical_store = layout
        .sessions
        .canonicalize()
        .ok()
        .is_some_and(|root| has_store_shape(&canonical_path, &root, active_components))
        || layout
            .managed_sessions
            .canonicalize()
            .ok()
            .is_some_and(|root| has_store_shape(&canonical_path, &root, 2));
    anyhow::ensure!(
        canonical_store,
        "OMP breadcrumb resolves outside its allowed session store"
    );
    Ok(canonical_path)
}

/// Rejects an excluded id and requires a materialized header to match the
/// breadcrumb; the path is already resolved and gated by the caller.
fn validate_breadcrumb(
    breadcrumb: Breadcrumb<'_>,
    session_path: &Path,
    materialized_header: Option<(Option<String>, Option<String>)>,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let session_id = super::pi::extract_pi_uuid_from_filename(session_path)
        .context("OMP breadcrumb session filename has no UUID")?;
    if exclusion.contains(&session_id) {
        anyhow::bail!("OMP terminal breadcrumb session is excluded");
    }
    if let Some((header_id, header_cwd)) = materialized_header {
        if header_id.as_deref() != Some(session_id.as_str())
            || header_cwd
                .as_deref()
                .map(super::canonicalize_or_raw)
                .as_ref()
                != Some(&super::canonicalize_or_raw(breadcrumb.cwd))
        {
            anyhow::bail!("OMP session header does not match its terminal breadcrumb");
        }
    } else if breadcrumb.fresh {
        anyhow::bail!("fresh OMP breadcrumb target is not materialized");
    } else {
        anyhow::bail!("OMP breadcrumb target is missing");
    }
    Ok(session_id)
}

fn validate_launch_marker(
    metadata: &OmpCaptureMetadata,
    terminal_id: &str,
    session_path: &str,
) -> Result<bool> {
    if metadata.launch_marker.is_empty() {
        return Ok(false);
    }
    let marker_path = Path::new(&metadata.launch_marker);
    let Some(file) = open_regular_file_no_follow(marker_path)? else {
        anyhow::bail!("OMP launch marker is unavailable");
    };
    let mut content = String::new();
    file.take((MAX_LAUNCH_MARKER_BYTES + 1) as u64)
        .read_to_string(&mut content)
        .context("Failed to read OMP launch marker")?;
    anyhow::ensure!(
        content.len() <= MAX_LAUNCH_MARKER_BYTES,
        "OMP launch marker exceeds its capture limit"
    );
    let mut lines = content.lines();
    let terminal = lines.next();
    let launch = lines.next();
    let pending = lines.next().filter(|value| !value.is_empty());
    let routing_fingerprint = lines.next();
    if terminal != Some(terminal_id)
        || launch != Some(metadata.launch_id.as_str())
        || pending.is_none()
        || routing_fingerprint != Some(metadata.routing_fingerprint.as_str())
        || metadata.routing_fingerprint.is_empty()
        || lines.next().is_some()
    {
        anyhow::bail!("OMP launch marker does not match the active pane generation");
    }
    anyhow::ensure!(
        pending != Some(session_path),
        "OMP breadcrumb still has its pre-launch pending path"
    );
    Ok(true)
}

fn read_host_breadcrumb(root: &Path, terminal_id: &str) -> Result<(String, u64)> {
    let breadcrumb_path = root.join(terminal_id);
    #[cfg(unix)]
    let file = {
        use std::os::fd::AsFd;
        use std::os::unix::fs::OpenOptionsExt;

        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .with_context(|| {
                format!(
                    "Failed to open OMP terminal breadcrumb root {}",
                    root.display()
                )
            })?;
        let fd = nix::fcntl::openat(
            directory.as_fd(),
            terminal_id,
            nix::fcntl::OFlag::O_RDONLY
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_CLOEXEC
                | nix::fcntl::OFlag::O_NONBLOCK,
            nix::sys::stat::Mode::empty(),
        )
        .with_context(|| {
            format!(
                "Failed to open OMP breadcrumb {}",
                breadcrumb_path.display()
            )
        })?;
        std::fs::File::from(fd)
    };
    #[cfg(not(unix))]
    let mut file = {
        let metadata = std::fs::symlink_metadata(&breadcrumb_path).with_context(|| {
            format!(
                "Failed to inspect OMP breadcrumb {}",
                breadcrumb_path.display()
            )
        })?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "OMP breadcrumb is a symlink"
        );
        std::fs::File::open(&breadcrumb_path).with_context(|| {
            format!(
                "Failed to open OMP breadcrumb {}",
                breadcrumb_path.display()
            )
        })?
    };
    let metadata = file.metadata().with_context(|| {
        format!(
            "Failed to inspect OMP breadcrumb {}",
            breadcrumb_path.display()
        )
    })?;
    anyhow::ensure!(metadata.is_file(), "OMP breadcrumb is not a regular file");
    anyhow::ensure!(
        metadata.len() <= MAX_BREADCRUMB_BYTES as u64,
        "OMP breadcrumb exceeds its capture limit"
    );
    let modified_at_ms = metadata
        .modified()
        .context("Failed to read OMP breadcrumb modification time")?
        .duration_since(std::time::UNIX_EPOCH)
        .context("OMP breadcrumb modification time predates UNIX_EPOCH")
        .and_then(|elapsed| {
            u64::try_from(elapsed.as_millis())
                .context("OMP breadcrumb modification time does not fit in u64")
        })?;
    let mut content = String::new();
    file.take((MAX_BREADCRUMB_BYTES + 1) as u64)
        .read_to_string(&mut content)
        .with_context(|| {
            format!(
                "Failed to read OMP breadcrumb {}",
                breadcrumb_path.display()
            )
        })?;
    anyhow::ensure!(
        content.len() <= MAX_BREADCRUMB_BYTES,
        "OMP breadcrumb grew beyond its capture limit"
    );
    Ok((content, modified_at_ms))
}

fn omp_source_observation(
    metadata: &OmpCaptureMetadata,
    sid: String,
    session_path: &Path,
    cwd: &str,
    active: Option<&crate::session::instance::ActiveExecution>,
) -> Result<crate::session::poller::SessionIdObservation> {
    let mut observation = metadata.session_observation(sid);
    let Some(active) = active else {
        return Ok(observation);
    };
    anyhow::ensure!(
        matches!(&active.capture,
        Some(crate::session::instance::CaptureContext::Omp(expected)) if expected == metadata),
        "OMP source metadata differs from the recorded launch"
    );
    anyhow::ensure!(
        active.binding.agent == "omp",
        "OMP source has a different native identity"
    );
    let (filesystem, path, cwd_filesystem, cwd) = if let Some(container) = &active.container {
        let path = container
            .runtime
            .canonical_path(&container.id, session_path)?;
        let cwd = container
            .runtime
            .canonical_path(&container.id, Path::new(cwd))?;
        let (filesystem, path) = container.physical_path(&path);
        let (cwd_filesystem, cwd) = container.physical_path(&cwd);
        (filesystem, path, cwd_filesystem, cwd)
    } else {
        (
            "host".to_owned(),
            ensure_canonical_store(&metadata.layout, session_path)?,
            "host".to_owned(),
            super::canonicalize_or_raw(cwd),
        )
    };
    anyhow::ensure!(
        cwd == active.binding.cwd && cwd_filesystem == active.binding.cwd_filesystem,
        "OMP transcript belongs to another working directory or filesystem"
    );
    let mut source = active.binding.clone();
    source.stores = vec![path
        .parent()
        .context("OMP transcript has no store directory")?
        .to_path_buf()];
    source.filesystem = filesystem;
    observation.execution = Some(active.clone());
    observation.source = Some(source);
    observation.transcript_path = Some(path);
    Ok(observation)
}

fn capture_omp_session_id_from_terminal(
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    terminal_id: &str,
    active: Option<&crate::session::instance::ActiveExecution>,
) -> Result<crate::session::poller::SessionIdObservation> {
    validate_layout(&metadata.layout)?;
    if !valid_omp_terminal_id(terminal_id) {
        anyhow::bail!("Invalid OMP terminal id");
    }
    let (content, modified_at_ms) =
        read_host_breadcrumb(&metadata.layout.terminal_sessions, terminal_id)?;
    let breadcrumb = parse_breadcrumb(&content)?;
    validate_launch_marker(metadata, terminal_id, breadcrumb.session_path)?;
    anyhow::ensure!(
        modified_at_ms > metadata.launched_at_ms,
        "unproven OMP breadcrumb predates the active pane"
    );
    let session_path = lexical_store_session_path(&metadata.layout, &breadcrumb)?;
    let header = if session_path.is_file() {
        ensure_canonical_store(&metadata.layout, &session_path)?;
        Some(
            super::pi::extract_pi_header_fields(&session_path)
                .context("OMP session JSONL has no valid session header")?,
        )
    } else {
        None
    };
    let cwd = breadcrumb.cwd;
    let session_id = validate_breadcrumb(breadcrumb, &session_path, header, exclusion)?;
    omp_source_observation(metadata, session_id, &session_path, cwd, active)
}

pub(crate) fn capture_omp_session_id(
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    tmux_session_name: &str,
    active: Option<&crate::session::instance::ActiveExecution>,
) -> Result<crate::session::poller::SessionIdObservation> {
    let (_, terminal_id) = tty_and_terminal_id_for_tmux(tmux_session_name)?;
    capture_omp_session_id_from_terminal(metadata, exclusion, &terminal_id, active)
}

#[derive(PartialEq)]
struct OmpPollIdentity {
    metadata: OmpCaptureMetadata,
    tty: String,
    terminal_id: String,
}

fn resolve_omp_poll_identity(tmux_session_name: &str) -> Result<OmpPollIdentity> {
    let metadata = load_omp_capture_metadata(tmux_session_name)?;
    let (tty, terminal_id) = tty_and_terminal_id_for_tmux(tmux_session_name)?;
    Ok(OmpPollIdentity {
        metadata,
        tty,
        terminal_id,
    })
}

pub(crate) fn omp_poll_fn(
    instance_id: String,
    extra_excludes: HashSet<crate::session::ConversationBinding>,
    active: Option<crate::session::instance::ActiveExecution>,
) -> impl Fn(&str) -> Option<crate::session::poller::SessionIdObservation> + Send + 'static {
    move |tmux_session_name| {
        let identity = resolve_omp_poll_identity(tmux_session_name)
            .map_err(|error| {
                tracing::debug!(target: "session.capture", "OMP poll identity refresh failed: {}", error)
            })
            .ok()?;
        let captured = capture_omp_session_id_from_terminal(
            &identity.metadata,
            &HashSet::new(),
            &identity.terminal_id,
            active.as_ref(),
        )
        .map_err(|error| {
            tracing::debug!(target: "session.capture", "OMP poll capture failed: {}", error)
        })
        .ok()?;
        let refreshed = resolve_omp_poll_identity(tmux_session_name).ok()?;
        if refreshed != identity {
            return None;
        }
        let exclusion =
            super::compose_exclusion(&instance_id, &extra_excludes, captured.source.as_ref());
        (!exclusion.contains(&captured.sid)).then_some(captured)
    }
}

#[cfg(test)]
mod fixtures {
    use super::*;

    pub(super) fn metadata(root: &Path, launched_at_ms: u64) -> OmpCaptureMetadata {
        OmpCaptureMetadata {
            layout: OmpStoreLayout {
                sessions: root.join("sessions"),
                managed_sessions: root.join("sessions"),
                terminal_sessions: root.join("terminal-sessions"),
                kind: OmpStoreKind::Managed,
            },
            launched_at_ms,
            launch_id: "launch-a".to_string(),
            launch_marker: String::new(),
            routing_fingerprint: "a".repeat(64),
            container_runtime: None,
        }
    }

    pub(super) const ID: &str = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";

    /// Writes a session JSONL with a header for `id` and `cwd` (plus `extra` fields).
    pub(super) fn write_session(path: &Path, id: &str, cwd: &str, extra: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{cwd}\"{extra}}}\n"),
        )
        .unwrap();
    }

    pub(super) fn session_in(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"))
    }

    /// Runs the container script on the host shell against `meta`'s layout.
    #[cfg(unix)]
    pub(super) fn run_container_script(meta: &OmpCaptureMetadata, marker: &Path) -> Vec<u8> {
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            CONTAINER_BREADCRUMB_SCRIPT,
            "aoe-omp-test",
            meta.layout.terminal_sessions.to_str().unwrap(),
            marker.to_str().unwrap(),
            &meta.launch_id,
            meta.layout.sessions.to_str().unwrap(),
            meta.layout.managed_sessions.to_str().unwrap(),
            "managed",
            &meta.routing_fingerprint,
        ]);
        super::super::run_with_timeout_limit(
            command,
            COMMAND_TIMEOUT,
            "container script test",
            MAX_CONTAINER_CAPTURE_BYTES,
        )
        .unwrap()
    }

    pub(super) fn write_breadcrumb(
        metadata: &OmpCaptureMetadata,
        terminal: &str,
        cwd: &Path,
        session: &Path,
        fresh: bool,
    ) -> PathBuf {
        std::fs::create_dir_all(&metadata.layout.terminal_sessions).unwrap();
        let path = metadata.layout.terminal_sessions.join(terminal);
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}",
                cwd.display(),
                session.display(),
                if fresh { "fresh\n" } else { "" }
            ),
        )
        .unwrap();
        path
    }

    pub(super) fn launch_marker(
        metadata: &OmpCaptureMetadata,
        terminal: &str,
        pending: &str,
    ) -> String {
        format!(
            "{terminal}\n{}\n{pending}\n{}\n",
            metadata.launch_id, metadata.routing_fingerprint
        )
    }

    pub(super) fn set_mtime_ms(path: &Path, millis: u64) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(
                    std::time::SystemTime::UNIX_EPOCH + Duration::from_millis(millis),
                ),
            )
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    fn capture_sid(
        metadata: &OmpCaptureMetadata,
        exclusion: &HashSet<String>,
        terminal_id: &str,
    ) -> Result<String> {
        capture_omp_session_id_from_terminal(metadata, exclusion, terminal_id, None)
            .map(|observation| observation.sid)
    }
    #[test]
    fn breadcrumb_extras_accept_known_formats_and_reject_invalid_ones() {
        let accepted = [
            ("/work\n/session.jsonl\n", false),
            ("/work\n/session.jsonl\nfresh\n", true),
            ("/work\n/session.jsonl\ncwdstat 12 34\n", false),
            ("/work\n/session.jsonl\nfresh\ncwdstat 12 34\n", true),
            ("/work\n/session.jsonl\ncwdstat 12 34\nfresh\n", true),
        ];
        for (content, expected_fresh) in accepted {
            let breadcrumb = parse_breadcrumb(content)
                .unwrap_or_else(|error| panic!("expected valid breadcrumb {content:?}: {error:#}"));
            assert_eq!(breadcrumb.cwd, "/work");
            assert_eq!(breadcrumb.session_path, "/session.jsonl");
            assert_eq!(breadcrumb.fresh, expected_fresh, "{content:?}");
        }

        for content in [
            "/work\n/session.jsonl\nunknown\n",
            "/work\n/session.jsonl\ncwdstat 12\n",
            "/work\n/session.jsonl\ncwdstat 12 34 56\n",
            "/work\n/session.jsonl\ncwdstat twelve 34\n",
            "/work\n/session.jsonl\nfresh\nfresh\n",
            "/work\n/session.jsonl\ncwdstat 12 34\ncwdstat 12 34\n",
        ] {
            assert!(parse_breadcrumb(content).is_err(), "{content:?}");
        }
    }

    #[test]
    fn terminal_id_preserves_the_exact_host_tty_identity() {
        for (tty, expected) in [
            ("/dev/pts/41", Some("pts-41")),
            ("/dev/ttys003", Some("ttys003")),
            ("pts/41", None),
            ("/dev/", None),
        ] {
            assert_eq!(omp_terminal_id_from_tty(tty).as_deref(), expected, "{tty}");
        }
    }

    #[test]
    fn custom_launch_accepts_a_later_managed_resume_breadcrumb() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        let home = tmp.path().join("home");
        let (layout, _) = resolve_layout(
            &HashMap::from([("HOME".to_string(), home.display().to_string())]),
            &cwd,
            None,
            &OmpCliCaptureOptions {
                session_dir: Some(tmp.path().join("custom")),
                ..OmpCliCaptureOptions::default()
            },
            |_| false,
        )
        .unwrap();
        assert_eq!(layout.kind, OmpStoreKind::Custom);
        assert_eq!(layout.sessions, tmp.path().join("custom"));
        assert_eq!(layout.managed_sessions, home.join(".omp/agent/sessions"));
        let resumed = session_in(&layout.managed_sessions.join("other-project"), ID);
        write_session(&resumed, ID, cwd.to_str().unwrap(), "");
        let resume_breadcrumb = Breadcrumb {
            cwd: cwd.to_str().unwrap(),
            session_path: resumed.to_str().unwrap(),
            fresh: false,
        };
        let resume_path = lexical_store_session_path(&layout, &resume_breadcrumb).unwrap();
        ensure_canonical_store(&layout, &resume_path).unwrap();
        assert_eq!(
            validate_breadcrumb(
                resume_breadcrumb,
                &resume_path,
                Some((Some(ID.to_string()), Some(cwd.display().to_string()))),
                &HashSet::new(),
            )
            .unwrap(),
            ID
        );
    }

    #[test]
    fn lexical_store_gate_and_exclusion() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 0);
        let cwd = tmp.path().join("project").to_string_lossy().into_owned();
        let bucket = meta.layout.sessions.join("bucket");
        let path = |p: PathBuf| p.to_string_lossy().into_owned();
        let in_store = path(session_in(&bucket, ID));
        let cases = [
            ("in-store jsonl", in_store.clone(), true),
            (
                "out-of-store",
                path(session_in(&tmp.path().join("outside"), ID)),
                false,
            ),
            (
                "nested",
                path(session_in(&bucket.join("nested"), ID)),
                false,
            ),
            ("wrong extension", path(bucket.join("session.txt")), false),
            ("managed relative", "relative.jsonl".to_string(), false),
        ];
        for (label, session_path, expect_ok) in cases {
            let breadcrumb = Breadcrumb {
                cwd: &cwd,
                session_path: &session_path,
                fresh: true,
            };
            assert_eq!(
                lexical_store_session_path(&meta.layout, &breadcrumb).is_ok(),
                expect_ok,
                "{label}"
            );
        }
        let breadcrumb = Breadcrumb {
            cwd: &cwd,
            session_path: &in_store,
            fresh: true,
        };
        let session_path = lexical_store_session_path(&meta.layout, &breadcrumb).unwrap();
        assert!(validate_breadcrumb(
            breadcrumb,
            &session_path,
            None,
            &HashSet::from([ID.to_string()]),
        )
        .is_err());
    }

    #[test]
    fn legacy_breadcrumb_requires_freshness_and_matching_header() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 100_000);
        let historical = tmp.path().join("historical");
        std::fs::create_dir_all(&historical).unwrap();
        let session = session_in(&meta.layout.managed_sessions.join("historical-project"), ID);
        write_session(&session, ID, historical.to_str().unwrap(), "");
        let breadcrumb = write_breadcrumb(&meta, "pts-1", &historical, &session, false);
        let capture = || capture_sid(&meta, &HashSet::new(), "pts-1");
        assert_eq!(capture().unwrap(), ID, "cross-project targets are accepted");
        set_mtime_ms(&breadcrumb, meta.launched_at_ms);
        assert!(capture().is_err());
        set_mtime_ms(&breadcrumb, meta.launched_at_ms + 1);
        assert_eq!(capture().unwrap(), ID);
        write_session(&session, ID, "/wrong", "");
        assert!(capture().is_err());
    }

    #[test]
    fn marked_generation_requires_rewrite_freshness_and_matching_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let mut meta = metadata(tmp.path(), 150_000);
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let session = session_in(&meta.layout.sessions.join("bucket"), ID);
        write_session(&session, ID, cwd.to_str().unwrap(), "");
        let pending = meta
            .layout
            .managed_sessions
            .join(".aoe-pending-launch-a")
            .join("aoe-pending_launch-a.jsonl");
        let crumb = write_breadcrumb(&meta, "pts-1", &cwd, &pending, true);
        let marker = tmp.path().join("launch-marker");
        let write_marker = |content: String| std::fs::write(&marker, content).unwrap();
        write_marker(launch_marker(&meta, "pts-1", &pending.to_string_lossy()));
        meta.launch_marker = marker.to_string_lossy().into_owned();
        set_mtime_ms(&crumb, meta.launched_at_ms + 1);
        let capture = || capture_sid(&meta, &HashSet::new(), "pts-1");
        assert!(
            capture().is_err(),
            "the fresh sentinel is still the pending path"
        );

        write_breadcrumb(&meta, "pts-1", &cwd, &session, false);
        // A stale pre-launch breadcrumb differs from the sentinel too (#3230).
        set_mtime_ms(&crumb, meta.launched_at_ms - 1);
        assert!(
            capture().is_err(),
            "a marker-proven rewrite must still be post-launch"
        );
        set_mtime_ms(&crumb, meta.launched_at_ms + 1);
        assert_eq!(capture().unwrap(), ID);

        let pending = pending.display();
        for (label, content) in [
            ("empty pending", launch_marker(&meta, "pts-1", "")),
            (
                "pending equals target",
                launch_marker(&meta, "pts-1", &session.to_string_lossy()),
            ),
            (
                "other routing snapshot",
                format!("pts-1\nlaunch-a\n{pending}\n{}\n", "b".repeat(64)),
            ),
            (
                "superseded launch",
                format!(
                    "pts-1\nstale-launch\n{pending}\n{}\n",
                    meta.routing_fingerprint
                ),
            ),
        ] {
            write_marker(content);
            assert!(capture().is_err(), "{label}");
        }
    }

    #[test]
    fn fresh_breadcrumb_waits_for_a_materialized_target() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 0);
        let cwd = tmp.path().join("project");
        let session = session_in(&meta.layout.sessions.join("bucket"), ID);
        for (terminal, fresh) in [("fresh", true), ("not-fresh", false)] {
            write_breadcrumb(&meta, terminal, &cwd, &session, fresh);
            assert!(capture_sid(&meta, &HashSet::new(), terminal).is_err());
        }
    }
}
