//! Split preview: the agent above its paired terminal, `view_mode` picking the focused
//! half and the other half painted from its own observer-only capture.

use super::*;

/// One selected session with split preview and a daemon-observed paired terminal.
fn split_env(structured: bool, running: bool) -> (TestEnv, String, String) {
    let mut inst = Instance::new("splitter", "/tmp/split");
    if structured {
        inst.view = crate::session::View::Structured;
    }
    let agent = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    let term = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);
    let mut env = seeded_env(test_home(), &[inst.clone()], true);
    env.view.mutate_instance(&inst.id, |instance| {
        instance.auxiliary = vec![crate::session::AuxiliaryObservation {
            target: crate::session::AuxiliaryTarget::Host { index: 0 },
            pane: crate::session::PaneObservation {
                state: if running {
                    crate::session::PanePresence::Alive
                } else {
                    crate::session::PanePresence::Absent
                },
                tmux_session: running.then(|| term.clone()),
                legacy_tool: None,
                physical: None,
            },
        }];
    });
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
        let (mut env, agent, _term) = split_env(structured, running);
        let present: &[&str] = if agent_alive { &[agent.as_str()] } else { &[] };
        cache.force_present(present);
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
    let (mut env, agent, term) = split_env(false, true);
    cache.force_present(&[agent.as_str()]);
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

#[test]
#[serial]
fn passive_split_follows_captured_cursor_without_discarding_history_or_clear_frames() {
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    let present = crate::tmux::SessionCacheGuard::capture();
    let (mut env, agent, _term) = split_env(false, true);
    present.force_present(&[agent.as_str()]);
    render_home_to_string(&mut env.view, 100, 30);
    env.view
        .split_capture_worker
        .as_mut()
        .expect("passive observer exists")
        .stop_for_test();
    let cursor = crate::tmux::PaneCursor {
        x: 33,
        y: 2,
        visible: true,
        pane_height: 26,
        history_size: 2,
        pane_width: 61,
        alternate_on: false,
        mouse_tracking: false,
        mouse_sgr: false,
        mouse_all: false,
        position_reliable: true,
        composite_pane0: None,
    };
    let mut top = [""; 28];
    top[0] = "HISTORY_ONLY";
    top[2] = "SHELL_SIDE";
    top[4] = "$ ";
    let mut background = top;
    background[3] = "\x1b[44m \x1b[0m";
    let mut cleared = [""; 28];
    cleared[0] = "HISTORY_ONLY";
    cleared[1] = "OLD_SHELL_SIDE";
    let mut bottom = [""; 28];
    bottom[0] = "HISTORY_ONLY";
    bottom[27] = "SHELL_SIDE";
    let top = top.join("\n") + "\n";
    let background = background.join("\n") + "\n";
    let cleared = cleared.join("\n") + "\n";
    let bottom = bottom.join("\n") + "\n";
    let theme = crate::tui::styles::load_theme("empire");
    for (name, content, cursor, shell_visible, background_visible) in [
        ("taller screen", top.as_str(), Some(cursor), true, false),
        (
            "cleared screen",
            cleared.as_str(),
            Some(crate::tmux::PaneCursor { y: 0, ..cursor }),
            false,
            false,
        ),
        ("fresh empty frame", "", Some(cursor), false, false),
        (
            "painted background",
            background.as_str(),
            Some(cursor),
            true,
            true,
        ),
        (
            "unreliable cursor",
            top.as_str(),
            Some(crate::tmux::PaneCursor {
                position_reliable: false,
                ..cursor
            }),
            false,
            false,
        ),
        ("missing cursor", top.as_str(), None, false, false),
        (
            "bottom cursor",
            bottom.as_str(),
            Some(crate::tmux::PaneCursor { y: 25, ..cursor }),
            true,
            false,
        ),
    ] {
        env.view
            .split_capture_worker
            .as_ref()
            .expect("passive observer exists")
            .inject_frame_with_cursor_for_test(2000, content, cursor);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                env.view.render(frame, area, &theme, None, None, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert_eq!(
            screen.contains("SHELL_SIDE"),
            shell_visible,
            "{name}:\n{screen}"
        );
        assert!(!screen.contains("HISTORY_ONLY"), "{name}:\n{screen}");
        if background_visible {
            let outer = env.view.preview_outer_area;
            let passive_top = env.view.preview_area.bottom();
            assert!(
                (passive_top..outer.bottom()).any(|y| {
                    (outer.x..outer.right()).any(|x| buffer[(x, y)].bg == Color::Blue)
                }),
                "captured background cells remain painted:\n{screen}"
            );
        }
    }
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

/// The empty-terminal hint names the key that actually starts it: in live mode `T` goes
/// to the agent, so the hint points at the leader chord instead.
#[test]
#[serial]
fn split_empty_terminal_hint_follows_live_mode() {
    use crate::tui::home::live_send::{parse_chord, LiveSendState, LiveSendTarget};

    let cache = crate::tmux::SessionCacheGuard::capture();
    let cases = [
        ("not live", None, "Press T to start one"),
        (
            "live with leader",
            parse_chord("C-b"),
            "Press Ctrl+B then t to start one",
        ),
        (
            "live, leader disabled",
            None,
            "Exit live mode, then press T to start one",
        ),
    ];
    for (i, (name, leader, expected)) in cases.into_iter().enumerate() {
        let (mut env, agent, _term) = split_env(false, false);
        cache.force_present(&[agent.as_str()]);
        if i > 0 {
            let inst = env.view.instance_at(0).clone();
            env.view.live_send = Some(LiveSendState {
                session_id: inst.id.clone(),
                title: inst.title.clone(),
                tmux_name: agent.clone(),
                target: LiveSendTarget::Agent,
                exit_chords: Vec::new(),
                leader,
            });
        }

        let screen = render_home_to_string(&mut env.view, 120, 40);

        assert!(screen.contains(expected), "{name}:\n{screen}");
    }
}
