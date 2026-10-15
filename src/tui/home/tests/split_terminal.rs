//! Split preview: the agent above its paired terminal, `view_mode` picking the focused
//! half and the other half painted from its own observer-only capture.

use super::*;

/// One tmux-backed session, selected, with the split on and the session cache pinned to
/// `running_terminal`.
fn split_env(structured: bool) -> (TestEnv, String) {
    let mut inst = Instance::new("splitter", "/tmp/split");
    if structured {
        inst.view = crate::session::View::Structured;
    }
    let term = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);
    let mut env = seeded_env(test_home(), &[inst.clone()], true);
    env.view.select_session_by_id(&inst.id);
    env.view.split_terminal = true;
    (env, term)
}

#[test]
#[serial]
fn split_preview_layout_and_focus() {
    let cache = crate::tmux::SessionCacheGuard::capture();
    let cases = [
        ("agent focused", false, ViewMode::Structured, true, 40, true),
        (
            "terminal focused",
            false,
            ViewMode::Terminal,
            true,
            40,
            true,
        ),
        (
            "no terminal yet",
            false,
            ViewMode::Structured,
            false,
            40,
            true,
        ),
        (
            "structured session stays single",
            true,
            ViewMode::Structured,
            true,
            40,
            false,
        ),
        (
            "short preview stays single",
            false,
            ViewMode::Structured,
            true,
            14,
            false,
        ),
    ];
    for (name, structured, mode, running, height, expect_split) in cases {
        let (mut env, term) = split_env(structured);
        let present: &[&str] = if running { &[term.as_str()] } else { &[] };
        cache.force_present(present);
        env.view.view_mode = mode.clone();

        let screen = render_home_to_string(&mut env.view, 120, height);

        assert_eq!(
            screen.contains(" Terminal "),
            expect_split,
            "{name}:\n{screen}"
        );
        if !expect_split {
            assert!(env.view.split_capture_worker.is_none(), "{name}");
            continue;
        }
        let outer = env.view.preview_outer_area;
        let midpoint = outer.y + outer.height / 2;
        let preview = env.view.preview_area;
        match mode {
            ViewMode::Terminal => assert!(preview.y > midpoint, "{name}: {preview:?}"),
            _ => assert!(preview.bottom() <= midpoint + 1, "{name}: {preview:?}"),
        }
        assert_eq!(
            screen.contains("No terminal running"),
            !running,
            "{name}:\n{screen}"
        );
        assert_eq!(
            env.view.split_capture_worker.is_some(),
            running || mode == ViewMode::Terminal,
            "{name}: the passive half polls only a pane that exists"
        );
    }
}

/// Swapping focus hands each pane to the other worker without blanking the cache the
/// newly focused worker inherits.
#[test]
#[serial]
fn split_focus_swap_keeps_the_other_half_cached() {
    let cache = crate::tmux::SessionCacheGuard::capture();
    let (mut env, term) = split_env(false);
    cache.force_present(&[term.as_str()]);
    let id = env.view.selected_session.clone().unwrap();
    render_home_to_string(&mut env.view, 120, 40);
    assert_eq!(
        env.view.split_capture_target.as_deref(),
        Some(term.as_str())
    );

    env.view.terminal_preview_cache.store_capture(
        "shell output".into(),
        id,
        term.clone(),
        0,
        (100, 10),
        None,
    );
    env.view.view_mode = ViewMode::Terminal;
    render_home_to_string(&mut env.view, 120, 40);

    assert_eq!(
        env.view.preview_capture_target.as_deref(),
        Some(term.as_str())
    );
    assert_eq!(env.view.terminal_preview_cache.content, "shell output");
    assert_ne!(
        env.view.split_capture_target.as_deref(),
        Some(term.as_str())
    );
}

/// A worktree agent's tall info header must not eat the split half: headers are off while
/// split, so the focused output (and the fleet size derived from it) is the whole half.
#[test]
#[serial]
fn split_hides_info_headers_so_output_owns_each_half() {
    let cache = crate::tmux::SessionCacheGuard::capture();
    let mut inst = Instance::new("treeish", "/tmp/tree");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/tree-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let term = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);
    let mut env = seeded_env(test_home(), &[inst.clone()], true);
    env.view.select_session_by_id(&inst.id);
    env.view.split_terminal = true;
    env.view.show_preview_info = true;
    cache.force_present(&[term.as_str()]);

    let screen = render_home_to_string(&mut env.view, 120, 24);

    assert!(screen.contains(" Terminal "), "{screen}");
    assert_eq!(
        env.view.preview_pane_area, env.view.preview_area,
        "{screen}"
    );
    assert!(!screen.contains("hide info with"), "{screen}");
}
