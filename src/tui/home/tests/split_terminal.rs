//! Split preview: the agent above its paired terminal, `view_mode` picking the focused
//! half and the other half painted from its own observer-only capture.

use super::*;

/// One tmux-backed session, selected, with the split on. Returns the agent and paired
/// terminal tmux names for the caller to pin in the session cache.
fn split_env(structured: bool) -> (TestEnv, String, String) {
    let mut inst = Instance::new("splitter", "/tmp/split");
    if structured {
        inst.view = crate::session::View::Structured;
    }
    let agent = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    let term = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);
    let mut env = seeded_env(test_home(), &[inst.clone()], true);
    env.view.select_session_by_id(&inst.id);
    env.view.split_terminal = true;
    (env, agent, term)
}

#[test]
#[serial]
fn split_preview_layout_and_focus() {
    let cache = crate::tmux::SessionCacheGuard::capture();
    struct Case {
        name: &'static str,
        structured: bool,
        mode: ViewMode,
        agent_alive: bool,
        running: bool,
        height: u16,
        expect_split: bool,
    }
    let base = |name| Case {
        name,
        structured: false,
        mode: ViewMode::Structured,
        agent_alive: true,
        running: true,
        height: 40,
        expect_split: true,
    };
    let cases = [
        base("agent focused"),
        Case {
            mode: ViewMode::Terminal,
            ..base("terminal focused")
        },
        Case {
            running: false,
            ..base("no terminal yet")
        },
        Case {
            structured: true,
            expect_split: false,
            ..base("structured session stays single")
        },
        Case {
            height: 14,
            expect_split: false,
            ..base("short preview stays single")
        },
        Case {
            mode: ViewMode::Terminal,
            agent_alive: false,
            expect_split: false,
            ..base("gone agent stays single")
        },
    ];
    for case in cases {
        let Case {
            name,
            structured,
            mode,
            agent_alive,
            running,
            height,
            expect_split,
        } = case;
        let (mut env, agent, term) = split_env(structured);
        let present: Vec<&str> = [(agent_alive, agent.as_str()), (running, term.as_str())]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect();
        cache.force_present(&present);
        env.view.view_mode = mode.clone();

        let screen = render_home_to_string(&mut env.view, 120, height);

        assert_eq!(
            screen.contains(" Terminal ─"),
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
    let (mut env, agent, term) = split_env(false);
    cache.force_present(&[agent.as_str(), term.as_str()]);
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
    let agent = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    let term = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);
    let mut env = seeded_env(test_home(), &[inst.clone()], true);
    env.view.select_session_by_id(&inst.id);
    env.view.split_terminal = true;
    env.view.show_preview_info = true;
    cache.force_present(&[agent.as_str(), term.as_str()]);

    let screen = render_home_to_string(&mut env.view, 120, 24);

    assert!(screen.contains(" Terminal ─"), "{screen}");
    assert_eq!(
        env.view.preview_pane_area, env.view.preview_area,
        "{screen}"
    );
    assert!(!screen.contains("hide info with"), "{screen}");
}
