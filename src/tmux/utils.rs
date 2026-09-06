//! tmux utility functions

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

/// Append `; set-option -t <target> default-shell <shell>` so panes the user
/// later splits off this session use their real shell instead of the shared
/// tmux server's frozen `default-shell` (which a dev build with a sandboxed
/// env can poison; see #2608). The first pane is launched with an explicit
/// login-shell command at create time because a `default-shell` set chained
/// after `new-session` is too late for the already-spawned pane.
pub fn append_default_shell_args(args: &mut Vec<String>, target: &str, shell: &str) {
    args.extend([
        ";".to_string(),
        "set-option".to_string(),
        "-t".to_string(),
        target.to_string(),
        "default-shell".to_string(),
        shell.to_string(),
    ]);
}

/// Append every `[tmux]`-driven option write that a brand-new session needs, in
/// one call, so the three create paths (`session.rs`, `terminal_session.rs`,
/// `tool_session.rs`) cannot drift in which managed settings they honor.
///
/// Iterates the whole managed-settings table (`TmuxSetting::ALL`) and emits the
/// writes each resolved action declares, so a new managed option is one table
/// row rather than a helper plus an edit here. `LeaveToUser` writes nothing: a
/// session created this instant has no session-scoped value aoe wrote to clear,
/// so declining to write already leaves the user's own config in charge. A tmux
/// session option outranks a global one, so unconditionally forcing values
/// here is what silently overrode the file the user wrote and made `[tmux]
/// mouse` look like a setting that did nothing (issue #3207).
///
/// The status bar row declares no creation-time writes on purpose: it needs a
/// resolved theme and the session's title, so it is applied after creation by
/// [`crate::tmux::status_bar::apply_all_tmux_options`], which resolves the same
/// table and, unlike creation, actively unsets stale session-scoped values on
/// `LeaveToUser`. Resolving the row here still probes the user's tmux config
/// (`user_has_tmux_config`, an `exists()` check over the user's tmux config
/// paths) and discards the result; that is the cost of iterating the table
/// uniformly.
///
/// `mouse on` is what the web dashboard's two-finger scroll on mobile needs
/// when the underlying agent uses tmux copy-mode for scrollback (the default
/// renderer for Claude Code, and all other agents). Claude Code's fullscreen
/// renderer (`/tui fullscreen`) bypasses tmux copy-mode: it runs on the
/// alternate screen and relies on alternate-scroll turning the wheel into
/// arrow keys (it binds the arrows to scroll), so the option is harmless but
/// unused in that mode.
///
/// `config` must be the profile-merged config for the session being created;
/// see [`crate::session::config::resolve_tmux_setting`].
pub fn append_tmux_setting_args(args: &mut Vec<String>, target: &str, config: &Config) {
    for setting in TmuxSetting::ALL {
        let writes = tmux_setting_writes(setting, resolve_tmux_setting(setting, config));
        append_tmux_setting_writes(args, target, writes);
    }
}

/// Append the writes of one managed setting to an in-flight tmux argument
/// list. Pure over the writes, so the emitted tokens are table-testable.
fn append_tmux_setting_writes(args: &mut Vec<String>, target: &str, writes: &[TmuxOptionWrite]) {
    for write in writes {
        args.push(";".to_string());
        args.push("set-option".to_string());
        // Only the scope flags differ per variant; the `-q` guard and the
        // option/value pushes are shared.
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
        if quiet {
            args.push("-q".to_string());
        }
        args.extend(scope_flags.iter().map(|flag| flag.to_string()));
        args.push(option.to_string());
        args.push(value.to_string());
    }
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

pub fn is_pane_dead(session_name: &str) -> bool {
    // Use `^.0` to target the first window's first pane regardless of
    // base-index or which pane is active, so the check always hits the
    // agent's pane even when the user has created additional tmux windows
    // or split panes.  See #435, #488.
    let target = format!("{session_name}:^.0");
    crate::tmux::tmux_command()
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
    crate::tmux::tmux_command()
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

/// The terminal title the pane's program published over OSC, for callers
/// outside the batched poll that reads it as part of [`crate::tmux::PaneMetadata`].
///
/// Only `display-message`'s own trailing newline comes off, where the sibling
/// helpers above trim: the batched read does not trim either, and a title is
/// matched by `^`-anchored rules, so trimming here would let the same pane
/// read one way through the poller and another through `aoe session capture`.
pub(crate) fn pane_title(session_name: &str) -> Option<String> {
    let target = format!("{session_name}:^.0");
    crate::tmux::tmux_command()
        .args(["display-message", "-t", &target, "-p", "#{pane_title}"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| strip_display_delimiter(&s).to_string())
        .filter(|s| !s.is_empty())
}

/// Drop the single newline `display-message -p` appends, and only that one.
/// Trimming every trailing newline would also eat one the title itself
/// carried, which is the difference between reporting a title and reporting a
/// truncated one.
fn strip_display_delimiter(raw: &str) -> &str {
    raw.strip_suffix('\n').unwrap_or(raw)
}

fn pane_start_command_is_protected(session_name: &str) -> bool {
    let target = format!("{session_name}:^.0");
    crate::tmux::tmux_command()
        .args([
            "display-message",
            "-t",
            &target,
            "-p",
            "#{pane_start_command}",
        ])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .is_some_and(|command| command.contains(PANE_ENV_FILE_PREFIX))
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

    // Protected pane environment values are sourced by a short-lived script
    // executed by the user's POSIX shell. While the launch command is alive,
    // tmux therefore reports that shell rather than the agent as the pane's
    // current command. The script itself is the pane command, so once the agent
    // exits the pane becomes dead instead of returning to a prompt. Do not
    // mistake this live wrapper for a resurrected or interactive shell.
    is_pane_running_shell_command(
        &current_command,
        pane_start_command_is_protected(session_name),
    )
}

/// Stock tmux keys, under the prefix, that take a client out of a session:
/// `L` is `switch-client -l`, `d` is `detach-client`.
pub(crate) const SWITCH_BACK_KEY: &str = "L";
pub(crate) const DETACH_KEY: &str = "d";

/// Whether this process runs inside a tmux client. Attaching from inside is a
/// `switch-client` of that client; from outside it is a fresh `attach-session`.
pub(crate) fn inside_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

/// The key that brings the client back to aoe after this process attaches in
/// tmux mode: `prefix L` undoes a `switch-client`, `prefix d` ends an
/// `attach-session`.
pub fn attach_return_key() -> &'static str {
    if inside_tmux() {
        SWITCH_BACK_KEY
    } else {
        DETACH_KEY
    }
}

/// Returns the tmux prefix key formatted for display (e.g. "Ctrl+a", "Ctrl+b").
/// Reads `tmux show-option -gv prefix` once on first call and caches the
/// result; falls back to "Ctrl+b" if tmux is unavailable or the option can't
/// be parsed. The prefix can't change while AOE is running, so caching avoids
/// per-render-frame subprocess calls from the welcome dialog.
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

/// Run `tmux kill-session -t <name>`. A missing session is treated as
/// success, since the goal is "this session is not present": `can't find
/// session` (the session is gone, e.g. callers commonly kill the pane's
/// process tree first, which can tear the session down before this lands)
/// and `no server running` (no tmux server at all, so no session exists)
/// are both swallowed in the C locale. Any other tmux failure returns
/// `Err`. Caller is responsible for `refresh_session_cache` after a
/// successful kill.
pub(crate) fn kill_session_if_present(name: &str) -> Result<()> {
    let output = crate::tmux::tmux_query_command()
        .args(["kill-session", "-t", name])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Deliberately broader than `tmux_no_server_running`: for a kill, ANY
        // connect failure (`error connecting`, any errno) means there is no
        // server and thus no session to remove, so it is success. The status
        // pollers need the narrower ENOENT-only test to keep transient glitches
        // on the error path; here a false "absent" cannot act on a live pane.
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

    /// One tmux `set-option` write form emits exactly its tmux tokens: scope
    /// flags, `-q` when quiet, and no target for the server scope. This pins
    /// the emitted-args contract the table rows must keep (issue #3349).
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
                // No table row uses a quiet session write today; the arm
                // still exists, so pin its tokens like the other scopes.
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
                // No target: a server option must not be addressed per-session.
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

    /// The full (setting, action) -> writes matrix, straight from the table
    /// through the `tmux_setting_writes` seam. Covers the user stories at the
    /// table level (issue #3349): US1 (Clipboard/Apply declares the
    /// passthrough writes), US2 (ForceOff rows never carry a forced-on value;
    /// clipboard declares no "off"), US3 (every row maps through one seam, no
    /// per-helper branch).
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
            // The status bar is painted after creation with dynamic theme
            // values; it declares no creation-time writes for any action.
            (TmuxSetting::StatusBar, Apply, &[][..]),
            (TmuxSetting::StatusBar, ForceOff, &[][..]),
            (TmuxSetting::StatusBar, LeaveToUser, &[][..]),
            (TmuxSetting::Mouse, Apply, &mouse_on[..]),
            (TmuxSetting::Mouse, ForceOff, &mouse_off[..]),
            // LeaveToUser writes nothing at creation: a fresh session has no
            // session-scoped value aoe wrote to clear.
            (TmuxSetting::Mouse, LeaveToUser, &[][..]),
            (TmuxSetting::Clipboard, Apply, &clipboard[..]),
            // No expressible "off": unsetting would reach the user's server.
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

    /// The public entry point: resolving the whole table and emitting in
    /// canonical order (mouse before clipboard). Explicit modes keep the
    /// assertions independent of the probe's result, and the isolated HOME
    /// keeps the probe itself away from the real user files (issue #3349,
    /// US2: no forced-on write survives `disabled`).
    #[test]
    #[serial_test::serial]
    fn test_append_tmux_setting_args_emits_rows_in_order() {
        use crate::session::config::TmuxSettingMode::{Disabled, Enabled};
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(tmp.path());

        let mut config = Config::default();
        let all_on = vec![
            ";",
            "set-option",
            "-t",
            "aoe_x",
            "mouse",
            "on",
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
        // (status_bar, mouse, clipboard)
        let cases = [
            ((Enabled, Enabled, Enabled), all_on.clone()),
            // The status bar never contributes at creation, whatever its mode.
            ((Disabled, Enabled, Enabled), all_on),
            (
                (Enabled, Disabled, Enabled),
                vec![
                    ";",
                    "set-option",
                    "-t",
                    "aoe_x",
                    "mouse",
                    "off",
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
                ],
            ),
            // A disabled clipboard declares no writes at all.
            (
                (Enabled, Disabled, Disabled),
                vec![";", "set-option", "-t", "aoe_x", "mouse", "off"],
            ),
            // And a disabled clipboard with mouse on emits just the mouse.
            (
                (Enabled, Enabled, Disabled),
                vec![";", "set-option", "-t", "aoe_x", "mouse", "on"],
            ),
        ];
        for ((status_bar, mouse, clipboard), expected) in cases {
            config.tmux.status_bar = status_bar;
            config.tmux.mouse = mouse;
            config.tmux.clipboard = clipboard;
            let mut args: Vec<String> = Vec::new();
            append_tmux_setting_args(&mut args, "aoe_x", &config);
            assert_eq!(
                args, expected,
                "status_bar={status_bar:?} mouse={mouse:?} clipboard={clipboard:?}"
            );
        }
    }

    /// US1 (issue #3349): a tmux.conf that sets a prefix key but never touches
    /// clipboard still gets aoe's `set-clipboard` passthrough under
    /// `clipboard = "auto"`, and each option defers independently of the
    /// others.
    #[test]
    #[serial_test::serial]
    fn test_user_config_silent_on_option_still_applies_auto() {
        use crate::session::config::{resolve_tmux_setting, TmuxSetting, TmuxSettingAction};
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(tmp.path());
        let tmux_conf = tmp.path().join(".tmux.conf");

        // All `auto`: the default config.
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

        // (a) Prefix key only: aoe still applies its mouse and clipboard writes.
        std::fs::write(&tmux_conf, "set -g prefix C-a\n").unwrap();
        let mut args: Vec<String> = Vec::new();
        append_tmux_setting_args(&mut args, "aoe_x", &config);
        let mut expected = mouse_on.clone();
        expected.extend(clipboard.clone());
        assert_eq!(
            args, expected,
            "a prefix-only tmux.conf must not defer clipboard"
        );
        // A config existing at all does defer the status bar (coarse by
        // design), pinning its WhenUserHasAnyConfig wiring.
        assert_eq!(
            resolve_tmux_setting(TmuxSetting::StatusBar, &config),
            TmuxSettingAction::LeaveToUser
        );

        // (b) User takes set-clipboard: only the clipboard writes defer.
        std::fs::write(&tmux_conf, "set -g prefix C-a\nset -s set-clipboard on\n").unwrap();
        let mut args: Vec<String> = Vec::new();
        append_tmux_setting_args(&mut args, "aoe_x", &config);
        assert_eq!(
            args, mouse_on,
            "set-clipboard must defer only the clipboard writes"
        );

        // (c) User takes mouse: only the mouse write defers.
        std::fs::write(&tmux_conf, "set -g prefix C-a\nset -g mouse on\n").unwrap();
        let mut args: Vec<String> = Vec::new();
        append_tmux_setting_args(&mut args, "aoe_x", &config);
        assert_eq!(args, clipboard, "mouse must defer only the mouse write");
    }

    #[test]
    fn test_sanitize_session_name() {
        assert_eq!(sanitize_session_name("my-project"), "my-project");
        assert_eq!(sanitize_session_name("my project"), "my_project");
        assert_eq!(sanitize_session_name("a".repeat(30).as_str()).len(), 20);
    }

    #[test]
    fn test_strip_ansi() {
        // Covers SGR (single, compound, 256-color, truecolor), OSC terminated
        // by both BEL and ST, and passthrough of code-free input.
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
    fn test_is_pane_running_shell_command_accounts_for_protected_wrapper() {
        let cases = [
            ("sh", true, false),
            ("sh", false, true),
            ("claude", false, false),
        ];
        for (current_command, pane_start_command_is_protected, expected) in cases {
            assert_eq!(
                is_pane_running_shell_command(current_command, pane_start_command_is_protected),
                expected,
                "{current_command:?}, protected={pane_start_command_is_protected}"
            );
        }
    }

    #[test]
    fn test_format_tmux_prefix() {
        // Case is preserved: tmux returns the prefix in whatever case the user
        // wrote it, and the displayed hint should match their muscle memory.
        // An empty prefix falls back to tmux's own default.
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

    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
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
        let _ = crate::tmux::tmux_command()
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
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", name])
            .output();
        let spawn = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", name])
            .status();
        if !spawn.map(|s| s.success()).unwrap_or(false) {
            return;
        }
        assert!(kill_session_if_present(name).is_ok());
        let exists = crate::tmux::tmux_command()
            .args(["has-session", "-t", name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            !exists,
            "session should be gone after kill_session_if_present"
        );
    }

    /// `aoe session capture` reads the pane title through this helper, and the
    /// only test that covers that path runs an agent with no `osc_title`
    /// rules: a wrong target here would silently restore the empty title
    /// #3625 was about.
    #[test]
    #[serial_test::serial]
    fn pane_title_reads_the_panes_published_title() {
        if !tmux_available() {
            return;
        }
        let name = "aoe_test_pane_title";
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", name])
            .output();
        let spawn = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", name, "sleep", "30"])
            .status();
        if !spawn.map(|s| s.success()).unwrap_or(false) {
            return;
        }
        let target = format!("{name}:^.0");
        let _ = crate::tmux::tmux_command()
            .args(["select-pane", "-t", &target, "-T", "aoe-title-probe"])
            .output();
        let title = pane_title(name);
        let _ = kill_session_if_present(name);
        assert_eq!(title.as_deref(), Some("aoe-title-probe"));
    }

    /// Only the delimiter `display-message` adds comes off. tmux 3.6 will not
    /// store a newline in a title (`select-pane -T` refuses one and an OSC
    /// title is sanitized), so this is locked here rather than against a live
    /// pane, which cannot produce the input.
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
