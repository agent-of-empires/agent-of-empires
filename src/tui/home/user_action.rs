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

    /// Queue a read intent after engagement. Canonical feedback clears the row.
    pub(crate) fn clear_unread_on_view(&mut self, id: &str) -> bool {
        if self.manual_unread_hold.as_deref() == Some(id) {
            self.manual_unread_hold = None;
        }
        if self.get_instance(id).is_some_and(|i| i.is_unread()) && self.session_feed.can_submit(id)
        {
            return self
                .session_feed
                .submit(
                    id.to_owned(),
                    crate::daemon::SessionMutation::Unread(crate::daemon::UpdateUnreadBody {
                        unread: false,
                    }),
                )
                .is_ok();
        }
        false
    }

    /// Queue a read after a foreground dwell, except for a manually marked visit.
    /// Returns whether a request was admitted, not whether the row changed.
    pub fn tick_unread_dwell(&mut self, now: std::time::Instant) -> bool {
        if !crate::session::unread_enabled() || self.has_dialog() {
            self.unread_dwell = None;
            return false;
        }
        let Some(id) = self.selected_session.clone() else {
            self.unread_dwell = None;
            return false;
        };
        if self
            .manual_unread_hold
            .as_deref()
            .is_some_and(|held| held != id)
        {
            self.manual_unread_hold = None;
        }
        let started = match &self.unread_dwell {
            Some((prev, started)) if *prev == id => *started,
            _ => {
                self.unread_dwell = Some((id, now));
                return false;
            }
        };
        if now.duration_since(started) < UNREAD_DWELL {
            return false;
        }
        if self.manual_unread_hold.as_deref() == Some(id.as_str()) {
            return false;
        }
        self.clear_unread_on_view(&id)
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
}
