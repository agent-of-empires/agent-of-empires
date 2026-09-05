use super::*;
use crate::migrations::progress::Event;
use crate::tui::store_move_poller::{StoreMovePoller, StoreMoveResult};

fn in_flight(view: &mut HomeView, title: &str) {
    view.store_move_in_flight = Some(super::super::store_move::StoreMoveInFlight {
        title: title.to_string(),
        console: Default::default(),
        last_line: None,
    });
}

fn seed_result(view: &mut HomeView, id: &str, outcome: Result<bool, String>) {
    view.store_move_poller = StoreMovePoller::with_result_for_test(StoreMoveResult {
        session_id: id.to_string(),
        outcome,
        resume: Some(Action::AttachSession(id.to_string())),
    });
}

/// A moved store, or a container that was already up, hands the deferred
/// attach back; a store that is still shared after the move, or a failed
/// move, explains itself in a dialog and hands nothing back.
#[test]
#[serial]
fn a_finished_move_resumes_or_explains() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();

    in_flight(&mut env.view, "session0");
    seed_result(&mut env.view, &id, Ok(true));
    let poll = env.view.poll_store_move();
    assert!(poll.changed);
    assert_eq!(poll.resume, Some(Action::AttachSession(id.clone())));
    assert!(env.view.store_move_in_flight.is_none());
    assert!(env.view.info_dialog.is_none());
    assert!(!env.view.poll_store_move().changed, "nothing in flight");

    in_flight(&mut env.view, "session0");
    seed_result(&mut env.view, &id, Ok(false));
    assert_eq!(
        env.view.poll_store_move().resume,
        Some(Action::AttachSession(id.clone()))
    );

    in_flight(&mut env.view, "session0");
    seed_result(&mut env.view, &id, Err("disk full".to_string()));
    let poll = env.view.poll_store_move();
    assert!(poll.changed);
    assert_eq!(poll.resume, None);
    let dialog = env.view.info_dialog.take().expect("failure dialog");
    assert_eq!(dialog.title(), "Agent Store Move Failed");
    assert!(dialog.message().contains("disk full"));

    // The row stays on the shared generation after a move that published
    // nothing (a live cohort peer): no attach, and a dialog saying why.
    Storage::new_unwatched("test")
        .unwrap()
        .update(|instances, _| {
            instances[0].sandbox_store_generation = 0;
            instances[0].sandbox_info = Some(
                serde_json::from_value(serde_json::json!({
                    "enabled": true,
                    "image": "img",
                    "container_name": "aoe-sandbox-x",
                }))
                .unwrap(),
            );
            Ok(())
        })
        .unwrap();
    env.view.reload().unwrap();
    assert!(env.view.sandbox_store_move_pending(&id));
    in_flight(&mut env.view, "session0");
    seed_result(&mut env.view, &id, Ok(true));
    let poll = env.view.poll_store_move();
    assert_eq!(poll.resume, None);
    let dialog = env.view.info_dialog.take().expect("still-shared dialog");
    assert_eq!(dialog.title(), "Agent Store Still Shared");
    // A container that was already up hands the attach back and lets it
    // through the launch gate once: the row is still on the shared
    // generation, so a gate reading only the row would defer to another
    // move, which would find the same container and loop.
    in_flight(&mut env.view, "session0");
    seed_result(&mut env.view, &id, Ok(false));
    assert_eq!(
        env.view.poll_store_move().resume,
        Some(Action::AttachSession(id.clone()))
    );
    assert!(env.view.sandbox_store_move_pending(&id));
    assert!(!env.view.needs_store_move_before_launch(&id));
    assert!(env.view.needs_store_move_before_launch(&id));
    // A move with nothing to resume (a live send) exempts no later launch.
    in_flight(&mut env.view, "session0");
    env.view.store_move_poller = StoreMovePoller::with_result_for_test(StoreMoveResult {
        session_id: id.clone(),
        outcome: Ok(false),
        resume: None,
    });
    assert_eq!(env.view.poll_store_move().resume, None);
    assert!(env.view.needs_store_move_before_launch(&id));
    // A second move is refused while one is in flight.
    assert!(env.view.begin_store_move(&id, None));
    assert!(!env.view.begin_store_move(&id, None));
}

/// The status line follows the migration's step and progress, and reports a
/// change only when its text would differ.
#[test]
#[serial]
fn status_line_follows_the_moves_progress() {
    let mut env = create_test_env_empty();
    assert_eq!(env.view.store_move_status_line(), None);
    in_flight(&mut env.view, "big session");
    assert_eq!(
        env.view.store_move_status_line().as_deref(),
        Some("moving the agent store of 'big session': starting")
    );
    let inflight = env.view.store_move_in_flight.as_mut().unwrap();
    inflight.console.apply(Event::Started {
        version: 27,
        name: "isolate_sandbox_stores",
        position: 1,
        total: 1,
    });
    inflight
        .console
        .apply(Event::Step("copying agent store 1/1".into()));
    inflight
        .console
        .apply(Event::Progress("200 files, 11 MB".into()));
    let line = env.view.store_move_status_line().unwrap();
    assert!(
        line.starts_with(
            "moving the agent store of 'big session': copying agent store 1/1, 200 files, 11 MB ("
        ),
        "{line}"
    );
}
