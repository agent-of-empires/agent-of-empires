//! Sound effects for agent state transitions, played from user .wav/.ogg files.

mod bundled;
mod config;
mod discovery;
mod playback;

pub use bundled::install_bundled_sounds;
pub use config::{volume_from_option, volume_options, volume_to_index, SoundConfig};
pub use discovery::{get_sounds_dir, list_available_sounds, validate_sound_exists};
pub use playback::{play_sound, play_sound_blocking};

use rand::seq::IndexedRandom;

use crate::session::Status;

fn resolve_sound_name(override_name: Option<&str>) -> Option<String> {
    if let Some(name) = override_name {
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    let sounds = list_available_sounds();
    if sounds.is_empty() {
        return None;
    }
    let mut rng = rand::rng();
    sounds.choose(&mut rng).cloned()
}

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

    if let Some(name) = resolve_sound_name(override_name) {
        play_sound(&name, config.volume);
    }
}
