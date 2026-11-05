//! Standalone ACP stdio queries without managed session authority.
//!
//! Listing sends `initialize`, then `session/list` pages, never `session/new` or
//! `session/load`: those would narrow pi-acp's unfiltered list to that session's cwd.
//! Catalog discovery alone creates one disposable session in a private directory.

use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ClientCapabilities, InitializeResponse, ListSessionsRequest, NewSessionRequest,
    NewSessionResponse, SessionConfigOption, SessionNotification, SessionUpdate,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use super::config_options::map_acp_config_option;
use super::handshake::build_initialize_request;
use super::spawn::{spawn_subprocess, SpawnConfig};
use crate::acp::state::ConfigOptionDescriptor;
use crate::session::import::{
    sort_newest_first, ImportableList, ImportableSession, Owned, MAX_SESSIONS,
};

/// Bound on `initialize` and on each `session/list` page.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on `session/list` pages for an agent whose order is unknown.
const MAX_PAGES: usize = 50;
/// ACP does not order `session/list`. These adapters sort their whole store newest first before
/// paging (pi-acp 0.0.34 `listPiSessions`), so the lister may stop at the cap.
const NEWEST_FIRST_AGENTS: &[&str] = &["pi"];

#[derive(Debug, thiserror::Error)]
pub enum ListSessionsError {
    #[error("not a built-in ACP agent")]
    UnknownAgent,
    #[error("agent adapter is not installed")]
    NotInstalled,
    #[error("agent is not allowed by the operator policy")]
    NotAllowed,
    #[error("agent does not advertise session/list and session/load")]
    Unsupported,
    #[error("agent timed out during {0}")]
    Timeout(&'static str),
    #[error("{0}")]
    Failed(String),
}

struct StandaloneProcess {
    child: tokio::process::Child,
    prime_runtime: Option<crate::process::prime::PrimeCleanupGuard>,
}

impl StandaloneProcess {
    async fn stop(&mut self) {
        let _ = self.child.kill().await;
    }
}

impl Drop for StandaloneProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        drop(self.prime_runtime.take());
    }
}

/// A cleanup task keeps the physical child and prime guard together even if its query is dropped.
struct StandaloneChild {
    shutdown: CancellationToken,
    cleanup: tokio::task::JoinHandle<()>,
}

impl StandaloneChild {
    fn new(
        child: tokio::process::Child,
        prime_runtime: Option<crate::process::prime::PrimeCleanupGuard>,
    ) -> Self {
        let mut process = StandaloneProcess {
            child,
            prime_runtime,
        };
        let shutdown = CancellationToken::new();
        let requested = shutdown.clone();
        let cleanup = tokio::spawn(async move {
            requested.cancelled().await;
            process.stop().await;
        });
        Self { shutdown, cleanup }
    }

    async fn finish(mut self) {
        self.shutdown.cancel();
        let _ = (&mut self.cleanup).await;
    }
}

impl Drop for StandaloneChild {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Discover only the first nonempty catalog. No worker, replay, Current identity, filesystem,
/// terminal, permission, or elicitation handlers are installed on this disposable connection.
pub(crate) async fn probe_option_catalog(
    config: SpawnConfig,
    handshake_timeout: Duration,
    snapshot_timeout: Duration,
) -> anyhow::Result<Option<Vec<ConfigOptionDescriptor>>> {
    let (mut child, _, prime_runtime) = spawn_subprocess(&config)?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let child = StandaloneChild::new(child, prime_runtime);
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        child.finish().await;
        anyhow::bail!("agent has no stdio");
    };
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let (snapshot_tx, mut snapshot_rx) = mpsc::channel(1);
    let notifications = snapshot_tx.clone();
    let result = Client
        .builder()
        .name("aoe-acp-catalog")
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let snapshots = notifications.clone();
                async move {
                    if let SessionUpdate::ConfigOptionUpdate(update) = notification.update {
                        capture_catalog_snapshot(Some(update.config_options), &snapshots);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
            tokio::time::timeout(handshake_timeout, async {
                // Reuse client identity, but advertise no authority to serve agent requests.
                connection
                    .send_request(
                        build_initialize_request().client_capabilities(ClientCapabilities::new()),
                    )
                    .block_task()
                    .await?;
                let (response_tx, response_rx) = oneshot::channel();
                connection
                    .send_request(NewSessionRequest::new(config.cwd))
                    .on_receiving_result(
                        move |result: Result<NewSessionResponse, agent_client_protocol::Error>| {
                            async move {
                                // Ordered response consumption preserves precedence against Native
                                // notifications before and after the session/new response.
                                let result = result.map(|response| {
                                    capture_catalog_snapshot(response.config_options, &snapshot_tx);
                                });
                                let _ = response_tx.send(result);
                                Ok(())
                            }
                        },
                    )?;
                response_rx.await.map_err(|_| {
                    agent_client_protocol::Error::internal_error()
                        .data("catalog session/new response dropped")
                })?
            })
            .await
            .map_err(|_| {
                agent_client_protocol::Error::internal_error().data("catalog handshake timed out")
            })??;
            Ok(tokio::time::timeout(snapshot_timeout, snapshot_rx.recv())
                .await
                .ok()
                .flatten())
        })
        .await;
    child.finish().await;
    Ok(result?)
}

fn capture_catalog_snapshot(
    raw: Option<Vec<SessionConfigOption>>,
    snapshots: &mpsc::Sender<Vec<ConfigOptionDescriptor>>,
) {
    let Some(raw) = raw else {
        return;
    };
    let options: Vec<_> = raw.into_iter().filter_map(map_acp_config_option).collect();
    if !options.is_empty() {
        // Capacity one keeps the first nonempty snapshot without buffering unbounded updates.
        let _ = snapshots.try_send(options);
    }
}

/// Native sessions in the agent's own store that `owned` does not exclude, up to
/// [`MAX_SESSIONS`]. Filtering per page keeps owned sessions from using up the cap.
pub async fn list_native_sessions(
    config: SpawnConfig,
    owned: &Owned,
) -> Result<ImportableList, ListSessionsError> {
    list_with_timeout(config, owned, STEP_TIMEOUT).await
}

async fn list_with_timeout(
    config: SpawnConfig,
    owned: &Owned,
    step_timeout: Duration,
) -> Result<ImportableList, ListSessionsError> {
    let (mut child, _, prime_runtime) =
        spawn_subprocess(&config).map_err(|e| ListSessionsError::Failed(e.to_string()))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let child = StandaloneChild::new(child, prime_runtime);
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        child.finish().await;
        return Err(ListSessionsError::Failed("agent has no stdio".into()));
    };
    let newest_first = NEWEST_FIRST_AGENTS.contains(&config.agent_key.as_str());
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let result = Client
        .builder()
        .name("aoe-acp")
        .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
            Ok(list_pages(&connection, owned, newest_first, step_timeout).await)
        })
        .await
        .unwrap_or_else(|e| Err(ListSessionsError::Failed(e.to_string())));
    child.finish().await;
    result
}

async fn list_pages(
    connection: &ConnectionTo<Agent>,
    owned: &Owned,
    newest_first: bool,
    step_timeout: Duration,
) -> Result<ImportableList, ListSessionsError> {
    let failed = |e: agent_client_protocol::Error| ListSessionsError::Failed(e.to_string());
    let init: InitializeResponse = tokio::time::timeout(
        step_timeout,
        connection
            .send_request(build_initialize_request())
            .block_task(),
    )
    .await
    .map_err(|_| ListSessionsError::Timeout("initialize"))?
    .map_err(failed)?;
    let caps = &init.agent_capabilities;
    if !caps.load_session || caps.session_capabilities.list.is_none() {
        return Err(ListSessionsError::Unsupported);
    }

    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        pages += 1;
        let page = tokio::time::timeout(
            step_timeout,
            connection
                .send_request(ListSessionsRequest::new().cursor(cursor.take()))
                .block_task(),
        )
        .await
        .map_err(|_| ListSessionsError::Timeout("session/list"))?
        .map_err(failed)?;
        let page_empty = page.sessions.is_empty();
        for info in page.sessions {
            let s = ImportableSession::new(
                info.session_id.0.to_string(),
                info.cwd.to_string_lossy().into_owned(),
                info.title,
                info.updated_at,
            );
            if !owned.excludes(&s.session_id, &s.cwd) {
                sessions.push(s);
            }
        }
        let more = page.next_cursor.is_some() && !page_empty;
        let capped = (newest_first && sessions.len() >= MAX_SESSIONS) || pages >= MAX_PAGES;
        if capped || !more {
            let truncated = sessions.len() > MAX_SESSIONS || more;
            sort_newest_first(&mut sessions);
            sessions.truncate(MAX_SESSIONS);
            return Ok(ImportableList {
                sessions,
                truncated,
            });
        }
        cursor = page.next_cursor;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::reset_fake_spawn_config;

    #[test]
    fn catalog_keeps_first_nonempty_snapshot() {
        use agent_client_protocol::schema::v1::SessionConfigSelectOption;

        let (snapshots, mut received) = mpsc::channel(1);
        capture_catalog_snapshot(None, &snapshots);
        capture_catalog_snapshot(Some(Vec::new()), &snapshots);
        assert!(received.try_recv().is_err());

        for id in ["first", "later"] {
            capture_catalog_snapshot(
                Some(vec![SessionConfigOption::select(
                    id,
                    "Model",
                    "current",
                    vec![SessionConfigSelectOption::new("current", "Current")],
                )]),
                &snapshots,
            );
        }
        let options = received.try_recv().unwrap();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id, "first");
        assert!(received.try_recv().is_err());
    }

    /// A stub agent that appends each request line to `requests.log` and answers `initialize`
    /// with `init_caps`. Page `N` (cursor `N`, first page `0`) returns `page_size` entries, with
    /// `nextCursor` until `pages` pages were served. `stall` names a method left unanswered.
    fn stub(dir: &std::path::Path, init_caps: &str, page_size: u32, pages: u32, stall: &str) {
        stub_stamped(
            dir,
            init_caps,
            page_size,
            pages,
            stall,
            "2026-01-01T00:00:00Z",
        );
    }

    /// `stamp` is the shell expression for entry `$i`'s `updatedAt`.
    fn stub_stamped(
        dir: &std::path::Path,
        init_caps: &str,
        page_size: u32,
        pages: u32,
        stall: &str,
        stamp: &str,
    ) {
        let log = dir.join("requests.log");
        let script = format!(
            r#"#!/bin/sh
entries() {{
  i=$1; end=$(($1+$2)); out=''
  while [ $i -lt $end ]; do
    out="$out{{\"sessionId\":\"s$i\",\"cwd\":\"/p/$i\",\"title\":\"t$i\",\"updatedAt\":\"{stamp}\"}},"
    i=$((i+1))
  done
  printf '%s' "${{out%,}}"
}}
while IFS= read -r line; do
  printf '%s\n' "$line" >> '{log}'
  id=$(printf '%s' "$line" | sed -En 's/.*"id":("[^"]*"|[0-9]+).*/\1/p')
  case $line in
    *'"method":"{stall}"'*) ;;
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":1,"agentCapabilities":{init_caps}}}}}\n' "$id" ;;
    *'"method":"session/list"'*)
      page=$(printf '%s' "$line" | sed -En 's/.*"cursor":"([0-9]+)".*/\1/p'); page=${{page:-0}}
      next=''; [ $((page+1)) -lt {pages} ] && next=",\"nextCursor\":\"$((page+1))\""
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessions":[%s]%s}}}}\n' "$id" "$(entries $((page*{page_size})) {page_size})" "$next" ;;
  esac
done
"#,
            log = log.display(),
        );
        std::fs::write(dir.join("agent.sh"), script).unwrap();
    }

    const LISTING: &str = r#"{"loadSession":true,"sessionCapabilities":{"list":{}}}"#;

    async fn run(dir: &std::path::Path) -> Result<ImportableList, ListSessionsError> {
        run_owned(dir, &Owned::default()).await
    }

    async fn run_owned(
        dir: &std::path::Path,
        owned: &Owned,
    ) -> Result<ImportableList, ListSessionsError> {
        run_as(dir, owned, "codex").await
    }

    async fn run_as(
        dir: &std::path::Path,
        owned: &Owned,
        agent: &str,
    ) -> Result<ImportableList, ListSessionsError> {
        let mut config = reset_fake_spawn_config(&dir.join("agent.sh"), dir);
        config.agent_key = agent.into();
        list_with_timeout(config, owned, Duration::from_millis(1500)).await
    }

    fn methods(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("requests.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["method"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn pages_through_list_without_creating_a_session() {
        // (page_size, pages, expected count, truncated, list calls)
        for (page_size, pages, count, truncated, calls) in [
            (2, 1, 2, false, 1),
            (120, 2, 200, true, 2),
            (100, 2, 200, false, 2),
            (100, 3, 200, true, 3),
            (1, MAX_PAGES as u32 + 1, MAX_PAGES, true, MAX_PAGES),
        ] {
            let dir = tempfile::tempdir().unwrap();
            stub(dir.path(), LISTING, page_size, pages, "none");
            let ImportableList {
                sessions,
                truncated: got_truncated,
            } = run(dir.path()).await.unwrap();
            let case = format!("page_size={page_size} pages={pages}");
            assert_eq!(sessions.len(), count, "{case}");
            assert_eq!(got_truncated, truncated, "{case}");
            let mut expected = vec!["initialize".to_string()];
            expected.extend(std::iter::repeat_n("session/list".to_string(), calls));
            assert_eq!(methods(dir.path()), expected, "{case}");
            assert_eq!(
                sessions[1],
                ImportableSession {
                    session_id: "s1".into(),
                    cwd: "/p/1".into(),
                    title: Some("t1".into()),
                    updated_at: Some("2026-01-01T00:00:00Z".into()),
                    cwd_exists: false,
                    conversation_binding: None,
                },
                "{case}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        stub(dir.path(), LISTING, 120, 2, "none");
        run(dir.path()).await.unwrap();
        let log = std::fs::read_to_string(dir.path().join("requests.log")).unwrap();
        let lists: Vec<serde_json::Value> = log
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|r| r["method"] == "session/list")
            .collect();
        assert!(lists.iter().all(|r| r["params"].get("cwd").is_none()));
        assert_eq!(lists[1]["params"]["cursor"], "1");
    }

    #[tokio::test]
    async fn only_a_newest_first_adapter_stops_paging_at_the_cap() {
        // Two descending pages, then a third page newer than both.
        let stamp = r#"$([ $i -ge 200 ] && echo 2026-02-01T00:00:00Z || printf '2026-01-01T%02d:%02d:00Z' $(((299-i)/60)) $(((299-i)%60)))"#;
        for (agent, calls, first, last) in [("codex", 3, "s200", "s99"), ("pi", 2, "s0", "s199")] {
            let dir = tempfile::tempdir().unwrap();
            stub_stamped(dir.path(), LISTING, 100, 3, "none", stamp);
            let list = run_as(dir.path(), &Owned::default(), agent).await.unwrap();
            let mut expected = vec!["initialize".to_string()];
            expected.extend(std::iter::repeat_n("session/list".to_string(), calls));
            assert_eq!(methods(dir.path()), expected, "{agent}");
            assert_eq!(list.sessions.len(), MAX_SESSIONS, "{agent}");
            assert_eq!(list.sessions[0].session_id, first, "{agent}");
            assert_eq!(list.sessions[MAX_SESSIONS - 1].session_id, last, "{agent}");
            assert!(list.truncated, "{agent}");
        }
    }

    #[tokio::test]
    async fn owned_sessions_do_not_use_up_the_cap() {
        let owned_instances: Vec<_> = ["s0", "s1"]
            .into_iter()
            .map(|id| {
                let mut inst = crate::session::Instance::new("t", "/tmp");
                inst.acp_session_id = Some(id.into());
                inst
            })
            .collect();
        let owned = Owned::new(&owned_instances, Vec::new());
        let dir = tempfile::tempdir().unwrap();
        stub(dir.path(), LISTING, 101, 2, "none");
        let list = run_owned(dir.path(), &owned).await.unwrap();
        assert_eq!(list.sessions.len(), MAX_SESSIONS);
        assert_eq!(list.sessions[0].session_id, "s2");
        assert!(!list.truncated);
    }

    #[tokio::test]
    async fn refuses_agents_without_list_and_load() {
        for caps in [
            r#"{"loadSession":true}"#,
            r#"{"loadSession":false,"sessionCapabilities":{"list":{}}}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            stub(dir.path(), caps, 1, 1, "none");
            assert!(
                matches!(run(dir.path()).await, Err(ListSessionsError::Unsupported)),
                "{caps}"
            );
            assert_eq!(methods(dir.path()), ["initialize"], "{caps}");
        }
    }

    #[tokio::test]
    async fn a_stalled_step_is_an_error_not_a_partial_list() {
        for (stall, step) in [
            ("initialize", "initialize"),
            ("session/list", "session/list"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            stub(dir.path(), LISTING, 1, 1, stall);
            match run(dir.path()).await {
                Err(ListSessionsError::Timeout(got)) => assert_eq!(got, step),
                other => panic!("stall on {stall}: {other:?}"),
            }
        }
    }
}
