//! Update-availability checks for installed external plugins.

use serde::Serialize;

use super::lockfile::Lockfile;
use super::source::PluginSource;

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub id: String,
    pub source: String,
    pub current: String,
    pub available: Option<String>,
    pub needs_update: bool,
    pub error: Option<String>,
}

struct Target {
    id: String,
    source: String,
}

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
            let reference = match source.reference() {
                Some(r) => Some(r.to_string()),
                None => match resolve_latest_release(&source).await {
                    Ok(Some(tag)) => Some(tag),
                    Ok(None) => return status(short(&current_commit), None, None),
                    Err(e) => return err(format!("{e:#}")),
                },
            };
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

async fn resolve_latest_release(source: &PluginSource) -> anyhow::Result<Option<String>> {
    match source {
        PluginSource::Github { owner, repo, .. } => {
            super::fetch::latest_release_tag(owner, repo).await
        }
        PluginSource::Local(_) => Ok(None),
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
