//! Pure pane-content heuristics: what a captured pane says about a
//! session's status, and how to summarize an error banner.

use super::*;

/// omp's error banner footer, and the terminal retry lines it can replace.
/// They live here rather than with detection: the manifest carries its own
/// copies for deciding status, while these drive the message this module
/// lifts out of the banner.
const OMP_BANNER_DISMISSAL_ANCHOR: &str = "dismissed when you send your next message";
const OMP_TERMINAL_RETRY_MARKERS: &[&str] =
    &["error: retry budget exhausted", "error: retry failed after"];

/// Build a short human-readable hint for why a session transitioned to Error.
///
/// Called when we set Status::Error but don't already have a `last_error`
/// populated (e.g. an agent process exited on its own). We grab the last few
/// non-empty lines of the pane and pick something that looks like an error
/// message; otherwise fall back to a generic "stopped responding" string so
/// the UI never renders an Error state without any explanation.
pub(super) fn summarize_error_from_pane(pane_content: &str) -> String {
    const MAX_BANNER_LINES: usize = 3;

    let cleaned = crate::tmux::utils::strip_ansi(pane_content);
    let tail: Vec<&str> = cleaned
        .lines()
        .rev()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .take(12)
        .collect();

    // omp pins an error banner whose dismissal footer is the anchor. When the
    // anchor is the lowest of {anchor, terminal retry lines} (positions are
    // 1-based from the bottom of the tail), the banner message is the reason:
    // walk up from the anchor (excluded), collecting the consecutive message
    // lines until the first border line (all `─`), at most MAX_BANNER_LINES.
    let anchor_idx = tail
        .iter()
        .position(|l| l.to_lowercase().contains(OMP_BANNER_DISMISSAL_ANCHOR));
    let terminal_idx = tail.iter().position(|l| {
        let lower = l.to_lowercase();
        OMP_TERMINAL_RETRY_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
    });
    if let Some(anchor_idx) = anchor_idx
        .filter(|anchor_idx| terminal_idx.is_none_or(|terminal_idx| *anchor_idx <= terminal_idx))
    {
        let mut msg_lines: Vec<&str> = Vec::new();
        for line in tail.iter().skip(anchor_idx + 1) {
            // Border line: the banner's DynamicBorder (U+2500 by default,
            // `-` under omp's ascii theme).
            let trimmed = line.trim();
            if !trimmed.is_empty() && trimmed.chars().all(|c| c == '─' || c == '-') {
                break;
            }
            msg_lines.push(line);
            if msg_lines.len() == MAX_BANNER_LINES {
                break;
            }
        }
        if !msg_lines.is_empty() {
            // Reorder to pane order (top first), trim per line, strip a
            // leading error glyph (theme-dependent), join with one space.
            let mut reason = String::new();
            for line in msg_lines.iter().rev() {
                let mut text = line.trim();
                // status.error glyphs across omp themes (✘ default, ✖
                // poimandres override, [!!] ascii, U+F00D nerd); ✕ is the
                // tool-result icon.error slot, included defensively.
                for glyph in ["✖", "✘", "✕", "[!!]", "\u{f00d}"] {
                    if let Some(rest) = text.strip_prefix(glyph) {
                        text = rest.trim_start();
                        break;
                    }
                }
                if !reason.is_empty() {
                    reason.push(' ');
                }
                reason.push_str(text);
            }
            return truncate_error_line(&reason);
        }
        // No collectable banner lines (exotic theme): fall through to the
        // word list below.
    }

    for line in &tail {
        let lower = line.to_lowercase();
        if lower.contains("error")
            || lower.contains("command not found")
            || lower.contains("permission denied")
            || lower.contains("cannot")
            || lower.contains("failed")
            || lower.contains("no such file")
            || lower.contains("traceback")
            || lower.contains("panic")
        {
            return truncate_error_line(line);
        }
    }

    if let Some(last) = tail.first() {
        return format!(
            "Agent stopped responding. Last line: {}",
            truncate_error_line(last)
        );
    }

    "Agent stopped responding and the pane is empty.".to_string()
}

fn truncate_error_line(line: &str) -> String {
    const MAX: usize = 200;
    let trimmed = line.trim();
    if trimmed.len() <= MAX {
        trimmed.to_string()
    } else {
        let mut out = String::with_capacity(MAX + 1);
        for (i, ch) in trimmed.char_indices() {
            if i >= MAX {
                break;
            }
            out.push(ch);
        }
        out.push('…');
        out
    }
}

pub(super) fn resolve_detected_status(
    detected: Status,
    is_dead: bool,
    is_shell_stale: bool,
    has_command_override: bool,
    pane_content: &str,
    tool: &str,
) -> Status {
    match detected {
        Status::Idle if has_command_override => {
            // Custom commands run agents through wrapper scripts that appear
            // as shell processes to tmux, so we can't trust the pane's current
            // command here; decide from pane *content* instead. A pane that is
            // still rendering the agent TUI is genuinely parked at its prompt,
            // so a detected Idle is real and we keep it (otherwise on_idle /
            // on_waiting status hooks never fire for wrapped agents, e.g. an
            // opencode session launched via agent_command_override, see #2022).
            // Only declare Error when the pane is actually dead; a live pane
            // without recognizable agent content stays Unknown.
            if is_dead {
                Status::Error
            } else if pane_has_agent_content(pane_content, tool) {
                Status::Idle
            } else {
                Status::Unknown
            }
        }
        Status::Idle if is_dead => Status::Error,
        Status::Idle if is_shell_stale => resolve_shell_stale_status(pane_content, tool),
        other => other,
    }
}

fn resolve_shell_stale_status(pane_content: &str, tool: &str) -> Status {
    if pane_has_agent_content(pane_content, tool) {
        Status::Idle
    } else if pane_looks_like_bare_shell_prompt(pane_content) {
        Status::Error
    } else {
        Status::Unknown
    }
}

fn pane_looks_like_bare_shell_prompt(raw_content: &str) -> bool {
    let clean = crate::tmux::utils::strip_ansi(raw_content);
    let Some(last) = clean.lines().rev().find(|l| !l.trim().is_empty()) else {
        return false;
    };
    let last = last.trim();
    last.ends_with('$') || last.ends_with('#') || last.ends_with('%') || last.ends_with('\u{276f}')
}

/// Check whether captured pane content indicates a living agent rather than
/// a bare shell prompt. Used to prevent `is_shell_stale()` from producing
/// false `Error` status when the agent binary is a shell wrapper or spawns
/// persistent child shell processes.
fn pane_has_agent_content(raw_content: &str, tool: &str) -> bool {
    let clean = crate::tmux::utils::strip_ansi(raw_content);
    let non_empty: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();

    if non_empty.is_empty() {
        return false;
    }

    // If the last visible line looks like a shell prompt, the agent likely
    // exited and the shell took over. This catches servers with verbose MOTD
    // that would otherwise exceed the line-count threshold.
    if pane_looks_like_bare_shell_prompt(raw_content) {
        return false;
    }

    // Agent TUIs fill the screen with UI elements. A bare shell prompt
    // (after MOTD) rarely exceeds this threshold once the prompt check
    // above filters out typical shell endings.
    if non_empty.len() > 5 {
        return true;
    }

    // Use word-boundary matching so short names like "pi" don't produce
    // false positives inside words like "api" or "pipeline".
    let mut tool_names = vec![tool.to_lowercase()];
    if let Some(agent) = crate::agents::get_agent(tool) {
        let binary = agent.binary.to_lowercase();
        if !tool_names.contains(&binary) {
            tool_names.push(binary);
        }
    }
    let lower = clean.to_lowercase();
    if lower
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .any(|word| tool_names.iter().any(|name| word == name))
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_error_from_pane_handles_banner_shapes() {
        let cases = [
            (
                "message above anchor",
                "────\n\
                 ✘ 401 Incorrect API key provided: sk-dummy.\n\
                 Dismissed when you send your next message.\n\
                 ────\n\
                 ╭── π  > GPT-5.6 Sol ─╮\n\
                 ╰─                   ─╯",
                "401 Incorrect API key provided: sk-dummy.",
            ),
            (
                "multiline message",
                "────\n\
                 ✖ 429 Too Many Requests (rate limited). Retry after 30s.\n\
                    This is a continuation with more detail.\n\
                    And a third line.\n\
                 Dismissed when you send your next message.\n\
                 ────\n\
                 ╭── π  > GPT-5.6 Sol ─╮\n\
                 ╰─                   ─╯",
                "429 Too Many Requests (rate limited). Retry after 30s. This is a continuation with more detail. And a third line.",
            ),
            (
                "terminal lines below stale banner",
                "────\n\
                 ✖ 429 Too Many Requests (rate limited). Retry after 30s.\n\
                 Dismissed when you send your next message.\n\
                 ────\n\
                 Error: Retry budget exhausted after 10 retries: Unable to connect. Is the computer able to access the url?\n\
                 Error: Retry failed after 10 attempts: Unable to connect. Is the computer able to access the url?\n\
                 ╭── π  > GPT-5.6 Sol ─╮\n\
                 ╰─                   ─╯",
                "Error: Retry failed after 10 attempts: Unable to connect. Is the computer able to access the url?",
            ),
            (
                "no banner",
                "building failed: no such file\n╭── π  > GPT-5.6 Sol ─╮\n╰─   ─╯",
                "building failed: no such file",
            ),
            (
                "anchor without message",
                "────\n\
                 Dismissed when you send your next message.\n\
                 ────\n\
                 building failed: no such file\n\
                 ╭── π  > GPT-5.6 Sol ─╮\n\
                 ╰─                   ─╯",
                "building failed: no such file",
            ),
        ];

        for (name, pane, expected) in cases {
            assert_eq!(summarize_error_from_pane(pane), expected, "{name}");
        }
    }

    #[test]
    fn test_pane_has_agent_content_bare_shell() {
        assert!(!pane_has_agent_content("$ ", "opencode"));
        assert!(!pane_has_agent_content("user@host:~$ ", "opencode"));
        assert!(!pane_has_agent_content("\n\n$ \n", "opencode"));
    }

    #[test]
    fn test_resolve_detected_status_shell_stale_agent_content_stays_idle() {
        let content = "ctrl+p commands \u{2022} OpenCode 1.3.13+650d0db";
        assert_eq!(
            resolve_detected_status(Status::Idle, false, true, false, content, "opencode"),
            Status::Idle
        );
    }

    #[test]
    fn test_resolve_detected_status_shell_stale_bare_prompt_is_error() {
        assert_eq!(
            resolve_detected_status(
                Status::Idle,
                false,
                true,
                false,
                "Welcome\nuser@host:~$ ",
                "opencode",
            ),
            Status::Error
        );
    }

    #[test]
    fn test_resolve_detected_status_shell_stale_unclear_is_unknown() {
        assert_eq!(
            resolve_detected_status(
                Status::Idle,
                false,
                true,
                false,
                "Restoring previous session...",
                "opencode",
            ),
            Status::Unknown
        );
        assert_eq!(
            resolve_detected_status(Status::Idle, false, true, false, "", "opencode"),
            Status::Unknown
        );
    }

    #[test]
    fn test_resolve_detected_status_keeps_hard_failures_as_error() {
        assert_eq!(
            resolve_detected_status(Status::Idle, true, false, false, "", "opencode"),
            Status::Error
        );
        assert_eq!(
            resolve_detected_status(Status::Idle, true, true, true, "", "opencode"),
            Status::Error
        );
    }

    #[test]
    fn test_resolve_detected_status_live_command_override_is_unknown() {
        assert_eq!(
            resolve_detected_status(Status::Idle, false, true, true, "$ ", "opencode"),
            Status::Unknown
        );
    }

    #[test]
    fn test_resolve_detected_status_command_override_agent_content_stays_idle() {
        // A wrapped agent (agent_command_override) whose pane still renders the
        // agent TUI must keep its detected Idle so on_idle / on_waiting status
        // hooks fire; previously the override masked every Idle to Unknown and
        // those hooks never ran (#2022).
        let content = "ctrl+p commands \u{2022} OpenCode 1.16.2";
        assert_eq!(
            resolve_detected_status(Status::Idle, false, false, true, content, "opencode"),
            Status::Idle
        );
    }

    #[test]
    fn test_pane_has_agent_content_agent_ui() {
        let opencode_idle = "ctrl+p commands \u{2022} OpenCode 1.3.13+650d0db";
        assert!(pane_has_agent_content(opencode_idle, "opencode"));
    }

    #[test]
    fn test_pane_has_agent_content_substantial_output() {
        let many_lines = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(pane_has_agent_content(&many_lines, "vibe"));
    }

    #[test]
    fn test_pane_has_agent_content_empty() {
        assert!(!pane_has_agent_content("", "opencode"));
        assert!(!pane_has_agent_content("   \n  \n  ", "opencode"));
    }

    #[test]
    fn test_pane_has_agent_content_shell_prompt_at_end() {
        // Verbose MOTD followed by shell prompt should be detected as a
        // bare shell, not agent content, even with >5 lines.
        let motd_then_prompt = "Welcome to Ubuntu 22.04 LTS\n\
            System load:  0.5\n\
            Memory usage: 42%\n\
            Disk usage:   67%\n\
            Swap usage:   0%\n\
            Temperature:  45C\n\
            2 updates available\n\
            user@host:~$ ";
        assert!(!pane_has_agent_content(motd_then_prompt, "opencode"));

        // Same with # prompt (root)
        let root_prompt = "line1\nline2\nline3\nline4\nline5\nline6\n# ";
        assert!(!pane_has_agent_content(root_prompt, "opencode"));

        // Fish/zsh fancy prompt (❯)
        let fancy_prompt = "line1\nline2\nline3\nline4\nline5\nline6\n\u{276f}";
        assert!(!pane_has_agent_content(fancy_prompt, "opencode"));
    }

    #[test]
    fn test_pane_has_agent_content_short_tool_name() {
        // Short tool names like "pi" should NOT match substrings in
        // unrelated content (e.g., "api" contains "pi").
        assert!(!pane_has_agent_content("api endpoint ready", "pi"));
        assert!(!pane_has_agent_content("pipeline started", "pi"));

        // But "pi" as a standalone word should match.
        assert!(pane_has_agent_content("pi file saved", "pi"));
        assert!(pane_has_agent_content("done\npi>", "pi"));

        // Longer names like "opencode" should still match.
        assert!(pane_has_agent_content("OpenCode v1.0", "opencode"));
    }

    #[test]
    fn test_pane_has_agent_content_matches_agent_binary_alias() {
        assert!(pane_has_agent_content("agy ready", "antigravity"));
    }
}
