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
    Homebrew { brew: PathBuf },
    Tarball { binary_path: PathBuf },
    Nix,
    Cargo,
    Unknown { binary_path: PathBuf },
}

/// Where Homebrew installs: Apple silicon, Intel macOS, and Linuxbrew.
const HOMEBREW_PREFIXES: [&str; 3] = ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"];

/// The prefix whose `<prefix>/Cellar/aoe/<version>/bin/aoe` is `path`, where a canonicalized
/// Homebrew `aoe` lives. Read from the path alone, so it holds when `brew` itself can't run (#5515).
fn homebrew_cellar_prefix(path: &Path) -> Option<&'static Path> {
    let prefixes = HOMEBREW_PREFIXES.map(Path::new);
    cellar_prefix_in(&prefixes, path)
}

fn cellar_prefix_in<'a>(prefixes: &[&'a Path], path: &Path) -> Option<&'a Path> {
    prefixes.iter().copied().find(|prefix| {
        let Ok(rest) = path.strip_prefix(prefix.join("Cellar").join("aoe")) else {
            return false;
        };
        let parts: Vec<_> = rest.components().map(|c| c.as_os_str()).collect();
        matches!(parts.as_slice(), [_, bin, name] if *bin == "bin" && *name == "aoe")
    })
}

/// The `<prefix>` of a running `<prefix>/Cellar/aoe/<version>/bin/aoe` that has its own `brew`,
/// which finds a custom prefix PATH's `brew` doesn't report.
fn running_cellar_prefix(path: &Path) -> Option<&Path> {
    let prefix = path.ancestors().nth(5)?;
    (cellar_prefix_in(&[prefix], path).is_some() && homebrew_brew(prefix).is_file())
        .then_some(prefix)
}

/// A prefix's own `brew`, so a second Homebrew earlier on PATH never acts for it.
pub fn homebrew_brew(prefix: &Path) -> PathBuf {
    prefix.join("bin").join("brew")
}

/// Every Homebrew prefix whose own `brew` lists `aoe`, plus the prefix whose Cellar holds
/// `running` even when its `brew` can't answer, so that removal is still attempted (#5547).
/// The candidates are the known prefixes, PATH's `brew --prefix`, and the running binary's
/// own Cellar prefix, so a custom prefix counts when either reports it.
pub fn homebrew_aoe_prefixes(running: Option<&Path>) -> Vec<PathBuf> {
    let path_prefix = path_brew_prefix(BREW_PROBE_TIMEOUT);
    select_homebrew_prefixes(
        &HOMEBREW_PREFIXES.map(Path::new),
        path_prefix.as_deref(),
        running,
        |brew| brew_lists_aoe(brew, BREW_INFO_TIMEOUT),
    )
}

fn select_homebrew_prefixes(
    known: &[&Path],
    path_prefix: Option<&Path>,
    running: Option<&Path>,
    mut lists_aoe: impl FnMut(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut candidates = known.to_vec();
    for prefix in [path_prefix, running.and_then(running_cellar_prefix)]
        .into_iter()
        .flatten()
    {
        if !candidates.contains(&prefix) {
            candidates.push(prefix);
        }
    }
    let owner = running.and_then(|path| cellar_prefix_in(&candidates, path));
    candidates
        .iter()
        .filter(|prefix| Some(**prefix) == owner || lists_aoe(&homebrew_brew(prefix)))
        .map(|prefix| prefix.to_path_buf())
        .collect()
}

/// The prefix of PATH's `brew`, which is only asked for this, never to list or remove.
fn path_brew_prefix(timeout: std::time::Duration) -> Option<PathBuf> {
    let mut cmd = Command::new("brew");
    cmd.arg("--prefix");
    let output = crate::process::run_with_timeout(&mut cmd, timeout)
        .ok()
        .flatten()
        .filter(|output| output.status.success())?;
    let prefix = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    prefix.is_absolute().then_some(prefix)
}

fn brew_lists_aoe(brew: &Path, timeout: std::time::Duration) -> bool {
    let mut cmd = Command::new(brew);
    cmd.args(["list", "aoe"]);
    crate::process::run_with_timeout(&mut cmd, timeout)
        .ok()
        .flatten()
        .is_some_and(|output| output.status.success())
}

/// Removes `aoe` with `brew`, failing when `brew uninstall` doesn't succeed.
pub fn uninstall_via_brew(brew: &Path) -> Result<()> {
    let status = Command::new(brew)
        .args(["uninstall", "aoe"])
        .status()
        .with_context(|| format!("running `{} uninstall aoe`", brew.display()))?;
    if !status.success() {
        anyhow::bail!("`{} uninstall aoe` failed ({})", brew.display(), status);
    }
    Ok(())
}

fn classify_path_prefix(binary_path: &Path, home: &Path) -> InstallMethod {
    let path = binary_path;

    if let Some(prefix) = homebrew_cellar_prefix(path) {
        return InstallMethod::Homebrew {
            brew: homebrew_brew(prefix),
        };
    }

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

/// `Homebrew`, run by PATH's `brew`, only when `brew list aoe` resolves to the running binary.
fn classify_with_brew(
    prefix: InstallMethod,
    brew_path: Option<&Path>,
    binary_path: &Path,
) -> InstallMethod {
    if let Some(bp) = brew_path {
        if paths_canonicalize_equal(bp, binary_path) {
            return InstallMethod::Homebrew {
                brew: PathBuf::from("brew"),
            };
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
    // A Cellar path already says Homebrew, and `brew` may not be on a Daemon's PATH.
    if matches!(prefix, InstallMethod::Homebrew { .. }) {
        return Ok(prefix);
    }
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
    let tag = super::release_tag(version);
    format!("{base}/{tag}/aoe-{platform}.tar.gz")
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

fn brew_available_version(brew: &Path) -> Option<String> {
    brew_available_version_with_timeout(brew, BREW_INFO_TIMEOUT)
}

fn brew_available_version_with_timeout(
    brew: &Path,
    timeout: std::time::Duration,
) -> Option<String> {
    let mut cmd = Command::new(brew);
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
        (InstallMethod::Homebrew { .. }, Some(v)) => !is_newer_version(target, v),
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
    let brew_version = match &method {
        InstallMethod::Homebrew { brew } => brew_available_version(brew),
        _ => None,
    };
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
            InstallMethod::Homebrew { .. } => InstallKind::Homebrew,
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

/// `brew` is the install's own Homebrew, so a second prefix on PATH is never updated instead.
fn update_via_brew(brew: &Path, target_version: &str) -> Result<()> {
    let mut update_cmd = Command::new(brew);
    update_cmd.args(["update"]);
    // Runs inside `IgnoreSignalsGuard`'s window; see `extract_tarball`.
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut update_cmd);
    let status = update_cmd
        .status()
        .with_context(|| format!("running `{} update`", brew.display()))?;
    if !status.success() {
        anyhow::bail!("`{} update` failed (exit {})", brew.display(), status);
    }

    // A lagging formula makes `brew upgrade aoe` exit 0 without upgrading.
    let brew_version = brew_available_version(brew);
    let method = InstallMethod::Homebrew {
        brew: brew.to_path_buf(),
    };
    if !supports_target(&method, brew_version.as_deref(), target_version) {
        anyhow::bail!(brew_formula_lag_message(target_version));
    }

    let mut upgrade_cmd = Command::new(brew);
    upgrade_cmd.args(["upgrade", "aoe"]);
    #[cfg(unix)]
    crate::process::reset_signals_on_exec(&mut upgrade_cmd);
    let status = upgrade_cmd
        .status()
        .with_context(|| format!("running `{} upgrade aoe`", brew.display()))?;
    if !status.success() {
        anyhow::bail!("`{} upgrade aoe` failed (exit {})", brew.display(), status);
    }
    Ok(())
}

fn nix_refusal_message() -> String {
    format!(
        "aoe was installed via Nix. Update by running:\n\n    {NIX_UPDATE_COMMAND}\n\n(check the entry name with `nix profile list`), or, if aoe comes from a flake input (NixOS or home-manager), run `nix flake update <input>` (your flake's name for aoe) and rebuild (`nixos-rebuild switch`, `home-manager switch`, ...)."
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
        InstallMethod::Homebrew { .. } => ("homebrew", "managed by Homebrew".to_string()),
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
        InstallMethod::Homebrew { brew } => update_via_brew(brew, version)?,
        InstallMethod::Tarball { binary_path } => {
            update_via_tarball(binary_path, version, on_progress).await?
        }
        InstallMethod::Nix => {
            print_nix_refusal();
            return Ok(());
        }
        InstallMethod::Cargo => {
            print_cargo_refusal();
            return Ok(());
        }
        InstallMethod::Unknown { binary_path } => {
            print_unknown_refusal(binary_path);
            return Ok(());
        }
    }
    tracing::info!(
        target: "update",
        from = env!("CARGO_PKG_VERSION"),
        to = version,
        method = ?method,
        "update installed"
    );
    Ok(())
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
        let brew_at = |brew: &'static str| {
            move |_: &Path| InstallMethod::Homebrew {
                brew: PathBuf::from(brew),
            }
        };
        let arm = brew_at("/opt/homebrew/bin/brew");
        let intel = brew_at("/usr/local/bin/brew");
        let linuxbrew = brew_at("/home/linuxbrew/.linuxbrew/bin/brew");
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
            // #5515: a Cellar binary is Homebrew, run by its own prefix's `brew`.
            (
                PathBuf::from("/opt/homebrew/Cellar/aoe/1.22.3/bin/aoe"),
                &arm,
            ),
            (
                PathBuf::from("/usr/local/Cellar/aoe/1.22.3/bin/aoe"),
                &intel,
            ),
            (
                PathBuf::from("/home/linuxbrew/.linuxbrew/Cellar/aoe/1.22.3/bin/aoe"),
                &linuxbrew,
            ),
            // Look-alikes stay Unknown.
            (
                PathBuf::from("/opt/homebrew/Cellar/aoe-fork/1.0/bin/aoe"),
                &unknown,
            ),
            (
                PathBuf::from("/opt/homebrew/Cellar/aoe/1.22.3/libexec/aoe"),
                &unknown,
            ),
            (PathBuf::from("/opt/homebrew/Cellar/aoe/bin/aoe"), &unknown),
            (
                PathBuf::from("/srv/opt/homebrew/Cellar/aoe/1.22.3/bin/aoe"),
                &unknown,
            ),
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
        let homebrew = InstallMethod::Homebrew {
            brew: PathBuf::from("brew"),
        };
        let tarball = InstallMethod::Tarball {
            binary_path: home().join(".local/bin/aoe"),
        };
        let unknown = InstallMethod::Unknown {
            binary_path: PathBuf::from("/opt/aoe"),
        };
        let cases = [
            (homebrew.clone(), Some("1.0.0"), false),
            (homebrew.clone(), Some("1.1.0"), true),
            (homebrew.clone(), Some("1.2.0"), true),
            // `brew info` failed: keep the update on offer.
            (homebrew, None, true),
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
            (
                InstallMethod::Homebrew {
                    brew: PathBuf::from("brew"),
                },
                "homebrew",
                "aoe update",
            ),
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
                InstallMethod::Homebrew {
                    brew: PathBuf::from("brew"),
                },
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
                InstallMethod::Homebrew {
                    brew: PathBuf::from("/opt/homebrew/bin/brew"),
                },
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

                // PATH holds the same shim, so a stray PATH `brew` never reaches a real one.
                let _path = crate::session::test_support::path_prepended(dir.path());
                let result = update_via_brew(&dir.path().join("brew"), "1.5.2");
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

        #[test]
        #[serial]
        fn brew_update_runs_the_owning_brew_never_paths() {
            for owner_installed in [true, false] {
                let owner_dir = TempDir::new().unwrap();
                let decoy_dir = TempDir::new().unwrap();
                let decoy_log = write_recording_brew_shim(decoy_dir.path(), "1.5.2", None);
                let owner_log = owner_installed
                    .then(|| write_recording_brew_shim(owner_dir.path(), "1.5.2", None));
                let owner = owner_dir.path().join("brew");

                let _path = crate::session::test_support::path_prepended(decoy_dir.path());
                let result = update_via_brew(&owner, "1.5.2");
                match owner_log {
                    Some(log) => {
                        result.unwrap();
                        let calls = std::fs::read_to_string(log).unwrap();
                        assert_eq!(
                            calls.lines().collect::<Vec<_>>(),
                            ["update", "info aoe --json=v2", "upgrade aoe"]
                        );
                    }
                    None => {
                        let msg = result.expect_err("a missing owner brew fails").to_string();
                        assert!(msg.contains(&owner.display().to_string()), "{msg}");
                    }
                }
                let decoy_calls = std::fs::read_to_string(&decoy_log).unwrap_or_default();
                assert!(
                    decoy_calls.is_empty(),
                    "owner installed {owner_installed}: PATH's brew ran {decoy_calls:?}"
                );
            }
        }
    }

    #[test]
    fn homebrew_prefixes_are_found_by_their_own_brew() {
        let (a, b) = (Path::new("/a"), Path::new("/b"));
        let custom = Path::new("/home/me/homebrew");
        let in_a = Path::new("/a/Cellar/aoe/1.5.2/bin/aoe");
        let in_custom = Path::new("/home/me/homebrew/Cellar/aoe/1.5.2/bin/aoe");
        let with_brew = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(with_brew.path().join("bin")).unwrap();
        std::fs::write(homebrew_brew(with_brew.path()), "").unwrap();
        let without_brew = tempfile::TempDir::new().unwrap();
        let cellar_aoe = |prefix: &Path| prefix.join("Cellar/aoe/1.5.2/bin/aoe");
        let (in_unreported, in_brewless) = (
            cellar_aoe(with_brew.path()),
            cellar_aoe(without_brew.path()),
        );
        // (case, PATH brew's prefix, prefixes whose brew lists aoe, running binary, expected)
        let cases: [(&str, Option<&Path>, &[&Path], Option<&Path>, &[&Path]); 12] = [
            ("none", None, &[], None, &[]),
            ("one", None, &[b], None, &[b]),
            ("both", None, &[a, b], None, &[a, b]),
            ("running from a silent prefix", None, &[], Some(in_a), &[a]),
            (
                "running from one, both listed",
                None,
                &[a, b],
                Some(in_a),
                &[a, b],
            ),
            (
                "running outside every Cellar",
                None,
                &[],
                Some(Path::new("/usr/bin/aoe")),
                &[],
            ),
            (
                "PATH brew at a custom prefix lists aoe",
                Some(custom),
                &[custom],
                None,
                &[custom],
            ),
            (
                "PATH brew at a custom prefix without aoe",
                Some(custom),
                &[a],
                None,
                &[a],
            ),
            (
                "PATH brew at a known prefix (no duplicate)",
                Some(a),
                &[a],
                None,
                &[a],
            ),
            (
                "running from a silent custom prefix",
                Some(custom),
                &[],
                Some(in_custom),
                &[custom],
            ),
            (
                "running from a custom Cellar PATH's brew doesn't report",
                Some(custom),
                &[],
                Some(&in_unreported),
                &[with_brew.path()],
            ),
            (
                "running from a Cellar-shaped path with no brew",
                None,
                &[],
                Some(&in_brewless),
                &[],
            ),
        ];
        for (case, path_prefix, listing, running, expected) in cases {
            let mut asked = Vec::new();
            let found = select_homebrew_prefixes(&[a, b], path_prefix, running, |brew| {
                asked.push(brew.to_path_buf());
                listing.iter().any(|prefix| homebrew_brew(prefix) == brew)
            });
            assert_eq!(
                found,
                expected.iter().map(|p| p.to_path_buf()).collect::<Vec<_>>(),
                "{case}"
            );
            // Each candidate's own brew, once, except the running binary's owner: never a prefix twice.
            let mut candidates = vec![a, b];
            candidates.extend(path_prefix.filter(|p| ![a, b].contains(p)));
            candidates.extend(running.and_then(running_cellar_prefix));
            let owner = running.and_then(|r| cellar_prefix_in(&candidates, r));
            let expected_asked: Vec<PathBuf> = candidates
                .iter()
                .filter(|p| Some(**p) != owner)
                .map(|p| homebrew_brew(p))
                .collect();
            assert_eq!(asked, expected_asked, "{case}");
        }
    }

    #[cfg(unix)]
    mod brew_uninstall_tests {
        use super::*;
        use serial_test::serial;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

        /// A Homebrew prefix whose `bin/brew` logs its argv and exits `list` / `uninstall`.
        fn prefix_with_brew(list: i32, uninstall: i32) -> (TempDir, PathBuf) {
            let prefix = TempDir::new().unwrap();
            let log = prefix.path().join("brew.log");
            std::fs::create_dir(prefix.path().join("bin")).unwrap();
            let brew = homebrew_brew(prefix.path());
            std::fs::write(
                &brew,
                format!(
                    "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1\" in\n  list) exit {list} ;;\n  uninstall) exit {uninstall} ;;\nesac\nexit 0\n",
                    log = log.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&brew, std::fs::Permissions::from_mode(0o755)).unwrap();
            (prefix, log)
        }

        fn calls(log: &Path) -> Vec<String> {
            std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        #[test]
        #[serial]
        fn detection_and_uninstall_run_each_prefixs_own_brew() {
            let (installed, installed_log) = prefix_with_brew(0, 0);
            let (other, other_log) = prefix_with_brew(1, 0);
            let (pinned, pinned_log) = prefix_with_brew(0, 1);
            let (custom, custom_log) = prefix_with_brew(0, 0);
            // The running binary's prefix, unknown to PATH, whose `brew` doesn't list `aoe`.
            let (running, running_log) = prefix_with_brew(1, 0);
            let running_aoe = running.path().join("Cellar/aoe/1.5.2/bin/aoe");
            let without_brew = TempDir::new().unwrap();
            // PATH's `brew` lives elsewhere and names the custom prefix; it may only be asked that.
            let decoy = TempDir::new().unwrap();
            let decoy_log = decoy.path().join("brew.log");
            let decoy_brew = decoy.path().join("brew");
            std::fs::write(
                &decoy_brew,
                format!(
                    "#!/bin/sh\necho \"$@\" >> {log}\n[ \"$1\" = --prefix ] && echo {prefix}\nexit 0\n",
                    log = decoy_log.display(),
                    prefix = custom.path().display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&decoy_brew, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _path = crate::session::test_support::path_prepended(decoy.path());

            let path_prefix = path_brew_prefix(BREW_PROBE_TIMEOUT);
            assert_eq!(path_prefix.as_deref(), Some(custom.path()));
            let known = [
                installed.path(),
                other.path(),
                pinned.path(),
                without_brew.path(),
            ];
            let found = select_homebrew_prefixes(
                &known,
                path_prefix.as_deref(),
                Some(&running_aoe),
                |brew| brew_lists_aoe(brew, BREW_INFO_TIMEOUT),
            );
            assert_eq!(
                found,
                [
                    installed.path(),
                    pinned.path(),
                    custom.path(),
                    running.path()
                ]
            );
            assert_eq!(calls(&other_log), ["list aoe"]);
            assert!(calls(&running_log).is_empty());

            for (prefix, log) in [(&installed, &installed_log), (&custom, &custom_log)] {
                uninstall_via_brew(&homebrew_brew(prefix.path())).unwrap();
                assert_eq!(calls(log), ["list aoe", "uninstall aoe"]);
            }
            uninstall_via_brew(&homebrew_brew(running.path())).unwrap();
            assert_eq!(calls(&running_log), ["uninstall aoe"]);

            let brew = homebrew_brew(pinned.path());
            let error = format!(
                "{:#}",
                uninstall_via_brew(&brew).expect_err("exit 1 must fail")
            );
            assert!(
                error.contains(&format!("`{} uninstall aoe` failed", brew.display())),
                "{error}"
            );
            assert_eq!(calls(&pinned_log), ["list aoe", "uninstall aoe"]);

            let missing = homebrew_brew(without_brew.path());
            let error = format!(
                "{:#}",
                uninstall_via_brew(&missing).expect_err("no brew must fail")
            );
            assert!(error.contains(&missing.display().to_string()), "{error}");

            assert_eq!(
                calls(&decoy_log),
                ["--prefix"],
                "PATH's brew was asked more than its prefix"
            );
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
