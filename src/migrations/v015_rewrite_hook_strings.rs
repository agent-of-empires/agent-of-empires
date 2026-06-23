//! Migration v015: rewrite previously-installed AoE hook shell strings to
//! the hardened shape introduced by PR #1803 (quoted `"$AOE_INSTANCE_ID"`
//! plus a POSIX allowlist guard). Pre-#1803 installs keep the legacy
//! unhardened bytes forever otherwise; this migration runs once on first
//! launch after upgrade.
//!
//! ## Strategy
//!
//! Per-file marker-presence gate guards a reuse of the existing `install_*`
//! functions. The gate prevents resurrecting hooks the user uninstalled
//! (install creates files when absent). Reuse prevents byte-drift from the
//! live install path.
//!
//! ## Failure policy
//!
//! Per `AGENTS.md > Data Migrations`, a returned `Err` aborts boot. Gate-
//! stage parse errors fail closed silently (a corrupt config would
//! otherwise spam every boot, and we cannot prove the file is ours
//! without parsing). Rewrite-stage failures (locked file, permission
//! denied, broken symlink, transient I/O) are `tracing::warn!`'d and
//! skipped. Only `dirs::home_dir() == None` propagates. Schema bumps
//! after attempting every known path so one corrupt file does not block
//! boot.
//!
//! ## Known limitations
//!
//! ### Env vars set in interactive shell only
//!
//! `CLAUDE_CONFIG_DIR` (or similar) set only in the launch shell and not
//! in any profile's `environment` list is invisible at migration time.
//! Recovery: relaunch under that env, or `aoe uninstall && aoe add --cmd
//! <agent>`.
//!
//! ### Transient per-target failures are not retried
//!
//! Per-target rewrite failures bump the schema regardless, so v015 runs
//! at most once. Recovery: `aoe uninstall && aoe add --cmd <agent>`.
//!
//! ### TOCTOU on the gate path (all formats, including Codex)
//!
//! `has_aoe_marker` is read lock-free for every format. The rewrite is
//! locked only for Codex (`with_codex_config_lock`); JSON / sidecar run
//! unlocked end-to-end. Three race windows:
//!
//! 1. **Gate -> write** (all formats): concurrent `aoe uninstall` between
//!    gate-true and rewrite resurrects just-uninstalled hooks.
//! 2. **Codex snapshot -> install gap**: `snapshot_codex_hooks_state`
//!    drops the lock before `install_codex_hooks_with_preserved_state`
//!    re-acquires it. A locked writer in the gap loses its state.
//! 3. **JSON / sidecar gate-vs-write**: no lock at all. Same as (1).
//!
//! Window is the few hundred ms of v015 execution. Recovery: re-run
//! `aoe uninstall`. Defense-in-depth fix (gate inside the rewrite lock,
//! plus locks for JSON / sidecar) tracked as a follow-up.
//!
//! ### Mixed user+AoE matcher groups
//!
//! A hand-merged matcher group containing both a user hook and a legacy
//! AoE hook is left untouched (`remove_aoe_entries` only drops
//! all-AoE groups), AND v015 appends a fresh AoE-only group with the
//! hardened command. Result: legacy unhardened bytes persist alongside
//! the fresh hardened entry; both fire per event. Defense-in-depth gap
//! bounded by PR #1803's host-side `AOE_INSTANCE_ID` validator in
//! `Instance::start_with_size_opts`. Locked by
//! `mixed_user_aoe_matcher_group_documents_double_firing`. Closing the
//! gap requires in-place string rewrite inside non-AoE matcher groups;
//! tracked as a follow-up.
//!
//! ### Power-loss durability across formats
//!
//! Every install/uninstall path in `hooks/mod.rs` routes through
//! `crate::session::atomic_write_following_symlinks` (resolve symlink
//! chain, then temp file + fsync + rename + dir fsync on the resolved
//! target). A power loss mid-rewrite either keeps the prior bytes intact
//! or surfaces the freshly written bytes; partial writes are not
//! observable. Symlinks at the destination (a common dotfile-manager
//! pattern: `~/.claude/settings.json -> ~/dotfiles/...`) are followed
//! rather than replaced, so the underlying dotfile target receives the
//! rewrite and the symlink survives.
//!
//! ### Sandbox-image hooks are not rewritten
//!
//! v015 walks host-reachable paths only. Hooks installed via
//! `HookInstallTarget::Sandbox` (baked into a Docker / Podman / Apple-
//! Containers image) keep the legacy bytes until the image is rebuilt;
//! the next `aoe sandbox rebuild` (or equivalent) will pick up the
//! current canonical bytes. Defense-in-depth bound: container isolation
//! already gates the `AOE_INSTANCE_ID` injection surface PR #1803 closed.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::hooks::{
    has_aoe_marker, install_codex_hooks_with_preserved_state, install_hooks, iter_hook_targets_in,
    snapshot_codex_hooks_state, HookInstallTarget, HookTarget, HookTargetKind,
};

pub fn run() -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let app_dir = crate::session::get_app_dir()?;
    run_in(&home, &app_dir)
}

pub(crate) fn run_in(home: &Path, app_dir: &Path) -> Result<()> {
    let env_lists = collect_env_lists(app_dir);
    debug!(
        target: "migrations.v015",
        home = %home.display(),
        app_dir = %app_dir.display(),
        env_lists = env_lists.len(),
        "v015: scanning hook targets"
    );

    let mut rewritten = 0usize;
    for target in iter_hook_targets_in(home, &env_lists) {
        if !has_aoe_marker(&target) {
            continue;
        }
        match rewrite_one(&target) {
            Ok(()) => {
                rewritten += 1;
                info!(
                    target: "migrations.v015",
                    agent = target.agent_name,
                    path = %target.path.display(),
                    "v015: rewrote AoE hook entries to current canonical form"
                );
            }
            Err(e) => {
                warn!(
                    target: "migrations.v015",
                    agent = target.agent_name,
                    path = %target.path.display(),
                    error = %e,
                    "v015: skipped (rewrite failed)"
                );
            }
        }
    }

    info!(target: "migrations.v015", count = rewritten, "v015: done");
    Ok(())
}

/// Reuses the live `install_*` functions to rewrite all canonical AoE entries.
/// As a consequence, files that had hooks for only a *subset* of the agent's
/// declared events end up with the full canonical set after this runs (per
/// plan §3.3, the install path is the source of truth for "what AoE hooks
/// look like today"). We do not preserve a historical narrower-set state
/// because there is no reliable way to distinguish "user removed event X"
/// from "AoE never installed event X."
fn rewrite_one(target: &HookTarget) -> Result<()> {
    match target.kind {
        HookTargetKind::JsonSettings | HookTargetKind::CodexJson => {
            install_hooks(&target.path, target.events, HookInstallTarget::Host)
        }
        // Defensive: `iter_hook_targets_in` does not emit `CodexToml` for
        // any registered agent (codex declares `CodexJson`). The arm stays
        // for `HookTargetKind` exhaustiveness and as a guard rail should a
        // future agent reintroduce a TOML-format codex.
        HookTargetKind::CodexToml => {
            let preserved = snapshot_codex_hooks_state(&target.path)?;
            install_codex_hooks_with_preserved_state(
                &target.path,
                target.events,
                preserved,
                HookInstallTarget::Host,
            )
        }
        HookTargetKind::Sidecar(sidecar) => {
            // We deliberately do NOT invoke `sidecar.post_install_host`:
            // Kiro's `set_kiro_default_agent_if_builtin` shells out to
            // `kiro-cli`, which is launcher-state mutation, not file-content
            // reconciliation.
            (sidecar.install)(&target.path, HookInstallTarget::Host)
        }
    }
}

/// Read `environment` arrays from raw TOML (global config + each profile).
/// Migrations run before the live process commits to the current `Config`
/// schema, so we deliberately avoid `Config::load()` here. If the
/// `environment` schema key is renamed, both this and
/// `crate::hooks::collect_env_lists_from_session` must update.
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
    use serde_json::Value;
    use std::fs;
    use tempfile::TempDir;

    /// A pre-#1803 unquoted, unguarded `mkdir`/`printf` snippet. Contains the
    /// `aoe-hooks` substring via the path, so `is_aoe_hook_command` flags it.
    const LEGACY_STATUS_CMD: &str = "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
        mkdir -p /tmp/aoe-hooks/$AOE_INSTANCE_ID && \
        printf running > /tmp/aoe-hooks/$AOE_INSTANCE_ID/status'";

    /// `EnvGuard` clears CODEX_HOME, CLAUDE_CONFIG_DIR, etc. for the test
    /// duration so the migration's path resolution sees only the explicit
    /// fixtures in `home` / `app_dir`.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }
    impl EnvGuard {
        fn unset_all() -> Self {
            let keys = [
                "CODEX_HOME",
                "CLAUDE_CONFIG_DIR",
                "CURSOR_CONFIG_DIR",
                "GEMINI_CONFIG_DIR",
                "QWEN_CONFIG_DIR",
            ];
            let saved = keys
                .iter()
                .map(|k| {
                    let prev = std::env::var(k).ok();
                    std::env::remove_var(k);
                    (*k, prev)
                })
                .collect();
            Self { saved }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn setup_dirs() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let app_dir = tmp.path().join("app");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&app_dir).unwrap();
        (tmp, home, app_dir)
    }

    fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    /// Assert every AoE-marked command in a Claude-shape settings file is
    /// byte-equal to the live install path's canonical output for its
    /// `(event, status, session_id_capture)` tuple. Issue #1845 acceptance
    /// criterion #4 (byte-for-byte, not "contains the guard substring").
    fn assert_claude_canonical(claude: &Path) {
        use crate::hooks::{
            canonical_session_id_command, canonical_status_command, HookInstallTarget,
        };
        let parsed: Value = serde_json::from_str(&fs::read_to_string(claude).unwrap()).unwrap();
        let hooks = parsed["hooks"].as_object().expect("hooks present");
        // An empty `hooks: {}` would silently pass the per-event loop below.
        // Catches a regression where v015 strips every AoE entry to nothing.
        assert!(
            !hooks.is_empty(),
            "v015 wrote an empty hooks object; canonical check would be vacuous on {}",
            claude.display(),
        );
        let claude_events = crate::agents::AGENTS
            .iter()
            .find(|a| a.name == "claude")
            .and_then(|a| a.hook_config.as_ref())
            .map(|hc| hc.events)
            .expect("Claude must declare hook_config");
        for (event_name, matchers) in hooks {
            let Some(arr) = matchers.as_array() else {
                continue;
            };
            for matcher in arr {
                let Some(hooks_arr) = matcher["hooks"].as_array() else {
                    continue;
                };
                for hook in hooks_arr {
                    let cmd = hook["command"].as_str().unwrap_or_default();
                    if !cmd.contains("aoe-hooks") {
                        continue;
                    }
                    let event_def = claude_events
                        .iter()
                        .find(|e| e.name == event_name)
                        .unwrap_or_else(|| panic!("unknown Claude event: {event_name}"));
                    let mut canonical_set: Vec<String> = Vec::new();
                    if event_def.session_id_capture {
                        canonical_set.push(canonical_session_id_command(HookInstallTarget::Host));
                    }
                    if let Some(status) = event_def.status {
                        canonical_set
                            .push(canonical_status_command(status, HookInstallTarget::Host));
                    }
                    assert!(
                        canonical_set.iter().any(|c| c == cmd),
                        "non-canonical AoE command on {event_name}: \
                         got {cmd:?}, expected one of {canonical_set:?}",
                    );
                }
            }
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn claude_legacy_settings_rewritten_user_preserved() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude/settings.json");
        write_json(
            &claude,
            &serde_json::json!({
                "hooks": {
                    "PreToolUse": [
                        {
                            "matcher": "Bash",
                            "hooks": [{"type": "command", "command": "echo user-hook"}]
                        },
                        {
                            "hooks": [{"type": "command", "command": LEGACY_STATUS_CMD}]
                        }
                    ]
                }
            }),
        );

        run_in(&home, &app_dir).unwrap();

        let content: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        let pre_tool = content["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(
            pre_tool.len() >= 2,
            "v015 must keep the user matcher group AND append the AoE group; got {} block(s)",
            pre_tool.len(),
        );
        assert_eq!(pre_tool[0]["matcher"], "Bash");
        assert_eq!(pre_tool[0]["hooks"][0]["command"], "echo user-hook");
        // Byte-for-byte canonical check (issue #1845 acceptance #4); catches
        // wrong-status / wrong-shape regressions a substring would miss.
        assert_claude_canonical(&claude);
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn claude_no_marker_untouched() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude/settings.json");
        write_json(
            &claude,
            &serde_json::json!({
                "hooks": {
                    "PreToolUse": [
                        {"hooks": [{"type": "command", "command": "echo only-user"}]}
                    ]
                }
            }),
        );
        let before = fs::read(&claude).unwrap();

        run_in(&home, &app_dir).unwrap();

        assert_eq!(
            fs::read(&claude).unwrap(),
            before,
            "files without an AoE marker must be byte-untouched"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn claude_idempotent_byte_identical_and_canonical() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude/settings.json");
        write_json(
            &claude,
            &serde_json::json!({
                "hooks": { "PreToolUse": [
                    {"hooks": [{"type": "command", "command": LEGACY_STATUS_CMD}]}
                ]}
            }),
        );

        run_in(&home, &app_dir).unwrap();
        let after_first = fs::read(&claude).unwrap();

        // Catches the `{}` regression: without this assertion, run-2 would
        // see no marker, skip, and the file would be byte-equal to a totally
        // broken run-1 output.
        assert_claude_canonical(&claude);

        run_in(&home, &app_dir).unwrap();
        let after_second = fs::read(&claude).unwrap();

        // Byte-equality relies on serde_json's BTreeMap-ordered serialization;
        // switch to parsed-`Value` equality if `preserve_order` ever defaults.
        assert_eq!(after_first, after_second, "v015 must be byte-idempotent");

        // Catches "idempotent on the wrong fixed point" (run-2 produces
        // non-canonical bytes that happen to byte-equal run-1's).
        assert_claude_canonical(&claude);
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn mixed_user_aoe_matcher_group_documents_double_firing() {
        // Documented limitation: a hand-merged matcher group with both a
        // user hook and a legacy AoE hook stays intact (`remove_aoe_entries`
        // drops only all-AoE groups), AND v015 appends a fresh AoE-only
        // group. Both fire per event; the legacy command stays unhardened.
        // Defense-in-depth gap bounded by PR #1803's host-side
        // `AOE_INSTANCE_ID` validator.
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude/settings.json");
        write_json(
            &claude,
            &serde_json::json!({
                "hooks": { "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "echo user"},
                        {"type": "command", "command": LEGACY_STATUS_CMD}
                    ]
                }]}
            }),
        );

        run_in(&home, &app_dir).unwrap();

        let after: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        let pre_tool = after["hooks"]["PreToolUse"].as_array().unwrap();

        // (1) Two matcher groups: the original mixed group at index 0 PLUS
        // a fresh canonical AoE-only group at index >= 1. This explicitly
        // locks the double-firing behaviour.
        assert!(
            pre_tool.len() >= 2,
            "v015 must APPEND a fresh canonical AoE matcher block alongside \
             the legacy mixed group; got {} matcher block(s)",
            pre_tool.len(),
        );

        // (2) The mixed group at index 0 is byte-identical to its input shape.
        let mixed = &pre_tool[0];
        assert_eq!(mixed["matcher"], "Bash");
        let inner = mixed["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 2);
        assert_eq!(inner[0]["command"], "echo user");
        assert_eq!(
            inner[1]["command"], LEGACY_STATUS_CMD,
            "legacy AoE bytes inside the mixed group are NOT rewritten",
        );

        // (3) Some subsequent matcher group (index >= 1) carries a fresh
        // canonical hardened AoE entry. This locks the dual invariant: the
        // legacy entry is preserved AND v015 still installs the current
        // canonical bytes adjacent to it (so live status detection works
        // for the next session).
        use crate::hooks::canonical_status_command;
        let canonical_running =
            canonical_status_command("running", crate::hooks::HookInstallTarget::Host);
        let found_hardened = pre_tool.iter().skip(1).any(|m| {
            m["hooks"].as_array().is_some_and(|arr| {
                arr.iter()
                    .any(|h| h["command"].as_str() == Some(canonical_running.as_str()))
            })
        });
        assert!(
            found_hardened,
            "v015 must append a fresh canonical AoE matcher block alongside \
             the legacy mixed group: {pre_tool:#?}",
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn settl_marker_only_rewrites_aoe_lines() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let settl = home.join(".settl/config.toml");
        fs::create_dir_all(settl.parent().unwrap()).unwrap();
        fs::write(
            &settl,
            format!(
                "[[hooks]]\n\
                 event = \"GameWon\"\n\
                 command = \"echo user-only\"\n\
                 \n\
                 [[hooks]]\n\
                 event = \"TurnStarted\"\n\
                 command = {LEGACY_STATUS_CMD:?}\n"
            ),
        )
        .unwrap();

        run_in(&home, &app_dir).unwrap();

        let parsed: toml::Value = toml::from_str(&fs::read_to_string(&settl).unwrap()).unwrap();
        let hooks = parsed["hooks"].as_array().unwrap();
        let user = hooks
            .iter()
            .find(|h| h["command"].as_str() == Some("echo user-only"))
            .expect("user hook must survive");
        assert_eq!(user["event"].as_str(), Some("GameWon"));
        let aoe: Vec<_> = hooks
            .iter()
            .filter_map(|h| h["command"].as_str())
            .filter(|c| c.contains("aoe-hooks"))
            .collect();
        assert!(
            aoe.iter().all(|c| c.contains("case \"$AOE_INSTANCE_ID\"")),
            "every AoE line must carry the hardened guard"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn hermes_config_and_allowlist_both_rewritten() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let cfg = home.join(".hermes/config.yaml");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(
            &cfg,
            format!("hooks:\n  pre_tool_call:\n    - command: {LEGACY_STATUS_CMD:?}\n"),
        )
        .unwrap();

        run_in(&home, &app_dir).unwrap();

        let yaml = fs::read_to_string(&cfg).unwrap();
        assert!(
            yaml.contains("case \"$AOE_INSTANCE_ID\""),
            "Hermes YAML must be rewritten to hardened form"
        );
        let allow = home.join(".hermes/shell-hooks-allowlist.json");
        assert!(allow.exists(), "allowlist must be created alongside config");
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&allow).unwrap()).unwrap();
        let approvals = parsed["approvals"].as_array().unwrap();
        for approval in approvals {
            let cmd = approval["command"].as_str().unwrap();
            assert!(
                cmd.contains("case \"$AOE_INSTANCE_ID\""),
                "allowlist must key on the new hardened command"
            );
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn hermes_allowlist_approved_at_preserved_on_idempotency() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let cfg = home.join(".hermes/config.yaml");
        let allow_path = home.join(".hermes/shell-hooks-allowlist.json");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(
            &cfg,
            format!("hooks:\n  pre_tool_call:\n    - command: {LEGACY_STATUS_CMD:?}\n"),
        )
        .unwrap();

        // First run rewrites legacy YAML to hardened form and creates the
        // canonical allowlist with current `approved_at` values.
        run_in(&home, &app_dir).unwrap();

        // Plant a sentinel timestamp on every approval. Because the entries
        // now carry the HARDENED command (the same bytes a re-run of
        // render_hermes_allowlist will match on), a subsequent rewrite must
        // hit the (event, command) collision branch and preserve approved_at.
        // Without this trick (e.g. running run_in twice back-to-back without
        // the sentinel injection), the test would pass even if preservation
        // were reverted to per-call `Utc::now()`, because to_rfc3339_opts
        // collapses sub-second timestamps to the same string within one
        // wall-clock second.
        const SENTINEL: &str = "2020-01-01T00:00:00Z";
        let mut data: Value =
            serde_json::from_str(&fs::read_to_string(&allow_path).unwrap()).unwrap();
        for approval in data["approvals"].as_array_mut().unwrap() {
            approval["approved_at"] = Value::String(SENTINEL.into());
            // Re-render canary: render_hermes_allowlist's retain+push path
            // re-emits only its 4 canonical fields, so this stripped key
            // distinguishes "re-render ran" from "re-render skipped". A
            // skipped re-render would leave the planted canary intact and
            // let the sentinel assertion below pass without proving anything.
            approval["__reentry_canary"] = Value::Bool(true);
        }
        fs::write(&allow_path, serde_json::to_string_pretty(&data).unwrap()).unwrap();

        // Re-plant the legacy YAML so the marker gate fires again and v015
        // genuinely re-enters the rewrite path.
        fs::write(
            &cfg,
            format!("hooks:\n  pre_tool_call:\n    - command: {LEGACY_STATUS_CMD:?}\n"),
        )
        .unwrap();

        run_in(&home, &app_dir).unwrap();

        let after: Value = serde_json::from_str(&fs::read_to_string(&allow_path).unwrap()).unwrap();
        let approvals = after["approvals"].as_array().unwrap();
        assert!(!approvals.is_empty(), "allowlist must not be wiped");
        for approval in approvals {
            assert!(
                approval.get("__reentry_canary").is_none(),
                "v015 must re-render the allowlist on the second run; canary survived: {approval}"
            );
            assert_eq!(
                approval["approved_at"].as_str(),
                Some(SENTINEL),
                "approved_at must be preserved on (event, hardened_command) collision: {approval}"
            );
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn kiro_rewrite_preserves_extra_keys() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let kiro = home.join(".kiro/agents/aoe-hooks.json");
        write_json(
            &kiro,
            &serde_json::json!({
                "name": "my-custom-agent",
                "tools": ["Read", "Bash"],
                "description": "user description that must survive",
                "model": "claude-3-5-sonnet",
                "custom_user_field": {"nested": [1, 2, 3]},
                "hooks": {
                    "preToolUse": [{"command": LEGACY_STATUS_CMD}]
                }
            }),
        );

        run_in(&home, &app_dir).unwrap();

        let parsed: Value = serde_json::from_str(&fs::read_to_string(&kiro).unwrap()).unwrap();
        assert_eq!(parsed["name"].as_str(), Some("my-custom-agent"));
        assert_eq!(parsed["tools"][0], "Read");
        assert_eq!(
            parsed["description"].as_str(),
            Some("user description that must survive"),
            "arbitrary user-set keys must be preserved"
        );
        assert_eq!(parsed["model"].as_str(), Some("claude-3-5-sonnet"));
        assert_eq!(parsed["custom_user_field"]["nested"][2], 3);
        let cmd = parsed["hooks"]["preToolUse"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            cmd.contains("case \"$AOE_INSTANCE_ID\""),
            "Kiro hook command must be rewritten"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn missing_files_noop() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();

        run_in(&home, &app_dir).unwrap();

        assert!(
            !home.join(".claude/settings.json").exists(),
            "no AoE config existed; migration must NOT create new files"
        );
        assert!(!home.join(".codex/config.toml").exists());
        assert!(!home.join(".hermes/config.yaml").exists());
        assert!(!home.join(".settl/config.toml").exists());
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn malformed_json_gate_fails_closed_silently() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let claude = home.join(".claude/settings.json");
        fs::create_dir_all(claude.parent().unwrap()).unwrap();
        fs::write(&claude, "{not json").unwrap();

        run_in(&home, &app_dir).unwrap();

        assert_eq!(
            fs::read_to_string(&claude).unwrap(),
            "{not json",
            "malformed file must stay byte-identical (gate fails closed)"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn malformed_toml_gate_fails_closed_silently() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let settl = home.join(".settl/config.toml");
        fs::create_dir_all(settl.parent().unwrap()).unwrap();
        fs::write(&settl, "[[hooks\n# unclosed").unwrap();

        run_in(&home, &app_dir).unwrap();

        assert_eq!(
            fs::read_to_string(&settl).unwrap(),
            "[[hooks\n# unclosed",
            "malformed TOML must stay byte-identical (gate fails closed)"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn profile_codex_home_path_is_rewritten() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let codex_override = home.join("work-codex");
        fs::create_dir_all(&codex_override).unwrap();
        write_json(
            &codex_override.join("hooks.json"),
            &serde_json::json!({
                "hooks": {
                    "SessionStart": [
                        {"hooks": [{"type": "command", "command": LEGACY_STATUS_CMD}]}
                    ]
                }
            }),
        );

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

        let parsed: Value =
            serde_json::from_str(&fs::read_to_string(codex_override.join("hooks.json")).unwrap())
                .unwrap();
        let cmd = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("AoE command must be present at the override path");
        assert!(
            cmd.contains("case \"$AOE_INSTANCE_ID\""),
            "profile-overridden Codex path must be reached and rewritten; got: {cmd}"
        );
        assert!(
            !home.join(".codex/hooks.json").exists(),
            "default ~/.codex/hooks.json must not be magicked into existence"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn profile_claude_config_dir_path_is_rewritten() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        // CLAUDE_CONFIG_DIR replaces the whole `~/.claude` dir, so the
        // override file lands at `<override>/settings.json` (basename of
        // `settings_rel_path`), matching `agent_settings_path_in`'s behavior.
        let claude_override = home.join("work-claude");
        fs::create_dir_all(&claude_override).unwrap();
        write_json(
            &claude_override.join("settings.json"),
            &serde_json::json!({
                "hooks": {
                    "PreToolUse": [
                        {"hooks": [{"type": "command", "command": LEGACY_STATUS_CMD}]}
                    ]
                }
            }),
        );

        let profile_dir = app_dir.join("profiles/work");
        fs::create_dir_all(&profile_dir).unwrap();
        fs::write(
            profile_dir.join("config.toml"),
            format!(
                "environment = [\"CLAUDE_CONFIG_DIR={}\"]\n",
                claude_override.display()
            ),
        )
        .unwrap();

        run_in(&home, &app_dir).unwrap();

        let parsed: Value = serde_json::from_str(
            &fs::read_to_string(claude_override.join("settings.json")).unwrap(),
        )
        .unwrap();
        let cmd = parsed["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("AoE command must be present at the override path");
        assert!(
            cmd.contains("case \"$AOE_INSTANCE_ID\""),
            "profile-overridden Claude path must be reached and rewritten; got: {cmd}"
        );
        assert!(
            !home.join(".claude/settings.json").exists(),
            "default ~/.claude/settings.json must not be magicked into existence"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn malformed_yaml_gate_fails_closed_silently() {
        let _g = EnvGuard::unset_all();
        let (_tmp, home, app_dir) = setup_dirs();
        let hermes = home.join(".hermes/config.yaml");
        fs::create_dir_all(hermes.parent().unwrap()).unwrap();
        let original = "hooks:\n  pre_tool_call:\n    - command: 'unterminated\n";
        fs::write(&hermes, original).unwrap();

        run_in(&home, &app_dir).unwrap();

        assert_eq!(
            fs::read_to_string(&hermes).unwrap(),
            original,
            "malformed YAML must stay byte-identical (gate fails closed)"
        );
        assert!(
            !home.join(".hermes/shell-hooks-allowlist.json").exists(),
            "no allowlist may be written when the YAML gate fails closed"
        );
    }
}
