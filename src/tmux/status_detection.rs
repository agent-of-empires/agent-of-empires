//! Status detection for agent sessions

use crate::session::Status;

use super::utils::strip_ansi;

const SPINNER_CHARS: &[&str] = &[
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⠘", "⠣", "⠆", "⠳", "⠰", "⠞", "⣻",
];
const LIVE_ACTIVITY_WORDS: &[&str] = &[
    "analyzing",
    "applying",
    "building",
    "editing",
    "executing",
    "fetching",
    "generating",
    "grepping",
    "processing",
    "reading",
    "running",
    "searching",
    "testing",
    "thinking",
    "working",
    "writing",
];
const COMPLETED_ACTIVITY_MARKERS: &[&str] = &[
    "complete",
    "completed",
    "done",
    "finished",
    "success",
    "successful",
    "successfully",
];

fn has_any_spinner(lines: &[&str]) -> bool {
    lines
        .iter()
        .any(|line| SPINNER_CHARS.iter().any(|s| line.contains(s)))
}

fn has_live_activity_word(text_lower: &str) -> bool {
    LIVE_ACTIVITY_WORDS
        .iter()
        .any(|word| status_line_starts_with_phrase(text_lower.trim(), word))
}

fn has_spinner_activity_line(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let line_lower = line.to_lowercase();
        has_any_spinner(&[*line])
            && LIVE_ACTIVITY_WORDS
                .iter()
                .any(|word| line_lower.contains(word))
    })
}

fn contains_approval_prompt(text_lower: &str, extra: &[&str]) -> bool {
    const BASE: &[&str] = &["(y/n)", "[y/n]", "approve", "allow"];
    BASE.iter()
        .chain(extra.iter())
        .any(|p| text_lower.contains(p))
}

fn matches_input_prompt(non_empty_lines: &[&str], take_n: usize, tool_prompts: &[&str]) -> bool {
    for line in non_empty_lines.iter().rev().take(take_n) {
        let clean_line = strip_ansi(line).trim().to_string();
        if clean_line == ">" {
            return true;
        }
        if tool_prompts.iter().any(|p| clean_line == *p) {
            return true;
        }
        if clean_line.starts_with("> ") && !clean_line.contains("esc") && clean_line.len() < 100 {
            return true;
        }
    }
    false
}

pub fn detect_status_from_content(content: &str, tool: &str) -> Status {
    // Strip ANSI escape codes before passing to detectors. capture-pane is
    // called with -e (to preserve colors for the TUI preview), but color codes
    // interspersed in text like "esc interrupt" break plain substring matches.
    let clean = strip_ansi(content);
    crate::agents::get_agent(tool)
        .map(|a| (a.detect_status)(&clean))
        .unwrap_or(Status::Idle)
}

/// Spinner frame characters Claude Code rotates through next to its active
/// verb. macOS uses `· ✢ ✳ ✶ ✻ ✽`, other platforms swap `✽` for `*`, and
/// reduced-motion mode renders a static `●`.
const CLAUDE_SPINNER_CHARS: &[char] = &['·', '✢', '✳', '✶', '✻', '✽', '*', '●'];

/// The banner Claude renders after the user cancels a turn with Esc:
/// `⎿  Interrupted · What should Claude do instead?`. We key on the
/// distinctive tail so a differently rendered separator doesn't break the
/// match. This is the positive signal that an interrupted turn has parked at
/// the prompt; see `reconcile_claude_hook_status`.
const CLAUDE_INTERRUPT_MARKER: &str = "what should claude do instead";

/// Claude Code status is primarily detected via hooks (file-based) installed
/// in `~/.claude/settings.json`. When hooks aren't reachable (first few
/// seconds before a hook fires, custom `--cmd` wrappers, `docker exec` into
/// a user-managed container that aoe didn't provision), the dispatcher falls
/// back to this pane-based detector.
///
/// The dispatcher strips ANSI before calling us, so we only match on
/// human-readable text shapes:
///   1. The interrupt hint ("esc to interrupt" / "ctrl+c to interrupt").
///   2. The live token counter ("(4s · ↓ 88 tokens)") that only renders
///      while a turn is generating.
///   3. The spinner+verb shape ("✶ Working…") on a recent line.
///   4. The parked background-agent wait line ("✻ Waiting for 1 background
///      agent to finish").
///
/// The `…` in shape (3) is what distinguishes active from completed lines.
/// Claude renders active verbs as gerunds with a trailing `…` (`Working…`)
/// and past-tense completions without one (`Worked for 1m 52s`), so we
/// don't need a separate past-tense verb list. Shape (4) is the one active
/// state rendered without an ellipsis; it gets its own structural match.
pub fn detect_claude_status(content: &str) -> Status {
    // Claude often leaves the bottom of the pane blank (cursor parked below
    // the spinner line, or a small response in a tall pane), so we filter
    // empty lines first and look at the last 30 non-empty lines. Matches
    // the pattern used by detect_opencode_status and friends.
    let non_empty: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let recent: Vec<&str> = non_empty.iter().rev().take(30).rev().copied().collect();
    let recent_joined = recent.join("\n");
    let recent_lower = recent_joined.to_lowercase();

    // A blocking approval prompt has to outrank the spinner. Claude keeps its
    // live "Working…" line rendered *below* the permission prompt while it
    // waits for the user, so a sandboxed session (whose in-container hook
    // status the host can't read, see `claude_poll_fn_sandboxed`) would
    // otherwise match the spinner and report Running the whole time it is
    // blocked. See #1913.
    if claude_has_approval_prompt(&recent, &recent_lower) {
        return Status::Waiting;
    }

    if claude_pane_has_running_signal(&recent, &recent_joined, &recent_lower) {
        return Status::Running;
    }

    Status::Idle
}

/// True when the recent pane lines show that a turn is actively generating or
/// the session is otherwise still working: the interrupt hint, the live token
/// counter, the spinner+verb shape, or the parked background-agent wait line.
/// `recent_joined` and `recent_lower` are the join/lowercased-join of `recent`,
/// passed in so callers that already computed them don't redo the work.
fn claude_pane_has_running_signal(
    recent: &[&str],
    recent_joined: &str,
    recent_lower: &str,
) -> bool {
    // The interrupt hints are checked on a whitespace-collapsed join as well:
    // a narrow pane word-wraps the footer, and a break inside the hint
    // ("... · esc\n  to interrupt · ...") would otherwise hide the running
    // signal while the parked markers on the other footer fragment survive,
    // flipping an active turn to Idle. False joins across unrelated lines
    // only bias toward Running, the safe direction.
    let collapsed = collapse_ascii_whitespace(recent_lower);
    if collapsed.contains("esc to interrupt") || collapsed.contains("ctrl+c to interrupt") {
        return true;
    }
    if has_claude_live_token_counter(recent_joined) {
        return true;
    }
    recent
        .iter()
        .any(|line| claude_line_is_active_spinner(line) || claude_line_is_background_wait(line))
}

/// Detect the live token counter Claude Code prints during generation,
/// e.g. `(4s · ↓ 88 tokens)`. The parenthesized `s · ↓ N tokens)` shape is
/// unique to the active counter on the spinner line.
///
/// The background-agents strip below the input footer renders unparenthesized
/// counters (`1m 14s · ↓ 40.4k tokens`) and stays on screen, frozen at its
/// final values, after the agent completes and the session is fully idle.
/// Matching it would pin a parked session on Running (the bug #2909 fixed),
/// so two structural requirements exclude it: the count must be a plain
/// integer (no `40.4k` decimal/suffix forms) and `tokens` must be followed by
/// the counter's closing paren, which strip rows never have.
fn has_claude_live_token_counter(content: &str) -> bool {
    let mut search = content;
    while let Some(pos) = search.find("s · ↓") {
        let after = search[pos + "s · ↓".len()..].trim_start();
        let mut digits_end = 0;
        for (i, c) in after.char_indices() {
            if c.is_ascii_digit() {
                digits_end = i + c.len_utf8();
            } else {
                break;
            }
        }
        if digits_end > 0 {
            let tail = after[digits_end..].trim_start();
            if let Some(after_tokens) = tail.strip_prefix("tokens") {
                if after_tokens.trim_start().starts_with(')') {
                    return true;
                }
            }
        }
        // Advance past this match so we don't loop on the same position.
        search = &search[pos + "s · ↓".len()..];
    }
    false
}

/// Match the `<frame> <Verb…>` shape on a single pane line. The ellipsis must
/// be inside the first or second word after the frame char: single-verb lines
/// end it on word one (`Working…`), and compaction ends it on word two
/// (`✢ Compacting conversation… (17s)`, captured from 2.1.211). Later words
/// don't count, so past-tense completions (`Worked for 1m 52s`, no `…`) and
/// rendered markdown bullets (`* Cooked an amazing dish today…`, `…` several
/// words in) stay rejected.
fn claude_line_is_active_spinner(line: &str) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !CLAUDE_SPINNER_CHARS.contains(&first) {
        return false;
    }
    let rest = chars.as_str().trim_start();
    if rest.is_empty() {
        return false;
    }

    let mut words = rest.split_whitespace();
    let Some(first_word) = words.next() else {
        return false;
    };
    if !first_word.chars().next().is_some_and(|c| c.is_uppercase()) {
        return false;
    }
    first_word.contains('…') || words.next().is_some_and(|w| w.contains('…'))
}

/// Match the parked background-agent wait line: `✻ Waiting for 1 background
/// agent to finish`. The main REPL is between turns while background agents
/// run, so the pane shows the idle input box with this status line above it,
/// but the session is still working. It has no ellipsis in the first word, so
/// `claude_line_is_active_spinner` misses it; without a dedicated match the
/// pane reads as parked-idle and the reconciler flip-flops the session between
/// Idle (age-gated downgrade during tool gaps) and Running (each background
/// agent PreToolUse rewrites the status file).
///
/// The full `Waiting for <N> background agent(s) to finish` structure is
/// required, not just a substring: Claude prefixes assistant prose with `●`
/// and renders markdown bullets as `*` (both in `CLAUDE_SPINNER_CHARS`), so a
/// loose match on response text like "● Waiting for background agent results"
/// would pin an idle session on Running with no recovery path. The digit
/// count and the exact `to finish` tail are what ordinary prose lacks.
fn claude_line_is_background_wait(line: &str) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !CLAUDE_SPINNER_CHARS.contains(&first) {
        return false;
    }
    let rest = chars.as_str().trim().to_lowercase();
    let Some(count_and_tail) = rest.strip_prefix("waiting for ") else {
        return false;
    };
    let digits_end = count_and_tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(count_and_tail.len());
    if digits_end == 0 {
        return false;
    }
    let tail = count_and_tail[digits_end..].trim_start();
    tail.starts_with("background agent") && tail.ends_with("to finish")
}

/// Claude renders a blocking approval prompt when a tool needs the user's
/// permission (Bash command, file edit, plan exit, ...). Every variant pairs
/// a yes/no question ("Do you want to proceed?", "Do you want to make this
/// edit to <file>?", "Would you like to proceed?") with a numbered choice
/// menu. Requiring both keeps an assistant-authored numbered list from being
/// mistaken for a prompt. `recent_lower` is the lowercased join of `recent`.
fn claude_has_approval_prompt(recent: &[&str], recent_lower: &str) -> bool {
    let has_question = recent_lower.contains("do you want to")
        || recent_lower.contains("would you like to proceed");
    has_question
        && recent
            .iter()
            .any(|line| claude_line_is_numbered_choice(line))
}

/// A numbered menu option, optionally preceded by the `❯`/`>` selection
/// cursor: `❯ 1. Yes`, `2. No`, `3. No, and tell Claude ...`.
fn claude_line_is_numbered_choice(line: &str) -> bool {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix('❯')
        .or_else(|| trimmed.strip_prefix('>'))
        .map(str::trim_start)
        .unwrap_or(trimmed);
    let mut chars = rest.chars();
    matches!(chars.next(), Some('1'..='9')) && matches!(chars.next(), Some('.'))
}

/// Strip ANSI and scan the recent pane lines for an approval prompt. Shares
/// the same recent-window shape as `detect_claude_status`; this entry point
/// exists for callers that hold raw (un-stripped) `capture-pane -e` output.
fn claude_pane_has_approval_prompt(raw_content: &str) -> bool {
    let clean = strip_ansi(raw_content);
    let non_empty: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    let recent: Vec<&str> = non_empty.iter().rev().take(30).rev().copied().collect();
    let recent_lower = recent.join("\n").to_lowercase();
    claude_has_approval_prompt(&recent, &recent_lower)
}

/// Claude has parked at the prompt after the user cancelled a turn with Esc.
/// That path fires neither `Stop` nor an `idle_prompt` notification (verified
/// against Claude Code 2.1.193: the `idle_prompt` timer is armed by turn
/// completion, and an interrupt produces no completion), so the hook status
/// file stays on its last `running` write. We require the interrupt banner
/// *and* the absence of any active-turn signal so that a fresh turn started
/// right after the interrupt (banner still in scrollback, spinner now showing)
/// still reads as Running.
fn claude_pane_shows_interrupted_turn(raw_content: &str) -> bool {
    let clean = strip_ansi(raw_content);
    let non_empty: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    let recent: Vec<&str> = non_empty.iter().rev().take(30).rev().copied().collect();
    let recent_joined = recent.join("\n");
    let recent_lower = recent_joined.to_lowercase();
    recent_lower.contains(CLAUDE_INTERRUPT_MARKER)
        && !claude_pane_has_running_signal(&recent, &recent_joined, &recent_lower)
}

/// How long a `running` hook write must have been standing before a pane that
/// looks parked at the idle prompt is trusted over it. The idle ready-prompt
/// pane is identical whether Claude just finished a turn (the hook missed the
/// idle write, file stuck on `running`) or the user just submitted a prompt and
/// the spinner hasn't rendered yet. The two are told apart by age: the
/// start-of-turn gap resolves within ~1s (a running-mapped hook just wrote the
/// file), while a stuck value has been standing since the turn's last tool
/// call.
///
/// The threshold is sized for cost asymmetry, not just the render gap. A false
/// downgrade flaps a working session to Idle (the original 6s gate did this on
/// every >6s tool gap while a background-agent wait pane went unrecognized,
/// #2909 regression); a late one only means a silently-finished session shows
/// Running a bit longer. The ready-prompt detector string-matches a
/// third-party TUI that changes between releases, so keep wide margin against
/// the next unrecognized running state.
const IDLE_RECONCILE_MIN_RUNNING_AGE: std::time::Duration = std::time::Duration::from_secs(30);

/// Claude has finished a turn and parked at the idle ready prompt, but no idle
/// hook fired (the "silent tool stop" path: a tool result followed by no text
/// fires neither `Stop` nor `idle_prompt`), so the status file is stuck on
/// `running`. The positive marker is Claude's empty input prompt (a bare `❯`
/// line, distinct from a numbered `❯ 1.` menu) or one of its input-box
/// footers, combined with the absence of any active-turn signal. Requiring a
/// positive ready-prompt marker (not merely "no spinner") keeps a blank or
/// mid-redraw capture from reading as Idle.
///
/// Two footer markers are needed because the footer varies by permission
/// mode: manual mode shows `? for shortcuts`, while the mode-cycle footers
/// drop it (all verified against 2.1.211: `⏵⏵ accept edits on`,
/// `⏸ plan mode on`, `⏵⏵ auto mode on`, and `⏵⏵ bypass permissions on`, each
/// with `(shift+tab to cycle)`). Without the second marker, bypass-mode
/// sessions had no footer match, and ghost suggestion text (a pre-filled
/// follow-up rendered on the `❯` line within a couple seconds of turn end)
/// defeats the bare-prompt marker, so silent stops stayed stuck on Running.
/// The marker text is identical while running and while parked: the running
/// variant only appends `esc to interrupt`, which the running-signal check
/// catches first.
///
/// The mode-cycle marker is anchored to a line starting with the footer's
/// `⏵`/`⏸` glyph rather than matched as a bare substring, so panes merely
/// echoing the footer text (a `git diff` of this file, quoted docs, this
/// repo's own test fixtures in tool output) don't read as parked.
fn claude_pane_shows_ready_prompt(raw_content: &str) -> bool {
    let clean = strip_ansi(raw_content);
    let non_empty: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    let recent: Vec<&str> = non_empty.iter().rev().take(30).rev().copied().collect();
    let recent_joined = recent.join("\n");
    let recent_lower = recent_joined.to_lowercase();

    let has_empty_prompt = recent.iter().any(|line| line.trim() == "❯");
    let has_idle_footer = recent_lower.contains("? for shortcuts")
        || recent.iter().any(|line| {
            let trimmed = line.trim_start();
            (trimmed.starts_with('⏵') || trimmed.starts_with('⏸'))
                && trimmed.to_lowercase().contains("shift+tab to cycle")
        });
    (has_empty_prompt || has_idle_footer)
        && !claude_pane_has_running_signal(&recent, &recent_joined, &recent_lower)
}

/// When Claude's status hook reports Running, the pane is consulted to catch two
/// cases the hook stream can't express on its own:
///
/// 1. A blocking approval prompt: Claude keeps its live spinner rendered below
///    the prompt and re-emits running-mapped hook events (`PreToolUse`,
///    `UserPromptSubmit`) while it waits, so the last hook write stays
///    `running` even though the agent is blocked on the user. Downgrade to
///    Waiting. See #1913.
/// 2. An Esc-interrupted turn: cancelling a turn fires no `Stop` and no
///    `idle_prompt`, so the status file sticks on `running` indefinitely.
///    Downgrade to Idle when the pane shows the interrupt banner and no
///    active-turn signal.
/// 3. A completed turn whose idle hook never fired (the "silent tool stop":
///    a tool result with no following text fires neither `Stop` nor
///    `idle_prompt`). The pane parks at the idle ready prompt with no
///    active-turn signal, but that is also how a just-started turn looks
///    before its spinner renders, so this downgrade is gated on the `running`
///    write having been standing for `IDLE_RECONCILE_MIN_RUNNING_AGE`.
///    `running_age` is how long ago the status file was last written (its mtime
///    elapsed); `None` (age unavailable) is treated as not-yet-stale so we
///    never downgrade on missing evidence.
///
/// Otherwise trust the hook. Mirrors `reconcile_codex_hook_status`'s
/// positive-evidence approach so an active turn whose pane hasn't rendered a
/// spinner yet keeps Running rather than flickering Idle.
pub(crate) fn reconcile_claude_hook_status(
    hook_status: Status,
    raw_content: &str,
    running_age: Option<std::time::Duration>,
) -> Status {
    if hook_status != Status::Running {
        return hook_status;
    }
    if claude_pane_has_approval_prompt(raw_content) {
        return Status::Waiting;
    }
    if claude_pane_shows_interrupted_turn(raw_content) {
        return Status::Idle;
    }
    if running_age.is_some_and(|age| age >= IDLE_RECONCILE_MIN_RUNNING_AGE)
        && claude_pane_shows_ready_prompt(raw_content)
    {
        return Status::Idle;
    }
    hook_status
}

pub fn detect_opencode_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");
    let last_lines_lower = last_lines.to_lowercase();

    if last_lines_lower.contains("esc to interrupt") || last_lines_lower.contains("esc interrupt") {
        return Status::Running;
    }

    if has_any_spinner(&lines) {
        return Status::Running;
    }

    if contains_approval_prompt(
        &last_lines_lower,
        &["continue?", "proceed?", "enter to select", "esc to cancel"],
    ) {
        return Status::Waiting;
    }

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with("❯") && trimmed.len() > 2 {
            let after_cursor = trimmed.get(3..).unwrap_or("").trim_start();
            if after_cursor.starts_with("1.")
                || after_cursor.starts_with("2.")
                || after_cursor.starts_with("3.")
            {
                return Status::Waiting;
            }
        }
    }
    if lines.iter().any(|line| {
        line.contains("❯") && (line.contains(" 1.") || line.contains(" 2.") || line.contains(" 3."))
    }) {
        return Status::Waiting;
    }

    if matches_input_prompt(&non_empty_lines, 10, &[">>"]) {
        return Status::Waiting;
    }

    // Completion indicators + input prompt nearby
    let completion_indicators = [
        "complete",
        "done",
        "finished",
        "ready",
        "what would you like",
        "what else",
        "anything else",
        "how can i help",
        "let me know",
    ];
    let has_completion = completion_indicators
        .iter()
        .any(|ind| last_lines_lower.contains(ind));
    if has_completion {
        for line in non_empty_lines.iter().rev().take(10) {
            let clean = strip_ansi(line).trim().to_string();
            if clean == ">" || clean == ">>" {
                return Status::Waiting;
            }
        }
    }

    Status::Idle
}

pub fn detect_vibe_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");
    let last_lines_lower = last_lines.to_lowercase();

    // Vibe uses Textual TUI which can render text vertically (one char per line).
    // Join recent single-char lines to reconstruct words for detection.
    let recent_text: String = non_empty_lines
        .iter()
        .rev()
        .take(50)
        .rev()
        .map(|l| l.trim())
        .collect::<Vec<&str>>()
        .join("");
    let recent_text_lower = recent_text.to_lowercase();

    if last_lines_lower.contains("↑↓ navigate")
        || last_lines_lower.contains("enter select")
        || last_lines_lower.contains("esc reject")
    {
        return Status::Waiting;
    }

    if last_lines.contains("⚠") && last_lines_lower.contains("command") {
        return Status::Waiting;
    }

    let approval_options = [
        "yes and always allow",
        "no and tell the agent",
        "› 1.",
        "› 2.",
        "› 3.",
    ];
    for option in &approval_options {
        if last_lines_lower.contains(option) {
            return Status::Waiting;
        }
    }

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with("›") && trimmed.len() > 2 {
            return Status::Waiting;
        }
    }

    for spinner in SPINNER_CHARS {
        if recent_text.contains(spinner) {
            return Status::Running;
        }
    }

    let activity_indicators = [
        "running",
        "reading",
        "writing",
        "executing",
        "processing",
        "generating",
        "thinking",
    ];
    for indicator in &activity_indicators {
        if recent_text_lower.contains(indicator) {
            return Status::Running;
        }
    }

    if recent_text.ends_with("…") || recent_text.ends_with("...") {
        return Status::Running;
    }

    Status::Idle
}

/// Fallback Codex status detection from pane text. Strategy, in priority order:
///
///   1. Structured Plan-mode radio prompts win immediately, since Codex
///      sometimes renders these alongside a stale spinner from earlier in the
///      turn.
///   2. Running is detected from the *current turn block* only, i.e. the lines
///      below the most recent `─ Worked for ... ─` divider. This stops stale
///      `• Working ...` markers from a previous turn leaking into a turn that
///      has already completed.
///   3. Within the current block we look for two shapes: a bullet-prefixed
///      live status line carrying an `esc to interrupt` hint (anywhere in the
///      block), or a bare activity verb / spinner+verb in the last ~10 lines.
///   4. Waiting is detected from approval prompts and numbered `›`/`❯`
///      choices. A normal free-form prompt means the turn is done.
///
/// All comparisons are case-insensitive (content is lowercased on entry).
pub fn detect_codex_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");
    let last_lines_lower = last_lines.to_lowercase();

    if codex_has_plan_radio_prompt(&non_empty_lines) {
        return Status::Waiting;
    }

    if codex_has_running_signal(&non_empty_lines) {
        return Status::Running;
    }

    if contains_approval_prompt(
        &last_lines_lower,
        &[
            "continue?",
            "proceed?",
            "execute?",
            "run command?",
            "enter to select",
            "esc to cancel",
        ],
    ) {
        return Status::Waiting;
    }

    if codex_has_recent_numbered_choice_prompt(&non_empty_lines) {
        return Status::Waiting;
    }

    if codex_has_interrupted_turn_without_new_activity(&non_empty_lines) {
        return Status::Idle;
    }

    Status::Idle
}

pub(crate) fn reconcile_codex_hook_status(hook_status: Status, raw_content: &str) -> Status {
    if hook_status != Status::Running {
        return hook_status;
    }

    detect_codex_hook_gap_status(raw_content).unwrap_or(hook_status)
}

fn detect_codex_hook_gap_status(raw_content: &str) -> Option<Status> {
    let clean = strip_ansi(raw_content);
    let content = clean.to_lowercase();
    let non_empty_lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    // A cancelled Plan-mode radio prompt remains in scrollback above the
    // interruption marker, so the newer interruption must win here.
    if codex_has_interrupted_turn_without_new_activity(&non_empty_lines) {
        return Some(Status::Idle);
    }

    if codex_has_plan_radio_prompt(&non_empty_lines)
        || codex_has_recent_numbered_choice_prompt(&non_empty_lines)
    {
        return Some(Status::Waiting);
    }

    if codex_has_completed_turn_prompt(&non_empty_lines) {
        return Some(Status::Idle);
    }

    if codex_has_completed_review_prompt(&non_empty_lines) {
        return Some(Status::Idle);
    }

    None
}

fn codex_has_plan_radio_prompt(non_empty_lines: &[&str]) -> bool {
    let recent_start = non_empty_lines.len().saturating_sub(40);
    let recent = &non_empty_lines[recent_start..];

    let Some(question_index) = recent.iter().rposition(|line| {
        let trimmed = line.trim();
        trimmed.starts_with("question ") && trimmed.contains("unanswered")
    }) else {
        return false;
    };
    let Some(choice_index) = recent
        .iter()
        .rposition(|line| codex_line_has_numbered_choice_cursor(line.trim()))
    else {
        return false;
    };
    let Some(submit_hint_index) = recent
        .iter()
        .rposition(|line| line.contains("enter to submit answer"))
    else {
        return false;
    };

    if !(question_index <= choice_index && choice_index <= submit_hint_index) {
        return false;
    }

    !codex_has_running_signal(&recent[submit_hint_index + 1..])
}

fn codex_line_has_numbered_choice_cursor(line: &str) -> bool {
    let Some(rest) = line
        .strip_prefix("❯")
        .or_else(|| line.strip_prefix("›"))
        .map(str::trim_start)
    else {
        return false;
    };

    let mut chars = rest.chars();
    matches!(chars.next(), Some('1'..='9')) && matches!(chars.next(), Some('.'))
}

fn codex_has_recent_numbered_choice_prompt(non_empty_lines: &[&str]) -> bool {
    let recent_start = non_empty_lines.len().saturating_sub(10);
    let recent = &non_empty_lines[recent_start..];
    let Some(choice_index) = recent
        .iter()
        .rposition(|line| codex_line_has_numbered_choice_cursor(line.trim()))
    else {
        return false;
    };
    let lines_after_choice = &recent[choice_index + 1..];

    !codex_has_running_signal(lines_after_choice)
        && !codex_has_non_numbered_cursor_prompt(lines_after_choice)
}

fn codex_has_non_numbered_cursor_prompt(non_empty_lines: &[&str]) -> bool {
    non_empty_lines
        .iter()
        .any(|line| codex_is_non_numbered_cursor_prompt(line.trim()))
}

fn codex_has_tail_non_numbered_cursor_prompt(non_empty_lines: &[&str]) -> bool {
    let Some(prompt_index) = non_empty_lines
        .iter()
        .rposition(|line| codex_is_non_numbered_cursor_prompt(line.trim()))
    else {
        return false;
    };

    non_empty_lines[prompt_index + 1..]
        .iter()
        .all(|line| codex_is_terminal_footer_line(line.trim()))
}

fn codex_is_non_numbered_cursor_prompt(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("❯").or_else(|| line.strip_prefix("›")) else {
        return false;
    };

    !rest.trim_start().is_empty() && !codex_line_has_numbered_choice_cursor(line)
}

// The footer Codex prints under its input prompt looks like
// `gpt-5.5 xhigh fast · ~/project`. The model-prefix list is intentionally
// narrow so unrelated lines (e.g. assistant prose containing ` · `) don't
// accidentally satisfy the tail check. If Codex ships a new model family
// prefix this list needs to grow; the safe failure mode is that the hook
// keeps reporting Running until it catches up on its own.
fn codex_is_terminal_footer_line(line: &str) -> bool {
    line.contains(" · ")
        && (line.starts_with("gpt-") || line.starts_with("o3") || line.starts_with("o4"))
}

fn codex_has_interrupted_turn_without_new_activity(non_empty_lines: &[&str]) -> bool {
    let Some(marker_index) = codex_interruption_marker_end_index(non_empty_lines) else {
        return false;
    };

    let lines_after_marker = &non_empty_lines[marker_index + 1..];
    if codex_has_running_signal(lines_after_marker)
        || codex_has_plan_radio_prompt(lines_after_marker)
        || codex_has_recent_numbered_choice_prompt(lines_after_marker)
        || codex_has_approval_prompt(lines_after_marker)
        || codex_cursor_prompt_count(lines_after_marker) > 1
    {
        return false;
    }

    true
}

fn codex_has_completed_turn_prompt(non_empty_lines: &[&str]) -> bool {
    codex_has_idle_prompt_after_marker(non_empty_lines, |line| {
        codex_is_completed_work_divider(line.trim())
    })
}

fn codex_has_completed_review_prompt(non_empty_lines: &[&str]) -> bool {
    codex_has_idle_prompt_after_marker(non_empty_lines, |line| {
        line.trim().contains("<< code review finished >>")
    })
}

fn codex_has_idle_prompt_after_marker(
    non_empty_lines: &[&str],
    is_marker: impl Fn(&str) -> bool,
) -> bool {
    let Some(marker_index) = non_empty_lines.iter().rposition(|line| is_marker(line)) else {
        return false;
    };

    let lines_after_marker = &non_empty_lines[marker_index + 1..];
    !codex_has_running_signal(lines_after_marker)
        && !codex_has_plan_radio_prompt(lines_after_marker)
        && !codex_has_recent_numbered_choice_prompt(lines_after_marker)
        && !codex_has_approval_prompt(lines_after_marker)
        && codex_has_tail_non_numbered_cursor_prompt(lines_after_marker)
}

fn codex_interruption_marker_end_index(non_empty_lines: &[&str]) -> Option<usize> {
    const INTERRUPTED_MARKER: &str =
        "conversation interrupted - tell the model what to do differently";
    const MAX_MARKER_LINES: usize = 4;

    for start in (0..non_empty_lines.len()).rev() {
        let end_exclusive = (start + MAX_MARKER_LINES).min(non_empty_lines.len());
        let mut joined = String::new();

        for (end, line) in non_empty_lines
            .iter()
            .enumerate()
            .take(end_exclusive)
            .skip(start)
        {
            if !joined.is_empty() {
                joined.push(' ');
            }
            joined.push_str(codex_interruption_line_body(line));

            if collapse_ascii_whitespace(&joined).contains(INTERRUPTED_MARKER) {
                return Some(end);
            }
        }
    }

    None
}

fn codex_interruption_line_body(line: &str) -> &str {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix('■')
        .map(str::trim_start)
        .unwrap_or(trimmed)
}

fn collapse_ascii_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn codex_has_approval_prompt(non_empty_lines: &[&str]) -> bool {
    let text = non_empty_lines.join("\n");
    contains_approval_prompt(
        &text,
        &[
            "continue?",
            "proceed?",
            "execute?",
            "run command?",
            "enter to select",
            "esc to cancel",
        ],
    )
}

fn codex_cursor_prompt_count(non_empty_lines: &[&str]) -> usize {
    non_empty_lines
        .iter()
        .filter(|line| {
            let trimmed = line.trim();
            let Some(rest) = trimmed
                .strip_prefix("❯")
                .or_else(|| trimmed.strip_prefix("›"))
            else {
                return false;
            };
            !rest.trim_start().is_empty()
        })
        .count()
}

fn codex_line_starts_with_activity(line: &str) -> bool {
    let trimmed = codex_status_line_body(line);
    ["working", "thinking", "processing", "generating"]
        .iter()
        .any(|activity| status_line_starts_with_phrase(trimmed, activity))
}

fn codex_line_starts_with_live_interrupt_activity(line: &str) -> bool {
    let trimmed = codex_status_line_body(line);
    [
        "working",
        "thinking",
        "processing",
        "generating",
        "running command",
        "starting mcp servers",
    ]
    .iter()
    .any(|activity| status_line_starts_with_phrase(trimmed, activity))
}

fn codex_line_has_activity_spinner(line: &str) -> bool {
    let trimmed = codex_status_line_body(line);
    let Some(rest) = SPINNER_CHARS
        .iter()
        .find_map(|spinner| trimmed.strip_prefix(spinner))
    else {
        return false;
    };

    codex_line_starts_with_activity(rest)
}

fn codex_status_line_body(line: &str) -> &str {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("•")
        .map(str::trim_start)
        .unwrap_or(trimmed)
}

const CODEX_RECENT_ACTIVITY_WINDOW: usize = 10;

fn codex_has_running_signal(non_empty_lines: &[&str]) -> bool {
    for (index, line) in codex_current_block_lines(non_empty_lines).enumerate() {
        let trimmed = line.trim();

        if trimmed == "esc to interrupt" || trimmed == "ctrl+c to interrupt" {
            return true;
        }

        if codex_line_starts_with_live_interrupt_activity(trimmed)
            && (trimmed.contains("esc to interrupt") || trimmed.contains("ctrl+c to interrupt"))
        {
            return true;
        }

        if index < CODEX_RECENT_ACTIVITY_WINDOW
            && (codex_line_starts_with_activity(trimmed)
                || codex_line_has_activity_spinner(trimmed))
        {
            return true;
        }
    }

    false
}

fn codex_current_block_lines<'a>(
    non_empty_lines: &'a [&'a str],
) -> impl Iterator<Item = &'a str> + 'a {
    non_empty_lines
        .iter()
        .rev()
        .copied()
        .take_while(|line| !codex_is_completed_work_divider(line.trim()))
}

fn codex_is_completed_work_divider(line: &str) -> bool {
    line.trim_start_matches('─')
        .trim_start()
        .starts_with("worked for")
}

/// Shared with Codex (`codex_line_starts_with_activity`,
/// `codex_line_starts_with_live_interrupt_activity`) as well as the Cursor and
/// Antigravity fallbacks, so the completion-marker suppression applies to every
/// caller. The completion list is kept small and explicit to avoid swallowing
/// legitimate activity descriptions that happen to contain past-tense words.
fn status_line_starts_with_phrase(line: &str, phrase: &str) -> bool {
    let Some(rest) = line.strip_prefix(phrase) else {
        return false;
    };
    let has_valid_boundary = rest
        .chars()
        .next()
        .is_none_or(|c| c.is_whitespace() || c == '.' || c == '…' || c == ':');
    has_valid_boundary && !activity_tail_has_completion_marker(rest)
}

fn activity_tail_has_completion_marker(rest: &str) -> bool {
    let tail =
        rest.trim_start_matches(|c: char| c.is_whitespace() || c == '.' || c == '…' || c == ':');
    if tail.is_empty() {
        return false;
    }

    tail.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(5)
        .map(str::to_lowercase)
        .any(|word| COMPLETED_ACTIVITY_MARKERS.contains(&word.as_str()))
}

/// Cursor agent status is detected via hooks first, but pane parsing is still
/// needed when hooks are missing or the Cursor CLI is executing a long-running
/// turn between hook writes.
pub fn detect_cursor_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let recent: Vec<&str> = {
        let non_empty: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        non_empty.iter().rev().take(30).rev().copied().collect()
    };
    let recent_lower = recent.join("\n");

    if contains_approval_prompt(
        &recent_lower,
        &[
            "permission required",
            "approval required",
            "allow command",
            "allow this command",
            "run this command",
            "enter to approve",
            "enter to select",
            "esc to cancel",
        ],
    ) {
        return Status::Waiting;
    }

    // The interrupt hint, spinner, and verb-prefixed activity line all live on
    // or below Cursor's bottom status bar while a turn is running. Restricting
    // the check to the last follow-up prompt and the lines below it mirrors the
    // boundary already used elsewhere and keeps stale scrollback (e.g. a
    // `ctrl+c to stop` from the previous turn) from re-triggering Running.
    let active_region = cursor_active_region(&recent);
    let active_joined = active_region.join("\n");

    if active_joined.contains("ctrl+c to stop")
        || active_joined.contains("ctrl+c to interrupt")
        || active_joined.contains("esc to interrupt")
    {
        return Status::Running;
    }

    if has_spinner_activity_line(active_region) {
        return Status::Running;
    }

    if active_region
        .iter()
        .any(|line| has_live_activity_word(line))
    {
        return Status::Running;
    }

    if cursor_has_follow_up_prompt(&recent) {
        return Status::Idle;
    }

    if cursor_has_background_task(&recent_lower) {
        return Status::Running;
    }

    Status::Idle
}

fn cursor_has_background_task(text_lower: &str) -> bool {
    text_lower.contains("background task") || text_lower.contains("background tasks")
}

fn cursor_has_follow_up_prompt(lines: &[&str]) -> bool {
    cursor_last_follow_up_prompt_index(lines).is_some()
}

/// The active region is the last follow-up prompt plus the lines below it.
/// Cursor renders its live status bar (interrupt hint, spinner, verb-prefixed
/// activity) on this prompt line or just below; anything above belongs to the
/// previous turn's scrollback and must not be treated as a live signal.
fn cursor_active_region<'a>(lines: &'a [&'a str]) -> &'a [&'a str] {
    match cursor_last_follow_up_prompt_index(lines) {
        Some(index) => &lines[index..],
        None => lines,
    }
}

fn cursor_last_follow_up_prompt_index(lines: &[&str]) -> Option<usize> {
    lines
        .iter()
        .rposition(|line| cursor_is_follow_up_prompt(line))
}

fn cursor_is_follow_up_prompt(line: &str) -> bool {
    let clean_line = line.trim();
    clean_line == "→" || clean_line.starts_with("→ add a follow-up")
}

/// Copilot CLI status detection via tmux pane parsing.
///
/// Copilot CLI (v1.0.65) is a full-screen TUI rendered inside a bordered input
/// box. The bottom status line is the reliable signal:
///   - `◎ Working ... esc cancel` while the model is generating (Running).
///   - `/ commands · ? help · tab next tab` when parked at an empty prompt,
///     ready for the next message (Waiting).
///   - a numbered choice list with `enter to select` / `esc to cancel` for a
///     tool/folder-trust approval (Waiting). `--yolo` (allow-all-paths +
///     allow-all-tools) suppresses most of these.
pub fn detect_copilot_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");
    let last_lines_lower = last_lines.to_lowercase();

    // Terminal states are checked before Running. capture-pane grabs 50 lines of
    // scrollback (`-S -50`), and Copilot leaves a finished turn's `◎ Working esc
    // cancel` footer and spinner glyphs in that history. A completed turn whose
    // live footer is the approval or ready prompt must win over those stale
    // lines, otherwise the session spins forever (#2815).
    if contains_approval_prompt(
        &last_lines_lower,
        &[
            "continue?",
            "run command?",
            "allow this tool",
            "approve for the rest",
            "enter to select",
            "esc to cancel",
        ],
    ) {
        return Status::Waiting;
    }

    // Empty ready prompt: Copilot's idle footer is `/ commands · ? help · tab
    // next tab`. Require all three tokens together so ordinary prose mentioning
    // `? help` or `tab next tab` mid-turn does not falsely read as Waiting; the
    // full footer only renders at the ready prompt (Working and approval footers
    // differ). `copilot>` is kept for custom wrappers/older builds.
    if (last_lines_lower.contains("/ commands")
        && last_lines_lower.contains("? help")
        && last_lines_lower.contains("tab next tab"))
        || matches_input_prompt(&non_empty_lines, 10, &["copilot>"])
    {
        return Status::Waiting;
    }

    // Running signals only count on the live footer, the bottom few non-empty
    // lines where Copilot renders its status footer and input box. Scanning the
    // whole capture would latch onto a completed turn's `◎ Working`/spinner line
    // still sitting in scrollback and never let go (#2815).
    let footer: Vec<&str> = non_empty_lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .copied()
        .collect();
    let footer_lower = footer.join("\n");

    if has_any_spinner(&footer) {
        return Status::Running;
    }

    if footer_lower.contains("thinking")
        || footer_lower.contains("working")
        || footer_lower.contains("esc to interrupt")
        || footer_lower.contains("ctrl+c to interrupt")
        // Copilot's live footer reads `◎ Working ... esc cancel`; key on the
        // interrupt hint too so a verb change doesn't drop the Running signal.
        || footer_lower.contains("esc cancel")
    {
        return Status::Running;
    }

    Status::Idle
}

/// Pi coding agent status detection via tmux pane parsing.
/// Pi always auto-approves tool use (no approval gates), so we only detect
/// Running vs Idle/Waiting-for-input states.
pub fn detect_pi_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");
    let last_lines_lower = last_lines.to_lowercase();

    if has_any_spinner(&lines) {
        return Status::Running;
    }

    if last_lines_lower.contains("esc to interrupt")
        || last_lines_lower.contains("ctrl+c to interrupt")
    {
        return Status::Running;
    }

    // Check for input prompt before activity indicators: words like
    // "reading" or "writing" linger in scrollback after the agent finishes.
    if matches_input_prompt(&non_empty_lines, 5, &["pi>"]) {
        return Status::Waiting;
    }

    let activity_indicators = ["thinking", "working", "reading", "writing", "executing"];
    for indicator in &activity_indicators {
        if last_lines_lower.contains(indicator) {
            return Status::Running;
        }
    }

    Status::Idle
}

/// Factory Droid CLI status detection via tmux pane parsing.
/// Droid uses an interactive REPL similar to other coding agents. It shows
/// activity indicators while processing and prompts for input when idle.
pub fn detect_droid_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");
    let last_lines_lower = last_lines.to_lowercase();

    if has_any_spinner(&lines) {
        return Status::Running;
    }

    if last_lines_lower.contains("esc to interrupt")
        || last_lines_lower.contains("ctrl+c to interrupt")
        || last_lines_lower.contains("thinking")
        || last_lines_lower.contains("working")
        || last_lines_lower.contains("executing")
    {
        return Status::Running;
    }

    if contains_approval_prompt(
        &last_lines_lower,
        &[
            "continue?",
            "proceed?",
            "execute?",
            "enter to select",
            "esc to cancel",
        ],
    ) {
        return Status::Waiting;
    }

    if matches_input_prompt(&non_empty_lines, 10, &["droid>"]) {
        return Status::Waiting;
    }

    Status::Idle
}

/// Hermes (NousResearch) status detection via tmux pane parsing.
/// Used as a fallback when the YAML hook system hasn't written a status file yet.
/// Detects spinner faces (◜ ◠ ✧), tool execution prefix (┊), thinking verbs,
/// dangerous-command approval prompt, and input prompt (❯ / ⚡).
pub fn detect_hermes_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");

    // Hermes spinner faces animate during LLM calls; only present while active
    // (unicode, unaffected by to_lowercase).
    const HERMES_SPINNERS: &[&str] = &["◜", "◠", "✧"];
    if lines
        .iter()
        .any(|line| HERMES_SPINNERS.iter().any(|s| line.contains(s)))
    {
        return Status::Running;
    }

    // While running, Hermes replaces the input prompt with
    // "❯ Ctrl+C to interrupt…". Check this before the idle-prompt
    // detection below so we don't misidentify Running as Waiting.
    if non_empty_lines
        .iter()
        .rev()
        .take(5)
        .any(|l| l.contains("ctrl+c to interrupt"))
    {
        return Status::Running;
    }

    // Input prompt ❯ (default skin) or ⚡ (cyberpunk skin) on its own means
    // the agent finished its turn and is ready for the next message — Idle,
    // not Waiting (which in AoE means "needs user approval for a dangerous
    // command"). Placed before scrollback activity words to avoid false-positive
    // Running from a previous turn.
    for line in non_empty_lines.iter().rev().take(5) {
        let clean = strip_ansi(line).trim().to_string();
        if clean == "❯" || clean.starts_with("❯ ") || clean == "⚡" || clean.starts_with("⚡ ")
        {
            return Status::Idle;
        }
    }

    // Active streaming lines are prefixed with ┊; check recent lines only
    // to avoid triggering on scrollback from a completed turn.
    if non_empty_lines
        .iter()
        .rev()
        .take(10)
        .any(|l| l.contains("┊"))
    {
        return Status::Running;
    }

    // Thinking verbs from the default skin and community Hermes skins.
    let activity_indicators = [
        "reasoning",
        "pondering",
        "contemplating",
        "forging",
        "plotting",
        "jacking in",
        "decrypting",
        "uploading",
        "processing",
        "analyzing",
        "computing",
        "evaluating",
    ];
    for indicator in &activity_indicators {
        if last_lines.contains(indicator) {
            return Status::Running;
        }
    }

    // Dangerous-command approval prompt.
    if contains_approval_prompt(
        &last_lines,
        &["choice [o/s/a/d]:", "[o]nce", "dangerous command"],
    ) {
        return Status::Waiting;
    }

    Status::Idle
}

/// Kiro CLI status is detected via hooks (JSON-based), not tmux pane parsing.
/// This stub exists so the agent registry has a valid function pointer.
pub fn detect_kiro_status(_content: &str) -> Status {
    Status::Idle
}

/// settl status is detected via hooks (TOML-based), not tmux pane parsing.
/// This stub exists so the agent registry has a valid function pointer.
pub fn detect_settl_status(_content: &str) -> Status {
    Status::Idle
}

/// Kimi Code status is detected via hooks (`[[hooks]]` in config.toml), not
/// tmux pane parsing. This stub exists so the agent registry has a valid
/// function pointer.
pub fn detect_kimi_status(_content: &str) -> Status {
    Status::Idle
}

pub fn detect_gemini_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");
    let last_lines_lower = last_lines.to_lowercase();

    if last_lines_lower.contains("esc to interrupt")
        || last_lines_lower.contains("ctrl+c to interrupt")
    {
        return Status::Running;
    }

    if has_any_spinner(&lines) {
        return Status::Running;
    }

    if contains_approval_prompt(
        &last_lines_lower,
        &["execute?", "enter to select", "esc to cancel"],
    ) {
        return Status::Waiting;
    }

    // Gemini's input prompt is a bare `>` with nothing after it, so we don't
    // share matches_input_prompt (which also fires on `> something` lines).
    for line in non_empty_lines.iter().rev().take(10) {
        let clean_line = strip_ansi(line).trim().to_string();
        if clean_line == ">" {
            return Status::Waiting;
        }
    }

    Status::Idle
}

/// Qwen Code status detection via tmux pane parsing.
/// Qwen Code is a fork of Gemini CLI, so the running/waiting markers mirror
/// Gemini's: braille spinner + "esc to interrupt" while working, approval
/// prompts and a numbered `❯` selection menu while waiting.
pub fn detect_qwen_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines_lower: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");

    if last_lines_lower.contains("esc to interrupt")
        || last_lines_lower.contains("ctrl+c to interrupt")
    {
        return Status::Running;
    }

    if has_any_spinner(&lines) {
        return Status::Running;
    }

    if contains_approval_prompt(
        &last_lines_lower,
        &[
            "execute?",
            "run command?",
            "enter to select",
            "esc to cancel",
        ],
    ) {
        return Status::Waiting;
    }

    // Numbered selection menu cursor. Qwen renders `›` (U+203A) by default but
    // also `❯` (U+276F) in some themes; the shared helpers don't cover either.
    for line in &lines {
        let trimmed = line.trim();
        let after_cursor = trimmed
            .strip_prefix("›")
            .or_else(|| trimmed.strip_prefix("❯"));
        if let Some(rest) = after_cursor {
            let rest = rest.trim_start();
            if rest.starts_with("1.") || rest.starts_with("2.") || rest.starts_with("3.") {
                return Status::Waiting;
            }
        }
    }

    if matches_input_prompt(&non_empty_lines, 10, &["qwen>"]) {
        return Status::Waiting;
    }

    Status::Idle
}

pub fn detect_antigravity_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    let last_lines_lower: String = non_empty_lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");

    if last_lines_lower.contains("not signed in")
        || last_lines_lower.contains("signing in")
        || last_lines_lower.contains("authorization url")
        || last_lines_lower.contains("authorization code")
        || last_lines_lower.contains("google sign-in")
    {
        return Status::Waiting;
    }

    // "Approval Required" is the actual header Antigravity renders above tool
    // permission prompts. The substring "approve" does NOT appear in
    // "approval", so the base contains_approval_prompt list misses it; match
    // explicitly. "deny access" is the rejection button rendered alongside.
    // "awaiting user approval" is the status line shown while the agent is
    // blocked on the user's decision.
    if last_lines_lower.contains("approval required")
        || last_lines_lower.contains("awaiting user approval")
        || last_lines_lower.contains("deny access")
    {
        return Status::Waiting;
    }

    if contains_approval_prompt(
        &last_lines_lower,
        &[
            "permission request",
            "do you trust the contents",
            "yes, i trust this folder",
            "execute?",
            "run command?",
            "enter to select",
            "enter confirm",
            "esc to cancel",
        ],
    ) {
        return Status::Waiting;
    }

    if last_lines_lower.contains("esc to interrupt")
        || last_lines_lower.contains("ctrl+c to interrupt")
        || last_lines_lower.contains("ctrl+c to stop")
    {
        return Status::Running;
    }

    if has_any_spinner(&lines) {
        return Status::Running;
    }

    if non_empty_lines
        .iter()
        .rev()
        .take(10)
        .any(|line| has_live_activity_word(line))
    {
        return Status::Running;
    }

    Status::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_cursor_status_running_on_live_activity() {
        let content = "\
  Grepped \"legacy_engine\" in .

 ⠘⠣ Reading  6.66k tokens

  → Add a follow-up                                      ctrl+c to stop

  Composer 2.5 · 48.2%                                  Auto-run";
        assert_eq!(detect_cursor_status(content), Status::Running);
    }

    #[test]
    fn test_detect_cursor_status_running_on_calling_spinner() {
        let content = "\
 ⠀⠞ Calling  23.62k tokens


  → Add a follow-up  ctrl+c to stop


  Composer 2.5 · 55.7% · 49 files edited  Auto-run
";
        assert_eq!(detect_cursor_status(content), Status::Running);
    }

    #[test]
    fn test_detect_cursor_status_idle_on_background_task_after_follow_up_prompt() {
        let content = "\
  → Add a follow-up


  1 background task
  Composer 2.5 · 39.2% · 20 files edited  Auto-run
";
        assert_eq!(detect_cursor_status(content), Status::Idle);
    }

    #[test]
    fn test_detect_cursor_status_running_on_background_task_without_prompt() {
        let content = "\
  Started processing the request.

  1 background task
  Composer 2.5 · 39.2% · 20 files edited  Auto-run
";
        assert_eq!(detect_cursor_status(content), Status::Running);
    }

    #[test]
    fn test_detect_cursor_status_running_on_editing_spinner() {
        let content = "\
  ┌──────────────────────────────┐
  │ Editing src/app/submit/page.tsx
  └──────────────────────────────┘

 ⠘⠆ Editing  39.76k tokens";
        assert_eq!(detect_cursor_status(content), Status::Running);
    }

    #[test]
    fn test_detect_cursor_status_waiting_for_permission_prompt() {
        let content = "\
Run this command?

> Allow this command
  Deny

enter to select · esc to cancel";
        assert_eq!(detect_cursor_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_cursor_status_idle_on_completed_output() {
        let content = "\
  Finished the requested changes.

  → Add a follow-up

  Composer 2.5 · 60.9% · 4 files edited                 Auto-run";
        assert_eq!(detect_cursor_status(content), Status::Idle);
    }

    #[test]
    fn test_detect_cursor_status_idle_on_completed_activity_phrases() {
        for content in [
            "Running tests completed successfully.\n\n→ Add a follow-up",
            "Reading config.toml finished.\n\n→ Add a follow-up",
            "Editing src/app.rs done.\n\n→ Add a follow-up",
            "Testing finished with success.\n\n→ Add a follow-up",
        ] {
            assert_eq!(detect_cursor_status(content), Status::Idle);
        }
    }

    #[test]
    fn test_detect_cursor_status_idle_on_completed_activity_without_prompt() {
        // Exercises activity_tail_has_completion_marker directly: no follow-up
        // prompt line is present, so the result depends on the verb-prefixed
        // line being suppressed because of the completion marker that follows.
        for content in [
            "Running tests completed successfully.\n  Composer 2.5",
            "Reading config.toml finished.\n  Composer 2.5",
            "Editing src/app.rs done.\n  Composer 2.5",
            "Testing finished with success.\n  Composer 2.5",
        ] {
            assert_eq!(detect_cursor_status(content), Status::Idle);
        }
    }

    #[test]
    fn test_detect_cursor_status_idle_on_stale_spinner_before_follow_up_prompt() {
        let content = "\
 ⠘⠆ Editing  39.76k tokens

  Updated src/app/submit/page.tsx

  → Add a follow-up

  Composer 2.5 · 56.1% · 26 files edited  Auto-run";
        assert_eq!(detect_cursor_status(content), Status::Idle);
    }

    #[test]
    fn test_detect_claude_status_idle_on_plain_text() {
        // No spinner, no interrupt hint, no token counter: Idle.
        assert_eq!(detect_claude_status(""), Status::Idle);
        assert_eq!(detect_claude_status("Some output\n> "), Status::Idle);
        assert_eq!(
            detect_claude_status("file saved successfully"),
            Status::Idle
        );
    }

    #[test]
    fn test_detect_claude_status_running_on_interrupt_hint() {
        // The most reliable signal: Claude prints an interrupt hint while
        // a turn is generating.
        assert_eq!(
            detect_claude_status("✶ Working…\n  esc to interrupt"),
            Status::Running
        );
        assert_eq!(
            detect_claude_status("Generating...\nctrl+c to interrupt"),
            Status::Running
        );
    }

    #[test]
    fn test_detect_claude_status_running_on_live_token_counter() {
        // The (Xs · ↓ N tokens) counter only renders during generation.
        assert_eq!(
            detect_claude_status("✶ Working… (4s · ↓ 88 tokens)"),
            Status::Running
        );
        assert_eq!(
            detect_claude_status("● Cooking… (12s · ↓ 1234 tokens)"),
            Status::Running
        );
    }

    #[test]
    fn test_detect_claude_status_running_on_spinner_verb_shape() {
        // <frame> <Verb…> is the live spinner line.
        assert_eq!(detect_claude_status("✶ Working…"), Status::Running);
        assert_eq!(detect_claude_status("✻ Herding…"), Status::Running);
        assert_eq!(detect_claude_status("● Pondering…"), Status::Running);
        assert_eq!(detect_claude_status("· Sautéing…"), Status::Running);
        // Reduced-motion mode renders a static ●.
        assert_eq!(detect_claude_status("● Working…"), Status::Running);
    }

    #[test]
    fn test_detect_claude_status_idle_on_past_tense_completion() {
        // Same frame char, but "Worked for 1m 52s" means the turn is done.
        assert_eq!(detect_claude_status("✻ Worked for 1m 52s"), Status::Idle);
        assert_eq!(detect_claude_status("● Cooked for 30s"), Status::Idle);
        assert_eq!(detect_claude_status("· Brewed for 2m 10s"), Status::Idle);
    }

    #[test]
    fn test_detect_claude_status_ignores_lowercase_after_frame() {
        // "* foo…" (e.g. a markdown bullet that happens to end with an
        // ellipsis) should not be mistaken for an active spinner. Active
        // verbs are always capitalized.
        assert_eq!(detect_claude_status("* foo…"), Status::Idle);
    }

    #[test]
    fn test_detect_claude_status_ignores_markdown_bullet_with_trailing_ellipsis() {
        // Rendered markdown bullets can start with a frame char and a
        // capitalized word and end with a trailing `…`. The live spinner
        // line always has the ellipsis inside the first word
        // (`Cooking…`), not several words later, so we don't flag this
        // as Running.
        assert_eq!(
            detect_claude_status("* Cooked an amazing dish today…"),
            Status::Idle
        );
        assert_eq!(
            detect_claude_status("· Some random response text ending with…"),
            Status::Idle
        );
    }

    #[test]
    fn test_detect_claude_status_finds_signal_above_blank_padding() {
        // Real `tmux capture-pane -S -50` typically returns 50 lines even
        // when the agent has only painted 2-3 lines at the top, with the
        // rest blank. The detector must skip blank lines, not just look at
        // the literal last N lines, or it'll miss every signal.
        let mut content = String::from("✶ Working… (4s · ↓ 88 tokens)\n  esc to interrupt\n");
        for _ in 0..40 {
            content.push('\n');
        }
        assert_eq!(detect_claude_status(&content), Status::Running);
    }

    #[test]
    fn test_detect_claude_status_waiting_on_bash_permission_prompt() {
        // Regression for #1913: a sandboxed Claude session reaches the
        // pane fallback (the host can't read the in-container hook status),
        // and Claude keeps its live spinner line rendered *below* the
        // approval prompt while it waits. The prompt must outrank the
        // spinner or the session reports Running (green) the whole time
        // it is blocked on the user.
        let content = "\
  Bash command

    SANDBOX=aoe-sandbox-ee1a86c7
    echo \"checking sandbox gitconfig\"

  Do you want to proceed?
  ❯ 1. Yes
    2. No

  Esc to cancel · Tab to amend

✶ Herding… (53s · ↓ 7.0k tokens)
  Tip: Use /bts to ask a quick side question without interrupting Claude's current work";
        assert_eq!(detect_claude_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_claude_status_waiting_on_edit_permission_prompt() {
        let content = "\
  Do you want to make this edit to src/main.rs?
  ❯ 1. Yes
    2. Yes, allow all edits during this session (shift+tab)
    3. No, and tell Claude what to do differently (esc)

✶ Cooking… (8s · ↓ 412 tokens)";
        assert_eq!(detect_claude_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_claude_status_waiting_on_plan_exit_prompt() {
        let content = "\
  Would you like to proceed?
  ❯ 1. Yes, and auto-accept edits
    2. Yes, and manually approve edits
    3. No, keep planning";
        assert_eq!(detect_claude_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_claude_status_running_not_confused_by_numbered_prose() {
        // A numbered list in assistant prose must not be mistaken for an
        // approval prompt: without a "do you want to" / "would you like to
        // proceed" question, the live spinner still wins.
        let content = "\
  Here is the plan:
  1. Read the config
  2. Patch the parser

✶ Working… (4s · ↓ 88 tokens)
  esc to interrupt";
        assert_eq!(detect_claude_status(content), Status::Running);
    }

    #[test]
    fn test_reconcile_claude_hook_status_waiting_on_approval_prompt() {
        // The hook reports Running (PreToolUse fired) but the pane is parked
        // on a permission prompt with the spinner still alive below it. The
        // reconciler must downgrade to Waiting. ANSI is preserved here to
        // exercise the strip path the live capture goes through. See #1913.
        let pane = "\x1b[1m  Do you want to proceed?\x1b[0m\n\
  ❯ 1. Yes\n    2. No\n\n  Esc to cancel · Tab to amend\n\
\x1b[38;5;174m✶\x1b[0m Herding… (53s · ↓ 7.0k tokens)";
        assert_eq!(
            reconcile_claude_hook_status(Status::Running, pane, None),
            Status::Waiting
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_keeps_running_without_prompt() {
        let pane = "✶ Working… (4s · ↓ 88 tokens)\n  esc to interrupt";
        assert_eq!(
            reconcile_claude_hook_status(Status::Running, pane, None),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_passes_non_running_through() {
        // A Notification(permission_prompt) hook that does fire writes
        // Waiting directly; the reconciler must not second-guess it, and an
        // Idle/Waiting hook is trusted as-is even with no pane evidence.
        assert_eq!(
            reconcile_claude_hook_status(Status::Waiting, "", None),
            Status::Waiting
        );
        assert_eq!(
            reconcile_claude_hook_status(Status::Idle, "Do you want to proceed?\n1. Yes", None),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_idle_on_esc_interrupt() {
        // The user cancelled a turn with Esc. Claude fires neither Stop nor an
        // idle_prompt notification, so the hook stream is stuck on its last
        // `running` write. The pane shows the interrupt banner and the idle
        // footer with no active-turn signal, so the reconciler must fall to
        // Idle. ANSI is preserved to exercise the strip path the live capture
        // goes through.
        let pane = "\x1b[2m  ⎿  Interrupted · What should Claude do instead?\x1b[0m\n\n\
\x1b[1m❯ \x1b[0m\n\n  ? for shortcuts · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(Status::Running, pane, None),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_keeps_running_when_new_turn_follows_interrupt() {
        // The interrupt banner lingers in scrollback, but the user has already
        // started another turn (spinner + interrupt hint now showing). The
        // active-turn signal must win so we don't flicker Idle mid-turn.
        let pane = "  ⎿  Interrupted · What should Claude do instead?\n\
● Picking up where we left off\n\
✶ Herding… (3s · ↓ 42 tokens)\n  esc to interrupt";
        assert_eq!(
            reconcile_claude_hook_status(Status::Running, pane, None),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_trusts_fresh_running_at_idle_prompt() {
        // No interrupt banner and no active-turn signal yet: the gap right
        // after UserPromptSubmit before the spinner renders. The `running`
        // write is fresh (well under the stale threshold), so we trust the
        // hook's Running rather than flickering Idle on the idle-looking pane.
        let pane = "❯ \n\n  ? for shortcuts · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(1))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_idle_on_stale_running_at_idle_prompt() {
        // The "silent tool stop": a tool result with no following text parked
        // Claude at the idle prompt firing neither Stop nor idle_prompt, so the
        // file is stuck on `running`. The pane shows the idle ready prompt with
        // no active-turn signal and the write has been standing well past the
        // threshold, so the reconciler recovers to Idle.
        let pane = "\x1b[1m❯ \x1b[0m\n\n  ? for shortcuts · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_keeps_running_on_background_agent_wait() {
        // Captured from Claude Code 2.1.211: the main REPL parked at the input
        // box while a background agent works. The wait line has no ellipsis
        // and the agents-strip token counter is k-suffixed, so neither older
        // running-signal check matched; the pane must still read as working
        // even with the `running` write standing far past the age gate
        // (background tool gaps routinely exceed it). See #2909 regression.
        let pane = "\
● Agent(Summarize tmux module pub fns)\n\
  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)\n\
● The background agent is running. I'll wait for its completion notification.\n\
✻ Waiting for 1 background agent to finish\n\
──────────────────────────────\n\
❯ \n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents · ↓ to manage\n\
  ● main\n\
  ◯ general-purpose  Summarize tmux module pub fns    19s · ↓ 36.4k tokens";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(300))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_idle_after_background_agent_finished() {
        // Same session after the agent completed and the turn ended: the
        // agents strip stays on screen frozen at its final counters
        // (`1m 14s · ↓ 40.4k tokens`) and the status slot shows the past-tense
        // completion line. A stale `running` write must still downgrade to
        // Idle; the frozen strip must not count as a live token counter.
        let pane = "\
  The agent flagged two things worth noting about the module surface.\n\
✻ Churned for 1m 40s\n\
──────────────────────────────\n\
❯ \n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents · ↓ to manage\n\
  ● main\n\
  ◯ general-purpose  Summarize tmux module pub fns    1m 14s · ↓ 40.4k tokens";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_ignores_prose_background_wait_mention() {
        // Assistant prose is prefixed with `●` (a spinner frame char), so a
        // response line mentioning a background-agent wait must not read as
        // the wait status line; that would pin an idle session on Running
        // with no recovery path. The structural match (digit count + "to
        // finish" tail) rejects it.
        let pane = "\
● Waiting for background agent results before summarizing.\n\
* Waiting for 2 background agents to finish before merging\n\
❯ \n\
  ? for shortcuts · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_idle_with_frozen_integer_strip_counter() {
        // A quick background agent can finish under 1k downloaded tokens, so
        // the frozen agents strip shows a plain-integer count that would look
        // exactly like the live counter without the closing-paren
        // requirement. The parked session must still downgrade to Idle.
        let pane = "\
✻ Churned for 12s\n\
──────────────────────────────\n\
❯ \n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents · ↓ to manage\n\
  ● main\n\
  ◯ general-purpose  Quick lookup    19s · ↓ 728 tokens";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_age_gate_boundary() {
        // The gate is inclusive: at the threshold the ready-prompt pane
        // downgrades, one second under it keeps Running. Derived from the
        // constant so a future retune keeps the boundary semantics tested.
        let pane = "❯ \n\n  ? for shortcuts · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(IDLE_RECONCILE_MIN_RUNNING_AGE)
            ),
            Status::Idle
        );
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(IDLE_RECONCILE_MIN_RUNNING_AGE - std::time::Duration::from_secs(1))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_detect_claude_status_background_agent_panes() {
        // The hookless fallback path (sandboxed sessions, custom --cmd
        // wrappers) shares claude_pane_has_running_signal: the wait pane is
        // Running, the finished pane with the frozen strip is Idle.
        let waiting = "\
✻ Waiting for 1 background agent to finish\n\
❯ \n\
  ◯ general-purpose  Summarize tmux module pub fns    19s · ↓ 36.4k tokens";
        assert_eq!(detect_claude_status(waiting), Status::Running);

        let finished = "\
✻ Churned for 1m 40s\n\
❯ \n\
  ◯ general-purpose  Summarize tmux module pub fns    1m 14s · ↓ 40.4k tokens";
        assert_eq!(detect_claude_status(finished), Status::Idle);
    }

    #[test]
    fn test_claude_line_is_background_wait_variants() {
        assert!(claude_line_is_background_wait(
            "✻ Waiting for 1 background agent to finish"
        ));
        assert!(claude_line_is_background_wait(
            "✶ Waiting for 2 background agents to finish"
        ));
        assert!(claude_line_is_background_wait(
            "  · Waiting for 12 background agents to finish"
        ));
        // No spinner frame char.
        assert!(!claude_line_is_background_wait(
            "Waiting for 1 background agent to finish"
        ));
        // Prose: no digit count.
        assert!(!claude_line_is_background_wait(
            "● Waiting for background agent results"
        ));
        // Prose: trailing words after "to finish" break the exact tail.
        assert!(!claude_line_is_background_wait(
            "* Waiting for 2 background agents to finish before merging"
        ));
        assert!(!claude_line_is_background_wait(""));
    }

    #[test]
    fn test_reconcile_claude_hook_status_idle_in_bypass_mode_with_ghost_text() {
        // Captured from Claude Code 2.1.211 in bypass-permissions mode after a
        // finished turn: ghost suggestion text occupies the `❯` line (so the
        // bare-prompt marker misses) and the bypass footer has no
        // `? for shortcuts`. The mode-cycle footer is the parked marker; a
        // stale `running` write must still recover to Idle.
        let pane = "\
✻ Churned for 1m 40s\n\
──────────────────────────────\n\
❯ Explain how the vt.rs VtChannel is shared across viewers\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_running_in_bypass_mode_while_active() {
        // The running variant of the same footer appends `esc to interrupt`,
        // so an active bypass-mode turn must not read as parked even though
        // the mode-cycle footer marker is present and the write is stale.
        let pane = "\
✽ Crunching… (19s · ↓ 166 tokens)\n\
  ⎿  Tip: Use /memory to view and manage Claude memory\n\
──────────────────────────────\n\
❯ \n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_waiting_outranks_mode_cycle_footer() {
        // An approval prompt pane can also carry the mode-cycle footer. The
        // Waiting downgrade must win over the ready-prompt downgrade even
        // with a stale `running` write, so a blocked question is never
        // reported as Idle.
        let pane = "\
Do you want to proceed?\n\
❯ 1. Yes\n\
  2. No\n\
──────────────────────────────\n\
  ⏸ plan mode on (shift+tab to cycle) · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Waiting
        );
    }

    #[test]
    fn test_claude_ready_prompt_footer_variants() {
        // Parked footers captured from 2.1.211 by cycling shift+tab, each
        // with ghost suggestion text defeating the bare-prompt marker. All
        // four mode-cycle variants must read as parked; an echoed footer
        // (diff/tool output, so the line doesn't start with the footer
        // glyph) and the running footer variant must not.
        for footer in [
            "  ⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
            "  ⏸ plan mode on (shift+tab to cycle) · ← for agents",
            "  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
            "  ⏸ manual mode on · ? for shortcuts · ← for agents",
        ] {
            let pane = format!("✻ Churned for 10s\n❯ ghost suggestion text\n{footer}");
            assert!(
                claude_pane_shows_ready_prompt(&pane),
                "expected parked for footer: {footer}"
            );
        }
        let echoed = "\
+  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n\
❯ ghost suggestion text";
        assert!(!claude_pane_shows_ready_prompt(echoed));
        let running = "\
❯ ghost suggestion text\n\
  ⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt · ← for agents";
        assert!(!claude_pane_shows_ready_prompt(running));
    }

    #[test]
    fn test_reconcile_claude_hook_status_running_during_compaction() {
        // Compaction renders its ellipsis on the second word
        // (`✢ Compacting conversation… (17s)`, captured from 2.1.211) and
        // fires no hooks, so the `running` write goes stale while it runs.
        // The spinner match must keep the session Running even when the
        // wrapped footer splits the `esc to interrupt` hint across lines.
        let pane = "\
✢ Compacting conversation… (17s)\n\
❯ \n\
  ⏵⏵ auto mode on (shift+tab to cycle) · esc\n\
  to interrupt · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_running_with_wrapped_interrupt_hint() {
        // A narrow pane word-wraps the footer; a break inside the interrupt
        // hint must not hide the running signal while the mode-cycle marker
        // survives intact on its fragment (that combination flipped an
        // active turn to Idle before the whitespace-collapsed hint check).
        let pane = "\
❯ \n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc\n\
  to interrupt · ← for agents";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_stale_running_keeps_running_while_active() {
        // A long tool run can leave the `running` write stale (mtime old)
        // while the turn is genuinely active. The live active-turn signal must
        // still win over the age gate; only an idle-looking pane downgrades.
        let pane = "✶ Working… (90s · ↓ 4.1k tokens)\n  esc to interrupt";
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                pane,
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_stale_running_keeps_running_on_blank_pane() {
        // Stale write but no positive idle marker (a blank / mid-redraw
        // capture). Absence of a spinner is not enough; without the ready
        // prompt we trust the hook rather than flicker Idle.
        assert_eq!(
            reconcile_claude_hook_status(
                Status::Running,
                "   \n\n  ",
                Some(std::time::Duration::from_secs(120))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_detect_claude_status_handles_v2_1_118_per_word_ansi() {
        // Regression for #890: Claude Code v2.1.118 wraps each word in ANSI
        // color escapes. After the dispatcher strips ANSI we should still
        // see the spinner+verb shape and the interrupt hint.
        let ansi_running = "\x1b[38;5;174m✶\x1b[39m \x1b[38;5;180mWorking…\x1b[38;5;174m \x1b[38;5;246m(4s · ↓\x1b[39m \x1b[38;5;246m88 tokens)\x1b[39m\n\x1b[39m  \x1b[38;5;246mesc\x1b[39m \x1b[38;5;246mto\x1b[39m \x1b[38;5;246minterrupt\x1b[39m";
        assert_eq!(
            detect_status_from_content(ansi_running, "claude"),
            Status::Running,
            "Per-word ANSI coloring must not prevent Running detection for Claude Code"
        );
    }

    #[test]
    fn test_detect_status_from_content_unknown_tool_returns_idle() {
        let status = detect_status_from_content("Processing ⠋", "unknown_tool");
        assert_eq!(status, Status::Idle);
    }

    #[test]
    fn test_detect_status_strips_ansi_before_matching() {
        // capture-pane -e injects ANSI color codes between characters, which
        // can split signal strings like "esc interrupt" so they no longer match
        // as plain substrings. The dispatcher must strip ANSI before calling
        // any agent detector.
        let ansi_running =
            "\x1b[38;2;39;62;94m⬝⬝⬝⬝⬝⬝⬝⬝\x1b[0m  \x1b[38;2;238;238;238mesc \x1b[38;2;128;128;128minterrupt\x1b[0m";
        assert_eq!(
            detect_status_from_content(ansi_running, "opencode"),
            Status::Running,
            "ANSI codes around 'esc interrupt' should not prevent Running detection"
        );

        let ansi_spinner = "\x1b[38;2;255;255;255m⠋\x1b[0m generating";
        assert_eq!(
            detect_status_from_content(ansi_spinner, "opencode"),
            Status::Running,
            "ANSI codes around spinner chars should not prevent Running detection"
        );
    }

    #[test]
    fn test_detect_opencode_status_running() {
        assert_eq!(
            detect_opencode_status("Processing your request\nesc to interrupt"),
            Status::Running
        );
        assert_eq!(
            detect_opencode_status("Working... esc interrupt"),
            Status::Running
        );
        assert_eq!(detect_opencode_status("Generating ⠋"), Status::Running);
        assert_eq!(detect_opencode_status("Loading ⠹"), Status::Running);
    }

    #[test]
    fn test_detect_opencode_status_waiting() {
        assert_eq!(
            detect_opencode_status("allow this action? [y/n]"),
            Status::Waiting
        );
        assert_eq!(detect_opencode_status("continue? (y/n)"), Status::Waiting);
        assert_eq!(detect_opencode_status("approve changes"), Status::Waiting);
        assert_eq!(detect_opencode_status("task complete.\n>"), Status::Waiting);
        assert_eq!(
            detect_opencode_status("ready for input\n> "),
            Status::Waiting
        );
        assert_eq!(
            detect_opencode_status("done! what else can i help with?\n>"),
            Status::Waiting
        );
    }

    #[test]
    fn test_detect_opencode_status_idle() {
        assert_eq!(detect_opencode_status("some random output"), Status::Idle);
        assert_eq!(
            detect_opencode_status("file saved successfully"),
            Status::Idle
        );
    }

    #[test]
    fn test_detect_opencode_status_numbered_selection() {
        let content = "Select:\n❯ 1. Option A\n  2. Option B";
        assert_eq!(detect_opencode_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_opencode_status_completion_with_prompt() {
        let content = "Task complete! What else can I help with?\n>";
        assert_eq!(detect_opencode_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_opencode_status_double_prompt() {
        assert_eq!(detect_opencode_status("Ready\n>>"), Status::Waiting);
    }

    #[test]
    fn test_detect_vibe_status_running() {
        // Braille spinners
        assert_eq!(detect_vibe_status("processing ⠋"), Status::Running);
        assert_eq!(detect_vibe_status("⠹"), Status::Running);

        // Activity indicators
        assert_eq!(detect_vibe_status("Running bash"), Status::Running);
        assert_eq!(detect_vibe_status("Reading file"), Status::Running);
        assert_eq!(detect_vibe_status("Writing changes"), Status::Running);
        assert_eq!(detect_vibe_status("Generating code"), Status::Running);

        // Vertical text (Vibe's Textual TUI renders one char per line)
        assert_eq!(
            detect_vibe_status("⠋\nR\nu\nn\nn\ni\nn\ng\nb\na\ns\nh\n…"),
            Status::Running
        );

        // Ellipsis indicates ongoing activity
        assert_eq!(detect_vibe_status("Working…"), Status::Running);
        assert_eq!(detect_vibe_status("Loading..."), Status::Running);
    }

    #[test]
    fn test_detect_vibe_status_waiting() {
        // Vibe's approval prompt navigation hints
        assert_eq!(
            detect_vibe_status("↑↓ navigate  Enter select  ESC reject"),
            Status::Waiting
        );
        // Tool approval warning
        assert_eq!(
            detect_vibe_status("⚠ bash command\nExecute this?"),
            Status::Waiting
        );
        // Approval options
        assert_eq!(
            detect_vibe_status(
                "› Yes\n  Yes and always allow bash for this session\n  No and tell the agent"
            ),
            Status::Waiting
        );
    }

    #[test]
    fn test_detect_vibe_status_idle() {
        assert_eq!(detect_vibe_status("some random output"), Status::Idle);
        assert_eq!(detect_vibe_status("file saved successfully"), Status::Idle);
        assert_eq!(detect_vibe_status("Done!"), Status::Idle);
    }

    #[test]
    fn test_detect_codex_status_running() {
        assert_eq!(
            detect_codex_status("processing request\nesc to interrupt"),
            Status::Running
        );
        assert_eq!(
            detect_codex_status("thinking about your request"),
            Status::Running
        );
        assert_eq!(detect_codex_status("working on task"), Status::Running);
        assert_eq!(detect_codex_status("generating ⠋"), Status::Running);
        assert_eq!(
            detect_codex_status("⠋ thinking about your request"),
            Status::Running
        );
        assert_eq!(
            detect_codex_status("• Working (4s • esc to interrupt)"),
            Status::Running
        );
    }

    #[test]
    fn test_detect_codex_status_waiting() {
        assert_eq!(
            detect_codex_status("run this command? (y/n)"),
            Status::Waiting
        );
        assert_eq!(detect_codex_status("approve changes?"), Status::Waiting);
        assert_eq!(
            detect_codex_status("execute this action? [y/n]"),
            Status::Waiting
        );
    }

    #[test]
    fn test_detect_codex_status_idle() {
        assert_eq!(detect_codex_status("file saved"), Status::Idle);
        assert_eq!(detect_codex_status("random output text"), Status::Idle);
        assert_eq!(
            detect_codex_status("based on your working example, aliases are safest"),
            Status::Idle
        );
        assert_eq!(
            detect_codex_status("braille spinner characters like ⠋, ⠙, etc."),
            Status::Idle
        );
        assert_eq!(
            detect_codex_status("• I found the shared API base and the routing map"),
            Status::Idle
        );
        assert_eq!(
            detect_codex_status("• Starting MCP servers can take a while"),
            Status::Idle
        );
        assert_eq!(
            detect_codex_status("• Running command examples can be misleading"),
            Status::Idle
        );
        assert_eq!(detect_codex_status("ready\ncodex>"), Status::Idle);
        assert_eq!(detect_codex_status("done\n>"), Status::Idle);
        assert_eq!(
            detect_codex_status("› Find and fix a bug in @filename"),
            Status::Idle
        );
        assert_eq!(
            detect_codex_status("› Run /review on my current changes"),
            Status::Idle
        );
    }

    #[test]
    fn test_detect_codex_status_idle_for_normal_prompt_tails() {
        let lithuanians = r#"
• Fixed and staged src/tui/home/render.rs:695. The margin span now uses Span::raw(" "), avoiding clippy::repeat_once.

  Verification passed: cargo clippy --lib -- -D warnings.


› Find and fix a bug in @filename

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#;

        let persians = r#"
• You picked: Banana.


› Run /review on my current changes

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#;

        assert_eq!(detect_codex_status(lithuanians), Status::Idle);
        assert_eq!(detect_codex_status(persians), Status::Idle);
    }

    #[test]
    fn test_detect_codex_status_idle_after_interruption() {
        let pane = r#"
  If your API supports an array/operator filter like value_in, then this could be shorter,
  but based on your working example, aliases are the safest GraphQL-native way to query all of them in one request.


› asdasd


■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.


› dasdasd

  gpt-5.5 medium · ~/tomatom/connector-plus-shopty/shopty
"#;

        assert_eq!(detect_codex_status(pane), Status::Idle);
    }

    #[test]
    fn test_detect_codex_status_waiting_after_stale_interruption_before_approval() {
        let pane = r#"
■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.

› Try again

run this command? (y/n)
"#;

        assert_eq!(detect_codex_status(pane), Status::Waiting);
    }

    #[test]
    fn test_detect_codex_status_idle_after_stale_interruption_before_prompt() {
        let pane = r#"
■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.

› Try again

• No action taken.

› What next?
"#;

        assert_eq!(detect_codex_status(pane), Status::Idle);
    }

    #[test]
    fn test_detect_codex_status_idle_after_completed_turn() {
        let pane = r#"
  Note: git status still shows MM src/tmux/status_detection.rs, meaning earlier staged changes exist and this latest fix is
  unstaged on top.

• Working (4s • esc to interrupt)

─ Worked for 1m 22s ───────────────────────────────────────────────────────────────────────────────────────────────────────────


› asd


• No action taken.

  gpt-5.5 high · ~/appsSource/agent-of-empires
"#;

        assert_eq!(detect_codex_status(pane), Status::Idle);
    }

    #[test]
    fn test_detect_codex_status_idle_with_spinner_examples_in_scrollback() {
        let pane = r#"
  tmux capture-pane -p -e -S -50

  Then it strips ANSI and runs the detector for that agent.
  See src/tmux/session.rs:290 and src/tmux/
  status_detection.rs:38.

  For Codex specifically, active work is detected from:

  - esc to interrupt
  - ctrl+c to interrupt
  - recent status-like lines starting with working, thinking,
    processing, or generating
  - braille spinner characters like ⠋, ⠙, etc.

  That logic is in src/tmux/status_detection.rs:344.

  If those running signals are not present, it then checks
  waiting signals like approvals or numbered choices.
  If none match, it falls back to Idle.

  So this is not OS process-state detection like “is the
  process using CPU.” It is mostly agent UI/state detection
  from hooks or tmux pane text.

──────────────────────────────────────────────────────────────


› Run /review on my current changes

  gpt-5.5 high · ~/appsSource/agent-of-empires
"#;

        assert_eq!(detect_codex_status(pane), Status::Idle);
    }

    #[test]
    fn test_detect_codex_status_running_with_prompt_below_activity_line() {
        let pane = r#"
│ model:     gpt-5.4-mini medium   /model to change │
│ directory: ~/tomatom/connector-plus-shopty/shopty │
╰───────────────────────────────────────────────────╯

  Tip: Start a fresh idea with /new; the previous session stays in history.

Token usage: total=36,319 input=35,006 (+ 79,744 cached) output=1,313 (reasoning 234)
To continue this session, run codex resume 019e270b-5139-7752-ac61-86fe4bb5170c


› look into possible pain points in our api endpoints here


• I’m going to inspect the API modules and their shared base classes first, then trace any authentication, response, and
  routing patterns that could create recurring pain points. After that I’ll summarize the concrete risks with file references.

• Explored
  └ Search class .*ApiActions|BaseJsonApiActions|renderJsonResponse|requireAuthentication|api/|api[A-Z] in plugins

───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────

• I found the shared API base and the routing map; next I’m checking whether there are known project-specific caveats in memory
  and then I’ll inspect the base class and a few representative endpoints for consistency problems.

• Working (4s • esc to interrupt)


› Summarize recent commits

  gpt-5.4-mini medium · ~/tomatom/connector-plus-shopty/shopty
"#;

        assert_eq!(detect_codex_status(pane), Status::Running);
    }

    #[test]
    fn test_detect_codex_status_running_with_verbose_command_output() {
        let pane = r#"
› Run the tests

• Running command: cargo test (18s • esc to interrupt)
  output line 01
  output line 02
  output line 03
  output line 04
  output line 05
  output line 06
  output line 07
  output line 08
  output line 09
  output line 10
  output line 11
  output line 12
  output line 13
  output line 14
  output line 15

› Summarize recent commits

  gpt-5.5 high · ~/appsSource/agent-of-empires
"#;

        assert_eq!(detect_codex_status(pane), Status::Running);
    }

    #[test]
    fn test_detect_codex_status_running_while_starting_mcp_servers() {
        let pane = r#"
  Note: git status still shows MM src/tmux/status_detection.rs, meaning earlier staged changes exist and this latest fix is
  unstaged on top.

─ Worked for 1m 22s ───────────────────────────────────────────────────────────────────────────────────────────────────────────


› asd


• No action taken.

>> Code review started: staged changes <<

• Ran git diff --staged --stat && git diff --staged --
  └  src/tmux/status_detection.rs | 205 +++++++++++++++++++++++++++++++++++++++++--
     1 file changed, 198 insertions(+), 7 deletions(-)
    … +253 lines (ctrl + t to view transcript)

         #[test]

• Explored
  └ Read status_detection.rs
    Search ctrl+c to interrupt\|Running (\|Running command\|esc to interrupt\|Working ( in .

• Starting MCP servers (1/2): sentry (31s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close


› Run /review on my current changes

  gpt-5.5 high · ~/appsSource/agent-of-empires
"#;

        assert_eq!(detect_codex_status(pane), Status::Running);
    }

    #[test]
    fn test_detect_codex_status_running_with_verbose_mcp_startup_output() {
        let pane = r#"
› Run /review on my current changes

• Starting MCP servers (1/2): sentry (31s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close
  output line 01
  output line 02
  output line 03
  output line 04
  output line 05
  output line 06
  output line 07
  output line 08
  output line 09
  output line 10
  output line 11
  output line 12
  output line 13
  output line 14
  output line 15

› Summarize recent commits

  gpt-5.5 high · ~/appsSource/agent-of-empires
"#;

        assert_eq!(detect_codex_status(pane), Status::Running);
    }

    #[test]
    fn test_detect_codex_status_request_user_input() {
        // Regression test for codex `request_user_input` (Plan-mode radio UI).
        // The hint line contains "esc to interrupt", which previously
        // short-circuited to Running before any Waiting heuristic could fire.
        let pane = "\
  Question 1/1 (1 unanswered)
  Which fruit do you want?

  › 1. Banana (Recommended)  Choose banana.
    2. Orange                Choose orange.
    3. Apple                 Choose apple.
    4. None of the above     Optionally, add details in notes (tab).

  tab to add notes | enter to submit answer | esc to interrupt
";
        assert_eq!(detect_codex_status(pane), Status::Waiting);
    }

    #[test]
    fn test_detect_codex_status_request_user_input_radio_only() {
        // `›` (U+203A) menu cursor should also flip to Waiting on its own,
        // independent of the hint-line tokens.
        let pane = "\
  › 1. Yes
    2. No
    3. Maybe
";
        assert_eq!(detect_codex_status(pane), Status::Waiting);
    }

    #[test]
    fn test_detect_codex_status_running_after_stale_radio_prompt() {
        let pane = r#"
  Question 1/1 (1 unanswered)
  Do you want apple, banana, orange, or something else?

  › 1. Apple (Recommended)  Pick apple for the default simple choice.
    2. Banana               Pick banana for a second common option.
    3. Orange               Pick orange for a citrus option.
    4. None of the above    Optionally, add details in notes (tab).

  tab to add notes | enter to submit answer | esc to interrupt

› Apple

• Working (4s • esc to interrupt)
"#;

        assert_eq!(detect_codex_status(pane), Status::Running);
    }

    #[test]
    fn test_reconcile_codex_hook_status_waiting_for_plan_radio_input() {
        let pane = r#"
│                                                    │
│ model:     gpt-5.5 xhigh   fast   /model to change │
│ directory: ~/appsSource/agent-of-empires           │
╰────────────────────────────────────────────────────╯

  Tip: See the Codex keymap documentation for supported actions and examples.


› ask me something using codex radio button selection


• I tried to open the Codex radio selector, but request_user_input is unavailable in Default mode.

  To show actual radio buttons, switch this session to Plan mode and ask again.


› okay i switched to plan mode



  Question 1/1 (1 unanswered)
  Do you want apple, banana, orange, or something else?

  › 1. Apple (Recommended)  Pick apple for the default simple choice.
    2. Banana               Pick banana for a second common option.
    3. Orange               Pick orange for a citrus option.
    4. None of the above    Optionally, add details in notes (tab).

  tab to add notes | enter to submit answer | esc to interrupt
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Waiting
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_waiting_for_radio_only_input() {
        let pane = "\
  › 1. Yes
    2. No
    3. Maybe
";

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Waiting
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_ignores_stale_radio_prompt_before_activity() {
        let pane = r#"
  Question 1/1 (1 unanswered)
  Do you want apple, banana, orange, or something else?

  › 1. Apple (Recommended)  Pick apple for the default simple choice.
    2. Banana               Pick banana for a second common option.
    3. Orange               Pick orange for a citrus option.
    4. None of the above    Optionally, add details in notes (tab).

  tab to add notes | enter to submit answer | esc to interrupt

› Apple

• Working (4s • esc to interrupt)
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_idle_after_cancelled_radio_prompt() {
        let pane = r#"
  Question 1/1 (1 unanswered)
  Do you want apple, banana, orange, or something else?

  › 1. Apple (Recommended)  Pick apple for the default simple choice.
    2. Banana               Pick banana for a second common option.
    3. Orange               Pick orange for a citrus option.
    4. None of the above    Optionally, add details in notes (tab).

  tab to add notes | enter to submit answer | esc to interrupt


■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to
report the issue.


› Write tests for @filename

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_idle_after_wrapped_esc_interruption() {
        let pane = r#"
› something


■ Conversation interrupted - tell the model what to
do differently. Something went wrong? Hit `/feedback` to
report the issue.


› Write tests for @filename

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_idle_after_wrapped_interruption_without_glyph() {
        let pane = r#"
› something


Conversation interrupted - tell the model what to
do differently. Something went wrong? Hit `/feedback` to
report the issue.


› Write tests for @filename

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_idle_after_esc_interruption() {
        let pane = r#"
╭────────────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.130.0)                         │
│                                                    │
│ model:     gpt-5.5 xhigh   fast   /model to change │
│ directory: ~/appsSource/agent-of-empires           │
╰────────────────────────────────────────────────────╯

  Tip: Use /rename to rename your threads for easier thread resuming.


› something


■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to
report the issue.


› Write tests for @filename

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_idle_after_completed_review() {
        let pane = r#"
>> Code review started: staged changes <<

• Ran git diff --stat
  └ 1 file changed, 3 insertions(+)

• Explored
  └ Read src/main.rs

<< Code review finished >>

──────────────────────────────────────────────────────────────

• No discrete correctness issues were found in the provided command changes.

─ Worked for 7m 40s ──────────────────────────────────────────

› Implement the fix

  gpt-5.5 xhigh fast · ~/project
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_idle_after_completed_review_without_worked_divider() {
        let pane = r#"
╭────────────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.133.0)                         │
│                                                    │
│ model:     gpt-5.5 xhigh   fast   /model to change │
│ directory: ~/project                               │
╰────────────────────────────────────────────────────╯

  Tip: Use /rename to rename your threads for easier thread resuming.

>> Code review started: src/main.rs <<

<< Code review finished >>

• No discrete correctness issues were found in the provided command changes.

› Improve documentation in @filename

  gpt-5.5 xhigh fast · ~/project
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_keeps_running_after_completed_turn_with_new_activity() {
        let pane = r#"
<< Code review finished >>

─ Worked for 7m 40s ──────────────────────────────────────────

› Implement the fix

• Working (4s • esc to interrupt)
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_keeps_running_after_completed_turn_with_plain_new_output() {
        let pane = r#"
─ Worked for 7m 40s ──────────────────────────────────────────

› Implement the fix

I’ll inspect the status detection path first and then adjust the idle override.
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_keeps_running_after_completed_review_with_plain_new_output()
    {
        let pane = r#"
>> Code review started: staged changes <<

<< Code review finished >>

› Implement the review comment

I’ll inspect the status detection path first and then adjust the idle override.
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_does_not_use_generic_pane_states() {
        assert_eq!(
            reconcile_codex_hook_status(Status::Running, "run this command? (y/n)"),
            Status::Running
        );
        assert_eq!(
            reconcile_codex_hook_status(Status::Running, "› Write tests for @filename"),
            Status::Running
        );
        assert_eq!(
            reconcile_codex_hook_status(Status::Running, "file saved"),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_only_overrides_running_hooks() {
        let pane = "\
  Question 1/1 (1 unanswered)
  Pick one

  › 1. Apple
    2. Banana

  tab to add notes | enter to submit answer | esc to interrupt
";

        assert_eq!(
            reconcile_codex_hook_status(Status::Waiting, pane),
            Status::Waiting
        );
        assert_eq!(
            reconcile_codex_hook_status(Status::Idle, pane),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_ignores_stale_interruption_before_activity() {
        let pane = r#"
■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to
report the issue.

› Try again

• Working (4s • esc to interrupt)
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_ignores_stale_interruption_before_approval() {
        let pane = r#"
■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to
report the issue.

› Try again

run this command? (y/n)
"#;

        assert_eq!(
            reconcile_codex_hook_status(Status::Running, pane),
            Status::Running
        );
    }

    #[test]
    fn test_detect_gemini_status_running() {
        assert_eq!(
            detect_gemini_status("processing request\nesc to interrupt"),
            Status::Running
        );
        assert_eq!(detect_gemini_status("generating ⠋"), Status::Running);
        assert_eq!(detect_gemini_status("working ⠹"), Status::Running);
    }

    #[test]
    fn test_detect_gemini_status_waiting() {
        assert_eq!(
            detect_gemini_status("run this command? (y/n)"),
            Status::Waiting
        );
        assert_eq!(detect_gemini_status("approve changes?"), Status::Waiting);
        assert_eq!(
            detect_gemini_status("execute this action? [y/n]"),
            Status::Waiting
        );
        assert_eq!(detect_gemini_status("ready\n>"), Status::Waiting);
    }

    #[test]
    fn test_detect_gemini_status_idle() {
        assert_eq!(detect_gemini_status("file saved"), Status::Idle);
        assert_eq!(detect_gemini_status("random output text"), Status::Idle);
    }

    #[test]
    fn test_detect_copilot_status_running() {
        assert_eq!(
            detect_copilot_status("processing request\nesc to interrupt"),
            Status::Running
        );
        assert_eq!(
            detect_copilot_status("Thinking about your request"),
            Status::Running
        );
        assert_eq!(detect_copilot_status("working ⠋"), Status::Running);
        assert_eq!(detect_copilot_status("loading ⠹"), Status::Running);
        // Real v1.0.65 working footer.
        assert_eq!(
            detect_copilot_status("┃\n◎ Working esc cancel    MAI-Code-1-Flash"),
            Status::Running
        );
    }

    #[test]
    fn test_detect_copilot_status_waiting() {
        assert_eq!(detect_copilot_status("run command? (y/n)"), Status::Waiting);
        assert_eq!(
            detect_copilot_status("Allow this tool to run?"),
            Status::Waiting
        );
        assert_eq!(
            detect_copilot_status("pick an option\nenter to select"),
            Status::Waiting
        );
        assert_eq!(detect_copilot_status("done\n>"), Status::Waiting);
        assert_eq!(detect_copilot_status("done\ncopilot>"), Status::Waiting);
        // Real v1.0.65 idle/ready footer: turn done, waiting for the next message.
        assert_eq!(
            detect_copilot_status("answer text\n┃\n/ commands · ? help · tab next tab"),
            Status::Waiting
        );
    }

    #[test]
    fn test_detect_copilot_status_idle() {
        assert_eq!(detect_copilot_status("file saved"), Status::Idle);
        assert_eq!(detect_copilot_status("random output text"), Status::Idle);
        // Prose mentioning footer phrases without the full footer must not read
        // as Waiting: only the complete `/ commands · ? help · tab next tab`
        // shape (or `copilot>`) marks the turn done.
        assert_eq!(
            detect_copilot_status("need more? help is available; use tab next tab to switch"),
            Status::Idle
        );
    }

    #[test]
    fn test_detect_copilot_status_stale_working_in_scrollback() {
        // #2815: capture-pane returns 50 lines of scrollback, so a finished
        // turn's `◎ Working esc cancel` footer and a frozen spinner glyph
        // linger above the live idle footer. The turn is done; status must read
        // Waiting, not spin forever on the stale lines.
        let pane = "> summarize the readme\n\
                    ◎ Working esc cancel    MAI-Code-1-Flash\n\
                    Here is the summary. ⠋\n\
                    It covers setup and usage.\n\
                    More detail follows here.\n\
                    ┃\n\
                    / commands · ? help · tab next tab";
        assert_eq!(detect_copilot_status(pane), Status::Waiting);

        // Same stale scrollback, but the live footer is a bare ready prompt
        // (footer text drifted / no full three-token footer). Still done.
        let pane_prompt = "> summarize the readme\n\
                           ◎ Working esc cancel\n\
                           Here is the summary.\n\
                           It covers setup and usage.\n\
                           More detail follows here.\n\
                           >";
        assert_eq!(detect_copilot_status(pane_prompt), Status::Waiting);
    }

    #[test]
    fn test_detect_pi_status_running() {
        assert_eq!(detect_pi_status("generating ⠋"), Status::Running);
        assert_eq!(detect_pi_status("loading ⠹"), Status::Running);
        assert_eq!(
            detect_pi_status("processing request\nesc to interrupt"),
            Status::Running
        );
        assert_eq!(detect_pi_status("thinking about code"), Status::Running);
        assert_eq!(detect_pi_status("reading file.ts"), Status::Running);
    }

    #[test]
    fn test_detect_pi_status_waiting() {
        assert_eq!(detect_pi_status("done\n>"), Status::Waiting);
        assert_eq!(detect_pi_status("ready\n> "), Status::Waiting);
        assert_eq!(detect_pi_status("complete\npi>"), Status::Waiting);
        // Prompt takes priority over activity words lingering in scrollback
        assert_eq!(
            detect_pi_status("reading config.toml\nDone.\n>"),
            Status::Waiting
        );
    }

    #[test]
    fn test_detect_pi_status_idle() {
        assert_eq!(detect_pi_status("file saved"), Status::Idle);
        assert_eq!(detect_pi_status("random output text"), Status::Idle);
    }

    #[test]
    fn test_detect_droid_status_running() {
        assert_eq!(
            detect_droid_status("processing request\nesc to interrupt"),
            Status::Running
        );
        assert_eq!(
            detect_droid_status("thinking about your request"),
            Status::Running
        );
        assert_eq!(detect_droid_status("working on task"), Status::Running);
        assert_eq!(detect_droid_status("executing command"), Status::Running);
        assert_eq!(detect_droid_status("generating ⠋"), Status::Running);
    }

    #[test]
    fn test_detect_droid_status_waiting() {
        assert_eq!(
            detect_droid_status("run this command? (y/n)"),
            Status::Waiting
        );
        assert_eq!(detect_droid_status("approve changes?"), Status::Waiting);
        assert_eq!(
            detect_droid_status("execute this action? [y/n]"),
            Status::Waiting
        );
        assert_eq!(detect_droid_status("ready\ndroid>"), Status::Waiting);
        assert_eq!(detect_droid_status("done\n>"), Status::Waiting);
    }

    #[test]
    fn test_detect_droid_status_idle() {
        assert_eq!(detect_droid_status("file saved"), Status::Idle);
        assert_eq!(detect_droid_status("random output text"), Status::Idle);
    }

    #[test]
    fn test_detect_hermes_status_running_on_spinner() {
        assert_eq!(
            detect_hermes_status("◜ (｡•́︿•̀｡) pondering... (1.2s)"),
            Status::Running
        );
        assert_eq!(
            detect_hermes_status("◠ (⊙_⊙) contemplating... (2.4s)"),
            Status::Running
        );
        assert_eq!(
            detect_hermes_status("✧٩(ˊᗜˋ*)و✧ got it! (3.1s)"),
            Status::Running
        );
    }

    #[test]
    fn test_detect_hermes_status_running_on_tool_execution() {
        assert_eq!(
            detect_hermes_status("┊ 💻 terminal 'ls -la' (0.3s)"),
            Status::Running
        );
        assert_eq!(
            detect_hermes_status("┊ 🔍 web_search (1.2s)"),
            Status::Running
        );
    }

    #[test]
    fn test_detect_hermes_status_running_on_thinking_verbs() {
        assert_eq!(detect_hermes_status("reasoning…"), Status::Running);
        assert_eq!(
            detect_hermes_status("pondering the question"),
            Status::Running
        );
        assert_eq!(
            detect_hermes_status("analyzing the codebase"),
            Status::Running
        );
        assert_eq!(detect_hermes_status("computing result"), Status::Running);
    }

    #[test]
    fn test_detect_hermes_status_running_on_interrupt_hint() {
        // While running, Hermes shows "❯ Ctrl+C to interrupt…" in the prompt
        // area. Must detect as Running, not Waiting.
        assert_eq!(
            detect_hermes_status("┊ some response\n❯ Ctrl+C to interrupt…"),
            Status::Running
        );
        assert_eq!(
            detect_hermes_status("─ (¬‿¬) reasoning…\n❯ Ctrl+C to interrupt…"),
            Status::Running
        );
    }

    #[test]
    fn test_detect_hermes_status_waiting_on_approval() {
        assert_eq!(
            detect_hermes_status(
                "⚠️  DANGEROUS COMMAND: rm -rf /tmp\n[o]nce  |  [s]ession  |  [a]lways  |  [d]eny\nChoice [o/s/a/D]:"
            ),
            Status::Waiting
        );
        assert_eq!(
            detect_hermes_status("dangerous command detected\nproceed?"),
            Status::Waiting
        );
    }

    #[test]
    fn test_detect_hermes_status_idle_on_input_prompt() {
        // The bare ❯/⚡ prompt means "ready for next message" — Idle in AoE
        // semantics. Waiting is reserved for dangerous-command approval gates.
        assert_eq!(detect_hermes_status("some output\n❯"), Status::Idle);
        assert_eq!(detect_hermes_status("some output\n❯ "), Status::Idle);
        assert_eq!(detect_hermes_status("some output\n⚡"), Status::Idle);
    }

    #[test]
    fn test_detect_hermes_status_prompt_overrides_scrollback() {
        // If the input prompt is visible, don't mis-detect Running from old scrollback.
        assert_eq!(
            detect_hermes_status("pondering the question\ntask complete\n❯"),
            Status::Idle
        );
    }

    #[test]
    fn test_detect_hermes_status_idle_on_plain_text() {
        assert_eq!(detect_hermes_status("anything"), Status::Idle);
        assert_eq!(detect_hermes_status(""), Status::Idle);
        assert_eq!(
            detect_hermes_status("task completed successfully"),
            Status::Idle
        );
    }

    #[test]
    fn test_detect_settl_status_is_stub() {
        // settl uses hook-based detection; the stub always returns Idle
        assert_eq!(detect_settl_status("anything"), Status::Idle);
    }

    #[test]
    fn test_detect_kimi_status_is_stub() {
        // Kimi uses hook-based detection; the stub always returns Idle
        assert_eq!(detect_kimi_status("anything"), Status::Idle);
        assert_eq!(detect_kimi_status(""), Status::Idle);
    }

    #[test]
    fn test_detect_qwen_status_running() {
        assert_eq!(
            detect_qwen_status("processing request\nesc to interrupt"),
            Status::Running
        );
        assert_eq!(
            detect_qwen_status("⠋ Thinking about your request"),
            Status::Running
        );
        assert_eq!(detect_qwen_status("working ⠋"), Status::Running);
        assert_eq!(detect_qwen_status("loading ⠹"), Status::Running);
        assert_eq!(
            detect_qwen_status("⠹ Generating code\nesc to interrupt"),
            Status::Running
        );
        assert_eq!(detect_qwen_status("⠧ Reading file.rs"), Status::Running);
    }

    #[test]
    fn test_detect_qwen_status_waiting() {
        assert_eq!(detect_qwen_status("run command? (y/n)"), Status::Waiting);
        assert_eq!(
            detect_qwen_status("Allow this tool to run?"),
            Status::Waiting
        );
        assert_eq!(
            detect_qwen_status("pick an option\nenter to select"),
            Status::Waiting
        );
        assert_eq!(detect_qwen_status("done\n>"), Status::Waiting);
        assert_eq!(detect_qwen_status("done\nqwen>"), Status::Waiting);
        assert_eq!(
            detect_qwen_status("Select:\n❯ 1. Option A\n  2. Option B"),
            Status::Waiting
        );
        // Qwen's default theme uses `›` (U+203A), not `❯`.
        assert_eq!(
            detect_qwen_status("Select Authentication Method\n› 1. Alibaba ModelStudio"),
            Status::Waiting
        );
    }

    #[test]
    fn test_detect_qwen_status_idle() {
        assert_eq!(detect_qwen_status("file saved"), Status::Idle);
        assert_eq!(detect_qwen_status("random output text"), Status::Idle);
    }

    #[test]
    fn test_detect_antigravity_status_waiting_for_auth() {
        let content = "\
     ▄▀▀▄
    ▀▀▀▀▀▀

 Welcome to the Antigravity CLI. You are currently not signed in.

 ⣻  Signing in...";
        assert_eq!(detect_antigravity_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_antigravity_status_waiting_for_workspace_trust() {
        let content = "\
Accessing workspace:

/tmp/aoe-agy-smoke-proj

Do you trust the contents of this project?

Antigravity CLI requires permission to read, edit, and execute files here.

> Yes, I trust this folder
  No, exit

  ↑/↓ Navigate · enter Confirm
                                                         Gemini 3.5 Flash (High)";
        assert_eq!(detect_antigravity_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_antigravity_status_running() {
        assert_eq!(
            detect_antigravity_status("processing request\nesc to interrupt"),
            Status::Running
        );
        assert_eq!(
            detect_antigravity_status("⠋ Thinking about your request"),
            Status::Running
        );
    }

    #[test]
    fn test_detect_antigravity_status_running_on_stop_hint() {
        let content = "\
  Applying patch to src/session/instance.rs

  → Add a follow-up                                      ctrl+c to stop";
        assert_eq!(detect_antigravity_status(content), Status::Running);
    }

    #[test]
    fn test_detect_antigravity_status_running_on_live_activity_line() {
        let content = "\
  Generated summary for the previous step.

  Editing src/session/instance.rs";
        assert_eq!(detect_antigravity_status(content), Status::Running);
    }

    #[test]
    fn test_detect_antigravity_status_idle_on_completed_activity_phrases() {
        for content in [
            "Running tests completed successfully.",
            "Reading config.toml finished.",
            "Editing src/session/instance.rs done.",
            "Testing finished with success.",
        ] {
            assert_eq!(detect_antigravity_status(content), Status::Idle);
        }
    }

    #[test]
    fn test_detect_antigravity_status_waiting_for_prompt() {
        assert_eq!(
            detect_antigravity_status("run command? (y/n)"),
            Status::Waiting
        );
    }

    #[test]
    fn test_detect_antigravity_status_waiting_for_tool_approval() {
        // Real header rendered above Antigravity tool permission prompts.
        // "approval" does not contain "approve", so the shared
        // contains_approval_prompt helper misses this header; the detector
        // matches "approval required" explicitly instead.
        let content = "\
read_file
path: /workspace/secrets.env

⚠ Approval Required

> Yes, just this once
  Yes, allow always
  No, deny access";
        assert_eq!(detect_antigravity_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_antigravity_status_waiting_user_approval_status_line() {
        // "awaiting user approval" is the status line shown while the agent
        // is blocked on the user's tool-permission decision.
        let content = "I'll read that file now.\n awaiting user approval.";
        assert_eq!(detect_antigravity_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_antigravity_status_idle() {
        assert_eq!(detect_antigravity_status("file saved"), Status::Idle);
        assert_eq!(
            detect_antigravity_status("random output text"),
            Status::Idle
        );
    }

    #[test]
    fn test_detect_kiro_status_is_stub() {
        // Kiro CLI uses hook-based detection; the stub always returns Idle
        assert_eq!(detect_kiro_status("anything"), Status::Idle);
    }
}
