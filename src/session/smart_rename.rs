//! Automatic "smart" rename of a structured-view (ACP) session from its first
//! turn.
//!
//! When a session still carries its auto-generated civilization name (see
//! [`crate::session::civilizations`]) the session's own agent is run once in
//! non-interactive one-shot mode (e.g. `claude -p`) to produce a short title,
//! and the session is renamed. Timing is set by `smart_rename_timing`: by
//! default the one-shot fires at turn-end and summarizes the whole first turn
//! (prompt plus agent output); `prompt_start` fires on the first prompt and
//! uses only that prompt. This is best-effort and fire-and-forget: it never
//! blocks or fails the user's prompt, and any failure leaves the generated
//! name in place.
//!
//! Title only: the worktree directory is intentionally not moved. The live ACP
//! worker holds the worktree as its working directory, so a directory move
//! would fail exactly like a manual rename of a running tied session does. The
//! visible session title is what gains meaning here.

use crate::agents;
use crate::session::civilizations::is_default_civ_name;
use crate::session::config::{SessionConfig, SmartRenameTiming};
use serde::Serialize;
use std::collections::HashMap;
#[cfg(feature = "serve")]
use std::path::Path;

/// Cap on concurrent smart-rename one-shots across the process. Two slots keep
/// steady-state throughput on multi-core hosts without letting N stuck
/// sessions each hold a slot for up to `ONESHOT_TIMEOUT`. See #2348.
pub const MAX_CONCURRENT: usize = 2;

/// Per-session smart-rename state surfaced to the dashboard so the sidebar can
/// show that a session will be (or is being) auto-named. `Inactive` for
/// sessions that are not eligible or already renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SmartRenameState {
    #[default]
    Inactive,
    /// Eligible and still default-named: will auto-name on the next prompt.
    Pending,
    /// A one-shot title call is in flight for this session right now.
    Running,
}

/// Why a session is not eligible for smart rename, for logging and to gate the
/// `Pending` indicator. The same predicate drives both the runtime gate and the
/// sidebar state so they cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NotStructured,
    Disabled,
    NameNotDefault,
    Sandboxed,
    NoOneshot,
    CommandOverridden,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::NotStructured => "not_structured",
            SkipReason::Disabled => "disabled",
            SkipReason::NameNotDefault => "name_not_default",
            SkipReason::Sandboxed => "sandboxed",
            SkipReason::NoOneshot => "no_oneshot",
            SkipReason::CommandOverridden => "command_overridden",
        }
    }
}

/// Single source of truth for "is this session eligible to be auto-named right
/// now". `Ok(())` means a first prompt would trigger a rename; `Err` carries the
/// disqualifying reason. `command_override_in_cfg` is whether the profile config
/// replaces this agent's binary; `command` is the instance's launch command (a
/// non-empty value differing from the agent binary is also an override).
pub fn check_eligible(
    structured: bool,
    setting_on: bool,
    title: &str,
    agent: Option<&agents::AgentDef>,
    sandboxed: bool,
    command: &str,
    command_override_in_cfg: bool,
) -> Result<(), SkipReason> {
    if !structured {
        return Err(SkipReason::NotStructured);
    }
    if !setting_on {
        return Err(SkipReason::Disabled);
    }
    if !is_default_civ_name(title) {
        return Err(SkipReason::NameNotDefault);
    }
    if sandboxed {
        return Err(SkipReason::Sandboxed);
    }
    let Some(agent) = agent else {
        return Err(SkipReason::NoOneshot);
    };
    if agent.oneshot_flag.is_none() {
        return Err(SkipReason::NoOneshot);
    }
    if command_override_in_cfg || (!command.is_empty() && command != agent.binary) {
        return Err(SkipReason::CommandOverridden);
    }
    Ok(())
}

/// Resolve the tool name used for the one-shot rename: the configured
/// `smart_rename_agent` when non-empty, otherwise the session's own tool. A
/// blank or whitespace-only setting means "same as session".
pub fn resolve_rename_tool<'a>(session_tool: &'a str, rename_setting: &'a str) -> &'a str {
    let setting = rename_setting.trim();
    if setting.is_empty() {
        session_tool
    } else {
        setting
    }
}

/// Resolve the rename agent from the `smart_rename_agent` setting and gate it,
/// returning the resolved built-in agent on success. This is the single place
/// the command-override semantics differ by rename target: when the rename
/// agent is the session's own agent, the session's launch command and an
/// override of that agent count (exactly as before). When the rename agent is
/// a DIFFERENT agent, the session's launch command is irrelevant (the one-shot
/// spawns the built-in binary fresh), so only a config override of the rename
/// agent's own binary disqualifies it. Both the runtime gate
/// (`try_smart_rename`) and the sidebar `Pending` indicator call this so they
/// cannot drift.
// One more input than `check_eligible` (the rename-agent setting); a params
// struct would only add boilerplate to the two call sites and the unit tests.
#[allow(clippy::too_many_arguments)]
pub fn check_eligible_resolved(
    structured: bool,
    setting_on: bool,
    title: &str,
    session_tool: &str,
    rename_setting: &str,
    sandboxed: bool,
    session_command: &str,
    overrides: &HashMap<String, String>,
) -> Result<&'static agents::AgentDef, SkipReason> {
    let rename_tool = resolve_rename_tool(session_tool, rename_setting);
    let agent = agents::get_agent(rename_tool);
    let (command, command_override_in_cfg) = if rename_tool == session_tool {
        (session_command, overrides.contains_key(session_tool))
    } else {
        ("", overrides.contains_key(rename_tool))
    };
    check_eligible(
        structured,
        setting_on,
        title,
        agent,
        sandboxed,
        command,
        command_override_in_cfg,
    )?;
    Ok(agent.expect("check_eligible Ok implies a built-in agent"))
}

/// Config fields the smart-rename indicator and runtime gate both consume.
/// Named fields (rather than a tuple) prevent the sidebar overlay and
/// `try_smart_rename` from drifting on positional order. Fields borrow from
/// the caller-owned [`SessionConfig`] so the sidebar's per-row projection is
/// allocation-free on the 3s poll hot path.
#[derive(Debug, Clone, Copy)]
pub struct SmartRenameConfig<'a> {
    pub setting_on: bool,
    pub rename_agent: &'a str,
    pub overrides: &'a HashMap<String, String>,
    pub timing: SmartRenameTiming,
}

/// Which firing site is asking to rename. A session runs exactly one site per
/// its `smart_rename_timing` setting; the other site's task self-cancels after
/// resolving the config (see [`try_smart_rename`]). `PromptStart` fires from the
/// ACP prompt handler on the first prompt; `TurnEnd` fires from the daemon event
/// listener on the first `prompt_complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameTrigger {
    PromptStart,
    TurnEnd,
    /// The manual "Auto-name now" action. The user asked for a rename
    /// explicitly, so it bypasses the timing gate and runs regardless of the
    /// session's `smart_rename_timing` setting.
    Forced,
}

// Only the serve-gated firing sites (and their tests) call `matches`; without
// the `serve` feature the method has no caller and clippy's dead-code lint
// (denied in CI) would fail the build.
#[cfg(feature = "serve")]
impl RenameTrigger {
    fn matches(self, timing: SmartRenameTiming) -> bool {
        match self {
            RenameTrigger::Forced => true,
            RenameTrigger::PromptStart => timing == SmartRenameTiming::PromptStart,
            RenameTrigger::TurnEnd => timing == SmartRenameTiming::TurnEnd,
        }
    }
}

/// Input for a one-shot title call. `context` is what the agent summarizes (for
/// `TurnEnd` it is the rendered first-turn transcript; for `PromptStart` just
/// the first prompt). `first_user_prompt` is kept separately as the echo
/// baseline so [`sanitize_title`] rejects a title that merely parrots the raw
/// prompt, even when `context` wraps that prompt in a `User:`/`Agent:` frame.
#[derive(Debug, Clone)]
pub struct SmartRenameInput {
    pub first_user_prompt: String,
    pub context: String,
}

/// Byte budget for the agent's prose in the rendered first-turn context. Kept
/// well under `MAX_PROMPT_BYTES` so a large first prompt cannot starve the agent
/// half: [`render_first_turn`] caps the prompt and the agent independently.
pub const FIRST_TURN_AGENT_BYTES: usize = 1024;
/// Byte budget for the user prompt inside the rendered first-turn context.
const FIRST_TURN_USER_BYTES: usize = 3072;

/// Render the first turn into a single summarizable block. Prompt and agent
/// prose are capped independently so neither can crowd the other out. With no
/// agent prose the render is prompt-only, identical to the pre-#2801 behavior.
pub fn render_first_turn(user_prompt: &str, agent_prose: &str) -> String {
    let user = truncate_bytes(user_prompt.trim(), FIRST_TURN_USER_BYTES);
    let agent = truncate_bytes(agent_prose.trim(), FIRST_TURN_AGENT_BYTES);
    if agent.is_empty() {
        user.to_string()
    } else {
        format!("User:\n{user}\n\nAgent:\n{agent}")
    }
}

/// Project a resolved [`SessionConfig`] into the three fields the smart-rename
/// indicator (`list_sessions` in `src/server/api/sessions.rs`) and the runtime
/// gate ([`try_smart_rename`]) both consume. Shared projection so the two
/// call sites cannot drift on which fields count: each site fetches the
/// resolved config via
/// [`crate::session::repo_config::resolve_config_with_repo_or_warn`] and
/// passes `.session` through this function. Returns borrowed refs so the
/// sidebar's per-row call does not allocate. See #2603.
pub fn resolve_smart_rename_config(session: &SessionConfig) -> SmartRenameConfig<'_> {
    SmartRenameConfig {
        setting_on: session.smart_rename,
        rename_agent: &session.smart_rename_agent,
        overrides: &session.agent_command_override,
        timing: session.smart_rename_timing,
    }
}

/// Hard cap on how much of the user's first message is handed to the one-shot
/// call. A title needs only the opening intent, and very large argv values can
/// trip some shells/agents.
const MAX_PROMPT_BYTES: usize = 4096;
/// Reject a candidate title longer than this many characters.
const MAX_TITLE_CHARS: usize = 60;
/// Reject a candidate title with more than this many words.
const MAX_TITLE_WORDS: usize = 8;

/// Instruction prefix sent to the agent. Constrains the output so the sanitizer
/// has the least possible work to do; anything off-format is rejected, never
/// salvaged.
const INSTRUCTION: &str = "Generate a concise 3 to 5 word title summarizing the following task. \
Output the title and nothing else: no quotes, no markdown, no code fences, no labels, \
no preamble, no explanation, no trailing punctuation. The entire response must be just \
the title on a single line. Do not refuse: if the task is unclear, still produce your \
best-guess title rather than commentary. Only if you truly cannot produce any title, \
respond with exactly NONE.";

/// Build the prompt string for the one-shot title call: the fixed instruction
/// plus the (NUL-stripped, trimmed, byte-capped) first user message.
pub fn build_prompt(user_message: &str) -> String {
    let sanitized = user_message.replace('\0', " ");
    let trimmed = sanitized.trim();
    let capped = truncate_bytes(trimmed, MAX_PROMPT_BYTES);
    format!("{INSTRUCTION}\n\nTask:\n{capped}")
}

/// Build the argv for a one-shot title call, or `None` when the agent has no
/// known one-shot mode. Shape is `[binary, oneshot_token, extra.., prompt,
/// trailing..]`: the prompt is a single argv element passed straight to the
/// process, never interpolated into a shell string, so untrusted user text
/// cannot inject arguments. `oneshot_trailing_args` is only populated for
/// flag-value one-shots (e.g. copilot `-p`), where the CLI binds the prompt to
/// the flag, so trailing flags after it stay unambiguous.
pub fn build_oneshot_argv(agent: &agents::AgentDef, prompt: &str) -> Option<Vec<String>> {
    let token = agent.oneshot_flag?;
    let mut argv = vec![agent.binary.to_string(), token.to_string()];
    // Static per-agent flags (e.g. codex `--skip-git-repo-check`) go between the
    // one-shot token and the prompt; the prompt stays directly after them so
    // untrusted user text can never be read as an argument.
    argv.extend(agent.oneshot_extra_args().iter().map(|s| s.to_string()));
    argv.push(prompt.to_string());
    // Static trailing flags (e.g. copilot `-s --allow-all-tools --no-ask-user`)
    // follow the prompt for flag-value one-shots; the CLI has already bound the
    // prompt to the one-shot flag, so these parse as options, not the prompt.
    argv.extend(agent.oneshot_trailing_args().iter().map(|s| s.to_string()));
    Some(argv)
}

/// Turn raw agent stdout into a clean title, or `None` to keep the generated
/// name. Strips ANSI escapes, scans every line, and returns the last line that
/// looks like a plausible title (short, has letters, not a refusal, not an echo
/// of the prompt). Verbose agents (`codex exec`, `opencode run`) print logs
/// around the answer; the final qualifying line is the answer.
pub fn sanitize_title(raw: &str, user_message: &str) -> Option<String> {
    let cleaned = strip_ansi(raw);
    let user_lc = user_message.trim().to_lowercase();
    let mut best: Option<String> = None;
    for line in cleaned.lines() {
        let t = clean_line(line);
        if t.is_empty() {
            continue;
        }
        let lc = t.to_lowercase();
        if lc == "none" || lc == user_lc || is_refusal(&lc) {
            continue;
        }
        let words = t.split_whitespace().count();
        if words == 0 || words > MAX_TITLE_WORDS {
            continue;
        }
        if t.chars().count() > MAX_TITLE_CHARS {
            continue;
        }
        if !t.chars().any(|c| c.is_alphabetic()) {
            continue;
        }
        best = Some(t);
    }
    best
}

/// Strip leading markdown markers / list numbering, wrapping quotes and
/// backticks, trailing sentence punctuation, and collapse inner whitespace.
fn clean_line(line: &str) -> String {
    let mut s = line.trim();
    // Leading markdown markers: bullets, headings, blockquote.
    s = s.trim_start_matches(['#', '-', '*', '>', '+']).trim_start();
    // Leading list numbering like "1." or "2)".
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &s[digits.len()..];
        if let Some(after) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
            s = after.trim_start();
        }
    }
    // Wrapping quotes / backticks / stray markdown emphasis.
    let s = s.trim_matches(['"', '\'', '`', '*', '_']);
    // Trailing sentence punctuation.
    let s = s.trim_end_matches(['.', ',', ':', ';', '!']);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_refusal(lc: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "i cannot",
        "i can't",
        "i can not",
        "i am unable",
        "i'm unable",
        "i won't",
        "i will not",
        "unable to",
        "sorry",
        "as an ai",
    ];
    PREFIXES.iter().any(|p| lc.starts_with(p)) || lc.contains("cannot determine")
}

/// Remove ANSI/CSI escape sequences (color codes etc.) that CLI agents emit.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
            }
            // Consume until the final byte (a letter) of the escape sequence.
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub(crate) fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(feature = "serve")]
pub(crate) use serve::run_oneshot;
#[cfg(feature = "serve")]
pub use serve::{prompt_start_candidate, should_trigger_smart_rename, try_smart_rename};

#[cfg(feature = "serve")]
mod serve {
    use super::*;
    use crate::server::AppState;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // Since #2348 the one-shot is deferred to the first `prompt_complete`
    // `Event::Stopped`, so it no longer races the live worker for the same
    // provider API. Standalone the call finishes well under 12s; 60s is a
    // conservative ceiling that leaves headroom for cold agent starts without
    // holding a global-semaphore slot as long as #2347's 120s band-aid did.
    // The child is killed on drop, so a timed-out call leaves no orphan.
    const ONESHOT_TIMEOUT: Duration = Duration::from_secs(60);

    /// Should this ACP broadcast event trigger a smart-rename one-shot for its
    /// session? Cheap sync predicate: reason-allowlists `prompt_complete` (all
    /// other `Stopped` reasons like `user_stopped`, `rate_limited`,
    /// `agent_unresponsive`, `reattach_idle` are either not turn boundaries or
    /// states where auto-renaming would be intrusive), and short-circuits on
    /// the two per-session gates so the listener drops non-matching events
    /// before touching the event store or spawning a task. See #2348.
    pub fn should_trigger_smart_rename(
        event: &crate::acp::state::Event,
        session_id: &str,
        attempted: &HashSet<String>,
        inflight: &HashSet<String>,
    ) -> bool {
        let is_clean_stop = matches!(
            event,
            crate::acp::state::Event::Stopped { reason } if reason == "prompt_complete"
        );
        is_clean_stop && !attempted.contains(session_id) && !inflight.contains(session_id)
    }

    /// Cheap in-memory pre-gate for the `PromptStart` firing site so the ACP
    /// prompt handler does not spawn a task (and resolve repo config) on every
    /// prompt for the common `TurnEnd` default. Checks only in-memory state: the
    /// session is structured and still default-named, and no attempt is recorded
    /// or in flight. The authoritative timing and eligibility gate stays inside
    /// [`try_smart_rename`], which re-checks under the resolved config.
    pub async fn prompt_start_candidate(state: &AppState, session_id: &str) -> bool {
        let still_default = {
            let instances = state.instances.read().await;
            instances
                .iter()
                .find(|i| i.id == session_id)
                .map(|i| i.is_structured() && is_default_civ_name(&i.title))
                .unwrap_or(false)
        };
        still_default
            && !attempted_contains(state, session_id)
            && !state
                .smart_rename_inflight
                .lock()
                .expect("smart_rename_inflight poisoned")
                .contains(session_id)
    }

    /// Whether a one-shot has already been attempted for this session this
    /// process lifetime. Shared by both firing sites so a `PromptStart` retry
    /// after a sanitizer-rejected answer, or a `TurnEnd` fire after a
    /// `PromptStart` that already produced output, both no-op.
    fn attempted_contains(state: &AppState, session_id: &str) -> bool {
        state
            .smart_rename_attempted
            .lock()
            .expect("smart_rename_attempted poisoned")
            .contains(session_id)
    }

    /// Marks a session as having an in-flight one-shot rename so a burst of
    /// rapid first prompts cannot spawn concurrent title generators. Removed on
    /// drop, so every exit path (including early returns) releases it.
    struct InflightGuard<'a> {
        set: &'a Mutex<HashSet<String>>,
        id: String,
    }

    impl<'a> InflightGuard<'a> {
        fn acquire(set: &'a Mutex<HashSet<String>>, id: &str) -> Option<Self> {
            let mut guard = set.lock().expect("smart_rename_inflight poisoned");
            let id = id.to_string();
            if !guard.insert(id.clone()) {
                return None;
            }
            Some(Self { set, id })
        }
    }

    impl Drop for InflightGuard<'_> {
        fn drop(&mut self) {
            if let Ok(mut guard) = self.set.lock() {
                guard.remove(&self.id);
            }
        }
    }

    /// Best-effort auto-rename of a structured-view session from its first
    /// turn. Spawn this detached from a firing site (the prompt handler for
    /// `PromptStart`, the event listener for `TurnEnd`, the manual action for
    /// `Forced`); it never returns an error and never touches the prompt flow.
    /// The `trigger` self-cancels against the session's `smart_rename_timing`,
    /// and all gates are re-checked under the per-session lock before the title
    /// is written, so a manual rename (or a deletion) that lands during the
    /// one-shot call always wins.
    pub async fn try_smart_rename(
        state: Arc<AppState>,
        session_id: String,
        input: SmartRenameInput,
        trigger: RenameTrigger,
    ) {
        if input.first_user_prompt.trim().is_empty() {
            return;
        }

        // Internal attempted gate. With two firing sites (`PromptStart` from the
        // prompt handler, `TurnEnd` from the listener), call-site gating alone is
        // not enough: the prompt handler spawns directly without the listener's
        // `should_trigger_smart_rename` check. A session that already produced a
        // one-shot answer (even one the sanitizer rejected) must not be retried.
        if attempted_contains(&state, &session_id) {
            return;
        }

        let Some((profile, tool, command, project_path, sandboxed, title, structured)) = ({
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == session_id).map(|i| {
                (
                    i.source_profile.clone(),
                    i.tool.clone(),
                    i.command.clone(),
                    i.project_path.clone(),
                    i.is_sandboxed(),
                    i.title.clone(),
                    i.is_structured(),
                )
            })
        }) else {
            return;
        };

        let resolved = crate::session::repo_config::resolve_config_with_repo_or_warn(
            &profile,
            Path::new(&project_path),
        );
        let cfg = resolve_smart_rename_config(&resolved.session);
        // Timing self-cancel: the session runs exactly one firing site. The
        // other site's spawned task lands here and returns, so the two modes are
        // mutually exclusive without the callers needing to resolve config.
        if !trigger.matches(cfg.timing) {
            tracing::debug!(target: "smart_rename", session = %session_id, timing = cfg.timing.as_str(), trigger = ?trigger, "skip: timing mismatch");
            return;
        }
        let agent = match check_eligible_resolved(
            structured,
            cfg.setting_on,
            &title,
            &tool,
            cfg.rename_agent,
            sandboxed,
            &command,
            cfg.overrides,
        ) {
            Ok(agent) => agent,
            Err(reason) => {
                tracing::debug!(target: "smart_rename", session = %session_id, tool = %tool, reason = reason.as_str(), "skip");
                return;
            }
        };

        let Some(_guard) = InflightGuard::acquire(&state.smart_rename_inflight, &session_id) else {
            return;
        };

        // Re-check attempted after taking the inflight slot: another task may
        // have completed and marked this session between the entry check and
        // acquiring the guard.
        if attempted_contains(&state, &session_id) {
            return;
        }

        let prompt = build_prompt(&input.context);
        let Some(argv) = build_oneshot_argv(agent, &prompt) else {
            return;
        };

        // A spawn error, timeout, or non-zero exit returns None. Do NOT mark the
        // session attempted in that case: a transient slow first prompt (cold
        // agent start) must not permanently disable naming. A later prompt
        // retries. The inflight guard above already prevents concurrent spawns.
        //
        // The permit is scoped tightly around `run_oneshot` so ineligible /
        // early-return paths above never consume a slot. Same-session duplicates
        // are already rejected by the InflightGuard, so this permit only gates
        // cross-session concurrency (#2348).
        let raw = {
            let Ok(_permit) = state.smart_rename_semaphore.acquire().await else {
                return;
            };
            run_oneshot(&session_id, &argv, &project_path, ONESHOT_TIMEOUT).await
        };
        let Some(raw) = raw else {
            return;
        };

        // The agent produced output (usable or not). Mark attempted now, once per
        // session lifetime: an answer the sanitizer rejects is not worth respawning
        // a one-shot agent (tokens) for on every later prompt.
        {
            let mut attempted = state
                .smart_rename_attempted
                .lock()
                .expect("smart_rename_attempted poisoned");
            if !attempted.insert(session_id.clone()) {
                return;
            }
        }
        let Some(new_title) = sanitize_title(&raw, &input.first_user_prompt) else {
            tracing::debug!(target: "smart_rename", session = %session_id, "skip: agent output not a usable title");
            return;
        };

        // Serialization against manual rename / worktree edits is handled
        // inside apply_auto_title via the per-session instance lock.
        apply_auto_title(&state, &session_id, &profile, &new_title).await;
    }

    /// Run the agent one-shot in the session's working directory, capturing
    /// stdout. Returns `None` on spawn error, non-zero exit, or timeout. The
    /// child is killed on drop, so a timed-out call leaves no orphan. Shared
    /// with `session::conversation_summary`, which passes a longer `timeout`
    /// for its larger transcript input.
    pub(crate) async fn run_oneshot(
        session_id: &str,
        argv: &[String],
        cwd: &str,
        timeout: Duration,
    ) -> Option<String> {
        use tokio::process::Command;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            // Capture stderr so a non-zero exit logs WHY (e.g. codex's
            // "Not inside a trusted directory"); without it the failure is an
            // opaque exit code.
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if !cwd.is_empty() {
            cmd.current_dir(cwd);
        }
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(target: "smart_rename", session = %session_id, "one-shot spawn failed: {e}");
                return None;
            }
        };
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) if out.status.success() => {
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let tail: String = stderr
                    .trim()
                    .chars()
                    .rev()
                    .take(300)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                tracing::debug!(target: "smart_rename", session = %session_id, code = ?out.status.code(), stderr = %tail, "one-shot exited non-zero");
                None
            }
            Ok(Err(e)) => {
                tracing::debug!(target: "smart_rename", session = %session_id, "one-shot io error: {e}");
                None
            }
            Err(_) => {
                tracing::debug!(target: "smart_rename", session = %session_id, "one-shot timed out");
                None
            }
        }
    }

    /// Apply an automatically-generated title to a session, persisting to
    /// storage and mirroring the in-memory instance list so connected clients
    /// see it without a reload. The write happens only while the current title
    /// is still a default civ name or still equals the last auto title we wrote
    /// (`title_is_auto_overwritable`), so a manual rename is never clobbered.
    /// Serializes against manual renames / worktree edits on this session via
    /// the per-session instance lock, and mirrors memory only when the storage
    /// write actually happened so the two never diverge.
    pub(crate) async fn apply_auto_title(
        state: &Arc<AppState>,
        id: &str,
        profile: &str,
        new_title: &str,
    ) {
        let lock = state.instance_lock(id).await;
        let _serialized = lock.lock().await;

        let storage = match crate::session::storage::Storage::new(profile, state.file_watch.clone())
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "smart_rename", session = %id, "storage open failed: {e}");
                return;
            }
        };
        let id_owned = id.to_string();
        let title_owned = new_title.to_string();
        let persisted = tokio::task::spawn_blocking(move || {
            storage.update(|instances, _groups| {
                let Some(inst) = instances.iter_mut().find(|i| i.id == id_owned) else {
                    return Ok(false);
                };
                if title_is_auto_overwritable(inst) {
                    inst.title = title_owned.clone();
                    inst.last_auto_title = Some(title_owned.clone());
                    return Ok(true);
                }
                Ok(false)
            })
        })
        .await;
        let wrote = match persisted {
            Ok(Ok(wrote)) => wrote,
            Ok(Err(e)) => {
                tracing::warn!(target: "smart_rename", session = %id, "persist failed: {e}");
                return;
            }
            Err(e) => {
                tracing::warn!(target: "smart_rename", session = %id, "persist join failed: {e}");
                return;
            }
        };
        if !wrote {
            return;
        }

        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            tracing::info!(target: "smart_rename", session = %id, old = %inst.title, new = %new_title, "auto-renamed session");
            inst.title = new_title.to_string();
            inst.last_auto_title = Some(new_title.to_string());
        }
    }

    /// Whether an automatic renamer may overwrite this session's title: either
    /// it is still a default civ name (never explicitly set), or it still
    /// matches the last title an auto renamer wrote. A manual rename leaves
    /// `title` diverged from `last_auto_title`, which freezes it against auto
    /// writes.
    pub(crate) fn title_is_auto_overwritable(inst: &crate::session::instance::Instance) -> bool {
        is_default_civ_name(&inst.title)
            || inst.last_auto_title.as_deref() == Some(inst.title.as_str())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn run_oneshot_returns_none_on_spawn_failure() {
            // A failed spawn must surface as None so try_smart_rename leaves the
            // session un-attempted and a later prompt can retry. A binary that
            // does not exist is the deterministic, machine-independent failure.
            let argv = vec![
                "aoe-smart-rename-nonexistent-binary-xyz".to_string(),
                "-p".to_string(),
                "title this".to_string(),
            ];
            assert!(
                run_oneshot("test-session", &argv, "", Duration::from_secs(60))
                    .await
                    .is_none()
            );
        }

        #[test]
        fn auto_overwritable_tracks_until_manual_rename() {
            use crate::session::instance::Instance;
            // A still-default civ name is overwritable.
            let mut inst = Instance::new("Britons", "/tmp");
            assert!(title_is_auto_overwritable(&inst));
            // After an auto write, title == last_auto_title, so a forced
            // retry can still replace an automatic title.
            inst.title = "Fix login redirect".to_string();
            inst.last_auto_title = Some("Fix login redirect".to_string());
            assert!(title_is_auto_overwritable(&inst));
            // A manual rename diverges title from last_auto_title: frozen.
            inst.title = "Production hotfix".to_string();
            assert!(!title_is_auto_overwritable(&inst));
            // Legacy record: a non-default title with no recorded auto title
            // is left untouched.
            let mut legacy = Instance::new("Vikings", "/tmp");
            legacy.title = "Hand-picked name".to_string();
            legacy.last_auto_title = None;
            assert!(!title_is_auto_overwritable(&legacy));
        }

        #[test]
        fn rename_trigger_matches_only_its_timing() {
            assert!(RenameTrigger::TurnEnd.matches(SmartRenameTiming::TurnEnd));
            assert!(RenameTrigger::PromptStart.matches(SmartRenameTiming::PromptStart));
            // The mismatched site self-cancels: this is what makes the two
            // firing sites mutually exclusive per the session's setting.
            assert!(!RenameTrigger::TurnEnd.matches(SmartRenameTiming::PromptStart));
            assert!(!RenameTrigger::PromptStart.matches(SmartRenameTiming::TurnEnd));
            // Forced (manual "Auto-name now") bypasses the timing gate.
            assert!(RenameTrigger::Forced.matches(SmartRenameTiming::TurnEnd));
            assert!(RenameTrigger::Forced.matches(SmartRenameTiming::PromptStart));
        }

        #[test]
        fn oneshot_timeout_is_60s() {
            // Drift-guard against future bump-back: #2347 raised this to 120s
            // to absorb the prompt-handler race; #2348 removed the race at
            // source, so this should stay at the deferred-trigger ceiling.
            assert_eq!(ONESHOT_TIMEOUT, Duration::from_secs(60));
        }

        #[test]
        fn should_trigger_smart_rename_only_on_clean_prompt_complete_stop() {
            use crate::acp::state::Event;
            let id = "s-1";
            let empty: HashSet<String> = HashSet::new();

            let clean = Event::Stopped {
                reason: "prompt_complete".into(),
            };
            assert!(should_trigger_smart_rename(&clean, id, &empty, &empty));

            for reason in [
                "rate_limited",
                "user_stopped",
                "user_forced",
                "agent_unresponsive",
                "prompt_orphaned",
                "reattach_idle",
                "approval_cancelled_on_restart",
                "restart_pending",
            ] {
                let ev = Event::Stopped {
                    reason: reason.into(),
                };
                assert!(
                    !should_trigger_smart_rename(&ev, id, &empty, &empty),
                    "reason={reason} should not fire smart-rename"
                );
            }

            let non_stop = Event::UserPromptSent {
                text: "hi".into(),
                attachments: vec![],
            };
            assert!(!should_trigger_smart_rename(&non_stop, id, &empty, &empty));

            let mut attempted = HashSet::new();
            attempted.insert(id.to_string());
            assert!(
                !should_trigger_smart_rename(&clean, id, &attempted, &empty),
                "attempted-gate must short-circuit even for prompt_complete"
            );

            let mut inflight = HashSet::new();
            inflight.insert(id.to_string());
            assert!(
                !should_trigger_smart_rename(&clean, id, &empty, &inflight),
                "inflight-gate must short-circuit even for prompt_complete"
            );

            assert!(
                should_trigger_smart_rename(&clean, "other-session", &attempted, &empty),
                "gates must be per-session, not global"
            );
        }

        #[tokio::test]
        async fn smart_rename_semaphore_bounds_concurrent_permits_to_max() {
            // A burst of would-be one-shots must see peak concurrency capped
            // at MAX_CONCURRENT, so N stuck sessions cannot fan out into N
            // host processes each holding a slot for `ONESHOT_TIMEOUT`.
            use std::sync::atomic::{AtomicUsize, Ordering};
            use tokio::sync::Semaphore;

            let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
            let live = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));

            let mut handles = Vec::new();
            for _ in 0..5 {
                let sem = sem.clone();
                let live = live.clone();
                let peak = peak.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    live.fetch_sub(1, Ordering::SeqCst);
                }));
            }
            for h in handles {
                h.await.expect("permit task panicked");
            }

            let seen = peak.load(Ordering::SeqCst);
            assert!(
                seen <= MAX_CONCURRENT,
                "peak concurrency {seen} exceeded cap {MAX_CONCURRENT}"
            );
            assert!(
                seen >= 2,
                "expected the burst to actually saturate the pool (seen={seen})"
            );
        }

        #[test]
        fn force_smart_rename_attempted_clear_re_enables_retry() {
            // `force_smart_rename` at sessions.rs:2582-2587 clears the
            // attempted gate before spawning `try_smart_rename`, and does NOT
            // wait for an `Event::Stopped`: the manual retry path stays
            // on-demand. The bounding is delegated to the shared semaphore
            // acquired inside `try_smart_rename`. This test emulates the
            // clear step and asserts the predicate would fire again for the
            // same session (which the listener uses; force_smart_rename itself
            // skips the predicate and spawns directly).
            use crate::acp::state::Event;
            let id = "s-1";
            let mut attempted = HashSet::new();
            attempted.insert(id.to_string());
            let inflight = HashSet::new();
            let ev = Event::Stopped {
                reason: "prompt_complete".into(),
            };
            assert!(!should_trigger_smart_rename(&ev, id, &attempted, &inflight));
            attempted.remove(id);
            assert!(should_trigger_smart_rename(&ev, id, &attempted, &inflight));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> &'static agents::AgentDef {
        agents::get_agent("claude").expect("claude agent exists")
    }

    #[test]
    fn argv_is_binary_token_prompt() {
        let argv = build_oneshot_argv(claude(), "hello").expect("claude has one-shot");
        assert_eq!(argv, vec!["claude", "-p", "hello"]);
    }

    #[test]
    fn argv_none_for_agent_without_oneshot() {
        let cursor = agents::get_agent("cursor").expect("cursor agent exists");
        assert!(build_oneshot_argv(cursor, "hello").is_none());
    }

    #[test]
    fn check_eligible_reasons() {
        let c = Some(claude());
        // Happy path.
        assert!(check_eligible(true, true, "Vikings", c, false, "", false).is_ok());
        // Each disqualifier maps to its reason.
        assert_eq!(
            check_eligible(false, true, "Vikings", c, false, "", false),
            Err(SkipReason::NotStructured)
        );
        assert_eq!(
            check_eligible(true, false, "Vikings", c, false, "", false),
            Err(SkipReason::Disabled)
        );
        assert_eq!(
            check_eligible(true, true, "Fix login bug", c, false, "", false),
            Err(SkipReason::NameNotDefault)
        );
        assert_eq!(
            check_eligible(true, true, "Vikings", c, true, "", false),
            Err(SkipReason::Sandboxed)
        );
        assert_eq!(
            check_eligible(true, true, "Vikings", None, false, "", false),
            Err(SkipReason::NoOneshot)
        );
        assert_eq!(
            check_eligible(
                true,
                true,
                "Vikings",
                Some(agents::get_agent("cursor").unwrap()),
                false,
                "",
                false
            ),
            Err(SkipReason::NoOneshot)
        );
        assert_eq!(
            check_eligible(true, true, "Vikings", c, false, "", true),
            Err(SkipReason::CommandOverridden)
        );
        assert_eq!(
            check_eligible(true, true, "Vikings", c, false, "my-wrapper", false),
            Err(SkipReason::CommandOverridden)
        );
        // Command equal to the agent binary is not an override.
        assert!(check_eligible(true, true, "Vikings", c, false, "claude", false).is_ok());
    }

    #[test]
    fn argv_codex_skips_git_repo_check_with_prompt_last() {
        // codex `exec` refuses to run outside a git repo without this flag, so a
        // scratch-session one-shot would exit non-zero. The flag goes between
        // the token and the prompt; the prompt stays the final element.
        let argv = build_oneshot_argv(agents::get_agent("codex").unwrap(), "name this")
            .expect("codex one-shot");
        assert_eq!(
            argv,
            vec!["codex", "exec", "--skip-git-repo-check", "name this"]
        );
        // claude takes no extra args: still [binary, flag, prompt].
        assert_eq!(
            build_oneshot_argv(claude(), "name this").unwrap(),
            vec!["claude", "-p", "name this"]
        );
    }

    #[test]
    fn argv_copilot_appends_silent_autoapprove_flags_after_prompt() {
        // Copilot's `-p` binds the prompt as its value, so the auto-approve and
        // silent flags follow the prompt. Without them a non-interactive title
        // call can block on a permission prompt or print stats that pollute the
        // title; with them stdout is just the final answer.
        let argv = build_oneshot_argv(agents::get_agent("copilot").unwrap(), "name this")
            .expect("copilot one-shot");
        assert_eq!(
            argv,
            vec![
                "copilot",
                "-p",
                "name this",
                "-s",
                "--allow-all-tools",
                "--no-ask-user"
            ]
        );
    }

    #[test]
    fn resolve_rename_tool_falls_back_to_session() {
        // Empty / whitespace setting => use the session's own tool.
        assert_eq!(resolve_rename_tool("claude", ""), "claude");
        assert_eq!(resolve_rename_tool("claude", "   "), "claude");
        // Non-empty setting => use it verbatim (trimmed).
        assert_eq!(resolve_rename_tool("claude", "codex"), "codex");
        assert_eq!(resolve_rename_tool("claude", "  codex "), "codex");
    }

    #[test]
    fn resolved_unset_uses_session_agent() {
        let overrides = HashMap::new();
        // Unset rename agent => resolves to the session's claude agent.
        let agent =
            check_eligible_resolved(true, true, "Vikings", "claude", "", false, "", &overrides)
                .expect("eligible");
        assert_eq!(agent.binary, "claude");
    }

    #[test]
    fn resolved_picks_distinct_rename_agent() {
        let overrides = HashMap::new();
        let agent = check_eligible_resolved(
            true, true, "Vikings", "claude", "codex", false, "", &overrides,
        )
        .expect("eligible");
        assert_eq!(agent.binary, "codex");
    }

    #[test]
    fn resolved_override_gate_targets_the_right_agent() {
        // A session-agent command override only blocks when the rename agent IS
        // the session agent.
        let mut overrides = HashMap::new();
        overrides.insert("claude".to_string(), "my-wrapper".to_string());
        assert!(matches!(
            check_eligible_resolved(true, true, "Vikings", "claude", "", false, "", &overrides),
            Err(SkipReason::CommandOverridden)
        ));
        // ...but when the rename agent is a DIFFERENT agent (codex), the
        // session's claude override is irrelevant: the one-shot launches codex
        // fresh, so it stays eligible.
        assert!(check_eligible_resolved(
            true, true, "Vikings", "claude", "codex", false, "", &overrides
        )
        .is_ok());
        // An override of the RENAME agent's own binary does block it.
        let mut codex_override = HashMap::new();
        codex_override.insert("codex".to_string(), "my-codex".to_string());
        assert!(matches!(
            check_eligible_resolved(
                true,
                true,
                "Vikings",
                "claude",
                "codex",
                false,
                "",
                &codex_override
            ),
            Err(SkipReason::CommandOverridden)
        ));
    }

    #[test]
    fn resolved_session_command_ignored_for_distinct_rename_agent() {
        // The instance's launch command (for the session agent) must not be
        // matched against a different rename agent's binary.
        let overrides = HashMap::new();
        assert!(check_eligible_resolved(
            true, true, "Vikings", "opencode", "claude", false, "opencode", &overrides
        )
        .is_ok());
    }

    #[test]
    fn resolved_unknown_rename_agent_is_no_oneshot() {
        let overrides = HashMap::new();
        assert!(matches!(
            check_eligible_resolved(
                true,
                true,
                "Vikings",
                "claude",
                "not-a-real-agent",
                false,
                "",
                &overrides
            ),
            Err(SkipReason::NoOneshot)
        ));
    }

    #[test]
    fn render_first_turn_frames_prompt_and_agent() {
        // With agent prose, both halves appear under labels.
        let r = render_first_turn("fix the login bug", "Patched the redirect in auth.rs");
        assert_eq!(
            r,
            "User:\nfix the login bug\n\nAgent:\nPatched the redirect in auth.rs"
        );
        // With no agent prose, render is prompt-only (pre-#2801 behavior).
        assert_eq!(
            render_first_turn("fix the login bug", ""),
            "fix the login bug"
        );
        assert_eq!(
            render_first_turn("fix the login bug", "   "),
            "fix the login bug"
        );
    }

    #[test]
    fn render_first_turn_caps_each_half_independently() {
        // A huge prompt must not crowd out the agent half: each side is capped
        // to its own budget, so the agent prose still survives.
        let huge_prompt = "p".repeat(FIRST_TURN_USER_BYTES * 2);
        let agent = "concise agent summary";
        let r = render_first_turn(&huge_prompt, agent);
        assert!(r.contains(agent), "agent prose must survive a huge prompt");
        assert!(r.starts_with("User:\n"));
    }

    #[test]
    fn sanitize_picks_title_from_chatty_output() {
        // The tightened instruction asks for the bare title, but a chatty agent
        // may still wrap it; the last qualifying line is the title.
        let raw = "Sure, here is a concise title:\n\nFix login redirect bug\n";
        assert_eq!(
            sanitize_title(raw, "fix the login redirect").as_deref(),
            Some("Fix login redirect bug")
        );
    }

    #[test]
    fn argv_per_agent_tokens() {
        assert_eq!(
            build_oneshot_argv(agents::get_agent("codex").unwrap(), "x").unwrap()[1],
            "exec"
        );
        assert_eq!(
            build_oneshot_argv(agents::get_agent("opencode").unwrap(), "x").unwrap()[1],
            "run"
        );
        assert_eq!(
            build_oneshot_argv(agents::get_agent("gemini").unwrap(), "x").unwrap()[1],
            "-p"
        );
    }

    #[test]
    fn build_prompt_truncates_and_strips_nul() {
        let msg = format!("start{}\u{0}end", "x".repeat(5000));
        let p = build_prompt(&msg);
        assert!(p.contains("start"));
        assert!(!p.contains('\u{0}'));
        // Instruction + capped body, well under message length.
        assert!(p.len() < 5000 + INSTRUCTION.len() + 64);
    }

    #[test]
    fn sanitize_plain_title() {
        assert_eq!(
            sanitize_title("Fix login bug", "whatever").as_deref(),
            Some("Fix login bug")
        );
    }

    #[test]
    fn sanitize_strips_quotes_markdown_punctuation() {
        assert_eq!(
            sanitize_title("**\"Refactor auth module.\"**", "x").as_deref(),
            Some("Refactor auth module")
        );
        assert_eq!(
            sanitize_title("- Update README", "x").as_deref(),
            Some("Update README")
        );
        assert_eq!(
            sanitize_title("1. Add dark mode", "x").as_deref(),
            Some("Add dark mode")
        );
    }

    #[test]
    fn sanitize_picks_last_qualifying_line_from_verbose_output() {
        let raw = "[2024] booting agent\nthinking...\nWire up websockets\n";
        assert_eq!(
            sanitize_title(raw, "x").as_deref(),
            Some("Wire up websockets")
        );
    }

    #[test]
    fn sanitize_strips_ansi() {
        let raw = "\u{1b}[32mGreen title here\u{1b}[0m";
        assert_eq!(
            sanitize_title(raw, "x").as_deref(),
            Some("Green title here")
        );
    }

    #[test]
    fn sanitize_rejects_refusals_none_empty_and_echo() {
        assert!(sanitize_title("I cannot help with that", "x").is_none());
        assert!(sanitize_title("Sorry, no.", "x").is_none());
        assert!(sanitize_title("NONE", "x").is_none());
        assert!(sanitize_title("   \n  ", "x").is_none());
        assert!(sanitize_title("fix the thing", "fix the thing").is_none());
    }

    #[test]
    fn sanitize_rejects_too_long_or_wordy() {
        assert!(sanitize_title("a ".repeat(20).trim(), "x").is_none());
        assert!(sanitize_title(&"z".repeat(80), "x").is_none());
        // Numeric-only is not a title.
        assert!(sanitize_title("12345", "x").is_none());
    }

    // Regression for #2351: pins the shared helper that both `try_smart_rename`
    // and the sidebar indicator overlay in `src/server/api/sessions.rs` route
    // through. The helper is verified in isolation here; call-site coverage is
    // design-level (reverting either site to bypass the helper is visible in
    // review because both explicitly name `resolve_smart_rename_config`).
    #[test]
    #[serial_test::serial]
    fn resolve_smart_rename_config_honors_repo_local_overrides() {
        let home = tempfile::tempdir().expect("tempdir HOME");
        // SAFETY: serialized by `#[serial]`; matches `set_tmp_home` in
        // `src/session/mcp_state.rs`.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
        }

        let repo = tempfile::tempdir().expect("tempdir repo");
        let cfg_dir = repo.path().join(".agent-of-empires");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"
[session]
smart_rename_agent = "opencode"

[session.agent_command_override]
claude = "my-wrapper"
"#,
        )
        .unwrap();

        let resolved =
            crate::session::repo_config::resolve_config_with_repo_or_warn("default", repo.path());
        let cfg = resolve_smart_rename_config(&resolved.session);
        assert_eq!(cfg.rename_agent, "opencode");
        assert_eq!(
            cfg.overrides.get("claude").map(String::as_str),
            Some("my-wrapper"),
        );

        let agent = check_eligible_resolved(
            true,
            cfg.setting_on,
            "Vikings",
            "claude",
            cfg.rename_agent,
            false,
            "",
            cfg.overrides,
        )
        .expect("eligible");
        assert_eq!(agent.binary, "opencode");
    }
}
