//! Migration v002: Seed shared sandbox directories from existing named Docker volumes.
//!
//! Previously, agent auth was stored in named Docker volumes (e.g. `hmp-claude-auth`).
//! Now sandbox dirs are the only mechanism. This migration copies data from any
//! existing named volumes into the corresponding sandbox directories so users don't
//! lose their auth state.
//!
//! Uses a merge strategy: files from the volume are only copied if they don't already
//! exist in the sandbox dir. This means the migration is safe to run even if
//! sync_agent_config has already populated the sandbox dir with non-credential files.
//!
//! Old volumes are intentionally preserved after migration. Users can remove them
//! manually with `docker volume rm hmp-claude-auth hmp-opencode-auth ...`.

use anyhow::Result;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use tracing::info;

/// Mapping from legacy named volume to the host-relative sandbox directory.
struct VolumeMigration {
    volume_name: &'static str,
    /// Path relative to home where the sandbox dir lives (e.g. ".claude/sandbox").
    sandbox_rel: &'static str,
}

const VOLUME_MIGRATIONS: &[VolumeMigration] = &[
    VolumeMigration {
        volume_name: "hmp-claude-auth",
        sandbox_rel: ".claude/sandbox",
    },
    VolumeMigration {
        volume_name: "hmp-opencode-auth",
        sandbox_rel: ".local/share/opencode/sandbox",
    },
    VolumeMigration {
        volume_name: "hmp-codex-auth",
        sandbox_rel: ".codex/sandbox",
    },
    VolumeMigration {
        volume_name: "hmp-gemini-auth",
        sandbox_rel: ".gemini/sandbox",
    },
    VolumeMigration {
        volume_name: "hmp-vibe-auth",
        sandbox_rel: ".vibe/sandbox",
    },
];

/// Check whether Docker is available and the daemon is running.
fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check whether a named Docker volume exists.
fn volume_exists(name: &str) -> bool {
    Command::new("docker")
        .args(["volume", "inspect", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Find a container image available locally to use for volume extraction.
/// Tries the HMP sandbox image first, then small well-known images, then
/// falls back to whatever is locally available.
fn find_local_image() -> Option<String> {
    let candidates = [
        "ghcr.io/njbrake/hmp-sandbox:latest",
        "alpine",
        "busybox",
        "ubuntu",
    ];

    // Check well-known small images first.
    for candidate in candidates {
        let ok = Command::new("docker")
            .args(["image", "inspect", candidate])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(candidate.to_string());
        }
    }

    // Fall back to any locally available image.
    let output = Command::new("docker")
        .args(["images", "-q", "--format", "{{.Repository}}:{{.Tag}}"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .next()
        .filter(|s| !s.is_empty() && !s.contains("<none>"))
        .map(|s| s.to_string())
}

/// Extract contents of a named volume into a temp directory using a locally
/// available image. Returns the temp dir path on success. Caller is responsible
/// for cleaning up the temp dir.
fn extract_volume_to_temp(volume_name: &str, image: &str) -> Result<std::path::PathBuf> {
    let tmp = std::env::temp_dir().join(format!("hmp-migration-{}", volume_name));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    std::fs::create_dir_all(&tmp)?;

    let tmp_str = tmp.to_string_lossy();
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/vol", volume_name),
            "-v",
            &format!("{}:/host", tmp_str),
            image,
            "sh",
            "-c",
            "cp -a /vol/. /host/",
        ])
        .output()?;

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to extract volume {}: {}",
            volume_name,
            stderr.trim()
        );
    }

    Ok(tmp)
}

/// Merge files from src into dest, skipping files that already exist in dest.
/// This preserves any files that sync_agent_config has already placed.
fn merge_into(src: &Path, dest: &Path) -> Result<u32> {
    std::fs::create_dir_all(dest)?;
    let mut count = 0;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        let metadata = match std::fs::metadata(entry.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.is_dir() {
            count += merge_into(&entry.path(), &target)?;
        } else if !target.exists() {
            std::fs::copy(entry.path(), &target)?;
            count += 1;
        }
    }

    Ok(count)
}

/// Wait for a child process with a timeout. Kills the process if it exceeds the deadline.
fn wait_with_timeout(
    mut child: Child,
    timeout: std::time::Duration,
) -> std::io::Result<ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status),
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "process timed out",
                ));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

pub fn run() -> Result<()> {
    if !docker_available() {
        info!("Docker not available, skipping volume migration");
        return Ok(());
    }

    let Some(home) = dirs::home_dir() else {
        info!("Cannot determine home directory, skipping volume migration");
        return Ok(());
    };

    // Find a locally available image to use for extraction. Avoids pulling
    // images (which can fail if Docker's credential helper has keychain issues).
    let image = match find_local_image() {
        Some(img) => {
            info!("Using local image '{}' for volume extraction", img);
            img
        }
        None => {
            // Try pulling alpine as a last resort, with a timeout so we don't hang
            // on slow/absent networks.
            info!("No local images found, attempting to pull alpine (15s timeout)");
            let pull = Command::new("docker")
                .args(["pull", "alpine"])
                .spawn()
                .and_then(|child| wait_with_timeout(child, std::time::Duration::from_secs(15)));
            match pull {
                Ok(status) if status.success() => "alpine".to_string(),
                _ => {
                    tracing::warn!(target: "migrations",
                        "No container images available for volume migration. \
                         Run 'docker pull alpine' manually and restart to retry."
                    );
                    return Ok(());
                }
            }
        }
    };

    for migration in VOLUME_MIGRATIONS {
        if !volume_exists(migration.volume_name) {
            continue;
        }

        let sandbox_dir = home.join(migration.sandbox_rel);

        info!(
            "Merging data from named volume {} into {}",
            migration.volume_name,
            sandbox_dir.display()
        );

        // Extract to a temp dir first, then merge only missing files.
        let tmp = match extract_volume_to_temp(migration.volume_name, &image) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(target: "migrations", "Failed to extract volume {}: {}", migration.volume_name, e);
                continue;
            }
        };

        match merge_into(&tmp, &sandbox_dir) {
            Ok(n) => {
                info!(
                    "Merged {} new file(s) from volume {} into {}",
                    n,
                    migration.volume_name,
                    sandbox_dir.display()
                );
            }
            Err(e) => {
                tracing::warn!(target: "migrations",
                    "Failed to merge files from volume {}: {}",
                    migration.volume_name,
                    e
                );
            }
        }

        // Clean up temp dir.
        if let Err(e) = std::fs::remove_dir_all(&tmp) {
            tracing::debug!(target: "migrations", "Failed to clean up temp dir {}: {}", tmp.display(), e);
        }
    }

    Ok(())
}
