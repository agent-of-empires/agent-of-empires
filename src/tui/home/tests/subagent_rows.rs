//! Subagent rows nest under their session, are reachable by keyboard, and never
//! route session actions to the parent.

use super::*;
use crate::session::subagents::{Subagent, SubagentActivity, SubagentScan, SubagentState};
use crate::tui::subagent_poller::{SubagentPoller, SubagentSnapshot};

fn subagent(agent_id: &str, state: SubagentState, description: &str) -> Subagent {
    Subagent {
        agent_id: agent_id.into(),
        agent_type: "Explore".into(),
        description: description.into(),
        state,
        started_at: None,
    }
}

fn default_activity(agent_id: &str) -> Vec<SubagentActivity> {
    vec![
        SubagentActivity::Tool {
            name: "Grep".into(),
            detail: "fn handle_key".into(),
        },
        SubagentActivity::Text(format!("{agent_id} found three callers")),
    ]
}

fn apply(view: &mut HomeView, snapshot: SubagentSnapshot) -> bool {
    apply_with(view, snapshot, None)
}

/// Apply a scan the way the poller answers it: the focused subagent, when
/// listed, carries `activity` or a default.
fn apply_with(
    view: &mut HomeView,
    snapshot: SubagentSnapshot,
    activity: Option<Vec<SubagentActivity>>,
) -> bool {
    let focus = view
        .selected_subagent
        .clone()
        .filter(|(parent, agent)| {
            snapshot
                .get(parent)
                .is_some_and(|listed| listed.iter().any(|s| s.agent_id == *agent))
        })
        .map(|focus| {
            let activity = activity.unwrap_or_else(|| default_activity(&focus.1));
            (focus, activity)
        });
    view.subagent_poller = SubagentPoller::seeded_for_test(SubagentScan {
        subagents: snapshot,
        focus,
    });
    view.pending_subagent_refresh = true;
    view.apply_subagent_updates()
}

fn subagent_rows(view: &HomeView) -> Vec<(String, String, usize)> {
    view.flat_items
        .iter()
        .filter_map(|item| match item {
            Item::Subagent {
                parent_id,
                agent_id,
                depth,
            } => Some((parent_id.clone(), agent_id.clone(), *depth)),
            _ => None,
        })
        .collect()
}

#[test]
#[serial]
fn subagent_rows_expand_select_and_return_to_parent() {
    let mut env = create_test_env_with_sessions(2);
    env.view.cursor = 0;
    env.view.update_selected();
    let parent = cursor_session_id(&env.view).unwrap();
    let parent_depth = env.view.flat_items[0].depth();
    let rows_before = env.view.flat_items.len();

    let snapshot = SubagentSnapshot::from([(
        parent.clone(),
        vec![
            subagent("a1", SubagentState::Done, "find callers"),
            subagent("a2", SubagentState::Running, "read the docs"),
        ],
    )]);
    assert!(apply(&mut env.view, snapshot.clone()));
    assert_eq!(
        env.view.flat_items.len(),
        rows_before,
        "collapsed by default"
    );
    assert!(rendered_row_text(&env.view, &env.view.flat_items[0]).contains("▶2"));
    assert!(
        !apply(&mut env.view, snapshot.clone()),
        "an unchanged scan is not a change"
    );

    env.view.handle_key(key(KeyCode::Right), None);
    assert_eq!(
        subagent_rows(&env.view),
        vec![
            (parent.clone(), "a1".into(), parent_depth + 1),
            (parent.clone(), "a2".into(), parent_depth + 1),
        ]
    );
    assert_eq!(
        cursor_session_id(&env.view).as_deref(),
        Some(parent.as_str())
    );
    let row = rendered_row_text(&env.view, &env.view.flat_items[1]);
    assert!(
        row.contains("find callers") && !row.contains("Explore"),
        "{row}"
    );

    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Down), None);
    assert_eq!(
        env.view.selected_subagent,
        Some((parent.clone(), "a2".into()))
    );
    assert_eq!(env.view.selected_session, None);

    // The preview waits for the scan that reads this subagent's activity, and
    // that scan repaints it without touching the rows.
    let screen = render_home_to_string(&mut env.view, 120, 30);
    assert!(screen.contains("Loading…"), "{screen}");
    let rows = env.view.flat_items.len();
    assert!(apply(&mut env.view, snapshot.clone()));
    assert_eq!(env.view.flat_items.len(), rows);

    // Session actions have no target on a subagent row.
    disable_delete_to_trash();
    env.view.handle_key(key(KeyCode::Char('d')), None);
    assert!(env.view.unified_delete_dialog.is_none());
    assert!(env.view.confirm_dialog.is_none());

    let screen = render_home_to_string(&mut env.view, 120, 30);
    for expected in [
        "Subagent",
        "read the docs",
        "Grep fn handle_key",
        "a2 found three callers",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
    }

    // Enter opens the parent as Enter on its own row would (tmux attach by default).
    assert_eq!(
        env.view.handle_key(key(KeyCode::Enter), None),
        Some(Action::AttachSession(parent.clone()))
    );
    assert_eq!(env.view.cursor, 0);
    assert_eq!(env.view.selected_session.as_deref(), Some(parent.as_str()));
    assert_eq!(env.view.selected_subagent, None);

    env.view.handle_key(key(KeyCode::Left), None);
    assert!(subagent_rows(&env.view).is_empty());
    assert!(rendered_row_text(&env.view, &env.view.flat_items[0]).contains("▶2"));
}

#[test]
#[serial]
fn selected_subagent_that_ages_out_falls_back_to_parent() {
    let mut env = create_test_env_with_sessions(2);
    env.view.cursor = 0;
    env.view.update_selected();
    let parent = cursor_session_id(&env.view).unwrap();
    let first =
        |state| SubagentSnapshot::from([(parent.clone(), vec![subagent("a1", state, "x")])]);
    apply(&mut env.view, first(SubagentState::Running));
    env.view.handle_key(key(KeyCode::Char('l')), None);
    env.view.handle_key(key(KeyCode::Down), None);
    assert_eq!(
        env.view.selected_subagent,
        Some((parent.clone(), "a1".into()))
    );

    // A state change keeps the row and the cursor on it.
    apply(&mut env.view, first(SubagentState::Done));
    assert_eq!(env.view.cursor, 1);
    assert_eq!(
        env.view.selected_subagent,
        Some((parent.clone(), "a1".into()))
    );

    // Gone entirely: the expansion is dropped and the cursor lands on the parent.
    apply(&mut env.view, SubagentSnapshot::new());
    assert!(subagent_rows(&env.view).is_empty());
    assert!(env.view.expanded_subagents.is_empty());
    assert_eq!(env.view.selected_session.as_deref(), Some(parent.as_str()));
    assert_eq!(env.view.selected_subagent, None);
}

#[test]
#[serial]
fn single_click_toggles_subagent_rows_but_double_click_does_not() {
    let mut env = create_test_env_with_sessions(2);
    env.view.list_inner_area = ratatui::layout::Rect::new(1, 1, 40, 10);
    env.view.cursor = 1;
    env.view.update_selected();
    let parent = session_id_at(&env.view, 0).unwrap();
    apply(
        &mut env.view,
        SubagentSnapshot::from([(
            parent.clone(),
            vec![subagent("a1", SubagentState::Running, "x")],
        )]),
    );
    let ms = std::time::Duration::from_millis;

    // A single click selects at once and toggles once no second click can follow.
    let t0 = std::time::Instant::now();
    assert_eq!(env.view.handle_click_at(t0, 5, 1), None);
    assert_eq!(env.view.selected_session.as_deref(), Some(parent.as_str()));
    assert!(!env.view.tick_pending_subagent_toggle(t0 + ms(100)));
    assert!(
        subagent_rows(&env.view).is_empty(),
        "still inside the window"
    );
    assert!(env.view.tick_pending_subagent_toggle(t0 + ms(500)));
    assert_eq!(subagent_rows(&env.view).len(), 1);

    let t1 = t0 + ms(2000);
    env.view.handle_click_at(t1, 5, 1);
    env.view.tick_pending_subagent_toggle(t1 + ms(500));
    assert!(subagent_rows(&env.view).is_empty());

    // A double-click attaches and leaves the rows as they were.
    let t2 = t1 + ms(2000);
    assert_eq!(env.view.handle_click_at(t2, 5, 1), None);
    assert_eq!(
        env.view.handle_click_at(t2 + ms(150), 5, 1),
        Some(Action::AttachSession(parent.clone()))
    );
    assert!(!env.view.tick_pending_subagent_toggle(t2 + ms(1000)));
    assert!(subagent_rows(&env.view).is_empty());
}

#[test]
#[serial]
fn wheel_scrolls_subagent_preview_and_holds_while_output_grows() {
    let mut env = create_test_env_with_sessions(1);
    env.view.cursor = 0;
    env.view.update_selected();
    let parent = cursor_session_id(&env.view).unwrap();
    let snapshot = SubagentSnapshot::from([(
        parent.clone(),
        vec![subagent("a1", SubagentState::Running, "long")],
    )]);
    let steps = |count: usize| {
        Some(
            (0..count)
                .map(|i| SubagentActivity::Text(format!("step-{i:02}")))
                .collect(),
        )
    };
    apply(&mut env.view, snapshot.clone());
    env.view.handle_key(key(KeyCode::Char('l')), None);
    env.view.handle_key(key(KeyCode::Down), None);
    assert!(env.view.selected_subagent.is_some());
    apply_with(&mut env.view, snapshot.clone(), steps(40));

    let screen = render_home_to_string(&mut env.view, 100, 20);
    assert!(screen.contains("step-39") && !screen.contains("step-10"));

    let (col, row) = (env.view.preview_area.x + 2, env.view.preview_area.y + 2);
    for _ in 0..100 {
        env.view.handle_scroll_up(col, row);
    }
    let screen = render_home_to_string(&mut env.view, 100, 20);
    assert!(
        screen.contains("step-00") && !screen.contains("step-39"),
        "{screen}"
    );
    let top = env.view.preview_scroll_offset;
    assert!(!env.view.handle_scroll_up(col, row), "the top is a limit");

    // New output lands below a scrolled view without moving it.
    apply_with(&mut env.view, snapshot, steps(45));
    let screen = render_home_to_string(&mut env.view, 100, 20);
    assert!(screen.contains("step-00"), "{screen}");
    assert_eq!(
        env.view.preview_scroll_offset,
        top + 10,
        "five entries, two rows each"
    );

    for _ in 0..100 {
        env.view.handle_scroll_down(col, row);
    }
    let screen = render_home_to_string(&mut env.view, 100, 20);
    assert!(screen.contains("step-44"), "{screen}");
    assert_eq!(env.view.preview_scroll_offset, 0);

    env.view.handle_scroll_up(col, row);
    env.view.handle_key(key(KeyCode::Up), None);
    assert_eq!(
        env.view.preview_scroll_offset, 0,
        "changing rows resets the scroll"
    );
}
