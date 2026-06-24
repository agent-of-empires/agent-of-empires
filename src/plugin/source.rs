//! Parsing an external plugin install source.
//!
//! A source is either a GitHub slug (`gh:owner/repo` with an optional `@ref`)
//! or a local directory path. Parsing is pure: it does not touch the network or
//! the filesystem, so the same parser interprets a freshly typed argument and a
//! source string read back from config on update.

use std::path::PathBuf;

use anyhow::{bail, Result};

/// Where a plugin is installed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    /// A GitHub repository, written `gh:owner/repo` with an optional `@ref`
    /// (branch, tag, or commit).
    Github {
        owner: String,
        repo: String,
        reference: Option<String>,
    },
    /// A local directory containing an `aoe-plugin.toml`.
    Local(PathBuf),
}

impl PluginSource {
    /// Parse an install source argument. A `gh:` prefix selects GitHub;
    /// anything else is treated as a local path.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty plugin source");
        }
        if let Some(rest) = input.strip_prefix("gh:") {
            let (slug, reference) = match rest.split_once('@') {
                Some((slug, reference)) => {
                    if reference.is_empty() {
                        bail!("empty ref after '@' in {input:?}");
                    }
                    (slug, Some(reference.to_string()))
                }
                None => (rest, None),
            };
            let (owner, repo) = slug
                .split_once('/')
                .filter(|(o, r)| !o.is_empty() && !r.is_empty() && !r.contains('/'))
                .ok_or_else(|| anyhow::anyhow!("expected gh:owner/repo, got {input:?}"))?;
            Ok(PluginSource::Github {
                owner: owner.to_string(),
                repo: repo.to_string(),
                reference,
            })
        } else {
            Ok(PluginSource::Local(PathBuf::from(input)))
        }
    }

    /// The canonical source string persisted in config and the lockfile. For
    /// GitHub this drops the `@ref` (the ref is recorded separately); for a
    /// local source it is the path.
    pub fn slug(&self) -> String {
        match self {
            PluginSource::Github { owner, repo, .. } => format!("gh:{owner}/{repo}"),
            PluginSource::Local(path) => path.display().to_string(),
        }
    }

    /// The requested git ref, if any. Always `None` for a local source.
    pub fn reference(&self) -> Option<&str> {
        match self {
            PluginSource::Github { reference, .. } => reference.as_deref(),
            PluginSource::Local(_) => None,
        }
    }

    /// The clone URL for a GitHub source. The host base defaults to
    /// `https://github.com` and is overridable via `AOE_GITHUB_CLONE_BASE` (a
    /// GitHub Enterprise host, or a local path/`file://` base in tests).
    pub fn github_clone_url(&self) -> Option<String> {
        match self {
            PluginSource::Github { owner, repo, .. } => {
                let base = std::env::var("AOE_GITHUB_CLONE_BASE")
                    .unwrap_or_else(|_| "https://github.com".to_string());
                let base = base.trim_end_matches('/');
                Some(format!("{base}/{owner}/{repo}.git"))
            }
            PluginSource::Local(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_with_and_without_ref() {
        let s = PluginSource::parse("gh:acme/widget").unwrap();
        assert_eq!(
            s,
            PluginSource::Github {
                owner: "acme".into(),
                repo: "widget".into(),
                reference: None
            }
        );
        assert_eq!(s.slug(), "gh:acme/widget");
        assert_eq!(s.reference(), None);

        let s = PluginSource::parse("gh:acme/widget@v1.2.3").unwrap();
        assert_eq!(s.reference(), Some("v1.2.3"));
        assert_eq!(s.slug(), "gh:acme/widget");
        assert_eq!(
            s.github_clone_url().as_deref(),
            Some("https://github.com/acme/widget.git")
        );
    }

    #[test]
    fn rejects_malformed_github() {
        for bad in [
            "gh:",
            "gh:acme",
            "gh:acme/",
            "gh:/widget",
            "gh:a/b/c",
            "gh:acme/widget@",
        ] {
            assert!(
                PluginSource::parse(bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn treats_non_gh_as_local_path() {
        let s = PluginSource::parse("/tmp/my-plugin").unwrap();
        assert_eq!(s, PluginSource::Local(PathBuf::from("/tmp/my-plugin")));
        assert_eq!(s.reference(), None);
        assert_eq!(s.github_clone_url(), None);
    }
}
