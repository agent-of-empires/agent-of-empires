// Path template system for worktrees

use std::path::{Component, Path, PathBuf};

use super::error::{GitError, Result};

pub struct TemplateVars {
    pub repo_name: String,
    pub branch: String,
    pub session_id: String,
    pub base_path: PathBuf,
}

pub fn sanitize_branch_name(branch: &str) -> String {
    branch.replace(
        ['/', '@', '#', '\\', ':', '*', '?', '"', '<', '>', '|'],
        "-",
    )
}

/// Lexically resolve `.` and `..` components without touching the filesystem
/// (the worktree does not exist yet at resolve time).
///
/// The default template (`../{repo-name}-worktrees/{branch}`) otherwise stores
/// paths like `/repos/my-repo/../my-repo-worktrees/feat` verbatim in
/// `sessions.json` as the session's `project_path`. Every site that treats
/// `project_path` as an identity — raw string comparison against a peer's
/// path, `~/.claude/projects/<encoded-cwd>` encoding of a since-deleted dir
/// that `fs::canonicalize` can no longer resolve — then sees a different
/// spelling of the same directory and misattributes session ids (#2858).
///
/// A `..` with no concrete parent to pop (relative base that begins with
/// `..`) is preserved rather than dropped.
///
/// Also used by `session::capture::canonicalize_or_raw` as the fallback when
/// `fs::canonicalize` fails (deleted directory): sessions created before the
/// normalization here may still carry a `..` spelling in `sessions.json`, and
/// identity comparisons must not depend on the directory still existing.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                match out.components().next_back() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    // `/..` is `/`; drop the component.
                    Some(Component::RootDir) => {}
                    // Nothing concrete to pop; keep the `..`.
                    _ => out.push(Component::ParentDir),
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Unset, empty or relative `$XDG_*` variables fall back to these, relative to
/// home, per the XDG Base Directory spec.
const XDG_DEFAULTS: [(&str, &str); 4] = [
    ("XDG_CACHE_HOME", ".cache"),
    ("XDG_CONFIG_HOME", ".config"),
    ("XDG_DATA_HOME", ".local/share"),
    ("XDG_STATE_HOME", ".local/state"),
];

fn is_env_name(name: &str) -> bool {
    !name.starts_with(|c: char| c.is_ascii_digit())
        && !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Expand a leading `~`, `$VAR` / `${VAR}` references and the `placeholders`
/// (`("{branch}", value)` pairs) in one pass, so expanded values are never
/// rescanned and a branch named `$HOME` stays literal.
///
/// An unset or empty variable is an error: substituting nothing would turn
/// `$VAR/wt` into the absolute path `/wt`. `$$` is a literal `$`, as is a `$`
/// that does not start a valid name.
fn expand_env(
    template: &str,
    home: Option<&Path>,
    placeholders: &[(&str, &str)],
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let need_home = |what: &str| {
        home.ok_or_else(|| {
            GitError::InvalidTemplate(format!(
                "{what} in template `{template}` needs a home directory, which could not be determined"
            ))
        })
    };

    let mut out = String::new();
    let mut rest = template;
    if template == "~" || template.starts_with("~/") {
        out.push_str(&need_home("`~`")?.to_string_lossy());
        rest = &template[1..];
    }

    while let Some(i) = rest.find(['$', '{']) {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if rest[i..].starts_with('{') {
            match placeholders.iter().find(|(p, _)| rest[i..].starts_with(p)) {
                Some((p, value)) => {
                    out.push_str(value);
                    rest = &rest[i + p.len()..];
                }
                None => {
                    out.push('{');
                    rest = after;
                }
            }
            continue;
        }
        if let Some(escaped) = after.strip_prefix('$') {
            out.push('$');
            rest = escaped;
            continue;
        }
        let (name, remainder, braced) = match after.strip_prefix('{') {
            Some(inner) => {
                let end = inner.find('}').ok_or_else(|| {
                    GitError::InvalidTemplate(format!(
                        "unterminated `${{` in template `{template}`"
                    ))
                })?;
                (&inner[..end], &inner[end + 1..], true)
            }
            None => {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..end], &after[end..], false)
            }
        };
        if !is_env_name(name) {
            if braced {
                return Err(GitError::InvalidTemplate(format!(
                    "invalid variable name `${{{name}}}` in template `{template}`"
                )));
            }
            out.push('$');
            rest = after;
            continue;
        }
        let xdg_default = XDG_DEFAULTS.iter().find(|(var, _)| *var == name);
        let value = lookup(name)
            .filter(|v| !v.is_empty() && (xdg_default.is_none() || Path::new(v).is_absolute()));
        match value {
            Some(value) => out.push_str(&value),
            None => match xdg_default {
                Some((_, default)) => {
                    out.push_str(
                        &need_home(&format!("`${name}`"))?
                            .join(default)
                            .to_string_lossy(),
                    );
                }
                None => {
                    return Err(GitError::InvalidTemplate(format!(
                        "environment variable `{name}` in template `{template}` is unset or empty"
                    )));
                }
            },
        }
        rest = remainder;
    }
    out.push_str(rest);
    Ok(out)
}

/// Resolve a worktree path template to an absolute, normalized path.
///
/// `~` and `$VAR` / `${VAR}` are expanded together with the `{repo-name}` /
/// `{branch}` / `{session-id}` placeholders before the absolute check, so
/// `$XDG_CACHE_HOME/...` is seen as absolute rather than joined onto `base_path`.
pub fn resolve_template(template: &str, vars: &TemplateVars) -> Result<PathBuf> {
    resolve_template_with(template, vars, dirs::home_dir().as_deref(), |name| {
        std::env::var(name).ok()
    })
}

fn resolve_template_with(
    template: &str,
    vars: &TemplateVars,
    home: Option<&Path>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf> {
    let sanitized_branch = sanitize_branch_name(&vars.branch);

    let placeholders = [
        ("{repo-name}", vars.repo_name.as_str()),
        ("{branch}", sanitized_branch.as_str()),
        ("{session-id}", vars.session_id.as_str()),
    ];
    let resolved = expand_env(template, home, &placeholders, lookup)?;

    let path = if resolved.starts_with('/') {
        PathBuf::from(resolved)
    } else {
        vars.base_path.join(&resolved)
    };
    let path = lexical_normalize(&path);

    tracing::debug!(
        target: "git.template",
        template = %template,
        repo = %vars.repo_name,
        branch = %vars.branch,
        session_id = %vars.session_id,
        resolved = %path.display(),
        "worktree template resolved",
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_template_with_repo_name() {
        let vars = TemplateVars {
            repo_name: "my-repo".to_string(),
            branch: "feat/test".to_string(),
            session_id: "abc123".to_string(),
            base_path: PathBuf::from("/home/user/repos/my-repo"),
        };

        let result = resolve_template("../{repo-name}-wt/{branch}", &vars).unwrap();
        assert!(result.to_string_lossy().contains("my-repo-wt"));
        assert!(result.to_string_lossy().contains("feat-test"));
    }

    #[test]
    fn test_sanitize_branch_name_replaces_path_and_shell_characters() {
        for (branch, expected) in [
            ("feat/my-feature", "feat-my-feature"),
            ("feat@bug#123", "feat-bug-123"),
            ("a\\b:c*d?e\"f<g>h|i", "a-b-c-d-e-f-g-h-i"),
        ] {
            assert_eq!(sanitize_branch_name(branch), expected);
        }
    }

    #[test]
    fn test_resolve_template_normalizes_parent_components() {
        let vars = TemplateVars {
            repo_name: "my-repo".to_string(),
            branch: "feat/test".to_string(),
            session_id: "abc123".to_string(),
            base_path: PathBuf::from("/home/user/repos/my-repo"),
        };

        let result = resolve_template("../{repo-name}-worktrees/{branch}", &vars).unwrap();
        assert_eq!(
            result,
            PathBuf::from("/home/user/repos/my-repo-worktrees/feat-test"),
            "resolved path must not retain a literal `..` component"
        );
        assert!(
            !result.components().any(|c| c == Component::ParentDir),
            "got: {}",
            result.display()
        );
    }

    #[test]
    fn test_lexical_normalize_preserves_unresolvable_parent() {
        assert_eq!(
            lexical_normalize(Path::new("../sibling/x")),
            PathBuf::from("../sibling/x")
        );
        assert_eq!(lexical_normalize(Path::new("/../x")), PathBuf::from("/x"));
        assert_eq!(
            lexical_normalize(Path::new("/a/b/./../c")),
            PathBuf::from("/a/c")
        );
    }

    fn env(name: &str) -> Option<String> {
        match name {
            "CACHE_ROOT" => Some("/cache".to_string()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn test_expand_env_substitutes_tilde_and_variables() {
        let home = Some(Path::new("/home/me"));
        for (template, expected) in [
            ("$CACHE_ROOT/wt", "/cache/wt"),
            ("${CACHE_ROOT}x/wt", "/cachex/wt"),
            ("$CACHE_ROOT-wt", "/cache-wt"),
            ("~/wt", "/home/me/wt"),
            ("~", "/home/me"),
            ("~/$CACHE_ROOT", "/home/me//cache"),
            ("../{branch}", "../{branch}"),
            ("a/~/b", "a/~/b"),
            ("~other/wt", "~other/wt"),
            ("cost$", "cost$"),
            ("$1/$ /$-x", "$1/$ /$-x"),
        ] {
            assert_eq!(
                expand_env(template, home, &[], env).unwrap(),
                expected,
                "{template}"
            );
        }
    }

    #[test]
    fn test_expand_env_falls_back_to_xdg_defaults() {
        let home = Some(Path::new("/home/me"));
        for (template, expected) in [
            ("$XDG_CACHE_HOME/wt", "/home/me/.cache/wt"),
            ("${XDG_CONFIG_HOME}/wt", "/home/me/.config/wt"),
            ("$XDG_DATA_HOME/wt", "/home/me/.local/share/wt"),
            ("$XDG_STATE_HOME/wt", "/home/me/.local/state/wt"),
        ] {
            assert_eq!(
                expand_env(template, home, &[], env).unwrap(),
                expected,
                "{template}"
            );
        }
        let set = |name: &str| (name == "XDG_CACHE_HOME").then(|| "/fast".to_string());
        assert_eq!(
            expand_env("$XDG_CACHE_HOME/wt", home, &[], set).unwrap(),
            "/fast/wt"
        );
        for ignored in ["", "cache"] {
            let lookup = |name: &str| (name == "XDG_CACHE_HOME").then(|| ignored.to_string());
            assert_eq!(
                expand_env("$XDG_CACHE_HOME/wt", home, &[], lookup).unwrap(),
                "/home/me/.cache/wt",
                "{ignored:?}"
            );
        }
    }

    #[test]
    fn test_expand_env_escapes_dollar_and_never_rescans_values() {
        let home = Some(Path::new("/home/me"));
        let placeholders = [("{branch}", "$CACHE_ROOT"), ("{repo-name}", "r")];
        let lookup = |name: &str| (name == "TRICKY").then(|| "{repo-name}".to_string());
        for (template, expected) in [
            ("price-$$USD/{branch}", "price-$USD/$CACHE_ROOT"),
            ("$TRICKY/{repo-name}/{unknown}", "{repo-name}/r/{unknown}"),
        ] {
            assert_eq!(
                expand_env(template, home, &placeholders, lookup).unwrap(),
                expected,
                "{template}"
            );
        }
    }

    #[test]
    fn test_expand_env_rejects_unresolvable_references() {
        let home = Some(Path::new("/home/me"));
        for (template, home, needle) in [
            ("$MISSING/wt", home, "MISSING"),
            ("${MISSING}/wt", home, "MISSING"),
            ("$EMPTY/wt", home, "EMPTY"),
            ("${CACHE_ROOT/wt", home, "unterminated"),
            ("${}/wt", home, "invalid"),
            ("${A-B}/wt", home, "A-B"),
            ("~/wt", None, "home"),
            ("$XDG_CACHE_HOME/wt", None, "home"),
        ] {
            let err = expand_env(template, home, &[], env)
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{template}: {err}");
        }
    }

    #[test]
    fn test_resolve_template_expands_env_before_absolute_check() {
        let vars = TemplateVars {
            repo_name: "my-repo".to_string(),
            branch: "feat/test".to_string(),
            session_id: "abc123".to_string(),
            base_path: PathBuf::from("/home/user/repos/my-repo"),
        };
        let home = Path::new("/home/test");
        let lookup = |name: &str| (name == "HOME").then(|| "/home/test".to_string());

        let result =
            resolve_template_with("~/wt/{repo-name}/{branch}", &vars, Some(home), lookup).unwrap();
        assert_eq!(result, PathBuf::from("/home/test/wt/my-repo/feat-test"));

        let result =
            resolve_template_with("$HOME/../x/{branch}", &vars, Some(home), lookup).unwrap();
        assert_eq!(result, PathBuf::from("/home/x/feat-test"));
    }

    #[test]
    fn test_resolve_template_does_not_expand_env_in_branch() {
        let vars = TemplateVars {
            repo_name: "my-repo".to_string(),
            branch: "$HOME".to_string(),
            session_id: "abc123".to_string(),
            base_path: PathBuf::from("/repos/my-repo"),
        };
        let result = resolve_template("../wt/{branch}", &vars).unwrap();
        assert_eq!(result, PathBuf::from("/repos/wt/$HOME"));
    }

    #[test]
    fn test_resolve_template_errors_on_unset_variable() {
        let vars = TemplateVars {
            repo_name: "r".to_string(),
            branch: "b".to_string(),
            session_id: "s".to_string(),
            base_path: PathBuf::from("/repos/r"),
        };
        let err = resolve_template_with(
            "$UNSET_VAR/{branch}",
            &vars,
            Some(Path::new("/home/test")),
            |_| None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("UNSET_VAR"));
    }

    #[test]
    fn test_resolve_template_with_all_variables() {
        let vars = TemplateVars {
            repo_name: "test".to_string(),
            branch: "main".to_string(),
            session_id: "xyz789".to_string(),
            base_path: PathBuf::from("/repos/test"),
        };

        let result = resolve_template("../wt/{repo-name}/{branch}/{session-id}", &vars).unwrap();

        assert!(result.to_string_lossy().contains("test"));
        assert!(result.to_string_lossy().contains("main"));
        assert!(result.to_string_lossy().contains("xyz789"));
    }
}
