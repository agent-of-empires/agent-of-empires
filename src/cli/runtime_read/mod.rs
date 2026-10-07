//! Validate and render a daemon snapshot after a two-frame WebSocket exchange.
//! Explicit endpoints never fall back. Absent environment-selected endpoints
//! use the local store with a notice that does not disclose the endpoint URL.

/// Shared with the producer's contract checks against the real client decoder.
pub(crate) mod dto;
mod endpoint;
#[cfg(test)]
pub(crate) mod pack;
#[cfg(test)]
mod pack_tests;
mod render;
/// Linux descriptor admission shared with the local publisher.
/// Other platforms use HTTP when selected, otherwise the local store.
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
/// The line a read prints when a variable named an endpoint that did not
/// answer, so the store's rows were printed by the local command.
pub(crate) const LOCAL_STORE_NOTICE: &str =
    "notice: AOE_DAEMON_URL named an endpoint that did not answer, so this answer is the local store's.\n";

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
    format!("daemon read: {code}\n")
}

#[derive(Debug)]
pub(crate) struct ReadFailure {
    code: &'static str,
    exit: i32,
    /// Exact user-facing diagnosis when a stable wire code cannot name the cause.
    exact: Option<String>,
    attempt_close: bool,
}

impl ReadFailure {
    pub(crate) fn pre(code: &'static str) -> Self {
        Self::refusal(code, 2, false)
    }

    pub(crate) fn post(code: &'static str) -> Self {
        Self::refusal(code, 4, true)
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

/// What a scoped read found: either the daemon's answer, or the fact that no
/// daemon publishes a local read here, which leaves the command to the caller.
pub enum ScopedRead {
    /// Final CLI projection or refusal from a served exchange; no local handler runs.
    Answered(ReadOutcome),
    /// Run the local handler; notice is present only for an absent environment endpoint.
    NoLocalPublication(Option<&'static str>),
}

pub async fn attempt(command: ScopedCommand<'_>, source: &ReadRequestSource) -> ScopedRead {
    match execute_inner(command, source).await {
        Ok(projection) => {
            let stderr = if projection.session_table {
                crate::update::update_notice().await
            } else {
                None
            };
            ScopedRead::Answered(ReadOutcome {
                stdout: Some(projection.stdout),
                stderr,
                exit: 0,
            })
        }
        Err(error) if absent_local_publication(&error, source) => {
            ScopedRead::NoLocalPublication(source.env_url_is_set().then_some(LOCAL_STORE_NOTICE))
        }
        Err(error) => ScopedRead::Answered(error.into()),
    }
}

/// Explicit endpoints never fall back. Environment endpoints do so only before a peer is reached.
fn absent_local_publication(error: &ReadFailure, source: &ReadRequestSource) -> bool {
    if source.explicit_url.is_some() {
        return false;
    }
    match error.code() {
        "marker_missing" => !source.env_url_is_set(),
        "establishment_timeout" | "publisher_absent" => source.env_url_is_set(),
        _ => false,
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
                let request = "ws://localhost/api/runtime/ws"
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
                Err(ReadFailure::pre("marker_missing"))
            }
        }
        SelectedEndpoint::Http { request } => {
            let local_home = loopback_home(&request);
            // Environment discovery is optional; an explicit endpoint keeps the full budget.
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
    let auth = if source.token.is_none() && std::env::var_os("AOE_DAEMON_PASSPHRASE").is_some() {
        let uri = request.uri();
        let path = uri
            .path()
            .strip_suffix("/api/runtime/ws")
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
        let endpoint = DaemonEndpoint::new(base, None, Source::Env);
        endpoint
            .resolved_passphrase()
            .is_some()
            .then(|| (endpoint, PassphraseSessionCache::default()))
    } else {
        None
    };
    let mut refreshed = false;
    loop {
        let mut had_cached_session = false;
        if let Some((endpoint, cache)) = &auth {
            let session = match cache.get(endpoint) {
                Some(session) => {
                    had_cached_session = true;
                    session
                }
                None => {
                    tokio::time::timeout_at(deadline, passphrase_session::login(endpoint, cache))
                        .await
                        .map_err(|_| ReadFailure::post("unavailable"))?
                        .map_err(map_login_error)?
                }
            };
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
                if response.status().as_u16() == 401 && had_cached_session && !refreshed =>
            {
                let (endpoint, cache) = auth.as_ref().expect("cached passphrase session");
                cache.invalidate(endpoint);
                refreshed = true;
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
            )
            .await
        }
    };
    if let Err(code) = validate_snapshot(&snapshot) {
        return finish_with_close(&mut stream, deadline, Err(ReadFailure::post(code))).await;
    }
    let local_uid = match &expected {
        ExpectedPeer::Remote => None,
        #[cfg(target_os = "linux")]
        ExpectedPeer::Local(identity) => Some(identity.owner_uid),
    };
    if let Err(code) = validate_cross_message(&hello, &snapshot, local_uid) {
        return finish_with_close(&mut stream, deadline, Err(ReadFailure::post(code))).await;
    }
    let projection = render::evaluate(&command, &snapshot, source, local_home);
    finish_with_close(&mut stream, deadline, projection).await
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
) -> Result<render::Projection, ReadFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if let Err(error) = &result {
        if !error.attempt_close {
            return Err(ReadFailure {
                code: error.code,
                exit: error.exit,
                exact: error.exact.clone(),
                attempt_close: false,
            });
        }
    }
    match (result, close_local(stream, deadline).await) {
        (Err(error), _) => Err(error),
        (Ok(projection), Ok(())) => Ok(projection),
        (Ok(_), Err(close)) => Err(close),
    }
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
    match error {
        WsError::Http(response) => map_http_status(response.status().as_u16()),
        _ => ReadFailure::post("unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::ffi::OsString;

    // Localhost is pinned; other TLS names do not establish local provenance.
    #[test]
    fn only_a_loopback_host_answers_with_this_machines_home() {
        assert!(
            dirs::home_dir().is_some(),
            "this machine has a home to collapse to"
        );
        let cases: [(&str, bool); 8] = [
            ("http://127.0.0.1:8080/api/runtime/ws", true),
            ("http://[::1]:8080/api/runtime/ws", true),
            ("https://[::1]/api/runtime/ws", true),
            ("https://localhost:8080/api/runtime/ws", true),
            ("https://example.test/api/runtime/ws", false),
            ("wss://[fe80::1]/api/runtime/ws", false),
            ("wss://[2001:db8::1]/api/runtime/ws", false),
            ("http://192.168.1.10:8080/api/runtime/ws", false),
        ];
        for (url, expected) in cases {
            let request = tokio_tungstenite::tungstenite::http::Request::builder()
                .uri(url)
                .body(())
                .expect("the uri parses");
            assert_eq!(loopback_home(&request).is_some(), expected, "{url}");
        }
    }

    /// Empty or blank environment endpoints preserve local takeover.
    #[test]
    fn an_empty_environment_url_still_takes_over_from_the_local_store() {
        let missing = ReadFailure::pre("marker_missing");
        let cases: [(&str, Option<OsString>); 3] = [
            ("absent", None),
            ("empty", Some(OsString::from(""))),
            ("whitespace", Some(OsString::from("   "))),
        ];
        for (label, env_url) in cases {
            let source = ReadRequestSource {
                explicit_url: None,
                env_url,
                token: None,
                explicit_profile: None,
                env_profile: None,
            };
            assert!(
                absent_local_publication(&missing, &source),
                "{label} must answer from the local store"
            );
        }
        let named = ReadRequestSource {
            explicit_url: None,
            env_url: Some(OsString::from("https://example.test")),
            token: None,
            explicit_profile: None,
            env_profile: None,
        };
        assert!(
            !absent_local_publication(&missing, &named),
            "a named endpoint is the daemon's answer to keep"
        );
        assert!(
            !absent_local_publication(
                &ReadFailure::pre("marker_invalid"),
                &ReadRequestSource {
                    explicit_url: None,
                    env_url: None,
                    token: None,
                    explicit_profile: None,
                    env_profile: None,
                }
            ),
            "only an absent publication is a take-over"
        );
    }
    /// New refusal codes must not silently widen local takeover.
    #[test]
    fn only_the_endpoint_codes_take_over_and_nothing_else_does() {
        const TAKES_OVER: [&str; 3] = [
            "marker_missing",
            "establishment_timeout",
            "publisher_absent",
        ];
        for code in EMITTABLE_CODES {
            let error = ReadFailure::pre(code);
            for (label, explicit_url, env_url) in [
                ("nothing named", None, None),
                ("a flag", Some("https://flag.test".to_string()), None),
                ("a variable", None, Some(OsString::from("https://env.test"))),
            ] {
                let source = ReadRequestSource {
                    explicit_url,
                    env_url,
                    token: None,
                    explicit_profile: None,
                    env_profile: None,
                };
                // `marker_missing` takes over only with no endpoint named, and
                // the transport codes only with a variable naming one; an
                // explicit flag is a request for a served answer either way.
                let expected = match (*code, label) {
                    ("marker_missing", "nothing named") => true,
                    ("marker_missing", _) => false,
                    (_, "a variable") => TAKES_OVER.contains(code),
                    _ => false,
                };
                assert_eq!(
                    absent_local_publication(&error, &source),
                    expected,
                    "{code} with {label}"
                );
            }
        }
    }

    /// A TCP discovery timeout may fall back; an accepted but stalled peer may not.
    #[test]
    fn a_stall_after_the_socket_exists_is_not_the_same_as_a_stall_before_it() {
        let source = ReadRequestSource {
            explicit_url: None,
            env_url: Some(OsString::from("http://127.0.0.1:9")),
            token: Some(OsString::from("t")),
            explicit_profile: None,
            env_profile: None,
        };

        let before = ReadFailure::pre("establishment_timeout");
        let after = ReadFailure::post("unavailable");

        assert_ne!(before.code(), after.code(), "the two stages must differ");
        assert!(
            absent_local_publication(&before, &source),
            "a connect that never completed takes over"
        );
        assert!(
            !absent_local_publication(&after, &source),
            "a peer that accepted and then stopped does not take over"
        );
    }

    /// An absent environment-selected peer falls back with an endpoint notice.
    #[tokio::test]
    async fn a_variable_naming_an_endpoint_that_did_not_answer_says_the_store_answered() {
        let absent = "http://127.0.0.1:9";
        let source = |explicit_url: Option<&str>| ReadRequestSource {
            explicit_url: explicit_url.map(str::to_string),
            env_url: Some(OsString::from(absent)),
            token: Some(OsString::from("t")),
            explicit_profile: None,
            env_profile: None,
        };
        let cli = Cli::try_parse_from(["aoe", "list"]).expect("the argv parses");
        let command = classify(cli.command.as_ref()).expect("`aoe list` is scoped");

        let read = attempt(command, &source(None)).await;
        let ScopedRead::NoLocalPublication(notice) = read else {
            panic!("an endpoint that is not there must not be answered by a daemon");
        };
        assert_eq!(
            notice,
            Some(LOCAL_STORE_NOTICE),
            "the local store's rows have to say they are the local store's"
        );

        let flagged = attempt(command, &source(Some(absent))).await;
        let ScopedRead::Answered(outcome) = flagged else {
            panic!("a flag naming an endpoint is a request for a served answer");
        };
        assert_eq!(
            outcome.exit, 4,
            "a named endpoint that is not there leaves the transport refusal's exit"
        );
        assert!(outcome.stdout.is_none(), "a refusal prints no answer");
    }

    /// Connect targets omit the authority brackets around IPv6 literals.
    #[test]
    fn a_bracketed_ipv6_endpoint_connects_to_an_address_literal() {
        for (url, host, port) in [
            ("http://[::1]:8080/api/runtime/ws", "::1", 8080),
            ("wss://[fe80::1]/api/runtime/ws", "fe80::1", 443),
            ("http://127.0.0.1:8080/api/runtime/ws", "127.0.0.1", 8080),
            ("wss://example.test/api/runtime/ws", "example.test", 443),
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
    async fn an_explicitly_empty_token_refuses_before_endpoint_discovery() {
        let source = ReadRequestSource {
            explicit_url: None,
            env_url: Some(OsString::from("http://127.0.0.1:9")),
            token: Some(OsString::new()),
            explicit_profile: None,
            env_profile: None,
        };
        let cli = Cli::parse_from(["aoe", "list"]);
        let ScopedRead::Answered(outcome) =
            attempt(classify(cli.command.as_ref()).unwrap(), &source).await
        else {
            panic!("a malformed credential is not an absent endpoint");
        };
        assert_eq!(outcome.exit, 2);
        assert_eq!(outcome.stdout, None);
        assert_eq!(
            outcome.stderr.as_deref(),
            Some("daemon read: invalid_token\n")
        );
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
    fn scoped_parser_rejects_conflicts_and_missing_profile_values() {
        let cases: &[&[&str]] = &[
            &["aoe", "status", "--json", "--quiet"],
            &["aoe", "status", "--quiet", "--verbose"],
            &["aoe", "status", "--json", "--verbose"],
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
        let mut stream = broken_stream().await;
        let deadline = Instant::now() + CONNECTION_BUDGET;
        for code in ["schema_invalid", "profile_missing", "freshness_unavailable"] {
            let error = finish_with_close(&mut stream, deadline, Err(ReadFailure::post(code)))
                .await
                .expect_err("the read failed");
            assert_eq!(error.code(), code, "{code} must survive a broken close");
        }
    }

    /// An otherwise successful read reports its close failure.
    #[tokio::test]
    async fn a_failed_close_is_the_answer_when_the_read_otherwise_succeeded() {
        let mut stream = broken_stream().await;
        let deadline = Instant::now() + CONNECTION_BUDGET;
        let projection = render::Projection {
            stdout: "rows\n".into(),
            session_table: false,
        };
        let error = finish_with_close(&mut stream, deadline, Ok(projection))
            .await
            .expect_err("a broken close is not a clean finish");
        assert_eq!(error.code(), "close_timeout");
        assert_eq!(error.exit, 4);
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
        let mut state = test_support::build_test_app_state_with_policy(
            vec![row],
            vec!["localhost".into(), "127.0.0.1".into()],
            Vec::new(),
            None,
        );
        let mutable = Arc::get_mut(&mut state).unwrap();
        mutable.login_manager = Arc::new(crate::server::login::LoginManager::new(Some(phrase)));
        mutable.behind_tunnel = true;
        mutable.auth_mode = "passphrase";
        test_support::accept_runtime_read_cache_for_test(&state).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = test_support::build_router_for_test(state.clone());
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
            env_url: Some(format!("http://localhost:{port}").into()),
            token: None,
            explicit_profile: Some("main".into()),
            env_profile: None,
        };
        let mut previous_session = None;
        for (index, revoke) in [false, false, true].into_iter().enumerate() {
            if revoke {
                assert_eq!(state.login_manager.logout_all().await, 1);
            }
            let ScopedRead::Answered(outcome) =
                attempt(classify(cli.command.as_ref()).unwrap(), &source).await
            else {
                panic!("an authenticated localhost peer must not fall back to disk");
            };
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
}
