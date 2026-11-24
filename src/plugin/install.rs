//! Plugin enable/disable and external install / update / uninstall.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use aoe_plugin_api::agent_integration::AgentIntegrationContribution;
use aoe_plugin_api::{BuildStep, PluginId, PluginManifest, RuntimeSpec, UiContribution};
use serde::{Deserialize, Serialize};

use crate::session::{update_config, CapabilityGrant, Config, PluginConfig, StorageFlock};

use super::changelog::UpdateChangelog;
use super::featured::FeaturedIndex;
use super::fetch::{self, FetchedPlugin};
use super::lockfile::{LockedPlugin, Lockfile};
use super::registry::ValidationState;
use super::source::PluginSource;
use super::NodeEnv;

mod publication;

pub enum OperationLog {
    Inherit,
    File(std::fs::File),
}

impl OperationLog {
    pub fn file(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating plugin job log dir {}", parent.display()))?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(path)
            .with_context(|| format!("opening plugin job log {}", path.display()))?;
        Ok(OperationLog::File(file))
    }

    fn line(&self, msg: &str) {
        match self {
            OperationLog::Inherit => eprintln!("{msg}"),
            OperationLog::File(file) => {
                let _ = writeln!(&mut &*file, "{msg}");
            }
        }
    }

    fn child_stdio(&self) -> Result<(Stdio, Stdio)> {
        match self {
            OperationLog::Inherit => Ok((Stdio::inherit(), Stdio::inherit())),
            OperationLog::File(file) => {
                let out = file.try_clone().context("cloning plugin job log handle")?;
                let err = file.try_clone().context("cloning plugin job log handle")?;
                Ok((Stdio::from(out), Stdio::from(err)))
            }
        }
    }
}

struct RecoveryState {
    roots: BTreeSet<PathBuf>,
    unknown_root: bool,
}

static RECOVERY_STATE: parking_lot::Mutex<RecoveryState> = parking_lot::Mutex::new(RecoveryState {
    roots: BTreeSet::new(),
    unknown_root: false,
});

pub(super) fn log_recovery<T>(result: &Result<T>) {
    if let Err(error) = result {
        let marker = match crate::session::get_app_dir() {
            Ok(root) => {
                let marker = root.join(RECOVERY_MARKER);
                RECOVERY_STATE.lock().roots.insert(root);
                Some(marker)
            }
            Err(path_error) => {
                RECOVERY_STATE.lock().unknown_root = true;
                tracing::error!(target: "plugin.install", %path_error, "plugin recovery root unavailable; manual recovery is required before restarting");
                None
            }
        };
        let mut cause = crate::logging::redact(&format!("{error:#}"));
        tracing::error!(target: "plugin.install", error = %cause, "plugin mutation ownership retained for recovery");
        cause.push('\n');
        if let Some(marker) = marker {
            let persist = (|| -> Result<()> {
                let mut file = std::fs::File::create(&marker)?;
                file.write_all(cause.as_bytes())?;
                file.sync_all()?;
                std::fs::File::open(marker.parent().expect("recovery marker parent"))?
                    .sync_all()?;
                Ok(())
            })();
            if let Err(persist_error) = persist {
                tracing::error!(target: "plugin.install", %persist_error, "plugin recovery marker could not be persisted; manual recovery is required before restarting");
            }
        }
    }
}

pub(crate) const OPERATION_LOCK: &str = ".plugin-operation.lock";
pub(crate) const STAGING_MARKER: &str = ".plugin-staging-active";
pub(crate) const RECOVERY_MARKER: &str = ".plugin-recovery-required";
static LIVE_STAGING: parking_lot::Mutex<BTreeSet<PathBuf>> =
    parking_lot::Mutex::new(BTreeSet::new());

#[cfg(test)]
fn register_staging(marker: &Path) {
    LIVE_STAGING.lock().insert(marker.to_path_buf());
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagingEvidence {
    schema: u32,
    owner_pid: u32,
    owner_birth: crate::process::ProcessBirthIdentity,
    #[cfg(target_os = "linux")]
    owner_scope: crate::process::StagingRecoveryScope,
    mount: PathBuf,
    directory_identity: (u64, u64),
    mount_identity: (u64, u64),
    #[serde(default, skip_serializing_if = "Option::is_none")]
    removal: Option<StagingRemovalProof>,
    custody: crate::process::StagingCustody,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagingRemovalProof {
    plugins_identity: (u64, u64),
    plugins_created: std::time::SystemTime,
    directory_created: std::time::SystemTime,
    mount_created: std::time::SystemTime,
}

fn staging_directory_identity(directory: &crate::session::AnchoredDir) -> Result<(u64, u64)> {
    let (device, inode) = directory.identity()?;
    #[cfg(target_os = "macos")]
    let device = device as u64;
    Ok((device, inode))
}

fn staging_evidence(
    root: &crate::session::AnchoredDir,
) -> Result<(StagingEvidence, Vec<crate::process::StagingProcess>, bool)> {
    use std::io::Read as _;
    let file = root
        .open_regular(Path::new(STAGING_MARKER), 1024 * 1024)?
        .context("plugin staging evidence is missing or not a bounded regular file")?;
    crate::process::validate_recovery_file(&file)?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 1024 * 1024,
        "plugin staging journal exceeds its bound"
    );
    anyhow::ensure!(
        bytes.last() == Some(&b'\n'),
        "incomplete plugin staging journal"
    );
    let mut lines = bytes[..bytes.len() - 1].split(|byte| *byte == b'\n');
    let mut evidence: StagingEvidence =
        serde_json::from_slice(lines.next().context("missing plugin staging header")?)
            .context("plugin staging evidence has no complete ownership proof")?;
    anyhow::ensure!(
        evidence.schema == 1,
        "unsupported plugin staging evidence schema"
    );
    anyhow::ensure!(
        evidence.owner_pid > 1
            && evidence.owner_pid <= i32::MAX as u32
            && !evidence.owner_birth.boot_id.is_empty()
            && !evidence.owner_birth.start_id.is_empty(),
        "incomplete staging owner identity"
    );
    uuid::Uuid::parse_str(&evidence.owner_birth.boot_id)
        .context("invalid staging owner boot identity")?;
    #[cfg(target_os = "linux")]
    anyhow::ensure!(
        evidence
            .owner_birth
            .start_id
            .strip_prefix("proc:")
            .is_some_and(|start| start.parse::<u64>().is_ok_and(|start| start > 0)),
        "invalid staging owner birth identity"
    );
    #[cfg(target_os = "macos")]
    anyhow::ensure!(
        evidence
            .owner_birth
            .start_id
            .strip_prefix("audit:")
            .is_some_and(
                |token| token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
            ),
        "invalid staging owner birth identity"
    );
    let mut writers = Vec::new();
    let mut cleanup_started = false;
    for line in lines {
        anyhow::ensure!(!cleanup_started, "records after staging cleanup intent");
        if line == b"D" {
            anyhow::ensure!(evidence.removal.is_some(), "missing staging removal proof");
            cleanup_started = true;
            continue;
        }
        let mut fields = std::str::from_utf8(line)?.split_ascii_whitespace();
        if line == b"U" {
            evidence.custody = crate::process::StagingCustody::Uncertain;
            continue;
        }
        anyhow::ensure!(fields.next() == Some("W"), "unknown staging journal record");
        let pid: u32 = fields
            .next()
            .context("missing staging writer PID")?
            .parse()?;
        let start: u64 = fields
            .next()
            .context("missing staging writer birth")?
            .parse()?;
        anyhow::ensure!(
            pid > 1 && start > 0 && fields.next().is_none(),
            "invalid staging writer identity"
        );
        writers.push(crate::process::StagingProcess {
            pid,
            birth: crate::process::ProcessBirthIdentity {
                boot_id: evidence.owner_birth.boot_id.clone(),
                start_id: format!("proc:{start}"),
            },
        });
    }
    Ok((evidence, writers, cleanup_started))
}

pub(crate) fn record_staging_custody(
    mount: &Path,
    custody: crate::process::StagingCustody,
) -> Result<std::fs::File> {
    let app = mount
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("staging mount has no application root")?;
    let root = crate::session::AnchoredDir::open(app)?;
    let (mut evidence, writers, cleanup_started) = staging_evidence(&root)?;
    anyhow::ensure!(
        writers.is_empty() && !cleanup_started,
        "staging custody cannot replace admitted writers"
    );
    anyhow::ensure!(
        evidence.owner_pid == std::process::id()
            && crate::process::process_birth_identity(evidence.owner_pid)?.as_ref()
                == Some(&evidence.owner_birth)
            && app.join(&evidence.mount) == mount,
        "plugin staging custody does not belong to this process"
    );
    evidence.custody = custody;
    let mut bytes = serde_json::to_vec(&evidence)?;
    bytes.push(b'\n');
    root.bind_file(std::ffi::OsStr::new(STAGING_MARKER))?
        .replace(&bytes)?;
    root.sync()
        .context("persisting staging custody replacement")?;
    open_staging_journal(&root)
}

fn open_staging_journal(root: &crate::session::AnchoredDir) -> Result<std::fs::File> {
    use nix::fcntl::{openat, OFlag};
    use nix::sys::stat::Mode;
    use std::os::unix::fs::MetadataExt as _;
    let fd = openat(
        root.descriptor(),
        STAGING_MARKER,
        OFlag::O_WRONLY
            | OFlag::O_APPEND
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC
            | OFlag::O_NONBLOCK,
        Mode::empty(),
    )?;
    let journal = std::fs::File::from(fd);
    crate::process::validate_recovery_file(&journal)?;
    let observed = root
        .open_regular(Path::new(STAGING_MARKER), 1024 * 1024)?
        .context("staging journal changed before opening")?;
    anyhow::ensure!(
        (journal.metadata()?.dev(), journal.metadata()?.ino())
            == (observed.metadata()?.dev(), observed.metadata()?.ino()),
        "staging journal identity changed"
    );
    Ok(journal)
}

pub(super) fn mark_staging_publication(marker: &Path) -> Result<()> {
    let app = marker
        .parent()
        .context("staging marker has no application root")?;
    let root = crate::session::AnchoredDir::open(app)?;
    let (evidence, _, cleanup_started) = staging_evidence(&root)?;
    anyhow::ensure!(
        !cleanup_started
            && evidence.owner_pid == std::process::id()
            && crate::process::process_birth_identity(evidence.owner_pid)?.as_ref()
                == Some(&evidence.owner_birth),
        "staging publication owner changed"
    );
    let mut journal = open_staging_journal(&root)?;
    journal.write_all(b"U\n")?;
    journal.sync_all()?;
    Ok(())
}

fn recover_staging(root_path: &Path) -> Result<()> {
    let root = crate::session::AnchoredDir::open(root_path)?;
    let marker_identity = root
        .entry_stat(Path::new(STAGING_MARKER))?
        .context("staging marker disappeared before recovery")?;
    let (evidence, writers, cleanup_started) = staging_evidence(&root)?;
    let boot = crate::process::boot_id().context("plugin staging boot identity unavailable")?;
    let previous_boot = boot != evidence.owner_birth.boot_id;
    #[cfg(target_os = "linux")]
    evidence.owner_scope.prove_current(previous_boot)?;
    if let crate::process::StagingCustody::LinuxPidNamespace { holder, init } = &evidence.custody {
        anyhow::ensure!(
            holder.birth.boot_id == evidence.owner_birth.boot_id
                && init.birth.boot_id == evidence.owner_birth.boot_id,
            "staging namespace and owner boot identities differ"
        );
    }
    if !previous_boot {
        match crate::process::process_birth_identity(evidence.owner_pid)? {
            Some(current) if current != evidence.owner_birth => {
                bail!("plugin staging owner PID was reused; native resource inventory is required")
            }
            Some(_) if crate::process::process_executing(evidence.owner_pid)? => {
                bail!("plugin staging owner is still live")
            }
            _ => {}
        }
    }
    crate::process::prove_staging_custody_gone(&evidence.custody, &writers, previous_boot)?;
    let directory = staging_directory_leaf(&evidence.mount)?;
    let check_marker = || -> Result<()> {
        let current = root
            .entry_stat(Path::new(STAGING_MARKER))?
            .context("staging marker disappeared during recovery")?;
        anyhow::ensure!(
            current.st_dev == marker_identity.st_dev
                && current.st_ino == marker_identity.st_ino
                && current.st_mode == marker_identity.st_mode,
            "staging marker identity changed during recovery"
        );
        Ok(())
    };
    let plugins = root.child(Path::new("plugins"))?;
    if let Some(proof) = &evidence.removal {
        root.require_owned_directory()?;
        plugins.require_owned_directory()?;
        anyhow::ensure!(
            staging_directory_identity(&plugins)? == proof.plugins_identity
                && plugins.created() == Some(proof.plugins_created),
            "plugin staging parent identity changed"
        );
    }
    if !cleanup_started || plugins.entry_stat(directory)?.is_some() {
        let stage = plugins.child(directory)?;
        anyhow::ensure!(
            staging_directory_identity(&stage)? == evidence.directory_identity,
            "plugin staging directory identity changed"
        );
        crate::process::prove_staging_host_mount_absent(plugins.descriptor(), stage.descriptor())?;
        if let Some(proof) = &evidence.removal {
            stage.require_owned_directory()?;
            anyhow::ensure!(
                stage.created() == Some(proof.directory_created),
                "plugin staging directory birth changed"
            );
        }

        let volume = Path::new("volume");
        let mount = if !cleanup_started || stage.entry_stat(volume)?.is_some() {
            let mount = stage.child(volume)?;
            anyhow::ensure!(
                staging_directory_identity(&mount)? == evidence.mount_identity
                    && mount.read_dir_complete(Path::new(""), 1)?.is_empty()
                    && stage.read_dir_complete(Path::new(""), 2)?
                        == [std::ffi::OsString::from("volume")],
                "plugin staging mount changed or staging is not empty"
            );
            crate::process::prove_staging_host_mount_absent(
                stage.descriptor(),
                mount.descriptor(),
            )?;
            Some(mount)
        } else {
            None
        };
        if let Some(proof) = &evidence.removal {
            if let Some(mount) = &mount {
                mount.require_owned_directory()?;
                anyhow::ensure!(
                    mount.created() == Some(proof.mount_created),
                    "plugin staging mount birth changed"
                );
            }
            // Persist intent before either rmdir, so a retry need not infer deletion.
            let mut journal = open_staging_journal(&root)?;
            check_marker()?;
            if !cleanup_started {
                journal.write_all(b"D\n")?;
            }
            journal.sync_all()?;
            root.require_current_child(Path::new("plugins"), &plugins)?;
            if let Some(mount) = &mount {
                plugins.require_current_child(directory, &stage)?;
                stage.remove_empty_child(volume, mount)?;
            }
            stage.sync()?;
            plugins.remove_empty_child(directory, &stage)?;
        }
    }
    if evidence.removal.is_some() {
        plugins.sync()?;
    }
    check_marker()?;
    root.remove_file(Path::new(STAGING_MARKER))?;
    root.sync()?;
    Ok(())
}

fn staging_directory_leaf(mount: &Path) -> Result<&Path> {
    use std::path::Component::Normal;

    let mut components = mount.components();
    let (Some(Normal(plugins)), Some(Normal(directory)), Some(Normal(volume)), None) = (
        components.next(),
        components.next(),
        components.next(),
        components.next(),
    ) else {
        bail!("plugin staging evidence names an unexpected mount");
    };
    anyhow::ensure!(
        plugins == "plugins"
            && directory
                .to_str()
                .is_some_and(|name| name.starts_with(".staging-"))
            && volume == "volume",
        "plugin staging evidence names an unexpected mount"
    );
    Ok(Path::new(directory))
}

pub(super) fn create_staging_marker(marker: &Path, mount: &Path) -> Result<()> {
    // Readers hold this same lock while inspecting evidence, so an acquisition
    // cannot briefly appear to belong to a crashed process.
    let mut live = LIVE_STAGING.lock();
    let parent = marker
        .parent()
        .context("plugin staging marker has no parent")?;
    let root = crate::session::AnchoredDir::open(parent)?;
    let relative = mount.strip_prefix(parent)?;
    let directory = staging_directory_leaf(relative)?;
    let plugins = root.child(Path::new("plugins"))?;
    let stage = plugins.child(directory)?;
    let mount_dir = stage.child(Path::new("volume"))?;
    let owned = [&root, &plugins, &stage, &mount_dir]
        .into_iter()
        .all(|dir| dir.require_owned_directory().is_ok());
    let removal = match (plugins.created(), stage.created(), mount_dir.created()) {
        (Some(plugins_created), Some(directory_created), Some(mount_created)) if owned => {
            stage.sync()?;
            plugins.sync()?;
            Some(StagingRemovalProof {
                plugins_identity: staging_directory_identity(&plugins)?,
                plugins_created,
                directory_created,
                mount_created,
            })
        }
        _ => None,
    };
    let evidence_record = StagingEvidence {
        schema: 1,
        owner_pid: std::process::id(),
        owner_birth: crate::process::process_birth_identity(std::process::id())?
            .context("plugin staging owner birth identity unavailable")?,
        #[cfg(target_os = "linux")]
        owner_scope: crate::process::StagingRecoveryScope::capture()?,
        mount: relative.to_path_buf(),
        directory_identity: staging_directory_identity(&stage)?,
        mount_identity: staging_directory_identity(&mount_dir)?,
        removal,
        custody: crate::process::StagingCustody::Prepared,
    };
    let mut evidence = root
        .create_new_regular(Path::new(STAGING_MARKER))?
        .context("plugin staging evidence already exists")?;
    let persist = (|| -> Result<()> {
        serde_json::to_writer(&mut evidence, &evidence_record)?;
        evidence.write_all(b"\n")?;
        evidence
            .sync_all()
            .context("persisting plugin staging marker")?;
        root.sync()?;
        Ok(())
    })();
    if let Err(error) = persist {
        let cleanup = (|| -> Result<()> {
            std::fs::remove_file(marker)?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if let Err(cleanup) = cleanup {
            return Err(super::execution::RecoveryRequired(format!(
                "plugin staging marker publication failed: {error:#}; marker cleanup unproven: {cleanup:#}"
            )).into());
        }
        return Err(error);
    }
    live.insert(marker.to_path_buf());
    Ok(())
}

pub(super) fn unregister_staging(marker: &Path) {
    LIVE_STAGING.lock().remove(marker);
}

pub(crate) fn recover_stale_staging() -> Result<()> {
    let root = crate::session::get_app_dir()?;
    let live = LIVE_STAGING.lock();
    if live.contains(&root.join(STAGING_MARKER)) {
        return Ok(());
    }
    match std::fs::symlink_metadata(root.join(STAGING_MARKER)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspecting plugin staging evidence"),
        Ok(_) => {}
    }
    let _operation = crate::session::try_acquire_storage_flock(&root, OPERATION_LOCK)?
        .context("another plugin operation owns staging recovery")?;
    recover_staging(&root)
}

pub(crate) fn check_recovery_evidence() -> Result<()> {
    recover_stale_staging()?;
    check_recovery_evidence_inner(true)
}

fn check_recovery_evidence_inner(allow_live_staging: bool) -> Result<()> {
    let root = crate::session::get_app_dir()?;
    let required = {
        let state = RECOVERY_STATE.lock();
        state.unknown_root || state.roots.contains(&root)
    };
    if required {
        bail!("plugin recovery is required in this Daemon process; stop the Daemon, verify owned commands have exited and restore the installation before restarting");
    }
    let live = LIVE_STAGING.lock();
    for name in [RECOVERY_MARKER, STAGING_MARKER, publication::MARKER] {
        let evidence = root.join(name);
        if name == STAGING_MARKER && allow_live_staging && live.contains(&evidence) {
            continue;
        }
        match std::fs::symlink_metadata(&evidence) {
            Ok(_) => bail!("plugin recovery evidence exists at {}; stop the Daemon, verify owned commands have exited and restore the installation before removing this file, then restart the Daemon", evidence.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("checking plugin recovery evidence"),
        }
    }
    Ok(())
}

pub(crate) fn recover_stale_publication() -> Result<()> {
    let app = crate::session::get_app_dir()?;
    if !publication::exists(&app)? {
        return Ok(());
    }
    let _operation = crate::session::try_acquire_storage_flock(&app, OPERATION_LOCK)?
        .context("another plugin operation owns publication recovery")?;
    publication::recover(&app)
}

pub(crate) fn begin_operation() -> Result<Arc<StorageFlock>> {
    let root = crate::session::get_app_dir()?;
    #[cfg(test)]
    fetch::test_gate::reach(&root, "operation-admission");
    let lock = crate::session::try_acquire_storage_flock(&root, OPERATION_LOCK)
        .context("acquiring plugin operation lock")?
        .map(Arc::new)
        .ok_or_else(|| {
            anyhow!("another plugin operation is running; inspect its outcome before retrying")
        })?;
    publication::recover(&root).context("recovering interrupted plugin publication")?;
    if !LIVE_STAGING.lock().contains(&root.join(STAGING_MARKER))
        && std::fs::symlink_metadata(root.join(STAGING_MARKER)).is_ok()
    {
        recover_staging(&root).context("recovering abandoned plugin staging")?;
    }
    check_recovery_evidence_inner(false)?;
    Ok(lock)
}

pub fn set_enabled(plugin_id: &str, enabled: bool) -> Result<()> {
    let _operation = begin_operation()?;
    let registry = super::reload_registry();
    if registry.get(plugin_id).is_none() {
        bail!("unknown plugin {plugin_id:?}; see `aoe plugin list`");
    }
    enable_in_config(plugin_id, enabled)?;
    super::reload_registry();
    tracing::info!(target: "plugin.host", plugin = %plugin_id, enabled, "plugin toggled");
    Ok(())
}

#[derive(Debug)]
pub enum LiveToggle {
    Daemon,
    Local,
    LocalDaemonStale { reason: String },
}

pub(crate) const WEB_TOGGLE_RECOVERY: &str =
    "Local transport/404: run `aoe serve --stop`, inspect state, then retry";

pub(crate) fn web_toggle_error(error: anyhow::Error) -> anyhow::Error {
    use crate::acp::client::HttpError;
    use crate::daemon::DaemonClientError;
    let daemon_recoverable = |error: &DaemonClientError| {
        matches!(
            error,
            DaemonClientError::UnixTransport
                | DaemonClientError::Transport
                | DaemonClientError::Timeout
        ) || matches!(error, DaemonClientError::Status { status, .. } if *status == reqwest::StatusCode::NOT_FOUND)
    };
    let recoverable = error.chain().any(|cause| {
        cause
            .downcast_ref::<HttpError>()
            .is_some_and(|error| match error {
                HttpError::Transport => true,
                HttpError::Server { status, .. } => *status == reqwest::StatusCode::NOT_FOUND,
                HttpError::Daemon(error) => daemon_recoverable(error),
                _ => false,
            })
            || cause
                .downcast_ref::<DaemonClientError>()
                .is_some_and(daemon_recoverable)
    });
    if recoverable {
        error.context(WEB_TOGGLE_RECOVERY)
    } else {
        error
    }
}

/// Web RPC errors are terminal: idempotent local writes would bypass daemon
/// authorization and live-worker reconciliation. Other plugins retain local fallback.
pub async fn set_enabled_live(plugin_id: &str, enabled: bool) -> Result<LiveToggle> {
    if super::registry().get(plugin_id).is_none() {
        bail!("unknown plugin {plugin_id:?}; see `aoe plugin list`");
    }
    let endpoint = match crate::acp::client::discovery::discover_local() {
        Ok(endpoint) => endpoint,
        Err(_) => {
            set_enabled(plugin_id, enabled)?;
            return Ok(LiveToggle::Local);
        }
    };
    let daemon_result = async {
        let client = crate::acp::client::HttpClient::new(endpoint)?;
        client.set_plugin_enabled(plugin_id, enabled).await
    }
    .await;
    match daemon_result {
        Ok(()) => {
            super::reload_registry();
            Ok(LiveToggle::Daemon)
        }
        Err(e) if plugin_id == "aoe.web" => Err(web_toggle_error(e.into())),
        Err(e) => {
            set_enabled(plugin_id, enabled)?;
            Ok(LiveToggle::LocalDaemonStale {
                reason: format!("{e}"),
            })
        }
    }
}

#[derive(Debug)]
pub enum LiveRestart {
    Daemon,
    NoDaemon,
    DaemonStale { reason: String },
}

pub async fn restart_worker_live(plugin_id: &str) -> LiveRestart {
    let Ok(endpoint) = crate::acp::client::discovery::discover_local() else {
        return LiveRestart::NoDaemon;
    };
    let result = async {
        let client = crate::acp::client::HttpClient::new(endpoint)?;
        client.restart_plugin_worker(plugin_id).await
    }
    .await;
    match result {
        Ok(()) => LiveRestart::Daemon,
        Err(crate::acp::client::HttpError::ReadOnly) => LiveRestart::NoDaemon,
        Err(e) => LiveRestart::DaemonStale {
            reason: format!("{e}"),
        },
    }
}

fn enable_in_config(plugin_id: &str, enabled: bool) -> Result<()> {
    update_config(|config| {
        config
            .plugins
            .entry(plugin_id.to_string())
            .or_insert_with(PluginConfig::default)
            .enabled = enabled;
    })
}

#[derive(Debug)]
pub struct InstallReport {
    pub id: String,
    pub version: String,
    pub capabilities: Vec<String>,
    pub granted: bool,
    pub validation: ValidationState,
}

fn install_validation(featured_verified: bool, dev: bool, source: &str) -> ValidationState {
    if dev {
        ValidationState::Dev
    } else if featured_verified {
        ValidationState::Featured
    } else if source.starts_with("gh:") {
        ValidationState::Community
    } else {
        ValidationState::Local
    }
}

fn trust_label(featured_verified: bool, dev: bool) -> &'static str {
    if dev {
        "dev"
    } else if featured_verified {
        "featured"
    } else {
        "community"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentMode {
    Interactive,
    AssumeYes,
    CleanOnlyNonInteractive,
}

#[derive(Debug)]
pub enum UpdateOutcome {
    Applied(InstallReport),
    Skipped {
        id: String,
        reason: String,
        fingerprint: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiView {
    pub slot: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConsent {
    pub id: String,
    pub from_version: String,
    pub to_version: String,
    pub prior_capabilities: Vec<String>,
    pub new_capabilities: Vec<String>,
    pub added_capabilities: Vec<String>,
    pub removed_capabilities: Vec<String>,
    pub ui: Vec<UiView>,
    pub build_steps: Vec<String>,
    pub agent_integrations: Vec<AgentIntegrationContribution>,
    pub runtime_change: Option<String>,
    pub trust_downgrade: bool,
    pub fingerprint: String,
    pub stays_active_if_declined: bool,
    pub changelog: UpdateChangelog,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdatePreview {
    NoUpdate {
        fingerprint: String,
        notice: String,
    },
    SafeUpdate {
        to_version: String,
        fingerprint: String,
        changelog: UpdateChangelog,
        notice: String,
    },
    ConsentRequired {
        consent: Box<UpdateConsent>,
        dismissed: bool,
        notice: String,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("{id} is already installed; run `aoe plugin update {id}` or uninstall it first")]
pub(crate) struct PluginAlreadyInstalled {
    id: String,
}

struct PreparedInstall {
    persisted_source: String,
    unverified: bool,
    notice: String,
    fetched: FetchedPlugin,
    featured_verified: bool,
    dev: bool,
    id: String,
    capabilities: Vec<String>,
    manifest_hash: String,
    fingerprint: String,
    validation: ValidationState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallConsent {
    pub id: String,
    pub version: String,
    pub source: String,
    pub notice: String,
    pub unverified: bool,
    pub validation: String,
    pub capabilities: Vec<String>,
    pub ui: Vec<UiView>,
    pub build_steps: Vec<String>,
    pub agent_integrations: Vec<AgentIntegrationContribution>,
    pub fingerprint: String,
}

async fn prepare_install(
    input: &str,
    dev: bool,
    operation: Option<Arc<StorageFlock>>,
) -> Result<PreparedInstall> {
    let source = PluginSource::parse(input)?;
    if dev && !matches!(source, PluginSource::Local(_)) {
        bail!("--dev requires a local plugin directory");
    }
    let resolved = resolve_source(source, true).await?;
    let mut staged = Some(fetch::fetch(&resolved.source, operation).await?);
    let result = (|| {
        let fetched = staged.as_ref().unwrap();

        let id = fetched.manifest.id.as_str().to_string();
        let featured_verified = !dev && verify_featured(&FeaturedIndex::load()?, fetched)?;
        reject_reserved_or_builtin(&fetched.manifest, featured_verified || dev)?;
        reject_incompatible_host(&fetched.manifest)?;

        if super::plugins_dir()?.join(&id).exists() {
            return Err(PluginAlreadyInstalled { id }.into());
        }

        let capabilities = capability_strings(fetched)?;
        let manifest_hash = PluginManifest::hash_bytes(&fetched.manifest_bytes);
        let trust = trust_label(featured_verified, dev);
        let fingerprint = fingerprint(&fetched.tree_hash, fetched.asset_sha256.as_deref(), trust);
        let persisted_source = persisted_source(&resolved.source, input);
        let validation = install_validation(featured_verified, dev, &persisted_source);

        Ok(PreparedInstall {
            persisted_source,
            unverified: resolved.unverified,
            notice: resolved.notice,
            fetched: staged.take().unwrap(),
            featured_verified,
            dev,
            id,
            capabilities,
            manifest_hash,
            fingerprint,
            validation,
        })
    })();
    if let Some(fetched) = staged {
        fetched.cleanup().await?;
    }
    result
}

#[derive(Serialize)]
struct PriorMetadata {
    config: Option<PluginConfig>,
    locked: Option<LockedPlugin>,
}

impl PriorMetadata {
    fn capture(id: &str) -> Result<Self> {
        Ok(Self {
            config: Config::load()?.plugins.get(id).cloned(),
            locked: Lockfile::load()?.get(id).cloned(),
        })
    }

    fn restore(&self, id: &str) -> Result<()> {
        let config_result = (|| -> Result<()> {
            let current = Config::load()?.plugins.remove(id);
            if current != self.config {
                update_config(|config| match &self.config {
                    Some(prior) => {
                        config.plugins.insert(id.to_owned(), prior.clone());
                    }
                    None => {
                        config.plugins.remove(id);
                    }
                })?;
            }
            Ok(())
        })();
        let lock_result = (|| -> Result<()> {
            let mut lock = Lockfile::load()?;
            if lock.get(id) != self.locked.as_ref() {
                match &self.locked {
                    Some(prior) => lock.upsert(id, prior.clone()),
                    None => {
                        lock.remove(id);
                    }
                }
                lock.save()?;
            }
            Ok(())
        })();
        match (config_result, lock_result) {
            (Ok(()), Ok(())) => Ok(()),
            (config, lock) => {
                bail!("target config restoration: {config:?}; target lock restoration: {lock:?}")
            }
        }
    }
}

fn apply_prepared_install(
    p: &PreparedInstall,
    node_env: &NodeEnv,
    log: &OperationLog,
) -> Result<InstallReport> {
    let final_dir = super::plugins_dir()?.join(&p.id);
    let prior = PriorMetadata::capture(&p.id)?;
    if final_dir.exists() {
        bail!(
            "{} is already installed; run `aoe plugin update {}` or uninstall it first",
            p.id,
            p.id
        );
    }

    log.line(&format!(
        "installing {} {}",
        p.id, p.fetched.manifest.version
    ));
    if p.fetched.journals_publication() {
        with_publication_transaction(&p.fetched, &final_dir, &prior, false, |tree, journal| {
            place_publication_tree(journal.as_deref(), tree, &final_dir)?;
            settle_install(p, &final_dir, node_env, log, &prior, journal)
        })?;
    } else {
        move_into_place(&p.fetched, &final_dir)?;
        settle_install(p, &final_dir, node_env, log, &prior, None)?;
    }
    super::reload_registry();
    log.line(&format!(
        "installed {} {}",
        p.id, p.fetched.manifest.version
    ));
    tracing::info!(target: "plugin.install", plugin = %p.id, version = %p.fetched.manifest.version, "plugin installed");

    Ok(InstallReport {
        id: p.id.clone(),
        version: p.fetched.manifest.version.clone(),
        capabilities: p.capabilities.clone(),
        granted: true,
        validation: p.validation,
    })
}

fn settle_install(
    p: &PreparedInstall,
    final_dir: &Path,
    node_env: &NodeEnv,
    log: &OperationLog,
    prior: &PriorMetadata,
    mut journal: Option<&mut publication::Journal>,
) -> Result<()> {
    before_publication_build(&mut journal, final_dir, &p.fetched.manifest)?;
    if let Err(e) = build_in_place(
        &p.id,
        final_dir,
        &p.fetched.manifest,
        node_env,
        log,
        p.fetched.budget,
    ) {
        if e.downcast_ref::<super::execution::RecoveryRequired>()
            .is_some()
        {
            return Err(e);
        }
        remove_published_tree(journal.as_deref(), final_dir).map_err(|error| {
            super::execution::RecoveryRequired(format!(
                "removing failed install {}: {error}; original failure: {e:#}",
                final_dir.display()
            ))
        })?;
        return Err(e);
    }

    #[cfg(test)]
    fetch::test_gate::reach(final_dir, "metadata-commit");
    publication_working(&mut journal, final_dir)?;
    let persisted = (|| -> Result<()> {
        persist_install(
            &p.persisted_source,
            &p.id,
            &p.capabilities,
            &p.manifest_hash,
            p.dev,
        )?;
        write_lock(
            &p.id,
            &p.fetched,
            &p.manifest_hash,
            trust_label(p.featured_verified, p.dev),
        )
    })();
    if let Err(e) = persisted {
        #[cfg(test)]
        fetch::test_gate::reach(final_dir, "rollback");
        let metadata = prior.restore(&p.id);
        let tree = remove_published_tree(journal.as_deref(), final_dir);
        if metadata.is_err() || tree.is_err() {
            return Err(super::execution::RecoveryRequired(format!(
                "install {} failed: {e:#}; metadata restoration: {metadata:?}; tree restoration: {tree:?}", p.id
            )).into());
        }
        return Err(e);
    }
    publication_committed(journal, final_dir)
}

pub async fn install(input: &str, assume_yes: bool, dev: bool) -> Result<InstallReport> {
    let operation = begin_operation()?;
    let prepared = prepare_install(input, dev, Some(operation.clone())).await?;
    eprintln!("{}", prepared.notice);
    let approval = (|| -> Result<()> {
        if prepared.unverified && !assume_yes && !confirm_unverified()? {
            bail!("install cancelled; the unverified source was not approved");
        }
        let build = build_steps(&prepared.fetched.manifest);
        let granted = if assume_yes
            || !install_needs_consent(
                &prepared.capabilities,
                build,
                &prepared.fetched.manifest.ui,
                &prepared.fetched.manifest.agent_integrations,
            ) {
            true
        } else {
            confirm_capabilities(
                &prepared.id,
                &prepared.capabilities,
                &prepared.fetched.manifest.ui,
                build,
                &prepared.fetched.manifest.agent_integrations,
            )?
        };
        if !granted {
            bail!("install cancelled; no capabilities were granted");
        }
        Ok(())
    })();
    if let Err(error) = approval {
        prepared.fetched.cleanup().await?;
        return Err(error);
    }
    let node_env = apply_node_env(&prepared.fetched.manifest).await;
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let result = super::execution::catch_panic(|| {
            apply_prepared_install(&prepared, &node_env, &OperationLog::Inherit)
        });
        if super::execution::needs_recovery(&result) {
            log_recovery(&result);
            std::mem::forget(prepared);
            std::mem::forget(_operation);
        } else {
            prepared.fetched.close()?;
        }
        result
    })
    .await
    .context("plugin install task failed")?
}

pub async fn preview_install(input: &str, dev: bool) -> Result<InstallConsent> {
    let operation = begin_operation()?;
    let p = prepare_install(input, dev, Some(operation)).await?;
    let consent = InstallConsent {
        id: p.id,
        version: p.fetched.manifest.version.clone(),
        source: p.persisted_source,
        notice: p.notice,
        unverified: p.unverified,
        validation: p.validation.as_str().to_string(),
        capabilities: p.capabilities,
        ui: p
            .fetched
            .manifest
            .ui
            .iter()
            .map(|u| UiView {
                slot: u.slot.as_str().to_string(),
                id: u.id.clone(),
            })
            .collect(),
        build_steps: build_steps(&p.fetched.manifest)
            .iter()
            .map(|s| s.command.join(" "))
            .collect(),
        agent_integrations: disclosure_integrations(&p.fetched.manifest.agent_integrations),
        fingerprint: p.fingerprint,
    };
    p.fetched.cleanup().await?;
    Ok(consent)
}

pub async fn apply_install(
    input: &str,
    expected_fingerprint: &str,
    dev: bool,
    log: OperationLog,
) -> Result<InstallReport> {
    let operation = begin_operation()?;
    let prepared = prepare_install(input, dev, Some(operation.clone())).await?;
    if prepared.fingerprint != expected_fingerprint {
        prepared.fetched.cleanup().await?;
        bail!(
            "the plugin at {input} changed since it was shown; review it again before installing"
        );
    }
    let node_env = apply_node_env(&prepared.fetched.manifest).await;
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let result =
            super::execution::catch_panic(|| apply_prepared_install(&prepared, &node_env, &log));
        if super::execution::needs_recovery(&result) {
            log_recovery(&result);
            std::mem::forget(prepared);
            std::mem::forget(_operation);
        } else {
            prepared.fetched.close()?;
        }
        result
    })
    .await
    .context("plugin install task failed")?
}

pub async fn update(id: &str, assume_yes: bool) -> Result<InstallReport> {
    let mode = if assume_yes {
        ConsentMode::AssumeYes
    } else {
        ConsentMode::Interactive
    };
    match update_with_consent(id, mode).await? {
        UpdateOutcome::Applied(report) => Ok(report),
        UpdateOutcome::Skipped { id, reason, .. } => {
            bail!("update for {id} was skipped unexpectedly: {reason}")
        }
    }
}

pub async fn update_clean(id: &str) -> Result<UpdateOutcome> {
    update_with_consent(id, ConsentMode::CleanOnlyNonInteractive).await
}

struct Prepared {
    id: String,
    source_str: String,
    notice: String,
    fetched: FetchedPlugin,
    featured_verified: bool,
    dev: bool,
    prior_grant: Option<CapabilityGrant>,
    capabilities: Vec<String>,
    manifest_hash: String,
    fingerprint: String,
    prior_fingerprint: Option<String>,
    prior_requested_ref: Option<String>,
    prior_resolved_commit: Option<String>,
    target_requested_ref: Option<String>,
    target_resolved_commit: Option<String>,
    from_version: String,
    caps_changed: bool,
    added_capabilities: Vec<String>,
    removed_capabilities: Vec<String>,
    build_changed: bool,
    ui_changed: bool,
    agents_changed: bool,
    runtime_change: Option<String>,
    trust_downgrade: bool,
    needs_consent: bool,
    installed_matches_source: bool,
    stays_active_if_declined: bool,
}

/// The consent structures serialize to the CLI/TUI/web disclosures, so their
/// agent commands carry the display-escaped form rather than the raw bytes.
/// The runnable command in the manifest is never altered.
fn disclosure_integrations(
    integrations: &[AgentIntegrationContribution],
) -> Vec<AgentIntegrationContribution> {
    integrations
        .iter()
        .map(|integration| {
            let mut out = integration.clone();
            if let Some(command) = &out.command {
                out.command = Some(super::agent_integration::display_command(command));
            }
            out
        })
        .collect()
}

fn fingerprint(tree_hash: &str, asset_sha256: Option<&str>, trust: &str) -> String {
    format!("{tree_hash}|{}|{trust}", asset_sha256.unwrap_or(""))
}

fn installed_manifest(id: &str) -> Option<(PluginManifest, String)> {
    let bytes = std::fs::read(super::plugins_dir().ok()?.join(id).join("aoe-plugin.toml")).ok()?;
    let hash = PluginManifest::hash_bytes(&bytes);
    let manifest = PluginManifest::from_toml_str(std::str::from_utf8(&bytes).ok()?).ok()?;
    Some((manifest, hash))
}

fn agent_integrations_changed(
    previous: Option<&[AgentIntegrationContribution]>,
    current: &[AgentIntegrationContribution],
) -> bool {
    let Some(previous) = previous else {
        return !current.is_empty();
    };
    current.iter().any(|current| {
        previous
            .iter()
            .find(|previous| previous.tool == current.tool)
            .is_none_or(|previous| previous != current)
    })
}

async fn prepare_update(
    id: &str,
    allow_local_source: bool,
    operation: Option<Arc<StorageFlock>>,
) -> Result<Prepared> {
    let config = Config::load()?;
    let plugin_config = config
        .plugins
        .get(id)
        .ok_or_else(|| anyhow!("{id} is not installed; see `aoe plugin list`"))?;
    let source_str = plugin_config
        .source
        .clone()
        .ok_or_else(|| anyhow!("{id} is a builtin plugin; there is nothing to update"))?;
    let prior_grant = plugin_config.grant.clone();
    let dev = plugin_config.dev;

    let source = PluginSource::parse(&source_str)?;
    if !allow_local_source && (dev || matches!(source, PluginSource::Local(_))) {
        bail!("local plugin directories and --dev require a verified local Unix socket peer");
    }
    let resolved = resolve_source(source, false).await?;
    let mut staged = Some(fetch::fetch(&resolved.source, operation).await?);
    let result = (|| {
        let fetched = staged.as_ref().unwrap();
        if fetched.manifest.id.as_str() != id {
            bail!(
                "source {source_str:?} now resolves to plugin {:?}, not {id}",
                fetched.manifest.id.as_str()
            );
        }
        let featured_verified = !dev && verify_featured(&FeaturedIndex::load()?, fetched)?;
        reject_reserved_or_builtin(&fetched.manifest, featured_verified || dev)?;
        reject_incompatible_host(&fetched.manifest)?;

        let capabilities = capability_strings(fetched)?;
        let manifest_hash = PluginManifest::hash_bytes(&fetched.manifest_bytes);
        let installed = prior_grant.as_ref().and_then(|_| installed_manifest(id));
        let grant_valid = installed.as_ref().is_some_and(|(manifest, hash)| {
            prior_grant
                .as_ref()
                .is_some_and(|grant| super::registry::grant_covers(grant, manifest, hash))
        });
        let prior_grant = prior_grant.filter(|_| grant_valid);
        let installed_matches_source = grant_valid
            && installed
                .as_ref()
                .is_some_and(|(_, hash)| hash == &manifest_hash);
        let previous_agent_integrations = installed
            .as_ref()
            .filter(|(_, hash)| {
                prior_grant
                    .as_ref()
                    .is_some_and(|grant| &grant.manifest_hash == hash)
            })
            .map(|(manifest, _)| manifest.agent_integrations.as_slice());
        let stays_active_if_declined = grant_valid
            && plugin_config.enabled
            && super::registry()
                .get(id)
                .is_some_and(|plugin| plugin.active());

        let lock = Lockfile::load()?;
        let prior_locked = lock.get(id);
        let prior_tree_hash = prior_locked
            .map(|l| l.tree_hash.clone())
            .unwrap_or_default();
        let prior_was_release_binary = prior_locked.is_some_and(|l| l.asset_sha256.is_some());
        let prior_trust = prior_locked.map(|l| l.trust.clone()).unwrap_or_default();
        let from_version = prior_locked
            .map(|l| l.version.clone())
            .unwrap_or_else(|| "?".to_string());
        let prior_requested_ref = prior_locked.and_then(|l| l.requested_ref.clone());
        let prior_resolved_commit = prior_locked.and_then(|l| l.resolved_commit.clone());
        let target_requested_ref = fetched.requested_ref.clone();
        let target_resolved_commit = fetched.resolved_commit.clone();

        let trust = trust_label(featured_verified, dev);
        let prior_fingerprint =
            prior_locked.map(|l| fingerprint(&l.tree_hash, l.asset_sha256.as_deref(), &l.trust));
        let fingerprint = fingerprint(&fetched.tree_hash, fetched.asset_sha256.as_deref(), trust);

        let prior_caps: BTreeSet<&str> = prior_grant
            .as_ref()
            .map(|g| g.capabilities.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let new_caps: BTreeSet<&str> = capabilities.iter().map(String::as_str).collect();
        let caps_changed = prior_caps != new_caps;
        let added_capabilities: Vec<String> = new_caps
            .difference(&prior_caps)
            .map(|s| s.to_string())
            .collect();
        let removed_capabilities: Vec<String> = prior_caps
            .difference(&new_caps)
            .map(|s| s.to_string())
            .collect();

        let manifest_changed =
            prior_grant.as_ref().map(|g| g.manifest_hash.as_str()) != Some(manifest_hash.as_str());
        let tree_changed = if prior_tree_hash.is_empty() {
            manifest_changed
        } else {
            prior_tree_hash != fetched.tree_hash
        };
        let build_changed = tree_changed && !build_steps(&fetched.manifest).is_empty();
        let ui_changed = manifest_changed && !fetched.manifest.ui.is_empty();
        let agents_changed = agent_integrations_changed(
            previous_agent_integrations,
            &fetched.manifest.agent_integrations,
        );
        let new_is_release_binary = matches!(
            fetched.manifest.runtime,
            Some(RuntimeSpec::ReleaseBinary { .. })
        );
        let runtime_change = if new_is_release_binary && !prior_was_release_binary {
            Some(
                "the worker uses a prebuilt release binary (opaque, not source-auditable)"
                    .to_string(),
            )
        } else if prior_was_release_binary && !new_is_release_binary {
            Some(
                "the worker is now an in-tree command (previously a prebuilt release binary)"
                    .to_string(),
            )
        } else {
            None
        };
        let trust_downgrade = prior_trust == "featured" && !featured_verified;

        let needs_consent = (!capabilities.is_empty() && caps_changed)
            || build_changed
            || ui_changed
            || agents_changed
            || runtime_change.is_some()
            || trust_downgrade
            || !grant_valid;

        Ok(Prepared {
            id: id.to_string(),
            source_str,
            notice: resolved.notice,
            fetched: staged.take().unwrap(),
            featured_verified,
            dev,
            prior_grant,
            capabilities,
            manifest_hash,
            fingerprint,
            prior_fingerprint,
            prior_requested_ref,
            prior_resolved_commit,
            target_requested_ref,
            target_resolved_commit,
            from_version,
            caps_changed,
            added_capabilities,
            removed_capabilities,
            build_changed,
            ui_changed,
            agents_changed,
            runtime_change,
            trust_downgrade,
            needs_consent,
            installed_matches_source,
            stays_active_if_declined,
        })
    })();
    if let Some(fetched) = staged {
        fetched.cleanup().await?;
    }
    result
}

fn ensure_approved(id: &str, needs_consent: bool, grant: Option<&CapabilityGrant>) -> Result<()> {
    if needs_consent && grant.is_none() {
        bail!("update cancelled for {id}; the previously trusted version was kept");
    }
    Ok(())
}

fn apply_prepared(
    prepared: &Prepared,
    grant: Option<CapabilityGrant>,
    node_env: &NodeEnv,
    log: &OperationLog,
) -> Result<InstallReport> {
    let id = prepared.id.as_str();
    let final_dir = super::plugins_dir()?.join(id);
    ensure_approved(id, prepared.needs_consent, grant.as_ref())?;
    log.line(&format!(
        "updating {id} to {}",
        prepared.fetched.manifest.version
    ));
    let prior = PriorMetadata::capture(id)?;
    let granted = grant.is_some();
    replace_and_build(
        id,
        &prepared.fetched,
        &final_dir,
        node_env,
        log,
        || {
            persist_update(id, &prepared.source_str, grant)?;
            write_lock(
                id,
                &prepared.fetched,
                &prepared.manifest_hash,
                trust_label(prepared.featured_verified, prepared.dev),
            )
        },
        &prior,
    )?;
    super::reload_registry();
    tracing::info!(target: "plugin.install", plugin = %id, version = %prepared.fetched.manifest.version, "plugin updated");

    if prepared.caps_changed && !granted {
        eprintln!(
            "{id} updated but its capability set changed; it stays inactive until you re-approve with `aoe plugin update {id}`."
        );
    }

    Ok(InstallReport {
        id: id.to_string(),
        version: prepared.fetched.manifest.version.clone(),
        capabilities: prepared.capabilities.clone(),
        granted,
        validation: install_validation(
            prepared.featured_verified,
            prepared.dev,
            &prepared.source_str,
        ),
    })
}

async fn update_with_consent(id: &str, mode: ConsentMode) -> Result<UpdateOutcome> {
    let operation = begin_operation()?;
    let prepared = prepare_update(id, true, Some(operation.clone())).await?;
    if mode != ConsentMode::CleanOnlyNonInteractive {
        eprintln!("{}", prepared.notice);
    }
    if prepared.needs_consent && mode == ConsentMode::CleanOnlyNonInteractive {
        let skipped = UpdateOutcome::Skipped {
            id: id.to_string(),
            reason: skip_reason(&prepared),
            fingerprint: prepared.fingerprint.clone(),
        };
        prepared.fetched.cleanup().await?;
        return Ok(skipped);
    }

    let grant = (|| -> Result<Option<CapabilityGrant>> {
        Ok(if prepared.needs_consent {
            if mode == ConsentMode::AssumeYes
                || confirm_capabilities(
                    id,
                    &prepared.capabilities,
                    &prepared.fetched.manifest.ui,
                    build_steps(&prepared.fetched.manifest),
                    &prepared.fetched.manifest.agent_integrations,
                )?
            {
                Some(CapabilityGrant {
                    manifest_hash: prepared.manifest_hash.clone(),
                    capabilities: prepared.capabilities.clone(),
                    granted_at: crate::time::Timestamp::now(),
                })
            } else {
                None
            }
        } else if prepared.capabilities.is_empty() {
            Some(CapabilityGrant {
                manifest_hash: prepared.manifest_hash.clone(),
                capabilities: vec![],
                granted_at: crate::time::Timestamp::now(),
            })
        } else {
            prepared.prior_grant.clone().map(|g| CapabilityGrant {
                manifest_hash: prepared.manifest_hash.clone(),
                capabilities: g.capabilities,
                granted_at: g.granted_at,
            })
        })
    })();
    let grant = match grant {
        Ok(grant) => grant,
        Err(error) => {
            prepared.fetched.cleanup().await?;
            return Err(error);
        }
    };

    finish_update(prepared, grant, operation, |manifest| async move {
        apply_node_env(&manifest).await
    })
    .await
}

/// Refuses an unapproved update before `provision` can fetch Node or any
/// build runs.
async fn finish_update<F, Fut>(
    prepared: Prepared,
    grant: Option<CapabilityGrant>,
    operation: Arc<StorageFlock>,
    provision: F,
) -> Result<UpdateOutcome>
where
    F: FnOnce(PluginManifest) -> Fut,
    Fut: std::future::Future<Output = NodeEnv>,
{
    if let Err(error) = ensure_approved(&prepared.id, prepared.needs_consent, grant.as_ref()) {
        prepared.fetched.cleanup().await?;
        return Err(error);
    }
    let node_env = provision(prepared.fetched.manifest.clone()).await;
    let report = tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let result = super::execution::catch_panic(|| {
            apply_prepared(&prepared, grant, &node_env, &OperationLog::Inherit)
        });
        if super::execution::needs_recovery(&result) {
            log_recovery(&result);
            std::mem::forget(prepared);
            std::mem::forget(_operation);
        } else {
            prepared.fetched.close()?;
        }
        result
    })
    .await
    .context("plugin update task failed")??;
    Ok(UpdateOutcome::Applied(report))
}

fn consent_of(p: &Prepared, changelog: UpdateChangelog) -> UpdateConsent {
    UpdateConsent {
        id: p.id.clone(),
        from_version: p.from_version.clone(),
        to_version: p.fetched.manifest.version.clone(),
        prior_capabilities: p
            .prior_grant
            .as_ref()
            .map(|g| g.capabilities.clone())
            .unwrap_or_default(),
        new_capabilities: p.capabilities.clone(),
        added_capabilities: p.added_capabilities.clone(),
        removed_capabilities: p.removed_capabilities.clone(),
        ui: p
            .fetched
            .manifest
            .ui
            .iter()
            .map(|u| UiView {
                slot: u.slot.as_str().to_string(),
                id: u.id.clone(),
            })
            .collect(),
        build_steps: build_steps(&p.fetched.manifest)
            .iter()
            .map(|s| s.command.join(" "))
            .collect(),
        agent_integrations: disclosure_integrations(&p.fetched.manifest.agent_integrations),
        runtime_change: p.runtime_change.clone(),
        trust_downgrade: p.trust_downgrade,
        fingerprint: p.fingerprint.clone(),
        stays_active_if_declined: p.stays_active_if_declined,
        changelog,
    }
}

async fn changelog_of(p: &Prepared) -> UpdateChangelog {
    let source = match PluginSource::parse(&p.source_str) {
        Ok(s) => s,
        Err(_) => return UpdateChangelog::unavailable("Changelog unavailable."),
    };
    super::changelog::build(
        &source,
        p.prior_requested_ref.as_deref(),
        p.prior_resolved_commit.as_deref(),
        p.target_requested_ref.as_deref(),
        p.target_resolved_commit.as_deref(),
    )
    .await
}

pub async fn preview_update(id: &str, allow_local_source: bool) -> Result<UpdatePreview> {
    let operation = begin_operation()?;
    let prepared = prepare_update(id, allow_local_source, Some(operation)).await?;
    let preview = if prepared.prior_fingerprint.as_ref() == Some(&prepared.fingerprint)
        && prepared.installed_matches_source
    {
        UpdatePreview::NoUpdate {
            fingerprint: prepared.fingerprint,
            notice: prepared.notice,
        }
    } else {
        let changelog = changelog_of(&prepared).await;
        if !prepared.needs_consent {
            UpdatePreview::SafeUpdate {
                to_version: prepared.fetched.manifest.version.clone(),
                fingerprint: prepared.fingerprint,
                changelog,
                notice: prepared.notice,
            }
        } else {
            let dismissed = Config::load()
                .ok()
                .and_then(|c| c.plugins.get(id).and_then(|p| p.dismissed_update.clone()))
                .as_deref()
                == Some(prepared.fingerprint.as_str());
            UpdatePreview::ConsentRequired {
                consent: Box::new(consent_of(&prepared, changelog)),
                dismissed,
                notice: prepared.notice,
            }
        }
    };
    prepared.fetched.cleanup().await?;
    Ok(preview)
}

pub async fn apply_update(
    id: &str,
    expected_fingerprint: Option<String>,
    allow_local_source: bool,
    log: OperationLog,
) -> Result<InstallReport> {
    let operation = begin_operation()?;
    let prepared = prepare_update(id, allow_local_source, Some(operation.clone())).await?;
    match &expected_fingerprint {
        Some(expected) if *expected != prepared.fingerprint => {
            prepared.fetched.cleanup().await?;
            bail!(
                "the available update for {id} changed since it was shown; review it again before approving"
            );
        }
        None if prepared.needs_consent => {
            prepared.fetched.cleanup().await?;
            bail!("approving the update for {id} requires the fingerprint it was previewed with");
        }
        _ => {}
    }
    let grant = Some(CapabilityGrant {
        manifest_hash: prepared.manifest_hash.clone(),
        capabilities: prepared.capabilities.clone(),
        granted_at: crate::time::Timestamp::now(),
    });
    let node_env = apply_node_env(&prepared.fetched.manifest).await;
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let result =
            super::execution::catch_panic(|| apply_prepared(&prepared, grant, &node_env, &log));
        if super::execution::needs_recovery(&result) {
            log_recovery(&result);
            std::mem::forget(prepared);
            std::mem::forget(_operation);
        } else {
            prepared.fetched.close()?;
        }
        result
    })
    .await
    .context("plugin update task failed")?
}

#[derive(Debug, Clone, Serialize)]
pub struct ReapproveConsent {
    pub id: String,
    pub version: String,
    pub validation: String,
    pub capabilities: Vec<String>,
    pub ui: Vec<UiView>,
    pub agent_integrations: Vec<AgentIntegrationContribution>,
    pub manifest_hash: String,
}

pub fn reapprove_consent(id: &str) -> Result<ReapproveConsent> {
    let registry = super::registry();
    let plugin = registry
        .get(id)
        .ok_or_else(|| anyhow!("unknown plugin {id:?}; see `aoe plugin list`"))?;
    if plugin.builtin() {
        bail!("{id} is a builtin plugin; it is always granted");
    }
    let unknown: Vec<&str> = plugin
        .manifest
        .capabilities
        .iter()
        .filter(|c| !c.is_known())
        .map(|c| c.as_str())
        .collect();
    if !unknown.is_empty() {
        bail!(
            "plugin requests capabilities this host does not support: {}; upgrade aoe",
            unknown.join(", ")
        );
    }
    Ok(ReapproveConsent {
        id: id.to_string(),
        version: plugin.manifest.version.clone(),
        validation: plugin.validation.as_str().to_string(),
        capabilities: plugin
            .manifest
            .capabilities
            .iter()
            .map(|c| c.as_str().to_string())
            .collect(),
        ui: plugin
            .manifest
            .ui
            .iter()
            .map(|u| UiView {
                slot: u.slot.as_str().to_string(),
                id: u.id.clone(),
            })
            .collect(),
        agent_integrations: disclosure_integrations(&plugin.manifest.agent_integrations),
        manifest_hash: plugin.manifest_hash.clone(),
    })
}

pub fn approve_installed(id: &str, expected_manifest_hash: &str) -> Result<()> {
    let _operation = begin_operation()?;
    super::reload_registry();
    let consent = reapprove_consent(id)?;
    if consent.manifest_hash != expected_manifest_hash {
        bail!("{id} changed on disk since its disclosure was shown; review it again");
    }
    update_config(|config| {
        let Some(entry) = config.plugins.get_mut(id) else {
            bail!("{id} is not an installed external plugin");
        };
        if entry.source.is_none() {
            bail!("{id} is not an installed external plugin");
        }
        entry.grant = Some(CapabilityGrant {
            manifest_hash: consent.manifest_hash.clone(),
            capabilities: consent.capabilities.clone(),
            granted_at: crate::time::Timestamp::now(),
        });
        Ok(())
    })??;
    super::reload_registry();
    Ok(())
}

pub fn nudge_daemon_enabled(changes: Vec<(String, bool)>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    static IN_FLIGHT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    if changes.is_empty() {
        return;
    }
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    tokio::spawn(async move {
        let _serialize = IN_FLIGHT.lock().await;
        if GENERATION.load(Ordering::SeqCst) != generation {
            return;
        }
        let Ok(endpoint) = crate::acp::client::discovery::discover_local() else {
            return;
        };
        let client = match crate::acp::client::HttpClient::new(endpoint) {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("plugin toggle: daemon client build failed: {e}");
                return;
            }
        };
        for (id, enabled) in changes {
            if GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            // Re-read persisted intent; never replay an enabled value after
            // releasing the settings operation lease.
            if let Err(e) = client.restart_plugin_worker(&id).await {
                tracing::warn!(
                    "plugin toggle: daemon did not refresh {id} (saved enabled={enabled}): {e}; restart the daemon or toggle from the dashboard"
                );
            }
        }
    });
}

pub fn dismiss_update(id: &str, fingerprint: &str) -> Result<()> {
    let _operation = begin_operation()?;
    update_config(|config| {
        let Some(entry) = config.plugins.get_mut(id) else {
            bail!("{id} is not an installed external plugin");
        };
        if entry.source.is_none() {
            bail!("{id} is not an installed external plugin");
        }
        entry.dismissed_update = Some(fingerprint.to_string());
        Ok(())
    })?
}

fn skip_reason(prepared: &Prepared) -> String {
    let mut parts = Vec::new();
    if prepared.caps_changed {
        parts.push("capability change");
    }
    if prepared.build_changed {
        parts.push("build-step change");
    }
    if prepared.ui_changed {
        parts.push("UI change");
    }
    if prepared.agents_changed {
        parts.push("agent launch change");
    }
    if prepared.runtime_change.is_some() {
        parts.push("runtime change");
    }
    if prepared.trust_downgrade {
        parts.push("trust downgrade");
    }
    if parts.is_empty() {
        "needs approval".to_string()
    } else {
        format!("{} needs approval", parts.join(" + "))
    }
}

pub fn uninstall(id: &str) -> Result<()> {
    PluginId::new(id.to_string()).map_err(|e| anyhow!("{e}"))?;
    let _operation = begin_operation()?;
    let result = super::execution::catch_panic(|| {
        if !is_installed_external(id)? {
            bail!("{id} is not an installed external plugin");
        }
        let version = super::registry()
            .get(id)
            .map(|p| p.manifest.version.clone());
        remove_external(id)?;
        tracing::info!(target: "plugin.install", plugin = %id, ?version, "plugin uninstalled");
        Ok(())
    });
    if super::execution::needs_recovery(&result) {
        log_recovery(&result);
        std::mem::forget(_operation);
    }
    result
}

fn is_installed_external(id: &str) -> Result<bool> {
    let config = Config::load()?;
    Ok(config
        .plugins
        .get(id)
        .and_then(|p| p.source.as_ref())
        .is_some())
}

/// Drop the plugin's directory, config entry and lock entry.
fn remove_external(id: &str) -> Result<()> {
    let dir = super::plugins_dir()?.join(id);
    remove_owned_tree(&dir).with_context(|| format!("removing {}", dir.display()))?;
    update_config(|config| config.plugins.remove(id))?;
    let mut lock = Lockfile::load()?;
    if lock.remove(id) {
        lock.save()?;
    }
    super::reload_registry();
    Ok(())
}

pub fn uninstall_logged(id: &str, log: &OperationLog) -> Result<()> {
    log.line(&format!("uninstalling {id}"));
    uninstall(id)?;
    log.line(&format!("uninstalled {id}"));
    Ok(())
}

fn reject_reserved_or_builtin(manifest: &PluginManifest, featured_verified: bool) -> Result<()> {
    let id = manifest.id.as_str();
    if super::registry::is_builtin_id(id) {
        bail!("plugin id {id:?} collides with a builtin plugin");
    }
    if manifest.id.is_reserved_namespace() && !featured_verified {
        bail!("plugin id {id:?} uses a reserved namespace (aoe.* / agent-of-empires.*); only a featured-verified plugin may claim one");
    }
    Ok(())
}

fn reject_incompatible_host(manifest: &PluginManifest) -> Result<()> {
    manifest
        .host_compat(env!("CARGO_PKG_VERSION"))
        .map_err(|msg| anyhow!("{}: {msg}", manifest.id.as_str()))
}

struct ResolvedSource {
    source: PluginSource,
    unverified: bool,
    notice: String,
}

async fn resolve_source(
    source: PluginSource,
    allow_branch_fallback: bool,
) -> Result<ResolvedSource> {
    match &source {
        PluginSource::Local(_) => Ok(ResolvedSource {
            unverified: false,
            notice: "installing from a local directory".to_string(),
            source,
        }),
        PluginSource::Github {
            reference: Some(reference),
            ..
        } => {
            let notice =
                format!("installing the explicit ref {reference:?} (not an audited release)");
            Ok(ResolvedSource {
                unverified: true,
                notice,
                source,
            })
        }
        PluginSource::Github {
            owner,
            repo,
            reference: None,
        } => match fetch::latest_release_tag(owner, repo).await? {
            Some(tag) => {
                let notice = format!("installing the latest release {tag}");
                let source = PluginSource::Github {
                    owner: owner.clone(),
                    repo: repo.clone(),
                    reference: Some(tag),
                };
                Ok(ResolvedSource {
                    unverified: false,
                    notice,
                    source,
                })
            }
            None if allow_branch_fallback => Ok(ResolvedSource {
                unverified: true,
                notice: format!(
                    "{owner}/{repo} has no published release; falling back to the unverified default branch"
                ),
                source,
            }),
            None => bail!(
                "{owner}/{repo} has no published release to update to; the prior version is kept"
            ),
        },
    }
}

fn confirm_unverified() -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("this is unverified, un-audited code; re-run with --yes to install it");
    }
    println!(
        "This is unverified, un-audited code: it does not come from a vetted release and is\n\
         not covered by the featured index. Install it only if you trust the source."
    );
    print!("Continue? [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn verify_featured(featured: &FeaturedIndex, fetched: &FetchedPlugin) -> Result<bool> {
    let id = fetched.manifest.id.as_str();
    let Some(entry) = featured.get(id) else {
        return Ok(false);
    };
    if matches!(
        fetched.manifest.runtime,
        Some(RuntimeSpec::ReleaseBinary { .. })
    ) {
        bail!("{id} is featured but ships a release-binary worker, which the featured index cannot pin yet; refusing install");
    }
    let slug = fetched.source.slug();
    if !slug.eq_ignore_ascii_case(&entry.source) {
        bail!(
            "{id} is featured from {:?} but you are installing from {slug:?}; refusing install",
            entry.source
        );
    }
    Ok(entry.verifies(&fetched.tree_hash))
}

fn capability_strings(fetched: &FetchedPlugin) -> Result<Vec<String>> {
    let unknown: Vec<&str> = fetched
        .manifest
        .capabilities
        .iter()
        .filter(|c| !c.is_known())
        .map(|c| c.as_str())
        .collect();
    if !unknown.is_empty() {
        bail!(
            "plugin requests capabilities this host does not support: {}; upgrade aoe",
            unknown.join(", ")
        );
    }
    Ok(fetched
        .manifest
        .capabilities
        .iter()
        .map(|c| c.as_str().to_string())
        .collect())
}

fn install_needs_consent(
    capabilities: &[String],
    build: &[BuildStep],
    ui: &[UiContribution],
    integrations: &[AgentIntegrationContribution],
) -> bool {
    !capabilities.is_empty() || !build.is_empty() || !ui.is_empty() || !integrations.is_empty()
}

fn confirm_capabilities(
    id: &str,
    capabilities: &[String],
    ui: &[UiContribution],
    build: &[BuildStep],
    integrations: &[AgentIntegrationContribution],
) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!(
            "{id} declares plugin access or executable commands but stdin is not a terminal; re-run with --yes to approve them",
        );
    }
    if !capabilities.is_empty() {
        println!("Plugin {id} requests these capabilities:");
        for capability in capabilities {
            println!("  - {capability}");
        }
    }
    if !ui.is_empty() {
        println!("Plugin {id} will add UI elements to these dashboard slots:");
        for u in ui {
            println!("  - {} ({})", u.slot.as_str(), u.id);
        }
    }
    if !build.is_empty() {
        println!(
            "Plugin {id} will run these build commands now, in its install directory,\n\
             as your user and outside capability enforcement:"
        );
        for step in build {
            println!("  $ {}", step.command.join(" "));
        }
    }
    if !integrations.is_empty() {
        println!("Plugin {id} declares these agent behaviors (commands run as you, unsandboxed):");
        for integration in integrations {
            for line in super::agent_integration::consent_lines(integration) {
                println!("  {line}");
            }
        }
    }
    println!(
        "Note: installing trusts this plugin. The host checks capabilities at its API boundary,\n\
         but a plugin worker (and any build step) runs without OS-level sandboxing, so a malicious\n\
         plugin is not contained. Build steps run as your user before any capability gate. Only\n\
         install plugins you trust."
    );
    print!("Grant them and install? [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Remove only an AoE-owned tree. Never follow links or chmod a pathname:
/// legacy dev-install links are unlinked without touching their user-owned target.
pub(super) fn remove_owned_tree(path: &Path) -> Result<()> {
    remove_owned_tree_inner(path, None)
}

fn remove_owned_tree_checked(
    parent: &crate::session::AnchoredDir,
    leaf: &Path,
    tree: publication::Identity,
) -> Result<()> {
    remove_owned_tree_inner(&parent.path().join(leaf), Some((parent, tree)))
}

fn remove_owned_tree_inner(
    path: &Path,
    expected: Option<(&crate::session::AnchoredDir, publication::Identity)>,
) -> Result<()> {
    use nix::dir::Dir;
    use nix::errno::Errno;
    use nix::fcntl::{open, openat, AtFlags, OFlag};
    use nix::sys::stat::{fchmod, fstat, fstatat, FileStat, Mode};
    use nix::unistd::{geteuid, unlinkat, UnlinkatFlags};
    use std::ffi::CStr;
    use std::os::fd::{AsFd, OwnedFd};

    fn same_entry(left: &FileStat, right: &FileStat) -> bool {
        left.st_dev == right.st_dev
            && left.st_ino == right.st_ino
            && left.st_mode & libc::S_IFMT == right.st_mode & libc::S_IFMT
    }

    fn remove(
        parent: &OwnedFd,
        name: &CStr,
        device: libc::dev_t,
        root: bool,
        expected: Option<publication::Identity>,
        verify_mount: bool,
    ) -> Result<()> {
        let observed = match fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) if expected.is_none() => return Ok(()),
            Err(error) => return Err(error).context("inspecting owned cleanup entry"),
        };
        let kind = observed.st_mode & libc::S_IFMT;
        anyhow::ensure!(
            expected.is_none() || kind == libc::S_IFDIR,
            "owned cleanup root identity changed"
        );
        match kind {
            libc::S_IFDIR => {
                if observed.st_uid != geteuid().as_raw() || device != observed.st_dev {
                    bail!("owned cleanup refuses a foreign directory or mount");
                }
                let fd = openat(
                    parent,
                    name,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .context("opening owned cleanup directory without following links")?;
                let opened = fstat(&fd)?;
                if !same_entry(&observed, &opened) || opened.st_uid != geteuid().as_raw() {
                    bail!("owned cleanup directory changed before opening");
                }
                if let Some(expected) = expected {
                    anyhow::ensure!(
                        publication::Identity::capture(fd.as_fd())? == expected,
                        "owned cleanup root identity changed"
                    );
                }
                if verify_mount {
                    crate::process::prove_same_mount(parent.as_fd(), fd.as_fd())?;
                }
                if opened.st_mode & 0o700 != 0o700 {
                    fchmod(&fd, Mode::from_bits_truncate(opened.st_mode | 0o700))
                        .context("restoring owner access to owned cleanup directory")?;
                }
                let mut entries = Dir::from_fd(fd.try_clone()?)?;
                for entry in entries.iter() {
                    let entry = entry.context("reading owned cleanup directory")?;
                    let name = entry.file_name();
                    if name.to_bytes() != b"." && name.to_bytes() != b".." {
                        remove(&fd, name, opened.st_dev, false, None, verify_mount)?;
                    }
                }
                let current = fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
                if !same_entry(&opened, &current) {
                    bail!("owned cleanup directory changed before removal");
                }
                unlinkat(parent, name, UnlinkatFlags::RemoveDir)?;
            }
            libc::S_IFLNK | libc::S_IFREG if !root || kind == libc::S_IFLNK => {
                let current = fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
                if !same_entry(&observed, &current) {
                    bail!("owned cleanup leaf changed before removal");
                }
                unlinkat(parent, name, UnlinkatFlags::NoRemoveDir)?;
            }
            _ => bail!("owned cleanup refuses an unexpected file or special entry"),
        }
        Ok(())
    }

    let leaf = path
        .file_name()
        .context("owned cleanup needs a tree leaf")?;
    let parent = path.parent().context("owned cleanup needs a tree parent")?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent = if let Some((anchor, _)) = expected {
        anchor.descriptor().try_clone_to_owned()?
    } else {
        match open(
            parent,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => return Ok(()),
            Err(error) => return Err(error).context("opening owned cleanup parent"),
        }
    };
    use std::os::unix::ffi::OsStrExt;
    let leaf = std::ffi::CString::new(leaf.as_bytes())?;
    let observed = fstat(&parent)?;
    remove(
        &parent,
        &leaf,
        observed.st_dev,
        true,
        expected.map(|(_, tree)| tree),
        expected.is_some(),
    )
    .with_context(|| format!("removing owned tree {}", path.display()))
}

fn with_publication<T>(
    fetched: &FetchedPlugin,
    final_dir: &Path,
    publish: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    with_publication_copy(fetched, final_dir, None, |tree, _| publish(tree))
}

fn with_publication_copy<T>(
    fetched: &FetchedPlugin,
    final_dir: &Path,
    transaction: Option<(&PriorMetadata, bool)>,
    publish: impl FnOnce(&Path, Option<&mut publication::Journal>) -> Result<T>,
) -> Result<T> {
    fetched.begin_publication()?;
    let parent = final_dir
        .parent()
        .context("plugin destination has no parent")?;
    // Keep the path before copying: TempDir::drop silently discards cleanup
    // errors, including partial copies whose directory modes are read-only.
    let directory = tempfile::Builder::new()
        .prefix(".publish-")
        .tempdir_in(parent)
        .context("creating destination-local plugin publication directory")?
        .keep();
    let mut journal = None;
    let result = super::execution::catch_panic(|| {
        let tree = directory.join("tree");
        fetched.copy_for_publication(&tree)?;
        if let Some((prior, update)) = transaction {
            journal = Some(publication::Journal::prepare(
                final_dir, &tree, prior, update,
            )?);
        }
        publish(&tree, journal.as_mut())
    });
    if transaction.is_some() && super::execution::needs_recovery(&result) {
        return result;
    }
    #[cfg(test)]
    fetch::test_gate::reach(final_dir, "publication-cleanup");
    let cleanup = match &journal {
        Some(journal) => journal.clean_container(final_dir),
        None => remove_owned_tree(&directory),
    };
    if let Err(cleanup) = cleanup {
        return Err(super::execution::RecoveryRequired(format!(
            "plugin publication cleanup unproven at {}: {cleanup:#}; publication outcome: {}",
            directory.display(),
            match &result {
                Ok(_) => "succeeded".to_owned(),
                Err(error) => format!("{error:#}"),
            }
        ))
        .into());
    }
    if let Some(journal) = journal {
        journal
            .finish(final_dir, result.is_ok())
            .map_err(publication_error)?;
    }
    result.map_err(|error| {
        if error.is::<publication::PriorBackup>() {
            publication_error(error)
        } else {
            error
        }
    })
}

fn publication_error(error: anyhow::Error) -> anyhow::Error {
    super::execution::RecoveryRequired(format!("plugin publication ownership unproven: {error:#}"))
        .into()
}

fn with_publication_transaction<T>(
    fetched: &FetchedPlugin,
    final_dir: &Path,
    prior: &PriorMetadata,
    update: bool,
    publish: impl FnOnce(&Path, Option<&mut publication::Journal>) -> Result<T>,
) -> Result<T> {
    let transaction = fetched.journals_publication().then_some((prior, update));
    with_publication_copy(fetched, final_dir, transaction, publish)
}

fn before_publication_build(
    journal: &mut Option<&mut publication::Journal>,
    final_dir: &Path,
    manifest: &PluginManifest,
) -> Result<()> {
    if build_steps(manifest).iter().any(|step| {
        step.platforms.is_empty() || step.platforms.iter().any(|os| os == std::env::consts::OS)
    }) {
        publication_working(journal, final_dir)?;
    }
    Ok(())
}

fn publication_working(
    journal: &mut Option<&mut publication::Journal>,
    final_dir: &Path,
) -> Result<()> {
    if let Some(journal) = journal {
        journal.working(final_dir).map_err(publication_error)?;
    }
    Ok(())
}

fn publication_committed(
    journal: Option<&mut publication::Journal>,
    final_dir: &Path,
) -> Result<()> {
    if let Some(journal) = journal {
        journal.committed(final_dir).map_err(publication_error)?;
    }
    Ok(())
}

fn remove_published_tree(journal: Option<&publication::Journal>, final_dir: &Path) -> Result<()> {
    match journal {
        Some(journal) => journal.remove_installed(final_dir),
        None => remove_owned_tree(final_dir),
    }
}

fn rename_publication_tree(tree: &Path, destination: &Path) -> Result<()> {
    place_publication_tree(None, tree, destination)
}

fn place_publication_tree(
    journal: Option<&publication::Journal>,
    tree: &Path,
    destination: &Path,
) -> Result<()> {
    use nix::fcntl::{open, OFlag};
    use nix::sys::stat::{fchmod, fstat, Mode};

    let directory = open(
        tree,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .context("opening publication root")?;
    let mode = Mode::from_bits_truncate(fstat(&directory)?.st_mode);
    // Moving a directory across parents can require updating its read-only `..`.
    fchmod(&directory, mode | Mode::S_IWUSR)?;
    let result = match journal {
        Some(journal) => journal.place(destination),
        None => std::fs::rename(tree, destination).map_err(Into::into),
    }
    .with_context(|| format!("publishing plugin into {}", destination.display()));
    if let Err(error) = fchmod(&directory, mode) {
        return Err(super::execution::RecoveryRequired(format!(
            "restoring publication root permissions for {} -> {}: {error}; rename outcome: {result:?}",
            tree.display(),
            destination.display()
        ))
        .into());
    }
    result
}

fn move_into_place(fetched: &FetchedPlugin, final_dir: &Path) -> Result<()> {
    with_publication(fetched, final_dir, |tree| {
        if final_dir.exists() {
            bail!("plugin destination {} already exists", final_dir.display());
        }
        rename_publication_tree(tree, final_dir)
    })
}

fn build_steps(manifest: &PluginManifest) -> &[BuildStep] {
    match &manifest.runtime {
        Some(RuntimeSpec::Command { build, .. }) => build,
        _ => &[],
    }
}

/// Resolve Node once, off the runtime threads, for the sync apply step.
async fn apply_node_env(manifest: &PluginManifest) -> NodeEnv {
    if !super::needs_node(manifest) {
        return NodeEnv::new();
    }
    super::ensure_node().await;
    tokio::task::spawn_blocking(super::node_env)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(target: "plugin.install", %error, "Node resolution task failed");
            NodeEnv::new()
        })
}

fn build_in_place(
    plugin_id: &str,
    dir: &Path,
    manifest: &PluginManifest,
    node_env: &NodeEnv,
    log: &OperationLog,
    budget: super::execution::Budget,
) -> Result<()> {
    run_build(plugin_id, dir, build_steps(manifest), node_env, log, budget)?;
    if let Some(RuntimeSpec::Command {
        command,
        system: false,
        ..
    }) = &manifest.runtime
    {
        let resolver = super::launch::OsLaunchResolver::new(node_env.clone());
        super::launch::resolve_command(plugin_id, dir, command, &resolver).with_context(|| {
            format!(
                "plugin {plugin_id}: worker command is not runnable after install \
                     (a build step may have been skipped on this platform, or did not produce it)"
            )
        })?;
    }
    if let Some(RuntimeSpec::ReleaseBinary { asset, bin }) = &manifest.runtime {
        super::launch::resolve_release_binary(
            plugin_id,
            dir,
            asset,
            &manifest.version,
            bin.as_deref(),
            &super::launch::OsLaunchResolver::new(node_env.clone()),
        )
        .context("release-binary worker is not runnable after install")?;
    }
    Ok(())
}

fn run_build(
    plugin_id: &str,
    dir: &Path,
    steps: &[BuildStep],
    node_env: &NodeEnv,
    log: &OperationLog,
    budget: super::execution::Budget,
) -> Result<()> {
    let os = std::env::consts::OS;
    let resolver = super::launch::OsLaunchResolver::new(node_env.clone());
    for (i, step) in steps.iter().enumerate() {
        if !step.platforms.is_empty() && !step.platforms.iter().any(|p| p == os) {
            continue;
        }
        let pretty = step.command.join(" ");
        let (program, args) =
            super::launch::resolve_command(plugin_id, dir, &step.command, &resolver)
                .with_context(|| format!("resolving build step {} ({pretty})", i + 1))?;
        log.line(&format!("  building {plugin_id}: {pretty}"));
        let (stdout, stderr) = log.child_stdio()?;
        let mut command = std::process::Command::new(&program);
        command
            .args(&args)
            .current_dir(dir)
            .env("AOE_PLUGIN_ID", plugin_id)
            .envs(node_env.iter().map(|(k, v)| (*k, v)))
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        let status = budget
            .run(&mut command)
            .with_context(|| format!("running build step {} ({pretty})", i + 1))?
            .status;
        if !status.success() {
            bail!("build step {} ({pretty}) failed with {status}", i + 1);
        }
    }
    Ok(())
}

fn replace_and_build(
    plugin_id: &str,
    fetched: &FetchedPlugin,
    final_dir: &Path,
    node_env: &NodeEnv,
    log: &OperationLog,
    commit_metadata: impl FnOnce() -> Result<()>,
    prior: &PriorMetadata,
) -> Result<()> {
    let backup_dir = final_dir.with_file_name(format!("{plugin_id}.bak"));

    if backup_dir.exists() {
        return Err(super::execution::RecoveryRequired(format!(
            "ambiguous prior update backup {}; manual recovery required",
            backup_dir.display()
        ))
        .into());
    }

    // Copy and verify before displacing the prior installation. Rename stays on
    // the destination filesystem even when acquisition used a private mount.
    with_publication_transaction(fetched, final_dir, prior, true, |tree, mut journal| {
        let had_prior = match journal.as_deref() {
            Some(journal) => journal.has_prior_tree(),
            None => final_dir.exists(),
        };
        if had_prior {
            #[cfg(test)]
            fetch::test_gate::reach(final_dir, "publication-backup");
            match journal.as_deref() {
                Some(journal) => journal.displace(final_dir),
                None => std::fs::rename(final_dir, &backup_dir).map_err(Into::into),
            }
            .with_context(|| format!("backing up current {plugin_id} before update"))?;
        }
        #[cfg(test)]
        fetch::test_gate::reach(final_dir, "publication-displaced");

        let mut placed = false;
        let place_and_build = (|| -> Result<()> {
            place_publication_tree(journal.as_deref(), tree, final_dir)?;
            placed = true;
            before_publication_build(&mut journal, final_dir, &fetched.manifest)?;
            build_in_place(
                plugin_id,
                final_dir,
                &fetched.manifest,
                node_env,
                log,
                fetched.budget,
            )?;
            #[cfg(test)]
            fetch::test_gate::reach(final_dir, "metadata-commit");
            publication_working(&mut journal, final_dir)?;
            commit_metadata()?;
            publication_committed(journal.as_deref_mut(), final_dir)
        })();

        match place_and_build {
            Ok(()) => {
                if had_prior {
                    #[cfg(test)]
                    fetch::test_gate::reach(final_dir, "backup-cleanup");
                    let cleanup = match journal.as_deref() {
                        Some(journal) => journal.remove_backup(&backup_dir),
                        None => remove_owned_tree(&backup_dir),
                    };
                    if let Err(error) = cleanup {
                        return Err(super::execution::RecoveryRequired(format!(
                        "update {plugin_id} committed, but backup cleanup at {} failed: {error}; do not restore this backup over the committed installation",
                        backup_dir.display()
                    )).into());
                    }
                }
                Ok(())
            }
            Err(e) => {
                if e.downcast_ref::<super::execution::RecoveryRequired>()
                    .is_some()
                {
                    return Err(e);
                }
                #[cfg(test)]
                fetch::test_gate::reach(final_dir, "rollback");
                let metadata = prior.restore(plugin_id);
                if placed || journal.is_none() {
                    remove_published_tree(journal.as_deref(), final_dir).map_err(|error| {
                        super::execution::RecoveryRequired(format!(
                            "removing failed update {}: {error}; original failure: {e:#}; metadata restoration: {metadata:?}",
                            final_dir.display()
                        ))
                    })?;
                }
                if had_prior {
                    let restore = match journal.as_deref() {
                        Some(journal) => journal.restore_backup(final_dir),
                        None => std::fs::rename(&backup_dir, final_dir).map_err(Into::into),
                    };
                    restore.map_err(|error| {
                        super::execution::RecoveryRequired(format!(
                            "restoring {} from {}: {error}; original failure: {e:#}; metadata restoration: {metadata:?}",
                            final_dir.display(),
                            backup_dir.display()
                        ))
                    })?;
                }
                if let Err(error) = metadata {
                    return Err(super::execution::RecoveryRequired(format!(
                        "update {plugin_id} failed: {e:#}; metadata restoration failed: {error:#}"
                    ))
                    .into());
                }
                Err(e)
            }
        }
    })
}

fn persisted_source(source: &PluginSource, input: &str) -> String {
    match source {
        PluginSource::Github { .. } => input.to_string(),
        PluginSource::Local(path) => std::fs::canonicalize(path)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| input.to_string()),
    }
}

fn persist_install(
    source: &str,
    id: &str,
    capabilities: &[String],
    manifest_hash: &str,
    dev: bool,
) -> Result<()> {
    update_config(|config| {
        let entry = config
            .plugins
            .entry(id.to_string())
            .or_insert_with(PluginConfig::default);
        entry.source = Some(source.to_string());
        entry.dev = dev;
        entry.grant = Some(CapabilityGrant {
            manifest_hash: manifest_hash.to_string(),
            capabilities: capabilities.to_vec(),
            granted_at: crate::time::Timestamp::now(),
        });
    })
}

fn persist_update(id: &str, source: &str, grant: Option<CapabilityGrant>) -> Result<()> {
    update_config(|config| {
        let entry = config
            .plugins
            .entry(id.to_string())
            .or_insert_with(PluginConfig::default);
        entry.source = Some(source.to_string());
        entry.grant = grant;
        entry.dismissed_update = None;
    })
}

fn write_lock(id: &str, fetched: &FetchedPlugin, manifest_hash: &str, trust: &str) -> Result<()> {
    let mut lock = Lockfile::load()?;
    lock.upsert(
        id,
        LockedPlugin {
            source: fetched.source.slug(),
            requested_ref: fetched.requested_ref.clone(),
            resolved_commit: fetched.resolved_commit.clone(),
            version: fetched.manifest.version.clone(),
            manifest_hash: manifest_hash.to_string(),
            tree_hash: fetched.tree_hash.clone(),
            trust: trust.to_string(),
            release_tag: fetched.release_tag.clone(),
            asset_name: fetched.asset_name.clone(),
            asset_sha256: fetched.asset_sha256.clone(),
        },
    );
    lock.save()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::isolate_app_dir;
    use aoe_plugin_api::UiSlot;
    use serial_test::serial;

    const PUBLICATION_ID: &str = "acme.journal";
    const PUBLICATION_SOURCE: &str = "gh:acme/journal";

    #[tokio::test]
    #[serial]
    async fn publication_preparation_refusal_cleans_unjournaled_copy() {
        let _home = isolate_app_dir();
        let app = crate::session::get_app_dir().unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(
            source.path().join("aoe-plugin.toml"),
            "id = \"acme.journal\"\nname = \"Journal\"\nversion = \"2.0.0\"\napi_version = 2\n",
        )
        .unwrap();
        let operation = begin_operation().unwrap();
        let fetched = fetch::fetch(
            &PluginSource::Local(source.path().to_owned()),
            Some(operation.clone()),
        )
        .await
        .unwrap();
        let plugins = super::super::plugins_dir().unwrap();
        let final_dir = plugins.join(PUBLICATION_ID);
        let backup = plugins.join(format!("{PUBLICATION_ID}.bak"));
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("payload"), b"prior").unwrap();
        let prior = PriorMetadata::capture(PUBLICATION_ID).unwrap();
        // Exercise the journal transaction without depending on a Git clone.
        let result: Result<()> =
            with_publication_copy(&fetched, &final_dir, Some((&prior, true)), |_, _| {
                panic!("a backup obstruction must prevent publication")
            });
        assert!(super::super::execution::needs_recovery(&result));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("publication backup already exists"));
        let marker = app.join(publication::MARKER);
        assert!(!marker.exists());
        assert!(!final_dir.exists());
        assert_eq!(std::fs::read(backup.join("payload")).unwrap(), b"prior");
        std::fs::rename(&backup, &final_dir).unwrap();
        let result: Result<()> =
            with_publication_copy(&fetched, &final_dir, Some((&prior, false)), |_, _| {
                panic!("an occupied install destination must prevent publication")
            });
        assert!(!super::super::execution::needs_recovery(&result));
        assert_eq!(
            result.unwrap_err().to_string(),
            "plugin destination already exists"
        );
        assert!(!marker.exists());
        assert_eq!(std::fs::read(final_dir.join("payload")).unwrap(), b"prior");
        assert!(std::fs::read_dir(&plugins).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".publish-")
        }));
        assert!(check_recovery_evidence().is_ok());

        std::fs::write(&marker, b"existing evidence").unwrap();
        let result: Result<()> =
            with_publication_copy(&fetched, &final_dir, Some((&prior, true)), |_, _| {
                panic!("existing evidence must prevent publication")
            });
        assert!(super::super::execution::needs_recovery(&result));
        assert_eq!(std::fs::read(&marker).unwrap(), b"existing evidence");
        assert!(check_recovery_evidence().is_err());
        fetched.cleanup().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "subprocess fixture for publication crash recovery"]
    async fn publication_crash_child() {
        use std::os::unix::net::UnixStream;

        let Some(home) = std::env::var_os("AOE_PUBLICATION_CRASH_HOME") else {
            return;
        };
        let _home = crate::session::test_support::isolate_app_dir_at(Path::new(&home));
        let phase = std::env::var("AOE_PUBLICATION_CRASH_PHASE").unwrap();
        let _operation = begin_operation().unwrap();
        let final_dir = super::super::plugins_dir().unwrap().join(PUBLICATION_ID);
        let fetched = fetch::stage_remote_fixture(
            PluginSource::parse(PUBLICATION_SOURCE).unwrap(),
            Path::new(&std::env::var_os("AOE_PUBLICATION_CRASH_REPO").unwrap()),
        )
        .unwrap();
        assert!(fetched.journals_publication());
        let mut gate = fetch::test_gate::install(
            &final_dir,
            match phase.as_str() {
                "install-ready" => "metadata-commit",
                "install-committed" => "publication-cleanup",
                "update-displaced" => "publication-displaced",
                "update-committed" => "backup-cleanup",
                "update-partial" => "publication-partial-metadata",
                _ => panic!("unknown crash phase"),
            },
        );
        let task = tokio::task::spawn_blocking(move || {
            let manifest_hash = PluginManifest::hash_bytes(&fetched.manifest_bytes);
            if phase.starts_with("install") {
                let prepared = PreparedInstall {
                    persisted_source: PUBLICATION_SOURCE.to_owned(),
                    unverified: true,
                    notice: String::new(),
                    fetched,
                    featured_verified: false,
                    dev: false,
                    id: PUBLICATION_ID.to_owned(),
                    capabilities: Vec::new(),
                    manifest_hash,
                    fingerprint: String::new(),
                    validation: install_validation(false, false, PUBLICATION_SOURCE),
                };
                apply_prepared_install(&prepared, &NodeEnv::new(), &OperationLog::Inherit).unwrap();
            } else {
                std::fs::create_dir(&final_dir).unwrap();
                std::fs::write(final_dir.join("payload"), b"old").unwrap();
                persist_install(PUBLICATION_SOURCE, PUBLICATION_ID, &[], "old", false).unwrap();
                write_lock(PUBLICATION_ID, &fetched, "old", "unpinned").unwrap();
                let prior = PriorMetadata::capture(PUBLICATION_ID).unwrap();
                replace_and_build(
                    PUBLICATION_ID,
                    &fetched,
                    &final_dir,
                    &NodeEnv::new(),
                    &OperationLog::Inherit,
                    || {
                        persist_update(PUBLICATION_ID, "gh:acme/journal@new", None)?;
                        fetch::test_gate::reach(&final_dir, "publication-partial-metadata");
                        write_lock(PUBLICATION_ID, &fetched, &manifest_hash, "unpinned")
                    },
                    &prior,
                )
                .unwrap();
            }
        });
        (&mut gate.entered).await.unwrap();
        let mut control =
            UnixStream::connect(std::env::var_os("AOE_PUBLICATION_CRASH_CONTROL").unwrap())
                .unwrap();
        control.write_all(b"R").unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "subprocess fixture for writable daemon startup"]
    async fn publication_restart_child() {
        let Some(home) = std::env::var_os("AOE_PUBLICATION_CRASH_HOME") else {
            return;
        };
        let _home = crate::session::test_support::isolate_app_dir_at(Path::new(&home));
        let transaction = crate::daemon::lifecycle::Transaction::try_acquire().unwrap();
        let _lifetime = transaction.lifetime().unwrap();
        crate::server::start_server(
            crate::server::ServerConfig {
                profile: "",
                host: "127.0.0.1",
                port: 0,
                core_only: true,
                read_acquisition: false,
                auth_mode: crate::cli::serve::AuthMode::None,
                read_only: false,
                remote: false,
                tunnel_name: None,
                tunnel_url: None,
                no_tailscale: true,
                funnel_port: 443,
                is_daemon: true,
                passphrase: None,
                behind_proxy: false,
                allow_plaintext_remote: false,
                open_browser: false,
                extra_allowed_hosts: Vec::new(),
                extra_allowed_origins: Vec::new(),
            },
            transaction,
        )
        .await
        .unwrap();
    }

    #[test]
    #[serial]
    fn default_remote_publication_reconciles_after_crash() {
        use crate::git::test_support::run_git;
        use std::io::Read as _;
        use std::os::unix::net::{UnixDatagram, UnixListener};
        use std::time::{Duration, Instant};

        struct Daemon(std::process::Child);
        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let fixture = tempfile::tempdir_in("/tmp").unwrap();
        let repo = fixture.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(
            repo.join("aoe-plugin.toml"),
            "id = \"acme.journal\"\nname = \"Journal\"\nversion = \"2.0.0\"\napi_version = 2\n",
        )
        .unwrap();
        std::fs::write(repo.join("payload"), b"new").unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-qm", "fixture"]);

        for (case, phase) in [
            ("install-ready", "install-ready"),
            ("install-committed", "install-committed"),
            ("update-displaced", "update-displaced"),
            ("update-committed", "update-committed"),
            ("update-partial", "update-partial"),
            ("replaced-tree", "install-ready"),
            ("changed-birth", "install-ready"),
        ] {
            let home = fixture.path().join(case);
            let _home = crate::session::test_support::isolate_app_dir_at(&home);
            let app = crate::session::get_app_dir().unwrap();
            let plugins = app.join("plugins");
            let installed = plugins.join(PUBLICATION_ID);
            let backup = plugins.join(format!("{PUBLICATION_ID}.bak"));
            let marker = app.join(publication::MARKER);
            let socket = home.join("control");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut daemon = Daemon(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "plugin::install::tests::publication_crash_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("AOE_PUBLICATION_CRASH_HOME", &home)
                    .env("AOE_PUBLICATION_CRASH_PHASE", phase)
                    .env("AOE_PUBLICATION_CRASH_REPO", &repo)
                    .env("AOE_PUBLICATION_CRASH_CONTROL", &socket)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_TEMPLATE_DIR", "")
                    .env_remove("GIT_CONFIG")
                    .env_remove("GIT_CONFIG_COUNT")
                    .env_remove("GIT_CONFIG_PARAMETERS")
                    .env_remove("GIT_ALLOW_PROTOCOL")
                    .env_remove("GIT_PROTOCOL_FROM_USER")
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut control = loop {
                match listener.accept() {
                    Ok((control, _)) => break control,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            daemon.0.try_wait().unwrap().is_none(),
                            "{case} exited before gate"
                        );
                        assert!(Instant::now() < deadline, "{case} did not reach gate");
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("{case}: {error}"),
                }
            };
            control.set_nonblocking(false).unwrap();
            control
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut ready = [0];
            control.read_exact(&mut ready).unwrap();
            assert_eq!(ready, *b"R");
            let mut evidence = std::fs::read(&marker).unwrap();
            assert!(recover_stale_publication().is_err(), "{case}: live owner");
            assert!(check_recovery_evidence().is_err(), "{case}: worker launch");
            if phase == "update-displaced" {
                assert!(!installed.exists());
            } else {
                assert_eq!(std::fs::read(installed.join("payload")).unwrap(), b"new");
            }
            if phase.starts_with("update") {
                assert_eq!(std::fs::read(backup.join("payload")).unwrap(), b"old");
            }
            daemon.0.kill().unwrap();
            assert!(!daemon.0.wait().unwrap().success());

            if case == "replaced-tree" {
                std::fs::rename(&installed, installed.with_file_name("held")).unwrap();
                std::fs::create_dir(&installed).unwrap();
                std::fs::write(installed.join("payload"), b"foreign").unwrap();
            } else if case == "changed-birth" {
                let mut record: serde_json::Value = serde_json::from_slice(&evidence).unwrap();
                record["new_tree"]["birth"] =
                    serde_json::json!({"secs_since_epoch": 1, "nanos_since_epoch": 0});
                evidence = serde_json::to_vec(&record).unwrap();
                evidence.push(b'\n');
                std::fs::write(&marker, &evidence).unwrap();
            }
            if matches!(case, "update-partial" | "replaced-tree" | "changed-birth") {
                assert!(
                    recover_stale_publication().is_err(),
                    "{case}: ambiguous state must stay fenced"
                );
                assert_eq!(std::fs::read(&marker).unwrap(), evidence);
                assert!(begin_operation().is_err());
                assert!(check_recovery_evidence().is_err());
                if case == "update-partial" {
                    assert_eq!(std::fs::read(backup.join("payload")).unwrap(), b"old");
                    assert_eq!(std::fs::read(installed.join("payload")).unwrap(), b"new");
                    let metadata = PriorMetadata::capture(PUBLICATION_ID).unwrap();
                    assert_eq!(
                        metadata.config.unwrap().source.as_deref(),
                        Some("gh:acme/journal@new")
                    );
                    assert_eq!(metadata.locked.unwrap().manifest_hash, "old");
                } else if case == "replaced-tree" {
                    assert_eq!(
                        std::fs::read(installed.join("payload")).unwrap(),
                        b"foreign"
                    );
                } else {
                    assert_eq!(std::fs::read(installed.join("payload")).unwrap(), b"new");
                }
                continue;
            }

            let notify_path = home.join("notify");
            let notify = UnixDatagram::bind(&notify_path).unwrap();
            notify
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut restarted = Daemon(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "plugin::install::tests::publication_restart_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("AOE_PUBLICATION_CRASH_HOME", &home)
                    .env("AOE_TMUX_SOCKET", home.join("tmux.sock"))
                    .env("NOTIFY_SOCKET", &notify_path)
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut message = [0; 256];
            loop {
                match notify.recv(&mut message) {
                    Ok(len) if message[..len].starts_with(b"READY=1\n") => break,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => panic!("{case}: receiving daemon readiness: {error}"),
                }
                assert!(
                    restarted.0.try_wait().unwrap().is_none(),
                    "{case}: restarted daemon exited before readiness"
                );
                assert!(
                    Instant::now() < deadline,
                    "{case}: daemon startup timed out"
                );
            }
            // Check durable state before any plugin operation can recover it.
            assert!(!marker.exists());
            assert!(!backup.exists());
            assert!(std::fs::read_dir(&plugins).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".publish-")
            }));
            let metadata = PriorMetadata::capture(PUBLICATION_ID).unwrap();
            match case {
                "install-ready" => {
                    assert!(!installed.exists());
                    assert!(metadata.config.is_none() && metadata.locked.is_none());
                }
                "update-displaced" => {
                    assert_eq!(std::fs::read(installed.join("payload")).unwrap(), b"old");
                    assert!(metadata.config.unwrap().grant.is_some());
                    assert_eq!(metadata.locked.unwrap().manifest_hash, "old");
                }
                _ => {
                    assert_eq!(std::fs::read(installed.join("payload")).unwrap(), b"new");
                    let config = metadata.config.unwrap();
                    let locked = metadata.locked.unwrap();
                    assert_eq!(locked.version, "2.0.0");
                    assert_eq!(
                        locked.manifest_hash,
                        PluginManifest::hash_bytes(
                            &std::fs::read(installed.join("aoe-plugin.toml")).unwrap()
                        )
                    );
                    assert_eq!(config.grant.is_some(), case == "install-committed");
                }
            }
            drop(restarted);
            assert!(check_recovery_evidence().is_ok());
            assert!(begin_operation().is_ok());
        }
    }

    #[test]
    #[ignore = "subprocess fixture for staging crash recovery"]
    fn staging_crash_child() {
        use std::io::{Read as _, Write as _};
        use std::os::unix::fs::DirBuilderExt as _;
        use std::os::unix::net::UnixStream;
        let Some(root) = std::env::var_os("AOE_STAGING_CRASH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let phase = std::env::var("AOE_STAGING_CRASH_PHASE").unwrap();
        #[cfg(target_os = "linux")]
        if phase == "foreign" {
            let error = recover_staging(&root).unwrap_err();
            assert!(
                error.to_string().contains("different PID namespace"),
                "{error:#}"
            );
            assert!(root.join(STAGING_MARKER).exists());
            return;
        }
        let _operation = crate::session::try_acquire_storage_flock(&root, OPERATION_LOCK)
            .unwrap()
            .unwrap();
        let mount = root.join("plugins/.staging-crash/volume");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&mount)
            .unwrap();
        create_staging_marker(&root.join(STAGING_MARKER), &mount).unwrap();
        let mut volume = (phase != "prepared")
            .then(|| crate::process::StagingVolume::create(&mount, 1024 * 1024).unwrap());
        let mut writer = if phase == "writers" {
            let mut command = volume.as_ref().unwrap().git_command();
            command.args([
                "-c",
                "alias.hold=!exec /bin/sh -c 'exec 3>\"$TMPDIR/writer\"; printf x >&3; printf W; while :; do :; done'",
                "hold",
            ]);
            let mut child = command.spawn().unwrap();
            let mut ready = [0];
            child
                .stdout
                .as_mut()
                .unwrap()
                .read_exact(&mut ready)
                .unwrap();
            assert_eq!(ready, *b"W");
            Some(child)
        } else {
            None
        };
        let mut control =
            UnixStream::connect(std::env::var_os("AOE_STAGING_CRASH_CONTROL").unwrap()).unwrap();
        control.write_all(b"R").unwrap();
        let mut release = [0];
        let _ = control.read(&mut release);
        if let Some(writer) = &mut writer {
            let _ = writer.kill();
            let _ = writer.wait();
        }
        if let Some(volume) = &mut volume {
            volume.close().unwrap();
        }
    }

    fn crash_staging_and_recover(phase: &str, legacy: bool, plugins_mode: u32) {
        use std::io::Read as _;
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::net::UnixListener;
        use std::time::{Duration, Instant};
        struct Daemon(std::process::Child);
        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _home = isolate_app_dir();
        let root = crate::session::get_app_dir().unwrap();
        let plugins = root.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::set_permissions(&plugins, std::fs::Permissions::from_mode(plugins_mode)).unwrap();
        let control_dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = control_dir.path().join("control");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut daemon = Daemon(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "plugin::install::tests::staging_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("AOE_STAGING_CRASH_ROOT", &root)
                .env("AOE_STAGING_CRASH_PHASE", phase)
                .env("AOE_STAGING_CRASH_CONTROL", &socket)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut control = loop {
            match listener.accept() {
                Ok((control, _)) => break control,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        daemon.0.try_wait().unwrap().is_none(),
                        "staging daemon exited before its phase gate"
                    );
                    assert!(
                        Instant::now() < deadline,
                        "staging daemon did not reach phase gate"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("staging control failed: {error}"),
            }
        };
        control.set_nonblocking(false).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut ready = [0];
        control.read_exact(&mut ready).unwrap();
        assert_eq!(ready, *b"R");
        assert!(
            recover_stale_staging().is_err(),
            "live operation exclusion must hold"
        );
        assert!(
            recover_staging(&root).is_err(),
            "live owner must retain staging"
        );
        assert!(root.join("plugins/.staging-crash/volume").is_dir());
        assert!(begin_operation().is_err());
        let anchored = crate::session::AnchoredDir::open(&root).unwrap();
        let original = anchored
            .read_regular(Path::new(STAGING_MARKER), 1024 * 1024)
            .unwrap()
            .unwrap();

        #[cfg(target_os = "linux")]
        if phase == "writers" {
            let (evidence, writers, _) = staging_evidence(&anchored).unwrap();
            let crate::process::StagingCustody::LinuxPidNamespace { init, .. } = evidence.custody
            else {
                panic!("missing PID namespace custody")
            };
            let members =
                crate::process::runner_group_identities(writers.last().unwrap().pid).unwrap();
            assert!(
                members.len() >= 2,
                "writer must have entered its PID namespace"
            );
            let handles: Vec<_> = members
                .iter()
                .map(|member| {
                    crate::process::CapturedProcess::capture(
                        member.pid,
                        &crate::process::ProcessBirthIdentity {
                            boot_id: init.birth.boot_id.clone(),
                            start_id: member.start_id.clone(),
                        },
                    )
                    .unwrap()
                    .unwrap()
                })
                .collect();
            let init_handle = crate::process::CapturedProcess::capture(init.pid, &init.birth)
                .unwrap()
                .unwrap();
            init_handle.signal(libc::SIGKILL).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while handles.iter().any(|handle| !handle.exited().unwrap()) {
                assert!(
                    Instant::now() < deadline,
                    "kernel did not tear down live staging writers"
                );
                assert!(
                    daemon.0.try_wait().unwrap().is_none(),
                    "daemon must stay alive during init teardown"
                );
                std::thread::yield_now();
            }
            assert!(
                recover_stale_staging().is_err(),
                "init exit must not bypass live operation exclusion"
            );
        }

        daemon.0.kill().unwrap();
        daemon.0.wait().unwrap();
        #[cfg(target_os = "linux")]
        if phase != "prepared" {
            let mut foreign = std::process::Command::new("unshare")
                .args([
                    "--user",
                    "--map-root-user",
                    "--mount",
                    "--pid",
                    "--fork",
                    "--kill-child=KILL",
                    "--mount-proc",
                    "--",
                ])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "plugin::install::tests::staging_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("AOE_STAGING_CRASH_ROOT", &root)
                .env("AOE_STAGING_CRASH_PHASE", "foreign")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = foreign.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    let _ = foreign.kill();
                    let _ = foreign.wait();
                    panic!("foreign PID namespace recovery did not finish");
                }
                std::thread::yield_now();
            };
            assert!(
                status.success(),
                "{}",
                std::io::read_to_string(foreign.stderr.take().unwrap()).unwrap()
            );
            assert_eq!(
                anchored
                    .read_regular(Path::new(STAGING_MARKER), 1024 * 1024)
                    .unwrap()
                    .unwrap(),
                original
            );
        }
        let (mut evidence, _, _) = staging_evidence(&anchored).unwrap();
        evidence.owner_pid = std::process::id();
        evidence.owner_birth = crate::process::process_birth_identity(std::process::id())
            .unwrap()
            .unwrap();
        let save_header = |evidence: &StagingEvidence| {
            let mut bytes = serde_json::to_vec(evidence).unwrap();
            bytes.push(b'\n');
            anchored
                .bind_file(std::ffi::OsStr::new(STAGING_MARKER))
                .unwrap()
                .replace(&bytes)
                .unwrap();
        };
        save_header(&evidence);
        assert!(
            recover_staging(&root).is_err(),
            "live owner must not be reclaimed"
        );
        let last = evidence.owner_birth.start_id.pop().unwrap();
        evidence
            .owner_birth
            .start_id
            .push(if last == '0' { '1' } else { '0' });
        save_header(&evidence);
        assert!(
            recover_staging(&root).is_err(),
            "reused PID must not match old custody"
        );
        evidence.owner_pid = daemon.0.id();
        evidence.owner_birth = serde_json::from_slice::<StagingEvidence>(
            original.split(|byte| *byte == b'\n').next().unwrap(),
        )
        .unwrap()
        .owner_birth;
        evidence.custody = crate::process::StagingCustody::Uncertain;
        save_header(&evidence);
        assert!(
            recover_staging(&root).is_err(),
            "unknown native resources must remain Recovery"
        );
        anchored
            .bind_file(std::ffi::OsStr::new(STAGING_MARKER))
            .unwrap()
            .replace(&original[..original.len() - 1])
            .unwrap();
        assert!(
            recover_staging(&root).is_err(),
            "torn journal must remain Recovery"
        );
        let mut publication = original.clone();
        publication.extend_from_slice(b"U\n");
        anchored
            .bind_file(std::ffi::OsStr::new(STAGING_MARKER))
            .unwrap()
            .replace(&publication)
            .unwrap();
        assert!(
            recover_staging(&root).is_err(),
            "publication may have orphaned non-journalled builds"
        );
        anchored
            .bind_file(std::ffi::OsStr::new(STAGING_MARKER))
            .unwrap()
            .replace(&original)
            .unwrap();
        let stage = root.join("plugins/.staging-crash");
        let mount = stage.join("volume");
        let unrelated = root.join("plugins/.staging-unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        let (mut evidence, _, _) = staging_evidence(&anchored).unwrap();
        let may_remove = !legacy
            && plugins_mode & 0o022 == 0
            && [&root.join("plugins"), &stage, &mount].iter().all(|path| {
                std::fs::metadata(path)
                    .and_then(|metadata| metadata.created())
                    .is_ok()
            });
        if plugins_mode & 0o022 != 0 {
            assert!(evidence.removal.is_none());
        }
        if legacy {
            evidence.removal = None;
            save_header(&evidence);
        }
        if phase == "prepared" && !legacy {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("retained"), b"untouched").unwrap();
            let empty_replacement = stage.with_file_name(".staging-replacement");
            std::fs::rename(&stage, &empty_replacement).unwrap();
            std::fs::create_dir_all(&mount).unwrap();
            assert!(recover_staging(&root).is_err());
            assert!(mount.is_dir(), "replacement stage must survive");
            std::fs::remove_dir(&mount).unwrap();
            std::fs::remove_dir(&stage).unwrap();
            std::fs::rename(&empty_replacement, &stage).unwrap();

            for dir in [&stage, &mount] {
                let retained = dir.join("retained");
                std::fs::write(&retained, b"untouched").unwrap();
                assert!(recover_staging(&root).is_err());
                assert_eq!(std::fs::read(&retained).unwrap(), b"untouched");
                assert!(mount.is_dir(), "nonempty staging must remain intact");
                std::fs::remove_file(retained).unwrap();
            }
            let saved = mount.with_file_name("saved");
            std::fs::rename(&mount, &saved).unwrap();
            std::os::unix::fs::symlink(outside.path(), &mount).unwrap();
            assert!(
                recover_staging(&root).is_err(),
                "recovery must not follow a replaced mountpoint"
            );
            assert_eq!(
                std::fs::read(outside.path().join("retained")).unwrap(),
                b"untouched"
            );
            std::fs::remove_file(&mount).unwrap();
            assert!(
                recover_staging(&root).is_err(),
                "missing mount without cleanup intent"
            );
            std::fs::rename(&saved, &mount).unwrap();

            if let Some(proof) = evidence.removal.as_mut() {
                proof.directory_created += Duration::from_secs(1);
                save_header(&evidence);
                assert!(recover_staging(&root).is_err());
                assert!(
                    mount.is_dir(),
                    "device and inode alone cannot authorize removal"
                );
                anchored
                    .bind_file(std::ffi::OsStr::new(STAGING_MARKER))
                    .unwrap()
                    .replace(&original)
                    .unwrap();
            }
        }
        if may_remove && phase == "prepared" {
            use std::os::unix::fs::PermissionsExt;

            let permissions = std::fs::metadata(&mount).unwrap().permissions();
            std::fs::set_permissions(
                &mount,
                std::fs::Permissions::from_mode(permissions.mode() | 0o022),
            )
            .unwrap();
            let error = recover_staging(&root).unwrap_err();
            assert_eq!(
                error.to_string(),
                "anchored directory is not owned or permits shared writes"
            );
            assert!(mount.is_dir(), "shared-writable mount must survive");
            assert_eq!(
                anchored
                    .read_regular(Path::new(STAGING_MARKER), 1024 * 1024)
                    .unwrap()
                    .unwrap(),
                original
            );
            std::fs::set_permissions(&mount, permissions).unwrap();

            crate::session::anchored_fs::FAIL_SYNC_ONCE.set(Some(stage.clone()));
            assert!(recover_stale_staging().is_err());
            assert!(
                stage.is_dir(),
                "cleanup was interrupted after mountpoint removal"
            );
            assert!(!mount.exists());
            assert!(root.join(STAGING_MARKER).exists());
            std::fs::rename(&unrelated, &mount).unwrap();
            assert!(recover_stale_staging().is_err());
            assert!(mount.is_dir(), "a retry must preserve a replacement mount");
            std::fs::rename(&mount, &unrelated).unwrap();
            crate::session::anchored_fs::FAIL_SYNC_ONCE.set(Some(root.join("plugins")));
            assert!(recover_stale_staging().is_err());
            assert!(!stage.exists());
            assert!(root.join(STAGING_MARKER).exists());
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match recover_stale_staging() {
                Ok(()) => break,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "settled staging was not reclaimed: {error:#}"
                    );
                    std::thread::yield_now();
                }
            }
        }
        assert!(check_recovery_evidence().is_ok());
        assert!(begin_operation().is_ok());
        assert!(!root.join(STAGING_MARKER).exists());
        if may_remove {
            assert!(std::fs::symlink_metadata(&stage)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound));
        } else {
            assert!(
                mount.is_dir(),
                "a journal without deletion authority cannot remove staging"
            );
        }
        assert!(
            unrelated.is_dir(),
            "recovery must not scan other staging directories"
        );
    }

    #[test]
    #[serial]
    fn startup_reclaims_preacquisition_after_real_daemon_crash() {
        for mode in [0o755, 0o775] {
            crash_staging_and_recover("prepared", false, mode);
        }
    }

    #[test]
    #[serial]
    fn startup_reclaims_legacy_journal_without_removing_staging_directory() {
        crash_staging_and_recover("prepared", true, 0o755);
    }

    #[test]
    #[serial]
    #[cfg(target_os = "linux")]
    #[ignore = "requires private user/PID/mount namespaces, nsenter, Git, and bubblewrap"]
    fn startup_reclaims_bounded_linux_volume_after_real_daemon_crash() {
        for phase in ["volume", "writers"] {
            crash_staging_and_recover(phase, false, 0o755);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires mount --bind in a private mount namespace"]
    fn journal_cleanup_refuses_same_device_bind_mount() {
        use crate::session::AnchoredDir;

        struct Mounted(PathBuf);
        impl Drop for Mounted {
            fn drop(&mut self) {
                let _ = std::process::Command::new("umount").arg(&self.0).status();
            }
        }

        let fixture = tempfile::tempdir().unwrap();
        let root = AnchoredDir::open(fixture.path()).unwrap();
        let tree = root.create_child(Path::new("tree")).unwrap();
        let outside = root.create_child(Path::new("outside")).unwrap();
        let mount = tree.create_child(Path::new("mount")).unwrap();
        let outside_file = outside.path().join("retain");
        std::fs::write(&outside_file, b"outside").unwrap();
        assert_eq!(tree.identity().unwrap().0, outside.identity().unwrap().0);

        let status = std::process::Command::new("mount")
            .arg("--bind")
            .arg(outside.path())
            .arg(mount.path())
            .status()
            .unwrap();
        assert!(status.success());
        let _mount = Mounted(mount.path().to_path_buf());
        let error = remove_owned_tree_checked(
            &root,
            Path::new("tree"),
            publication::Identity::capture(tree.descriptor()).unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("operation crosses a host mount"));
        assert_eq!(std::fs::read(&outside_file).unwrap(), b"outside");
    }

    #[test]
    fn owned_cleanup_unlinks_legacy_and_internal_links_without_changing_targets() {
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
        let fixture = tempfile::tempdir().unwrap();
        let outside = fixture.path().join("user-source");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("retained"), b"user bytes").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o555)).unwrap();
        let before = std::fs::metadata(&outside).unwrap();
        let legacy = fixture.path().join("legacy-dev");
        symlink(&outside, &legacy).unwrap();
        remove_owned_tree(&legacy).unwrap();
        assert!(std::fs::symlink_metadata(&legacy).is_err());
        let owned = fixture.path().join("owned");
        std::fs::create_dir(&owned).unwrap();
        std::fs::create_dir(owned.join("nested")).unwrap();
        std::fs::write(owned.join("nested/payload"), b"owned bytes").unwrap();
        std::fs::set_permissions(owned.join("nested"), std::fs::Permissions::from_mode(0o555))
            .unwrap();
        symlink(&outside, owned.join("outside")).unwrap();
        symlink(fixture.path().join("missing"), owned.join("dangling")).unwrap();
        std::fs::set_permissions(&owned, std::fs::Permissions::from_mode(0o555)).unwrap();
        remove_owned_tree(&owned).unwrap();
        assert!(!owned.exists());
        let after = std::fs::metadata(&outside).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(after.permissions().mode(), before.permissions().mode());
        assert_eq!(
            std::fs::read(outside.join("retained")).unwrap(),
            b"user bytes"
        );
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    async fn readonly_installation_cleanup(case: &str) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let _reload = super::super::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        let effects = tempfile::tempdir().unwrap();
        let node = super::super::write_fake_node(effects.path(), "node");
        let _node = crate::session::test_support::EnvGuard::set(&[("AOE_ACP_NODE", &node)]);
        let id = "acme.readonly";
        let base = format!(
            "id = \"{id}\"\nname = \"Readonly\"\nversion = \"1.0.0\"\napi_version = 2\ncapabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"command\"\ncommand = [\"sh\"]\nsystem = true\n"
        );
        let manifest = source.path().join("aoe-plugin.toml");
        std::fs::write(&manifest, &base).unwrap();
        std::fs::create_dir_all(source.path().join("assets/nested")).unwrap();
        std::fs::write(source.path().join("assets/nested/payload"), b"retained").unwrap();
        let directories = [
            source.path().to_path_buf(),
            source.path().join("assets"),
            source.path().join("assets/nested"),
        ];
        for directory in &directories {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let source_identity = std::fs::metadata(source.path()).unwrap();
        let input = source.path().to_str().unwrap();
        let final_dir = super::super::plugins_dir().unwrap().join(id);
        if case == "failed-install" {
            std::fs::write(
                &manifest,
                format!("{base}\n[[runtime.build]]\ncommand = [\"sh\", \"-c\", \"exit 1\"]\n"),
            )
            .unwrap();
            let result = install(input, true, false).await;
            assert!(!super::super::execution::needs_recovery(&result));
            assert!(format!("{:#}", result.unwrap_err()).contains("build step"));
            assert!(!final_dir.exists());
            assert!(!Config::load().unwrap().plugins.contains_key(id));
            assert!(Lockfile::load().unwrap().get(id).is_none());
            assert!(begin_operation().is_ok());
            for directory in &directories {
                assert_eq!(
                    std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                    0o555
                );
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            return;
        }
        install(input, true, false).await.unwrap();
        assert_eq!(
            std::fs::metadata(final_dir.join("assets/nested"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
        if case != "uninstall" {
            let before_hash = super::super::integrity::tree_hash(&final_dir).unwrap();
            let before_config = serde_json::to_value(&Config::load().unwrap().plugins[id]).unwrap();
            let before_lock = Lockfile::load().unwrap().get(id).cloned();
            let mut next = base.replace("1.0.0", "2.0.0");
            if case == "failed-build" {
                next.push_str("\n[[runtime.build]]\ncommand = [\"sh\", \"-c\", \"exit 1\"]\n");
            }
            std::fs::write(&manifest, &next).unwrap();
            let fingerprint = match preview_update(id, true).await.unwrap() {
                UpdatePreview::SafeUpdate { fingerprint, .. } => fingerprint,
                UpdatePreview::ConsentRequired { consent, .. } => consent.fingerprint,
                other => panic!("expected update: {other:?}"),
            };
            let result = apply_update(id, Some(fingerprint), true, OperationLog::Inherit).await;
            if case == "failed-build" {
                assert!(!super::super::execution::needs_recovery(&result));
                assert!(format!("{:#}", result.unwrap_err()).contains("build step"));
                assert_eq!(
                    super::super::integrity::tree_hash(&final_dir).unwrap(),
                    before_hash
                );
                assert_eq!(
                    serde_json::to_value(&Config::load().unwrap().plugins[id]).unwrap(),
                    before_config
                );
                assert_eq!(Lockfile::load().unwrap().get(id), before_lock.as_ref());
            } else {
                assert_eq!(result.unwrap().version, "2.0.0");
            }
            assert!(!final_dir.with_file_name(format!("{id}.bak")).exists());
        }
        assert_eq!(
            std::fs::metadata(&final_dir).unwrap().permissions().mode() & 0o777,
            0o555
        );
        uninstall(id).unwrap();
        assert!(!final_dir.exists());
        assert!(!Config::load().unwrap().plugins.contains_key(id));
        assert!(Lockfile::load().unwrap().get(id).is_none());
        let after = std::fs::metadata(source.path()).unwrap();
        assert_eq!(
            (after.dev(), after.ino()),
            (source_identity.dev(), source_identity.ino())
        );
        for directory in &directories {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o555
            );
        }
        assert_eq!(
            std::fs::read(source.path().join("assets/nested/payload")).unwrap(),
            b"retained"
        );
        assert!(begin_operation().is_ok());
        for directory in &directories {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[tokio::test]
    #[serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    async fn readonly_nonempty_installation_uninstalls() {
        readonly_installation_cleanup("uninstall").await;
    }

    #[tokio::test]
    #[serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    async fn readonly_nonempty_failed_install_removes_uncommitted_tree() {
        readonly_installation_cleanup("failed-install").await;
    }

    #[tokio::test]
    #[serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    async fn readonly_nonempty_failed_build_restores_prior_installation() {
        readonly_installation_cleanup("failed-build").await;
    }

    #[tokio::test]
    #[serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    async fn readonly_nonempty_update_removes_old_backup() {
        readonly_installation_cleanup("update").await;
    }

    #[test]
    #[serial]
    fn staging_marker_blocks_admission_after_process_ownership_is_lost() {
        let _home = isolate_app_dir();
        let marker = crate::session::get_app_dir().unwrap().join(STAGING_MARKER);
        std::fs::write(&marker, "staging acquisition interrupted").unwrap();
        assert!(check_recovery_evidence().is_err());
        assert!(begin_operation().is_err());
        register_staging(&marker);
        assert!(check_recovery_evidence().is_ok());
        assert!(begin_operation().is_err());
        unregister_staging(&marker);
        assert!(check_recovery_evidence().is_err());
        std::fs::remove_file(marker).unwrap();
        assert!(begin_operation().is_ok());
    }

    #[tokio::test]
    #[serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    async fn staging_volume_failed_publication_copy_preserves_prior_install() {
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(
            source.path().join("aoe-plugin.toml"),
            "id = \"acme.copy\"\nname = \"Copy\"\nversion = \"1.0.0\"\napi_version = 2\n",
        )
        .unwrap();
        std::fs::write(source.path().join("worker"), b"new").unwrap();
        let operation = begin_operation().unwrap();
        let prepared = prepare_install(
            source.path().to_str().unwrap(),
            false,
            Some(operation.clone()),
        )
        .await
        .unwrap();
        let final_dir = super::super::plugins_dir().unwrap().join("acme.copy");
        std::fs::create_dir(&final_dir).unwrap();
        std::fs::write(final_dir.join("old"), b"old").unwrap();
        let prior = PriorMetadata::capture("acme.copy").unwrap();
        let changing = prepared.fetched.tree.join("worker");
        let mut gate = fetch::test_gate::install(&changing, "copy");
        let destination = final_dir.clone();
        let task = tokio::task::spawn_blocking(move || {
            let result = replace_and_build(
                "acme.copy",
                &prepared.fetched,
                &destination,
                &NodeEnv::new(),
                &OperationLog::Inherit,
                || panic!("metadata must not commit before a verified copy"),
                &prior,
            );
            prepared.fetched.close().unwrap();
            result
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut gate.entered)
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_file(changing).unwrap();
        gate.release();
        assert!(task.await.unwrap().is_err());
        assert_eq!(std::fs::read(final_dir.join("old")).unwrap(), b"old");
        assert!(!final_dir.with_file_name("acme.copy.bak").exists());
        assert_eq!(std::fs::read_dir(final_dir).unwrap().count(), 1);
        assert!(std::fs::read_dir(super::super::plugins_dir().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".publish-")));
        drop(operation);
        assert!(begin_operation().is_ok());
    }

    #[tokio::test]
    #[serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    async fn staging_volume_readonly_publication_exits_clean_without_displacing_prior_tree() {
        use std::os::unix::fs::PermissionsExt;
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(
            source.path().join("aoe-plugin.toml"),
            "id = \"acme.copy\"\nname = \"Copy\"\nversion = \"1.0.0\"\napi_version = 2\n",
        )
        .unwrap();
        let readonly = source.path().join("readonly");
        std::fs::create_dir(&readonly).unwrap();
        std::fs::write(readonly.join("payload"), b"approved").unwrap();
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555)).unwrap();
        let operation = begin_operation().unwrap();
        let prepared = prepare_install(
            source.path().to_str().unwrap(),
            false,
            Some(operation.clone()),
        )
        .await
        .unwrap();
        let final_dir = super::super::plugins_dir().unwrap().join("acme.copy");
        std::fs::create_dir(&final_dir).unwrap();
        std::fs::write(final_dir.join("old"), b"old").unwrap();
        let before = super::super::integrity::tree_hash(&final_dir).unwrap();
        let prior = PriorMetadata::capture("acme.copy").unwrap();
        let backup = final_dir.with_file_name("acme.copy.bak");
        let mut backup_gate = fetch::test_gate::install(&final_dir, "publication-backup");
        let backup_destination = final_dir.clone();
        let backup_prior = PriorMetadata::capture("acme.copy").unwrap();
        let backup_task = tokio::task::spawn_blocking(move || {
            let error = replace_and_build(
                "acme.copy",
                &prepared.fetched,
                &backup_destination,
                &NodeEnv::new(),
                &OperationLog::Inherit,
                || panic!("failed backup must not commit"),
                &backup_prior,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("backing up current"));
            prepared
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut backup_gate.entered)
            .await
            .unwrap()
            .unwrap();
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("obstruction"), b"retained").unwrap();
        backup_gate.release();
        let prepared = backup_task.await.unwrap();
        remove_owned_tree(&backup).unwrap();
        let destination = final_dir.clone();
        tokio::task::spawn_blocking(move || {
            let fetched = &prepared.fetched;
            // All these exits leave a complete, nonempty read-only copy.
            let exists = move_into_place(fetched, &destination).unwrap_err();
            assert!(format!("{exists:#}").contains("already exists"));
            let rename: Result<()> = with_publication(fetched, &destination, |tree| {
                std::fs::rename(tree, destination.join("missing/child"))?;
                Ok(())
            });
            assert!(rename.is_err());
            let panic: Result<()> = with_publication(fetched, &destination, |_| {
                panic!("controlled publication panic")
            });
            assert!(super::super::execution::needs_recovery(&panic));
            std::fs::write(fetched.tree.join("readonly/payload"), b"changed").unwrap();
            let mismatch = replace_and_build(
                "acme.copy",
                fetched,
                &destination,
                &NodeEnv::new(),
                &OperationLog::Inherit,
                || panic!("unverified copy must not commit"),
                &prior,
            )
            .unwrap_err();
            assert!(format!("{mismatch:#}").contains("differs from the approved staging tree"));
            prepared.fetched.close().unwrap();
        })
        .await
        .unwrap();
        assert_eq!(
            super::super::integrity::tree_hash(&final_dir).unwrap(),
            before
        );
        assert!(!final_dir.with_file_name("acme.copy.bak").exists());
        assert!(std::fs::read_dir(super::super::plugins_dir().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".publish-")));
        assert_eq!(
            std::fs::metadata(&readonly).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert_eq!(
            std::fs::read(readonly.join("payload")).unwrap(),
            b"approved"
        );
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(operation);
        assert!(begin_operation().is_ok());
    }

    #[tokio::test]
    #[serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    async fn staging_volume_publication_cleanup_failure_retains_path_and_exclusion() {
        use nix::sys::stat::Mode;
        let _reload = super::super::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(
            source.path().join("aoe-plugin.toml"),
            "id = \"acme.publish\"\nname = \"Publish\"\nversion = \"1.0.0\"\napi_version = 2\n",
        )
        .unwrap();
        let plugins = super::super::plugins_dir().unwrap();
        let final_dir = plugins.join("acme.publish");
        let mut cleanup = fetch::test_gate::install(&final_dir, "publication-cleanup");
        let mut preparation = fetch::test_gate::install(&plugins, "source-prepared");
        let input = source.path().to_str().unwrap().to_owned();
        let task = tokio::spawn(async move { install(&input, true, false).await });
        preparation.wait_for_preparation().await;
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut cleanup.entered)
            .await
            .unwrap()
            .unwrap();
        let publication = std::fs::read_dir(&plugins)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".publish-")
            })
            .unwrap();
        let obstruction = publication.join("unsafe-fifo");
        nix::unistd::mkfifo(&obstruction, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        cleanup.release();
        let error = task.await.unwrap().unwrap_err();
        assert!(error
            .downcast_ref::<super::super::execution::RecoveryRequired>()
            .is_some());
        assert!(format!("{error:#}").contains(publication.to_str().unwrap()));
        assert!(publication.is_dir());
        assert!(final_dir.join("aoe-plugin.toml").is_file());
        assert!(crate::session::get_app_dir()
            .unwrap()
            .join(".plugin-recovery-required")
            .is_file());
        assert!(begin_operation().is_err());
        std::fs::remove_file(obstruction).unwrap();
        remove_owned_tree(&publication).unwrap();
    }

    #[tokio::test]
    #[serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    async fn staging_volume_cleanup_failure_retains_marker_and_operation_lock() {
        let _home = isolate_app_dir();
        update_config(|config| config.session.plugin_staging_sandbox = true).unwrap();
        let source = tempfile::tempdir().unwrap();
        let repo = source.path().join("acme/cleanup.git");
        std::fs::create_dir_all(&repo).unwrap();
        let _git_paths = crate::session::test_support::EnvGuard::unset(&[
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_TEMPLATE_DIR",
        ]);
        let clone_base = format!("file://{}", source.path().display());
        let _git = crate::session::test_support::EnvGuard::set(&[
            ("AOE_GITHUB_CLONE_BASE", clone_base.as_str()),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ]);
        std::fs::write(
            repo.join("aoe-plugin.toml"),
            "id = \"acme.cleanup\"\nname = \"Cleanup\"\nversion = \"1.0.0\"\napi_version = 2\n",
        )
        .unwrap();
        for args in [
            ["init", "--initial-branch=main"].as_slice(),
            ["add", "aoe-plugin.toml"].as_slice(),
            [
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-m",
                "fixture",
            ]
            .as_slice(),
        ] {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let operation = begin_operation().unwrap();
        let prepared = prepare_install("gh:acme/cleanup@main", false, Some(operation.clone()))
            .await
            .unwrap();
        let marker = crate::session::get_app_dir().unwrap().join(STAGING_MARKER);
        std::fs::remove_file(&marker).unwrap();
        std::fs::create_dir(&marker).unwrap();
        let result = prepared.fetched.cleanup().await;
        assert!(super::super::execution::needs_recovery(&result));
        drop(operation);
        assert!(marker.is_dir());
        assert!(check_recovery_evidence().is_err());
        assert!(begin_operation().is_err());
    }

    #[tokio::test]
    #[serial]
    async fn admission_rechecks_recovery_after_prior_owner_releases_exclusion() {
        let _home = isolate_app_dir();
        let root = crate::session::get_app_dir().unwrap();
        let owner = begin_operation().unwrap();
        let mut gate = fetch::test_gate::install(&root, "operation-admission");
        let contender = tokio::task::spawn_blocking(begin_operation);
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut gate.entered)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(
            root.join(".plugin-recovery-required"),
            "prior owner could not restore",
        )
        .unwrap();
        drop(owner);
        gate.release();
        let error = contender
            .await
            .unwrap()
            .err()
            .expect("recovery must fence the admitted contender");
        assert!(format!("{error:#}").contains("recovery evidence"));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn local_release_binary_install_validates_target_before_granting() {
        use std::os::unix::fs::PermissionsExt;
        let _reload = super::super::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        for dev in [false, true] {
            for target in ["missing", "directory", "non-executable", "executable"] {
                let source = tempfile::tempdir().unwrap();
                let id = format!("acme.binary-{}-{target}", if dev { "dev" } else { "copy" });
                std::fs::write(source.path().join("aoe-plugin.toml"), format!(
                    "id = \"{id}\"\nname = \"Binary\"\nversion = \"1.0.0\"\napi_version = 2\ncapabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"release-binary\"\nasset = \"worker\"\nbin = \"worker\"\n"
                )).unwrap();
                let worker = source.path().join("worker");
                match target {
                    "directory" => std::fs::create_dir(&worker).unwrap(),
                    "non-executable" | "executable" => {
                        std::fs::write(&worker, "#!/bin/sh\nexit 0\n").unwrap();
                        std::fs::set_permissions(
                            &worker,
                            std::fs::Permissions::from_mode(if target == "executable" {
                                0o700
                            } else {
                                0o600
                            }),
                        )
                        .unwrap();
                    }
                    _ => {}
                }
                update_config(|config| {
                    let row = config.plugins.entry(id.clone()).or_default();
                    row.enabled = false;
                    row.settings.insert(
                        "retained".into(),
                        toml::Value::String("user setting".into()),
                    );
                })
                .unwrap();
                let before = serde_json::to_value(&Config::load().unwrap().plugins[&id]).unwrap();
                let result = install(source.path().to_str().unwrap(), true, dev).await;
                if target == "executable" {
                    result.unwrap();
                    let config = Config::load().unwrap();
                    assert!(!config.plugins[&id].enabled);
                    assert!(config.plugins[&id].grant.is_some());
                    assert!(Lockfile::load().unwrap().get(&id).is_some());
                } else {
                    assert!(
                        !super::super::execution::needs_recovery(&result),
                        "{dev}/{target}: {:#}",
                        result.as_ref().unwrap_err()
                    );
                    let error = result.unwrap_err();
                    assert!(
                        format!("{error:#}")
                            .contains("release-binary worker is not runnable after install"),
                        "{dev}/{target}: {error:#}"
                    );
                    assert_eq!(
                        serde_json::to_value(&Config::load().unwrap().plugins[&id]).unwrap(),
                        before
                    );
                    assert!(Lockfile::load().unwrap().get(&id).is_none());
                    assert!(!super::super::plugins_dir().unwrap().join(&id).exists());
                }
                assert!(
                    source.path().join("aoe-plugin.toml").is_file(),
                    "dev source remains user-owned"
                );
                let operation =
                    begin_operation().unwrap_or_else(|error| panic!("{dev}/{target}: {error:#}"));
                drop(operation);
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn metadata_failures_restore_target_tree_and_exact_prior_intent() {
        let _reload = super::super::ReloadRegistryOnDrop;
        for operation in ["install", "update", "repair"] {
            for boundary in ["config", "lock", "restoration"] {
                for dev in [false, true] {
                    let _home = isolate_app_dir();
                    let source = tempfile::tempdir().unwrap();
                    let id = "acme.metadata";
                    let manifest = |version| {
                        format!("id = \"{id}\"\nname = \"Metadata\"\nversion = \"{version}\"\napi_version = 2\n")
                    };
                    std::fs::write(source.path().join("aoe-plugin.toml"), manifest("1.0.0"))
                        .unwrap();
                    let input = source.path().display().to_string();
                    if operation != "install" {
                        install(&input, true, dev).await.unwrap();
                    }
                    update_config(|config| {
                        let row = config.plugins.entry(id.into()).or_default();
                        row.enabled = false;
                        row.settings.insert(
                            "retained".into(),
                            toml::Value::String("exact setting".into()),
                        );
                        row.dismissed_update = Some("prior dismissal".into());
                    })
                    .unwrap();
                    let prior_config =
                        serde_json::to_value(&Config::load().unwrap().plugins[id]).unwrap();
                    let prior_lock = Lockfile::load().unwrap().get(id).cloned();
                    let final_dir = super::super::plugins_dir().unwrap().join(id);
                    let prior_tree = if operation == "update" {
                        Some(std::fs::read(final_dir.join("aoe-plugin.toml")).unwrap())
                    } else {
                        if operation == "repair" {
                            std::fs::remove_dir_all(&final_dir).unwrap();
                        }
                        None
                    };
                    std::fs::write(source.path().join("aoe-plugin.toml"), manifest("2.0.0"))
                        .unwrap();
                    let fingerprint = if operation == "install" {
                        preview_install(&input, dev).await.unwrap().fingerprint
                    } else {
                        match preview_update(id, true).await.unwrap() {
                            UpdatePreview::SafeUpdate { fingerprint, .. } => fingerprint,
                            UpdatePreview::ConsentRequired { consent, .. } => consent.fingerprint,
                            other => panic!("expected repair/update candidate: {other:?}"),
                        }
                    };
                    let mut commit = fetch::test_gate::install(&final_dir, "metadata-commit");
                    let mut rollback = fetch::test_gate::install(&final_dir, "rollback");
                    let mut preparation = fetch::test_gate::install(
                        &super::super::plugins_dir().unwrap(),
                        "source-prepared",
                    );
                    let task = tokio::spawn(async move {
                        if operation == "install" {
                            apply_install(&input, &fingerprint, dev, OperationLog::Inherit).await
                        } else {
                            apply_update(id, Some(fingerprint), true, OperationLog::Inherit).await
                        }
                    });
                    preparation.wait_for_preparation().await;
                    tokio::time::timeout(std::time::Duration::from_secs(5), &mut commit.entered)
                        .await
                        .unwrap()
                        .unwrap();
                    update_config(|config| {
                        config.default_profile = "concurrent-unrelated-edit".into()
                    })
                    .unwrap();
                    let path = if boundary == "config" {
                        crate::session::config::config_path().unwrap()
                    } else {
                        crate::session::get_app_dir().unwrap().join("plugins.lock")
                    };
                    let rescue = path.with_extension("saved");
                    let existed = path.exists();
                    if existed {
                        std::fs::rename(&path, &rescue).unwrap();
                    }
                    std::fs::create_dir(&path).unwrap();
                    commit.release();
                    tokio::time::timeout(std::time::Duration::from_secs(5), &mut rollback.entered)
                        .await
                        .unwrap()
                        .unwrap();
                    if boundary != "restoration" {
                        std::fs::remove_dir(&path).unwrap();
                        if existed {
                            std::fs::rename(&rescue, &path).unwrap();
                        }
                    }
                    rollback.release();
                    let error = task.await.unwrap().unwrap_err();
                    if boundary == "restoration" {
                        assert!(
                            error
                                .downcast_ref::<super::super::execution::RecoveryRequired>()
                                .is_some(),
                            "{error:#}"
                        );
                        assert!(crate::session::get_app_dir()
                            .unwrap()
                            .join(".plugin-recovery-required")
                            .exists());
                    } else {
                        assert!(!super::super::execution::needs_recovery::<()>(&Err(error)));
                        assert_eq!(Lockfile::load().unwrap().get(id), prior_lock.as_ref());
                    }
                    assert_eq!(
                        serde_json::to_value(&Config::load().unwrap().plugins[id]).unwrap(),
                        prior_config
                    );
                    assert_eq!(
                        Config::load().unwrap().default_profile,
                        "concurrent-unrelated-edit"
                    );
                    assert_eq!(
                        std::fs::read(final_dir.join("aoe-plugin.toml")).ok(),
                        prior_tree
                    );
                    assert!(source.path().join("aoe-plugin.toml").exists());
                }
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn post_commit_cleanup_failure_never_restores_old_tree_over_commit() {
        let _reload = super::super::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        let manifest = |version| {
            format!("id = \"acme.cleanup\"\nname = \"Cleanup\"\nversion = \"{version}\"\napi_version = 2\n")
        };
        std::fs::write(source.path().join("aoe-plugin.toml"), manifest("1.0.0")).unwrap();
        install(source.path().to_str().unwrap(), true, false)
            .await
            .unwrap();
        std::fs::write(source.path().join("aoe-plugin.toml"), manifest("2.0.0")).unwrap();
        let fingerprint = match preview_update("acme.cleanup", true).await.unwrap() {
            UpdatePreview::SafeUpdate { fingerprint, .. } => fingerprint,
            other => panic!("expected safe update: {other:?}"),
        };
        let final_dir = super::super::plugins_dir().unwrap().join("acme.cleanup");
        let backup = final_dir.with_file_name("acme.cleanup.bak");
        let rescue = source.path().join("old-tree");
        let mut cleanup = fetch::test_gate::install(&final_dir, "backup-cleanup");
        let mut preparation =
            fetch::test_gate::install(&super::super::plugins_dir().unwrap(), "source-prepared");
        let task = tokio::spawn(async move {
            apply_update(
                "acme.cleanup",
                Some(fingerprint),
                true,
                OperationLog::Inherit,
            )
            .await
        });
        preparation.wait_for_preparation().await;
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut cleanup.entered)
            .await
            .unwrap()
            .unwrap();
        std::fs::rename(&backup, &rescue).unwrap();
        std::fs::write(&backup, "filesystem cleanup obstruction").unwrap();
        cleanup.release();
        let error = task.await.unwrap().unwrap_err();
        assert!(error
            .downcast_ref::<super::super::execution::RecoveryRequired>()
            .is_some());
        assert!(format!("{error:#}").contains("committed"));
        assert_eq!(
            PluginManifest::from_toml_str(
                &std::fs::read_to_string(final_dir.join("aoe-plugin.toml")).unwrap()
            )
            .unwrap()
            .version,
            "2.0.0"
        );
        assert_eq!(
            Lockfile::load()
                .unwrap()
                .get("acme.cleanup")
                .unwrap()
                .version,
            "2.0.0"
        );
        assert!(backup.is_file() && rescue.is_dir());
        assert!(crate::session::get_app_dir()
            .unwrap()
            .join(".plugin-recovery-required")
            .exists());
        assert!(begin_operation().is_err());
    }

    #[test]
    fn web_recovery_guidance_preserves_authority_refusals() {
        use crate::acp::client::HttpError;
        use crate::daemon::DaemonClientError;
        use reqwest::StatusCode;
        for (error, guidance) in [
            (HttpError::Transport, true),
            (HttpError::Daemon(DaemonClientError::UnixTransport), true),
            (HttpError::Daemon(DaemonClientError::Timeout), true),
            (HttpError::Daemon(DaemonClientError::PeerIdentity), false),
            (HttpError::Unauthorized, false),
            (HttpError::ReadOnly, false),
            (
                HttpError::Server {
                    status: StatusCode::FORBIDDEN,
                    body: "policy refused".into(),
                },
                false,
            ),
            (
                HttpError::Server {
                    status: StatusCode::CONFLICT,
                    body: "lifecycle_locked".into(),
                },
                false,
            ),
            (
                HttpError::Server {
                    status: StatusCode::NOT_FOUND,
                    body: "missing route".into(),
                },
                true,
            ),
        ] {
            let original = error.to_string();
            let result = web_toggle_error(error.into());
            assert!(result.downcast_ref::<HttpError>().is_some());
            assert_eq!(format!("{result:#}").contains("aoe serve --stop"), guidance);
            if !guidance {
                assert_eq!(result.to_string(), original);
            }
        }
    }

    fn ui(slot: UiSlot, id: &str) -> UiContribution {
        UiContribution {
            slot,
            id: id.to_string(),
            title: None,
            icon: None,
            entry: None,
            deep_link: false,
        }
    }

    fn manifest_with_aoe_version(range: Option<&str>) -> PluginManifest {
        let aoe = range
            .map(|r| format!("aoe_version = \"{r}\"\n"))
            .unwrap_or_default();
        PluginManifest::from_toml_str(&format!(
            "id = \"acme.thing\"\nname = \"Thing\"\nversion = \"1.0.0\"\napi_version = 4\n{aoe}"
        ))
        .unwrap()
    }

    /// Declining must neither provision Node nor build nor touch the installed
    /// version; the approved control proves the observations are connected.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn a_declined_update_neither_provisions_nor_builds() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _home = isolate_app_dir();
        let src = tempfile::tempdir().unwrap();
        let manifest = |version: &str, build: &str| {
            format!(
                "id = \"acme.upd\"\nname = \"Upd\"\nversion = \"{version}\"\napi_version = 2\n\
                 capabilities = [\"runtime.worker\"]\n\n[runtime]\nkind = \"command\"\n\
                 command = [\"sh\"]\nsystem = true\n{build}"
            )
        };
        let manifest_path = src.path().join("aoe-plugin.toml");
        std::fs::write(&manifest_path, manifest("0.1.0", "")).unwrap();
        install(src.path().to_str().unwrap(), true, false)
            .await
            .unwrap();
        let installed = super::super::plugins_dir().unwrap().join("acme.upd");
        let installed_version = || {
            Lockfile::load()
                .unwrap()
                .get("acme.upd")
                .unwrap()
                .version
                .clone()
        };

        std::fs::write(
            &manifest_path,
            manifest(
                "0.2.0",
                "\n[[runtime.build]]\ncommand = [\"cp\", \"aoe-plugin.toml\", \"built\"]\n",
            ),
        )
        .unwrap();
        let provisioned = AtomicUsize::new(0);
        let provision = |_: PluginManifest| {
            provisioned.fetch_add(1, Ordering::SeqCst);
            std::future::ready(NodeEnv::new())
        };

        let operation = begin_operation().unwrap();
        let declined = prepare_update("acme.upd", true, Some(operation.clone()))
            .await
            .unwrap();
        assert!(declined.needs_consent);
        let err = finish_update(declined, None, operation, provision)
            .await
            .err();
        assert!(err.is_some_and(|e| e.to_string().contains("update cancelled")));
        assert_eq!(provisioned.load(Ordering::SeqCst), 0);
        assert!(!installed.join("built").exists());
        assert_eq!(installed_version(), "0.1.0");

        let operation = begin_operation().unwrap();
        let approved = prepare_update("acme.upd", true, Some(operation.clone()))
            .await
            .unwrap();
        let grant = CapabilityGrant {
            manifest_hash: approved.manifest_hash.clone(),
            capabilities: approved.capabilities.clone(),
            granted_at: crate::time::Timestamp::now(),
        };
        finish_update(approved, Some(grant), operation, provision)
            .await
            .unwrap();
        assert_eq!(provisioned.load(Ordering::SeqCst), 1);
        assert!(installed.join("built").exists());
        assert_eq!(installed_version(), "0.2.0");
    }

    #[test]
    fn only_manifests_that_build_or_run_a_non_system_worker_need_node() {
        let cases = [
            (None, false),
            (Some("kind = \"release-binary\"\nasset = \"w\""), false),
            (
                Some("kind = \"command\"\ncommand = [\"w\"]\nsystem = true"),
                false,
            ),
            (Some("kind = \"command\"\ncommand = [\"bin/w\"]"), true),
            (
                Some(
                    "kind = \"command\"\ncommand = [\"w\"]\nsystem = true\n\
                     build = [{ command = [\"npm\", \"ci\"] }]",
                ),
                true,
            ),
        ];
        for (runtime, expected) in cases {
            let runtime = runtime
                .map(|r| format!("capabilities = [\"runtime.worker\"]\n[runtime]\n{r}"))
                .unwrap_or_default();
            let manifest = PluginManifest::from_toml_str(&format!(
                "id = \"acme.thing\"\nname = \"Thing\"\nversion = \"1.0.0\"\napi_version = 4\n{runtime}"
            ))
            .unwrap();
            assert_eq!(crate::plugin::needs_node(&manifest), expected, "{runtime}");
        }
    }

    #[test]
    fn reject_incompatible_host_blocks_out_of_range_and_allows_in_range() {
        let in_range = manifest_with_aoe_version(Some(">=1.0.0, <2.0.0"));
        assert!(reject_incompatible_host(&in_range).is_ok());

        let out = manifest_with_aoe_version(Some(">=2.0.0"));
        let err = reject_incompatible_host(&out).unwrap_err().to_string();
        assert!(err.contains("acme.thing"), "{err}");
        assert!(err.contains("plugin requires aoe"), "{err}");

        assert!(reject_incompatible_host(&manifest_with_aoe_version(None)).is_ok());
    }

    #[test]
    fn install_consent_required_for_caps_build_ui_or_agent_integrations() {
        assert!(!install_needs_consent(&[], &[], &[], &[]));
        assert!(install_needs_consent(&["net".to_string()], &[], &[], &[]));
        assert!(install_needs_consent(
            &[],
            &[],
            &[ui(UiSlot::StatusBar, "s")],
            &[],
        ));
        assert!(install_needs_consent(
            &[],
            &[],
            &[],
            &[AgentIntegrationContribution {
                tool: "agent".into(),
                provider_versions: Vec::new(),
                command: Some("agent --flag".into()),
                replies: false,
                ..Default::default()
            }],
        ));
    }

    #[test]
    fn disclosure_integrations_escapes_commands_for_display_but_keeps_manifest_raw() {
        let raw = AgentIntegrationContribution {
            tool: "session-helper".into(),
            provider_versions: Vec::new(),
            command: Some("x\u{202e}tail".into()),
            replies: false,
            ..Default::default()
        };
        let disclosed = disclosure_integrations(std::slice::from_ref(&raw));
        // The consent that the CLI/TUI/web serialize carries the escaped form.
        assert_eq!(disclosed[0].command.as_deref(), Some("x\\u{202e}tail"));
        // The manifest / launch command stays raw.
        assert_eq!(raw.command.as_deref(), Some("x\u{202e}tail"));
    }

    #[test]
    fn agent_declaration_changes_require_consent_but_removal_does_not() {
        use aoe_plugin_api::agent_integration::{
            AgentDetectionDeclaration, AgentDetectionMode, AgentKeyDeclaration,
            AgentLaunchDeclaration, AgentReplyDeclaration,
        };
        let base = AgentIntegrationContribution {
            tool: "helper".into(),
            command: Some("helper".into()),
            ..Default::default()
        };
        let previous = std::slice::from_ref(&base);
        assert!(!agent_integrations_changed(Some(previous), previous));
        assert!(!agent_integrations_changed(Some(previous), &[]));
        assert!(agent_integrations_changed(None, previous));
        let mut changed = base.clone();
        changed.command = None;
        assert!(agent_integrations_changed(Some(previous), &[changed]));
        let mut changed = base.clone();
        changed.launch = Some(AgentLaunchDeclaration {
            container_env: [("AGENT_HOME".into(), "/state".into())]
                .into_iter()
                .collect(),
            ..Default::default()
        });
        assert!(agent_integrations_changed(Some(previous), &[changed]));
        let mut changed = base.clone();
        changed.detection = Some(AgentDetectionDeclaration {
            mode: AgentDetectionMode::NativeOnly,
        });
        assert!(agent_integrations_changed(Some(previous), &[changed]));
        let mut changed = base.clone();
        changed.reply_fallback = Some(AgentReplyDeclaration {
            allow: vec![AgentKeyDeclaration::Literal { value: "y".into() }],
            deny: vec![AgentKeyDeclaration::Literal { value: "n".into() }],
            allow_always: None,
        });
        assert!(agent_integrations_changed(Some(previous), &[changed]));
        let mut changed = base.clone();
        changed.replies = true;
        assert!(agent_integrations_changed(Some(previous), &[changed]));
    }

    /// Uninstall removes only a configured external plugin's directory: a
    /// malformed id or a bare plugins subdirectory is refused untouched, and a
    /// configured plugin whose directory is gone still has its config removed.
    #[test]
    #[serial]
    fn uninstall_only_removes_installed_external_plugins() {
        let _home = isolate_app_dir();
        let plugins = super::super::plugins_dir().unwrap();
        let jobs = plugins.join("jobs");
        let keep = plugins.join("keepdir");
        for dir in [&jobs, &keep] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("keep.log"), b"keep").unwrap();
        }

        for (id, needle) in [
            ("jobs/../keepdir", "invalid plugin id"),
            ("jobs", "not an installed external plugin"),
        ] {
            let err = uninstall(id).unwrap_err().to_string();
            assert!(err.contains(needle), "{id}: {err}");
        }
        for dir in [&jobs, &keep] {
            assert!(
                dir.join("keep.log").exists(),
                "{} was removed",
                dir.display()
            );
        }

        update_config(|config| {
            config.plugins.insert(
                "acme.thing".to_string(),
                PluginConfig {
                    source: Some("gh:acme/thing".to_string()),
                    ..PluginConfig::default()
                },
            );
        })
        .unwrap();
        uninstall("acme.thing").unwrap();
        assert!(!Config::load().unwrap().plugins.contains_key("acme.thing"));
    }

    /// Dismissing an update for a plugin that is not an installed external
    /// plugin must neither create a blank `[plugins.*]` entry nor record the
    /// dismissal on a source-less one.
    #[test]
    #[serial]
    fn dismiss_update_refuses_plugins_that_are_not_installed_externally() {
        let _home = isolate_app_dir();
        update_config(|config| {
            config
                .plugins
                .insert("acme.thing".to_string(), PluginConfig::default());
        })
        .unwrap();

        for id in ["no.such.plugin", "acme.thing"] {
            let err = dismiss_update(id, "abc123").unwrap_err().to_string();
            assert!(err.contains("not an installed external plugin"), "{err}");
        }
        let config = Config::load().unwrap();
        assert!(!config.plugins.contains_key("no.such.plugin"));
        assert!(config.plugins["acme.thing"].dismissed_update.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn blocking_updates_keep_exclusion_through_cancellation() {
        use std::io::{Read, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::process::Command;
        use std::sync::mpsc;
        use std::time::Duration;

        const ID: &str = "acme.serial";
        const MODE: &str = "AOE_PLUGIN_OPERATION_TEST_MODE";
        const CONTROL: &str = "AOE_PLUGIN_OPERATION_TEST_CONTROL";
        const RECORD: &str = "AOE_PLUGIN_OPERATION_TEST_RECORD";
        const TEST: &str =
            "plugin::install::tests::blocking_updates_keep_exclusion_through_cancellation";
        if let Ok(mode) = std::env::var(MODE) {
            if mode == "build" {
                let mut stream = UnixStream::connect(std::env::var_os(CONTROL).unwrap()).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                stream.write_all(b"B").unwrap();
                let mut release = [0];
                stream.read_exact(&mut release).unwrap();
                assert_ne!(release, [b'F'], "controlled build failure");
            } else {
                assert_eq!(mode, "uninstall");
                assert!(uninstall(ID).is_err());
                std::fs::write(std::env::var_os(RECORD).unwrap(), b"refused").unwrap();
            }
            return;
        }

        struct ReleaseBuild {
            release: Option<mpsc::Sender<()>>,
            controller: Option<std::thread::JoinHandle<bool>>,
            socket: std::path::PathBuf,
            log_reader: Option<std::thread::JoinHandle<std::io::Result<u64>>>,
        }
        impl ReleaseBuild {
            fn finish(mut self) -> bool {
                let _ = self.release.take().unwrap().send(());
                let requested = self.controller.take().unwrap().join().unwrap();
                self.log_reader.take().unwrap().join().unwrap().unwrap();
                requested
            }
        }
        impl Drop for ReleaseBuild {
            fn drop(&mut self) {
                if let Some(release) = self.release.take() {
                    let _ = release.send(());
                }
                if let Some(controller) = self.controller.take() {
                    if let Ok(mut stream) = UnixStream::connect(&self.socket) {
                        let _ = stream.write_all(b"X");
                    }
                    let _ = controller.join();
                }
                if let Some(reader) = self.log_reader.take() {
                    let _ = reader.join();
                }
            }
        }

        let _reload = super::super::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        let effects = tempfile::tempdir_in("/tmp").unwrap();
        let node = super::super::write_fake_node(effects.path(), "node");
        let _node = crate::session::test_support::EnvGuard::set(&[("AOE_ACP_NODE", &node)]);
        let manifest = format!(
            "id = \"{ID}\"\nname = \"Serial\"\nversion = \"1.0.0\"\napi_version = 2\ncapabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"command\"\ncommand = [\"sh\"]\nsystem = true\n"
        );
        let source_manifest = source.path().join("aoe-plugin.toml");
        std::fs::write(&source_manifest, &manifest).unwrap();
        install(source.path().to_str().unwrap(), true, false)
            .await
            .unwrap();
        let executable = std::env::current_exe().unwrap();
        let installed = super::super::plugins_dir()
            .unwrap()
            .join(ID)
            .join("aoe-plugin.toml");

        for (index, fail) in [false, true].into_iter().enumerate() {
            let socket = effects.path().join(format!("build{index}.sock"));
            let listener = UnixListener::bind(&socket).unwrap();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let controller = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                let mut started = [0];
                stream.read_exact(&mut started).unwrap();
                if started != *b"B" {
                    return false;
                }
                let _ = started_tx.send(());
                let requested = release_rx.recv_timeout(Duration::from_secs(30)).is_ok();
                stream.write_all(&[if fail { b'F' } else { b'S' }]).unwrap();
                requested
            });
            let mut release = ReleaseBuild {
                release: Some(release_tx),
                controller: Some(controller),
                socket: socket.clone(),
                log_reader: None,
            };
            let command = vec![
                "env".to_string(),
                format!("{MODE}=build"),
                format!("{CONTROL}={}", socket.display()),
                executable.to_str().unwrap().to_string(),
                "--exact".to_string(),
                TEST.to_string(),
                "--nocapture".to_string(),
            ];
            let next_manifest = format!(
                "{}\n[[runtime.build]]\ncommand = {}\n",
                manifest.replace("1.0.0", &format!("{}.0.0", index + 2)),
                serde_json::to_string(&command).unwrap()
            );
            std::fs::write(&source_manifest, &next_manifest).unwrap();
            let before_manifest = std::fs::read(&installed).unwrap();
            let before_config = serde_json::to_value(&Config::load().unwrap().plugins[ID]).unwrap();
            let fingerprint = match preview_update(ID, true).await.unwrap() {
                UpdatePreview::ConsentRequired { consent, .. } => consent.fingerprint,
                other => panic!("build update needs approval: {other:?}"),
            };
            let (mut log_reader, log_writer) = UnixStream::pair().unwrap();
            log_reader
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            release.log_reader = Some(std::thread::spawn(move || {
                std::io::copy(&mut log_reader, &mut std::io::sink())
            }));
            let log =
                OperationLog::File(std::fs::File::from(std::os::fd::OwnedFd::from(log_writer)));
            let operation = tokio::spawn(apply_update(ID, Some(fingerprint), true, log));
            tokio::time::timeout(Duration::from_secs(35), started_rx)
                .await
                .unwrap()
                .unwrap();
            let record = effects.path().join(format!("uninstall{index}"));
            let child_record = record.clone();
            let child_executable = executable.clone();
            let probe = tokio::task::spawn_blocking(move || {
                Command::new(child_executable)
                    .args(["--exact", TEST, "--nocapture"])
                    .env(MODE, "uninstall")
                    .env(RECORD, child_record)
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(probe.status.success(), "{probe:?}");
            assert_eq!(std::fs::read(record).unwrap(), b"refused");
            assert!(set_enabled(ID, false).is_err());
            assert!(dismiss_update(ID, "declined").is_err());
            assert!(
                approve_installed(ID, &PluginManifest::hash_bytes(next_manifest.as_bytes()))
                    .is_err()
            );
            operation.abort();
            assert!(operation.await.unwrap_err().is_cancelled());
            assert!(begin_operation().is_err());
            assert!(
                release.finish(),
                "runtime made no progress before watchdog release"
            );

            let lease = begin_operation().unwrap();
            if fail {
                assert_eq!(std::fs::read(&installed).unwrap(), before_manifest);
                assert_eq!(
                    serde_json::to_value(&Config::load().unwrap().plugins[ID]).unwrap(),
                    before_config
                );
            } else {
                assert_eq!(std::fs::read(&installed).unwrap(), next_manifest.as_bytes());
                assert!(super::super::registry().get(ID).unwrap().active());
            }
            assert_eq!(Lockfile::load().unwrap().get(ID).unwrap().version, "2.0.0");
            drop(lease);
        }
        uninstall(ID).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn source_preparation_keeps_runtime_and_exclusion_through_cancellation() {
        let _reload = super::super::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        let input = source.path().to_str().unwrap().to_owned();
        std::fs::write(
            source.path().join("aoe-plugin.toml"),
            "id = \"acme.preparation\"\nname = \"Preparation\"\nversion = \"1.0.0\"\napi_version = 2\n",
        )
        .unwrap();
        let consent = preview_install(&input, false).await.unwrap();
        let mut gate = fetch::test_gate::install(
            &source
                .path()
                .join("aoe-plugin.toml")
                .canonicalize()
                .unwrap(),
            "copy",
        );
        let mut preparation =
            fetch::test_gate::install(&super::super::plugins_dir().unwrap(), "staging-ready");
        let operation = tokio::spawn(async move {
            apply_install(&input, &consent.fingerprint, false, OperationLog::Inherit).await
        });
        preparation.wait_for_preparation().await;
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut gate.entered)
            .await
            .unwrap()
            .unwrap();
        operation.abort();
        assert!(operation.await.unwrap_err().is_cancelled());
        assert!(begin_operation().is_err());
        gate.release();
        let _operation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(operation) = begin_operation() {
                    break operation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!Config::load()
            .unwrap()
            .plugins
            .contains_key("acme.preparation"));
        assert!(!super::super::plugins_dir()
            .unwrap()
            .join("acme.preparation")
            .exists());
    }

    #[tokio::test]
    #[serial]
    async fn special_source_added_after_preview_fails_without_publication() {
        let _reload = super::super::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        let input = source.path().display().to_string();
        std::fs::write(
            source.path().join("aoe-plugin.toml"),
            "id = \"acme.special\"\nname = \"Special\"\nversion = \"1.0.0\"\napi_version = 2\n",
        )
        .unwrap();
        let consent = preview_install(&input, false).await.unwrap();
        nix::unistd::mkfifo(&source.path().join("fifo"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let mut preparation =
            fetch::test_gate::install(&super::super::plugins_dir().unwrap(), "staging-ready");
        let task = tokio::spawn(async move {
            apply_install(&input, &consent.fingerprint, false, OperationLog::Inherit).await
        });
        preparation.wait_for_preparation().await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("special file"));
        assert!(!Config::load().unwrap().plugins.contains_key("acme.special"));
        assert!(Lockfile::load().unwrap().get("acme.special").is_none());
        assert!(!super::super::plugins_dir()
            .unwrap()
            .join("acme.special")
            .exists());
        assert!(begin_operation().is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn previews_reject_concurrent_plugin_operations() {
        let _home = isolate_app_dir();
        let _operation = begin_operation().unwrap();
        let install = preview_install("/missing", false).await.unwrap_err();
        let update = preview_update("missing.plugin", false).await.unwrap_err();
        for error in [install, update] {
            assert!(error
                .to_string()
                .contains("another plugin operation is running"));
        }
    }

    #[tokio::test]
    #[serial]
    async fn budget_timeout_settles_descendant_before_rollback_and_lease_release() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        static EXPIRED: AtomicBool = AtomicBool::new(false);
        EXPIRED.store(false, Ordering::SeqCst);
        struct ExpireOnDrop;
        impl Drop for ExpireOnDrop {
            fn drop(&mut self) {
                EXPIRED.store(true, Ordering::SeqCst);
            }
        }
        let _expire = ExpireOnDrop;
        let _reload = super::super::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        let effects = tempfile::tempdir().unwrap();
        let manifest = source.path().join("aoe-plugin.toml");
        let base = "id = \"acme.budget\"\nname = \"Budget\"\nversion = \"1.0.0\"\napi_version = 2\ncapabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"command\"\ncommand = [\"sh\"]\nsystem = true\n";
        std::fs::write(&manifest, base).unwrap();
        std::fs::write(source.path().join("original"), b"previous bytes").unwrap();
        install(source.path().to_str().unwrap(), true, false)
            .await
            .unwrap();
        let final_dir = super::super::plugins_dir().unwrap().join("acme.budget");
        let config_path = crate::session::get_app_dir().unwrap().join("config.toml");
        let before_config = std::fs::read(&config_path).unwrap();
        let lock_path = crate::session::get_app_dir().unwrap().join("plugins.lock");
        let before_lock = std::fs::read(&lock_path).unwrap();
        let before_hash = super::super::integrity::tree_hash(&final_dir).unwrap();
        let pid_file = effects.path().join("descendant-pid");
        let ready = effects.path().join("descendant-ready");
        let writer = r#"import os, pathlib, signal, sys, time
if os.fork() == 0:
    os.setpgid(0, 0)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    pathlib.Path(sys.argv[1]).write_text(str(os.getpid()))
    with open(sys.argv[3], "ab", buffering=0) as output:
        output.write(b"writer")
        pathlib.Path(sys.argv[2]).touch()
        while True:
            output.write(b"writer")
            time.sleep(0.01)
else:
    os.wait()
"#;
        let command = serde_json::to_string(&[
            "python3",
            "-c",
            writer,
            pid_file.to_str().unwrap(),
            ready.to_str().unwrap(),
            final_dir.join("writer-output").to_str().unwrap(),
        ])
        .unwrap();
        std::fs::write(
            &manifest,
            format!(
                "{}\n[[runtime.build]]\ncommand = {command}\n",
                base.replace("1.0.0", "2.0.0")
            ),
        )
        .unwrap();
        let operation = begin_operation().unwrap();
        let mut prepared = prepare_update("acme.budget", true, Some(operation.clone()))
            .await
            .unwrap();
        prepared.fetched.budget = super::super::execution::Budget::controlled_for_test(&EXPIRED);
        let mut rollback = fetch::test_gate::install(&final_dir, "rollback");
        let log_path = effects.path().join("operation.log");
        let log = OperationLog::file(&log_path).unwrap();
        let task = tokio::task::spawn_blocking(move || {
            let _operation = operation;
            let result = apply_prepared(
                &prepared,
                prepared.prior_grant.clone(),
                &NodeEnv::new(),
                &log,
            );
            prepared.fetched.close().unwrap();
            result
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!(
                "writer readiness: {error}; build finished={}; log={}",
                task.is_finished(),
                std::fs::read_to_string(&log_path).unwrap_or_default()
            )
        });
        let pid: u32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        let birth = crate::process::process_birth_identity(pid)
            .unwrap()
            .expect("live descendant");
        let process = crate::process::CapturedProcess::capture(pid, &birth)
            .unwrap()
            .expect("live descendant");
        struct Cleanup(Option<crate::process::CapturedProcessTree>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().settle(std::time::Duration::ZERO);
            }
        }
        let _cleanup = Cleanup(Some(
            crate::process::CapturedProcessTree::capture(pid).unwrap(),
        ));
        assert!(begin_operation().is_err());
        // Trigger deadline only after the controlled descendant has begun writing.
        EXPIRED.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(10), &mut rollback.entered)
            .await
            .unwrap()
            .unwrap();
        assert!(
            process.exited().unwrap(),
            "physical exit must precede rollback"
        );
        assert!(
            final_dir.join("writer-output").exists(),
            "rollback is still gated"
        );
        assert!(
            begin_operation().is_err(),
            "exclusion must span restoration"
        );
        rollback.release();
        let error = task.await.unwrap().unwrap_err();
        assert!(
            error
                .downcast_ref::<super::super::execution::Exhausted>()
                .is_some(),
            "{error:#}"
        );
        assert_eq!(
            super::super::integrity::tree_hash(&final_dir).unwrap(),
            before_hash
        );
        assert_eq!(
            std::fs::read(final_dir.join("original")).unwrap(),
            b"previous bytes"
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), before_config);
        assert_eq!(std::fs::read(&lock_path).unwrap(), before_lock);
        assert!(std::fs::read_to_string(&log_path)
            .unwrap()
            .contains("building acme.budget"));
        assert!(begin_operation().is_ok());
    }

    #[test]
    #[serial]
    fn recovery_diagnostics_redact_both_filtered_trace_and_marker() {
        for persist in [true, false] {
            let _home = isolate_app_dir();
            let root = crate::session::get_app_dir().unwrap();
            std::fs::create_dir_all(&root).unwrap();
            let marker = root.join(".plugin-recovery-required");
            if !persist {
                std::fs::create_dir(&marker).unwrap();
            }
            let logs = tempfile::NamedTempFile::new().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter("off,plugin.install=error")
                .with_ansi(false)
                .with_writer(logs.reopen().unwrap())
                .finish();
            let result: Result<()> = Err(super::super::execution::RecoveryRequired(
                "command-pid=42 termination unproven".into(),
            )
            .into());
            let result = result.context(
                "build https://alice:private-password@example.test/?token=private-token ghp_privatebaretoken",
            );
            tracing::subscriber::with_default(subscriber, || log_recovery(&result));
            let trace = std::fs::read_to_string(logs.path()).unwrap();
            assert!(
                trace.contains("command-pid=42") && trace.contains("[REDACTED]"),
                "{trace}"
            );
            for secret in [
                "alice",
                "private-password",
                "private-token",
                "ghp_privatebaretoken",
            ] {
                assert!(!trace.contains(secret), "{trace}");
            }
            if persist {
                let saved = std::fs::read_to_string(&marker).unwrap();
                assert!(
                    saved.contains("command-pid=42") && saved.contains("[REDACTED]"),
                    "{saved}"
                );
                for secret in [
                    "alice",
                    "private-password",
                    "private-token",
                    "ghp_privatebaretoken",
                ] {
                    assert!(!saved.contains(secret), "{saved}");
                }
            }
            assert!(check_recovery_evidence().is_err());
        }
    }

    #[tokio::test]
    #[serial]
    async fn failed_restoration_retains_exclusion_and_recovery_evidence() {
        let _reload = super::super::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        let effects = tempfile::tempdir().unwrap();
        let node = super::super::write_fake_node(effects.path(), "node");
        let _node = crate::session::test_support::EnvGuard::set(&[("AOE_ACP_NODE", &node)]);
        let base = "id = \"acme.recovery\"\nname = \"Recovery\"\nversion = \"1.0.0\"\napi_version = 2\ncapabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"command\"\ncommand = [\"sh\"]\nsystem = true\n";
        std::fs::write(source.path().join("aoe-plugin.toml"), base).unwrap();
        install(source.path().to_str().unwrap(), true, false)
            .await
            .unwrap();
        let before = serde_json::to_value(Config::load().unwrap().plugins).unwrap();
        std::fs::write(
            source.path().join("aoe-plugin.toml"),
            format!(
                "{}\n[[runtime.build]]\ncommand = [\"sh\", \"-c\", \"exit 1\"]\n",
                base.replace("1.0.0", "2.0.0")
            ),
        )
        .unwrap();
        let preview = preview_update("acme.recovery", true).await.unwrap();
        let fingerprint = match preview {
            UpdatePreview::SafeUpdate { fingerprint, .. } => fingerprint,
            UpdatePreview::ConsentRequired { consent, .. } => consent.fingerprint,
            UpdatePreview::NoUpdate { .. } => panic!("fixture must update"),
        };
        let final_dir = super::super::plugins_dir().unwrap().join("acme.recovery");
        let backup = final_dir.with_file_name("acme.recovery.bak");
        let rescue = effects.path().join("prior-installation");
        let mut gate = fetch::test_gate::install(&final_dir, "rollback");
        let task = tokio::spawn(async move {
            apply_update(
                "acme.recovery",
                Some(fingerprint),
                true,
                OperationLog::Inherit,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), &mut gate.entered)
            .await
            .unwrap()
            .unwrap();
        assert!(begin_operation().is_err());
        std::fs::rename(&backup, &rescue).unwrap();
        gate.release();
        let error = task.await.unwrap().unwrap_err();
        assert!(error
            .downcast_ref::<super::super::execution::RecoveryRequired>()
            .is_some());
        let evidence = crate::session::get_app_dir()
            .unwrap()
            .join(".plugin-recovery-required");
        assert!(std::fs::read_to_string(evidence)
            .unwrap()
            .contains("restoring"));
        assert!(begin_operation().is_err(), "failure must retain exclusion");
        assert_eq!(
            serde_json::to_value(Config::load().unwrap().plugins).unwrap(),
            before
        );
        assert!(
            rescue.join("aoe-plugin.toml").exists(),
            "prior installation remains available for recovery"
        );
    }
}
