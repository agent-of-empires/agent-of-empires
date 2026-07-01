//! Session management module

pub mod artifacts;
pub mod builder;
pub(crate) mod capture;
pub mod civilizations;
pub mod config;
pub(crate) mod container_config;
pub mod deletion;
pub(crate) mod environment;
mod groups;
pub mod idle_reap;
mod instance;
pub mod mcp_model;
pub mod mcp_overrides;
pub mod mcp_state;
pub mod poller;
pub mod profile_config;
pub mod project_mcp;
pub mod projects;
pub(crate) mod recovery;
pub mod repo_config;
pub mod restart;
pub mod scratch;
pub(crate) mod serde_helpers;
pub mod settings_schema;
pub mod smart_rename;
pub mod stop;
mod storage;
pub(crate) mod sync;
pub mod trash;
pub mod worktree_edit;

pub use crate::sound::SoundConfig;
pub use crate::status_hooks::StatusHookConfig;
pub(crate) use capture::is_valid_session_id;
pub use config::{
    get_telemetry_settings, get_update_settings, load_config, save_config,
    validate_snooze_duration, CapabilityGrant, ClickAction, Config, ContainerRuntimeName,
    DefaultTerminalMode, GroupByMode, NewSessionAttachMode, PluginConfig, RowTagMode,
    SandboxConfig, SessionConfig, TelemetryConfig, ThemeConfig, TmuxClipboardMode, TmuxMouseMode,
    TmuxStatusBarMode, UpdatesConfig, VolumeIgnoresStrategy, WorktreeConfig,
};
pub(crate) use environment::user_shell;
pub use environment::{validate_env_entries, validate_env_entry};
pub use groups::{
    append_archived_section, append_archived_section_by_project, append_trash_section,
    archived_project_sub_path, flatten_sessions_by_attention, flatten_tree,
    flatten_tree_all_profiles, is_archived_section_path, is_trash_section_path,
    is_within_archived_section, is_within_trash_section, Group, GroupTree, Item,
    ARCHIVED_SECTION_NAME, ARCHIVED_SECTION_PATH, TRASH_SECTION_NAME, TRASH_SECTION_PATH,
};
pub(crate) use instance::{persist_session_to_storage, ResumeIntent, SidWrite};
pub use instance::{
    EnsureReadyError, EnsureReadyOutcome, Instance, LaunchSidOutcome, SandboxInfo, SessionBucket,
    StartOutcome, Status, TerminalInfo, View, WorkspaceInfo, WorkspaceRepo, WorktreeInfo,
    TMUX_SESSION_GONE_ERROR,
};

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

pub use profile_config::{
    load_profile_config, merge_configs, resolve_config, resolve_config_or_warn,
    save_profile_config, validate_check_interval, validate_env_format, validate_memory_limit,
    validate_port_mapping_format, validate_volume_format, ProfileConfig,
};
pub use projects::{Project, ProjectScope};
pub use recovery::HookTimeoutScope;
pub use repo_config::{
    check_repo_trust, execute_hooks, execute_hooks_in_container, load_repo_config,
    merge_repo_config, profile_to_repo_config, repo_config_to_profile, resolve_config_with_repo,
    resolve_config_with_repo_or_warn, save_repo_config, trust_repo, HookTimeout, HooksConfig,
    RepoConfig, RepoTrust, TrustSurface,
};
pub(crate) use storage::{atomic_write, atomic_write_following_symlinks, resolve_symlink_chain};
pub use storage::{
    load_recent_projects, load_workspace_ordering, recent_project_entry_for, record_recent_project,
    update_workspace_ordering, RecentProjectEntry, Storage, WorkspaceOrdering,
};

use anyhow::Result;
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
fn list_profile_names_in(profiles_dir: &std::path::Path) -> Result<Vec<String>> {
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

    fn make_temp_profiles_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aoe-profile-listing-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn list_profile_names_skips_symlinks_to_real_profiles() {
        let dir = make_temp_profiles_dir();
        fs::create_dir(dir.join("default")).unwrap();
        fs::create_dir(dir.join("personal")).unwrap();
        // The cs/cxa pattern: aliases are symlinks pointing at `default`.
        symlink("default", dir.join("forit-work")).unwrap();
        symlink("default", dir.join("wma-work")).unwrap();

        let names = list_profile_names_in(&dir).expect("list");
        assert_eq!(
            names,
            vec!["default".to_string(), "personal".to_string()],
            "symlinked aliases must be invisible to list_profiles; \
             otherwise each alias inflates the all-profiles session list \
             with duplicates of the linked profile's data (the original \
             three-of-every-folder bug)."
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_profile_names_includes_real_dirs_only() {
        let dir = make_temp_profiles_dir();
        fs::create_dir(dir.join("default")).unwrap();
        fs::create_dir(dir.join("work")).unwrap();
        // A regular file in profiles/ should also be ignored.
        fs::write(dir.join("README"), "ignore me").unwrap();

        let names = list_profile_names_in(&dir).expect("list");
        assert_eq!(names, vec!["default".to_string(), "work".to_string()]);

        let _ = fs::remove_dir_all(&dir);
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

pub fn create_profile(name: &str) -> Result<()> {
    validate_profile_name(name)?;

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

pub fn rename_profile(old_name: &str, new_name: &str) -> Result<()> {
    if new_name.is_empty() {
        anyhow::bail!("New profile name cannot be empty");
    }
    if new_name.contains('/') || new_name.contains('\\') {
        anyhow::bail!("Profile name cannot contain path separators");
    }

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
    let mut config = load_config()?.unwrap_or_default();
    config.default_profile = name.to_string();
    save_config(&config)?;
    Ok(())
}

/// Probe the global config and the active profile's config at startup so the
/// TUI can show a single user-visible warning when either fails to parse.
/// `tracing::warn!` calls inside the `_or_warn` helpers are silently dropped
/// in default TUI mode (no subscriber), so this gives users a chance to see
/// that their settings have been ignored without needing `AGENT_OF_EMPIRES_DEBUG=1`.
pub fn collect_startup_config_warnings(profile: &str) -> Option<String> {
    let mut messages: Vec<String> = Vec::new();

    let global_path_display = config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "config.toml".to_string());

    if let Err(e) = Config::load() {
        messages.push(format!(
            "Failed to load global config ({global_path_display}); using defaults.\n{e}"
        ));
    }

    let effective = if profile.is_empty() {
        config::resolve_default_profile()
    } else {
        profile.to_string()
    };

    let profile_path_display = profile_config::get_profile_config_path(&effective)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| format!("profiles/{effective}/config.toml"));

    if let Err(e) = profile_config::load_profile_config(&effective) {
        messages.push(format!(
            "Failed to load profile config '{effective}' ({profile_path_display}); using defaults.\n{e}"
        ));
    }

    if messages.is_empty() {
        None
    } else {
        Some(messages.join("\n\n"))
    }
}

// ── TUI presence ────────────────────────────────────────────────────────────
//
// Each running TUI process drops a `<pid>` file under `tui-presence/` and
// refreshes its mtime on the heartbeat tick. This lets us (a) tell the push
// consumer whether *any* TUI is watching, and (b) count how many TUIs are
// alive so the footer can surface "another instance is watching" when two
// `aoe` TUIs run at once (the launcher TUI isn't tmux-backed, so there's no
// tmux client list to read). A presence file is considered live while its
// mtime is fresh; stale ones (crash without cleanup) are swept on read.

const TUI_PRESENCE_DIR: &str = "tui-presence";

fn presence_dir() -> Option<std::path::PathBuf> {
    get_app_dir().ok().map(|d| d.join(TUI_PRESENCE_DIR))
}

fn own_presence_file() -> Option<std::path::PathBuf> {
    presence_dir().map(|d| d.join(std::process::id().to_string()))
}

/// Write (or touch) this process's presence file so the push consumer knows
/// a TUI is running and other TUIs can count us. Called periodically from the
/// TUI event loop.
pub fn write_tui_heartbeat() {
    if let Some(dir) = presence_dir() {
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join(std::process::id().to_string()), b"");
    }
}

/// Remove this process's presence file on TUI exit.
pub fn clear_tui_heartbeat() {
    if let Some(file) = own_presence_file() {
        let _ = fs::remove_file(file);
    }
}

/// Count TUI presence files whose mtime is fresh within `threshold`, sweeping
/// any stale entries left behind by crashed processes. Returns the number of
/// live TUIs (including this process, if its file is fresh).
pub fn count_active_tuis(threshold: Duration) -> usize {
    let dir = match presence_dir() {
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

/// Returns true if any TUI presence file was modified within `threshold`.
/// Used by the push consumer to suppress notifications when the user is
/// actively watching a TUI.
pub fn is_tui_active(threshold: Duration) -> bool {
    count_active_tuis(threshold) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolate_app_dir() -> tempfile::TempDir {
        let temp_home = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));
        temp_home
    }

    fn app_dir(temp_home: &tempfile::TempDir) -> PathBuf {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let dir = temp_home.path().join(".config").join(APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let dir = temp_home.path().join(APP_DIR_NAME_OTHER);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[serial_test::serial]
    fn test_xdg_config_base_prefers_absolute_xdg_config_home() {
        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        let custom = temp.path().join("custom-xdg");
        std::env::set_var("XDG_CONFIG_HOME", &custom);

        assert_eq!(xdg_config_base().unwrap(), custom);
        // The global config path is derived from that base on both Linux and
        // macOS, so a dotfile manager sees a single location. See issue #1948.
        assert_eq!(get_app_dir_path().unwrap(), custom.join(APP_DIR_NAME_XDG));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[serial_test::serial]
    fn test_xdg_config_base_falls_back_to_home_dot_config() {
        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        // A relative (non-absolute) value is ignored per the XDG spec.
        std::env::set_var("XDG_CONFIG_HOME", "relative/path");

        assert_eq!(xdg_config_base().unwrap(), temp.path().join(".config"));

        std::env::remove_var("XDG_CONFIG_HOME");
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
        assert!(is_tui_active(Duration::from_secs(30)));

        // A second instance's presence file bumps the count to two.
        fs::write(pdir.join("999999"), b"").unwrap();
        assert_eq!(count_active_tuis(Duration::from_secs(30)), 2);

        // A zero threshold makes every file stale; they're swept and the
        // directory is left empty.
        assert_eq!(count_active_tuis(Duration::ZERO), 0);
        assert!(!is_tui_active(Duration::from_secs(30)));
        assert_eq!(fs::read_dir(&pdir).unwrap().count(), 0);

        // Exit cleanup removes only our own file.
        write_tui_heartbeat();
        fs::write(pdir.join("999999"), b"").unwrap();
        clear_tui_heartbeat();
        assert_eq!(count_active_tuis(Duration::from_secs(30)), 1);
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_startup_config_warnings_clean() {
        let _temp = isolate_app_dir();
        // No config files written = defaults everywhere = no warning.
        assert!(collect_startup_config_warnings("").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_startup_config_warnings_bad_global() {
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::write(
            dir.join("config.toml"),
            "[sandbox]\nenabled_by_default = \"not-a-bool\"\n",
        )
        .unwrap();

        let warning = collect_startup_config_warnings("").expect("expected a warning");
        assert!(warning.contains("Failed to load global config"));
        assert!(warning.contains("config.toml"));
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

    fn release_dir_in(temp: &tempfile::TempDir) -> PathBuf {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let d = temp.path().join(".config").join("agent-of-empires");
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let d = temp.path().join(".agent-of-empires");
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
    fn test_format_warning_mentions_both_paths_and_migration_command() {
        let release = PathBuf::from("/home/u/.config/agent-of-empires");
        let dev = PathBuf::from("/home/u/.config/agent-of-empires-dev");
        let msg = format_debug_namespace_warning(&release, &dev);
        assert!(msg.contains("/home/u/.config/agent-of-empires"));
        assert!(msg.contains("/home/u/.config/agent-of-empires-dev"));
        assert!(msg.contains("cp -r"));
        assert!(msg.contains("not repeat"));
    }

    #[test]
    #[serial_test::serial]
    fn test_fresh_install_bootstraps_main_profile() {
        // Genuine first run: no profiles/ entries. Resolution must create a
        // single profile named "main", never the magic "default".
        let _temp = isolate_app_dir();
        assert!(list_profiles().unwrap().is_empty());

        let resolved = config::resolve_default_profile();
        assert_eq!(resolved, "main");
        assert_eq!(list_profiles().unwrap(), vec!["main".to_string()]);
    }

    #[test]
    #[serial_test::serial]
    fn test_existing_default_profile_is_untouched_and_usable() {
        // An install that already has profiles/default/ keeps it; "default"
        // is now an ordinary profile, resolved like any other first entry.
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("default")).unwrap();

        let resolved = config::resolve_default_profile();
        assert_eq!(resolved, "default");
        assert!(dir.join("profiles").join("default").exists());

        let storage = Storage::new_unwatched("default").unwrap();
        assert_eq!(storage.profile(), "default");
    }

    #[test]
    #[serial_test::serial]
    fn test_get_profile_dir_empty_resolves_without_default_literal() {
        // An empty profile argument resolves through resolve_default_profile,
        // landing on the first existing profile rather than a "default" name.
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("alpha")).unwrap();
        fs::create_dir_all(dir.join("profiles").join("beta")).unwrap();

        let resolved = get_profile_dir("").unwrap();
        assert_eq!(resolved, dir.join("profiles").join("alpha"));
    }

    #[test]
    #[serial_test::serial]
    fn test_delete_profile_refuses_last_remaining() {
        // The invariant is a count, not a name: deleting the only profile is
        // refused so AoE always has somewhere to file sessions.
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("solo")).unwrap();

        let err = delete_profile("solo").expect_err("deleting the last profile must fail");
        assert!(err.to_string().contains("at least one profile must exist"));
        assert!(dir.join("profiles").join("solo").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_delete_profile_named_default_allowed_when_others_exist() {
        // A profile literally named "default" carries no protection once
        // other profiles exist; only the count invariant applies.
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("default")).unwrap();
        fs::create_dir_all(dir.join("profiles").join("work")).unwrap();

        delete_profile("default").expect("a non-last profile named default is deletable");
        assert!(!dir.join("profiles").join("default").exists());
        assert!(dir.join("profiles").join("work").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_delete_profile_rejects_path_traversal() {
        // Without name validation, delete_profile("../foo") would resolve
        // to <app_dir>/profiles/../foo and remove an arbitrary sibling
        // directory. The validator must catch this before any FS work.
        let temp = isolate_app_dir();
        let dir = app_dir(&temp);
        fs::create_dir_all(dir.join("profiles").join("real")).unwrap();
        // A directory that must NOT be touched by the call below.
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
        assert!(dir.join("profiles").join("real").exists());
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

        let cfg = crate::session::profile_config::load_profile_config("does-not-exist")
            .expect("loading config for an unknown profile must succeed with defaults");
        assert!(
            !crate::session::profile_config::profile_has_overrides(&cfg),
            "unknown profile must load to defaults",
        );
        assert!(
            !unknown_dir.exists(),
            "load_profile_config must not create profiles/<unknown>/ as a side effect",
        );
    }

    #[test]
    fn validate_instance_id_rejects_unsafe() {
        assert!(validate_instance_id("").is_err(), "empty");
        assert!(validate_instance_id("..").is_err(), "parent ref");
        assert!(validate_instance_id(".").is_err(), "current ref");
        assert!(validate_instance_id("/etc").is_err(), "absolute path");
        assert!(validate_instance_id("foo/bar").is_err(), "subdir traversal");
        assert!(validate_instance_id("foo\\bar").is_err(), "backslash");
        assert!(validate_instance_id("foo\0bar").is_err(), "NUL byte");
        assert!(validate_instance_id("foo bar").is_err(), "whitespace");
        assert!(
            validate_instance_id(&"x".repeat(65)).is_err(),
            "over length cap"
        );
    }

    #[test]
    fn validate_instance_id_accepts_production_and_test() {
        assert!(
            validate_instance_id("a3f7c2d1e4b89012").is_ok(),
            "production hex"
        );
        assert!(
            validate_instance_id("0123456789abcdef").is_ok(),
            "production hex lower"
        );
        assert!(validate_instance_id("compact").is_ok(), "test label");
        assert!(validate_instance_id("nested_first").is_ok(), "underscore");
        assert!(validate_instance_id("a-b-c").is_ok(), "hyphen");
    }

    #[test]
    fn validate_instance_id_error_messages_do_not_echo_input() {
        const SENTINEL: &str = "ZZ_unique_sentinel_aabbcc";

        let bad = format!("{SENTINEL}/x");
        let e = validate_instance_id(&bad).unwrap_err().to_string();
        assert!(e.contains("disallowed"));
        assert!(
            !e.contains(SENTINEL),
            "must not echo input bytes; risk of log injection"
        );

        let bad = format!("{SENTINEL}{}", "x".repeat(70));
        let e = validate_instance_id(&bad).unwrap_err().to_string();
        assert!(e.contains("too long"));
        assert!(
            !e.contains(SENTINEL),
            "must not echo input bytes from oversize branch"
        );
    }
}
