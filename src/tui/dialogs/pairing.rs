//! Pairing block of the exposed serve view: keeps a valid one-time code on
//! screen, minted on the local daemon and replaced when it expires or a device
//! redeems it, lists paired devices with revoke, and lists IPs locked out by
//! failed attempts with a key to let them back in.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use serde::Deserialize;
use tokio::sync::mpsc;

const CONFIRM_WINDOW: Duration = Duration::from_secs(3);
/// How often the device list is re-read, which is also how soon a redeemed
/// code is replaced.
const DEVICE_REFRESH: Duration = Duration::from_secs(3);

enum CodeState {
    Minting,
    Ready { code: String, expires_at: Instant },
    Failed(String),
}

#[derive(Clone, Deserialize)]
struct PairedDevice {
    session_id: String,
    device_name: Option<String>,
    #[serde(default)]
    created_ip: String,
    last_seen: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Deserialize)]
struct Lockout {
    ip: String,
    remaining_secs: u64,
}

#[derive(Deserialize)]
struct Minted {
    code: String,
    expires_in_secs: u64,
}

enum Update {
    Minted(Result<Minted, String>),
    Devices(Result<Vec<PairedDevice>, String>),
    Revoked(Result<(), String>),
    /// A daemon without the lockout routes answers with an error; it shows none.
    Lockouts(Vec<Lockout>),
    Unblocked,
}

pub(super) struct PairingPanel {
    code: CodeState,
    /// `None` until the first listing arrives.
    devices: Option<Result<Vec<PairedDevice>, String>>,
    /// Devices paired before the code on screen; one outside it redeemed it.
    known_devices: Option<HashSet<String>>,
    selected: usize,
    confirm_revoke: Option<(String, Instant)>,
    /// With when they were read, so the time left counts down between reads.
    lockouts: (Vec<Lockout>, Instant),
    runtime: Option<tokio::runtime::Handle>,
    sender: mpsc::UnboundedSender<Update>,
    updates: mpsc::UnboundedReceiver<Update>,
    next_refresh: Instant,
    drawn_secs: Option<u64>,
}

async fn local_client() -> Result<crate::daemon::DaemonClient, String> {
    crate::acp::client::discovery::discover_local()
        .map_err(|e| e.to_string())?
        .daemon_client()
        .map_err(|e| e.to_string())
}

impl PairingPanel {
    /// Start minting a code and listing devices.
    pub(super) fn open() -> Self {
        let mut panel = Self::detached(tokio::runtime::Handle::try_current().ok());
        panel.mint();
        panel.refresh_devices(Instant::now());
        panel
    }

    fn detached(runtime: Option<tokio::runtime::Handle>) -> Self {
        let (sender, updates) = mpsc::unbounded_channel();
        Self {
            code: CodeState::Minting,
            devices: None,
            known_devices: None,
            selected: 0,
            confirm_revoke: None,
            lockouts: (Vec::new(), Instant::now()),
            runtime,
            sender,
            updates,
            next_refresh: Instant::now(),
            drawn_secs: None,
        }
    }

    #[cfg(test)]
    pub(super) fn ready(code: &str, devices: &[&str]) -> Self {
        let mut panel = Self::detached(None);
        panel.apply(Update::Minted(Ok(Minted {
            code: code.into(),
            expires_in_secs: 600,
        })));
        panel.apply(Update::Devices(Ok(devices
            .iter()
            .map(|name| PairedDevice {
                session_id: name.to_string(),
                device_name: Some(name.to_string()),
                created_ip: "192.168.1.9".to_string(),
                last_seen: chrono::Utc::now() - chrono::Duration::minutes(2),
            })
            .collect())));
        panel
    }

    #[cfg(test)]
    pub(super) fn with_lockout(mut self, ip: &str, remaining_secs: u64) -> Self {
        self.apply(Update::Lockouts(vec![Lockout {
            ip: ip.into(),
            remaining_secs,
        }]));
        self
    }

    fn spawn(&self, work: impl std::future::Future<Output = Update> + Send + 'static) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let _ = sender.send(work.await);
        });
    }

    fn mint(&mut self) {
        if self.runtime.is_none() {
            self.code = CodeState::Failed("No async runtime to reach the daemon.".into());
            return;
        }
        self.code = CodeState::Minting;
        self.known_devices = self.device_ids();
        self.spawn(async {
            let result = async {
                local_client()
                    .await?
                    .post_api::<_, Minted>(&["pair", "codes"], &serde_json::json!({}))
                    .await
                    .map_err(|e| e.summary())
            };
            Update::Minted(result.await)
        });
    }

    fn refresh_devices(&mut self, now: Instant) {
        self.next_refresh = now + DEVICE_REFRESH;
        self.refresh_lockouts();
        self.spawn(async {
            let result = async {
                local_client()
                    .await?
                    .get_api::<Vec<PairedDevice>>(&["devices"], &[])
                    .await
                    .map_err(|e| e.summary())
            };
            Update::Devices(result.await.map(|all| {
                all.into_iter()
                    .filter(|d| d.device_name.is_some())
                    .collect()
            }))
        });
    }

    fn refresh_lockouts(&self) {
        self.spawn(async {
            let result = async {
                local_client()
                    .await?
                    .get_api::<Vec<Lockout>>(&["pair", "lockouts"], &[])
                    .await
                    .map_err(|e| e.summary())
            };
            Update::Lockouts(result.await.unwrap_or_default())
        });
    }

    fn unblock(&self) {
        self.spawn(async {
            if let Ok(client) = local_client().await {
                let _ = client.delete_api(&["pair", "lockouts"]).await;
            }
            Update::Unblocked
        });
    }

    fn revoke(&mut self, session_id: String) {
        self.spawn(async move {
            let result = async {
                local_client()
                    .await?
                    .delete_api(&["login", "sessions", &session_id])
                    .await
                    .map_err(|e| e.summary())
            };
            Update::Revoked(result.await)
        });
    }

    fn device_list(&self) -> &[PairedDevice] {
        match &self.devices {
            Some(Ok(devices)) => devices,
            _ => &[],
        }
    }

    fn device_ids(&self) -> Option<HashSet<String>> {
        match &self.devices {
            Some(Ok(devices)) => Some(devices.iter().map(|d| d.session_id.clone()).collect()),
            _ => None,
        }
    }

    fn apply(&mut self, update: Update) {
        match update {
            Update::Minted(Ok(minted)) => {
                self.code = CodeState::Ready {
                    code: minted.code,
                    expires_at: Instant::now() + Duration::from_secs(minted.expires_in_secs),
                };
            }
            Update::Minted(Err(error)) => self.code = CodeState::Failed(error),
            Update::Devices(devices) => {
                self.devices = Some(devices);
                self.selected = self
                    .selected
                    .min(self.device_list().len().saturating_sub(1));
                let current = self.device_ids();
                match (&self.known_devices, current) {
                    (Some(known), Some(current)) if !current.is_subset(known) => {
                        if matches!(self.code, CodeState::Ready { .. }) {
                            self.mint();
                        }
                    }
                    (None, current) => self.known_devices = current,
                    _ => {}
                }
            }
            Update::Revoked(Ok(())) => self.refresh_devices(Instant::now()),
            Update::Revoked(Err(error)) => self.devices = Some(Err(error)),
            Update::Lockouts(lockouts) => self.lockouts = (lockouts, Instant::now()),
            Update::Unblocked => self.refresh_lockouts(),
        }
    }

    /// Device selection and revoke. Returns whether the key was used.
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> bool {
        let confirmed = self
            .confirm_revoke
            .take()
            .filter(|(_, at)| at.elapsed() <= CONFIRM_WINDOW)
            .map(|(id, _)| id);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                let last = self.device_list().len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
            }
            KeyCode::Char('u') if self.has_lockouts() => {
                self.lockouts.0.clear();
                self.unblock();
            }
            KeyCode::Char('x') | KeyCode::Char('X') => {
                if let Some(device) = self.device_list().get(self.selected) {
                    let id = device.session_id.clone();
                    if confirmed.as_deref() == Some(id.as_str()) {
                        self.revoke(id);
                    } else {
                        self.confirm_revoke = Some((id, Instant::now()));
                    }
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn has_lockouts(&self) -> bool {
        !self.lockouts.0.is_empty()
    }

    /// Locked-out IPs with the seconds left, longest first.
    pub(super) fn lockouts(&self) -> Vec<(&str, u64)> {
        let (lockouts, read_at) = &self.lockouts;
        let elapsed = read_at.elapsed().as_secs();
        lockouts
            .iter()
            .filter_map(|lockout| {
                let left = lockout.remaining_secs.checked_sub(elapsed)?.max(1);
                Some((lockout.ip.as_str(), left))
            })
            .collect()
    }

    /// Drain background results, keep a live code on screen and advance the
    /// countdown. Returns true when a redraw is needed.
    pub(super) fn tick(&mut self) -> bool {
        self.tick_at(Instant::now())
    }

    fn tick_at(&mut self, now: Instant) -> bool {
        let mut changed = false;
        while let Ok(update) = self.updates.try_recv() {
            self.apply(update);
            changed = true;
        }
        if matches!(&self.code, CodeState::Ready { expires_at, .. } if *expires_at <= now) {
            self.mint();
            changed = true;
        }
        if now >= self.next_refresh {
            self.refresh_devices(now);
        }
        if self
            .confirm_revoke
            .as_ref()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) > CONFIRM_WINDOW)
        {
            self.confirm_revoke = None;
            changed = true;
        }
        let secs = match &self.code {
            CodeState::Ready { expires_at, .. } => {
                Some(expires_at.saturating_duration_since(now).as_secs())
            }
            _ => None,
        };
        if secs != self.drawn_secs {
            self.drawn_secs = secs;
            changed = true;
        }
        changed
    }

    pub(super) fn code(&self) -> CodeView<'_> {
        match &self.code {
            CodeState::Minting => CodeView::Minting,
            CodeState::Ready { code, expires_at } => CodeView::Ready {
                spaced: spaced(code),
                expires_in: countdown(expires_at.saturating_duration_since(Instant::now())),
            },
            CodeState::Failed(error) => CodeView::Failed(error),
        }
    }

    /// Paired devices as rows, or why there are none to show: `Err(None)`
    /// while the first listing loads.
    pub(super) fn devices(&self) -> Result<Vec<DeviceRow<'_>>, Option<&str>> {
        let devices = match &self.devices {
            None => return Err(None),
            Some(Err(error)) => return Err(Some(error)),
            Some(Ok(devices)) => devices,
        };
        let now = chrono::Utc::now();
        let armed = self
            .confirm_revoke
            .as_ref()
            .filter(|(_, at)| at.elapsed() <= CONFIRM_WINDOW)
            .map(|(id, _)| id.as_str());
        Ok(devices
            .iter()
            .enumerate()
            .map(|(index, device)| DeviceRow {
                name: device.device_name.as_deref().unwrap_or_default(),
                address: device.created_ip.as_str(),
                seen: match (now - device.last_seen).num_seconds().max(0) as u64 {
                    0..60 => "just now".to_string(),
                    secs => format!("{} ago", ago(secs)),
                },
                selected: index == self.selected,
                armed: armed == Some(device.session_id.as_str()),
            })
            .collect())
    }
}

pub(super) enum CodeView<'a> {
    Minting,
    Ready { spaced: String, expires_in: String },
    Failed(&'a str),
}

pub(super) struct DeviceRow<'a> {
    pub(super) name: &'a str,
    pub(super) address: &'a str,
    pub(super) seen: String,
    pub(super) selected: bool,
    /// The first `x` landed on it; a second revokes.
    pub(super) armed: bool,
}

/// `K7F-3QX` as `K 7 F - 3 Q X`, easier to read across a room.
fn spaced(code: &str) -> String {
    code.chars().map(String::from).collect::<Vec<_>>().join(" ")
}

fn countdown(remaining: Duration) -> String {
    let secs = remaining.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub(super) fn ago(secs: u64) -> String {
    match secs {
        0..60 => "<1m".to_string(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn press(panel: &mut PairingPanel, code: KeyCode) -> bool {
        panel.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn devices(ids: &[&str]) -> Update {
        Update::Devices(Ok(ids
            .iter()
            .map(|id| PairedDevice {
                session_id: id.to_string(),
                device_name: Some(id.to_string()),
                created_ip: String::new(),
                last_seen: chrono::Utc::now(),
            })
            .collect()))
    }

    #[test]
    fn revoke_needs_a_second_press_on_the_same_device() {
        let mut panel = PairingPanel::ready("K7F-3QX", &["laptop", "phone"]);
        press(&mut panel, KeyCode::Char('x'));
        let rows = panel.devices().unwrap();
        assert!(rows[0].armed && !rows[1].armed);
        assert_eq!(rows[0].seen, "2m ago");
        press(&mut panel, KeyCode::Down);
        assert!(panel.confirm_revoke.is_none(), "any other key cancels");
        press(&mut panel, KeyCode::Char('x'));
        press(&mut panel, KeyCode::Char('x'));
        assert!(panel.confirm_revoke.is_none());
        assert_eq!(panel.selected, 1);
        assert!(
            !press(&mut panel, KeyCode::Char('c')),
            "other keys fall through"
        );
    }

    #[test]
    fn lockouts_count_down_and_u_lets_them_back_in() {
        let mut panel = PairingPanel::ready("K7F-3QX", &[]);
        assert!(!press(&mut panel, KeyCode::Char('u')), "nothing to unblock");
        panel.apply(Update::Lockouts(vec![Lockout {
            ip: "100.89.98.53".into(),
            remaining_secs: 725,
        }]));
        assert_eq!(panel.lockouts(), [("100.89.98.53", 725)]);
        assert!(press(&mut panel, KeyCode::Char('u')));
        assert!(panel.lockouts().is_empty());
    }

    #[tokio::test]
    async fn a_new_code_replaces_one_that_expired_or_was_redeemed() {
        let mut panel = PairingPanel::detached(Some(tokio::runtime::Handle::current()));
        panel.apply(devices(&["laptop"]));
        panel.apply(Update::Minted(Ok(Minted {
            code: "K7F-3QX".into(),
            expires_in_secs: 600,
        })));

        panel.tick_at(Instant::now() + Duration::from_secs(1));
        assert!(matches!(panel.code, CodeState::Ready { .. }), "still live");
        panel.tick_at(Instant::now() + Duration::from_secs(601));
        assert!(matches!(panel.code, CodeState::Minting), "expired");

        panel.apply(Update::Minted(Ok(Minted {
            code: "M2P-9TR".into(),
            expires_in_secs: 600,
        })));
        panel.apply(devices(&["laptop"]));
        assert!(
            matches!(panel.code, CodeState::Ready { .. }),
            "no new device"
        );
        panel.apply(devices(&["laptop", "phone"]));
        assert!(matches!(panel.code, CodeState::Minting), "redeemed");
    }
}
