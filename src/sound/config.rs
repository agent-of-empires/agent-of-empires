//! Sound configuration, profile-level overrides, and volume helpers.

use aoe_settings_derive::SettingsSection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "sound", category = "Sound")]
pub struct SoundConfig {
    /// Play sounds on agent state transitions.
    #[serde(default)]
    #[setting(label = "Enabled", widget = "toggle")]
    pub enabled: bool,

    /// Specify file name with extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(label = "On Start", widget = "optional_text")]
    pub on_start: Option<String>,

    /// Specify file name with extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(label = "On Running", widget = "optional_text")]
    pub on_running: Option<String>,

    /// Specify file name with extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(label = "On Waiting", widget = "optional_text")]
    pub on_waiting: Option<String>,

    /// Specify file name with extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(label = "On Idle", widget = "optional_text")]
    pub on_idle: Option<String>,

    /// Specify file name with extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(label = "On Error", widget = "optional_text")]
    pub on_error: Option<String>,

    /// Acp only. Played in the browser when a session needs permission.
    /// Specify file name with extension. Surfaced by the acp's approval
    /// hook (host-side playback intentionally has no approval transition; the
    /// host audio device is the wrong side of the wire when the user is
    /// running the dashboard on a separate machine). See #1038.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(label = "On Approval", widget = "optional_text")]
    pub on_approval: Option<String>,

    /// Playback volume (0.1 = min, 1.0 = normal, 1.5 = max), step 0.1. Ignored
    /// when aplay is the Linux backend.
    #[serde(default = "default_volume", skip_serializing_if = "is_default_volume")]
    #[setting(label = "Volume", widget = "custom:sound-volume")]
    pub volume: f64,
}

impl Default for SoundConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            on_start: None,
            on_running: None,
            on_waiting: None,
            on_idle: None,
            on_error: None,
            on_approval: None,
            volume: default_volume(),
        }
    }
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
    fn test_sound_config_default() {
        let config = SoundConfig::default();
        assert!(!config.enabled);
        assert!(config.on_start.is_none());
        assert!(config.on_running.is_none());
        assert!(config.on_waiting.is_none());
        assert!(config.on_idle.is_none());
        assert!(config.on_error.is_none());
        assert!(config.on_approval.is_none());
        // A 0.0 default would mute playback on a fresh install.
        assert!((config.volume - 1.0).abs() < 1e-9);
    }

    #[test]
    fn deserialize_reads_known_keys_and_tolerates_the_removed_mode_field() {
        let cases = [
            ("", None, None),
            ("enabled = true\non_error = \"alarm\"", Some("alarm"), None),
            (
                "enabled = true\non_approval = \"alarm\"",
                None,
                Some("alarm"),
            ),
            // The removed `mode` field must still deserialize.
            (
                "enabled = true\nmode = { specific = \"wololo\" }",
                None,
                None,
            ),
        ];
        for (toml, on_error, on_approval) in cases {
            let config: SoundConfig = toml::from_str(toml).expect(toml);
            assert_eq!(config.enabled, !toml.is_empty(), "{toml}");
            assert_eq!(config.on_error.as_deref(), on_error, "{toml}");
            assert_eq!(config.on_approval.as_deref(), on_approval, "{toml}");
        }
    }

    #[test]
    fn volume_options_are_fifteen_tenths_from_a_tenth_to_one_and_a_half() {
        let options = volume_options();
        assert_eq!(options.len(), 15);
        for (i, opt) in options.iter().enumerate() {
            assert_eq!(opt, &format!("{:.1}", (i + 1) as f64 * 0.1));
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

    #[test]
    fn test_volume_options_roundtrip() {
        for (i, opt) in volume_options().iter().enumerate() {
            let v = volume_from_option(opt);
            assert_eq!(volume_to_index(v), i);
        }
    }
}
