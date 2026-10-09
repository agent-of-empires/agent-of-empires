//! Applying a user action to one row or to a selection, and the unread
//! bookkeeping that follows.

use super::*;

impl HomeView {
    /// Stage a concrete field diff. The worker writes it and ACKs before any dependent continuation.
    pub(in crate::tui) fn apply_user_action<F>(
        &mut self,
        id: &str,
        mutate: F,
    ) -> anyhow::Result<super::TransactionDisposition>
    where
        F: FnOnce(&mut Instance),
    {
        self.apply_user_action_after(
            id,
            mutate,
            persistence_transactions::MetadataContinuation::None,
        )
    }
    pub(super) fn apply_user_action_after<F>(
        &mut self,
        id: &str,
        mutate: F,
        after: persistence_transactions::MetadataContinuation,
    ) -> anyhow::Result<super::TransactionDisposition>
    where
        F: FnOnce(&mut Instance),
    {
        let Some(before) = self.get_instance(id).cloned() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        let row = persistence_transactions::RowCapture::capture(before.clone())?;
        let save = self.capture_save_snapshot();
        let mut edited = before;
        mutate(&mut edited);
        let revision = self.record_row_edit(&edited.source_profile, id);
        self.instances.insert(id.to_owned(), edited.clone());
        self.enqueue_transaction(
            persistence_transactions::TransactionRequest::Metadata {
                edits: vec![persistence_transactions::MetadataEdit {
                    row,
                    after: edited,
                    revision,
                }],
                after,
            },
            save,
        )
    }

    /// Clear the unread marker because the user engaged with the session (live-send, attach,
    /// or dwell). Runs regardless of the feature flag so a stale marker can't survive a
    /// disable and reappear, and writes only when the session is actually unread.
    pub(crate) fn clear_unread_on_view(&mut self, id: &str) {
        // Engaging with the row ends its manual-flag visit, so drop any hold;
        // otherwise a stale hold could later suppress an auto mark on this row.
        if self.manual_unread_hold.as_deref() == Some(id) {
            self.manual_unread_hold = None;
        }
        let is_unread = self.get_instance(id).is_some_and(|i| i.is_unread());
        if is_unread {
            let _ = self.apply_user_action(id, |i| i.mark_read());
        }
    }

    /// Dwell-to-read: clear the selected session's unread marker once it has stayed selected,
    /// with the list in the foreground, for `UNREAD_DWELL`, separating "scrolled past it"
    /// from "stopped to read it". Driven from the app tick loop; true when it cleared one.
    ///
    /// The clock is suspended and reset whenever the feature is off, a dialog or live-send is
    /// up, or nothing is selected, and it restarts when the selection moves. A row the user
    /// just flagged by hand is held until the cursor leaves it (`manual_unread_hold`).
    pub fn tick_unread_dwell(&mut self, now: std::time::Instant) -> bool {
        if !crate::session::unread_enabled() || self.has_dialog() {
            self.unread_dwell = None;
            return false;
        }
        let Some(id) = self.selected_session.clone() else {
            self.unread_dwell = None;
            return false;
        };
        // The manual hold only protects the row while it stays selected, so release it once
        // the cursor moves and a later return reads normally.
        if self
            .manual_unread_hold
            .as_deref()
            .is_some_and(|held| held != id)
        {
            self.manual_unread_hold = None;
        }
        let started = match &self.unread_dwell {
            Some((prev, started)) if *prev == id => *started,
            // First tick on this row (or selection moved): start the clock.
            _ => {
                self.unread_dwell = Some((id, now));
                return false;
            }
        };
        if now.duration_since(started) < UNREAD_DWELL {
            return false;
        }
        // A row flagged by hand is held for this visit so the dwell doesn't undo the mark.
        // The clock stays parked on the row either way, to avoid re-evaluating every tick.
        if self.manual_unread_hold.as_deref() == Some(id.as_str()) {
            return false;
        }
        if self.get_instance(&id).is_some_and(|i| i.is_unread()) {
            self.clear_unread_on_view(&id);
            return true;
        }
        false
    }

    /// Batch concrete row diffs; profile storage writes remain on the FIFO worker.
    pub(in crate::tui) fn bulk_apply_user_action<F>(
        &mut self,
        ids: &[String],
        mutate: F,
    ) -> anyhow::Result<super::TransactionDisposition>
    where
        F: Fn(&mut Instance),
    {
        let save = self.capture_save_snapshot();
        let mut edits = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(before) = self.get_instance(id).cloned() {
                let row = persistence_transactions::RowCapture::capture(before.clone())?;
                let mut after = before;
                mutate(&mut after);
                edits.push(persistence_transactions::MetadataEdit {
                    row,
                    after,
                    revision: 0,
                });
            }
        }
        if edits.is_empty() {
            return Ok(super::TransactionDisposition::Ignored);
        }
        for edit in &mut edits {
            edit.revision = self.record_row_edit(&edit.after.source_profile, &edit.after.id);
            self.instances
                .insert(edit.after.id.clone(), edit.after.clone());
        }
        self.enqueue_transaction(
            persistence_transactions::TransactionRequest::Metadata {
                edits,
                after: persistence_transactions::MetadataContinuation::None,
            },
            save,
        )
    }

    /// Like `mutate_instance` but fallible: applies `f` to a clone and writes back only on
    /// success, leaving the stored entry untouched on `Err`.
    pub(in crate::tui) fn try_mutate_instance<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut Instance) -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        if let Some(inst) = self.instances.get_mut(id) {
            let mut updated = inst.clone();
            let out = f(&mut updated)?;
            *inst = updated;
            return Ok(Some(out));
        }
        Ok(None)
    }

    /// Like `try_mutate_instance`, but writes the mutated clone back even on `Err`.
    ///
    /// Required for callers of `Instance::restart_with_size_opts` / `ensure_pane_ready`,
    /// whose resume path can mutate `agent_session_id`, `resume_probe_failed_sid` and
    /// `retroactive_capture_excludes` before returning `Err`. Dropping the clone there would
    /// leave live state inconsistent with disk until a later reload.
    pub(in crate::tui) fn try_mutate_instance_writeback_on_err<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut Instance) -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        if let Some(inst) = self.instances.get_mut(id) {
            let mut updated = inst.clone();
            let result = f(&mut updated);
            *inst = updated;
            return result.map(Some);
        }
        Ok(None)
    }

    pub fn set_instance_error(&mut self, id: &str, error: Option<String>) {
        self.mutate_instance(id, |inst| inst.last_error = error);
    }
}
