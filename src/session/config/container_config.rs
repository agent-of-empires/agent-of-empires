//! Container configuration building for sandboxed sessions.
//!
//! Standalone functions for computing Docker volume mounts and building
//! `ContainerConfig` structs. Includes sandbox directory sync, agent config
//! mounting, and credential extraction.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::containers::{ContainerConfig, EnvEntry, NamedVolumeMount, RunPolicy, VolumeMount};
use crate::git::GitWorktree;
use crate::session::config::VolumeIgnoresStrategy;

use crate::hooks::SymlinkPolicy;
use crate::session::environment::collect_environment;
use crate::session::instance::SandboxInfo;

mod seed;
use seed::{sync_agent_config, NativeStateBoundary};
/// Subdirectory name inside each agent's config dir for sandbox config.
const SANDBOX_SUBDIR: &str = "sandbox";
const SANDBOX_PRIVATE_SUBDIR: &str = "sandbox-v2";
pub(crate) const CURRENT_SANDBOX_STORE_GENERATION: u8 = 2;

/// Content seeded into the Claude sandbox `.sandbox-gitconfig`. Scoped to github.com;
/// the helper emits credentials only on `get` and only when GH_TOKEN is non-empty, so
/// other remotes and sessions without a forwarded token fall through to normal git
/// behavior.
const SANDBOX_GITCONFIG_SEED: &str = r#"[credential "https://github.com"]
	helper = "!f() { test \"$1\" = get || exit 0; test -n \"$GH_TOKEN\" || exit 0; echo username=x-access-token; echo \"password=$GH_TOKEN\"; }; f"
"#;

/// Positive configuration/resource policy for one native config mount.
/// Runtime paths below are alias guards, never a negative copy policy or proof
/// that a pre-existing sandbox history belongs to its current instance.
struct AgentConfigMount {
    tool_name: &'static str,
    host_rel: &'static str,
    container_suffix: &'static str,
    /// Only these regular configuration files may be refreshed from the host.
    copy_files: &'static [&'static str],
    seed_files: &'static [(&'static str, &'static str)],
    /// Approved code/resource collections, seeded only while individually absent.
    copy_dirs: &'static [&'static str],
    keychain_credential: Option<(&'static str, &'static str)>,
    home_seed_files: &'static [(&'static str, &'static str)],
    preserve_files: &'static [&'static str],
    /// Shared rotating credentials retain their existing separately locked flow.
    shared_credential_files: &'static [&'static str],
    /// Portable encrypted credentials are published as complete seed-once pairs.
    credential_pairs: &'static [(&'static str, &'static str)],
    /// Seed-once consistent snapshots, not raw database/sidecar copies.
    sqlite_seed_files: &'static [&'static str],
    /// Known native state paths/globs relative to this mount, for resource alias checks.
    native_state_paths: &'static [&'static str],
}

/// The sole source-copy registry. Unknown root files never gain copy authority.
const AGENT_CONFIG_MOUNTS: &[AgentConfigMount] = &[
    AgentConfigMount {
        tool_name: "claude",
        host_rel: ".claude",
        container_suffix: ".claude",
        copy_files: &["settings.json", "CLAUDE.md", "keybindings.json"],
        seed_files: &[],
        copy_dirs: &["plugins", "skills", "hooks"],
        // Keychain credentials seed the existing shared OAuth-token mount.
        keychain_credential: Some(("Claude Code-credentials", ".credentials.json")),
        // Onboarding is home-level. IS_SANDBOX makes Claude use the separately
        // mounted gitconfig; its helper forwards GH_TOKEN only for github.com get.
        home_seed_files: &[
            (".claude.json", r#"{"hasCompletedOnboarding":true}"#),
            (".sandbox-gitconfig", SANDBOX_GITCONFIG_SEED),
        ],
        preserve_files: &["settings.json"],
        shared_credential_files: &[".credentials.json"],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &[
            "projects",
            "history.jsonl",
            "file-history",
            "session-env",
            "todos",
            "debug",
            "statsig",
            "paste-cache",
            "shell-snapshots",
            "tasks",
        ],
    },
    AgentConfigMount {
        tool_name: "opencode",
        host_rel: ".local/share/opencode",
        container_suffix: ".local/share/opencode",
        copy_files: &["auth.json", "mcp-auth.json"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["opencode.db*", "storage", "log", "snapshot"],
    },
    AgentConfigMount {
        tool_name: "opencode",
        host_rel: ".config/opencode",
        container_suffix: ".config/opencode",
        copy_files: &["opencode.json", "opencode.jsonc", "config.json"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &[],
    },
    AgentConfigMount {
        tool_name: "codex",
        host_rel: ".codex",
        container_suffix: ".codex",
        copy_files: &["config.toml", "auth.json", "hooks.json", "AGENTS.md"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &[
            "sessions",
            "archived_sessions",
            "history.jsonl",
            "state_*.sqlite*",
            "session_index.jsonl",
            "shell_snapshots",
            "log",
            "logs",
            "tmp",
        ],
    },
    AgentConfigMount {
        tool_name: "gemini",
        host_rel: ".gemini",
        container_suffix: ".gemini",
        copy_files: &[
            "settings.json",
            "oauth_creds.json",
            "mcp-oauth-tokens.json",
            "a2a-oauth-tokens.json",
            "keybindings.json",
            "GEMINI.md",
            ".env",
        ],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["tmp", "history", "sessions", "cache"],
    },
    AgentConfigMount {
        tool_name: "vibe",
        host_rel: ".vibe",
        container_suffix: ".vibe",
        copy_files: &["config.toml", ".env", "hooks.toml", "AGENTS.md"],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["logs", "sessions", "history"],
    },
    AgentConfigMount {
        tool_name: "cursor",
        host_rel: ".cursor",
        container_suffix: ".cursor",
        copy_files: &[
            "cli-config.json",
            "auth.json",
            "mcp-auth.json",
            "mcp-approvals.json",
            "mcp.json",
        ],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["chats", "projects", "agent-transcripts", "store.db*"],
    },
    AgentConfigMount {
        tool_name: "copilot",
        host_rel: ".copilot",
        container_suffix: ".copilot",
        copy_files: &[
            "config.json",
            "config",
            "settings.json",
            "mcp-config.json",
            "lsp-config.json",
        ],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["session-state", "sessions", "history.jsonl", "logs"],
    },
    AgentConfigMount {
        tool_name: "pi",
        host_rel: ".pi",
        container_suffix: ".pi",
        copy_files: &[
            "agent/auth.json",
            "agent/models.json",
            "agent/settings.json",
            "agent/keybindings.json",
            "agent/AGENTS.override.md",
            "agent/AGENTS.md",
            "agent/AGENTS.MD",
            "agent/CLAUDE.md",
            "agent/CLAUDE.MD",
            "agent/SYSTEM.md",
            "agent/APPEND_SYSTEM.md",
        ],
        seed_files: &[],
        copy_dirs: &[
            "agent/extensions",
            "agent/skills",
            "agent/prompts",
            "agent/themes",
            "agent/npm",
            "agent/git",
        ],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["agent/sessions", "agent/pi-debug.log", "agent/tmp"],
    },
    AgentConfigMount {
        tool_name: "omp",
        host_rel: ".omp",
        container_suffix: ".omp",
        copy_files: &[
            ".env",
            "auth-broker.token",
            "agent/.env",
            "agent/config.yml",
            "agent/config.yaml",
            "agent/settings.json",
            "agent/models.yml",
            "agent/models.yaml",
            "agent/models.json",
            "agent/keybindings.yml",
            "agent/keybindings.yaml",
            "agent/keybindings.json",
            "agent/AGENTS.md",
            "agent/SYSTEM.md",
            "agent/APPEND_SYSTEM.md",
            "agent/TITLE_SYSTEM.md",
            "agent/PERSONALITY.md",
            "agent/RULES.md",
            "agent/WATCHDOG.md",
            "agent/WATCHDOG.yml",
            "agent/WATCHDOG.yaml",
            "agent/mcp.json",
            "agent/.mcp.json",
            "agent/ssh.json",
            "agent/secrets.yml",
            "agent/smithery.json",
            "agent/lsp.json",
            "agent/.lsp.json",
            "agent/lsp.yaml",
            "agent/.lsp.yaml",
            "agent/lsp.yml",
            "agent/.lsp.yml",
            "agent/dap.json",
            "agent/.dap.json",
            "agent/dap.yaml",
            "agent/.dap.yaml",
            "agent/dap.yml",
            "agent/.dap.yml",
            "agent/share.ts",
            "agent/share.js",
            "agent/share.mjs",
        ],
        seed_files: &[],
        copy_dirs: &[
            "agent/extensions",
            "agent/skills",
            "agent/commands",
            "agent/rules",
            "agent/prompts",
            "agent/instructions",
            "agent/hooks",
            "agent/tools",
            "agent/themes",
            "agent/agents",
        ],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &["agent/agent.db"],
        native_state_paths: &[
            "agent/sessions",
            "agent/history.db*",
            "agent/blobs",
            "agent/memories",
            "agent/managed-skills",
            "agent/terminal-sessions",
            "agent/debug",
            "agent/crashes",
        ],
    },
    AgentConfigMount {
        tool_name: "hermes",
        host_rel: ".hermes",
        container_suffix: ".hermes",
        copy_files: &[
            "config.yaml",
            ".env",
            "auth.json",
            "shell-hooks-allowlist.json",
            "SOUL.md",
        ],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &["shell-hooks-allowlist.json"],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        // Hermes native state is declared by its own rule catalogue
        // (`seed/hermes/state.rs`), which registers through `register_home`
        // before this list is ever read.
        native_state_paths: &[],
    },
    AgentConfigMount {
        tool_name: "droid",
        host_rel: ".factory",
        container_suffix: ".factory",
        copy_files: &[
            "settings.json",
            "settings.local.json",
            "config.json",
            "hooks.json",
            "mcp.json",
        ],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[
            ("auth.v2.file", "auth.v2.key"),
            ("mcp-oauth.v2.file", "mcp-oauth.v2.key"),
        ],
        sqlite_seed_files: &[],
        native_state_paths: &["sessions", "history.json", "logs", "cache"],
    },
    AgentConfigMount {
        tool_name: "kiro",
        host_rel: ".kiro",
        container_suffix: ".kiro",
        copy_files: &[],
        seed_files: &[],
        copy_dirs: &["agents", "steering", "prompts", "settings"],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["sessions", "logs", "cache"],
    },
    AgentConfigMount {
        tool_name: "qwen",
        host_rel: ".qwen",
        container_suffix: ".qwen",
        copy_files: &[
            "settings.json",
            "oauth_creds.json",
            "mcp-oauth-tokens.json",
            "QWEN.md",
            "AGENTS.md",
        ],
        seed_files: &[],
        copy_dirs: &[],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["sessions", "tmp", "cache", "history"],
    },
    AgentConfigMount {
        tool_name: "antigravity",
        host_rel: ".gemini/antigravity-cli",
        container_suffix: ".gemini/antigravity-cli",
        copy_files: &[
            "antigravity-oauth-token",
            "settings.json",
            "keybindings.json",
            "hooks.json",
            "mcp_config.json",
            "skills.json",
            "plugins.json",
        ],
        seed_files: &[],
        copy_dirs: &["plugins"],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &["antigravity-oauth-token"],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &[
            "conversation_summaries.db*",
            "jetbox_summaries.pb",
            "jetbox_summaries_proto.pb",
            "jetski_state.pbtxt",
            "history.jsonl",
            "conversations",
            "cascade",
            "brain",
            "cache",
            "logs",
        ],
    },
    AgentConfigMount {
        tool_name: "kimi",
        host_rel: ".kimi-code",
        container_suffix: ".kimi-code",
        copy_files: &["config.toml"],
        seed_files: &[],
        copy_dirs: &["skills"],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["sessions", "session_index.jsonl", "logs", "cache", "agents"],
    },
    AgentConfigMount {
        tool_name: "prime-agent",
        host_rel: ".prime/agent",
        container_suffix: ".prime/agent",
        copy_files: &["auth.json", "settings.json", "models.json", "AGENTS.md"],
        seed_files: &[],
        copy_dirs: &["skills"],
        keychain_credential: None,
        home_seed_files: &[],
        preserve_files: &[],
        shared_credential_files: &[],
        credential_pairs: &[],
        sqlite_seed_files: &[],
        native_state_paths: &["sessions", "kernel-venv"],
    },
];

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

/// The furthest `expiresAt` a real token carries. The shared file is writable
/// from inside every sandbox, so a planted timestamp beyond this would
/// otherwise outrank every later login for good.
const CREDENTIAL_EXPIRY_HORIZON: std::time::Duration =
    std::time::Duration::from_secs(400 * 24 * 60 * 60);

/// The `expiresAt` of a credential a container could use, or `None` when the
/// content is not one.
///
/// A blanked token block is not one. Claude Code empties `accessToken` and
/// `refreshToken` in place when its credential fails to authenticate and
/// keeps the rest of the block, so what it leaves still parses and can still
/// carry an expiry (#3860). Read as a credential, that shell mounts dead into
/// every later container and blocks the seed that would repair it.
fn plausible_credential_expires_at(content: &str, now_ms: u64) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let oauth = value.get("claudeAiOauth")?;
    let filled = |field| {
        oauth
            .get(field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|token| !token.trim().is_empty())
    };
    if !filled("accessToken") && !filled("refreshToken") {
        return None;
    }
    oauth.get("expiresAt")?.as_u64().filter(|expires_at| {
        *expires_at <= now_ms.saturating_add(CREDENTIAL_EXPIRY_HORIZON.as_millis() as u64)
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Decide whether an incoming credential should overwrite the existing one,
/// based on `expiresAt` timestamps. Returns `true` if the incoming credential
/// should be written.
fn should_overwrite_credential(existing_content: &str, incoming_content: &str) -> bool {
    let now = now_ms();
    let existing_exp = plausible_credential_expires_at(existing_content, now);
    let incoming_exp = plausible_credential_expires_at(incoming_content, now);

    match (existing_exp, incoming_exp) {
        (Some(existing), Some(incoming)) => incoming > existing,
        (Some(_), None) => false,
        _ => true,
    }
}

/// Read a credential from the macOS Keychain. `None` when there is no usable
/// entry.
#[cfg(target_os = "macos")]
fn read_keychain_credential(service: &str) -> Result<Option<String>> {
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
        return Ok(None);
    }

    let content = String::from_utf8_lossy(&output.stdout);
    let trimmed = content.trim();
    if trimmed.is_empty() {
        tracing::warn!(target: "session.profile",
            "Keychain entry for service '{}' exists but has empty content",
            service
        );
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

#[cfg(not(target_os = "macos"))]
fn read_keychain_credential(_service: &str) -> Result<Option<String>> {
    Ok(None)
}

/// The shared credential file for the store root `sandbox_dir` sits under, or
/// `None` for the legacy single shared store, whose parent is the host config.
fn shared_credential_path(sandbox_dir: &Path, name: &str) -> Option<PathBuf> {
    let root = sandbox_dir.parent()?;
    (root.file_name()? == SANDBOX_PRIVATE_SUBDIR).then(|| root.join(name))
}

/// A file's content, or `None` when it is absent, empty (the placeholder a
/// runtime leaves under a file mount) or not a plain file. A plain file that
/// cannot be read is an error, never `None`: the caller would take absence as
/// leave to write over it. The store is container-writable, so a link planted
/// there is not followed; the host file is the user's own.
fn read_credential_file(dir: &Path, name: &str, follow: SymlinkPolicy) -> Result<Option<String>> {
    let path = dir.join(name);
    if let Some(content) = follow.read(&path)? {
        return Ok(Some(content).filter(|content| !content.trim().is_empty()));
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            anyhow::bail!("{} exists but could not be read", path.display())
        }
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("inspecting {}", path.display())),
    }
}

/// A candidate that cannot be read is skipped; the file it would replace is
/// still the one every sandbox mounts.
fn read_credential_candidate(dir: &Path, name: &str, follow: SymlinkPolicy) -> Option<String> {
    read_credential_file(dir, name, follow).unwrap_or_else(|e| {
        tracing::warn!(target: "session.profile", "Skipping credential candidate: {}", e);
        None
    })
}

/// Which candidates a fold may put over a credential the shared file holds.
/// The host file and the Keychain only ever seed a file holding none: a token
/// copied from the host is the host's own refresh token, and the first side
/// to refresh would log the other out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CredentialFold {
    /// A come-up, create or start: a fresher copy left in the store by an
    /// earlier layout, a sandbox chain of its own, replaces what the file
    /// holds.
    Freshest,
    /// Off a come-up: nothing replaces what the file holds.
    SeedOnly,
}

/// Whether `content` carries a credential a container could use.
fn holds_credential(content: &str) -> bool {
    plausible_credential_expires_at(content, now_ms()).is_some()
}

/// Fold the freshest credential into the file every store of this agent
/// mounts. This is a come-up fold rather than a migration because a container
/// built before the file was shared keeps refreshing its store copy while it
/// runs, so which copy is freshest is only known at come-up. That copy is left
/// for such a container; [`place_shadowed_credential_mountpoints`] empties it
/// once a container mounting the shared file exists.
///
/// Candidates are the store's private copy (left by the v027 move or by a
/// login before the file was shared), the host file and the macOS Keychain,
/// the freshest by `expiresAt` winning among those `fold` admits; see
/// [`CredentialFold`]. Only the winner's
/// `claudeAiOauth` replaces the file's, so what the agent keeps beside it
/// survives. The file is written in place: a rename would leave every running
/// container's bind mount on the old inode. An absent file is created
/// empty-but-valid so the mount has a source; the agent's own login fills it.
fn sync_shared_credential(
    mount: &AgentConfigMount,
    host_dir: &Path,
    sandbox_dir: &Path,
    name: &str,
    fold: CredentialFold,
) -> Result<Option<PathBuf>> {
    let Some(shared) = shared_credential_path(sandbox_dir, name) else {
        return Ok(None);
    };
    let Some(root) = shared.parent() else {
        return Ok(None);
    };
    std::fs::create_dir_all(root)?;

    // Each candidate with whether it is a sandbox chain of its own.
    let mut candidates = Vec::new();
    candidates.extend(
        read_credential_candidate(sandbox_dir, name, SymlinkPolicy::Never).map(|c| (c, true)),
    );
    candidates.extend(
        read_credential_candidate(host_dir, name, SymlinkPolicy::Follow).map(|c| (c, false)),
    );
    if let Some((service, _)) = mount
        .keychain_credential
        .filter(|(_, filename)| *filename == name)
    {
        match read_keychain_credential(service) {
            Ok(content) => candidates.extend(content.map(|c| (c, false))),
            Err(e) => tracing::warn!(target: "session.profile",
                "Failed to read keychain credential for {}: {}", mount.host_rel, e),
        }
    }

    // The candidates are gathered outside the lock (the Keychain read is a
    // subprocess); the file is read again under it, right before the write,
    // so another come-up or a container's own refresh in between is seen.
    crate::hooks::with_config_lock_policy(&shared, "lock", SymlinkPolicy::Never, || {
        let existing = read_credential_file(root, name, SymlinkPolicy::Never)?;
        let existing_usable = existing.as_deref().is_some_and(holds_credential);
        let mut winner: Option<&str> = None;
        for (candidate, sandbox_chain) in &candidates {
            if existing_usable && !(*sandbox_chain && fold == CredentialFold::Freshest) {
                continue;
            }
            let current = winner.or(existing.as_deref());
            if current.is_none_or(|current| should_overwrite_credential(current, candidate)) {
                winner = Some(candidate);
            }
        }
        match (existing.as_deref(), winner) {
            (None, None) => write_credential_in_place(&shared, "{}"),
            (_, None) => Ok(()),
            (existing, Some(winner)) => {
                write_credential_in_place(&shared, &merge_credential(existing, winner))
            }
        }
    })?;
    Ok(Some(shared))
}

/// `winner` with everything `existing` held beside `claudeAiOauth` kept, so a
/// host copy that wins on expiry does not drop what the agent stored in the
/// file inside a container. Anything that is not two JSON objects is replaced
/// whole.
fn merge_credential(existing: Option<&str>, winner: &str) -> String {
    let parsed = |content: &str| {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(content).ok()
    };
    let merged = existing.and_then(parsed).and_then(|mut merged| {
        let oauth = parsed(winner)?.remove("claudeAiOauth")?;
        merged.insert("claudeAiOauth".to_string(), oauth);
        serde_json::to_string(&merged).ok()
    });
    merged.unwrap_or_else(|| winner.to_string())
}

/// Place the mountpoint each store needs for the shared credential file
/// mounted over it, and empty the store's own copy once the shared file holds
/// it.
///
/// The mount is a file nested inside the store's directory mount, so a runtime
/// asked to create the mountpoint resolves it through that mount and lands on
/// the host store, outside the container's rootfs. Docker Desktop refuses it
/// there with `create mountpoint for ... is outside of rootfs` yet leaves the
/// file behind, which is why a store without it failed every first create and
/// worked on the retry (#3845, upstream docker/for-mac#7853). Placing the
/// file ourselves is what makes the first create work, and keeps the runtime
/// from having to create a mountpoint through a bind mount at all.
///
/// Empty rather than carrying the copy's old content: [`read_credential_file`]
/// reads an empty file as no credential, so a copy [`sync_shared_credential`]
/// already folded in cannot come back as a candidate. Runs before every create
/// and start, since the runtime needs the mountpoint each time it applies the
/// mounts.
pub(crate) fn place_shadowed_credential_mountpoints(config: &ContainerConfig) {
    let volume_at = |target: &Path| {
        config
            .volumes
            .iter()
            .find(|volume| Path::new(&volume.container_path) == target)
    };
    for container_path in &config.shared_credential_mounts {
        let path = Path::new(container_path);
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            continue;
        };
        let (Some(shared), Some(store)) = (volume_at(path), volume_at(dir)) else {
            continue;
        };
        // Only a store beside the shared file holds a shadowed copy. A user
        // extra_volumes entry at the config path replaces the store mount, and
        // the credential under it is the user's own login.
        if Path::new(&store.host_path).parent() != Path::new(&shared.host_path).parent() {
            continue;
        }
        let store_dir = Path::new(&store.host_path);
        let copy = store_dir.join(name);
        let folded = Path::new(&shared.host_path)
            .parent()
            .zip(name.to_str())
            .is_some_and(|(root, name)| shadowed_copy_is_folded(root, store_dir, name));
        if let Err(e) = place_credential_mountpoint(&copy, folded) {
            tracing::warn!(target: "session.profile",
                "Failed to place credential mountpoint {}: {}", copy.display(), e);
        }
    }
}

/// Whether the shared file holds a credential at least as fresh as the store's
/// own copy, so the copy can be emptied. Read now rather than remembered from
/// the fold: a fold that failed, or a copy it could not read, leaves the copy
/// the only chain there is, and a plain file already serves as the
/// mountpoint. Either side that cannot be read keeps the copy.
fn shadowed_copy_is_folded(shared_root: &Path, store: &Path, name: &str) -> bool {
    let shared = read_credential_file(shared_root, name, SymlinkPolicy::Never);
    let copy = read_credential_file(store, name, SymlinkPolicy::Never);
    match (shared, copy) {
        (Ok(Some(shared)), Ok(Some(copy))) => !should_overwrite_credential(&shared, &copy),
        _ => false,
    }
}

/// A plain file at `path` for the runtime to mount over, emptied when
/// `replace` and left as it is otherwise. The store is container-writable and
/// a runtime that got as far as making a directory there leaves one, so
/// anything but a plain file is replaced rather than mounted through.
fn place_credential_mountpoint(path: &Path, replace: bool) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && (!replace || metadata.len() == 0) => return Ok(()),
        Ok(metadata) => {
            if metadata.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            }
            .with_context(|| format!("removing {}", path.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("inspecting {}", path.display())),
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map(|_| ())
        .with_context(|| format!("creating {}", path.display()))
}

/// Write in place, never rename: containers bind-mount this inode. The new
/// content lands before the old tail is cut, so a concurrent reader never
/// sees an empty file.
fn write_credential_in_place(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening shared credential {}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.set_len(content.len() as u64))
        .with_context(|| format!("writing shared credential {}", path.display()))
}

/// Return a sandbox config path. A supplied instance ID creates the physically
/// isolated store mounted into that pane; no live conversation store is shared.
fn sandbox_dir_for(
    mount: &AgentConfigMount,
    home: &Path,
    instance_id: Option<&str>,
) -> Result<PathBuf> {
    let sandbox_dir = home.join(mount.host_rel).join(SANDBOX_SUBDIR);
    let Some(instance_id) = instance_id else {
        return Ok(sandbox_dir);
    };
    crate::session::validate_instance_id(instance_id).map_err(|e| {
        anyhow::anyhow!(
            "refusing to use sandbox config for unsafe AOE_INSTANCE_ID {instance_id:?}: {e}"
        )
    })?;
    Ok(home
        .join(mount.host_rel)
        .join(SANDBOX_PRIVATE_SUBDIR)
        .join(instance_id))
}
pub(crate) fn sandbox_store_dir(
    tool: &str,
    home: &Path,
    declared_config_dir: Option<&Path>,
    instance_id: &str,
) -> Result<Option<PathBuf>> {
    let Some(mount) = AGENT_CONFIG_MOUNTS
        .iter()
        .find(|mount| mount.tool_name == tool)
    else {
        return Ok(None);
    };
    declared_config_dir
        .map(|dir| Ok(dir.join(SANDBOX_PRIVATE_SUBDIR).join(instance_id)))
        .unwrap_or_else(|| sandbox_dir_for(mount, home, Some(instance_id)))
        .map(Some)
}

/// Legacy shared store and new instance-private store pairs for one agent.
/// Multiple entries matter for agents such as OpenCode that mount config and
/// data directories separately. Identical declared roots are deduplicated.
pub(crate) fn sandbox_store_migration_paths(
    tool: &str,
    home: &Path,
    declared_config_dir: Option<&Path>,
    instance_id: &str,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    crate::session::validate_instance_id(instance_id)?;
    let mut paths = Vec::new();
    for mount in AGENT_CONFIG_MOUNTS
        .iter()
        .filter(|mount| mount.tool_name == tool)
    {
        let shared = declared_config_dir
            .map(|dir| dir.join(SANDBOX_SUBDIR))
            .unwrap_or_else(|| home.join(mount.host_rel).join(SANDBOX_SUBDIR));
        let private = declared_config_dir
            .map(|dir| dir.join(SANDBOX_PRIVATE_SUBDIR).join(instance_id))
            .unwrap_or_else(|| {
                home.join(mount.host_rel)
                    .join(SANDBOX_PRIVATE_SUBDIR)
                    .join(instance_id)
            });
        if !paths
            .iter()
            .any(|pair| pair == &(shared.clone(), private.clone()))
        {
            paths.push((shared, private));
        }
    }
    Ok(paths)
}

/// Every private store root an agent can own: one per config mount, or the
/// single declared root when the profile points the agent elsewhere. This is
/// the directory whose children are per-instance stores.
pub(crate) fn sandbox_store_roots(
    tool: &str,
    home: &Path,
    declared_config_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for mount in AGENT_CONFIG_MOUNTS
        .iter()
        .filter(|mount| mount.tool_name == tool)
    {
        let root = declared_config_dir
            .map(|dir| dir.join(SANDBOX_PRIVATE_SUBDIR))
            .unwrap_or_else(|| home.join(mount.host_rel).join(SANDBOX_PRIVATE_SUBDIR));
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// Every private store one instance owns. [`sandbox_store_dir`] answers for
/// the mount an agent launches from; reclaiming has to reach the rest too,
/// since agents such as OpenCode mount config and data separately.
pub(crate) fn sandbox_store_dirs(
    tool: &str,
    home: &Path,
    declared_config_dir: Option<&Path>,
    instance_id: &str,
) -> Result<Vec<PathBuf>> {
    crate::session::validate_instance_id(instance_id)?;
    Ok(sandbox_store_roots(tool, home, declared_config_dir)
        .into_iter()
        .map(|root| root.join(instance_id))
        .collect())
}

pub(crate) fn sandbox_content_roots(
    tool: &str,
    command: Option<&str>,
    session: &super::SessionConfig,
    home: &Path,
    instance: &str,
) -> Result<Vec<crate::migrations::v033_isolate_sandbox_content::ContentRoot>> {
    use crate::migrations::v033_isolate_sandbox_content::{canonical_expected_path, ContentRoot};
    crate::session::validate_instance_id(instance)?;
    let Some(agent) = resolve_active_agent(tool, command, session) else {
        return Ok(Vec::new());
    };
    let declared = session.agent_config_dir_for(tool, home);
    let mut roots: Vec<ContentRoot> = Vec::new();
    for mount in AGENT_CONFIG_MOUNTS
        .iter()
        .filter(|mount| mount.tool_name == agent.name)
    {
        let host = canonical_expected_path(
            &declared
                .clone()
                .unwrap_or_else(|| home.join(mount.host_rel)),
        )?;
        let path = canonical_expected_path(&host.join(SANDBOX_PRIVATE_SUBDIR))?.join(instance);
        if let Some(root) = roots.iter_mut().find(|root| root.path == path) {
            root.roles.push(mount.container_suffix.to_owned());
        } else {
            roots.push(ContentRoot {
                path,
                host,
                roles: vec![mount.container_suffix.to_owned()],
            });
        }
    }
    roots.sort_by(|left, right| left.path.cmp(&right.path));
    for root in &mut roots {
        root.roles.sort();
        root.roles.dedup();
    }
    Ok(roots)
}

/// Complete shared-host policy only while planning a transition. Capture and
/// admission checks retain their small, requested role subsets.
pub(crate) fn expand_content_roles(
    roots: &mut [crate::migrations::v033_isolate_sandbox_content::ContentRoot],
    home: &Path,
    session: &super::SessionConfig,
) -> Result<()> {
    use crate::migrations::v033_isolate_sandbox_content::canonical_expected_path;
    let mut add_tool = |tool: &str| -> Result<()> {
        let Some(agent) = resolve_active_agent(tool, None, session) else {
            return Ok(());
        };
        let declared = session.agent_config_dir_for(tool, home);
        for mount in AGENT_CONFIG_MOUNTS
            .iter()
            .filter(|mount| mount.tool_name == agent.name)
        {
            let host = canonical_expected_path(
                &declared
                    .clone()
                    .unwrap_or_else(|| home.join(mount.host_rel)),
            )?;
            for root in roots.iter_mut().filter(|root| root.host == host) {
                root.roles.push(mount.container_suffix.to_owned());
            }
        }
        Ok(())
    };
    for tool in agent_config_mount_tools() {
        add_tool(tool)?;
    }
    for tool in session.agent_config_dir.keys() {
        if !AGENT_CONFIG_MOUNTS
            .iter()
            .any(|mount| mount.tool_name == tool)
        {
            add_tool(tool)?;
        }
    }
    for root in roots {
        root.roles.sort();
        root.roles.dedup();
    }
    Ok(())
}

pub(crate) fn content_role_agent(role: &str) -> Option<&'static str> {
    AGENT_CONFIG_MOUNTS
        .iter()
        .find(|mount| mount.container_suffix == role)
        .map(|mount| mount.tool_name)
}

/// v027 copied shared stores for most agents, including sandbox-only dirs.
/// Codex was already private; Claude and OpenCode keep their existing carry.
fn carried_state(mount: &AgentConfigMount) -> &'static [&'static str] {
    match (mount.tool_name, mount.container_suffix) {
        ("claude", ".claude") => &["projects"],
        ("opencode", ".local/share/opencode") => {
            &["opencode.db", "opencode.db-wal", "opencode.db-shm"]
        }
        ("codex", ".codex") => &["sessions", "archived_sessions"],
        _ => &[],
    }
}

/// Only these carried formats retain an established native resume path.
pub(crate) fn agent_retains_native_resume(agent: &str) -> bool {
    matches!(agent, "claude" | "opencode" | "codex")
}

pub(crate) fn retry_source_change<T>(seed: impl FnMut() -> Result<T>) -> Result<T> {
    seed::retry_source_change(seed)
}

pub(crate) fn source_changed(error: &anyhow::Error) -> bool {
    seed::source_changed(error)
}

enum ContentSeedMode {
    Fresh,
    StoppedOriginal,
    OwnedExtension,
}

pub(crate) fn seed_content_stage(
    input: &crate::migrations::v033_isolate_sandbox_content::ContentSeed<'_>,
    root: &crate::migrations::v033_isolate_sandbox_content::ContentRoot,
    destination: &Path,
    home: &Path,
    session: &super::SessionConfig,
    workspace: &Path,
) -> Result<BTreeSet<String>> {
    seed_content_roles(
        input.path(),
        root,
        destination,
        home,
        session,
        workspace,
        Some(input),
    )
}

/// Add only absent configuration for newly required roles. This does not
/// construct a fresh-stage capability or replace an owned native store.
pub(crate) fn extend_owned_content(
    root: &crate::migrations::v033_isolate_sandbox_content::ContentRoot,
    home: &Path,
    session: &super::SessionConfig,
    workspace: &Path,
) -> Result<()> {
    seed_content_roles(&root.host, root, &root.path, home, session, workspace, None).map(|_| ())
}

/// The bytes to publish when a JSON seed default meets a file carried forward
/// from a retired original: the default's keys win, the carried file's other
/// keys stay. `None` when the file is absent or either side is not a JSON
/// object, so the caller keeps the seed-once publish.
fn merged_json_seed(
    output: &crate::session::anchored_fs::AnchoredDir,
    path: &Path,
    default: &str,
) -> Result<Option<Vec<u8>>> {
    let Some(existing) = output.read_regular(path, 8 << 20)? else {
        return Ok(None);
    };
    let (Ok(serde_json::Value::Object(mut carried)), Ok(serde_json::Value::Object(seed))) = (
        serde_json::from_slice::<serde_json::Value>(&existing),
        serde_json::from_str::<serde_json::Value>(default),
    ) else {
        return Ok(None);
    };
    for (key, value) in seed {
        carried.insert(key, value);
    }
    Ok(Some(serde_json::to_vec(&serde_json::Value::Object(
        carried,
    ))?))
}

fn seed_content_roles(
    source: &Path,
    root: &crate::migrations::v033_isolate_sandbox_content::ContentRoot,
    destination: &Path,
    home: &Path,
    session: &super::SessionConfig,
    workspace: &Path,
    input: Option<&crate::migrations::v033_isolate_sandbox_content::ContentSeed<'_>>,
) -> Result<BTreeSet<String>> {
    use std::os::unix::fs::PermissionsExt;
    let mode = match input {
        Some(input) if input.is_stopped_original() => ContentSeedMode::StoppedOriginal,
        Some(_) => ContentSeedMode::Fresh,
        None => ContentSeedMode::OwnedExtension,
    };
    let output = crate::session::anchored_fs::AnchoredDir::open(destination)?;
    let mut carried = BTreeSet::new();
    for mount in AGENT_CONFIG_MOUNTS
        .iter()
        .filter(|mount| root.roles.iter().any(|role| role == mount.container_suffix))
    {
        if source.exists() {
            let mut seed = || -> Result<()> {
                let mut boundary =
                    NativeStateBoundary::new(source, mount, home, session, destination)?;
                let stopped = matches!(mode, ContentSeedMode::StoppedOriginal);
                let files = if stopped {
                    boundary = boundary.for_stopped_original(&root.host, mount)?;
                    let mut files = mount.copy_files.to_vec();
                    files.extend(mount.home_seed_files.iter().map(|(name, _)| *name));
                    files.extend(mount.shared_credential_files.iter().copied());
                    std::borrow::Cow::Owned(files)
                } else {
                    std::borrow::Cow::Borrowed(mount.copy_files)
                };
                let preserve = if matches!(mode, ContentSeedMode::OwnedExtension) {
                    files.as_ref()
                } else {
                    mount.preserve_files
                };
                sync_agent_config(
                    source,
                    destination,
                    &files,
                    mount.seed_files,
                    mount.copy_dirs,
                    preserve,
                    &boundary,
                )?;
                seed::seed_credential_pairs(
                    source,
                    destination,
                    mount.credential_pairs,
                    &boundary,
                )?;
                seed::seed_sqlite_files(source, destination, mount.sqlite_seed_files, &boundary)?;
                seed::seed_configured_resources(
                    mount,
                    source,
                    destination,
                    home,
                    workspace,
                    &boundary,
                )?;
                if stopped {
                    seed::carry_sandbox_state(
                        source,
                        destination,
                        carried_state(mount),
                        &boundary,
                    )?;
                    let capability = input.context("stopped original lacks seed capability")?;
                    carried.extend(seed::carry_selected_sandbox_state(
                        source,
                        destination,
                        mount,
                        capability.resumes(),
                        capability.container_workdir(),
                        &boundary,
                    )?);
                }
                boundary.validate_source()?;
                Ok(())
            };
            if matches!(mode, ContentSeedMode::OwnedExtension) {
                seed::retry_source_change(&mut seed)?;
            } else {
                seed()?;
            }
        }
        for &(name, content) in mount.seed_files.iter().chain(mount.home_seed_files) {
            let path = Path::new(name);
            output.create_child(path.parent().unwrap_or(Path::new("")))?;
            // A retired original is carried forward with its old `.claude.json`,
            // whose `hasCompletedOnboarding` main may have written false; left as
            // is it strands the sandbox on the login picker. A JSON seed default
            // therefore enforces its own keys over a carried file, the default
            // winning for its keys while the file's other keys stay. A non-JSON
            // default or an absent file keeps the seed-once publish.
            if let Some(merged) = merged_json_seed(&output, path, content)? {
                output.publish_file(
                    path,
                    &mut merged.as_slice(),
                    std::fs::Permissions::from_mode(0o600),
                    true,
                    None,
                )?;
            } else {
                output.publish_file(
                    path,
                    &mut content.as_bytes(),
                    std::fs::Permissions::from_mode(0o600),
                    false,
                    None,
                )?;
            }
        }
    }
    output.sync()?;
    Ok(carried)
}

/// Tool names with a config mount, deduplicated in table order.
pub(crate) fn agent_config_mount_tools() -> Vec<&'static str> {
    let mut tools: Vec<&'static str> = Vec::new();
    for mount in AGENT_CONFIG_MOUNTS {
        if !tools.contains(&mount.tool_name) {
            tools.push(mount.tool_name);
        }
    }
    tools
}

/// Seed a newly isolated Codex home from the legacy shared sandbox only for the
/// credential that prior AoE versions migrated there. SQLite state is deliberately
/// not copied: it is process-local and is the source of the single-instance lock.
fn seed_legacy_codex_auth(sandbox_dir: &Path, home: &Path) {
    let auth = sandbox_dir.join("auth.json");
    let legacy_auth = home.join(".codex").join(SANDBOX_SUBDIR).join("auth.json");
    if auth.exists() || !legacy_auth.is_file() {
        return;
    }

    if let Err(e) = std::fs::copy(&legacy_auth, &auth) {
        tracing::warn!(target: "session.profile",
            "Failed to seed isolated Codex auth from {} to {}: {}",
            legacy_auth.display(), auth.display(), e
        );
    }
}

/// Sync a single agent's host config into its sandbox directory.
/// Handles config file sync, keychain credential extraction, and home-level seed files.
fn prepare_sandbox_dir(
    mount: &AgentConfigMount,
    home: &Path,
    instance_id: Option<&str>,
    fold: CredentialFold,
    session_config: &super::SessionConfig,
    workspace: &Path,
) -> Result<PathBuf> {
    let host_dir = home.join(mount.host_rel);
    let sandbox_dir = sandbox_dir_for(mount, home, instance_id)?;
    prepare_sandbox_dir_from(
        mount,
        host_dir,
        sandbox_dir,
        home,
        fold,
        session_config,
        workspace,
    )
}

fn prepare_sandbox_dir_from(
    mount: &AgentConfigMount,
    host_dir: PathBuf,
    sandbox_dir: PathBuf,
    home: &Path,
    fold: CredentialFold,
    session_config: &super::SessionConfig,
    workspace: &Path,
) -> Result<PathBuf> {
    let _admission = crate::migrations::v033_isolate_sandbox_content::guard_preparation(
        &host_dir,
        &sandbox_dir,
        mount.container_suffix,
    )?;
    seed_sandbox_dir_from(
        mount,
        host_dir,
        sandbox_dir,
        home,
        fold,
        session_config,
        workspace,
    )
}

fn seed_sandbox_dir_from(
    mount: &AgentConfigMount,
    host_dir: PathBuf,
    sandbox_dir: PathBuf,
    home: &Path,
    fold: CredentialFold,
    session_config: &super::SessionConfig,
    workspace: &Path,
) -> Result<PathBuf> {
    std::fs::create_dir_all(&sandbox_dir)?;

    if host_dir.exists() {
        // Codex writes `trusted_hash` into `[hooks.state]` of the sandbox
        // copy of `config.toml` when the user accepts a hook hash inside
        // the container; that copy is overwritten on each
        // `sync_agent_config` from the host. Snapshot here and restore
        // after the sync so accepted hashes survive the host-to-sandbox
        // refresh (the sandbox value wins by design, since trust is local
        // to the container). The codex config lock is released between
        // snapshot and restore; the sandbox path is process-private, so
        // no concurrent writer is expected.
        let preserved_codex_state = if agent_format_is_codex_json(mount.tool_name) {
            let sandbox_config = sandbox_dir.join("config.toml");
            crate::hooks::snapshot_codex_hooks_state(&sandbox_config)
                .inspect_err(|e| {
                    tracing::warn!(target: "session.profile",
                        "Failed to snapshot Codex [hooks.state] from {}: {}",
                        sandbox_config.display(),
                        e
                    );
                })
                .ok()
                .flatten()
        } else {
            None
        };

        seed::retry_source_change(|| {
            let boundary =
                NativeStateBoundary::new(&host_dir, mount, home, session_config, &sandbox_dir)?;
            sync_agent_config(
                &host_dir,
                &sandbox_dir,
                mount.copy_files,
                mount.seed_files,
                mount.copy_dirs,
                mount.preserve_files,
                &boundary,
            )?;
            seed::seed_credential_pairs(
                &host_dir,
                &sandbox_dir,
                mount.credential_pairs,
                &boundary,
            )?;
            seed::seed_sqlite_files(&host_dir, &sandbox_dir, mount.sqlite_seed_files, &boundary)?;
            seed::seed_configured_resources(
                mount,
                &host_dir,
                &sandbox_dir,
                home,
                workspace,
                &boundary,
            )?;
            boundary.validate_source()?;
            Ok(())
        })?;

        if mount.tool_name == "codex" {
            seed_legacy_codex_auth(&sandbox_dir, home);
        }

        if let Some(state) = preserved_codex_state {
            let sandbox_config = sandbox_dir.join("config.toml");
            if !sandbox_config.exists() {
                tracing::warn!(target: "session.profile",
                    "Codex [hooks.state] snapshotted but sandbox config.toml absent after sync at {}; trust block dropped",
                    sandbox_config.display()
                );
            } else if let Err(e) = crate::hooks::restore_codex_hooks_state(&sandbox_config, state) {
                tracing::warn!(target: "session.profile",
                    "Failed to restore Codex [hooks.state] in {}: {}",
                    sandbox_config.display(),
                    e
                );
            }
        }

        if mount.tool_name == "claude" {
            if let Err(e) = rewrite_claude_plugin_paths(&sandbox_dir, home) {
                tracing::warn!(target: "session.profile",
                    "Failed to rewrite Claude plugin paths in {}: {}",
                    sandbox_dir.display(),
                    e
                );
            }
        }
    } else {
        std::fs::create_dir_all(&sandbox_dir)?;
    }

    for &name in mount.shared_credential_files {
        if let Err(e) = sync_shared_credential(mount, &host_dir, &sandbox_dir, name, fold) {
            tracing::warn!(target: "session.profile",
                "Failed to sync shared credential {} for {}: {}", name, mount.host_rel, e);
        }
    }

    for &(filename, content) in mount.home_seed_files {
        let path = sandbox_dir.join(filename);
        if !path.exists() {
            std::fs::write(&path, content)?;
        }
    }

    let config = crate::session::config::Config::load_or_warn();
    let app_dir = crate::session::get_app_dir().ok();
    if let Some(app_dir) = app_dir {
        sync_managed_skills_into_sandbox(
            mount,
            &sandbox_dir,
            &app_dir,
            config.skills.auto_propagate,
        );
    }

    Ok(sandbox_dir)
}

/// Reconcile AoE-managed skills into this agent's sandbox skills dir (#3053).
///
/// Deliberately not folded into `copy_dirs`: that path is host-to-sandbox and
/// runs only on a sandbox's first launch, so a skill authored after the
/// container first ran would never reach it. Managed skills are a small,
/// AoE-owned tree, so reconciling them on every launch is cheap and safe in a
/// way re-copying the whole config tree is not. `copy_dirs` keeps doing its own
/// job of importing the user's hand-written host skills on first run.
///
/// The sandbox target is derived from the agent's own skills root, which must
/// live under the dir the sandbox mirrors. Codex reads `~/.agents/skills`, which
/// is outside its `.codex` mount, so it has no sandbox target and is host-only
/// for now.
/// `auto_propagate` is passed in rather than read here so the opt-in gate is
/// directly testable; a leaf function that reaches for global config cannot be.
fn sync_managed_skills_into_sandbox(
    mount: &AgentConfigMount,
    sandbox_dir: &Path,
    app_dir: &Path,
    auto_propagate: bool,
) {
    if !auto_propagate {
        return;
    }
    let Some(root) = crate::session::skills_model::primary_root_for_agent(mount.tool_name) else {
        return;
    };
    let Some(suffix) = root.relative_path.strip_prefix(mount.host_rel) else {
        tracing::debug!(target: "session.skills",
            agent = mount.tool_name,
            root = root.id,
            "skills root is outside the sandboxed config dir; host only"
        );
        return;
    };
    let target = sandbox_dir.join(suffix.trim_start_matches('/'));
    // Never replaces: a container starting must not overwrite a skill the user
    // put in the sandbox by hand.
    let outcomes = crate::session::skills_model::sync_skills_into(
        &target,
        app_dir,
        root.id,
        &crate::session::skills_model::SyncOptions::default(),
    );
    crate::session::skills_model::log_sync_outcomes(
        &format!("sandbox:{}", mount.tool_name),
        &outcomes,
    );
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
/// Where a sandboxed Pi publishes its conversation, inside the container.
///
/// Under the pi config mount rather than the hook base: that bind already
/// exists for every Pi container, so no new mount is needed and an image
/// created by an older AoE keeps working. Docker cannot add a mount to a
/// container that already exists, and reusing one is the normal path.
pub(crate) const PI_SIDECAR_DIR_IN_CONTAINER: &str = "/root/.pi/aoe-session";
pub(crate) const PRIME_AGENT_DIR_IN_CONTAINER: &str = "/root/.prime/agent";

fn install_session_extension_at(root: &Path, rel: &Path) -> Result<()> {
    let source = crate::session::instance::SESSION_IDENTITY_EXTENSION;
    let current = crate::session::read_file_no_follow(root, rel)?;
    if current.as_deref() != Some(source) {
        crate::session::replace_file_no_follow(root, rel, source.as_bytes())?;
    }
    Ok(())
}

/// Write the session-id extension where a sandboxed Pi discovers it.
pub(crate) fn install_pi_sandbox_extension_at(root: &Path) -> Result<()> {
    let rel = Path::new("agent")
        .join("extensions")
        .join("aoe-session-id.js");
    install_session_extension_at(root, &rel)
}

/// Stage the root-only identity extension inside Prime's private sandbox store.
pub(crate) fn install_prime_sandbox_extension_at(root: &Path) -> Result<()> {
    install_session_extension_at(root, Path::new("extensions/aoe-session-id.js"))
}
/// Which branch [`compute_volume_paths_with_resolve`] resolved the mounts through.
///
/// Reported rather than re-derived, so a caller acting on the difference between the
/// derived paths and reality reads the resolve that produced *those* paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountResolve {
    /// The mounts follow from the project's git layout.
    Resolved,
    /// The mounts fell through to `/workspace/{basename}`, which is where a worktree
    /// lands when its linkage is broken (#2414) and its container never mounted those
    /// paths. Also where a plain directory and a repo root legitimately land, but their
    /// workdir never moves, so nothing keys a decision on the difference.
    Fallthrough,
}

pub(crate) fn container_workdir_for(project_path: &str, pinned: Option<&str>) -> String {
    if let Some(pinned) = pinned {
        return pinned.to_owned();
    }
    compute_volume_paths(Path::new(project_path), project_path)
        .map(|(_, wd)| wd)
        .unwrap_or_else(|_| "/workspace".to_string())
}

pub(crate) fn compute_volume_paths(
    project_path: &Path,
    project_path_str: &str,
) -> Result<(Vec<VolumeMount>, String)> {
    let (volumes, workspace_path, _) =
        compute_volume_paths_with_resolve(project_path, project_path_str)?;
    Ok((volumes, workspace_path))
}

/// [`compute_volume_paths`] plus the branch it resolved through; see [`MountResolve`].
pub(crate) fn compute_volume_paths_with_resolve(
    project_path: &Path,
    project_path_str: &str,
) -> Result<(Vec<VolumeMount>, String, MountResolve)> {
    // Only look for a main repo when the project path itself has a `.git` entry:
    // a repo has a directory, a worktree a file holding a gitdir pointer. Without
    // the check `Repository::discover` walks up into an unrelated ancestor repo
    // (a dotfile-managed home) and mounts the whole of $HOME into the container.
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
                        MountResolve::Resolved,
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
                        MountResolve::Resolved,
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
        MountResolve::Fallthrough,
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
pub(crate) fn compute_workspace_volume_paths(
    workspace_path: &Path,
    ws_info: &crate::session::WorkspaceInfo,
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

/// Whether the agent `tool` resolves to under `profile` shares a credential
/// file across its sandboxes.
pub(crate) fn agent_shares_credential_file(
    profile: &str,
    tool: &str,
    command: Option<&str>,
) -> bool {
    let profile_config = super::profile_config::resolve_config_or_warn(profile);
    resolve_active_agent(tool, command, &profile_config.session)
        .is_some_and(|agent| agent_mounts_share_credential_file(agent.name))
}

fn agent_mounts_share_credential_file(agent: &str) -> bool {
    AGENT_CONFIG_MOUNTS
        .iter()
        .any(|mount| mount.tool_name == agent && !mount.shared_credential_files.is_empty())
}

/// Re-sync the current instance's physically isolated agent config.
pub(crate) fn refresh_agent_configs_for_instance(
    profile: &str,
    instance_id: &str,
    tool: &str,
    command: Option<&str>,
    fold: CredentialFold,
    workspace: &Path,
) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let profile_config = super::profile_config::resolve_config_or_warn(profile);
    let Some(agent) = resolve_active_agent(tool, command, &profile_config.session) else {
        return;
    };
    let declared = profile_config.session.agent_config_dir_for(tool, &home);
    for mount in AGENT_CONFIG_MOUNTS
        .iter()
        .filter(|mount| mount.tool_name == agent.name)
    {
        let result = match declared.as_ref() {
            Some(directory) => prepare_sandbox_dir_from(
                mount,
                directory.clone(),
                directory.join(SANDBOX_PRIVATE_SUBDIR).join(instance_id),
                &home,
                fold,
                &profile_config.session,
                workspace,
            ),
            None => prepare_sandbox_dir(
                mount,
                &home,
                Some(instance_id),
                fold,
                &profile_config.session,
                workspace,
            ),
        };
        match result {
            Ok(sandbox_dir) => {
                if agent.name == "codex"
                    && profile_config.session.agent_status_hooks
                    && should_refresh_codex_hooks(mount, &sandbox_dir, &home)
                {
                    refresh_codex_sandbox_hooks(mount, &sandbox_dir, &profile_config);
                }
            }
            Err(error) => tracing::warn!(target: "session.profile",
                "Failed to refresh isolated {} config for {}: {}",
                agent.name, instance_id, error
            ),
        }
    }
}
fn agent_format_is_codex_json(tool_name: &str) -> bool {
    crate::agents::get_agent(tool_name)
        .and_then(|agent| agent.hook_config.as_ref())
        .is_some_and(|config| config.format == crate::agents::HookFormat::CodexJson)
}

fn should_refresh_codex_hooks(mount: &AgentConfigMount, sandbox_dir: &Path, home: &Path) -> bool {
    if !agent_format_is_codex_json(mount.tool_name) {
        return false;
    }
    let host_hooks = home.join(mount.host_rel).join("hooks.json");
    let sandbox_hooks = sandbox_dir.join("hooks.json");
    host_hooks.exists() || sandbox_hooks.exists()
}

fn refresh_codex_sandbox_hooks(
    mount: &AgentConfigMount,
    sandbox_dir: &Path,
    profile_config: &super::Config,
) {
    let Some(agent) = crate::agents::get_agent(mount.tool_name) else {
        return;
    };

    let hooks_path = sandbox_dir.join("hooks.json");
    let events = match crate::agents::resolved_hook_events(agent, profile_config) {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!(target: "session.profile",
                "Failed to resolve Codex hooks while refreshing sandbox config {}: {}",
                hooks_path.display(),
                e
            );
            return;
        }
    };
    if let Err(e) = crate::hooks::install_codex_json_hooks(
        &hooks_path,
        &events,
        crate::hooks::HookInstallTarget::Sandbox,
    ) {
        tracing::warn!(target: "session.profile",
            "Failed to refresh Codex hooks in sandbox config {}: {}",
            hooks_path.display(),
            e
        );
    }
}

/// Pre-trust the container workspace in the config `mount`'s agent will read.
///
/// Codex and Gemini stay gated on YOLO mode: their prompts guard approvals the
/// user has already opted out of there, and outside YOLO the prompt is the
/// approval. Claude Code's dialog is not an approval gate but a startup gate,
/// so it blocks a non-YOLO session just as hard and is applied unconditionally.
///
/// `staged` marks a directory AoE writes for one container. A directory named
/// by `session.agent_config_dir` is the user's own, so Gemini takes the
/// per-path entry there rather than the wholesale feature disable, which would
/// outlive the session in a config the user also runs on the host.
fn apply_folder_trust_config(
    mount: &AgentConfigMount,
    sandbox_dir: &Path,
    container_workspace_path: &str,
    is_yolo_mode: bool,
    staged: bool,
) -> Result<()> {
    match (mount.tool_name, mount.host_rel) {
        ("codex", ".codex") if is_yolo_mode => crate::hooks::trust_codex_project(
            &sandbox_dir.join("config.toml"),
            container_workspace_path,
            // Bind-mounted into the container, which can plant a link here.
            crate::hooks::SymlinkPolicy::Never,
        ),
        ("gemini", ".gemini") if is_yolo_mode && staged => {
            crate::hooks::disable_gemini_folder_trust(&sandbox_dir.join("settings.json"))
        }
        ("gemini", ".gemini") if is_yolo_mode => crate::hooks::trust_gemini_project(
            &sandbox_dir.join("trustedFolders.json"),
            container_workspace_path,
            // Bind-mounted into the container, which can plant a link here.
            crate::hooks::SymlinkPolicy::Never,
        ),
        // The same host file is bind-mounted at both `$CLAUDE_CONFIG_DIR/.claude.json`
        // (via the config dir) and `~/.claude.json` (via `home_seed_files`), so one
        // write covers whichever path this Claude Code build reads.
        ("claude", ".claude") => crate::hooks::trust_claude_project(
            &sandbox_dir.join(".claude.json"),
            container_workspace_path,
            // Bind-mounted into the container, which can plant a link here.
            crate::hooks::SymlinkPolicy::Never,
        ),
        _ => Ok(()),
    }
}

pub(crate) fn ensure_folder_trust_config_for_active_agent(
    tool: &str,
    command: Option<&str>,
    profile: &str,
    instance_id: &str,
    container_workspace_path: &str,
    is_yolo_mode: bool,
) {
    let Some(home) = dirs::home_dir() else {
        return;
    };

    let resolved_profile = super::effective_profile(profile);
    let config = super::profile_config::resolve_config_or_warn(&resolved_profile);
    let session_config = config.session;
    let active_agent = resolve_active_agent(tool, command, &session_config);
    let config_tool = active_agent.map_or(tool, |agent| agent.name);
    // A session whose agent reads its own config dir (a wrapper exporting
    // CLAUDE_CONFIG_DIR, say) is seeded there instead: the staged directory
    // below is mounted at the built-in default, which that agent never opens.
    let agent_config_dir = session_config.agent_config_dir_for(tool, &home);

    for mount in AGENT_CONFIG_MOUNTS
        .iter()
        .filter(|m| m.tool_name == config_tool)
    {
        // The instance segment is the ownership boundary. The mounted path is
        // still the agent's config root inside this one container.

        let sandbox_dir = match agent_config_dir.as_ref() {
            Some(dir) => Ok(dir.join(SANDBOX_PRIVATE_SUBDIR).join(instance_id)),
            None => sandbox_dir_for(mount, &home, Some(instance_id)),
        };
        let sandbox_dir = match sandbox_dir {
            Ok(dir) => dir,
            Err(e) => {
                tracing::warn!(target: "session.profile",
                    "Failed to resolve sandbox folder trust config for {}: {}", mount.tool_name, e
                );
                continue;
            }
        };
        if let Err(e) = std::fs::create_dir_all(&sandbox_dir)
            .with_context(|| format!("creating sandbox config dir {}", sandbox_dir.display()))
            .and_then(|_| {
                apply_folder_trust_config(
                    mount,
                    &sandbox_dir,
                    container_workspace_path,
                    is_yolo_mode,
                    agent_config_dir.is_none(),
                )
            })
        {
            tracing::warn!(target: "session.profile",
                "Failed to apply sandbox folder trust config for {} at {}: {}",
                mount.tool_name,
                sandbox_dir.display(),
                e
            );
        }
    }
}

pub(crate) fn resolve_active_agent(
    tool: &str,
    command: Option<&str>,
    session_config: &super::SessionConfig,
) -> Option<&'static crate::agents::AgentDef> {
    resolve_executed_agent(tool, command, session_config).or_else(|| {
        // Legacy status-only provisioning never grants native conversation authority.
        if session_config.agent_execution_as.contains_key(tool) {
            return None;
        }
        match session_config.agent_detect_as.get(tool) {
            Some(name) => crate::agents::get_agent(name),
            None => crate::agents::get_agent(tool),
        }
    })
}

fn resolve_executed_agent(
    tool: &str,
    command: Option<&str>,
    session_config: &super::SessionConfig,
) -> Option<&'static crate::agents::AgentDef> {
    let command = command
        .or_else(|| session_config.custom_agents.get(tool).map(String::as_str))
        .or_else(|| crate::agents::get_agent(tool).map(|agent| agent.binary))
        .unwrap_or(tool);
    crate::session::Instance::execution_agent_for(tool, command, session_config).ok()
}

/// The identity a sandbox container's agent config mounts are built for. Mounts
/// follow the resolved agent and store roots follow the tool, so an alias
/// carries both. Fails rather than defaulting: defaults drop config-only
/// aliases, which would misread a valid container as built for another agent.
pub(crate) fn container_agent_identity(
    tool: &str,
    command: Option<&str>,
    profile: &str,
) -> Result<String> {
    let resolved_profile = super::effective_profile(profile);
    let session_config = super::profile_config::resolve_config(&resolved_profile)?.session;
    // A mount identity, never conversation authority: resolve the agent from
    // the same inputs as the label writer in `build_container_config`, so a
    // reused container is not rebuilt on every launch.
    Ok(agent_identity(
        tool,
        resolve_active_agent(tool, command, &session_config).map_or(tool, |a| a.name),
    ))
}

fn agent_identity(tool: &str, config_tool: &str) -> String {
    if tool == config_tool {
        tool.to_string()
    } else {
        format!("{tool}:{config_tool}")
    }
}

/// The managed Codex home for an instance, when its resolved agent uses Codex
/// configuration. This is also passed to `docker exec`, so pre-isolation
/// containers use their private child directory without being recreated.
pub(crate) fn managed_codex_home(
    tool: &str,
    command: Option<&str>,
    profile: &str,
    instance_id: &str,
) -> Result<Option<String>> {
    let resolved_profile = super::effective_profile(profile);
    let session_config = super::profile_config::resolve_config_or_warn(&resolved_profile).session;
    managed_codex_home_from_config(tool, command, &session_config, instance_id)
}

pub(crate) fn managed_codex_home_from_config(
    tool: &str,
    command: Option<&str>,
    session_config: &super::SessionConfig,
    instance_id: &str,
) -> Result<Option<String>> {
    crate::session::validate_instance_id(instance_id).map_err(|e| {
        anyhow::anyhow!("refusing to build Codex home for unsafe AOE_INSTANCE_ID: {e}")
    })?;
    let config_tool = resolve_active_agent(tool, command, session_config).map_or(tool, |a| a.name);
    Ok((config_tool == "codex").then(|| format!("/root/.codex/{instance_id}")))
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
    command: Option<&'a str>,
    /// The agent name a user selected via the agent's selected-agent flag (e.g.
    /// Kiro's `--agent NAME`), if any. When set, sidecar status hooks are
    /// installed into that agent's sandbox config file rather than the
    /// standalone hooks agent, mirroring the host path so a sandboxed session
    /// running a user's own agent still reports status (Kiro has no global
    /// hooks). `None` for the default / no selection.
    selected_agent: Option<&'a str>,
    /// How the launch treats the credential file the agent's sandboxes share.
    credential_fold: CredentialFold,
}

impl<'a> ContainerAgentSelection<'a> {
    pub(crate) fn new(tool: &'a str, command: Option<&'a str>) -> Self {
        Self {
            tool,
            command,
            selected_agent: None,
            credential_fold: CredentialFold::Freshest,
        }
    }

    /// Set how the launch treats the shared credential file (see
    /// [`CredentialFold`]).
    pub(crate) fn with_credential_fold(mut self, fold: CredentialFold) -> Self {
        self.credential_fold = fold;
        self
    }

    /// Set the user-selected agent name (see [`Self::selected_agent`]).
    pub(crate) fn with_selected_agent(mut self, selected_agent: Option<&'a str>) -> Self {
        self.selected_agent = selected_agent;
        self
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
    workspace_info: Option<&crate::session::WorkspaceInfo>,
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

/// The volume names a move stranded: the names the paths under `config.working_dir`
/// carried when the container was created at `previous_workdir` (#3742).
///
/// Scoped to the mounts that provably moved. A mount whose container path survives the
/// move keeps its volume even when this run's config fails to resolve it, which is what
/// a glob whose directory is absent from the host looks like.
///
/// Empty unless the mounts are established to have moved. `previous_workdir` is that
/// evidence, pinned on `SandboxInfo::container_workdir` at create for #2414: absent (a
/// session that never had a container, or an attach, which clears the pin) or equal to
/// this config's, and nothing moved. A resolve that fell through to the collapsed
/// `/workspace/{basename}` withholds it too, since there the workdir is provisional and
/// the apparent move may be nothing but that fallthrough.
pub(crate) fn stranded_named_ignore_volumes(
    config: &ContainerConfig,
    instance_id: &str,
    previous_workdir: Option<&str>,
) -> Vec<String> {
    let Some(previous) = previous_workdir else {
        return Vec::new();
    };
    if !config.named_ignore_volumes_authoritative || previous == config.working_dir {
        return Vec::new();
    }
    // A remapped name that the create is about to mount is a live cache, not a strand:
    // the previous workdir can be the mount root of a mount that survived.
    let live: std::collections::HashSet<&str> = config
        .named_ignore_volumes
        .iter()
        .map(|volume| volume.volume_name.as_str())
        .collect();
    let moved = format!("{}/", config.working_dir);
    config
        .named_ignore_volumes
        .iter()
        .filter_map(|volume| volume.container_path.strip_prefix(moved.as_str()))
        .map(|relative| named_volume_for(instance_id, &format!("{}/{}", previous, relative)))
        .filter(|name| !live.contains(name.as_str()))
        .collect()
}

/// Reject managed agent paths that would bypass the sandbox's private mounts.
fn validate_managed_container_environment(
    environment: &[EnvEntry],
    active_agent: Option<&crate::agents::AgentDef>,
    container_home: &str,
) -> Result<()> {
    let require_path = |key: &str, expected: &str| -> Result<()> {
        if let Some(actual) = environment
            .iter()
            .find(|entry| entry.key() == key)
            .map(EnvEntry::value)
        {
            if Path::new(actual) != Path::new(expected) {
                anyhow::bail!(
                    "sandbox environment sets {key}={actual}, but AoE mounts the managed agent config at {expected}; remove this sandbox environment or per-session override, or set it to {expected}"
                );
            }
        }
        Ok(())
    };

    require_path("HOME", container_home)?;
    if let Some(agent) = active_agent {
        for &(key, expected) in agent.container_env.iter().filter(|(key, _)| {
            matches!(
                *key,
                "CLAUDE_CONFIG_DIR" | "CURSOR_CONFIG_DIR" | "PRIME_AGENT_CODING_AGENT_DIR"
            )
        }) {
            require_path(key, expected)?;
        }
    }
    Ok(())
}

/// Build a sandboxed container config with the selected profile's overrides.
/// An empty profile uses the configured default.
pub(crate) fn build_container_config(
    project_path_str: &str,
    sandbox_info: &SandboxInfo,
    agent_selection: ContainerAgentSelection<'_>,
    is_yolo_mode: bool,
    instance_id: &str,
    workspace_info: Option<&crate::session::WorkspaceInfo>,
    profile: &str,
) -> Result<ContainerConfig> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;
    crate::session::validate_instance_id(instance_id).map_err(|e| {
        anyhow::anyhow!("refusing to build sandbox config for unsafe AOE_INSTANCE_ID: {e}")
    })?;

    let project_path = Path::new(project_path_str);
    let resolved_profile = super::effective_profile(profile);
    let profile_config = super::profile_config::resolve_config_or_warn(&resolved_profile);
    let profile_session_config = &profile_config.session;
    let active_agent = resolve_active_agent(
        agent_selection.tool,
        agent_selection.command,
        profile_session_config,
    );
    let config_tool = active_agent.map_or(agent_selection.tool, |agent| agent.name);

    // A workspace mounts its own dir plus every main repo, a bare-repo worktree
    // mounts the whole bare repo with working_dir inside it, and a sibling
    // worktree mounts the main repo and the worktree separately. A workspace
    // resolve is always Resolved: it derives mounts from each repo's stored
    // `main_repo_path` and never falls back to `find_main_repo`.
    let (project_volumes, workspace_path, mount_resolve) = if let Some(ws_info) = workspace_info {
        let (volumes, path) = compute_workspace_volume_paths(project_path, ws_info)?;
        (volumes, path, MountResolve::Resolved)
    } else {
        compute_volume_paths_with_resolve(project_path, project_path_str)?
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
    // Pin the home used to place agent hooks. Container images may declare a
    // different ENV HOME, so absence is not proof that `/root` is effective.
    if !environment.iter().any(|entry| entry.key() == "HOME") {
        environment.push(EnvEntry::Literal {
            key: "HOME".to_string(),
            value: CONTAINER_HOME.to_string(),
        });
    }
    validate_managed_container_environment(&environment, active_agent, CONTAINER_HOME)?;
    if !environment.iter().any(|entry| entry.key() == "CODEX_HOME") {
        if let Some(codex_home) = managed_codex_home(
            agent_selection.tool,
            agent_selection.command,
            profile,
            instance_id,
        )? {
            environment.push(EnvEntry::Literal {
                key: "CODEX_HOME".to_string(),
                value: codex_home,
            });
        }
    }

    // Bind-mount the session's managed artifact dir and point the agent at it
    // via AOE_ARTIFACT_DIR, so screenshots/status files the agent writes land
    // in the host dir the dashboard can serve. See #2587.
    if let Ok(host_artifact_dir) = crate::session::artifacts::session_artifact_dir(instance_id) {
        volumes.push(VolumeMount {
            host_path: host_artifact_dir.to_string_lossy().to_string(),
            container_path: crate::session::artifacts::CONTAINER_ARTIFACT_DIR.to_string(),
            read_only: false,
        });
        environment.push(EnvEntry::Literal {
            key: crate::session::artifacts::ARTIFACT_DIR_ENV.to_string(),
            value: crate::session::artifacts::CONTAINER_ARTIFACT_DIR.to_string(),
        });
    }

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

    // Mount GCP credentials at the well-known ADC path for Claude+Vertex.
    // `CLAUDE_CODE_USE_VERTEX` is Claude-specific, so a globally exported flag
    // must not hand GCP creds to other agents. `GOOGLE_APPLICATION_CREDENTIALS`
    // is not forwarded: client libraries find the well-known path themselves.
    if agent_selection.tool == "claude" && crate::session::environment::host_vertex_enabled() {
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

    // Bind only the resolved agent's config, including declared custom roots.
    let declared_config_dir =
        profile_session_config.agent_config_dir_for(agent_selection.tool, &home);
    // Agent definitions are in AGENT_CONFIG_MOUNTS. Add new agents there.
    let mut active_sandbox_config: Option<(&AgentConfigMount, PathBuf)> = None;
    let mut shared_credential_mounts = Vec::new();
    let mut identity_publisher_installed = false;
    let mut identity_publisher_path: Option<(PathBuf, String)> = None;
    let mut identity_output_path: Option<(PathBuf, String)> = None;
    let content_roots = sandbox_content_roots(
        agent_selection.tool,
        agent_selection.command,
        profile_session_config,
        &home,
        instance_id,
    )?;
    let _content_admission = crate::migrations::v033_isolate_sandbox_content::ensure_fresh_content(
        &crate::session::get_app_dir()?,
        &home,
        instance_id,
        agent_selection.tool,
        &content_roots,
        &profile_config,
        Path::new(&workspace_path),
    )?;
    for mount in AGENT_CONFIG_MOUNTS
        .iter()
        .filter(|m| m.tool_name == config_tool)
    {
        let container_path = agent_config_container_path(mount, CONTAINER_HOME, &environment);

        let sandbox_dir = match declared_config_dir.as_ref() {
            Some(directory) => prepare_sandbox_dir_from(
                mount,
                directory.clone(),
                directory.join(SANDBOX_PRIVATE_SUBDIR).join(instance_id),
                &home,
                agent_selection.credential_fold,
                profile_session_config,
                Path::new(&workspace_path),
            ),
            None => prepare_sandbox_dir(
                mount,
                &home,
                Some(instance_id),
                agent_selection.credential_fold,
                profile_session_config,
                Path::new(&workspace_path),
            ),
        }
        .with_context(|| format!("preparing isolated {} configuration", mount.host_rel))?;
        active_sandbox_config = Some((mount, sandbox_dir.clone()));

        tracing::debug!(target: "session.profile",
            "Sandbox dir ready for {}, binding {} -> {}",
            mount.host_rel,
            sandbox_dir.display(),
            container_path
        );
        for &name in mount.shared_credential_files {
            let Some(shared) = shared_credential_path(&sandbox_dir, name) else {
                continue;
            };
            if shared.is_file() {
                let container_path = format!("{container_path}/{name}");
                shared_credential_mounts.push(container_path.clone());
                volumes.push(VolumeMount {
                    host_path: shared.to_string_lossy().to_string(),
                    container_path,
                    read_only: false,
                });
            }
        }
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

    let status_hooks_enabled = profile_session_config.agent_status_hooks;
    if let Some(agent) = active_agent {
        let identity_hooks_required = agent.hook_config.as_ref().is_some_and(|config| {
            config
                .events
                .iter()
                .any(|event| event.identity_field.is_some())
        }) || agent.sidecar_hooks.as_ref().is_some_and(|config| {
            config
                .events
                .iter()
                .any(|event| event.identity_field.is_some())
        });
        if agent.sidecar_hooks.is_some() || agent.hook_config.is_some() {
            // Sidecar agents (hermes YAML, kiro per-agent JSON) use schemas the
            // generic hook_config path below cannot emit; they install through
            // their SidecarHooks installer at the sandbox config subpath.
            if status_hooks_enabled || identity_hooks_required {
                crate::session::validate_instance_id(instance_id).map_err(|e| {
                    anyhow::anyhow!(
                        "refusing to mount hook directory: AOE_INSTANCE_ID failed validation: {e}"
                    )
                })?;
                match crate::hooks::ensure_instance_dir_path(instance_id) {
                    Ok(hook_dir) => {
                        let container_hook_path = format!(
                            "{}/{instance_id}",
                            crate::hooks::HOOK_STATUS_BASE_IN_CONTAINER
                        );
                        identity_output_path =
                            Some((hook_dir.clone(), container_hook_path.clone()));
                        volumes.push(VolumeMount {
                            host_path: hook_dir.to_string_lossy().to_string(),
                            container_path: container_hook_path,
                            read_only: false,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(target: "session.profile",
                            "Hook directory unavailable, skipping bind-mount; \
                             agent boots without status hooks (pane detection takes over): {e:#}");
                    }
                }
            }

            if let Some(sidecar) = &agent.sidecar_hooks {
                let mut events = match crate::agents::resolved_sidecar_hook_events(
                    agent,
                    &profile_config,
                ) {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!(target: "session.profile", "Failed to resolve {} hooks in sandbox: {}", agent.name, e);
                        Vec::new()
                    }
                };
                if !status_hooks_enabled {
                    events.retain(|event| event.identity_field.is_some());
                    for event in &mut events {
                        event.status = None;
                    }
                }
                let config_file =
                    active_sandbox_config
                        .as_ref()
                        .and_then(|(mount, sandbox_dir)| {
                            let prefix = Path::new(mount.host_rel).join(SANDBOX_SUBDIR);
                            let relative = Path::new(sidecar.sandbox_config_subpath)
                                .strip_prefix(prefix)
                                .ok()?;
                            sidecar
                                .selected_agent_hooks
                                .as_ref()
                                .zip(agent_selection.selected_agent)
                                .and_then(|(selected, name)| {
                                    let agents_dir = sandbox_dir.join(relative.parent()?);
                                    Some((selected.resolve_config_file)(&agents_dir, name))
                                })
                                .or_else(|| Some(sandbox_dir.join(relative)))
                        });
                if let Some(config_file) = config_file {
                    match (sidecar.install)(
                        &config_file,
                        crate::hooks::HookInstallTarget::Sandbox,
                        &events,
                    ) {
                        Ok(()) => {
                            let publishes_identity =
                                events.iter().any(|event| event.identity_field.is_some());
                            identity_publisher_installed |= publishes_identity;
                            if publishes_identity {
                                if let Some((mount, sandbox_dir)) = active_sandbox_config.as_ref() {
                                    if let Ok(relative) = config_file.strip_prefix(sandbox_dir) {
                                        identity_publisher_path = Some((
                                            config_file.clone(),
                                            Path::new(&agent_config_container_path(
                                                mount,
                                                CONTAINER_HOME,
                                                &environment,
                                            ))
                                            .join(relative)
                                            .to_string_lossy()
                                            .into_owned(),
                                        ));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(target: "session.profile", "Failed to install {} hooks in sandbox: {}", agent.name, e)
                        }
                    }
                } else {
                    tracing::warn!(target: "session.profile", "No sandbox config mount for {} hooks", agent.name);
                }
            } else if let Some(hook_cfg) = &agent.hook_config {
                let mut events = match crate::agents::resolved_hook_events(agent, &profile_config) {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!(target: "session.profile", "Failed to resolve hooks in sandbox config: {}", e);
                        Vec::new()
                    }
                };
                if !status_hooks_enabled {
                    events.retain(|event| event.identity_field.is_some());
                    for event in &mut events {
                        event.status = None;
                    }
                }
                let settings_file =
                    active_sandbox_config
                        .as_ref()
                        .and_then(|(mount, sandbox_dir)| {
                            Path::new(hook_cfg.settings_rel_path)
                                .strip_prefix(mount.host_rel)
                                .ok()
                                .map(|relative| sandbox_dir.join(relative))
                        });
                if let Some(settings_file) = settings_file {
                    let result = match hook_cfg.format {
                        crate::agents::HookFormat::CodexJson => {
                            crate::hooks::install_codex_json_hooks(
                                &settings_file,
                                &events,
                                crate::hooks::HookInstallTarget::Sandbox,
                            )
                        }
                        crate::agents::HookFormat::JsonSettings => crate::hooks::install_hooks(
                            &settings_file,
                            &events,
                            crate::hooks::HookInstallTarget::Sandbox,
                        ),
                    };
                    match result {
                        Ok(()) => {
                            let publishes_identity =
                                events.iter().any(|event| event.identity_field.is_some());
                            identity_publisher_installed |= publishes_identity;
                            if publishes_identity {
                                if let Some((mount, sandbox_dir)) = active_sandbox_config.as_ref() {
                                    if let Ok(relative) = settings_file.strip_prefix(sandbox_dir) {
                                        identity_publisher_path = Some((
                                            settings_file.clone(),
                                            Path::new(&agent_config_container_path(
                                                mount,
                                                CONTAINER_HOME,
                                                &environment,
                                            ))
                                            .join(relative)
                                            .to_string_lossy()
                                            .into_owned(),
                                        ));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(target: "session.profile", "Failed to install hooks in sandbox config: {}", e)
                        }
                    }
                } else {
                    tracing::warn!(target: "session.profile", "No sandbox config mount for {} hooks", agent.name);
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

    // Folder trust goes through the shared registry so the create path and the
    // attach/restart path in `instance/container.rs` cannot drift (issue #472).
    // Called outside the YOLO gate because the registry decides per agent which
    // prompts are approval gates and which merely block startup.
    ensure_folder_trust_config_for_active_agent(
        agent_selection.tool,
        agent_selection.command,
        profile,
        instance_id,
        &workspace_path,
        is_yolo_mode,
    );

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

    let named_ignore_volumes_authoritative = mount_resolve == MountResolve::Resolved;

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

    if identity_publisher_installed {
        let candidate = ContainerConfig {
            volumes: deduped.clone(),
            environment: environment.clone(),
            ..ContainerConfig::default()
        };
        identity_publisher_installed = candidate.uses_default_container_home()
            && identity_publisher_path
                .as_ref()
                .is_some_and(|(host, container)| {
                    candidate.path_is_mounted(host, Path::new(container), false)
                })
            && identity_output_path
                .as_ref()
                .is_some_and(|(host, container)| {
                    candidate.path_is_mounted(host, Path::new(container), true)
                });
    }

    Ok(ContainerConfig {
        working_dir: workspace_path,
        volumes: deduped,
        anonymous_volumes,
        named_ignore_volumes,
        named_ignore_volumes_authoritative,
        environment,
        cpu_limit: sandbox_config.cpu_limit,
        memory_limit: sandbox_config.memory_limit,
        port_mappings: sandbox_config.port_mappings.clone(),
        network: sanitize_network(sandbox_config.network.as_deref()),
        selinux_relabel: sandbox_config.selinux_relabel,
        identity_publisher_installed,
        shared_credential_mounts,
        agent_tool: agent_identity(agent_selection.tool, config_tool),
        run_policy: RunPolicy {
            privileged: sandbox_config.privileged,
            cap_add: sandbox_config.cap_add.clone(),
            cap_drop: sandbox_config.cap_drop.clone(),
            security_opt: sandbox_config.security_opt.clone(),
            extra_run_args: sandbox_config.extra_run_args.clone(),
        },
    })
}

/// Normalize the configured `sandbox.network` into the value passed to
/// `--network`. Unset and `bridge` both map to `None` (runtime default, no
/// flag); `none` is canonicalized to lowercase. Anything the settings
/// validator rejects (`host`, namespace-sharing forms like `container:` and
/// `ns:`, malformed names) is dropped with a warning here too, because
/// repo/profile TOML is only type-checked, not value-validated (a repo config
/// could set it directly, the concern raised in #2706). Valid named networks
/// pass through verbatim.
fn sanitize_network(network: Option<&str>) -> Option<String> {
    let value = network.map(str::trim).filter(|v| !v.is_empty())?;
    if value.eq_ignore_ascii_case("bridge") {
        return None;
    }
    if value.eq_ignore_ascii_case("host") {
        tracing::warn!(
            target: "session.profile",
            "Ignoring sandbox.network = \"host\": host network mode defeats sandbox isolation"
        );
        return None;
    }
    // Canonicalize the reserved keyword so a "None"/"NONE" config value still
    // emits the runtime's lowercase `none` mode rather than a nonexistent
    // network name. Named networks keep their original case (they are
    // user-defined and case-sensitive).
    if value.eq_ignore_ascii_case("none") {
        return Some("none".to_string());
    }
    if let Err(reason) = crate::session::validate_network_format(value) {
        tracing::warn!(
            target: "session.profile",
            "Ignoring sandbox.network = {value:?}: {reason}"
        );
        return None;
    }
    Some(value.to_string())
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
    use crate::hooks::test_support::BaseGuard;
    use std::fs;
    use tempfile::TempDir;

    struct IsolatedHome {
        _home: crate::session::test_support::HomeGuard,
        home: TempDir,
        _base_dir: TempDir,
        _base: BaseGuard,
    }

    impl IsolatedHome {
        fn new() -> Self {
            let (base, _, base_dir) = BaseGuard::ready();
            let home = TempDir::new().unwrap();
            fs::create_dir_all(home.path().join(".local/share")).unwrap();
            let home_guard = crate::session::test_support::isolate_home(home.path());
            Self {
                _home: home_guard,
                home,
                _base_dir: base_dir,
                _base: base,
            }
        }

        fn path(&self) -> &Path {
            self.home.path()
        }
    }

    /// `build_container_config` with the arguments these tests rarely vary.
    struct Build<'a> {
        selection: ContainerAgentSelection<'a>,
        info: crate::session::instance::SandboxInfo,
        yolo: bool,
        instance: &'a str,
        profile: &'a str,
    }

    impl<'a> Build<'a> {
        fn select(selection: ContainerAgentSelection<'a>) -> Self {
            Self {
                selection,
                info: test_sandbox_info(),
                yolo: false,
                instance: "test-instance-id",
                profile: "",
            }
        }

        fn new(tool: &'a str) -> Self {
            Self::select(ContainerAgentSelection::new(tool, None))
        }

        fn info(mut self, info: SandboxInfo) -> Self {
            self.info = info;
            self
        }

        fn yolo(mut self, yolo: bool) -> Self {
            self.yolo = yolo;
            self
        }

        fn profile(mut self, profile: &'a str) -> Self {
            self.profile = profile;
            self
        }

        fn instance(mut self, instance: &'a str) -> Self {
            self.instance = instance;
            self
        }

        fn run(self, project: &Path) -> Result<ContainerConfig> {
            build_container_config(
                project.to_str().unwrap(),
                &self.info,
                self.selection,
                self.yolo,
                self.instance,
                None,
                self.profile,
            )
        }
    }

    #[test]
    #[serial_test::serial]
    fn sandboxed_pi_config_mount_backs_the_sidecar_and_extension() {
        let _home = IsolatedHome::new();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();
        let instance_id = "pisandboxbind001";
        let config = Build::new("pi")
            .instance(instance_id)
            .run(project_dir.path())
            .unwrap();

        let bind = config
            .volumes
            .iter()
            .find(|v| v.container_path == "/root/.pi")
            .expect("the Pi config dir must be bound at /root/.pi");
        let sandbox_dir = std::path::PathBuf::from(&bind.host_path);
        assert!(!bind.read_only, "the pane publishes into this bind");

        // Both container paths therefore resolve under that host directory.
        assert!(PI_SIDECAR_DIR_IN_CONTAINER.starts_with("/root/.pi/"));
        let host_sidecar = sandbox_dir.join(
            PI_SIDECAR_DIR_IN_CONTAINER
                .strip_prefix("/root/.pi/")
                .unwrap(),
        );
        assert!(host_sidecar.starts_with(&sandbox_dir));

        install_pi_sandbox_extension_at(&sandbox_dir).expect("install the extension");
        assert!(
            sandbox_dir
                .join("agent/extensions/aoe-session-id.js")
                .is_file(),
            "the extension must land where the container discovers it"
        );

        // Nothing this branch adds may introduce a mount: a container that
        // already exists cannot gain one.
        assert!(
            !config
                .volumes
                .iter()
                .any(|v| v.container_path.contains("aoe-pi-session-id")),
            "no per-extension mount may be required"
        );
    }

    /// The extension read must go through the anchored walk, not a stat of the
    /// pathname followed by a second open of it. With `agent/extensions`
    /// swapped for a link to a directory that already holds the current
    /// extension, the check-then-open shape stats the host copy through the
    /// link, finds it equal, and reports success without ever looking inside
    /// the bind; the walk refuses the linked component instead, and the write
    /// behind it fails closed.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn install_pi_sandbox_extension_refuses_a_linked_parent() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("pi-sandbox");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join("aoe-session-id.js"),
            crate::session::instance::SESSION_IDENTITY_EXTENSION,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("agent")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("agent").join("extensions")).unwrap();

        assert!(
            install_pi_sandbox_extension_at(&root).is_err(),
            "a linked parent must fail the install, not read the host copy through it"
        );
        let outside_entries: Vec<_> = std::fs::read_dir(&outside)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            outside_entries.len(),
            1,
            "nothing may be written through the link: {outside_entries:?}"
        );
    }

    /// Unset, blank and `bridge` mean the runtime default; `host` and the
    /// namespace-sharing forms are dropped here too, because repo/profile TOML
    /// is only type-checked, not value-validated (#2706); `none` is lowercased
    /// and a named network passes through trimmed.
    #[test]
    fn sanitize_network_canonicalizes_or_drops_every_form() {
        let cases: &[(Option<&str>, Option<&str>)] = &[
            (None, None),
            (Some(""), None),
            (Some("  "), None),
            (Some("bridge"), None),
            (Some("BRIDGE"), None),
            (Some("host"), None),
            (Some("Host"), None),
            (Some("container:abc"), None),
            (Some("ns:/var/run/netns/x"), None),
            (Some("has space"), None),
            (Some("none"), Some("none")),
            (Some("None"), Some("none")),
            (Some("NONE"), Some("none")),
            (Some(" egress-proxy "), Some("egress-proxy")),
        ];
        for (input, expected) in cases {
            assert_eq!(
                sanitize_network(*input).as_deref(),
                *expected,
                "network {input:?}"
            );
        }
    }

    fn commit_head(repo: &git2::Repository) {
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial", &tree, &[])
            .unwrap();
    }

    fn setup_regular_repo() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        commit_head(&git2::Repository::init(dir.path()).unwrap());
        let repo_path = dir.path().to_path_buf();
        (dir, repo_path)
    }

    /// Branch `wt-branch` off HEAD and check it out at `worktree`. False when
    /// the `git` binary is unavailable, which the callers treat as a skip.
    fn add_worktree(repo_path: &Path, worktree: &Path) -> bool {
        {
            let repo = git2::Repository::open(repo_path).unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("wt-branch", &head, false).unwrap();
        }
        std::process::Command::new("git")
            .args(["worktree", "add", worktree.to_str().unwrap(), "wt-branch"])
            .current_dir(repo_path)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn setup_bare_repo_with_worktree() -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new().unwrap();
        let bare_path = dir.path().join(".bare");
        let repo = git2::Repository::init_bare(&bare_path).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial", &tree, &[])
            .unwrap();
        fs::write(dir.path().join(".git"), "gitdir: ./.bare\n").unwrap();

        let worktree_path = dir.path().join("main");
        let _ = std::process::Command::new("git")
            .args(["worktree", "add", worktree_path.to_str().unwrap(), "HEAD"])
            .current_dir(&bare_path)
            .output();

        let main_repo_path = dir.path().to_path_buf();
        (dir, main_repo_path, worktree_path)
    }

    /// A repo root, a plain directory and a non-git subdirectory of a repo all
    /// mount themselves alone at `/workspace/{basename}`. The last is #375: the
    /// ancestor repo (a home directory under dotfile management) must not be
    /// what gets mounted.
    #[test]
    fn compute_volume_paths_mounts_one_directory_for_a_repo_root_or_plain_dir() {
        let (_repo_dir, repo_path) = setup_regular_repo();
        let plain_dir = TempDir::new().unwrap();
        let plain = plain_dir.path().to_path_buf();
        let ancestor = TempDir::new().unwrap();
        git2::Repository::init(ancestor.path()).unwrap();
        let subdir = ancestor.path().join("playground");
        fs::create_dir_all(&subdir).unwrap();

        for project in [&repo_path, &plain, &subdir] {
            let (volumes, working_dir) =
                compute_volume_paths(project, project.to_str().unwrap()).unwrap();
            let label = project.display();
            assert_eq!(volumes.len(), 1, "{label}");
            assert_eq!(volumes[0].host_path, project.to_string_lossy(), "{label}");
            assert_eq!(volumes[0].container_path, working_dir, "{label}");
            assert_eq!(
                working_dir,
                format!(
                    "/workspace/{}",
                    project.file_name().unwrap().to_string_lossy()
                ),
                "{label}"
            );
        }
    }

    /// Every arrival at `/workspace/{basename}` is reported as `Fallthrough`, including
    /// a worktree whose `.git` file is gone rather than merely unresolvable, which
    /// collapses the same way without `find_main_repo` ever being asked.
    #[test]
    fn compute_volume_paths_reports_every_collapsed_resolve() {
        let dir = TempDir::new().unwrap();

        // An orphaned worktree: a `.git` file whose gitdir points nowhere, the
        // state a pruned admin entry leaves behind (#2414).
        let orphaned = dir.path().join("myrepo-worktrees").join("contexec");
        fs::create_dir_all(&orphaned).unwrap();
        fs::write(
            orphaned.join(".git"),
            "gitdir: ../../does-not-exist/.git/worktrees/contexec\n",
        )
        .unwrap();

        let plain = dir.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        let (_repo_dir, repo_path) = setup_regular_repo();

        for (case, path) in [
            ("an orphaned worktree", &orphaned),
            ("a worktree with no .git at all", &plain),
            ("a healthy repo root", &repo_path),
        ] {
            let (_volumes, workspace_path, resolve) =
                compute_volume_paths_with_resolve(path, path.to_str().unwrap()).unwrap();
            assert_eq!(resolve, MountResolve::Fallthrough, "{case}");
            // All three land on the same path, which is why a caller cannot tell
            // them apart from the result alone.
            assert_eq!(
                workspace_path,
                format!("/workspace/{}", path.file_name().unwrap().to_string_lossy()),
                "{case}"
            );
        }
    }

    /// A bare-repo layout mounts the repo root either way; from a worktree the
    /// working dir points inside that one mount.
    #[test]
    fn compute_volume_paths_bare_repo_mounts_the_repo_root() {
        let (_dir, main_repo_path, worktree_path) = setup_bare_repo_with_worktree();
        let main_canon = main_repo_path.canonicalize().unwrap();
        let repo_name = main_repo_path.file_name().unwrap().to_string_lossy();

        let (volumes, working_dir) =
            compute_volume_paths(&main_repo_path, main_repo_path.to_str().unwrap()).unwrap();
        assert_eq!(volumes.len(), 1);
        assert_eq!(
            Path::new(&volumes[0].host_path).canonicalize().unwrap(),
            main_canon
        );
        assert!(!working_dir.is_empty());

        // git may be unavailable, in which case there is no worktree to check.
        if !worktree_path.exists() {
            return;
        }
        let (volumes, working_dir) =
            compute_volume_paths(&worktree_path, worktree_path.to_str().unwrap()).unwrap();
        assert_eq!(volumes.len(), 1);
        assert_eq!(
            Path::new(&volumes[0].host_path).canonicalize().unwrap(),
            main_canon
        );
        assert_eq!(
            volumes[0].container_path,
            format!("/workspace/{}", repo_name),
            "Container mount path should be /workspace/{{repo_name}}"
        );
        assert!(working_dir.starts_with(&format!("/workspace/{}", repo_name)));
        assert!(working_dir.ends_with("/main"));
    }

    /// A sibling worktree of a non-bare repo mounts both trees, flat under
    /// `/workspace/`, and works from the worktree.
    #[test]
    fn compute_volume_paths_sibling_worktree_mounts_both_trees() {
        let (_dir, repo_path) = setup_regular_repo();
        let worktree_path = repo_path.parent().unwrap().join("my-worktree");
        if !add_worktree(&repo_path, &worktree_path) {
            return;
        }

        let (volumes, working_dir) =
            compute_volume_paths(&worktree_path, worktree_path.to_str().unwrap()).unwrap();

        assert_eq!(volumes.len(), 2);
        let repo_canon = repo_path.canonicalize().unwrap();
        assert_eq!(
            Path::new(&volumes[0].host_path).canonicalize().unwrap(),
            repo_canon
        );
        assert_eq!(
            volumes[0].container_path,
            format!(
                "/workspace/{}",
                repo_canon.file_name().unwrap().to_string_lossy()
            )
        );
        assert_eq!(
            Path::new(&volumes[1].host_path).canonicalize().unwrap(),
            worktree_path.canonicalize().unwrap()
        );
        assert_eq!(volumes[1].container_path, "/workspace/my-worktree");
        assert_eq!(working_dir, "/workspace/my-worktree");
    }

    /// A worktree nested deeper than its main repo (repo at `/scm/my-repo`,
    /// worktree at `/scm/worktrees/my-repo/1`) keeps its relative depth in the
    /// container, so the `.git` file's relative gitdir still resolves.
    #[test]
    fn compute_volume_paths_nested_worktree_keeps_relative_depth() {
        let dir = TempDir::new().unwrap();
        let repo_path = dir.path().join("my-repo");
        fs::create_dir_all(&repo_path).unwrap();
        commit_head(&git2::Repository::init(&repo_path).unwrap());

        let worktree_path = dir.path().join("worktrees").join("my-repo").join("1");
        fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
        if !add_worktree(&repo_path, &worktree_path) {
            return;
        }

        // AoE's create_worktree rewrites .git to a relative gitdir; calling git
        // directly does not, so replicate it.
        let git_file = worktree_path.join(".git");
        let gitdir = read_gitdir(&git_file);
        if Path::new(&gitdir).is_absolute() {
            let wt_canon = worktree_path.canonicalize().unwrap();
            let gitdir_canon = Path::new(&gitdir).canonicalize().unwrap();
            if let Some(rel) = crate::git::GitWorktree::diff_paths(&gitdir_canon, &wt_canon) {
                fs::write(&git_file, format!("gitdir: {}\n", rel.display())).unwrap();
            }
        }

        let (volumes, working_dir) =
            compute_volume_paths(&worktree_path, worktree_path.to_str().unwrap()).unwrap();
        assert_eq!(volumes.len(), 2);

        let repo_canon = repo_path.canonicalize().unwrap();
        let wt_canon = worktree_path.canonicalize().unwrap();
        let common = common_ancestor(&repo_canon, &wt_canon);
        let container_path = |path: &Path| {
            format!(
                "/workspace/{}",
                path.strip_prefix(&common).unwrap().display()
            )
        };
        assert_eq!(volumes[0].container_path, container_path(&repo_canon));
        assert_eq!(volumes[1].container_path, container_path(&wt_canon));
        assert_eq!(working_dir, container_path(&wt_canon));

        // The relative gitdir must land inside the main repo's mount.
        let resolved = PathBuf::from(&working_dir).join(read_gitdir(&git_file));
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
        assert!(normalized
            .to_string_lossy()
            .starts_with(&volumes[0].container_path));
    }

    fn read_gitdir(git_file: &Path) -> String {
        fs::read_to_string(git_file)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))
            .unwrap()
            .to_string()
    }

    // --- sandbox config tests ---

    fn sync_fixture(
        host: &Path,
        sandbox: &Path,
        files: &[&str],
        seeds: &[(&str, &str)],
        directories: &[&str],
        preserved: &[&str],
    ) -> Result<()> {
        fs::create_dir_all(sandbox)?;
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "claude")
            .unwrap();
        let boundary = NativeStateBoundary::for_fixture(host, sandbox, mount)?;
        sync_agent_config(
            host,
            sandbox,
            files,
            seeds,
            directories,
            preserved,
            &boundary,
        )
    }
    fn certify_fixture_content(path: &Path, role: &str) -> Result<()> {
        fs::create_dir_all(path)?;
        let instance = path.file_name().and_then(|value| value.to_str()).unwrap();
        crate::migrations::v033_isolate_sandbox_content::certify_test_content(
            &crate::session::get_app_dir()?,
            instance,
            path,
            &[role],
        )
    }
    fn prepare_owned_fixture(
        mount: &AgentConfigMount,
        home: &Path,
        instance: Option<&str>,
        fold: CredentialFold,
        session: &super::super::SessionConfig,
        workspace: &Path,
    ) -> Result<PathBuf> {
        prepare_owned_fixture_from(
            mount,
            home.join(mount.host_rel),
            sandbox_dir_for(mount, home, instance)?,
            home,
            fold,
            session,
            workspace,
        )
    }
    fn prepare_owned_fixture_from(
        mount: &AgentConfigMount,
        host: PathBuf,
        sandbox: PathBuf,
        home: &Path,
        fold: CredentialFold,
        session: &super::super::SessionConfig,
        workspace: &Path,
    ) -> Result<PathBuf> {
        certify_fixture_content(&sandbox, mount.container_suffix)?;
        super::prepare_sandbox_dir_from(mount, host, sandbox, home, fold, session, workspace)
    }
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
    fn sync_copies_listed_files_and_dirs_only() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        fs::create_dir_all(host.join("plugins/lsp")).unwrap();
        fs::write(host.join("plugins/lsp/gopls.wasm"), "binary").unwrap();
        let real_skills = dir.path().join("real-skills");
        fs::create_dir_all(&real_skills).unwrap();
        fs::write(real_skills.join("skill.md"), "# Skill").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real_skills, host.join("skills")).unwrap();
            std::os::unix::fs::symlink("/nonexistent/path", host.join("broken-link")).unwrap();
        }
        let sandbox = dir.path().join("sandbox");
        sync_fixture(
            &host,
            &sandbox,
            &["auth.json", "settings.json", "broken-link"],
            &[],
            &["plugins", "skills", "nonexistent"],
            &[],
        )
        .unwrap();

        assert!(sandbox.join("auth.json").exists());
        assert!(sandbox.join("settings.json").exists());
        assert_eq!(
            fs::read_to_string(sandbox.join("plugins/lsp/gopls.wasm")).unwrap(),
            "binary"
        );
        // Unlisted directories are skipped.
        assert!(!sandbox.join("subdir").exists());
        #[cfg(unix)]
        {
            // A symlinked directory is followed; a broken top-level link is skipped.
            assert_eq!(
                fs::read_to_string(sandbox.join("skills/skill.md")).unwrap(),
                "# Skill"
            );
            assert!(!sandbox.join("broken-link").exists());
        }
    }

    #[test]
    fn test_only_explicitly_selected_files_are_copied() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");

        sync_fixture(&host, &sandbox, &["settings.json"], &[], &[], &[]).unwrap();

        assert!(!sandbox.join("auth.json").exists());
        assert!(sandbox.join("settings.json").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_hermes_mount_skips_runtime_dirs() {
        let (_hook_guard, _, _application) = BaseGuard::ready();
        let dir = TempDir::new().unwrap();
        let host = dir.path().join(".hermes");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("config.yaml"), "model: claude-opus\n").unwrap();
        fs::write(host.join(".env"), "API_KEY=token\n").unwrap();
        fs::write(host.join("SOUL.md"), "# soul\n").unwrap();

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
        let sandbox = prepare_owned_fixture(
            mount,
            dir.path(),
            None,
            CredentialFold::Freshest,
            &crate::session::config::SessionConfig::default(),
            dir.path(),
        )
        .unwrap();

        assert!(sandbox.join("config.yaml").exists());
        assert!(sandbox.join(".env").exists());
        assert!(sandbox.join("SOUL.md").exists());

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
    #[serial_test::serial]
    fn test_opencode_mount_preserves_sqlite_db_across_prepares() {
        let (_hook_guard, _, _application) = BaseGuard::ready(); // Regression for #2605. Before the fix, `prepare_sandbox_dir` walked
                                                                 // `clean_files` on every invocation and wiped the sandbox-owned
                                                                 // opencode SQLite DB, so `aoe resume` hit "Session not found". This
                                                                 // test plants a DB in the sandbox subdir, invokes `prepare_sandbox_dir`
                                                                 // (which fires at container_config.rs:987 and :1436), and asserts the
                                                                 // DB survives byte-for-byte.
        let dir = TempDir::new().unwrap();
        let host = dir.path().join(".local/share/opencode");
        let sandbox = host.join(SANDBOX_SUBDIR);
        fs::create_dir_all(&sandbox).unwrap();

        let db_bytes = b"SQLite format 3\0-opencode-session-state";
        fs::write(sandbox.join("opencode.db"), db_bytes).unwrap();
        fs::write(sandbox.join("opencode.db-wal"), b"wal-frames").unwrap();
        fs::write(sandbox.join("opencode.db-shm"), b"shm-index").unwrap();

        // Host config so `sync_agent_config` has real work to do; without a
        // non-empty host_dir the sync short-circuits and we would not exercise
        // the code path that used to wipe the DB.
        fs::write(host.join("config.json"), "{}").unwrap();

        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|m| m.tool_name == "opencode" && m.host_rel == ".local/share/opencode")
            .expect("opencode data-dir mount");
        let out = prepare_owned_fixture(
            mount,
            dir.path(),
            None,
            CredentialFold::Freshest,
            &crate::session::config::SessionConfig::default(),
            dir.path(),
        )
        .unwrap();
        assert_eq!(out, sandbox);

        assert!(
            out.join("opencode.db").exists(),
            "opencode.db must survive prepare_sandbox_dir (regression of #2605)",
        );
        assert_eq!(
            fs::read(out.join("opencode.db")).unwrap(),
            db_bytes,
            "opencode.db content must be untouched",
        );
        assert!(
            out.join("opencode.db-wal").exists(),
            "opencode.db-wal must survive",
        );
        assert!(
            out.join("opencode.db-shm").exists(),
            "opencode.db-shm must survive",
        );
    }

    #[test]
    fn seed_files_are_written_once_and_lose_to_host_files() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");
        let seeds = [
            ("seed.json", r#"{"seeded":true}"#),
            // Same name as a host file: the host copy wins.
            ("auth.json", "seed-content"),
        ];
        sync_fixture(&host, &sandbox, &["auth.json"], &seeds, &[], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("seed.json")).unwrap(),
            r#"{"seeded":true}"#
        );
        assert_eq!(
            fs::read_to_string(sandbox.join("auth.json")).unwrap(),
            r#"{"token":"abc"}"#
        );

        // A re-sync keeps the container's edit to a seed.
        fs::write(sandbox.join("seed.json"), r#"{"modified":true}"#).unwrap();
        sync_fixture(&host, &sandbox, &["auth.json"], &seeds, &[], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("seed.json")).unwrap(),
            r#"{"modified":true}"#
        );
    }

    #[test]
    #[serial_test::serial]
    fn dropped_config_files_cross_into_the_sandbox() {
        // #3981 review: keybindings.json (claude) and .env (gemini) were dropped.
        let (_hook_guard, _, _application) = BaseGuard::ready();
        for (tool, file, content) in [
            ("claude", "keybindings.json", "{}\n"),
            ("gemini", ".env", "GEMINI_API_KEY=token\n"),
        ] {
            let dir = TempDir::new().unwrap();
            let mount = AGENT_CONFIG_MOUNTS
                .iter()
                .find(|m| m.tool_name == tool)
                .unwrap();
            let host = dir.path().join(mount.host_rel);
            fs::create_dir_all(&host).unwrap();
            fs::write(host.join(file), content).unwrap();
            let sandbox = prepare_owned_fixture(
                mount,
                dir.path(),
                None,
                CredentialFold::Freshest,
                &crate::session::config::SessionConfig::default(),
                dir.path(),
            )
            .unwrap();
            assert!(
                sandbox.join(file).exists(),
                "{tool} must cross {file} into the sandbox"
            );
        }
    }

    #[test]
    fn a_json_seed_default_enforces_its_keys_over_a_carried_file() {
        // #3981 review: retirement carries the old `.claude.json` forward; its
        // onboarding flag must not linger false while other keys stay.
        let dir = TempDir::new().unwrap();
        let output = crate::session::anchored_fs::AnchoredDir::open(dir.path()).unwrap();
        fs::write(
            dir.path().join(".claude.json"),
            r#"{"hasCompletedOnboarding":false,"mcpServers":{"x":1}}"#,
        )
        .unwrap();
        let merged = merged_json_seed(
            &output,
            Path::new(".claude.json"),
            r#"{"hasCompletedOnboarding":true}"#,
        )
        .unwrap()
        .expect("a carried JSON file merges the seed default");
        let value: serde_json::Value = serde_json::from_slice(&merged).unwrap();
        assert_eq!(value["hasCompletedOnboarding"], serde_json::json!(true));
        assert_eq!(value["mcpServers"]["x"], serde_json::json!(1));
        // An absent file keeps the seed-once publish.
        assert!(
            merged_json_seed(&output, Path::new("absent.json"), r#"{"a":1}"#)
                .unwrap()
                .is_none()
        );
        // A non-JSON default is never merged (e.g. the gitconfig home seed).
        fs::write(dir.path().join("gitconfig"), "[user]\n").unwrap();
        assert!(
            merged_json_seed(&output, Path::new("gitconfig"), "[credential]\n")
                .unwrap()
                .is_none()
        );
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
        sync_fixture(&host, &sandbox, &[], &[], &["plugins"], &[]).unwrap();
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
    fn resync_refreshes_host_files_and_keeps_sandbox_owned_ones() {
        let dir = TempDir::new().unwrap();
        let host = setup_host_dir(&dir);
        let sandbox = dir.path().join("sandbox");
        fs::write(host.join("history.jsonl"), "host-entry\n").unwrap();
        let files = ["auth.json", "settings.json", "new_cred.json"];
        let sync = || sync_fixture(&host, &sandbox, &files, &[], &[], &["auth.json"]).unwrap();
        let read = |name: &str| fs::read_to_string(sandbox.join(name)).unwrap();

        // A preserved file is still seeded when the sandbox lacks it.
        sync();
        assert_eq!(read("auth.json"), r#"{"token":"abc"}"#);
        assert!(!sandbox.join("new_cred.json").exists());
        assert!(!sandbox.join("history.jsonl").exists());

        fs::write(sandbox.join("auth.json"), r#"{"token":"container"}"#).unwrap();
        fs::write(sandbox.join("history.jsonl"), "container-session-1\n").unwrap();
        fs::write(host.join("auth.json"), r#"{"token":"refreshed"}"#).unwrap();
        fs::write(host.join("settings.json"), "updated").unwrap();
        fs::write(host.join("new_cred.json"), "new").unwrap();
        sync();

        assert_eq!(read("settings.json"), "updated");
        assert_eq!(read("new_cred.json"), "new");
        assert_eq!(read("auth.json"), r#"{"token":"container"}"#);
        assert_eq!(read("history.jsonl"), "container-session-1\n");
    }

    // Regression for #3014: settings.json (a top-level file) referenced a hook
    // script under ~/.claude/hooks/, but `hooks` was absent from Claude's
    // copy_dirs, so the config was carried into the sandbox without the script
    // it points at and every tool call errored ("No such file or directory").
    #[test]
    fn test_claude_mount_copies_hooks_alongside_settings() {
        let claude_mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|m| m.tool_name == "claude")
            .expect("claude mount must exist");

        let dir = TempDir::new().unwrap();
        let host = dir.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(
            host.join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"command":"~/.claude/hooks/secret-guard.sh"}]}]}}"#,
        )
        .unwrap();
        fs::create_dir_all(host.join("hooks")).unwrap();
        fs::write(host.join("hooks").join("secret-guard.sh"), "#!/bin/sh\n").unwrap();

        let sandbox = dir.path().join("sandbox");
        sync_fixture(
            &host,
            &sandbox,
            claude_mount.copy_files,
            &[],
            claude_mount.copy_dirs,
            claude_mount.preserve_files,
        )
        .unwrap();

        assert!(sandbox.join("settings.json").exists());
        assert!(
            sandbox.join("hooks").join("secret-guard.sh").exists(),
            "hook script referenced by settings.json must be copied into the sandbox"
        );
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

        let dest = dir.path().join("sandbox/src");
        sync_fixture(dir.path(), dest.parent().unwrap(), &[], &[], &["src"], &[]).unwrap();
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

        let dest = dir.path().join("sandbox/src");
        sync_fixture(dir.path(), dest.parent().unwrap(), &[], &[], &["src"], &[]).unwrap();
        assert_eq!(fs::read_to_string(dest.join("good.txt")).unwrap(), "good");
        assert!(!dest.join("dangling").exists());
    }

    #[test]
    fn test_sandbox_skills_sync_requires_opt_in() {
        let dir = TempDir::new().unwrap();
        let app_dir = dir.path().join("app");
        crate::session::skills_model::create_skill(&app_dir, "shared", Some("d")).unwrap();
        for (index, (tool, role, expected)) in [
            ("claude", ".claude", true),
            ("gemini", ".gemini", true),
            ("opencode", ".config/opencode", true),
            ("opencode", ".local/share/opencode", false),
            ("kimi", ".kimi-code", true),
            ("prime-agent", ".prime/agent", true),
            ("codex", ".codex", false),
        ]
        .into_iter()
        .enumerate()
        {
            let sandbox = dir.path().join(format!("sandbox-{index}"));
            let mount = AGENT_CONFIG_MOUNTS
                .iter()
                .find(|mount| mount.tool_name == tool && mount.container_suffix == role)
                .unwrap();
            let skill = sandbox.join("skills/shared/SKILL.md");
            sync_managed_skills_into_sandbox(mount, &sandbox, &app_dir, false);
            assert!(!skill.exists(), "{tool} propagated without opt-in");
            sync_managed_skills_into_sandbox(mount, &sandbox, &app_dir, true);
            assert_eq!(skill.is_file(), expected, "{tool} at {role}");
        }
    }

    #[test]
    fn test_resource_seed_is_independent_of_claude_history_sentinel() {
        let dir = TempDir::new().unwrap();
        let host = dir.path().join("host");
        fs::create_dir_all(host.join("plugins")).unwrap();
        fs::write(host.join("plugins").join("p.txt"), "host-plugin").unwrap();
        let sandbox = dir.path().join("sandbox");

        // Prior container session sentinel.
        fs::create_dir_all(sandbox.join("projects")).unwrap();

        sync_fixture(&host, &sandbox, &[], &[], &["plugins"], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("plugins/p.txt")).unwrap(),
            "host-plugin"
        );
        fs::write(sandbox.join("plugins/p.txt"), "local-plugin").unwrap();
        sync_fixture(&host, &sandbox, &[], &[], &["plugins"], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.join("plugins/p.txt")).unwrap(),
            "local-plugin"
        );
    }

    // --- credential freshness tests ---

    /// Fields in the order `serde_json` writes them back, so a fold that
    /// re-serializes a credential produces this string again.
    fn credential(expires_at: u64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"access-{expires_at}","expiresAt":{expires_at},"refreshToken":"refresh-{expires_at}"}}}}"#
        )
    }

    /// What Claude Code leaves behind when its credential fails to
    /// authenticate: both tokens emptied in place, the rest of the block kept.
    fn blanked_credential(expires_at: u64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"","expiresAt":{expires_at},"refreshToken":"","scopes":["user:inference"],"subscriptionType":"max"}}}}"#
        )
    }

    #[test]
    fn only_a_usable_credential_carries_an_expiry() {
        let now = now_ms();
        let horizon = CREDENTIAL_EXPIRY_HORIZON.as_millis() as u64;
        let cases = [
            (credential(1700000000), Some(1700000000)),
            // Emptied tokens are not a credential, whatever expiry is left
            // beside them (#3860).
            (blanked_credential(1700000000), None),
            (blanked_credential(0), None),
            // Either token alone keeps a container going: an access token
            // until it expires, a refresh token to get another.
            (
                r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":10}}"#.into(),
                Some(10),
            ),
            (
                r#"{"claudeAiOauth":{"refreshToken":"r","expiresAt":10}}"#.into(),
                Some(10),
            ),
            (r#"{"other":"data"}"#.into(), None),
            (r#"{"claudeAiOauth":{"accessToken":"a"}}"#.into(), None),
            (
                r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":"10"}}"#.into(),
                None,
            ),
            ("not json at all".into(), None),
            (String::new(), None),
            // Planted from inside a sandbox to outrank every later login.
            (credential(now + horizon + 1), None),
        ];
        for (content, expires_at) in cases {
            assert_eq!(
                plausible_credential_expires_at(&content, now),
                expires_at,
                "{content}"
            );
        }
    }

    #[test]
    fn the_freshest_credential_wins_an_overwrite() {
        let far = credential(now_ms() + 2 * CREDENTIAL_EXPIRY_HORIZON.as_millis() as u64);
        let cases = [
            (credential(2000), credential(1000), false),
            (credential(1000), credential(2000), true),
            (credential(1000), credential(1000), false),
            (credential(1000), "not-json".into(), false),
            ("bad".into(), "also-bad".into(), true),
            ("not-json".into(), credential(1000), true),
            // A blanked file never outranks a credential however far its
            // leftover expiry reaches, and a credential always replaces it.
            (credential(1000), blanked_credential(9000), false),
            (blanked_credential(9000), credential(1000), true),
            // A planted far-future expiry never outranks a real one.
            (credential(now_ms()), far.clone(), false),
            (far, credential(now_ms()), true),
        ];
        for (existing, incoming, overwrite) in cases {
            assert_eq!(
                should_overwrite_credential(&existing, &incoming),
                overwrite,
                "{existing} -> {incoming}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn an_unreadable_shared_file_is_never_overwritten() {
        let (_hook_guard, _, _application) = BaseGuard::ready();
        use std::os::unix::fs::PermissionsExt;
        let home = TempDir::new().unwrap();
        let host = home.path().join(".claude");
        let mount = claude_mount_without_keychain();
        let store = host.join(SANDBOX_PRIVATE_SUBDIR).join("aaaaaaaaaaaaaaaa");
        let shared = host.join(SANDBOX_PRIVATE_SUBDIR).join(".credentials.json");
        fs::create_dir_all(&store).unwrap();
        // A failed sync is logged and the launch goes on; the file must be
        // left exactly as it was.
        let prepare = || {
            prepare_owned_fixture_from(
                &mount,
                host.clone(),
                store.clone(),
                home.path(),
                CredentialFold::Freshest,
                &crate::session::config::SessionConfig::default(),
                home.path(),
            )
            .unwrap()
        };

        // Root reads a write-only file regardless, so the case cannot fail there.
        if !nix::unistd::geteuid().is_root() {
            fs::write(&shared, credential(5)).unwrap();
            fs::set_permissions(&shared, fs::Permissions::from_mode(0o200)).unwrap();
            prepare();
            fs::set_permissions(&shared, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(fs::read_to_string(&shared).unwrap(), credential(5));
            fs::remove_file(&shared).unwrap();
        }

        // A path that is not a plain file is not seeded over either.
        fs::create_dir(&shared).unwrap();
        prepare();
        assert!(shared.is_dir());
    }

    #[test]
    fn merge_keeps_what_the_file_held_beside_the_oauth_token() {
        let existing = r#"{"claudeAiOauth":{"expiresAt":1},"designOauth":{"k":"v"}}"#;
        let winner = r#"{"claudeAiOauth":{"expiresAt":2}}"#;
        let merged: serde_json::Value =
            serde_json::from_str(&merge_credential(Some(existing), winner)).unwrap();
        assert_eq!(merged["claudeAiOauth"]["expiresAt"], 2);
        assert_eq!(merged["designOauth"]["k"], "v");
        assert_eq!(merge_credential(Some("{}"), winner), winner);
        assert_eq!(merge_credential(Some("not json"), winner), winner);
        assert_eq!(merge_credential(None, winner), winner);
    }

    /// The Claude mount without its Keychain source, so a developer's own
    /// login never reaches the assertions on macOS.
    fn claude_mount_without_keychain() -> AgentConfigMount {
        AgentConfigMount {
            keychain_credential: None,
            ..*AGENT_CONFIG_MOUNTS
                .iter()
                .find(|mount| mount.tool_name == "claude")
                .unwrap()
        }
    }

    #[test]
    #[serial_test::serial]
    fn shared_credential_follows_the_freshest_copy_across_starts() {
        let (_hook_guard, _, _application) = BaseGuard::ready();
        let home = TempDir::new().unwrap();
        let host = home.path().join(".claude");
        fs::create_dir_all(&host).unwrap();
        let mount = claude_mount_without_keychain();
        let root = host.join(SANDBOX_PRIVATE_SUBDIR);
        let store = root.join("aaaaaaaaaaaaaaaa");
        let shared = root.join(".credentials.json");
        let private = store.join(".credentials.json");
        let prepare = || {
            prepare_owned_fixture_from(
                &mount,
                host.clone(),
                store.clone(),
                home.path(),
                CredentialFold::Freshest,
                &crate::session::config::SessionConfig::default(),
                home.path(),
            )
            .unwrap()
        };

        // Nothing to seed: the mount source still has to exist.
        prepare();
        assert_eq!(fs::read_to_string(&shared).unwrap(), "{}");
        assert!(!private.exists());

        // A host login reaches the shared file and never the store.
        fs::write(host.join(".credentials.json"), credential(100)).unwrap();
        prepare();
        assert_eq!(fs::read_to_string(&shared).unwrap(), credential(100));
        assert!(!private.exists());

        // A fresher host login is the host's own chain: seeding it again
        // would have the first side to refresh log the other out.
        fs::create_dir_all(store.join("projects")).unwrap();
        fs::write(host.join(".credentials.json"), credential(200)).unwrap();
        prepare();
        assert_eq!(fs::read_to_string(&shared).unwrap(), credential(100));

        // Nor does a stale host copy clobber a login made in a container.
        fs::write(host.join(".credentials.json"), credential(50)).unwrap();
        prepare();
        assert_eq!(fs::read_to_string(&shared).unwrap(), credential(100));

        // A private copy left by the v027 move is folded in and left for a
        // container that still mounts only the store.
        fs::write(&private, credential(300)).unwrap();
        prepare();
        assert_eq!(fs::read_to_string(&shared).unwrap(), credential(300));
        assert_eq!(fs::read_to_string(&private).unwrap(), credential(300));

        // The empty placeholder a runtime creates for the file mount is ignored.
        fs::write(&private, "").unwrap();
        prepare();
        assert_eq!(fs::read_to_string(&shared).unwrap(), credential(300));
    }

    #[test]
    #[serial_test::serial]
    fn off_a_come_up_the_fold_only_seeds() {
        let (_hook_guard, _, _application) = BaseGuard::ready();
        let home = TempDir::new().unwrap();
        let host = home.path().join(".claude");
        fs::create_dir_all(&host).unwrap();
        let mount = claude_mount_without_keychain();
        let root = host.join(SANDBOX_PRIVATE_SUBDIR);
        let store = root.join("aaaaaaaaaaaaaaaa");
        let shared = root.join(".credentials.json");
        fs::create_dir_all(&store).unwrap();
        let prepare = |fold| {
            prepare_owned_fixture_from(
                &mount,
                host.clone(),
                store.clone(),
                home.path(),
                fold,
                &crate::session::config::SessionConfig::default(),
                home.path(),
            )
            .unwrap()
        };

        // A file holding no usable credential is seeded from the host.
        fs::write(host.join(".credentials.json"), credential(100)).unwrap();
        for unusable in ["", "{}", "not json"] {
            fs::write(&shared, unusable).unwrap();
            prepare(CredentialFold::SeedOnly);
            assert_eq!(
                fs::read_to_string(&shared).unwrap(),
                credential(100),
                "{unusable:?}"
            );
        }

        // A fresher copy in the store, a sandbox chain of its own, waits for
        // a come-up; a fresher host login never replaces what the file holds.
        fs::write(host.join(".credentials.json"), credential(300)).unwrap();
        fs::write(store.join(".credentials.json"), credential(200)).unwrap();
        prepare(CredentialFold::SeedOnly);
        assert_eq!(fs::read_to_string(&shared).unwrap(), credential(100));
        prepare(CredentialFold::Freshest);
        assert_eq!(fs::read_to_string(&shared).unwrap(), credential(200));
    }

    #[test]
    #[serial_test::serial]
    fn an_emptied_shared_file_is_seeded_for_the_next_container() {
        let (_hook_guard, _, _application) = BaseGuard::ready();
        let home = TempDir::new().unwrap();
        let host = home.path().join(".claude");
        fs::create_dir_all(&host).unwrap();
        let mount = claude_mount_without_keychain();
        let root = host.join(SANDBOX_PRIVATE_SUBDIR);
        let store = root.join("aaaaaaaaaaaaaaaa");
        let shared = root.join(".credentials.json");
        fs::create_dir_all(&store).unwrap();
        let prepare = |fold| {
            prepare_owned_fixture_from(
                &mount,
                host.clone(),
                store.clone(),
                home.path(),
                fold,
                &crate::session::config::SessionConfig::default(),
                home.path(),
            )
            .unwrap()
        };

        // A container whose credential fails to authenticate empties both
        // tokens in place, through the mount every sandbox shares. The file
        // it leaves keeps its expiry, and used to read as a credential no
        // seed would replace: every container created afterwards came up on
        // it and only deleting the file by hand got out (#3860). The next
        // come-up seeds it now, and so does the poll, for the containers
        // already running.
        fs::write(host.join(".credentials.json"), credential(100)).unwrap();
        for fold in [CredentialFold::SeedOnly, CredentialFold::Freshest] {
            fs::write(&shared, blanked_credential(900)).unwrap();
            prepare(fold);
            let seeded = fs::read_to_string(&shared).unwrap();
            assert!(holds_credential(&seeded), "{fold:?}");
            assert_eq!(seeded, credential(100), "{fold:?}");
        }

        // The emptied file the container left is not a fresher credential
        // than the one seeded over it, however far its leftover expiry
        // reaches, so a second come-up does not put it back.
        prepare(CredentialFold::Freshest);
        assert_eq!(fs::read_to_string(&shared).unwrap(), credential(100));

        // A copy a container emptied in a store is not a candidate either: it
        // would otherwise outrank the login it is folded against.
        fs::write(store.join(".credentials.json"), blanked_credential(900)).unwrap();
        prepare(CredentialFold::Freshest);
        assert_eq!(fs::read_to_string(&shared).unwrap(), credential(100));
    }

    #[test]
    #[serial_test::serial]
    fn claude_sandboxes_share_one_credential_mount() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let host = temp_home.path().join(".claude");
        fs::create_dir_all(&host).unwrap();
        // Beats any real credential the macOS Keychain contributes while
        // staying inside the horizon a real token can carry.
        let cred = credential(now_ms() + 300 * 24 * 60 * 60 * 1000);
        fs::write(host.join(".credentials.json"), &cred).unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();
        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let shared = host.join(SANDBOX_PRIVATE_SUBDIR).join(".credentials.json");
        // A store the v027 move left a copy in, and a fresh one with none: both
        // end up with the empty mountpoint the nested file mount needs (#3845).
        for (instance_id, stale_copy) in [("shared-cred-a", true), ("shared-cred-b", false)] {
            let store = host.join(SANDBOX_PRIVATE_SUBDIR).join(instance_id);
            fs::create_dir_all(&store).unwrap();
            if stale_copy {
                fs::write(store.join(".credentials.json"), credential(1)).unwrap();
            }
            certify_fixture_content(&store, ".claude").unwrap();
            let config = build_container_config(
                project_dir.path().to_str().unwrap(),
                &sandbox_info,
                ContainerAgentSelection::new("claude", None),
                false,
                instance_id,
                None,
                "",
            )
            .unwrap();
            let mount = config
                .volumes
                .iter()
                .find(|v| v.container_path == "/root/.claude/.credentials.json")
                .expect("credential file mount");
            assert_eq!(mount.host_path, shared.to_string_lossy());
            assert!(!mount.read_only);
            assert_eq!(
                config.shared_credential_mounts,
                vec!["/root/.claude/.credentials.json".to_string()]
            );
            assert_eq!(store.join(".credentials.json").exists(), stale_copy);
            place_shadowed_credential_mountpoints(&config);
            assert_eq!(
                fs::read_to_string(store.join(".credentials.json")).unwrap(),
                ""
            );
            crate::hooks::cleanup_hook_status_dir(instance_id);
        }
        assert_eq!(fs::read_to_string(&shared).unwrap(), cred);
    }

    #[test]
    fn shadowed_credential_copy_is_emptied_only_in_a_store() {
        let home = TempDir::new().unwrap();
        let host = home.path().join(".claude");
        let root = host.join(SANDBOX_PRIVATE_SUBDIR);
        let store = root.join("aaaaaaaaaaaaaaaa");
        let shared = root.join(".credentials.json");
        fs::create_dir_all(&store).unwrap();
        fs::write(&shared, credential(2)).unwrap();
        let config_for = |dir: &Path| ContainerConfig {
            shared_credential_mounts: vec!["/root/.claude/.credentials.json".to_string()],
            volumes: vec![
                VolumeMount {
                    host_path: shared.to_string_lossy().to_string(),
                    container_path: "/root/.claude/.credentials.json".to_string(),
                    read_only: false,
                },
                VolumeMount {
                    host_path: dir.to_string_lossy().to_string(),
                    container_path: "/root/.claude".to_string(),
                    read_only: false,
                },
            ],
            ..Default::default()
        };
        // A user extra_volumes entry at the config path replaces the store
        // mount; its host copy is the user's own login, not a shadowed one.
        for (dir, emptied) in [(&store, true), (&host, false)] {
            let copy = dir.join(".credentials.json");
            fs::write(&copy, credential(1)).unwrap();
            place_shadowed_credential_mountpoints(&config_for(dir));
            let kept = fs::read_to_string(&copy).unwrap() == credential(1);
            assert_eq!(!kept, emptied, "{}", dir.display());
        }

        // The copy is kept while it is the only chain there is: the shared
        // file holds no credential, or one the copy is fresher than because
        // the fold did not land it. A plain file is already the mountpoint.
        let copy = store.join(".credentials.json");
        for (unfolded, copy_content) in [
            ("", credential(1)),
            ("{}", credential(1)),
            (&credential(2), credential(3)),
            // Blanked in place by a container that failed to authenticate:
            // the copy is the only chain left, so it stays (#3860).
            (&blanked_credential(9), credential(1)),
        ] {
            fs::write(&shared, unfolded).unwrap();
            fs::write(&copy, &copy_content).unwrap();
            place_shadowed_credential_mountpoints(&config_for(&store));
            assert_eq!(
                fs::read_to_string(&copy).unwrap(),
                copy_content,
                "{unfolded:?}"
            );
        }
        fs::write(&shared, credential(2)).unwrap();

        // The store is container-writable, so a link planted at the mountpoint
        // is replaced rather than followed.
        let outside = home.path().join("outside");
        fs::write(&outside, "token").unwrap();
        fs::remove_file(&copy).unwrap();
        std::os::unix::fs::symlink(&outside, &copy).unwrap();
        place_shadowed_credential_mountpoints(&config_for(&store));
        assert!(fs::symlink_metadata(&copy).unwrap().is_file());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "token");

        // As is a directory left by a runtime that got that far.
        fs::remove_file(&copy).unwrap();
        fs::create_dir(&copy).unwrap();
        place_shadowed_credential_mountpoints(&config_for(&store));
        assert!(fs::symlink_metadata(&copy).unwrap().is_file());
    }

    /// End-to-end test: repo-level `volume_ignores` flows through
    /// build_container_config into the final ContainerConfig (#557), while
    /// repo-denied `environment` and `extra_volumes` do not.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_includes_repo_sandbox_settings() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

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
mount_ssh = true
"#,
        )
        .unwrap();

        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
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

        // A repo cannot set container env (#3710).
        let env_keys: Vec<&str> = config.environment.iter().map(|e| e.key()).collect();
        assert!(
            !env_keys.contains(&"MY_VAR") && !env_keys.contains(&"CI"),
            "repo-declared environment must not apply, got: {:?}",
            env_keys
        );

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

        // A repo cannot mount host paths into the container or hand it the
        // user's SSH keys (#3154); the tuning fields above still apply.
        let volume_pairs: Vec<(&str, &str)> = config
            .volumes
            .iter()
            .map(|v| (v.host_path.as_str(), v.container_path.as_str()))
            .collect();
        assert!(
            !volume_pairs.contains(&("/host/data", "/container/data")),
            "repo-declared extra_volumes must not mount, got: {:?}",
            volume_pairs
        );

        // #2587: the session artifact dir is bind-mounted at the fixed
        // container path and exported via AOE_ARTIFACT_DIR.
        assert!(
            env_keys.contains(&crate::session::artifacts::ARTIFACT_DIR_ENV),
            "AOE_ARTIFACT_DIR should be in environment, got: {:?}",
            config.environment
        );
        assert!(
            config
                .volumes
                .iter()
                .any(|v| v.container_path == crate::session::artifacts::CONTAINER_ARTIFACT_DIR),
            "artifact dir should be mounted at {}, got: {:?}",
            crate::session::artifacts::CONTAINER_ARTIFACT_DIR,
            volume_pairs
        );
    }

    /// The `[sandbox]` run-policy fields flow from user config into the
    /// ContainerConfig, and a repo config cannot set any of them (#3218, #2704).
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_run_policy() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        // Global config carries run policy; repo config overrides are ignored.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let app_dir = temp_home
            .path()
            .join(".config")
            .join(crate::session::APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let app_dir = temp_home.path().join(crate::session::APP_DIR_NAME_OTHER);
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(
            app_dir.join("config.toml"),
            r#"
[sandbox]
cap_add = ["SYS_ADMIN"]
cap_drop = ["ALL"]
security_opt = ["seccomp=unconfined"]
"#,
        )
        .unwrap();

        let project_dir = TempDir::new().unwrap();
        let config_dir = project_dir.path().join(".agent-of-empires");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            r#"
[sandbox]
privileged = true
cap_add = ["NET_ADMIN"]
cap_drop = []
extra_run_args = ["--privileged"]
"#,
        )
        .unwrap();

        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
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

        assert_eq!(config.run_policy.cap_add, vec!["SYS_ADMIN"]);
        assert_eq!(config.run_policy.cap_drop, vec!["ALL"]);
        assert_eq!(config.run_policy.security_opt, vec!["seccomp=unconfined"]);
        assert!(
            !config.run_policy.privileged,
            "repo must not grant --privileged"
        );
        assert!(config.run_policy.extra_run_args.is_empty());
    }

    /// `sandbox.network` stays repo-overridable, but namespace-sharing forms
    /// (`container:` on Docker, `ns:` on Podman) must be dropped at container
    /// build, because repo/profile TOML is only type-checked, never
    /// value-validated. `selinux_relabel` is repo-denied outright: `:z`
    /// relabels host paths.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_drops_repo_network_escape_and_relabel() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        for network in ["container:victim", "ns:/var/run/netns/x"] {
            let project_dir = TempDir::new().unwrap();
            let config_dir = project_dir.path().join(".agent-of-empires");
            fs::create_dir_all(&config_dir).unwrap();
            fs::write(
                config_dir.join("config.toml"),
                format!("[sandbox]\nnetwork = \"{network}\"\nselinux_relabel = true\n"),
            )
            .unwrap();

            git2::Repository::init(project_dir.path()).unwrap();

            let sandbox_info = crate::session::instance::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "test-container".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            };

            let config = build_container_config(
                project_dir.path().to_str().unwrap(),
                &sandbox_info,
                ContainerAgentSelection::new("claude", None),
                false,
                "test-instance-id",
                None,
                "",
            )
            .unwrap();

            assert_eq!(
                config.network, None,
                "repo-declared network {network:?} must be dropped"
            );
            assert!(
                !config.selinux_relabel,
                "repo-declared selinux_relabel must be dropped"
            );
        }
    }

    /// Feature test for #2045: glob volume_ignores entries are expanded against the
    /// live workspace at build time, emitting one mount per matched directory, while
    /// literal entries still mount unconditionally and no `*` ever reaches a mount
    /// path (the #2036 host-littering regression must not return).
    #[test]
    #[serial_test::serial]
    fn test_volume_ignores_expands_glob_entries() {
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

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

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
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

    /// The named-volume reclaim (#3742) runs off the resolve the mounts came from,
    /// carried on the config so the gate cannot re-derive a different answer.
    #[test]
    #[serial_test::serial]
    fn named_ignore_volumes_authoritative_tracks_the_mount_resolve() {
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let (_dir, repo_path) = setup_regular_repo();
        let worktree_path = repo_path.parent().unwrap().join("my-worktree");
        {
            let repo = git2::Repository::open(&repo_path).unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("wt-branch", &head, false).unwrap();
        }
        let added = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt-branch",
            ])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        if !added.status.success() {
            return; // git not available
        }

        let build = |project: &Path| {
            let config_dir = project.join(".agent-of-empires");
            fs::create_dir_all(&config_dir).unwrap();
            fs::write(
                config_dir.join("config.toml"),
                r#"
[sandbox]
volume_ignores = ["target"]
volume_ignores_strategy = "named"
"#,
            )
            .unwrap();
            let sandbox_info = crate::session::instance::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "test-container".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            };
            build_container_config(
                project.to_str().unwrap(),
                &sandbox_info,
                ContainerAgentSelection::new("claude", None),
                false,
                "test-instance-id",
                None,
                "",
            )
            .unwrap()
        };

        let healthy = build(&worktree_path);
        assert!(
            healthy.named_ignore_volumes_authoritative,
            "a worktree that resolves to its main repo derives real mount paths"
        );

        // Break the linkage: the same worktree now collapses to /workspace/{basename},
        // naming volumes the session never had.
        fs::remove_file(worktree_path.join(".git")).unwrap();
        fs::write(
            worktree_path.join(".git"),
            "gitdir: ../does-not-exist/.git/worktrees/my-worktree\n",
        )
        .unwrap();

        let orphaned = build(&worktree_path);
        assert!(
            !orphaned.named_ignore_volumes.is_empty(),
            "the literal entry still mounts, so an empty set cannot be what saves this case"
        );
        assert!(
            !orphaned.named_ignore_volumes_authoritative,
            "a collapsed resolve must not drive a deletion"
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

    /// Regression: when project_path is a sibling worktree, `.agent-of-empires/config.toml`
    /// lives in the main repo, not the worktree. `build_container_config` must
    /// resolve repo config from the main repo path, so a repo-allowed sandbox
    /// setting still applies. (Observed through `volume_ignores`; `extra_volumes`
    /// is no longer repo-overridable, see #3154.)
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_sibling_worktree_loads_main_repo_sandbox_settings() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

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
volume_ignores = ["node_modules"]
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

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
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

        assert!(
            config
                .anonymous_volumes
                .iter()
                .any(|v| v.ends_with("/node_modules")),
            "volume_ignores from main-repo config should apply in a sibling worktree session, got: {:?}",
            config.anonymous_volumes
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_installs_codex_hooks_files() {
        // Symlinked base prefix: keeps a mount-source assertion sensitive to
        // the #3240 canonicalization even on Linux, where /tmp is already
        // real and lexical comparison alone would survive a regression.
        let tmp_base = TempDir::new().unwrap();
        let real_parent = tmp_base.path().join("real-parent");
        std::fs::create_dir(&real_parent).unwrap();
        let link_parent = tmp_base.path().join("via-symlink");
        std::os::unix::fs::symlink(&real_parent, &link_parent).unwrap();
        let _hg = BaseGuard::with_base(link_parent.join("aoe-hooks"));
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
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

        let codex_sandbox = temp_home
            .path()
            .join(".codex")
            .join(SANDBOX_PRIVATE_SUBDIR)
            .join(instance_id);
        assert!(codex_sandbox.join("hooks.json").exists());
        assert!(!codex_sandbox.join("config.toml").exists());
        assert!(!codex_sandbox.join("settings.json").exists());
        let codex_hooks = fs::read_to_string(codex_sandbox.join("hooks.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&codex_hooks).unwrap();
        assert!(parsed["hooks"]["PreToolUse"].is_array());
        assert!(codex_hooks.contains("aoe-hooks"));
        assert!(config.volumes.iter().any(|v| {
            v.host_path == codex_sandbox.to_string_lossy()
                && v.container_path == format!("/root/.codex/{instance_id}")
        }));

        let hook_dir =
            crate::hooks::hook_status_dir(instance_id).expect("test id must be allowlist-safe");
        // Canonicalize for comparison (handles /var -> /private/var on macOS);
        // the mount source is the resolved real path since #3240.
        let hook_dir = hook_dir.canonicalize().unwrap();
        let expected_container_path = format!(
            "{}/{instance_id}",
            crate::hooks::HOOK_STATUS_BASE_IN_CONTAINER
        );
        let mount = config
            .volumes
            .iter()
            .find(|v| v.host_path == hook_dir.to_string_lossy())
            .expect("status hook directory should be mounted");
        assert_eq!(
            mount.container_path, expected_container_path,
            "container path must be the fixed in-container path, not the host euid path"
        );
        assert_ne!(
            mount.host_path, mount.container_path,
            "host (per-user) and container (fixed) paths MUST differ for the bind-mount remap"
        );
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_isolates_codex_home_per_instance() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();
        let legacy_sandbox = temp_home.path().join(".codex").join(SANDBOX_SUBDIR);
        fs::create_dir_all(&legacy_sandbox).unwrap();
        fs::write(legacy_sandbox.join("auth.json"), "legacy-auth").unwrap();
        fs::write(legacy_sandbox.join("state_5.sqlite"), "legacy-state").unwrap();
        let sandbox_info = test_sandbox_info();
        let instance_ids = ["codex-isolated-home-one", "codex-isolated-home-two"];
        let configs: Vec<_> = instance_ids
            .iter()
            .map(|instance_id| {
                build_container_config(
                    project_dir.path().to_str().unwrap(),
                    &sandbox_info,
                    ContainerAgentSelection::new("codex", None),
                    false,
                    instance_id,
                    None,
                    "",
                )
                .unwrap()
            })
            .collect();

        let homes: Vec<_> = instance_ids
            .iter()
            .map(|instance_id| {
                temp_home
                    .path()
                    .join(".codex")
                    .join(SANDBOX_PRIVATE_SUBDIR)
                    .join(instance_id)
            })
            .collect();
        assert_ne!(homes[0], homes[1]);
        for home in &homes {
            assert_eq!(fs::read(home.join("auth.json")).unwrap(), b"legacy-auth");
            assert!(
                !home.join("state_5.sqlite").exists(),
                "legacy SQLite state must not enter an isolated Codex home"
            );
        }
        fs::write(homes[0].join("state_5.sqlite"), "first").unwrap();
        fs::write(homes[1].join("state_5.sqlite"), "second").unwrap();
        assert_eq!(fs::read(homes[0].join("state_5.sqlite")).unwrap(), b"first");
        assert_eq!(
            fs::read(homes[1].join("state_5.sqlite")).unwrap(),
            b"second"
        );

        for ((config, home), instance_id) in configs.iter().zip(&homes).zip(instance_ids) {
            assert!(config.volumes.iter().any(|volume| {
                volume.host_path == home.to_string_lossy()
                    && volume.container_path == format!("/root/.codex/{instance_id}")
            }));
            assert!(config.environment.iter().any(|entry| {
                entry.key() == "CODEX_HOME"
                    && entry.value() == format!("/root/.codex/{instance_id}")
            }));
        }
        for instance_id in instance_ids {
            crate::hooks::cleanup_hook_status_dir(instance_id);
        }
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_config_seed_never_imports_or_refreshes_native_history() {
        let (_hooks, _, _hook_dir) = BaseGuard::ready();
        let home = TempDir::new().unwrap();
        let _env = crate::session::test_support::isolate_app_dir_at(home.path());
        let projects = [TempDir::new().unwrap(), TempDir::new().unwrap()];
        for project in &projects {
            git2::Repository::init(project.path()).unwrap();
        }
        let mut violations = Vec::new();
        let mut next_id = 0xa000_u64;
        for declared in [false, true] {
            for (tool, relative, histories) in [
                ("codex", ".codex", &["history.jsonl", "state_5.sqlite"][..]),
                ("kimi", ".kimi-code", &["session_index.jsonl"][..]),
                ("pi", ".pi", &["agent/sessions/conversation.jsonl"][..]),
                ("omp", ".omp", &["agent/sessions/conversation.jsonl"][..]),
            ] {
                let host = if declared {
                    home.path().join(format!("declared-{tool}"))
                } else {
                    home.path().join(relative)
                };
                fs::create_dir_all(&host).unwrap();
                let app = crate::session::get_app_dir().unwrap();
                fs::write(
                    app.join("config.toml"),
                    if declared {
                        format!("[session.agent_config_dir]\n{tool} = {host:?}\n")
                    } else {
                        String::new()
                    },
                )
                .unwrap();
                for history in histories {
                    let path = host.join(history);
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(path, "HOST_HISTORY").unwrap();
                }
                // Siblings share a project; the third owner has another project.
                for project in [0, 0, 1] {
                    let id = format!("{next_id:016x}");
                    next_id += 1;
                    let config = build_container_config(
                        projects[project].path().to_str().unwrap(),
                        &test_sandbox_info(),
                        ContainerAgentSelection::new(tool, None),
                        false,
                        &id,
                        None,
                        "",
                    )
                    .unwrap();
                    let store = host.join(SANDBOX_PRIVATE_SUBDIR).join(&id);
                    assert!(config
                        .volumes
                        .iter()
                        .any(|volume| { Path::new(&volume.host_path) == store }));
                    for history in histories {
                        let path = store.join(history);
                        if path.exists() {
                            violations.push(format!("{tool}/{id}: imported {history}"));
                        }
                        fs::create_dir_all(path.parent().unwrap()).unwrap();
                        fs::write(path, format!("LOCAL_{id}")).unwrap();
                    }
                    refresh_agent_configs_for_instance(
                        &crate::session::config::effective_profile(""),
                        &id,
                        tool,
                        None,
                        CredentialFold::Freshest,
                        Path::new("/workspace"),
                    );
                    for history in histories {
                        if fs::read_to_string(store.join(history)).unwrap() != format!("LOCAL_{id}")
                        {
                            violations.push(format!("{tool}/{id}: overwrote {history}"));
                        }
                    }
                    crate::hooks::cleanup_hook_status_dir(&id);
                }
            }
        }
        assert!(
            violations.is_empty(),
            "native history crossed config seeding: {violations:#?}"
        );
    }

    // Issue #472: a YOLO-mode sandbox session must disable the agent's
    // folder-trust prompt so the ephemeral container does not re-prompt on
    // every launch.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_yolo_seeds_codex_and_gemini_folder_trust() {
        for (tool, dir, file) in [
            ("codex", ".codex", "config.toml"),
            ("gemini", ".gemini", "settings.json"),
        ] {
            let home = IsolatedHome::new();
            let project_dir = TempDir::new().unwrap();
            git2::Repository::init(project_dir.path()).unwrap();
            let instance_id = format!("{tool}-yolo-trust-test");
            let config = Build::new(tool)
                .yolo(true)
                .instance(&instance_id)
                .run(project_dir.path())
                .unwrap();
            let path = home
                .path()
                .join(dir)
                .join(SANDBOX_PRIVATE_SUBDIR)
                .join(&instance_id)
                .join(file);
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("yolo {tool} sandbox must write {file}: {err}"));
            if tool == "codex" {
                let parsed: toml::Value = toml::from_str(&text).unwrap();
                // The trust key is the in-container working dir, not the host path.
                assert_eq!(
                    parsed["projects"][config.working_dir.as_str()]["trust_level"].as_str(),
                    Some("trusted")
                );
            } else {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(
                    parsed["security"]["folderTrust"]["enabled"],
                    serde_json::Value::Bool(false)
                );
            }
            crate::hooks::cleanup_hook_status_dir(&instance_id);
        }
    }

    // Claude Code's folder-trust dialog is keyed on the git root, so every
    // container workspace is a fresh key and the dialog blocks startup. Unlike
    // Codex and Gemini this is not an approval gate, so it is seeded in both
    // YOLO and non-YOLO sessions, and it must merge into the onboarding state
    // the same file already carries (issue #472).
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_seeds_claude_folder_trust() {
        for is_yolo in [false, true] {
            let (_hg, _, _tmp_base) = BaseGuard::ready();
            let temp_home = TempDir::new().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

            let project_dir = TempDir::new().unwrap();
            git2::Repository::init(project_dir.path()).unwrap();

            let sandbox_info = crate::session::instance::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "test-container".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            };
            let instance_id = format!("claude-trust-test-{is_yolo}");
            let config = build_container_config(
                project_dir.path().to_str().unwrap(),
                &sandbox_info,
                ContainerAgentSelection::new("claude", None),
                is_yolo,
                &instance_id,
                None,
                "",
            )
            .unwrap();

            let seeded = temp_home
                .path()
                .join(".claude")
                .join(SANDBOX_PRIVATE_SUBDIR)
                .join(&instance_id)
                .join(".claude.json");
            let parsed: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&seeded).unwrap()).unwrap();
            // The trust key is the in-container working dir, not the host path.
            assert_eq!(
                parsed["projects"][&config.working_dir]["hasTrustDialogAccepted"].as_bool(),
                Some(true),
                "yolo={is_yolo}: container workspace must be pre-trusted"
            );
            // The same file carries onboarding state seeded by the mount.
            assert_eq!(
                parsed["hasCompletedOnboarding"].as_bool(),
                Some(true),
                "yolo={is_yolo}: trust seed must merge, not replace"
            );

            crate::hooks::cleanup_hook_status_dir(&instance_id);
        }
    }

    // A wrapper that points its CLI at another config dir (one binary, two
    // accounts) makes the staged default a directory the agent never opens, so
    // the seed has to follow `session.agent_config_dir` instead. Keyed on the
    // session's own agent name, so a plain `claude` session in the same profile
    // keeps the default.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_seeds_declared_agent_config_dir() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let declared = temp_home.path().join(".claude-personal");
        let app_dir = crate::session::get_app_dir().unwrap();
        fs::write(
            app_dir.join("config.toml"),
            r#"
[session.custom_agents]
claude-personal = "claude"

[session.agent_detect_as]
claude-personal = "claude"

[session.agent_config_dir]
claude-personal = "~/.claude-personal"
"#,
        )
        .unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let instance_id = "declared-config-dir-test";
        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("claude-personal", Some("claude")),
            false,
            instance_id,
            None,
            "",
        )
        .unwrap();
        let staged = declared.join(SANDBOX_PRIVATE_SUBDIR).join(instance_id);
        assert!(config.volumes.iter().any(|volume| {
            Path::new(&volume.host_path) == staged
                && volume.container_path == "/root/.claude"
                && !volume.read_only
        }));
        assert!(
            staged.join("settings.json").is_file(),
            "sandbox hooks must be installed into the mounted declared store"
        );

        let seeded: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(staged.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            seeded["projects"][&config.working_dir]["hasTrustDialogAccepted"].as_bool(),
            Some(true),
            "the declared config dir must carry the trust record"
        );

        let default_config = temp_home
            .path()
            .join(".claude")
            .join(SANDBOX_PRIVATE_SUBDIR)
            .join(instance_id)
            .join(".claude.json");
        let default_trust = fs::read_to_string(&default_config)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .map(|c| c["projects"][&config.working_dir].clone())
            .unwrap_or(serde_json::Value::Null);
        assert!(
            default_trust.is_null(),
            "the built-in config dir must be left alone, got {default_trust}"
        );

        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    // Declared Codex roots stage one writable store per instance, then mount
    // that staged directory directly at CODEX_HOME.
    #[test]
    #[serial_test::serial]
    fn test_declared_codex_config_dir_trusts_at_the_mounted_level() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let declared = temp_home.path().join(".codex-work");
        let app_dir = crate::session::get_app_dir().unwrap();
        fs::write(
            app_dir.join("config.toml"),
            format!(
                r#"
[session.custom_agents]
codex-work = "codex"

[session.agent_detect_as]
codex-work = "codex"

[session.agent_config_dir]
codex-work = "{}"
"#,
                declared.display()
            ),
        )
        .unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let instance_id = "declared-codex-dir-test";
        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("codex-work", Some("codex")),
            true,
            instance_id,
            None,
            "",
        )
        .unwrap();
        let codex_home = config
            .environment
            .iter()
            .find(|entry| entry.key() == "CODEX_HOME")
            .map(EnvEntry::value)
            .expect("managed CODEX_HOME");
        let staged = declared.join(SANDBOX_PRIVATE_SUBDIR).join(instance_id);
        assert!(config.volumes.iter().any(|volume| {
            Path::new(&volume.host_path) == staged
                && volume.container_path == codex_home
                && !volume.read_only
        }));
        assert!(
            staged.join("hooks.json").is_file(),
            "Codex hooks must be installed into the mounted declared store"
        );
        let trusted = fs::read_to_string(staged.join("config.toml")).unwrap();
        assert!(
            trusted.contains(&config.working_dir) && trusted.contains("trusted"),
            "the mounted directory itself must carry the trust record, got {trusted}"
        );
        assert!(
            !declared.join(SANDBOX_SUBDIR).join("config.toml").exists(),
            "the legacy shared store must remain unused"
        );
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    // The wholesale folder-trust disable is only safe against a config AoE
    // stages for one container. In a config dir the user named (and also runs
    // on the host) it would outlive the session, so that arm trusts one path.
    #[test]
    fn test_apply_folder_trust_config_gemini_disable_is_staged_only() {
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|m| m.tool_name == "gemini")
            .unwrap();

        for staged in [true, false] {
            let dir = TempDir::new().unwrap();
            apply_folder_trust_config(mount, dir.path(), "/workspace/wt", true, staged).unwrap();

            let settings = dir.path().join("settings.json");
            let trusted = dir.path().join("trustedFolders.json");
            if staged {
                let parsed: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
                assert_eq!(
                    parsed["security"]["folderTrust"]["enabled"],
                    serde_json::json!(false)
                );
                assert!(!trusted.exists());
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(&trusted).unwrap()).unwrap();
                assert_eq!(parsed["/workspace/wt"], serde_json::json!("TRUST_FOLDER"));
                assert!(!settings.exists(), "folder trust must stay enabled");
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_ensure_folder_trust_config_restores_trust_after_refresh() {
        // (agent, config dir, file, host content, staged sandbox trust, host key, trust pointer, trust value)
        let cases = [
            (
                "codex",
                ".codex",
                "config.toml",
                r#"model = "host""#,
                "[projects.\"/workspace/project\"]\ntrust_level = \"trusted\"\n",
                "/model",
                "/projects/~1workspace~1project/trust_level",
                serde_json::json!("trusted"),
            ),
            (
                "gemini",
                ".gemini",
                "settings.json",
                r#"{"theme":"host"}"#,
                r#"{"security":{"folderTrust":{"enabled":false}}}"#,
                "/theme",
                "/security/folderTrust/enabled",
                serde_json::json!(false),
            ),
        ];
        for (agent, dir, file, host, staged, host_key, trust, trusted) in cases {
            let (_hg, _, _tmp_base) = BaseGuard::ready();
            let temp_home = TempDir::new().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
            let agent_dir = temp_home.path().join(dir);
            let instance_id = format!("{agent}-yolo-refresh-test");
            let sandbox = agent_dir.join(SANDBOX_PRIVATE_SUBDIR).join(&instance_id);
            fs::create_dir_all(&sandbox).unwrap();
            fs::write(agent_dir.join(file), host).unwrap();
            fs::write(sandbox.join(file), staged).unwrap();
            certify_fixture_content(&sandbox, dir).unwrap();
            let read = || -> serde_json::Value {
                let text = fs::read_to_string(sandbox.join(file)).unwrap();
                if file.ends_with(".toml") {
                    serde_json::to_value(toml::from_str::<toml::Value>(&text).unwrap()).unwrap()
                } else {
                    serde_json::from_str(&text).unwrap()
                }
            };

            refresh_agent_configs_for_instance(
                &crate::session::config::effective_profile(""),
                &instance_id,
                agent,
                None,
                CredentialFold::Freshest,
                Path::new("/workspace"),
            );
            let refreshed = read();
            assert_eq!(
                refreshed.pointer(host_key),
                Some(&serde_json::json!("host")),
                "{agent}"
            );
            assert_eq!(
                refreshed.pointer(trust),
                None,
                "{agent}: refresh drops staged trust"
            );

            ensure_folder_trust_config_for_active_agent(
                agent,
                None,
                "",
                &instance_id,
                "/workspace/project",
                true,
            );
            let restored = read();
            assert_eq!(
                restored.pointer(host_key),
                Some(&serde_json::json!("host")),
                "{agent}"
            );
            assert_eq!(
                restored.pointer(trust),
                Some(&trusted),
                "{agent}: trust restored"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn user_volume_shadowing_hook_config_disables_publisher_evidence() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let shadow = temp_home.path().join("shadow-cursor");
        fs::create_dir_all(&shadow).unwrap();
        crate::session::config::update_config(|config| {
            config.sandbox.extra_volumes =
                vec![format!("{}:/root/.cursor", shadow.to_string_lossy())];
        })
        .unwrap();
        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();
        let sandbox_info = crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("cursor", None),
            false,
            "cursor-shadow-test",
            None,
            "",
        )
        .unwrap();

        assert!(!config.identity_publisher_installed);

        let output_shadow = temp_home.path().join("shadow-output");
        fs::create_dir_all(&output_shadow).unwrap();
        crate::session::config::update_config(|config| {
            config.sandbox.extra_volumes = vec![format!(
                "{}:{}/cursor-output-shadow",
                output_shadow.to_string_lossy(),
                crate::hooks::HOOK_STATUS_BASE_IN_CONTAINER
            )];
        })
        .unwrap();
        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("cursor", None),
            false,
            "cursor-output-shadow",
            None,
            "",
        )
        .unwrap();
        assert!(!config.identity_publisher_installed);

        let hook_dir = crate::hooks::ensure_instance_dir_path("cursor-readonly").unwrap();
        crate::session::config::update_config(|config| {
            config.sandbox.extra_volumes = vec![format!(
                "{}:{}/cursor-readonly:ro",
                hook_dir.to_string_lossy(),
                crate::hooks::HOOK_STATUS_BASE_IN_CONTAINER
            )];
        })
        .unwrap();
        let config = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("cursor", None),
            false,
            "cursor-readonly",
            None,
            "",
        )
        .unwrap();
        assert!(!config.identity_publisher_installed);

        crate::session::config::update_config(|config| {
            config.sandbox.extra_volumes.clear();
            config.sandbox.environment = vec!["HOME=/alternate-home".to_string()];
        })
        .unwrap();
        let error = build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("cursor", None),
            false,
            "cursor-home-override",
            None,
            "",
        )
        .err()
        .expect("conflicting HOME must be rejected");
        assert!(error.to_string().contains("HOME=/alternate-home"));
        assert!(error
            .to_string()
            .contains("remove this sandbox environment"));
        assert!(!temp_home
            .path()
            .join(".cursor/sandbox-v2/cursor-home-override")
            .exists());
    }

    #[test]
    #[serial_test::serial]
    fn every_sandboxable_agent_mounts_its_own_content_root() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let project = TempDir::new().unwrap();
        git2::Repository::init(project.path()).unwrap();
        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".into(),
            container_name: "test-container".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        for agent in crate::agents::AGENTS
            .iter()
            .filter(|agent| !agent.host_only)
        {
            let mounts: Vec<_> = AGENT_CONFIG_MOUNTS
                .iter()
                .filter(|mount| mount.tool_name == agent.name)
                .collect();
            assert!(
                !mounts.is_empty(),
                "{} has no private content mount",
                agent.name
            );
            let id = format!("{}-content-mount-test", agent.name);
            let config = build_container_config(
                project.path().to_str().unwrap(),
                &sandbox_info,
                ContainerAgentSelection::new(agent.name, None),
                false,
                &id,
                None,
                "",
            )
            .unwrap();
            for mount in mounts {
                let source = sandbox_dir_for(mount, home.path(), Some(&id)).unwrap();
                let target = if agent.name == "codex" {
                    Path::new("/root/.codex").join(&id)
                } else {
                    Path::new("/root").join(mount.container_suffix)
                };
                assert!(
                    config.volumes.iter().any(|volume| {
                        Path::new(&volume.host_path) == source
                            && Path::new(&volume.container_path) == target
                    }),
                    "{} omitted private mount {}",
                    agent.name,
                    mount.container_suffix
                );
            }
        }
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
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

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

            let sandbox_info = crate::session::instance::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "test-container".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
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

            let expects_identity = crate::agents::resolved_sidecar_hook_events(
                agent,
                &crate::session::config::profile_config::resolve_config_or_warn(""),
            )
            .unwrap()
            .iter()
            .any(|event| event.identity_field.is_some());
            assert_eq!(
                config.identity_publisher_installed, expects_identity,
                "{} publisher evidence",
                agent.name
            );

            let mount = AGENT_CONFIG_MOUNTS
                .iter()
                .find(|mount| mount.tool_name == agent.name)
                .unwrap();
            let relative = Path::new(sidecar.sandbox_config_subpath)
                .strip_prefix(Path::new(mount.host_rel).join(SANDBOX_SUBDIR))
                .unwrap();
            let sandbox_config = sandbox_dir_for(mount, temp_home.path(), Some(&instance_id))
                .unwrap()
                .join(relative);
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
            // Canonicalize for comparison (handles /var -> /private/var on
            // macOS); the mount source is the resolved real path since #3240.
            let hook_dir = hook_dir.canonicalize().unwrap();
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
    fn sandbox_identity_hooks_remain_when_status_hooks_are_disabled() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let profile = "sandbox-identity-only-hooks";
        let profile_dir = crate::session::get_profile_dir(profile).unwrap();
        fs::write(
            profile_dir.join("config.toml"),
            "[session]\nagent_status_hooks = false\n",
        )
        .unwrap();
        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();
        let instance_id = "cursor-identity-only-sandbox-test";
        build_container_config(
            project_dir.path().to_str().unwrap(),
            &crate::session::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "test-container".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            },
            ContainerAgentSelection::new("cursor", None),
            false,
            instance_id,
            None,
            profile,
        )
        .unwrap();

        let hooks: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(
                temp_home
                    .path()
                    .join(".cursor")
                    .join(SANDBOX_PRIVATE_SUBDIR)
                    .join(instance_id)
                    .join("hooks.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let entries = hooks["hooks"]["beforeSubmitPrompt"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0]["command"]
            .as_str()
            .unwrap()
            .contains("conversation_id"));
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    // #2381 (sandbox side): a sandboxed Kiro session launched with `--agent
    // NAME` loads NAME's config inside the container, which has no AoE hooks, so
    // status detection goes dark. When a selected agent is threaded in, the
    // sandbox install must target NAME's staged config file, not the standalone
    // aoe-hooks agent.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_resolves_selected_kiro_agent_by_name_in_sandbox() {
        // The host `.kiro/agents` dir is staged into `.kiro/sandbox/agents`
        // before hook install, so a prefixed agent file (filename != name) must
        // be resolved by its `name` field there, mirroring the host path.
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let host_agents = temp_home.path().join(".kiro/agents");
        std::fs::create_dir_all(&host_agents).unwrap();
        std::fs::write(
            host_agents.join("TeamAgents-custom-agent.json"),
            r#"{"name":"custom-agent","hooks":{"agentSpawn":[{"command":"team-tool emit"}]}}"#,
        )
        .unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let instance_id = "kiro-managed-agent-sandbox-test";
        build_container_config(
            project_dir.path().to_str().unwrap(),
            &crate::session::instance::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "test-container".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            },
            ContainerAgentSelection::new("kiro", None).with_selected_agent(Some("custom-agent")),
            false,
            instance_id,
            None,
            "",
        )
        .unwrap();

        let matched = temp_home
            .path()
            .join(".kiro")
            .join(SANDBOX_PRIVATE_SUBDIR)
            .join(instance_id)
            .join("agents/TeamAgents-custom-agent.json");
        assert!(
            matched.exists(),
            "hooks should install into the name-matched staged file at {}",
            matched.display()
        );
        let body = fs::read_to_string(&matched).unwrap();
        assert!(
            body.contains("aoe-hooks"),
            "AoE hook command must be present"
        );
        assert!(
            body.contains("agentSpawn"),
            "the agent's own hook must be preserved"
        );

        let stem_clone = temp_home
            .path()
            .join(".kiro")
            .join(SANDBOX_PRIVATE_SUBDIR)
            .join(instance_id)
            .join("agents/custom-agent.json");
        assert!(
            !stem_clone.exists(),
            "must not create a filename-stem clone the CLI never loads"
        );
        let standalone = temp_home.path().join(
            crate::agents::get_agent("kiro")
                .unwrap()
                .sidecar_hooks
                .as_ref()
                .unwrap()
                .sandbox_config_subpath,
        );
        assert!(
            !standalone.exists(),
            "standalone aoe-hooks sandbox config must not be written when an agent is selected"
        );

        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_refuses_unsafe_instance_id() {
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let result = Build::new("codex")
            .instance("../etc")
            .run(project_dir.path());

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

    /// The profile decides codex sandbox hooks: disabling them installs nothing, and a custom
    /// wrapper detected as codex gets them even when the global setting is off.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_codex_hooks_follow_the_profile() {
        // (profile, profile config, global hooks, tool, hooks installed)
        for (profile, profile_config, global_hooks, tool, installed) in [
            (
                "sandbox-hooks-disabled",
                "[session]\nagent_status_hooks = false\n",
                true,
                "codex",
                false,
            ),
            (
                "sandbox-wrapped-codex",
                "[session]\nagent_status_hooks = true\nagent_detect_as = { \"wrapped-codex\" = \"codex\" }\n",
                false,
                "wrapped-codex",
                true,
            ),
        ] {
            let (_hg, _, _tmp_base) = BaseGuard::ready();
            let temp_home = TempDir::new().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
            crate::session::config::update_config(|global| {
                global.session.agent_status_hooks = global_hooks;
            })
            .unwrap();
            let profile_dir = crate::session::get_profile_dir(profile).unwrap();
            fs::write(profile_dir.join("config.toml"), profile_config).unwrap();
            let project_dir = TempDir::new().unwrap();
            git2::Repository::init(project_dir.path()).unwrap();
            let instance_id = format!("{profile}-test");
            // build_container_config installs the profile's agent_detect_as globally.
            let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);
            let config = build_container_config(
                project_dir.path().to_str().unwrap(),
                &test_sandbox_info(),
                ContainerAgentSelection::new(tool, None),
                false,
                &instance_id,
                None,
                profile,
            )
            .unwrap();

            let codex_sandbox = temp_home
                .path()
                .join(".codex")
                .join(SANDBOX_PRIVATE_SUBDIR)
                .join(&instance_id);
            assert!(!codex_sandbox.join("config.toml").exists(), "{profile}");
            assert_eq!(codex_sandbox.join("hooks.json").exists(), installed, "{profile}");
            if installed {
                let codex_hooks = fs::read_to_string(codex_sandbox.join("hooks.json")).unwrap();
                let parsed: serde_json::Value = serde_json::from_str(&codex_hooks).unwrap();
                assert!(parsed["hooks"]["PreToolUse"].is_array());
                assert!(codex_hooks.contains("aoe-hooks"));
                assert!(config.volumes.iter().any(|v| {
                    v.host_path == codex_sandbox.to_string_lossy()
                        && v.container_path == format!("/root/.codex/{instance_id}")
                }));
            }
            let hook_dir = crate::hooks::hook_status_dir(&instance_id).unwrap();
            // Without hooks the directory is never created, so compare lexically.
            let hook_dir = hook_dir.canonicalize().unwrap_or(hook_dir);
            assert_eq!(
                config
                    .volumes
                    .iter()
                    .any(|v| v.host_path == hook_dir.to_string_lossy()),
                installed,
                "{profile}: status hook directory mount"
            );
            crate::hooks::cleanup_hook_status_dir(&instance_id);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_refresh_agent_configs_preserves_codex_hooks_and_trust_state() {
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let codex_dir = temp_home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(codex_dir.join("config.toml"), r#"model = "initial""#).unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
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

        let codex_sandbox = codex_dir.join(SANDBOX_PRIVATE_SUBDIR).join(instance_id);
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

        refresh_agent_configs_for_instance(
            &crate::session::config::effective_profile(""),
            instance_id,
            "codex",
            None,
            CredentialFold::Freshest,
            Path::new("/workspace"),
        );

        let config_text = fs::read_to_string(&sandbox_config_path).unwrap();
        let config: toml::Value = toml::from_str(&config_text).unwrap();
        assert_eq!(config["model"].as_str(), Some("updated"));
        assert_eq!(
            config["hooks"]["state"]["trusted"]["trusted_hash"].as_str(),
            Some("keep")
        );

        let hooks_path = codex_sandbox.join("hooks.json");
        let hooks_text = fs::read_to_string(&hooks_path).unwrap();
        let hooks: serde_json::Value = serde_json::from_str(&hooks_text).unwrap();
        assert!(
            hooks["hooks"]["PreToolUse"].is_array(),
            "PreToolUse array must be installed in sandbox hooks.json"
        );
        assert!(hooks_text.contains("aoe-hooks"));
        crate::hooks::cleanup_hook_status_dir(instance_id);
    }

    #[test]
    fn test_refresh_codex_sandbox_hooks_honors_codex_feature_opt_out() {
        let temp = TempDir::new().unwrap();
        let profile_config = crate::session::config::Config::default();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "codex")
            .unwrap();

        let writes = [
            ("hooks-off", "[features]\nhooks = false\n"),
            ("legacy-hooks-off", "[features]\ncodex_hooks = false\n"),
            ("hooks-on", "[features]\nhooks = true\n"),
        ]
        .map(|(case, config)| {
            let sandbox_dir = temp.path().join(case);
            fs::create_dir_all(&sandbox_dir).unwrap();
            fs::write(sandbox_dir.join("config.toml"), config).unwrap();

            refresh_codex_sandbox_hooks(mount, &sandbox_dir, &profile_config);

            sandbox_dir.join("hooks.json").exists()
        });

        assert_eq!(writes, [false, false, true]);
    }

    #[test]
    #[serial_test::serial]
    fn test_refresh_agent_configs_uses_profile_status_map_for_codex_hooks() {
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let codex_dir = temp_home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(codex_dir.join("config.toml"), r#"model = "initial""#).unwrap();

        let mut work_config = super::super::profile_config::ProfileConfig::default();
        work_config.overrides.insert(
            "agents".to_string(),
            serde_json::json!({
                "codex": {
                    "status_map": {
                        "PreToolUse": "waiting"
                    }
                }
            }),
        );
        super::super::profile_config::save_profile_config("work", &work_config).unwrap();
        let mut personal_config = super::super::profile_config::ProfileConfig::default();
        personal_config.overrides.insert(
            "agents".to_string(),
            serde_json::json!({
                "codex": {
                    "status_map": {
                        "PreToolUse": "running"
                    }
                }
            }),
        );
        super::super::profile_config::save_profile_config("personal", &personal_config).unwrap();

        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let instances = [
            ("codex-work-status-map-refresh-test", "work", "waiting"),
            (
                "codex-personal-status-map-refresh-test",
                "personal",
                "running",
            ),
        ];
        for (instance_id, profile, _) in instances {
            build_container_config(
                project_dir.path().to_str().unwrap(),
                &sandbox_info,
                ContainerAgentSelection::new("codex", None),
                false,
                instance_id,
                None,
                profile,
            )
            .unwrap();
            refresh_agent_configs_for_instance(
                profile,
                instance_id,
                "codex",
                None,
                CredentialFold::Freshest,
                Path::new("/workspace"),
            );
        }

        for (instance_id, _, expected_status) in instances {
            let hooks_path = codex_dir
                .join(SANDBOX_PRIVATE_SUBDIR)
                .join(instance_id)
                .join("hooks.json");
            let hooks: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
            let cmd = hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(
                cmd.contains(&format!("printf {expected_status}")),
                "got command: {cmd}"
            );
            crate::hooks::cleanup_hook_status_dir(instance_id);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_build_container_config_mounts_codex_home_from_configured_env() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();

        for (instance_id, from_extra_env, container_home) in [
            (
                "codex-sandbox-extra-env-hooks-test",
                true,
                "/root/custom-codex",
            ),
            (
                "codex-sandbox-config-env-hooks-test",
                false,
                "/root/profile-codex",
            ),
        ] {
            let mut info = test_sandbox_info();
            let entry = format!("CODEX_HOME={container_home}");
            if from_extra_env {
                info.extra_env = Some(vec![entry]);
            } else {
                fs::write(
                    crate::session::get_app_dir().unwrap().join("config.toml"),
                    format!("[sandbox]\nenvironment = [\"{entry}\"]\n"),
                )
                .unwrap();
            }
            let config = Build::new("codex")
                .info(info)
                .instance(instance_id)
                .run(project_dir.path())
                .unwrap();

            let codex_sandbox = temp_home
                .path()
                .join(".codex")
                .join(SANDBOX_PRIVATE_SUBDIR)
                .join(instance_id);
            assert!(codex_sandbox.join("hooks.json").exists(), "{instance_id}");
            let mounted_at = |path: &str| {
                config.volumes.iter().any(|v| {
                    v.host_path == codex_sandbox.to_string_lossy() && v.container_path == path
                })
            };
            assert!(mounted_at(container_home), "{instance_id}");
            assert!(!mounted_at("/root/.codex"), "{instance_id}");
            crate::hooks::cleanup_hook_status_dir(instance_id);
        }
    }

    /// Regression test: when an instance was created under a non-default profile,
    /// `build_container_config` must resolve sandbox overrides (extra_volumes here)
    /// against THAT profile, not the user's globally configured default profile.
    /// Pre-fix, the TUI's container creation flow always picked up the global
    /// default's volumes regardless of which profile the session was launched under.
    #[test]
    #[serial_test::serial]
    fn test_build_container_config_uses_passed_profile_not_global_default() {
        let (_hg, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

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
        let cfg_personal = Build::new("claude")
            .profile("personal")
            .run(project_dir.path())
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
        let cfg_default = Build::new("claude")
            .profile("default")
            .run(project_dir.path())
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
        let cfg_empty = Build::new("claude").run(project_dir.path()).unwrap();
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
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

        let (_dir, repo_path) = setup_regular_repo();

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

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
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
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());

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

        let sandbox_info = crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
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

    #[test]
    #[serial_test::serial]
    fn test_opencode_auth_seed_does_not_import_host_database() {
        let (_hook_guard, _, _application) = BaseGuard::ready();
        let home = TempDir::new().unwrap();
        let host_dir = home.path().join(".local/share/opencode");
        let sandbox_dir = host_dir.join("sandbox");
        fs::create_dir_all(&host_dir).unwrap();

        // Host has a database that should NOT be copied
        fs::write(host_dir.join("opencode.db"), "host-db").unwrap();
        // Host also has a config file that SHOULD be copied
        fs::write(host_dir.join("auth.json"), "config").unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.host_rel == ".local/share/opencode")
            .unwrap();

        prepare_owned_fixture(
            &mount,
            home.path(),
            None,
            CredentialFold::Freshest,
            &crate::session::config::SessionConfig::default(),
            home.path(),
        )
        .unwrap();

        assert!(
            !sandbox_dir.join("opencode.db").exists(),
            "Host database should not be copied to sandbox"
        );
        assert!(
            sandbox_dir.join("auth.json").exists(),
            "Positive credential file must still be copied"
        );
    }

    // --- GCP credential mount tests ---
    //
    // These exercise the Vertex AI cred mount inside `build_container_config`.
    // They use `serial_test::serial` because they mutate process-wide env vars,
    // and isolate `HOME`/`XDG_CONFIG_HOME` so global config doesn't bleed in.

    fn test_sandbox_info() -> crate::session::instance::SandboxInfo {
        crate::session::instance::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
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
        Build::new(tool).run(project_dir).unwrap()
    }

    /// The ADC mount is Claude-only, needs a non-empty `CLAUDE_CODE_USE_VERTEX`
    /// and an existing credential file, and prefers an explicit
    /// `GOOGLE_APPLICATION_CREDENTIALS` over the well-known host path.
    #[test]
    #[serial_test::serial]
    fn vertex_adc_is_mounted_only_for_claude_with_the_flag_and_a_file() {
        const TARGET: &str = "/root/.config/gcloud/application_default_credentials.json";
        // (CLAUDE_CODE_USE_VERTEX, tool, default ADC on disk, custom credential, mounted)
        let cases: &[(Option<&str>, &str, bool, bool, bool)] = &[
            (Some("1"), "claude", true, false, true),
            (Some("1"), "claude", false, true, true),
            (None, "claude", true, false, false),
            (Some("1"), "opencode", true, false, false),
            (Some(""), "claude", true, false, false),
            (Some("1"), "claude", false, false, false),
        ];

        for &(flag, tool, write_default, write_custom, mounted) in cases {
            let label = format!("flag={flag:?} tool={tool}");
            let temp_home = TempDir::new().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
            match flag {
                Some(value) => std::env::set_var("CLAUDE_CODE_USE_VERTEX", value),
                None => std::env::remove_var("CLAUDE_CODE_USE_VERTEX"),
            }
            let default_adc = write_default.then(|| write_adc_at(temp_home.path()));
            let cred_dir = TempDir::new().unwrap();
            let custom = write_custom.then(|| {
                let path = cred_dir.path().join("custom-key.json");
                fs::write(&path, r#"{"type":"service_account"}"#).unwrap();
                path
            });
            match custom.as_ref() {
                Some(path) => std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", path),
                None => std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS"),
            }

            let project_dir = TempDir::new().unwrap();
            let config = run_build_for_vertex_test(tool, project_dir.path());
            let mount = config.volumes.iter().find(|v| v.container_path == TARGET);

            match mount {
                Some(mount) => {
                    assert!(mounted, "unexpected ADC mount for {label}");
                    let host = custom.or(default_adc).unwrap();
                    assert_eq!(mount.host_path, host.to_string_lossy(), "{label}");
                    assert!(mount.read_only, "{label}");
                }
                None => assert!(!mounted, "missing ADC mount for {label}"),
            }
        }

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
    }

    /// The reporter's layout (#3742): a sibling-worktree session whose worktree
    /// moved from otari-worktrees/905 to otari-worktrees/rev-912. The main repo's
    /// mount does not move, so its volume must never be named.
    fn moved_config() -> ContainerConfig {
        ContainerConfig {
            working_dir: "/workspace/otari-worktrees/rev-912".to_string(),
            named_ignore_volumes: vec![
                NamedVolumeMount {
                    volume_name: named_volume_for("sess1", "/workspace/otari/target"),
                    container_path: "/workspace/otari/target".to_string(),
                },
                NamedVolumeMount {
                    volume_name: named_volume_for(
                        "sess1",
                        "/workspace/otari-worktrees/rev-912/target",
                    ),
                    container_path: "/workspace/otari-worktrees/rev-912/target".to_string(),
                },
            ],
            named_ignore_volumes_authoritative: true,
            ..Default::default()
        }
    }

    #[test]
    fn stranded_volumes_are_only_the_moved_paths_old_names() {
        // The literal suffix is the same `DefaultHasher` canary as in
        // `containers::runtime_base`: a toolchain bump that changed it would orphan
        // every existing named volume.
        const MOVED: &str = "aoe-vi-sess1-workspace-otari-worktrees-905-target-31ddd0322290";
        let moved_from = Some("/workspace/otari-worktrees/905");
        // The main repo's `**/bin` is absent from the host this run; its container
        // path did not move, so its volume is a live cache, not a strand.
        let mut unmounted_main = moved_config();
        unmounted_main.named_ignore_volumes.remove(0);
        // A worktree leaf that slugs to the repo's own name remaps onto the main
        // repo's volume, which the create is about to mount.
        let remap = ContainerConfig {
            working_dir: "/workspace/otari-worktrees/otari".to_string(),
            named_ignore_volumes: [
                "/workspace/otari/target",
                "/workspace/otari-worktrees/otari/target",
            ]
            .into_iter()
            .map(|path| NamedVolumeMount {
                volume_name: named_volume_for("sess1", path),
                container_path: path.to_string(),
            })
            .collect(),
            named_ignore_volumes_authoritative: true,
            ..Default::default()
        };
        // The workdir is provisional too, so the apparent move may be nothing but
        // a find_main_repo failure.
        let degraded = ContainerConfig {
            named_ignore_volumes_authoritative: false,
            ..moved_config()
        };

        for (case, config, previous_workdir, expected) in [
            ("a moved worktree", moved_config(), moved_from, vec![MOVED]),
            (
                "an unmounted unmoved mount",
                unmounted_main,
                moved_from,
                vec![MOVED],
            ),
            (
                "a remap onto a live volume",
                remap,
                Some("/workspace/otari"),
                vec![],
            ),
            (
                "the workdir did not move",
                moved_config(),
                Some("/workspace/otari-worktrees/rev-912"),
                vec![],
            ),
            ("no pinned workdir", moved_config(), None, vec![]),
            ("a degraded mount resolve", degraded, moved_from, vec![]),
        ] {
            assert_eq!(
                stranded_named_ignore_volumes(&config, "sess1", previous_workdir),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn named_volume_for_is_stable_prefixed_sanitized_and_distinct() {
        let base = named_volume_for("sess-abc123", "/workspace/node_modules");
        assert_eq!(
            base,
            named_volume_for("sess-abc123", "/workspace/node_modules")
        );
        assert!(base.starts_with("aoe-vi-sess-abc123-"));
        let long = "/workspace/a-very-long-directory-name-that-exceeds-40-chars-v";
        for (session, path) in [
            ("sess-abc123", "/workspace/.venv"),
            ("sess-bbb", "/workspace/node_modules"),
        ] {
            assert_ne!(base, named_volume_for(session, path), "{session} {path}");
        }
        // Long paths sharing a slug prefix are split by the hash suffix.
        assert_ne!(
            named_volume_for("sess-1", &format!("{long}1")),
            named_volume_for("sess-1", &format!("{long}2"))
        );
        let name = named_volume_for("sess-1", "/workspace/path with spaces/foo:bar");
        assert!(!name.contains(' ') && !name.contains(':'), "{name}");
        // Cleanup matches "aoe-vi-{id}-", so sess1 must not claim sess10's volumes.
        assert!(!named_volume_for("sess10", "/workspace/node_modules").starts_with("aoe-vi-sess1-"));
    }
    #[test]
    fn sandbox_stores_are_physically_isolated_per_instance() {
        let home = Path::new("/tmp/home");
        let declared = Path::new("/tmp/declared");
        let default_a = sandbox_store_dir("gemini", home, None, "instance-a")
            .unwrap()
            .unwrap();
        let default_b = sandbox_store_dir("gemini", home, None, "instance-b")
            .unwrap()
            .unwrap();
        let declared_a = sandbox_store_dir("gemini", home, Some(declared), "instance-a")
            .unwrap()
            .unwrap();
        let declared_b = sandbox_store_dir("gemini", home, Some(declared), "instance-b")
            .unwrap()
            .unwrap();

        assert_ne!(default_a, default_b);
        assert_ne!(declared_a, declared_b);
        assert_eq!(default_a, home.join(".gemini/sandbox-v2/instance-a"));
        assert_eq!(declared_a, declared.join("sandbox-v2/instance-a"));

        assert_eq!(
            sandbox_store_migration_paths("gemini", home, None, "instance-a").unwrap(),
            vec![(
                home.join(".gemini/sandbox"),
                home.join(".gemini/sandbox-v2/instance-a")
            )]
        );
        assert_eq!(
            sandbox_store_migration_paths("gemini", home, Some(declared), "instance-a").unwrap(),
            vec![(
                declared.join("sandbox"),
                declared.join("sandbox-v2/instance-a")
            )]
        );
        assert_eq!(
            sandbox_store_migration_paths("opencode", home, None, "instance-a")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            sandbox_store_migration_paths("opencode", home, Some(declared), "instance-a")
                .unwrap()
                .len(),
            1,
            "a declared root shared by both OpenCode mounts must be copied once"
        );
    }
    #[test]
    fn managed_container_environment_rejects_conflicting_active_roots() {
        let literal = |key: &str, value: &str| EnvEntry::Literal {
            key: key.to_string(),
            value: value.to_string(),
        };
        let claude = crate::agents::get_agent("claude");
        let cursor = crate::agents::get_agent("cursor");
        let prime = crate::agents::get_agent("prime-agent");

        for (environment, agent, rejected_key) in [
            (vec![literal("HOME", "/home/user")], claude, "HOME"),
            (
                vec![
                    literal("HOME", "/root"),
                    literal("CLAUDE_CONFIG_DIR", "/other"),
                ],
                claude,
                "CLAUDE_CONFIG_DIR",
            ),
            (
                vec![
                    literal("HOME", "/root"),
                    literal("CURSOR_CONFIG_DIR", "/other"),
                ],
                cursor,
                "CURSOR_CONFIG_DIR",
            ),
            (
                vec![
                    literal("HOME", "/root"),
                    literal("PRIME_AGENT_CODING_AGENT_DIR", "/other"),
                ],
                prime,
                "PRIME_AGENT_CODING_AGENT_DIR",
            ),
        ] {
            let error =
                validate_managed_container_environment(&environment, agent, "/root").unwrap_err();
            assert!(error.to_string().contains(rejected_key));
            assert!(error
                .to_string()
                .contains("remove this sandbox environment"));
        }

        validate_managed_container_environment(
            &[
                literal("HOME", "/root"),
                literal("CLAUDE_CONFIG_DIR", "/root/.claude"),
            ],
            claude,
            "/root",
        )
        .unwrap();
        validate_managed_container_environment(
            &[
                literal("HOME", "/root"),
                literal("CURSOR_CONFIG_DIR", "/other"),
            ],
            claude,
            "/root",
        )
        .unwrap();
        validate_managed_container_environment(
            &[
                literal("HOME", "/root"),
                literal("PRIME_AGENT_CODING_AGENT_DIR", "/root/.prime/agent"),
            ],
            prime,
            "/root",
        )
        .unwrap();
    }
    #[test]
    #[serial_test::serial]
    fn sandbox_empty_desired_hooks_remove_stale_aoe_entries() {
        let (_hook_guard, _, _tmp_base) = BaseGuard::ready();
        let temp_home = TempDir::new().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let profile = "sandbox-empty-hook-cleanup";
        let profile_dir = crate::session::get_profile_dir(profile).unwrap();
        fs::write(
            profile_dir.join("config.toml"),
            "[session]
    agent_status_hooks = false
    ",
        )
        .unwrap();

        let instance_id = "gemini-empty-hook-cleanup";
        let settings_path = temp_home
            .path()
            .join(".gemini")
            .join(SANDBOX_PRIVATE_SUBDIR)
            .join(instance_id)
            .join("settings.json");
        let events = crate::agents::resolved_hook_events(
            crate::agents::get_agent("gemini").unwrap(),
            &crate::session::config::Config::default(),
        )
        .unwrap();
        crate::hooks::install_hooks(
            &settings_path,
            &events,
            crate::hooks::HookInstallTarget::Sandbox,
        )
        .unwrap();
        let mut settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        settings["hooks"]["ForeignEvent"] = serde_json::json!([{
            "hooks": [{"type": "command", "command": "printf foreign"}]
        }]);
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();
        certify_fixture_content(settings_path.parent().unwrap(), ".gemini").unwrap();
        let project_dir = TempDir::new().unwrap();
        git2::Repository::init(project_dir.path()).unwrap();
        let sandbox_info = crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "test-container".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        build_container_config(
            project_dir.path().to_str().unwrap(),
            &sandbox_info,
            ContainerAgentSelection::new("gemini", None),
            false,
            instance_id,
            None,
            profile,
        )
        .unwrap();

        let content = fs::read_to_string(settings_path).unwrap();
        assert!(content.contains("printf foreign"));
        assert!(!content.contains("aoe-hooks"));
    }
}
