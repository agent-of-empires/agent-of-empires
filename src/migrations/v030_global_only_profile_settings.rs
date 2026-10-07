//! Move the default profile's global-only overrides into the global config and
//! drop them from every other profile, before profile merges start ignoring them.

use anyhow::{Context, Result};
use std::{collections::HashSet, fs, io::ErrorKind, path::Path};
use tracing::info;

use crate::session::{
    acquire_storage_flock, atomic_write,
    config::{
        settings_schema::{merge_json, schema_ref},
        CONFIG_LOCK_FILENAME,
    },
    list_profile_names_in, Config,
};

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?, atomic_write)
}

fn read_config(path: &Path) -> Result<Option<toml::Table>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    content
        .parse()
        .map(Some)
        .with_context(|| format!("Parse {} during v030 migration", path.display()))
}

fn run_in(app_dir: &Path, mut write: impl FnMut(&Path, &[u8]) -> Result<()>) -> Result<()> {
    let _lock = acquire_storage_flock(app_dir, CONFIG_LOCK_FILENAME)?;
    let global_path = app_dir.join("config.toml");
    let mut global = read_config(&global_path)?.unwrap_or_default();
    let profiles_dir = app_dir.join("profiles");
    let names = if profiles_dir.exists() {
        list_profile_names_in(&profiles_dir)?
    } else {
        Vec::new()
    };
    // Same rule as `resolve_default_profile`: the configured name, else the first profile.
    let default_path = global
        .get("default_profile")
        .and_then(toml::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| names.first().cloned())
        .map(|name| profiles_dir.join(name).join("config.toml"));
    let paths = default_path.iter().cloned().chain(
        names
            .iter()
            .map(|name| profiles_dir.join(name).join("config.toml")),
    );

    // Only the default profile's values move, so the profile users launch by
    // default keeps its effective settings. A profile aliasing the global
    // config is skipped: its values are already global.
    let mut seen = HashSet::new();
    if global_path.exists() {
        seen.insert(fs::canonicalize(&global_path)?);
    }
    let mut default_identity = None;
    let mut promoted = false;
    let mut profiles = Vec::new();
    for path in paths {
        let target = match fs::canonicalize(&path) {
            Ok(target) => target,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("Resolve {}", path.display())),
        };
        if default_path.as_ref() == Some(&path) {
            default_identity = Some(target.clone());
        }
        let is_default = default_identity.as_ref() == Some(&target);
        if !seen.insert(target) {
            continue;
        }
        let Some(mut profile) = read_config(&path)? else {
            continue;
        };
        let mut moved = Vec::new();
        for field in schema_ref()
            .iter()
            .filter(|field| !field.profile_overridable)
        {
            let Some(section) = profile
                .get_mut(&field.section)
                .and_then(toml::Value::as_table_mut)
            else {
                continue;
            };
            let Some(value) = section.remove(&field.field) else {
                continue;
            };
            if section.is_empty() {
                profile.remove(&field.section);
            }
            moved.push(field.path());
            if !is_default {
                continue;
            }
            let global_section = global
                .entry(&field.section)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                .as_table_mut()
                .with_context(|| {
                    format!("Global config section '{}' must be a table", field.section)
                })?;
            if let Some(current) = global_section.get_mut(&field.field) {
                let mut merged = serde_json::to_value(&*current)?;
                merge_json(&mut merged, &serde_json::to_value(value)?);
                *current = toml::Value::try_from(merged)?;
            } else {
                global_section.insert(field.field.clone(), value);
            }
            promoted = true;
        }
        if !moved.is_empty() {
            profiles.push((path, is_default, moved, toml::to_string_pretty(&profile)?));
        }
    }
    if profiles.is_empty() {
        return Ok(());
    }
    // Global first: a retry after a failed profile write re-promotes the same values.
    if promoted {
        let _: Config = toml::Value::Table(global.clone())
            .try_into()
            .context("Invalid global config after migrating profile settings")?;
        write(&global_path, toml::to_string_pretty(&global)?.as_bytes())?;
    }
    for (path, is_default, fields, content) in profiles {
        write(&path, content.as_bytes())?;
        if is_default {
            info!(path = %path.display(), ?fields, "v030: moved default profile's global-only settings to global config");
        } else {
            info!(path = %path.display(), ?fields, "v030: dropped global-only settings from non-default profile");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(app: &Path, path: &str, contents: &str) {
        let path = app.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn read(app: &Path, path: &str) -> toml::Table {
        read_config(&app.join(path)).unwrap().unwrap()
    }

    #[test]
    fn promotes_only_the_default_profile_and_preserves_other_data() {
        // (global prefix, default profile resolved from it)
        for (default, winner) in [
            ("default_profile = 'work'\n", Some("work")),
            ("", Some("alpha")),
            ("default_profile = ''\n", Some("alpha")),
            ("default_profile = 'missing'\n", None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let app = dir.path();
            seed(
                app,
                "config.toml",
                &format!("{default}[theme]\nname = 'empire'\n[unknown]\nkeep = 7\n"),
            );
            seed(
                app,
                "profiles/alpha/config.toml",
                "[theme]\nname = 'dracula'\n[web]\nnotify_on_idle = true\n",
            );
            seed(app, "profiles/work/config.toml", "description = 'keep me'\n[theme]\nname = 'rose-pine'\ncolor_mode = 'palette'\nidle_decay_minutes = 5\n[session]\nconfirm_before_quit = false\nsession_id_poller_max_threads = 12\nsidebar_position = 'right'\ndefault_tool = 'codex'\n[web]\nnotify_on_error = false\n[unknown]\nkeep = 'profile'\n");

            run_in(app, atomic_write).unwrap();

            let global = read(app, "config.toml");
            let get = |section: &str, field: &str| {
                global
                    .get(section)
                    .and_then(|s| s.get(field))
                    .map(ToString::to_string)
            };
            let theme = match winner {
                Some("work") => "rose-pine",
                Some(_) => "dracula",
                None => "empire",
            };
            assert_eq!(global["theme"]["name"].as_str(), Some(theme), "{default:?}");
            let work = winner == Some("work");
            let alpha = winner == Some("alpha");
            assert_eq!(get("theme", "color_mode").is_some(), work, "{default:?}");
            assert_eq!(get("session", "confirm_before_quit").is_some(), work);
            assert_eq!(
                get("session", "session_id_poller_max_threads").is_some(),
                work
            );
            assert_eq!(get("session", "sidebar_position").is_some(), work);
            assert_eq!(get("web", "notify_on_error").is_some(), work);
            assert_eq!(get("web", "notify_on_idle").is_some(), alpha);
            if work {
                assert_eq!(
                    global["session"]["sidebar_position"].as_str(),
                    Some("right")
                );
            }
            assert_eq!(global["unknown"]["keep"].as_integer(), Some(7));
            assert_eq!(read(app, "profiles/work/config.toml"), "description = 'keep me'\n[theme]\nidle_decay_minutes = 5\n[session]\ndefault_tool = 'codex'\n[unknown]\nkeep = 'profile'\n".parse::<toml::Table>().unwrap());
            assert!(read(app, "profiles/alpha/config.toml").is_empty());
            run_in(app, |_, _| {
                anyhow::bail!("idempotent migration must not write")
            })
            .unwrap();
        }
    }

    #[test]
    fn resumes_after_each_write_failure_without_changing_the_winner() {
        for fail_at in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let app = dir.path();
            seed(app, "config.toml", "default_profile = 'work'\n");
            for (name, theme) in [
                ("alpha", "dracula"),
                ("beta", "empire"),
                ("work", "rose-pine"),
            ] {
                seed(
                    app,
                    &format!("profiles/{name}/config.toml"),
                    &format!("[theme]\nname = '{theme}'\n"),
                );
            }
            let mut writes = 0;
            let result = run_in(app, |path, contents| {
                let current = writes;
                writes += 1;
                anyhow::ensure!(current != fail_at, "injected write failure");
                atomic_write(path, contents)
            });
            assert!(result.is_err());
            // The default's value is never lost: it is global, or still in the profile.
            let theme = |path| {
                read(app, path)
                    .get("theme")
                    .and_then(|t| t.get("name"))
                    .and_then(|n| n.as_str().map(str::to_owned))
            };
            assert!(
                theme("config.toml").as_deref() == Some("rose-pine")
                    || theme("profiles/work/config.toml").as_deref() == Some("rose-pine")
            );
            run_in(app, atomic_write).unwrap();
            assert_eq!(
                read(app, "config.toml")["theme"]["name"].as_str(),
                Some("rose-pine")
            );
            for name in ["alpha", "beta", "work"] {
                assert!(read(app, &format!("profiles/{name}/config.toml")).is_empty());
            }
        }
    }

    #[test]
    fn rejects_malformed_input_before_any_writes() {
        for (path, contents) in [
            ("config.toml", "[invalid"),
            ("profiles/work/config.toml", "[invalid"),
            (
                "profiles/aaa/config.toml",
                "[session]\nconfirm_before_quit = 'wrong type'\n",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let app = dir.path();
            seed(
                app,
                "profiles/other/config.toml",
                "[theme]\nname = 'dracula'\n",
            );
            seed(app, path, contents);
            assert!(run_in(app, |_, _| panic!(
                "must validate all inputs before writing"
            ))
            .is_err());
            assert_eq!(fs::read_to_string(app.join(path)).unwrap(), contents);
        }
    }

    #[test]
    fn handles_missing_files_and_does_not_rewrite_unrelated_settings() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path();
        run_in(app, |_, _| panic!("fresh install must not write")).unwrap();
        fs::create_dir_all(app.join("profiles/empty")).unwrap();
        seed(
            app,
            "profiles/work/config.toml",
            "# keep this comment\n[session]\ndefault_tool = 'codex'\n",
        );
        run_in(app, |_, _| {
            panic!("unrelated settings must not be rewritten")
        })
        .unwrap();
        seed(
            app,
            "profiles/other/config.toml",
            "[theme]\nname = 'dracula'\n",
        );
        run_in(app, atomic_write).unwrap();
        // `empty` is the default profile and has no file, so nothing moves.
        assert!(!app.join("config.toml").exists());
        assert!(read(app, "profiles/other/config.toml").is_empty());
        assert!(fs::read_to_string(app.join("profiles/work/config.toml"))
            .unwrap()
            .starts_with("# keep this comment"));
    }

    #[test]
    fn merges_default_profile_maps_and_replaces_lists() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path();
        seed(app, "config.toml", "default_profile = 'work'\n[logging.targets]\ntmux = 'info'\nserver = 'error'\n[acp]\nallowed_agents = ['claude']\n");
        seed(app, "profiles/alpha/config.toml", "[logging.targets]\nsession = 'debug'\nserver = 'warn'\n[acp]\nallowed_agents = ['gemini']\n");
        seed(
            app,
            "profiles/work/config.toml",
            "[logging.targets]\nserver = 'debug'\n[acp]\nallowed_agents = ['codex']\n",
        );
        run_in(app, atomic_write).unwrap();
        let global = read(app, "config.toml");
        assert_eq!(
            global["logging"]["targets"],
            toml::Value::Table(
                "tmux = 'info'\nserver = 'debug'\n"
                    .parse::<toml::Table>()
                    .unwrap()
            )
        );
        assert_eq!(
            global["acp"]["allowed_agents"].as_array().unwrap(),
            &[toml::Value::String("codex".into())]
        );
        assert!(read(app, "profiles/work/config.toml").is_empty());
        assert!(read(app, "profiles/alpha/config.toml").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn profile_aliases_preserve_precedence_and_retry_safety() {
        for (default, winner) in [("", "dracula"), ("default_profile = 'aaa'\n", "rose-pine")] {
            let dir = tempfile::tempdir().unwrap();
            let app = dir.path();
            seed(app, "config.toml", default);
            seed(
                app,
                "profiles/alpha/config.toml",
                "[theme]\nname = 'dracula'\n",
            );
            seed(
                app,
                "profiles/zulu/config.toml",
                "[theme]\nname = 'rose-pine'\n",
            );
            std::os::unix::fs::symlink("zulu", app.join("profiles/aaa")).unwrap();
            run_in(app, atomic_write).unwrap();
            assert_eq!(
                read(app, "config.toml")["theme"]["name"].as_str(),
                Some(winner)
            );
            assert!(app.join("profiles/aaa").is_symlink());
        }
        for fail_at in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let app = dir.path();
            seed(app, "config.toml", "default_profile = 'zulu'\n");
            seed(
                app,
                "profiles/alpha/config.toml",
                "[theme]\nname = 'dracula'\n",
            );
            seed(
                app,
                "profiles/zulu/config.toml",
                "[theme]\nname = 'rose-pine'\n",
            );
            fs::create_dir_all(app.join("profiles/beta")).unwrap();
            std::os::unix::fs::symlink(
                "../zulu/config.toml",
                app.join("profiles/beta/config.toml"),
            )
            .unwrap();
            let mut writes = 0;
            assert!(run_in(app, |path, content| {
                let current = writes;
                writes += 1;
                anyhow::ensure!(current != fail_at, "injected write failure");
                atomic_write(path, content)
            })
            .is_err());
            run_in(app, atomic_write).unwrap();
            assert_eq!(
                read(app, "config.toml")["theme"]["name"].as_str(),
                Some("rose-pine")
            );
            assert!(read(app, "profiles/beta/config.toml").is_empty());
            assert!(app.join("profiles/beta/config.toml").is_symlink());
        }
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path();
        seed(
            app,
            "config.toml",
            "default_profile = 'work'\n[theme]\nname = 'dracula'\n",
        );
        fs::create_dir_all(app.join("profiles/work")).unwrap();
        std::os::unix::fs::symlink("../../config.toml", app.join("profiles/work/config.toml"))
            .unwrap();
        seed(
            app,
            "profiles/alpha/config.toml",
            "[theme]\nname = 'rose-pine'\n[web]\nnotify_on_idle = true\n",
        );
        run_in(app, atomic_write).unwrap();
        assert!(read(app, "profiles/alpha/config.toml").is_empty());
        assert!(!read(app, "config.toml").contains_key("web"));
        assert!(app.join("profiles/work/config.toml").is_symlink());
        run_in(app, |_, _| {
            panic!("migrated aliases must not trigger writes")
        })
        .unwrap();
        assert_eq!(
            read(app, "config.toml")["theme"]["name"].as_str(),
            Some("dracula")
        );
    }

    #[test]
    fn holds_the_global_config_lock_through_profile_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path();
        seed(
            app,
            "profiles/work/config.toml",
            "[theme]\nname = 'dracula'\n",
        );
        let mut written = Vec::new();
        run_in(app, |path, contents| {
            assert!(
                crate::session::try_acquire_storage_flock(app, CONFIG_LOCK_FILENAME)?.is_none()
            );
            atomic_write(path, contents)?;
            written.push(path.strip_prefix(app).unwrap().to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            written,
            [
                Path::new("config.toml"),
                Path::new("profiles/work/config.toml")
            ]
        );
        assert!(
            crate::session::try_acquire_storage_flock(app, CONFIG_LOCK_FILENAME)
                .unwrap()
                .is_some()
        );
    }
}
