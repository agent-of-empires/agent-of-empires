//! tmux utility functions

use super::{tmux_no_server_running, SessionKind};
use crate::session::config::{
    resolve_tmux_setting, tmux_setting_writes, Config, TmuxOptionWrite, TmuxSetting,
};
use anyhow::{bail, Result};
use std::sync::OnceLock;

pub(crate) const PANE_ENV_FILE_PREFIX: &str = "aoe-pane-env-";

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

/// Strip only ST-terminated OSC sequences; BEL-terminated ones pass through.
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

/// Chain `; set-option <flags>` onto an in-flight tmux argv, so the option
/// lands atomically with the command before it.
fn chain_set_option(args: &mut Vec<String>, flags: &[&str]) {
    args.push(";".to_string());
    args.push("set-option".to_string());
    args.extend(flags.iter().map(|flag| flag.to_string()));
}

/// Pane-level (`-p`, tmux >= 3.0) so user-created panes are unaffected.
pub fn append_remain_on_exit_args(args: &mut Vec<String>, target: &str) {
    chain_set_option(args, &["-p", "-t", target, "remain-on-exit", "on"]);
}

/// Pins pane indices to 0 so `^.0` always targets the agent's pane.
pub fn append_pane_base_index_args(args: &mut Vec<String>, target: &str) {
    chain_set_option(args, &["-t", target, "pane-base-index", "0"]);
}

/// Later splits use the real shell rather than the shared server's possibly
/// poisoned `default-shell`; the first pane gets an explicit command instead.
pub fn append_default_shell_args(args: &mut Vec<String>, target: &str, shell: &str) {
    chain_set_option(args, &["-t", target, "default-shell", shell]);
}

/// Every `[tmux]`-driven creation-time write, from the managed-settings table.
/// `LeaveToUser` writes nothing; the status bar row is applied after creation
/// by [`crate::tmux::status_bar::apply_all_tmux_options`]. `config` must be the
/// profile-merged config.
pub fn append_tmux_setting_args(args: &mut Vec<String>, target: &str, config: &Config) {
    for setting in TmuxSetting::ALL {
        let writes = tmux_setting_writes(setting, resolve_tmux_setting(setting, config));
        append_tmux_setting_writes(args, target, writes);
    }
}

fn append_tmux_setting_writes(args: &mut Vec<String>, target: &str, writes: &[TmuxOptionWrite]) {
    for write in writes {
        let (scope_flags, option, value, quiet) = match *write {
            TmuxOptionWrite::Session {
                option,
                value,
                quiet,
            } => (&["-t", target][..], option, value, quiet),
            TmuxOptionWrite::Server {
                option,
                value,
                quiet,
            } => (&["-s"][..], option, value, quiet),
            TmuxOptionWrite::Window {
                option,
                value,
                quiet,
            } => (&["-w", "-t", target][..], option, value, quiet),
        };
        let mut flags = Vec::with_capacity(scope_flags.len() + 3);
        if quiet {
            flags.push("-q");
        }
        flags.extend_from_slice(scope_flags);
        flags.extend([option, value]);
        chain_set_option(args, &flags);
    }
}

/// The window follows the most recent client, whatever the user's
/// `window-size` config says.
pub fn append_window_size_args(args: &mut Vec<String>, target: &str) {
    chain_set_option(args, &["-t", target, "window-size", "latest"]);
}

/// The option chain every aoe session kind appends to its `new-session`.
pub(crate) fn append_session_setup_args(
    args: &mut Vec<String>,
    target: &str,
    config: &Config,
    default_shell: Option<&str>,
    kind: SessionKind,
) {
    append_remain_on_exit_args(args, target);
    append_pane_base_index_args(args, target);
    append_window_size_args(args, target);
    if let Some(shell) = default_shell {
        append_default_shell_args(args, target, shell);
    }
    append_tmux_setting_args(args, target, config);
    super::append_session_kind_args(args, target, kind);
}

/// Run a `new-session` chain. A concurrent creator winning the race
/// ("duplicate session") counts as success.
pub(crate) fn create_session_tolerating_duplicate(
    args: &[String],
    error: impl FnOnce(&str) -> String,
) -> Result<()> {
    let output = crate::tmux::tmux_command().args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("duplicate session") {
            bail!("{}", error(&stderr));
        }
    }
    super::refresh_session_cache();
    Ok(())
}

/// `switch-client` from inside tmux, else (or when that fails, e.g. an
/// inherited `TMUX`) `attach-session`. Returns the failed exit status, if any.
pub(crate) fn attach_client(name: &str) -> Result<Option<std::process::ExitStatus>> {
    if inside_tmux() {
        let status = crate::tmux::tmux_command()
            .args(["switch-client", "-t", name])
            .status()?;
        if status.success() {
            return Ok(None);
        }
    }
    let status = crate::tmux::tmux_command()
        .args(["attach-session", "-t", name])
        .status()?;
    Ok((!status.success()).then_some(status))
}

/// Kill a session's pane process tree (children can survive SIGHUP), then the
/// session itself.
pub(crate) fn kill_session_tree(name: &str) -> Result<()> {
    if !crate::tmux::session_exists(name) {
        return Ok(());
    }
    if let Some(pane_pid) = crate::process::get_pane_pid(name) {
        crate::process::kill_process_tree(pane_pid);
    }
    kill_session_if_present(name)?;
    super::refresh_session_cache();
    Ok(())
}

/// Best-effort kill of every live session whose name `matches`.
pub(crate) fn kill_sessions_matching(matches: impl Fn(&str) -> bool) {
    let output = crate::tmux::tmux_query_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();
    if let Some(out) = output.as_ref().ok().filter(|out| out.status.success()) {
        for name in String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|name| matches(name))
        {
            if let Some(pid) = crate::process::get_pane_pid(name) {
                crate::process::kill_process_tree(pid);
            }
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", name])
                .output();
        }
    }
    super::refresh_session_cache();
}

/// One `#{pane_dead}` probe of a session's agent pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneProbe {
    Alive,
    /// The pane exists and its process exited.
    Dead,
    /// tmux resolved nothing: exit 0 with empty stdout (any resolvable session
    /// expands to a digit, even with out-of-range indices), or a recognized
    /// no-server failure. Long-lived pollers stop on this.
    Missing,
    /// Anything else, including unrecognized failures; never terminal.
    Unknown,
}

pub(crate) fn probe_pane(session_name: &str) -> PaneProbe {
    // An empty name would resolve `:^.0` against the current session.
    if session_name.is_empty() {
        return PaneProbe::Missing;
    }
    // `^.0` is the agent's pane whatever the base-index or active pane.
    let target = format!("{session_name}:^.0");
    // Query command: the no-server match reads a localized `strerror`.
    let Some(output) = crate::tmux::tmux_query_command()
        .args(["display-message", "-t", &target, "-p", "#{pane_dead}"])
        .output()
        .ok()
    else {
        return PaneProbe::Unknown;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    classify_pane_probe(output.status.success(), stdout.trim(), &output.stderr)
}

pub(crate) fn classify_pane_probe(succeeded: bool, stdout: &str, stderr: &[u8]) -> PaneProbe {
    match stdout {
        "1" => PaneProbe::Dead,
        "0" => PaneProbe::Alive,
        "" if succeeded || tmux_no_server_running(stderr) => PaneProbe::Missing,
        _ => PaneProbe::Unknown,
    }
}

/// A missing session reads as not dead; callers pair this with `exists()`.
pub fn is_pane_dead(session_name: &str) -> bool {
    probe_pane(session_name) == PaneProbe::Dead
}

fn display_first_pane(session_name: &str, format: &str) -> Option<String> {
    let target = format!("{session_name}:^.0");
    crate::tmux::tmux_command()
        .args(["display-message", "-t", &target, "-p", format])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
}

pub(crate) fn pane_current_command(session_name: &str) -> Option<String> {
    display_first_pane(session_name, "#{pane_current_command}")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The OSC terminal title. Only `display-message`'s own newline is removed, to
/// match the untrimmed batched read that `^`-anchored rules see.
pub(crate) fn pane_title(session_name: &str) -> Option<String> {
    display_first_pane(session_name, "#{pane_title}")
        .map(|s| strip_display_delimiter(&s).to_string())
        .filter(|s| !s.is_empty())
}

fn strip_display_delimiter(raw: &str) -> &str {
    raw.strip_suffix('\n').unwrap_or(raw)
}

fn pane_start_command_is_protected(session_name: &str) -> bool {
    display_first_pane(session_name, "#{pane_start_command}")
        .is_some_and(|command| command.contains(PANE_ENV_FILE_PREFIX))
}

/// Shells that mean the agent is not running.
const KNOWN_SHELLS: &[&str] = &[
    "bash", "zsh", "sh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "pwsh",
];

pub(crate) fn is_shell_command(cmd: &str) -> bool {
    let normalized = cmd.strip_prefix('-').unwrap_or(cmd);
    KNOWN_SHELLS.contains(&normalized)
}

pub(crate) fn is_pane_running_shell_command(
    current_command: &str,
    pane_start_command_is_protected: bool,
) -> bool {
    is_shell_command(current_command) && !pane_start_command_is_protected
}

pub fn is_pane_running_shell(session_name: &str) -> bool {
    let Some(current_command) = pane_current_command(session_name) else {
        return false;
    };
    if !is_shell_command(&current_command) {
        return false;
    }
    // The protected env launch script runs under a POSIX shell, which tmux
    // reports while the agent is alive; that wrapper is not a prompt.
    is_pane_running_shell_command(
        &current_command,
        pane_start_command_is_protected(session_name),
    )
}

/// Prefix keys that leave a session: `L` is `switch-client -l`, `d` detaches.
pub(crate) const SWITCH_BACK_KEY: &str = "L";
pub(crate) const DETACH_KEY: &str = "d";

pub(crate) fn inside_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

/// Inside tmux the attach is a `switch-client` (`L`) that may fall back to an
/// attach (`d`), so both keys are named.
pub fn attach_return_hint() -> String {
    attach_return_hint_for(inside_tmux())
}

pub(crate) fn attach_return_hint_for(inside_tmux: bool) -> String {
    if inside_tmux {
        format!("{SWITCH_BACK_KEY} (or {DETACH_KEY})")
    } else {
        DETACH_KEY.to_string()
    }
}

/// The tmux prefix for display (e.g. "Ctrl+a"), read once and cached.
pub fn tmux_prefix_display() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| {
        let raw = crate::tmux::tmux_command()
            .args(["show-option", "-gv", "prefix"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        format_tmux_prefix(&raw)
    })
}

/// `kill-session` that treats an absent session or server as success. Any
/// connect failure counts as absent here, unlike the pollers' narrower test.
/// The caller refreshes the session cache.
pub(crate) fn kill_session_if_present(name: &str) -> Result<()> {
    let output = crate::tmux::tmux_query_command()
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

/// tmux prefix notation ("C-a", "M-b", "F12") to display form.
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
    use crate::tmux::test_helpers::require_tmux;

    #[test]
    fn attach_return_hint_names_both_keys_inside_tmux() {
        assert_eq!(attach_return_hint_for(true), "L (or d)");
        assert_eq!(attach_return_hint_for(false), "d");
    }

    #[test]
    fn test_tmux_option_write_emission() {
        use crate::session::config::TmuxOptionWrite;
        let cases = [
            (
                TmuxOptionWrite::Session {
                    option: "mouse",
                    value: "on",
                    quiet: false,
                },
                vec![";", "set-option", "-t", "aoe_x", "mouse", "on"],
            ),
            (
                TmuxOptionWrite::Session {
                    option: "mouse",
                    value: "off",
                    quiet: false,
                },
                vec![";", "set-option", "-t", "aoe_x", "mouse", "off"],
            ),
            (
                TmuxOptionWrite::Session {
                    option: "mouse",
                    value: "on",
                    quiet: true,
                },
                vec![";", "set-option", "-q", "-t", "aoe_x", "mouse", "on"],
            ),
            (
                TmuxOptionWrite::Server {
                    option: "set-clipboard",
                    value: "on",
                    quiet: true,
                },
                vec![";", "set-option", "-q", "-s", "set-clipboard", "on"],
            ),
            (
                TmuxOptionWrite::Window {
                    option: "allow-passthrough",
                    value: "on",
                    quiet: true,
                },
                vec![
                    ";",
                    "set-option",
                    "-q",
                    "-w",
                    "-t",
                    "aoe_x",
                    "allow-passthrough",
                    "on",
                ],
            ),
        ];
        for (write, expected) in cases {
            let mut args: Vec<String> = Vec::new();
            append_tmux_setting_writes(&mut args, "aoe_x", std::slice::from_ref(&write));
            assert_eq!(args, expected, "{write:?}");
        }
    }

    #[test]
    fn test_tmux_setting_writes_table() {
        use crate::session::config::{TmuxOptionWrite, TmuxSettingAction};
        use TmuxOptionWrite::{Server, Session, Window};
        use TmuxSettingAction::{Apply, ForceOff, LeaveToUser};
        let mouse_on = [Session {
            option: "mouse",
            value: "on",
            quiet: false,
        }];
        let mouse_off = [Session {
            option: "mouse",
            value: "off",
            quiet: false,
        }];
        let clipboard = [
            Server {
                option: "set-clipboard",
                value: "on",
                quiet: true,
            },
            Window {
                option: "allow-passthrough",
                value: "on",
                quiet: true,
            },
        ];
        let cases = [
            (TmuxSetting::StatusBar, Apply, &[][..]),
            (TmuxSetting::StatusBar, ForceOff, &[][..]),
            (TmuxSetting::StatusBar, LeaveToUser, &[][..]),
            (TmuxSetting::Mouse, Apply, &mouse_on[..]),
            (TmuxSetting::Mouse, ForceOff, &mouse_off[..]),
            (TmuxSetting::Mouse, LeaveToUser, &[][..]),
            (TmuxSetting::Clipboard, Apply, &clipboard[..]),
            (TmuxSetting::Clipboard, ForceOff, &[][..]),
            (TmuxSetting::Clipboard, LeaveToUser, &[][..]),
        ];
        for (setting, action, expected) in cases {
            assert_eq!(
                tmux_setting_writes(setting, action),
                expected,
                "{setting:?} {action:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_append_tmux_setting_args_emits_rows_in_order() {
        use crate::session::config::TmuxSettingMode::{Disabled, Enabled};
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(tmp.path());

        const MOUSE_ON: &str = "; set-option -t aoe_x mouse on";
        const MOUSE_OFF: &str = "; set-option -t aoe_x mouse off";
        const CLIPBOARD: &str = "; set-option -q -s set-clipboard on";
        const PASSTHROUGH: &str = "; set-option -q -w -t aoe_x allow-passthrough on";

        let mut config = Config::default();
        let cases = [
            (
                (Enabled, Enabled, Enabled),
                format!("{MOUSE_ON} {CLIPBOARD} {PASSTHROUGH}"),
            ),
            (
                (Disabled, Enabled, Enabled),
                format!("{MOUSE_ON} {CLIPBOARD} {PASSTHROUGH}"),
            ),
            (
                (Enabled, Disabled, Enabled),
                format!("{MOUSE_OFF} {CLIPBOARD} {PASSTHROUGH}"),
            ),
            ((Enabled, Disabled, Disabled), MOUSE_OFF.to_string()),
            ((Enabled, Enabled, Disabled), MOUSE_ON.to_string()),
        ];
        for ((status_bar, mouse, clipboard), expected) in cases {
            config.tmux.status_bar = status_bar;
            config.tmux.mouse = mouse;
            config.tmux.clipboard = clipboard;
            let mut args: Vec<String> = Vec::new();
            append_tmux_setting_args(&mut args, "aoe_x", &config);
            assert_eq!(
                args,
                expected.split(' ').collect::<Vec<_>>(),
                "status_bar={status_bar:?} mouse={mouse:?} clipboard={clipboard:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_user_config_silent_on_option_still_applies_auto() {
        use crate::session::config::{resolve_tmux_setting, TmuxSetting, TmuxSettingAction};
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(tmp.path());
        let tmux_conf = tmp.path().join(".tmux.conf");

        let config = Config::default();
        let mouse_on = vec![";", "set-option", "-t", "aoe_x", "mouse", "on"];
        let clipboard = vec![
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
            "aoe_x",
            "allow-passthrough",
            "on",
        ];

        std::fs::write(&tmux_conf, "set -g prefix C-a\n").unwrap();
        let mut args: Vec<String> = Vec::new();
        append_tmux_setting_args(&mut args, "aoe_x", &config);
        let mut expected = mouse_on.clone();
        expected.extend(clipboard.clone());
        assert_eq!(
            args, expected,
            "a prefix-only tmux.conf must not defer clipboard"
        );
        assert_eq!(
            resolve_tmux_setting(TmuxSetting::StatusBar, &config),
            TmuxSettingAction::LeaveToUser
        );

        std::fs::write(&tmux_conf, "set -g prefix C-a\nset -s set-clipboard on\n").unwrap();
        let mut args: Vec<String> = Vec::new();
        append_tmux_setting_args(&mut args, "aoe_x", &config);
        assert_eq!(
            args, mouse_on,
            "set-clipboard must defer only the clipboard writes"
        );

        std::fs::write(&tmux_conf, "set -g prefix C-a\nset -g mouse on\n").unwrap();
        let mut args: Vec<String> = Vec::new();
        append_tmux_setting_args(&mut args, "aoe_x", &config);
        assert_eq!(args, clipboard, "mouse must defer only the mouse write");
    }

    #[test]
    fn test_sanitize_session_name() {
        for (input, expected) in [
            ("my-project", "my-project"),
            ("my project", "my_project"),
            ("test/path", "test_path"),
            ("test.name", "test_name"),
            ("test@name", "test_name"),
            ("test:name", "test_name"),
            ("test-name_123", "test-name_123"),
            ("", ""),
        ] {
            assert_eq!(sanitize_session_name(input), expected, "{input:?}");
        }
        assert_eq!(sanitize_session_name("a".repeat(30).as_str()).len(), 20);
        let unicode = sanitize_session_name("test😀emoji");
        assert!(unicode.starts_with("test") && unicode.contains('_'));
        assert!(!unicode.contains('😀'));
    }

    #[test]
    fn test_strip_ansi() {
        let cases = [
            ("\x1b[32mgreen\x1b[0m", "green"),
            ("no codes here", "no codes here"),
            ("", ""),
            ("\x1b[1;34mbold blue\x1b[0m", "bold blue"),
            (
                "\x1b[1m\x1b[32mbold green\x1b[0m normal",
                "bold green normal",
            ),
            ("\x1b[38;5;196mred\x1b[0m", "red"),
            ("\x1b[38;2;255;100;50mRGB color\x1b[0m", "RGB color"),
            ("\x1b]0;Window Title\x07text", "text"),
            ("\x1b]0;Window Title\x1b\\text", "text"),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_ansi(input), expected, "{input:?}");
        }
    }

    #[test]
    fn test_strip_osc_st() {
        let cases = [
            (
                "\x1b]8;;https://example.com\x1b\\Click Here\x1b]8;;\x1b\\",
                "Click Here",
            ),
            (
                "before \x1b]8;;https://github.com\x1b\\link text\x1b]8;;\x1b\\ after",
                "before link text after",
            ),
            (
                "\x1b]8;;https://a.com\x1b\\A\x1b]8;;\x1b\\ and \x1b]8;;https://b.com\x1b\\B\x1b]8;;\x1b\\",
                "A and B",
            ),
            ("plain text", "plain text"),
            (
                "\x1b[32m\x1b]8;;url\x1b\\green link\x1b]8;;\x1b\\\x1b[0m",
                "\x1b[32mgreen link\x1b[0m",
            ),
            (
                "\x1b]8;;url without terminator",
                "\x1b]8;;url without terminator",
            ),
            ("\x1b]0;Window Title\x07", "\x1b]0;Window Title\x07"),
            (
                "\x1b]0;Title\x07before\x1b]8;;https://x.com\x1b\\link\x1b]8;;\x1b\\after",
                "\x1b]0;Title\x07beforelinkafter",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_osc_st(input), expected, "{input:?}");
        }
    }

    #[test]
    fn test_is_shell_command() {
        let login = ["-bash", "-zsh", "-sh", "-fish"];
        for shell in KNOWN_SHELLS.iter().copied().chain(login) {
            assert!(is_shell_command(shell), "{shell} is a shell");
        }
        for cmd in [
            "claude", "opencode", "codex", "gemini", "cursor", "droid", "sleep", "python",
        ] {
            assert!(!is_shell_command(cmd), "{cmd} is not a shell");
        }
    }

    #[test]
    fn test_format_tmux_prefix() {
        let cases = [
            ("C-a", "Ctrl+a"),
            ("C-b", "Ctrl+b"),
            ("C-Space", "Ctrl+Space"),
            ("C-A", "Ctrl+A"),
            ("M-x", "Alt+x"),
            ("F12", "F12"),
            ("Space", "Space"),
            ("", "Ctrl+b"),
        ];
        for (input, expected) in cases {
            assert_eq!(format_tmux_prefix(input), expected, "{input:?}");
        }
    }

    #[test]
    fn test_append_default_shell_args() {
        let mut args: Vec<String> = vec!["new-session".into()];
        append_default_shell_args(&mut args, "aoe_test", "/bin/zsh");
        assert_eq!(
            args,
            vec![
                "new-session",
                ";",
                "set-option",
                "-t",
                "aoe_test",
                "default-shell",
                "/bin/zsh",
            ]
        );
    }
    #[test]
    #[serial_test::serial]
    fn kill_session_if_present_kills_or_swallows_missing() {
        require_tmux!();
        let guard =
            crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_kill_if_present_alive");
        let name = guard.name();
        let spawn = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", name, "sleep", "30"])
            .output()
            .expect("create tmux fixture");
        assert!(
            spawn.status.success(),
            "tmux fixture: {}",
            String::from_utf8_lossy(&spawn.stderr)
        );
        assert!(kill_session_if_present(name).is_ok());
        let exists = crate::tmux::tmux_command()
            .args(["has-session", "-t", name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!exists, "session should be gone");
        assert!(
            kill_session_if_present(name).is_ok(),
            "a missing session is not an error"
        );
    }

    #[test]
    #[serial_test::serial]
    fn pane_title_reads_the_panes_published_title() {
        require_tmux!();
        let guard = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_pane_title");
        let name = guard.name();
        let mut args: Vec<String> = ["new-session", "-d", "-s", name, "sleep", "30"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        append_pane_base_index_args(&mut args, name);
        assert!(crate::tmux::tmux_command()
            .args(&args)
            .status()
            .expect("create title fixture")
            .success());
        let target = crate::tmux::test_helpers::only_pane_id(name);
        assert!(crate::tmux::tmux_command()
            .args(["select-pane", "-t", &target, "-T", "aoe-title-probe"])
            .status()
            .expect("set pane title")
            .success());
        let title = pane_title(name);
        assert_eq!(title.as_deref(), Some("aoe-title-probe"));
    }

    #[test]
    fn strip_display_delimiter_removes_only_the_delimiter() {
        assert_eq!(strip_display_delimiter("title\n"), "title");
        assert_eq!(strip_display_delimiter("title"), "title");
        assert_eq!(strip_display_delimiter(""), "");
        assert_eq!(
            strip_display_delimiter("title\n\n"),
            "title\n",
            "a newline the title itself carried must survive"
        );
    }
}

#[cfg(test)]
mod pane_probe_tests {
    use super::*;

    #[test]
    fn classify_pane_probe_splits_missing_from_unknown() {
        use std::str::from_utf8;

        assert_eq!(classify_pane_probe(true, "0", &[]), PaneProbe::Alive);
        assert_eq!(classify_pane_probe(true, "1", &[]), PaneProbe::Dead);

        assert_eq!(classify_pane_probe(true, "", &[]), PaneProbe::Missing);
        let no_server = b"no server running on /tmp/tmux-501/default\n";
        assert_eq!(
            classify_pane_probe(false, "", no_server),
            PaneProbe::Missing
        );
        let enoent = b"error connecting to /tmp/tmux-501/default (No such file or directory)\n";
        assert_eq!(classify_pane_probe(false, "", enoent), PaneProbe::Missing);

        let eacces = b"error connecting to /tmp/tmux-501/default (Permission denied)\n";
        assert_eq!(classify_pane_probe(false, "", eacces), PaneProbe::Unknown);
        let enotsock =
            b"error connecting to /tmp/tmux-501/default (Socket operation on non-socket)\n";
        assert_eq!(classify_pane_probe(false, "", enotsock), PaneProbe::Unknown);
        assert_eq!(
            classify_pane_probe(false, "", b"garbage\n"),
            PaneProbe::Unknown
        );

        assert_eq!(
            classify_pane_probe(true, "garbage", &[]),
            PaneProbe::Unknown
        );
        assert_eq!(classify_pane_probe(false, "0", &[]), PaneProbe::Alive);
        assert_eq!(classify_pane_probe(false, "1", &[]), PaneProbe::Dead);

        assert!(from_utf8(no_server).is_ok());
        assert!(from_utf8(enoent).is_ok());
    }

    #[test]
    fn probe_pane_rejects_an_empty_session_name() {
        assert_eq!(probe_pane(""), PaneProbe::Missing);
        assert!(!is_pane_dead(""));
    }
}
