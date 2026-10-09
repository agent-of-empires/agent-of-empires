//! Switching the active profile.

use super::*;

impl HomeView {
    /// The active profile filter name, or `None` when no filter is applied.
    /// Returning `None` lets callers (e.g. the list-pane title) omit the
    /// `[<profile>]` segment entirely instead of rendering a noisy `[all]`.
    pub fn active_profile_display(&self) -> Option<&str> {
        self.active_profile.as_deref()
    }

    pub(in crate::tui) fn switch_profile(
        &mut self,
        new_profile: Option<String>,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let target = new_profile
            .as_deref()
            .map(|p| match self.storages.get(p) {
                Some(s) => Ok(s.clone()),
                None => Storage::open(p, self.file_watch.clone()),
            })
            .transpose()?;
        self.request_transaction(
            persistence_transactions::TransactionRequest::SwitchProfile {
                profile: new_profile,
                target,
            },
        )
    }
}
