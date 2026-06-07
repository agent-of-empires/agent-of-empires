//! Repository-level configuration (`.agent-of-empires/config.toml`)
//!
//! Allows repos to define hooks and override session/sandbox/worktree settings.
//! Settings that are personal/global (theme, updates, tmux) are intentionally
//! not overridable at the repo level.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Progress messages streamed from hook execution.
#[derive(Debug, Clone)]
pub enum HookProgress {
    /// A new hook command is starting.
    Started(String),
    /// A line of stdout/stderr output from the running hook.
    Output(String),
}

use super::config::Config;
use super::profile_config::ProfileConfig;
use super::project_mcp::ProjectMcpServer;

/// Config sections a repo `.agent-of-empires/config.toml` may override.
/// Personal/global sections (theme, status_hooks, acp, web, logging, host
/// environment) are intentionally excluded, matching the historical typed
/// `RepoConfig` that simply had no field for them.
const REPO_OVERRIDABLE_SECTIONS: &[&str] = &[
    "hooks", "session", "sandbox", "worktree", "updates", "tmux", "sound",
];

/// Repository-level configuration loaded from `.agent-of-empires/config.toml`.
///
/// Stored as a sparse override tree like [`ProfileConfig`] (#1692): section
/// tables keyed by config-section name. Only [`REPO_OVERRIDABLE_SECTIONS`] are
/// honored; any other section is dropped on merge and on save, so a repo can
/// never override personal/global settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(flatten)]
    pub overrides: serde_json::Map<String, serde_json::Value>,
}

impl RepoConfig {
    /// The lifecycle hooks section parsed into a [`HooksConfig`], if present.
    /// Used by the trust system to hash and display the repo's hook commands.
    pub fn hooks(&self) -> Option<HooksConfig> {
        self.overrides
            .get("hooks")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// The overrides restricted to repo-allowed sections, as a JSON object.
    fn allowed_overrides(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.overrides
                .iter()
                .filter(|(k, _)| REPO_OVERRIDABLE_SECTIONS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }
}

/// Hook commands to run at various lifecycle points.
///
/// Failure semantics differ by hook type:
/// - `on_create`: failures abort session creation (hard failure).
/// - `on_launch`: failures are logged as warnings but do not prevent the session
///   from starting, since blocking an existing session on a transient hook failure
///   would be disruptive.
/// - `on_destroy`: failures are logged as warnings but do not prevent session
///   deletion. Runs before worktree/sandbox cleanup so resources are still
///   available for teardown commands (e.g. `docker-compose down`).
///
/// All fields accept either a single string or an array of strings in TOML:
///   `on_launch = "npm start"`  or  `on_launch = ["npm install", "npm start"]`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    /// Commands run once when a session is first created.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    pub on_create: Vec<String>,

    /// Commands run every time a session starts (failures are non-fatal).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    pub on_launch: Vec<String>,

    /// Commands run when a session is deleted (failures are non-fatal).
    /// Executed before worktree and sandbox cleanup.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::serde_helpers::string_or_vec"
    )]
    pub on_destroy: Vec<String>,
}

impl HooksConfig {
    pub fn is_empty(&self) -> bool {
        self.on_create.is_empty() && self.on_launch.is_empty() && self.on_destroy.is_empty()
    }
}

/// Path to the repo config file relative to the project root.
const REPO_CONFIG_PATH: &str = ".agent-of-empires/config.toml";

/// Legacy path (pre-1.1) for backwards compatibility.
const LEGACY_REPO_CONFIG_PATH: &str = ".aoe/config.toml";

/// Load repo config from `<project_path>/.agent-of-empires/config.toml`.
/// Falls back to the legacy `.aoe/config.toml` path with a deprecation warning.
/// Returns `None` if neither file exists.
pub fn load_repo_config(project_path: &Path) -> Result<Option<RepoConfig>> {
    let config_path = project_path.join(REPO_CONFIG_PATH);
    let (config_path, is_legacy) = if config_path.exists() {
        (config_path, false)
    } else {
        let legacy_path = project_path.join(LEGACY_REPO_CONFIG_PATH);
        if legacy_path.exists() {
            (legacy_path, true)
        } else {
            return Ok(None);
        }
    };

    if is_legacy {
        tracing::warn!(target: "session.store",
            "Found repo config at legacy path .aoe/config.toml -- please rename to .agent-of-empires/config.toml"
        );
    }

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    if content.trim().is_empty() {
        return Ok(None);
    }

    let config: RepoConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", config_path.display()))?;

    // Type-check the repo overrides by merging onto a default Config (the sparse
    // map accepts any JSON). A wrong-typed value surfaces as a load error here
    // rather than a merge-time panic; the caller degrades to profile config.
    super::profile_config::validate_overrides_typecheck(&config.allowed_overrides())
        .with_context(|| format!("Invalid override in {}", config_path.display()))?;

    Ok(Some(config))
}

/// Save repo config to `<project_path>/.agent-of-empires/config.toml`.
/// Creates the `.agent-of-empires/` directory if it does not exist.
/// If a legacy `.aoe/config.toml` exists, it is removed after a successful save
/// to prevent stale config from silently reactivating.
pub fn save_repo_config(project_path: &Path, config: &RepoConfig) -> Result<()> {
    let config_dir = project_path.join(".agent-of-empires");
    fs::create_dir_all(&config_dir)
        .with_context(|| format!("Failed to create {}", config_dir.display()))?;

    let config_path = project_path.join(REPO_CONFIG_PATH);
    let content = toml::to_string_pretty(config)
        .with_context(|| "Failed to serialize repo config".to_string())?;

    super::atomic_write(&config_path, content.as_bytes())
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    // Clean up legacy .aoe/config.toml to prevent stale config from reactivating
    let legacy_config = project_path.join(LEGACY_REPO_CONFIG_PATH);
    if legacy_config.exists() {
        if let Err(e) = fs::remove_file(&legacy_config) {
            tracing::warn!(target: "session.store", "Failed to remove legacy {}: {}", legacy_config.display(), e);
        } else {
            tracing::info!(target: "session.store", "Removed legacy .aoe/config.toml after migrating to .agent-of-empires/");
        }
        // Also remove the .aoe/ directory if it's now empty
        let legacy_dir = project_path.join(".aoe");
        if legacy_dir.exists() {
            let _ = fs::remove_dir(&legacy_dir); // only succeeds if empty
        }
    }

    Ok(())
}

/// Merge repo config overrides into an already-resolved config (global + profile).
///
/// Routes through the generic sparse-JSON merge (#1692): the repo's
/// allowed-section overrides are applied onto the serialized config. Object
/// keys recurse and scalars/arrays replace, matching the legacy per-field merge
/// (empty hook lists never serialize, so they inherit rather than wipe).
pub fn merge_repo_config(config: Config, repo: &RepoConfig) -> Config {
    super::profile_config::merge_configs_generic(&config, &repo.allowed_overrides())
}

/// Filter a sparse override map to the repo-allowed sections.
fn repo_overridable_overrides(
    overrides: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    overrides
        .iter()
        .filter(|(k, _)| REPO_OVERRIDABLE_SECTIONS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Convert a RepoConfig into a ProfileConfig for TUI editing.
/// This allows the settings TUI to reuse the same field infrastructure
/// for all three scopes (Global, Profile, Repo). Only repo-allowed sections
/// carry over so the TUI never shows a personal/global field as repo-overridden.
pub fn repo_config_to_profile(repo: &RepoConfig) -> ProfileConfig {
    ProfileConfig {
        description: None,
        overrides: repo_overridable_overrides(&repo.overrides),
    }
}

/// Convert a ProfileConfig back into a RepoConfig after TUI editing. Sections
/// the repo may not override (theme, acp, ...) are dropped here, matching
/// the historical behavior where editing them in Repo scope was discarded.
pub fn profile_to_repo_config(profile: &ProfileConfig) -> RepoConfig {
    RepoConfig {
        overrides: repo_overridable_overrides(&profile.overrides),
    }
}

/// For worktrees, `.agent-of-empires/config.toml` lives in the main repo, not
/// the worktree dir. Resolve the main repo path so repo config follows the
/// session regardless of which checkout it was created from.
///
/// Only attempts the lookup when `project_path` itself has a `.git` entry,
/// matching the guard in `compute_volume_paths` (avoids `Repository::discover`
/// walking up to an unrelated ancestor repo, e.g. a dotfile-managed `$HOME`).
pub fn repo_config_source_path(project_path: &Path) -> PathBuf {
    if project_path.join(".git").exists() {
        if let Ok(main_repo) = crate::git::GitWorktree::find_main_repo(project_path) {
            return main_repo;
        }
    }
    project_path.to_path_buf()
}

/// Resolve config with repo overrides: global -> profile -> repo.
pub fn resolve_config_with_repo(profile: &str, project_path: &Path) -> Result<Config> {
    let config = super::profile_config::resolve_config(profile)?;
    let config_path = repo_config_source_path(project_path);

    match load_repo_config(&config_path)? {
        Some(repo_config) => Ok(merge_repo_config(config, &repo_config)),
        None => Ok(config),
    }
}

/// Like [`resolve_config_with_repo`], but logs a warning on failure and falls
/// back gracefully instead of propagating the error: a malformed repo config
/// degrades to the profile-merged config (preserving profile customization),
/// and a malformed profile config degrades to defaults.
pub fn resolve_config_with_repo_or_warn(profile: &str, project_path: &Path) -> Config {
    let base = super::profile_config::resolve_config_or_warn(profile);
    let config_path = repo_config_source_path(project_path);
    match load_repo_config(&config_path) {
        Ok(Some(repo_config)) => merge_repo_config(base, &repo_config),
        Ok(None) => base,
        Err(e) => {
            tracing::warn!(target: "session.store",
                "Failed to load repo config at '{}', falling back to profile config: {e}",
                config_path.display()
            );
            base
        }
    }
}

// ---------------------------------------------------------------------------
// Hook trust system
// ---------------------------------------------------------------------------

/// A single trusted repo entry. A row may carry hook trust, project-MCP trust,
/// or both, recorded independently so trusting one surface never silently
/// re-authorizes a stale version of the other (#1985). `hooks_hash` was a bare
/// `String` before MCP trust existed; it is now `Option<String>` so a repo
/// trusted only for its `.mcp.json` (no hooks) is representable. Existing
/// on-disk rows with a string `hooks_hash` deserialize as `Some`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustedRepo {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hooks_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mcp_hash: Option<String>,
    trusted_at: String,
}

/// Top-level structure for `trusted_repos.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TrustedRepos {
    #[serde(default)]
    repos: Vec<TrustedRepo>,
}

/// Compute a SHA-256 hash of the hook commands for change detection.
pub fn compute_hooks_hash(hooks: &HooksConfig) -> String {
    let mut hasher = Sha256::new();
    for cmd in &hooks.on_create {
        hasher.update(b"on_create:");
        hasher.update(cmd.as_bytes());
        hasher.update(b"\n");
    }
    for cmd in &hooks.on_launch {
        hasher.update(b"on_launch:");
        hasher.update(cmd.as_bytes());
        hasher.update(b"\n");
    }
    for cmd in &hooks.on_destroy {
        hasher.update(b"on_destroy:");
        hasher.update(cmd.as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
}

/// Path to the global trust store. Trust decisions are shared across all
/// profiles so that a repo trusted in one profile doesn't require re-approval
/// in another.
fn trusted_repos_path() -> Result<PathBuf> {
    Ok(super::get_app_dir()?.join("trusted_repos.toml"))
}

fn load_trusted_repos() -> Result<TrustedRepos> {
    let path = trusted_repos_path()?;
    if !path.exists() {
        return Ok(TrustedRepos::default());
    }
    let content = fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(TrustedRepos::default());
    }
    Ok(toml::from_str(&content)?)
}

/// Normalize a path by canonicalizing it, with fallback to the original string.
fn normalize_path(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

/// Check whether a repo is trusted for the given surface fingerprints. Each
/// argument is per-surface: `Some(hash)` requires the stored entry to match that
/// hash for that surface; `None` means "do not care about this surface". A repo
/// with no stored row is never trusted. So the supervisor's MCP gate calls
/// `is_repo_trusted(path, None, Some(&mcp_hash))`, while the hook lifecycle
/// calls `is_repo_trusted(path, Some(&hooks_hash), None)`. Normalizes
/// `project_path` before lookup.
pub fn is_repo_trusted(
    project_path: &Path,
    hooks_hash: Option<&str>,
    mcp_hash: Option<&str>,
) -> Result<bool> {
    let normalized = normalize_path(project_path);
    is_repo_trusted_normalized(&normalized, hooks_hash, mcp_hash)
}

/// Does the stored surface hash satisfy the requested one? `None` requested
/// means the surface is not being checked; `Some` requires an exact match.
fn surface_matches(stored: Option<&str>, want: Option<&str>) -> bool {
    match want {
        None => true,
        Some(w) => stored == Some(w),
    }
}

/// Like `is_repo_trusted` but expects an already-normalized path.
fn is_repo_trusted_normalized(
    normalized_path: &str,
    hooks_hash: Option<&str>,
    mcp_hash: Option<&str>,
) -> Result<bool> {
    let trusted = load_trusted_repos()?;
    Ok(trusted.repos.iter().any(|r| {
        r.path == normalized_path
            && surface_matches(r.hooks_hash.as_deref(), hooks_hash)
            && surface_matches(r.mcp_hash.as_deref(), mcp_hash)
    }))
}

/// Record trust for a repo, per surface. `Some(hash)` writes (or updates) that
/// surface's trust; `None` preserves whatever was already stored for it, so
/// approving project MCP never wipes an existing hook-trust record and vice
/// versa. A single approval dialog passes `Some` for every surface it showed.
///
/// Uses file locking to prevent concurrent writes from clobbering each other
/// (e.g. multiple sessions being created simultaneously). Writes through the
/// locked file handle to ensure the lock is effective.
pub fn trust_repo(
    project_path: &Path,
    hooks_hash: Option<&str>,
    mcp_hash: Option<&str>,
) -> Result<()> {
    use fs2::FileExt;
    use std::io::{Read, Seek, SeekFrom, Write};

    let normalized = normalize_path(project_path);
    let path = trusted_repos_path()?;

    // Ensure the file exists so we can lock it
    if !path.exists() {
        fs::write(&path, "")?;
    }

    let mut lock_file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
    lock_file
        .lock_exclusive()
        .context("Failed to acquire lock on trusted_repos.toml")?;

    // Read through the locked handle to avoid a separate file descriptor race
    let mut content = String::new();
    lock_file.read_to_string(&mut content)?;

    let mut trusted: TrustedRepos = if content.trim().is_empty() {
        TrustedRepos::default()
    } else {
        toml::from_str(&content).context("Failed to parse trusted_repos.toml")?
    };

    // Preserve the surface not being updated by carrying its existing hash.
    let existing = trusted.repos.iter().find(|r| r.path == normalized);
    let hooks_final = hooks_hash
        .map(str::to_string)
        .or_else(|| existing.and_then(|e| e.hooks_hash.clone()));
    let mcp_final = mcp_hash
        .map(str::to_string)
        .or_else(|| existing.and_then(|e| e.mcp_hash.clone()));

    trusted.repos.retain(|r| r.path != normalized);

    trusted.repos.push(TrustedRepo {
        path: normalized,
        hooks_hash: hooks_final,
        mcp_hash: mcp_final,
        trusted_at: chrono::Utc::now().to_rfc3339(),
    });

    let new_content = toml::to_string_pretty(&trusted)?;
    lock_file.seek(SeekFrom::Start(0))?;
    lock_file.set_len(0)?;
    lock_file.write_all(new_content.as_bytes())?;

    Ok(())
}

/// Trust state of one reviewable surface (lifecycle hooks or project MCP). Each
/// surface is resolved independently so an untrusted project-MCP file never
/// suppresses already-trusted hooks, and vice versa (#1985).
pub enum TrustSurface<T> {
    /// Surface absent on disk; nothing to trust.
    Absent,
    /// On-disk surface matches the stored trust hash.
    Trusted(T),
    /// Present but unapproved at its current fingerprint.
    NeedsTrust { config: T, hash: String },
}

impl<T> TrustSurface<T> {
    pub fn needs_trust(&self) -> bool {
        matches!(self, TrustSurface::NeedsTrust { .. })
    }

    /// The config when the surface is trusted, else `None`. Used by the hook
    /// lifecycle callers that only run already-trusted hooks.
    pub fn trusted(self) -> Option<T> {
        match self {
            TrustSurface::Trusted(config) => Some(config),
            _ => None,
        }
    }
}

/// Combined trust state for a repo: hooks plus project MCP, both keyed on the
/// same normalized main-repo `project_path`. `.mcp.json` is read from the main
/// repo (the same source as hooks), so the file reviewed in the trust dialog is
/// exactly the file the supervisor later forwards; per-worktree `.mcp.json`
/// divergence is intentionally not supported (use the per-profile layer).
pub struct RepoTrust {
    pub project_path: String,
    pub hooks: TrustSurface<HooksConfig>,
    pub mcp: TrustSurface<Vec<ProjectMcpServer>>,
}

impl RepoTrust {
    /// True when any surface needs interactive approval.
    pub fn needs_prompt(&self) -> bool {
        self.hooks.needs_trust() || self.mcp.needs_trust()
    }
}

/// Check combined repo trust for a project path: hook commands from
/// `.agent-of-empires/config.toml` and MCP servers from `.mcp.json`, both read
/// from the main repo (resolved from a worktree path) and checked against the
/// stored per-surface hashes.
pub fn check_repo_trust(project_path: &Path) -> Result<RepoTrust> {
    let source = repo_config_source_path(project_path);
    let normalized = normalize_path(&source);
    let trusted = load_trusted_repos()?;
    let row = trusted.repos.iter().find(|r| r.path == normalized);

    let hooks = match load_repo_config(Path::new(&normalized))?.and_then(|rc| rc.hooks()) {
        Some(h) if !h.is_empty() => {
            let hash = compute_hooks_hash(&h);
            if row.and_then(|r| r.hooks_hash.as_deref()) == Some(hash.as_str()) {
                TrustSurface::Trusted(h)
            } else {
                TrustSurface::NeedsTrust { config: h, hash }
            }
        }
        _ => TrustSurface::Absent,
    };

    // MCP load errors must not suppress hook trust: a malformed `.mcp.json`
    // here would otherwise make the whole call fail and silently drop
    // already-trusted hooks. Treat a load error as "no project MCP" (the
    // supervisor is the real MCP gate and logs/skips a broken file at spawn).
    let servers = super::project_mcp::load_project_mcp_servers(Path::new(&normalized))
        .unwrap_or_else(|e| {
            tracing::warn!(
                target: "session.store",
                path = %normalized,
                error = %e,
                "failed to load project .mcp.json for trust; treating as absent"
            );
            Vec::new()
        });
    let mcp = if servers.is_empty() {
        TrustSurface::Absent
    } else {
        let hash = super::project_mcp::fingerprint(&servers);
        if row.and_then(|r| r.mcp_hash.as_deref()) == Some(hash.as_str()) {
            TrustSurface::Trusted(servers)
        } else {
            TrustSurface::NeedsTrust {
                config: servers,
                hash,
            }
        }
    };

    Ok(RepoTrust {
        project_path: normalized,
        hooks,
        mcp,
    })
}

// ---------------------------------------------------------------------------
// Hook resolution helpers (shared by CLI and TUI)
// ---------------------------------------------------------------------------

/// Resolve hooks from global+profile config when no repo hooks are defined.
/// Returns `None` if no on_create or on_launch hooks are configured.
pub fn resolve_global_profile_hooks(profile: &str) -> Option<HooksConfig> {
    let config = super::profile_config::resolve_config_or_warn(profile);
    if config.hooks.on_create.is_empty() && config.hooks.on_launch.is_empty() {
        None
    } else {
        Some(config.hooks)
    }
}

/// Merge trusted repo hooks onto the global+profile base config.
/// Repo hooks override (not append) global hooks per-field.
/// Returns `None` if the merged result has no on_create or on_launch hooks.
pub fn merge_hooks_with_config(profile: &str, repo_hooks: HooksConfig) -> Option<HooksConfig> {
    let mut base = super::profile_config::resolve_config_or_warn(profile).hooks;

    if !repo_hooks.on_create.is_empty() {
        base.on_create = repo_hooks.on_create;
    }
    if !repo_hooks.on_launch.is_empty() {
        base.on_launch = repo_hooks.on_launch;
    }

    if base.on_create.is_empty() && base.on_launch.is_empty() {
        None
    } else {
        Some(base)
    }
}

/// Apply repo hooks onto a base (global/profile) hooks config, mirroring how hooks
/// actually resolve: repo overrides global per type. Unlike `merge_hooks_with_config`
/// this keeps `on_destroy` and never collapses to `None`, since it feeds the trust
/// dialog rather than execution gating.
fn apply_repo_hook_overrides(mut base: HooksConfig, repo_hooks: &HooksConfig) -> HooksConfig {
    if !repo_hooks.on_create.is_empty() {
        base.on_create = repo_hooks.on_create.clone();
    }
    if !repo_hooks.on_launch.is_empty() {
        base.on_launch = repo_hooks.on_launch.clone();
    }
    if !repo_hooks.on_destroy.is_empty() {
        base.on_destroy = repo_hooks.on_destroy.clone();
    }
    base
}

/// Resolve the full merged hook set for display in the trust dialog: global/profile
/// hooks overlaid by the repo's per-type overrides. Lets the user see every command
/// that will actually run, not just the repo-defined ones (#596).
pub fn merge_hooks_for_display(profile: &str, repo_hooks: &HooksConfig) -> HooksConfig {
    let base = super::profile_config::resolve_config_or_warn(profile).hooks;
    apply_repo_hook_overrides(base, repo_hooks)
}

/// One hook type's resolved commands plus where they came from, for display in the
/// CLI trust prompt and the TUI trust dialog. `from_repo` is true when the repo
/// defined this type (and thus overrode global); false means the commands fell
/// through to global/profile config.
pub struct HookDisplayGroup {
    pub name: &'static str,
    pub from_repo: bool,
    pub commands: Vec<String>,
}

impl HookDisplayGroup {
    /// Human-readable source suffix, e.g. ` (from repo)`. Shared so the CLI and TUI
    /// render the exact same wording.
    pub fn source_label(&self) -> &'static str {
        if self.from_repo {
            " (from repo)"
        } else {
            " (from global config)"
        }
    }
}

/// Build the per-type display groups for a merged hook set, labeling each type by
/// source and dropping types with no commands. `include_destroy` controls whether
/// `on_destroy` is listed: callers that only run the create lifecycle can omit it,
/// while the trust surfaces show every type the user is trusting.
pub fn hook_display_groups(
    merged: &HooksConfig,
    repo: &HooksConfig,
    include_destroy: bool,
) -> Vec<HookDisplayGroup> {
    let mut types: Vec<(&'static str, &[String], &[String])> = vec![
        ("on_create", &merged.on_create, &repo.on_create),
        ("on_launch", &merged.on_launch, &repo.on_launch),
    ];
    if include_destroy {
        types.push(("on_destroy", &merged.on_destroy, &repo.on_destroy));
    }
    types
        .into_iter()
        .filter(|(_, merged_cmds, _)| !merged_cmds.is_empty())
        .map(|(name, merged_cmds, repo_cmds)| HookDisplayGroup {
            name,
            from_repo: !repo_cmds.is_empty(),
            commands: merged_cmds.to_vec(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Hook execution
// ---------------------------------------------------------------------------

/// Where to run a hook command.
enum HookTarget<'a> {
    /// Run locally in the given project directory.
    Local { project_path: &'a Path },
    /// Run inside a Docker container.
    Container {
        container_name: &'a str,
        workdir: &'a str,
    },
}

/// Spawn-time options for a hook child process.
///
/// `detach_tty` disconnects the child from the parent's controlling terminal so
/// interactive prompts (e.g., `git clone` over HTTPS asking for a username)
/// cannot reach `/dev/tty` and corrupt the TUI screen. Used by every code path
/// reachable from the TUI or web server; the CLI paths leave the terminal
/// attached so the user's shell can still service prompts.
#[derive(Clone, Copy, Default)]
struct HookSpawnOpts {
    /// Append `2>&1` to the shell command so stderr is captured alongside
    /// stdout. Used by the streamed path that pipes a single fd to the UI.
    merge_stderr: bool,
    /// Severs every channel a credential prompt could escape through.
    detach_tty: bool,
}

/// Env vars that defang non-interactive credential prompts. Setting these on
/// the spawned process covers local hooks; for container hooks they must be
/// re-injected via `docker exec -e` since `docker exec` does not forward host
/// env vars by default.
const PROMPT_SUPPRESS_ENV: &[(&str, &str)] = &[
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ASKPASS", "true"),
    ("SSH_ASKPASS", "true"),
];

/// Build a `Command` for running a hook. Local hooks use the user's `$SHELL`;
/// container hooks use `bash` since the user shell may not be installed.
///
/// `extra_env` carries session metadata (see [`lifecycle_env_vars`]) that
/// scripts can read via `$AOE_SESSION_ID`, `$AOE_PROJECT_PATH`, etc. For
/// container hooks these are re-emitted as `docker exec -e KEY=VALUE` since
/// host env vars don't propagate into `docker exec` by default.
fn build_hook_command(
    cmd: &str,
    target: &HookTarget,
    opts: HookSpawnOpts,
    extra_env: &[(&'static str, String)],
) -> std::process::Command {
    let shell_cmd = if opts.merge_stderr {
        format!("{} 2>&1", cmd)
    } else {
        cmd.to_string()
    };

    let mut command = match target {
        HookTarget::Local { project_path } => {
            let shell = super::environment::user_shell();
            let mut command = std::process::Command::new(shell);
            command.arg("-c").arg(shell_cmd).current_dir(project_path);
            for (k, v) in extra_env {
                command.env(k, v);
            }
            command
        }
        HookTarget::Container {
            container_name,
            workdir,
        } => {
            let binary = crate::containers::runtime_binary();
            let mut command = std::process::Command::new(binary);
            command.arg("exec").arg("--workdir").arg(workdir);
            // For container hooks, env vars on the `docker exec` parent do not
            // propagate inside the container; inject them via `-e` instead.
            for (k, v) in extra_env {
                command.arg("-e").arg(format!("{}={}", k, v));
            }
            if opts.detach_tty {
                for (k, v) in PROMPT_SUPPRESS_ENV {
                    command.arg("-e").arg(format!("{}={}", k, v));
                }
            }
            command
                .arg(container_name)
                .arg("bash")
                .arg("-c")
                .arg(&shell_cmd);
            command
        }
    };

    if opts.detach_tty {
        // Cut every channel a credential prompt could escape through:
        //   - stdin: don't inherit the TUI's raw-mode terminal
        //   - prompt-suppression env vars (set on the parent for local hooks;
        //     forwarded via `-e` above for container hooks)
        //   - setsid (Unix, local only): no controlling terminal, so /dev/tty
        //     open fails. Container hooks already run via `docker exec` with
        //     no TTY allocated.
        command.stdin(std::process::Stdio::null());
        if matches!(target, HookTarget::Local { .. }) {
            for (k, v) in PROMPT_SUPPRESS_ENV {
                command.env(k, v);
            }
        }

        #[cfg(unix)]
        if matches!(target, HookTarget::Local { .. }) {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid is async-signal-safe per POSIX, which is the only
            // requirement for pre_exec closures.
            unsafe {
                command.pre_exec(|| {
                    nix::unistd::setsid().map_err(std::io::Error::other)?;
                    Ok(())
                });
            }
        }
    }

    command
}

/// Env vars exposed to lifecycle hooks (`on_create`, `on_launch`, `on_destroy`).
///
/// Mirrors the naming in [`crate::status_hooks::StatusHookContext::env_vars`]
/// so a single vocabulary covers both hook surfaces. `AOE_SESSION_BRANCH` is
/// only present when the session has a worktree; other fields may be empty
/// strings (e.g., `AOE_GROUP_PATH` for ungrouped sessions).
pub(crate) fn lifecycle_env_vars(instance: &super::Instance) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("AOE_SESSION_ID", instance.id.clone()),
        ("AOE_SESSION_TITLE", instance.title.clone()),
        ("AOE_PROJECT_PATH", instance.project_path.clone()),
        ("AOE_PROFILE", instance.effective_profile()),
        ("AOE_TOOL", instance.tool.clone()),
        ("AOE_GROUP_PATH", instance.group_path.clone()),
    ];
    if let Some(wt) = instance.worktree_info.as_ref() {
        env.push(("AOE_SESSION_BRANCH", wt.branch.clone()));
    }
    env
}

/// Format a hook failure error message from captured output.
fn format_hook_error(
    cmd: &str,
    exit_code: Option<i32>,
    stderr: &str,
    stdout: &str,
    in_container: bool,
) -> String {
    let prefix = if in_container {
        "Hook command failed in container"
    } else {
        "Hook command failed"
    };
    let mut detail = format!(
        "{} with exit code {}: {}",
        prefix,
        exit_code.unwrap_or(-1),
        cmd
    );
    if !stderr.is_empty() {
        detail.push_str(&format!("\nstderr:\n{}", stderr.trim_end()));
    }
    if !stdout.is_empty() {
        detail.push_str(&format!("\nstdout:\n{}", stdout.trim_end()));
    }
    detail
}

/// Run hook commands with captured output (non-streamed).
fn run_hooks_captured(
    commands: &[String],
    target: &HookTarget,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    let in_container = matches!(target, HookTarget::Container { .. });
    let timeout = crate::session::recovery::current_hook_timeout();

    for cmd in commands {
        tracing::info!(target: "session.store", "Running hook: {}", cmd);
        let mut command = build_hook_command(cmd, target, HookSpawnOpts::default(), extra_env);
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = match timeout {
            None => command
                .output()
                .with_context(|| format!("Failed to execute hook: {}", cmd))?,
            Some(deadline) => run_hook_with_timeout(&mut command, deadline, cmd)?,
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            anyhow::bail!(format_hook_error(
                cmd,
                output.status.code(),
                &stderr,
                &stdout,
                in_container
            ));
        }

        tracing::debug!(target: "session.store",
            "Hook completed: {} (stdout: {} bytes, stderr: {} bytes)",
            cmd,
            output.stdout.len(),
            output.stderr.len()
        );
    }
    Ok(())
}

/// Spawn a hook child, drain its pipes concurrently, and enforce a per-call
/// wall-clock deadline. On timeout, [`crate::process::kill_process_tree`]
/// reaps the descendant tree (SIGTERM, 100 ms grace, then SIGKILL) so the
/// recovery cascade can release its cross-process lock (#1265).
fn run_hook_with_timeout(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    cmd_label: &str,
) -> Result<std::process::Output> {
    // Match Command::output's stdin so hooks reading stdin see EOF.
    command.stdin(std::process::Stdio::null());
    let child = command
        .spawn()
        .with_context(|| format!("Failed to spawn hook: {}", cmd_label))?;
    let pid = child.id();

    let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
    std::thread::Builder::new()
        .name(format!("aoe-hook-drain-{}", pid))
        .spawn(move || {
            let _ = tx.send(child.wait_with_output());
        })
        .expect("hook drain thread spawn");

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(io_err)) => {
            // Reap defensively: wait_with_output can Err while the child is
            // still live, which would re-pin the recovery lock.
            crate::process::kill_process_tree(pid);
            Err(anyhow::Error::from(io_err)
                .context(format!("Failed to wait on hook: {}", cmd_label)))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                target: "session.startup_recovery",
                cmd = %cmd_label,
                timeout_secs = timeout.as_secs(),
                "on_launch hook timed out; killing process tree to release recovery lock"
            );
            crate::process::kill_process_tree(pid);
            anyhow::bail!(
                "on_launch hook timed out after {}s: {}",
                timeout.as_secs(),
                cmd_label
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!(
            "hook drain thread disconnected before reporting result: {}",
            cmd_label
        ),
    }
}

/// Run hook commands with streamed output sent through a progress channel.
fn run_hooks_streamed(
    commands: &[String],
    target: &HookTarget,
    progress_tx: &mpsc::Sender<HookProgress>,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    use std::io::BufRead;

    let in_container = matches!(target, HookTarget::Container { .. });

    for cmd in commands {
        tracing::info!(target: "session.store", "Running hook (streamed): {}", cmd);
        let _ = progress_tx.send(HookProgress::Started(cmd.clone()));

        let mut command = build_hook_command(
            cmd,
            target,
            HookSpawnOpts {
                merge_stderr: true,
                detach_tty: true,
            },
            extra_env,
        );
        let mut child = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("Failed to execute hook: {}", cmd))?;

        if let Some(stdout) = child.stdout.take() {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let _ = progress_tx.send(HookProgress::Output(line));
            }
        }

        let status = child.wait()?;
        if !status.success() {
            let detail = format_hook_error(cmd, status.code(), "", "", in_container);
            let _ = progress_tx.send(HookProgress::Output(detail.clone()));
            anyhow::bail!(detail);
        }

        tracing::debug!(target: "session.store", "Hook completed (streamed): {}", cmd);
    }
    Ok(())
}

/// Execute a list of hook commands in the given directory.
///
/// `extra_env` is exported to each hook process; see [`lifecycle_env_vars`]
/// for the canonical set of session env vars. Pass `&[]` if no session context
/// is available.
pub fn execute_hooks(
    commands: &[String],
    project_path: &Path,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    run_hooks_captured(commands, &HookTarget::Local { project_path }, extra_env)
}

/// Execute hooks inside a Docker container.
pub fn execute_hooks_in_container(
    commands: &[String],
    container_name: &str,
    workdir: &str,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    run_hooks_captured(
        commands,
        &HookTarget::Container {
            container_name,
            workdir,
        },
        extra_env,
    )
}

/// Execute hooks with best-effort semantics: all commands are attempted even if
/// some fail. Returns collected error messages. Designed for teardown hooks
/// (on_destroy) where partial cleanup is better than aborting on first failure.
///
/// `detach_tty` should be true when called from a TUI/web context so a hook that
/// blocks on a credential prompt cannot corrupt the rendered UI; false when
/// called from a CLI context where the user can answer prompts in their shell.
fn run_hooks_best_effort(
    commands: &[String],
    target: &HookTarget,
    detach_tty: bool,
    extra_env: &[(&'static str, String)],
) -> Vec<String> {
    let in_container = matches!(target, HookTarget::Container { .. });
    let mut errors = Vec::new();

    for cmd in commands {
        tracing::info!(target: "session.store", "Running hook (best-effort): {}", cmd);
        let mut command = build_hook_command(
            cmd,
            target,
            HookSpawnOpts {
                merge_stderr: false,
                detach_tty,
            },
            extra_env,
        );
        match command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(output) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let err = format_hook_error(
                        cmd,
                        output.status.code(),
                        &stderr,
                        &stdout,
                        in_container,
                    );
                    tracing::warn!(target: "session.store", "{}", err);
                    errors.push(err);
                } else {
                    tracing::debug!(target: "session.store",
                        "Hook completed: {} (stdout: {} bytes, stderr: {} bytes)",
                        cmd,
                        output.stdout.len(),
                        output.stderr.len()
                    );
                }
            }
            Err(e) => {
                let err = format!("Failed to execute hook: {}: {}", cmd, e);
                tracing::warn!(target: "session.store", "{}", err);
                errors.push(err);
            }
        }
    }
    errors
}

/// Execute hooks locally with best-effort semantics (all commands attempted).
///
/// `detach_tty` should be true when called from a TUI/web context to keep
/// credential prompts off the UI; false from CLI so prompts remain answerable.
/// Returns a list of error messages for any hooks that failed.
pub fn execute_hooks_best_effort(
    commands: &[String],
    project_path: &Path,
    detach_tty: bool,
    extra_env: &[(&'static str, String)],
) -> Vec<String> {
    run_hooks_best_effort(
        commands,
        &HookTarget::Local { project_path },
        detach_tty,
        extra_env,
    )
}

/// Execute hooks in a container with best-effort semantics (all commands attempted).
///
/// `detach_tty` should be true when called from a TUI/web context to keep
/// credential prompts off the UI; false from CLI so prompts remain answerable.
/// Returns a list of error messages for any hooks that failed.
pub fn execute_hooks_in_container_best_effort(
    commands: &[String],
    container_name: &str,
    workdir: &str,
    detach_tty: bool,
    extra_env: &[(&'static str, String)],
) -> Vec<String> {
    run_hooks_best_effort(
        commands,
        &HookTarget::Container {
            container_name,
            workdir,
        },
        detach_tty,
        extra_env,
    )
}

/// Execute a list of hook commands with streamed output.
pub fn execute_hooks_streamed(
    commands: &[String],
    project_path: &Path,
    progress_tx: &mpsc::Sender<HookProgress>,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    run_hooks_streamed(
        commands,
        &HookTarget::Local { project_path },
        progress_tx,
        extra_env,
    )
}

/// Execute hooks inside a Docker container with streamed output.
pub fn execute_hooks_in_container_streamed(
    commands: &[String],
    container_name: &str,
    workdir: &str,
    progress_tx: &mpsc::Sender<HookProgress>,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    run_hooks_streamed(
        commands,
        &HookTarget::Container {
            container_name,
            workdir,
        },
        progress_tx,
        extra_env,
    )
}

/// Template content for `aoe init`.
pub const INIT_TEMPLATE: &str = r#"# Agent of Empires - Repository Configuration
# This file configures aoe behavior for this repository.
# See: https://github.com/agent-of-empires/agent-of-empires

# [hooks]
# Commands run once when a session is first created
# on_create = ["npm install", "cp .env.example .env"]
# Commands run every time a session starts
# on_launch = ["npm install"]
# Commands run when a session is deleted (before cleanup)
# on_destroy = ["docker-compose down"]

# [session]
# default_tool = "claude"

# [sandbox]
# enabled_by_default = true
# default_image = "ghcr.io/agent-of-empires/aoe-dev-sandbox:0.10"
# List fields below replace (not append to) global settings when set:
# environment = ["NODE_ENV", "DATABASE_URL"]
# volume_ignores = ["node_modules", ".next"]

# [worktree]
# enabled = true

# [updates]
# update_check_mode = "off"

# [tmux]
# status_bar = "auto"
# mouse = "auto"

# [sound]
# enabled = false
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hooks_config_empty() {
        let hooks = HooksConfig::default();
        assert!(hooks.is_empty());
    }

    #[test]
    fn test_hooks_config_not_empty() {
        let hooks = HooksConfig {
            on_create: vec!["npm install".to_string()],
            ..Default::default()
        };
        assert!(!hooks.is_empty());
    }

    #[test]
    fn test_hooks_config_not_empty_on_destroy() {
        let hooks = HooksConfig {
            on_destroy: vec!["docker-compose down".to_string()],
            ..Default::default()
        };
        assert!(!hooks.is_empty());
    }

    #[test]
    fn test_compute_hooks_hash_deterministic() {
        let hooks = HooksConfig {
            on_create: vec!["npm install".to_string()],
            on_launch: vec!["echo hello".to_string()],
            ..Default::default()
        };
        let hash1 = compute_hooks_hash(&hooks);
        let hash2 = compute_hooks_hash(&hooks);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_compute_hooks_hash_differs_on_change() {
        let hooks1 = HooksConfig {
            on_create: vec!["npm install".to_string()],
            ..Default::default()
        };
        let hooks2 = HooksConfig {
            on_create: vec!["yarn install".to_string()],
            ..Default::default()
        };
        assert_ne!(compute_hooks_hash(&hooks1), compute_hooks_hash(&hooks2));
    }

    #[test]
    fn test_compute_hooks_hash_distinguishes_hook_types() {
        let hooks1 = HooksConfig {
            on_create: vec!["echo hello".to_string()],
            ..Default::default()
        };
        let hooks2 = HooksConfig {
            on_launch: vec!["echo hello".to_string()],
            ..Default::default()
        };
        assert_ne!(compute_hooks_hash(&hooks1), compute_hooks_hash(&hooks2));
    }

    #[test]
    fn test_compute_hooks_hash_includes_on_destroy() {
        let hooks1 = HooksConfig {
            on_destroy: vec!["cleanup".to_string()],
            ..Default::default()
        };
        let hooks2 = HooksConfig::default();
        assert_ne!(compute_hooks_hash(&hooks1), compute_hooks_hash(&hooks2));
    }

    #[test]
    fn test_repo_config_deserialization() {
        let toml = r#"
            [hooks]
            on_create = ["npm install"]
            on_launch = ["echo start"]

            [session]
            default_tool = "opencode"

            [sandbox]
            enabled_by_default = true
            volume_ignores = ["node_modules"]

            [worktree]
            enabled = true
        "#;

        let config: RepoConfig = toml::from_str(toml).unwrap();
        let hooks = config.hooks().unwrap();
        assert_eq!(hooks.on_create, vec!["npm install"]);
        assert_eq!(hooks.on_launch, vec!["echo start"]);
        let ov = serde_json::to_value(&config).unwrap();
        assert_eq!(ov["session"]["default_tool"], serde_json::json!("opencode"));
        assert_eq!(ov["sandbox"]["enabled_by_default"], serde_json::json!(true));
        assert_eq!(ov["worktree"]["enabled"], serde_json::json!(true));
    }

    #[test]
    fn test_hooks_string_instead_of_array_parses_ok() {
        // Regression test for #561: user writes on_launch as a plain string
        // instead of an array. Previously this caused the entire RepoConfig to
        // fail deserialization, silently dropping all settings including sandbox
        // env vars. Now string_or_vec accepts both formats.
        let toml = r#"
            [sandbox]
            environment = ["ANTHROPIC_API_KEY", "UV_LINK_MODE=copy", "CI=true"]

            [hooks]
            on_launch = "uv python install 3.11 && uv venv /opt/venv --python 3.11"
        "#;

        let config: RepoConfig = toml::from_str(toml).unwrap();
        let hooks = config.hooks().unwrap();
        assert_eq!(
            hooks.on_launch,
            vec!["uv python install 3.11 && uv venv /opt/venv --python 3.11"]
        );
        assert!(hooks.on_create.is_empty());

        // Verify the sandbox config is also preserved
        let ov = serde_json::to_value(&config).unwrap();
        assert_eq!(
            ov["sandbox"]["environment"],
            serde_json::json!(["ANTHROPIC_API_KEY", "UV_LINK_MODE=copy", "CI=true"])
        );
    }

    #[test]
    fn test_hooks_on_create_string_parses_ok() {
        let toml = r#"
            [hooks]
            on_create = "npm install"
        "#;

        let config: RepoConfig = toml::from_str(toml).unwrap();
        let hooks = config.hooks().unwrap();
        assert_eq!(hooks.on_create, vec!["npm install"]);
        assert!(hooks.on_launch.is_empty());
    }

    #[test]
    fn test_hooks_on_destroy_string_parses_ok() {
        let toml = r#"
            [hooks]
            on_destroy = "docker-compose down"
        "#;

        let config: RepoConfig = toml::from_str(toml).unwrap();
        let hooks = config.hooks().unwrap();
        assert_eq!(hooks.on_destroy, vec!["docker-compose down"]);
        assert!(hooks.on_create.is_empty());
        assert!(hooks.on_launch.is_empty());
    }

    #[test]
    fn test_hooks_on_destroy_array_parses_ok() {
        let toml = r#"
            [hooks]
            on_destroy = ["docker-compose down", "rm -rf /tmp/cache"]
        "#;

        let config: RepoConfig = toml::from_str(toml).unwrap();
        let hooks = config.hooks().unwrap();
        assert_eq!(
            hooks.on_destroy,
            vec!["docker-compose down", "rm -rf /tmp/cache"]
        );
    }

    #[test]
    fn test_hooks_array_still_works() {
        let toml = r#"
            [hooks]
            on_create = ["npm install", "cp .env.example .env"]
            on_launch = ["npm start"]
        "#;

        let config: RepoConfig = toml::from_str(toml).unwrap();
        let hooks = config.hooks().unwrap();
        assert_eq!(hooks.on_create, vec!["npm install", "cp .env.example .env"]);
        assert_eq!(hooks.on_launch, vec!["npm start"]);
    }

    #[test]
    fn test_repo_config_empty_deserialization() {
        let config: RepoConfig = toml::from_str("").unwrap();
        assert!(config.hooks().is_none());
        assert!(config.overrides.is_empty());
    }

    #[test]
    fn test_merge_repo_config_session() {
        let config = Config::default();
        let repo: RepoConfig =
            serde_json::from_value(serde_json::json!({"session": {"default_tool": "opencode"}}))
                .unwrap();
        let merged = merge_repo_config(config, &repo);
        assert_eq!(merged.session.default_tool, Some("opencode".to_string()));
    }

    #[test]
    fn test_merge_repo_config_sandbox() {
        let config = Config::default();
        let repo: RepoConfig = serde_json::from_value(serde_json::json!({"sandbox": {
            "enabled_by_default": true,
            "default_image": "ghcr.io/example/custom:latest",
            "volume_ignores": ["node_modules"]
        }}))
        .unwrap();
        let merged = merge_repo_config(config, &repo);
        assert!(merged.sandbox.enabled_by_default);
        // `aoe add --sandbox` reads `config.sandbox.default_image` as its
        // fallback image, so the repo override must land here (see #1651).
        assert_eq!(
            merged.sandbox.default_image,
            "ghcr.io/example/custom:latest"
        );
        assert_eq!(merged.sandbox.volume_ignores, vec!["node_modules"]);
    }

    #[test]
    fn test_merge_repo_config_worktree() {
        let config = Config::default();
        let repo: RepoConfig = serde_json::from_value(
            serde_json::json!({"worktree": {"enabled": true, "path_template": "../wt/{branch}"}}),
        )
        .unwrap();
        let merged = merge_repo_config(config, &repo);
        assert!(merged.worktree.enabled);
        assert_eq!(merged.worktree.path_template, "../wt/{branch}");
    }

    #[test]
    fn test_merge_repo_config_no_overrides() {
        let config = Config::default();
        let repo = RepoConfig::default();
        let merged = merge_repo_config(config.clone(), &repo);
        assert_eq!(merged.worktree.enabled, config.worktree.enabled);
        assert_eq!(
            merged.sandbox.enabled_by_default,
            config.sandbox.enabled_by_default
        );
    }

    #[test]
    fn test_load_repo_config_nonexistent() {
        let result = load_repo_config(Path::new("/nonexistent/path")).unwrap();
        assert!(result.is_none());
    }

    fn global_hooks_fixture() -> HooksConfig {
        HooksConfig {
            on_create: vec!["global-create".to_string()],
            on_launch: vec!["global-launch".to_string()],
            on_destroy: vec!["global-destroy".to_string()],
        }
    }

    #[test]
    fn test_apply_repo_hook_overrides_replaces_not_appends() {
        // A repo-defined type fully replaces the global list (per-field override,
        // never append): the global command must NOT survive in the merged list.
        let repo = HooksConfig {
            on_create: vec!["repo-create-a".to_string(), "repo-create-b".to_string()],
            on_launch: vec!["repo-launch".to_string()],
            on_destroy: vec!["repo-destroy".to_string()],
        };
        let merged = apply_repo_hook_overrides(global_hooks_fixture(), &repo);
        assert_eq!(
            merged.on_create,
            vec!["repo-create-a".to_string(), "repo-create-b".to_string()]
        );
        assert_eq!(merged.on_launch, vec!["repo-launch".to_string()]);
        assert_eq!(merged.on_destroy, vec!["repo-destroy".to_string()]);
        assert!(
            !merged.on_create.iter().any(|c| c.starts_with("global")),
            "global on_create should be replaced, not appended"
        );
    }

    #[test]
    fn test_apply_repo_hook_overrides_falls_through_to_global() {
        // Types the repo leaves empty fall through to the global/profile values.
        let repo = HooksConfig::default();
        let merged = apply_repo_hook_overrides(global_hooks_fixture(), &repo);
        assert_eq!(merged.on_create, vec!["global-create".to_string()]);
        assert_eq!(merged.on_launch, vec!["global-launch".to_string()]);
        assert_eq!(merged.on_destroy, vec!["global-destroy".to_string()]);
    }

    #[test]
    fn test_apply_repo_hook_overrides_mixed_per_type() {
        // Override and fall-through coexist: repo defines only on_create, so
        // on_launch/on_destroy keep the global values.
        let repo = HooksConfig {
            on_create: vec!["repo-create".to_string()],
            ..Default::default()
        };
        let merged = apply_repo_hook_overrides(global_hooks_fixture(), &repo);
        assert_eq!(merged.on_create, vec!["repo-create".to_string()]);
        assert_eq!(merged.on_launch, vec!["global-launch".to_string()]);
        assert_eq!(merged.on_destroy, vec!["global-destroy".to_string()]);
    }

    #[test]
    fn test_hook_display_groups_labels_source_and_filters_empty() {
        // Repo overrides on_create; on_launch falls through to global; on_destroy
        // has no commands and must be dropped from the display groups.
        let repo = HooksConfig {
            on_create: vec!["repo-create".to_string()],
            ..Default::default()
        };
        let merged = HooksConfig {
            on_create: vec!["repo-create".to_string()],
            on_launch: vec!["global-launch".to_string()],
            on_destroy: vec![],
        };
        let groups = hook_display_groups(&merged, &repo, true);
        assert_eq!(groups.len(), 2, "empty on_destroy should be filtered out");

        assert_eq!(groups[0].name, "on_create");
        assert!(groups[0].from_repo);
        assert_eq!(groups[0].source_label(), " (from repo)");
        assert_eq!(groups[0].commands, vec!["repo-create".to_string()]);

        assert_eq!(groups[1].name, "on_launch");
        assert!(!groups[1].from_repo);
        assert_eq!(groups[1].source_label(), " (from global config)");
        assert_eq!(groups[1].commands, vec!["global-launch".to_string()]);
    }

    #[test]
    fn test_hook_display_groups_include_destroy_toggle() {
        // on_destroy only appears when the caller opts in (e.g. the trust surfaces),
        // not for create-lifecycle callers.
        let repo = HooksConfig {
            on_destroy: vec!["repo-destroy".to_string()],
            ..Default::default()
        };
        let merged = HooksConfig {
            on_destroy: vec!["repo-destroy".to_string()],
            ..Default::default()
        };
        assert!(
            hook_display_groups(&merged, &repo, false).is_empty(),
            "on_destroy should be hidden when include_destroy is false"
        );

        let groups = hook_display_groups(&merged, &repo, true);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "on_destroy");
        assert!(groups[0].from_repo);
        assert_eq!(groups[0].commands, vec!["repo-destroy".to_string()]);
    }

    #[test]
    fn test_init_template_is_valid_toml_when_uncommented() {
        // Verify that uncommenting the TOML sections produces valid TOML.
        // Skip pure comment lines (those that don't look like TOML key/section syntax).
        let uncommented: String = INIT_TEMPLATE
            .lines()
            .filter_map(|line| {
                if let Some(stripped) = line.strip_prefix("# ") {
                    // Only uncomment lines that look like TOML (start with [ or key =)
                    let trimmed = stripped.trim();
                    if trimmed.starts_with('[') || trimmed.contains(" = ") || trimmed.contains("= ")
                    {
                        Some(stripped.to_string())
                    } else {
                        None
                    }
                } else {
                    Some(line.to_string())
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let _config: RepoConfig = toml::from_str(&uncommented).unwrap();
    }

    #[test]
    fn test_trusted_repos_serialization() {
        let trusted = TrustedRepos {
            repos: vec![TrustedRepo {
                path: "/home/user/project".to_string(),
                hooks_hash: Some("abc123".to_string()),
                mcp_hash: Some("def456".to_string()),
                trusted_at: "2026-01-31T00:00:00Z".to_string(),
            }],
        };
        let serialized = toml::to_string_pretty(&trusted).unwrap();
        assert!(serialized.contains("path = \"/home/user/project\""));
        assert!(serialized.contains("hooks_hash = \"abc123\""));
        assert!(serialized.contains("mcp_hash = \"def456\""));

        let deserialized: TrustedRepos = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.repos.len(), 1);
        assert_eq!(deserialized.repos[0].path, "/home/user/project");
    }

    /// Back-compat: an existing pre-#1985 `trusted_repos.toml` row has only a
    /// string `hooks_hash` and no `mcp_hash`; it must still deserialize, with
    /// `mcp_hash` defaulting to `None` (project MCP never reviewed).
    #[test]
    fn test_trusted_repos_legacy_row_deserializes() {
        let legacy = r#"
[[repos]]
path = "/home/user/project"
hooks_hash = "abc123"
trusted_at = "2026-01-31T00:00:00Z"
"#;
        let parsed: TrustedRepos = toml::from_str(legacy).unwrap();
        assert_eq!(parsed.repos.len(), 1);
        assert_eq!(parsed.repos[0].hooks_hash.as_deref(), Some("abc123"));
        assert_eq!(parsed.repos[0].mcp_hash, None);
    }

    #[test]
    fn test_normalize_path_nonexistent_falls_back() {
        let path = Path::new("/nonexistent/path/that/does/not/exist");
        assert_eq!(normalize_path(path), path.to_string_lossy());
    }

    #[test]
    fn test_normalize_path_real_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let normalized = normalize_path(tmp.path());
        assert_eq!(
            std::fs::canonicalize(tmp.path()).unwrap().to_string_lossy(),
            normalized
        );
    }

    #[test]
    fn test_normalize_path_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let link_dir = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let normalized_real = normalize_path(&real_dir);
        let normalized_link = normalize_path(&link_dir);
        assert_eq!(normalized_real, normalized_link);
    }

    #[test]
    fn test_execute_hooks_in_container_fails_gracefully() {
        let result = execute_hooks_in_container(
            &["echo test".to_string()],
            "nonexistent_container",
            "/workspace/myproject",
            &[],
        );
        // Should fail because docker/container doesn't exist, but should not panic
        assert!(result.is_err());
    }

    #[test]
    fn test_merge_repo_config_preserves_unset_fields() {
        let mut config = Config::default();
        config.sandbox.enabled_by_default = true;
        config.sandbox.auto_cleanup = true;
        config.worktree.enabled = true;
        config.worktree.auto_cleanup = true;

        // Only override one field per section
        let repo: RepoConfig = serde_json::from_value(serde_json::json!({
            "sandbox": {"enabled_by_default": false},
            "worktree": {"enabled": false}
        }))
        .unwrap();

        let merged = merge_repo_config(config, &repo);
        // Overridden fields should change
        assert!(!merged.sandbox.enabled_by_default);
        assert!(!merged.worktree.enabled);
        // Non-overridden fields should be preserved
        assert!(merged.sandbox.auto_cleanup);
        assert!(merged.worktree.auto_cleanup);
    }

    /// Regression for issue #901: streamed hooks must run detached from the
    /// TUI's controlling terminal, so an interactive prompt (e.g., `git clone`
    /// over HTTPS asking for a username) cannot reach `/dev/tty` and corrupt
    /// the TUI screen. We verify the contract holds:
    ///   1. stdin is not a TTY (`[ -t 0 ]` is false)
    ///   2. `GIT_TERMINAL_PROMPT=0` is exported, so git fails fast with a
    ///      clean error instead of falling back to a tty prompt
    ///   3. `GIT_ASKPASS` / `SSH_ASKPASS` are defanged
    #[test]
    fn streamed_hook_detached_from_tty() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = r#"
            if [ -t 0 ]; then echo "STDIN=tty"; else echo "STDIN=notty"; fi
            echo "GIT_TERMINAL_PROMPT=${GIT_TERMINAL_PROMPT:-unset}"
            echo "GIT_ASKPASS=${GIT_ASKPASS:-unset}"
            echo "SSH_ASKPASS=${SSH_ASKPASS:-unset}"
        "#;
        let (tx, rx) = mpsc::channel();
        execute_hooks_streamed(&[probe.to_string()], tmp.path(), &tx, &[]).unwrap();
        drop(tx);

        let lines: Vec<String> = rx
            .into_iter()
            .filter_map(|p| match p {
                HookProgress::Output(line) => Some(line),
                HookProgress::Started(_) => None,
            })
            .collect();
        let joined = lines.join("\n");

        assert!(
            joined.contains("STDIN=notty"),
            "streamed hook stdin should be disconnected from any TTY, got:\n{}",
            joined
        );
        assert!(
            joined.contains("GIT_TERMINAL_PROMPT=0"),
            "GIT_TERMINAL_PROMPT must be 0 to prevent git tty prompts, got:\n{}",
            joined
        );
        assert!(
            joined.contains("GIT_ASKPASS=true"),
            "GIT_ASKPASS must be defanged, got:\n{}",
            joined
        );
        assert!(
            joined.contains("SSH_ASKPASS=true"),
            "SSH_ASKPASS must be defanged, got:\n{}",
            joined
        );
    }

    /// The CLI/captured path leaves the terminal attached so users running
    /// `aoe add` from a real shell can still answer interactive prompts. We
    /// only verify the env vars are NOT forced here (stdin may or may not be
    /// a TTY depending on how tests are launched).
    #[test]
    fn captured_hook_does_not_force_git_env() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = "echo \"GIT_TERMINAL_PROMPT=${GIT_TERMINAL_PROMPT:-unset}\" > out.txt";
        execute_hooks(&[probe.to_string()], tmp.path(), &[]).unwrap();
        let out = std::fs::read_to_string(tmp.path().join("out.txt")).unwrap();
        assert!(
            !out.contains("GIT_TERMINAL_PROMPT=0"),
            "captured path must not force GIT_TERMINAL_PROMPT, got: {}",
            out
        );
    }

    /// Container hooks need prompt-suppression env vars forwarded into the
    /// container via `docker exec -e`, since `docker exec` does not pass host
    /// env vars by default. This is a structural test: we don't actually run
    /// `docker exec` (CI lacks it), we just inspect the args we'd hand to it.
    #[test]
    fn container_hook_forwards_prompt_env_via_dash_e() {
        let target = HookTarget::Container {
            container_name: "test_container",
            workdir: "/work",
        };
        let detached = build_hook_command(
            "git clone https://example.com/repo",
            &target,
            HookSpawnOpts {
                merge_stderr: true,
                detach_tty: true,
            },
            &[],
        );
        let args: Vec<String> = detached
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(
            joined.contains("-e GIT_TERMINAL_PROMPT=0"),
            "expected `-e GIT_TERMINAL_PROMPT=0` in docker exec args, got: {:?}",
            args
        );
        assert!(
            joined.contains("-e GIT_ASKPASS=true"),
            "expected `-e GIT_ASKPASS=true` in docker exec args, got: {:?}",
            args
        );
        assert!(
            joined.contains("-e SSH_ASKPASS=true"),
            "expected `-e SSH_ASKPASS=true` in docker exec args, got: {:?}",
            args
        );

        let attached =
            build_hook_command("rm -rf /work/build", &target, HookSpawnOpts::default(), &[]);
        let attached_args: Vec<String> = attached
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !attached_args.iter().any(|a| a == "-e"),
            "captured container path must not inject `-e` flags, got: {:?}",
            attached_args
        );
    }

    /// on_destroy hooks invoked from the TUI/web (via session::deletion) must
    /// also detach from the controlling terminal. Verifies the new
    /// `detach_tty` flag on `execute_hooks_best_effort` actually flows through.
    #[test]
    fn best_effort_hook_detaches_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = "echo \"GIT_TERMINAL_PROMPT=${GIT_TERMINAL_PROMPT:-unset}\" > out.txt";
        let errors = execute_hooks_best_effort(&[probe.to_string()], tmp.path(), true, &[]);
        assert!(
            errors.is_empty(),
            "hook should succeed, errors: {:?}",
            errors
        );
        let out = std::fs::read_to_string(tmp.path().join("out.txt")).unwrap();
        assert!(
            out.contains("GIT_TERMINAL_PROMPT=0"),
            "best-effort path with detach_tty=true must export GIT_TERMINAL_PROMPT=0, got: {}",
            out
        );
    }

    #[test]
    fn best_effort_hook_attached_for_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = "echo \"GIT_TERMINAL_PROMPT=${GIT_TERMINAL_PROMPT:-unset}\" > out.txt";
        let errors = execute_hooks_best_effort(&[probe.to_string()], tmp.path(), false, &[]);
        assert!(
            errors.is_empty(),
            "hook should succeed, errors: {:?}",
            errors
        );
        let out = std::fs::read_to_string(tmp.path().join("out.txt")).unwrap();
        assert!(
            !out.contains("GIT_TERMINAL_PROMPT=0"),
            "CLI best-effort path must not force GIT_TERMINAL_PROMPT, got: {}",
            out
        );
    }

    /// `lifecycle_env_vars` is the contract advertised to hook authors in
    /// docs/guides/repo-config.md. Keys (and conditional inclusion of
    /// `AOE_SESSION_BRANCH`) must stay stable; pin the shape here so changes
    /// have to update both the helper and the docs in lockstep.
    #[test]
    fn lifecycle_env_vars_shape() {
        use super::super::instance::WorktreeInfo;
        use crate::session::Instance;

        let mut instance = Instance::new("My Session", "/tmp/proj");
        instance.tool = "claude".to_string();
        instance.group_path = "backend/api".to_string();
        instance.source_profile = "work".to_string();

        let env: std::collections::HashMap<_, _> =
            lifecycle_env_vars(&instance).into_iter().collect();

        assert_eq!(
            env.get("AOE_SESSION_ID").map(String::as_str),
            Some(instance.id.as_str())
        );
        assert_eq!(
            env.get("AOE_SESSION_TITLE").map(String::as_str),
            Some("My Session")
        );
        assert_eq!(
            env.get("AOE_PROJECT_PATH").map(String::as_str),
            Some("/tmp/proj")
        );
        assert_eq!(env.get("AOE_TOOL").map(String::as_str), Some("claude"));
        assert_eq!(
            env.get("AOE_GROUP_PATH").map(String::as_str),
            Some("backend/api")
        );
        assert!(env.contains_key("AOE_PROFILE"));
        assert!(
            !env.contains_key("AOE_SESSION_BRANCH"),
            "branch should be omitted when no worktree is attached"
        );

        instance.worktree_info = Some(WorktreeInfo {
            branch: "feature/auth".to_string(),
            main_repo_path: "/tmp/proj".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let env_with_branch: std::collections::HashMap<_, _> =
            lifecycle_env_vars(&instance).into_iter().collect();
        assert_eq!(
            env_with_branch
                .get("AOE_SESSION_BRANCH")
                .map(String::as_str),
            Some("feature/auth")
        );
    }

    /// End-to-end check that session env vars actually reach the hook process:
    /// run a real local hook that echoes its env into a file, then read it
    /// back. Covers the captured (`execute_hooks`), streamed
    /// (`execute_hooks_streamed`), and best-effort (`execute_hooks_best_effort`)
    /// paths so a regression in any one of them fails this test.
    #[test]
    fn local_hooks_see_session_env_vars() {
        use crate::session::Instance;

        let tmp = tempfile::tempdir().unwrap();
        let mut instance = Instance::new("My Title", tmp.path().to_str().unwrap());
        instance.tool = "codex".to_string();
        instance.source_profile = "work".to_string();
        let env = lifecycle_env_vars(&instance);

        let probe = r#"
            echo "ID=${AOE_SESSION_ID}" > env.txt
            echo "TITLE=${AOE_SESSION_TITLE}" >> env.txt
            echo "PATH=${AOE_PROJECT_PATH}" >> env.txt
            echo "TOOL=${AOE_TOOL}" >> env.txt
        "#;

        execute_hooks(&[probe.to_string()], tmp.path(), &env).unwrap();
        let out = std::fs::read_to_string(tmp.path().join("env.txt")).unwrap();
        assert!(
            out.contains(&format!("ID={}", instance.id)),
            "captured: {}",
            out
        );
        assert!(out.contains("TITLE=My Title"), "captured: {}", out);
        assert!(out.contains("TOOL=codex"), "captured: {}", out);

        std::fs::remove_file(tmp.path().join("env.txt")).unwrap();
        let errors = execute_hooks_best_effort(&[probe.to_string()], tmp.path(), false, &env);
        assert!(errors.is_empty(), "best-effort errors: {:?}", errors);
        let out = std::fs::read_to_string(tmp.path().join("env.txt")).unwrap();
        assert!(
            out.contains(&format!("ID={}", instance.id)),
            "best-effort: {}",
            out
        );

        std::fs::remove_file(tmp.path().join("env.txt")).unwrap();
        let (tx, _rx) = mpsc::channel();
        execute_hooks_streamed(&[probe.to_string()], tmp.path(), &tx, &env).unwrap();
        drop(tx);
        let out = std::fs::read_to_string(tmp.path().join("env.txt")).unwrap();
        assert!(
            out.contains(&format!("ID={}", instance.id)),
            "streamed: {}",
            out
        );
    }

    /// Container hooks must forward session env via `docker exec -e KEY=VALUE`
    /// since host env doesn't propagate into `docker exec`. Structural check
    /// against the constructed argv (CI has no docker daemon).
    #[test]
    fn container_hook_forwards_session_env_via_dash_e() {
        let target = HookTarget::Container {
            container_name: "test_container",
            workdir: "/work",
        };
        let env = vec![
            ("AOE_SESSION_ID", "s_abc".to_string()),
            ("AOE_SESSION_TITLE", "My Title".to_string()),
        ];
        let cmd = build_hook_command("true", &target, HookSpawnOpts::default(), &env);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Assert directly against the argv vector so a value with spaces ("My
        // Title") must be a single argument; a substring match on the joined
        // string would pass even if the value were split across two args.
        let has_pair = |k: &str, v: &str| -> bool {
            args.windows(2)
                .any(|w| w[0] == "-e" && w[1] == format!("{}={}", k, v))
        };
        assert!(
            has_pair("AOE_SESSION_ID", "s_abc"),
            "expected (-e, AOE_SESSION_ID=s_abc) pair in argv, got: {:?}",
            args
        );
        assert!(
            has_pair("AOE_SESSION_TITLE", "My Title"),
            "expected (-e, AOE_SESSION_TITLE=My Title) pair as single argv element, got: {:?}",
            args
        );
    }
}
