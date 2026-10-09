//! Moving a legacy sandbox store ahead of a launch, off the event loop.

use super::*;
use crate::migrations::progress::ConsoleProgress;
use crate::tui::app::Action;
use crate::tui::store_move_poller::StoreMoveResult;

/// The move in flight. One at a time: the worker is serial and the status
/// line has one row.
pub(super) struct StoreMoveInFlight {
    pub(super) title: String,
    pub(super) origin: RequestOrigin,
    pub(super) console: ConsoleProgress,
    /// The status line as last rendered, so a tick reports a change only
    /// when the line would read differently.
    pub(super) last_line: Option<String>,
}

/// What a tick found: whether the status line changed. Deferred actions are handed
/// back separately, only after their original-profile reload is acknowledged.
#[derive(Default)]
pub(crate) struct StoreMovePoll {
    pub(crate) changed: bool,
}

impl HomeView {
    /// Whether launching `id` would first copy its sandbox store off the
    /// shared one, which can take minutes.
    pub(crate) fn sandbox_store_move_pending(&self, id: &str) -> bool {
        self.get_instance(id)
            .is_some_and(Instance::sandbox_store_move_pending)
    }

    /// Whether a launch of `id` must first move its store. False once, for the launch a move
    /// handed back after finding the container up; see `store_move_bypass`.
    pub(crate) fn needs_store_move_before_launch(&mut self, id: &str) -> bool {
        if self.store_move_bypass.as_deref() == Some(id) {
            self.store_move_bypass = None;
            return false;
        }
        self.sandbox_store_move_pending(id)
    }

    /// Start moving `id`'s sandbox store on the worker, running `resume` once it has moved.
    /// Returns `false` while another move is in flight, since the status line already says
    /// what is happening.
    pub(crate) fn begin_store_move(&mut self, id: &str, resume: Option<Action>) -> bool {
        if self.store_move_in_flight.is_some() {
            return false;
        }
        let Some(instance) = self.get_instance(id).cloned() else {
            return false;
        };
        let row = match self.capture_transaction_row(id) {
            Ok(row) => row,
            Err(error) => {
                self.info_dialog = Some(InfoDialog::new(
                    "Agent Store Move Failed",
                    &format!("{error:#}"),
                ));
                return false;
            }
        };
        let origin = row.origin.clone();
        self.store_move_in_flight = Some(StoreMoveInFlight {
            title: instance.title.clone(),
            origin,
            console: ConsoleProgress::default(),
            last_line: None,
        });
        // Anything a previous move left unread belongs to that move.
        while self.store_move_poller.try_recv_progress().is_some() {}
        if let Err(error) =
            self.request_transaction(persistence_transactions::TransactionRequest::StoreMove {
                row,
                resume,
            })
        {
            self.store_move_in_flight = None;
            self.info_dialog = Some(InfoDialog::new(
                "Agent Store Move Failed",
                &format!("{error:#}"),
            ));
            return false;
        }
        true
    }

    /// Drain the move's progress into the status line and apply its result: a moved store
    /// re-reads the row and hands back the resume action, while a store that could not move
    /// explains itself in a dialog.
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
        let original = self
            .store_move_in_flight
            .take()
            .expect("original store move remains in flight")
            .origin;
        poll.changed = true;
        let StoreMoveResult {
            session_id,
            outcome,
            resume,
        } = result;
        match outcome {
            // The container was up, so the launch proceeds on the shared store. Only a
            // launch handed back here may pass the gate: a move started with nothing to
            // resume must not exempt a later launch, by which time the container may have
            // stopped.
            Ok(false) => self.request_reload_after(
                ReloadKind::Full,
                persistence_lane::ReloadContinuation::StoreMove {
                    id: session_id,
                    title,
                    origin: original,
                    resume,
                    container_up: true,
                },
            ),
            Ok(true) => {
                self.request_reload_after(
                    ReloadKind::Full,
                    persistence_lane::ReloadContinuation::StoreMove {
                        id: session_id,
                        title,
                        origin: original,
                        resume,
                        container_up: false,
                    },
                );
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
