//! Pane status detection: configured status rules, then agent manifests.

use crate::session::Status;

use super::detect::{Detection, HookObservation};
use super::utils::strip_ansi;

/// Configured rules for `(profile, tool)` outrank the built-in detector.
pub fn detect_status_from_content_in(profile: &str, content: &str, tool: &str) -> Status {
    // capture-pane runs with -e, so colors would split plain substring matches.
    let clean = strip_ansi(content);
    if let Some(status) = super::status_rules::detect(profile, tool, &clean) {
        return status;
    }
    crate::agents::get_agent(tool)
        .map(|a| (a.detect_status)(&clean))
        .unwrap_or(Status::Idle)
}

/// Shared by the status poller and `aoe session capture` so they agree.
/// `rules_tool` keys configured rules; `agent` is the manifest identity, which
/// follows `agent_detect_as`. `clean` must be ANSI-stripped. `None` means no
/// manifest and no configured rule matched.
pub fn detect_with_rules(
    profile: &str,
    rules_tool: &str,
    agent: &str,
    clean: &str,
    osc_title: &str,
    hook: Option<HookObservation>,
) -> Option<Detection> {
    if let Some(status) = super::status_rules::detect(profile, rules_tool, clean) {
        return Some(Detection {
            status: Some(status),
            visible: true,
            rule: "configured_status_rule",
        });
    }
    super::detect::detect(agent, clean, osc_title, hook)
}

/// Run an agent's manifest over one capture, Idle when no rule matches.
pub fn detect_via_manifest(
    agent: &str,
    raw_content: &str,
    osc_title: &str,
    hook: Option<HookObservation>,
) -> Status {
    super::detect::detect(agent, &strip_ansi(raw_content), osc_title, hook)
        .and_then(|d| d.status)
        .unwrap_or(Status::Idle)
}

macro_rules! manifest_detectors {
    ($($name:ident => $agent:literal),* $(,)?) => {
        $(pub fn $name(raw_content: &str) -> Status {
            detect_via_manifest($agent, raw_content, "", None)
        })*
    };
}

// Stable `fn` pointers for the agent registry.
manifest_detectors! {
    detect_claude_status => "claude",
    detect_opencode_status => "opencode",
    detect_vibe_status => "vibe",
    detect_codex_status => "codex",
    detect_cursor_status => "cursor",
    detect_copilot_status => "copilot",
    detect_pi_status => "pi",
    detect_omp_status => "omp",
    detect_droid_status => "droid",
    detect_hermes_status => "hermes",
    detect_gemini_status => "gemini",
    detect_qwen_status => "qwen",
    detect_antigravity_status => "antigravity",
}

/// Agents whose status comes from hooks alone render no pane shape to parse.
pub fn detect_hook_only_status(_content: &str) -> Status {
    Status::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn claude_rule_matches(rule: &str, content: &str) -> bool {
        super::super::detect::rule_matches("claude", rule, &strip_ansi(content), "", None)
    }

    fn vibe_rule_matches(rule: &str, content: &str) -> bool {
        super::super::detect::rule_matches("vibe", rule, &strip_ansi(content), "", None)
    }

    fn stale_wait() -> Option<super::super::detect::HookObservation> {
        hook(Status::Waiting, secs(600))
    }

    fn claude_rule(content: &str) -> &'static str {
        super::super::detect::detect("claude", &strip_ansi(content), "", None)
            .expect("claude has a manifest")
            .rule
    }

    fn claude_fresh_bound() -> std::time::Duration {
        super::super::detect::rule_max_age("claude", "hook_running_fresh")
            .expect("hook_running_fresh declares a bound")
    }

    fn hook(
        status: Status,
        age: Option<std::time::Duration>,
    ) -> Option<super::super::detect::HookObservation> {
        Some(super::super::detect::HookObservation { status, age })
    }

    fn secs(n: u64) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(n))
    }

    fn assert_all(detect: fn(&str) -> Status, expected: Status, fixtures: &[&str]) {
        for (i, content) in fixtures.iter().enumerate() {
            assert_eq!(detect(content), expected, "fixture {i}: {content}");
        }
    }

    fn assert_hook_all(
        agent: &str,
        hook: Option<super::super::detect::HookObservation>,
        expected: Status,
        panes: &[&str],
    ) {
        for (i, pane) in panes.iter().enumerate() {
            assert_eq!(
                detect_via_manifest(agent, pane, "", hook),
                expected,
                "{agent} fixture {i}: {pane}"
            );
        }
    }

    #[test]
    fn pane_fixture_table() {
        assert_all(detect_pi_status, Status::Running, &[PI_RUNNING_PANE]);
        assert_all(
            detect_pi_status,
            Status::Idle,
            &[PI_FINISHED_PANE_WITH_ACTIVITY_PROSE],
        );
        assert_all(detect_omp_status, Status::Idle, &[
            // activity timer without interrupt row
                "Completed response.\n⠸ 1s > historical timing\n╰─",
            // indented prose is not an interrupt row
                "  Completed response.\n⠸ 1s > historical timing\n╰─",
            // interrupt row without activity timer
                "⎋ Working…\nπ > idle status\n╰─",
            // digitless timer
                "⎋ Working…\n⠸ .s > model status\n╰─",
            // multi-decimal timer
                "⎋ Working…\n⠸ 1..2s > model status\n╰─",
            // leading-zero timer
                "⎋ Working…\n⠸ 01s > model status\n╰─",
            // duration prose below interrupt row
                "⎋ Working…\nThe probe took 1s > historical timing\n╰─",
            // stale interrupt rows around completed output
                "⎋ Working…\nDone. Wrote 3 files.\nesc Working...",
            // duration prose with a single-cell prefix
                "esc Working...\nx 30m saved per run",
            // active band pushed above current composer
                "⎋ Working…\n⠸ 1s > model status\nCompleted response.\n╭── π > idle ─╮\n╰─           ─╯",
            // persistent elapsed segment
                "⎋ Working…\nπ > RCA Slow Turn > ⏱ 5m\n╰─",
            // clock-only first segment
                "  ⎋ Working…\n\n❯\n ⏱ 5m · RCA Slow Turn",
            // nerd clock-only first segment
                "  󱊷 Working…\n\n❯\n  5m  RCA Slow Turn",
            // ascii clock-only first segment
                "  esc Working...\n\n>\n t: 5m > RCA Slow Turn",
            // decorated nerd clock-only first segment
                "  󱊷 Working…\n\n❯\n  5m  RCA Slow Turn",
            // decorated unicode clock-only first segment
                "  ⎋ Working…\n❯\n╭── ⏱ 5m ─╮\n╰─",
            // stale band with parked pi footer
                "─ Continue Autonomous · ⏱ 2h4m ─\n❯\n───────────────────────────────────\n π · 🖥 host",
            // parked claude shape at prompt
                "❯\n───────────────────────────────────\n π · 🖥 host",
            // stale parked spinner with prose mentioning esc to cancel
                "Some tool output: press (esc to cancel) to abort\n\
                 ❯\n\
                 ───────────────────────────────────\n\
                  ⠏ 28s · 🖥 host",
        ]);
        // Selector hints without an approval panel are prose, not a prompt.
        let box_ = "╭── π ─╮\n╰─ ─╯";
        let selector_hints = [
            format!("Quoted UI:\nApprove and execute\nRefine plan\nSave and quit\ntab regions · esc cancel\n{box_}"),
            format!("The instructions said: Enter select · n note\n{box_}"),
            "╭── π  > approve and execute the migration ─╮\n│ then refine plan wording                    │\n╰─                                           ─╯".to_string(),
            format!("Options were:\n> Approve and execute\nor Refine plan\n{box_}"),
            format!("| > Approve and execute |\n|   Refine plan |\n|   Save and quit |\nPlan approved.\nrunning step 1\ndone\n{box_}"),
            format!("I approve and execute\nthen refine plan things\n{box_}"),
            format!("│ up/down navigate  enter select  esc cancel │\n{box_}"),
            format!("│ up/down navigate  enter select  esc cancel │\nI will approve or deny later\n{box_}"),
            format!("I would approve and execute refine plan steps\n{box_}"),
            "╭── π > GPT-5.6 Sol ─╮\n│ Enter select · n note while documenting the UI │\n│ second draft line │\n╰──────────────────╯"
                .to_string(),
            "│ Enter submit · ↑/↓ scroll · current prompt to answer │\n╭── \u{f0d57} > ─╮"
                .to_string(),
            format!("press enter to select an option\n{box_}"),
        ];
        assert_all(
            detect_omp_status,
            Status::Idle,
            &selector_hints
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        assert_all(
            detect_omp_status,
            Status::Waiting,
            &[
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
                "\
| Space toggle · Enter next · ↑/↓ move · Esc   |
+----------------------------------------------+",
                "\
│ Enter submit · ↑/↓ scroll · Esc              │
╰──────────────────────────────────────────────╯",
                "\
│ Finish or clear the current prompt to answer · Esc cancel │
╰──────────────────────────────────────────────╯",
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
            ],
        );
        assert_all(
            detect_claude_status,
            Status::Waiting,
            &[
                CLAUDE_FOLDER_TRUST_PROMPT,
                CLAUDE_FOLDER_TRUST_PROMPT_WRAPPED,
                CLAUDE_FOLDER_TRUST_PROMPT_NARROW,
            ],
        );
        // A trust prompt echoed during a live turn is not the dialog.
        let bodies = [
            " \u{276f} 1. Yes, I trust this folder\n   2. No, exit",
            "     \u{276f} 1. Yes, I trust this folder\n       2. No, exit",
            "  \u{276f} 1. Yes, I trust this folder\n    2. No, exit\n     test result: FAILED",
            " 1. Yes, I trust this folder is what you pick, and then\n 2. the session starts",
        ];
        let mut echoed = Vec::new();
        for body in bodies {
            let pane = format!(
                "\u{25cf} The first-run dialog reads:\n \
                 Quick safety check: Is this a project you created or one you trust?\n\
                 {body}\n \u{2736} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)\n   \
                 esc to interrupt\n"
            );
            echoed.push(pane);
        }
        assert_all(
            detect_claude_status,
            Status::Running,
            &echoed.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        assert_all(
            detect_cursor_status,
            Status::Running,
            &[
                "\
  Grepped \"legacy_engine\" in .

 ⠘⠣ Reading  6.66k tokens

  → Add a follow-up                                      ctrl+c to stop

  Composer 2.5 · 48.2%                                  Auto-run",
                "\
 ⠀⠞ Calling  23.62k tokens


  → Add a follow-up  ctrl+c to stop


  Composer 2.5 · 55.7% · 49 files edited  Auto-run
",
                "\
  Started processing the request.

  1 background task
  Composer 2.5 · 39.2% · 20 files edited  Auto-run
",
                "\
  ┌──────────────────────────────┐
  │ Editing src/app/submit/page.tsx
  └──────────────────────────────┘

 ⠘⠆ Editing  39.76k tokens",
            ],
        );
        assert_all(
            detect_cursor_status,
            Status::Idle,
            &[
                "\
  → Add a follow-up


  1 background task
  Composer 2.5 · 39.2% · 20 files edited  Auto-run
",
                "\
  Finished the requested changes.

  → Add a follow-up

  Composer 2.5 · 60.9% · 4 files edited                 Auto-run",
                "\
 ⠘⠆ Editing  39.76k tokens

  Updated src/app/submit/page.tsx

  → Add a follow-up

  Composer 2.5 · 56.1% · 26 files edited  Auto-run",
            ],
        );
        assert_all(
            detect_cursor_status,
            Status::Waiting,
            &["\
Run this command?

> Allow this command
  Deny

enter to select · esc to cancel"],
        );
        assert_all(
            detect_claude_status,
            Status::Idle,
            &[
                "",
                "Some output\n> ",
                "file saved successfully",
                "✻ Worked for 1m 52s",
                "● Cooked for 30s",
                "· Brewed for 2m 10s",
                "* foo…",
                "* Cooked an amazing dish today…",
                "· Some random response text ending with…",
                "\
\u{25cf} The detector asks: Is this a project you created or
 one you trust? That phrase is the third arm.
 1. the first arm
 2. the second arm
",
                "\
 Q: what is this
 a project you created or one you trust is one you can vouch for.
 1. yes
",
                "\
✻ Churned for 1m 40s\n\
❯ \n\
  ◯ general-purpose  Summarize tmux module pub fns    1m 14s · ↓ 40.4k tokens",
            ],
        );
        assert_all(
            detect_claude_status,
            Status::Running,
            &[
                "✶ Working…\n  esc to interrupt",
                "Generating...\nctrl+c to interrupt",
                "✶ Working… (4s · ↓ 88 tokens)",
                "● Cooking… (12s · ↓ 1234 tokens)",
                "✶ Working…",
                "✻ Herding…",
                "● Pondering…",
                "· Sautéing…",
                "● Working…",
                "\
● The fixture renders these options:
  ❯ 1. Static plugin (comparator stays core)
    2. True-worker extraction
  and then the footer line:
  ⎿ 2052   Enter to select · ↑/↓ to navigate · Esc to cancel

✶ Herding… (12s · ↓ 1234 tokens)
  esc to interrupt",
                "\
  The footer reads \"Enter to select · ↑/↓ to navigate\" while parked.

✶ Working… (4s · ↓ 88 tokens)
  esc to interrupt",
                "\
  Here is the plan:
  1. Read the config
  2. Patch the parser

✶ Working… (4s · ↓ 88 tokens)
  esc to interrupt",
                "\
\u{25cf} The detector asks: Is this a project you created or
 one you trust? That phrase is the third arm.
 1. the first arm
 \u{2736} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)
   esc to interrupt
",
                "\
\u{25cf} The prompt asks: Is this a project you created or one you trust?
 The highlighted option reads Yes, I trust this folder.
 1. the first arm
 2. the second arm
 \u{2736} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)
   esc to interrupt
",
                "\
 \u{25cf} the answer the user gives is Yes,
 I trust this folder more than the upstream mirror. Is this a project
 you created or one you trust? was the wording.
 1. unrelated
 \u{2736} Working\u{2026} (3s)
   esc to interrupt
",
                "\
  2812 \u{276f} 1. Yes, I trust this folder
  2813   2. No, exit
\u{25cf} That is the fixture. Is this a project you created or one you trust?
 1. an unrelated list item
 \u{2736} Working\u{2026} (4s)
   esc to interrupt
",
                "\
\u{25cf} Here is what the docs show:
> 1. Yes, I trust this folder
> 2. No, exit
\u{25cf} And the question was: Is this a project you created or one you trust?
 \u{273b} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)
   esc to interrupt
",
                "\
\u{25cf} That is the fixture. Is this a project you created or one you trust?
 1. an unrelated list item
  2812 \u{276f} 1. Yes, I trust this folder
  2813   2. No, exit
 \u{273b} Working\u{2026} (4s)
   esc to interrupt
",
                "\
\u{25cf} The plan:
 1. read the prompt, which asks: Is this a project you created or one you trust?
 the highlighted option is Yes, I trust this folder
 and then we proceed.
 \u{273b} Working\u{2026} (9s)
   esc to interrupt
",
                "\
✻ Waiting for 1 background agent to finish\n\
❯ \n\
  ◯ general-purpose  Summarize tmux module pub fns    19s · ↓ 36.4k tokens",
            ],
        );
        assert_all(
            detect_claude_status,
            Status::Waiting,
            &[
                "\
  Bash command

    SANDBOX=aoe-sandbox-ee1a86c7
    echo \"checking sandbox gitconfig\"

  Do you want to proceed?
  ❯ 1. Yes
    2. No

  Esc to cancel · Tab to amend

✶ Herding… (53s · ↓ 7.0k tokens)
  Tip: Use /bts to ask a quick side question without interrupting Claude's current work",
                "\
  Do you want to make this edit to src/main.rs?
  ❯ 1. Yes
    2. Yes, allow all edits during this session (shift+tab)
    3. No, and tell Claude what to do differently (esc)

✶ Cooking… (8s · ↓ 412 tokens)",
                "\
  Would you like to proceed?
  ❯ 1. Yes, and auto-accept edits
    2. Yes, and manually approve edits
    3. No, keep planning",
                "\
  PREMISE GATE (your call, not auto-decided).
  So which shape do you actually want?

  ❯ 1. Static plugin (comparator stays core)
    2. True-worker extraction (as first scoped)
    3. Don't extract; ship the valuable byproducts

  Enter to select · ↑/↓ to navigate · Esc to cancel",
                "\
  How should the encryption key be managed?

  ❯ 1. Require OTARI_SECRET_KEY
    2. Auto-generate KEK to a file
    3. Auto-generate KEK in DB

  Enter to select · ↑/↓ to navigate · n to add notes · Tab to switch questions · Esc to cancel",
            ],
        );
        assert_all(
            detect_opencode_status,
            Status::Running,
            &[
                "Processing your request\nesc to interrupt",
                "Working... esc interrupt",
                "Generating ⠋",
                "Loading ⠹",
            ],
        );
        assert_all(
            detect_opencode_status,
            Status::Waiting,
            &[
                "allow this action? [y/n]",
                "continue? (y/n)",
                "approve changes",
                "task complete.\n>",
                "ready for input\n> ",
                "done! what else can i help with?\n>",
                "Select:\n❯ 1. Option A\n  2. Option B",
                "Task complete! What else can I help with?\n>",
                "Ready\n>>",
            ],
        );
        assert_all(
            detect_opencode_status,
            Status::Idle,
            &["some random output", "file saved successfully"],
        );
        assert_all(
            detect_vibe_status,
            Status::Idle,
            &[
                "some random output",
                "file saved successfully",
                "Done!",
                "main · 3 files changed",
            ],
        );
        assert_all(
            detect_codex_status,
            Status::Running,
            &[
                "processing request\nesc to interrupt",
                "thinking about your request",
                "working on task",
                "generating ⠋",
                "⠋ thinking about your request",
                "• Working (4s • esc to interrupt)",
                r#"
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
"#,
                r#"
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
"#,
                r#"
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
"#,
                r#"
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
"#,
                r#"
  Question 1/1 (1 unanswered)
  Do you want apple, banana, orange, or something else?

  › 1. Apple (Recommended)  Pick apple for the default simple choice.
    2. Banana               Pick banana for a second common option.
    3. Orange               Pick orange for a citrus option.
    4. None of the above    Optionally, add details in notes (tab).

  tab to add notes | enter to submit answer | esc to interrupt

› Apple

• Working (4s • esc to interrupt)
"#,
            ],
        );
        assert_all(
            detect_codex_status,
            Status::Waiting,
            &[
                "run this command? (y/n)",
                "approve changes?",
                "execute this action? [y/n]",
                r#"
■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.

› Try again

run this command? (y/n)
"#,
                "\
  Question 1/1 (1 unanswered)
  Which fruit do you want?

  › 1. Banana (Recommended)  Choose banana.
    2. Orange                Choose orange.
    3. Apple                 Choose apple.
    4. None of the above     Optionally, add details in notes (tab).

  tab to add notes | enter to submit answer | esc to interrupt
",
                "\
  › 1. Yes
    2. No
    3. Maybe
",
            ],
        );
        assert_all(
            detect_codex_status,
            Status::Idle,
            &[
                "file saved",
                "random output text",
                "based on your working example, aliases are safest",
                "braille spinner characters like ⠋, ⠙, etc.",
                "• I found the shared API base and the routing map",
                "• Starting MCP servers can take a while",
                "• Running command examples can be misleading",
                "ready\ncodex>",
                "done\n>",
                "› Find and fix a bug in @filename",
                "› Run /review on my current changes",
                r#"
• Fixed and staged src/tui/home/render.rs:695. The margin span now uses Span::raw(" "), avoiding clippy::repeat_once.

  Verification passed: cargo clippy --lib -- -D warnings.


› Find and fix a bug in @filename

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#,
                r#"
• You picked: Banana.


› Run /review on my current changes

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#,
                r#"
  If your API supports an array/operator filter like value_in, then this could be shorter,
  but based on your working example, aliases are the safest GraphQL-native way to query all of them in one request.


› asdasd


■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.


› dasdasd

  gpt-5.5 medium · ~/tomatom/connector-plus-shopty/shopty
"#,
                r#"
■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.

› Try again

• No action taken.

› What next?
"#,
                r#"
  Note: git status still shows MM src/tmux/status_detection.rs, meaning earlier staged changes exist and this latest fix is
  unstaged on top.

• Working (4s • esc to interrupt)

─ Worked for 1m 22s ───────────────────────────────────────────────────────────────────────────────────────────────────────────


› asd


• No action taken.

  gpt-5.5 high · ~/appsSource/agent-of-empires
"#,
                r#"
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
"#,
            ],
        );
        assert_all(
            detect_gemini_status,
            Status::Running,
            &[
                "processing request\nesc to interrupt",
                "generating ⠋",
                "working ⠹",
            ],
        );
        assert_all(
            detect_gemini_status,
            Status::Waiting,
            &[
                "run this command? (y/n)",
                "approve changes?",
                "execute this action? [y/n]",
                "ready\n>",
            ],
        );
        assert_all(
            detect_gemini_status,
            Status::Idle,
            &["file saved", "random output text"],
        );
        assert_all(
            detect_copilot_status,
            Status::Running,
            &[
                "processing request\nesc to interrupt",
                "Thinking about your request",
                "working ⠋",
                "loading ⠹",
                "┃\n◎ Working esc cancel    MAI-Code-1-Flash",
            ],
        );
        assert_all(
            detect_copilot_status,
            Status::Waiting,
            &[
                "run command? (y/n)",
                "Allow this tool to run?",
                "pick an option\nenter to select",
                "done\n>",
                "done\ncopilot>",
                "answer text\n┃\n/ commands · ? help · tab next tab",
                "> summarize the readme\n\
                    ◎ Working esc cancel    MAI-Code-1-Flash\n\
                    Here is the summary. ⠋\n\
                    It covers setup and usage.\n\
                    More detail follows here.\n\
                    ┃\n\
                    / commands · ? help · tab next tab",
                "> summarize the readme\n\
                           ◎ Working esc cancel\n\
                           Here is the summary.\n\
                           It covers setup and usage.\n\
                           More detail follows here.\n\
                           >",
            ],
        );
        assert_all(
            detect_copilot_status,
            Status::Idle,
            &[
                "file saved",
                "random output text",
                "need more? help is available; use tab next tab to switch",
            ],
        );
        assert_all(
            detect_pi_status,
            Status::Running,
            &[
                "generating ⠋",
                "loading ⠹",
                "processing request\nesc to interrupt",
                "thinking about code",
                "reading file.ts",
            ],
        );
        assert_all(
            detect_pi_status,
            Status::Waiting,
            &[
                "done\n>",
                "ready\n> ",
                "complete\npi>",
                "reading config.toml\nDone.\n>",
            ],
        );
        assert_all(
            detect_pi_status,
            Status::Idle,
            &["file saved", "random output text"],
        );
        assert_all(
            detect_droid_status,
            Status::Running,
            &[
                "processing request\nesc to interrupt",
                "thinking about your request",
                "working on task",
                "executing command",
                "generating ⠋",
            ],
        );
        assert_all(
            detect_droid_status,
            Status::Waiting,
            &[
                "run this command? (y/n)",
                "approve changes?",
                "execute this action? [y/n]",
                "ready\ndroid>",
                "done\n>",
            ],
        );
        assert_all(
            detect_droid_status,
            Status::Idle,
            &["file saved", "random output text"],
        );
        assert_all(
            detect_hermes_status,
            Status::Running,
            &[
                "◜ (｡•́︿•̀｡) pondering... (1.2s)",
                "◠ (⊙_⊙) contemplating... (2.4s)",
                "✧٩(ˊᗜˋ*)و✧ got it! (3.1s)",
                "┊ 💻 terminal 'ls -la' (0.3s)",
                "┊ 🔍 web_search (1.2s)",
                "reasoning…",
                "pondering the question",
                "analyzing the codebase",
                "computing result",
                "┊ some response\n❯ Ctrl+C to interrupt…",
                "─ (¬‿¬) reasoning…\n❯ Ctrl+C to interrupt…",
            ],
        );
        assert_all(
            detect_hermes_status,
            Status::Idle,
            &[
                "some output\n❯",
                "some output\n❯ ",
                "some output\n⚡",
                "pondering the question\ntask complete\n❯",
                "anything",
                "",
                "task completed successfully",
            ],
        );
        assert_all(
            detect_qwen_status,
            Status::Running,
            &[
                "processing request\nesc to interrupt",
                "⠋ Thinking about your request",
                "working ⠋",
                "loading ⠹",
                "⠹ Generating code\nesc to interrupt",
                "⠧ Reading file.rs",
            ],
        );
        assert_all(
            detect_qwen_status,
            Status::Waiting,
            &[
                "run command? (y/n)",
                "Allow this tool to run?",
                "pick an option\nenter to select",
                "done\n>",
                "done\nqwen>",
                "Select:\n❯ 1. Option A\n  2. Option B",
                "Select Authentication Method\n› 1. Alibaba ModelStudio",
            ],
        );
        assert_all(
            detect_qwen_status,
            Status::Idle,
            &["file saved", "random output text"],
        );
        assert_all(
            detect_antigravity_status,
            Status::Waiting,
            &[
                "\
     ▄▀▀▄
    ▀▀▀▀▀▀

 Welcome to the Antigravity CLI. You are currently not signed in.

 ⣻  Signing in...",
                "\
Accessing workspace:

/tmp/aoe-agy-smoke-proj

Do you trust the contents of this project?

Antigravity CLI requires permission to read, edit, and execute files here.

> Yes, I trust this folder
  No, exit

  ↑/↓ Navigate · enter Confirm
                                                         Gemini 3.5 Flash (High)",
                "run command? (y/n)",
                "\
read_file
path: /workspace/secrets.env

⚠ Approval Required

> Yes, just this once
  Yes, allow always
  No, deny access",
                "I'll read that file now.\n awaiting user approval.",
            ],
        );
        assert_all(
            detect_antigravity_status,
            Status::Running,
            &[
                "processing request\nesc to interrupt",
                "⠋ Thinking about your request",
                "\
  Applying patch to src/session/instance.rs

  → Add a follow-up                                      ctrl+c to stop",
                "\
  Generated summary for the previous step.

  Editing src/session/instance.rs",
            ],
        );
        assert_all(
            detect_antigravity_status,
            Status::Idle,
            &[
                "file saved",
                "random output text",
                "Running tests completed successfully.",
                "Reading config.toml finished.",
                "Editing src/session/instance.rs done.",
                "Testing finished with success.",
            ],
        );
        let completed = [
            "Running tests completed successfully.",
            "Reading config.toml finished.",
            "Editing src/app.rs done.",
            "Testing finished with success.",
        ];
        for tail in ["\n\n→ Add a follow-up", "\n  Composer 2.5"] {
            for phrase in completed {
                assert_eq!(
                    detect_cursor_status(&format!("{phrase}{tail}")),
                    Status::Idle,
                    "{phrase}{tail}"
                );
            }
        }
        assert_all(
            detect_hermes_status,
            Status::Waiting,
            &[
                "⚠️  DANGEROUS COMMAND: rm -rf /tmp\n[o]nce  |  [s]ession  |  [a]lways  |  [d]eny\nChoice [o/s/a/D]:",
                "dangerous command detected\nproceed?",
            ],
        );
        assert_all(
            detect_omp_status,
            Status::Idle,
            &["plain command output", "", " \n\t\n"],
        );
        assert_all(
            detect_vibe_status,
            Status::Waiting,
            &[
                "↑↓ navigate  Enter select  ESC reject",
                "⚠ bash command\nExecute this?",
                "› Yes\n  Yes and always allow bash for this session\n  No and tell the agent",
            ],
        );
        let padded = format!(
            "✶ Working… (4s · ↓ 88 tokens)\n  esc to interrupt\n{}",
            "\n".repeat(40)
        );
        assert_eq!(detect_claude_status(&padded), Status::Running);

        // ANSI is stripped before matching, including Claude 2.1.118's per-word colouring.
        for (tool, pane, expected) in [
            (
                "claude",
                "\x1b[38;5;174m✶\x1b[39m \x1b[38;5;180mWorking…\x1b[38;5;174m \x1b[38;5;246m(4s · ↓\x1b[39m \x1b[38;5;246m88 tokens)\x1b[39m\n\x1b[39m  \x1b[38;5;246mesc\x1b[39m \x1b[38;5;246mto\x1b[39m \x1b[38;5;246minterrupt\x1b[39m",
                Status::Running,
            ),
            (
                "opencode",
                "\x1b[38;2;39;62;94m⬝⬝⬝⬝⬝⬝⬝⬝\x1b[0m  \x1b[38;2;238;238;238mesc \x1b[38;2;128;128;128minterrupt\x1b[0m",
                Status::Running,
            ),
            (
                "opencode",
                "\x1b[38;2;255;255;255m⠋\x1b[0m generating",
                Status::Running,
            ),
            ("unknown_tool", "Processing ⠋", Status::Idle),
        ] {
            assert_eq!(
                detect_status_from_content_in("", pane, tool),
                expected,
                "{tool}: {pane:?}"
            );
        }
    }

    #[test]
    fn test_detect_claude_status_running_on_abbreviated_token_counter() {
        let long_turn_pane = "\
● Clippy clean on both; waiting on the base-commit control.\n\
  Ran 2 shell commands\n\
✻ Judging #3413 feedback… (22m 8s · ↓ 44.7k tokens)\n\
┌─────\n\
❯\n\
└─────\n\
  ⏵⏵ auto mode on";
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
            ("wrap inside seconds", "(22m 8\ns · ↓ 44.7k tokens)"),
        ];
        for (name, pane) in cases {
            assert_eq!(detect_claude_status(pane), Status::Running, "{name}");
        }
    }

    #[test]
    fn test_has_claude_live_token_counter_variants() {
        let cases = [
            ("plain integer", "(4s · ↓ 88 tokens)", true),
            ("multi-digit", "(12s · ↓ 1234 tokens)", true),
            ("decimal with k", "(53s · ↓ 7.0k tokens)", true),
            ("plain decimal", "(4s · ↓ 44.7 tokens)", true),
            ("integer with k", "(4s · ↓ 512k tokens)", true),
            ("decimal with m", "(4s · ↓ 1.2m tokens)", true),
            ("integer with g", "(4s · ↓ 3g tokens)", true),
            ("two-digit fraction", "(4s · ↓ 1.23m tokens)", true),
            (
                "wrapped before paren",
                "✻ Judging #3413 feedback… (4s · ↓ 88 tokens\n)",
                true,
            ),
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
            ("empty duration", "(s · ↓ 88 tokens)", false),
            ("unit without own digits", "(22m s · ↓ 88 tokens)", false),
            ("no count", "(4s · ↓ tokens)", false),
            ("comma separator", "(4s · ↓ 12,345 tokens)", false),
            ("uppercase suffix", "(4s · ↓ 44.7K tokens)", false),
            ("non-digit count", "(4s · ↓ many tokens)", false),
            ("no opening paren", "summary: 4s · ↓ 88 tokens)", false),
            (
                "prose before the duration",
                "see issue s · ↓ 88 tokens)",
                false,
            ),
            ("double dot", "(4s · ↓ 44..7k tokens)", false),
            ("no digit after dot", "(4s · ↓ 44.tokens)", false),
            ("punctuation after paren", "(4s · ↓ 7.0k tokens),", false),
            ("quote after paren", "(4s · ↓ 88 tokens)\",", false),
            (
                "decoy anchor then real counter",
                "  ⏵⏵ bypass permissions on · ← for agents · ↓ to manage\n(4s · ↓ 88 tokens)",
                true,
            ),
            ("bare arrow in prose", "watch the ↓ 88 tokens) chart", false),
            ("prose after paren", "(4s · ↓ 88 tokens) renders", false),
            (
                "next line completes shape",
                "● The helper reads s · ↓ 42 tokens\n) -> Status {",
                false,
            ),
            (
                "middle dot arrow without duration",
                "chart · ↓ 88 tokens)",
                false,
            ),
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

    /// Pane shape against the hook's last word, for Claude Code.
    #[test]
    fn claude_hook_reconciliation_table() {
        let parked_over_completion = "\
✻ Sautéed for 39s · 1 monitor still running\n\
──────────────────────────────\n\
❯ stop the monitor\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on · PR #444 · 1 monitor · ← for agents · ↓ to manage";
        let streaming_over_prompt = "\
  prose still being generated by the model\n\
──────────────────────────────\n\
❯ stop the monitor\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on · PR #444 · 1 monitor · ← for agents · ↓ to manage";
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
        let streaming_over_chrome = "\
  prose still being generated by the model\n\
                                              new task? /clear to save 131.6k tokens\n\
─────────────────────── rebrand-chord-charts-primary ──\n\
❯ merge it\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · PR #484 · ← for agents";
        let running_stale = hook(Status::Running, secs(120));
        let fresh_bound = claude_fresh_bound();
        let parked_at_prompt = "❯ \n\n  ? for shortcuts · ← for agents";
        // A standing waiting hook holds on a blank pane.
        assert_hook_all("claude", stale_wait(), Status::Waiting, &["", "   \n\n"]);
        assert_hook_all(
            "claude",
            hook(Status::Idle, None),
            Status::Idle,
            &["✻ Worked for 1m 52s\n❯\n  ? for shortcuts", "  \n \n"],
        );
        // A stale running hook yields to parked evidence under a typed prompt,
        // never to a blank pane or to prose still streaming.
        assert_hook_all(
            "claude",
            running_stale,
            Status::Idle,
            &[parked_over_completion, clear_hint, labeled_separator],
        );
        assert_hook_all(
            "claude",
            running_stale,
            Status::Running,
            &["   \n\n  ", streaming_over_prompt, streaming_over_chrome],
        );
        // The freshness bound is exclusive.
        assert_hook_all(
            "claude",
            hook(Status::Running, Some(fresh_bound)),
            Status::Idle,
            &[parked_at_prompt],
        );
        assert_hook_all(
            "claude",
            hook(
                Status::Running,
                Some(fresh_bound - std::time::Duration::from_secs(1)),
            ),
            Status::Running,
            &[parked_at_prompt],
        );
        let running = hook(Status::Running, None);
        let idle = hook(Status::Idle, None);
        let running_fresh = hook(Status::Running, secs(1));
        let running_stale = hook(Status::Running, secs(120));
        let running_old = hook(Status::Running, secs(300));
        assert_hook_all(
            "claude",
            running,
            Status::Waiting,
            &[
                // waiting on ask user question
                "\x1b[1m  Which approach do you prefer?\x1b[0m\n\
\x1b[1m❯ 1. First\x1b[0m\n    2. Second\n\n\
  Enter to select · ↑/↓ to navigate · Esc to cancel",
                // waiting on approval prompt
                "\x1b[1m  Do you want to proceed?\x1b[0m\n\
  ❯ 1. Yes\n    2. No\n\n  Esc to cancel · Tab to amend\n\
\x1b[38;5;174m✶\x1b[0m Herding… (53s · ↓ 7.0k tokens)",
            ],
        );
        assert_hook_all(
            "claude",
            running,
            Status::Running,
            &[
                // keeps running without prompt
                "✶ Working… (4s · ↓ 88 tokens)\n  esc to interrupt",
                // keeps running when new turn follows interrupt
                "  ⎿  Interrupted · What should Claude do instead?\n\
● Picking up where we left off\n\
✶ Herding… (3s · ↓ 42 tokens)\n  esc to interrupt",
            ],
        );
        assert_hook_all(
            "claude",
            idle,
            Status::Running,
            &[
                // running pane upgrades to running
                "✶ Working… (4s · ↓ 88 tokens)\n  esc to interrupt",
            ],
        );
        assert_hook_all(
            "claude",
            idle,
            Status::Waiting,
            &[
                // blocking prompt upgrades to waiting
                "\
  Do you want to proceed?\n\
  ❯ 1. Yes\n    2. No\n\n  Esc to cancel · Tab to amend",
            ],
        );
        assert_hook_all(
            "claude",
            idle,
            Status::Idle,
            &[
                // resists echoed running text
                "\
●  Read(src/tmux/status_detection.rs)\n\
  ⎿  2472:        let pane = \"✶ Working… (4s · ↓ 88 tokens)\\n  esc to interrupt\";\n\
  ⎿  +    if collapsed.contains(\"esc to interrupt\") {\n\
✻ Worked for 12s\n\
❯\n\
  ? for shortcuts",
            ],
        );
        assert_hook_all(
            "claude",
            stale_wait(),
            Status::Idle,
            &[
                // cleared on esc cancel
                "\x1b[1m> Tell me about the weather\x1b[0m\n\
● I'll pull that up.\n\n\
What should Claude do instead?\n❯\n  ? for shortcuts",
                // cleared at ready prompt
                "● Done for now.\n\n❯\n  ? for shortcuts",
            ],
        );
        assert_hook_all(
            "claude",
            stale_wait(),
            Status::Running,
            &[
                // resumed turn reads running
                "✶ Working… (4s · ↓ 88 tokens)\n  esc to interrupt",
            ],
        );
        assert_hook_all(
            "claude",
            stale_wait(),
            Status::Waiting,
            &[
                // keeps waiting while question on screen
                "\x1b[1m  Which approach do you prefer?\x1b[0m\n\
❯ 1. First\n    2. Second\n\n\
  Enter to select · ↑/↓ to navigate · Esc to cancel",
                // keeps waiting while approval on screen
                "\x1b[1m  Do you want to proceed?\x1b[0m\n\
  ❯ 1. Yes\n    2. No\n\n  Esc to cancel · Tab to amend",
            ],
        );
        assert_hook_all(
            "claude",
            running,
            Status::Idle,
            &[
                // idle on esc interrupt
                "\x1b[2m  ⎿  Interrupted · What should Claude do instead?\x1b[0m\n\n\
\x1b[1m❯ \x1b[0m\n\n  ? for shortcuts · ← for agents",
            ],
        );
        assert_hook_all(
            "claude",
            running_fresh,
            Status::Running,
            &[
                // trusts fresh running at idle prompt
                "❯ \n\n  ? for shortcuts · ← for agents",
            ],
        );
        assert_hook_all(
            "claude",
            running_stale,
            Status::Idle,
            &[
                // idle on stale running at idle prompt
                "\x1b[1m❯ \x1b[0m\n\n  ? for shortcuts · ← for agents",
                // idle after background agent finished
                "\
  The agent flagged two things worth noting about the module surface.\n\
✻ Churned for 1m 40s\n\
──────────────────────────────\n\
❯ \n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents · ↓ to manage\n\
  ● main\n\
  ◯ general-purpose  Summarize tmux module pub fns    1m 14s · ↓ 40.4k tokens",
                // ignores prose background wait mention
                "\
● Waiting for background agent results before summarizing.\n\
* Waiting for 2 background agents to finish before merging\n\
❯ \n\
  ? for shortcuts · ← for agents",
                // idle with frozen integer strip counter
                "\
✻ Churned for 12s\n\
──────────────────────────────\n\
❯ \n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents · ↓ to manage\n\
  ● main\n\
  ◯ general-purpose  Quick lookup    19s · ↓ 728 tokens",
                // idle in bypass mode with ghost text
                "\
✻ Churned for 1m 40s\n\
──────────────────────────────\n\
❯ Explain how the vt.rs VtChannel is shared across viewers\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
                // idle with typed text after turn end
                "\
✻ Cooked for 49s\n\
──────────────────────────────\n\
❯ this is some unsubmitted text i am typing while the agent works\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle)",
            ],
        );
        assert_hook_all(
            "claude",
            running_old,
            Status::Running,
            &[
                // keeps running on background agent wait
                "\
● Agent(Summarize tmux module pub fns)\n\
  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)\n\
● The background agent is running. I'll wait for its completion notification.\n\
✻ Waiting for 1 background agent to finish\n\
──────────────────────────────\n\
❯ \n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents · ↓ to manage\n\
  ● main\n\
  ◯ general-purpose  Summarize tmux module pub fns    19s · ↓ 36.4k tokens",
            ],
        );
        assert_hook_all(
            "claude",
            running_stale,
            Status::Running,
            &[
                // running with typed text while streaming
                "\
  signals onto a single channel. Applied to terminals, the idea was seductive: what if a\n\
  single physical terminal could host several independent logical sessions, each behaving\n\
  as though it had the machine to itself?\n\
──────────────────────────────\n\
❯ this is some unsubmitted text i am typing while the agent works\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle)",
                // running in bypass mode while active
                "\
✽ Crunching… (19s · ↓ 166 tokens)\n\
  ⎿  Tip: Use /memory to view and manage Claude memory\n\
──────────────────────────────\n\
❯ \n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ← for agents",
                // running during compaction
                "\
✢ Compacting conversation… (17s)\n\
❯ \n\
  ⏵⏵ auto mode on (shift+tab to cycle) · esc\n\
  to interrupt · ← for agents",
                // running with wrapped interrupt hint
                "\
❯ \n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc\n\
  to interrupt · ← for agents",
                // stale running keeps running while active
                "✶ Working… (90s · ↓ 4.1k tokens)\n  esc to interrupt",
            ],
        );
        assert_hook_all(
            "claude",
            running_stale,
            Status::Waiting,
            &[
                // waiting outranks mode cycle footer
                "\
Do you want to proceed?\n\
❯ 1. Yes\n\
  2. No\n\
──────────────────────────────\n\
  ⏸ plan mode on (shift+tab to cycle) · ← for agents",
            ],
        );

        // Non-running hooks pass through whatever the pane shows.
        assert_hook_all(
            "claude",
            hook(Status::Waiting, None),
            Status::Waiting,
            &[""],
        );
        assert_hook_all(
            "claude",
            hook(Status::Idle, None),
            Status::Idle,
            &["Do you want to proceed?\n1. Yes"],
        );
    }

    /// Pane shape against the hook's last word, for Codex.
    #[test]
    fn codex_hook_reconciliation_table() {
        let running_now = hook(Status::Running, secs(0));
        assert_hook_all(
            "codex",
            running_now,
            Status::Waiting,
            &[
                // waiting for plan radio input
                r#"
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
"#,
                // waiting for radio only input
                "\
  › 1. Yes
    2. No
    3. Maybe
",
            ],
        );
        assert_hook_all(
            "codex",
            running_now,
            Status::Running,
            &[
                // ignores stale radio prompt before activity
                r#"
  Question 1/1 (1 unanswered)
  Do you want apple, banana, orange, or something else?

  › 1. Apple (Recommended)  Pick apple for the default simple choice.
    2. Banana               Pick banana for a second common option.
    3. Orange               Pick orange for a citrus option.
    4. None of the above    Optionally, add details in notes (tab).

  tab to add notes | enter to submit answer | esc to interrupt

› Apple

• Working (4s • esc to interrupt)
"#,
                // keeps running after completed turn with new activity
                r#"
<< Code review finished >>

─ Worked for 7m 40s ──────────────────────────────────────────

› Implement the fix

• Working (4s • esc to interrupt)
"#,
                // keeps running after completed turn with plain new output
                r#"
─ Worked for 7m 40s ──────────────────────────────────────────

› Implement the fix

I’ll inspect the status detection path first and then adjust the idle override.
"#,
                // ignores stale interruption before activity
                r#"
■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to
report the issue.

› Try again

• Working (4s • esc to interrupt)
"#,
                // ignores stale interruption before approval
                r#"
■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to
report the issue.

› Try again

run this command? (y/n)
"#,
            ],
        );
        assert_hook_all(
            "codex",
            running_now,
            Status::Idle,
            &[
                // idle after cancelled radio prompt
                r#"
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
"#,
                // idle after wrapped esc interruption
                r#"
› something


■ Conversation interrupted - tell the model what to
do differently. Something went wrong? Hit `/feedback` to
report the issue.


› Write tests for @filename

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#,
                // idle after wrapped interruption without glyph
                r#"
› something


Conversation interrupted - tell the model what to
do differently. Something went wrong? Hit `/feedback` to
report the issue.


› Write tests for @filename

  gpt-5.5 xhigh fast · ~/appsSource/agent-of-empires
"#,
                // idle after esc interruption
                r#"
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
"#,
                // idle after completed review
                r#"
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
"#,
                // idle after completed review without worked divider
                r#"
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
"#,
            ],
        );

        // Generic pane states never override a running codex hook.
        assert_hook_all(
            "codex",
            running_now,
            Status::Running,
            &[
                "\n>> Code review started: staged changes <<\n\n<< Code review finished >>\n\n\
                 › Implement the review comment\n\n\
                 I’ll inspect the status detection path first and then adjust the idle override.\n",
                "run this command? (y/n)",
                "› Write tests for @filename",
                "file saved",
            ],
        );
        // Only running hooks are overridden by a question on screen.
        let question =
            "  Question 1/1 (1 unanswered)\n  Pick one\n\n  › 1. Apple\n    2. Banana\n\n\
                        \x20 tab to add notes | enter to submit answer | esc to interrupt\n";
        for status in [Status::Waiting, Status::Idle] {
            assert_hook_all("codex", hook(status, secs(0)), Status::Waiting, &[question]);
        }
    }

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
        let cases = [
            "\
 Tensions I'd want us to discuss, not resolve by menu:
 1. What does deterministic bind? The arc's step gates what runs may launch.
 2. Whether product survives as a word.
 3. What's left of lane the tool under this shape.
 Where do you want to dig, the delta policy or the teeth question?
 \u{2733} Puttering\u{2026} (26s \u{b7} thinking more with high effort)",
            "\
\u{25cf} The menu it showed was:
> 1. Yes
> 2. No
\u{25cf} Do you want to proceed with that reading?
 \u{273b} Working\u{2026} (12s \u{b7} \u{2193} 431 tokens)
   esc to interrupt
",
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

    #[test]
    fn test_claude_deciding_rule_names_the_evidence() {
        let running = "\
● Sure, let me look at that.\n\
✶ Working… (4s · ↓ 88 tokens)\n\
  esc to interrupt\n";
        for rule in ["active_spinner", "live_token_counter", "interrupt_hint"] {
            assert!(claude_rule_matches(rule, running), "{rule}");
        }
        assert_eq!(
            detect_via_manifest("claude", running, "", None),
            Status::Running
        );

        let parked = "\
✻ Worked for 1m 52s\n\
❯\n\
  ? for shortcuts\n";
        assert_eq!(claude_rule(parked), "completed_turn");

        let typed = "\
✻ Worked for 1m 52s\n\
❯ half-typed next prompt\n\
  ? for shortcuts\n";
        assert_eq!(claude_rule(typed), "completed_turn");

        assert_eq!(claude_rule("   \n  \n"), "no_rule");
        assert_eq!(claude_rule("plain prose only"), "no_rule");
    }

    #[test]
    fn test_waiting_hook_claude_survives_question_scrolled_out_of_window() {
        let question = "  Which approach do you prefer?\n\
❯ 1. First\n    2. Second\n\n\
  Enter to select · ↑/↓ to navigate · Esc to cancel\n";
        let noise: String = (0..31).map(|i| format!("notification {i}\n")).collect();
        let fresh = hook(Status::Waiting, secs(1));
        for hook in [fresh, stale_wait()] {
            assert_eq!(
                detect_via_manifest("claude", &format!("{question}{noise}"), "", hook),
                Status::Waiting
            );
            assert_eq!(
                detect_via_manifest(
                    "claude",
                    &format!("{question}{noise}"),
                    "\u{2733} Claude Code",
                    hook
                ),
                Status::Waiting
            );
            assert_eq!(
                detect_via_manifest(
                    "claude",
                    &format!("{noise}❯ half-typed follow-up"),
                    "",
                    hook
                ),
                Status::Idle
            );
        }
    }

    /// A stale waiting hook survives only while its prompt is still on screen.
    #[test]
    fn stale_waiting_hook_cleared_and_kept_per_agent() {
        let cursor_prompt = "Run this command?\n\n> Allow this command\n  Deny\n\n\
enter to select · esc to cancel";
        for (agent, cleared, kept) in [
            ("codex", "file saved", "approve changes?"),
            ("cursor", "→ add a follow-up", cursor_prompt),
            ("qwen", "random output text", "Allow this tool to run?"),
            ("gemini", "file saved", "approve changes?"),
        ] {
            assert_hook_all(agent, stale_wait(), Status::Idle, &[cleared]);
            assert_hook_all(agent, stale_wait(), Status::Waiting, &[kept]);
        }
    }

    #[test]
    fn test_claude_background_wait_only_counts_in_the_status_slot() {
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
        assert_eq!(
            detect_status_from_content_in("", stale, "claude"),
            Status::Idle
        );
        assert_eq!(
            detect_via_manifest("claude", stale, "", hook(Status::Running, secs(300))),
            Status::Idle
        );
        assert_eq!(
            detect_via_manifest("claude", stale, "", hook(Status::Idle, None)),
            Status::Idle
        );
        let live = "\
● Agent(Review PR #484)\n\
  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)\n\
✻ Waiting for 1 background agent to finish\n\
──────────────────────────────\n\
❯ merge it\n\
──────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · PR #484 · ← for agents";
        assert_eq!(
            detect_via_manifest("claude", live, "", hook(Status::Idle, None)),
            Status::Running
        );
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
                detect_via_manifest("claude", &no_prompt_line, "", hook(Status::Idle, None)),
                Status::Running,
                "footer: {footer}"
            );
        }
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
        assert!(!claude_rule_matches(
            "background_agent_wait",
            "Waiting for 1 background agent to finish"
        ));
        assert!(!claude_rule_matches(
            "background_agent_wait",
            "● Waiting for background agent results"
        ));
        assert!(!claude_rule_matches(
            "background_agent_wait",
            "* Waiting for 2 background agents to finish before merging"
        ));
        assert!(!claude_rule_matches("background_agent_wait", ""));
    }

    #[test]
    fn test_claude_completed_turn_rule() {
        assert!(claude_rule_matches("completed_turn", "✻ Cooked for 49s"));
        assert!(claude_rule_matches(
            "completed_turn",
            "✻ Baked for 10s · 1 shell still running"
        ));
        assert!(claude_rule_matches("completed_turn", "✻ Worked for 1m 52s"));
        assert!(!claude_rule_matches(
            "completed_turn",
            "· Undulating… (14s · ↓ 144 tokens)"
        ));
        assert!(!claude_rule_matches(
            "completed_turn",
            "✻ Waiting for 1 background agent to finish"
        ));
        assert!(!claude_rule_matches("completed_turn", "Worked for 1m 52s"));
        assert!(!claude_rule_matches("completed_turn", ""));
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
        let parked = "✻ Cooked for 1m 58s\n❯ \n";
        let footer =
            |tail: &str| format!("{parked}  ⏵⏵ auto mode on (shift+tab to cycle) · PR #3600{tail}");

        let with_shells = footer(" · 5 shells · ← for agents");
        assert_eq!(
            detect_via_manifest("claude", &with_shells, "", None),
            Status::Idle
        );
        assert_eq!(claude_rule(&with_shells), "completed_turn");
        let live = format!("✻ Brewing… (17s · esc to interrupt)\n{with_shells}");
        assert_eq!(
            detect_via_manifest("claude", &live, "", None),
            Status::Running
        );

        let mcp = format!("{parked}✻ Ran 3 tools · 2 MCP tasks still running\n");
        assert!(claude_rule_matches("background_mcp_task", &mcp));
        assert_eq!(
            detect_via_manifest("claude", &mcp, "", None),
            Status::Running
        );

        let quoted = format!("{mcp}Do you want to proceed?\n❯ 1. Yes\n  2. No\n");
        assert!(!claude_rule_matches("background_mcp_task", &quoted));
    }

    #[test]
    fn test_claude_stuck_running_pane_recovers() {
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
        assert_eq!(
            detect_via_manifest("claude", pane, "", hook(Status::Running, secs(1))),
            Status::Running
        );
        for age in [30, 120, 7200] {
            assert_eq!(
                detect_via_manifest(
                    "claude",
                    pane,
                    "",
                    hook(Status::Running, Some(std::time::Duration::from_secs(age)))
                ),
                Status::Idle,
                "age {age}s"
            );
        }
        let resumed = "\
✢ Precipitating… (11m 14s · ↓ 25.1k tokens)\n\
                    ✔ Update installed · Restart to update\n\
────────────────────────────────────────────────────────────\n\
❯ \n\
────────────────────────────────────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to …";
        assert_eq!(
            detect_via_manifest("claude", resumed, "", None),
            Status::Running
        );
    }

    #[test]
    fn test_claude_typed_prompt_is_not_evidence() {
        let stale = hook(Status::Running, secs(120));
        let box_ = "──────────────────────────────";

        let streaming =
            format!("  prose still being generated\n{box_}\n❯ half-typed next prompt\n{box_}");
        assert_eq!(
            detect_via_manifest("claude", &streaming, "", stale),
            Status::Running
        );
        assert_eq!(
            detect_via_manifest("claude", &streaming, "⠹ Working", None),
            Status::Running
        );

        let parked = format!("✻ Cooked for 49s\n{box_}\n❯ half-typed next prompt\n{box_}");
        assert_eq!(
            detect_via_manifest("claude", &parked, "", stale),
            Status::Idle
        );

        let interrupted = "\
⎿  Interrupted · What should Claude do instead?\n\
❯ half-typed next prompt\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(
            detect_via_manifest("claude", interrupted, "", stale),
            Status::Idle
        );

        let bare = "  some prose\n❯ \n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(detect_via_manifest("claude", bare, "", stale), Status::Idle);

        let menu = "\
Do you want to proceed?\n\
❯ 1. Yes\n\
  2. No\n\
  ⏸ plan mode on (shift+tab to cycle)";
        assert_eq!(
            detect_via_manifest("claude", menu, "", stale),
            Status::Waiting
        );

        let running =
            format!("✽ Crunching… (19s · ↓ 166 tokens)\n{box_}\n❯ half-typed next prompt\n{box_}");
        assert_eq!(
            detect_via_manifest("claude", &running, "", stale),
            Status::Running
        );
    }

    #[test]
    fn test_claude_mode_footer_is_chrome_not_evidence() {
        let stale = hook(Status::Running, secs(120));
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
            assert_eq!(
                detect_via_manifest("claude", &pane, "", stale),
                Status::Idle,
                "{footer}"
            );
        }

        let echoed = "\
✻ Churned for 10s\n\
+  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n\
❯ ghost suggestion text";
        assert!(!claude_rule_matches("completed_turn", echoed));
        assert_eq!(
            detect_via_manifest("claude", echoed, "", stale),
            Status::Running
        );

        let running = "\
✻ Churned for 10s\n\
❯ ghost suggestion text\n\
  ⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt · ← for agents";
        assert_eq!(
            detect_via_manifest("claude", running, "", stale),
            Status::Running
        );
    }

    #[test]
    fn test_detect_vibe_status_running() {
        assert_eq!(detect_vibe_status("processing ⠋"), Status::Running);
        assert_eq!(detect_vibe_status("⠹"), Status::Running);

        assert_eq!(detect_vibe_status("Running bash"), Status::Running);
        assert_eq!(detect_vibe_status("Reading file"), Status::Running);
        assert_eq!(detect_vibe_status("Writing changes"), Status::Running);
        assert_eq!(detect_vibe_status("Generating code"), Status::Running);

        let vertical = "R\nu\nn\nn\ni\nn\ng";
        assert_eq!(detect_vibe_status(vertical), Status::Running);
        assert!(vibe_rule_matches("activity_word", vertical));
        assert!(!vibe_rule_matches("spinner", vertical));
        assert!(!vibe_rule_matches("trailing_ellipsis", vertical));

        let glued = "finished a long run\nning total of 3 files";
        assert_eq!(detect_vibe_status(glued), Status::Idle);
        assert!(!vibe_rule_matches("activity_word", glued));

        let split = "R\nu\nn\n\nn\ni\nn\ng";
        assert_eq!(detect_vibe_status(split), Status::Idle);
        assert!(!vibe_rule_matches("activity_word", split));

        assert_eq!(detect_vibe_status("Working…"), Status::Running);
        assert_eq!(detect_vibe_status("Loading..."), Status::Running);
    }

    #[test]
    fn test_hook_only_agents_report_idle_from_the_pane() {
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

    const PI_RUNNING_PANE: &str = "\
Twelve is a dozen.\n\
⠏ Working...\n\
────────────────────────────────────────\n\
────────────────────────────────────────\n\
/tmp\n\
0.0%/272k (auto)                    gpt-5.5 • medium\n";

    const PI_FINISHED_PANE_WITH_ACTIVITY_PROSE: &str = "\
I'll launch an aoe session to fix #443.\n\
The agent is now working on #443, extending the SSRF gate to the write path.\n\
You can monitor progress with aoe session logs.\n\
────────────────────────────────────────\n\
\n\
────────────────────────────────────────\n\
/Users/nbrake/scm/otari-workspace/otari-worktrees/orchestrator\n\
↑45k ↓11k $0.009 9.6%/500k (auto)                    gpt-5.5 • medium\n";

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

    const OMO_DEEP_FOOTER_PARKED_PANE: &str = "\
Working through the eval matrix, results streaming to the report ⠋\n\
Tip: Set thinkingBudgets in settings.json to choose which models think.\n\
↳ Want the full story on any tip? Ask about it in chat.\n\
────────────────────────────────────────\n\
❯\n\
────────────────────────────────────────\n\
~ • CH93.4% • $2.870 • 115K/1M (11.5%) (auto)      claude-opus-4-6:xhigh\n\
(😺 OmO Native) Pursuing goal (1m) mem:12k/200k\n";

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

    fn pane_with_line_at_depth(line: &str, depth: usize) -> String {
        let filler = "Footer filler line.\n".repeat(depth.saturating_sub(1));
        format!("{line}\n{filler}")
    }

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

    const MINIMAL_COMPOSER_BOX: &str = "╭── π  > GPT-5.6 Sol ─╮\n╰─                   ─╯";

    const OMP_PARKED_AT_COMPOSER_REPRO: &str = "\
 ※ recap: Goal was a simple probe: replied OK and ran echo, which returned rca-probe-42 successfully.

╭── π  > ⬢ Ox Alpha · ◉ max > 🗑 …of-empires-dev/scratch/4d9eb39378df4f4e ▶───2%───────────────────┃──────────1M─◀ Reply with OK ──╮
╰─                                                                                                                                                                                      ─╯";

    #[test]
    fn test_detect_omp_status_idle_at_composer_box() {
        let cases = [
            ("bare box", MINIMAL_COMPOSER_BOX.to_string()),
            ("turn finished", format!("OK\n{MINIMAL_COMPOSER_BOX}")),
            (
                "stale loader ignored",
                format!("⠋ Working… ⟦esc⟧\nCompleted response.\nAdditional output.\nOK\n{MINIMAL_COMPOSER_BOX}"),
            ),
            (
                "loader pushed past footer",
                format!("⠋ Working… ⟦esc⟧\nOK\n{MINIMAL_COMPOSER_BOX}"),
            ),
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
    fn test_detect_omp_status_error_retry_table() {
        let prompt_box = MINIMAL_COMPOSER_BOX;
        let br = "─".repeat(24);
        let dismissed = " Dismissed when you send your next message.";
        let banner = |msg: &str| format!("{br}\n ✖ {msg}\n{dismissed}\n{br}\n{prompt_box}");
        let over_box = |lines: &str| format!("{lines}\n{prompt_box}");
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

        let mut cases: Vec<(String, String, Status)> = Vec::new();

        // Any provider failure the banner can carry reads as Error.
        for msg in [
            "429 Too Many Requests (rate limited). Retry after 30s.",
            "Provider returned error: overloaded",
            "Rate limit exceeded",
            "503 Service Unavailable",
            "500 Internal Server Error",
            "websocket closed before response completion",
            "Connection refused",
            "fetch failed: socket hang up",
            "timed out after 30s",
            "terminated by upstream",
            "retry delay exceeded",
            "Output blocked by content filtering policy",
            "Unknown error",
        ] {
            cases.push((format!("banner {msg}"), banner(msg), Status::Error));
        }

        // The retry label is Running whatever spelling the delay takes.
        for delay in [
            "in 5.0s",
            "in 1m5s",
            "in 500ms",
            "in 876.5ms",
            "in 2m",
            "in 2h",
            "in 1h30m",
            "in 1d",
            "in 1d5h",
        ] {
            cases.push((
                format!("label {delay}"),
                over_box(&format!(
                    "retrying 2/3 {delay}: 429 Too Many Requests (rate limited)."
                )),
                Status::Running,
            ));
        }

        // Prose that only mentions a retry, a failure or the esc key stays Idle.
        for (name, line) in [
            (
                "curl timed out",
                "curl: (28) Operation timed out after 30000 milliseconds",
            ),
            (
                "ssh refused",
                "ssh: connect to host 10.0.0.1 port 22: Connection refused",
            ),
            (
                "terminated by user",
                "The agent was terminated by the user.",
            ),
            ("retry-after header", "Retry-After: 30"),
            ("attempt prose", "I will attempt 2/3 of the cases"),
            (
                "retrying prose",
                "The tool kept retrying 2/3 of the files before giving up.",
            ),
            (
                "retrying next batch",
                "I will be retrying 2/3 in the next batch",
            ),
            (
                "stop retrying intervals",
                "Stop retrying (2/3) in 5s intervals!",
            ),
            (
                "retry failed no prefix",
                "The tool reported retry failed after 3 attempts",
            ),
            (
                "retrying my tests",
                "I keep retrying 2/3 in my tests: still failing",
            ),
            (
                "sub agent gave up",
                "auto-retry gave up after 3 attempts: 429 Too Many Requests (rate limited).",
            ),
            ("ascii esc prose", "The keymap binds cancel to [esc]"),
            (
                "maintenance esc prose",
                "Docs say: press esc (esc to cancel) during compaction",
            ),
            ("markdown working bullet", "- Working tree status is clean."),
            ("unicode markdown bullet", "• The interrupt key is [esc]"),
            ("idle recap prefix", "※ Working… ⟦esc⟧"),
            (
                "symbolic prose without hint",
                "◐ Working through the explanation",
            ),
            (
                "unindented symbolic prose pair",
                "✓ Done with step 3\nSee docs: press [esc]",
            ),
            (
                "symbolic prose pair across blank row",
                "→ Some heading\n\n The cancel key is [esc]",
            ),
            (
                "indented prose after completed sentence",
                "✓ Done with step 3.\n See docs: press [esc]",
            ),
            ("unicode quote border", "▏ quoted: press [esc]"),
            (
                "nerd markdown bullet",
                "\u{f111} The interrupt key is [esc]",
            ),
            (
                "stale terminal lines out of window",
                " Error: Retry failed after 10 attempts: …\n OK\n Done.\n Next\n Final",
            ),
        ] {
            cases.push((name.to_string(), over_box(line), Status::Idle));
        }

        // Shapes that each pin one window bound or one precedence rule.
        let pinned: [(&str, String, Status); 26] = [
            (
                "banner alt glyph",
                format!(
                    "{br}\n ✘ 429 Too Many Requests (rate limited). Retry after 30s.\n{dismissed}\n{br}\n{prompt_box}"
                ),
                Status::Error,
            ),
            (
                "terminal lines",
                over_box(
                    " Error: Retry budget exhausted after 10 retries: Unable to connect. Is the computer able to access the url?\n Error: Retry failed after 10 attempts: Unable to connect. Is the computer able to access the url?",
                ),
                Status::Error,
            ),
            (
                "banner retry failed",
                over_box(
                    "✖ Retry failed after 3 attempts: 429 Too Many Requests (rate limited).\n Dismissed when you send your next message.",
                ),
                Status::Error,
            ),
            (
                "banner no box",
                format!(
                    "{br}\n ✖ 429 Too Many Requests (rate limited). Retry after 30s.\n{dismissed}\n{br}"
                ),
                Status::Error,
            ),
            (
                "anchor pos 6 bound",
                over_box(&format!("{dismissed}\n l1\n l2\n l3")),
                Status::Error,
            ),
            (
                "anchor pos 7 out",
                over_box(&format!("{dismissed}\n l1\n l2\n l3\n l4")),
                Status::Idle,
            ),
            (
                "countdown",
                over_box("⠋ Retrying (2/3) in 30s… (esc to cancel)"),
                Status::Running,
            ),
            (
                "countdown no frame",
                over_box("Retrying (2/3) in 30s…"),
                Status::Running,
            ),
            (
                "countdown with banner",
                format!(
                    "{br}\n ✖ 429 Too Many Requests (rate limited). Retry after 30s.\n{dismissed}\n{br}\n⠋ Retrying (2/3) in 30s… (esc to cancel)\n{prompt_box}"
                ),
                Status::Running,
            ),
            (
                "countdown wrapped",
                over_box("⠋ Retrying (2/3)\nin 30s… (esc to cancel)"),
                Status::Running,
            ),
            (
                "countdown pos 6 bound",
                over_box("⠋ Retrying (2/3) in 30s… (esc to cancel)\n l1\n l2\n l3"),
                Status::Running,
            ),
            (
                "label now",
                over_box(
                    "└─ retrying 2/3 now: 429 Too Many Requests (rate limited). Retry after 30s.",
                ),
                Status::Running,
            ),
            (
                "rule repair attempt",
                over_box("Attempt 2/3 · generating…"),
                Status::Running,
            ),
            (
                "countdown cut 30|s",
                over_box("⠋ Retrying (2/3) in 30\ns… (esc to cancel)"),
                Status::Running,
            ),
            (
                "countdown cut s|ellipsis",
                over_box("⠋ Retrying (2/3) in 30s\n… (esc to cancel)"),
                Status::Running,
            ),
            (
                "tie terminal over label",
                over_box("retrying 1/3 now: Error: Retry failed after 2 attempts."),
                Status::Error,
            ),
            (
                "label prose accepted",
                over_box("I'm retrying 2/3 now: the API timed out."),
                Status::Running,
            ),
            (
                "label pos 12 bound",
                over_box(
                    "retrying 2/3 now: 429 Too Many Requests (rate limited).\n f1\n f2\n f3\n f4\n f5\n f6\n f7\n f8\n f9",
                ),
                Status::Running,
            ),
            (
                "label pos 13 out",
                over_box(
                    "retrying 2/3 now: 429 Too Many Requests (rate limited).\n f1\n f2\n f3\n f4\n f5\n f6\n f7\n f8\n f9\n f10",
                ),
                Status::Idle,
            ),
            (
                "answered approval above fresh banner",
                over_box(&format!(
                    "{approval_panel}\n ✖ 429 Too Many Requests (rate limited).\n{dismissed}"
                )),
                Status::Error,
            ),
            (
                "answered approval past filler above fresh banner",
                over_box(&format!(
                    "{approval_panel}\n l1\n l2\n ✖ 429 Too Many Requests (rate limited).\n{dismissed}"
                )),
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
                    "{approval_panel}\n ✖ 429 Too Many Requests (rate limited).\n{dismissed}\n{br}\n{prompt_box}"
                ),
                Status::Error,
            ),
            (
                "live countdown below answered approval",
                over_box(&format!(
                    "{approval_panel}\n⠋ Retrying (2/3) in 30s… (esc to cancel)"
                )),
                Status::Running,
            ),
            (
                "live approval below stale countdown",
                format!("⠋ Retrying (2/3) in 30s… (esc to cancel)\n{approval_panel}"),
                Status::Waiting,
            ),
            (
                "live loader below answered approval",
                format!(
                    "{approval_panel}\n⠋ Working… ⟦esc⟧\n╭── π  > GPT-5.6 Sol ─╮\n╰─ deny that         ─╯"
                ),
                Status::Running,
            ),
        ];
        cases.extend(
            pinned
                .into_iter()
                .map(|(name, pane, want)| (name.to_string(), pane, want)),
        );
        cases.push((
            "anchor over label".to_string(),
            over_box(&format!(
                "retrying 2/3 now: 429…\n ✖ 429 Too Many Requests (rate limited).\n{dismissed}"
            )),
            Status::Error,
        ));
        cases.push((
            "live approval below label".to_string(),
            format!("retrying 2/3 now: 429…\n{approval_panel}"),
            Status::Waiting,
        ));

        for (name, pane, expected) in &cases {
            assert_eq!(detect_omp_status(pane), *expected, "case: {name}");
        }
    }

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
    fn test_detect_omp_status_multiline_task_composer() {
        let band = "╭── ⠏ 1h > ◒ GPT-5.6-Sol > branch ⚙ 1 < Corriger tous les fin… ──╮";
        for (name, interrupt, header, body) in [
            (
                "localized task",
                "  ⎋ Poursuivre suite contrainte",
                band.to_string(),
                "│ draft first line │\n".to_string(),
            ),
            (
                "without task segment",
                "  ⎋ Poursuivre suite contrainte",
                band.replace(" ⚙ 1 < Corriger tous les fin…", ""),
                "│ draft first line │\n".to_string(),
            ),
            (
                "canonical interrupt",
                "  ⎋ Working…",
                band.to_string(),
                "│ draft first line │\n".to_string(),
            ),
            (
                "wrapped interrupt",
                "  ⎋ Searching the parent tree\n continuation",
                band.to_string(),
                "│ draft first line │\n".to_string(),
            ),
            (
                "composer at capture boundary",
                "  ⎋ Poursuivre suite contrainte",
                band.to_string(),
                "│ draft │\n".repeat(27),
            ),
            (
                "draft resembles activity",
                "  ⎋ Poursuivre suite contrainte",
                band.to_string(),
                "│ Working… │\n│ 1s > fake timer │\n│ ╭── fake header │\n".to_string(),
            ),
        ] {
            let pane = format!("{interrupt}\n{header}\n{body}╰─ draft last line ─╯");
            assert_eq!(detect_omp_status(&pane), Status::Running, "case: {name}");
        }
    }

    #[test]
    fn test_detect_omp_status_multiline_composer_rejects_stale_activity() {
        let active = "  ⎋ Poursuivre suite contrainte\n╭── ⠏ 1h > model status ──╮\n│ draft │\n╰─ continued draft ─╯";
        let approval = "│ ❯ Approve │\n│ Deny │\n│ up/down navigate  enter select  esc cancel │";
        let idle = "╭── π > model status ──╮\n│ draft │\n╰─ continued draft ─╯";
        for (name, pane, expected) in [
            (
                "no interrupt",
                active.replace("  ⎋ Poursuivre suite contrainte\n", ""),
                Status::Idle,
            ),
            ("elapsed clock", active.replace("⠏", "⏱"), Status::Idle),
            (
                "duration prose",
                active.replace("⠏ 1h > model status", "x 1h saved per run"),
                Status::Idle,
            ),
            (
                "new idle composer",
                format!("{active}\n{idle}"),
                Status::Idle,
            ),
            (
                "fake timer inside idle draft",
                idle.replace("│ draft │", "│ 1s > fake timer │"),
                Status::Idle,
            ),
            (
                "frame without its bottom border",
                active.replace("╰─ continued draft ─╯", "│ continued draft │"),
                Status::Running,
            ),
            (
                "output painted over the composer body",
                active.replace("│ draft │", "Completed response."),
                Status::Running,
            ),
            (
                "interrupt outside capture window",
                active.replace("│ draft │\n", &"│ draft │\n".repeat(28)),
                Status::Idle,
            ),
            (
                "lower terminal error",
                format!("{active}\nError: Retry budget exhausted after 10 retries"),
                Status::Error,
            ),
            (
                "lower approval",
                format!("{active}\n{approval}"),
                Status::Waiting,
            ),
            (
                "lower active composer",
                format!("{approval}\n{active}"),
                Status::Running,
            ),
        ] {
            assert_eq!(detect_omp_status(&pane), expected, "case: {name}");
        }
    }

    #[test]
    fn test_detect_omp_status_band_hint_above_the_narrow_windows() {
        let filler = " context 40%\n tokens 1000\n cost 0.42\n branch main";
        for (name, pane) in [
            (
                "Unicode band, hint above the compact window",
                format!("  \u{238B} Working\u{2026}\n{filler}\n\u{256D}\u{2500}\u{2500} \u{2839} 4m > model status \u{2500}\u{2500}\u{256E}\n\u{2570}\u{2500} draft \u{2500}\u{256F}"),
            ),
            (
                "ascii band, hint above the compact window",
                format!("  \u{238B} Working\u{2026}\n{filler}\n+== \u{2839} 4m == model status\n+-- draft --"),
            ),
            (
                "Unicode composer, hint above the composer window",
                "  \u{238B} Working\u{2026}\n context 40%\n tokens 1000\n\u{256D}\u{2500}\u{2500} \u{2839} 4m > model status \u{2500}\u{2500}\u{256E}\n\u{2502} draft \u{2502}\n\u{2570}\u{2500} continued draft \u{2500}\u{256F}".to_string(),
            ),
        ] {
            assert_eq!(detect_omp_status(&pane), Status::Running, "case: {name}");
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
    fn test_detect_omp_status_running_on_active_statusline() {
        for pane in [
            // claude shape with task band and separate statusline
            "  ⎋ Capturing live sessions\n\
                 ───── ⚙ 1 · Review PR · ⏱ 28.6s ─\n\
                 ❯\n\
                 ───────────────────────────────────\n\
                  ⠏ 28s · 🖥 host · 🏃 Prewalk",
            // compaction on claude shape
            " ⠧ Auto server compaction… (esc to cancel)\n\
                 ─ 👥 5 agents · Fix unresolved · ⏱ 9h12m ─\n\
                 ❯\n\
                 ───────────────────────────────────\n\
                  ⠼ 16m · 🖥 host",
            // compaction on compact Unicode band
            " ⠧ Auto server compaction… (esc to cancel)\n\
                 ╭── ⠼ 16m > model status ──╮\n\
                 ╰─ draft ─╯",
            // compaction on multiline Unicode composer
            " ⠧ Auto server compaction… (esc to cancel)\n\
                 ╭── ⠼ 16m > model status ──╮\n\
                 │ draft │\n\
                 ╰─ continued draft ─╯",
            // rule shape
            "  ⎋ Running tests\n\
                 ── ⚙ 1 · Test · ⏱ 4s ──\n\
                 ❯\n\
                  ⠦ 5s · 🖥 host",
            // pi shape
            "  ⎋ Running tools\n\
                 ───────────────────────────────────\n\
                 Ask anything, edit files, run tools\n\
                 ───────────────────────────────────\n\
                  ⠧ 12s · 🖥 host · gallery",
            // borderless shape
            "  ⎋ Working…\n\
                 ❯ Ask anything\n\
                  ⠙ 1m · 🖥 host",
            // field shape
            "  ⎋ Working…\n\
                 ▐ Ask anything ▌\n\
                  ⠸ 3s · 🖥 host",
            // rail shape
            "  ⎋ Working…\n\
                 ▎ Ask anything\n\
                  ⠴ 45s · 🖥 host",
            // band shape
            "  ⎋ Working…\n\
                  ⠦ 6s > ⬢ Sonnet > 🗺 Plan\n\
                 ╰─ Ask anything ─╯",
            // active statusline with quoted selector hint is still running
            "  ⎋ Running tests\n\
                 │ up/down navigate  enter select  esc cancel │\n\
                 ❯\n\
                 ───────────────────────────────────\n\
                  ⠏ 28s · 🖥 host",
        ] {
            assert_eq!(detect_omp_status(pane), Status::Running, "{pane}");
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
            (
                "lower approval wins over active statusline",
                format!("⎋ Running tests\n{approval}\n❯\n───────────────────────────────────\n ⠏ 28s · 🖥 host"),
                Status::Waiting,
            ),
        ];
        for (name, pane, expected) in cases {
            assert_eq!(detect_omp_status(&pane), expected, "case: {name}");
        }
    }

    #[test]
    fn test_detect_omp_status_waiting_on_plan_review_overlay() {
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
}
