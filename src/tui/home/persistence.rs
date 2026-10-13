//! Namespace RPC admission and presentation-only session helpers.

use super::*;

impl HomeView {
    pub(super) fn ensure_namespace_idle(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.pending_creation.is_none() && self.creating_stub_id.is_none(), "Resolve the pending or unknown creation before changing the namespace; no change submitted");
        anyhow::ensure!(
            self.pending_native_attachment.is_none(),
            "Wait for the native continuation before changing the namespace; no change submitted"
        );
        anyhow::ensure!(self.pending_namespace_intent.is_none() && self.session_feed.can_submit_namespace(), "Namespace change waits for the current canonical state and all pending or unknown changes; no change submitted");
        Ok(())
    }

    pub(super) fn submit_namespace_intent(
        &mut self,
        mutation: crate::daemon::NamespaceMutation,
        intent: NamespaceIntent,
    ) -> anyhow::Result<()> {
        self.ensure_namespace_idle()?;
        self.session_feed.submit_namespace(mutation)?;
        self.pending_namespace_intent = Some(intent);
        Ok(())
    }

    pub(super) fn canonical_group_location(
        &self,
        path: &str,
    ) -> anyhow::Result<crate::daemon::GroupLocation> {
        anyhow::ensure!(
            self.group_by == crate::session::config::GroupByMode::Manual
                && !crate::session::is_within_archived_section(path)
                && !crate::session::is_within_trash_section(path),
            "This is not a manual namespace group"
        );
        let profile = self
            .selected_group_profile
            .clone()
            .or_else(|| self.active_profile.clone())
            .or_else(|| {
                (self.group_trees.len() == 1)
                    .then(|| self.group_trees.keys().next().unwrap().clone())
            })
            .ok_or_else(|| {
                anyhow::anyhow!("The owning profile is ambiguous; select a profile-qualified group")
            })?;
        anyhow::ensure!(
            self.group_trees
                .get(&profile)
                .is_some_and(|tree| tree.group_exists(path)),
            "The group changed elsewhere; no change submitted"
        );
        Ok(crate::daemon::GroupLocation {
            profile,
            path: path.to_owned(),
        })
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
                    // Empty headers belong only to a unique canonical profile tree.
                    let mut owners = self
                        .group_trees
                        .iter()
                        .filter(|(_, tree)| tree.group_exists(path));
                    let (owner, _) = owners.next()?;
                    return owners.next().is_none().then(|| owner.clone());
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

    /// Test-only display insertion; durable fixtures explicitly seed Storage.
    /// Insert a display-only fixture row. This helper never persists it.
    #[cfg(test)]
    pub(in crate::tui) fn add_instance(&mut self, instance: Instance) {
        self.instances.insert(instance.id.clone(), instance);
    }

    /// Mutate the display mirror only; unknown ids are ignored.
    pub(in crate::tui) fn mutate_instance(&mut self, id: &str, f: impl FnOnce(&mut Instance)) {
        if let Some(inst) = self.instances.get_mut(id) {
            f(inst);
        }
    }
}
