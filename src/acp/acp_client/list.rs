//! List-only ACP connection: `initialize`, then `session/list` pages, never `session/new` or
//! `session/load`. Creating or loading a session would narrow pi-acp's unfiltered list to that
//! session's cwd.

use std::time::Duration;

use agent_client_protocol::schema::v1::{InitializeResponse, ListSessionsRequest};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::handshake::build_initialize_request;
use super::spawn::{spawn_subprocess, SpawnConfig};
use crate::session::import::{ImportableSession, MAX_SESSIONS};

/// Bound on `initialize` and on each `session/list` page.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ListSessionsError {
    #[error("not a built-in ACP agent")]
    UnknownAgent,
    #[error("agent adapter is not installed")]
    NotInstalled,
    #[error("agent does not advertise session/list and session/load")]
    Unsupported,
    #[error("agent timed out during {0}")]
    Timeout(&'static str),
    #[error("{0}")]
    Failed(String),
}

/// Native sessions in the agent's own store, up to [`MAX_SESSIONS`], and whether more exist.
pub async fn list_native_sessions(
    config: SpawnConfig,
) -> Result<(Vec<ImportableSession>, bool), ListSessionsError> {
    list_with_timeout(config, STEP_TIMEOUT).await
}

async fn list_with_timeout(
    config: SpawnConfig,
    step_timeout: Duration,
) -> Result<(Vec<ImportableSession>, bool), ListSessionsError> {
    let (mut child, _) =
        spawn_subprocess(&config).map_err(|e| ListSessionsError::Failed(e.to_string()))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill().await;
        return Err(ListSessionsError::Failed("agent has no stdio".into()));
    };
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let result = Client
        .builder()
        .name("aoe-acp")
        .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
            Ok(list_pages(&connection, step_timeout).await)
        })
        .await
        .unwrap_or_else(|e| Err(ListSessionsError::Failed(e.to_string())));
    let _ = child.kill().await;
    result
}

async fn list_pages(
    connection: &ConnectionTo<Agent>,
    step_timeout: Duration,
) -> Result<(Vec<ImportableSession>, bool), ListSessionsError> {
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
    loop {
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
        sessions.extend(page.sessions.into_iter().map(|s| {
            ImportableSession::new(
                s.session_id.0.to_string(),
                s.cwd.to_string_lossy().into_owned(),
                s.title,
                s.updated_at,
            )
        }));
        let more = page.next_cursor.is_some();
        if sessions.len() >= MAX_SESSIONS || !more || page_empty {
            let truncated = sessions.len() > MAX_SESSIONS || (more && !page_empty);
            sessions.truncate(MAX_SESSIONS);
            return Ok((sessions, truncated));
        }
        cursor = page.next_cursor;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::reset_fake_spawn_config;

    /// A stub agent that appends each request line to `requests.log` and answers `initialize`
    /// with `init_caps`. Page `N` (cursor `N`, first page `0`) returns `page_size` entries, with
    /// `nextCursor` until `pages` pages were served. `stall` names a method left unanswered.
    fn stub(dir: &std::path::Path, init_caps: &str, page_size: u32, pages: u32, stall: &str) {
        let log = dir.join("requests.log");
        let script = format!(
            r#"#!/bin/sh
entries() {{
  i=$1; end=$(($1+$2)); out=''
  while [ $i -lt $end ]; do
    out="$out{{\"sessionId\":\"s$i\",\"cwd\":\"/p/$i\",\"title\":\"t$i\",\"updatedAt\":\"2026-01-01T00:00:00Z\"}},"
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

    async fn run(
        dir: &std::path::Path,
    ) -> Result<(Vec<ImportableSession>, bool), ListSessionsError> {
        let config = reset_fake_spawn_config(&dir.join("agent.sh"), dir);
        list_with_timeout(config, Duration::from_millis(1500)).await
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
            (100, 3, 200, true, 2),
        ] {
            let dir = tempfile::tempdir().unwrap();
            stub(dir.path(), LISTING, page_size, pages, "none");
            let (sessions, got_truncated) = run(dir.path()).await.unwrap();
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
