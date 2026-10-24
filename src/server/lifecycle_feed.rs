//! Publishes session lifecycle changes (created, archived, trashed, renamed, deleted ...).
//!
//! One shared tracker, two producers. The commit observer reports every write this process
//! commits through `Storage::update`, exactly. The disk consumer diffs what it reloads, which
//! also covers writers in other processes and profile moves, but is debounced.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;

use super::reload::LoadedInstances;
use crate::session::lifecycle::{
    self, CommitObserverGuard, LifecycleChange, LifecycleRow, LifecycleTracker,
};

/// Room for a burst of lifecycle changes, such as a bulk delete, before a slow subscriber lags.
const LIFECYCLE_CHANNEL_CAPACITY: usize = 256;

/// Reads of the baseline before giving up on a quiet moment between commits.
const BASELINE_ATTEMPTS: usize = 5;

/// Wait before looking again at a missing session or an overtaken snapshot.
const AGAIN_DELAY: Duration = Duration::from_millis(250);

/// Observations repeated after a failed read before waiting for the next filesystem event.
const READ_RETRY_ATTEMPTS: u32 = 5;

/// Wait before the first read retry; each further one doubles it.
const READ_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

/// What an observation asks of the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Observed {
    Settled,
    /// A missing session awaits a second look, or a commit overtook the snapshot and nothing
    /// was applied.
    Again,
    /// The profile listing or a profile's storage could not be read.
    ReadFailed,
}

/// Paces repeated observations. Read failures draw on a bounded budget that a healthy read or
/// fresh filesystem activity refills, so a profile that stays unreadable is not reloaded
/// forever.
#[derive(Default)]
pub(super) struct ReadRetry {
    failures: u32,
}

impl ReadRetry {
    /// How long to wait before observing again; `None` waits for the next filesystem event.
    /// `fresh` is whether this observation was woken by filesystem activity, not by a retry.
    pub(super) fn next(&mut self, observed: Observed, fresh: bool) -> Option<Duration> {
        if fresh {
            self.failures = 0;
        }
        match observed {
            Observed::Settled => {
                self.failures = 0;
                None
            }
            Observed::Again => Some(AGAIN_DELAY),
            Observed::ReadFailed if self.failures < READ_RETRY_ATTEMPTS => {
                let delay = READ_RETRY_BASE_DELAY * 2u32.pow(self.failures);
                self.failures += 1;
                Some(delay)
            }
            Observed::ReadFailed => None,
        }
    }
}

/// Runs before each publication, so a test can hold one producer there.
#[cfg(test)]
type PublishHook = Arc<dyn Fn(&LifecycleChange) + Send + Sync>;

pub struct LifecycleFeed {
    tracker: Mutex<LifecycleTracker>,
    tx: broadcast::Sender<LifecycleChange>,
    /// Held by whoever reads the store for the tracker, from before the read to after the apply.
    read_gate: tokio::sync::Mutex<()>,
    #[cfg(test)]
    publish_hook: Mutex<Option<PublishHook>>,
}

impl LifecycleFeed {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tracker: Mutex::new(LifecycleTracker::default()),
            tx: broadcast::channel(LIFECYCLE_CHANNEL_CAPACITY).0,
            read_gate: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            publish_hook: Mutex::new(None),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LifecycleChange> {
        self.tx.subscribe()
    }

    /// Commits applied so far. Read before loading from disk, then hand to `observe_disk`,
    /// which refuses a snapshot a commit has overtaken.
    pub fn commit_generation(&self) -> u64 {
        self.tracker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .commits()
    }

    /// Diff the freshly loaded sessions of every profile, read after `since` was taken from
    /// [`Self::commit_generation`].
    pub(super) fn observe_disk(&self, since: u64, loaded: &LoadedInstances) -> Observed {
        let health = &loaded.cache.health;
        if health.enumeration_failed {
            // An empty listing says nothing about which sessions exist.
            self.apply(|tracker| {
                tracker.baseline_unavailable();
                (Vec::new(), ())
            });
            return Observed::ReadFailed;
        }
        let rows: Vec<LifecycleRow> = loaded
            .instances
            .iter()
            .map(|inst| LifecycleRow::of(inst, &inst.effective_profile()))
            .collect();
        let unread: Vec<String> = health.unreadable_profiles.iter().cloned().collect();
        self.apply(|tracker| {
            if tracker.commits() != since {
                return (Vec::new(), Observed::Again);
            }
            let changes = tracker.observe(&rows, &unread);
            let observed = if tracker.has_suspects(&unread) {
                Observed::Again
            } else if unread.is_empty() {
                Observed::Settled
            } else {
                Observed::ReadFailed
            };
            (changes, observed)
        })
    }

    /// Apply one committed write of a profile's rows.
    pub(crate) fn on_commit(&self, before: &[LifecycleRow], after: &[LifecycleRow]) {
        self.apply(|tracker| (tracker.commit(before, after), ()));
    }

    /// Record what is on disk before any worker can list sessions, so every later change is
    /// reported against it. A commit that lands while loading makes the snapshot stale, and a
    /// failed profile listing is no snapshot, so both read again; the commit observer must
    /// already be installed to count commits. Returns whether a baseline was established.
    pub(super) async fn establish_baseline(
        self: &Arc<Self>,
        load: impl FnMut() -> LoadedInstances + Send + 'static,
    ) -> bool {
        let _reading = self.lock_reads().await;
        let feed = Arc::clone(self);
        tokio::task::spawn_blocking(move || feed.baseline_with(load))
            .await
            .unwrap_or(false)
    }

    /// Take the turn to read the store and apply what was read, held from before the read to
    /// after the apply. Without it a reader that is mid-read could apply an older snapshot
    /// over a newer one that another reader applied meanwhile.
    pub(super) async fn lock_reads(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.read_gate.lock().await
    }

    fn baseline_with(&self, load: impl FnMut() -> LoadedInstances) -> bool {
        let established = self.read_baseline(load);
        if !established {
            // Workers start anyway. Until a read is accepted, each profile starts from its
            // first commit's before rows instead of dropping the commit.
            self.apply(|tracker| {
                tracker.baseline_unavailable();
                (Vec::new(), ())
            });
        }
        established
    }

    fn read_baseline(&self, mut load: impl FnMut() -> LoadedInstances) -> bool {
        for _ in 0..BASELINE_ATTEMPTS {
            let since = self.commit_generation();
            let loaded = load();
            if loaded.cache.health.enumeration_failed {
                continue;
            }
            if self.observe_disk(since, &loaded) != Observed::Again {
                return true;
            }
        }
        false
    }

    /// Report every `Storage::update` commit of this process, until the guard drops. The
    /// observer runs under the storage lock, so it only takes the tracker briefly and sends on
    /// a broadcast channel.
    pub fn install_commit_observer(self: &Arc<Self>) -> CommitObserverGuard {
        let feed = Arc::clone(self);
        lifecycle::install_commit_observer(move |_profile, before, after| {
            feed.on_commit(before, after);
        })
    }

    /// Mutate the tracker and publish what it reports before releasing it, so subscribers see
    /// changes in the order the tracker applied them.
    fn apply<R>(&self, f: impl FnOnce(&mut LifecycleTracker) -> (Vec<LifecycleChange>, R)) -> R {
        let mut tracker = self.tracker.lock().unwrap_or_else(|p| p.into_inner());
        let (changes, result) = f(&mut tracker);
        for change in changes {
            #[cfg(test)]
            {
                let hook = self
                    .publish_hook
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                if let Some(hook) = hook {
                    hook(&change);
                }
            }
            // No subscriber (no plugin host) is fine.
            let _ = self.tx.send(change);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::lifecycle::LifecycleKind;
    use crate::session::{Instance, Storage};

    fn instance(title: &str, profile: &str) -> Instance {
        let mut inst = Instance::new(title, &format!("/tmp/feed-{title}"));
        inst.source_profile = profile.to_string();
        inst
    }

    /// A read that saw every profile.
    fn complete(instances: Vec<Instance>) -> LoadedInstances {
        LoadedInstances::from(instances)
    }

    /// A read that could not load the storage of `unread` profiles.
    fn partial(instances: Vec<Instance>, unread: &[&str]) -> LoadedInstances {
        let mut loaded = LoadedInstances::from(instances);
        loaded.cache.health.unreadable_profiles = unread.iter().map(|p| p.to_string()).collect();
        loaded
    }

    /// A read that could not even list the profiles.
    fn listing_failed() -> LoadedInstances {
        let mut loaded = LoadedInstances::default();
        loaded.cache.health.enumeration_failed = true;
        loaded
    }

    /// A complete disk read taken just now.
    fn observe(feed: &LifecycleFeed, instances: Vec<Instance>) -> bool {
        feed.observe_disk(feed.commit_generation(), &complete(instances)) == Observed::Again
    }

    #[test]
    fn observe_disk_seeds_then_publishes_with_the_shared_summary() {
        let feed = LifecycleFeed::new();
        let mut rx = feed.subscribe();
        let a = instance("a", "p");
        let b = instance("b", "p");

        assert!(!observe(&feed, vec![a.clone()]), "nothing is missing");
        assert!(rx.try_recv().is_err(), "the seed is silent");

        observe(&feed, vec![a, b.clone()]);
        let change = rx.try_recv().expect("b was created");
        assert_eq!(change.kind, LifecycleKind::Created);
        assert_eq!(change.session_id, b.id);
        assert_eq!(change.effective_profile, "p");
        assert_eq!(
            change.summary,
            Some(crate::plugin::host_api::session_summary(&b))
        );
    }

    #[test]
    fn observe_disk_asks_to_be_repeated_until_a_missing_row_is_confirmed_gone() {
        let feed = LifecycleFeed::new();
        let mut rx = feed.subscribe();
        let a = instance("a", "p");
        let b = instance("b", "p");
        observe(&feed, vec![a.clone(), b.clone()]);

        assert!(observe(&feed, vec![a.clone()]), "b is only a suspect");
        assert!(rx.try_recv().is_err());
        assert!(!observe(&feed, vec![a]), "the second miss confirms it");
        let change = rx.try_recv().expect("b was deleted");
        assert_eq!(change.kind, LifecycleKind::Deleted);
        assert_eq!(change.session_id, b.id);
    }

    /// A commit made in this process is published exactly once: the observer reports it, and
    /// the later disk diff finds nothing new.
    #[test]
    #[serial_test::serial]
    fn the_commit_observer_publishes_storage_commits_once() {
        let _home = crate::session::test_support::isolate_app_dir();
        let feed = LifecycleFeed::new();
        let mut rx = feed.subscribe();
        observe(&feed, Vec::new());
        let guard = feed.install_commit_observer();

        let inst = instance("committed", "feed-hook");
        Storage::new_unwatched("feed-hook")
            .unwrap()
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        drop(guard);

        let change = rx.try_recv().expect("the commit is published at once");
        assert_eq!(change.kind, LifecycleKind::Created);
        assert_eq!(change.session_id, inst.id);
        assert_eq!(change.effective_profile, "feed-hook");

        observe(&feed, vec![inst]);
        assert!(rx.try_recv().is_err(), "the disk diff must not repeat it");
    }

    fn row_of(inst: &Instance) -> LifecycleRow {
        LifecycleRow::of(inst, &inst.effective_profile())
    }

    /// A snapshot read before a commit must not be applied after it: that would undo the
    /// rename the commit just published, and report the rename back.
    #[test]
    fn a_disk_snapshot_a_commit_overtook_is_dropped() {
        let feed = LifecycleFeed::new();
        let mut rx = feed.subscribe();
        let before = instance("a", "p");
        observe(&feed, vec![before.clone()]);

        // The consumer takes its generation, then loads the old title from disk ...
        let since = feed.commit_generation();
        let stale = complete(vec![before.clone()]);
        // ... a rename commits and is published ...
        let mut after = before.clone();
        after.title = "renamed".to_string();
        feed.on_commit(&[row_of(&before)], &[row_of(&after)]);
        assert_eq!(
            rx.try_recv().expect("the commit is published").kind,
            LifecycleKind::Renamed
        );

        // ... and only then is the old snapshot observed.
        assert_eq!(
            feed.observe_disk(since, &stale),
            Observed::Again,
            "dropped, so observe again"
        );
        assert!(rx.try_recv().is_err(), "no reverse rename");

        assert!(!observe(&feed, vec![after]), "a fresh read agrees with it");
        assert!(rx.try_recv().is_err());
    }

    /// Both producers publish before the tracker is released, so a second producer cannot
    /// apply and publish its change between the first one's apply and its publication: the
    /// last notification a plugin gets is always the tracker's current state.
    #[test]
    fn every_publication_happens_under_the_tracker_lock() {
        let feed = LifecycleFeed::new();
        let a = instance("a", "p");
        observe(&feed, vec![a.clone()]);
        let titled = |title: &str| {
            let mut inst = a.clone();
            inst.title = title.to_string();
            inst
        };

        let held_while_publishing = Arc::new(Mutex::new(Vec::new()));
        let hook_feed = Arc::downgrade(&feed);
        let seen = Arc::clone(&held_while_publishing);
        *feed.publish_hook.lock().unwrap() = Some(Arc::new(move |_| {
            let feed = hook_feed.upgrade().expect("the feed outlives its hook");
            seen.lock().unwrap().push(feed.tracker.try_lock().is_err());
        }));

        observe(&feed, vec![titled("from disk")]);
        feed.on_commit(
            &[row_of(&titled("from disk"))],
            &[row_of(&titled("from commit"))],
        );
        assert_eq!(
            *held_while_publishing.lock().unwrap(),
            [true, true],
            "the disk producer and the commit producer each published under the lock"
        );
    }

    /// A failed profile listing says nothing about the sessions on disk, so it is not a
    /// baseline: read again, and report plainly when none could be established.
    #[test]
    fn a_failed_profile_listing_is_not_a_baseline() {
        let feed = LifecycleFeed::new();
        let mut reads = 0;
        let established = feed.baseline_with(|| {
            reads += 1;
            if reads == 1 {
                listing_failed()
            } else {
                complete(vec![instance("a", "p")])
            }
        });
        assert!(established, "the second read listed the profiles");
        assert_eq!(reads, 2);

        let feed = LifecycleFeed::new();
        assert!(!feed.baseline_with(listing_failed));
    }

    /// With no baseline at all, a commit still reports against its own before rows, so a
    /// worker that listed the profile is not left behind until the next reload.
    #[test]
    fn a_commit_after_a_failed_baseline_is_reported_against_its_before_rows() {
        let feed = LifecycleFeed::new();
        let mut rx = feed.subscribe();
        assert!(!feed.baseline_with(listing_failed));

        let before = instance("a", "p");
        let mut after = before.clone();
        after.archive();
        feed.on_commit(&[row_of(&before)], &[row_of(&after)]);
        assert_eq!(rx.try_recv().unwrap().kind, LifecycleKind::Archived);

        // The listing recovers and agrees: nothing is announced again.
        assert!(!observe(&feed, vec![after]));
        assert!(rx.try_recv().is_err());
    }

    /// The baseline and the disk consumer both read the store, so they take turns: a reader
    /// that is mid-read would otherwise apply an older snapshot over the baseline.
    #[tokio::test]
    async fn the_baseline_waits_for_a_reader_that_is_mid_read() {
        use std::sync::atomic::{AtomicU8, Ordering};
        // 0: not read yet, 1: read with the turn held, 2: read without it.
        let read = Arc::new(AtomicU8::new(0));
        let feed = LifecycleFeed::new();

        let consumer = feed.lock_reads().await;
        let baseline = feed.establish_baseline({
            let (read, feed) = (Arc::clone(&read), Arc::clone(&feed));
            move || {
                let held = feed.read_gate.try_lock().is_err();
                read.store(if held { 1 } else { 2 }, Ordering::SeqCst);
                complete(Vec::new())
            }
        });
        tokio::pin!(baseline);
        assert!(futures_util::poll!(&mut baseline).is_pending());
        assert_ne!(
            read.load(Ordering::SeqCst),
            2,
            "the baseline must hold the turn while it reads"
        );

        drop(consumer);
        assert!(baseline.await);
        assert_eq!(
            read.load(Ordering::SeqCst),
            1,
            "it read only once it had the turn, and held it throughout"
        );
    }

    /// If commits overtake every baseline read, the tracker is still uninitialised when
    /// workers start. A change after their list must be reported against its own before
    /// rows, not dropped and then absorbed by the next accepted read.
    #[test]
    fn a_baseline_overtaken_on_every_read_still_reports_later_commits() {
        let feed = LifecycleFeed::new();
        let mut rx = feed.subscribe();
        let a = instance("a", "p");
        let mut archived = a.clone();
        archived.archive();

        let mut reads = 0;
        let established = feed.baseline_with(|| {
            reads += 1;
            // Any commit advances the counter, even one that changes no session.
            feed.on_commit(&[], &[]);
            complete(vec![a.clone()])
        });
        assert!(!established);
        assert_eq!(reads, BASELINE_ATTEMPTS);
        assert!(rx.try_recv().is_err());

        feed.on_commit(&[row_of(&a)], &[row_of(&archived)]);
        assert_eq!(rx.try_recv().unwrap().kind, LifecycleKind::Archived);

        // The next accepted read agrees and announces nothing.
        assert!(!observe(&feed, vec![archived]));
        assert!(rx.try_recv().is_err());
    }

    /// A commit that lands while the baseline is read makes that read stale, and the baseline
    /// must not adopt it: the sessions would then silently miss the change.
    #[test]
    fn the_baseline_rereads_when_a_commit_lands_during_the_read() {
        let feed = LifecycleFeed::new();
        let mut rx = feed.subscribe();
        let before = instance("a", "p");
        let mut after = before.clone();
        after.title = "renamed".to_string();

        let mut reads = 0;
        feed.baseline_with(|| {
            reads += 1;
            if reads == 1 {
                // The disk still holds the old title when it is read ...
                let loaded = complete(vec![before.clone()]);
                // ... and the rename commits before the read reaches the tracker.
                feed.on_commit(&[row_of(&before)], &[row_of(&after)]);
                loaded
            } else {
                complete(vec![after.clone()])
            }
        });
        assert_eq!(reads, 2, "the stale read was discarded and read again");
        assert!(rx.try_recv().is_err(), "the baseline itself is silent");

        // The baseline holds the renamed session, so nothing is reported for it now ...
        assert!(!observe(&feed, vec![after.clone()]));
        assert!(rx.try_recv().is_err());
        // ... and the next change is reported against it.
        let mut archived = after.clone();
        archived.archive();
        feed.on_commit(&[row_of(&after)], &[row_of(&archived)]);
        assert_eq!(rx.try_recv().unwrap().kind, LifecycleKind::Archived);
    }

    /// A missing session of a profile that then turns unreadable cannot be confirmed or
    /// cleared, so it must not keep asking for another quick read: that would reload every
    /// store four times a second for as long as the profile stays unreadable.
    #[test]
    fn a_suspect_in_a_profile_that_turns_unreadable_does_not_ask_for_a_quick_retry() {
        let feed = LifecycleFeed::new();
        let kept = instance("kept", "p1");
        let gone = instance("gone", "p2");
        observe(&feed, vec![kept.clone(), gone.clone()]);

        assert!(observe(&feed, vec![kept.clone()]), "gone is a suspect");
        let unreadable = |feed: &LifecycleFeed| {
            feed.observe_disk(
                feed.commit_generation(),
                &partial(vec![kept.clone()], &["p2"]),
            )
        };
        assert_eq!(
            unreadable(&feed),
            Observed::ReadFailed,
            "p2 cannot be looked at again yet"
        );
        assert_eq!(unreadable(&feed), Observed::ReadFailed);

        // It reads again and gone is still missing: now it is confirmed.
        let mut rx = feed.subscribe();
        assert!(!observe(&feed, vec![kept]), "the second miss confirms it");
        assert_eq!(rx.try_recv().unwrap().kind, LifecycleKind::Deleted);
    }

    /// A profile whose storage failed to load is not a profile with no sessions.
    #[test]
    fn an_unreadable_profile_is_not_reported_deleted_and_back_created() {
        let feed = LifecycleFeed::new();
        let mut rx = feed.subscribe();
        let kept = instance("kept", "p1");
        let held = instance("held", "p2");
        observe(&feed, vec![kept.clone(), held.clone()]);

        let failed_read = |feed: &LifecycleFeed| {
            feed.observe_disk(
                feed.commit_generation(),
                &partial(vec![kept.clone()], &["p2"]),
            )
        };
        // Two failed reads, as when the scheduled retry also fails.
        assert_eq!(failed_read(&feed), Observed::ReadFailed);
        assert_eq!(failed_read(&feed), Observed::ReadFailed);
        assert!(rx.try_recv().is_err(), "held was not deleted");

        // The profile recovers with its session intact: nothing is created.
        observe(&feed, vec![kept.clone(), held]);
        assert!(rx.try_recv().is_err(), "held was not created again");

        // Failing to list profiles at all is no evidence either.
        for _ in 0..2 {
            assert_eq!(
                feed.observe_disk(feed.commit_generation(), &listing_failed()),
                Observed::ReadFailed
            );
        }
        assert!(rx.try_recv().is_err(), "an empty listing deleted nothing");
    }

    /// A wake consumed by a transient read failure must still publish the change once the
    /// read recovers, with no further event; a persistent failure stops asking after its
    /// budget, and fresh activity or a healthy read refills it.
    #[test]
    fn read_failures_are_retried_within_a_bounded_budget() {
        let mut retry = ReadRetry::default();
        let failed = |retry: &mut ReadRetry, fresh| retry.next(Observed::ReadFailed, fresh);

        let delays: Vec<_> = std::iter::once(failed(&mut retry, true))
            .chain((1..READ_RETRY_ATTEMPTS).map(|_| failed(&mut retry, false)))
            .collect();
        let want: Vec<_> = (0..READ_RETRY_ATTEMPTS)
            .map(|n| Some(READ_RETRY_BASE_DELAY * 2u32.pow(n)))
            .collect();
        assert_eq!(delays, want, "each retry waits twice as long");
        assert_eq!(failed(&mut retry, false), None, "the budget is spent");
        assert_eq!(failed(&mut retry, false), None);

        assert!(failed(&mut retry, true).is_some(), "fresh activity refills");
        assert_eq!(retry.next(Observed::Settled, false), None);
        assert_eq!(
            failed(&mut retry, false),
            Some(READ_RETRY_BASE_DELAY),
            "a healthy read refills"
        );

        let mut retry = ReadRetry::default();
        for _ in 0..=READ_RETRY_ATTEMPTS {
            assert_eq!(retry.next(Observed::Again, false), Some(AGAIN_DELAY));
        }
        assert!(
            failed(&mut retry, false).is_some(),
            "a second look does not draw on the read budget"
        );
    }

    #[test]
    fn a_rename_consumed_by_a_failed_read_is_published_when_the_retry_reads() {
        let feed = LifecycleFeed::new();
        let mut rx = feed.subscribe();
        let a = instance("a", "p1");
        let b = instance("b", "p2");
        observe(&feed, vec![a.clone(), b.clone()]);
        let mut renamed = b.clone();
        renamed.title = "renamed".to_string();

        let mut retry = ReadRetry::default();
        let read = |feed: &LifecycleFeed, loaded: LoadedInstances| {
            feed.observe_disk(feed.commit_generation(), &loaded)
        };
        // The wake finds p2 unreadable ...
        let observed = read(&feed, partial(vec![a.clone()], &["p2"]));
        assert_eq!(observed, Observed::ReadFailed);
        assert!(retry.next(observed, true).is_some(), "a retry is scheduled");
        assert!(rx.try_recv().is_err());
        // ... and with no new event the retry reads it back.
        let observed = read(&feed, complete(vec![a, renamed]));
        assert_eq!(observed, Observed::Settled);
        assert_eq!(rx.try_recv().unwrap().kind, LifecycleKind::Renamed);
        assert_eq!(retry.next(observed, false), None);
    }
}
