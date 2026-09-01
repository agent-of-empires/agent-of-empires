//! Status detection for agent sessions

use crate::session::Status;

use super::utils::strip_ansi;

/// Rules-aware pane detection for `profile`'s session. Configured declarative
/// rules outrank the built-in detector: they are the only detection path for a
/// custom agent that is not the same binary as any built-in, and an explicit
/// override when the user writes rules for a built-in name. Rules are looked up
/// per `(profile, tool)`, so a session consults only its own profile's rules.
pub fn detect_status_from_content_in(profile: &str, content: &str, tool: &str) -> Status {
    // Strip ANSI escape codes before passing to detectors. capture-pane is
    // called with -e (to preserve colors for the TUI preview), but color codes
    // interspersed in text like "esc interrupt" break plain substring matches.
    let clean = strip_ansi(content);
    if let Some(status) = super::status_rules::detect(profile, tool, &clean) {
        return status;
    }
    crate::agents::get_agent(tool)
        .map(|a| (a.detect_status)(&clean))
        .unwrap_or(Status::Idle)
}

/// The one manifest-backed detection entry point: a profile's own
/// `[[agents.<name>.status_rules]]` first, then the agent's manifest.
///
/// The status poller and `aoe session capture` both route through this so they
/// cannot disagree about a configured rule or a terminal title (#3625). The
/// two identities are separate on purpose: `rules_tool` is what configured
/// rules are keyed to (`status_rules::detection_tool`, which keeps a session's
/// own rules ahead of its `agent_detect_as` alias), while `agent` is the
/// manifest identity, which follows the alias. `clean` must already be
/// ANSI-stripped; `osc_title` is tmux's `#{pane_title}`, empty when unknown.
///
/// `None` means the agent has no manifest and no configured rules, leaving the
/// verdict to the caller.
pub fn detect_with_rules(
    profile: &str,
    rules_tool: &str,
    agent: &str,
    clean: &str,
    osc_title: &str,
    hook: Option<super::detect::HookObservation>,
) -> Option<super::detect::Detection> {
    if let Some(status) = super::status_rules::detect(profile, rules_tool, clean) {
        return Some(super::detect::Detection {
            status: Some(status),
            visible: true,
            rule: "configured_status_rule",
        });
    }
    super::detect::detect(agent, clean, osc_title, hook)
}

/// Run an agent's detection manifest over one capture, falling back to Idle
/// when no rule matches. The per-agent `detect_*_status` entry points are thin
/// wrappers so the agent registry keeps its stable function pointers.
pub fn detect_via_manifest(
    agent: &str,
    raw_content: &str,
    osc_title: &str,
    hook: Option<super::detect::HookObservation>,
) -> Status {
    super::detect::detect(agent, &strip_ansi(raw_content), osc_title, hook)
        .and_then(|d| d.status)
        .unwrap_or(Status::Idle)
}

/// Rules-free pane detection: strip ANSI, then the built-in detector only, no
/// status-rule registry consult. Used by callers that are keyed to the
/// built-in / alias identity rather than to a session's profile, so their
/// behavior is independent of any configured `[[agents.<name>.status_rules]]`.
pub fn detect_status_from_content(content: &str, tool: &str) -> Status {
    let clean = strip_ansi(content);
    crate::agents::get_agent(tool)
        .map(|a| (a.detect_status)(&clean))
        .unwrap_or(Status::Idle)
}

/// Claude Code pane detection. The rules, and the reasoning behind each, are
/// in `detect/manifests/claude.toml`.
pub fn detect_claude_status(content: &str) -> Status {
    detect_claude(content, "", None)
}

/// Claude pane detection with the two signals a bare capture does not carry:
/// the terminal title the agent publishes, and its status-hook file. Both are
/// rules in `detect/manifests/claude.toml` alongside the screen shapes, so
/// their authority is declared rather than layered on afterwards.
pub fn detect_claude(
    content: &str,
    osc_title: &str,
    hook: Option<super::detect::HookObservation>,
) -> Status {
    super::detect::detect("claude", &strip_ansi(content), osc_title, hook)
        .and_then(|d| d.status)
        .unwrap_or(Status::Idle)
}

pub fn detect_opencode_status(raw_content: &str) -> Status {
    detect_via_manifest("opencode", raw_content, "", None)
}

pub fn detect_vibe_status(raw_content: &str) -> Status {
    detect_via_manifest("vibe", raw_content, "", None)
}

/// Codex pane detection. See `detect/manifests/codex.toml`.
pub fn detect_codex_status(raw_content: &str) -> Status {
    detect_via_manifest("codex", raw_content, "", None)
}

/// Cursor agent status is detected via hooks first, but pane parsing is still
/// needed when hooks are missing or the Cursor CLI is executing a long-running
/// turn between hook writes.
pub fn detect_cursor_status(raw_content: &str) -> Status {
    detect_cursor(raw_content, None)
}

/// Cursor pane detection with the session's status-hook file, which is a rule
/// in `detect/manifests/cursor.toml` alongside the screen shapes.
pub fn detect_cursor(raw_content: &str, hook: Option<super::detect::HookObservation>) -> Status {
    super::detect::detect("cursor", &strip_ansi(raw_content), "", hook)
        .and_then(|d| d.status)
        .unwrap_or(Status::Idle)
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
    detect_via_manifest("copilot", raw_content, "", None)
}

/// Pi pane detection. Pi has no hooks and always auto-approves, so the pane is
/// the only signal it has. See `detect/manifests/pi.toml`.
pub fn detect_pi_status(raw_content: &str) -> Status {
    detect_via_manifest("pi", raw_content, "", None)
}

/// omp pane detection. Its markers stack, so the rules are arbitrated by
/// position rather than rank. See `detect/manifests/omp.toml`.
pub fn detect_omp_status(raw_content: &str) -> Status {
    detect_via_manifest("omp", raw_content, "", None)
}

/// Factory Droid CLI status detection via tmux pane parsing.
/// Droid uses an interactive REPL similar to other coding agents. It shows
/// activity indicators while processing and prompts for input when idle.
pub fn detect_droid_status(raw_content: &str) -> Status {
    detect_via_manifest("droid", raw_content, "", None)
}

/// Hermes (NousResearch) status detection via tmux pane parsing.
/// Used as a fallback when the YAML hook system hasn't written a status file yet.
/// Detects spinner faces (◜ ◠ ✧), tool execution prefix (┊), thinking verbs,
/// dangerous-command approval prompt, and input prompt (❯ / ⚡).
pub fn detect_hermes_status(raw_content: &str) -> Status {
    detect_via_manifest("hermes", raw_content, "", None)
}

/// Agents whose status comes from hooks alone: Kiro, settl, Kimi Code and
/// Prime Agent render no pane shape worth parsing, so the pane fallback
/// reports Idle and the hook file speaks for them.
pub fn detect_hook_only_status(_content: &str) -> Status {
    Status::Idle
}

pub fn detect_gemini_status(raw_content: &str) -> Status {
    detect_via_manifest("gemini", raw_content, "", None)
}

/// Qwen Code status detection via tmux pane parsing.
/// Qwen Code is a fork of Gemini CLI, so the running/waiting markers mirror
/// Gemini's: braille spinner + "esc to interrupt" while working, approval
/// prompts and a numbered `❯` selection menu while waiting.
pub fn detect_qwen_status(raw_content: &str) -> Status {
    detect_via_manifest("qwen", raw_content, "", None)
}

pub fn detect_antigravity_status(raw_content: &str) -> Status {
    detect_via_manifest("antigravity", raw_content, "", None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3625: `aoe session capture` went straight to the manifest, so a
    /// profile's own `[[agents.<name>.status_rules]]` were skipped and the
    /// terminal title arrived empty. Both are inputs the poller supplies, and
    /// both now come through this one entry point.
    #[test]
    #[serial_test::serial]
    fn detect_with_rules_puts_configured_rules_and_the_title_in_reach() {
        const PROFILE: &str = "detect-with-rules-test";
        let _registry = super::super::status_rules::ProfileRegistryGuard::take(PROFILE);
        let mut config = crate::session::Config::default();
        config
            .agents
            .entry("claude".to_string())
            .or_default()
            .status_rules = vec![crate::session::config::StatusRule {
            status: crate::agents::HookStatus::Waiting,
            contains: Some("deploy to prod?".to_string()),
            regex: None,
        }];
        super::super::status_rules::install_from_config(PROFILE, &config);

        // A screen the manifest reads as Running: the configured rule has to
        // outrank it, not merely fill in where the manifest is silent.
        let running = "\u{2736} Working\u{2026} (5s)\ndeploy to prod?\n";
        assert_eq!(
            detect_via_manifest("claude", running, "", None),
            Status::Running,
            "fixture invariant: the manifest alone reads this screen as Running"
        );
        let detection = detect_with_rules(PROFILE, "claude", "claude", running, "", None)
            .expect("claude has a manifest");
        assert_eq!(detection.status, Some(Status::Waiting));
        assert_eq!(detection.rule, "configured_status_rule");

        // The terminal title is a rule region of its own, and Claude ranks it
        // above every screen shape. A capture that drops it cannot see this.
        let idle_screen = "turn over\n";
        let titled = detect_with_rules(
            "no-rules-profile-for-title-test",
            "claude",
            "claude",
            idle_screen,
            "\u{2807}",
            None,
        )
        .expect("claude has a manifest");
        assert_eq!(titled.status, Some(Status::Running));
        assert_eq!(titled.rule, "osc_title_working");
        assert_eq!(
            detect_via_manifest("claude", idle_screen, "", None),
            Status::Idle,
            "the same capture without the title is what the CLI used to report"
        );
    }

    /// Whether one manifest rule matches a fixture, used where a test asserts
    /// the shape it claims to exercise is really present.
    fn claude_rule_matches(rule: &str, content: &str) -> bool {
        super::super::detect::rule_matches("claude", rule, &strip_ansi(content), "", None)
    }

    /// Whether one of Vibe's manifest rules matches a fixture.
    fn vibe_rule_matches(rule: &str, content: &str) -> bool {
        super::super::detect::rule_matches("vibe", rule, &strip_ansi(content), "", None)
    }

    /// A `waiting` write left behind by a prompt the user cancelled: the write
    /// nothing clears, which is why the screen has to be able to outrank it.
    fn stale_wait() -> Option<super::super::detect::HookObservation> {
        Some(hook_at(
            Status::Waiting,
            Some(std::time::Duration::from_secs(600)),
        ))
    }

    /// The rule that decided a capture, which is what the status-change log
    /// records.
    fn claude_rule(content: &str) -> &'static str {
        super::super::detect::detect("claude", &strip_ansi(content), "", None)
            .expect("claude has a manifest")
            .rule
    }

    /// The freshness bound a `running` hook write keeps its priority over
    /// parked evidence for, read from the manifest that declares it.
    fn claude_fresh_bound() -> std::time::Duration {
        super::super::detect::rule_max_age("claude", "hook_running_fresh")
            .expect("hook_running_fresh declares a bound")
    }

    /// A hook observation for the detection tests. `None` age means the write
    /// exists but its mtime could not be read, the case the freshness bounds
    /// deliberately do not fire on.
    fn hook_at(
        status: Status,
        age: Option<std::time::Duration>,
    ) -> super::super::detect::HookObservation {
        super::super::detect::HookObservation { status, age }
    }

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
    fn test_detect_claude_status_running_on_abbreviated_token_counter() {
        // Claude abbreviates the live count once a turn runs long
        // (`↓ 44.7k tokens`); the spinner line's ellipsis can sit past the
        // second word, so the counter is that pane's only running signal.
        // Captured from #3440.
        let long_turn_pane = "\
● Clippy clean on both; waiting on the base-commit control.\n\
  Ran 2 shell commands\n\
✻ Judging #3413 feedback… (22m 8s · ↓ 44.7k tokens)\n\
┌─────\n\
❯\n\
└─────\n\
  ⏵⏵ auto mode on";
        // The synthetic rows put the ellipsis on the third word, like the
        // captured pane: `claude_line_is_active_spinner` then rejects the
        // line and the counter is the only running signal being pinned.
        let cases = [
            ("issue pane", long_turn_pane),
            (
                "k suffix",
                "✶ Summarizing the findings… (53s · ↓ 7.0k tokens)",
            ),
            (
                "m suffix",
                "✶ Summarizing the findings… (4s · ↓ 1.2m tokens)",
            ),
            ("g suffix", "✶ Summarizing the findings… (4s · ↓ 3g tokens)"),
            (
                "integer k, no decimal",
                "✶ Summarizing the findings… (4s · ↓ 512k tokens)",
            ),
            (
                "wrap between duration and arrow",
                "(22m 8s\n↓ 44.7k tokens)",
            ),
            // Narrow panes wrap mid-token: the joined capture carries the
            // newline inside what was `8s`.
            ("wrap inside seconds", "(22m 8\ns · ↓ 44.7k tokens)"),
        ];
        for (name, pane) in cases {
            assert_eq!(detect_claude_status(pane), Status::Running, "{name}");
        }
    }

    #[test]
    fn test_has_claude_live_token_counter_variants() {
        // Accepts every count form Claude renders inside the parenthesized
        // live counter plus the regular extensions of that shape (m, g and
        // bare decimals are extrapolations, not captures); rejects the
        // unparenthesized frozen agents-strip counters (#2909) and
        // malformed echoes.
        let cases = [
            ("plain integer", "(4s · ↓ 88 tokens)", true),
            ("multi-digit", "(12s · ↓ 1234 tokens)", true),
            ("decimal with k", "(53s · ↓ 7.0k tokens)", true),
            ("plain decimal", "(4s · ↓ 44.7 tokens)", true),
            ("integer with k", "(4s · ↓ 512k tokens)", true),
            ("decimal with m", "(4s · ↓ 1.2m tokens)", true),
            ("integer with g", "(4s · ↓ 3g tokens)", true),
            ("two-digit fraction", "(4s · ↓ 1.23m tokens)", true),
            // A bare `)` opening the next line still completes a wrapped
            // counter; pinning it so a future tightening knows what it
            // changes.
            (
                "wrapped before paren",
                "✻ Judging #3413 feedback… (4s · ↓ 88 tokens\n)",
                true,
            ),
            // Transcript prose may follow on the next physical line; only
            // the paren's own line must stay blank.
            (
                "prose on the following line",
                "(4s · ↓ 88 tokens)\nRan 2 shell commands",
                true,
            ),
            (
                "wrapped across lines",
                "✶ Summarizing the findings… (22m 8s · ↓ 44.7k\ntokens)",
                true,
            ),
            // Duration segments without their own digits are malformed
            // pane text, not a counter.
            ("empty duration", "(s · ↓ 88 tokens)", false),
            ("unit without own digits", "(22m s · ↓ 88 tokens)", false),
            ("no count", "(4s · ↓ tokens)", false),
            ("comma separator", "(4s · ↓ 12,345 tokens)", false),
            ("uppercase suffix", "(4s · ↓ 44.7K tokens)", false),
            ("non-digit count", "(4s · ↓ many tokens)", false),
            // The duration must sit inside an opening paren; an anchor tail
            // loose in prose is not a live counter (review finding on
            // #3488).
            ("no opening paren", "summary: 4s · ↓ 88 tokens)", false),
            (
                "prose before the duration",
                "see issue s · ↓ 88 tokens)",
                false,
            ),
            ("double dot", "(4s · ↓ 44..7k tokens)", false),
            // A dot with no digit after it must not be eaten as a fraction,
            // or `44.tokens)` would half-parse into a live counter.
            ("no digit after dot", "(4s · ↓ 44.tokens)", false),
            // Only whitespace may follow the closing paren: a quoted
            // literal row carries punctuation there and must stay
            // rejected, echo or not.
            ("punctuation after paren", "(4s · ↓ 7.0k tokens),", false),
            ("quote after paren", "(4s · ↓ 88 tokens)\",", false),
            // A decoy anchor inside footer text must not stop the scan
            // from finding the real counter later in the window.
            (
                "decoy anchor then real counter",
                "  ⏵⏵ bypass permissions on · ← for agents · ↓ to manage\n(4s · ↓ 88 tokens)",
                true,
            ),
            // The anchor needs the duration's `s`; a bare arrow in prose is
            // not a counter.
            ("bare arrow in prose", "watch the ↓ 88 tokens) chart", false),
            // Text after the closing paren on its own line means the shape
            // is quoted prose, not a live counter.
            ("prose after paren", "(4s · ↓ 88 tokens) renders", false),
            // A following physical line starting with `)` must not supply
            // the paren to a prose line ending in the anchor tail.
            (
                "next line completes shape",
                "● The helper reads s · ↓ 42 tokens\n) -> Status {",
                false,
            ),
            // Relaxing the anchor to a bare middle-dot arrow would let
            // ordinary prose through; the duration's `s` is load-bearing.
            (
                "middle dot arrow without duration",
                "chart · ↓ 88 tokens)",
                false,
            ),
            // Unobserved magnitude units stay out of the alphabet.
            ("b suffix", "(4s · ↓ 512b tokens)", false),
        ];
        for (name, content, expected) in cases {
            assert_eq!(
                claude_rule_matches("live_token_counter", content),
                expected,
                "{name}"
            );
        }
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
    fn test_detect_claude_status_waiting_on_ask_user_question() {
        // Regression: Claude's AskUserQuestion tool renders a selection UI while
        // blocked on the user, but the question is author-written (no "Do you
        // want to" phrasing), so the permission-prompt detector misses it and
        // the session reports Running the whole time it is waiting. The
        // "Enter to select · ↑/↓ to navigate" footer is the marker.
        let content = "\
  PREMISE GATE (your call, not auto-decided).
  So which shape do you actually want?

  ❯ 1. Static plugin (comparator stays core)
    2. True-worker extraction (as first scoped)
    3. Don't extract; ship the valuable byproducts

  Enter to select · ↑/↓ to navigate · Esc to cancel";
        assert_eq!(detect_claude_status(content), Status::Waiting);
    }

    #[test]
    fn test_detect_claude_status_waiting_on_multi_question_ask_user_question() {
        // The multi-question footer variant carries the extra "Tab to switch
        // questions" / "n to add notes" hints; it must still read as Waiting.
        let content = "\
  How should the encryption key be managed?

  ❯ 1. Require OTARI_SECRET_KEY
    2. Auto-generate KEK to a file
    3. Auto-generate KEK in DB

  Enter to select · ↑/↓ to navigate · n to add notes · Tab to switch questions · Esc to cancel";
        assert_eq!(detect_claude_status(content), Status::Waiting);
    }

    #[test]
    fn test_reconcile_claude_hook_status_waiting_on_ask_user_question() {
        // The hook reports Running (PreToolUse for AskUserQuestion fired) but the
        // pane is parked on the selection UI. The reconciler must downgrade to
        // Waiting. ANSI is preserved to exercise the strip path.
        let pane = "\x1b[1m  Which approach do you prefer?\x1b[0m\n\
\x1b[1m❯ 1. First\x1b[0m\n    2. Second\n\n\
  Enter to select · ↑/↓ to navigate · Esc to cancel";
        assert_eq!(
            detect_claude(pane, "", Some(hook_at(Status::Running, None))),
            Status::Waiting
        );
    }

    #[test]
    fn test_detect_claude_status_running_when_pane_echoes_fixture_footer() {
        // A Read/grep of this repo's own test fixtures (or a diff of this
        // file) echoes the AskUserQuestion footer into the pane while a turn
        // is live, alongside prose that quotes a numbered choice. Echoed
        // footer lines carry a prefix (line numbers, `+`, `⎿`), so the footer
        // match is anchored to the start of the trimmed line and must not
        // fire; the live spinner wins. Same hardening rationale as the
        // mode-cycle footer anchoring in claude_pane_shows_ready_prompt.
        let content = "\
● The fixture renders these options:
  ❯ 1. Static plugin (comparator stays core)
    2. True-worker extraction
  and then the footer line:
  ⎿ 2052   Enter to select · ↑/↓ to navigate · Esc to cancel

✶ Herding… (12s · ↓ 1234 tokens)
  esc to interrupt";
        assert_eq!(detect_claude_status(content), Status::Running);
    }

    #[test]
    fn test_detect_claude_status_running_not_confused_by_select_footer_prose() {
        // The select footer must not be mistaken for a live prompt when it only
        // appears as quoted text (e.g. this file's own fixtures shown in tool
        // output) with an active spinner running below it: the footer needs a
        // real numbered choice AND the spinner still wins if there is none.
        let content = "\
  The footer reads \"Enter to select · ↑/↓ to navigate\" while parked.

✶ Working… (4s · ↓ 88 tokens)
  esc to interrupt";
        assert_eq!(detect_claude_status(content), Status::Running);
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
            detect_claude(pane, "", Some(hook_at(Status::Running, None))),
            Status::Waiting
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_keeps_running_without_prompt() {
        let pane = "✶ Working… (4s · ↓ 88 tokens)\n  esc to interrupt";
        assert_eq!(
            detect_claude(pane, "", Some(hook_at(Status::Running, None))),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_passes_non_running_through() {
        // A `waiting` write speaks for an empty capture, and stock question
        // phrasing without a cursored menu row is prose rather than a prompt,
        // so an `idle` write stands against it.
        assert_eq!(
            detect_claude("", "", Some(hook_at(Status::Waiting, None))),
            Status::Waiting
        );
        assert_eq!(
            detect_claude(
                "Do you want to proceed?\n1. Yes",
                "",
                Some(hook_at(Status::Idle, None))
            ),
            Status::Idle
        );
    }

    #[test]
    fn test_stale_waiting_hook_blank_pane_keeps_waiting() {
        // No evidence either way: a blank or whitespace-only capture must not
        // flip a live prompt to Idle. Keep the hook's Waiting.
        assert_eq!(detect_claude("", "", stale_wait()), Status::Waiting);
        assert_eq!(detect_claude("   \n\n", "", stale_wait()), Status::Waiting);
    }

    #[test]
    fn test_reconcile_claude_idle_hook_running_pane_upgrades_to_running() {
        // The boundary race: a queued prompt submits the moment a turn ends,
        // and the fire-and-forget `idle_prompt` notification lands its `idle`
        // write after `UserPromptSubmit`'s `running`. The pane shows the new
        // turn's live spinner, so the fresh idle must read as Running.
        let pane = "✶ Working… (4s · ↓ 88 tokens)\n  esc to interrupt";
        assert_eq!(
            detect_claude(pane, "", Some(hook_at(Status::Idle, None))),
            Status::Running
        );
    }

    /// Verbatim `tmux capture-pane -p` of a claude pane parked at the
    /// folder-trust prompt, 2026-08-15. `aoe status` read `0 waiting` while
    /// this was on screen.
    const CLAUDE_FOLDER_TRUST_PROMPT: &str = "\
 Accessing workspace:
 /tmp/scratch/exp
 Quick safety check: Is this a project you created or one you trust? (Like your
 own code, a well-known open source project, or work from your team). If not,
 take a moment to review what's in this folder first.
 Claude Code'll be able to read, edit, and execute files here.
 Security guide
 \u{276f} 1. Yes, I trust this folder
   2. No, exit
";

    /// The trust prompt's option label is menu text, so matching it as the
    /// question would collapse the two-signal guard: an assistant quoting the
    /// option while working renders both signals on one line.
    const CLAUDE_ASSISTANT_QUOTING_THE_TRUST_OPTION: &str = "\
 I found the folder-trust handling in src/tmux/status_detection.rs. The two
 menu options Claude renders are:
   1. Yes, I trust this folder
   2. No, exit
 The detector matches those against the numbered-choice helper.
 \u{2736} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)
   esc to interrupt
";

    #[test]
    fn claude_prose_carrying_a_numbered_list_is_not_waiting() {
        // A blocking prompt outranks the running signal (#1913), so whatever
        // reads as a menu wins over the spinner below it. The cursor is what
        // keeps an assistant-authored numbered list out of that: the guard's
        // other signal is a phrase common in ordinary turn text, so each pane
        // here carries both a numbered list and one such phrase.
        let cases = [
            // Prose: a numbered list and a stock question, no cursor anywhere.
            "\
 Tensions I'd want us to discuss, not resolve by menu:
 1. What does deterministic bind? The arc's step gates what runs may launch.
 2. Whether product survives as a word.
 3. What's left of lane the tool under this shape.
 Where do you want to dig, the delta policy or the teeth question?
 \u{2733} Puttering\u{2026} (26s \u{b7} thinking more with high effort)",
            // A markdown blockquote of a real menu renders `>` ahead of the
            // number, which is why `>` is not read as a cursor.
            "\
\u{25cf} The menu it showed was:
> 1. Yes
> 2. No
\u{25cf} Do you want to proceed with that reading?
 \u{273b} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)
   esc to interrupt
",
            // U+203A is `figures.pointerSmall`. Claude renders its cursor from
            // `figures.pointer`, so this shape is a bulleted list, not a menu.
            "\
  Do you want to proceed?
  \u{203a} 1. Yes
    2. No
\u{2736} Herding\u{2026} (53s \u{b7} \u{2193} 7.0k tokens)",
        ];
        for content in cases {
            assert!(
                claude_rule_matches("active_spinner", content),
                "fixture must carry a live spinner, or it proves nothing about the ranking",
            );
            assert_eq!(detect_claude_status(content), Status::Running, "{content}");
        }
    }

    #[test]
    fn claude_assistant_quoting_the_trust_option_is_not_waiting() {
        // Pinned, not implied. The fixture's spinner line failed to match for
        // TWO independent reasons: it used ASCII dots rather than U+2026, and
        // its frame char was U+2726, which is not in `CLAUDE_SPINNER_CHARS`.
        // Both are fixed above. `Running` still did not rest on the interrupt
        // hint alone - the live token counter is a second signal - so both are
        // asserted here rather than left to the verdict. Raised by njbrake in
        // review.
        assert!(
            claude_rule_matches("active_spinner", CLAUDE_ASSISTANT_QUOTING_THE_TRUST_OPTION),
            "fixture must carry a live spinner",
        );
        assert!(
            claude_rule_matches(
                "live_token_counter",
                CLAUDE_ASSISTANT_QUOTING_THE_TRUST_OPTION
            ),
            "fixture must carry a live token counter",
        );
        assert_eq!(
            detect_claude_status(CLAUDE_ASSISTANT_QUOTING_THE_TRUST_OPTION),
            Status::Running
        );
    }

    /// The same prompt as Claude wraps it once the pane is too narrow to hold
    /// the question on one line. `recent_lower` is a newline join, so the
    /// unwrapped `contains` misses here and the pane read `Idle` again - the
    /// bug this whole change exists to fix, in the width band AoE's own
    /// side-by-side preview produces. Raised by njbrake in review.
    const CLAUDE_FOLDER_TRUST_PROMPT_WRAPPED: &str = "\
 Accessing workspace:
 /tmp/scratch/exp
 Quick safety check: Is this a project you created or one you
 trust? (Like your own code, a well-known open source project,
 or work from your team). If not, take a moment to review what's
 in this folder first.
 Claude Code'll be able to read, edit, and execute files here.
 Security guide
 \u{276f} 1. Yes, I trust this folder
   2. No, exit
";

    /// The collapsed match joins across newlines, and unlike
    /// `claude_pane_has_running_signal`'s collapse it biases toward Waiting,
    /// which outranks Running. Without the option-label requirement these all
    /// read `Waiting`; the last one is an actively generating turn.
    #[test]
    fn claude_wrapped_trust_question_in_prose_is_not_a_prompt() {
        let quoted_across_a_break = "\
\u{25cf} The detector asks: Is this a project you created or
 one you trust? That phrase is the third arm.
 1. the first arm
 2. the second arm
";
        assert_eq!(detect_claude_status(quoted_across_a_break), Status::Idle);

        let unrelated_lines_that_join = "\
 Q: what is this
 a project you created or one you trust is one you can vouch for.
 1. yes
";
        assert_eq!(
            detect_claude_status(unrelated_lines_that_join),
            Status::Idle
        );

        let while_generating = "\
\u{25cf} The detector asks: Is this a project you created or
 one you trust? That phrase is the third arm.
 1. the first arm
 \u{2736} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)
   esc to interrupt
";
        assert_eq!(detect_claude_status(while_generating), Status::Running);
    }

    /// The prompt on an 18-column pane, i.e. the stacked preview at a
    /// 22-column viewport (`responsive.rs` documents viewports down to ~26,
    /// so this is below the documented floor and deliberately so). The option
    /// label wraps too, which a label match anchored to a single
    /// numbered-choice line misses. Abridged, not verbatim: the trailing prose
    /// and the `Security guide` row are dropped to keep the fixture short.
    const CLAUDE_FOLDER_TRUST_PROMPT_NARROW: &str = "\
 Quick safety
 check: Is this a
 project you
 created or one
 you trust? (Like
 your own code, a
 well-known open
 source project.)
 \u{276f} 1. Yes, I trust
   this folder
   2. No, exit
";

    /// The label match is anchored to the choice block, not the whole window.
    /// Window-wide collapsing found the label in ordinary prose, and because a
    /// blocking rule outranks the running signal these all reported `Waiting`
    /// on an actively generating turn.
    #[test]
    fn claude_trust_label_in_prose_is_not_a_prompt() {
        let label_in_prose = "\
\u{25cf} The prompt asks: Is this a project you created or one you trust?
 The highlighted option reads Yes, I trust this folder.
 1. the first arm
 2. the second arm
 \u{2736} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)
   esc to interrupt
";
        assert_eq!(detect_claude_status(label_in_prose), Status::Running);

        let label_spliced_from_two_lines = "\
 \u{25cf} the answer the user gives is Yes,
 I trust this folder more than the upstream mirror. Is this a project
 you created or one you trust? was the wording.
 1. unrelated
 \u{2736} Working\u{2026} (3s)
   esc to interrupt
";
        assert_eq!(
            detect_claude_status(label_spliced_from_two_lines),
            Status::Running
        );
    }

    /// A `cat -n` / `nl` echo of this file's own fixture. It is rejected by the
    /// option-text requirement, not by anything that recognises the `  2812 `
    /// prefix; the anchor row the block opens on is ` 1. an unrelated list
    /// item`, whose text does not start with the label.
    ///
    /// The `>` blockquote and `grep -n` (`N:content`, no space) cases live in
    /// `claude_trust_label_outside_a_menu_row_is_not_a_prompt`. Worth knowing
    /// which rejects what: `claude_line_is_numbered_choice` STRIPS a leading
    /// `>`, so a blockquote row is a valid numbered choice to it, and only
    /// `claude_trust_choice_option_text` (which tolerates just `❯`) turns it
    /// away.
    #[test]
    fn claude_echoed_trust_fixture_is_not_a_prompt() {
        let echoed = "\
  2812 \u{276f} 1. Yes, I trust this folder
  2813   2. No, exit
\u{25cf} That is the fixture. Is this a project you created or one you trust?
 1. an unrelated list item
 \u{2736} Working\u{2026} (4s)
   esc to interrupt
";
        assert_eq!(detect_claude_status(echoed), Status::Running);
    }

    /// The three shapes a whole-window or bare-line anchor let through, each
    /// an actively generating turn that reported `Waiting`.
    #[test]
    fn claude_trust_label_outside_a_menu_row_is_not_a_prompt() {
        let blockquote = "\
\u{25cf} Here is what the docs show:
> 1. Yes, I trust this folder
> 2. No, exit
\u{25cf} And the question was: Is this a project you created or one you trust?
 \u{273b} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)
   esc to interrupt
";
        assert_eq!(detect_claude_status(blockquote), Status::Running);

        // Defended by requirement 2, not by any echo filter: the anchor row is
        // ` 1. an unrelated list item`, whose option text fails `starts_with`.
        let echoed_after_a_list = "\
\u{25cf} That is the fixture. Is this a project you created or one you trust?
 1. an unrelated list item
  2812 \u{276f} 1. Yes, I trust this folder
  2813   2. No, exit
 \u{273b} Working\u{2026} (4s)
   esc to interrupt
";
        assert_eq!(detect_claude_status(echoed_after_a_list), Status::Running);

        let numbered_prose_plan = "\
\u{25cf} The plan:
 1. read the prompt, which asks: Is this a project you created or one you trust?
 the highlighted option is Yes, I trust this folder
 and then we proceed.
 \u{273b} Working\u{2026} (9s)
   esc to interrupt
";
        assert_eq!(detect_claude_status(numbered_prose_plan), Status::Running);
    }

    #[test]
    fn claude_folder_trust_prompt_is_waiting() {
        let cases = [
            ("default", CLAUDE_FOLDER_TRUST_PROMPT),
            ("wrapped", CLAUDE_FOLDER_TRUST_PROMPT_WRAPPED),
            ("narrow", CLAUDE_FOLDER_TRUST_PROMPT_NARROW),
        ];
        for (name, fixture) in cases {
            assert_eq!(detect_claude_status(fixture), Status::Waiting, "{name}");
        }
    }

    /// The shapes the label anchor admits: an unprefixed verbatim menu row, a
    /// `cat`-style echo indented under a tool result, a `--nocapture` dump of
    /// this file's own fixtures, and trailing prose after the label. Each one
    /// reproduces the whole prompt, so the label and the question both match;
    /// the running signal is what keeps them Running.
    #[test]
    fn claude_echoed_trust_prompt_during_a_turn_is_not_waiting() {
        let bodies = [
            " \u{276f} 1. Yes, I trust this folder\n   2. No, exit",
            "     \u{276f} 1. Yes, I trust this folder\n       2. No, exit",
            "  \u{276f} 1. Yes, I trust this folder\n    2. No, exit\n     test result: FAILED",
            " 1. Yes, I trust this folder is what you pick, and then\n 2. the session starts",
        ];
        for body in bodies {
            let pane = format!(
                "\u{25cf} The first-run dialog reads:\n \
                 Quick safety check: Is this a project you created or one you trust?\n\
                 {body}\n \u{2736} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)\n   \
                 esc to interrupt\n"
            );
            assert_eq!(detect_claude_status(&pane), Status::Running, "{body:?}");
        }
    }

    #[test]
    fn test_reconcile_claude_idle_hook_blocking_prompt_upgrades_to_waiting() {
        // Same race shape for the permission_prompt notification: the pane
        // shows a blocking approval prompt while the file says idle.
        let pane = "\
  Do you want to proceed?\n\
  ❯ 1. Yes\n    2. No\n\n  Esc to cancel · Tab to amend";
        assert_eq!(
            detect_claude(pane, "", Some(hook_at(Status::Idle, None))),
            Status::Waiting
        );
    }

    #[test]
    fn test_reconcile_claude_idle_hook_parked_pane_keeps_idle() {
        // Genuine turn end: completion line above the ready prompt, no live
        // signal. The hook's idle is accepted.
        let pane = "✻ Worked for 1m 52s\n❯\n  ? for shortcuts";
        assert_eq!(
            detect_claude(pane, "", Some(hook_at(Status::Idle, None))),
            Status::Idle
        );
        // An empty capture carries no evidence either way; keep the hook.
        assert_eq!(
            detect_claude("  \n \n", "", Some(hook_at(Status::Idle, None))),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_claude_idle_hook_resists_echoed_running_text() {
        // A parked session whose last tool output echoed running-signal text
        // (a diff of this repo's own detector, quoted docs) must keep the
        // hook's idle. Echoed lines carry a prefix (line numbers, `+`,
        // quotes), so the anchored spinner-line match rejects them; the loose
        // interrupt-hint and token-counter substrings would have pinned this
        // pane on Running with no recovery until the text scrolled away.
        let pane = "\
●  Read(src/tmux/status_detection.rs)\n\
  ⎿  2472:        let pane = \"✶ Working… (4s · ↓ 88 tokens)\\n  esc to interrupt\";\n\
  ⎿  +    if collapsed.contains(\"esc to interrupt\") {\n\
✻ Worked for 12s\n\
❯\n\
  ? for shortcuts";
        assert_eq!(
            detect_claude(pane, "", Some(hook_at(Status::Idle, None))),
            Status::Idle
        );
    }

    #[test]
    fn test_claude_deciding_rule_names_the_evidence() {
        // The status-change log carries the rule that decided, so a
        // wrong-state report says which shape fired rather than listing which
        // markers were on screen and leaving the reader to infer the rest.
        // A live turn carries several running shapes at once; the log names
        // whichever the table ranks first, and every one of them is present.
        let running = "\
● Sure, let me look at that.\n\
✶ Working… (4s · ↓ 88 tokens)\n\
  esc to interrupt\n";
        for rule in ["active_spinner", "live_token_counter", "interrupt_hint"] {
            assert!(claude_rule_matches(rule, running), "{rule}");
        }
        assert_eq!(detect_claude(running, "", None), Status::Running);

        let parked = "\
✻ Worked for 1m 52s\n\
❯\n\
  ? for shortcuts\n";
        assert_eq!(claude_rule(parked), "completed_turn");

        // Typed text does not change the verdict: the completion line above
        // the box is what says the turn is over, and the box's contents are
        // not evidence either way.
        let typed = "\
✻ Worked for 1m 52s\n\
❯ half-typed next prompt\n\
  ? for shortcuts\n";
        assert_eq!(claude_rule(typed), "completed_turn");

        // Nothing to go on: no rule fires and the default stands.
        assert_eq!(claude_rule("   \n  \n"), "no_rule");
        assert_eq!(claude_rule("plain prose only"), "no_rule");
    }

    #[test]
    fn test_stale_waiting_hook_claude_cleared_on_esc_cancel() {
        // Regression from #2937: Claude's PreToolUse writes `waiting` for
        // AskUserQuestion, but Esc-cancelling the question fires no PostToolUse
        // (the tool never completes), so the hook file sticks on `waiting`. Once
        // the selection UI is gone and the pane shows the interrupt banner with
        // no active-turn signal, the detector reads Idle and the stale wait
        // clears. Before the fix the Waiting hook was trusted as-is and left the
        // session stuck yellow. ANSI is preserved to exercise the strip path.
        let pane = "\x1b[1m> Tell me about the weather\x1b[0m\n\
● I'll pull that up.\n\n\
What should Claude do instead?\n❯\n  ? for shortcuts";
        assert_eq!(detect_claude(pane, "", stale_wait()), Status::Idle);
    }

    #[test]
    fn test_stale_waiting_hook_claude_cleared_at_ready_prompt() {
        // Same stale-`waiting` gap, cancel dropped straight back to the idle
        // ready prompt. The parked `❯` plus the idle footer, no running signal,
        // reads as Idle.
        let pane = "● Done for now.\n\n❯\n  ? for shortcuts";
        assert_eq!(detect_claude(pane, "", stale_wait()), Status::Idle);
    }

    #[test]
    fn test_stale_waiting_hook_claude_resumed_turn_reads_running() {
        // The user cancelled the question and Claude started generating again
        // before the poll: the live spinner means Running, not a stale wait.
        let pane = "✶ Working… (4s · ↓ 88 tokens)\n  esc to interrupt";
        assert_eq!(detect_claude(pane, "", stale_wait()), Status::Running);
    }

    #[test]
    fn test_stale_waiting_hook_claude_keeps_waiting_while_question_on_screen() {
        // The AskUserQuestion selection UI is still parked on the pane: the
        // detector re-reports Waiting, so the wait survives (answering a real
        // question is unaffected).
        let pane = "\x1b[1m  Which approach do you prefer?\x1b[0m\n\
❯ 1. First\n    2. Second\n\n\
  Enter to select · ↑/↓ to navigate · Esc to cancel";
        assert_eq!(detect_claude(pane, "", stale_wait()), Status::Waiting);
    }

    #[test]
    fn test_stale_waiting_hook_claude_keeps_waiting_while_approval_on_screen() {
        let pane = "\x1b[1m  Do you want to proceed?\x1b[0m\n\
  ❯ 1. Yes\n    2. No\n\n  Esc to cancel · Tab to amend";
        assert_eq!(detect_claude(pane, "", stale_wait()), Status::Waiting);
    }

    #[test]
    fn test_stale_waiting_hook_codex_cleared_and_kept() {
        // Codex writes `waiting` from PermissionRequest; Esc-denying it fires no
        // PostToolUse. Prompt gone -> detector reads Idle and clears; prompt
        // still up -> Waiting kept.
        assert_eq!(
            detect_via_manifest("codex", "file saved", "", stale_wait()),
            Status::Idle
        );
        assert_eq!(
            detect_via_manifest("codex", "approve changes?", "", stale_wait()),
            Status::Waiting
        );
    }

    #[test]
    fn test_stale_waiting_hook_cursor_cleared_and_kept() {
        // Cursor writes `waiting` from a permission_prompt Notification. After
        // cancel it parks at the follow-up prompt (Idle); while the approval is
        // up it stays Waiting.
        assert_eq!(
            detect_via_manifest("cursor", "→ add a follow-up", "", stale_wait()),
            Status::Idle
        );
        let prompt = "Run this command?\n\n> Allow this command\n  Deny\n\n\
enter to select · esc to cancel";
        assert_eq!(
            detect_via_manifest("cursor", prompt, "", stale_wait()),
            Status::Waiting
        );
    }

    #[test]
    fn test_stale_waiting_hook_qwen_cleared_and_kept() {
        // Qwen writes `waiting` from a permission_prompt Notification.
        assert_eq!(
            detect_via_manifest("qwen", "random output text", "", stale_wait()),
            Status::Idle
        );
        assert_eq!(
            detect_via_manifest("qwen", "Allow this tool to run?", "", stale_wait()),
            Status::Waiting
        );
    }

    #[test]
    fn test_stale_waiting_hook_gemini_cleared_and_kept() {
        // Gemini writes `waiting` from a ToolPermission Notification.
        assert_eq!(
            detect_via_manifest("gemini", "file saved", "", stale_wait()),
            Status::Idle
        );
        assert_eq!(
            detect_via_manifest("gemini", "approve changes?", "", stale_wait()),
            Status::Waiting
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
            detect_claude(pane, "", Some(hook_at(Status::Running, None))),
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
            detect_claude(pane, "", Some(hook_at(Status::Running, None))),
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(1))
                ))
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(300))
                ))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_claude_background_wait_only_counts_in_the_status_slot() {
        // Regression: unlike the spinner, the wait line stays in the transcript
        // after the agents finish, so a finished turn's copy scrolling in the
        // recent window read as a live running signal. That pinned the session
        // on Running through every path at once: the hookless detector, the
        // stale-`running` downgrade, and the `idle` hook write (upgraded back to
        // Running), leaving no recovery until the line scrolled away. Pane from
        // the hung session, whose turn ended ~10 minutes before the capture.
        let stale = "\
● Agent(Review PR #484)\n\
  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)\n\
✻ Waiting for 1 background agent to finish\n\
● The review came back clean. Summary of what it found:\n\
  PR #484 is green across all checks and ready for your call on merging.\n\
✻ Crunched for 10m 12s\n\
                                              new task? /clear to save 131.6k tokens\n\
──────────────────────────────\n\
❯ merge it\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · PR #484 · ← for agents";
        assert_eq!(detect_status_from_content(stale, "claude"), Status::Idle);
        assert_eq!(
            detect_claude(
                stale,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(300))
                ))
            ),
            Status::Idle
        );
        assert_eq!(
            detect_claude(stale, "", Some(hook_at(Status::Idle, None))),
            Status::Idle
        );
        // The live shape (wait line in the slot directly above the box) still
        // reads as working on the idle-hook path, the `Stop`-fires-while-agents-
        // run race `reconcile_claude_idle_hook_status` exists for.
        let live = "\
● Agent(Review PR #484)\n\
  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)\n\
✻ Waiting for 1 background agent to finish\n\
──────────────────────────────\n\
❯ merge it\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · PR #484 · ← for agents";
        assert_eq!(
            detect_claude(live, "", Some(hook_at(Status::Idle, None))),
            Status::Running
        );
        // A capture that caught no `❯` line (mid-redraw, or a window too short
        // to reach the box) has no anchor, so the slot is the last transcript
        // line in the window. The footers below the box have to read as chrome
        // for that to find the wait line, in every mode: manual mode's footer
        // carries neither `shift+tab to cycle` nor a `CLAUDE_MODE_FOOTER_MODES`
        // name, so it needs its own arm.
        for footer in [
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
            "  ⏸ manual mode on · ? for shortcuts · ← for agents",
        ] {
            let no_prompt_line = format!(
                "● Agent(Review PR #484)\n\
✻ Waiting for 1 background agent to finish\n\
──────────────────────────────\n\
{footer}"
            );
            assert_eq!(
                detect_claude(&no_prompt_line, "", Some(hook_at(Status::Idle, None))),
                Status::Running,
                "footer: {footer}"
            );
        }
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
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
            detect_claude(
                pane,
                "",
                Some(hook_at(Status::Running, Some(claude_fresh_bound())))
            ),
            Status::Idle
        );
        assert_eq!(
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(claude_fresh_bound() - std::time::Duration::from_secs(1))
                ))
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
        assert!(claude_rule_matches(
            "background_agent_wait",
            "✻ Waiting for 1 background agent to finish"
        ));
        assert!(claude_rule_matches(
            "background_agent_wait",
            "✶ Waiting for 2 background agents to finish"
        ));
        assert!(claude_rule_matches(
            "background_agent_wait",
            "  · Waiting for 12 background agents to finish"
        ));
        // No spinner frame char.
        assert!(!claude_rule_matches(
            "background_agent_wait",
            "Waiting for 1 background agent to finish"
        ));
        // Prose: no digit count.
        assert!(!claude_rule_matches(
            "background_agent_wait",
            "● Waiting for background agent results"
        ));
        // Prose: trailing words after "to finish" break the exact tail.
        assert!(!claude_rule_matches(
            "background_agent_wait",
            "* Waiting for 2 background agents to finish before merging"
        ));
        assert!(!claude_rule_matches("background_agent_wait", ""));
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
            ),
            Status::Idle
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_running_with_typed_text_while_streaming() {
        // Captured from Claude Code 2.1.212 mid-turn with unsubmitted text in
        // the input box: typing repurposes Esc to "clear input" so the footer
        // drops `esc to interrupt`, and prose streaming renders no spinner
        // line, leaving zero running signals while the agent works. The
        // mode-cycle footer alone must not read as parked here; the stale
        // `running` write has to survive.
        let pane = "\
  signals onto a single channel. Applied to terminals, the idea was seductive: what if a\n\
  single physical terminal could host several independent logical sessions, each behaving\n\
  as though it had the machine to itself?\n\
──────────────────────────────\n\
❯ this is some unsubmitted text i am typing while the agent works\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_idle_with_typed_text_after_turn_end() {
        // The parked variant of the typed-text pane (also captured from
        // 2.1.212): identical footer and prompt line, but the past-tense
        // completion line above the input box is positive parked evidence, so
        // the stale `running` write still recovers to Idle.
        let pane = "\
✻ Cooked for 49s\n\
──────────────────────────────\n\
❯ this is some unsubmitted text i am typing while the agent works\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
            ),
            Status::Idle
        );
    }

    #[test]
    fn test_claude_completed_turn_rule() {
        assert!(claude_rule_matches("completed_turn", "✻ Cooked for 49s"));
        assert!(claude_rule_matches(
            "completed_turn",
            "✻ Baked for 10s · 1 shell still running"
        ));
        assert!(claude_rule_matches("completed_turn", "✻ Worked for 1m 52s"));
        // Active spinner: ellipsis on the verb.
        assert!(!claude_rule_matches(
            "completed_turn",
            "· Undulating… (14s · ↓ 144 tokens)"
        ));
        // Background-agent wait shares the `for <digit>` skeleton but means
        // the session is still working.
        assert!(!claude_rule_matches(
            "completed_turn",
            "✻ Waiting for 1 background agent to finish"
        ));
        // No spinner frame char.
        assert!(!claude_rule_matches("completed_turn", "Worked for 1m 52s"));
        assert!(!claude_rule_matches("completed_turn", ""));
        // Rendered markdown bullets in streamed prose (`*` is a spinner frame
        // char) must not read as parked evidence: the `for` tail needs a
        // digits+unit duration, not a bare count or an ordinary word.
        assert!(!claude_rule_matches(
            "completed_turn",
            "* Thanks for 2 examples"
        ));
        assert!(!claude_rule_matches(
            "completed_turn",
            "* Tested for 3 edge cases in the parser"
        ));
        assert!(!claude_rule_matches(
            "completed_turn",
            "● Asked for permission twice"
        ));
    }

    #[test]
    fn test_claude_background_work_outlives_the_turn() {
        // A finished turn whose background work is still going: the REPL is
        // parked and the box is free, but shells, MCP tasks or agents the
        // agent started are still running. Captured from a live footer.
        let parked = "✻ Cooked for 1m 58s\n❯ \n";
        let footer =
            |tail: &str| format!("{parked}  ⏵⏵ auto mode on (shift+tab to cycle) · PR #3600{tail}");

        // The footer's shells segment is live: present while they run, gone
        // when the last one exits, so its absence is the parked case.
        let with_shells = footer(" · 5 shells · ← for agents");
        assert!(claude_rule_matches("background_shell", &with_shells));
        assert_eq!(detect_claude(&with_shells, "", None), Status::Running);

        let without = footer(" · ← for agents");
        assert!(!claude_rule_matches("background_shell", &without));
        assert_eq!(detect_claude(&without, "", None), Status::Idle);

        // MCP tasks outliving the turn that started them.
        let mcp = format!("{parked}✻ Ran 3 tools · 2 MCP tasks still running\n");
        assert!(claude_rule_matches("background_mcp_task", &mcp));
        assert_eq!(detect_claude(&mcp, "", None), Status::Running);

        // The same sentence quoted inside a permission prompt is not evidence
        // that anything is running.
        let quoted = format!("{mcp}Do you want to proceed?\n❯ 1. Yes\n  2. No\n");
        assert!(!claude_rule_matches("background_mcp_task", &quoted));
    }

    #[test]
    fn test_claude_stuck_running_pane_recovers() {
        // Captured from a session that had reported Running for two hours: a
        // finished turn, Claude's own update banner between the completion
        // line and the box, and unsent text in the box. The banner stood in
        // for the status slot, so the parked evidence was unreachable and a
        // `running` write nobody had refreshed since kept winning. Typed text
        // is replaced here; the shape is verbatim.
        let pane = "\
✻ Cooked for 1m 58s · done 7:17 PM\n\
                    ✔ Update installed · Restart to update\n\
────────────────────────────────────────────────────────────\n\
❯ a half-typed follow-up\n\
────────────────────────────────────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for a…";
        assert!(
            claude_rule_matches("completed_turn", pane),
            "the update banner must be skipped as chrome"
        );
        // A write younger than the fresh window still carries the pane: that
        // is a turn the user has just sent, whose spinner has not rendered.
        assert_eq!(
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(1))
                ))
            ),
            Status::Running
        );
        for age in [30, 120, 7200] {
            assert_eq!(
                detect_claude(
                    pane,
                    "",
                    Some(hook_at(
                        Status::Running,
                        Some(std::time::Duration::from_secs(age))
                    ))
                ),
                Status::Idle,
                "age {age}s"
            );
        }
        // The same pane once the user sends the next turn: the spinner line
        // replaces the completion line and the session reads Running again.
        let resumed = "\
✢ Precipitating… (11m 14s · ↓ 25.1k tokens)\n\
                    ✔ Update installed · Restart to update\n\
────────────────────────────────────────────────────────────\n\
❯ \n\
────────────────────────────────────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to …";
        assert_eq!(detect_claude(resumed, "", None), Status::Running);
    }

    #[test]
    fn test_claude_typed_prompt_is_not_evidence() {
        // Unsubmitted text in the input box used to be a state of its own:
        // typing suppresses the `esc to interrupt` hint and prose streams with
        // no spinner, so a working pane and a parked one differ only by the
        // completion line above the box. The box's contents are no longer
        // consulted at all; the line above it, the hook and the title decide,
        // and each of these fixtures has a determinate answer.
        let stale = Some(hook_at(
            Status::Running,
            Some(std::time::Duration::from_secs(120)),
        ));
        let box_ = "──────────────────────────────";

        // Prose above the box: nothing says the turn ended, so a standing
        // `running` write still carries it.
        let streaming =
            format!("  prose still being generated\n{box_}\n❯ half-typed next prompt\n{box_}");
        assert_eq!(detect_claude(&streaming, "", stale), Status::Running);
        // Hookless, the title is what carries it: a spinner frame there is
        // proof of a live turn no transcript shape can give.
        assert_eq!(
            detect_claude(&streaming, "⠹ Working", None),
            Status::Running
        );

        // Completion line above the box: parked, whatever the box holds.
        let parked = format!("✻ Cooked for 49s\n{box_}\n❯ half-typed next prompt\n{box_}");
        assert_eq!(detect_claude(&parked, "", stale), Status::Idle);

        // Esc-interrupt banner above the box: parked.
        let interrupted = "\
⎿  Interrupted · What should Claude do instead?\n\
❯ half-typed next prompt\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(detect_claude(interrupted, "", stale), Status::Idle);

        // An empty box is parked evidence in its own right.
        let bare = "  some prose\n❯ \n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(detect_claude(bare, "", stale), Status::Idle);

        // A numbered approval menu on the `❯` line is a blocking prompt.
        let menu = "\
Do you want to proceed?\n\
❯ 1. Yes\n\
  2. No\n\
  ⏸ plan mode on (shift+tab to cycle)";
        assert_eq!(detect_claude(menu, "", stale), Status::Waiting);

        // A live running signal outranks the parked shapes below it.
        let running =
            format!("✽ Crunching… (19s · ↓ 166 tokens)\n{box_}\n❯ half-typed next prompt\n{box_}");
        assert_eq!(detect_claude(&running, "", stale), Status::Running);
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
            ),
            Status::Waiting
        );
    }

    #[test]
    fn test_reconcile_claude_hook_status_stale_running_typed_prompt_over_completion_line() {
        // Regression for a session pinned on Running: the turn ended but no
        // idle hook fired, and the parked pane offered neither of the old
        // positive markers. Typed unsubmitted text defeats the bare-`❯`
        // marker, and this newer footer drops `(shift+tab to cycle)` (extra
        // segments take its place), so the stale `running` write was trusted
        // forever. The completion line directly above the typed prompt is
        // the parked evidence; pane captured verbatim from the hung session.
        let parked = "\
✻ Sautéed for 39s · 1 monitor still running\n\
──────────────────────────────\n\
❯ stop the monitor\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on · PR #444 · 1 monitor · ← for agents · ↓ to manage";
        // The same box over a still-streaming transcript stays ambiguous:
        // the footer alone must not downgrade a pre-typed working session.
        let streaming = "\
  prose still being generated by the model\n\
──────────────────────────────\n\
❯ stop the monitor\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on · PR #444 · 1 monitor · ← for agents · ↓ to manage";
        let cases = [(parked, Status::Idle), (streaming, Status::Running)];
        for (pane, expected) in cases {
            assert_eq!(
                detect_claude(
                    pane,
                    "",
                    Some(hook_at(
                        Status::Running,
                        Some(std::time::Duration::from_secs(120))
                    ))
                ),
                expected,
                "pane:\n{pane}"
            );
        }
    }

    #[test]
    fn test_reconcile_claude_hook_status_stale_running_typed_prompt_over_box_chrome() {
        // Regression: chrome between the transcript and the input box hid the
        // completion line from the parked-evidence walk-up, so the pane read
        // Ambiguous and the stale `running` write of a silent tool stop was
        // trusted forever once the user pre-typed the next prompt. Both panes
        // captured verbatim from hung sessions; the first carries the `new
        // task?` context hint, the second a labeled top separator.
        let clear_hint = "\
  PR #484 is green across all checks and ready for your call on merging.\n\
✻ Crunched for 10m 12s\n\
                                              new task? /clear to save 131.6k tokens\n\
──────────────────────────────\n\
❯ merge it\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · PR #484 · ← for agents";
        let labeled_separator = "\
✻ Worked for 43s\n\
─────────────────────── rebrand-chord-charts-primary ──\n\
❯ merge it and confirm the deploy\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents";
        // Same chrome over a still-streaming transcript stays ambiguous: the
        // skip must not invent parked evidence where there is none.
        let streaming = "\
  prose still being generated by the model\n\
                                              new task? /clear to save 131.6k tokens\n\
─────────────────────── rebrand-chord-charts-primary ──\n\
❯ merge it\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · PR #484 · ← for agents";
        let cases = [
            (clear_hint, Status::Idle),
            (labeled_separator, Status::Idle),
            (streaming, Status::Running),
        ];
        for (pane, expected) in cases {
            assert_eq!(
                detect_claude(
                    pane,
                    "",
                    Some(hook_at(
                        Status::Running,
                        Some(std::time::Duration::from_secs(120))
                    ))
                ),
                expected,
                "pane:\n{pane}"
            );
        }
    }

    #[test]
    fn test_claude_mode_footer_is_chrome_not_evidence() {
        // Parked footers captured from 2.1.211 by cycling shift+tab, plus the
        // newer variant that drops the shift+tab suffix for extra segments.
        // Each pane carries ghost suggestion text in the box, so the only
        // parked evidence is the completion line, and it is only reachable if
        // the footer between them is skipped as chrome. A stale `running`
        // write makes the difference visible: skipped, the pane reads Idle.
        let stale = Some(hook_at(
            Status::Running,
            Some(std::time::Duration::from_secs(120)),
        ));
        for footer in [
            "  ⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
            "  ⏸ plan mode on (shift+tab to cycle) · ← for agents",
            "  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
            "  ⏸ manual mode on · ? for shortcuts · ← for agents",
            "  ⏵⏵ bypass permissions on · PR #444 · 1 monitor · ← for agents · ↓ to manage",
        ] {
            let pane = format!("✻ Churned for 10s\n❯ ghost suggestion text\n{footer}");
            assert!(
                claude_rule_matches("completed_turn", &pane),
                "footer must be skipped as chrome: {footer}"
            );
            assert_eq!(detect_claude(&pane, "", stale), Status::Idle, "{footer}");
        }

        // An echoed footer (a diff hunk, tool output) does not start with the
        // footer glyph, so it is transcript rather than chrome and it hides
        // the completion line behind it. The stale write then stands.
        let echoed = "\
✻ Churned for 10s\n\
+  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n\
❯ ghost suggestion text";
        assert!(!claude_rule_matches("completed_turn", echoed));
        assert_eq!(detect_claude(echoed, "", stale), Status::Running);

        // The running footer variant carries the interrupt hint, which
        // outranks the completion line above it.
        let running = "\
✻ Churned for 10s\n\
❯ ghost suggestion text\n\
  ⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt · ← for agents";
        assert_eq!(detect_claude(running, "", stale), Status::Running);
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
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
            detect_claude(
                pane,
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
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
            detect_claude(
                "   \n\n  ",
                "",
                Some(hook_at(
                    Status::Running,
                    Some(std::time::Duration::from_secs(120))
                ))
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

        // Vertical text (Vibe's Textual TUI renders one char per line). No
        // spinner and no trailing ellipsis, so the activity word alone has to
        // carry it.
        let vertical = "R\nu\nn\nn\ni\nn\ng";
        assert_eq!(detect_vibe_status(vertical), Status::Running);
        assert!(vibe_rule_matches("activity_word", vertical));
        assert!(!vibe_rule_matches("spinner", vertical));
        assert!(!vibe_rule_matches("trailing_ellipsis", vertical));

        // Only stacked runs are glued: two ordinary lines that happen to meet
        // mid-word stay separate, so they cannot spell an activity word
        // neither of them contains.
        let glued = "finished a long run\nning total of 3 files";
        assert_eq!(detect_vibe_status(glued), Status::Idle);
        assert!(!vibe_rule_matches("activity_word", glued));

        // A blank row ends a run too: the window drops blank lines, so without
        // this the halves of two separate blocks would stack into one word.
        let split = "R\nu\nn\n\nn\ni\nn\ng";
        assert_eq!(detect_vibe_status(split), Status::Idle);
        assert!(!vibe_rule_matches("activity_word", split));

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

        // A middle dot is separator punctuation in parked output; Vibe's
        // Textual spinner is braille and draws no other frame.
        assert_eq!(detect_vibe_status("main · 3 files changed"), Status::Idle);
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_reconcile_codex_hook_status_does_not_use_generic_pane_states() {
        assert_eq!(
            detect_via_manifest(
                "codex",
                "run this command? (y/n)",
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
            Status::Running
        );
        assert_eq!(
            detect_via_manifest(
                "codex",
                "› Write tests for @filename",
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
            Status::Running
        );
        assert_eq!(
            detect_via_manifest(
                "codex",
                "file saved",
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Waiting, Some(std::time::Duration::ZERO)))
            ),
            Status::Waiting
        );
        // A live radio prompt on screen now outranks an `idle` write, where
        // the reconciler this replaces only ever looked at the pane for a
        // `running` one. Codex's idle writers are not ordered against its
        // prompt events any more than Claude's are, so the prompt is the
        // better evidence.
        assert_eq!(
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Idle, Some(std::time::Duration::ZERO)))
            ),
            Status::Waiting
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
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
            detect_via_manifest(
                "codex",
                pane,
                "",
                Some(hook_at(Status::Running, Some(std::time::Duration::ZERO)))
            ),
            Status::Running
        );
    }

    #[test]
    fn test_hook_only_agents_report_idle_from_the_pane() {
        // Kiro, settl, Kimi and Prime Agent are hook-detected; their pane
        // fallback is a constant, so what is worth pinning is the registry
        // wiring: each must route to it rather than to a real detector.
        assert_eq!(detect_hook_only_status("anything"), Status::Idle);
        assert_eq!(detect_hook_only_status(""), Status::Idle);
        for agent in ["kiro", "settl", "kimi", "prime-agent"] {
            let Some(def) = crate::agents::get_agent(agent) else {
                continue;
            };
            assert_eq!(
                (def.detect_status)("\u{2736} Working\u{2026} (4s \u{b7} \u{2193} 88 tokens)"),
                Status::Idle,
                "{agent} must not parse the pane"
            );
        }
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

    /// Pi's live running frame: a braille spinner + `Working...` line sits just
    /// above the input box (two `────` rules), with the `%/Nk (auto)` status
    /// line at the very bottom. Captured from pi 0.82 driving a real turn.
    const PI_RUNNING_PANE: &str = "\
Twelve is a dozen.\n\
⠏ Working...\n\
────────────────────────────────────────\n\
────────────────────────────────────────\n\
/tmp\n\
0.0%/272k (auto)                    gpt-5.5 • medium\n";

    /// Pi parked after finishing a turn whose response prose contains the word
    /// "working" (an agent narrating "now working on #443"). Pi renders no `>`
    /// prompt at rest, so the old activity-word substring scan over the last 30
    /// lines matched "working" and pinned the session on Running forever.
    /// Captured shape from pi 0.82 idle footer.
    const PI_FINISHED_PANE_WITH_ACTIVITY_PROSE: &str = "\
I'll launch an aoe session to fix #443.\n\
The agent is now working on #443, extending the SSRF gate to the write path.\n\
You can monitor progress with aoe session logs.\n\
────────────────────────────────────────\n\
\n\
────────────────────────────────────────\n\
/Users/nbrake/scm/otari-workspace/otari-worktrees/orchestrator\n\
↑45k ↓11k $0.009 9.6%/500k (auto)                    gpt-5.5 • medium\n";

    /// omo (a pi derivative aliased via `agent_detect_as = pi`) renders a
    /// taller footer than plain pi: two tip lines, the input box (rule,
    /// prompt, rule), a usage line, and a persistent harness status line.
    /// Its busy line (`• Running eval ... esc to interrupt`) lands at
    /// position 8 above the bottom: three lines above the box's topmost
    /// rule, caught by the input-box hint anchor.
    /// Captured shape from #3475's live pane, ANSI stripped, with one
    /// neutral transcript line of scrollback above it.
    const OMO_DEEP_FOOTER_BUSY_PANE: &str = "\
Eval suite streaming results to the report.\n\
• Running eval (3m 19s • esc to interrupt)\n\
Tip: Set thinkingBudgets in settings.json to choose which models think.\n\
↳ Want the full story on any tip? Ask about it in chat.\n\
────────────────────────────────────────\n\
❯\n\
────────────────────────────────────────\n\
~ • CH93.4% • $2.870 • 115K/1M (11.5%) (auto)      claude-opus-4-6:xhigh\n\
(😺 OmO Native) Pursuing goal (1m) mem:12k/200k\n";

    /// The same omo frame after the turn ends: the busy line is removed and
    /// nothing else on the pane carries a running signal. The scrollback
    /// prose deliberately carries, at position 8, an embedded spinner glyph
    /// and an activity-verb start, arming three traps: the row fails if the
    /// spinner scan or the activity-word scan ever extends above the box
    /// top, and it fails just the same if `PI_FOOTER_WINDOW` widens far
    /// enough to reach the prose, instead of silently pinning idle
    /// derivative sessions on Running.
    const OMO_DEEP_FOOTER_PARKED_PANE: &str = "\
Working through the eval matrix, results streaming to the report ⠋\n\
Tip: Set thinkingBudgets in settings.json to choose which models think.\n\
↳ Want the full story on any tip? Ask about it in chat.\n\
────────────────────────────────────────\n\
❯\n\
────────────────────────────────────────\n\
~ • CH93.4% • $2.870 • 115K/1M (11.5%) (auto)      claude-opus-4-6:xhigh\n\
(😺 OmO Native) Pursuing goal (1m) mem:12k/200k\n";

    /// A finished turn whose response renders two markdown horizontal rules
    /// (pi draws them with the same `────` glyph run as its input box) while
    /// the input box itself is off-capture: startup, a full-screen pager, or
    /// a derivative that hides the box while streaming. The rule anchor then
    /// lands on prose at position 7, so without the shallow-anchor guard the
    /// hint band floats up to positions 8 through 10 and the quoted hint at
    /// position 8 pins the session on Running with no depth cap.
    const PI_PROSE_RULES_WITHOUT_BOX_PANE: &str = "\
Two horizontal rules in this response, and the input box is off-capture.\n\
Here is the first section of the answer.\n\
You can press esc to interrupt at any time.\n\
────────────────────────────────────────\n\
Second section of the answer.\n\
More prose in the second section.\n\
Still more prose in the second section.\n\
────────────────────────────────────────\n\
Closing prose line.\n\
Final prose line.\n";

    #[test]
    fn test_detect_pi_status_running_spinner_footer() {
        assert_eq!(detect_pi_status(PI_RUNNING_PANE), Status::Running);
    }

    #[test]
    fn test_detect_pi_status_finished_with_activity_prose_is_not_running() {
        // Regression for the "stuck on Running" bug: a finished pi turn whose
        // response prose contains an activity word must NOT read as Running.
        assert_eq!(
            detect_pi_status(PI_FINISHED_PANE_WITH_ACTIVITY_PROSE),
            Status::Idle
        );
    }

    /// A synthetic pane holding `line` at non-empty position `depth`, with
    /// neutral filler lines below it.
    fn pane_with_line_at_depth(line: &str, depth: usize) -> String {
        let filler = "Footer filler line.\n".repeat(depth.saturating_sub(1));
        format!("{line}\n{filler}")
    }

    /// The same, ending in plain pi's four-line input box furniture (two
    /// rules, cwd line, status line) instead of bare fillers.
    fn boxed_pane_with_line_at_depth(line: &str, depth: usize) -> String {
        let mut lines = vec![line.to_string()];
        for _ in 0..depth.saturating_sub(5) {
            lines.push("Footer filler line.".to_string());
        }
        lines.push("────────────────────────────────────────".to_string());
        lines.push("────────────────────────────────────────".to_string());
        lines.push("/tmp/proj".to_string());
        lines.push("0.0%/272k (auto)      gpt-5.5 • medium".to_string());
        lines.join("\n")
    }

    #[test]
    fn test_detect_pi_status_window_bounds() {
        // Both scan knobs at one line of granularity; each row names its own
        // scope in `desc`, so a drift in either direction fails a row rather
        // than silently widening the Running signal. Footer rows pin
        // `PI_FOOTER_WINDOW`: a spinner at position 6 still reads Running,
        // activity prose at position 7 stays Idle, so drift to 5 or to 7
        // fails a row. Hint rows pin the input-box anchor (#3475): the omo
        // busy line three lines above the box's rule anchor reads Running,
        // while a finished response quoting the hint past that band stays
        // Idle. The position 7 row is the known-bad residual and is asserted
        // as Running on purpose: in a finished frame the busy line is gone,
        // so positions 5 through 7 are all prose, and narrowing the band to
        // close it drops the omo busy line. That is one line of prose
        // exposure against main's two, and the row is here so the tradeoff
        // is visible where the bounds are read.
        let quote_line = "You can press esc to interrupt at any time.";
        let cases = [
            (
                "footer: spinner at position 6, the last line it reaches",
                pane_with_line_at_depth("⠋ Working...", 6),
                Status::Running,
            ),
            (
                "footer: activity prose at position 7, past the footer",
                pane_with_line_at_depth("Working through the eval matrix.", 7),
                Status::Idle,
            ),
            (
                "hint: derivative busy line three lines above the box rule",
                OMO_DEEP_FOOTER_BUSY_PANE.to_string(),
                Status::Running,
            ),
            (
                "hint: parked frame without the busy line",
                OMO_DEEP_FOOTER_PARKED_PANE.to_string(),
                Status::Idle,
            ),
            (
                "hint: quoted hint at position 8, past the anchored band",
                boxed_pane_with_line_at_depth(quote_line, 8),
                Status::Idle,
            ),
            (
                "hint: quoted hint at position 10, past the anchored band",
                boxed_pane_with_line_at_depth(quote_line, 10),
                Status::Idle,
            ),
            (
                "hint: quoted hint at position 11, past the anchored band",
                boxed_pane_with_line_at_depth(quote_line, 11),
                Status::Idle,
            ),
            (
                "hint: quoted hint at position 7 is the accepted residual",
                boxed_pane_with_line_at_depth(quote_line, 7),
                Status::Running,
            ),
            (
                "hint: prose rules with the box off-capture stay bounded",
                PI_PROSE_RULES_WITHOUT_BOX_PANE.to_string(),
                Status::Idle,
            ),
            (
                "hint: bare hint line falls back to the footer when no box",
                "processing request\nesc to interrupt".to_string(),
                Status::Running,
            ),
        ];
        for (desc, pane, expected) in &cases {
            assert_eq!(detect_pi_status(pane), *expected, "{desc}");
        }
    }

    /// The two-line composer box omp renders at rest, shared by the
    /// fixture-based tests below.
    const MINIMAL_COMPOSER_BOX: &str = "╭── π  > GPT-5.6 Sol ─╮\n╰─                   ─╯";

    /// Archived repro for the "idle omp sessions render yellow forever"
    /// bug: tail of a live pane captured after returning to the session
    /// panel. omp parks every healthy frame on its always-visible composer
    /// box, so box-only frames are the at-rest shape, not a Waiting signal.
    const OMP_PARKED_AT_COMPOSER_REPRO: &str = "\
 ※ recap: Goal was a simple probe: replied OK and ran echo, which returned rca-probe-42 successfully.

╭── π  > ⬢ Ox Alpha · ◉ max > 🗑 …of-empires-dev/scratch/4d9eb39378df4f4e ▶───2%───────────────────┃──────────1M─◀ Reply with OK ──╮
╰─                                                                                                                                                                                      ─╯";

    #[test]
    fn test_detect_omp_status_idle_at_composer_box() {
        let cases = [
            ("bare box", MINIMAL_COMPOSER_BOX.to_string()),
            // Completed turn above the box (the pre-fix contract said
            // Waiting here, which painted every idle omp session yellow).
            ("turn finished", format!("OK\n{MINIMAL_COMPOSER_BOX}")),
            // Stale loader from the previous turn buried in scrollback.
            (
                "stale loader ignored",
                format!("⠋ Working… ⟦esc⟧\nCompleted response.\nAdditional output.\nOK\n{MINIMAL_COMPOSER_BOX}"),
            ),
            // A live loader pushed one line past the footer window becomes an
            // unwitnessed Idle candidate, so the poller waits for confirmation.
            (
                "loader pushed past footer",
                format!("⠋ Working… ⟦esc⟧\nOK\n{MINIMAL_COMPOSER_BOX}"),
            ),
            // Full archived repro snapshot (see the const doc).
            ("repro snapshot", OMP_PARKED_AT_COMPOSER_REPRO.to_string()),
        ];
        for (name, pane) in &cases {
            assert_eq!(detect_omp_status(pane), Status::Idle, "case: {name}");
        }

        let detection = crate::tmux::detect::detect("omp", MINIMAL_COMPOSER_BOX, "", None)
            .expect("omp manifest");
        assert_eq!(detection.status, Some(Status::Idle));
        assert!(!detection.visible);
    }

    #[test]
    fn test_detect_omp_status_idle_without_prompt() {
        // Empty and whitespace-only panes must stay Idle without panicking:
        // every window is empty and the unsignaled fallback applies.
        let panes = ["plain command output", "", " \n\t\n"];
        for pane in panes {
            assert_eq!(detect_omp_status(pane), Status::Idle, "case: {pane:?}");
        }
    }

    #[test]
    fn test_detect_omp_status_error_retry_table() {
        // #3377: omp's pane heuristic must stop reporting Idle for provider
        // errors and retries. Error comes from omp's pinned banner (matched by
        // its dismissal footer) or the terminal retry lines; retries read
        // Running via the countdown and the sub-agent labels. Positions are
        // 1-based from the bottom; the lowest signal wins.
        let prompt_box = MINIMAL_COMPOSER_BOX;
        let br = "─".repeat(24);
        let banner = |msg: &str| {
            format!(
                "{br}\n ✖ {msg}\n Dismissed when you send your next message.\n{br}\n{prompt_box}"
            )
        };
        let approval_panel = "\
╭─ Allow tool: bash ───────────────────────────────────────╮
│                                                          │
│ Command: echo approval-probe                             │
│                                                          │
│  ❯ Approve                                               │
│    Deny                                                  │
│                                                          │
│ up/down navigate  enter select  esc cancel               │
│                                                          │
╰──────────────────────────────────────────────────────────╯";
        let cases: &[(&str, String, Status)] = &[
            // US1: rate limit / provider errors -> Error (banner anchor).
            (
                "banner 429",
                banner("429 Too Many Requests (rate limited). Retry after 30s."),
                Status::Error,
            ),
            (
                "banner overloaded",
                banner("Provider returned error: overloaded"),
                Status::Error,
            ),
            ("banner rate limit", banner("Rate limit exceeded"), Status::Error),
            (
                "banner 503",
                banner("503 Service Unavailable"),
                Status::Error,
            ),
            ("banner 500", banner("500 Internal Server Error"), Status::Error),
            (
                "banner websocket",
                banner("websocket closed before response completion"),
                Status::Error,
            ),
            ("banner refused", banner("Connection refused"), Status::Error),
            (
                "banner fetch failed",
                banner("fetch failed: socket hang up"),
                Status::Error,
            ),
            ("banner timed out", banner("timed out after 30s"), Status::Error),
            ("banner terminated", banner("terminated by upstream"), Status::Error),
            ("banner retry delay", banner("retry delay exceeded"), Status::Error),
            // Out-of-corpus errors still pin via the banner anchor alone.
            (
                "banner content filter",
                banner("Output blocked by content filtering policy"),
                Status::Error,
            ),
            ("banner unknown", banner("Unknown error"), Status::Error),
            // Alternate glyph theme (default unicode theme uses U+2718).
            (
                "banner alt glyph",
                format!(
                    "{br}\n ✘ 429 Too Many Requests (rate limited). Retry after 30s.\n Dismissed when you send your next message.\n{br}\n{prompt_box}"
                ),
                Status::Error,
            ),
            // Terminal retry lines (live form, no banner on this path). The
            // budget-exhausted line is the attested terminal render; the
            // failed-after line is defensive (omp 17.3.4 routes it through
            // showPinnedError -> banner, covered by the anchor).
            (
                "terminal lines",
                format!(
                    " Error: Retry budget exhausted after 10 retries: Unable to connect. Is the computer able to access the url?\n Error: Retry failed after 10 attempts: Unable to connect. Is the computer able to access the url?\n{prompt_box}"
                ),
                Status::Error,
            ),
            // Banner with the retry-failed message (anchor is the signal).
            (
                "banner retry failed",
                format!(
                    "✖ Retry failed after 3 attempts: 429 Too Many Requests (rate limited).\n Dismissed when you send your next message.\n{prompt_box}"
                ),
                Status::Error,
            ),
            // Banner without the prompt box: the anchor alone suffices.
            (
                "banner no box",
                format!(
                    "{br}\n ✖ 429 Too Many Requests (rate limited). Retry after 30s.\n Dismissed when you send your next message.\n{br}"
                ),
                Status::Error,
            ),
            // Anchor at the window bound (pos 6) -> Error; past it (pos 7)
            // only the parked-composer fallback remains, which reads Idle.
            (
                "anchor pos 6 bound",
                format!(
                    " Dismissed when you send your next message.\n l1\n l2\n l3\n{prompt_box}"
                ),
                Status::Error,
            ),
            (
                "anchor pos 7 out",
                format!(
                    " Dismissed when you send your next message.\n l1\n l2\n l3\n l4\n{prompt_box}"
                ),
                Status::Idle,
            ),
            // US2: retry in progress -> Running.
            (
                "countdown",
                format!("⠋ Retrying (2/3) in 30s… (esc to cancel)\n{prompt_box}"),
                Status::Running,
            ),
            // No spinner frame and no esc glyph: isolates the countdown check.
            (
                "countdown no frame",
                format!("Retrying (2/3) in 30s…\n{prompt_box}"),
                Status::Running,
            ),
            // Countdown coexists with a pinned banner (preserved-turn retry).
            (
                "countdown with banner",
                format!(
                    "{br}\n ✖ 429 Too Many Requests (rate limited). Retry after 30s.\n Dismissed when you send your next message.\n{br}\n⠋ Retrying (2/3) in 30s… (esc to cancel)\n{prompt_box}"
                ),
                Status::Running,
            ),
            // Character wrap cutting between tokens is re-joined via (b).
            (
                "countdown wrapped",
                format!("⠋ Retrying (2/3)\nin 30s… (esc to cancel)\n{prompt_box}"),
                Status::Running,
            ),
            // Countdown at the window bound (pos 6).
            (
                "countdown pos 6 bound",
                format!(
                    "⠋ Retrying (2/3) in 30s… (esc to cancel)\n l1\n l2\n l3\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label now",
                format!(
                    "└─ retrying 2/3 now: 429 Too Many Requests (rate limited). Retry after 30s.\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label 5.0s",
                format!(
                    "retrying 2/3 in 5.0s: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label 1m5s",
                format!(
                    "retrying 2/3 in 1m5s: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label 500ms",
                format!(
                    "retrying 2/3 in 500ms: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Running,
            ),
            // Fractional ms: the retry jitter leaves a fractional delayMs.
            (
                "label 876.5ms",
                format!(
                    "retrying 2/3 in 876.5ms: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label 2m",
                format!(
                    "retrying 2/3 in 2m: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label 2h",
                format!(
                    "retrying 2/3 in 2h: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label 1h30m",
                format!(
                    "retrying 2/3 in 1h30m: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label 1d",
                format!(
                    "retrying 2/3 in 1d: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label 1d5h",
                format!(
                    "retrying 2/3 in 1d5h: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "rule repair attempt",
                format!("Attempt 2/3 · generating…\n{prompt_box}"),
                Status::Running,
            ),
            // Wrap cut between the countdown number and its unit (R8).
            (
                "countdown cut 30|s",
                format!("⠋ Retrying (2/3) in 30\ns… (esc to cancel)\n{prompt_box}"),
                Status::Running,
            ),
            // Wrap cut between the unit and the ellipsis.
            (
                "countdown cut s|ellipsis",
                format!("⠋ Retrying (2/3) in 30s\n… (esc to cancel)\n{prompt_box}"),
                Status::Running,
            ),
            // Tie at equal position: terminal lines outrank labels.
            (
                "tie terminal over label",
                format!(
                    "retrying 1/3 now: Error: Retry failed after 2 attempts.\n{prompt_box}"
                ),
                Status::Error,
            ),
            // US3: ordinary tool output never pins a healthy session. These
            // rows pre-fix expected Waiting only because the composer-box
            // fallback returned Waiting; they are pure Idle cases.
            (
                "curl timed out",
                format!(
                    "curl: (28) Operation timed out after 30000 milliseconds\n{prompt_box}"
                ),
                Status::Idle,
            ),
            (
                "ssh refused",
                format!(
                    "ssh: connect to host 10.0.0.1 port 22: Connection refused\n{prompt_box}"
                ),
                Status::Idle,
            ),
            (
                "terminated by user",
                format!("The agent was terminated by the user.\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "retry-after header",
                format!("Retry-After: 30\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "attempt prose",
                format!("I will attempt 2/3 of the cases\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "retrying prose",
                format!(
                    "The tool kept retrying 2/3 of the files before giving up.\n{prompt_box}"
                ),
                Status::Idle,
            ),
            (
                "retrying next batch",
                format!("I will be retrying 2/3 in the next batch\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "stop retrying intervals",
                format!("Stop retrying (2/3) in 5s intervals!\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "retry failed no prefix",
                format!(
                    "The tool reported retry failed after 3 attempts\n{prompt_box}"
                ),
                Status::Idle,
            ),
            (
                "retrying my tests",
                format!("I keep retrying 2/3 in my tests: still failing\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "sub agent gave up",
                format!(
                    "auto-retry gave up after 3 attempts: 429 Too Many Requests (rate limited).\n{prompt_box}"
                ),
                Status::Idle,
            ),
            // Accepted: prose indistinguishable from the real label render
            // (family R3, bounded) reads Running by design.
            (
                "label prose accepted",
                format!("I'm retrying 2/3 now: the API timed out.\n{prompt_box}"),
                Status::Running,
            ),
            (
                "label pos 12 bound",
                format!(
                    "retrying 2/3 now: 429 Too Many Requests (rate limited).\n f1\n f2\n f3\n f4\n f5\n f6\n f7\n f8\n f9\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "label pos 13 out",
                format!(
                    "retrying 2/3 now: 429 Too Many Requests (rate limited).\n f1\n f2\n f3\n f4\n f5\n f6\n f7\n f8\n f9\n f10\n{prompt_box}"
                ),
                Status::Idle,
            ),
            // Esc hints quoted in prose without a live activity frame must
            // not pin Running.
            (
                "ascii esc prose",
                format!("The keymap binds cancel to [esc]\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "maintenance esc prose",
                format!("Docs say: press esc (esc to cancel) during compaction\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "markdown working bullet",
                format!("- Working tree status is clean.\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "unicode markdown bullet",
                format!("• The interrupt key is [esc]\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "idle recap prefix",
                format!("※ Working… ⟦esc⟧\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "symbolic prose without hint",
                format!("◐ Working through the explanation\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "unindented symbolic prose pair",
                format!("✓ Done with step 3\nSee docs: press [esc]\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "symbolic prose pair across blank row",
                format!("→ Some heading\n\n The cancel key is [esc]\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "indented prose after completed sentence",
                format!("✓ Done with step 3.\n See docs: press [esc]\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "unicode quote border",
                format!("▏ quoted: press [esc]\n{prompt_box}"),
                Status::Idle,
            ),
            (
                "nerd markdown bullet",
                format!("\u{f111} The interrupt key is [esc]\n{prompt_box}"),
                Status::Idle,
            ),
            // Precedences: the lowest live signal wins. Approval fixtures
            // use omp's real bordered selector rather than synthetic text.
            (
                "answered approval above fresh banner",
                format!(
                    "{approval_panel}\n ✖ 429 Too Many Requests (rate limited).\n Dismissed when you send your next message.\n{prompt_box}"
                ),
                Status::Error,
            ),
            (
                "answered approval past filler above fresh banner",
                format!(
                    "{approval_panel}\n l1\n l2\n ✖ 429 Too Many Requests (rate limited).\n Dismissed when you send your next message.\n{prompt_box}"
                ),
                Status::Error,
            ),
            (
                "live approval below terminal line",
                format!(" Error: Retry budget exhausted after 10 retries: …\n{approval_panel}"),
                Status::Waiting,
            ),
            (
                "answered approval above banner border",
                format!(
                    "{approval_panel}\n ✖ 429 Too Many Requests (rate limited).\n Dismissed when you send your next message.\n{br}\n{prompt_box}"
                ),
                Status::Error,
            ),
            (
                "live countdown below answered approval",
                format!("{approval_panel}\n⠋ Retrying (2/3) in 30s… (esc to cancel)\n{prompt_box}"),
                Status::Running,
            ),
            (
                "live approval below stale countdown",
                format!("⠋ Retrying (2/3) in 30s… (esc to cancel)\n{approval_panel}"),
                Status::Waiting,
            ),
            (
                "live loader below answered approval",
                format!("{approval_panel}\n⠋ Working… ⟦esc⟧\n╭── π  > GPT-5.6 Sol ─╮\n╰─ deny that         ─╯"),
                Status::Running,
            ),
            (
                "anchor over label",
                format!(
                    "retrying 2/3 now: 429…\n ✖ 429 Too Many Requests (rate limited).\n Dismissed when you send your next message.\n{prompt_box}"
                ),
                Status::Error,
            ),
            (
                "live approval below label",
                format!("retrying 2/3 now: 429…\n{approval_panel}"),
                Status::Waiting,
            ),
            (
                "stale terminal lines out of window",
                format!(
                    " Error: Retry failed after 10 attempts: …\n OK\n Done.\n Next\n Final\n{prompt_box}"
                ),
                Status::Idle,
            ),
        ];
        for (name, pane, expected) in cases {
            assert_eq!(detect_omp_status(pane), *expected, "case: {name}");
        }
    }

    /// Verbatim tail of a live approval prompt (omp 18.0.3): the select panel
    /// replaces the composer and its blank padding rows carry `│` glyphs, so
    /// every row counts as non-empty and the `Allow tool:` title sits 10 rows
    /// above the pane bottom, outside any window that still sees Approve/Deny.
    const OMP_LIVE_APPROVAL_PANEL: &str = "\
⠸ Working… ⟦esc⟧
╭─ Allow tool: bash ───────────────────────────────────────╮
│                                                          │
│ Command: echo appr-probe-19                              │
│                                                          │
│  ❯ Approve                                               │
│    Deny                                                  │
│                                                          │
│ up/down navigate  enter select  esc cancel               │
│                                                          │
╰──────────────────────────────────────────────────────────╯";

    #[test]
    fn test_detect_omp_status_waiting_on_real_approval_panel() {
        // Same selector structure with normal, wrapped, or absent tool
        // details: option rows and the footer stay fixed near the bottom.
        let cases = [
            OMP_LIVE_APPROVAL_PANEL,
            "\
╭─ Allow tool: bash ───────────────────────────────────────╮
│                                                          │
│ Command: for f in $(find . -type f | head -400); do      │
│   echo $f; grep -R audit --include=*.rs $f; done         │
│   echo done-with-scan                                    │
│                                                          │
│  ❯ Approve                                               │
│    Deny                                                  │
│                                                          │
│ up/down navigate  enter select  esc cancel               │
│                                                          │
╰──────────────────────────────────────────────────────────╯",
            // Custom tools may omit detail rows; the same real selector
            // furniture remains, so title text is not a separate signal.
            "\
╭─ Allow tool: custom_tool ────────────────────────────────╮
│                                                          │
│  ❯ Approve                                               │
│    Deny                                                  │
│                                                          │
│ up/down navigate  enter select  esc cancel               │
│                                                          │
╰──────────────────────────────────────────────────────────╯",
        ];
        for (i, pane) in cases.iter().enumerate() {
            assert_eq!(detect_omp_status(pane), Status::Waiting, "case {i}");
        }
    }

    #[test]
    fn test_detect_omp_status_running_loaders() {
        // Same behavior and setup: every live loader has a preset activity or
        // configured frame plus a direct marker or indented wrapped continuation.
        let box_unicode = "╭── π ─╮\n╰─ ─╯";
        let box_ascii = "+-- pi ---+\n+- -------+";
        let answered_panel = "\
╭─ Allow tool: bash ───────────────────────────────────────╮
│                                                          │
│ Command: echo approval-probe                             │
│                                                          │
│  ❯ Approve                                               │
│    Deny                                                  │
│                                                          │
│ up/down navigate  enter select  esc cancel               │
│                                                          │
╰──────────────────────────────────────────────────────────╯";
        let cases = [
            ("unicode default", format!("⠋ Working… ⟦esc⟧\n{box_unicode}")),
            (
                "unicode intent",
                format!("⠴ Set permissions on audit bait path ⟦esc⟧\n{box_unicode}"),
            ),
            (
                "nerd intent",
                format!("⠹ Reading audit fixtures ⟨esc⟩\n{box_unicode}"),
            ),
            (
                "custom symbolic frame",
                format!("◐ Working… ⟦esc⟧\n{box_unicode}"),
            ),
            (
                "ascii intent",
                format!("/ Running requested echo probe [esc]\n{box_ascii}"),
            ),
            (
                "manual compaction",
                format!("⠼ Compacting context... (esc to cancel)\n{box_unicode}"),
            ),
            (
                "wrapped ascii ellipsis maintenance",
                format!("⠼ Compacting context...\n (esc to cancel)\n{box_unicode}"),
            ),
            (
                "auto compaction",
                format!("⠼ Auto-compacting context... (esc to cancel)\n{box_unicode}"),
            ),
            (
                "context maintenance",
                format!("⠋ Context overflow detected, Auto context-full maintenance… (esc to cancel)\n{box_unicode}"),
            ),
            (
                "auto handoff",
                format!("⠋ Response incomplete, Auto-handoff… (esc to cancel)\n{box_unicode}"),
            ),
            (
                "wrapped unicode intent",
                format!("⠹ Locating audit config files in parent tree\n ⟦esc⟧\n{box_unicode}"),
            ),
            (
                "wrapped custom symbolic frame",
                format!("◐ Locating audit config files in parent tree\n ⟦esc⟧\n{box_unicode}"),
            ),
            (
                "wrapped ascii intent",
                format!("/ Locating audit config files in parent tree\n [esc]\n{box_ascii}"),
            ),
            (
                "fresh loader below answered approval",
                format!("{answered_panel}\n⠋ Working… ⟦esc⟧\n{box_unicode}"),
            ),
        ];
        for (name, pane) in &cases {
            assert_eq!(detect_omp_status(pane), Status::Running, "case: {name}");
        }
    }

    #[test]
    fn test_detect_omp_status_running_on_active_brand() {
        let cases = [
            (
                "captured default band",
                "  ⎋ Working…\n ⠸ 1s  > ⬢ RCA Slow Turn > 🌳 …-rca ▶─13%─┃128K─\n╰─",
            ),
            (
                "captured bordered default band",
                "  ⎋ Waiting\n╭── ⠋ 16s  > ⬢ GPT-5.6-Terra · ◒ high > 📁 …4260 ▶─4%─┃272K───╮\n╰─                                                                      ─╯",
            ),
            (
                "bordered ascii band",
                "  esc Working...\n+-- - 1s > [M] RCA Slow Turn >-13%--:|128K--+\n+-------------------------------------------+",
            ),
            (
                "narrow unicode band",
                "  ⎋ Working…\n ⠧ 37s > ⬢ RCA Slow Turn ▶─13%─┃128K─\n╰─",
            ),
            (
                "nerd symbols",
                "  󱊷 Working…\n ⠹ 59s  host  model\n╰─",
            ),
            (
                "ascii symbols",
                "  esc Working...\n - 1m > model default\n+-",
            ),
            (
                "configured single-cell symbols",
                "  CANCEL Working…\n X 2h / model status\n╰─",
            ),
            (
                "configured interrupt and separator",
                "  CANCEL Frobnicate quux\n ⠋ 2s ▶ RCA Slow Turn ▶ branch\n╰─",
            ),
            (
                "separator none",
                "  ⎋ Working…\n ⠋ 3s ⬢ Model status\n╰─",
            ),
            (
                "pipe separator",
                "  esc Working...\n / 4s | Model status\n+-",
            ),
            (
                "wrapped working message",
                "  ⎋ Locating files in the parent tree\n continuation\n ⠋ 0s > model status\n╰─",
            ),
            (
                "timer-only narrow band",
                "  ⎋ Waiting\n╭── ⠋ 16s ─╮\n╰─",
            ),
            (
                "status preset without pi segment",
                "  ⎋ Working…",
            ),
            (
                "timer-only nerd band",
                "  󱊷 Working…\n ⠋ 0s ",
            ),
        ];
        for (name, pane) in cases {
            assert_eq!(detect_omp_status(pane), Status::Running, "case: {name}");
        }
    }

    #[test]
    fn test_detect_omp_status_active_brand_near_misses_idle() {
        let cases = [
            (
                "activity timer without interrupt row",
                "Completed response.\n⠸ 1s > historical timing\n╰─",
            ),
            (
                "indented prose is not an interrupt row",
                "  Completed response.\n⠸ 1s > historical timing\n╰─",
            ),
            (
                "interrupt row without activity timer",
                "⎋ Working…\nπ > idle status\n╰─",
            ),
            (
                "digitless timer",
                "⎋ Working…\n⠸ .s > model status\n╰─",
            ),
            (
                "multi-decimal timer",
                "⎋ Working…\n⠸ 1..2s > model status\n╰─",
            ),
            (
                "leading-zero timer",
                "⎋ Working…\n⠸ 01s > model status\n╰─",
            ),
            (
                "duration prose below interrupt row",
                "⎋ Working…\nThe probe took 1s > historical timing\n╰─",
            ),
            (
                "stale interrupt rows around completed output",
                "⎋ Working…\nDone. Wrote 3 files.\nesc Working...",
            ),
            (
                "duration prose with a single-cell prefix",
                "esc Working...\nx 30m saved per run",
            ),
            (
                "active band pushed above current composer",
                "⎋ Working…\n⠸ 1s > model status\nCompleted response.\n╭── π > idle ─╮\n╰─           ─╯",
            ),
            (
                "persistent elapsed segment",
                "⎋ Working…\nπ > RCA Slow Turn > ⏱ 5m\n╰─",
            ),
            (
                "clock-only first segment",
                "  ⎋ Working…\n\n❯\n ⏱ 5m · RCA Slow Turn",
            ),
            (
                "nerd clock-only first segment",
                "  󱊷 Working…\n\n❯\n  5m  RCA Slow Turn",
            ),
            (
                "ascii clock-only first segment",
                "  esc Working...\n\n>\n t: 5m > RCA Slow Turn",
            ),
            (
                "decorated nerd clock-only first segment",
                "  󱊷 Working…\n\n❯\n  5m  RCA Slow Turn",
            ),
            (
                "decorated unicode clock-only first segment",
                "  ⎋ Working…\n❯\n╭── ⏱ 5m ─╮\n╰─",
            ),
        ];
        for (name, pane) in cases {
            assert_eq!(detect_omp_status(pane), Status::Idle, "case: {name}");
        }
    }

    #[test]
    fn test_detect_omp_status_active_brand_uses_lowest_marker() {
        let band = "⎋ Working…\n⠸ 1s > model status";
        let approval = "│ ❯ Approve │\n│ Deny │\n│ up/down navigate  enter select  esc cancel │";
        let cases = [
            (
                "lower approval wins",
                format!("{band}\n{approval}"),
                Status::Waiting,
            ),
            (
                "lower terminal error wins",
                format!("{band}\nError: Retry budget exhausted after 10 retries"),
                Status::Error,
            ),
            (
                "lower active band wins",
                format!("{approval}\n{band}\n╰─"),
                Status::Running,
            ),
        ];
        for (name, pane, expected) in cases {
            assert_eq!(detect_omp_status(&pane), expected, "case: {name}");
        }
    }

    #[test]
    fn test_detect_omp_status_waiting_on_ask_dialog() {
        // The built-in ask tool swaps its dialog into the composer slot and
        // blocks the turn; the footer hint rows are the stable anchor.
        let cases = [
            // Single-select footer.
            "\
╭─ Ask ────────────────────────────────────────╮
│                                              │
│ Which database for the new service?          │
│                                              │
│  ❯ PostgreSQL                                │
│    SQLite                                    │
│    Other (type your own)                     │
│                                              │
│ Enter select · n note · ↑/↓ move · Esc       │
│                                              │
╰──────────────────────────────────────────────╯",
            // ASCII dialog footer.
            "\
| Space toggle · Enter next · ↑/↓ move · Esc   |
+----------------------------------------------+",
            // Nerd uses the same unicode box border as the unicode preset.
            "\
│ Enter submit · ↑/↓ scroll · Esc              │
╰──────────────────────────────────────────────╯",
            // Input-guard footer: shown while a composer draft exists.
            "\
│ Finish or clear the current prompt to answer · Esc cancel │
╰──────────────────────────────────────────────╯",
            // The composer remains visible below a blocked ask dialog.
            "\
╭─ Ask ────────────────────────────────────────╮
│ Enter select · n note · ↑/↓ move · Esc       │
╰──────────────────────────────────────────────╯
╭── π > draft ─────────────────────────────────╮
╰──────────────────────────────────────────────╯",
            "\
╭─ Ask ────────────────────────────────────────╮
│ Finish or clear the current prompt to answer · Esc cancel │
╰──────────────────────────────────────────────╯
╭── π > draft ─────────────────────────────────╮
╰──────────────────────────────────────────────╯",
        ];
        for (i, pane) in cases.iter().enumerate() {
            assert_eq!(detect_omp_status(pane), Status::Waiting, "case {i}");
        }
    }

    #[test]
    fn test_detect_omp_status_waiting_on_plan_review_overlay() {
        // Same overlay contract under each focus region: stable option labels
        // plus the live footer (tab regions, esc cancel).
        let cases = [
            (
                "actions focus (ascii)",
                "\
| Plan mode - next step                                                        |
| > Approve and execute                                                        |
|   Approve and compact context                                                |
|   Approve and keep context (~28k / 1m)                                       |
|   Refine plan                                                                |
|   Save and quit                                                              |
+------------------------------------------------------------------------------+
| ↑↓ select · ⏎ confirm · c copy · tab regions · Ctrl+G editor · esc cancel    |
+------------------------------------------------------------------------------+",
            ),
            (
                "toc focus (unicode)",
                "\
│ Plan mode - next step                                                        │
│   Approve and execute                                                        │
│   Approve and compact context                                                │
│   Approve and keep context (~28k / 1m)                                       │
│ ❯ Refine plan                                                                │
│   Save and quit                                                              │
├──────────────────────────────────────────────────────────────────────────────┤
│ ↑↓ section · ⏎ open · a annotate · d delete · u undo · tab regions · esc cancel │
╰──────────────────────────────────────────────────────────────────────────────╯",
            ),
            (
                "body focus (nerd)",
                "\
│ Plan mode - next step                                                        │
│   Approve and execute                                                        │
│   Approve and compact context                                                │
│   Approve and keep context (~28k / 1m)                                       │
│   Refine plan                                                                │
│ \u{f054} Save and quit                                                      │
├──────────────────────────────────────────────────────────────────────────────┤
│ ↑↓ scroll · ⇧ faster · pgup/pgdn · g/G ends · tab regions · esc cancel      │
╰──────────────────────────────────────────────────────────────────────────────╯",
            ),
        ];
        for (name, pane) in cases {
            assert_eq!(detect_omp_status(pane), Status::Waiting, "case: {name}");
        }
    }

    #[test]
    fn test_detect_omp_status_selector_hint_without_approval() {
        // The panel help row alone must not pin Waiting: generic selectors
        // render it without Approve/Deny options, and prose naming the plan
        // options must not trip the overlay arm either.
        let box_ = "╭── π ─╮\n╰─ ─╯";
        let cases = [
            // Quoted Plan Review labels/footer are not a live overlay.
            format!("Quoted UI:\nApprove and execute\nRefine plan\nSave and quit\ntab regions · esc cancel\n{box_}"),
            // Quoted ask instructions in an ordinary response are not a dialog.
            format!("The instructions said: Enter select · n note\n{box_}"),
            // Real composer top row carries a > status separator: it must not
            // become a Plan Review cursor when the draft names an option.
            "╭── π  > approve and execute the migration ─╮\n│ then refine plan wording                    │\n╰─                                           ─╯".to_string(),
            // Markdown blockquote with option prose is not a live overlay.
            format!("Options were:\n> Approve and execute\nor Refine plan\n{box_}"),
            // Answered overlay rows retained in scrollback have no live
            // overlay footer and must not pin Waiting over recent output.
            format!("| > Approve and execute |\n|   Refine plan |\n|   Save and quit |\nPlan approved.\nrunning step 1\ndone\n{box_}"),
            // Wrapped draft naming both plan options without overlay proof.
            format!("I approve and execute\nthen refine plan things\n{box_}"),
            format!("│ up/down navigate  enter select  esc cancel │\n{box_}"),
            // Panel help plus approval prose is not a real approval panel.
            format!("│ up/down navigate  enter select  esc cancel │\nI will approve or deny later\n{box_}"),
            format!("I would approve and execute refine plan steps\n{box_}"),
            // A footer phrase inside the live composer is draft text.
            "╭── π > GPT-5.6 Sol ─╮\n│ Enter select · n note while documenting the UI │\n│ second draft line │\n╰──────────────────╯"
                .to_string(),
            "│ Enter submit · ↑/↓ scroll · current prompt to answer │\n╭── \u{f0d57} > ─╮"
                .to_string(),
            // Ask-arm verbs without the dialog's exact footer phrasing.
            format!("press enter to select an option\n{box_}"),
        ];
        for pane in &cases {
            assert_eq!(detect_omp_status(pane), Status::Idle, "case: {pane:?}");
        }
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
}
