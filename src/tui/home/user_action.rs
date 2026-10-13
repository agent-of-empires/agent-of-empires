//! Applying a user action to one row or to a selection, and the unread
//! bookkeeping that follows.

use super::*;

impl HomeView {
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
}
