//! Resolving an instance to its tmux session, and the options applied to it.

use super::*;

pub(super) fn tmux_env_session_name_for_instance_id(instance_id: &str) -> Option<String> {
    let output = crate::tmux::tmux_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    crate::tmux::live_any_kind_name_for_id(stdout.lines(), instance_id)
}

/// Find another session that owns the exact title and normalized path.
///
/// `exclude_id` lets mutation paths ignore the row being renamed.
pub(crate) fn find_duplicate_session<'a>(
    instances: impl IntoIterator<Item = &'a Instance>,
    title: &str,
    path: &str,
    exclude_id: Option<&str>,
) -> Option<&'a Instance> {
    let normalized_path = path.trim_end_matches('/');
    instances.into_iter().find(|inst| {
        exclude_id != Some(inst.id.as_str())
            && inst.project_path.trim_end_matches('/') == normalized_path
            && inst.title == title
    })
}

pub(crate) fn is_duplicate_session<'a>(
    instances: impl IntoIterator<Item = &'a Instance>,
    title: &str,
    path: &str,
    exclude_id: Option<&str>,
) -> bool {
    find_duplicate_session(instances, title, path, exclude_id).is_some()
}

pub(crate) fn duplicate_session_error(title: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Session already exists with same title and path: {}\n\
         Tip: use a different title or remove the existing session first",
        title
    )
}

impl Instance {
    pub fn tmux_session(&self) -> Result<tmux::Session> {
        tmux::Session::new(&self.id, &self.title)
    }

    pub(crate) fn tmux_env_session_name(&self) -> Option<String> {
        tmux_env_session_name_for_instance_id(&self.id)
    }

    /// [`Self::tmux_env_session_name`] answered from a snapshot the caller
    /// already holds, for passes that ask once per stored session.
    pub(crate) fn tmux_env_session_name_in(
        &self,
        snapshot: &crate::tmux::LiveSessionSnapshot,
    ) -> Option<String> {
        crate::tmux::live_any_kind_name_for_id_in(snapshot, &self.id)
    }

    /// [`Self::tmux_env_session_name_in`] for a one-shot pass that cannot
    /// retry: an unreachable tmux server is Unknown, not "no live pane", so
    /// fall back to a fresh per-item probe rather than dropping the row.
    ///
    /// The startup hidden-env publication in `HomeView::new` is such a pass.
    /// Nothing re-runs it on reload and a poller does not re-emit an unchanged
    /// sid, so a row skipped there stays unpublished until an unrelated sid
    /// change or a relaunch, weakening the ownership attribution
    /// `build_exclusion_set` reads. Startup recovery treats the same
    /// distinction the other way, skipping its whole pass on a failed probe
    /// rather than reading it as "every pane is dead".
    pub(crate) fn tmux_env_session_name_in_or_probe(
        &self,
        snapshot: &crate::tmux::LiveSessionSnapshot,
    ) -> Option<String> {
        match snapshot.names() {
            Some(_) => self.tmux_env_session_name_in(snapshot),
            None => self.tmux_env_session_name(),
        }
    }

    /// Whether this instance has a live tmux pane, answered from a snapshot
    /// the caller already holds. `exists()` alone is insufficient: a pane can
    /// exist while its agent has died. Used by peer exclusion, poller repair,
    /// and TUI reload.
    pub(crate) fn has_live_tmux_pane_in(
        &self,
        snapshot: &crate::tmux::LiveSessionSnapshot,
    ) -> bool {
        self.tmux_env_session_name_in(snapshot).is_some()
    }

    pub(super) fn sandbox_display(&self) -> Option<crate::tmux::status_bar::SandboxDisplay> {
        self.sandbox_info.as_ref().and_then(|s| {
            if s.enabled {
                Some(crate::tmux::status_bar::SandboxDisplay {
                    container_name: s.container_name.clone(),
                })
            } else {
                None
            }
        })
    }

    /// Apply all configured tmux options to a session with the given name and title.
    fn apply_session_tmux_options(&self, session_name: &str, display_title: &str) {
        let branch = self
            .worktree_info
            .as_ref()
            .map(|w| w.branch.as_str())
            .or_else(|| self.workspace_info.as_ref().map(|w| w.branch.as_str()));
        let sandbox = self.sandbox_display();
        crate::tmux::status_bar::apply_all_tmux_options(
            session_name,
            display_title,
            branch,
            sandbox.as_ref(),
            &self.effective_profile(),
        );
    }

    pub(super) fn apply_container_terminal_tmux_options(&self, index: u32) {
        let name =
            tmux::ContainerTerminalSession::resolve_name_indexed(&self.id, &self.title, index);
        self.apply_session_tmux_options(&name, &format!("{} (container)", self.title));
    }

    pub(super) fn apply_terminal_tmux_options(&self, index: u32) {
        let name = tmux::TerminalSession::resolve_name_indexed(&self.id, &self.title, index);
        self.apply_session_tmux_options(&name, &format!("{} (terminal)", self.title));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::test_support::EnvGuard;

    #[test]
    fn duplicate_session_normalizes_path_and_excludes_self() {
        let first = Instance::new("main", "/tmp/repo/");
        let second = Instance::new("other", "/tmp/repo");
        let instances = vec![first.clone(), second.clone()];

        assert!(is_duplicate_session(&instances, "main", "/tmp/repo", None));
        assert!(!is_duplicate_session(
            &instances,
            "main",
            "/tmp/repo/",
            Some(&first.id)
        ));
        assert!(!is_duplicate_session(
            &instances,
            "other",
            "/tmp/elsewhere",
            None
        ));
    }

    /// A one-shot pass must not read an unreachable snapshot as "no live
    /// pane". The startup hidden-env publication is batched behind one
    /// `LiveSessionSnapshot`, and nothing re-runs it, so collapsing Unknown
    /// into Absent there would leave every row's `AOE_INSTANCE_ID` and
    /// `AOE_CAPTURED_SESSION_ID` unpublished until an unrelated sid change or
    /// a relaunch, and peer exclusion reads exactly those variables.
    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn one_shot_name_probes_when_the_snapshot_missed_tmux() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let inst = Instance::new("Refactor billing", "/tmp/aoe-test-one-shot-probe");
        let live_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);

        // A `tmux` that answers with one live session name, standing in for the
        // probe that succeeds after the snapshot's own `list-sessions` failed.
        // The pane-liveness check reads the same output and parses it as "not
        // dead", which is what the real probe does for any answer but `1`.
        let shim = temp.path().join("tmux");
        std::fs::write(&shim, format!("#!/bin/sh\necho '{live_name}'\n")).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            temp.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _guard = EnvGuard::set(&[("PATH", path)]);

        let missed = crate::tmux::LiveSessionSnapshot::from_parts(None, None);
        assert_eq!(
            inst.tmux_env_session_name_in(&missed),
            None,
            "the snapshot alone has nothing to answer an unreachable server with"
        );
        assert_eq!(
            inst.tmux_env_session_name_in_or_probe(&missed).as_deref(),
            Some(live_name.as_str()),
            "a one-shot caller falls back to the per-item probe"
        );

        // A snapshot that did reach the server is authoritative: absent from
        // its list means absent, with no probe behind it.
        let observed = crate::tmux::LiveSessionSnapshot::from_parts(Some(Vec::new()), None);
        assert_eq!(inst.tmux_env_session_name_in_or_probe(&observed), None);
    }
}
