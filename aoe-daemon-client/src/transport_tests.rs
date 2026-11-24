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

async fn serve_deadline_body<S>(
    mut stream: S,
    headers_after: Duration,
    chunks: usize,
    chunk_after: Duration,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut request = Vec::new();
    let mut buf = [0; 1024];
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let count = stream.read(&mut buf).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&buf[..count]);
    }
    tokio::time::sleep(headers_after).await;
    stream
        .write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {chunks}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    for _ in 0..chunks {
        tokio::time::sleep(chunk_after).await;
        if stream.write_all(b" ").await.is_err() {
            break;
        }
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

#[tokio::test]
async fn body_helpers_share_deadlines_without_shortening_long_operations() {
    for unix in [true, false] {
        for (headers_ms, chunks, chunk_ms, timeout_ms, expires) in [
            (0, 2, 0, 5000, false),
            (350, 1, 300, 500, true),
            (0, 10, 100, 500, true),
        ] {
            for consumer in 0..3 {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("api.sock");
                let (url, server) = if unix {
                    let listener = tokio::net::UnixListener::bind(&path).unwrap();
                    let server = tokio::spawn(async move {
                        let (stream, _) = listener.accept().await.unwrap();
                        serve_deadline_body(
                            stream,
                            Duration::from_millis(headers_ms),
                            chunks,
                            Duration::from_millis(chunk_ms),
                        )
                        .await;
                    });
                    ("http://localhost/operation".to_owned(), server)
                } else {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let url = format!("http://{}/operation", listener.local_addr().unwrap());
                    let server = tokio::spawn(async move {
                        let (stream, _) = listener.accept().await.unwrap();
                        serve_deadline_body(
                            stream,
                            Duration::from_millis(headers_ms),
                            chunks,
                            Duration::from_millis(chunk_ms),
                        )
                        .await;
                    });
                    (url, server)
                };
                let http = reqwest::Client::new();
                let request = http
                    .get(&url)
                    .timeout(Duration::from_millis(timeout_ms))
                    .build()
                    .unwrap();
                let mut response = if unix {
                    execute_unix_test(&path, request).await
                } else {
                    execute(&http, request).await
                }
                .unwrap();
                let client = DaemonClient::new(&url, None).unwrap();
                let result = match consumer {
                    0 => read_bounded_body(&mut response, 1024).await.map(|_| ()),
                    1 => discard_bounded_body(&mut response).await,
                    _ => client.read_error_body(&mut response).await.map(|_| ()),
                };
                assert_eq!(
                    matches!(result, Err(DaemonClientError::Timeout)),
                    expires,
                    "unix={unix}, consumer={consumer}, chunks={chunks}, result={result:?}"
                );
                if !expires {
                    result.unwrap();
                }
                drop(response);
                server.abort();
                let _ = server.await;
            }
        }
    }
}

#[tokio::test]
async fn create_hook_error_body_uses_the_request_deadline_and_bad_json_is_decode() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for malformed in [false, true] {
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
            if malformed {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{",
                    )
                    .await
                    .unwrap();
            } else {
                stream.write_all(b"HTTP/1.1 400 Bad Request\r\nAoE-Error-Code: create_hook_failed\r\nContent-Length: 100\r\n\r\n").await.unwrap();
                let mut eof = [0];
                let _ = stream.read(&mut eof).await;
            }
        });
        let client = unix_client(&path).unwrap();
        let request = client
            .http
            .get("http://localhost/operation")
            .timeout(Duration::from_millis(100));
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            client.request_json::<serde_json::Value>(request),
        )
        .await
        .unwrap();
        if malformed {
            assert!(matches!(
                result,
                Err(DaemonClientError::AuthenticatedDecode)
            ));
        } else {
            assert!(
                matches!(result, Err(DaemonClientError::Timeout)),
                "result={result:?}"
            );
        }
        server.await.unwrap();
    }
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
