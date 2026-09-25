/// `pub(crate)` so the server's wire contract test can drive the client's real
/// decoders; the two halves then cannot drift on a field name or member order.
pub(crate) mod dto;
mod endpoint;
/// The Contract Pack verifier and its frozen fixtures. Gated to the test
/// profile: the pack is fixture governance, never a production input.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod pack;
#[cfg(test)]
mod pack_tests;
mod render;
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
use self::uds::UdsIdentity;
use super::group::GroupListArgs;
use super::list::ListArgs;
use super::project::ProjectListArgs;
use super::session::ShowArgs;
use super::status::StatusArgs;
use super::{Cli, Commands};

const ESTABLISHMENT_BUDGET: Duration = Duration::from_secs(15);
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

#[derive(Debug)]
pub(crate) struct ReadFailure {
    code: &'static str,
    exit: i32,
    exact: Option<&'static str>,
    attempt_close: bool,
}

impl ReadFailure {
    pub(crate) fn pre(code: &'static str) -> Self {
        Self {
            code,
            exit: 2,
            exact: None,
            attempt_close: false,
        }
    }

    pub(crate) fn post(code: &'static str) -> Self {
        Self {
            code,
            exit: 4,
            exact: None,
            attempt_close: true,
        }
    }

    pub(crate) fn post_no_close(code: &'static str) -> Self {
        Self {
            code,
            exit: 4,
            exact: None,
            attempt_close: false,
        }
    }

    pub(crate) fn exit(exit: i32, message: &'static str) -> Self {
        Self {
            code: "renderer_internal",
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

pub async fn execute(command: ScopedCommand<'_>, source: &ReadRequestSource) -> ReadOutcome {
    match execute_inner(command, source).await {
        Ok(stdout) => ReadOutcome {
            stdout: Some(stdout),
            stderr: None,
            exit: 0,
        },
        Err(error) => error.into(),
    }
}

async fn execute_inner(
    command: ScopedCommand<'_>,
    source: &ReadRequestSource,
) -> Result<String, ReadFailure> {
    if let ScopedCommand::Show(args) = command {
        if args.identifier().is_none() {
            return Err(ReadFailure::exit(
                2,
                "identifier required in daemon read mode\n",
            ));
        }
    }
    let endpoint = endpoint::select_endpoint(source)?;
    let establishment_deadline = Instant::now() + ESTABLISHMENT_BUDGET;
    match endpoint {
        SelectedEndpoint::Local => {
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
    Local(UdsIdentity),
}

async fn exchange_stream<S>(
    stream: tokio_tungstenite::WebSocketStream<S>,
    deadline: Instant,
    expected: ExpectedPeer,
    local_home: Option<&std::path::Path>,
    command: ScopedCommand<'_>,
    source: &ReadRequestSource,
) -> Result<String, ReadFailure>
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
) -> Result<String, ReadFailure>
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

async fn finish_with_close<S>(
    stream: &mut tokio_tungstenite::WebSocketStream<S>,
    deadline: Instant,
    result: Result<String, ReadFailure>,
) -> Result<String, ReadFailure>
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
    close_local(stream, deadline).await?;
    result
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
        Ok(Err(WsError::ConnectionClosed)) => {
            let mut failure = ReadFailure::post("connection_closed");
            failure.attempt_close = false;
            Err(failure)
        }
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
        assert_eq!(post.exit, 4);
    }
}
