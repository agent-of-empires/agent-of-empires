//! Sound configuration, profile-level overrides, and volume helpers.

use aoe_settings_derive::SettingsSection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserScope {
    #[default]
    All,
    OtherSessions,
    HiddenTab,
}

fn is_default_scope(s: &BrowserScope) -> bool {
    *s == BrowserScope::All
}

#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "sound", category = "Sound")]
pub struct SoundConfig {
    /// Play built-in sounds when a Session changes status.
    #[serde(default)]
    #[setting(
        label = "Play sounds",
        widget = "toggle",
        summary = "A sound when a Session changes status, and a chime when a Structured view Session needs you."
    )]
    pub enabled: bool,

    /// Play status sounds on the machine running the Daemon.
    #[serde(default = "default_true")]
    #[setting(
        label = "On the computer running AoE",
        widget = "toggle",
        summary = "Plays through afplay on macOS, or aplay or paplay on Linux."
    )]
    pub server_enabled: bool,

    /// Play status sounds in the web dashboard for every session. Needs an
    /// open dashboard tab.
    #[serde(default = "default_true")]
    #[setting(
        label = "In the Web dashboard",
        widget = "toggle",
        summary = "Needs a Web dashboard tab open."
    )]
    pub browser_enabled: bool,

    /// Server playback volume (0.1 = min, 1.0 = normal, 1.5 = max), step 0.1.
    /// Ignored when aplay is the Linux backend.
    #[serde(default = "default_volume", skip_serializing_if = "is_default_volume")]
    #[setting(
        label = "Volume",
        widget = "custom:sound-volume",
        summary = "From 0.1 to 1.5. Ignored on Linux when sound plays through aplay."
    )]
    pub server_volume: f64,

    /// Browser playback volume (0.1 = min, 1.0 = normal, 1.5 = max), step 0.1.
    #[serde(default = "default_volume", skip_serializing_if = "is_default_volume")]
    #[setting(
        label = "Volume",
        widget = "custom:sound-volume",
        summary = "From 0.1 to 1.5."
    )]
    pub browser_volume: f64,

    /// Which sessions sound in the browser: all of them, every session except
    /// the one open in that tab, or only while the tab is hidden.
    #[serde(default, skip_serializing_if = "is_default_scope")]
    #[setting(
        label = "Which Sessions",
        widget = "select",
        options = "all:All sessions,other_sessions:Other sessions,hidden_tab:Hidden tab only",
        summary = "Every Session · Every Session except the one open in this tab · Only while this tab is hidden."
    )]
    pub browser_scope: BrowserScope,

    /// Play the start sound on the machine running aoe.
    #[serde(default = "default_true")]
    #[setting(label = "Server: Start", widget = "toggle", advanced)]
    pub server_start_enabled: bool,

    /// Play the running sound on the machine running aoe.
    #[serde(default = "default_true")]
    #[setting(label = "Server: Running", widget = "toggle", advanced)]
    pub server_running_enabled: bool,

    /// Play the waiting sound on the machine running aoe.
    #[serde(default = "default_true")]
    #[setting(label = "Server: Waiting", widget = "toggle", advanced)]
    pub server_waiting_enabled: bool,

    /// Play the idle sound on the machine running aoe.
    #[serde(default = "default_true")]
    #[setting(label = "Server: Idle", widget = "toggle", advanced)]
    pub server_idle_enabled: bool,

    /// Play the error sound on the machine running aoe.
    #[serde(default = "default_true")]
    #[setting(label = "Server: Error", widget = "toggle", advanced)]
    pub server_error_enabled: bool,

    /// Play the approval chime in the web dashboard when a Structured view
    /// Session asks for permission or a question.
    #[serde(default = "default_true")]
    #[setting(label = "Browser: Approval", widget = "toggle", advanced)]
    pub browser_approval_enabled: bool,

    /// Play the waiting sound in the web dashboard.
    #[serde(default = "default_true")]
    #[setting(label = "Browser: Waiting", widget = "toggle", advanced)]
    pub browser_waiting_enabled: bool,

    /// Play the idle sound in the web dashboard.
    #[serde(default = "default_true")]
    #[setting(label = "Browser: Idle", widget = "toggle", advanced)]
    pub browser_idle_enabled: bool,

    /// Play the error sound in the web dashboard.
    #[serde(default = "default_true")]
    #[setting(label = "Browser: Error", widget = "toggle", advanced)]
    pub browser_error_enabled: bool,
}

impl Default for SoundConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_enabled: true,
            browser_enabled: true,
            server_volume: default_volume(),
            browser_volume: default_volume(),
            browser_scope: BrowserScope::default(),
            server_start_enabled: true,
            server_running_enabled: true,
            server_waiting_enabled: true,
            server_idle_enabled: true,
            server_error_enabled: true,
            browser_approval_enabled: true,
            browser_waiting_enabled: true,
            browser_idle_enabled: true,
            browser_error_enabled: true,
        }
    }
}

impl SoundConfig {
    /// Whether the Daemon plays `event` on its host. Approval has no host sound.
    pub fn host_plays(&self, event: super::SoundEvent) -> bool {
        use super::SoundEvent;
        self.enabled
            && self.server_enabled
            && match event {
                SoundEvent::Start => self.server_start_enabled,
                SoundEvent::Running => self.server_running_enabled,
                SoundEvent::Waiting => self.server_waiting_enabled,
                SoundEvent::Idle => self.server_idle_enabled,
                SoundEvent::Error => self.server_error_enabled,
                SoundEvent::Approval => false,
            }
    }
}

fn default_true() -> bool {
    true
}

pub(super) fn default_volume() -> f64 {
    1.0
}

pub(super) fn is_default_volume(v: &f64) -> bool {
    (*v - 1.0).abs() < 1e-9
}

pub fn volume_options() -> Vec<String> {
    (1..=15).map(|i| format!("{:.1}", i as f64 * 0.1)).collect()
}

pub fn volume_to_index(v: f64) -> usize {
    ((v.clamp(0.1, 1.5) / 0.1).round() as usize).min(15) - 1
}

pub fn volume_from_option(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(1.0).clamp(0.1, 1.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_tolerates_removed_fields() {
        // `mode` and the `on_*` file overrides are gone; old configs must still load.
        for toml in [
            "",
            "enabled = true
mode = { specific = \"wololo\" }",
            "enabled = true
on_idle = \"idle.wav\"
on_approval = \"gem\"",
        ] {
            let config: SoundConfig = toml::from_str(toml).expect(toml);
            assert_eq!(config.enabled, !toml.is_empty(), "{toml}");
            // A 0.0 default would mute playback on a fresh install.
            assert!((config.server_volume - 1.0).abs() < 1e-9, "{toml}");
        }
    }

    #[test]
    fn host_plays_needs_the_master_target_and_event_switches() {
        use super::super::SoundEvent::{Approval, Error, Idle, Start};
        for (toml, event, plays) in [
            ("", Idle, false),
            ("enabled = true", Idle, true),
            ("enabled = true", Start, true),
            ("enabled = true", Approval, false),
            ("enabled = true\nserver_enabled = false", Idle, false),
            ("enabled = true\nserver_idle_enabled = false", Idle, false),
            ("enabled = true\nserver_idle_enabled = false", Error, true),
            ("enabled = true\nbrowser_enabled = false", Idle, true),
        ] {
            let config: SoundConfig = toml::from_str(toml).expect(toml);
            assert_eq!(config.host_plays(event), plays, "{toml} {event:?}");
        }
    }

    #[test]
    fn event_toggles_scope_and_volumes_default_to_today_and_parse() {
        let config: SoundConfig = toml::from_str("").unwrap();
        assert!(config.server_start_enabled && config.server_error_enabled);
        assert!(config.browser_approval_enabled && config.browser_waiting_enabled);
        assert_eq!(config.browser_scope, BrowserScope::All);
        assert!((config.browser_volume - 1.0).abs() < 1e-9);

        let config: SoundConfig = toml::from_str(
            "server_idle_enabled = false\nbrowser_error_enabled = false\nbrowser_scope = \"hidden_tab\"\nserver_volume = 1.4\nbrowser_volume = 0.5",
        )
        .unwrap();
        assert!(!config.server_idle_enabled && config.server_waiting_enabled);
        assert!(!config.browser_error_enabled && config.browser_idle_enabled);
        assert_eq!(config.browser_scope, BrowserScope::HiddenTab);
        assert!((config.server_volume - 1.4).abs() < 1e-9);
        assert!((config.browser_volume - 0.5).abs() < 1e-9);
    }

    #[test]
    fn volume_options_are_fifteen_tenths_from_a_tenth_to_one_and_a_half() {
        let options = volume_options();
        assert_eq!(options.len(), 15);
        for (i, opt) in options.iter().enumerate() {
            assert_eq!(opt, &format!("{:.1}", (i + 1) as f64 * 0.1));
            assert_eq!(volume_to_index(volume_from_option(opt)), i);
        }
    }

    #[test]
    fn volume_to_index_clamps_to_the_option_range() {
        for (volume, index) in [
            (0.1, 0),
            (1.0, 9),
            (1.5, 14),
            (0.0, 0),
            (-1.0, 0),
            (2.0, 14),
            (99.0, 14),
        ] {
            assert_eq!(volume_to_index(volume), index, "{volume}");
        }
    }

    #[test]
    fn volume_from_option_clamps_and_falls_back_to_the_default() {
        for (option, expected) in [
            ("0.1", 0.1),
            ("1.0", 1.0),
            ("1.5", 1.5),
            ("0.0", 0.1),
            ("-1.0", 0.1),
            ("2.0", 1.5),
            ("99.9", 1.5),
            ("", 1.0),
            ("bad", 1.0),
        ] {
            assert!(
                (volume_from_option(option) - expected).abs() < 1e-9,
                "{option}"
            );
        }
    }
}
