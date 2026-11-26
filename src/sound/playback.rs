//! Playback backend: picks afplay / paplay / aplay and runs them.

use std::path::PathBuf;

use super::config::is_default_volume;
use super::SoundEvent;

fn get_audio_command(path: &str, volume: f64) -> Result<(String, Vec<String>), std::io::Error> {
    if cfg!(target_os = "macos") {
        Ok((
            "afplay".to_string(),
            vec!["-v".to_string(), format!("{:.4}", volume), path.to_string()],
        ))
    } else {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("wav");
        let pa_volume = ((volume * 65536.0).round() as u32).to_string();

        if ext.eq_ignore_ascii_case("ogg") {
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

/// aplay has no volume flag; warn once so the ignored setting is not silent.
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

/// The players read a path, so each built-in sound is written once under the
/// app dir. The length in the name replaces a file from an older build.
fn sound_file(event: SoundEvent) -> std::io::Result<PathBuf> {
    let bytes = event.wav();
    let dir = crate::session::get_app_dir()
        .map_err(std::io::Error::other)?
        .join("sound-cache");
    let path = dir.join(format!("{}-{}.wav", event.as_str(), bytes.len()));
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir)?;
    // Written aside under a per-call name and renamed, so a concurrent play never reads a partial file.
    static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = dir.join(format!(".{}.{}.{call}", event.as_str(), std::process::id()));
    std::fs::write(&staging, bytes)?;
    std::fs::rename(&staging, &path)?;
    Ok(path)
}

pub fn play_blocking(event: SoundEvent, volume: f64) -> Result<(), std::io::Error> {
    let path_str = sound_file(event)?.to_string_lossy().to_string();
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

/// Plays on a background thread. Only the first failure warns, since a host
/// with no audio would otherwise log one on every status change.
pub fn play(event: SoundEvent, volume: f64) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    std::thread::spawn(move || {
        let Err(e) = play_blocking(event, volume) else {
            return;
        };
        if WARNED.swap(true, Ordering::Relaxed) {
            tracing::debug!(target: "sound.playback", sound = event.as_str(), "Sound playback failed: {e}");
        } else {
            tracing::warn!(target: "sound.playback", sound = event.as_str(), "Sound playback failed: {e}");
        }
    });
}
