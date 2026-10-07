//! Sounds directory lookup, installed-sound enumeration, and validation.

use std::path::PathBuf;

use crate::session::get_app_dir;

pub fn get_sounds_dir() -> Option<PathBuf> {
    get_app_dir().ok().map(|d| d.join("sounds"))
}

pub fn list_available_sounds() -> Vec<String> {
    let Some(dir) = get_sounds_dir() else {
        return Vec::new();
    };
    if !dir.exists() {
        return Vec::new();
    }

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut sounds = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("wav") || ext.eq_ignore_ascii_case("ogg") {
                if let Some(filename) = path.file_name().and_then(|s| s.to_str()) {
                    sounds.push(filename.to_string());
                }
            }
        }
    }
    sounds.sort();
    sounds
}

pub(super) fn find_sound_file(filename: &str) -> Option<PathBuf> {
    let dir = get_sounds_dir()?;
    let path = dir.join(filename);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

pub fn validate_sound_exists(filename: &str) -> Result<(), String> {
    if filename.is_empty() {
        return Ok(());
    }

    let available = list_available_sounds();
    if available.is_empty() {
        return Err(
            "No sounds installed. Run 'aoe sounds install' or add your own .wav/.ogg files."
                .to_string(),
        );
    }

    if !available.contains(&filename.to_string()) {
        return Err(format!(
            "Sound '{}' not found. Available sounds: {}",
            filename,
            available.join(", ")
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_sound_exists_empty() {
        assert!(validate_sound_exists("").is_ok());
    }

    #[test]
    fn test_validate_sound_exists_nonexistent() {
        let result = validate_sound_exists("nonexistent_sound_xyz");
        assert!(result.is_err());
        if let Err(msg) = result {
            assert!(
                msg.contains("not found") || msg.contains("No sounds installed"),
                "Error message: {}",
                msg
            );
        }
    }
}
