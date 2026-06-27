//! Update-availability checks for installed external plugins.
//!
//! An explicit action (CLI `aoe plugin outdated`, TUI `c`, the dashboard
//! `GET /api/plugins/updates`), never run during the registry's offline load
//! path. For a GitHub source it compares the lockfile's resolved commit against
//! `git ls-remote` of the requested ref (no clone, no REST rate limit); for a
//! local source it re-hashes the source directory against the lockfile tree
//! hash. Builtins have nothing to update and are skipped.
//!
//! Limitation: a `release-binary` plugin whose GitHub release asset is replaced
//! without a source-commit change is not detected here; `ls-remote` only sees
//! the source tree. That asset drift is out of scope for #2365.

use serde::Serialize;

use super::lockfile::Lockfile;
use super::source::PluginSource;

/// One plugin's update status, rendered identically by CLI / TUI / web.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub id: String,
    pub source: String,
    /// The currently installed marker: a short commit (GitHub) or `local`.
    pub current: String,
    /// The newer marker when an update exists: a short commit for GitHub. `None`
    /// for a changed local tree (there is no commit to name) or when current.
    pub available: Option<String>,
    pub needs_update: bool,
    /// Why the check could not run for this plugin (missing lock, git absent,
    /// dead remote). Never silently treated as up-to-date.
    pub error: Option<String>,
}

/// One installed external plugin's identity, pulled off the registry before any
/// blocking work so nothing non-`Send` is held across an await.
struct Target {
    id: String,
    source: String,
}

/// Check every installed external plugin for an available update. Results are
/// sorted by id; per-plugin failures land in `error`, not as a hard error.
pub async fn outdated() -> Vec<UpdateStatus> {
    let targets: Vec<Target> = super::registry()
        .all()
        .iter()
        .filter_map(|p| {
            Some(Target {
                id: p.id().to_string(),
                source: p.source.clone()?,
            })
        })
        .collect();

    let lock = Lockfile::load();
    let mut out = Vec::with_capacity(targets.len());
    for target in targets {
        out.push(check_one(&target, lock.as_ref()).await);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

async fn check_one(target: &Target, lock: Result<&Lockfile, &anyhow::Error>) -> UpdateStatus {
    let status = |current: String, available: Option<String>, error: Option<String>| UpdateStatus {
        id: target.id.clone(),
        source: target.source.clone(),
        needs_update: available.is_some(),
        current,
        available,
        error,
    };
    let err = |msg: String| status(String::new(), None, Some(msg));

    let lock = match lock {
        Ok(lock) => lock,
        // A corrupt or unreadable plugins.lock must surface as itself, not be
        // misreported as a missing entry across every plugin.
        Err(e) => return err(format!("reading plugins.lock: {e:#}")),
    };
    let Some(locked) = lock.get(&target.id) else {
        return err(format!(
            "no lockfile entry for {}; reinstall to record one",
            target.id
        ));
    };

    match PluginSource::parse(&target.source) {
        Ok(source @ PluginSource::Github { .. }) => {
            let Some(url) = source.github_clone_url() else {
                return err("github source without a clone url".to_string());
            };
            let Some(current_commit) = locked.resolved_commit.clone() else {
                return err("lockfile has no resolved commit".to_string());
            };
            let reference = source.reference().map(String::from);
            let remote = tokio::task::spawn_blocking(move || {
                super::fetch::ls_remote(&url, reference.as_deref())
            })
            .await;
            match remote {
                Ok(Ok(remote_commit)) => {
                    let needs = !remote_commit.eq_ignore_ascii_case(&current_commit);
                    status(
                        short(&current_commit),
                        needs.then(|| short(&remote_commit)),
                        None,
                    )
                }
                Ok(Err(e)) => err(format!("{e:#}")),
                Err(e) => err(format!("ls-remote task failed: {e}")),
            }
        }
        Ok(PluginSource::Local(path)) => {
            let pinned = locked.tree_hash.clone();
            let probe = path.clone();
            let rehash =
                tokio::task::spawn_blocking(move || super::integrity::tree_hash(&probe)).await;
            match rehash {
                Ok(Ok(hash)) => {
                    let needs = !pinned.is_empty() && hash != pinned;
                    status(
                        "local".to_string(),
                        needs.then(|| "modified".to_string()),
                        None,
                    )
                }
                Ok(Err(e)) => err(format!("re-hashing {}: {e:#}", path.display())),
                Err(e) => err(format!("hash task failed: {e}")),
            }
        }
        Err(e) => err(format!("unparseable source {:?}: {e:#}", target.source)),
    }
}

fn short(commit: &str) -> String {
    commit.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_truncates() {
        assert_eq!(short("abcdef0123456789"), "abcdef01");
        assert_eq!(short("abc"), "abc");
    }
}
