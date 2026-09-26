//! Migration v018: strip legacy AoE-managed hooks from `~/.codex/config.toml`.
//!
//! Codex hooks moved to `.codex/hooks.json`, so the install and uninstall
//! lifecycle no longer reaches entries an older release left in
//! `config.toml`. They keep firing alongside the new ones and keep Codex's
//! dual-source warning up. This walks every reachable `config.toml` (the
//! default plus each `CODEX_HOME` override in the global and per-profile
//! environment lists), marker-gates it, and hands it to
//! [`crate::hooks::uninstall_codex_hooks`], which preserves user-authored
//! hooks and the `[hooks.state]` trust block. It strips regardless of
//! `[features].hooks`: that flag gates execution, not file presence.
//!
//! Per-target failures warn and are skipped; only a missing home directory
//! aborts boot. Host paths only, as in v015 and v017.

use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use crate::hooks::{has_aoe_marker, HookTarget, HookTargetKind};

/// Run v018 against the real user `home` and AoE app directory. Wired into
/// the [`super::MIGRATIONS`] table.
pub fn run() -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let app_dir = crate::session::get_app_dir()?;
    run_in(&home, &app_dir)
}

/// Home-injectable variant of [`run`]: strips AoE hooks from every
/// reachable Codex `config.toml` under `home`, expanding `CODEX_HOME`
/// overrides discovered in `app_dir`'s global and per-profile
/// `environment` lists. Tests synthesise both directories.
pub(crate) fn run_in(home: &Path, app_dir: &Path) -> Result<()> {
    // Default env (no overrides) seeds the scan with `<home>/.codex/config.toml`.
    let mut env_lists: Vec<Vec<String>> = vec![Vec::new()];
    env_lists.extend(collect_env_lists(app_dir));

    debug!(
        target: "migrations.v018",
        home = %home.display(),
        app_dir = %app_dir.display(),
        env_lists = env_lists.len(),
        "v018: scanning Codex config paths"
    );

    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut stripped = 0usize;
    for env in &env_lists {
        let path = crate::hooks::codex_config_path_in(home, env);
        if !seen.insert(path.clone()) {
            continue;
        }
        // Synthetic CodexToml target: the live enumerator does not produce
        // this kind for any registered agent, so v018 builds the marker-gate
        // fixture itself rather than mining `iter_hook_targets_in` for a
        // variant only this migration constructs. `events: Vec::new()` because
        // the only consumer here is `has_aoe_marker` (bytes-only marker
        // scan); `uninstall_codex_hooks` reads the file separately.
        let target = HookTarget {
            agent_name: "codex",
            kind: HookTargetKind::CodexToml,
            path: path.clone(),
            events: Vec::new(),
        };
        if !has_aoe_marker(&target) {
            continue;
        }
        match crate::hooks::uninstall_codex_hooks(&path) {
            Ok(true) => {
                stripped += 1;
                info!(
                    target: "migrations.v018",
                    path = %path.display(),
                    "v018: stripped legacy AoE hooks from Codex config.toml"
                );
            }
            Ok(false) => {
                // Mixed user+AoE matcher groups stay byte-intact (the
                // uninstaller only drops all-AoE groups). The marker is
                // still present afterwards, but on a second run this
                // remains an idempotent no-write outcome.
                debug!(
                    target: "migrations.v018",
                    path = %path.display(),
                    "v018: marker present but no all-AoE group; nothing to remove"
                );
            }
            Err(e) => {
                warn!(
                    target: "migrations.v018",
                    path = %path.display(),
                    error = %e,
                    "v018: skipped (uninstall failed)"
                );
            }
        }
    }

    info!(target: "migrations.v018", count = stripped, "v018: done");
    Ok(())
}

/// Read `environment` arrays from raw TOML (global config + each profile).
/// Mirrors `v015::collect_env_lists`: migrations run before the live
/// process commits to the current `Config` schema, so we deliberately
/// avoid `Config::load()` here. If the `environment` schema key is renamed,
/// both this and `crate::hooks::targets::collect_env_lists_from_session` must update.
fn collect_env_lists(app_dir: &Path) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    if let Some(env) = read_environment_from_toml(&app_dir.join("config.toml")) {
        out.push(env);
    }
    let profiles_dir = app_dir.join("profiles");
    let Ok(entries) = fs::read_dir(&profiles_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if let Some(env) = read_environment_from_toml(&entry.path().join("config.toml")) {
                out.push(env);
            }
        }
    }
    out
}

/// Best-effort `environment = [...]` extractor. Returns `None` on any
/// I/O or parse failure (warn-and-continue convention).
fn read_environment_from_toml(path: &Path) -> Option<Vec<String>> {
    let content = fs::read_to_string(path).ok()?;
    let table: toml::Value = toml::from_str(&content).ok()?;
    let env = table.get("environment")?.as_array()?;
    Some(
        env.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{hook_command, HookInstallTarget};
    use crate::migrations::hook_fixtures::{setup_dirs, unset_agent_home_env};
    use crate::migrations::test_cases::assert_rewrites;
    use std::fs;

    /// Build a TOML fixture with one AoE-marked `SessionStart` hook.
    /// Uses the live `hook_command` so the planted bytes
    /// carry the same `# aoe-hooks ...` trailing sentinel
    /// [`has_aoe_marker`] looks for.
    fn aoe_session_start_block() -> String {
        let cmd = hook_command("running", HookInstallTarget::Host);
        format!(
            "[[hooks.SessionStart]]\n\
             [[hooks.SessionStart.hooks]]\n\
             type = \"command\"\n\
             command = {cmd:?}\n"
        )
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn missing_config_is_noop() {
        let _g = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();

        run_in(&home, &app_dir).unwrap();

        assert!(
            !home.join(".codex/config.toml").exists(),
            "v018 must not magic a missing config.toml into existence"
        );
        assert!(
            !home.join(".codex").exists(),
            "v018 must not create the parent dir either"
        );
    }

    /// Only all-AoE matcher groups go: a hand-merged group with a user hook
    /// stays byte-identical, and `[features].hooks = false` does not stop the
    /// strip (v018 is about file presence, unlike v015).
    #[test]
    #[serial_test::serial(shell_env)]
    fn strips_only_all_aoe_matcher_groups() {
        let _g = unset_agent_home_env();
        let aoe = aoe_session_start_block();
        let cmd = hook_command("running", HookInstallTarget::Host);
        let with_state = format!(
            "[hooks.state]\nexisting = {{ enabled = true, trusted_hash = \"keep-me\" }}\n\n{aoe}"
        );
        let features_off = format!("[features]\nhooks = false\n\n{aoe}");
        let mixed = format!(
            "[[hooks.SessionStart]]\n[[hooks.SessionStart.hooks]]\ntype = \"command\"\n\
             command = \"echo user-hook\"\n[[hooks.SessionStart.hooks]]\ntype = \"command\"\n\
             command = {cmd:?}\n"
        );
        let user_only = "[[hooks.SessionStart]]\n[[hooks.SessionStart.hooks]]\n\
                         type = \"command\"\ncommand = \"echo user-only\"\n";
        let malformed = "[[hooks\n# unclosed";
        assert_rewrites(
            ".codex/config.toml",
            |path| {
                let home = path.parent().unwrap().parent().unwrap();
                let app_dir = home.join("app");
                fs::create_dir_all(&app_dir)?;
                run_in(home, &app_dir)
            },
            &[
                (
                    Some(with_state.as_str()),
                    Some("[hooks.state]\nexisting = { enabled = true, trusted_hash = \"keep-me\" }\n"),
                ),
                (Some(features_off.as_str()), Some("[features]\nhooks = false\n")),
                (Some(aoe.as_str()), Some("")),
                (Some(mixed.as_str()), Some(mixed.as_str())),
                (Some(user_only), Some(user_only)),
                (Some(malformed), Some(malformed)),
            ],
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn profile_codex_home_visited() {
        let _g = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let codex_override = home.join("work-codex");
        fs::create_dir_all(&codex_override).unwrap();
        fs::write(
            codex_override.join("config.toml"),
            aoe_session_start_block(),
        )
        .unwrap();

        let profile_dir = app_dir.join("profiles/work");
        fs::create_dir_all(&profile_dir).unwrap();
        fs::write(
            profile_dir.join("config.toml"),
            format!(
                "environment = [\"CODEX_HOME={}\"]\n",
                codex_override.display()
            ),
        )
        .unwrap();

        run_in(&home, &app_dir).unwrap();

        let text = fs::read_to_string(codex_override.join("config.toml")).unwrap();
        assert!(
            !text.contains("aoe-hooks"),
            "profile-overridden Codex path must be reached and stripped; got: {text}"
        );
        assert!(
            !home.join(".codex/config.toml").exists(),
            "default ~/.codex/config.toml must not be magicked into existence"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn symlinked_config_resolved_and_stripped() {
        let _g = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let real = home.join("real-codex.toml");
        fs::write(&real, aoe_session_start_block()).unwrap();
        let link = home.join(".codex/config.toml");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        run_in(&home, &app_dir).unwrap();

        // The underlying file received the strip.
        let after_real = fs::read_to_string(&real).unwrap();
        assert!(
            !after_real.contains("aoe-hooks"),
            "real config file (target of symlink) must be stripped: {after_real}"
        );
        // The symlink itself was not replaced by a regular file.
        let link_meta = fs::symlink_metadata(&link).unwrap();
        assert!(
            link_meta.file_type().is_symlink(),
            "symlink at .codex/config.toml must survive the rewrite"
        );
    }

    #[test]
    fn codex_agent_declares_codex_json_hook_format() {
        // Regression lock for the Codex hooks.json pivot: the codex agent must declare
        // `HookFormat::CodexJson`. The `CodexToml` variant of `HookFormat`
        // is absent from the codebase, so this test pins the chosen value
        // by exact match; if a future refactor reintroduces a TOML-based
        // codex `HookFormat`, v018 stops being sufficient (the live install
        // path would resurrect what was stripped) and this assertion fires.
        let codex = crate::agents::get_agent("codex").expect("codex agent must be registered");
        let hook_cfg = codex
            .hook_config
            .as_ref()
            .expect("codex must declare a hook_config");
        assert_eq!(
            hook_cfg.format,
            crate::agents::HookFormat::CodexJson,
            "codex agent must use CodexJson; v018 is only safe when no agent \
             installs AoE hooks back into config.toml"
        );
        assert_eq!(
            hook_cfg.settings_rel_path, ".codex/hooks.json",
            "codex hook path must be .codex/hooks.json"
        );
    }
}
