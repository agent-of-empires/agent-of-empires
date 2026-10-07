//! OMP store layout resolution from launcher env, dotenv files and profiles.

use super::*;

/// Resolve OMP's host store and the native routing context used by the launch.
pub(crate) fn resolve_omp_store_layout(
    environment: &[String],
    launch_cwd: &str,
    options: &OmpCliCaptureOptions,
) -> Result<OmpStoreLayout> {
    resolve_omp_store_layout_with_environment(
        host_launcher_environment(environment),
        launch_cwd,
        options,
    )
    .map(|context| context.layout)
}

pub(crate) fn resolve_omp_store_layout_with_environment(
    launcher_env: HashMap<String, String>,
    launch_cwd: &str,
    options: &OmpCliCaptureOptions,
) -> Result<OmpResolvedContext> {
    let cwd = absolute_launch_cwd(launch_cwd)?;
    let routing_fingerprint = routing_fingerprint(&launcher_env);
    let launcher_routing = omp_routing_values(&launcher_env);
    let auto_env = autoload_bun_dotenv(launcher_env, &cwd, read_dotenv_content)?;
    let profile = resolve_profile(options.profile.as_deref(), &auto_env)?;
    let locations = dotenv_locations(&auto_env, &cwd, profile.as_deref())?;
    let files = locations
        .iter()
        .map(|path| {
            Ok(read_dotenv_content(path)?
                .map(|content| parse_dotenv(&content))
                .unwrap_or_default())
        })
        .collect::<Result<Vec<_>>>()?;
    let merged = merge_omp_environment(auto_env, &files);
    let (layout, agent_dir) =
        resolve_layout(&merged, &cwd, profile.as_deref(), options, Path::exists)?;
    Ok(OmpResolvedContext {
        layout,
        routing_fingerprint,
        launcher_routing,
        profile,
        cwd: omp_session_cwd(&cwd, options),
        agent_dir,
    })
}

pub(crate) fn resolve_omp_store_layout_in_container_with_environment(
    runtime: &crate::containers::RuntimeExecutionSnapshot,
    container_name: &str,
    container_cwd: &str,
    launcher_env: HashMap<String, String>,
    options: &OmpCliCaptureOptions,
) -> Result<OmpResolvedContext> {
    let cwd = absolute_launch_cwd(container_cwd)?;
    let routing_fingerprint = routing_fingerprint(&launcher_env);
    let launcher_routing = omp_routing_values(&launcher_env);
    if nonempty(&launcher_env, "HOME").is_none() {
        anyhow::bail!("OMP container has no HOME");
    }
    let auto_env = autoload_bun_dotenv(launcher_env, &cwd, |path| {
        read_container_dotenv_content(runtime, container_name, path)
    })?;
    let profile = resolve_profile(options.profile.as_deref(), &auto_env)?;
    let locations = dotenv_locations(&auto_env, &cwd, profile.as_deref())?;
    let files = locations
        .iter()
        .map(|path| read_container_dotenv(runtime, container_name, path))
        .collect::<Result<Vec<_>>>()?;
    let merged = merge_omp_environment(auto_env, &files);
    let agent_dir = managed_agent_dir(&merged, &cwd, profile.as_deref())?;
    let data_candidate = xdg_candidate(&merged, &cwd, "XDG_DATA_HOME", profile.as_deref());
    let state_candidate = xdg_candidate(&merged, &cwd, "XDG_STATE_HOME", profile.as_deref());
    let existence = probe_container_paths(
        runtime,
        container_name,
        [data_candidate.as_deref(), state_candidate.as_deref()],
    )?;
    let managed_sessions = data_candidate
        .as_ref()
        .filter(|_| existence[0])
        .unwrap_or(&agent_dir)
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
        terminal_sessions: state_candidate
            .as_ref()
            .filter(|_| existence[1])
            .unwrap_or(&agent_dir)
            .join("terminal-sessions"),
        kind: if custom.is_some() {
            OmpStoreKind::Custom
        } else {
            OmpStoreKind::Managed
        },
    };
    Ok(OmpResolvedContext {
        layout,
        routing_fingerprint,
        launcher_routing,
        profile,
        cwd: session_cwd,
        agent_dir,
    })
}

pub(super) fn resolve_layout(
    env: &HashMap<String, String>,
    cwd: &Path,
    profile: Option<&str>,
    options: &OmpCliCaptureOptions,
    mut exists: impl FnMut(&Path) -> bool,
) -> Result<(OmpStoreLayout, PathBuf)> {
    let session_cwd = omp_session_cwd(cwd, options);
    let agent_dir = managed_agent_dir(env, cwd, profile)?;
    let data_candidate = xdg_candidate(env, cwd, "XDG_DATA_HOME", profile);
    let state_candidate = xdg_candidate(env, cwd, "XDG_STATE_HOME", profile);
    let managed_sessions = data_candidate
        .as_ref()
        .filter(|path| exists(path))
        .unwrap_or(&agent_dir)
        .join("sessions");
    let terminal_sessions = state_candidate
        .as_ref()
        .filter(|path| exists(path))
        .unwrap_or(&agent_dir)
        .join("terminal-sessions");
    let custom = options
        .session_dir
        .as_deref()
        .or_else(|| nonempty(env, "PI_CODING_AGENT_SESSION_DIR").map(Path::new));
    Ok((
        OmpStoreLayout {
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
        },
        agent_dir,
    ))
}

pub(super) fn resolve_profile(
    cli_profile: Option<&str>,
    env: &HashMap<String, String>,
) -> Result<Option<String>> {
    let raw = cli_profile
        .map(str::to_string)
        .or_else(|| env.get("OMP_PROFILE").cloned())
        .or_else(|| env.get("PI_PROFILE").cloned());
    normalize_profile(raw.as_deref())
}

pub(super) fn normalize_profile(raw: Option<&str>) -> Result<Option<String>> {
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

pub(super) fn dotenv_locations(
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

pub(crate) fn host_launcher_environment(entries: &[String]) -> HashMap<String, String> {
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
    // Absent and empty differ: OMP_PROFILE falls back to PI_PROFILE only when absent.
    values
}
/// Host pane routing env matching the resolver's snapshot; explicit unsets stop
/// tmux's server environment reviving stale values.
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

pub(super) fn autoload_bun_dotenv(
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

pub(super) fn routing_fingerprint(env: &HashMap<String, String>) -> String {
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

pub(super) fn merge_omp_environment(
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

pub(super) fn read_dotenv_content(path: &Path) -> Result<Option<String>> {
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
pub(super) fn open_regular_file_no_follow(path: &Path) -> Result<Option<std::fs::File>> {
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
pub(super) fn open_regular_file_no_follow(path: &Path) -> Result<Option<std::fs::File>> {
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

pub(super) fn parse_dotenv_line(line: &str) -> Option<(&str, String, bool)> {
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

pub(super) fn parse_dotenv(content: &str) -> HashMap<String, String> {
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

pub(super) fn apply_bun_dotenv(
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

pub(super) fn is_omp_routing_assignment(key: &str) -> bool {
    OMP_STORE_ENV_KEYS.contains(&key)
        || key.strip_prefix("OMP_").is_some_and(|suffix| {
            OMP_STORE_ENV_KEYS.iter().any(|candidate| {
                candidate
                    .strip_prefix("PI_")
                    .is_some_and(|candidate| candidate == suffix)
            })
        })
}

/// A token of the `\$` / `$VAR` / `${VAR}` grammar shared by detection and expansion.
pub(super) enum DotenvToken<'a> {
    /// Verbatim bytes: ordinary text, a dangling `$`, a `$<digit>` start, or an
    /// unterminated / invalid-key `${...}`.
    Literal(&'a str),
    /// A `\$` escape, expanding to a single `$`.
    EscapedDollar,
    /// A `$KEY` or `${KEY}` reference whose key is a valid env variable name.
    Reference(&'a str),
}

pub(super) fn dotenv_tokens(value: &str) -> impl Iterator<Item = DotenvToken<'_>> {
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
        // Stops only at ASCII bytes, so never inside a multi-byte char.
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

pub(super) fn has_nonrouting_reference(value: &str) -> bool {
    dotenv_tokens(value).any(
        |token| matches!(token, DotenvToken::Reference(key) if !OMP_STORE_ENV_KEYS.contains(&key)),
    )
}

pub(super) fn expand_dotenv_value(value: &str, env: &HashMap<String, String>) -> String {
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

pub(super) fn home_dir(env: &HashMap<String, String>, cwd: &Path) -> Result<PathBuf> {
    if let Some(home) = nonempty(env, "HOME") {
        return Ok(absolute_path(cwd, Path::new(home)));
    }
    dirs::home_dir()
        .map(|home| absolute_path(cwd, &home))
        .context("Cannot determine home directory")
}

pub(super) fn config_root(
    env: &HashMap<String, String>,
    cwd: &Path,
    home: &Path,
    profile: Option<&str>,
) -> PathBuf {
    let config = nonempty(env, "PI_CONFIG_DIR").unwrap_or(".omp");
    // Only the root is stripped, re-rooting under HOME like OMP's join; `..` is kept
    // because OMP keeps it.
    let relative: PathBuf = Path::new(config)
        .components()
        .filter(|component| !matches!(component, Component::Prefix(_) | Component::RootDir))
        .collect();
    let root = absolute_path(cwd, &home.join(relative));
    profile.map_or(root.clone(), |profile| root.join("profiles").join(profile))
}

pub(super) fn initial_agent_dir(
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

pub(super) fn absolute_launch_cwd(cwd: &str) -> Result<PathBuf> {
    let cwd = Path::new(cwd);
    if !cwd.is_absolute() {
        anyhow::bail!("OMP launch cwd is not absolute");
    }
    Ok(crate::git::template::lexical_normalize(cwd))
}
pub(super) fn omp_session_cwd(launch_cwd: &Path, options: &OmpCliCaptureOptions) -> PathBuf {
    options.cwd.as_deref().map_or_else(
        || launch_cwd.to_path_buf(),
        |cwd| absolute_path(launch_cwd, cwd),
    )
}

pub(super) fn managed_agent_dir(
    env: &HashMap<String, String>,
    cwd: &Path,
    profile: Option<&str>,
) -> Result<PathBuf> {
    let home = home_dir(env, cwd)?;
    let root = config_root(env, cwd, &home, profile);
    Ok(initial_agent_dir(env, cwd, &root, profile))
}

pub(super) fn xdg_candidate(
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

pub(super) fn nonempty<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

pub(super) fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    crate::git::template::lexical_normalize(&path)
}

pub(crate) fn read_container_environment(
    runtime: &crate::containers::RuntimeExecutionSnapshot,
    container_name: &str,
) -> Result<HashMap<String, String>> {
    let command = container_exec_command(container_name, None, Some(runtime), &["env", "-0"]);
    let output = super::super::run_with_timeout_limit(
        command,
        COMMAND_TIMEOUT,
        "container exec (native environment probe)",
        MAX_CONTAINER_ENV_BYTES,
    )?;
    let text = String::from_utf8(output).context("Container environment is not UTF-8")?;
    let mut values = HashMap::new();
    for line in text.split(char::from(0)) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if crate::session::environment::is_valid_env_key(key) {
            values.insert(key.to_string(), value.to_string());
        }
    }
    Ok(values)
}

pub(super) fn read_container_dotenv_content(
    runtime: &crate::containers::RuntimeExecutionSnapshot,
    container_name: &str,
    path: &Path,
) -> Result<Option<String>> {
    // Container-controlled paths permit a check/read race; bounded reads confer no host privileges.
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
        Some(runtime),
        &["sh", "-c", SCRIPT, "aoe-omp-dotenv", path],
    );
    let output = super::super::run_with_timeout_limit(
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

fn read_container_dotenv(
    runtime: &crate::containers::RuntimeExecutionSnapshot,
    container_name: &str,
    path: &Path,
) -> Result<HashMap<String, String>> {
    Ok(
        read_container_dotenv_content(runtime, container_name, path)?
            .map(|content| parse_dotenv(&content))
            .unwrap_or_default(),
    )
}

pub(super) fn probe_container_paths(
    runtime: &crate::containers::RuntimeExecutionSnapshot,
    container_name: &str,
    paths: [Option<&Path>; 2],
) -> Result<[bool; 2]> {
    const SCRIPT: &str = r#"for path do
  if [ -n "$path" ] && [ -e "$path" ]; then printf '1\n'; else printf '0\n'; fi
done"#;
    let path_values = paths.map(|path| path.and_then(Path::to_str).unwrap_or_default().to_string());
    let command = container_exec_command(
        container_name,
        None,
        Some(runtime),
        &[
            "sh",
            "-c",
            SCRIPT,
            "aoe-omp-paths",
            &path_values[0],
            &path_values[1],
        ],
    );
    let output = super::super::run_with_timeout_limit(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::EnvGuard;
    fn resolve_with_entries(
        entries: &[String],
        cwd: &str,
        options: &OmpCliCaptureOptions,
    ) -> Result<(OmpStoreLayout, String)> {
        resolve_omp_store_layout_with_environment(host_launcher_environment(entries), cwd, options)
            .map(|context| (context.layout, context.routing_fingerprint))
    }
    use serial_test::serial;

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
        let (pi_layout, absent_fingerprint) = resolve_with_entries(
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
        let (default_layout, empty_fingerprint) = resolve_with_entries(
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
        let (mode_layout, _) = resolve_with_entries(
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
        let (layout, fingerprint) = resolve_with_entries(
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
        let (layout, fingerprint) = resolve_with_entries(
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
            let error = resolve_with_entries(
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
        assert!(read_dotenv_content(&invalid).is_err());
        let unreadable = tmp.path().join("directory.env");
        std::fs::create_dir(&unreadable).unwrap();
        assert!(read_dotenv_content(&unreadable).is_err());
        let oversized = tmp.path().join("oversized.env");
        let oversized_file = std::fs::File::create(&oversized).unwrap();
        oversized_file
            .set_len((MAX_DOTENV_BYTES + 1) as u64)
            .unwrap();
        assert!(read_dotenv_content(&oversized).is_err());
        #[cfg(unix)]
        {
            let target = tmp.path().join("secret.env");
            let link = tmp.path().join("linked.env");
            std::fs::write(&target, "OMP_PROFILE=must-not-follow\n").unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(
                read_dotenv_content(&link).is_err(),
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
        let (data_only, _) =
            resolve_layout(&env, cwd, None, &OmpCliCaptureOptions::default(), |path| {
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
        let (profile, _) = resolve_layout(
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
        let (restored_default, _) =
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
        let (custom, _) = resolve_layout(&env, cwd, None, &custom_options, |path| {
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
