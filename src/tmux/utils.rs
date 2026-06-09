//! tmux utility functions

use anyhow::{bail, Result};
use std::process::Command;
use std::sync::OnceLock;

pub fn strip_ansi(content: &str) -> String {
    let mut result = strip_osc_st(content);

    while let Some(start) = result.find("\x1b[") {
        let rest = &result[start + 2..];
        let end_offset = rest
            .find(|c: char| c.is_ascii_alphabetic())
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        result = format!("{}{}", &result[..start], &result[start + 2 + end_offset..]);
    }

    while let Some(start) = result.find("\x1b]") {
        if let Some(end) = result[start..].find('\x07') {
            result = format!("{}{}", &result[..start], &result[start + end + 1..]);
        } else {
            break;
        }
    }

    result
}

/// Only targets ST-terminated (`\x1b\\`) OSC sequences; BEL-terminated ones
/// must pass through unchanged since downstream parsers handle those correctly.
pub(crate) fn strip_osc_st(content: &str) -> String {
    const OSC: &str = "\x1b]";
    const ST: &str = "\x1b\\";

    let mut result = String::with_capacity(content.len());
    let mut remaining = content;

    while let Some(osc_start) = remaining.find(OSC) {
        result.push_str(&remaining[..osc_start]);
        let payload = &remaining[osc_start + OSC.len()..];

        let bel_pos = payload.find('\x07');
        let st_pos = payload.find(ST);

        match (bel_pos, st_pos) {
            (Some(b), Some(s)) if b < s => {
                let end = osc_start + OSC.len() + b + 1;
                result.push_str(&remaining[osc_start..end]);
                remaining = &remaining[end..];
            }
            (_, Some(s)) => {
                remaining = &payload[s + ST.len()..];
            }
            _ => {
                result.push_str(&remaining[osc_start..osc_start + OSC.len()]);
                remaining = &remaining[osc_start + OSC.len()..];
            }
        }
    }
    result.push_str(remaining);
    result
}

pub fn sanitize_session_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(20)
        .collect()
}

/// Append `; set-option -p -t <target> remain-on-exit on` to an in-flight
/// tmux argument list so that remain-on-exit is set atomically with session
/// creation. Using pane-level (`-p`) avoids bleeding into user-created panes
/// in the same session.
///
/// Note: the `-p` (pane-level) flag requires tmux >= 3.0.
pub fn append_remain_on_exit_args(args: &mut Vec<String>, target: &str) {
    args.extend([
        ";".to_string(),
        "set-option".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        target.to_string(),
        "remain-on-exit".to_string(),
        "on".to_string(),
    ]);
}

/// Append `; set-option -t <target> pane-base-index 0` to an in-flight tmux
/// argument list so that pane indices always start at 0 regardless of the
/// user's global config.  This lets status checks use `.0` to reliably target
/// the agent's pane.  See #488.
pub fn append_pane_base_index_args(args: &mut Vec<String>, target: &str) {
    args.extend([
        ";".to_string(),
        "set-option".to_string(),
        "-t".to_string(),
        target.to_string(),
        "pane-base-index".to_string(),
        "0".to_string(),
    ]);
}

/// Append `; set-option -t <target> mouse on` to an in-flight tmux argument
/// list so that mouse/wheel events are forwarded into tmux copy-mode.
///
/// Required for the web dashboard's two-finger scroll on mobile when the
/// underlying agent uses tmux copy-mode for scrollback (the default
/// renderer for Claude Code, and all other agents). Claude Code's
/// fullscreen renderer (`/tui fullscreen`) bypasses tmux copy-mode and
/// handles wheel events itself, so the option is harmless but unused in
/// that mode.
pub fn append_mouse_on_args(args: &mut Vec<String>, target: &str) {
    args.extend([
        ";".to_string(),
        "set-option".to_string(),
        "-t".to_string(),
        target.to_string(),
        "mouse".to_string(),
        "on".to_string(),
    ]);
}

/// Append `; set-option -t <target> window-size latest` so the tmux window
/// follows the most recently active client. Required for the primary-client
/// resize model: without this, a user's `~/.tmux.conf` could set
/// `window-size smallest`, which would shrink the window to the smallest
/// attached PTY regardless of which client is primary.
pub fn append_window_size_args(args: &mut Vec<String>, target: &str) {
    args.extend([
        ";".to_string(),
        "set-option".to_string(),
        "-t".to_string(),
        target.to_string(),
        "window-size".to_string(),
        "latest".to_string(),
    ]);
}

/// Append the two tmux options required for OSC 52 clipboard escapes from
/// the wrapped agent (Claude Code, OpenCode, Codex, etc.) to reach the outer
/// terminal. Without these, "select to copy" inside the agent silently fails
/// because tmux drops the sequence (see #897).
///
/// Two distinct mechanisms are covered:
///   * `set-clipboard on` (server option): captures and forwards raw OSC 52
///     sequences to attached terminal clients.
///   * `allow-passthrough on` (window option, added in tmux 3.3): allows
///     `\ePtmux;...\e\\`-wrapped escapes (the form OpenCode uses) to be
///     unwrapped and forwarded.
///
/// Programs vary in which form they emit, so both are set defensively. Scope
/// flags are explicit (`-s`, `-w`) so the call site is unambiguous and
/// resilient to future tmux scope-inference changes; matches the convention
/// used by `append_remain_on_exit_args` for `remain-on-exit`.
///
/// `-q` (silently ignore errors) keeps aoe compatible with tmux < 3.3, where
/// `allow-passthrough` does not exist. On those versions the set-option call
/// quietly no-ops instead of failing the whole `new-session` invocation.
pub fn append_clipboard_passthrough_args(args: &mut Vec<String>, target: &str) {
    args.extend([
        ";".to_string(),
        "set-option".to_string(),
        "-q".to_string(),
        "-s".to_string(),
        "set-clipboard".to_string(),
        "on".to_string(),
        ";".to_string(),
        "set-option".to_string(),
        "-q".to_string(),
        "-w".to_string(),
        "-t".to_string(),
        target.to_string(),
        "allow-passthrough".to_string(),
        "on".to_string(),
    ]);
}

pub fn is_pane_dead(session_name: &str) -> bool {
    // Use `^.0` to target the first window's first pane regardless of
    // base-index or which pane is active, so the check always hits the
    // agent's pane even when the user has created additional tmux windows
    // or split panes.  See #435, #488.
    let target = format!("{session_name}:^.0");
    Command::new("tmux")
        .args(["display-message", "-t", &target, "-p", "#{pane_dead}"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

pub(crate) fn pane_current_command(session_name: &str) -> Option<String> {
    // Use `^.0` to target the first window's first pane regardless of
    // base-index or which pane is active.  See #435, #488.
    let target = format!("{session_name}:^.0");
    Command::new("tmux")
        .args([
            "display-message",
            "-t",
            &target,
            "-p",
            "#{pane_current_command}",
        ])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// Shells that indicate the agent is not running (the pane was restored by
// tmux-resurrect, the agent crashed back to a prompt, or the user exited).
const KNOWN_SHELLS: &[&str] = &[
    "bash", "zsh", "sh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "pwsh",
];

pub(crate) fn is_shell_command(cmd: &str) -> bool {
    let normalized = cmd.strip_prefix('-').unwrap_or(cmd);
    KNOWN_SHELLS.contains(&normalized)
}

pub fn is_pane_running_shell(session_name: &str) -> bool {
    pane_current_command(session_name)
        .map(|cmd| is_shell_command(&cmd))
        .unwrap_or(false)
}

/// Returns the tmux prefix key formatted for display (e.g. "Ctrl+a", "Ctrl+b").
/// Reads `tmux show-option -gv prefix` once on first call and caches the
/// result; falls back to "Ctrl+b" if tmux is unavailable or the option can't
/// be parsed. The prefix can't change while AOE is running, so caching avoids
/// per-render-frame subprocess calls from the welcome dialog.
pub fn tmux_prefix_display() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| {
        let raw = Command::new("tmux")
            .args(["show-option", "-gv", "prefix"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        format_tmux_prefix(&raw)
    })
}

/// Run `tmux kill-session -t <name>`. A missing session is treated as
/// success, since the goal is "this session is not present": `can't find
/// session` (the session is gone, e.g. callers commonly kill the pane's
/// process tree first, which can tear the session down before this lands)
/// and `no server running` (no tmux server at all, so no session exists)
/// are both swallowed in the C locale. Any other tmux failure returns
/// `Err`. Caller is responsible for `refresh_session_cache` after a
/// successful kill.
pub(crate) fn kill_session_if_present(name: &str) -> Result<()> {
    let output = Command::new("tmux")
        .env("LC_ALL", "C")
        .args(["kill-session", "-t", name])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let absent = stderr.contains("can't find session")
            || stderr.contains("no server running")
            || stderr.contains("error connecting");
        if !absent {
            bail!("Failed to kill tmux session '{}': {}", name, stderr);
        }
    }
    Ok(())
}

/// Convert tmux's raw prefix notation (e.g. "C-a", "M-b", "F12") to the
/// display form shown in UI hints. Preserves case from tmux so users see the
/// same letter they typed in `~/.tmux.conf`.
fn format_tmux_prefix(raw: &str) -> String {
    if let Some(key) = raw.strip_prefix("C-") {
        format!("Ctrl+{key}")
    } else if let Some(key) = raw.strip_prefix("M-") {
        format!("Alt+{key}")
    } else if !raw.is_empty() {
        raw.to_string()
    } else {
        "Ctrl+b".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_session_name() {
        assert_eq!(sanitize_session_name("my-project"), "my-project");
        assert_eq!(sanitize_session_name("my project"), "my_project");
        assert_eq!(sanitize_session_name("a".repeat(30).as_str()).len(), 20);
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\x1b[32mgreen\x1b[0m"), "green");
        assert_eq!(strip_ansi("no codes here"), "no codes here");
        assert_eq!(strip_ansi("\x1b[1;34mbold blue\x1b[0m"), "bold blue");
    }

    #[test]
    fn test_strip_ansi_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn test_strip_ansi_multiple_codes() {
        assert_eq!(
            strip_ansi("\x1b[1m\x1b[32mbold green\x1b[0m normal"),
            "bold green normal"
        );
    }

    #[test]
    fn test_strip_ansi_osc_bel() {
        assert_eq!(strip_ansi("\x1b]0;Window Title\x07text"), "text");
    }

    #[test]
    fn test_strip_ansi_osc_st() {
        assert_eq!(strip_ansi("\x1b]0;Window Title\x1b\\text"), "text");
    }

    #[test]
    fn test_strip_osc_st_hyperlink() {
        assert_eq!(
            strip_osc_st("\x1b]8;;https://example.com\x1b\\Click Here\x1b]8;;\x1b\\"),
            "Click Here"
        );
    }

    #[test]
    fn test_strip_osc_st_preserves_surrounding_text() {
        assert_eq!(
            strip_osc_st("before \x1b]8;;https://github.com\x1b\\link text\x1b]8;;\x1b\\ after"),
            "before link text after"
        );
    }

    #[test]
    fn test_strip_osc_st_multiple_links() {
        let input = "\x1b]8;;https://a.com\x1b\\A\x1b]8;;\x1b\\ and \x1b]8;;https://b.com\x1b\\B\x1b]8;;\x1b\\";
        assert_eq!(strip_osc_st(input), "A and B");
    }

    #[test]
    fn test_strip_osc_st_no_osc() {
        assert_eq!(strip_osc_st("plain text"), "plain text");
    }

    #[test]
    fn test_strip_osc_st_preserves_sgr() {
        assert_eq!(
            strip_osc_st("\x1b[32m\x1b]8;;url\x1b\\green link\x1b]8;;\x1b\\\x1b[0m"),
            "\x1b[32mgreen link\x1b[0m"
        );
    }

    #[test]
    fn test_strip_osc_st_unterminated() {
        assert_eq!(
            strip_osc_st("\x1b]8;;url without terminator"),
            "\x1b]8;;url without terminator"
        );
    }

    #[test]
    fn test_strip_osc_st_passes_bel_terminated_through() {
        let bel_osc = "\x1b]0;Window Title\x07";
        assert_eq!(strip_osc_st(bel_osc), bel_osc);
    }

    #[test]
    fn test_strip_osc_st_mixed_bel_then_st() {
        let input = "\x1b]0;Title\x07before\x1b]8;;https://x.com\x1b\\link\x1b]8;;\x1b\\after";
        assert_eq!(strip_osc_st(input), "\x1b]0;Title\x07beforelinkafter");
    }

    #[test]
    fn test_strip_ansi_nested_sequences() {
        assert_eq!(strip_ansi("\x1b[38;5;196mred\x1b[0m"), "red");
    }

    #[test]
    fn test_strip_ansi_with_256_colors() {
        assert_eq!(
            strip_ansi("\x1b[38;2;255;100;50mRGB color\x1b[0m"),
            "RGB color"
        );
    }

    #[test]
    fn test_sanitize_session_name_special_chars() {
        assert_eq!(sanitize_session_name("test/path"), "test_path");
        assert_eq!(sanitize_session_name("test.name"), "test_name");
        assert_eq!(sanitize_session_name("test@name"), "test_name");
        assert_eq!(sanitize_session_name("test:name"), "test_name");
    }

    #[test]
    fn test_sanitize_session_name_preserves_valid_chars() {
        assert_eq!(sanitize_session_name("test-name_123"), "test-name_123");
    }

    #[test]
    fn test_sanitize_session_name_empty() {
        assert_eq!(sanitize_session_name(""), "");
    }

    #[test]
    fn test_sanitize_session_name_unicode() {
        let result = sanitize_session_name("test😀emoji");
        assert!(result.starts_with("test"));
        assert!(result.contains('_'));
        assert!(!result.contains('😀'));
    }

    #[test]
    fn test_is_shell_command_recognizes_common_shells() {
        for shell in KNOWN_SHELLS {
            assert!(
                is_shell_command(shell),
                "{shell} should be recognized as a shell"
            );
        }
    }

    #[test]
    fn test_is_shell_command_recognizes_login_shells() {
        for shell in ["-bash", "-zsh", "-sh", "-fish"] {
            assert!(
                is_shell_command(shell),
                "{shell} should be recognized as a login shell"
            );
        }
    }

    #[test]
    fn test_is_shell_command_rejects_agent_binaries() {
        for cmd in [
            "claude", "opencode", "codex", "gemini", "cursor", "droid", "sleep", "python",
        ] {
            assert!(
                !is_shell_command(cmd),
                "{cmd} should not be recognized as a shell"
            );
        }
    }

    #[test]
    fn test_format_tmux_prefix_ctrl() {
        assert_eq!(format_tmux_prefix("C-a"), "Ctrl+a");
        assert_eq!(format_tmux_prefix("C-b"), "Ctrl+b");
        assert_eq!(format_tmux_prefix("C-Space"), "Ctrl+Space");
    }

    #[test]
    fn test_format_tmux_prefix_alt() {
        assert_eq!(format_tmux_prefix("M-x"), "Alt+x");
    }

    #[test]
    fn test_format_tmux_prefix_preserves_case() {
        // tmux returns the prefix in whatever case the user wrote it; preserve
        // it so the displayed hint matches their muscle memory.
        assert_eq!(format_tmux_prefix("C-A"), "Ctrl+A");
        assert_eq!(format_tmux_prefix("C-b"), "Ctrl+b");
    }

    #[test]
    fn test_format_tmux_prefix_special_keys() {
        assert_eq!(format_tmux_prefix("F12"), "F12");
        assert_eq!(format_tmux_prefix("Space"), "Space");
    }

    #[test]
    fn test_format_tmux_prefix_empty_falls_back() {
        assert_eq!(format_tmux_prefix(""), "Ctrl+b");
    }

    #[test]
    fn test_append_clipboard_passthrough_args() {
        let mut args: Vec<String> = vec!["new-session".into()];
        append_clipboard_passthrough_args(&mut args, "aoe_test");
        assert_eq!(
            args,
            vec![
                "new-session",
                ";",
                "set-option",
                "-q",
                "-s",
                "set-clipboard",
                "on",
                ";",
                "set-option",
                "-q",
                "-w",
                "-t",
                "aoe_test",
                "allow-passthrough",
                "on",
            ]
        );
    }

    fn tmux_available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    // Serialized like every test that talks to the shared tmux server: a
    // non-serial test that kills the server's last session makes the server
    // exit, and a `#[serial]` peer whose `new-session` connects inside that
    // teardown window fails with "server exited unexpectedly" (CI flake on
    // update_status_reconciles_running_hook_to_waiting_on_claude_approval_prompt).
    #[test]
    #[serial_test::serial]
    fn kill_session_if_present_swallows_missing_session() {
        if !tmux_available() {
            return;
        }
        let name = "aoe_test_kill_if_present_missing";
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", name])
            .output();
        assert!(kill_session_if_present(name).is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn kill_session_if_present_kills_existing_session() {
        if !tmux_available() {
            return;
        }
        let name = "aoe_test_kill_if_present_alive";
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", name])
            .output();
        let spawn = Command::new("tmux")
            .args(["new-session", "-d", "-s", name])
            .status();
        if !spawn.map(|s| s.success()).unwrap_or(false) {
            return;
        }
        assert!(kill_session_if_present(name).is_ok());
        let exists = Command::new("tmux")
            .args(["has-session", "-t", name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            !exists,
            "session should be gone after kill_session_if_present"
        );
    }
}
