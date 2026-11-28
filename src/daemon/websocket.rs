//! Shared native WebSocket transport and non-reflecting failures.

use std::time::Duration;

use thiserror::Error;
use tokio::net::{TcpStream, UnixStream};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, protocol::frame::coding::CloseCode},
    MaybeTlsStream, WebSocketStream,
};

use super::DaemonEndpoint;
use super::{ApiErrorCode, DaemonClientError};
use crate::acp::client::http::HttpError;
use reqwest::header::{HeaderName, HeaderValue};

#[derive(Debug, Error)]
pub enum WsError {
    #[error("websocket transport failed")]
    Transport,
    #[error("daemon rejected websocket with HTTP {status}")]
    Rejected {
        status: reqwest::StatusCode,
        code: Option<ApiErrorCode>,
    },
    /// The passphrase-login handshake (see `acp::client::passphrase_session`)
    /// failed before the WS upgrade was even attempted.
    #[error("passphrase login failed: {0}")]
    Auth(#[from] HttpError),
    #[error("invalid websocket URL")]
    InvalidUrl,
    #[error(transparent)]
    Daemon(#[from] DaemonClientError),
    #[error("websocket closed unexpectedly (code {0:?})")]
    UnexpectedClose(Option<CloseCode>),
    #[error("failed to parse websocket frame")]
    Parse,
}

impl From<tokio_tungstenite::tungstenite::Error> for WsError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => Self::Rejected {
                status: response.status(),
                code: ApiErrorCode::from_headers(response.status(), response.headers(), false),
            },
            _ => Self::Transport,
        }
    }
}

pub(crate) enum NativeSocket {
    Unix(Box<WebSocketStream<UnixStream>>),
    Tcp(Box<WebSocketStream<MaybeTlsStream<TcpStream>>>),
}

/// Open the native WebSocket, authenticating with the endpoint's bearer
/// token when it has one.
pub(crate) async fn connect(
    endpoint: &DaemonEndpoint,
    path: &str,
    query: Option<&str>,
) -> Result<NativeSocket, WsError> {
    connect_with_headers(endpoint, path, query, &[]).await
}

/// [`connect`], plus headers to add to the upgrade request: a passphrase
/// daemon never mints a bearer token, so its login session travels as a
/// `Cookie` plus a `Sec-WebSocket-Protocol` offering instead.
pub(crate) async fn connect_with_headers(
    endpoint: &DaemonEndpoint,
    path: &str,
    query: Option<&str>,
    extra_headers: &[(HeaderName, HeaderValue)],
) -> Result<NativeSocket, WsError> {
    let token = endpoint.bearer_token();
    let base = super::native_url(&endpoint.base_url)?;
    if token.is_some() && base.scheme() == "http" && !super::is_loopback_url(&base) {
        return Err(DaemonClientError::InsecureCredentialTransport.into());
    }
    let mut url = base.clone();
    url.set_scheme(if base.scheme() == "https" {
        "wss"
    } else {
        "ws"
    })
    .map_err(|_| WsError::InvalidUrl)?;
    url.set_path(&format!("{}{path}", base.path().trim_end_matches('/')));
    url.set_query(query);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| WsError::InvalidUrl)?;
    if let Some(authorization) = super::authorization_header(token)? {
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, authorization);
    }
    for (name, value) in extra_headers {
        request.headers_mut().insert(name.clone(), value.clone());
    }
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(16 * 1024 * 1024))
        .max_frame_size(Some(16 * 1024 * 1024));
    tokio::time::timeout(Duration::from_secs(15), async {
        if let Some(unix) = endpoint.unix_transport() {
            let socket = unix.connect(None).await?;
            let (stream, _) =
                tokio_tungstenite::client_async_with_config(request, socket, Some(config)).await?;
            return Ok(NativeSocket::Unix(Box::new(stream)));
        }
        let (stream, _) = if base.host_str() == Some("localhost") {
            let port = base.port_or_known_default().ok_or(WsError::InvalidUrl)?;
            let tcp = match TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await {
                Ok(stream) => stream,
                Err(_) => TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port))
                    .await
                    .map_err(|_| WsError::Transport)?,
            };
            tokio_tungstenite::client_async_tls_with_config(request, tcp, Some(config), None)
                .await?
        } else {
            tokio_tungstenite::connect_async_with_config(request, Some(config), true).await?
        };
        Ok(NativeSocket::Tcp(Box::new(stream)))
    })
    .await
    .map_err(|_| WsError::Daemon(DaemonClientError::Timeout))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::transport::UnixTransport;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn unix_websocket_sends_no_upgrade_to_a_rejected_peer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let transport = UnixTransport::new(path, |_, _| Err(DaemonClientError::PeerIdentity));
        let endpoint = DaemonEndpoint::local_unix(transport, None);

        tokio::time::timeout(Duration::from_secs(5), async {
            let (result, read) = tokio::join!(connect(&endpoint, "/api/runtime/ws", None), async {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.read(&mut [0]).await.unwrap()
            });
            assert!(matches!(
                result,
                Err(WsError::Daemon(DaemonClientError::PeerIdentity))
            ));
            assert_eq!(read, 0);
        })
        .await
        .expect("Unix WebSocket rejection did not complete");
    }
}
