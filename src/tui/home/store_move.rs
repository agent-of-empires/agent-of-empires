//! Moving a legacy sandbox store ahead of a launch, off the event loop.

use super::*;
use crate::migrations::progress::ConsoleProgress;
use crate::tui::app::Action;
use crate::tui::store_move_poller::{StoreMoveRequest, StoreMoveResult};

/// The move in flight. One at a time: the worker is serial and the status
/// line has one row.
pub(super) struct StoreMoveInFlight {
    pub(super) title: String,
    pub(super) console: ConsoleProgress,
    /// The status line as last rendered, so a tick reports a change only
    /// when the line would read differently.
    pub(super) last_line: Option<String>,
}

/// What a tick found: whether the status line changed, and the action a
/// finished move hands back.
#[derive(Default)]
pub(crate) struct StoreMovePoll {
    pub(crate) changed: bool,
    pub(crate) resume: Option<Action>,
}

impl HomeView {
    /// Whether launching `id` would first copy its sandbox store off the
    /// shared one, which can take minutes.
    pub(crate) fn sandbox_store_move_pending(&self, id: &str) -> bool {
        self.get_instance(id)
            .is_some_and(Instance::sandbox_store_move_pending)
    }

    /// Whether a launch of `id` must first move its store. False once, for
    /// the launch a move handed back after finding the container up; see
    /// `store_move_bypass`.
    pub(crate) fn needs_store_move_before_launch(&mut self, id: &str) -> bool {
        if self.store_move_bypass.as_deref() == Some(id) {
            self.store_move_bypass = None;
            return false;
        }
        self.sandbox_store_move_pending(id)
    }

    /// Start moving `id`'s sandbox store on the worker; `resume` runs once it
    /// has moved. Refused, returning `false`, while another move is in
    /// flight: the status line already says what is happening.
    pub(crate) fn begin_store_move(&mut self, id: &str, resume: Option<Action>) -> bool {
        if self.store_move_in_flight.is_some() {
            return false;
        }
        let Some(instance) = self.get_instance(id).cloned() else {
            return false;
        };
        self.store_move_in_flight = Some(StoreMoveInFlight {
            title: instance.title.clone(),
            console: ConsoleProgress::default(),
            last_line: None,
        });
        // Anything a previous move left unread belongs to that move.
        while self.store_move_poller.try_recv_progress().is_some() {}
        self.store_move_poller
            .request_move(StoreMoveRequest { instance, resume });
        true
    }

    /// Drain the move's progress into the status line and apply its result:
    /// a moved store re-reads the row and hands back the resume action, a
    /// store that could not move or a failed move explains itself in a
    /// dialog instead.
    pub(crate) fn poll_store_move(&mut self) -> StoreMovePoll {
        use std::sync::mpsc::TryRecvError;

        let mut poll = StoreMovePoll::default();
        let Some(inflight) = self.store_move_in_flight.as_mut() else {
            return poll;
        };
        while let Some(event) = self.store_move_poller.try_recv_progress() {
            inflight.console.apply(event);
        }
        let line = Self::render_store_move_line(inflight);
        if inflight.last_line != line {
            inflight.last_line = line;
            poll.changed = true;
        }
        let title = inflight.title.clone();
        let result = match self.store_move_poller.try_recv_result() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return poll,
            Err(TryRecvError::Disconnected) => {
                tracing::error!(target: "session.store", "store move worker gone");
                self.store_move_in_flight = None;
                self.info_dialog = Some(InfoDialog::new(
                    "Agent Store Move Failed",
                    &format!(
                        "The worker moving the agent store of '{title}' stopped. The session \
                         stays on the shared agent store; restart aoe to retry."
                    ),
                ));
                poll.changed = true;
                return poll;
            }
        };
        self.store_move_in_flight = None;
        poll.changed = true;
        let StoreMoveResult {
            session_id,
            outcome,
            resume,
        } = result;
        match outcome {
            // The container was up, so the launch proceeds on the shared
            // store. Only a launch handed back here may pass the gate: a move
            // started with nothing to resume must not exempt a later launch,
            // by which time the container may have stopped.
            Ok(false) => {
                self.store_move_bypass = resume.is_some().then_some(session_id);
                poll.resume = resume;
            }
            Ok(true) => {
                if let Err(error) = self.reload() {
                    tracing::warn!(
                        target: "session.store",
                        %error,
                        "reload after sandbox store move failed"
                    );
                }
                if self.sandbox_store_move_pending(&session_id) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Agent Store Still Shared",
                        &format!(
                            "The agent store of '{title}' did not move: another sandboxed \
                             session sharing it is running or could not be checked, or \
                             another aoe process is still moving it. Stop that session and \
                             open this one again, or run `aoe migrate`."
                        ),
                    ));
                } else {
                    poll.resume = resume;
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "session.store",
                    id = %session_id,
                    %error,
                    "sandbox store move failed"
                );
                self.info_dialog = Some(InfoDialog::new(
                    "Agent Store Move Failed",
                    &format!(
                        "Could not move the agent store of '{title}': {error}. The session \
                         stays on the shared agent store; opening it again retries."
                    ),
                ));
            }
        }
        poll
    }

    /// The status line for the move in flight, if any.
    pub(crate) fn store_move_status_line(&self) -> Option<String> {
        self.store_move_in_flight
            .as_ref()
            .and_then(Self::render_store_move_line)
    }

    fn render_store_move_line(inflight: &StoreMoveInFlight) -> Option<String> {
        let activity = inflight
            .console
            .activity()
            .unwrap_or_else(|| "starting".to_string());
        Some(format!(
            "moving the agent store of '{}': {activity}",
            inflight.title
        ))
    }
}
