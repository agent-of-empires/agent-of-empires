//! Every on-disk location AoE may have written hooks to, shared by uninstall
//! and the hook-rewrite migrations.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::codex::{codex_group_aoe_flags, CODEX_HOOK_EVENT_NAMES};
use super::command::{is_aoe_hook_command, json_command_is_aoe};
use super::config_io::resolved;
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
    pub agent_name: String,
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
    let mut targets = iter_all_hook_targets_in(&home, &collect_env_lists_from_session());
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
                    let Some(root) = config.session.agent_config_dir_for(tool, &home) else {
                        continue;
                    };
                    // A declaration outranks the built-in its command names, as at launch.
                    let command = config.session.launch_command_for(tool);
                    if let Some(declared) =
                        crate::session::host_hook_declared_agent(tool, &command, &config.session)
                    {
                        declared.with_definition(|agent| {
                            push_relocated_target(&mut targets, agent, root, &config)
                        });
                        continue;
                    }
                    if let Some(agent) =
                        crate::session::resolved_agent_for(&profile, tool, detect_as)
                    {
                        if agent.hook_config.is_some() {
                            push_relocated_target(&mut targets, agent, root, &config);
                        } else if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
                            let relative: PathBuf = Path::new(sidecar.host_config_subpath)
                                .components()
                                .skip(1)
                                .collect();
                            let path = root.join(relative);
                            if !targets.iter().any(|target| {
                                matches!(target.kind, HookTargetKind::Sidecar(_))
                                    && target.path == path
                            }) {
                                let events =
                                    crate::agents::resolved_sidecar_hook_events(agent, &config)
                                        .unwrap_or_default();
                                targets.push(HookTarget {
                                    agent_name: agent.name.to_string(),
                                    kind: HookTargetKind::Sidecar(sidecar),
                                    path,
                                    events,
                                });
                            }
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
                                agent_name: "claude".to_string(),
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

/// Built-in agents only. The versioned hook-rewrite migrations run once with the
/// lenient writer, so they must never see a plugin's files.
pub(crate) fn iter_hook_targets_in(home: &Path, env_lists: &[Vec<String>]) -> Vec<HookTarget> {
    builtin_hook_targets_in(home, env_lists, &crate::session::config::Config::default())
}

/// Built-in and plugin-declared targets, for status, consent and uninstall.
pub(crate) fn iter_all_hook_targets_in(home: &Path, env_lists: &[Vec<String>]) -> Vec<HookTarget> {
    let defaults = crate::session::config::Config::default();
    let mut out = builtin_hook_targets_in(home, env_lists, &defaults);
    // Declarations of plugins that are active now; hooks written by one that
    // has since been removed stay until the plugin is back.
    let registry = crate::plugin::registry();
    for tool in crate::plugin::agent_integration::hook_declaring_tools(&registry) {
        let Some(declared) = crate::plugin::agent_integration::declared_agent(
            registry.clone(),
            &tool,
            std::iter::empty(),
        ) else {
            continue;
        };
        declared.with_definition(|agent| {
            push_hook_config_targets(&mut out, agent, home, env_lists, &defaults)
        });
    }
    out
}

/// Whether `path` is a built-in agent other than `agent_name`'s own settings file,
/// at its default, environment or `session.agent_config_dir` location, which no
/// plugin declaration may take over.
pub(crate) fn is_other_builtin_hook_path(
    path: &Path,
    agent_name: &str,
    home: &Path,
    host_env: &[String],
    session: &crate::session::config::SessionConfig,
) -> bool {
    let destination = resolved(path);
    let relocated = crate::agents::AGENTS
        .iter()
        .filter(|agent| agent.name != agent_name)
        .filter_map(|agent| {
            let leaf = Path::new(agent.hook_config.as_ref()?.settings_rel_path).file_name()?;
            Some(session.agent_config_dir_for(agent.name, home)?.join(leaf))
        });
    builtin_hook_targets_in(
        home,
        &[host_env.to_vec()],
        &crate::session::config::Config::default(),
    )
    .into_iter()
    .filter(|target| target.agent_name != agent_name)
    .map(|target| target.path)
    .chain(relocated)
    .any(|builtin| resolved(&builtin) == destination)
}

fn builtin_hook_targets_in(
    home: &Path,
    env_lists: &[Vec<String>],
    defaults: &crate::session::config::Config,
) -> Vec<HookTarget> {
    let mut out = Vec::new();
    for agent in crate::agents::AGENTS {
        push_hook_config_targets(&mut out, agent, home, env_lists, defaults);
        if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
            out.push(HookTarget {
                agent_name: agent.name.to_string(),
                kind: HookTargetKind::Sidecar(sidecar),
                path: home.join(sidecar.host_config_subpath),
                events: crate::agents::resolved_sidecar_hook_events(agent, defaults)
                    .unwrap_or_default(),
            });
        }
    }
    out
}

/// A settings file relocated by `session.agent_config_dir`.
fn push_relocated_target(
    targets: &mut Vec<HookTarget>,
    agent: &crate::agents::AgentDef,
    root: PathBuf,
    config: &crate::session::config::Config,
) {
    let Some(hook_cfg) = agent.hook_config.as_ref() else {
        return;
    };
    let Some(file) = Path::new(hook_cfg.settings_rel_path).file_name() else {
        return;
    };
    let path = root.join(file);
    if targets
        .iter()
        .any(|target| matches!(target.kind, HookTargetKind::JsonSettings) && target.path == path)
    {
        return;
    }
    let events = crate::agents::resolved_hook_events(agent, config).unwrap_or_default();
    targets.push(HookTarget {
        agent_name: agent.name.to_string(),
        kind: HookTargetKind::JsonSettings,
        path,
        events,
    });
}

fn push_hook_config_targets(
    out: &mut Vec<HookTarget>,
    agent: &crate::agents::AgentDef,
    home: &Path,
    env_lists: &[Vec<String>],
    defaults: &crate::session::config::Config,
) {
    let Some(hook_cfg) = agent.hook_config.as_ref() else {
        return;
    };
    let resolve = |env: &[String]| match hook_cfg.format {
        HookFormat::JsonSettings => agent_settings_path_in(home, hook_cfg, env),
        HookFormat::CodexJson => codex_hooks_json_path_in(home, env),
    };
    let kind = match hook_cfg.format {
        HookFormat::JsonSettings => HookTargetKind::JsonSettings,
        HookFormat::CodexJson => HookTargetKind::CodexJson,
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for path in std::iter::once(resolve(&[])).chain(env_lists.iter().map(|env| resolve(env))) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    for path in paths {
        if out.iter().any(|target| {
            target.path == path
                && matches!(
                    target.kind,
                    HookTargetKind::JsonSettings | HookTargetKind::CodexJson
                )
        }) {
            continue;
        }
        out.push(HookTarget {
            agent_name: agent.name.to_string(),
            kind,
            path,
            events: crate::agents::resolved_hook_events(agent, defaults).unwrap_or_default(),
        });
    }
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
    let Ok(value) = serde_saphyr::from_str_with_options::<Value>(content, crate::yaml::options())
    else {
        return false;
    };
    value
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .any(|entry| {
                    entry
                        .get("command")
                        .and_then(Value::as_str)
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
    fn hermes_marker_requires_a_string_hook_command() {
        for (yaml, installed) in [
            (
                "hooks:\n  pre_tool_call:\n    - command: 'exit 0 # aoe-hooks'\n",
                true,
            ),
            (
                "hooks:\n  pre_tool_call:\n    - command: echo user-hook\n",
                false,
            ),
            ("model: 'exit 0 # aoe-hooks'\n", false),
            (
                "hooks:\n  pre_tool_call: {command: 'exit 0 # aoe-hooks'}\n",
                false,
            ),
            ("hooks:\n  pre_tool_call: [{command: 17}]\n", false),
            ("hooks: [", false),
            ("scalar", false),
            ("", false),
        ] {
            assert_eq!(hermes_config_has_aoe_marker(yaml), installed, "{yaml:?}");
        }
    }

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

    #[test]
    #[serial_test::serial(shell_env)]
    fn plugin_declared_hooks_are_listed_and_consented() {
        let app = TempDir::new().unwrap();
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _app = crate::session::test_support::isolate_app_dir_at(app.path());
        let _env = EnvGuard::unset(&["EXAMPLE_CONFIG_DIR"]);
        let manifest =
            "id = \"acme.declared-hooks\"\nname = \"Declared hooks\"\nversion = \"1.0.0\"\n\
            api_version = 28\n[[agent_integrations]]\ntool = \"example-helper\"\n\
            command = \"example-helper\"\n\
            [agent_integrations.launch.container_env]\nEXAMPLE_CONFIG_DIR = \"/opt/example\"\n\
            [agent_integrations.hooks]\nsettings_rel_path = \".example/settings.json\"\n\
            config_dir_env_var = \"EXAMPLE_CONFIG_DIR\"\nformat = \"json_settings\"\n\
            [[agent_integrations.hooks.events]]\nevent = \"Stop\"\nstatus = \"idle\"\n\
            [agent_integrations.trust]\nfile = \".example.json\"\n\
            location = \"config_dir_or_home\"\npointer = \"/projects/{path}/trusted\"\n\
            value = true\nsandbox = \"always\"\n";
        crate::session::test_support::install_agent_plugin("acme.declared-hooks", manifest);

        let home = TempDir::new().unwrap();
        let target = iter_all_hook_targets_in(home.path(), &[])
            .into_iter()
            .find(|target| target.agent_name == "example-helper")
            .expect("declared agent is a hook target");
        assert!(matches!(target.kind, HookTargetKind::JsonSettings));
        assert_eq!(target.path, home.path().join(".example/settings.json"));
        assert_eq!(target.events.len(), 1);

        let lines = crate::plugin::agent_integration::consent_lines(
            &aoe_plugin_api::PluginManifest::from_toml_str(manifest)
                .unwrap()
                .agent_integrations[0],
        )
        .join("\n");
        assert!(lines.contains("~/.example/settings.json or $EXAMPLE_CONFIG_DIR/settings.json"));
        assert!(lines.contains("/projects/{path}/trusted = true into ~/.example.json"));
        assert!(lines.contains("kept on uninstall"));

        // Migrations see built-in agents only.
        assert!(iter_hook_targets_in(home.path(), &[])
            .iter()
            .all(|target| target.agent_name != "example-helper"));

        // A profile's relocated config directory is found too.
        let relocated = home.path().join("relocated");
        let profile_dir = crate::session::get_profile_dir("declared-profile").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            format!(
                "[session.agent_config_dir]\nexample-helper = \"{}\"\n",
                relocated.display()
            ),
        )
        .unwrap();
        assert!(iter_hook_targets()
            .into_iter()
            .any(|target| target.agent_name == "example-helper"
                && target.path == relocated.join("settings.json")));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(shell_env)]
    fn builtin_hook_ownership_follows_symlinks() {
        let _env = EnvGuard::unset(&["CLAUDE_CONFIG_DIR", "CODEX_HOME"]);
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::os::unix::fs::symlink(home.join(".claude"), home.join(".example")).unwrap();
        let dotfiles = home.join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::os::unix::fs::symlink(&dotfiles, home.join(".mine")).unwrap();
        let owned = |path: &str, agent: &str| {
            is_other_builtin_hook_path(&home.join(path), agent, home, &[], &Default::default())
        };

        // An alias of another agent's directory is that agent's file, existing or not.
        assert!(owned(".example/settings.json", "example"));
        std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
        assert!(owned(".example/settings.json", "example"));
        // A symlinked dotfile that lands elsewhere stays the plugin's own, as does the built-in itself.
        assert!(!owned(".mine/settings.json", "example"));
        assert!(!owned(".claude/settings.json", "claude"));
        assert!(!owned(".example/settings.json", "claude"));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(shell_env)]
    fn builtin_hook_ownership_follows_dangling_symlinks() {
        let _env = EnvGuard::unset(&["CLAUDE_CONFIG_DIR", "CODEX_HOME"]);
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".mine")).unwrap();
        std::fs::create_dir_all(home.join(".rel")).unwrap();
        std::fs::create_dir_all(home.join("dotfiles")).unwrap();
        // Claude's settings file does not exist yet, so every link here dangles.
        std::os::unix::fs::symlink(
            home.join(".claude/settings.json"),
            home.join(".mine/settings.json"),
        )
        .unwrap();
        std::os::unix::fs::symlink("../.claude/settings.json", home.join(".rel/settings.json"))
            .unwrap();
        std::os::unix::fs::symlink(
            home.join(".mine/settings.json"),
            home.join("dotfiles/chain.json"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            home.join("dotfiles/missing.json"),
            home.join(".mine/own.json"),
        )
        .unwrap();
        let owned = |path: &str| {
            is_other_builtin_hook_path(&home.join(path), "example", home, &[], &Default::default())
        };

        assert!(owned(".mine/settings.json"));
        assert!(owned(".rel/settings.json"));
        assert!(owned("dotfiles/chain.json"), "a chain of links is followed");
        assert!(
            !owned(".mine/own.json"),
            "a link to the plugin's own file stays allowed"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn builtin_hook_ownership_covers_relocated_settings() {
        let _env = EnvGuard::unset(&["CLAUDE_CONFIG_DIR", "CODEX_HOME"]);
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let mut session = crate::session::config::SessionConfig::default();
        session.agent_config_dir.insert(
            "claude".into(),
            home.join("shared").to_string_lossy().into_owned(),
        );
        let owned = |path: &str, agent: &str| {
            is_other_builtin_hook_path(&home.join(path), agent, home, &[], &session)
        };

        assert!(owned("shared/settings.json", "example"));
        assert!(!owned("shared/settings.json", "claude"), "its own file");
        assert!(!owned("elsewhere/settings.json", "example"));
        assert!(
            owned(".claude/settings.json", "example"),
            "defaults still count"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(shell_env)]
    fn builtin_hook_ownership_follows_chains_as_deep_as_the_writer() {
        let _env = EnvGuard::unset(&["CLAUDE_CONFIG_DIR", "CODEX_HOME"]);
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join("links")).unwrap();
        // 20 links, the last dangling at Claude's missing settings file.
        let link = |n: usize| home.join(format!("links/l{n}"));
        std::os::unix::fs::symlink(home.join(".claude/settings.json"), link(19)).unwrap();
        for n in (0..19).rev() {
            std::os::unix::fs::symlink(link(n + 1), link(n)).unwrap();
        }

        assert!(is_other_builtin_hook_path(
            &link(0),
            "example",
            home,
            &[],
            &Default::default()
        ));
    }
}
