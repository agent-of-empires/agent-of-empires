//! Writing session and group changes back to storage under the mutation
//! lock, including profile moves.

use super::*;

impl HomeView {
    /// Drop in-memory mirror rows that no longer exist on disk (peer-deleted via the CLI or
    /// aoe serve), rebuilding derived UI state so callers don't target removed rows.
    pub(super) fn drop_peer_deleted_rows(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let drop: HashSet<&str> = ids.iter().map(String::as_str).collect();
        self.instances.retain(|k, _| !drop.contains(k.as_str()));
        if self
            .selected_session
            .as_ref()
            .is_some_and(|s| drop.contains(s.as_str()))
        {
            self.selected_session = None;
        }
        self.rebuild_group_trees();
        self.rebuild_flat_items();
        if self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len().saturating_sub(1);
        }
    }

    /// Rebuild all per-profile GroupTrees from the current instances,
    /// preserving each tree's collapsed state.
    pub(in crate::tui) fn rebuild_group_trees(&mut self) {
        for (profile_name, tree) in &mut self.group_trees {
            let existing_groups = tree.get_all_groups();
            let profile_instances: Vec<Instance> = self
                .instances
                .values()
                .filter(|i| i.source_profile == *profile_name)
                .cloned()
                .collect();
            *tree = GroupTree::new_with_groups(&profile_instances, &existing_groups);
        }
    }

    /// Determine which profile the item at the given cursor position belongs to.
    pub(in crate::tui) fn profile_for_cursor(&self, cursor: usize) -> Option<String> {
        if let Some(profile) = &self.active_profile {
            return Some(profile.clone());
        }
        if let Some(item) = self.flat_items.get(cursor) {
            match item {
                crate::session::Item::Session { id, .. } => {
                    return self
                        .get_instance(id.as_str())
                        .map(|i| i.source_profile.clone());
                }
                crate::session::Item::Group { profile, path, .. } => {
                    if let Some(p) = profile {
                        return Some(p.clone());
                    }
                    // Fallback for single-profile mode: find any instance in this group
                    return self
                        .instances
                        .values()
                        .find(|i| {
                            i.group_path == *path || i.group_path.starts_with(&format!("{}/", path))
                        })
                        .map(|i| i.source_profile.clone());
                }
            }
        }
        None
    }

    /// Collect all groups from all per-profile GroupTrees.
    pub(in crate::tui) fn all_groups(&self) -> Vec<Group> {
        self.group_trees
            .values()
            .flat_map(|t| t.get_all_groups())
            .collect()
    }

    /// Check if any profile has groups, without collecting them all.
    pub(in crate::tui) fn has_any_groups(&self) -> bool {
        self.group_trees
            .values()
            .any(|t| !t.get_all_groups().is_empty())
    }

    /// Centralized instance addition: inserts into the ordered map (insertion order is
    /// sidebar order) and records the id in `pending_added`, so the next `save`
    /// distinguishes TUI-new rows from peer-deleted ones, which look identical on disk.
    pub(in crate::tui) fn add_instance(&mut self, instance: Instance) {
        // Count only finalized session inserts for the opt-in create-trend counter (#1897).
        // `add_instance` is also the funnel for `Creating` stubs, so counting every call
        // would double-count a successful background create and count a cancelled one. A
        // real create is never `Creating`. Mirrors the serve side's single increment.
        if instance.status != crate::session::Status::Creating {
            crate::tui::app::record_session_create();
        }
        let token = self.record_row_edit(&instance.source_profile, &instance.id);
        self.pending_added
            .entry(instance.source_profile.clone())
            .or_default()
            .insert(instance.id.clone(), token);
        self.instances.insert(instance.id.clone(), instance);
    }

    /// Publish a row this process already committed through `Storage::update`. Unlike a
    /// provisional TUI add, a later missing disk row is a peer deletion and must not be
    /// recreated by `request_save` acknowledgement.
    pub(super) fn publish_persisted_instance(&mut self, instance: Instance) {
        let profile = instance.source_profile.clone();
        let id = instance.id.clone();
        self.add_instance(instance);
        let remove_profile_entry = self.pending_added.get_mut(&profile).is_some_and(|pending| {
            pending.remove(&id);
            pending.is_empty()
        });
        if remove_profile_entry {
            self.pending_added.remove(&profile);
        }
    }

    /// Centralized instance mutation: applies `f` in place, a no-op on unknown ids so
    /// callers can be idempotent (matching `remove_instance`).
    pub(in crate::tui) fn mutate_instance(&mut self, id: &str, f: impl FnOnce(&mut Instance)) {
        if let Some(inst) = self.instances.get_mut(id) {
            f(inst);
            let profile = inst.source_profile.clone();
            self.record_row_edit(&profile, id);
        }
    }

    /// Queue a passive observation on its original row; the daemon owns structured status.
    pub(in crate::tui) fn persist_passive_status_transition(
        &mut self,
        id: &str,
        mark_unread: bool,
    ) {
        let Some(row) = self
            .get_instance(id)
            .filter(|r| !r.is_structured())
            .cloned()
        else {
            return;
        };
        let result = persistence_transactions::RowCapture::capture(row).and_then(|row| {
            self.request_transaction(persistence_transactions::TransactionRequest::Passive {
                row,
                mark_unread,
            })
        });
        if let Err(error) = result {
            tracing::warn!(target:"session.store",session_id=%id,"Passive observation was not queued: {error:#}");
        }
    }
}
