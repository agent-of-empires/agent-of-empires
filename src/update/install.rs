//! Self-update: detect install method, perform update.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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

/// `Homebrew` only when `brew list aoe` resolves to the running binary.
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
    // Canonicalize home too: a symlinked HOME (macOS `/tmp`) would otherwise misclassify as Unknown.
    let home = dirs::home_dir().context("locating home directory")?;
    let home = home.canonicalize().unwrap_or(home);
    let prefix = classify_path_prefix(&exe, &home);
    let brew_path = probe_brew_aoe_path();
    Ok(classify_with_brew(prefix, brew_path.as_deref(), &exe))
}

/// Detection runs on TUI startup, so this bounds startup latency.
const BREW_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn probe_brew_aoe_path() -> Option<PathBuf> {
    probe_brew_aoe_path_with_timeout(BREW_PROBE_TIMEOUT)
}

fn probe_brew_aoe_path_with_timeout(timeout: std::time::Duration) -> Option<PathBuf> {
    let mut cmd = Command::new("brew");
    cmd.args(["list", "aoe"]);

    let output = crate::process::run_with_timeout(&mut cmd, timeout)
        .ok()
        .flatten()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with("/aoe") || trimmed.ends_with("/bin/aoe") {
            return Some(PathBuf::from(trimmed));
        }
    }
    None
}

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

fn extract_tarball(tarball: &Path, dest_dir: &Path, platform: &str) -> Result<PathBuf> {
    let mut cmd = Command::new("tar");
    cmd.arg("xzf").arg(tarball).arg("-C").arg(dest_dir);
    // Runs inside `IgnoreSignalsGuard`'s window; reset signals so `tar` can be interrupted.
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut cmd);
    let status = cmd.status().context("running `tar xzf`")?;
    if !status.success() {
        anyhow::bail!("tar extraction failed (exit {})", status);
    }
    let extracted = dest_dir.join(format!("aoe-{platform}"));
    if !extracted.exists() {
        anyhow::bail!("extracted tarball did not contain {}", extracted.display());
    }
    Ok(extracted)
}

fn sanity_check_binary(binary: &Path, expected_version: &str) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.arg("--version");
    // Runs inside `IgnoreSignalsGuard`'s window; see `extract_tarball`.
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut cmd);
    let output = cmd
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

/// Both paths must share a filesystem. Falls back to `sudo mv` + `sudo chmod` on `EACCES`.
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
    let mut mv_cmd = Command::new("sudo");
    mv_cmd.arg("mv").arg(source).arg(target);
    // Runs inside `IgnoreSignalsGuard`'s window; see `extract_tarball`.
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut mv_cmd);
    let mv_status = mv_cmd.status().context("invoking `sudo mv`")?;
    if !mv_status.success() {
        anyhow::bail!("sudo mv failed (exit {})", mv_status);
    }
    let mut chmod_cmd = Command::new("sudo");
    chmod_cmd.arg("chmod").arg("0755").arg(target);
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut chmod_cmd);
    let chmod_status = chmod_cmd.status().context("invoking `sudo chmod`")?;
    if !chmod_status.success() {
        anyhow::bail!("sudo chmod failed (exit {})", chmod_status);
    }
    Ok(())
}

pub fn parent_is_writable(binary_path: &Path) -> bool {
    let Some(parent) = binary_path.parent() else {
        return false;
    };
    tempfile::Builder::new()
        .prefix(".aoe-update-probe-")
        .tempfile_in(parent)
        .is_ok()
}

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

const BREW_INFO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Deserialize)]
struct BrewInfoEntry {
    versions: BrewVersions,
}

#[derive(Deserialize)]
struct BrewVersions {
    stable: Option<String>,
}

fn brew_available_version() -> Option<String> {
    brew_available_version_with_timeout(BREW_INFO_TIMEOUT)
}

fn brew_available_version_with_timeout(timeout: std::time::Duration) -> Option<String> {
    let mut cmd = Command::new("brew");
    cmd.args(["info", "aoe", "--json=v2"]);
    // Runs inside `IgnoreSignalsGuard`'s window; see `extract_tarball`.
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut cmd);

    let output = crate::process::run_with_timeout(&mut cmd, timeout)
        .ok()
        .flatten()?;
    if !output.status.success() {
        return None;
    }
    parse_brew_stable_version(&output.stdout)
}

/// Accepts both the v2 `formulae` envelope and the bare v1 array.
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

/// Whether `aoe update` can reach `target_version` from this install: `false` only for a
/// Homebrew install whose formula is older, which `update_via_brew` refuses. An unknown
/// formula version keeps the update on offer.
fn supports_target(method: &InstallMethod, brew_version: Option<&str>, target: &str) -> bool {
    match (method, brew_version) {
        (InstallMethod::Homebrew, Some(v)) => !is_newer_version(target, v),
        _ => true,
    }
}

/// `false` only while the Homebrew formula lags the release, so the TUI and the Web dashboard
/// do not offer an update `aoe update` would refuse. Blocking; call from `spawn_blocking` on
/// tokio, or use [`update_actionable`].
pub fn install_method_supports_target(target_version: &str) -> bool {
    let Ok(method) = detect_install_method() else {
        return true;
    };
    let brew_version = matches!(method, InstallMethod::Homebrew)
        .then(brew_available_version)
        .flatten();
    let supported = supports_target(&method, brew_version.as_deref(), target_version);
    if !supported {
        tracing::info!(
            target: "update.suppress",
            brew_version = ?brew_version,
            target_version = %target_version,
            "suppressing update banner: Homebrew formula lags GitHub release"
        );
    }
    supported
}

/// The formula usually catches up within a few hours, so an hour bounds how long a stale
/// answer hides or shows the banner.
const ACTIONABLE_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

struct Actionable {
    target: String,
    checked_at: std::time::Instant,
    supported: bool,
}

static ACTIONABLE: tokio::sync::Mutex<Option<Actionable>> = tokio::sync::Mutex::const_new(None);

/// [`install_method_supports_target`], cached per target for an hour. The Web dashboard asks on
/// every poll and tab switch from every Device, and each probe shells out to `brew` twice.
pub async fn update_actionable(target_version: &str) -> bool {
    actionable_detached(
        &ACTIONABLE,
        target_version,
        std::time::Instant::now(),
        install_method_supports_target,
    )
    .await
}

/// Runs the lookup in its own task. A request dropped mid-probe (the client went away) would
/// otherwise release the lock while the blocking probe keeps running, so the next request starts
/// a second probe and the first answer is never cached.
async fn actionable_detached(
    cache: &'static tokio::sync::Mutex<Option<Actionable>>,
    target: &str,
    now: std::time::Instant,
    probe: impl FnOnce(&str) -> bool + Send + 'static,
) -> bool {
    let target = target.to_string();
    tokio::spawn(async move { actionable_cached(cache, &target, now, probe).await })
        .await
        .unwrap_or(true)
}

/// The lock is held across the probe, so requests that miss together share one probe.
async fn actionable_cached(
    cache: &tokio::sync::Mutex<Option<Actionable>>,
    target: &str,
    now: std::time::Instant,
    probe: impl FnOnce(&str) -> bool + Send + 'static,
) -> bool {
    let mut entry = cache.lock().await;
    if let Some(hit) = entry.as_ref().filter(|e| {
        e.target == target && now.saturating_duration_since(e.checked_at) < ACTIONABLE_TTL
    }) {
        return hit.supported;
    }
    let owned = target.to_string();
    let supported = tokio::task::spawn_blocking(move || probe(&owned))
        .await
        .unwrap_or(true);
    *entry = Some(Actionable {
        target: target.to_string(),
        checked_at: now,
        supported,
    });
    supported
}

const CARGO_UPDATE_COMMAND: &str =
    "cargo install --git https://github.com/agent-of-empires/agent-of-empires aoe";
const NIX_UPDATE_COMMAND: &str = "nix profile upgrade agent-of-empires";

/// How this install updates, as `/api/system/update-status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallKind {
    Homebrew,
    Release,
    Cargo,
    Nix,
    Unknown,
}

impl From<&InstallMethod> for InstallKind {
    fn from(method: &InstallMethod) -> Self {
        match method {
            InstallMethod::Homebrew => InstallKind::Homebrew,
            InstallMethod::Tarball { .. } => InstallKind::Release,
            InstallMethod::Cargo => InstallKind::Cargo,
            InstallMethod::Nix => InstallKind::Nix,
            InstallMethod::Unknown { .. } => InstallKind::Unknown,
        }
    }
}

impl InstallKind {
    /// What to run on the machine running AoE. Where `aoe update` can't act it prints the way
    /// that can, so it is also the answer for an unknown install.
    pub fn update_command(self) -> &'static str {
        match self {
            InstallKind::Cargo => CARGO_UPDATE_COMMAND,
            InstallKind::Nix => NIX_UPDATE_COMMAND,
            InstallKind::Homebrew | InstallKind::Release | InstallKind::Unknown => "aoe update",
        }
    }
}

static INSTALL_KIND: tokio::sync::OnceCell<InstallKind> = tokio::sync::OnceCell::const_new();

/// The running binary's install method, detected once per process (it shells out to `brew`).
/// A detection error counts as unknown.
pub async fn install_kind() -> InstallKind {
    install_kind_detached(&INSTALL_KIND, || {
        detect_install_method().map_or(InstallKind::Unknown, |method| InstallKind::from(&method))
    })
    .await
}

/// In its own task for the same reason as [`actionable_detached`]: a dropped request must not
/// abandon a detection that is already running.
async fn install_kind_detached(
    cell: &'static tokio::sync::OnceCell<InstallKind>,
    detect: impl FnOnce() -> InstallKind + Send + 'static,
) -> InstallKind {
    tokio::spawn(async move {
        *cell
            .get_or_init(|| async {
                tokio::task::spawn_blocking(detect)
                    .await
                    .unwrap_or(InstallKind::Unknown)
            })
            .await
    })
    .await
    .unwrap_or(InstallKind::Unknown)
}

fn update_via_brew(target_version: &str) -> Result<()> {
    let mut update_cmd = Command::new("brew");
    update_cmd.args(["update"]);
    // Runs inside `IgnoreSignalsGuard`'s window; see `extract_tarball`.
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut update_cmd);
    let status = update_cmd.status().context("running `brew update`")?;
    if !status.success() {
        anyhow::bail!("`brew update` failed (exit {})", status);
    }

    // A lagging formula makes `brew upgrade aoe` exit 0 without upgrading.
    let brew_version = brew_available_version();
    if !supports_target(
        &InstallMethod::Homebrew,
        brew_version.as_deref(),
        target_version,
    ) {
        anyhow::bail!(brew_formula_lag_message(target_version));
    }

    let mut upgrade_cmd = Command::new("brew");
    upgrade_cmd.args(["upgrade", "aoe"]);
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut upgrade_cmd);
    let status = upgrade_cmd.status().context("running `brew upgrade aoe`")?;
    if !status.success() {
        anyhow::bail!("`brew upgrade aoe` failed (exit {})", status);
    }
    Ok(())
}

fn nix_refusal_message() -> String {
    format!(
        "aoe was installed via Nix. Update by running:\n\n    {NIX_UPDATE_COMMAND}\n\n(check the entry name with `nix profile list`), or, if aoe comes from a flake input (NixOS or home-manager), run `nix flake update agent-of-empires` and rebuild (`nixos-rebuild switch`, `home-manager switch`, ...)."
    )
}

fn print_nix_refusal() {
    println!("{}", nix_refusal_message());
}

fn cargo_refusal_message() -> String {
    format!(
        "aoe was installed via cargo. Update by running:\n\n    {CARGO_UPDATE_COMMAND}\n\n(or `git pull && cargo install --path .` from a local clone)."
    )
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
    fn classify_path_prefix_maps_install_locations() {
        let home = home();
        let nix = |_: &Path| InstallMethod::Nix;
        let cargo = |_: &Path| InstallMethod::Cargo;
        let tarball = |p: &Path| InstallMethod::Tarball {
            binary_path: p.to_path_buf(),
        };
        let unknown = |p: &Path| InstallMethod::Unknown {
            binary_path: p.to_path_buf(),
        };
        let cases: &[(PathBuf, &dyn Fn(&Path) -> InstallMethod)] = &[
            (PathBuf::from("/nix/store/abc123-aoe-0.4.5/bin/aoe"), &nix),
            (home.join(".cargo/bin/aoe"), &cargo),
            (PathBuf::from("/usr/local/bin/aoe"), &tarball),
            (home.join(".local/bin/aoe"), &tarball),
            (home.join("bin/aoe"), &tarball),
            (PathBuf::from("/opt/aoe-custom/bin/aoe"), &unknown),
        ];
        for (path, expected) in cases {
            assert_eq!(
                classify_path_prefix(path, &home),
                expected(path.as_path()),
                "{path:?}"
            );
        }
    }

    #[test]
    fn only_a_lagging_homebrew_formula_hides_the_update() {
        let tarball = InstallMethod::Tarball {
            binary_path: home().join(".local/bin/aoe"),
        };
        let unknown = InstallMethod::Unknown {
            binary_path: PathBuf::from("/opt/aoe"),
        };
        let cases = [
            (InstallMethod::Homebrew, Some("1.0.0"), false),
            (InstallMethod::Homebrew, Some("1.1.0"), true),
            (InstallMethod::Homebrew, Some("1.2.0"), true),
            // `brew info` failed: keep the update on offer.
            (InstallMethod::Homebrew, None, true),
            (tarball, Some("1.0.0"), true),
            (InstallMethod::Cargo, Some("1.0.0"), true),
            (InstallMethod::Nix, Some("1.0.0"), true),
            (unknown, Some("1.0.0"), true),
        ];
        for (method, brew_version, supported) in cases {
            assert_eq!(
                supports_target(&method, brew_version, "1.1.0"),
                supported,
                "{method:?} {brew_version:?}"
            );
        }
    }

    #[tokio::test]
    async fn actionable_probes_once_per_target_and_lifetime() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let probe = |answer: bool| {
            let calls = calls.clone();
            move |_: &str| {
                calls.fetch_add(1, Ordering::SeqCst);
                answer
            }
        };
        let cache = tokio::sync::Mutex::new(None);
        let now = std::time::Instant::now();

        // Two requests that miss together share one probe.
        let (a, b) = tokio::join!(
            actionable_cached(&cache, "1.1.0", now, probe(false)),
            actionable_cached(&cache, "1.1.0", now, probe(false)),
        );
        assert_eq!((a, b, calls.load(Ordering::SeqCst)), (false, false, 1));

        let later = now + ACTIONABLE_TTL - std::time::Duration::from_secs(1);
        assert!(!actionable_cached(&cache, "1.1.0", later, probe(true)).await);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "hit within the lifetime");

        assert!(actionable_cached(&cache, "1.2.0", later, probe(true)).await);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a new release probes again"
        );

        let expired = later + ACTIONABLE_TTL;
        assert!(!actionable_cached(&cache, "1.2.0", expired, probe(false)).await);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "an expired entry probes again"
        );
    }

    #[tokio::test]
    async fn a_cancelled_request_still_caches_its_probe() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache: &'static _ = Box::leak(Box::new(tokio::sync::Mutex::new(None)));
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let now = std::time::Instant::now();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let first_calls = calls.clone();
        let first = tokio::spawn(actionable_detached(cache, "1.1.0", now, move |_: &str| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            first_calls.fetch_add(1, Ordering::SeqCst);
            false
        }));
        entered_rx.await.unwrap();
        // The client goes away while `brew` is still running.
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let second_calls = calls.clone();
        let second = tokio::spawn(actionable_detached(cache, "1.1.0", now, move |_: &str| {
            second_calls.fetch_add(1, Ordering::SeqCst);
            true
        }));
        release_tx.send(()).unwrap();
        assert!(
            !second.await.unwrap(),
            "the second request reads the first probe's answer"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one probe ran");
        assert!(cache
            .lock()
            .await
            .as_ref()
            .is_some_and(|e| e.target == "1.1.0" && !e.supported));
    }

    #[test]
    fn install_kinds_serialise_and_name_the_command_the_cli_prints() {
        let tarball = InstallMethod::Tarball {
            binary_path: home().join(".local/bin/aoe"),
        };
        let unknown = InstallMethod::Unknown {
            binary_path: PathBuf::from("/opt/aoe"),
        };
        let cases = [
            (InstallMethod::Homebrew, "homebrew", "aoe update"),
            (tarball, "release", "aoe update"),
            (InstallMethod::Cargo, "cargo", CARGO_UPDATE_COMMAND),
            (InstallMethod::Nix, "nix", NIX_UPDATE_COMMAND),
            (unknown, "unknown", "aoe update"),
        ];
        for (method, wire, command) in cases {
            let kind = InstallKind::from(&method);
            assert_eq!(serde_json::to_value(kind).unwrap(), wire, "{method:?}");
            assert_eq!(kind.update_command(), command, "{method:?}");
        }
        // The dashboard and the CLI refusal name the same command.
        assert!(cargo_refusal_message().contains(InstallKind::Cargo.update_command()));
        let nix = nix_refusal_message();
        assert!(nix.contains(InstallKind::Nix.update_command()));
        assert!(nix.contains("nix flake update"));
    }

    #[tokio::test]
    async fn a_cancelled_request_still_caches_the_install_kind() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cell: &'static _ = Box::leak(Box::new(tokio::sync::OnceCell::new()));
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let first_calls = calls.clone();
        let first = tokio::spawn(install_kind_detached(cell, move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            first_calls.fetch_add(1, Ordering::SeqCst);
            InstallKind::Cargo
        }));
        entered_rx.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let second_calls = calls.clone();
        let second = tokio::spawn(install_kind_detached(cell, move || {
            second_calls.fetch_add(1, Ordering::SeqCst);
            InstallKind::Nix
        }));
        release_tx.send(()).unwrap();
        assert_eq!(
            second.await.unwrap(),
            InstallKind::Cargo,
            "the first detection's answer"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one detection ran");
        assert_eq!(cell.get(), Some(&InstallKind::Cargo));
    }

    #[test]
    fn brew_classification_needs_a_probe_path_equal_to_the_exe() {
        let brew_exe = PathBuf::from("/opt/homebrew/Cellar/aoe/0.4.5/bin/aoe");
        let other_exe = PathBuf::from("/usr/local/bin/aoe");
        let tarball = InstallMethod::Tarball {
            binary_path: other_exe.clone(),
        };
        let cases = [
            (
                &brew_exe,
                Some(brew_exe.as_path()),
                InstallMethod::Unknown {
                    binary_path: brew_exe.clone(),
                },
                InstallMethod::Homebrew,
            ),
            (
                &other_exe,
                Some(brew_exe.as_path()),
                tarball.clone(),
                tarball.clone(),
            ),
            (&other_exe, None, tarball.clone(), tarball.clone()),
        ];
        for (exe, brew_path, prefix_class, expected) in cases {
            assert_eq!(classify_with_brew(prefix_class, brew_path, exe), expected);
        }
    }

    #[test]
    fn platform_string_maps_supported_targets_and_rejects_the_rest() {
        for (os, arch, expected) in [
            ("linux", "x86_64", "linux-amd64"),
            ("linux", "aarch64", "linux-arm64"),
            ("macos", "x86_64", "darwin-amd64"),
            ("macos", "aarch64", "darwin-arm64"),
        ] {
            assert_eq!(platform_string_for(os, arch).unwrap(), expected);
        }
        for (os, arch) in [("linux", "riscv64"), ("windows", "x86_64")] {
            let err = platform_string_for(os, arch).unwrap_err().to_string();
            assert!(err.contains(arch) || err.contains(os), "{err}");
        }
    }

    #[test]
    #[serial]
    fn release_tarball_url_honors_the_base_url_override() {
        for (base, expected) in [
            (
                None,
                "https://github.com/agent-of-empires/agent-of-empires/releases/download/v0.5.0/aoe-linux-amd64.tar.gz",
            ),
            (
                Some("http://127.0.0.1:9999/releases"),
                "http://127.0.0.1:9999/releases/v0.5.0/aoe-linux-amd64.tar.gz",
            ),
        ] {
            let _env = match base {
                Some(base) => {
                    crate::session::test_support::EnvGuard::set(&[("AOE_UPDATE_BASE_URL", base)])
                }
                None => crate::session::test_support::EnvGuard::unset(&["AOE_UPDATE_BASE_URL"]),
            };
            assert_eq!(release_tarball_url("0.5.0", "linux-amd64"), expected);
        }
    }

    #[test]
    fn prompt_block_reports_method_location_and_sudo() {
        let tarball = |p: &str| InstallMethod::Tarball {
            binary_path: PathBuf::from(p),
        };
        let cases = [
            (
                tarball("/home/u/.local/bin/aoe"),
                false,
                "Update v0.4.5 \u{2192} v0.5.0|Method:    tarball install|Location:  /home/u/.local/bin/aoe",
            ),
            (
                tarball("/usr/local/bin/aoe"),
                true,
                "Sudo:      required (write-protected directory)",
            ),
            (
                InstallMethod::Homebrew,
                false,
                "Method:    homebrew|Location:  managed by Homebrew",
            ),
            (InstallMethod::Nix, false, "Method:    nix"),
        ];
        for (method, sudo, needles) in &cases {
            let s = format_prompt_block("0.4.5", "0.5.0", method, *sudo);
            for needle in needles.split('|') {
                assert!(s.contains(needle), "{needle} missing from {s}");
            }
            assert_eq!(s.contains("Sudo:"), *sudo);
        }
    }

    #[test]
    #[serial]
    fn sudo_replace_moves_then_chmods_and_propagates_mv_failure() {
        for (shim_body, ok) in [("exec \"$@\"", true), ("exit 1", false)] {
            let dir = tempfile::TempDir::new().unwrap();
            let shim = dir.path().join("sudo");
            std::fs::write(&shim, format!("#!/bin/sh\n{shim_body}\n")).unwrap();
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
            let source = dir.path().join("source");
            std::fs::write(&source, b"new").unwrap();
            let target = dir.path().join("target");
            std::fs::write(&target, b"old").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();

            let _path = crate::session::test_support::path_prepended(dir.path());
            let result = sudo_replace(&source, &target);
            if ok {
                result.expect("sudo_replace should succeed");
                assert!(!source.exists(), "source should be moved");
                assert_eq!(std::fs::read(&target).unwrap(), b"new");
                let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o755, "chmod should set 0o755");
            } else {
                let err = result
                    .expect_err("failing sudo should propagate")
                    .to_string();
                assert!(err.contains("sudo mv failed"), "{err}");
            }
        }
    }

    #[test]
    fn parse_brew_stable_version_reads_v1_and_v2_and_rejects_garbage() {
        let cases: [(&[u8], Option<&str>); 6] = [
            (br#"[{"versions":{"stable":"1.5.2"}}]"#, Some("1.5.2")),
            (
                br#"{"formulae":[{"versions":{"stable":"1.5.2"}}],"casks":[]}"#,
                Some("1.5.2"),
            ),
            (b"not json", None),
            (b"", None),
            (b"[]", None),
            (br#"{"formulae":[],"casks":[]}"#, None),
        ];
        for (stdout, expected) in cases {
            assert_eq!(parse_brew_stable_version(stdout).as_deref(), expected);
        }
    }

    mod brew_upgrade_tests {
        use super::*;
        use serial_test::serial;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

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
            // An empty version models `brew info` printing no JSON at all.
            let info_branch = match stable_version {
                "" => String::new(),
                stable => format!(
                    "if [ \"$1\" = \"info\" ]; then\n\
                     printf '[{{\"versions\":{{\"stable\":\"{stable}\"}}}}]'\n\
                     fi\n"
                ),
            };
            let body = format!(
                "#!/bin/sh\n\
                 echo \"$@\" >> {log}\n\
                 {fail_branch}\
                 {info_branch}\
                 exit 0\n",
                log = log.display(),
            );
            std::fs::write(&shim, body).unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
            log
        }

        #[test]
        #[serial]
        fn brew_upgrade_stops_at_the_first_failing_step() {
            // (formula version, step the shim fails on, brew calls expected, error substrings)
            let cases: [(&str, Option<&str>, &[&str], &[&str]); 5] = [
                (
                    "1.5.2",
                    None,
                    &["update", "info aoe --json=v2", "upgrade aoe"],
                    &[],
                ),
                (
                    "",
                    None,
                    &["update", "info aoe --json=v2", "upgrade aoe"],
                    &[],
                ),
                ("1.5.2", Some("update"), &["update"], &["brew update"]),
                (
                    "1.5.2",
                    Some("upgrade"),
                    &["update", "info aoe --json=v2", "upgrade aoe"],
                    &["brew upgrade aoe"],
                ),
                (
                    "1.5.1",
                    None,
                    &["update", "info aoe --json=v2"],
                    &["v1.5.2", "Homebrew"],
                ),
            ];
            for (formula_version, fail_on, expected_calls, expected_error) in cases {
                let dir = TempDir::new().unwrap();
                let log = write_recording_brew_shim(dir.path(), formula_version, fail_on);

                let _path = crate::session::test_support::path_prepended(dir.path());
                let result = update_via_brew("1.5.2");
                match expected_error {
                    [] => {
                        result.unwrap_or_else(|e| panic!("{formula_version} {fail_on:?}: {e}"));
                    }
                    needles => {
                        let msg = result.expect_err("expected a failure").to_string();
                        for needle in needles {
                            assert!(msg.contains(needle), "{needle} missing from {msg}");
                        }
                    }
                }

                let invocations = std::fs::read_to_string(&log).unwrap();
                assert_eq!(
                    invocations.lines().collect::<Vec<_>>(),
                    expected_calls,
                    "{formula_version} {fail_on:?}"
                );
            }
        }
    }

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

            let _path = crate::session::test_support::path_prepended(dir.path());

            let started = Instant::now();
            let result = probe_brew_aoe_path_with_timeout(Duration::from_millis(300));
            let elapsed = started.elapsed();

            assert!(result.is_none(), "hanging brew should return None");
            assert!(
                elapsed < Duration::from_secs(2),
                "probe should give up quickly; took {elapsed:?}"
            );
        }
    }
}
