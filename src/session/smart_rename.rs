//! Automatic "smart" rename of a structured-view (ACP) session from its first
//! message.
//!
//! When a session still carries its auto-generated civilization name (see
//! [`crate::session::civilizations`]) and the user sends a first prompt, the
//! session's own agent is run once in non-interactive one-shot mode (e.g.
//! `claude -p`) to produce a short title, and the session is renamed. This is
//! best-effort and fire-and-forget: it never blocks or fails the user's prompt,
//! and any failure leaves the generated name in place.
//!
//! Title only: the worktree directory is intentionally not moved. The live ACP
//! worker holds the worktree as its working directory, so a directory move
//! would fail exactly like a manual rename of a running tied session does. The
//! visible session title is what gains meaning here.

use crate::agents;
use crate::session::civilizations::is_default_civ_name;
use serde::Serialize;

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
Respond with ONLY the title: no quotes, no markdown, no surrounding punctuation, no explanation. \
If you cannot, respond with exactly NONE.";

/// Build the prompt string for the one-shot title call: the fixed instruction
/// plus the (NUL-stripped, trimmed, byte-capped) first user message.
pub fn build_prompt(user_message: &str) -> String {
    let sanitized = user_message.replace('\0', " ");
    let trimmed = sanitized.trim();
    let capped = truncate_bytes(trimmed, MAX_PROMPT_BYTES);
    format!("{INSTRUCTION}\n\nTask:\n{capped}")
}

/// Build the argv for a one-shot title call, or `None` when the agent has no
/// known one-shot mode. Always `[binary, oneshot_token, prompt]`: the prompt is
/// a single argv element passed straight to the process, never interpolated
/// into a shell string, so untrusted user text cannot inject arguments.
pub fn build_oneshot_argv(agent: &agents::AgentDef, prompt: &str) -> Option<Vec<String>> {
    let token = agent.oneshot_flag?;
    Some(vec![
        agent.binary.to_string(),
        token.to_string(),
        prompt.to_string(),
    ])
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
fn strip_ansi(s: &str) -> String {
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

fn truncate_bytes(s: &str, max: usize) -> &str {
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
pub use serve::try_smart_rename;

#[cfg(feature = "serve")]
mod serve {
    use super::*;
    use crate::server::AppState;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const ONESHOT_TIMEOUT: Duration = Duration::from_secs(12);

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
            if !guard.insert(id.to_string()) {
                return None;
            }
            Some(Self {
                set,
                id: id.to_string(),
            })
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
    /// message. Spawn this detached from the prompt handler; it never returns an
    /// error and never touches the prompt flow. All gates are re-checked under
    /// the per-session lock before the title is written, so a manual rename (or
    /// a deletion) that lands during the one-shot call always wins.
    pub async fn try_smart_rename(state: Arc<AppState>, session_id: String, first_message: String) {
        if first_message.trim().is_empty() {
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

        let config = crate::session::profile_config::resolve_config_or_warn(&profile);
        let agent = agents::get_agent(&tool);
        let command_override_in_cfg = config.session.agent_command_override.contains_key(&tool);
        if let Err(reason) = check_eligible(
            structured,
            config.session.smart_rename,
            &title,
            agent,
            sandboxed,
            &command,
            command_override_in_cfg,
        ) {
            tracing::debug!(target: "smart_rename", session = %session_id, tool = %tool, reason = reason.as_str(), "skip");
            return;
        }
        let agent = agent.expect("check_eligible Ok implies a built-in agent");

        let Some(_guard) = InflightGuard::acquire(&state.smart_rename_inflight, &session_id) else {
            return;
        };

        // One attempt per session lifetime. A failed or unusable first try
        // leaves the name default; without this guard every later prompt would
        // respawn a one-shot agent (12s + tokens) until one happened to succeed.
        {
            let mut attempted = state
                .smart_rename_attempted
                .lock()
                .expect("smart_rename_attempted poisoned");
            if !attempted.insert(session_id.clone()) {
                return;
            }
        }

        let prompt = build_prompt(&first_message);
        let Some(argv) = build_oneshot_argv(agent, &prompt) else {
            return;
        };

        let Some(raw) = run_oneshot(&argv, &project_path).await else {
            return;
        };
        let Some(new_title) = sanitize_title(&raw, &first_message) else {
            tracing::debug!(target: "smart_rename", session = %session_id, "skip: agent output not a usable title");
            return;
        };

        // Serialize against manual rename / worktree edits on this session.
        let lock = state.instance_lock(&session_id).await;
        let _serialized = lock.lock().await;
        apply_title_only(&state, &session_id, &profile, &new_title).await;
    }

    /// Run the agent one-shot in the session's working directory, capturing
    /// stdout. Returns `None` on spawn error, non-zero exit, or timeout. The
    /// child is killed on drop, so a timed-out call leaves no orphan.
    async fn run_oneshot(argv: &[String], cwd: &str) -> Option<String> {
        use tokio::process::Command;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if !cwd.is_empty() {
            cmd.current_dir(cwd);
        }
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(target: "smart_rename", "one-shot spawn failed: {e}");
                return None;
            }
        };
        match tokio::time::timeout(ONESHOT_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(out)) if out.status.success() => {
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(Ok(out)) => {
                tracing::debug!(target: "smart_rename", "one-shot exited {:?}", out.status.code());
                None
            }
            Ok(Err(e)) => {
                tracing::debug!(target: "smart_rename", "one-shot io error: {e}");
                None
            }
            Err(_) => {
                tracing::debug!(target: "smart_rename", "one-shot timed out");
                None
            }
        }
    }

    /// Persist the new title (re-checking the still-default gate under the
    /// storage lock) then mirror it into the in-memory instance list so
    /// connected clients see it without waiting for a reload.
    async fn apply_title_only(state: &Arc<AppState>, id: &str, profile: &str, new_title: &str) {
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
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id_owned) {
                    if is_default_civ_name(&inst.title) {
                        inst.title = title_owned.clone();
                    }
                }
                Ok(())
            })
        })
        .await;
        match persisted {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(target: "smart_rename", session = %id, "persist failed: {e}");
                return;
            }
            Err(e) => {
                tracing::warn!(target: "smart_rename", session = %id, "persist join failed: {e}");
                return;
            }
        }

        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            if is_default_civ_name(&inst.title) {
                tracing::info!(target: "smart_rename", session = %id, old = %inst.title, new = %new_title, "renamed session from first message");
                inst.title = new_title.to_string();
            }
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
}
