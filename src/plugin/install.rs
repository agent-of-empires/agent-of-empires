//! Plugin enable/disable and external install / update / uninstall.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use aoe_plugin_api::{BuildStep, PluginId, PluginManifest, RuntimeSpec, UiContribution};
use serde::Serialize;

use crate::session::{update_config, CapabilityGrant, Config, PluginConfig};

use super::changelog::UpdateChangelog;
use super::featured::FeaturedIndex;
use super::fetch::{self, FetchedPlugin};
use super::lockfile::{LockedPlugin, Lockfile};
use super::registry::ValidationState;
use super::source::PluginSource;

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

pub fn set_enabled(plugin_id: &str, enabled: bool) -> Result<()> {
    let registry = super::registry();
    if registry.get(plugin_id).is_none() {
        bail!("unknown plugin {plugin_id:?}; see `aoe plugin list`");
    }
    enable_in_config(plugin_id, enabled)?;
    super::reload_registry();
    Ok(())
}

#[derive(Debug)]
pub enum LiveToggle {
    Daemon,
    Local,
    LocalDaemonStale { reason: String },
}

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

fn install_validation(featured_verified: bool, source: &str) -> ValidationState {
    if featured_verified {
        ValidationState::Featured
    } else if source.starts_with("gh:") {
        ValidationState::Community
    } else {
        ValidationState::Local
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentMode {
    Interactive,
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

#[derive(Debug, Clone, Serialize)]
pub struct UiView {
    pub slot: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
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
    pub runtime_change: Option<String>,
    pub trust_downgrade: bool,
    pub fingerprint: String,
    pub stays_active_if_declined: bool,
    pub changelog: UpdateChangelog,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdatePreview {
    NoUpdate,
    SafeUpdate {
        to_version: String,
        fingerprint: String,
        changelog: UpdateChangelog,
    },
    ConsentRequired {
        consent: Box<UpdateConsent>,
        dismissed: bool,
    },
}

struct PreparedInstall {
    persisted_source: String,
    unverified: bool,
    notice: String,
    fetched: FetchedPlugin,
    featured_verified: bool,
    id: String,
    capabilities: Vec<String>,
    manifest_hash: String,
    fingerprint: String,
    validation: ValidationState,
}

#[derive(Debug, Clone, Serialize)]
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
    pub fingerprint: String,
}

async fn prepare_install(input: &str) -> Result<PreparedInstall> {
    let source = PluginSource::parse(input)?;
    let resolved = resolve_source(source, true).await?;
    let fetched = fetch::fetch(&resolved.source).await?;

    let id = fetched.manifest.id.as_str().to_string();
    let featured_verified = verify_featured(&FeaturedIndex::load()?, &fetched)?;
    reject_reserved_or_builtin(&fetched.manifest, featured_verified)?;
    reject_incompatible_host(&fetched.manifest)?;

    if super::plugins_dir()?.join(&id).exists() {
        bail!("{id} is already installed; run `aoe plugin update {id}` or uninstall it first");
    }

    let capabilities = capability_strings(&fetched)?;
    let manifest_hash = PluginManifest::hash_bytes(&fetched.manifest_bytes);
    let trust = if featured_verified {
        "featured"
    } else {
        "community"
    };
    let fingerprint = fingerprint(&fetched.tree_hash, fetched.asset_sha256.as_deref(), trust);
    let persisted_source = persisted_source(&resolved.source, input);
    let validation = install_validation(featured_verified, &persisted_source);

    Ok(PreparedInstall {
        persisted_source,
        unverified: resolved.unverified,
        notice: resolved.notice,
        fetched,
        featured_verified,
        id,
        capabilities,
        manifest_hash,
        fingerprint,
        validation,
    })
}

fn apply_prepared_install(p: &PreparedInstall, log: &OperationLog) -> Result<InstallReport> {
    let final_dir = super::plugins_dir()?.join(&p.id);
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
    move_into_place(&p.fetched, &final_dir)?;
    if let Err(e) = build_in_place(&p.id, &final_dir, &p.fetched.manifest, log) {
        let _ = std::fs::remove_dir_all(&final_dir);
        return Err(e);
    }

    let persisted = (|| -> Result<()> {
        persist_install(
            &p.persisted_source,
            &p.id,
            &p.capabilities,
            &p.manifest_hash,
        )?;
        write_lock(&p.id, &p.fetched, &p.manifest_hash, p.featured_verified)
    })();
    if let Err(e) = persisted {
        let _ = uninstall(&p.id);
        let _ = std::fs::remove_dir_all(&final_dir);
        return Err(e);
    }
    super::reload_registry();
    log.line(&format!(
        "installed {} {}",
        p.id, p.fetched.manifest.version
    ));

    Ok(InstallReport {
        id: p.id.clone(),
        version: p.fetched.manifest.version.clone(),
        capabilities: p.capabilities.clone(),
        granted: true,
        validation: p.validation,
    })
}

pub async fn install(input: &str, assume_yes: bool) -> Result<InstallReport> {
    let prepared = prepare_install(input).await?;
    eprintln!("{}", prepared.notice);
    if prepared.unverified && !assume_yes && !confirm_unverified()? {
        bail!("install cancelled; the unverified source was not approved");
    }
    let build = build_steps(&prepared.fetched.manifest);
    let granted = if assume_yes
        || !install_needs_consent(&prepared.capabilities, build, &prepared.fetched.manifest.ui)
    {
        true
    } else {
        confirm_capabilities(
            &prepared.id,
            &prepared.capabilities,
            &prepared.fetched.manifest.ui,
            build,
        )?
    };
    if !granted {
        bail!("install cancelled; no capabilities were granted");
    }
    apply_prepared_install(&prepared, &OperationLog::Inherit)
}

pub async fn preview_install(input: &str) -> Result<InstallConsent> {
    if !input.starts_with("gh:") {
        bail!("web install supports gh: sources only; use `aoe plugin install` for a local path");
    }
    let p = prepare_install(input).await?;
    Ok(InstallConsent {
        id: p.id.clone(),
        version: p.fetched.manifest.version.clone(),
        source: p.persisted_source.clone(),
        notice: p.notice.clone(),
        unverified: p.unverified,
        validation: p.validation.as_str().to_string(),
        capabilities: p.capabilities.clone(),
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
        fingerprint: p.fingerprint.clone(),
    })
}

pub async fn apply_install(
    input: &str,
    expected_fingerprint: &str,
    log: &OperationLog,
) -> Result<InstallReport> {
    if !input.starts_with("gh:") {
        bail!("web install supports gh: sources only; use `aoe plugin install` for a local path");
    }
    let prepared = prepare_install(input).await?;
    if prepared.fingerprint != expected_fingerprint {
        bail!(
            "the plugin at {input} changed since it was shown; review it again before installing"
        );
    }
    apply_prepared_install(&prepared, log)
}

pub async fn update(id: &str) -> Result<InstallReport> {
    match update_with_consent(id, ConsentMode::Interactive).await? {
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
    runtime_change: Option<String>,
    trust_downgrade: bool,
    needs_consent: bool,
}

fn fingerprint(tree_hash: &str, asset_sha256: Option<&str>, trust: &str) -> String {
    format!("{tree_hash}|{}|{trust}", asset_sha256.unwrap_or(""))
}

async fn prepare_update(id: &str) -> Result<Prepared> {
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

    let source = PluginSource::parse(&source_str)?;
    let resolved = resolve_source(source, false).await?;
    let fetched = fetch::fetch(&resolved.source).await?;
    if fetched.manifest.id.as_str() != id {
        bail!(
            "source {source_str:?} now resolves to plugin {:?}, not {id}",
            fetched.manifest.id.as_str()
        );
    }
    let featured_verified = verify_featured(&FeaturedIndex::load()?, &fetched)?;
    reject_reserved_or_builtin(&fetched.manifest, featured_verified)?;
    reject_incompatible_host(&fetched.manifest)?;

    let capabilities = capability_strings(&fetched)?;
    let manifest_hash = PluginManifest::hash_bytes(&fetched.manifest_bytes);

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

    let trust = if featured_verified {
        "featured"
    } else {
        "community"
    };
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
    let new_is_release_binary = matches!(
        fetched.manifest.runtime,
        Some(RuntimeSpec::ReleaseBinary { .. })
    );
    let runtime_change = if new_is_release_binary && !prior_was_release_binary {
        Some(
            "the worker is now a downloaded release binary (opaque, not source-auditable)"
                .to_string(),
        )
    } else if prior_was_release_binary && !new_is_release_binary {
        Some("the worker is now an in-tree command (was a downloaded release binary)".to_string())
    } else {
        None
    };
    let trust_downgrade = prior_trust == "featured" && !featured_verified;

    let needs_consent = (!capabilities.is_empty() && caps_changed)
        || build_changed
        || ui_changed
        || runtime_change.is_some()
        || trust_downgrade;

    Ok(Prepared {
        id: id.to_string(),
        source_str,
        notice: resolved.notice,
        fetched,
        featured_verified,
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
        runtime_change,
        trust_downgrade,
        needs_consent,
    })
}

fn apply_prepared(
    prepared: &Prepared,
    grant: Option<CapabilityGrant>,
    log: &OperationLog,
) -> Result<InstallReport> {
    let id = prepared.id.as_str();
    let final_dir = super::plugins_dir()?.join(id);
    if prepared.needs_consent && grant.is_none() {
        bail!("update cancelled for {id}; the previously trusted version was kept");
    }
    log.line(&format!(
        "updating {id} to {}",
        prepared.fetched.manifest.version
    ));
    replace_and_build(id, &prepared.fetched, &final_dir, log)?;

    let granted = grant.is_some();
    persist_update(id, &prepared.source_str, grant)?;
    write_lock(
        id,
        &prepared.fetched,
        &prepared.manifest_hash,
        prepared.featured_verified,
    )?;
    super::reload_registry();

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
        validation: install_validation(prepared.featured_verified, &prepared.source_str),
    })
}

async fn update_with_consent(id: &str, mode: ConsentMode) -> Result<UpdateOutcome> {
    let prepared = prepare_update(id).await?;
    if mode == ConsentMode::Interactive {
        eprintln!("{}", prepared.notice);
    }

    let grant = if prepared.needs_consent {
        if mode == ConsentMode::CleanOnlyNonInteractive {
            return Ok(UpdateOutcome::Skipped {
                id: id.to_string(),
                reason: skip_reason(&prepared),
                fingerprint: prepared.fingerprint.clone(),
            });
        }
        if confirm_capabilities(
            id,
            &prepared.capabilities,
            &prepared.fetched.manifest.ui,
            build_steps(&prepared.fetched.manifest),
        )? {
            Some(CapabilityGrant {
                manifest_hash: prepared.manifest_hash.clone(),
                capabilities: prepared.capabilities.clone(),
                granted_at: chrono::Utc::now(),
            })
        } else {
            None
        }
    } else if prepared.capabilities.is_empty() {
        Some(CapabilityGrant {
            manifest_hash: prepared.manifest_hash.clone(),
            capabilities: vec![],
            granted_at: chrono::Utc::now(),
        })
    } else {
        prepared.prior_grant.clone().map(|g| CapabilityGrant {
            manifest_hash: prepared.manifest_hash.clone(),
            capabilities: g.capabilities,
            granted_at: g.granted_at,
        })
    };

    Ok(UpdateOutcome::Applied(apply_prepared(
        &prepared,
        grant,
        &OperationLog::Inherit,
    )?))
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
        runtime_change: p.runtime_change.clone(),
        trust_downgrade: p.trust_downgrade,
        fingerprint: p.fingerprint.clone(),
        stays_active_if_declined: true,
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

pub async fn preview_update(id: &str) -> Result<UpdatePreview> {
    let prepared = prepare_update(id).await?;
    if prepared.prior_fingerprint.as_ref() == Some(&prepared.fingerprint) {
        return Ok(UpdatePreview::NoUpdate);
    }
    let changelog = changelog_of(&prepared).await;
    if !prepared.needs_consent {
        return Ok(UpdatePreview::SafeUpdate {
            to_version: prepared.fetched.manifest.version.clone(),
            fingerprint: prepared.fingerprint.clone(),
            changelog,
        });
    }
    let dismissed = Config::load()
        .ok()
        .and_then(|c| c.plugins.get(id).and_then(|p| p.dismissed_update.clone()))
        == Some(prepared.fingerprint.clone());
    Ok(UpdatePreview::ConsentRequired {
        consent: Box::new(consent_of(&prepared, changelog)),
        dismissed,
    })
}

pub async fn apply_update(
    id: &str,
    expected_fingerprint: Option<String>,
    log: &OperationLog,
) -> Result<InstallReport> {
    let prepared = prepare_update(id).await?;
    match &expected_fingerprint {
        Some(expected) if *expected != prepared.fingerprint => {
            bail!(
                "the available update for {id} changed since it was shown; review it again before approving"
            );
        }
        None if prepared.needs_consent => {
            bail!("approving the update for {id} requires the fingerprint it was previewed with");
        }
        _ => {}
    }
    let grant = Some(CapabilityGrant {
        manifest_hash: prepared.manifest_hash.clone(),
        capabilities: prepared.capabilities.clone(),
        granted_at: chrono::Utc::now(),
    });
    apply_prepared(&prepared, grant, log)
}

#[derive(Debug, Clone, Serialize)]
pub struct ReapproveConsent {
    pub id: String,
    pub version: String,
    pub validation: String,
    pub capabilities: Vec<String>,
    pub ui: Vec<UiView>,
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
        manifest_hash: plugin.manifest_hash.clone(),
    })
}

pub fn approve_installed(id: &str, expected_manifest_hash: &str) -> Result<()> {
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
            granted_at: chrono::Utc::now(),
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
            if let Err(e) = client.set_plugin_enabled(&id, enabled).await {
                tracing::warn!(
                    "plugin toggle: daemon did not reconcile {id} (enabled={enabled}): {e}; \
                         restart the daemon or toggle from the dashboard"
                );
            }
        }
    });
}

pub fn dismiss_update(id: &str, fingerprint: &str) -> Result<()> {
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
    let config = Config::load()?;
    let is_external = config
        .plugins
        .get(id)
        .and_then(|p| p.source.as_ref())
        .is_some();
    if !is_external {
        bail!("{id} is not an installed external plugin");
    }

    let dir = super::plugins_dir()?.join(id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
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
) -> bool {
    !capabilities.is_empty() || !build.is_empty() || !ui.is_empty()
}

fn confirm_capabilities(
    id: &str,
    capabilities: &[String],
    ui: &[UiContribution],
    build: &[BuildStep],
) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!(
            "{id} requests capabilities [{}]{} but stdin is not a terminal; re-run with --yes to grant them",
            capabilities.join(", "),
            if build.is_empty() { "" } else { " and declares build steps" },
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

fn move_into_place(fetched: &FetchedPlugin, final_dir: &std::path::Path) -> Result<()> {
    if final_dir.exists() {
        std::fs::remove_dir_all(final_dir)
            .with_context(|| format!("replacing {}", final_dir.display()))?;
    }
    std::fs::rename(&fetched.tree, final_dir).with_context(|| {
        format!(
            "moving plugin into {} (cross-device staging?)",
            final_dir.display()
        )
    })
}

fn build_steps(manifest: &PluginManifest) -> &[BuildStep] {
    match &manifest.runtime {
        Some(RuntimeSpec::Command { build, .. }) => build,
        _ => &[],
    }
}

fn build_in_place(
    plugin_id: &str,
    dir: &Path,
    manifest: &PluginManifest,
    log: &OperationLog,
) -> Result<()> {
    run_build(plugin_id, dir, build_steps(manifest), log)?;
    if let Some(RuntimeSpec::Command {
        command,
        system: false,
        ..
    }) = &manifest.runtime
    {
        super::launch::resolve_command(plugin_id, dir, command, &super::launch::OsLaunchResolver)
            .with_context(|| {
            format!(
                "plugin {plugin_id}: worker command is not runnable after install \
                     (a build step may have been skipped on this platform, or did not produce it)"
            )
        })?;
    }
    Ok(())
}

fn run_build(plugin_id: &str, dir: &Path, steps: &[BuildStep], log: &OperationLog) -> Result<()> {
    let os = std::env::consts::OS;
    for (i, step) in steps.iter().enumerate() {
        if !step.platforms.is_empty() && !step.platforms.iter().any(|p| p == os) {
            continue;
        }
        let pretty = step.command.join(" ");
        let (program, args) = super::launch::resolve_command(
            plugin_id,
            dir,
            &step.command,
            &super::launch::OsLaunchResolver,
        )
        .with_context(|| format!("resolving build step {} ({pretty})", i + 1))?;
        log.line(&format!("  building {plugin_id}: {pretty}"));
        let (stdout, stderr) = log.child_stdio()?;
        let status = std::process::Command::new(&program)
            .args(&args)
            .current_dir(dir)
            .env("AOE_PLUGIN_ID", plugin_id)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .status()
            .with_context(|| format!("spawning build step {} ({pretty})", i + 1))?;
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
    log: &OperationLog,
) -> Result<()> {
    let backup_dir = final_dir.with_file_name(format!("{plugin_id}.bak"));

    if backup_dir.exists() {
        if final_dir.exists() {
            let _ = std::fs::remove_dir_all(final_dir);
        }
        std::fs::rename(&backup_dir, final_dir)
            .with_context(|| format!("recovering interrupted update backup for {plugin_id}"))?;
    }

    let had_prior = final_dir.exists();
    if had_prior {
        std::fs::rename(final_dir, &backup_dir)
            .with_context(|| format!("backing up current {plugin_id} before update"))?;
    }

    let place_and_build = (|| -> Result<()> {
        std::fs::rename(&fetched.tree, final_dir).with_context(|| {
            format!(
                "moving plugin into {} (cross-device staging?)",
                final_dir.display()
            )
        })?;
        build_in_place(plugin_id, final_dir, &fetched.manifest, log)
    })();

    match place_and_build {
        Ok(()) => {
            if had_prior {
                let _ = std::fs::remove_dir_all(&backup_dir);
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(final_dir);
            if had_prior {
                let _ = std::fs::rename(&backup_dir, final_dir);
            }
            Err(e)
        }
    }
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
) -> Result<()> {
    update_config(|config| {
        let entry = config
            .plugins
            .entry(id.to_string())
            .or_insert_with(PluginConfig::default);
        entry.source = Some(source.to_string());
        entry.grant = Some(CapabilityGrant {
            manifest_hash: manifest_hash.to_string(),
            capabilities: capabilities.to_vec(),
            granted_at: chrono::Utc::now(),
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

fn write_lock(
    id: &str,
    fetched: &FetchedPlugin,
    manifest_hash: &str,
    featured_verified: bool,
) -> Result<()> {
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
            trust: if featured_verified {
                "featured"
            } else {
                "community"
            }
            .to_string(),
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

    fn ui(slot: UiSlot, id: &str) -> UiContribution {
        UiContribution {
            slot,
            id: id.to_string(),
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
    fn install_consent_required_for_caps_build_or_ui() {
        assert!(!install_needs_consent(&[], &[], &[]));
        assert!(install_needs_consent(&["net".to_string()], &[], &[]));
        assert!(install_needs_consent(
            &[],
            &[],
            &[ui(UiSlot::StatusBar, "s")]
        ));
    }

    #[tokio::test]
    async fn web_install_rejects_non_gh_sources() {
        let err = preview_install("/tmp/some/plugin")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("gh:"), "{err}");
        let err = apply_install("./local/dir", "fp", &OperationLog::Inherit)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("gh:"), "{err}");
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
}
