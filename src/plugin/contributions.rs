//! Normalized contributions from the active plugin set.

use std::path::{Component, Path, PathBuf};

use super::registry::LoadedPlugin;

fn resolve_under_dir(plugin: &LoadedPlugin, rel: &str) -> Option<PathBuf> {
    let dir = plugin.dir.as_ref()?;
    let rel = Path::new(rel);
    if rel.as_os_str().is_empty()
        || rel.has_root()
        || rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return None;
    }
    let base = dir.canonicalize().ok()?;
    let candidate = base.join(rel).canonicalize().ok()?;
    candidate.starts_with(&base).then_some(candidate)
}

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

    fn theme(name: &str, path: &str) -> ThemeContribution {
        ThemeContribution {
            name: name.into(),
            path: path.into(),
        }
    }

    #[test]
    fn resolves_relative_theme_under_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("acme.kit");
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        let file = dir.join("themes/dark.toml");
        std::fs::write(&file, "background = \"#000000\"\n").unwrap();

        let p = loaded(Some(dir), vec![theme("kit-dark", "themes/dark.toml")]);
        let themes = active_themes(&[&p]);
        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0].0, "kit-dark");
        assert_eq!(themes[0].1, file.canonicalize().unwrap());
    }

    #[test]
    fn rejects_escaping_symlinked_and_builtin_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("acme.kit");
        std::fs::create_dir_all(&dir).unwrap();
        let escaping = loaded(
            Some(dir),
            vec![
                theme("abs", "/etc/evil.toml"),
                theme("dotdot", "../../etc/evil.toml"),
                theme("empty", ""),
            ],
        );
        assert!(active_themes(&[&escaping]).is_empty());

        let builtin = loaded(None, vec![theme("x", "x.toml")]);
        assert!(active_themes(&[&builtin]).is_empty());

        let dir = tmp.path().join("acme.kit");
        let outside = tmp.path().join("outside.toml");
        std::fs::write(&outside, "background = \"#000000\"\n").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link.toml")).unwrap();
        let symlinked = loaded(Some(dir), vec![theme("esc", "link.toml")]);
        assert!(
            active_themes(&[&symlinked]).is_empty(),
            "a symlink escaping the plugin dir must not resolve"
        );
    }
}
