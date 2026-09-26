//! Retain ambiguous native stores, then publish positively seeded private stores.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{progress, v027_isolate_sandbox_stores as layout};
use crate::session::config::container_config;
use crate::session::AnchoredDir;

const RECEIPTS: &str = "sandbox-content-receipts";
pub(crate) const RECOVERY: &str = ".aoe-sandbox-recovery";
pub(crate) const CONTENT_POLICY: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContentRoot {
    pub(crate) path: PathBuf,
    pub(crate) host: PathBuf,
    pub(crate) roles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResetLane {
    pending: bool,
    generation: Option<u64>,
}

#[derive(Clone, Copy)]
pub(crate) enum NativeContextView {
    Terminal,
    Structured,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SandboxContentReset {
    slot: String,
    pub(crate) transaction: String,
    pub(crate) tool: String,
    pub(crate) agent: String,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) recovery: Vec<PathBuf>,
    terminal: ResetLane,
    structured: ResetLane,
    /// Pre-retirement candidates; a later isolated context is never cleared
    /// just because its first notice has not yet been acknowledged.
    retired_terminal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retired_terminal_binding: Option<crate::session::ConversationBinding>,
    retired_structured: Vec<String>,
    retired_import: bool,
}

impl SandboxContentReset {
    fn lane(&mut self, view: NativeContextView) -> &mut ResetLane {
        match view {
            NativeContextView::Terminal => &mut self.terminal,
            NativeContextView::Structured => &mut self.structured,
        }
    }
}

fn retired_binding_matches(
    current: Option<&crate::session::ConversationBinding>,
    retired: Option<&crate::session::ConversationBinding>,
    sid: &str,
) -> bool {
    match retired {
        Some(retired) => current == Some(retired),
        None => current.is_none_or(|binding| {
            binding.session_id == sid
                && binding.execution.is_none()
                && binding.provenance == crate::session::ConversationProvenance::Unknown
                && binding.transcript_path.is_none()
        }),
    }
}

fn claim_context_reset(
    instance: &mut crate::session::Instance,
    agent: Option<&str>,
    view: NativeContextView,
    generation: u64,
) -> Option<(String, Vec<String>)> {
    let mut slots = Vec::new();
    let mut recovery = BTreeSet::new();
    for reset in &mut instance.sandbox_content_resets {
        if reset.tool != instance.tool
            || agent.is_some_and(|agent| reset.agent != agent)
            || !reset.lane(view).pending
        {
            continue;
        }
        if reset.lane(view).generation.is_none() {
            match view {
                NativeContextView::Terminal => {
                    if let Some(retired) = reset.retired_terminal.as_deref() {
                        if instance.agent_session_id.as_deref() == Some(retired)
                            && retired_binding_matches(
                                instance.agent_session_binding.as_ref(),
                                reset.retired_terminal_binding.as_ref(),
                                retired,
                            )
                        {
                            instance.agent_session_id = None;
                            instance.agent_session_binding = None;
                            instance.pi_session_path = None;
                        }
                    }
                    if instance.agent_session_id.is_none()
                        && instance.pi_session_path.is_none()
                        && matches!(
                            instance.resume_intent,
                            crate::session::ResumeIntent::Default
                                | crate::session::ResumeIntent::Cleared
                        )
                    {
                        instance.resume_intent = crate::session::ResumeIntent::Cleared;
                        instance.resume_binding = None;
                        instance.capture_started_at = Some(std::time::SystemTime::now());
                    }
                }
                NativeContextView::Structured => {
                    let newer = instance
                        .acp_session_id
                        .as_ref()
                        .is_some_and(|id| !reset.retired_structured.contains(id))
                        || instance
                            .fork_pending
                            .as_ref()
                            .is_some_and(|id| !reset.retired_structured.contains(id));
                    if !newer {
                        instance.acp_session_id = None;
                        instance.fork_pending = None;
                        if reset.retired_import {
                            instance.import_pending = None;
                        }
                    }
                }
            }
        }
        reset.lane(view).generation = Some(generation);
        slots.push(reset.slot.clone());
        recovery.extend(reset.recovery.iter().map(|path| path.display().to_string()));
    }
    if slots.is_empty() {
        return None;
    }
    let continuing = match view {
        NativeContextView::Terminal => {
            instance.agent_session_id.is_some() || instance.pi_session_path.is_some()
        }
        NativeContextView::Structured => {
            instance.acp_session_id.is_some() || instance.fork_pending.is_some()
        }
    };
    let context = if continuing {
        "continues its own isolated"
    } else {
        "starts a fresh"
    };
    let labelled = agent.unwrap_or(instance.tool.as_str());
    Some((format!("Sandbox native history was isolated; this launch {context} {labelled} conversation. The existing AoE transcript is retained. Complete originals: {}", recovery.into_iter().collect::<Vec<_>>().join(", ")), slots))
}

pub(crate) struct AcpLaunchContext {
    pub(crate) profile: String,
    pub(crate) stored_session_id: Option<String>,
    pub(crate) fork_from: Option<String>,
    pub(crate) seed_history_replay: bool,
    pub(crate) notice: Option<(String, Vec<String>)>,
}

#[derive(Clone, Copy)]
pub(crate) enum AcpContextUse {
    Launch,
    Attach,
}

/// Read continuation after sandbox admission, not from a caller's earlier
/// snapshot. The resolved ACP adapter owns this lane, not the row's TUI tool.
pub(crate) fn prepare_acp_context(
    profile: &str,
    id: &str,
    agent: Option<&str>,
    generation: u64,
    usage: AcpContextUse,
    continuation: crate::acp::supervisor::SandboxContinuation,
) -> Result<AcpLaunchContext> {
    let storage = crate::session::Storage::new_unwatched(profile)?;
    storage.update(|instances, _| {
        let instance = instances
            .iter_mut()
            .find(|instance| instance.id == id)
            .context("sandbox session disappeared before structured launch")?;
        if !instance.is_sandboxed() || !instance_ready(instance)? {
            bail!("sandbox native content is not ready for structured launch");
        }
        // An adapter that names no native agent cannot prove which lane it is
        // about to continue, so it is answered for the whole row: refuse the
        // attach while any structured lane is pending, and claim them all.
        let pending = instance.sandbox_content_resets.iter().any(|reset| {
            reset.tool == instance.tool
                && agent.is_none_or(|agent| reset.agent == agent)
                && reset.structured.pending
                && reset.structured.generation.is_none()
        });
        let carried = || {
            instance
                .sandbox_content_resets
                .iter()
                .filter(|reset| reset.tool == instance.tool && !reset.structured.pending)
        };
        let foreign_adapter = carried().next().is_some()
            && !carried().any(|reset| agent == Some(reset.agent.as_str()));
        let unproven_old_id = foreign_adapter
            && carried().any(|reset| {
                instance
                    .acp_session_id
                    .as_ref()
                    .into_iter()
                    .chain(instance.fork_pending.as_ref())
                    .any(|id| reset.retired_structured.contains(id))
                    || (reset.retired_import && instance.import_pending == Some(true))
            });
        if matches!(usage, AcpContextUse::Attach) && (pending || unproven_old_id) {
            bail!("runner predates its sandbox content reset; a fresh launch is required");
        }
        let notice =
            claim_context_reset(instance, agent, NativeContextView::Structured, generation);
        let (stored_session_id, fork_from, seed_history_replay) = match continuation {
            crate::acp::supervisor::SandboxContinuation::Persisted if unproven_old_id => {
                (None, None, false)
            }
            crate::acp::supervisor::SandboxContinuation::Persisted => (
                instance.acp_session_id.clone(),
                instance.fork_pending.clone(),
                instance.import_pending == Some(true),
            ),
            crate::acp::supervisor::SandboxContinuation::ImportTerminal if notice.is_none() => {
                let id = instance
                    .agent_session_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_owned);
                let replay = id.is_some();
                (id, None, replay)
            }
            crate::acp::supervisor::SandboxContinuation::ImportTerminal
            | crate::acp::supervisor::SandboxContinuation::Fresh => (None, None, false),
        };
        Ok(AcpLaunchContext {
            profile: storage.profile().to_owned(),
            stored_session_id,
            fork_from,
            seed_history_replay,
            notice,
        })
    })
}

pub(crate) fn prepare_terminal_launch_context(
    instance: &mut crate::session::Instance,
    agent: &str,
) -> Result<Option<(String, Vec<String>)>> {
    if !instance.is_sandboxed()
        || !instance.sandbox_content_resets.iter().any(|reset| {
            reset.tool == instance.tool && reset.agent == agent && reset.terminal.pending
        })
    {
        return Ok(None);
    }
    let generation = instance.lifecycle_generation;
    let storage = crate::session::Storage::new_unwatched(&instance.source_profile)?;
    let (resets, notice, sid, sid_binding, pi_path, intent, resume_binding, floor, omp_generation) =
        storage.update(|instances, _| {
            let row = instances
                .iter_mut()
                .find(|row| row.id == instance.id)
                .context("sandbox session disappeared before terminal launch")?;
            if row.lifecycle_generation != generation || row.tool != instance.tool {
                bail!("terminal content reset lost its launch scope");
            }
            let notice =
                claim_context_reset(row, Some(agent), NativeContextView::Terminal, generation);
            Ok((
                row.sandbox_content_resets.clone(),
                notice,
                row.agent_session_id.clone(),
                row.agent_session_binding.clone(),
                row.pi_session_path.clone(),
                row.resume_intent.clone(),
                row.resume_binding.clone(),
                row.capture_started_at,
                row.omp_capture_generation.clone(),
            ))
        })?;
    instance.sandbox_content_resets = resets;
    if notice.is_some() {
        instance.agent_session_id = sid;
        instance.agent_session_binding = sid_binding;
        instance.pi_session_path = pi_path;
        instance.resume_intent = intent;
        instance.resume_binding = resume_binding;
        instance.capture_started_at = floor;
        instance.omp_capture_generation = omp_generation;
    }
    Ok(notice)
}

pub(crate) fn acknowledge_context_reset(
    profile: &str,
    id: &str,
    view: NativeContextView,
    generation: u64,
    slots: &[String],
    assigned_id: Option<&str>,
) -> Result<()> {
    if slots.is_empty() {
        return Ok(());
    }
    crate::session::Storage::new_unwatched(profile)?.update(|instances, _| {
        let instance = instances
            .iter_mut()
            .find(|instance| instance.id == id)
            .context("sandbox session disappeared before context-reset acknowledgment")?;
        if matches!(view, NativeContextView::Terminal)
            && instance.lifecycle_generation != generation
        {
            bail!("sandbox context reset lost its terminal generation");
        }
        for slot in slots {
            let reset = instance
                .sandbox_content_resets
                .iter_mut()
                .find(|reset| &reset.slot == slot)
                .context("sandbox context-reset slot disappeared")?;
            if reset.tool != instance.tool {
                bail!("sandbox context reset lost its literal tool");
            }
            let lane = reset.lane(view);
            if lane.generation != Some(generation) {
                bail!("sandbox context reset lost its launch generation");
            }
            lane.pending = false;
        }
        if matches!(view, NativeContextView::Structured) {
            if let Some(sid) = assigned_id {
                instance.acp_session_id = Some(sid.to_owned());
            }
        }
        Ok(())
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RootTransition {
    root: ContentRoot,
    stage: PathBuf,
    recovery: PathBuf,
    original: Option<Identity>,
    staged: Option<Identity>,
    published: Option<Identity>,
    #[serde(default)]
    carried_tools: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Planned,
    Staged,
    Published,
    Committed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    policy: u8,
    instance: String,
    tool: String,
    transaction: String,
    roots: Vec<RootTransition>,
    phase: Phase,
    retired_identity: Value,
    retired_tools: BTreeMap<String, String>,
}

pub(crate) struct ResumeCandidate {
    pub(crate) tool: String,
    pub(crate) agent: String,
    pub(crate) id: String,
}

/// Only a stopped, journalled migration constructs a stage-seeding capability.
pub(crate) struct ContentSeed<'a> {
    source: &'a Path,
    stopped_original: bool,
    resumes: &'a [ResumeCandidate],
    container_workdir: &'a str,
}

impl ContentSeed<'_> {
    pub(crate) fn path(&self) -> &Path {
        self.source
    }
    pub(crate) fn is_stopped_original(&self) -> bool {
        self.stopped_original
    }
    pub(crate) fn resumes(&self) -> &[ResumeCandidate] {
        self.resumes
    }
    pub(crate) fn container_workdir(&self) -> &str {
        self.container_workdir
    }
}

pub(crate) fn canonical_expected_path(path: &Path) -> Result<PathBuf> {
    let normalized = crate::git::template::lexical_normalize(path);
    if !normalized.is_absolute() {
        bail!("sandbox content path must be absolute");
    }
    let mut existing = normalized.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for leaf in suffix.into_iter().rev() {
                    canonical.push(leaf);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .context("content path has no existing ancestor")?,
                );
                existing = existing.parent().context("content path has no parent")?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("resolving {}", path.display()))
            }
        }
    }
}

fn identity(path: &Path) -> Result<Option<Identity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "sandbox content root is not a plain directory: {}",
                path.display()
            )
        }
        Ok(_) => {
            let anchor = AnchoredDir::open(path)?;
            let (device, inode) = anchor.identity()?;
            // Darwin dev_t is signed; MetadataExt and persisted identities use u64.
            #[cfg(target_os = "macos")]
            let device = device as u64;
            Ok(Some(Identity { device, inode }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn receipt_key(value: &impl Serialize) -> Result<String> {
    use std::fmt::Write;
    let digest = Sha256::digest(serde_json::to_vec(value)?);
    let mut key = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(key, "{byte:02x}");
    }
    Ok(key)
}

fn receipt_path(app: &Path, instance: &str, tool: &str) -> Result<PathBuf> {
    Ok(app
        .join(RECEIPTS)
        .join(instance)
        .join(format!("{}.json", receipt_key(&tool)?)))
}

fn archive_receipt(path: &Path, receipt: &Receipt) -> Result<()> {
    write_receipt(
        &path.with_extension(format!("{}.complete", receipt.transaction)),
        receipt,
    )?;
    match fs::remove_file(path) {
        Ok(()) => fs::File::open(path.parent().context("receipt has no parent")?)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn pending_receipt(app: &Path, instance: &str, tool: &str) -> Result<bool> {
    match fs::symlink_metadata(receipt_path(app, instance, tool)?) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn guard_other_transactions(
    app: &Path,
    instance: &str,
    tool: &str,
    roots: &[ContentRoot],
) -> Result<()> {
    let own = receipt_path(app, instance, tool)?;
    let entries = match fs::read_dir(own.parent().context("receipt has no parent")?) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path == own || path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let receipt =
            read_receipt(&path)?.context("content journal disappeared under transition lock")?;
        if receipt.instance != instance {
            bail!("content journal belongs to another instance");
        }
        if receipt
            .roots
            .iter()
            .any(|part| roots.iter().any(|root| root.path == part.root.path))
        {
            bail!("sandbox {instance} has an unfinished {} content transition; restore that tool's original configuration and finish aoe migrate before sharing its roots", receipt.tool);
        }
    }
    Ok(())
}

fn read_receipt(path: &Path) -> Result<Option<Receipt>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    Ok(Some(
        serde_json::from_slice(&bytes).context("reading sandbox content receipt")?,
    ))
}

fn receipt_directory(path: &Path) -> Result<AnchoredDir> {
    let parent = path.parent().context("content receipt has no parent")?;
    let Some(receipts) = parent
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == RECEIPTS))
    else {
        // A receipt outside the namespace (the legacy recovery journal) owns
        // its durability: its caller syncs each level it creates.
        return AnchoredDir::open(parent);
    };
    let app = receipts
        .parent()
        .context("receipt namespace has no parent")?;
    AnchoredDir::open(app)?.create_child(parent.strip_prefix(app)?)
}

fn write_receipt(path: &Path, receipt: &impl Serialize) -> Result<()> {
    let anchor = receipt_directory(path)?;
    let bytes = serde_json::to_vec_pretty(receipt)?;
    use std::os::unix::fs::PermissionsExt;
    anchor.publish_file(
        Path::new(path.file_name().context("receipt has no leaf")?),
        &mut bytes.as_slice(),
        fs::Permissions::from_mode(0o600),
        true,
        None,
    )?;
    Ok(())
}

fn receipt_matches(receipt: &Receipt, instance: &str, tool: &str, roots: &[ContentRoot]) -> bool {
    receipt.policy == CONTENT_POLICY
        && receipt.instance == instance
        && receipt.tool == tool
        && receipt.roots.len() == roots.len()
        && receipt
            .roots
            .iter()
            .zip(roots)
            .all(|(part, root)| part.root == *root)
}

fn roots_are_current(receipt: &Receipt) -> Result<bool> {
    if receipt.phase != Phase::Committed {
        return Ok(false);
    }
    for part in &receipt.roots {
        if part.published.is_none() || identity(&part.root.path)? != part.published {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Serialize, Deserialize)]
struct RootCertificate {
    policy: u8,
    instance: String,
    root: ContentRoot,
    identity: Identity,
    transaction: String,
}

fn certificate_path(app: &Path, instance: &str, root: &Path) -> Result<PathBuf> {
    let key = receipt_key(&(instance, root))?;
    Ok(app.join(RECEIPTS).join(format!("root-{key}.json")))
}

fn owned_root(app: &Path, instance: &str, path: &Path) -> Result<Option<RootCertificate>> {
    let bytes = match fs::read(certificate_path(app, instance, path)?) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let certificate: RootCertificate = serde_json::from_slice(&bytes)?;
    if certificate.policy != CONTENT_POLICY
        || certificate.instance != instance
        || certificate.root.path != path
        || identity(path)?.as_ref() != Some(&certificate.identity)
    {
        return Ok(None);
    }
    Ok(Some(certificate))
}

/// Prove ownership of the pinned directory, independently of role readiness.
pub(crate) fn owns_content_root(app: &Path, instance: &str, root: &AnchoredDir) -> Result<bool> {
    let path = canonical_expected_path(root.path())?;
    let Some(certificate) = owned_root(app, instance, &path)? else {
        return Ok(false);
    };
    let (device, inode) = root.identity()?;
    #[cfg(target_os = "macos")]
    let device = device as u64;
    Ok(certificate.identity == Identity { device, inode })
}

pub(crate) fn has_pending_content(app: &Path, instance: &str) -> Result<bool> {
    let entries = match fs::read_dir(app.join(RECEIPTS).join(instance)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        if Path::new(&entry?.file_name())
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Revoke durable authority before deleting its directory: inode reuse must
/// not make a later unrelated root look owned. A failed deletion stays retained.
pub(crate) fn revoke_content_root(app: &Path, instance: &str, root: &AnchoredDir) -> Result<bool> {
    if !owns_content_root(app, instance, root)? {
        return Ok(false);
    }
    let path = certificate_path(app, instance, &canonical_expected_path(root.path())?)?;
    fs::remove_file(&path)?;
    fs::File::open(path.parent().context("certificate has no parent")?)?.sync_all()?;
    Ok(true)
}

#[cfg(test)]
pub(crate) fn certify_owned_test_root(app: &Path, instance: &str, path: &Path) -> Result<()> {
    certify_test_content(app, instance, path, &[])
}
#[cfg(test)]
pub(crate) fn certify_test_content(
    app: &Path,
    instance: &str,
    path: &Path,
    roles: &[&str],
) -> Result<()> {
    let path = canonical_expected_path(path)?;
    let certificate = RootCertificate {
        policy: CONTENT_POLICY,
        instance: instance.to_owned(),
        root: ContentRoot {
            path: path.clone(),
            host: PathBuf::new(),
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
        },
        identity: identity(&path)?.context("test fixture has no directory")?,
        transaction: uuid::Uuid::new_v4().to_string(),
    };
    write_receipt(&certificate_path(app, instance, &path)?, &certificate)
}

fn root_ready(app: &Path, instance: &str, root: &ContentRoot) -> Result<bool> {
    Ok(
        owned_root(app, instance, &root.path)?.is_some_and(|certificate| {
            root.roles
                .iter()
                .all(|role| certificate.root.roles.contains(role))
        }),
    )
}

pub(crate) fn roots_ready(
    app: &Path,
    instance: &str,
    tool: &str,
    roots: &[ContentRoot],
) -> Result<bool> {
    if pending_receipt(app, instance, tool)? {
        return Ok(false);
    }
    for root in roots {
        if !root_ready(app, instance, root)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn certify_receipt(app: &Path, receipt: &Receipt) -> Result<()> {
    if !roots_are_current(receipt)? {
        bail!("content transaction is not committed to its physical roots");
    }
    for part in &receipt.roots {
        let mut root = part.root.clone();
        if let Some(previous) = owned_root(app, &receipt.instance, &root.path)? {
            root.roles.extend(previous.root.roles);
            root.roles.sort();
            root.roles.dedup();
        }
        write_receipt(
            &certificate_path(app, &receipt.instance, &root.path)?,
            &RootCertificate {
                policy: CONTENT_POLICY,
                instance: receipt.instance.clone(),
                root,
                identity: part
                    .published
                    .clone()
                    .context("committed root has no identity")?,
                transaction: receipt.transaction.clone(),
            },
        )?;
    }
    Ok(())
}

fn rename_directory(source: &Path, destination: &Path) -> Result<()> {
    // Walk from a resolved spelling: an ancestor that is a symlink belongs to
    // the host (macOS `/tmp`, a developer's symlinked temporary root), is not a
    // component this process owns, and refusing it would fail every move below
    // it. The leaf keeps its own check when the directory is published.
    let source = canonical_expected_path(source)?;
    let destination = canonical_expected_path(destination)?;
    let filesystem = AnchoredDir::open(Path::new("/"))?;
    if !filesystem.publish_directory(
        &filesystem,
        source
            .strip_prefix("/")
            .context("source must be absolute")?,
        destination
            .strip_prefix("/")
            .context("destination must be absolute")?,
    )? {
        bail!(
            "refusing to replace existing recovery/publication {}",
            destination.display()
        );
    }
    Ok(())
}

fn recovery_root(host: &Path) -> Result<PathBuf> {
    Ok(host
        .parent()
        .context("native config root has no parent")?
        .join(RECOVERY))
}

fn private_recovery_root(host: &Path) -> Result<AnchoredDir> {
    let recovery = recovery_root(host)?;
    let parent = AnchoredDir::open(recovery.parent().context("recovery has no parent")?)?;
    let root = parent.create_child(Path::new(RECOVERY))?;
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(root.path())?;
    let (device, inode) = root.identity()?;
    #[cfg(target_os = "macos")]
    let device = device as u64;
    if metadata.dev() != device
        || metadata.ino() != inode
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        bail!(
            "sandbox recovery directory is not a private owned directory: {}",
            root.path().display()
        );
    }
    parent.sync()?;
    Ok(root)
}

/// What a retention request did, so a caller that can leave work pending tells
/// "there was nothing to retain" from "a live mount keeps the namespace open".
pub(crate) enum Retained {
    Original(PathBuf),
    Absent,
    Deferred,
}

/// Retain a v027 original without its lossy copy/overlay policy. The caller
/// holds the stopped cohort lock; the entire tree moves intact,
/// including unclassified entries and links that were never copy candidates.
pub(crate) fn retain_legacy_original(source: &Path, host: &Path) -> Result<Retained> {
    let retained = retain_legacy_original_with(source, host, &live_exposure)?;
    if let Retained::Original(kept) = &retained {
        progress::notice(format!(
            "Retained complete legacy sandbox original at {}",
            kept.display()
        ));
    }
    Ok(retained)
}

/// The live mounts a sandbox holds, or the sources a test asked to pretend it
/// holds. Tests drive this seam rather than a container runtime, so the suite
/// behaves the same on a host that has no runtime installed, which is also why
/// an unset hook means "nothing is mounted".
fn live_exposure(id: &str) -> Result<Vec<PathBuf>> {
    #[cfg(test)]
    {
        let _ = id;
        Ok(EXPOSED_SOURCES
            .with(|hook| hook.borrow().clone())
            .unwrap_or_default())
    }
    #[cfg(not(test))]
    {
        live_bind_sources(id)
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static EXPOSED_SOURCES: std::cell::RefCell<Option<Vec<PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

fn retain_legacy_original_with(
    source: &Path,
    host: &Path,
    exposure: &ExposureProbe<'_>,
) -> Result<Retained> {
    let Some(original) = identity(source)? else {
        return Ok(Retained::Absent);
    };
    let app = crate::session::get_app_dir()?;
    let recovery = recovery_root(host)?;
    if let Some(message) =
        recovery_exposure(&app, &[canonical_expected_path(&recovery)?], exposure)?
    {
        progress::notice(format!(
            "{message}; the shared store stays where it is until that mount is gone"
        ));
        return Ok(Retained::Deferred);
    }
    let root = private_recovery_root(host)?;
    let transaction = root.create_child(Path::new(&format!("v027-{}", uuid::Uuid::new_v4())))?;
    let destination = transaction.path().join("original");
    write_receipt(
        &transaction.path().join("receipt.json"),
        &serde_json::json!({
            "source": source, "original": original, "recovery": destination,
        }),
    )?;
    transaction.sync()?;
    root.sync()?;
    if identity(source)?.as_ref() != Some(&original) {
        bail!("legacy original changed before retention");
    }
    rename_directory(source, &destination)?;
    Ok(Retained::Original(destination))
}

fn read_registries(app: &Path) -> Result<Vec<(PathBuf, Value)>> {
    let mut registries = Vec::new();
    for path in layout::registry_paths(app)? {
        match serde_json::from_slice(&fs::read(&path)?) {
            Ok(value) => registries.push((path, value)),
            // A registry AoE cannot parse is a pre-existing anomaly it cannot
            // reason about; skipping it with a warning keeps one corrupt file
            // from bricking every launch and migration, matching the reuse path
            // that already tolerates a failed reload.
            Err(error) => tracing::warn!(
                target: "session.store",
                registry = %path.display(),
                error = %error,
                "skipping unparseable session registry",
            ),
        }
    }
    Ok(registries)
}

fn lock_registries(app: &Path) -> Result<Vec<crate::session::StorageFlock>> {
    let directories: BTreeSet<PathBuf> = layout::registry_paths(app)?
        .into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect();
    layout::lock_registry_dirs(&directories.into_iter().collect::<Vec<_>>())
}

fn write_registry(path: &Path, value: &Value) -> Result<()> {
    crate::session::atomic_write(path, &serde_json::to_vec_pretty(value)?)?;
    fs::File::open(path.parent().context("registry has no parent")?)?.sync_all()?;
    Ok(())
}

fn read_row(path: &Path, id: &str) -> Result<Option<Value>> {
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(value
        .as_array()
        .context("session registry must be an array")?
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
        .cloned())
}

fn row_tools(row: &Value) -> BTreeSet<String> {
    let mut tools = BTreeSet::new();
    if let Some(tool) = row.get("tool").and_then(Value::as_str) {
        tools.insert(tool.to_owned());
    }
    if let Some(prior) = row.get("prior_tool_session_ids").and_then(Value::as_object) {
        tools.extend(prior.keys().cloned());
    }
    if let Some(resets) = row.get("sandbox_content_resets").and_then(Value::as_array) {
        tools.extend(
            resets
                .iter()
                .filter_map(|reset| reset.get("tool").and_then(Value::as_str))
                .map(str::to_owned),
        );
    }
    tools
}

fn row_roots(
    row: &Value,
    tool: &str,
    home: &Path,
    config: &crate::session::Config,
) -> Result<Vec<ContentRoot>> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .context("sandbox row has no id")?;
    let current = row.get("tool").and_then(Value::as_str) == Some(tool);
    let command = if current {
        row.get("command")
            .and_then(Value::as_str)
            .filter(|command| !command.is_empty())
    } else {
        config.session.custom_agents.get(tool).map(String::as_str)
    };
    let command = command
        .or_else(|| crate::agents::get_agent(tool).map(|agent| agent.binary))
        .unwrap_or("bash");
    container_config::sandbox_content_roots(tool, Some(command), &config.session, home, id)
}

/// Whether the same-kernel inode proof can authenticate a running container's
/// mounts. It needs a running container on a kernel-sharing host with no VM
/// runtime handler; otherwise the declared bind sources are the overlap
/// evidence and the proof is skipped rather than failing the caller.
fn can_prove_mounts(
    inspected: &crate::containers::InspectedContainer,
    host_shares_kernel: bool,
) -> bool {
    inspected.running && inspected.runtime_handler.is_none() && host_shares_kernel
}

/// The host path an `extra_volumes` entry contributes to the exposure scan, or
/// `None` for a named volume ("cache:/data"): a name is not a host path and
/// feeding it to `canonical_expected_path` would wrongly fail every launch and
/// migration. A source that starts with `.` is not a name: the runtime resolves
/// it against this process's current directory and binds that path, so it is a
/// host path and can reach the recovery namespace.
fn extra_volume_host_source(entry: &str) -> Option<PathBuf> {
    let (source, _) = entry.split_once(':')?;
    let source = PathBuf::from(source);
    if source.is_absolute() {
        return Some(source);
    }
    if !source.as_os_str().as_encoded_bytes().starts_with(b".") {
        return None;
    }
    std::env::current_dir().ok().map(|cwd| cwd.join(&source))
}

fn ordinary_mount_masks_bind(inspected: &crate::containers::InspectedContainer) -> bool {
    inspected.ordinary_mounts.iter().any(|ordinary| {
        inspected.bind_mounts.iter().any(|bind| {
            let bind = Path::new(&bind.container_path);
            bind.starts_with(&ordinary.container_path)
        })
    })
}

fn live_bind_sources(id: &str) -> Result<Vec<PathBuf>> {
    use std::os::unix::fs::MetadataExt;
    let mut container = crate::containers::DockerContainer::from_session_id(id);
    if !container.exists()? {
        return Ok(Vec::new());
    }
    let inspected = container
        .inspect()?
        .context("runtime cannot establish recovery mount isolation")?;
    let sources: Vec<_> = inspected
        .bind_mounts
        .iter()
        .map(|mount| PathBuf::from(&mount.host_path))
        .collect();
    if !inspected.running {
        return Ok(sources);
    }
    // The inode proof below authenticates a mount only when the container
    // shares the host kernel. A VM-backed runtime (Docker Desktop, OrbStack, a
    // Podman machine) or Apple `container` runs its own kernel, so its boot_id
    // can never match the host's, and a non-Linux host cannot share a kernel
    // with any container. In those cases the declared bind sources are the
    // authoritative overlap evidence, which the caller still canonicalizes
    // against the recovery namespace.
    if !can_prove_mounts(&inspected, crate::process::host_shares_container_kernel()) {
        return Ok(sources);
    }
    if !inspected.opaque_mounts.is_empty() {
        bail!("live sandbox {id} has unproven mounts; recovery exposure cannot be proven");
    }
    if ordinary_mount_masks_bind(&inspected) {
        bail!("live sandbox {id} masks a bind destination needed for recovery exposure proof");
    }
    if sources.is_empty() {
        return Ok(sources);
    }
    let boot = crate::process::boot_id().context("host kernel identity is unavailable")?;
    let boot = uuid::Uuid::parse_str(&boot).context("host kernel identity is malformed")?;
    let before: Vec<_> = sources
        .iter()
        .map(|source| fs::metadata(source).map(|metadata| (metadata.dev(), metadata.ino())))
        .collect::<std::io::Result<_>>()?;
    // Pin Docker/Podman's immutable runtime id.
    container.name = inspected.id;
    let mut command = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        r#"PATH=/usr/bin:/bin; export PATH; stat -f -c %t /proc/sys/kernel/random/boot_id && cat /proc/sys/kernel/random/boot_id && stat -L -c %d:%i -- "$@""#.to_owned(),
        "aoe-content-mount-proof".to_owned(),
    ];
    command.extend(
        inspected
            .bind_mounts
            .iter()
            .map(|mount| mount.container_path.clone()),
    );
    let argv = container.build_exec_argv("", &command);
    let (program, arguments) = argv
        .split_first()
        .context("runtime returned no exec program")?;
    let output = crate::process::run_with_timeout_process_group(
        std::process::Command::new(program)
            .args(arguments)
            .stdin(std::process::Stdio::null()),
        std::time::Duration::from_secs(10),
    )?
    .context("live mount proof timed out")?;
    if !output.status.success() {
        bail!("live sandbox mount proof failed; stop it before isolating native content");
    }
    let text = std::str::from_utf8(&output.stdout)?;
    let mut lines = text.lines();
    // A bind-mounted echo of the host UUID must not authenticate a VM. The
    // UUID must be read from genuine procfs in this same execution envelope. A
    // mismatch means a VM or remote daemon sat behind a local-looking socket
    // after all, so fall back to the declared sources rather than refusing.
    if lines.next() != Some("9fa0")
        || lines
            .next()
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            != Some(boot)
    {
        return Ok(sources);
    }
    for (source, expected) in sources.iter().zip(before) {
        let observed =
            lines
                .next()
                .and_then(|line| line.split_once(':'))
                .and_then(|(device, inode)| {
                    Some((device.parse::<u64>().ok()?, inode.parse::<u64>().ok()?))
                });
        let after = fs::metadata(source)?;
        if observed != Some(expected) || expected != (after.dev(), after.ino()) {
            bail!("live sandbox {id} source spelling does not identify its actual mount; defer native content isolation");
        }
    }
    if lines.next().is_some() {
        bail!("unexpected live mount proof output");
    }
    Ok(sources)
}

type ExposureProbe<'a> = dyn Fn(&str) -> Result<Vec<PathBuf>> + 'a;

/// The refusal a live mount deserves, or `None` when the recovery namespace
/// stays private. Callers that can leave work pending report this instead of
/// failing, so one exposed mount cannot stop every other session from moving.
fn recovery_exposure(
    app: &Path,
    targets: &[PathBuf],
    exposure: &ExposureProbe<'_>,
) -> Result<Option<String>> {
    for (path, registry) in read_registries(app)? {
        let profile = layout::profile_for_registry(app, &path);
        let config = crate::session::config::profile_config::resolve_config(&profile)?;
        for row in registry
            .as_array()
            .context("session registry must be an array")?
        {
            if row
                .pointer("/sandbox_info/enabled")
                .and_then(Value::as_bool)
                != Some(true)
            {
                continue;
            }
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .context("sandbox row has no id")?;
            let mut sources = match exposure(id) {
                Ok(sources) => sources,
                // A mount that cannot be proved clear is not proved clear, and
                // one unanswerable container must not stop every other session
                // from moving, so this defers rather than fails the pass.
                Err(error) => {
                    return Ok(Some(format!(
                        "sandbox {id}: cannot prove its mounts stay clear of the isolation recovery namespace ({error}); stop it and retry"
                    )))
                }
            };
            for entry in &config.sandbox.extra_volumes {
                if let Some(source) = extra_volume_host_source(entry) {
                    sources.push(source);
                }
            }
            if let Some(project) = row.get("project_path").and_then(Value::as_str) {
                let workspace = row
                    .get("workspace_info")
                    .filter(|value| !value.is_null())
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?;
                let (volumes, _) = if let Some(workspace) = workspace {
                    container_config::compute_workspace_volume_paths(
                        Path::new(project),
                        &workspace,
                    )?
                } else {
                    container_config::compute_volume_paths(Path::new(project), project)?
                };
                sources.extend(
                    volumes
                        .into_iter()
                        .map(|mount| PathBuf::from(mount.host_path)),
                );
            }
            for source in sources {
                let canonical = match canonical_expected_path(&source) {
                    Ok(canonical) => canonical,
                    // A declared mount AoE cannot resolve is not proof of
                    // privacy, and one unanswerable entry must not stop every
                    // other session from moving.
                    Err(error) => {
                        return Ok(Some(format!(
                            "sandbox {id} declares the mount {} that cannot be resolved ({error}); remove it and retry",
                            source.display()
                        )))
                    }
                };
                if targets.iter().any(|target| {
                    target.starts_with(&source)
                        || source.starts_with(target)
                        || target.starts_with(&canonical)
                        || canonical.starts_with(target)
                }) {
                    return Ok(Some(format!(
                        "sandbox {id} exposes the isolation recovery namespace through {}; remove that mount before migrating",
                        source.display()
                    )));
                }
            }
        }
    }
    Ok(None)
}

/// Retained originals must stay invisible to a running sandbox, so an exposed
/// namespace is a hard refusal where the caller cannot leave work pending.
fn ensure_private_recovery(
    app: &Path,
    targets: &[PathBuf],
    exposure: &ExposureProbe<'_>,
) -> Result<()> {
    match recovery_exposure(app, targets, exposure)? {
        Some(message) => bail!(message),
        None => Ok(()),
    }
}

fn carried_resume(receipt: &Receipt, tool: &str, agent: &str) -> bool {
    container_config::agent_retains_native_resume(agent)
        || receipt.roots.iter().any(|part| {
            part.original.is_some()
                && part.carried_tools.contains(tool)
                && part
                    .root
                    .roles
                    .iter()
                    .any(|role| container_config::content_role_agent(role) == Some(agent))
        })
}

fn record_retirement(
    receipt: &mut Receipt,
    row: &Value,
    home: &Path,
    config: &crate::session::Config,
) -> Result<()> {
    receipt.retired_identity = row.clone();
    receipt.retired_tools.clear();
    for tool in row_tools(row) {
        for root in row_roots(row, &tool, home, config)? {
            if receipt
                .roots
                .iter()
                .any(|part| part.original.is_some() && part.root.path == root.path)
            {
                if let Some(agent) = root
                    .roles
                    .iter()
                    .find_map(|role| container_config::content_role_agent(role))
                {
                    if !carried_resume(receipt, &tool, agent) {
                        receipt.retired_tools.insert(tool.clone(), agent.to_owned());
                    }
                }
            }
        }
    }
    Ok(())
}

fn new_receipt(app: &Path, row: &Value, tool: &str, roots: &[ContentRoot]) -> Result<Receipt> {
    let transaction = uuid::Uuid::new_v4().to_string();
    let instance = row
        .get("id")
        .and_then(Value::as_str)
        .context("sandbox row has no id")?
        .to_owned();
    let mut parts = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let owned = owned_root(app, &instance, &root.path)?.map(|certificate| certificate.identity);
        let physical = identity(&root.path)?;
        if owned.is_some() && owned != physical {
            bail!("owned native content root changed before planning");
        }
        let original = if owned.is_some() { None } else { physical };
        let parent = root
            .path
            .parent()
            .context("private content store has no parent")?;
        parts.push(RootTransition {
            root: root.clone(),
            stage: parent.join(format!(".v031-stage-{transaction}-{index}")),
            recovery: recovery_root(&root.host)?
                .join(&transaction)
                .join(index.to_string())
                .join("original"),
            original,
            staged: owned.clone(),
            published: owned,
            carried_tools: BTreeSet::new(),
        });
    }
    Ok(Receipt {
        policy: CONTENT_POLICY,
        instance,
        tool: tool.to_owned(),
        transaction,
        roots: parts,
        phase: Phase::Planned,
        retired_identity: row.clone(),
        retired_tools: BTreeMap::new(),
    })
}

fn durable_stage_parent(path: &Path) -> Result<AnchoredDir> {
    let parent = path.parent().context("stage parent has no ancestor")?;
    let leaf = Path::new(path.file_name().context("stage parent has no name")?);
    match AnchoredDir::open(path) {
        Ok(directory) => {
            // Replay the publication barrier after an earlier mkdir sync failure.
            AnchoredDir::open(parent)?.sync()?;
            Ok(directory)
        }
        Err(error) if !path.try_exists()? => durable_stage_parent(parent)?
            .create_child(leaf)
            .with_context(|| format!("creating stage parent after {error}")),
        Err(error) => Err(error),
    }
}

fn resume_candidates(
    row: &Value,
    root: &ContentRoot,
    home: &Path,
    config: &crate::session::Config,
) -> Result<Vec<ResumeCandidate>> {
    let mut candidates = Vec::new();
    for tool in row_tools(row) {
        let prior = if row.get("tool").and_then(Value::as_str) == Some(&tool) {
            Some(row)
        } else {
            row.get("prior_tool_session_ids")
                .and_then(|prior| prior.get(&tool))
        };
        let Some(id) = prior
            .and_then(|prior| prior.get("agent_session_id"))
            .and_then(Value::as_str)
            .filter(|id| crate::session::capture::is_valid_session_id(id))
        else {
            continue;
        };
        for matching in row_roots(row, &tool, home, config)?
            .into_iter()
            .filter(|matching| matching.path == root.path)
        {
            for role in matching
                .roles
                .iter()
                .filter(|role| root.roles.contains(role))
            {
                let Some(agent) = container_config::content_role_agent(role) else {
                    continue;
                };
                if matches!(agent, "gemini" | "kimi" | "prime-agent") {
                    candidates.push(ResumeCandidate {
                        tool: tool.clone(),
                        agent: agent.to_owned(),
                        id: id.to_owned(),
                    });
                }
            }
        }
    }
    Ok(candidates)
}

fn stage_receipt(
    app: &Path,
    receipt: &mut Receipt,
    path: &Path,
    home: &Path,
    config: &crate::session::Config,
    workspace: &Path,
) -> Result<()> {
    if receipt.phase != Phase::Planned {
        return Ok(());
    }
    let container_workdir = container_config::container_workdir_for(
        workspace
            .to_str()
            .context("sandbox project path is not UTF-8")?,
        receipt
            .retired_identity
            .pointer("/sandbox_info/container_workdir")
            .and_then(Value::as_str),
    );
    for part in &mut receipt.roots {
        if let Some(published) = &part.published {
            let certificate = owned_root(app, &receipt.instance, &part.root.path)?.context(
                "owned native content lost its physical certificate before role preparation",
            )?;
            if &certificate.identity != published {
                bail!("owned native content changed before role preparation");
            }
            let mut missing = part.root.clone();
            missing
                .roles
                .retain(|role| !certificate.root.roles.contains(role));
            if !missing.roles.is_empty() {
                container_config::extend_owned_content(&missing, home, &config.session, workspace)?;
                if identity(&part.root.path)?.as_ref() != Some(published) {
                    bail!("owned native content changed during role preparation");
                }
            }
            continue;
        }
        if identity(&part.root.path)? != part.original {
            bail!("native content root changed before staging");
        }
        let parent = part.stage.parent().context("stage has no parent")?;
        let anchor = durable_stage_parent(parent)?;
        let leaf = Path::new(part.stage.file_name().context("stage has no leaf")?);
        let source = if part.original.is_some() {
            &part.root.path
        } else {
            &part.root.host
        };
        let resumes = if part.original.is_some() {
            resume_candidates(&receipt.retired_identity, &part.root, home, config)?
        } else {
            Vec::new()
        };
        let capability = ContentSeed {
            source,
            stopped_original: part.original.is_some(),
            resumes: &resumes,
            container_workdir: &container_workdir,
        };
        let (staged, carried_tools) = container_config::retry_source_change(|| {
            // Only this transaction owns the stage. Never clear a published root.
            anchor.remove_staged_dir(leaf)?;
            let stage = anchor.create_child(leaf)?;
            let carried = container_config::seed_content_stage(
                &capability,
                &part.root,
                stage.path(),
                home,
                &config.session,
                workspace,
            )?;
            super::store_fs::barrier(&fs::File::open(stage.path())?)?;
            Ok((identity(stage.path())?, carried))
        })?;
        part.staged = staged;
        part.carried_tools = carried_tools;
    }
    receipt.phase = Phase::Staged;
    write_receipt(path, receipt)
}

/// Drop a stage that was built while a writer could still appear and return the
/// journal to `Planned`, so the next pass seeds from a source it has proven
/// stopped instead of publishing content a container may have written to.
fn discard_stage(app: &Path, receipt: &mut Receipt, path: &Path) -> Result<()> {
    // A part whose root no longer holds what it was planned from, or whose
    // owned root is no longer certified, has already begun publishing: the
    // retention rename, the stage rename, and the journal write that records
    // them are three steps a crash can separate, and the stage is what the
    // resume path needs in every one of those states.
    let mut mid_publication = false;
    for part in &receipt.roots {
        // A part that was planned from an original has begun publishing the
        // moment its root stops holding that original, whether or not the
        // journal has recorded the publish; a part that was already owned has
        // begun once its certificate no longer matches the root.
        let renamed = match &part.original {
            Some(original) => identity(&part.root.path)?.as_ref() != Some(original),
            None => {
                (part.published.is_some()
                    && owned_root(app, &receipt.instance, &part.root.path)?.is_none())
                    || (part.published.is_none()
                        && part.staged.is_some()
                        && identity(&part.root.path)? == part.staged)
            }
        };
        if renamed {
            mid_publication = true;
            break;
        }
    }
    if receipt.phase != Phase::Staged || mid_publication {
        return Ok(());
    }
    // Journal first: a stage that a later pass finds named but already gone is
    // read as a rename it may tolerate, and a leftover stage this transaction
    // owns is removed by the next `stage_receipt` before it seeds.
    for part in &mut receipt.roots {
        if part.published.is_none() {
            part.staged = None;
        }
    }
    receipt.phase = Phase::Planned;
    write_receipt(path, receipt)?;
    for part in &receipt.roots {
        if part.published.is_some() {
            continue;
        }
        let parent = part.stage.parent().context("stage has no parent")?;
        let anchor = AnchoredDir::open(parent)?;
        let leaf = Path::new(part.stage.file_name().context("stage has no leaf")?);
        anchor.remove_staged_dir(leaf)?;
    }
    Ok(())
}

fn publish_receipt(receipt: &mut Receipt, path: &Path) -> Result<()> {
    if receipt.phase != Phase::Staged {
        return Ok(());
    }
    receipt_directory(path)?;
    for index in 0..receipt.roots.len() {
        let part = &mut receipt.roots[index];
        if let Some(published) = &part.published {
            if identity(&part.root.path)?.as_ref() != Some(published) {
                bail!("published content root changed during recovery");
            }
            continue;
        }
        let active = identity(&part.root.path)?;
        let backup = identity(&part.recovery)?;
        if let Some(original) = &part.original {
            if backup.is_none() {
                if active.as_ref() != Some(original) {
                    bail!("original content root changed before retention");
                }
                let recovery = private_recovery_root(&part.root.host)?;
                let relative = part
                    .recovery
                    .parent()
                    .context("recovery has no parent")?
                    .strip_prefix(recovery.path())
                    .context("recovery escaped its private root")?;
                let parent = recovery.create_child(relative)?;
                parent.sync()?;
                recovery
                    .child(
                        relative
                            .parent()
                            .context("recovery index has no transaction")?,
                    )?
                    .sync()?;
                recovery.sync()?;
                rename_directory(&part.root.path, &part.recovery)?;
            } else if backup.as_ref() != Some(original) {
                bail!("retained original identity does not match its journal");
            }
        } else if backup.is_some() {
            bail!("fresh content transaction unexpectedly has an original");
        }
        if let Some(staged) = identity(&part.stage)? {
            if part.staged.as_ref() != Some(&staged) {
                bail!("staged content identity changed before publication");
            }
            if identity(&part.root.path)?.is_some() {
                bail!("a writer replaced the native content root during publication");
            }
            rename_directory(&part.stage, &part.root.path)?;
        }
        part.published = identity(&part.root.path)?;
        if part.published.is_none() || part.published != part.staged {
            bail!("fresh content publication identity does not match its staged receipt");
        }
        write_receipt(path, receipt)?;
    }
    receipt.phase = Phase::Published;
    write_receipt(path, receipt)
}

fn retired_json_binding_matches(
    current: Option<&Value>,
    retired: Option<&Value>,
    sid: Option<&str>,
) -> bool {
    let current = current.filter(|binding| !binding.is_null());
    match retired.filter(|binding| !binding.is_null()) {
        Some(retired) => current == Some(retired),
        None => current.is_none_or(|binding| {
            sid.is_some_and(|sid| {
                binding.get("session_id").and_then(Value::as_str) == Some(sid)
                    && binding.get("execution").is_none_or(Value::is_null)
                    && binding.get("provenance").and_then(Value::as_str) == Some("unknown")
                    && binding.get("transcript_path").is_none_or(Value::is_null)
            })
        }),
    }
}

fn reset_row(row: &mut Value, receipt: &Receipt) -> Result<()> {
    let object = row
        .as_object_mut()
        .context("sandbox row must be an object")?;
    if !receipt.roots.iter().any(|part| part.original.is_some()) {
        object.insert("sandbox_content_policy".into(), CONTENT_POLICY.into());
        return Ok(());
    }
    if object
        .get("sandbox_content_resets")
        .and_then(Value::as_array)
        .is_some_and(|resets| {
            resets.iter().any(|reset| {
                reset.get("transaction").and_then(Value::as_str) == Some(&receipt.transaction)
            })
        })
    {
        return Ok(());
    }
    let snapshot = &receipt.retired_identity;
    let snapshot_tool = snapshot.get("tool").and_then(Value::as_str);
    let original_for = |tool: &str| {
        if snapshot_tool == Some(tool) {
            Some(snapshot)
        } else {
            snapshot
                .get("prior_tool_session_ids")
                .and_then(|prior| prior.get(tool))
        }
    };
    let agents: BTreeSet<_> = receipt
        .roots
        .iter()
        .filter(|part| part.original.is_some())
        .flat_map(|part| part.root.roles.iter())
        .filter_map(|role| container_config::content_role_agent(role))
        .collect();
    let mut additions = Vec::new();
    for tool in row_tools(snapshot) {
        let prior = original_for(&tool);
        for agent in &agents {
            let terminal = receipt
                .retired_tools
                .get(&tool)
                .is_some_and(|canonical| canonical == agent);
            let current = snapshot_tool == Some(tool.as_str());
            let old_acp = prior
                .and_then(|prior| prior.get("acp_session_id"))
                .and_then(Value::as_str);
            if !terminal && !current && old_acp.is_none() {
                continue;
            }
            let mut retired_structured: Vec<String> =
                old_acp.map(str::to_owned).into_iter().collect();
            if current {
                if let Some(fork) = snapshot.get("fork_pending").and_then(Value::as_str) {
                    if !retired_structured.iter().any(|id| id == fork) {
                        retired_structured.push(fork.to_owned());
                    }
                }
            }
            let parts: Vec<_> =
                receipt
                    .roots
                    .iter()
                    .filter(|part| {
                        part.original.is_some()
                            && part.root.roles.iter().any(|role| {
                                container_config::content_role_agent(role) == Some(*agent)
                            })
                    })
                    .collect();
            additions.push(serde_json::to_value(SandboxContentReset {
                slot: uuid::Uuid::new_v4().to_string(),
                transaction: receipt.transaction.clone(),
                tool: tool.clone(),
                agent: (*agent).to_owned(),
                roots: parts.iter().map(|part| part.root.path.clone()).collect(),
                recovery: parts.iter().map(|part| part.recovery.clone()).collect(),
                terminal: ResetLane {
                    pending: terminal,
                    generation: None,
                },
                structured: ResetLane {
                    pending: (current || old_acp.is_some())
                        && (!carried_resume(receipt, &tool, agent)
                            || matches!(*agent, "gemini" | "kimi" | "prime-agent")),
                    generation: None,
                },
                retired_terminal: prior
                    .and_then(|prior| prior.get("agent_session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                retired_terminal_binding: prior
                    .and_then(|prior| prior.get("agent_session_binding"))
                    .filter(|binding| !binding.is_null())
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?,
                retired_structured,
                retired_import: current
                    && snapshot.get("import_pending").and_then(Value::as_bool) == Some(true),
            })?);
        }
    }
    object
        .entry("sandbox_content_resets")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("content resets must be an array")?
        .extend(additions);
    let current_tool = object
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut reset_current = false;
    if receipt.retired_tools.contains_key(&current_tool) {
        if let Some(prior) = original_for(&current_tool) {
            let same_id = object
                .get("agent_session_id")
                .filter(|value| !value.is_null())
                == prior
                    .get("agent_session_id")
                    .filter(|value| !value.is_null());
            let same_pi = object
                .get("pi_session_path")
                .filter(|value| !value.is_null())
                == prior
                    .get("pi_session_path")
                    .filter(|value| !value.is_null());
            let same_binding = retired_json_binding_matches(
                object.get("agent_session_binding"),
                prior.get("agent_session_binding"),
                prior.get("agent_session_id").and_then(Value::as_str),
            );
            let same_resume = object.get("resume_intent") == prior.get("resume_intent")
                && retired_json_binding_matches(
                    object.get("resume_binding"),
                    prior.get("resume_binding"),
                    None,
                );
            if same_id && same_pi && same_binding && same_resume {
                object.remove("agent_session_id");
                object.remove("agent_session_binding");
                object.remove("pi_session_path");
                object.remove("resume_binding");
                object.insert(
                    "resume_intent".into(),
                    serde_json::json!({"kind":"Cleared"}),
                );
                object.insert(
                    "capture_started_at".into(),
                    serde_json::to_value(std::time::SystemTime::now())?,
                );
                reset_current = true;
            }
        }
    }
    let mut retired_parked_omp = false;
    if let Some(parked) = object
        .get_mut("prior_tool_session_ids")
        .and_then(Value::as_object_mut)
    {
        for (tool, agent) in &receipt.retired_tools {
            if let (Some(current), Some(prior)) = (
                parked.get_mut(tool).and_then(Value::as_object_mut),
                original_for(tool),
            ) {
                if current
                    .get("agent_session_id")
                    .filter(|value| !value.is_null())
                    == prior
                        .get("agent_session_id")
                        .filter(|value| !value.is_null())
                    && retired_json_binding_matches(
                        current.get("agent_session_binding"),
                        prior.get("agent_session_binding"),
                        prior.get("agent_session_id").and_then(Value::as_str),
                    )
                {
                    let removed = current.remove("agent_session_id").is_some();
                    current.remove("agent_session_binding");
                    retired_parked_omp |= agent == "omp" && removed;
                }
            }
        }
    }
    if (reset_current
        && receipt
            .retired_tools
            .get(&current_tool)
            .is_some_and(|agent| agent == "omp"))
        || (retired_parked_omp
            && receipt
                .retired_tools
                .get(&current_tool)
                .is_none_or(|agent| agent != "omp"))
    {
        object.insert(
            "omp_capture_generation".into(),
            uuid::Uuid::new_v4().to_string().into(),
        );
    }
    object.insert("sandbox_content_policy".into(), CONTENT_POLICY.into());
    Ok(())
}

/// Record on one row the context a committed transaction retired, when that row
/// still resolves the same store and never recorded it. Callers hold the
/// registry lock; the row that ran the transaction is left exactly as it is.
fn record_reset_in(
    app: &Path,
    registry: &Path,
    id: &str,
    tool: &str,
    home: &Path,
    config: &crate::session::Config,
    roots: &[ContentRoot],
) -> Result<()> {
    let mut fresh: Value = serde_json::from_slice(&fs::read(registry)?)?;
    let Some(current) = fresh
        .as_array_mut()
        .context("session registry must be an array")?
        .iter_mut()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
    else {
        return Ok(());
    };
    let mut current_roots = row_roots(current, tool, home, config)?;
    container_config::expand_content_roles(&mut current_roots, home, &config.session)?;
    // The reset is per transaction, and `reset_row` already answers for this
    // one. What the caller must decide is whether the row still resolves the
    // store the transaction moved, which is about paths: a shared config
    // directory gains roles from later tool registrations without moving.
    let resolves_same_store: BTreeSet<_> =
        current_roots.iter().map(|root| root.path.clone()).collect();
    if resolves_same_store != roots.iter().map(|root| root.path.clone()).collect() {
        return Ok(());
    }
    let Some(receipt) = retired_receipt(app, id, tool, current, &current_roots)? else {
        return Ok(());
    };
    let before = current.clone();
    reset_row(current, &receipt)?;
    if *current != before {
        write_registry(registry, &fresh)?;
    }
    Ok(())
}

/// The journal entry that retired this row's context, read from beside the live
/// receipt because a completed transaction archives its own.
fn retired_receipt(
    app: &Path,
    instance: &str,
    tool: &str,
    row: &Value,
    roots: &[ContentRoot],
) -> Result<Option<Receipt>> {
    let path = receipt_path(app, instance, tool)?;
    let mut receipts = Vec::new();
    if let Some(receipt) = read_receipt(&path)? {
        receipts.push(receipt);
    }
    let Some(directory) = path.parent() else {
        return Ok(None);
    };
    let key = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    for entry in match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    } {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(&format!("{key}.")) || !name.ends_with(".complete") {
            continue;
        }
        if let Some(receipt) = read_receipt(&entry.path())? {
            receipts.push(receipt);
        }
    }
    let expected: BTreeSet<_> = roots.iter().map(|root| root.path.clone()).collect();
    Ok(receipts.into_iter().find(|receipt| {
        if receipt.phase != Phase::Committed
            || receipt.roots.iter().all(|part| part.original.is_none())
            || receipt.retired_identity.get("id") != row.get("id")
        {
            return false;
        }
        // The archived receipt must belong to the roots this row resolves now:
        // once a store changes to other already-certified roots, an older
        // receipt would otherwise attach the wrong transaction's recovery paths.
        let receipt_roots: BTreeSet<_> = receipt
            .roots
            .iter()
            .map(|part| part.root.path.clone())
            .collect();
        if receipt_roots != expected {
            return false;
        }
        // The terminal lane keys on the native session id; the structured lane
        // keys on the ACP session id and fork state. Match either identity
        // independently so an ACP-only side row (no native id) still resets.
        let matches_field = |field: &str| {
            row.get(field)
                .filter(|value| !value.is_null())
                .is_some_and(|current| {
                    receipt
                        .retired_identity
                        .get(field)
                        .filter(|value| !value.is_null())
                        == Some(current)
                })
        };
        let terminal = !receipt.retired_tools.is_empty() && matches_field("agent_session_id");
        let structured = matches_field("acp_session_id")
            && row.get("fork_pending") == receipt.retired_identity.get("fork_pending");
        terminal || structured
    }))
}

fn detached_writer_live(app: &Path, id: &str) -> Result<bool> {
    let path = app.join("acp-workers").join(format!("{id}.json"));
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let record: crate::process::worker_registry::WorkerRecord = serde_json::from_slice(&bytes)?;
    if record.session_id != id {
        bail!("detached worker record identity mismatch");
    }
    Ok(crate::process::worker::is_pid_alive(record.pid))
}

fn checked_receipt(
    app: &Path,
    row: &Value,
    tool: &str,
    roots: &[ContentRoot],
) -> Result<(PathBuf, Receipt)> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .context("sandbox row has no id")?;
    let path = receipt_path(app, id, tool)?;
    guard_other_transactions(app, id, tool, roots)?;
    if let Some(receipt) = read_receipt(&path)? {
        let matches = receipt_matches(&receipt, id, tool, roots);
        if !matches && receipt.phase != Phase::Committed {
            bail!("unfinished content transition has a different root or role plan; restore the original configuration and finish aoe migrate; its originals and journal are retained");
        }
        uuid::Uuid::parse_str(&receipt.transaction)
            .context("invalid content transaction identity")?;
        for (index, part) in receipt.roots.iter().enumerate() {
            let expected_stage = part
                .root
                .path
                .parent()
                .context("root has no parent")?
                .join(format!(".v031-stage-{}-{index}", receipt.transaction));
            let expected_recovery = recovery_root(&part.root.host)?
                .join(&receipt.transaction)
                .join(index.to_string())
                .join("original");
            if part.stage != expected_stage || part.recovery != expected_recovery {
                bail!("content journal contains paths outside its owned transaction");
            }
        }
        if matches && (receipt.phase != Phase::Committed || roots_are_current(&receipt)?) {
            return Ok((path, receipt));
        }
        if roots_are_current(&receipt)? {
            certify_receipt(app, &receipt)?;
        }
        archive_receipt(&path, &receipt)?;
    }
    let receipt = new_receipt(app, row, tool, roots)?;
    write_receipt(&path, &receipt)?;
    Ok((path, receipt))
}

fn migration_targets(app: &Path, roots: &[ContentRoot]) -> Result<Vec<PathBuf>> {
    let mut targets = vec![canonical_expected_path(&app.join(RECEIPTS))?];
    for root in roots {
        targets.push(canonical_expected_path(&recovery_root(&root.host)?)?);
    }
    targets.sort();
    targets.dedup();
    Ok(targets)
}

fn migrate_target(
    app: &Path,
    home: &Path,
    target: (&Path, &str, &str),
    running: &dyn Fn(&str) -> Result<bool>,
    reap: &dyn Fn(&str) -> Result<bool>,
    exposure: &ExposureProbe<'_>,
) -> Result<bool> {
    let (registry, id, tool) = target;
    let profile = layout::profile_for_registry(app, registry);
    let config = crate::session::config::profile_config::resolve_config(&profile)?;
    let Some(snapshot) = read_row(registry, id)? else {
        return Ok(false);
    };
    let mut roots = row_roots(&snapshot, tool, home, &config)?;
    if roots.is_empty() || roots_ready(app, id, tool, &roots)? {
        // A store another row already moved still has to stop this row from
        // resuming the retired context, and this is the only pass that sees it.
        let registries = lock_registries(app)?;
        record_reset_in(app, registry, id, tool, home, &config, &roots)?;
        drop(registries);
        return Ok(true);
    }
    container_config::expand_content_roles(&mut roots, home, &config.session)?;
    let mut cohorts = Vec::with_capacity(roots.len());
    for root in &roots {
        cohorts.push(layout::acquire_cohort_lock(app, &root.path)?);
    }
    let mut transition = Some(crate::session::acquire_storage_flock(app, layout::LOCK)?);
    let mut registries = Some(lock_registries(app)?);
    let Some(row) = read_row(registry, id)? else {
        return Ok(false);
    };
    let config = crate::session::config::profile_config::resolve_config(&profile)?;
    let mut locked_roots = row_roots(&row, tool, home, &config)?;
    container_config::expand_content_roles(&mut locked_roots, home, &config.session)?;
    if locked_roots != roots || !row_tools(&row).contains(tool) {
        return Ok(false);
    }
    if row
        .get("sandbox_store_generation")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        < 2
    {
        return Ok(false);
    }
    if running(id)? || detached_writer_live(app, id)? {
        progress::notice(format!(
            "sandbox {id}: stop its container and structured runner to isolate native history"
        ));
        return Ok(false);
    }
    if let Some(message) = recovery_exposure(app, &migration_targets(app, &roots)?, exposure)? {
        progress::notice(format!("{message}; this sandbox stays pending"));
        return Ok(false);
    }
    let (path, mut receipt) = checked_receipt(app, &row, tool, &roots)?;
    if receipt.phase == Phase::Committed {
        certify_receipt(app, &receipt)?;
        archive_receipt(&path, &receipt)?;
        // The row that ran the transaction recorded its own reset. Every other
        // row resolving this store still has to stop resuming the context that
        // transaction retired.
        record_reset_in(app, registry, id, tool, home, &config, &roots)?;
        return Ok(true);
    }
    discard_stage(app, &mut receipt, &path)?;
    if receipt.phase == Phase::Planned {
        receipt.retired_identity = row.clone();
        write_receipt(&path, &receipt)?;
    }
    drop(registries.take());
    drop(transition.take());
    let workspace = Path::new(
        row.get("project_path")
            .and_then(Value::as_str)
            .context("sandbox row has no project path")?,
    );
    stage_receipt(app, &mut receipt, &path, home, &config, workspace)?;
    transition = Some(crate::session::acquire_storage_flock(app, layout::LOCK)?);
    registries = Some(lock_registries(app)?);
    let fresh_config = crate::session::config::profile_config::resolve_config(&profile)?;
    let mut fresh: Value = serde_json::from_slice(&fs::read(registry)?)?;
    let Some(current) = fresh
        .as_array_mut()
        .context("session registry must be an array")?
        .iter_mut()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
    else {
        discard_stage(app, &mut receipt, &path)?;
        return Ok(false);
    };
    let mut current_roots = row_roots(current, tool, home, &fresh_config)?;
    container_config::expand_content_roles(&mut current_roots, home, &fresh_config.session)?;
    if current_roots != roots
        || !row_tools(current).contains(tool)
        || current.get("project_path") != row.get("project_path")
        || [
            "tool",
            "command",
            "extra_args",
            "detect_as",
            "agent_session_id",
            "agent_session_binding",
            "resume_intent",
            "resume_binding",
            "prior_tool_session_ids",
        ]
        .iter()
        .any(|field| current.get(*field) != row.get(*field))
        || current.pointer("/sandbox_info/container_workdir")
            != row.pointer("/sandbox_info/container_workdir")
    {
        // A container could have started after the stage was seeded, so the
        // seed is dropped with the plan it was made for.
        discard_stage(app, &mut receipt, &path)?;
        return Ok(false);
    }
    layout::refresh_liveness();
    if running(id)? || detached_writer_live(app, id)? || !reap(id)? {
        progress::notice(format!(
            "sandbox {id}: stop its container and structured runner to isolate native history"
        ));
        discard_stage(app, &mut receipt, &path)?;
        return Ok(false);
    }
    if let Some(message) = recovery_exposure(app, &migration_targets(app, &roots)?, exposure)? {
        progress::notice(format!("{message}; this sandbox stays pending"));
        discard_stage(app, &mut receipt, &path)?;
        return Ok(false);
    }
    if receipt.phase == Phase::Staged {
        record_retirement(&mut receipt, current, home, &fresh_config)?;
        write_receipt(&path, &receipt)?;
    }
    publish_receipt(&mut receipt, &path)?;
    reset_row(current, &receipt)?;
    write_registry(registry, &fresh)?;
    receipt.phase = Phase::Committed;
    write_receipt(&path, &receipt)?;
    certify_receipt(app, &receipt)?;
    archive_receipt(&path, &receipt)?;
    drop(registries);
    drop(transition);
    drop(cohorts);
    Ok(true)
}

fn reconcile_in(
    app: &Path,
    home: &Path,
    only: Option<&str>,
    move_stores: bool,
    running: &dyn Fn(&str) -> Result<bool>,
    reap: &dyn Fn(&str) -> Result<bool>,
    exposure: &ExposureProbe<'_>,
) -> Result<()> {
    let mut targets = BTreeMap::new();
    for (path, registry) in read_registries(app)? {
        for row in registry
            .as_array()
            .context("session registry must be an array")?
        {
            if row
                .pointer("/sandbox_info/enabled")
                .and_then(Value::as_bool)
                != Some(true)
            {
                continue;
            }
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .context("sandbox row has no id")?;
            crate::session::validate_instance_id(id)?;
            if only.is_some_and(|only| only != id) {
                continue;
            }
            for tool in row_tools(row) {
                targets.insert((path.clone(), id.to_owned(), tool), ());
            }
        }
    }
    for ((path, id, tool), ()) in targets {
        if move_stores || only.is_some() {
            if let Err(error) =
                migrate_target(app, home, (&path, &id, &tool), running, reap, exposure)
            {
                if !container_config::source_changed(&error) {
                    return Err(error);
                }
                tracing::warn!(target: "session.profile", %error, %id, %tool, "Native source kept changing during content isolation");
                progress::notice(format!("sandbox {id}: native configuration kept changing; content isolation remains pending"));
            }
        } else {
            let config = crate::session::config::profile_config::resolve_config(
                &layout::profile_for_registry(app, &path),
            )?;
            if let Some(row) = read_row(&path, &id)? {
                // A parked row cannot be acted on by a bare start, and v027 is
                // deliberately silent for a backlog it only holds; the startup
                // pass must not narrate work no start can do.
                if super::v027_isolate_sandbox_stores::row_is_parked(&row) {
                    continue;
                }
                let roots = row_roots(&row, &tool, home, &config)?;
                if !roots_ready(app, &id, &tool, &roots)? {
                    progress::notice(format!("sandbox {id}: native content isolation pending; originals will be preserved before a fresh native session starts"));
                }
            }
        }
    }
    Ok(())
}

pub fn run() -> Result<()> {
    // A prerelease v031 may have used this number for content isolation.
    super::v031_conversation_provenance::run()?;
    // Content isolation is keyed by home. A host without one still advances
    // the schema and retries reconciliation once a home is available.
    if dirs::home_dir().is_none() {
        return Ok(());
    }
    if progress::announced() {
        layout::reconcile_pending(true)?;
    }
    reconcile_pending(progress::announced())
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct TestReconcileProbes {
    running: fn(&str) -> Result<bool>,
    reap: fn(&str) -> Result<bool>,
    exposure: fn(&str) -> Result<Vec<PathBuf>>,
}

#[cfg(test)]
thread_local! {
    static TEST_RECONCILE_PROBES: std::cell::Cell<Option<TestReconcileProbes>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn reconcile_pending(move_stores: bool) -> Result<()> {
    reconcile_pending_with_home(move_stores, dirs::home_dir())
}

fn reconcile_pending_with_home(move_stores: bool, home: Option<PathBuf>) -> Result<()> {
    // Match run(): startup may reconcile an already-current schema without HOME.
    let Some(home) = home else {
        return Ok(());
    };
    let app = crate::session::get_app_dir()?;
    #[cfg(test)]
    if let Some(probes) = TEST_RECONCILE_PROBES.get() {
        return reconcile_in(
            &app,
            &home,
            None,
            move_stores,
            &probes.running,
            &probes.reap,
            &probes.exposure,
        );
    }
    reconcile_in(
        &app,
        &home,
        None,
        move_stores,
        &layout::batched_running_probe(false),
        &layout::reap_migrated_container,
        &live_bind_sources,
    )
}

pub(crate) fn migrate_instance(id: &str) -> Result<()> {
    let app = crate::session::get_app_dir()?;
    let home = dirs::home_dir().context("home directory unavailable for content isolation")?;
    reconcile_in(
        &app,
        &home,
        Some(id),
        true,
        &layout::batched_running_probe(false),
        &layout::reap_migrated_container,
        &live_bind_sources,
    )
}

/// Fresh builders may certify only absent roots or already certified roots.
/// Existing unproven data requires the stopped, registry-backed migration.
pub(crate) fn ensure_fresh_content(
    app: &Path,
    home: &Path,
    instance: &str,
    tool: &str,
    roots: &[ContentRoot],
    config: &crate::session::Config,
    workspace: &Path,
) -> Result<crate::session::StorageFlock> {
    crate::session::validate_instance_id(instance)?;
    if !roots_ready(app, instance, tool, roots)? {
        // Never wait for a cohort held by a migrator while already holding the
        // transition lock. Callers initialize fresh stores before launch admission.
        let mut planned = roots.to_vec();
        container_config::expand_content_roles(&mut planned, home, &config.session)?;
        let mut cohorts = Vec::with_capacity(planned.len());
        for root in &planned {
            cohorts.push(layout::acquire_cohort_lock(app, &root.path)?);
        }
        if !roots_ready(app, instance, tool, &planned)? {
            let transition = crate::session::acquire_storage_flock(app, layout::LOCK)?;
            if !pending_receipt(app, instance, tool)? {
                for root in &planned {
                    if owned_root(app, instance, &root.path)?.is_none()
                        && (identity(&root.path)?.is_some()
                            || certificate_path(app, instance, &root.path)?.exists())
                    {
                        bail!("sandbox {instance} has unproven native content; stop it and run aoe migrate before relaunch");
                    }
                }
            }
            ensure_private_recovery(app, &migration_targets(app, &planned)?, &live_bind_sources)?;
            let row = serde_json::json!({"id":instance,"tool":tool});
            let (path, mut receipt) = checked_receipt(app, &row, tool, &planned)?;
            if receipt.roots.iter().any(|part| part.original.is_some()) {
                bail!("fresh-store admission cannot retire existing native content");
            }
            drop(transition);
            stage_receipt(app, &mut receipt, &path, home, config, workspace)?;
            let _transition = crate::session::acquire_storage_flock(app, layout::LOCK)?;
            let agent = roots
                .first()
                .and_then(|root| root.roles.first())
                .and_then(|role| container_config::content_role_agent(role));
            let mut current = container_config::sandbox_content_roots(
                tool,
                agent,
                &config.session,
                home,
                instance,
            )?;
            container_config::expand_content_roles(&mut current, home, &config.session)?;
            if current != planned {
                bail!("native content configuration changed during role preparation");
            }
            ensure_private_recovery(app, &migration_targets(app, &planned)?, &live_bind_sources)?;
            publish_receipt(&mut receipt, &path)?;
            receipt.phase = Phase::Committed;
            write_receipt(&path, &receipt)?;
            certify_receipt(app, &receipt)?;
            archive_receipt(&path, &receipt)?;
        }
    }
    let transition = crate::session::acquire_storage_shared_flock(app, layout::LOCK)?;
    if !roots_ready(app, instance, tool, roots)? {
        bail!("native content roots changed during launch admission");
    }
    Ok(transition)
}

pub(crate) fn instance_roots(instance: &crate::session::Instance) -> Result<Vec<ContentRoot>> {
    let home = dirs::home_dir().context("home directory unavailable for content isolation")?;
    let config =
        crate::session::config::profile_config::resolve_config(&instance.effective_profile())?;
    container_config::sandbox_content_roots(
        &instance.tool,
        Some(instance.get_tool_command()),
        &config.session,
        &home,
        &instance.id,
    )
}

pub(crate) fn instance_ready(instance: &crate::session::Instance) -> Result<bool> {
    if !instance.is_sandboxed() {
        return Ok(true);
    }
    if instance.sandbox_store_generation < container_config::CURRENT_SANDBOX_STORE_GENERATION {
        return Ok(false);
    }
    roots_ready(
        &crate::session::get_app_dir()?,
        &instance.id,
        &instance.tool,
        &instance_roots(instance)?,
    )
}

pub(crate) fn admit_fresh_instance(
    instance: &crate::session::Instance,
) -> Result<crate::session::StorageFlock> {
    if instance.sandbox_store_generation < container_config::CURRENT_SANDBOX_STORE_GENERATION {
        bail!("sandbox {} still uses a legacy shared store; stop the other owners and run aoe migrate", instance.id);
    }
    let app = crate::session::get_app_dir()?;
    let home = dirs::home_dir().context("home directory unavailable for content isolation")?;
    let config =
        crate::session::config::profile_config::resolve_config(&instance.effective_profile())?;
    ensure_fresh_content(
        &app,
        &home,
        &instance.id,
        &instance.tool,
        &instance_roots(instance)?,
        &config,
        Path::new(&instance.container_workdir()),
    )
}

pub(crate) fn guard_preparation(
    host: &Path,
    sandbox: &Path,
    role: &str,
) -> Result<crate::session::StorageFlock> {
    let instance = sandbox
        .file_name()
        .and_then(|name| name.to_str())
        .context("sandbox path has no instance id")?;
    crate::session::validate_instance_id(instance)?;
    let app = crate::session::get_app_dir()?;
    let transition = crate::session::acquire_storage_shared_flock(&app, layout::LOCK)?;
    let root = ContentRoot {
        path: canonical_expected_path(sandbox)?,
        host: canonical_expected_path(host)?,
        roles: vec![role.to_owned()],
    };
    if !root_ready(&app, instance, &root)? {
        bail!("sandbox {instance}: refusing configuration refresh on unproven native content");
    }
    Ok(transition)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SyncFailureGuard;

    impl Drop for SyncFailureGuard {
        fn drop(&mut self) {
            crate::session::anchored_fs::FAIL_SYNC_ONCE.set(None);
        }
    }

    #[test]
    #[serial_test::serial]
    fn receipt_namespace_sync_failure_preserves_original_until_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let mut instance =
            crate::session::Instance::new("codex", temporary.path().to_str().unwrap());
        instance.tool = "codex".to_owned();
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(root.join("sessions/original.jsonl"), b"SYNTHETIC_ORIGINAL").unwrap();
        fs::write(root.join("config.toml"), b"model = 'fixture'\n").unwrap();
        let original = identity(root).unwrap();
        let row = serde_json::to_value(&instance).unwrap();
        let path = receipt_path(&app, &instance.id, "codex").unwrap();
        let _failure = SyncFailureGuard;
        for _ in 0..2 {
            crate::session::anchored_fs::FAIL_SYNC_ONCE.set(Some(app.clone()));
            assert!(checked_receipt(&app, &row, "codex", &roots).is_err());
            assert_eq!(identity(root).unwrap(), original);
            assert_eq!(
                fs::read(root.join("sessions/original.jsonl")).unwrap(),
                b"SYNTHETIC_ORIGINAL"
            );
            assert!(read_receipt(&path).unwrap().is_none());
            assert!(!roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        }
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(None);
        let (path, mut receipt) = checked_receipt(&app, &row, "codex", &roots).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(Some(app.clone()));
        assert!(publish_receipt(&mut receipt, &path).is_err());
        assert_eq!(identity(root).unwrap(), original);
        assert!(!receipt.roots[0].recovery.exists());
        assert_eq!(read_receipt(&path).unwrap().unwrap().phase, Phase::Staged);
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(None);
        publish_receipt(&mut receipt, &path).unwrap();
        receipt.phase = Phase::Committed;
        write_receipt(&path, &receipt).unwrap();
        certify_receipt(&app, &receipt).unwrap();
        archive_receipt(&path, &receipt).unwrap();
        assert!(roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert_eq!(identity(&receipt.roots[0].recovery).unwrap(), original);
        assert_eq!(
            fs::read(receipt.roots[0].recovery.join("sessions/original.jsonl")).unwrap(),
            b"SYNTHETIC_ORIGINAL"
        );
        assert!(!path.exists());
    }

    #[test]
    #[serial_test::serial]
    fn stage_parent_sync_failure_keeps_planned_original_and_retries() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let mut instance =
            crate::session::Instance::new("codex", temporary.path().to_str().unwrap());
        instance.tool = "codex".to_owned();
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(root.join("sessions/original.jsonl"), b"SYNTHETIC_ORIGINAL").unwrap();
        fs::write(root.join("config.toml"), b"model = 'fixture'\n").unwrap();
        let original = identity(root).unwrap();
        let row = serde_json::to_value(&instance).unwrap();
        let (path, mut receipt) = checked_receipt(&app, &row, "codex", &roots).unwrap();
        let _failure = SyncFailureGuard;
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(Some(root.parent().unwrap().to_path_buf()));
        assert!(
            stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).is_err()
        );
        assert_eq!(receipt.phase, Phase::Planned);
        assert_eq!(read_receipt(&path).unwrap().unwrap().phase, Phase::Planned);
        assert_eq!(identity(root).unwrap(), original);
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"SYNTHETIC_ORIGINAL"
        );
        assert!(!receipt.roots[0].recovery.exists());
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(None);
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
        publish_receipt(&mut receipt, &path).unwrap();
        receipt.phase = Phase::Committed;
        write_receipt(&path, &receipt).unwrap();
        certify_receipt(&app, &receipt).unwrap();
        archive_receipt(&path, &receipt).unwrap();
        assert!(roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert_eq!(identity(&receipt.roots[0].recovery).unwrap(), original);
        assert!(!path.exists());
    }

    #[test]
    fn nested_stage_parent_sync_failure_retries_existing_components() {
        let temporary = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let nested = root.join("new/parents/stages");
        let _failure = SyncFailureGuard;
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(Some(root.clone()));
        assert!(durable_stage_parent(&nested).is_err());
        assert!(!nested.exists());
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(Some(root.clone()));
        assert!(durable_stage_parent(&nested).is_err());
        assert!(!nested.exists());
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(None);
        let parent = durable_stage_parent(&nested).unwrap();
        assert_eq!(identity(parent.path()).unwrap(), identity(&nested).unwrap());
        parent.create_child(Path::new("stage/tree/nested")).unwrap();
        assert!(nested.join("stage/tree/nested").is_dir());
    }

    struct TestReconcileGuard {
        content: Option<TestReconcileProbes>,
        layout: Option<layout::TestReconcileProbes>,
    }

    impl Drop for TestReconcileGuard {
        fn drop(&mut self) {
            TEST_RECONCILE_PROBES.set(self.content);
            layout::TEST_RECONCILE_PROBES.set(self.layout);
        }
    }

    fn install_test_reconcile_probes(
        running: fn(&str) -> Result<bool>,
        reap: fn(&str) -> Result<bool>,
        exposure: fn(&str) -> Result<Vec<PathBuf>>,
    ) -> TestReconcileGuard {
        TestReconcileGuard {
            content: TEST_RECONCILE_PROBES.replace(Some(TestReconcileProbes {
                running,
                reap,
                exposure,
            })),
            layout: layout::TEST_RECONCILE_PROBES
                .replace(Some(layout::TestReconcileProbes { running, reap })),
        }
    }

    /// The same-kernel mount proof runs only for a running container on a
    /// kernel-sharing host with no VM runtime handler. A VM-backed runtime
    /// (Docker Desktop, OrbStack, a Podman machine), Apple `container`, or a
    /// non-Linux host cannot be proven, so its declared bind sources are trusted
    /// rather than failing a new session because a sibling sandbox exists.
    #[test]
    fn mounts_are_provable_only_on_a_kernel_sharing_native_runtime() {
        use crate::containers::InspectedContainer;
        let inspected = |running: bool, handler: Option<&str>| InspectedContainer {
            id: "c".into(),
            running,
            bind_mounts: Vec::new(),
            ordinary_mounts: Vec::new(),
            opaque_mounts: Vec::new(),
            runtime_handler: handler.map(str::to_owned),
        };
        assert!(can_prove_mounts(&inspected(true, None), true));
        assert!(!can_prove_mounts(
            &inspected(true, Some("container-runtime-linux")),
            true
        ));
        assert!(!can_prove_mounts(&inspected(true, None), false));
        assert!(!can_prove_mounts(&inspected(false, None), true));
    }

    #[test]
    fn ordinary_volume_only_blocks_a_mount_proof_when_destinations_overlap() {
        use crate::containers::container_interface::InspectedMount;
        use crate::containers::{InspectedContainer, VolumeMount};

        let mut inspected = InspectedContainer {
            id: "c".into(),
            running: true,
            bind_mounts: vec![VolumeMount {
                host_path: "/host/source".into(),
                container_path: "/workspace".into(),
                read_only: false,
            }],
            ordinary_mounts: vec![InspectedMount {
                kind: "volume".into(),
                name: Some("cache".into()),
                source: Some("/runtime/cache".into()),
                container_path: "/cache".into(),
                read_only: false,
            }],
            opaque_mounts: Vec::new(),
            runtime_handler: None,
        };
        assert!(!ordinary_mount_masks_bind(&inspected));
        inspected.ordinary_mounts[0].container_path = "/workspace".into();
        assert!(ordinary_mount_masks_bind(&inspected));
        inspected.ordinary_mounts[0].container_path = "/workspace/cache".into();
        assert!(!ordinary_mount_masks_bind(&inspected));
        inspected.ordinary_mounts[0].container_path = "/".into();
        assert!(ordinary_mount_masks_bind(&inspected));
    }

    #[test]
    #[serial_test::serial]
    fn unavailable_home_preserves_pending_content_for_later_reconciliation() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        let version = super::super::current_schema_version().to_string();
        fs::write(app.join(".schema_version"), &version).unwrap();
        let registry = br#"[{"id":"1111111111111111","tool":"codex","sandbox_store_generation":2,"sandbox_info":{"enabled":true}}]"#;
        fs::write(app.join("sessions.json"), registry).unwrap();

        // Linux falls back to passwd when HOME is unset; inject the genuinely
        // unavailable result instead of changing process-wide user identity.
        reconcile_pending_with_home(false, None).unwrap();
        assert_eq!(fs::read(app.join("sessions.json")).unwrap(), registry);
        assert_eq!(
            fs::read(app.join(".schema_version")).unwrap(),
            version.as_bytes()
        );
    }

    /// A named volume or a relative source is not a host path, so it is dropped
    /// from the exposure scan; an absolute host source is kept for the overlap
    /// check. Feeding a named volume to `canonical_expected_path` used to fail
    /// every launch and migration.
    #[test]
    fn only_host_path_extra_volume_sources_enter_the_exposure_scan() {
        assert_eq!(extra_volume_host_source("cache:/data"), None);
        assert_eq!(extra_volume_host_source("no-colon"), None);
        assert_eq!(
            extra_volume_host_source("/abs/host:/data:ro"),
            Some(PathBuf::from("/abs/host"))
        );
        // A dot-prefixed source is not a named volume: the runtime resolves it
        // against this process's directory, so it belongs in the overlap check.
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            extra_volume_host_source("./rel:/data"),
            Some(cwd.join("./rel"))
        );
        assert_eq!(extra_volume_host_source(".:/data"), Some(cwd.join(".")));
    }

    /// A side row that resolves the moved store through its ACP session id
    /// alone, with no native session id, must still be reset: the structured
    /// identity is matched independently of the terminal one, or that row keeps
    /// resuming the retired adapter context.
    #[test]
    #[serial_test::serial]
    fn an_acp_only_side_row_records_the_retired_structured_context() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("codex", project.to_str().unwrap());
        instance.tool = "codex".to_owned();
        // ACP-only: a structured lane with no native session id.
        instance.acp_session_id = Some("retired-adapter-context".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(
            root.join("sessions/original.jsonl"),
            b"PRIVATE_ORIGINAL_CONTEXT",
        )
        .unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let main = app.join("sessions.json");
        fs::write(&main, serde_json::to_vec(&vec![row.clone()]).unwrap()).unwrap();
        let side = app.join("profiles/side/sessions.json");
        fs::create_dir_all(side.parent().unwrap()).unwrap();
        fs::write(&side, serde_json::to_vec(&vec![row]).unwrap()).unwrap();

        for registry in [&main, &side] {
            assert!(
                migrate_target(
                    &app,
                    &home,
                    (registry, &instance.id, "codex"),
                    &|_| Ok(false),
                    &|_| Ok(true),
                    &|_| Ok(Vec::new()),
                )
                .unwrap(),
                "each row of a moved store completes"
            );
        }
        let rows: Value = serde_json::from_slice(&fs::read(&side).unwrap()).unwrap();
        let row = &rows[0];
        assert_eq!(
            row.get("sandbox_content_policy").and_then(Value::as_u64),
            Some(u64::from(CONTENT_POLICY)),
            "the ACP-only side row was not reset: {row}"
        );
        let retired_structured: Vec<&str> = row
            .get("sandbox_content_resets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|reset| reset.get("retired_structured").and_then(Value::as_array))
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            retired_structured.contains(&"retired-adapter-context"),
            "the retired adapter context is not recorded on the side row: {row}"
        );
    }

    /// With more than one archived transaction for the same identity, the reset
    /// must bind to the receipt whose roots the row resolves now, or a store
    /// change to other already-certified roots would attach the wrong
    /// transaction's recovery paths.
    #[test]
    #[serial_test::serial]
    fn retired_receipt_scopes_roots_and_preserves_newer_bindings() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let instance = crate::session::Instance::new("codex", temporary.path().to_str().unwrap());
        let mut row = serde_json::to_value(&instance).unwrap();
        row["tool"] = "codex".into();
        row["agent_session_id"] = "old-native-context".into();
        let config = crate::session::Config::default();
        let current = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let other = vec![ContentRoot {
            path: temporary.path().join("other-roots"),
            host: temporary.path().to_path_buf(),
            roles: current[0].roles.clone(),
        }];
        let make = |transaction: &str, roots: &[ContentRoot]| Receipt {
            policy: CONTENT_POLICY,
            instance: instance.id.clone(),
            tool: "codex".to_owned(),
            transaction: transaction.to_owned(),
            roots: roots
                .iter()
                .map(|root| RootTransition {
                    root: root.clone(),
                    stage: root.path.join("stage"),
                    recovery: root.path.join("recovery"),
                    original: Some(Identity {
                        device: 1,
                        inode: 2,
                    }),
                    staged: None,
                    published: None,
                    carried_tools: BTreeSet::new(),
                })
                .collect(),
            phase: Phase::Committed,
            retired_identity: row.clone(),
            retired_tools: std::collections::BTreeMap::from([(
                "codex".to_owned(),
                "codex".to_owned(),
            )]),
        };
        let base = receipt_path(&app, &instance.id, "codex").unwrap();
        archive_receipt(&base, &make("txn-other", &other)).unwrap();
        archive_receipt(&base, &make("txn-current", &current)).unwrap();

        assert_eq!(
            retired_receipt(&app, &instance.id, "codex", &row, &current)
                .unwrap()
                .map(|receipt| receipt.transaction),
            Some("txn-current".to_owned()),
            "the receipt bound to the current roots is selected"
        );
        assert_eq!(
            retired_receipt(&app, &instance.id, "codex", &row, &other)
                .unwrap()
                .map(|receipt| receipt.transaction),
            Some("txn-other".to_owned()),
            "each roots set selects only its own archive"
        );
        let old_binding = crate::session::ConversationBinding {
            session_id: "old-native-context".into(),
            execution: Some(crate::session::ExecutionBinding {
                agent: "codex".into(),
                stores: vec![current[0].path.clone()],
                configuration: Vec::new(),
                exported_default_store: false,
                cwd: temporary.path().to_path_buf(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
            }),
            provenance: crate::session::ConversationProvenance::Observed,
            transcript_path: None,
        };
        let mut receipt = make("txn-bound", &current);
        receipt.retired_identity["agent_session_binding"] =
            serde_json::to_value(&old_binding).unwrap();
        let mut removed = receipt.retired_identity.clone();
        reset_row(&mut removed, &receipt).unwrap();
        assert!(removed.get("agent_session_id").is_none());
        assert!(removed.get("agent_session_binding").is_none());
        assert_eq!(removed["resume_intent"]["kind"], "Cleared");
        let notice: SandboxContentReset =
            serde_json::from_value(removed["sandbox_content_resets"][0].clone()).unwrap();
        assert_eq!(notice.retired_terminal_binding.as_ref(), Some(&old_binding));

        let mut newer = old_binding.clone();
        newer.execution.as_mut().unwrap().stores = vec![temporary.path().join("new-store")];
        let mut rebound = receipt.retired_identity.clone();
        rebound["agent_session_binding"] = serde_json::to_value(&newer).unwrap();
        reset_row(&mut rebound, &receipt).unwrap();
        assert_eq!(rebound["agent_session_id"], "old-native-context");
        assert_eq!(
            rebound["agent_session_binding"],
            serde_json::to_value(&newer).unwrap()
        );

        for (binding, must_clear) in [(old_binding.clone(), true), (newer.clone(), false)] {
            let mut terminal = instance.clone();
            terminal.tool = "codex".into();
            terminal.agent_session_id = Some("old-native-context".into());
            terminal.agent_session_binding = Some(binding.clone());
            terminal.sandbox_content_resets.push(notice.clone());
            claim_context_reset(&mut terminal, Some("codex"), NativeContextView::Terminal, 8);
            if must_clear {
                assert!(terminal.agent_session_id.is_none());
                assert!(terminal.agent_session_binding.is_none());
                assert!(matches!(
                    terminal.resume_intent,
                    crate::session::ResumeIntent::Cleared
                ));
            } else {
                assert_eq!(
                    terminal.agent_session_id.as_deref(),
                    Some("old-native-context")
                );
                assert_eq!(terminal.agent_session_binding, Some(binding));
            }
        }

        let mut configuration_only = receipt.clone();
        configuration_only.roots[0].original = None;
        let mut preserved = receipt.retired_identity.clone();
        preserved["resume_intent"] = serde_json::json!({"kind":"Use","value":"old-native-context"});
        preserved["resume_binding"] = serde_json::to_value(&old_binding).unwrap();
        preserved["prior_tool_session_ids"] = serde_json::json!({
            "claude": {"agent_session_id": "parked", "agent_session_binding": {
                "session_id": "parked", "execution": null, "provenance": "unknown"
            }}
        });
        let original = preserved.clone();
        reset_row(&mut preserved, &configuration_only).unwrap();
        for field in [
            "agent_session_id",
            "agent_session_binding",
            "resume_intent",
            "resume_binding",
            "prior_tool_session_ids",
        ] {
            assert_eq!(
                preserved[field], original[field],
                "configuration-only seed changed {field}"
            );
        }
    }

    /// A live mount that reaches the recovery namespace defers retention, and
    /// the caller has to be able to tell that from "nothing to retain", or the
    /// deferred root would never be retried.
    #[test]
    #[serial_test::serial]
    fn retention_is_deferred_while_a_mount_reaches_the_recovery_namespace() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut row = serde_json::to_value(crate::session::Instance::new(
            "codex",
            project.to_str().unwrap(),
        ))
        .unwrap();
        row["sandbox_info"] = serde_json::json!({ "enabled": true });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        let host = home.join(".codex");
        let source = host.join("sandbox");
        fs::create_dir_all(source.join("one")).unwrap();
        fs::write(source.join("one/own"), b"own").unwrap();

        assert!(matches!(
            retain_legacy_original_with(&source, &host, &|_| Ok(vec![home.clone()])).unwrap(),
            Retained::Deferred
        ));
        assert_eq!(fs::read(source.join("one/own")).unwrap(), b"own");
        assert!(!recovery_root(&host).unwrap().exists());
        assert!(matches!(
            retain_legacy_original_with(&source, &host, &|_| Ok(vec![temporary
                .path()
                .join("elsewhere")]))
            .unwrap(),
            Retained::Original(_)
        ));
        assert!(!source.exists());
    }

    /// A mount that reaches the recovery namespace is refused, but refusing it
    /// must leave the session pending rather than fail the pass: one such mount
    /// cannot be allowed to stop every other session from moving either.
    #[test]
    #[serial_test::serial]
    fn an_exposed_recovery_namespace_leaves_the_sandbox_pending() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        // A project inside HOME keeps the row's own mount clear of the recovery
        // namespace, so only the injected source can expose it.
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("codex", project.to_str().unwrap());
        instance.tool = "codex".to_owned();
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(
            root.join("sessions/original.jsonl"),
            b"PRIVATE_ORIGINAL_CONTEXT",
        )
        .unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let registry = app.join("sessions.json");
        fs::write(&registry, serde_json::to_vec(&vec![row]).unwrap()).unwrap();

        let targets = migration_targets(&app, &roots).unwrap();
        let exposing = |_: &str| Ok(vec![home.clone()]);
        let elsewhere = |_: &str| Ok(vec![temporary.path().join("elsewhere")]);
        assert!(recovery_exposure(&app, &targets, &elsewhere)
            .unwrap()
            .is_none());
        assert!(recovery_exposure(&app, &targets, &exposing)
            .unwrap()
            .is_some());
        // A probe that cannot answer is not proof of privacy, and it defers
        // rather than failing every other session's move with it.
        assert!(recovery_exposure(&app, &targets, &|_: &str| anyhow::bail!(
            "runtime unavailable"
        ))
        .unwrap()
        .is_some());
        assert!(
            !migrate_target(
                &app,
                &home,
                (&registry, &instance.id, "codex"),
                &|_| Ok(false),
                &|_| Ok(true),
                &exposing,
            )
            .unwrap(),
            "an exposed mount defers this sandbox instead of failing the pass"
        );
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        assert!(!roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert!(!receipt_path(&app, &instance.id, "codex").unwrap().exists());
    }

    /// One store can be resolved by more than one row: the same instance id in
    /// another profile, or a row restored after the transaction committed. Every
    /// such row has to stop resuming the context that transaction retired.
    #[test]
    #[serial_test::serial]
    fn every_row_resolving_a_moved_store_records_the_retired_context() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("pi", project.to_str().unwrap());
        instance.tool = "pi".to_owned();
        instance.agent_session_id = Some("old-native-context".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "pi",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("agent/sessions")).unwrap();
        fs::write(
            root.join("agent/sessions/original.jsonl"),
            b"PRIVATE_ORIGINAL_CONTEXT",
        )
        .unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let main = app.join("sessions.json");
        fs::write(&main, serde_json::to_vec(&vec![row.clone()]).unwrap()).unwrap();
        let side = app.join("profiles/side/sessions.json");
        fs::create_dir_all(side.parent().unwrap()).unwrap();
        fs::write(&side, serde_json::to_vec(&vec![row]).unwrap()).unwrap();

        for registry in [&main, &side] {
            assert!(
                migrate_target(
                    &app,
                    &home,
                    (registry, &instance.id, "pi"),
                    &|_| Ok(false),
                    &|_| Ok(true),
                    &|_| Ok(Vec::new()),
                )
                .unwrap(),
                "each row of a moved store completes"
            );
        }
        for registry in [&main, &side] {
            let rows: Value = serde_json::from_slice(&fs::read(registry).unwrap()).unwrap();
            let row = &rows[0];
            assert_eq!(
                row.get("sandbox_content_policy").and_then(Value::as_u64),
                Some(u64::from(CONTENT_POLICY)),
                "{} was not reset: {row}",
                registry.display()
            );
            assert!(
                row.get("agent_session_id").is_none(),
                "{} still resumes the retired context: {row}",
                registry.display()
            );
        }
    }
    #[test]
    #[serial_test::serial]
    fn codex_private_sessions_survive_isolation_without_host_history() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("codex", project.to_str().unwrap());
        instance.tool = "codex".to_owned();
        instance.agent_session_id = Some("private-codex-session".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0];
        fs::create_dir_all(root.path.join("sessions")).unwrap();
        fs::write(root.path.join("sessions/private.jsonl"), b"PRIVATE_HISTORY").unwrap();
        fs::create_dir_all(root.host.join("sessions")).unwrap();
        fs::write(root.host.join("sessions/host.jsonl"), b"HOST_HISTORY").unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({"enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"});
        let registry = app.join("sessions.json");
        fs::write(&registry, serde_json::to_vec(&vec![row]).unwrap()).unwrap();

        assert!(migrate_target(
            &app,
            &home,
            (&registry, &instance.id, "codex"),
            &|_| Ok(false),
            &|_| Ok(true),
            &|_| Ok(Vec::new()),
        )
        .unwrap());
        assert_eq!(
            fs::read(root.path.join("sessions/private.jsonl")).unwrap(),
            b"PRIVATE_HISTORY"
        );
        assert!(!root.path.join("sessions/host.jsonl").exists());
        let rows: Value = serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
        assert_eq!(rows[0]["agent_session_id"], "private-codex-session");
    }

    #[test]
    #[serial_test::serial]
    fn opencode_carries_only_its_native_database_not_a_host_copyable_backup() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("opencode", project.to_str().unwrap());
        instance.tool = "opencode".to_owned();
        instance.agent_session_id = Some("private-opencode-session".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "opencode",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = roots
            .iter()
            .find(|root| {
                root.roles
                    .iter()
                    .any(|role| role == ".local/share/opencode")
            })
            .unwrap();
        fs::create_dir_all(&root.path).unwrap();
        fs::create_dir_all(&root.host).unwrap();
        let database = rusqlite::Connection::open(root.path.join("opencode.db")).unwrap();
        database
            .execute_batch(
                "CREATE TABLE history(value TEXT); INSERT INTO history VALUES ('private');",
            )
            .unwrap();
        drop(database);
        fs::write(
            root.path.join("opencode.db.backup"),
            b"POSSIBLY_HOST_COPIED",
        )
        .unwrap();
        let host_db = rusqlite::Connection::open(root.host.join("opencode.db")).unwrap();
        host_db
            .execute_batch("CREATE TABLE history(value TEXT); INSERT INTO history VALUES ('host');")
            .unwrap();
        drop(host_db);
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({"enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"});
        let registry = app.join("sessions.json");
        fs::write(&registry, serde_json::to_vec(&vec![row]).unwrap()).unwrap();

        assert!(migrate_target(
            &app,
            &home,
            (&registry, &instance.id, "opencode"),
            &|_| Ok(false),
            &|_| Ok(true),
            &|_| Ok(Vec::new()),
        )
        .unwrap());
        let carried = rusqlite::Connection::open(root.path.join("opencode.db")).unwrap();
        let value: String = carried
            .query_row("SELECT value FROM history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "private");
        assert!(!root.path.join("opencode.db.backup").exists());
        let rows: Value = serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
        assert_eq!(rows[0]["agent_session_id"], "private-opencode-session");
    }

    /// A retired store keeps the resume the old denylist already held
    /// sandbox-only: Claude `projects/` crosses into the fresh store and the row
    /// keeps its session id, while host-importable `history.jsonl` is still
    /// isolated and a link out of `projects/` never smuggles it back.
    #[test]
    #[serial_test::serial]
    fn a_retired_store_carries_its_own_resume_and_keeps_session_ids() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("claude", project.to_str().unwrap());
        instance.tool = "claude".to_owned();
        instance.agent_session_id = Some("kept-native-context".to_owned());
        instance.acp_session_id = Some("kept-structured-context".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "claude",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("projects/proj")).unwrap();
        fs::write(root.join("projects/proj/session.jsonl"), b"RESUME_STATE").unwrap();
        fs::hard_link(
            root.join("projects/proj/session.jsonl"),
            root.join("projects/proj/backup.jsonl"),
        )
        .unwrap();
        fs::write(root.join("history.jsonl"), b"HOST_IMPORTABLE_HISTORY").unwrap();
        fs::write(root.join("settings.json"), b"{}").unwrap();
        // A link out of the carried tree must not smuggle the retired history back.
        std::os::unix::fs::symlink("../history.jsonl", root.join("projects/leak")).unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let registry = crate::session::get_profile_dir("default")
            .unwrap()
            .join("sessions.json");
        fs::write(&registry, serde_json::to_vec(&vec![row]).unwrap()).unwrap();

        assert!(
            migrate_target(
                &app,
                &home,
                (&registry, &instance.id, "claude"),
                &|_| Ok(false),
                &|_| Ok(true),
                &|_| Ok(Vec::new()),
            )
            .unwrap(),
            "the store migrates"
        );
        assert_eq!(
            fs::read(root.join("projects/proj/session.jsonl")).unwrap(),
            b"RESUME_STATE",
            "the sandbox's own resume state is carried into the fresh store"
        );
        assert_eq!(
            fs::read(root.join("projects/proj/backup.jsonl")).unwrap(),
            b"RESUME_STATE",
            "an internal hardlink must keep both names and their resume bytes"
        );
        assert!(
            !root.join("history.jsonl").exists(),
            "host-importable history must stay isolated"
        );
        assert!(
            !root.join("projects/leak").exists(),
            "a link out of projects must not carry the retired history back"
        );
        let rows: Value = serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
        assert_eq!(
            rows[0].get("agent_session_id").and_then(Value::as_str),
            Some("kept-native-context"),
            "a carried resume must keep its session id: {}",
            rows[0]
        );
        let continuation = prepare_acp_context(
            "default",
            &instance.id,
            Some("claude"),
            1,
            AcpContextUse::Launch,
            crate::acp::supervisor::SandboxContinuation::Persisted,
        )
        .unwrap();
        assert_eq!(
            continuation.stored_session_id.as_deref(),
            Some("kept-structured-context"),
            "the native-backed ACP adapter must load its carried conversation"
        );
        for other_agent in [None, Some("external-acp")] {
            let outside = prepare_acp_context(
                "default",
                &instance.id,
                other_agent,
                2,
                AcpContextUse::Launch,
                crate::acp::supervisor::SandboxContinuation::Persisted,
            )
            .unwrap();
            assert!(
                outside.stored_session_id.is_none(),
                "an unproven adapter cannot load the retired ACP ID"
            );
        }
        let native_again = prepare_acp_context(
            "default",
            &instance.id,
            Some("claude"),
            3,
            AcpContextUse::Launch,
            crate::acp::supervisor::SandboxContinuation::Persisted,
        )
        .unwrap();
        assert_eq!(
            native_again.stored_session_id.as_deref(),
            Some("kept-structured-context")
        );
    }

    #[test]
    #[serial_test::serial]
    fn linked_conversation_directories_do_not_block_content_migration() {
        use std::os::unix::fs::symlink;

        for (tool, role) in [
            ("gemini", ".gemini"),
            ("kimi", ".kimi-code"),
            ("prime-agent", ".prime/agent"),
        ] {
            // (internal link, Kimi index is a symlink loop)
            for (internal, index_loop) in [(false, false), (true, false), (false, true)] {
                if index_loop && tool != "kimi" {
                    continue;
                }
                let temporary = tempfile::tempdir().unwrap();
                let _environment =
                    crate::session::test_support::isolate_app_dir_at(temporary.path());
                let home = dirs::home_dir().unwrap();
                let app = crate::session::get_app_dir().unwrap();
                let project = temporary.path().join("project");
                fs::create_dir_all(&project).unwrap();
                let mut instance = crate::session::Instance::new(tool, project.to_str().unwrap());
                instance.tool = tool.into();
                instance.agent_session_id = Some("own-context".into());
                let cwd = instance.container_workdir();
                let roots = container_config::sandbox_content_roots(
                    tool,
                    None,
                    &crate::session::Config::default().session,
                    &home,
                    &instance.id,
                )
                .unwrap();
                let root = roots
                    .iter()
                    .find(|root| root.roles.iter().any(|name| name == role))
                    .unwrap();
                let linked = if tool == "gemini" {
                    Path::new("tmp")
                        .join(crate::session::capture::project_hash(&cwd))
                        .join("chats")
                } else {
                    PathBuf::from("sessions")
                };
                fs::create_dir_all(root.path.join(&linked).parent().unwrap()).unwrap();
                let foreign = if internal {
                    root.path.join("unrelated-native-state")
                } else {
                    temporary.path().join("foreign")
                };
                fs::create_dir_all(foreign.join("own")).unwrap();
                fs::write(foreign.join("own/session.jsonl"), b"FOREIGN_HISTORY").unwrap();
                if index_loop {
                    fs::create_dir_all(root.path.join("sessions/own")).unwrap();
                    fs::write(root.path.join("sessions/own/session.jsonl"), b"OWN").unwrap();
                    symlink("session_index.jsonl", root.path.join("session_index.jsonl")).unwrap();
                } else {
                    symlink(&foreign, root.path.join(&linked)).unwrap();
                }
                if tool == "kimi" && !index_loop {
                    fs::write(
                        root.path.join("session_index.jsonl"),
                        format!(
                            "{}\n",
                            serde_json::json!({
                                "sessionId": "own-context",
                                "sessionDir": "/root/.kimi-code/sessions/own",
                                "workDir": cwd
                            })
                        ),
                    )
                    .unwrap();
                }
                let mut row = serde_json::to_value(&instance).unwrap();
                row["sandbox_info"] = serde_json::json!({
                    "enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"
                });
                let registry = crate::session::get_profile_dir("default")
                    .unwrap()
                    .join("sessions.json");
                fs::write(&registry, serde_json::to_vec(&vec![row]).unwrap()).unwrap();

                assert!(
                    migrate_target(
                        &app,
                        &home,
                        (&registry, &instance.id, tool),
                        &|_| Ok(false),
                        &|_| Ok(true),
                        &|_| Ok(Vec::new()),
                    )
                    .unwrap(),
                    "{tool} internal={internal} index_loop={index_loop}"
                );
                assert!(
                    !root.path.join(&linked).exists(),
                    "{tool} imported a linked directory"
                );
                let recovery = fs::read_dir(recovery_root(&root.host).unwrap())
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path()
                    .join("0/original");
                let retained = if index_loop {
                    PathBuf::from("session_index.jsonl")
                } else {
                    linked.clone()
                };
                assert!(fs::symlink_metadata(recovery.join(retained))
                    .unwrap()
                    .file_type()
                    .is_symlink());
                if index_loop {
                    // Unattributable without its index, the session stays whole in recovery.
                    assert_eq!(
                        fs::read(recovery.join("sessions/own/session.jsonl")).unwrap(),
                        b"OWN"
                    );
                }
                assert!(read_row(&registry, &instance.id)
                    .unwrap()
                    .unwrap()
                    .get("agent_session_id")
                    .is_none_or(Value::is_null));
            }
        }
    }
    #[test]
    #[serial_test::serial]
    fn a_looped_carried_root_does_not_stop_migration_of_other_rows() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut rows = Vec::new();
        let mut instances = Vec::new();
        for title in ["looped", "fresh"] {
            let mut instance = crate::session::Instance::new(title, project.to_str().unwrap());
            instance.tool = "claude".into();
            let mut row = serde_json::to_value(&instance).unwrap();
            row["sandbox_info"] = serde_json::json!({
                "enabled": true, "image": "img", "container_name": format!("aoe-sandbox-{title}")
            });
            rows.push(row);
            instances.push(instance);
        }
        let roots = container_config::sandbox_content_roots(
            "claude",
            None,
            &crate::session::Config::default().session,
            &home,
            &instances[0].id,
        )
        .unwrap();
        let looped = roots
            .iter()
            .find(|root| root.roles.iter().any(|name| name == ".claude"))
            .unwrap();
        fs::create_dir_all(&looped.path).unwrap();
        fs::write(looped.path.join("settings.json"), b"{}").unwrap();
        fs::write(looped.path.join("CLAUDE.md"), b"AUTHORED").unwrap();
        symlink("projects", looped.path.join("projects")).unwrap();
        let registry = crate::session::get_profile_dir("default")
            .unwrap()
            .join("sessions.json");
        fs::write(&registry, serde_json::to_vec(&rows).unwrap()).unwrap();

        reconcile_in(
            &app,
            &home,
            None,
            true,
            &|_| Ok(false),
            &|_| Ok(true),
            &|_| Ok(Vec::new()),
        )
        .unwrap();

        for instance in &instances {
            let roots = container_config::sandbox_content_roots(
                "claude",
                None,
                &crate::session::Config::default().session,
                &home,
                &instance.id,
            )
            .unwrap();
            assert!(
                roots_ready(&app, &instance.id, "claude", &roots).unwrap(),
                "{} was left pending",
                instance.title
            );
        }
        assert!(fs::symlink_metadata(looped.path.join("projects")).is_err());
        let recovery = fs::read_dir(recovery_root(&looped.host).unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("0/original");
        assert!(fs::symlink_metadata(recovery.join("projects"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    #[serial_test::serial]
    fn retired_gemini_kimi_and_prime_keep_only_their_own_native_conversation() {
        use crate::acp::supervisor::SandboxContinuation;

        for (tool, role) in [
            ("gemini", ".gemini"),
            ("kimi", ".kimi-code"),
            ("prime-agent", ".prime/agent"),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
            let home = dirs::home_dir().unwrap();
            let app = crate::session::get_app_dir().unwrap();
            let project = temporary.path().join("project");
            fs::create_dir_all(&project).unwrap();
            let mut instance = crate::session::Instance::new(tool, project.to_str().unwrap());
            instance.tool = tool.to_owned();
            instance.agent_session_id = Some("own-context".into());
            instance.acp_session_id = Some("own-acp-context".into());
            let cwd = instance.container_workdir();
            let roots = container_config::sandbox_content_roots(
                tool,
                None,
                &crate::session::Config::default().session,
                &home,
                &instance.id,
            )
            .unwrap();
            let root = roots
                .iter()
                .find(|root| root.roles.iter().any(|name| name == role))
                .expect("a registered native agent has its sandbox content mount");
            let (own, peer) =
                match tool {
                    "gemini" => {
                        let chats = Path::new("tmp")
                            .join(crate::session::capture::project_hash(&cwd))
                            .join("chats");
                        fs::create_dir_all(root.path.join(&chats)).unwrap();
                        let own = chats.join("session-own.json");
                        let peer = chats.join("session-peer.json");
                        for (path, id) in [(&own, "own-context"), (&peer, "peer-context")] {
                            fs::write(
                                root.path.join(path),
                                serde_json::to_vec(&serde_json::json!({
                                    "sessionId": id,
                                    "projectHash": crate::session::capture::project_hash(&cwd),
                                    "messages": [id]
                                }))
                                .unwrap(),
                            )
                            .unwrap();
                        }
                        (own, peer)
                    }
                    "kimi" => {
                        let own = PathBuf::from("sessions/own-dir/content");
                        let peer = PathBuf::from("sessions/peer-dir/content");
                        for (relative, content) in
                            [(&own, &b"OWN_HISTORY"[..]), (&peer, &b"PEER_HISTORY"[..])]
                        {
                            fs::create_dir_all(root.path.join(relative).parent().unwrap()).unwrap();
                            fs::write(root.path.join(relative), content).unwrap();
                        }
                        let records = [("own-context", "own-dir"), ("peer-context", "peer-dir")]
                            .map(|(id, dir)| {
                                serde_json::json!({
                        "sessionId": id, "sessionDir": format!("/root/.kimi-code/sessions/{dir}"),
                        "workDir": cwd
                    }).to_string()
                            })
                            .join("\n");
                        fs::write(
                            root.path.join("session_index.jsonl"),
                            format!("{records}\n"),
                        )
                        .unwrap();
                        (own, peer)
                    }
                    "prime-agent" => {
                        let own = PathBuf::from("sessions/first.jsonl");
                        let peer = PathBuf::from("sessions/second.jsonl");
                        fs::create_dir_all(root.path.join("sessions")).unwrap();
                        for (relative, id) in [(&own, "own-context"), (&peer, "peer-context")] {
                            fs::write(root.path.join(relative), format!(
                            "{}\n{{\"message\":\"{id}\"}}\n",
                            serde_json::json!({"type":"session","rlmDepth":0,"id":id,"cwd":cwd})
                        )).unwrap();
                        }
                        (own, peer)
                    }
                    _ => unreachable!(),
                };
            let peer_bytes = fs::read(root.path.join(&peer)).unwrap();
            let own_bytes = fs::read(root.path.join(&own)).unwrap();
            fs::create_dir_all(root.host.join(&peer).parent().unwrap()).unwrap();
            fs::write(root.host.join(&peer), b"HOST_HISTORY").unwrap();

            let mut row = serde_json::to_value(&instance).unwrap();
            row["sandbox_info"] = serde_json::json!({
                "enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"
            });
            let registry = crate::session::get_profile_dir("default")
                .unwrap()
                .join("sessions.json");
            fs::write(&registry, serde_json::to_vec(&vec![row]).unwrap()).unwrap();
            assert!(
                migrate_target(
                    &app,
                    &home,
                    (&registry, &instance.id, tool),
                    &|_| Ok(false),
                    &|_| Ok(true),
                    &|_| Ok(Vec::new()),
                )
                .unwrap(),
                "{tool} migration must publish"
            );
            assert_eq!(
                fs::read(root.path.join(&own)).unwrap(),
                own_bytes,
                "{tool} own transcript"
            );
            assert!(
                !root.path.join(&peer).exists(),
                "{tool} copied a peer transcript"
            );
            if tool == "kimi" {
                let index = fs::read_to_string(root.path.join("session_index.jsonl")).unwrap();
                assert!(index.contains("own-context"));
                assert!(!index.contains("peer-context"));
            }
            let recovery = fs::read_dir(recovery_root(&root.host).unwrap())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path()
                .join("0/original");
            assert_eq!(fs::read(recovery.join(&peer)).unwrap(), peer_bytes);
            let stored = read_row(&registry, &instance.id).unwrap().unwrap();
            assert_eq!(
                stored["agent_session_id"], "own-context",
                "{tool} lost native resume"
            );
            let continuation = prepare_acp_context(
                "default",
                &instance.id,
                Some(tool),
                1,
                AcpContextUse::Launch,
                SandboxContinuation::Persisted,
            )
            .unwrap();
            assert_eq!(
                continuation.stored_session_id.as_deref(),
                None,
                "{tool} has no proof that the ACP conversation was carried"
            );
            assert!(
                continuation.notice.is_some(),
                "{tool} must report the ACP reset"
            );
            let stored = read_row(&registry, &instance.id).unwrap().unwrap();
            assert!(stored.get("acp_session_id").is_none_or(Value::is_null));
        }
    }

    #[test]
    #[serial_test::serial]
    fn ambiguous_or_deleted_native_conversation_is_retained_only_in_recovery() {
        use crate::acp::supervisor::SandboxContinuation;

        for (tool, off_mount) in [
            ("gemini", false),
            ("kimi", false),
            ("kimi", true),
            ("prime-agent", false),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
            let app = crate::session::get_app_dir().unwrap();
            let home = dirs::home_dir().unwrap();
            let project = temporary.path().join("project");
            fs::create_dir(&project).unwrap();
            let mut instance = crate::session::Instance::new(tool, project.to_str().unwrap());
            instance.tool = tool.to_owned();
            instance.agent_session_id = Some("own-context".into());
            instance.acp_session_id = Some("own-acp-context".into());
            let cwd = instance.container_workdir();
            let roots = container_config::sandbox_content_roots(
                tool,
                None,
                &crate::session::Config::default().session,
                &home,
                &instance.id,
            )
            .unwrap();
            let role = match tool {
                "gemini" => ".gemini",
                "kimi" => ".kimi-code",
                _ => ".prime/agent",
            };
            let root = roots
                .iter()
                .find(|root| root.roles.iter().any(|name| name == role))
                .unwrap();
            let witness =
                match tool {
                    "gemini" => {
                        let chats = Path::new("tmp")
                            .join(crate::session::capture::project_hash(&cwd))
                            .join("chats");
                        fs::create_dir_all(root.path.join(&chats)).unwrap();
                        for name in ["session-first.json", "session-second.json"] {
                            fs::write(
                                root.path.join(&chats).join(name),
                                serde_json::to_vec(&serde_json::json!({
                                    "sessionId":"own-context",
                                    "projectHash":crate::session::capture::project_hash(&cwd)
                                }))
                                .unwrap(),
                            )
                            .unwrap();
                        }
                        chats.join("session-first.json")
                    }
                    "kimi" => {
                        let witness = PathBuf::from("sessions/owner/content");
                        fs::create_dir_all(root.path.join("sessions/owner")).unwrap();
                        fs::write(root.path.join(&witness), b"OLD_SESSION").unwrap();
                        let mut index = format!(
                            "{}\n",
                            serde_json::json!({
                                "sessionId":"own-context",
                                "sessionDir": if off_mount {
                                    "/other-store/sessions/owner"
                                } else {
                                    "/root/.kimi-code/sessions/owner"
                                },
                                "workDir":cwd
                            })
                        );
                        if !off_mount {
                            index.push_str(&format!(
                                "{}\n",
                                serde_json::json!({
                                    "sessionId":"own-context", "deleted":true
                                })
                            ));
                        }
                        fs::write(root.path.join("session_index.jsonl"), index).unwrap();
                        witness
                    }
                    _ => {
                        let sessions = Path::new("sessions");
                        fs::create_dir_all(root.path.join(sessions)).unwrap();
                        for name in ["first.jsonl", "second.jsonl"] {
                            fs::write(root.path.join(sessions).join(name), format!(
                            "{}\n", serde_json::json!({
                                "type":"session", "rlmDepth":0, "id":"own-context", "cwd":cwd
                            })
                        )).unwrap();
                        }
                        sessions.join("first.jsonl")
                    }
                };
            let original = fs::read(root.path.join(&witness)).unwrap();
            let mut row = serde_json::to_value(&instance).unwrap();
            row["sandbox_info"] = serde_json::json!({
                "enabled":true,"image":"img","container_name":"aoe-sandbox-fixture"
            });
            let registry = crate::session::get_profile_dir("default")
                .unwrap()
                .join("sessions.json");
            fs::write(&registry, serde_json::to_vec(&vec![row]).unwrap()).unwrap();
            assert!(migrate_target(
                &app,
                &home,
                (&registry, &instance.id, tool),
                &|_| Ok(false),
                &|_| Ok(true),
                &|_| Ok(Vec::new()),
            )
            .unwrap());
            assert!(
                !root.path.join(&witness).exists(),
                "{tool} carried ambiguous history"
            );
            let recovery = fs::read_dir(recovery_root(&root.host).unwrap())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path()
                .join("0/original");
            assert_eq!(fs::read(recovery.join(&witness)).unwrap(), original);
            let stored = read_row(&registry, &instance.id).unwrap().unwrap();
            assert!(
                stored.get("agent_session_id").is_none(),
                "{tool} retained an unproven ID"
            );
            let context = prepare_acp_context(
                "default",
                &instance.id,
                Some(tool),
                1,
                AcpContextUse::Launch,
                SandboxContinuation::Persisted,
            )
            .unwrap();
            assert!(
                context.stored_session_id.is_none(),
                "{tool} reused an unproven ACP ID"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn unreadable_resume_state_leaves_the_store_pending_until_retry() {
        use std::os::unix::fs::PermissionsExt;

        for (tool, role, resume_path) in [
            ("claude", ".claude", "projects/proj/session.jsonl"),
            ("opencode", ".local/share/opencode", "opencode.db"),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
            let home = dirs::home_dir().unwrap();
            let app = crate::session::get_app_dir().unwrap();
            let project = temporary.path().join("project");
            fs::create_dir_all(&project).unwrap();
            let mut instance = crate::session::Instance::new(tool, project.to_str().unwrap());
            instance.tool = tool.to_owned();
            instance.agent_session_id = Some("kept-native-context".to_owned());
            instance.acp_session_id = Some("kept-structured-context".to_owned());
            let config = crate::session::Config::default();
            let roots = container_config::sandbox_content_roots(
                tool,
                None,
                &config.session,
                &home,
                &instance.id,
            )
            .unwrap();
            let root = &roots
                .iter()
                .find(|root| root.roles.iter().any(|candidate| candidate == role))
                .unwrap()
                .path;
            let resume = root.join(resume_path);
            fs::create_dir_all(resume.parent().unwrap()).unwrap();
            fs::write(&resume, b"RESUME_STATE").unwrap();
            let original_identity = identity(root).unwrap().unwrap();
            let mut row = serde_json::to_value(&instance).unwrap();
            row["sandbox_info"] = serde_json::json!({
                "enabled": true,
                "image": "img",
                "container_name": "aoe-sandbox-fixture",
            });
            let registry = app.join("sessions.json");
            let original_registry = serde_json::to_vec(&vec![row]).unwrap();
            fs::write(&registry, &original_registry).unwrap();

            // Keep a handle so permissions can be restored even if the store moves.
            let resume_file = fs::File::open(&resume).unwrap();
            let permissions = resume_file.metadata().unwrap().permissions();
            resume_file
                .set_permissions(fs::Permissions::from_mode(0o000))
                .unwrap();
            match fs::File::open(&resume) {
                Ok(_) => {
                    resume_file.set_permissions(permissions).unwrap();
                    eprintln!(
                        "skipping unreadable resume regression: privileges permit reading mode 000"
                    );
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
                Err(error) => {
                    resume_file.set_permissions(permissions).unwrap();
                    panic!("expected PermissionDenied opening {tool} resume state: {error}");
                }
            }

            let result = migrate_target(
                &app,
                &home,
                (&registry, &instance.id, tool),
                &|_| Ok(false),
                &|_| Ok(true),
                &|_| Ok(Vec::new()),
            );
            resume_file.set_permissions(permissions).unwrap();
            assert!(result.is_err(), "{tool}: unreadable resume state must fail");
            assert_eq!(
                identity(root).unwrap(),
                Some(original_identity),
                "{tool}: the original store must remain in place"
            );
            assert_eq!(fs::read(&resume).unwrap(), b"RESUME_STATE");
            assert_eq!(fs::read(&registry).unwrap(), original_registry);
            assert!(!roots_ready(&app, &instance.id, tool, &roots).unwrap());

            assert!(
                migrate_target(
                    &app,
                    &home,
                    (&registry, &instance.id, tool),
                    &|_| Ok(false),
                    &|_| Ok(true),
                    &|_| Ok(Vec::new()),
                )
                .unwrap(),
                "{tool}: restoring access must allow the complete retry"
            );
            assert!(roots_ready(&app, &instance.id, tool, &roots).unwrap());
            assert_eq!(fs::read(&resume).unwrap(), b"RESUME_STATE");
            let row = read_row(&registry, &instance.id).unwrap().unwrap();
            assert_eq!(
                row.get("agent_session_id").and_then(Value::as_str),
                Some("kept-native-context"),
                "{tool}: retry must preserve the native session id"
            );
            assert_eq!(
                row.get("acp_session_id").and_then(Value::as_str),
                Some("kept-structured-context"),
                "{tool}: retry must preserve the structured session id"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn acp_continuation_intent_controls_only_the_requested_history_lane() {
        use crate::acp::supervisor::SandboxContinuation;

        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("claude", project.to_str().unwrap());
        instance.tool = "claude".into();
        instance.agent_session_id = Some("terminal-context".into());
        instance.acp_session_id = Some("persisted-acp-context".into());
        instance.fork_pending = Some("persisted-fork".into());
        instance.import_pending = Some(true);
        instance.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".into(),
            container_name: "aoe-sandbox-continuation".into(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });
        drop(admit_fresh_instance(&instance).unwrap());
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        storage
            .update(|instances, _| {
                *instances = vec![instance.clone()];
                Ok(())
            })
            .unwrap();

        let imported = prepare_acp_context(
            "default",
            &instance.id,
            Some("claude"),
            7,
            AcpContextUse::Launch,
            SandboxContinuation::ImportTerminal,
        )
        .unwrap();
        assert_eq!(
            imported.stored_session_id.as_deref(),
            Some("terminal-context")
        );
        assert!(imported.fork_from.is_none());
        assert!(imported.seed_history_replay);

        let fresh = prepare_acp_context(
            "default",
            &instance.id,
            Some("claude"),
            8,
            AcpContextUse::Launch,
            SandboxContinuation::Fresh,
        )
        .unwrap();
        assert!(fresh.stored_session_id.is_none());
        assert!(fresh.fork_from.is_none());
        assert!(!fresh.seed_history_replay);

        storage
            .update(|instances, _| {
                instances[0]
                    .sandbox_content_resets
                    .push(SandboxContentReset {
                        slot: "claude".into(),
                        transaction: "reset".into(),
                        tool: "claude".into(),
                        agent: "claude".into(),
                        roots: Vec::new(),
                        recovery: Vec::new(),
                        terminal: ResetLane {
                            pending: false,
                            generation: None,
                        },
                        structured: ResetLane {
                            pending: true,
                            generation: None,
                        },
                        retired_terminal: Some("terminal-context".into()),
                        retired_terminal_binding: None,
                        retired_structured: vec![
                            "persisted-acp-context".into(),
                            "persisted-fork".into(),
                        ],
                        retired_import: true,
                    });
                Ok(())
            })
            .unwrap();
        let reset_import = prepare_acp_context(
            "default",
            &instance.id,
            Some("claude"),
            9,
            AcpContextUse::Launch,
            SandboxContinuation::ImportTerminal,
        )
        .unwrap();
        assert!(reset_import.notice.is_some());
        assert!(reset_import.stored_session_id.is_none());
        assert!(reset_import.fork_from.is_none());
        assert!(!reset_import.seed_history_replay);
    }

    /// An adapter without a matching native store cannot load the retired ACP ID.
    #[test]
    #[serial_test::serial]
    fn an_adapter_without_a_native_agent_cannot_reuse_carried_acp_id() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let mut instance =
            crate::session::Instance::new("codex", temporary.path().to_str().unwrap());
        instance.tool = "codex".to_owned();
        instance.agent_session_id = Some("old-native-context".to_owned());
        instance.acp_session_id = Some("retired-adapter-context".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(
            root.join("sessions/original.jsonl"),
            b"PRIVATE_ORIGINAL_CONTEXT",
        )
        .unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let (path, mut receipt) = checked_receipt(&app, &row, "codex", &roots).unwrap();
        record_retirement(&mut receipt, &row, &home, &config).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
        publish_receipt(&mut receipt, &path).unwrap();
        reset_row(&mut row, &receipt).unwrap();
        receipt.phase = Phase::Committed;
        write_receipt(&path, &receipt).unwrap();
        certify_receipt(&app, &receipt).unwrap();
        archive_receipt(&path, &receipt).unwrap();
        fs::create_dir_all(&app).unwrap();
        let registry = crate::session::get_profile_dir("default")
            .unwrap()
            .join("sessions.json");
        fs::create_dir_all(registry.parent().unwrap()).unwrap();
        fs::write(&registry, serde_json::to_vec(&vec![row]).unwrap()).unwrap();

        assert!(
            prepare_acp_context(
                "default",
                &instance.id,
                None,
                1,
                AcpContextUse::Attach,
                crate::acp::supervisor::SandboxContinuation::Persisted,
            )
            .is_err(),
            "an adapter that names no native agent cannot attach to a moved lane"
        );
        let context = prepare_acp_context(
            "default",
            &instance.id,
            None,
            1,
            AcpContextUse::Launch,
            crate::acp::supervisor::SandboxContinuation::Persisted,
        )
        .unwrap();
        assert!(
            context.notice.is_none(),
            "carrying the native store is not a native reset"
        );
        assert!(
            context.stored_session_id.is_none(),
            "the retired adapter context is not resumed"
        );
    }

    /// A stage built while a container could still have been writing is not
    /// content to publish: the pass discards it and seeds again once the row is
    /// proven stopped, rather than certifying a possibly concurrent copy.
    #[test]
    #[serial_test::serial]
    fn a_deferred_stage_is_rebuilt_before_it_is_published() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("codex", project.to_str().unwrap());
        instance.tool = "codex".to_owned();
        instance.agent_session_id = Some("old-native-context".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(
            root.join("sessions/original.jsonl"),
            b"PRIVATE_ORIGINAL_CONTEXT",
        )
        .unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let registry = app.join("sessions.json");
        fs::write(&registry, serde_json::to_vec(&vec![row]).unwrap()).unwrap();
        let target = (registry.as_path(), instance.id.as_str(), "codex");

        assert!(
            !migrate_target(
                &app,
                &home,
                target,
                &|_| Ok(false),
                &|_| Ok(false),
                &|_| Ok(Vec::new()),
            )
            .unwrap(),
            "a container that cannot be reaped defers the row"
        );
        let stored = read_receipt(&receipt_path(&app, &instance.id, "codex").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(stored.phase, Phase::Planned, "the deferral drops the stage");
        for part in &stored.roots {
            assert!(identity(&part.stage).unwrap().is_none());
        }
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );

        // A crash between the journal write and the removal leaves a stage the
        // next pass must clean rather than publish.
        let stage = stored.roots[0].stage.clone();
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("stale"), b"STALE").unwrap();

        assert!(
            migrate_target(&app, &home, target, &|_| Ok(false), &|_| Ok(true), &|_| Ok(
                Vec::new()
            ),)
            .unwrap()
        );
        assert!(roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        assert!(
            !root.join("stale").exists(),
            "a stale stage is not published"
        );
        assert!(!stage.exists());
    }

    #[test]
    #[serial_test::serial]
    fn a_staged_receipt_is_rebuilt_from_the_latest_stopped_content() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("claude", project.to_str().unwrap());
        instance.tool = "claude".to_owned();
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "claude",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots
            .iter()
            .find(|root| root.roles.iter().any(|role| role == ".claude"))
            .unwrap()
            .path;
        let resume = root.join("projects/proj/session.jsonl");
        fs::create_dir_all(resume.parent().unwrap()).unwrap();
        fs::write(&resume, b"OLD").unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let registry = app.join("sessions.json");
        fs::write(&registry, serde_json::to_vec(&vec![row.clone()]).unwrap()).unwrap();
        let (receipt_path, mut receipt) = checked_receipt(&app, &row, "claude", &roots).unwrap();
        stage_receipt(&app, &mut receipt, &receipt_path, &home, &config, &project).unwrap();
        assert_eq!(receipt.phase, Phase::Staged);
        let staged_resume = receipt
            .roots
            .iter()
            .find(|part| part.root.path == *root)
            .unwrap()
            .stage
            .join("projects/proj/session.jsonl");
        assert_eq!(fs::read(staged_resume).unwrap(), b"OLD");

        fs::write(&resume, b"LATEST").unwrap();
        let target = (registry.as_path(), instance.id.as_str(), "claude");
        assert!(
            migrate_target(&app, &home, target, &|_| Ok(false), &|_| Ok(true), &|_| {
                Ok(Vec::new())
            })
            .unwrap()
        );
        assert_eq!(fs::read(&resume).unwrap(), b"LATEST");
    }

    #[test]
    #[serial_test::serial]
    fn a_fresh_stage_renamed_into_place_survives_deferral_and_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("codex", project.to_str().unwrap());
        instance.tool = "codex".to_owned();
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(&roots[0].host).unwrap();
        fs::write(roots[0].host.join("auth.json"), b"PORTABLE_AUTH").unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({"enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"});
        let registry = app.join("sessions.json");
        fs::write(&registry, serde_json::to_vec(&vec![row.clone()]).unwrap()).unwrap();
        let (path, mut receipt) = checked_receipt(&app, &row, "codex", &roots).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, &project).unwrap();
        assert!(receipt.roots[0].original.is_none());
        assert!(receipt.roots[0].published.is_none());
        let staged = receipt.roots[0].staged.clone();
        assert!(staged.is_some());
        fs::create_dir_all(root.parent().unwrap()).unwrap();
        fs::rename(&receipt.roots[0].stage, root).unwrap();
        let target = (registry.as_path(), instance.id.as_str(), "codex");
        assert!(!migrate_target(
            &app,
            &home,
            target,
            &|_| Ok(false),
            &|_| Ok(false),
            &|_| Ok(Vec::new())
        )
        .unwrap());
        let deferred = read_receipt(&path).unwrap().unwrap();
        assert_eq!(deferred.phase, Phase::Staged);
        assert_eq!(deferred.roots[0].staged, staged);
        assert_eq!(identity(root).unwrap(), staged);
        assert!(!roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert!(
            migrate_target(&app, &home, target, &|_| Ok(false), &|_| Ok(true), &|_| Ok(
                Vec::new()
            ))
            .unwrap()
        );
        assert!(roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert_eq!(identity(root).unwrap(), staged);
        assert_eq!(fs::read(root.join("auth.json")).unwrap(), b"PORTABLE_AUTH");
        assert!(read_receipt(&path).unwrap().is_none());
        assert!(!receipt.roots[0].recovery.exists());
        assert!(
            migrate_target(&app, &home, target, &|_| Ok(false), &|_| Ok(true), &|_| Ok(
                Vec::new()
            ))
            .unwrap()
        );
        assert_eq!(identity(root).unwrap(), staged);
    }

    /// A part renamed into place without a certificate is mid-publication: its
    /// stage is what the resume path needs, and discarding it would make every
    /// later pass refuse the store.
    #[test]
    #[serial_test::serial]
    fn a_mid_publication_journal_is_not_returned_to_planned() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let instance = crate::session::Instance::new("codex", temporary.path().to_str().unwrap());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(root.join("sessions/original.jsonl"), b"PRIVATE").unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let (path, mut receipt) = checked_receipt(&app, &row, "codex", &roots).unwrap();
        record_retirement(&mut receipt, &row, &home, &config).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();

        // As `publish_receipt` leaves it: renamed into place, not yet certified.
        let published = receipt.roots[0].staged.clone();
        receipt.roots[0].published = published;
        receipt.roots[0].original = None;
        write_receipt(&path, &receipt).unwrap();
        discard_stage(&app, &mut receipt, &path).unwrap();
        assert_eq!(
            read_receipt(&path).unwrap().unwrap().phase,
            Phase::Staged,
            "a part renamed into place keeps its stage for the resume path"
        );

        // The same part certified is a fresh seed, not mid-publication.
        certify_test_content(&app, &instance.id, root, &[]).unwrap();
        discard_stage(&app, &mut receipt, &path).unwrap();
        assert_eq!(read_receipt(&path).unwrap().unwrap().phase, Phase::Planned);
    }

    /// Death between `publish_receipt`'s rename and its journal write leaves the
    /// root holding the staged identity with nothing recorded: the stage is
    /// still what the resume path needs, so it must not be discarded.
    #[test]
    #[serial_test::serial]
    fn a_stage_renamed_into_place_is_not_discarded() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let instance = crate::session::Instance::new("codex", temporary.path().to_str().unwrap());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(root.join("sessions/original.jsonl"), b"PRIVATE").unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let (path, mut receipt) = checked_receipt(&app, &row, "codex", &roots).unwrap();
        record_retirement(&mut receipt, &row, &home, &config).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
        let stage = receipt.roots[0].stage.clone();
        // What the rename leaves behind: the root holds the seeded content and
        // the journal still says nothing about it.
        fs::remove_dir_all(root).unwrap();
        fs::rename(&stage, root).unwrap();
        assert_eq!(identity(root).unwrap(), receipt.roots[0].staged);
        assert!(receipt.roots[0].published.is_none());

        discard_stage(&app, &mut receipt, &path).unwrap();
        assert_eq!(
            read_receipt(&path).unwrap().unwrap().phase,
            Phase::Staged,
            "a rename the journal has not recorded is still mid-publication"
        );
    }

    /// The announced runner dispatches through the production wiring from
    /// schema 28, so an announced first call must certify a fresh store and
    /// retire its private context, not merely report the row as pending.
    #[test]
    #[serial_test::serial]
    fn first_announced_run_from_schema_28_completes_content_isolation() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("codex", project.to_str().unwrap());
        instance.tool = "codex".to_owned();
        instance.agent_session_id = Some("old-native-context".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(
            root.join("sessions/original.jsonl"),
            b"PRIVATE_ORIGINAL_CONTEXT",
        )
        .unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({"enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"});
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        std::fs::write(app.join(".schema_version"), "28").unwrap();

        let _probes =
            install_test_reconcile_probes(|_| Ok(false), |_| Ok(true), |_| Ok(Vec::new()));
        super::super::run_migrations_announced(None).unwrap();
        assert!(roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        let receipt = receipt_path(&app, &instance.id, "codex").unwrap();
        let archives: Vec<_> = fs::read_dir(receipt.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "complete")
            })
            .collect();
        assert_eq!(archives.len(), 1);
        let archive: Receipt = serde_json::from_slice(&fs::read(&archives[0]).unwrap()).unwrap();
        assert_eq!(archive.phase, Phase::Committed);
        assert_eq!(
            fs::read(archive.roots[0].recovery.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        assert!(
            read_receipt(&receipt_path(&app, &instance.id, "codex").unwrap())
                .unwrap()
                .is_none()
        );
        let rows: Vec<Value> =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["agent_session_id"], "old-native-context");
    }

    /// Startup never migrates content on its own: it must advance the schema,
    /// report the pending row, and leave the original store byte-identical.
    #[test]
    #[serial_test::serial]
    fn startup_from_schema_28_with_reporter_only_reports_content_isolation() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("codex", project.to_str().unwrap());
        instance.tool = "codex".to_owned();
        instance.agent_session_id = Some("old-native-context".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(
            root.join("sessions/original.jsonl"),
            b"PRIVATE_ORIGINAL_CONTEXT",
        )
        .unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({"enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"});
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        std::fs::write(app.join(".schema_version"), "28").unwrap();
        let original: Vec<Value> =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let reporter: progress::Reporter =
            std::sync::Arc::new(move |event| captured.lock().unwrap().push(event));
        let _probes =
            install_test_reconcile_probes(|_| Ok(false), |_| Ok(true), |_| Ok(Vec::new()));
        super::super::run_migrations_with(Some(reporter)).unwrap();
        assert!(
            read_receipt(&receipt_path(&app, &instance.id, "codex").unwrap())
                .unwrap()
                .is_none()
        );
        let rows: Vec<Value> =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["agent_session_id"], original[0]["agent_session_id"]);
        assert_eq!(rows[0]["agent_session_binding"]["provenance"], "unknown");
        assert!(rows[0]["agent_session_binding"]["execution"].is_null());
        assert!(!roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        let notices: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                crate::migrations::progress::Event::Notice(message) => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notices
                .iter()
                .any(|message| message.contains(&format!("sandbox {}", instance.id))),
            "startup reports the pending row: {notices:?}"
        );
    }

    /// The window's other sub-state: the retention rename has already moved the
    /// root into the recovery namespace while the stage is still there.
    /// Discarding the stage would leave every later pass refusing the store with
    /// "native content root changed before staging".
    #[test]
    #[serial_test::serial]
    fn a_root_already_retained_is_not_a_fresh_seed() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let instance = crate::session::Instance::new("codex", temporary.path().to_str().unwrap());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(root.join("sessions/original.jsonl"), b"PRIVATE").unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-fixture",
        });
        let (path, mut receipt) = checked_receipt(&app, &row, "codex", &roots).unwrap();
        record_retirement(&mut receipt, &row, &home, &config).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
        // The retention rename has run and the journal has not recorded it.
        fs::remove_dir_all(root).unwrap();
        assert!(receipt.roots[0].original.is_some());
        assert!(receipt.roots[0].published.is_none());

        discard_stage(&app, &mut receipt, &path).unwrap();
        assert_eq!(
            read_receipt(&path).unwrap().unwrap().phase,
            Phase::Staged,
            "the publish already began, so the stage stays for the resume path"
        );
    }

    #[test]
    #[serial_test::serial]
    fn hermes_migration_seeds_single_link_controls_into_an_empty_private_store() {
        use std::os::unix::fs::MetadataExt;

        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("hermes", project.to_str().unwrap());
        instance.tool = "hermes".to_owned();
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "hermes",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let source = &roots[0].host;
        let destination = &roots[0].path;
        fs::create_dir_all(source.join("workspace/meetings")).unwrap();
        fs::create_dir_all(source.join("skills/authored")).unwrap();
        let nodes = br#"{"nodes":[{"id":"remote","token":"portable"}]}"#;
        let curator = br#"{"paused":true}"#;
        fs::write(source.join("workspace/meetings/nodes.json"), nodes).unwrap();
        fs::write(source.join("skills/.curator_state"), curator).unwrap();
        fs::write(source.join("skills/authored/SKILL.md"), b"AUTHORED_SKILL").unwrap();
        assert_eq!(
            fs::metadata(source.join("workspace/meetings/nodes.json"))
                .unwrap()
                .nlink(),
            1
        );
        assert_eq!(
            fs::metadata(source.join("skills/.curator_state"))
                .unwrap()
                .nlink(),
            1
        );
        assert!(!destination.exists());
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_info"] = serde_json::json!({"enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"});
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(app.join(".schema_version"), "28").unwrap();
        let _probes =
            install_test_reconcile_probes(|_| Ok(false), |_| Ok(true), |_| Ok(Vec::new()));

        super::super::run_migrations_announced(None).unwrap();
        assert!(roots_ready(&app, &instance.id, "hermes", &roots).unwrap());
        assert_eq!(
            fs::read(destination.join("workspace/meetings/nodes.json")).unwrap(),
            nodes
        );
        assert_eq!(
            serde_json::from_slice::<Value>(
                &fs::read(destination.join("skills/.curator_state")).unwrap()
            )
            .unwrap(),
            serde_json::json!({"paused": true})
        );
        assert_eq!(
            fs::read(destination.join("skills/authored/SKILL.md")).unwrap(),
            b"AUTHORED_SKILL"
        );
        assert_eq!(
            fs::read(source.join("workspace/meetings/nodes.json")).unwrap(),
            nodes
        );
        assert_eq!(
            fs::read(source.join("skills/.curator_state")).unwrap(),
            curator
        );
    }

    #[test]
    #[serial_test::serial]
    fn first_announced_run_from_schema_28_drains_generation_one_backlog() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("codex", project.to_str().unwrap());
        instance.tool = "codex".to_owned();
        instance.agent_session_id = Some("old-native-context".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let legacy = home.join(".codex/sandbox").join(&instance.id);
        fs::create_dir_all(legacy.join("sessions")).unwrap();
        fs::write(
            legacy.join("sessions/original.jsonl"),
            b"LEGACY_PRIVATE_CONTEXT",
        )
        .unwrap();
        fs::write(legacy.join("config.toml"), b"model = 'fixture-model'\n").unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_store_generation"] = serde_json::json!(1);
        row["sandbox_info"] = serde_json::json!({"enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"});
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(app.join(".schema_version"), "28").unwrap();
        let _probes =
            install_test_reconcile_probes(|_| Ok(false), |_| Ok(true), |_| Ok(Vec::new()));

        super::super::run_migrations_announced(None).unwrap();
        let registry = fs::read(app.join("sessions.json")).unwrap();
        let rows: Vec<Value> = serde_json::from_slice(&registry).unwrap();
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        assert!(roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert!(!legacy.exists());
        assert_eq!(
            fs::read(roots[0].path.join("sessions/original.jsonl")).unwrap(),
            b"LEGACY_PRIVATE_CONTEXT"
        );
        assert_eq!(
            fs::read(roots[0].path.join("config.toml")).unwrap(),
            b"model = 'fixture-model'\n"
        );
        let receipt = receipt_path(&app, &instance.id, "codex").unwrap();
        let archives: BTreeMap<_, _> = fs::read_dir(receipt.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "complete")
            })
            .map(|path| {
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        assert_eq!(archives.len(), 1);
        let archive: Receipt = serde_json::from_slice(archives.values().next().unwrap()).unwrap();
        assert_eq!(archive.phase, Phase::Committed);
        let recovery = &archive.roots[0].recovery;
        assert_eq!(
            fs::read(recovery.join("sessions/original.jsonl")).unwrap(),
            b"LEGACY_PRIVATE_CONTEXT"
        );
        let active_identity = identity(&roots[0].path).unwrap();
        let recovery_identity = identity(recovery).unwrap();

        super::super::run_migrations_announced(None).unwrap();
        assert_eq!(identity(&roots[0].path).unwrap(), active_identity);
        assert_eq!(identity(recovery).unwrap(), recovery_identity);
        assert_eq!(fs::read(app.join("sessions.json")).unwrap(), registry);
        assert!(roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        let retried: BTreeMap<_, _> = fs::read_dir(receipt.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "complete")
            })
            .map(|path| {
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        assert_eq!(retried, archives);
        assert_eq!(
            fs::read(recovery.join("sessions/original.jsonl")).unwrap(),
            b"LEGACY_PRIVATE_CONTEXT"
        );
    }

    #[test]
    #[serial_test::serial]
    fn startup_from_schema_28_leaves_generation_one_backlog_untouched() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("codex", project.to_str().unwrap());
        instance.tool = "codex".to_owned();
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let legacy = home.join(".codex/sandbox").join(&instance.id);
        fs::create_dir_all(legacy.join("sessions")).unwrap();
        fs::write(
            legacy.join("sessions/original.jsonl"),
            b"LEGACY_PRIVATE_CONTEXT",
        )
        .unwrap();
        let original_identity = identity(&legacy).unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        row["sandbox_store_generation"] = serde_json::json!(1);
        row["sandbox_info"] = serde_json::json!({"enabled": true, "image": "img", "container_name": "aoe-sandbox-fixture"});
        let registry = serde_json::to_vec(&vec![row]).unwrap();
        fs::write(app.join("sessions.json"), &registry).unwrap();
        fs::write(app.join(".schema_version"), "28").unwrap();
        let _probes = install_test_reconcile_probes(
            |_| panic!("startup must not inspect pending containers"),
            |_| panic!("startup must not reap pending containers"),
            |_| panic!("startup must not inspect exposure"),
        );
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let reporter: progress::Reporter =
            std::sync::Arc::new(move |event| captured.lock().unwrap().push(event));

        super::super::run_migrations_with(Some(reporter)).unwrap();
        assert_eq!(fs::read(app.join("sessions.json")).unwrap(), registry);
        assert_eq!(identity(&legacy).unwrap(), original_identity);
        assert_eq!(
            fs::read(legacy.join("sessions/original.jsonl")).unwrap(),
            b"LEGACY_PRIVATE_CONTEXT"
        );
        assert!(!roots[0].path.exists());
        assert!(!roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert!(
            read_receipt(&receipt_path(&app, &instance.id, "codex").unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[serial_test::serial]
    fn recovery_parent_sync_failure_keeps_original_and_retryable_receipt() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let mut instance =
            crate::session::Instance::new("codex", temporary.path().to_str().unwrap());
        instance.tool = "codex".to_owned();
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(
            root.join("sessions/original.jsonl"),
            b"PRIVATE_ORIGINAL_CONTEXT",
        )
        .unwrap();
        fs::write(root.join("config.toml"), b"model = 'fixture-model'\n").unwrap();
        let row = serde_json::to_value(&instance).unwrap();
        let (path, mut receipt) = checked_receipt(&app, &row, "codex", &roots).unwrap();
        record_retirement(&mut receipt, &row, &home, &config).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
        let original = identity(root).unwrap();
        let stage = receipt.roots[0].stage.clone();
        let staged = identity(&stage).unwrap();
        let recovery = receipt.roots[0].recovery.clone();
        let transaction = recovery.parent().unwrap().parent().unwrap().to_path_buf();
        let _failure = SyncFailureGuard;
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(Some(transaction));
        let result = publish_receipt(&mut receipt, &path);
        crate::session::anchored_fs::FAIL_SYNC_ONCE.set(None);
        assert!(
            result.is_err(),
            "retention requires a durable transaction directory"
        );
        assert_eq!(identity(root).unwrap(), original);
        assert_eq!(identity(&stage).unwrap(), staged);
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        assert_eq!(
            fs::read(stage.join("config.toml")).unwrap(),
            b"model = 'fixture-model'\n"
        );
        assert!(!recovery.exists());
        assert_eq!(read_receipt(&path).unwrap().unwrap().phase, Phase::Staged);
        assert!(!roots_ready(&app, &instance.id, "codex", &roots).unwrap());

        publish_receipt(&mut receipt, &path).unwrap();
        receipt.phase = Phase::Committed;
        write_receipt(&path, &receipt).unwrap();
        certify_receipt(&app, &receipt).unwrap();
        archive_receipt(&path, &receipt).unwrap();
        assert!(roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert_eq!(identity(&recovery).unwrap(), original);
        assert_eq!(identity(root).unwrap(), staged);
        assert_eq!(
            fs::read(recovery.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
    }

    #[test]
    #[serial_test::serial]
    fn stopped_original_is_preserved_before_fresh_content_is_certified() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let mut instance =
            crate::session::Instance::new("codex", temporary.path().to_str().unwrap());
        instance.tool = "codex".to_owned();
        instance.agent_session_id = Some("old-native-context".to_owned());
        instance.acp_session_id = Some("independent-adapter-context".to_owned());
        let config = crate::session::Config::default();
        let roots = container_config::sandbox_content_roots(
            "codex",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(root.join("config.toml"), "model = 'fixture-model'\n").unwrap();
        fs::write(
            root.join("sessions/original.jsonl"),
            b"PRIVATE_ORIGINAL_CONTEXT",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "/outside-do-not-follow",
            root.join("escaping-original-link"),
        )
        .unwrap();
        let mut row = serde_json::to_value(&instance).unwrap();
        let (path, mut receipt) = checked_receipt(&app, &row, "codex", &roots).unwrap();
        record_retirement(&mut receipt, &row, &home, &config).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        publish_receipt(&mut receipt, &path).unwrap();
        assert!(!roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        assert_eq!(
            fs::read(root.join("config.toml")).unwrap(),
            b"model = 'fixture-model'\n"
        );
        assert_eq!(
            fs::read(root.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        assert!(!root.join("escaping-original-link").exists());
        let recovery = &receipt.roots[0].recovery;
        assert_eq!(
            fs::read(recovery.join("sessions/original.jsonl")).unwrap(),
            b"PRIVATE_ORIGINAL_CONTEXT"
        );
        assert_eq!(
            fs::read_link(recovery.join("escaping-original-link")).unwrap(),
            Path::new("/outside-do-not-follow")
        );
        reset_row(&mut row, &receipt).unwrap();
        let restored: crate::session::Instance = serde_json::from_value(row.clone()).unwrap();
        assert_eq!(
            restored.agent_session_id.as_deref(),
            Some("old-native-context")
        );
        assert_eq!(
            restored.acp_session_id.as_deref(),
            Some("independent-adapter-context")
        );
        let reset = row.clone();
        reset_row(&mut row, &receipt).unwrap();
        assert_eq!(row, reset, "recovery retry must not mint another reset");
        receipt.phase = Phase::Committed;
        write_receipt(&path, &receipt).unwrap();
        certify_receipt(&app, &receipt).unwrap();
        archive_receipt(&path, &receipt).unwrap();
        assert!(roots_ready(&app, &instance.id, "codex", &roots).unwrap());
        let replaced = root.with_file_name("old-owned-root");
        fs::rename(root, &replaced).unwrap();
        fs::create_dir(root).unwrap();
        assert!(
            !roots_ready(&app, &instance.id, "codex", &roots).unwrap(),
            "copied logical metadata cannot certify a replacement physical directory"
        );
    }
    #[test]
    #[serial_test::serial]
    fn shared_declared_root_preserves_each_agents_configuration() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let shared = home.join("shared-native-config");
        fs::create_dir_all(shared.join("agent")).unwrap();
        fs::write(
            shared.join("settings.json"),
            r#"{"permissions":{"allow":["Read"]}}"#,
        )
        .unwrap();
        fs::write(shared.join("agent/settings.json"), r#"{"theme":"light"}"#).unwrap();
        let mut config = crate::session::Config::default();
        for tool in ["claude", "omp"] {
            config
                .session
                .agent_config_dir
                .insert(tool.to_owned(), shared.to_str().unwrap().to_owned());
        }
        let instance = crate::session::Instance::new("shared", temporary.path().to_str().unwrap());
        let roots = container_config::sandbox_content_roots(
            "claude",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let guard = ensure_fresh_content(
            &app,
            &home,
            &instance.id,
            "claude",
            &roots,
            &config,
            temporary.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read(roots[0].path.join("settings.json")).unwrap(),
            fs::read(shared.join("settings.json")).unwrap()
        );
        assert_eq!(
            fs::read(roots[0].path.join("agent/settings.json")).unwrap(),
            fs::read(shared.join("agent/settings.json")).unwrap()
        );
        drop(guard);
    }

    #[test]
    #[serial_test::serial]
    fn shared_declared_root_never_restages_certified_owned_content() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let shared = home.join("shared-native-config");
        let mut config = crate::session::Config::default();
        config
            .session
            .agent_config_dir
            .insert("claude".to_owned(), shared.to_str().unwrap().to_owned());
        let instance = crate::session::Instance::new("shared", temporary.path().to_str().unwrap());
        let roots = container_config::sandbox_content_roots(
            "claude",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        drop(
            ensure_fresh_content(
                &app,
                &home,
                &instance.id,
                "claude",
                &roots,
                &config,
                temporary.path(),
            )
            .unwrap(),
        );
        let owned = roots[0].path.join("projects/owned.jsonl");
        fs::create_dir_all(owned.parent().unwrap()).unwrap();
        fs::write(&owned, b"OWNED_CONTEXT_MUST_STAY").unwrap();
        config
            .session
            .agent_config_dir
            .insert("omp".to_owned(), shared.to_str().unwrap().to_owned());
        let requested = container_config::sandbox_content_roots(
            "omp",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let row = serde_json::json!({"id":instance.id,"tool":"omp"});
        let mut receipt = new_receipt(&app, &row, "omp", &requested)
            .expect("an owned root the alias shares must plan, not refuse");
        let path = receipt_path(&app, &instance.id, "omp").unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path())
            .expect("a role extension of an owned root must stage");
        publish_receipt(&mut receipt, &path).expect("and publish");
        assert_eq!(fs::read(&owned).unwrap(), b"OWNED_CONTEXT_MUST_STAY");
    }

    #[test]
    #[serial_test::serial]
    fn owned_root_adds_an_alias_role_without_overwriting_local_state() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let shared = home.join("shared-native-config");
        let mut config = crate::session::Config::default();
        config
            .session
            .agent_config_dir
            .insert("claude".into(), shared.to_str().unwrap().into());
        let instance = crate::session::Instance::new("shared", temporary.path().to_str().unwrap());
        let claude = container_config::sandbox_content_roots(
            "claude",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        drop(
            ensure_fresh_content(
                &app,
                &home,
                &instance.id,
                "claude",
                &claude,
                &config,
                temporary.path(),
            )
            .unwrap(),
        );
        let root = &claude[0].path;
        fs::create_dir_all(root.join("agent")).unwrap();
        fs::create_dir_all(root.join("projects")).unwrap();
        fs::write(root.join("agent/settings.json"), br#"{"theme":"local"}"#).unwrap();
        fs::write(root.join("projects/owned.jsonl"), b"OWNED_CONTEXT").unwrap();
        fs::create_dir_all(shared.join("agent")).unwrap();
        fs::write(shared.join("agent/settings.json"), br#"{"theme":"host"}"#).unwrap();
        fs::write(
            shared.join("agent/models.json"),
            br#"{"providers":{"fixture":{"baseUrl":"http://fixture.invalid","models":[]}}}"#,
        )
        .unwrap();
        config
            .session
            .agent_config_dir
            .insert("my-omp".into(), shared.to_str().unwrap().into());
        config
            .session
            .agent_detect_as
            .insert("my-omp".into(), "omp".into());
        let omp = container_config::sandbox_content_roots(
            "my-omp",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        drop(
            ensure_fresh_content(
                &app,
                &home,
                &instance.id,
                "my-omp",
                &omp,
                &config,
                temporary.path(),
            )
            .unwrap(),
        );
        assert_eq!(
            fs::read(root.join("agent/settings.json")).unwrap(),
            br#"{"theme":"local"}"#
        );
        assert_eq!(
            fs::read(root.join("agent/models.json")).unwrap(),
            fs::read(shared.join("agent/models.json")).unwrap()
        );
        assert_eq!(
            fs::read(root.join("projects/owned.jsonl")).unwrap(),
            b"OWNED_CONTEXT"
        );
        assert!(roots_ready(&app, &instance.id, "my-omp", &omp).unwrap());
        assert!(roots_ready(&app, &instance.id, "claude", &claude).unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn pending_original_cannot_be_bypassed_by_role_or_tool_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        let mut config = crate::session::Config::default();
        let shared = home.join("shared-native-config");
        config
            .session
            .agent_config_dir
            .insert("claude".into(), shared.to_str().unwrap().into());
        let instance = crate::session::Instance::new("shared", temporary.path().to_str().unwrap());
        let roots = container_config::sandbox_content_roots(
            "claude",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let root = &roots[0].path;
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("history.jsonl"), b"ORIGINAL_CONTEXT").unwrap();
        let row = serde_json::json!({"id":instance.id,"tool":"claude"});
        let (path, mut receipt) = checked_receipt(&app, &row, "claude", &roots).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
        config
            .session
            .agent_config_dir
            .insert("omp".into(), shared.to_str().unwrap().into());
        let mut changed = roots.clone();
        container_config::expand_content_roles(&mut changed, &home, &config.session).unwrap();
        assert!(checked_receipt(&app, &row, "claude", &changed).is_err());
        let other = container_config::sandbox_content_roots(
            "omp",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        assert!(checked_receipt(&app, &row, "omp", &other).is_err());
        assert_eq!(
            fs::read(root.join("history.jsonl")).unwrap(),
            b"ORIGINAL_CONTEXT"
        );
        let (resumed_path, mut resumed) = checked_receipt(&app, &row, "claude", &roots).unwrap();
        publish_receipt(&mut resumed, &resumed_path).unwrap();
        assert_eq!(
            fs::read(resumed.roots[0].recovery.join("history.jsonl")).unwrap(),
            b"ORIGINAL_CONTEXT"
        );
        assert!(!root.join("history.jsonl").exists());
    }

    #[test]
    #[serial_test::serial]
    fn fresh_publication_recovers_before_certificate_commit() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = dirs::home_dir().unwrap();
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            br#"{"fixture":"original-config"}"#,
        )
        .unwrap();
        let config = crate::session::Config::default();
        let instance = crate::session::Instance::new("fresh", temporary.path().to_str().unwrap());
        let roots = container_config::sandbox_content_roots(
            "claude",
            None,
            &config.session,
            &home,
            &instance.id,
        )
        .unwrap();
        let row = serde_json::json!({"id":instance.id,"tool":"claude"});
        let (path, mut receipt) = checked_receipt(&app, &row, "claude", &roots).unwrap();
        stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
        publish_receipt(&mut receipt, &path).unwrap();
        fs::write(
            roots[0].path.join("settings.json"),
            br#"{"fixture":"retained-local-config"}"#,
        )
        .unwrap();
        drop(
            ensure_fresh_content(
                &app,
                &home,
                &instance.id,
                "claude",
                &roots,
                &config,
                temporary.path(),
            )
            .unwrap(),
        );
        assert_eq!(
            fs::read(roots[0].path.join("settings.json")).unwrap(),
            br#"{"fixture":"retained-local-config"}"#
        );
        assert!(roots_ready(&app, &instance.id, "claude", &roots).unwrap());
    }
}
