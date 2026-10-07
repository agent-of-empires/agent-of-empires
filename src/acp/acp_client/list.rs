//! List-only ACP connection: `initialize`, then `session/list` pages, never `session/new` or
//! `session/load`. Creating or loading a session would narrow pi-acp's unfiltered list to that
//! session's cwd.

use std::time::Duration;

use agent_client_protocol::schema::v1::{InitializeResponse, ListSessionsRequest};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::handshake::build_initialize_request;
use super::spawn::{spawn_subprocess, SpawnConfig};
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
    let (mut child, _) =
        spawn_subprocess(&config).map_err(|e| ListSessionsError::Failed(e.to_string()))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill().await;
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
    let _ = child.kill().await;
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
        let owned = Owned::new(&owned_instances, Vec::new()).unwrap();
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
