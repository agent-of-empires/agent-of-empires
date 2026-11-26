//! `agent-of-empires sounds` subcommands implementation

use anyhow::Result;
use clap::Subcommand;

use crate::sound::{self, SoundEvent};

#[derive(Subcommand)]
pub enum SoundsCommands {
    /// Play a built-in sound on this machine to check that audio works
    Test {
        /// The sound to play
        #[arg(value_enum, default_value_t = SoundEvent::Idle)]
        event: SoundEvent,
    },
}

#[tracing::instrument(target = "cli.session", skip_all)]
pub async fn run(command: SoundsCommands) -> Result<()> {
    match command {
        SoundsCommands::Test { event } => test_sound(event),
    }
}

fn test_sound(event: SoundEvent) -> Result<()> {
    let volume = crate::session::Config::load()
        .map(|c| c.sound.server_volume)
        .unwrap_or(1.0);

    print!(
        "🔊 Playing '{}' at volume {:.1}... ",
        event.as_str(),
        volume
    );
    std::io::Write::flush(&mut std::io::stdout())?;

    match sound::play_blocking(event, volume) {
        Ok(()) => {
            println!("✓");
            Ok(())
        }
        Err(e) => {
            println!("✗");
            eprintln!("\n❌ Failed to play sound: {}", e);
            eprintln!("\n💡 Troubleshooting:");
            if cfg!(target_os = "linux") {
                eprintln!("  • Ensure audio tools are installed:");
                eprintln!("    - Debian/Ubuntu: sudo apt install alsa-utils pulseaudio-utils");
                eprintln!("    - Arch: sudo pacman -S alsa-utils pulseaudio");
            }
            eprintln!("  • Check that your audio device is working");
            eprintln!("  • Note: Audio doesn't work over SSH sessions");
            Err(e.into())
        }
    }
}
