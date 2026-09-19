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
//! `Instance::swap_tool` already parks the outgoing ids for it.

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
/// would have kept. Anything unresolvable therefore parks.
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
    let (Some(agent), Some(home)) = (instance.resolved_agent(), dirs::home_dir()) else {
        return ToolSwap::Park;
    };
    let (Some(source_root), Some(target_root)) = (
        config_root(
            instance,
            &instance.effective_profile(),
            &instance.tool,
            agent,
            &home,
        ),
        config_root(instance, new_profile, new_tool, agent, &home),
    ) else {
        return ToolSwap::Park;
    };
    if source_root == target_root {
        // Both names read one config root, so the transcript is already where
        // the incoming account looks for it.
        return ToolSwap::KeepConversation(None);
    }
    ToolSwap::KeepConversation(Some(ConversationCarry {
        source_root,
        target_root,
        session_ids: conversation_ids(instance),
    }))
}

impl ConversationCarry {
    /// Point the carry at the conversation the disk row actually holds.
    ///
    /// The plan is built from the TUI's in-memory mirror, but the capture
    /// pollers own `agent_session_id` through CAS writes to disk, so the disk
    /// row can carry a conversation the snapshot had not seen yet. That row is
    /// what the next launch resumes from, which makes it the one whose
    /// transcript has to travel. `persist_tool_swap` reads it under the storage
    /// lock it already takes, so this costs no extra read and opens no race the
    /// swap did not already have. Empty leaves the planned ids alone.
    pub(crate) fn retarget(&mut self, session_ids: Vec<String>) {
        if !session_ids.is_empty() {
            self.session_ids = session_ids;
        }
    }

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
pub(crate) fn conversation_ids(instance: &Instance) -> Vec<String> {
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
/// sandbox store when the session is sandboxed, else whatever
/// [`crate::session::capture::claude_home_for_host_environment`] resolves, so
/// the carry writes to the directory the launch-time transcript probe reads.
///
/// `profile` is explicit rather than the instance's own because a restart that
/// also moves profiles resolves the incoming tool under the target profile's
/// config.
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
    crate::session::capture::claude_home_for_host_environment(
        declared.as_deref(),
        &instance.resolved_host_environment(),
    )
    .ok()
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
    let encoded_dirs = root.read_dir(projects, PROJECT_DIR_SCAN_MAX)?;
    if encoded_dirs.len() == PROJECT_DIR_SCAN_MAX {
        tracing::warn!(
            target: "session.store",
            root = %root.path().display(),
            scanned = PROJECT_DIR_SCAN_MAX,
            "stopped scanning the account's projects at the cap; a transcript \
             beyond it will not be carried"
        );
    }
    let mut found = Vec::new();
    for encoded in encoded_dirs {
        let relative = projects.join(encoded).join(&leaf);
        if root.regular_exists(&relative) {
            found.push(relative);
        }
    }
    Ok(found)
}

/// Copy `relative` from `source` to `target`.
///
/// A destination copy at least as new as the source is left alone; a strictly
/// older one is replaced. The source account is the one the session was just
/// running on, so its transcript is the live continuation, and an older
/// destination is a stale earlier carry. Skipping it unconditionally forked the
/// conversation on a swap back: `A -> B -> A` found `A`'s pre-swap copy still
/// in place and resumed that, silently stranding everything that happened on
/// `B` under `B`'s root.
///
/// The content lands under a staging name before it is published, so a half
/// transcript is never reachable under the real one: it would resume into a
/// conversation that silently loses its tail. A crash leaves the staging name
/// behind instead, which nothing reads.
fn copy_file(source: &AnchoredDir, target: &AnchoredDir, relative: &Path) -> Result<()> {
    let Some(mut reader) = source.open_regular(relative, TRANSCRIPT_MAX_BYTES)? else {
        if source.regular_exists(relative) {
            tracing::warn!(
                target: "session.store",
                transcript = %relative.display(),
                max_bytes = TRANSCRIPT_MAX_BYTES,
                "transcript is larger than the carry cap and was not copied"
            );
        }
        return Ok(());
    };
    let incoming = source.regular_modified(relative)?;
    if let Some(parent) = relative.parent() {
        target.ensure_dir(parent)?;
    }
    let replace = match (target.regular_modified(relative)?, incoming) {
        // Nothing there, or nothing readable there: publish without replacing,
        // so a writer that lands one first keeps it.
        (None, _) => false,
        (Some(existing), Some(incoming)) if incoming > existing => true,
        // Current, or two timestamps that cannot be ordered. Either way the
        // destination is not provably stale, so leave it.
        (Some(_), _) => return Ok(()),
    };
    let staging = staging_path(relative);
    // A leftover from a crashed carry: the publish below would hand over
    // whatever it holds, so start from a fresh file.
    target.remove_file(&staging)?;
    let Some(mut writer) = target.create_new_regular(&staging)? else {
        return Ok(());
    };
    let copied = std::io::copy(&mut reader, &mut writer)
        .and_then(|_| writer.sync_all())
        .map_err(anyhow::Error::from)
        .and_then(|()| target.publish_staged(&staging, relative, replace));
    match copied {
        Ok(_) => Ok(()),
        Err(error) => {
            let _ = target.remove_file(&staging);
            Err(error)
        }
    }
}

/// The temporary name `relative` is written under before it is published.
/// Same directory, so the rename is atomic.
fn staging_path(relative: &Path) -> PathBuf {
    let mut staged = relative.as_os_str().to_os_string();
    staged.push(".aoe-carry");
    PathBuf::from(staged)
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

    /// The decisive end-to-end fact: a host session pinned to two accounts
    /// through `agent_config_dir` must have its transcript copied into the
    /// directory the launch-time resume probe reads. If those two disagree the
    /// launch downgrades `--resume <sid>` to `--session-id <sid>`, which the
    /// agent rejects as already in use now that the transcript exists there.
    #[test]
    #[serial_test::serial]
    fn carry_lands_where_the_launch_probe_looks_for_it() {
        const SID: &str = "11111111-2222-3333-4444-555555555555";
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let home = dirs::home_dir().expect("home");
        let app_dir = crate::session::get_app_dir().expect("app dir");
        std::fs::create_dir_all(&app_dir).expect("app dir");
        std::fs::write(
            app_dir.join("config.toml"),
            "[session.agent_detect_as]\n\
             claude-1 = \"claude\"\n\
             claude-2 = \"claude\"\n\
             \n\
             [session.agent_config_dir]\n\
             claude-1 = \"~/dot-claude-1\"\n\
             claude-2 = \"~/dot-claude-2\"\n",
        )
        .expect("config");
        let profile = crate::session::config::effective_profile("");
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
        crate::session::config::profile_config::resolve_config_or_warn(&profile);

        let project = home.join("project");
        std::fs::create_dir_all(&project).expect("project");
        let mut inst = Instance::new("t", project.to_str().unwrap());
        inst.tool = "claude-1".to_string();
        inst.detect_as = "claude".to_string();
        inst.agent_session_id = Some(SID.to_string());

        let ToolSwap::KeepConversation(Some(carry)) = classify(&inst, &profile, "claude-2") else {
            panic!("an account swap with a conversation must plan a carry");
        };
        assert_eq!(carry.source_root, home.join("dot-claude-1"));
        assert_eq!(carry.target_root, home.join("dot-claude-2"));

        // Seed the outgoing account exactly where Claude writes it, then carry.
        let encoded = crate::session::capture::encode_claude_project_path(
            &crate::session::capture::canonicalize_or_raw(project.to_str().unwrap())
                .to_string_lossy(),
        );
        let seeded = carry.source_root.join("projects").join(&encoded);
        std::fs::create_dir_all(&seeded).expect("seed dir");
        std::fs::write(seeded.join(format!("{SID}.jsonl")), "conversation\n").expect("seed");

        assert!(
            crate::session::capture::claude_host_transcript_confirmed_absent(
                project.to_str().unwrap(),
                SID,
                &[],
                inst.declared_agent_config_dir_for("claude-2").as_deref(),
            ),
            "pre-condition: the incoming account cannot see it yet"
        );

        carry.run();

        assert!(
            !crate::session::capture::claude_host_transcript_confirmed_absent(
                project.to_str().unwrap(),
                SID,
                &[],
                inst.declared_agent_config_dir_for("claude-2").as_deref(),
            ),
            "after the carry the launch must resume rather than re-pin the id"
        );
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

    /// Set `path`'s mtime so the newer-wins comparison is deterministic rather
    /// than at the mercy of filesystem timestamp granularity.
    fn set_age(path: &Path, seconds_ago: u64) {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds_ago);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    #[test]
    fn run_leaves_a_transcript_the_target_account_has_more_recently() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("claude-1");
        let target = temp.path().join("claude-2");
        let from = seed_transcript(&source, "-work-repo", "sid-a", "incoming\n");
        let to = seed_transcript(&target, "-work-repo", "sid-a", "already here\n");
        set_age(&from, 3600);
        set_age(&to, 60);

        carry(&source, &target, &["sid-a"]).run();

        assert_eq!(
            std::fs::read_to_string(&to).unwrap(),
            "already here\n",
            "the destination was written after the source, so it is not stale"
        );
    }

    /// A swap back to an account that already holds an earlier copy of the
    /// conversation must resume everything that happened in between, not the
    /// snapshot it kept from before the first swap.
    #[test]
    fn run_replaces_a_stale_copy_left_by_an_earlier_swap() {
        let temp = tempfile::tempdir().unwrap();
        let account_a = temp.path().join("claude-1");
        let account_b = temp.path().join("claude-2");
        let on_a = seed_transcript(&account_a, "-work-repo", "sid-a", "first\n");
        set_age(&on_a, 3600);

        // A -> B, then the agent keeps working on B.
        carry(&account_a, &account_b, &["sid-a"]).run();
        let on_b = account_b.join("projects/-work-repo/sid-a.jsonl");
        std::fs::write(&on_b, "first\nsecond\n").unwrap();
        set_age(&on_b, 60);

        // B -> A: A still holds its pre-swap copy.
        carry(&account_b, &account_a, &["sid-a"]).run();

        assert_eq!(
            std::fs::read_to_string(&on_a).unwrap(),
            "first\nsecond\n",
            "swapping back must resume the work done on the other account"
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
