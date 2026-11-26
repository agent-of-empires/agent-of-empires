//! Built-in sound effects for Session status changes and approvals.

mod config;
mod playback;

pub use config::{volume_from_option, volume_options, volume_to_index, SoundConfig};
pub use playback::{play, play_blocking};

use crate::session::Status;

/// One built-in sound. The Daemon plays the status ones on its host and serves
/// every one to the Web dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum SoundEvent {
    Start,
    Running,
    Waiting,
    Idle,
    Error,
    /// A Structured view Session asks for permission or a question; browser only.
    Approval,
}

impl SoundEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Error => "error",
            Self::Approval => "approval",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        <Self as clap::ValueEnum>::from_str(name, false).ok()
    }

    pub fn for_status(status: Status) -> Option<Self> {
        match status {
            Status::Starting => Some(Self::Start),
            Status::Running => Some(Self::Running),
            Status::Waiting => Some(Self::Waiting),
            Status::Idle => Some(Self::Idle),
            Status::Error => Some(Self::Error),
            Status::Unknown | Status::Stopped | Status::Deleting | Status::Creating => None,
        }
    }

    /// The WAV bytes, from the CC0 pack in `bundled_sounds/`.
    pub fn wav(self) -> &'static [u8] {
        match self {
            Self::Start => include_bytes!("../../bundled_sounds/start.wav"),
            Self::Running => include_bytes!("../../bundled_sounds/running.wav"),
            Self::Waiting => include_bytes!("../../bundled_sounds/waiting.wav"),
            Self::Idle => include_bytes!("../../bundled_sounds/idle.wav"),
            Self::Error => include_bytes!("../../bundled_sounds/error.wav"),
            Self::Approval => include_bytes!("../../bundled_sounds/gem.wav"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_round_trips_its_name_and_embeds_a_wav() {
        use clap::ValueEnum;
        for event in SoundEvent::value_variants() {
            assert_eq!(SoundEvent::parse(event.as_str()), Some(*event));
            assert_eq!(&event.wav()[..4], b"RIFF", "{event:?}");
        }
        assert_eq!(SoundEvent::parse("idle.wav"), None);
    }
}
