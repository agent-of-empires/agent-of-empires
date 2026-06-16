//! Playback backend: picks afplay / paplay / aplay and runs them.

use super::config::is_default_volume;
use super::discovery::find_sound_file;

/// Get the platform-specific audio command for playing a sound file
fn get_audio_command(path: &str, volume: f64) -> Result<(String, Vec<String>), std::io::Error> {
    if cfg!(target_os = "macos") {
        Ok((
            "afplay".to_string(),
            vec!["-v".to_string(), format!("{:.4}", volume), path.to_string()],
        ))
    } else {
        // Linux
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("wav");
        let pa_volume = ((volume * 65536.0).round() as u32).to_string();

        if ext.eq_ignore_ascii_case("ogg") {
            // Check if paplay is available
            if which::which("paplay").is_ok() {
                Ok((
                    "paplay".to_string(),
                    vec![format!("--volume={}", pa_volume), path.to_string()],
                ))
            } else if which::which("aplay").is_ok() {
                tracing::warn!(target: "sound.playback", "paplay not found, using aplay (may not support .ogg files)");
                warn_aplay_volume_once(volume);
                Ok(("aplay".to_string(), vec![path.to_string()]))
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No audio player found. Install alsa-utils (aplay) or pulseaudio-utils (paplay)",
                ))
            }
        } else {
            // WAV files
            if which::which("aplay").is_ok() {
                warn_aplay_volume_once(volume);
                Ok(("aplay".to_string(), vec![path.to_string()]))
            } else if which::which("paplay").is_ok() {
                Ok((
                    "paplay".to_string(),
                    vec![format!("--volume={}", pa_volume), path.to_string()],
                ))
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No audio player found. Install alsa-utils (aplay) or pulseaudio-utils (paplay)",
                ))
            }
        }
    }
}

/// aplay has no volume flag, so the configured volume is ignored when it's
/// the backend. Warn the user once per process so the "slider does nothing"
/// case isn't silent.
fn warn_aplay_volume_once(volume: f64) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !is_default_volume(&volume) && !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(target: "sound.playback",
            "aplay does not support volume control; ignoring configured volume {:.1}. Install pulseaudio-utils (paplay) to enable volume.",
            volume
        );
    }
}

/// Play a sound file by name (blocking version for testing)
pub fn play_sound_blocking(name: &str, volume: f64) -> Result<(), std::io::Error> {
    let Some(path) = find_sound_file(name) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Sound file not found: {}", name),
        ));
    };

    let path_str = path.to_string_lossy().to_string();
    let (cmd, args) = get_audio_command(&path_str, volume)?;

    let output = std::process::Command::new(cmd)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()?;

    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "Sound playback failed with exit code: {:?}",
            output.status.code()
        )))
    }
}

/// Play a sound file by name (fire-and-forget, non-blocking)
pub fn play_sound(name: &str, volume: f64) {
    let Some(path) = find_sound_file(name) else {
        tracing::debug!(target: "sound.playback", "Sound file not found: {}", name);
        return;
    };

    let path_str = path.to_string_lossy().to_string();

    std::thread::spawn(move || {
        let (cmd, args) = match get_audio_command(&path_str, volume) {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(target: "sound.playback", "Audio player not available: {}", e);
                return;
            }
        };

        let result = std::process::Command::new(cmd)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();

        if let Err(e) = result {
            tracing::debug!(target: "sound.playback", "Failed to play sound: {}", e);
        }
    });
}
