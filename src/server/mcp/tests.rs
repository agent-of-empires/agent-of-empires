use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::server::test_helpers::vecs;
use crate::server::test_support;
use crate::session::Instance;

const TOKEN: &str = "mcp-test-token";

fn app_with_state(
    mcp_enabled: bool,
    prior: Vec<Instance>,
) -> (axum::Router, std::sync::Arc<crate::server::AppState>) {
    let state = test_support::build_test_app_state_with_policy_configured(
        prior,
        vecs(&["localhost"]),
        Vec::new(),
        Some(TOKEN.to_string()),
        |state| state.web_config.mcp_enabled = mcp_enabled,
    );
    (test_support::build_router_for_test(state.clone()), state)
}

fn app(mcp_enabled: bool, prior: Vec<Instance>) -> axum::Router {
    app_with_state(mcp_enabled, prior).0
}

async fn send(app: &axum::Router, mut req: Request<axum::body::Body>) -> axum::response::Response {
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            5555,
        ))));
    app.clone().oneshot(req).await.unwrap()
}

/// POST one JSON-RPC message to `/mcp`; returns the status and the JSON-RPC response.
async fn rpc(app: &axum::Router, token: Option<&str>, body: Value) -> (StatusCode, Option<Value>) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let Some(token) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let resp = send(
        app,
        req.body(axum::body::Body::from(body.to_string())).unwrap(),
    )
    .await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).ok())
}

async fn initialize(app: &axum::Router) {
    let (status, message) = rpc(
        app,
        Some(TOKEN),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"}
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result = message.unwrap()["result"].clone();
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(result["serverInfo"]["name"], "aoe");
    let (status, _) = rpc(
        app,
        Some(TOKEN),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

/// The whole `tools/call` result, so a test can read a tool error.
async fn call_result(app: &axum::Router, name: &str, arguments: Value) -> Value {
    let (status, message) = rpc(
        app,
        Some(TOKEN),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    message.unwrap()["result"].clone()
}

async fn call(app: &axum::Router, name: &str, arguments: Value) -> Value {
    let (status, message) = rpc(
        app,
        Some(TOKEN),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result = message.unwrap()["result"].clone();
    assert_ne!(result["isError"], json!(true), "{result}");
    result["structuredContent"].clone()
}

fn session_in(title: &str, group: &str, parent: Option<&str>) -> Instance {
    let mut inst = Instance::new(title, "/repo");
    inst.group_path = group.to_string();
    inst.parent_session_id = parent.map(str::to_string);
    inst
}

#[tokio::test]
async fn route_exists_only_when_setting_is_on() {
    let off = app(false, vec![]);
    let (status, _) = rpc(&off, Some(TOKEN), json!({"jsonrpc": "2.0"})).await;
    let unrouted = Request::builder()
        .method("POST")
        .uri("/nope")
        .header(header::HOST, "localhost")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let nope = send(&off, unrouted).await.status();
    assert_eq!(status, nope, "off behaves like any unrouted path");
    // The dashboard build answers unknown paths from a GET-only SPA fallback.
    let expected = if cfg!(feature = "web") {
        StatusCode::METHOD_NOT_ALLOWED
    } else {
        StatusCode::NOT_FOUND
    };
    assert_eq!(status, expected);

    initialize(&app(true, vec![])).await;
}

/// With passphrase login on, a remote caller without a login session gets a JSON `401`, not a
/// redirect to the login page, whether or not it presents a valid token.
#[tokio::test]
async fn login_wall_answers_mcp_with_401_not_a_redirect() {
    for token in [None, Some(TOKEN)] {
        let state = test_support::build_test_app_state_with_policy_configured(
            Vec::new(),
            vecs(&["localhost"]),
            Vec::new(),
            token.map(str::to_string),
            |state| {
                state.web_config.mcp_enabled = true;
                state.login_manager = std::sync::Arc::new(crate::server::login::LoginManager::new(
                    Some("passphrase"),
                ));
            },
        );
        let app = test_support::build_router_for_test(state);

        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(token) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let mut req = req
            .body(axum::body::Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [203, 0, 113, 7],
                5555,
            ))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "token {token:?}");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "login_required", "token {token:?}");
    }
}

#[tokio::test]
async fn requests_without_a_valid_token_are_rejected() {
    let app = app(true, vec![]);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    for token in [None, Some("wrong-token")] {
        let (status, _) = rpc(&app, token, body.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "token {token:?}");
    }
}

#[tokio::test]
async fn read_only_reads_work_but_cityhall_denies_mcp() {
    for cityhall in [false, true] {
        let state = test_support::build_test_app_state_with_policy_configured(
            vec![],
            vecs(&["localhost"]),
            Vec::new(),
            Some(TOKEN.to_string()),
            |state| {
                state.web_config.mcp_enabled = true;
                state.read_only = true;
                state.cityhall_mode = cityhall;
            },
        );
        let app = test_support::build_router_for_test(state);
        let (status, message) = rpc(
            &app,
            Some(TOKEN),
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "list_sessions", "arguments": {}}
            }),
        )
        .await;
        if cityhall {
            assert_eq!(status, StatusCode::FORBIDDEN);
        } else {
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                message.unwrap()["result"]["structuredContent"]["sessions"],
                json!([])
            );
        }
    }
}

#[tokio::test]
async fn tools_list_advertises_each_tool_with_an_output_schema() {
    let app = app(true, vec![]);
    // No `initialize` first: the server keeps no session state between requests.
    let (status, message) = rpc(
        &app,
        Some(TOKEN),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tools = message.unwrap()["result"]["tools"].clone();
    let mut names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "create_session",
            "list_sessions",
            "read_output",
            "send",
            "whoami"
        ]
    );
    for tool in tools.as_array().unwrap() {
        assert!(tool["outputSchema"].is_object(), "{tool}");
    }
    let list = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "list_sessions")
        .unwrap();
    let mut params: Vec<&str> = list["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    params.sort_unstable();
    assert_eq!(params, ["children_of", "group", "state"]);
    // A model can't approve hooks, add callbacks or override the launch command.
    let create = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "create_session")
        .unwrap();
    let properties = create["inputSchema"]["properties"].as_object().unwrap();
    for field in [
        "trust_hooks",
        "trust_review",
        "callback_url",
        "command_override",
        "extra_args",
        "extra_env",
        "hook_env",
        "fork_from",
        "import_acp_session_id",
    ] {
        assert!(!properties.contains_key(field), "{field}");
    }
    assert!(properties.contains_key("prompt") && properties.contains_key("parent_session_id"));
}

#[tokio::test]
async fn whoami_is_null() {
    let app = app(true, vec![]);
    assert_eq!(call(&app, "whoami", json!({})).await, Value::Null);
}

#[tokio::test]
#[serial_test::serial]
async fn list_sessions_honours_state_group_and_children_of() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let ordering_path = crate::session::get_app_dir()
        .unwrap()
        .join("workspace-ordering.json");
    let ordering_bytes = br#"{"order":["manual-last"]}"#;
    std::fs::write(&ordering_path, ordering_bytes).unwrap();
    let parent = session_in("parent", "work", None);
    let child = session_in("child", "work/sub", Some(&parent.id));
    let other = session_in("other", "elsewhere", None);
    let lookalike = session_in("lookalike", "workshop", None);
    let mut trashed = session_in("trashed", "work", None);
    trashed.trash();
    let (parent_id, child_id) = (parent.id.clone(), child.id.clone());
    let (app, state) = app_with_state(true, vec![parent, child, other, lookalike, trashed]);
    let canonical = state.runtime.snapshot(&state).await.unwrap();
    let unpublished = session_in("unpublished", "work/sub", Some(&parent_id));
    state.instances.write().await.push(unpublished);

    let titles = |envelope: Value| -> Vec<String> {
        let mut titles: Vec<String> = envelope["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["title"].as_str().unwrap().to_string())
            .collect();
        titles.sort();
        titles
    };

    // (arguments, expected titles)
    let cases = [
        (json!({}), vec!["child", "lookalike", "other", "parent"]),
        (
            json!({"state": "all"}),
            vec!["child", "lookalike", "other", "parent", "trashed"],
        ),
        (json!({"group": "work"}), vec!["child", "parent"]),
        (json!({"group": "work/sub"}), vec!["child"]),
        (
            json!({"group": "work", "state": "all"}),
            vec!["child", "parent", "trashed"],
        ),
        (json!({"children_of": parent_id}), vec!["child"]),
        (json!({"children_of": child_id}), vec![]),
    ];
    for (arguments, expected) in cases {
        let envelope = call(&app, "list_sessions", arguments.clone()).await;
        assert_eq!(
            envelope["workspace_ordering"],
            json!(canonical.value.contents.workspace_ordering),
            "filters retain the published workspace ordering",
        );
        assert_eq!(titles(envelope), expected, "{arguments}");
    }
    let request = Request::builder()
        .uri("/api/sessions?state=all")
        .header(header::HOST, "localhost")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = send(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let rest: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        call(&app, "list_sessions", json!({"state": "all"})).await,
        rest
    );
    state.runtime.publish(&state).await.unwrap();
    assert_eq!(
        titles(call(&app, "list_sessions", json!({"children_of": parent_id})).await),
        ["child", "unpublished"],
        "a new published revision makes the child visible",
    );
    assert_eq!(std::fs::read(ordering_path).unwrap(), ordering_bytes);
}

/// The code of a tool error, or `None` for a result that isn't one.
fn error_code(result: &Value) -> Option<&str> {
    (result["isError"] == json!(true))
        .then(|| result["structuredContent"]["code"].as_str().unwrap())
}

fn structured_session(id: &str) -> Instance {
    let mut inst = Instance::new(id, &format!("/tmp/aoe-{id}"));
    inst.id = id.to_string();
    inst.source_profile = "default".into();
    inst.view = crate::session::View::Structured;
    inst.status = crate::session::Status::Idle;
    inst
}

/// An MCP router over `prior`, with those rows on disk as the canonical store expects.
async fn seeded_app(
    prior: Vec<Instance>,
    read_only: bool,
) -> (axum::Router, std::sync::Arc<crate::server::AppState>) {
    crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
    test_support::seed_instances_on_disk_for_test("default", prior.clone());
    let state = test_support::build_test_app_state_with_policy_configured(
        prior,
        vecs(&["localhost"]),
        Vec::new(),
        Some(TOKEN.to_string()),
        |state| {
            state.web_config.mcp_enabled = true;
            state.read_only = read_only;
        },
    );
    test_support::refresh_canonical_metadata_for_test(&state).await;
    (test_support::build_router_for_test(state.clone()), state)
}

fn user_prompt(text: &str) -> crate::acp::Event {
    crate::acp::Event::UserPromptSent {
        text: text.into(),
        attachments: Vec::new(),
        prompt_id: None,
        synthesized: false,
        origin_plugin: None,
    }
}

fn seed_events(state: &crate::server::AppState, id: &str, events: &[crate::acp::Event]) {
    for (seq, event) in events.iter().enumerate() {
        state
            .acp_event_store
            .record(id, seq as u64 + 1, event)
            .expect("seed");
    }
    state
        .acp_supervisor
        .hydrate_seqs([(id.to_string(), state.acp_event_store.highest_seq(id))]);
}

/// `send` reports what the dispatch decided and, for a turn, the seq `read_output` resumes
/// from; a queued prompt has a row id and no seq.
#[tokio::test]
#[serial_test::serial]
async fn send_reports_the_dispatch_and_the_prompts_seq() {
    let caps = |steering| crate::acp::Event::PromptCapabilities {
        image: false,
        audio: false,
        embedded_context: false,
        load_session: None,
        steering,
    };
    // (case, events already on disk, disposition, queue reason)
    let cases = [
        ("idle", vec![], "sent", None),
        (
            "mid-turn, steerable",
            vec![caps(true), user_prompt("an earlier turn")],
            "steered",
            None,
        ),
        (
            "mid-turn, not steerable",
            vec![caps(false), user_prompt("an earlier turn")],
            "queued",
            Some("turn_active"),
        ),
    ];
    for (case, events, disposition, reason) in cases {
        let _guard = crate::session::test_support::isolate_app_dir();
        let id = "mcp-send";
        let (app, state) = seeded_app(vec![structured_session(id)], false).await;
        let _cmds = state
            .acp_supervisor
            .test_insert_worker_cmd_recording(id)
            .await;
        seed_events(&state, id, &events);

        let sent = call(
            &app,
            "send",
            json!({"session_id": id, "message": "the new prompt"}),
        )
        .await;
        assert_eq!(sent["disposition"], disposition, "{case}: {sent}");
        assert_eq!(sent["reason"].as_str(), reason, "{case}: {sent}");
        let recorded = state
            .acp_event_store
            .replay_from(id, 0)
            .into_iter()
            .find_map(|(seq, e)| {
                matches!(&e, crate::acp::Event::UserPromptSent { text, .. } if text == "the new prompt")
                    .then_some(seq)
            });
        assert_eq!(sent["seq"].as_u64(), recorded, "{case}: {sent}");
        assert_eq!(
            sent["queued_id"].is_string(),
            disposition == "queued",
            "{case}: {sent}"
        );
    }
}

/// A structured transcript reads as text from a cursor, pages on from `next_since`, and comes
/// back raw on request.
#[tokio::test]
#[serial_test::serial]
async fn read_output_pages_a_structured_transcript() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let id = "mcp-read";
    let (app, state) = seeded_app(vec![structured_session(id)], false).await;
    seed_events(
        &state,
        id,
        &[
            user_prompt("what is two plus two"),
            crate::acp::Event::AgentMessageChunk {
                text: "four".into(),
                message_id: None,
            },
        ],
    );

    let first = call(&app, "read_output", json!({"session_id": id})).await;
    let text = first["text"].as_str().unwrap();
    assert!(
        text.contains("[User]") && text.contains("what is two plus two"),
        "{text}"
    );
    assert!(
        text.contains("[Assistant]") && text.contains("four"),
        "{text}"
    );
    assert_eq!(first["next_since"], 2);
    assert_eq!(first["has_more"], false);

    let after = call(&app, "read_output", json!({"session_id": id, "since": 2})).await;
    assert_eq!(after["text"], "", "{after}");
    assert_eq!(after["next_since"], 2);

    let raw = call(&app, "read_output", json!({"session_id": id, "raw": true})).await;
    let seqs: Vec<u64> = raw["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, [1, 2]);
    assert!(raw.get("text").is_none(), "{raw}");
}

/// `/mcp` has no read-only layer, so the mutating tools refuse themselves; reading stays open.
#[tokio::test]
#[serial_test::serial]
async fn mutating_tools_refuse_while_read_only() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let id = "mcp-read-only";
    let (app, state) = seeded_app(vec![structured_session(id)], true).await;
    let refused = [
        ("send", json!({"session_id": id, "message": "hi"})),
        (
            "create_session",
            json!({"path": "/tmp", "tool": "claude", "view": "structured"}),
        ),
    ];
    for (tool, arguments) in refused {
        let result = call_result(&app, tool, arguments).await;
        assert_eq!(error_code(&result), Some("read_only"), "{tool}: {result}");
        assert_eq!(
            result["structuredContent"]["message"],
            crate::server::api::READ_ONLY_MESSAGE
        );
    }
    assert_eq!(state.instances.read().await.len(), 1);
    assert!(state.acp_event_store.replay_from(id, 0).is_empty());
    let read = call(&app, "read_output", json!({"session_id": id})).await;
    assert_eq!(read["view"], "structured");
}

/// A structured create stores its prompt as the pending initial turn and its parent link; a
/// refusal comes back with the HTTP error code it carried.
#[tokio::test]
#[serial_test::serial]
async fn create_session_stores_a_structured_prompt_and_its_parent() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let _adapter = crate::session::test_support::stub_claude_adapter_on_agent_path();
    crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
    crate::tmux::tool_retirement::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
    crate::session::config::update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })
    .expect("acknowledge the agent hook paths");
    crate::session::Storage::new_unwatched("test").unwrap();
    let mut parent = Instance::new("parent", "/tmp/aoe-mcp-parent");
    parent.id = "parent".into();
    parent.source_profile = "test".into();
    parent.group_path = "team".into();
    let mut child = Instance::new("child", "/tmp/aoe-mcp-child");
    child.id = "child".into();
    child.source_profile = "test".into();
    child.parent_session_id = Some("parent".into());
    test_support::seed_instances_on_disk_for_test("test", vec![parent.clone(), child.clone()]);
    let (launcher, _launches) = test_support::counting_failing_launcher();
    let state = test_support::build_test_app_state_impl(
        vec![parent, child],
        vecs(&["localhost"]),
        Vec::new(),
        Some(TOKEN.to_string()),
        false,
        |state| state.web_config.mcp_enabled = true,
        |supervisor| supervisor.with_launcher(launcher),
    );
    test_support::refresh_canonical_metadata_for_test(&state).await;
    let app = test_support::build_router_for_test(state.clone());
    let project = tempfile::tempdir().unwrap();
    // A repository whose hooks nobody has trusted yet.
    let hooked = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(hooked.path().join(".agent-of-empires")).unwrap();
    std::fs::write(
        hooked.path().join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"make setup\"]\n",
    )
    .unwrap();
    let create = |path: &std::path::Path, parent_id: &str| {
        json!({
            "path": path, "tool": "claude", "view": "structured",
            "title": "worker", "prompt": "review the diff", "parent_session_id": parent_id,
        })
    };

    // (path, parent, code); each refusal writes no row.
    let refusals = [
        (project.path(), "missing", "parent_not_found"),
        (project.path(), "child", "parent_is_sub_session"),
        // `trust_hooks` isn't in the input, so a model can never approve them itself.
        (hooked.path(), "parent", "hooks_need_trust"),
    ];
    for (path, parent_id, code) in refusals {
        let refused = call_result(&app, "create_session", create(path, parent_id)).await;
        assert_eq!(error_code(&refused), Some(code), "{refused}");
        assert_eq!(state.instances.read().await.len(), 2, "{code}");
    }

    let created = call(&app, "create_session", create(project.path(), "parent")).await;
    assert_eq!(created["prompt_sent"], true, "{created}");
    assert_eq!(created["prompt_delivery"], "pending_initial_turn");
    let id = created["session"]["id"].as_str().unwrap();
    let row = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .unwrap();
    assert_eq!(row.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(row.group_path, "team");
    assert_eq!(
        row.pending_initial_turn.map(|turn| turn.text).as_deref(),
        Some("review the diff")
    );
}

/// A terminal session's `send` and `read_output` refuse what HTTP refuses, and read a live pane.
#[tokio::test]
#[serial_test::serial]
async fn terminal_sessions_send_and_read_through_their_pane() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut live = Instance::new("mcp-pane", "/tmp/aoe-mcp-pane");
    live.source_profile = "default".into();
    let mut stopped = Instance::new("mcp-stopped", "/tmp/aoe-mcp-stopped");
    stopped.source_profile = "default".into();
    let (live_id, stopped_id) = (live.id.clone(), stopped.id.clone());
    let pane = live.tmux_session().unwrap();
    let (app, _state) = seeded_app(vec![live, stopped], false).await;

    // (tool, arguments, error code)
    let refusals = [
        (
            "send",
            json!({"session_id": live_id, "message": "\n"}),
            "invalid_request",
        ),
        (
            "send",
            json!({"session_id": "nope", "message": "hi"}),
            "not_found",
        ),
        ("read_output", json!({"session_id": "nope"}), "not_found"),
        (
            "read_output",
            json!({"session_id": stopped_id}),
            "not_running",
        ),
    ];
    for (tool, arguments, code) in refusals {
        let result = call_result(&app, tool, arguments.clone()).await;
        assert_eq!(
            error_code(&result),
            Some(code),
            "{tool} {arguments}: {result}"
        );
    }

    if !crate::tmux::test_helpers::tmux_available() {
        return;
    }
    let mut args: Vec<String> = [
        "new-session",
        "-d",
        "-s",
        pane.name(),
        "printf 'pane-says-hello\\n'; exec cat",
    ]
    .map(str::to_string)
    .to_vec();
    crate::tmux::utils::append_pane_base_index_args(&mut args, pane.name());
    let started = crate::tmux::tmux_command().args(&args).status().unwrap();
    assert!(started.success());
    let _cleanup = crate::tmux::test_helpers::TmuxTestSession::from_name(pane.name());
    let pane_shows = |needle: &'static str| {
        let (app, live_id) = (app.clone(), live_id.clone());
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let arguments = json!({"session_id": live_id, "lines": 5});
                    let result = call_result(&app, "read_output", arguments).await;
                    let text = result["structuredContent"]["text"].as_str().unwrap_or("");
                    if text.contains(needle) {
                        return result;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("the pane never showed {needle:?}"))
        }
    };
    let read = pane_shows("pane-says-hello").await;
    assert_eq!(read["structuredContent"]["view"], "terminal");
    assert!(read["structuredContent"].get("next_since").is_none());

    // A shell pane reads as not running, so send once `cat` owns it; the pane echoes the keys.
    crate::tmux::test_helpers::wait_for_pane_command(
        &crate::tmux::test_helpers::only_pane_id(pane.name()),
        "cat",
    );
    let arguments = json!({"session_id": live_id, "message": "typed-by-mcp\n"});
    let sent = call(&app, "send", arguments).await;
    assert_eq!(sent["disposition"], "sent", "{sent}");
    assert!(sent.get("seq").is_none(), "{sent}");
    pane_shows("typed-by-mcp").await;
}

/// A retried `idempotency_key` answers with the first create's session and delivers nothing:
/// a terminal pane is not typed into again, and a structured session stores no prompt.
#[tokio::test]
#[serial_test::serial]
async fn a_keyed_retry_returns_the_first_session_and_delivers_nothing() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut terminal = Instance::new("keyed-terminal", "/tmp/aoe-mcp-keyed-terminal");
    terminal.source_profile = "default".into();
    terminal.idempotency_key = Some("k-terminal".into());
    let mut structured = structured_session("keyed-structured");
    structured.idempotency_key = Some("k-structured".into());
    let (terminal_id, structured_id) = (terminal.id.clone(), structured.id.clone());
    let (app, state) = seeded_app(vec![terminal, structured], false).await;

    // (view, key, the session the first create made)
    let retries = [
        ("terminal", "k-terminal", terminal_id.as_str()),
        ("structured", "k-structured", structured_id.as_str()),
    ];
    for (view, key, id) in retries {
        let created = call(
            &app,
            "create_session",
            json!({
                "path": "/tmp/aoe-mcp-keyed", "tool": "claude", "view": view,
                "prompt": "do it again", "idempotency_key": key,
            }),
        )
        .await;
        assert_eq!(created["session"]["id"], id, "{view}: {created}");
        assert_eq!(created["fresh"], false, "{view}");
        assert_eq!(created["prompt_delivery"], "replayed", "{view}");
        assert_eq!(created["prompt_sent"], false, "{view}");
        assert_eq!(state.instances.read().await.len(), 2, "{view}");
    }
    let rows = state.instances.read().await;
    let stored = rows.iter().find(|row| row.id == structured_id).unwrap();
    assert!(stored.pending_initial_turn.is_none());
}

/// Text pages stop at the last event that fits whole, so following `next_since` reads every
/// event once; one event that alone is too big comes back whole on its own page.
#[tokio::test]
#[serial_test::serial]
async fn read_output_pages_text_by_rendered_size_without_skipping() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let (long, huge) = ("mcp-long", "mcp-huge");
    let (app, state) = seeded_app(
        vec![structured_session(long), structured_session(huge)],
        false,
    )
    .await;
    let filler = "x".repeat(1_500);
    let prompts: Vec<crate::acp::Event> = (0..60)
        .map(|i| user_prompt(&format!("marker-{i:03} {filler}")))
        .collect();
    seed_events(&state, long, &prompts);

    let (mut since, mut pages, mut text) = (0u64, 0, String::new());
    loop {
        let page = call(
            &app,
            "read_output",
            json!({"session_id": long, "since": since}),
        )
        .await;
        let chunk = page["text"].as_str().unwrap();
        assert!(chunk.len() <= 48_000, "page {pages}: {} bytes", chunk.len());
        assert!(!chunk.contains("truncated"), "page {pages} dropped text");
        assert!(page.get("truncated").is_none(), "{page}");
        text.push_str(chunk);
        pages += 1;
        since = page["next_since"].as_u64().unwrap();
        if page["has_more"] == false {
            break;
        }
        assert!(pages < 10, "the cursor never reached the end");
    }
    assert!(pages > 1, "a ~90 KB transcript fits no single page");
    let found: Vec<usize> = (0..60)
        .filter_map(|i| text.find(&format!("marker-{i:03}")))
        .collect();
    assert_eq!(found.len(), 60, "an event was skipped");
    assert!(found.windows(2).all(|w| w[0] < w[1]), "out of order");
    assert_eq!(since, 60);

    seed_events(
        &state,
        huge,
        &[user_prompt(&"y".repeat(60_000)), user_prompt("after")],
    );
    let first = call(&app, "read_output", json!({"session_id": huge})).await;
    let whole = first["text"].as_str().unwrap();
    assert!(whole.contains(&"y".repeat(60_000)), "{} bytes", whole.len());
    assert!(
        !whole.contains("after"),
        "the next event shares the oversized page"
    );
    assert_eq!(first["next_since"], 1);
    assert_eq!(first["has_more"], true);
    let next = call(&app, "read_output", json!({"session_id": huge, "since": 1})).await;
    assert!(next["text"].as_str().unwrap().contains("after"), "{next}");
}

/// A message that strips to nothing is refused in either view, and nothing is recorded or queued.
#[tokio::test]
#[serial_test::serial]
async fn send_refuses_an_empty_message() {
    for (view, message) in [("structured", "  "), ("terminal", "\n")] {
        let _guard = crate::session::test_support::isolate_app_dir();
        let id = "mcp-send-empty";
        let session = if view == "structured" {
            structured_session(id)
        } else {
            let mut inst = Instance::new(id, &format!("/tmp/aoe-{id}"));
            inst.id = id.to_string();
            inst.source_profile = "default".into();
            inst
        };
        let (app, state) = seeded_app(vec![session], false).await;
        let _cmds = state
            .acp_supervisor
            .test_insert_worker_cmd_recording(id)
            .await;
        let refused =
            call_result(&app, "send", json!({"session_id": id, "message": message})).await;
        assert_eq!(
            error_code(&refused),
            Some("invalid_request"),
            "{view}: {refused}"
        );
        assert!(
            state.acp_event_store.replay_from(id, 0).is_empty(),
            "{view}"
        );
        assert!(
            state
                .session_service
                .queued_prompts_snapshot(id)
                .await
                .is_empty(),
            "{view}"
        );
    }
}

/// A prompt that strips to nothing is refused before a session is created, as `send` refuses it.
#[tokio::test]
#[serial_test::serial]
async fn create_session_refuses_an_empty_prompt() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let (app, state) = seeded_app(Vec::new(), false).await;
    for (view, prompt) in [
        ("terminal", "\n"),
        ("terminal", "\u{1b}[201~"),
        ("structured", "  "),
    ] {
        let refused = call_result(
            &app,
            "create_session",
            json!({"path": "/tmp/aoe-mcp-empty", "tool": "claude", "view": view, "prompt": prompt}),
        )
        .await;
        assert_eq!(
            error_code(&refused),
            Some("invalid_request"),
            "{view} {prompt:?}: {refused}"
        );
    }
    assert!(state.instances.read().await.is_empty());
}
