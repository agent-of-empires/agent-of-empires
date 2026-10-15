//! Pre-trusting project folders so agents skip their folder-trust prompt.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::codex::{read_codex_config, with_codex_config_lock, write_codex_config};
use super::config_io::{object_at, parse_json_or_empty, with_config_lock_policy, SymlinkPolicy};
use super::resolve_config_dir_override;

/// Merge-edits a JSON config under its lock, writing only when `edit` changed it.
fn edit_json_config(
    path: &Path,
    policy: SymlinkPolicy,
    lock_path: &Path,
    edit: impl FnOnce(&mut serde_json::Map<String, Value>),
    root_error: &str,
) -> Result<bool> {
    with_config_lock_policy(lock_path, "json.lock", policy, || {
        let mut config = parse_json_or_empty(policy.read(path)?, path);
        let before = config.clone();
        edit(
            config
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("{root_error}"))?,
        );
        if config == before {
            return Ok(false);
        }
        policy.write(path, serde_json::to_string_pretty(&config)?.as_bytes())?;
        Ok(true)
    })
}

/// Merge `security.folderTrust.enabled = false` into a sandbox-staged Gemini
/// `settings.json`, whose container would otherwise re-prompt on every launch (#472).
pub fn disable_gemini_folder_trust(settings_path: &Path) -> Result<()> {
    // Staged for one container, so always inside a bind the agent can write.
    let written = edit_json_config(
        settings_path,
        SymlinkPolicy::Never,
        settings_path,
        |root| {
            object_at(object_at(root, "security"), "folderTrust")
                .insert("enabled".to_string(), Value::Bool(false));
        },
        "Settings file root is not a JSON object",
    )?;
    if written {
        tracing::info!(target: "hooks.install",
            "Disabled Gemini folder trust in {}", settings_path.display());
    }
    Ok(())
}

/// Merge `[projects."<project_path>"].trust_level = "trusted"` into Codex's
/// `config.toml`. `project_path` is the cwd Codex sees (inside a sandbox, the
/// container path).
pub fn trust_codex_project(
    config_path: &Path,
    project_path: &str,
    policy: SymlinkPolicy,
) -> Result<()> {
    with_codex_config_lock(config_path, policy, || {
        let mut config = read_codex_config(config_path, policy)?;
        let before = config.to_string();

        let projects = config.as_table_mut().entry("projects").or_insert_with(|| {
            let mut projects = toml_edit::Table::new();
            // Implicit so only the `[projects."<path>"]` sub-table renders.
            projects.set_implicit(true);
            toml_edit::Item::Table(projects)
        });
        let projects = projects
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("Codex projects key is not a TOML table"))?;
        if !projects
            .get(project_path)
            .is_some_and(toml_edit::Item::is_table)
        {
            projects.insert(
                project_path,
                toml_edit::Item::Table(toml_edit::Table::new()),
            );
        }
        projects
            .get_mut(project_path)
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or_else(|| {
                anyhow::anyhow!("Codex projects.{project_path} entry is not a TOML table")
            })?
            .insert("trust_level", toml_edit::value("trusted"));

        if config.to_string() == before {
            return Ok(());
        }
        write_codex_config(config_path, &config, policy)?;
        tracing::info!(target: "hooks.install",
            "Marked {} trusted in Codex config {}", project_path, config_path.display());
        Ok(())
    })
}

/// Merge `projects."<project_path>".hasTrustDialogAccepted = true` into Claude
/// Code's `.claude.json`, which also holds its onboarding state. A persisted
/// entry, unlike `CLAUDE_CODE_SANDBOXED`, lets the repo's own permission rules load.
pub fn trust_claude_project(
    config_path: &Path,
    project_path: &str,
    policy: SymlinkPolicy,
) -> Result<()> {
    let written = edit_json_config(
        config_path,
        policy,
        &policy.lock_path(config_path)?,
        |root| {
            object_at(object_at(root, "projects"), project_path)
                .insert("hasTrustDialogAccepted".to_string(), Value::Bool(true));
        },
        "Claude config root is not a JSON object",
    )?;
    if written {
        tracing::info!(target: "hooks.install",
            "Marked {} trusted in Claude config {}", project_path, config_path.display());
    }
    Ok(())
}

/// Merge `"<project_path>": "TRUST_FOLDER"` into Gemini's `trustedFolders.json`:
/// the per-path counterpart that the host uses instead of disabling trust globally.
pub fn trust_gemini_project(
    trusted_folders_path: &Path,
    project_path: &str,
    policy: SymlinkPolicy,
) -> Result<()> {
    let written = edit_json_config(
        trusted_folders_path,
        policy,
        &policy.lock_path(trusted_folders_path)?,
        |root| {
            root.insert(
                project_path.to_string(),
                Value::String("TRUST_FOLDER".to_string()),
            );
        },
        "Gemini trustedFolders root is not a JSON object",
    )?;
    if written {
        tracing::info!(target: "hooks.install",
            "Marked {} trusted in Gemini trustedFolders {}",
            project_path, trusted_folders_path.display());
    }
    Ok(())
}

/// Pre-trust `project_path` in the agent's real host config.
///
/// `config_dir` (the session's `agent_config_dir`) wins over the agent's
/// config-dir env var: a wrapper that exports the variable itself does so after
/// AoE picked a file. Agents without a folder-trust prompt are a no-op.
pub fn trust_host_project(
    agent_name: &str,
    home: &Path,
    host_env: &[String],
    config_dir: Option<&Path>,
    project_path: &str,
) -> Result<()> {
    let declared = || config_dir.map(Path::to_path_buf);
    let from_env = |var: &str| resolve_config_dir_override(var, host_env).map(PathBuf::from);
    match agent_name {
        // Without an override the record lives in `~/.claude.json`, beside `~/.claude/`.
        "claude" => trust_claude_project(
            &declared()
                .or_else(|| from_env("CLAUDE_CONFIG_DIR"))
                .map(|dir| dir.join(".claude.json"))
                .unwrap_or_else(|| home.join(".claude.json")),
            project_path,
            SymlinkPolicy::Follow,
        ),
        "codex" => trust_codex_project(
            &declared()
                .or_else(|| from_env("CODEX_HOME"))
                .unwrap_or_else(|| home.join(".codex"))
                .join("config.toml"),
            project_path,
            SymlinkPolicy::Follow,
        ),
        // Gemini's env var names the file rather than its directory.
        "gemini" => trust_gemini_project(
            &config_dir
                .map(|dir| dir.join("trustedFolders.json"))
                .or_else(|| from_env("GEMINI_CLI_TRUSTED_FOLDERS_PATH"))
                .unwrap_or_else(|| home.join(".gemini").join("trustedFolders.json")),
            project_path,
            SymlinkPolicy::Follow,
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{assert_not_rewritten, read_json};
    use crate::session::test_support::EnvGuard;
    use tempfile::TempDir;

    #[test]
    fn disable_gemini_folder_trust_merges_nested_flag() {
        let tmp = TempDir::new().unwrap();
        let fresh = tmp.path().join(".gemini/settings.json");
        disable_gemini_folder_trust(&fresh).unwrap();
        assert_eq!(
            read_json(&fresh)["security"]["folderTrust"]["enabled"],
            false
        );

        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"theme":"dark","security":{"auth":{"selectedType":"oauth"}}}"#,
        )
        .unwrap();
        disable_gemini_folder_trust(&path).unwrap();
        let settings = read_json(&path);
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["security"]["auth"]["selectedType"], "oauth");
        assert_eq!(settings["security"]["folderTrust"]["enabled"], false);
        assert_not_rewritten(&path, || disable_gemini_folder_trust(&path).unwrap());
    }

    #[test]
    fn trust_codex_project_merges_project_table() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "model = \"gpt-5.3-codex\"\n\n[projects.\"/other/path\"]\ntrust_level = \"trusted\"\n",
        )
        .unwrap();
        let trust = || trust_codex_project(&path, "/workspace/wt", SymlinkPolicy::Follow).unwrap();

        trust();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#"[projects."/workspace/wt"]"#), "{text}");
        let config: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(config["model"].as_str(), Some("gpt-5.3-codex"));
        assert_eq!(
            config["projects"]["/other/path"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(
            config["projects"]["/workspace/wt"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_not_rewritten(&path, trust);
    }

    #[test]
    fn trust_claude_project_merges_into_existing_config() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".claude.json");
        let trust = || trust_claude_project(&path, "/workspace/wt", SymlinkPolicy::Follow).unwrap();

        // A half-written config must not block the launch.
        std::fs::write(&path, "{not json").unwrap();
        trust();
        assert_eq!(
            read_json(&path)["projects"]["/workspace/wt"]["hasTrustDialogAccepted"],
            true
        );

        std::fs::write(
            &path,
            r#"{"hasCompletedOnboarding":true,"projects":{"/other":{"hasTrustDialogAccepted":true,"allowedTools":["Bash"]}}}"#,
        )
        .unwrap();
        trust();
        let config = read_json(&path);
        assert_eq!(config["hasCompletedOnboarding"], true);
        assert_eq!(
            config["projects"]["/workspace/wt"]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(config["projects"]["/other"]["allowedTools"][0], "Bash");
        assert_not_rewritten(&path, trust);
    }

    /// `Follow` keeps a dotfiles link working; `Never` replaces a link planted
    /// in a container-writable bind instead of writing through it.
    #[cfg(unix)]
    #[test]
    fn trust_claude_project_symlink_policies() {
        let tmp = TempDir::new().unwrap();
        let dotfiles = tmp.path().join("dotfiles.json");
        std::fs::write(&dotfiles, "{}").unwrap();
        let host_config = tmp.path().join("host.claude.json");
        std::os::unix::fs::symlink(&dotfiles, &host_config).unwrap();
        trust_claude_project(&host_config, "/workspace/wt", SymlinkPolicy::Follow).unwrap();
        assert!(std::fs::symlink_metadata(&host_config)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            read_json(&dotfiles)["projects"]["/workspace/wt"]["hasTrustDialogAccepted"],
            true
        );

        let outside = tmp.path().join("host-secret");
        let host_json = r#"{"projects":{"/host/secret":{"hasTrustDialogAccepted":true}}}"#;
        std::fs::write(&outside, host_json).unwrap();
        let bind = tmp.path().join("bind");
        std::fs::create_dir(&bind).unwrap();
        let planted = bind.join(".claude.json");
        std::os::unix::fs::symlink(&outside, &planted).unwrap();

        trust_claude_project(&planted, "/workspace/wt", SymlinkPolicy::Never).unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), host_json);
        assert!(!std::fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_symlink());
        let written = read_json(&planted);
        assert_eq!(
            written["projects"]["/workspace/wt"]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(written.as_object().unwrap().len(), 1);
        assert!(
            written["projects"].get("/host/secret").is_none(),
            "link target not merged"
        );

        // A link on the lock sidecar fails the write closed.
        let locked = bind.join("locked.claude.json");
        std::os::unix::fs::symlink(&outside, bind.join("locked.claude.json.lock")).unwrap();
        assert!(trust_claude_project(&locked, "/workspace/wt", SymlinkPolicy::Never).is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), host_json);
    }

    #[test]
    fn trust_gemini_project_writes_per_path_entry() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trustedFolders.json");
        std::fs::write(&path, r#"{"/other":"TRUST_FOLDER"}"#).unwrap();
        trust_gemini_project(&path, "/workspace/wt", SymlinkPolicy::Follow).unwrap();
        let folders = read_json(&path);
        assert_eq!(folders["/other"], "TRUST_FOLDER");
        assert_eq!(folders["/workspace/wt"], "TRUST_FOLDER");
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn trust_host_project_routes_per_agent() {
        let _guard = EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let default_home = TempDir::new().unwrap();
        trust_host_project("claude", default_home.path(), &[], None, "/repo").unwrap();
        let config = read_json(&default_home.path().join(".claude.json"));
        assert_eq!(config["projects"]["/repo"]["hasTrustDialogAccepted"], true);

        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude_dir = home.join("custom-claude");
        let env = [format!("CLAUDE_CONFIG_DIR={}", claude_dir.display())];
        trust_host_project("claude", home, &env, None, "/repo").unwrap();
        trust_host_project("gemini", home, &[], None, "/repo").unwrap();
        trust_host_project("opencode", home, &[], None, "/repo").unwrap();
        assert!(claude_dir.join(".claude.json").exists());
        assert!(home.join(".gemini/trustedFolders.json").exists());
        assert_eq!(
            std::fs::read_dir(home).unwrap().count(),
            2,
            "opencode writes nothing"
        );
    }

    #[test]
    fn trust_host_project_config_dir_wins_over_env_var() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let from_env = home.join("from-env");
        let declared = home.join("declared");

        for (agent, env_var, rel) in [
            ("claude", "CLAUDE_CONFIG_DIR", ".claude.json"),
            ("codex", "CODEX_HOME", "config.toml"),
            (
                "gemini",
                "GEMINI_CLI_TRUSTED_FOLDERS_PATH",
                "trustedFolders.json",
            ),
        ] {
            let env_value = if agent == "gemini" {
                from_env.join(rel)
            } else {
                from_env.clone()
            };
            let env = [format!("{env_var}={}", env_value.display())];
            trust_host_project(agent, home, &env, Some(&declared), "/repo").unwrap();
            assert!(declared.join(rel).exists(), "{agent}");
            assert!(!from_env.join(rel).exists(), "{agent}");
            std::fs::remove_dir_all(&declared).unwrap();
        }
    }
}
