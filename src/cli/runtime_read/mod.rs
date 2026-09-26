/// `pub(crate)` so the server's wire contract test can drive the client's real
/// decoders; the two halves then cannot drift on a field name or member order.
pub(crate) mod dto;
mod endpoint;
/// The Contract Pack verifier and its frozen fixtures. Test-only surface, in
/// the same shape as the other `test_support` modules, so the integration
/// tests that verify the pack can reach it.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub mod pack;
#[cfg(test)]
mod pack_tests;
mod render;
/// The local admission walk and its peer-credential check, which read process
/// identity from `/proc` and are therefore Linux-only. The publisher refuses
/// every other platform for the same reason (`server::runtime_uds::publish`),
/// so on one there is no local read to admit and the Local transport below
/// reports the publication absent, which runs the command from the local store
/// exactly as it did before the read existed.
#[cfg(target_os = "linux")]
mod uds;

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

/// How long a read may take to establish, admission included. The daemon
/// bounds its own side of the same window with the same constant
/// (`server::runtime_ws::CONNECTION_BUDGET`), so one stalled peer can never
/// hold a client task and a server task open at once.
pub(crate) const ESTABLISHMENT_BUDGET: Duration = Duration::from_secs(15);
const EXCHANGE_BUDGET: Duration = Duration::from_secs(15);
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

/// Every code a scoped read can report, and the only vocabulary the Contract
/// Pack's phase/code/exit table may use.
///
/// The two lists are held together from both ends. Each constructor below
/// asserts that the code it was handed is in this set, so a new emitter cannot
/// introduce a code the table does not describe; the pack verifier requires
/// every code in its table to be in this set, so the table cannot describe a
/// code no emitter produces. A code with no emitter is what left
/// `identifier_required` frozen in the pack for a renderer that no longer
/// exists.
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
    "renderer_internal",
    "schema_invalid",
    "session_ambiguous",
    "session_missing",
    "socket_identity",
    "unauthorized",
    "unavailable",
];

/// The code a caller-chosen-exit refusal carries: the renderer refused on the
/// user's own state, not on the wire.
const RENDERER_INTERNAL: &str = "renderer_internal";

#[derive(Debug)]
pub(crate) struct ReadFailure {
    code: &'static str,
    exit: i32,
    exact: Option<&'static str>,
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
    pub(crate) fn exit(exit: i32, message: &'static str) -> Self {
        debug_assert!(EMITTABLE_CODES.contains(&RENDERER_INTERNAL));
        Self {
            code: RENDERER_INTERNAL,
            exit,
            exact: Some(message),
            attempt_close: true,
        }
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
        let message = error
            .exact
            .map(str::to_string)
            .unwrap_or_else(|| format!("daemon read: {}\n", error.code));
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
    /// exactly as it did before the read existed.
    NoLocalPublication,
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
        Err(error) if absent_local_publication(&error, source) => ScopedRead::NoLocalPublication,
        Err(error) => ScopedRead::Answered(error.into()),
    }
}

/// Whether this failure means no daemon has ever published here, which is the
/// one pre-admission refusal the local command path may take over. A named
/// endpoint, and every refusal that says the artifacts are present but not
/// trustworthy, are the daemon's answer to keep: falling back on those would
/// quietly serve data the admission was built to withhold.
fn absent_local_publication(error: &ReadFailure, source: &ReadRequestSource) -> bool {
    error.code() == "marker_missing" && source.explicit_url.is_none() && source.env_url.is_none()
}

async fn execute_inner(
    command: ScopedCommand<'_>,
    source: &ReadRequestSource,
) -> Result<render::Projection, ReadFailure> {
    let endpoint = endpoint::select_endpoint(source)?;
    let establishment_deadline = Instant::now() + ESTABLISHMENT_BUDGET;
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
            let connected = tokio::time::timeout_at(
                establishment_deadline,
                tokio_tungstenite::connect_async_with_config(
                    *request,
                    Some(websocket_config()),
                    false,
                ),
            )
            .await
            .map_err(|_| ReadFailure::pre("establishment_timeout"))?;
            let (stream, _) = connected.map_err(map_upgrade_error)?;
            let exchange_deadline = connection_deadline(Instant::now());
            exchange_stream(
                stream,
                exchange_deadline,
                ExpectedPeer::Remote,
                None,
                command,
                source,
            )
            .await
        }
    }
}

fn connection_deadline(now: Instant) -> Instant {
    now + EXCHANGE_BUDGET
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
            )) => {
                let mut failure = ReadFailure::post("connection_closed");
                failure.attempt_close = false;
                return Err(failure);
            }
            Some(Err(WsError::Protocol(_)))
            | Some(Err(WsError::Utf8(_)))
            | Some(Err(WsError::AttackAttempt)) => return Err(ReadFailure::post("schema_invalid")),
            Some(Err(_)) => return Err(ReadFailure::post("unavailable")),
            None => {
                let mut failure = ReadFailure::post("connection_closed");
                failure.attempt_close = false;
                return Err(failure);
            }
        }
    }
}

/// Close the exchange and settle on the answer.
///
/// A close that fails is still an error — the contract says the read reports
/// one, and a peer that cannot be told to stop must not pass for a clean
/// finish. It is not, however, allowed to become *the* error: a snapshot the
/// client refused (`schema_invalid`), a profile the daemon does not have
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
                exact: error.exact,
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
        _ => ReadFailure::post("unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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

    #[test]
    fn websocket_limits_and_close_budget_are_normative() {
        let config = websocket_config();
        assert_eq!(config.max_frame_size, Some(APPLICATION_LIMIT));
        assert_eq!(config.max_message_size, Some(APPLICATION_LIMIT));
        assert_eq!(ESTABLISHMENT_BUDGET, Duration::from_secs(15));
        assert_eq!(EXCHANGE_BUDGET, Duration::from_secs(15));
        assert_eq!(CLOSE_BUDGET, Duration::from_millis(200));
    }

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
        let deadline = Instant::now() + EXCHANGE_BUDGET;
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
        let deadline = Instant::now() + EXCHANGE_BUDGET;
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
