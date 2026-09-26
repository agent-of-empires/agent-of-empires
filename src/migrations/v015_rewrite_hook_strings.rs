//! Migration v015: rewrite installed AoE hook shell strings to the hardened
//! shape from #1803 (quoted `"$AOE_INSTANCE_ID"` plus a POSIX allowlist
//! guard).
//!
//! A per-file marker gate guards reuse of the live `install_*` functions, so
//! the rewrite can neither resurrect hooks the user uninstalled nor drift
//! from the bytes install writes. Gate parse errors and per-target rewrite
//! failures are skipped; only a missing home directory aborts boot, and the
//! schema bumps either way, so v015 runs once. A matcher group holding both
//! a user hook and a legacy AoE hook keeps its legacy bytes and gains a
//! hardened entry, so both fire; the host-side `AOE_INSTANCE_ID` validator
//! bounds that. Host paths only: hooks baked into a sandbox image keep the
//! legacy bytes until the image is rebuilt.

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
            install_hooks(&target.path, &target.events, HookInstallTarget::Host)
        }
        // Defensive: `iter_hook_targets_in` does not emit `CodexToml` for
        // any registered agent (codex declares `CodexJson`). The arm stays
        // for `HookTargetKind` exhaustiveness and as a guard rail should a
        // future agent reintroduce a TOML-format codex.
        HookTargetKind::CodexToml => {
            let preserved = snapshot_codex_hooks_state(&target.path)?;
            install_codex_hooks_with_preserved_state(
                &target.path,
                &target.events,
                preserved,
                HookInstallTarget::Host,
            )
        }
        HookTargetKind::Sidecar(sidecar) => {
            // We deliberately do NOT invoke `sidecar.post_install_host`:
            // Kiro's `set_kiro_default_agent_if_builtin` shells out to
            // `kiro-cli`, which is launcher-state mutation, not file-content
            // reconciliation.
            (sidecar.install)(&target.path, HookInstallTarget::Host, &target.events)
        }
    }
}

/// Read `environment` arrays from raw TOML (global config + each profile).
/// Migrations run before the live process commits to the current `Config`
/// schema, so we deliberately avoid `Config::load()` here. If the
/// `environment` schema key is renamed, both this and
/// `crate::hooks::targets::collect_env_lists_from_session` must update.
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
    use crate::migrations::hook_fixtures::{setup_dirs, unset_agent_home_env, write_json};
    use serde_json::Value;
    use std::fs;

    /// A pre-#1803 unquoted, unguarded `mkdir`/`printf` snippet. Contains the
    /// `aoe-hooks` substring via the path, so `is_aoe_hook_command` flags it.
    const LEGACY_STATUS_CMD: &str = "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
        mkdir -p /tmp/aoe-hooks/$AOE_INSTANCE_ID && \
        printf running > /tmp/aoe-hooks/$AOE_INSTANCE_ID/status'";

    /// Assert every AoE-marked command in a Claude-shape settings file is
    /// byte-equal to the live install path's canonical output for its
    /// `(event, status, session_id_capture)` tuple. Issue #1845 acceptance
    /// criterion #4 (byte-for-byte, not "contains the guard substring").
    fn assert_claude_canonical(claude: &Path) {
        use crate::hooks::{hook_command_session_id, status_command_for_event, HookInstallTarget};
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
                    // An event name may declare several matcher groups with
                    // different statuses (Claude's `Notification` splits
                    // permission/elicitation → waiting from idle_prompt →
                    // idle), so the canonical set is the union over every event
                    // def sharing this name, not just the first.
                    let event_defs: Vec<_> = claude_events
                        .iter()
                        .filter(|e| e.name == event_name)
                        .collect();
                    assert!(!event_defs.is_empty(), "unknown Claude event: {event_name}");
                    let mut canonical_set: Vec<String> = Vec::new();
                    for event_def in event_defs {
                        if event_def.identity_field.is_some() {
                            canonical_set.push(hook_command_session_id(
                                HookInstallTarget::Host,
                                crate::agents::HookIdentityField::SessionId,
                            ));
                        }
                        if let Some(status) = event_def.status {
                            let waiting_tools: Vec<String> = event_def
                                .waiting_tools
                                .iter()
                                .map(|t| t.to_string())
                                .collect();
                            canonical_set.push(status_command_for_event(
                                status,
                                &waiting_tools,
                                HookInstallTarget::Host,
                            ));
                        }
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
        let _g = unset_agent_home_env();
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
    fn claude_idempotent_byte_identical_and_canonical() {
        let _g = unset_agent_home_env();
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
        let _g = unset_agent_home_env();
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
        use crate::hooks::status_command_for_event;
        // Claude's PreToolUse is the tool-gated writer (running by default,
        // waiting for AskUserQuestion), so canonicalize through the same
        // selector the installer uses.
        let canonical_running = status_command_for_event(
            crate::agents::HookStatus::Running,
            &["AskUserQuestion".to_string()],
            crate::hooks::HookInstallTarget::Host,
        );
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
        let _g = unset_agent_home_env();
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
        let _g = unset_agent_home_env();
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
        let _g = unset_agent_home_env();
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
        let _g = unset_agent_home_env();
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

    /// Files the gate must leave alone: no AoE marker, unparseable, or absent
    /// (never created). A hermes allowlist is never written beside a bad config.
    #[test]
    #[serial_test::serial(shell_env)]
    fn gate_leaves_unmarked_unparseable_and_missing_files_alone() {
        let user_only = serde_json::to_string(&serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {"hooks": [{"type": "command", "command": "echo only-user"}]}
                ]
            }
        }))
        .unwrap();
        for (rel, body, sibling) in [
            (".claude/settings.json", Some(user_only.as_str()), None),
            (".claude/settings.json", Some("{not json"), None),
            (".settl/config.toml", Some("[[hooks\n# unclosed"), None),
            (
                ".hermes/config.yaml",
                Some("hooks:\n  pre_tool_call:\n    - command: 'unterminated\n"),
                Some(".hermes/shell-hooks-allowlist.json"),
            ),
            (".claude/settings.json", None, None),
            (".codex/config.toml", None, None),
            (".hermes/config.yaml", None, None),
            (".settl/config.toml", None, None),
        ] {
            let _g = unset_agent_home_env();
            let (_tmp, home, app_dir) = setup_dirs();
            let path = home.join(rel);
            if let Some(body) = body {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, body).unwrap();
            }

            run_in(&home, &app_dir).unwrap();

            assert_eq!(fs::read_to_string(&path).ok().as_deref(), body, "{rel}");
            if let Some(sibling) = sibling {
                assert!(
                    !home.join(sibling).exists(),
                    "{sibling} must not be written"
                );
            }
        }
    }

    /// A profile `environment` override is reached, and the default path it
    /// replaces is not created.
    #[test]
    #[serial_test::serial(shell_env)]
    fn profile_config_dir_overrides_are_rewritten() {
        // `CLAUDE_CONFIG_DIR` replaces the whole `~/.claude` directory, so the
        // override file lands at the basename of `settings_rel_path`.
        for (var, dir, file, event, default) in [
            (
                "CODEX_HOME",
                "work-codex",
                "hooks.json",
                "SessionStart",
                ".codex/hooks.json",
            ),
            (
                "CLAUDE_CONFIG_DIR",
                "work-claude",
                "settings.json",
                "PreToolUse",
                ".claude/settings.json",
            ),
        ] {
            let _g = unset_agent_home_env();
            let (_tmp, home, app_dir) = setup_dirs();
            let override_dir = home.join(dir);
            fs::create_dir_all(&override_dir).unwrap();
            let mut events = serde_json::Map::new();
            events.insert(
                event.to_string(),
                serde_json::json!([{"hooks": [{"type": "command", "command": LEGACY_STATUS_CMD}]}]),
            );
            write_json(
                &override_dir.join(file),
                &serde_json::json!({"hooks": events}),
            );

            let profile_dir = app_dir.join("profiles/work");
            fs::create_dir_all(&profile_dir).unwrap();
            fs::write(
                profile_dir.join("config.toml"),
                format!("environment = [\"{var}={}\"]\n", override_dir.display()),
            )
            .unwrap();

            run_in(&home, &app_dir).unwrap();

            let parsed: Value =
                serde_json::from_str(&fs::read_to_string(override_dir.join(file)).unwrap())
                    .unwrap();
            let cmd = parsed["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .expect("AoE command must be present at the override path");
            assert!(
                cmd.contains("case \"$AOE_INSTANCE_ID\""),
                "{var} override must be reached and rewritten; got: {cmd}"
            );
            assert!(
                !home.join(default).exists(),
                "{default} must not be magicked into existence"
            );
        }
    }
}
