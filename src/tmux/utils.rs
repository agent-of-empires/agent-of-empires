//! tmux utility functions

use super::tmux_no_server_running;
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
/// user's global config. Pane targets address the first pane as `:^`, which
/// resolves regardless of that base; the pin keeps the chained `^.0..^.{n}`
/// captures addressing every pane without a prior `list-panes` round trip.
/// See #488.
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

/// Outcome of one `#{pane_dead}` probe against a session's agent pane.
///
/// The distinction that matters is `Dead` vs `Missing`: tmux answers a
/// `display-message` against a session it cannot find with exit status 0, an
/// empty stdout, and nothing on stderr, so "the pane is alive and not dead"
/// and "there is no such session" are only separable by looking at whether
/// the format expanded to anything at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneProbe {
    /// `#{pane_dead}` expanded to `0`.
    Alive,
    /// `#{pane_dead}` expanded to `1`: the pane exists and its process exited.
    Dead,
    /// tmux ran and resolved nothing, so the session is gone. Not the same as
    /// `Dead`: a caller that gates on `exists()` first wants this to read as
    /// "not dead", while a caller holding a long-lived handle to the session
    /// wants it to read as "stop".
    ///
    /// `Missing` is the precise "tmux resolved the target to nothing" signal,
    /// and only that: exit 0 with empty stdout (the name did not resolve), or
    /// a non-zero exit carrying the recognized no-server / dead-socket
    /// (ENOENT) markers on stderr. There is no tmux server, so every session
    /// on it is gone too. Classifying those as `Unknown` would be worse, not
    /// safer: nothing ever terminates on `Unknown`, so a vanished server would
    /// leave every poller running against sessions that cannot come back
    /// under that server, which is the leak this variant exists to close.
    ///
    /// Every other empty-stdout failure (EACCES, ENOTSOCK, malformed stderr)
    /// is `Unknown`: folding it into `Missing` stops a poller that has seen
    /// the target alive after two such probes. So does stdout that is neither
    /// `0`, `1`, nor empty.
    ///
    /// `Missing` stays precise because any target tmux can resolve *to a
    /// session* expands the format to a digit, including out-of-range window
    /// and pane indices (`:99.0`, `:^.99`), which it falls back from. Only an
    /// unresolvable session name yields empty stdout, so a user's
    /// `pane-base-index` setting cannot make a live pane read as `Missing`.
    /// (Measured against tmux 3.7c.)
    Missing,
    /// tmux itself could not be run, so the probe says nothing about the pane.
    Unknown,
}

/// Probe the first pane without treating lookup failures as disappearance.
pub(crate) fn probe_pane(session_name: &str) -> PaneProbe {
    if session_name.is_empty() {
        return PaneProbe::Missing;
    }
    // The first pane by id order, never the active one: `^` follows focus,
    // and `.0` is base-index sensitive. `list-panes` order is index order.
    let first = match first_window_pane_ids_classified(
        session_name,
        &crate::tmux::TmuxCommandDeadline::new(),
    ) {
        PaneListLookup::Panes(ids) => ids,
        PaneListLookup::Missing => return PaneProbe::Missing,
        PaneListLookup::Unknown => return PaneProbe::Unknown,
    };
    let Some(first) = first.into_iter().next() else {
        return PaneProbe::Missing;
    };
    // `tmux_query_command`, not `tmux_command`: `classify_pane_probe` matches
    // the ENOENT marker in tmux's `error connecting to <socket> (<strerror>)`,
    // and glibc localizes `strerror` by `LC_MESSAGES`.
    let Some(output) = crate::tmux::tmux_query_command()
        .args(["display-message", "-t", &first, "-p", "#{pane_dead}"])
        .output()
        .ok()
    else {
        return PaneProbe::Unknown;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    classify_pane_probe(output.status.success(), stdout.trim(), &output.stderr)
}

pub fn first_pane_id(session_name: &str) -> Option<String> {
    first_pane_id_with_deadline(session_name, &crate::tmux::TmuxCommandDeadline::new())
}

pub(crate) fn first_pane_id_with_deadline(
    session_name: &str,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> Option<String> {
    first_window_pane_ids_with_deadline(session_name, deadline)?
        .into_iter()
        .next()
}

enum PaneListLookup {
    Panes(Vec<String>),
    Missing,
    Unknown,
}

fn first_window_pane_ids_classified(
    session_name: &str,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> PaneListLookup {
    let target = format!("={session_name}:");
    let mut command = crate::tmux::tmux_query_command();
    command.args([
        "list-panes",
        "-s",
        "-t",
        &target,
        "-F",
        "#{window_index} #{pane_index} #{pane_id}",
    ]);
    let Ok(output) = deadline.run(&mut command) else {
        return PaneListLookup::Unknown;
    };
    if !output.status.success() {
        return if tmux_no_server_running(&output.stderr)
            || String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line.trim().starts_with("can't find session:"))
        {
            PaneListLookup::Missing
        } else {
            PaneListLookup::Unknown
        };
    }
    let Ok(stdout) = String::from_utf8(output.stdout) else {
        return PaneListLookup::Unknown;
    };
    let mut indexed: Vec<_> = stdout
        .lines()
        .filter_map(|line| {
            let (window, rest) = line.split_once(' ')?;
            let (index, id) = rest.split_once(' ')?;
            let window: u32 = window.parse().ok()?;
            let index: u32 = index.parse().ok()?;
            (id.starts_with('%')).then_some((window, index, id.to_string()))
        })
        .collect();
    indexed.sort_by_key(|(window, index, _)| (*window, *index));
    let Some((first_window, _, _)) = indexed.first() else {
        return if stdout.trim().is_empty() {
            PaneListLookup::Missing
        } else {
            PaneListLookup::Unknown
        };
    };
    let first_window = *first_window;
    PaneListLookup::Panes(
        indexed
            .into_iter()
            .take_while(|(window, _, _)| *window == first_window)
            .map(|(_, _, id)| id)
            .collect(),
    )
}

pub(crate) fn first_window_pane_ids_with_deadline(
    session_name: &str,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> Option<Vec<String>> {
    match first_window_pane_ids_classified(session_name, deadline) {
        PaneListLookup::Panes(ids) => Some(ids),
        PaneListLookup::Missing | PaneListLookup::Unknown => None,
    }
}

/// Pure classification of one `#{pane_dead}` probe, split out from the real
/// tmux call so the failure taxonomy is unit-testable without a socket.
pub(crate) fn classify_pane_probe(succeeded: bool, stdout: &str, stderr: &[u8]) -> PaneProbe {
    match stdout {
        "1" => PaneProbe::Dead,
        "0" => PaneProbe::Alive,
        "" => {
            if succeeded || tmux_no_server_running(stderr) {
                PaneProbe::Missing
            } else {
                PaneProbe::Unknown
            }
        }
        _ => PaneProbe::Unknown,
    }
}

/// Whether `session_name`'s agent pane exists and its process has exited.
///
/// A session tmux cannot find reads as `false`, not `true`: every caller of
/// this pairs it with an `exists()` check, so folding "missing" into "dead"
/// here would make an absent session look like a dead pane. Callers that
/// treat an absent session as terminal (the session-id poller, which holds no
/// separate existence gate) use [`probe_pane`] instead.
pub fn is_pane_dead(session_name: &str) -> bool {
    probe_pane(session_name) == PaneProbe::Dead
}

pub(crate) fn pane_current_command(session_name: &str) -> Option<String> {
    let first = first_pane_id(session_name)?;
    crate::tmux::tmux_command()
        .args([
            "display-message",
            "-t",
            &first,
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
    let target = format!("={session_name}:^");
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
    let Some(first) = first_pane_id(session_name) else {
        return false;
    };
    let target = first;
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

/// The key hint for coming back to aoe after this process attaches in tmux
/// mode. From outside tmux the attach is an `attach-session`, undone by
/// `prefix d`. From inside tmux it is a `switch-client`, undone by
/// `prefix L`, but with no client to switch (an inherited `TMUX`) it falls
/// back to `attach-session`, so the hint names both keys.
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

/// Kill a session, accepting only confirmed session or server absence.
/// Callers refresh the session cache after success.
pub(crate) fn kill_session_if_present(name: &str) -> Result<()> {
    let output = crate::tmux::tmux_query_command()
        .args(["kill-session", "-t"])
        .arg(format!("={name}"))
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let absent = crate::tmux::tmux_no_server_running(&output.stderr)
            || stderr
                .lines()
                .any(|line| line.trim().starts_with("can't find session:"));
        if !absent {
            bail!("Failed to kill tmux session '{}': {}", name, stderr);
        }
    }
    Ok(())
}

/// Kill indexed terminals and tool sessions, including names left behind by retitles.
pub(crate) fn kill_ancillary_sessions_for_id(id: &str) -> Result<()> {
    use crate::tmux::{CONTAINER_TERMINAL_PREFIX, TERMINAL_PREFIX, TOOL_PREFIX};
    let output = crate::tmux::tmux_query_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()?;
    if !output.status.success() {
        if crate::tmux::tmux_no_server_running(&output.stderr) {
            crate::tmux::refresh_session_cache();
            return Ok(());
        }
        bail!(
            "Failed to enumerate ancillary tmux sessions: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let suffix = super::id_suffix(id);
    for name in std::str::from_utf8(&output.stdout)?.lines() {
        let matches = if name.starts_with(TOOL_PREFIX) {
            name.ends_with(&suffix)
        } else if name.starts_with(TERMINAL_PREFIX) || name.starts_with(CONTAINER_TERMINAL_PREFIX) {
            name.rsplit_once(&suffix).is_some_and(|(_, tail)| {
                tail.is_empty()
                    || tail.strip_prefix("_t").is_some_and(|index| {
                        !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
                    })
            })
        } else {
            false
        };
        if matches {
            if let Some(pid) = crate::process::get_pane_pid(name) {
                crate::process::kill_process_tree(pid);
            }
            kill_session_if_present(name)?;
        }
    }
    crate::tmux::refresh_session_cache();
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
    fn kill_session_if_present_kills_or_swallows_missing() {
        if !tmux_available() {
            return;
        }
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
    fn kill_session_if_present_rejects_invalid_socket_paths() {
        if std::env::var_os("AOE_TEST_TMUX_KILL_CHILD").is_some() {
            assert_eq!(probe_pane("proof-target"), PaneProbe::Unknown);
            assert!(
                kill_session_if_present("proof-target").is_err(),
                "a connection error must not claim the session was removed"
            );
            let _home = crate::session::test_support::isolate_app_dir();
            let instance = crate::session::Instance::new("proof", "/tmp");
            let results = [
                ("agent", instance.kill_locked()),
                (
                    "ancillary enumeration",
                    kill_ancillary_sessions_for_id(&instance.id),
                ),
                (
                    "host",
                    crate::tmux::TerminalSession::new(&instance.id, "proof")
                        .unwrap()
                        .kill(),
                ),
                (
                    "container",
                    crate::tmux::ContainerTerminalSession::new(&instance.id, "proof")
                        .unwrap()
                        .kill(),
                ),
                (
                    "tool",
                    crate::tmux::ToolSession::new(&instance.id, "proof", "tool").kill(),
                ),
            ];
            for (kind, result) in results {
                assert!(
                    result.is_err(),
                    "{kind} kill concealed an inaccessible server"
                );
            }
            crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap())
                .unwrap();
            for structured in [false, true] {
                use crate::session::deletion::{
                    DeletionRequest, PurgeReservation, PurgeTransaction,
                };
                let mut row = crate::session::Instance::new("purge proof", "");
                row.source_profile = "socket-proof".into();
                row.status = crate::session::Status::Stopped;
                row.scratch = true;
                if structured {
                    row.view = crate::session::View::Structured;
                }
                let path = crate::session::scratch::provision_scratch_dir(&row.id).unwrap();
                row.project_path = path.to_string_lossy().into_owned();
                std::fs::write(path.join("payload"), b"retain while runtime is unknown").unwrap();
                let store = crate::session::Storage::new_unwatched("socket-proof").unwrap();
                store
                    .update(|rows, _| {
                        rows.push(row.clone());
                        Ok(())
                    })
                    .unwrap();
                let request = DeletionRequest {
                    session_id: row.id.clone(),
                    instance: row,
                    delete_worktree: false,
                    delete_branch: false,
                    delete_sandbox: false,
                    force_delete: false,
                    detach_hooks: true,
                    keep_scratch: false,
                };
                let PurgeReservation::Reserved(transaction) =
                    PurgeTransaction::reserve(store, request, None).unwrap()
                else {
                    panic!("purge must be admitted");
                };
                let transaction = transaction.run_hooks().unwrap();
                let result = if structured {
                    transaction.begin_irreversible().unwrap().finish()
                } else {
                    transaction.complete()
                };
                assert!(
                    !result.success,
                    "purge claimed success without a reachable tmux server"
                );
                assert_eq!(
                    std::fs::read(path.join("payload")).unwrap(),
                    b"retain while runtime is unknown"
                );
            }

            return;
        }
        if !tmux_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let not_directory = root.path().join("not-a-directory");
        std::fs::write(&not_directory, b"not a socket directory").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tmux::utils::tests::kill_session_if_present_rejects_invalid_socket_paths",
                "--nocapture",
            ])
            .env("AOE_TEST_TMUX_KILL_CHILD", "1")
            .env("AOE_TMUX_SOCKET", not_directory.join("socket"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn ancillary_teardown_preserves_other_sessions_and_exact_name_peers() {
        if std::env::var_os("AOE_TEST_ANCILLARY_KILL_CHILD").is_some() {
            use crate::tmux::test_helpers::TmuxTestSession;
            use crate::tmux::{ContainerTerminalSession, Session, TerminalSession, ToolSession};
            let _home = crate::session::test_support::isolate_app_dir();
            let id = "abc12345-owned";
            let owned = [
                TerminalSession::generate_name(id, "old title"),
                TerminalSession::generate_name_indexed(id, "another title", 12),
                ContainerTerminalSession::generate_name_indexed(id, "container", 3),
                ToolSession::generate_name(id, "removed config", "old-tool"),
            ];
            let missing = ToolSession::generate_name(id, "absent", "prefix");
            let peers = [
                Session::generate_name(id, "agent"),
                TerminalSession::generate_name("xyz98765-peer", "peer"),
                format!("{}_topic", owned[0]),
                format!("{missing}_peer"),
            ];
            let sessions: Vec<_> = owned
                .iter()
                .chain(&peers)
                .map(|name| TmuxTestSession::from_name(name.clone()))
                .collect();
            for session in &sessions {
                let output = crate::tmux::tmux_command()
                    .args([
                        "-f",
                        "/dev/null",
                        "new-session",
                        "-d",
                        "-s",
                        session.name(),
                        "sleep 120",
                    ])
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert_eq!(
                crate::process::get_pane_pid(&missing),
                None,
                "an absent session must not resolve a peer process"
            );
            kill_session_if_present(&missing).unwrap();
            kill_ancillary_sessions_for_id(id).unwrap();
            let output = crate::tmux::tmux_query_command()
                .args(["list-sessions", "-F", "#{session_name}"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let names: std::collections::HashSet<_> = std::str::from_utf8(&output.stdout)
                .unwrap()
                .lines()
                .collect();
            for name in &owned {
                assert!(
                    !names.contains(name.as_str()),
                    "owned session survived: {name}"
                );
            }
            for name in &peers {
                assert!(
                    names.contains(name.as_str()),
                    "unselected session was killed: {name}"
                );
            }
            return;
        }
        if !tmux_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tmux::utils::tests::ancillary_teardown_preserves_other_sessions_and_exact_name_peers", "--nocapture"])
            .env("AOE_TEST_ANCILLARY_KILL_CHILD", "1")
            .env("AOE_TMUX_SOCKET", root.path().join("tmux.sock"))
            .env("HOME", root.path())
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Capture must read the title published on the agent pane.
    #[test]
    #[serial_test::serial]
    fn pane_title_reads_the_panes_published_title() {
        if !tmux_available() {
            return;
        }
        let guard = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_pane_title");
        let name = guard.name();
        // Separate argv bypasses shell startup, which could overwrite the title.
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

#[cfg(test)]
mod pane_probe_tests {
    use super::*;

    /// The failure taxonomy the session-id poller depends on: a recognized
    /// "gone" stays Missing, an unrecognized failure never folds into it, so
    /// a poller that has seen the target alive is not stopped by two
    /// unexpected probes.
    #[test]
    fn classify_pane_probe_splits_missing_from_unknown() {
        use std::str::from_utf8;

        assert_eq!(classify_pane_probe(true, "0", &[]), PaneProbe::Alive);
        assert_eq!(classify_pane_probe(true, "1", &[]), PaneProbe::Dead);

        // Precise "gone": the name did not resolve (exit 0, empty stdout),
        // or tmux reported no server / a dead socket file.
        assert_eq!(classify_pane_probe(true, "", &[]), PaneProbe::Missing);
        let no_server = b"no server running on /tmp/tmux-501/default\n";
        assert_eq!(
            classify_pane_probe(false, "", no_server),
            PaneProbe::Missing
        );
        let enoent = b"error connecting to /tmp/tmux-501/default (No such file or directory)\n";
        assert_eq!(classify_pane_probe(false, "", enoent), PaneProbe::Missing);

        // Unexpected failures stay Unknown: the poller's alive-seen guard
        // only stops on two Missing probes.
        let eacces = b"error connecting to /tmp/tmux-501/default (Permission denied)\n";
        assert_eq!(classify_pane_probe(false, "", eacces), PaneProbe::Unknown);
        let enotsock =
            b"error connecting to /tmp/tmux-501/default (Socket operation on non-socket)\n";
        assert_eq!(classify_pane_probe(false, "", enotsock), PaneProbe::Unknown);
        assert_eq!(
            classify_pane_probe(false, "", b"garbage\n"),
            PaneProbe::Unknown
        );

        // Malformed stdout is not a resolvable shape either.
        assert_eq!(
            classify_pane_probe(true, "garbage", &[]),
            PaneProbe::Unknown
        );
        assert_eq!(classify_pane_probe(false, "0", &[]), PaneProbe::Alive);
        assert_eq!(classify_pane_probe(false, "1", &[]), PaneProbe::Dead);

        // The bytes really are valid UTF-8 in the shapes above, matching the
        // lossy conversion probe_pane performs.
        assert!(from_utf8(no_server).is_ok());
        assert!(from_utf8(enoent).is_ok());
    }

    /// An empty name never reaches tmux: `:^` resolves against whatever
    /// session is current, so an unrelated live pane would answer `Alive` and
    /// a poller seeded with no name would hold its budget slot forever.
    #[test]
    fn probe_pane_rejects_an_empty_session_name() {
        assert_eq!(probe_pane(""), PaneProbe::Missing);
        assert!(!is_pane_dead(""));
    }
}
