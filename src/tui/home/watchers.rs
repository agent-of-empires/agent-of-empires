//! The disk and config watchers the view owns, and how a failed reload is surfaced.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};

use super::*;

/// Keeps a profile literally named `"<global>"` apart from the app-wide config subscription.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::tui) enum ConfigWatchKey {
    Global,
    Profile(String),
}

impl ConfigWatchKey {
    pub(super) fn profile(name: &str) -> Self {
        Self::Profile(name.to_string())
    }
}

pub(super) const RELOAD_FAILED_TITLE: &str = "Reload Failed";

pub(super) const WATCHER_WARNING_TITLE: &str = "Watcher Warning";

/// Watcher-driven refreshes stay silent; interactive ones may surface dialogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum ConfigRefreshOrigin {
    Interactive,
    Watcher,
}

/// Eligible, unseen tip count for the home-view badge, honoring `session.show_tips`.
pub(in crate::tui) fn tips_unseen_count(config: &crate::session::Config) -> usize {
    if !config.session.show_tips {
        return 0;
    }
    crate::tips::unseen_count(
        crate::tips::TipSurface::Tui,
        &config.app_state.tips_seen,
        &crate::tips::TipSignals::from(config),
    )
}

/// A subscription plus the task forwarding its events into a dirty latch.
pub(in crate::tui) struct DiskWatchEntry {
    handle: crate::file_watch::SubscriptionHandle,
    forwarder: tokio::task::AbortHandle,
    canonical_dir: PathBuf,
    /// `(dev, ino, btime)` at install: a recreated dir keeps its path and often its inode.
    pub(super) installed_identity: crate::file_watch::WatchIdentity,
}

impl DiskWatchEntry {
    /// Whether the watched dir was deleted or recreated since install; notify watches don't reattach.
    fn invalidated(&self, current_dir: Option<PathBuf>) -> bool {
        if self.handle.installation_retry_due() {
            return true;
        }
        match current_dir.and_then(|p| std::fs::canonicalize(p).ok()) {
            Some(canonical) => {
                canonical != self.canonical_dir
                    || crate::file_watch::capture_watch_identity(&canonical)
                        .is_ok_and(|id| id != self.installed_identity)
            }
            None => true,
        }
    }

    fn installation_failure(&self, profile: Option<String>) -> Option<WatcherInitError> {
        self.handle
            .installation_failure()
            .map(|failure| WatcherInitError {
                profile,
                kind: WatcherInitErrorKind::Watch(failure.kind),
                message: failure.message,
            })
    }
}

/// Drop the handle before aborting, so the channel closes before the forwarder dies.
pub(super) fn drop_disk_watch_entry(entry: DiskWatchEntry) {
    drop(entry.handle);
    entry.forwarder.abort();
}

fn subscribe(
    task_name: &'static str,
    file_watch: &Arc<FileWatchService>,
    spec: WatchSpec,
    capacity: usize,
    latches: &[Arc<AtomicBool>],
    span: tracing::Span,
) -> Result<DiskWatchEntry, crate::file_watch::WatchError> {
    use tracing::Instrument;
    let dir = spec.dir.clone();
    let (mut rx, handle) = file_watch.subscribe_channel_deferred(spec, capacity)?;
    let latches = latches.to_vec();
    let join = crate::task_util::spawn_supervised(
        task_name,
        crate::task_util::PanicPolicy::Log,
        async move {
            while rx.recv().await.is_some() {
                for latch in &latches {
                    latch.store(true, Ordering::Release);
                }
            }
        }
        .instrument(span),
    );
    let canonical_dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    Ok(DiskWatchEntry {
        handle,
        forwarder: join.abort_handle(),
        installed_identity: crate::file_watch::capture_watch_identity(&canonical_dir)
            .unwrap_or_default(),
        canonical_dir,
    })
}

fn watch_spec(dir: &Path, matcher: FileMatcher, debounce_ms: u64) -> WatchSpec {
    WatchSpec {
        dir: dir.to_path_buf(),
        matcher,
        debounce: Some(Duration::from_millis(debounce_ms)),
    }
}

/// Profile dir for a new subscription; `None` (logged) when it can't be resolved or is gone.
/// Uses the non-creating resolver so a peer-deleted profile isn't resurrected.
fn existing_profile_dir(name: &str, what: &str) -> Option<PathBuf> {
    match crate::session::get_profile_dir_path(name) {
        Ok(dir) if dir.exists() => Some(dir),
        Ok(_) => {
            tracing::debug!(target: "tui.file_watch", profile = %name, "skipping {what} subscribe; profile dir absent");
            None
        }
        Err(e) => {
            tracing::warn!(target: "tui.file_watch", profile = %name, error = %e, "skipping {what} subscribe; profile dir resolution failed");
            None
        }
    }
}

/// `(to_remove, to_add, invalidated)` for a set of profile-keyed entries.
fn diff_profiles<'a>(
    prior: impl Iterator<Item = (&'a String, &'a DiskWatchEntry)>,
    current: &[String],
) -> (Vec<String>, Vec<String>, bool) {
    let mut to_remove = Vec::new();
    let mut invalidated = HashSet::new();
    let mut prior_names = HashSet::new();
    for (name, entry) in prior {
        prior_names.insert(name.clone());
        let stale = entry.invalidated(crate::session::get_profile_dir_path(name).ok());
        if stale {
            invalidated.insert(name.clone());
        }
        if stale || !current.contains(name) {
            to_remove.push(name.clone());
        }
    }
    let to_add = current
        .iter()
        .filter(|n| !prior_names.contains(*n) || invalidated.contains(*n))
        .cloned()
        .collect();
    (to_remove, to_add, !invalidated.is_empty())
}

/// Per-profile `sessions.json`/`groups.json` subscriptions and the latch their forwarders set.
pub(in crate::tui) struct DiskWatchState {
    pub(in crate::tui) dirty: Arc<AtomicBool>,
    pub(in crate::tui) handles: HashMap<String, DiskWatchEntry>,
}

/// Global and per-profile `config.toml` subscriptions and the latch their forwarders set.
pub(in crate::tui) struct ConfigWatchState {
    pub(in crate::tui) dirty: Arc<AtomicBool>,
    /// Set with `dirty` but taken even during live-send, which defers the full
    /// reload: a removed `session.preview_link_prefixes` entry stops at once.
    pub(in crate::tui) link_policy_dirty: Arc<AtomicBool>,
    pub(in crate::tui) handles: HashMap<ConfigWatchKey, DiskWatchEntry>,
}

impl DiskWatchState {
    /// Set-diff the subscriptions against `current`, rebuilding entries whose dir was recreated.
    pub(in crate::tui) fn rewire(
        &mut self,
        file_watch: &Arc<FileWatchService>,
        current: &[String],
        reload_failure: &mut ReloadFailureState,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let (to_remove, to_add, any_invalidated) = diff_profiles(self.handles.iter(), current);
        let mut new_init_error = self
            .handles
            .iter()
            .filter(|(name, _)| current.contains(name))
            .find_map(|(name, entry)| entry.installation_failure(Some(name.clone())));
        if to_remove.is_empty()
            && to_add.is_empty()
            && new_init_error.is_none()
            && reload_failure.disk_watcher_init_error.is_none()
            && !reload_failure.disk_watcher_init_error_references_missing_profile(current)
        {
            return;
        }

        // Applied once per pass so a recurring identical failure doesn't re-arm the dialog.
        for name in &to_remove {
            if let Some(entry) = self.handles.remove(name) {
                drop_disk_watch_entry(entry);
            }
        }
        for name in &to_add {
            let Some(dir) = existing_profile_dir(name, "disk") else {
                continue;
            };
            let matcher =
                FileMatcher::AnyOf(vec![dir.join("sessions.json"), dir.join("groups.json")]);
            let span = tracing::debug_span!("tui.disk_watch.forwarder", profile = %name);
            let name_ = "tui.disk_watch.forwarder";
            let spec = watch_spec(&dir, matcher, 75);
            match subscribe(
                name_,
                file_watch,
                spec,
                16,
                std::slice::from_ref(&self.dirty),
                span,
            ) {
                Ok(entry) => {
                    self.handles.insert(name.clone(), entry);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        profile = %name,
                        error = %e,
                        "subscribe_channel failed; falling back to 5s heartbeat for this profile"
                    );
                    new_init_error = Some(WatcherInitError {
                        profile: Some(name.clone()),
                        kind: WatcherInitErrorKind::Watch(e.kind()),
                        message: e.to_string(),
                    });
                }
            }
        }
        let stored_error_pending = reload_failure
            .disk_watcher_init_error
            .as_ref()
            .and_then(|error| error.profile.as_ref())
            .and_then(|profile| self.handles.get(profile))
            .is_some_and(|entry| entry.handle.installation_pending());
        if new_init_error.is_some()
            || reload_failure.disk_watcher_init_error_references_missing_profile(current)
            || !stored_error_pending
        {
            reload_failure.apply_disk_watcher_init_pass(new_init_error);
        }
        tracing::debug!(
            target: "tui.file_watch",
            added = ?to_add,
            removed = ?to_remove,
            "reconciled per-profile disk-watch subscriptions"
        );
        // A write during the dead-watch window produced no event, so force a reload.
        if any_invalidated {
            self.dirty.store(true, Ordering::Release);
        }
    }
}

impl ConfigWatchState {
    fn latches(&self) -> [Arc<AtomicBool>; 2] {
        [Arc::clone(&self.dirty), Arc::clone(&self.link_policy_dirty)]
    }

    /// Keep the global subscription (rebuilt only if the app dir was recreated) and set-diff the
    /// per-profile ones against `current`.
    pub(in crate::tui) fn rewire(
        &mut self,
        file_watch: &Arc<FileWatchService>,
        current: &[String],
        reload_failure: &mut ReloadFailureState,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        let mut new_init_error = self.handles.iter().find_map(|(key, entry)| {
            let profile = match key {
                ConfigWatchKey::Global => None,
                ConfigWatchKey::Profile(name) if !current.contains(name) => return None,
                ConfigWatchKey::Profile(name) => Some(name.clone()),
            };
            entry.installation_failure(profile)
        });
        let global_invalidated = self
            .handles
            .get(&ConfigWatchKey::Global)
            .is_some_and(|entry| entry.invalidated(crate::session::get_app_dir().ok()));
        if global_invalidated {
            if let Some(entry) = self.handles.remove(&ConfigWatchKey::Global) {
                drop_disk_watch_entry(entry);
            }
        }
        let global_needs_install = !self.handles.contains_key(&ConfigWatchKey::Global);

        let (to_remove, to_add, any_invalidated) = diff_profiles(
            self.handles.iter().filter_map(|(key, entry)| match key {
                ConfigWatchKey::Global => None,
                ConfigWatchKey::Profile(name) => Some((name, entry)),
            }),
            current,
        );

        if !global_needs_install
            && to_remove.is_empty()
            && to_add.is_empty()
            && new_init_error.is_none()
            && reload_failure.config_watcher_init_error.is_none()
            && !reload_failure.config_watcher_init_error_references_missing_profile(current)
        {
            return;
        }

        if global_needs_install {
            match crate::session::get_app_dir() {
                Ok(app_dir) => {
                    let matcher = FileMatcher::Exact(app_dir.join("config.toml"));
                    let span = tracing::debug_span!("tui.config_watch.global.forwarder");
                    let name_ = "tui.config_watch.global.forwarder";
                    let spec = watch_spec(&app_dir, matcher, 100);
                    match subscribe(name_, file_watch, spec, 4, &self.latches(), span) {
                        Ok(entry) => {
                            self.handles.insert(ConfigWatchKey::Global, entry);
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "tui.file_watch",
                                error = %e,
                                "global config subscribe_channel failed; \
                                 falling back to settings-close + profile-switch reload"
                            );
                            new_init_error = Some(WatcherInitError {
                                profile: None,
                                kind: WatcherInitErrorKind::Watch(e.kind()),
                                message: e.to_string(),
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        error = %e,
                        "skipping global config subscribe; app dir resolution failed"
                    );
                    new_init_error = Some(WatcherInitError {
                        profile: None,
                        kind: WatcherInitErrorKind::Resolution,
                        message: format!("app dir resolution failed: {e}"),
                    });
                }
            }
        }

        for name in &to_remove {
            if let Some(entry) = self.handles.remove(&ConfigWatchKey::profile(name)) {
                drop_disk_watch_entry(entry);
            }
        }
        for name in &to_add {
            let Some(dir) = existing_profile_dir(name, "config") else {
                continue;
            };
            let matcher = FileMatcher::Exact(dir.join("config.toml"));
            let span = tracing::debug_span!("tui.config_watch.profile.forwarder", profile = %name);
            let name_ = "tui.config_watch.profile.forwarder";
            let spec = watch_spec(&dir, matcher, 100);
            match subscribe(name_, file_watch, spec, 4, &self.latches(), span) {
                Ok(entry) => {
                    self.handles.insert(ConfigWatchKey::profile(name), entry);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        profile = %name,
                        error = %e,
                        "config subscribe_channel failed; \
                         falling back to settings-close + profile-switch reload for this profile"
                    );
                    new_init_error = Some(WatcherInitError {
                        profile: Some(name.clone()),
                        kind: WatcherInitErrorKind::Watch(e.kind()),
                        message: e.to_string(),
                    });
                }
            }
        }
        let stored_error_pending = reload_failure
            .config_watcher_init_error
            .as_ref()
            .and_then(|error| {
                let key = error
                    .profile
                    .as_deref()
                    .map_or(ConfigWatchKey::Global, ConfigWatchKey::profile);
                self.handles.get(&key)
            })
            .is_some_and(|entry| entry.handle.installation_pending());
        if new_init_error.is_some()
            || reload_failure.config_watcher_init_error_references_missing_profile(current)
            || !stored_error_pending
        {
            reload_failure.apply_config_watcher_init_pass(new_init_error);
        }
        if global_invalidated || any_invalidated {
            self.dirty.store(true, Ordering::Release);
        }
    }
}

/// Stable identity of a watcher-init failure; `notify`'s Display text is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum WatcherInitErrorKind {
    Watch(crate::file_watch::WatchErrorKind),
    /// App-dir resolution failed before any subscribe attempt.
    Resolution,
}

/// Equality is keyed on `(profile, kind)`; `message` is display-only.
pub(in crate::tui) struct WatcherInitError {
    pub(in crate::tui) profile: Option<String>,
    pub(in crate::tui) kind: WatcherInitErrorKind,
    pub(in crate::tui) message: String,
}

impl PartialEq for WatcherInitError {
    fn eq(&self, other: &Self) -> bool {
        self.profile == other.profile && self.kind == other.kind
    }
}

impl Eq for WatcherInitError {}

/// Tick-driven reload failures, aggregated into one dialog per failure burst.
#[derive(Default)]
pub(in crate::tui) struct ReloadFailureState {
    canonical_error: Option<String>,
    daemon_error: Option<String>,
    storage_error: Option<String>,
    config_error: Option<String>,
    pub(super) disk_watcher_init_error: Option<WatcherInitError>,
    pub(super) config_watcher_init_error: Option<WatcherInitError>,
    /// Latched once shown; re-armed when a source newly fails or all sources recover.
    dialog_acknowledged: bool,
}

impl ReloadFailureState {
    pub(in crate::tui) fn record_canonical(&mut self, result: &anyhow::Result<()>) -> bool {
        self.record(|state| &mut state.canonical_error, result)
    }

    pub(in crate::tui) fn record_daemon(&mut self, result: &anyhow::Result<()>) -> bool {
        self.record(|state| &mut state.daemon_error, result)
    }

    /// Returns true on a failed-to-healthy transition.
    fn record(
        &mut self,
        slot: fn(&mut Self) -> &mut Option<String>,
        result: &anyhow::Result<()>,
    ) -> bool {
        let was_failed = slot(self).is_some();
        match result {
            Ok(()) if was_failed => {
                *slot(self) = None;
                if !self.has_any_failure() {
                    self.dialog_acknowledged = false;
                }
                true
            }
            Ok(()) => false,
            Err(e) => {
                if !was_failed {
                    self.dialog_acknowledged = false;
                }
                *slot(self) = Some(format!("{e:#}"));
                false
            }
        }
    }

    pub(in crate::tui) fn record_storage(&mut self, result: &anyhow::Result<()>) -> bool {
        let safe = result
            .as_ref()
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!(crate::session::sanitized_load_failure(error)));
        self.record(|s| &mut s.storage_error, &safe)
    }

    pub(in crate::tui) fn record_config(&mut self, result: &anyhow::Result<()>) -> bool {
        let safe = result
            .as_ref()
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!(crate::session::sanitized_load_failure(error)));
        self.record(|s| &mut s.config_error, &safe)
    }

    /// Apply one rewire pass's outcome; only a changed failure re-arms the dialog.
    fn apply_init_pass(
        &mut self,
        slot: fn(&mut Self) -> &mut Option<WatcherInitError>,
        new: Option<WatcherInitError>,
    ) {
        let was = std::mem::replace(slot(self), new);
        let curr = &*slot(self);
        let (changed, failing) = (was != *curr, curr.is_some());
        if changed && (failing || !self.has_any_failure()) {
            self.dialog_acknowledged = false;
        }
    }

    pub(in crate::tui) fn apply_disk_watcher_init_pass(&mut self, new: Option<WatcherInitError>) {
        self.apply_init_pass(|s| &mut s.disk_watcher_init_error, new);
    }

    pub(in crate::tui) fn apply_config_watcher_init_pass(&mut self, new: Option<WatcherInitError>) {
        self.apply_init_pass(|s| &mut s.config_watcher_init_error, new);
    }

    fn init_error_references_missing_profile(
        error: &Option<WatcherInitError>,
        current: &[String],
    ) -> bool {
        error
            .as_ref()
            .and_then(|e| e.profile.as_deref())
            .is_some_and(|name| !current.iter().any(|p| p == name))
    }

    pub(in crate::tui) fn disk_watcher_init_error_references_missing_profile(
        &self,
        current: &[String],
    ) -> bool {
        Self::init_error_references_missing_profile(&self.disk_watcher_init_error, current)
    }

    pub(in crate::tui) fn config_watcher_init_error_references_missing_profile(
        &self,
        current: &[String],
    ) -> bool {
        Self::init_error_references_missing_profile(&self.config_watcher_init_error, current)
    }

    pub(in crate::tui) fn has_any_failure(&self) -> bool {
        self.daemon_error.is_some()
            || self.canonical_error.is_some()
            || self.storage_error.is_some()
            || self.config_error.is_some()
            || self.disk_watcher_init_error.is_some()
            || self.config_watcher_init_error.is_some()
    }

    pub(in crate::tui) fn has_unacknowledged_failure(&self) -> bool {
        self.has_any_failure() && !self.dialog_acknowledged
    }

    pub(in crate::tui) fn build_dialog_body(&self) -> String {
        let mut lines = vec!["The following reload sources are degraded:".to_string()];
        if let Some(error) = &self.canonical_error {
            lines.push(format!("- Canonical projection: {error}"));
        }
        if let Some(error) = &self.daemon_error {
            lines.push(format!("- Runtime: {error}"));
        }
        if let Some(e) = &self.storage_error {
            lines.push(format!("- Storage: {e}"));
        }
        if let Some(e) = &self.config_error {
            lines.push(format!("- Config: {e}"));
        }
        if let Some(e) = &self.disk_watcher_init_error {
            let detail = match &e.profile {
                Some(name) => format!("{name}: {}", e.message),
                None => e.message.clone(),
            };
            lines.push(format!("- Disk watcher init: {detail}"));
        }
        if let Some(e) = &self.config_watcher_init_error {
            let detail = match &e.profile {
                Some(name) => format!("profile {name} config: {}", e.message),
                None => format!("global config: {}", e.message),
            };
            lines.push(format!("- Config watcher init: {detail}"));
        }
        lines.push(String::new());
        lines.push("In-memory state preserved; sources retry automatically.".to_string());
        lines.join("\n")
    }

    pub(in crate::tui) fn acknowledge_dialog(&mut self) {
        self.dialog_acknowledged = true;
    }
}

/// Log each legacy duplicate once per process; it persists until the user edits files by hand.
pub(in crate::tui) fn log_legacy_duplicates_once(reports: &[crate::session::DuplicateIdReport]) {
    static REPORTED_IDS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut seen = REPORTED_IDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for report in reports {
        if seen.iter().any(|id| id == &report.id) {
            continue;
        }
        seen.push(report.id.clone());
        tracing::error!(target: "tui.home", "{}", report.actionable_message());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn recovered_disk_watch_error_clears_while_another_watch_is_pending() {
        let _home = crate::session::test_support::isolate_app_dir();
        let current = vec!["recovered".to_string(), "pending".to_string()];
        for name in &current {
            crate::session::get_profile_dir(name).unwrap();
        }
        let (service, backend) = crate::file_watch::test_support::manual_filewatch();
        let mut watches = DiskWatchState {
            dirty: Arc::new(AtomicBool::new(false)),
            handles: HashMap::new(),
        };
        let mut failure = ReloadFailureState::default();
        watches.rewire(&service, &current[..1], &mut failure);
        let dir = backend.complete_watch(Err(notify::Error::generic("fixture native failure")));
        watches.rewire(&service, &current[..1], &mut failure);
        assert!(failure.disk_watcher_init_error.is_some());

        backend.allow_retry(&dir);
        watches.rewire(&service, &current, &mut failure);
        assert!(watches.handles["recovered"].handle.installation_pending());
        assert!(
            failure.disk_watcher_init_error.is_some(),
            "retain the error while its own retry is pending"
        );
        assert_eq!(backend.complete_watch(Ok(())), dir);
        assert!(watches.handles["pending"].handle.installation_pending());
        watches.rewire(&service, &current, &mut failure);
        assert!(
            failure.disk_watcher_init_error.is_none(),
            "an unrelated pending watch must not retain a recovered error"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn recovered_config_watch_error_clears_while_another_watch_is_pending() {
        let _home = crate::session::test_support::isolate_app_dir();
        for name in ["recovered", "pending"] {
            crate::session::get_profile_dir(name).unwrap();
        }
        for profile_error in [true, false] {
            let (service, backend) = crate::file_watch::test_support::manual_filewatch();
            let mut watches = ConfigWatchState {
                dirty: Arc::new(AtomicBool::new(false)),
                link_policy_dirty: Arc::new(AtomicBool::new(false)),
                handles: HashMap::new(),
            };
            let mut failure = ReloadFailureState::default();
            let mut current = if profile_error {
                vec!["recovered".to_string()]
            } else {
                vec![]
            };
            watches.rewire(&service, &current, &mut failure);
            if profile_error {
                backend.complete_watch(Ok(()));
            }
            let dir = backend.complete_watch(Err(notify::Error::generic("fixture native failure")));
            watches.rewire(&service, &current, &mut failure);
            assert!(failure.config_watcher_init_error.is_some());

            backend.allow_retry(&dir);
            current.push("pending".to_string());
            watches.rewire(&service, &current, &mut failure);
            let key = if profile_error {
                ConfigWatchKey::profile("recovered")
            } else {
                ConfigWatchKey::Global
            };
            assert!(watches.handles[&key].handle.installation_pending());
            assert!(
                failure.config_watcher_init_error.is_some(),
                "retain the error while its own retry is pending"
            );
            assert_eq!(backend.complete_watch(Ok(())), dir);
            assert!(watches.handles[&ConfigWatchKey::profile("pending")]
                .handle
                .installation_pending());
            watches.rewire(&service, &current, &mut failure);
            assert!(failure.config_watcher_init_error.is_none(), "an unrelated pending watch must not retain a recovered error (profile_error={profile_error})");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn deferred_config_watch_failure_retries_and_clears_only_after_success() {
        let _home = crate::session::test_support::isolate_app_dir();
        for observe_before_backoff in [true, false] {
            let (service, backend) = crate::file_watch::test_support::manual_filewatch();
            let mut watches = ConfigWatchState {
                dirty: Arc::new(AtomicBool::new(false)),
                link_policy_dirty: Arc::new(AtomicBool::new(false)),
                handles: HashMap::new(),
            };
            let mut failure = ReloadFailureState::default();
            watches.rewire(&service, &[], &mut failure);
            let dir = backend.complete_watch(Err(notify::Error::generic("fixture native failure")));
            if observe_before_backoff {
                watches.rewire(&service, &[], &mut failure);
                assert!(
                    failure.config_watcher_init_error.is_some(),
                    "asynchronous failure must reach the warning state"
                );
                assert!(
                    !watches.handles[&ConfigWatchKey::Global]
                        .handle
                        .installation_pending(),
                    "backoff prevents an immediate retry loop"
                );
            }

            backend.allow_retry(&dir);
            watches.rewire(&service, &[], &mut failure);
            assert!(watches.handles[&ConfigWatchKey::Global]
                .handle
                .installation_pending());
            assert!(
                failure.config_watcher_init_error.is_some(),
                "queued retry is not a successful native installation"
            );
            backend.complete_watch(Ok(()));
            watches.rewire(&service, &[], &mut failure);
            assert!(failure.config_watcher_init_error.is_none());
            assert_eq!(service.subscriber_count(), 1);
        }
    }
}
