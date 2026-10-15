//! OpenCode session id preassignment and forking through a short-lived
//! `opencode serve`.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::agents::AgentGeneration;

/// Budget for the serve boot plus the request that follows it (boot measured at
/// ~1.8s). Covers both the preassign and the fork.
const OPENCODE_SERVE_DEADLINE: Duration = Duration::from_secs(6);

const OPENCODE_SERVE_TERMINATION_GRACE: Duration = Duration::from_millis(150);

struct ServeGuard {
    child: Option<std::process::Child>,
    grace: Duration,
}

impl ServeGuard {
    fn finish(mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            crate::process::terminate_and_reap_process_group(&mut child, self.grace)
                .context("failed to stop the ephemeral OpenCode server")?;
        }
        Ok(())
    }
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = crate::process::terminate_and_reap_process_group(&mut child, self.grace);
        }
    }
}

/// An authenticated client for the ephemeral server. OpenCode 2.x refuses every
/// unauthenticated request, so the password this pins is what makes the
/// readiness probe and the call that follows it possible at all.
struct ServeClient {
    client: reqwest::Client,
    base: String,
}

impl ServeClient {
    fn new(base: String, password: &str) -> Result<Self> {
        use base64::Engine;
        let token =
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Basic {token}"))
                .context("the pinned OpenCode server password is not a valid header value")?,
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .default_headers(headers)
            .build()
            .context("failed to build the OpenCode serve HTTP client")?;
        Ok(Self { client, base })
    }

    async fn await_ready(&self) -> Result<()> {
        let deadline = Instant::now() + OPENCODE_SERVE_DEADLINE;
        loop {
            if let Ok(resp) = self
                .client
                .get(format!("{}/api/session", self.base))
                .send()
                .await
            {
                if resp.status().is_success() {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("opencode serve did not become ready within the deadline");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Creates the session the launch will own. Only the current generation
    /// exposes this route; the caller has already refused the other one.
    async fn create_session(&self, id: &str, project_path: &str) -> Result<()> {
        let body = serde_json::json!({
            "id": id,
            "location": { "directory": project_path },
        });
        let resp = self
            .client
            .post(format!("{}/api/session", self.base))
            .json(&body)
            .send()
            .await
            .context("opencode preassign POST /api/session failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("opencode preassign POST returned {}", resp.status());
        }
        let created: serde_json::Value = resp
            .json()
            .await
            .context("opencode preassign response was not JSON")?;
        let created_id = created
            .get("data")
            .and_then(|d| d.get("id"))
            .and_then(|v| v.as_str());
        if created_id != Some(id) {
            anyhow::bail!("opencode assigned {created_id:?}, expected {id}");
        }
        Ok(())
    }

    /// Asks the store for a child of `parent_id`. The id it returns replaces the
    /// one AoE pre-pinned, because only the store knows which conversation the
    /// child continues. Only the current generation reaches here; the older one
    /// forks through its own root flag.
    async fn fork_session(&self, parent_id: &str) -> Result<String> {
        let resp = self
            .client
            .post(format!("{}/api/session/{parent_id}/fork", self.base))
            .json(&serde_json::json!({}))
            .send()
            .await
            .with_context(|| format!("opencode fork of {parent_id} failed"))?;
        let status = resp.status();
        anyhow::ensure!(
            status.is_success(),
            "opencode fork of {parent_id} returned {status}"
        );
        let body: serde_json::Value = resp
            .json()
            .await
            .context("opencode fork response was not JSON")?;
        body.pointer("/data/id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("opencode fork of {parent_id} carried no child id: {body}"))
    }
}

/// Uses the launch environment and stops the isolated server group before returning.
fn with_serve<T: Send + 'static>(
    project_path: &str,
    mut cmd: std::process::Command,
    grace: Duration,
    call: impl FnOnce(ServeClient) -> Pin<Box<dyn Future<Output = Result<T>> + Send>> + Send,
) -> Result<T> {
    // The bind/drop/bind race is covered by the readiness deadline.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .context("failed to reserve a loopback port for opencode serve")?
        .local_addr()
        .context("failed to read the reserved loopback port")?
        .port();
    // Pinning the password keeps the server's own random one off AoE's stderr,
    // which stays discarded, and an inherited value cannot make the probe 401.
    let password = Uuid::new_v4().simple().to_string();

    cmd.args([
        "serve",
        "--hostname",
        "127.0.0.1",
        "--port",
        &port.to_string(),
    ])
    .env("OPENCODE_PASSWORD", &password)
    .current_dir(project_path)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let child = crate::process::spawn_process_group(&mut cmd)
        .context("failed to spawn `opencode serve`")?;
    let guard = ServeGuard {
        child: Some(child),
        grace,
    };

    let base = format!("http://127.0.0.1:{port}");
    // The caller may already be inside a Tokio runtime, so the blocking runtime
    // runs on its own OS thread.
    let run = || -> Result<T> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build the opencode serve runtime")?;
        let client = ServeClient::new(base, &password)?;
        rt.block_on(async {
            client.await_ready().await?;
            call(client).await
        })
    };
    let result = std::thread::scope(|scope| {
        scope
            .spawn(run)
            .join()
            .map_err(|_| anyhow::anyhow!("opencode serve worker thread panicked"))?
    });
    guard.finish()?;
    result
}

/// Creates the OpenCode session up front under the launch's own `environment`
/// so both processes select the same store. Any failure leaves it unowned.
pub(crate) fn preassign_opencode_session_id(
    project_path: &str,
    command: std::process::Command,
    generation: AgentGeneration,
) -> Option<String> {
    // 1.16.0 has no create route under `/api`, but 1.18.29 does. An unreadable
    // help establishes nothing about either. Answering before the server is
    // spawned is the point: the alternative is booting one and waiting out its
    // readiness to learn there was nothing to send.
    match generation {
        AgentGeneration::Legacy => {
            tracing::warn!(target: "session.capture",
                "opencode 1.x may or may not have a session-create route under /api, so its session id cannot be preassigned");
            return None;
        }
        AgentGeneration::Unknown => {
            tracing::warn!(target: "session.capture",
                "opencode --help did not answer, so its generation cannot be established; \
                 its session id is not preassigned");
            return None;
        }
        AgentGeneration::Current => {}
    }
    let id = format!("ses_{}", Uuid::new_v4().simple());
    let owned_path = project_path.to_owned();
    let served_path = owned_path.clone();
    with_serve(&owned_path, command, OPENCODE_SERVE_TERMINATION_GRACE, move |client| {
        let id = id.clone();
        Box::pin(async move { client.create_session(&id, &served_path).await.map(|()| id) })
    })
    .map_err(|e| {
        tracing::warn!(target: "session.capture", error = %e, "opencode session preassignment failed");
    })
    .ok()
    .and_then(super::validated_session_id)
}

/// Forks `parent_id` in the store the launch selects and returns the child the
/// store minted. `None` leaves the fork refused rather than started unforked.
pub(crate) fn fork_opencode_session_id(
    project_path: &str,
    command: std::process::Command,
    parent_id: &str,
) -> Option<String> {
    let Some(parent_id) = super::validated_session_id(parent_id.to_owned()) else {
        tracing::warn!(target: "session.capture", %parent_id, "refusing to fork an invalid OpenCode session id");
        return None;
    };
    let project_path = project_path.to_owned();
    with_serve(
        &project_path,
        command,
        OPENCODE_SERVE_TERMINATION_GRACE,
        move |client| {
            let parent_id = parent_id.clone();
            Box::pin(async move { client.fork_session(&parent_id).await })
        },
    )
    .map_err(|e| {
        tracing::warn!(target: "session.capture", error = %e, "opencode session fork failed");
    })
    .ok()
    .and_then(super::validated_session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Reads one request from a listener that answers with `response`, and
    /// returns the request line plus whatever `Authorization` header arrived.
    fn serve_once(response: &'static str) -> (std::thread::JoinHandle<String>, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("local HTTP fixture did not receive a request: {error}"),
                }
            };
            // Accepted sockets inherit nonblocking mode on macOS.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            // Read the complete headers and announced body before replying.
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
                    let announced = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + announced {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            String::from_utf8_lossy(&request).to_string()
        });
        (handle, port)
    }

    fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request
            .lines()
            .find(|line| {
                line.to_ascii_lowercase()
                    .starts_with(&format!("{name}:").to_ascii_lowercase())
            })
            .map(|line| {
                line.split_once(':')
                    .map(|(_, value)| value.trim())
                    .unwrap_or_default()
            })
    }

    /// Drives one client call against the stub on its own runtime, so the tests
    /// never inherit the caller's.
    fn block_on<T>(
        call: impl FnOnce(ServeClient) -> Pin<Box<dyn Future<Output = Result<T>> + Send>>,
        port: u16,
    ) -> Result<T> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = ServeClient::new(format!("http://127.0.0.1:{port}"), "pw")?;
        rt.block_on(call(client))
    }

    /// OpenCode 2.x refuses every unauthenticated request, so the password AoE
    /// pins is what makes the readiness probe possible at all.
    #[test]
    fn the_serve_client_authenticates_with_the_pinned_password() {
        use base64::Engine;
        let (handle, port) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"data\":{}}",
        );
        block_on(
            |client| Box::pin(async move { client.await_ready().await }),
            port,
        )
        .unwrap();
        let request = handle.join().unwrap();
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("opencode:pw")
        );
        assert_eq!(header(&request, "authorization"), Some(expected.as_str()));
    }

    #[test]
    fn the_loopback_client_bypasses_ambient_proxies() {
        const CHILD_ENV: &str = "AOE_LOOPBACK_PROXY_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            the_serve_client_authenticates_with_the_pinned_password();
            println!("loopback-child-completed");
            return;
        }
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        proxy.set_nonblocking(true).unwrap();
        let url = format!("http://{}", proxy.local_addr().unwrap());
        let module = module_path!().split_once("::").unwrap().1;
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                &format!("{module}::the_loopback_client_bypasses_ambient_proxies"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy");
        for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            command.env(name, &url);
        }
        let output =
            crate::process::run_with_timeout_process_group(&mut command, Duration::from_secs(20))
                .unwrap()
                .expect("proxy-isolation child timed out");
        assert!(
            matches!(proxy.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "loopback credentials were routed through a proxy"
        );
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("loopback-child-completed"));
    }
    /// The store mints the child, so the request names the parent in its path
    /// and the id that comes back is the one AoE must adopt.
    #[test]
    fn the_fork_request_names_the_parent_and_returns_the_stored_child() {
        let (handle, port) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 51\r\n\r\n{\"data\":{\"id\":\"ses_child000000000000000000000000\"}}",
        );
        let child = block_on(
            |client| {
                let parent = "ses_parent00000000000000000000000".to_string();
                Box::pin(async move { client.fork_session(&parent).await })
            },
            port,
        );
        assert_eq!(child.unwrap(), "ses_child000000000000000000000000");
        let request = handle.join().unwrap();
        assert!(
            request.starts_with("POST /api/session/ses_parent00000000000000000000000/fork "),
            "{request}"
        );
        assert!(header(&request, "authorization").is_some(), "{request}");
    }

    /// 1.x exposes no create route under `/api`, so a launch against it must be
    /// told so instead of waiting out a request that cannot succeed.
    #[test]
    fn the_legacy_generation_refuses_before_it_spawns_a_server() {
        let dir = tempfile::tempdir().unwrap();
        // The agent leaves a mark if it runs at all, so a mark proves the
        // refusal came after the spawn rather than before it.
        let marker = dir.path().join("spawned");
        let agent = dir.path().join("opencode");
        std::fs::write(
            &agent,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let id = preassign_opencode_session_id(
            dir.path().to_str().unwrap(),
            std::process::Command::new(&agent),
            AgentGeneration::Legacy,
        );
        assert!(id.is_none(), "1.x has no create route to preassign against");
        // The marker is the observation: no spawn happened. Elapsed time would
        // only assert how busy the machine was.
        assert!(!marker.exists(), "the agent must never be spawned");
    }

    #[test]
    fn the_fork_rejects_a_response_without_a_child_id() {
        let (handle, port) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"data\":{}}",
        );
        let result = block_on(
            |client| {
                let parent = "ses_parent".to_string();
                Box::pin(async move { client.fork_session(&parent).await })
            },
            port,
        );
        assert!(result.is_err(), "a fork without a child id is refused");
        let _ = handle.join();
    }

    #[cfg(unix)]
    #[test]
    fn serve_cleanup_waits_for_a_cooperative_descendant_on_every_return_path() {
        let Ok(python) = which::which("python3") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("opencode");
        std::fs::write(
            &script,
            r#"import os, signal, socket, sys
if os.fork():
    os.wait()
    sys.exit(0)
control = socket.create_connection(('127.0.0.1', int(os.environ['SHUTDOWN_CONTROL_PORT'])), timeout=3)
def stopped(signum, frame):
    control.sendall(b'T')
    if control.recv(1) != b'R': sys.exit(2)
    with open(os.environ['SHUTDOWN_COMPLETE'], 'w') as marker: marker.write('finished')
    sys.exit(0)
signal.signal(signal.SIGTERM, stopped)
server = socket.socket()
server.bind(('127.0.0.1', int(sys.argv[sys.argv.index('--port') + 1])))
server.listen()
server.settimeout(15)
while True:
    connection, _ = server.accept()
    with connection:
        connection.recv(8192)
        connection.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{"data":{}}')
"#,
        )
        .unwrap();
        for outcome in ["success", "error", "panic"] {
            let finished = directory.path().join(outcome);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut command = std::process::Command::new(&python);
            command
                .arg(&script)
                .env("SHUTDOWN_COMPLETE", &finished)
                .env(
                    "SHUTDOWN_CONTROL_PORT",
                    listener.local_addr().unwrap().port().to_string(),
                );
            let (sender, receiver) = std::sync::mpsc::channel();
            let project_path = directory.path().to_str().unwrap();
            let result = std::thread::scope(|scope| {
                let worker = scope.spawn(move || {
                    let result =
                        with_serve(project_path, command, Duration::from_secs(5), move |_| {
                            Box::pin(async move {
                                match outcome {
                                    "success" => Ok("completed"),
                                    "error" => anyhow::bail!("call failed"),
                                    _ => panic!("worker failed"),
                                }
                            })
                        });
                    let _ = sender.send(result);
                });
                let deadline = Instant::now() + Duration::from_secs(8);
                let mut control = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("shutdown control did not connect: {error}"),
                    }
                };
                control.set_nonblocking(false).unwrap();
                control
                    .set_read_timeout(Some(Duration::from_secs(8)))
                    .unwrap();
                control
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut entered = [0];
                control.read_exact(&mut entered).unwrap();
                assert_eq!(entered, *b"T");
                assert!(!finished.exists());
                assert!(
                    matches!(
                        receiver.recv_timeout(Duration::from_millis(100)),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ),
                    "helper returned while descendant shutdown was blocked"
                );
                control.write_all(b"R").unwrap();
                let result = receiver.recv_timeout(Duration::from_secs(8)).unwrap();
                worker.join().unwrap();
                result
            });
            assert_eq!(
                result.is_ok(),
                outcome == "success",
                "{outcome}: {result:?}"
            );
            assert_eq!(
                std::fs::read_to_string(&finished).unwrap(),
                "finished",
                "{outcome}: descendant shutdown must finish before the helper returns"
            );
        }
    }
}
