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
    format!(
        "https://github.com/agent-of-empires/agent-of-empires/releases/tag/{}",
        release_tag(version)
    )
}

const NIGHTLY_TAG_PREFIX: &str = "nightly-";

/// Nightly builds are versioned `X.Y.Z-nightly.YYYYMMDD` and tagged `nightly-YYYYMMDD`,
/// outside `v*` so release tooling never mistakes one for a stable release.
pub fn release_tag(version: &str) -> String {
    let version = version.trim_start_matches('v');
    match version.split_once("-nightly.") {
        Some((_, date)) => format!("{NIGHTLY_TAG_PREFIX}{date}"),
        None => format!("v{version}"),
    }
}

fn is_nightly(version: &str) -> bool {
    version.contains("-nightly.")
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

/// A cache written by a build on the other channel describes the wrong releases.
fn load_cache_for(current_version: &str) -> Option<UpdateCache> {
    load_cache().filter(|cache| same_channel(&cache.latest_version, current_version))
}

fn same_channel(a: &str, b: &str) -> bool {
    is_nightly(a) == is_nightly(b)
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
        if let Some(cache) = load_cache_for(current_version) {
            let age = chrono::Utc::now() - cache.checked_at;
            let max_age = chrono::Duration::hours(UPDATE_CHECK_INTERVAL_HOURS as i64);

            // The user upgraded past the cached latest, so the cache is stale.
            let current_is_newer = is_newer_version(current_version, &cache.latest_version);

            if age < max_age && !current_is_newer {
                tracing::debug!(
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

    let releases = match fetch_releases(&client, current_version).await {
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

    // `releases/latest` never returns a prerelease, so it cannot stand in for a nightly.
    if latest_version.is_empty() && !is_nightly(current_version) {
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
async fn fetch_releases(
    client: &crate::github::GitHubClient,
    current_version: &str,
) -> Result<Vec<ReleaseInfo>> {
    let releases = client.list_releases(GITHUB_OWNER, GITHUB_REPO, 20).await?;
    Ok(channel_releases(releases, current_version))
}

/// Stable builds only see stable releases, nightly builds only nightlies.
fn channel_releases(
    releases: Vec<crate::github::GitHubRelease>,
    current_version: &str,
) -> Vec<ReleaseInfo> {
    let nightly = is_nightly(current_version);
    releases
        .into_iter()
        .filter(|r| {
            !r.draft
                && r.prerelease == nightly
                && r.tag_name.starts_with(NIGHTLY_TAG_PREFIX) == nightly
        })
        .map(release_info_from)
        .collect()
}

fn release_info_from(release: crate::github::GitHubRelease) -> ReleaseInfo {
    // A nightly tag only carries its date, so the version comes from the release name.
    let version = if release.tag_name.starts_with(NIGHTLY_TAG_PREFIX) {
        release
            .name
            .as_deref()
            .and_then(|name| name.split_whitespace().last())
            .unwrap_or_default()
    } else {
        release.tag_name.as_str()
    };
    ReleaseInfo {
        version: version.trim_start_matches('v').to_string(),
        body: release.body.unwrap_or_default(),
        published_at: release.published_at,
    }
}

pub fn get_cached_releases(from_version: Option<&str>) -> Vec<ReleaseInfo> {
    let cache = match load_cache_for(env!("CARGO_PKG_VERSION")) {
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
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest.cmp_precedence(&current).is_gt(),
        _ => false,
    }
}

fn parse_version(v: &str) -> Option<semver::Version> {
    semver::Version::parse(v.trim().trim_start_matches('v')).ok()
}

/// An empty or unparsable latest is `Unknown`, never `Current`.
fn classify_update_status(current: &str, cached_latest: Option<&str>) -> UpdateStatus {
    let Some(latest) = cached_latest.and_then(parse_version) else {
        return UpdateStatus::Unknown;
    };
    let Some(current) = parse_version(current) else {
        return UpdateStatus::Unknown;
    };
    if latest.cmp_precedence(&current).is_le() {
        return UpdateStatus::Current;
    }
    if latest.major > current.major {
        UpdateStatus::MajorBehind
    } else if latest.minor > current.minor {
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
    let Some(latest) = cached_latest.filter(|v| parse_version(v).is_some()) else {
        return ReleasesBehind::Unknown;
    };
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
    let cache = load_cache_for(current);
    let latest = cache.as_ref().map(|c| c.latest_version.as_str());
    let releases: &[ReleaseInfo] = cache.as_ref().map(|c| c.releases.as_slice()).unwrap_or(&[]);
    (
        classify_update_status(current, latest),
        classify_releases_behind(current, latest, releases),
    )
}

/// The notice a command appends after its output, or `None` when the check is
/// off, fails, or nothing is newer. Returned rather than printed so a caller
/// that owns its own streams can place it after them.
pub async fn update_notice() -> Option<String> {
    if !get_update_settings().update_check_mode.notifies() {
        return None;
    }
    let info = check_for_update(env!("CARGO_PKG_VERSION"), false)
        .await
        .ok()?;
    info.available.then(|| {
        format!(
            "\n💡 Update available: v{} → v{} (run: aoe update)\n",
            info.current_version, info.latest_version
        )
    })
}

pub async fn print_update_notice() {
    if let Some(notice) = update_notice().await {
        eprint!("{notice}");
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
            ("1.22.2-nightly.20261108", "1.22.1", true),
            ("1.22.2-nightly.20261109", "1.22.2-nightly.20261108", true),
            ("1.22.2", "1.22.2-nightly.20261108", true),
            ("1.22.2-nightly.20261108", "1.22.2", false),
        ] {
            assert_eq!(
                is_newer_version(candidate, baseline),
                newer,
                "{candidate} vs {baseline}"
            );
        }
    }

    #[test]
    fn nightly_versions_map_to_dated_tags() {
        for (version, tag) in [
            ("1.22.1", "v1.22.1"),
            ("v1.22.1", "v1.22.1"),
            ("1.22.2-nightly.20261108", "nightly-20261108"),
        ] {
            assert_eq!(release_tag(version), tag, "{version}");
        }
    }

    #[test]
    fn caches_from_the_other_channel_are_ignored() {
        for (cached, current, usable) in [
            ("1.22.1", "1.22.0", true),
            ("1.22.2-nightly.20261108", "1.22.2-nightly.20261107", true),
            ("1.22.2-nightly.20261108", "1.22.1", false),
            ("1.22.1", "1.22.2-nightly.20261107", false),
        ] {
            assert_eq!(
                same_channel(cached, current),
                usable,
                "{cached} for {current}"
            );
        }
    }

    #[test]
    fn each_channel_only_sees_its_own_releases() {
        let gh =
            |tag: &str, name: &str, prerelease: bool, draft: bool| crate::github::GitHubRelease {
                tag_name: tag.to_string(),
                name: Some(name.to_string()),
                body: None,
                published_at: None,
                draft,
                prerelease,
                assets: vec![],
            };
        let releases = || {
            vec![
                gh(
                    "nightly-20261109",
                    "Agent of Empires v1.22.2-nightly.20261109",
                    true,
                    false,
                ),
                gh("v1.22.1", "Agent of Empires v1.22.1", false, false),
                gh(
                    "nightly-20261108",
                    "Agent of Empires v1.22.2-nightly.20261108",
                    true,
                    false,
                ),
                gh("v1.23.0-rc.1", "rc", true, false),
                gh("v1.23.0", "draft", false, true),
            ]
        };
        for (current, expected) in [
            ("1.22.0", &["1.22.1"][..]),
            (
                "1.22.2-nightly.20261107",
                &["1.22.2-nightly.20261109", "1.22.2-nightly.20261108"][..],
            ),
        ] {
            let versions: Vec<String> = channel_releases(releases(), current)
                .into_iter()
                .map(|r| r.version)
                .collect();
            assert_eq!(versions, expected, "{current}");
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
        assert_eq!(
            classify_update_status("1.2.3", Some("1.2.4-nightly.20261108")),
            PatchBehind
        );

        let releases = [
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
