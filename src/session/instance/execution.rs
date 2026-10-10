//! Conversation identity is independent of status detection and command spelling.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationProvenance {
    #[default]
    Unknown,
    Preallocated,
    Observed,
    Asserted,
    Imported,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionLocation {
    pub filesystem: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionBinding {
    pub agent: String,
    pub stores: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub configuration: Vec<ExecutionLocation>,
    pub cwd: PathBuf,
    pub cwd_filesystem: String,
    pub filesystem: String,
    /// Whether the launch explicitly exported Claude's implicit default store.
    /// `None` identifies bindings written before routing provenance existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_default_store: Option<bool>,
}

impl PartialEq for ExecutionBinding {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl Eq for ExecutionBinding {}

impl std::hash::Hash for ExecutionBinding {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

impl ExecutionBinding {
    fn identity(
        &self,
    ) -> (
        &str,
        &[PathBuf],
        &[ExecutionLocation],
        &std::path::Path,
        &str,
        &str,
    ) {
        (
            &self.agent,
            &self.stores,
            &self.configuration,
            &self.cwd,
            &self.cwd_filesystem,
            &self.filesystem,
        )
    }

    pub(crate) fn key<'a>(&'a self, sid: &'a str) -> ConversationKey<'a> {
        ConversationKey {
            session_id: sid,
            agent: &self.agent,
            stores: &self.stores,
            filesystem: &self.filesystem,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationBinding {
    pub session_id: String,
    pub execution: Option<ExecutionBinding>,
    #[serde(default)]
    pub provenance: ConversationProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,
}

impl ConversationBinding {
    pub fn unknown(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            execution: None,
            provenance: ConversationProvenance::Unknown,
            transcript_path: None,
        }
    }

    pub fn is_known(&self) -> bool {
        self.execution.is_some()
            && matches!(
                self.provenance,
                ConversationProvenance::Observed
                    | ConversationProvenance::Asserted
                    | ConversationProvenance::Imported
            )
    }

    pub fn is_unattributed(&self) -> bool {
        self.execution.is_none() && self.provenance == ConversationProvenance::Unknown
    }

    pub(crate) fn excludes_capture(&self, sid: &str, source: Option<&ExecutionBinding>) -> bool {
        if self.session_id != sid {
            return false;
        }
        match (
            source,
            self.execution
                .as_ref()
                .filter(|_| self.provenance != ConversationProvenance::Unknown),
        ) {
            (Some(source), Some(owner)) => source.key(sid) == owner.key(sid),
            _ => true,
        }
    }

    pub(crate) fn key(&self) -> Option<ConversationKey<'_>> {
        let execution = self.execution.as_ref()?;
        Some(ConversationKey {
            session_id: &self.session_id,
            agent: &execution.agent,
            stores: &execution.stores,
            filesystem: &execution.filesystem,
        })
    }
}

/// Attest the default-store route a launch observed onto a binding written
/// before routing provenance existed (#4127).
///
/// The marker records what a worker was actually launched with, so only an
/// observed launch may write it: a request never persists a guess about the
/// configuration as it stands, and a marker already written is never
/// overwritten. The observation must be a single-store host Claude launch
/// carrying its own marker, and it must name the same store as the target, so
/// a different agent, filesystem, or store attests nothing. Stores are compared
/// canonically because two spellings of one directory are one store.
///
/// Nothing else about the binding moves — not the session id, the provenance,
/// the transcript path, the store, the cwd — which is what keeps the
/// conversation identity stable: `identity()` excludes the marker, so stamping
/// it cannot reclassify the conversation it describes.
pub(crate) fn attest_observed_default_store(
    binding: &mut ConversationBinding,
    observed: Option<&ExecutionBinding>,
) -> bool {
    let Some(observed) = observed.filter(|o| o.agent == "claude" && o.filesystem == "host") else {
        return false;
    };
    let Some(observed_store) = single_store(&observed.stores) else {
        return false;
    };
    let Some(observed_marker) = observed.exported_default_store else {
        return false;
    };
    if !binding.is_known() {
        return false;
    }
    let Some(execution) = binding.execution.as_mut() else {
        return false;
    };
    if execution.exported_default_store.is_some() {
        return false;
    }
    if execution.agent != observed.agent || execution.filesystem != observed.filesystem {
        return false;
    }
    let same_store = match single_store(&execution.stores) {
        Some(target) => same_canonical_store(target, observed_store),
        None => false,
    };
    if !same_store {
        return false;
    }
    execution.exported_default_store = Some(observed_marker);
    true
}

/// Whether a reserved (`Preallocated`) Claude id has its transcript in the
/// binding's own store, which proves it names a resumable conversation.
///
/// The hook sidecar that otherwise confirms a reserved id survives neither a
/// resume nor a `/tmp` wipe, so without this a session can stay `Preallocated`
/// for good and be refused as a fork parent despite holding a complete
/// transcript. Existence uses the same probe as the `--resume` arm.
pub(super) fn transcript_confirms_reserved_claude_conversation(
    binding: &ConversationBinding,
) -> bool {
    if binding.provenance != ConversationProvenance::Preallocated {
        return false;
    }
    let Some(execution) = binding.execution.as_ref() else {
        return false;
    };
    if execution.agent != "claude" || execution.filesystem != "host" {
        return false;
    }
    let Some(store) = single_store(&execution.stores) else {
        return false;
    };
    !crate::session::capture::claude_host_transcript_confirmed_absent(
        &execution.cwd.to_string_lossy(),
        &binding.session_id,
        &[],
        Some(store),
    )
}

/// Confirms, from inside a live pane's sidecar poller, the reserved Claude id
/// the row held when the poller started, for a pane whose sidecar never
/// reports it.
///
/// The confirmation names the row's own id under its recorded execution, so it
/// claims nothing new, and the sync path's ownership, exclusion and
/// compare-and-set checks still decide it. The poller only re-sends an
/// observation that changed, so a refused confirmation is not retried every
/// tick.
pub(super) struct ReservedClaudeConfirmation {
    binding: ConversationBinding,
    active: ActiveExecution,
    superseded: std::cell::Cell<bool>,
    confirmed: std::cell::OnceCell<crate::session::poller::SessionIdObservation>,
}

impl ReservedClaudeConfirmation {
    pub(super) fn for_instance(instance: &Instance) -> Option<Self> {
        if !matches!(instance.resume_intent, ResumeIntent::Default) {
            return None;
        }
        let active = instance.active_execution.as_ref()?;
        let binding = instance.agent_session_binding.as_ref().filter(|binding| {
            instance.agent_session_id.as_deref() == Some(binding.session_id.as_str())
                && binding.provenance == ConversationProvenance::Preallocated
                && binding.execution.as_ref() == Some(&active.binding)
        })?;
        Some(Self {
            binding: binding.clone(),
            active: active.clone(),
            superseded: std::cell::Cell::new(false),
            confirmed: std::cell::OnceCell::new(),
        })
    }

    /// Passes the sidecar's fresh observation through, falling back to the
    /// transcript only while the sidecar has named no other conversation, so a
    /// `/clear` the sidecar reported is never reverted to the reserved id.
    /// `published` reads the sidecar at any age: Claude only rewrites it on
    /// session start and prompt submission, so a `/clear` followed by an idle
    /// pane is older than the fresh read accepts.
    pub(super) fn observe(
        &self,
        fresh: Option<crate::session::poller::SessionIdObservation>,
        published: impl FnOnce() -> Option<crate::session::poller::SessionIdObservation>,
    ) -> Option<crate::session::poller::SessionIdObservation> {
        if let Some(observation) = fresh {
            if observation.sid != self.binding.session_id {
                self.superseded.set(true);
            }
            return Some(observation);
        }
        if !self.superseded.get()
            && published().is_some_and(|observation| observation.sid != self.binding.session_id)
        {
            self.superseded.set(true);
        }
        if self.superseded.get() {
            return None;
        }
        if let Some(confirmed) = self.confirmed.get() {
            return Some(confirmed.clone());
        }
        if !transcript_confirms_reserved_claude_conversation(&self.binding) {
            return None;
        }
        let execution = self.binding.execution.clone()?;
        let mut observation = crate::session::poller::SessionIdObservation::instance_sidecar(
            self.binding.session_id.clone(),
            None,
        );
        observation.execution = Some(self.active.clone());
        observation.scope_to(execution);
        Some(self.confirmed.get_or_init(|| observation).clone())
    }
}

fn single_store(stores: &[PathBuf]) -> Option<&std::path::Path> {
    match stores {
        [store] => Some(store.as_path()),
        _ => None,
    }
}

fn same_canonical_store(left: &std::path::Path, right: &std::path::Path) -> bool {
    match (
        crate::session::capture::canonicalize_allowing_missing_leaf(left),
        crate::session::capture::canonicalize_allowing_missing_leaf(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ConversationKey<'a> {
    session_id: &'a str,
    agent: &'a str,
    stores: &'a [PathBuf],
    filesystem: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActiveExecution {
    pub(crate) launch_id: String,
    pub(crate) binding: ExecutionBinding,
    pub(crate) capture: Option<CaptureContext>,
    pub(crate) container: Option<crate::containers::ContainerExecutionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CaptureContext {
    Hooks(PathBuf),
    Store {
        root: PathBuf,
        cwd: String,
    },
    Pi {
        source: super::SessionSidecarSource,
        /// Canonical, as the launch builds it: readers compare the published transcript path
        /// against it, and a differently spelled root silently drops every observation.
        root: PathBuf,
    },
    Omp(super::OmpCaptureMetadata),
    Prime {
        plan: super::PrimeAgentCapturePlan,
        sidecar: Option<super::SessionSidecarSource>,
    },
}

use super::{Instance, ResumeIntent};
use crate::agents::{AgentDef, AGENTS};
use crate::session::fork::ForkParentRef;
use anyhow::{bail, Context, Result};

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_NEXT_NATIVE_RESOLUTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const VIBE_NAMESPACE_ENVIRONMENT: &[&str] = &[
    "SAVE_DIR",
    "SESSION_PREFIX",
    "VIBE_AGENT_PATHS",
    "VIBE_DEFAULT_AGENT",
    "VIBE_SESSION_LOGGING",
    "VIBE_SESSION_LOGGING__SAVE_DIR",
    "VIBE_SESSION_LOGGING__SESSION_PREFIX",
];

pub(super) struct NativeExecution {
    pub(super) agent: &'static AgentDef,
    pub(super) binding: ExecutionBinding,
    pub(super) routing: Vec<(String, Option<String>)>,
    pub(super) case_insensitive_routing: &'static [&'static str],
    pub(super) omp: Option<crate::session::capture::OmpResolvedContext>,
    pub(super) inputs: NativeLaunchInputs,
    pub(super) program: PathBuf,
    pub(super) capture: Option<CaptureContext>,
    pub(super) pi_transcript_path: Option<String>,
    pub(super) namespace_arguments: Vec<String>,
    pub(super) target_session_id: Option<String>,
    pub(super) resolved_target_session_id: Option<String>,
    pub(super) pi_pinnable: bool,
    pub(super) opencode_preassign: bool,
    pub(super) opencode_schema: Option<OpenCodeSessionSchema>,
    /// A recorded store outranks the store a new session would use here, as
    /// `(launch, new_session, source)`. The launch reports it once.
    pub(super) store_override: Option<(PathBuf, PathBuf, &'static str)>,
}

impl NativeExecution {
    pub(super) fn host_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command
            .current_dir(&self.inputs.cwd)
            .env_clear()
            .envs(
                self.inputs
                    .raw_environment
                    .iter()
                    .map(|(key, value)| (key, value)),
            )
            .envs(&self.inputs.environment);
        for (key, value) in &self.routing {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
    }

    pub(super) fn validate_opencode_session_target(&self, sid: &str) -> Result<()> {
        let schema = self
            .opencode_schema
            .context("OpenCode active schema was not prepared")?;
        let database = self
            .binding
            .stores
            .first()
            .context("OpenCode store is missing")?;
        anyhow::ensure!(
            self.binding.filesystem == "host",
            "OpenCode routing requires a local database projection"
        );
        let connection = rusqlite::Connection::open_with_flags(
            database.canonicalize()?,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(std::time::Duration::from_millis(100))?;
        let (workspace, directory) = opencode_session_row(&connection, sid, schema)?
            .with_context(|| format!("OpenCode target {sid} is absent from this build's active session table in OPENCODE_DB"))?;
        anyhow::ensure!(
            !workspace,
            "OpenCode target may forward to another workspace; managed resume and fork are refused"
        );
        anyhow::ensure!(
            std::path::Path::new(&directory).is_absolute()
                && self
                    .inputs
                    .canonical_path(std::path::Path::new(&directory))?
                    == self.inputs.cwd,
            "OpenCode target routes to a different working directory"
        );
        Ok(())
    }

    fn opencode_session_schema(&self) -> Result<OpenCodeSessionSchema> {
        let probe = || -> Result<OpenCodeSessionSchema> {
            let env_file;
            let mut command = if let Some(container) = &self.inputs.container {
                let environment = self
                    .inputs
                    .docker_env
                    .as_ref()
                    .context("OpenCode container environment is unavailable")?;
                env_file = crate::session::environment::container_env_file(
                    environment
                        .env
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str()))
                        .chain(self.routing.iter().filter_map(|(key, value)| {
                            value.as_deref().map(|value| (key.as_str(), value))
                        })),
                )?;
                let mut script = String::new();
                for (key, value) in &self.routing {
                    if value.is_none() {
                        script.push_str(&format!(
                            "unset {}; ",
                            crate::session::environment::shell_escape_script_word(key)
                        ));
                    }
                }
                script.push_str(&format!(
                    "exec {} --version",
                    crate::session::environment::shell_escape_script_word(
                        self.program
                            .to_str()
                            .context("OpenCode program is not UTF-8")?
                    )
                ));
                container.runtime.exec_with_env_file(
                    &container.id,
                    self.inputs
                        .cwd
                        .to_str()
                        .context("OpenCode cwd is not UTF-8")?,
                    &["/bin/sh".into(), "-c".into(), script],
                    env_file.path(),
                )
            } else {
                let mut command = self.host_command();
                command.arg("--version");
                command
            };
            command.stdin(std::process::Stdio::null());
            let output = crate::session::capture::run_with_timeout_limit(
                command,
                std::time::Duration::from_secs(5),
                "OpenCode version probe",
                4096,
            )?;
            let output = std::str::from_utf8(&output).context("OpenCode version is not UTF-8")?;
            OpenCodeSessionSchema::from_version_output(output).with_context(|| {
                format!(
                    "OpenCode version/build is not source-attested: {:?}",
                    output.trim()
                )
            })
        };
        probe().map_err(|error| error.context(UnattestedOpenCodeSchema))
    }
}
#[derive(Clone, Copy)]
pub(super) struct AgentLaunchContext<'a> {
    pub(super) generation: crate::agents::AgentGeneration,
    pub(super) host_command: Option<&'a std::process::Command>,
}

impl<'a> AgentLaunchContext<'a> {
    pub(super) fn host(
        agent: Option<&crate::agents::AgentDef>,
        host_command: Option<&'a std::process::Command>,
    ) -> Self {
        let generation = agent.map_or(crate::agents::AgentGeneration::Current, |agent| {
            if !agent.spans_agent_generations {
                crate::agents::AgentGeneration::Current
            } else {
                host_command.map_or(crate::agents::AgentGeneration::Unknown, |command| {
                    crate::agents::agent_generation_for(agent, command)
                })
            }
        });
        Self {
            generation,
            host_command,
        }
    }
}

const OPENCODE_NATIVE_ENV_KEYS: &[&str] = &[
    "OPENCODE_DB",
    "OPENCODE_DISABLE_CHANNEL_DB",
    "OPENCODE_CONFIG_DIR",
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_WORKSPACE_ID",
];

fn host_launch_environment_from(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    entries: &[String],
    opencode_inheritance: Option<bool>,
) -> (
    std::collections::HashMap<String, String>,
    Vec<(std::ffi::OsString, std::ffi::OsString)>,
) {
    use std::ffi::{OsStr, OsString};
    let mut environment = std::collections::HashMap::new();
    let mut raw_environment = Vec::new();
    for (key, value) in vars {
        if let Some(passthrough) = opencode_inheritance {
            let live = crate::process::LIVE_PANE_ENV_KEYS
                .iter()
                .any(|live| key == OsStr::new(live));
            let required = crate::session::environment::HOST_BASE_ENV_KEYS
                .iter()
                .chain(OPENCODE_NATIVE_ENV_KEYS)
                .any(|required| key == OsStr::new(required));
            if live
                || !(required
                    || crate::session::environment::host_env_key_inherited(&key, passthrough))
            {
                continue;
            }
        }
        match (key.into_string(), value.into_string()) {
            (Ok(key), Ok(value)) => {
                environment.insert(key, value);
            }
            (key, value) => raw_environment.push((
                key.map(OsString::from).unwrap_or_else(|key| key),
                value.map(OsString::from).unwrap_or_else(|value| value),
            )),
        }
    }
    for (key, value) in crate::session::environment::resolve_host_environment_pairs(entries) {
        raw_environment.retain(|(raw_key, _)| raw_key != OsStr::new(&key));
        environment.insert(key, value);
    }
    (environment, raw_environment)
}

pub(super) struct NativeLaunchInputs {
    pub(super) launch_id: String,
    pub(super) environment: std::collections::HashMap<String, String>,
    pub(super) raw_environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub(super) cwd: PathBuf,
    pub(super) profile: String,
    pub(super) container: Option<crate::containers::ContainerExecutionSnapshot>,
    pub(super) docker_env: Option<crate::session::environment::DockerExecEnv>,
    pub(super) pane_env: Vec<crate::tmux::PaneEnvMutation>,
    pub(super) identity_extension: Option<(String, String)>,
}
impl NativeLaunchInputs {
    /// Whether the pane runs `program` as the binary AoE's own PATH resolves `binary` to, which
    /// is what a host `--help` probe of `binary` describes.
    pub(super) fn runs_host_path_binary(&self, binary: &str, program: &std::path::Path) -> bool {
        self.container.is_none()
            && !self.pane_env.iter().any(|entry| matches!(entry, crate::tmux::PaneEnvMutation::Set { key, .. } | crate::tmux::PaneEnvMutation::Unset { key } if key == "PATH"))
            && self.environment.get("PATH") == std::env::var("PATH").ok().as_ref()
            && which::which(binary).ok().as_deref() == Some(program)
    }

    fn read_native_file(&self, path: &std::path::Path) -> Result<Option<Vec<u8>>> {
        let native = self.canonical_path(path)?;
        let location = self.physical_location(&native);
        anyhow::ensure!(
            location.filesystem == "host",
            "native configuration requires a local filesystem projection"
        );
        match std::fs::symlink_metadata(&location.path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let parent = location
            .path
            .parent()
            .context("native configuration has no parent")?;
        let leaf = location
            .path
            .file_name()
            .context("native configuration has no filename")?;
        let directory = crate::session::AnchoredDir::open(parent)?;
        directory
            .read_regular(std::path::Path::new(leaf), 65536)?
            .map(Some)
            .context("native configuration is not a bounded regular file")
    }
    fn read_native_dotenv(&self, path: &std::path::Path) -> Result<dotenv_ng_core::EnvMap> {
        let Some(bytes) = self.read_native_file(path)? else {
            return Ok(Default::default());
        };
        let contents = std::str::from_utf8(&bytes).context("native dotenv is not UTF-8")?;
        dotenv_ng_core::EnvLoader::with_reader(contents.trim_start_matches('\u{feff}').as_bytes())
            .sequence(dotenv_ng_core::EnvSequence::InputOnly)
            .substitution(false)
            .multiline(true)
            .load()
            .map_err(|_| anyhow::anyhow!("native dotenv cannot be resolved: {}", path.display()))
    }

    fn validate_hermes_workdir(&self, value: &str) -> Result<()> {
        if matches!(value, "" | "." | "auto" | "cwd") {
            return Ok(());
        }
        anyhow::ensure!(
            !value.contains('$'),
            "Hermes terminal cwd contains unresolved interpolation"
        );
        let path = if value == "~" || value.starts_with("~/") {
            let home = self
                .environment
                .get("HOME")
                .context("Hermes HOME is unavailable")?;
            std::path::Path::new(home).join(value.strip_prefix("~/").unwrap_or(""))
        } else {
            PathBuf::from(value)
        };
        anyhow::ensure!(
            self.canonical_path(&path)? == self.cwd,
            "Hermes terminal cwd differs from the declared launch context"
        );
        Ok(())
    }
    fn validate_hermes_stored_cwd(&self, value: Option<&str>, setup: bool) -> Result<()> {
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return Ok(());
        };
        let value = if setup {
            value
        } else {
            value.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
        };
        let path = if setup && value.starts_with('~') {
            anyhow::ensure!(
                value == "~" || value.starts_with("~/"),
                "Hermes stored cwd has an unresolved user home"
            );
            PathBuf::from(
                self.environment
                    .get("HOME")
                    .context("Hermes HOME is unavailable")?,
            )
            .join(value.strip_prefix("~/").unwrap_or(""))
        } else {
            PathBuf::from(value)
        };
        anyhow::ensure!(
            self.canonical_path(&path)? == self.cwd,
            "Hermes stored cwd differs from the prepared launch context"
        );
        Ok(())
    }

    fn resolve_hermes_target(&self, root: &std::path::Path, sid: &str) -> Result<String> {
        anyhow::ensure!(
            !sid.eq_ignore_ascii_case("latest"),
            "Hermes latest is not an exact resume identity"
        );
        let native = self.canonical_path(root)?;
        let location = self.physical_location(&native);
        anyhow::ensure!(
            location.filesystem == "host",
            "Hermes resume requires a local database-directory projection"
        );
        let directory = location.path.canonicalize()?;
        anyhow::ensure!(
            directory == location.path && std::fs::metadata(&directory)?.is_dir(),
            "Hermes database directory is unavailable"
        );
        for name in [
            "state.db",
            "state.db-wal",
            "state.db-shm",
            "state.db-journal",
        ] {
            let path = native.join(name);
            let canonical = self.canonical_path(&path)?;
            anyhow::ensure!(
                canonical == path,
                "Hermes SQLite files must not be redirected by symlinks"
            );
            let projected = self.physical_location(&canonical);
            // A sidecar that does not exist yet is spelled through the mount's
            // configured path, which a symlinked root spells differently from
            // the canonical directory. Compare the parent's identity and the
            // file name instead of the raw spellings.
            let projected_directory = crate::session::capture::canonicalize_allowing_missing_leaf(
                projected
                    .path
                    .parent()
                    .context("Hermes SQLite file has no parent directory")?,
            );
            anyhow::ensure!(
                projected.filesystem == "host"
                    && projected_directory.as_deref() == Some(directory.as_path())
                    && projected.path.file_name() == path.file_name(),
                "Hermes SQLite directory and sidecars have inconsistent projections"
            );
            match std::fs::symlink_metadata(&projected.path) {
                Ok(metadata) => {
                    anyhow::ensure!(
                        metadata.file_type().is_file(),
                        "Hermes SQLite file is not a regular file"
                    );
                    std::fs::File::open(&projected.path)?;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound && name != "state.db" => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut database = rusqlite::Connection::open_with_flags(
            directory.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        database.busy_timeout(std::time::Duration::from_millis(100))?;
        let transaction = database.transaction()?;
        let target = self.hermes_resume_tip(&transaction, sid)?;
        anyhow::ensure!(
            !target.eq_ignore_ascii_case("latest"),
            "Hermes resolved target is a reserved resume selector"
        );
        anyhow::ensure!(
            self.hermes_resume_tip(&transaction, &target)? == target,
            "Hermes resume target redirects again when emitted"
        );
        transaction.commit()?;
        Ok(target)
    }

    fn hermes_resume_tip(&self, database: &rusqlite::Connection, sid: &str) -> Result<String> {
        use rusqlite::OptionalExtension;
        fn exact(database: &rusqlite::Connection, sid: &str) -> Result<()> {
            anyhow::ensure!(
                crate::session::capture::is_valid_session_id(sid),
                "Hermes session identity is invalid"
            );
            let mut query =
                database.prepare_cached("SELECT id FROM sessions WHERE id = ? LIMIT 2")?;
            let mut rows = query.query([sid])?;
            let row = rows
                .next()?
                .context("Hermes exact session identity does not exist")?;
            anyhow::ensure!(
                row.get_ref(0)?.as_str()? == sid && rows.next()?.is_none(),
                "Hermes exact session identity is ambiguous"
            );
            Ok(())
        }
        fn edge(database: &rusqlite::Connection, child: &str) -> Result<()> {
            let boundary: bool = database.prepare_cached(r#"
                SELECT json_extract(CASE WHEN json_valid(child.model_config) THEN child.model_config ELSE json_object() END, '$._branched_from') IS NOT NULL
                    OR json_extract(CASE WHEN json_valid(child.model_config) THEN child.model_config ELSE json_object() END, '$._reset_from') IS NOT NULL
                    OR EXISTS (SELECT 1 FROM sessions parent WHERE parent.id = child.parent_session_id
                        AND parent.end_reason = 'branched' AND child.started_at >= parent.ended_at)
                    OR EXISTS (SELECT 1 FROM sessions parent WHERE parent.id = child.parent_session_id
                        AND parent.end_reason IN ('session_reset','session_switch','idle','daily','suspended','resume_pending_expired')
                        AND child.session_key IS NOT NULL AND child.session_key != '' AND child.session_key = parent.session_key)
                FROM sessions child WHERE child.id = ?
            "#)?.query_row([child], |row| row.get(0))?;
            anyhow::ensure!(
                !boundary,
                "Hermes selected lineage crosses a separate conversation boundary"
            );
            Ok(())
        }
        let mut compression = database.prepare_cached(r#"
            SELECT child.id FROM sessions parent JOIN sessions child ON child.parent_session_id = parent.id
            WHERE parent.id = ? AND parent.end_reason = 'compression'
                AND json_extract(CASE WHEN json_valid(child.model_config) THEN child.model_config ELSE json_object() END, '$._branched_from') IS NULL
                AND json_extract(CASE WHEN json_valid(child.model_config) THEN child.model_config ELSE json_object() END, '$._delegate_from') IS NULL
                AND COALESCE(child.source, '') != 'tool'
            ORDER BY CASE WHEN child.end_reason = 'compression' THEN 0 WHEN child.ended_at IS NULL THEN 1 ELSE 2 END,
                COALESCE((SELECT MAX(activity.v) FROM (SELECT child.last_activity_at AS v UNION ALL SELECT
                    (SELECT MAX(message.timestamp) FROM messages message WHERE message.session_id = child.id)) activity), child.started_at) DESC,
                child.started_at DESC, child.id DESC LIMIT 1
        "#)?;
        let mut continuation = database.prepare_cached(r#"
            SELECT child.id FROM sessions child WHERE child.parent_session_id = ?
                AND json_extract(CASE WHEN json_valid(child.model_config) THEN child.model_config ELSE json_object() END, '$._branched_from') IS NULL
                AND json_extract(CASE WHEN json_valid(child.model_config) THEN child.model_config ELSE json_object() END, '$._delegate_from') IS NULL
                AND json_extract(CASE WHEN json_valid(child.model_config) THEN child.model_config ELSE json_object() END, '$._reset_from') IS NULL
                AND NOT EXISTS (SELECT 1 FROM sessions parent WHERE parent.id = child.parent_session_id
                    AND parent.end_reason IN ('session_reset','session_switch','idle','daily','suspended','resume_pending_expired')
                    AND child.session_key IS NOT NULL AND child.session_key != '' AND child.session_key = parent.session_key)
                AND COALESCE(child.source, '') != 'tool'
            ORDER BY child.started_at DESC, child.id DESC LIMIT 1
        "#)?;
        let mut current = sid.to_owned();
        let mut seen = std::collections::HashSet::new();
        for depth in 0..=100 {
            anyhow::ensure!(
                !seen.contains(&current),
                "Hermes compression lineage contains a cycle"
            );
            exact(database, &current)?;
            let next: Option<String> = compression
                .query_row([&current], |row| row.get(0))
                .optional()?;
            let Some(next) = next else {
                break;
            };
            anyhow::ensure!(
                depth < 100,
                "Hermes compression lineage exceeds the native traversal bound"
            );
            exact(database, &next)?;
            edge(database, &next)?;
            seen.insert(current);
            current = next;
        }
        let mut cwd = database.prepare_cached("SELECT cwd FROM sessions WHERE id = ?")?;
        let main_cwd: Option<String> = cwd.query_row([&current], |row| row.get(0))?;
        self.validate_hermes_stored_cwd(main_cwd.as_deref(), false)?;
        // R repeats C, which is already exhausted in this read transaction.
        let compression_tip = current.clone();
        seen.clear();
        let mut best = None;
        let mut messages =
            database.prepare_cached("SELECT 1 FROM messages WHERE session_id = ? LIMIT 1")?;
        for depth in 0..32 {
            anyhow::ensure!(
                !seen.contains(&current),
                "Hermes continuation lineage contains a cycle"
            );
            if messages.exists([&current])? {
                best = Some(current.clone());
            }
            let next: Option<String> = continuation
                .query_row([&current], |row| row.get(0))
                .optional()?;
            let Some(next) = next else {
                break;
            };
            anyhow::ensure!(
                depth < 31,
                "Hermes continuation lineage exceeds the native traversal bound"
            );
            exact(database, &next)?;
            edge(database, &next)?;
            seen.insert(current);
            current = next;
        }
        let target = best.unwrap_or(compression_tip);
        let setup_cwd: Option<String> = cwd.query_row([&target], |row| row.get(0))?;
        self.validate_hermes_stored_cwd(setup_cwd.as_deref(), true)?;
        Ok(target)
    }

    fn validate_vibe_namespace(
        &self,
        root: &std::path::Path,
        arguments: &str,
        yolo: bool,
    ) -> Result<PathBuf> {
        let store = self.canonical_path(&root.join("logs/session"))?;
        let validate_logging = |logging: &serde_json::Value| -> Result<()> {
            let logging = logging
                .as_object()
                .context("Vibe session_logging is not an object")?;
            if let Some(path) = logging.get("save_dir") {
                let path = path.as_str().context("Vibe save_dir is not a string")?;
                let home = self
                    .environment
                    .get("HOME")
                    .context("Vibe HOME is unavailable")?;
                let path = if path == "~" {
                    PathBuf::from(home)
                } else if let Some(path) = path.strip_prefix("~/") {
                    std::path::Path::new(home).join(path)
                } else {
                    PathBuf::from(path)
                };
                anyhow::ensure!(
                    self.canonical_path(&path)? == store,
                    "Vibe logging redirects the declared session store"
                );
            }
            anyhow::ensure!(
                logging
                    .get("session_prefix")
                    .is_none_or(|prefix| prefix.as_str() == Some("session")),
                "Vibe session prefix differs from the declared layout"
            );
            Ok(())
        };
        let routes_namespace = |key: &str| {
            VIBE_NAMESPACE_ENVIRONMENT
                .iter()
                .any(|name| name.eq_ignore_ascii_case(key))
        };
        for key in self.read_native_dotenv(&root.join(".env"))?.keys() {
            if self
                .environment
                .get(key)
                .is_some_and(|value| !value.is_empty())
            {
                continue;
            }
            anyhow::ensure!(
                !routes_namespace(&key.to_ascii_lowercase()),
                "Vibe dotenv routing is not represented by the declared context"
            );
        }
        let mut environments = std::collections::BTreeMap::new();
        for (key, value) in &self.environment {
            if !routes_namespace(key)
                || (value.is_empty()
                    && !key.eq_ignore_ascii_case("SAVE_DIR")
                    && !key.eq_ignore_ascii_case("SESSION_PREFIX"))
            {
                continue;
            }
            if let Some(previous) = environments.insert(key.to_ascii_lowercase(), value) {
                anyhow::ensure!(
                    previous == value,
                    "Vibe environment has conflicting case-insensitive settings"
                );
            }
        }
        let mut selected_profile = None;
        let words = shell_words::split(arguments)?;
        for (index, word) in words.iter().enumerate() {
            if let Some(name) = word.strip_prefix("--agent=").or_else(|| {
                (word == "--agent")
                    .then(|| words.get(index + 1).map(String::as_str))
                    .flatten()
            }) {
                selected_profile = Some(name.to_owned());
            }
        }
        if yolo {
            selected_profile = Some("auto-approve".into());
        }
        let use_defaults = selected_profile.is_none();
        let mut profiles = selected_profile.into_iter().collect::<Vec<_>>();
        if use_defaults {
            profiles.extend(["accept-edits".into(), "smart-approve".into()]);
            if let Some(name) = environments.get("vibe_default_agent") {
                profiles.push((*name).clone());
            }
        }
        let mut directories = vec![root.join("agents"), self.cwd.join(".vibe/agents")];
        let mut configs = vec![root.join("config.toml")];
        for directory in self
            .cwd
            .ancestors()
            .take_while(|directory| Some(*directory) != root.parent())
        {
            let path = directory.join(".vibe/config.toml");
            if self.read_native_file(&path)?.is_some() {
                configs.push(path);
                break;
            }
        }
        for path in configs {
            let Some(bytes) = self.read_native_file(&path)? else {
                continue;
            };
            let config: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)
                .context("Vibe configuration cannot be resolved")?;
            if let Some(logging) = config.get("session_logging") {
                validate_logging(&serde_json::to_value(logging)?)?;
            }
            if let Some(name) = config.get("default_agent").filter(|_| use_defaults) {
                profiles.push(
                    name.as_str()
                        .context("Vibe default_agent is not a string")?
                        .to_owned(),
                );
            }
            if let Some(paths) = config.get("agent_paths") {
                for path in paths
                    .as_array()
                    .context("Vibe agent_paths is not an array")?
                {
                    directories.push(PathBuf::from(
                        path.as_str().context("Vibe agent path is not a string")?,
                    ));
                }
            }
        }
        if let Some(paths) = environments.get("vibe_agent_paths") {
            directories.extend(
                serde_json::from_str::<Vec<PathBuf>>(paths)
                    .context("Vibe agent_paths cannot be resolved")?,
            );
        }
        for (key, value) in environments {
            match key.as_str() {
                "vibe_session_logging" => validate_logging(
                    &serde_json::from_str(value)
                        .context("Vibe session_logging cannot be resolved")?,
                )?,
                "save_dir" | "vibe_session_logging__save_dir" => {
                    validate_logging(&serde_json::json!({"save_dir": value}))?
                }
                "session_prefix" | "vibe_session_logging__session_prefix" => {
                    validate_logging(&serde_json::json!({"session_prefix": value}))?
                }
                _ => {}
            }
        }
        let home = self
            .environment
            .get("HOME")
            .context("Vibe HOME is unavailable")?;
        for directory in &mut directories {
            let expanded = match directory.to_str() {
                Some("~") => PathBuf::from(home),
                Some(path) if path.starts_with("~/") => std::path::Path::new(home).join(&path[2..]),
                _ => std::mem::take(directory),
            };
            *directory = self.canonical_path(&expanded)?;
        }
        directories.sort_unstable();
        directories.dedup();
        profiles.sort_unstable();
        profiles.dedup();
        for name in profiles {
            anyhow::ensure!(
                !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']),
                "Vibe agent profile name is not a file stem"
            );
            let filename = format!("{name}.toml");
            for directory in &directories {
                let path = directory.join(&filename);
                let Some(bytes) = self.read_native_file(&path)? else {
                    continue;
                };
                let profile: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)
                    .context("Vibe agent profile cannot be resolved")?;
                if let Some(logging) = profile.get("session_logging") {
                    validate_logging(&serde_json::to_value(logging)?)?;
                }
            }
        }
        Ok(store)
    }

    fn validate_hermes_namespace(&self, root: &std::path::Path) -> Result<()> {
        anyhow::ensure!(
            self.read_native_file(&root.join(".container-mode"))?
                .is_none(),
            "Hermes container delegation is not the declared execution context"
        );
        let managed = self
            .environment
            .get("HERMES_MANAGED_DIR")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/hermes"));
        anyhow::ensure!(
            managed.is_absolute(),
            "Hermes managed directory must be absolute"
        );
        for name in [".env", "config.yaml"] {
            anyhow::ensure!(
                self.read_native_file(&managed.join(name))?.is_none(),
                "Hermes managed configuration is not represented by the declared store"
            );
        }
        for (key, value) in self
            .environment
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "TERMINAL_ENV" | "TERMINAL_CWD"))
        {
            if key == "TERMINAL_CWD" {
                self.validate_hermes_workdir(value)?;
            } else {
                anyhow::ensure!(
                    matches!(value.as_str(), "" | "local"),
                    "Hermes terminal backend is not local to the declared execution"
                );
            }
        }
        for name in [".env", ".op.env"] {
            for key in self.read_native_dotenv(&root.join(name))?.keys() {
                anyhow::ensure!(!matches!(key.as_str(), "HERMES_HOME" | "HERMES_MANAGED_DIR" | "HOME" | "TERMINAL_CWD" | "TERMINAL_ENV"),
                    "Hermes dotenv routing override {key} is not represented by the declared context");
            }
        }
        if let Some(bytes) = self.read_native_file(&root.join("config.yaml"))? {
            let mut config: serde_yaml::Value =
                serde_yaml::from_slice(&bytes).context("Hermes config.yaml cannot be resolved")?;
            config
                .apply_merge()
                .context("Hermes YAML merges cannot be resolved")?;
            if let Some(terminal) = config.get("terminal") {
                if let Some(backend) = terminal.get("backend") {
                    anyhow::ensure!(
                        backend.as_str() == Some("local"),
                        "Hermes terminal backend differs from the declared execution"
                    );
                }
                if let Some(cwd) = terminal.get("cwd") {
                    self.validate_hermes_workdir(
                        cwd.as_str()
                            .context("Hermes terminal cwd is not a string")?,
                    )?;
                }
            }
            if let Some(secrets) = config
                .get("secrets")
                .and_then(serde_yaml::Value::as_mapping)
            {
                anyhow::ensure!(
                    !secrets.values().any(|value| value
                        .get("enabled")
                        .and_then(serde_yaml::Value::as_bool)
                        == Some(true)),
                    "Hermes external secret sources can redirect the declared namespace"
                );
            }
        }
        Ok(())
    }

    pub(super) fn canonical_path(&self, path: &std::path::Path) -> Result<PathBuf> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        match &self.container {
            Some(container) => container.runtime.canonical_path(&container.id, &path),
            None => {
                let mut ancestor = path.as_path();
                loop {
                    match ancestor.canonicalize() {
                        Ok(mut resolved) => {
                            for component in path.strip_prefix(ancestor)?.components() {
                                match component {
                                    std::path::Component::ParentDir => {
                                        resolved.pop();
                                    }
                                    std::path::Component::CurDir => {}
                                    component => resolved.push(component.as_os_str()),
                                }
                            }
                            return Ok(resolved);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            match std::fs::symlink_metadata(ancestor) {
                                Ok(_) => {
                                    bail!("native path cannot be resolved: {}", ancestor.display())
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                                Err(error) => return Err(error.into()),
                            }
                            ancestor = ancestor
                                .parent()
                                .context("native path has no existing ancestor")?;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
    }

    fn physical_location(&self, canonical_path: &std::path::Path) -> ExecutionLocation {
        match &self.container {
            Some(container) => {
                let (filesystem, path) = container.physical_path(canonical_path);
                ExecutionLocation { filesystem, path }
            }
            None => ExecutionLocation {
                filesystem: "host".into(),
                path: canonical_path.to_path_buf(),
            },
        }
    }

    fn native_transcript_path(
        &self,
        file: &std::path::Path,
        root: &std::path::Path,
    ) -> Result<PathBuf> {
        let path = match &self.container {
            None => file.to_path_buf(),
            Some(container) => container
                .mounts
                .iter()
                .filter(|mount| !mount.read_only)
                .find_map(|mount| {
                    let source = crate::session::capture::canonicalize_or_raw(&mount.host_path);
                    let path =
                        PathBuf::from(&mount.container_path).join(file.strip_prefix(source).ok()?);
                    (path.starts_with(root)
                        && container.host_path(&path, true).as_deref() == Some(file))
                    .then_some(path)
                })
                .context("transcript no longer belongs to the inspected writable native store")?,
        };
        self.canonical_path(&path)
    }

    fn resolve_program(&self, program: &str) -> Result<PathBuf> {
        let path = self.environment.get("PATH").map(String::as_str);
        let Some(container) = &self.container else {
            return which::which_in(program, path, &self.cwd)
                .context("native program is unavailable in the prepared PATH");
        };
        let args = [
            "/bin/sh",
            "-c",
            r#"PATH=$1; export PATH; case "$2" in -*) exit 1;; esac; command -v "$2""#,
            "aoe-native-program",
            path.unwrap_or(""),
            program,
        ]
        .map(str::to_owned);
        let output = crate::session::capture::run_with_timeout_limit(
            container.runtime.exec(
                &container.id,
                self.cwd.to_str().context("native cwd is not UTF-8")?,
                &args,
            ),
            std::time::Duration::from_secs(5),
            "native executable lookup",
            8192,
        )?;
        let output = String::from_utf8(output).context("native executable path is not UTF-8")?;
        let output = output.strip_suffix('\n').unwrap_or(&output);
        anyhow::ensure!(
            !output.is_empty() && !output.contains('\n'),
            "native executable lookup is ambiguous"
        );
        self.canonical_path(std::path::Path::new(output))
    }

    fn hook_capture_context(&self, instance_id: &str) -> Result<Option<CaptureContext>> {
        let leaf = crate::hooks::session_id_leaf(Some(&self.launch_id))?;
        let directory = if self.container.is_some() {
            PathBuf::from(crate::hooks::HOOK_STATUS_BASE_IN_CONTAINER).join(instance_id)
        } else {
            crate::hooks::hook_base_path().join(instance_id)
        };
        let path = self.canonical_path(&directory.join(leaf.as_ref()))?;
        let location = self.physical_location(&path);
        Ok((location.filesystem == "host").then_some(CaptureContext::Hooks(location.path)))
    }
}

pub(super) fn hook_session_observation(
    instance_id: &str,
    active_execution: Option<&ActiveExecution>,
    max_age: Option<std::time::Duration>,
) -> Option<crate::session::poller::SessionIdObservation> {
    let Some(active) = active_execution else {
        return crate::hooks::read_hook_session_id_within(instance_id, "session_id", max_age)
            .map(|sid| crate::session::poller::SessionIdObservation::instance_sidecar(sid, None));
    };
    let CaptureContext::Hooks(source) = active.capture.as_ref()? else {
        return None;
    };
    let leaf = crate::hooks::session_id_leaf(Some(&active.launch_id)).ok()?;
    if source.file_name()? != std::ffi::OsStr::new(leaf.as_ref()) {
        return None;
    }
    let bytes = crate::hooks::read_hook_sidecar_at(
        instance_id,
        source.parent()?,
        &leaf,
        crate::session::capture::MAX_SESSION_ID_LEN + 1,
        max_age,
    )?;
    let sid = std::str::from_utf8(&bytes).ok()?.trim().to_owned();
    if !crate::session::capture::is_valid_session_id(&sid) {
        return None;
    }
    let mut observation = crate::session::poller::SessionIdObservation::instance_sidecar(sid, None);
    observation.execution = Some(active.clone());
    observation.scope_to(active.binding.clone());
    Some(observation)
}

impl Instance {
    pub(super) fn sandbox_launch_environment(
        &self,
        agent: Option<&AgentDef>,
        identity_extension: Option<&(String, String)>,
        profile: &str,
        source: Option<&str>,
        config: &crate::session::config::Config,
    ) -> Result<crate::session::environment::DockerExecEnv> {
        let sandbox = self
            .sandbox_info
            .as_ref()
            .filter(|sandbox| sandbox.enabled)
            .context("sandbox launch has no container")?;
        let managed_codex_home =
            crate::session::config::container_config::managed_codex_home_from_config(
                &self.tool,
                Some(self.get_tool_command()),
                &config.session,
                &self.id,
            )?;
        let mut environment = crate::session::environment::docker_exec_environment(
            sandbox,
            &config.sandbox,
            managed_codex_home.as_deref(),
        );
        environment
            .env
            .push(("AOE_PROFILE".into(), profile.to_owned()));
        environment
            .env
            .push(("AOE_INSTANCE_ID".into(), self.id.clone()));
        if let Some(source) = source {
            environment
                .env
                .push((crate::hooks::SESSION_SOURCE_ENV.into(), source.to_owned()));
        }
        // The container has no launch pid, so the session-id hook identifies a
        // nested agent by this binary name instead.
        if let Some(agent) = agent {
            environment
                .env
                .push(("AOE_AGENT_BIN".into(), agent.binary.to_owned()));
        }
        if let Some((key, value)) = agent.and_then(|agent| {
            agent
                .container_env
                .iter()
                .find(|(key, _)| *key == "PRIME_AGENT_CODING_AGENT_DIR")
        }) {
            environment
                .env
                .push(((*key).to_owned(), (*value).to_owned()));
        }
        if let Some((_, values)) = identity_extension {
            for entry in shell_words::split(values)? {
                let (key, value) = entry
                    .split_once('=')
                    .context("invalid identity publisher environment")?;
                environment.env.push((key.to_owned(), value.to_owned()));
            }
            let root_only = matches!(
                agent
                    .and_then(|agent| agent.session_support.as_ref())
                    .and_then(|support| support.capture.as_ref())
                    .and_then(|capture| capture.backend.identity_publisher()),
                Some(crate::agents::SessionIdentityPublisher::Extension { root_only: true })
            );
            environment.env.push((
                "AOE_SESSION_ROOT_ONLY".into(),
                if root_only { "1" } else { "0" }.into(),
            ));
        }
        environment.docker_args = format!(
            "--env-file {}",
            crate::session::environment::CONTAINER_EXEC_ENV_PATH
        );
        Ok(environment)
    }

    fn native_launch_inputs(
        &self,
        agent: &AgentDef,
        config: &crate::session::config::Config,
    ) -> Result<NativeLaunchInputs> {
        let launch_id = uuid::Uuid::new_v4().to_string();
        let profile = self.effective_profile();
        let identity_extension = self.identity_extension_launch();
        if let Some(sandbox) = self.sandbox_info.as_ref().filter(|sandbox| sandbox.enabled) {
            let docker_env = self.sandbox_launch_environment(
                Some(agent),
                identity_extension.as_ref(),
                &profile,
                Some(&launch_id),
                config,
            )?;
            let runtime = crate::containers::RuntimeExecutionSnapshot::capture(
                &crate::containers::get_container_runtime(),
            )?;
            let container = crate::containers::ContainerExecutionSnapshot::capture(
                runtime,
                &sandbox.container_name,
            )?;
            let cwd = container.runtime.canonical_path(
                &container.id,
                std::path::Path::new(&self.container_workdir()),
            )?;
            let mut environment = crate::session::capture::read_container_environment(
                &container.runtime,
                &container.id,
            )?;
            environment.extend(docker_env.env.iter().cloned());
            Ok(NativeLaunchInputs {
                launch_id,
                environment,
                raw_environment: Vec::new(),
                cwd,
                container: Some(container),
                docker_env: Some(docker_env),
                pane_env: Vec::new(),
                identity_extension,
                profile,
            })
        } else {
            let entries = self.resolved_host_environment_from(config.environment.clone());
            let (mut environment, raw_environment) = host_launch_environment_from(
                std::env::vars_os(),
                &entries,
                (agent.name == "opencode").then_some(config.session.inherit_host_environment),
            );
            environment.insert(crate::hooks::SESSION_SOURCE_ENV.into(), launch_id.clone());
            if let Some((_, values)) = &identity_extension {
                for entry in shell_words::split(values)? {
                    let (key, value) = entry
                        .split_once('=')
                        .context("invalid identity publisher environment")?;
                    environment.insert(key.to_owned(), value.to_owned());
                }
            }
            let pane_env = crate::session::environment::resolve_host_environment_pairs(&entries)
                .into_iter()
                .map(|(key, value)| crate::tmux::PaneEnvMutation::set(key, value))
                .collect();
            Ok(NativeLaunchInputs {
                launch_id,
                environment,
                raw_environment,
                cwd: crate::session::capture::canonicalize_or_raw(&self.project_path),
                container: None,
                docker_env: None,
                pane_env,
                identity_extension,
                profile,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OpenCodeSessionSchema {
    Session,
    SessionV2,
}

impl OpenCodeSessionSchema {
    // Only source-verified releases/builds qualify; CLI generation is not a schema discriminator.
    fn from_version_output(output: &str) -> Option<Self> {
        let version = output.trim();
        let version = version.strip_prefix("opencode v").unwrap_or(version);
        if version == "2.0.24+31bdd8e" {
            return Some(Self::SessionV2);
        }
        let mut parts = version.split('.');
        let mut number = || {
            let part = parts.next()?;
            if part.is_empty()
                || (part.len() > 1 && part.starts_with('0'))
                || !part.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            part.parse::<u16>().ok()
        };
        let version = (number()?, number()?, number()?);
        if parts.next().is_some() {
            return None;
        }
        match version {
            (1, 16, 0 | 2) | (1, 17, 0..=20) | (1, 18, 0..=35) => Some(Self::Session),
            (2, 0, 0..=26) => Some(Self::SessionV2),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("OpenCode native session schema cannot be attested; managed resume and fork are refused")]
pub(super) struct UnattestedOpenCodeSchema;

fn opencode_session_row(
    connection: &rusqlite::Connection,
    sid: &str,
    schema: OpenCodeSessionSchema,
) -> Result<Option<(bool, String)>> {
    use rusqlite::OptionalExtension as _;
    let sql = match schema {
        OpenCodeSessionSchema::Session => "SELECT workspace_id IS NOT NULL, directory FROM session WHERE id = ?1 AND length(CAST(directory AS BLOB)) <= 65536",
        OpenCodeSessionSchema::SessionV2 => "SELECT workspace_id IS NOT NULL, directory FROM session_v2 WHERE id = ?1 AND length(CAST(directory AS BLOB)) <= 65536",
    };
    connection
        .query_row(sql, [sid], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()
        .context("OpenCode store rejected its active session schema or routing row")
}

impl Instance {
    /// Snapshot the actual host executable and its effective environment.
    pub(super) fn host_agent_command(
        &self,
        agent: &crate::agents::AgentDef,
        execution: Option<&NativeExecution>,
    ) -> Option<std::process::Command> {
        if self.is_sandboxed() {
            return None;
        }
        match execution {
            Some(execution) => Some(execution.host_command()),
            None => {
                if !self.launch_invokes_resolved_agent_directly(agent)
                    && !self
                        .execution_agent()
                        .is_ok_and(|actual| actual.name == agent.name)
                {
                    return None;
                }
                let command = self.get_tool_command();
                let extra = if self.command.is_empty() {
                    crate::session::config::quote_model_value_in_args(&self.extra_args)
                } else {
                    self.extra_args.clone()
                };
                if Self::contains_active_shell_syntax(command)
                    || Self::contains_active_shell_syntax(&extra)
                {
                    return None;
                }
                let parsed = super::launch_command::parse_launch_command(command)?;
                let program = parsed.words.first()?;
                let config = crate::session::config::profile_config::resolve_config_or_warn(
                    &self.effective_profile(),
                );
                let entries = self.resolved_host_environment_from(config.environment);
                let (environment, raw_environment) = host_launch_environment_from(
                    std::env::vars_os(),
                    &entries,
                    (agent.name == "opencode").then_some(config.session.inherit_host_environment),
                );
                let path = environment
                    .get("PATH")
                    .map(std::ffi::OsStr::new)
                    .or_else(|| {
                        raw_environment.iter().find_map(|(key, value)| {
                            (key == std::ffi::OsStr::new("PATH")).then_some(value.as_os_str())
                        })
                    });
                match which::which_in(program, path, &self.project_path) {
                    Ok(program) => {
                        let mut command = std::process::Command::new(program);
                        command
                            .current_dir(&self.project_path)
                            .env_clear()
                            .envs(raw_environment)
                            .envs(environment);
                        Some(command)
                    }
                    Err(error) => {
                        tracing::warn!(target: "session.store",
                            agent = %agent.name,
                            error = %error,
                            program,
                            "host launch executable cannot be resolved");
                        None
                    }
                }
            }
        }
    }
}
impl Instance {
    /// The conversation an explicit fork would carry, with the evidence for it:
    /// `Bound` when a binding qualifies the recorded id, `Unattributed` when a
    /// migration left the binding without an execution, `Recorded` when the id
    /// stands alone, so an unqualified parent reaches `terminal_fork_seed` and
    /// is refused as such rather than as a session with no conversation, and
    /// `Unlaunched` for a fork whose launch has not happened. The row's own
    /// native agent is resolved to decide a fork's capability, so a wrapper
    /// whose identity cannot be resolved fails instead of forking.
    pub(crate) fn fork_parent_ref(&self) -> Result<Option<ForkParentRef<'_>>> {
        let (sid, binding) = match &self.resume_intent {
            ResumeIntent::Fork { .. } => return Ok(Some(ForkParentRef::Unlaunched)),
            ResumeIntent::Use(sid) => (Some(sid), self.resume_binding.as_ref()),
            _ => (
                self.agent_session_id.as_ref(),
                self.agent_session_binding.as_ref(),
            ),
        };
        let Some(sid) = sid else {
            return Ok(None);
        };
        match binding {
            Some(binding) if binding.session_id == *sid && binding.is_unattributed() => {
                // The row's own agent decides the fork capability; the store comes
                // from the context the child's launch resolves.
                Ok(Some(ForkParentRef::Unattributed {
                    binding,
                    agent: self.execution_agent()?.name,
                }))
            }
            Some(binding) if binding.session_id == *sid => Ok(Some(ForkParentRef::Bound(binding))),
            // A binding naming a different conversation is an inconsistency,
            // not a recorded id awaiting proof.
            Some(_) => Ok(None),
            None => Ok(Some(ForkParentRef::Recorded(sid))),
        }
    }

    pub(super) fn execution_agent(&self) -> Result<&'static AgentDef> {
        let config =
            crate::session::config::profile_config::resolve_config(&self.effective_profile())?;
        Self::execution_agent_for(&self.tool, self.get_tool_command(), &config.session)
    }

    pub(crate) fn execution_agent_for(
        tool: &str,
        command: &str,
        config: &crate::session::config::SessionConfig,
    ) -> Result<&'static AgentDef> {
        anyhow::ensure!(!Self::contains_active_shell_syntax(command),
            "managed conversation requires a direct native command or an explicitly declared wrapper");
        let words = shell_words::split(command)?;
        let program = words.first().context("empty agent command")?;
        let direct = AGENTS.iter().find(|agent| agent.binary == program);
        let declared = config
            .agent_execution_as
            .get(tool)
            .map(|name| {
                crate::agents::get_agent(name)
                    .context("agent_execution_as names an unknown builtin")
            })
            .transpose()?;
        let logical = crate::agents::get_agent(tool);
        let actual = direct.or(declared).context(
            "wrapper execution identity is unknown: set session.agent_execution_as and session.agent_config_dir; agent_detect_as is only status detection")?;
        anyhow::ensure!(
            logical.is_none_or(|logical| logical.name == actual.name)
                && declared.is_none_or(|declared| declared.name == actual.name),
            "agent command contradicts the selected native execution identity"
        );
        if direct.is_none() {
            let basename = std::path::Path::new(program)
                .file_name()
                .and_then(|name| name.to_str());
            anyhow::ensure!(
                !AGENTS
                    .iter()
                    .any(|agent| Some(agent.binary) == basename && agent.name != actual.name),
                "wrapper command names a different native agent"
            );
            anyhow::ensure!(
                config.agent_config_dir.contains_key(tool),
                "wrapper requires an explicit session.agent_config_dir namespace"
            );
        }
        Ok(actual)
    }

    pub(super) fn managed_user_argv(&self, agent: &AgentDef) -> Result<Option<String>> {
        let extra = if self.command.is_empty() {
            crate::session::config::quote_model_value_in_args(&self.extra_args)
        } else {
            self.extra_args.clone()
        };
        anyhow::ensure!(
            !Self::contains_active_shell_syntax(&extra),
            "managed conversation cannot use active shell syntax in arguments"
        );
        let mut words = shell_words::split(self.get_tool_command())?;
        words.extend(shell_words::split(&extra)?);
        validate_managed_arguments(agent, &words[1..])
    }

    pub(super) fn resolve_native_execution(
        &self,
        target: Option<(&str, Option<&ConversationBinding>, bool)>,
    ) -> Result<NativeExecution> {
        #[cfg(test)]
        if FAIL_NEXT_NATIVE_RESOLUTION.with(|fail| fail.replace(false)) {
            anyhow::bail!("injected transient native resolution failure");
        }
        let config = crate::session::config::repo_config::resolve_config_with_repo(
            &self.effective_profile(),
            std::path::Path::new(&self.project_path),
        )?;
        let agent =
            Self::execution_agent_for(&self.tool, self.get_tool_command(), &config.session)?;
        let session_dir = self.managed_user_argv(agent)?;
        let direct_capture = self.launch_invokes_resolved_agent_directly(agent);
        let target_session_id = target.map(|(sid, _, _)| sid.to_owned());
        let mut resolved_target_session_id = None;
        let mut inputs = self.native_launch_inputs(agent, &config)?;
        anyhow::ensure!(
            inputs.container.as_ref().is_none_or(|container| {
                container.runtime.kind != crate::session::ContainerRuntimeName::AppleContainer
            }),
            "managed conversation requires an immutable container execution identity"
        );
        let words = shell_words::split(self.get_tool_command())?;
        let program =
            inputs.resolve_program(words.first().context("native program is missing")?)?;
        anyhow::ensure!(
            program.is_absolute() && program.to_str().is_some(),
            "native program must have an absolute UTF-8 path"
        );
        let mut prime = None;
        let value = |key: &str| inputs.environment.get(key).cloned();
        let home = value("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .context("native HOME is unavailable")?;
        let absolute = |path: PathBuf| {
            crate::git::template::lexical_normalize(&if path.is_absolute() {
                path
            } else {
                inputs.cwd.join(path)
            })
        };
        // A container routes to an isolated store, so the declaration does not
        // select its root and is filtered out below. It still names a store
        // otherwise, though, and the default-store route rule reads the
        // declaration whether or not a container is in play.
        let declared_store = config
            .session
            .agent_config_dir_for(&self.tool, &home)
            .map(absolute);
        let declared = declared_store
            .clone()
            .filter(|_| inputs.container.is_none());
        let config_home = absolute(
            value("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config")),
        );
        let data_home = absolute(
            value("XDG_DATA_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share")),
        );
        let mut routing = Vec::new();
        let mut case_insensitive_routing: &'static [&'static str] = &[];
        let mut configuration = Vec::new();
        let mut exported_default_store = None;
        let mut pi_root = None;
        let mut pi_transcript_path = None;
        let mut namespace_arguments = Vec::new();
        let mut store_override = None;
        let mut roots = match agent.name {
            "claude" => {
                let recorded_execution = (inputs.container.is_none())
                    .then(|| {
                        target
                            .and_then(|(_, binding, _)| binding)
                            .filter(|binding| binding.is_known())
                            .and_then(|binding| binding.execution.as_ref())
                            .filter(|execution| !execution.stores.is_empty())
                    })
                    .flatten();
                let recorded = recorded_execution.and_then(|execution| execution.stores.first().cloned());
                // The same root is exported here and recorded on the binding
                // below, and the binding canonicalizes it either way: resolve
                // it now so the routed value and the stored one name one path,
                // as the sibling namespaces already do. A new session takes
                // that chain without the recorded store, and the difference
                // between the two is a diagnostic, never what decides.
                let ambient_raw = value("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty());
                let declared_spelling = absolute(declared
                    .clone()
                    .or_else(|| ambient_raw.clone().map(PathBuf::from))
                    .unwrap_or_else(|| home.join(".claude")));
                let resolved = inputs.canonical_path(&declared_spelling);
                let root = match recorded.as_ref() {
                    Some(recorded) => {
                        let root = inputs.canonical_path(&absolute(recorded.clone()))?;
                        // A selector that cannot be resolved still differs
                        // from where this launch runs, and that is the one
                        // case a user can act on, so report the spelling they
                        // wrote rather than dropping the line.
                        let new_session = resolved
                            .as_ref()
                            .ok()
                            .cloned()
                            .unwrap_or_else(|| declared_spelling.clone());
                        let source = if declared.is_some() {
                            "agent_config_dir"
                        } else if ambient_raw.is_some() {
                            "environment"
                        } else {
                            "default"
                        };
                        store_override =
                            (root != new_session).then_some((root.clone(), new_session, source));
                        root
                    }
                    None => resolved
                        .context("the configured Claude store cannot be resolved")?,
                };
                let default = crate::session::capture::is_default_claude_store(&root, &home);
                let explicit = recorded_execution
                    .and_then(|execution| execution.exported_default_store)
                    .unwrap_or_else(|| {
                        let ambient = value("CLAUDE_CONFIG_DIR")
                            .filter(|value| !value.is_empty())
                            .and_then(|value| {
                                inputs.canonical_path(&absolute(PathBuf::from(value))).ok()
                            });
                        crate::session::capture::is_explicit_claude_store_route(
                            &root,
                            &home,
                            declared_store.as_deref(),
                            ambient.as_deref(),
                        )
                    });
                let export = inputs.container.is_some() || explicit || !default;
                exported_default_store = Some(export && default);
                let pinned = root.to_str().context("native store is not UTF-8")?.to_owned();
                routing.push(("CLAUDE_CONFIG_DIR".into(), export.then_some(pinned)));
                vec![root]
            }
            "codex" => {
                // A host single-shot launch is deliberately left unmanaged:
                // the refusal below only governs managed resume and fork, where
                // an unattested preference surface could redirect the
                // conversation this pin claims to continue.
                anyhow::ensure!(inputs.container.is_some() || !crate::process::HAS_CODEX_MANAGED_PREFERENCES, "Codex managed preferences cannot be attested by the local file contract");
                for key in ["CODEX_EXEC_SERVER_URL", "OPENAI_FEDERATION_RULE_ID", "OPENAI_IDENTITY_TOKEN_FILE", "OPENAI_WORKLOAD_IDENTITY_CONTEXT"] {
                    anyhow::ensure!(value(key).is_none(), "managed Codex does not support {key}");
                    routing.push((key.into(), None));
                }
                let root = inputs.canonical_path(&absolute(declared.clone().or_else(|| value("CODEX_HOME").filter(|value| !value.is_empty()).map(PathBuf::from))
                    .unwrap_or_else(|| home.join(".codex"))))?;
                let location = inputs.physical_location(&root);
                anyhow::ensure!(location.filesystem == "host" && location.path.is_dir(), "managed Codex requires an existing local CODEX_HOME");
                for file in [PathBuf::from("/etc/codex/requirements.toml"), PathBuf::from("/etc/codex/managed_config.toml")] {
                    anyhow::ensure!(inputs.read_native_file(&file)?.is_none(), "Codex managed requirements require an independently attested namespace");
                    configuration.push(file);
                }
                let mut sqlite = value("CODEX_SQLITE_HOME").filter(|value| !value.trim().is_empty()).map(|value| absolute(PathBuf::from(value.trim()))).unwrap_or_else(|| root.clone());
                let user_config = root.join("config.toml");
                for file in [PathBuf::from("/etc/codex/config.toml"), user_config.clone()] {
                    if let Some(bytes) = inputs.read_native_file(&file)? {
                        let settings: toml::Table = toml::from_str(std::str::from_utf8(&bytes)?)?;
                        anyhow::ensure!(!settings.contains_key("profile"), "managed Codex does not support a selected configuration profile");
                        anyhow::ensure!(settings.get("cli_auth_credentials_store").is_none_or(|mode| mode.as_str() == Some("file")), "Codex keyring and external authentication do not attest a local namespace");
                        anyhow::ensure!(settings.get("experimental_thread_store").is_none_or(|store| store.get("type").and_then(toml::Value::as_str) == Some("local")), "managed Codex requires its durable local thread store");
                        if let Some(path) = settings.get("sqlite_home") {
                            let path = PathBuf::from(path.as_str().context("Codex sqlite_home must be a path")?);
                            sqlite = if path.is_absolute() { path } else { file.parent().unwrap().join(path) };
                        }
                    }
                    configuration.push(file);
                }
                for directory in inputs.cwd.ancestors() {
                    let file = directory.join(".codex/config.toml");
                    if file == user_config { continue; }
                    if let Some(bytes) = inputs.read_native_file(&file)? {
                        let settings: toml::Table = toml::from_str(std::str::from_utf8(&bytes)?)?;
                        anyhow::ensure!(["sqlite_home", "experimental_thread_store", "cli_auth_credentials_store", "profile"].iter().all(|key| !settings.contains_key(*key)), "Codex project routing requires its native trust and profile resolution");
                    }
                    configuration.push(file);
                }
                let auth_file = root.join("auth.json");
                let Some(bytes) = inputs.read_native_file(&auth_file)? else {
                    bail!("Codex has no auth.json in its resolved configuration directory (session.agent_config_dir on a host launch, else CODEX_HOME, else ~/.codex); sign in with an OpenAI API key and re-run");
                };
                let auth: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(["tokens", "agent_identity", "personal_access_token"].iter().all(|key| auth.get(*key).is_none_or(serde_json::Value::is_null))
                    && auth.get("auth_mode").is_none_or(|mode| mode.is_null() || mode.as_str() == Some("apikey"))
                    && auth.get("OPENAI_API_KEY").and_then(serde_json::Value::as_str).is_some_and(|key| !key.trim().is_empty()), "Codex is not authenticated with a local OpenAI API key; set OPENAI_API_KEY in the auth.json of its resolved configuration directory (session.agent_config_dir on a host launch, else CODEX_HOME, else ~/.codex) and re-run");
                configuration.push(auth_file);
                let sqlite = inputs.canonical_path(&sqlite)?;
                routing.push(("CODEX_HOME".into(), Some(root.to_str().context("Codex home is not UTF-8")?.into())));
                routing.push(("CODEX_SQLITE_HOME".into(), Some(sqlite.to_str().context("Codex SQLite home is not UTF-8")?.into())));
                namespace_arguments.extend(["-c".into(), format!("sqlite_home={}", toml::Value::String(sqlite.to_str().unwrap().into())), "-c".into(), "experimental_thread_store={type=\"local\"}".into(), "-c".into(), "cli_auth_credentials_store=\"file\"".into()]);
                vec![root, sqlite]
            }
            "opencode" => {
                anyhow::ensure!(value("OPENCODE_CONFIG_CONTENT").is_none(), "managed OpenCode does not support OPENCODE_CONFIG_CONTENT");
                let data = data_home.join("opencode");
                let config = absolute(declared.clone().or_else(|| value("OPENCODE_CONFIG_DIR").filter(|value| !value.is_empty()).map(PathBuf::from))
                    .unwrap_or_else(|| config_home.join("opencode")));
                routing.push(("OPENCODE_CONFIG_DIR".into(), Some(config.to_str().context("OpenCode configuration path is not UTF-8")?.to_owned())));
                configuration.push(config);
                let database = if let Some(raw) = value("OPENCODE_DB").filter(|value| !value.is_empty()) {
                    anyhow::ensure!(raw != ":memory:", "managed OpenCode requires a durable database");
                    let path = PathBuf::from(raw);
                    if path.is_absolute() { path } else { data.join(path) }
                } else {
                    anyhow::ensure!(matches!(value("OPENCODE_DISABLE_CHANNEL_DB").as_deref(), Some("1" | "true")),
                        "OpenCode build channel does not prove its database filename; configure OPENCODE_DB with the path reported by opencode debug paths");
                    data.join("opencode.db")
                };
                let database = inputs.canonical_path(&database)?;
                anyhow::ensure!(value("OPENCODE_WORKSPACE_ID").is_none_or(|value| value.is_empty()), "managed OpenCode does not support workspace routing");
                routing.push(("OPENCODE_DB".into(), Some(database.to_str().context("OpenCode database path is not UTF-8")?.into())));
                if let Some(path) = value("OPENCODE_CONFIG").filter(|value| !value.is_empty()).map(PathBuf::from).map(absolute) {
                    routing.push(("OPENCODE_CONFIG".into(), Some(path.to_str().context("OpenCode config path is not UTF-8")?.into())));
                    configuration.push(path);
                }
                routing.push(("OPENCODE_CONFIG_CONTENT".into(), None));
                routing.push(("OPENCODE_WORKSPACE_ID".into(), None));
                vec![database]
            }
            "pi" => {
                let expand = |raw: &str| absolute(if raw == "~" { home.clone() }
                    else if let Some(relative) = raw.strip_prefix("~/") { home.join(relative) }
                    else { PathBuf::from(raw) });
                let agent_dir = inputs.canonical_path(&declared.clone().or_else(|| value("PI_CODING_AGENT_DIR").filter(|value| !value.is_empty()).map(|value| expand(&value)))
                    .unwrap_or_else(|| home.join(".pi/agent")))?;
                routing.push(("PI_CODING_AGENT_DIR".into(), Some(agent_dir.to_str().context("Pi agent directory is not UTF-8")?.into())));
                routing.push(("PI_CODING_AGENT_SESSION_DIR".into(), value("PI_CODING_AGENT_SESSION_DIR")));
                configuration.push(agent_dir.clone());
                let mut selected = session_dir.clone().or_else(|| value("PI_CODING_AGENT_SESSION_DIR"));
                if selected.is_none() {
                    for file in [inputs.cwd.join(".pi/settings.json"), agent_dir.join("settings.json")] {
                        let file = inputs.canonical_path(&file)?;
                        let location = inputs.physical_location(&file);
                        anyhow::ensure!(location.filesystem == "host", "Pi settings require a supported local filesystem projection");
                        configuration.push(file);
                        if let Some(mut settings) = super::session_id::read_session_settings(&location.path)? {
                            if let Some(value) = settings.remove("sessionDir") {
                                selected = Some(value.as_str().context("Pi sessionDir must be a string")?.to_owned());
                                break;
                            }
                        }
                    }
                }
                let custom = selected.is_some();
                let root = inputs.canonical_path(&match selected {
                    Some(ref path) => { anyhow::ensure!(!path.is_empty(), "Pi session directory is empty"); expand(path) },
                    None => agent_dir.join("sessions"),
                })?;
                let mut store = root.clone();
                if let Some((sid, binding, explicit)) = target {
                    if let Some(binding) = binding.filter(|binding| binding.session_id == sid) {
                        if let Some(file) = &binding.transcript_path {
                            anyhow::ensure!(binding.execution.as_ref().is_some_and(|source| source.filesystem == "host"), "Pi transcript filesystem is unsupported");
                            let native = inputs.native_transcript_path(file, &root)?;
                            let parent = native.parent().context("Pi transcript has no parent directory")?;
                            anyhow::ensure!(native.starts_with(&root) && (!custom || parent == root), "Pi transcript does not belong to the configured session directory");
                            let directory = crate::session::AnchoredDir::open(file.parent().context("Pi transcript has no parent")?)?;
                            match directory.regular_lookup(std::path::Path::new(file.file_name().context("Pi transcript has no filename")?))? {
                                Some(true) => {
                                    let (header_sid, _) = crate::session::capture::extract_pi_header_fields(file).context("Pi transcript has no readable session header")?;
                                    anyhow::ensure!(header_sid.as_deref() == Some(sid), "Pi transcript names a different conversation");
                                    pi_transcript_path = Some(native.to_str().context("Pi transcript path is not UTF-8")?.to_owned());
                                }
                                None => anyhow::ensure!(!explicit && file.file_name().and_then(|name| name.to_str()).and_then(|name| name.rsplit_once('_')).and_then(|(_, tail)| tail.strip_suffix(".jsonl")) == Some(sid), "explicit Pi resume requires a materialized transcript"),
                                Some(false) => anyhow::bail!("Pi transcript is not a regular file"),
                            }
                            store = parent.to_path_buf();
                            if session_dir.is_none() {
                                namespace_arguments.extend(["--session-dir".into(), store.to_str().context("Pi store path is not UTF-8")?.into()]);
                            }
                        } else {
                            anyhow::ensure!(!explicit && binding.provenance == ConversationProvenance::Preallocated,
                                "Pi resume requires its exact transcript path; rebind with aoe session set-session-id and --store pointing to the transcript");
                        }
                    }
                }
                pi_root = Some(if namespace_arguments.is_empty() { root } else { store.clone() });
                vec![store]
            }
            "prime-agent" => {
                let root = absolute(declared.clone()
                    .or_else(|| value("PRIME_AGENT_CODING_AGENT_DIR").filter(|value| !value.is_empty()).map(PathBuf::from))
                    .unwrap_or_else(|| home.join(".prime/agent")));
                let root = inputs.canonical_path(&root)?;
                routing.push(("PRIME_AGENT_CODING_AGENT_DIR".into(), Some(root.to_str().context("Prime store is not UTF-8")?.into())));
                let plan = self.prime_agent_launch_plan_from_inputs(&inputs, &root, &home)?;
                configuration.push(root);
                namespace_arguments.extend([
                    "--cwd".into(), plan.container_cwd.clone(),
                    "--session-dir".into(), plan.container_session_dir.to_str().context("Prime session directory is not UTF-8")?.into(),
                ]);
                let stores = vec![plan.container_session_dir.clone()];
                prime = Some(plan);
                stores
            }
            "hermes" => {
                let root = match declared.clone() {
                    Some(root) => root,
                    None if inputs.container.is_some() && config.session.agent_config_dir_for(&self.tool, &home).is_some() => home.join(".hermes"),
                    None => bail!("Hermes managed resume requires an explicit configuration-directory declaration"),
                };
                inputs.validate_hermes_namespace(&root)?;
                if let Some((sid, _, _)) = target {
                    let resolved = inputs.resolve_hermes_target(&root, sid)?;
                    resolved_target_session_id = (resolved != sid).then_some(resolved);
                }
                routing.push(("HERMES_HOME".into(), Some(root.to_str().context("Hermes home is not UTF-8")?.into())));
                for key in ["HERMES_MANAGED_DIR", "TERMINAL_ENV", "TERMINAL_CWD"] {
                    routing.push((key.into(), value(key)));
                }
                if !root.parent().and_then(std::path::Path::file_name).is_some_and(|name| name == "profiles") {
                    namespace_arguments.extend(["--profile".into(), "default".into()]);
                }
                configuration.push(root.clone());
                vec![root.join("state.db")]
            }
            "vibe" => {
                let root = match declared.clone() {
                    Some(root) => root,
                    None if inputs.container.is_some() && config.session.agent_config_dir_for(&self.tool, &home).is_some() => home.join(".vibe"),
                    None => bail!("Vibe managed resume requires an explicit configuration-directory declaration"),
                };
                let root = inputs.canonical_path(&root)?;
                let store = inputs.validate_vibe_namespace(&root, &self.selected_agent_args(), self.is_yolo_mode())?;
                routing.push(("VIBE_HOME".into(), Some(root.to_str().context("Vibe home is not UTF-8")?.into())));
                case_insensitive_routing = VIBE_NAMESPACE_ENVIRONMENT;
                routing.extend(inputs.environment.iter().filter(|(key, _)| {
                    VIBE_NAMESPACE_ENVIRONMENT.iter().any(|name| name.eq_ignore_ascii_case(key))
                }).map(|(key, value)| (key.clone(), Some(value.clone()))));
                configuration.push(root);
                vec![store]
            }
            "omp" => Vec::new(),
            "kimi" | "copilot" => {
                let (key, suffix) = if agent.name == "kimi" { ("KIMI_SHARE_DIR", ".kimi") } else { ("COPILOT_HOME", ".copilot") };
                let root = absolute(declared.clone().or_else(|| value(key).filter(|value| !value.is_empty()).map(PathBuf::from)).unwrap_or_else(|| home.join(suffix)));
                routing.push((key.into(), Some(root.to_str().context("native store is not UTF-8")?.into())));
                vec![root]
            }
            "cursor" => {
                let root = absolute(declared.clone().or_else(|| value("CURSOR_CONFIG_DIR").filter(|value| !value.trim().is_empty()).map(PathBuf::from))
                    .unwrap_or_else(|| if value("XDG_CONFIG_HOME").is_some_and(|value| !value.is_empty()) { config_home.join("cursor") } else { home.join(".cursor") }));
                routing.push(("CURSOR_CONFIG_DIR".into(), Some(root.to_str().context("Cursor store is not UTF-8")?.into())));
                vec![root]
            }
            "gemini" => {
                let configured = value("GEMINI_CLI_HOME").filter(|value| !value.is_empty()).map(PathBuf::from);
                if declared.is_none() && configured.is_none() {
                    for directory in inputs.cwd.ancestors().chain(std::iter::once(home.as_path())) {
                        for file in [directory.join(".env"), directory.join(".gemini/.env")] {
                            anyhow::ensure!(inputs.read_native_file(&file)?.is_none(), "Gemini dotenv may select another home; configure GEMINI_CLI_HOME explicitly for managed conversations");
                            configuration.push(file);
                        }
                    }
                }
                let base = if let Some(root) = declared.as_ref() {
                    anyhow::ensure!(root.file_name().is_some_and(|name| name == ".gemini"), "Gemini configuration directory must be named .gemini");
                    root.parent().context("Gemini home is missing")?.to_path_buf()
                } else { absolute(configured.unwrap_or_else(|| home.clone())) };
                routing.push(("GEMINI_CLI_HOME".into(), Some(base.to_str().context("Gemini home is not UTF-8")?.into())));
                vec![base.join(".gemini")]
            }
            _ => bail!("the effective {} conversation namespace is not attested; managed resume and fork are refused", agent.name),
        };
        if agent.name != "omp" {
            for (key, path) in [
                ("HOME", home),
                ("XDG_CONFIG_HOME", config_home),
                ("XDG_DATA_HOME", data_home),
            ] {
                routing.push((
                    key.into(),
                    Some(
                        absolute(path)
                            .into_os_string()
                            .into_string()
                            .map_err(|_| anyhow::anyhow!("native routing path is not UTF-8"))?,
                    ),
                ));
            }
            routing.push(("XDG_STATE_HOME".into(), value("XDG_STATE_HOME")));
        }
        let path = value("PATH");
        let omp = if agent.name == "omp" {
            let args = if self.command.is_empty() {
                crate::session::config::quote_model_value_in_args(&self.extra_args)
            } else {
                self.selected_agent_args()
            };
            let options = super::OmpCliCaptureOptions::parse(&args)?;
            if let Some(declared) = &declared {
                let directory = inputs.canonical_path(declared)?;
                inputs.environment.insert(
                    "PI_CODING_AGENT_DIR".into(),
                    directory
                        .to_str()
                        .context("declared OMP directory is not UTF-8")?
                        .into(),
                );
            }
            let environment = std::mem::take(&mut inputs.environment);
            let cwd = inputs.cwd.to_str().context("native cwd is not UTF-8")?;
            let mut context = if let Some(container) = &inputs.container {
                crate::session::capture::resolve_omp_store_layout_in_container_with_environment(
                    &container.runtime,
                    &container.id,
                    cwd,
                    environment,
                    &options,
                )?
            } else {
                crate::session::capture::resolve_omp_store_layout_with_environment(
                    environment,
                    cwd,
                    &options,
                )?
            };
            if let Some(declared) = &declared {
                anyhow::ensure!(
                    inputs.canonical_path(&context.agent_dir)?
                        == inputs.canonical_path(declared)?,
                    "OMP profile or dotenv overrides the declared wrapper namespace"
                );
            }
            context.layout.sessions = inputs.canonical_path(&context.layout.sessions)?;
            if let Some((sid, binding, _)) = target {
                let binding = binding.context("OMP resume requires a bound transcript")?;
                let file = binding.transcript_path.as_ref().context(
                    "OMP resume requires its exact transcript; rebind using --store with that file",
                )?;
                anyhow::ensure!(
                    binding
                        .execution
                        .as_ref()
                        .is_some_and(|source| source.filesystem == "host"),
                    "OMP transcript filesystem is unsupported"
                );
                let native = inputs.native_transcript_path(file, &context.layout.sessions)?;
                let parent = native.parent().context("OMP transcript has no parent")?;
                anyhow::ensure!(
                    native.starts_with(&context.layout.sessions)
                        && (context.layout.kind == crate::session::capture::OmpStoreKind::Managed
                            || parent == context.layout.sessions),
                    "OMP transcript is outside the configured namespace"
                );
                let (header_sid, header_cwd) =
                    crate::session::capture::extract_pi_header_fields(file)
                        .context("OMP transcript has no readable session header")?;
                anyhow::ensure!(
                    header_sid.as_deref() == Some(sid),
                    "OMP transcript names a different conversation"
                );
                let header_cwd = header_cwd.context("OMP transcript has no working directory")?;
                anyhow::ensure!(
                    inputs.canonical_path(std::path::Path::new(&header_cwd))?
                        == inputs.canonical_path(&context.cwd)?,
                    "OMP transcript restores a different working directory"
                );
                context.layout.sessions = parent.to_path_buf();
                context.layout.kind = crate::session::capture::OmpStoreKind::Custom;
            }
            if context.layout.kind == crate::session::capture::OmpStoreKind::Custom {
                namespace_arguments.extend([
                    "--session-dir".into(),
                    context
                        .layout
                        .sessions
                        .to_str()
                        .context("OMP session directory is not UTF-8")?
                        .into(),
                ]);
            }
            namespace_arguments.extend([
                "--profile".into(),
                context.profile.as_deref().unwrap_or("default").into(),
            ]);
            roots = vec![context.layout.sessions.clone()];
            configuration.push(context.agent_dir.clone());
            routing = context.launcher_routing.clone();
            Some(context)
        } else {
            None
        };
        routing.push(("PATH".into(), path));
        routing.push((
            crate::hooks::SESSION_SOURCE_ENV.into(),
            Some(inputs.launch_id.clone()),
        ));
        let mut stores = Vec::with_capacity(roots.len());
        let mut filesystem = None;
        for root in roots {
            let root = inputs.canonical_path(&root)?;
            let location = inputs.physical_location(&root);
            anyhow::ensure!(
                filesystem
                    .as_ref()
                    .is_none_or(|domain| domain == &location.filesystem),
                "native stores span incompatible filesystems"
            );
            filesystem.get_or_insert(location.filesystem);
            stores.push(location.path);
        }
        let configuration = configuration
            .into_iter()
            .map(|path| {
                inputs
                    .canonical_path(&path)
                    .map(|path| inputs.physical_location(&path))
            })
            .collect::<Result<Vec<_>>>()?;
        let cwd = if let Some(context) = &omp {
            inputs.canonical_path(&context.cwd)?
        } else if let Some(plan) = &prime {
            inputs.canonical_path(std::path::Path::new(&plan.container_cwd))?
        } else {
            inputs.cwd.clone()
        };
        let cwd = inputs.physical_location(&cwd);
        let capture = if agent
            .session_support
            .as_ref()
            .and_then(|support| support.capture.as_ref())
            .is_some_and(|capture| {
                capture.reads_hook_sidecar(if inputs.container.is_some() {
                    capture.sandbox
                } else {
                    capture.host
                })
            }) {
            inputs.hook_capture_context(&self.id)?
        } else if let Some(plan) = prime
            .take()
            .filter(|_| direct_capture && inputs.container.is_some())
        {
            let sidecar = inputs.identity_extension.as_ref().map(|_| {
                super::SessionSidecarSource::SandboxDir(
                    plan.store.join("aoe-session").join(&self.id),
                )
            });
            Some(CaptureContext::Prime { plan, sidecar })
        } else if let Some(root) = pi_root {
            if let Some(path) = inputs
                .environment
                .get("AOE_SESSION_ID_FILE")
                .filter(|_| inputs.identity_extension.is_some())
            {
                let path = inputs.canonical_path(
                    PathBuf::from(path)
                        .parent()
                        .context("Pi publication has no directory")?,
                )?;
                let source = match &inputs.container {
                    Some(container) => super::SessionSidecarSource::SandboxDir(
                        container
                            .host_path(&path, true)
                            .context("Pi publication is not in a writable local mount")?,
                    ),
                    None => super::SessionSidecarSource::HostHooks(path),
                };
                Some(CaptureContext::Pi { source, root })
            } else {
                None
            }
        } else if matches!(agent.name, "codex" | "gemini" | "kimi" | "hermes")
            && direct_capture
            && inputs.container.is_some()
            && filesystem.as_deref() == Some("host")
        {
            let store = stores.first().context("native capture store is missing")?;
            let root = if agent.name == "hermes" {
                store
                    .parent()
                    .context("Hermes database has no directory")?
                    .to_path_buf()
            } else {
                store.clone()
            };
            Some(CaptureContext::Store {
                root,
                cwd: inputs
                    .cwd
                    .to_str()
                    .context("native capture cwd is not UTF-8")?
                    .into(),
            })
        } else {
            None
        };
        let pi_pinnable = agent.name == "pi"
            && inputs.runs_host_path_binary("pi", &program)
            && crate::agents::pi_supports_session_id_flag();
        let opencode_preassign = agent.name == "opencode"
            && inputs.container.is_none()
            && config.session.opencode_preassign_session_id
            && direct_capture;
        let mut execution = NativeExecution {
            agent,
            binding: ExecutionBinding {
                agent: agent.name.into(),
                stores,
                configuration,
                exported_default_store,
                cwd: cwd.path,
                cwd_filesystem: cwd.filesystem,
                filesystem: filesystem.context("native conversation store is unavailable")?,
            },
            routing,
            case_insensitive_routing,
            omp: omp.filter(|_| direct_capture),
            inputs,
            program,
            capture,
            pi_transcript_path,
            namespace_arguments,
            target_session_id,
            resolved_target_session_id,
            pi_pinnable,
            opencode_preassign,
            opencode_schema: None,
            store_override,
        };
        if agent.name == "opencode" {
            if let Some((sid, _, _)) = target {
                execution.opencode_schema = Some(execution.opencode_session_schema()?);
                execution.validate_opencode_session_target(sid)?;
            }
        }
        Ok(execution)
    }

    pub(crate) fn conversation_target(&self) -> Option<(&str, Option<&ConversationBinding>, bool)> {
        Some(match &self.resume_intent {
            ResumeIntent::Cleared => return None,
            ResumeIntent::Use(sid) => (sid.as_str(), self.resume_binding.as_ref(), true),
            ResumeIntent::Fork { from } => (from.as_str(), self.resume_binding.as_ref(), true),
            ResumeIntent::Default => (
                self.agent_session_id.as_deref()?,
                self.agent_session_binding.as_ref(),
                false,
            ),
        })
    }

    /// Whether two executions describe one context.
    ///
    /// Host paths compare by local filesystem identity. Paths in every other
    /// filesystem domain compare as pure lexical paths.
    pub(super) fn execution_identity_matches(
        left: &ExecutionBinding,
        right: &ExecutionBinding,
    ) -> bool {
        fn paths_match(left: &std::path::Path, right: &std::path::Path, filesystem: &str) -> bool {
            if left == right {
                return true;
            }
            let identity = |path: &std::path::Path| {
                if filesystem == "host" {
                    crate::session::capture::canonicalize_allowing_missing_leaf(path)
                        .unwrap_or_else(|| crate::git::template::lexical_normalize(path))
                } else {
                    crate::git::template::lexical_normalize(path)
                }
            };
            identity(left) == identity(right)
        }
        fn locations_match(left: &[ExecutionLocation], right: &[ExecutionLocation]) -> bool {
            left.len() == right.len()
                && left.iter().zip(right).all(|(left, right)| {
                    left.filesystem == right.filesystem
                        && paths_match(&left.path, &right.path, &left.filesystem)
                })
        }
        left.agent == right.agent
            && left.filesystem == right.filesystem
            && left.cwd_filesystem == right.cwd_filesystem
            && paths_match(&left.cwd, &right.cwd, &left.cwd_filesystem)
            && locations_match(&left.configuration, &right.configuration)
            && left.stores.len() == right.stores.len()
            && left
                .stores
                .iter()
                .zip(&right.stores)
                .all(|(left_store, right_store)| {
                    paths_match(left_store, right_store, &left.filesystem)
                })
    }

    pub(super) fn validate_conversation_target(
        &self,
        execution: &ExecutionBinding,
        target_session_id: Option<&str>,
    ) -> Result<()> {
        let Some((sid, binding, explicit)) = self.conversation_target() else {
            return Ok(());
        };
        anyhow::ensure!(
            Some(sid) == target_session_id,
            "conversation changed during launch preparation; retry with its latest publication"
        );
        let binding = binding.filter(|binding| binding.session_id == sid)
            .context("conversation provenance is unknown; use aoe session set-session-id with an explicitly configured execution identity and store before resuming or forking")?;
        anyhow::ensure!(binding.is_known() || binding.is_unattributed() || (!explicit && binding.provenance == ConversationProvenance::Preallocated),
            "conversation has not been observed or explicitly asserted; a preallocated ID is not a forkable conversation");
        // Default may adopt a known ID after a context move when agent and filesystem agree.
        anyhow::ensure!(
            binding
                .execution
                .as_ref()
                .is_some_and(|bound| Self::execution_identity_matches(bound, execution))
                || !binding.is_known()
                || (matches!(self.resume_intent, ResumeIntent::Default)
                    && binding.is_known()
                    && binding.execution.as_ref().is_some_and(|bound| {
                        bound.agent == execution.agent
                            && bound.filesystem == execution.filesystem
                            && bound.cwd_filesystem == execution.cwd_filesystem
                    })),
            "conversation execution identity, store or working directory differs from this launch; restore its context or explicitly rebind the intended conversation"
        );
        Ok(())
    }
    /// The native execution the carry must attest before copying anything.
    ///
    /// Resolving without the source binding keeps the recorded store from
    /// masking the configured destination, and the identity match then proves
    /// the conversation's context (cwd, configuration, filesystem) survives
    /// the store relocation.
    pub(crate) fn attested_carry_destination(&self) -> Result<ExecutionBinding> {
        let native = self.resolve_native_execution(None)?;
        anyhow::ensure!(
            native.agent.name == "claude" && native.binding.filesystem == "host",
            "conversation carry requires a host Claude launch destination"
        );
        Ok(native.binding)
    }

    /// Whether a conversation binding's context survives relocation to
    /// `destination` with only its store replaced.
    pub(crate) fn execution_matches_destination(
        execution: &ExecutionBinding,
        destination: &ExecutionBinding,
    ) -> bool {
        let mut expected = execution.clone();
        expected.stores = destination.stores.clone();
        Self::execution_identity_matches(&expected, destination)
    }
}

pub(super) fn validate_managed_arguments(
    agent: &AgentDef,
    words: &[String],
) -> Result<Option<String>> {
    let mut index = 0;
    let mut session_dir = None;
    while index < words.len() {
        let word = &words[index];
        let (key, inline) = word
            .split_once('=')
            .map_or((word.as_str(), None), |(key, value)| (key, Some(value)));
        let (values, switches): (&[&str], &[&str]) = match agent.name {
            "claude" => (
                &[
                    "--model",
                    "--fallback-model",
                    "--effort",
                    "--permission-mode",
                    "--append-system-prompt",
                    "--system-prompt",
                ],
                &[
                    "--dangerously-skip-permissions",
                    "--allow-dangerously-skip-permissions",
                    "--verbose",
                ],
            ),
            "codex" => (
                &[
                    "--model",
                    "-m",
                    "--sandbox",
                    "-s",
                    "--ask-for-approval",
                    "-a",
                    "--config",
                    "-c",
                ],
                &[
                    "--full-auto",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--yolo",
                    "--search",
                    "--no-alt-screen",
                    "--verbose",
                ],
            ),
            "opencode" => (
                &["--model", "-m", "--agent", "--prompt"],
                &[
                    "--auto",
                    "--yolo",
                    "--dangerously-skip-permissions",
                    "--verbose",
                ],
            ),
            "omp" => (
                &[
                    "--model",
                    "--thinking",
                    "-m",
                    "--system-prompt",
                    "--append-system-prompt",
                    "--agent",
                    "--session-dir",
                    "--profile",
                    "--cwd",
                ],
                &["--yolo", "--dangerously-skip-permissions", "--verbose"],
            ),
            "prime-agent" => (
                &[
                    "--model",
                    "-m",
                    "--system-prompt",
                    "--append-system-prompt",
                    "--agent",
                    "--session-dir",
                    "--cwd",
                ],
                &[
                    "--yolo",
                    "--dangerously-skip-permissions",
                    "--allow-all-tools",
                    "--trust-all-tools",
                    "--verbose",
                ],
            ),
            _ => (
                &[
                    "--model",
                    "-m",
                    "--system-prompt",
                    "--append-system-prompt",
                    "--agent",
                    "--session-dir",
                ],
                &[
                    "--yolo",
                    "--dangerously-skip-permissions",
                    "--allow-all-tools",
                    "--trust-all-tools",
                    "--verbose",
                ],
            ),
        };
        if values.contains(&key) {
            anyhow::ensure!(
                agent.name != "prime-agent"
                    || !matches!(key, "--cwd" | "--session-dir")
                    || inline.is_none(),
                "Prime namespace options require a separate value"
            );
            let value = if let Some(value) = inline {
                value
            } else {
                index += 1;
                words
                    .get(index)
                    .context("managed command option is missing its value")?
            };
            if key == "--session-dir" {
                anyhow::ensure!(
                    matches!(agent.name, "pi" | "omp" | "prime-agent"),
                    "this agent does not support a managed session directory"
                );
                session_dir = Some(value.to_owned());
            }
            if agent.name == "codex" && matches!(key, "--config" | "-c") {
                let (setting, _) = value
                    .split_once('=')
                    .context("Codex config requires key=value")?;
                anyhow::ensure!(
                    [
                        "developer_instructions",
                        "model",
                        "model_reasoning_effort",
                        "model_reasoning_summary",
                        "model_verbosity",
                        "service_tier"
                    ]
                    .contains(&setting),
                    "Codex configuration key {setting} is not supported for a managed conversation"
                );
            }
        } else if (switches.contains(&key) && inline.is_none())
            || (index == 0 && agent.launch_subcommand == Some(word.as_str()))
        {
        } else if !word.starts_with('-')
            && index + 1 == words.len()
            && matches!(agent.name, "claude" | "codex")
        {
            let commands: &[&str] = if agent.name == "claude" {
                &[
                    "agents",
                    "attach",
                    "auth",
                    "auto-mode",
                    "doctor",
                    "gateway",
                    "import",
                    "install",
                    "logs",
                    "mcp",
                    "plugin",
                    "plugins",
                    "project",
                    "respawn",
                    "rm",
                    "setup-token",
                    "stop",
                    "kill",
                    "ultrareview",
                    "update",
                    "upgrade",
                ]
            } else {
                &[
                    "resume",
                    "fork",
                    "exec",
                    "cloud",
                    "app-server",
                    "login",
                    "logout",
                    "mcp",
                    "mcp-server",
                    "completion",
                    "debug",
                    "apply",
                    "sandbox",
                    "review",
                ]
            };
            anyhow::ensure!(
                !commands.contains(&word.as_str()),
                "native subcommand {word} is not supported for a managed conversation"
            );
        } else {
            bail!("argument {key} is not supported for a managed {} conversation; remove native selectors and unsupported context overrides", agent.name);
        }
        index += 1;
    }
    Ok(session_dir)
}

impl Instance {
    pub(super) fn freeze_native_invocation(
        &self,
        command: String,
        execution: Option<&super::execution::NativeExecution>,
        launch_context: AgentLaunchContext<'_>,
    ) -> Result<String> {
        let Some(execution) = execution else {
            return Ok(command);
        };
        let mut words = shell_words::split(&command)?;
        *words.first_mut().context("native program is missing")? = execution
            .program
            .to_str()
            .context("native executable path is not UTF-8")?
            .to_owned();
        // A shared service may have opened a different database.
        if execution.agent.name == "opencode" {
            match launch_context.generation {
                crate::agents::AgentGeneration::Current => words.push("--standalone".into()),
                crate::agents::AgentGeneration::Legacy => {},
                crate::agents::AgentGeneration::Unknown => anyhow::bail!(
                    "OpenCode --help did not establish a generation; the selected native database cannot be guaranteed, so the launch was refused"
                ),
            }
        }
        Ok(shell_words::join(words))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversationState {
    pub(crate) session_id: Option<String>,
    pub(crate) binding: Option<ConversationBinding>,
    pub(crate) intent: ResumeIntent,
    pub(crate) resume_binding: Option<ConversationBinding>,
    pub(crate) active: Option<ActiveExecution>,
    pub(crate) pi_session_path: Option<String>,
}

impl ConversationState {
    pub(crate) fn matches(&self, instance: &Instance) -> bool {
        self.session_id == instance.agent_session_id
            && self.binding == instance.agent_session_binding
            && self.intent == instance.resume_intent
            && self.resume_binding == instance.resume_binding
            && self.active == instance.active_execution
            && self.pi_session_path == instance.pi_session_path
    }
}

impl Instance {
    pub(crate) fn conversation_state(&self) -> ConversationState {
        ConversationState {
            session_id: self.agent_session_id.clone(),
            binding: self.agent_session_binding.clone(),
            intent: self.resume_intent.clone(),
            resume_binding: self.resume_binding.clone(),
            active: self.active_execution.clone(),
            pi_session_path: self.pi_session_path.clone(),
        }
    }

    pub(crate) fn set_agent_conversation(
        &mut self,
        sid: Option<String>,
        binding: Option<ConversationBinding>,
        pi_session_path: Option<String>,
    ) {
        self.agent_session_binding =
            binding.filter(|binding| Some(&binding.session_id) == sid.as_ref());
        self.pi_session_path = pi_session_path.filter(|_| sid.is_some());
        self.agent_session_id = sid;
    }
    /// The binding an observation is allowed to establish for this instance.
    ///
    /// An observation without launch evidence cannot qualify a conversation, so
    /// it may refresh the published id and transcript path but must keep the
    /// binding an earlier qualified publication established.
    pub(super) fn observed_binding(
        &self,
        observation: &crate::session::poller::SessionIdObservation,
    ) -> Option<ConversationBinding> {
        observation.conversation_binding().or_else(|| {
            self.agent_session_binding
                .clone()
                .filter(|binding| binding.session_id == observation.sid)
        })
    }

    /// A sidecar observation that named no transcript may refresh the SID without
    /// erasing the path already published for that same conversation.
    pub(super) fn observed_pi_session_path(
        &self,
        observation: &crate::session::poller::SessionIdObservation,
    ) -> Option<String> {
        observation.pi_session_path.clone().or_else(|| {
            let id_only_sidecar = observation.source().is_none()
                && matches!(
                    &observation.guard,
                    crate::session::poller::SessionIdGuard::InstanceSidecar { transcript: None }
                )
                && self.agent_session_id.as_deref() == Some(observation.sid.as_str());
            id_only_sidecar
                .then(|| self.pi_session_path.clone())
                .flatten()
        })
    }

    pub(crate) fn apply_conversation_observation(
        &mut self,
        observation: &crate::session::poller::SessionIdObservation,
    ) {
        let binding = self.observed_binding(observation);
        let pi_session_path = self.observed_pi_session_path(observation);
        self.set_agent_conversation(Some(observation.sid.clone()), binding, pi_session_path);
    }

    pub(crate) fn asserted_resume_binding(
        &self,
        sid: &str,
        store: Option<&std::path::Path>,
    ) -> Result<ConversationBinding> {
        anyhow::ensure!(
            crate::session::capture::is_valid_session_id(sid),
            "invalid conversation ID"
        );
        let mut execution = if store.is_none() {
            self.active_execution
                .as_ref()
                .map(|active| active.binding.clone())
        } else {
            None
        }
        .map(Ok)
        .unwrap_or_else(|| {
            self.resolve_native_execution(None)
                .map(|execution| execution.binding)
                .context("aoe session set-session-id cannot resolve the native execution identity for this context")
        })?;
        anyhow::ensure!(
            crate::agents::get_agent(&execution.agent)
                .is_some_and(|agent| agent.session_support.is_some()),
            "agent does not support exact native resume"
        );
        let mut transcript_path = None;
        if matches!(execution.agent.as_str(), "pi" | "omp") {
            let file = store.context("recovery requires --store with the exact transcript file")?;
            anyhow::ensure!(
                !self.is_sandboxed() && file.is_absolute(),
                "recovery requires an absolute host transcript path"
            );
            let file = file.canonicalize().context("transcript is unavailable")?;
            let primary = execution
                .stores
                .first_mut()
                .context("native store is unavailable")?;
            anyhow::ensure!(
                file.starts_with(&*primary),
                "transcript is outside the configured store"
            );
            let (header_sid, _) = crate::session::capture::extract_pi_header_fields(&file)
                .context("transcript has no readable session header")?;
            anyhow::ensure!(
                header_sid.as_deref() == Some(sid),
                "transcript names a different conversation"
            );
            *primary = file
                .parent()
                .context("transcript has no parent")?
                .to_path_buf();
            transcript_path = Some(file);
        } else if let Some(store) = store {
            anyhow::ensure!(
                execution.agent.as_str() == "claude",
                "--store routing is only supported for Claude; other agents resolve their store from configuration"
            );
            anyhow::ensure!(
                !self.is_sandboxed(),
                "sandbox recovery must use its managed mounted store"
            );
            let primary = execution
                .stores
                .first_mut()
                .context("native store is unavailable")?;
            anyhow::ensure!(
                store.is_absolute(),
                "--store must be an absolute native store path"
            );
            *primary = crate::session::capture::canonicalize_or_raw(
                store.to_str().context("store path must be UTF-8")?,
            );
            let selected_home = super::hooks::host_home(&self.resolved_host_environment())
                .context("native HOME is unavailable")?;
            execution.exported_default_store = Some(
                crate::git::template::lexical_normalize(store)
                    != crate::git::template::lexical_normalize(&selected_home.join(".claude"))
                    && crate::session::capture::is_default_claude_store(primary, &selected_home),
            );
        }
        Ok(ConversationBinding {
            session_id: sid.into(),
            execution: Some(execution),
            provenance: ConversationProvenance::Asserted,
            transcript_path,
        })
    }

    pub(crate) fn adopt_conversation_state(&mut self, state: ConversationState) {
        self.settle_poller_for(state.active.as_ref());
        self.set_agent_conversation(state.session_id, state.binding, state.pi_session_path);
        self.resume_intent = state.intent;
        self.resume_binding = state.resume_binding;
        self.active_execution = state.active;
    }

    /// Take on the execution `src` launched, settling the session-id poller with it.
    ///
    /// A poller is only usable by a row holding the execution it was installed for: the drain
    /// erases an observation that names another execution, and a launch-scoped one reads the
    /// other launch's file. A poller for another execution is stopped here; the repair walk
    /// installs one for this pane once the relaunch that stamped it says the pane is new.
    pub(crate) fn adopt_active_execution(&mut self, src: &Self) {
        self.settle_poller_for(src.active_execution.as_ref());
        self.active_execution = src.active_execution.clone();
    }

    pub(super) fn capture_store_dir(&self) -> Option<PathBuf> {
        if let Some(active) = self.active_execution.as_ref() {
            return match &active.capture {
                Some(super::CaptureContext::Store { root, .. }) => Some(root.clone()),
                Some(super::CaptureContext::Pi { root, .. }) => {
                    active.container.as_ref().map_or_else(
                        || Some(root.clone()),
                        |container| container.host_path(root, false),
                    )
                }
                Some(super::CaptureContext::Prime { plan, .. }) => Some(plan.store.clone()),
                _ => None,
            };
        }
        self.sandbox_capture_store_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn opencode_host_snapshot_enforces_selection_and_explicit_override_precedence() {
        use std::ffi::{OsStr, OsString};
        use std::os::unix::ffi::OsStrExt;
        let vars = [
            ("HOME", "/review/home"),
            ("PATH", "/review/bin"),
            ("XDG_DATA_HOME", "/review/data"),
            ("DISPLAY", ":17"),
            ("OPENCODE_DB", "/review/store.db"),
            ("OPENCODE_CONFIG_CONTENT", "{}"),
            ("OPENCODE_WORKSPACE_ID", "unsupported-workspace"),
            ("REVIEW_UNLISTED", "ambient"),
            ("REVIEW_OVERRIDE", "ambient"),
            ("AOE_REVIEW_INTERNAL", "fixture"),
            ("AGENT_OF_EMPIRES_REVIEW_INTERNAL", "fixture"),
            ("TERM", "parent-term"),
            ("TMUX", "parent-tmux"),
            ("TMUX_PANE", "%parent"),
        ];
        let entries = vec![
            "REVIEW_OVERRIDE=explicit".into(),
            "REVIEW_DECLARED=explicit".into(),
            "REVIEW_RAW_OVERRIDE=explicit".into(),
        ];
        for inherit in [false, true] {
            let mut captured = vars
                .iter()
                .map(|(key, value)| (OsString::from(*key), OsString::from(*value)))
                .collect::<Vec<_>>();
            captured.extend([
                (
                    OsString::from("REVIEW_RAW_VALUE"),
                    OsStr::from_bytes(b"\xff").to_owned(),
                ),
                (
                    OsStr::from_bytes(b"REVIEW.RAW_\xff").to_owned(),
                    OsString::from("fixture"),
                ),
                (
                    OsStr::from_bytes(b"AOE_REVIEW_\xff").to_owned(),
                    OsString::from("fixture"),
                ),
                (
                    OsString::from("REVIEW_RAW_OVERRIDE"),
                    OsStr::from_bytes(b"\xfe").to_owned(),
                ),
            ]);
            let (environment, raw) =
                host_launch_environment_from(captured, &entries, Some(inherit));
            for (key, value) in &vars[..7] {
                assert_eq!(
                    environment.get(*key).map(String::as_str),
                    Some(*value),
                    "{inherit}:{key}"
                );
            }
            assert_eq!(
                environment.get("REVIEW_UNLISTED").map(String::as_str),
                inherit.then_some("ambient")
            );
            assert_eq!(
                environment.get("REVIEW_OVERRIDE").map(String::as_str),
                Some("explicit")
            );
            assert_eq!(
                environment.get("REVIEW_DECLARED").map(String::as_str),
                Some("explicit")
            );
            assert_eq!(
                environment.get("REVIEW_RAW_OVERRIDE").map(String::as_str),
                Some("explicit")
            );
            for key in [
                "AOE_REVIEW_INTERNAL",
                "AGENT_OF_EMPIRES_REVIEW_INTERNAL",
                "TERM",
                "TMUX",
                "TMUX_PANE",
            ] {
                assert!(!environment.contains_key(key), "{inherit}:{key}");
            }
            assert_eq!(
                raw.iter()
                    .find(|(key, _)| key == "REVIEW_RAW_VALUE")
                    .map(|(_, value)| value.as_bytes()),
                inherit.then_some(b"\xff".as_slice())
            );
            assert_eq!(
                raw.iter()
                    .any(|(key, _)| key == OsStr::from_bytes(b"REVIEW.RAW_\xff")),
                inherit
            );
            assert!(!raw
                .iter()
                .any(|(key, _)| key.as_bytes().starts_with(b"AOE_")));
            assert!(!raw.iter().any(|(key, _)| key == "REVIEW_RAW_OVERRIDE"));
        }
        let (other_agent, _) = host_launch_environment_from(
            vars.map(|(key, value)| (key.into(), value.into())),
            &[],
            None,
        );
        assert_eq!(
            other_agent.get("REVIEW_UNLISTED").map(String::as_str),
            Some("ambient")
        );
        assert_eq!(
            other_agent.get("AOE_REVIEW_INTERNAL").map(String::as_str),
            Some("fixture")
        );
    }

    #[cfg(unix)]
    #[test]
    fn opencode_routing_consumes_the_physical_store_and_compares_native_cwd() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("host");
        let native = temp.path().join("native");
        for directory in [&host, &native] {
            std::fs::create_dir_all(directory.join("checkout")).unwrap();
        }
        let transport = temp.path().join("transport");
        // Run the production path resolver; replace only the container transport.
        std::fs::write(&transport, "#!/bin/sh\n[ \"$1\" = exec ] && [ \"$2\" = -w ] && [ \"$3\" = / ] && [ \"$4\" = fixture-id ] || exit 2\nshift 4\nexec \"$@\"\n").unwrap();
        std::fs::set_permissions(&transport, std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = host.join("opencode.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, workspace_id TEXT, directory TEXT NOT NULL)").unwrap();
        for (sid, workspace, directory) in [
            (
                "native",
                None,
                native.join("checkout").display().to_string(),
            ),
            (
                "physical",
                None,
                host.join("checkout").display().to_string(),
            ),
            ("relative", None, ".".into()),
            (
                "workspace",
                Some("remote"),
                native.join("checkout").display().to_string(),
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO session VALUES (?1, ?2, ?3)",
                    rusqlite::params![sid, workspace, directory],
                )
                .unwrap();
        }
        let inputs = NativeLaunchInputs {
            launch_id: "review".into(),
            environment: std::collections::HashMap::new(),
            raw_environment: Vec::new(),
            cwd: native.join("checkout"),
            profile: "default".into(),
            container: Some(crate::containers::ContainerExecutionSnapshot {
                runtime: crate::containers::RuntimeExecutionSnapshot {
                    kind: crate::session::ContainerRuntimeName::Docker,
                    program: transport,
                    cwd: temp.path().into(),
                    endpoint: String::new(),
                    local_mounts: true,
                    routing: Vec::new(),
                    global_arguments: Vec::new(),
                },
                name: "fixture".into(),
                id: "fixture-id".into(),
                mounts: vec![crate::containers::VolumeMount {
                    host_path: host.display().to_string(),
                    container_path: native.display().to_string(),
                    read_only: false,
                }],
                shadow_mounts: Vec::new(),
            }),
            docker_env: None,
            pane_env: Vec::new(),
            identity_extension: None,
        };
        let store = inputs.physical_location(&native.join("opencode.db"));
        let cwd = inputs.physical_location(&inputs.cwd);
        let mut execution = NativeExecution {
            agent: crate::agents::get_agent("opencode").unwrap(),
            binding: ExecutionBinding {
                agent: "opencode".into(),
                stores: vec![store.path],
                filesystem: store.filesystem,
                cwd: cwd.path,
                cwd_filesystem: cwd.filesystem,
                configuration: Vec::new(),
                exported_default_store: None,
            },
            routing: Vec::new(),
            case_insensitive_routing: &[],
            omp: None,
            inputs,
            program: PathBuf::from("opencode"),
            capture: None,
            pi_transcript_path: None,
            namespace_arguments: Vec::new(),
            target_session_id: None,
            resolved_target_session_id: None,
            pi_pinnable: false,
            opencode_preassign: false,
            opencode_schema: Some(OpenCodeSessionSchema::Session),
            store_override: None,
        };
        execution
            .validate_opencode_session_target("native")
            .unwrap();
        for (sid, reason) in [
            ("physical", "working directory"),
            ("relative", "working directory"),
            ("workspace", "workspace"),
        ] {
            let error = execution
                .validate_opencode_session_target(sid)
                .unwrap_err()
                .to_string();
            assert!(error.contains(reason), "{sid}: {error}");
        }
        execution
            .inputs
            .container
            .as_mut()
            .unwrap()
            .runtime
            .local_mounts = false;
        let remote_store = execution
            .inputs
            .physical_location(&native.join("opencode.db"));
        execution.binding.stores = vec![remote_store.path];
        execution.binding.filesystem = remote_store.filesystem;
        let error = execution
            .validate_opencode_session_target("native")
            .unwrap_err()
            .to_string();
        assert!(error.contains("local database projection"), "{error}");
    }

    #[test]
    fn opencode_build_identity_refuses_unattested_versions_and_aliases() {
        use OpenCodeSessionSchema::{Session, SessionV2};
        for (version, expected) in [
            ("1.16.0\n", Some(Session)),
            ("1.16.2", Some(Session)),
            ("1.17.20", Some(Session)),
            ("1.18.35", Some(Session)),
            ("opencode v2.0.0\n", Some(SessionV2)),
            ("opencode v2.0.26", Some(SessionV2)),
            ("opencode v2.0.24+31bdd8e", Some(SessionV2)),
            ("1.16.1", None),
            ("1.17.21", None),
            ("1.18.36", None),
            ("2.0.27", None),
            ("2.1.0", None),
            ("2.0.24+unknown", None),
            ("2.0.24-preview", None),
            ("0.0.0-v2-20261008", None),
            ("local", None),
            ("02.0.24", None),
            ("2.00.24", None),
            ("2.0.024", None),
            ("2.0.24 extra", None),
            ("2.0.24\n2.0.24", None),
            ("", None),
        ] {
            assert_eq!(
                OpenCodeSessionSchema::from_version_output(version),
                expected,
                "{version:?}"
            );
        }
    }

    #[test]
    fn opencode_routing_reads_only_the_attested_active_schema() {
        use OpenCodeSessionSchema::{Session, SessionV2};
        let store = rusqlite::Connection::open_in_memory().unwrap();
        store.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, workspace_id TEXT, directory TEXT NOT NULL);
            CREATE TABLE session_v2 (id TEXT PRIMARY KEY, workspace_id TEXT, directory TEXT NOT NULL);
            INSERT INTO session VALUES ('shared', NULL, '/legacy/100%');
            INSERT INTO session_v2 VALUES ('shared', 'remote', '/current');
            INSERT INTO session VALUES ('legacy-only', NULL, '/legacy');
            INSERT INTO session_v2 VALUES ('current-only', NULL, '/current');").unwrap();
        for (schema, sid, expected) in [
            (Session, "shared", Some((false, "/legacy/100%".into()))),
            (SessionV2, "shared", Some((true, "/current".into()))),
            (Session, "current-only", None),
            (SessionV2, "legacy-only", None),
            (Session, "absent", None),
            (SessionV2, "absent", None),
        ] {
            assert_eq!(
                opencode_session_row(&store, sid, schema).unwrap(),
                expected,
                "{schema:?}:{sid}"
            );
        }
        store.execute_batch("DROP TABLE session_v2").unwrap();
        assert!(
            opencode_session_row(&store, "legacy-only", SessionV2).is_err(),
            "a missing active table must not select the surviving inactive one"
        );
    }

    #[cfg(unix)]
    #[test]
    fn execution_identity_canonicalizes_symlinks_only_on_the_host_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let alias = temp.path().join("alias");
        for path in ["cwd", "config", "store"] {
            std::fs::create_dir_all(real.join(path)).unwrap();
        }
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        let binding = |root: &std::path::Path| ExecutionBinding {
            agent: "claude".into(),
            stores: vec![root.join("store")],
            configuration: vec![ExecutionLocation {
                filesystem: "host".into(),
                path: root.join("config"),
            }],
            cwd: root.join("cwd"),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
            exported_default_store: None,
        };
        let host = binding(&real);
        let host_alias = binding(&alias);
        assert!(Instance::execution_identity_matches(&host, &host_alias));
        // Identity is the whole location, not its last component: two distinct
        // directories that happen to share a leaf name are different contexts.
        let twin = temp.path().join("twin");
        std::fs::create_dir_all(twin.join("store")).unwrap();
        assert!(
            !Instance::execution_identity_matches(&host, &binding(&twin)),
            "a same-named directory elsewhere is a different execution context"
        );

        let mut runtime_cwd = host.clone();
        runtime_cwd.cwd_filesystem = "runtime:docker:test".into();
        let mut runtime_cwd_alias = runtime_cwd.clone();
        runtime_cwd_alias.cwd = alias.join("cwd");
        assert!(!Instance::execution_identity_matches(
            &runtime_cwd,
            &runtime_cwd_alias
        ));

        let mut runtime_config = host.clone();
        runtime_config.configuration[0].filesystem = "runtime:docker:test".into();
        let mut runtime_config_alias = runtime_config.clone();
        runtime_config_alias.configuration[0].path = alias.join("config");
        assert!(!Instance::execution_identity_matches(
            &runtime_config,
            &runtime_config_alias
        ));

        let mut container_store = host.clone();
        container_store.filesystem = "container:session".into();
        let mut container_store_alias = container_store.clone();
        container_store_alias.stores[0] = alias.join("store");
        assert!(!Instance::execution_identity_matches(
            &container_store,
            &container_store_alias
        ));
    }

    #[test]
    fn legacy_binding_routing_marker_is_optional_and_not_identity() {
        use std::hash::{Hash, Hasher};

        let legacy: ExecutionBinding = serde_json::from_value(serde_json::json!({
            "agent": "claude",
            "stores": ["/tmp/claude"],
            "cwd": "/tmp",
            "cwd_filesystem": "host",
            "filesystem": "host"
        }))
        .unwrap();
        assert_eq!(legacy.exported_default_store, None);
        let mut exported = legacy.clone();
        exported.exported_default_store = Some(true);
        assert_eq!(legacy, exported);
        // The predicate handoff, validation and carry compare on is its own
        // thing: it ignores the routing marker, so a legacy row and its
        // attested counterpart stay the same execution.
        assert!(Instance::execution_identity_matches(&legacy, &exported));

        let digest = |binding: &ExecutionBinding| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            binding.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(digest(&legacy), digest(&exported));
    }

    /// #4127: a binding written before routing provenance existed adopts the
    /// route a launch attested, and nothing else about it moves.
    #[cfg(unix)]
    #[test]
    fn attest_observed_default_store_stamps_only_the_attested_route() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("claude");
        std::fs::create_dir_all(&store).unwrap();
        let observed = ExecutionBinding {
            agent: "claude".into(),
            stores: vec![store.clone()],
            configuration: Vec::new(),
            cwd: temp.path().to_path_buf(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
            exported_default_store: Some(true),
        };
        let legacy = |agent: &str, filesystem: &str, store: PathBuf| ConversationBinding {
            session_id: "sid-1".into(),
            execution: Some(ExecutionBinding {
                agent: agent.into(),
                stores: vec![store],
                configuration: Vec::new(),
                cwd: temp.path().to_path_buf(),
                cwd_filesystem: "host".into(),
                filesystem: filesystem.into(),
                exported_default_store: None,
            }),
            provenance: ConversationProvenance::Observed,
            transcript_path: Some(temp.path().join("transcript.jsonl")),
        };
        let marker = |binding: &ConversationBinding| {
            binding
                .execution
                .as_ref()
                .and_then(|execution| execution.exported_default_store)
        };
        let without_marker = |binding: &ConversationBinding| {
            let mut execution = binding.execution.clone().unwrap();
            execution.exported_default_store = None;
            (
                binding.session_id.clone(),
                binding.provenance.clone(),
                binding.transcript_path.clone(),
                execution,
            )
        };

        // Two spellings of one store are one store, so the marker is written.
        let mut binding = legacy("claude", "host", temp.path().join("claude/"));
        let before = binding.clone();
        assert!(attest_observed_default_store(&mut binding, Some(&observed)));
        assert_eq!(marker(&binding), Some(true));
        assert_eq!(without_marker(&binding), without_marker(&before));
        assert_eq!(binding, before, "the marker is not part of the identity");

        // Idempotent, and a marker already attested is never overwritten.
        assert!(!attest_observed_default_store(
            &mut binding,
            Some(&observed)
        ));
        let mut attested = binding.clone();
        attested.execution.as_mut().unwrap().exported_default_store = Some(false);
        let downgrade = ExecutionBinding {
            exported_default_store: Some(false),
            ..observed.clone()
        };
        assert!(!attest_observed_default_store(
            &mut attested,
            Some(&downgrade)
        ));
        assert_eq!(marker(&attested), Some(false));

        let other_store = temp.path().join("other");
        let refusals = [
            ("codex", "host", store.clone(), observed.clone()),
            (
                "claude",
                "container:session",
                store.clone(),
                observed.clone(),
            ),
            ("claude", "host", other_store, observed.clone()),
            (
                "claude",
                "host",
                store.clone(),
                ExecutionBinding {
                    stores: vec![store.clone(), temp.path().join("second")],
                    ..observed.clone()
                },
            ),
            (
                "claude",
                "host",
                store.clone(),
                ExecutionBinding {
                    exported_default_store: None,
                    ..observed.clone()
                },
            ),
        ];
        for (agent, filesystem, store, observed) in refusals {
            let mut binding = legacy(agent, filesystem, store);
            assert!(
                !attest_observed_default_store(&mut binding, Some(&observed)),
                "agent={agent} filesystem={filesystem} stores={:?}",
                observed.stores
            );
            assert_eq!(marker(&binding), None);
        }

        // A binding no observation qualified, and an observation nobody
        // reported, attest nothing.
        let mut unknown = legacy("claude", "host", store.clone());
        unknown.provenance = ConversationProvenance::Unknown;
        assert!(!attest_observed_default_store(
            &mut unknown,
            Some(&observed)
        ));
        let mut binding = legacy("claude", "host", store);
        assert!(!attest_observed_default_store(&mut binding, None));
        assert_eq!(marker(&binding), None);
    }

    /// Only a reserved host Claude id whose transcript is in its own store is
    /// confirmed; an id nothing reserved still needs explicit qualification.
    #[test]
    fn preallocated_claude_binding_is_confirmed_by_its_transcript() {
        let store = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path().to_string_lossy().to_string();
        let present = "11111111-2222-3333-4444-555555555555";
        let absent = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let project_dir = store.path().join("projects").join(
            crate::session::capture::encode_claude_project_path(
                &crate::session::capture::canonicalize_or_raw(&cwd).to_string_lossy(),
            ),
        );
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join(format!("{present}.jsonl")), "data\n").unwrap();

        use ConversationProvenance::{Preallocated, Unknown};
        let second = store.path().join("second");
        // (sid, agent, filesystem, provenance, extra store, confirmed)
        for (sid, agent, filesystem, provenance, extra, confirmed) in [
            (present, "claude", "host", Preallocated, None, true),
            (absent, "claude", "host", Preallocated, None, false),
            // A container store is not on this filesystem.
            (
                present,
                "claude",
                "container:session",
                Preallocated,
                None,
                false,
            ),
            (present, "codex", "host", Preallocated, None, false),
            (present, "claude", "host", Unknown, None, false),
            (
                present,
                "claude",
                "host",
                Preallocated,
                Some(&second),
                false,
            ),
        ] {
            let binding = ConversationBinding {
                session_id: sid.into(),
                execution: Some(ExecutionBinding {
                    agent: agent.into(),
                    stores: std::iter::once(store.path().to_path_buf())
                        .chain(extra.cloned())
                        .collect(),
                    configuration: Vec::new(),
                    cwd: cwd.clone().into(),
                    cwd_filesystem: "host".into(),
                    filesystem: filesystem.into(),
                    exported_default_store: None,
                }),
                provenance: provenance.clone(),
                transcript_path: None,
            };
            assert_eq!(
                transcript_confirms_reserved_claude_conversation(&binding),
                confirmed,
                "sid={sid} agent={agent} filesystem={filesystem} provenance={provenance:?} extra={extra:?}"
            );
        }
    }

    /// The transcript fallback confirms the reserved id only while the sidecar,
    /// fresh or stale, names no other conversation, and once it does the
    /// fallback stays off, so a `/clear` is never reverted.
    #[test]
    fn reserved_claude_confirmation_defers_to_any_other_published_id() {
        let root = tempfile::tempdir().unwrap();
        let reserved = "019342ab-1234-7def-8901-abcdef012345";
        let other = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let mut inst = super::super::test_helpers::reserved_claude_instance(root.path(), reserved);
        // The recorded routing marker is not part of execution identity, so only
        // scoping to the recorded copy keeps it.
        inst.agent_session_binding
            .as_mut()
            .and_then(|binding| binding.execution.as_mut())
            .unwrap()
            .exported_default_store = Some(true);
        let sidecar = |sid: &str| {
            Some(crate::session::poller::SessionIdObservation::instance_sidecar(sid.into(), None))
        };
        let confirmation = ReservedClaudeConfirmation::for_instance(&inst).unwrap();
        assert_eq!(
            confirmation.observe(None, || None),
            None,
            "no transcript yet"
        );
        super::super::test_helpers::write_reserved_claude_transcript(&inst, reserved);

        // (fresh sidecar, any-age sidecar, confirms the reserved id)
        for (fresh, published, confirms) in [
            (None, None, true),
            (None, Some(reserved), true),
            (None, Some(other), false),
        ] {
            let confirmation = ReservedClaudeConfirmation::for_instance(&inst).unwrap();
            let observed =
                confirmation.observe(fresh.and_then(sidecar), || published.and_then(sidecar));
            assert_eq!(
                observed
                    .as_ref()
                    .map(|observation| observation.sid.as_str()),
                confirms.then_some(reserved),
                "fresh={fresh:?} published={published:?}"
            );
            if confirms {
                let binding = observed.unwrap().conversation_binding().unwrap();
                assert!(binding.is_known());
                assert_eq!(
                    binding.execution,
                    inst.active_execution.as_ref().map(|a| a.binding.clone())
                );
                assert_eq!(
                    binding.execution.unwrap().exported_default_store,
                    Some(true)
                );
            }
        }

        let confirmation = ReservedClaudeConfirmation::for_instance(&inst).unwrap();
        assert_eq!(
            confirmation.observe(sidecar(other), || None),
            sidecar(other)
        );
        assert_eq!(
            confirmation.observe(None, || None),
            None,
            "latched after a /clear"
        );
    }

    fn hermes_fixture() -> (tempfile::TempDir, NativeLaunchInputs, rusqlite::Connection) {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let inputs = NativeLaunchInputs {
            launch_id: uuid::Uuid::new_v4().to_string(),
            environment: std::collections::HashMap::from([(
                "HOME".into(),
                cwd.display().to_string(),
            )]),
            raw_environment: Vec::new(),
            cwd,
            profile: "default".into(),
            container: None,
            docker_env: None,
            pane_env: Vec::new(),
            identity_extension: None,
        };
        let database = crate::session::instance::test_helpers::create_hermes_database(root.path());
        (root, inputs, database)
    }

    #[test]
    fn hermes_native_lineage_preserves_selection_and_conversation_boundaries() {
        for (name, requested, sql, expected) in [
            ("compression priority", "S", "INSERT INTO sessions(id,parent_session_id,end_reason,started_at) VALUES ('S',NULL,'compression',1),('A','S','compression',2),('B','S',NULL,4),('T','A',NULL,3),('foreign',NULL,NULL,100); INSERT INTO messages VALUES ('B',99),('T',3),('foreign',100);", Some("T")),
            ("message recency", "S", "INSERT INTO sessions(id,parent_session_id,end_reason,started_at,last_activity_at) VALUES ('S',NULL,'compression',1,NULL),('A','S',NULL,2,3),('B','S',NULL,4,5); INSERT INTO messages VALUES ('A',10);", Some("A")),
            ("compression tie", "S", "INSERT INTO sessions(id,parent_session_id,end_reason,started_at) VALUES ('S',NULL,'compression',1),('A','S',NULL,2),('B','S',NULL,2); INSERT INTO messages VALUES ('A',2),('B',2);", Some("B")),
            ("continuation tie", "S", "INSERT INTO sessions(id,parent_session_id,started_at) VALUES ('S',NULL,1),('A','S',2),('B','S',2); INSERT INTO messages VALUES ('A',2),('B',2);", Some("B")),
            ("excluded native children", "S", r#"INSERT INTO sessions(id,parent_session_id,started_at,model_config,source) VALUES ('S',NULL,0,NULL,NULL),('T','S',1,NULL,NULL),('branch','S',5,'{"_branched_from":false}',NULL),('delegate','S',6,'{"_delegate_from":false}',NULL),('reset','S',7,'{"_reset_from":false}',NULL),('tool','S',8,NULL,'tool'); INSERT INTO messages SELECT id,started_at FROM sessions;"#, Some("T")),
            ("empty newest path", "S", "INSERT INTO sessions(id,parent_session_id,started_at) VALUES ('S',NULL,1),('A','S',2),('B','S',3); INSERT INTO messages VALUES ('S',1),('A',2);", Some("S")),
            ("selected reset under compression", "S", r#"INSERT INTO sessions(id,parent_session_id,end_reason,started_at,model_config) VALUES ('S',NULL,'compression',1,NULL),('A','S',NULL,2,NULL),('T','S',NULL,3,'{"_reset_from":false}'); INSERT INTO messages VALUES ('A',2),('T',3);"#, None),
            ("historical branch equality", "S", "INSERT INTO sessions(id,parent_session_id,end_reason,ended_at,started_at) VALUES ('S',NULL,'branched',2,0),('A','S',NULL,NULL,1),('T','S',NULL,NULL,2); INSERT INTO messages VALUES ('A',1),('T',2);", None),
            ("historical branch NULL", "S", "INSERT INTO sessions(id,parent_session_id,end_reason,started_at) VALUES ('S',NULL,'branched',1),('T','S',NULL,2); INSERT INTO messages VALUES ('T',2);", Some("T")),
            ("reset different key", "S", "INSERT INTO sessions(id,parent_session_id,end_reason,started_at,session_key) VALUES ('S',NULL,'session_reset',1,'one'),('same','S',NULL,4,'one'),('T','S',NULL,3,'two'); INSERT INTO messages VALUES ('same',4),('T',3);", Some("T")),
            ("reset empty key", "S", "INSERT INTO sessions(id,parent_session_id,end_reason,started_at,session_key) VALUES ('S',NULL,'session_reset',1,''),('T','S',NULL,2,''); INSERT INTO messages VALUES ('T',2);", Some("T")),
            ("explicit boundary root", "T", r#"INSERT INTO sessions(id,parent_session_id,end_reason,ended_at,started_at,model_config) VALUES ('S',NULL,'branched',1,0,NULL),('T','S','compression',NULL,2,'{"_branched_from":false,"_reset_from":false}'),('U','T',NULL,NULL,3,NULL); INSERT INTO messages VALUES ('U',3);"#, Some("U")),
            ("nonfixed emitted target", "S", "INSERT INTO sessions(id,parent_session_id,end_reason,started_at) VALUES ('S',NULL,NULL,0),('A','S','compression',1),('B','A',NULL,2),('C','A',NULL,3); INSERT INTO messages VALUES ('A',1),('B',10);", None),
            ("malformed optional model JSON", "S", "INSERT INTO sessions(id,parent_session_id,started_at,model_config) VALUES ('S',NULL,1,NULL),('T','S',2,'{bad'); INSERT INTO messages VALUES ('T',2);", Some("T")),
        ] {
            let (root, inputs, database) = hermes_fixture();
            database.execute_batch(sql).unwrap();
            let result = inputs.resolve_hermes_target(root.path(), requested);
            match expected {
                Some(expected) => assert_eq!(result.unwrap_or_else(|error| panic!("{name}: {error:#}")), expected, "{name}"),
                None => assert!(result.is_err(), "{name}: {result:?}"),
            }
        }
    }

    #[test]
    fn hermes_native_lineage_rejects_cycles_and_unexamined_remainders() {
        for (compression, edges, accepted) in [
            (true, 100, true),
            (true, 101, false),
            (false, 31, true),
            (false, 32, false),
        ] {
            let (root, inputs, database) = hermes_fixture();
            for index in 0..=edges {
                database.execute("INSERT INTO sessions(id,parent_session_id,end_reason,started_at) VALUES (?,?,?,?)", rusqlite::params![format!("node_{index}"), (index > 0).then(|| format!("node_{}", index - 1)), (compression && index < edges).then_some("compression"), index]).unwrap();
            }
            let target = format!("node_{edges}");
            database
                .execute("INSERT INTO messages VALUES (?,1)", [&target])
                .unwrap();
            let result = inputs.resolve_hermes_target(root.path(), "node_0");
            if accepted {
                assert_eq!(result.unwrap(), target);
            } else {
                assert!(result.is_err());
            }
        }
        for reason in [None, Some("compression")] {
            let (root, inputs, database) = hermes_fixture();
            database.execute("INSERT INTO sessions(id,parent_session_id,end_reason) VALUES ('S','T',?),('T','S',?)", [reason,reason]).unwrap();
            assert!(inputs.resolve_hermes_target(root.path(), "S").is_err());
        }
    }

    #[test]
    fn hermes_stored_cwd_uses_both_native_interpretations() {
        for (node, value, accepted) in [
            ("C", " \n", true),
            ("T", " \n", false),
            ("C", "auto", false),
            ("C", "~", false),
            ("T", "~other", false),
            ("C", "{cwd}/missing", false),
            ("C", "\u{1c} {cwd}\r\n ", true),
            ("T", " {cwd} ", false),
            ("T", ".", true),
        ] {
            let (root, inputs, database) = hermes_fixture();
            database.execute_batch("INSERT INTO sessions(id,parent_session_id,end_reason) VALUES ('S',NULL,'compression'),('C','S',NULL),('T','C',NULL); INSERT INTO messages VALUES ('T',1);").unwrap();
            let value = value.replace("{cwd}", inputs.cwd.to_str().unwrap());
            database
                .execute("UPDATE sessions SET cwd=? WHERE id=?", [&value, node])
                .unwrap();
            let result = inputs.resolve_hermes_target(root.path(), "S");
            if accepted {
                assert_eq!(result.unwrap(), "T");
            } else {
                assert!(result.is_err(), "{node}: {value:?}");
            }
        }
        let (root, inputs, database) = hermes_fixture();
        std::os::unix::fs::symlink(&inputs.cwd, inputs.cwd.join("~")).unwrap();
        database.execute_batch("INSERT INTO sessions(id,cwd) VALUES ('T','~'); INSERT INTO messages VALUES ('T',1);").unwrap();
        assert_eq!(inputs.resolve_hermes_target(root.path(), "T").unwrap(), "T");
    }

    #[test]
    fn hermes_resume_requires_exact_unique_ids_and_readable_schema() {
        let (root, inputs, database) = hermes_fixture();
        database.execute_batch("INSERT INTO sessions(id,parent_session_id,end_reason) VALUES ('S',NULL,'compression'),('latest','S',NULL); INSERT INTO messages VALUES ('latest',1);").unwrap();
        assert!(inputs.resolve_hermes_target(root.path(), "latest").is_err());
        assert!(inputs.resolve_hermes_target(root.path(), "S").is_err());
        assert!(inputs
            .resolve_hermes_target(root.path(), "missing-title")
            .is_err());
        database.execute_batch("UPDATE sessions SET id='T' WHERE id='latest'; UPDATE messages SET session_id='T'; CREATE TABLE copied AS SELECT * FROM sessions; DROP TABLE sessions; ALTER TABLE copied RENAME TO sessions; INSERT INTO sessions SELECT * FROM sessions WHERE id='T';").unwrap();
        assert!(inputs.resolve_hermes_target(root.path(), "S").is_err());
        database.execute_batch("DELETE FROM sessions WHERE rowid=(SELECT MAX(rowid) FROM sessions); ALTER TABLE sessions DROP COLUMN cwd;").unwrap();
        assert!(inputs.resolve_hermes_target(root.path(), "S").is_err());
    }

    #[test]
    fn hermes_resume_reads_live_wal_and_refuses_redirected_sidecars() {
        let (root, inputs, database) = hermes_fixture();
        database.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO sessions(id,parent_session_id,end_reason) VALUES ('S',NULL,'compression'),('T','S',NULL); INSERT INTO messages VALUES ('T',1);").unwrap();
        assert_eq!(inputs.resolve_hermes_target(root.path(), "S").unwrap(), "T");
        let foreign = root.path().join("foreign");
        std::fs::write(&foreign, b"").unwrap();
        std::os::unix::fs::symlink(&foreign, root.path().join("state.db-journal")).unwrap();
        assert!(inputs.resolve_hermes_target(root.path(), "S").is_err());
    }

    #[test]
    fn missing_store_keeps_physical_identity_after_creation() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let inputs = NativeLaunchInputs {
            launch_id: uuid::Uuid::new_v4().to_string(),
            environment: Default::default(),
            raw_environment: Vec::new(),
            cwd: root.path().to_path_buf(),
            profile: "default".into(),
            container: None,
            docker_env: None,
            pane_env: Vec::new(),
            identity_extension: None,
        };
        let database = alias.join("new/opencode.db");
        let before = inputs.canonical_path(&database).unwrap();
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        std::fs::write(&database, b"").unwrap();
        assert_eq!(before, inputs.canonical_path(&database).unwrap());
        let cycle = root.path().join("cycle");
        std::os::unix::fs::symlink(&cycle, &cycle).unwrap();
        assert!(inputs.canonical_path(&cycle).is_err());
    }

    #[test]
    fn managed_verbose_flag_is_allowed_without_namespace_effect() {
        for name in ["claude", "codex", "opencode", "omp", "prime-agent", "kimi"] {
            let agent = crate::agents::get_agent(name).unwrap();
            assert!(
                validate_managed_arguments(agent, &[String::from("--verbose")]).is_ok(),
                "{name} must accept a bare verbosity flag"
            );
            assert!(
                validate_managed_arguments(agent, &[String::from("--verbose=x")]).is_err(),
                "{name} must refuse a valued verbosity override"
            );
        }
    }

    /// A recorded id must reach `terminal_fork_seed` whatever evidence stands
    /// behind it: a binding a migration left unattributed forks on the row's
    /// own agent, while a binding a degraded launch dropped is still refused as
    /// unqualified, because nothing proves what it names.
    #[test]
    #[serial_test::serial]
    fn fork_parent_ref_keeps_a_recorded_but_unqualified_conversation() {
        let root = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(root.path());
        let _claude = crate::session::test_support::install_login_shell_path_command(
            root.path(),
            "claude",
            "#!/bin/sh\nexit 0\n",
        );
        let mut instance = Instance::new("parent", root.path().to_str().unwrap());
        instance.tool = "claude".into();
        instance.agent_session_id = Some("legacy-uuid".into());

        instance.agent_session_binding = Some(ConversationBinding::unknown("legacy-uuid"));
        assert!(matches!(
            crate::session::fork::terminal_fork_seed(
                instance.fork_parent_ref().unwrap(),
                "child-uuid".into()
            ),
            Ok(crate::session::ForkSeed::Terminal { .. })
        ));

        instance.agent_session_binding = None;
        assert_eq!(
            crate::session::fork::terminal_fork_seed(
                instance.fork_parent_ref().unwrap(),
                "child-uuid".into()
            ),
            Err(crate::session::ForkDenied::UnqualifiedParent {
                preallocated: false,
                recorded: "legacy-uuid".into(),
            })
        );
    }

    /// A row whose own fork intent has not launched holds the parent's
    /// conversation, not one of its own, so it is refused as the fork it is.
    #[test]
    fn fork_parent_ref_reports_a_child_whose_fork_has_not_launched() {
        let mut instance = Instance::new("child", "/tmp");
        instance.agent_session_id = Some("parent-uuid".into());
        instance.agent_session_binding = Some(ConversationBinding {
            session_id: "parent-uuid".into(),
            execution: Some(ExecutionBinding {
                agent: "claude".into(),
                stores: vec!["/store".into()],
                configuration: Vec::new(),
                cwd: "/work".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });
        instance.resume_intent = ResumeIntent::Fork {
            from: "parent-uuid".into(),
        };

        assert_eq!(
            crate::session::fork::terminal_fork_seed(
                instance.fork_parent_ref().unwrap(),
                "child-uuid".into()
            ),
            Err(crate::session::ForkDenied::UnlaunchedFork)
        );
    }

    /// The exported `CLAUDE_CONFIG_DIR` and the store recorded on the binding
    /// must name one identity: a symlinked declaration would otherwise route
    /// the agent through the alias while persisting the resolved path.
    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn claude_store_route_and_binding_share_one_canonical_identity() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _config_dir = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let _claude = crate::session::test_support::install_login_shell_path_command(
            temp.path(),
            "claude",
            "#!/bin/sh\nexit 0\n",
        );
        let real = temp.path().join("real-store");
        let alias = temp.path().join("alias-store");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let profile = "claude-store-route-identity";
        let path =
            crate::session::config::profile_config::get_profile_config_path(profile).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "[session.agent_config_dir]\nclaude = {:?}\n",
                alias.to_str().unwrap()
            ),
        )
        .unwrap();
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);
        let mut inst = Instance::new("claude-route-identity", temp.path().to_str().unwrap());
        inst.source_profile = profile.into();
        inst.tool = "claude".into();
        inst.view = crate::session::View::Structured;

        let native = inst.resolve_native_execution(None).unwrap();

        let resolved = real.canonicalize().unwrap();
        assert_eq!(native.binding.stores, vec![resolved.clone()]);
        let routed = native
            .routing
            .iter()
            .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
            .map(|(_, value)| value.as_deref());
        assert_eq!(routed, Some(resolved.to_str()));
    }
}
