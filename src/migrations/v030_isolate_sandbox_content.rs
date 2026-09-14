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

fn claim_context_reset(
    instance: &mut crate::session::Instance,
    agent: &str,
    view: NativeContextView,
    generation: u64,
) -> Option<(String, Vec<String>)> {
    let mut slots = Vec::new();
    let mut recovery = BTreeSet::new();
    for reset in &mut instance.sandbox_content_resets {
        if reset.tool != instance.tool || reset.agent != agent || !reset.lane(view).pending {
            continue;
        }
        if reset.lane(view).generation.is_none() {
            match view {
                NativeContextView::Terminal => {
                    if reset.retired_terminal.is_some()
                        && instance.agent_session_id == reset.retired_terminal
                    {
                        instance.agent_session_id = None;
                        instance.pi_session_path = None;
                    }
                    if instance.agent_session_id.is_none() && instance.pi_session_path.is_none() {
                        instance.resume_intent = crate::session::ResumeIntent::Cleared;
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
    Some((format!("Sandbox native history was isolated; this launch {context} {agent} conversation. The existing AoE transcript is retained. Complete originals: {}", recovery.into_iter().collect::<Vec<_>>().join(", ")), slots))
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
        if matches!(usage, AcpContextUse::Attach)
            && instance.sandbox_content_resets.iter().any(|reset| {
                reset.tool == instance.tool
                    && Some(reset.agent.as_str()) == agent
                    && reset.structured.pending
                    && reset.structured.generation.is_none()
            })
        {
            bail!("runner predates its sandbox content reset; a fresh launch is required");
        }
        let notice = agent.and_then(|agent| {
            claim_context_reset(instance, agent, NativeContextView::Structured, generation)
        });
        Ok(AcpLaunchContext {
            profile: storage.profile().to_owned(),
            stored_session_id: instance.acp_session_id.clone(),
            fork_from: instance.fork_pending.clone(),
            seed_history_replay: instance.import_pending == Some(true),
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
    let (resets, notice, sid, pi_path, intent, floor, omp_generation) =
        storage.update(|instances, _| {
            let row = instances
                .iter_mut()
                .find(|row| row.id == instance.id)
                .context("sandbox session disappeared before terminal launch")?;
            if row.lifecycle_generation != generation || row.tool != instance.tool {
                bail!("terminal content reset lost its launch scope");
            }
            let notice = claim_context_reset(row, agent, NativeContextView::Terminal, generation);
            Ok((
                row.sandbox_content_resets.clone(),
                notice,
                row.agent_session_id.clone(),
                row.pi_session_path.clone(),
                row.resume_intent.clone(),
                row.capture_started_at,
                row.omp_capture_generation.clone(),
            ))
        })?;
    instance.sandbox_content_resets = resets;
    if notice.is_some() {
        instance.agent_session_id = sid;
        instance.pi_session_path = pi_path;
        instance.resume_intent = intent;
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

/// Only a stopped, journalled migration constructs a stage-seeding capability.
pub(crate) struct ContentSeed<'a> {
    source: &'a Path,
    stopped_original: bool,
}

impl ContentSeed<'_> {
    pub(crate) fn path(&self) -> &Path {
        self.source
    }
    pub(crate) fn is_stopped_original(&self) -> bool {
        self.stopped_original
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

fn write_receipt(path: &Path, receipt: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("content receipt has no parent")?;
    fs::create_dir_all(parent)?;
    let anchor = AnchoredDir::open(parent)?;
    let bytes = serde_json::to_vec_pretty(receipt)?;
    use std::os::unix::fs::PermissionsExt;
    anchor.publish_file(
        Path::new(path.file_name().context("receipt has no leaf")?),
        &mut bytes.as_slice(),
        fs::Permissions::from_mode(0o600),
        true,
        None,
    )?;
    fs::File::open(parent.parent().context("receipt directory has no parent")?)?.sync_all()?;
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
    let path = canonical_expected_path(path)?;
    let certificate = RootCertificate {
        policy: CONTENT_POLICY,
        instance: instance.to_owned(),
        root: ContentRoot {
            path: path.clone(),
            host: PathBuf::new(),
            roles: Vec::new(),
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

/// Retain a v027 original without its lossy copy/overlay policy. The caller
/// holds the stopped cohort lock; the entire tree moves intact,
/// including unclassified entries and links that were never copy candidates.
pub(crate) fn retain_legacy_original(source: &Path, host: &Path) -> Result<Option<PathBuf>> {
    let Some(original) = identity(source)? else {
        return Ok(None);
    };
    let app = crate::session::get_app_dir()?;
    let recovery = recovery_root(host)?;
    ensure_private_recovery(
        &app,
        &[canonical_expected_path(&recovery)?],
        &live_bind_sources,
    )?;
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
    progress::notice(format!(
        "Retained complete legacy sandbox original at {}",
        destination.display()
    ));
    Ok(Some(destination))
}

fn read_registries(app: &Path) -> Result<Vec<(PathBuf, Value)>> {
    layout::registry_paths(app)?
        .into_iter()
        .map(|path| {
            let value = serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok((path, value))
        })
        .collect()
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
    let detect = (row.get("tool").and_then(Value::as_str) == Some(tool))
        .then(|| row.get("detect_as").and_then(Value::as_str))
        .flatten()
        .filter(|name| !name.is_empty());
    container_config::sandbox_content_roots(tool, detect, &config.session, home, id)
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
    if inspected.runtime_handler.is_some() {
        bail!("Apple VM mount lifetime is unproven; native content isolation remains pending");
    }
    let sources: Vec<_> = inspected
        .bind_mounts
        .iter()
        .map(|mount| PathBuf::from(&mount.host_path))
        .collect();
    if !inspected.running {
        return Ok(sources);
    }
    if !inspected.opaque_mounts.is_empty() {
        bail!("live sandbox {id} has opaque mounts; recovery exposure cannot be proven");
    }
    let boot = crate::process::boot_id().context("host kernel identity is unavailable")?;
    let boot = uuid::Uuid::parse_str(&boot).context("host kernel identity is malformed")?;
    let before: Vec<_> = sources
        .iter()
        .map(|source| fs::metadata(source).map(|metadata| (metadata.dev(), metadata.ino())))
        .collect::<std::io::Result<_>>()?;
    // Pin Docker/Podman's immutable runtime id. A local-looking socket is not
    // same-kernel proof: Desktop/machine and remote daemons may sit behind it.
    container.name = inspected.id;
    let mut command = vec!["/bin/sh".to_owned(), "-c".to_owned(),
        r#"PATH=/usr/bin:/bin; export PATH; stat -f -c %t /proc/sys/kernel/random/boot_id && cat /proc/sys/kernel/random/boot_id && stat -L -c %d:%i -- "$@""#.to_owned(),
        "aoe-content-mount-proof".to_owned()];
    command.extend(
        inspected
            .bind_mounts
            .iter()
            .map(|mount| mount.container_path.clone()),
    );
    if sources.is_empty() {
        return Ok(sources);
    }
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
    // UUID must be read from genuine procfs in this same execution envelope.
    if lines.next() != Some("9fa0")
        || lines
            .next()
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            != Some(boot)
    {
        bail!("live sandbox {id} is not proven to share the host kernel; defer native content isolation");
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

fn ensure_private_recovery(
    app: &Path,
    targets: &[PathBuf],
    exposure: &ExposureProbe<'_>,
) -> Result<()> {
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
            let mut sources = exposure(id)?;
            for entry in &config.sandbox.extra_volumes {
                if let Some((source, _)) = entry.split_once(':') {
                    sources.push(PathBuf::from(source));
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
                let canonical = canonical_expected_path(&source)?;
                if targets.iter().any(|target| {
                    target.starts_with(&source)
                        || source.starts_with(target)
                        || target.starts_with(&canonical)
                        || canonical.starts_with(target)
                }) {
                    bail!("sandbox {id} exposes the isolation recovery namespace through {}; remove that mount before migrating", source.display());
                }
            }
        }
    }
    Ok(())
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
                    receipt.retired_tools.insert(tool.clone(), agent.to_owned());
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
            stage: parent.join(format!(".v030-stage-{transaction}-{index}")),
            recovery: recovery_root(&root.host)?
                .join(&transaction)
                .join(index.to_string())
                .join("original"),
            original,
            staged: owned.clone(),
            published: owned,
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
        fs::create_dir_all(parent)?;
        let anchor = AnchoredDir::open(parent)?;
        let leaf = Path::new(part.stage.file_name().context("stage has no leaf")?);
        // Only this durable transaction owns this stage. Original data is never removed here.
        anchor.remove_staged_dir(leaf)?;
        let stage = anchor.create_child(leaf)?;
        let source = if part.original.is_some() {
            &part.root.path
        } else {
            &part.root.host
        };
        let capability = ContentSeed {
            source,
            stopped_original: part.original.is_some(),
        };
        container_config::seed_content_stage(
            &capability,
            &part.root,
            stage.path(),
            home,
            &config.session,
            workspace,
        )?;
        super::store_fs::barrier(&fs::File::open(stage.path())?)?;
        part.staged = identity(stage.path())?;
    }
    receipt.phase = Phase::Staged;
    write_receipt(path, receipt)
}

fn publish_receipt(receipt: &mut Receipt, path: &Path) -> Result<()> {
    if receipt.phase != Phase::Staged {
        return Ok(());
    }
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
                    pending: current || old_acp.is_some(),
                    generation: None,
                },
                retired_terminal: prior
                    .and_then(|prior| prior.get("agent_session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
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
            if same_id && same_pi {
                object.remove("agent_session_id");
                object.remove("pi_session_path");
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
                {
                    let removed = current.remove("agent_session_id").is_some();
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
                .join(format!(".v030-stage-{}-{index}", receipt.transaction));
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
    ensure_private_recovery(app, &migration_targets(app, &roots)?, exposure)?;
    let (path, mut receipt) = checked_receipt(app, &row, tool, &roots)?;
    if receipt.phase == Phase::Committed {
        certify_receipt(app, &receipt)?;
        archive_receipt(&path, &receipt)?;
        return Ok(true);
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
        return Ok(false);
    };
    let mut current_roots = row_roots(current, tool, home, &fresh_config)?;
    container_config::expand_content_roles(&mut current_roots, home, &fresh_config.session)?;
    if current_roots != roots
        || !row_tools(current).contains(tool)
        || current.get("project_path") != row.get("project_path")
    {
        return Ok(false);
    }
    layout::refresh_liveness();
    if running(id)? || detached_writer_live(app, id)? || !reap(id)? {
        return Ok(false);
    }
    ensure_private_recovery(app, &migration_targets(app, &roots)?, exposure)?;
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
            migrate_target(app, home, (&path, &id, &tool), running, reap, exposure)?;
        } else {
            let config = crate::session::config::profile_config::resolve_config(
                &layout::profile_for_registry(app, &path),
            )?;
            if let Some(row) = read_row(&path, &id)? {
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
    reconcile_pending(false)
}

pub(crate) fn reconcile_pending(move_stores: bool) -> Result<()> {
    let app = crate::session::get_app_dir()?;
    let home = dirs::home_dir().context("home directory unavailable for content isolation")?;
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
        Some(&instance.detect_as),
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
        assert!(!root.join("sessions").exists());
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
        assert!(restored.agent_session_id.is_none());
        assert_eq!(
            restored.acp_session_id.as_deref(),
            Some("independent-adapter-context")
        );
        assert!(matches!(
            restored.resume_intent,
            crate::session::ResumeIntent::Cleared
        ));
        assert!(restored.capture_started_at.is_some());
        let reset = row.clone();
        reset_row(&mut row, &receipt).unwrap();
        assert_eq!(
            row, reset,
            "recovery retry must not mint another reset or capture floor"
        );
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
        match new_receipt(&app, &row, "omp", &requested) {
            Ok(mut receipt) => {
                let path = receipt_path(&app, &instance.id, "omp").unwrap();
                stage_receipt(&app, &mut receipt, &path, &home, &config, temporary.path()).unwrap();
                publish_receipt(&mut receipt, &path).unwrap();
            }
            Err(_) => {}
        }
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
        fs::create_dir_all(root.join("projects")).unwrap();
        fs::write(root.join("projects/original.jsonl"), b"ORIGINAL_CONTEXT").unwrap();
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
            fs::read(root.join("projects/original.jsonl")).unwrap(),
            b"ORIGINAL_CONTEXT"
        );
        let (resumed_path, mut resumed) = checked_receipt(&app, &row, "claude", &roots).unwrap();
        publish_receipt(&mut resumed, &resumed_path).unwrap();
        assert_eq!(
            fs::read(resumed.roots[0].recovery.join("projects/original.jsonl")).unwrap(),
            b"ORIGINAL_CONTEXT"
        );
        assert!(!root.join("projects/original.jsonl").exists());
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
