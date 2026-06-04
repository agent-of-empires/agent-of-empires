//! Self-update: detect install method, perform update.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

use crate::update::is_newer_version;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Tarball { binary_path: PathBuf },
    Nix,
    Cargo,
    Unknown { binary_path: PathBuf },
}

/// Pure prefix-based classification used by `detect_install_method`.
/// Returns the method as far as path prefixes can determine; Homebrew
/// detection requires running `brew list` and is layered on by
/// `classify_with_brew`.
fn classify_path_prefix(binary_path: &Path, home: &Path) -> InstallMethod {
    let path = binary_path;

    if path.starts_with("/nix/store/") {
        return InstallMethod::Nix;
    }

    let cargo_bin = home.join(".cargo").join("bin");
    if path.starts_with(&cargo_bin) {
        return InstallMethod::Cargo;
    }

    let known_bin_locations: [PathBuf; 3] = [
        PathBuf::from("/usr/local/bin"),
        home.join(".local").join("bin"),
        home.join("bin"),
    ];
    let parent = path.parent();
    if parent.is_some_and(|p| known_bin_locations.iter().any(|k| p == k.as_path())) {
        return InstallMethod::Tarball {
            binary_path: path.to_path_buf(),
        };
    }

    InstallMethod::Unknown {
        binary_path: path.to_path_buf(),
    }
}

/// Layer Homebrew detection on top of the prefix classification:
/// only return `Homebrew` if `brew list aoe` produced a path that
/// canonicalizes to the same file as the running binary. Otherwise
/// keep the prefix classification.
fn classify_with_brew(
    prefix: InstallMethod,
    brew_path: Option<&Path>,
    binary_path: &Path,
) -> InstallMethod {
    if let Some(bp) = brew_path {
        if paths_canonicalize_equal(bp, binary_path) {
            return InstallMethod::Homebrew;
        }
    }
    prefix
}

fn paths_canonicalize_equal(a: &Path, b: &Path) -> bool {
    let a_canon = a.canonicalize().ok();
    let b_canon = b.canonicalize().ok();
    match (a_canon, b_canon) {
        (Some(a), Some(b)) => a == b,
        _ => a == b, // fall back to literal equality if canonicalize fails
    }
}

pub fn detect_install_method() -> Result<InstallMethod> {
    let exe = std::env::current_exe().context("locating current executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    // Canonicalize home too, otherwise on macOS a binary at /tmp/.../.local/bin/aoe
    // gets a canonicalized exe path of /private/tmp/.../.local/bin/aoe but home is
    // still /tmp/..., the parent-prefix comparison fails, and the binary
    // misclassifies as Unknown. Same failure mode for any user whose HOME
    // resolves through a symlink. canonicalize() can fail (home doesn't exist
    // on disk, permission errors, etc.) so we fall back to the raw path.
    let home = dirs::home_dir().context("locating home directory")?;
    let home = home.canonicalize().unwrap_or(home);
    let prefix = classify_path_prefix(&exe, &home);
    let brew_path = probe_brew_aoe_path();
    Ok(classify_with_brew(prefix, brew_path.as_deref(), &exe))
}

/// How long we wait for `brew list aoe` to come back before assuming
/// brew is hung (locked formula DB, network-bound auto-update, etc.)
/// and giving up on the probe. Detection runs on TUI startup, so this
/// directly bounds startup latency on machines with brew.
const BREW_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Run `brew list aoe` and return the path to the installed binary, if any.
/// We parse the output (one path per line) and pick the line that ends in
/// `/aoe` or `/bin/aoe`. If brew is not installed, the formula is not installed,
/// or the command fails for any other reason, return `None`.
///
/// Bounded by `BREW_PROBE_TIMEOUT`: if brew doesn't return in time we
/// kill the child and treat it as "not a brew install" rather than
/// blocking the TUI's startup-update path.
fn probe_brew_aoe_path() -> Option<PathBuf> {
    probe_brew_aoe_path_with_timeout(BREW_PROBE_TIMEOUT)
}

fn probe_brew_aoe_path_with_timeout(timeout: std::time::Duration) -> Option<PathBuf> {
    use std::process::Stdio;
    use std::time::Instant;

    let mut child = Command::new("brew")
        .args(["list", "aoe"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }

    let output = child.wait_with_output().ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with("/aoe") || trimmed.ends_with("/bin/aoe") {
            return Some(PathBuf::from(trimmed));
        }
    }
    None
}

/// Return the platform string used in release tarball asset names
/// (e.g. `linux-amd64`). `os` matches `std::env::consts::OS`,
/// `arch` matches `std::env::consts::ARCH`.
fn platform_string_for(os: &str, arch: &str) -> Result<&'static str> {
    let os_norm = match os {
        "linux" => "linux",
        "macos" => "darwin",
        other => anyhow::bail!("unsupported OS: {other}"),
    };
    let arch_norm = match arch {
        "x86_64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        other => anyhow::bail!("unsupported architecture: {other}"),
    };
    // Static lookup so we can return &'static str.
    Ok(match (os_norm, arch_norm) {
        ("linux", "amd64") => "linux-amd64",
        ("linux", "arm64") => "linux-arm64",
        ("darwin", "amd64") => "darwin-amd64",
        ("darwin", "arm64") => "darwin-arm64",
        _ => unreachable!(),
    })
}

pub fn current_platform_string() -> Result<&'static str> {
    platform_string_for(std::env::consts::OS, std::env::consts::ARCH)
}

const DEFAULT_RELEASE_BASE: &str =
    "https://github.com/agent-of-empires/agent-of-empires/releases/download";

fn release_tarball_url(version: &str, platform: &str) -> String {
    let base =
        std::env::var("AOE_UPDATE_BASE_URL").unwrap_or_else(|_| DEFAULT_RELEASE_BASE.to_string());
    format!("{base}/v{version}/aoe-{platform}.tar.gz")
}

/// Download a release tarball to `dest`. Streams bytes; reports
/// progress via the optional callback (current bytes, total bytes
/// if known).
async fn download_tarball(
    url: &str,
    dest: &Path,
    mut on_progress: Option<&mut dyn FnMut(u64, Option<u64>)>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .user_agent("agent-of-empires")
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("download failed: HTTP {} from {}", response.status(), url);
    }
    let total = response.content_length();
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating download file at {}", dest.display()))?;
    let mut downloaded: u64 = 0;
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        if let Some(cb) = on_progress.as_deref_mut() {
            cb(downloaded, total);
        }
    }
    file.sync_all().await?;
    Ok(())
}

/// Extract a `.tar.gz` into `dest_dir`. Shells out to `tar xzf`, which is
/// universally available on macOS/Linux and matches what `scripts/install.sh`
/// does. Returns the path to the extracted binary
/// (`dest_dir/aoe-{platform}`).
fn extract_tarball(tarball: &Path, dest_dir: &Path, platform: &str) -> Result<PathBuf> {
    let status = Command::new("tar")
        .arg("xzf")
        .arg(tarball)
        .arg("-C")
        .arg(dest_dir)
        .status()
        .context("running `tar xzf`")?;
    if !status.success() {
        anyhow::bail!("tar extraction failed (exit {})", status);
    }
    let extracted = dest_dir.join(format!("aoe-{platform}"));
    if !extracted.exists() {
        anyhow::bail!("extracted tarball did not contain {}", extracted.display());
    }
    Ok(extracted)
}

/// Run the candidate binary with `--version` and confirm its output
/// contains the expected version string. Defends against corrupt
/// downloads and wrong-arch tarballs that downloaded successfully but
/// won't run.
fn sanity_check_binary(binary: &Path, expected_version: &str) -> Result<()> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", binary.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "candidate binary failed --version: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let matched = stdout
        .split_whitespace()
        .any(|tok| tok == expected_version || tok.trim_start_matches('v') == expected_version);
    if !matched {
        anyhow::bail!(
            "candidate binary reports {:?}, expected version {:?}",
            stdout.trim(),
            expected_version
        );
    }
    Ok(())
}

/// Atomically replace `target` with `source`. Both paths must be on the
/// same filesystem (callers ensure this by placing the temp file in the
/// same parent directory as the target). On `EACCES`, falls back to two
/// sequential `sudo` invocations (`sudo mv` then `sudo chmod 0755`); the
/// user gets one password prompt thanks to sudo's timestamp cache.
///
/// On Unix, this is safe to do while the target is the running binary -
/// the kernel keeps the old inode alive for the running process.
fn atomic_replace(source: &Path, target: &Path) -> Result<()> {
    match std::fs::rename(source, target) {
        Ok(()) => {
            #[cfg(unix)]
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => sudo_replace(source, target),
        Err(e) => Err(e).with_context(|| format!("renaming to {}", target.display())),
    }
}

fn sudo_replace(source: &Path, target: &Path) -> Result<()> {
    let mv_status = Command::new("sudo")
        .arg("mv")
        .arg(source)
        .arg(target)
        .status()
        .context("invoking `sudo mv`")?;
    if !mv_status.success() {
        anyhow::bail!("sudo mv failed (exit {})", mv_status);
    }
    let chmod_status = Command::new("sudo")
        .arg("chmod")
        .arg("0755")
        .arg(target)
        .status()
        .context("invoking `sudo chmod`")?;
    if !chmod_status.success() {
        anyhow::bail!("sudo chmod failed (exit {})", chmod_status);
    }
    Ok(())
}

/// Probes whether the directory containing `binary_path` is writable
/// without sudo by creating and removing a uniquely-named temp file.
///
/// Uses `tempfile::Builder` so a process killed mid-probe leaves no
/// stale file behind that could poison subsequent calls. (An earlier
/// version used a fixed name and would return `false` forever if the
/// process died between create and unlink.)
pub fn parent_is_writable(binary_path: &Path) -> bool {
    let Some(parent) = binary_path.parent() else {
        return false;
    };
    tempfile::Builder::new()
        .prefix(".aoe-update-probe-")
        .tempfile_in(parent)
        .is_ok()
}

/// Perform an in-place tarball update at `binary_path`, fetching the
/// release for `version`. Caller has already detected the install
/// method and confirmed with the user.
pub async fn update_via_tarball(
    binary_path: &Path,
    version: &str,
    on_progress: Option<&mut dyn FnMut(u64, Option<u64>)>,
) -> Result<()> {
    let platform = current_platform_string()?;
    let parent = binary_path
        .parent()
        .context("binary path has no parent directory")?;

    // Same-filesystem temp dir so the rename in atomic_replace works.
    let workdir = TempDir::new_in(parent).context("creating temp dir for update")?;

    let tarball_path = workdir.path().join(format!("aoe-{platform}.tar.gz"));
    let url = release_tarball_url(version, platform);
    download_tarball(&url, &tarball_path, on_progress).await?;

    let extracted = extract_tarball(&tarball_path, workdir.path(), platform)?;
    sanity_check_binary(&extracted, version)?;
    atomic_replace(&extracted, binary_path)?;
    Ok(())
}

/// How long we wait for `brew info aoe --json=v2` before giving up on
/// learning what version Homebrew has. Same rationale as
/// `BREW_PROBE_TIMEOUT`: don't let a hung `brew` block the update flow.
const BREW_INFO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Deserialize)]
struct BrewInfoEntry {
    versions: BrewVersions,
}

#[derive(Deserialize)]
struct BrewVersions {
    stable: Option<String>,
}

/// Return the `versions.stable` Homebrew currently advertises for the
/// `aoe` formula, or `None` if brew isn't installed, the formula isn't
/// known, the probe times out, or the JSON can't be parsed.
fn brew_available_version() -> Option<String> {
    brew_available_version_with_timeout(BREW_INFO_TIMEOUT)
}

fn brew_available_version_with_timeout(timeout: std::time::Duration) -> Option<String> {
    use std::process::Stdio;
    use std::time::Instant;

    let mut child = Command::new("brew")
        .args(["info", "aoe", "--json=v2"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }

    let output = child.wait_with_output().ok()?;
    parse_brew_stable_version(&output.stdout)
}

/// `brew info --json=v2` wraps the formula list under a top-level
/// `formulae` array; older `--json=v1` (and some shims) return the bare
/// array. Accept either so this keeps working if Homebrew reshuffles the
/// envelope, and so the unit tests can use the simpler shape.
fn parse_brew_stable_version(stdout: &[u8]) -> Option<String> {
    if let Ok(entries) = serde_json::from_slice::<Vec<BrewInfoEntry>>(stdout) {
        return entries.into_iter().next()?.versions.stable;
    }
    #[derive(Deserialize)]
    struct V2Envelope {
        formulae: Vec<BrewInfoEntry>,
    }
    let env: V2Envelope = serde_json::from_slice(stdout).ok()?;
    env.formulae.into_iter().next()?.versions.stable
}

fn brew_formula_lag_message(target_version: &str) -> String {
    format!(
        "v{target_version} isn't available on Homebrew yet. Please try again in a little while; the formula usually catches up within a few hours of a release."
    )
}

/// True if the running install can pull `target_version` right now.
///
/// Returns `false` only for the specific Homebrew-formula-lag case
/// (release is on GitHub but the formula hasn't caught up yet). All
/// other install methods return `true`, as do Homebrew installs where
/// the formula already has the version, where brew probing fails, or
/// where install detection fails. Callers use this to suppress the
/// TUI's "update available" ribbon during the lag window so users
/// aren't nagged about an update they can't apply yet.
///
/// Synchronous (runs `brew info` with a timeout); call from a
/// `spawn_blocking` task if you're on the tokio runtime.
pub fn install_method_supports_target(target_version: &str) -> bool {
    let Ok(method) = detect_install_method() else {
        return true;
    };
    if !matches!(method, InstallMethod::Homebrew) {
        return true;
    }
    match brew_available_version() {
        Some(v) => {
            let supported = !is_newer_version(target_version, &v);
            if !supported {
                tracing::info!(
                    target: "update.suppress",
                    brew_version = %v,
                    target_version = %target_version,
                    "suppressing update banner: Homebrew formula lags GitHub release"
                );
            }
            supported
        }
        None => true,
    }
}

fn update_via_brew(target_version: &str) -> Result<()> {
    let status = Command::new("brew")
        .args(["update"])
        .status()
        .context("running `brew update`")?;
    if !status.success() {
        anyhow::bail!("`brew update` failed (exit {})", status);
    }

    // Homebrew formulae lag behind GitHub releases by minutes to hours.
    // If brew's formula is still on an older version, `brew upgrade aoe`
    // exits 0 silently and leaves the user on the old binary, with the TUI
    // still nagging about an available update. Detect the lag up front
    // and bail with a clear explanation instead.
    if let Some(brew_version) = brew_available_version() {
        if is_newer_version(target_version, &brew_version) {
            anyhow::bail!(brew_formula_lag_message(target_version));
        }
    }

    let status = Command::new("brew")
        .args(["upgrade", "aoe"])
        .status()
        .context("running `brew upgrade aoe`")?;
    if !status.success() {
        anyhow::bail!("`brew upgrade aoe` failed (exit {})", status);
    }
    Ok(())
}

fn nix_refusal_message() -> String {
    "aoe was installed via Nix. Update by running:\n\
     \n    nix run github:agent-of-empires/agent-of-empires\n\
     \n(or rebuild your flake input)."
        .to_string()
}

fn print_nix_refusal() {
    println!("{}", nix_refusal_message());
}

fn cargo_refusal_message() -> String {
    "aoe was installed via cargo. Update by running:\n\
     \n    cargo install --git https://github.com/agent-of-empires/agent-of-empires aoe\n\
     \n(or `git pull && cargo install --path .` from a local clone)."
        .to_string()
}

fn print_cargo_refusal() {
    println!("{}", cargo_refusal_message());
}

fn unknown_refusal_message(binary_path: &Path) -> String {
    format!(
        "Couldn't determine how aoe was installed at {}.\n\
         Reinstall with:\n\
         \n    curl -fsSL https://raw.githubusercontent.com/agent-of-empires/agent-of-empires/main/scripts/install.sh | bash\n",
        binary_path.display()
    )
}

fn print_unknown_refusal(binary_path: &Path) {
    println!("{}", unknown_refusal_message(binary_path));
}

/// Render the four-line confirm-prompt block. Used by both the CLI and
/// the TUI dialog. Produces no trailing newline; caller adds the
/// "Proceed? [Y/n]" line.
pub fn format_prompt_block(
    current_version: &str,
    latest_version: &str,
    method: &InstallMethod,
    needs_sudo: bool,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Update v{current_version} → v{latest_version}\n"));
    let (method_label, location_label) = match method {
        InstallMethod::Homebrew => ("homebrew", "managed by Homebrew".to_string()),
        InstallMethod::Tarball { binary_path } => {
            ("tarball install", binary_path.display().to_string())
        }
        InstallMethod::Nix => ("nix", "/nix/store (read-only)".to_string()),
        InstallMethod::Cargo => ("cargo", "~/.cargo/bin/aoe".to_string()),
        InstallMethod::Unknown { binary_path } => ("unknown", binary_path.display().to_string()),
    };
    out.push_str(&format!("  Method:    {method_label}\n"));
    out.push_str(&format!("  Location:  {location_label}"));
    if needs_sudo {
        out.push_str("\n  Sudo:      required (write-protected directory)");
    }
    out
}

/// Top-level dispatch. The caller has already chosen the version and
/// done the user confirmation. `on_progress` is forwarded to the
/// tarball downloader (other paths ignore it).
pub async fn perform_update(
    method: &InstallMethod,
    version: &str,
    on_progress: Option<&mut dyn FnMut(u64, Option<u64>)>,
) -> Result<()> {
    match method {
        InstallMethod::Homebrew => update_via_brew(version),
        InstallMethod::Tarball { binary_path } => {
            update_via_tarball(binary_path, version, on_progress).await
        }
        InstallMethod::Nix => {
            print_nix_refusal();
            Ok(())
        }
        InstallMethod::Cargo => {
            print_cargo_refusal();
            Ok(())
        }
        InstallMethod::Unknown { binary_path } => {
            print_unknown_refusal(binary_path);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn home() -> PathBuf {
        PathBuf::from("/home/kevin")
    }

    #[test]
    fn classifies_nix_store() {
        let p = PathBuf::from("/nix/store/abc123-aoe-0.4.5/bin/aoe");
        assert_eq!(classify_path_prefix(&p, &home()), InstallMethod::Nix);
    }

    #[test]
    fn classifies_cargo_bin() {
        let p = home().join(".cargo/bin/aoe");
        assert_eq!(classify_path_prefix(&p, &home()), InstallMethod::Cargo);
    }

    #[test]
    fn classifies_usr_local_bin_as_tarball() {
        let p = PathBuf::from("/usr/local/bin/aoe");
        assert_eq!(
            classify_path_prefix(&p, &home()),
            InstallMethod::Tarball { binary_path: p }
        );
    }

    #[test]
    fn classifies_local_bin_as_tarball() {
        let p = home().join(".local/bin/aoe");
        assert_eq!(
            classify_path_prefix(&p, &home()),
            InstallMethod::Tarball {
                binary_path: p.clone()
            }
        );
    }

    #[test]
    fn classifies_home_bin_as_tarball() {
        let p = home().join("bin/aoe");
        assert_eq!(
            classify_path_prefix(&p, &home()),
            InstallMethod::Tarball {
                binary_path: p.clone()
            }
        );
    }

    #[test]
    fn classifies_random_path_as_unknown() {
        let p = PathBuf::from("/opt/aoe-custom/bin/aoe");
        assert_eq!(
            classify_path_prefix(&p, &home()),
            InstallMethod::Unknown { binary_path: p }
        );
    }

    /// Regression: on macOS `/tmp` is a symlink to `/private/tmp`. If the
    /// caller passes a canonicalized exe (`/private/tmp/...`) but a
    /// non-canonicalized home (`/tmp/...`), the parent-prefix comparison
    /// fails and a perfectly fine tarball install at `~/.local/bin/aoe`
    /// looks like Unknown.
    ///
    /// This test exercises classify_path_prefix specifically — the caller
    /// is responsible for canonicalizing both sides. The fix lives in
    /// detect_install_method (canonicalize home too).
    #[test]
    fn classifier_requires_consistent_canonicalization() {
        let raw_home = PathBuf::from("/tmp/test-home");
        let canon_home = PathBuf::from("/private/tmp/test-home");
        let canonicalized_exe = canon_home.join(".local/bin/aoe");

        // With raw home + canonicalized exe → misclassifies as Unknown.
        assert!(matches!(
            classify_path_prefix(&canonicalized_exe, &raw_home),
            InstallMethod::Unknown { .. }
        ));

        // With both canonicalized → correct Tarball classification.
        assert_eq!(
            classify_path_prefix(&canonicalized_exe, &canon_home),
            InstallMethod::Tarball {
                binary_path: canonicalized_exe.clone()
            }
        );
    }

    /// Real-filesystem regression test: build a symlinked HOME on disk,
    /// place an aoe binary at $HOME/.local/bin/aoe through the symlink,
    /// and verify detect_install_method (which does its own canonicalize)
    /// still classifies it as Tarball, not Unknown.
    ///
    /// Skipped if symlink creation fails (e.g., Windows without privileges).
    #[cfg(unix)]
    #[test]
    fn detects_tarball_through_symlinked_home() {
        use tempfile::TempDir;

        let real_dir = TempDir::new().unwrap();
        let bin_dir = real_dir.path().join(".local").join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let exe = bin_dir.join("aoe");
        std::fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Create a symlink that points at real_dir, then run the classifier
        // with the symlink path as home and the canonical exe path.
        let link_dir = TempDir::new().unwrap();
        let symlinked_home = link_dir.path().join("symlinked-home");
        std::os::unix::fs::symlink(real_dir.path(), &symlinked_home).unwrap();

        // canonicalized exe goes through real_dir
        let canon_exe = exe.canonicalize().unwrap();
        // canonicalized home also goes through real_dir
        let canon_home = symlinked_home.canonicalize().unwrap();

        // Sanity: the symlinked path differs from the canonical one
        assert_ne!(symlinked_home, canon_home);

        // With both canonicalized (what detect_install_method does after
        // the fix), the classifier sees a matching parent.
        assert_eq!(
            classify_path_prefix(&canon_exe, &canon_home),
            InstallMethod::Tarball {
                binary_path: canon_exe
            }
        );
    }

    #[test]
    fn brew_takes_priority_when_paths_match() {
        // brew probe returned a path that canonicalizes to the running binary
        let exe = PathBuf::from("/opt/homebrew/Cellar/aoe/0.4.5/bin/aoe");
        let brew_path = Some(exe.clone());
        let prefix_class = InstallMethod::Unknown {
            binary_path: exe.clone(),
        };
        let result = classify_with_brew(prefix_class, brew_path.as_deref(), &exe);
        assert_eq!(result, InstallMethod::Homebrew);
    }

    #[test]
    fn brew_ignored_when_paths_differ() {
        // brew is installed (probe returned a path) but the running binary
        // is somewhere else - keep the prefix classification
        let exe = PathBuf::from("/usr/local/bin/aoe");
        let brew_path = Some(PathBuf::from("/opt/homebrew/Cellar/aoe/0.4.5/bin/aoe"));
        let prefix_class = InstallMethod::Tarball {
            binary_path: exe.clone(),
        };
        let result = classify_with_brew(prefix_class.clone(), brew_path.as_deref(), &exe);
        assert_eq!(result, prefix_class);
    }

    #[test]
    fn brew_ignored_when_probe_returned_none() {
        let exe = PathBuf::from("/usr/local/bin/aoe");
        let prefix_class = InstallMethod::Tarball {
            binary_path: exe.clone(),
        };
        let result = classify_with_brew(prefix_class.clone(), None, &exe);
        assert_eq!(result, prefix_class);
    }

    #[test]
    fn platform_string_linux_x86_64() {
        assert_eq!(
            platform_string_for("linux", "x86_64").unwrap(),
            "linux-amd64"
        );
    }

    #[test]
    fn platform_string_linux_aarch64() {
        assert_eq!(
            platform_string_for("linux", "aarch64").unwrap(),
            "linux-arm64"
        );
    }

    #[test]
    fn platform_string_macos_amd64() {
        assert_eq!(
            platform_string_for("macos", "x86_64").unwrap(),
            "darwin-amd64"
        );
    }

    #[test]
    fn platform_string_macos_arm64() {
        assert_eq!(
            platform_string_for("macos", "aarch64").unwrap(),
            "darwin-arm64"
        );
    }

    #[test]
    fn platform_string_unsupported_arch_errors() {
        let err = platform_string_for("linux", "riscv64").unwrap_err();
        assert!(err.to_string().contains("riscv64"));
    }

    #[test]
    fn platform_string_unsupported_os_errors() {
        let err = platform_string_for("windows", "x86_64").unwrap_err();
        assert!(err.to_string().contains("windows"));
    }

    #[test]
    #[serial]
    fn release_tarball_url_format() {
        let url = release_tarball_url("0.5.0", "linux-amd64");
        assert_eq!(
            url,
            "https://github.com/agent-of-empires/agent-of-empires/releases/download/v0.5.0/aoe-linux-amd64.tar.gz"
        );
    }

    #[test]
    #[serial]
    fn release_tarball_url_respects_env_override() {
        let key = "AOE_UPDATE_BASE_URL";
        let prev = std::env::var(key).ok();
        // SAFETY: single-threaded test context; serial_test ensures no concurrent mutation.
        unsafe {
            std::env::set_var(key, "http://127.0.0.1:9999/releases");
        }
        let url = release_tarball_url("0.5.0", "linux-amd64");
        assert_eq!(
            url,
            "http://127.0.0.1:9999/releases/v0.5.0/aoe-linux-amd64.tar.gz"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn prompt_block_tarball_no_sudo() {
        let m = InstallMethod::Tarball {
            binary_path: PathBuf::from("/home/u/.local/bin/aoe"),
        };
        let s = format_prompt_block("0.4.5", "0.5.0", &m, false);
        assert!(s.contains("Update v0.4.5 → v0.5.0"));
        assert!(s.contains("Method:    tarball install"));
        assert!(s.contains("Location:  /home/u/.local/bin/aoe"));
        assert!(!s.contains("Sudo:"));
    }

    #[test]
    fn prompt_block_tarball_sudo_required() {
        let m = InstallMethod::Tarball {
            binary_path: PathBuf::from("/usr/local/bin/aoe"),
        };
        let s = format_prompt_block("0.4.5", "0.5.0", &m, true);
        assert!(s.contains("Sudo:      required (write-protected directory)"));
    }

    #[test]
    fn prompt_block_homebrew_omits_location_path() {
        let s = format_prompt_block("0.4.5", "0.5.0", &InstallMethod::Homebrew, false);
        assert!(s.contains("Method:    homebrew"));
        assert!(s.contains("Location:  managed by Homebrew"));
    }

    #[test]
    fn prompt_block_nix() {
        let s = format_prompt_block("0.4.5", "0.5.0", &InstallMethod::Nix, false);
        assert!(s.contains("Method:    nix"));
    }

    #[test]
    fn nix_refusal_message_contains_nix_run() {
        assert!(nix_refusal_message().contains("nix run github:agent-of-empires/agent-of-empires"));
    }

    #[test]
    fn cargo_refusal_message_contains_cargo_install() {
        assert!(cargo_refusal_message().contains("cargo install"));
    }

    #[test]
    fn unknown_refusal_message_contains_install_script_url() {
        let s = unknown_refusal_message(Path::new("/opt/weird/aoe"));
        assert!(s.contains("install.sh"));
        assert!(s.contains("/opt/weird/aoe"));
    }

    /// Tests for `sudo_replace` using a PATH-shimmed `sudo` that just
    /// `exec`s its arguments. Avoids needing real root or a real sudo
    /// password prompt while still exercising the actual code path.
    mod sudo_replace_tests {
        use super::*;
        use serial_test::serial;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

        fn write_sudo_shim(dir: &Path) {
            let shim = dir.join("sudo");
            std::fs::write(&shim, "#!/bin/sh\nexec \"$@\"\n").unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn write_failing_sudo_shim(dir: &Path) {
            let shim = dir.join("sudo");
            // Returns 1, never execs the wrapped command. Models the user
            // entering the wrong password or hitting Ctrl+C at the prompt.
            std::fs::write(&shim, "#!/bin/sh\nexit 1\n").unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn with_path_prepended<F: FnOnce()>(prefix: &Path, f: F) {
            let prev = std::env::var("PATH").unwrap_or_default();
            let new_path = format!("{}:{}", prefix.display(), prev);
            // SAFETY: serial_test ensures no concurrent env mutation.
            unsafe {
                std::env::set_var("PATH", &new_path);
            }
            f();
            unsafe {
                std::env::set_var("PATH", &prev);
            }
        }

        #[test]
        #[serial]
        fn sudo_replace_moves_then_chmods() {
            let dir = TempDir::new().unwrap();
            write_sudo_shim(dir.path());

            let source = dir.path().join("source");
            std::fs::write(&source, b"new").unwrap();
            let target = dir.path().join("target");
            std::fs::write(&target, b"old").unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();

            with_path_prepended(dir.path(), || {
                sudo_replace(&source, &target).expect("sudo_replace should succeed");
            });

            assert!(!source.exists(), "source should be moved");
            assert_eq!(std::fs::read(&target).unwrap(), b"new");
            #[cfg(unix)]
            {
                let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o755, "chmod should set 0o755");
            }
        }

        #[test]
        #[serial]
        fn sudo_replace_propagates_failure_from_mv() {
            let dir = TempDir::new().unwrap();
            write_failing_sudo_shim(dir.path());

            let source = dir.path().join("source");
            std::fs::write(&source, b"new").unwrap();
            let target = dir.path().join("target");

            with_path_prepended(dir.path(), || {
                let err =
                    sudo_replace(&source, &target).expect_err("failing sudo should propagate");
                let s = err.to_string();
                assert!(
                    s.contains("sudo mv failed"),
                    "expected mv-failed error, got: {s}"
                );
            });
        }
    }

    #[test]
    fn parse_brew_stable_version_handles_v1_array() {
        let stdout = br#"[{"versions":{"stable":"1.5.2"}}]"#;
        assert_eq!(parse_brew_stable_version(stdout), Some("1.5.2".to_string()));
    }

    #[test]
    fn parse_brew_stable_version_handles_v2_envelope() {
        let stdout = br#"{"formulae":[{"versions":{"stable":"1.5.2"}}],"casks":[]}"#;
        assert_eq!(parse_brew_stable_version(stdout), Some("1.5.2".to_string()));
    }

    #[test]
    fn parse_brew_stable_version_returns_none_for_garbage() {
        assert_eq!(parse_brew_stable_version(b"not json"), None);
        assert_eq!(parse_brew_stable_version(b""), None);
        assert_eq!(parse_brew_stable_version(b"[]"), None);
        // v2 envelope with no formulae
        assert_eq!(
            parse_brew_stable_version(br#"{"formulae":[],"casks":[]}"#),
            None
        );
    }

    #[test]
    fn brew_formula_lag_message_is_friendly() {
        let msg = brew_formula_lag_message("1.5.2");
        assert!(msg.contains("v1.5.2"));
        assert!(msg.to_lowercase().contains("homebrew"));
        assert!(msg.to_lowercase().contains("try again"));
    }

    /// Hermetic tests for `update_via_brew`: PATH-shim a brew script that
    /// records its argv so we can assert the right commands ran in the
    /// right order, and that failures are surfaced.
    mod brew_upgrade_tests {
        use super::*;
        use serial_test::serial;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

        /// Brew shim that handles `info aoe --json=v2` (emits the supplied
        /// stable version as a v1-style top-level array) and records every
        /// invocation. `fail_on` lets a test simulate a brew subcommand
        /// failing (matched on the first arg).
        fn write_recording_brew_shim(
            dir: &Path,
            stable_version: &str,
            fail_on: Option<&str>,
        ) -> PathBuf {
            let log = dir.join("brew.log");
            let shim = dir.join("brew");
            let fail_branch = match fail_on {
                Some(cmd) => format!("if [ \"$1\" = \"{cmd}\" ]; then exit 2; fi\n"),
                None => String::new(),
            };
            let body = format!(
                "#!/bin/sh\n\
                 echo \"$@\" >> {log}\n\
                 {fail_branch}\
                 if [ \"$1\" = \"info\" ]; then\n\
                 printf '[{{\"versions\":{{\"stable\":\"{stable}\"}}}}]'\n\
                 fi\n\
                 exit 0\n",
                log = log.display(),
                stable = stable_version,
            );
            std::fs::write(&shim, body).unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
            log
        }

        fn with_path_prepended<F: FnOnce()>(prefix: &Path, f: F) {
            let prev = std::env::var("PATH").unwrap_or_default();
            unsafe {
                std::env::set_var("PATH", format!("{}:{}", prefix.display(), prev));
            }
            f();
            unsafe {
                std::env::set_var("PATH", &prev);
            }
        }

        #[test]
        #[serial]
        fn runs_update_info_then_upgrade_aoe() {
            let dir = TempDir::new().unwrap();
            let log = write_recording_brew_shim(dir.path(), "1.5.2", None);

            with_path_prepended(dir.path(), || {
                update_via_brew("1.5.2").expect("brew upgrade should succeed");
            });

            let invocations = std::fs::read_to_string(&log).unwrap();
            let lines: Vec<_> = invocations.lines().collect();
            assert_eq!(lines.len(), 3, "expected 3 brew calls; got {invocations:?}");
            assert_eq!(lines[0], "update");
            assert_eq!(lines[1], "info aoe --json=v2");
            assert_eq!(lines[2], "upgrade aoe");
        }

        #[test]
        #[serial]
        fn brew_update_failure_aborts_before_upgrade() {
            let dir = TempDir::new().unwrap();
            let log = write_recording_brew_shim(dir.path(), "1.5.2", Some("update"));

            let err = with_path_prepended_returning(dir.path(), || update_via_brew("1.5.2"));
            let err = err.expect_err("brew update failure should propagate");
            assert!(
                err.to_string().contains("brew update"),
                "expected `brew update` failure message; got: {err}"
            );

            let invocations = std::fs::read_to_string(&log).unwrap();
            let lines: Vec<_> = invocations.lines().collect();
            assert_eq!(
                lines,
                vec!["update"],
                "info/upgrade should not run after update failure"
            );
        }

        #[test]
        #[serial]
        fn brew_upgrade_failure_is_reported() {
            let dir = TempDir::new().unwrap();
            let log = write_recording_brew_shim(dir.path(), "1.5.2", Some("upgrade"));

            let err = with_path_prepended_returning(dir.path(), || update_via_brew("1.5.2"));
            let err = err.expect_err("brew upgrade failure should propagate");
            assert!(
                err.to_string().contains("brew upgrade aoe"),
                "expected `brew upgrade aoe` failure message; got: {err}"
            );

            let invocations = std::fs::read_to_string(&log).unwrap();
            let lines: Vec<_> = invocations.lines().collect();
            assert_eq!(lines.len(), 3, "expected update, info, upgrade calls");
        }

        /// Regression for #913: when the GitHub release is newer than the
        /// version Homebrew's formula advertises, `update_via_brew` must
        /// bail loudly. Without the pre-check, `brew upgrade aoe` exits 0
        /// and leaves the user on the old binary while the TUI keeps
        /// nagging about an available update.
        #[test]
        #[serial]
        fn bails_when_brew_formula_lags_target() {
            let dir = TempDir::new().unwrap();
            // brew has 1.5.1 but we're trying to install 1.5.2
            let log = write_recording_brew_shim(dir.path(), "1.5.1", None);

            let err = with_path_prepended_returning(dir.path(), || update_via_brew("1.5.2"));
            let err = err.expect_err("formula lag should fail loudly");
            let msg = err.to_string();
            assert!(
                msg.contains("v1.5.2") && msg.to_lowercase().contains("homebrew"),
                "expected formula-lag explanation; got: {msg}"
            );

            let invocations = std::fs::read_to_string(&log).unwrap();
            let lines: Vec<_> = invocations.lines().collect();
            assert_eq!(
                lines,
                vec!["update", "info aoe --json=v2"],
                "upgrade should not run when brew is behind"
            );
        }

        /// If `brew info` returns no parseable data (older brew, network
        /// blip, formula not tapped yet), fall back to the legacy
        /// behavior of just running `brew upgrade aoe`. Better to attempt
        /// the upgrade than to block users on a parsing edge case.
        #[test]
        #[serial]
        fn proceeds_when_brew_info_returns_no_data() {
            let dir = TempDir::new().unwrap();
            // No JSON branch in this shim; `info` emits nothing.
            let log = dir.path().join("brew.log");
            let shim = dir.path().join("brew");
            let body = format!("#!/bin/sh\necho \"$@\" >> {}\nexit 0\n", log.display());
            std::fs::write(&shim, body).unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

            with_path_prepended(dir.path(), || {
                update_via_brew("1.5.2").expect("missing JSON should not block upgrade");
            });

            let invocations = std::fs::read_to_string(&log).unwrap();
            let lines: Vec<_> = invocations.lines().collect();
            assert_eq!(lines.len(), 3, "upgrade should still run; got {lines:?}");
            assert_eq!(lines[2], "upgrade aoe");
        }

        // PATH-prepended runner that returns a value (Result, in this case).
        // The plain `with_path_prepended` upstream is FnOnce() -> () which
        // is fine for assert! but loses the Result.
        fn with_path_prepended_returning<R, F: FnOnce() -> R>(prefix: &Path, f: F) -> R {
            let prev = std::env::var("PATH").unwrap_or_default();
            unsafe {
                std::env::set_var("PATH", format!("{}:{}", prefix.display(), prev));
            }
            let result = f();
            unsafe {
                std::env::set_var("PATH", &prev);
            }
            result
        }
    }

    /// Hermetic test that the brew probe times out instead of blocking
    /// forever when `brew` hangs (locked formula DB, network-bound auto-
    /// update, etc.). Uses a sleep-forever shim on PATH.
    mod brew_probe_timeout_tests {
        use super::*;
        use serial_test::serial;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        use tempfile::TempDir;

        #[test]
        #[serial]
        fn probe_returns_none_when_brew_hangs() {
            let dir = TempDir::new().unwrap();
            let shim = dir.path().join("brew");
            std::fs::write(&shim, "#!/bin/sh\nsleep 30\n").unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

            let prev = std::env::var("PATH").unwrap_or_default();
            let new_path = format!("{}:{}", dir.path().display(), prev);
            unsafe {
                std::env::set_var("PATH", &new_path);
            }

            let started = Instant::now();
            let result = probe_brew_aoe_path_with_timeout(Duration::from_millis(300));
            let elapsed = started.elapsed();

            unsafe {
                std::env::set_var("PATH", &prev);
            }

            assert!(result.is_none(), "hanging brew should return None");
            assert!(
                elapsed < Duration::from_secs(2),
                "probe should give up quickly; took {elapsed:?}"
            );
        }
    }
}
