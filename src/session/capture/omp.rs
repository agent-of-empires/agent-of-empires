//! Oh My Pi (OMP) session capture.
//!
//! OMP attribution is based exclusively on terminal breadcrumbs. Store layout
//! and launch identity are persisted in tmux and reloaded on every poll so a
//! restarted process cannot stay attached to a superseded pane generation.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const OMP_STORE_ENV_KEYS: [&str; 8] = [
    "HOME",
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

/// Transient launch snapshot. Environment values are carried only until the
/// exact pane/docker exec is assembled and are never serialized.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct OmpCapturePlan {
    pub layout: OmpStoreLayout,
    pub launch_environment: Vec<(String, String)>,
    pub launch_id: String,
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
    pub initial_known: Option<String>,
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
            index += if omp_flag_consumes_next(arg, argv.get(index + 1).map(String::as_str)) {
                2
            } else {
                1
            };
        }
        Ok(parsed)
    }
}

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
    ];
    const VALUELESS_FLAGS: &[&str] = &[
        "--help",
        "--version",
        "--allow-home",
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
            Some(_) => unreachable!(),
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
/// Resolve OMP 17.2.9's host store. Bun's cwd dotenv autoload is applied
/// before profile selection, then OMP's four literal dotenv files are merged.
pub(crate) fn resolve_omp_store_layout(
    environment: &[String],
    launch_cwd: &str,
    options: &OmpCliCaptureOptions,
) -> Result<OmpStoreLayout> {
    resolve_omp_store_layout_with_environment(environment, launch_cwd, options)
        .map(|(layout, _)| layout)
}

/// Resolve the store and the routing-only environment that must be pinned to
/// the launch. The full launcher environment is used transiently for dotenv
/// expansion but is never returned or persisted.
pub(crate) fn resolve_omp_store_layout_with_environment(
    environment: &[String],
    launch_cwd: &str,
    options: &OmpCliCaptureOptions,
) -> Result<(OmpStoreLayout, Vec<(String, String)>)> {
    let cwd = absolute_launch_cwd(launch_cwd)?;
    let launcher_env = host_launcher_environment(environment);
    let auto_env = autoload_bun_dotenv(launcher_env, &cwd, |path| Ok(read_dotenv_content(path)))?;
    let profile = resolve_profile(options.profile.as_deref(), &auto_env)?;
    let locations = dotenv_locations(&auto_env, &cwd, profile.as_deref())?;
    let files = locations
        .iter()
        .map(|path| read_dotenv_file(path))
        .collect::<Vec<_>>();
    let merged = merge_omp_environment(auto_env, &files);
    let layout = resolve_layout(&merged, &cwd, profile.as_deref(), options, |path| {
        path.exists()
    })?;
    Ok((layout, routing_environment(&merged, profile.as_deref())))
}

/// Resolve OMP's store inside a private container using bounded probes. The
/// returned paths are container paths and are never resolved again by pollers.
pub(crate) fn resolve_omp_store_layout_in_container(
    container_name: &str,
    container_cwd: &str,
    launch_environment: &[(String, String)],
    options: &OmpCliCaptureOptions,
) -> Result<OmpStoreLayout> {
    resolve_omp_store_layout_in_container_with_environment(
        container_name,
        container_cwd,
        launch_environment,
        options,
    )
    .map(|(layout, _)| layout)
}

pub(crate) fn resolve_omp_store_layout_in_container_with_environment(
    container_name: &str,
    container_cwd: &str,
    launch_environment: &[(String, String)],
    options: &OmpCliCaptureOptions,
) -> Result<(OmpStoreLayout, Vec<(String, String)>)> {
    let cwd = absolute_launch_cwd(container_cwd)?;
    let mut launcher_env = read_container_environment(container_name)?;
    for (key, value) in launch_environment {
        launcher_env.insert(key.clone(), value.clone());
    }
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
    Ok((layout, routing_environment(&merged, profile.as_deref())))
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
    values
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
            apply_bun_dotenv(&content, &mut env, &protected);
        }
    }
    Ok(env)
}

fn routing_environment(
    env: &HashMap<String, String>,
    profile: Option<&str>,
) -> Vec<(String, String)> {
    OMP_STORE_ENV_KEYS
        .into_iter()
        .map(|key| {
            let value = if key == "OMP_PROFILE" {
                profile.unwrap_or("default").to_string()
            } else {
                env.get(key).cloned().unwrap_or_default()
            };
            (key.to_string(), value)
        })
        .collect()
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

fn read_dotenv_content(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(bytes).ok()
}

fn read_dotenv_file(path: &Path) -> HashMap<String, String> {
    read_dotenv_content(path)
        .map(|content| parse_dotenv(&content))
        .unwrap_or_default()
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

fn apply_bun_dotenv(content: &str, env: &mut HashMap<String, String>, protected: &HashSet<String>) {
    for (key, value, expand) in content.lines().filter_map(parse_dotenv_line) {
        if protected.contains(key) {
            continue;
        }
        let value = if expand {
            expand_dotenv_value(&value, env)
        } else {
            value
        };
        env.insert(key.to_string(), value);
    }
}

fn expand_dotenv_value(value: &str, env: &HashMap<String, String>) -> String {
    let mut expanded = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && bytes.get(index + 1) == Some(&b'$') {
            expanded.push('$');
            index += 2;
            continue;
        }
        if bytes[index] != b'$' {
            let ch = value[index..].chars().next().expect("valid char boundary");
            expanded.push(ch);
            index += ch.len_utf8();
            continue;
        }
        if bytes.get(index + 1) == Some(&b'{') {
            let Some(relative_end) = value[index + 2..].find('}') else {
                expanded.push('$');
                index += 1;
                continue;
            };
            let end = index + 2 + relative_end;
            let key = &value[index + 2..end];
            if crate::session::environment::is_valid_env_key(key) {
                if let Some(replacement) = env.get(key) {
                    expanded.push_str(replacement);
                }
            } else {
                expanded.push_str(&value[index..=end]);
            }
            index = end + 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            end += 1;
        }
        if end == start || bytes[start].is_ascii_digit() {
            expanded.push('$');
            index += 1;
            continue;
        }
        if let Some(replacement) = env.get(&value[start..end]) {
            expanded.push_str(replacement);
        }
        index = end;
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

fn read_container_environment(container_name: &str) -> Result<HashMap<String, String>> {
    let mut command = std::process::Command::new("docker");
    command.args(["exec", container_name, "env"]);
    let output = super::run_with_timeout(command, COMMAND_TIMEOUT, "docker exec (OMP env probe)")?;
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
    const SCRIPT: &str = r#"[ -r "$1" ] && cat "$1" 2>/dev/null || :"#;
    let mut command = std::process::Command::new("docker");
    command.args(["exec", container_name, "sh", "-c", SCRIPT, "aoe-omp-dotenv"]);
    command.arg(path);
    let output =
        super::run_with_timeout(command, COMMAND_TIMEOUT, "docker exec (OMP dotenv probe)")?;
    Ok(String::from_utf8(output).ok())
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
    let mut command = std::process::Command::new("docker");
    command.args([
        "exec",
        container_name,
        "sh",
        "-c",
        SCRIPT,
        "aoe-omp-paths",
        &path_values[0],
        &path_values[1],
    ]);
    let output = super::run_with_timeout(command, COMMAND_TIMEOUT, "docker exec (OMP path probe)")?;
    let text = String::from_utf8(output).context("OMP container path probe is not UTF-8")?;
    let mut lines = text.lines();
    let result = [lines.next() == Some("1"), lines.next() == Some("1")];
    if lines.next().is_some() {
        anyhow::bail!("OMP container path probe returned trailing data");
    }
    Ok(result)
}

/// Return the per-launch sandbox marker path. The marker is created immediately
/// before OMP exec and is not a session id or a resumable artifact.
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
    validate_capture_metadata(&metadata)?;
    Ok(metadata)
}

fn validate_capture_metadata(metadata: &OmpCaptureMetadata) -> Result<()> {
    validate_layout(&metadata.layout)?;
    if metadata.launched_at_ms == 0
        || metadata.launch_id.is_empty()
        || metadata.launch_id.contains('\r')
        || metadata.launch_id.contains('\n')
    {
        anyhow::bail!("OMP capture metadata has an invalid generation");
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OmpPollGeneration {
    launch_id: String,
    layout: OmpStoreLayout,
    tty: String,
}

#[derive(Debug, Default)]
struct OmpPollState {
    generation: Option<OmpPollGeneration>,
    known: Option<String>,
    established: bool,
}

impl OmpPollState {
    fn rebind_metadata(&mut self, metadata: &OmpCaptureMetadata) -> bool {
        if self.generation.as_ref().is_some_and(|generation| {
            generation.launch_id == metadata.launch_id && generation.layout == metadata.layout
        }) {
            return false;
        }
        self.generation = Some(OmpPollGeneration {
            launch_id: metadata.launch_id.clone(),
            layout: metadata.layout.clone(),
            tty: String::new(),
        });
        self.known = metadata.initial_known.clone();
        self.established = false;
        true
    }

    fn rebind(&mut self, metadata: &OmpCaptureMetadata, tty: &str) -> bool {
        let generation = OmpPollGeneration {
            launch_id: metadata.launch_id.clone(),
            layout: metadata.layout.clone(),
            tty: tty.to_string(),
        };
        if self.generation.as_ref() == Some(&generation) {
            return false;
        }
        self.generation = Some(generation);
        self.known = metadata.initial_known.clone();
        self.established = false;
        true
    }
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

fn has_store_shape(path: &Path, root: &Path, components: usize) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|relative| relative.components().count() == components)
}

fn validate_breadcrumb(
    layout: &OmpStoreLayout,
    breadcrumb: Breadcrumb<'_>,
    materialized_header: Option<(Option<String>, Option<String>)>,
    exclusion: &HashSet<String>,
) -> Result<String> {
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
    let materialized = materialized_header.is_some();
    if (!valid_store && !materialized)
        || session_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("jsonl")
    {
        anyhow::bail!("OMP breadcrumb does not point to an allowed session JSONL");
    }
    let session_id = super::extract_pi_uuid_from_filename(&session_path)
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
    } else if !breadcrumb.fresh {
        anyhow::bail!("OMP breadcrumb target is missing without a fresh marker");
    }
    Ok(session_id)
}

fn capture_omp_session_id_from_terminal(
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    terminal_id: &str,
    known_id: Option<&str>,
    established: bool,
) -> Result<String> {
    validate_layout(&metadata.layout)?;
    if terminal_id.is_empty() || matches!(terminal_id, "." | "..") || terminal_id.contains('/') {
        anyhow::bail!("Invalid OMP terminal id");
    }
    let breadcrumb_path = metadata.layout.terminal_sessions.join(terminal_id);
    let modified_ms = std::fs::metadata(&breadcrumb_path)
        .and_then(|metadata| metadata.modified())
        .map(crate::util::system_time_to_ms)
        .with_context(|| {
            format!(
                "Failed to stat OMP breadcrumb {}",
                breadcrumb_path.display()
            )
        })?;
    let content = std::fs::read_to_string(&breadcrumb_path).with_context(|| {
        format!(
            "Failed to read OMP breadcrumb {}",
            breadcrumb_path.display()
        )
    })?;
    let breadcrumb = parse_breadcrumb(&content)?;
    let session_path = if Path::new(breadcrumb.session_path).is_absolute() {
        PathBuf::from(breadcrumb.session_path)
    } else {
        absolute_path(
            Path::new(breadcrumb.cwd),
            Path::new(breadcrumb.session_path),
        )
    };
    let header = if session_path.is_file() {
        Some(
            super::extract_pi_header_fields(&session_path)
                .context("OMP session JSONL has no valid session header")?,
        )
    } else {
        None
    };
    let session_id = validate_breadcrumb(&metadata.layout, breadcrumb, header, exclusion)?;
    if !established
        && modified_ms <= metadata.launched_at_ms
        && known_id != Some(session_id.as_str())
    {
        anyhow::bail!("OMP breadcrumb was not rewritten after launch");
    }
    Ok(session_id)
}

/// Capture the OMP session owned by one exact host tmux pane.
pub(crate) fn capture_omp_session_id(
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    tmux_session_name: &str,
    known_id: Option<&str>,
) -> Result<String> {
    let (_, terminal_id) = tty_and_terminal_id_for_tmux(tmux_session_name)?;
    capture_omp_session_id_from_terminal(metadata, exclusion, &terminal_id, known_id, false)
}

/// Host poller. Every tick follows the current pane and the metadata generation
/// published in that pane's hidden tmux environment.
pub(crate) fn omp_poll_fn(
    instance_id: String,
    tmux_session_name: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    let state = Mutex::new(OmpPollState::default());
    move || {
        let metadata = load_omp_capture_metadata(&tmux_session_name)
            .and_then(|metadata| {
                let (tty, terminal_id) = tty_and_terminal_id_for_tmux(&tmux_session_name)?;
                Ok((metadata, tty, terminal_id))
            })
            .map_err(|error| {
                tracing::debug!(target: "session.capture", "OMP poll identity refresh failed: {}", error)
            })
            .ok()?;
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes);
        let mut state = state.lock().ok()?;
        state.rebind(&metadata.0, &metadata.1);
        let captured = capture_omp_session_id_from_terminal(
            &metadata.0,
            &exclusion,
            &metadata.2,
            state.known.as_deref(),
            state.established,
        )
        .map_err(|error| {
            tracing::debug!(target: "session.capture", "OMP poll capture failed: {}", error)
        })
        .ok()
        .and_then(super::validated_session_id);
        let refreshed = load_omp_capture_metadata(&tmux_session_name)
            .and_then(|metadata| {
                let (tty, terminal_id) = tty_and_terminal_id_for_tmux(&tmux_session_name)?;
                Ok((metadata, tty, terminal_id))
            })
            .ok()?;
        if refreshed != metadata {
            state.rebind(&refreshed.0, &refreshed.1);
            return None;
        }
        if let Some(id) = captured.as_ref() {
            state.known = Some(id.clone());
            state.established = true;
        }
        captured
    }
}

const CONTAINER_BREADCRUMB_SCRIPT: &str = r#"TERM_DIR=$1
LAUNCH_MARKER=$2
EXPECTED_LAUNCH=$3
[ -d "$TERM_DIR" ] || exit 0
[ -f "$LAUNCH_MARKER" ] || exit 0
marker_lines=$(wc -l < "$LAUNCH_MARKER" 2>/dev/null) || exit 0
[ "$marker_lines" = 2 ] || exit 0
terminal=$(sed -n '1p' "$LAUNCH_MARKER")
marker_launch=$(sed -n '2p' "$LAUNCH_MARKER")
case "$terminal" in ''|.|..|*[!A-Za-z0-9._-]*) exit 0 ;; esac
[ -n "$EXPECTED_LAUNCH" ] && [ "$marker_launch" = "$EXPECTED_LAUNCH" ] || exit 0
f="$TERM_DIR/$terminal"
[ -f "$f" ] || exit 0
newer=0
[ "$f" -nt "$LAUNCH_MARKER" ] && newer=1
cwd=$(sed -n '1p' "$f")
session_path=$(sed -n '2p' "$f")
marker=$(sed -n '3p' "$f")
full_path=$session_path
case "$full_path" in /*) ;; *) full_path="$cwd/$full_path" ;; esac
exists=0
header=
if [ -f "$full_path" ]; then
  exists=1
  header=$(head -n 8 "$full_path" | grep -m1 '^{"type":"session"')
fi
terminal_after=$(sed -n '1p' "$LAUNCH_MARKER")
launch_after=$(sed -n '2p' "$LAUNCH_MARKER")
[ "$terminal_after" = "$terminal" ] && [ "$launch_after" = "$marker_launch" ] || exit 0
printf '===OMP===\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n===END===\n' \
  "$terminal" "$marker_launch" "$newer" "$cwd" "$session_path" "$marker" "$exists" "$header""#;

#[derive(Clone, Debug)]
struct ContainerCandidate {
    id: String,
    terminal_id: String,
    newer_than_marker: bool,
}

fn select_omp_session_in_container(
    stdout: &[u8],
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
) -> Result<ContainerCandidate> {
    let text = std::str::from_utf8(stdout).context("OMP container capture is not UTF-8")?;
    let body = text
        .strip_prefix("===OMP===\n")
        .and_then(|text| text.strip_suffix("\n===END===\n"))
        .context("No valid OMP terminal breadcrumb found in container")?;
    let mut fields = body.split('\n');
    let (
        Some(terminal_id),
        Some(marker_launch),
        Some(newer),
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
        fields.next(),
    )
    else {
        anyhow::bail!("Malformed OMP terminal breadcrumb response");
    };
    if fields.next().is_some()
        || !valid_omp_terminal_id(terminal_id)
        || marker_launch != metadata.launch_id.as_str()
        || !matches!(newer, "0" | "1")
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
    let id = validate_breadcrumb(&metadata.layout, breadcrumb, parsed_header, exclusion)?;
    Ok(ContainerCandidate {
        id,
        terminal_id: terminal_id.to_string(),
        newer_than_marker: newer == "1",
    })
}

fn establish_sandbox_candidate(
    state: &mut OmpPollState,
    metadata: &OmpCaptureMetadata,
    candidate: ContainerCandidate,
) -> Result<String> {
    state.rebind(metadata, &candidate.terminal_id);
    if !state.established && !candidate.newer_than_marker {
        anyhow::bail!("OMP container breadcrumb predates its launch marker");
    }
    state.known = Some(candidate.id.clone());
    state.established = true;
    Ok(candidate.id)
}

fn capture_omp_session_in_container(
    container_name: &str,
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    launch_marker: &str,
) -> Result<ContainerCandidate> {
    validate_capture_metadata(metadata)?;
    let terminals = metadata
        .layout
        .terminal_sessions
        .to_str()
        .context("OMP container terminal path is not UTF-8")?;
    if launch_marker.is_empty() {
        anyhow::bail!("OMP sandbox launch marker is unavailable");
    }
    let mut command = std::process::Command::new("docker");
    command.args([
        "exec",
        container_name,
        "sh",
        "-c",
        CONTAINER_BREADCRUMB_SCRIPT,
        "aoe-omp-capture",
        terminals,
        launch_marker,
        &metadata.launch_id,
    ]);
    let output = super::run_with_timeout(
        command,
        COMMAND_TIMEOUT,
        "docker exec (OMP breadcrumb capture)",
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
    let candidate = capture_omp_session_in_container(
        container_name,
        metadata,
        exclusion,
        launch_marker.context("OMP sandbox launch marker is unavailable")?,
    )?;
    if !candidate.newer_than_marker {
        anyhow::bail!("OMP container breadcrumb was not rewritten after its launch marker");
    }
    Ok(candidate.id)
}

/// Sandbox poller. Every tick reloads the tmux generation, then the marker
/// selects the one and only terminal breadcrumb that this launch may own.
pub(crate) fn omp_poll_fn_sandboxed(
    container_name: String,
    instance_id: String,
    tmux_session_name: String,
    launch_marker: Option<String>,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    let state = Mutex::new(OmpPollState::default());
    move || {
        let metadata = load_omp_capture_metadata(&tmux_session_name)
            .map_err(|error| {
                tracing::debug!(target: "session.capture", "OMP container poll metadata refresh failed: {}", error)
            })
            .ok()?;
        state.lock().ok()?.rebind_metadata(&metadata);
        let marker = launch_marker.as_deref()?;
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes);
        let candidate =
            capture_omp_session_in_container(&container_name, &metadata, &exclusion, marker)
                .map_err(|error| {
                    tracing::debug!(target: "session.capture", "OMP container poll capture failed: {}", error)
                })
                .ok()?;
        let refreshed = load_omp_capture_metadata(&tmux_session_name).ok()?;
        if refreshed != metadata {
            state.lock().ok()?.rebind_metadata(&refreshed);
            return None;
        }
        let mut state = state.lock().ok()?;
        establish_sandbox_candidate(&mut state, &metadata, candidate)
            .map_err(|error| {
                tracing::debug!(target: "session.capture", "OMP container poll identity rejected: {}", error)
            })
            .ok()
            .and_then(super::validated_session_id)
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
            initial_known: None,
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
        let (layout, routing) = resolve_omp_store_layout_with_environment(
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
        assert!(routing.contains(&("OMP_PROFILE".to_string(), "local".to_string())));
        assert_eq!(routing.len(), OMP_STORE_ENV_KEYS.len());
        assert!(routing.contains(&("PI_CODING_AGENT_DIR".to_string(), String::new())));
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
    fn bun_cwd_dotenv_expands_full_launcher_environment_and_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(".env.local"),
            "PI_CODING_AGENT_DIR='$HOME/${ROUTE}/\\$ROUTE'\n",
        )
        .unwrap();
        let _env = EnvGuard::unset(&OMP_STORE_ENV_KEYS);
        let (layout, routing) = resolve_omp_store_layout_with_environment(
            &[
                format!("HOME={}", home.display()),
                "ROUTE=expanded".to_string(),
            ],
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        assert_eq!(layout.sessions, home.join("expanded/$ROUTE/sessions"));
        assert!(routing
            .iter()
            .all(|(key, _)| OMP_STORE_ENV_KEYS.contains(&key.as_str())));
        assert!(!routing.iter().any(|(key, _)| key == "ROUTE"));
    }

    #[test]
    #[serial]
    fn large_dotenv_is_loaded_and_unreadable_or_invalid_files_are_empty() {
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
        assert!(read_dotenv_file(&invalid).is_empty());
        let unreadable = tmp.path().join("directory.env");
        std::fs::create_dir(&unreadable).unwrap();
        assert!(read_dotenv_file(&unreadable).is_empty());
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
        assert_eq!(
            validate_breadcrumb(
                &layout,
                Breadcrumb {
                    cwd: cwd.to_str().unwrap(),
                    session_path: resumed.to_str().unwrap(),
                    fresh: false,
                },
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
            assert!(validate_breadcrumb(
                &meta.layout,
                Breadcrumb {
                    cwd: cwd.to_str().unwrap(),
                    session_path: path.to_str().unwrap(),
                    fresh: true,
                },
                None,
                &HashSet::new(),
            )
            .is_err());
        }
        assert!(validate_breadcrumb(
            &meta.layout,
            Breadcrumb {
                cwd: cwd.to_str().unwrap(),
                session_path: "relative.jsonl",
                fresh: true,
            },
            None,
            &HashSet::new(),
        )
        .is_err());
        assert!(validate_breadcrumb(
            &meta.layout,
            Breadcrumb {
                cwd: cwd.to_str().unwrap(),
                session_path: valid.to_str().unwrap(),
                fresh: true,
            },
            None,
            &HashSet::from([id.to_string()]),
        )
        .is_err());
    }

    #[test]
    fn materialized_breadcrumb_accepts_cross_project_but_requires_header_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 0);
        let historical = tmp.path().join("historical");
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let session = tmp
            .path()
            .join(format!("external/2025-01-01T00-00-00-000Z_{id}.jsonl"));
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
        write_breadcrumb(&meta, "pts-1", &historical, &session, false);
        assert_eq!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1", None, false,)
                .unwrap(),
            id
        );
        std::fs::write(
            &session,
            format!("{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"/wrong\"}}\n"),
        )
        .unwrap();
        assert!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1", None, false,)
                .is_err()
        );
    }

    #[test]
    fn first_historical_capture_uses_breadcrumb_rewrite_not_jsonl_name() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 150_000);
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
        let crumb = write_breadcrumb(&meta, "pts-1", &cwd, &session, false);
        set_mtime_ms(&crumb, 200_000);
        assert_eq!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1", None, false,)
                .unwrap(),
            id
        );
        set_mtime_ms(&crumb, 100_000);
        assert!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1", None, false,)
                .is_err()
        );
        assert_eq!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-1", Some(id), false,)
                .unwrap(),
            id
        );
    }

    #[test]
    fn fresh_only_permits_an_absent_target() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 0);
        let cwd = tmp.path().join("project");
        let bucket = meta.layout.sessions.join("bucket");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&bucket).unwrap();
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let session = bucket.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        write_breadcrumb(&meta, "fresh", &cwd, &session, true);
        assert_eq!(
            capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "fresh", None, false,)
                .unwrap(),
            id
        );
        write_breadcrumb(&meta, "not-fresh", &cwd, &session, false);
        assert!(capture_omp_session_id_from_terminal(
            &meta,
            &HashSet::new(),
            "not-fresh",
            None,
            false,
        )
        .is_err());
    }
    #[cfg(unix)]
    #[test]
    fn container_script_reads_only_the_marker_terminal() {
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
        std::fs::write(&marker, format!("pts-9\n{}\n", meta.launch_id)).unwrap();
        set_mtime_ms(&marker, 100_000);
        let breadcrumb = meta.layout.terminal_sessions.join("pts-9");
        std::fs::write(&breadcrumb, format!("{cwd}\n{}\n", session.display())).unwrap();
        set_mtime_ms(&breadcrumb, 101_000);
        std::fs::write(
            meta.layout.terminal_sessions.join("pts-decoy"),
            format!("{cwd}\n{}\nfresh\n", session.display()),
        )
        .unwrap();

        let output = std::process::Command::new("sh")
            .args([
                "-c",
                CONTAINER_BREADCRUMB_SCRIPT,
                "aoe-omp-test",
                meta.layout.terminal_sessions.to_str().unwrap(),
                marker.to_str().unwrap(),
                &meta.launch_id,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let candidate =
            select_omp_session_in_container(&output.stdout, &meta, &HashSet::new()).unwrap();
        assert_eq!(candidate.id, id);
        assert_eq!(candidate.terminal_id, "pts-9");
        assert!(candidate.newer_than_marker);
    }

    fn sandbox_record(
        metadata: &OmpCaptureMetadata,
        terminal: &str,
        launch_id: &str,
        newer: bool,
        id: &str,
    ) -> String {
        format!(
            "===OMP===\n{terminal}\n{launch_id}\n{}\n/workspace/project\n{}/bucket/2026-01-01T00-00-00-000Z_{id}.jsonl\nfresh\n0\n\n===END===\n",
            u8::from(newer),
            metadata.layout.sessions.display()
        )
    }

    fn sandbox_materialized_record(
        metadata: &OmpCaptureMetadata,
        terminal: &str,
        newer: bool,
        id: &str,
    ) -> String {
        format!(
            "===OMP===\n{terminal}\n{}\n{}\n/workspace/project\n{}/bucket/2025-01-01T00-00-00-000Z_{id}.jsonl\n\n1\n{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"/workspace/project\"}}\n===END===\n",
            metadata.launch_id,
            u8::from(newer),
            metadata.layout.sessions.display()
        )
    }

    #[test]
    fn host_state_rebinds_on_metadata_generation_layout_or_exact_tty() {
        let mut first = metadata(Path::new("/root/.omp/agent"), 100_000);
        first.initial_known = Some("old-known".to_string());
        let mut state = OmpPollState::default();
        assert!(state.rebind(&first, "/dev/pts/1"));
        assert_eq!(state.known.as_deref(), Some("old-known"));
        state.known = Some("captured".to_string());
        state.established = true;
        assert!(!state.rebind(&first, "/dev/pts/1"));
        assert!(state.established);

        let mut relaunched = first.clone();
        relaunched.launch_id = "launch-b".to_string();
        relaunched.initial_known = Some("restart-known".to_string());
        assert!(state.rebind(&relaunched, "/dev/pts/1"));
        assert!(!state.established);
        assert_eq!(state.known.as_deref(), Some("restart-known"));

        state.established = true;
        assert!(state.rebind(&relaunched, "/dev/pts/2"));
        assert!(!state.established);

        state.established = true;
        let mut moved = metadata(Path::new("/other/.omp/agent"), 100_000);
        moved.launch_id = relaunched.launch_id.clone();
        assert!(state.rebind(&moved, "/dev/pts/2"));
        assert!(!state.established);
    }

    #[test]
    fn sandbox_rejects_marker_from_a_newer_launch_generation() {
        let meta = metadata(Path::new("/root/.omp/agent"), 100_000);
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let output = sandbox_record(&meta, "pts-9", "launch-b", true, id);
        assert!(
            select_omp_session_in_container(output.as_bytes(), &meta, &HashSet::new()).is_err()
        );
    }

    #[test]
    fn sandbox_marker_selects_exactly_one_terminal() {
        let meta = metadata(Path::new("/root/.omp/agent"), 100_000);
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let output = sandbox_record(&meta, "pts-9", &meta.launch_id, true, id);
        let selected =
            select_omp_session_in_container(output.as_bytes(), &meta, &HashSet::new()).unwrap();
        assert_eq!(selected.id, id);
        assert_eq!(selected.terminal_id, "pts-9");

        let other = "019fc9df-34e1-7000-949e-43ecb1b5c08d";
        let global_scan_shape = format!(
            "{}{}",
            output,
            sandbox_record(&meta, "pts-10", &meta.launch_id, true, other)
        );
        assert!(select_omp_session_in_container(
            global_scan_shape.as_bytes(),
            &meta,
            &HashSet::new()
        )
        .is_err());
    }

    #[test]
    fn sandbox_refuses_known_breadcrumb_that_predates_marker() {
        let mut meta = metadata(Path::new("/root/.omp/agent"), 100_000);
        let id = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        meta.initial_known = Some(id.to_string());
        let output = sandbox_materialized_record(&meta, "pts-9", false, id);
        let candidate =
            select_omp_session_in_container(output.as_bytes(), &meta, &HashSet::new()).unwrap();
        let mut state = OmpPollState::default();
        assert!(establish_sandbox_candidate(&mut state, &meta, candidate).is_err());
        assert_eq!(state.known.as_deref(), Some(id));
        assert!(!state.established);
    }

    #[test]
    fn sandbox_accepts_historical_transition_after_terminal_established() {
        let meta = metadata(Path::new("/root/.omp/agent"), 100_000);
        let first = "019fc9a0-f688-7000-ae45-d9e51e5e1b8a";
        let historical = "019fc9df-34e1-7000-949e-43ecb1b5c08d";
        let initial = select_omp_session_in_container(
            sandbox_record(&meta, "pts-9", &meta.launch_id, true, first).as_bytes(),
            &meta,
            &HashSet::new(),
        )
        .unwrap();
        let resumed = select_omp_session_in_container(
            sandbox_materialized_record(&meta, "pts-9", false, historical).as_bytes(),
            &meta,
            &HashSet::new(),
        )
        .unwrap();
        let mut state = OmpPollState::default();
        assert_eq!(
            establish_sandbox_candidate(&mut state, &meta, initial).unwrap(),
            first
        );
        assert_eq!(
            establish_sandbox_candidate(&mut state, &meta, resumed).unwrap(),
            historical
        );
        assert!(state.established);
    }
}
