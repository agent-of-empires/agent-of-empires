//! Oh My Pi (OMP) session capture.
//!
//! OMP attribution is based exclusively on terminal breadcrumbs. Store layout
//! and launch identity are persisted in tmux and reloaded on every poll so a
//! restarted process cannot stay attached to a superseded pane generation.
//!
//! # On-disk wire formats (single source of truth)
//!
//! Two newline-terminated artifacts bind a capture to one pane generation.
//! There is no shared serializer: OMP owns the breadcrumb writer, `wrap_omp_launch`
//! (`session::instance`) is POSIX sh, and one reader (`CONTAINER_BREADCRUMB_SCRIPT`)
//! is also sh, so every site below must change together.
//!
//! Launch marker, exactly 4 lines (writer `wrap_omp_launch`; readers
//! `validate_launch_marker` and `CONTAINER_BREADCRUMB_SCRIPT`):
//!   1. terminal id (tty leaf, `/` rewritten to `-`)
//!   2. launch id (this generation; the compare-and-set anchor)
//!   3. non-empty pending pre-launch session path
//!   4. routing fingerprint (64 lowercase hex; the second CAS anchor)
//!
//! Terminal breadcrumb, 2 or 3 lines (written by OMP, rewritten or installed by
//! `wrap_omp_launch`; readers `wrap_omp_launch` (which reads all three fields
//! inline before it rewrites them), `parse_breadcrumb`, and
//! `CONTAINER_BREADCRUMB_SCRIPT`):
//!   1. cwd (absolute)
//!   2. session path
//!   3. optional literal `fresh`
//!
//! The two marker CAS anchors (launch id, routing fingerprint) prove the marker
//! belongs to this generation, but NOT that the breadcrumb was authored after
//! launch: a stale pre-launch breadcrumb also differs from the pending sentinel.
//! Post-launch authorship is a third, necessary invariant, proven by freshness:
//! the breadcrumb must be newer than the launch (host: `modified_at_ms >
//! launched_at_ms`; container: the breadcrumb is `-nt` the launch marker, which
//! `wrap_omp_launch` writes just before exec). See `#3230`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DOTENV_BYTES: usize = 1024 * 1024;
const MAX_CONTAINER_ENV_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONTAINER_PROBE_BYTES: usize = 16 * 1024;
const MAX_BREADCRUMB_BYTES: usize = 16 * 1024;
const MAX_LAUNCH_MARKER_BYTES: usize = MAX_BREADCRUMB_BYTES + 1024;
// Container capture streams the whole session header (grepped within the same
// window the host scans, `PI_HEADER_SCAN_BYTES`) plus the breadcrumb-derived
// cwd/session_path (bounded by `MAX_BREADCRUMB_BYTES`) and a few short marker
// fields. Deriving the transport cap from those two keeps it in lockstep with
// the host scan, so a large header captures in-container exactly where the host
// accepts it instead of failing closed once the small probe cap is exceeded.
const MAX_CONTAINER_CAPTURE_BYTES: usize = super::PI_HEADER_SCAN_BYTES + MAX_BREADCRUMB_BYTES;
pub(crate) const OMP_STORE_ENV_KEYS: [&str; 9] = [
    "HOME",
    "NODE_ENV",
    "OMP_PROFILE",
    "PI_PROFILE",
    "PI_CODING_AGENT_DIR",
    "PI_CODING_AGENT_SESSION_DIR",
    "PI_CONFIG_DIR",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
];

/// Shape of the effective OMP session store.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OmpStoreKind {
    /// OMP's normal bucket-per-cwd `sessions/<bucket>/<session>.jsonl` layout.
    Managed,
    /// An explicit flat `--session-dir` / `PI_CODING_AGENT_SESSION_DIR` layout.
    Custom,
}

/// Absolute roots used by one OMP process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OmpStoreLayout {
    pub sessions: PathBuf,
    /// Managed store retained even when `sessions` is an explicit custom store.
    pub managed_sessions: PathBuf,
    pub terminal_sessions: PathBuf,
    pub kind: OmpStoreKind,
}

/// Transient launch snapshot. Routing values are used only to resolve the
/// capture layout; only their one-way fingerprint survives into pane metadata.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct OmpCapturePlan {
    pub layout: OmpStoreLayout,
    pub routing_fingerprint: String,
    pub launch_id: String,
    pub launch_marker: String,
    pub container_runtime: Option<crate::session::config::ContainerRuntimeName>,
}

/// Stable capture inputs persisted with a tmux session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OmpCaptureMetadata {
    pub layout: OmpStoreLayout,
    pub launched_at_ms: u64,
    #[serde(default)]
    pub launch_id: String,
    #[serde(default)]
    pub launch_marker: String,
    #[serde(default)]
    pub routing_fingerprint: String,
    #[serde(default)]
    pub container_runtime: Option<crate::session::config::ContainerRuntimeName>,
}

impl OmpCaptureMetadata {
    /// Wrap a captured session id in the guard this generation warrants: a
    /// pre-marker (legacy) generation persists unguarded, a marked generation
    /// carries its launch id for the compare-and-set.
    pub(crate) fn session_observation(
        &self,
        sid: String,
    ) -> crate::session::poller::SessionIdObservation {
        if self.launch_marker.is_empty() {
            crate::session::poller::SessionIdObservation::omp_legacy(sid)
        } else {
            crate::session::poller::SessionIdObservation::omp(sid, self.launch_id.clone())
        }
    }
}

/// Store-affecting OMP flags extracted from AoE's extra argument string.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OmpCliCaptureOptions {
    pub profile: Option<String>,
    pub session_dir: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

impl OmpCliCaptureOptions {
    /// Parse a shell-word argument string while refusing constructs that can
    /// obscure store-affecting argv. Store flags use OMP's last-wins rule.
    pub(crate) fn parse(extra_args: &str) -> Result<Self> {
        let shell_words = inspect_shell_syntax(extra_args)?;
        let argv = shell_words::split(extra_args).context("Invalid OMP extra_args quoting")?;
        anyhow::ensure!(
            shell_words.len() == argv.len(),
            "Invalid OMP extra_args tokenization"
        );
        let mut parsed = Self::default();
        let mut index = 0;
        while index < argv.len() {
            let arg = &argv[index];
            let shell_word = &shell_words[index];
            if shell_word.unquoted_glob && !expansion_cannot_produce_flag(arg) {
                anyhow::bail!("OMP extra_args contains an ambiguous shell expansion");
            }
            if arg == "--" {
                break;
            }
            if arg == "--no-session" || arg.starts_with("--no-session=") {
                anyhow::bail!("OMP --no-session disables breadcrumb capture");
            }
            if arg == "--cwd" {
                if shell_words
                    .get(index + 1)
                    .is_some_and(|word| word.unquoted_tilde || word.unquoted_glob)
                {
                    anyhow::bail!("OMP --cwd contains an opaque shell expansion");
                }
                let value = argv
                    .get(index + 1)
                    .filter(|value| !value.is_empty())
                    .context("OMP --cwd requires a directory")?;
                parsed.cwd = Some(PathBuf::from(value));
                index += 2;
                continue;
            }
            if let Some(value) = arg.strip_prefix("--cwd=") {
                if value.is_empty() {
                    anyhow::bail!("OMP --cwd requires a directory");
                }
                parsed.cwd = Some(PathBuf::from(value));
                index += 1;
                continue;
            }
            if arg == "--profile" {
                if shell_words
                    .get(index + 1)
                    .is_some_and(|word| word.unquoted_tilde || word.unquoted_glob)
                {
                    anyhow::bail!("OMP --profile contains an opaque shell expansion");
                }
                let value = argv
                    .get(index + 1)
                    .filter(|value| !value.is_empty() && !value.starts_with('-'))
                    .context("OMP --profile requires a profile name")?;
                parsed.profile = Some(value.clone());
                index += 2;
                continue;
            }
            if let Some(value) = arg.strip_prefix("--profile=") {
                if value.is_empty() {
                    anyhow::bail!("OMP --profile requires a profile name");
                }
                parsed.profile = Some(value.to_string());
                index += 1;
                continue;
            }
            if arg == "--session-dir" {
                if shell_words
                    .get(index + 1)
                    .is_some_and(|word| word.unquoted_tilde || word.unquoted_glob)
                {
                    anyhow::bail!("OMP --session-dir contains an opaque shell expansion");
                }
                let value = argv
                    .get(index + 1)
                    .filter(|value| !value.is_empty())
                    .context("OMP --session-dir requires a directory")?;
                parsed.session_dir = Some(PathBuf::from(value));
                index += 2;
                continue;
            }
            if let Some(value) = arg.strip_prefix("--session-dir=") {
                if value.is_empty() {
                    anyhow::bail!("OMP --session-dir requires a directory");
                }
                parsed.session_dir = Some(PathBuf::from(value));
                index += 1;
                continue;
            }
            let consumes_next =
                omp_flag_consumes_next(arg, argv.get(index + 1).map(String::as_str));
            if consumes_next
                && shell_words
                    .get(index + 1)
                    .is_some_and(|word| word.unquoted_glob)
                && argv
                    .get(index + 1)
                    .is_some_and(|value| !expansion_cannot_produce_flag(value))
            {
                anyhow::bail!("OMP extra_args contains an ambiguous shell expansion");
            }
            index += if consumes_next { 2 } else { 1 };
        }
        Ok(parsed)
    }
}

/// Reject credentials that would otherwise be persisted in the pane's start
/// command and exposed through process argv. OMP supports provider credentials
/// through environment variables, which AoE transports via its protected
/// one-shot channel.
pub(crate) fn reject_omp_secret_args(extra_args: &str) -> Result<()> {
    // Reject shell expansions before checking literal argv. Otherwise an option
    // such as `--api-key$EMPTY` becomes `--api-key` only after the launch shell
    // has already bypassed this credential boundary.
    inspect_shell_syntax(extra_args)?;
    let argv = shell_words::split(extra_args).context("Invalid OMP extra_args quoting")?;
    anyhow::ensure!(
        !argv
            .iter()
            .any(|arg| arg == "--api-key" || arg.starts_with("--api-key=")),
        "OMP --api-key is not allowed in extra_args; configure the provider API key through the environment"
    );
    Ok(())
}

/// Skip a non-store flag and, when present, its value so tokenization never
/// mistakes a flag argument for a positional. Deliberately fail-open: store
/// attribution (`--profile`, `--session-dir`, `--cwd`, `--no-session`) and the
/// `--api-key` secret are matched explicitly in `OmpCliCaptureOptions::parse`
/// and `reject_omp_secret_args` before this heuristic runs, and all of them
/// begin with `-`, so the `!next.starts_with('-')` guard keeps an unknown flag
/// from ever swallowing one. A wrong skip can only drop a benign positional,
/// never mis-select a store. The lists mirror OMP 17.2.10's CLI surface; when a
/// store-affecting flag is added upstream, extend `OmpCliCaptureOptions::parse`,
/// not this allowlist. Do not make the tail fail-closed: it would reject benign
/// launches carrying new OMP flags without adding any store safety.
fn omp_flag_consumes_next(flag: &str, next: Option<&str>) -> bool {
    const STRING_FLAGS: &[&str] = &[
        "--config",
        "--add-dir",
        "--mode",
        "--fork",
        "--provider",
        "--model",
        "--smol",
        "--slow",
        "--prewalk-into",
        "--plan-yolo-into",
        "--max-time",
        "--service-tier",
        "--api-key",
        "--system-prompt",
        "--append-system-prompt",
        "--provider-session-id",
        "--prompt-cache-key",
        "--models",
        "--tools",
        "--thinking",
        "--export",
        "--hook",
        "--extension",
        "-e",
        "--plugin-dir",
        "--skills",
        "--approval-mode",
        "--trusted-extension",
    ];
    const VALUELESS_FLAGS: &[&str] = &[
        "--help",
        "--version",
        "--allow-home",
        "-c",
        "--continue",
        "--from-claude",
        "--from-codex",
        "--no-tools",
        "--no-lsp",
        "--no-pty",
        "--hide-thinking",
        "--advisor",
        "--prewalk",
        "--no-prewalk",
        "--plan-yolo",
        "--print",
        "--print-thoughts",
        "--no-extensions",
        "--no-skills",
        "--no-rules",
        "--no-title",
        "--auto-approve",
        "--yolo",
    ];
    let Some(next) = next else {
        return false;
    };
    if flag == "--plan" || matches!(flag, "--resume" | "-r" | "--session") {
        return !next.starts_with('-') && !next.is_empty();
    }
    if STRING_FLAGS.contains(&flag) {
        return true;
    }
    flag.starts_with("--")
        && !flag.contains('=')
        && !VALUELESS_FLAGS.contains(&flag)
        && !next.starts_with('-')
}

#[derive(Default)]
struct ShellWordInspection {
    unquoted_tilde: bool,
    unquoted_glob: bool,
}

fn inspect_shell_syntax(input: &str) -> Result<Vec<ShellWordInspection>> {
    let mut quote = None;
    let mut escaped = false;
    let mut in_word = false;
    let mut word = ShellWordInspection::default();
    let mut words = Vec::new();
    for byte in input.bytes() {
        if escaped {
            escaped = false;
            in_word = true;
            continue;
        }
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                }
            }
            Some(b'"') => {
                if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quote = None;
                } else if matches!(byte, b'$' | b'`') {
                    anyhow::bail!("OMP extra_args contains opaque shell syntax");
                }
            }
            // quote only ever holds None/Some('\'')/Some('"') today; fail closed
            // rather than panic if a future delimiter is added to the None arm,
            // since this parses user-supplied extra_args.
            Some(_) => anyhow::bail!("OMP extra_args contains opaque shell syntax"),
            None => match byte {
                b' ' | b'\t' => {
                    if in_word {
                        words.push(word);
                        in_word = false;
                        word = ShellWordInspection::default();
                    }
                }
                b'\\' => {
                    escaped = true;
                    in_word = true;
                }
                b'\'' | b'"' => {
                    quote = Some(byte);
                    in_word = true;
                }
                b'~' if !in_word => {
                    word.unquoted_tilde = true;
                    in_word = true;
                }
                b'*' | b'?' | b'[' | b'{' | b'}' => {
                    word.unquoted_glob = true;
                    in_word = true;
                }
                b'$' | b'`' | b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b')' | b'#' | b'\n'
                | b'\r' => anyhow::bail!("OMP extra_args contains opaque shell syntax"),
                _ => in_word = true,
            },
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

fn expansion_cannot_produce_flag(word: &str) -> bool {
    word.as_bytes()
        .first()
        .is_some_and(|byte| !matches!(byte, b'-' | b'*' | b'?' | b'[' | b'{' | b'}'))
}
/// Resolve OMP 17.2.12's host store. Bun's cwd dotenv autoload is applied
/// before profile selection, then OMP's four literal dotenv files are merged.
pub(crate) fn resolve_omp_store_layout(
    environment: &[String],
    launch_cwd: &str,
    options: &OmpCliCaptureOptions,
) -> Result<OmpStoreLayout> {
    resolve_omp_store_layout_with_environment(environment, launch_cwd, options)
        .map(|(layout, _)| layout)
}

/// Resolve the store and fingerprint the pre-dotenv launcher routing. The
/// fingerprint lets the pane reject capture if login startup files change
/// routing after this snapshot, without carrying any routing value through
/// argv or tmux metadata.
pub(crate) fn resolve_omp_store_layout_with_environment(
    environment: &[String],
    launch_cwd: &str,
    options: &OmpCliCaptureOptions,
) -> Result<(OmpStoreLayout, String)> {
    let cwd = absolute_launch_cwd(launch_cwd)?;
    let launcher_env = host_launcher_environment(environment);
    let routing_fingerprint = routing_fingerprint(&launcher_env);
    let auto_env = autoload_bun_dotenv(launcher_env, &cwd, read_dotenv_content)?;
    let profile = resolve_profile(options.profile.as_deref(), &auto_env)?;
    let locations = dotenv_locations(&auto_env, &cwd, profile.as_deref())?;
    let files = locations
        .iter()
        .map(|path| read_dotenv_file(path))
        .collect::<Result<Vec<_>>>()?;
    let merged = merge_omp_environment(auto_env, &files);
    let layout = resolve_layout(&merged, &cwd, profile.as_deref(), options, |path| {
        path.exists()
    })?;
    Ok((layout, routing_fingerprint))
}

pub(crate) fn resolve_omp_store_layout_in_container_with_environment(
    container_name: &str,
    container_cwd: &str,
    launch_environment: &[(String, String)],
    options: &OmpCliCaptureOptions,
) -> Result<(OmpStoreLayout, String)> {
    let cwd = absolute_launch_cwd(container_cwd)?;
    let mut launcher_env = read_container_environment(container_name)?;
    for (key, value) in launch_environment {
        launcher_env.insert(key.clone(), value.clone());
    }
    let routing_fingerprint = routing_fingerprint(&launcher_env);
    if nonempty(&launcher_env, "HOME").is_none() {
        anyhow::bail!("OMP container has no HOME");
    }
    let auto_env = autoload_bun_dotenv(launcher_env, &cwd, |path| {
        read_container_dotenv_content(container_name, path)
    })?;
    let profile = resolve_profile(options.profile.as_deref(), &auto_env)?;
    let locations = dotenv_locations(&auto_env, &cwd, profile.as_deref())?;
    let files = locations
        .iter()
        .map(|path| read_container_dotenv(container_name, path))
        .collect::<Result<Vec<_>>>()?;
    let merged = merge_omp_environment(auto_env, &files);

    let agent_dir = managed_agent_dir(&merged, &cwd, profile.as_deref())?;
    let data_candidate = xdg_candidate(&merged, &cwd, "XDG_DATA_HOME", profile.as_deref());
    let state_candidate = xdg_candidate(&merged, &cwd, "XDG_STATE_HOME", profile.as_deref());
    let existence = probe_container_paths(
        container_name,
        [data_candidate.as_deref(), state_candidate.as_deref()],
    )?;
    let managed_sessions = existence[0]
        .then_some(data_candidate)
        .flatten()
        .unwrap_or_else(|| agent_dir.clone())
        .join("sessions");
    let session_cwd = omp_session_cwd(&cwd, options);
    let custom = options
        .session_dir
        .as_deref()
        .or_else(|| nonempty(&merged, "PI_CODING_AGENT_SESSION_DIR").map(Path::new));
    let layout = OmpStoreLayout {
        sessions: custom.map_or_else(
            || managed_sessions.clone(),
            |path| absolute_path(&session_cwd, path),
        ),
        managed_sessions,
        terminal_sessions: existence[1]
            .then_some(state_candidate)
            .flatten()
            .unwrap_or(agent_dir)
            .join("terminal-sessions"),
        kind: if custom.is_some() {
            OmpStoreKind::Custom
        } else {
            OmpStoreKind::Managed
        },
    };
    Ok((layout, routing_fingerprint))
}

fn resolve_layout(
    env: &HashMap<String, String>,
    cwd: &Path,
    profile: Option<&str>,
    options: &OmpCliCaptureOptions,
    mut exists: impl FnMut(&Path) -> bool,
) -> Result<OmpStoreLayout> {
    let session_cwd = omp_session_cwd(cwd, options);
    let agent_dir = managed_agent_dir(env, cwd, profile)?;
    let managed_sessions = xdg_candidate(env, cwd, "XDG_DATA_HOME", profile)
        .filter(|path| exists(path))
        .unwrap_or_else(|| agent_dir.clone())
        .join("sessions");
    let terminal_sessions = xdg_candidate(env, cwd, "XDG_STATE_HOME", profile)
        .filter(|path| exists(path))
        .unwrap_or(agent_dir)
        .join("terminal-sessions");
    let custom = options
        .session_dir
        .as_deref()
        .or_else(|| nonempty(env, "PI_CODING_AGENT_SESSION_DIR").map(Path::new));
    Ok(OmpStoreLayout {
        sessions: custom.map_or_else(
            || managed_sessions.clone(),
            |path| absolute_path(&session_cwd, path),
        ),
        managed_sessions,
        terminal_sessions,
        kind: if custom.is_some() {
            OmpStoreKind::Custom
        } else {
            OmpStoreKind::Managed
        },
    })
}

fn resolve_profile(
    cli_profile: Option<&str>,
    env: &HashMap<String, String>,
) -> Result<Option<String>> {
    let raw = cli_profile
        .map(str::to_string)
        .or_else(|| env.get("OMP_PROFILE").cloned())
        .or_else(|| env.get("PI_PROFILE").cloned());
    normalize_profile(raw.as_deref())
}

fn normalize_profile(raw: Option<&str>) -> Result<Option<String>> {
    let normalized = raw.map(str::trim).unwrap_or_default();
    if normalized.is_empty() || normalized == "default" {
        return Ok(None);
    }
    let basename = normalized
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let windows_reserved = matches!(basename.as_str(), "con" | "prn" | "aux" | "nul")
        || basename
            .strip_prefix("com")
            .or_else(|| basename.strip_prefix("lpt"))
            .is_some_and(|suffix| suffix.len() == 1 && suffix.as_bytes()[0].is_ascii_digit());
    if normalized == "."
        || normalized == ".."
        || normalized.len() > 64
        || normalized.ends_with('.')
        || windows_reserved
        || !normalized.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
    {
        anyhow::bail!("Invalid OMP profile name");
    }
    Ok(Some(normalized.to_string()))
}

fn dotenv_locations(
    env: &HashMap<String, String>,
    cwd: &Path,
    profile: Option<&str>,
) -> Result<[PathBuf; 4]> {
    let home = home_dir(env, cwd)?;
    let config_root = config_root(env, cwd, &home, profile);
    let agent_dir = initial_agent_dir(env, cwd, &config_root, profile);
    Ok([
        cwd.join(".env"),
        agent_dir.join(".env"),
        config_root.join(".env"),
        home.join(".env"),
    ])
}

fn host_launcher_environment(entries: &[String]) -> HashMap<String, String> {
    let mut values = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect::<HashMap<_, _>>();
    for entry in entries {
        let key = entry.split_once('=').map_or(entry.as_str(), |(key, _)| key);
        if !crate::session::environment::is_valid_env_key(key) {
            continue;
        }
        if let Some(value) =
            crate::session::environment::resolve_host_environment_value(entries, key)
        {
            values.insert(key.to_string(), value);
        }
    }
    // Preserve absence separately from an explicit empty value. OMP_PROFILE
    // only falls back to PI_PROFILE when it is absent, while an empty value
    // explicitly selects the default profile.
    values
}

/// Routing mutations that must be applied in a host pane so the resolver and
/// launched OMP process start from the same environment snapshot. An explicit
/// unset prevents tmux's long-lived server environment from reviving a stale
/// routing value.
pub(crate) fn omp_host_routing_environment(
    entries: &[String],
) -> Vec<crate::tmux::PaneEnvMutation> {
    let values = host_launcher_environment(entries);
    OMP_STORE_ENV_KEYS
        .iter()
        .map(|key| match values.get(*key) {
            Some(value) => crate::tmux::PaneEnvMutation::set((*key).to_string(), value.clone()),
            None => crate::tmux::PaneEnvMutation::unset((*key).to_string()),
        })
        .collect()
}

fn autoload_bun_dotenv(
    mut env: HashMap<String, String>,
    cwd: &Path,
    mut read_content: impl FnMut(&Path) -> Result<Option<String>>,
) -> Result<HashMap<String, String>> {
    let protected = env
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, _)| key.clone())
        .collect::<HashSet<_>>();
    let mode = nonempty(&env, "NODE_ENV").unwrap_or("development");
    let paths = [
        cwd.join(".env"),
        cwd.join(format!(".env.{mode}")),
        cwd.join(".env.local"),
    ];
    for path in paths {
        if let Some(content) = read_content(&path)? {
            apply_bun_dotenv(&content, &mut env, &protected)?;
        }
    }
    Ok(env)
}

fn routing_fingerprint(env: &HashMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    for key in OMP_STORE_ENV_KEYS {
        hasher.update(key.as_bytes());
        hasher.update([0]);
        match env.get(key) {
            Some(value) => {
                hasher.update(b"1");
                hasher.update([0]);
                hasher.update(value.as_bytes());
            }
            None => {
                hasher.update(b"0");
                hasher.update([0]);
            }
        }
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing SHA-256 to String cannot fail");
    }
    encoded
}

fn merge_omp_environment(
    mut exec_env: HashMap<String, String>,
    files_high_to_low: &[HashMap<String, String>],
) -> HashMap<String, String> {
    for file in files_high_to_low {
        for (key, value) in file {
            if exec_env.get(key).is_none_or(String::is_empty) {
                exec_env.insert(key.clone(), value.clone());
            }
        }
    }
    exec_env
}

fn read_dotenv_content(path: &Path) -> Result<Option<String>> {
    let Some(file) = open_regular_file_no_follow(path)? else {
        return Ok(None);
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect dotenv {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "dotenv {} is not a regular file",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_DOTENV_BYTES as u64,
        "dotenv {} exceeds the {} byte capture limit",
        path.display(),
        MAX_DOTENV_BYTES
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_DOTENV_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read dotenv {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_DOTENV_BYTES,
        "dotenv {} grew beyond the {} byte capture limit",
        path.display(),
        MAX_DOTENV_BYTES
    );
    Ok(Some(String::from_utf8(bytes).with_context(|| {
        format!("dotenv {} is not UTF-8", path.display())
    })?))
}

#[cfg(unix)]
fn open_regular_file_no_follow(path: &Path) -> Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;

    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to safely open {}", path.display()))
        }
    }
}

#[cfg(not(unix))]
fn open_regular_file_no_follow(path: &Path) -> Result<Option<std::fs::File>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink() && metadata.is_file(),
                "{} is not a safe regular file",
                path.display()
            );
            Ok(Some(std::fs::File::open(path).with_context(|| {
                format!("failed to safely open {}", path.display())
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn read_dotenv_file(path: &Path) -> Result<HashMap<String, String>> {
    Ok(read_dotenv_content(path)?
        .map(|content| parse_dotenv(&content))
        .unwrap_or_default())
}

fn parse_dotenv_line(line: &str) -> Option<(&str, String, bool)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (raw_key, raw_value) = trimmed.split_once('=')?;
    let raw_key = raw_key.trim();
    let key = raw_key
        .strip_prefix("export")
        .filter(|rest| rest.starts_with(' ') || rest.starts_with('\t'))
        .unwrap_or(raw_key)
        .trim();
    if !crate::session::environment::is_valid_env_key(key) {
        return None;
    }
    let raw_value = raw_value.trim_start_matches([' ', '\t']);
    let (value, expand) = match raw_value.as_bytes().first().copied() {
        Some(quote @ (b'\'' | b'"' | b'`')) => {
            let rest = &raw_value[1..];
            let end = rest
                .bytes()
                .enumerate()
                .find(|(index, byte)| {
                    *byte == quote && (*index == 0 || rest.as_bytes()[*index - 1] != b'\\')
                })
                .map(|(index, _)| index)
                .unwrap_or(rest.len());
            (rest[..end].to_string(), true)
        }
        _ => {
            let end = raw_value
                .as_bytes()
                .windows(2)
                .position(|pair| (pair[0] == b' ' || pair[0] == b'\t') && pair[1] == b'#')
                .unwrap_or(raw_value.len());
            (raw_value[..end].trim_end().to_string(), true)
        }
    };
    (!value.contains('\0')).then_some((key, value, expand))
}

fn parse_dotenv(content: &str) -> HashMap<String, String> {
    let mut values = content
        .lines()
        .filter_map(parse_dotenv_line)
        .map(|(key, value, _)| (key.to_string(), value))
        .collect::<HashMap<_, _>>();
    let mirrors = values
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix("OMP_")
                .map(|suffix| (format!("PI_{suffix}"), value.clone()))
        })
        .collect::<Vec<_>>();
    values.extend(mirrors);
    values
}

fn apply_bun_dotenv(
    content: &str,
    env: &mut HashMap<String, String>,
    protected: &HashSet<String>,
) -> Result<()> {
    for (key, value, expand) in content.lines().filter_map(parse_dotenv_line) {
        if protected.contains(key) {
            continue;
        }
        if expand && is_omp_routing_assignment(key) && has_nonrouting_reference(&value) {
            anyhow::bail!(
                "OMP capture cannot safely resolve dotenv routing key {key} from non-routing variables"
            );
        }
        let value = if expand {
            expand_dotenv_value(&value, env)
        } else {
            value
        };
        env.insert(key.to_string(), value);
    }
    Ok(())
}

fn is_omp_routing_assignment(key: &str) -> bool {
    OMP_STORE_ENV_KEYS.contains(&key)
        || key.strip_prefix("OMP_").is_some_and(|suffix| {
            OMP_STORE_ENV_KEYS.iter().any(|candidate| {
                candidate
                    .strip_prefix("PI_")
                    .is_some_and(|candidate| candidate == suffix)
            })
        })
}

/// One lexical token of a dotenv value under the shared `\$` / `$VAR` /
/// `${VAR}` grammar. `has_nonrouting_reference` and `expand_dotenv_value` walk
/// this single stream so their reference detection can never drift apart.
enum DotenvToken<'a> {
    /// Verbatim bytes: ordinary text, a dangling `$`, a `$<digit>` start, or an
    /// unterminated / invalid-key `${...}`.
    Literal(&'a str),
    /// A `\$` escape, expanding to a single `$`.
    EscapedDollar,
    /// A `$KEY` or `${KEY}` reference whose key is a valid env variable name.
    Reference(&'a str),
}

/// Scan a dotenv value into `$`-reference tokens. Callers decide what each
/// token means; the lexing is identical for detection and expansion.
fn dotenv_tokens(value: &str) -> impl Iterator<Item = DotenvToken<'_>> {
    let bytes = value.as_bytes();
    let mut index = 0;
    std::iter::from_fn(move || {
        let start = index;
        if start >= bytes.len() {
            return None;
        }
        if bytes[start] == b'\\' && bytes.get(start + 1) == Some(&b'$') {
            index = start + 2;
            return Some(DotenvToken::EscapedDollar);
        }
        if bytes[start] == b'$' {
            if bytes.get(start + 1) == Some(&b'{') {
                let Some(relative_end) = value[start + 2..].find('}') else {
                    // Unterminated `${`: emit the lone `$`, rescan from `{`.
                    index = start + 1;
                    return Some(DotenvToken::Literal(&value[start..start + 1]));
                };
                let end = start + 2 + relative_end;
                let key = &value[start + 2..end];
                index = end + 1;
                return Some(if crate::session::environment::is_valid_env_key(key) {
                    DotenvToken::Reference(key)
                } else {
                    DotenvToken::Literal(&value[start..=end])
                });
            }
            let key_start = start + 1;
            let mut end = key_start;
            while bytes
                .get(end)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                end += 1;
            }
            if end > key_start && !bytes[key_start].is_ascii_digit() {
                index = end;
                return Some(DotenvToken::Reference(&value[key_start..end]));
            }
            // Lone `$` or `$<digit>...`: literal dollar, rescan the remainder.
            index = start + 1;
            return Some(DotenvToken::Literal(&value[start..start + 1]));
        }
        // Ordinary run up to the next `$` or `\$`. Both begin with an ASCII
        // byte, so `end` never lands inside a multi-byte character.
        let mut end = start;
        while end < bytes.len()
            && bytes[end] != b'$'
            && !(bytes[end] == b'\\' && bytes.get(end + 1) == Some(&b'$'))
        {
            end += 1;
        }
        index = end;
        Some(DotenvToken::Literal(&value[start..end]))
    })
}

fn has_nonrouting_reference(value: &str) -> bool {
    dotenv_tokens(value).any(
        |token| matches!(token, DotenvToken::Reference(key) if !OMP_STORE_ENV_KEYS.contains(&key)),
    )
}

fn expand_dotenv_value(value: &str, env: &HashMap<String, String>) -> String {
    let mut expanded = String::with_capacity(value.len());
    for token in dotenv_tokens(value) {
        match token {
            DotenvToken::Literal(text) => expanded.push_str(text),
            DotenvToken::EscapedDollar => expanded.push('$'),
            DotenvToken::Reference(key) => {
                if let Some(replacement) = env.get(key) {
                    expanded.push_str(replacement);
                }
            }
        }
    }
    expanded
}

fn home_dir(env: &HashMap<String, String>, cwd: &Path) -> Result<PathBuf> {
    if let Some(home) = nonempty(env, "HOME") {
        return Ok(absolute_path(cwd, Path::new(home)));
    }
    dirs::home_dir()
        .map(|home| absolute_path(cwd, &home))
        .context("Cannot determine home directory")
}

fn config_root(
    env: &HashMap<String, String>,
    cwd: &Path,
    home: &Path,
    profile: Option<&str>,
) -> PathBuf {
    let config = nonempty(env, "PI_CONFIG_DIR").unwrap_or(".omp");
    // Strip only an absolute prefix/root so PI_CONFIG_DIR re-roots under HOME,
    // matching OMP's own join(home, config). ParentDir (`..`) is kept on
    // purpose: OMP joins the value literally, so filtering it here would resolve
    // to a different store than OMP uses and misattribute the session. The
    // captured id is still gated downstream by validate_breadcrumb's store-shape
    // and cwd checks, so mirroring OMP's view is the safe choice.
    let relative: PathBuf = Path::new(config)
        .components()
        .filter(|component| !matches!(component, Component::Prefix(_) | Component::RootDir))
        .collect();
    let root = absolute_path(cwd, &home.join(relative));
    profile.map_or(root.clone(), |profile| root.join("profiles").join(profile))
}

fn initial_agent_dir(
    env: &HashMap<String, String>,
    cwd: &Path,
    config_root: &Path,
    profile: Option<&str>,
) -> PathBuf {
    if profile.is_none() {
        if let Some(agent) = nonempty(env, "PI_CODING_AGENT_DIR") {
            let inherited_profile = env
                .get("PI_PROFILE")
                .and_then(|value| normalize_profile(Some(value)).ok().flatten());
            let profile_derived = inherited_profile.is_some_and(|profile| {
                Path::new(agent)
                    == config_root
                        .join("profiles")
                        .join(profile)
                        .join("agent")
                        .as_path()
            });
            if !profile_derived {
                return absolute_path(cwd, Path::new(agent));
            }
        }
    }
    config_root.join("agent")
}

fn absolute_launch_cwd(cwd: &str) -> Result<PathBuf> {
    let cwd = Path::new(cwd);
    if !cwd.is_absolute() {
        anyhow::bail!("OMP launch cwd is not absolute");
    }
    Ok(crate::git::template::lexical_normalize(cwd))
}
fn omp_session_cwd(launch_cwd: &Path, options: &OmpCliCaptureOptions) -> PathBuf {
    options.cwd.as_deref().map_or_else(
        || launch_cwd.to_path_buf(),
        |cwd| absolute_path(launch_cwd, cwd),
    )
}

fn managed_agent_dir(
    env: &HashMap<String, String>,
    cwd: &Path,
    profile: Option<&str>,
) -> Result<PathBuf> {
    let home = home_dir(env, cwd)?;
    let root = config_root(env, cwd, &home, profile);
    Ok(initial_agent_dir(env, cwd, &root, profile))
}

fn xdg_candidate(
    env: &HashMap<String, String>,
    cwd: &Path,
    key: &str,
    profile: Option<&str>,
) -> Option<PathBuf> {
    let home = home_dir(env, cwd).ok()?;
    let root = config_root(env, cwd, &home, profile);
    let default_agent = root.join("agent");
    if initial_agent_dir(env, cwd, &root, profile) != default_agent {
        return None;
    }
    nonempty(env, key).map(|value| {
        let root = absolute_path(cwd, Path::new(value)).join("omp");
        profile.map_or(root.clone(), |profile| root.join("profiles").join(profile))
    })
}

fn nonempty<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    crate::git::template::lexical_normalize(&path)
}

fn container_exec_command(
    container_name: &str,
    runtime_name: Option<crate::session::config::ContainerRuntimeName>,
    argv: &[&str],
) -> std::process::Command {
    use crate::session::config::ContainerRuntimeName;

    let runtime = match runtime_name {
        Some(ContainerRuntimeName::AppleContainer) => {
            crate::containers::ContainerRuntime::apple_container()
        }
        Some(ContainerRuntimeName::Docker) => crate::containers::ContainerRuntime::docker(),
        Some(ContainerRuntimeName::Podman) => crate::containers::ContainerRuntime::podman(),
        None => crate::containers::get_container_runtime(),
    };
    let command_argv = argv
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    let exec_argv = runtime.build_exec_argv(container_name, "", &command_argv);
    let mut command = std::process::Command::new(&exec_argv[0]);
    command.args(&exec_argv[1..]);
    command
}

fn read_container_environment(container_name: &str) -> Result<HashMap<String, String>> {
    let command = container_exec_command(container_name, None, &["env"]);
    let output = super::run_with_timeout_limit(
        command,
        COMMAND_TIMEOUT,
        "container exec (OMP env probe)",
        MAX_CONTAINER_ENV_BYTES,
    )?;
    let text = String::from_utf8_lossy(&output);
    let mut values = HashMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if crate::session::environment::is_valid_env_key(key) {
            values.insert(key.to_string(), value.to_string());
        }
    }
    Ok(values)
}

fn read_container_dotenv_content(container_name: &str, path: &Path) -> Result<Option<String>> {
    // TOCTOU accepted, not hardened: a POSIX shell cannot open with O_NOFOLLOW,
    // so the `[ -L ]`/`[ -f ]` pre-checks cannot be made atomic with the `dd`
    // read. The probe runs inside a container the user already fully controls,
    // the output is size-capped and parsed only for routing env keys, and no
    // host privilege boundary is crossed; the worst case is routing-key
    // confusion within that same container. The host reader uses O_NOFOLLOW
    // because it can.
    const SCRIPT: &str = r#"if [ -L "$1" ]; then
  printf 'unsafe\n'
elif [ ! -e "$1" ]; then
  printf 'missing\n'
elif [ ! -f "$1" ]; then
  printf 'unsafe\n'
else
  printf 'file\n'
  dd if="$1" bs=1048577 count=1 2>/dev/null
fi"#;
    let path = path
        .to_str()
        .context("OMP container dotenv path is not UTF-8")?;
    let command = container_exec_command(
        container_name,
        None,
        &["sh", "-c", SCRIPT, "aoe-omp-dotenv", path],
    );
    let output = super::run_with_timeout_limit(
        command,
        COMMAND_TIMEOUT,
        "container exec (OMP dotenv probe)",
        MAX_DOTENV_BYTES + 32,
    )?;
    let separator = output
        .iter()
        .position(|byte| *byte == b'\n')
        .context("OMP container dotenv probe returned no status")?;
    let status = &output[..separator];
    let content = &output[separator + 1..];
    match status {
        b"missing" => {
            anyhow::ensure!(
                content.is_empty(),
                "OMP container dotenv probe returned trailing missing-file data"
            );
            Ok(None)
        }
        b"unsafe" => anyhow::bail!("OMP container dotenv path is not a safe regular file"),
        b"file" => {
            anyhow::ensure!(
                content.len() <= MAX_DOTENV_BYTES,
                "OMP container dotenv exceeds its capture limit"
            );
            Ok(Some(
                String::from_utf8(content.to_vec()).context("OMP container dotenv is not UTF-8")?,
            ))
        }
        _ => anyhow::bail!("OMP container dotenv probe returned an invalid status"),
    }
}

fn read_container_dotenv(container_name: &str, path: &Path) -> Result<HashMap<String, String>> {
    Ok(read_container_dotenv_content(container_name, path)?
        .map(|content| parse_dotenv(&content))
        .unwrap_or_default())
}

fn probe_container_paths(container_name: &str, paths: [Option<&Path>; 2]) -> Result<[bool; 2]> {
    const SCRIPT: &str = r#"for path do
  if [ -n "$path" ] && [ -e "$path" ]; then printf '1\n'; else printf '0\n'; fi
done"#;
    let path_values = paths.map(|path| path.and_then(Path::to_str).unwrap_or_default().to_string());
    let command = container_exec_command(
        container_name,
        None,
        &[
            "sh",
            "-c",
            SCRIPT,
            "aoe-omp-paths",
            &path_values[0],
            &path_values[1],
        ],
    );
    let output = super::run_with_timeout_limit(
        command,
        COMMAND_TIMEOUT,
        "container exec (OMP path probe)",
        MAX_CONTAINER_PROBE_BYTES,
    )?;
    let text = String::from_utf8(output).context("OMP container path probe is not UTF-8")?;
    let mut lines = text.lines();
    let result = [lines.next() == Some("1"), lines.next() == Some("1")];
    if lines.next().is_some() {
        anyhow::bail!("OMP container path probe returned trailing data");
    }
    Ok(result)
}

/// Return the sandbox marker path for an instance. The marker is created
/// immediately before OMP exec and is not a session id or a resumable artifact.
///
/// The path is derived from `instance_id` alone, so relaunches of the same
/// instance REUSE it, and it is never unlinked. Reuse is safe because a marker
/// left by a superseded launch fails closed: both `validate_launch_marker` and
/// `CONTAINER_BREADCRUMB_SCRIPT` reject any marker whose launch id or routing
/// fingerprint differs from the live tmux generation, and
/// `load_omp_capture_metadata` trusts a marked generation only after the parent
/// publishes its READY key. Do not weaken those checks to a path/existence test.
pub(crate) fn omp_sandbox_launch_marker(instance_id: &str) -> String {
    let safe = instance_id
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        .map(char::from)
        .collect::<String>();
    format!("/tmp/aoe-omp-launch-{safe}")
}

fn valid_omp_terminal_id(terminal_id: &str) -> bool {
    !terminal_id.is_empty()
        && !matches!(terminal_id, "." | "..")
        && terminal_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn omp_terminal_id_from_tty(tty: &str) -> Option<String> {
    let device = tty.strip_prefix("/dev/")?;
    let terminal_id = device.replace('/', "-");
    valid_omp_terminal_id(&terminal_id).then_some(terminal_id)
}

fn tty_and_terminal_id_for_tmux(tmux_session_name: &str) -> Result<(String, String)> {
    let tty = crate::tmux::Session::from_name(tmux_session_name).pane_tty()?;
    let terminal_id = omp_terminal_id_from_tty(&tty)
        .ok_or_else(|| anyhow::anyhow!("Unsupported OMP pane TTY: {tty:?}"))?;
    Ok((tty, terminal_id))
}

fn load_omp_capture_metadata(tmux_session_name: &str) -> Result<OmpCaptureMetadata> {
    let key = crate::tmux::env::AOE_OMP_CAPTURE_META_KEY;
    let output = crate::tmux::tmux_command()
        .args(["show-environment", "-h", "-t", tmux_session_name, key])
        .output()
        .context("Failed to read OMP capture metadata from tmux")?;
    if !output.status.success() {
        anyhow::bail!("OMP capture metadata is unavailable in tmux");
    }
    let encoded = String::from_utf8(output.stdout).context("OMP capture metadata is not UTF-8")?;
    let encoded = encoded
        .strip_suffix("\r\n")
        .or_else(|| encoded.strip_suffix('\n'))
        .context("tmux returned unterminated OMP capture metadata")?;
    let encoded = encoded
        .strip_prefix(key)
        .and_then(|value| value.strip_prefix('='))
        .context("tmux returned malformed OMP capture metadata")?;
    if encoded.contains('\r') || encoded.contains('\n') {
        anyhow::bail!("tmux returned trailing OMP capture metadata");
    }
    let metadata: OmpCaptureMetadata =
        serde_json::from_str(encoded).context("tmux returned invalid OMP capture metadata")?;
    validate_omp_capture_metadata(&metadata)?;
    if !metadata.launch_marker.is_empty() {
        let ready = crate::tmux::env::get_hidden_env_uncached(
            tmux_session_name,
            crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
        );
        anyhow::ensure!(
            ready.as_deref() == Some(metadata.launch_id.as_str()),
            "OMP capture metadata generation is not ready"
        );
    }
    Ok(metadata)
}

pub(crate) fn validate_omp_capture_metadata(metadata: &OmpCaptureMetadata) -> Result<()> {
    validate_layout(&metadata.layout)?;
    if metadata.launched_at_ms == 0
        || metadata.launch_id.is_empty()
        || metadata.launch_id.contains('\r')
        || metadata.launch_id.contains('\n')
        || (!metadata.launch_marker.is_empty()
            && (!Path::new(&metadata.launch_marker).is_absolute()
                || metadata.launch_marker.contains('\r')
                || metadata.launch_marker.contains('\n')
                || metadata.routing_fingerprint.len() != 64
                || !metadata
                    .routing_fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())))
    {
        anyhow::bail!("OMP capture metadata has an invalid generation");
    }
    Ok(())
}

#[derive(Debug)]
struct Breadcrumb<'a> {
    cwd: &'a str,
    session_path: &'a str,
    fresh: bool,
}

fn parse_breadcrumb(content: &str) -> Result<Breadcrumb<'_>> {
    let mut lines = content.lines();
    let cwd = lines
        .next()
        .filter(|value| !value.is_empty())
        .context("OMP terminal breadcrumb has no cwd")?;
    let session_path = lines
        .next()
        .filter(|value| !value.is_empty())
        .context("OMP terminal breadcrumb has no session path")?;
    let fresh = match lines.next() {
        None => false,
        Some("fresh") => true,
        Some(_) => anyhow::bail!("OMP terminal breadcrumb has an invalid marker"),
    };
    if lines.next().is_some() {
        anyhow::bail!("OMP terminal breadcrumb has unexpected trailing data");
    }
    Ok(Breadcrumb {
        cwd,
        session_path,
        fresh,
    })
}

fn validate_layout(layout: &OmpStoreLayout) -> Result<()> {
    if !layout.sessions.is_absolute()
        || !layout.managed_sessions.is_absolute()
        || !layout.terminal_sessions.is_absolute()
    {
        anyhow::bail!("OMP capture layout roots must be absolute");
    }
    Ok(())
}

/// A path has store shape when it sits exactly `components` levels under
/// `root` (managed = 2, custom = 1). This invariant is mirrored, by necessity,
/// in `CONTAINER_BREADCRUMB_SCRIPT` as `case "$relative" in */*/*) ;; */*)`;
/// the parity test `host_and_container_store_shape_verdicts_match` locks the
/// two together. Change both, or the sandbox and host capture will diverge.
fn has_store_shape(path: &Path, root: &Path, components: usize) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|relative| relative.components().count() == components)
}

/// Resolve a breadcrumb's session path and reject anything that does not sit at
/// the store's session-file depth, without opening it. The host capture path
/// calls this to gate a file open on the same lexical store shape the
/// in-container script checks before it reads a JSONL header, so a hostile
/// breadcrumb cannot steer an open at an out-of-store path.
fn lexical_store_session_path(
    layout: &OmpStoreLayout,
    breadcrumb: &Breadcrumb<'_>,
) -> Result<PathBuf> {
    validate_layout(layout)?;
    let raw_path = Path::new(breadcrumb.session_path);
    let session_path = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else if layout.kind == OmpStoreKind::Custom {
        absolute_path(Path::new(breadcrumb.cwd), raw_path)
    } else {
        anyhow::bail!("Managed OMP breadcrumb session path is not absolute");
    };
    let normalized_path = crate::git::template::lexical_normalize(&session_path);
    let normalized_active = crate::git::template::lexical_normalize(&layout.sessions);
    let normalized_managed = crate::git::template::lexical_normalize(&layout.managed_sessions);
    let active_components = match layout.kind {
        OmpStoreKind::Managed => 2,
        OmpStoreKind::Custom => 1,
    };
    let valid_store = has_store_shape(&normalized_path, &normalized_active, active_components)
        || has_store_shape(&normalized_path, &normalized_managed, 2);
    if !valid_store
        || session_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("jsonl")
    {
        anyhow::bail!("OMP breadcrumb does not point to an allowed session JSONL");
    }
    Ok(session_path)
}

/// Canonicalize a materialized target and confirm it still resolves within the
/// store, run by the host BEFORE it opens the file. It executes after the
/// lexical gate so a directory symlink inside the store cannot redirect the
/// O_NOFOLLOW read to an out-of-store target; the in-container script performs
/// the equivalent realpath check before its own header read, so both engines
/// validate the canonical store before reading.
fn ensure_canonical_store(layout: &OmpStoreLayout, session_path: &Path) -> Result<()> {
    let active_components = match layout.kind {
        OmpStoreKind::Managed => 2,
        OmpStoreKind::Custom => 1,
    };
    let canonical_path = session_path
        .canonicalize()
        .context("Failed to canonicalize OMP session JSONL")?;
    let canonical_store = layout
        .sessions
        .canonicalize()
        .ok()
        .is_some_and(|root| has_store_shape(&canonical_path, &root, active_components))
        || layout
            .managed_sessions
            .canonicalize()
            .ok()
            .is_some_and(|root| has_store_shape(&canonical_path, &root, 2));
    anyhow::ensure!(
        canonical_store,
        "OMP breadcrumb resolves outside its allowed session store"
    );
    Ok(())
}

/// Validate an already-resolved breadcrumb target: reject an excluded id and
/// require a materialized header to match the breadcrumb. The caller resolves
/// the path with `lexical_store_session_path` (and, on the host, gates the open
/// with `ensure_canonical_store`), so this does no path resolution itself.
fn validate_breadcrumb(
    breadcrumb: Breadcrumb<'_>,
    session_path: &Path,
    materialized_header: Option<(Option<String>, Option<String>)>,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let session_id = super::extract_pi_uuid_from_filename(session_path)
        .context("OMP breadcrumb session filename has no UUID")?;
    if exclusion.contains(&session_id) {
        anyhow::bail!("OMP terminal breadcrumb session is excluded");
    }
    if let Some((header_id, header_cwd)) = materialized_header {
        if header_id.as_deref() != Some(session_id.as_str())
            || header_cwd
                .as_deref()
                .map(super::canonicalize_or_raw)
                .as_ref()
                != Some(&super::canonicalize_or_raw(breadcrumb.cwd))
        {
            anyhow::bail!("OMP session header does not match its terminal breadcrumb");
        }
    } else if breadcrumb.fresh {
        anyhow::bail!("fresh OMP breadcrumb target is not materialized");
    } else {
        anyhow::bail!("OMP breadcrumb target is missing");
    }
    Ok(session_id)
}

fn validate_launch_marker(
    metadata: &OmpCaptureMetadata,
    terminal_id: &str,
    session_path: &str,
) -> Result<bool> {
    if metadata.launch_marker.is_empty() {
        return Ok(false);
    }
    let marker_path = Path::new(&metadata.launch_marker);
    let Some(file) = open_regular_file_no_follow(marker_path)? else {
        anyhow::bail!("OMP launch marker is unavailable");
    };
    let mut content = String::new();
    file.take((MAX_LAUNCH_MARKER_BYTES + 1) as u64)
        .read_to_string(&mut content)
        .context("Failed to read OMP launch marker")?;
    anyhow::ensure!(
        content.len() <= MAX_LAUNCH_MARKER_BYTES,
        "OMP launch marker exceeds its capture limit"
    );
    let mut lines = content.lines();
    let terminal = lines.next();
    let launch = lines.next();
    let pending = lines.next().filter(|value| !value.is_empty());
    let routing_fingerprint = lines.next();
    if terminal != Some(terminal_id)
        || launch != Some(metadata.launch_id.as_str())
        || pending.is_none()
        || routing_fingerprint != Some(metadata.routing_fingerprint.as_str())
        || metadata.routing_fingerprint.is_empty()
        || lines.next().is_some()
    {
        anyhow::bail!("OMP launch marker does not match the active pane generation");
    }
    anyhow::ensure!(
        pending != Some(session_path),
        "OMP breadcrumb still has its pre-launch pending path"
    );
    Ok(true)
}

fn read_host_breadcrumb(root: &Path, terminal_id: &str) -> Result<(String, u64)> {
    let breadcrumb_path = root.join(terminal_id);
    #[cfg(unix)]
    let file = {
        use std::os::fd::AsFd;
        use std::os::unix::fs::OpenOptionsExt;

        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .with_context(|| {
                format!(
                    "Failed to open OMP terminal breadcrumb root {}",
                    root.display()
                )
            })?;
        let fd = nix::fcntl::openat(
            directory.as_fd(),
            terminal_id,
            nix::fcntl::OFlag::O_RDONLY
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_CLOEXEC
                | nix::fcntl::OFlag::O_NONBLOCK,
            nix::sys::stat::Mode::empty(),
        )
        .with_context(|| {
            format!(
                "Failed to open OMP breadcrumb {}",
                breadcrumb_path.display()
            )
        })?;
        std::fs::File::from(fd)
    };
    #[cfg(not(unix))]
    let mut file = {
        let metadata = std::fs::symlink_metadata(&breadcrumb_path).with_context(|| {
            format!(
                "Failed to inspect OMP breadcrumb {}",
                breadcrumb_path.display()
            )
        })?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "OMP breadcrumb is a symlink"
        );
        std::fs::File::open(&breadcrumb_path).with_context(|| {
            format!(
                "Failed to open OMP breadcrumb {}",
                breadcrumb_path.display()
            )
        })?
    };
    let metadata = file.metadata().with_context(|| {
        format!(
            "Failed to inspect OMP breadcrumb {}",
            breadcrumb_path.display()
        )
    })?;
    anyhow::ensure!(metadata.is_file(), "OMP breadcrumb is not a regular file");
    anyhow::ensure!(
        metadata.len() <= MAX_BREADCRUMB_BYTES as u64,
        "OMP breadcrumb exceeds its capture limit"
    );
    let modified_at_ms = metadata
        .modified()
        .context("Failed to read OMP breadcrumb modification time")?
        .duration_since(std::time::UNIX_EPOCH)
        .context("OMP breadcrumb modification time predates UNIX_EPOCH")
        .and_then(|elapsed| {
            u64::try_from(elapsed.as_millis())
                .context("OMP breadcrumb modification time does not fit in u64")
        })?;
    let mut content = String::new();
    file.take((MAX_BREADCRUMB_BYTES + 1) as u64)
        .read_to_string(&mut content)
        .with_context(|| {
            format!(
                "Failed to read OMP breadcrumb {}",
                breadcrumb_path.display()
            )
        })?;
    anyhow::ensure!(
        content.len() <= MAX_BREADCRUMB_BYTES,
        "OMP breadcrumb grew beyond its capture limit"
    );
    Ok((content, modified_at_ms))
}

fn capture_omp_session_id_from_terminal(
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    terminal_id: &str,
) -> Result<String> {
    validate_layout(&metadata.layout)?;
    if !valid_omp_terminal_id(terminal_id) {
        anyhow::bail!("Invalid OMP terminal id");
    }
    let (content, modified_at_ms) =
        read_host_breadcrumb(&metadata.layout.terminal_sessions, terminal_id)?;
    let breadcrumb = parse_breadcrumb(&content)?;
    // The marker proves the breadcrumb is no longer the pre-launch sentinel,
    // but not that THIS pane produced it after launch: a stale breadcrumb that
    // lands at the tty after launch also satisfies `pending != session_path`.
    // A post-launch write (freshness) is the only evidence of post-launch
    // authorship, so it is required unconditionally; the marker stays a
    // necessary, not sufficient, guard against adopting the sentinel itself.
    validate_launch_marker(metadata, terminal_id, breadcrumb.session_path)?;
    anyhow::ensure!(
        modified_at_ms > metadata.launched_at_ms,
        "unproven OMP breadcrumb predates the active pane"
    );
    // Resolve the target lexically, then canonicalize and re-check store
    // membership BEFORE opening it, matching the in-container script which
    // realpath-validates the store before reading a header. Gating the open
    // this way stops a hostile breadcrumb, including a directory symlink inside
    // the store, from steering the O_NOFOLLOW read at an out-of-store target.
    let session_path = lexical_store_session_path(&metadata.layout, &breadcrumb)?;
    let header = if session_path.is_file() {
        ensure_canonical_store(&metadata.layout, &session_path)?;
        Some(
            super::extract_pi_header_fields(&session_path)
                .context("OMP session JSONL has no valid session header")?,
        )
    } else {
        None
    };
    let session_id = validate_breadcrumb(breadcrumb, &session_path, header, exclusion)?;
    Ok(session_id)
}

/// Capture the OMP session owned by one exact host tmux pane.
pub(crate) fn capture_omp_session_id(
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    tmux_session_name: &str,
) -> Result<String> {
    let (_, terminal_id) = tty_and_terminal_id_for_tmux(tmux_session_name)?;
    capture_omp_session_id_from_terminal(metadata, exclusion, &terminal_id)
}

/// Pane identity resolved on each host poll tick. `tty` is compared only by the
/// end-of-tick equality guard: if the pane's TTY or its published metadata
/// generation changed while the breadcrumb was read, the observation belongs to
/// a superseded pane and is dropped. A same-generation breadcrumb rewrite is
/// deliberately not covered here; the on-disk launch-marker CAS is the authority
/// for that.
#[derive(PartialEq)]
struct OmpPollIdentity {
    metadata: OmpCaptureMetadata,
    tty: String,
    terminal_id: String,
}

fn resolve_omp_poll_identity(tmux_session_name: &str) -> Result<OmpPollIdentity> {
    let metadata = load_omp_capture_metadata(tmux_session_name)?;
    let (tty, terminal_id) = tty_and_terminal_id_for_tmux(tmux_session_name)?;
    Ok(OmpPollIdentity {
        metadata,
        tty,
        terminal_id,
    })
}

/// Host poller. Every tick follows the pane name resolved by the outer poller
/// and refreshes the metadata generation and TTY twice on that same name.
pub(crate) fn omp_poll_fn(
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn(&str) -> Option<crate::session::poller::SessionIdObservation> + Send + 'static {
    move |tmux_session_name| {
        let identity = resolve_omp_poll_identity(tmux_session_name)
            .map_err(|error| {
                tracing::debug!(target: "session.capture", "OMP poll identity refresh failed: {}", error)
            })
            .ok()?;
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes);
        let captured = capture_omp_session_id_from_terminal(
            &identity.metadata,
            &exclusion,
            &identity.terminal_id,
        )
        .map_err(|error| {
            tracing::debug!(target: "session.capture", "OMP poll capture failed: {}", error)
        })
        .ok()
        .and_then(super::validated_session_id);
        let refreshed = resolve_omp_poll_identity(tmux_session_name).ok()?;
        if refreshed != identity {
            return None;
        }
        captured.map(|sid| identity.metadata.session_observation(sid))
    }
}

const CONTAINER_BREADCRUMB_SCRIPT: &str = r#"TERM_DIR=$1
LAUNCH_MARKER=$2
EXPECTED_LAUNCH=$3
ACTIVE_ROOT=$4
MANAGED_ROOT=$5
STORE_KIND=$6
EXPECTED_FINGERPRINT=$7
[ -d "$TERM_DIR" ] && [ ! -L "$TERM_DIR" ] || exit 0
[ -f "$LAUNCH_MARKER" ] && [ ! -L "$LAUNCH_MARKER" ] || exit 0
marker_bytes=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | wc -c) || exit 0
[ "$marker_bytes" -le 17408 ] || exit 0
marker_lines=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | wc -l) || exit 0
terminal=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '1p')
marker_launch=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '2p')
marker_pending=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '3p')
marker_fingerprint=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '4p')
case "$terminal" in ''|.|..|*[!A-Za-z0-9._-]*) exit 0 ;; esac
[ -n "$EXPECTED_LAUNCH" ] && [ "$marker_launch" = "$EXPECTED_LAUNCH" ] || exit 0
[ -n "$EXPECTED_FINGERPRINT" ] && [ "$marker_fingerprint" = "$EXPECTED_FINGERPRINT" ] || exit 0
[ "$(( marker_lines + 0 ))" -eq 4 ] || exit 0
[ -n "$marker_pending" ] || exit 0
f="$TERM_DIR/$terminal"
[ -f "$f" ] && [ ! -L "$f" ] || exit 0
# Post-launch authorship proof, mirroring the host guard (capture reads
# modified_at_ms > launched_at_ms). The launch marker is written by
# wrap_omp_launch just before exec of the agent, so a breadcrumb newer than
# the marker was written after launch. Without this, a stale pre-launch
# breadcrumb pointing at another project's session satisfies the
# `session_path != marker_pending` CAS below and gets mis-adopted (#3230).
# `-nt` is a `test` extension implemented by every realistic container shell
# (dash, BusyBox ash, bash), avoiding the GNU/BSD `stat` format split.
[ "$f" -nt "$LAUNCH_MARKER" ] || exit 0
breadcrumb_bytes=$(head -c 16385 "$f" 2>/dev/null | wc -c) || exit 0
[ "$breadcrumb_bytes" -le 16384 ] || exit 0
# These sed reads keep a trailing CR while the host `str::lines()` strips it,
# but OMP and wrap_omp_launch write breadcrumbs with bare LF, so no CR ever
# reaches either reader. On a hypothetical CRLF crumb this path only fails
# closed (the embedded CR breaks the realpath/compare below), never mis-routes,
# so normalizing here would add sh complexity for an unreachable input.
cwd=$(head -c 16385 "$f" 2>/dev/null | sed -n '1p')
session_path=$(head -c 16385 "$f" 2>/dev/null | sed -n '2p')
marker=$(head -c 16385 "$f" 2>/dev/null | sed -n '3p')
[ "$session_path" != "$marker_pending" ] || exit 0
full_path=$session_path
case "$full_path" in /*) ;; *) full_path="$cwd/$full_path" ;; esac
exists=0
header=
if [ -f "$full_path" ] && [ ! -L "$full_path" ]; then
  canonical_full=$(realpath "$full_path" 2>/dev/null) || exit 0
  canonical_active=$(realpath "$ACTIVE_ROOT" 2>/dev/null) || canonical_active=
  canonical_managed=$(realpath "$MANAGED_ROOT" 2>/dev/null) || canonical_managed=
  valid_store=0
  # Store-shape parity: mirrors Rust has_store_shape (managed=2, custom=1).
  if [ -n "$canonical_active" ]; then
    case "$canonical_full" in
      "$canonical_active"/*)
        relative=${canonical_full#"$canonical_active"/}
        if [ "$STORE_KIND" = custom ]; then
          case "$relative" in */*) ;; *) valid_store=1 ;; esac
        else
          case "$relative" in */*/*) ;; */*) valid_store=1 ;; esac
        fi
        ;;
    esac
  fi
  if [ -n "$canonical_managed" ]; then
    case "$canonical_full" in
      "$canonical_managed"/*)
        relative=${canonical_full#"$canonical_managed"/}
        case "$relative" in */*/*) ;; */*) valid_store=1 ;; esac
        ;;
    esac
  fi
  [ "$valid_store" = 1 ] || exit 0
  exists=1
  # Anchored `^{"type":"session"` on purpose (hardening from 420bf0fd): an
  # unanchored pattern would match a `"type":"session"` substring quoted inside
  # an earlier record. Stricter than the host parse_pi_header_json, which
  # re-validates; do not loosen. Verified against oh-my-pi v17.2.10
  # (session-manager.ts:2458 writes the header via bare `JSON.stringify(header)`,
  # built type-first at :1021/:1350/:2450): OMP emits it compact, no space after
  # the colon, `type` first, so this byte-exact anchor matches real output. If a
  # future OMP changes that serializer the container fails closed while the host
  # serde parse still succeeds, so a capture regression surfaces here, not silently.
  # 65536 keeps byte parity with the host scan (PI_HEADER_SCAN_BYTES = 64*1024);
  # this raw sh literal cannot interpolate the Rust const, so a smaller window
  # here would capture a large-header session on the host yet fail closed
  # in-container. The line cap (head -n 8) already mirrors PI_HEADER_SCAN_LINES.
  # The stdout transport cap is sized from the same window (MAX_CONTAINER_CAPTURE_BYTES),
  # so a header this grep matches also survives transport, not just the grep.
  header=$(head -c 65536 "$canonical_full" | head -n 8 | grep -m1 '^{"type":"session"')
fi
marker_bytes_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | wc -c) || exit 0
[ "$marker_bytes_after" -le 17408 ] || exit 0
terminal_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '1p')
launch_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '2p')
pending_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '3p')
fingerprint_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '4p')
[ "$terminal_after" = "$terminal" ] && [ "$launch_after" = "$marker_launch" ] \
  && [ "$pending_after" = "$marker_pending" ] \
  && [ "$fingerprint_after" = "$marker_fingerprint" ] || exit 0
printf '===OMP===\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n===END===\n' \
  "$terminal" "$marker_launch" "$cwd" "$session_path" "$marker" "$exists" "$header""#;

fn select_omp_session_in_container(
    stdout: &[u8],
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let text = std::str::from_utf8(stdout).context("OMP container capture is not UTF-8")?;
    let body = text
        .strip_prefix("===OMP===\n")
        .and_then(|text| text.strip_suffix("\n===END===\n"))
        .context("No valid OMP terminal breadcrumb found in container")?;
    let mut fields = body.split('\n');
    let (
        Some(terminal_id),
        Some(marker_launch),
        Some(cwd),
        Some(path),
        Some(marker),
        Some(exists),
        Some(header),
    ) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    )
    else {
        anyhow::bail!("Malformed OMP terminal breadcrumb response");
    };
    if fields.next().is_some()
        || !valid_omp_terminal_id(terminal_id)
        || marker_launch != metadata.launch_id.as_str()
        || !matches!(marker, "" | "fresh")
        || !matches!(exists, "0" | "1")
    {
        anyhow::bail!("OMP terminal breadcrumb response has invalid identity fields");
    }
    let breadcrumb = Breadcrumb {
        cwd,
        session_path: path,
        fresh: marker == "fresh",
    };
    let parsed_header = if exists == "1" {
        Some(
            super::parse_pi_header_json(header)
                .context("OMP container session JSONL has no valid session header")?,
        )
    } else {
        None
    };
    let session_path = lexical_store_session_path(&metadata.layout, &breadcrumb)?;
    let id = validate_breadcrumb(breadcrumb, &session_path, parsed_header, exclusion)?;
    Ok(id)
}

fn capture_omp_session_in_container(
    container_name: &str,
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    launch_marker: &str,
) -> Result<String> {
    validate_omp_capture_metadata(metadata)?;
    let terminals = metadata
        .layout
        .terminal_sessions
        .to_str()
        .context("OMP container terminal path is not UTF-8")?;
    if launch_marker.is_empty() {
        anyhow::bail!("OMP sandbox launch marker is unavailable");
    }
    let active = metadata
        .layout
        .sessions
        .to_str()
        .context("OMP container session path is not UTF-8")?;
    let managed = metadata
        .layout
        .managed_sessions
        .to_str()
        .context("OMP container managed session path is not UTF-8")?;
    let kind = match metadata.layout.kind {
        OmpStoreKind::Managed => "managed",
        OmpStoreKind::Custom => "custom",
    };
    let command = container_exec_command(
        container_name,
        metadata.container_runtime,
        &[
            "sh",
            "-c",
            CONTAINER_BREADCRUMB_SCRIPT,
            "aoe-omp-capture",
            terminals,
            launch_marker,
            &metadata.launch_id,
            active,
            managed,
            kind,
            &metadata.routing_fingerprint,
        ],
    );
    let output = super::run_with_timeout_limit(
        command,
        COMMAND_TIMEOUT,
        "container exec (OMP breadcrumb capture)",
        MAX_CONTAINER_CAPTURE_BYTES,
    )?;
    select_omp_session_in_container(&output, metadata, exclusion)
}

/// One-shot sandbox capture bound exclusively by the launch marker.
pub(crate) fn try_capture_omp_session_id_in_container(
    container_name: &str,
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    launch_marker: Option<&str>,
) -> Result<String> {
    capture_omp_session_in_container(
        container_name,
        metadata,
        exclusion,
        launch_marker.context("OMP sandbox launch marker is unavailable")?,
    )
}

/// Sandbox poller. Every tick reloads the tmux generation from the pane name
/// resolved by the outer poller, then the marker selects the one and only
/// terminal breadcrumb that this launch may own.
pub(crate) fn omp_poll_fn_sandboxed(
    container_name: String,
    instance_id: String,
    launch_marker: Option<String>,
    extra_excludes: HashSet<String>,
) -> impl Fn(&str) -> Option<crate::session::poller::SessionIdObservation> + Send + 'static {
    move |tmux_session_name| {
        let metadata = load_omp_capture_metadata(tmux_session_name)
            .map_err(|error| {
                tracing::debug!(target: "session.capture", "OMP container poll metadata refresh failed: {}", error)
            })
            .ok()?;
        let marker = launch_marker.as_deref()?;
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes);
        let captured =
            capture_omp_session_in_container(&container_name, &metadata, &exclusion, marker)
                .map_err(|error| {
                    tracing::debug!(target: "session.capture", "OMP container poll capture failed: {}", error)
                })
                .ok()?;
        let refreshed = load_omp_capture_metadata(tmux_session_name).ok()?;
        if refreshed != metadata {
            return None;
        }
        super::validated_session_id(captured).map(|sid| metadata.session_observation(sid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::EnvGuard;
    use serial_test::serial;

    fn metadata(root: &Path, launched_at_ms: u64) -> OmpCaptureMetadata {
        OmpCaptureMetadata {
            layout: OmpStoreLayout {
                sessions: root.join("sessions"),
                managed_sessions: root.join("sessions"),
                terminal_sessions: root.join("terminal-sessions"),
                kind: OmpStoreKind::Managed,
            },
            launched_at_ms,
            launch_id: "launch-a".to_string(),
            launch_marker: String::new(),
            routing_fingerprint: "a".repeat(64),
            container_runtime: None,
        }
    }

    #[test]
    fn container_probe_command_uses_selected_runtime() {
        use crate::session::config::ContainerRuntimeName;

        let cases = [
            (ContainerRuntimeName::Docker, "docker"),
            (ContainerRuntimeName::Podman, "podman"),
            (ContainerRuntimeName::AppleContainer, "container"),
        ];
        for (runtime, expected_binary) in cases {
            let command = container_exec_command("aoe-test", Some(runtime), &["env"]);
            assert_eq!(command.get_program(), expected_binary, "{runtime:?}");
        }
    }

    fn write_breadcrumb(
        metadata: &OmpCaptureMetadata,
        terminal: &str,
        cwd: &Path,
        session: &Path,
        fresh: bool,
    ) -> PathBuf {
        std::fs::create_dir_all(&metadata.layout.terminal_sessions).unwrap();
        let path = metadata.layout.terminal_sessions.join(terminal);
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}",
                cwd.display(),
                session.display(),
                if fresh { "fresh\n" } else { "" }
            ),
        )
        .unwrap();
        path
    }

    fn launch_marker(metadata: &OmpCaptureMetadata, terminal: &str, pending: &str) -> String {
        format!(
            "{terminal}\n{}\n{pending}\n{}\n",
            metadata.launch_id, metadata.routing_fingerprint
        )
    }

    fn set_mtime_ms(path: &Path, millis: u64) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(
                    std::time::SystemTime::UNIX_EPOCH + Duration::from_millis(millis),
                ),
            )
            .unwrap();
    }

    #[test]
    fn terminal_id_preserves_the_exact_host_tty_identity() {
        for (tty, expected) in [
            ("/dev/pts/41", Some("pts-41")),
            ("/dev/ttys003", Some("ttys003")),
            ("pts/41", None),
            ("/dev/", None),
        ] {
            assert_eq!(omp_terminal_id_from_tty(tty).as_deref(), expected, "{tty}");
        }
    }

    #[test]
    fn parses_benign_and_store_arguments_last_wins() {
        let parsed = OmpCliCaptureOptions::parse(
            "--model sonnet --cwd old --profile old --yolo --session-dir one --profile=new --cwd=../target --session-dir=two",
        )
        .unwrap();
        assert_eq!(parsed.profile.as_deref(), Some("new"));
        assert_eq!(parsed.session_dir.as_deref(), Some(Path::new("two")));
        assert_eq!(parsed.cwd.as_deref(), Some(Path::new("../target")));
        assert_eq!(
            OmpCliCaptureOptions::parse("-- --profile ignored --no-session").unwrap(),
            OmpCliCaptureOptions::default()
        );
        assert_eq!(
            OmpCliCaptureOptions::parse("--system-prompt --profile work")
                .unwrap()
                .profile,
            None
        );
        assert_eq!(
            OmpCliCaptureOptions::parse("--trusted-extension --session-dir /x")
                .unwrap()
                .session_dir,
            None
        );
        for invalid in [
            "--no-session",
            "--profile",
            "--session-dir=",
            "--cwd=",
            "--model x; echo bad",
        ] {
            assert!(OmpCliCaptureOptions::parse(invalid).is_err(), "{invalid}");
        }
        for profile in ["con", "aux.txt", "com0", "lpt9"] {
            assert!(normalize_profile(Some(profile)).is_err(), "{profile}");
        }
        assert_eq!(
            normalize_profile(Some("valid_profile")).unwrap().as_deref(),
            Some("valid_profile")
        );
        assert!(OmpCliCaptureOptions::parse(
            "--add-dir src/* --system-prompt prompts/*.md --cwd=~/project"
        )
        .is_ok());
    }

    #[test]
    fn rejects_only_unquoted_shell_path_expansions() {
        for expansion in [
            "--cwd ~/project",
            "--cwd=project/*",
            "--cwd=project/?",
            "--cwd=project/[ab]",
            "--cwd=project/{one,two}",
        ] {
            assert!(
                OmpCliCaptureOptions::parse(expansion).is_err(),
                "{expansion}"
            );
        }
        assert_eq!(
            OmpCliCaptureOptions::parse("--add-dir ~/shared --cwd=~/project")
                .unwrap()
                .cwd
                .as_deref(),
            Some(Path::new("~/project"))
        );
        for (literal, expected) in [
            (
                "--cwd='~/project/*?[ab]{one,two}'",
                "~/project/*?[ab]{one,two}",
            ),
            (
                r"--cwd=\~/project/\*/\?/\[ab\]/\{one,two\}",
                "~/project/*/?/[ab]/{one,two}",
            ),
        ] {
            assert_eq!(
                OmpCliCaptureOptions::parse(literal).unwrap().cwd.as_deref(),
                Some(Path::new(expected)),
                "{literal}"
            );
        }
    }

    #[test]
    fn dotenv_parser_is_literal_and_mirrors_omp_names() {
        let parsed = parse_dotenv(
            "export PI_CODING_AGENT_DIR=$HOME/store\nOMP_CODING_AGENT_SESSION_DIR='relative/$USER'\n",
        );
        assert_eq!(parsed["PI_CODING_AGENT_DIR"], "$HOME/store");
        assert_eq!(parsed["PI_CODING_AGENT_SESSION_DIR"], "relative/$USER");
    }

    #[test]
    fn dotenv_precedence_is_exec_project_agent_config_home() {
        let mut exec = HashMap::from([("HOME".to_string(), "/exec".to_string())]);
        let files = [
            HashMap::from([("HOME".to_string(), "/project".to_string())]),
            HashMap::from([("XDG_DATA_HOME".to_string(), "/agent".to_string())]),
            HashMap::from([("XDG_DATA_HOME".to_string(), "/config".to_string())]),
            HashMap::from([("XDG_STATE_HOME".to_string(), "/home".to_string())]),
        ];
        let merged = merge_omp_environment(exec.clone(), &files);
        assert_eq!(merged["HOME"], "/exec");
        assert_eq!(merged["XDG_DATA_HOME"], "/agent");
        assert_eq!(merged["XDG_STATE_HOME"], "/home");
        exec.insert("HOME".to_string(), String::new());
        assert_eq!(merge_omp_environment(exec, &files)["HOME"], "/project");
    }

    #[test]
    fn bun_dotenv_replaces_an_empty_launcher_value_but_not_a_nonempty_one() {
        let cwd = Path::new("/workspace");
        for (launcher, expected) in [
            (None, "from-dotenv"),
            (Some(""), "from-dotenv"),
            (Some("from-launcher"), "from-launcher"),
        ] {
            let mut env = HashMap::new();
            if let Some(value) = launcher {
                env.insert("PI_CODING_AGENT_DIR".to_string(), value.to_string());
            }
            let resolved = autoload_bun_dotenv(env, cwd, |path| {
                Ok((path == cwd.join(".env"))
                    .then(|| "PI_CODING_AGENT_DIR=from-dotenv\n".to_string()))
            })
            .unwrap();
            assert_eq!(
                resolved.get("PI_CODING_AGENT_DIR").map(String::as_str),
                Some(expected),
                "launcher value: {launcher:?}"
            );
        }
    }

    #[test]
    #[serial]
    fn resolver_applies_real_dotenv_precedence_and_exec_override() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        let config = home.join(".omp");
        let agent = config.join("agent");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(home.join(".env"), "OMP_CODING_AGENT_DIR=home-store\n").unwrap();
        std::fs::write(config.join(".env"), "OMP_CODING_AGENT_DIR=config-store\n").unwrap();
        std::fs::write(agent.join(".env"), "OMP_CODING_AGENT_DIR=agent-store\n").unwrap();
        std::fs::write(project.join(".env"), "OMP_CODING_AGENT_DIR=project-store\n").unwrap();
        let _env = EnvGuard::unset(&OMP_STORE_ENV_KEYS);
        let base = vec![format!("HOME={}", home.display())];
        let layout = resolve_omp_store_layout(
            &base,
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(layout.sessions, project.join("project-store/sessions"));

        let mut explicitly_empty = base.clone();
        explicitly_empty.push("PI_CODING_AGENT_DIR=".to_string());
        let layout = resolve_omp_store_layout(
            &explicitly_empty,
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(layout.sessions, project.join("project-store/sessions"));

        let mut overridden = base;
        overridden.push(format!(
            "PI_CODING_AGENT_DIR={}",
            project.join("exec-store").display()
        ));
        let layout = resolve_omp_store_layout(
            &overridden,
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(layout.sessions, project.join("exec-store/sessions"));

        let routing_project = tmp.path().join("routing-project");
        std::fs::create_dir_all(&routing_project).unwrap();
        let pi_only = vec![
            format!("HOME={}", home.display()),
            "PI_PROFILE=work".to_string(),
        ];
        let (pi_layout, absent_fingerprint) = resolve_omp_store_layout_with_environment(
            &pi_only,
            routing_project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(
            pi_layout.sessions,
            home.join(".omp/profiles/work/agent/sessions")
        );
        let mutations = omp_host_routing_environment(&pi_only);
        assert!(mutations.contains(&crate::tmux::PaneEnvMutation::unset(
            "OMP_PROFILE".to_string()
        )));
        assert!(mutations.contains(&crate::tmux::PaneEnvMutation::set(
            "PI_PROFILE".to_string(),
            "work".to_string()
        )));

        let mut explicit_default = pi_only;
        explicit_default.push("OMP_PROFILE=".to_string());
        let (default_layout, empty_fingerprint) = resolve_omp_store_layout_with_environment(
            &explicit_default,
            routing_project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(
            default_layout.sessions,
            routing_project.join("agent-store/sessions")
        );
        assert_ne!(absent_fingerprint, empty_fingerprint);
    }

    #[test]
    #[serial]
    fn bun_cwd_dotenv_selects_profile_with_mode_local_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".env"), "OMP_PROFILE=base\n").unwrap();
        std::fs::write(project.join(".env.testing"), "OMP_PROFILE=mode\n").unwrap();
        let _env = EnvGuard::unset(&OMP_STORE_ENV_KEYS);
        let (mode_layout, _) = resolve_omp_store_layout_with_environment(
            &[
                format!("HOME={}", home.display()),
                "NODE_ENV=testing".to_string(),
            ],
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(
            mode_layout.sessions,
            home.join(".omp/profiles/mode/agent/sessions")
        );

        std::fs::write(project.join(".env.local"), "OMP_PROFILE=local\n").unwrap();
        let (layout, fingerprint) = resolve_omp_store_layout_with_environment(
            &[
                format!("HOME={}", home.display()),
                "NODE_ENV=testing".to_string(),
            ],
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(
            layout.sessions,
            home.join(".omp/profiles/local/agent/sessions")
        );
        assert_eq!(fingerprint.len(), 64);
        let launcher = resolve_omp_store_layout(
            &[
                format!("HOME={}", home.display()),
                "NODE_ENV=testing".to_string(),
                "OMP_PROFILE=launcher".to_string(),
            ],
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(
            launcher.sessions,
            home.join(".omp/profiles/launcher/agent/sessions")
        );
    }

    #[test]
    #[serial]
    fn cli_profile_selects_its_dotenv_locations_before_store_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        let cli_agent = home.join(".omp/profiles/cli/agent");
        let dotenv_agent = home.join(".omp/profiles/from_dotenv/agent");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&cli_agent).unwrap();
        std::fs::create_dir_all(&dotenv_agent).unwrap();
        std::fs::write(project.join(".env"), "OMP_PROFILE=from_dotenv\n").unwrap();
        std::fs::write(
            cli_agent.join(".env"),
            "PI_CODING_AGENT_SESSION_DIR=cli-profile-store\n",
        )
        .unwrap();
        std::fs::write(
            dotenv_agent.join(".env"),
            "PI_CODING_AGENT_SESSION_DIR=dotenv-profile-store\n",
        )
        .unwrap();
        let _env = EnvGuard::unset(&OMP_STORE_ENV_KEYS);
        let options = OmpCliCaptureOptions {
            profile: Some("cli".to_string()),
            ..OmpCliCaptureOptions::default()
        };
        let layout = resolve_omp_store_layout(
            &[format!("HOME={}", home.display())],
            project.to_str().unwrap(),
            &options,
        )
        .unwrap();
        assert_eq!(layout.sessions, project.join("cli-profile-store"));
    }

    #[test]
    #[serial]
    fn bun_cwd_dotenv_allows_routing_dependencies_and_rejects_other_expansions() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(".env.local"),
            "PI_CODING_AGENT_SESSION_DIR='$HOME/${PI_PROFILE}/\\$ROUTE/sessions'\n",
        )
        .unwrap();
        let _env = EnvGuard::unset(&OMP_STORE_ENV_KEYS);
        let (layout, fingerprint) = resolve_omp_store_layout_with_environment(
            &[
                format!("HOME={}", home.display()),
                "PI_PROFILE=expanded".to_string(),
            ],
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(layout.sessions, home.join("expanded/$ROUTE/sessions"));
        assert_eq!(fingerprint.len(), 64);

        for key in ["PI_CONFIG_DIR", "OMP_CONFIG_DIR"] {
            std::fs::write(
                project.join(".env.local"),
                format!("{key}=$AWS_SECRET_ACCESS_KEY\n"),
            )
            .unwrap();
            let error = resolve_omp_store_layout_with_environment(
                &[
                    format!("HOME={}", home.display()),
                    "AWS_SECRET_ACCESS_KEY=must-not-persist".to_string(),
                ],
                project.to_str().unwrap(),
                &OmpCliCaptureOptions::default(),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("non-routing variables"),
                "{key}: {error:#}"
            );
        }
    }

    #[test]
    #[serial]
    fn large_dotenv_is_loaded_and_unreadable_or_invalid_files_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut large = "# padding\n".repeat(8_000);
        large.push_str("PI_CODING_AGENT_DIR=large-store\n");
        std::fs::write(project.join(".env"), large).unwrap();
        let _env = EnvGuard::unset(&OMP_STORE_ENV_KEYS);
        let layout = resolve_omp_store_layout(
            &[format!("HOME={}", home.display())],
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(layout.sessions, project.join("large-store/sessions"));

        let invalid = tmp.path().join("invalid.env");
        std::fs::write(&invalid, [0xff, b'=', b'x']).unwrap();
        assert!(read_dotenv_file(&invalid).is_err());
        let unreadable = tmp.path().join("directory.env");
        std::fs::create_dir(&unreadable).unwrap();
        assert!(read_dotenv_file(&unreadable).is_err());
        let oversized = tmp.path().join("oversized.env");
        let oversized_file = std::fs::File::create(&oversized).unwrap();
        oversized_file
            .set_len((MAX_DOTENV_BYTES + 1) as u64)
            .unwrap();
        assert!(read_dotenv_file(&oversized).is_err());
        #[cfg(unix)]
        {
            let target = tmp.path().join("secret.env");
            let link = tmp.path().join("linked.env");
            std::fs::write(&target, "OMP_PROFILE=must-not-follow\n").unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(
                read_dotenv_file(&link).is_err(),
                "dotenv symlinks must disable capture rather than alter launch routing"
            );
        }
    }

    #[test]
    fn resolver_routes_xdg_independently_and_honors_typed_layouts() {
        let cwd = Path::new("/workspace/project");
        let mut env = HashMap::from([
            ("HOME".to_string(), "/home/test".to_string()),
            ("XDG_DATA_HOME".to_string(), "/data".to_string()),
            ("XDG_STATE_HOME".to_string(), "/state".to_string()),
        ]);
        let data_only = resolve_layout(&env, cwd, None, &OmpCliCaptureOptions::default(), |path| {
            path == Path::new("/data/omp")
        })
        .unwrap();
        assert_eq!(data_only.sessions, Path::new("/data/omp/sessions"));
        assert_eq!(
            data_only.terminal_sessions,
            Path::new("/home/test/.omp/agent/terminal-sessions")
        );

        env.insert(
            "PI_CODING_AGENT_DIR".to_string(),
            "/ignored-for-profile".to_string(),
        );
        let profile = resolve_layout(
            &env,
            cwd,
            Some("work"),
            &OmpCliCaptureOptions::default(),
            |_| true,
        )
        .unwrap();
        assert_eq!(
            profile.sessions,
            Path::new("/data/omp/profiles/work/sessions")
        );
        assert_eq!(
            profile.terminal_sessions,
            Path::new("/state/omp/profiles/work/terminal-sessions")
        );

        env.insert("OMP_PROFILE".to_string(), String::new());
        env.insert("PI_PROFILE".to_string(), "work".to_string());
        env.insert(
            "PI_CODING_AGENT_DIR".to_string(),
            "/home/test/.omp/profiles/work/agent".to_string(),
        );
        assert_eq!(resolve_profile(None, &env).unwrap(), None);
        let restored_default =
            resolve_layout(&env, cwd, None, &OmpCliCaptureOptions::default(), |_| false).unwrap();
        assert_eq!(
            restored_default.sessions,
            Path::new("/home/test/.omp/agent/sessions"),
            "an explicitly default OMP profile must not inherit PI's profile-derived agent dir"
        );
        let custom_options = OmpCliCaptureOptions {
            profile: None,
            session_dir: Some(PathBuf::from(".sessions")),
            cwd: Some(PathBuf::from("../other")),
        };
        env.remove("PI_CODING_AGENT_DIR");
        let custom = resolve_layout(&env, cwd, None, &custom_options, |path| {
            path == Path::new("/state/omp")
        })
        .unwrap();
        assert_eq!(custom.kind, OmpStoreKind::Custom);
        assert_eq!(custom.sessions, Path::new("/workspace/other/.sessions"));
        assert_eq!(
            custom.managed_sessions,
            Path::new("/home/test/.omp/agent/sessions")
        );
        assert_eq!(
            custom.terminal_sessions,
            Path::new("/state/omp/terminal-sessions")
        );
    }

    #[test]
    #[serial]
    fn resolves_relative_agent_dir_against_launch_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let _env = EnvGuard::unset(&[
            "HOME",
            "OMP_PROFILE",
            "PI_PROFILE",
            "PI_CODING_AGENT_DIR",
            "PI_CODING_AGENT_SESSION_DIR",
            "PI_CONFIG_DIR",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
        ]);
        let layout = resolve_omp_store_layout(
            &[
                "HOME=/home/test".into(),
                "PI_CODING_AGENT_DIR=.store".into(),
            ],
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(layout.sessions, project.join(".store/sessions"));
        assert_eq!(
            layout.terminal_sessions,
            project.join(".store/terminal-sessions")
        );
    }

    #[test]
    fn custom_launch_accepts_a_later_managed_resume_breadcrumb() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        let home = tmp.path().join("home");
        let layout = resolve_layout(
            &HashMap::from([("HOME".to_string(), home.display().to_string())]),
            &cwd,
            None,
            &OmpCliCaptureOptions {
                session_dir: Some(tmp.path().join("custom")),
                ..OmpCliCaptureOptions::default()
            },
            |_| false,
        )
        .unwrap();
        assert_eq!(layout.kind, OmpStoreKind::Custom);
        assert_eq!(layout.sessions, tmp.path().join("custom"));
        assert_eq!(layout.managed_sessions, home.join(".omp/agent/sessions"));
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let resumed = layout
            .managed_sessions
            .join("other-project")
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::create_dir_all(resumed.parent().unwrap()).unwrap();
        std::fs::write(
            &resumed,
            format!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{}\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();
        let resume_breadcrumb = Breadcrumb {
            cwd: cwd.to_str().unwrap(),
            session_path: resumed.to_str().unwrap(),
            fresh: false,
        };
        let resume_path = lexical_store_session_path(&layout, &resume_breadcrumb).unwrap();
        ensure_canonical_store(&layout, &resume_path).unwrap();
        assert_eq!(
            validate_breadcrumb(
                resume_breadcrumb,
                &resume_path,
                Some((Some(id.to_string()), Some(cwd.display().to_string()))),
                &HashSet::new(),
            )
            .unwrap(),
            id
        );
    }

    #[test]
    fn common_validation_rejects_escape_nesting_relative_managed_and_exclusion() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 0);
        let cwd = tmp.path().join("project");
        let bucket = meta.layout.sessions.join("bucket");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&bucket).unwrap();
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let valid = bucket.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        let outside = tmp
            .path()
            .join(format!("outside/2026-01-01T00-00-00-000Z_{id}.jsonl"));
        let nested = bucket
            .join("nested")
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        for path in [&outside, &nested] {
            assert!(lexical_store_session_path(
                &meta.layout,
                &Breadcrumb {
                    cwd: cwd.to_str().unwrap(),
                    session_path: path.to_str().unwrap(),
                    fresh: true,
                },
            )
            .is_err());
        }
        assert!(lexical_store_session_path(
            &meta.layout,
            &Breadcrumb {
                cwd: cwd.to_str().unwrap(),
                session_path: "relative.jsonl",
                fresh: true,
            },
        )
        .is_err());
        let excluded_breadcrumb = Breadcrumb {
            cwd: cwd.to_str().unwrap(),
            session_path: valid.to_str().unwrap(),
            fresh: true,
        };
        let excluded_path = lexical_store_session_path(&meta.layout, &excluded_breadcrumb).unwrap();
        assert!(validate_breadcrumb(
            excluded_breadcrumb,
            &excluded_path,
            None,
            &HashSet::from([id.to_string()]),
        )
        .is_err());
    }

    #[test]
    fn prelaunch_stale_breadcrumb_is_rejected_even_when_marker_differs() {
        // A modern marker records the pre-launch pending SENTINEL (no breadcrumb
        // existed at launch). A stale pre-launch breadcrumb then lands at the tty
        // pointing at another project's old session: internally consistent, but
        // its mtime predates the launch. It differs from the sentinel, so the
        // marker "proves rewrite"; only the freshness guard can reject it, so
        // freshness must apply even when the marker matches (#3230).
        let tmp = tempfile::tempdir().unwrap();
        let mut meta = metadata(tmp.path(), 100_000);
        let old_project = tmp.path().join("unrelated-old-project");
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let session = meta
            .layout
            .managed_sessions
            .join("old-project")
            .join(format!("2020-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::create_dir_all(&old_project).unwrap();
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(
            &session,
            format!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{}\"}}\n",
                old_project.display()
            ),
        )
        .unwrap();
        let sentinel = meta
            .layout
            .managed_sessions
            .join(format!(".aoe-pending-{}", meta.launch_id))
            .join(format!("aoe-pending_{}.jsonl", meta.launch_id));
        let marker = tmp.path().join("launch-marker");
        std::fs::write(
            &marker,
            launch_marker(&meta, "pts-7", &sentinel.to_string_lossy()),
        )
        .unwrap();
        meta.launch_marker = marker.to_string_lossy().into_owned();

        let crumb = write_breadcrumb(&meta, "pts-7", &old_project, &session, false);
        set_mtime_ms(&crumb, meta.launched_at_ms - 1);
        assert!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-7").is_err(),
            "a stale pre-launch breadcrumb must not be accepted merely because it \
             differs from the marker's pending sentinel"
        );
        set_mtime_ms(&crumb, meta.launched_at_ms + 1);
        assert_eq!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-7").unwrap(),
            id,
            "a genuine post-launch rewrite of the same target is still accepted"
        );
    }

    #[test]
    fn materialized_breadcrumb_accepts_cross_project_but_requires_header_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 100_000);
        let historical = tmp.path().join("historical");
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let session = meta
            .layout
            .managed_sessions
            .join("historical-project")
            .join(format!("2025-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::create_dir_all(&historical).unwrap();
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(
            &session,
            format!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{}\"}}\n",
                historical.display()
            ),
        )
        .unwrap();
        let breadcrumb = write_breadcrumb(&meta, "pts-1", &historical, &session, false);
        assert_eq!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").unwrap(),
            id
        );
        set_mtime_ms(&breadcrumb, meta.launched_at_ms);
        assert!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").is_err(),
            "a same-watermark legacy breadcrumb may belong to a previous pane"
        );
        set_mtime_ms(&breadcrumb, meta.launched_at_ms + 1);
        assert_eq!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").unwrap(),
            id
        );
        std::fs::write(
            &session,
            format!("{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"/wrong\"}}\n"),
        )
        .unwrap();
        assert!(capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").is_err());
    }

    #[test]
    fn absent_breadcrumb_sentinel_must_be_rewritten_and_modern_pending_is_nonempty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut meta = metadata(tmp.path(), 150_000);
        let cwd = tmp.path().join("project");
        let bucket = meta.layout.sessions.join("bucket");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&bucket).unwrap();
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let session = bucket.join(format!("1970-01-01T00-00-01-000Z_{id}.jsonl"));
        std::fs::write(
            &session,
            format!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{}\"}}",
                cwd.display()
            ),
        )
        .unwrap();
        meta.launch_id = id.to_string();
        let pending = meta
            .layout
            .managed_sessions
            .join(format!(".aoe-pending-{id}"))
            .join(format!("aoe-pending_{id}.jsonl"));
        let crumb = write_breadcrumb(&meta, "pts-1", &cwd, &pending, true);
        let marker = tmp.path().join("launch-marker");
        std::fs::write(
            &marker,
            launch_marker(&meta, "pts-1", &pending.to_string_lossy()),
        )
        .unwrap();
        meta.launch_marker = marker.to_string_lossy().into_owned();
        set_mtime_ms(&marker, 100_000);
        set_mtime_ms(&crumb, 100_000);
        assert!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").is_err(),
            "the installed fresh sentinel is still the marker's pending path"
        );

        write_breadcrumb(&meta, "pts-1", &cwd, &session, false);
        set_mtime_ms(&crumb, 100_000);
        assert!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").is_err(),
            "even a marker-proven rewrite must be a post-launch write; otherwise a stale \
             pre-launch breadcrumb that merely differs from the sentinel is adopted (#3230)"
        );
        set_mtime_ms(&crumb, meta.launched_at_ms + 1);
        assert_eq!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").unwrap(),
            id,
            "a fresh marker-proven rewrite is accepted"
        );

        std::fs::write(&marker, launch_marker(&meta, "pts-1", "")).unwrap();
        set_mtime_ms(&crumb, meta.launched_at_ms + 1);
        assert!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").is_err(),
            "a modern marker with an empty pending path must not fall back to mtime"
        );
        set_mtime_ms(&crumb, 100_000);
        std::fs::write(
            &marker,
            launch_marker(&meta, "pts-1", &pending.to_string_lossy()),
        )
        .unwrap();
        std::fs::write(
            &marker,
            format!(
                "pts-1\n{}\n{}\n{}\n",
                meta.launch_id,
                pending.display(),
                "b".repeat(64)
            ),
        )
        .unwrap();
        assert!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").is_err(),
            "a marker from a different routing snapshot must be rejected"
        );
        std::fs::write(
            &marker,
            format!(
                "pts-1\nstale-launch\n{}\n{}\n",
                pending.display(),
                meta.routing_fingerprint
            ),
        )
        .unwrap();
        assert!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").is_err(),
            "a marker from a superseded launch generation (reused path) must be rejected"
        );
        std::fs::write(
            &marker,
            launch_marker(&meta, "pts-1", &session.to_string_lossy()),
        )
        .unwrap();
        assert!(capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1").is_err());
    }

    #[test]
    fn fresh_breadcrumb_waits_for_a_materialized_target() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 0);
        let cwd = tmp.path().join("project");
        let bucket = meta.layout.sessions.join("bucket");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&bucket).unwrap();
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let session = bucket.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        write_breadcrumb(&meta, "fresh", &cwd, &session, true);
        assert!(capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "fresh").is_err());
        write_breadcrumb(&meta, "not-fresh", &cwd, &session, false);
        assert!(capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "not-fresh").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn container_script_bounds_inputs_and_reads_only_the_marker_terminal() {
        assert!(
            CONTAINER_BREADCRUMB_SCRIPT.contains(&format!(
                "head -n {} | grep",
                super::super::PI_HEADER_SCAN_LINES
            )),
            "container header scan depth must match the host scan depth"
        );
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 100_000);
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let cwd = "/workspace/project";
        let bucket = meta.layout.sessions.join("bucket");
        let session = bucket.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::create_dir_all(&bucket).unwrap();
        std::fs::create_dir_all(&meta.layout.terminal_sessions).unwrap();
        std::fs::write(
            &session,
            format!("{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}\n"),
        )
        .unwrap();
        let marker = tmp.path().join("launch-marker");
        std::fs::write(&marker, launch_marker(&meta, "pts-9", "/pending")).unwrap();
        set_mtime_ms(&marker, 100_000);
        let breadcrumb = meta.layout.terminal_sessions.join("pts-9");
        std::fs::write(&breadcrumb, format!("{cwd}\n{}\n", session.display())).unwrap();
        // Newer than the launch marker for every run below: the `-nt` freshness
        // guard requires a post-launch breadcrumb, and marker rewrites here bump
        // the marker mtime, so pin the breadcrumb far ahead of any of them.
        set_mtime_ms(&breadcrumb, 4_000_000_000_000);
        std::fs::write(
            meta.layout.terminal_sessions.join("pts-decoy"),
            format!("{cwd}\n{}\nfresh\n", session.display()),
        )
        .unwrap();

        let run = |marker: &Path| {
            std::process::Command::new("sh")
                .args([
                    "-c",
                    CONTAINER_BREADCRUMB_SCRIPT,
                    "aoe-omp-test",
                    meta.layout.terminal_sessions.to_str().unwrap(),
                    marker.to_str().unwrap(),
                    &meta.launch_id,
                    meta.layout.sessions.to_str().unwrap(),
                    meta.layout.managed_sessions.to_str().unwrap(),
                    "managed",
                    &meta.routing_fingerprint,
                ])
                .output()
                .unwrap()
        };
        let output = run(&marker);
        assert!(output.status.success());
        let captured =
            select_omp_session_in_container(&output.stdout, &meta, &HashSet::new()).unwrap();
        assert_eq!(captured, id);
        std::fs::write(&marker, launch_marker(&meta, "pts-9", "")).unwrap();
        assert!(
            run(&marker).stdout.is_empty(),
            "the container reader must reject an empty modern pending path"
        );
        std::fs::write(
            &marker,
            launch_marker(&meta, "pts-9", &session.to_string_lossy()),
        )
        .unwrap();
        assert!(run(&marker).stdout.is_empty());
        std::fs::write(&marker, launch_marker(&meta, "pts-9", "/pending")).unwrap();

        std::fs::write(&breadcrumb, vec![b'x'; MAX_BREADCRUMB_BYTES + 1]).unwrap();
        set_mtime_ms(&breadcrumb, 4_000_000_000_000);
        assert!(run(&marker).stdout.is_empty());

        std::fs::write(&breadcrumb, format!("{cwd}\n{}\n", session.display())).unwrap();
        std::fs::write(&marker, vec![b'x'; MAX_LAUNCH_MARKER_BYTES + 1]).unwrap();
        assert!(run(&marker).stdout.is_empty());
        std::fs::write(&marker, launch_marker(&meta, "pts-9", "/pending")).unwrap();

        let marker_link = tmp.path().join("launch-marker-link");
        std::os::unix::fs::symlink(&marker, &marker_link).unwrap();
        assert!(run(&marker_link).stdout.is_empty());
    }

    #[test]
    fn lexical_store_gate_accepts_in_store_jsonl_and_rejects_others() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 0);
        let cwd = tmp.path().join("project");
        let cwd_str = cwd.to_string_lossy().into_owned();
        let in_store = meta
            .layout
            .sessions
            .join("bucket")
            .join("2026-01-01T00-00-00-000Z_019fc9a0-f688-7000-ae45-d9e51e5e1b8a.jsonl")
            .to_string_lossy()
            .into_owned();
        let out_of_store = tmp
            .path()
            .join("outside")
            .join("2026-01-01T00-00-00-000Z_019fc9a0-f688-7000-ae45-d9e51e5e1b8a.jsonl")
            .to_string_lossy()
            .into_owned();
        let wrong_extension = meta
            .layout
            .sessions
            .join("bucket")
            .join("session.txt")
            .to_string_lossy()
            .into_owned();
        let cases: [(&str, &str, bool); 4] = [
            ("in-store jsonl", &in_store, true),
            ("out-of-store", &out_of_store, false),
            ("wrong extension", &wrong_extension, false),
            ("managed relative", "relative.jsonl", false),
        ];
        for (label, session_path, expect_ok) in cases {
            let breadcrumb = Breadcrumb {
                cwd: &cwd_str,
                session_path,
                fresh: false,
            };
            assert_eq!(
                lexical_store_session_path(&meta.layout, &breadcrumb).is_ok(),
                expect_ok,
                "{label}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn host_and_container_store_shape_verdicts_match() {
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let cwd = "/workspace/project";
        let file = format!("2026-01-01T00-00-00-000Z_{id}.jsonl");
        let cases: [(&str, &[&str], bool); 3] = [
            ("in-store", &["bucket"], true),
            ("too-shallow", &[], false),
            ("too-deep", &["a", "b"], false),
        ];
        for (label, dirs, accepted) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let mut meta = metadata(tmp.path(), 100_000);
            let mut session = meta.layout.sessions.clone();
            for dir in dirs {
                session = session.join(dir);
            }
            session = session.join(&file);
            std::fs::create_dir_all(session.parent().unwrap()).unwrap();
            std::fs::write(
                &session,
                format!("{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}\n"),
            )
            .unwrap();
            let breadcrumb = write_breadcrumb(&meta, "pts-7", Path::new(cwd), &session, false);
            // BSD `test -nt` only compares timestamps at whole-second precision.
            // Keep the fixture fresh on the macOS CI host as well as in Linux containers.
            set_mtime_ms(&breadcrumb, 101_000);
            let marker = tmp.path().join("launch-marker");
            std::fs::write(&marker, launch_marker(&meta, "pts-7", "/pending")).unwrap();
            set_mtime_ms(&marker, 100_000);
            meta.launch_marker = marker.to_string_lossy().into_owned();

            let host = capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-7");
            let output = std::process::Command::new("sh")
                .args([
                    "-c",
                    CONTAINER_BREADCRUMB_SCRIPT,
                    "aoe-omp-parity",
                    meta.layout.terminal_sessions.to_str().unwrap(),
                    marker.to_str().unwrap(),
                    &meta.launch_id,
                    meta.layout.sessions.to_str().unwrap(),
                    meta.layout.managed_sessions.to_str().unwrap(),
                    "managed",
                    &meta.routing_fingerprint,
                ])
                .output()
                .unwrap();
            let container = select_omp_session_in_container(&output.stdout, &meta, &HashSet::new());
            assert_eq!(
                host.is_ok(),
                container.is_ok(),
                "{label}: host/container verdict diverged"
            );
            assert_eq!(host.is_ok(), accepted, "{label}");
            if accepted {
                assert_eq!(host.unwrap(), id, "{label}");
                assert_eq!(container.unwrap(), id, "{label}");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn container_capture_transports_a_header_larger_than_the_probe_cap() {
        // A session header past the small probe cap (e.g. a multi-repo launch
        // with many additionalDirectories) must still capture in-container: the
        // host scans up to PI_HEADER_SCAN_BYTES, and the transport cap is derived
        // from the same window. Under the old probe-sized cap this bailed on
        // "exceeded its stdout limit" while the host succeeded, so this drives
        // the capture through run_with_timeout_limit to exercise that boundary.
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let cwd = "/workspace/project";
        let file = format!("2026-01-01T00-00-00-000Z_{id}.jsonl");
        let pad = "x".repeat(32 * 1024);
        assert!(
            pad.len() > MAX_CONTAINER_PROBE_BYTES,
            "the header must exceed the probe cap for this test to be meaningful"
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut meta = metadata(tmp.path(), 100_000);
        let session = meta.layout.sessions.join("bucket").join(&file);
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(
            &session,
            format!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"pad\":\"{pad}\"}}\n"
            ),
        )
        .unwrap();
        let breadcrumb = write_breadcrumb(&meta, "pts-7", Path::new(cwd), &session, false);
        // The container script is exercised by the host shell in this test, whose
        // BSD `test -nt` comparison needs a whole-second timestamp difference.
        set_mtime_ms(&breadcrumb, 101_000);
        let marker = tmp.path().join("launch-marker");
        std::fs::write(&marker, launch_marker(&meta, "pts-7", "/pending")).unwrap();
        set_mtime_ms(&marker, 100_000);
        meta.launch_marker = marker.to_string_lossy().into_owned();

        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            CONTAINER_BREADCRUMB_SCRIPT,
            "aoe-omp-large-header",
            meta.layout.terminal_sessions.to_str().unwrap(),
            marker.to_str().unwrap(),
            &meta.launch_id,
            meta.layout.sessions.to_str().unwrap(),
            meta.layout.managed_sessions.to_str().unwrap(),
            "managed",
            &meta.routing_fingerprint,
        ]);
        let output = super::super::run_with_timeout_limit(
            command,
            COMMAND_TIMEOUT,
            "container exec (large header capture test)",
            MAX_CONTAINER_CAPTURE_BYTES,
        )
        .expect("a header within the host scan window must not exceed the container cap");
        assert_eq!(
            select_omp_session_in_container(&output, &meta, &HashSet::new()).unwrap(),
            id
        );
    }

    #[test]
    #[cfg(unix)]
    fn container_rejects_a_pre_launch_stale_breadcrumb_but_accepts_a_post_launch_rewrite() {
        // Container parity with the host freshness guard (#3230): a breadcrumb
        // older than the launch marker predates launch and must be rejected even
        // though it satisfies the `session_path != marker_pending` CAS; a
        // breadcrumb newer than the marker is a post-launch write and captures.
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let cwd = "/workspace/project";
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 100_000);
        let session = meta
            .layout
            .sessions
            .join("bucket")
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(
            &session,
            format!("{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}\n"),
        )
        .unwrap();
        let breadcrumb = write_breadcrumb(&meta, "pts-7", Path::new(cwd), &session, false);
        let marker = tmp.path().join("launch-marker");
        std::fs::write(&marker, launch_marker(&meta, "pts-7", "/pending")).unwrap();
        set_mtime_ms(&marker, 100_000);

        let run = || {
            let mut command = std::process::Command::new("sh");
            command.args([
                "-c",
                CONTAINER_BREADCRUMB_SCRIPT,
                "aoe-omp-freshness",
                meta.layout.terminal_sessions.to_str().unwrap(),
                marker.to_str().unwrap(),
                &meta.launch_id,
                meta.layout.sessions.to_str().unwrap(),
                meta.layout.managed_sessions.to_str().unwrap(),
                "managed",
                &meta.routing_fingerprint,
            ]);
            super::super::run_with_timeout_limit(
                command,
                COMMAND_TIMEOUT,
                "container exec (freshness test)",
                MAX_CONTAINER_CAPTURE_BYTES,
            )
            .unwrap()
        };

        // Stale: breadcrumb older than the marker -> script emits no record.
        set_mtime_ms(&breadcrumb, 1_000);
        assert!(
            select_omp_session_in_container(&run(), &meta, &HashSet::new()).is_err(),
            "a breadcrumb predating the launch marker must not be captured in-container"
        );

        // Fresh: breadcrumb written after the marker -> captured.
        set_mtime_ms(&breadcrumb, 200_000);
        assert_eq!(
            select_omp_session_in_container(&run(), &meta, &HashSet::new()).unwrap(),
            id
        );
    }

    fn sandbox_record(
        metadata: &OmpCaptureMetadata,
        terminal: &str,
        launch_id: &str,
        id: &str,
    ) -> String {
        format!(
            "===OMP===\n{terminal}\n{launch_id}\n/workspace/project\n{}/bucket/2026-01-01T00-00-00-000Z_{id}.jsonl\nfresh\n0\n\n===END===\n",
            metadata.layout.sessions.display()
        )
    }

    fn sandbox_materialized_record(
        metadata: &OmpCaptureMetadata,
        terminal: &str,
        id: &str,
    ) -> String {
        format!(
            "===OMP===\n{terminal}\n{}\n/workspace/project\n{}/bucket/2025-01-01T00-00-00-000Z_{id}.jsonl\n\n1\n{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"/workspace/project\"}}\n===END===\n",
            metadata.launch_id,
            metadata.layout.sessions.display()
        )
    }

    #[test]
    fn sandbox_rejects_marker_from_a_different_launch_generation() {
        let meta = metadata(Path::new("/root/.omp/agent"), 100_000);
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let output = sandbox_record(&meta, "pts-9", "launch-b", id);
        assert!(
            select_omp_session_in_container(output.as_bytes(), &meta, &HashSet::new()).is_err()
        );
    }

    #[test]
    fn sandbox_marker_selects_exactly_one_terminal() {
        let meta = metadata(Path::new("/root/.omp/agent"), 100_000);
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let output = sandbox_materialized_record(&meta, "pts-9", id);
        let selected =
            select_omp_session_in_container(output.as_bytes(), &meta, &HashSet::new()).unwrap();
        assert_eq!(selected, id);

        let other = "019fc9df-34e1-7000-949e-43ecb1b5c08d";
        let global_scan_shape = format!(
            "{}{}",
            output,
            sandbox_record(&meta, "pts-10", &meta.launch_id, other)
        );
        assert!(select_omp_session_in_container(
            global_scan_shape.as_bytes(),
            &meta,
            &HashSet::new()
        )
        .is_err());
    }

    #[test]
    fn sandbox_accepts_marker_selected_breadcrumb_without_mtime_proof() {
        let meta = metadata(Path::new("/root/.omp/agent"), 100_000);
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let output = sandbox_materialized_record(&meta, "pts-9", id);
        let captured =
            select_omp_session_in_container(output.as_bytes(), &meta, &HashSet::new()).unwrap();
        assert_eq!(captured, id);
    }

    #[test]
    fn sandbox_accepts_historical_transition_after_initial_capture() {
        let meta = metadata(Path::new("/root/.omp/agent"), 100_000);
        let first = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let historical = "019fc9df-34e1-7000-949e-43ecb1b5c08d";
        let initial = select_omp_session_in_container(
            sandbox_materialized_record(&meta, "pts-9", first).as_bytes(),
            &meta,
            &HashSet::new(),
        )
        .unwrap();
        let resumed = select_omp_session_in_container(
            sandbox_materialized_record(&meta, "pts-9", historical).as_bytes(),
            &meta,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(initial, first);
        assert_eq!(resumed, historical);
    }

    #[test]
    fn dollar_scanner_preserves_reject_and_expand_semantics() {
        let env = HashMap::from([
            ("FOO".to_string(), "f".to_string()),
            ("PI_CONFIG_DIR".to_string(), "c".to_string()),
        ]);
        // (input, has_nonrouting_reference, expand_dotenv_value)
        let cases = [
            ("$123", false, "$123"),          // digit-first: lone $, verbatim
            ("\\$FOO", false, "$FOO"),        // escaped dollar
            ("${FOO}", true, "f"),            // valid non-routing braced ref
            ("$FOO/x", true, "f/x"),          // valid non-routing bare ref
            ("${PI_CONFIG_DIR}", false, "c"), // routing ref: safe, still expands
            ("$PI_CONFIG_DIR", false, "c"),   // bare routing ref: safe, still expands
            ("é$FOO", true, "éf"),            // multi-byte prefix: no boundary panic
            ("${A B}", false, "${A B}"),      // invalid braced key: verbatim
            ("${FOO", false, "${FOO"),        // unterminated brace: verbatim
            ("$", false, "$"),                // lone trailing dollar
            ("plain", false, "plain"),        // no dollars
        ];
        for (input, reject, expanded) in cases {
            assert_eq!(has_nonrouting_reference(input), reject, "detect {input:?}");
            assert_eq!(
                expand_dotenv_value(input, &env),
                expanded,
                "expand {input:?}"
            );
        }
    }
}
