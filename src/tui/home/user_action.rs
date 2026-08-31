//! Applying a user action to one row or to a selection, and the unread
//! bookkeeping that follows.

use super::*;

impl HomeView {
    /// Atomic per-action mutate: update memory once, then merge the user-owned
    /// diff under the storage flock. Roll memory back if persistence fails.
    pub(in crate::tui) fn apply_user_action<F>(&mut self, id: &str, mutate: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut Instance),
    {
        let Some(profile) = self
            .instances
            .get(id)
            .map(|instance| instance.source_profile.clone())
        else {
            return Ok(());
        };
        let Some(in_memory) = self.instances.get_mut(id) else {
            return Ok(());
        };
        let before = in_memory.clone();
        mutate(in_memory);
        let after = in_memory.clone();

        let id_owned = id.to_string();
        let result = if let Some(storage) = self.storages.get(&profile) {
            storage.update(|instances, _groups| {
                if let Some(disk) = instances
                    .iter_mut()
                    .find(|instance| instance.id == id_owned)
                {
                    disk.merge_user_action_diff(&before, &after);
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
        } else {
            tracing::warn!(
                target: "tui.home",
                profile = %profile,
                id = %id_owned,
                "apply_user_action: no storage registered for profile; in-memory mutation will not persist"
            );
            Ok(true)
        };
        match result {
            Ok(true) => Ok(()),
            Ok(false) => {
                let added = self
                    .pending_added
                    .get(&profile)
                    .is_some_and(|pending| pending.contains(id));
                if !added {
                    self.drop_peer_deleted_rows(&[id.to_string()]);
                }
                Ok(())
            }
            Err(error) => {
                if let Some(slot) = self.instances.get_mut(id) {
                    *slot = before;
                }
                Err(error)
            }
        }
    }

    /// Clear the unread marker because the user engaged with the session
    /// (Tab into live-send, Enter to attach, or dwell on it in the list).
    /// Runs regardless of the feature flag so a stale marker can't survive a
    /// disable/re-enable and reappear later; only writes when the session is
    /// actually unread, so an already-read session doesn't churn the storage
    /// flock.
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

    /// Dwell-to-read: clear the selected session's unread marker once it has
    /// stayed selected, with the list in the foreground, for `UNREAD_DWELL`.
    /// This is what separates "scrolled past it" from "stopped to read it."
    /// Driven from the app tick loop; returns true when it cleared a marker
    /// (so the caller can request a redraw).
    ///
    /// The clock is suspended (and reset) whenever the feature is off, a
    /// dialog or live-send is up (the list isn't being read then), or nothing
    /// is selected, and it restarts whenever the selection moves to a
    /// different row. A row the user just flagged unread by hand is held until
    /// the cursor leaves it (`manual_unread_hold`), so flagging it and sitting
    /// there doesn't instantly undo the mark; once you leave and come back, it
    /// clears on dwell like any other unread row.
    pub fn tick_unread_dwell(&mut self, now: std::time::Instant) -> bool {
        if !crate::session::unread_enabled() || self.has_dialog() {
            self.unread_dwell = None;
            return false;
        }
        let Some(id) = self.selected_session.clone() else {
            self.unread_dwell = None;
            return false;
        };
        // The manual hold only protects the row while it stays selected; the
        // moment the cursor moves elsewhere, release it so a later return reads
        // normally.
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
        // A row the user just flagged by hand is held for this visit, so the
        // dwell doesn't undo the mark while they sit on it. The clock stays
        // parked on this row either way so we don't re-evaluate every tick.
        if self.manual_unread_hold.as_deref() == Some(id.as_str()) {
            return false;
        }
        if self.get_instance(&id).is_some_and(|i| i.is_unread()) {
            self.clear_unread_on_view(&id);
            return true;
        }
        false
    }

    /// Bulk `apply_user_action`: one `Storage::update` per affected
    /// profile (single flock cycle), grouping ids by `source_profile`.
    pub(in crate::tui) fn bulk_apply_user_action<F>(
        &mut self,
        ids: &[String],
        mutate: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&mut Instance),
    {
        let mut by_profile: HashMap<String, Vec<(String, Instance, Instance)>> = HashMap::new();
        for id in ids {
            let Some(inst) = self.instances.get_mut(id) else {
                continue;
            };
            let pre = inst.clone();
            mutate(inst);
            let post = inst.clone();
            by_profile
                .entry(post.source_profile.clone())
                .or_default()
                .push((id.clone(), pre, post));
        }
        let mut peer_deleted: Vec<String> = Vec::new();
        for (profile, items) in by_profile {
            let Some(storage) = self.storages.get(&profile) else {
                tracing::warn!(
                    target: "tui.home",
                    profile = %profile,
                    count = items.len(),
                    "bulk_apply_user_action: no storage registered for profile; in-memory mutations will not persist"
                );
                continue;
            };
            let added: HashSet<String> = self
                .pending_added
                .get(&profile)
                .cloned()
                .unwrap_or_default();
            let res = storage.update(|insts, _groups| {
                let mut missing: Vec<String> = Vec::new();
                for (id, pre, post) in &items {
                    if let Some(disk) = insts.iter_mut().find(|i| i.id == *id) {
                        disk.merge_user_action_diff(pre, post);
                    } else if !added.contains(id) {
                        missing.push(id.clone());
                    }
                }
                Ok(missing)
            });
            match res {
                Ok(missing) => peer_deleted.extend(missing),
                Err(e) => {
                    for (id, pre, _post) in items {
                        if let Some(slot) = self.instances.get_mut(&id) {
                            *slot = pre;
                        }
                    }
                    return Err(e);
                }
            }
        }
        if !peer_deleted.is_empty() {
            self.drop_peer_deleted_rows(&peer_deleted);
        }
        Ok(())
    }

    /// Like `mutate_instance`, but for fallible operations. Clones the entry,
    /// applies `f` to the clone, and writes back only on success; the stored
    /// entry is left untouched on `Err`.
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

    /// Like `try_mutate_instance`, but writes the mutated clone back even
    /// when `f` returns `Err`.
    ///
    /// Required for callers of `Instance::restart_with_size_opts` /
    /// `ensure_pane_ready`, because the resume path can mutate
    /// `agent_session_id`, `resume_probe_failed_sid`, and
    /// `retroactive_capture_excludes` before returning `Err`. The default
    /// `try_mutate_instance` drops the mutated clone on `Err`, leaving live
    /// state inconsistent with disk until a later reload. This helper keeps
    /// the live state consistent with the attempted restart.
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
