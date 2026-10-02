//! The daemon-served read: one WebSocket exchange, two application frames, and
//! a projection the CLI prints verbatim.
//!
//! What this module owes the operator is where the bytes came from. Every
//! answer it returns was rendered by a daemon from a snapshot, so a command
//! whose bytes came from the local store instead has to say so, or its output
//! is indistinguishable from a read that was served.
//!
//! `--daemon-url` and `AOE_DAEMON_URL` are not in the same position and do not
//! behave the same way. The flag names an endpoint, so refusing it (or failing
//! to reach it) is what the user asked for by name and stays a refusal. The
//! variable names a remote that may simply not be running, which is a state a
//! local command still answers in, so a variable naming an endpoint that does
//! not answer still lets the command run, and says on stderr that the local
//! store answered. What it never does is fall over silently: the notice names
//! the variable rather than its value, so a URL carrying a token is not echoed
//! to a terminal or a log.

/// `pub(crate)` so the server's wire contract test can drive the client's real
/// decoders; the two halves then cannot drift on a field name or member order.
pub(crate) mod dto;
mod endpoint;
/// Debug-only Contract Pack support for unit and integration tests.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod pack;
#[cfg(all(test, debug_assertions))]
mod pack_tests;
mod render;
/// The local admission walk and its peer-credential check, which read process
/// identity from `/proc` and are therefore Linux-only. The publisher refuses
/// every other platform for the same reason (`server::runtime_uds::publish`),
/// so on one there is no local read to admit and the Local transport below
/// reports the publication absent, which runs the command from the local store
/// exactly as it did before the read existed.
#[cfg(target_os = "linux")]
/// `pub(crate)` because the publisher runs the identical walk
/// (`server::runtime_uds`): the client stays the enforcing boundary, and a
/// producer that re-derived the rules could only ever be weaker.
pub(crate) mod uds;

use std::time::Duration;
use tokio::time::Instant;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::{CapacityError, Error as WsError, ProtocolError};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, WebSocketConfig};

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

// The producer's own budget for one connection, so the client's exchange
// deadline and the daemon's server-side bound are the same number rather
// than two literals that happen to agree today. One stalled peer can never
// hold a client task and a server task open at once, and a read has exactly
// one budget: the exchange deadline is the establishment deadline, never a
// second window opened after it.
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

/// Every code a scoped read can report, and the only vocabulary the Contract
/// Pack's phase/code/exit table may use.
///
/// The two lists are held together from both ends, and in a debug build only:
/// every constructor below asserts in a `debug_assert!` that the code it was
/// handed is in this set, so a new emitter cannot introduce a code the table
/// does not describe, and the pack verifier is behind the pack's own
/// `#[cfg(debug_assertions)]`, so the table cannot describe a code this half
/// cannot emit. A release build checks neither.
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

/// The sentence a wire code is reported under, in the one place it is spelled.
/// Three callers report `renderer_internal` (this module's own refusal, the
/// renderer's failed serialisation, and a failed write of the answer to stdout)
/// and the first of them reports a code the caller chose, so a literal in any
/// of them is a second spelling waiting to drift.
pub fn read_sentence(code: &str) -> String {
    format!("daemon read: {code}\n")
}

#[derive(Debug)]
pub(crate) struct ReadFailure {
    code: &'static str,
    exit: i32,
    /// The sentence the renderer prints, when the refusal is the user's to
    /// read rather than a wire code. Owned text, not a `&'static str`, for the
    /// same reason the producer half owns its detail
    /// (`server::runtime_uds::PublishError`): a refusal that has to name the
    /// path it refused, or the candidates that would resolve it, cannot say so
    /// out of a constant. The code stays `&'static str` because it is checked
    /// against [`EMITTABLE_CODES`].
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

    /// A pre-admission refusal that says what to fix. The walk's refusal has
    /// to name the component it refused, the mode it found and the command
    /// that clears it, and none of that is a constant, for the same reason
    /// [`ReadFailure::exit`] owns its text. The code and the exit are exactly
    /// the ones the refusal already carried, so saying more cannot widen it:
    /// a diagnosis is not an admission.
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

    /// A refusal whose exit the renderer chooses: a user-facing message that is
    /// not a wire failure. The code stays the renderer's own, so the pack can
    /// still tell an internal fault (exit 1) from a refusal (exit 2).
    pub(crate) fn exit(exit: i32, message: impl Into<String>) -> Self {
        Self::exit_with(exit, message.into())
    }

    /// A refusal on the user's own state that keeps its own code. The three
    /// user-input refusals (`profile_missing`, `session_missing`,
    /// `session_ambiguous`) are not wire failures, so what they carry is the
    /// local path's exit (1) and the local path's sentence, but the code is
    /// still what says *which* refusal this was, so a caller can tell a
    /// missing profile from a missing session from an internal fault. The code
    /// is checked against the emittable set exactly as every other constructor
    /// checks it.
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
    /// The daemon rendered the command, or refused it. This is the final result.
    Answered(ReadOutcome),
    /// No local daemon has published a runtime read. Only ever returned for the
    /// local transport, so the caller runs the command against the local store
    /// exactly as it did before the read existed, including its best-effort
    /// `agent_session_id` backfill, which an answered read does not run.
    ///
    /// The notice is `Some` only when a variable named an endpoint that did
    /// not answer, the one case where the local store answers a command the
    /// user aimed somewhere else. With no endpoint named this is the ordinary
    /// local read and has nothing to report.
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

/// Whether this failure means the local command path may answer for itself.
///
/// An explicit `--daemon-url` is a request for a served answer: whatever the
/// endpoint says, including that it cannot be reached, is what the user asked
/// for by name, so nothing here applies to it.
fn absent_local_publication(error: &ReadFailure, source: &ReadRequestSource) -> bool {
    if source.explicit_url.is_some() {
        return false;
    }
    match error.code() {
        // A take-over may carry a statement about the environment, never one
        // about this client. The walk, the marker read and the join of the
        // blocking task are this client's, so they stay refusals, and so does
        // `marker_invalid`, because the artifact is there and untrustworthy.
        // `marker_missing` is the exception: it is defined as an absence.
        //
        // Past admission the markers have proved a live publisher and the
        // connected socket has been proved to be that publisher's, so failing
        // to get an answer out of it is a fact about the environment.
        "marker_missing" => !source.env_url_is_set(),
        // What proves the endpoint was not there. `publisher_absent` is the
        // TCP connect failing, so nothing was listening; a stall stays
        // because a silent peer and a dead host are the same observable from
        // here.
        //
        // `unavailable` leaves, because it now means a peer that was reached
        // and then did not serve, which is not absence. The two are separated
        // by which call failed rather than by the error: the connect is its own
        // step, and a refused certificate arrives from the handshake that
        // follows it. Measured on this build, a refused certificate is
        // `ErrorKind::InvalidData`, a refused port is `ConnectionRefused` and a
        // name that does not resolve is `Uncategorized` -- distinct kinds, so
        // an earlier note here claiming they were the same was wrong.
        //
        // What leaves the arm as well: `invalid_endpoint` and `invalid_token`
        // are configuration faults no socket can fix, and `server_error` is a
        // peer that answered and would not serve. Falling back on any of those
        // prints this machine's sessions as though they came from the remote.
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
            // No publisher runs on this platform, so the absence is the only
            // answer the local transport can give, and the caller runs the
            // command against the local store.
            #[cfg(not(target_os = "linux"))]
            {
                let _ = establishment_deadline;
                Err(ReadFailure::pre("marker_missing"))
            }
        }
        SelectedEndpoint::Http { request } => {
            // `~` in `aoe status --verbose` is a display convention meaning
            // "this machine's home", the same shorthand the local command
            // prints. A loopback host is therefore given one. The host string
            // does not prove the peer shares this home, since a forwarded
            // loopback reaches another machine, and nothing here treats it as
            // proof. The collapse only ever shows *less* than the wire
            // carries, so a wrong assumption widens the output rather than
            // narrowing it.
            let local_home = loopback_home(&request);
            // The TCP connect is split from the handshake, as the local path
            // already does, because a peer that answered and would not serve
            // us is not a peer that was absent, and only the split says which
            // happened. The TLS wrap survives the split because
            // `client_async_tls_with_config` is public and takes the connected
            // stream with the default connector, so `None` wraps exactly as
            // `connect_async` would. It is not `client_async_with_config`:
            // that one never wraps, and a `wss://` endpoint handed to it is
            // not a transport at all.
            let (connect_host, connect_port) = host_port(request.uri())?;
            let stream = tokio::time::timeout_at(
                establishment_deadline,
                tokio::net::TcpStream::connect((connect_host, connect_port)),
            )
            .await
            .map_err(|_| ReadFailure::pre("establishment_timeout"))?
            .map_err(|_| ReadFailure::post("publisher_absent"))?;
            let (stream, _) = tokio::time::timeout_at(
                establishment_deadline,
                tokio_tungstenite::client_async_tls_with_config(
                    *request,
                    stream,
                    Some(websocket_config()),
                    None,
                ),
            )
            .await
            .map_err(|_| ReadFailure::pre("establishment_timeout"))?
            .map_err(map_upgrade_error)?;
            // One budget per read: the exchange rides the establishment
            // window rather than opening a second one behind it.
            let exchange_deadline = establishment_deadline;
            exchange_stream(
                stream,
                exchange_deadline,
                ExpectedPeer::Remote,
                local_home.as_deref(),
                command,
                source,
            )
            .await
        }
    }
}

/// The `(host, port)` an endpoint names, defaulting the port the way the
/// connect does so the two cannot disagree about it. The host arrives
/// bracketed for IPv6, and the brackets are not part of the address: the
/// connect resolves what it is given, and `[::1]` is not an address literal.
/// `endpoint::unbracketed` is the same strip the handshake does.
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

/// This machine's home, when the endpoint names a loopback address, and
/// nothing else.
///
/// The question is whether the peer is the machine these paths belong to, and
/// only an address answers it. A name does not: nothing here resolves one, so
/// an endpoint called `localhost` that resolves to another machine would have
/// this machine's home collapsed into that machine's paths. That is wrong in
/// the direction that leaks, so a name is refused rather than trusted. The
/// cost is that a local daemon reached over TLS by name prints its rows
/// unabbreviated.
fn loopback_home(
    request: &tokio_tungstenite::tungstenite::handshake::client::Request,
) -> Option<std::path::PathBuf> {
    crate::daemon::is_loopback_address(request.uri().host()?)
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
    let hello_bytes = match read_application(&mut stream).await {
        Ok(bytes) => bytes,
        Err(error) => return Err(error),
    };
    let hello = match parse_hello(&hello_bytes) {
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

    let snapshot_bytes = match read_application(&mut stream).await {
        Ok(bytes) => bytes,
        Err(error) => return Err(error),
    };
    let snapshot = match parse_snapshot(&snapshot_bytes) {
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
) -> Result<Vec<u8>, ReadFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => return Ok(text.as_str().as_bytes().to_vec()),
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

/// Close the exchange and settle on the answer.
///
/// A close that fails is still an error, because the contract says the read
/// reports one, and a peer that cannot be told to stop must not pass for a
/// clean finish. It is not, however, allowed to become *the* error: a snapshot
/// the client refused (`schema_invalid`), a profile the daemon does not have
/// (`profile_missing`) and a freshness the daemon never observed
/// (`freshness_unavailable`) are the facts the exit code is about, and
/// replacing any of them with `close_timeout` would report a transport hiccup
/// as the reason the read failed. So the original failure is returned as it
/// stands, and a close failure is what is reported only when there was no
/// failure to report.
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
        WsError::Http(response)
            if response.status().as_u16() == 401 || response.status().as_u16() == 403 =>
        {
            ReadFailure::post("unauthorized")
        }
        // The peer answered and would not serve us, which is not the same as
        // being absent, and a local store answering here would report this
        // machine's sessions as the remote's.
        WsError::Http(_) => ReadFailure::post("server_error"),
        _ => ReadFailure::post("unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::ffi::OsString;

    /// The home collapse is a loopback question, so every spelling of a
    /// loopback host has to answer it the same way and no other host does. A
    /// bracketed IPv6 authority arrives bracketed, and the reachable
    /// bracketed hosts a TLS endpoint may name are exactly the ones that must
    /// not collapse.
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
            ("https://localhost:8080/api/runtime/ws", false),
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

    /// The local take-over is gated on the environment naming no endpoint, so
    /// it has to read "names no endpoint" through the same definition the
    /// selector does. An empty or whitespace-only `AOE_DAEMON_URL` is exactly
    /// the case this exists for: with no daemon published, the command must
    /// answer from the local store, on every platform.
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
    /// The property worth pinning is not that these codes take over, but that
    /// **nothing else does**. A new code added to the emit table is the edit
    /// that would silently widen this, so the table enumerates the whole
    /// vocabulary and asserts `false` for everything outside the set, under
    /// every combination of endpoint inputs.
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

    /// The one case where the local store answers a command the user aimed
    /// somewhere else: `AOE_DAEMON_URL` names an endpoint, nothing answers
    /// there, and the command runs locally anyway. That is the right fallback,
    /// and it is only right if the operator is told, because the transport's
    /// premise is that the bytes are a daemon's.
    ///
    /// A token is supplied so the read reaches the network and fails for the
    /// reason this test is about. Without one it never dials, and a missing
    /// token is a configuration fault rather than an absent endpoint, which the
    /// companion test below pins separately.
    ///
    /// `.test` is reserved and never resolves, so the absence is the host's
    /// and not this test's timing.
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

    /// The connect target has to be an address, not an authority. `uri.host()`
    /// hands IPv6 back bracketed, and the brackets are not part of the
    /// literal, so a bracketed endpoint resolved by the connect fails to
    /// resolve at all -- which, with a variable naming the endpoint, is a
    /// refusal and so answers from the local store instead of reporting that
    /// the endpoint could not be reached.
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

    /// The other side of the same line: a variable naming an endpoint with no
    /// bearer token never opens a socket, so nothing was ever asked whether it
    /// was there. Falling back would print this machine's rows under a notice
    /// claiming a remote did not answer, when the remote was never contacted.
    #[tokio::test]
    async fn a_variable_without_a_token_refuses_rather_than_claiming_the_endpoint_was_absent() {
        let source = ReadRequestSource {
            explicit_url: None,
            env_url: Some(OsString::from("http://127.0.0.1:9")),
            token: None,
            explicit_profile: None,
            env_profile: None,
        };
        let cli = Cli::try_parse_from(["aoe", "list"]).expect("the argv parses");
        let command = classify(cli.command.as_ref()).expect("`aoe list` is scoped");

        let ScopedRead::Answered(outcome) = attempt(command, &source).await else {
            panic!("a configuration fault is not an absent endpoint");
        };
        assert_eq!(outcome.exit, 2, "the refusal keeps its exit");
        assert!(outcome.stdout.is_none(), "a refusal prints no answer");
        assert_eq!(
            outcome.stderr.as_deref(),
            Some("daemon read: invalid_token\n"),
            "and it says what is actually wrong"
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

    /// The wire limits the verifier enforces, taken from the config a read
    /// actually builds; the budgets themselves are documented on the
    /// constants.

    #[test]
    fn error_rendering_uses_exact_exit_taxonomy() {
        let pre: ReadOutcome = ReadFailure::pre("marker_invalid").into();
        assert_eq!(pre.stderr.as_deref(), Some("daemon read: marker_invalid\n"));
        assert_eq!(pre.exit, 2);
        let post: ReadOutcome = ReadFailure::post("schema_invalid").into();
        assert_eq!(
            post.stderr.as_deref(),
            Some("daemon read: schema_invalid\n")
        );
    }

    /// A close that cannot be sent is still an error, but it is not allowed to
    /// become the error: the refusal that caused it is the fact the exit code
    /// is about. The peer here is gone, so every close fails.
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

    /// With nothing to report, a close that cannot be sent is the whole story,
    /// and a successful render is still refused rather than passed off as
    /// clean.
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
}
