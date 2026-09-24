use super::*;

/// `remove_instance` is the only way a row leaves `state.instances` on the
/// delete path, so the epoch bump must be tied to an actual removal: bumping
/// unconditionally would spend an epoch on the final commit block after the
/// early removal already took the row, and not bumping leaves a window a stale
/// reload uses to put a deleted row back.
#[test]
fn remove_instance_bumps_the_epoch_only_when_it_removes_a_row() {
    let epoch = std::sync::atomic::AtomicU64::new(0);
    let read = || epoch.load(std::sync::atomic::Ordering::SeqCst);
    let mut instances = vec![
        Instance::new("keep", "/tmp/keep"),
        Instance::new("doomed", "/tmp/doomed"),
    ];
    let doomed_id = instances[1].id.clone();

    remove_instance(&mut instances, &doomed_id, &epoch);
    assert_eq!(read(), 1, "a real removal bumps");
    assert_eq!(
        instances
            .iter()
            .map(|i| i.title.as_str())
            .collect::<Vec<_>>(),
        vec!["keep"]
    );

    // The final commit block runs after the early removal already took the
    // row, so no epoch is spent.
    remove_instance(&mut instances, &doomed_id, &epoch);
    assert_eq!(read(), 1, "a no-op removal does not bump");

    remove_instance(&mut instances, "never-existed", &epoch);
    assert_eq!(read(), 1, "an unknown id does not bump");
}
fn build_rename_test_state(
    persisted: Vec<Instance>,
    cached: Vec<Instance>,
) -> (Storage, std::sync::Arc<crate::server::AppState>) {
    let storage = Storage::new_unwatched("default").unwrap();
    storage
        .update(|instances, _groups| {
            *instances = persisted;
            Ok(())
        })
        .unwrap();
    let state = crate::server::test_support::build_test_app_state(cached);
    (storage, state)
}

#[tokio::test]
#[serial_test::serial]
async fn rename_session_rejects_duplicate_and_preserves_newer_cache() {
    use axum::body::to_bytes;

    let _guard = crate::session::test_support::isolate_app_dir();
    let mut existing = Instance::new("main branch", "/tmp/repo/");
    existing.source_profile = "default".to_string();
    let mut target = Instance::new("throwaway", "/tmp/repo");
    target.source_profile = "default".to_string();
    let target_id = target.id.clone();
    let mut stale_existing = existing.clone();
    stale_existing.title = "previous title".to_string();
    let mut stale_target = target.clone();
    stale_target.project_path = "/tmp/stale".to_string();
    let (storage, state) =
        build_rename_test_state(vec![existing, target], vec![stale_existing, stale_target]);

    let response = rename_session(
        State(state.clone()),
        Path(target_id.clone()),
        Ok(Json(RenameSessionBody {
            title: "main branch".to_string(),
            rename_branch: false,
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = to_bytes(response.into_body(), 2048).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("duplicate_session"));
    assert_eq!(
        state
            .instances
            .read()
            .await
            .iter()
            .find(|instance| instance.id == target_id)
            .unwrap()
            .title,
        "throwaway"
    );

    storage
        .update(|instances, _groups| {
            instances
                .iter_mut()
                .find(|instance| instance.id != target_id)
                .unwrap()
                .title = "other".to_string();
            Ok(())
        })
        .unwrap();
    // A user action can advance the live cache while the disk snapshot still
    // has the older row, so publication must patch only rename-owned identity
    // fields.
    state
        .instances
        .write()
        .await
        .iter_mut()
        .find(|instance| instance.id == target_id)
        .unwrap()
        .favorite();
    let response = rename_session(
        State(state.clone()),
        Path(target_id.clone()),
        Ok(Json(RenameSessionBody {
            title: "main branch".to_string(),
            rename_branch: false,
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let instances = state.instances.read().await;
    let target = instances
        .iter()
        .find(|instance| instance.id == target_id)
        .unwrap();
    assert_eq!(target.title, "main branch");
    assert_eq!(target.project_path, "/tmp/repo");
    assert_eq!(target.source_profile, "default");
    assert!(
        target.is_favorited(),
        "newer cached user action must survive rename publication"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn rename_session_rejects_tied_drifted_path_collision() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let _tie_guard = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let mut existing = Instance::new("main branch", "/tmp/worktrees/main-branch");
    existing.source_profile = "default".to_string();
    let mut drifted = Instance::new("main branch", "/tmp/worktrees/drifted");
    drifted.source_profile = "default".to_string();
    drifted.worktree_info = Some(worktree("main-branch", "/tmp/repo".to_string(), None));
    let drifted_id = drifted.id.clone();
    let (_storage, state) = build_rename_test_state(
        vec![existing.clone(), drifted.clone()],
        vec![existing, drifted],
    );

    let response = rename_session(
        State(state),
        Path(drifted_id),
        Ok(Json(RenameSessionBody {
            title: "main branch".to_string(),
            rename_branch: false,
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
#[serial_test::serial]
async fn concurrent_renames_commit_only_one_same_identity_pair() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut first = Instance::new("first", "/tmp/shared");
    first.source_profile = "default".to_string();
    let mut second = Instance::new("second", "/tmp/shared/");
    second.source_profile = "default".to_string();
    let first_id = first.id.clone();
    let second_id = second.id.clone();
    let storage = Storage::new_unwatched("default").unwrap();
    storage
        .update(|instances, _groups| {
            *instances = vec![first.clone(), second.clone()];
            Ok(())
        })
        .unwrap();
    let state = crate::server::test_support::build_test_app_state(vec![first, second]);

    let first_rename = rename_session(
        State(state.clone()),
        Path(first_id),
        Ok(Json(RenameSessionBody {
            title: "shared title".to_string(),
            rename_branch: false,
        })),
    );
    let second_rename = rename_session(
        State(state.clone()),
        Path(second_id),
        Ok(Json(RenameSessionBody {
            title: "shared title".to_string(),
            rename_branch: false,
        })),
    );
    let (first_response, second_response) = tokio::join!(first_rename, second_rename);
    let statuses = [
        first_response.into_response().status(),
        second_response.into_response().status(),
    ];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CONFLICT)
            .count(),
        1
    );
    assert_eq!(
        storage
            .load()
            .unwrap()
            .iter()
            .filter(|instance| {
                instance.title == "shared title"
                    && instance.project_path.trim_end_matches('/') == "/tmp/shared"
            })
            .count(),
        1
    );
}

// #2536: the workspace-delete order must tear down record-only siblings
// first and the shared-worktree owner last, so a sibling failure can never
// orphan a session against an already-removed worktree.
mod workspace_deletion {
    use super::*;

    fn body() -> DeleteWorkspaceBody {
        DeleteWorkspaceBody {
            session_ids: vec![],
            delete_worktree: true,
            delete_branch: true,
            delete_sandbox: true,
            force_delete: false,
            keep_scratch: false,
        }
    }

    #[test]
    fn owner_is_last_and_siblings_are_record_only() {
        let ids = vec!["owner".to_string(), "sib1".to_string(), "sib2".to_string()];
        let plan = order_workspace_deletion(&ids, &body());

        let order: Vec<&str> = plan.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            order,
            vec!["sib1", "sib2", "owner"],
            "siblings must precede the owner so the worktree owner is torn down last"
        );

        // Siblings never touch the shared worktree/branch.
        for (id, b) in &plan[..2] {
            assert!(
                !b.delete_worktree,
                "sibling {id} must not remove the worktree"
            );
            assert!(!b.delete_branch, "sibling {id} must not delete the branch");
            assert!(
                b.delete_sandbox,
                "sibling {id} still tears down its own sandbox"
            );
        }
        // The owner (last) carries the caller's worktree/branch flags.
        let (owner_id, owner_body) = plan.last().unwrap();
        assert_eq!(owner_id, "owner");
        assert!(owner_body.delete_worktree);
        assert!(owner_body.delete_branch);
    }

    #[test]
    fn single_session_is_owner_only_with_full_flags() {
        let ids = vec!["solo".to_string()];
        let plan = order_workspace_deletion(&ids, &body());
        assert_eq!(plan.len(), 1);
        let (id, b) = &plan[0];
        assert_eq!(id, "solo");
        assert!(
            b.delete_worktree,
            "the only session owns the worktree cleanup"
        );
        assert!(b.delete_branch);
    }

    #[test]
    fn empty_input_is_empty_plan() {
        assert!(order_workspace_deletion(&[], &body()).is_empty());
    }

    #[test]
    fn worktree_flags_off_stay_off_for_owner() {
        let mut b = body();
        b.delete_worktree = false;
        b.delete_branch = false;
        let ids = vec!["owner".to_string(), "sib".to_string()];
        let plan = order_workspace_deletion(&ids, &b);
        let (_, owner_body) = plan.last().unwrap();
        assert!(!owner_body.delete_worktree);
        assert!(!owner_body.delete_branch);
    }

    #[test]
    fn dedupe_drops_repeats_preserving_first_seen_order() {
        let ids = vec![
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
            "b".to_string(),
        ];
        assert_eq!(dedupe_session_ids(&ids), vec!["a", "b", "c"]);
    }

    #[test]
    fn duplicate_owner_still_removes_the_worktree() {
        // #2536 review: after dedupe the single owner entry keeps the real
        // worktree flags rather than the record-only sibling ones.
        let ids = dedupe_session_ids(&["owner".to_string(), "owner".to_string()]);
        assert_eq!(ids, vec!["owner"]);
        let plan = order_workspace_deletion(&ids, &body());
        assert_eq!(plan.len(), 1);
        let (id, b) = &plan[0];
        assert_eq!(id, "owner");
        assert!(
            b.delete_worktree,
            "the deduped owner must still own the worktree cleanup"
        );
        assert!(b.delete_branch);
    }
}

// CityHall create-time capability gate (#7): create_session rejects a
// non-ACP agent up front instead of downgrading to a hidden terminal view.
mod cityhall_capability {
    use super::*;
    use crate::session::test_support::isolate_app_dir;
    use serial_test::serial;

    #[test]
    fn builtin_agent_is_acp_capable() {
        // Built-in ACP agents resolve via the registry without reading config.
        assert!(agent_is_acp_capable(
            "default",
            std::path::Path::new("/nonexistent"),
            "claude",
            None,
        ));
    }

    #[test]
    #[serial]
    fn an_explicit_agent_name_keys_the_custom_acp_cmd_lookup() {
        // An explicit `agent_name` can point at a different `agent_acp_cmd`
        // entry than `tool`, so keying the lookup off `tool` reported
        // not-capable for an agent that spawns fine.
        let _tmp = isolate_app_dir();
        crate::session::config::update_config(|c| {
            c.session
                .agent_acp_cmd
                .insert("acp-helper".into(), "acp-helper --acp".into());
        })
        .unwrap();
        let path = std::path::Path::new("/nonexistent");
        assert!(agent_is_acp_capable(
            "default",
            path,
            "plain-tool",
            Some("acp-helper"),
        ));
        // Without the override there is nothing to resolve to, so the same
        // tool stays not-capable.
        assert!(!agent_is_acp_capable("default", path, "plain-tool", None));
    }

    #[test]
    #[serial]
    fn unknown_tool_is_not_acp_capable() {
        let _tmp = isolate_app_dir();
        assert!(!agent_is_acp_capable(
            "default",
            std::path::Path::new("/nonexistent"),
            "definitely-not-a-real-tool",
            None,
        ));
    }

    /// Why `acp_enable` gates on this predicate and not `pick_agent_for_tool`:
    /// the default-agent fallback always names a registry entry, so a
    /// post-fallback lookup reports every tool capable and would switch a
    /// terminal-only session into a structured one running some other agent.
    #[test]
    #[serial]
    fn the_default_agent_fallback_is_not_a_capability_signal() {
        let _tmp = isolate_app_dir();
        let fallback = crate::session::config::DEFAULT_ACP_AGENT;
        assert!(
            crate::acp::AgentRegistry::with_defaults()
                .get(fallback)
                .is_some(),
            "the fallback must be spawnable, which is what makes it useless as a gate"
        );
        assert!(!agent_is_acp_capable(
            "default",
            std::path::Path::new("/nonexistent"),
            "plain-tool",
            None,
        ));
    }
}

// #2587: the artifact route serves only canonicalized files confined to
// the session's artifact dir, sets nosniff, and never serves HTML inline.
mod artifact_route {
    use super::*;
    use crate::session::test_support::isolate_app_dir;
    use axum::body::to_bytes;
    use axum::extract::Path as AxumPath;
    use axum::http::header;
    use serial_test::serial;

    /// #2587: a type that can execute script as a top-level document must
    /// download rather than render inline, because the frontend opens artifacts
    /// via a same-origin blob URL. Passive types stay inline with `nosniff`.
    #[tokio::test]
    #[serial]
    async fn serves_passive_types_inline_and_scriptable_ones_as_attachments() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = crate::session::artifacts::session_artifact_dir(&id).unwrap();

        // (file name, bytes, content type, expected Content-Disposition)
        let cases: [(&str, &[u8], &str, Option<&str>); 3] = [
            ("shot.png", b"\x89PNG\r\n", "image/png", None),
            (
                "d.svg",
                b"<svg xmlns='http://www.w3.org/2000/svg'></svg>",
                "application/octet-stream",
                Some("attachment"),
            ),
            (
                "status.html",
                b"<h1>hi</h1>",
                "application/octet-stream",
                Some("attachment"),
            ),
        ];

        for (name, bytes, content_type, disposition) in cases {
            std::fs::write(dir.join(name), bytes).unwrap();
            let resp = serve_session_artifact(AxumPath((id.clone(), name.to_string())))
                .await
                .into_response();
            assert_eq!(resp.status(), StatusCode::OK, "{name}");
            assert_eq!(
                resp.headers().get(header::CONTENT_TYPE).unwrap(),
                content_type,
                "{name}"
            );
            assert_eq!(
                resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
                "nosniff",
                "{name}"
            );
            assert_eq!(
                resp.headers()
                    .get(header::CONTENT_DISPOSITION)
                    .map(|v| v.to_str().unwrap()),
                disposition,
                "{name}"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn rejects_traversal_with_empty_body() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        crate::session::artifacts::session_artifact_dir(&id).unwrap();
        let resp = serve_session_artifact(AxumPath((id, "../../../../etc/hosts".to_string())))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(body.is_empty(), "unexpected body: {body:?}");
    }
}

fn make_test_instance() -> Instance {
    let mut inst = Instance::new("test-session", "/tmp/test-project");
    inst.tool = "claude".to_string();
    inst.status = Status::Running;
    inst.group_path = "work/projects".to_string();
    inst
}

fn worktree(
    branch: &str,
    main_repo_path: impl Into<String>,
    base_branch: Option<&str>,
) -> crate::session::WorktreeInfo {
    crate::session::WorktreeInfo {
        branch: branch.to_string(),
        main_repo_path: main_repo_path.into(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: base_branch.map(str::to_string),
    }
}

// Regression witness for #2603: the ACP-capability and smart-rename overlays
// share ONE per-request `SessionConfig` cache keyed by (profile, project_path),
// so three instances covering two unique pairs must trigger exactly two
// resolver calls. A non-built-in tool is used so the ACP overlay does not
// short-circuit on the built-in registry and hide a regression.
// #3058 review: the preflight must resolve config with the repo-aware resolver
// so a repo-local agent_command_override is honored; the profile-only resolver
// would fall through to the "no prompt yet" path, which is also a 409, so this
// asserts the body message rather than the status.
#[tokio::test]
#[serial_test::serial]
async fn force_smart_rename_preflight_sees_command_override_but_not_from_a_repo() {
    use axum::body::to_bytes;

    async fn preflight_message(repo: &std::path::Path) -> String {
        let mut inst = Instance::new("Vikings", repo.to_str().unwrap());
        inst.tool = "claude".to_string();
        inst.source_profile = "default".to_string();
        inst.view = crate::session::View::Structured;
        let id = inst.id.clone();

        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        let resp = force_smart_rename(axum::extract::State(state), axum::extract::Path(id))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        String::from_utf8_lossy(&body).to_string()
    }

    let tmp_home = tempfile::tempdir().expect("tempdir HOME");
    let repo = tempfile::tempdir().expect("tempdir repo");
    let _home = crate::session::test_support::isolate_app_dir_at(tmp_home.path());

    // A repo declaring the override changes nothing: command-bearing
    // session fields are not repo-overridable (#3154).
    let cfg_dir = repo.path().join(".agent-of-empires");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[session.agent_command_override]\nclaude = \"wrapper-3058\"\n",
    )
    .unwrap();
    let msg = preflight_message(repo.path()).await;
    assert!(
        !msg.contains("command is overridden"),
        "a repo must not be able to declare the agent command override; got: {msg}"
    );

    // The user's own override is still seen through the repo-aware
    // resolution the preflight routes through (#3058).
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        app_dir.join("config.toml"),
        "[session.agent_command_override]\nclaude = \"wrapper-3058\"\n",
    )
    .unwrap();
    let msg = preflight_message(repo.path()).await;
    assert!(
        msg.contains("command is overridden"),
        "preflight must see the user's override via repo-aware resolution; got: {msg}"
    );
}

// The manual "Auto-name now" action regenerates a title even over one already
// chosen (a real request from a user who found the old "hide until eligible"
// behavior useless: the whole point of the always-shown action is to be able
// to re-roll a name on demand). The preflight must not reject a custom-named
// session with `NameNotDefault`; it falls through to the next gate instead
// (here, "no prompt yet", since this test seeds no ACP event-store data).
#[tokio::test]
#[serial_test::serial]
async fn force_smart_rename_ignores_a_custom_name() {
    use axum::body::to_bytes;

    let tmp_home = tempfile::tempdir().expect("tempdir HOME");
    let _home = crate::session::test_support::isolate_app_dir_at(tmp_home.path());

    let mut inst = Instance::new("Vikings", "/tmp/custom-name-regen");
    inst.title = "Fix login bug".to_string();
    inst.tool = "claude".to_string();
    inst.source_profile = "default".to_string();
    inst.view = crate::session::View::Structured;
    let id = inst.id.clone();

    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let resp = force_smart_rename(axum::extract::State(state), axum::extract::Path(id))
        .await
        .into_response();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let msg = String::from_utf8_lossy(&body);
    assert!(
        !msg.contains("custom name"),
        "manual regenerate must not refuse a custom-named session; got: {msg}"
    );
    assert!(
        msg.contains("No prompt to name this session from yet"),
        "must fall through to the next gate instead; got: {msg}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn list_sessions_shares_config_resolution_across_overlays() {
    use std::sync::atomic::Ordering;

    let tmp_home = tempfile::tempdir().expect("tempdir HOME");
    let _home = crate::session::test_support::isolate_app_dir_at(tmp_home.path());

    let mk = |profile: &str, project_path: &str| {
        let mut inst = Instance::new("test-session", project_path);
        inst.tool = "custom-tool-2603".to_string();
        inst.source_profile = profile.to_string();
        inst
    };
    let a = mk("default", "/tmp/repo-a-2603");
    let a2 = mk("default", "/tmp/repo-a-2603");
    let b = mk("default", "/tmp/repo-b-2603");

    // The counter lives on this state, so no concurrent test can bump it.
    let state = crate::server::test_support::build_test_app_state(vec![a, a2, b]);

    let _envelope = list_sessions(
        axum::extract::State(state.clone()),
        axum::extract::Query(ListSessionsQuery { state: None }),
    )
    .await;
    let misses = state.list_sessions_resolver_misses.load(Ordering::Relaxed);

    assert_eq!(
        misses, 2,
        "shared cache must resolve once per unique (profile, project_path) across both overlays; got {misses}",
    );
}

#[tokio::test]
#[serial_test::serial]
async fn list_sessions_state_filter() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut live = Instance::new("live", "/tmp/scope-live");
    live.id = "scope-live".to_string();
    let mut trashed = Instance::new("trashed", "/tmp/scope-trashed");
    trashed.id = "scope-trashed".to_string();
    trashed.trash();
    let mut archived = Instance::new("archived", "/tmp/scope-archived");
    archived.id = "scope-archived".to_string();
    archived.archived_at = Some(chrono::Utc::now());

    let state = crate::server::test_support::build_test_app_state(vec![
        live.clone(),
        trashed.clone(),
        archived.clone(),
    ]);

    async fn ids(response: Json<SessionsEnvelope>) -> Vec<String> {
        let response = response.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let envelope: crate::daemon::SessionsEnvelope = serde_json::from_slice(&body).unwrap();
        envelope.sessions.into_iter().map(|s| s.id).collect()
    }

    let all = list_sessions(
        axum::extract::State(state.clone()),
        axum::extract::Query(ListSessionsQuery { state: None }),
    )
    .await;
    assert_eq!(
        ids(all).await,
        ["scope-live", "scope-trashed", "scope-archived"]
    );

    let live_only = list_sessions(
        axum::extract::State(state.clone()),
        axum::extract::Query(ListSessionsQuery {
            state: Some(crate::session::SessionScope::Live),
        }),
    )
    .await;
    assert_eq!(ids(live_only).await, ["scope-live"]);

    let trashed_only = list_sessions(
        axum::extract::State(state.clone()),
        axum::extract::Query(ListSessionsQuery {
            state: Some(crate::session::SessionScope::Trashed),
        }),
    )
    .await;
    assert_eq!(ids(trashed_only).await, ["scope-trashed"]);

    let explicit_all = list_sessions(
        axum::extract::State(state),
        axum::extract::Query(ListSessionsQuery {
            state: Some(crate::session::SessionScope::All),
        }),
    )
    .await;
    assert_eq!(
        ids(explicit_all).await,
        ["scope-live", "scope-trashed", "scope-archived"]
    );
}

#[tokio::test]
async fn wait_until_left_starting_returns_immediately_if_already_left() {
    let mut inst = Instance::new("already-running", "/tmp/wait-a");
    inst.id = "wait-already-left".to_string();
    inst.status = Status::Running;
    let state = crate::server::test_support::build_test_app_state(vec![inst]);

    let result = wait_until_left_starting(
        &state,
        "wait-already-left",
        std::time::Duration::from_secs(5),
    );
    tokio::pin!(result);
    let result = futures_util::poll!(&mut result).map(|value| value.map(|i| i.status));
    assert_eq!(result, std::task::Poll::Ready(Some(Status::Running)));
}

#[tokio::test(start_paused = true)]
async fn wait_until_left_starting_resolves_on_broadcast() {
    let mut inst = Instance::new("starting", "/tmp/wait-b");
    inst.id = "wait-resolves".to_string();
    inst.status = Status::Starting;
    let state = crate::server::test_support::build_test_app_state(vec![inst]);

    let waiter =
        wait_until_left_starting(&state, "wait-resolves", std::time::Duration::from_secs(5));
    tokio::pin!(waiter);
    assert!(futures_util::poll!(waiter.as_mut()).is_pending());
    {
        let mut instances = state.instances.write().await;
        instances
            .iter_mut()
            .find(|i| i.id == "wait-resolves")
            .unwrap()
            .status = Status::Waiting;
    }
    state
        .status_tx
        .send(crate::server::push::StatusChange {
            instance_id: "wait-resolves".to_string(),
            instance_title: "starting".to_string(),
            old: Status::Starting,
            new: Status::Waiting,
            at: chrono::Utc::now(),
        })
        .expect("the waiter must be subscribed before the transition");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .expect("the broadcast must resolve before the fallback timeout");
    assert_eq!(result.map(|i| i.status), Some(Status::Waiting));
}

#[tokio::test(start_paused = true)]
async fn wait_until_left_starting_times_out_with_current_status() {
    let mut inst = Instance::new("stuck", "/tmp/wait-c");
    inst.id = "wait-timeout".to_string();
    inst.status = Status::Starting;
    let state = crate::server::test_support::build_test_app_state(vec![inst]);

    let timeout = std::time::Duration::from_millis(150);
    let waiter = wait_until_left_starting(&state, "wait-timeout", timeout);
    tokio::pin!(waiter);
    assert!(futures_util::poll!(waiter.as_mut()).is_pending());
    state.instances.write().await[0].status = Status::Waiting;
    assert!(futures_util::poll!(waiter.as_mut()).is_pending());
    tokio::time::advance(timeout).await;
    assert_eq!(waiter.await.map(|i| i.status), Some(Status::Waiting));
}

#[tokio::test]
async fn wait_until_left_starting_returns_none_if_instance_vanished() {
    let state = crate::server::test_support::build_test_app_state(vec![]);
    let result = wait_until_left_starting(
        &state,
        "never-existed",
        std::time::Duration::from_millis(100),
    )
    .await;
    assert!(result.is_none());
}

#[test]
fn find_by_idempotency_key_matches_trashed_but_not_missing() {
    let mut with_key = Instance::new("has-key", "/tmp/idem-a");
    with_key.id = "idem-has-key".to_string();
    with_key.idempotency_key = Some("retry-token-1".to_string());
    with_key.trash(); // soft-deleted; a retry must still find it.

    let mut without_key = Instance::new("no-key", "/tmp/idem-b");
    without_key.id = "idem-no-key".to_string();

    let instances = vec![with_key, without_key];

    let found = find_by_idempotency_key(&instances, "retry-token-1");
    assert_eq!(found.map(|i| i.id.as_str()), Some("idem-has-key"));

    assert!(find_by_idempotency_key(&instances, "never-seen").is_none());
}

#[test]
fn fork_from_builds_terminal_seed_for_claude() {
    // A non-structured fork resolves through `terminal_fork_seed`; a claude
    // parent id yields a Terminal seed with a fresh, valid child id.
    let seed = resolve_create_fork_seed("claude", "parent-uuid", false)
        .expect("claude terminal fork allowed");
    match seed {
        crate::session::ForkSeed::Terminal {
            parent_agent_session_id,
            child_session_id,
        } => {
            assert_eq!(parent_agent_session_id, "parent-uuid");
            assert!(crate::session::capture::is_valid_session_id(
                &child_session_id
            ));
        }
        _ => panic!("expected Terminal seed"),
    }
}

#[test]
fn fork_from_builds_structured_seed_when_view_is_structured() {
    // A structured fork carries the parent's acp_session_id onto a Structured
    // seed; the builder turns that into the one-shot fork_pending marker and the
    // live session/fork handshake mints the child id.
    let seed = resolve_create_fork_seed("claude", "parent-acp-id", true)
        .expect("structured fork seed is always allowed at create time");
    assert_eq!(
        seed,
        crate::session::ForkSeed::Structured {
            parent_acp_session_id: "parent-acp-id".into(),
        }
    );
}

fn create_body_from_json(value: serde_json::Value) -> CreateSessionBody {
    serde_json::from_value(value).expect("valid CreateSessionBody")
}

#[test]
fn worktree_enabled_true_opts_in_without_branch() {
    let body = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
        "worktree_enabled": true,
    }));

    assert!(create_body_uses_worktree(&body));
    assert!(body.worktree_branch.is_none());
}

#[test]
fn worktree_branch_preserves_legacy_worktree_opt_in() {
    let explicit = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
        "worktree_branch": "feat/api",
    }));
    assert!(create_body_uses_worktree(&explicit));

    let empty = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
        "worktree_branch": "",
    }));
    assert!(create_body_uses_worktree(&empty));
}

#[test]
fn worktree_defaults_off_without_flag_or_branch() {
    let body = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
    }));

    assert!(!create_body_uses_worktree(&body));
}

#[test]
fn worktree_enabled_conflicts_with_scratch() {
    let body = create_body_from_json(serde_json::json!({
        "path": "",
        "tool": "claude",
        "scratch": true,
        "worktree_enabled": true,
    }));

    assert!(create_body_combines_scratch_and_worktree(&body));
}

#[test]
fn both_import_and_fork_rejected() {
    // A request that sets both seeds the session from two contradictory
    // sources; the create handler rejects it before doing any work.
    let body = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
        "import_acp_session_id": "import-id",
        "fork_from": "parent-id",
    }));
    assert!(both_import_and_fork_set(&body));

    // Either alone is fine; trailing whitespace counts as unset.
    let import_only = create_body_from_json(serde_json::json!({
        "path": "/tmp/p", "tool": "claude", "import_acp_session_id": "import-id",
    }));
    assert!(!both_import_and_fork_set(&import_only));
    let fork_only = create_body_from_json(serde_json::json!({
        "path": "/tmp/p", "tool": "claude", "fork_from": "parent-id",
    }));
    assert!(!both_import_and_fork_set(&fork_only));
    let blank_fork = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
        "import_acp_session_id": "import-id",
        "fork_from": "   ",
    }));
    assert!(!both_import_and_fork_set(&blank_fork));
}

#[test]
fn invalid_fork_id_is_rejected_by_create_guard() {
    // `fork_from` is gated on `is_valid_session_id`, so a malformed id cannot
    // reach `build_fork_flags`, which fails closed and would silently start a
    // fresh session.
    use crate::session::capture::is_valid_session_id;
    assert!(!is_valid_session_id("../etc/passwd"));
    assert!(!is_valid_session_id("has spaces"));
    assert!(!is_valid_session_id("slash/id"));
    // A well-formed id still passes the same gate.
    assert!(is_valid_session_id("parent-uuid_123.v2"));
}

#[test]
fn structured_fork_create_guard_matches_acp_can_fork() {
    // The create-time guard and the web `acp_can_fork` projection share
    // `agent_is_structured_fork_capable`, so they must agree per agent.
    // claude is ACP-capable with a real fork strategy: forkable.
    assert!(agent_is_structured_fork_capable("claude", None));
    // aoe-agent is ACP-capable but resume-only (no fork strategy), so the
    // create guard must reject a structured fork for it just as the web
    // suppresses the Fork affordance; gating on ACP-capability alone would
    // accept a create that can only fail later at the `session/fork`
    // handshake.
    assert!(!agent_is_structured_fork_capable("aoe-agent", None));
    // codex and opencode are ACP-registered AND declare a real terminal
    // ForkStrategy (used by the CLI `--fork-from` path), but neither ACP
    // adapter is verified to implement `session/fork`. Gating on
    // "fork_strategy != Unsupported" alone would report them forkable and
    // reproduce the same dead-end-handshake failure this function exists
    // to prevent for aoe-agent.
    assert!(!agent_is_structured_fork_capable("codex", None));
    assert!(!agent_is_structured_fork_capable("opencode", None));
    // A non-ACP tool is neither ACP-capable nor fork-capable.
    assert!(!agent_is_structured_fork_capable(
        "definitely-not-an-acp-agent",
        None
    ));

    // The two surfaces must report the same capability for each agent.
    for tool in [
        "claude",
        "aoe-agent",
        "codex",
        "opencode",
        "definitely-not-an-acp-agent",
    ] {
        let mut inst = make_test_instance();
        inst.tool = tool.to_string();
        assert_eq!(
            SessionResponse::from_instance(&inst, false).acp_can_fork,
            agent_is_structured_fork_capable(tool, None),
            "acp_can_fork and the create guard disagree for '{tool}'"
        );
    }
}

#[test]
fn acp_can_fork_tracks_acp_capable_and_fork_strategy() {
    // claude is ACP-capable AND declares a real fork strategy, so the web
    // gets a forkable signal.
    let mut claude = make_test_instance();
    claude.tool = "claude".to_string();
    assert!(SessionResponse::from_instance(&claude, false).acp_can_fork);

    // aoe-agent is ACP-capable but declares no fork strategy, so it is NOT
    // forkable. Gating the web Fork action on acp_session_id alone would offer
    // a dead-end button for it.
    let mut aoe_agent = make_test_instance();
    aoe_agent.tool = "aoe-agent".to_string();
    assert!(!SessionResponse::from_instance(&aoe_agent, false).acp_can_fork);

    // codex has a terminal fork strategy but its ACP adapter is not verified
    // to implement `session/fork`, so the web signal stays false.
    let mut codex = make_test_instance();
    codex.tool = "codex".to_string();
    assert!(!SessionResponse::from_instance(&codex, false).acp_can_fork);

    // A non-ACP agent is neither ACP-capable nor fork-capable.
    let mut other = make_test_instance();
    other.tool = "definitely-not-an-acp-agent".to_string();
    assert!(!SessionResponse::from_instance(&other, false).acp_can_fork);
}

#[test]
fn trash_body_default_keeps_kill_pane_true() {
    // #2523: a no-body trash request resolves through `unwrap_or_default()`,
    // and the derived `Default` would leave the pane running, so the hand impl
    // must match the serde field default.
    assert!(TrashSessionBody::default().kill_pane);

    // An empty JSON object goes through serde, which honors the field
    // default helper.
    let from_empty: TrashSessionBody = serde_json::from_str("{}").unwrap();
    assert!(from_empty.kill_pane);

    // An explicit `false` is still respected.
    let explicit: TrashSessionBody = serde_json::from_str(r#"{"kill_pane": false}"#).unwrap();
    assert!(!explicit.kill_pane);
}

#[test]
fn upsert_instance_replaces_same_id_instead_of_duplicating() {
    // `create_session` persists to disk before pushing the in-memory copy, so
    // a `status_poll_loop` tick can insert the row first. The handler's insert
    // must replace that entry, not append a duplicate id.
    let poll_loaded = make_test_instance();
    let id = poll_loaded.id.clone();
    let mut instances = vec![poll_loaded];

    let mut handler_copy = make_test_instance();
    handler_copy.id = id.clone();
    handler_copy.status = Status::Starting;

    upsert_instance(&mut instances, handler_copy);

    assert_eq!(
        instances.len(),
        1,
        "same id must not duplicate in the registry"
    );
    assert_eq!(instances[0].id, id);
    assert_eq!(
        instances[0].status,
        Status::Starting,
        "handler copy must win"
    );
}

#[test]
fn upsert_instance_appends_a_new_id() {
    let mut instances = vec![make_test_instance()];
    let other = Instance::new("other-session", "/tmp/other-project");
    let other_id = other.id.clone();
    upsert_instance(&mut instances, other);
    assert_eq!(instances.len(), 2);
    assert!(instances.iter().any(|i| i.id == other_id));
}

// Regression for #2363: a multi-repo workspace session carries
// `workspace_info` and no `worktree_info`, so the DTO must report
// `has_cleanable_worktree: true` for the delete dialog's checkbox while keeping
// `has_managed_worktree: false` so worktree-only actions stay hidden.
#[test]
fn from_instance_reports_managed_worktree_for_workspace_session() {
    let mut inst = make_test_instance();
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/abc".to_string(),
        workspace_dir: "/tmp/ws".to_string(),
        repos: vec![crate::session::WorkspaceRepo {
            name: "repo-a".to_string(),
            source_path: "/tmp/src/repo-a".to_string(),
            branch: "feature/abc".to_string(),
            worktree_path: "/tmp/ws/repo-a".to_string(),
            main_repo_path: "/tmp/src/repo-a".to_string(),
            managed_by_aoe: true,
            branch_preexisting: false,
            base_branch: None,
            base_branch_override: None,
        }],
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    let resp = SessionResponse::from_instance(&inst, false);
    assert!(
        resp.has_cleanable_worktree,
        "workspace session must report a cleanable worktree so the delete checkbox shows"
    );
    assert!(
        !resp.has_managed_worktree,
        "workspace session must NOT report a single-repo managed worktree (keeps Edit-workdir hidden)"
    );
}

#[test]
#[serial_test::serial(hook_base)]
fn from_instance_surfaces_hook_urgent_flag() {
    // #1640: the web Attention sort needs `Instance::is_urgent()` on the wire.
    // Write the hook-side attention.json the agent would emit and confirm it
    // round-trips, then that a session with no hook file reports urgent: false.
    let (_g, _, _tmp_base) = crate::hooks::test_support::BaseGuard::ready();
    let inst = make_test_instance();
    let dir = crate::hooks::ensure_instance_dir_path(&inst.id)
        .expect("guard must create instance subdir");
    std::fs::write(
        dir.join("attention.json"),
        r#"{"urgent":true,"urgent_reason":"needs input"}"#,
    )
    .unwrap();

    let urgent_resp = SessionResponse::from_instance(&inst, false);
    assert!(urgent_resp.urgent, "hook-flagged session must be urgent");

    crate::hooks::cleanup_hook_status_dir(&inst.id);
    let plain_resp = SessionResponse::from_instance(&inst, false);
    assert!(
        !plain_resp.urgent,
        "session with no hook file must not be urgent"
    );
}

#[test]
fn public_create_session_error_forwards_whitelisted_git_errors() {
    let dup: anyhow::Error =
        GitError::WorktreeAlreadyExists(std::path::PathBuf::from("/tmp/repo-worktrees/foo")).into();
    assert_eq!(
        public_create_session_error(&dup),
        "Worktree already exists at /tmp/repo-worktrees/foo"
    );

    let in_use: anyhow::Error = GitError::BranchAlreadyCheckedOut("feature/foo".to_string()).into();
    assert_eq!(
        public_create_session_error(&in_use),
        "Branch 'feature/foo' is already in use by another worktree"
    );

    // Whitelisted variants survive an anyhow::Context wrapper too.
    let wrapped = anyhow::Error::from(GitError::BranchNotFound("nope".to_string()))
        .context("while creating worktree");
    assert_eq!(
        public_create_session_error(&wrapped),
        "Branch 'nope' not found"
    );
}

#[test]
fn public_create_session_error_hides_unsafe_messages() {
    // Raw git stderr (even already-sanitized) must not reach the client.
    let cmd: anyhow::Error = GitError::WorktreeCommandFailed(
        "fatal: unable to access 'https://<redacted>@host/repo.git'".to_string(),
    )
    .into();
    assert_eq!(
        public_create_session_error(&cmd),
        "Failed to create session"
    );

    let clone: anyhow::Error =
        GitError::CloneFailed("https://alice:supersecret@host/repo.git".to_string()).into();
    let msg = public_create_session_error(&clone);
    assert_eq!(msg, "Failed to create session");
    assert!(!msg.contains("supersecret"));

    // A non-GitError anyhow also stays generic.
    let other = anyhow::anyhow!("something internal at /home/user/.config/secret");
    assert_eq!(
        public_create_session_error(&other),
        "Failed to create session"
    );
}

#[test]
fn session_response_projects_core_instance_fields() {
    let inst = make_test_instance();
    let resp = SessionResponse::from_instance(&inst, false);

    assert_eq!(resp.id, inst.id);
    assert_eq!(resp.title, "test-session");
    assert_eq!(resp.project_path, "/tmp/test-project");
    assert_eq!(resp.tool, "claude");
    assert_eq!(resp.status, "Running");
    assert_eq!(resp.group_path, "work/projects");
    assert!(!resp.is_sandboxed);
    assert!(!resp.has_terminal);
}

#[test]
fn session_response_status_variants() {
    let mut inst = make_test_instance();
    for (status, expected) in [
        (Status::Running, "Running"),
        (Status::Waiting, "Waiting"),
        (Status::Error, "Error"),
        (Status::Stopped, "Stopped"),
        (Status::Idle, "Idle"),
        (Status::Starting, "Starting"),
    ] {
        inst.status = status;
        assert_eq!(
            SessionResponse::from_instance(&inst, false).status,
            expected
        );
    }
}

#[test]
fn session_response_dormant_reflects_shown_dormant() {
    let mut inst = make_test_instance();
    inst.status = Status::Idle;
    assert!(!SessionResponse::from_instance(&inst, false).dormant);

    inst.mark_idle_dormant();
    assert!(SessionResponse::from_instance(&inst, false).dormant);

    // A deliberate stop keeps the neutral Stopped dot rather than dormant (#2250).
    inst.status = Status::Stopped;
    assert!(!SessionResponse::from_instance(&inst, false).dormant);
}

#[test]
fn session_response_surfaces_worktree_branch_and_bases() {
    let mut inst = make_test_instance();
    let resp = SessionResponse::from_instance(&inst, false);
    assert!(resp.branch.is_none());
    assert!(resp.base_branch.is_none());

    inst.worktree_info = Some(worktree("feature/test", "/tmp/repo", None));
    let resp = SessionResponse::from_instance(&inst, false);
    assert_eq!(resp.branch.as_deref(), Some("feature/test"));
    assert!(resp.base_branch.is_none());

    inst.worktree_info = Some(worktree("feature/test", "/tmp/repo", Some("release-1.2")));
    assert_eq!(
        SessionResponse::from_instance(&inst, false)
            .base_branch
            .as_deref(),
        Some("release-1.2")
    );

    inst.base_branch_override = Some("upstream/main".to_string());
    assert_eq!(
        SessionResponse::from_instance(&inst, false)
            .base_branch_override
            .as_deref(),
        Some("upstream/main")
    );
}

#[test]
fn resolve_diff_base_prefers_override_then_worktree_then_config_then_auto() {
    let tmp = tempfile::tempdir().unwrap();
    // Override wins over everything.
    assert_eq!(
        resolve_diff_base(Some("release-1.2"), None, Some("develop"), tmp.path()),
        "release-1.2"
    );
    // Worktree base wins after override; whitespace override falls through.
    assert_eq!(
        resolve_diff_base(
            Some("   "),
            Some("worktree-base"),
            Some("develop"),
            tmp.path()
        ),
        "worktree-base"
    );
    // Config wins when no override and no worktree base.
    assert_eq!(
        resolve_diff_base(None, None, Some("develop"), tmp.path()),
        "develop"
    );
    // Auto-detect when nothing is set: the tmp dir is not a repo, so
    // `get_default_base_ref` errors and falls back to "main".
    assert_eq!(resolve_diff_base(None, None, None, tmp.path()), "main");
}

/// Each workspace member carries its own override and recorded base, and the
/// session-level `base_branch_override` does not leak into any of them. That
/// leak made a multi-repo diff compare every repo against one ref (#3329).
#[test]
fn diff_repos_of_scopes_bases_per_workspace_repo() {
    fn repo(name: &str, base: Option<&str>, over: Option<&str>) -> crate::session::WorkspaceRepo {
        crate::session::WorkspaceRepo {
            name: name.to_string(),
            source_path: format!("/src/{name}"),
            branch: "feature/x".to_string(),
            worktree_path: format!("/ws/{name}"),
            main_repo_path: format!("/src/{name}"),
            managed_by_aoe: true,
            branch_preexisting: false,
            base_branch: base.map(str::to_string),
            base_branch_override: over.map(str::to_string),
        }
    }

    let mut inst = make_test_instance();
    inst.base_branch_override = Some("session-wide".to_string());
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/x".to_string(),
        workspace_dir: "/ws".to_string(),
        repos: vec![
            repo("api", Some("develop"), None),
            repo("web", Some("develop"), Some("epic/checkout")),
            repo("infra", None, None),
        ],
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    let repos = diff_repos_of(&inst);
    let seen: Vec<_> = repos
        .iter()
        .map(|r| {
            (
                r.name.as_deref(),
                r.base_override.as_deref(),
                r.recorded_base.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            (Some("api"), None, Some("develop")),
            (Some("web"), Some("epic/checkout"), Some("develop")),
            (Some("infra"), None, None),
        ],
        "workspace members must not inherit the session-level override"
    );

    // A single-repo session is the other shape: one unnamed entry whose
    // override IS the session-level field.
    let mut single = make_test_instance();
    single.base_branch_override = Some("upstream/main".to_string());
    single.worktree_info = Some(worktree(
        "feature/x",
        "/src/only".to_string(),
        Some("develop"),
    ));
    let repos = diff_repos_of(&single);
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].name, None);
    assert_eq!(repos[0].base_override.as_deref(), Some("upstream/main"));
    assert_eq!(repos[0].recorded_base.as_deref(), Some("develop"));
}

/// The PATCH write lands on exactly the named repo, and the unnamed
/// target still writes the session field. See #3329.
#[test]
fn apply_diff_base_override_writes_only_the_named_repo() {
    let mut inst = make_test_instance();
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/x".to_string(),
        workspace_dir: "/ws".to_string(),
        repos: ["api", "web"]
            .iter()
            .map(|n| crate::session::WorkspaceRepo {
                name: n.to_string(),
                source_path: format!("/src/{n}"),
                branch: "feature/x".to_string(),
                worktree_path: format!("/ws/{n}"),
                main_repo_path: format!("/src/{n}"),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            })
            .collect(),
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    apply_diff_base_override(&mut inst, Some("web"), Some("epic/checkout".to_string()));
    let overrides: Vec<_> = inst
        .all_repos()
        .iter()
        .map(|r| (r.name.as_str(), r.base_branch_override.as_deref()))
        .collect();
    assert_eq!(
        overrides,
        vec![("api", None), ("web", Some("epic/checkout"))]
    );
    assert_eq!(
        inst.base_branch_override, None,
        "a per-repo write must not touch the session field"
    );

    // Clearing one repo leaves its sibling alone.
    apply_diff_base_override(&mut inst, Some("web"), None);
    assert_eq!(inst.all_repos()[1].base_branch_override, None);

    // The unnamed target is the session's own checkout.
    apply_diff_base_override(&mut inst, None, Some("develop".to_string()));
    assert_eq!(inst.base_branch_override.as_deref(), Some("develop"));
}

#[test]
fn session_response_serializes_to_json() {
    let json =
        serde_json::to_value(SessionResponse::from_instance(&make_test_instance(), false)).unwrap();
    assert!(json.get("id").is_some());
    assert_eq!(json["tool"], "claude");
    assert_eq!(json["status"], "Running");
    assert_eq!(json["is_sandboxed"], false);
    assert_eq!(json["claude_fullscreen"], false);
}

/// Optional fields stay off the wire until they are set, so old clients never
/// see a flood of nulls.
#[test]
fn session_response_omits_unset_optional_fields() {
    let cases: [(&str, fn(&mut Instance)); 5] = [
        ("base_branch", |i| {
            i.worktree_info = Some(worktree("feature/test", "/tmp/repo", Some("release-1.2")))
        }),
        ("base_branch_override", |i| {
            i.base_branch_override = Some("upstream/main".to_string())
        }),
        ("pinned_at", |i| i.pin()),
        ("archived_at", |i| i.archive()),
        ("snoozed_until", |i| i.snooze(30)),
    ];

    for (field, set) in cases {
        let mut inst = make_test_instance();
        let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
        assert!(
            json.get(field).is_none(),
            "{field} must be omitted while unset, got: {json}"
        );

        set(&mut inst);
        let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
        assert!(
            json.get(field).is_some(),
            "{field} must appear once set, got: {json}"
        );
    }
}

#[test]
fn session_response_warnings_omitted_when_empty_and_listed_when_set() {
    let mut resp = SessionResponse::from_instance(&make_test_instance(), false);
    assert!(resp.warnings.is_empty());
    let json = serde_json::to_value(&resp).unwrap();
    assert!(
        json.get("warnings").is_none(),
        "empty warnings must be omitted, got: {json}"
    );

    resp.warnings = vec![
        "post-checkout hook failed for repo-a".to_string(),
        "post-checkout hook failed for repo-b".to_string(),
    ];
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(
        json["warnings"],
        serde_json::json!([
            "post-checkout hook failed for repo-a",
            "post-checkout hook failed for repo-b"
        ])
    );
}

/// An expired snooze stays on disk for the next mutation to rewrite, but the
/// wire value is gated on `is_snoozed()` so the web never renders "snoozed 0m".
#[test]
fn session_response_gates_snoozed_until_on_active_snooze() {
    let mut inst = make_test_instance();
    inst.snooze(30);
    assert!(SessionResponse::from_instance(&inst, false)
        .snoozed_until
        .is_some());

    inst.snoozed_until = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    assert!(SessionResponse::from_instance(&inst, false)
        .snoozed_until
        .is_none());
}

#[test]
fn claude_fullscreen_needs_both_claude_and_the_setting() {
    for (tool, enabled, expected) in [
        ("claude", true, true),
        ("claude", false, false),
        ("cursor", true, false),
    ] {
        let mut inst = make_test_instance();
        inst.tool = tool.to_string();
        assert_eq!(
            SessionResponse::from_instance(&inst, enabled).claude_fullscreen,
            expected,
            "tool={tool} enabled={enabled}"
        );
    }
}

#[test]
fn update_bodies_parse_with_their_defaults() {
    for (body, expected) in [
        (r#"{"pinned": true}"#, true),
        (r#"{"pinned": false}"#, false),
    ] {
        let parsed: UpdatePinBody = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.pinned, expected);
    }

    // kill_pane defaults to true so callers that omit it get TUI/CLI parity.
    let archive: UpdateArchiveBody = serde_json::from_str(r#"{"archived": true}"#).unwrap();
    assert!(archive.archived && archive.kill_pane);
    let archive: UpdateArchiveBody =
        serde_json::from_str(r#"{"archived": true, "kill_pane": false}"#).unwrap();
    assert!(!archive.kill_pane);

    // `{"minutes": null}` and an empty body both mean unsnooze.
    for (body, expected) in [
        (r#"{"minutes": 60}"#, Some(60)),
        (r#"{"minutes": null}"#, None),
        ("{}", None),
    ] {
        let parsed: UpdateSnoozeBody = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.minutes, expected);
    }
}

/// Mirrors the TUI snooze dialog presets, so a regression in the shared
/// validator shape is caught here too.
#[test]
fn update_snooze_validates_against_shared_bounds() {
    assert!(crate::session::validate_snooze_duration(0).is_err());
    for &m in &[60u64, 120, 180, 240, 300, 360, 1440, 7 * 1440] {
        assert!(
            crate::session::validate_snooze_duration(m).is_ok(),
            "preset {m} min must pass the validator"
        );
    }
}

#[test]
fn rename_updates_title_without_changing_worktree_branch() {
    let mut inst = make_test_instance();
    inst.worktree_info = Some(worktree("feature/test", "/tmp/repo".to_string(), None));

    apply_session_title_rename(&mut inst, "Renamed Session".to_string());

    assert_eq!(inst.title, "Renamed Session");
    assert_eq!(
        inst.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
        Some("feature/test")
    );
}

#[test]
fn title_only_rename_cache_patch_preserves_newer_path_and_branch() {
    let mut cached = make_test_instance();
    cached.title = "Old title".to_string();
    cached.project_path = "/tmp/worktrees/concurrent".to_string();
    cached.worktree_info = Some(worktree("concurrent-branch", "/tmp/repo".to_string(), None));

    apply_session_rename_cache_patch(
        &mut cached,
        SessionRenameCachePatch {
            title: "New title",
            initial_path: "/tmp/worktrees/initial",
            initial_branch: Some("initial-branch"),
            authoritative_path: "/tmp/worktrees/earlier-snapshot",
            authoritative_branch: Some("earlier-snapshot-branch"),
            renamed_path: None,
            renamed_branch: None,
        },
    );

    assert_eq!(cached.title, "New title");
    assert_eq!(cached.project_path, "/tmp/worktrees/concurrent");
    assert_eq!(
        cached
            .worktree_info
            .as_ref()
            .map(|worktree| worktree.branch.as_str()),
        Some("concurrent-branch")
    );
    let response = SessionResponse::from_instance(&cached, false);
    assert_eq!(response.title, "New title");
}

#[test]
fn tied_rename_cache_patch_publishes_owned_path_and_branch() {
    let mut cached = make_test_instance();
    cached.project_path = "/tmp/worktrees/concurrent".to_string();
    cached.worktree_info = Some(worktree("concurrent-branch", "/tmp/repo".to_string(), None));

    apply_session_rename_cache_patch(
        &mut cached,
        SessionRenameCachePatch {
            title: "New title",
            initial_path: "/tmp/worktrees/initial",
            initial_branch: Some("initial-branch"),
            authoritative_path: "/tmp/worktrees/renamed",
            authoritative_branch: Some("renamed-branch"),
            renamed_path: Some("/tmp/worktrees/renamed"),
            renamed_branch: Some("renamed-branch"),
        },
    );

    assert_eq!(cached.title, "New title");
    assert_eq!(cached.project_path, "/tmp/worktrees/renamed");
    assert_eq!(
        cached
            .worktree_info
            .as_ref()
            .map(|worktree| worktree.branch.as_str()),
        Some("renamed-branch")
    );
}

#[tokio::test]
#[serial_test::serial]
async fn rename_session_distinguishes_cwd_stable_title_and_branch_changes() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let paths = tempfile::tempdir().unwrap();
    let title_path = paths.path().join("my-session");
    let branch_path = paths.path().join("branch-only");
    let title_id = "rename-title-only".to_string();
    let branch_id = "rename-branch-only".to_string();

    let mut title_only = Instance::new(
        "Original title",
        title_path.to_str().expect("UTF-8 temp path"),
    );
    title_only.id = title_id.clone();
    title_only.status = Status::Running;
    title_only.view = crate::session::View::Structured;
    title_only.worktree_info = Some(worktree(
        "my-session",
        paths
            .path()
            .join("missing-repo")
            .to_string_lossy()
            .into_owned(),
        None,
    ));

    let mut branch_only = Instance::new(
        "Branch Only",
        branch_path.to_str().expect("UTF-8 temp path"),
    );
    branch_only.id = branch_id.clone();
    branch_only.status = Status::Running;
    branch_only.worktree_info = Some(worktree(
        "existing-branch",
        paths
            .path()
            .join("missing-repo")
            .to_string_lossy()
            .into_owned(),
        None,
    ));

    let (_storage, state) = build_rename_test_state(
        vec![title_only.clone(), branch_only.clone()],
        vec![title_only, branch_only],
    );
    state.acp_supervisor.test_insert_worker(&title_id).await;

    // The title changes, but its slug already matches both the cwd leaf
    // and branch. Even with the branch toggle armed, this is title-only.
    let title_response = rename_session(
        State(state.clone()),
        Path(title_id.clone()),
        Ok(Json(RenameSessionBody {
            title: "My Session!".to_string(),
            rename_branch: true,
        })),
    )
    .await
    .into_response();
    assert_eq!(title_response.status(), StatusCode::OK);
    let title_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(title_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(title_json["tie_workdir_to_name"], true);
    assert!(
        state.acp_supervisor.is_running(&title_id).await,
        "a cwd-stable title-only rename must not stop the structured worker"
    );

    {
        let instances = state.instances.read().await;
        let renamed = instances.iter().find(|inst| inst.id == title_id).unwrap();
        assert_eq!(renamed.title, "My Session!");
        assert_eq!(renamed.project_path, title_path.to_str().unwrap());
        assert_eq!(
            renamed.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
            Some("my-session")
        );
    }

    let branch_response = rename_session(
        State(state.clone()),
        Path(branch_id.clone()),
        Ok(Json(RenameSessionBody {
            title: "Branch Only".to_string(),
            rename_branch: true,
        })),
    )
    .await
    .into_response();
    assert_eq!(branch_response.status(), StatusCode::CONFLICT);
    let branch_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(branch_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(branch_json["error"], "session_running");

    let instances = state.instances.read().await;
    let rejected = instances.iter().find(|inst| inst.id == branch_id).unwrap();
    assert_eq!(rejected.title, "Branch Only");
    assert_eq!(rejected.project_path, branch_path.to_str().unwrap());
    assert_eq!(
        rejected.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
        Some("existing-branch"),
        "the active branch-only request must be rejected before git mutation"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn rename_session_quiesces_structured_worker_only_when_its_cwd_moves() {
    // Invariant #2260: a live structured worker is pinned to its cwd, so a
    // tied rename that MOVES the worktree must stop the worker first, while one
    // that leaves the cwd in place must not interrupt it. The quiesce runs
    // before the git edit, so the assertion holds even though the edit then
    // fails on a fixture with no real worktree to move.
    let _app_dir = crate::session::test_support::isolate_app_dir();

    struct Case {
        id: &'static str,
        leaf: &'static str,
        new_title: &'static str,
        // Whether the new title's slug relocates the worktree directory.
        moves_cwd: bool,
    }
    // The cwd-stable row's slug ("my-session") equals the existing leaf, so
    // the edit is a no-op move; the cwd-moving row's slug differs, forcing
    // a relocation.
    let cases = [
        Case {
            id: "quiesce-cwd-stable",
            leaf: "my-session",
            new_title: "My Session!",
            moves_cwd: false,
        },
        Case {
            id: "quiesce-cwd-moving",
            leaf: "old-leaf",
            new_title: "A Brand New Name",
            moves_cwd: true,
        },
    ];

    for case in cases {
        let paths = tempfile::tempdir().unwrap();
        let project_path = paths.path().join(case.leaf);
        let mut inst = Instance::new(
            "Original title",
            project_path.to_str().expect("UTF-8 temp path"),
        );
        inst.id = case.id.to_string();
        // Idle, not Running: a structured session the user "stopped" sits at
        // Idle yet still owns a live worker, the gap `blocks_worktree_edit`
        // misses and quiesce closes.
        inst.status = Status::Idle;
        inst.view = crate::session::View::Structured;
        inst.worktree_info = Some(worktree(
            case.leaf,
            paths
                .path()
                .join("missing-repo")
                .to_string_lossy()
                .into_owned(),
            None,
        ));

        let (_storage, state) = build_rename_test_state(vec![inst.clone()], vec![inst]);
        state.acp_supervisor.test_insert_worker(case.id).await;

        let _ = rename_session(
            State(state.clone()),
            Path(case.id.to_string()),
            Ok(Json(RenameSessionBody {
                title: case.new_title.to_string(),
                rename_branch: false,
            })),
        )
        .await
        .into_response();

        assert_eq!(
            state.acp_supervisor.is_running(case.id).await,
            !case.moves_cwd,
            "{}: worker should be {} for moves_cwd={}",
            case.id,
            if case.moves_cwd {
                "stopped"
            } else {
                "preserved"
            },
            case.moves_cwd
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn set_worktree_name_quiesces_structured_worker_only_when_its_cwd_moves() {
    // The standalone-endpoint mirror of the rename_session gate above: both
    // stop a live structured worker only when the edit actually moves the cwd
    // (#2260). The quiesce precedes the git edit, so the assertion holds even
    // though the edit then fails on a fixture with no real worktree.
    let _app_dir = crate::session::test_support::isolate_app_dir();
    // set_worktree_name refuses a tied managed worktree (tied callers must
    // go through rename_session), so untie the profile to reach the worker
    // gate that this test exercises.
    let mut overrides = serde_json::Map::new();
    overrides.insert(
        "session".to_string(),
        serde_json::json!({ "tie_workdir_to_name": false }),
    );
    crate::session::config::profile_config::save_profile_config(
        "test",
        &crate::session::config::profile_config::ProfileConfig {
            description: None,
            overrides,
        },
    )
    .expect("write test profile override");

    struct Case {
        id: &'static str,
        leaf: &'static str,
        new_name: &'static str,
        // Whether the requested name relocates the worktree directory.
        moves_cwd: bool,
    }
    // The cwd-stable row's name equals the existing leaf (a no-op move); the
    // cwd-moving row's name differs, forcing a relocation.
    let cases = [
        Case {
            id: "sw-cwd-stable",
            leaf: "my-session",
            new_name: "my-session",
            moves_cwd: false,
        },
        Case {
            id: "sw-cwd-moving",
            leaf: "old-leaf",
            new_name: "new-leaf",
            moves_cwd: true,
        },
    ];

    for case in cases {
        let paths = tempfile::tempdir().unwrap();
        let project_path = paths.path().join(case.leaf);
        let mut inst = Instance::new(
            "Original title",
            project_path.to_str().expect("UTF-8 temp path"),
        );
        inst.id = case.id.to_string();
        inst.source_profile = "test".to_string();
        inst.status = Status::Idle;
        inst.view = crate::session::View::Structured;
        inst.worktree_info = Some(worktree(
            case.leaf,
            paths
                .path()
                .join("missing-repo")
                .to_string_lossy()
                .into_owned(),
            None,
        ));

        let storage = Storage::new_unwatched("test").unwrap();
        storage
            .update(|instances, _groups| {
                *instances = vec![inst.clone()];
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        state.acp_supervisor.test_insert_worker(case.id).await;

        let _ = set_worktree_name(
            State(state.clone()),
            Path(case.id.to_string()),
            Ok(Json(SetWorktreeNameBody {
                name: case.new_name.to_string(),
                rename_branch: false,
            })),
        )
        .await
        .into_response();

        assert_eq!(
            state.acp_supervisor.is_running(case.id).await,
            !case.moves_cwd,
            "{}: worker should be {} for moves_cwd={}",
            case.id,
            if case.moves_cwd {
                "stopped"
            } else {
                "preserved"
            },
            case.moves_cwd
        );
    }
}

#[test]
fn worktree_name_edit_updates_path_and_optionally_branch() {
    let mut inst = make_test_instance();
    inst.project_path = "/tmp/repo-worktrees/old".to_string();
    inst.title = "My Session".to_string();
    inst.worktree_info = Some(worktree("old", "/tmp/repo".to_string(), None));

    // Path-only edit leaves the branch and title untouched.
    apply_worktree_name_edit(&mut inst, "/tmp/repo-worktrees/new", None);
    assert_eq!(inst.project_path, "/tmp/repo-worktrees/new");
    assert_eq!(inst.title, "My Session");
    assert_eq!(
        inst.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
        Some("old")
    );

    // Branch rename also updates worktree_info.branch.
    apply_worktree_name_edit(&mut inst, "/tmp/repo-worktrees/newer", Some("newer"));
    assert_eq!(inst.project_path, "/tmp/repo-worktrees/newer");
    assert_eq!(inst.title, "My Session");
    assert_eq!(
        inst.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
        Some("newer")
    );
}

#[test]
#[serial_test::serial]
fn apply_post_restart_sync_propagates_agent_session_id() {
    // The rapid double-restart case: in-memory state is stale because the 2s
    // poller has not refreshed, while the just-finished restart produced a
    // Claude UUID. The sync must propagate it, or a second ensure_session inside
    // the poller window mints a fresh UUID and orphans the conversation.
    let mut live = make_test_instance();
    live.status = Status::Stopped;
    live.last_error = Some("prior failure".to_string());
    live.agent_session_id = None;
    live.last_start_time = None;
    let before = live.clone();

    let mut started = make_test_instance();
    started.status = Status::Starting;
    started.agent_session_id = Some("claude-uuid-restart".to_string());
    started.omp_capture_generation = Some("omp-generation-restart".to_string());
    let mut poller = crate::session::poller::SessionPoller::new("omp-restarted".to_string());
    assert_eq!(
        poller.start(before.id.clone(), Box::new(|| None), Box::new(|_| {}), None,),
        crate::session::poller::PollerSpawn::Spawned
    );
    let restarted_poller = std::sync::Arc::new(std::sync::Mutex::new(poller));
    started.session_id_poller = Some(restarted_poller.clone());
    started.last_start_time = Some(std::time::Instant::now());

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Starting);
    assert!(live.last_error.is_none());
    assert_eq!(
        live.agent_session_id.as_deref(),
        Some("claude-uuid-restart")
    );
    assert_eq!(
        live.omp_capture_generation.as_deref(),
        Some("omp-generation-restart")
    );
    assert!(live.session_id_poller.is_some());
    assert_eq!(live.last_start_time, started.last_start_time);

    let mut generation_converged = before.clone();
    generation_converged.agent_session_id = Some("peer-sid".to_string());
    generation_converged.omp_capture_generation = Some("omp-generation-restart".to_string());
    apply_post_restart_identity_sync(&mut generation_converged, &before, &started);
    assert_eq!(
        generation_converged.agent_session_id.as_deref(),
        Some("peer-sid")
    );
    assert!(generation_converged.session_id_poller.is_some());

    let mut peer_relaunched = before.clone();
    peer_relaunched.omp_capture_generation = Some("peer-generation".to_string());
    apply_post_restart_identity_sync(&mut peer_relaunched, &before, &started);
    assert_eq!(
        peer_relaunched.omp_capture_generation.as_deref(),
        Some("peer-generation")
    );
    assert!(std::sync::Arc::ptr_eq(
        peer_relaunched
            .session_id_poller
            .as_ref()
            .expect("running restart poller"),
        &restarted_poller,
    ));
    restarted_poller
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .stop();
}

#[test]
fn apply_post_restart_identity_sync_clears_repair_backoff_when_restart_poller_runs() {
    let mut before = make_test_instance();
    before.omp_capture_generation = Some("generation-a".to_string());
    let now = std::time::Instant::now();
    before.poller_repair.defer(now);
    before.poller_repair.defer(now);
    assert_eq!(before.poller_repair.deferrals(), 2);

    let mut started = before.clone();
    started.omp_capture_generation = Some("generation-b".to_string());
    started.poller_repair.reset();
    let mut poller = crate::session::poller::SessionPoller::new("omp-restarted".to_string());
    assert_eq!(
        poller.start(before.id.clone(), Box::new(|| None), Box::new(|_| {}), None,),
        crate::session::poller::PollerSpawn::Spawned
    );
    let restarted_poller = std::sync::Arc::new(std::sync::Mutex::new(poller));
    started.session_id_poller = Some(restarted_poller.clone());

    let mut live = before.clone();
    apply_post_restart_identity_sync(&mut live, &before, &started);
    assert_eq!(
        live.poller_repair.deferrals(),
        0,
        "the merged live row must not keep the pre-restart backoff"
    );

    let mut peer_relaunched = before.clone();
    peer_relaunched.omp_capture_generation = Some("peer-generation".to_string());
    apply_post_restart_identity_sync(&mut peer_relaunched, &before, &started);
    assert_eq!(peer_relaunched.poller_repair.deferrals(), 0);

    let mut not_started = started.clone();
    not_started.session_id_poller = None;
    let mut live = before.clone();
    apply_post_restart_identity_sync(&mut live, &before, &not_started);
    assert_eq!(
        live.poller_repair.deferrals(),
        2,
        "a restart without a running poller leaves the schedule alone"
    );
    restarted_poller
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .stop();
}

#[test]
fn apply_post_restart_sync_overwrites_stale_session_id() {
    // When the in-memory id is non-None and the start path produced a newer
    // one, `started` wins. In practice acquire_session_id reuses the existing
    // id, but that is the contract.
    let mut live = make_test_instance();
    live.agent_session_id = Some("stale-id".to_string());
    let before = live.clone();

    let mut started = make_test_instance();
    started.agent_session_id = Some("fresh-id".to_string());

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.agent_session_id.as_deref(), Some("fresh-id"));
}

#[test]
fn apply_post_restart_sync_propagates_resume_failed_marker_and_error() {
    let mut live = make_test_instance();
    live.status = Status::Running;
    live.last_error = Some("prior failure".to_string());
    live.agent_session_id = Some("sid-before".to_string());
    live.resume_probe_failed_sid = None;
    let before = live.clone();

    let mut started = make_test_instance();
    started.status = Status::Error;
    started.agent_session_id = Some("sid-after".to_string());
    started.resume_probe_failed_sid = Some("sid-after".to_string());
    started.last_error =
        Some("resume failed for sid sid-after; preserved for explicit retry".to_string());
    started.last_error_check = Some(std::time::Instant::now());

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Error);
    assert_eq!(
        live.last_error.as_deref(),
        Some("resume failed for sid sid-after; preserved for explicit retry")
    );
    assert!(live.last_error_check.is_some());
    assert_eq!(live.agent_session_id.as_deref(), Some("sid-after"));
    assert_eq!(live.resume_probe_failed_sid.as_deref(), Some("sid-after"));
}

#[test]
fn apply_cascade_state_sync_propagates_marker_without_status() {
    let mut live = make_test_instance();
    live.status = Status::Running;
    live.last_error = Some("keep me".to_string());
    live.agent_session_id = Some("sid-before".to_string());
    live.resume_probe_failed_sid = None;
    let before = live.clone();

    let mut started = make_test_instance();
    started.status = Status::Error;
    started.last_error = Some("resume failed".to_string());
    started.agent_session_id = Some("sid-after".to_string());
    started.resume_probe_failed_sid = Some("sid-after".to_string());

    apply_cascade_state_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Running);
    assert_eq!(live.last_error.as_deref(), Some("keep me"));
    assert_eq!(live.agent_session_id.as_deref(), Some("sid-after"));
    assert_eq!(live.resume_probe_failed_sid.as_deref(), Some("sid-after"));
}

#[test]
fn apply_post_restart_sync_preserves_peer_sid_write() {
    let mut before = make_test_instance();
    before.agent_session_id = Some("stale-restart-sid".to_string());
    before.resume_probe_failed_sid = None;

    let mut live = make_test_instance();
    live.agent_session_id = Some("peer-fresh-sid".to_string());
    live.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());

    let mut started = make_test_instance();
    started.status = Status::Error;
    started.agent_session_id = Some("stale-restart-sid".to_string());
    started.resume_probe_failed_sid = Some("stale-restart-sid".to_string());
    started.last_error = Some("resume failed".to_string());

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Error);
    assert_eq!(live.last_error.as_deref(), Some("resume failed"));
    assert_eq!(live.agent_session_id.as_deref(), Some("peer-fresh-sid"));
    assert_eq!(
        live.resume_probe_failed_sid.as_deref(),
        Some("peer-fresh-sid")
    );
}

#[test]
fn restart_sync_rejects_an_older_lifecycle_generation() {
    let mut before = make_test_instance();
    before.lifecycle_generation = 4;

    let mut started = before.clone();
    started.status = Status::Error;
    started.agent_session_id = Some("stale-restart-sid".to_string());
    started.retroactive_capture_excludes = ["stale-exclusion".to_string()].into();

    let mut live = before.clone();
    live.lifecycle_generation = 5;
    live.status = Status::Running;
    live.agent_session_id = Some("newer-restart-sid".to_string());
    live.retroactive_capture_excludes = ["newer-exclusion".to_string()].into();

    assert!(!apply_post_restart_sync(&mut live, &before, &started));
    apply_cascade_state_sync(&mut live, &before, &started);

    assert_eq!(live.lifecycle_generation, 5);
    assert_eq!(live.status, Status::Running);
    assert_eq!(live.agent_session_id.as_deref(), Some("newer-restart-sid"));
    assert_eq!(
        live.retroactive_capture_excludes,
        ["newer-exclusion".to_string()].into()
    );
}

#[test]
fn apply_post_restart_sync_preserves_peer_marker_for_same_sid() {
    let mut before = make_test_instance();
    before.agent_session_id = Some("same-sid".to_string());
    before.resume_probe_failed_sid = None;

    let mut live = before.clone();
    live.resume_probe_failed_sid = Some("same-sid".to_string());

    let mut started = before.clone();
    started.status = Status::Starting;
    started.resume_probe_failed_sid = None;

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Starting);
    assert_eq!(live.agent_session_id.as_deref(), Some("same-sid"));
    assert_eq!(live.resume_probe_failed_sid.as_deref(), Some("same-sid"));
}

#[test]
fn apply_cascade_state_sync_preserves_peer_sid_write() {
    let mut before = make_test_instance();
    before.agent_session_id = Some("stale-restart-sid".to_string());
    before.resume_probe_failed_sid = None;

    let mut live = make_test_instance();
    live.status = Status::Running;
    live.last_error = Some("keep me".to_string());
    live.agent_session_id = Some("peer-fresh-sid".to_string());
    live.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());

    let mut started = make_test_instance();
    started.status = Status::Error;
    started.last_error = Some("resume failed".to_string());
    started.agent_session_id = Some("stale-restart-sid".to_string());
    started.resume_probe_failed_sid = Some("stale-restart-sid".to_string());

    apply_cascade_state_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Running);
    assert_eq!(live.last_error.as_deref(), Some("keep me"));
    assert_eq!(live.agent_session_id.as_deref(), Some("peer-fresh-sid"));
    assert_eq!(
        live.resume_probe_failed_sid.as_deref(),
        Some("peer-fresh-sid")
    );
}

#[test]
#[serial_test::serial]
fn send_message_post_restart_save_preserves_peer_sid_write() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");

    let profile = "send-post-restart-peer-sid";
    let storage = Storage::new_unwatched(profile).unwrap();
    let mut seed = make_test_instance();
    let id = seed.id.clone();
    seed.agent_session_id = Some("peer-fresh-sid".to_string());
    seed.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());
    storage
        .update(|instances, _groups| {
            instances.push(seed.clone());
            Ok(())
        })
        .unwrap();

    let mut sync_base_for_save = make_test_instance();
    sync_base_for_save.id = id.clone();
    sync_base_for_save.agent_session_id = Some("stale-restart-sid".to_string());
    sync_base_for_save.resume_probe_failed_sid = None;

    let mut started_for_save = make_test_instance();
    started_for_save.id = id.clone();
    started_for_save.status = Status::Starting;
    started_for_save.agent_session_id = Some("stale-restart-sid".to_string());
    started_for_save.resume_probe_failed_sid = None;

    storage
        .update(|all, _groups| {
            if let Some(disk_inst) = all.iter_mut().find(|i| i.id == id) {
                apply_post_restart_sync(disk_inst, &sync_base_for_save, &started_for_save);
                disk_inst.touch_last_accessed();
            }
            Ok(())
        })
        .unwrap();

    let reloaded = storage.load().unwrap();
    let disk = reloaded.iter().find(|i| i.id == seed.id).unwrap();
    assert_eq!(disk.status, Status::Starting);
    assert_eq!(disk.agent_session_id.as_deref(), Some("peer-fresh-sid"));
    assert_eq!(
        disk.resume_probe_failed_sid.as_deref(),
        Some("peer-fresh-sid")
    );
    assert!(disk.last_accessed_at.is_some());
}

/// A tool name must resolve to a built-in agent, or to a custom agent whose
/// configured command is non-empty.
#[test]
#[serial_test::serial]
fn session_tool_identity_accepts_builtins_and_non_empty_custom_agents() {
    // (custom_agents config body, agent, expected)
    let cases = [
        ("", "claude", true),
        (
            "remote-claude = \"ssh -t host claude\"",
            "remote-claude",
            true,
        ),
        ("", "surprise-agent", false),
        ("remote-claude = \"\"", "remote-claude", false),
        ("remote-claude = \"   \"", "remote-claude", false),
    ];

    for (custom_agents, agent, expected) in cases {
        let temp_home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
        let app_dir = crate::session::get_app_dir().expect("isolated app dir");
        std::fs::create_dir_all(&app_dir).unwrap();
        if !custom_agents.is_empty() {
            std::fs::write(
                app_dir.join("config.toml"),
                format!("[session.custom_agents]\n{custom_agents}\n"),
            )
            .unwrap();
        }
        let project = tempfile::tempdir().unwrap();

        assert_eq!(
            validate_session_tool_identity(agent, "default", project.path()),
            expected,
            "agent={agent} custom_agents={custom_agents:?}"
        );
    }
}

/// A custom agent declared under one profile is invisible from another.
#[test]
#[serial_test::serial]
fn session_tool_identity_uses_requested_profile() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    let work_profile = app_dir.join("profiles").join("work");
    std::fs::create_dir_all(&work_profile).unwrap();
    std::fs::write(
        work_profile.join("config.toml"),
        r#"
            [session.custom_agents]
            work-agent = "ssh -t work claude"
        "#,
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();

    assert!(!validate_session_tool_identity(
        "work-agent",
        "default",
        project.path()
    ));
    assert!(validate_session_tool_identity(
        "work-agent",
        "work",
        project.path()
    ));
}

#[test]
#[serial_test::serial]
fn session_tool_identity_uses_repo_aware_config_but_not_repo_custom_agents() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        app_dir.join("config.toml"),
        r#"
            [session.custom_agents]
            my-agent = "ssh -t lenovo claude"
        "#,
    )
    .unwrap();

    let project = tempfile::tempdir().unwrap();
    let repo_config_dir = project.path().join(".agent-of-empires");
    std::fs::create_dir_all(&repo_config_dir).unwrap();
    std::fs::write(
        repo_config_dir.join("config.toml"),
        r#"
            [session.custom_agents]
            repo-agent = "ssh -t repo claude"
        "#,
    )
    .unwrap();

    // The user's own custom agent resolves through the repo-aware path.
    assert!(validate_session_tool_identity(
        "my-agent",
        "default",
        project.path()
    ));
    // A repo-defined one does not exist as far as AoE is concerned (#3154).
    assert!(!validate_session_tool_identity(
        "repo-agent",
        "default",
        project.path()
    ));
}

/// Build one structured, idle session with an empty `source_profile`, so
/// `purge_session_artifacts` refuses on its first line. The teardown that
/// follows is what a delete must not start under an in-flight submission;
/// these tests only need to observe that it waits for one.
fn delete_race_state(id: &str) -> std::sync::Arc<crate::server::AppState> {
    delete_race_state_for(&[id])
}

/// [`delete_race_state`] for a workspace: every id shares the shape, so a
/// sibling teardown can be observed the same way the owner's is.
fn delete_race_state_for(ids: &[&str]) -> std::sync::Arc<crate::server::AppState> {
    let instances = ids
        .iter()
        .map(|id| {
            let mut inst = Instance::new("delete-3650", "/tmp/aoe-3650-delete");
            inst.id = (*id).to_string();
            inst.view = crate::session::View::Structured;
            inst.status = Status::Idle;
            inst
        })
        .collect();
    crate::server::test_support::build_test_app_state(instances)
}

/// #3650: prompt submission moved off `instance_lock`, so a permanent delete
/// that takes only `instance_lock` no longer excludes a queue drain that
/// snapshotted an idle turn and is on its way to `send_turn`. The delete would
/// then stop the worker, purge the transcript and remove the worktree under a
/// delivery already in flight.
///
/// Each permanent-delete path is checked the same way: hold the session's
/// submission guard and assert the delete parks before any teardown, then
/// completes once the guard drops.
#[tokio::test]
async fn permanent_deletion_waits_for_an_in_flight_submission() {
    let _home = crate::session::test_support::isolate_app_dir();
    use std::time::Duration;

    // Direct delete.
    let state = delete_race_state("sess-3650-direct");
    let delivering = state
        .session_service
        .prompt_submission("sess-3650-direct")
        .await;
    let mut claims = state.session_service.watch_submission_claims();
    let delete = {
        let state = std::sync::Arc::clone(&state);
        async move {
            delete_session(
                State(state),
                Path("sess-3650-direct".to_string()),
                Some(Json(DeleteSessionBody::default())),
            )
            .await
            .into_response()
        }
    };
    tokio::pin!(delete);
    assert!(
        futures_util::poll!(&mut delete).is_pending(),
        "a delete must not tear a session down under an in-flight submission"
    );
    assert_eq!(
        claims
            .try_recv()
            .expect("contender reached submission claim"),
        "sess-3650-direct"
    );
    assert_eq!(
        state.instances.read().await[0].status,
        Status::Idle,
        "the session must not even be marked Deleting yet"
    );
    drop(delivering);
    tokio::time::timeout(Duration::from_secs(10), delete)
        .await
        .expect("the delete lands once the submission releases the session");

    // Workspace teardown, on the owner's own guard.
    let state = delete_race_state("sess-3650-owner");
    let delivering = state
        .session_service
        .prompt_submission("sess-3650-owner")
        .await;
    let mut claims = state.session_service.watch_submission_claims();
    let workspace = {
        let state = std::sync::Arc::clone(&state);
        async move {
            purge_workspace_artifacts(
                &state,
                "sess-3650-owner".to_string(),
                vec![("sess-3650-owner".to_string(), DeleteSessionBody::default())],
                false,
            )
            .await
        }
    };
    tokio::pin!(workspace);
    assert!(
        futures_util::poll!(&mut workspace).is_pending(),
        "a workspace teardown must wait for the owner's in-flight submission"
    );
    assert_eq!(
        claims
            .try_recv()
            .expect("contender reached submission claim"),
        "sess-3650-owner"
    );
    drop(delivering);
    tokio::time::timeout(Duration::from_secs(10), workspace)
        .await
        .expect("the workspace teardown lands once the submission releases");

    // A workspace teardown must also wait on a sibling's live submission.
    let state = delete_race_state_for(&["sess-3650-sib", "sess-3650-ws-owner"]);
    let delivering = state
        .session_service
        .prompt_submission("sess-3650-sib")
        .await;
    let mut claims = state.session_service.watch_submission_claims();
    let workspace = {
        let state = std::sync::Arc::clone(&state);
        async move {
            purge_workspace_artifacts(
                &state,
                "sess-3650-ws-owner".to_string(),
                vec![
                    ("sess-3650-sib".to_string(), DeleteSessionBody::default()),
                    (
                        "sess-3650-ws-owner".to_string(),
                        DeleteSessionBody::default(),
                    ),
                ],
                false,
            )
            .await
        }
    };
    tokio::pin!(workspace);
    assert!(
        futures_util::poll!(&mut workspace).is_pending(),
        "a workspace teardown must wait for a sibling's in-flight submission"
    );
    assert!(
        std::iter::from_fn(|| claims.try_recv().ok()).any(|id| id == "sess-3650-sib"),
        "contender reached the sibling submission claim"
    );
    assert!(
        state
            .instances
            .read()
            .await
            .iter()
            .all(|i| i.status == Status::Idle),
        "no row may be marked Deleting while the sibling's submission is in flight"
    );
    drop(delivering);
    tokio::time::timeout(Duration::from_secs(10), workspace)
        .await
        .expect("the workspace teardown lands once the sibling submission releases");
}

/// Retention waits for submissions, then rechecks restores under the instance lock.
#[tokio::test]
async fn the_retention_purge_takes_submission_before_the_instance_lock() {
    let _home = crate::session::test_support::isolate_app_dir();
    std::fs::write(
        crate::session::get_app_dir().unwrap().join("config.toml"),
        "[session]\ntrash_retention_days = 1\n",
    )
    .unwrap();
    let mut inst = make_test_instance();
    inst.trashed_at = Some(chrono::Utc::now() - chrono::Duration::days(2));
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let submission = state.session_service.prompt_submission(&id).await;
    let mut claims = state.session_service.watch_submission_claims();
    let purge = purge_expired_trash(&state);
    tokio::pin!(purge);
    assert!(futures_util::poll!(&mut purge).is_pending());
    assert_eq!(
        claims.try_recv().expect("purge reaches submission claim"),
        id
    );
    let lock = state.instance_lock(&id).await;
    let held = lock
        .try_lock()
        .expect("submission must precede instance lock");
    drop(submission);
    assert!(futures_util::poll!(&mut purge).is_pending());
    state.instances.write().await[0].trashed_at = None;
    drop(held);
    purge.await;
    assert_eq!(
        state.instances.read().await[0].id,
        id,
        "restore wins before purge snapshot"
    );
}

/// #3650's barrier applies to every handler that stops a worker, not only the
/// ones that delete. `drain_queued_prompts_once` reads status and the
/// trashed/archived/snoozed flags once under the submission guard and only then
/// reaches `send_turn`, which respawns a worker it finds gone, so a stop landing
/// inside that window is undone. Before #3639 the drain held `instance_lock`
/// across delivery and excluded these four handlers.
#[tokio::test]
async fn worker_stopping_handlers_wait_for_an_in_flight_submission() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    use std::time::Duration;

    async fn call(
        which: &str,
        state: std::sync::Arc<crate::server::AppState>,
        id: String,
    ) -> axum::response::Response {
        match which {
            "stop" => stop_session(State(state), Path(id)).await.into_response(),
            "trash" => trash_session(State(state), Path(id), None)
                .await
                .into_response(),
            "archive" => update_session_archive(
                State(state),
                Path(id),
                Ok(Json(UpdateArchiveBody {
                    archived: true,
                    kill_pane: true,
                })),
            )
            .await
            .into_response(),
            "snooze" => update_session_snooze(
                State(state),
                Path(id),
                Ok(Json(UpdateSnoozeBody { minutes: Some(30) })),
            )
            .await
            .into_response(),
            other => unreachable!("unknown handler {other}"),
        }
    }

    for which in ["stop", "trash", "archive", "snooze"] {
        let id = format!("sess-3650-{which}");
        let state = delete_race_state(&id);
        let delivering = state.session_service.prompt_submission(&id).await;
        let mut claims = state.session_service.watch_submission_claims();
        let handler = {
            let state = std::sync::Arc::clone(&state);
            let id = id.clone();
            async move { call(which, state, id).await }
        };
        tokio::pin!(handler);
        assert!(
            futures_util::poll!(&mut handler).is_pending(),
            "{which} must not quiesce a worker a submission is mid-delivery on"
        );
        assert_eq!(
            claims
                .try_recv()
                .expect("contender reached submission claim"),
            id
        );
        assert_eq!(
            state.instances.read().await[0].status,
            Status::Idle,
            "{which} must not have touched the session yet"
        );

        drop(delivering);
        tokio::time::timeout(Duration::from_secs(10), handler)
            .await
            .unwrap_or_else(|_| panic!("{which} must finish once the submission releases"));
    }
}

/// #3651: `prompt_submission` auto-vivifies a registry entry for whatever id it
/// is handed and nothing prunes it, so every externally reachable mutation must
/// prove the session exists first, or an authenticated client can grow daemon
/// memory with random ids.
#[tokio::test]
async fn session_mutations_allocate_no_prompt_lock_for_an_unknown_id() {
    let state = crate::server::test_support::build_test_app_state(Vec::new());
    let service = std::sync::Arc::clone(&state.session_service);

    for i in 0..3 {
        let id = format!("sess-gone-{i}");
        assert_eq!(
            rename_session(
                State(std::sync::Arc::clone(&state)),
                Path(id.clone()),
                Ok(Json(RenameSessionBody {
                    title: "new title".to_string(),
                    rename_branch: false,
                })),
            )
            .await
            .into_response()
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            set_worktree_name(
                State(std::sync::Arc::clone(&state)),
                Path(id.clone()),
                Ok(Json(SetWorktreeNameBody {
                    name: "new-dir".to_string(),
                    rename_branch: false,
                })),
            )
            .await
            .into_response()
            .status(),
            StatusCode::NOT_FOUND
        );
        assert!(matches!(
            crate::server::attach_project::attach_project(
                &state,
                &id,
                std::path::Path::new("/tmp"),
                crate::session::attach_project::ExistingBranch::Refuse,
            )
            .await,
            Err(crate::server::attach_project::AttachError::NotFound)
        ));
        assert!(matches!(
            service
                .edit_queued_prompt(&id, "q1".to_string(), "text".to_string())
                .await,
            crate::server::session_service::EditQueuedOutcome::NotFound
        ));
        assert!(!service.remove_queued_prompt(&id, "q1".to_string()).await);
        service.clear_queued_prompts(&id).await;
    }

    assert_eq!(
        service.prompt_locks_len().await,
        0,
        "an id that was never admitted must not leave a lock-registry entry behind"
    );
}

#[test]
fn create_session_validates_tool_before_builder_or_persistence() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/api/sessions/create.rs"),
    )
    .unwrap();
    let create_start = source.find("pub async fn create_session").unwrap();
    let create_source = &source[create_start..];
    // Anchor on the call, not the bare name: a comment above mentions the
    // fn earlier in the handler and would satisfy a name-only find.
    let validation = create_source
        .find("if !validate_session_tool_identity(")
        .unwrap();
    let unwrap_or_else = create_source.find("body.profile.unwrap_or_else").unwrap();
    let spawn_blocking = create_source.find("tokio::task::spawn_blocking").unwrap();
    // Build and persistence both go through session_spawn.
    let session_spawn = create_source
        .find("crate::server::session_spawn::")
        .unwrap();

    assert!(validation < unwrap_or_else);
    assert!(validation < spawn_blocking);
    assert!(validation < session_spawn);
    assert!(create_source.contains("body.profile.as_deref().unwrap_or(&state.profile)"));
    assert!(create_source.contains("std::path::Path::new(&body.path)"));
    assert!(!create_source[validation..spawn_blocking].contains("command_override"));
}

#[tokio::test]
async fn ensure_session_refreshes_instance_after_instance_lock() {
    let _home = crate::session::test_support::isolate_app_dir();
    let inst = make_test_instance();
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let lock = state.instance_lock(&id).await;
    let held = lock.lock().await;
    let handler = ensure_session(State(state.clone()), Path(id.clone()));
    tokio::pin!(handler);
    assert!(futures_util::poll!(&mut handler).is_pending());
    state.instances.write().await.clear();
    drop(held);
    assert_eq!(
        handler.await.into_response().status(),
        StatusCode::NOT_FOUND
    );
}

/// The three terminal handlers must take the per-session lock before
/// snapshotting the instance, like `ensure_session`; a read-then-lock order
/// lets a concurrent mutation land between the two and hands `spawn_blocking`
/// a stale clone.
#[tokio::test]
async fn terminal_handlers_take_instance_lock_before_snapshot() {
    let _home = crate::session::test_support::isolate_app_dir();
    for which in ["ensure", "container", "kill"] {
        let inst = make_test_instance();
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        let lock = state.instance_lock(&id).await;
        let held = lock.lock().await;
        let handler = async {
            let query =
                axum::extract::Query(crate::server::live_ws::TerminalIndexQuery { index: 1 });
            match which {
                "ensure" => ensure_terminal(State(state.clone()), Path(id.clone()), query)
                    .await
                    .into_response(),
                "container" => {
                    ensure_container_terminal(State(state.clone()), Path(id.clone()), query)
                        .await
                        .into_response()
                }
                "kill" => kill_terminal(State(state.clone()), Path(id.clone()), query)
                    .await
                    .into_response(),
                _ => unreachable!(),
            }
        };
        tokio::pin!(handler);
        assert!(futures_util::poll!(&mut handler).is_pending(), "{which}");
        state.instances.write().await.clear();
        drop(held);
        assert_eq!(handler.await.status(), StatusCode::NOT_FOUND, "{which}");
    }
}

/// A workspace row can persist with `repos: []`; a file-diff request that
/// omits `?repo=` must get a 400 for it, not a panic on the empty repo list.
#[tokio::test]
async fn diff_file_rejects_workspace_with_no_repos() {
    use axum::extract::Query;

    let mut inst = Instance::new("empty-ws", "/tmp/aoe-empty-ws");
    inst.id = "empty-ws".to_string();
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "main".to_string(),
        workspace_dir: "/tmp/aoe-empty-ws".to_string(),
        repos: Vec::new(),
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });
    let state = crate::server::test_support::build_test_app_state(vec![inst]);

    let resp = session_diff_file(
        State(state),
        Path("empty-ws".to_string()),
        Query(FileDiffQuery {
            path: "Cargo.toml".to_string(),
            repo: None,
        }),
    )
    .await
    .into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn send_message_refreshes_instance_after_instance_lock() {
    let _home = crate::session::test_support::isolate_app_dir();
    let inst = make_test_instance();
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let lock = state.instance_lock(&id).await;
    let held = lock.lock().await;
    let handler = send_message(
        State(state.clone()),
        Path(id.clone()),
        Ok(Json(SendMessageRequest {
            message: "hello".into(),
            revive: false,
        })),
    );
    tokio::pin!(handler);
    assert!(futures_util::poll!(&mut handler).is_pending());
    state.instances.write().await.clear();
    drop(held);
    assert_eq!(
        handler.await.into_response().status(),
        StatusCode::NOT_FOUND
    );
}
// Regression for a path-traversal vulnerability in the first cut of
// `/api/sessions/{id}/diff/file?path=...`, where any authenticated user could
// pass `?path=/etc/passwd` and have the server dump it in a diff response.

use crate::git::diff::{DiffFile, FileStatus};
use std::path::PathBuf;
use tempfile::TempDir;

fn changed(paths: &[&str]) -> Vec<DiffFile> {
    paths
        .iter()
        .map(|p| DiffFile {
            path: PathBuf::from(p),
            old_path: None,
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
        })
        .collect()
}

#[test]
fn validate_diff_path_rejects_unsafe_shapes() {
    let dir = TempDir::new().unwrap();
    for path in [
        "/etc/passwd",
        "../../etc/passwd",
        "src/../../etc/passwd",
        "",
    ] {
        let err = validate_diff_path(
            dir.path(),
            std::path::Path::new(path),
            &changed(&["src/main.rs"]),
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST, "path={path:?}");
    }
}

/// An in-repo file that exists but is not in the changed set is accepted for the
/// full-file fallback (#1810), flagged `is_changed = false`. A changed file
/// deleted from disk stays diffable, so the validator falls back to the
/// non-canonical path when `canonicalize()` fails.
#[test]
fn validate_diff_path_accepts_in_repo_and_changed_files() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("existing.txt"), "hello").unwrap();
    std::fs::write(dir.path().join("changed.txt"), "hello").unwrap();

    // (requested path, changed set, expected is_changed)
    for (path, changed_set, expected) in [
        ("existing.txt", &["src/main.rs"][..], false),
        ("changed.txt", &["changed.txt"][..], true),
        ("deleted.txt", &["deleted.txt"][..], true),
    ] {
        let (_, is_changed) = validate_diff_path(
            dir.path(),
            std::path::Path::new(path),
            &changed(changed_set),
        )
        .unwrap_or_else(|e| panic!("{path} should validate, got {:?}", e.0));
        assert_eq!(is_changed, expected, "path={path}");
    }
}

/// Not in the changed set and not on disk: nothing to show.
#[test]
fn validate_diff_path_rejects_nonexistent_unchanged_file() {
    let dir = TempDir::new().unwrap();
    let err = validate_diff_path(
        dir.path(),
        std::path::Path::new("ghost.txt"),
        &changed(&["src/main.rs"]),
    )
    .unwrap_err();
    assert_eq!(err.0, StatusCode::NOT_FOUND);
}

#[test]
fn truncate_title_truncates_on_character_boundaries() {
    // (input, limit, expected)
    for (input, limit, expected) in [
        ("hello", 10, "hello"),
        ("hello", 5, "hello"),
        ("abcdefghij", 5, "abcd\u{2026}"),
        // Each snowman is 3 bytes and 1 char, so the split must be by character.
        (
            "\u{2603}\u{2603}\u{2603}\u{2603}\u{2603}",
            3,
            "\u{2603}\u{2603}\u{2026}",
        ),
    ] {
        let out = truncate_title(input, limit);
        assert_eq!(out, expected, "input={input} limit={limit}");
        assert!(out.chars().count() <= limit);
    }
}
#[test]
fn session_response_serializes_unread_marker() {
    use crate::session::Instance;
    let mut inst = Instance::new("t", "/tmp");
    // Read: the field is omitted from the wire (skip_serializing_if false).
    let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
    assert!(json.get("unread").is_none());
    // Unread serializes as a bare boolean the web reads directly.
    inst.unread = true;
    let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
    assert_eq!(json["unread"], serde_json::json!(true));
}

fn step(
    id: &str,
    title: &str,
    status: crate::acp::state::PlanStepStatus,
) -> crate::acp::state::PlanStep {
    crate::acp::state::PlanStep {
        id: id.into(),
        title: title.into(),
        detail: None,
        status,
    }
}

#[test]
fn plan_summary_counts_done_and_picks_the_first_non_done_step() {
    use crate::acp::state::PlanStepStatus::*;

    // (steps, expected total, completed, current step title)
    let cases: Vec<(Vec<_>, u32, u32, Option<&str>)> = vec![
        (
            vec![
                step("a", "alpha", Done),
                step("b", "beta", Done),
                step("c", "gamma", InProgress),
                step("d", "delta", Pending),
            ],
            4,
            2,
            Some("gamma"),
        ),
        // The first non-Done wins even when a later step is InProgress,
        // matching the helper's `find(..)`.
        (
            vec![
                step("a", "alpha", Done),
                step("b", "beta", Pending),
                step("c", "gamma", InProgress),
            ],
            3,
            1,
            Some("beta"),
        ),
        (
            vec![step("a", "alpha", Done), step("b", "beta", Done)],
            2,
            2,
            None,
        ),
        (vec![], 0, 0, None),
    ];

    for (steps, total, completed, current) in cases {
        let plan = crate::acp::state::Plan {
            plan_id: "p1".into(),
            version: 1,
            steps,
        };
        let s = plan_summary_from_plan(plan);
        assert_eq!(s.total, total);
        assert_eq!(s.completed, completed);
        assert_eq!(s.current_step_title.as_deref(), current);
    }
}

#[test]
fn plan_summary_truncates_a_long_current_step_title() {
    use crate::acp::state::PlanStepStatus::Pending;
    let plan = crate::acp::state::Plan {
        plan_id: "p1".into(),
        version: 1,
        steps: vec![step("a", &"x".repeat(120), Pending)],
    };
    let title = plan_summary_from_plan(plan).current_step_title.unwrap();
    assert_eq!(title.chars().count(), 80);
    assert!(title.ends_with('\u{2026}'));
}

// --- persist_session_update (the persist-first contract from #1589) ---
//
// The session-mutation PATCH handlers route every write through this helper and
// only touch memory after it returns `Ok`. Full-handler coverage is impractical
// (AppState has no test constructor), so these lock its two guarantees: a
// success durably writes, and every storage failure surfaces as `Err`.

#[test]
#[serial_test::serial]
fn rename_persistence_reports_missing_authoritative_row() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");
    let storage = Storage::new_unwatched("rename-missing").unwrap();

    let outcome = persist_rename_metadata(&storage, "missing-id", "New title", None, None).unwrap();
    assert_eq!(outcome, RenamePersistOutcome::Missing);
    assert!(
        storage.load().unwrap().is_empty(),
        "a missing row must not be synthesized by rename persistence"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn persist_session_update_writes_to_disk() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");

    let profile = "persist-success";
    let storage = Storage::new_unwatched(profile).unwrap();
    let seed = make_test_instance();
    let id = seed.id.clone();
    storage
        .update(|instances, _groups| {
            instances.push(seed.clone());
            Ok(())
        })
        .unwrap();

    let persist_id = id.clone();
    persist_session_update(
        profile.to_string(),
        "test",
        crate::file_watch::FileWatchService::noop(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                inst.base_branch_override = Some("release/x".to_string());
            }
        },
    )
    .await
    .expect("persist should succeed");

    let reloaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
    let inst = reloaded.iter().find(|i| i.id == id).unwrap();
    assert_eq!(
        inst.base_branch_override.as_deref(),
        Some("release/x"),
        "mutation must be durable on disk"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn persist_session_update_surfaces_storage_error() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");

    let profile = "persist-failure";
    // Make `sessions.json` a directory so the store's `read_to_string`
    // during `update` fails, forcing the write path to error.
    let dir = crate::session::get_profile_dir(profile).unwrap();
    std::fs::create_dir_all(dir.join("sessions.json")).unwrap();

    let result = persist_session_update(
        profile.to_string(),
        "test",
        crate::file_watch::FileWatchService::noop(),
        |_instances| {},
    )
    .await;
    assert!(result.is_err(), "a storage failure must surface as Err");
}

// Group edit (#1726): only the persisted instance's group_path changes; the
// groups Vec is left alone, since the group list is derived from instance
// group_path exactly as in create_session.
#[tokio::test]
#[serial_test::serial]
async fn group_edit_set_and_clear_round_trip_to_disk() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");

    let profile = "group-edit";
    let storage = Storage::new_unwatched(profile).unwrap();
    let seed = make_test_instance(); // seeded in "work/projects"
    let id = seed.id.clone();
    storage
        .update(|instances, _groups| {
            instances.push(seed.clone());
            Ok(())
        })
        .unwrap();

    // Move to a brand-new group.
    let set_id = id.clone();
    persist_session_update(
        profile.to_string(),
        "group update",
        crate::file_watch::FileWatchService::noop(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == set_id) {
                apply_session_group(inst, "team/alpha".to_string());
            }
        },
    )
    .await
    .expect("set should succeed");

    let reloaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
    assert_eq!(
        reloaded.iter().find(|i| i.id == id).unwrap().group_path,
        "team/alpha",
        "group must move to the new path on disk"
    );

    // Clear to ungrouped via the empty-string sentinel.
    let clear_id = id.clone();
    persist_session_update(
        profile.to_string(),
        "group update",
        crate::file_watch::FileWatchService::noop(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == clear_id) {
                apply_session_group(inst, String::new());
            }
        },
    )
    .await
    .expect("clear should succeed");

    let reloaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
    assert_eq!(
        reloaded.iter().find(|i| i.id == id).unwrap().group_path,
        "",
        "empty string must clear the group on disk"
    );
}

// --- #2066: web-API on_create hook trust + execution ---

/// Write `.agent-of-empires/config.toml` with the given `on_create` hooks
/// into a fresh project dir. Returns the dir so the caller keeps it alive.
fn project_with_on_create_hooks(commands: &[&str]) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let cfg_dir = project.path().join(".agent-of-empires");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let list = commands
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!("[hooks]\non_create = [{list}]\n"),
    )
    .unwrap();
    project
}

#[test]
#[serial_test::serial]
fn resolve_hook_plan_refuses_untrusted_repo_hooks() {
    // #2066: the web API used to skip hooks entirely. The plan must refuse an
    // untrusted repo with hooks unless trust_hooks is passed, so the caller can
    // prompt rather than silently get an un-bootstrapped worktree.
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _app_dir = crate::session::get_app_dir().expect("isolated app dir");
    let project = project_with_on_create_hooks(&["bash scripts/setup-worktree.sh"]);
    // Approval trusts the whole hooks hash, so the refusal must surface
    // every hook type, not just on_create.
    std::fs::write(
        project.path().join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"bash scripts/setup-worktree.sh\"]\non_launch = [\"npm start\"]\non_destroy = [\"rm -rf /tmp/seed\"]\n",
    )
    .unwrap();

    let err = resolve_create_hook_plan("default", project.path(), false, false)
        .expect_err("untrusted hooks must be refused");
    let needs_trust = err
        .downcast_ref::<HooksNeedTrust>()
        .expect("error must be HooksNeedTrust");
    assert_eq!(
        needs_trust.on_create,
        vec!["bash scripts/setup-worktree.sh".to_string()],
        "the refused error must carry the commands for the prompt"
    );
    assert_eq!(
        needs_trust.on_launch,
        vec!["npm start".to_string()],
        "approval also trusts on_launch, so the prompt must show it"
    );
    assert_eq!(needs_trust.on_destroy, vec!["rm -rf /tmp/seed".to_string()]);
    assert!(!needs_trust.needs_mcp_trust);
}

#[test]
#[serial_test::serial]
fn resolve_hook_plan_trusts_and_runs_with_trust_hooks() {
    // trust_hooks: true mirrors the CLI --trust-hooks flag: approve, record
    // trust, and return the commands to run.
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _app_dir = crate::session::get_app_dir().expect("isolated app dir");
    let project = project_with_on_create_hooks(&["echo hi"]);

    let plan = resolve_create_hook_plan("default", project.path(), false, true)
        .expect("trust_hooks: true must approve");
    assert_eq!(plan.on_create(), vec!["echo hi".to_string()]);
    let (hooks_hash, mcp_hash) = plan
        .trust_write
        .expect("a newly-approved repo must record trust");
    assert!(hooks_hash.is_some(), "hooks hash must be recorded");
    assert!(mcp_hash.is_none(), "no .mcp.json means no mcp hash");

    // And the recorded trust makes a later create succeed without opting in.
    crate::session::config::repo_config::trust_repo(
        project.path(),
        hooks_hash.as_deref(),
        mcp_hash.as_deref(),
    )
    .unwrap();
    let plan2 = resolve_create_hook_plan("default", project.path(), false, false)
        .expect("already-trusted hooks must run without trust_hooks");
    assert_eq!(plan2.on_create(), vec!["echo hi".to_string()]);
    assert!(
        plan2.trust_write.is_none(),
        "already-trusted repo needs no new trust record"
    );
}

/// None of these refuse a create. A scratch session has no repo config anchor,
/// so it skips the repo trust check entirely (matching the CLI scratch branch),
/// and an untrusted `.mcp.json` is gated by the supervisor at spawn rather than
/// here, so blocking creation on it would be stricter than the CLI.
#[test]
#[serial_test::serial]
fn resolve_hook_plan_refuses_nothing_without_untrusted_repo_hooks() {
    // (label, repo has untrusted on_create hooks, repo has .mcp.json, scratch)
    for (label, hooks, mcp, scratch) in [
        ("no hooks at all", false, false, false),
        ("scratch pointed at untrusted hooks", true, false, true),
        ("untrusted mcp without hooks", false, true, false),
    ] {
        let temp_home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
        let _app_dir = crate::session::get_app_dir().expect("isolated app dir");

        let hooked;
        let plain;
        let project = if hooks {
            hooked = project_with_on_create_hooks(&["echo nope"]);
            hooked.path()
        } else {
            plain = tempfile::tempdir().unwrap();
            plain.path()
        };
        if mcp {
            std::fs::write(
                project.join(".mcp.json"),
                r#"{"mcpServers": {"foo": {"command": "echo"}}}"#,
            )
            .unwrap();
        }

        let plan = resolve_create_hook_plan("default", project, scratch, false)
            .unwrap_or_else(|e| panic!("{label} must not refuse: {e:#}"));
        assert!(plan.on_create().is_empty(), "{label}");
        assert!(plan.trust_write.is_none(), "{label}");
    }
}

#[test]
#[serial_test::serial]
fn resolve_hook_plan_inherits_trust_across_worktrees() {
    // Secondary half of #2066: hook trust is keyed on the main repo, so a
    // worktree created from an already-trusted repo inherits that trust without
    // a fresh prompt, even with trust_hooks: false.
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _app_dir = crate::session::get_app_dir().expect("isolated app dir");

    let parent = tempfile::Builder::new()
        .prefix("aoe-test-")
        .tempdir()
        .unwrap();
    let root = parent.path().join("proj");
    std::fs::create_dir(&root).unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    std::fs::create_dir_all(root.join(".agent-of-empires")).unwrap();
    std::fs::write(
        root.join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"echo wt\"]\n",
    )
    .unwrap();
    std::fs::write(root.join("README.md"), "proj\n").unwrap();
    let tree_id = {
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("README.md")).unwrap();
        index.write_tree().unwrap()
    };
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    // Trust the main repo at its current hooks hash.
    let hooks = crate::session::config::repo_config::load_repo_config(&root)
        .unwrap()
        .and_then(|rc| rc.hooks())
        .unwrap();
    let hash = crate::session::config::repo_config::compute_hooks_hash(&hooks);
    crate::session::config::repo_config::trust_repo(&root, Some(&hash), None).unwrap();

    // A worktree of that repo inherits the trust.
    let main_wt = crate::git::GitWorktree::new(root.clone()).unwrap();
    let wt_path = parent.path().join("proj-wt");
    main_wt
        .create_worktree("wt-branch", &wt_path, true, None)
        .unwrap();

    let plan = resolve_create_hook_plan("default", &wt_path, false, false)
        .expect("worktree must inherit the main repo's hook trust");
    assert_eq!(plan.on_create(), vec!["echo wt".to_string()]);
    assert!(
        plan.trust_write.is_none(),
        "inherited trust needs no new record"
    );
}
#[tokio::test]
async fn list_sessions_projects_pending_approvals_only_for_running_workers() {
    use crate::acp::permissions::build_approval;
    use crate::acp::state::ToolCall;
    use crate::acp::Event;

    let mut inst = Instance::new("pending-approval", "/tmp/pending-approval");
    inst.id = "pending-approval".to_string();
    inst.view = crate::session::View::Structured;
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let approval = build_approval(
        ToolCall {
            id: "tool-1".to_string(),
            name: "shell".to_string(),
            kind: "execute".to_string(),
            args_preview: r#"{"command":"echo hello"}"#.to_string(),
            started_at: chrono::Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        },
        Vec::new(),
    );
    let nonce = approval.nonce.0.clone();
    state
        .acp_event_store
        .record(&id, 1, &Event::ApprovalRequested { approval })
        .expect("record pending approval");

    // No live worker: the durable log has an unresolved nonce, but a pending
    // nonce only exists on a running worker, so projecting it would surface a
    // phantom approval the resolver can only 404 on.
    let response = list_sessions(
        axum::extract::State(state.clone()),
        axum::extract::Query(ListSessionsQuery { state: None }),
    )
    .await;
    assert!(
        response.sessions[0].pending_approvals.is_empty(),
        "a pending approval on a non-running worker must not be projected"
    );

    // A running worker is the authoritative source; now the approval is
    // real and carries what the home dialog needs to render.
    state.acp_supervisor.test_insert_worker(&id).await;
    let response = list_sessions(
        axum::extract::State(state),
        axum::extract::Query(ListSessionsQuery { state: None }),
    )
    .await;
    assert_eq!(
        response.sessions[0].pending_approvals,
        vec![PendingApproval {
            nonce,
            tool_name: "shell".to_string(),
            target: "echo hello".to_string(),
            destructive: false,
            choice: false,
        }]
    );
}

// A worktree session's project_path is its checkout, so the override must be keyed by the main repo.
#[tokio::test]
#[serial_test::serial]
async fn list_sessions_applies_project_smart_rename_override_to_worktree_sessions() {
    let tmp_home = tempfile::tempdir().expect("tempdir HOME");
    let _home = crate::session::test_support::isolate_app_dir_at(tmp_home.path());
    let repo = tempfile::tempdir().expect("repo");
    let checkout = tempfile::tempdir().expect("worktree checkout");
    crate::session::projects::add(
        "default",
        crate::session::ProjectScope::Global,
        crate::session::Project::new(
            "demo",
            repo.path().to_string_lossy(),
            crate::session::ProjectScope::Global,
        )
        .with_overrides(crate::session::ProjectOverrides {
            smart_rename: Some(false),
            ..Default::default()
        }),
        false,
    )
    .unwrap();

    let mk = |path: &std::path::Path| {
        let mut inst = Instance::new("Vikings", path.to_str().unwrap());
        inst.tool = "claude".to_string();
        inst.source_profile = "default".to_string();
        inst.view = crate::session::View::Structured;
        inst
    };
    let mut in_worktree = mk(checkout.path());
    in_worktree.worktree_info = Some(worktree("feat", repo.path().to_string_lossy(), None));
    // Same checkout without the worktree link: unregistered, so it stays eligible.
    let unregistered = mk(checkout.path());

    let state = crate::server::test_support::build_test_app_state(vec![
        mk(repo.path()),
        in_worktree,
        unregistered,
    ]);
    let resp = list_sessions(
        axum::extract::State(state),
        axum::extract::Query(ListSessionsQuery { state: None }),
    )
    .await
    .into_response();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let states: Vec<&str> = envelope["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["smart_rename"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["inactive", "inactive", "pending"]);
}

/// #4084 review: deleting one session of a shared managed worktree, through
/// either delete endpoint, removes that record but keeps the worktree and
/// branch a surviving session still works in.
#[tokio::test]
#[serial_test::serial]
async fn permanent_delete_keeps_a_worktree_a_surviving_session_uses() {
    use axum::body::to_bytes;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    for workspace_endpoint in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("home"));
        let main_repo = tmp.path().join("main");
        let checkout = tmp.path().join("shared");
        std::fs::create_dir_all(&main_repo).unwrap();
        git(&main_repo, &["init", "-b", "main"]);
        git(&main_repo, &["commit", "--allow-empty", "-m", "init"]);
        git(
            &main_repo,
            &["worktree", "add", "-b", "feat", checkout.to_str().unwrap()],
        );

        let profile = "shared-worktree-4084";
        let mk = |title: &str, managed: bool| {
            let mut inst = Instance::new(title, checkout.to_str().unwrap());
            inst.source_profile = profile.to_string();
            let mut info = worktree("feat", main_repo.to_string_lossy(), None);
            info.managed_by_aoe = managed;
            inst.worktree_info = Some(info);
            inst
        };
        let owner = mk("owner", true);
        let survivor = mk("survivor", false);
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|instances, _groups| {
                instances.extend([owner.clone(), survivor.clone()]);
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![
            owner.clone(),
            survivor.clone(),
        ]);

        let resp = if workspace_endpoint {
            delete_workspace(
                State(state.clone()),
                Some(Json(DeleteWorkspaceBody {
                    session_ids: vec![owner.id.clone()],
                    delete_worktree: true,
                    delete_branch: true,
                    ..Default::default()
                })),
            )
            .await
            .into_response()
        } else {
            delete_session(
                State(state.clone()),
                Path(owner.id.clone()),
                Some(Json(DeleteSessionBody {
                    delete_worktree: true,
                    delete_branch: true,
                    ..Default::default()
                })),
            )
            .await
            .into_response()
        };
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "endpoint {workspace_endpoint}"
        );
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["messages"].to_string().contains("another session"),
            "the kept worktree must be reported: {body}"
        );

        assert!(
            checkout.join(".git").exists(),
            "shared worktree was removed"
        );
        let branches = std::process::Command::new("git")
            .args(["branch", "--list", "feat"])
            .current_dir(&main_repo)
            .output()
            .unwrap();
        assert!(!branches.stdout.is_empty(), "shared branch was deleted");
        let stored: Vec<String> = storage.load().unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(stored, vec![survivor.id.clone()]);
        assert!(state
            .instances
            .read()
            .await
            .iter()
            .all(|i| i.id != owner.id));
    }
}
