//! Carrying a conversation when a restart swaps only which account an agent
//! runs as.
//!
//! `[session.agent_config_dir]` gives one agent several tool names, one per
//! account. Swapping between them changes the agent's config root, so the
//! transcript the outgoing account wrote is invisible to the incoming one and
//! the agent comes up on an empty conversation even though its session id is
//! still valid (#4030). Copying the transcript into the incoming account's
//! root is what makes `--resume <sid>` reach it there.
//!
//! The copy is bounded to swaps that keep the same built-in agent. A swap to a
//! genuinely different agent lands a transcript that agent cannot read, and
//! [`Instance::swap_tool`] already parks the outgoing ids for it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::agents::{AgentDef, SessionCaptureBackend};
use crate::session::capture::is_valid_session_id;
use crate::session::config::container_config;
use crate::session::config::profile_config::resolve_config_or_warn;
use crate::session::{AnchoredDir, Instance};

/// Claude refuses to open a transcript it cannot fit in memory long before
/// this, so the cap only exists to keep [`AnchoredDir::open_regular`] bounded.
const TRANSCRIPT_MAX_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Encoded-cwd directories scanned under `projects/`. One entry per directory
/// an account has ever run the agent in.
const PROJECT_DIR_SCAN_MAX: usize = 4096;

/// A planned copy of one session's transcript from the outgoing account's
/// agent config root into the incoming account's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationCarry {
    source_root: PathBuf,
    target_root: PathBuf,
    session_ids: Vec<String>,
}

/// Whether restarting on `new_tool` changes only which account the same agent
/// runs as, rather than the agent itself.
///
/// Both sides resolve through the same registry lookup a launch uses, so a
/// tool named after a built-in, a `custom_agents` entry aliased with
/// `agent_detect_as`, and a built-in reached under its own name all compare
/// on the built-in they land on. An unresolvable side answers `false`: with no
/// agent to compare, AoE cannot claim the conversation transports.
pub(crate) fn is_account_swap(
    current_profile: &str,
    current_tool: &str,
    current_detect_as: &str,
    new_profile: &str,
    new_tool: &str,
) -> bool {
    if current_tool == new_tool {
        return false;
    }
    let from = crate::session::resolved_agent_for(current_profile, current_tool, current_detect_as);
    let to = crate::session::resolved_agent_for(new_profile, new_tool, "");
    match (from, to) {
        (Some(from), Some(to)) => from.name == to.name,
        _ => false,
    }
}

/// Whether this row has a conversation AoE could carry, independent of which
/// tool the restart picks.
///
/// Sandboxed rows still on the shared store layout are excluded: their store
/// moves to its private location during the next launch, after the point a
/// restart could seed it, so a copy staged beforehand would land beside the
/// move rather than in it.
fn is_carry_eligible(instance: &Instance) -> bool {
    instance.resolved_agent().is_some_and(carries_transcript)
        && !instance.sandbox_store_move_pending()
        && !conversation_ids(instance).is_empty()
}

/// What a restart does with the conversation when it changes the tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolSwap {
    /// Park the outgoing tool's ids under its name and pick up whatever was
    /// parked for the incoming one. Where a swap lands unless every part of a
    /// carry holds, starting with the new tool running the same agent.
    Park,
    /// Keep the conversation: only the account changed. Carries the copy that
    /// puts the transcript where the incoming account looks for it, unless
    /// both accounts already share one config root.
    KeepConversation(Option<ConversationCarry>),
}

/// Decide the swap, reading `instance` in its pre-swap state.
///
/// [`ToolSwap::KeepConversation`] needs every part of the carry to hold, not
/// just the classification: keeping a session id the incoming account has no
/// transcript for would resume into nothing and lose the id the parking swap
/// would have kept.
pub(crate) fn classify(instance: &Instance, new_profile: &str, new_tool: &str) -> ToolSwap {
    if !is_account_swap(
        &instance.source_profile,
        &instance.tool,
        &instance.detect_as,
        new_profile,
        new_tool,
    ) || !is_carry_eligible(instance)
    {
        return ToolSwap::Park;
    }
    match plan(instance, new_profile, new_tool) {
        Some(carry) => ToolSwap::KeepConversation(Some(carry)),
        // Both tools resolve to one config root, so the transcript is already
        // where the incoming account reads it.
        None if shares_config_root(instance, new_profile, new_tool) => {
            ToolSwap::KeepConversation(None)
        }
        None => ToolSwap::Park,
    }
}

fn shares_config_root(instance: &Instance, new_profile: &str, new_tool: &str) -> bool {
    let (Some(agent), Some(home)) = (instance.resolved_agent(), dirs::home_dir()) else {
        return false;
    };
    let source = config_root(
        instance,
        &instance.effective_profile(),
        &instance.tool,
        agent,
        &home,
    );
    source.is_some() && source == config_root(instance, new_profile, new_tool, agent, &home)
}

/// Plan the copy for a swap already classified by [`is_account_swap`], reading
/// `instance` in its pre-swap state. `None` when there is nothing to carry or
/// either config root is unresolvable.
fn plan(instance: &Instance, new_profile: &str, new_tool: &str) -> Option<ConversationCarry> {
    let agent = instance.resolved_agent()?;
    if !carries_transcript(agent) || instance.sandbox_store_move_pending() {
        return None;
    }
    let session_ids = conversation_ids(instance);
    if session_ids.is_empty() {
        return None;
    }
    let home = dirs::home_dir()?;
    let source_root = config_root(
        instance,
        &instance.effective_profile(),
        &instance.tool,
        agent,
        &home,
    )?;
    let target_root = config_root(instance, new_profile, new_tool, agent, &home)?;
    if source_root == target_root {
        return None;
    }
    Some(ConversationCarry {
        source_root,
        target_root,
        session_ids,
    })
}

impl ConversationCarry {
    /// Copy each planned transcript. Best-effort: the launch that follows
    /// falls back to a fresh conversation the same way it does for any sid
    /// whose transcript is missing, so a failure is logged rather than
    /// surfaced as a restart failure.
    pub fn run(&self) {
        if let Err(error) = self.copy_all() {
            tracing::warn!(
                target: "session.store",
                source = %self.source_root.display(),
                target = %self.target_root.display(),
                error = %format_args!("{error:#}"),
                "could not carry the conversation to the new account; the agent starts fresh there"
            );
        }
    }

    fn copy_all(&self) -> Result<()> {
        let source = AnchoredDir::open(&self.source_root)
            .with_context(|| format!("opening {}", self.source_root.display()))?;
        std::fs::create_dir_all(&self.target_root)
            .with_context(|| format!("creating {}", self.target_root.display()))?;
        let target = AnchoredDir::open(&self.target_root)
            .with_context(|| format!("opening {}", self.target_root.display()))?;
        for session_id in &self.session_ids {
            for relative in claude_transcripts_for(&source, session_id)? {
                copy_file(&source, &target, &relative)
                    .with_context(|| format!("copying {}", relative.display()))?;
            }
        }
        Ok(())
    }
}

/// Whether AoE knows where this agent keeps one conversation inside its config
/// root. Teaching another agent to carry means giving it a locator alongside
/// [`claude_transcripts_for`].
fn carries_transcript(agent: &'static AgentDef) -> bool {
    agent
        .session_support
        .as_ref()
        .and_then(|support| support.capture)
        .is_some_and(|capture| capture.backend == SessionCaptureBackend::Claude)
}

/// The ids whose transcripts a carry copies: the terminal conversation, the
/// structured one, or both when the row holds each.
fn conversation_ids(instance: &Instance) -> Vec<String> {
    let mut ids: Vec<String> = [
        instance.agent_session_id.as_deref(),
        instance.acp_session_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|id| is_valid_session_id(id))
    .map(str::to_string)
    .collect();
    ids.dedup();
    ids
}

/// The agent config root one tool reads on this session: the per-instance
/// sandbox store when the session is sandboxed, else the profile's declared
/// directory for that tool, else the account the host environment points at.
fn config_root(
    instance: &Instance,
    profile: &str,
    tool: &str,
    agent: &'static AgentDef,
    home: &Path,
) -> Option<PathBuf> {
    let declared = resolve_config_or_warn(profile)
        .session
        .agent_config_dir_for(tool, home);
    if instance.is_sandboxed() {
        return container_config::sandbox_store_dir(
            agent.name,
            home,
            declared.as_deref(),
            &instance.id,
        )
        .ok()
        .flatten();
    }
    declared.or_else(|| {
        crate::session::capture::claude_home_for_host_environment(
            &instance.resolved_host_environment(),
        )
        .ok()
    })
}

/// Every `projects/<encoded-cwd>/<sid>.jsonl` under `root`.
///
/// The encoded directory is derived from the cwd the agent ran in, which for a
/// sandboxed session is the container's workspace path rather than the host's,
/// so this scans `projects/` instead of recomputing it. A session that moved
/// between directories has one transcript per cwd it spoke in.
fn claude_transcripts_for(root: &AnchoredDir, session_id: &str) -> Result<Vec<PathBuf>> {
    let projects = Path::new("projects");
    if root.directory_modified(projects)?.is_none() {
        return Ok(Vec::new());
    }
    let leaf = format!("{session_id}.jsonl");
    let mut found = Vec::new();
    for encoded in root.read_dir(projects, PROJECT_DIR_SCAN_MAX)? {
        let relative = projects.join(encoded).join(&leaf);
        if root.regular_exists(&relative) {
            found.push(relative);
        }
    }
    Ok(found)
}

/// Copy `relative` from `source` to `target`, leaving an existing target file
/// alone. A failed write is removed rather than left truncated: a half
/// transcript resumes into a conversation that silently loses its tail.
fn copy_file(source: &AnchoredDir, target: &AnchoredDir, relative: &Path) -> Result<()> {
    let Some(mut reader) = source.open_regular(relative, TRANSCRIPT_MAX_BYTES)? else {
        return Ok(());
    };
    if let Some(parent) = relative.parent() {
        target.ensure_dir(parent)?;
    }
    let Some(mut writer) = target.create_new_regular(relative)? else {
        return Ok(());
    };
    if let Err(error) = std::io::copy(&mut reader, &mut writer) {
        drop(writer);
        let _ = target.remove_file(relative);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_transcript(root: &Path, encoded: &str, session_id: &str, body: &str) -> PathBuf {
        let dir = root.join("projects").join(encoded);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session_id}.jsonl"));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn carry(source: &Path, target: &Path, session_ids: &[&str]) -> ConversationCarry {
        ConversationCarry {
            source_root: source.to_path_buf(),
            target_root: target.to_path_buf(),
            session_ids: session_ids.iter().map(|id| id.to_string()).collect(),
        }
    }

    #[test]
    fn is_account_swap_only_for_two_names_of_one_agent() {
        const PROFILE: &str = "account-swap-classify-test";
        let _registry = crate::session::install_aliases(
            PROFILE,
            &[
                ("claude-1", "claude"),
                ("claude-2", "claude"),
                ("cx", "codex"),
            ],
        );

        // (current tool, stored alias, new tool, is an account swap)
        let cases = [
            ("claude-1", "claude", "claude-2", true),
            // A built-in reached under its own name is still the same account.
            ("claude", "", "claude-2", true),
            ("claude-2", "claude", "claude", true),
            // A different agent: carrying a transcript there is meaningless.
            ("claude-1", "claude", "cx", false),
            ("claude-1", "claude", "codex", false),
            // No swap at all.
            ("claude-1", "claude", "claude-1", false),
            // Nothing resolves the tool, so nothing proves the agent matches.
            ("claude-1", "claude", "mystery", false),
        ];
        for (tool, detect_as, new_tool, expected) in cases {
            assert_eq!(
                is_account_swap(PROFILE, tool, detect_as, PROFILE, new_tool),
                expected,
                "{tool} -> {new_tool}"
            );
        }
    }

    #[test]
    fn classify_parks_when_the_agent_has_no_known_transcript_layout() {
        const PROFILE: &str = "account-swap-classify-agent-test";
        let _registry = crate::session::install_aliases(
            PROFILE,
            &[
                ("codex-1", "codex"),
                ("codex-2", "codex"),
                ("claude-1", "claude"),
                ("claude-2", "claude"),
            ],
        );

        let mut inst = Instance::new("t", "/tmp/x");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "codex-1".to_string();
        inst.detect_as = "codex".to_string();
        inst.agent_session_id = Some("sid-a".to_string());
        assert_eq!(
            classify(&inst, PROFILE, "codex-2"),
            ToolSwap::Park,
            "keeping a sid AoE cannot move the transcript for resumes into nothing"
        );

        // Same shape on an agent AoE can locate, but with no conversation yet.
        let mut inst = Instance::new("t", "/tmp/x");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "claude-1".to_string();
        inst.detect_as = "claude".to_string();
        assert_eq!(classify(&inst, PROFILE, "claude-2"), ToolSwap::Park);
    }

    #[test]
    fn run_copies_every_cwd_the_conversation_spoke_in() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("claude-1");
        let target = temp.path().join("claude-2");
        seed_transcript(&source, "-work-repo", "sid-a", "first\n");
        seed_transcript(&source, "-work-other", "sid-a", "second\n");
        seed_transcript(&source, "-work-repo", "sid-b", "unrelated\n");

        carry(&source, &target, &["sid-a"]).run();

        assert_eq!(
            std::fs::read_to_string(target.join("projects/-work-repo/sid-a.jsonl")).unwrap(),
            "first\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("projects/-work-other/sid-a.jsonl")).unwrap(),
            "second\n"
        );
        assert!(!target.join("projects/-work-repo/sid-b.jsonl").exists());
    }

    #[test]
    fn run_leaves_a_transcript_the_target_account_already_has() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("claude-1");
        let target = temp.path().join("claude-2");
        seed_transcript(&source, "-work-repo", "sid-a", "incoming\n");
        seed_transcript(&target, "-work-repo", "sid-a", "already here\n");

        carry(&source, &target, &["sid-a"]).run();

        assert_eq!(
            std::fs::read_to_string(target.join("projects/-work-repo/sid-a.jsonl")).unwrap(),
            "already here\n"
        );
    }

    #[test]
    fn run_is_a_no_op_when_the_source_account_has_no_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("claude-1");
        let target = temp.path().join("claude-2");
        std::fs::create_dir_all(&source).unwrap();

        carry(&source, &target, &["sid-a"]).run();

        assert!(!target.join("projects").exists());
    }
}
