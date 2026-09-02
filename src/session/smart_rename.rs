//! Automatic "smart" rename of a structured-view (ACP) session from its first
//! turn.
//!
//! When a session still carries its auto-generated civilization name (see
//! [`crate::session::civilizations`]) the session's own agent is run once in
//! non-interactive one-shot mode (e.g. `claude -p`) to produce a short title,
//! and the session is renamed. The one-shot fires at turn-end and summarizes
//! the whole first turn (prompt plus agent output), so it never races the
//! live worker for the provider API (#2348). This is best-effort and
//! fire-and-forget: it never blocks or fails the user's prompt, and any
//! failure leaves the generated name in place.
//!
//! Title only: the worktree directory is intentionally not moved. The live ACP
//! worker holds the worktree as its working directory, so a directory move
//! would fail exactly like a manual rename of a running tied session does. The
//! visible session title is what gains meaning here.

use crate::agents;
use crate::session::civilizations::is_default_civ_name;
use crate::session::config::SessionConfig;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// Sandboxed at all. Smart rename runs inside the container instead, so only
    /// `session::conversation_summary` still reports this.
    Sandboxed,
    /// Sandboxed, with a utility agent other than the session's own.
    SandboxRenameAgentMismatch,
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
            SkipReason::SandboxRenameAgentMismatch => "sandbox_rename_agent_mismatch",
            SkipReason::NoOneshot => "no_oneshot",
            SkipReason::CommandOverridden => "command_overridden",
        }
    }

    /// The reason phrased for a user. Shared by the web endpoint's response and
    /// the TUI's "Auto-name now" dialog so the two surfaces cannot word the same
    /// skip differently.
    pub fn user_message(self) -> &'static str {
        match self {
            SkipReason::NotStructured => "Session is not a structured-view session",
            SkipReason::Disabled => "Smart rename is disabled in settings",
            SkipReason::NameNotDefault => "Session already has a custom name",
            SkipReason::Sandboxed => "Not available for sandboxed sessions",
            SkipReason::SandboxRenameAgentMismatch => {
                "A sandboxed session can only be auto-named by its own agent, because only that agent's credentials are mounted in the container"
            }
            SkipReason::NoOneshot => "The smart-rename agent has no one-shot mode",
            SkipReason::CommandOverridden => "The smart-rename agent's command is overridden",
        }
    }
}

/// Single source of truth for "is this session eligible to be auto-named right
/// now". `Ok(())` means a first prompt would trigger a rename; `Err` carries the
/// disqualifying reason. `command_override_in_cfg` is whether the profile config
/// replaces this agent's binary; `command` is the instance's launch command (a
/// non-empty value differing from the agent binary is also an override).
///
/// Sandbox state is not an input here: a sandboxed session runs its one-shot
/// inside its own container. The one sandbox rule that does disqualify (a rename
/// agent other than the session's own) needs both tool names, so it lives in
/// [`check_eligible_resolved`].
pub fn check_eligible(
    structured: bool,
    setting_on: bool,
    title: &str,
    agent: Option<&agents::AgentDef>,
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
///
/// `sandboxed` gates one rule: a sandboxed session's one-shot runs inside that
/// session's container, and `build_container_config` mounts only the SESSION
/// agent's credential dir there, so a different rename agent would find its
/// binary (the sandbox image ships them all) and then fail to authenticate. Left
/// ungated it would fail on every turn forever, since a non-zero exit
/// deliberately leaves the session un-attempted so a later turn retries.
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
        command,
        command_override_in_cfg,
    )?;
    // After the generic checks, so an unknown `smart_rename_agent` still reports
    // NoOneshot rather than implying a mounted-credentials problem.
    if sandboxed && rename_tool != session_tool {
        return Err(SkipReason::SandboxRenameAgentMismatch);
    }
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
    pub rename_model: &'a HashMap<String, String>,
}

/// Input for a one-shot title call. `context` is what the agent summarizes
/// (the rendered first-turn transcript for the turn-end fire; the manual
/// "Auto-name now" action passes whatever context it has).
/// `first_user_prompt` is kept separately as the echo baseline so
/// [`sanitize_title`] rejects a title that merely parrots the raw prompt,
/// even when `context` wraps that prompt in a `User:`/`Agent:` frame.
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
        rename_model: &session.smart_rename_model,
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
The transcript may begin with the CLI tool's startup banner, welcome message, tips, or help \
text; ignore that boilerplate and title the user's actual request and the work done, never the \
tool's own introduction. \
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

/// What a one-shot argv targets. `Title(model_args)` is a throwaway
/// smart-rename title: it injects the already-resolved model selector tokens
/// (empty = the CLI's own model). `CliDefault` is a whole-transcript
/// conversation summary that always runs the agent's normal (bigger) model. It
/// deliberately has no zero-value, so every call site states its intent: a
/// title cannot silently bill the frontier model and a summary cannot silently
/// be downgraded to the cheap tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneshotModel {
    Title(Vec<String>),
    CliDefault,
}

/// Resolve the model-selector tokens (`[flag, model_id]` or empty) for a title
/// one-shot from the per-agent `smart_rename_model` map. Three-state: an absent
/// key uses the agent's built-in cheap default (claude pins `haiku`); an empty
/// value forces the CLI default (opt out of the cheap alias); a non-empty value
/// pins that model. An agent with no model flag (or no default and no override)
/// yields no tokens, i.e. the CLI default. This is the sole entry point for the
/// title-vs-CLI-default decision, so a caller must pass the raw map through: the
/// empty-string state must not be collapsed in the call chain (e.g. by stripping
/// empty values before this call). The web widget cannot emit an empty value (it
/// removes the key on clear), so that state is reachable only via the TUI or
/// config.toml.
pub fn resolve_title_model_args(
    agent: &agents::AgentDef,
    models: &HashMap<String, String>,
) -> Vec<String> {
    let Some(flag) = agent.oneshot_model_flag() else {
        return Vec::new();
    };
    let model = match models.get(agent.name).map(|m| m.trim()) {
        Some("") => return Vec::new(),
        Some(id) => id.to_string(),
        None => match agent.oneshot_cheap_model() {
            Some(default) => default.to_string(),
            None => return Vec::new(),
        },
    };
    vec![flag.to_string(), model]
}

/// Build the argv for a one-shot title or summary call, or `None` when the
/// agent has no known one-shot mode. Shape is `[binary, oneshot_token, model..,
/// extra.., prompt, trailing..]`, where `model..` (for a `Title`) sits before
/// the prompt for a positional-prompt flag but AFTER the prompt for a
/// value-binding flag (copilot, gemini, kimi `-p`, whose value is the prompt),
/// so the flag can never bind the model selector as the prompt. The prompt is a
/// single argv element passed straight to the process, never interpolated into
/// a shell string, so untrusted user text cannot inject arguments.
/// `oneshot_trailing_args` is only populated for value-binding one-shots, where
/// the CLI has already bound the prompt to the flag, so trailing flags stay
/// unambiguous.
pub fn build_oneshot_argv(
    agent: &agents::AgentDef,
    prompt: &str,
    model: OneshotModel,
) -> Option<Vec<String>> {
    let token = agent.oneshot_flag?;
    let mut argv = vec![agent.binary.to_string(), token.to_string()];
    let model_args = match model {
        OneshotModel::Title(args) => args,
        OneshotModel::CliDefault => Vec::new(),
    };
    let binds_prompt = agent.oneshot_flag_binds_prompt();
    if !binds_prompt {
        argv.extend(model_args.iter().cloned());
    }
    argv.extend(agent.oneshot_extra_args().iter().map(|s| s.to_string()));
    argv.push(prompt.to_string());
    if binds_prompt {
        argv.extend(model_args.iter().cloned());
    }
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

// Since #2348 the ACP one-shot is deferred to the first `prompt_complete`
// `Event::Stopped`, so it no longer races the live worker for the same
// provider API. The terminal path (below) fires only after the poller sees the
// pane go idle, so it likewise runs post-turn. Standalone the call finishes
// well under 12s; 60s is a conservative ceiling that leaves headroom for cold
// agent starts.
pub(crate) const ONESHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run the agent one-shot in the session's working directory, capturing
/// stdout. Returns `None` on spawn error, non-zero exit, or timeout. Shared
/// with the serve ACP path, the terminal `__smart-rename` runner, and
/// `session::conversation_summary` (which passes a longer `timeout` for its
/// larger transcript input).
///
/// The child is killed on drop, so a timed-out HOST call leaves no orphan. For
/// a sandboxed session (see [`resolve_oneshot_target`]) the child is the
/// container runtime client, and killing it does not kill the agent process the
/// `exec` started inside the container.
// ponytail: a hung in-container one-shot outlives its 60s timeout and is only
// reaped when the container goes down. Bounding it needs an in-container
// `timeout`, which is not in the sandbox-image contract (a custom image without
// coreutils would exit 127 and lose the feature instead). Follow-up, not fixed
// here.
pub(crate) async fn run_oneshot(
    session_id: &str,
    argv: &[String],
    cwd: &str,
    timeout: std::time::Duration,
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

/// Where a one-shot runs for this session: the argv to spawn and the host
/// working directory to spawn it in (empty for a container, whose workdir comes
/// from the `exec` itself).
pub(crate) struct OneshotTarget {
    pub argv: Vec<String>,
    pub cwd: String,
}

/// Resolve the spawn target for a session's one-shot: unchanged on the host,
/// wrapped in a container `exec` when the session is sandboxed.
///
/// The agent runs in the container, so its binary and the credential dir the
/// sandbox mounts for it are the ones the pane already uses; a host spawn would
/// reach neither. `None` means the container cannot take an `exec` right now
/// (stopped, absent, or the runtime could not be inspected). That is transient,
/// so the caller must leave the session un-attempted and let a later turn retry
/// rather than burning its one attempt. A stopped container is never started
/// just to name a session.
pub(crate) async fn resolve_oneshot_target(
    session_id: &str,
    sandboxed: bool,
    container_workdir: &str,
    project_path: &str,
    argv: Vec<String>,
) -> Option<OneshotTarget> {
    if !sandboxed {
        return Some(OneshotTarget {
            argv,
            cwd: project_path.to_string(),
        });
    }
    // `docker inspect` blocks; keep it off the caller's runtime thread, mirroring
    // the sandbox install path in `server::api::acp::install_in_container`.
    let sid = session_id.to_string();
    let probed = tokio::task::spawn_blocking(move || {
        let container = crate::containers::DockerContainer::from_session_id(&sid);
        (container.probe_running(), container)
    })
    .await;
    let (probe, container) = match probed {
        Ok(pair) => pair,
        Err(e) => {
            tracing::debug!(target: "smart_rename", session = %session_id, "container probe task failed: {e}");
            return None;
        }
    };
    match probe {
        crate::containers::Probe::Running => Some(OneshotTarget {
            argv: container.build_exec_argv(container_workdir, &argv),
            cwd: String::new(),
        }),
        crate::containers::Probe::NotRunning => {
            tracing::debug!(target: "smart_rename", session = %session_id, "skip: sandbox container is not running");
            None
        }
        crate::containers::Probe::Unknown(e) => {
            tracing::debug!(target: "smart_rename", session = %session_id, "skip: sandbox container state unknown: {e}");
            None
        }
    }
}

/// Whether an automatic renamer may overwrite this session's title: either it
/// is still a default civ name (never explicitly set), or it still matches the
/// last title an auto renamer wrote. A manual rename leaves `title` diverged
/// from `last_auto_title`, which freezes it against auto writes.
pub(crate) fn title_is_auto_overwritable(inst: &crate::session::instance::Instance) -> bool {
    is_default_civ_name(&inst.title) || inst.last_auto_title.as_deref() == Some(inst.title.as_str())
}

// ---------------------------------------------------------------------------
// Terminal (non-ACP) smart rename.
//
// ACP sessions rename from a typed turn-boundary event inside the daemon.
// Terminal sessions have no such event and no clean first-message chokepoint
// (native `tmux attach` types straight into the pane, invisible to AoE at
// input time). Instead the status poller (both the TUI's and the daemon's)
// fires `maybe_spawn_terminal_smart_rename` on the `Running -> Idle` edge of a
// still-default-named session: that single edge covers every input path
// (native attach, web live-view, `aoe send`) and fires only once the pane
// agent is idle, so the one-shot never races it for the provider API. The work
// runs in a detached `aoe __smart-rename` child so it never blocks the poller
// and is identical in the TUI-only and serve builds. Cross-process guards (a
// per-session advisory lock, MAX_CONCURRENT global slot locks, and the
// persisted `Instance.smart_rename_attempted` marker) coordinate the TUI, the
// daemon, and sibling children, which are all separate processes.
// ---------------------------------------------------------------------------

/// Head/tail byte budgets for the captured first-turn transcript handed to the
/// one-shot. The user's opening intent sits near the top and the agent's
/// result near the bottom, so a middle-elided head+tail keeps both while
/// staying well under `MAX_PROMPT_BYTES`.
const CONTEXT_HEAD_BYTES: usize = 3072;
const CONTEXT_TAIL_BYTES: usize = 1024;

/// Cheap poll-hot-path gate: fire a detached terminal rename for this session
/// iff it is a still-default-named, non-structured, not-yet-attempted session
/// whose resolved config has smart rename on. The full eligibility check
/// (one-shot support, command override, sandbox rename-agent match) runs in the
/// detached child, which re-reads storage so it can never act on the stale
/// snapshot the poller held. Called from both status pollers on the
/// `Running -> Idle` edge.
pub fn maybe_spawn_terminal_smart_rename(inst: &crate::session::instance::Instance) {
    if inst.is_structured() || inst.smart_rename_attempted || !is_default_civ_name(&inst.title) {
        return;
    }
    // Resolve config and run the FULL eligibility check on the (rare)
    // turn-completion edge, never per tick. Doing the whole check here (not just
    // the setting) matters: an ineligible session (disabled, no one-shot,
    // overridden command, or a sandbox rename-agent mismatch) never marks itself
    // attempted, so a cheaper gate would re-fork a child on every later turn. A
    // sandboxed session whose container is down is filtered later, in the
    // child's resolve_oneshot_target, so it can retry when the container comes
    // back. The child re-checks against fresh storage anyway, so this is a
    // fork-avoidance filter, not the authority.
    let resolved = crate::session::repo_config::resolve_config_with_repo_or_warn(
        &inst.source_profile,
        Path::new(&inst.project_path),
    );
    let cfg = resolve_smart_rename_config(&resolved.session);
    if check_eligible_resolved(
        true,
        cfg.setting_on,
        &inst.title,
        &inst.tool,
        cfg.rename_agent,
        inst.is_sandboxed(),
        &inst.command,
        cfg.overrides,
    )
    .is_err()
    {
        return;
    }
    spawn_detached(&inst.source_profile, &inst.id, false);
}

/// Spawn an on-demand terminal rename for a session, forcing past the
/// `smart_rename`-disabled gate. The manual TUI "Auto-name now" action calls
/// this for a still-default-named terminal session; the detached child re-reads
/// storage and re-checks every other gate, so this never acts on a stale
/// snapshot (#3039).
pub fn spawn_smart_rename_now(profile: &str, session_id: &str) {
    spawn_detached(profile, session_id, true);
}

/// Re-exec `aoe __smart-rename [--force] <profile> <id>` as a detached child (setsid,
/// null stdio, dropped handle), mirroring the `__acp-runner` launcher. Never
/// blocks and never fails the caller.
fn spawn_detached(profile: &str, session_id: &str, force: bool) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__smart-rename");
    if force {
        cmd.arg("--force");
    }
    cmd.arg(profile).arg(session_id);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and the closure touches no other
        // state; matches the __acp-runner detach.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }
    }
    match cmd.spawn() {
        // The child is setsid-detached and does its own work; we only need to
        // reap it so it does not linger as a zombie in the long-lived poller
        // process (unlike __acp-runner, this child exits quickly). A short
        // dedicated thread waits for it, then ends.
        Ok(child) => {
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
        }
        Err(e) => {
            tracing::debug!(target: "smart_rename", session = %session_id, "terminal rename spawn failed: {e}");
        }
    }
}

/// Directory holding the advisory lock files, under the app data dir so the
/// path is identical across the TUI, the daemon, and detached children in the
/// same build namespace.
fn lock_dir() -> Option<PathBuf> {
    let dir = crate::session::get_app_dir().ok()?.join("smart-rename");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Try to take an exclusive advisory (`flock`) lock on `path`, returning the
/// held file on success. Dropping the returned file releases the lock, so a
/// crash also releases it (unlike a `create_new` sentinel).
fn try_lock(path: &Path) -> Option<std::fs::File> {
    use fs2::FileExt;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .ok()?;
    f.try_lock_exclusive().ok().map(|()| f)
}

/// Non-blocking per-session lock: prevents the TUI and daemon (and repeated
/// poll ticks) from running concurrent one-shots for one session.
fn try_session_lock(id: &str) -> Option<std::fs::File> {
    try_lock(&lock_dir()?.join(format!("{id}.lock")))
}

/// Non-blocking global slot lock preserving `MAX_CONCURRENT` across processes,
/// so a burst of sessions going idle at once (e.g. a batch launch) cannot fan
/// out into one host agent process per session.
fn try_global_slot() -> Option<std::fs::File> {
    let dir = lock_dir()?;
    (0..MAX_CONCURRENT).find_map(|n| try_lock(&dir.join(format!("slot-{n}.lock"))))
}

/// Capture the pane's full first-turn transcript and reduce it to a bounded,
/// middle-elided head+tail block, or `None` when the capture is empty or looks
/// like garbage (so the caller keeps the civ name without paying for a
/// one-shot).
fn capture_terminal_context(tmux: &crate::tmux::Session, tool: &str) -> Option<String> {
    let raw = tmux.capture_pane_full().ok()?;
    let cleaned = strip_ansi(&raw);
    let stripped = strip_agent_banner(&cleaned, tool);
    let trimmed = stripped.trim();
    if !context_looks_usable(trimmed) {
        return None;
    }
    Some(head_tail(trimmed, CONTEXT_HEAD_BYTES, CONTEXT_TAIL_BYTES))
}

/// CLI agents print a startup banner (a welcome box plus "getting started"
/// tips) on launch. It dominates the pane head, so a first-turn capture ends up
/// summarizing the banner instead of the task (Claude Code -> "claude code
/// getting started"). Best-effort: for agents we recognize, drop the leading
/// run of banner lines. It only acts when the banner's signature marker is
/// present, and falls back to the original text if stripping would leave nothing
/// substantive, so it can never make the capture worse. The `INSTRUCTION` prose
/// is the backstop for banners this misses or for other agents.
fn strip_agent_banner(text: &str, tool: &str) -> String {
    // Only Claude Code has a verified banner shape to key on. Other agents rely
    // on the instruction prose; add a case here per agent as needed.
    if !tool.eq_ignore_ascii_case("claude") {
        return text.to_string();
    }
    // Gate loosely: the startup box names the tool ("Claude Code v2.1.216" in
    // its top border). A false positive is harmless because the line filter
    // below only strips an actual leading run of box chrome, so a task that
    // merely mentions Claude Code (with no leading box) loses nothing.
    if !text.to_lowercase().contains("claude code") {
        return text.to_string();
    }
    let mut in_banner = true;
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        if in_banner && is_claude_banner_line(line) {
            continue;
        }
        in_banner = false;
        kept.push(line);
    }
    let stripped = kept.join("\n");
    // Guard against eating the whole transcript (a pane that was nothing but
    // banner, or a future banner shape that trips the heuristic): keep the
    // original when stripping leaves too little to title.
    if stripped.chars().filter(|c| c.is_alphabetic()).count() < 12 {
        return text.to_string();
    }
    stripped
}

/// Whether a line belongs to the Claude Code startup banner. The banner is a
/// box: its borders are box-drawing glyphs, every body row opens with a vertical
/// box edge, and its logo uses block-element glyphs, so a structural test
/// captures the whole box regardless of the (version-dependent) wording inside.
/// The `MARKERS` cover the notices printed just under the box (MCP-auth warning,
/// tips, "what's new") before the REPL settles. Blank lines inside the leading
/// block count so the run isn't cut short by the gaps between sections.
///
/// Verified against Claude Code v2.1.216, whose banner is a two-column box
/// titled `Claude Code v<ver>` with "Welcome back <name>!", an ASCII logo, and a
/// tips / what's-new column, followed by a `⚠ N MCP servers ...` notice.
fn is_claude_banner_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true;
    }
    // Box-drawing (U+2500..=U+257F) borders/edges and block-element (U+2580..=
    // U+259F) logo glyphs.
    let is_chrome = |c: char| ('\u{2500}'..='\u{259F}').contains(&c);
    if t.chars().all(|c| is_chrome(c) || c.is_whitespace()) {
        return true;
    }
    if t.starts_with(|c: char| is_chrome(c) || c == '|') {
        return true;
    }
    let lc = t.to_lowercase();
    const MARKERS: &[&str] = &[
        "claude code v",
        "welcome to claude code",
        "welcome back",
        "tips for getting started",
        "what's new",
        "/release-notes",
        "run /mcp",
        "need authentication",
        "/help for help",
    ];
    if MARKERS.iter().any(|m| lc.contains(m)) {
        return true;
    }
    // Startup notice / tip callouts: ⚠ (U+26A0) and ※ (U+203B).
    if t.starts_with('\u{26A0}') || t.starts_with('\u{203B}') || lc.starts_with("tip:") {
        return true;
    }
    // Numbered tip: "1. ..." or "2) ...".
    let mut chars = t.chars();
    matches!((chars.next(), chars.next()), (Some(d), Some(p)) if d.is_ascii_digit() && (p == '.' || p == ')'))
}

/// Reject a pane capture that is empty, has no letters, or is dominated by
/// control characters (a garbled/binary pane). Syntactic only: a semantically
/// useless capture can still pass here and is caught later by `sanitize_title`.
fn context_looks_usable(s: &str) -> bool {
    if s.is_empty() || !s.chars().any(|c| c.is_alphabetic()) {
        return false;
    }
    let control = s
        .chars()
        .filter(|c| c.is_control() && *c != '\n' && *c != '\t')
        .count();
    let total = s.chars().count().max(1);
    control * 100 / total < 30
}

/// Bounded head + `...` + tail on char boundaries. Whole string if it already
/// fits.
fn head_tail(s: &str, head: usize, tail: usize) -> String {
    if s.len() <= head + tail {
        return s.to_string();
    }
    let h = truncate_bytes(s, head);
    let mut start = s.len().saturating_sub(tail);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("{h}\n...\n{}", &s[start..])
}

/// First non-empty line of the transcript, the best proxy for the user's
/// opening message, used only as the echo baseline so `sanitize_title` rejects
/// a title that merely parrots the prompt.
fn extract_echo_baseline(context: &str) -> String {
    context
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Persist the outcome of a terminal one-shot. Always marks the session
/// attempted (the one-shot returned output, usable or not, so we must not
/// respawn on every later turn), and writes the new title only while the
/// current title is still auto-overwritable, so a manual rename that landed
/// during the one-shot always wins.
fn apply_terminal_title(
    storage: &crate::session::storage::Storage,
    id: &str,
    new_title: Option<&str>,
) -> anyhow::Result<()> {
    let id = id.to_string();
    let new_title = new_title.map(str::to_string);
    let identity_lock = crate::session::acquire_session_identity_lock()?;
    let _session_title_lock = crate::session::storage::acquire_session_title_lock(&id)?;
    let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(&id)?;
    let rekey = storage.update(|instances, _groups| {
        let mut rekey = None;
        if let Some(index) = instances.iter().position(|instance| instance.id == id) {
            instances[index].smart_rename_attempted = true;
            if let Some(title) = &new_title {
                let should_write = title_is_auto_overwritable(&instances[index])
                    && instances[index].title != *title;
                // Manual and automatic rename paths share one domain predicate;
                // exclude this row explicitly so the uniqueness contract does
                // not depend on `should_write` remaining title-sensitive.
                let path = instances[index].project_path.clone();
                let duplicate = should_write
                    && crate::session::is_duplicate_session(
                        instances.iter(),
                        title,
                        &path,
                        Some(&id),
                    );
                if duplicate {
                    tracing::warn!(target: "smart_rename", session = %id, title = %title, "skipped duplicate auto-title");
                } else if should_write {
                    let instance = &mut instances[index];
                    rekey = Some((instance.title.clone(), title.clone()));
                    tracing::info!(target: "smart_rename", session = %id, old = %instance.title, new = %title, "auto-renamed terminal session");
                    instance.title = title.clone();
                    instance.last_auto_title = Some(title.clone());
                }
            }
        }
        Ok(rekey)
    })?;
    drop(identity_lock);
    if let Some((old_title, new_title)) = rekey {
        if let Err(error) = crate::tmux::rekey_session(&id, &old_title, &new_title) {
            tracing::warn!(target: "smart_rename", session = %id, "tmux rename failed: {error}");
        }
    }
    Ok(())
}

/// Entry point for the detached `aoe __smart-rename [--force] <profile> <id>`
/// child. Routes by session kind: a structured (ACP) session renames through
/// the daemon's `/smart-rename` endpoint (its title lives behind the ACP
/// worker, not a tmux pane), a terminal session runs the local one-shot below.
/// `force` bypasses the `smart_rename`-disabled gate for the manual on-demand
/// action (#3039); the poller passes `false`. Best-effort: every failure leaves
/// the generated name in place.
pub async fn run_smart_rename_now(
    profile: &str,
    session_id: &str,
    force: bool,
) -> anyhow::Result<()> {
    let storage = crate::session::storage::Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    let structured = instances
        .iter()
        .find(|i| i.id == session_id)
        .map(|i| i.is_structured());
    drop(instances);
    match structured {
        // Structured sessions rename through the daemon, whose client lives
        // behind the `serve` feature. A non-serve (TUI-only) build has no
        // structured view and no daemon client, so there is nothing to do.
        Some(true) => {
            #[cfg(feature = "serve")]
            {
                rename_structured_via_daemon(session_id).await
            }
            #[cfg(not(feature = "serve"))]
            {
                let _ = session_id;
                Ok(())
            }
        }
        Some(false) => run_terminal_rename(profile, session_id, force).await,
        None => Ok(()),
    }
}

/// Ask the running daemon to (re-)run the smart-rename one-shot for a
/// structured session via `POST /api/sessions/{id}/smart-rename`. The endpoint
/// already forces past the disabled-setting gate. Best-effort: no daemon, or a
/// non-2xx response, just leaves the generated name in place.
#[cfg(feature = "serve")]
async fn rename_structured_via_daemon(session_id: &str) -> anyhow::Result<()> {
    use crate::acp::client::{discovery, HttpClient};
    let endpoint = match discovery::discover() {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(target: "smart_rename", session = %session_id, "no daemon for structured rename: {e}");
            return Ok(());
        }
    };
    let client = HttpClient::new(endpoint)?;
    if let Err(e) = client.smart_rename(session_id).await {
        tracing::debug!(target: "smart_rename", session = %session_id, "structured smart-rename request failed: {e}");
    }
    Ok(())
}

/// Body of the terminal (non-ACP) smart rename. Best-effort and
/// fire-and-forget: every early return leaves the civ name in place. All
/// gates are re-checked against freshly loaded storage so a rename that landed
/// (or a deletion, or a config change) since the poller observed the edge
/// always wins. `force` bypasses only the `smart_rename`-disabled gate (#3039).
pub async fn run_terminal_rename(
    profile: &str,
    session_id: &str,
    force: bool,
) -> anyhow::Result<()> {
    // Per-session lock first: if another process is already handling this
    // session, exit immediately (do not queue).
    let Some(_session_lock) = try_session_lock(session_id) else {
        return Ok(());
    };

    let storage = crate::session::storage::Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    let Some((
        title,
        tool,
        command,
        project_path,
        sandboxed,
        container_workdir,
        detect_as,
        already,
        structured,
    )) = instances.iter().find(|i| i.id == session_id).map(|i| {
        (
            i.title.clone(),
            i.tool.clone(),
            i.command.clone(),
            i.project_path.clone(),
            i.is_sandboxed(),
            i.container_workdir(),
            i.detect_as.clone(),
            i.smart_rename_attempted,
            i.is_structured(),
        )
    })
    else {
        return Ok(());
    };
    drop(instances);
    // Durable double-check: storage may have propagated a completed attempt (or
    // a manual rename) since the poller observed the edge. `force` (the manual
    // "Auto-name now") bypasses the attempted gate so a session whose automatic
    // one-shot already ran but produced no usable title can be re-run, mirroring
    // how the structured endpoint clears its attempted set. The NameNotDefault
    // gate below and title_is_auto_overwritable still protect a named session.
    if (already && !force) || structured {
        return Ok(());
    }

    let resolved = crate::session::repo_config::resolve_config_with_repo_or_warn(
        profile,
        Path::new(&project_path),
    );
    let cfg = resolve_smart_rename_config(&resolved.session);
    let agent = match check_eligible_resolved(
        // Terminal owned turns are an eligible session kind; the `structured`
        // gate exists only so the daemon's generic ACP listener skips
        // non-structured sessions, which does not apply to this deliberate
        // terminal trigger.
        true,
        cfg.setting_on || force,
        &title,
        &tool,
        cfg.rename_agent,
        sandboxed,
        &command,
        cfg.overrides,
    ) {
        Ok(agent) => agent,
        Err(reason) => {
            tracing::debug!(target: "smart_rename", session = %session_id, reason = reason.as_str(), "terminal skip");
            return Ok(());
        }
    };

    let tmux = crate::tmux::Session::new(session_id, &title)?;
    // Same resolution the status poller uses: own status rules first, then the
    // `agent_detect_as` alias, resolved through the live registry so a session
    // whose alias entry landed after it was created is not stranded on the raw
    // tool name here (which would skip `strip_agent_banner` and read the pane
    // with no detector).
    let detect_tool = crate::tmux::status_rules::detection_tool(profile, &tool, &detect_as);

    // Best-effort no-race: the poller fired on Running -> Idle, but the user may
    // have started another turn since. Only proceed while the pane still reads
    // idle; a later idle edge retries.
    if let Ok(content) = tmux.capture_pane(50) {
        if crate::tmux::detect_status_from_content_in(profile, &content, &detect_tool)
            == crate::session::Status::Running
        {
            return Ok(());
        }
    }

    let Some(context) = capture_terminal_context(&tmux, &detect_tool) else {
        tracing::debug!(target: "smart_rename", session = %session_id, "terminal skip: unusable pane capture");
        return Ok(());
    };

    // Global concurrency slot, taken only once real work is imminent so
    // early-return paths never hold one.
    let Some(_slot) = try_global_slot() else {
        return Ok(());
    };

    let baseline = extract_echo_baseline(&context);
    let prompt = build_prompt(&context);
    let model = OneshotModel::Title(resolve_title_model_args(agent, cfg.rename_model));
    let Some(argv) = build_oneshot_argv(agent, &prompt, model) else {
        return Ok(());
    };
    let Some(target) = resolve_oneshot_target(
        session_id,
        sandboxed,
        &container_workdir,
        &project_path,
        argv,
    )
    .await
    else {
        // Container not usable right now: transient, so leave the session
        // un-attempted for a later idle edge.
        return Ok(());
    };
    let Some(raw) = run_oneshot(session_id, &target.argv, &target.cwd, ONESHOT_TIMEOUT).await
    else {
        // Transient failure (spawn / timeout / non-zero exit): leave the session
        // un-attempted so a later turn can retry.
        return Ok(());
    };
    let new_title = sanitize_title(&raw, &baseline);
    apply_terminal_title(&storage, session_id, new_title.as_deref())?;
    Ok(())
}

#[cfg(feature = "serve")]
pub use serve::{should_trigger_smart_rename, try_smart_rename};

#[cfg(feature = "serve")]
mod serve {
    use super::*;
    use crate::server::AppState;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

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

    /// Whether a one-shot has already been attempted for this session this
    /// process lifetime. Shared by the turn-end firing site and the manual
    /// action so a fire after an attempt that already produced output no-ops.
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
    /// turn. Spawn this detached from a firing site (the daemon event
    /// listener at turn-end, or the manual "Auto-name now" action); it never
    /// returns an error and never touches the prompt flow. All gates are
    /// re-checked under the per-session lock before the title is written, so
    /// a manual rename (or a deletion) that lands during the one-shot call
    /// always wins.
    ///
    /// `force` bypasses only the `smart_rename`-disabled gate: the manual
    /// "Auto-name now" action runs on demand even when auto-rename-on-start is
    /// off (#3039), mirroring how the manual summary bypasses the
    /// `conversation_summary` setting (#2808). The automatic listener passes
    /// `false`; every other gate (structured, name-not-default, sandbox,
    /// one-shot support, command override) still applies on both paths.
    pub async fn try_smart_rename(
        state: Arc<AppState>,
        session_id: String,
        input: SmartRenameInput,
        force: bool,
    ) {
        if input.first_user_prompt.trim().is_empty() {
            return;
        }

        // Internal attempted gate. With two firing sites (the listener at
        // turn-end and the manual action), call-site gating alone is not
        // enough. A session that already produced a one-shot answer (even one
        // the sanitizer rejected) must not be retried.
        if attempted_contains(&state, &session_id) {
            return;
        }

        let Some((
            profile,
            tool,
            command,
            project_path,
            sandboxed,
            container_workdir,
            title,
            structured,
        )) = ({
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == session_id).map(|i| {
                (
                    i.source_profile.clone(),
                    i.tool.clone(),
                    i.command.clone(),
                    i.project_path.clone(),
                    i.is_sandboxed(),
                    i.container_workdir(),
                    i.title.clone(),
                    i.is_structured(),
                )
            })
        })
        else {
            return;
        };

        let resolved = crate::session::repo_config::resolve_config_with_repo_or_warn(
            &profile,
            Path::new(&project_path),
        );
        let cfg = resolve_smart_rename_config(&resolved.session);
        let agent = match check_eligible_resolved(
            structured,
            cfg.setting_on || force,
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
        let model = OneshotModel::Title(resolve_title_model_args(agent, cfg.rename_model));
        let Some(argv) = build_oneshot_argv(agent, &prompt, model) else {
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
        let Some(target) = resolve_oneshot_target(
            &session_id,
            sandboxed,
            &container_workdir,
            &project_path,
            argv,
        )
        .await
        else {
            // Container not usable right now: transient, so leave the session
            // un-attempted for a later turn.
            return;
        };
        let raw = {
            let Ok(_permit) = state.smart_rename_semaphore.acquire().await else {
                return;
            };
            run_oneshot(&session_id, &target.argv, &target.cwd, ONESHOT_TIMEOUT).await
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

    /// Persist an automatically generated title and mirror it into AppState.
    /// The title changes only while it remains auto-overwritable. The
    /// process-local instance lock, global identity lock, and per-session title
    /// lock serialize the write with manual renames. Identity covers duplicate
    /// validation through the durable write. The title lock remains held
    /// through the AppState mirror so a later writer cannot be overwritten in
    /// memory. Unchanged and duplicate titles are skipped.
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
        // Own copies for the closure below: `storage.update` runs inside
        // `spawn_blocking`, whose body must be `'static + Send`, so the borrowed
        // `id`/`new_title` cannot cross the thread boundary. The in-memory
        // mirror after the join reuses the borrowed `new_title` directly.
        let id_owned = id.to_string();
        let title_owned = new_title.to_string();
        let persisted = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let identity_lock = crate::session::acquire_session_identity_lock()?;
            let session_title_lock =
                crate::session::storage::acquire_session_title_lock(&id_owned)?;
            let wrote = storage.update(|instances, _groups| {
                let Some(index) = instances
                    .iter()
                    .position(|instance| instance.id == id_owned)
                else {
                    return Ok(false);
                };
                // Manual and automatic rename paths share one domain predicate;
                // exclude this row explicitly so a future no-op policy change
                // cannot make the row collide with itself.
                let should_write = title_is_auto_overwritable(&instances[index])
                    && instances[index].title != title_owned;
                let path = instances[index].project_path.clone();
                let duplicate = should_write
                    && crate::session::is_duplicate_session(
                        instances.iter(),
                        &title_owned,
                        &path,
                        Some(&id_owned),
                    );
                if duplicate {
                    tracing::warn!(target: "smart_rename", session = %id_owned, title = %title_owned, "skipped duplicate auto-title");
                    Ok(false)
                } else if should_write {
                    instances[index].title = title_owned.clone();
                    // Last owned use of `title_owned`: move it into the field
                    // rather than cloning a second time.
                    instances[index].last_auto_title = Some(title_owned);
                    Ok(true)
                } else {
                    Ok(false)
                }
            })?;
            drop(identity_lock);
            Ok((wrote, session_title_lock))
        })
        .await;
        let (wrote, _session_title_lock) = match persisted {
            Ok(Ok(result)) => result,
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::Duration;

        #[tokio::test]
        #[serial_test::serial]
        async fn apply_auto_title_skips_duplicate_title_and_path() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let storage =
                crate::session::storage::Storage::new_unwatched("default").expect("storage");
            let mut target = crate::session::Instance::new("Franks", "/tmp/shared/");
            target.source_profile = "default".to_string();
            let target_id = target.id.clone();
            let mut owner = crate::session::Instance::new("Already owned", "/tmp/shared");
            owner.source_profile = "default".to_string();
            storage
                .update(|instances, _groups| {
                    *instances = vec![target.clone(), owner.clone()];
                    Ok(())
                })
                .unwrap();
            let state = crate::server::test_support::build_test_app_state(vec![target, owner]);

            apply_auto_title(&state, &target_id, "default", "Already owned").await;

            let persisted = storage.load().unwrap();
            assert_eq!(
                persisted
                    .iter()
                    .find(|instance| instance.id == target_id)
                    .unwrap()
                    .title,
                "Franks"
            );
            let in_memory = state.instances.read().await;
            assert_eq!(
                in_memory
                    .iter()
                    .find(|instance| instance.id == target_id)
                    .unwrap()
                    .title,
                "Franks"
            );
        }

        #[test]
        fn is_duplicate_session_normalizes_trailing_slash() {
            // The skip in `apply_auto_title` / `apply_terminal_title` reuses the
            // creation predicate, which trims trailing '/' on both sides before
            // comparing paths. Pin that normalization explicitly: an existing
            // session at "/tmp/shared" collides with a candidate whose path
            // differs only by a trailing slash.
            let mut existing = crate::session::Instance::new("Already owned", "/tmp/shared");
            existing.source_profile = "default".to_string();
            let instances = [existing];
            let cases = [
                ("Already owned", "/tmp/shared", true),
                // "/tmp/shared/" and "/tmp/shared" are equal after trim_end_matches('/').
                ("Already owned", "/tmp/shared/", true),
                // trim_end_matches removes every trailing slash, not just one.
                ("Already owned", "/tmp/shared//", true),
                ("Already owned", "/tmp/other", false),
                // Same path, different title: not a duplicate.
                ("Different", "/tmp/shared", false),
            ];
            for (title, path, expected) in cases {
                assert_eq!(
                    crate::session::is_duplicate_session(instances.iter(), title, path, None),
                    expected,
                    "title {title:?} path {path:?}"
                );
            }
        }

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
                prompt_id: None,
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

    /// A title one-shot with no user override: the agent's built-in default
    /// (claude pins `haiku`, others none), reproducing the pre-tunable behavior.
    fn title_default(agent: &agents::AgentDef) -> OneshotModel {
        OneshotModel::Title(resolve_title_model_args(agent, &HashMap::new()))
    }

    #[test]
    fn argv_is_binary_token_prompt() {
        let argv = build_oneshot_argv(claude(), "hello", title_default(claude()))
            .expect("claude one-shot");
        assert_eq!(argv, vec!["claude", "-p", "--model", "haiku", "hello"]);
    }

    #[test]
    fn argv_none_for_agent_without_oneshot() {
        let cursor = agents::get_agent("cursor").expect("cursor agent exists");
        assert!(build_oneshot_argv(cursor, "hello", OneshotModel::CliDefault).is_none());
    }

    #[test]
    fn check_eligible_reasons() {
        let c = Some(claude());
        // Happy path.
        assert!(check_eligible(true, true, "Vikings", c, "", false).is_ok());
        // Each disqualifier maps to its reason.
        assert_eq!(
            check_eligible(false, true, "Vikings", c, "", false),
            Err(SkipReason::NotStructured)
        );
        assert_eq!(
            check_eligible(true, false, "Vikings", c, "", false),
            Err(SkipReason::Disabled)
        );
        assert_eq!(
            check_eligible(true, true, "Fix login bug", c, "", false),
            Err(SkipReason::NameNotDefault)
        );
        assert_eq!(
            check_eligible(true, true, "Vikings", None, "", false),
            Err(SkipReason::NoOneshot)
        );
        assert_eq!(
            check_eligible(
                true,
                true,
                "Vikings",
                Some(agents::get_agent("cursor").unwrap()),
                "",
                false
            ),
            Err(SkipReason::NoOneshot)
        );
        assert_eq!(
            check_eligible(true, true, "Vikings", c, "", true),
            Err(SkipReason::CommandOverridden)
        );
        assert_eq!(
            check_eligible(true, true, "Vikings", c, "my-wrapper", false),
            Err(SkipReason::CommandOverridden)
        );
        // Command equal to the agent binary is not an override.
        assert!(check_eligible(true, true, "Vikings", c, "claude", false).is_ok());
    }

    #[test]
    fn sandboxed_session_is_eligible_for_its_own_agent() {
        // #3159: a sandboxed session used to be rejected outright. Its one-shot
        // now runs inside its container, where that agent's credentials are
        // mounted, so it is eligible like any other session.
        let overrides = HashMap::new();
        assert!(
            check_eligible_resolved(true, true, "Vikings", "claude", "", true, "", &overrides)
                .is_ok()
        );
    }

    #[test]
    fn sandboxed_session_rejects_a_different_rename_agent() {
        // Only the session agent's credential dir is mounted in the container
        // (see build_container_config), so `codex` would resolve its binary from
        // the sandbox image and then fail to authenticate, on every turn.
        let overrides = HashMap::new();
        assert!(matches!(
            check_eligible_resolved(true, true, "Vikings", "claude", "codex", true, "", &overrides),
            Err(SkipReason::SandboxRenameAgentMismatch)
        ));
        // An unsupported rename agent still reports the more specific reason, so
        // the message does not blame credential mounting for a bad setting.
        assert!(matches!(
            check_eligible_resolved(
                true, true, "Vikings", "claude", "cursor", true, "", &overrides
            ),
            Err(SkipReason::NoOneshot)
        ));
        // A host session may still borrow a different rename agent.
        assert!(check_eligible_resolved(
            true, true, "Vikings", "claude", "codex", false, "", &overrides
        )
        .is_ok());
    }

    #[tokio::test]
    async fn host_session_spawns_the_agent_binary_in_the_project_dir() {
        let argv = vec!["claude".to_string(), "-p".to_string(), "hi".to_string()];
        let target = resolve_oneshot_target("abc123", false, "/workspace", "/repo", argv.clone())
            .await
            .expect("host target");
        assert_eq!(target.argv, argv, "a host one-shot must not be wrapped");
        assert_eq!(target.cwd, "/repo");
    }

    #[tokio::test]
    async fn sandboxed_session_spawns_through_the_container_runtime() {
        // Without a live container the probe returns NotRunning or Unknown, which
        // must yield no target at all: that keeps the session un-attempted so a
        // later turn retries, instead of silently spawning the agent on the host
        // where the sandbox's credentials are not mounted.
        assert!(
            resolve_oneshot_target(
                "nosuchsession",
                true,
                "/workspace",
                "/repo",
                vec!["claude".to_string()],
            )
            .await
            .is_none(),
            "a sandboxed session with no usable container must not fall back to the host"
        );
    }

    #[test]
    fn sandboxed_target_wraps_the_agent_argv_for_the_container() {
        // The wrapping itself, without needing a running container: the runtime
        // binary leads, the container workdir is explicit, and the agent argv
        // (prompt included) is carried through unchanged.
        let container = crate::containers::DockerContainer::from_session_id("abc12345");
        let argv = vec![
            "claude".to_string(),
            "-p".to_string(),
            "name this: $(id)".to_string(),
        ];
        let wrapped = container.build_exec_argv("/workspace/repo", &argv);
        assert_ne!(wrapped[0], "claude", "must spawn the container runtime");
        assert!(wrapped.contains(&"exec".to_string()));
        assert!(wrapped.contains(&"aoe-sandbox-abc12345".to_string()));
        assert!(wrapped.contains(&"/workspace/repo".to_string()));
        assert_eq!(
            &wrapped[wrapped.len() - argv.len()..],
            &argv[..],
            "the agent argv must survive verbatim as the trailing elements"
        );
    }

    #[test]
    fn every_skip_reason_has_a_user_message() {
        // Every variant, kept exhaustive by the compiler: a new `SkipReason` has
        // to be destructured here, which forces it into the list below.
        let all = {
            let _exhaustive = |r: SkipReason| match r {
                SkipReason::NotStructured
                | SkipReason::Disabled
                | SkipReason::NameNotDefault
                | SkipReason::Sandboxed
                | SkipReason::SandboxRenameAgentMismatch
                | SkipReason::NoOneshot
                | SkipReason::CommandOverridden => (),
            };
            [
                SkipReason::NotStructured,
                SkipReason::Disabled,
                SkipReason::NameNotDefault,
                SkipReason::Sandboxed,
                SkipReason::SandboxRenameAgentMismatch,
                SkipReason::NoOneshot,
                SkipReason::CommandOverridden,
            ]
        };
        for reason in all {
            assert!(
                !reason.user_message().is_empty(),
                "{} has no user message",
                reason.as_str()
            );
        }
    }

    #[test]
    fn manual_force_bypasses_only_the_disabled_gate() {
        // The manual "Auto-name now" action calls check_eligible with
        // `setting_on = cfg.setting_on || force`. `auto` is the (off) setting,
        // `force` is the manual flag; the automatic path is `auto` alone,
        // the manual path is `auto || force`. Both are bindings, not literals,
        // so the boolean expression documents the real call shape (#3039).
        let c = Some(claude());
        let auto = false;
        let force = true;
        assert_eq!(
            check_eligible(true, auto, "Vikings", c, "", false),
            Err(SkipReason::Disabled),
            "automatic path must still honor the disabled setting"
        );
        assert!(
            check_eligible(true, auto || force, "Vikings", c, "", false).is_ok(),
            "manual force must bypass the disabled gate"
        );
        // Forcing past Disabled must not smuggle past any other gate: an
        // otherwise-ineligible session is still rejected on the forced path.
        assert!(
            matches!(
                check_eligible_resolved(
                    true,
                    auto || force,
                    "Vikings",
                    "claude",
                    "codex",
                    true,
                    "",
                    &HashMap::new()
                ),
                Err(SkipReason::SandboxRenameAgentMismatch)
            ),
            "sandbox rename-agent gate still applies when forced"
        );
        assert_eq!(
            check_eligible(true, auto || force, "Fix login bug", c, "", false),
            Err(SkipReason::NameNotDefault),
            "already-named gate still applies when forced"
        );
        assert_eq!(
            check_eligible(false, auto || force, "Vikings", c, "", false),
            Err(SkipReason::NotStructured),
            "structured gate still applies when forced"
        );
        assert_eq!(
            check_eligible(true, auto || force, "Vikings", None, "", false),
            Err(SkipReason::NoOneshot),
            "no-one-shot gate still applies when forced"
        );
        assert_eq!(
            check_eligible(true, auto || force, "Vikings", c, "", true),
            Err(SkipReason::CommandOverridden),
            "command-override gate still applies when forced"
        );
    }

    #[test]
    fn argv_codex_skips_git_repo_check_with_prompt_last() {
        // codex `exec` refuses to run outside a git repo without this flag, so a
        // scratch-session one-shot would exit non-zero. The flag goes between
        // the token and the prompt; the prompt stays the final element.
        // codex has no built-in cheap alias, so with no override it takes no
        // model args: still [binary, flag, skip-git-repo-check, prompt].
        let codex = agents::get_agent("codex").unwrap();
        let argv =
            build_oneshot_argv(codex, "name this", title_default(codex)).expect("codex one-shot");
        assert_eq!(
            argv,
            vec!["codex", "exec", "--skip-git-repo-check", "name this"]
        );
        // claude pins the cheap `haiku` alias between the flag and the prompt;
        // the prompt stays the final element.
        assert_eq!(
            build_oneshot_argv(claude(), "name this", title_default(claude())).unwrap(),
            vec!["claude", "-p", "--model", "haiku", "name this"]
        );
    }

    #[test]
    fn argv_copilot_appends_silent_autoapprove_flags_after_prompt() {
        // Copilot's `-p` binds the prompt as its value, so the auto-approve and
        // silent flags follow the prompt. Without them a non-interactive title
        // call can block on a permission prompt or print stats that pollute the
        // title; with them stdout is just the final answer.
        let copilot = agents::get_agent("copilot").unwrap();
        let argv = build_oneshot_argv(copilot, "name this", title_default(copilot))
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
    fn argv_claude_injects_cheap_model_before_prompt() {
        let argv = build_oneshot_argv(claude(), "name this", title_default(claude()))
            .expect("claude one-shot");
        let model_idx = argv.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(argv[model_idx + 1], "haiku");
        let prompt_idx = argv.iter().position(|a| a == "name this").expect("prompt");
        assert!(
            model_idx < prompt_idx,
            "model args must precede the prompt, got {argv:?}"
        );
        assert_eq!(
            prompt_idx,
            argv.len() - 1,
            "prompt must be the last element"
        );
    }

    #[test]
    fn argv_summary_model_uses_cli_default() {
        // conversation_summary reads the whole transcript and may need a bigger
        // model turn, so OneshotModel::CliDefault must NOT inject any model
        // selector: the argv is exactly the pre-tunable CLI-default shape.
        let argv = build_oneshot_argv(claude(), "name this", OneshotModel::CliDefault)
            .expect("claude one-shot");
        assert!(!argv.iter().any(|a| a == "--model" || a == "haiku"));
        assert_eq!(argv, vec!["claude", "-p", "name this"]);
    }

    #[test]
    fn argv_cli_default_omits_only_the_resolved_model_args() {
        // Across every one-shot agent, CliDefault yields exactly the built-in
        // Title argv minus the resolved model slot: the model selector is the
        // only difference between the two intents.
        for agent in agents::AGENTS.iter().filter(|a| a.oneshot_flag.is_some()) {
            let default_args = resolve_title_model_args(agent, &HashMap::new());
            let title = build_oneshot_argv(
                agent,
                "name this",
                OneshotModel::Title(default_args.clone()),
            )
            .expect("one-shot");
            let cli =
                build_oneshot_argv(agent, "name this", OneshotModel::CliDefault).expect("one-shot");
            assert!(!cli.iter().any(|a| a == "--model" || a == "-m"));
            assert_eq!(cli.len(), title.len() - default_args.len());
        }
    }

    #[test]
    fn argv_agents_without_cheap_default_take_no_model_args() {
        // With no user override, only claude has a built-in cheap alias; every
        // other one-shot agent runs the CLI default (no model flag).
        for name in ["opencode", "kimi", "codex", "gemini", "copilot"] {
            let agent = agents::get_agent(name).unwrap();
            let argv = build_oneshot_argv(agent, "name this", title_default(agent))
                .unwrap_or_else(|| panic!("{name} one-shot"));
            assert!(
                !argv.iter().any(|a| a == "--model" || a == "-m"),
                "{name} has no built-in cheap alias, so its default argv carries no model flag: {argv:?}"
            );
        }
    }

    #[test]
    fn argv_user_model_override_positioned_by_flag_binding() {
        // A positional-prompt agent (codex `exec`) gets the model selector
        // before the prompt; a value-binding agent (copilot `-p`) gets it after
        // the prompt so the flag never swallows `--model` as its value.
        let mut models = HashMap::new();
        models.insert("codex".to_string(), "gpt-5".to_string());
        models.insert("copilot".to_string(), "claude-haiku-4.5".to_string());

        let codex = agents::get_agent("codex").unwrap();
        let codex_argv = build_oneshot_argv(
            codex,
            "name this",
            OneshotModel::Title(resolve_title_model_args(codex, &models)),
        )
        .expect("codex one-shot");
        assert_eq!(
            codex_argv,
            vec![
                "codex",
                "exec",
                "-m",
                "gpt-5",
                "--skip-git-repo-check",
                "name this"
            ]
        );

        let copilot = agents::get_agent("copilot").unwrap();
        let copilot_argv = build_oneshot_argv(
            copilot,
            "name this",
            OneshotModel::Title(resolve_title_model_args(copilot, &models)),
        )
        .expect("copilot one-shot");
        assert_eq!(
            copilot_argv,
            vec![
                "copilot",
                "-p",
                "name this",
                "--model",
                "claude-haiku-4.5",
                "-s",
                "--allow-all-tools",
                "--no-ask-user"
            ]
        );

        // gemini and kimi `-p` are value-binding (verified), so the model
        // selector must trail the prompt, exactly like copilot.
        let mut vb_models = HashMap::new();
        vb_models.insert("gemini".to_string(), "gemini-2.5-flash".to_string());
        vb_models.insert("kimi".to_string(), "moonshot-v1-8k".to_string());
        let gemini = agents::get_agent("gemini").unwrap();
        assert_eq!(
            build_oneshot_argv(
                gemini,
                "name this",
                OneshotModel::Title(resolve_title_model_args(gemini, &vb_models)),
            )
            .expect("gemini one-shot"),
            vec!["gemini", "-p", "name this", "-m", "gemini-2.5-flash"]
        );
        let kimi = agents::get_agent("kimi").unwrap();
        assert_eq!(
            build_oneshot_argv(
                kimi,
                "name this",
                OneshotModel::Title(resolve_title_model_args(kimi, &vb_models)),
            )
            .expect("kimi one-shot"),
            vec!["kimi", "-p", "name this", "-m", "moonshot-v1-8k"]
        );

        // opencode `run` takes a positional prompt, so its model selector goes
        // before the prompt.
        let mut oc_models = HashMap::new();
        oc_models.insert(
            "opencode".to_string(),
            "anthropic/claude-haiku-4-5".to_string(),
        );
        let opencode = agents::get_agent("opencode").unwrap();
        assert_eq!(
            build_oneshot_argv(
                opencode,
                "name this",
                OneshotModel::Title(resolve_title_model_args(opencode, &oc_models)),
            )
            .expect("opencode one-shot"),
            vec![
                "opencode",
                "run",
                "-m",
                "anthropic/claude-haiku-4-5",
                "name this"
            ]
        );
    }

    #[test]
    fn resolve_title_model_args_precedence() {
        let claude = claude();
        let codex = agents::get_agent("codex").unwrap();
        let mut models = HashMap::new();
        // Absent key -> built-in default (claude pins haiku; codex has none).
        assert_eq!(
            resolve_title_model_args(claude, &models),
            vec!["--model", "haiku"]
        );
        assert!(resolve_title_model_args(codex, &models).is_empty());
        // Non-empty override -> that model via the agent's flag.
        models.insert("claude".to_string(), "opus".to_string());
        models.insert("codex".to_string(), "gpt-5".to_string());
        assert_eq!(
            resolve_title_model_args(claude, &models),
            vec!["--model", "opus"]
        );
        assert_eq!(
            resolve_title_model_args(codex, &models),
            vec!["-m", "gpt-5"]
        );
        // Empty (or whitespace) value -> force CLI default (opt out of haiku).
        models.insert("claude".to_string(), "  ".to_string());
        assert!(resolve_title_model_args(claude, &models).is_empty());
        // A padded non-empty value is trimmed before it is pinned.
        models.insert("claude".to_string(), "  opus  ".to_string());
        assert_eq!(
            resolve_title_model_args(claude, &models),
            vec!["--model", "opus"]
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

    // Reproduces the real Claude Code v2.1.216 startup banner shape: a
    // two-column box titled `Claude Code v<ver>` (identity + logo on the left,
    // tips / what's-new on the right), then a `⚠ ... MCP servers ...` notice,
    // then the actual conversation. Captured from a live `claude` launch.
    const CLAUDE_BANNER_TRANSCRIPT: &str = "\
╭─── Claude Code v2.1.216 ──────────────────────────────────────────────────╮
│                                    │ Tips for getting started             │
│         Welcome back Nathan!       │ Ask Claude to create a new app or     │
│                                    │ ───────────────────────────────────  │
│              ▐▛███▜▌               │ What's new                            │
│             ▝▜█████▛▘              │ Added sandbox.filesystem.disabled     │
│               ▘▘ ▝▝                │ Fixed a slowdown in long sessions     │
│   Opus 4.8 (1M context) · Max ·    │ /release-notes for more               │
│   nathan@mozilla.ai's Org          │                                       │
╰────────────────────────────────────────────────────────────────────────────╯

 ⚠ 3 MCP servers need authentication · run /mcp

> fix the flaky login redirect test

I'll look at the auth redirect logic now.
Patched the race in auth.rs and added a regression test.";

    #[test]
    fn strip_agent_banner_drops_claude_startup_box() {
        let stripped = strip_agent_banner(CLAUDE_BANNER_TRANSCRIPT, "claude");
        // The box, its wording, the logo, and the MCP notice are gone.
        assert!(!stripped.contains("Claude Code v"));
        assert!(!stripped.contains("Welcome back"));
        assert!(!stripped.contains("Tips for getting started"));
        assert!(!stripped.contains("What's new"));
        assert!(!stripped.contains("MCP servers"));
        assert!(!stripped.contains('╭') && !stripped.contains('│') && !stripped.contains('█'));
        // The real conversation survives.
        assert!(stripped.contains("fix the flaky login redirect test"));
        assert!(stripped.contains("Patched the race in auth.rs"));
    }

    #[test]
    fn strip_agent_banner_is_noop_for_other_agents() {
        // A non-claude agent keeps the text verbatim (no verified signature).
        assert_eq!(
            strip_agent_banner(CLAUDE_BANNER_TRANSCRIPT, "codex"),
            CLAUDE_BANNER_TRANSCRIPT
        );
    }

    #[test]
    fn strip_agent_banner_is_noop_without_claude_code_mention() {
        // Real transcript that never names the tool is untouched, even for claude.
        let plain = "> refactor the payment retry loop\n\nDone: added a backoff and a test.";
        assert_eq!(strip_agent_banner(plain, "claude"), plain);
    }

    #[test]
    fn strip_agent_banner_keeps_content_merely_mentioning_claude_code() {
        // The gate matches "claude code", but a task ABOUT Claude Code with no
        // leading banner box must be preserved: the line filter only strips an
        // actual leading run of chrome, so `in_banner` flips off on line 1.
        let about = "> make the Claude Code onboarding docs clearer\n\n\
Rewrote the getting-started section and fixed two broken links.";
        assert_eq!(strip_agent_banner(about, "claude"), about);
    }

    #[test]
    fn strip_agent_banner_falls_back_when_only_banner() {
        // A pane that is nothing but banner must not strip down to empty; keep
        // the original so the caller still has something (and the instruction
        // prose can do its job) rather than skipping on an empty capture.
        let banner_only = "\
╭─── Claude Code v2.1.216 ──────────╮
│         Welcome back Nathan!      │
│              ▐▛███▜▌              │
╰────────────────────────────────────╯

 ⚠ 3 MCP servers need authentication · run /mcp";
        assert_eq!(strip_agent_banner(banner_only, "claude"), banner_only);
    }

    #[test]
    fn instruction_tells_model_to_ignore_startup_banner() {
        let lc = INSTRUCTION.to_lowercase();
        assert!(lc.contains("startup banner"));
        assert!(lc.contains("ignore"));
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
            build_oneshot_argv(
                agents::get_agent("codex").unwrap(),
                "x",
                OneshotModel::CliDefault
            )
            .unwrap()[1],
            "exec"
        );
        assert_eq!(
            build_oneshot_argv(
                agents::get_agent("opencode").unwrap(),
                "x",
                OneshotModel::CliDefault
            )
            .unwrap()[1],
            "run"
        );
        assert_eq!(
            build_oneshot_argv(
                agents::get_agent("gemini").unwrap(),
                "x",
                OneshotModel::CliDefault
            )
            .unwrap()[1],
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
    //
    // Also pins the repo boundary from #3154: the utility agent and the command
    // override are global/profile only, so a checked-out repo cannot redirect
    // the one-shot at another agent or swap the binary it launches.
    #[test]
    #[serial_test::serial]
    fn resolve_smart_rename_config_reads_repo_aware_config_but_not_repo_commands() {
        let home = tempfile::tempdir().expect("tempdir HOME");
        // SAFETY: serialized by `#[serial]`; matches `set_tmp_home` in
        // `src/session/mcp_state.rs`.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let app_dir = home
            .path()
            .join(".config")
            .join(crate::session::APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let app_dir = home.path().join(crate::session::APP_DIR_NAME_OTHER);
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
[session]
smart_rename_agent = "opencode"

[session.agent_command_override]
claude = "my-wrapper"
"#,
        )
        .unwrap();

        let repo = tempfile::tempdir().expect("tempdir repo");
        let cfg_dir = repo.path().join(".agent-of-empires");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"
[session]
default_tool = "codex"
smart_rename_agent = "gemini"

[session.agent_command_override]
claude = "repo-wrapper"
"#,
        )
        .unwrap();

        let resolved =
            crate::session::repo_config::resolve_config_with_repo_or_warn("default", repo.path());
        // Pins that the repo file was actually discovered: an allowed field
        // from it lands, so the assertions below are about the boundary and
        // not about a fixture that silently never loaded.
        assert_eq!(resolved.session.default_tool.as_deref(), Some("codex"));
        let cfg = resolve_smart_rename_config(&resolved.session);
        assert_eq!(
            cfg.rename_agent, "opencode",
            "the user's utility agent wins; a repo cannot redirect the one-shot"
        );
        assert_eq!(
            cfg.overrides.get("claude").map(String::as_str),
            Some("my-wrapper"),
            "a repo cannot replace the binary the one-shot launches"
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

    // ---- Terminal (non-ACP) smart rename ----

    #[test]
    fn terminal_eligibility_reasons() {
        // A terminal session is not "structured", but the terminal trigger is a
        // deliberate opt-in, so the call site passes `true`; the remaining gates
        // (user story 3) still keep the civ name where they must.
        let overrides = HashMap::new();
        assert!(check_eligible_resolved(
            true, true, "Vikings", "claude", "", false, "", &overrides
        )
        .is_ok());
        // A sandboxed terminal session is eligible for its own agent (#3159);
        // only a different rename agent is not.
        assert!(
            check_eligible_resolved(true, true, "Vikings", "claude", "", true, "", &overrides)
                .is_ok()
        );
        assert!(matches!(
            check_eligible_resolved(true, true, "Vikings", "cursor", "", false, "", &overrides),
            Err(SkipReason::NoOneshot)
        ));
        let mut ov = HashMap::new();
        ov.insert("claude".to_string(), "my-wrapper".to_string());
        assert!(matches!(
            check_eligible_resolved(true, true, "Vikings", "claude", "", false, "", &ov),
            Err(SkipReason::CommandOverridden)
        ));
        // A manually-named session (user story 2) is never a candidate.
        assert!(matches!(
            check_eligible_resolved(
                true,
                true,
                "Fix login bug",
                "claude",
                "",
                false,
                "",
                &overrides
            ),
            Err(SkipReason::NameNotDefault)
        ));
    }

    #[test]
    fn context_usable_rejects_garbage() {
        assert!(context_looks_usable("Fix the login bug in auth.rs"));
        assert!(!context_looks_usable(""));
        // No letters.
        assert!(!context_looks_usable("12345 6789 %%%"));
        // Control-char dominated (garbled/binary pane): keep the civ name.
        let garbled: String = std::iter::repeat_n('\u{7}', 50)
            .chain("ab".chars())
            .collect();
        assert!(!context_looks_usable(&garbled));
    }

    #[test]
    fn head_tail_keeps_both_ends() {
        let short = "just a short line";
        assert_eq!(head_tail(short, 3072, 1024), short);
        let long = format!("HEAD{}TAIL", "x".repeat(5000));
        let r = head_tail(&long, 10, 10);
        assert!(r.starts_with("HEAD"));
        assert!(r.ends_with("TAIL"));
        assert!(r.contains("\n...\n"));
        assert!(r.len() < long.len());
    }

    #[test]
    fn echo_baseline_is_first_nonempty_line() {
        assert_eq!(
            extract_echo_baseline("\n\n  fix the bug  \nmore"),
            "fix the bug"
        );
        assert_eq!(extract_echo_baseline(""), "");
    }

    /// Explicit named skip for tests that need a live tmux server. Returns
    /// `true` when tmux is usable; otherwise prints a per-test skip line and
    /// the caller early-returns, so a tmux-less environment reports the skip
    /// instead of silently asserting nothing.
    fn require_tmux(test: &str) -> bool {
        let available = crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !available {
            eprintln!("Skipping {test}: tmux not available");
        }
        available
    }

    #[test]
    #[serial_test::serial]
    fn title_mutation_locks_validate_and_serialize_writers() {
        use crate::session::instance::Instance;
        use crate::session::storage::Storage;
        let home = tempfile::tempdir().expect("tempdir HOME");
        let _home_guard = crate::session::test_support::isolate_home(home.path());

        assert!(crate::session::storage::acquire_session_title_lock("../unsafe").is_err());

        let instance = Instance::new("Vikings", "/tmp/x");
        let id = instance.id.clone();
        let first_title_lock = crate::session::storage::acquire_session_title_lock(&id).unwrap();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let competing_id = id.clone();
        let competing_writer = std::thread::spawn(move || {
            let _lock = crate::session::storage::acquire_session_title_lock(&competing_id).unwrap();
            acquired_tx.send(()).unwrap();
        });
        assert!(
            acquired_rx
                .recv_timeout(std::time::Duration::from_millis(150))
                .is_err(),
            "a competing title writer entered before release"
        );
        drop(first_title_lock);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("competing writer should enter after release");
        competing_writer.join().unwrap();

        let storage = Storage::new_unwatched("identity-lock").unwrap();
        storage
            .update(|instances, _groups| {
                instances.push(instance);
                Ok(())
            })
            .unwrap();
        let identity_lock = crate::session::acquire_session_identity_lock().unwrap();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let writer_id = id.clone();
        let identity_writer = std::thread::spawn(move || {
            let storage = Storage::new_unwatched("identity-lock").unwrap();
            apply_terminal_title(&storage, &writer_id, Some("Shared title")).unwrap();
            finished_tx.send(()).unwrap();
        });
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(150))
                .is_err(),
            "smart rename bypassed the identity transaction"
        );
        drop(identity_lock);
        finished_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("smart rename should finish after identity release");
        identity_writer.join().unwrap();
        assert_eq!(
            storage.load().unwrap()[0].title,
            "Shared title",
            "smart rename should commit after entering the transaction"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_terminal_title_renames_auto_named_session_and_marks_attempted() {
        use crate::session::instance::Instance;
        use crate::session::storage::Storage;
        let home = tempfile::tempdir().expect("tempdir HOME");
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = Storage::new_unwatched("default").expect("storage");
        let civ = Instance::new("Vikings", "/tmp/x");
        let civ_id = civ.id.clone();
        storage
            .update(|instances, _groups| {
                instances.push(civ);
                Ok(())
            })
            .unwrap();

        apply_terminal_title(&storage, &civ_id, Some("Fix login bug")).unwrap();

        let inst = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == civ_id)
            .unwrap();
        assert_eq!(inst.title, "Fix login bug");
        assert_eq!(inst.last_auto_title.as_deref(), Some("Fix login bug"));
        assert!(inst.smart_rename_attempted);
    }

    #[test]
    #[serial_test::serial]
    fn apply_terminal_title_never_overwrites_a_manual_title() {
        use crate::session::instance::Instance;
        use crate::session::storage::Storage;
        let home = tempfile::tempdir().expect("tempdir HOME");
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = Storage::new_unwatched("default").expect("storage");
        let mut manual = Instance::new("Britons", "/tmp/y");
        manual.title = "Hand-picked".to_string();
        let manual_id = manual.id.clone();
        let duplicate_candidate = Instance::new("Franks", "/tmp/z/");
        let duplicate_id = duplicate_candidate.id.clone();
        let mut duplicate_owner = Instance::new("Saxons", "/tmp/z");
        duplicate_owner.title = "Already owned".to_string();
        storage
            .update(|instances, _groups| {
                instances.extend([manual, duplicate_candidate, duplicate_owner]);
                Ok(())
            })
            .unwrap();

        apply_terminal_title(&storage, &manual_id, Some("Should Not Apply")).unwrap();
        apply_terminal_title(&storage, &duplicate_id, Some("Already owned")).unwrap();

        let instances = storage.load().unwrap();
        let manual = instances.iter().find(|i| i.id == manual_id).unwrap();
        assert_eq!(manual.title, "Hand-picked");
        assert!(manual.smart_rename_attempted);
        let duplicate = instances.iter().find(|i| i.id == duplicate_id).unwrap();
        assert_eq!(duplicate.title, "Franks");
        assert_eq!(duplicate.last_auto_title, None);
        assert!(duplicate.smart_rename_attempted);
    }

    #[test]
    #[serial_test::serial]
    fn apply_terminal_title_write_failure_preserves_metadata_and_tmux() {
        use crate::session::instance::Instance;
        use crate::session::storage::Storage;
        use crate::tmux::test_helpers::TmuxTestSession;
        let home = tempfile::tempdir().expect("tempdir HOME");
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let mut storage = Storage::new_unwatched("default").expect("storage");
        let failed = Instance::new("Franks", "/tmp/write-failure");
        let failed_id = failed.id.clone();
        let failed_tmux_name = crate::tmux::Session::generate_name(&failed_id, &failed.title);
        storage
            .update(|instances, _groups| {
                instances.push(failed);
                Ok(())
            })
            .unwrap();

        let tmux = require_tmux("apply_terminal_title_write_failure_preserves_metadata_and_tmux");
        let _tmux_guard = if tmux {
            let guard = TmuxTestSession::from_name(failed_tmux_name.clone());
            let output = crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", guard.name(), "sleep 60"])
                .output()
                .expect("tmux new-session");
            assert!(
                output.status.success(),
                "failed to create tmux session {}: {}",
                guard.name(),
                String::from_utf8_lossy(&output.stderr)
            );
            crate::tmux::refresh_session_cache();
            Some(guard)
        } else {
            None
        };

        storage.set_fail_writes_for_test(true);
        let error = apply_terminal_title(&storage, &failed_id, Some("Must Not Land"))
            .expect_err("injected persistence failure must abort the title mutation");
        assert!(error
            .to_string()
            .contains("injected sessions write failure"));

        let failed = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == failed_id)
            .unwrap();
        assert_eq!(failed.title, "Franks");
        assert!(!failed.smart_rename_attempted);
        if tmux {
            crate::tmux::refresh_session_cache();
            assert!(
                crate::tmux::Session::from_name(&failed_tmux_name).exists(),
                "tmux must not rekey when the title commit fails"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn apply_terminal_title_rekeys_live_tmux_to_committed_title() {
        if !require_tmux("apply_terminal_title_rekeys_live_tmux_to_committed_title") {
            return;
        }
        use crate::session::instance::Instance;
        use crate::session::storage::Storage;
        use crate::tmux::test_helpers::TmuxTestSession;
        let home = tempfile::tempdir().expect("tempdir HOME");
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = Storage::new_unwatched("default").expect("storage");
        let civ = Instance::new("Vikings", "/tmp/x");
        let civ_id = civ.id.clone();
        let old_tmux_name = crate::tmux::Session::generate_name(&civ_id, &civ.title);
        let new_tmux_name = crate::tmux::Session::generate_name(&civ_id, "Fix login bug");
        storage
            .update(|instances, _groups| {
                instances.push(civ);
                Ok(())
            })
            .unwrap();

        let old_guard = TmuxTestSession::from_name(old_tmux_name.clone());
        let new_guard = TmuxTestSession::from_name(new_tmux_name.clone());
        let output = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", old_guard.name(), "sleep 60"])
            .output()
            .expect("tmux new-session");
        assert!(
            output.status.success(),
            "failed to create tmux session {}: {}",
            old_guard.name(),
            String::from_utf8_lossy(&output.stderr)
        );
        crate::tmux::refresh_session_cache();

        apply_terminal_title(&storage, &civ_id, Some("Fix login bug")).unwrap();

        assert!(!crate::tmux::Session::from_name(&old_tmux_name).exists());
        assert!(
            crate::tmux::Session::from_name(&new_tmux_name).exists(),
            "the committed title must determine the tmux destination"
        );
        drop((old_guard, new_guard));
    }
}
