//! Detects a newer registry digest for the configured sandbox image; never pulls.

use anyhow::{anyhow, Result};

/// The digest doubles as the snooze key, so a dismissal sticks until the registry moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageUpdate {
    pub image: String,
    pub remote_digest: String,
}

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.manifest.v1+json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRef {
    pub host: String,
    pub repository: String,
    pub reference: String,
    pub pinned: bool,
}

impl RegistryRef {
    /// Docker CLI defaults: a first segment with `.`/`:` or `localhost` is the registry,
    /// otherwise Docker Hub with an implicit `library/` for single-segment names.
    pub fn parse(image: &str) -> Option<Self> {
        let image = image.trim();
        if image.is_empty() {
            return None;
        }

        let (name_and_tag, digest) = match image.split_once('@') {
            Some((lhs, rhs)) => (lhs, Some(rhs.to_string())),
            None => (image, None),
        };

        let (host, remainder) = match name_and_tag.split_once('/') {
            Some((first, rest))
                if first == "localhost" || first.contains('.') || first.contains(':') =>
            {
                (first.to_string(), rest.to_string())
            }
            _ => ("registry-1.docker.io".to_string(), name_and_tag.to_string()),
        };

        let (mut repository, tag) = match remainder.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), Some(tag.to_string())),
            _ => (remainder, None),
        };

        if repository.is_empty() {
            return None;
        }

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
        // Strip one outer pair of quotes so an embedded `"` survives.
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

/// Falls back to the first entry when none matches the repository.
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
        // Suffix match so a registry-qualified entry still matches `owner/img`.
        if let Some(wanted) = &wanted {
            if repo == wanted || repo.ends_with(&format!("/{wanted}")) {
                return Some(digest.to_string());
            }
        }
    }
    first
}

pub async fn check_for_image_update(image: &str) -> Result<Option<ImageUpdate>> {
    let Some(reference) = RegistryRef::parse(image) else {
        return Ok(None);
    };
    if reference.pinned {
        return Ok(None);
    }

    let image_owned = image.to_string();
    let local = tokio::task::spawn_blocking(move || {
        crate::containers::get_container_runtime().local_image_digest(&image_owned)
    })
    .await
    .ok()
    .flatten();

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
    fn registry_ref_parse_splits_host_repository_and_reference() {
        let cases = [
            (
                "ghcr.io/agent-of-empires/aoe-sandbox:latest",
                "ghcr.io",
                "agent-of-empires/aoe-sandbox",
                "latest",
                false,
            ),
            (
                "ghcr.io/agent-of-empires/aoe-sandbox",
                "ghcr.io",
                "agent-of-empires/aoe-sandbox",
                "latest",
                false,
            ),
            (
                "ubuntu:22.04",
                "registry-1.docker.io",
                "library/ubuntu",
                "22.04",
                false,
            ),
            (
                "mozillaai/foo",
                "registry-1.docker.io",
                "mozillaai/foo",
                "latest",
                false,
            ),
            (
                "localhost:5000/team/img:dev",
                "localhost:5000",
                "team/img",
                "dev",
                false,
            ),
            (
                "ghcr.io/agent-of-empires/aoe-sandbox@sha256:abc123def4567890",
                "ghcr.io",
                "agent-of-empires/aoe-sandbox",
                "sha256:abc123def4567890",
                true,
            ),
        ];
        for (raw, host, repository, reference, pinned) in cases {
            let r = RegistryRef::parse(raw).unwrap_or_else(|| panic!("{raw} must parse"));
            assert_eq!(
                (
                    r.host.as_str(),
                    r.repository.as_str(),
                    r.reference.as_str(),
                    r.pinned
                ),
                (host, repository, reference, pinned),
                "{raw}"
            );
        }
        for empty in ["", "   "] {
            assert!(RegistryRef::parse(empty).is_none());
        }
    }

    #[test]
    fn pick_repo_digest_prefers_a_match_then_falls_back_to_the_first() {
        let image = "ghcr.io/agent-of-empires/aoe-sandbox:latest";
        let cases = [
            (
                "ghcr.io/agent-of-empires/aoe-sandbox@sha256:aaa\ndocker.io/library/ubuntu@sha256:bbb\n",
                Some("sha256:aaa"),
            ),
            ("registry.example.com/other/img@sha256:ccc\n", Some("sha256:ccc")),
            ("", None),
            ("\n  \n", None),
        ];
        for (repo_digests, expected) in cases {
            assert_eq!(
                pick_repo_digest(image, repo_digests).as_deref(),
                expected,
                "{repo_digests:?}"
            );
        }
    }

    #[test]
    fn parses_only_bearer_challenges() {
        let cases = [
            (
                "Bearer realm=\"https://ghcr.io/token\",service=\"ghcr.io\",scope=\"repository:agent-of-empires/aoe-sandbox:pull\"",
                Some(("https://ghcr.io/token", Some("ghcr.io"), Some("repository:agent-of-empires/aoe-sandbox:pull"))),
            ),
            // Only the outer quote pair is stripped.
            (
                "Bearer realm=https://r.example/token,service=svc,scope=\"a\"b\"",
                Some(("https://r.example/token", Some("svc"), Some("a\"b"))),
            ),
            ("Basic realm=\"x\"", None),
        ];
        for (header, expected) in cases {
            let got = parse_bearer_challenge(header);
            let got = got
                .as_ref()
                .map(|c| (c.realm.as_str(), c.service.as_deref(), c.scope.as_deref()));
            assert_eq!(got, expected, "{header}");
        }
    }
}
