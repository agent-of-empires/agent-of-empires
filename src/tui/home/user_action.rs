//! Applying a user action to one row or to a selection, and the unread
//! bookkeeping that follows.

use super::*;

impl HomeView {
    fn user_action_ownership(&mut self) -> anyhow::Result<crate::session::storage::OwnershipGuard> {
        crate::session::storage::try_acquire_ownership_read()?.ok_or_else(|| {
            let message = "Another session operation is in progress. No changes were saved. Try again when it finishes.";
            self.info_dialog = Some(InfoDialog::new("Session operation in progress", message));
            anyhow::anyhow!(message)
        })
    }

    pub(in crate::tui) fn apply_user_action<F>(&mut self, id: &str, mutate: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut Instance),
    {
        let ownership = self.user_action_ownership()?;
        self.apply_user_action_with_ownership(&ownership, id, mutate)
    }

    pub(in crate::tui) fn apply_user_action_with_ownership<F>(
        &mut self,
        ownership: &crate::session::storage::OwnershipGuard,
        id: &str,
        mutate: F,
    ) -> anyhow::Result<()>
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
            storage.update_with_ownership(ownership, |instances, _groups| {
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

    /// Viewing clears unread when ownership is available; dwell retries contention.
    pub(crate) fn clear_unread_on_view(&mut self, id: &str) {
        if self.manual_unread_hold.as_deref() == Some(id) {
            self.manual_unread_hold = None;
        }
        let is_unread = self.get_instance(id).is_some_and(|i| i.is_unread());
        if is_unread {
            match crate::session::storage::try_acquire_ownership_read() {
                Ok(Some(ownership)) => {
                    if let Err(error) =
                        self.apply_user_action_with_ownership(&ownership, id, |i| i.mark_read())
                    {
                        tracing::warn!(target: "tui.home", %error, "Could not mark session read");
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(target: "tui.home", %error, "Could not acquire session ownership")
                }
            }
        }
    }

    /// Dwell-to-read retries while ownership is busy, without blocking input.
    /// Manual unread holds last until the cursor leaves the row.
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
            return self.get_instance(&id).is_some_and(|i| !i.is_unread());
        }
        false
    }

    /// Merge one user-owned diff per affected profile under a shared ownership lease.
    pub(in crate::tui) fn bulk_apply_user_action<F>(
        &mut self,
        ids: &[String],
        mutate: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&mut Instance),
    {
        let ownership = self.user_action_ownership()?;
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
            let res = storage.update_with_ownership(&ownership, |insts, _groups| {
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
