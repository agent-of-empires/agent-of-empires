//! Normalized Tier 0 contributions from the active plugin set.
//!
//! Each surface (themes, settings schema, keybinds, CLI) reads its slice of the
//! manifest through one place here rather than walking `registry().active()` and
//! manifest fields itself, so contribution-filtering rules (active-only, path
//! safety, id namespacing) live once.

use std::path::{Component, Path, PathBuf};

use super::registry::LoadedPlugin;

/// Resolve a plugin-relative resource path under the plugin's install
/// directory, rejecting anything that escapes it (absolute paths, `..`). A
/// builtin (no on-disk dir) ships no file resources, so it returns `None`.
fn resolve_under_dir(plugin: &LoadedPlugin, rel: &str) -> Option<PathBuf> {
    let dir = plugin.dir.as_ref()?;
    let rel = Path::new(rel);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return None;
    }
    Some(dir.join(rel))
}

/// Themes contributed by active plugins, as `(name, path)` pairs. The path is
/// resolved under the contributing plugin's directory; unsafe or builtin-only
/// paths are skipped.
pub fn active_themes(plugins: &[&LoadedPlugin]) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for plugin in plugins {
        for theme in &plugin.manifest.themes {
            if let Some(path) = resolve_under_dir(plugin, &theme.path) {
                out.push((theme.name.clone(), path));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::registry::ValidationState;
    use aoe_plugin_api::{PluginManifest, ThemeContribution, TrustLevel};

    fn loaded(dir: Option<PathBuf>, themes: Vec<ThemeContribution>) -> LoadedPlugin {
        let mut manifest = PluginManifest::from_toml_str(
            r#"
id = "acme.kit"
name = "Kit"
version = "0.1.0"
api_version = 2
"#,
        )
        .unwrap();
        manifest.themes = themes;
        LoadedPlugin {
            manifest,
            enabled: true,
            trust: TrustLevel::Community,
            validation: ValidationState::Community,
            source: None,
            dir,
            manifest_hash: "sha256:x".into(),
            granted: true,
        }
    }

    #[test]
    fn resolves_relative_theme_under_dir() {
        let p = loaded(
            Some(PathBuf::from("/plugins/acme.kit")),
            vec![ThemeContribution {
                name: "kit-dark".into(),
                path: "themes/dark.toml".into(),
            }],
        );
        let themes = active_themes(&[&p]);
        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0].0, "kit-dark");
        assert_eq!(
            themes[0].1,
            PathBuf::from("/plugins/acme.kit/themes/dark.toml")
        );
    }

    #[test]
    fn rejects_escaping_and_builtin_paths() {
        let escaping = loaded(
            Some(PathBuf::from("/plugins/acme.kit")),
            vec![
                ThemeContribution {
                    name: "abs".into(),
                    path: "/etc/evil.toml".into(),
                },
                ThemeContribution {
                    name: "dotdot".into(),
                    path: "../../etc/evil.toml".into(),
                },
            ],
        );
        assert!(active_themes(&[&escaping]).is_empty());

        // A builtin (no dir) contributes no file themes.
        let builtin = loaded(
            None,
            vec![ThemeContribution {
                name: "x".into(),
                path: "x.toml".into(),
            }],
        );
        assert!(active_themes(&[&builtin]).is_empty());
    }
}
