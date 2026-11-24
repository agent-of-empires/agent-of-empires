//! Native HTTP transport. Unix identity is checked on the connected stream.

#[cfg(unix)]
use std::path::Path;

use super::DaemonClientError;

const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'/')
    .add(b'?')
    .add(b'#')
    .add(b'%')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

pub fn path_segment(value: &str) -> Result<percent_encoding::PercentEncode<'_>, DaemonClientError> {
    // URL parsers normalize even percent-encoded dot segments.
    if matches!(value, "" | "." | "..") {
        return Err(DaemonClientError::InvalidPathSegment);
    }
    Ok(percent_encoding::utf8_percent_encode(value, PATH_SEGMENT))
}

/// Authorize the connected Unix peer before HTTP sends any bytes.
/// If a PID is supplied, authorization must also match that process.
#[cfg(unix)]
pub type VerifyPeer = fn(&tokio::net::UnixStream, Option<u32>) -> Result<(), DaemonClientError>;

#[cfg(unix)]
#[derive(Clone)]
pub struct UnixTransport {
    path: std::path::PathBuf,
    verify_peer: VerifyPeer,
}

#[cfg(unix)]
impl UnixTransport {
    pub fn new(path: impl Into<std::path::PathBuf>, verify_peer: VerifyPeer) -> Self {
        Self {
            path: path.into(),
            verify_peer,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Connect and authorize the peer before handing the stream to a protocol.
    pub async fn connect(
        &self,
        expected_pid: Option<u32>,
    ) -> Result<tokio::net::UnixStream, DaemonClientError> {
        connect_unix(&self.path, self.verify_peer, expected_pid).await
    }

    pub async fn execute(
        &self,
        request: reqwest::Request,
        expected_pid: Option<u32>,
    ) -> Result<reqwest::Response, DaemonClientError> {
        execute_unix(&self.path, self.verify_peer, expected_pid, request).await
    }
}

#[cfg(unix)]
pub async fn connect_unix(
    path: &Path,
    verify_peer: VerifyPeer,
    expected_pid: Option<u32>,
) -> Result<tokio::net::UnixStream, DaemonClientError> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|_| DaemonClientError::UnixTransport)?;
    verify_peer(&stream, expected_pid)?;
    Ok(stream)
}

pub async fn execute(
    http: &reqwest::Client,
    request: reqwest::Request,
) -> Result<reqwest::Response, DaemonClientError> {
    let deadline = request_deadline(&request);
    let mut response = tokio::time::timeout_at(deadline, http.execute(request))
        .await
        .map_err(|_| DaemonClientError::Timeout)?
        .map_err(request_error)?;
    response
        .extensions_mut()
        .insert(OperationDeadline(deadline));
    Ok(response)
}

fn request_deadline(request: &reqwest::Request) -> tokio::time::Instant {
    tokio::time::Instant::now() + request.timeout().copied().unwrap_or(super::DEFAULT_TIMEOUT)
}

#[cfg(unix)]
async fn execute_unix(
    path: &Path,
    verify_peer: VerifyPeer,
    expected_pid: Option<u32>,
    request: reqwest::Request,
) -> Result<reqwest::Response, DaemonClientError> {
    use hyper::body::Body;
    use hyper::header::{HeaderValue, HOST};
    use std::future::poll_fn;
    use std::pin::Pin;
    use std::task::{ready, Poll};

    let deadline = request_deadline(&request);
    tokio::time::timeout_at(deadline, async {
        let stream = connect_unix(path, verify_peer, expected_pid).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(|_| DaemonClientError::UnixTransport)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut request: hyper::Request<reqwest::Body> =
            request.try_into().map_err(request_error)?;
        if !request.headers().contains_key(HOST) {
            request
                .headers_mut()
                .insert(HOST, HeaderValue::from_static("localhost"));
        }
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| DaemonClientError::UnixTransport)?;
        let response = response.map(|body| {
            let chunks = futures_util::stream::try_unfold(body, move |mut body| async move {
                if tokio::time::Instant::now() >= deadline {
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                }
                let next_data = poll_fn(|cx| loop {
                    match ready!(Pin::new(&mut body).poll_frame(cx)) {
                        Some(Ok(frame)) => {
                            if let Ok(data) = frame.into_data() {
                                return Poll::Ready(Ok(Some(data)));
                            }
                        }
                        Some(Err(error)) => return Poll::Ready(Err(error)),
                        None => return Poll::Ready(Ok(None)),
                    }
                });
                let data = tokio::time::timeout_at(deadline, next_data)
                    .await
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
                    .map_err(std::io::Error::other)?;
                Ok(data.map(|data| (data, body)))
            });
            reqwest::Body::wrap_stream(chunks)
        });
        let mut response = reqwest::Response::from(response);
        response
            .extensions_mut()
            .insert(OperationDeadline(deadline));
        Ok(response)
    })
    .await
    .map_err(|_| DaemonClientError::Timeout)?
}

#[derive(Clone, Copy)]
struct OperationDeadline(tokio::time::Instant);

pub fn response_deadline(response: &reqwest::Response) -> tokio::time::Instant {
    response
        .extensions()
        .get::<OperationDeadline>()
        .map_or_else(
            || tokio::time::Instant::now() + super::DEFAULT_TIMEOUT,
            |deadline| deadline.0,
        )
}

pub fn request_error(error: reqwest::Error) -> DaemonClientError {
    tracing::debug!(target: "daemon.client", %error, "daemon request failed");
    if error.is_timeout() {
        DaemonClientError::Timeout
    } else {
        DaemonClientError::Transport
    }
}
