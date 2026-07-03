//! GitHub plugin discovery over the `aoe-plugin` topic.
//!
//! Discovery is an explicit action (CLI `aoe plugin discover`, TUI `d`, the
//! dashboard "Search GitHub" button), never a background task. It is repo-level,
//! not manifest-level: it runs one GitHub search and badges each result by
//! matching the repo slug against the featured index and the installed set. It
//! deliberately does NOT clone or read each repo's `aoe-plugin.toml` (an N+1
//! network blowup that would burn the unauthenticated search rate limit), so a
//! result is "a GitHub repository tagged `aoe-plugin`", not "a verified plugin".
//! Install remains the trust boundary: it fetches the manifest, prompts for
//! capabilities, and enforces the featured pin (`install::install`).

use std::time::Duration;

use anyhow::{bail, Result};
use aoe_plugin_api::{screenshot_path_ok, MAX_SCREENSHOTS};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};

use crate::github::{GitHubClient, GitHubClientConfig, GitHubRepo, DEFAULT_USER_AGENT};

/// Characters to percent-encode in a `raw.githubusercontent.com` path while
/// keeping `/` (segment separators) and the unreserved set intact. The path is
/// already structurally validated by [`screenshot_path_ok`]; this guards the
/// remaining URL-unsafe bytes (spaces, `?`, `#`, `%`, ...).
const RAW_PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'^')
    .add(b'\\')
    .add(b'[')
    .add(b']');

use super::featured::FeaturedIndex;
use super::source::PluginSource;

/// The GitHub topic plugins are published under.
const PLUGIN_TOPIC: &str = "aoe-plugin";

/// How a discovered repository relates to what the host already knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryBadge {
    /// This source slug is already installed.
    Installed,
    /// This source slug is pinned in the featured index (a curated source; not a
    /// claim that the current tree matches the pin).
    Featured,
    /// A GitHub repo tagged `aoe-plugin` that is neither installed nor featured.
    Unvetted,
}

impl DiscoveryBadge {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscoveryBadge::Installed => "installed",
            DiscoveryBadge::Featured => "featured",
            DiscoveryBadge::Unvetted => "unvetted",
        }
    }
}

/// One discovery result, repo-level and ready to render on any surface.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryResult {
    /// `gh:owner/repo`, the slug `aoe plugin install` accepts.
    pub slug: String,
    pub html_url: String,
    pub description: Option<String>,
    pub stars: u64,
    pub badge: DiscoveryBadge,
    /// Whether this source is in the featured index, tracked independently of
    /// `badge`: an installed-and-featured repo shows the `Installed` badge but
    /// must still rank as featured (the badge is one-of, ranking is not).
    pub featured: bool,
    /// The exact `aoe plugin install` command for this plugin, shown alongside
    /// the in-app Install button for users who prefer the terminal.
    pub install_command: String,
}

/// Search the `aoe-plugin` topic and badge each result. `query` is an optional
/// free-text term ANDed with the topic filter.
pub async fn discover(query: Option<&str>) -> Result<Vec<DiscoveryResult>> {
    let client = client()?;

    let mut q = format!("topic:{PLUGIN_TOPIC} fork:false archived:false");
    if let Some(term) = query.map(str::trim).filter(|t| !t.is_empty()) {
        q.push(' ');
        q.push_str(term);
    }
    let repos = client.search_repositories(&q, 30).await?;

    // Treat a featured-index load failure as fatal, matching install-time
    // `verify_featured`: silently defaulting to an empty index would re-badge
    // every curated plugin as unvetted and drop featured-first ordering, so
    // discovery and install would disagree about the same trust signal.
    let featured = FeaturedIndex::load()?;
    let installed = installed_slugs();
    Ok(rank(badge_repos(repos, &featured, &installed)))
}

/// The normalized `gh:owner/repo` slugs of every installed external GitHub
/// plugin, lower-cased for case-insensitive matching.
fn installed_slugs() -> Vec<String> {
    super::registry()
        .all()
        .iter()
        .filter_map(|p| p.source.as_deref())
        .filter_map(|s| PluginSource::parse(s).ok())
        .filter(|s| matches!(s, PluginSource::Github { .. }))
        .map(|s| s.slug().to_ascii_lowercase())
        .collect()
}

/// Map raw repos to badged results. Pure given the featured index and the
/// installed slug set, so it is unit-tested without the network.
fn badge_repos(
    repos: Vec<GitHubRepo>,
    featured: &FeaturedIndex,
    installed: &[String],
) -> Vec<DiscoveryResult> {
    repos
        .into_iter()
        .filter_map(|repo| {
            // A search result is `owner/repo`; anything else is not installable.
            if repo.full_name.split('/').filter(|s| !s.is_empty()).count() != 2 {
                return None;
            }
            let slug = format!("gh:{}", repo.full_name);
            let normalized = slug.to_ascii_lowercase();
            let is_installed = installed.contains(&normalized);
            let is_featured = featured.is_featured_source(&slug);
            // Installed wins the one-of display badge, but `featured` is kept
            // separately so an installed-and-featured repo still ranks featured.
            let badge = if is_installed {
                DiscoveryBadge::Installed
            } else if is_featured {
                DiscoveryBadge::Featured
            } else {
                DiscoveryBadge::Unvetted
            };
            Some(DiscoveryResult {
                install_command: format!("aoe plugin install {slug}"),
                slug,
                html_url: repo.html_url,
                featured: is_featured,
                description: repo.description.filter(|d| !d.is_empty()),
                stars: repo.stargazers_count,
                badge,
            })
        })
        .collect()
}

/// Rank featured sources first, then by GitHub stars descending (#2105 will add
/// popularity ranking; until then this is the issue's "featured status + stars").
fn rank(mut results: Vec<DiscoveryResult>) -> Vec<DiscoveryResult> {
    results.sort_by(|a, b| {
        b.featured
            .cmp(&a.featured)
            .then(b.stars.cmp(&a.stars))
            .then(a.slug.cmp(&b.slug))
    });
    results
}

fn client() -> Result<GitHubClient> {
    Ok(GitHubClient::unauthenticated(GitHubClientConfig {
        api_base: api_base(),
        user_agent: DEFAULT_USER_AGENT.to_string(),
        timeout: Duration::from_secs(30),
    })?)
}

fn api_base() -> String {
    std::env::var("AOE_UPDATE_API_BASE")
        .unwrap_or_else(|_| crate::github::DEFAULT_GITHUB_API_BASE.to_string())
}

/// The manifest fields a detail view shows, parsed leniently (unknown and
/// future keys are ignored) so a plugin targeting a newer `api_version` than
/// this host can install still renders in the modal.
#[derive(Debug, Clone, Serialize)]
pub struct DetailManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub api_version: u32,
    pub capabilities: Vec<String>,
    pub ui_contributions: Vec<UiSlotView>,
    /// Screenshot/GIF previews, each resolved to a browser-fetchable URL on
    /// `raw.githubusercontent.com`. Author-declared paths that fail validation
    /// are dropped here rather than failing the whole detail.
    pub screenshots: Vec<ScreenshotView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UiSlotView {
    pub slot: String,
    pub id: String,
}

/// A screenshot resolved for the detail modal: `src` is a fully-qualified
/// `raw.githubusercontent.com` URL the browser fetches directly.
#[derive(Debug, Clone, Serialize)]
pub struct ScreenshotView {
    pub src: String,
    pub alt: String,
    pub caption: String,
}

/// Resolve a manifest's raw screenshots into browser-fetchable views. Gated on
/// `api_version >= 5` to mirror [`aoe_plugin_api::PluginManifest::validate`], so
/// the detail modal never shows media for a manifest the install path would
/// reject; entries that fail [`screenshot_path_ok`] or have empty alt text are
/// dropped (one bad entry never poisons the whole detail), capped at
/// [`MAX_SCREENSHOTS`].
fn resolve_screenshots(
    api_version: u32,
    raws: Vec<RawScreenshot>,
    owner: &str,
    repo: &str,
    reference: Option<&str>,
) -> Vec<ScreenshotView> {
    if api_version < 5 {
        return Vec::new();
    }
    raws.into_iter()
        .filter(|s| screenshot_path_ok(&s.path) && !s.alt.trim().is_empty())
        .take(MAX_SCREENSHOTS)
        .map(|s| ScreenshotView {
            src: raw_url(owner, repo, reference, &s.path),
            alt: s.alt,
            caption: s.caption,
        })
        .collect()
}

/// Build the `raw.githubusercontent.com` URL for a repository-relative path.
/// `reference` defaults to `HEAD` (the repo's default branch) when the source
/// is unpinned. The path is already validated by [`screenshot_path_ok`]; this
/// only percent-encodes the remaining URL-unsafe bytes per segment.
fn raw_url(owner: &str, repo: &str, reference: Option<&str>, path: &str) -> String {
    let reference = reference.unwrap_or("HEAD");
    format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        utf8_percent_encode(owner, RAW_PATH),
        utf8_percent_encode(repo, RAW_PATH),
        utf8_percent_encode(reference, RAW_PATH),
        utf8_percent_encode(path, RAW_PATH),
    )
}

/// The on-demand detail for one plugin source: its manifest fields plus the
/// repo's published release tags (the available versions).
#[derive(Debug, Clone, Serialize)]
pub struct PluginDetail {
    pub source: String,
    pub manifest: Option<DetailManifest>,
    /// Why the manifest could not be read/parsed, if it could not.
    pub manifest_error: Option<String>,
    /// Published GitHub release tags, newest first (the available versions).
    pub release_tags: Vec<String>,
}

/// Lenient `aoe-plugin.toml` shape for the detail view. Unlike the strict host
/// parser it ignores unknown fields and does not range-check `api_version`, so a
/// not-yet-installable plugin still shows its version/description/capabilities.
#[derive(Deserialize)]
struct RawManifest {
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    description: String,
    api_version: u32,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    ui: Vec<RawUi>,
    #[serde(default)]
    screenshots: Vec<RawScreenshot>,
}

#[derive(Deserialize)]
struct RawUi {
    slot: String,
    id: String,
}

#[derive(Deserialize)]
struct RawScreenshot {
    #[serde(default)]
    path: String,
    #[serde(default)]
    alt: String,
    #[serde(default)]
    caption: String,
}

/// Fetch the detail for a `gh:owner/repo` source: its `aoe-plugin.toml` (read
/// via the contents API, no clone) and the repo's release tags. A manifest that
/// is missing or unparseable is reported in `manifest_error` while the release
/// tags still load, so the modal degrades gracefully.
pub async fn details(source: &str) -> Result<PluginDetail> {
    let parsed = PluginSource::parse(source)?;
    let PluginSource::Github { owner, repo, .. } = &parsed else {
        bail!("details are only available for a gh:owner/repo source");
    };
    // Honor a pinned tag/commit so a ref-pinned installed plugin's modal shows
    // the installed version, not whatever is on HEAD today.
    let reference = parsed.reference();
    let client = client()?;

    let manifest = match client
        .get_repo_file(owner, repo, "aoe-plugin.toml", reference)
        .await
    {
        Ok(text) => toml::from_str::<RawManifest>(&text)
            .map(|m| DetailManifest {
                id: m.id,
                name: m.name,
                version: m.version,
                description: m.description,
                api_version: m.api_version,
                capabilities: m.capabilities,
                ui_contributions: m
                    .ui
                    .into_iter()
                    .map(|u| UiSlotView {
                        slot: u.slot,
                        id: u.id,
                    })
                    .collect(),
                screenshots: resolve_screenshots(
                    m.api_version,
                    m.screenshots,
                    owner,
                    repo,
                    reference,
                ),
            })
            .map_err(|e| format!("aoe-plugin.toml is invalid: {e}")),
        Err(e) => Err(format!("{e}")),
    };

    // Release tags are best-effort: a repo with no releases is normal, so a
    // failure here just yields an empty list rather than failing the request.
    let release_tags = client
        .list_releases(owner, repo, 30)
        .await
        .map(|rs| rs.into_iter().map(|r| r.tag_name).collect())
        .unwrap_or_default();

    let (manifest, manifest_error) = match manifest {
        Ok(m) => (Some(m), None),
        Err(e) => (None, Some(e)),
    };
    Ok(PluginDetail {
        source: parsed.slug(),
        manifest,
        manifest_error,
        release_tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(full_name: &str, stars: u64) -> GitHubRepo {
        GitHubRepo {
            full_name: full_name.to_string(),
            html_url: format!("https://github.com/{full_name}"),
            description: Some("a plugin".to_string()),
            stargazers_count: stars,
            topics: vec!["aoe-plugin".to_string()],
        }
    }

    fn featured(slug: &str) -> FeaturedIndex {
        FeaturedIndex::from_toml_str(&format!(
            "[plugins.\"x.y\"]\nsource = \"{slug}\"\nversions = {{ \"1.0\" = \"sha256:abc\" }}\n"
        ))
        .unwrap()
    }

    #[test]
    fn badges_installed_featured_unvetted() {
        let repos = vec![
            repo("acme/installed", 5),
            repo("acme/vetted", 10),
            repo("acme/random", 100),
        ];
        let index = featured("gh:acme/vetted");
        let installed = vec!["gh:acme/installed".to_string()];
        let out = badge_repos(repos, &index, &installed);
        let by_slug = |slug: &str| out.iter().find(|r| r.slug == slug).unwrap().badge;
        assert_eq!(by_slug("gh:acme/installed"), DiscoveryBadge::Installed);
        assert_eq!(by_slug("gh:acme/vetted"), DiscoveryBadge::Featured);
        assert_eq!(by_slug("gh:acme/random"), DiscoveryBadge::Unvetted);
    }

    #[test]
    fn installed_match_is_case_insensitive() {
        let repos = vec![repo("Acme/Widget", 1)];
        let installed = vec!["gh:acme/widget".to_string()];
        let out = badge_repos(repos, &FeaturedIndex::default(), &installed);
        assert_eq!(out[0].badge, DiscoveryBadge::Installed);
    }

    #[test]
    fn ranks_featured_first_then_stars() {
        // A low-star featured result outranks a high-star unvetted one.
        let repos = vec![repo("acme/popular", 999), repo("acme/vetted", 1)];
        let index = featured("gh:acme/vetted");
        let out = rank(badge_repos(repos, &index, &[]));
        assert_eq!(out[0].slug, "gh:acme/vetted");
        assert_eq!(out[1].slug, "gh:acme/popular");
    }

    #[test]
    fn installed_and_featured_still_ranks_featured() {
        // A repo that is both installed and featured shows the Installed badge
        // but must still outrank a high-star unvetted repo (#2473 review).
        let repos = vec![repo("acme/popular", 999), repo("acme/vetted", 1)];
        let index = featured("gh:acme/vetted");
        let installed = vec!["gh:acme/vetted".to_string()];
        let out = rank(badge_repos(repos, &index, &installed));
        assert_eq!(out[0].slug, "gh:acme/vetted");
        assert_eq!(out[0].badge, DiscoveryBadge::Installed);
        assert!(out[0].featured);
    }

    #[test]
    fn drops_non_owner_repo_results() {
        let repos = vec![repo("not-a-slug", 1), repo("a/b/c", 1)];
        let out = badge_repos(repos, &FeaturedIndex::default(), &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn detail_manifest_parse_tolerates_newer_api_version_and_unknown_keys() {
        // A plugin targeting an api_version this host cannot install must still
        // render in the detail modal, and unknown/future keys are ignored.
        let toml = r#"
id = "acme.future"
name = "Future"
version = "9.9.9"
api_version = 99
description = "from the future"
capabilities = ["net"]
some_unknown_future_key = true

[[ui]]
slot = "status-bar"
id = "s"
"#;
        let m: RawManifest = toml::from_str(toml).expect("lenient parse");
        assert_eq!(m.version, "9.9.9");
        assert_eq!(m.api_version, 99);
        assert_eq!(m.capabilities, vec!["net"]);
        assert_eq!(m.ui.len(), 1);
        assert_eq!(m.ui[0].slot, "status-bar");
    }

    #[test]
    fn raw_url_defaults_to_head_and_encodes_path() {
        assert_eq!(
            raw_url("acme", "widget", None, "docs/shots/a.png"),
            "https://raw.githubusercontent.com/acme/widget/HEAD/docs/shots/a.png"
        );
        // A pinned ref is honored; spaces in a path are percent-encoded while
        // the `/` separators survive.
        assert_eq!(
            raw_url("acme", "widget", Some("v1.2.0"), "media/cool demo.gif"),
            "https://raw.githubusercontent.com/acme/widget/v1.2.0/media/cool%20demo.gif"
        );
    }

    #[test]
    fn detail_manifest_parses_screenshots_and_drops_bad_entries() {
        let toml = r#"
id = "acme.widget"
name = "Widget"
version = "1.0.0"
api_version = 5

[[screenshots]]
path = "docs/a.png"
alt = "good"

[[screenshots]]
path = "https://tracker.example.com/x.png"
alt = "bad url"

[[screenshots]]
path = "docs/b.png"
alt = "   "
"#;
        let m: RawManifest = toml::from_str(toml).expect("lenient parse");
        let kept = resolve_screenshots(m.api_version, m.screenshots, "acme", "widget", None);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].src,
            "https://raw.githubusercontent.com/acme/widget/HEAD/docs/a.png"
        );
    }

    #[test]
    fn screenshots_gated_out_below_api_version_5() {
        // A v4 manifest must not surface screenshots in the detail modal, since
        // the strict install validator rejects them; the lenient detail path
        // mirrors that gate.
        let toml = r#"
id = "acme.widget"
name = "Widget"
version = "1.0.0"
api_version = 4

[[screenshots]]
path = "docs/a.png"
alt = "good"
"#;
        let m: RawManifest = toml::from_str(toml).expect("lenient parse");
        let kept = resolve_screenshots(m.api_version, m.screenshots, "acme", "widget", None);
        assert!(kept.is_empty(), "v4 must not expose screenshots");
    }

    #[test]
    fn install_command_uses_the_slug() {
        let out = badge_repos(vec![repo("acme/widget", 1)], &FeaturedIndex::default(), &[]);
        assert_eq!(out[0].install_command, "aoe plugin install gh:acme/widget");
    }
}
