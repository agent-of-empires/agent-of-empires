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

    #[test]
    #[serial_test::serial(shell_env)]
    fn user_only_config_byte_identical() {
        let _g = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let codex = home.join(".codex/config.toml");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        let original = "[[hooks.SessionStart]]\n\
                        [[hooks.SessionStart.hooks]]\n\
                        type = \"command\"\n\
                        command = \"echo user-only\"\n";
        fs::write(&codex, original).unwrap();
        let before = fs::read(&codex).unwrap();

        run_in(&home, &app_dir).unwrap();

        assert_eq!(
            fs::read(&codex).unwrap(),
            before,
            "config.toml without an AoE marker must be byte-untouched"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn aoe_only_strips_and_preserves_state() {
        let _g = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let codex = home.join(".codex/config.toml");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        let content = format!(
            "[hooks.state]\n\
             existing = {{ enabled = true, trusted_hash = \"keep-me\" }}\n\
             \n\
             {}",
            aoe_session_start_block()
        );
        fs::write(&codex, content).unwrap();

        run_in(&home, &app_dir).unwrap();

        let text = fs::read_to_string(&codex).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["hooks"]["state"]["existing"]["trusted_hash"].as_str(),
            Some("keep-me"),
            "[hooks.state] must survive the strip"
        );
        assert!(
            parsed["hooks"].get("SessionStart").is_none(),
            "every AoE-marked SessionStart entry must be removed: {text}"
        );
        assert!(
            !text.contains("aoe-hooks"),
            "no AoE-marker substring may remain anywhere in the file: {text}"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn mixed_user_aoe_matcher_left_intact() {
        // The uninstaller drops only all-AoE matcher groups (per
        // `remove_codex_aoe_hooks` / `codex_matcher_group_is_all_aoe`).
        // A hand-merged group with both a user hook and an AoE hook is
        // preserved byte-for-byte. v018 still treats this as success
        // (returns Ok); the only observable side effect is the debug
        // "marker present but no all-AoE group" log.
        let _g = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let codex = home.join(".codex/config.toml");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        let cmd = hook_command("running", HookInstallTarget::Host);
        let original = format!(
            "[[hooks.SessionStart]]\n\
             [[hooks.SessionStart.hooks]]\n\
             type = \"command\"\n\
             command = \"echo user-hook\"\n\
             [[hooks.SessionStart.hooks]]\n\
             type = \"command\"\n\
             command = {cmd:?}\n"
        );
        fs::write(&codex, &original).unwrap();
        let before = fs::read(&codex).unwrap();

        run_in(&home, &app_dir).unwrap();

        assert_eq!(
            fs::read(&codex).unwrap(),
            before,
            "mixed user+AoE matcher groups must stay byte-identical \
             (locks the conservative uninstaller behaviour)"
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
    #[serial_test::serial(shell_env)]
    fn idempotent_byte_identical_on_second_run() {
        let _g = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let codex = home.join(".codex/config.toml");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        fs::write(&codex, aoe_session_start_block()).unwrap();

        run_in(&home, &app_dir).unwrap();
        let after_first = fs::read(&codex).unwrap();

        // First run must actually have stripped the marker; otherwise the
        // idempotency assertion below would be vacuous.
        assert!(
            !String::from_utf8_lossy(&after_first).contains("aoe-hooks"),
            "first run must strip the AoE marker"
        );

        run_in(&home, &app_dir).unwrap();
        let after_second = fs::read(&codex).unwrap();

        assert_eq!(
            after_first, after_second,
            "v018 must be byte-idempotent: second run finds no marker and skips"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn malformed_toml_skipped_silently() {
        let _g = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let codex = home.join(".codex/config.toml");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        let original = "[[hooks\n# unclosed";
        fs::write(&codex, original).unwrap();

        run_in(&home, &app_dir).unwrap();

        assert_eq!(
            fs::read_to_string(&codex).unwrap(),
            original,
            "malformed TOML must stay byte-identical (gate fails closed)"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn features_hooks_false_still_stripped() {
        // Intentional divergence from v015: v018 strips AoE entries even
        // when `[features].hooks = false`. The feature flag controls
        // execution; v018 is about file presence (the dual-source
        // warning). See module-level "Divergence from v015".
        let _g = unset_agent_home_env();
        let (_tmp, home, app_dir) = setup_dirs();
        let codex = home.join(".codex/config.toml");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        let content = format!("[features]\nhooks = false\n\n{}", aoe_session_start_block());
        fs::write(&codex, content).unwrap();

        run_in(&home, &app_dir).unwrap();

        let text = fs::read_to_string(&codex).unwrap();
        assert!(
            !text.contains("aoe-hooks"),
            "v018 must strip AoE entries regardless of features.hooks: {text}"
        );
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["features"]["hooks"].as_bool(),
            Some(false),
            "[features] table itself must be preserved"
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
