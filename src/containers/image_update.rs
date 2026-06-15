//! Detect when the configured sandbox image has a newer build in its registry.
//!
//! The TUI surfaces a "sandbox image update available" banner so users on the
//! `:latest` tag (the default) learn that `ghcr.io/.../aoe-sandbox` moved on
//! and can pull the refresh without leaving the app. The check compares the
//! digest of the locally-stored image against the registry's current digest
//! for the same tag; it never pulls and degrades to "no update" whenever the
//! runtime, image, or network is unavailable.

use anyhow::{anyhow, Result};

/// A detected sandbox-image update: the configured image plus the registry
/// digest it now resolves to. The digest doubles as the snooze key so a
/// dismissal sticks until the registry moves again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageUpdate {
    pub image: String,
    pub remote_digest: String,
}

/// Default Accept header advertising every manifest media type a tag might
/// resolve to (single image or multi-arch index, Docker or OCI), so the
/// registry returns a `Docker-Content-Digest` for whichever it serves.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.manifest.v1+json";

/// A parsed image reference, split into the registry coordinates needed to
/// query the registry HTTP API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRef {
    /// Registry host (e.g. `ghcr.io`, `registry-1.docker.io`).
    pub host: String,
    /// Repository path (e.g. `agent-of-empires/aoe-sandbox`, `library/ubuntu`).
    pub repository: String,
    /// Tag or digest reference (e.g. `latest`, `sha256:...`).
    pub reference: String,
    /// True when `reference` is a content digest rather than a tag. A
    /// digest-pinned image can't go stale, so the staleness check is skipped.
    pub pinned: bool,
}

impl RegistryRef {
    /// Parse a Docker/OCI image reference into registry coordinates, applying
    /// the same defaults the docker CLI uses: a first path segment that looks
    /// like a hostname (contains `.`/`:` or is `localhost`) is the registry,
    /// otherwise the image lives on Docker Hub and a single-segment name gets
    /// the implicit `library/` namespace. Returns `None` for an empty name.
    pub fn parse(image: &str) -> Option<Self> {
        let image = image.trim();
        if image.is_empty() {
            return None;
        }

        // Peel a trailing `@sha256:...` digest first; whatever precedes it is
        // the name (and possibly a tag we then ignore for the request).
        let (name_and_tag, digest) = match image.split_once('@') {
            Some((lhs, rhs)) => (lhs, Some(rhs.to_string())),
            None => (image, None),
        };

        // Split host from the remaining path. Only the first segment can be a
        // registry host, and only if it carries a `.`/`:` or is `localhost`.
        let (host, remainder) = match name_and_tag.split_once('/') {
            Some((first, rest))
                if first == "localhost" || first.contains('.') || first.contains(':') =>
            {
                (first.to_string(), rest.to_string())
            }
            _ => ("registry-1.docker.io".to_string(), name_and_tag.to_string()),
        };

        // The tag is the part after the final `:` in the path remainder; a
        // colon only counts when it isn't inside a path segment boundary.
        let (mut repository, tag) = match remainder.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), Some(tag.to_string())),
            _ => (remainder, None),
        };

        if repository.is_empty() {
            return None;
        }

        // Docker Hub's library namespace is implicit for single-segment names.
        if host == "registry-1.docker.io" && !repository.contains('/') {
            repository = format!("library/{repository}");
        }

        let (reference, pinned) = match digest {
            Some(d) => (d, true),
            None => (tag.unwrap_or_else(|| "latest".to_string()), false),
        };

        Some(Self {
            host,
            repository,
            reference,
            pinned,
        })
    }

    fn manifest_url(&self) -> String {
        format!(
            "https://{}/v2/{}/manifests/{}",
            self.host, self.repository, self.reference
        )
    }

    /// Fetch the registry digest for this reference, performing the standard
    /// token handshake when the registry answers the first request with a
    /// `401 WWW-Authenticate: Bearer` challenge (ghcr.io and Docker Hub both
    /// do for anonymous pulls of public images).
    async fn fetch_remote_digest(&self, client: &reqwest::Client) -> Result<String> {
        let url = self.manifest_url();
        let resp = client
            .get(&url)
            .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT)
            .send()
            .await?;

        let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let token = self.obtain_token(client, &resp).await?;
            client
                .get(&url)
                .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT)
                .bearer_auth(token)
                .send()
                .await?
        } else {
            resp
        };

        let resp = resp.error_for_status()?;
        resp.headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("registry response missing Docker-Content-Digest header"))
    }

    /// Resolve a bearer token from a `401`'s `WWW-Authenticate: Bearer` header
    /// by calling the advertised `realm` with its `service`/`scope` params.
    async fn obtain_token(
        &self,
        client: &reqwest::Client,
        challenge: &reqwest::Response,
    ) -> Result<String> {
        let header = challenge
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("registry 401 without WWW-Authenticate header"))?;

        let params = parse_bearer_challenge(header)
            .ok_or_else(|| anyhow!("unsupported WWW-Authenticate challenge: {header}"))?;

        let mut url = reqwest::Url::parse(&params.realm)?;
        {
            let mut qp = url.query_pairs_mut();
            if let Some(service) = &params.service {
                qp.append_pair("service", service);
            }
            if let Some(scope) = &params.scope {
                qp.append_pair("scope", scope);
            }
        }

        let token: TokenResponse = client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        token
            .token
            .or(token.access_token)
            .ok_or_else(|| anyhow!("registry token endpoint returned no token"))
    }
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

/// Parse a `Bearer realm="...",service="...",scope="..."` challenge into its
/// parts. Returns `None` when the scheme isn't `Bearer` or `realm` is absent.
fn parse_bearer_challenge(header: &str) -> Option<BearerChallenge> {
    let rest = header.strip_prefix("Bearer ").or_else(|| {
        header
            .strip_prefix("bearer ")
            .or_else(|| header.strip_prefix("BEARER "))
    })?;

    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for part in rest.split(',') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        // Strip only a single outer pair of quotes, not every quote, so a
        // value that legitimately contains a `"` survives intact.
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value)
            .to_string();
        match key.trim() {
            "realm" => realm = Some(value),
            "service" => service = Some(value),
            "scope" => scope = Some(value),
            _ => {}
        }
    }

    Some(BearerChallenge {
        realm: realm?,
        service,
        scope,
    })
}

/// Pick the `sha256:...` digest matching `image`'s repository out of docker's
/// newline-separated `RepoDigests` (`repo@sha256:...` per line). Falls back to
/// the first entry's digest when none matches the parsed repository, and
/// returns `None` when there are no usable entries (e.g. a locally-built image
/// with an empty `RepoDigests`).
pub fn pick_repo_digest(image: &str, repo_digests: &str) -> Option<String> {
    let wanted = RegistryRef::parse(image).map(|r| r.repository);

    let mut first = None;
    for line in repo_digests.lines() {
        let line = line.trim();
        let Some((repo, digest)) = line.split_once("@") else {
            continue;
        };
        if digest.is_empty() {
            continue;
        }
        if first.is_none() {
            first = Some(digest.to_string());
        }
        // `repo` carries the registry-qualified path; match on a suffix so a
        // `ghcr.io/owner/img@...` entry still matches the parsed `owner/img`.
        if let Some(wanted) = &wanted {
            if repo == wanted || repo.ends_with(&format!("/{wanted}")) {
                return Some(digest.to_string());
            }
        }
    }
    first
}

/// Check whether the configured sandbox `image` has a newer digest in its
/// registry than the copy stored locally. Returns:
/// - `Ok(Some(_))` when a different digest is available to pull,
/// - `Ok(None)` when up to date, not locally present, digest-pinned, or the
///   runtime can't report a local digest (nothing actionable to surface),
/// - `Err(_)` when the registry query itself fails (caller logs and stays quiet).
pub async fn check_for_image_update(image: &str) -> Result<Option<ImageUpdate>> {
    let Some(reference) = RegistryRef::parse(image) else {
        return Ok(None);
    };
    // A digest-pinned image is already exact; it can't drift.
    if reference.pinned {
        return Ok(None);
    }

    // Reading the local digest shells out to the runtime; keep it off the
    // async worker threads.
    let image_owned = image.to_string();
    let local = tokio::task::spawn_blocking(move || {
        crate::containers::get_container_runtime().local_image_digest(&image_owned)
    })
    .await
    .ok()
    .flatten();

    // No local copy (or no reportable digest) means there's nothing to update
    // in place; the first sandbox launch pulls it via `ensure_image`.
    let Some(local) = local else {
        return Ok(None);
    };

    let client = reqwest::Client::builder()
        .user_agent(crate::github::DEFAULT_USER_AGENT)
        .timeout(std::time::Duration::from_secs(8))
        .build()?;

    let remote = reference.fetch_remote_digest(&client).await?;

    if remote != local {
        tracing::info!(
            target: "containers.image_update",
            %image,
            local = %local,
            remote = %remote,
            "sandbox image update available"
        );
        Ok(Some(ImageUpdate {
            image: image.to_string(),
            remote_digest: remote,
        }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ghcr_reference_with_tag() {
        let r = RegistryRef::parse("ghcr.io/agent-of-empires/aoe-sandbox:latest").unwrap();
        assert_eq!(r.host, "ghcr.io");
        assert_eq!(r.repository, "agent-of-empires/aoe-sandbox");
        assert_eq!(r.reference, "latest");
        assert!(!r.pinned);
    }

    #[test]
    fn defaults_tag_to_latest() {
        let r = RegistryRef::parse("ghcr.io/agent-of-empires/aoe-sandbox").unwrap();
        assert_eq!(r.reference, "latest");
        assert!(!r.pinned);
    }

    #[test]
    fn applies_docker_hub_library_namespace() {
        let r = RegistryRef::parse("ubuntu:22.04").unwrap();
        assert_eq!(r.host, "registry-1.docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.reference, "22.04");
    }

    #[test]
    fn keeps_namespaced_docker_hub_repository() {
        let r = RegistryRef::parse("mozillaai/foo").unwrap();
        assert_eq!(r.host, "registry-1.docker.io");
        assert_eq!(r.repository, "mozillaai/foo");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn parses_registry_with_port() {
        let r = RegistryRef::parse("localhost:5000/team/img:dev").unwrap();
        assert_eq!(r.host, "localhost:5000");
        assert_eq!(r.repository, "team/img");
        assert_eq!(r.reference, "dev");
    }

    #[test]
    fn marks_digest_pinned_references() {
        let r = RegistryRef::parse("ghcr.io/agent-of-empires/aoe-sandbox@sha256:abc123def4567890")
            .unwrap();
        assert!(r.pinned);
        assert_eq!(r.reference, "sha256:abc123def4567890");
    }

    #[test]
    fn rejects_empty_reference() {
        assert!(RegistryRef::parse("").is_none());
        assert!(RegistryRef::parse("   ").is_none());
    }

    #[test]
    fn picks_matching_repo_digest() {
        let repo_digests = "\
ghcr.io/agent-of-empires/aoe-sandbox@sha256:aaa\n\
docker.io/library/ubuntu@sha256:bbb\n";
        let digest =
            pick_repo_digest("ghcr.io/agent-of-empires/aoe-sandbox:latest", repo_digests).unwrap();
        assert_eq!(digest, "sha256:aaa");
    }

    #[test]
    fn falls_back_to_first_repo_digest_when_none_match() {
        let repo_digests = "registry.example.com/other/img@sha256:ccc\n";
        let digest =
            pick_repo_digest("ghcr.io/agent-of-empires/aoe-sandbox:latest", repo_digests).unwrap();
        assert_eq!(digest, "sha256:ccc");
    }

    #[test]
    fn returns_none_for_empty_repo_digests() {
        assert!(pick_repo_digest("ghcr.io/agent-of-empires/aoe-sandbox:latest", "").is_none());
        assert!(
            pick_repo_digest("ghcr.io/agent-of-empires/aoe-sandbox:latest", "\n  \n").is_none()
        );
    }

    #[test]
    fn parses_bearer_challenge_fields() {
        let header = "Bearer realm=\"https://ghcr.io/token\",service=\"ghcr.io\",scope=\"repository:agent-of-empires/aoe-sandbox:pull\"";
        let c = parse_bearer_challenge(header).unwrap();
        assert_eq!(c.realm, "https://ghcr.io/token");
        assert_eq!(c.service.as_deref(), Some("ghcr.io"));
        assert_eq!(
            c.scope.as_deref(),
            Some("repository:agent-of-empires/aoe-sandbox:pull")
        );
    }

    #[test]
    fn rejects_non_bearer_challenge() {
        assert!(parse_bearer_challenge("Basic realm=\"x\"").is_none());
    }

    #[test]
    fn strips_only_the_outer_quote_pair() {
        // An unquoted value passes through; only a single surrounding pair is
        // removed, so an embedded quote is preserved rather than stripped.
        let c = parse_bearer_challenge(
            "Bearer realm=https://r.example/token,service=svc,scope=\"a\"b\"",
        )
        .unwrap();
        assert_eq!(c.realm, "https://r.example/token");
        assert_eq!(c.service.as_deref(), Some("svc"));
        assert_eq!(c.scope.as_deref(), Some("a\"b"));
    }
}
