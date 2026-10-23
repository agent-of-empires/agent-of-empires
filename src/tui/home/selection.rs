//! Moving and revealing the cursor.

use super::*;

impl HomeView {
    fn session_row(&self, session_id: &str) -> Option<usize> {
        self.flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == session_id))
    }

    /// Select `session_id`'s row, or its group header while the `y` filter hides it, so the
    /// selection never names a session the list does not show.
    pub fn select_session_by_id(&mut self, session_id: &str) {
        if let Some(idx) = self
            .session_row(session_id)
            .or_else(|| self.header_row_for_hidden_session(session_id))
        {
            self.cursor = idx;
            self.update_selected();
        }
    }

    /// Rebuild `flat_items` and move the cursor back onto the selected session or group.
    pub(super) fn rebuild_flat_items_keeping_cursor(&mut self) {
        self.rebuild_flat_items();
        let restored = match (&self.selected_session, &self.selected_group) {
            (Some(sid), _) => self.session_row(sid),
            (None, Some(gpath)) => {
                // Two profiles can hold the same group path in the all-profiles list, and
                // landing on the wrong one leaves `selected_group_profile` pointing at the
                // other profile, which the next action would act on.
                let selected_profile = self.selected_group_profile.clone();
                self.flat_items
                    .iter()
                    .position(|item| {
                        matches!(item, Item::Group { path, profile, .. }
                            if path == gpath && *profile == selected_profile)
                    })
                    .or_else(|| {
                        self.flat_items.iter().position(
                            |item| matches!(item, Item::Group { path, .. } if path == gpath),
                        )
                    })
            }
            (None, None) => None,
        };
        let hidden_header = restored
            .is_none()
            .then(|| self.selected_session.clone())
            .flatten()
            .and_then(|sid| self.header_row_for_hidden_session(&sid));
        if let Some(header) = hidden_header {
            self.cursor = header;
            self.update_selected();
            self.context_menu = None;
            return;
        }
        match restored {
            Some(idx) => self.cursor = idx,
            None if self.cursor >= self.flat_items.len() && !self.flat_items.is_empty() => {
                self.cursor = self.flat_items.len() - 1;
            }
            None => {}
        }
    }
    pub fn sort_order(&self) -> SortOrder {
        self.sort_order
    }

    /// Move the cursor to the first session row, skipping `returning_id` unless it is the only one.
    pub fn select_top_attention(&mut self, returning_id: Option<&str>) {
        let mut sessions =
            self.flat_items
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| match item {
                    Item::Session { id, .. } => Some((idx, returning_id == Some(id.as_str()))),
                    _ => None,
                });
        let first = sessions.next();
        let pick = match first {
            Some((_, true)) => sessions.find(|(_, returning)| !returning).or(first),
            other => other,
        };
        if let Some((idx, _)) = pick {
            self.cursor = idx;
            self.update_selected();
        }
    }

    /// Reveal the original session through profile-qualified canonical expansion.
    pub fn select_and_reveal_session(&mut self, session_id: &str) {
        if let Err(error) = self.reveal_session(session_id, false) {
            self.info_dialog = Some(InfoDialog::sized_to_fit(
                "Session Not Revealed",
                &error.to_string(),
            ));
        }
    }

    pub(super) fn reveal_session(&mut self, id: &str, attach: bool) -> anyhow::Result<bool> {
        let row = self
            .session_feed
            .applied_session(id)
            .ok_or_else(|| anyhow::anyhow!("The original session is unavailable"))?;
        let intent = NamespaceIntent::Reveal {
            epoch: self
                .session_feed
                .applied_epoch()
                .unwrap_or_default()
                .to_owned(),
            profile: row.profile.clone(),
            id: row.id.clone(),
            generation: row.lifecycle_generation,
            created_at: row.created_at.clone(),
            group: row.group_path.clone(),
            attach,
        };
        self.continue_reveal(intent)
    }

    pub(super) fn continue_reveal(&mut self, intent: NamespaceIntent) -> anyhow::Result<bool> {
        let NamespaceIntent::Reveal {
            epoch,
            profile,
            id,
            generation,
            created_at,
            group,
            attach,
        } = &intent
        else {
            anyhow::bail!("No reveal is owed");
        };
        let valid = self.session_feed.applied_epoch() == Some(epoch.as_str())
            && self.session_feed.applied_session(id).is_some_and(|row| {
                row.profile == *profile
                    && row.lifecycle_generation == *generation
                    && row.created_at == *created_at
                    && row.group_path == *group
                    && row.trashed_at.is_none()
                    && row.archived_at.is_none()
            });
        if !valid {
            self.pending_created_attach = None;
            anyhow::bail!("The original session identity or runtime epoch changed; reveal and attachment cancelled");
        }
        let Some(instance) = self.get_instance(id) else {
            anyhow::bail!("Native metadata is not yet available for the original session");
        };
        match self.group_by {
            GroupByMode::Project => {
                let key = project_group_key(instance);
                self.project_group_collapsed.insert(key, false);
                self.save_project_group_collapsed();
            }
            GroupByMode::Org => {
                let key = self.org_group_key(instance);
                self.org_group_collapsed.insert(key, false);
                self.save_org_group_collapsed();
            }
            GroupByMode::Manual => {
                let path = self.session_feed.applied_snapshot().and_then(|snapshot| {
                    snapshot
                        .contents
                        .profiles
                        .iter()
                        .find(|candidate| candidate.name == *profile)
                        .and_then(|owner| {
                            owner
                                .groups
                                .iter()
                                .filter(|candidate| {
                                    candidate.collapsed
                                        && (candidate.path == *group
                                            || group
                                                .strip_prefix(candidate.path.as_str())
                                                .is_some_and(|tail| tail.starts_with('/')))
                                })
                                .min_by_key(|candidate| candidate.path.len())
                                .map(|candidate| candidate.path.clone())
                        })
                });
                if let Some(path) = path {
                    let location = crate::daemon::GroupLocation {
                        profile: profile.clone(),
                        path,
                    };
                    self.set_group_collapsed(location, false, intent)?;
                    return Ok(false);
                }
            }
        }
        self.rebuild_flat_items();
        let position = self
            .session_row(id)
            .ok_or_else(|| anyhow::anyhow!("The original session is not visible in this view"))?;
        self.cursor = position;
        self.update_selected();
        self.preview_scroll_offset = 0;
        self.manual_unread_hold = None;
        if *attach {
            self.pending_created_attach = Some((
                epoch.clone(),
                profile.clone(),
                id.clone(),
                *generation,
                created_at.clone(),
            ));
        }
        Ok(true)
    }

    pub(in crate::tui) fn take_created_attach(&mut self) -> Option<String> {
        let (epoch, profile, id, generation, created_at) = self.pending_created_attach.as_ref()?;
        let identity_matches = self.session_feed.applied_epoch() == Some(epoch.as_str())
            && self.session_feed.applied_session(id).is_some_and(|row| {
                row.profile == *profile
                    && row.lifecycle_generation == *generation
                    && row.created_at == *created_at
                    && row.trashed_at.is_none()
                    && row.archived_at.is_none()
            });
        if !self.session_feed.mutations_available() || !identity_matches {
            self.pending_created_attach = None;
            return None;
        }
        if !self.session_feed.current_snapshot_applied() {
            return None;
        }
        self.pending_created_attach
            .take()
            .map(|(_, _, id, _, _)| id)
    }

    pub(super) fn offer_retained_reveal(&mut self) {
        if self.retained_reveal.is_some() {
            self.confirm_dialog = Some(ConfirmDialog::new(
                "Reveal Not Changed", "The original reveal is retained. Retry only after reviewing the current canonical state. A changed epoch or target cancels attachment.",
                "resume_reveal",
            ).buttons("Retry", "Keep Draft"));
        }
    }
}
