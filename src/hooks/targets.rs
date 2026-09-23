//! Every on-disk location AoE may have written hooks to, shared by uninstall
//! and the hook-rewrite migrations.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::codex::{codex_group_aoe_flags, CODEX_HOOK_EVENT_NAMES};
use super::command::{is_aoe_hook_command, json_command_is_aoe};
use super::{agent_settings_path_in, codex_hooks_json_path_in};
use crate::agents::{HookFormat, SidecarFormat};

#[derive(Clone, Copy, Debug)]
pub(crate) enum HookTargetKind {
    /// `hooks.<event>[].hooks[].command` JSON (Claude, Gemini, Qwen, ...).
    JsonSettings,
    /// Legacy Codex `config.toml`; only the v018 cleanup migration builds it.
    CodexToml,
    /// Codex `hooks.json`, located through `CODEX_HOME`.
    CodexJson,
    /// A format installed through the agent's `SidecarHooks` functions.
    Sidecar(&'static crate::agents::SidecarHooks),
}

#[derive(Debug)]
pub(crate) struct HookTarget {
    pub agent_name: &'static str,
    pub kind: HookTargetKind,
    pub path: PathBuf,
    /// Default events: migrations repair canonical files, not profile status maps.
    pub events: Vec<crate::agents::ResolvedHookEvent>,
}

/// Targets reachable from this process: each agent's home default plus paths
/// overridden by the global or a profile `environment`.
pub(crate) fn iter_hook_targets() -> Vec<HookTarget> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut targets = iter_hook_targets_in(&home, &collect_env_lists_from_session());
    match crate::session::list_profiles() {
        Ok(profiles) => {
            for profile in profiles {
                let config =
                    crate::session::config::profile_config::resolve_config_or_warn(&profile);
                let home = crate::session::environment::resolve_host_environment_value(
                    &config.environment,
                    "HOME",
                )
                .map(PathBuf::from)
                .unwrap_or_else(|| home.clone());
                for tool in config.session.agent_config_dir.keys() {
                    let detect_as = config
                        .session
                        .agent_detect_as
                        .get(tool)
                        .map(String::as_str)
                        .unwrap_or("");
                    let Some(agent) = crate::session::resolved_agent_for(&profile, tool, detect_as)
                    else {
                        continue;
                    };
                    let Some(root) = config.session.agent_config_dir_for(tool, &home) else {
                        continue;
                    };
                    if let Some(hook_cfg) = agent.hook_config.as_ref() {
                        if let Some(file) = Path::new(hook_cfg.settings_rel_path).file_name() {
                            let path = root.join(file);
                            if !targets.iter().any(|target| {
                                matches!(target.kind, HookTargetKind::JsonSettings)
                                    && target.path == path
                            }) {
                                let events = crate::agents::resolved_hook_events(agent, &config)
                                    .unwrap_or_default();
                                targets.push(HookTarget {
                                    agent_name: agent.name,
                                    kind: HookTargetKind::JsonSettings,
                                    path,
                                    events,
                                });
                            }
                        }
                    } else if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
                        let relative: PathBuf = Path::new(sidecar.host_config_subpath)
                            .components()
                            .skip(1)
                            .collect();
                        let path = root.join(relative);
                        if !targets.iter().any(|target| {
                            matches!(target.kind, HookTargetKind::Sidecar(_)) && target.path == path
                        }) {
                            let events =
                                crate::agents::resolved_sidecar_hook_events(agent, &config)
                                    .unwrap_or_default();
                            targets.push(HookTarget {
                                agent_name: agent.name,
                                kind: HookTargetKind::Sidecar(sidecar),
                                path,
                                events,
                            });
                        }
                    }
                }
                let instances = match crate::session::Storage::new_unwatched(&profile)
                    .and_then(|storage| storage.load())
                {
                    Ok(instances) => instances,
                    Err(error) => {
                        tracing::warn!(target: "hooks", %profile, %error, "Failed to read conversation hook targets");
                        continue;
                    }
                };
                for instance in instances {
                    let bindings = [
                        instance
                            .agent_session_binding
                            .as_ref()
                            .and_then(|binding| binding.execution.as_ref()),
                        instance
                            .resume_binding
                            .as_ref()
                            .and_then(|binding| binding.execution.as_ref()),
                        instance
                            .active_execution
                            .as_ref()
                            .map(|active| &active.binding),
                    ];
                    let prior_bindings =
                        instance
                            .prior_tool_session_ids
                            .values()
                            .filter_map(|prior| {
                                prior
                                    .agent_session_binding
                                    .as_ref()
                                    .and_then(|binding| binding.execution.as_ref())
                            });
                    for binding in bindings.into_iter().flatten().chain(prior_bindings) {
                        if binding.agent != "claude" || binding.filesystem != "host" {
                            continue;
                        }
                        for root in &binding.stores {
                            let path = root.join("settings.json");
                            if targets.iter().any(|target| {
                                matches!(target.kind, HookTargetKind::JsonSettings)
                                    && target.path == path
                            }) {
                                continue;
                            }
                            targets.push(HookTarget {
                                agent_name: "claude",
                                kind: HookTargetKind::JsonSettings,
                                path,
                                events: crate::agents::resolved_hook_events(
                                    crate::agents::get_agent("claude")
                                        .expect("built-in Claude agent"),
                                    &crate::session::config::Config::default(),
                                )
                                .unwrap_or_default(),
                            });
                        }
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!(target: "hooks", %error, "Failed to list conversation hook profiles")
        }
    }
    targets
}

pub(crate) fn iter_hook_targets_in(home: &Path, env_lists: &[Vec<String>]) -> Vec<HookTarget> {
    let defaults = crate::session::config::Config::default();
    let mut out = Vec::new();
    for agent in crate::agents::AGENTS {
        if let Some(hook_cfg) = agent.hook_config.as_ref() {
            let resolve = |env: &[String]| match hook_cfg.format {
                HookFormat::JsonSettings => agent_settings_path_in(home, hook_cfg, env),
                HookFormat::CodexJson => codex_hooks_json_path_in(home, env),
            };
            let kind = match hook_cfg.format {
                HookFormat::JsonSettings => HookTargetKind::JsonSettings,
                HookFormat::CodexJson => HookTargetKind::CodexJson,
            };
            let mut paths: Vec<PathBuf> = Vec::new();
            for path in
                std::iter::once(resolve(&[])).chain(env_lists.iter().map(|env| resolve(env)))
            {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
            for path in paths {
                out.push(HookTarget {
                    agent_name: agent.name,
                    kind,
                    path,
                    events: crate::agents::resolved_hook_events(agent, &defaults)
                        .unwrap_or_default(),
                });
            }
        }
        if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
            out.push(HookTarget {
                agent_name: agent.name,
                kind: HookTargetKind::Sidecar(sidecar),
                path: home.join(sidecar.host_config_subpath),
                events: crate::agents::resolved_sidecar_hook_events(agent, &defaults)
                    .unwrap_or_default(),
            });
        }
    }
    out
}

/// The global and per-profile `environment` lists. A missing config is silent;
/// a malformed one warns through `load_or_warn`.
pub(super) fn collect_env_lists_from_session() -> Vec<Vec<String>> {
    let mut out = vec![crate::session::config::Config::load_or_warn().environment];
    match crate::session::list_profiles() {
        Ok(profiles) => out.extend(profiles.iter().map(|p| {
            crate::session::config::profile_config::resolve_config_or_warn(p).environment
        })),
        Err(e) => tracing::warn!(target: "hooks", "Failed to list profiles: {}", e),
    }
    out
}

/// Whether the target holds an AoE hook. Migrations check this first because
/// installers create missing files and must not restore uninstalled hooks.
pub(crate) fn has_aoe_marker(target: &HookTarget) -> bool {
    let Ok(content) = std::fs::read_to_string(&target.path) else {
        return false;
    };
    match target.kind {
        HookTargetKind::JsonSettings | HookTargetKind::CodexJson => json_hook_entries(&content)
            .any(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|hooks| hooks.iter().any(json_command_is_aoe))
            }),
        HookTargetKind::CodexToml => codex_config_has_aoe_marker(&content),
        HookTargetKind::Sidecar(sidecar) => match sidecar.format {
            // Kimi shares settl's flat `[[hooks]]` layout.
            SidecarFormat::SettlToml | SidecarFormat::KimiToml => {
                toml::from_str::<toml::Value>(&content).is_ok_and(|value| {
                    value
                        .get("hooks")
                        .and_then(toml::Value::as_array)
                        .is_some_and(|hooks| {
                            hooks.iter().any(|hook| {
                                hook.get("command")
                                    .and_then(toml::Value::as_str)
                                    .is_some_and(is_aoe_hook_command)
                            })
                        })
                })
            }
            SidecarFormat::HermesYaml => hermes_config_has_aoe_marker(&content),
            SidecarFormat::KiroJson => json_hook_entries(&content).any(|e| json_command_is_aoe(&e)),
        },
    }
}

/// Entries of every `hooks.<event>` array in a JSON document.
fn json_hook_entries(content: &str) -> impl Iterator<Item = Value> {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|mut v| v.get_mut("hooks").map(Value::take))
        .and_then(|hooks| match hooks {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .into_iter()
        .flat_map(|map| map.into_iter())
        .filter_map(|(_, entries)| match entries {
            Value::Array(entries) => Some(entries),
            _ => None,
        })
        .flatten()
}

fn codex_config_has_aoe_marker(content: &str) -> bool {
    let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    let Some(hooks) = doc.as_table().get("hooks").and_then(|h| h.as_table_like()) else {
        return false;
    };
    let group_has_aoe =
        |group: &dyn toml_edit::TableLike| codex_group_aoe_flags(group).contains(&true);
    CODEX_HOOK_EVENT_NAMES
        .iter()
        .filter_map(|name| hooks.get(name))
        .any(|item| {
            if let Some(groups) = item.as_array_of_tables() {
                groups.iter().any(|g| group_has_aoe(g))
            } else {
                item.as_array().is_some_and(|groups| {
                    groups
                        .iter()
                        .filter_map(toml_edit::Value::as_inline_table)
                        .any(|g| group_has_aoe(g))
                })
            }
        })
}

fn hermes_config_has_aoe_marker(content: &str) -> bool {
    if content.trim().is_empty() {
        return false;
    }
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(content) else {
        return false;
    };
    value
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("hooks".into())))
        .and_then(serde_yaml::Value::as_mapping)
        .is_some_and(|hooks| {
            hooks
                .values()
                .filter_map(serde_yaml::Value::as_sequence)
                .flatten()
                .any(|entry| {
                    entry
                        .as_mapping()
                        .and_then(|m| m.get(serde_yaml::Value::String("command".into())))
                        .and_then(serde_yaml::Value::as_str)
                        .is_some_and(is_aoe_hook_command)
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::EnvGuard;
    use tempfile::TempDir;

    #[test]
    #[serial_test::serial]
    fn iter_hook_targets_includes_profile_codex_home() {
        let tmp = TempDir::new().unwrap();
        let _guard = EnvGuard::unset(&["CODEX_HOME"]);
        let _home = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let codex_home = tmp.path().join("profile-codex-home");
        let profile_dir = crate::session::get_profile_dir("codex-profile").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            format!("environment = [\"CODEX_HOME={}\"]\n", codex_home.display()),
        )
        .unwrap();

        let codex_paths: Vec<_> = iter_hook_targets()
            .into_iter()
            .filter(|t| matches!(t.kind, HookTargetKind::CodexJson))
            .map(|t| t.path)
            .collect();
        assert!(codex_paths.contains(&tmp.path().join(".codex/hooks.json")));
        assert!(codex_paths.contains(&codex_home.join("hooks.json")));
    }

    fn capture_logs(f: impl FnOnce()) -> String {
        let logs = crate::session::test_support::LogCapture::start();
        f();
        logs.contents()
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn collect_env_lists_warns_on_corrupt_global_config_but_not_on_missing() {
        let tmp = TempDir::new().unwrap();
        let _codex = EnvGuard::unset(&["CODEX_HOME"]);
        let _home = EnvGuard::set(&[("HOME", tmp.path())]);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let _xdg = EnvGuard::set(&[("XDG_CONFIG_HOME", tmp.path().join(".config"))]);
        let config_path = crate::session::get_app_dir().unwrap().join("config.toml");

        assert!(!config_path.exists());
        let missing = capture_logs(|| {
            collect_env_lists_from_session();
        });
        assert!(
            !missing.contains("Failed to load global config"),
            "{missing}"
        );

        std::fs::write(&config_path, "this = is = not = toml\n").unwrap();
        let malformed = capture_logs(|| {
            collect_env_lists_from_session();
        });
        assert_eq!(
            malformed.matches("Failed to load global config").count(),
            1,
            "{malformed}"
        );
        assert!(malformed.contains("session.store"), "{malformed}");
    }
}
