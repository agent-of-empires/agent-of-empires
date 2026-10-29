//! Detects a newer registry digest for the configured sandbox image; never pulls.

use std::net::IpAddr;

use anyhow::{anyhow, Result};

/// The digest doubles as the snooze key, so a dismissal sticks until the registry moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageUpdate {
    pub image: String,
    pub remote_digest: String,
}

/// What the Container runtime knows about the local copy of an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalDigest {
    Digest(String),
    /// Present, but with no repo digest to compare: built locally, or Apple Container,
    /// whose `image inspect` exposes none.
    NoDigest,
    Missing,
    /// The runtime didn't answer (not installed, or its daemon isn't running), so whether the
    /// image is pulled is unknown too.
    Unavailable,
}

/// Why a newer image can't be detected. Never "current": the dashboard must not say an
/// image is up to date when it can't know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    Unparseable,
    /// Pinned by digest, so it never changes.
    Pinned,
    NotPulled,
    NoLocalDigest,
    RuntimeUnavailable,
}

impl UnknownReason {
    pub fn code(self) -> &'static str {
        match self {
            UnknownReason::Unparseable => "unparseable",
            UnknownReason::Pinned => "pinned",
            UnknownReason::NotPulled => "not_pulled",
            UnknownReason::NoLocalDigest => "no_local_digest",
            UnknownReason::RuntimeUnavailable => "runtime_unavailable",
        }
    }
}

/// The result of asking the registry. An `Err` from [`check_image`] means it couldn't be asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageCheck {
    Available(ImageUpdate),
    Current,
    Unknown(UnknownReason),
}

/// Why the registry couldn't be asked. The code is what an API client sees; the chain stays
/// in the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryErrorKind {
    Unreachable,
    Auth,
    BadResponse,
}

impl RegistryErrorKind {
    fn code(self) -> &'static str {
        match self {
            RegistryErrorKind::Unreachable => "registry_unreachable",
            RegistryErrorKind::Auth => "registry_auth",
            RegistryErrorKind::BadResponse => "registry_bad_response",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{}", .kind.code())]
struct RegistryError {
    kind: RegistryErrorKind,
    #[source]
    source: anyhow::Error,
}

fn fail(kind: RegistryErrorKind, source: impl Into<anyhow::Error>) -> anyhow::Error {
    RegistryError {
        kind,
        source: source.into(),
    }
    .into()
}

/// The short code for an error from [`check_image`]; anything unclassified is a bad response.
pub fn error_code(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<RegistryError>()
        .map_or(RegistryErrorKind::BadResponse, |e| e.kind)
        .code()
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
        let request = || {
            client
                .get(&url)
                .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT)
        };
        let resp = send(request()).await?;

        let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let token = self.obtain_token(client, &resp).await?;
            send(request().bearer_auth(token)).await?
        } else {
            resp
        };

        let resp = ensure_success(resp)?;
        resp.headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                fail(
                    RegistryErrorKind::BadResponse,
                    anyhow!("registry response missing Docker-Content-Digest header"),
                )
            })
    }

    async fn obtain_token(
        &self,
        client: &reqwest::Client,
        challenge: &reqwest::Response,
    ) -> Result<String> {
        let bad = |e: anyhow::Error| fail(RegistryErrorKind::BadResponse, e);
        let header = challenge
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| bad(anyhow!("registry 401 without WWW-Authenticate header")))?;

        let params = parse_bearer_challenge(header)
            .ok_or_else(|| bad(anyhow!("unsupported WWW-Authenticate challenge: {header}")))?;

        let mut url = validate_bearer_realm(&params.realm)?;
        {
            let mut qp = url.query_pairs_mut();
            if let Some(service) = &params.service {
                qp.append_pair("service", service);
            }
            if let Some(scope) = &params.scope {
                qp.append_pair("scope", scope);
            }
        }

        let token: TokenResponse = ensure_success(send(client.get(url)).await?)?
            .json()
            .await
            .map_err(|e| bad(e.into()))?;

        token
            .token
            .or(token.access_token)
            .ok_or_else(|| bad(anyhow!("registry token endpoint returned no token")))
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    request
        .send()
        .await
        .map_err(|e| fail(RegistryErrorKind::Unreachable, e))
}

/// Only 2xx counts: redirects are off, so a 3xx is the registry misbehaving.
fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let kind = match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            RegistryErrorKind::Auth
        }
        _ => RegistryErrorKind::BadResponse,
    };
    Err(fail(kind, anyhow!("registry returned HTTP {status}")))
}

/// The realm is chosen by the registry, so it must not point the check at the local machine.
fn validate_bearer_realm(realm: &str) -> Result<reqwest::Url> {
    let refuse = |why: &str| {
        fail(
            RegistryErrorKind::BadResponse,
            anyhow!("refused bearer realm {realm}: {why}"),
        )
    };
    let url = reqwest::Url::parse(realm).map_err(|e| fail(RegistryErrorKind::BadResponse, e))?;
    if url.scheme() != "https" {
        return Err(refuse("not https"));
    }
    let host = url.host_str().ok_or_else(|| refuse("no host"))?;
    let host = host.to_ascii_lowercase();
    let local = host == "localhost"
        || host.ends_with(".localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| match ip.to_canonical() {
                IpAddr::V4(ip) => ip.is_loopback() || ip.is_unspecified() || ip.is_link_local(),
                IpAddr::V6(ip) => {
                    ip.is_loopback() || ip.is_unspecified() || ip.is_unicast_link_local()
                }
            });
    if local {
        return Err(refuse("local host"));
    }
    Ok(url)
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

/// The digest for the entry naming this image's repository; none when only other repos match.
pub fn pick_repo_digest(image: &str, repo_digests: &str) -> Option<String> {
    let wanted = RegistryRef::parse(image)?.repository;
    repo_digests.lines().find_map(|line| {
        let (repo, digest) = line.trim().split_once('@')?;
        // Suffix match so a registry-qualified entry still matches `owner/img`.
        let matches = repo == wanted || repo.ends_with(&format!("/{wanted}"));
        (matches && !digest.is_empty()).then(|| digest.to_string())
    })
}

/// Everything decided before the registry is asked: the reference and local digest to
/// compare, or the reason there is nothing to compare.
fn pre_check(
    image: &str,
    local: impl FnOnce() -> LocalDigest,
) -> std::result::Result<(RegistryRef, String), UnknownReason> {
    let reference = RegistryRef::parse(image).ok_or(UnknownReason::Unparseable)?;
    if reference.pinned {
        return Err(UnknownReason::Pinned);
    }
    match local() {
        LocalDigest::Digest(digest) => Ok((reference, digest)),
        LocalDigest::NoDigest => Err(UnknownReason::NoLocalDigest),
        LocalDigest::Missing => Err(UnknownReason::NotPulled),
        LocalDigest::Unavailable => Err(UnknownReason::RuntimeUnavailable),
    }
}

fn compare(image: &str, local: &str, remote: String) -> ImageCheck {
    if remote == local {
        return ImageCheck::Current;
    }
    tracing::info!(
        target: "containers.image_update",
        %image,
        %local,
        %remote,
        "sandbox image update available"
    );
    ImageCheck::Available(ImageUpdate {
        image: image.to_string(),
        remote_digest: remote,
    })
}

fn registry_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(crate::github::DEFAULT_USER_AGENT)
        .timeout(std::time::Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// `Err` means the registry couldn't be asked (network, auth, timeout), not that the image is
/// current.
pub async fn check_image(
    runtime: crate::containers::ContainerRuntime,
    image: &str,
) -> Result<ImageCheck> {
    let image_owned = image.to_string();
    let pre = tokio::task::spawn_blocking(move || {
        pre_check(&image_owned, || runtime.local_image_digest(&image_owned))
    })
    .await
    .map_err(|e| anyhow!("local image check panicked: {e}"))?;
    let (reference, local) = match pre {
        Ok(pair) => pair,
        Err(reason) => return Ok(ImageCheck::Unknown(reason)),
    };

    let client = registry_client()?;
    let remote = reference.fetch_remote_digest(&client).await?;
    Ok(compare(image, &local, remote))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_local_outcome_maps_to_its_own_state_never_current() {
        const IMAGE: &str = "ghcr.io/agent-of-empires/aoe-sandbox:latest";
        let digest = || LocalDigest::Digest("sha256:aaa".into());
        let cases: [(&str, fn() -> LocalDigest, Option<UnknownReason>); 6] = [
            ("", digest, Some(UnknownReason::Unparseable)),
            ("ubuntu@sha256:0123", digest, Some(UnknownReason::Pinned)),
            (
                IMAGE,
                || LocalDigest::Missing,
                Some(UnknownReason::NotPulled),
            ),
            // Docker Desktop not started: not "not pulled", which a pull would contradict.
            (
                IMAGE,
                || LocalDigest::Unavailable,
                Some(UnknownReason::RuntimeUnavailable),
            ),
            // Apple Container, or an image built locally.
            (
                IMAGE,
                || LocalDigest::NoDigest,
                Some(UnknownReason::NoLocalDigest),
            ),
            (IMAGE, digest, None),
        ];
        for (image, local, unknown) in cases {
            let pre = pre_check(image, local);
            assert_eq!(pre.as_ref().err().copied(), unknown, "{image:?}");
            if let Ok((_, local)) = pre {
                assert_eq!(local, "sha256:aaa");
            }
        }
        assert_eq!(
            compare(IMAGE, "sha256:aaa", "sha256:aaa".into()),
            ImageCheck::Current
        );
        assert_eq!(
            compare(IMAGE, "sha256:aaa", "sha256:bbb".into()),
            ImageCheck::Available(ImageUpdate {
                image: IMAGE.into(),
                remote_digest: "sha256:bbb".into(),
            })
        );
    }

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
    fn pick_repo_digest_only_returns_a_digest_for_the_same_repository() {
        let image = "ghcr.io/agent-of-empires/aoe-sandbox:latest";
        let cases = [
            (
                "ghcr.io/agent-of-empires/aoe-sandbox@sha256:aaa\ndocker.io/library/ubuntu@sha256:bbb\n",
                Some("sha256:aaa"),
            ),
            // Only other repos: no digest to compare, not an unrelated one.
            ("registry.example.com/other/img@sha256:ccc\n", None),
            (
                "docker.io/library/ubuntu@sha256:bbb\nmirror.example.com/agent-of-empires/aoe-sandbox@sha256:ddd\n",
                Some("sha256:ddd"),
            ),
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

    #[test]
    fn bearer_realm_must_be_https_and_not_local() {
        let cases = [
            ("https://auth.docker.io/token", true),
            ("https://ghcr.io/token?x=1", true),
            ("http://auth.example/token", false),
            ("https://localhost/token", false),
            ("https://LOCALHOST:8443/token", false),
            ("https://foo.localhost/token", false),
            ("https://127.0.0.1/token", false),
            ("https://127.42.0.9:5000/token", false),
            ("https://0.0.0.0/token", false),
            ("https://169.254.169.254/latest", false),
            ("https://[::1]/token", false),
            ("https://[::]/token", false),
            ("https://[fe80::1]/token", false),
            ("https://[::ffff:127.0.0.1]/token", false),
            ("https://[::ffff:169.254.169.254]/token", false),
            ("/token", false),
        ];
        for (realm, allowed) in cases {
            let result = validate_bearer_realm(realm);
            assert_eq!(result.is_ok(), allowed, "{realm}");
            if let Err(error) = result {
                assert_eq!(error_code(&error), "registry_bad_response", "{realm}");
            }
        }
    }

    #[test]
    fn errors_map_to_short_codes_without_the_chain() {
        let cases = [
            (RegistryErrorKind::Unreachable, "registry_unreachable"),
            (RegistryErrorKind::Auth, "registry_auth"),
            (RegistryErrorKind::BadResponse, "registry_bad_response"),
        ];
        for (kind, code) in cases {
            let error = fail(kind, anyhow!("tls handshake to 10.0.0.5 failed"));
            assert_eq!(error_code(&error), code);
            assert!(
                format!("{error:#}").contains("10.0.0.5"),
                "chain kept for logs"
            );
        }
        assert_eq!(
            error_code(&anyhow!("local image check panicked")),
            "registry_bad_response"
        );
    }

    /// Serves one canned response per connection and reports each request line.
    async fn serve(
        responses: Vec<String>,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let _ = tx.send(request.lines().next().unwrap_or("").to_string());
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_are_a_bad_response() {
        let (target, mut target_requests) = serve(vec![
            "HTTP/1.1 200 OK\r\nDocker-Content-Digest: sha256:evil\r\nContent-Length: 0\r\n\r\n"
                .into(),
        ])
        .await;
        let (origin, mut origin_requests) = serve(vec![format!(
            "HTTP/1.1 302 Found\r\nLocation: {target}/\r\nContent-Length: 0\r\n\r\n"
        )])
        .await;

        let client = registry_client().unwrap();
        let resp = send(client.get(format!("{origin}/v2/x/manifests/latest")))
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
        assert!(origin_requests.recv().await.is_some());

        let error = ensure_success(resp).unwrap_err();
        assert_eq!(error_code(&error), "registry_bad_response");
        assert!(
            target_requests.try_recv().is_err(),
            "the redirect target was contacted"
        );
    }

    #[tokio::test]
    async fn statuses_and_transport_failures_map_to_codes() {
        let client = registry_client().unwrap();
        for (status, code) in [
            ("401 Unauthorized", "registry_auth"),
            ("403 Forbidden", "registry_auth"),
            ("404 Not Found", "registry_bad_response"),
            ("500 Internal Server Error", "registry_bad_response"),
        ] {
            let (base, _) = serve(vec![format!(
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n"
            )])
            .await;
            let resp = send(client.get(&base)).await.unwrap();
            assert_eq!(
                error_code(&ensure_success(resp).unwrap_err()),
                code,
                "{status}"
            );
        }

        // Nothing listens on a port that was just released.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = send(client.get(format!("http://127.0.0.1:{port}")))
            .await
            .unwrap_err();
        assert_eq!(error_code(&error), "registry_unreachable");
    }
}
