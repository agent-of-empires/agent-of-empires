//! Sound configuration: [`SoundConfig`], profile-level overrides, and volume helpers.

use aoe_settings_derive::SettingsSection;
use serde::{Deserialize, Serialize};

/// How to select which sound file to play
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SoundMode {
    /// Pick a random sound from available files
    #[default]
    Random,
    /// Always play a specific sound file (by name, without extension)
    Specific(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "sound", category = "Sound")]
pub struct SoundConfig {
    /// Play sounds on agent state transitions.
    #[serde(default)]
    #[setting(label = "Enabled", widget = "toggle")]
    pub enabled: bool,

    /// How to select sounds (Random or a specific file name).
    #[serde(default)]
    #[setting(label = "Mode", widget = "custom:sound-mode")]
    pub mode: SoundMode,

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

    /// Cockpit only. Played in the browser when a session needs permission.
    /// Specify file name with extension. Surfaced by the cockpit's approval
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
            mode: SoundMode::default(),
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

/// Profile override for sound config (all fields optional, None = inherit)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SoundConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SoundMode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_start: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_running: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_waiting: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_idle: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_error: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_approval: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
}

/// Apply sound config overrides from a profile
pub fn apply_sound_overrides(target: &mut SoundConfig, source: &SoundConfigOverride) {
    if let Some(enabled) = source.enabled {
        target.enabled = enabled;
    }
    if let Some(ref mode) = source.mode {
        target.mode = mode.clone();
    }
    if source.on_start.is_some() {
        target.on_start = source.on_start.clone();
    }
    if source.on_running.is_some() {
        target.on_running = source.on_running.clone();
    }
    if source.on_waiting.is_some() {
        target.on_waiting = source.on_waiting.clone();
    }
    if source.on_idle.is_some() {
        target.on_idle = source.on_idle.clone();
    }
    if source.on_error.is_some() {
        target.on_error = source.on_error.clone();
    }
    if source.on_approval.is_some() {
        target.on_approval = source.on_approval.clone();
    }
    if let Some(volume) = source.volume {
        target.volume = volume;
    }
}

/// Returns the 15 volume level strings "0.1", "0.2", ..., "1.5"
pub fn volume_options() -> Vec<String> {
    (1..=15).map(|i| format!("{:.1}", i as f64 * 0.1)).collect()
}

/// Convert an f64 volume to the nearest Select index (1..=15)
pub fn volume_to_index(v: f64) -> usize {
    ((v.clamp(0.1, 1.5) / 0.1).round() as usize).min(15) - 1
}

/// Parse a volume option string back to f64
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
        assert_eq!(config.mode, SoundMode::Random);
        assert!(config.on_start.is_none());
        assert!(config.on_running.is_none());
        assert!(config.on_waiting.is_none());
        assert!(config.on_idle.is_none());
        assert!(config.on_error.is_none());
        assert!(config.on_approval.is_none());
        // Fresh installs load `Config::default()` when no config.toml exists;
        // a 0.0 default here would mute all playback on first run.
        assert!((config.volume - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_sound_config_deserialize_empty() {
        let config: SoundConfig = toml::from_str("").unwrap();
        assert!(!config.enabled);
    }

    #[test]
    fn test_sound_config_deserialize() {
        let toml = r#"
            enabled = true
            mode = "random"
            on_error = "alarm"
        "#;
        let config: SoundConfig = toml::from_str(toml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.mode, SoundMode::Random);
        assert_eq!(config.on_error, Some("alarm".to_string()));
    }

    #[test]
    fn test_sound_mode_specific_deserialize() {
        let toml = r#"
            enabled = true
            mode = { specific = "wololo" }
        "#;
        let config: SoundConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.mode, SoundMode::Specific("wololo".to_string()));
    }

    #[test]
    fn test_sound_config_override_default() {
        let ovr = SoundConfigOverride::default();
        assert!(ovr.enabled.is_none());
        assert!(ovr.mode.is_none());
    }

    #[test]
    fn test_apply_sound_overrides() {
        let mut config = SoundConfig::default();
        let ovr = SoundConfigOverride {
            enabled: Some(true),
            on_error: Some("alarm".to_string()),
            ..Default::default()
        };
        apply_sound_overrides(&mut config, &ovr);
        assert!(config.enabled);
        assert_eq!(config.on_error, Some("alarm".to_string()));
        // Non-overridden fields stay default
        assert_eq!(config.mode, SoundMode::Random);
    }

    #[test]
    fn test_apply_sound_overrides_on_approval() {
        let mut config = SoundConfig {
            on_approval: Some("waiting".to_string()),
            ..Default::default()
        };
        let ovr = SoundConfigOverride {
            on_approval: Some("alarm".to_string()),
            ..Default::default()
        };
        apply_sound_overrides(&mut config, &ovr);
        assert_eq!(config.on_approval, Some("alarm".to_string()));
    }

    #[test]
    fn test_sound_config_deserialize_on_approval() {
        let toml = r#"
            enabled = true
            on_approval = "alarm"
        "#;
        let config: SoundConfig = toml::from_str(toml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.on_approval, Some("alarm".to_string()));
    }

    #[test]
    fn test_volume_options_count_and_range() {
        let options = volume_options();
        assert_eq!(options.len(), 15);
        assert_eq!(options[0], "0.1");
        assert_eq!(options[14], "1.5");
    }

    #[test]
    fn test_volume_options_step() {
        let options = volume_options();
        for (i, opt) in options.iter().enumerate() {
            let expected = format!("{:.1}", (i + 1) as f64 * 0.1);
            assert_eq!(opt, &expected);
        }
    }

    #[test]
    fn test_volume_to_index_normal_values() {
        assert_eq!(volume_to_index(0.1), 0);
        assert_eq!(volume_to_index(1.0), 9);
        assert_eq!(volume_to_index(1.5), 14);
    }

    #[test]
    fn test_volume_to_index_clamps_below_min() {
        assert_eq!(volume_to_index(0.0), 0);
        assert_eq!(volume_to_index(-1.0), 0);
    }

    #[test]
    fn test_volume_to_index_clamps_above_max() {
        assert_eq!(volume_to_index(2.0), 14);
        assert_eq!(volume_to_index(99.0), 14);
    }

    #[test]
    fn test_volume_from_option_valid() {
        assert!((volume_from_option("0.1") - 0.1).abs() < 1e-9);
        assert!((volume_from_option("1.0") - 1.0).abs() < 1e-9);
        assert!((volume_from_option("1.5") - 1.5).abs() < 1e-9);
    }

    #[test]
    fn test_volume_from_option_clamps_below_min() {
        assert!((volume_from_option("0.0") - 0.1).abs() < 1e-9);
        assert!((volume_from_option("-1.0") - 0.1).abs() < 1e-9);
    }

    #[test]
    fn test_volume_from_option_clamps_above_max() {
        assert!((volume_from_option("2.0") - 1.5).abs() < 1e-9);
        assert!((volume_from_option("99.9") - 1.5).abs() < 1e-9);
    }

    #[test]
    fn test_volume_from_option_invalid_falls_back_to_default() {
        assert!((volume_from_option("") - 1.0).abs() < 1e-9);
        assert!((volume_from_option("bad") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_volume_options_roundtrip() {
        for (i, opt) in volume_options().iter().enumerate() {
            let v = volume_from_option(opt);
            assert_eq!(volume_to_index(v), i);
        }
    }
}
