use super::*;
use crate::tests::{unix_client, verify_test_peer};
use crate::transport::*;

#[tokio::test]
async fn unix_requests_preserve_host_and_stream_body_data() {
    use hyper::body::Body;
    use std::future::poll_fn;
    use std::pin::Pin;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let http = reqwest::Client::new();
    for (host, expected_host, truncated) in [
        (None, "localhost", false),
        (Some("custom.invalid"), "custom.invalid", false),
        (None, "localhost", true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(&mut stream).lines();
            let mut host = None;
            loop {
                let line = lines.next_line().await.unwrap().unwrap();
                if line.is_empty() {
                    break;
                }
                if let Some((_, value)) = line
                    .split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
                {
                    assert!(host.replace(value.trim().to_owned()).is_none());
                }
            }
            assert_eq!(host.as_deref(), Some(expected_host));
            let start: &[u8] = if truncated {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\na"
            } else {
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
                  Trailer: X-Checksum\r\nConnection: close\r\n\r\n1\r\na\r\n"
            };
            stream.write_all(start).await.unwrap();
            released.await.unwrap();
            if !truncated {
                stream
                    .write_all(b"1\r\nb\r\n0\r\nX-Checksum: ok\r\n\r\n")
                    .await
                    .unwrap();
            }
        });

        let mut request = http
            .get("http://request.invalid/operation")
            .timeout(Duration::from_secs(5));
        if let Some(host) = host {
            request = request.header(hyper::header::HOST, host);
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut response = execute_unix_test(&path, request.build().unwrap())
                .await
                .unwrap();
            assert_eq!(response.chunk().await.unwrap().unwrap().as_ref(), b"a");
            release.send(()).unwrap();
            let mut body = hyper::Response::<reqwest::Body>::from(response).into_body();
            if !truncated {
                let frame = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(frame.into_data().unwrap().as_ref(), b"b");
            }
            let tail = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
            if truncated {
                assert!(
                    matches!(&tail, Some(Err(error)) if !error.is_timeout()),
                    "{tail:?}"
                );
            } else {
                assert!(tail.is_none(), "{tail:?}");
            }
            server.await.unwrap();
        })
        .await
        .expect("Unix HTTP fixture did not complete");
    }
}

#[tokio::test(start_paused = true)]
async fn unix_body_keeps_the_header_operation_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let (arrived, mut observed) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let mut used = 0;
        while !request[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut request[used..]).await.unwrap();
            assert!(count > 0, "request must contain complete HTTP headers");
            used += count;
        }
        arrived.send(()).unwrap();
        released.await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
            .await
            .unwrap();
        let mut eof = [0];
        assert_eq!(stream.read(&mut eof).await.unwrap(), 0);
    });
    let http = reqwest::Client::new();
    let request = http
        .get("http://localhost/operation")
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let start = tokio::time::Instant::now();
    let expected_deadline = start + Duration::from_millis(100);
    let client = tokio::spawn(async move { execute_unix_test(&path, request).await });
    // Keep the runtime runnable while real socket I/O establishes request arrival.
    loop {
        match observed.try_recv() {
            Ok(()) => break,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                tokio::task::yield_now().await;
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                panic!("server closed before receiving the request");
            }
        }
    }
    assert_eq!(tokio::time::Instant::now(), start);
    tokio::time::advance(Duration::from_millis(60)).await;
    assert!(!client.is_finished());
    release.send(()).unwrap();
    // Paused time must not auto-advance while the OS makes headers readable.
    while !client.is_finished() {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        tokio::time::Instant::now(),
        start + Duration::from_millis(60)
    );
    let mut response = client.await.unwrap().unwrap();
    assert_eq!(response_deadline(&response), expected_deadline);
    {
        let body = tokio::time::timeout(Duration::from_secs(1), response.chunk());
        tokio::pin!(body);
        assert!(futures_util::poll!(&mut body).is_pending());
        tokio::time::advance(Duration::from_millis(39)).await;
        assert!(futures_util::poll!(&mut body).is_pending());
        tokio::time::advance(Duration::from_millis(11)).await;
        let result = body.await;
        assert!(
            matches!(&result, Ok(Err(error)) if error.is_timeout()),
            "body={result:?}"
        );
        assert_eq!(
            tokio::time::Instant::now(),
            start + Duration::from_millis(110)
        );
    }
    drop(response);
    server.await.unwrap();
}

#[tokio::test]
async fn unix_requests_honor_their_operation_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for (deadline, elapsed, should_timeout) in [(30, 16, false), (1, 2, true)] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (arrived, observed) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = [0; 4096];
            let mut used = 0;
            while !header[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut header[used..]).await.unwrap();
                assert!(read > 0);
                used += read;
            }
            arrived.send(()).unwrap();
            released.await.unwrap();
            let _ = stream
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        });
        let http = reqwest::Client::new();
        let request = http
            .get("http://localhost/operation")
            .timeout(Duration::from_secs(deadline))
            .build()
            .unwrap();
        let client = tokio::spawn(async move { execute_unix_test(&path, request).await });
        observed.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(elapsed)).await;
        tokio::time::resume();
        if should_timeout {
            assert!(matches!(
                client.await.unwrap(),
                Err(DaemonClientError::Timeout)
            ));
            release.send(()).unwrap();
        } else {
            release.send(()).unwrap();
            assert_eq!(
                client.await.unwrap().unwrap().status(),
                reqwest::StatusCode::CREATED
            );
        }
        server.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn extended_unix_body_keeps_its_operation_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut buf).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buf[..count]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
            .await
            .unwrap();
        released.await.unwrap();
        stream.write_all(b"x").await.unwrap();
    });
    let http = reqwest::Client::new();
    let request = http
        .get("http://localhost/operation")
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let start = tokio::time::Instant::now();
    let response = tokio::spawn(async move { execute_unix_test(&path, request).await });
    let wall_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !response.is_finished() {
        assert!(
            std::time::Instant::now() < wall_deadline,
            "fixture headers did not arrive"
        );
        tokio::task::yield_now().await;
    }
    let mut response = response.await.unwrap().unwrap();
    assert_eq!(
        response_deadline(&response),
        start + Duration::from_secs(30)
    );
    let body = read_bounded_body(&mut response, 1024);
    tokio::pin!(body);
    assert!(futures_util::poll!(&mut body).is_pending());
    tokio::time::advance(Duration::from_secs(16)).await;
    assert!(futures_util::poll!(&mut body).is_pending());
    release.send(()).unwrap();
    let bytes = loop {
        if let std::task::Poll::Ready(result) = futures_util::poll!(&mut body) {
            break result.unwrap();
        }
        assert!(
            std::time::Instant::now() < wall_deadline,
            "fixture body did not arrive"
        );
        tokio::task::yield_now().await;
    };
    assert_eq!(bytes.as_slice(), b"x");
    server.await.unwrap();
}

fn stalled_response(
    status: StatusCode,
    deadline: tokio::time::Instant,
) -> (
    reqwest::Response,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let polled = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&polled);
    let body = reqwest::Body::wrap_stream(futures_util::stream::poll_fn(move |_| {
        observed.store(true, Ordering::Relaxed);
        std::task::Poll::<Option<Result<hyper::body::Bytes, std::io::Error>>>::Pending
    }));
    let mut response = hyper::Response::new(body);
    *response.status_mut() = status;
    response
        .extensions_mut()
        .insert(OperationDeadline(deadline));
    (response.into(), polled)
}

#[tokio::test]
async fn bounded_decoder_rejects_an_unrepresentable_body_before_reading() {
    use hyper::body::{Body, Bytes, Frame, SizeHint};
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    struct Oversize;

    impl Body for Oversize {
        type Data = Bytes;
        type Error = io::Error;

        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
            panic!("oversized body should not be polled");
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(isize::MAX as u64 + 1)
        }
    }

    let response = hyper::Response::new(reqwest::Body::wrap(Oversize)).into();
    let result = decode_json_bounded::<serde_json::Value>(response, usize::MAX).await;
    assert!(matches!(
        result,
        Err(DaemonClientError::ResponseTooLarge { limit }) if limit == isize::MAX as usize
    ));
}

#[tokio::test(start_paused = true)]
async fn body_helpers_enforce_the_attached_deadline_without_a_transport_timer() {
    use std::sync::atomic::Ordering;

    let client = DaemonClient::new("http://localhost", None).unwrap();
    for consumer in 0..3 {
        let budget = DEFAULT_TIMEOUT + Duration::from_secs(10);
        let (mut response, polled) =
            stalled_response(StatusCode::OK, tokio::time::Instant::now() + budget);
        let read = async {
            match consumer {
                0 => read_bounded_body(&mut response, 1024).await.map(|_| ()),
                1 => discard_bounded_body(&mut response).await,
                _ => client.read_error_body(&mut response).await.map(|_| ()),
            }
        };
        tokio::pin!(read);
        assert!(futures_util::poll!(&mut read).is_pending());
        assert!(polled.load(Ordering::Relaxed));
        tokio::time::advance(budget - Duration::from_secs(1)).await;
        assert!(
            futures_util::poll!(&mut read).is_pending(),
            "consumer={consumer}"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            matches!(
                futures_util::poll!(&mut read),
                std::task::Poll::Ready(Err(DaemonClientError::Timeout))
            ),
            "consumer={consumer}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn local_create_hook_refusal_times_out_while_reading_the_body() {
    use std::sync::atomic::Ordering;

    let client = unix_client("/unused.sock").unwrap();
    let budget = Duration::from_secs(2);
    let (mut response, polled) = stalled_response(
        StatusCode::BAD_REQUEST,
        tokio::time::Instant::now() + budget,
    );
    response.headers_mut().insert(
        ERROR_CODE_HEADER,
        HeaderValue::from_static(ApiErrorCode::CreateHookFailed.as_str()),
    );
    let read = client.check_response(response, true, None);
    tokio::pin!(read);
    assert!(futures_util::poll!(&mut read).is_pending());
    assert!(
        polled.load(Ordering::Relaxed),
        "hook error body was not read"
    );
    tokio::time::advance(budget + Duration::from_secs(1)).await;
    assert!(matches!(
        futures_util::poll!(&mut read),
        std::task::Poll::Ready(Err(DaemonClientError::Timeout))
    ));
}

#[tokio::test]
async fn malformed_unix_json_uses_the_redacted_decode_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let mut used = 0;
        while !request[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut request[used..]).await.unwrap();
            assert!(count > 0, "request must contain complete HTTP headers");
            used += count;
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{")
            .await
            .unwrap();
    });
    let client = unix_client(&path).unwrap();
    let result = client
        .request_json::<serde_json::Value>(client.http.get("http://localhost/operation"))
        .await;
    assert!(matches!(
        result,
        Err(DaemonClientError::AuthenticatedDecode)
    ));
    server.await.unwrap();
}

async fn execute_unix_test(
    path: &std::path::Path,
    request: reqwest::Request,
) -> Result<reqwest::Response, DaemonClientError> {
    UnixTransport::new(path, verify_test_peer)
        .execute(request, None)
        .await
}

#[tokio::test]
async fn rejected_unix_peer_gets_no_http_bytes_and_never_uses_tcp() {
    use tokio::io::AsyncReadExt;

    fn reject(stream: &tokio::net::UnixStream, pid: Option<u32>) -> Result<(), DaemonClientError> {
        assert_eq!(pid, Some(42));
        assert!(stream.peer_cred().is_ok());
        Err(DaemonClientError::PeerIdentity)
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("api.sock");
    let unix = tokio::net::UnixListener::bind(&path).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = DaemonClient::new_unix(UnixTransport::new(path, reject)).unwrap();
    client.sessions_url = sessions_url(&format!("http://{}", tcp.local_addr().unwrap())).unwrap();
    let seen = tokio::spawn(async move {
        let (mut peer, _) = unix.accept().await.unwrap();
        assert_eq!(peer.read(&mut [0u8]).await.unwrap(), 0);
    });
    assert!(matches!(
        client.runtime_info_for_peer(42).await,
        Err(DaemonClientError::PeerIdentity)
    ));
    tokio::time::timeout(Duration::from_secs(5), seen)
        .await
        .unwrap()
        .unwrap();
    let tcp_accept = tcp.accept();
    tokio::pin!(tcp_accept);
    assert!(futures_util::poll!(&mut tcp_accept).is_pending());
}
