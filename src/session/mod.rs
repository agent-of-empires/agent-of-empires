//! Session management module

pub(crate) mod anchored_fs;
pub mod artifacts;
pub mod attach_project;
pub mod builder;
pub(crate) mod capture;
pub mod cityhall_bundle;
pub mod civilizations;
pub(crate) mod claim;
// Discovery of on-disk Claude Code sessions. Lives here rather than under
// `acp` because terminal/tmux import via the CLI does not involve ACP.
pub mod claude_import;
pub mod config;
pub mod conversation_carry;
pub mod conversation_summary;
pub mod deletion;
pub(crate) mod environment;
pub mod fork;
mod groups;
pub mod idle_reap;
mod instance;
pub mod mcp;
mod move_journal;
pub mod poller;
pub mod projects;
pub(crate) mod recovery;
pub mod restart;
pub mod sandbox_store_reclaim;
pub mod scope;
pub mod scratch;
pub(crate) mod serde_helpers;
pub mod skills_model;
pub mod smart_rename;
pub mod stop;
mod storage;
pub(crate) mod sync;
#[cfg(test)]
pub(crate) mod test_support;
pub mod trash;
pub mod worktree_edit;
pub mod worktree_reconcile;

pub use crate::sound::SoundConfig;
pub use crate::status_hooks::StatusHookConfig;
pub(crate) use anchored_fs::AnchoredDir;
pub(crate) use capture::is_valid_session_id;
pub use config::{
    get_telemetry_settings, get_update_settings, load_config, update_app_state, update_config,
    validate_snooze_duration, AgentRuntimeConfig, AttachMode, CapabilityGrant, ClickAction, Config,
    ContainerRuntimeName, DefaultTerminalMode, GroupByMode, NewSessionMode, PluginConfig,
    RowTagMode, SandboxConfig, SessionConfig, TelemetryConfig, ThemeConfig, TmuxSettingMode,
    UpdatesConfig, VolumeIgnoresStrategy, WorktreeConfig,
};
pub(crate) use environment::user_shell;
pub use environment::{validate_env_entries, validate_env_entry};
pub use fork::{ForkDenied, ForkSeed};
/// Shared by the sorter and the row renderer so a row is decorated as a
/// favorite exactly when it is pinned as one.
pub(crate) use groups::is_live_favorite;
pub use groups::{
    append_archived_section, append_archived_section_by_project, append_trash_section,
    archived_project_sub_path, flatten_sessions_by_attention, flatten_tree,
    flatten_tree_all_profiles, is_archived_section_path, is_synthetic_project_header,
    is_trash_section_path, is_within_archived_section, is_within_trash_section,
    project_group_display_name, Group, GroupTree, Item, ARCHIVED_SECTION_NAME,
    ARCHIVED_SECTION_PATH, SCRATCH_GROUP_NAME, SCRATCH_GROUP_PATH, TRASH_SECTION_NAME,
    TRASH_SECTION_PATH,
};
#[cfg(test)]
pub(crate) use instance::install_aliases;
#[cfg(test)]
pub(crate) use instance::test_helpers::publish_host_pi_transcript;
pub(crate) use instance::{
    duplicate_session_error, find_duplicate_session, is_duplicate_session,
    persist_session_to_storage, PassiveStatusPatch, ResumeIntent, SidWrite,
    NEWER_GENERATION_BUSY_REASON,
};
pub(crate) use instance::{
    generic_host_config_path_for, resolved_agent_for, sidecar_host_config_path_for,
    ConversationState, ResumeAttemptPolicy, TerminalContextResume,
};
pub use instance::{
    is_valid_session_color, ConversationBinding, ConversationProvenance, DetectionState,
    EnsureReadyError, EnsureReadyOutcome, ExecutionBinding, ExecutionLocation, Instance,
    LaunchSidOutcome, LifecycleOperation, LifecycleReservation, LifecycleReservationError,
    PendingInitialTurn, PluginCreateIdempotency, PollerStart, SandboxInfo, SessionBucket,
    StartOutcome, Status, TerminalInfo, View, WorkspaceInfo, WorkspaceRepo, WorktreeInfo,
    SESSION_COLORS, TMUX_SESSION_GONE_ERROR,
};
#[cfg(test)]
pub(crate) use move_journal::{
    record as record_move_journal, MoveJournalEntry, MOVE_JOURNAL_VERSION,
};
pub(crate) use storage::acquire_session_identity_lock;
#[cfg(test)]
pub(crate) use storage::observe_lock_contention_for_test;
pub(crate) use storage::{reconcile_profile_duplicates, DuplicateIdReport};

use std::sync::atomic::{AtomicBool, Ordering};

/// Process-wide cache of the `session.unread_indicator` toggle (default on).
static UNREAD_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the unread-session indicator feature is enabled.
pub fn unread_enabled() -> bool {
    UNREAD_ENABLED.load(Ordering::Relaxed)
}

/// Update the cached unread-indicator flag from resolved config.
pub fn set_unread_enabled(on: bool) {
    UNREAD_ENABLED.store(on, Ordering::Relaxed);
}

/// Process-wide cache of the `session.favorites_first` toggle (default on).
static FAVORITES_FIRST: AtomicBool = AtomicBool::new(true);

/// Whether favorited rows pin to the top of their sibling scope outside the
/// Attention sort.
pub fn favorites_first() -> bool {
    FAVORITES_FIRST.load(Ordering::Relaxed)
}

/// Update the cached favorites-first flag from resolved config.
pub fn set_favorites_first(on: bool) {
    FAVORITES_FIRST.store(on, Ordering::Relaxed);
}

pub use config::profile_config::{
    load_profile_config, merge_configs, resolve_config, resolve_config_or_warn,
    save_profile_config, validate_capability_format, validate_check_interval, validate_env_format,
    validate_memory_limit, validate_network_format, validate_port_mapping_format,
    validate_security_opt_format, validate_volume_format, ProfileConfig,
};
pub use config::repo_config::{
    check_repo_trust, execute_hooks, execute_hooks_in_container, load_repo_config,
    merge_repo_config, profile_to_repo_config, repo_config_to_profile, resolve_config_with_repo,
    resolve_config_with_repo_or_warn, save_repo_config, trust_repo, HookTimeout, HooksConfig,
    RepoConfig, RepoTrust, TrustSurface,
};
pub use projects::{Project, ProjectOverrides, ProjectScope};
pub use recovery::HookTimeoutScope;
pub use scope::SessionScope;
#[cfg(test)]
pub(crate) use storage::migration_backups;
pub(crate) use storage::{
    acquire_session_title_lock, acquire_storage_flock, acquire_storage_shared_flock, atomic_write,
    backup_before_migration, read_file_no_follow, replace_file_no_follow, resolve_symlink_chain,
    try_acquire_storage_flock, GroupMovePlan, StorageFlock, STORAGE_LOCK_FILENAME,
};
pub use storage::{
    load_recent_projects, load_workspace_ordering, recent_project_entry_for, record_recent_project,
    update_workspace_ordering, RecentProjectEntry, Storage, WorkspaceOrdering,
};

use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// App dir name under the XDG config base (`$XDG_CONFIG_HOME`, default `~/.config`).
pub const APP_DIR_NAME_XDG: &str = if cfg!(debug_assertions) {
    "agent-of-empires-dev"
} else {
    "agent-of-empires"
};

/// Home-dotfile app dir name (under `$HOME`).
pub const APP_DIR_NAME_OTHER: &str = if cfg!(debug_assertions) {
    ".agent-of-empires-dev"
} else {
    ".agent-of-empires"
};

/// Resolve the XDG-style config base directory: `$XDG_CONFIG_HOME` when set to an absolute path,
/// otherwise `~/.config`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn xdg_config_base() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        if dir.is_absolute() {
            return Ok(dir);
        }
    }
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".config"))
}

/// Whether `$XDG_CONFIG_HOME` is set to an absolute path, i.e. the user has meaningfully opted into
/// the XDG layout.
#[cfg(target_os = "macos")]
fn xdg_config_home_set() -> bool {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(|v| std::path::Path::new(&v).is_absolute())
        .unwrap_or(false)
}

/// macOS app-dir resolution: prefer the XDG location, fall back to the home-dotfile location,
/// without ever moving data.
#[cfg(target_os = "macos")]
fn macos_app_dir(xdg_name: &str, legacy_name: &str) -> Option<PathBuf> {
    let xdg = xdg_config_base().ok()?.join(xdg_name);
    let legacy = dirs::home_dir()?.join(legacy_name);
    Some(resolve_app_dir_with_fallback(
        xdg,
        legacy,
        xdg_config_home_set(),
    ))
}

/// Pure precedence used by [`macos_app_dir`]; split out so the rule is testable
/// off-macOS. See that function for the meaning of each branch.
#[cfg(any(target_os = "macos", test))]
fn resolve_app_dir_with_fallback(xdg: PathBuf, legacy: PathBuf, xdg_env_set: bool) -> PathBuf {
    if xdg.exists() {
        xdg
    } else if legacy.exists() {
        legacy
    } else if xdg_env_set {
        xdg
    } else {
        legacy
    }
}

pub fn get_app_dir() -> Result<PathBuf> {
    let dir = get_app_dir_path()?;
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

/// Whether the app data dir already exists, **without** creating it (unlike [`get_app_dir`], which
/// auto-creates).
pub fn app_dir_exists() -> bool {
    get_app_dir_path().map(|p| p.exists()).unwrap_or(false)
}

/// The app dir of one build namespace, named by its XDG-layout and home-dotfile directory names.
fn app_dir_for(xdg_name: &str, other_name: &str) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let _ = other_name;
        xdg_config_base().ok().map(|base| base.join(xdg_name))
    }
    #[cfg(target_os = "macos")]
    {
        macos_app_dir(xdg_name, other_name)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = xdg_name;
        dirs::home_dir().map(|home| home.join(other_name))
    }
}

fn get_app_dir_path() -> Result<PathBuf> {
    app_dir_for(APP_DIR_NAME_XDG, APP_DIR_NAME_OTHER)
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))
}

/// Detect the first-launch case where a debug build is being run on a machine that has populated
/// release-build state in `~/.agent-of-empires` but no dev-build state yet.
pub fn debug_namespace_drift() -> Option<(PathBuf, PathBuf)> {
    if !cfg!(debug_assertions) {
        return None;
    }

    let release_dir = app_dir_for("agent-of-empires", ".agent-of-empires")?;
    let dev_dir = app_dir_for(APP_DIR_NAME_XDG, APP_DIR_NAME_OTHER)?;

    let release_populated = fs::read_dir(&release_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);

    if release_populated && !dev_dir.exists() {
        Some((release_dir, dev_dir))
    } else {
        None
    }
}

/// The app dir of the *other* build namespace: the release dir from a debug build, the dev dir from
/// a release build.
pub(crate) fn sibling_namespace_app_dir() -> Option<PathBuf> {
    let (xdg, other) = if cfg!(debug_assertions) {
        ("agent-of-empires", ".agent-of-empires")
    } else {
        ("agent-of-empires-dev", ".agent-of-empires-dev")
    };
    app_dir_for(xdg, other)
}

/// Format the user-facing warning shown when `debug_namespace_drift()` fires.
pub fn format_debug_namespace_warning(release: &Path, dev: &Path) -> String {
    format!(
        "Debug builds now use an isolated app dir:\n  \
         {}\n\n\
         Your existing state in\n  \
         {}\n\
         is not visible to this build.\n\n\
         To migrate it, run:\n  \
         cp -r {} {}\n\n\
         Otherwise, do nothing — this notice will not repeat once the dev dir exists.\n\
         See docs/development.md for details.",
        dev.display(),
        release.display(),
        release.display(),
        dev.display(),
    )
}

pub fn get_profile_dir(profile: &str) -> Result<PathBuf> {
    let base = get_app_dir()?;
    let resolved;
    let profile_name = if profile.is_empty() {
        resolved = config::resolve_default_profile();
        resolved.as_str()
    } else {
        profile
    };
    let dir = base.join("profiles").join(profile_name);
    if !dir.exists() {
        // Only a name about to be created runs the strict grammar; an existing directory still
        // opens, so older malformed profiles stay listable and deletable.
        validate_new_profile_name(profile_name)?;
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

/// Resolve the on-disk profile directory path WITHOUT creating it.
pub fn get_profile_dir_path(profile: &str) -> Result<PathBuf> {
    let base = get_app_dir()?;
    let resolved;
    let profile_name = if profile.is_empty() {
        resolved = config::resolve_default_profile();
        resolved.as_str()
    } else {
        profile
    };
    Ok(base.join("profiles").join(profile_name))
}

/// Resolve the effective profile name for a read/reference operation.
pub fn resolve_existing_profile(profile: &str) -> Result<String> {
    let name = if profile.is_empty() {
        config::resolve_default_profile()
    } else {
        profile.to_string()
    };
    validate_profile_name(&name)?;
    let dir = get_profile_dir_path(&name)?;
    if !dir.exists() {
        anyhow::bail!("Profile '{name}' does not exist. Create it with: aoe profile create {name}");
    }
    Ok(name)
}

pub fn list_profiles() -> Result<Vec<String>> {
    // Test-only failure injection: when set, the next call returns Err and the flag clears.
    #[cfg(test)]
    if FAIL_NEXT_LIST_PROFILES.swap(false, std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("list_profiles failure injected for test");
    }
    let base = get_app_dir()?;
    let profiles_dir = base.join("profiles");

    if !profiles_dir.exists() {
        return Ok(vec![]);
    }

    list_profile_names_in(&profiles_dir)
}

/// Picker order: alphabetical, with a profile named `default` last.
pub fn sort_profiles_for_display(profiles: &mut [String]) {
    profiles.sort_by(|a, b| {
        (a == "default")
            .cmp(&(b == "default"))
            .then_with(|| a.cmp(b))
    });
}

/// [`list_profiles`] in picker order, for surfaces a human chooses from.
/// Programmatic resolution keeps [`list_profiles`].
pub fn list_profiles_for_display() -> Result<Vec<String>> {
    let mut profiles = list_profiles()?;
    sort_profiles_for_display(&mut profiles);
    Ok(profiles)
}

/// Refuse an explicit `-p`/`--profile` naming a profile that does not exist, so a typo never
/// reaches [`get_profile_dir`] and mints a stray directory.
pub fn require_known_profile(profile: &str) -> Result<()> {
    if profile.is_empty() {
        return Ok(());
    }
    let known = list_profiles()?;
    if known.is_empty() || known.iter().any(|p| p == profile) {
        return Ok(());
    }
    // Escaped: arbitrary input headed for stderr and the log.
    let shown = profile.escape_debug();
    anyhow::bail!(
        "Profile '{shown}' does not exist. Create it explicitly with \
         `aoe profile create {shown}`; a bare -p/--profile will not mint one \
         (guards against stray profiles from typos or session titles). \
         Run `aoe profile list` to see existing profiles."
    );
}

#[cfg(test)]
pub(crate) static FAIL_NEXT_LIST_PROFILES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// RAII guard for the `FAIL_NEXT_LIST_PROFILES` test seam.
#[cfg(test)]
pub(crate) struct FailNextListProfilesGuard;

#[cfg(test)]
impl FailNextListProfilesGuard {
    pub(crate) fn new() -> Self {
        FAIL_NEXT_LIST_PROFILES.store(true, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for FailNextListProfilesGuard {
    fn drop(&mut self) {
        FAIL_NEXT_LIST_PROFILES.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Enumerate profile directory names in `profiles_dir`, skipping symlinks.
pub(crate) fn list_profile_names_in(profiles_dir: &std::path::Path) -> Result<Vec<String>> {
    let mut profiles = Vec::new();
    for entry in fs::read_dir(profiles_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                profiles.push(name.to_string());
            }
        }
    }
    // Resolution input: `resolve_default_profile` takes the first entry, so
    // this stays plain. Picker order lives in `sort_profiles_for_display`.
    profiles.sort();
    Ok(profiles)
}

#[cfg(test)]
mod profile_listing_tests {
    //! Regression tests for the "three of every folder" bug (2026-04-25).
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn list_profile_names_lists_real_dirs_in_plain_order() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let dir = tmp.path();
        for name in ["default", "alpha", "personal", "zeta"] {
            fs::create_dir(dir.join(name)).unwrap();
        }
        // The cs/cxa pattern: aliases are symlinks pointing at `default`.
        symlink("default", dir.join("forit-work")).unwrap();
        symlink("default", dir.join("wma-work")).unwrap();
        fs::write(dir.join("README"), "ignore me").unwrap();

        // Symlinked aliases would duplicate the linked profile's sessions (the
        // three-of-every-folder bug); "default" sorts like any other name here
        // because resolution takes the first entry.
        let names = list_profile_names_in(dir).expect("list");
        assert_eq!(names, ["alpha", "default", "personal", "zeta"]);
    }

    #[test]
    fn sort_profiles_for_display_sinks_default_to_last() {
        let mut names: Vec<String> = ["zeta", "default", "beta", "alpha"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sort_profiles_for_display(&mut names);
        assert_eq!(
            names,
            vec![
                "alpha".to_string(),
                "beta".to_string(),
                "zeta".to_string(),
                "default".to_string(),
            ],
            "default must sort last; all other profiles stay alphabetical"
        );

        let mut plain: Vec<String> = ["b", "a"].iter().map(|s| s.to_string()).collect();
        sort_profiles_for_display(&mut plain);
        assert_eq!(plain, vec!["a".to_string(), "b".to_string()]);
        let mut lone = vec!["default".to_string()];
        sort_profiles_for_display(&mut lone);
        assert_eq!(lone, vec!["default".to_string()]);
    }
}

/// Validate `AOE_INSTANCE_ID` is safe as a single path component and
/// for shell interpolation. Allowlist `[A-Za-z0-9_-]`, max 64 bytes.
pub(crate) fn validate_instance_id(id: &str) -> Result<()> {
    if id.is_empty() {
        anyhow::bail!("AOE_INSTANCE_ID must not be empty");
    }
    if id.len() > 64 {
        anyhow::bail!("AOE_INSTANCE_ID too long ({} bytes, max 64)", id.len());
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        anyhow::bail!("AOE_INSTANCE_ID contains disallowed characters");
    }
    Ok(())
}

/// Validate that `name` is a safe, single-component profile name.
fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("Profile name cannot be empty");
    }
    if name.eq_ignore_ascii_case("all") {
        anyhow::bail!("Profile name 'all' is reserved");
    }
    // Unix Path treats `\` as a regular byte, so backslashes pass the components check below.
    if name.contains('\\') {
        anyhow::bail!("Profile name cannot contain path separators");
    }
    let mut components = Path::new(name).components();
    let first = components.next();
    if components.next().is_some() {
        anyhow::bail!("Profile name cannot contain path separators");
    }
    match first {
        Some(std::path::Component::Normal(c)) if c == std::ffi::OsStr::new(name) => Ok(()),
        _ => anyhow::bail!(
            "Profile name '{}' is not a valid single-component name",
            name
        ),
    }
}

/// Grammar for a profile about to be created: `[A-Za-z0-9_-]`, at most 64 characters, on top of the
/// traversal guard in `validate_profile_name`.
fn validate_new_profile_name(name: &str) -> Result<()> {
    validate_profile_name(name)?;
    if name.len() > 64 {
        anyhow::bail!("Profile name is too long ({} chars; max 64)", name.len());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        // Escaped: arbitrary input headed for stderr and the log.
        anyhow::bail!(
            "Profile name '{}' has disallowed characters (allowed: A-Z a-z 0-9 _ -)",
            name.escape_debug()
        );
    }
    Ok(())
}

pub fn create_profile(name: &str) -> Result<()> {
    validate_new_profile_name(name)?;

    let profiles = list_profiles()?;
    if profiles.contains(&name.to_string()) {
        anyhow::bail!("Profile '{}' already exists", name);
    }

    get_profile_dir(name)?;
    Ok(())
}

pub fn delete_profile(name: &str) -> Result<()> {
    validate_profile_name(name)?;

    let base = get_app_dir()?;
    let profile_dir = base.join("profiles").join(name);

    if !profile_dir.exists() {
        anyhow::bail!("Profile '{}' does not exist", name);
    }

    // The invariant is "at least one profile must exist", a count, not a name.
    // Any profile is deletable as long as deleting it would not leave zero.
    if list_profiles()?.len() <= 1 {
        anyhow::bail!("Cannot delete '{}': at least one profile must exist", name);
    }

    fs::remove_dir_all(&profile_dir)?;
    Ok(())
}

/// The source keeps the permissive traversal guard so a stray minted by an older binary stays
/// renameable; the destination is a new profile and is held to the create grammar.
pub fn rename_profile(old_name: &str, new_name: &str) -> Result<()> {
    validate_profile_name(old_name)?;
    validate_new_profile_name(new_name)?;

    let base = get_app_dir()?;
    let old_dir = base.join("profiles").join(old_name);
    let new_dir = base.join("profiles").join(new_name);

    if !old_dir.exists() {
        anyhow::bail!("Profile '{}' does not exist", old_name);
    }
    if new_dir.exists() {
        anyhow::bail!("Profile '{}' already exists", new_name);
    }

    fs::rename(&old_dir, &new_dir)?;

    // Update default profile if the renamed profile was the default
    if let Some(config) = load_config()? {
        if config.default_profile == old_name {
            set_default_profile(new_name)?;
        }
    }

    Ok(())
}

pub fn set_default_profile(name: &str) -> Result<()> {
    update_config(|config| {
        config.default_profile = name.to_string();
    })?;
    Ok(())
}

/// One file's probe result: either the parse errored out (per-key values fall back to defaults), or
/// it loaded but some keys were unrecognized and silently dropped.
pub struct ConfigProbe {
    pub load_err: Option<String>,
    pub ignored_keys: Vec<String>,
}

/// Try to load a config file, and on success enumerate its unrecognized keys.
fn probe<T, E: std::fmt::Display>(
    load: impl FnOnce() -> Result<T, E>,
    ignored: impl FnOnce(&T) -> Vec<String>,
) -> ConfigProbe {
    match load() {
        Ok(cfg) => ConfigProbe {
            load_err: None,
            ignored_keys: ignored(&cfg),
        },
        Err(e) => ConfigProbe {
            load_err: Some(e.to_string()),
            ignored_keys: Vec::new(),
        },
    }
}

/// Probe the global `config.toml`: run the real `Config::load` and, if it succeeded, run
/// `serde_ignored` to enumerate any unknown struct fields at any depth.
pub fn probe_global_config() -> ConfigProbe {
    probe(Config::load, |_| Config::config_ignored_keys())
}

/// Same shape as [`probe_global_config`] but for a profile's `config.toml`.
pub fn probe_profile_config(profile: &str) -> ConfigProbe {
    probe(
        || config::profile_config::load_profile_config(profile),
        config::profile_config::profile_config_ignored_keys,
    )
}

/// Human-readable path of the global `config.toml`, with a stable fallback so
/// the message reads sensibly when the app dir can't even be resolved.
pub(crate) fn config_path_display() -> String {
    config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "config.toml".to_string())
}

/// Which classes of probe finding a caller wants surfaced.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WarningClass {
    /// Parse failures and unrecognized keys.
    All,
    /// Unrecognized keys only.
    IgnoredKeysOnly,
}

/// Format one probe result as a user-visible line, or `None` when the file is clean (or failed to
/// parse under [`WarningClass::IgnoredKeysOnly`], which carries no ignored-key information anyway).
fn format_probe(
    probe: &ConfigProbe,
    scope_label: &str,
    path_display: &str,
    class: WarningClass,
) -> Option<String> {
    if let Some(e) = probe.load_err.as_deref() {
        (class == WarningClass::All)
            .then(|| format!("Failed to load {scope_label} ({path_display}); using defaults.\n{e}"))
    } else if !probe.ignored_keys.is_empty() {
        Some(format!(
            "Unrecognized keys in {scope_label} ({path_display}) were ignored: {}",
            probe.ignored_keys.join(", ")
        ))
    } else {
        None
    }
}

/// Probe the global config and the active profile's config, formatting the
/// requested classes into one blank-line-separated message.
fn collect_startup_warnings(profile: &str, class: WarningClass) -> Option<String> {
    let mut messages: Vec<String> = Vec::new();

    if let Some(msg) = format_probe(
        &probe_global_config(),
        "global config",
        &config_path_display(),
        class,
    ) {
        messages.push(msg);
    }

    let effective = if profile.is_empty() {
        config::resolve_default_profile()
    } else {
        profile.to_string()
    };
    // Non-creating resolver: `get_profile_config_path` goes through the creating `get_profile_dir`,
    // so naming an unknown profile (`aoe list -p ghost`) would birth `profiles/ghost/` here, before
    // the command's own `resolve_existing_profile` gets to reject it.
    let profile_path_display = get_profile_dir_path(&effective)
        .map(|p| p.join("config.toml").display().to_string())
        .unwrap_or_else(|_| format!("profiles/{effective}/config.toml"));
    let profile_scope = format!("profile config '{effective}'");
    if let Some(msg) = format_probe(
        &probe_profile_config(&effective),
        &profile_scope,
        &profile_path_display,
        class,
    ) {
        messages.push(msg);
    }

    if messages.is_empty() {
        None
    } else {
        Some(messages.join("\n\n"))
    }
}

/// Probe the global config and the active profile's config at startup so the TUI can show a single
/// user-visible warning when either fails to parse OR contains unrecognized keys.
pub fn collect_startup_config_warnings(profile: &str) -> Option<String> {
    collect_startup_warnings(profile, WarningClass::All)
}

/// Like [`collect_startup_config_warnings`], but only the unrecognized-keys class.
pub fn collect_startup_ignored_key_warnings(profile: &str) -> Option<String> {
    collect_startup_warnings(profile, WarningClass::IgnoredKeysOnly)
}

// ── TUI presence ────────────────────────────────────────────────────────────

const TUI_PRESENCE_DIR: &str = "tui-presence";
const TUI_ACTIVITY_DIR: &str = "tui-activity";

fn tui_dir(name: &str) -> Option<std::path::PathBuf> {
    get_app_dir().ok().map(|d| d.join(name))
}

fn own_tui_file(name: &str) -> Option<std::path::PathBuf> {
    tui_dir(name).map(|d| d.join(std::process::id().to_string()))
}

/// Write (or touch) this process's presence file so the push consumer knows a TUI is running and
/// other TUIs can count us.
pub fn write_tui_heartbeat() {
    if let Some(dir) = tui_dir(TUI_PRESENCE_DIR) {
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join(std::process::id().to_string()), b"");
    }
}

/// Record real user input in this TUI.
pub fn write_tui_activity() {
    if let Some(dir) = tui_dir(TUI_ACTIVITY_DIR) {
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join(std::process::id().to_string()), b"");
    }
}

/// Remove this process's presence file on TUI exit.
pub fn clear_tui_heartbeat() {
    if let Some(file) = own_tui_file(TUI_PRESENCE_DIR) {
        let _ = fs::remove_file(file);
    }
    if let Some(file) = own_tui_file(TUI_ACTIVITY_DIR) {
        let _ = fs::remove_file(file);
    }
}

/// Count TUI presence files whose mtime is fresh within `threshold`, sweeping any stale entries
/// left behind by crashed processes.
pub fn count_active_tuis(threshold: Duration) -> usize {
    count_fresh_tui_files(TUI_PRESENCE_DIR, threshold)
}

fn count_fresh_tui_files(dir_name: &str, threshold: Duration) -> usize {
    let dir = match tui_dir(dir_name) {
        Some(d) => d,
        None => return 0,
    };
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut live = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let fresh = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or(Duration::MAX) < threshold)
            .unwrap_or(false);
        if fresh {
            live += 1;
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    live
}

/// Returns true if any TUI received real input within `threshold`.
pub fn is_tui_active(threshold: Duration) -> bool {
    count_fresh_tui_files(TUI_ACTIVITY_DIR, threshold) > 0
}

#[cfg(test)]
mod tests {
    use super::test_support::{isolate_app_dir, AppDirGuard};
    use super::*;

    fn app_dir(root: impl AsRef<Path>) -> PathBuf {
        let root = root.as_ref();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let dir = root.join(".config").join(APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let dir = root.join(APP_DIR_NAME_OTHER);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[serial_test::serial]
    fn xdg_config_base_uses_only_an_absolute_xdg_config_home() {
        let temp = tempfile::TempDir::new().unwrap();
        let _home = super::test_support::isolate_home(temp.path());
        let custom = temp.path().join("custom-xdg");
        let _xdg = super::test_support::EnvGuard::set(&[("XDG_CONFIG_HOME", &custom)]);

        assert_eq!(xdg_config_base().unwrap(), custom);
        assert_eq!(get_app_dir_path().unwrap(), custom.join(APP_DIR_NAME_XDG));

        let _relative = super::test_support::EnvGuard::set(&[("XDG_CONFIG_HOME", "relative/path")]);
        assert_eq!(xdg_config_base().unwrap(), temp.path().join(".config"));
        let _unset = super::test_support::EnvGuard::unset(&["XDG_CONFIG_HOME"]);
        assert_eq!(xdg_config_base().unwrap(), temp.path().join(".config"));
    }

    #[test]
    fn app_dir_fallback_never_moves_existing_data() {
        // (case, xdg dir exists, legacy dir exists, XDG_CONFIG_HOME set, xdg wins)
        let cases = [
            ("both present", true, true, true, true),
            ("both present, env unset", true, true, false, true),
            ("only legacy present", false, true, true, false),
            ("fresh install, env set", false, false, true, true),
            ("fresh install, env unset", false, false, false, false),
        ];
        for (case, xdg_exists, legacy_exists, env_set, xdg_wins) in cases {
            let tmp = tempfile::TempDir::new().unwrap();
            let xdg = tmp.path().join(".config").join("agent-of-empires");
            let legacy = tmp.path().join(".agent-of-empires");
            for (dir, exists) in [(&xdg, xdg_exists), (&legacy, legacy_exists)] {
                if exists {
                    fs::create_dir_all(dir).unwrap();
                }
            }
            let want = if xdg_wins { &xdg } else { &legacy };
            assert_eq!(
                &resolve_app_dir_with_fallback(xdg.clone(), legacy.clone(), env_set),
                want,
                "{case}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_tui_presence_counts_and_sweeps() {
        let temp = isolate_app_dir();
        let pdir = app_dir(&temp).join(TUI_PRESENCE_DIR);

        write_tui_heartbeat();
        assert_eq!(count_active_tuis(Duration::from_secs(30)), 1);
        assert!(
            !is_tui_active(Duration::from_secs(30)),
            "a live but untouched TUI must not suppress phone notifications"
        );

        write_tui_activity();
        assert!(is_tui_active(Duration::from_secs(30)));

        fs::write(pdir.join("999999"), b"").unwrap();
        assert_eq!(count_active_tuis(Duration::from_secs(30)), 2);

        assert_eq!(count_active_tuis(Duration::ZERO), 0);
        assert_eq!(count_fresh_tui_files(TUI_ACTIVITY_DIR, Duration::ZERO), 0);
        assert!(!is_tui_active(Duration::from_secs(30)));
        assert_eq!(fs::read_dir(&pdir).unwrap().count(), 0);

        write_tui_heartbeat();
        fs::write(pdir.join("999999"), b"").unwrap();
        clear_tui_heartbeat();
        assert_eq!(count_active_tuis(Duration::from_secs(30)), 1);
    }

    /// Write `global` and/or `profile` config into an isolated app dir and return the guard.
    fn seed_configs(global: Option<&str>, profile: Option<&str>) -> AppDirGuard {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        if let Some(body) = global {
            fs::write(dir.join("config.toml"), body).unwrap();
        }
        if let Some(body) = profile {
            let profile_dir = dir.join("profiles").join("default");
            fs::create_dir_all(&profile_dir).unwrap();
            fs::write(profile_dir.join("config.toml"), body).unwrap();
        }
        temp
    }

    const BAD_TYPE: &str = "[sandbox]\nenabled_by_default = \"not-a-bool\"\n";
    const UNKNOWN_KEY: &str = "[sandbox]\nenabled_by_default = true\nprivildged = true\n";

    #[test]
    #[serial_test::serial]
    fn startup_config_warnings_report_parse_failures_and_unknown_keys() {
        let default_config = toml::to_string_pretty(&config::Config::default()).unwrap();
        // (case, global, profile, profile argument, fragments the warning must contain; none
        // means no warning)
        type Case<'a> = (
            &'static str,
            Option<&'a str>,
            Option<&'static str>,
            &'static str,
            &'static [&'static str],
        );
        let cases: &[Case] = &[
            ("no config", None, None, "", &[]),
            (
                "round-tripped defaults",
                Some(&default_config),
                Some("description = \"work\"\n[sandbox]\nenabled_by_default = true\n"),
                "default",
                &[],
            ),
            (
                "documented map keys",
                Some(
                    "[session]\n\
                     custom_agents = { myagent = \"true\" }\n\
                     [agents.claude.status_map]\n\
                     SessionStart = \"running\"\n\
                     [tools.lazygit]\n\
                     command = \"lazygit\"\n\
                     [plugins.\"aoe.web\"]\n\
                     enabled = true\n",
                ),
                None,
                "",
                &[],
            ),
            (
                "unparseable global",
                Some(BAD_TYPE),
                None,
                "",
                &["Failed to load global config", "config.toml"],
            ),
            (
                "unparseable profile",
                None,
                Some("[worktree]\nenabled = \"not-a-bool\"\n"),
                "default",
                &["Failed to load profile config 'default'"],
            ),
            (
                "unknown nested global key",
                Some(UNKNOWN_KEY),
                None,
                "",
                &["Unrecognized keys in global config", "sandbox.privildged"],
            ),
            (
                "unknown profile key",
                None,
                Some("[sandbox]\nprivildged = true\n"),
                "default",
                &[
                    "Unrecognized keys in profile config 'default'",
                    "sandbox.privildged",
                ],
            ),
            (
                "typo inside a documented map section",
                Some("[agents.claude]\nstatus_maap = { foo = \"bar\" }\n"),
                None,
                "",
                &["agents.claude.status_maap"],
            ),
        ];
        for (case, global, profile, arg, fragments) in cases {
            let _temp = seed_configs(*global, *profile);
            let warning = collect_startup_config_warnings(arg);
            if fragments.is_empty() {
                assert!(warning.is_none(), "{case}: got {warning:?}");
            }
            for fragment in *fragments {
                let warning = warning.as_deref().unwrap_or_default();
                assert!(warning.contains(fragment), "{case}: got {warning:?}");
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn ignored_key_warnings_name_the_key_but_stay_silent_on_a_parse_failure() {
        let _temp = seed_configs(Some(UNKNOWN_KEY), None);
        let warning = collect_startup_ignored_key_warnings("").expect("ignored key is reported");
        assert!(warning.contains("sandbox.privildged"));
        assert!(!warning.contains("Failed to load"));

        let _temp = seed_configs(Some(BAD_TYPE), None);
        assert!(collect_startup_ignored_key_warnings("").is_none());
    }

    fn release_dir_in(root: impl AsRef<Path>) -> PathBuf {
        let root = root.as_ref();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let d = root.join(".config").join("agent-of-empires");
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let d = root.join(".agent-of-empires");
        d
    }

    #[test]
    #[serial_test::serial]
    fn drift_fires_only_for_a_populated_release_dir_with_no_dev_dir() {
        let temp = isolate_app_dir();
        assert!(debug_namespace_drift().is_none(), "no release dir at all");

        let release = release_dir_in(&temp);
        fs::create_dir_all(&release).unwrap();
        assert!(debug_namespace_drift().is_none(), "release dir is empty");

        fs::create_dir_all(release.join("profiles")).unwrap();
        let drift = debug_namespace_drift();
        if cfg!(debug_assertions) {
            let (r, d) = drift.expect("expected drift on a debug build");
            assert_eq!(r, release);
            assert!(d.to_string_lossy().contains("-dev"));
        } else {
            assert!(drift.is_none());
        }

        let _dev = app_dir(&temp);
        assert!(debug_namespace_drift().is_none(), "dev dir now exists");
    }

    #[test]
    #[serial_test::serial]
    fn test_implicit_resolution_ignores_picker_order_on_mixed_registry() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("default")).unwrap();
        fs::create_dir_all(dir.join("profiles").join("work")).unwrap();

        assert_eq!(
            list_profiles().unwrap(),
            vec!["default".to_string(), "work".to_string()],
            "list_profiles is the resolution input and stays plainly sorted"
        );
        assert_eq!(config::resolve_default_profile(), "default");
        assert_eq!(
            get_profile_dir("").unwrap(),
            dir.join("profiles").join("default")
        );
        assert_eq!(
            list_profiles_for_display().unwrap(),
            vec!["work".to_string(), "default".to_string()],
            "only the picker order sinks default"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[serial_test::serial]
    fn delete_profile_validates_names_but_removes_strays_and_keeps_the_last() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        let profiles = dir.join("profiles");
        let stray = "work 0123456789abcdef Some Title";
        fs::create_dir_all(profiles.join(stray)).unwrap();
        fs::create_dir_all(profiles.join("real")).unwrap();
        let bystander = dir.join("bystander");
        fs::create_dir_all(&bystander).unwrap();

        for malicious in ["..", "../bystander", "/etc", "a/b", "", "all"] {
            let err = delete_profile(malicious).expect_err(&format!(
                "delete_profile({malicious:?}) must fail validation"
            ));
            let msg = err.to_string();
            assert!(
                msg.contains("Profile name")
                    || msg.contains("cannot be empty")
                    || msg.contains("reserved")
                    || msg.contains("path separators"),
                "unexpected error for {malicious:?}: {msg}"
            );
        }
        assert!(bystander.exists(), "bystander directory must survive");

        delete_profile(stray).expect("a pre-existing spaced stray must be deletable");
        assert!(!profiles.join(stray).exists());

        let err = delete_profile("real").expect_err("deleting the last profile must fail");
        assert!(err.to_string().contains("at least one profile must exist"));
        assert!(profiles.join("real").exists());
    }

    #[test]
    fn validate_profile_name_accepts_existing_dirs_and_rejects_traversal() {
        for name in ["work", "personal", "client-a", ".hidden", "1", "main"] {
            validate_profile_name(name)
                .unwrap_or_else(|e| panic!("expected {name:?} to validate: {e}"));
        }
        for bad in ["", "..", ".", "/etc", "a/b", "a\\b", "all", "ALL"] {
            validate_profile_name(bad)
                .err()
                .unwrap_or_else(|| panic!("expected {bad:?} to be rejected"));
        }
    }

    #[test]
    fn validate_new_profile_name_gates_creation_more_tightly() {
        for name in [
            "default",
            "work",
            "personal-main",
            "team_b",
            "main",
            "client-a",
            "1",
        ] {
            validate_new_profile_name(name)
                .unwrap_or_else(|e| panic!("expected {name:?} to pass create gate: {e}"));
        }
        for bad in [
            "work 0123456789abcdef Some Title",
            "ZZTEST spaced name",
            "has space",
            "tab\tname",
            "emoji\u{1f600}",
            "all",
            "..",
            "a/b",
            ".hidden",
            "a.b",
        ] {
            validate_new_profile_name(bad)
                .err()
                .unwrap_or_else(|| panic!("expected create gate to reject {bad:?}"));
        }
        validate_new_profile_name(&"a".repeat(65)).expect_err("65 chars is too long");

        let text = validate_new_profile_name("bad\u{1b}[31mname")
            .expect_err("control char must be rejected")
            .to_string();
        assert!(
            !text.contains('\u{1b}') && text.contains("\\u{1b}"),
            "expected only the escaped ESC in the error: {text:?}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[serial_test::serial]
    fn test_get_profile_dir_refuses_to_vivify_stray() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("work")).unwrap();

        let stray = "work 0123456789abcdef Some Title";
        let err = get_profile_dir(stray).expect_err("stray name must be refused");
        assert!(
            err.to_string().contains("disallowed characters")
                || err.to_string().contains("path separators"),
            "unexpected error: {err}"
        );
        assert!(
            !dir.join("profiles").join(stray).exists(),
            "stray profile dir must NOT have been created"
        );
        let good = get_profile_dir("personal").expect("valid name must create dir");
        assert!(good.exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_require_known_profile_rejects_unknown_when_registry_nonempty() {
        let temp = isolate_app_dir();
        require_known_profile("main")
            .expect("first-run profile must be allowed when registry empty");
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("work")).unwrap();

        let err =
            require_known_profile("ghost-profile").expect_err("unknown profile must be refused");
        assert!(
            err.to_string().contains("does not exist"),
            "unexpected error: {err}"
        );
        assert!(
            !dir.join("profiles").join("ghost-profile").exists(),
            "guard must not vivify the unknown profile"
        );

        require_known_profile("work").expect("existing profile must be allowed");
        require_known_profile("").expect("empty/default profile must be allowed");

        let err = require_known_profile("nope\u{1b}[31m")
            .expect_err("unknown profile with control chars must be refused");
        let text = err.to_string();
        assert!(
            !text.contains('\u{1b}') && text.contains("\\u{1b}"),
            "expected escaped ESC in the error: {text:?}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[serial_test::serial]
    fn rename_profile_gates_the_destination_but_repairs_a_stray_source() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("real")).unwrap();

        let too_long = "a".repeat(65);
        for bad in [
            "has space",
            "emoji\u{1f600}",
            "all",
            "a.b",
            too_long.as_str(),
        ] {
            let err = rename_profile("real", bad)
                .err()
                .unwrap_or_else(|| panic!("expected rename to refuse destination {bad:?}"));
            let msg = err.to_string();
            assert!(
                msg.contains("disallowed characters")
                    || msg.contains("reserved")
                    || msg.contains("too long"),
                "unexpected error for {bad:?}: {msg}"
            );
            assert!(
                dir.join("profiles").join("real").exists(),
                "source must be untouched after refusing {bad:?}"
            );
            assert!(!dir.join("profiles").join(bad).exists());
        }

        let stray = "work 0123456789abcdef Some Title";
        fs::create_dir_all(dir.join("profiles").join(stray)).unwrap();
        rename_profile(stray, "work").expect("a spaced stray must be renameable");
        assert!(!dir.join("profiles").join(stray).exists());
        assert!(dir.join("profiles").join("work").exists());

        fs::create_dir_all(dir.join("bystander")).unwrap();
        let err = rename_profile("../bystander", "escaped").expect_err("traversal source");
        assert!(err.to_string().contains("path separators"), "{err}");
        assert!(dir.join("bystander").exists());
        assert!(!dir.join("profiles").join("escaped").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_load_profile_config_does_not_create_dir_for_unknown_profile() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("real")).unwrap();
        let unknown_dir = dir.join("profiles").join("does-not-exist");
        assert!(!unknown_dir.exists());

        let cfg = crate::session::config::profile_config::load_profile_config("does-not-exist")
            .expect("loading config for an unknown profile must succeed with defaults");
        assert!(
            !crate::session::config::profile_config::profile_has_overrides(&cfg),
            "unknown profile must load to defaults",
        );
        assert!(
            !unknown_dir.exists(),
            "load_profile_config must not create profiles/<unknown>/ as a side effect",
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_existing_profile_never_creates_a_profile_it_was_not_asked_to_bootstrap() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        assert!(list_profiles().unwrap().is_empty());
        assert_eq!(
            resolve_existing_profile("").unwrap(),
            "main",
            "fresh install"
        );
        assert_eq!(list_profiles().unwrap(), vec!["main".to_string()]);

        let err = resolve_existing_profile("ghost").expect_err("unknown profile must error");
        let msg = err.to_string();
        assert!(msg.contains("does not exist"), "unexpected message: {msg}");
        assert!(
            msg.contains("aoe profile create"),
            "unexpected message: {msg}"
        );
        assert!(!dir.join("profiles").join("ghost").exists());

        create_profile("newly-created").unwrap();
        assert_eq!(
            resolve_existing_profile("newly-created").unwrap(),
            "newly-created"
        );

        fs::create_dir_all(dir.join("etc")).unwrap();
        let err =
            resolve_existing_profile("../etc").expect_err("path traversal name must be rejected");
        assert!(err.to_string().contains("path separators"), "{err}");

        fs::write(
            dir.join("config.toml"),
            r#"default_profile = "deleted-profile""#,
        )
        .unwrap();
        let err = resolve_existing_profile("").expect_err("stale default must error");
        assert!(err.to_string().contains("does not exist"));
        assert!(
            !dir.join("profiles").join("deleted-profile").exists(),
            "stale default_profile must not be silently revived on disk",
        );
    }

    #[test]
    fn validate_instance_id_allowlists_one_path_component() {
        for (id, ok) in [
            ("a3f7c2d1e4b89012", true),
            ("compact", true),
            ("nested_first", true),
            ("a-b-c", true),
            ("", false),
            ("..", false),
            (".", false),
            ("/etc", false),
            ("foo/bar", false),
            ("foo\\bar", false),
            ("foo\0bar", false),
            ("foo bar", false),
            ("x".repeat(65).as_str(), false),
        ] {
            assert_eq!(validate_instance_id(id).is_ok(), ok, "{id:?}");
        }

        // Errors must not echo input bytes (log injection).
        const SENTINEL: &str = "ZZ_unique_sentinel_aabbcc";
        for (bad, reason) in [
            (format!("{SENTINEL}/x"), "disallowed"),
            (format!("{SENTINEL}{}", "x".repeat(70)), "too long"),
        ] {
            let e = validate_instance_id(&bad).unwrap_err().to_string();
            assert!(e.contains(reason) && !e.contains(SENTINEL), "{e}");
        }
    }
}
