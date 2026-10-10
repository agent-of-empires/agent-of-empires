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

/// Reaps the ephemeral `opencode serve` process group on drop, so no attempt
/// leaks a server and two servers never share the SQLite store.
struct ServeGuard(Option<std::process::Child>);

impl Drop for ServeGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let pid = child.id();
            #[cfg(unix)]
            {
                signal_serve_group(pid, Signal::Term);
                let deadline = Instant::now() + Duration::from_millis(150);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        _ => break,
                    }
                }
                signal_serve_group(pid, Signal::Kill);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

enum Signal {
    Term,
    Kill,
}

/// Signals the process group, then the bare pid. No-op off unix.
fn signal_serve_group(pid: u32, signal: Signal) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, killpg, Signal as Nix};
        let sig = match signal {
            Signal::Term => Nix::SIGTERM,
            Signal::Kill => Nix::SIGKILL,
        };
        let p = nix::unistd::Pid::from_raw(pid as i32);
        let _ = killpg(p, sig);
        let _ = kill(p, sig);
    }
    #[cfg(not(unix))]
    let _ = (pid, signal);
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

/// Runs `call` against a short-lived `opencode serve` on the launch's own
/// environment, so both processes select the same store. The server is reaped
/// before this returns, which is what keeps two processes from holding the
/// store at once.
fn with_serve<T: Send + 'static>(
    project_path: &str,
    mut cmd: std::process::Command,
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().context("failed to spawn `opencode serve`")?;
    let _guard = ServeGuard(Some(child));

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
    std::thread::scope(|scope| {
        scope
            .spawn(run)
            .join()
            .map_err(|_| anyhow::anyhow!("opencode serve worker thread panicked"))?
    })
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
    with_serve(&owned_path, command, move |client| {
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
    with_serve(&project_path, command, move |client| {
        let parent_id = parent_id.clone();
        Box::pin(async move { client.fork_session(&parent_id).await })
    })
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
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            // One read can stop mid-request, so keep reading until the headers
            // are complete and the body they announce has arrived. A POST here
            // carries a JSON body, which routinely lands in a second segment.
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

    /// The guard is what keeps two servers from sharing the store, so it must
    /// have reaped the child before the helper returned.
    #[cfg(unix)]
    #[test]
    fn the_serve_guard_reaps_the_server_before_the_helper_returns() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "exec sleep 30"]);
        let child = ServeGuard(Some(cmd.spawn().unwrap()));
        let pid = child.0.as_ref().unwrap().id();
        drop(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // A reaped pid no longer resolves to a live process.
            let alive =
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok();
            if !alive {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the serve process outlived its guard"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
