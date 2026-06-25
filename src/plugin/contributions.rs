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
    // Reject syntactic escapes first: empty, rooted (absolute or Windows
    // root-relative like `\Windows\...`), a drive prefix, or any `..`.
    if rel.as_os_str().is_empty()
        || rel.has_root()
        || rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return None;
    }
    // Then canonicalize both and require the resolved candidate to stay under
    // the plugin directory, so a symlink inside the plugin dir cannot point
    // outside it. A non-existent file canonicalizes to None and is dropped (it
    // could not load anyway).
    let base = dir.canonicalize().ok()?;
    let candidate = base.join(rel).canonicalize().ok()?;
    candidate.starts_with(&base).then_some(candidate)
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
    fn rejects_escaping_and_builtin_paths() {
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

        // A builtin (no dir) contributes no file themes.
        let builtin = loaded(None, vec![theme("x", "x.toml")]);
        assert!(active_themes(&[&builtin]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("acme.kit");
        std::fs::create_dir_all(&dir).unwrap();
        let outside = tmp.path().join("outside.toml");
        std::fs::write(&outside, "background = \"#000000\"\n").unwrap();
        // A symlink inside the plugin dir pointing outside it must be rejected.
        std::os::unix::fs::symlink(&outside, dir.join("link.toml")).unwrap();

        let p = loaded(Some(dir), vec![theme("esc", "link.toml")]);
        assert!(
            active_themes(&[&p]).is_empty(),
            "a symlink escaping the plugin dir must not resolve"
        );
    }
}
