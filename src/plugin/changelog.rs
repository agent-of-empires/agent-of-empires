//! Best-effort changelog assembly for an in-UI plugin update preview.
//!
//! Given the prior installed ref/commit and the target ref/commit, produce a
//! human-readable list of what changed between them. Release-tracking updates
//! show GitHub release notes; everything else (a branch/ref-tracked install, a
//! moved tag whose content changed, or a release whose notes cannot be
//! bracketed) falls back to commit subjects from the compare endpoint.
//!
//! This is presentation metadata, not a gate: every failure path (rate limit,
//! 404, a local source) returns [`UpdateChangelog::unavailable`] rather than an
//! error, so a missing changelog never blocks an update the user already chose
//! to review. It is assembled only in `install::preview_update`, behind an
//! explicit user action, so the extra unauthenticated GitHub request (60/hr/IP)
//! is never spent on a background sweep.

use std::time::Duration;

use serde::Serialize;

use crate::github::{
    GitHubClient, GitHubClientConfig, GitHubCompareCommit, GitHubError, GitHubRelease,
    DEFAULT_USER_AGENT,
};

use super::source::PluginSource;

/// At most this many release entries before marking the changelog truncated.
const RELEASES_CAP: usize = 20;
/// At most this many commit entries before marking the changelog truncated.
const COMMITS_CAP: usize = 50;
/// Truncate a release body to this many bytes (on a char boundary) so a
/// pathological release note does not bloat the preview payload.
const BODY_CAP: usize = 8 * 1024;

/// What changed between the installed version and the update target. `entries`
/// is newest-first. `truncated` flags that more existed than are shown.
/// `unavailable_reason` distinguishes "could not load the changelog" from "there
/// were genuinely no entries" (both leave `entries` empty).
#[derive(Debug, Clone, Serialize)]
pub struct UpdateChangelog {
    pub entries: Vec<ChangelogEntry>,
    pub truncated: bool,
    pub unavailable_reason: Option<String>,
    /// A GitHub URL for the full history when the changelog is capped or a
    /// surface cannot show it all (the releases page, or the compare view). The
    /// non-scrollable TUI popup links to it; the web modal shows it on truncation.
    pub more_url: Option<String>,
}

/// One changelog item: a published release's notes, or a single commit subject.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangelogEntry {
    Release {
        tag: String,
        body: Option<String>,
        published_at: Option<String>,
    },
    Commit {
        sha: String,
        subject: String,
        url: Option<String>,
    },
}

impl UpdateChangelog {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            truncated: false,
            unavailable_reason: None,
            more_url: None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            entries: Vec::new(),
            truncated: false,
            unavailable_reason: Some(reason.into()),
            more_url: None,
        }
    }
}

/// The human GitHub base URL for a source, `https://github.com/{owner}/{repo}`.
fn github_web_base(owner: &str, repo: &str) -> String {
    format!("https://github.com/{owner}/{repo}")
}

/// Map a GitHub error to a short, user-facing "unavailable" reason. A rate limit
/// is the common one worth naming so the user knows to retry later.
fn unavailable_for(err: &GitHubError) -> UpdateChangelog {
    let reason = match err {
        GitHubError::RateLimited => "GitHub rate limit reached; changelog unavailable.",
        _ => "Changelog unavailable.",
    };
    UpdateChangelog::unavailable(reason)
}

fn client() -> Result<GitHubClient, GitHubError> {
    GitHubClient::unauthenticated(GitHubClientConfig {
        api_base: super::fetch::github_api_base(),
        user_agent: DEFAULT_USER_AGENT.to_string(),
        timeout: Duration::from_secs(30),
    })
}

/// The first line of a commit message, the conventional "subject".
fn subject(message: &str) -> String {
    message.lines().next().unwrap_or("").trim().to_string()
}

/// Truncate `body` to [`BODY_CAP`] bytes on a char boundary, appending an
/// ellipsis when cut.
fn cap_body(body: String) -> String {
    if body.len() <= BODY_CAP {
        return body;
    }
    let mut end = BODY_CAP;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

/// Build the changelog between the installed version and the update target.
/// Best-effort: returns an `unavailable` changelog rather than erroring on any
/// failure.
pub async fn build(
    source: &PluginSource,
    prior_ref: Option<&str>,
    prior_commit: Option<&str>,
    target_ref: Option<&str>,
    target_commit: Option<&str>,
) -> UpdateChangelog {
    let (owner, repo) = match source {
        PluginSource::Github { owner, repo, .. } => (owner.as_str(), repo.as_str()),
        PluginSource::Local(_) => {
            return UpdateChangelog::unavailable("Changelog is only available for GitHub plugins.")
        }
    };

    let client = match client() {
        Ok(c) => c,
        Err(e) => return unavailable_for(&e),
    };

    // Release path: only when the refs are distinct release tags we can bracket
    // in the releases list. A moved tag (same ref, changed content) or an
    // unbracketable pair falls through to the commit compare.
    if let (Some(prior_ref), Some(target_ref)) = (prior_ref, target_ref) {
        if prior_ref != target_ref {
            if let Some(changelog) =
                release_changelog(&client, owner, repo, prior_ref, target_ref).await
            {
                return changelog;
            }
        }
    }

    match (prior_commit, target_commit) {
        (Some(base), Some(head)) => commit_changelog(&client, owner, repo, base, head).await,
        _ => UpdateChangelog::unavailable("Changelog unavailable."),
    }
}

/// Collect release notes for the published releases strictly newer than
/// `prior_ref` up to and including `target_ref`, bracketed by tag identity in
/// the list order GitHub returns (newest-first). Returns `None` to signal "fall
/// back to commits" when the pair cannot be bracketed (target tag absent, no
/// release entries between them); returns `Some(unavailable)` on an API error so
/// a rate limit does not silently turn into a noisy commit dump.
async fn release_changelog(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    prior_ref: &str,
    target_ref: &str,
) -> Option<UpdateChangelog> {
    let releases = match client.list_releases(owner, repo, 100).await {
        Ok(r) => r,
        Err(GitHubError::NotFound { .. }) => return None,
        Err(e) => return Some(unavailable_for(&e)),
    };
    bracket_releases(&releases, prior_ref, target_ref).map(|(entries, truncated)| UpdateChangelog {
        entries,
        truncated,
        unavailable_reason: None,
        more_url: Some(format!("{}/releases", github_web_base(owner, repo))),
    })
}

/// Pure release-bracketing: from the releases list (newest-first as GitHub
/// returns it), collect published (non-draft, non-prerelease) entries from
/// `target_ref` down to, but excluding, `prior_ref`. Returns `None` when the
/// pair cannot be bracketed (target tag absent, or nothing sits between them),
/// signalling the caller to fall back to commits. The bool is the truncation
/// flag (cap hit, or the prior tag was older than the fetched page).
fn bracket_releases(
    releases: &[GitHubRelease],
    prior_ref: &str,
    target_ref: &str,
) -> Option<(Vec<ChangelogEntry>, bool)> {
    let mut entries = Vec::new();
    let mut truncated = false;
    let mut collecting = false;
    let mut found_prior = false;
    for release in releases.iter().filter(|r| !r.draft && !r.prerelease) {
        if release.tag_name == target_ref {
            collecting = true;
        }
        if !collecting {
            continue;
        }
        if release.tag_name == prior_ref {
            found_prior = true;
            break;
        }
        if entries.len() >= RELEASES_CAP {
            truncated = true;
            break;
        }
        entries.push(ChangelogEntry::Release {
            tag: release.tag_name.clone(),
            body: release
                .body
                .clone()
                .map(|b| cap_body(b.trim().to_string()))
                .filter(|b| !b.is_empty()),
            published_at: release.published_at.clone(),
        });
    }

    if entries.is_empty() {
        // Target tag never appeared, or nothing sits between the two tags: not a
        // usable release bracket.
        return None;
    }
    // The prior tag was older than the fetched page (or filtered out), so there
    // may be releases we did not show.
    Some((entries, truncated || !found_prior))
}

/// Commit subjects on `head` not on `base`, newest-first. The compare endpoint
/// returns commits oldest-first and caps the list at 250, so reverse for display
/// and mark truncation off `total_commits`.
async fn commit_changelog(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> UpdateChangelog {
    let compare = match client.compare_commits(owner, repo, base, head).await {
        Ok(c) => c,
        Err(e) => return unavailable_for(&e),
    };

    // identical / behind have nothing meaningful to show going forward.
    if compare.commits.is_empty() {
        return UpdateChangelog::empty();
    }

    let (entries, truncated) = map_commits(&compare.commits, compare.total_commits);
    UpdateChangelog {
        entries,
        truncated,
        unavailable_reason: None,
        more_url: Some(format!(
            "{}/compare/{base}...{head}",
            github_web_base(owner, repo)
        )),
    }
}

/// Pure commit mapping: the compare endpoint returns commits oldest-first and
/// caps the list at 250, so reverse for newest-first display, take [`COMMITS_CAP`],
/// and mark truncation off the gap between `total_commits` and what was returned
/// or shown.
fn map_commits(commits: &[GitHubCompareCommit], total_commits: u64) -> (Vec<ChangelogEntry>, bool) {
    let returned = commits.len() as u64;
    let entries: Vec<ChangelogEntry> = commits
        .iter()
        .rev()
        .take(COMMITS_CAP)
        .map(|c| ChangelogEntry::Commit {
            sha: c.sha.clone(),
            subject: subject(&c.commit.message),
            url: (!c.html_url.is_empty()).then(|| c.html_url.clone()),
        })
        .collect();
    let truncated = total_commits > returned || returned as usize > COMMITS_CAP;
    (entries, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_takes_first_line() {
        assert_eq!(subject("feat: add thing\n\nlong body"), "feat: add thing");
        assert_eq!(subject("  trimmed  "), "trimmed");
        assert_eq!(subject(""), "");
    }

    #[test]
    fn cap_body_truncates_on_char_boundary() {
        let body = "é".repeat(BODY_CAP); // each 'é' is 2 bytes, so this exceeds the cap
        let capped = cap_body(body);
        assert!(capped.ends_with('…'));
        // The slice point landed on a valid boundary (no panic) and is bounded.
        assert!(capped.len() <= BODY_CAP + "…".len());
    }

    #[test]
    fn cap_body_keeps_short_bodies() {
        assert_eq!(cap_body("short".to_string()), "short");
    }

    fn release(tag: &str, body: Option<&str>, prerelease: bool, draft: bool) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.to_string(),
            body: body.map(str::to_string),
            published_at: Some("2026-01-01T00:00:00Z".to_string()),
            draft,
            prerelease,
            assets: vec![],
        }
    }

    fn release_tags(entries: &[ChangelogEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|e| match e {
                ChangelogEntry::Release { tag, .. } => tag.as_str(),
                ChangelogEntry::Commit { .. } => panic!("expected release entry"),
            })
            .collect()
    }

    #[test]
    fn bracket_collects_between_target_and_prior_exclusive() {
        // Newest-first, as GitHub returns. prior=v1.0.0, target=v1.2.0.
        let releases = vec![
            release("v1.3.0", Some("newer, excluded"), false, false),
            release("v1.2.0", Some("target notes"), false, false),
            release("v1.1.0", Some("middle notes"), false, false),
            release("v1.0.0", Some("prior, excluded"), false, false),
        ];
        let (entries, truncated) = bracket_releases(&releases, "v1.0.0", "v1.2.0").unwrap();
        assert_eq!(release_tags(&entries), vec!["v1.2.0", "v1.1.0"]);
        assert!(!truncated, "prior tag was found in the page");
    }

    #[test]
    fn bracket_filters_drafts_and_prereleases() {
        let releases = vec![
            release("v1.2.0", Some("target"), false, false),
            release("v1.2.0-rc1", Some("rc"), true, false),
            release("v1.1.5-draft", Some("draft"), false, true),
            release("v1.1.0", Some("middle"), false, false),
            release("v1.0.0", None, false, false),
        ];
        let (entries, _) = bracket_releases(&releases, "v1.0.0", "v1.2.0").unwrap();
        assert_eq!(release_tags(&entries), vec!["v1.2.0", "v1.1.0"]);
    }

    #[test]
    fn bracket_returns_none_when_target_absent() {
        let releases = vec![release("v1.1.0", None, false, false)];
        assert!(bracket_releases(&releases, "v1.0.0", "v9.9.9").is_none());
    }

    #[test]
    fn bracket_truncates_when_prior_older_than_page() {
        // prior tag is not present (older than the fetched page) => truncated.
        let releases = vec![
            release("v2.0.0", Some("a"), false, false),
            release("v1.9.0", Some("b"), false, false),
        ];
        let (entries, truncated) = bracket_releases(&releases, "v1.0.0", "v2.0.0").unwrap();
        assert_eq!(release_tags(&entries), vec!["v2.0.0", "v1.9.0"]);
        assert!(truncated, "prior tag not in page should flag truncation");
    }

    #[test]
    fn bracket_empty_body_becomes_none() {
        let releases = vec![
            release("v1.1.0", Some("   "), false, false),
            release("v1.0.0", None, false, false),
        ];
        let (entries, _) = bracket_releases(&releases, "v1.0.0", "v1.1.0").unwrap();
        match &entries[0] {
            ChangelogEntry::Release { body, .. } => assert!(body.is_none()),
            _ => panic!("expected release"),
        }
    }

    fn commit(sha: &str, message: &str, url: &str) -> GitHubCompareCommit {
        GitHubCompareCommit {
            sha: sha.to_string(),
            html_url: url.to_string(),
            commit: crate::github::client::GitHubCommitInner {
                message: message.to_string(),
            },
        }
    }

    #[test]
    fn map_commits_reverses_to_newest_first_and_takes_subject() {
        // GitHub returns oldest-first; display is newest-first.
        let commits = vec![
            commit("aaa", "first\n\nbody", "http://x/aaa"),
            commit("bbb", "second", ""),
        ];
        let (entries, truncated) = map_commits(&commits, 2);
        match (&entries[0], &entries[1]) {
            (
                ChangelogEntry::Commit {
                    sha: s0,
                    subject: j0,
                    url: u0,
                },
                ChangelogEntry::Commit {
                    sha: s1,
                    subject: j1,
                    url: u1,
                },
            ) => {
                assert_eq!((s0.as_str(), j0.as_str()), ("bbb", "second"));
                assert_eq!(u0, &None, "empty html_url maps to None");
                assert_eq!((s1.as_str(), j1.as_str()), ("aaa", "first"));
                assert_eq!(u1.as_deref(), Some("http://x/aaa"));
            }
            _ => panic!("expected commit entries"),
        }
        assert!(!truncated);
    }

    #[test]
    fn map_commits_flags_truncation_when_total_exceeds_returned() {
        let commits = vec![commit("aaa", "x", "")];
        let (_, truncated) = map_commits(&commits, 300);
        assert!(truncated, "total_commits > returned must flag truncation");
    }
}
