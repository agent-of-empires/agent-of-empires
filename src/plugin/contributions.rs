//! Normalized Tier 0 contributions from the active plugin set.
//!
//! Each surface (themes, settings schema, keybinds, CLI) reads its slice of the
//! manifest through one place here rather than walking `registry().active()` and
//! manifest fields itself, so contribution-filtering rules (active-only, path
//! safety, id namespacing) live once.

use std::path::{Component, Path, PathBuf};

use regex::{Captures, Regex};

use super::registry::LoadedPlugin;

pub(crate) const MAX_BRANCH_OUTPUT_BYTES: usize = 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum BranchTransformError {
    #[error(
        "multiple active plugins contribute branch transforms: {}",
        .plugin_ids.join(", ")
    )]
    Conflict { plugin_ids: Vec<String> },
    #[error("plugin `{plugin_id}` branch transform rule {rule_index} has an invalid regex")]
    InvalidRegex {
        plugin_id: String,
        rule_index: usize,
    },
    #[error(
        "plugin `{plugin_id}` branch transform rule {rule_index} produced output over the 1024-byte limit"
    )]
    OutputTooLong {
        plugin_id: String,
        rule_index: usize,
    },
    #[error(
        "plugin `{plugin_id}` branch transform rule {rule_index} produced an invalid branch name"
    )]
    InvalidOutput {
        plugin_id: String,
        rule_index: usize,
    },
    #[error("resolved worktree branch name is invalid")]
    InvalidResolvedBranch,
}

#[derive(Debug)]
struct CompiledBranchTransform {
    regex: Regex,
    replacement: String,
}

fn capture_len(captures: &Captures<'_>, reference: &str) -> usize {
    match reference.parse::<usize>() {
        Ok(index) => captures.get(index),
        Err(_) => captures.name(reference),
    }
    .map_or(0, |matched| matched.as_str().len())
}

/// Compute regex replacement expansion length without materializing captures.
/// This mirrors the regex crate's `$name`, `${name}`, `$1`, and `$$` syntax.
fn expanded_replacement_len(captures: &Captures<'_>, mut replacement: &str) -> usize {
    let mut len = 0usize;
    while let Some(dollar) = replacement.as_bytes().iter().position(|byte| *byte == b'$') {
        len = len.saturating_add(dollar);
        replacement = &replacement[dollar..];

        if replacement.as_bytes().get(1) == Some(&b'$') {
            len = len.saturating_add(1);
            replacement = &replacement[2..];
            continue;
        }

        let capture = if replacement.as_bytes().get(1) == Some(&b'{') {
            replacement[2..]
                .find('}')
                .map(|end| (&replacement[2..2 + end], 3 + end))
        } else {
            let end = replacement.as_bytes()[1..]
                .iter()
                .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                .count();
            (end > 0).then(|| (&replacement[1..1 + end], 1 + end))
        };

        let Some((reference, end)) = capture else {
            len = len.saturating_add(1);
            replacement = &replacement[1..];
            continue;
        };
        len = len.saturating_add(capture_len(captures, reference));
        replacement = &replacement[end..];
    }
    len.saturating_add(replacement.len())
}

fn replace_first_bounded(rule: &CompiledBranchTransform, input: &str) -> Result<String, ()> {
    let Some(captures) = rule.regex.captures(input) else {
        return (input.len() <= MAX_BRANCH_OUTPUT_BYTES)
            .then(|| input.to_string())
            .ok_or(());
    };
    let matched = captures.get(0).expect("capture zero always exists");
    let output_len = matched
        .start()
        .saturating_add(expanded_replacement_len(&captures, &rule.replacement))
        .saturating_add(input.len() - matched.end());
    if output_len > MAX_BRANCH_OUTPUT_BYTES {
        return Err(());
    }

    let mut output = String::with_capacity(output_len);
    output.push_str(&input[..matched.start()]);
    captures.expand(&rule.replacement, &mut output);
    output.push_str(&input[matched.end()..]);
    Ok(output)
}

/// An owned, immutable snapshot of the active branch transform contribution.
#[derive(Debug, Default)]
pub(crate) struct BranchTransformPlan {
    plugin_id: Option<String>,
    rules: Vec<CompiledBranchTransform>,
}

impl BranchTransformPlan {
    pub(crate) fn apply(&self, branch: &str) -> Result<String, BranchTransformError> {
        let Some(plugin_id) = self.plugin_id.as_deref() else {
            return Ok(branch.to_string());
        };

        let mut output = branch.to_string();
        for (rule_index, rule) in self.rules.iter().enumerate() {
            output = replace_first_bounded(rule, &output).map_err(|()| {
                BranchTransformError::OutputTooLong {
                    plugin_id: plugin_id.to_string(),
                    rule_index,
                }
            })?;
            if !git2::Branch::name_is_valid(&output).unwrap_or(false) {
                return Err(BranchTransformError::InvalidOutput {
                    plugin_id: plugin_id.to_string(),
                    rule_index,
                });
            }
        }
        Ok(output)
    }
}

/// Build the branch transform plan from loaded plugins. Inactive contributions
/// are ignored, and one active contributor owns the complete ordered rule set.
pub(crate) fn branch_transform_plan(
    plugins: &[LoadedPlugin],
) -> Result<BranchTransformPlan, BranchTransformError> {
    let mut contributors: Vec<&LoadedPlugin> = plugins
        .iter()
        .filter(|plugin| plugin.active() && !plugin.manifest.branch_transforms.is_empty())
        .collect();

    if contributors.len() > 1 {
        let mut plugin_ids: Vec<String> = contributors
            .iter()
            .map(|plugin| plugin.id().to_string())
            .collect();
        plugin_ids.sort();
        return Err(BranchTransformError::Conflict { plugin_ids });
    }

    let Some(plugin) = contributors.pop() else {
        return Ok(BranchTransformPlan::default());
    };
    let rules = plugin
        .manifest
        .branch_transforms
        .iter()
        .enumerate()
        .map(|(rule_index, rule)| {
            Regex::new(&rule.pattern)
                .map(|regex| CompiledBranchTransform {
                    regex,
                    replacement: rule.replacement.clone(),
                })
                .map_err(|_| BranchTransformError::InvalidRegex {
                    plugin_id: plugin.id().to_string(),
                    rule_index,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(BranchTransformPlan {
        plugin_id: Some(plugin.id().to_string()),
        rules,
    })
}

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
    use aoe_plugin_api::{
        BranchTransformContribution, PluginManifest, ThemeContribution, TrustLevel,
    };

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

    fn branch_transform(pattern: &str, replacement: &str) -> BranchTransformContribution {
        BranchTransformContribution {
            pattern: pattern.into(),
            replacement: replacement.into(),
        }
    }

    fn loaded_with_branch_transforms(
        id: &str,
        transforms: Vec<BranchTransformContribution>,
        enabled: bool,
        granted: bool,
    ) -> LoadedPlugin {
        let mut manifest = PluginManifest::from_toml_str(&format!(
            "id = \"{id}\"\nname = \"Branch Style\"\nversion = \"0.1.0\"\napi_version = 9\n"
        ))
        .unwrap();
        manifest.branch_transforms = transforms;
        LoadedPlugin {
            manifest,
            enabled,
            trust: TrustLevel::Community,
            validation: ValidationState::Community,
            source: None,
            dir: None,
            manifest_hash: "sha256:x".into(),
            granted,
        }
    }

    #[test]
    fn branch_transform_replaces_only_first_match() {
        let plugin = loaded_with_branch_transforms(
            "acme.branch-style",
            vec![branch_transform("-", "/")],
            true,
            true,
        );
        let plan = branch_transform_plan(&[plugin]).unwrap();

        assert_eq!(
            plan.apply("chore-update-deps").unwrap(),
            "chore/update-deps"
        );
    }

    #[test]
    fn branch_transform_nonmatch_is_identity() {
        let plugin = loaded_with_branch_transforms(
            "acme.branch-style",
            vec![branch_transform("^fix-", "fix/")],
            true,
            true,
        );
        let plan = branch_transform_plan(&[plugin]).unwrap();

        assert_eq!(
            plan.apply("chore-update-deps").unwrap(),
            "chore-update-deps"
        );
    }

    #[test]
    fn branch_transform_preserves_manifest_order_and_expands_captures() {
        let plugin = loaded_with_branch_transforms(
            "acme.branch-style",
            vec![
                branch_transform(r"^([^-]+)-(.+)$", "$1/$2"),
                branch_transform(r"^([^/]+)/update-(.+)$", "${1}/${2}"),
            ],
            true,
            true,
        );
        let plan = branch_transform_plan(&[plugin]).unwrap();

        assert_eq!(plan.apply("chore-update-deps").unwrap(), "chore/deps");
    }

    #[test]
    fn branch_transform_ignores_disabled_and_ungranted_plugins() {
        let active = loaded_with_branch_transforms(
            "acme.active",
            vec![branch_transform("-", "/")],
            true,
            true,
        );
        let mut disabled = loaded_with_branch_transforms(
            "acme.disabled",
            vec![branch_transform("x", "y")],
            false,
            true,
        );
        disabled.manifest.branch_transforms[0].pattern = "private-invalid-(".into();
        let mut ungranted = loaded_with_branch_transforms(
            "acme.ungranted",
            vec![branch_transform("x", "y")],
            true,
            false,
        );
        ungranted.manifest.branch_transforms[0].pattern = "private-invalid-[".into();

        let plan = branch_transform_plan(&[disabled, active, ungranted]).unwrap();
        assert_eq!(
            plan.apply("chore-update-deps").unwrap(),
            "chore/update-deps"
        );
    }

    #[test]
    fn branch_transform_conflict_ids_are_sorted_regardless_of_input_order() {
        let first = loaded_with_branch_transforms(
            "acme.alpha",
            vec![branch_transform("a", "b")],
            true,
            true,
        );
        let second = loaded_with_branch_transforms(
            "acme.zulu",
            vec![branch_transform("a", "b")],
            true,
            true,
        );
        let reversed = branch_transform_plan(&[second, first]).unwrap_err();

        let first = loaded_with_branch_transforms(
            "acme.alpha",
            vec![branch_transform("a", "b")],
            true,
            true,
        );
        let second = loaded_with_branch_transforms(
            "acme.zulu",
            vec![branch_transform("a", "b")],
            true,
            true,
        );
        let ordered = branch_transform_plan(&[first, second]).unwrap_err();

        assert_eq!(reversed, ordered);
        assert_eq!(
            reversed,
            BranchTransformError::Conflict {
                plugin_ids: vec!["acme.alpha".into(), "acme.zulu".into()]
            }
        );
    }

    #[test]
    fn branch_transform_defensively_rejects_invalid_regex_without_leaking_it() {
        let mut plugin = loaded_with_branch_transforms(
            "acme.branch-style",
            vec![branch_transform("valid", "replacement")],
            true,
            true,
        );
        plugin.manifest.branch_transforms[0].pattern = "private-invalid-(".into();

        let error = branch_transform_plan(&[plugin]).unwrap_err();
        assert_eq!(
            error,
            BranchTransformError::InvalidRegex {
                plugin_id: "acme.branch-style".into(),
                rule_index: 0,
            }
        );
        assert!(!error.to_string().contains("private-invalid"));
    }

    #[test]
    fn branch_transform_rejects_invalid_output_without_leaking_it() {
        let plugin = loaded_with_branch_transforms(
            "acme.branch-style",
            vec![branch_transform("^private-branch$", ".")],
            true,
            true,
        );
        let plan = branch_transform_plan(&[plugin]).unwrap();

        let error = plan.apply("private-branch").unwrap_err();
        assert_eq!(
            error,
            BranchTransformError::InvalidOutput {
                plugin_id: "acme.branch-style".into(),
                rule_index: 0,
            }
        );
        let message = error.to_string();
        assert!(!message.contains("private-branch"));
        assert!(!message.contains("^private"));
    }

    #[test]
    fn branch_transform_rejects_oversized_output_without_leaking_it() {
        let plugin = loaded_with_branch_transforms(
            "acme.branch-style",
            vec![branch_transform(
                "^short$",
                &"x".repeat(MAX_BRANCH_OUTPUT_BYTES + 1),
            )],
            true,
            true,
        );
        let plan = branch_transform_plan(&[plugin]).unwrap();

        let error = plan.apply("short").unwrap_err();
        assert_eq!(
            error,
            BranchTransformError::OutputTooLong {
                plugin_id: "acme.branch-style".into(),
                rule_index: 0,
            }
        );
        assert!(!error.to_string().contains(&"x".repeat(64)));
    }

    #[test]
    fn branch_transform_bounds_capture_expansion_before_allocation() {
        let plugin = loaded_with_branch_transforms(
            "acme.branch-style",
            vec![branch_transform(
                "^(.+)$",
                &"$1".repeat(MAX_BRANCH_OUTPUT_BYTES),
            )],
            true,
            true,
        );
        let plan = branch_transform_plan(&[plugin]).unwrap();

        assert_eq!(
            plan.apply(&"x".repeat(MAX_BRANCH_OUTPUT_BYTES))
                .unwrap_err(),
            BranchTransformError::OutputTooLong {
                plugin_id: "acme.branch-style".into(),
                rule_index: 0,
            }
        );
    }

    #[test]
    fn branch_transform_rejects_reserved_and_option_like_branch_names() {
        for output in ["HEAD", "-f", "--detach"] {
            let plugin = loaded_with_branch_transforms(
                "acme.branch-style",
                vec![branch_transform("^source$", output)],
                true,
                true,
            );
            let plan = branch_transform_plan(&[plugin]).unwrap();

            assert!(matches!(
                plan.apply("source"),
                Err(BranchTransformError::InvalidOutput { .. })
            ));
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
