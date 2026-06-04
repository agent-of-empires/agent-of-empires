//! Bundled-sound registry and one-shot installer that fetches them from GitHub.

use super::discovery::get_sounds_dir;

const GITHUB_SOUNDS_BASE_URL: &str =
    "https://raw.githubusercontent.com/agent-of-empires/agent-of-empires/main/bundled_sounds";

/// List of bundled sound files available for download
const BUNDLED_SOUND_FILES: &[&str] = &[
    "start.wav",
    "running.wav",
    "waiting.wav",
    "idle.wav",
    "error.wav",
    "spell.wav",
    "coins.wav",
    "metal.wav",
    "chain.wav",
    "gem.wav",
];

/// Download and install bundled sounds from GitHub
pub async fn install_bundled_sounds() -> anyhow::Result<()> {
    let Some(sounds_dir) = get_sounds_dir() else {
        return Err(anyhow::anyhow!("Could not determine sounds directory"));
    };

    if !sounds_dir.exists() {
        std::fs::create_dir_all(&sounds_dir)?;
    }

    let client = reqwest::Client::builder()
        .user_agent("agent-of-empires")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut failed = Vec::new();

    for filename in BUNDLED_SOUND_FILES {
        let path = sounds_dir.join(filename);
        if path.exists() {
            tracing::debug!(target: "sound.bundled", "Sound already exists, skipping: {}", filename);
            continue;
        }

        let url = format!("{}/{}", GITHUB_SOUNDS_BASE_URL, filename);
        tracing::info!(target: "sound.bundled", "Downloading sound: {}", filename);

        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => match response.bytes().await {
                Ok(bytes) => {
                    if let Err(e) = std::fs::write(&path, &bytes) {
                        tracing::warn!(target: "sound.bundled", "Failed to write sound file {}: {}", filename, e);
                        failed.push(filename.to_string());
                    } else {
                        tracing::info!(target: "sound.bundled", "Installed sound: {}", filename);
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "sound.bundled", "Failed to download sound {}: {}", filename, e);
                    failed.push(filename.to_string());
                }
            },
            Ok(response) => {
                tracing::warn!(target: "sound.bundled",
                    "Failed to download {} (HTTP {})",
                    filename,
                    response.status()
                );
                failed.push(filename.to_string());
            }
            Err(e) => {
                tracing::warn!(target: "sound.bundled", "Failed to download sound {}: {}", filename, e);
                failed.push(filename.to_string());
            }
        }
    }

    if !failed.is_empty() {
        return Err(anyhow::anyhow!(
            "Failed to download {} sound(s): {}",
            failed.len(),
            failed.join(", ")
        ));
    }

    Ok(())
}
