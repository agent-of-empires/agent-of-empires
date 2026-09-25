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

use std::collections::HashSet;
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
    // Registry semantics, like `is_account_swap`: the carry moves a
    // transcript for any alias of a transcript-carrying agent, regardless of
    // how the launch command resolves. Native execution identity is a
    // launch-time contract, not a carry-time one.
    crate::session::resolved_agent_for(
        &instance.source_profile,
        &instance.tool,
        &instance.detect_as,
    )
    .is_some_and(carries_transcript)
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
    if instance.is_structured() {
        // Structured rows have no terminal launch to consume a carry; their
        // restart short-circuits before any copy could happen.
        return ToolSwap::Park;
    }

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
    let (Some(agent), Some(home)) = (
        crate::session::resolved_agent_for(
            &instance.source_profile,
            &instance.tool,
            &instance.detect_as,
        ),
        dirs::home_dir(),
    ) else {
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
    if source_root == target_root
        && ![
            instance.agent_session_binding.as_ref(),
            instance.resume_binding.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|binding| binding.is_known())
    {
        return ToolSwap::KeepConversation(None);
    }
    ToolSwap::KeepConversation(Some(ConversationCarry {
        source_root,
        target_root,
        session_ids: conversation_ids(instance),
    }))
}

#[cfg(test)]
#[test]
#[serial_test::serial]
fn shared_account_roots_carry_the_selected_external_store() {
    let _app = crate::session::test_support::isolate_app_dir();
    let stub = tempfile::tempdir().unwrap();
    let _claude = crate::session::test_support::install_login_shell_path_command(
        stub.path(),
        "claude",
        "#!/bin/sh\nexit 1\n",
    );
    let home = dirs::home_dir().unwrap();
    let app = crate::session::get_app_dir().unwrap();
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("config.toml"),
        "[session.agent_detect_as]\na = \"claude\"\nb = \"claude\"\n\
         [session.agent_config_dir]\na = \"~/shared\"\nb = \"~/shared\"\n",
    )
    .unwrap();
    let profile = crate::session::config::effective_profile("");
    let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
    crate::session::config::profile_config::resolve_config_or_warn(&profile);
    let project = home.join("project");
    let source = home.join("external");
    std::fs::create_dir_all(&project).unwrap();
    let sid = "11111111-2222-3333-4444-555555555555";
    let relative = Path::new("projects")
        .join(crate::session::capture::encode_claude_project_path(
            &project.to_string_lossy(),
        ))
        .join(format!("{sid}.jsonl"));
    std::fs::create_dir_all(source.join(relative.parent().unwrap())).unwrap();
    std::fs::write(source.join(&relative), "selected conversation\n").unwrap();
    let mut instance = Instance::new("carry", project.to_str().unwrap());
    instance.source_profile = profile.clone();
    instance.tool = "a".into();
    instance.command = "claude".into();
    instance.detect_as = "claude".into();
    instance.agent_session_id = Some(sid.into());
    instance.agent_session_binding = Some(crate::session::ConversationBinding {
        session_id: sid.into(),
        provenance: crate::session::ConversationProvenance::Asserted,
        transcript_path: None,
        execution: Some(crate::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec![source],
            configuration: vec![],
            exported_default_store: false,
            cwd: project,
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        }),
    });
    let swap = classify(&instance, &profile, "b");
    let ToolSwap::KeepConversation(Some(carry)) = swap else {
        panic!("the selected external store still requires a carry");
    };
    instance.swap_account("b");
    carry.run_for(&mut instance).unwrap();
    let destination = home.join("shared");
    assert_eq!(
        std::fs::read_to_string(destination.join(relative)).unwrap(),
        "selected conversation\n",
    );
    assert_eq!(
        instance
            .agent_session_binding
            .unwrap()
            .execution
            .unwrap()
            .stores,
        vec![destination.canonicalize().unwrap()],
    );
}

#[cfg(test)]
#[test]
#[serial_test::serial]
fn carry_preserves_known_conversation_already_in_destination() {
    let _app = crate::session::test_support::isolate_app_dir();
    let stub = tempfile::tempdir().unwrap();
    let _claude = crate::session::test_support::install_login_shell_path_command(
        stub.path(),
        "claude",
        "#!/bin/sh\nexit 1\n",
    );
    let home = dirs::home_dir().unwrap();
    let app = crate::session::get_app_dir().unwrap();
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("config.toml"),
        "[session.agent_detect_as]\na = \"claude\"\nb = \"claude\"\n\
         [session.agent_config_dir]\na = \"~/source\"\nb = \"~/destination\"\n",
    )
    .unwrap();
    let profile = crate::session::config::effective_profile("");
    let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
    crate::session::config::profile_config::resolve_config_or_warn(&profile);
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let sid = "11111111-2222-3333-4444-555555555555";
    let relative = Path::new("projects")
        .join(crate::session::capture::encode_claude_project_path(
            &project.to_string_lossy(),
        ))
        .join(format!("{sid}.jsonl"));
    for (root, contents, seconds) in [
        ("source", "unselected conversation\n", 200),
        ("destination", "selected conversation\n", 100),
    ] {
        let path = home.join(root).join(&relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
            )
            .unwrap();
    }
    let destination = home.join("destination").canonicalize().unwrap();
    let mut instance = Instance::new("carry", project.to_str().unwrap());
    instance.source_profile = profile.clone();
    instance.tool = "a".into();
    instance.command = "claude".into();
    instance.detect_as = "claude".into();
    instance.agent_session_id = Some(sid.into());
    instance.agent_session_binding = Some(crate::session::ConversationBinding {
        session_id: sid.into(),
        provenance: crate::session::ConversationProvenance::Asserted,
        transcript_path: None,
        execution: Some(crate::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec![destination.clone()],
            configuration: vec![],
            exported_default_store: false,
            cwd: project,
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        }),
    });
    let ToolSwap::KeepConversation(Some(carry)) = classify(&instance, &profile, "b") else {
        panic!("different configured roots must plan a carry");
    };
    instance.swap_account("b");
    carry.run_for(&mut instance).unwrap();
    assert_eq!(
        std::fs::read_to_string(destination.join(relative)).unwrap(),
        "selected conversation\n",
        "a newer unselected transcript must not overwrite the selected conversation",
    );
}

#[cfg(test)]
#[test]
#[serial_test::serial]
fn carry_refuses_unproven_destination_before_writing() {
    let _app = crate::session::test_support::isolate_app_dir();
    let home = dirs::home_dir().unwrap();
    let app = crate::session::get_app_dir().unwrap();
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("config.toml"),
        "[session.agent_detect_as]\na = \"claude\"\nb = \"claude\"\nc = \"claude\"\n\
         [session.custom_agents]\na = \"claude\"\nb = \"codex\"\nc = \"opaque-wrapper\"\n\
         [session.agent_config_dir]\na = \"~/source\"\nb = \"~/codex-target\"\nc = \"~/opaque-target\"\n",
    ).unwrap();
    let profile = crate::session::config::effective_profile("");
    let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
    crate::session::config::profile_config::resolve_config_or_warn(&profile);
    let project = home.join("project");
    let source = home.join("source");
    std::fs::create_dir_all(&project).unwrap();
    let sid = "11111111-2222-3333-4444-555555555555";
    let relative = Path::new("projects")
        .join(crate::session::capture::encode_claude_project_path(
            &project.to_string_lossy(),
        ))
        .join(format!("{sid}.jsonl"));
    std::fs::create_dir_all(source.join(relative.parent().unwrap())).unwrap();
    std::fs::write(source.join(&relative), "private Claude conversation\n").unwrap();
    let mut original = Instance::new("carry", project.to_str().unwrap());
    original.source_profile = profile.clone();
    original.tool = "a".into();
    original.command = "claude".into();
    original.detect_as = "claude".into();
    original.agent_session_id = Some(sid.into());
    original.agent_session_binding = Some(crate::session::ConversationBinding {
        session_id: sid.into(),
        provenance: crate::session::ConversationProvenance::Asserted,
        transcript_path: None,
        execution: Some(crate::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec![source],
            configuration: vec![],
            exported_default_store: false,
            cwd: project,
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        }),
    });
    let mut violations = Vec::new();
    for (tool, directory, command) in [
        ("b", "codex-target", "codex"),
        ("c", "opaque-target", "opaque-wrapper"),
    ] {
        let mut instance = original.clone();
        if let ToolSwap::KeepConversation(Some(carry)) = classify(&instance, &profile, tool) {
            instance.swap_account(tool);
            instance.command = command.into();
            let result = carry.run_for(&mut instance);
            if result.is_ok() || home.join(directory).exists() {
                violations.push((tool, result.is_ok(), home.join(directory).exists()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "unproven destinations accepted or written: {violations:?}"
    );
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

    /// Copy each planned transcript and report per-SID confirmation.
    ///
    /// Legacy best-effort behavior lives in [`Self::run`]: a missing or
    /// oversized source transcript is not an error there, because the launch
    /// falls back to a fresh conversation on its own. Relocating a known
    /// binding needs the opposite contract: the caller must know whether the
    /// transcript the binding names is actually present at the destination.
    pub fn execute(&self) -> Result<HashSet<String>> {
        let source = AnchoredDir::open(&self.source_root)
            .with_context(|| format!("opening {}", self.source_root.display()))?;
        std::fs::create_dir_all(&self.target_root)
            .with_context(|| format!("creating {}", self.target_root.display()))?;
        let target = AnchoredDir::open(&self.target_root)
            .with_context(|| format!("opening {}", self.target_root.display()))?;
        let mut confirmed = HashSet::new();
        for session_id in &self.session_ids {
            let relatives = claude_transcripts_for(&source, session_id)?;
            if relatives.is_empty() {
                continue;
            }
            let mut copied = true;
            for relative in relatives {
                if !copy_file_confirmed(&source, &target, &relative)? {
                    copied = false;
                }
            }
            if copied {
                confirmed.insert(session_id.clone());
            }
        }
        Ok(confirmed)
    }

    pub(crate) fn run_for(&self, instance: &mut Instance) -> Result<bool> {
        // Attest the incoming command's native identity and store before any
        // filesystem write: a Claude-labeled alias resolving to a codex or
        // opaque wrapper must not receive a Claude transcript. Resolving
        // without the source binding keeps the recorded store from masking the
        // configured destination.
        let destination = instance.attested_carry_destination()?;
        let mut bindings = [
            instance.agent_session_binding.clone(),
            instance.resume_binding.clone(),
        ];
        for binding in bindings.iter_mut().flatten() {
            if !binding.is_known() {
                continue;
            }
            let execution = binding.execution.as_ref().expect("known execution");
            anyhow::ensure!(
                execution.agent == "claude"
                    && execution.filesystem == "host"
                    && execution.stores.len() == 1,
                "conversation carry requires one host-accessible Claude store"
            );
            anyhow::ensure!(
                Instance::execution_matches_destination(execution, &destination),
                "incoming execution context differs from the attested destination"
            );
        }
        let mut relocated = false;
        for binding in bindings.iter_mut().flatten() {
            if !binding.is_known() {
                continue;
            }
            let execution = binding.execution.as_mut().expect("known execution");
            let source_root = execution.stores[0].canonicalize()?;
            let target_root = destination.stores[0].clone();
            if source_root == target_root {
                continue;
            }
            std::fs::create_dir_all(&target_root)?;
            let source = AnchoredDir::open(&source_root)?;
            let target = AnchoredDir::open(&target_root)?;
            // Scan by sid rather than encoding a required cwd: in a sandbox
            // the binding carries the physical host cwd while Claude wrote
            // the transcript under the container's cwd namespace, so a
            // cwd-derived path would miss the very file the store holds.
            let relatives = claude_transcripts_for(&source, &binding.session_id)?;
            anyhow::ensure!(
                !relatives.is_empty(),
                "could not carry conversation {}: no transcript in {}",
                binding.session_id,
                source_root.display()
            );
            for relative in relatives {
                anyhow::ensure!(
                    copy_file_confirmed(&source, &target, &relative)?,
                    "could not carry transcript {}",
                    relative.display()
                );
            }
            execution.stores[0] = target_root;
            relocated = true;
        }
        if relocated {
            let [agent, resume] = bindings;
            instance.agent_session_binding = agent;
            instance.resume_binding = resume;
        } else if !bindings.iter().flatten().any(|binding| binding.is_known()) {
            self.run();
        }
        Ok(relocated)
    }

    /// Best-effort legacy path: copy what is there, log failures, confirm
    /// nothing. The launch falls back to a fresh conversation the same way it
    /// does for any sid whose transcript is missing.
    pub fn run(&self) {
        let _ = self.execute().map_err(|error| {
            tracing::warn!(
                target: "session.store",
                source = %self.source_root.display(),
                target = %self.target_root.display(),
                error = %format_args!("{error:#}"),
                "could not carry the conversation to the new account; the agent starts fresh there"
            );
        });
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
/// Copy one transcript and report whether the conversation is readable at the
/// destination afterward. `true` covers a skipped newer destination copy too:
/// the file there is a regular transcript this carry found live, which is what
/// a resume reads. `false` is every silent skip (`copy_file` has several:
/// missing or oversized source, a staging name left in place, a publish that
/// lost the no-replace race) — those must not authorize a known binding's
/// relocation to a store the launch would then trust.
fn copy_file_confirmed(
    source: &AnchoredDir,
    target: &AnchoredDir,
    relative: &Path,
) -> Result<bool> {
    let Some(mut reader) = source.open_regular(relative, TRANSCRIPT_MAX_BYTES)? else {
        if source.regular_exists(relative) {
            tracing::warn!(
                target: "session.store",
                transcript = %relative.display(),
                max_bytes = TRANSCRIPT_MAX_BYTES,
                "transcript is larger than the carry cap and was not copied"
            );
        }
        return Ok(false);
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
        (Some(_), _) => return Ok(target.regular_exists(relative)),
    };
    let staging = staging_path(relative);
    // A leftover from a crashed carry: the publish below would hand over
    // whatever it holds, so start from a fresh file.
    target.remove_file(&staging)?;
    let Some(mut writer) = target.create_new_regular(&staging)? else {
        return Ok(false);
    };
    let copied = std::io::copy(&mut reader, &mut writer)
        .and_then(|_| writer.sync_all())
        .map_err(anyhow::Error::from)
        .and_then(|()| target.publish_staged(&staging, relative, replace));
    match copied {
        Ok(published) => Ok(published),
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

        let swap = classify(&inst, &profile, "claude-2");
        let ToolSwap::KeepConversation(Some(carry)) = swap else {
            panic!("an account swap with a conversation must plan a carry, got {swap:?}");
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

        let empty = temp.path().join("claude-3");
        std::fs::create_dir_all(&empty).unwrap();
        let untouched = temp.path().join("claude-4");
        carry(&empty, &untouched, &["sid-a"]).run();
        assert!(!untouched.join("projects").exists(), "no transcript, no-op");
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

    /// Newer wins: a fresher destination is kept, and a swap back to an account
    /// holding a pre-swap snapshot resumes the work done on the other account.
    #[test]
    fn run_keeps_the_newer_copy_in_either_direction() {
        let temp = tempfile::tempdir().unwrap();
        let account_a = temp.path().join("claude-1");
        let account_b = temp.path().join("claude-2");
        let on_a = seed_transcript(&account_a, "-work-repo", "sid-a", "first\n");
        let on_b = seed_transcript(&account_b, "-work-repo", "sid-a", "already here\n");
        set_age(&on_a, 3600);
        set_age(&on_b, 60);

        carry(&account_a, &account_b, &["sid-a"]).run();
        assert_eq!(std::fs::read_to_string(&on_b).unwrap(), "already here\n");

        std::fs::write(&on_b, "first\nsecond\n").unwrap();
        set_age(&on_b, 60);
        carry(&account_b, &account_a, &["sid-a"]).run();
        assert_eq!(std::fs::read_to_string(&on_a).unwrap(), "first\nsecond\n");
    }
}
