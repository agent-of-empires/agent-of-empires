//! Adds featured-index pins for plugin releases published since the newest pinned one.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use toml_edit::DocumentMut;

const INDEX_PATH: &str = "plugins/featured.toml";

pub fn run() {
    match bump(Path::new(INDEX_PATH)) {
        Ok(0) => println!("Featured pins are current."),
        Ok(added) => println!("Added {added} pin(s) to {INDEX_PATH}."),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
    }
}

/// Pins every release above a plugin's newest pinned version, so an install naming
/// any published `@ref` verifies. Returns how many pins were added.
fn bump(path: &Path) -> Result<usize> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut document: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let plugins = document
        .get_mut("plugins")
        .and_then(|plugins| plugins.as_table_mut())
        .ok_or_else(|| anyhow!("{} has no [plugins] table", path.display()))?;

    let mut added = 0;
    let mut failures = Vec::new();
    let ids: Vec<String> = plugins.iter().map(|(id, _)| id.to_owned()).collect();
    for id in ids {
        let entry = plugins
            .get_mut(&id)
            .and_then(|entry| entry.as_table_mut())
            .ok_or_else(|| anyhow!("featured entry {id} is not a table"))?;
        let Some(repository) = entry
            .get("source")
            .and_then(|source| source.as_str())
            .and_then(|source| source.strip_prefix("gh:"))
        else {
            continue;
        };
        let repository = repository.to_owned();
        let versions = entry
            .get_mut("versions")
            .and_then(|versions| versions.as_inline_table_mut())
            .ok_or_else(|| anyhow!("featured entry {id} has no inline `versions` table"))?;

        let newest = versions
            .iter()
            .filter_map(|(pinned, _)| semver::Version::parse(pinned).ok())
            .max();
        let missing = match remote_releases(&repository) {
            Ok(releases) => releases_to_pin(releases, newest.as_ref()),
            Err(error) => {
                failures.push(format!("{id} ({repository}): {error:#}"));
                continue;
            }
        };

        if let Some((left, right)) = ambiguous_versions(&missing) {
            failures.push(format!(
                "{id} ({repository}): tags {left} and {right} name one version"
            ));
            continue;
        }

        // Oldest first, and stop at the first failure: a newer pin would raise
        // this plugin's newest pinned version past it, so the failed release
        // would never be retried.
        for (version, tag) in missing {
            match tree_hash_at(&repository, &tag) {
                Ok(hash) => {
                    println!("  {id} {version} {hash}");
                    versions.insert(version.to_string(), hash.into());
                    added += 1;
                }
                Err(error) => {
                    failures.push(format!("{id} ({repository} at {tag}): {error:#}"));
                    break;
                }
            }
        }
        // `insert` gives the last value a trailing space the hand-written rows lack.
        for (_, value) in versions.iter_mut() {
            value.decor_mut().set_suffix("");
        }
    }

    if added > 0 {
        std::fs::write(path, document.to_string())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if !failures.is_empty() {
        bail!(
            "could not pin {} release(s):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    Ok(added)
}

/// The releases a plugin still needs pinned: everything above its newest pinned
/// version, oldest first, so a backfill reads in order. Compares versions rather
/// than release dates, because the pin key is the version.
fn releases_to_pin(
    mut releases: Vec<(semver::Version, String)>,
    newest: Option<&semver::Version>,
) -> Vec<(semver::Version, String)> {
    releases.retain(|(version, _)| newest.is_none_or(|newest| version > newest));
    releases.sort_by(|left, right| left.0.cmp(&right.0));
    releases
}

/// Sorted releases can hold one version twice (`v1.2.3` and `1.2.3`) while the
/// tags point at different trees, so that version has no unambiguous pin.
fn ambiguous_versions(releases: &[(semver::Version, String)]) -> Option<(&str, &str)> {
    releases.windows(2).find_map(|pair| match pair {
        [(version, left), (next, right)] if version == next => {
            Some((left.as_str(), right.as_str()))
        }
        _ => None,
    })
}

fn remote_releases(repository: &str) -> Result<Vec<(semver::Version, String)>> {
    let url = repository_url(repository);
    let output = Command::new("git")
        .args([
            "-c",
            "core.autocrlf=false",
            "ls-remote",
            "--tags",
            "--refs",
            "--",
            &url,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("listing tags of {repository} (is git on PATH?)"))?;
    if !output.status.success() {
        bail!(
            "git ls-remote {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut releases = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((_, reference)) = line.split_once('\t') else {
            continue;
        };
        let Some(tag) = reference.strip_prefix("refs/tags/") else {
            continue;
        };
        let Some(version) = release_version(tag) else {
            continue;
        };
        releases.push((version, tag.to_owned()));
    }
    Ok(releases)
}

/// `v1.2.3` and `1.2.3` both name version `1.2.3`; a prerelease is not what an
/// install without a `@ref` resolves, so it is not pinned.
fn release_version(tag: &str) -> Option<semver::Version> {
    let version = semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    version.pre.is_empty().then_some(version)
}

fn tree_hash_at(repository: &str, tag: &str) -> Result<String> {
    let temp = tempfile::tempdir().context("creating a checkout directory")?;
    let tree = temp.path().join("tree");
    let url = repository_url(repository);
    let output = Command::new("git")
        .args([
            "-c",
            "core.autocrlf=false",
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--branch",
            tag,
            "--",
            &url,
        ])
        .arg(&tree)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("cloning {repository} at {tag}"))?;
    if !output.status.success() {
        bail!(
            "git clone {url} at {tag} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    std::fs::remove_dir_all(tree.join(".git")).context("removing the checkout's git metadata")?;
    agent_of_empires::plugin::integrity::tree_hash(&tree)
        .with_context(|| format!("hashing {repository} at {tag}"))
}

fn repository_url(repository: &str) -> String {
    format!("https://github.com/{repository}.git")
}

#[cfg(test)]
mod tests {
    use super::{ambiguous_versions, release_version, releases_to_pin};

    fn version(text: &str) -> semver::Version {
        semver::Version::parse(text).expect("test version parses")
    }

    fn tagged(versions: &[&str]) -> Vec<(semver::Version, String)> {
        versions
            .iter()
            .map(|text| (version(text), format!("v{text}")))
            .collect()
    }

    fn tags(selected: &[(semver::Version, String)]) -> Vec<String> {
        selected.iter().map(|(_, tag)| tag.clone()).collect()
    }

    #[test]
    fn pins_every_release_above_the_newest_pinned_one_oldest_first() {
        let above = releases_to_pin(
            tagged(&["1.0.0", "1.2.0", "1.1.0", "0.9.0"]),
            Some(&version("1.1.0")),
        );
        assert_eq!(tags(&above), vec!["v1.2.0"]);

        let backfill = releases_to_pin(tagged(&["0.2.0", "0.1.0"]), Some(&version("0.0.9")));
        assert_eq!(tags(&backfill), vec!["v0.1.0", "v0.2.0"]);

        let unpinned = releases_to_pin(tagged(&["0.2.0", "0.1.0"]), None);
        assert_eq!(
            tags(&unpinned),
            vec!["v0.1.0", "v0.2.0"],
            "nothing pinned yet"
        );
    }

    #[test]
    fn one_version_with_two_tags_is_reported_rather_than_picked() {
        let releases = vec![
            (version("1.0.0"), "v1.0.0".to_owned()),
            (version("1.1.0"), "1.1.0".to_owned()),
            (version("1.1.0"), "v1.1.0".to_owned()),
        ];
        assert_eq!(ambiguous_versions(&releases), Some(("1.1.0", "v1.1.0")));
        assert_eq!(ambiguous_versions(&releases[..2]), None);
    }

    #[test]
    fn release_tags_are_versioned_with_or_without_a_v_prefix() {
        assert_eq!(release_version("v1.2.3"), Some(version("1.2.3")));
        assert_eq!(release_version("1.2.3"), Some(version("1.2.3")));
        assert_eq!(release_version("v1.2"), None, "not a release version");
        assert_eq!(release_version("nightly"), None);
        assert_eq!(
            release_version("v2.0.0-rc.1"),
            None,
            "an install with no @ref never resolves a prerelease"
        );
    }
}
