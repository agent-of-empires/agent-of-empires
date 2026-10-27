//! Validate and render a daemon snapshot after a two-frame WebSocket exchange.
//! Production reads acquire a local Daemon only after proven publication absence.

/// Shared with the producer's contract checks against the real client decoder.
pub(crate) mod dto;
mod endpoint;
#[cfg(test)]
pub(crate) mod pack;
#[cfg(test)]
mod pack_tests;
mod render;
/// Linux descriptor admission shared with the local publisher.
#[cfg(target_os = "linux")]
pub(crate) mod uds;

use std::time::Duration;
use tokio::time::Instant;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::{CapacityError, Error as WsError, ProtocolError};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, WebSocketConfig};
use tokio_tungstenite::tungstenite::Utf8Bytes;

use self::dto::{
    parse_hello, parse_snapshot, validate_cross_message, validate_hello, validate_snapshot,
    HelloParseError,
};
/// Re-exported so an out-of-process harness can aim a read at a chosen
/// endpoint; production callers use [`read_request_source`].
pub use self::endpoint::ReadRequestSource;
use self::endpoint::SelectedEndpoint;
#[cfg(target_os = "linux")]
use self::uds::UdsIdentity;
use super::group::GroupListArgs;
use super::list::ListArgs;
use super::project::ProjectListArgs;
use super::session::ShowArgs;
use super::status::StatusArgs;
use super::{Cli, Commands};

// Client establishment and exchange share one deadline; the server uses the same duration.
use crate::server::runtime_ws::CONNECTION_BUDGET;
const CLOSE_BUDGET: Duration = Duration::from_millis(200);
pub(crate) const APPLICATION_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
pub enum ScopedCommand<'a> {
    List(&'a ListArgs),
    Status(&'a StatusArgs),
    Show(&'a ShowArgs),
    ListTrash,
    GroupList(&'a GroupListArgs),
    Profile,
    ProjectList(&'a ProjectListArgs),
}

pub fn classify(command: Option<&Commands>) -> Option<ScopedCommand<'_>> {
    match command? {
        Commands::List(args) => Some(ScopedCommand::List(args)),
        Commands::Status(args) => Some(ScopedCommand::Status(args)),
        Commands::Session {
            command: super::session::SessionCommands::Show(args),
        } => Some(ScopedCommand::Show(args)),
        Commands::Session {
            command: super::session::SessionCommands::ListTrash,
        } => Some(ScopedCommand::ListTrash),
        Commands::Group {
            command: super::group::GroupCommands::List(args),
        } => Some(ScopedCommand::GroupList(args)),
        Commands::Profile {
            command: Some(super::profile::ProfileCommands::List) | None,
        } => Some(ScopedCommand::Profile),
        Commands::Project {
            command: super::project::ProjectCommands::List(args),
        } => Some(ScopedCommand::ProjectList(args)),
        _ => None,
    }
}

/// Stable failure codes emitted by scoped runtime reads.
pub(crate) const EMITTABLE_CODES: &[&str] = &[
    // `parser_error` is clap's, raised before any read begins.
    "parser_error",
    "anchored_alias_unavailable",
    "cityhall_mode",
    "close_timeout",
    "connection_closed",
    "default_missing",
    "establishment_timeout",
    "freshness_unavailable",
    "frame_limit",
    "health_degraded",
    "invalid_endpoint",
    "invalid_token",
    "marker_identity",
    "marker_invalid",
    "marker_missing",
    "peer_identity",
    "profile_missing",
    "protocol_mismatch",
    "publisher_absent",
    "renderer_internal",
    "schema_invalid",
    "session_ambiguous",
    "session_missing",
    "socket_identity",
    "server_error",
    "unauthorized",
    "unavailable",
];

/// The code a caller-chosen-exit refusal carries: the renderer refused on the
/// user's own state, not on the wire.
pub const RENDERER_INTERNAL: &str = "renderer_internal";

/// Format a runtime refusal code for CLI stderr.
pub fn read_sentence(code: &str) -> String {
    if code == "protocol_mismatch" {
        return "daemon read: protocol_mismatch\nClient and daemon protocol versions differ. Restart the target daemon on its host with `aoe serve --restart` (or its service manager); update that host's aoe installation if needed.\n".into();
    }
    if code == "cityhall_mode" {
        return "daemon read: cityhall_mode\nCityHall mode disables runtime CLI reads on this Daemon. This is a mode restriction, not an authentication failure. Check the deployment mode with its administrator; retrying authentication will not enable these reads.\n".into();
    }
    if matches!(code, "peer_identity" | "marker_identity") {
        return format!("daemon read: {code}\nThe Daemon identity could not be verified. Check the expected Daemon, app/build namespace and CLI/Daemon versions. For local connections, inspect socket and publication ownership through the Daemon's manager. Do not remove publication artifacts or start another Daemon while ownership is uncertain.\n");
    }
    if matches!(
        code,
        "publisher_absent" | "establishment_timeout" | "marker_missing" | "unavailable"
    ) {
        return format!("daemon read: {code}\nReconnect to the selected daemon and retry. Check --daemon-url or AOE_DAEMON_URL; local storage is never used as a fallback.\n");
    }
    format!("daemon read: {code}\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscoveryBoundary {
    Refused,
    LocalAbsent,
}

#[derive(Debug)]
pub(crate) struct ReadFailure {
    code: &'static str,
    exit: i32,
    /// Exact user-facing diagnosis when a stable wire code cannot name the cause.
    exact: Option<String>,
    attempt_close: bool,
    boundary: DiscoveryBoundary,
}

impl ReadFailure {
    pub(crate) fn pre(code: &'static str) -> Self {
        Self::refusal(code, 2, false)
    }

    pub(crate) fn post(code: &'static str) -> Self {
        Self::refusal(code, 4, true)
    }

    pub(crate) fn local_absence() -> Self {
        let mut failure = Self::pre("marker_missing");
        failure.boundary = DiscoveryBoundary::LocalAbsent;
        failure
    }

    pub(crate) fn post_no_close(code: &'static str) -> Self {
        Self::refusal(code, 4, false)
    }

    /// Pre-admission diagnosis; it does not authorize a connection.
    pub(crate) fn pre_exact(code: &'static str, message: impl Into<String>) -> Self {
        let mut failure = Self::refusal(code, 2, false);
        failure.exact = Some(message.into());
        failure
    }
    fn refusal(code: &'static str, exit: i32, attempt_close: bool) -> Self {
        debug_assert!(
            EMITTABLE_CODES.contains(&code),
            "{code} is not in the emittable code set"
        );
        Self {
            code,
            exit,
            exact: None,
            attempt_close,
            boundary: DiscoveryBoundary::Refused,
        }
    }

    /// Preserve the renderer's chosen exit status and exact message.
    pub(crate) fn exit(exit: i32, message: impl Into<String>) -> Self {
        Self::exit_with(exit, message.into())
    }

    /// User-state refusal with its stable code and local CLI exit/message.
    pub(crate) fn refuse(code: &'static str, exit: i32, message: impl Into<String>) -> Self {
        let mut failure = Self::refusal(code, exit, true);
        failure.exact = Some(message.into());
        failure
    }

    fn exit_with(exit: i32, message: String) -> Self {
        Self::refuse(RENDERER_INTERNAL, exit, message)
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn code(&self) -> &str {
        self.code
    }
}

#[derive(Debug)]
pub struct ReadOutcome {
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub exit: i32,
}

impl From<ReadFailure> for ReadOutcome {
    fn from(error: ReadFailure) -> Self {
        let message = match error.exact {
            Some(exact) => exact,
            None => read_sentence(error.code),
        };
        Self {
            stdout: None,
            stderr: Some(message),
            exit: error.exit,
        }
    }
}

pub fn read_request_source(cli: &Cli) -> ReadRequestSource {
    endpoint::read_request_source(cli)
}

/// Read an already published endpoint without acquiring or starting a Daemon.
pub async fn attempt(command: ScopedCommand<'_>, source: &ReadRequestSource) -> ReadOutcome {
    finish(execute_inner(command, source).await, source).await
}

/// Acquire a local Daemon only after definitive absence, then read it once.
pub async fn run(command: ScopedCommand<'_>, source: &ReadRequestSource) -> ReadOutcome {
    let result = execute_inner(command, source).await;
    let result = match result {
        Err(error)
            if error.boundary == DiscoveryBoundary::LocalAbsent
                && source.explicit_url.is_none()
                && !source.env_url_is_set() =>
        {
            acquire_local_read(command, source).await
        }
        result => result,
    };
    finish(result, source).await
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn acquire_local_read(
    command: ScopedCommand<'_>,
    source: &ReadRequestSource,
) -> Result<render::Projection, ReadFailure> {
    let endpoint = crate::acp::client::daemon_manager::require_local_read_daemon()
        .await
        .map_err(|error| {
            ReadFailure::pre_exact(
                "unavailable",
                format!("daemon read: unavailable\n{error}\n"),
            )
        })?;
    #[cfg(target_os = "linux")]
    {
        drop(endpoint);
        execute_inner(command, source).await
    }
    #[cfg(target_os = "macos")]
    {
        execute_local_api(command, source, &endpoint).await
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn acquire_local_read(
    _command: ScopedCommand<'_>,
    _source: &ReadRequestSource,
) -> Result<render::Projection, ReadFailure> {
    Err(ReadFailure::pre_exact(
        "unavailable",
        "daemon read: unavailable\nThis client has no local Daemon backend. Set --daemon-url <url> or AOE_DAEMON_URL to an existing daemon and configure its authentication.\n",
    ))
}

async fn finish(
    result: Result<render::Projection, ReadFailure>,
    source: &ReadRequestSource,
) -> ReadOutcome {
    match result {
        Ok(projection) => {
            let notice = if projection.session_table {
                crate::update::update_notice().await
            } else {
                None
            };
            let stderr = match (projection.stderr, notice) {
                (Some(mut diagnostics), Some(notice)) => {
                    diagnostics.push_str(&notice);
                    Some(diagnostics)
                }
                (diagnostics, None) => diagnostics,
                (None, notice) => notice,
            };
            ReadOutcome {
                stdout: Some(projection.stdout),
                stderr,
                exit: projection.exit,
            }
        }
        Err(mut error) => {
            if error.exact.is_none()
                && source.explicit_url.is_none()
                && !source.env_url_is_set()
                && matches!(
                    error.code,
                    "publisher_absent" | "establishment_timeout" | "marker_missing" | "unavailable"
                )
            {
                error.exact = Some(format!(
                    "daemon read: {}\nThe local Daemon's runtime read is unavailable. \
                     Check its mode, CLI/Daemon versions and publication diagnostics. \
                     CityHall mode disables runtime reads. When appropriate, restart the local \
                     Daemon through its service manager or `aoe serve --restart`, then retry. \
                     Local storage is never used as a fallback.\n",
                    error.code
                ));
            }
            error.into()
        }
    }
}

async fn execute_inner(
    command: ScopedCommand<'_>,
    source: &ReadRequestSource,
) -> Result<render::Projection, ReadFailure> {
    let endpoint = endpoint::select_endpoint(source)?;
    let establishment_deadline = Instant::now() + CONNECTION_BUDGET;
    match endpoint {
        SelectedEndpoint::Local => {
            #[cfg(target_os = "linux")]
            {
                let connection = uds::connect(establishment_deadline).await?;
                let request = "ws://localhost/api/runtime/read/ws"
                    .into_client_request()
                    .map_err(|_| ReadFailure::post("unavailable"))?;
                let exchange = connection.upgrade(request).await?;
                let uds::UdsExchange {
                    stream,
                    identity,
                    home,
                    deadline,
                    _admission,
                } = exchange;
                exchange_stream(
                    stream,
                    deadline,
                    ExpectedPeer::Local(identity),
                    Some(home.as_path()),
                    command,
                    source,
                )
                .await
            }
            // No local publisher on this platform.
            #[cfg(not(target_os = "linux"))]
            {
                let _ = establishment_deadline;
                Err(ReadFailure::local_absence())
            }
        }
        SelectedEndpoint::Http { request } => {
            let local_home = loopback_home(&request);
            // Preserve the environment target's shorter establishment budget.
            let connect_deadline = if source.explicit_url.is_none() {
                establishment_deadline.min(Instant::now() + Duration::from_secs(1))
            } else {
                establishment_deadline
            };
            let stream = tokio::time::timeout_at(connect_deadline, connect_http(request.uri()))
                .await
                .map_err(|_| ReadFailure::pre("establishment_timeout"))?
                .map_err(|_| ReadFailure::post("publisher_absent"))?;
            let stream = upgrade_http(*request, stream, source, establishment_deadline).await?;
            exchange_stream(
                stream,
                establishment_deadline,
                ExpectedPeer::Remote,
                local_home.as_deref(),
                command,
                source,
            )
            .await
        }
    }
}

#[cfg(any(target_os = "macos", all(test, unix)))]
async fn execute_local_api(
    command: ScopedCommand<'_>,
    source: &ReadRequestSource,
    endpoint: &crate::acp::client::DaemonEndpoint,
) -> Result<render::Projection, ReadFailure> {
    let path = endpoint
        .unix_path()
        .ok_or_else(|| ReadFailure::pre("unavailable"))?;
    let deadline = Instant::now() + CONNECTION_BUDGET;
    let stream = tokio::time::timeout_at(deadline, async {
        let stream =
            crate::daemon::transport::connect_unix(path)
                .await
                .map_err(|error| match error {
                    crate::daemon::DaemonClientError::PeerIdentity => {
                        ReadFailure::pre("peer_identity")
                    }
                    _ => ReadFailure::pre("unavailable"),
                })?;
        let request = "ws://localhost/api/runtime/read/ws"
            .into_client_request()
            .map_err(|_| ReadFailure::pre("invalid_endpoint"))?;
        tokio_tungstenite::client_async_with_config(request, stream, Some(websocket_config()))
            .await
            .map(|(stream, _)| stream)
            .map_err(map_upgrade_error)
    })
    .await
    .map_err(|_| ReadFailure::pre("establishment_timeout"))??;
    let home = dirs::home_dir();
    exchange_stream(
        stream,
        deadline,
        ExpectedPeer::ApiUnix,
        home.as_deref(),
        command,
        source,
    )
    .await
}

async fn connect_http(
    uri: &tokio_tungstenite::tungstenite::http::Uri,
) -> Result<tokio::net::TcpStream, std::io::Error> {
    let (host, port) = host_port(uri)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid endpoint"))?;
    if host.eq_ignore_ascii_case("localhost") {
        let addresses = [
            std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port),
            std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), port),
        ];
        tokio::net::TcpStream::connect(addresses.as_slice()).await
    } else {
        tokio::net::TcpStream::connect((host, port)).await
    }
}

async fn upgrade_http(
    mut request: tokio_tungstenite::tungstenite::handshake::client::Request,
    mut stream: tokio::net::TcpStream,
    source: &ReadRequestSource,
    deadline: Instant,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ReadFailure,
> {
    use crate::acp::client::passphrase_session::{self, PassphraseSessionCache};
    use crate::acp::client::{DaemonEndpoint, Source};

    let peer = stream
        .peer_addr()
        .map_err(|_| ReadFailure::post("unavailable"))?;
    let had_bearer = request.headers().contains_key("Authorization");
    let auth = if std::env::var_os("AOE_DAEMON_PASSPHRASE").is_some() {
        let uri = request.uri();
        let path = uri
            .path()
            .strip_suffix("/api/runtime/read/ws")
            .ok_or_else(|| ReadFailure::pre("invalid_endpoint"))?;
        let base = if uri.scheme_str() == Some("wss") {
            format!(
                "https://{}{path}",
                uri.authority()
                    .ok_or_else(|| ReadFailure::pre("invalid_endpoint"))?
            )
        } else {
            // Login uses the connected loopback address, never a second localhost lookup.
            format!("http://{peer}{path}")
        };
        let bearer = request
            .headers()
            .get("Authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::to_string);
        let endpoint = DaemonEndpoint::new(base, bearer, Source::Env);
        endpoint
            .resolved_passphrase()
            .is_some()
            .then(|| (endpoint, PassphraseSessionCache::default()))
    } else {
        None
    };
    // A bearer goes first. A 401 on it falls back to one passphrase login, which then
    // replaces the bearer, the same order the HTTP client uses.
    let mut login_now = !had_bearer;
    let mut refreshed = false;
    loop {
        let mut had_cached_session = false;
        if let Some((endpoint, cache)) = &auth {
            let session = match cache.get(endpoint) {
                Some(session) => {
                    had_cached_session = true;
                    Some(session)
                }
                None if login_now => Some(
                    tokio::time::timeout_at(deadline, passphrase_session::login(endpoint, cache))
                        .await
                        .map_err(|_| ReadFailure::post("unavailable"))?
                        .map_err(map_login_error)?,
                ),
                None => None,
            };
            if let Some(session) = session {
                request.headers_mut().remove("Authorization");
                request.headers_mut().insert(
                    "Cookie",
                    session
                        .cookie
                        .parse()
                        .map_err(|_| ReadFailure::post("unauthorized"))?,
                );
                request.headers_mut().insert(
                    "X-Aoe-Device-Binding",
                    session
                        .binding_secret
                        .parse()
                        .map_err(|_| ReadFailure::post("unauthorized"))?,
                );
            }
        }
        let result = tokio::time::timeout_at(
            deadline,
            tokio_tungstenite::client_async_tls_with_config(
                request,
                stream,
                Some(websocket_config()),
                None,
            ),
        )
        .await
        .map_err(|_| ReadFailure::post("unavailable"))?;
        match result {
            Ok((stream, _)) => return Ok(stream),
            Err(WsError::Http(response))
                if response.status().as_u16() == 401
                    && auth.is_some()
                    && ((had_cached_session && !refreshed) || !login_now) =>
            {
                let (endpoint, cache) = auth.as_ref().expect("passphrase auth");
                if had_cached_session {
                    cache.invalidate(endpoint);
                    refreshed = true;
                }
                login_now = true;
                request = match endpoint::select_endpoint(source)? {
                    SelectedEndpoint::Http { request } => *request,
                    SelectedEndpoint::Local => unreachable!("captured HTTP source"),
                };
                stream = tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect(peer))
                    .await
                    .map_err(|_| ReadFailure::post("unavailable"))?
                    .map_err(|_| ReadFailure::post("unavailable"))?;
            }
            Err(error) => return Err(map_upgrade_error(error)),
        }
    }
}

fn map_login_error(error: crate::acp::client::HttpError) -> ReadFailure {
    use crate::acp::client::HttpError;
    match error {
        HttpError::Unauthorized => map_http_status(401),
        HttpError::Server { status, .. } => map_http_status(status.as_u16()),
        _ => ReadFailure::post("unavailable"),
    }
}

fn map_http_status(status: u16) -> ReadFailure {
    if matches!(status, 401 | 403) {
        ReadFailure::post("unauthorized")
    } else {
        ReadFailure::post("server_error")
    }
}

/// Connect target with IPv6 brackets removed and the scheme's default port.
fn host_port(uri: &tokio_tungstenite::tungstenite::http::Uri) -> Result<(&str, u16), ReadFailure> {
    let host = uri
        .host()
        .map(endpoint::unbracketed)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| ReadFailure::pre("invalid_endpoint"))?;
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("wss") | Some("https") => 443,
        _ => 80,
    });
    Ok((host, port))
}

/// Collapse this host’s paths only for literals and the localhost name pinned by connect_http.
fn loopback_home(
    request: &tokio_tungstenite::tungstenite::handshake::client::Request,
) -> Option<std::path::PathBuf> {
    crate::daemon::is_loopback_host(request.uri().host()?)
        .then(dirs::home_dir)
        .flatten()
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(APPLICATION_LIMIT))
        .max_frame_size(Some(APPLICATION_LIMIT))
}

#[derive(Debug)]
enum ExpectedPeer {
    Remote,
    #[cfg(any(target_os = "macos", all(test, unix)))]
    ApiUnix,
    #[cfg(target_os = "linux")]
    Local(UdsIdentity),
}

async fn exchange_stream<S>(
    stream: tokio_tungstenite::WebSocketStream<S>,
    deadline: Instant,
    expected: ExpectedPeer,
    local_home: Option<&std::path::Path>,
    command: ScopedCommand<'_>,
    source: &ReadRequestSource,
) -> Result<render::Projection, ReadFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::time::timeout_at(
        deadline,
        exchange_inner(stream, deadline, expected, local_home, command, source),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ReadFailure::post("connection_closed")),
    }
}

async fn exchange_inner<S>(
    mut stream: tokio_tungstenite::WebSocketStream<S>,
    deadline: Instant,
    expected: ExpectedPeer,
    local_home: Option<&std::path::Path>,
    command: ScopedCommand<'_>,
    source: &ReadRequestSource,
) -> Result<render::Projection, ReadFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    #[cfg(any(target_os = "macos", all(test, unix)))]
    let server_closes = matches!(&expected, ExpectedPeer::ApiUnix);
    #[cfg(not(any(target_os = "macos", all(test, unix))))]
    let server_closes = false;
    let hello_text = match read_application(&mut stream).await {
        Ok(text) => text,
        Err(error) => return Err(error),
    };
    let hello = match parse_hello(hello_text.as_bytes()) {
        Ok(hello) => hello,
        Err(HelloParseError::ProtocolVersion) => {
            return Err(ReadFailure::post_no_close("protocol_mismatch"))
        }
        Err(HelloParseError::Schema) => return Err(ReadFailure::post_no_close("schema_invalid")),
    };
    if let Err(code) = validate_hello(&hello) {
        return Err(ReadFailure::post_no_close(code));
    }
    // The local transport proves the peer it reached is the publisher whose
    // markers it admitted; a named endpoint has no such claim to check.
    #[cfg(target_os = "linux")]
    if let ExpectedPeer::Local(identity) = &expected {
        if hello.namespace != identity.namespace
            || hello.prebind_instance_id != identity.prebind_instance_id
            || hello.runtime_instance_id != identity.runtime_instance_id
            || hello.runtime_epoch != identity.runtime_epoch
            || hello.owner.uid != Some(identity.owner_uid)
        {
            return Err(ReadFailure::post_no_close("peer_identity"));
        }
    }

    let snapshot_text = match read_application(&mut stream).await {
        Ok(text) => text,
        Err(error) => return Err(error),
    };
    let snapshot = match parse_snapshot(snapshot_text.as_bytes()) {
        Ok(snapshot) => snapshot,
        Err(()) => {
            return finish_with_close(
                &mut stream,
                deadline,
                Err(ReadFailure::post("schema_invalid")),
                server_closes,
            )
            .await
        }
    };
    if let Err(code) = validate_snapshot(&snapshot) {
        return finish_with_close(
            &mut stream,
            deadline,
            Err(ReadFailure::post(code)),
            server_closes,
        )
        .await;
    }
    let local_uid = match &expected {
        ExpectedPeer::Remote => None,
        #[cfg(any(target_os = "macos", all(test, unix)))]
        ExpectedPeer::ApiUnix => None,
        #[cfg(target_os = "linux")]
        ExpectedPeer::Local(identity) => Some(identity.owner_uid),
    };
    if let Err(code) = validate_cross_message(&hello, &snapshot, local_uid) {
        return finish_with_close(
            &mut stream,
            deadline,
            Err(ReadFailure::post(code)),
            server_closes,
        )
        .await;
    }
    let projection = render::evaluate(&command, &snapshot, source, local_home);
    finish_with_close(&mut stream, deadline, projection, server_closes).await
}

async fn read_application<S>(
    stream: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<Utf8Bytes, ReadFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => return Ok(text),
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Ping(_))) => {
                // tungstenite queues exactly one identical Pong and flushes it on the next I/O.
            }
            Some(Ok(Message::Close(_))) => {
                return Err(ReadFailure::post("connection_closed"));
            }
            Some(Ok(Message::Binary(_))) => {
                return Err(ReadFailure::post("schema_invalid"));
            }
            Some(Ok(Message::Frame(_))) => {
                return Err(ReadFailure::post("schema_invalid"));
            }
            Some(Err(WsError::Capacity(
                CapacityError::MessageTooLong { .. } | CapacityError::TooManyHeaders,
            ))) => return Err(ReadFailure::post("frame_limit")),
            Some(Err(WsError::Protocol(ProtocolError::ControlFrameTooBig))) => {
                return Err(ReadFailure::post("frame_limit"))
            }
            Some(Err(
                WsError::ConnectionClosed
                | WsError::AlreadyClosed
                | WsError::Io(_)
                | WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake),
            )) => return Err(peer_gone()),
            Some(Err(WsError::Protocol(_)))
            | Some(Err(WsError::Utf8(_)))
            | Some(Err(WsError::AttackAttempt)) => return Err(ReadFailure::post("schema_invalid")),
            Some(Err(_)) => return Err(ReadFailure::post("unavailable")),
            None => return Err(peer_gone()),
        }
    }
}

/// A peer that is already gone: the connection is closed, so there is nobody
/// left to send a close frame to and the refusal stands on its own.
fn peer_gone() -> ReadFailure {
    let mut failure = ReadFailure::post("connection_closed");
    failure.attempt_close = false;
    failure
}

/// Close failure replaces success, never the refusal that already caused the close.
async fn finish_with_close<S>(
    stream: &mut tokio_tungstenite::WebSocketStream<S>,
    deadline: Instant,
    result: Result<render::Projection, ReadFailure>,
    server_closes: bool,
) -> Result<render::Projection, ReadFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if result.as_ref().is_err_and(|error| !error.attempt_close) {
        return result;
    }
    let closed = if server_closes {
        close_from_server(stream, deadline).await
    } else {
        close_local(stream, deadline).await
    };
    match (result, closed) {
        (Err(error), _) => Err(error),
        (Ok(projection), Ok(())) => Ok(projection),
        (Ok(_), Err(close)) => Err(close),
    }
}

async fn close_from_server<S>(
    stream: &mut tokio_tungstenite::WebSocketStream<S>,
    deadline: Instant,
) -> Result<(), ReadFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let budget = CLOSE_BUDGET.min(deadline.saturating_duration_since(Instant::now()));
    tokio::time::timeout(budget, async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Close(_))) => return Ok(()),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return Err(ReadFailure::post("close_timeout")),
            }
        }
    })
    .await
    .map_err(|_| ReadFailure::post("close_timeout"))?
}

async fn close_local<S>(
    stream: &mut tokio_tungstenite::WebSocketStream<S>,
    deadline: Instant,
) -> Result<(), ReadFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let now = Instant::now();
    if now >= deadline {
        return Err(ReadFailure::post("connection_closed"));
    }
    let budget = CLOSE_BUDGET.min(deadline.saturating_duration_since(now));
    let frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "".into(),
    };
    match tokio::time::timeout(budget, stream.send(Message::Close(Some(frame)))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(ReadFailure::post("close_timeout")),
    }
}

fn map_upgrade_error(error: WsError) -> ReadFailure {
    use crate::daemon::ApiErrorCode;

    match error {
        WsError::Http(response) if response.status().as_u16() == 404 => {
            let mut failure = ReadFailure::post("server_error");
            failure.exact = Some(
                "daemon read: server_error\nThe runtime-read route was not found. Check the selected URL and proxy routing. If the daemon predates this CLI, update it and restart it on its host with `aoe serve --restart` (or its service manager).\n".into(),
            );
            failure
        }
        WsError::Http(response)
            if ApiErrorCode::from_headers(response.status(), response.headers(), false)
                == Some(ApiErrorCode::CityhallMode) =>
        {
            ReadFailure::post("cityhall_mode")
        }
        WsError::Http(response) => map_http_status(response.status().as_u16()),
        _ => ReadFailure::post("unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::ffi::OsString;

    #[tokio::test]
    #[cfg(unix)]
    #[serial_test::serial]
    async fn local_api_reads_keep_unix_transport_and_remote_wire_shape() {
        use crate::server::test_support::{
            accept_runtime_read_cache_for_test, build_router_for_test,
            build_test_app_state_with_policy, seed_instances_on_disk_for_test,
            serve_owner_router_for_test,
        };
        let home = tempfile::TempDir::new_in("/tmp").unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(home.path());
        let mut row = crate::session::Instance::new("API UDS only", "/repo");
        row.id = "api-uds-row".into();
        row.source_profile = "main".into();
        seed_instances_on_disk_for_test("main", vec![row.clone()]);
        let state =
            build_test_app_state_with_policy(vec![row], vec!["localhost".into()], Vec::new(), None);
        accept_runtime_read_cache_for_test(&state).await;
        let path = crate::daemon::transport::local_socket_path().unwrap();
        let server = serve_owner_router_for_test(&path, build_router_for_test(state.clone())).await;
        let endpoint = crate::acp::client::DaemonEndpoint::local_unix(path);
        let source = ReadRequestSource {
            explicit_url: None,
            env_url: None,
            token: None,
            explicit_profile: Some("main".into()),
            env_profile: None,
        };
        let cli = Cli::parse_from(["aoe", "list", "--json"]);
        let projection =
            execute_local_api(classify(cli.command.as_ref()).unwrap(), &source, &endpoint)
                .await
                .unwrap();
        let rows: serde_json::Value = serde_json::from_str(&projection.stdout).unwrap();
        assert_eq!(rows[0]["id"], "api-uds-row");
        assert_eq!(rows[0]["title"], "API UDS only");
        assert!(endpoint.dashboard_url().is_none());
        state.shutdown.cancel();
        server.abort();
        let _ = server.await;
    }

    // Localhost is pinned; other TLS names do not establish local provenance.
    #[test]
    fn only_a_loopback_host_answers_with_this_machines_home() {
        assert!(
            dirs::home_dir().is_some(),
            "this machine has a home to collapse to"
        );
        let cases: [(&str, bool); 8] = [
            ("http://127.0.0.1:8080/api/runtime/read/ws", true),
            ("http://[::1]:8080/api/runtime/read/ws", true),
            ("https://[::1]/api/runtime/read/ws", true),
            ("https://localhost:8080/api/runtime/read/ws", true),
            ("https://example.test/api/runtime/read/ws", false),
            ("wss://[fe80::1]/api/runtime/read/ws", false),
            ("wss://[2001:db8::1]/api/runtime/read/ws", false),
            ("http://192.168.1.10:8080/api/runtime/read/ws", false),
        ];
        for (url, expected) in cases {
            let request = tokio_tungstenite::tungstenite::http::Request::builder()
                .uri(url)
                .body(())
                .expect("the uri parses");
            assert_eq!(loopback_home(&request).is_some(), expected, "{url}");
        }
    }

    /// Connect targets omit the authority brackets around IPv6 literals.
    #[test]
    fn a_bracketed_ipv6_endpoint_connects_to_an_address_literal() {
        for (url, host, port) in [
            ("http://[::1]:8080/api/runtime/read/ws", "::1", 8080),
            ("wss://[fe80::1]/api/runtime/read/ws", "fe80::1", 443),
            (
                "http://127.0.0.1:8080/api/runtime/read/ws",
                "127.0.0.1",
                8080,
            ),
            (
                "wss://example.test/api/runtime/read/ws",
                "example.test",
                443,
            ),
        ] {
            let request = url.into_client_request().expect("the URL parses");
            let (resolved, resolved_port) =
                host_port(request.uri()).expect("the endpoint names a host");
            assert_eq!(resolved, host, "{url}");
            assert_eq!(resolved_port, port, "{url}");
            assert!(
                resolved.parse::<std::net::IpAddr>().is_ok()
                    || resolved
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.'),
                "{url} yields a name the connect could resolve"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn invalid_environment_and_explicit_config_both_refuse() {
        let cli = Cli::parse_from(["aoe", "list"]);
        for (url, token, phrase) in [
            ("not a URL", None, ""),
            ("http://192.0.2.1:9", Some("secret"), ""),
            ("http://192.0.2.1:9", None, "phrase"),
            ("https://example.invalid", Some("bad token"), ""),
        ] {
            let _env =
                crate::session::test_support::EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", phrase)]);
            for explicit in [false, true] {
                let source = ReadRequestSource {
                    explicit_url: explicit.then(|| url.into()),
                    env_url: (!explicit).then(|| OsString::from(url)),
                    token: token.map(OsString::from),
                    explicit_profile: None,
                    env_profile: None,
                };
                let outcome = attempt(classify(cli.command.as_ref()).unwrap(), &source).await;
                assert_eq!(outcome.exit, 2);
                assert_eq!(outcome.stdout, None);
            }
        }
    }

    #[test]
    fn classifier_matches_only_exact_scoped_paths() {
        let cases = [
            ("aoe list", true),
            ("aoe ls", true),
            ("aoe status", true),
            ("aoe session show abc", true),
            ("aoe session list-trash", true),
            ("aoe group list", true),
            ("aoe group ls", true),
            ("aoe profile", true),
            ("aoe profile list", true),
            ("aoe profile ls", true),
            ("aoe project list", true),
            ("aoe project ls", true),
            ("aoe agents", false),
            ("aoe profile create x", false),
            ("aoe project add /tmp", false),
            ("aoe session empty-trash", false),
        ];
        for (argv, expected) in cases {
            let cli = Cli::try_parse_from(argv.split(' ')).unwrap();
            assert_eq!(classify(cli.command.as_ref()).is_some(), expected, "{argv}");
        }
    }

    #[test]
    fn scoped_parser_rejects_missing_and_invalid_values() {
        let cases: &[&[&str]] = &[
            &["aoe", "-p"],
            &["aoe", "list", "--state", "bogus"],
            &["aoe", "project", "list", "--scope", "bogus"],
        ];
        for argv in cases {
            assert!(Cli::try_parse_from(*argv).is_err(), "{argv:?}");
        }
        assert!(Cli::try_parse_from(["aoe", "session", "show"]).is_ok());
        assert!(Cli::try_parse_from(["aoe", "session", "list-trash"]).is_ok());
    }

    /// A close failure cannot replace the refusal that prompted it.
    #[tokio::test]
    async fn a_failed_close_never_replaces_the_failure_that_caused_it() {
        for server_closes in [false, true] {
            let mut stream = broken_stream().await;
            let deadline = Instant::now() + CONNECTION_BUDGET;
            for code in ["schema_invalid", "profile_missing", "freshness_unavailable"] {
                let error = finish_with_close(
                    &mut stream,
                    deadline,
                    Err(ReadFailure::post(code)),
                    server_closes,
                )
                .await
                .expect_err("the read failed");
                assert_eq!(error.code(), code, "{code} must survive a broken close");
            }
        }
    }

    /// An otherwise successful read reports its close failure.
    #[tokio::test]
    async fn a_failed_close_is_the_answer_when_the_read_otherwise_succeeded() {
        for server_closes in [false, true] {
            let mut stream = broken_stream().await;
            let deadline = Instant::now() + CONNECTION_BUDGET;
            let projection = render::Projection {
                stdout: "rows\n".into(),
                session_table: false,
                stderr: None,
                exit: 0,
            };
            let error = finish_with_close(&mut stream, deadline, Ok(projection), server_closes)
                .await
                .expect_err("a broken close is not a clean finish");
            assert_eq!(error.code(), "close_timeout");
            assert_eq!(error.exit, 4);
        }
    }

    /// A client-side stream whose peer is gone: every write fails, which is
    /// what a close to a stalled or reset peer looks like from here.
    async fn broken_stream() -> tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream> {
        let (stream, peer) = tokio::io::duplex(1024);
        drop(peer);
        tokio_tungstenite::WebSocketStream::from_raw_socket(
            stream,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn localhost_passphrase_reads_reuse_a_session_and_relogin_after_revocation() {
        use crate::server::test_support;
        use crate::session::test_support::EnvGuard;
        use std::sync::Arc;
        let mut _env = test_support::RuntimeEnvGuard::read_lock();
        let dir = tempfile::tempdir().unwrap();
        _env.bind(dir.path());
        let phrase = "runtime read passphrase evidence";
        let _phrase = EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", phrase)]);
        crate::session::create_profile("main").unwrap();
        let mut row = crate::session::Instance::new("passphrase read evidence", "/repo");
        row.id = "passphrase-read".into();
        row.source_profile = "main".into();
        row.tool = "claude".into();
        test_support::seed_instances_on_disk_for_test("main", vec![row.clone()]);
        let state = test_support::build_test_app_state_with_policy_configured(
            vec![row],
            vec!["localhost".into(), "127.0.0.1".into()],
            Vec::new(),
            None,
            |state| {
                state.login_manager =
                    Arc::new(crate::server::login::LoginManager::new(Some(phrase)));
                state.behind_tunnel = true;
                state.auth_mode = "passphrase";
            },
        );
        test_support::accept_runtime_read_cache_for_test(&state).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app =
            axum::Router::new().nest("/base", test_support::build_router_for_test(state.clone()));
        let shutdown = state.shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
        });
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let recording = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://{}", recording.local_addr().unwrap());
        let proxy_requests = Arc::new(AtomicUsize::new(0));
        let stopped = tokio_util::sync::CancellationToken::new();
        let proxy = tokio::spawn({
            let requests = proxy_requests.clone();
            let stopped = stopped.clone();
            async move {
                loop {
                    let (mut socket, _) = tokio::select! {
                        _ = stopped.cancelled() => break,
                        accepted = recording.accept() => accepted.unwrap(),
                    };
                    let mut request = Vec::new();
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let mut buffer = [0; 1024];
                        let count = socket.read(&mut buffer).await.unwrap();
                        assert_ne!(count, 0);
                        request.extend_from_slice(&buffer[..count]);
                    }
                    requests.fetch_add(1, Ordering::SeqCst);
                    let response: &[u8] = if request.starts_with(b"GET ") {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                    } else {
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                    };
                    socket.write_all(response).await.unwrap();
                }
            }
        });
        let _proxy_exclusions =
            EnvGuard::unset(&["NO_PROXY", "no_proxy", "HTTPS_PROXY", "https_proxy"]);
        let _proxy_env = EnvGuard::set(&[
            ("HTTP_PROXY", proxy_url.as_str()),
            ("http_proxy", proxy_url.as_str()),
            ("ALL_PROXY", proxy_url.as_str()),
            ("all_proxy", proxy_url.as_str()),
        ]);
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/api/health"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(
            proxy_requests.load(Ordering::SeqCst),
            1,
            "positive control: the live proxy receives ordinary loopback HTTP"
        );
        let cli = Cli::parse_from(["aoe", "session", "show", "passphrase-read", "--json"]);
        let source = ReadRequestSource {
            explicit_url: None,
            env_url: Some(format!("http://localhost:{port}/base/").into()),
            token: None,
            explicit_profile: Some("main".into()),
            env_profile: None,
        };
        let mut previous_session = None;
        for (index, revoke) in [false, false, true].into_iter().enumerate() {
            if revoke {
                assert_eq!(state.login_manager.logout_all().await, 1);
            }
            let outcome = attempt(classify(cli.command.as_ref()).unwrap(), &source).await;
            assert_eq!(outcome.exit, 0, "read {index}: {:?}", outcome.stderr);
            let value: serde_json::Value = serde_json::from_str(&outcome.stdout.unwrap()).unwrap();
            assert_eq!(value["id"], "passphrase-read");
            assert_eq!(value["title"], "passphrase read evidence");
            let sessions = state.login_manager.device_snapshot(None).await;
            assert_eq!(
                sessions.len(),
                1,
                "repeated CLI reads must not create extra device sessions"
            );
            if let Some(previous) = previous_session {
                if revoke {
                    assert_ne!(sessions[0].session_id, previous);
                } else {
                    assert_eq!(sessions[0].session_id, previous);
                }
            }
            previous_session = Some(sessions[0].session_id.clone());
        }
        assert_eq!(
            proxy_requests.load(Ordering::SeqCst),
            1,
            "login and cached401 relogin must never reach the environment proxy"
        );
        stopped.cancel();
        tokio::time::timeout(Duration::from_secs(5), proxy)
            .await
            .unwrap()
            .unwrap();
        state.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }

    /// A bearer alone is refused on a token-plus-passphrase daemon, so the read falls back to
    /// one passphrase login (which carries the bearer) instead of failing.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_bearer_refused_for_a_second_factor_falls_back_to_a_passphrase_login() {
        use crate::server::test_support;
        use crate::session::test_support::EnvGuard;
        let mut _env = test_support::RuntimeEnvGuard::read_lock();
        let dir = tempfile::tempdir().unwrap();
        _env.bind(dir.path());
        let phrase = "runtime read second factor";
        let token = "a".repeat(64);
        let _phrase = EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", phrase)]);
        crate::session::create_profile("main").unwrap();
        let mut row = crate::session::Instance::new("second factor evidence", "/repo");
        row.id = "second-factor-read".into();
        row.source_profile = "main".into();
        row.tool = "claude".into();
        test_support::seed_instances_on_disk_for_test("main", vec![row.clone()]);
        let state = test_support::build_test_app_state_with_policy_configured(
            vec![row],
            vec!["localhost".into(), "127.0.0.1".into()],
            Vec::new(),
            Some(token.clone()),
            |state| {
                state.login_manager =
                    std::sync::Arc::new(crate::server::login::LoginManager::new(Some(phrase)));
            },
        );
        test_support::accept_runtime_read_cache_for_test(&state).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app =
            axum::Router::new().nest("/base", test_support::build_router_for_test(state.clone()));
        let shutdown = state.shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
        });
        let cli = Cli::parse_from(["aoe", "session", "show", "second-factor-read", "--json"]);
        let source = ReadRequestSource {
            explicit_url: None,
            env_url: Some(format!("http://127.0.0.1:{port}/base/").into()),
            token: Some(OsString::from(token)),
            explicit_profile: Some("main".into()),
            env_profile: None,
        };
        let outcome = attempt(classify(cli.command.as_ref()).unwrap(), &source).await;
        assert_eq!(outcome.exit, 0, "{:?}", outcome.stderr);
        let value: serde_json::Value = serde_json::from_str(&outcome.stdout.unwrap()).unwrap();
        assert_eq!(value["id"], "second-factor-read");
        assert_eq!(
            state.login_manager.device_snapshot(None).await.len(),
            1,
            "exactly one passphrase login"
        );
        state.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
