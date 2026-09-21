//! Writing session and group changes back to storage under the mutation
//! lock, including profile moves.

use super::*;

impl HomeView {
    pub fn save(&mut self) -> anyhow::Result<()> {
        let mut all_peer_deleted: Vec<String> = Vec::new();

        for (profile_name, storage) in &self.storages {
            let tui_rows: Vec<Instance> = self
                .instances()
                .filter(|instance| instance.source_profile == *profile_name)
                .filter(|instance| self.creating_stub_id.as_deref() != Some(instance.id.as_str()))
                .cloned()
                .collect();
            let dels: HashSet<String> = self
                .pending_deletions
                .get(profile_name)
                .cloned()
                .unwrap_or_default();
            let added: HashSet<String> = self
                .pending_added
                .get(profile_name)
                .cloned()
                .unwrap_or_default();
            let group_dels: HashSet<String> = self
                .pending_group_deletions
                .get(profile_name)
                .cloned()
                .unwrap_or_default();
            let groups_target = self
                .group_trees
                .get(profile_name)
                .map(|t| t.get_all_groups())
                .unwrap_or_default();

            let peer_deleted: Vec<String> = storage.update(|disk_instances, disk_groups| {
                disk_instances.retain(|d| !dels.contains(&d.id));
                let mut peer_deleted: Vec<String> = Vec::new();
                for tui_inst in &tui_rows {
                    if let Some(disk_inst) = disk_instances.iter_mut().find(|d| d.id == tui_inst.id)
                    {
                        let durable_status = disk_inst.status;
                        disk_inst.merge_from_tui(tui_inst);
                        if tui_inst.status == crate::session::Status::Deleting {
                            disk_inst.status = durable_status;
                        }
                    } else if added.contains(&tui_inst.id) {
                        if tui_inst.status != crate::session::Status::Deleting {
                            disk_instances.push(tui_inst.clone());
                        }
                    } else {
                        // Disk had no row with this id and we did not add it
                        // this session: a peer (CLI / aoe serve) removed it.
                        peer_deleted.push(tui_inst.id.clone());
                    }
                }
                disk_groups.retain(|g| !group_dels.contains(&g.path));
                for tui_g in &groups_target {
                    if let Some(disk_g) = disk_groups.iter_mut().find(|g| g.path == tui_g.path) {
                        disk_g.name = tui_g.name.clone();
                        disk_g.collapsed = tui_g.collapsed;
                        disk_g.archived_at = tui_g.archived_at;
                    } else {
                        disk_groups.push(tui_g.clone());
                    }
                }
                Ok(peer_deleted)
            })?;

            self.pending_deletions.remove(profile_name);
            self.pending_group_deletions.remove(profile_name);
            self.pending_added.remove(profile_name);
            all_peer_deleted.extend(peer_deleted);
        }

        if !all_peer_deleted.is_empty() {
            self.drop_peer_deleted_rows(&all_peer_deleted);
            tracing::info!(
                target: "tui.home",
                count = all_peer_deleted.len(),
                "Dropped peer-deleted rows from in-memory mirror"
            );
        }
        Ok(())
    }

    /// Drop in-memory mirror rows that no longer exist on disk (peer-deleted
    /// via CLI / aoe serve). Rebuilds derived UI state so callers don't
    /// render or target removed rows.
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
                crate::session::Item::LocalGroup { .. }
                | crate::session::Item::RemoteGroup { .. }
                | crate::session::Item::RemoteSession { .. } => return None,
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

    /// Test fixture helper: insert a row and register it as TUI-added so a
    /// following `save` persists it. Production row arrival is daemon-owned.
    #[cfg(test)]
    pub(in crate::tui) fn add_instance(&mut self, instance: Instance) {
        self.pending_added
            .entry(instance.source_profile.clone())
            .or_default()
            .insert(instance.id.clone());
        self.instances.insert(instance.id.clone(), instance);
    }

    /// Centralized instance removal: shift-removes from the ordered map
    /// (preserves the order of trailing rows; swap_remove would silently
    /// reorder the sidebar), records the id in `pending_deletions` so the
    /// next `save` propagates the removal under the flock, and clears any
    /// `pending_added` entry so an add+remove in the same save cycle does
    /// not end up persisted. Idempotent: safe to call on ids already
    /// removed.
    pub(in crate::tui) fn remove_instance(&mut self, id: &str) {
        if let Some(inst) = self.instances.get(id) {
            let profile = inst.source_profile.clone();
            self.pending_deletions
                .entry(profile.clone())
                .or_default()
                .insert(id.to_string());
            if let Some(set) = self.pending_added.get_mut(&profile) {
                set.remove(id);
            }
        }
        self.instances.shift_remove(id);
    }

    /// Tombstones `path` and every descendant from the per-profile tree so
    /// `save()` drops them under the flock instead of wholesale-replacing.
    pub(in crate::tui) fn delete_group_in_profile(&mut self, profile: &str, path: &str) {
        let prefix = format!("{}/", path);
        let descendants: Vec<String> = self
            .group_trees
            .get(profile)
            .map(|tree| {
                tree.get_all_groups()
                    .into_iter()
                    .filter(|g| g.path == path || g.path.starts_with(&prefix))
                    .map(|g| g.path)
                    .collect()
            })
            .unwrap_or_else(|| vec![path.to_string()]);
        if let Some(tree) = self.group_trees.get_mut(profile) {
            tree.delete_group(path);
        }
        self.pending_group_deletions
            .entry(profile.to_string())
            .or_default()
            .extend(descendants);
    }

    /// Centralized instance mutation: applies `f` to the entry in place.
    /// No-op on unknown ids so callers can be idempotent (matches
    /// `remove_instance`).
    pub(in crate::tui) fn mutate_instance(&mut self, id: &str, f: impl FnOnce(&mut Instance)) {
        if let Some(inst) = self.instances.get_mut(id) {
            f(inst);
        }
    }

    /// Acquire the per-session title flock followed by the source profile's
    /// per-instance lifecycle flock, then replace the TUI snapshot with the
    /// authoritative source row. Only TUI-owned launch configuration is merged
    /// from the snapshot; lifecycle/runtime fields always remain the values
    /// reloaded under the source lifecycle lock.
    pub(in crate::tui) fn lock_session_mutation_and_reload(
        &mut self,
        id: &str,
    ) -> anyhow::Result<SessionMutationGuards> {
        let snapshot = self
            .instances
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
        let source_profile = snapshot.source_profile.clone();
        let session_title = crate::session::acquire_session_title_lock(id)
            .map_err(|error| anyhow::anyhow!("failed to acquire session title lock: {error}"))?;
        if !self.storages.contains_key(&source_profile) {
            self.storages.insert(
                source_profile.clone(),
                Storage::open(&source_profile, self.file_watch.clone())?,
            );
        }
        let storage = self
            .storages
            .get(&source_profile)
            .expect("source storage was registered above");
        let lifecycle = storage
            .acquire_instance_lifecycle_lock(id)
            .map_err(|error| {
                anyhow::anyhow!("failed to acquire session lifecycle lock: {error}")
            })?;
        // Read-only: both flocks (title + lifecycle) are already held above, so
        // a plain `load()` gives the authoritative on-disk row without going
        // through `update()`, which would unconditionally rewrite sessions.json
        // and fire a `notify_local_change` even when nothing changed. Callers
        // like `move_group_to_profile` acquire these guards per member in a
        // loop, so an update-per-read would be N identical rewrites.
        let mut authoritative = storage
            .load()?
            .into_iter()
            .find(|instance| instance.id == id)
            .ok_or_else(|| anyhow::anyhow!("Session not found in source profile: {id}"))?;
        let authoritative_generation = authoritative.lifecycle_generation;
        let authoritative_status = authoritative.status;
        let authoritative_idle_entered_at = authoritative.idle_entered_at;
        let authoritative_last_accessed_at = authoritative.last_accessed_at;
        authoritative.merge_runtime_from_reload(&snapshot);
        authoritative.merge_from_tui(&snapshot);
        authoritative.source_profile.clone_from(&source_profile);
        authoritative.lifecycle_generation = authoritative_generation;
        authoritative.status = authoritative_status;
        authoritative.idle_entered_at = authoritative_idle_entered_at;
        authoritative.last_accessed_at =
            authoritative_last_accessed_at.max(snapshot.last_accessed_at);
        self.instances.insert(id.to_string(), authoritative);
        Ok(SessionMutationGuards {
            _session_title: session_title,
            _lifecycle: lifecycle,
        })
    }

    /// Cross-profile move: structurally distinct from `mutate_instance`
    /// because the source row and group metadata must be removed in the same
    /// transaction that durably publishes the target row and metadata.
    ///
    /// `before_commit` runs after authoritative target validation while both
    /// profile storage locks are held. It may perform only bounded
    /// worktree/container effects; it must not re-enter storage or rekey tmux.
    /// Callers retain [`SessionMutationGuards`] around the transaction and any
    /// post-persist tmux rekey.
    pub(in crate::tui) fn move_to_profile_with_effect<B>(
        &mut self,
        id: &str,
        target: &str,
        mut requested: Instance,
        baseline: Option<&Instance>,
        account_swap: bool,
        before_commit: B,
    ) -> anyhow::Result<()>
    where
        B: FnOnce(&Instance) -> anyhow::Result<()>,
    {
        let Some(current) = self.instances.get(id).cloned() else {
            return Ok(());
        };
        let lifecycle_reserved = current.has_fresh_lifecycle_reservation(chrono::Utc::now());
        let before = baseline.cloned().unwrap_or_else(|| current.clone());
        let old_profile = before.source_profile.clone();
        requested.source_profile = old_profile.clone();
        if old_profile == target {
            requested.source_profile = target.to_string();
            self.instances.insert(id.to_string(), requested);
            return Ok(());
        }
        anyhow::ensure!(
            current.status != crate::session::Status::Creating,
            "Cannot move session {id} between profiles while it is being created"
        );
        anyhow::ensure!(
            !lifecycle_reserved,
            "Cannot move session {id} between profiles while a lifecycle operation is in progress"
        );

        if !self.storages.contains_key(target) {
            self.storages.insert(
                target.to_string(),
                Storage::open(target, self.file_watch.clone())?,
            );
        }
        let source = self
            .storages
            .get(&old_profile)
            .ok_or_else(|| anyhow::anyhow!("Source profile storage is not loaded"))?;
        let target_storage = self
            .storages
            .get(target)
            .ok_or_else(|| anyhow::anyhow!("Target profile storage is not loaded"))?;
        let mut moved = source.move_instance_to_with_effect(
            target_storage,
            &before,
            &requested,
            account_swap,
            |instances, candidate| {
                if crate::session::is_duplicate_session(
                    instances.iter(),
                    &candidate.title,
                    &candidate.project_path,
                    None,
                ) {
                    return Err(crate::session::duplicate_session_error(&candidate.title));
                }
                Ok(())
            },
            before_commit,
        )?;
        moved.merge_runtime_for_profile_move(&current);
        moved.source_profile = target.to_string();
        self.instances.insert(id.to_string(), moved);
        Ok(())
    }

    /// Reload storage after a profile move without dropping runtime-only state
    /// from the rows the transaction just published.
    pub(in crate::tui) fn reload_preserving_profile_move_runtime(
        &mut self,
        ids: &[String],
    ) -> anyhow::Result<()> {
        let previous: Vec<(String, Instance)> = ids
            .iter()
            .filter_map(|id| {
                self.instances
                    .get(id)
                    .cloned()
                    .map(|instance| (id.clone(), instance))
            })
            .collect();
        self.reload()?;
        for (id, prior) in previous {
            if let Some(reloaded) = self.instances.get_mut(&id) {
                reloaded.merge_runtime_for_profile_move(&prior);
            }
        }
        Ok(())
    }
}
