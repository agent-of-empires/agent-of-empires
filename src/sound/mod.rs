//! Sound effects for agent state transitions
//!
//! Plays AoE II-style sounds when agent sessions change state.
//! Users place .wav/.ogg files in the sounds directory:
//!   - Linux: ~/.config/agent-of-empires/sounds/
//!   - macOS: ~/.agent-of-empires/sounds/
//!
//! Expected filenames (any .wav/.ogg file works):
//!   wololo.wav, rogan.wav, allhail.wav, monk.wav,
//!   alarm.wav, start.wav
//!
//! Layout:
//!   - `config`    — `SoundConfig`, `SoundMode`, volume helpers
//!   - `discovery` — sounds directory + available-files probing
//!   - `bundled`   — GitHub-hosted default sound pack installer
//!   - `playback`  — afplay / paplay / aplay dispatch
//!   - this file   — transition-to-sound glue

mod bundled;
mod config;
mod discovery;
mod playback;

pub use bundled::install_bundled_sounds;
pub use config::{volume_from_option, volume_options, volume_to_index, SoundConfig, SoundMode};
pub use discovery::{get_sounds_dir, list_available_sounds, validate_sound_exists};
pub use playback::{play_sound, play_sound_blocking};

use rand::seq::IndexedRandom;

use crate::session::Status;

/// Resolve which sound name to play for the given config
fn resolve_sound_name(override_name: Option<&str>, config: &SoundConfig) -> Option<String> {
    // Per-transition override takes priority
    if let Some(name) = override_name {
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    match &config.mode {
        SoundMode::Specific(name) => Some(name.clone()),
        SoundMode::Random => {
            let sounds = list_available_sounds();
            if sounds.is_empty() {
                return None;
            }
            let mut rng = rand::rng();
            sounds.choose(&mut rng).cloned()
        }
    }
}

/// Play a sound for a state transition (if enabled and sounds are available)
pub fn play_for_transition(old: Status, new: Status, config: &SoundConfig) {
    if !config.enabled || old == new {
        return;
    }

    let override_name = match new {
        Status::Starting => config.on_start.as_deref(),
        Status::Running => config.on_running.as_deref(),
        Status::Waiting => config.on_waiting.as_deref(),
        Status::Idle => config.on_idle.as_deref(),
        Status::Error => config.on_error.as_deref(),
        Status::Unknown => return,
        Status::Stopped => return,
        Status::Deleting => return,
        Status::Creating => return,
    };

    if let Some(name) = resolve_sound_name(override_name, config) {
        play_sound(&name, config.volume);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_sound_name_override() {
        let config = SoundConfig {
            mode: SoundMode::Specific("default_sound".to_string()),
            ..Default::default()
        };
        let result = resolve_sound_name(Some("alarm"), &config);
        assert_eq!(result, Some("alarm".to_string()));
    }

    #[test]
    fn test_resolve_sound_name_specific_mode() {
        let config = SoundConfig {
            mode: SoundMode::Specific("wololo".to_string()),
            ..Default::default()
        };
        let result = resolve_sound_name(None, &config);
        assert_eq!(result, Some("wololo".to_string()));
    }

    #[test]
    fn test_resolve_sound_name_empty_override_uses_mode() {
        let config = SoundConfig {
            mode: SoundMode::Specific("wololo".to_string()),
            ..Default::default()
        };
        let result = resolve_sound_name(Some(""), &config);
        assert_eq!(result, Some("wololo".to_string()));
    }

    #[test]
    fn test_play_for_transition_disabled() {
        let config = SoundConfig::default();
        // Should not panic even when disabled
        play_for_transition(Status::Idle, Status::Running, &config);
    }

    #[test]
    fn test_play_for_transition_same_status() {
        let config = SoundConfig {
            enabled: true,
            mode: SoundMode::Specific("wololo".to_string()),
            ..Default::default()
        };
        // Same status - should be a no-op
        play_for_transition(Status::Running, Status::Running, &config);
    }

    #[test]
    fn test_play_for_transition_deleting_skipped() {
        let config = SoundConfig {
            enabled: true,
            mode: SoundMode::Specific("wololo".to_string()),
            ..Default::default()
        };
        // Deleting transitions should be skipped
        play_for_transition(Status::Running, Status::Deleting, &config);
    }
}
