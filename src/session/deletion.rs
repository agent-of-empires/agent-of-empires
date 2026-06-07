//! Shared session deletion logic used by CLI, TUI, and web server.

use std::path::{Path, PathBuf};

use crate::containers::DockerContainer;
use crate::git::cleanup::remove_managed_worktree;
use crate::git::GitWorktree;
use crate::session::repo_config;
use crate::session::Instance;

pub struct DeletionRequest {
    pub session_id: String,
    pub instance: Instance,
    pub delete_worktree: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
    pub force_delete: bool,
    /// When `true`, on_destroy hooks run detached from the controlling
    /// terminal (TUI/web). When `false`, hooks inherit stdin/stdout so
    /// interactive prompts work (CLI).
    pub detach_hooks: bool,
    /// When `true` AND `instance.scratch` is `true`, the scratch directory
    /// is left on disk instead of being removed. The kept path is logged at
    /// info level and surfaced in the deletion result's messages. Has no
    /// effect on non-scratch sessions.
    pub keep_scratch: bool,
}

#[derive(Debug)]
pub struct DeletionResult {
    pub session_id: String,
    pub success: bool,
    pub messages: Vec<String>,
    pub errors: Vec<String>,
}

pub fn perform_deletion(request: &DeletionRequest) -> DeletionResult {
    let mut errors = Vec::new();
    let mut messages = Vec::new();

    tracing::debug!(target: "session.delete",
        session_id = %request.session_id,
        title = %request.instance.title,
        delete_worktree = request.delete_worktree,
        delete_branch = request.delete_branch,
        delete_sandbox = request.delete_sandbox,
        force_delete = request.force_delete,
        worktree_branch = request.instance.worktree_info.as_ref().map(|w| w.branch.as_str()).unwrap_or("<none>"),
        worktree_managed = request.instance.worktree_info.as_ref().map(|w| w.managed_by_aoe).unwrap_or(false),
        worktree_main_repo = request.instance.worktree_info.as_ref().map(|w| w.main_repo_path.as_str()).unwrap_or("<none>"),
        workspace_repos = request.instance.workspace_info.as_ref().map(|w| w.repos.len()).unwrap_or(0),
        "perform_deletion: starting"
    );

    // Stage 1: on_destroy hooks. The container and worktree are still
    // alive here so teardown commands have full access.
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "on_destroy_hooks", "perform_deletion: stage");
    run_on_destroy_hooks(&request.instance, request.detach_hooks);

    // Stage 2: sever the live agent BEFORE we touch the working tree it
    // may be writing to. Killing the tmux session terminates the user's
    // `docker exec`; for sandboxed sessions we also wipe root-owned
    // worktree contents from INSIDE the container so the host's
    // `git worktree remove` below doesn't fight permissions or a still-
    // running bind mount. Previously the order was reversed (worktree
    // first, container second, tmux last), which raced the in-container
    // agent and produced flaky deletions on Docker + worktree sessions.
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "tmux_kill", "perform_deletion: stage");
    request.instance.kill_all_tmux_sessions();

    let is_sandboxed = request
        .instance
        .sandbox_info
        .as_ref()
        .is_some_and(|s| s.enabled);

    // Host-side dirty check. The in-container preclean below destroys
    // worktree contents unconditionally, which would silently violate
    // the `force_delete=false` safety contract for users with untracked
    // or modified files. Walk every managed worktree we'd touch and
    // collect the dirty ones; preclean is skipped if anything is dirty
    // (the `find -delete` runs at the workspace root and can't easily
    // skip subpaths), and host-side worktree removal is skipped per
    // path that's dirty. Container, branch, and hook stages still run
    // per the user's flags.
    let mut skip_worktree_paths: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    if request.delete_worktree && !request.force_delete {
        if let Some(wt_info) = &request.instance.worktree_info {
            if wt_info.managed_by_aoe {
                let path = PathBuf::from(&request.instance.project_path);
                if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                    tracing::debug!(target: "session.delete",
                        session_id = %request.session_id,
                        path = %path.display(),
                        "perform_deletion: dirty worktree, skipping preclean + host remove"
                    );
                    errors.push(format!("Worktree: {}", msg));
                    skip_worktree_paths.insert(path);
                }
            }
        }
        if let Some(ws_info) = &request.instance.workspace_info {
            if ws_info.cleanup_on_delete {
                for repo in &ws_info.repos {
                    if repo.managed_by_aoe {
                        let path = PathBuf::from(&repo.worktree_path);
                        if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                            tracing::debug!(target: "session.delete",
                                session_id = %request.session_id,
                                repo = %repo.name,
                                path = %path.display(),
                                "perform_deletion: dirty workspace repo, skipping preclean + host remove"
                            );
                            errors.push(format!("Workspace ({}): {}", repo.name, msg));
                            skip_worktree_paths.insert(path);
                        }
                    }
                }
            }
        }
    }
    let any_dirty = !skip_worktree_paths.is_empty();

    if request.delete_worktree && is_sandboxed && !any_dirty {
        tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "sandbox_worktree_preclean", "perform_deletion: stage");
        // Best-effort. The container's workdir is the session's main
        // worktree (or, for workspace sessions, the workspace root that
        // contains every per-repo worktree). `find . -delete` from
        // there recursively wipes everything we care about under the
        // bind mount. If this fails, the host-side removal below will
        // surface a permission error and exit cleanly. We do NOT walk
        // workspace_info.repos here: their container mount paths are
        // computed relative to the common ancestor of all repos
        // (see compute_workspace_volume_paths) and don't necessarily
        // match `/workspace/{repo.name}`, and the workspace-root walk
        // already covers their contents.
        let _ = crate::git::cleanup::cleanup_sandbox_worktree(&request.instance);
    }

    // Stage 3: container removal. Releases the bind mount on the
    // worktree so the host can finish cleanup without racing in-
    // container processes.
    if request.delete_sandbox && is_sandboxed {
        tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "container_remove", "perform_deletion: stage");
        let container = DockerContainer::from_session_id(&request.instance.id);
        if container.exists().unwrap_or(false) {
            if let Err(e) = container.remove(true) {
                errors.push(format!("Container: {}", e));
            } else {
                messages.push("Container removed".to_string());
            }
        }
        // Remove named ignore volumes even if the container is already gone — volumes created
        // with volume_ignores_strategy = "named" outlive the container and need explicit cleanup.
        container.remove_named_ignore_volumes(&request.instance.id);
    }

    // Stage 4: worktree cleanup. Container is gone, agent is gone, no
    // bind mount holds the directory open, and (for sandboxed sessions)
    // the preclean above wiped any root-owned files. Must happen
    // before branch deletion since the worktree is using the branch.
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "worktree_remove", "perform_deletion: stage");
    let branch_to_delete = if request.delete_branch {
        request
            .instance
            .worktree_info
            .as_ref()
            .filter(|wt| wt.managed_by_aoe)
            .map(|wt| (wt.branch.clone(), PathBuf::from(&wt.main_repo_path)))
    } else {
        None
    };
    if let Some((b, r)) = branch_to_delete.as_ref() {
        tracing::debug!(target: "session.delete", branch = %b, main_repo = %r.display(), "perform_deletion: branch_to_delete resolved");
    }

    if request.delete_worktree {
        if let Some(wt_info) = &request.instance.worktree_info {
            if wt_info.managed_by_aoe {
                let worktree_path = PathBuf::from(&request.instance.project_path);
                if !skip_worktree_paths.contains(&worktree_path) {
                    let main_repo = PathBuf::from(&wt_info.main_repo_path);

                    match GitWorktree::new(main_repo.clone()) {
                        Ok(git_wt) => {
                            if let Err(errs) = remove_managed_worktree(
                                &git_wt,
                                &worktree_path,
                                &main_repo,
                                &request.instance,
                                request.force_delete,
                                request.delete_sandbox,
                            ) {
                                errors.extend(errs);
                            } else {
                                messages.push("Worktree removed".to_string());
                            }
                        }
                        Err(e) => {
                            errors.push(format!("Worktree: {}", e));
                        }
                    }
                }
            }
        }
    }

    // Workspace cleanup (if user opted to delete worktrees and instance has workspace_info)
    if request.delete_worktree {
        if let Some(ws_info) = &request.instance.workspace_info {
            if ws_info.cleanup_on_delete {
                for repo in &ws_info.repos {
                    if repo.managed_by_aoe {
                        let worktree_path = PathBuf::from(&repo.worktree_path);
                        if skip_worktree_paths.contains(&worktree_path) {
                            continue;
                        }
                        let main_repo = PathBuf::from(&repo.main_repo_path);

                        match GitWorktree::new(main_repo.clone()) {
                            Ok(git_wt) => {
                                if let Err(errs) = remove_managed_worktree(
                                    &git_wt,
                                    &worktree_path,
                                    &main_repo,
                                    &request.instance,
                                    request.force_delete,
                                    request.delete_sandbox,
                                ) {
                                    errors.extend(
                                        errs.into_iter()
                                            .map(|e| format!("Workspace ({}): {}", repo.name, e)),
                                    );
                                } else {
                                    messages.push(format!(
                                        "Workspace ({}) worktree removed",
                                        repo.name
                                    ));
                                }
                            }
                            Err(e) => {
                                errors.push(format!("Workspace ({}): {}", repo.name, e));
                            }
                        }
                    }
                }
                // Remove workspace parent directory only when every repo
                // under it cleared the dirty check; otherwise we'd nuke
                // the user's uncommitted changes through the back door.
                if !any_dirty {
                    let ws_path = PathBuf::from(&ws_info.workspace_dir);
                    if ws_path.exists() {
                        if let Err(e) = std::fs::remove_dir_all(&ws_path) {
                            errors.push(format!("Workspace dir: {}", e));
                        } else {
                            messages.push("Workspace directory removed".to_string());
                        }
                    }
                }
            }
        }
    }

    // Stage 5: branch cleanup (if user opted to delete it and worktree
    // was successfully removed).
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "branch_delete", "perform_deletion: stage");
    if let Some((branch, main_repo)) = branch_to_delete {
        let worktree_ok =
            !request.delete_worktree || !errors.iter().any(|e| e.starts_with("Worktree:"));
        tracing::debug!(target: "session.delete", branch = %branch, main_repo = %main_repo.display(), worktree_ok, "perform_deletion: attempting branch deletion");
        if worktree_ok {
            match GitWorktree::new(main_repo.clone()) {
                Ok(git_wt) => {
                    if let Err(e) = git_wt.delete_branch(&branch) {
                        tracing::debug!(target: "session.delete", branch = %branch, error = %e, "perform_deletion: delete_branch returned error");
                        errors.push(format!("Branch: {}", e));
                    } else {
                        messages.push(format!("Branch '{}' deleted", branch));
                    }
                }
                Err(e) => {
                    tracing::debug!(target: "session.delete", main_repo = %main_repo.display(), error = %e, "perform_deletion: GitWorktree::new failed");
                    errors.push(format!("Branch: {}", e));
                }
            }
        } else {
            tracing::debug!(target: "session.delete",
                "perform_deletion: skipping branch deletion (worktree removal had errors)"
            );
        }
    }

    // Branch cleanup for workspace repos
    if request.delete_branch {
        if let Some(ws_info) = &request.instance.workspace_info {
            let worktree_ok =
                !request.delete_worktree || !errors.iter().any(|e| e.starts_with("Workspace ("));
            if worktree_ok {
                for repo in &ws_info.repos {
                    if repo.managed_by_aoe {
                        let main_repo = PathBuf::from(&repo.main_repo_path);
                        if let Ok(git_wt) = GitWorktree::new(main_repo) {
                            if let Err(e) = git_wt.delete_branch(&repo.branch) {
                                errors.push(format!("Branch ({}): {}", repo.name, e));
                            } else {
                                messages.push(format!(
                                    "Branch '{}' ({}) deleted",
                                    repo.branch, repo.name
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // Scratch directory cleanup. Runs unconditionally for scratch sessions
    // regardless of `request.delete_worktree`, since the scratch directory
    // is the entire reason the session has any on-disk state. Skipped when
    // the user opted in to keeping the directory via `request.keep_scratch`.
    // Guarded by `is_scratch_path` to refuse to follow a tampered or
    // corrupted `project_path` (e.g. JSON edited by hand to claim
    // `scratch: true` while pointing at `/etc`).
    if request.instance.scratch {
        let path = PathBuf::from(&request.instance.project_path);
        // keep_scratch + tampered project_path used to surface
        // "Scratch directory kept at: /etc" which implied AoE was
        // intentionally leaving a path it never owned. Gate the
        // keep-scratch message on the same `is_scratch_path` guard
        // the remove branch uses so the message only fires for
        // paths AoE actually controls.
        let guard_ok = path.exists() && super::scratch::is_scratch_path(&path);
        if request.keep_scratch && guard_ok {
            tracing::info!(
                target: "session.delete",
                session_id = %request.session_id,
                path = %path.display(),
                "keep-scratch opted in; leaving scratch directory on disk"
            );
            messages.push(format!("Scratch directory kept at: {}", path.display()));
        } else if request.keep_scratch {
            // Tampered or missing path with keep_scratch on: still nothing
            // to remove, but we cannot claim ownership of the path either.
            tracing::warn!(
                target: "session.delete",
                session_id = %request.session_id,
                path = %path.display(),
                "keep-scratch requested but project_path failed the guard or is missing"
            );
        } else if !path.exists() {
            // Already gone (user removed it manually, FS hiccup, prior
            // partial cleanup). Nothing to do, and we must not reach the
            // guard branch: a canonicalized `is_scratch_path` rejects
            // missing paths and would otherwise surface this as a guard
            // refusal even though it is not a tampering case.
            tracing::debug!(
                target: "session.delete",
                session_id = %request.session_id,
                path = %path.display(),
                "scratch dir already gone before deletion ran"
            );
        } else if super::scratch::is_scratch_path(&path) {
            tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "scratch_remove", "perform_deletion: stage");
            match std::fs::remove_dir_all(&path) {
                Ok(()) => messages.push("Scratch directory removed".to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(target: "session.delete",
                        session_id = %request.session_id,
                        path = %path.display(),
                        "perform_deletion: scratch dir already gone, treating as success"
                    );
                }
                Err(e) => {
                    errors.push(format!("Scratch directory: {}", e));
                }
            }
        } else {
            // Tampered `project_path` (e.g. JSON edited by hand to claim
            // `scratch: true` while pointing outside the scratch root)
            // is the only path that reaches this branch in normal use.
            // The session record will still be deleted, so callers need
            // a visible signal that on-disk cleanup was skipped.
            tracing::warn!(
                target: "session.delete",
                session_id = %request.session_id,
                path = %path.display(),
                "scratch flag set but project_path failed the guard; refusing to remove"
            );
            errors.push(format!(
                "Scratch directory: refused to remove {} (path failed scratch guard)",
                path.display()
            ));
        }
    }

    // Stage 6: hook status cleanup
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "hook_status_cleanup", "perform_deletion: stage");
    crate::hooks::cleanup_hook_status_dir(&request.instance.id);

    if !errors.is_empty() {
        tracing::debug!(target: "session.delete",
            session_id = %request.session_id,
            error_count = errors.len(),
            errors = ?errors,
            "perform_deletion: completed with errors"
        );
    } else {
        tracing::debug!(target: "session.delete", session_id = %request.session_id, "perform_deletion: completed successfully");
    }

    DeletionResult {
        session_id: request.session_id.clone(),
        success: errors.is_empty(),
        messages,
        errors,
    }
}

/// Run on_destroy hooks for an instance. Uses best-effort execution so all
/// hooks are attempted even if some fail. Failures are logged as warnings
/// and never prevent deletion.
///
/// Global/profile hooks are implicitly trusted. Repo-level hooks go through
/// the same trust verification as on_launch: if the hooks hash has changed
/// since the user last approved, repo hooks are silently skipped.
fn run_on_destroy_hooks(instance: &Instance, detach: bool) {
    let profile = crate::session::config::effective_profile(&instance.source_profile);

    let project_path = Path::new(&instance.project_path);

    // Start with global+profile on_destroy hooks (implicitly trusted).
    let mut resolved_on_destroy = crate::session::profile_config::resolve_config_or_warn(&profile)
        .hooks
        .on_destroy;

    // Check if repo has trusted hooks that override. Only the hooks surface
    // matters here; untrusted project MCP must not suppress trusted hooks.
    match repo_config::check_repo_trust(project_path) {
        Ok(trust) if trust.hooks.needs_trust() => {
            tracing::warn!(target: "session.delete",
                "Repo hooks changed since last trust approval; skipping repo on_destroy hooks"
            );
        }
        Ok(trust) => {
            if let Some(hooks) = trust.hooks.trusted() {
                if !hooks.on_destroy.is_empty() {
                    resolved_on_destroy = hooks.on_destroy;
                }
            }
        }
        Err(_) => {}
    }

    if resolved_on_destroy.is_empty() {
        return;
    }

    tracing::info!(target: "session.delete", "Running on_destroy hooks for session {}", instance.id);

    let is_sandboxed = instance.sandbox_info.as_ref().is_some_and(|s| s.enabled);
    let hook_env = repo_config::lifecycle_env_vars(instance);

    // The caller controls detachment: TUI/web pass detach=true to avoid
    // corrupting the rendered UI (see issue #901); CLI passes detach=false
    // so interactive prompts work.
    let errors = if is_sandboxed {
        if let Some(ref sandbox) = instance.sandbox_info {
            let workdir = instance.container_workdir();
            repo_config::execute_hooks_in_container_best_effort(
                &resolved_on_destroy,
                &sandbox.container_name,
                &workdir,
                detach,
                &hook_env,
            )
        } else {
            vec![]
        }
    } else {
        repo_config::execute_hooks_best_effort(
            &resolved_on_destroy,
            project_path,
            detach,
            &hook_env,
        )
    };

    if !errors.is_empty() {
        tracing::warn!(target: "session.delete",
            "on_destroy hooks had {} failure(s) for session {}",
            errors.len(),
            instance.id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_instance() -> Instance {
        Instance::new("Test Session", "/tmp/test-project")
    }

    #[test]
    fn test_deletion_result_success_when_no_worktree_or_sandbox() {
        let instance = create_test_instance();
        let request = DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };

        let result = perform_deletion(&request);

        assert!(result.success);
        assert!(result.errors.is_empty());
        assert_eq!(result.session_id, request.session_id);
    }

    #[test]
    fn test_deletion_result_success_even_with_delete_worktree_flag_when_no_worktree() {
        let instance = create_test_instance();
        let request = DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: true,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };

        let result = perform_deletion(&request);

        assert!(result.success);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_deletion_request_preserves_session_id() {
        let instance = create_test_instance();
        let custom_id = "custom-session-id-123".to_string();

        let request = DeletionRequest {
            session_id: custom_id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };

        let result = perform_deletion(&request);
        assert_eq!(result.session_id, custom_id);
    }

    mod ordering {
        use super::*;
        use crate::session::SandboxInfo;
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing::subscriber::with_default;
        use tracing::Subscriber;
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::registry::LookupSpan;
        use tracing_subscriber::Layer;

        /// tracing Layer that captures the `stage` field value of every
        /// `perform_deletion: stage` event in order of emission.
        struct StageRecorder {
            stages: Arc<Mutex<Vec<String>>>,
        }

        impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for StageRecorder {
            fn register_callsite(
                &self,
                _meta: &'static tracing::Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                tracing::subscriber::Interest::always()
            }

            fn enabled(&self, _meta: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
                true
            }

            fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
                Some(tracing::level_filters::LevelFilter::TRACE)
            }

            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                struct V {
                    msg: Option<String>,
                    stage: Option<String>,
                }
                impl Visit for V {
                    fn record_str(&mut self, field: &Field, value: &str) {
                        match field.name() {
                            "stage" => self.stage = Some(value.to_string()),
                            "message" => self.msg = Some(value.to_string()),
                            _ => {}
                        }
                    }
                    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                        let rendered = format!("{:?}", value);
                        let unquoted = rendered.trim_matches('"').to_string();
                        match field.name() {
                            "stage" => self.stage = Some(unquoted),
                            "message" => self.msg = Some(unquoted),
                            _ => {}
                        }
                    }
                }
                let mut v = V {
                    msg: None,
                    stage: None,
                };
                event.record(&mut v);
                if v.msg.as_deref() == Some("perform_deletion: stage") {
                    if let Some(stage) = v.stage {
                        self.stages.lock().unwrap().push(stage);
                    }
                }
            }
        }

        fn run_with_capture<F: Fn()>(f: F) -> Vec<String> {
            let stages = Arc::new(Mutex::new(Vec::new()));
            let layer = StageRecorder {
                stages: Arc::clone(&stages),
            };
            let subscriber = tracing_subscriber::registry().with(layer);
            with_default(subscriber, || {
                // Tracing's per-callsite `Interest` is cached globally on first
                // hit. `rebuild_interest_cache()` only re-evaluates callsites
                // that are *already* registered, so any stage callsite not yet
                // hit at this point can still lose a registration race to a
                // parallel test running `perform_deletion` without a subscriber
                // (the sibling tests at lines 335/354/372 do exactly this).
                // If they win, the callsite is cached as `Interest::never()`
                // and our subscriber never sees that one event, while the
                // other stages still come through. The fix is a two-pass run:
                //   1. Warmup pass: invoke f() once while we're the default,
                //      forcing the callsites to register under our subscriber
                //      (or be re-evaluated to Always if already registered).
                //   2. Clear captured stages, rebuild interest cache to fix up
                //      anything that lost a race during warmup, then run f()
                //      again as the measured pass.
                f();
                stages.lock().unwrap().clear();
                tracing::callsite::rebuild_interest_cache();
                f();
            });
            let g = stages.lock().unwrap();
            g.clone()
        }

        /// Index of the first occurrence of `needle` in `stages`. Panics
        /// with a descriptive message if absent so test failures point
        /// at the missing stage instead of an inscrutable `unwrap`.
        fn idx(stages: &[String], needle: &str) -> usize {
            stages
                .iter()
                .position(|s| s == needle)
                .unwrap_or_else(|| panic!("stage {:?} missing from {:?}", needle, stages))
        }

        /// Regression: sandboxed + worktree deletion must drop the
        /// container BEFORE touching the worktree directory. The old
        /// order (worktree first) raced the still-running in-container
        /// agent and produced flaky permission errors and dirty-tree
        /// failures.
        #[test]
        fn sandboxed_with_worktree_kills_tmux_and_container_before_worktree() {
            // Synthesize an instance with both worktree_info and an
            // (enabled) sandbox_info pointing at a non-existent
            // container. The container ops will no-op (container does
            // not exist), but the *order* of stage events is still
            // emitted, and that's what we're testing.
            let mut instance = Instance::new("Test", "/tmp/aoe-deletion-test-nonexistent");
            instance.sandbox_info = Some(SandboxInfo {
                enabled: true,
                container_id: None,
                image: "alpine".to_string(),
                container_name: "aoe-sandbox-doesnotexist".to_string(),
                extra_env: None,
                custom_instruction: None,
            });

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: true,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let stages = run_with_capture(|| {
                let _ = perform_deletion(&request);
            });

            // tmux_kill must precede container_remove must precede
            // worktree_remove must precede branch_delete.
            let i_tmux = idx(&stages, "tmux_kill");
            let i_container = idx(&stages, "container_remove");
            let i_worktree = idx(&stages, "worktree_remove");
            let i_branch = idx(&stages, "branch_delete");

            assert!(
                i_tmux < i_container,
                "tmux must be killed before container removal: stages={:?}",
                stages
            );
            assert!(
                i_container < i_worktree,
                "container must be removed before worktree cleanup: stages={:?}",
                stages
            );
            assert!(
                i_worktree < i_branch,
                "worktree must be cleaned before branch delete: stages={:?}",
                stages
            );

            // Sandboxed + delete_worktree: we should also see the
            // in-container preclean stage between tmux_kill and
            // container_remove.
            let i_preclean = idx(&stages, "sandbox_worktree_preclean");
            assert!(
                i_tmux < i_preclean && i_preclean < i_container,
                "preclean must run after tmux kill and before container remove: stages={:?}",
                stages
            );
        }

        /// End-to-end-on-disk: build a real git repo + worktree on the
        /// filesystem (no docker, no tmux session), call
        /// `perform_deletion(delete_worktree=true)`, and verify the
        /// worktree directory and `.git/worktrees/<name>` admin entry
        /// are gone afterwards. This is the closest we can get to an
        /// e2e test for the worktree-delete path without a real
        /// container runtime.
        #[test]
        fn e2e_real_worktree_is_removed_on_disk() {
            let tmp = tempfile::TempDir::new().unwrap();
            let main_repo = tmp.path().join("main");
            let worktree_path = tmp.path().join("worktree");
            std::fs::create_dir(&main_repo).unwrap();

            // init main repo with one commit so branches can be created
            let repo = git2::Repository::init(&main_repo).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = {
                let mut index = repo.index().unwrap();
                index.write_tree().unwrap()
            };
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();

            // create the worktree on a new branch via real `git` so the
            // admin files match what aoe creates in production
            let status = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    "feature/delete-me",
                    worktree_path.to_str().unwrap(),
                ])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "git worktree add failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            assert!(worktree_path.exists());
            assert!(
                main_repo.join(".git/worktrees/worktree").exists(),
                "worktree admin dir should exist before deletion"
            );

            // construct an Instance matching what builder would produce
            let mut instance = Instance::new("Test", worktree_path.to_str().unwrap());
            instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "feature/delete-me".to_string(),
                main_repo_path: main_repo.to_string_lossy().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let result = perform_deletion(&request);
            assert!(
                result.success,
                "perform_deletion failed: {:?}",
                result.errors
            );

            // worktree directory and its admin entry must be gone
            assert!(
                !worktree_path.exists(),
                "worktree dir should be removed after delete"
            );
            assert!(
                !main_repo.join(".git/worktrees/worktree").exists(),
                "worktree admin dir should be pruned after delete"
            );

            // branch should be deleted
            let branches_out = std::process::Command::new("git")
                .args(["branch", "--list", "feature/delete-me"])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&branches_out.stdout)
                    .trim()
                    .is_empty(),
                "branch should be deleted: stdout={}",
                String::from_utf8_lossy(&branches_out.stdout)
            );
        }

        /// Race-condition repro: the agent left untracked files in the
        /// worktree (this is what triggered the original
        /// "fatal: '<path>' contains modified or untracked files"
        /// failures). With `force_delete=true` the worktree must be
        /// removed cleanly even with untracked content, which is the
        /// fallback path the TUI takes when the user picks "force
        /// delete" after a normal delete failed.
        #[test]
        fn e2e_real_worktree_with_untracked_files_force_removed() {
            let tmp = tempfile::TempDir::new().unwrap();
            let main_repo = tmp.path().join("main");
            let worktree_path = tmp.path().join("worktree");
            std::fs::create_dir(&main_repo).unwrap();

            let repo = git2::Repository::init(&main_repo).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();

            let status = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    "feature/race-repro",
                    worktree_path.to_str().unwrap(),
                ])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(status.status.success());

            // simulate what the in-container agent leaves behind: an
            // untracked log file, plus a modified-but-not-committed
            // file. Without force_delete, `git worktree remove` refuses
            // to delete a dirty tree.
            std::fs::write(worktree_path.join("agent.log"), "scratch").unwrap();
            std::fs::write(worktree_path.join("debug.json"), "{\"k\":1}").unwrap();

            let mut instance = Instance::new("Test", worktree_path.to_str().unwrap());
            instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "feature/race-repro".to_string(),
                main_repo_path: main_repo.to_string_lossy().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });

            // First: without force, deletion must fail and leave the
            // worktree intact so the user can decide.
            let req_no_force = DeletionRequest {
                session_id: instance.id.clone(),
                instance: instance.clone(),
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&req_no_force);
            assert!(
                !result.success,
                "dirty worktree must NOT be deleted without --force"
            );
            assert!(
                worktree_path.exists(),
                "dirty worktree must still exist after failed delete"
            );

            // Now retry with force: must succeed and clean everything.
            let req_force = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&req_force);
            assert!(
                result.success,
                "force delete should succeed: {:?}",
                result.errors
            );
            assert!(!worktree_path.exists());
            assert!(!main_repo.join(".git/worktrees/worktree").exists());
        }

        /// Builds a real on-disk worktree on a fresh branch, then
        /// returns a tuple of `(_tmp, main_repo, worktree_path, instance)`
        /// where the instance has `worktree_info` + `sandbox_info`
        /// pointing at a non-existent container (so container ops are
        /// no-ops in the test). The caller can drop untracked files
        /// into `worktree_path` before invoking `perform_deletion`.
        fn build_sandboxed_worktree(
            branch: &str,
        ) -> (
            tempfile::TempDir,
            std::path::PathBuf,
            std::path::PathBuf,
            Instance,
        ) {
            let tmp = tempfile::TempDir::new().unwrap();
            let main_repo = tmp.path().join("main");
            let worktree_path = tmp.path().join("worktree");
            std::fs::create_dir(&main_repo).unwrap();

            let repo = git2::Repository::init(&main_repo).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();

            let status = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    branch,
                    worktree_path.to_str().unwrap(),
                ])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "git worktree add failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );

            let mut instance = Instance::new("Test", worktree_path.to_str().unwrap());
            instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: branch.to_string(),
                main_repo_path: main_repo.to_string_lossy().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });
            instance.sandbox_info = Some(SandboxInfo {
                enabled: true,
                container_id: None,
                image: "alpine".to_string(),
                container_name: "aoe-dirty-test-doesnotexist".to_string(),
                extra_env: None,
                custom_instruction: None,
            });

            (tmp, main_repo, worktree_path, instance)
        }

        /// Regression for the silent-data-destruction bug introduced by
        /// the preclean stage (#1023): if the user has uncommitted
        /// changes in a sandboxed worktree and asks for a normal (non-
        /// force) delete, the in-container `find . -delete` would
        /// previously wipe those changes before any dirty check ever
        /// ran. With the host-side dirty check, preclean must be
        /// skipped, the worktree must survive, and the error must
        /// describe what's dirty so the user can choose to force.
        #[test]
        fn sandboxed_with_dirty_worktree_skips_preclean_and_preserves_changes() {
            let (_tmp, main_repo, worktree_path, instance) =
                build_sandboxed_worktree("feature/dirty-no-force");

            std::fs::write(worktree_path.join("uncommitted.log"), "important").unwrap();

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: true,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            // Stage assertions: preclean must not run when dirty.
            let stages = run_with_capture(|| {
                let _ = perform_deletion(&request);
            });
            assert!(
                !stages.iter().any(|s| s == "sandbox_worktree_preclean"),
                "preclean must be skipped when worktree is dirty: stages={:?}",
                stages
            );

            // After run_with_capture, deletion must have left the
            // worktree intact on both passes. Run once more to capture
            // the result + error message.
            let result = perform_deletion(&request);
            assert!(
                !result.success,
                "dirty worktree must not be deleted without --force"
            );
            assert!(
                !result.errors.is_empty(),
                "dirty deletion should surface errors"
            );
            let err = result.errors.join("; ");
            assert!(
                err.contains("modified or untracked"),
                "error should describe dirty state: {}",
                err
            );
            assert!(
                err.contains("uncommitted.log"),
                "error should list the dirty path: {}",
                err
            );
            assert!(
                worktree_path.exists(),
                "worktree dir must survive a refused dirty delete"
            );
            assert!(
                worktree_path.join("uncommitted.log").exists(),
                "uncommitted user data must survive a refused dirty delete"
            );
            assert!(
                main_repo.join(".git/worktrees/worktree").exists(),
                "worktree admin entry must still be present"
            );
        }

        /// Counterpart: with `force_delete=true` the user has explicitly
        /// opted into losing uncommitted changes, so preclean runs and
        /// the worktree is removed. Preclean is a docker no-op in this
        /// test (container does not exist), so the host-side path uses
        /// `git worktree remove --force` which correctly handles the
        /// untracked file.
        #[test]
        fn sandboxed_with_dirty_worktree_force_runs_preclean_and_removes() {
            let (_tmp, main_repo, worktree_path, instance) =
                build_sandboxed_worktree("feature/dirty-force");

            std::fs::write(worktree_path.join("uncommitted.log"), "scratch").unwrap();

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: true,
                keep_scratch: false,
            };

            let stages = run_with_capture(|| {
                let _ = perform_deletion(&request);
            });
            assert!(
                stages.iter().any(|s| s == "sandbox_worktree_preclean"),
                "preclean must run when force_delete=true: stages={:?}",
                stages
            );

            // run_with_capture invokes perform_deletion twice; the
            // first pass already removed the worktree, so the second
            // pass is a no-op. Both succeed.
            assert!(
                !worktree_path.exists(),
                "force delete must remove the worktree dir"
            );
            assert!(
                !main_repo.join(".git/worktrees/worktree").exists(),
                "force delete must prune the admin entry"
            );
        }

        /// Non-sandboxed deletion: no preclean stage is emitted, but
        /// tmux still gets killed before worktree work.
        #[test]
        fn unsandboxed_kills_tmux_before_worktree() {
            let instance = Instance::new("Test", "/tmp/aoe-deletion-test-nonexistent");
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let stages = run_with_capture(|| {
                let _ = perform_deletion(&request);
            });

            assert!(
                idx(&stages, "tmux_kill") < idx(&stages, "worktree_remove"),
                "tmux must be killed before worktree cleanup: stages={:?}",
                stages
            );
            assert!(
                !stages.iter().any(|s| s == "sandbox_worktree_preclean"),
                "unsandboxed deletion must not emit sandbox preclean stage: stages={:?}",
                stages
            );
        }
    }

    mod scratch_cleanup {
        use super::*;
        use serial_test::serial;
        use std::fs;

        fn isolate_app_dir() -> tempfile::TempDir {
            let tmp = tempfile::tempdir().expect("create temp home for scratch deletion tests");
            std::env::set_var("HOME", tmp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
            tmp
        }

        fn scratch_instance() -> (Instance, PathBuf) {
            let id = format!("delete-test-{}", uuid::Uuid::new_v4());
            let dir = crate::session::scratch::provision_scratch_dir(&id)
                .expect("provision scratch dir for test");
            let mut instance = Instance::new("Scratch", dir.to_str().unwrap());
            instance.scratch = true;
            (instance, dir)
        }

        #[test]
        #[serial]
        fn scratch_session_removes_dir() {
            let _tmp = isolate_app_dir();
            let (instance, dir) = scratch_instance();
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let result = perform_deletion(&request);
            assert!(result.success, "deletion errors: {:?}", result.errors);
            assert!(
                !dir.exists(),
                "scratch directory must be gone after perform_deletion"
            );
            assert!(
                result
                    .messages
                    .iter()
                    .any(|m| m.contains("Scratch directory removed")),
                "expected scratch-removed message, got {:?}",
                result.messages
            );
        }

        #[test]
        #[serial]
        fn scratch_session_with_missing_dir_still_succeeds() {
            let _tmp = isolate_app_dir();
            let (instance, dir) = scratch_instance();
            // Simulate the "directory already gone" race (user deleted
            // manually, FS hiccup, etc.). Deletion must not fail.
            fs::remove_dir_all(&dir).unwrap();

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&request);
            assert!(
                result.success,
                "missing scratch dir must not fail deletion: {:?}",
                result.errors
            );
        }

        #[test]
        #[serial]
        fn tampered_project_path_does_not_get_removed() {
            // Defense against an edited or corrupted session JSON that
            // sets `scratch: true` while pointing project_path at something
            // the guard would reject. The directory must survive deletion.
            let _tmp = isolate_app_dir();
            let bystander =
                std::env::temp_dir().join(format!("important-data-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&bystander).expect("create bystander");
            fs::write(bystander.join("file.txt"), b"keep me").unwrap();

            let mut instance = Instance::new("Tampered", bystander.to_str().unwrap());
            instance.scratch = true;

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&request);

            assert!(
                bystander.exists(),
                "guard must refuse to remove a path outside the scratch root"
            );
            assert!(
                bystander.join("file.txt").exists(),
                "bystander contents must survive"
            );
            // The guard refusal must also surface as an error on the
            // deletion result, so callers can report the partial
            // cleanup instead of silently treating it as a clean
            // delete.
            assert!(
                result.errors.iter().any(|e| e.contains("scratch guard")),
                "guard refusal must be reported in result.errors, got: {:?}",
                result.errors
            );
            let _ = fs::remove_dir_all(&bystander);
        }

        #[test]
        #[serial]
        fn keep_scratch_leaves_dir_on_disk_and_reports_path() {
            // The --keep-scratch escape hatch. Session record still gets
            // removed (caller's responsibility), but the scratch directory
            // stays put and the deletion result calls out the kept path.
            let _tmp = isolate_app_dir();
            let (instance, dir) = scratch_instance();
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: true,
            };

            let result = perform_deletion(&request);
            assert!(
                result.success,
                "keep-scratch deletion errors: {:?}",
                result.errors
            );
            assert!(
                dir.exists(),
                "keep-scratch must leave the directory on disk"
            );
            let kept_msg = result
                .messages
                .iter()
                .find(|m| m.contains("Scratch directory kept at:"));
            assert!(
                kept_msg.is_some(),
                "expected kept-path message, got {:?}",
                result.messages
            );
            assert!(
                kept_msg.unwrap().contains(dir.to_str().unwrap()),
                "kept-path message must include the actual path; got: {}",
                kept_msg.unwrap()
            );
            // Clean up the leftover dir so the next test starts clean.
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        #[serial]
        fn non_scratch_session_under_app_dir_is_untouched() {
            // A regular session whose project_path happens to live under
            // the app dir (e.g. a test fixture) must not be removed.
            let _tmp = isolate_app_dir();
            let dir = crate::session::get_app_dir()
                .unwrap()
                .join(format!("non-scratch-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).expect("create non-scratch test dir");

            let instance = Instance::new("Regular", dir.to_str().unwrap());
            // scratch is false by default.

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let _ = perform_deletion(&request);

            assert!(
                dir.exists(),
                "non-scratch session must never trip the scratch cleanup branch"
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
