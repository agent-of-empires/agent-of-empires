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
// Depends on `crate::acp` (Event / event store) and is only driven from the
// serve daemon. See #2808.
pub mod conversation_summary;
pub mod deletion;
pub(crate) mod environment;
pub mod fork;
mod groups;
pub mod idle_reap;
mod instance;
pub mod mcp;
mod move_journal;
pub(crate) mod path_identity;
pub mod poller;
pub mod projects;
pub(crate) mod purge_owners;
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
    duplicate_session_error, is_duplicate_session, PassiveStatusPatch, ResumeIntent, SidWrite,
    ToolLaunchUnavailable, NEWER_GENERATION_BUSY_REASON,
};
pub(crate) use instance::{
    generic_host_config_path_for, resolved_agent_for, sidecar_host_config_path_for,
    ConversationState, LaunchReservation, ResumeAttemptPolicy, ResumeLaunchOptions,
    TerminalContextResume,
};
pub use instance::{
    is_valid_session_color, AuxiliaryObservation, AuxiliaryTarget, ConversationBinding,
    ConversationProvenance, DetectionState, EnsureReadyError, EnsureReadyOutcome, ExecutionBinding,
    ExecutionLocation, Instance, LaunchSidOutcome, LifecycleOperation, LifecycleReservation,
    LifecycleReservationError, PaneObservation, PanePresence, PendingInitialTurn,
    PluginCreateIdempotency, PollerStart, SandboxInfo, SessionBucket, StartOutcome, Status,
    TerminalInfo, View, WorkspaceInfo, WorkspaceRepo, WorktreeInfo, SESSION_COLORS,
    TMUX_SERVER_UNREACHABLE_ERROR, TMUX_SESSION_GONE_ERROR,
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
/// The TUI refreshes it via [`set_unread_enabled`] on startup and whenever
/// config is re-applied, so a runtime settings change takes effect without a
/// restart. Defaults to `true` so the feature is on out of the box before the
/// first config apply. Read on the hot Attention-sort path, hence a plain
/// atomic load rather than threading the flag through every sort helper.
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
/// Refreshed alongside [`set_unread_enabled`] whenever config is applied.
/// Read on every sort pass, so it is an atomic load rather than a parameter
/// threaded through the sort helpers.
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

pub(crate) use anchored_fs::ResolvedDataFile;
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
pub(crate) use storage::{
    acquire_open_storage_flock, acquire_session_title_lock, acquire_storage_flock,
    acquire_storage_shared_flock, atomic_write, read_file_no_follow, replace_file_no_follow,
    resolve_symlink_chain, try_acquire_storage_flock, CaptureStorage, GroupMovePlan, LaunchConfig,
    NativeStoreUnavailable, ProfileMoveRejected, SessionMutation, SessionStore, StorageFlock,
    StorageTransition, STORAGE_LOCK_FILENAME,
};
pub use storage::{
    load_recent_projects, load_workspace_ordering, recent_project_entry_for, record_recent_project,
    update_workspace_ordering, RecentProjectEntry, Storage, WorkspaceOrdering,
};

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// App dir name under the XDG config base (`$XDG_CONFIG_HOME`, default
/// `~/.config`). Always used on Linux; used on macOS when the user opts into
/// the XDG layout (see `get_app_dir_path` and issue #1948). Debug builds use
/// a `-dev` suffix so a `cargo run` instance shares no state with an installed
/// release binary.
pub const APP_DIR_NAME_XDG: &str = if cfg!(debug_assertions) {
    "agent-of-empires-dev"
} else {
    "agent-of-empires"
};

/// Home-dotfile app dir name (under `$HOME`). The default on macOS and the
/// only location on Windows. Debug builds use a `-dev` suffix; see
/// `APP_DIR_NAME_XDG`.
pub const APP_DIR_NAME_OTHER: &str = if cfg!(debug_assertions) {
    ".agent-of-empires-dev"
} else {
    ".agent-of-empires"
};

/// Resolve the XDG-style config base directory: `$XDG_CONFIG_HOME` when set to
/// an absolute path, otherwise `~/.config`.
///
/// On Linux this matches `dirs::config_dir()`. macOS uses it for the XDG layout
/// (rather than `dirs::config_dir()`, which there resolves to `~/Library/
/// Application Support`) so a dotfile manager like chezmoi can share one global
/// config path with Linux. See issue #1948.
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

/// Whether `$XDG_CONFIG_HOME` is set to an absolute path, i.e. the user has
/// meaningfully opted into the XDG layout. A relative or empty value is ignored
/// per the XDG spec (and by [`xdg_config_base`]), so it does not count.
#[cfg(target_os = "macos")]
fn xdg_config_home_set() -> bool {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(|v| std::path::Path::new(&v).is_absolute())
        .unwrap_or(false)
}

/// macOS app-dir resolution: prefer the XDG location, fall back to the
/// home-dotfile location, without ever moving data. See issue #1948.
///
/// Precedence (the first matching rule wins):
/// 1. the XDG dir already exists -> use it (picks up a `~/.config` tree synced
///    from Linux even when `$XDG_CONFIG_HOME` is unset);
/// 2. the legacy dir already exists -> use it (an existing install keeps its
///    data in place even after the user later sets `$XDG_CONFIG_HOME`);
/// 3. `$XDG_CONFIG_HOME` is set -> use the XDG dir (a fresh XDG opt-in);
/// 4. otherwise -> the home-dotfile dir (the historical macOS default).
///
/// `xdg_name` / `legacy_name` are passed in (rather than read from the
/// constants) so the dev/release namespace warning can resolve the release
/// pair from a debug build.
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

/// Whether the app data dir already exists, **without** creating it (unlike
/// [`get_app_dir`], which auto-creates). Lets side-effect-sensitive callers
/// probe install state cheaply: the per-command telemetry recorder uses it to
/// stay a true no-op for app-data-free commands (`aoe completion`, `aoe init`,
/// ...) on an install that is not opted in, so those commands keep working in
/// read-only / sandboxed (e.g. Nix) environments without materializing the dir.
pub fn app_dir_exists() -> bool {
    get_app_dir_path().map(|p| p.exists()).unwrap_or(false)
}

fn get_app_dir_path() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    let dir = xdg_config_base()?.join(APP_DIR_NAME_XDG);

    #[cfg(target_os = "macos")]
    let dir = macos_app_dir(APP_DIR_NAME_XDG, APP_DIR_NAME_OTHER)
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(APP_DIR_NAME_OTHER);

    Ok(dir)
}

/// Detect the first-launch case where a debug build is being run on a
/// machine that has populated release-build state in `~/.agent-of-empires`
/// but no dev-build state yet. Returns the (release_dir, dev_dir) pair so
/// callers can surface the paths in a one-time warning.
///
/// Self-extinguishing by design: once `get_app_dir` creates the dev dir on
/// any subsequent call, this returns `None` and the warning stops. No flag
/// file, no config state, no dismissal logic — the directory topology IS
/// the state.
///
/// Returns `None` on release builds (the dev/release split doesn't apply),
/// when the release dir is absent or empty (user has no prior state to
/// "lose visibility of"), or when the dev dir already exists.
pub fn debug_namespace_drift() -> Option<(PathBuf, PathBuf)> {
    if !cfg!(debug_assertions) {
        return None;
    }

    #[cfg(target_os = "linux")]
    let release_dir = xdg_config_base().ok()?.join("agent-of-empires");
    #[cfg(target_os = "macos")]
    let release_dir = macos_app_dir("agent-of-empires", ".agent-of-empires")?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let release_dir = dirs::home_dir()?.join(".agent-of-empires");

    #[cfg(target_os = "linux")]
    let dev_dir = xdg_config_base().ok()?.join(APP_DIR_NAME_XDG);
    #[cfg(target_os = "macos")]
    let dev_dir = macos_app_dir(APP_DIR_NAME_XDG, APP_DIR_NAME_OTHER)?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let dev_dir = dirs::home_dir()?.join(APP_DIR_NAME_OTHER);

    let release_populated = fs::read_dir(&release_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);

    if release_populated && !dev_dir.exists() {
        Some((release_dir, dev_dir))
    } else {
        None
    }
}

/// The app dir of the *other* build namespace: the release dir from a debug
/// build, the dev dir from a release build.
///
/// Debug and release builds keep separate app dirs but share `$HOME`, and so
/// share the agent store roots under it. Anything that decides whether a store
/// is owned has to read both registries or it will call the other build's
/// sessions orphans. `None` when the paths cannot be resolved.
pub(crate) fn sibling_namespace_app_dir() -> Option<PathBuf> {
    let (xdg, other) = if cfg!(debug_assertions) {
        ("agent-of-empires", ".agent-of-empires")
    } else {
        ("agent-of-empires-dev", ".agent-of-empires-dev")
    };
    #[cfg(target_os = "linux")]
    {
        let _ = other;
        xdg_config_base().ok().map(|base| base.join(xdg))
    }
    #[cfg(target_os = "macos")]
    {
        macos_app_dir(xdg, other)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = xdg;
        dirs::home_dir().map(|home| home.join(other))
    }
}

/// Format the user-facing warning shown when `debug_namespace_drift()`
/// fires. Shared between the CLI stderr print and the TUI startup popup so
/// both surfaces say exactly the same thing.
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
        // Only a name about to be created runs the strict grammar; an
        // existing directory still opens, so older malformed profiles stay
        // listable and deletable.
        validate_new_profile_name(profile_name)?;
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

/// Resolve the on-disk profile directory path WITHOUT creating it.
///
/// Use this for read-only operations (loading config, looking up paths)
/// where the directory-creation side effect of [`get_profile_dir`] would
/// pollute `profiles/` with empty stub directories. Notably, GET
/// `/api/settings?profile=<name>` for an unknown profile used to create
/// that profile's directory as a side effect of the read, which then
/// made the unknown profile appear in subsequent GET /api/profiles
/// responses. Routing those reads through this helper keeps the lookup
/// pure.
///
/// Empty `profile` resolves through [`config::resolve_default_profile`]
/// just like [`get_profile_dir`] does, including its bootstrap side
/// effect on a genuine first run; callers that want to avoid that should
/// pass an explicit non-empty name.
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
///
/// Never creates a profile directory. An empty `profile` resolves through
/// [`config::resolve_default_profile`], including its bootstrap side effect
/// on a genuine first run with zero profiles (that's intentional: AoE always
/// needs somewhere to file sessions). An explicitly named profile, or a
/// configured default whose directory has since been deleted, is an error
/// rather than being silently revived on disk; only [`create_profile`] and
/// the CLI's session-creation path are allowed to birth a profile directory.
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
    // Test-only failure injection: when set, the next call returns
    // Err and the flag clears. Used by the file-watch regression test
    // that locks the rewire-after-mutation error-handling path
    // without requiring a platform-fragile permission denial.
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
///
/// Presentation only. [`list_profiles`] stays plainly sorted because
/// [`config::resolve_default_profile`] takes its first entry when
/// `config.default_profile` is unset.
pub fn sort_profiles_for_display<T: AsRef<str>>(profiles: &mut [T]) {
    profiles.sort_by(|a, b| {
        let (a, b) = (a.as_ref(), b.as_ref());
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

/// Refuse an explicit `-p`/`--profile` naming a profile that does not exist
/// (#148), so a typo never reaches [`get_profile_dir`] and mints a stray
/// directory. The daemon create-session endpoint enforces the same check.
///
/// Passes without a directory check: an empty `profile` (default resolution
/// and bootstrap create downstream) and a first run with no profiles yet.
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

/// RAII guard for the `FAIL_NEXT_LIST_PROFILES` test seam. `new` sets
/// the flag; `drop` clears it unconditionally so a panic between set
/// and the next `list_profiles` call does not leak the seam into a
/// subsequent test that picks up the stale `true` value.
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
/// Symlinks are aliases used by the `cs`/`cxa` account-switcher (e.g.
/// `forit-work -> default`) so multiple Claude account names share a single
/// profile directory; without the skip, every alias renders as a duplicate
/// profile and the session list multiplies (the original "three of every
/// folder" symptom). Extracted from `list_profiles` so tests can drive it
/// against a tempdir.
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
    //!
    //! The `cs`/`cxa` account-switcher creates `~/.agent-of-empires/profiles/<name>`
    //! as a symlink to `default` so multiple Claude account names share a
    //! single AOE profile directory. Before the fix, `list_profiles()` used
    //! `entry.path().is_dir()` which follows symlinks, so each alias was
    //! enumerated as a separate profile and the all-profiles session list
    //! rendered the same data N times.
    //!
    //! These tests pin the skip-symlink behavior so a future refactor that
    //! "simplifies" the file-type check fails CI instead of silently
    //! re-introducing the duplication.
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
///
/// Defense in depth: `get_profile_dir` and `delete_profile` ultimately
/// `join` `name` onto `<app_dir>/profiles/`, so a name like `..`, `/etc`,
/// or `a/b` would resolve outside the profiles directory. We require
/// exactly one path component, and that component must be `Normal`. Also
/// rejects empty strings and the reserved `all` (which the TUI uses as a
/// sentinel for "all-profiles" mode in profile pickers).
fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("Profile name cannot be empty");
    }
    if name.eq_ignore_ascii_case("all") {
        anyhow::bail!("Profile name 'all' is reserved");
    }
    // Unix Path treats `\` as a regular byte, so backslashes pass the
    // components check below. Reject them explicitly so the validator
    // behaves the same on every host the binary might land on.
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

/// Grammar for a profile about to be created: `[A-Za-z0-9_-]`, at most 64
/// characters, on top of the traversal guard in `validate_profile_name`.
///
/// It matches the daemon API's `validate_profile_name`, so a profile the CLI
/// creates is one the web UI can delete, rename or configure. Deletion keeps
/// the permissive guard so strays minted by older binaries stay removable.
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

pub(crate) struct ProfileCatalogueTransaction {
    root: PathBuf,
    _storage: storage::StorageTransition,
    _identity: storage::StorageFlock,
    lifecycle: crate::daemon::lifecycle::Transaction,
}

impl ProfileCatalogueTransaction {
    fn acquire() -> Result<Self> {
        Self::with_transaction(crate::daemon::lifecycle::Transaction::acquire_blocking()?)
    }

    // Native callers acquire namespace exclusion first.
    pub(crate) fn with_transaction(
        transaction: crate::daemon::lifecycle::Transaction,
    ) -> Result<Self> {
        let root = get_app_dir()?;
        let identity = storage::acquire_session_identity_lock()?;
        let storage = storage::StorageTransition::acquire_exclusive(&root)?;
        Ok(Self {
            root,
            _storage: storage,
            _identity: identity,
            lifecycle: transaction,
        })
    }

    pub(crate) fn create(&self, name: &str) -> Result<()> {
        validate_new_profile_name(name)?;
        let profiles = self.root.join("profiles");
        fs::create_dir_all(&profiles)?;
        fs::create_dir(profiles.join(name))
            .with_context(|| format!("Cannot create profile '{name}'"))
    }

    pub(crate) fn delete(&self, name: &str, replacement_default: Option<&str>) -> Result<String> {
        validate_profile_name(name)?;
        let profile_dir = self.root.join("profiles").join(name);
        anyhow::ensure!(profile_dir.exists(), "Profile '{}' does not exist", name);
        let remaining: Vec<_> = list_profiles()?
            .into_iter()
            .filter(|profile| profile != name)
            .collect();
        anyhow::ensure!(
            !remaining.is_empty(),
            "Cannot delete '{}': at least one profile must exist",
            name
        );
        let configured = Config::load()?.default_profile;
        let replacement = if let Some(requested) = replacement_default {
            anyhow::ensure!(
                remaining.iter().any(|profile| profile == requested),
                "Replacement default must be a remaining profile"
            );
            requested.to_owned()
        } else if remaining.contains(&configured) {
            configured
        } else {
            remaining[0].clone()
        };
        let launches =
            crate::cli::serve::LaunchProfileUpdates::prepare(&self.lifecycle, name, &replacement)?;
        fs::remove_dir_all(&profile_dir)?;
        update_config(|config| replacement.clone_into(&mut config.default_profile))?;
        launches.commit()?;
        Ok(replacement)
    }

    pub(crate) fn rename(&self, old_name: &str, new_name: &str) -> Result<()> {
        validate_profile_name(old_name)?;
        validate_new_profile_name(new_name)?;
        let old_dir = self.root.join("profiles").join(old_name);
        let new_dir = self.root.join("profiles").join(new_name);
        anyhow::ensure!(old_dir.exists(), "Profile '{}' does not exist", old_name);
        anyhow::ensure!(
            !new_dir.try_exists()?,
            "Profile '{}' already exists",
            new_name
        );
        Config::load()?;
        let launches =
            crate::cli::serve::LaunchProfileUpdates::prepare(&self.lifecycle, old_name, new_name)?;
        fs::rename(&old_dir, &new_dir)?;
        update_config(|config| {
            if config.default_profile == old_name {
                config.default_profile = new_name.to_owned();
            }
        })?;
        launches.commit()?;
        Ok(())
    }

    pub(crate) fn set_default(&self, name: &str) -> Result<()> {
        validate_profile_name(name)?;
        anyhow::ensure!(
            list_profiles()?.iter().any(|profile| profile == name),
            "Profile '{}' does not exist",
            name
        );
        update_config(|config| config.default_profile = name.to_owned())
    }
}

pub fn create_profile(name: &str) -> Result<()> {
    ProfileCatalogueTransaction::acquire()?.create(name)
}

pub fn delete_profile(name: &str) -> Result<()> {
    ProfileCatalogueTransaction::acquire()?
        .delete(name, None)
        .map(|_| ())
}

/// Renaming can repair an old source name; the destination uses the create grammar.
pub fn rename_profile(old_name: &str, new_name: &str) -> Result<()> {
    ProfileCatalogueTransaction::acquire()?.rename(old_name, new_name)
}

pub fn set_default_profile(name: &str) -> Result<()> {
    ProfileCatalogueTransaction::acquire()?.set_default(name)
}

/// One file's probe result: either the parse errored out (per-key values fall
/// back to defaults), or it loaded but some keys were unrecognized and
/// silently dropped. The two are mutually exclusive per file: without
/// `deny_unknown_fields`, an unknown key never fails the load.
pub struct ConfigProbe {
    pub load_err: Option<String>,
    pub ignored_keys: Vec<String>,
}

/// Try to load a config file, and on success enumerate its unrecognized keys.
/// The two arms of `ConfigProbe` are always populated the same way; this
/// helper keeps the global and profile probes from drifting apart.
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

/// Probe the global `config.toml`: run the real `Config::load` and, if it
/// succeeded, run `serde_ignored` to enumerate any unknown struct fields at
/// any depth.
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
    /// Unrecognized keys only. For paths where a tracing subscriber is already
    /// running: a parse failure reaches the operator through that sink, but
    /// ignored keys are collected nowhere else.
    IgnoredKeysOnly,
}

/// Format one probe result as a user-visible line, or `None` when the file is
/// clean (or failed to parse under [`WarningClass::IgnoredKeysOnly`], which
/// carries no ignored-key information anyway). `scope_label` is prefixed to
/// both messages (e.g. `"global config"`, `"profile config 'foo'"`).
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
    // Non-creating resolver: `get_profile_config_path` goes through the
    // creating `get_profile_dir`, so naming an unknown profile (`aoe list -p
    // ghost`) would birth `profiles/ghost/` here, before the command's own
    // `resolve_existing_profile` gets to reject it. See
    // `tests/e2e/profile_lazy_creation.rs`.
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

/// Probe the global config and the active profile's config at startup so the
/// TUI can show a single user-visible warning when either fails to parse OR
/// contains unrecognized keys. `tracing::warn!` calls inside the `_or_warn`
/// helpers are silently dropped in default TUI mode (no subscriber), so this
/// gives users a chance to see that their settings have been ignored without
/// needing `AGENT_OF_EMPIRES_DEBUG=1`.
pub fn collect_startup_config_warnings(profile: &str) -> Option<String> {
    collect_startup_warnings(profile, WarningClass::All)
}

/// Like [`collect_startup_config_warnings`], but only the unrecognized-keys
/// class. Used on paths where a tracing subscriber is already running
/// (`should_init` true, e.g. TUI startup or `serve --daemon-child`): parse
/// failures reach the operator through `tracing::warn!` from `_or_warn`
/// helpers, but ignored keys are collected only by this probe, so they must
/// still be surfaced on stderr even when tracing is up (#3228 CodeRabbit).
pub fn collect_startup_ignored_key_warnings(profile: &str) -> Option<String> {
    collect_startup_warnings(profile, WarningClass::IgnoredKeysOnly)
}

// ── TUI presence ────────────────────────────────────────────────────────────
//
// Each running TUI process drops a `<pid>` file under `tui-presence/` and
// refreshes its mtime on the heartbeat tick. This lets the footer surface
// "another instance is watching" when two `aoe` TUIs run at once (the launcher
// TUI isn't tmux-backed, so there's no tmux client list to read). A presence
// file is considered live while its mtime is fresh; stale ones (crash without
// cleanup) are swept on read.
//
// Push suppression deliberately uses a separate `tui-activity/` directory.
// A live TUI can sit unattended while an agent needs attention, so process
// liveness is not evidence that the user is looking at it. Activity files are
// touched only for real keyboard, paste, and mouse input.

const TUI_PRESENCE_DIR: &str = "tui-presence";
const TUI_ACTIVITY_DIR: &str = "tui-activity";

fn tui_dir(name: &str) -> Option<std::path::PathBuf> {
    get_app_dir().ok().map(|d| d.join(name))
}

fn own_tui_file(name: &str) -> Option<std::path::PathBuf> {
    tui_dir(name).map(|d| d.join(std::process::id().to_string()))
}

/// Write (or touch) this process's presence file so the push consumer knows
/// a TUI is running and other TUIs can count us. Called periodically from the
/// TUI event loop.
pub fn write_tui_heartbeat() {
    if let Some(dir) = tui_dir(TUI_PRESENCE_DIR) {
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join(std::process::id().to_string()), b"");
    }
}

/// Record real user input in this TUI. Push notification suppression reads
/// this separately from the process-liveness heartbeat, so an unattended TUI
/// cannot permanently silence a phone's notifications.
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

/// Count TUI presence files whose mtime is fresh within `threshold`, sweeping
/// any stale entries left behind by crashed processes. Returns the number of
/// live TUIs (including this process, if its file is fresh).
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

/// Returns true if any TUI received real input within `threshold`. Used by the
/// push consumer to suppress notifications only while a user is interacting
/// with a TUI, rather than merely while a TUI process is alive.
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
        // The global config path is derived from that base on both Linux and
        // macOS, so a dotfile manager sees a single location. See issue #1948.
        assert_eq!(get_app_dir_path().unwrap(), custom.join(APP_DIR_NAME_XDG));

        let _relative = super::test_support::EnvGuard::set(&[("XDG_CONFIG_HOME", "relative/path")]);
        assert_eq!(xdg_config_base().unwrap(), temp.path().join(".config"));
        let _unset = super::test_support::EnvGuard::unset(&["XDG_CONFIG_HOME"]);
        assert_eq!(xdg_config_base().unwrap(), temp.path().join(".config"));
    }

    // Precedence behind the macOS read-fallback resolution (issue #1948). These
    // exercise the pure rule, so they run on every platform's CI, not just
    // macOS where `macos_app_dir` is compiled.
    #[test]
    fn test_fallback_prefers_existing_xdg_dir_even_over_legacy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let xdg = tmp.path().join(".config").join("agent-of-empires");
        let legacy = tmp.path().join(".agent-of-empires");
        fs::create_dir_all(&xdg).unwrap();
        fs::create_dir_all(&legacy).unwrap();

        // Both present: XDG wins, so there is never a split brain.
        assert_eq!(
            resolve_app_dir_with_fallback(xdg.clone(), legacy.clone(), true),
            xdg
        );
        assert_eq!(
            resolve_app_dir_with_fallback(xdg.clone(), legacy, false),
            xdg
        );
    }

    #[test]
    fn test_fallback_keeps_legacy_when_xdg_absent_even_if_env_set() {
        let tmp = tempfile::TempDir::new().unwrap();
        let xdg = tmp.path().join(".config").join("agent-of-empires");
        let legacy = tmp.path().join(".agent-of-empires");
        fs::create_dir_all(&legacy).unwrap();

        // An existing install that later sets XDG_CONFIG_HOME keeps reading its
        // data in place; nothing appears as a fresh install.
        assert_eq!(
            resolve_app_dir_with_fallback(xdg, legacy.clone(), true),
            legacy
        );
    }

    #[test]
    fn test_fallback_fresh_install_follows_env() {
        let tmp = tempfile::TempDir::new().unwrap();
        let xdg = tmp.path().join(".config").join("agent-of-empires");
        let legacy = tmp.path().join(".agent-of-empires");

        // Neither dir exists yet: XDG_CONFIG_HOME set -> XDG opt-in; unset ->
        // the historical macOS home-dotfile default.
        assert_eq!(
            resolve_app_dir_with_fallback(xdg.clone(), legacy.clone(), true),
            xdg
        );
        assert_eq!(
            resolve_app_dir_with_fallback(xdg, legacy.clone(), false),
            legacy
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_tui_presence_counts_and_sweeps() {
        let temp = isolate_app_dir();
        let pdir = app_dir(&temp).join(TUI_PRESENCE_DIR);

        // Our own heartbeat counts as one live TUI.
        write_tui_heartbeat();
        assert_eq!(count_active_tuis(Duration::from_secs(30)), 1);
        assert!(
            !is_tui_active(Duration::from_secs(30)),
            "a live but untouched TUI must not suppress phone notifications"
        );

        write_tui_activity();
        assert!(is_tui_active(Duration::from_secs(30)));

        // A second instance's presence file bumps the count to two.
        fs::write(pdir.join("999999"), b"").unwrap();
        assert_eq!(count_active_tuis(Duration::from_secs(30)), 2);

        // A zero threshold makes every file stale; they're swept and the
        // directory is left empty.
        assert_eq!(count_active_tuis(Duration::ZERO), 0);
        assert_eq!(count_fresh_tui_files(TUI_ACTIVITY_DIR, Duration::ZERO), 0);
        assert!(!is_tui_active(Duration::from_secs(30)));
        assert_eq!(fs::read_dir(&pdir).unwrap().count(), 0);

        // Exit cleanup removes only our own file.
        write_tui_heartbeat();
        fs::write(pdir.join("999999"), b"").unwrap();
        clear_tui_heartbeat();
        assert_eq!(count_active_tuis(Duration::from_secs(30)), 1);
    }

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
    fn test_collect_startup_config_warnings_bad_profile() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        let profile_dir = dir.join("profiles").join("default");
        fs::create_dir_all(&profile_dir).unwrap();
        fs::write(
            profile_dir.join("config.toml"),
            "[worktree]\nenabled = \"not-a-bool\"\n",
        )
        .unwrap();

        let warning = collect_startup_config_warnings("default").expect("expected a warning");
        assert!(warning.contains("Failed to load profile config 'default'"));
    }

    /// #3228 CodeRabbit follow-up: the ignored-keys-only variant is the
    /// path a subscribed run (TUI, foreground serve, or `AOE_LOG_LEVEL`
    /// set) uses so ignored keys are still surfaced without duplicating
    /// the parse-error line that `tracing::warn!` already reports. It
    /// must report unrecognized keys but NOT parse failures.
    #[test]
    #[serial_test::serial]
    fn test_collect_startup_ignored_key_warnings_reports_ignored_only() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::write(
            dir.join("config.toml"),
            "[sandbox]\nenabled_by_default = true\nprivildged = true\n",
        )
        .unwrap();

        let warning =
            collect_startup_ignored_key_warnings("").expect("ignored key must be reported");
        assert!(warning.contains("sandbox.privildged"));
        assert!(!warning.contains("Failed to load"));
    }

    /// #3228 CodeRabbit follow-up: a parse failure alone is *not* reported
    /// by the ignored-keys-only variant. `tracing::warn!` from
    /// `Config::load_or_warn` covers that class when a subscriber is up.
    #[test]
    #[serial_test::serial]
    fn test_collect_startup_ignored_key_warnings_silent_on_parse_failure() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::write(
            dir.join("config.toml"),
            "[sandbox]\nenabled_by_default = \"not-a-bool\"\n",
        )
        .unwrap();

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
    fn test_drift_none_when_no_release_dir() {
        let _temp = isolate_app_dir();
        // Neither dir exists → no drift to flag.
        assert!(debug_namespace_drift().is_none());
    }

    #[test]
    #[serial_test::serial]
    fn test_drift_none_when_release_empty() {
        let temp = isolate_app_dir();
        let release = release_dir_in(&temp);
        fs::create_dir_all(&release).unwrap();
        // Release dir exists but has no content — user has no prior state
        // to lose visibility of, so don't nag them.
        assert!(debug_namespace_drift().is_none());
    }

    #[test]
    #[serial_test::serial]
    fn test_drift_fires_when_release_populated_and_dev_absent() {
        let temp = isolate_app_dir();
        let release = release_dir_in(&temp);
        fs::create_dir_all(release.join("profiles")).unwrap();

        let drift = debug_namespace_drift();
        // Only assert presence on debug builds — release builds compile this
        // function to `None`.
        if cfg!(debug_assertions) {
            let (r, d) = drift.expect("expected drift on debug build");
            assert_eq!(r, release);
            assert!(d.to_string_lossy().contains("-dev"));
        } else {
            assert!(drift.is_none());
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_drift_silent_once_dev_dir_exists() {
        let temp = isolate_app_dir();
        let release = release_dir_in(&temp);
        fs::create_dir_all(release.join("profiles")).unwrap();
        // Simulate "user has already run aoe once after the namespace
        // change" by creating the dev dir.
        let _dev = app_dir(&temp);
        assert!(debug_namespace_drift().is_none());
    }

    #[test]
    #[serial_test::serial]
    fn test_implicit_resolution_ignores_picker_order_on_mixed_registry() {
        // Sinking "default" in the picker must not move the implicit profile:
        // with `default` + `work` and no configured default, implicit commands
        // land on `default`, the first entry in plain order.
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
    #[serial_test::serial]
    fn test_delete_profile_preserves_a_resolvable_default() {
        for (configured, replacement, expected) in [
            ("default", None, "alpha"),
            ("zeta", None, "zeta"),
            ("alpha", Some("zeta"), "zeta"),
        ] {
            let temp = isolate_app_dir();
            let dir = app_dir(&temp);
            for name in ["default", "alpha", "zeta"] {
                fs::create_dir_all(dir.join("profiles").join(name)).unwrap();
            }
            set_default_profile(configured).unwrap();
            if let Some(replacement) = replacement {
                let transaction = ProfileCatalogueTransaction::acquire().unwrap();
                for invalid in ["default", "missing"] {
                    assert!(transaction.delete("default", Some(invalid)).is_err());
                    assert!(dir.join("profiles/default").is_dir());
                    assert_eq!(load_config().unwrap().unwrap().default_profile, configured);
                }
                transaction.delete("default", Some(replacement)).unwrap();
            } else {
                delete_profile("default").unwrap();
            }
            assert!(!dir.join("profiles/default").exists());
            assert_eq!(resolve_existing_profile("").unwrap(), expected);
            assert_eq!(load_config().unwrap().unwrap().default_profile, expected);
        }
    }

    #[test]
    #[serial_test::serial]
    fn profile_catalogue_retains_owner_registries_during_resource_cleanup() {
        for rename in [true, false] {
            let temp = isolate_app_dir();
            let dir = app_dir(&temp);
            create_profile("source").unwrap();
            create_profile("keep").unwrap();
            let storage = Storage::new_unwatched("source").unwrap();
            storage
                .update(|rows, _| {
                    rows.push(Instance::new("owner", "/tmp/owned-resource"));
                    Ok(())
                })
                .unwrap();
            let identity = acquire_session_identity_lock().unwrap();
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                ready_tx.send(()).unwrap();
                let result = if rename {
                    rename_profile("source", "target")
                } else {
                    delete_profile("source")
                };
                done_tx.send(result).unwrap();
            });
            ready_rx.recv().unwrap();
            let premature = done_rx.recv_timeout(std::time::Duration::from_secs(2)).ok();
            let visible = fs::read(dir.join("profiles/source/sessions.json"));
            drop(identity);
            premature
                .unwrap_or_else(|| {
                    done_rx
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap()
                })
                .unwrap();
            worker.join().unwrap();
            let rows: Vec<Instance> = serde_json::from_slice(
                &visible.expect("catalogue mutation hid resource owners during cleanup"),
            )
            .unwrap();
            assert_eq!(rows[0].project_path, "/tmp/owned-resource");
            assert!(!dir.join("profiles/source").exists());
            assert_eq!(dir.join("profiles/target").exists(), rename);
        }
    }

    #[test]
    fn test_validate_profile_name_accepts_normal_names() {
        for name in ["work", "personal", "client-a", ".hidden", "1", "main"] {
            validate_profile_name(name)
                .unwrap_or_else(|e| panic!("expected {name:?} to validate: {e}"));
        }
    }

    #[test]
    fn test_validate_profile_name_rejects_traversal_and_separators() {
        for bad in ["", "..", ".", "/etc", "a/b", "a\\b", "all", "ALL"] {
            validate_profile_name(bad)
                .err()
                .unwrap_or_else(|| panic!("expected {bad:?} to be rejected"));
        }
    }

    #[test]
    fn test_validate_new_profile_name_accepts_typical_names() {
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
    }

    #[test]
    fn test_validate_new_profile_name_rejects_stray_shapes() {
        // The stray shape (`<profile> <16hex> <title>`, space-joined) plus
        // other junk must be rejected.
        for bad in [
            "work 0123456789abcdef Some Title",
            "ZZTEST spaced name",
            "has space",
            "tab\tname",
            "emoji\u{1f600}",
            "all",
            "..",
            "a/b",
            // Dots are outside the charset the daemon API enforces.
            ".hidden",
            "a.b",
        ] {
            validate_new_profile_name(bad)
                .err()
                .unwrap_or_else(|| panic!("expected create gate to reject {bad:?}"));
        }
        // 65 chars exceeds the length cap.
        let too_long = "a".repeat(65);
        validate_new_profile_name(&too_long).expect_err("65-char name must be rejected");
    }

    #[test]
    fn test_validate_new_profile_name_escapes_control_chars_in_error() {
        // A rejected name is echoed back escaped, never raw.
        let err = validate_new_profile_name("bad\u{1b}[31mname")
            .expect_err("control char must be rejected");
        let text = err.to_string();
        assert!(
            !text.contains('\u{1b}') && text.contains("\\u{1b}"),
            "expected only the escaped ESC in the error: {text:?}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[serial_test::serial]
    fn test_get_profile_dir_refuses_to_vivify_stray() {
        // A stray-shaped name must error and must not create a directory.
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
        // A valid name on the same path still vivifies normally.
        let good = get_profile_dir("personal").expect("valid name must create dir");
        assert!(good.exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_require_known_profile_rejects_unknown_when_registry_nonempty() {
        // An explicit -p naming an unknown profile is refused without
        // creating a directory (#148).
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

        // An existing profile and the empty (default) name both pass.
        require_known_profile("work").expect("existing profile must be allowed");
        require_known_profile("").expect("empty/default profile must be allowed");

        // The refusal echoes the name escaped, never raw.
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

        // Traversal on the source is still refused, and nothing is moved.
        fs::create_dir_all(dir.join("bystander")).unwrap();
        let err = rename_profile("../bystander", "escaped").expect_err("traversal source");
        assert!(err.to_string().contains("path separators"), "{err}");
        assert!(dir.join("bystander").exists());
        assert!(!dir.join("profiles").join("escaped").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_load_profile_config_does_not_create_dir_for_unknown_profile() {
        // Regression: previously `load_profile_config` flowed through
        // `get_profile_dir` which `create_dir_all`'d the profile dir as a
        // side effect of the read. That meant any GET against a profile
        // name that did not yet exist (the dashboard's mount-time settings
        // fetch fires before the profile list resolves) polluted
        // `profiles/` with a stub directory, and the stub then showed up
        // in subsequent GET /api/profiles responses. The read must stay
        // pure.
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
