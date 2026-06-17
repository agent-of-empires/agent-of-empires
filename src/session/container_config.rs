//! Container configuration building for sandboxed sessions.
//!
//! Standalone functions for computing Docker volume mounts and building
//! `ContainerConfig` structs. Includes sandbox directory sync, agent config
//! mounting, and credential extraction.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::containers::{ContainerConfig, EnvEntry, NamedVolumeMount, VolumeMount};
use crate::git::GitWorktree;
use crate::session::config::VolumeIgnoresStrategy;

use super::environment::collect_environment;
use super::instance::SandboxInfo;

/// Subdirectory name inside each agent's config dir for the shared sandbox config.
const SANDBOX_SUBDIR: &str = "sandbox";

/// Content seeded into the Claude sandbox `.sandbox-gitconfig`. Scoped to github.com;
/// the helper emits credentials only on `get` and only when GH_TOKEN is non-empty, so
/// other remotes and sessions without a forwarded token fall through to normal git
/// behavior.
const SANDBOX_GITCONFIG_SEED: &str = r#"[credential "https://github.com"]
	helper = "!f() { test \"$1\" = get || exit 0; test -n \"$GH_TOKEN\" || exit 0; echo username=x-access-token; echo \"password=$GH_TOKEN\"; }; f"
"#;

/// Declarative definition of an agent CLI's config directory for sandbox mounting.
struct AgentConfigMount {
    /// Canonical agent name from the agent registry (e.g. "claude", "opencode").
    /// Used to filter mounts so only the active tool's config is mounted.
    tool_name: &'static str,
    /// Path relative to home (e.g. ".claude").
    host_rel: &'static str,
    /// Path suffix relative to container home (e.g. ".claude").
    container_suffix: &'static str,
    /// Top-level entry names to skip when copying (large/recursive/unnecessary).
    skip_entries: &'static [&'static str],
    /// Files to seed into the sandbox dir with static content (write-once: only written
    /// if the file doesn't already exist, so container changes are preserved).
    seed_files: &'static [(&'static str, &'static str)],
    /// Directories to recursively copy into the sandbox dir (e.g. plugins, skills).
    copy_dirs: &'static [&'static str],
    /// macOS Keychain service name and target filename. If set, credentials are extracted
    /// from the Keychain and written to the sandbox dir as the specified file.
    keychain_credential: Option<(&'static str, &'static str)>,
    /// Files to seed at the container home directory level (outside the config dir).
    /// Each (filename, content) pair is written to the sandbox dir root and mounted as
    /// a separate file at CONTAINER_HOME/filename (write-once).
    home_seed_files: &'static [(&'static str, &'static str)],
    /// Files that should only be copied from the host if they don't already exist in the
    /// sandbox. Protects credentials placed by the v002 migration or by in-container
    /// authentication from being overwritten by stale host copies.
    preserve_files: &'static [&'static str],
    /// Files to delete from the sandbox dir before each launch. Prevents stale state
    /// (e.g. SQLite databases from a previous opencode version) from causing failures
    /// when the container image is updated.
    clean_files: &'static [&'static str],
}

/// Agent config definitions. Each entry describes one agent CLI's config directory.
/// To add a new agent, add an entry here -- no code changes needed.
const AGENT_CONFIG_MOUNTS: &[AgentConfigMount] = &[
    AgentConfigMount {
        tool_name: "claude",
        host_rel: ".claude",
        container_suffix: ".claude",
        skip_entries: &["sandbox", "projects"],
        seed_files: &[],
        copy_dirs: &["plugins", "skills"],
        // On macOS, OAuth tokens live in the Keychain. Extract and write as .credentials.json
        // so the container can authenticate without re-login.
        keychain_credential: Some(("Claude Code-credentials", ".credentials.json")),
        // Claude Code reads ~/.claude.json (home level, NOT inside ~/.claude/) for onboarding
        // state. Seeding hasCompletedOnboarding skips the first-run wizard.
        // Claude Code sets GIT_CONFIG_GLOBAL=/root/.sandbox-gitconfig when IS_SANDBOX=1;
        // the file must exist or all git commands fail. The seeded credential helper
        // lets `git push` to github.com authenticate automatically when GH_TOKEN is
        // forwarded via `sandbox.environment` (e.g. "GH_TOKEN=$GH_TOKEN"). Without a
        // helper, git ignores GH_TOKEN and prompts for a username; `gh auth setup-git`
        // can't fix it in-container because the gitconfig is a single-file bind mount
        // that can't be rewritten via atomic rename.
        home_seed_files: &[
            (".claude.json", r#"{"hasCompletedOnboarding":true}"#),
            (".sandbox-gitconfig", SANDBOX_GITCONFIG_SEED),
        ],
        preserve_files: &[".credentials.json", "history.jsonl"],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "opencode",
        host_rel: ".local/share/opencode",
        container_suffix: ".local/share/opencode",
        // Never copy or keep the SQLite database in the sandbox. Opencode must
        // create its own fresh database on each launch -- a stale db from a
        // previous opencode version (or copied from the host) causes drizzle
        // migration failures.
        skip_entries: &[
            "sandbox",
            "opencode.db",
            "opencode.db-wal",
            "opencode.db-shm",
        ],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &["opencode.db", "opencode.db-wal", "opencode.db-shm"],
    },
    AgentConfigMount {
        tool_name: "opencode",
        host_rel: ".config/opencode",
        container_suffix: ".config/opencode",
        skip_entries: &["sandbox"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "codex",
        host_rel: ".codex",
        container_suffix: ".codex",
        skip_entries: &["sandbox"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "gemini",
        host_rel: ".gemini",
        container_suffix: ".gemini",
        skip_entries: &["sandbox"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "vibe",
        host_rel: ".vibe",
        container_suffix: ".vibe",
        skip_entries: &["sandbox"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "cursor",
        host_rel: ".cursor",
        container_suffix: ".cursor",
        skip_entries: &["sandbox"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "copilot",
        host_rel: ".copilot",
        container_suffix: ".copilot",
        skip_entries: &["sandbox"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "pi",
        host_rel: ".pi",
        container_suffix: ".pi",
        skip_entries: &["sandbox"],
        seed_files: &[],
        copy_dirs: &["agent"],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "hermes",
        host_rel: ".hermes",
        container_suffix: ".hermes",
        // Skip Hermes-specific runtime/state dirs that should not bleed from
        // the host into the sandbox: see paths used by the upstream agent
        // (HERMES_HOME / ...). state.db is per-instance SQLite state.
        skip_entries: &[
            "sandbox",
            "sessions",
            "logs",
            "cache",
            "pastes",
            "images",
            "chrome-debug",
            "tmp",
            "state.db",
        ],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        // shell-hooks-allowlist.json is regenerated by install_hermes_hooks
        // on every session, but we preserve it in case the user has
        // additional approvals beyond the AoE-managed ones.
        preserve_files: &["shell-hooks-allowlist.json"],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "droid",
        host_rel: ".factory",
        container_suffix: ".factory",
        skip_entries: &["sandbox"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "kiro",
        host_rel: ".kiro",
        container_suffix: ".kiro",
        skip_entries: &["sandbox", "sessions", "logs", "cache"],
        seed_files: &[],
        copy_dirs: &["agents", "steering", "prompts", "settings"],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "qwen",
        host_rel: ".qwen",
        container_suffix: ".qwen",
        skip_entries: &["sandbox", "sessions", "cache"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        clean_files: &[],
    },
    AgentConfigMount {
        tool_name: "antigravity",
        host_rel: ".gemini/antigravity-cli",
        container_suffix: ".gemini/antigravity-cli",
        skip_entries: &["sandbox", "logs", "cache"],
        seed_files: &[],
        copy_dirs: &["plugins"],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &["antigravity-oauth-token"],
        clean_files: &[],
    },
];

/// Sync host agent config into the shared sandbox directory. Copies top-level files
/// and `copy_dirs` from the host (always overwritten on refresh). Seed files are
/// write-once: only created if they don't already exist, so container-accumulated
/// changes (e.g. permission approvals) are preserved across sessions.
fn sync_agent_config(
    host_dir: &Path,
    sandbox_dir: &Path,
    skip_entries: &[&str],
    seed_files: &[(&str, &str)],
    copy_dirs: &[&str],
    preserve_files: &[&str],
) -> Result<()> {
    std::fs::create_dir_all(sandbox_dir)?;

    // Write-once: only seed files that don't already exist.
    for &(name, content) in seed_files {
        let path = sandbox_dir.join(name);
        if !path.exists() {
            std::fs::write(path, content)?;
        }
    }

    // If the sandbox already has a "projects/" subdirectory, a prior container
    // session ran and created state we must not overwrite (e.g. settings.json,
    // statsig/, session metadata). Only seed files, copy_dirs, and keychain
    // credentials are still synced; the general top-level file copy is skipped.
    //
    // Why "projects/"? Claude Code creates this directory on first run to store
    // per-project session data. Its presence reliably indicates the container
    // has been used before. If this sentinel changes upstream, container restarts
    // would fall back to the old behavior of re-copying all host files (safe,
    // just potentially overwriting container-side customizations).
    let has_prior_data = sandbox_dir.join("projects").exists();
    if has_prior_data {
        tracing::info!(target: "session.profile",
            "sync_agent_config: sandbox={} has prior session data, skipping general file copy",
            sandbox_dir.display()
        );
    }

    for entry in std::fs::read_dir(host_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if skip_entries.iter().any(|&s| s == name_str.as_ref()) {
            continue;
        }

        // Follow symlinks so symlinked dirs are treated as dirs.
        let metadata = match std::fs::metadata(entry.path()) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(target: "session.profile", "Skipping {}: {}", entry.path().display(), e);
                continue;
            }
        };

        if metadata.is_dir() {
            // copy_dirs (e.g. plugins, skills) are host -> sandbox pushes. Like
            // the general file copy below, skip them once the sandbox has prior
            // session data: re-copying a large tree (plugins can hold full git
            // clones) on every restart stalled startup for tens of seconds and
            // would clobber container-side changes. A fresh sandbox still gets
            // them on its first launch.
            if !has_prior_data && copy_dirs.iter().any(|&d| d == name_str.as_ref()) {
                let dest = sandbox_dir.join(&name);
                if let Err(e) = copy_dir_recursive(&entry.path(), &dest) {
                    tracing::warn!(target: "session.profile", "Failed to copy dir {}: {}", name_str, e);
                }
            }
            continue;
        }

        // Skip general top-level file copies on restart to preserve
        // container-created files (settings.json, statsig/, etc.).
        if has_prior_data {
            continue;
        }

        let dest = sandbox_dir.join(&name);

        // Preserved files are only seeded from the host when they don't already exist
        // in the sandbox. This protects credentials placed by migration or in-container
        // authentication from being overwritten by stale host copies.
        if preserve_files.iter().any(|&p| p == name_str.as_ref()) && dest.exists() {
            continue;
        }

        if let Err(e) = std::fs::copy(entry.path(), &dest) {
            tracing::warn!(target: "session.profile", "Failed to copy {}: {}", name_str, e);
        }
    }

    Ok(())
}

fn rewrite_claude_plugin_paths(sandbox_dir: &Path, host_home: &Path) -> Result<()> {
    const CONTAINER_HOME: &str = "/root";

    let plugins_dir = sandbox_dir.join("plugins");
    if !plugins_dir.exists() {
        return Ok(());
    }

    let host_home_str = host_home.to_string_lossy();
    let targets = [
        plugins_dir.join("known_marketplaces.json"),
        plugins_dir.join("installed_plugins.json"),
        plugins_dir
            .join("marketplaces")
            .join("known_marketplaces.json"),
        plugins_dir
            .join("marketplaces")
            .join("installed_plugins.json"),
    ];

    for path in targets {
        if !path.exists() {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(target: "session.profile", "Failed to read {}: {}", path.display(), e);
                continue;
            }
        };

        let mut value: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(target: "session.profile", "Failed to parse {}: {}", path.display(), e);
                continue;
            }
        };

        let mut changed = false;
        rewrite_plugin_value_paths(&mut value, &host_home_str, CONTAINER_HOME, &mut changed);

        if changed {
            let serialized = serde_json::to_string(&value)?;
            if let Err(e) = std::fs::write(&path, serialized) {
                tracing::warn!(target: "session.profile", "Failed to write {}: {}", path.display(), e);
            }
        }
    }

    Ok(())
}

fn rewrite_plugin_value_paths(
    value: &mut serde_json::Value,
    host_home: &str,
    container_home: &str,
    changed: &mut bool,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if key == "installLocation" || key == "installPath" {
                    if let serde_json::Value::String(path) = val {
                        if path.starts_with(host_home) {
                            *path = format!("{}{}", container_home, &path[host_home.len()..]);
                            *changed = true;
                        }
                    }
                }
                rewrite_plugin_value_paths(val, host_home, container_home, changed);
            }
        }
        serde_json::Value::Array(values) => {
            for val in values {
                rewrite_plugin_value_paths(val, host_home, container_home, changed);
            }
        }
        _ => {}
    }
}

/// Recursively copy a directory tree, following symlinks.
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    let mut visited = std::collections::HashSet::new();
    copy_dir_recursive_inner(src, dest, &mut visited)
}

/// Whether an I/O error during the copy means the destination filesystem can no
/// longer accept writes, in which case continuing would silently produce a
/// partial copy (a sandbox missing arbitrary files, reported as success). Those
/// must abort the whole copy. Everything else (a single unreadable or dangling
/// source entry) is skipped best-effort.
fn is_fatal_copy_error(e: &std::io::Error) -> bool {
    // ENOSPC (no space), EROFS (read-only fs), EDQUOT (quota). EDQUOT differs by
    // platform (122 on Linux, 69 on macOS). raw_os_error covers all three more
    // portably than the (partly unstable) ErrorKind variants.
    matches!(e.raw_os_error(), Some(28) | Some(30) | Some(69) | Some(122))
}

fn copy_dir_recursive_inner(
    src: &Path,
    dest: &Path,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Result<()> {
    // Break symlink cycles. We follow symlinks (a legitimately symlinked config
    // dir should be copied), but a link that points back up its own tree would
    // otherwise recurse forever; a real cycle under ~/.claude/plugins churned
    // for 30s before aborting. Keying on the canonical (symlink-resolved) source
    // path stops the second visit. A canonicalize failure (e.g. ELOOP) is
    // exactly when we most need the guard, so skip the dir rather than descend
    // blindly.
    match std::fs::canonicalize(src) {
        Ok(real) => {
            if !visited.insert(real) {
                tracing::warn!(
                    target: "session.profile",
                    "skipping already-visited dir (symlink cycle?): {}",
                    src.display()
                );
                return Ok(());
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "session.profile",
                "skipping dir (cannot resolve, possible symlink loop): {}: {}",
                src.display(),
                e
            );
            return Ok(());
        }
    }
    // create_dir_all / read_dir failures: a filesystem that can't accept writes
    // is fatal (silent partial copy); anything else means we just skip this
    // subtree. This keeps the fail-soft behavior errno-aware and consistent.
    if let Err(e) = std::fs::create_dir_all(dest) {
        if is_fatal_copy_error(&e) {
            return Err(e).with_context(|| format!("creating {}", dest.display()));
        }
        tracing::warn!(
            target: "session.profile",
            "skipping dir (cannot create destination): {}: {}",
            dest.display(),
            e
        );
        return Ok(());
    }
    let entries = match std::fs::read_dir(src) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                target: "session.profile",
                "skipping dir (cannot read): {}: {}",
                src.display(),
                e
            );
            return Ok(());
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(target: "session.profile", "skipping entry in {}: {}", src.display(), e);
                continue;
            }
        };
        let target = dest.join(entry.file_name());
        // Follow symlinks so symlinked dirs/files are handled correctly; the
        // visited-set guard above stops a cycle from looping forever. A single
        // unreadable or dangling entry is skipped rather than aborting the whole
        // copy: one Permission-denied file under ~/.claude/plugins used to fail
        // the entire sync.
        let metadata = match std::fs::metadata(entry.path()) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    target: "session.profile",
                    "skipping {}: {}",
                    entry.path().display(),
                    e
                );
                continue;
            }
        };
        if metadata.is_dir() {
            // Propagate with `?`: the child only returns Err for a fatal
            // (filesystem-full) error, so a cycle / unreadable subdir resolves to
            // Ok inside the child and is skipped there, while a real out-of-space
            // condition aborts the whole copy.
            copy_dir_recursive_inner(&entry.path(), &target, visited)?;
        } else if let Err(e) = std::fs::copy(entry.path(), &target) {
            if is_fatal_copy_error(&e) {
                return Err(e).with_context(|| format!("copying {}", entry.path().display()));
            }
            tracing::warn!(
                target: "session.profile",
                "skipping file {}: {}",
                entry.path().display(),
                e
            );
        }
    }
    Ok(())
}

/// Parse the `expiresAt` timestamp from a Claude Code credential JSON string.
/// Returns `None` if the JSON is malformed or the field is missing/wrong type.
#[cfg(any(target_os = "macos", test))]
fn parse_credential_expires_at(content: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    value.get("claudeAiOauth")?.get("expiresAt")?.as_u64()
}

/// Decide whether an incoming credential should overwrite the existing one,
/// based on `expiresAt` timestamps. Returns `true` if the incoming credential
/// should be written.
#[cfg(any(target_os = "macos", test))]
fn should_overwrite_credential(existing_content: &str, incoming_content: &str) -> bool {
    let existing_exp = parse_credential_expires_at(existing_content);
    let incoming_exp = parse_credential_expires_at(incoming_content);

    match (existing_exp, incoming_exp) {
        (Some(existing), Some(incoming)) => incoming > existing,
        (Some(_), None) => false,
        _ => true,
    }
}

/// Extract credentials from the macOS Keychain and write to a file.
/// Returns Ok(true) if credentials were written, Ok(false) if not available.
#[cfg(target_os = "macos")]
fn extract_keychain_credential(service: &str, dest: &Path) -> Result<bool> {
    use std::process::Command;

    let user = std::env::var("USER").unwrap_or_default();
    let output = Command::new("security")
        .args(["find-generic-password", "-a"])
        .arg(&user)
        .args(["-w", "-s", service])
        .output()?;

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Exit code 36 = errSecInteractionNotAllowed (keychain locked or ACL denied)
        // Exit code 44 = errSecItemNotFound
        if code == 36 {
            tracing::warn!(target: "session.profile",
                "Keychain access denied for service '{}' (exit code 36). \
                 The keychain may be locked. Run 'security unlock-keychain' and restart. \
                 Stderr: {}",
                service,
                stderr.trim()
            );
        } else if code == 44 {
            tracing::debug!(target: "session.profile",
                "No keychain entry found for service '{}' (account '{}')",
                service,
                user
            );
        } else {
            tracing::warn!(target: "session.profile",
                "Failed to extract keychain credential for service '{}' \
                 (account '{}', exit code {}): {}",
                service,
                user,
                code,
                stderr.trim()
            );
        }
        return Ok(false);
    }

    let content = String::from_utf8_lossy(&output.stdout);
    let trimmed = content.trim();
    if trimmed.is_empty() {
        tracing::warn!(target: "session.profile",
            "Keychain entry for service '{}' exists but has empty content",
            service
        );
        return Ok(false);
    }

    // Only overwrite if the keychain credential is fresher than what the sandbox already has.
    if dest.exists() {
        if let Ok(existing_content) = std::fs::read_to_string(dest) {
            if !should_overwrite_credential(&existing_content, trimmed) {
                tracing::debug!(target: "session.profile",
                    "Keychain credential for '{}' is not fresher than sandbox, keeping sandbox",
                    service,
                );
                return Ok(false);
            }
        }
    }

    std::fs::write(dest, trimmed)?;
    tracing::debug!(target: "session.profile",
        "Extracted keychain credential for '{}' -> {}",
        service,
        dest.display()
    );
    Ok(true)
}

#[cfg(not(target_os = "macos"))]
fn extract_keychain_credential(_service: &str, _dest: &Path) -> Result<bool> {
    Ok(false)
}

/// Sync a single agent's host config into its shared sandbox directory.
/// Handles config file sync, keychain credential extraction, and home-level seed files.
fn prepare_sandbox_dir(mount: &AgentConfigMount, home: &Path) -> Result<std::path::PathBuf> {
    let host_dir = home.join(mount.host_rel);
    let sandbox_dir = home.join(mount.host_rel).join(SANDBOX_SUBDIR);

    // Remove stale files before syncing. This prevents leftovers from a previous
    // session (e.g. a SQLite database created by an older tool version) from
    // causing failures when the container image is updated.
    for &name in mount.clean_files {
        let path = sandbox_dir.join(name);
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(target: "session.profile", "Failed to clean {}: {}", path.display(), e);
            }
        }
    }

    if host_dir.exists() {
        sync_agent_config(
            &host_dir,
            &sandbox_dir,
            mount.skip_entries,
            mount.seed_files,
            mount.copy_dirs,
            mount.preserve_files,
        )?;

        if mount.tool_name == "claude" {
            if let Err(e) = rewrite_claude_plugin_paths(&sandbox_dir, home) {
                tracing::warn!(target: "session.profile",
                    "Failed to rewrite Claude plugin paths in {}: {}",
                    sandbox_dir.display(),
                    e
                );
            }
        }

        if let Some((service, filename)) = mount.keychain_credential {
            if let Err(e) = extract_keychain_credential(service, &sandbox_dir.join(filename)) {
                tracing::warn!(target: "session.profile",
                    "Failed to extract keychain credential for {}: {}",
                    mount.host_rel,
                    e
                );
            }
        }
    } else {
        std::fs::create_dir_all(&sandbox_dir)?;
    }

    for &(filename, content) in mount.home_seed_files {
        let path = sandbox_dir.join(filename);
        if !path.exists() {
            std::fs::write(&path, content)?;
        }
    }

    Ok(sandbox_dir)
}

/// Compute volume mount paths for Docker container.
///
/// For bare repo worktrees (worktree inside the repo), mounts the main repo.
/// For sibling worktrees (non-bare layout), mounts the main repo and worktree
/// as separate volumes at paths preserving their relative structure.
/// For non-git paths, mounts the project path directly.
///
/// `project_path_str` is the raw project path string (used as the host mount path in the
/// default case where no worktree is detected).
///
/// Returns (host_mount_path, container_mount_path, working_dir)
pub(crate) fn compute_volume_paths(
    project_path: &Path,
    project_path_str: &str,
) -> Result<(Vec<VolumeMount>, String)> {
    // Only look for a main repo if the project path itself has a .git entry (file or
    // directory). This prevents git2::Repository::discover from walking up the directory
    // tree and finding an unrelated ancestor repo (e.g., a dotfile-managed home directory),
    // which would cause aoe to mount that ancestor -- potentially the user's entire $HOME --
    // into the container.
    //
    // Legitimate git repos have a .git directory; worktrees have a .git file containing a
    // gitdir pointer. Both cases are covered by this check.
    if project_path.join(".git").exists() {
        if let Ok(main_repo) = GitWorktree::find_main_repo(project_path) {
            // Canonicalize paths for reliable comparison (handles symlinks like /tmp -> /private/tmp)
            let main_repo_canonical = main_repo
                .canonicalize()
                .unwrap_or_else(|_| main_repo.clone());
            let project_canonical = project_path
                .canonicalize()
                .unwrap_or_else(|_| project_path.to_path_buf());

            // Check if project_path is a worktree (different from the main repo root).
            // Mount enough of the filesystem so the worktree's relative gitdir reference
            // resolves correctly inside the container.
            if main_repo_canonical != project_canonical {
                if project_canonical.starts_with(&main_repo_canonical) {
                    // Worktree is inside the main repo (bare repo layout) --
                    // mounting the main repo is sufficient.
                    let name = main_repo_canonical
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "workspace".to_string());
                    let container_base = format!("/workspace/{}", name);
                    let relative_worktree = project_canonical
                        .strip_prefix(&main_repo_canonical)
                        .map(|p| p.to_path_buf())
                        .unwrap_or_default();
                    let working_dir = if relative_worktree.as_os_str().is_empty() {
                        container_base.clone()
                    } else {
                        format!("{}/{}", container_base, relative_worktree.display())
                    };

                    return Ok((
                        vec![VolumeMount {
                            host_path: main_repo_canonical.to_string_lossy().to_string(),
                            container_path: container_base,
                            read_only: false,
                        }],
                        working_dir,
                    ));
                } else {
                    // Worktree is a sibling of the main repo (non-bare layout).
                    // Mount each separately under /workspace/, preserving their
                    // relative path structure from their common ancestor. This
                    // ensures the worktree's .git file (which contains a relative
                    // gitdir path) resolves correctly inside the container.
                    let common = common_ancestor(&main_repo_canonical, &project_canonical);
                    let repo_rel = main_repo_canonical
                        .strip_prefix(&common)
                        .unwrap_or(&main_repo_canonical);
                    let wt_rel = project_canonical
                        .strip_prefix(&common)
                        .unwrap_or(&project_canonical);

                    let repo_container = format!("/workspace/{}", repo_rel.display());
                    let wt_container = format!("/workspace/{}", wt_rel.display());

                    return Ok((
                        vec![
                            VolumeMount {
                                host_path: main_repo_canonical.to_string_lossy().to_string(),
                                container_path: repo_container,
                                read_only: false,
                            },
                            VolumeMount {
                                host_path: project_canonical.to_string_lossy().to_string(),
                                container_path: wt_container.clone(),
                                read_only: false,
                            },
                        ],
                        wt_container,
                    ));
                }
            }
        }
    }

    // Default behavior: mount project_path directly
    let dir_name = project_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());
    let workspace_path = format!("/workspace/{}", dir_name);

    Ok((
        vec![VolumeMount {
            host_path: project_path_str.to_string(),
            container_path: workspace_path.clone(),
            read_only: false,
        }],
        workspace_path,
    ))
}

/// Compute volume mounts for a multi-repo workspace.
///
/// The workspace directory contains worktrees that point back to their main repos
/// via relative paths in `.git` files. We need to mount both the workspace directory
/// AND all main repos so these relative gitdir references resolve inside the container.
///
/// We find the common ancestor of all paths (workspace + main repos) and mount each
/// under `/workspace/` preserving relative structure.
fn compute_workspace_volume_paths(
    workspace_path: &Path,
    ws_info: &super::WorkspaceInfo,
) -> Result<(Vec<VolumeMount>, String)> {
    let workspace_canonical = workspace_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_path.to_path_buf());

    // Collect all unique main repo paths
    let mut main_repo_paths: Vec<PathBuf> = Vec::new();
    for repo in &ws_info.repos {
        let main_path = PathBuf::from(&repo.main_repo_path);
        let canonical = main_path
            .canonicalize()
            .unwrap_or_else(|_| main_path.clone());
        if !main_repo_paths.iter().any(|p| p == &canonical) {
            main_repo_paths.push(canonical);
        }
    }

    // Find common ancestor of workspace dir and all main repos
    let mut common = workspace_canonical.clone();
    for repo_path in &main_repo_paths {
        common = common_ancestor(&common, repo_path);
    }

    // Mount workspace dir
    let ws_rel = workspace_canonical
        .strip_prefix(&common)
        .unwrap_or(&workspace_canonical);
    let ws_container = format!("/workspace/{}", ws_rel.display());

    let mut volumes = vec![VolumeMount {
        host_path: workspace_canonical.to_string_lossy().to_string(),
        container_path: ws_container.clone(),
        read_only: false,
    }];

    // Mount each main repo (needed for .git/worktrees/ references)
    for repo_path in &main_repo_paths {
        let repo_rel = repo_path.strip_prefix(&common).unwrap_or(repo_path);
        let repo_container = format!("/workspace/{}", repo_rel.display());

        // Skip if already covered by the workspace mount
        if repo_path.starts_with(&workspace_canonical) {
            continue;
        }

        volumes.push(VolumeMount {
            host_path: repo_path.to_string_lossy().to_string(),
            container_path: repo_container,
            read_only: false,
        });
    }

    Ok((volumes, ws_container))
}

/// Re-sync shared sandbox directories from the host so the container picks up
/// any credential changes (e.g. re-auth) since it was created.
pub(crate) fn refresh_agent_configs() {
    let Some(home) = dirs::home_dir() else {
        return;
    };

    let hooks_enabled = super::config::Config::load()
        .map(|c| c.session.agent_status_hooks)
        .unwrap_or(true);

    for mount in AGENT_CONFIG_MOUNTS {
        let refresh_codex_hooks = hooks_enabled && should_refresh_codex_hooks(mount, &home);
        let preserved_codex_state = if refresh_codex_hooks {
            let config_path = home
                .join(mount.host_rel)
                .join(SANDBOX_SUBDIR)
                .join("config.toml");
            match crate::hooks::snapshot_codex_hooks_state(&config_path) {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!(target: "session.profile",
                        "Failed to read Codex sandbox hook state from {}: {}",
                        config_path.display(),
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        match prepare_sandbox_dir(mount, &home) {
            Ok(sandbox_dir) => {
                if refresh_codex_hooks {
                    refresh_codex_sandbox_hooks(&sandbox_dir, preserved_codex_state);
                }
            }
            Err(e) => {
                tracing::warn!(target: "session.profile",
                    "Failed to refresh agent config for {}: {}",
                    mount.host_rel,
                    e
                );
            }
        }
    }
}

fn should_refresh_codex_hooks(mount: &AgentConfigMount, home: &Path) -> bool {
    if mount.tool_name != "codex" || mount.host_rel != ".codex" {
        return false;
    }

    let host_config = home.join(mount.host_rel).join("config.toml");
    let sandbox_config = home
        .join(mount.host_rel)
        .join(SANDBOX_SUBDIR)
        .join("config.toml");
    host_config.exists() || sandbox_config.exists()
}

fn refresh_codex_sandbox_hooks(sandbox_dir: &Path, preserved_state: Option<toml_edit::Item>) {
    let Some(hook_cfg) = crate::agents::get_agent("codex").and_then(|a| a.hook_config.as_ref())
    else {
        return;
    };

    let config_path = sandbox_dir.join("config.toml");
    if let Err(e) = crate::hooks::install_codex_hooks_with_preserved_state(
        &config_path,
        hook_cfg.events,
        preserved_state,
    ) {
        tracing::warn!(
            "Failed to refresh Codex hooks in sandbox config {}: {}",
            config_path.display(),
            e
        );
    }
}

fn resolve_active_agent(
    tool: &str,
    detect_as: Option<&str>,
    session_config: &super::config::SessionConfig,
) -> Option<&'static crate::agents::AgentDef> {
    crate::agents::get_agent(tool)
        .or_else(|| {
            detect_as
                .filter(|name| !name.is_empty())
                .and_then(crate::agents::get_agent)
        })
        .or_else(|| {
            session_config
                .agent_detect_as
                .get(tool)
                .and_then(|detect_as| crate::agents::get_agent(detect_as))
        })
}

fn agent_config_container_path(
    mount: &AgentConfigMount,
    container_home: &str,
    environment: &[EnvEntry],
) -> String {
    let default_path = format!("{}/{}", container_home, mount.container_suffix);
    if mount.tool_name != "codex" || mount.host_rel != ".codex" {
        return default_path;
    }

    let Some(codex_home) = environment
        .iter()
        .find(|entry| entry.key() == "CODEX_HOME")
        .map(EnvEntry::value)
    else {
        return default_path;
    };

    if codex_home == "/" || !codex_home.starts_with('/') {
        tracing::warn!(
            "Ignoring sandbox CODEX_HOME for Codex config mount because it is not a usable absolute container directory: {}",
            codex_home
        );
        return default_path;
    }

    let normalized = codex_home.trim_end_matches('/');
    if normalized.is_empty() {
        tracing::warn!(
            "Ignoring sandbox CODEX_HOME for Codex config mount because it resolves to an empty container directory"
        );
        return default_path;
    }

    normalized.to_string()
}

#[derive(Clone, Copy)]
pub(crate) struct ContainerAgentSelection<'a> {
    tool: &'a str,
    detect_as: Option<&'a str>,
}

impl<'a> ContainerAgentSelection<'a> {
    pub(crate) fn new(tool: &'a str, detect_as: Option<&'a str>) -> Self {
        Self { tool, detect_as }
    }
}

/// A `volume_ignores` entry containing glob metacharacters is expanded against the
/// mounted workspace roots at container-create time (#2045); a literal entry is
/// concatenated onto each mount base unconditionally. This distinguishes the two.
pub(crate) fn has_glob_metachars(entry: &str) -> bool {
    entry.contains(['*', '?', '[', ']'])
}

/// Expand one glob `volume_ignores` entry against the host filesystem under each
/// mounted workspace root, returning the container-side mount paths for every
/// directory that matches right now.
///
/// `roots` is `(host_path, container_path)` for each project mount. Expansion is a
/// point-in-time snapshot: Docker needs concrete mount paths when the container
/// starts, so a directory created later by an in-container build is not shadowed.
/// Only directories are returned; files can't be ignore mounts. The glob's leading
/// host prefix is escaped so a literal `[` in the real path isn't treated as a
/// character class.
fn expand_glob_ignore(entry: &str, roots: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (host_base, container_base) in roots {
        let host_trimmed = host_base.trim_end_matches('/');
        let pattern = format!("{}/{}", glob::Pattern::escape(host_trimmed), entry);
        let matches = match glob::glob(&pattern) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    target: "session.profile",
                    "Skipping volume_ignores glob '{}': invalid pattern: {}",
                    entry, e
                );
                continue;
            }
        };
        for matched in matches {
            let path = match matched {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(target: "session.profile", "glob walk error for '{}': {}", entry, e);
                    continue;
                }
            };
            if !path.is_dir() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(host_trimmed) else {
                continue;
            };
            let rel = rel.to_string_lossy();
            if rel.is_empty() {
                continue;
            }
            out.push(format!("{}/{}", container_base, rel));
        }
    }
    out
}

/// One glob `volume_ignores` entry paired with the directories it currently
/// matches, expressed as container-side mount paths. The empty-`matches` case is
/// kept (rather than dropped) so callers can tell "configured but matched nothing"
/// from "no glob configured".
#[derive(Debug, Clone)]
pub(crate) struct GlobIgnoreExpansion {
    pub pattern: String,
    pub matched_container_paths: Vec<String>,
}

/// Compute how glob `volume_ignores` entries would expand for a session rooted at
/// `project_path_str`, without creating any container. The TUI confirm gate and the
/// web preview endpoint both call this so they describe the exact snapshot
/// [`build_container_config`] will materialize. Literal (non-glob) entries are
/// excluded; an `Ok(vec![])` means nothing needs confirming.
pub(crate) fn preview_glob_volume_ignores(
    project_path_str: &str,
    workspace_info: Option<&super::WorkspaceInfo>,
    volume_ignores: &[String],
) -> Result<Vec<GlobIgnoreExpansion>> {
    // An empty project path (scratch session) has no workspace to expand
    // against; globbing it would resolve to filesystem-root patterns. Nothing
    // to preview.
    if project_path_str.is_empty() {
        return Ok(Vec::new());
    }

    let glob_entries: Vec<&String> = volume_ignores
        .iter()
        .filter(|e| has_glob_metachars(e))
        .collect();
    if glob_entries.is_empty() {
        return Ok(Vec::new());
    }

    let project_path = Path::new(project_path_str);
    let (project_volumes, _workspace_path) = if let Some(ws_info) = workspace_info {
        compute_workspace_volume_paths(project_path, ws_info)?
    } else {
        compute_volume_paths(project_path, project_path_str)?
    };
    let roots = glob_roots(&project_volumes);

    Ok(glob_entries
        .into_iter()
        .map(|pattern| GlobIgnoreExpansion {
            pattern: pattern.clone(),
            matched_container_paths: expand_glob_ignore(pattern, &roots),
        })
        .collect())
}

/// `(host_path, container_path)` pairs for the project mounts, the roots glob
/// `volume_ignores` are expanded against.
fn glob_roots(project_volumes: &[VolumeMount]) -> Vec<(String, String)> {
    project_volumes
        .iter()
        .map(|v| (v.host_path.clone(), v.container_path.clone()))
        .collect()
}

/// Produce a deterministic Docker volume name for a named volume_ignores mount.
///
/// Uses the full session ID as a prefix so volumes can be enumerated on deletion.
/// A short hash of the container path is appended to handle slug collisions.
fn named_volume_for(session_id: &str, container_path: &str) -> String {
    use std::hash::{Hash, Hasher};
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    let slug: String = sanitize(container_path.trim_start_matches('/'))
        .chars()
        .take(40)
        .collect();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    container_path.hash(&mut h);
    let hash = format!("{:x}", h.finish());
    let hash12 = &hash[..12.min(hash.len())];
    format!("aoe-vi-{}-{}-{}", session_id, slug, hash12)
}

/// Build a full `ContainerConfig` for creating a sandboxed container.
///
/// `profile` selects which profile's overrides (volumes, mount_ssh, volume_ignores)
/// are merged on top of the global config. An empty `profile` falls back to the
/// user's globally configured default profile.
pub(crate) fn build_container_config(
    project_path_str: &str,
    sandbox_info: &SandboxInfo,
    agent_selection: ContainerAgentSelection<'_>,
    is_yolo_mode: bool,
    instance_id: &str,
    workspace_info: Option<&super::WorkspaceInfo>,
    profile: &str,
) -> Result<ContainerConfig> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;

    let project_path = Path::new(project_path_str);
    let resolved_profile = super::config::effective_profile(profile);
    let profile_session_config =
        super::profile_config::resolve_config_or_warn(&resolved_profile).session;
    let active_agent = resolve_active_agent(
        agent_selection.tool,
        agent_selection.detect_as,
        &profile_session_config,
    );
    let config_tool = active_agent.map_or(agent_selection.tool, |agent| agent.name);

    // Determine mount path(s) and working directory.
    // For multi-repo workspaces, mount the workspace dir and all main repos.
    // For bare repo worktrees, mount the entire bare repo and set working_dir to the worktree.
    // For sibling worktrees, mount the main repo and worktree as separate volumes.
    let (project_volumes, workspace_path) = if let Some(ws_info) = workspace_info {
        compute_workspace_volume_paths(project_path, ws_info)?
    } else {
        compute_volume_paths(project_path, project_path_str)?
    };

    // Collect all paths that should receive volume_ignores: the workspace_path
    // (where builds happen) plus every project mount root (which may differ in
    // bare-repo layouts where workspace_path is a subdirectory of the mount).
    let mut volume_ignore_bases: Vec<String> = project_volumes
        .iter()
        .map(|v| v.container_path.clone())
        .collect();
    if !volume_ignore_bases.contains(&workspace_path) {
        volume_ignore_bases.push(workspace_path.clone());
    }

    // (host, container) roots for expanding glob volume_ignores against the live
    // filesystem. Captured before `project_volumes` is moved into `volumes`.
    let glob_roots = glob_roots(&project_volumes);

    let mut volumes = project_volumes;

    let sandbox_config = {
        match super::repo_config::resolve_config_with_repo(&resolved_profile, project_path) {
            Ok(c) => {
                tracing::debug!(target: "session.profile",
                    "Loaded sandbox config: extra_volumes={:?}, mount_ssh={}, volume_ignores={:?}",
                    c.sandbox.extra_volumes,
                    c.sandbox.mount_ssh,
                    c.sandbox.volume_ignores
                );
                c.sandbox
            }
            Err(e) => {
                tracing::warn!(target: "session.profile", "Failed to load config, using defaults: {}", e);
                Default::default()
            }
        }
    };

    const CONTAINER_HOME: &str = "/root";

    let mut environment = collect_environment(&sandbox_config, sandbox_info);

    let gitconfig = home.join(".gitconfig");
    if gitconfig.exists() {
        volumes.push(VolumeMount {
            host_path: gitconfig.to_string_lossy().to_string(),
            container_path: format!("{}/.gitconfig", CONTAINER_HOME),
            read_only: true,
        });
    }

    if sandbox_config.mount_ssh {
        let ssh_dir = home.join(".ssh");
        if ssh_dir.exists() {
            volumes.push(VolumeMount {
                host_path: ssh_dir.to_string_lossy().to_string(),
                container_path: format!("{}/.ssh", CONTAINER_HOME),
                read_only: true,
            });
        }
    }

    // Mount GCP credentials into the well-known ADC path for Claude+Vertex sessions.
    // Gated on `tool == "claude"` because `CLAUDE_CODE_USE_VERTEX` is Claude-specific;
    // there's no reason to expose GCP creds to other agents (opencode, codex, etc.)
    // just because the user has the flag exported globally.
    // `GOOGLE_APPLICATION_CREDENTIALS` is not forwarded as an env var; client libraries
    // discover the well-known path automatically.
    if agent_selection.tool == "claude" && super::environment::host_vertex_enabled() {
        let container_cred_path = format!(
            "{}/.config/gcloud/application_default_credentials.json",
            CONTAINER_HOME
        );
        if let Ok(cred_path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
            let cred_file = std::path::Path::new(&cred_path);
            if cred_file.exists() {
                volumes.push(VolumeMount {
                    host_path: cred_path.clone(),
                    container_path: container_cred_path,
                    read_only: true,
                });
            } else {
                tracing::warn!(target: "session.profile",
                    "GOOGLE_APPLICATION_CREDENTIALS points to non-existent file: {}",
                    cred_path
                );
            }
        } else {
            let adc_path = home.join(".config/gcloud/application_default_credentials.json");
            if adc_path.exists() {
                volumes.push(VolumeMount {
                    host_path: adc_path.to_string_lossy().to_string(),
                    container_path: container_cred_path,
                    read_only: true,
                });
            }
        }
    }

    // Sync host agent config into a shared sandbox directory per agent and
    // bind-mount it read-write. Only mount the config for the active tool.
    // Agent definitions are in AGENT_CONFIG_MOUNTS -- add new agents there, not here.
    for mount in AGENT_CONFIG_MOUNTS
        .iter()
        .filter(|m| m.tool_name == config_tool)
    {
        let container_path = agent_config_container_path(mount, CONTAINER_HOME, &environment);

        let sandbox_dir = match prepare_sandbox_dir(mount, &home) {
            Ok(dir) => dir,
            Err(e) => {
                tracing::warn!(target: "session.profile",
                    "Failed to prepare sandbox dir for {}, skipping: {}",
                    mount.host_rel,
                    e
                );
                continue;
            }
        };

        tracing::debug!(target: "session.profile",
            "Sandbox dir ready for {}, binding {} -> {}",
            mount.host_rel,
            sandbox_dir.display(),
            container_path
        );
        volumes.push(VolumeMount {
            host_path: sandbox_dir.to_string_lossy().to_string(),
            container_path,
            read_only: false,
        });

        // Home-level seed files are mounted as individual files at the container
        // home directory (already written by prepare_sandbox_dir).
        for &(filename, _) in mount.home_seed_files {
            let file_path = sandbox_dir.join(filename);
            if file_path.exists() {
                volumes.push(VolumeMount {
                    host_path: file_path.to_string_lossy().to_string(),
                    container_path: format!("{}/{}", CONTAINER_HOME, filename),
                    read_only: false,
                });
            }
        }
    }

    let hooks_enabled = profile_session_config.agent_status_hooks;
    if let Some(agent) = active_agent {
        if hooks_enabled {
            // Sidecar agents (hermes YAML, kiro per-agent JSON) use schemas the
            // generic hook_config path below cannot emit; they install through
            // their SidecarHooks installer at the sandbox config subpath.
            if agent.sidecar_hooks.is_some() || agent.hook_config.is_some() {
                let hook_dir = crate::hooks::hook_status_dir(instance_id).context(
                    "refusing to mount hook directory: AOE_INSTANCE_ID failed validation",
                )?;
                if let Err(e) = std::fs::create_dir_all(&hook_dir) {
                    tracing::warn!(target: "session.profile",
                        "Failed to create hook directory {}: {}",
                        hook_dir.display(),
                        e
                    );
                }
                volumes.push(VolumeMount {
                    host_path: hook_dir.to_string_lossy().to_string(),
                    container_path: hook_dir.to_string_lossy().to_string(),
                    read_only: false,
                });
            }

            if let Some(sidecar) = &agent.sidecar_hooks {
                let config_file = home.join(sidecar.sandbox_config_subpath);
                if let Err(e) = (sidecar.install)(&config_file) {
                    tracing::warn!(target: "session.profile", "Failed to install {} hooks in sandbox: {}", agent.name, e);
                }
            } else if let Some(hook_cfg) = &agent.hook_config {
                // Install hooks into the sandbox config file for the containerized agent.
                // Shell one-liners work inside containers since they only use sh/mkdir/printf.
                let rel_path = std::path::Path::new(hook_cfg.settings_rel_path);
                let config_dir_name = rel_path.parent().unwrap_or(std::path::Path::new("."));
                let config_file_name = rel_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("settings.json");
                // Find the matching agent config mount to locate the sandbox dir
                for mount in AGENT_CONFIG_MOUNTS {
                    if std::path::Path::new(mount.host_rel) == config_dir_name {
                        let sandbox_dir = home.join(mount.host_rel).join(SANDBOX_SUBDIR);
                        let settings_file = sandbox_dir.join(config_file_name);
                        let result = if agent.name == "codex" {
                            crate::hooks::install_codex_hooks(&settings_file, hook_cfg.events)
                        } else {
                            crate::hooks::install_hooks(
                                &settings_file,
                                hook_cfg.events,
                                crate::hooks::HookInstallTarget::Sandbox,
                            )
                        };
                        if let Err(e) = result {
                            tracing::warn!(target: "session.profile", "Failed to install hooks in sandbox config: {}", e);
                        }
                        break;
                    }
                }
            }
        }
    }

    if let Some(agent) = active_agent {
        for &(key, value) in agent.container_env {
            environment.push(EnvEntry::Literal {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
        if is_yolo_mode {
            if let Some(crate::agents::YoloMode::EnvVar(key, value)) = &agent.yolo {
                environment.push(EnvEntry::Literal {
                    key: key.to_string(),
                    value: value.to_string(),
                });
            }
        }
    }

    // Add extra_volumes from config (host:container format)
    // Also collect container paths to filter conflicting volume_ignores later
    tracing::debug!(target: "session.profile",
        "extra_volumes from config: {:?}",
        sandbox_config.extra_volumes
    );
    let mut extra_volume_container_paths: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for entry in &sandbox_config.extra_volumes {
        let parts: Vec<&str> = entry.splitn(3, ':').collect();
        if parts.len() >= 2 {
            tracing::info!(target: "session.profile",
                "Mounting extra volume: {} -> {} (ro: {})",
                parts[0],
                parts[1],
                parts.get(2) == Some(&"ro")
            );
            extra_volume_container_paths.insert(parts[1].to_string());
            volumes.push(VolumeMount {
                host_path: parts[0].to_string(),
                container_path: parts[1].to_string(),
                read_only: parts.get(2) == Some(&"ro"),
            });
        } else {
            tracing::warn!(target: "session.profile", "Ignoring malformed extra_volume entry: {}", entry);
        }
    }

    // Resolve volume_ignores into concrete container mount paths. Literal entries
    // mount unconditionally at every workspace base (the path need not exist yet,
    // since the anonymous/named volume shadows it once created). Glob entries are
    // expanded against the host filesystem now (#2045): a point-in-time snapshot,
    // since Docker needs concrete mount paths when the container starts.
    let mut resolved_ignore_paths: Vec<String> = Vec::new();
    for ignore in &sandbox_config.volume_ignores {
        if has_glob_metachars(ignore) {
            resolved_ignore_paths.extend(expand_glob_ignore(ignore, &glob_roots));
        } else {
            for base_path in &volume_ignore_bases {
                resolved_ignore_paths.push(format!("{}/{}", base_path, ignore));
            }
        }
    }

    // Drop duplicates (a glob can match the same dir under overlapping bases) and
    // filter conflicts with extra_volumes, which take precedence over
    // volume_ignores. Conflicts: exact match, ignore is a parent of an extra, or
    // ignore sits inside an extra.
    let mut seen_ignore = std::collections::HashSet::new();
    let expanded_ignore_paths: Vec<String> = resolved_ignore_paths
        .into_iter()
        .filter(|path| seen_ignore.insert(path.clone()))
        .filter(|path| {
            !extra_volume_container_paths.iter().any(|extra_path| {
                path == extra_path
                    || extra_path.starts_with(&format!("{}/", path))
                    || path.starts_with(&format!("{}/", extra_path))
            })
        })
        .collect();

    // Route by strategy: anonymous volumes are the default; named volumes fix VirtioFS on macOS.
    let (anonymous_volumes, named_ignore_volumes): (Vec<String>, Vec<NamedVolumeMount>) =
        match sandbox_config.volume_ignores_strategy {
            VolumeIgnoresStrategy::Anonymous => (expanded_ignore_paths, vec![]),
            VolumeIgnoresStrategy::Named => {
                let named = expanded_ignore_paths
                    .into_iter()
                    .map(|container_path| NamedVolumeMount {
                        volume_name: named_volume_for(instance_id, &container_path),
                        container_path,
                    })
                    .collect();
                (vec![], named)
            }
        };

    // Deduplicate volumes by container_path (last writer wins, so extra_volumes
    // from user config override automatic mounts).
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::with_capacity(volumes.len());
    for vol in volumes.into_iter().rev() {
        if seen.insert(vol.container_path.clone()) {
            deduped.push(vol);
        } else {
            tracing::debug!(target: "session.profile", "Dropping duplicate mount for {}", vol.container_path);
        }
    }
    deduped.reverse();

    Ok(ContainerConfig {
        working_dir: workspace_path,
        volumes: deduped,
        anonymous_volumes,
        named_ignore_volumes,
        environment,
        cpu_limit: sandbox_config.cpu_limit,
        memory_limit: sandbox_config.memory_limit,
        port_mappings: sandbox_config.port_mappings.clone(),
        selinux_relabel: sandbox_config.selinux_relabel,
    })
}

/// Find the longest common ancestor path of two absolute paths.
fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    let mut a_components = a.components();
    let mut b_components = b.components();
    loop {
        match (a_components.next(), b_components.next()) {
            (Some(ac), Some(bc)) if ac == bc => result.push(ac),
            _ => break,
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // --- compute_volume_paths tests ---

    fn setup_regular_repo() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Create initial commit so HEAD is valid
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial", &tree, &[])
            .unwrap();

        let repo_path = dir.path().to_path_buf();
        (dir, repo_path)
    }

    fn setup_bare_repo_with_worktree() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let bare_path = dir.path().join(".bare");

        // Create bare repository
        let repo = git2::Repository::init_bare(&bare_path).unwrap();

        // Create initial commit
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial", &tree, &[])
            .unwrap();

        // Create .git file pointing to bare repo
        std::fs::write(dir.path().join(".git"), "gitdir: ./.bare\n").unwrap();

        // Create worktree
        let worktree_path = dir.path().join("main");
        let _ = std::process::Command::new("git")
            .args(["worktree", "add", worktree_path.to_str().unwrap(), "HEAD"])
            .current_dir(&bare_path)
            .output();

        let main_repo_path = dir.path().to_path_buf();
        (dir, main_repo_path, worktree_path)
    }

    #[test]
    fn test_compute_volume_paths_regular_repo() {
        let (_dir, repo_path) = setup_regular_repo();
        let project_path_str = repo_path.to_str().unwrap();

        let (volumes, working_dir) = compute_volume_paths(&repo_path, project_path_str).unwrap();

        assert_eq!(volumes.len(), 1);
        // Regular repo: mount path should be the project path
        assert_eq!(
            volumes[0].host_path,
            repo_path.to_string_lossy().to_string()
        );
        // Container path and working dir should be the same
        assert_eq!(volumes[0].container_path, working_dir);
        // Should be /workspace/{dir_name}
        let dir_name = repo_path.file_name().unwrap().to_string_lossy();
        assert_eq!(
            volumes[0].container_path,
            format!("/workspace/{}", dir_name)
        );
    }

    #[test]
    fn test_compute_volume_paths_non_git_directory() {
        let dir = TempDir::new().unwrap();
        let project_path_str = dir.path().to_str().unwrap();

        let (volumes, working_dir) = compute_volume_paths(dir.path(), project_path_str).unwrap();

        assert_eq!(volumes.len(), 1);
        // Non-git: mount path should be the project path
        assert_eq!(
            volumes[0].host_path,
            dir.path().to_string_lossy().to_string()
        );
        // Container path and working dir should be the same
        assert_eq!(volumes[0].container_path, working_dir);
    }

    #[test]
    fn test_compute_volume_paths_bare_repo_worktree() {
        let (_dir, main_repo_path, worktree_path) = setup_bare_repo_with_worktree();

        // Skip if worktree wasn't created (git might not be available)
        if !worktree_path.exists() {
            return;
        }

        let project_path_str = worktree_path.to_str().unwrap();

        let (volumes, working_dir) =
            compute_volume_paths(&worktree_path, project_path_str).unwrap();

        // Bare repo worktree: single mount of the repo root
        assert_eq!(volumes.len(), 1);

        // Canonicalize paths for comparison (handles /var -> /private/var on macOS)
        let mount_path_canon = Path::new(&volumes[0].host_path).canonicalize().unwrap();
        let main_repo_canon = main_repo_path.canonicalize().unwrap();

        // For bare repo worktree: mount the entire repo root
        assert_eq!(
            mount_path_canon, main_repo_canon,
            "Should mount the bare repo root, not just the worktree"
        );

        // Container path should be /workspace/{repo_name}
        let repo_name = main_repo_path.file_name().unwrap().to_string_lossy();
        assert_eq!(
            volumes[0].container_path,
            format!("/workspace/{}", repo_name),
            "Container mount path should be /workspace/{{repo_name}}"
        );

        // Working dir should point to the worktree within the mount
        assert!(
            working_dir.starts_with(&format!("/workspace/{}", repo_name)),
            "Working dir should be under /workspace/{{repo_name}}"
        );
        assert!(
            working_dir.ends_with("/main"),
            "Working dir should end with worktree name 'main', got: {}",
            working_dir
        );
    }

    #[test]
    fn test_compute_volume_paths_non_bare_repo_worktree() {
        let (_dir, repo_path) = setup_regular_repo();

        // Create a worktree from the regular (non-bare) repo
        let worktree_path = repo_path.parent().unwrap().join("my-worktree");
        let head = git2::Repository::open(&repo_path)
            .unwrap()
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        let repo = git2::Repository::open(&repo_path).unwrap();
        repo.branch("wt-branch", &repo.find_commit(head).unwrap(), false)
            .unwrap();
        drop(repo);

        let output = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt-branch",
            ])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        if !output.status.success() {
            // git not available, skip
            return;
        }

        let project_path_str = worktree_path.to_str().unwrap();

        let (volumes, working_dir) =
            compute_volume_paths(&worktree_path, project_path_str).unwrap();

        // For non-bare sibling worktrees: mount the main repo and worktree separately
        // as flat siblings under /workspace/.
        assert_eq!(
            volumes.len(),
            2,
            "Should have two volumes: main repo and worktree"
        );

        // First volume: the main repo
        let repo_canon = repo_path.canonicalize().unwrap();
        let mount0_canon = Path::new(&volumes[0].host_path).canonicalize().unwrap();
        assert_eq!(
            mount0_canon, repo_canon,
            "First volume should mount the main repo"
        );
        let repo_name = repo_canon.file_name().unwrap().to_string_lossy();
        assert_eq!(
            volumes[0].container_path,
            format!("/workspace/{}", repo_name),
        );

        // Second volume: the worktree
        let wt_canon = worktree_path.canonicalize().unwrap();
        let mount1_canon = Path::new(&volumes[1].host_path).canonicalize().unwrap();
        assert_eq!(
            mount1_canon, wt_canon,
            "Second volume should mount the worktree"
        );
        assert_eq!(volumes[1].container_path, "/workspace/my-worktree");

        // Working dir should point to the worktree
        assert_eq!(
            working_dir, "/workspace/my-worktree",
            "Working dir should be the worktree container path"
        );
    }

    #[test]
    fn test_compute_volume_paths_bare_repo_root() {
        let (_dir, main_repo_path, _worktree_path) = setup_bare_repo_with_worktree();

        let project_path_str = main_repo_path.to_str().unwrap();

        let (volumes, working_dir) =
            compute_volume_paths(&main_repo_path, project_path_str).unwrap();

        assert_eq!(volumes.len(), 1);

        // When at repo root, mount path equals project path
        let mount_canon = Path::new(&volumes[0].host_path).canonicalize().unwrap();
        let main_canon = main_repo_path.canonicalize().unwrap();
        assert_eq!(mount_canon, main_canon);

        // Working dir should be set
        assert!(!working_dir.is_empty());
    }

    #[test]
    fn test_compute_volume_paths_subdir_of_ancestor_repo_not_mounted() {
        // Simulates the scenario from GitHub issue #375: a user has a git repo at
        // their home directory (e.g., for dotfile management) and sets their project
        // path to a non-git subdirectory like ~/playground. Without the guard,
        // git2::Repository::discover walks up and finds the ancestor repo, causing
        // the entire parent (home directory) to be mounted into the container.
        let dir = TempDir::new().unwrap();

        // Create a git repo at the "parent" (simulating ~/  with dotfile management)
        let _repo = git2::Repository::init(dir.path()).unwrap();

        // Create a subdirectory that is NOT its own git repo (simulating ~/playground)
        let subdir = dir.path().join("playground");
        fs::create_dir_all(&subdir).unwrap();

        let project_path_str = subdir.to_str().unwrap();

        let (volumes, working_dir) = compute_volume_paths(&subdir, project_path_str).unwrap();

        assert_eq!(volumes.len(), 1);
        // The subdirectory should be mounted directly, NOT the parent repo
        assert_eq!(
            volumes[0].host_path,
            subdir.to_string_lossy().to_string(),
            "Should mount the subdirectory itself, not the ancestor git repo"
        );
        assert_eq!(volumes[0].container_path, working_dir);
        assert_eq!(volumes[0].container_path, "/workspace/playground");
    }

    #[test]
    fn test_common_ancestor() {
        assert_eq!(
            common_ancestor(Path::new("/a/b/c"), Path::new("/a/b/d")),
            PathBuf::from("/a/b")
        );
        assert_eq!(
            common_ancestor(Path::new("/a/b"), Path::new("/a/b")),
            PathBuf::from("/a/b")
        );
        assert_eq!(
            common_ancestor(Path::new("/a/b/c"), Path::new("/x/y/z")),
            PathBuf::from("/")
        );
    }

    #[test]
    fn test_compute_volume_paths_non_bare_worktree_nested_layout() {
        // Simulates a host layout where the worktree is nested deeper than the
        // main repo relative to their common ancestor (e.g., repo at
        // /scm/my-repo and worktree at /scm/worktrees/my-repo/1).
        let dir = TempDir::new().unwrap();
        let repo_path = dir.path().join("my-repo");
        fs::create_dir_all(&repo_path).unwrap();
        let repo = git2::Repository::init(&repo_path).unwrap();
        {
            let mut index = repo.index().unwrap();
            let oid = index.write_tree().unwrap();
            let sig = git2::Signature::now("test", "test@test.com").unwrap();
            let tree = repo.find_tree(oid).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        }

        let worktrees_dir = dir.path().join("worktrees").join("my-repo");
        fs::create_dir_all(&worktrees_dir).unwrap();
        let worktree_path = worktrees_dir.join("1");

        let head = repo.head().unwrap().peel_to_commit().unwrap().id();
        repo.branch("wt-branch", &repo.find_commit(head).unwrap(), false)
            .unwrap();
        drop(repo);

        let output = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt-branch",
            ])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        if !output.status.success() {
            return;
        }

        // AoE's create_worktree converts .git to relative paths via
        // convert_git_file_to_relative. Replicate that here since we
        // called git directly.
        let git_file = worktree_path.join(".git");
        let content = fs::read_to_string(&git_file).unwrap();
        let abs_path = content
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
            .unwrap();
        if Path::new(abs_path).is_absolute() {
            let wt_canon = worktree_path.canonicalize().unwrap();
            let gitdir_canon = Path::new(abs_path).canonicalize().unwrap();
            if let Some(rel) = crate::git::GitWorktree::diff_paths(&gitdir_canon, &wt_canon) {
                fs::write(&git_file, format!("gitdir: {}\n", rel.display())).unwrap();
            }
        }

        let project_path_str = worktree_path.to_str().unwrap();
        let (volumes, working_dir) =
            compute_volume_paths(&worktree_path, project_path_str).unwrap();

        assert_eq!(volumes.len(), 2);

        // The container paths must preserve relative depth so the .git file's
        // relative gitdir path resolves correctly.
        let repo_canon = repo_path.canonicalize().unwrap();
        let wt_canon = worktree_path.canonicalize().unwrap();
        let common = common_ancestor(&repo_canon, &wt_canon);
        let expected_repo = format!(
            "/workspace/{}",
            repo_canon.strip_prefix(&common).unwrap().display()
        );
        let expected_wt = format!(
            "/workspace/{}",
            wt_canon.strip_prefix(&common).unwrap().display()
        );

        assert_eq!(volumes[0].container_path, expected_repo);
        assert_eq!(volumes[1].container_path, expected_wt);
        assert_eq!(working_dir, expected_wt);

        // Verify the .git file's relative path resolves correctly in the
        // container layout.
        let content = fs::read_to_string(&git_file).unwrap();
        let gitdir_rel = content
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
            .unwrap();

        let resolved = PathBuf::from(&working_dir).join(gitdir_rel);

        // Normalize the path (resolve .. components)
        let mut normalized = Vec::new();
        for component in resolved.components() {
            match component {
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                c => normalized.push(c.as_os_str().to_owned()),
            }
        }
        let normalized: PathBuf = normalized.iter().collect();

        // Should land inside the main repo's .git/worktrees/ directory
        assert!(
            normalized
                .to_string_lossy()
                .starts_with(&volumes[0].container_path),
            "Resolved gitdir path '{}' should start with main repo container path '{}'",
            normalized.display(),
            volumes[0].container_path
        );
    }

    // --- sandbox config tests ---

    fn setup_host_dir(dir: &TempDir) -> std::path::PathBuf {
        let host = dir.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("auth.json"), r#"{"token":"abc"}"#).unwrap();
        fs::write(host.join("settings.json"), "{}").unwrap();
        fs::create_dir_all(host.join("subdir")).unwrap();
        fs::write(host.join("subdir").join("nested.txt"), "nested").unwrap();
        host
    }

    #[test]
    fn test_copies_top_level_files_only() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();

        assert!(sandbox.join("auth.json").exists());
        assert!(sandbox.join("settings.json").exists());
        assert!(!sandbox.join("subdir").exists());
    }

    #[test]
    fn test_skips_entries_in_skip_list() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        sync_agent_config(&host, &sandbox, &["auth.json"], &[], &[], &[]).unwrap();

        assert!(!sandbox.join("auth.json").exists());
        assert!(sandbox.join("settings.json").exists());
    }

    #[test]
    fn test_hermes_mount_skips_runtime_dirs() {
        let dir = TempDir::new().unwrap();
        let host = dir.path().join(".hermes");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("config.yaml"), "model: claude-opus\n").unwrap();
        fs::write(host.join(".env"), "API_KEY=token\n").unwrap();

        let runtime_dirs = [
            "sandbox",
            "sessions",
            "logs",
            "cache",
            "pastes",
            "images",
            "chrome-debug",
            "tmp",
        ];
        for runtime_dir in runtime_dirs {
            fs::create_dir_all(host.join(runtime_dir)).unwrap();
            fs::write(host.join(runtime_dir).join("runtime.txt"), "runtime").unwrap();
        }
        fs::write(host.join("state.db"), "sqlite-bytes").unwrap();

        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|m| m.tool_name == "hermes")
            .unwrap();
        let sandbox = prepare_sandbox_dir(mount, dir.path()).unwrap();

        assert!(sandbox.join("config.yaml").exists());
        assert!(sandbox.join(".env").exists());

        for runtime_dir in runtime_dirs {
            assert!(
                !sandbox.join(runtime_dir).exists(),
                "{} should be skipped",
                runtime_dir
            );
        }
        assert!(!sandbox.join("state.db").exists());
    }

    #[test]
    fn test_writes_seed_files_when_missing() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        let seeds = [("seed.json", r#"{"seeded":true}"#)];
        sync_agent_config(&host, &sandbox, &[], &seeds, &[], &[]).unwrap();

        let content = fs::read_to_string(sandbox.join("seed.json")).unwrap();
        assert_eq!(content, r#"{"seeded":true}"#);
    }

    #[test]
    fn test_seed_files_not_overwritten_if_exist() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        // First sync writes the seed.
        let seeds = [("seed.json", r#"{"seeded":true}"#)];
        sync_agent_config(&host, &sandbox, &[], &seeds, &[], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("seed.json")).unwrap(),
            r#"{"seeded":true}"#
        );

        // Container modifies the seed file.
        fs::write(sandbox.join("seed.json"), r#"{"modified":true}"#).unwrap();

        // Re-sync should NOT overwrite the container's changes.
        sync_agent_config(&host, &sandbox, &[], &seeds, &[], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("seed.json")).unwrap(),
            r#"{"modified":true}"#
        );
    }

    #[test]
    fn test_host_files_overwrite_seeds() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        // Seed has the same name as a host file -- host copy wins.
        let seeds = [("auth.json", "seed-content")];
        sync_agent_config(&host, &sandbox, &[], &seeds, &[], &[]).unwrap();

        let content = fs::read_to_string(sandbox.join("auth.json")).unwrap();
        assert_eq!(content, r#"{"token":"abc"}"#);
    }

    #[test]
    fn test_seed_survives_when_no_host_equivalent() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        let seeds = [(".claude.json", r#"{"hasCompletedOnboarding":true}"#)];
        sync_agent_config(&host, &sandbox, &[], &seeds, &[], &[]).unwrap();

        let content = fs::read_to_string(sandbox.join(".claude.json")).unwrap();
        assert_eq!(content, r#"{"hasCompletedOnboarding":true}"#);
    }

    #[test]
    fn test_creates_sandbox_dir_if_missing() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("deep").join("nested").join("sandbox");

        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();

        assert!(sandbox.exists());
        assert!(sandbox.join("auth.json").exists());
    }

    #[test]
    fn test_rewrites_claude_plugin_paths_to_container_home() {
        let dir = TempDir::new().unwrap();
        let host_home = dir.path().join("home");
        let host = host_home.join(".claude");
        fs::create_dir_all(host.join("plugins")).unwrap();

        let known = format!(
            r#"{{"installLocation":"{}/.claude/plugins/marketplaces/claude-plugins-official"}}"#,
            host_home.display()
        );
        fs::write(host.join("plugins/known_marketplaces.json"), known).unwrap();

        let installed = format!(
            r#"{{"rust-analyzer-lsp":{{"installPath":"{}/.claude/plugins/cache/claude-plugins-official/rust-analyzer-lsp/1.0.0"}}}}"#,
            host_home.display()
        );
        fs::write(host.join("plugins/installed_plugins.json"), installed).unwrap();

        let sandbox = dir.path().join("sandbox");
        sync_agent_config(&host, &sandbox, &[], &[], &["plugins"], &[]).unwrap();
        rewrite_claude_plugin_paths(&sandbox, &host_home).unwrap();

        let host_prefix = host_home.to_string_lossy();
        let known_out =
            fs::read_to_string(sandbox.join("plugins/known_marketplaces.json")).unwrap();
        assert!(known_out.contains("/root/.claude/plugins/marketplaces/claude-plugins-official"));
        assert!(!known_out.contains(host_prefix.as_ref()));

        let installed_out =
            fs::read_to_string(sandbox.join("plugins/installed_plugins.json")).unwrap();
        assert!(installed_out.contains(
            "/root/.claude/plugins/cache/claude-plugins-official/rust-analyzer-lsp/1.0.0"
        ));
        assert!(!installed_out.contains(host_prefix.as_ref()));
    }

    #[test]
    fn test_agent_config_mounts_have_valid_entries() {
        for mount in AGENT_CONFIG_MOUNTS {
            assert!(!mount.tool_name.is_empty());
            assert!(!mount.host_rel.is_empty());
            assert!(!mount.container_suffix.is_empty());
        }
    }

    #[test]
    fn test_agent_config_mounts_each_tool_has_expected_count() {
        let tool_names: Vec<&str> = AGENT_CONFIG_MOUNTS.iter().map(|m| m.tool_name).collect();
        for name in &tool_names {
            let count = tool_names.iter().filter(|n| *n == name).count();
            // OpenCode has two mounts: data dir (.local/share/opencode) + config dir (.config/opencode)
            let expected = if *name == "opencode" { 2 } else { 1 };
            assert_eq!(
                count, expected,
                "tool_name '{}' appears {} times, expected {}",
                name, count, expected
            );
        }
    }

    #[test]
    fn test_agent_config_mounts_filter_by_tool() {
        let claude_mounts: Vec<_> = AGENT_CONFIG_MOUNTS
            .iter()
            .filter(|m| m.tool_name == "claude")
            .collect();
        assert_eq!(claude_mounts.len(), 1);
        assert_eq!(claude_mounts[0].host_rel, ".claude");

        // OpenCode has both a data dir and a config dir mount
        let opencode_mounts: Vec<_> = AGENT_CONFIG_MOUNTS
            .iter()
            .filter(|m| m.tool_name == "opencode")
            .collect();
        assert_eq!(opencode_mounts.len(), 2);
        let opencode_paths: Vec<&str> = opencode_mounts.iter().map(|m| m.host_rel).collect();
        assert!(opencode_paths.contains(&".local/share/opencode"));
        assert!(opencode_paths.contains(&".config/opencode"));

        let cursor_mounts: Vec<_> = AGENT_CONFIG_MOUNTS
            .iter()
            .filter(|m| m.tool_name == "cursor")
            .collect();
        assert_eq!(cursor_mounts.len(), 1);
        assert_eq!(cursor_mounts[0].host_rel, ".cursor");

        let hermes_mounts: Vec<_> = AGENT_CONFIG_MOUNTS
            .iter()
            .filter(|m| m.tool_name == "hermes")
            .collect();
        assert_eq!(hermes_mounts.len(), 1);
        assert_eq!(hermes_mounts[0].host_rel, ".hermes");

        let antigravity_mounts: Vec<_> = AGENT_CONFIG_MOUNTS
            .iter()
            .filter(|m| m.tool_name == "antigravity")
            .collect();
        assert_eq!(antigravity_mounts.len(), 1);
        assert_eq!(antigravity_mounts[0].host_rel, ".gemini/antigravity-cli");
        assert_eq!(
            antigravity_mounts[0].container_suffix,
            ".gemini/antigravity-cli"
        );

        // Unknown tool should match nothing
        let unknown_mounts: Vec<_> = AGENT_CONFIG_MOUNTS
            .iter()
            .filter(|m| m.tool_name == "unknown")
            .collect();
        assert_eq!(unknown_mounts.len(), 0);
    }

    #[test]
    fn test_agent_config_mounts_match_agent_registry() {
        // Every mount should correspond to a registered agent
        for mount in AGENT_CONFIG_MOUNTS {
            assert!(
                crate::agents::get_agent(mount.tool_name).is_some(),
                "AGENT_CONFIG_MOUNTS entry '{}' has no matching agent in the registry",
                mount.tool_name
            );
        }
    }

    #[test]
    fn test_sandbox_gitconfig_seed_is_valid_gitconfig() {
        let dir = TempDir::new().unwrap();
        let gitconfig = dir.path().join("gitconfig");
        fs::write(&gitconfig, SANDBOX_GITCONFIG_SEED).unwrap();

        let out = std::process::Command::new("git")
            .args([
                "config",
                "--file",
                gitconfig.to_str().unwrap(),
                "--get",
                "credential.https://github.com.helper",
            ])
            .output();
        let Ok(out) = out else {
            eprintln!("skipping: git not available");
            return;
        };
        assert!(
            out.status.success(),
            "git failed to parse seeded gitconfig: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let helper = String::from_utf8_lossy(&out.stdout);
        assert!(helper.starts_with('!'), "helper must be a shell snippet");
        assert!(
            helper.contains("$GH_TOKEN"),
            "helper must read GH_TOKEN at runtime"
        );
    }

    #[test]
    fn test_home_seed_files_written_to_sandbox_root() {
        let dir = TempDir::new().unwrap();
        let sandbox_base = dir.path().join("sandbox-root");
        fs::create_dir_all(&sandbox_base).unwrap();

        let home_seeds: &[(&str, &str)] = &[(".claude.json", r#"{"hasCompletedOnboarding":true}"#)];

        for &(filename, content) in home_seeds {
            let path = sandbox_base.join(filename);
            if !path.exists() {
                fs::write(path, content).unwrap();
            }
        }

        let written = fs::read_to_string(sandbox_base.join(".claude.json")).unwrap();
        assert_eq!(written, r#"{"hasCompletedOnboarding":true}"#);

        // Verify it's NOT inside an agent config subdirectory.
        assert!(!sandbox_base.join(".claude").join(".claude.json").exists());
    }

    #[test]
    fn test_home_seed_files_not_overwritten_if_exist() {
        let dir = TempDir::new().unwrap();
        let sandbox_base = dir.path().join("sandbox-root");
        fs::create_dir_all(&sandbox_base).unwrap();

        // First write.
        let path = sandbox_base.join(".claude.json");
        fs::write(&path, r#"{"hasCompletedOnboarding":true}"#).unwrap();

        // Container modifies it.
        fs::write(&path, r#"{"hasCompletedOnboarding":true,"extra":"data"}"#).unwrap();

        // Write-once logic should not overwrite.
        if !path.exists() {
            fs::write(&path, r#"{"hasCompletedOnboarding":true}"#).unwrap();
        }

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, r#"{"hasCompletedOnboarding":true,"extra":"data"}"#);
    }

    #[test]
    fn test_refresh_updates_changed_host_files() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("auth.json")).unwrap(),
            r#"{"token":"abc"}"#
        );

        // Host file changes between sessions.
        fs::write(host.join("auth.json"), r#"{"token":"refreshed"}"#).unwrap();

        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("auth.json")).unwrap(),
            r#"{"token":"refreshed"}"#
        );
    }

    #[test]
    fn test_refresh_picks_up_new_host_files() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();
        assert!(!sandbox.join("new_cred.json").exists());

        // New credential file appears on host.
        fs::write(host.join("new_cred.json"), "new").unwrap();

        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("new_cred.json")).unwrap(),
            "new"
        );
    }

    #[test]
    fn test_refresh_preserves_container_written_files() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();

        // Container writes a runtime file into the sandbox dir.
        fs::write(sandbox.join("runtime.log"), "container-state").unwrap();

        // Refresh from host.
        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();

        // Container-written file survives (host has no file with that name).
        assert_eq!(
            fs::read_to_string(sandbox.join("runtime.log")).unwrap(),
            "container-state"
        );
    }

    #[test]
    fn test_copies_listed_dirs_recursively() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);

        // Create a "plugins" dir with nested content.
        let plugins = host.join("plugins");
        fs::create_dir_all(plugins.join("lsp")).unwrap();
        fs::write(plugins.join("config.json"), "{}").unwrap();
        fs::write(plugins.join("lsp").join("gopls.wasm"), "binary").unwrap();

        let sandbox = dir.path().join("sandbox");
        sync_agent_config(&host, &sandbox, &[], &[], &["plugins"], &[]).unwrap();

        assert!(sandbox.join("plugins").join("config.json").exists());
        assert!(sandbox
            .join("plugins")
            .join("lsp")
            .join("gopls.wasm")
            .exists());
        // "subdir" is NOT in copy_dirs, so still skipped.
        assert!(!sandbox.join("subdir").exists());
    }

    #[test]
    fn test_rewrite_claude_plugin_paths() {
        let dir = TempDir::new().unwrap();
        let host_home = dir.path().join("home");
        fs::create_dir_all(&host_home).unwrap();

        let sandbox = dir.path().join("sandbox");
        let marketplaces = sandbox.join("plugins").join("marketplaces");
        fs::create_dir_all(&marketplaces).unwrap();

        let host_marketplace = format!(
            "{}/.claude/plugins/marketplaces/claude-plugins-official",
            host_home.display()
        );
        let known = format!(
            r#"{{"marketplaces":[{{"installLocation":"{}"}}]}}"#,
            host_marketplace
        );
        fs::write(marketplaces.join("known_marketplaces.json"), known).unwrap();

        let host_install = format!(
            "{}/.claude/plugins/cache/claude-plugins-official/rust-analyzer-lsp/1.0.0",
            host_home.display()
        );
        let installed = format!(r#"{{"plugins":[{{"installPath":"{}"}}]}}"#, host_install);
        fs::write(
            sandbox.join("plugins").join("installed_plugins.json"),
            installed,
        )
        .unwrap();

        rewrite_claude_plugin_paths(&sandbox, &host_home).unwrap();

        let known = fs::read_to_string(marketplaces.join("known_marketplaces.json")).unwrap();
        let known_json: serde_json::Value = serde_json::from_str(&known).unwrap();
        assert_eq!(
            known_json["marketplaces"][0]["installLocation"],
            "/root/.claude/plugins/marketplaces/claude-plugins-official"
        );

        let installed =
            fs::read_to_string(sandbox.join("plugins").join("installed_plugins.json")).unwrap();
        let installed_json: serde_json::Value = serde_json::from_str(&installed).unwrap();
        assert_eq!(
            installed_json["plugins"][0]["installPath"],
            "/root/.claude/plugins/cache/claude-plugins-official/rust-analyzer-lsp/1.0.0"
        );
    }

    #[test]
    fn test_unlisted_dirs_still_skipped() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);

        // "subdir" exists from setup_host_dir but is not in copy_dirs.
        let sandbox = dir.path().join("sandbox");
        sync_agent_config(&host, &sandbox, &[], &[], &["nonexistent"], &[]).unwrap();

        assert!(!sandbox.join("subdir").exists());
        assert!(sandbox.join("auth.json").exists());
    }

    #[test]
    fn test_copy_dir_recursive() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("a").join("b")).unwrap();
        fs::write(src.join("root.txt"), "root").unwrap();
        fs::write(src.join("a").join("mid.txt"), "mid").unwrap();
        fs::write(src.join("a").join("b").join("deep.txt"), "deep").unwrap();

        let dest = dir.path().join("dest");
        copy_dir_recursive(&src, &dest).unwrap();

        assert_eq!(fs::read_to_string(dest.join("root.txt")).unwrap(), "root");
        assert_eq!(
            fs::read_to_string(dest.join("a").join("mid.txt")).unwrap(),
            "mid"
        );
        assert_eq!(
            fs::read_to_string(dest.join("a").join("b").join("deep.txt")).unwrap(),
            "deep"
        );
    }

    #[test]
    fn test_symlinked_dirs_are_followed() {
        let dir = TempDir::new().unwrap();
        let host = dir.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("config.json"), "{}").unwrap();

        // Create a real dir with content, then symlink to it from copy_dirs.
        let real_dir = dir.path().join("real-skills");
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(real_dir.join("skill.md"), "# Skill").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_dir, host.join("skills")).unwrap();

        let sandbox = dir.path().join("sandbox");
        sync_agent_config(&host, &sandbox, &[], &[], &["skills"], &[]).unwrap();

        assert!(sandbox.join("config.json").exists());
        #[cfg(unix)]
        {
            assert!(sandbox.join("skills").exists());
            assert_eq!(
                fs::read_to_string(sandbox.join("skills").join("skill.md")).unwrap(),
                "# Skill"
            );
        }
    }

    #[test]
    fn test_bad_entry_does_not_fail_sync() {
        let dir = TempDir::new().unwrap();
        let host = dir.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("good.json"), "ok").unwrap();

        // Create a symlink pointing to a nonexistent target.
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/path", host.join("broken-link")).unwrap();

        let sandbox = dir.path().join("sandbox");
        // Should succeed despite the broken symlink.
        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();

        assert_eq!(fs::read_to_string(sandbox.join("good.json")).unwrap(), "ok");
        // Broken symlink is skipped, not copied.
        assert!(!sandbox.join("broken-link").exists());
    }

    #[test]
    fn test_preserve_files_not_overwritten() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        // First sync seeds the preserved file from host.
        sync_agent_config(&host, &sandbox, &[], &[], &[], &["auth.json"]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("auth.json")).unwrap(),
            r#"{"token":"abc"}"#
        );

        // Simulate migration or in-container auth writing a different credential.
        fs::write(sandbox.join("auth.json"), r#"{"token":"container"}"#).unwrap();

        // Host file changes.
        fs::write(host.join("auth.json"), r#"{"token":"refreshed"}"#).unwrap();

        // Re-sync should NOT overwrite the preserved file.
        sync_agent_config(&host, &sandbox, &[], &[], &[], &["auth.json"]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("auth.json")).unwrap(),
            r#"{"token":"container"}"#
        );

        // Non-preserved files are still overwritten.
        fs::write(host.join("settings.json"), "updated").unwrap();
        sync_agent_config(&host, &sandbox, &[], &[], &[], &["auth.json"]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("settings.json")).unwrap(),
            "updated"
        );
    }

    #[test]
    fn test_history_preserved_across_resync() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        // Host has a history file with host-only entries.
        fs::write(host.join("history.jsonl"), "host-entry\n").unwrap();

        // First sync copies it in.
        sync_agent_config(&host, &sandbox, &[], &[], &[], &["history.jsonl"]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("history.jsonl")).unwrap(),
            "host-entry\n"
        );

        // Container session appends entries.
        fs::write(
            sandbox.join("history.jsonl"),
            "host-entry\ncontainer-session-1\ncontainer-session-2\n",
        )
        .unwrap();

        // Re-sync (container restart) should NOT clobber the container's history.
        sync_agent_config(&host, &sandbox, &[], &[], &[], &["history.jsonl"]).unwrap();
        let content = fs::read_to_string(sandbox.join("history.jsonl")).unwrap();
        assert!(
            content.contains("container-session-1"),
            "container history entries must survive re-sync"
        );
    }

    #[test]
    fn test_has_prior_data_skips_general_file_copy() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        // First sync copies everything in.
        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("settings.json")).unwrap(),
            "{}"
        );

        // Simulate a prior container session by creating the "projects/" sentinel.
        fs::create_dir_all(sandbox.join("projects")).unwrap();

        // Container modifies settings.json during its session.
        fs::write(sandbox.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();

        // Host updates settings.json independently.
        fs::write(host.join("settings.json"), r#"{"theme":"light"}"#).unwrap();

        // Re-sync should skip general file copies because projects/ exists,
        // preserving the container's settings.json.
        sync_agent_config(&host, &sandbox, &[], &[], &[], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("settings.json")).unwrap(),
            r#"{"theme":"dark"}"#,
            "container-side settings must not be overwritten when projects/ sentinel exists"
        );
    }

    #[test]
    fn test_is_fatal_copy_error_discriminates_storage_exhaustion() {
        use std::io::Error;
        // ENOSPC / EROFS / EDQUOT mean the destination can't accept writes;
        // continuing would silently produce a partial copy, so these abort.
        assert!(is_fatal_copy_error(&Error::from_raw_os_error(28)), "ENOSPC");
        assert!(is_fatal_copy_error(&Error::from_raw_os_error(30)), "EROFS");
        assert!(
            is_fatal_copy_error(&Error::from_raw_os_error(69)),
            "EDQUOT (macOS)"
        );
        assert!(
            is_fatal_copy_error(&Error::from_raw_os_error(122)),
            "EDQUOT (Linux)"
        );
        // A single unreadable / missing source entry is best-effort skippable.
        assert!(
            !is_fatal_copy_error(&Error::from_raw_os_error(13)),
            "EACCES"
        );
        assert!(!is_fatal_copy_error(&Error::from_raw_os_error(2)), "ENOENT");
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_dir_recursive_terminates_on_symlink_cycle() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("sub").join("file.txt"), "data").unwrap();
        // Cycle: src/sub/loop -> src. The old code followed it and recursed
        // forever; the visited-set guard must break it.
        std::os::unix::fs::symlink(&src, src.join("sub").join("loop")).unwrap();

        let dest = dir.path().join("dest");
        // Must return rather than infinite-loop / stack-overflow.
        copy_dir_recursive(&src, &dest).unwrap();
        assert_eq!(
            fs::read_to_string(dest.join("sub").join("file.txt")).unwrap(),
            "data"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_dir_recursive_skips_bad_entry_inside_subdir() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("good.txt"), "good").unwrap();
        // Dangling symlink inside the copied tree. The old code propagated the
        // stat error with `?` and aborted the whole copy (the log's "Failed to
        // copy dir plugins: Permission denied"); now a single bad entry is
        // skipped and the rest still copies.
        std::os::unix::fs::symlink("/nonexistent/target", src.join("dangling")).unwrap();

        let dest = dir.path().join("dest");
        copy_dir_recursive(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("good.txt")).unwrap(), "good");
        assert!(!dest.join("dangling").exists());
    }

    #[test]
    fn test_copy_dirs_skipped_when_prior_data() {
        let dir = TempDir::new().unwrap();
        let host = dir.path().join("host");
        fs::create_dir_all(host.join("plugins")).unwrap();
        fs::write(host.join("plugins").join("p.txt"), "host-plugin").unwrap();
        let sandbox = dir.path().join("sandbox");

        // Prior container session sentinel.
        fs::create_dir_all(sandbox.join("projects")).unwrap();

        sync_agent_config(&host, &sandbox, &[], &[], &["plugins"], &[]).unwrap();
        assert!(
            !sandbox.join("plugins").exists(),
            "copy_dirs must be skipped once the sandbox has prior session data, \
             so a restart no longer re-copies the whole plugins tree"
        );
    }

    #[test]
    fn test_preserve_files_seeded_when_missing() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        // Preserved file is copied when sandbox doesn't have it yet.
        sync_agent_config(&host, &sandbox, &[], &[], &[], &["auth.json"]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("auth.json")).unwrap(),
            r#"{"token":"abc"}"#
        );
    }

    // --- credential freshness tests ---

    #[test]
    fn test_parse_credential_expires_at_valid() {
        let json = r#"{"claudeAiOauth":{"expiresAt":1700000000}}"#;
        assert_eq!(parse_credential_expires_at(json), Some(1700000000));
    }

    #[test]
    fn test_parse_credential_expires_at_missing_key() {
        // Missing claudeAiOauth entirely.
        assert_eq!(parse_credential_expires_at(r#"{"other":"data"}"#), None);
        // Missing expiresAt inside claudeAiOauth.
        assert_eq!(
            parse_credential_expires_at(r#"{"claudeAiOauth":{"token":"abc"}}"#),
            None
        );
    }

    #[test]
    fn test_parse_credential_expires_at_invalid_json() {
        assert_eq!(parse_credential_expires_at("not json at all"), None);
        assert_eq!(parse_credential_expires_at(""), None);
    }

    #[test]
    fn test_parse_credential_expires_at_wrong_type() {
        // expiresAt is a string instead of a number.
        let json = r#"{"claudeAiOauth":{"expiresAt":"1700000000"}}"#;
        assert_eq!(parse_credential_expires_at(json), None);
    }

    #[test]
    fn test_should_not_overwrite_with_stale_keychain() {
        let sandbox = r#"{"claudeAiOauth":{"expiresAt":2000}}"#;
        let keychain = r#"{"claudeAiOauth":{"expiresAt":1000}}"#;
        assert!(!should_overwrite_credential(sandbox, keychain));
    }

    #[test]
    fn test_should_overwrite_with_fresh_keychain() {
        let sandbox = r#"{"claudeAiOauth":{"expiresAt":1000}}"#;
        let keychain = r#"{"claudeAiOauth":{"expiresAt":2000}}"#;
        assert!(should_overwrite_credential(sandbox, keychain));
    }

    #[test]
    fn test_should_not_overwrite_equal_timestamps() {
        let cred = r#"{"claudeAiOauth":{"expiresAt":1000}}"#;
        assert!(!should_overwrite_credential(cred, cred));
    }

    #[test]
    fn test_should_not_overwrite_when_keychain_unparseable() {
        let sandbox = r#"{"claudeAiOauth":{"expiresAt":1000}}"#;
        assert!(!should_overwrite_credential(sandbox, "not-json"));
    }

    #[test]
    fn test_should_overwrite_when_both_unparseable() {
        assert!(should_overwrite_credential("bad", "also-bad"));
    }

    #[test]
    fn test_should_overwrite_when_only_keychain_parseable() {
        let keychain = r#"{"claudeAiOauth":{"expiresAt":1000}}"#;
        assert!(should_overwrite_credential("not-json", keychain));
    }

    /// End-to-end test: repo-level sandbox config (environment, volume_ignores,
    /// extra_volumes) flows through build_container_config into the final ContainerConfig.
    /// Regression test for #557.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_includes_repo_sandbox_settings() {
        // Isolate HOME so global/profile config doesn't interfere
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        // Create a project directory with repo config
        let project_dir = TempDir::new().unwrap();
        let config_dir = project_dir.path().join(".agent-of-empires");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            r#"
[sandbox]
environment = ["MY_VAR=hello", "CI=true"]
volume_ignores = [".venv", "node_modules"]
extra_volumes = ["/host/data:/container/data:ro"]
"#,
        )
        .unwrap();

        // Initialize a git repo so compute_volume_paths works
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };

        let project_path_str = project_dir.path().to_str().unwrap();
        let config = build_container_config(
            project_path_str,
            &sandbox_info,
            ContainerAgentSelection::new("claude", None),
            false,
            "test-instance-id",
            None,
            "",
        )
        .unwrap();

        // Verify environment variables from repo config are present
        let env_keys: Vec<&str> = config.environment.iter().map(|e| e.key()).collect();
        assert!(
            env_keys.contains(&"MY_VAR"),
            "MY_VAR should be in environment, got: {:?}",
            config.environment
        );
        assert!(
            env_keys.contains(&"CI"),
            "CI should be in environment, got: {:?}",
            config.environment
        );

        // Verify volume_ignores became anonymous volumes
        let dir_name = project_dir.path().file_name().unwrap().to_string_lossy();
        let expected_venv = format!("/workspace/{}/.venv", dir_name);
        let expected_node = format!("/workspace/{}/node_modules", dir_name);
        assert!(
            config.anonymous_volumes.contains(&expected_venv),
            "anonymous_volumes should contain .venv path, got: {:?}",
            config.anonymous_volumes
        );
        assert!(
            config.anonymous_volumes.contains(&expected_node),
            "anonymous_volumes should contain node_modules path, got: {:?}",
            config.anonymous_volumes
        );

        // Verify extra_volumes from repo config are present
        let volume_pairs: Vec<(&str, &str)> = config
            .volumes
            .iter()
            .map(|v| (v.host_path.as_str(), v.container_path.as_str()))
            .collect();
        assert!(
            volume_pairs.contains(&("/host/data", "/container/data")),
            "extra_volumes should include /host/data:/container/data, got: {:?}",
            volume_pairs
        );
    }

    #[test]
    fn test_has_glob_metachars() {
        assert!(has_glob_metachars("**/bin"));
        assert!(has_glob_metachars("**/obj/"));
        assert!(has_glob_metachars("target/*"));
        assert!(has_glob_metachars("build?"));
        assert!(has_glob_metachars("cache[0-9]"));
        assert!(!has_glob_metachars("target"));
        assert!(!has_glob_metachars("node_modules"));
        assert!(!has_glob_metachars("src/bin"));
        assert!(!has_glob_metachars(".venv"));
    }

    /// Feature test for #2045: glob volume_ignores entries are expanded against the
    /// live workspace at build time, emitting one mount per matched directory, while
    /// literal entries still mount unconditionally and no `*` ever reaches a mount
    /// path (the #2036 host-littering regression must not return).
    #[test]
    #[serial_test::serial]
    fn test_volume_ignores_expands_glob_entries() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let project_dir = TempDir::new().unwrap();
        // Nested generated dirs the .NET-style globs should find.
        fs::create_dir_all(project_dir.path().join("src/App/bin")).unwrap();
        fs::create_dir_all(project_dir.path().join("src/App/obj")).unwrap();
        fs::create_dir_all(project_dir.path().join("tests/Lib/bin")).unwrap();

        let config_dir = project_dir.path().join(".agent-of-empires");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            r#"
[sandbox]
volume_ignores = ["**/bin", "**/obj", "target"]
"#,
        )
        .unwrap();

        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };

        let project_path_str = project_dir.path().to_str().unwrap();
        let config = build_container_config(
            project_path_str,
            &sandbox_info,
            ContainerAgentSelection::new("claude", None),
            false,
            "test-instance-id",
            None,
            "",
        )
        .unwrap();

        let dir_name = project_dir.path().file_name().unwrap().to_string_lossy();
        let expect = |p: &str| format!("/workspace/{}/{}", dir_name, p);

        for matched in ["src/App/bin", "tests/Lib/bin", "src/App/obj"] {
            assert!(
                config.anonymous_volumes.contains(&expect(matched)),
                "glob should have expanded to {}, got: {:?}",
                matched,
                config.anonymous_volumes
            );
        }
        assert!(
            config.anonymous_volumes.contains(&expect("target")),
            "literal 'target' entry should still mount, got: {:?}",
            config.anonymous_volumes
        );
        assert!(
            !config
                .anonymous_volumes
                .iter()
                .any(|p| p.contains('*') || p.contains('?')),
            "no glob metachar may reach a mount path, got: {:?}",
            config.anonymous_volumes
        );
    }

    /// `preview_glob_volume_ignores` reports the same expansion the build performs,
    /// keeps a configured-but-unmatched pattern with an empty match list, and ignores
    /// literal entries entirely.
    #[test]
    #[serial_test::serial]
    fn test_preview_glob_volume_ignores() {
        let project_dir = TempDir::new().unwrap();
        fs::create_dir_all(project_dir.path().join("src/App/bin")).unwrap();
        // A file (not a dir) named like a match must be ignored.
        fs::write(project_dir.path().join("notes.bin"), "x").unwrap();
        git2::Repository::init(project_dir.path()).unwrap();
        let project_path_str = project_dir.path().to_str().unwrap();

        let ignores = vec![
            "**/bin".to_string(),
            "**/missing".to_string(),
            "target".to_string(),
        ];
        let expansions = preview_glob_volume_ignores(project_path_str, None, &ignores).unwrap();

        // Two glob entries (literal "target" excluded); order preserved.
        assert_eq!(expansions.len(), 2);
        assert_eq!(expansions[0].pattern, "**/bin");
        let dir_name = project_dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(
            expansions[0].matched_container_paths,
            vec![format!("/workspace/{}/src/App/bin", dir_name)],
            "only the directory should match, not notes.bin"
        );
        assert_eq!(expansions[1].pattern, "**/missing");
        assert!(
            expansions[1].matched_container_paths.is_empty(),
            "an unmatched glob is kept with no matches"
        );
    }

    #[test]
    fn test_preview_glob_volume_ignores_empty_without_globs() {
        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();
        let ignores = vec!["target".to_string(), ".venv".to_string()];
        let expansions =
            preview_glob_volume_ignores(project_dir.path().to_str().unwrap(), None, &ignores)
                .unwrap();
        assert!(expansions.is_empty());
    }

    /// Regression: when project_path is a sibling worktree, `.agent-of-empires/config.toml`
    /// lives in the main repo, not the worktree. `build_container_config` must
    /// resolve repo config from the main repo path so extra_volumes still mount.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_sibling_worktree_loads_main_repo_extra_volumes() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        // Main repo with repo config under .agent-of-empires/
        let parent = TempDir::new().unwrap();
        let main_repo = parent.path().join("main");
        fs::create_dir_all(&main_repo).unwrap();
        let repo = git2::Repository::init(&main_repo).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial", &tree, &[])
            .unwrap();

        let config_dir = main_repo.join(".agent-of-empires");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            r#"
[sandbox]
extra_volumes = ["/host/screenshots:/root/screenshots"]
"#,
        )
        .unwrap();

        // Sibling worktree under <parent>/worktrees/feat
        let worktree_path = parent.path().join("worktrees").join("feat");
        fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
        let out = std::process::Command::new("git")
            .args(["worktree", "add", worktree_path.to_str().unwrap(), "HEAD"])
            .current_dir(&main_repo)
            .output()
            .expect("git worktree add");
        if !out.status.success() {
            // git not available or worktree add failed; skip.
            return;
        }

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };

        let config = build_container_config(
            worktree_path.to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("claude", None),
            false,
            "test-instance-id",
            None,
            "",
        )
        .unwrap();

        let volume_pairs: Vec<(&str, &str)> = config
            .volumes
            .iter()
            .map(|v| (v.host_path.as_str(), v.container_path.as_str()))
            .collect();
        assert!(
            volume_pairs.contains(&("/host/screenshots", "/root/screenshots")),
            "extra_volumes from main-repo config should mount in sibling worktree session, got: {:?}",
            volume_pairs
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_installs_codex_hooks_files() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };
        let instance_id = "codex-sandbox-hooks-test";
        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("codex", None),
            false,
            instance_id,
            None,
            "",
        )
        .unwrap();

        let codex_sandbox = temp_home.path().join(".codex").join(SANDBOX_SUBDIR);
        assert!(codex_sandbox.join("config.toml").exists());
        assert!(!codex_sandbox.join("hooks.json").exists());
        assert!(!codex_sandbox.join("settings.json").exists());
        let codex_config = fs::read_to_string(codex_sandbox.join("config.toml")).unwrap();
        assert!(codex_config.contains("[[hooks.PreToolUse]]"));
        assert!(codex_config.contains("aoe-hooks"));
        assert!(config.volumes.iter().any(|v| {
            v.host_path == codex_sandbox.to_string_lossy() && v.container_path == "/root/.codex"
        }));

        let hook_dir =
            crate::hooks::hook_status_dir(instance_id).expect("test id must be allowlist-safe");
        assert!(
            config
                .volumes
                .iter()
                .any(|v| v.host_path == hook_dir.to_string_lossy()),
            "status hook directory should be mounted"
        );
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    // Regression guard for the trap in #958: a sidecar agent (settl TOML,
    // hermes YAML, kiro per-agent JSON) that lands without wiring up the
    // sandbox install branch silently breaks status detection in containers.
    // Driving every sandboxable sidecar agent through build_container_config
    // and asserting its config lands at `sandbox_config_subpath` (plus a
    // mounted hook dir) means a future agent that forgets to set
    // `sidecar_hooks` fails this test instead of shipping broken.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_installs_sidecar_hooks_files() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sidecar_agents: Vec<&crate::agents::AgentDef> = crate::agents::AGENTS
            .iter()
            .filter(|a| a.sidecar_hooks.is_some() && !a.host_only)
            .collect();
        assert!(
            sidecar_agents.iter().any(|a| a.name == "hermes")
                && sidecar_agents.iter().any(|a| a.name == "kiro"),
            "expected hermes and kiro to be sandboxable sidecar agents"
        );

        for agent in sidecar_agents {
            let sidecar = agent.sidecar_hooks.as_ref().unwrap();
            assert!(
                !sidecar.sandbox_config_subpath.is_empty(),
                "{} is sandboxable so it needs a sandbox_config_subpath",
                agent.name
            );

            let sandbox_info = super::super::instance::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "test-container".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
            };
            let instance_id = format!("{}-sidecar-sandbox-test", agent.name);
            let config = build_container_config(
                project_dir.path().to_str().unwrap(),
                &sandbox_info,
                ContainerAgentSelection::new(agent.name, None),
                false,
                &instance_id,
                None,
                "",
            )
            .unwrap();

            let sandbox_config = temp_home.path().join(sidecar.sandbox_config_subpath);
            assert!(
                sandbox_config.exists(),
                "{} sandbox hook config should be installed at {}",
                agent.name,
                sandbox_config.display()
            );
            let contents = fs::read_to_string(&sandbox_config).unwrap();
            assert!(
                contents.contains("aoe-hooks"),
                "{} sandbox config should contain the AoE hook marker",
                agent.name
            );

            let hook_dir = crate::hooks::hook_status_dir(&instance_id)
                .expect("test id must be allowlist-safe");
            assert!(
                config
                    .volumes
                    .iter()
                    .any(|v| v.host_path == hook_dir.to_string_lossy()),
                "{} should mount its status hook directory",
                agent.name
            );
            crate::hooks::cleanup_hook_status_dir(&instance_id);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_refuses_unsafe_instance_id() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };

        let result = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("codex", None),
            false,
            "../etc",
            None,
            "",
        );

        let err = match result {
            Ok(_) => panic!("must refuse unsafe instance id"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("AOE_INSTANCE_ID"),
            "error must surface validator failure, got: {msg}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_respects_profile_hooks_disabled() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let profile_dir = crate::session::get_profile_dir("sandbox-hooks-disabled").unwrap();
        fs::write(
            profile_dir.join("config.toml"),
            "[session]\nagent_status_hooks = false\n",
        )
        .unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };
        let instance_id = "codex-sandbox-hooks-disabled-test";
        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("codex", None),
            false,
            instance_id,
            None,
            "sandbox-hooks-disabled",
        )
        .unwrap();

        let codex_sandbox = temp_home.path().join(".codex").join(SANDBOX_SUBDIR);
        assert!(!codex_sandbox.join("config.toml").exists());

        let hook_dir =
            crate::hooks::hook_status_dir(instance_id).expect("test id must be allowlist-safe");
        assert!(
            !config
                .volumes
                .iter()
                .any(|v| v.host_path == hook_dir.to_string_lossy()),
            "status hook directory should not be mounted when profile disables hooks"
        );
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_uses_detected_codex_for_custom_wrapper_hooks() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let mut global = crate::session::config::Config::default();
        global.session.agent_status_hooks = false;
        crate::session::config::save_config(&global).unwrap();

        let profile_dir = crate::session::get_profile_dir("sandbox-wrapped-codex").unwrap();
        fs::write(
            profile_dir.join("config.toml"),
            r#"[session]
agent_status_hooks = true
agent_detect_as = { "wrapped-codex" = "codex" }
"#,
        )
        .unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };
        let instance_id = "wrapped-codex-sandbox-hooks-test";
        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("wrapped-codex", None),
            false,
            instance_id,
            None,
            "sandbox-wrapped-codex",
        )
        .unwrap();

        let codex_sandbox = temp_home.path().join(".codex").join(SANDBOX_SUBDIR);
        assert!(codex_sandbox.join("config.toml").exists());
        assert!(config.volumes.iter().any(|v| {
            v.host_path == codex_sandbox.to_string_lossy() && v.container_path == "/root/.codex"
        }));

        let codex_config = fs::read_to_string(codex_sandbox.join("config.toml")).unwrap();
        assert!(codex_config.contains("[[hooks.PreToolUse]]"));
        assert!(codex_config.contains("aoe-hooks"));

        let hook_dir =
            crate::hooks::hook_status_dir(instance_id).expect("test id must be allowlist-safe");
        assert!(
            config
                .volumes
                .iter()
                .any(|v| v.host_path == hook_dir.to_string_lossy()),
            "status hook directory should be mounted for custom Codex wrappers"
        );
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    #[test]
    #[serial_test::serial]
    fn test_refresh_agent_configs_preserves_codex_hooks_and_trust_state() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let codex_dir = temp_home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(codex_dir.join("config.toml"), r#"model = "initial""#).unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };
        let instance_id = "codex-sandbox-refresh-hooks-test";
        build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("codex", None),
            false,
            instance_id,
            None,
            "",
        )
        .unwrap();

        let codex_sandbox = codex_dir.join(SANDBOX_SUBDIR);
        let sandbox_config_path = codex_sandbox.join("config.toml");
        let mut sandbox_config = fs::read_to_string(&sandbox_config_path).unwrap();
        sandbox_config.push_str(
            r#"

[hooks.state.trusted]
enabled = true
trusted_hash = "keep"
"#,
        );
        fs::write(&sandbox_config_path, sandbox_config).unwrap();
        fs::write(codex_dir.join("config.toml"), r#"model = "updated""#).unwrap();

        refresh_agent_configs();

        let config_text = fs::read_to_string(&sandbox_config_path).unwrap();
        let config: toml::Value = toml::from_str(&config_text).unwrap();
        assert_eq!(config["model"].as_str(), Some("updated"));
        assert_eq!(
            config["hooks"]["state"]["trusted"]["trusted_hash"].as_str(),
            Some("keep")
        );
        assert!(config_text.contains("[[hooks.PreToolUse]]"));
        assert!(config_text.contains("aoe-hooks"));
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_mounts_codex_home_from_extra_env() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: Some(vec!["CODEX_HOME=/root/custom-codex".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
        };
        let instance_id = "codex-sandbox-extra-env-hooks-test";
        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("codex", None),
            false,
            instance_id,
            None,
            "",
        )
        .unwrap();

        let codex_sandbox = temp_home.path().join(".codex").join(SANDBOX_SUBDIR);
        assert!(codex_sandbox.join("config.toml").exists());
        assert!(config.volumes.iter().any(|v| {
            v.host_path == codex_sandbox.to_string_lossy()
                && v.container_path == "/root/custom-codex"
        }));
        assert!(!config.volumes.iter().any(|v| {
            v.host_path == codex_sandbox.to_string_lossy() && v.container_path == "/root/.codex"
        }));
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_mounts_codex_home_from_sandbox_environment() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let project_dir = TempDir::new().unwrap();
        let config_dir = project_dir.path().join(".agent-of-empires");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            r#"
[sandbox]
environment = ["CODEX_HOME=/root/profile-codex"]
"#,
        )
        .unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };
        let instance_id = "codex-sandbox-config-env-hooks-test";
        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("codex", None),
            false,
            instance_id,
            None,
            "",
        )
        .unwrap();

        let codex_sandbox = temp_home.path().join(".codex").join(SANDBOX_SUBDIR);
        assert!(codex_sandbox.join("config.toml").exists());
        assert!(config.volumes.iter().any(|v| {
            v.host_path == codex_sandbox.to_string_lossy()
                && v.container_path == "/root/profile-codex"
        }));
        assert!(!config.volumes.iter().any(|v| {
            v.host_path == codex_sandbox.to_string_lossy() && v.container_path == "/root/.codex"
        }));
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    /// Regression test: when an instance was created under a non-default profile,
    /// `build_container_config` must resolve sandbox overrides (extra_volumes here)
    /// against THAT profile, not the user's globally configured default profile.
    /// Pre-fix, the TUI's container creation flow always picked up the global
    /// default's volumes regardless of which profile the session was launched under.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_uses_passed_profile_not_global_default() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let app_dir = temp_home
            .path()
            .join(".config")
            .join(crate::session::APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let app_dir = temp_home.path().join(crate::session::APP_DIR_NAME_OTHER);

        let profiles_dir = app_dir.join("profiles");
        fs::create_dir_all(profiles_dir.join("default")).unwrap();
        fs::create_dir_all(profiles_dir.join("personal")).unwrap();

        // Global config selects "default" as the user's currently-active profile.
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(
            app_dir.join("config.toml"),
            r#"default_profile = "default""#,
        )
        .unwrap();

        // Two profiles with distinct extra_volumes so we can tell which one resolved.
        fs::write(
            profiles_dir.join("default").join("config.toml"),
            r#"
[sandbox]
extra_volumes = ["/host/default-only:/container/default-only:ro"]
"#,
        )
        .unwrap();
        fs::write(
            profiles_dir.join("personal").join("config.toml"),
            r#"
[sandbox]
extra_volumes = ["/host/personal-only:/container/personal-only:ro"]
"#,
        )
        .unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();
        let project_path_str = project_dir.path().to_str().unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };

        let has_volume = |config: &crate::containers::container_interface::ContainerConfig,
                          host: &str,
                          container: &str|
         -> bool {
            config
                .volumes
                .iter()
                .any(|v| v.host_path == host && v.container_path == container)
        };

        // Passing "personal" must resolve the personal profile's extra_volumes
        // and NOT the default profile's.
        let cfg_personal = build_container_config(
            project_path_str,
            &sandbox_info,
            ContainerAgentSelection::new("claude", None),
            false,
            "test-instance-id",
            None,
            "personal",
        )
        .unwrap();
        assert!(
            has_volume(
                &cfg_personal,
                "/host/personal-only",
                "/container/personal-only"
            ),
            "personal profile mount missing for profile=personal, got volumes: {:?}",
            cfg_personal
                .volumes
                .iter()
                .map(|v| (&v.host_path, &v.container_path))
                .collect::<Vec<_>>(),
        );
        assert!(
            !has_volume(
                &cfg_personal,
                "/host/default-only",
                "/container/default-only"
            ),
            "default profile mount must NOT leak into profile=personal, got volumes: {:?}",
            cfg_personal
                .volumes
                .iter()
                .map(|v| (&v.host_path, &v.container_path))
                .collect::<Vec<_>>(),
        );

        // Passing "default" must resolve the default profile's extra_volumes.
        let cfg_default = build_container_config(
            project_path_str,
            &sandbox_info,
            ContainerAgentSelection::new("claude", None),
            false,
            "test-instance-id",
            None,
            "default",
        )
        .unwrap();
        assert!(
            has_volume(
                &cfg_default,
                "/host/default-only",
                "/container/default-only"
            ),
            "default profile mount missing for profile=default",
        );

        // Empty profile must fall back to the user's globally configured default,
        // preserving prior behavior for callers without a profile in hand.
        let cfg_empty = build_container_config(
            project_path_str,
            &sandbox_info,
            ContainerAgentSelection::new("claude", None),
            false,
            "test-instance-id",
            None,
            "",
        )
        .unwrap();
        assert!(
            has_volume(&cfg_empty, "/host/default-only", "/container/default-only"),
            "empty profile must fall back to global default",
        );
    }

    /// Regression test for #597: volume_ignores must apply to the parent repo
    /// mount as well as the worktree mount in sibling-worktree sessions.
    #[test]
    #[serial_test::serial]
    fn test_volume_ignores_applied_to_parent_repo_mount() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let (_dir, repo_path) = setup_regular_repo();

        // Create a sibling worktree (non-bare layout)
        let worktree_path = repo_path.parent().unwrap().join("my-worktree");
        let head = git2::Repository::open(&repo_path)
            .unwrap()
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        let repo = git2::Repository::open(&repo_path).unwrap();
        repo.branch("wt-branch", &repo.find_commit(head).unwrap(), false)
            .unwrap();
        drop(repo);

        let output = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt-branch",
            ])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        if !output.status.success() {
            return; // git not available, skip
        }

        // Write repo-level config in the main repo dir, since
        // resolve_config_with_repo loads it from there (find_main_repo) even
        // when the session targets a sibling worktree.
        let config_dir = repo_path.join(".agent-of-empires");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            r#"
[sandbox]
volume_ignores = ["target", "node_modules"]
"#,
        )
        .unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };

        let project_path_str = worktree_path.to_str().unwrap();
        let config = build_container_config(
            project_path_str,
            &sandbox_info,
            ContainerAgentSelection::new("claude", None),
            false,
            "test-instance-id",
            None,
            "",
        )
        .unwrap();

        // Verify volume_ignores are applied to the worktree mount
        assert!(
            config
                .anonymous_volumes
                .iter()
                .any(|v| v.ends_with("/my-worktree/target")),
            "anonymous_volumes should contain worktree target, got: {:?}",
            config.anonymous_volumes
        );
        assert!(
            config
                .anonymous_volumes
                .iter()
                .any(|v| v.ends_with("/my-worktree/node_modules")),
            "anonymous_volumes should contain worktree node_modules, got: {:?}",
            config.anonymous_volumes
        );

        // Verify volume_ignores are also applied to the parent repo mount (the fix for #597)
        let repo_name = repo_path
            .canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let expected_repo_target = format!("/workspace/{}/target", repo_name);
        let expected_repo_node = format!("/workspace/{}/node_modules", repo_name);
        assert!(
            config.anonymous_volumes.contains(&expected_repo_target),
            "anonymous_volumes should contain parent repo target ({}), got: {:?}",
            expected_repo_target,
            config.anonymous_volumes
        );
        assert!(
            config.anonymous_volumes.contains(&expected_repo_node),
            "anonymous_volumes should contain parent repo node_modules ({}), got: {:?}",
            expected_repo_node,
            config.anonymous_volumes
        );
    }

    /// Regression test: volume_ignores must still apply to the workspace_path
    /// in bare-repo layouts where workspace_path is a subdirectory of the mount.
    #[test]
    #[serial_test::serial]
    fn test_volume_ignores_applied_to_bare_repo_worktree() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let (_dir, main_repo_path, worktree_path) = setup_bare_repo_with_worktree();

        if !worktree_path.exists() {
            return; // git worktree add failed, skip
        }

        // Write repo-level config in the main repo dir, since
        // resolve_config_with_repo loads it from there (find_main_repo) even
        // when the session targets a sibling worktree.
        let config_dir = main_repo_path.join(".agent-of-empires");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            r#"
[sandbox]
volume_ignores = ["target"]
"#,
        )
        .unwrap();

        let sandbox_info = super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        };

        let project_path_str = worktree_path.to_str().unwrap();
        let config = build_container_config(
            project_path_str,
            &sandbox_info,
            ContainerAgentSelection::new("claude", None),
            false,
            "test-instance-id",
            None,
            "",
        )
        .unwrap();

        // In bare-repo layout, workspace_path is a subdirectory of the single mount.
        // volume_ignores must apply to the workspace_path (where builds run), not
        // just the mount root.
        let main_name = main_repo_path
            .canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let expected_wt_target = format!("/workspace/{}/main/target", main_name);
        assert!(
            config.anonymous_volumes.contains(&expected_wt_target),
            "anonymous_volumes should contain worktree target ({}), got: {:?}",
            expected_wt_target,
            config.anonymous_volumes
        );
    }

    // --- prepare_sandbox_dir / clean_files tests ---

    #[test]
    fn test_clean_files_deletes_stale_database() {
        let home = TempDir::new().unwrap();
        let host_dir = home.path().join(".local/share/opencode");
        let sandbox_dir = host_dir.join("sandbox");
        fs::create_dir_all(&sandbox_dir).unwrap();

        // Simulate stale database files left by a previous sandbox session
        fs::write(sandbox_dir.join("opencode.db"), "stale").unwrap();
        fs::write(sandbox_dir.join("opencode.db-wal"), "stale-wal").unwrap();
        fs::write(sandbox_dir.join("opencode.db-shm"), "stale-shm").unwrap();

        // Create a minimal host dir so sync_agent_config doesn't error
        fs::create_dir_all(&host_dir).unwrap();

        let mount = AgentConfigMount {
            tool_name: "opencode",
            host_rel: ".local/share/opencode",
            container_suffix: ".local/share/opencode",
            skip_entries: &[
                "sandbox",
                "opencode.db",
                "opencode.db-wal",
                "opencode.db-shm",
            ],
            seed_files: &[],
            copy_dirs: &[],
            keychain_credential: None,
            home_seed_files: &[],
            preserve_files: &[],
            clean_files: &["opencode.db", "opencode.db-wal", "opencode.db-shm"],
        };

        prepare_sandbox_dir(&mount, home.path()).unwrap();

        assert!(!sandbox_dir.join("opencode.db").exists());
        assert!(!sandbox_dir.join("opencode.db-wal").exists());
        assert!(!sandbox_dir.join("opencode.db-shm").exists());
    }

    #[test]
    fn test_skip_entries_prevents_host_db_copy() {
        let home = TempDir::new().unwrap();
        let host_dir = home.path().join(".local/share/opencode");
        let sandbox_dir = host_dir.join("sandbox");
        fs::create_dir_all(&host_dir).unwrap();

        // Host has a database that should NOT be copied
        fs::write(host_dir.join("opencode.db"), "host-db").unwrap();
        // Host also has a config file that SHOULD be copied
        fs::write(host_dir.join("some-config.txt"), "config").unwrap();

        let mount = AgentConfigMount {
            tool_name: "opencode",
            host_rel: ".local/share/opencode",
            container_suffix: ".local/share/opencode",
            skip_entries: &[
                "sandbox",
                "opencode.db",
                "opencode.db-wal",
                "opencode.db-shm",
            ],
            seed_files: &[],
            copy_dirs: &[],
            keychain_credential: None,
            home_seed_files: &[],
            preserve_files: &[],
            clean_files: &[],
        };

        prepare_sandbox_dir(&mount, home.path()).unwrap();

        assert!(
            !sandbox_dir.join("opencode.db").exists(),
            "Host database should not be copied to sandbox"
        );
        assert!(
            sandbox_dir.join("some-config.txt").exists(),
            "Non-skipped files should still be copied"
        );
    }

    #[test]
    fn test_clean_files_noop_when_no_stale_files() {
        let home = TempDir::new().unwrap();
        let host_dir = home.path().join(".local/share/opencode");
        fs::create_dir_all(&host_dir).unwrap();

        let mount = AgentConfigMount {
            tool_name: "opencode",
            host_rel: ".local/share/opencode",
            container_suffix: ".local/share/opencode",
            skip_entries: &["sandbox"],
            seed_files: &[],
            copy_dirs: &[],
            keychain_credential: None,
            home_seed_files: &[],
            preserve_files: &[],
            clean_files: &["opencode.db", "opencode.db-wal", "opencode.db-shm"],
        };

        // Should not panic or error when files don't exist
        prepare_sandbox_dir(&mount, home.path()).unwrap();
    }

    // --- GCP credential mount tests ---
    //
    // These exercise the Vertex AI cred mount inside `build_container_config`.
    // They use `serial_test::serial` because they mutate process-wide env vars,
    // and isolate `HOME`/`XDG_CONFIG_HOME` so global config doesn't bleed in.

    fn build_minimal_sandbox_info() -> super::super::instance::SandboxInfo {
        super::super::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        }
    }

    fn write_adc_at(home: &std::path::Path) -> std::path::PathBuf {
        let adc_dir = home.join(".config").join("gcloud");
        fs::create_dir_all(&adc_dir).unwrap();
        let adc_path = adc_dir.join("application_default_credentials.json");
        fs::write(&adc_path, r#"{"type":"authorized_user"}"#).unwrap();
        adc_path
    }

    fn run_build_for_vertex_test(tool: &str, project_dir: &std::path::Path) -> ContainerConfig {
        git2::Repository::init(project_dir).unwrap();
        let info = build_minimal_sandbox_info();
        build_container_config(
            project_dir.to_str().unwrap(),
            &info,
            ContainerAgentSelection::new(tool, None),
            false,
            "test-instance-id",
            None,
            "",
        )
        .unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn test_vertex_mounts_default_adc_when_flag_set_and_tool_is_claude() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));
        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        let adc_path = write_adc_at(temp_home.path());

        let project_dir = TempDir::new().unwrap();
        let config = run_build_for_vertex_test("claude", project_dir.path());

        let target = "/root/.config/gcloud/application_default_credentials.json";
        let mount = config
            .volumes
            .iter()
            .find(|v| v.container_path == target)
            .expect("expected ADC mount when Vertex flag is set");
        assert_eq!(mount.host_path, adc_path.to_string_lossy());
        assert!(mount.read_only);

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
    }

    #[test]
    #[serial_test::serial]
    fn test_vertex_mounts_custom_path_from_google_application_credentials() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));
        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");

        let cred_dir = TempDir::new().unwrap();
        let custom_cred = cred_dir.path().join("custom-key.json");
        fs::write(&custom_cred, r#"{"type":"service_account"}"#).unwrap();
        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", &custom_cred);

        let project_dir = TempDir::new().unwrap();
        let config = run_build_for_vertex_test("claude", project_dir.path());

        let target = "/root/.config/gcloud/application_default_credentials.json";
        let mount = config
            .volumes
            .iter()
            .find(|v| v.container_path == target)
            .expect("expected mount at well-known ADC path");
        assert_eq!(mount.host_path, custom_cred.to_string_lossy());
        assert!(mount.read_only);

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
    }

    #[test]
    #[serial_test::serial]
    fn test_vertex_skips_mount_when_flag_unset() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));
        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        let _ = write_adc_at(temp_home.path());

        let project_dir = TempDir::new().unwrap();
        let config = run_build_for_vertex_test("claude", project_dir.path());

        let target = "/root/.config/gcloud/application_default_credentials.json";
        assert!(
            !config.volumes.iter().any(|v| v.container_path == target),
            "ADC must not be mounted when Vertex flag is unset",
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_vertex_skips_mount_when_tool_is_not_claude() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));
        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        let _ = write_adc_at(temp_home.path());

        let project_dir = TempDir::new().unwrap();
        let config = run_build_for_vertex_test("opencode", project_dir.path());

        let target = "/root/.config/gcloud/application_default_credentials.json";
        assert!(
            !config.volumes.iter().any(|v| v.container_path == target),
            "ADC must not be mounted for non-claude tools even when Vertex flag is set",
        );

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
    }

    #[test]
    #[serial_test::serial]
    fn test_vertex_skips_mount_when_flag_is_empty_string() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));
        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "");
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        let _ = write_adc_at(temp_home.path());

        let project_dir = TempDir::new().unwrap();
        let config = run_build_for_vertex_test("claude", project_dir.path());

        let target = "/root/.config/gcloud/application_default_credentials.json";
        assert!(
            !config.volumes.iter().any(|v| v.container_path == target),
            "Empty CLAUDE_CODE_USE_VERTEX must be treated as unset",
        );

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
    }

    #[test]
    #[serial_test::serial]
    fn test_vertex_skips_mount_when_adc_file_missing() {
        let temp_home = TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));
        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        // Note: no ADC file written

        let project_dir = TempDir::new().unwrap();
        let config = run_build_for_vertex_test("claude", project_dir.path());

        let target = "/root/.config/gcloud/application_default_credentials.json";
        assert!(
            !config.volumes.iter().any(|v| v.container_path == target),
            "ADC must not be mounted when the host file does not exist",
        );

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
    }

    // --- named_volume_for tests ---

    #[test]
    fn test_named_volume_for_is_deterministic() {
        let a = named_volume_for("sess-abc123", "/workspace/node_modules");
        let b = named_volume_for("sess-abc123", "/workspace/node_modules");
        assert_eq!(a, b);
    }

    #[test]
    fn test_named_volume_for_differs_by_path() {
        let a = named_volume_for("sess-abc123", "/workspace/node_modules");
        let b = named_volume_for("sess-abc123", "/workspace/.venv");
        assert_ne!(a, b, "Different paths must produce different volume names");
    }

    #[test]
    fn test_named_volume_for_differs_by_session_id() {
        let a = named_volume_for("sess-aaa", "/workspace/node_modules");
        let b = named_volume_for("sess-bbb", "/workspace/node_modules");
        assert_ne!(a, b);
    }

    #[test]
    fn test_named_volume_for_sanitizes_unsafe_chars() {
        let name = named_volume_for("sess-1", "/workspace/path with spaces/foo:bar");
        assert!(
            !name.contains(' ') && !name.contains(':'),
            "Volume name must not contain spaces or colons"
        );
    }

    #[test]
    fn test_named_volume_for_starts_with_prefix() {
        let name = named_volume_for("sess-xyz", "/workspace/target");
        assert!(name.starts_with("aoe-vi-sess-xyz-"));
    }

    #[test]
    fn test_named_volume_for_slug_collision_prevented_by_hash() {
        // Two paths that have the same 40-char slug prefix (unlikely but possible for long paths)
        // are disambiguated by the hash suffix.
        let path1 = "/workspace/a-very-long-directory-name-that-exceeds-40-chars-v1";
        let path2 = "/workspace/a-very-long-directory-name-that-exceeds-40-chars-v2";
        let a = named_volume_for("sess-1", path1);
        let b = named_volume_for("sess-1", path2);
        assert_ne!(a, b);
    }

    #[test]
    fn test_named_volume_for_prefix_does_not_match_longer_session_id() {
        // "sess1" must not match volumes belonging to "sess10".
        // The cleanup prefix is "aoe-vi-{session_id}-" (trailing dash), so a volume
        // named "aoe-vi-sess10-..." must NOT start with "aoe-vi-sess1-".
        let vol_sess10 = named_volume_for("sess10", "/workspace/node_modules");
        let prefix_sess1 = format!("aoe-vi-{}-", "sess1");
        assert!(
            !vol_sess10.starts_with(&prefix_sess1),
            "Volume for sess10 must not match the cleanup prefix for sess1: {}",
            vol_sess10
        );
    }
}
