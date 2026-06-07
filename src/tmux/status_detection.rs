//! OMP status detection for agent sessions.

use crate::session::Status;

use super::utils::strip_ansi;

const SPINNER_CHARS: &[&str] = &[
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⠘", "⠣", "⠆", "⠳", "⠰", "⠞", "⣻", "·", "✢",
    "✳", "✶", "✻", "✽", "*", "●",
];

const LIVE_ACTIVITY_WORDS: &[&str] = &[
    "thinking",
    "working",
    "processing",
    "running",
    "searching",
    "reading",
    "writing",
    "editing",
    "executing",
    "analyzing",
    "calling",
    "using",
    "loading",
];

pub fn detect_status_from_content(content: &str, tool: &str) -> Status {
    if tool == "omp" {
        detect_omp_status(content)
    } else {
        Status::Idle
    }
}

pub(crate) fn reconcile_claude_hook_status(hook_status: Status, raw_content: &str) -> Status {
    if hook_status == Status::Running && has_approval_prompt(raw_content) {
        Status::Waiting
    } else {
        hook_status
    }
}

pub(crate) fn reconcile_codex_hook_status(hook_status: Status, raw_content: &str) -> Status {
    reconcile_claude_hook_status(hook_status, raw_content)
}

pub fn detect_omp_status(raw_content: &str) -> Status {
    let stripped = strip_ansi(raw_content);
    let text_lower = stripped.to_lowercase();

    if has_approval_prompt(&text_lower) {
        return Status::Waiting;
    }

    let lines: Vec<&str> = stripped
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let recent: Vec<&str> = lines.iter().rev().take(12).copied().collect();

    if text_lower.contains("esc to interrupt")
        || text_lower.contains("ctrl+c to interrupt")
        || text_lower.contains(" tokens")
        || has_spinner_activity_line(&recent)
        || has_live_activity_word(&text_lower)
    {
        return Status::Running;
    }

    Status::Idle
}

fn has_approval_prompt(text: &str) -> bool {
    let lower = text.to_lowercase();
    (lower.contains("do you want to proceed")
        || lower.contains("would you like to proceed")
        || lower.contains("approve")
        || lower.contains("permission"))
        && (lower.contains("1.") || lower.contains("yes"))
}

fn has_live_activity_word(text_lower: &str) -> bool {
    LIVE_ACTIVITY_WORDS
        .iter()
        .any(|word| text_lower.contains(word))
}

fn has_spinner_activity_line(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        SPINNER_CHARS.iter().any(|spinner| line.contains(spinner))
            && LIVE_ACTIVITY_WORDS.iter().any(|word| {
                let lower = line.to_lowercase();
                lower.contains(word) || lower.contains('…')
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_omp_running_from_interrupt_hint() {
        assert_eq!(
            detect_status_from_content("✶ Working…\nesc to interrupt", "omp"),
            Status::Running
        );
    }

    #[test]
    fn detects_omp_waiting_from_permission_prompt() {
        assert_eq!(
            detect_status_from_content("Do you want to proceed?\n1. Yes\n2. No", "omp"),
            Status::Waiting
        );
    }

    #[test]
    fn unknown_tool_is_idle() {
        assert_eq!(
            detect_status_from_content("✶ Working…\nesc to interrupt", "claude"),
            Status::Idle
        );
    }
}
