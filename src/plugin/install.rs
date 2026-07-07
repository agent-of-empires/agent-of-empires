//! Plugin enable/disable and external install / update / uninstall.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use aoe_plugin_api::{BuildStep, PluginId, PluginManifest, RuntimeSpec, UiContribution};
use serde::Serialize;

use crate::session::{save_config, CapabilityGrant, Config, PluginConfig};

use super::changelog::UpdateChangelog;
use super::featured::FeaturedIndex;
use super::fetch::{self, FetchedPlugin};
use super::lockfile::{LockedPlugin, Lockfile};
use super::registry::ValidationState;
use super::source::PluginSource;

/// Where install / update / uninstall progress and child build output are
/// written. The CLI uses `Inherit` so the user watches build output on their
/// terminal; the dashboard's host-side job path uses `File`, so a dashboard
/// user with no terminal attached can tail the same output.
pub enum OperationLog {
    Inherit,
    File(std::fs::File),
}

impl OperationLog {
    /// Open a job log file in append mode, owner-only (0600 on Unix). Build
    /// output is not secret, but the log lives beside other 0600 daemon state,
    /// so keep the convention.
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

    /// Write one host-side progress line.
    fn line(&self, msg: &str) {
        match self {
            OperationLog::Inherit => eprintln!("{msg}"),
            OperationLog::File(file) => {
                let _ = writeln!(&mut &*file, "{msg}");
            }
        }
    }

    /// stdout and stderr for a child build step: inherited for the CLI, or two
    /// clones of the job log handle so the child's output lands in the tail.
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
    /// the prior version stays installed and active. `fingerprint` identifies the
    /// skipped version so a caller (the auto-update sweep) can compare it to the
    /// user's recorded dismissal and avoid re-nagging.
    Skipped {
        id: String,
        reason: String,
        fingerprint: String,
    },
}

/// One dashboard UI slot a plugin contributes to, in a consent disclosure.
#[derive(Debug, Clone, Serialize)]
pub struct UiView {
    pub slot: String,
    pub id: String,
}

/// The structured disclosure an in-app (web / TUI) update approval renders. The
/// same payload the terminal prompt describes, so every surface consents to the
/// identical change. `fingerprint` pins the exact content the user is approving
/// (the source tree plus any release-binary asset and the trust class), so
/// `apply_update` can refuse if the remote moved since this was shown.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateConsent {
    pub id: String,
    pub from_version: String,
    pub to_version: String,
    /// Capabilities currently granted (the prior approval).
    pub prior_capabilities: Vec<String>,
    /// Capabilities the new manifest declares.
    pub new_capabilities: Vec<String>,
    /// Capabilities present in the new set but not the prior one.
    pub added_capabilities: Vec<String>,
    /// Capabilities present in the prior set but not the new one.
    pub removed_capabilities: Vec<String>,
    /// The dashboard UI slots the new version contributes to.
    pub ui: Vec<UiView>,
    /// Build commands the new version will run, unsandboxed, at apply time.
    pub build_steps: Vec<String>,
    /// A human description when the worker runtime kind changes (e.g. a script
    /// becomes a downloaded release binary), else `None`.
    pub runtime_change: Option<String>,
    /// The plugin was a verified featured plugin and the update no longer is.
    pub trust_downgrade: bool,
    /// Content fingerprint of the version being approved.
    pub fingerprint: String,
    /// Whether declining keeps the current version active (always true for the
    /// in-app path, which never touches the tree on decline).
    pub stays_active_if_declined: bool,
    /// What changed between the installed version and this one, for the user to
    /// review before approving. Best-effort; empty or unavailable on any fetch
    /// failure.
    pub changelog: UpdateChangelog,
}

/// What a non-interactive update preview found for one installed plugin.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdatePreview {
    /// The remote matches the installed content; nothing to do.
    NoUpdate,
    /// A newer version that needs no fresh consent; still returned to the UI for
    /// review so the user sees the changelog before applying.
    SafeUpdate {
        to_version: String,
        fingerprint: String,
        changelog: UpdateChangelog,
    },
    /// A newer version that expands access (capabilities, build steps, UI,
    /// runtime, or trust) and must be explicitly approved. `dismissed` is set
    /// when the user already declined this exact fingerprint. Boxed because the
    /// consent payload dwarfs the other variants.
    ConsentRequired {
        consent: Box<UpdateConsent>,
        dismissed: bool,
    },
}

/// Everything an install needs after fetching and validating, before any
/// consent decision or filesystem mutation. Computing this in one place is what
/// lets the CLI prompt and the in-app preview/apply flow stay in lockstep, the
/// same split `Prepared` gives the update path.
struct PreparedInstall {
    /// Persisted source string for a later `update`.
    persisted_source: String,
    /// The install is off the audited-release default path (an explicit `@ref`
    /// or a no-release default-branch fallback).
    unverified: bool,
    /// One line stating what is being installed.
    notice: String,
    fetched: FetchedPlugin,
    featured_verified: bool,
    id: String,
    capabilities: Vec<String>,
    manifest_hash: String,
    /// Content fingerprint of the fetched version (tree + release asset + trust).
    fingerprint: String,
    validation: ValidationState,
}

/// The structured disclosure an in-app (web / TUI) install approval renders, the
/// same data the terminal prompt prints. `fingerprint` pins the exact content
/// being approved so `apply_install` refuses if the remote moved since this was
/// shown.
#[derive(Debug, Clone, Serialize)]
pub struct InstallConsent {
    pub id: String,
    pub version: String,
    /// Persisted source slug (`gh:owner/repo[@ref]`).
    pub source: String,
    /// One line stating what is being installed (resolved release, ref, etc).
    pub notice: String,
    /// The source is off the audited-release default path (explicit ref or a
    /// no-release default-branch fallback); the dashboard warns on it.
    pub unverified: bool,
    /// Resolved trust class (featured / community / local).
    pub validation: String,
    /// Capabilities the manifest declares (each needs a grant).
    pub capabilities: Vec<String>,
    /// Dashboard UI slots the plugin contributes to.
    pub ui: Vec<UiView>,
    /// Build commands the plugin will run, unsandboxed, at install time.
    pub build_steps: Vec<String>,
    /// Content fingerprint of the version being approved.
    pub fingerprint: String,
}

/// Resolve, fetch, and validate an install candidate without touching the
/// installed tree. Network-only. The caller decides consent (CLI prompt or web
/// preview/apply) and then calls [`apply_prepared_install`].
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

/// Move the fetched tree into place, build it, and persist the grant, config,
/// and lockfile. Consent is already decided by the caller; build output goes to
/// `log`. A failed build leaves nothing behind.
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
        // A failed build must not leave a half-installed tree behind; nothing
        // is persisted to config or the lockfile, so removing the directory
        // returns the host to its pre-install state.
        let _ = std::fs::remove_dir_all(&final_dir);
        return Err(e);
    }

    // Persist config and lockfile together; if either fails, roll the install
    // back so a half-written config/lock and an untracked tree do not block
    // retries with "already installed" or leave the two out of sync.
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

/// Install an external plugin from `input` (`gh:owner/repo[@ref]` or a local
/// dir). Prompts once for the manifest's capabilities unless `assume_yes`.
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

/// Classify an install candidate for an in-app approval without installing it:
/// the dashboard "what would this install do" probe. Network-only. Restricted
/// to `gh:` sources so a browser request never makes the daemon read an
/// arbitrary local path; local installs stay on the CLI.
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

/// Apply an install previewed in-app, granting whatever the fetched manifest
/// declares. `expected_fingerprint` pins the exact content the user approved: if
/// the remote moved since the preview, this refuses rather than installing
/// something the user never saw. Build output goes to `log`. `gh:` only, like
/// [`preview_install`].
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

/// Re-fetch an installed external plugin from its recorded source, prompting on
/// a changed capability set (the manual `aoe plugin update` path). A changed
/// capability set re-prompts; until re-approved the plugin's contributions stay
/// inactive (the grant no longer covers the installed manifest).
pub async fn update(id: &str) -> Result<InstallReport> {
    match update_with_consent(id, ConsentMode::Interactive).await? {
        UpdateOutcome::Applied(report) => Ok(report),
        // Interactive mode prompts rather than skipping, so this is unreachable;
        // map it to an error rather than panicking if that ever changes.
        UpdateOutcome::Skipped { id, reason, .. } => {
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

/// Everything an update needs after fetching and diffing, before any decision
/// about whether to apply it. Computing the consent decision in exactly one
/// place is what lets the CLI prompt, the in-app preview/apply flow, and the
/// auto-update sweep stay in lockstep instead of drifting apart.
struct Prepared {
    id: String,
    source_str: String,
    fetched: FetchedPlugin,
    featured_verified: bool,
    prior_grant: Option<CapabilityGrant>,
    capabilities: Vec<String>,
    manifest_hash: String,
    /// Content fingerprint of the fetched version (tree + release asset + trust).
    fingerprint: String,
    /// Content fingerprint of the currently installed version, from the
    /// lockfile; `None` when no lock entry exists.
    prior_fingerprint: Option<String>,
    /// The installed source ref and resolved commit (from the lockfile), and the
    /// fetched target ref and commit. Drive the changelog assembly in
    /// `preview_update`. `requested_ref` is the source tag/branch (a no-`@ref`
    /// release install resolves to the release tag here); `release_tag` is NOT
    /// used (it is release-binary asset provenance only).
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

/// A content fingerprint of an installed or fetched version: the source tree
/// hash, the release-binary asset hash (whose bytes the tree hash does not
/// cover), and the trust class. This pins exactly what a consent approval
/// covers, so a preview cannot be applied if the remote moved underneath it: a
/// manifest hash alone would miss a `build.sh` or worker script changing under
/// an unchanged `aoe-plugin.toml`, which run unsandboxed at apply time.
fn fingerprint(tree_hash: &str, asset_sha256: Option<&str>, trust: &str) -> String {
    format!("{tree_hash}|{}|{trust}", asset_sha256.unwrap_or(""))
}

/// Fetch an installed plugin's recorded source and diff it against what is on
/// disk, classifying whether the update needs fresh consent. Network-only: it
/// never touches the installed tree.
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

    // The lockfile is the source of truth for what is installed on disk.
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
    // Captured before `fetched` is moved into the returned struct. The source
    // ref is `requested_ref` (a no-`@ref` release install resolved its tag into
    // this field), never `release_tag`.
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

    // Build steps run unsandboxed at apply time, so a changed recipe must
    // re-prompt. A manifest hash misses a build script changing under an
    // unchanged `aoe-plugin.toml`, so key this on the source tree hash: if the
    // tree moved and the new version declares build steps, the recipe could have
    // changed. Fall back to the manifest-hash heuristic only when no prior tree
    // hash is recorded (a pre-v2 lock).
    let manifest_changed =
        prior_grant.as_ref().map(|g| g.manifest_hash.as_str()) != Some(manifest_hash.as_str());
    let tree_changed = if prior_tree_hash.is_empty() {
        manifest_changed
    } else {
        prior_tree_hash != fetched.tree_hash
    };
    let build_changed = tree_changed && !build_steps(&fetched.manifest).is_empty();
    // UI contributions are disclosed at install, so an update that changes the
    // manifest while declaring UI slots must re-disclose them: otherwise an
    // update could add dashboard slots the user never saw.
    let ui_changed = manifest_changed && !fetched.manifest.ui.is_empty();
    // A worker that switches between an in-tree command and a downloaded release
    // binary is a meaningful change in auditability, even when capabilities are
    // unchanged; disclose and re-prompt for it.
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
    // A plugin that was a verified featured plugin and no longer is has lost the
    // curated-index vouch; treat the downgrade as consent-worthy.
    let trust_downgrade = prior_trust == "featured" && !featured_verified;

    // Re-prompt when there is something to consent to or disclose: capabilities
    // that changed, a build recipe that could have changed, UI slots on a
    // changed manifest, a runtime-kind switch, or a trust downgrade. Dropping
    // all capabilities with no other trigger has nothing to grant, so it still
    // (re)grants silently.
    let needs_consent = (!capabilities.is_empty() && caps_changed)
        || build_changed
        || ui_changed
        || runtime_change.is_some()
        || trust_downgrade;

    Ok(Prepared {
        id: id.to_string(),
        source_str,
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

/// Apply a prepared update with an already-decided grant: replace the tree, run
/// the build, persist the grant and lockfile, and reload the registry. A `None`
/// grant declines a consent-required update: it leaves the previously trusted
/// version active without rewriting the tree or lockfile, matching the in-app
/// decline. (Arbitrary build steps the user just refused must never run, and a
/// declined capability expansion must not silently replace the install.)
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

    // Decide the grant BEFORE touching the installed tree, so a declined or
    // non-interactive prompt bails while the old install, config, and lockfile
    // are still consistent.
    let grant = if prepared.needs_consent {
        // The auto-update sweep declines anything needing consent: skip without
        // touching the tree, leaving the working version active.
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
        // Nothing to grant; an empty capability set keeps the plugin active.
        Some(CapabilityGrant {
            manifest_hash: prepared.manifest_hash.clone(),
            capabilities: vec![],
            granted_at: chrono::Utc::now(),
        })
    } else {
        // Capabilities unchanged and the build recipe (if any) unchanged: carry
        // the prior grant forward, refreshed to the new manifest hash.
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

/// Build the structured consent disclosure from a prepared update. The
/// changelog is assembled by the caller (`preview_update`) and passed in, so the
/// disclosure stays free of network work.
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

/// Assemble the changelog for a prepared update from its prior/target refs and
/// commits. Best-effort and network-bound; only called from `preview_update`.
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

/// Classify an available update for one installed plugin without applying it:
/// the in-app (web / TUI) "what would this update do" probe. Network-only.
pub async fn preview_update(id: &str) -> Result<UpdatePreview> {
    let prepared = prepare_update(id).await?;
    if prepared.prior_fingerprint.as_ref() == Some(&prepared.fingerprint) {
        return Ok(UpdatePreview::NoUpdate);
    }
    // One best-effort changelog fetch, behind this explicit user-driven preview.
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

/// Apply an update that was previewed in-app, granting whatever the fetched
/// manifest declares. `expected_fingerprint` pins the exact content the user
/// approved: if the remote moved since the preview, this refuses rather than
/// silently granting something the user never saw. A capability-expanding update
/// MUST carry a fingerprint, so approval cannot bypass the stale-preview guard;
/// a safe update may omit it. Clears any recorded dismissal on success (via
/// `persist_update`).
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
        // A consent-needing update must be pinned to what the user reviewed;
        // refuse an unpinned approval rather than grant blind.
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

/// Record that the user declined an available update by its fingerprint, so the
/// popup and the auto-update notification stop nagging until the next version.
pub fn dismiss_update(id: &str, fingerprint: &str) -> Result<()> {
    let mut config = Config::load()?;
    let entry = config
        .plugins
        .entry(id.to_string())
        .or_insert_with(PluginConfig::default);
    if entry.source.is_none() {
        bail!("{id} is not an installed external plugin");
    }
    entry.dismissed_update = Some(fingerprint.to_string());
    save_config(&config)
}

/// Human-readable reason an auto-update was skipped, for the sweep log and the
/// in-app notification.
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

/// Remove an installed external plugin: its tree, its config entry, and its
/// lockfile entry.
pub fn uninstall(id: &str) -> Result<()> {
    PluginId::new(id.to_string()).map_err(|e| anyhow!("{e}"))?;
    let mut config = Config::load()?;
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

/// [`uninstall`] with progress written to `log`, for the dashboard job path so a
/// terminal-less user gets a readable tail that ends in success or failure.
pub fn uninstall_logged(id: &str, log: &OperationLog) -> Result<()> {
    log.line(&format!("uninstalling {id}"));
    uninstall(id)?;
    log.line(&format!("uninstalled {id}"));
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
fn build_in_place(
    plugin_id: &str,
    dir: &Path,
    manifest: &PluginManifest,
    log: &OperationLog,
) -> Result<()> {
    run_build(plugin_id, dir, build_steps(manifest), log)?;
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
/// and stderr go to `log` (the terminal for the CLI, or the job log file for a
/// dashboard install) so the user sees build progress either way.
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

/// Replace an installed plugin's directory with a freshly fetched tree and run
/// its build, keeping the prior version intact if the build fails.
///
/// A leftover `<id>.bak` means a previous update was interrupted between
/// exposing the new tree and finishing the build, leaving a possibly half-built
/// `<id>`; the backup is the last known-good version, so recover it first.
/// Then move the current install aside, place the new tree, and build: on
/// success drop the backup, on failure restore it so the user is never left
/// worse off than before the update.
fn replace_and_build(
    plugin_id: &str,
    fetched: &FetchedPlugin,
    final_dir: &Path,
    log: &OperationLog,
) -> Result<()> {
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
    // The applied version is no longer "the update the user declined"; clear any
    // stale dismissal so a later update is surfaced normally.
    entry.dismissed_update = None;
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
    use serial_test::serial;
    use std::ffi::OsString;

    struct EnvVarGuard {
        key: &'static str,
        old: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn isolated_app_env() -> (tempfile::TempDir, EnvVarGuard, EnvVarGuard) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let xdg = tmp.path().join("xdg");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let home_guard = EnvVarGuard::set("HOME", &home);
        let xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", &xdg);
        (tmp, home_guard, xdg_guard)
    }

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

    #[tokio::test]
    async fn web_install_rejects_non_gh_sources() {
        // The web install path is gh: only, so a browser request can never make
        // the daemon read an arbitrary local path. Both must bail before any
        // network or filesystem work, with a message naming the gh: constraint.
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

    #[test]
    #[serial]
    fn uninstall_rejects_malformed_id_before_touching_plugin_dir() {
        let (_tmp, _home, _xdg) = isolated_app_env();
        let plugins = super::super::plugins_dir().unwrap();
        let jobs = plugins.join("jobs");
        let keep = plugins.join("keepdir");
        std::fs::create_dir_all(&jobs).unwrap();
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::write(keep.join("keep.log"), b"keep").unwrap();

        let err = uninstall("jobs/../keepdir").unwrap_err().to_string();
        assert!(err.contains("invalid plugin id"), "{err}");
        assert!(
            keep.join("keep.log").exists(),
            "malformed ids must not remove plugin subdirectories"
        );
    }

    #[test]
    #[serial]
    fn uninstall_requires_external_plugin_config_before_removing_dir() {
        let (_tmp, _home, _xdg) = isolated_app_env();
        let plugins = super::super::plugins_dir().unwrap();
        let jobs = plugins.join("jobs");
        std::fs::create_dir_all(&jobs).unwrap();
        std::fs::write(jobs.join("keep.log"), b"keep").unwrap();

        let err = uninstall("jobs").unwrap_err().to_string();
        assert!(err.contains("not an installed external plugin"), "{err}");
        assert!(
            jobs.join("keep.log").exists(),
            "uninstall should not remove non-plugin subdirectories"
        );
    }

    #[test]
    #[serial]
    fn uninstall_external_plugin_cleans_config_even_when_dir_is_missing() {
        let (_tmp, _home, _xdg) = isolated_app_env();
        let mut config = Config::default();
        config.plugins.insert(
            "acme.thing".to_string(),
            PluginConfig {
                source: Some("gh:acme/thing".to_string()),
                ..PluginConfig::default()
            },
        );
        save_config(&config).unwrap();

        uninstall("acme.thing").unwrap();

        let config = Config::load().unwrap();
        assert!(!config.plugins.contains_key("acme.thing"));
    }
}
