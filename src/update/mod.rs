//! Update check functionality.

pub mod install;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tracing::warn;

use crate::session::{get_app_dir, get_update_settings};

const GITHUB_OWNER: &str = "agent-of-empires";
const GITHUB_REPO: &str = "agent-of-empires";

pub const UPDATE_CHECK_INTERVAL_HOURS: u64 = 24;

/// `AOE_UPDATE_API_BASE` overrides the GitHub API base for hermetic tests.
fn github_api_base() -> String {
    std::env::var("AOE_UPDATE_API_BASE")
        .unwrap_or_else(|_| crate::github::DEFAULT_GITHUB_API_BASE.to_string())
}

pub fn release_page_url(version: &str) -> String {
    let tag = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{}", version)
    };
    format!(
        "https://github.com/agent-of-empires/agent-of-empires/releases/tag/{}",
        tag
    )
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub available: bool,
    pub current_version: String,
    pub latest_version: String,
}

/// Semver distance only, never a version string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    Unknown,
    Current,
    PatchBehind,
    MinorBehind,
    MajorBehind,
}

/// Complements `UpdateStatus`: `major_behind` with `one_behind` reveals a thin fallback cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleasesBehind {
    Unknown,
    Current,
    OneBehind,
    SeveralBehind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub version: String,
    pub body: String,
    pub published_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCache {
    checked_at: chrono::DateTime<chrono::Utc>,
    latest_version: String,
    #[serde(default)]
    releases: Vec<ReleaseInfo>,
}

fn cache_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("update_cache.json"))
}

fn load_cache() -> Option<UpdateCache> {
    let path = cache_path().ok()?;
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_cache(cache: &UpdateCache) -> Result<()> {
    let path = cache_path()?;
    let content = serde_json::to_string_pretty(cache)?;
    fs::write(&path, content)?;
    Ok(())
}

#[tracing::instrument(target = "update.fetch", skip_all, fields(current = %current_version, force))]
pub async fn check_for_update(current_version: &str, force: bool) -> Result<UpdateInfo> {
    let settings = get_update_settings();

    if !settings.update_check_mode.is_enabled() {
        return Ok(UpdateInfo {
            available: false,
            current_version: current_version.to_string(),
            latest_version: String::new(),
        });
    }

    if !force {
        if let Some(cache) = load_cache() {
            let age = chrono::Utc::now() - cache.checked_at;
            let max_age = chrono::Duration::hours(UPDATE_CHECK_INTERVAL_HOURS as i64);

            // The user upgraded past the cached latest, so the cache is stale.
            let current_is_newer = is_newer_version(current_version, &cache.latest_version);

            if age < max_age && !current_is_newer {
                tracing::info!(
                    target: "update.cache",
                    age_hours = age.num_hours(),
                    latest = %cache.latest_version,
                    "update cache hit"
                );
                let available = is_newer_version(&cache.latest_version, current_version);
                return Ok(UpdateInfo {
                    available,
                    current_version: current_version.to_string(),
                    latest_version: cache.latest_version,
                });
            }
            tracing::info!(
                target: "update.cache",
                age_hours = age.num_hours(),
                current_is_newer,
                "update cache miss; refetching"
            );
        }
    }

    let client = crate::github::GitHubClient::unauthenticated(crate::github::GitHubClientConfig {
        api_base: github_api_base(),
        user_agent: crate::github::DEFAULT_USER_AGENT.to_string(),
        timeout: std::time::Duration::from_secs(5),
    })?;

    let releases = match fetch_releases(&client).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(target: "update.fetch", "Failed to fetch releases: {e}");
            Vec::new()
        }
    };

    let latest_version = releases
        .first()
        .map(|r| r.version.clone())
        .unwrap_or_default();

    if latest_version.is_empty() {
        let release = client.latest_release(GITHUB_OWNER, GITHUB_REPO).await?;
        let release_info = release_info_from(release);
        let version = release_info.version.clone();

        let cache = UpdateCache {
            checked_at: chrono::Utc::now(),
            latest_version: version.clone(),
            releases: vec![release_info],
        };
        if let Err(e) = save_cache(&cache) {
            warn!("Failed to save update cache: {}", e);
        }

        return Ok(UpdateInfo {
            available: is_newer_version(&version, current_version),
            current_version: current_version.to_string(),
            latest_version: version,
        });
    }

    let cache = UpdateCache {
        checked_at: chrono::Utc::now(),
        latest_version: latest_version.clone(),
        releases,
    };
    if let Err(e) = save_cache(&cache) {
        warn!("Failed to save update cache: {}", e);
    }

    let available = is_newer_version(&latest_version, current_version);
    tracing::info!(
        target: "update.parse",
        current = %current_version,
        latest = %latest_version,
        available,
        "version compared"
    );

    Ok(UpdateInfo {
        available,
        current_version: current_version.to_string(),
        latest_version,
    })
}

#[tracing::instrument(target = "update.fetch", skip_all)]
async fn fetch_releases(client: &crate::github::GitHubClient) -> Result<Vec<ReleaseInfo>> {
    let releases = client.list_releases(GITHUB_OWNER, GITHUB_REPO, 20).await?;
    Ok(releases.into_iter().map(release_info_from).collect())
}

fn release_info_from(release: crate::github::GitHubRelease) -> ReleaseInfo {
    ReleaseInfo {
        version: release.tag_name.trim_start_matches('v').to_string(),
        body: release.body.unwrap_or_default(),
        published_at: release.published_at,
    }
}

pub fn get_cached_releases(from_version: Option<&str>) -> Vec<ReleaseInfo> {
    let cache = match load_cache() {
        Some(c) => c,
        None => return vec![],
    };

    filter_releases(cache.releases, from_version)
}

fn filter_releases(releases: Vec<ReleaseInfo>, from_version: Option<&str>) -> Vec<ReleaseInfo> {
    match from_version {
        Some(from) => releases
            .into_iter()
            .take_while(|r| r.version != from)
            .collect(),
        None => releases,
    }
}

pub(crate) fn is_newer_version(latest: &str, current: &str) -> bool {
    let latest_parts = version_parts(latest);
    let current_parts = version_parts(current);

    for i in 0..latest_parts.len().max(current_parts.len()) {
        let l = latest_parts.get(i).copied().unwrap_or(0);
        let c = current_parts.get(i).copied().unwrap_or(0);
        if l > c {
            return true;
        }
        if l < c {
            return false;
        }
    }
    false
}

fn version_parts(v: &str) -> Vec<u32> {
    v.split('.').filter_map(|s| s.parse().ok()).collect()
}

/// An empty or unparsable latest is `Unknown`, never `Current`.
fn classify_update_status(current: &str, cached_latest: Option<&str>) -> UpdateStatus {
    let Some(latest) = cached_latest.map(str::trim).filter(|s| !s.is_empty()) else {
        return UpdateStatus::Unknown;
    };
    let latest_parts = version_parts(latest);
    if latest_parts.is_empty() {
        return UpdateStatus::Unknown;
    }
    if !is_newer_version(latest, current) {
        return UpdateStatus::Current;
    }
    let current_parts = version_parts(current);
    let part = |parts: &[u32], i: usize| parts.get(i).copied().unwrap_or(0);
    if part(&latest_parts, 0) > part(&current_parts, 0) {
        UpdateStatus::MajorBehind
    } else if part(&latest_parts, 1) > part(&current_parts, 1) {
        UpdateStatus::MinorBehind
    } else {
        UpdateStatus::PatchBehind
    }
}

/// A newer latest missing from the list reports `OneBehind` rather than overstating.
fn classify_releases_behind(
    current: &str,
    cached_latest: Option<&str>,
    releases: &[ReleaseInfo],
) -> ReleasesBehind {
    let Some(latest) = cached_latest.map(str::trim).filter(|s| !s.is_empty()) else {
        return ReleasesBehind::Unknown;
    };
    if version_parts(latest).is_empty() {
        return ReleasesBehind::Unknown;
    }
    if !is_newer_version(latest, current) {
        return ReleasesBehind::Current;
    }
    let newer = releases
        .iter()
        .filter(|r| is_newer_version(&r.version, current))
        .count();
    if newer >= 2 {
        ReleasesBehind::SeveralBehind
    } else {
        ReleasesBehind::OneBehind
    }
}

pub fn cached_version_health(current: &str) -> (UpdateStatus, ReleasesBehind) {
    let cache = load_cache();
    let latest = cache.as_ref().map(|c| c.latest_version.as_str());
    let releases: &[ReleaseInfo] = cache.as_ref().map(|c| c.releases.as_slice()).unwrap_or(&[]);
    (
        classify_update_status(current, latest),
        classify_releases_behind(current, latest, releases),
    )
}

pub async fn print_update_notice() {
    let settings = get_update_settings();
    if !settings.update_check_mode.notifies() {
        return;
    }

    let version = env!("CARGO_PKG_VERSION");

    if let Ok(info) = check_for_update(version, false).await {
        if info.available {
            eprintln!(
                "\n💡 Update available: v{} → v{} (run: aoe update)",
                info.current_version, info.latest_version
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_version_is_strict_so_a_rerun_never_invalidates_the_cache() {
        for (candidate, baseline, newer) in [
            ("1.0.1", "1.0.0", true),
            ("1.1.0", "1.0.9", true),
            ("2.0.0", "1.9.9", true),
            ("0.5.0", "0.4.5", true),
            ("1.0.0", "1.0.0", false),
            ("0.4.5", "0.4.5", false),
            ("1.0.0", "1.0.1", false),
            ("0.4.0", "0.4.5", false),
        ] {
            assert_eq!(
                is_newer_version(candidate, baseline),
                newer,
                "{candidate} vs {baseline}"
            );
        }
    }

    fn make_release(version: &str) -> ReleaseInfo {
        ReleaseInfo {
            version: version.to_string(),
            body: format!("Release notes for {}", version),
            published_at: None,
        }
    }

    #[test]
    fn filter_releases_keeps_only_what_is_newer_than_from_version() {
        let all = ["0.5.0", "0.4.3", "0.4.2", "0.4.1"];
        // (available versions, from_version, versions the caller should see)
        let cases: [(&[&str], Option<&str>, &[&str]); 6] = [
            (&all[..3], None, &["0.5.0", "0.4.3", "0.4.2"]),
            (&all, Some("0.4.3"), &["0.5.0"]),
            (&all[..2], Some("0.5.0"), &[]),
            (&all[..2], Some("0.3.0"), &["0.5.0", "0.4.3"]),
            (&[], Some("0.4.3"), &[]),
            (&[], None, &[]),
        ];
        for (available, from_version, expected) in cases {
            let releases = available.iter().map(|v| make_release(v)).collect();
            let versions: Vec<String> = filter_releases(releases, from_version)
                .into_iter()
                .map(|r| r.version)
                .collect();
            assert_eq!(versions, expected, "{available:?} from {from_version:?}");
        }
    }

    #[test]
    fn classify_update_status_and_releases_behind() {
        use UpdateStatus::*;
        assert_eq!(classify_update_status("1.2.3", None), Unknown);
        assert_eq!(classify_update_status("1.2.3", Some("")), Unknown);
        assert_eq!(classify_update_status("1.2.3", Some("   ")), Unknown);
        assert_eq!(classify_update_status("1.2.3", Some("garbage")), Unknown);
        assert_eq!(classify_update_status("1.2.3", Some("1.2.3")), Current);
        assert_eq!(classify_update_status("1.2.3", Some("1.2.0")), Current);
        assert_eq!(classify_update_status("1.2.3", Some("1.2.4")), PatchBehind);
        assert_eq!(classify_update_status("1.2.3", Some("1.3.0")), MinorBehind);
        assert_eq!(classify_update_status("1.2.3", Some("2.0.0")), MajorBehind);

        let releases = vec![
            make_release("1.3.0"),
            make_release("1.2.5"),
            make_release("1.2.3"),
            make_release("1.2.0"),
        ];
        for (current, latest, cached, expected) in [
            ("1.2.3", None, &[][..], ReleasesBehind::Unknown),
            (
                "1.3.0",
                Some("1.3.0"),
                &releases[..],
                ReleasesBehind::Current,
            ),
            (
                "1.2.3",
                Some("1.3.0"),
                &releases[..],
                ReleasesBehind::SeveralBehind,
            ),
            (
                "1.2.5",
                Some("1.3.0"),
                &releases[..],
                ReleasesBehind::OneBehind,
            ),
            ("1.2.3", Some("9.9.9"), &[][..], ReleasesBehind::OneBehind),
        ] {
            assert_eq!(
                classify_releases_behind(current, latest, cached),
                expected,
                "{current} -> {latest:?}"
            );
        }
    }
}
