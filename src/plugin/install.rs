//! Plugin enable/disable and external install / update / uninstall.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
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

pub(super) fn log_recovery<T>(result: &Result<T>) {
    if let Err(error) = result {
        tracing::error!(error = %format!("{error:#}"), "plugin mutation ownership retained for recovery");
        if let Ok(root) = crate::session::get_app_dir() {
            if let Err(persist_error) = std::fs::write(
                root.join(".plugin-recovery-required"),
                format!("{error:#}\n"),
            ) {
                tracing::error!(%persist_error, "plugin recovery marker could not be persisted; manual recovery is required before restarting");
            }
        }
    }
}

const OPERATION_LOCK: &str = ".plugin-operation.lock";

pub(crate) fn begin_operation() -> Result<Arc<StorageFlock>> {
    let evidence = crate::session::get_app_dir()?.join(".plugin-recovery-required");
    if evidence.exists() {
        bail!("plugin recovery evidence exists at {}; stop the Daemon, verify owned commands have exited and restore the installation before removing this file, then restart the Daemon", evidence.display());
    }
    crate::session::try_acquire_storage_flock(&crate::session::get_app_dir()?, OPERATION_LOCK)
        .context("acquiring plugin operation lock")?
        .map(Arc::new)
        .ok_or_else(|| {
            anyhow!("another plugin operation is running; inspect its outcome before retrying")
        })
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
            bail!("{id} is already installed; run `aoe plugin update {id}` or uninstall it first");
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

fn apply_prepared_install(
    p: &PreparedInstall,
    node_env: &NodeEnv,
    log: &OperationLog,
) -> Result<InstallReport> {
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
    if let Err(e) = build_in_place(
        &p.id,
        &final_dir,
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
        std::fs::remove_dir_all(&final_dir).map_err(|error| {
            super::execution::RecoveryRequired(format!(
                "removing failed install {}: {error}; original failure: {e:#}",
                final_dir.display()
            ))
        })?;
        return Err(e);
    }

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
        // Not `uninstall`: nothing finished installing, so nothing is announced as uninstalled.
        let rollback = match is_installed_external(&p.id) {
            Ok(true) => remove_external(&p.id),
            Ok(false) => std::fs::remove_dir_all(&final_dir)
                .with_context(|| format!("removing {}", final_dir.display())),
            // An unreadable config must not strand the staged tree, or the next
            // install of the same plugin is refused as already installed.
            Err(error) => {
                tracing::warn!(target: "plugin.install", plugin = %p.id, %error, "install rollback: config check failed; removing staged tree");
                std::fs::remove_dir_all(&final_dir)
                    .with_context(|| format!("removing {}", final_dir.display()))
            }
        };
        if let Err(error) = rollback {
            tracing::warn!(target: "plugin.install", plugin = %p.id, %error, "install rollback failed");
        }
        return Err(e);
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
        let result = apply_prepared_install(&prepared, &node_env, &OperationLog::Inherit);
        if super::execution::needs_recovery(&result) {
            log_recovery(&result);
            std::mem::forget(prepared);
            std::mem::forget(_operation);
        } else {
            prepared.fetched.close();
        }
        result
    })
    .await
    .context("plugin install task failed")?
}

pub async fn preview_install(input: &str, dev: bool) -> Result<InstallConsent> {
    let p = prepare_install(input, dev, None).await?;
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
        let result = apply_prepared_install(&prepared, &node_env, &log);
        if super::execution::needs_recovery(&result) {
            log_recovery(&result);
            std::mem::forget(prepared);
            std::mem::forget(_operation);
        } else {
            prepared.fetched.close();
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
/// agent commands carry the display-escaped form (control and bidi characters
/// made visible) rather than the raw bytes. The runnable command in the
/// manifest is never altered.
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
        let Some(previous) = previous
            .iter()
            .find(|previous| previous.tool == current.tool)
        else {
            return true;
        };
        current.command.is_some() && previous.command != current.command
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
                "the worker is now a downloaded release binary (opaque, not source-auditable)"
                    .to_string(),
            )
        } else if prior_was_release_binary && !new_is_release_binary {
            Some(
                "the worker is now an in-tree command (was a downloaded release binary)"
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
    replace_and_build(id, &prepared.fetched, &final_dir, node_env, log)?;

    let granted = grant.is_some();
    persist_update(id, &prepared.source_str, grant)?;
    write_lock(
        id,
        &prepared.fetched,
        &prepared.manifest_hash,
        trust_label(prepared.featured_verified, prepared.dev),
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
        let result = apply_prepared(&prepared, grant, &node_env, &OperationLog::Inherit);
        if super::execution::needs_recovery(&result) {
            log_recovery(&result);
            std::mem::forget(prepared);
            std::mem::forget(_operation);
        } else {
            prepared.fetched.close();
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
    let prepared = prepare_update(id, allow_local_source, None).await?;
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
        granted_at: chrono::Utc::now(),
    });
    let node_env = apply_node_env(&prepared.fetched.manifest).await;
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let result = apply_prepared(&prepared, grant, &node_env, &log);
        if super::execution::needs_recovery(&result) {
            log_recovery(&result);
            std::mem::forget(prepared);
            std::mem::forget(_operation);
        } else {
            prepared.fetched.close();
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
    if !is_installed_external(id)? {
        bail!("{id} is not an installed external plugin");
    }
    let version = super::registry()
        .get(id)
        .map(|p| p.manifest.version.clone());
    remove_external(id)?;
    tracing::info!(target: "plugin.install", plugin = %id, ?version, "plugin uninstalled");
    Ok(())
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
        println!("Plugin {id} declares these agent commands (run as your user, unsandboxed):");
        for integration in integrations {
            println!(
                "  {}: {}",
                integration.tool,
                integration
                    .command
                    .as_deref()
                    .map(super::agent_integration::display_command)
                    .unwrap_or_else(|| "No launch command declared".to_string())
            );
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
) -> Result<()> {
    let backup_dir = final_dir.with_file_name(format!("{plugin_id}.bak"));

    if backup_dir.exists() {
        if final_dir.exists() {
            std::fs::remove_dir_all(final_dir).map_err(|error| {
                super::execution::RecoveryRequired(format!(
                    "removing interrupted update {}: {error}",
                    final_dir.display()
                ))
            })?;
        }
        std::fs::rename(&backup_dir, final_dir).map_err(|error| {
            super::execution::RecoveryRequired(format!(
                "recovering interrupted update backup {} to {}: {error}",
                backup_dir.display(),
                final_dir.display()
            ))
        })?;
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
        build_in_place(
            plugin_id,
            final_dir,
            &fetched.manifest,
            node_env,
            log,
            fetched.budget,
        )
    })();

    match place_and_build {
        Ok(()) => {
            if had_prior {
                if let Err(error) = std::fs::remove_dir_all(&backup_dir) {
                    tracing::warn!(target: "plugin.install", plugin = %plugin_id, %error, "could not remove update backup");
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
            if final_dir.exists() {
                std::fs::remove_dir_all(final_dir).map_err(|error| {
                    super::execution::RecoveryRequired(format!(
                        "removing failed update {}: {error}; original failure: {e:#}",
                        final_dir.display()
                    ))
                })?;
            }
            if had_prior {
                std::fs::rename(&backup_dir, final_dir).map_err(|error| {
                    super::execution::RecoveryRequired(format!(
                        "restoring {} from {}: {error}; original failure: {e:#}",
                        final_dir.display(),
                        backup_dir.display()
                    ))
                })?;
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

        let declined = prepare_update("acme.upd", true, None).await.unwrap();
        assert!(declined.needs_consent);
        let err = finish_update(declined, None, begin_operation().unwrap(), provision)
            .await
            .err();
        assert!(err.is_some_and(|e| e.to_string().contains("update cancelled")));
        assert_eq!(provisioned.load(Ordering::SeqCst), 0);
        assert!(!installed.join("built").exists());
        assert_eq!(installed_version(), "0.1.0");

        let approved = prepare_update("acme.upd", true, None).await.unwrap();
        let grant = CapabilityGrant {
            manifest_hash: approved.manifest_hash.clone(),
            capabilities: approved.capabilities.clone(),
            granted_at: chrono::Utc::now(),
        };
        finish_update(approved, Some(grant), begin_operation().unwrap(), provision)
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
        };
        let disclosed = disclosure_integrations(std::slice::from_ref(&raw));
        // The consent that the CLI/TUI/web serialize carries the escaped form.
        assert_eq!(disclosed[0].command.as_deref(), Some("x\\u{202e}tail"));
        // The manifest / launch command stays raw.
        assert_eq!(raw.command.as_deref(), Some("x\u{202e}tail"));
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
        let mut gate = fetch::test_gate::install(&source.path().join("aoe-plugin.toml"), "copy");
        let operation = tokio::spawn(async move {
            apply_install(&input, &consent.fingerprint, false, OperationLog::Inherit).await
        });
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
        let input = source.path().to_str().unwrap();
        std::fs::write(
            source.path().join("aoe-plugin.toml"),
            "id = \"acme.special\"\nname = \"Special\"\nversion = \"1.0.0\"\napi_version = 2\n",
        )
        .unwrap();
        let consent = preview_install(input, false).await.unwrap();
        nix::unistd::mkfifo(&source.path().join("fifo"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            apply_install(input, &consent.fingerprint, false, OperationLog::Inherit),
        )
        .await
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
            prepared.fetched.close();
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
