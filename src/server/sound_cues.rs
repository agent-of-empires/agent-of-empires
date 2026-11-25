//! Status sound timing, shared by the dashboard's `sound_due` cues and the
//! Daemon's own host playback. Waiting, Idle and Error sound only once they
//! have held for their dwell, so a flapping Session stays quiet.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;

use super::push::{NotificationEvent, StatusChange};
use super::AppState;
use crate::session::{Instance, Status};
use crate::sound::SoundEvent;

/// Pending dwell per session; a later move replaces the entry, so a flap never cues.
#[derive(Default)]
pub(super) struct Dwells(HashMap<String, (Instant, NotificationEvent)>);

impl Dwells {
    fn track(&mut self, id: &str, status: Status, now: Instant) {
        match NotificationEvent::for_status(status) {
            Some(event) => {
                let due = now + Duration::from_millis(event.dwell_ms());
                self.0.insert(id.to_string(), (due, event));
            }
            None => {
                self.0.remove(id);
            }
        }
    }

    pub(super) fn on_change(&mut self, change: &StatusChange, now: Instant) {
        self.track(&change.instance_id, change.new, now);
    }

    /// A dwell for every session already in a cueing status, so a new connection still cues a
    /// status that started before it; the client drops a cue it already sounded.
    pub(super) async fn seeded(state: &AppState) -> Self {
        let now = Instant::now();
        let mut dwells = Self::default();
        for inst in state.instances.read().await.iter() {
            dwells.track(&inst.id, inst.status, now);
        }
        dwells
    }

    pub(super) fn next_due(&self) -> Option<Instant> {
        self.0.values().map(|(due, _)| *due).min()
    }

    pub(super) fn take_due(&mut self, now: Instant) -> Vec<(String, NotificationEvent)> {
        let ids: Vec<String> = self
            .0
            .iter()
            .filter(|(_, (due, _))| *due <= now)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.0.remove(&id).map(|(_, event)| (id, event)))
            .collect()
    }
}

/// Plays status sounds on the Daemon's host: Start and Running at once, the rest after their dwell.
pub fn spawn_host_player(state: Arc<AppState>) {
    let rx = state.status_tx.subscribe();
    let work = state.runtime.work.clone();
    work.spawn(
        "server.host_sound",
        run_host_player(state, rx, |profile, event| {
            tokio::task::spawn_blocking(move || play_on_host(&profile, event));
        }),
    );
}

fn play_on_host(profile: &str, event: SoundEvent) {
    match crate::session::resolve_config(profile) {
        Ok(config) if config.sound.host_plays(event) => {
            crate::sound::play(event, config.sound.server_volume)
        }
        Ok(_) => {}
        Err(e) => {
            tracing::debug!(target: "sound.playback", profile, "No host sound, config unreadable: {e}")
        }
    }
}

/// `play` gets the Session's profile, whose `[sound]` decides whether and how loud it plays.
async fn run_host_player(
    state: Arc<AppState>,
    mut rx: broadcast::Receiver<StatusChange>,
    play: impl Fn(String, SoundEvent),
) {
    let mut dwells = Dwells::default();
    loop {
        let sleep = match dwells.next_due() {
            Some(due) => tokio::time::sleep_until(due),
            None => tokio::time::sleep(Duration::from_secs(3600)),
        };
        let due = tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => return,
            recv = rx.recv() => match recv {
                Ok(change) => {
                    dwells.on_change(&change, Instant::now());
                    match SoundEvent::for_status(change.new) {
                        Some(event @ (SoundEvent::Start | SoundEvent::Running)) => {
                            vec![(change.instance_id, event)]
                        }
                        _ => continue,
                    }
                }
                // Dropped moves leave no way to tell which dwells still hold.
                Err(RecvError::Lagged(_)) => {
                    dwells = Dwells::default();
                    continue;
                }
                Err(RecvError::Closed) => return,
            },
            _ = sleep => dwells
                .take_due(Instant::now())
                .into_iter()
                .map(|(id, event)| (id, sound_for(event)))
                .collect(),
        };
        let instances = state.instances.read().await;
        // Sessions whose dwell ends together play one sound per event.
        let mut sounded = HashSet::new();
        for (id, event) in due {
            if let Some(inst) = instances.iter().find(|i| i.id == id) {
                if host_eligible(inst, event) && sounded.insert(event) {
                    play(inst.effective_profile(), event);
                }
            }
        }
    }
}

fn sound_for(event: NotificationEvent) -> SoundEvent {
    match event {
        NotificationEvent::Waiting => SoundEvent::Waiting,
        NotificationEvent::Idle => SoundEvent::Idle,
        NotificationEvent::Error => SoundEvent::Error,
    }
}

/// The dashboard's rule too: a dismissed Session, or one muted for this event's push, stays silent.
fn host_eligible(inst: &Instance, event: SoundEvent) -> bool {
    if inst.is_dismissed() || SoundEvent::for_status(inst.status) != Some(event) {
        return false;
    }
    let muted = match event {
        SoundEvent::Waiting => inst.notify_on_waiting,
        SoundEvent::Idle => inst.notify_on_idle,
        SoundEvent::Error => inst.notify_on_error,
        _ => None,
    };
    muted != Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Timestamp;

    fn change(id: &str, new: Status) -> StatusChange {
        StatusChange {
            instance_id: id.to_string(),
            instance_title: id.to_string(),
            effective_profile: "default".to_string(),
            old: Status::Running,
            new,
            at: Timestamp::now(),
        }
    }

    #[test]
    fn dwell_cues_after_the_status_holds_and_a_later_move_cancels_it() {
        let t0 = Instant::now();
        let mut dwells = Dwells::default();
        dwells.on_change(&change("a", Status::Waiting), t0);
        dwells.on_change(&change("b", Status::Idle), t0);
        dwells.on_change(&change("c", Status::Running), t0);
        assert_eq!(dwells.next_due(), Some(t0 + Duration::from_millis(2_000)));

        let mut due = dwells.take_due(t0 + Duration::from_millis(2_000));
        due.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(due, vec![("b".to_string(), NotificationEvent::Idle)]);

        dwells.on_change(
            &change("a", Status::Running),
            t0 + Duration::from_millis(3_000),
        );
        assert!(dwells.take_due(t0 + Duration::from_secs(60)).is_empty());
        assert_eq!(dwells.next_due(), None);
    }

    #[test]
    fn host_eligible_skips_dismissed_muted_and_moved_sessions() {
        let at = |status: Status| {
            let mut inst = Instance::new("s", "/tmp/aoe-host-sound");
            inst.status = status;
            inst
        };
        let mut archived = at(Status::Idle);
        archived.archived_at = Some(Timestamp::now());
        let mut muted = at(Status::Idle);
        muted.notify_on_idle = Some(false);
        let mut muted_other = at(Status::Idle);
        muted_other.notify_on_error = Some(false);
        for (label, inst, event, plays) in [
            ("plain", at(Status::Idle), SoundEvent::Idle, true),
            ("start", at(Status::Starting), SoundEvent::Start, true),
            ("archived", archived, SoundEvent::Idle, false),
            ("muted", muted, SoundEvent::Idle, false),
            (
                "muted for another event",
                muted_other,
                SoundEvent::Idle,
                true,
            ),
            ("moved on", at(Status::Running), SoundEvent::Idle, false),
        ] {
            assert_eq!(host_eligible(&inst, event), plays, "{label}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn host_player_plays_running_at_once_and_idle_after_its_dwell() {
        let mut row = Instance::new("h", "/tmp/aoe-host-sound-h");
        row.status = Status::Running;
        row.source_profile = "work".to_string();
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        let (played_tx, mut played) = tokio::sync::mpsc::unbounded_channel();
        let player = tokio::spawn(run_host_player(
            state.clone(),
            state.status_tx.subscribe(),
            move |profile, event| played_tx.send((profile, event, Instant::now())).unwrap(),
        ));

        state.status_tx.send(change(&id, Status::Running)).unwrap();
        let (profile, event, _) = played.recv().await.unwrap();
        assert_eq!((profile.as_str(), event), ("work", SoundEvent::Running));

        set_status(&state, &id, Status::Idle).await;
        let sent = Instant::now();
        state.status_tx.send(change(&id, Status::Idle)).unwrap();
        let (_, event, at) = played.recv().await.unwrap();
        assert_eq!(event, SoundEvent::Idle);
        assert!(
            at - sent >= Duration::from_millis(2_000),
            "idle sounded before its dwell"
        );

        state.shutdown.cancel();
        player.await.unwrap();
    }

    async fn set_status(state: &AppState, id: &str, status: Status) {
        if let Some(inst) = state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|i| i.id == id)
        {
            inst.status = status;
        }
    }
}
