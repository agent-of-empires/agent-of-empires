//! Plugin enable/disable and external install / update / uninstall.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use aoe_plugin_api::{BuildStep, PluginManifest, RuntimeSpec, UiContribution};

use crate::session::{save_config, CapabilityGrant, Config, PluginConfig};

use super::featured::FeaturedIndex;
use super::fetch::{self, FetchedPlugin};
use super::lockfile::{LockedPlugin, Lockfile};
use super::registry::ValidationState;
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
    /// Resolved trust / validation provenance, for the success output.
    pub validation: ValidationState,
}

/// Resolve the display validation for a just-installed plugin, mirroring
/// `registry::validation_for`: a featured-verified source is `Featured`, any
/// other `gh:` source is `Community`, and a local directory is `Local`. The
/// content hash is already verified upstream (`featured_verified`), so this maps
/// from that decision rather than re-hashing the tree.
fn install_validation(featured_verified: bool, source: &str) -> ValidationState {
    if featured_verified {
        ValidationState::Featured
    } else if source.starts_with("gh:") {
        ValidationState::Community
    } else {
        ValidationState::Local
    }
}

/// How an update treats a version that would need fresh consent (changed
/// capabilities, build recipe, or UI slots).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentMode {
    /// Prompt the user (the manual `aoe plugin update` path).
    Interactive,
    /// Apply only a "clean" update that needs no new consent; skip anything that
    /// would (the opt-in startup auto-update sweep). Never prompts, never runs a
    /// changed build step or installs a changed capability/UI set unattended.
    CleanOnlyNonInteractive,
}

/// The result of an update attempt.
#[derive(Debug)]
pub enum UpdateOutcome {
    /// The update was applied (the tree was replaced and the lockfile rewritten).
    Applied(InstallReport),
    /// A `CleanOnlyNonInteractive` update was skipped because it needs consent;
    /// the prior version stays installed and active.
    Skipped { id: String, reason: String },
}

/// Install an external plugin from `input` (`gh:owner/repo[@ref]` or a local
/// dir). Prompts once for the manifest's capabilities unless `assume_yes`.
pub async fn install(input: &str, assume_yes: bool) -> Result<InstallReport> {
    let source = PluginSource::parse(input)?;
    let resolved = resolve_source(source, true).await?;
    eprintln!("{}", resolved.notice);
    if resolved.unverified && !assume_yes && !confirm_unverified()? {
        bail!("install cancelled; the unverified source was not approved");
    }
    let fetched = fetch::fetch(&resolved.source).await?;

    let id = fetched.manifest.id.as_str().to_string();
    let featured_verified = verify_featured(&FeaturedIndex::load()?, &fetched)?;
    reject_reserved_or_builtin(&fetched.manifest, featured_verified)?;
    reject_incompatible_host(&fetched.manifest)?;

    let final_dir = super::plugins_dir()?.join(&id);
    if final_dir.exists() {
        bail!("{id} is already installed; run `aoe plugin update {id}` or uninstall it first");
    }

    let capabilities = capability_strings(&fetched)?;
    let build = build_steps(&fetched.manifest);
    let granted =
        if assume_yes || !install_needs_consent(&capabilities, build, &fetched.manifest.ui) {
            true
        } else {
            confirm_capabilities(&id, &capabilities, &fetched.manifest.ui, build)?
        };
    if !granted {
        bail!("install cancelled; no capabilities were granted");
    }

    move_into_place(&fetched, &final_dir)?;
    if let Err(e) = build_in_place(&id, &final_dir, &fetched.manifest) {
        // A failed build must not leave a half-installed tree behind; nothing
        // is persisted to config or the lockfile, so removing the directory
        // returns the host to its pre-install state.
        let _ = std::fs::remove_dir_all(&final_dir);
        return Err(e);
    }

    let manifest_hash = PluginManifest::hash_bytes(&fetched.manifest_bytes);
    let persisted = persisted_source(&resolved.source, input);
    persist_install(&persisted, &id, &capabilities, &manifest_hash)?;
    write_lock(&id, &fetched, &manifest_hash, featured_verified)?;
    super::reload_registry();

    Ok(InstallReport {
        id,
        version: fetched.manifest.version.clone(),
        capabilities,
        granted: true,
        validation: install_validation(featured_verified, &persisted),
    })
}

/// Re-fetch an installed external plugin from its recorded source, prompting on
/// a changed capability set (the manual `aoe plugin update` path). A changed
/// capability set re-prompts; until re-approved the plugin's contributions stay
/// inactive (the grant no longer covers the installed manifest).
pub async fn update(id: &str) -> Result<InstallReport> {
    match update_with_consent(id, ConsentMode::Interactive).await? {
        UpdateOutcome::Applied(report) => Ok(report),
        // Interactive mode prompts rather than skipping, so this is unreachable;
        // map it to an error rather than panicking if that ever changes.
        UpdateOutcome::Skipped { id, reason } => {
            bail!("update for {id} was skipped unexpectedly: {reason}")
        }
    }
}

/// Re-fetch an installed external plugin and apply it only if it needs no new
/// consent (the opt-in startup auto-update sweep). Returns whether it was
/// applied or skipped; never prompts.
pub async fn update_clean(id: &str) -> Result<UpdateOutcome> {
    update_with_consent(id, ConsentMode::CleanOnlyNonInteractive).await
}

async fn update_with_consent(id: &str, mode: ConsentMode) -> Result<UpdateOutcome> {
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
    // A no-`@ref` install tracks the release channel: re-resolve the latest
    // release each update (rolling). Disallow the default-branch fallback here
    // so an update never silently switches a release-tracking install onto the
    // moving default branch; an explicit `@ref` install keeps following its ref.
    let resolved = resolve_source(source, false).await?;
    eprintln!("{}", resolved.notice);
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

    let prior_caps: BTreeSet<&str> = prior_grant
        .as_ref()
        .map(|g| g.capabilities.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let new_caps: BTreeSet<&str> = capabilities.iter().map(String::as_str).collect();
    let caps_changed = prior_caps != new_caps;

    // Build steps run unsandboxed at update time, so a changed build recipe
    // must re-prompt even when the capability set is unchanged; a static
    // capability list must not let modified build commands run unattended. The
    // manifest hash covers the build steps, so a hash change with build steps
    // present means the recipe could have changed.
    let manifest_changed =
        prior_grant.as_ref().map(|g| g.manifest_hash.as_str()) != Some(manifest_hash.as_str());
    let build_changed = manifest_changed && !build_steps(&fetched.manifest).is_empty();
    // UI contributions are disclosed at install, so an update that changes the
    // manifest while declaring UI slots must re-disclose them: otherwise an
    // update could add new dashboard slots the user never saw. The manifest
    // hash covers the `[[ui]]` section, so a hash change with UI present means
    // the slots could have changed.
    let ui_changed = manifest_changed && !fetched.manifest.ui.is_empty();
    // Prompt when there is something to consent to or disclose: capabilities
    // that changed, build steps that could have changed, or UI slots on a
    // changed manifest. Dropping all capabilities with no build/UI has nothing
    // to grant, so it still (re)grants silently.
    let needs_prompt = (!capabilities.is_empty() && caps_changed) || build_changed || ui_changed;

    // Decide the grant BEFORE touching the installed tree, so a declined or
    // non-interactive prompt bails while the old install, config, and lockfile
    // are still consistent.
    let grant = if needs_prompt {
        // The auto-update sweep declines anything needing consent: skip without
        // touching the tree, leaving the working version active.
        if mode == ConsentMode::CleanOnlyNonInteractive {
            return Ok(UpdateOutcome::Skipped {
                id: id.to_string(),
                reason: skip_reason(caps_changed, build_changed, ui_changed),
            });
        }
        if confirm_capabilities(
            id,
            &capabilities,
            &fetched.manifest.ui,
            build_steps(&fetched.manifest),
        )? {
            Some(CapabilityGrant {
                manifest_hash: manifest_hash.clone(),
                capabilities: capabilities.clone(),
                granted_at: chrono::Utc::now(),
            })
        } else {
            None
        }
    } else if capabilities.is_empty() {
        // Nothing to grant; an empty capability set keeps the plugin active.
        Some(CapabilityGrant {
            manifest_hash: manifest_hash.clone(),
            capabilities: vec![],
            granted_at: chrono::Utc::now(),
        })
    } else {
        // Capabilities unchanged and the build recipe (if any) unchanged: carry
        // the prior grant forward, refreshed to the new manifest hash.
        prior_grant.map(|g| CapabilityGrant {
            manifest_hash: manifest_hash.clone(),
            capabilities: g.capabilities,
            granted_at: g.granted_at,
        })
    };

    let final_dir = super::plugins_dir()?.join(id);
    // A declined prompt must not run the plugin's build steps (arbitrary code
    // the user just refused): abort and keep the prior version. A capabilities
    // decline with no build steps keeps the prior behavior (tree updated, left
    // inactive until re-approved).
    if needs_prompt && grant.is_none() && !build_steps(&fetched.manifest).is_empty() {
        bail!("update cancelled for {id}; build steps were not approved, prior version kept");
    }
    replace_and_build(id, &fetched, &final_dir)?;

    let granted = grant.is_some();
    persist_update(id, &source_str, grant)?;
    write_lock(id, &fetched, &manifest_hash, featured_verified)?;
    super::reload_registry();

    if caps_changed && !granted {
        eprintln!(
            "{id} updated but its capability set changed; it stays inactive until you re-approve with `aoe plugin update {id}`."
        );
    }

    Ok(UpdateOutcome::Applied(InstallReport {
        id: id.to_string(),
        version: fetched.manifest.version.clone(),
        capabilities,
        granted,
        validation: install_validation(featured_verified, &source_str),
    }))
}

/// Human-readable reason an auto-update was skipped, for the sweep log.
fn skip_reason(caps_changed: bool, build_changed: bool, ui_changed: bool) -> String {
    let mut parts = Vec::new();
    if caps_changed {
        parts.push("capability change");
    }
    if build_changed {
        parts.push("build-step change");
    }
    if ui_changed {
        parts.push("UI change");
    }
    if parts.is_empty() {
        "needs approval".to_string()
    } else {
        format!("{} needs approval", parts.join(" + "))
    }
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

/// Reject a manifest that collides with a compiled-in builtin (always) or that
/// claims a reserved first-party namespace (`aoe.*` / `agent-of-empires.*`)
/// without being featured-verified. A featured-verified plugin is the one case
/// allowed into a reserved namespace (#2364).
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

/// Refuse a plugin whose declared `aoe_version` range excludes this host. A
/// plugin author states which aoe versions a plugin version was tested against;
/// installing outside that range invites runtime failure, so block it at
/// install/update with the manifest's own actionable message. The load-time
/// twin in the registry scan skips rather than bails so an aoe upgrade cannot
/// brick startup.
fn reject_incompatible_host(manifest: &PluginManifest) -> Result<()> {
    manifest
        .host_compat(env!("CARGO_PKG_VERSION"))
        .map_err(|msg| anyhow!("{}: {msg}", manifest.id.as_str()))
}

/// What [`resolve_source`] decided to fetch.
struct ResolvedSource {
    /// The source to actually fetch. A no-`@ref` GitHub source is rewritten to
    /// the resolved release tag; everything else is passed through unchanged.
    source: PluginSource,
    /// The install is off the audited-release default path: an explicit `@ref`,
    /// or the no-release default-branch fallback. The install path confirms
    /// before proceeding; update ignores it (it has its own consent flow).
    unverified: bool,
    /// One line stating what is being installed, printed by the caller.
    notice: String,
}

/// Resolve what to fetch for an install or update.
///
/// A no-`@ref` GitHub source installs the latest stable release, the audited
/// default path. With no published release, `allow_branch_fallback` (install)
/// falls back to the default branch as an unverified install; without it
/// (update) that is an error, so a release-tracking install never silently
/// switches onto the moving default branch. An explicit `@ref` is an unverified
/// opt-in fetched as-is; a local source is unchanged.
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

/// Confirm an install that is off the audited-release default path (an explicit
/// ref, or the no-release default-branch fallback). Mirrors
/// [`confirm_capabilities`]: a non-interactive stdin without `--yes` is an
/// error, not a silent yes. The caller has already printed what is being
/// installed, so this only states the trust caveat and prompts.
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

/// Check a fetched plugin against the curated index. Returns whether it is a
/// verified featured plugin.
///
/// If the id is in the index, the install must come from the pinned source slug
/// (case-insensitively, GitHub slugs are not case-sensitive) and must not ship a
/// release-binary worker (its bytes are not covered by the tree hash yet, so a
/// featured pin cannot vouch for them); both are hard errors. The tree hash is
/// checked against the entry's set of vetted release hashes: a match is
/// featured-verified, while an id-in-index but hash-not-vetted install is simply
/// an unvetted version (returns `false`, treated as community) rather than a
/// tamper-refuse. The reserved-namespace gate downstream still blocks an
/// unvetted version of a reserved-namespace plugin.
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

/// Whether an install must prompt for consent rather than auto-grant silently.
/// Capabilities and build steps need a grant; UI contributions need no grant
/// but are disclosed, so a UI-only plugin still prompts rather than installing
/// silently (#2366).
fn install_needs_consent(
    capabilities: &[String],
    build: &[BuildStep],
    ui: &[UiContribution],
) -> bool {
    !capabilities.is_empty() || !build.is_empty() || !ui.is_empty()
}

/// Prompt the user to grant a plugin's capabilities and run any build steps.
/// Fails on a non-interactive stdin rather than silently granting; the caller
/// can pass `--yes` there. Build steps are disclosed verbatim because they run
/// as the user, outside capability enforcement, before the plugin is
/// registered.
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
    // UI contributions are not capabilities (they need no grant), but the user
    // should know the plugin will render into the dashboard before trusting it.
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
    // The honest model (D8): the host enforces these capabilities at its API
    // boundary, which stops a cooperative plugin from overreaching. It does NOT
    // contain an adversarial plugin: a granted worker runs as an ordinary
    // process with no OS-level isolation. Build steps run with the same trust,
    // earlier. State this on every grant prompt.
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

/// The build steps a `command` runtime declares, or an empty slice for any
/// other (or absent) runtime.
fn build_steps(manifest: &PluginManifest) -> &[BuildStep] {
    match &manifest.runtime {
        Some(RuntimeSpec::Command { build, .. }) => build,
        _ => &[],
    }
}

/// Run a plugin's declared build steps in its final directory, then confirm the
/// worker entrypoint is runnable. Builds run in the final directory (not the
/// staging tree) because tools like Python venvs embed absolute paths and are
/// not relocatable, so a build followed by a rename would break the worker.
fn build_in_place(plugin_id: &str, dir: &Path, manifest: &PluginManifest) -> Result<()> {
    run_build(plugin_id, dir, build_steps(manifest))?;
    // A build can succeed by exit code yet not produce the entrypoint (every
    // step skipped on this platform, or a no-op build against a broken
    // project). Resolve the launch command now, while the user is watching, so
    // the failure is a clear install error instead of an opaque launch error
    // the next time the daemon starts.
    //
    // Only for an in-tree entrypoint. A `system = true` worker resolves its
    // program on PATH, and the install shell's PATH is not the daemon's PATH:
    // checking it here neither guarantees the daemon can launch the worker nor
    // should it reject a valid system-tool plugin whose tool is simply absent
    // from the install shell. Leave that entrypoint to resolve at launch.
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

/// Execute build steps sequentially in `dir`. Each step's argv is resolved with
/// the same policy as the launch command, immediately before it runs, so a step
/// like `.venv/bin/pip` resolves once the prior step created it. Build stdin is
/// `/dev/null` so an interactive prompt cannot hang a `--yes` install; stdout
/// and stderr inherit the terminal so the user sees build progress.
fn run_build(plugin_id: &str, dir: &Path, steps: &[BuildStep]) -> Result<()> {
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
        eprintln!("  building {plugin_id}: {pretty}");
        let status = std::process::Command::new(&program)
            .args(&args)
            .current_dir(dir)
            .env("AOE_PLUGIN_ID", plugin_id)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("spawning build step {} ({pretty})", i + 1))?;
        if !status.success() {
            bail!("build step {} ({pretty}) failed with {status}", i + 1);
        }
    }
    Ok(())
}

/// Replace an installed plugin's directory with a freshly fetched tree and run
/// its build, keeping the prior version intact if the build fails.
///
/// A leftover `<id>.bak` means a previous update was interrupted between
/// exposing the new tree and finishing the build, leaving a possibly half-built
/// `<id>`; the backup is the last known-good version, so recover it first.
/// Then move the current install aside, place the new tree, and build: on
/// success drop the backup, on failure restore it so the user is never left
/// worse off than before the update.
fn replace_and_build(plugin_id: &str, fetched: &FetchedPlugin, final_dir: &Path) -> Result<()> {
    // `with_file_name`, not `with_extension`: a plugin id like `acme.worker`
    // has a dot, and `with_extension("bak")` would replace `.worker`, yielding
    // `acme.bak` and colliding with every other `acme.*` plugin's backup.
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
        build_in_place(plugin_id, final_dir, &fetched.manifest)
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
    use aoe_plugin_api::UiSlot;

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
        // The host is this crate's CARGO_PKG_VERSION (a 1.x release); a range
        // bracketing 1.x installs, a future-major-only range is refused with an
        // id-prefixed message. Keep the literals semver-free so this compiles in
        // a TUI-only build, where the host crate's semver dep is serve-gated.
        let in_range = manifest_with_aoe_version(Some(">=1.0.0, <2.0.0"));
        assert!(reject_incompatible_host(&in_range).is_ok());

        let out = manifest_with_aoe_version(Some(">=2.0.0"));
        let err = reject_incompatible_host(&out).unwrap_err().to_string();
        assert!(err.contains("acme.thing"), "{err}");
        assert!(err.contains("plugin requires aoe"), "{err}");

        // No declared range installs everywhere.
        assert!(reject_incompatible_host(&manifest_with_aoe_version(None)).is_ok());
    }

    #[test]
    fn install_consent_required_for_caps_build_or_ui() {
        // Nothing declared: auto-grant is fine.
        assert!(!install_needs_consent(&[], &[], &[]));
        // A capability needs a grant.
        assert!(install_needs_consent(&["net".to_string()], &[], &[]));
        // A UI-only plugin must still prompt so the slots are disclosed (#2366):
        // the regression this guards is auto-granting when only `ui` is set.
        assert!(install_needs_consent(
            &[],
            &[],
            &[ui(UiSlot::StatusBar, "s")]
        ));
    }
}
