//! The disk and config watchers the view owns, and how a failed reload is
//! surfaced.

use super::*;

/// Identifies config-watch entries without letting a profile literally named
/// `"<global>"` collide with the app-wide config subscription.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::tui) enum ConfigWatchKey {
    /// The app-wide `<app_dir>/config.toml` subscription.
    Global,
    /// A per-profile `<profile>/config.toml` subscription.
    Profile(String),
}

impl ConfigWatchKey {
    pub(super) fn profile(name: &str) -> Self {
        Self::Profile(name.to_string())
    }
}

pub(super) const RELOAD_FAILED_TITLE: &str = "Reload Failed";

pub(super) const WATCHER_WARNING_TITLE: &str = "Watcher Warning";

/// Distinguishes user-driven config reloads from watcher kicks so
/// `refresh_from_config` can suppress interactive-only dialogs on
/// background refreshes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum ConfigRefreshOrigin {
    /// A user action triggered the reload and may surface dialogs.
    Interactive,
    /// A watcher kick triggered the reload and should stay silent.
    Watcher,
}

/// Eligible, unseen tip count for the home-view badge, honoring the
/// `session.show_tips` setting. Shared by the constructor, config refresh, and
/// the tips-state writers so the cached badge count can't drift.
pub(in crate::tui) fn tips_unseen_count(config: &crate::session::Config) -> usize {
    if !config.session.show_tips {
        return 0;
    }
    crate::tips::unseen_count(
        crate::tips::TipSurface::Tui,
        &config.app_state.tips_seen,
        &crate::tips::TipSignals {
            new_session_with_selection_count: config.app_state.new_session_with_selection_count,
            used_new_from_selection: config.app_state.used_new_from_selection,
            system_health_tip_earned: config.app_state.system_health_tip_earned,
            used_system_health: config.app_state.used_system_health,
        },
    )
}

/// Per-profile subscription pair, held in `HomeView::disk_watch.handles`
/// and `HomeView::config_watch.handles`.
///
/// Two teardown paths exist:
/// 1. Explicit-remove (rewire / profile delete via `drop_disk_watch_entry`):
///    drop the `SubscriptionHandle` first to close the source channel; the
///    forwarder's `rx.recv().await` returns `None` and exits naturally;
///    `forwarder.abort()` then runs as a fast-path safeguard for any
///    `recv` future that has not yet observed the close.
/// 2. HomeView field-drop on shutdown: the same order is guaranteed by
///    struct field declaration order. `disk_watch.handles` drops, each
///    entry's handle drops first (channel-close cascade), the forwarder
///    exits naturally; the AbortHandle drop is a no-op (Tokio's
///    `AbortHandle` does not abort on drop) but the forwarder is already
///    gone.
pub(in crate::tui) struct DiskWatchEntry {
    handle: crate::file_watch::SubscriptionHandle,
    forwarder: tokio::task::AbortHandle,
    /// Canonicalized dir at install time. Compared against the current
    /// canonical resolution on rewire to detect path-level moves.
    /// notify NonRecursive watches do not auto-reattach to a recreated
    /// directory on Linux inotify or macOS FSEvents.
    canonical_dir: std::path::PathBuf,
    /// Filesystem identity (`(dev, ino, btime)` on Unix; `()`
    /// elsewhere) captured when the subscription was installed. The
    /// canonical path string survives a peer `rm -rf X && mkdir X`
    /// race because the new dir resolves to the same string, and on
    /// ext4/overlayfs the freed inode number is routinely recycled by
    /// the immediate recreate; the birth time component is what
    /// distinguishes the new dir there. On rewire, mismatch against a
    /// fresh stat forces an entry rebuild even when the canonical path
    /// is unchanged. Stat failure at install stores the type's
    /// `Default` (`(0, 0, None)` on Unix; `()` elsewhere) as a
    /// sentinel; on Unix `(0, 0, _)` cannot collide with a real
    /// filesystem identity, so the next rewire that successfully stats
    /// the dir mismatches against the sentinel and forces a rebuild.
    pub(super) installed_identity: crate::file_watch::WatchIdentity,
}

/// Drop the subscription handle FIRST. Closing the source channel
/// before aborting the forwarder ensures no in-flight event reaches
/// an aborted task.
pub(super) fn drop_disk_watch_entry(entry: DiskWatchEntry) {
    let DiskWatchEntry {
        handle,
        forwarder,
        canonical_dir: _,
        installed_identity: _,
    } = entry;
    drop(handle);
    forwarder.abort();
}

/// Sibling of [`ConfigWatchState`]; groups the per-profile storage-mirror
/// subscriptions with the shared dirty latch their forwarders set. The
/// two fields must move together because every install/uninstall of a
/// subscription is paired with a kick or compensation `store` on the
/// latch. Per the file-watch service contract, the rewire path reuses
/// the single `Arc<FileWatchService>` constructed once for this TUI
/// process and never builds a second one.
pub(in crate::tui) struct DiskWatchState {
    /// Dirty latch (cap-1 fan-in) set with `Release` ordering by every
    /// profile forwarder and swapped to `false` with `Acquire` ordering
    /// by the tick loop in `App::run`; idempotent across forwarders so
    /// multiple events between two reloads collapse into one
    /// `reload_storage_only`.
    pub(in crate::tui) dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Per-profile subscription pairs keyed by profile name. Drop a
    /// value through [`drop_disk_watch_entry`] to honor the
    /// drop-then-abort teardown protocol; field-drop on `HomeView`
    /// shutdown falls back to the same order via declaration order.
    pub(in crate::tui) handles: HashMap<String, DiskWatchEntry>,
}

/// Sibling of [`DiskWatchState`]; groups the per-key config-file
/// subscriptions with the shared dirty latch their forwarders set. The
/// typed key keeps the global `<app_dir>/config.toml` entry from
/// colliding with any literal profile name. The Arc-reuse rule is the
/// same as [`DiskWatchState`]: the rewire path reuses the single
/// `Arc<FileWatchService>` constructed once for this TUI process.
pub(in crate::tui) struct ConfigWatchState {
    /// Dirty latch (cap-1 fan-in) set with `Release` ordering by every
    /// config forwarder (global + per-profile) and swapped to `false`
    /// with `Acquire` ordering by the tick loop in `App::run`;
    /// idempotent across forwarders so multiple events between two
    /// reloads collapse into one `refresh_from_config`.
    pub(in crate::tui) dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Per-key subscription pairs. Reuses [`DiskWatchEntry`] because
    /// the drop-then-abort teardown protocol is identical to the
    /// disk-watch sibling.
    pub(in crate::tui) handles: HashMap<ConfigWatchKey, DiskWatchEntry>,
}

impl DiskWatchState {
    /// Reconcile per-profile storage-mirror subscriptions
    /// (`sessions.json` / `groups.json`) against `current` via set-diff:
    /// drop entries for profiles in `prior - current`, keep entries in
    /// `prior ∩ current` untouched, install fresh entries for profiles
    /// in `current - prior`. Same-set rewires are a no-op.
    ///
    /// Inode-invalidation case (profile dir deleted and recreated under
    /// the same name): the caller must drop the stale entry first via
    /// `drop_disk_watch_entry` before invoking this helper, so the name
    /// is missing from `prior` and the install path runs.
    ///
    /// Service ownership: the rewire path reuses the caller's
    /// `Arc<FileWatchService>`, the single instance constructed once for
    /// this TUI process (per the file-watch service design's "one Arc
    /// per process" rule). It must NEVER construct a second service.
    /// In-process storage writes propagate through the Local fast path
    /// in `Storage::update`; cross-process writes propagate through the
    /// kernel watcher within its debounce window.
    pub(in crate::tui) fn rewire(
        &mut self,
        file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
        current: &[String],
        reload_failure: &mut ReloadFailureState,
    ) {
        use crate::file_watch::{FileMatcher, WatchSpec};
        use std::collections::HashSet;
        use std::time::Duration;

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        let prior: HashSet<String> = self.handles.keys().cloned().collect();
        let target: HashSet<&String> = current.iter().collect();

        // Detect peer-driven delete-and-recreate of any prior profile dir
        // by comparing each prior entry's stored canonical_dir against the
        // current canonical resolution. Mismatch forces a rewire of that
        // entry even when the name set is unchanged, since notify
        // NonRecursive watches do not auto-reattach across the inode
        // change on Linux inotify or macOS FSEvents. Resolve via the
        // non-creating `get_profile_dir_path`: this is a read-only
        // existence/canonicalization probe, and `get_profile_dir` would
        // resurrect a profile dir that a peer just deleted, leaving the
        // removed profile visible in `list_profiles()` forever.
        let inode_invalidated: HashSet<String> = prior
            .iter()
            .filter(|name| {
                let entry = match self.handles.get(*name) {
                    Some(e) => e,
                    None => return false,
                };
                let current_canonical = crate::session::get_profile_dir_path(name)
                    .ok()
                    .and_then(|p| std::fs::canonicalize(&p).ok());
                match current_canonical {
                    Some(canonical) => {
                        canonical != entry.canonical_dir
                            || crate::file_watch::capture_watch_identity(&canonical)
                                .map(|id| id != entry.installed_identity)
                                .unwrap_or(false)
                    }
                    None => true,
                }
            })
            .cloned()
            .collect();

        if prior == current.iter().cloned().collect()
            && inode_invalidated.is_empty()
            && !reload_failure.disk_watcher_init_error_references_missing_profile(current)
        {
            return;
        }

        // Buffer the install-loop outcome and apply it as one transition
        // at the end of the pass: an identical failure recurring across
        // rewires must not re-arm the ack latch (issue #2112).
        let mut new_init_error: Option<WatcherInitError> = None;

        let to_remove: Vec<String> = prior
            .iter()
            .filter(|n| !target.contains(*n) || inode_invalidated.contains(*n))
            .cloned()
            .collect();
        let to_add: Vec<String> = current
            .iter()
            .filter(|n| !prior.contains(*n) || inode_invalidated.contains(*n))
            .cloned()
            .collect();

        for name in &to_remove {
            if let Some(entry) = self.handles.remove(name) {
                drop_disk_watch_entry(entry);
            }
        }

        for name in &to_add {
            let dir = match crate::session::get_profile_dir_path(name) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        profile = %name,
                        error = %e,
                        "skipping subscribe; profile dir resolution failed"
                    );
                    continue;
                }
            };
            if !dir.exists() {
                tracing::debug!(
                    target: "tui.file_watch",
                    profile = %name,
                    "skipping disk subscribe; profile dir absent (peer delete raced the list_profiles snapshot)"
                );
                continue;
            }
            let canonical_dir = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            let sessions_path = dir.join("sessions.json");
            let groups_path = dir.join("groups.json");
            let spec = WatchSpec {
                dir: dir.clone(),
                matcher: FileMatcher::AnyOf(vec![sessions_path, groups_path]),
                debounce: Some(Duration::from_millis(75)),
            };
            match file_watch.subscribe_channel(spec, 16) {
                Ok((mut rx, handle)) => {
                    use tracing::Instrument;
                    let dirty = self.dirty.clone();
                    // Forwarder exits via `rx.recv() = None` when its
                    // SubscriptionHandle is dropped (rewire / HomeView
                    // teardown). The TUI has no graceful-drain phase, so
                    // no `CancellationToken` is plumbed through here.
                    let span = tracing::debug_span!(
                        "tui.disk_watch.forwarder",
                        profile = %name
                    );
                    let join = crate::task_util::spawn_supervised(
                        "tui.disk_watch.forwarder",
                        crate::task_util::PanicPolicy::Log,
                        async move {
                            while rx.recv().await.is_some() {
                                dirty.store(true, std::sync::atomic::Ordering::Release);
                            }
                        }
                        .instrument(span),
                    );
                    self.handles.insert(
                        name.clone(),
                        DiskWatchEntry {
                            handle,
                            forwarder: join.abort_handle(),
                            canonical_dir: canonical_dir.clone(),
                            installed_identity: crate::file_watch::capture_watch_identity(
                                &canonical_dir,
                            )
                            .unwrap_or_default(),
                        },
                    );
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
        reload_failure.apply_disk_watcher_init_pass(new_init_error);
        tracing::debug!(
            target: "tui.file_watch",
            added = ?to_add,
            removed = ?to_remove,
            "reconciled per-profile disk-watch subscriptions"
        );
        // Missed-window compensation, mirroring the config rewire: a
        // sessions.json/groups.json write into a recreated dir before
        // this rebuild produced no event, so kick the latch and let the
        // next tick reload storage from disk.
        if !inode_invalidated.is_empty() {
            self.dirty.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

impl ConfigWatchState {
    /// Reconcile the per-key config-file subscriptions against the live
    /// profile set + the always-present global key. The global key
    /// (`<app_dir>/config.toml`) is subscribed once and kept across
    /// rewires because the app dir is never deleted mid-session, so the
    /// kernel watch on it stays valid.
    ///
    /// Per-profile entries are fully torn down then re-subscribed on
    /// each rewire: every existing per-profile entry is dropped first,
    /// then a fresh subscription is installed for each profile in
    /// `current`. This handles the "profile dir deleted and recreated
    /// under the same name" case where the kernel watch is invalidated
    /// by the unlink even though the profile name has not changed.
    /// Unlike [`DiskWatchState::rewire`] which uses set-diff to
    /// preserve stable subscriptions across calls, full teardown is
    /// cheap here because each profile has a single config-file
    /// subscription rather than a directory watch with multiple matched
    /// files, and the per-rewire churn is bounded by profile count.
    ///
    /// Drop order on remove is canonical: drop the `SubscriptionHandle`
    /// FIRST, then abort the forwarder, so the source channel closes
    /// and the forwarder's `rx.recv()` returns `None` naturally before
    /// the abort fires as a safeguard.
    ///
    /// Service ownership: the rewire path reuses the caller's
    /// `Arc<FileWatchService>`, the single instance constructed once for
    /// this TUI process (per the file-watch service design's "one Arc
    /// per process" rule). It must NEVER construct a second service.
    /// Cross-process config edits (user `$EDITOR` save, peer
    /// `aoe profile create/delete`) propagate through the kernel
    /// watcher; in-process config writes are out of scope here because
    /// `Storage::update` does not write config files (only sessions /
    /// groups), so no `notify_local_change` is wired on this path.
    pub(in crate::tui) fn rewire(
        &mut self,
        file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
        current: &[String],
        reload_failure: &mut ReloadFailureState,
    ) {
        use crate::file_watch::{FileMatcher, WatchSpec};
        use std::time::Duration;

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        // Drop the existing global entry when its stored canonical_dir
        // does not match the live canonicalized app dir, mirroring the
        // disk-watch inode-aware rewire. The install-once branch below
        // picks up the new inode.
        let global_invalidated = match self.handles.get(&ConfigWatchKey::Global) {
            Some(entry) => {
                let current_canonical = crate::session::get_app_dir()
                    .ok()
                    .and_then(|p| std::fs::canonicalize(&p).ok());
                match current_canonical {
                    Some(canonical) => {
                        canonical != entry.canonical_dir
                            || crate::file_watch::capture_watch_identity(&canonical)
                                .map(|id| id != entry.installed_identity)
                                .unwrap_or(false)
                    }
                    None => true,
                }
            }
            None => false,
        };
        if global_invalidated {
            if let Some(entry) = self.handles.remove(&ConfigWatchKey::Global) {
                drop_disk_watch_entry(entry);
            }
        }
        let global_needs_install = !self.handles.contains_key(&ConfigWatchKey::Global);

        let prior_profiles: std::collections::HashSet<String> = self
            .handles
            .keys()
            .filter_map(|key| match key {
                ConfigWatchKey::Global => None,
                ConfigWatchKey::Profile(name) => Some(name.clone()),
            })
            .collect();
        let target: std::collections::HashSet<&String> = current.iter().collect();

        // Per-profile inode invalidation: peer-driven `aoe profile delete X
        // && aoe profile new X` keeps the same name but produces a new
        // inode, and notify NonRecursive watches do not auto-reattach.
        // Compare each prior entry's stored canonical_dir against the
        // current canonical resolution; mismatch forces a rewire even
        // when the name set is unchanged. Resolution goes through the
        // non-creating `get_profile_dir_path`; `get_profile_dir` calls
        // `fs::create_dir_all`, which recreates a profile directory
        // the user just deleted and re-surfaces it in `list_profiles()`
        // on the next heartbeat.
        let inode_invalidated: Vec<String> = prior_profiles
            .iter()
            .filter(|name| {
                let entry = match self.handles.get(&ConfigWatchKey::profile(name)) {
                    Some(e) => e,
                    None => return false,
                };
                let current_canonical = crate::session::get_profile_dir_path(name)
                    .ok()
                    .and_then(|p| std::fs::canonicalize(&p).ok());
                match current_canonical {
                    Some(canonical) => {
                        canonical != entry.canonical_dir
                            || crate::file_watch::capture_watch_identity(&canonical)
                                .map(|id| id != entry.installed_identity)
                                .unwrap_or(false)
                    }
                    None => true,
                }
            })
            .cloned()
            .collect();

        let to_remove: Vec<String> = prior_profiles
            .iter()
            .filter(|n| !target.contains(*n) || inode_invalidated.iter().any(|i| i == *n))
            .cloned()
            .collect();
        let to_add: Vec<String> = current
            .iter()
            .filter(|n| !prior_profiles.contains(*n) || inode_invalidated.iter().any(|i| i == *n))
            .cloned()
            .collect();

        if !global_needs_install
            && to_remove.is_empty()
            && to_add.is_empty()
            && !reload_failure.config_watcher_init_error_references_missing_profile(current)
        {
            return;
        }

        // Buffer the install-loop outcome and apply it as one transition
        // at the end of the pass: an identical failure recurring across
        // rewires must not re-arm the ack latch (issue #2112).
        let mut new_init_error: Option<WatcherInitError> = None;

        if global_needs_install {
            match crate::session::get_app_dir() {
                Ok(app_dir) => {
                    let canonical_dir =
                        std::fs::canonicalize(&app_dir).unwrap_or_else(|_| app_dir.clone());
                    let target = app_dir.join("config.toml");
                    let spec = WatchSpec {
                        dir: app_dir,
                        matcher: FileMatcher::Exact(target),
                        debounce: Some(Duration::from_millis(100)),
                    };
                    match file_watch.subscribe_channel(spec, 4) {
                        Ok((mut rx, handle)) => {
                            use tracing::Instrument;
                            let dirty = std::sync::Arc::clone(&self.dirty);
                            let span = tracing::debug_span!("tui.config_watch.global.forwarder");
                            let join = crate::task_util::spawn_supervised(
                                "tui.config_watch.global.forwarder",
                                crate::task_util::PanicPolicy::Log,
                                async move {
                                    while rx.recv().await.is_some() {
                                        dirty.store(true, std::sync::atomic::Ordering::Release);
                                    }
                                }
                                .instrument(span),
                            );
                            self.handles.insert(
                                ConfigWatchKey::Global,
                                DiskWatchEntry {
                                    handle,
                                    forwarder: join.abort_handle(),
                                    canonical_dir: canonical_dir.clone(),
                                    installed_identity: crate::file_watch::capture_watch_identity(
                                        &canonical_dir,
                                    )
                                    .unwrap_or_default(),
                                },
                            );
                            tracing::debug!(
                                target: "tui.file_watch",
                                "global config.toml subscription installed"
                            );
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
            let dir = match crate::session::get_profile_dir_path(name) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        target: "tui.file_watch",
                        profile = %name,
                        error = %e,
                        "skipping config subscribe; profile dir resolution failed"
                    );
                    continue;
                }
            };
            if !dir.exists() {
                tracing::debug!(
                    target: "tui.file_watch",
                    profile = %name,
                    "skipping config subscribe; profile dir absent (peer delete raced the list_profiles snapshot)"
                );
                continue;
            }
            let canonical_dir = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            let target_path = dir.join("config.toml");
            let spec = WatchSpec {
                dir: dir.clone(),
                matcher: FileMatcher::Exact(target_path),
                debounce: Some(Duration::from_millis(100)),
            };
            match file_watch.subscribe_channel(spec, 4) {
                Ok((mut rx, handle)) => {
                    use tracing::Instrument;
                    let dirty = self.dirty.clone();
                    let span = tracing::debug_span!(
                        "tui.config_watch.profile.forwarder",
                        profile = %name
                    );
                    let join = crate::task_util::spawn_supervised(
                        "tui.config_watch.profile.forwarder",
                        crate::task_util::PanicPolicy::Log,
                        async move {
                            while rx.recv().await.is_some() {
                                dirty.store(true, std::sync::atomic::Ordering::Release);
                            }
                        }
                        .instrument(span),
                    );
                    self.handles.insert(
                        ConfigWatchKey::profile(name),
                        DiskWatchEntry {
                            handle,
                            forwarder: join.abort_handle(),
                            canonical_dir: canonical_dir.clone(),
                            installed_identity: crate::file_watch::capture_watch_identity(
                                &canonical_dir,
                            )
                            .unwrap_or_default(),
                        },
                    );
                    tracing::debug!(
                        target: "tui.file_watch",
                        profile = %name,
                        "profile config.toml subscription installed"
                    );
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
        reload_failure.apply_config_watcher_init_pass(new_init_error);
        if !to_add.is_empty() || !to_remove.is_empty() {
            tracing::debug!(
                target: "tui.file_watch",
                added = ?to_add,
                removed = ?to_remove,
                "rewire_config_subscriptions: per-profile set-diff update"
            );
        }
        // Missed-window compensation: an invalidation-driven rebuild means
        // the kernel watch was dead for some interval (peer rm+recreate of
        // the watched dir), and any config write landing in that interval
        // produced no event. Kick the dirty latch so the next tick
        // re-reads config from disk rather than trusting the (silent)
        // fresh watch. Scoped to invalidation rebuilds; plain set-diff
        // adds/removes have no dead window to compensate.
        if global_invalidated || !inode_invalidated.is_empty() {
            self.dirty.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Stable identity for a watcher-init failure across rewire passes.
/// The `notify` crate's Display string is not part of its stability
/// guarantee; ack-equality is keyed on the structured kind so a
/// future Display drift does not silently re-arm the dialog on the
/// same persistent failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum WatcherInitErrorKind {
    Watch(crate::file_watch::WatchErrorKind),
    /// The app-dir resolution path errored before a subscribe attempt
    /// could be made. Distinct from any `Watch(_)` variant so a
    /// resolution failure followed by a backend failure surfaces as a
    /// content change.
    Resolution,
}

/// Latched record of a watcher-init failure. The disk slot always carries
/// `Some(profile)`; the config slot carries `None` for the global config
/// watch and `Some(profile)` per-profile. Equality is keyed on
/// `(profile, kind)`; `message` is display-only.
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

/// Per-tick reload failure tracking. Tick-driven reload paths in
/// `App::run` (heartbeat `reload()`, watcher-driven `reload_storage_only()`,
/// watcher-driven `refresh_from_config()`) route results through
/// `handle_tick_reload_storage` / `handle_tick_reload_config`, which
/// record failures here so the tick loop surfaces a single aggregated
/// `info_dialog` per failure burst rather than one dialog per tick.
///
/// `dialog_acknowledged` latches once the dialog is shown and clears
/// only after every source returns to healthy, so the user is notified
/// once per failure burst, not once per tick. The dialog body aggregates
/// every currently-failing source (storage, config, disk-watcher init,
/// config-watcher init) into a single message.
#[derive(Default)]
pub(in crate::tui) struct ReloadFailureState {
    storage_failed: bool,
    storage_error: Option<String>,
    config_failed: bool,
    config_error: Option<String>,
    /// Latched record of the most recent disk-watcher init failure
    /// (typically `subscribe_channel` returning Err on disk rewire).
    /// Surfaced in the reload-failure dialog body. Cleared on the next
    /// successful disk rewire pass for the affected profile.
    pub(super) disk_watcher_init_error: Option<WatcherInitError>,
    /// Latched record of the most recent config-watcher init failure
    /// (typically `subscribe_channel` returning Err on config rewire).
    /// Independent from `disk_watcher_init_error`: a config init failure
    /// is not overwritten by a disk rewire and persists until the next
    /// successful config rewire pass for the affected key.
    pub(super) config_watcher_init_error: Option<WatcherInitError>,
    dialog_acknowledged: bool,
}

impl ReloadFailureState {
    pub(in crate::tui) fn record_storage(&mut self, result: &anyhow::Result<()>) -> bool {
        match result {
            Ok(()) => {
                if self.storage_failed {
                    self.storage_failed = false;
                    self.storage_error = None;
                    if !self.has_any_failure() {
                        self.dialog_acknowledged = false;
                    }
                    return true;
                }
                false
            }
            Err(e) => {
                // Healthy-to-failed transition re-arms the dialog so a new
                // source failing during a previously acknowledged burst
                // surfaces a fresh notification rather than being silently
                // absorbed by the ack latch.
                if !self.storage_failed {
                    self.dialog_acknowledged = false;
                }
                self.storage_failed = true;
                self.storage_error = Some(format!("{e:#}"));
                false
            }
        }
    }

    pub(in crate::tui) fn record_config(&mut self, result: &anyhow::Result<()>) -> bool {
        match result {
            Ok(()) => {
                if self.config_failed {
                    self.config_failed = false;
                    self.config_error = None;
                    if !self.has_any_failure() {
                        self.dialog_acknowledged = false;
                    }
                    return true;
                }
                false
            }
            Err(e) => {
                if !self.config_failed {
                    self.dialog_acknowledged = false;
                }
                self.config_failed = true;
                self.config_error = Some(format!("{e:#}"));
                false
            }
        }
    }

    /// Apply the outcome of a disk-watch rewire pass as one transition.
    /// `new` is the per-pass install-loop result (`Some` if any profile's
    /// `subscribe_channel` returned `Err`, `None` otherwise). The latch
    /// is re-armed only on a content change: a same-as-before failure
    /// inside an acknowledged burst is treated as a no-op so the user
    /// is not re-notified every rewire pass while the underlying
    /// failure persists. A clean transition to `None` resets the ack
    /// latch when no other source remains failing, so a later identical
    /// failure surfaces a fresh dialog.
    pub(in crate::tui) fn apply_disk_watcher_init_pass(&mut self, new: Option<WatcherInitError>) {
        let was = std::mem::replace(&mut self.disk_watcher_init_error, new);
        match (&was, &self.disk_watcher_init_error) {
            (Some(prev), Some(curr)) if prev == curr => {}
            (None, None) => {}
            (Some(_), None) => {
                if !self.has_any_failure() {
                    self.dialog_acknowledged = false;
                }
            }
            (_, Some(_)) => {
                self.dialog_acknowledged = false;
            }
        }
    }

    /// Apply the outcome of a config-watch rewire pass as one transition.
    /// See [`Self::apply_disk_watcher_init_pass`] for the latch semantics;
    /// the two slots are independent.
    pub(in crate::tui) fn apply_config_watcher_init_pass(&mut self, new: Option<WatcherInitError>) {
        let was = std::mem::replace(&mut self.config_watcher_init_error, new);
        match (&was, &self.config_watcher_init_error) {
            (Some(prev), Some(curr)) if prev == curr => {}
            (None, None) => {}
            (Some(_), None) => {
                if !self.has_any_failure() {
                    self.dialog_acknowledged = false;
                }
            }
            (_, Some(_)) => {
                self.dialog_acknowledged = false;
            }
        }
    }

    pub(in crate::tui) fn disk_watcher_init_error_references_missing_profile(
        &self,
        current: &[String],
    ) -> bool {
        self.disk_watcher_init_error
            .as_ref()
            .and_then(|e| e.profile.as_deref())
            .is_some_and(|name| !current.iter().any(|p| p == name))
    }

    pub(in crate::tui) fn config_watcher_init_error_references_missing_profile(
        &self,
        current: &[String],
    ) -> bool {
        self.config_watcher_init_error
            .as_ref()
            .and_then(|e| e.profile.as_deref())
            .is_some_and(|name| !current.iter().any(|p| p == name))
    }

    pub(in crate::tui) fn has_any_failure(&self) -> bool {
        self.storage_failed
            || self.config_failed
            || self.disk_watcher_init_error.is_some()
            || self.config_watcher_init_error.is_some()
    }

    pub(in crate::tui) fn has_unacknowledged_failure(&self) -> bool {
        self.has_any_failure() && !self.dialog_acknowledged
    }

    pub(in crate::tui) fn build_dialog_body(&self) -> String {
        let mut lines: Vec<String> = vec!["The following reload sources are degraded:".to_string()];
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

/// Log each legacy duplicate's actionable details once per process; the
/// condition can persist until the user hand-edits files, so repeating it at
/// ERROR level on every reload tick would be spam.
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
