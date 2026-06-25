//! Plugin enable/disable and external install / update / uninstall.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};

use anyhow::{anyhow, bail, Context, Result};
use aoe_plugin_api::{PluginManifest, TrustLevel};

use crate::session::{save_config, CapabilityGrant, Config, PluginConfig};

use super::fetch::{self, FetchedPlugin};
use super::lockfile::{LockedPlugin, Lockfile};
use super::source::PluginSource;

/// Set the enabled flag for a known plugin id in the global config, then reload
/// the registry so the change takes effect.
pub fn set_enabled(plugin_id: &str, enabled: bool) -> Result<()> {
    let registry = super::registry();
    if registry.get(plugin_id).is_none() {
        bail!("unknown plugin {plugin_id:?}; see `aoe plugin list`");
    }
    enable_in_config(plugin_id, enabled)?;
    super::reload_registry();
    Ok(())
}

fn enable_in_config(plugin_id: &str, enabled: bool) -> Result<()> {
    let mut config = Config::load()?;
    config
        .plugins
        .entry(plugin_id.to_string())
        .or_insert_with(PluginConfig::default)
        .enabled = enabled;
    save_config(&config)
}

/// What an install or update did, for the caller to report.
#[derive(Debug)]
pub struct InstallReport {
    pub id: String,
    pub version: String,
    /// Capabilities the manifest declares.
    pub capabilities: Vec<String>,
    /// Whether the plugin is granted and live after the operation.
    pub granted: bool,
}

/// Install an external plugin from `input` (`gh:owner/repo[@ref]` or a local
/// dir). Prompts once for the manifest's capabilities unless `assume_yes`.
pub async fn install(input: &str, assume_yes: bool) -> Result<InstallReport> {
    let source = PluginSource::parse(input)?;
    let fetched = fetch::fetch(&source).await?;

    let id = fetched.manifest.id.as_str().to_string();
    reject_reserved_or_builtin(&fetched.manifest)?;

    let final_dir = super::plugins_dir()?.join(&id);
    if final_dir.exists() {
        bail!("{id} is already installed; run `aoe plugin update {id}` or uninstall it first");
    }

    let capabilities = capability_strings(&fetched)?;
    let granted = if capabilities.is_empty() || assume_yes {
        true
    } else {
        confirm_capabilities(&id, &capabilities)?
    };
    if !granted {
        bail!("install cancelled; no capabilities were granted");
    }

    move_into_place(&fetched, &final_dir)?;

    let manifest_hash = PluginManifest::hash_bytes(&fetched.manifest_bytes);
    persist_install(
        &persisted_source(&source, input),
        &id,
        &capabilities,
        &manifest_hash,
    )?;
    write_lock(&id, &fetched, &manifest_hash)?;
    super::reload_registry();

    Ok(InstallReport {
        id,
        version: fetched.manifest.version.clone(),
        capabilities,
        granted: true,
    })
}

/// Re-fetch an installed external plugin from its recorded source. A changed
/// capability set re-prompts; until re-approved the plugin's contributions stay
/// inactive (the grant no longer covers the installed manifest).
pub async fn update(id: &str) -> Result<InstallReport> {
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
    let fetched = fetch::fetch(&source).await?;
    if fetched.manifest.id.as_str() != id {
        bail!(
            "source {source_str:?} now resolves to plugin {:?}, not {id}",
            fetched.manifest.id.as_str()
        );
    }
    reject_reserved_or_builtin(&fetched.manifest)?;

    let capabilities = capability_strings(&fetched)?;
    let manifest_hash = PluginManifest::hash_bytes(&fetched.manifest_bytes);

    let prior_caps: BTreeSet<&str> = prior_grant
        .as_ref()
        .map(|g| g.capabilities.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let new_caps: BTreeSet<&str> = capabilities.iter().map(String::as_str).collect();
    let caps_changed = prior_caps != new_caps;

    // Decide the grant BEFORE touching the installed tree, so a declined or
    // non-interactive prompt bails while the old install, config, and lockfile
    // are still consistent. An empty capability set always (re)grants, so an
    // update that drops all capabilities reactivates the plugin even if it was
    // previously left ungranted.
    let grant = if capabilities.is_empty() {
        Some(CapabilityGrant {
            manifest_hash: manifest_hash.clone(),
            capabilities: vec![],
            granted_at: chrono::Utc::now(),
        })
    } else if !caps_changed {
        prior_grant.map(|g| CapabilityGrant {
            manifest_hash: manifest_hash.clone(),
            capabilities: g.capabilities,
            granted_at: g.granted_at,
        })
    } else if confirm_capabilities(id, &capabilities)? {
        Some(CapabilityGrant {
            manifest_hash: manifest_hash.clone(),
            capabilities: capabilities.clone(),
            granted_at: chrono::Utc::now(),
        })
    } else {
        None
    };

    let final_dir = super::plugins_dir()?.join(id);
    move_into_place(&fetched, &final_dir)?;

    let granted = grant.is_some();
    persist_update(id, &source_str, grant)?;
    write_lock(id, &fetched, &manifest_hash)?;
    super::reload_registry();

    if caps_changed && !granted {
        eprintln!(
            "{id} updated but its capability set changed; it stays inactive until you re-approve with `aoe plugin update {id}`."
        );
    }

    Ok(InstallReport {
        id: id.to_string(),
        version: fetched.manifest.version.clone(),
        capabilities,
        granted,
    })
}

/// Remove an installed external plugin: its tree, its config entry, and its
/// lockfile entry.
pub fn uninstall(id: &str) -> Result<()> {
    let dir = super::plugins_dir()?.join(id);
    let mut config = Config::load()?;
    let is_external = config
        .plugins
        .get(id)
        .and_then(|p| p.source.as_ref())
        .is_some();
    if !dir.exists() && !is_external {
        bail!("{id} is not an installed external plugin");
    }

    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    if config.plugins.remove(id).is_some() {
        save_config(&config)?;
    }
    let mut lock = Lockfile::load()?;
    if lock.remove(id) {
        lock.save()?;
    }
    super::reload_registry();
    Ok(())
}

/// Reject a manifest whose id is reserved for first-party plugins (`aoe.*` /
/// `agent-of-empires.*`, lifted only by featured verification in #2364) or
/// collides with a compiled-in builtin.
fn reject_reserved_or_builtin(manifest: &PluginManifest) -> Result<()> {
    let id = manifest.id.as_str();
    if manifest.id.is_reserved_namespace() {
        bail!("plugin id {id:?} uses a reserved namespace (aoe.* / agent-of-empires.*); external plugins cannot");
    }
    if super::registry::is_builtin_id(id) {
        bail!("plugin id {id:?} collides with a builtin plugin");
    }
    Ok(())
}

/// The manifest's capabilities as strings, rejecting any this host does not
/// recognize (never silently granted).
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

/// Prompt the user to grant a plugin's capabilities. Fails on a non-interactive
/// stdin rather than silently granting; the caller can pass `--yes` there.
fn confirm_capabilities(id: &str, capabilities: &[String]) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!(
            "{id} requests capabilities [{}] but stdin is not a terminal; re-run with --yes to grant them",
            capabilities.join(", ")
        );
    }
    println!("Plugin {id} requests these capabilities:");
    for capability in capabilities {
        println!("  - {capability}");
    }
    print!("Grant them and install? [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Move a fetched plugin's staging tree into its final directory.
fn move_into_place(fetched: &FetchedPlugin, final_dir: &std::path::Path) -> Result<()> {
    // The staging tree lives under the plugins dir, so this rename is
    // same-filesystem and atomic. On update, the old dir is replaced.
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

/// The source string to persist for a later `update`. A GitHub source keeps the
/// original `gh:owner/repo[@ref]` so the ref survives; a local source is
/// canonicalized to an absolute path so `update` does not resolve relative to
/// whatever directory happened to be current at install time.
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
    let mut config = Config::load()?;
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
    save_config(&config)
}

fn persist_update(id: &str, source: &str, grant: Option<CapabilityGrant>) -> Result<()> {
    let mut config = Config::load()?;
    let entry = config
        .plugins
        .entry(id.to_string())
        .or_insert_with(PluginConfig::default);
    entry.source = Some(source.to_string());
    entry.grant = grant;
    save_config(&config)
}

fn write_lock(id: &str, fetched: &FetchedPlugin, manifest_hash: &str) -> Result<()> {
    let mut lock = Lockfile::load()?;
    lock.upsert(
        id,
        LockedPlugin {
            source: fetched.source.slug(),
            requested_ref: fetched.requested_ref.clone(),
            resolved_commit: fetched.resolved_commit.clone(),
            version: fetched.manifest.version.clone(),
            manifest_hash: manifest_hash.to_string(),
            trust: TrustLevel::Community.as_str().to_string(),
            release_tag: fetched.release_tag.clone(),
            asset_name: fetched.asset_name.clone(),
            asset_sha256: fetched.asset_sha256.clone(),
        },
    );
    lock.save()
}
