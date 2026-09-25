//! Automatic "smart" rename of a structured-view (ACP) session from its first turn.

use crate::agents;
use crate::session::civilizations::is_default_civ_name;
use crate::session::config::SessionConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Cap on concurrent smart-rename one-shots across the process.
pub const MAX_CONCURRENT: usize = 2;

/// Per-session smart-rename state surfaced to the dashboard so the sidebar can show that a session
/// will be (or is being) auto-named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmartRenameState {
    #[default]
    Inactive,
    /// Eligible and still default-named: will auto-name on the next prompt.
    Pending,
    /// A one-shot title call is in flight for this session right now.
    Running,
}

/// Why a session is not eligible for smart rename, for logging and to gate the `Pending` indicator.
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

    /// The reason phrased for a user.
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

/// Single source of truth for "is this session eligible to be auto-named right now". `force` is the
/// manual "Auto-name now" action, which may regenerate over an already-chosen title.
pub fn check_eligible(
    structured: bool,
    setting_on: bool,
    force: bool,
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
    if !force && !is_default_civ_name(title) {
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

/// Resolve the tool name used for the one-shot rename: the configured `smart_rename_agent` when
/// non-empty, otherwise the session's own tool.
pub fn resolve_rename_tool<'a>(session_tool: &'a str, rename_setting: &'a str) -> &'a str {
    let setting = rename_setting.trim();
    if setting.is_empty() {
        session_tool
    } else {
        setting
    }
}

/// Resolve the rename agent from the `smart_rename_agent` setting and gate it, returning the
/// resolved built-in agent on success.
// One more input than `check_eligible` (the rename-agent setting); a params
// struct would only add boilerplate to the two call sites and the unit tests.
#[allow(clippy::too_many_arguments)]
pub fn check_eligible_resolved(
    structured: bool,
    setting_on: bool,
    force: bool,
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
        force,
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
#[derive(Debug, Clone, Copy)]
pub struct SmartRenameConfig<'a> {
    pub setting_on: bool,
    pub rename_agent: &'a str,
    pub overrides: &'a HashMap<String, String>,
    pub rename_model: &'a HashMap<String, String>,
}

/// Input for a one-shot title call.
#[derive(Debug, Clone)]
pub struct SmartRenameInput {
    pub first_user_prompt: String,
    pub context: String,
}

/// Byte budget for the agent's prose in the rendered first-turn context.
pub const FIRST_TURN_AGENT_BYTES: usize = 1024;
/// Byte budget for the user prompt inside the rendered first-turn context.
const FIRST_TURN_USER_BYTES: usize = 3072;

/// Render the first turn into a single summarizable block.
pub fn render_first_turn(user_prompt: &str, agent_prose: &str) -> String {
    let user = truncate_bytes(user_prompt.trim(), FIRST_TURN_USER_BYTES);
    let agent = truncate_bytes(agent_prose.trim(), FIRST_TURN_AGENT_BYTES);
    if agent.is_empty() {
        user.to_string()
    } else {
        format!("User:\n{user}\n\nAgent:\n{agent}")
    }
}

/// Project a resolved [`SessionConfig`] into the fields the smart-rename indicator
/// (`list_sessions` in `src/server/api/sessions/list.rs`) and the runtime gate
/// ([`try_smart_rename`]) both consume. `smart_rename_override` is the registered project's
/// override, resolved by the caller so a hot loop can look it up once per project.
pub fn resolve_smart_rename_config(
    session: &SessionConfig,
    smart_rename_override: Option<bool>,
) -> SmartRenameConfig<'_> {
    SmartRenameConfig {
        setting_on: smart_rename_override.unwrap_or(session.smart_rename),
        rename_agent: &session.smart_rename_agent,
        overrides: &session.agent_command_override,
        rename_model: &session.smart_rename_model,
    }
}

/// Hard cap on how much of the user's first message is handed to the one-shot call.
const MAX_PROMPT_BYTES: usize = 4096;
/// Reject a candidate title longer than this many characters.
const MAX_TITLE_CHARS: usize = 60;
/// Reject a candidate title with more than this many words.
const MAX_TITLE_WORDS: usize = 8;

/// Instruction prefix sent to the agent.
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

/// What a one-shot argv targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneshotModel {
    Title(Vec<String>),
    CliDefault,
}

/// Resolve the model-selector tokens (`[flag, model_id]` or empty) for a title one-shot from the
/// per-agent `smart_rename_model` map.
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

/// Build the argv for a one-shot title or summary call, or `None` when the agent has no known
/// one-shot mode.
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

/// Turn raw agent stdout into a clean title, or `None` to keep the generated name.
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

// The ACP one-shot is deferred to the first `prompt_complete` `Event::Stopped`, so it never races
// the live worker for the same provider API.
pub(crate) const ONESHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run the agent one-shot in the session's working directory, capturing stdout.
// ponytail: a hung in-container one-shot outlives its 60s timeout and is only reaped when the
// container goes down.
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
        // Capture stderr so a non-zero exit logs WHY (e.g. codex's "Not inside a trusted
        // directory"); without it the failure is an opaque exit code.
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

/// Where a one-shot runs for this session: the argv to spawn and the host working directory to
/// spawn it in (empty for a container, whose workdir comes from the `exec` itself).
pub(crate) struct OneshotTarget {
    pub argv: Vec<String>,
    pub cwd: String,
}

/// Resolve the spawn target for a session's one-shot: unchanged on the host, wrapped in a container
/// `exec` when the session is sandboxed.
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

/// Whether a renamer may overwrite this session's title: `force` (the manual "Auto-name now"
/// action) always may; otherwise only a still-default civ name or the last title an auto renamer
/// wrote.
pub(crate) fn title_is_auto_overwritable(
    inst: &crate::session::instance::Instance,
    force: bool,
) -> bool {
    force
        || is_default_civ_name(&inst.title)
        || inst.last_auto_title.as_deref() == Some(inst.title.as_str())
}

// Terminal (non-ACP) smart rename.

/// Head/tail byte budgets for the captured first-turn transcript handed to the one-shot.
const CONTEXT_HEAD_BYTES: usize = 3072;
const CONTEXT_TAIL_BYTES: usize = 1024;

/// Cheap poll-hot-path gate: fire a detached terminal rename for this session iff it is a
/// still-default-named, non-structured, not-yet-attempted session whose resolved config has smart
/// rename on.
pub fn maybe_spawn_terminal_smart_rename(inst: &crate::session::instance::Instance) {
    if inst.is_structured() || inst.smart_rename_attempted || !is_default_civ_name(&inst.title) {
        return;
    }
    // Resolve config and run the FULL eligibility check on the (rare) turn-completion edge, never
    // per tick.
    let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        &inst.source_profile,
        Path::new(&inst.project_path),
    );
    let smart_rename_override = crate::session::projects::find_by_canonical_path(
        &inst.source_profile,
        Path::new(inst.repo_path()),
    )
    .and_then(|p| p.overrides.smart_rename);
    let cfg = resolve_smart_rename_config(&resolved.session, smart_rename_override);
    if check_eligible_resolved(
        true,
        cfg.setting_on,
        false,
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

/// Spawn an on-demand terminal rename for a session, forcing past the `smart_rename`-disabled and
/// name-not-default gates.
pub fn spawn_smart_rename_now(profile: &str, session_id: &str) {
    spawn_detached(profile, session_id, true);
}

/// Re-exec `aoe __smart-rename [--force] <profile> <id>` as a detached child (setsid, null stdio,
/// dropped handle), mirroring the `__acp-runner` launcher.
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
        // The child is setsid-detached and does its own work; we only need to reap it so it does
        // not linger as a zombie in the long-lived poller process (unlike __acp-runner, this child
        // exits quickly).
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

/// Directory holding the advisory lock files, under the app data dir so the path is identical
/// across the TUI, the daemon, and detached children in the same build namespace.
fn lock_dir() -> Option<PathBuf> {
    let dir = crate::session::get_app_dir().ok()?.join("smart-rename");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Try to take an exclusive advisory (`flock`) lock on `path`, returning the held file on success.
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

/// Non-blocking global slot lock preserving `MAX_CONCURRENT` across processes, so a burst of
/// sessions going idle at once (e.g. a batch launch) cannot fan out into one host agent process per
/// session.
fn try_global_slot() -> Option<std::fs::File> {
    let dir = lock_dir()?;
    (0..MAX_CONCURRENT).find_map(|n| try_lock(&dir.join(format!("slot-{n}.lock"))))
}

/// Capture the pane's full first-turn transcript and reduce it to a bounded, middle-elided
/// head+tail block, or `None` when the capture is empty or looks like garbage (so the caller keeps
/// the civ name without paying for a one-shot).
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

/// CLI agents print a startup banner (a welcome box plus "getting started" tips) on launch.
fn strip_agent_banner(text: &str, tool: &str) -> String {
    // Only Claude Code has a verified banner shape to key on. Other agents rely
    // on the instruction prose; add a case here per agent as needed.
    if !tool.eq_ignore_ascii_case("claude") {
        return text.to_string();
    }
    // Gate loosely: the startup box names the tool ("Claude Code v2.1.216" in its top border).
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
    // Guard against eating the whole transcript (a pane that was nothing but banner, or a future
    // banner shape that trips the heuristic): keep the original when stripping leaves too little to
    // title.
    if stripped.chars().filter(|c| c.is_alphabetic()).count() < 12 {
        return text.to_string();
    }
    stripped
}

/// Whether a line belongs to the Claude Code startup banner.
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
    // Numbered tip: "1...." or "2)...".
    let mut chars = t.chars();
    matches!((chars.next(), chars.next()), (Some(d), Some(p)) if d.is_ascii_digit() && (p == '.' || p == ')'))
}

/// Reject a pane capture that is empty, has no letters, or is dominated by control characters (a
/// garbled/binary pane).
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

/// First non-empty line of the transcript, the best proxy for the user's opening message, used only
/// as the echo baseline so `sanitize_title` rejects a title that merely parrots the prompt.
fn extract_echo_baseline(context: &str) -> String {
    context
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Persist the outcome of a terminal one-shot. Without `force`, a manual rename that landed during
/// the one-shot wins.
fn apply_terminal_title(
    storage: &crate::session::storage::Storage,
    id: &str,
    new_title: Option<&str>,
    force: bool,
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
                let should_write = title_is_auto_overwritable(&instances[index], force)
                    && instances[index].title != *title;
                // Manual and automatic rename paths share one domain predicate; exclude this row
                // explicitly so the uniqueness contract does not depend on `should_write` remaining
                // title-sensitive.
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

/// Entry point for the detached `aoe __smart-rename [--force] <profile> <id>` child.
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
        Some(true) => rename_structured_via_daemon(session_id).await,
        Some(false) => run_terminal_rename(profile, session_id, force).await,
        None => Ok(()),
    }
}

/// Ask the running daemon to (re-)run the smart-rename one-shot for a structured session via `POST
/// /api/sessions/{id}/smart-rename`.
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

/// Body of the terminal (non-ACP) smart rename.
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
        repo_path,
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
            i.repo_path().to_string(),
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
    // Durable double-check: storage may have propagated a completed attempt (or a manual rename)
    // since the poller observed the edge.
    if (already && !force) || structured {
        return Ok(());
    }

    let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        profile,
        Path::new(&project_path),
    );
    let smart_rename_override =
        crate::session::projects::find_by_canonical_path(profile, Path::new(&repo_path))
            .and_then(|p| p.overrides.smart_rename);
    let cfg = resolve_smart_rename_config(&resolved.session, smart_rename_override);
    let agent = match check_eligible_resolved(
        // Terminal owned turns are an eligible session kind; the `structured` gate exists only so
        // the daemon's generic ACP listener skips non-structured sessions, which does not apply to
        // this deliberate terminal trigger.
        true,
        cfg.setting_on || force,
        force,
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
    // Same resolution the status poller uses: own status rules first, then the `agent_detect_as`
    // alias, resolved through the live registry so a session whose alias entry landed after it was
    // created is not stranded on the raw tool name here (which would skip `strip_agent_banner` and
    // read the pane with no detector).
    let detect_tool = crate::tmux::status_rules::detection_tool(profile, &tool, &detect_as);

    // Best-effort no-race: the poller fired on Running -> Idle, but the user may have started
    // another turn since.
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
    apply_terminal_title(&storage, session_id, new_title.as_deref(), force)?;
    Ok(())
}

pub use serve::{should_trigger_smart_rename, try_smart_rename};

mod serve {
    use super::*;
    use crate::server::AppState;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    /// Should this ACP broadcast event trigger a smart-rename one-shot for its session?
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

    /// Whether a one-shot has already been attempted for this session this process lifetime.
    fn attempted_contains(state: &AppState, session_id: &str) -> bool {
        state
            .smart_rename_attempted
            .lock()
            .expect("smart_rename_attempted poisoned")
            .contains(session_id)
    }

    /// Marks a session as having an in-flight one-shot rename so a burst of rapid first prompts
    /// cannot spawn concurrent title generators.
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

    /// Best-effort auto-rename of a structured-view session from its first turn.
    pub async fn try_smart_rename(
        state: Arc<AppState>,
        session_id: String,
        input: SmartRenameInput,
        force: bool,
    ) {
        if input.first_user_prompt.trim().is_empty() {
            return;
        }

        if attempted_contains(&state, &session_id) {
            return;
        }

        let Some((
            profile,
            tool,
            command,
            project_path,
            repo_path,
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
                    i.repo_path().to_string(),
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

        let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            &profile,
            Path::new(&project_path),
        );
        let smart_rename_override =
            crate::session::projects::find_by_canonical_path(&profile, Path::new(&repo_path))
                .and_then(|p| p.overrides.smart_rename);
        let cfg = resolve_smart_rename_config(&resolved.session, smart_rename_override);
        let agent = match check_eligible_resolved(
            structured,
            cfg.setting_on || force,
            force,
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

        // Re-check attempted after taking the inflight slot: another task may have completed and
        // marked this session between the entry check and acquiring the guard.
        if attempted_contains(&state, &session_id) {
            return;
        }

        let prompt = build_prompt(&input.context);
        let model = OneshotModel::Title(resolve_title_model_args(agent, cfg.rename_model));
        let Some(argv) = build_oneshot_argv(agent, &prompt, model) else {
            return;
        };

        // A spawn error, timeout, or non-zero exit returns None.
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

        // The agent produced output (usable or not).
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
        apply_auto_title(&state, &session_id, &profile, &new_title, force).await;
    }

    /// Persist a generated title and mirror it into AppState. Without `force`, a manual rename that
    /// landed during the one-shot wins.
    pub(crate) async fn apply_auto_title(
        state: &Arc<AppState>,
        id: &str,
        profile: &str,
        new_title: &str,
        force: bool,
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
        // Own copies for the closure below: `storage.update` runs inside `spawn_blocking`, whose
        // body must be `'static + Send`, so the borrowed `id`/`new_title` cannot cross the thread
        // boundary.
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
                // Manual and automatic rename paths share one domain predicate; exclude this row
                // explicitly so a future no-op policy change cannot make the row collide with
                // itself.
                let should_write = title_is_auto_overwritable(&instances[index], force)
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
        async fn apply_auto_title_skips_duplicates_and_force_overwrites_manual_titles() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let storage =
                crate::session::storage::Storage::new_unwatched("default").expect("storage");
            let mut target = crate::session::Instance::new("Franks", "/tmp/shared/");
            target.source_profile = "default".to_string();
            let target_id = target.id.clone();
            let mut owner = crate::session::Instance::new("Already owned", "/tmp/shared");
            owner.source_profile = "default".to_string();
            let mut manual = crate::session::Instance::new("Britons", "/tmp/y");
            manual.source_profile = "default".to_string();
            manual.title = "Hand-picked".to_string();
            let manual_id = manual.id.clone();
            let rows = vec![target, owner, manual];
            storage
                .update(|instances, _groups| {
                    *instances = rows.clone();
                    Ok(())
                })
                .unwrap();
            let state = crate::server::test_support::build_test_app_state(rows);

            apply_auto_title(&state, &target_id, "default", "Already owned", false).await;
            apply_auto_title(&state, &manual_id, "default", "Regenerated title", true).await;

            let title_of = |instances: &[crate::session::Instance], id: &str| {
                instances
                    .iter()
                    .find(|instance| instance.id == id)
                    .unwrap()
                    .title
                    .clone()
            };
            let persisted = storage.load().unwrap();
            let in_memory = state.instances.read().await.clone();
            for rows in [&persisted, &in_memory] {
                assert_eq!(title_of(rows, &target_id), "Franks");
                assert_eq!(title_of(rows, &manual_id), "Regenerated title");
            }
        }

        #[test]
        fn is_duplicate_session_normalizes_trailing_slash() {
            // The skip in `apply_auto_title` / `apply_terminal_title` reuses the creation
            // predicate, which trims trailing '/' on both sides before comparing paths.
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
            // A failed spawn must surface as None so try_smart_rename leaves the session
            // un-attempted and a later prompt can retry.
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
            assert!(title_is_auto_overwritable(&inst, false));
            // After an auto write, title == last_auto_title, so a forced
            // retry can still replace an automatic title.
            inst.title = "Fix login redirect".to_string();
            inst.last_auto_title = Some("Fix login redirect".to_string());
            assert!(title_is_auto_overwritable(&inst, false));
            // A manual rename diverges title from last_auto_title: frozen.
            inst.title = "Production hotfix".to_string();
            assert!(!title_is_auto_overwritable(&inst, false));
            // Legacy record: a non-default title with no recorded auto title
            // is left untouched.
            let mut legacy = Instance::new("Vikings", "/tmp");
            legacy.title = "Hand-picked name".to_string();
            legacy.last_auto_title = None;
            assert!(!title_is_auto_overwritable(&legacy, false));
            // The manual "Auto-name now" action may overwrite even a hand-picked title.
            assert!(title_is_auto_overwritable(&legacy, true));
        }

        #[test]
        fn oneshot_timeout_is_60s() {
            // Drift-guard against future bump-back: raised this to 120s to absorb the
            // prompt-handler race; removed the race at source, so this should stay at the
            // deferred-trigger ceiling.
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
                synthesized: false,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> &'static agents::AgentDef {
        agents::get_agent("claude").expect("claude agent exists")
    }

    /// The one-shot argv for `name`, joined with spaces, with `model` standing in
    /// for that agent's `smart_rename_model` entry (`None` leaves it unset).
    fn argv_for(name: &str, model: Option<&str>) -> String {
        let agent = agents::get_agent(name).unwrap_or_else(|| panic!("{name} agent exists"));
        let models: HashMap<String, String> = model
            .map(|m| (name.to_string(), m.to_string()))
            .into_iter()
            .collect();
        let args = OneshotModel::Title(resolve_title_model_args(agent, &models));
        build_oneshot_argv(agent, "name this", args)
            .unwrap_or_else(|| panic!("{name} one-shot"))
            .join(" ")
    }

    #[test]
    fn oneshot_argv_places_model_args_by_agent_convention() {
        // (agent, smart_rename_model entry, argv)
        let cases = [
            ("claude", None, "claude -p --model haiku name this"),
            ("codex", None, "codex exec --skip-git-repo-check name this"),
            ("copilot", None, COPILOT_DEFAULT),
            (
                "codex",
                Some("gpt-5"),
                "codex exec -m gpt-5 --skip-git-repo-check name this",
            ),
            ("copilot", Some("claude-haiku-4.5"), COPILOT_MODEL),
            (
                "gemini",
                Some("gemini-2.5-flash"),
                "gemini -p name this -m gemini-2.5-flash",
            ),
            (
                "kimi",
                Some("moonshot-v1-8k"),
                "kimi -p name this -m moonshot-v1-8k",
            ),
            (
                "opencode",
                Some("anthropic/claude-haiku-4-5"),
                OPENCODE_MODEL,
            ),
        ];
        for (name, model, want) in cases {
            assert_eq!(argv_for(name, model), want, "{name} model={model:?}");
        }
    }

    const COPILOT_DEFAULT: &str = "copilot -p name this -s --allow-all-tools --no-ask-user";
    const COPILOT_MODEL: &str =
        "copilot -p name this --model claude-haiku-4.5 -s --allow-all-tools --no-ask-user";
    const OPENCODE_MODEL: &str = "opencode run -m anthropic/claude-haiku-4-5 name this";

    #[test]
    fn cli_default_drops_only_the_resolved_model_args() {
        assert_eq!(
            build_oneshot_argv(claude(), "name this", OneshotModel::CliDefault).unwrap(),
            vec!["claude", "-p", "name this"]
        );
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
    fn check_eligible_reasons() {
        let c = Some(claude());
        assert!(check_eligible(true, true, false, "Vikings", c, "", false).is_ok());
        assert_eq!(
            check_eligible(false, true, false, "Vikings", c, "", false),
            Err(SkipReason::NotStructured)
        );
        assert_eq!(
            check_eligible(true, false, false, "Vikings", c, "", false),
            Err(SkipReason::Disabled)
        );
        assert_eq!(
            check_eligible(true, true, false, "Fix login bug", c, "", false),
            Err(SkipReason::NameNotDefault)
        );
        assert_eq!(
            check_eligible(true, true, false, "Vikings", None, "", false),
            Err(SkipReason::NoOneshot)
        );
        assert_eq!(
            check_eligible(
                true,
                true,
                false,
                "Vikings",
                Some(agents::get_agent("cursor").unwrap()),
                "",
                false
            ),
            Err(SkipReason::NoOneshot)
        );
        assert_eq!(
            check_eligible(true, true, false, "Vikings", c, "", true),
            Err(SkipReason::CommandOverridden)
        );
        assert_eq!(
            check_eligible(true, true, false, "Vikings", c, "my-wrapper", false),
            Err(SkipReason::CommandOverridden)
        );
        assert!(check_eligible(true, true, false, "Vikings", c, "claude", false).is_ok());

        // Manual "Auto-name now" bypasses only the disabled and already-named gates.
        let auto = false;
        let force = true;
        assert_eq!(
            check_eligible(true, auto, false, "Vikings", c, "", false),
            Err(SkipReason::Disabled),
            "automatic path must still honor the disabled setting"
        );
        assert!(
            check_eligible(true, auto || force, force, "Vikings", c, "", false).is_ok(),
            "manual force must bypass the disabled gate"
        );
        assert!(
            matches!(
                check_eligible_resolved(
                    true,
                    auto || force,
                    force,
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
        assert!(
            check_eligible(true, auto || force, force, "Fix login bug", c, "", false).is_ok(),
            "manual force must bypass the already-named gate too"
        );
        assert_eq!(
            check_eligible(false, auto || force, force, "Vikings", c, "", false),
            Err(SkipReason::NotStructured),
            "structured gate still applies when forced"
        );
        assert_eq!(
            check_eligible(true, auto || force, force, "Vikings", None, "", false),
            Err(SkipReason::NoOneshot),
            "no-one-shot gate still applies when forced"
        );
        assert_eq!(
            check_eligible(true, auto || force, force, "Vikings", c, "", true),
            Err(SkipReason::CommandOverridden),
            "command-override gate still applies when forced"
        );
    }

    #[tokio::test]
    async fn oneshot_target_runs_on_the_host_only_for_host_sessions() {
        let argv = vec!["claude".to_string(), "-p".to_string(), "hi".to_string()];
        let target = resolve_oneshot_target("abc123", false, "/workspace", "/repo", argv.clone())
            .await
            .expect("host target");
        assert_eq!(target.argv, argv, "a host one-shot must not be wrapped");
        assert_eq!(target.cwd, "/repo");
        assert!(
            resolve_oneshot_target("nosuchsession", true, "/workspace", "/repo", argv)
                .await
                .is_none(),
            "a sandboxed session with no usable container must not fall back to the host"
        );
    }

    #[test]
    fn sandboxed_target_wraps_the_agent_argv_for_the_container() {
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
    fn resolve_title_model_args_precedence() {
        let claude = claude();
        let codex = agents::get_agent("codex").unwrap();
        let mut models = HashMap::new();
        assert_eq!(
            resolve_title_model_args(claude, &models),
            vec!["--model", "haiku"]
        );
        assert!(resolve_title_model_args(codex, &models).is_empty());
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
        models.insert("claude".to_string(), "  ".to_string());
        assert!(resolve_title_model_args(claude, &models).is_empty());
        models.insert("claude".to_string(), "  opus  ".to_string());
        assert_eq!(
            resolve_title_model_args(claude, &models),
            vec!["--model", "opus"]
        );
    }

    #[test]
    fn the_rename_agent_and_its_override_gate_resolve_independently_of_the_session() {
        let none = HashMap::new();
        let resolve = |rename_agent: &str, command: &str, overrides: &HashMap<String, String>| {
            check_eligible_resolved(
                true,
                true,
                false,
                "Vikings",
                "claude",
                rename_agent,
                false,
                command,
                overrides,
            )
            .map(|agent| agent.binary)
        };
        assert_eq!(resolve("", "", &none), Ok("claude"));
        assert_eq!(resolve("codex", "", &none), Ok("codex"));
        assert_eq!(
            resolve("not-a-real-agent", "", &none),
            Err(SkipReason::NoOneshot)
        );

        // The override that matters is the one on the agent actually being run.
        let claude_override = HashMap::from([("claude".to_string(), "my-wrapper".to_string())]);
        assert_eq!(
            resolve("", "", &claude_override),
            Err(SkipReason::CommandOverridden)
        );
        assert_eq!(resolve("codex", "", &claude_override), Ok("codex"));
        let codex_override = HashMap::from([("codex".to_string(), "my-codex".to_string())]);
        assert_eq!(
            resolve("codex", "", &codex_override),
            Err(SkipReason::CommandOverridden)
        );

        assert!(
            check_eligible_resolved(
                true, true, false, "Vikings", "opencode", "claude", false, "opencode", &none
            )
            .is_ok(),
            "the session's own command is irrelevant to a distinct rename agent"
        );

        // A sandboxed session may only be named by its own agent.
        let sandboxed = |rename_agent: &str| {
            check_eligible_resolved(
                true,
                true,
                false,
                "Vikings",
                "claude",
                rename_agent,
                true,
                "",
                &none,
            )
        };
        assert!(sandboxed("").is_ok(), "its own agent is fine");
        assert!(matches!(
            sandboxed("codex"),
            Err(SkipReason::SandboxRenameAgentMismatch)
        ));
        assert!(
            matches!(sandboxed("cursor"), Err(SkipReason::NoOneshot)),
            "an agent with no one-shot mode reports that, not the sandbox gate"
        );
    }

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
    fn strip_agent_banner_drops_only_the_claude_startup_box() {
        let lc = INSTRUCTION.to_lowercase();
        assert!(lc.contains("startup banner") && lc.contains("ignore"));

        let stripped = strip_agent_banner(CLAUDE_BANNER_TRANSCRIPT, "claude");
        assert!(!stripped.contains("Claude Code v"));
        assert!(!stripped.contains("Welcome back"));
        assert!(!stripped.contains("Tips for getting started"));
        assert!(!stripped.contains("What's new"));
        assert!(!stripped.contains("MCP servers"));
        assert!(!stripped.contains('╭') && !stripped.contains('│') && !stripped.contains('█'));
        assert!(stripped.contains("fix the flaky login redirect test"));
        assert!(stripped.contains("Patched the race in auth.rs"));

        let banner_only = "\
╭─── Claude Code v2.1.216 ──────────╮
│         Welcome back Nathan!      │
│              ▐▛███▜▌              │
╰────────────────────────────────────╯

 ⚠ 3 MCP servers need authentication · run /mcp";
        for (case, text, tool) in [
            ("another agent", CLAUDE_BANNER_TRANSCRIPT, "codex"),
            (
                "no claude code mention",
                "> refactor the payment retry loop\n\nDone: added a backoff and a test.",
                "claude",
            ),
            (
                "content about claude code",
                "> make the Claude Code onboarding docs clearer\n\n\
Rewrote the getting-started section and fixed two broken links.",
                "claude",
            ),
            ("nothing but banner", banner_only, "claude"),
        ] {
            assert_eq!(strip_agent_banner(text, tool), text, "{case}");
        }
    }

    #[test]
    fn first_turn_and_prompt_are_framed_and_bounded() {
        let r = render_first_turn("fix the login bug", "Patched the redirect in auth.rs");
        assert_eq!(
            r,
            "User:\nfix the login bug\n\nAgent:\nPatched the redirect in auth.rs"
        );
        assert_eq!(
            render_first_turn("fix the login bug", ""),
            "fix the login bug"
        );
        assert_eq!(
            render_first_turn("fix the login bug", "   "),
            "fix the login bug"
        );
        let huge_prompt = "p".repeat(FIRST_TURN_USER_BYTES * 2);
        let r = render_first_turn(&huge_prompt, "concise agent summary");
        assert!(r.starts_with("User:\n"));
        assert!(
            r.contains("concise agent summary"),
            "each half is capped independently, so agent prose survives a huge prompt"
        );

        let msg = format!("start{}\u{0}end", "x".repeat(5000));
        let p = build_prompt(&msg);
        assert!(p.contains("start"));
        assert!(!p.contains('\u{0}'));
        assert!(p.len() < 5000 + INSTRUCTION.len() + 64);
    }

    #[test]
    fn sanitize_title_accepts_one_clean_line_and_rejects_the_rest() {
        // Raw agent stdout, and the title kept from it.
        let kept: &[(&str, &str)] = &[
            ("Fix login bug", "Fix login bug"),
            ("**\"Refactor auth module.\"**", "Refactor auth module"),
            ("- Update README", "Update README"),
            ("1. Add dark mode", "Add dark mode"),
            ("\u{1b}[32mGreen title here\u{1b}[0m", "Green title here"),
            // The last qualifying line wins over preamble and log noise.
            (
                "Sure, here is a concise title:\n\nFix login redirect bug\n",
                "Fix login redirect bug",
            ),
            (
                "[2024] booting agent\nthinking...\nWire up websockets\n",
                "Wire up websockets",
            ),
        ];
        for (raw, want) in kept {
            assert_eq!(sanitize_title(raw, "x").as_deref(), Some(*want), "{raw:?}");
        }

        let too_wordy = "a ".repeat(20);
        let too_long = "z".repeat(80);
        let rejected = [
            "I cannot help with that",
            "Sorry, no.",
            "NONE",
            "   \n  ",
            too_wordy.as_str(),
            too_long.as_str(),
            "12345",
        ];
        for raw in rejected {
            assert!(sanitize_title(raw, "x").is_none(), "{raw:?}");
        }
        assert!(
            sanitize_title("fix the thing", "fix the thing").is_none(),
            "a title echoing the prompt is not a title"
        );
    }

    // Regression for: pins the shared helper that both `try_smart_rename` and the sidebar indicator
    // overlay in `src/server/api/sessions/list.rs` route through.
    #[test]
    #[serial_test::serial]
    fn resolve_smart_rename_config_reads_repo_aware_config_but_not_repo_commands() {
        let home = tempfile::tempdir().expect("tempdir HOME");
        let _home_guard = crate::session::test_support::isolate_home(home.path());

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
agent_detect_as = { my-agent = "claude" }
smart_rename_agent = "gemini"

[session.agent_command_override]
claude = "repo-wrapper"
"#,
        )
        .unwrap();

        let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            "default",
            repo.path(),
        );
        assert_eq!(
            resolved
                .session
                .agent_detect_as
                .get("my-agent")
                .map(String::as_str),
            Some("claude")
        );
        let cfg = resolve_smart_rename_config(
            &resolved.session,
            crate::session::projects::find_by_canonical_path("default", repo.path())
                .and_then(|p| p.overrides.smart_rename),
        );
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
            false,
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

    #[test]
    #[serial_test::serial]
    fn resolve_smart_rename_config_honors_project_override() {
        let home = tempfile::tempdir().expect("tempdir HOME");
        let _home_guard = crate::session::test_support::isolate_home(home.path());

        let repo = tempfile::tempdir().expect("tempdir repo");
        crate::session::projects::add(
            "default",
            crate::session::ProjectScope::Global,
            crate::session::Project::new(
                "demo",
                repo.path().to_string_lossy(),
                crate::session::ProjectScope::Global,
            ),
            false,
        )
        .expect("register project");
        crate::session::projects::update_overrides(
            "default",
            crate::session::ProjectScope::Global,
            "demo",
            |ov| ov.smart_rename = Some(false),
        )
        .expect("set override");

        let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            "default",
            repo.path(),
        );
        let cfg = resolve_smart_rename_config(
            &resolved.session,
            crate::session::projects::find_by_canonical_path("default", repo.path())
                .and_then(|p| p.overrides.smart_rename),
        );
        assert!(
            !cfg.setting_on,
            "project override (false) should win over the global default (true, per mod.rs's smart_rename default)"
        );
    }

    #[test]
    fn terminal_context_helpers() {
        assert!(context_looks_usable("Fix the login bug in auth.rs"));
        assert!(!context_looks_usable(""));
        assert!(!context_looks_usable("12345 6789 %%%"));
        let garbled: String = std::iter::repeat_n('\u{7}', 50)
            .chain("ab".chars())
            .collect();
        assert!(!context_looks_usable(&garbled));

        let short = "just a short line";
        assert_eq!(head_tail(short, 3072, 1024), short);
        let long = format!("HEAD{}TAIL", "x".repeat(5000));
        let r = head_tail(&long, 10, 10);
        assert!(r.starts_with("HEAD") && r.ends_with("TAIL") && r.contains("\n...\n"));
        assert!(r.len() < long.len());

        assert_eq!(
            extract_echo_baseline("\n\n  fix the bug  \nmore"),
            "fix the bug"
        );
        assert_eq!(extract_echo_baseline(""), "");
    }

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
        let (title_contended_tx, title_contended_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let competing_id = id.clone();
        let competing_writer = std::thread::spawn(move || {
            let _observer =
                crate::session::storage::observe_lock_contention_for_test(title_contended_tx);
            let _lock = crate::session::storage::acquire_session_title_lock(&competing_id).unwrap();
            acquired_tx.send(()).unwrap();
        });
        let contended = title_contended_rx.recv_timeout(std::time::Duration::from_secs(2));
        let entered_early = acquired_rx.try_recv().is_ok();
        drop(first_title_lock);
        assert!(
            contended.is_ok(),
            "title writer never demonstrated contention"
        );
        assert!(!entered_early, "title writer entered before release");
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
        let (identity_contended_tx, identity_contended_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let writer_id = id.clone();
        let identity_writer = std::thread::spawn(move || {
            let _observer =
                crate::session::storage::observe_lock_contention_for_test(identity_contended_tx);
            let storage = Storage::new_unwatched("identity-lock").unwrap();
            apply_terminal_title(&storage, &writer_id, Some("Shared title"), false).unwrap();
            finished_tx.send(()).unwrap();
        });
        let contended = identity_contended_rx.recv_timeout(std::time::Duration::from_secs(2));
        let entered_early = finished_rx.try_recv().is_ok();
        drop(identity_lock);
        assert!(
            contended.is_ok(),
            "smart rename never contended on identity transaction"
        );
        assert!(!entered_early, "smart rename bypassed identity transaction");
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
    fn apply_terminal_title_writes_only_overwritable_non_duplicate_titles() {
        use crate::session::instance::Instance;
        use crate::session::storage::Storage;
        let home = tempfile::tempdir().expect("tempdir HOME");
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = Storage::new_unwatched("default").expect("storage");
        let civ = Instance::new("Vikings", "/tmp/x");
        let civ_id = civ.id.clone();
        let mut manual = Instance::new("Britons", "/tmp/y");
        manual.title = "Hand-picked".to_string();
        let manual_id = manual.id.clone();
        let mut forced = manual.clone();
        forced.id = format!("{}f", manual.id);
        let forced_id = forced.id.clone();
        let duplicate_candidate = Instance::new("Franks", "/tmp/z/");
        let duplicate_id = duplicate_candidate.id.clone();
        let mut duplicate_owner = Instance::new("Saxons", "/tmp/z");
        duplicate_owner.title = "Already owned".to_string();
        storage
            .update(|instances, _groups| {
                instances.extend([civ, manual, forced, duplicate_candidate, duplicate_owner]);
                Ok(())
            })
            .unwrap();

        apply_terminal_title(&storage, &civ_id, Some("Fix login bug"), false).unwrap();
        apply_terminal_title(&storage, &manual_id, Some("Should Not Apply"), false).unwrap();
        apply_terminal_title(&storage, &forced_id, Some("Regenerated title"), true).unwrap();
        apply_terminal_title(&storage, &duplicate_id, Some("Already owned"), false).unwrap();

        let instances = storage.load().unwrap();
        let row = |id: &str| instances.iter().find(|i| i.id == id).unwrap();
        // (id, title, last_auto_title); every attempt is marked.
        for (id, title, last_auto) in [
            (&civ_id, "Fix login bug", Some("Fix login bug")),
            (&manual_id, "Hand-picked", None),
            (&forced_id, "Regenerated title", Some("Regenerated title")),
            (&duplicate_id, "Franks", None),
        ] {
            let row = row(id);
            assert_eq!(row.title, title);
            assert_eq!(row.last_auto_title.as_deref(), last_auto, "{title}");
            assert!(row.smart_rename_attempted, "{title}");
        }
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
        let error = apply_terminal_title(&storage, &failed_id, Some("Must Not Land"), false)
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

        apply_terminal_title(&storage, &civ_id, Some("Fix login bug"), false).unwrap();

        assert!(!crate::tmux::Session::from_name(&old_tmux_name).exists());
        assert!(
            crate::tmux::Session::from_name(&new_tmux_name).exists(),
            "the committed title must determine the tmux destination"
        );
        drop((old_guard, new_guard));
    }
}
