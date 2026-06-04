//! Update check functionality

pub mod install;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tracing::warn;

use crate::session::{get_app_dir, get_update_settings};

const GITHUB_OWNER: &str = "agent-of-empires";
const GITHUB_REPO: &str = "agent-of-empires";

/// Resolve the GitHub API base URL, honoring `AOE_UPDATE_API_BASE` for
/// hermetic tests. The override mirrors `AOE_UPDATE_BASE_URL` (which
/// covers tarball downloads); tests that need to exercise the CLI
/// without rate-limiting GitHub set both.
fn github_api_base() -> String {
    std::env::var("AOE_UPDATE_API_BASE")
        .unwrap_or_else(|_| crate::github::DEFAULT_GITHUB_API_BASE.to_string())
}

/// Public release-page URL for a given version tag. Stable enough to
/// hardcode (GitHub redirects from `/releases/tag/vX.Y.Z` even when the
/// release is later edited). Used by the web update banner. See #984.
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

/// Coarse update-staleness signal by semver distance, derived from the cached
/// update check. Carries no raw version string, only the magnitude of the gap,
/// so telemetry can answer "are installs on patched/recent versions" without
/// leaking which version anyone runs. See `crate::telemetry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    /// No usable cache (never checked, or a malformed/unparsable cached latest).
    Unknown,
    /// The cached latest is not newer than the running build.
    Current,
    /// Behind by a patch only (major and minor match).
    PatchBehind,
    /// Behind by a minor (major matches).
    MinorBehind,
    /// Behind by a major.
    MajorBehind,
}

/// Coarse "how many releases behind" signal, counted from the cached release
/// list. Complements [`UpdateStatus`]: a `major_behind` + `one_behind` pair
/// reveals a thin/fallback cache that only fetched the latest release, while
/// `minor_behind` + `several_behind` is a genuinely lagging install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleasesBehind {
    /// No cache to count from.
    Unknown,
    /// On the cached latest.
    Current,
    /// Exactly one cached release is newer (or the cache only knows the latest).
    OneBehind,
    /// Two or more cached releases are newer.
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

    // Mode=off skips network entirely; return a "no update" stub so callers
    // can keep their unconditional shape without branching on the mode.
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
            let max_age = chrono::Duration::hours(settings.check_interval_hours as i64);

            // Invalidate cache if current version is newer than cached latest
            // (user upgraded and cache is stale)
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

    // Fetch all releases (includes body/release notes)
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
        // Fall back to the latest-release endpoint if the releases list failed.
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

/// Get cached release notes, filtered to show only releases newer than from_version.
/// Returns releases in newest-first order.
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

/// Classify the semver distance between the running build and a cached latest.
/// Pure (no I/O) so the bucketing is unit-testable without app-dir/env coupling.
/// An empty or unparsable cached latest is [`UpdateStatus::Unknown`], never
/// silently treated as `Current`.
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

/// Classify how many cached releases are newer than the running build. Pure (no
/// I/O). When the cached latest is newer but the release list does not enumerate
/// it (an old or fallback cache that only stored the latest), reports the
/// conservative [`ReleasesBehind::OneBehind`] rather than overstating the depth.
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

/// Read the cached update check (no network) and classify both version-health
/// signals in one pass. Returns [`UpdateStatus::Unknown`] / [`ReleasesBehind::Unknown`]
/// when no cache exists. Used by telemetry; the opt-in gate is the caller's job.
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
    // CLI nag fires only when both the global mode allows notifications
    // and the user has not opted out of CLI nags specifically.
    if !settings.update_check_mode.notifies() || !settings.notify_in_cli {
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
    fn test_version_comparison() {
        assert!(is_newer_version("1.0.1", "1.0.0"));
        assert!(is_newer_version("1.1.0", "1.0.9"));
        assert!(is_newer_version("2.0.0", "1.9.9"));
        assert!(!is_newer_version("1.0.0", "1.0.0"));
        assert!(!is_newer_version("1.0.0", "1.0.1"));
    }

    #[test]
    fn test_cache_should_invalidate_when_current_newer_than_cached() {
        // When user upgrades to a version newer than cached latest,
        // the cache should be invalidated to fetch fresh release notes.
        // This test documents the version comparison used for cache invalidation.
        let cached_latest = "0.4.5";
        let current_version = "0.5.0";

        // current > cached means cache is stale
        let current_is_newer = is_newer_version(current_version, cached_latest);
        assert!(current_is_newer, "0.5.0 should be newer than 0.4.5");

        // Same version means cache is valid
        let same_version = is_newer_version("0.4.5", "0.4.5");
        assert!(
            !same_version,
            "same version should not trigger invalidation"
        );

        // Older current version (downgrade) should not invalidate
        let downgrade = is_newer_version("0.4.0", "0.4.5");
        assert!(!downgrade, "downgrade should not trigger invalidation");
    }

    fn make_release(version: &str) -> ReleaseInfo {
        ReleaseInfo {
            version: version.to_string(),
            body: format!("Release notes for {}", version),
            published_at: None,
        }
    }

    #[test]
    fn test_filter_releases_returns_all_when_no_filter() {
        let releases = vec![
            make_release("0.5.0"),
            make_release("0.4.3"),
            make_release("0.4.2"),
        ];

        let filtered = filter_releases(releases.clone(), None);

        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].version, "0.5.0");
        assert_eq!(filtered[1].version, "0.4.3");
        assert_eq!(filtered[2].version, "0.4.2");
    }

    #[test]
    fn test_filter_releases_stops_at_from_version() {
        let releases = vec![
            make_release("0.5.0"),
            make_release("0.4.3"),
            make_release("0.4.2"),
            make_release("0.4.1"),
        ];

        let filtered = filter_releases(releases, Some("0.4.3"));

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].version, "0.5.0");
    }

    #[test]
    fn test_filter_releases_returns_empty_when_from_version_is_latest() {
        let releases = vec![make_release("0.5.0"), make_release("0.4.3")];

        let filtered = filter_releases(releases, Some("0.5.0"));

        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_releases_returns_all_when_from_version_not_found() {
        let releases = vec![make_release("0.5.0"), make_release("0.4.3")];

        let filtered = filter_releases(releases.clone(), Some("0.3.0"));

        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_classify_update_status_buckets_by_semver_distance() {
        use UpdateStatus::*;
        // No cache / empty / unparsable latest => Unknown, never Current.
        assert_eq!(classify_update_status("1.2.3", None), Unknown);
        assert_eq!(classify_update_status("1.2.3", Some("")), Unknown);
        assert_eq!(classify_update_status("1.2.3", Some("   ")), Unknown);
        assert_eq!(classify_update_status("1.2.3", Some("garbage")), Unknown);
        // Latest not newer => Current.
        assert_eq!(classify_update_status("1.2.3", Some("1.2.3")), Current);
        assert_eq!(classify_update_status("1.2.3", Some("1.2.0")), Current);
        // Distance buckets.
        assert_eq!(classify_update_status("1.2.3", Some("1.2.4")), PatchBehind);
        assert_eq!(classify_update_status("1.2.3", Some("1.3.0")), MinorBehind);
        assert_eq!(classify_update_status("1.2.3", Some("2.0.0")), MajorBehind);
    }

    #[test]
    fn test_classify_releases_behind_counts_cached_releases() {
        use ReleasesBehind::*;
        let releases = vec![
            make_release("1.3.0"),
            make_release("1.2.5"),
            make_release("1.2.3"),
            make_release("1.2.0"),
        ];
        // No cache => Unknown.
        assert_eq!(classify_releases_behind("1.2.3", None, &[]), Unknown);
        // Latest not newer => Current.
        assert_eq!(
            classify_releases_behind("1.3.0", Some("1.3.0"), &releases),
            Current
        );
        // Two cached releases newer than 1.2.3 (1.3.0, 1.2.5) => SeveralBehind.
        assert_eq!(
            classify_releases_behind("1.2.3", Some("1.3.0"), &releases),
            SeveralBehind
        );
        // Exactly one newer => OneBehind.
        assert_eq!(
            classify_releases_behind("1.2.5", Some("1.3.0"), &releases),
            OneBehind
        );
        // Thin/fallback cache: latest newer but list does not enumerate it =>
        // conservative OneBehind, not overstated.
        assert_eq!(
            classify_releases_behind("1.2.3", Some("9.9.9"), &[]),
            OneBehind
        );
    }

    #[test]
    fn test_filter_releases_handles_empty_list() {
        let releases: Vec<ReleaseInfo> = vec![];

        let filtered = filter_releases(releases.clone(), Some("0.4.3"));
        assert!(filtered.is_empty());

        let filtered = filter_releases(releases, None);
        assert!(filtered.is_empty());
    }
}
