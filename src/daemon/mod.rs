//! Reusable client and shared wire contract for the daemon REST API.

mod wire;

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION};
use reqwest::{StatusCode, Url};
use thiserror::Error;

pub use wire::{
    AcpWorkerState, CleanupDefaults, ContextResumeAvailability, ContextResumeIndeterminateReason,
    ContextResumeUnavailableReason, ListSessionsQuery, PlanSummary, PromptAttachmentKind,
    PromptAttachmentRef, QueuedPromptEntry, SessionResponse, SessionsEnvelope,
    WorkspaceRepoSummary,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const MAX_SUCCESS_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Client for the daemon's session REST API.
///
/// Clones share the underlying reqwest connection pool.
#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    sessions_url: Url,
    authorization: Option<HeaderValue>,
}

impl fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let authenticated = self.authorization.is_some();
        let mut debug = f.debug_struct("DaemonClient");
        if authenticated {
            debug.field("sessions_url", &"<redacted>");
        } else {
            debug.field("sessions_url", &self.sessions_url);
        }
        debug.field("authenticated", &authenticated).finish()
    }
}

/// Failure from constructing or calling a [`DaemonClient`].
#[derive(Debug, Error)]
pub enum DaemonClientError {
    /// The supplied URL is not a usable HTTP daemon base URL.
    #[error("invalid daemon base URL: {reason}")]
    InvalidBaseUrl { reason: &'static str },
    /// The bearer token cannot be represented as an HTTP authorization header.
    #[error("invalid daemon bearer token")]
    InvalidBearerToken,
    /// Bearer authentication was configured for a non-loopback plaintext URL.
    #[error("daemon bearer token requires HTTPS or a loopback HTTP URL")]
    InsecureBearerTransport,
    /// The default reqwest client could not be built.
    #[error("failed to build daemon HTTP client: {0}")]
    ClientBuild(#[source] reqwest::Error),
    /// Sending the request or reading its response failed.
    #[error("daemon transport error: {0}")]
    Transport(#[source] reqwest::Error),
    /// The daemon returned a non-successful HTTP status. Authenticated
    /// responses omit the body so transformed credentials cannot be reflected.
    #[error("daemon returned HTTP {status}: {body}")]
    Status {
        status: StatusCode,
        body: String,
        truncated: bool,
    },
    /// A successful response exceeded the bounded sessions-envelope limit.
    #[error("daemon response exceeded the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },
    /// A successful response did not match the shared wire contract.
    #[error("failed to decode daemon response: {0}")]
    Decode(#[source] serde_json::Error),
    /// An authenticated daemon response did not match the wire contract.
    #[error("failed to decode authenticated daemon response")]
    AuthenticatedDecode,
}

impl DaemonClient {
    /// Build a client with a 15-second timeout and redirects disabled.
    ///
    /// Bearer authentication requires HTTPS except for loopback HTTP endpoints.
    pub fn new(base_url: &str, bearer_token: Option<&str>) -> Result<Self, DaemonClientError> {
        let sessions_url = sessions_url(base_url)?;
        let authorization = authorization_header(bearer_token)?;
        if authorization.is_some()
            && sessions_url.scheme() == "http"
            && !is_loopback_url(&sessions_url)
        {
            return Err(DaemonClientError::InsecureBearerTransport);
        }
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .user_agent(concat!("aoe-daemon-client/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(DaemonClientError::ClientBuild)?;
        Ok(Self {
            http,
            sessions_url,
            authorization,
        })
    }

    /// Fetch the sessions endpoint, optionally filtered by session state.
    pub async fn list_sessions(
        &self,
        state: Option<crate::session::SessionScope>,
    ) -> Result<SessionsEnvelope, DaemonClientError> {
        let query = ListSessionsQuery { state };
        let mut request = self
            .http
            .get(self.sessions_url.clone())
            .query(&query)
            .timeout(DEFAULT_TIMEOUT);
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }
        let request = request
            .build()
            .map_err(|error| self.transport_error(error))?;
        let mut response = self
            .http
            .execute(request)
            .await
            .map_err(|error| self.transport_error(error))?;

        let status = response.status();
        if !status.is_success() {
            let (body, truncated) = self.read_error_body(&mut response).await?;
            return Err(DaemonClientError::Status {
                status,
                body,
                truncated,
            });
        }

        let body = self
            .read_bounded_body(&mut response, MAX_SUCCESS_BODY_BYTES)
            .await?;
        serde_json::from_slice(&body).map_err(|error| {
            if self.authorization.is_some() {
                DaemonClientError::AuthenticatedDecode
            } else {
                DaemonClientError::Decode(error)
            }
        })
    }

    async fn read_error_body(
        &self,
        response: &mut reqwest::Response,
    ) -> Result<(String, bool), DaemonClientError> {
        let mut bytes = Vec::with_capacity(MAX_ERROR_BODY_BYTES);
        let mut truncated = false;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| self.transport_error(error))?
        {
            let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(bytes.len());
            if chunk.len() > remaining {
                bytes.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }

        let mut body = String::from_utf8_lossy(&bytes).into_owned();
        if body.len() > MAX_ERROR_BODY_BYTES {
            truncate_utf8(&mut body, MAX_ERROR_BODY_BYTES);
            truncated = true;
        }
        if self.authorization.is_some() {
            body.clear();
        }
        Ok((body, truncated))
    }

    // An authenticated base path may itself contain credential material.
    fn transport_error(&self, error: reqwest::Error) -> DaemonClientError {
        let error = if self.authorization.is_some() {
            error.without_url()
        } else {
            error
        };
        DaemonClientError::Transport(error)
    }

    async fn read_bounded_body(
        &self,
        response: &mut reqwest::Response,
        limit: usize,
    ) -> Result<Vec<u8>, DaemonClientError> {
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(DaemonClientError::ResponseTooLarge { limit });
        }

        let capacity = response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(limit);
        let mut bytes = Vec::with_capacity(capacity);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| self.transport_error(error))?
        {
            let remaining = limit.saturating_sub(bytes.len());
            if chunk.len() > remaining {
                return Err(DaemonClientError::ResponseTooLarge { limit });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

pub(crate) fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let ip_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || ip_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn sessions_url(base_url: &str) -> Result<Url, DaemonClientError> {
    let mut base = Url::parse(base_url).map_err(|_| DaemonClientError::InvalidBaseUrl {
        reason: "could not parse URL",
    })?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err(DaemonClientError::InvalidBaseUrl {
            reason: "scheme must be http or https",
        });
    }
    if base.host().is_none() {
        return Err(DaemonClientError::InvalidBaseUrl {
            reason: "URL must include a host",
        });
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(DaemonClientError::InvalidBaseUrl {
            reason: "URL must not include credentials",
        });
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err(DaemonClientError::InvalidBaseUrl {
            reason: "URL must not include a query or fragment",
        });
    }
    if !base.path().ends_with('/') {
        base.path_segments_mut()
            .map_err(|_| DaemonClientError::InvalidBaseUrl {
                reason: "URL cannot be used as a base",
            })?
            .push("");
    }
    base.join("api/sessions")
        .map_err(|_| DaemonClientError::InvalidBaseUrl {
            reason: "could not join sessions endpoint",
        })
}

fn authorization_header(
    bearer_token: Option<&str>,
) -> Result<Option<HeaderValue>, DaemonClientError> {
    let Some(token) = bearer_token else {
        return Ok(None);
    };
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(DaemonClientError::InvalidBearerToken);
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| DaemonClientError::InvalidBearerToken)?;
    value.set_sensitive(true);
    Ok(Some(value))
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    let mut boundary = max_bytes.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}
