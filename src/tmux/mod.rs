//! tmux integration module

pub(crate) mod env;
mod session;
pub mod status_bar;
pub(crate) mod status_detection;
mod terminal_session;
#[cfg(test)]
mod test_helpers;
mod tool_session;
pub(crate) mod utils;

pub use session::{PaneCursor, Session};
pub use status_bar::{get_session_info_for_current, get_status_for_current_session};
pub use status_detection::detect_status_from_content;
pub(crate) use status_detection::{reconcile_claude_hook_status, reconcile_codex_hook_status};
pub use terminal_session::{ContainerTerminalSession, TerminalSession};
pub use tool_session::{kill_all_tool_sessions_for_id, ToolSession};
pub use utils::tmux_prefix_display;

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    pub use super::env::{
        get_hidden_env, get_hidden_env_batch, remove_hidden_env, set_hidden_env,
        set_hidden_env_batch, AOE_CAPTURED_SESSION_ID_KEY, AOE_INSTANCE_ID_KEY,
    };
}

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::RwLock;
use std::time::{Duration, Instant};

// Debug builds use `aoe_dev_*` prefixes so `cargo run` and an installed
// release `aoe` can coexist on the same tmux server without seeing each
// other's sessions.
pub const SESSION_PREFIX: &str = if cfg!(debug_assertions) {
    "aoe_dev_"
} else {
    "aoe_"
};
pub const TERMINAL_PREFIX: &str = if cfg!(debug_assertions) {
    "aoe_dev_term_"
} else {
    "aoe_term_"
};
pub const CONTAINER_TERMINAL_PREFIX: &str = if cfg!(debug_assertions) {
    "aoe_dev_cterm_"
} else {
    "aoe_cterm_"
};
pub const TOOL_PREFIX: &str = if cfg!(debug_assertions) {
    "aoe_dev_tool_"
} else {
    "aoe_tool_"
};

/// Pre-fetched pane metadata from a single `tmux list-panes -a` call.
#[derive(Debug, Clone)]
pub struct PaneMetadata {
    pub pane_dead: bool,
    pub pane_current_command: Option<String>,
}

static SESSION_CACHE: RwLock<SessionCache> = RwLock::new(SessionCache {
    data: None,
    time: None,
});

struct SessionCache {
    data: Option<HashMap<String, i64>>,
    time: Option<Instant>,
}

// Field separator for multi-field tmux `-F` format strings. Must be a
// printable ASCII byte that does not appear in `sanitize_session_name` output
// (which preserves `[A-Za-z0-9_-]` and replaces everything else with `_`).
// tmux 3.4 mangles whitespace (tab, newline become `_`) and octal-escapes
// control bytes (ASCII 0x1F is emitted as the literal 4-char sequence
// `\037`), so anything non-printable is unreliable. Pipe is safe.
const FIELD_SEP: char = '|';

pub fn refresh_session_cache() {
    let start = Instant::now();
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}|#{session_activity}"])
        .output();

    let new_data = match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut map = HashMap::new();
            for line in stdout.lines() {
                if let Some((name, activity)) = line.split_once(FIELD_SEP) {
                    let activity: i64 = activity.parse().unwrap_or(0);
                    map.insert(name.to_string(), activity);
                }
            }
            Some(map)
        }
        Ok(out) => {
            tracing::warn!(
                target: "tmux.cache",
                status = ?out.status,
                stderr_bytes = out.stderr.len(),
                "list-sessions returned non-zero; cache cleared",
            );
            None
        }
        Err(e) => {
            tracing::warn!(target: "tmux.cache", error = %e, "list-sessions spawn failed; cache cleared");
            None
        }
    };

    // Trace, not debug: the TUI status poller calls this every ~2s, so
    // at debug it dominates the idle log. Errors above still log at warn.
    let sessions = new_data.as_ref().map(|m| m.len()).unwrap_or(0);
    tracing::trace!(
        target: "tmux.cache",
        sessions,
        duration_ms = start.elapsed().as_millis() as u64,
        "session cache refreshed",
    );

    if let Ok(mut cache) = SESSION_CACHE.write() {
        cache.data = new_data;
        cache.time = Some(Instant::now());
    }
}

/// Batch-fetch pane metadata for all aoe sessions in a single tmux subprocess call.
/// Returns a map from session name to metadata for the first window's first pane.
///
/// Returns `Err` when the underlying `tmux list-panes` call fails to spawn or
/// exits non-zero. Callers MUST distinguish this from `Ok(map)` where a missing
/// key means the session is genuinely absent: `Err` means we don't know.
/// Startup recovery treats `Err` as "skip this pass" to avoid killing a
/// possibly-live pane on a transient tmux glitch; status pollers treat it as
/// `unwrap_or_default()` because their semantics are unchanged by an empty map.
pub fn batch_pane_metadata() -> anyhow::Result<HashMap<String, PaneMetadata>> {
    let start = Instant::now();
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}|#{pane_index}|#{pane_dead}|#{pane_current_command}",
        ])
        .output();

    let result: anyhow::Result<HashMap<String, PaneMetadata>> = match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            Ok(parse_pane_metadata(&stdout))
        }
        Ok(out) => {
            tracing::warn!(
                target: "tmux.pane",
                status = ?out.status,
                stderr_bytes = out.stderr.len(),
                "list-panes returned non-zero",
            );
            Err(anyhow::anyhow!(
                "tmux list-panes returned non-zero status: {:?}",
                out.status
            ))
        }
        Err(e) => {
            tracing::warn!(target: "tmux.pane", error = %e, "list-panes spawn failed");
            Err(anyhow::anyhow!("tmux list-panes spawn failed: {}", e))
        }
    };

    // Trace, not debug: paired with refresh_session_cache in the TUI
    // status poll loop (~every 2s). Debug-level here would dominate the
    // idle log.
    tracing::trace!(
        target: "tmux.pane",
        sessions = result.as_ref().map(|m| m.len()).unwrap_or(0),
        duration_ms = start.elapsed().as_millis() as u64,
        "batch pane metadata fetched",
    );
    result
}

/// Names of aoe tmux sessions that currently have at least one attached
/// client, from a single `tmux list-sessions` call.
///
/// Used by the idle auto-stop reapers (#1690) to spare a session the user is
/// reading. Returns `Err` when the underlying tmux call fails to spawn or
/// exits non-zero: callers MUST treat `Err` as "don't know, skip this reap
/// pass" rather than "nothing attached", so a transient tmux glitch cannot
/// kill a pane the user is sitting in.
pub fn attached_session_names() -> anyhow::Result<HashSet<String>> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}|#{session_attached}"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut attached = HashSet::new();
            for line in stdout.lines() {
                if let Some((name, flag)) = line.split_once(FIELD_SEP) {
                    // `#{session_attached}` is the attached client count; any
                    // non-zero value means a client is attached.
                    if name.starts_with(SESSION_PREFIX) && flag.trim() != "0" {
                        attached.insert(name.to_string());
                    }
                }
            }
            Ok(attached)
        }
        Ok(out) => {
            tracing::warn!(
                target: "tmux.cache",
                status = ?out.status,
                "list-sessions (attached) returned non-zero",
            );
            Err(anyhow::anyhow!(
                "tmux list-sessions returned non-zero status: {:?}",
                out.status
            ))
        }
        Err(e) => {
            tracing::warn!(target: "tmux.cache", error = %e, "list-sessions (attached) spawn failed");
            Err(anyhow::anyhow!("tmux list-sessions spawn failed: {}", e))
        }
    }
}

/// Parse the output of `tmux list-panes -a` into a map of session name to pane metadata.
/// Filters to aoe sessions, pane index 0, and takes only the first window per session.
fn parse_pane_metadata(output: &str) -> HashMap<String, PaneMetadata> {
    let mut map = HashMap::new();

    for line in output.lines() {
        let parts: Vec<&str> = line.split(FIELD_SEP).collect();
        if parts.len() < 4 {
            continue;
        }

        let session_name = parts[0];
        if !session_name.starts_with(SESSION_PREFIX) {
            continue;
        }

        // Only take pane 0 (the agent pane). aoe pins pane-base-index to 0.
        if parts[1] != "0" {
            continue;
        }

        // First occurrence per session = first window's pane 0 (list-panes
        // returns windows in index order).
        if map.contains_key(session_name) {
            continue;
        }

        map.insert(
            session_name.to_string(),
            PaneMetadata {
                pane_dead: parts[2] == "1",
                pane_current_command: if parts[3].is_empty() {
                    None
                } else {
                    Some(parts[3].to_string())
                },
            },
        );
    }

    map
}

/// Test-only: inject a synthetic session name into the cache so
/// callers of `session_exists_from_cache` see it as present. Used
/// by live-send tests that install a fake `LiveSendState` without a
/// real tmux pane; without this the per-keystroke drift check
/// (which calls `session_exists_from_cache`) trips in CI runs that
/// have already populated the cache via the e2e suite, causing the
/// drift detector to flag the fake session as gone.
#[cfg(test)]
pub fn test_inject_session_into_cache(name: &str) {
    if let Ok(mut cache) = SESSION_CACHE.write() {
        let map = cache.data.get_or_insert_with(HashMap::new);
        map.insert(name.to_string(), 0);
        cache.time = Some(Instant::now());
    }
}

pub fn session_exists_from_cache(name: &str) -> Option<bool> {
    let cache = SESSION_CACHE.read().ok()?;

    // Cache valid for 2 seconds
    if cache
        .time
        .map(|t| t.elapsed() > Duration::from_secs(2))
        .unwrap_or(true)
    {
        return None;
    }

    cache.data.as_ref().map(|m| m.contains_key(name))
}

pub fn get_current_session_name() -> Option<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .ok()?;

    if output.status.success() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

pub fn is_tmux_available() -> bool {
    Command::new("tmux").arg("-V").output().is_ok()
}

/// True when `binary` resolves on the user's PATH. An absolute or relative
/// path is checked for existence; a bare name is looked up with `which`,
/// falling back to a login shell so version-manager PATHs (NVM, etc.) are
/// loaded. Shared by `is_agent_available` and the `aoe add` override
/// availability check so both honor the same detection. See #1910.
pub(crate) fn is_binary_on_path(binary: &str) -> bool {
    if binary.contains('/') || binary.contains('\\') {
        return std::path::Path::new(binary).exists();
    }
    // First try direct `which` (fast path).
    let direct = Command::new("which")
        .arg(binary)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if direct {
        return true;
    }
    // Fall back to a login shell so version-manager PATHs (NVM, etc.) are loaded.
    let shell = crate::session::user_shell();
    Command::new(&shell)
        .args(["-lc", &format!("which {}", shell_words::quote(binary))])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub(crate) fn is_agent_available(agent: &crate::agents::AgentDef) -> bool {
    use crate::agents::DetectionMethod;
    match &agent.detection {
        DetectionMethod::Which(binary) => is_binary_on_path(binary),
        DetectionMethod::RunWithArg(binary, arg) => {
            if Command::new(binary)
                .arg(arg)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                return true;
            }
            let shell = crate::session::user_shell();
            Command::new(&shell)
                .args(["-lc", &format!("{} {}", binary, arg)])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
    }
}

#[derive(Debug, Clone)]
pub struct AvailableTools {
    available: Vec<String>,
}

impl AvailableTools {
    pub fn detect() -> Self {
        let mut available: Vec<String> = crate::agents::AGENTS
            .iter()
            .filter(|a| is_agent_available(a))
            .map(|a| a.name.to_string())
            .collect();

        // Append user-defined custom agents (always considered available since the
        // command may target a remote host or a wrapper script).
        if let Ok(config) = crate::session::config::Config::load() {
            config.session.warn_custom_agent_issues();
            let mut custom: Vec<_> = config
                .session
                .custom_agents
                .keys()
                .filter(|name| !name.is_empty() && !available.iter().any(|n| n == *name))
                .cloned()
                .collect();
            custom.sort();
            available.extend(custom);
        }

        Self { available }
    }

    pub fn any_available(&self) -> bool {
        !self.available.is_empty()
    }

    pub fn available_list(&self) -> &[String] {
        &self.available
    }

    #[cfg(test)]
    pub fn with_tools(tools: &[&str]) -> Self {
        Self {
            available: tools.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::TmuxTestSession;
    use super::*;

    // Session names embed `SESSION_PREFIX`, which differs between release
    // (`aoe_`) and debug (`aoe_dev_`) builds. Use the constant so the same
    // test bodies cover both.
    const P: &str = SESSION_PREFIX;

    #[test]
    fn test_parse_pane_metadata_basic() {
        let output = format!("{P}my_proj_abc12345|0|0|claude\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        let meta = map.get(&format!("{P}my_proj_abc12345")).unwrap();
        assert!(!meta.pane_dead);
        assert_eq!(meta.pane_current_command.as_deref(), Some("claude"));
    }

    #[test]
    fn test_parse_pane_metadata_dead_pane() {
        let output = format!("{P}proj_abc12345|0|1|bash\n");
        let map = parse_pane_metadata(&output);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert!(meta.pane_dead);
    }

    #[test]
    fn test_parse_pane_metadata_filters_non_aoe_sessions() {
        let output =
            format!("user_session|0|0|bash\n{P}proj_abc12345|0|0|claude\nmy_tmux|0|0|vim\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&format!("{P}proj_abc12345")));
    }

    #[test]
    fn test_parse_pane_metadata_filters_non_zero_panes() {
        let output = format!("{P}proj_abc12345|0|0|claude\n{P}proj_abc12345|1|0|bash\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert_eq!(meta.pane_current_command.as_deref(), Some("claude"));
    }

    #[test]
    fn test_parse_pane_metadata_first_window_wins() {
        // Two windows both have pane 0, first window's data should be kept
        let output = format!("{P}proj_abc12345|0|0|claude\n{P}proj_abc12345|0|1|bash\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert!(!meta.pane_dead);
        assert_eq!(meta.pane_current_command.as_deref(), Some("claude"));
    }

    #[test]
    fn test_parse_pane_metadata_empty_output() {
        assert!(parse_pane_metadata("").is_empty());
    }

    #[test]
    fn test_parse_pane_metadata_malformed_lines() {
        let output = format!("too|few|fields\n{P}proj_abc12345|0|0|claude\n\n");
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_parse_pane_metadata_empty_command() {
        let output = format!("{P}proj_abc12345|0|0|\n");
        let map = parse_pane_metadata(&output);
        let meta = map.get(&format!("{P}proj_abc12345")).unwrap();
        assert!(meta.pane_current_command.is_none());
    }

    #[test]
    fn test_parse_pane_metadata_multiple_sessions() {
        let output = format!(
            "{P}proj_a_abc12345|0|0|claude\n{P}proj_b_def67890|0|0|opencode\n{P}proj_c_ghi11111|0|1|bash\n"
        );
        let map = parse_pane_metadata(&output);
        assert_eq!(map.len(), 3);
        assert_eq!(
            map.get(&format!("{P}proj_a_abc12345"))
                .unwrap()
                .pane_current_command
                .as_deref(),
            Some("claude")
        );
        assert_eq!(
            map.get(&format!("{P}proj_b_def67890"))
                .unwrap()
                .pane_current_command
                .as_deref(),
            Some("opencode")
        );
        assert!(map.get(&format!("{P}proj_c_ghi11111")).unwrap().pane_dead);
    }

    fn tmux_available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Verify that the compound-command approach (export + exec) correctly
    /// passes env vars to the exec'd process while keeping secret values
    /// out of all long-lived process argv.
    ///
    /// This simulates the tmux session command:
    ///   export KEY='secret'; exec printenv KEY
    /// and verifies the secret reaches the exec'd process.
    #[test]
    #[serial_test::serial]
    fn test_export_exec_compound_command_passes_env() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        // Ensure the tmux server is already running so the test session's
        // command string doesn't end up in the server process's argv.
        let dummy_guard = TmuxTestSession::new("aoe_test_compound_dummy");
        let dummy = dummy_guard.name().to_string();
        let _ = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &dummy,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 120",
            ])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(200));

        let session_guard = TmuxTestSession::new("aoe_test_compound");
        let session_name = session_guard.name().to_string();
        let marker = format!("AOE_COMPOUND_TEST_{}", std::process::id());
        let secret_value = "s3cret_val!@#";

        // Simulate the compound command approach: export + exec as the session command
        let compound_cmd = format!(
            "export {}='{}'; exec printenv {}",
            marker,
            secret_value.replace('\'', "'\\''"),
            marker
        );

        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "120",
                "-y",
                "24",
                &compound_cmd,
                ";",
                "set-option",
                "-t",
                &session_name,
                "pane-base-index",
                "0",
                ";",
                "set-option",
                "-t",
                &session_name,
                "pane-base-index",
                "0",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success(), "Failed to create tmux session");

        // Wait for printenv to run and exit
        std::thread::sleep(std::time::Duration::from_millis(1000));

        // Capture pane output: should contain the secret value
        let capture = Command::new("tmux")
            .args([
                "capture-pane",
                "-t",
                &format!("{}:^.0", session_name),
                "-p",
                "-S",
                "-10",
            ])
            .output()
            .expect("capture-pane");
        let pane_content = String::from_utf8_lossy(&capture.stdout);
        assert!(
            pane_content.contains(secret_value),
            "Expected secret value in pane output (proves export reached exec'd process).\nPane:\n{}",
            pane_content
        );

        // Pane should be dead (exec replaced the shell, printenv exited)
        let dead_check = Command::new("tmux")
            .args(["display-message", "-t", &session_name, "-p", "#{pane_dead}"])
            .output()
            .expect("pane dead check");
        let is_dead = String::from_utf8_lossy(&dead_check.stdout).trim().eq("1");
        assert!(
            is_dead,
            "Pane should be dead after exec'd command exits (lifecycle preserved)"
        );
    }

    /// Verify that after `exec` replaces the outer shell, the secret
    /// values from export statements are NOT visible in `ps` output.
    ///
    /// Note: the tmux server must already be running before this test.
    /// If the test session is the FIRST tmux process, the `tmux new-session`
    /// process becomes the server and its argv (which contains the command
    /// string with the secret) persists. In real aoe usage the server is
    /// always already running. We start a dummy session first to ensure this.
    #[test]
    #[serial_test::serial]
    fn test_export_exec_secrets_not_in_ps_after_exec() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        // Ensure the tmux server is already running so our test session's
        // command string doesn't end up in the server process's argv.
        let dummy_guard = TmuxTestSession::new("aoe_test_ps_dummy");
        let dummy = dummy_guard.name().to_string();
        let _ = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &dummy,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 120",
            ])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(200));

        let session_guard = TmuxTestSession::new("aoe_test_ps");
        let session_name = session_guard.name().to_string();
        let secret_value = format!("UNIQUE_SECRET_{}_xyzzy", std::process::id());

        // Simulate: export SECRET='val'; exec sleep 30
        // After exec, the shell process (whose argv contained the export) is
        // replaced by sleep, whose argv is just "sleep 30" (no secret).
        let compound_cmd = format!("export AOE_PS_TEST='{}'; exec sleep 30", secret_value);

        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                &compound_cmd,
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        // Wait for exec to complete
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Check ps output for the secret value
        let ps_output = Command::new("ps")
            .args(["auxww"])
            .output()
            .expect("ps auxww");
        let ps_text = String::from_utf8_lossy(&ps_output.stdout);

        assert!(
            !ps_text.contains(&secret_value),
            "Secret value must NOT appear in ps output after exec.\nFound '{}' in ps:\n{}",
            secret_value,
            ps_text
                .lines()
                .filter(|l| l.contains(&secret_value))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
