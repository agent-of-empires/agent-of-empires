//! Shared native WebSocket transport and non-reflecting failures.

use std::time::Duration;

use thiserror::Error;
use tokio::net::{TcpStream, UnixStream};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, protocol::frame::coding::CloseCode},
    MaybeTlsStream, WebSocketStream,
};

use super::{ApiErrorCode, DaemonClientError};
use crate::acp::client::DaemonEndpoint;

#[derive(Debug, Error)]
pub enum WsError {
    #[error("websocket transport failed")]
    Transport,
    #[error("daemon rejected websocket with HTTP {status}")]
    Rejected {
        status: reqwest::StatusCode,
        code: Option<ApiErrorCode>,
    },
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

pub(crate) async fn connect(
    endpoint: &DaemonEndpoint,
    path: &str,
    query: Option<&str>,
) -> Result<NativeSocket, WsError> {
    let token = endpoint.bearer_token();
    let base = super::native_url(&endpoint.base_url)?;
    let authenticated = token.is_some() || endpoint.login().is_some();
    super::ensure_credential_transport(&base, authenticated, endpoint.allows_plaintext())?;
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
    if let Some(login) = endpoint.login() {
        super::insert_login_headers(request.headers_mut(), login)?;
        // The binding rides a subprotocol on upgrades; the server selects
        // `aoe-auth` from the offered list.
        let protocols = format!("aoe-auth, aoe-device.{}", login.binding);
        let mut protocols = reqwest::header::HeaderValue::from_str(&protocols)
            .map_err(|_| DaemonClientError::InvalidBearerToken)?;
        protocols.set_sensitive(true);
        request
            .headers_mut()
            .insert(reqwest::header::SEC_WEBSOCKET_PROTOCOL, protocols);
    }
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(16 * 1024 * 1024))
        .max_frame_size(Some(16 * 1024 * 1024));
    tokio::time::timeout(Duration::from_secs(15), async {
        if let Some(path) = endpoint.unix_path() {
            let socket = super::transport::connect_unix(path).await?;
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
