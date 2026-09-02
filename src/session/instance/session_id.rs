//! Acquiring the agent session id a launch resumes from.

use super::*;

impl Instance {
    /// Acquire a pre-launch session ID for the agent.
    ///
    /// Returns `(session_id, is_existing)`. Consults `resume_intent` first:
    /// `Use(sid)` returns the user-pinned target; `Cleared` skips both the
    /// observed sid and retroactive capture (forces a fresh start, generating
    /// a Claude UUID if applicable); `Default` verifies the observed sid
    /// against live tool state via `capture_freshest_session_id` (so a
    /// post-`/clear` session id supersedes a stale stored one), falls back
    /// to retroactive capture when no sid is observed, then to a fresh
    /// Claude UUID.
    pub fn acquire_session_id(&mut self) -> (Option<String>, bool) {
        // Both pre-mint decisions are made here rather than inside
        // acquire_session_id_with: it keeps the config read and the binary
        // probe off every other launch, and keeps the inner fn a pure,
        // testable seam.
        let preassign =
            self.capture_agent_name() == Some("opencode") && self.opencode_preassign_enabled();
        let pin_pi = self.pi_session_id_pinnable();
        self.acquire_session_id_with(&|path| {
            if pin_pi {
                return Some(crate::session::capture::generate_session_uuid());
            }
            preassign
                .then(|| crate::session::capture::preassign_opencode_session_id(path))
                .flatten()
        })
    }

    /// Session-id acquisition with the pre-mint step injected as a seam, so
    /// tests can drive the fresh-launch arms without a real opencode binary,
    /// network, or installed pi. Production wraps this with the live preassign
    /// helper and the Pi pin.
    fn acquire_session_id_with(
        &mut self,
        mint_fresh_id: &dyn Fn(&str) -> Option<String>,
    ) -> (Option<String>, bool) {
        match self.resume_intent.clone() {
            ResumeIntent::Use(sid) => {
                self.agent_session_id = Some(sid.clone());
                return (Some(sid), true);
            }
            ResumeIntent::Cleared => {
                self.agent_session_id = None;
                self.resume_probe_failed_sid = None;
                // The transcript belonged to the conversation being dropped.
                // `pi_resumable_transcript` would refuse it on the id check
                // anyway; not carrying it is one less thing depending on that.
                self.pi_session_path = None;
                let session_id = self.fresh_launch_session_id(mint_fresh_id);
                if let Some(ref id) = session_id {
                    self.agent_session_id = Some(id.clone());
                }
                return (session_id, false);
            }
            ResumeIntent::Fork { .. } => {
                // The child id was pre-generated and stored in
                // agent_session_id at creation. acquire returns it as the
                // session this instance owns; the actual fork flags
                // (--resume <parent> --fork-session --session-id <child>) are
                // emitted by apply_session_flags, which reads the parent off
                // the Fork intent. Report `false` (not an in-place resume): a
                // fork starts a new session.
                return (self.agent_session_id.clone(), false);
            }
            ResumeIntent::Default => {}
        }

        if let Some(stored) = self.agent_session_id.clone() {
            // Rebinding rather than returning early runs the observation
            // through the same empty-thread downgrade as the stored id below.
            // The SessionStart hook fires before Claude writes any content, so
            // the sidecar can legitimately name a thread with no transcript.
            let stored = match self.capture_freshest_session_id() {
                Some(fresh) => {
                    tracing::info!(
                        target: "session.store",
                        stale = %stored,
                        fresh = %fresh,
                        tool = %self.tool,
                        "Replacing stored session id with fresher live observation"
                    );
                    self.agent_session_id = Some(fresh.clone());
                    fresh
                }
                None => stored,
            };
            // A stored Claude sid with no transcript on disk is not resumable:
            // Claude minted the UUID at first launch but nothing was ever
            // written (an empty thread killed before the first prompt), so
            // `--resume <sid>` is a guaranteed launch failure that lands the
            // session in the "resume failed for sid ...; preserved for explicit
            // retry" state. Launch it as a fresh pinned session instead
            // (`is_existing = false` -> `--session-id <sid>`), which succeeds
            // and keeps the id stable so a later first prompt stays continuous.
            // Pi is pre-minted too, but it needs no equivalent branch: its
            // pin flag is also its create flag, so `apply_session_flags`
            // relaunches an unwritten pin with `--session-id` and pi recreates
            // the conversation under the same id (see
            // `resume_flag_arm_is_existing`). Host-only: a sandboxed transcript
            // lives inside the container, which may not be up at acquire time.
            // Keyed on the built-in binary rather than the resolved agent: a
            // wrapper is free to redirect `CLAUDE_CONFIG_DIR` itself, which
            // reads here as "no transcript" and would downgrade a live
            // conversation to a `--session-id` Claude rejects (#3399). A
            // declared `agent_config_dir` cannot widen this, since the probe
            // resolves the tree from the env var alone, and the env var can be
            // AoE's own rather than the one the wrapper exports. The cost is
            // that a wrapper session stopped before its first prompt attempts
            // one doomed `--resume`, reports Error, and starts fresh on the
            // next launch; losing a live conversation is the worse trade.
            if self.tool == "claude"
                && !self.is_sandboxed()
                && crate::session::capture::claude_host_transcript_confirmed_absent(
                    &self.project_path,
                    &stored,
                    &self.resolved_host_environment(),
                )
            {
                tracing::info!(
                    target: "session.store",
                    sid = %stored,
                    "stored Claude sid has no transcript on disk; launching fresh \
                     with --session-id instead of --resume to avoid a certain \
                     resume failure"
                );
                return (Some(stored), false);
            }
            return (Some(stored), true);
        }

        let tmux_exists = self.tmux_session().is_ok_and(|s| s.exists());
        if tmux_exists {
            if let Some(id) = self.try_retroactive_capture() {
                tracing::info!(target: "session.store",
                    "Retroactive capture found session ID for {}: {}",
                    self.tool,
                    id
                );
                self.agent_session_id = Some(id);
                return (self.agent_session_id.clone(), true);
            }
        }

        let session_id = self.fresh_launch_session_id(mint_fresh_id);

        if let Some(ref id) = session_id {
            tracing::debug!(target: "session.store", "Session ID for {}: {}", self.tool, id);
            self.agent_session_id = session_id.clone();
        }

        (session_id, false)
    }

    /// Mint the session id for a brand-new launch. Claude pre-mints a UUID
    /// (`--session-id`); Pi pre-mints one too when its binary takes that flag
    /// (see `pi_session_id_pinnable`); opencode optionally pre-creates its
    /// session through the injected seam (opt-in, returns `None` when disabled
    /// or on failure, deferring to the SQLite poller); every other agent
    /// starts without a pinned id and is captured post-launch.
    ///
    /// Resolved through `agent_detect_as`, so a wrapper mints what its
    /// built-in would. The pi arm is unreachable for one even so: its pin
    /// needs a binary AoE probed on its own PATH, which no wrapper is (see
    /// `pi_session_id_pinnable`), and the seam returns `None` for it.
    fn fresh_launch_session_id(
        &self,
        mint_fresh_id: &dyn Fn(&str) -> Option<String>,
    ) -> Option<String> {
        match self.capture_agent_name() {
            Some("claude") => Some(generate_session_uuid()),
            Some("opencode" | "pi") => mint_fresh_id(&self.project_path),
            _ => None,
        }
    }

    /// Whether opt-in opencode session-id preassignment applies to this launch.
    /// Host sessions only: the preassign POST targets a loopback `opencode
    /// serve` a sandboxed agent cannot reach, so containers keep polling.
    fn opencode_preassign_enabled(&self) -> bool {
        if self.is_sandboxed() {
            return false;
        }
        let profile = self.effective_profile();
        if !crate::session::profile_config::resolve_config_or_warn(&profile)
            .session
            .opencode_preassign_session_id
        {
            return false;
        }
        self.opencode_launch_mirrorable_by_ambient_serve()
    }

    /// Whether the ephemeral `opencode serve` used for preassignment provably
    /// hits the same binary and data store as the real launch.
    ///
    /// Preassignment spawns the ambient `opencode` with AoE's own environment.
    /// A command override swaps the binary, and profile-scoped host env can
    /// redirect opencode's data store (e.g. `XDG_DATA_HOME` / `OPENCODE_DB`);
    /// in either case the preassigned id would land in a different store, so
    /// `opencode --session <id>` would fail "Session not found" instead of
    /// gracefully falling back. When this returns false we skip preassignment
    /// and defer to the poller, which reads that same ambient store.
    fn opencode_launch_mirrorable_by_ambient_serve(&self) -> bool {
        !self.has_command_override() && self.profile_host_environment().is_empty()
    }

    /// Best-effort backfill of a missing `agent_session_id` from a read-only
    /// CLI command (`aoe status`, `aoe session show`).
    ///
    /// A capture-deferred agent launched purely through the CLI with no
    /// `aoe serve` daemon and no TUI has no long-lived loop draining its
    /// session-id poller. For an agent whose session store is populated lazily
    /// (opencode writes its SQLite `session` row only on the first user turn,
    /// well after the bounded launch-time wait in
    /// [`crate::session::sync::capture_launched_session_id_blocking`] has
    /// elapsed), the id is never observed at launch and, absent a TUI/daemon,
    /// is never recovered. This heals it: the next time the user inspects the
    /// session, read the tool's store directly (the same
    /// [`Self::try_retroactive_capture`] path the TUI/daemon use at next
    /// launch, which needs no live poller) and persist through the guarded
    /// [`persist_session_to_storage`] CAS.
    ///
    /// Gated so it can never adopt a wrong id: only a session that still owns
    /// no id (`agent_session_id.is_none()`), has a plain resume intent
    /// (`ResumeIntent::Default`, so a user-cleared, pinned, or fork-seeded id
    /// is left alone), is not mid-teardown or mid-creation (`Deleting` /
    /// `Creating`), is still in the active bucket (an archived or trashed row
    /// is a sink a read command must not mutate, and `--no-kill` archiving or a
    /// not-yet-torn-down trashed row can still own a live pane), does not share
    /// its cwd with another id-less session of the same tool (`contended`, see
    /// [`Self::contended_capture_cwds`]), and has a live tmux session is
    /// eligible. The live-tmux check is the real liveness guard; the status and
    /// bucket checks skip the rows a read command must leave alone regardless
    /// of pane state. A captured id equal to `resume_probe_failed_sid` is
    /// rejected so a known-bad id the resume cascade already abandoned is not
    /// re-adopted. The sandboxed arms of `try_retroactive_capture` already
    /// return `None` when the container is down, so nothing else is needed for
    /// that case.
    ///
    /// Best-effort: any miss (no id observable yet, a peer already owns it, a
    /// CAS race) is a silent no-op, so a read command never fails or stalls on
    /// this. `aoe status --json` reports only status counts, so the backfill is
    /// invisible there; `aoe session show --json` does surface the healed
    /// `agent_session_id`, and either way the info-level "backfilled
    /// agent_session_id" log below records it.
    pub(crate) fn self_heal_session_id(
        &mut self,
        profile: &str,
        contended: &HashSet<(String, String)>,
    ) {
        if self.agent_session_id.is_some()
            || !self.resume_intent.is_default()
            || matches!(self.status, Status::Deleting | Status::Creating)
            || self.effective_bucket() != SessionBucket::Active
            || contended.contains(&self.contended_capture_key())
        {
            return;
        }
        if !self.tmux_alive_cached() {
            return;
        }
        let file_watch = self.resolve_file_watch();
        let ownership: Result<_> = (|| {
            let storage = crate::session::storage::Storage::new(profile, file_watch.clone())?;
            let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&self.id)?;
            let generation = storage.update(|instances, _groups| {
                let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id)
                else {
                    anyhow::bail!("session disappeared before capture");
                };
                if stored.agent_session_id.is_some()
                    || !stored.resume_intent.is_default()
                    || matches!(stored.status, Status::Deleting | Status::Creating)
                    || stored.effective_bucket() != SessionBucket::Active
                {
                    anyhow::bail!("session is no longer eligible for capture");
                }
                stored
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Capture,
                        Self::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .map_err(|error| anyhow::anyhow!("capture blocked: {error}"))
            })?;
            Ok((storage, lifecycle_lock, generation))
        })();
        let Ok((storage, _lifecycle_lock, generation)) = ownership else {
            return;
        };
        let captured = self.try_retroactive_capture();
        let applied = captured.as_ref().is_some_and(|captured| {
            self.resume_probe_failed_sid.as_deref() != Some(captured.as_str())
                && persist_session_to_storage(profile, &self.id, captured, None, &file_watch)
                    == SidWrite::Applied
        });
        let released = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            Ok(stored
                .release_lifecycle_reservation_if_owned(LifecycleOperation::Capture, generation))
        });
        if !matches!(released, Ok(true)) {
            tracing::warn!(
                target: "session.sync",
                instance = %self.id,
                "self-heal capture lost its lifecycle reservation before release",
            );
            return;
        }
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = None;
        if applied {
            self.agent_session_id = captured;
            self.resume_probe_failed_sid = None;
            tracing::info!(
                target: "session.store",
                instance = %self.id,
                tool = %self.tool,
                "backfilled agent_session_id from a read-only CLI command; \
                 resume is now available without a TUI or daemon",
            );
        }
    }

    /// Returns `Some(fresh)` when the live tool state shows a session id
    /// distinct from `self.agent_session_id`, otherwise `None`. Reuses
    /// the per-tool dispatch in `try_retroactive_capture` so the freshness
    /// contract (mtime, SQLite ordering, exclusion set, host/container)
    /// stays encapsulated in each tool's existing capture function.
    ///
    /// For Claude the authoritative per-instance sidecar
    /// (`/tmp/aoe-hooks-<euid>/<instance_id>/session_id`, written by the
    /// SessionStart / UserPromptSubmit hooks) is consulted first. It is keyed
    /// by instance id, so it can never name a peer instance's conversation,
    /// unlike the mtime disk scan, which picks the most-recent jsonl in the
    /// shared `~/.claude/projects/<encoded-cwd>/` dir and so can select a
    /// co-located peer's session when several AoE sessions share one cwd
    /// (#2344). The mtime scan is only used as a fallback when no fresh
    /// sidecar exists (e.g. an old session resumed after the 5-minute
    /// sidecar window), matching the ordering already used by
    /// `claude_poll_fn`. Sandboxed Claude is included: its `SessionStart`
    /// hook writes through the `/tmp/aoe-hooks/<id>` bind-mount onto the
    /// host path, so `read_hook_session_id` reads it the same way, and the
    /// mtime fallback below still routes through the container-aware branch
    /// of `try_retroactive_capture`.
    ///
    /// Two deliberate divergences from `claude_poll_fn`, both correct for the
    /// resume context: (1) an excluded sidecar id returns `None` here rather
    /// than falling through to the mtime scan, since falling through is what
    /// re-opens #2344; (2) this reader and `claude_poll_fn` read the same
    /// sidecar without a shared snapshot, so a hook rotation between the two
    /// reads can briefly surface different UUIDs, benign under the existing
    /// eventual-consistency capture model.
    pub(crate) fn capture_freshest_session_id(&self) -> Option<String> {
        // Pi's extension publishes the pane's own conversation to the same
        // sidecar, so this is the one Pi observation that names a pane.
        if self.tool == "pi" {
            let authoritative = self.pi_published_session_id(false)?;
            if self.retroactive_capture_excludes.contains(&authoritative) {
                return None;
            }
            return override_if_distinct(self.agent_session_id.as_deref(), authoritative);
        }
        if self.capture_agent_name() == Some("claude") {
            if let Some(authoritative) = crate::hooks::read_hook_session_id(&self.id) {
                if self.retroactive_capture_excludes.contains(&authoritative) {
                    return None;
                }
                return override_if_distinct(self.agent_session_id.as_deref(), authoritative);
            }
        }
        // Kimi and Pi: a shared store refuses the scan entirely inside
        // try_retroactive_capture and surfaces here as None, so a Some from
        // the call below implies the store was sole-owned and the fresher
        // observation attributable. Execution reaches this line for every
        // tool; the gating lives in the callee.
        let live = self.try_retroactive_capture()?;
        override_if_distinct(self.agent_session_id.as_deref(), live)
    }

    #[cfg(test)]
    pub(crate) fn mark_pi_extension_launched_for_test(&mut self) {
        self.pi_extension_launched = true;
    }

    /// The `-e <extension>` flag and sidecar env var a Pi launch needs to
    /// publish its own conversation, or `None` when it cannot.
    ///
    /// The extension reports every `session_start`, so a `/new` inside the
    /// pane is attributed to it rather than inferred from a store keyed by
    /// cwd. Requires a binary AoE can vouch for on the host; a sandboxed pane
    /// runs the container's pi, which is why the paths differ: the extension
    /// and the instance dir are both bind-mounted in, so the flag and the env
    /// var name container paths (see `container_config::pi_extension_mounts`).
    pub(super) fn pi_extension_launch(&self) -> Option<(String, String)> {
        if self.tool != "pi" || self.has_command_override() {
            return None;
        }
        if self.is_sandboxed() {
            if self.declares_agent_config_dir() {
                tracing::warn!(target: "session.instance",
                    "session {} declares session.agent_config_dir for pi; publishing its conversation needs a config dir AoE mounts itself, so this session falls back to store polling",
                    self.id
                );
                return None;
            }
            // No `-e`: pi refuses to start when an `-e` path is missing, and a
            // container created before this change has no mount for one. The
            // extension is written where pi discovers it, inside the config
            // bind every Pi container already has, and the sidecar is published
            // into that same bind.
            if crate::session::container_config::install_pi_sandbox_extension().is_err() {
                return None;
            }
            return Some((
                String::new(),
                format!(
                    "AOE_PI_SESSION_ID_FILE={}/{}/session_id ",
                    crate::session::container_config::PI_SIDECAR_DIR_IN_CONTAINER,
                    self.id
                ),
            ));
        }
        if super::launch_command::environment_defines_path(&self.resolved_host_environment())
            || !crate::agents::pi_supports_extension_flag()
        {
            return None;
        }
        let extension = super::launch_command::pi_extension_path().ok()?;
        let sidecar = crate::hooks::ensure_instance_dir_path(&self.id)
            .ok()?
            .join("session_id");
        Some((
            format!(" -e {}", shell_escape(&extension.to_string_lossy())),
            format!(
                "AOE_PI_SESSION_ID_FILE={} ",
                shell_escape(&sidecar.to_string_lossy())
            ),
        ))
    }

    /// Whether this Pi pane publishes its conversation through the AoE
    /// extension, which is what makes its observations name a pane.
    ///
    /// Read from what the launch did, not from the binary probe: an upgrade
    /// mid-session must not reclassify a pane that is already running.
    /// The conversation this pane published, whichever side of the container
    /// boundary it published on. `any_age` drops the freshness window, which a
    /// final flush wants and a resume does not.
    pub(crate) fn pi_published_session_id(&self, any_age: bool) -> Option<String> {
        match self.pi_sidecar_source()? {
            PiSidecarSource::HostHooks => {
                if any_age {
                    crate::hooks::read_hook_session_id_any_age(&self.id)
                } else {
                    crate::hooks::read_hook_session_id(&self.id)
                }
            }
            PiSidecarSource::SandboxDir(dir) => {
                let raw = std::fs::read_to_string(dir.join("session_id")).ok()?;
                let id = raw.trim();
                uuid::Uuid::parse_str(id).ok().map(|_| id.to_string())
            }
        }
    }

    /// The transcript path this pane published, as the pane sees it. In a
    /// container that is a `/root/.pi/...` path, which is what pi's argv needs;
    /// `pi_host_view_of` maps it back for host-side checks.
    pub(crate) fn pi_published_session_path(&self) -> Option<String> {
        match self.pi_sidecar_source()? {
            PiSidecarSource::HostHooks => crate::hooks::read_hook_session_path(&self.id),
            PiSidecarSource::SandboxDir(dir) => {
                let raw = std::fs::read_to_string(dir.join("session_path")).ok()?;
                let path = raw.trim();
                path.starts_with('/').then(|| path.to_string())
            }
        }
    }

    /// Where this pane publishes, or `None` when that cannot be established.
    ///
    /// `None` is the fail-closed answer and never means "try the host": a
    /// sandboxed pane whose bind-backed path will not resolve must not read
    /// the host hook directory, or it adopts a conversation from another
    /// namespace, which is the attribution bug this change exists to remove.
    pub(crate) fn pi_sidecar_source(&self) -> Option<PiSidecarSource> {
        if self.is_sandboxed() {
            return self.pi_sandbox_sidecar().map(PiSidecarSource::SandboxDir);
        }
        Some(PiSidecarSource::HostHooks)
    }

    /// Whether this session's agent reads a config dir the profile named
    /// (`session.agent_config_dir`) rather than the one AoE stages.
    ///
    /// Both sidecar paths derive from AoE's own config bind: the host side from
    /// `pi_sandbox_dir`, the container side from `PI_SIDECAR_DIR_IN_CONTAINER`.
    /// A directory the user named is mounted by their own `extra_volumes`
    /// entry, whose container path AoE never sees, so neither side can be
    /// derived and such a session neither publishes a sidecar nor reads one
    /// (see `pi_config_bind_dir`).
    fn declares_agent_config_dir(&self) -> bool {
        *self.agent_config_dir_declared.get_or_init(|| {
            dirs::home_dir().is_some_and(|home| {
                crate::session::profile_config::resolve_config_or_warn(&self.effective_profile())
                    .session
                    .agent_config_dir_for(&self.tool, &home)
                    .is_some()
            })
        })
    }

    /// Host side of the Pi config bind for this sandboxed pane, or `None` when
    /// the session declares its own config dir.
    ///
    /// The gate belongs here rather than only at launch: the bind is writable
    /// from the container and a sidecar can outlive the config change that
    /// declared the directory, so a read of the staged dir could attribute a
    /// conversation this pane never published.
    fn pi_config_bind_dir(&self) -> Option<std::path::PathBuf> {
        if self.declares_agent_config_dir() {
            return None;
        }
        crate::session::container_config::pi_sandbox_dir()
    }

    /// Host directory backing this sandboxed pane's sidecar.
    fn pi_sandbox_sidecar(&self) -> Option<std::path::PathBuf> {
        crate::session::validate_instance_id(&self.id).ok()?;
        Some(
            self.pi_config_bind_dir()?
                .join("aoe-session")
                .join(&self.id),
        )
    }

    /// A published path as the host filesystem sees it. The Pi config dir is
    /// bound at `/root/.pi` inside the container, so a transcript published
    /// there lives under the sandbox dir here; checking the container path
    /// verbatim on the host finds nothing and would discard a valid transcript.
    fn pi_host_view_of(&self, published: &str) -> Option<std::path::PathBuf> {
        if !self.is_sandboxed() {
            return Some(std::path::PathBuf::from(published));
        }
        let rest = published.strip_prefix("/root/.pi/")?;
        Some(self.pi_config_bind_dir()?.join(rest))
    }

    /// The transcript to resume by path, when the pane published one that
    /// still exists and belongs to the conversation we hold.
    ///
    /// Pi names its files `<timestamp>_<uuid>.jsonl`, so the id check is what
    /// keeps a path left over from a previous conversation (a `/new` the drain
    /// recorded but no stop flushed) from resuming the wrong one.
    fn pi_resumable_transcript(&self) -> Option<String> {
        let path = self.pi_session_path.as_deref()?;
        let id = self.agent_session_id.as_deref()?;
        let name = std::path::Path::new(path).file_name()?.to_str()?;
        // `<timestamp>_<uuid>.jsonl`, matched on the whole id segment. A
        // substring test would let a partial pin (which `set-session-id`
        // accepts) match a timestamp digit or another file's uuid.
        let names_this_conversation = name
            .rsplit_once('_')
            .and_then(|(_, tail)| tail.strip_suffix(".jsonl"))
            .is_some_and(|uuid| uuid == id);
        let exists = self
            .pi_host_view_of(path)
            .is_some_and(|host_path| host_path.is_file());
        (names_this_conversation && exists).then(|| path.to_string())
    }

    /// Record the conversation and transcript the pane published, if any.
    pub(super) fn absorb_published_pi_session(&mut self) {
        if self.tool != "pi" {
            return;
        }
        if let Some(path) = self.pi_published_session_path() {
            self.pi_session_path = Some(path);
        }
    }

    pub(crate) fn uses_pi_session_sidecar(&self) -> bool {
        self.tool == "pi"
            && self.pi_sidecar_source().is_some()
            && (self.pi_extension_launched || self.pi_sidecar_exists())
    }

    /// Whether a sidecar exists for this pane, looked for where the pane
    /// publishes: the bind-backed directory for a container, the per-instance
    /// hook directory for a host pane.
    ///
    /// This is the reload path. `pi_extension_launched` is runtime state, so a
    /// daemon or TUI that reloads a still-live session has only the file to go
    /// on, and looking in the wrong place makes a publishing pane read as a
    /// silent one: no poller repair, and a final flush that returns early.
    fn pi_sidecar_exists(&self) -> bool {
        match self.pi_sidecar_source() {
            Some(PiSidecarSource::SandboxDir(dir)) => dir.join("session_id").is_file(),
            Some(PiSidecarSource::HostHooks) => crate::hooks::session_id_sidecar_exists(&self.id),
            None => false,
        }
    }

    /// Whether this session may pin its Pi conversation with `--session-id`.
    ///
    /// Requires a binary AoE can vouch for, the probe running `pi` from AoE's
    /// own PATH: a command override, a sandboxed launch, and a profile setting
    /// `PATH` all launch unpinned and defer to the floored poller.
    ///
    /// The tool check comes first so no other agent's launch pays for the
    /// `pi --help` probe.
    fn pi_session_id_pinnable(&self) -> bool {
        self.tool == "pi"
            && !self.has_command_override()
            && !self.is_sandboxed()
            && !super::launch_command::environment_defines_path(&self.resolved_host_environment())
            && crate::agents::pi_supports_session_id_flag()
    }

    /// Whether a launch emits the `existing` arm of the agent's
    /// [`ResumeStrategy`].
    ///
    /// It tracks `is_existing` except for Pi on a pinnable binary, where the
    /// pinning arm serves both: pi writes its session file on the first
    /// message, so a pane pinned and never prompted holds an id `--session`
    /// exits 1 on, and `--session-id` recreates it.
    fn resume_flag_arm_is_existing(
        &self,
        is_existing: bool,
        pi_pinnable: bool,
        session_id: Option<&str>,
        explicitly_pinned: bool,
    ) -> bool {
        // `--session-id` searches this project only and creates the
        // conversation when it is absent, so it is for ids AoE minted. A value
        // the user pinned keeps `--session`, which resolves partials and
        // searches wider; its shape says nothing about its origin.
        let takes_pinning_arm = self.tool == "pi"
            && pi_pinnable
            && !explicitly_pinned
            && session_id.is_some_and(|sid| uuid::Uuid::parse_str(sid).is_ok());
        is_existing && !takes_pinning_arm
    }

    /// Whether a `ResumeStrategy::Subcommand` (or `ForkStrategy::CodexFork`)
    /// token can be spliced into this launch command.
    ///
    /// [`splice_subcommand_or_append`] puts the token after the first
    /// whitespace-delimited word, so it reaches the agent only when that word
    /// is the program the pane runs. A single-word command is that program
    /// whatever it is named, which is the wrapper shape `custom_agents` and
    /// `agent_command_override` document (a script that exports something and
    /// execs the agent with `"$@"`). A multi-word command hides the binary
    /// behind a launcher, and `ssh -t host codex` would take `resume <sid>` on
    /// `ssh` and restart the wrong program, so those emit no resume unless the
    /// first word is the agent's own binary (#3638).
    fn subcommand_splice_is_safe(&self, agent: &crate::agents::AgentDef) -> bool {
        if !self.has_command_override() {
            return true;
        }
        let mut words = self.command.split_whitespace();
        let Some(first) = words.next() else {
            return false;
        };
        words.next().is_none()
            || first == agent.binary
            || first.ends_with(&format!("/{}", agent.binary))
    }

    pub(super) fn apply_session_flags(&mut self, cmd: &mut String, context: &str) -> bool {
        // Owned: the rest of this fn takes `&mut self`, and the unresolved
        // fallback borrows `self.tool`.
        let resolved = self.capture_agent_name().unwrap_or(&self.tool).to_string();
        let resume_tool = resolved.as_str();
        if let ResumeIntent::Fork { from } = self.resume_intent.clone() {
            let child = self.agent_session_id.clone();
            if let Some(child_id) = child.as_deref() {
                let fork_part = build_fork_flags(resume_tool, &from, child_id);
                if !fork_part.is_empty() {
                    // Codex's fork is a subcommand and must sit right after the
                    // binary (before other flags), like its resume subcommand.
                    // Flag-shaped forks (claude, opencode) append.
                    let is_subcommand = matches!(
                        crate::agents::get_agent(resume_tool).map(|a| &a.fork_strategy),
                        Some(crate::agents::ForkStrategy::CodexFork)
                    );
                    let splice_ok = !is_subcommand
                        || crate::agents::get_agent(resume_tool)
                            .is_some_and(|a| self.subcommand_splice_is_safe(a));
                    if splice_ok {
                        splice_subcommand_or_append(cmd, &fork_part, is_subcommand);
                    } else {
                        tracing::warn!(target: "session.store",
                            tool = %self.tool,
                            command = %self.command,
                            "fork subcommand needs the binary's position and this command hides it behind a launcher; launching without it"
                        );
                    }
                }
            }
            // A fork is a fresh session, not an in-place resume.
            return false;
        }
        // Read before acquisition: the `Use` intent is what marks an id the
        // user pinned rather than one AoE minted or captured.
        let explicitly_pinned = matches!(self.resume_intent, ResumeIntent::Use(_));
        self.absorb_published_pi_session();
        let (mut session_id, is_existing) = self.acquire_session_id();
        // Which ResumeStrategy arm to emit. Pi diverges from `is_existing`
        // (see `resume_flag_arm_is_existing`), so the launch flag and the
        // "this was a resume" answer this fn returns are decided separately.
        let flag_arm_is_existing = self.resume_flag_arm_is_existing(
            is_existing,
            self.pi_session_id_pinnable(),
            session_id.as_deref(),
            explicitly_pinned,
        );
        // Sandboxed Copilot, Kimi, and Prime Agent start fresh: their session
        // stores live inside the container (Copilot's SQLite db, Kimi's
        // `~/.kimi-code/session_index.jsonl`, Prime Agent's
        // `~/.prime/agent/sessions/*.jsonl`), so a host-captured or manually
        // pinned sid would launch `--resume <id>` against an id that does
        // not resolve there. Capture is already host-only above; drop the sid
        // to gate emission too.
        if matches!(resume_tool, "copilot" | "kimi" | "prime-agent") && self.is_sandboxed() {
            session_id = None;
        }
        // Same fail-closed rule for a subcommand-shaped resume whose splice
        // point this command hides: a mangled launch line is worse than a
        // fresh conversation.
        if let Some(agent) = crate::agents::get_agent(resume_tool) {
            if matches!(
                agent.resume_strategy,
                crate::agents::ResumeStrategy::Subcommand(_)
            ) && !self.subcommand_splice_is_safe(agent)
            {
                tracing::warn!(target: "session.store",
                    tool = %self.tool,
                    command = %self.command,
                    "resume subcommand needs the binary's position and this command hides it behind a launcher; starting fresh"
                );
                session_id = None;
            }
        }
        // A transcript the pane published outranks its id: `--session <path>`
        // resolves the conversation wherever it was started, while
        // `--session-id` looks only in the current project and would create an
        // empty one under the same uuid after a worktree move.
        // Never over an explicit pin: the user named a conversation, and a
        // stored path is AoE's own bookkeeping.
        if is_existing && !explicitly_pinned && session_id.is_some() {
            if let Some(path) = self.pi_resumable_transcript() {
                let flags = format!("--session {}", shell_escape(&path));
                splice_subcommand_or_append(cmd, &flags, false);
                tracing::debug!(target: "session.store", "Added resume flags to {} command: {}", context, flags);
                return true;
            }
        }
        let emitted = append_resume_flags(
            resume_tool,
            session_id.as_deref(),
            flag_arm_is_existing,
            cmd,
            context,
        );
        is_existing && emitted
    }

    /// Persist an ambiguous resume-probe failure without clearing the durable
    /// resume sid. The CAS guard keeps peer sid changes authoritative.
    pub(super) fn mark_resume_probe_failed(&mut self, profile: &str, sid: &str) -> SidWrite {
        let storage =
            match crate::session::storage::Storage::new(profile, self.resolve_file_watch()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: "session.store",
                        "Failed to create storage for resume-probe failure marker for {}: {}",
                        self.id,
                        e
                    );
                    return SidWrite::Failed;
                }
            };

        let instance_id = self.id.clone();
        let sid_for_closure = sid.to_string();
        let outcome = storage.update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) else {
                return Ok(SidWrite::Failed);
            };

            if inst.agent_session_id.as_deref() != Some(sid_for_closure.as_str()) {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected_sid = %sid_for_closure,
                    disk_sid = ?inst.agent_session_id,
                    "sid CAS mismatch in resume-probe failure marker; skipping write"
                );
                return Ok(SidWrite::Skipped);
            }

            inst.resume_probe_failed_sid = Some(sid_for_closure.clone());
            Ok(SidWrite::Applied)
        });

        match outcome {
            Ok(write @ (SidWrite::Applied | SidWrite::Skipped)) => {
                if let Ok(insts) = storage.load() {
                    if let Some(disk) = insts.into_iter().find(|i| i.id == self.id) {
                        self.agent_session_id = disk.agent_session_id;
                        self.resume_intent = disk.resume_intent;
                        self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                    }
                }
                write
            }
            Ok(SidWrite::Failed) => {
                tracing::warn!(target: "session.store",
                    "Resume-probe failure marker found no instance row for {}",
                    self.id
                );
                SidWrite::Failed
            }
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to mark resume-probe failure for {}: {}",
                    self.id,
                    e
                );
                SidWrite::Failed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::launch_command::build_resume_flags;
    use crate::session::instance::test_helpers::*;
    use crate::session::test_support::EnvGuard;
    use serial_test::serial;
    use tempfile::tempdir;

    // Tests for agent_session_id field
    #[test]
    fn test_agent_session_id_none_by_default() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_agent_session_id_serialization() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.agent_session_id = Some("session-123".to_string());

        let json = serde_json::to_string(&inst).unwrap();
        let deserialized: Instance = serde_json::from_str(&json).unwrap();

        assert_eq!(
            deserialized.agent_session_id,
            Some("session-123".to_string())
        );
    }

    #[test]
    fn test_agent_session_id_skips_none() {
        let inst = Instance::new("test", "/tmp/test");
        let json = serde_json::to_string(&inst).unwrap();

        // agent_session_id should not appear in JSON when None
        assert!(!json.contains("agent_session_id"));
    }

    #[test]
    fn test_agent_session_id_defaults_to_none() {
        let json = r#"{"id":"test123","title":"Test","project_path":"/tmp/test","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z"}"#;
        let inst: Instance = serde_json::from_str(json).unwrap();

        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_opencode_acquire_returns_none_for_deferred_capture() {
        let mut inst = Instance::new("Test", "/nonexistent/opencode/test");
        inst.tool = "opencode".to_string();

        let (session_id, is_existing) = inst.acquire_session_id();

        assert!(session_id.is_none());
        assert!(!is_existing);
        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_persisted_opencode_session_id_reused() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some("oc-session-42".to_string());

        let (session_id, is_existing) = inst.acquire_session_id();

        assert_eq!(session_id, Some("oc-session-42".to_string()));
        assert!(is_existing);
    }

    // Test that instance with agent_session_id can be serialized and deserialized
    #[test]
    fn test_instance_with_agent_session_id_roundtrip() {
        let mut inst = Instance::new("Test", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("session-abc-123".to_string());

        let json = serde_json::to_string(&inst).unwrap();
        let deserialized: Instance = serde_json::from_str(&json).unwrap();

        assert_eq!(inst.id, deserialized.id);
        assert_eq!(inst.title, deserialized.title);
        assert_eq!(inst.project_path, deserialized.project_path);
        assert_eq!(inst.tool, deserialized.tool);
        assert_eq!(inst.agent_session_id, deserialized.agent_session_id);
    }

    #[test]
    fn test_persisted_session_id_reused_when_already_set() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("session-42".to_string());

        // A persisted sid is returned as the session this instance owns. The
        // `--resume` vs `--session-id` decision (is_existing) is
        // transcript-dependent for Claude and is covered hermetically in
        // `verify_on_resume`; asserting it here would read the developer's real
        // `~/.claude`.
        let (session_id, _is_existing) = inst.acquire_session_id();
        assert_eq!(session_id, Some("session-42".to_string()));
    }

    #[test]
    fn test_persisted_session_id_reused_for_unsupported_agent() {
        // The cache-hit path is generic across agents; a persisted ID is
        // returned regardless of whether the agent supports resume yet.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.agent_session_id = Some("sess-99".to_string());

        let (session_id, is_existing) = inst.acquire_session_id();

        assert_eq!(session_id, Some("sess-99".to_string()));
        assert!(is_existing);
    }

    #[test]
    fn test_resume_with_arbitrary_session_id() {
        let mut inst = Instance::new("Test", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("invalid-session-id".to_string());

        // With an existing (persisted) session, should use --resume
        let flags = build_resume_flags(&inst.tool, inst.agent_session_id.as_ref().unwrap(), true);
        assert_eq!(flags, "--resume invalid-session-id");

        // A fresh (no prior transcript) launch pins the id instead.
        let flags = build_resume_flags(&inst.tool, inst.agent_session_id.as_ref().unwrap(), false);
        assert_eq!(flags, "--session-id invalid-session-id");

        // The method returns the persisted id as the owned session. The
        // is_existing flag is transcript-dependent for Claude (see
        // `verify_on_resume`) and would read the real `~/.claude` here.
        let (session_id, _is_existing) = inst.acquire_session_id();
        assert_eq!(session_id, Some("invalid-session-id".to_string()));
    }

    #[test]
    fn fork_intent_emits_resume_fork_session_and_pins_child() {
        let flags = build_fork_flags(
            "claude",
            "parent-1111-2222-3333-444444444444",
            "child-5555-6666-7777-888888888888",
        );
        assert_eq!(
            flags,
            "--resume parent-1111-2222-3333-444444444444 --fork-session --session-id child-5555-6666-7777-888888888888"
        );
    }

    #[test]
    fn acquire_session_id_fork_pins_child_and_reports_fresh() {
        let mut inst = Instance::new("Forked", "/tmp/x");
        inst.tool = "claude".to_string();
        // The child id was pre-generated and stored in agent_session_id at
        // creation; the Fork intent carries the parent to resume from.
        inst.agent_session_id = Some("child-5555-6666-7777-888888888888".to_string());
        inst.resume_intent = ResumeIntent::Fork {
            from: "parent-1111-2222-3333-444444444444".to_string(),
        };
        let mut cmd = "claude".to_string();
        let is_existing = inst.apply_session_flags(&mut cmd, "test");
        assert_eq!(
            cmd,
            "claude --resume parent-1111-2222-3333-444444444444 --fork-session --session-id child-5555-6666-7777-888888888888"
        );
        // A fork is a NEW session (not a resume-in-place), so report not-existing.
        assert!(!is_existing);
        // The child id we will resume from here on stays pinned in agent_session_id.
        assert_eq!(
            inst.agent_session_id.as_deref(),
            Some("child-5555-6666-7777-888888888888")
        );
    }

    #[test]
    fn sandboxed_host_only_capture_agents_drop_pinned_sid_at_emission() {
        // The apply_session_flags gate exists so a pinned or host-captured
        // resume id is never launched inside a container whose own sessions
        // store starts empty (copilot | kimi | prime-agent). Pin the
        // prime-agent arm: deleting it from the matches! must fail here.
        let sid = "11111111-2222-3333-4444-555555555555";
        for tool in ["copilot", "kimi", "prime-agent"] {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.tool = tool.to_string();
            inst.agent_session_id = Some(sid.to_string());
            inst.resume_intent = ResumeIntent::Use(sid.to_string());
            inst.sandbox_info = Some(SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test-image".to_string(),
                container_name: "test".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            let mut cmd = tool.to_string();
            let resumed = inst.apply_session_flags(&mut cmd, "test");
            assert_eq!(
                cmd, tool,
                "{tool}: sandboxed launch must not emit resume flags"
            );
            // The sid stays pinned in agent_session_id; only its emission
            // into the container command is suppressed, so the method reports
            // "no resume flags applied" (is_existing && emitted == false).
            assert!(!resumed, "{tool}");
            assert_eq!(
                inst.agent_session_id.as_deref(),
                Some(sid),
                "{tool}: suppression must not clear the stored sid"
            );
        }
        // Host control: without a sandbox the same pinned sid IS emitted.
        let mut host_inst = Instance::new("test", "/tmp/test");
        host_inst.tool = "prime-agent".to_string();
        host_inst.agent_session_id = Some(sid.to_string());
        host_inst.resume_intent = ResumeIntent::Use(sid.to_string());
        let mut cmd = "prime-agent".to_string();
        assert!(host_inst.apply_session_flags(&mut cmd, "test"));
        assert_eq!(cmd, format!("prime-agent --resume {sid}"));
    }

    #[test]
    #[serial_test::serial]
    fn sandboxed_prime_agent_capture_and_poller_stay_host_only() {
        // Both host-only dispatch points must decline before doing any work:
        // retroactive capture would otherwise read the HOST sessions dir for
        // a container session, and the poller would adopt a host peer's sid.
        // A matching host session is seeded so the capture assertion cannot
        // pass vacuously: only the sandbox gate keeps it None.
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("seed.jsonl"),
            "{\"type\":\"session\",\"version\":3,\
              \"id\":\"11111111-2222-3333-4444-555555555555\",\
              \"timestamp\":\"2026-08-23T00:00:00.000Z\",\
              \"cwd\":\"/tmp/test\",\"rlmDepth\":0}\n",
        )
        .unwrap();
        let _env = EnvGuard::set(&[("PRIME_AGENT_CODING_AGENT_DIR", tmp.path())]);
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));

        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "prime-agent".to_string();
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        assert_eq!(inst.try_retroactive_capture(), None);
        inst.maybe_start_poller_since(None);
        assert!(inst.session_id_poller.is_none());

        // Host control: the same store yields the matching sid once the
        // session is not sandboxed, proving the seed was loadable at all.
        inst.sandbox_info = None;
        assert_eq!(
            inst.try_retroactive_capture().as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
    }

    #[test]
    fn clearing_the_conversation_drops_its_transcript_path() {
        let mut inst = Instance::new("pi-clear", "/tmp/pi-clear");
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa".to_string());
        inst.pi_session_path = Some(
            "/store/2026-01-01T00-00-00-000Z_aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa.jsonl"
                .to_string(),
        );
        inst.resume_intent = ResumeIntent::Cleared;

        let (sid, is_existing) = inst.acquire_session_id_with(&|_| None);

        assert_eq!(sid, None, "no pin without a mint seam");
        assert!(!is_existing);
        assert_eq!(
            inst.pi_session_path, None,
            "the dropped conversation's transcript must not linger"
        );
    }

    // A reload keeps the row and drops the runtime flag, so the file is all
    // that says this pane publishes. Looking in the host hook dir for a
    // container's sidecar reads a live publisher as a silent one: no poller
    // repair, and a flush that returns before reading anything.
    // An unresolvable sandbox path must not read as "use the host one": that
    // is a conversation from another namespace, which is the attribution bug
    // this change removes.
    #[test]
    #[serial_test::serial]
    fn an_unresolvable_sandbox_source_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::EnvGuard::set(&[("HOME", temp.path())]);

        // An id the dir guard refuses is one way the path cannot resolve.
        let mut inst = Instance::new("pi-unresolvable", "/tmp/pi-unresolvable");
        inst.id = "../escape".to_string();
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-pi-unresolvable".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });

        assert_eq!(
            inst.pi_sidecar_source(),
            None,
            "no source is the safe answer"
        );
        assert!(
            !inst.uses_pi_session_sidecar(),
            "a pane with no resolvable source does not publish"
        );
        assert!(
            !inst.supports_session_poller(),
            "and must not poll, which would read the host sidecar"
        );
        assert_eq!(inst.pi_published_session_id(true), None);
        assert_eq!(inst.pi_published_session_path(), None);

        // The host pane it must not be confused with does have a source.
        let mut host = Instance::new("pi-host-src", "/tmp/pi-unresolvable");
        host.tool = "pi".to_string();
        assert_eq!(host.pi_sidecar_source(), Some(PiSidecarSource::HostHooks));
    }

    #[test]
    #[serial_test::serial]
    fn reloaded_sandbox_session_still_finds_its_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::EnvGuard::set(&[("HOME", temp.path())]);

        let mut inst = Instance::new("pireloadsandbox01", "/tmp/pi-reload");
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-pi-reload".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });
        inst.mark_pi_extension_launched_for_test();

        // Round-trip the way a daemon or TUI reload does.
        let reloaded: Instance =
            serde_json::from_str(&serde_json::to_string(&inst).unwrap()).unwrap();
        assert!(
            !reloaded.uses_pi_session_sidecar(),
            "nothing published yet, so nothing to find"
        );

        let dir = reloaded
            .pi_sidecar_source()
            .and_then(|s| match s {
                crate::session::instance::PiSidecarSource::SandboxDir(d) => Some(d),
                _ => None,
            })
            .expect("a sandboxed pane has a bind-backed sidecar");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session_id"),
            "01a053b6-c470-78de-9d8f-bc00ef05332a\n",
        )
        .unwrap();

        assert!(
            reloaded.uses_pi_session_sidecar(),
            "the published file is what a reloaded session has to go on"
        );
        assert!(
            reloaded.supports_session_poller(),
            "poller repair must stay available after a reload"
        );
        assert_eq!(
            reloaded.pi_published_session_id(true).as_deref(),
            Some("01a053b6-c470-78de-9d8f-bc00ef05332a"),
            "and the final flush must read it"
        );
    }

    /// A Pi session pointed at a config dir the user named gets no sidecar,
    /// neither writing one nor reading one.
    ///
    /// Both halves of the path (the discovered extension, the published file)
    /// live in the bind AoE mounts itself; the user's own dir reaches the
    /// container through their `extra_volumes` entry, at a path AoE cannot
    /// see. The read gate is the half that matters after the fact: the bind
    /// stays mounted and writable from the container, so a sidecar left there
    /// by an earlier launch (or written by the pane itself) would otherwise
    /// resume this session onto a conversation it never published.
    #[test]
    #[serial_test::serial]
    fn sandboxed_pi_with_its_own_config_dir_declines_the_sidecar() {
        const STALE_ID: &str = "01a053b6-c470-78de-9d8f-bc00ef05332a";
        let _guard = crate::session::test_support::isolate_app_dir();
        let app_dir = crate::session::get_app_dir().unwrap();

        let sandboxed_pi = |id: &str| {
            let mut inst = Instance::new(id, "/tmp/pi-own-config");
            inst.tool = "pi".to_string();
            inst.sandbox_info = Some(crate::session::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "aoe-pi-own-config".to_string(),
                extra_env: None,
                custom_instruction: None,
                container_workdir: None,
                before_start_env: Vec::new(),
            });
            inst
        };

        let inst = sandboxed_pi("piownconfig01");
        assert!(
            inst.pi_extension_launch().is_some(),
            "a session on the staged config dir publishes as before"
        );
        // What that session leaves behind, and what the container can write.
        let sidecar = inst.pi_sandbox_sidecar().expect("a staged sidecar dir");
        std::fs::create_dir_all(&sidecar).unwrap();
        std::fs::write(sidecar.join("session_id"), format!("{STALE_ID}\n")).unwrap();
        std::fs::write(
            sidecar.join("session_path"),
            "/root/.pi/sessions/--p--/2026-01-01T00-00-00-000Z_x.jsonl\n",
        )
        .unwrap();
        assert!(
            inst.uses_pi_session_sidecar(),
            "the staged sidecar is this session's until it declares otherwise"
        );

        std::fs::write(
            app_dir.join("config.toml"),
            "[session.agent_config_dir]\npi = \"~/.pi-personal\"\n",
        )
        .unwrap();

        // A fresh object, the way a reload builds one: the declared dir is
        // resolved once per instance rather than on every refresh.
        let mut declared = sandboxed_pi("piownconfig01");
        assert_eq!(
            declared.pi_extension_launch(),
            None,
            "a declared config dir leaves the session on store polling"
        );
        assert_eq!(declared.pi_sidecar_source(), None);
        assert!(!declared.uses_pi_session_sidecar());
        assert_eq!(declared.pi_published_session_id(true), None);
        assert_eq!(declared.pi_published_session_path(), None);

        let mut cmd = String::from("pi");
        declared.apply_session_flags(&mut cmd, "test");
        assert!(
            !cmd.contains(STALE_ID) && !cmd.contains("--session"),
            "the stale sidecar must not reach the launch line, got {cmd:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_transcript_paths_validate_in_the_host_namespace() {
        // The container publishes `/root/.pi/...`; the file lives under the
        // sandbox dir on this side. Checking the container path verbatim would
        // reject every sandbox transcript.
        let mut inst = Instance::new("pi-ns", "/tmp/pi-ns");
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "aoe-pi-ns".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });

        let published = "/root/.pi/sessions/--proj--/2026-01-01T00-00-00-000Z_x.jsonl";
        let host = inst
            .pi_host_view_of(published)
            .expect("a container path maps to the sandbox dir");
        assert!(
            host.starts_with(crate::session::container_config::pi_sandbox_dir().unwrap()),
            "resolved under the sandbox dir, got {host:?}"
        );
        assert!(host.ends_with("sessions/--proj--/2026-01-01T00-00-00-000Z_x.jsonl"));
        assert_eq!(
            inst.pi_host_view_of("/elsewhere/x.jsonl"),
            None,
            "a path outside the bind cannot be mapped"
        );

        // A host pane's path is already a host path.
        let mut host_inst = Instance::new("pi-host-ns", "/tmp/pi-ns");
        host_inst.tool = "pi".to_string();
        assert_eq!(
            host_inst.pi_host_view_of("/home/u/.pi/x.jsonl"),
            Some(std::path::PathBuf::from("/home/u/.pi/x.jsonl"))
        );
    }

    #[test]
    fn pi_resumes_by_published_path_only_for_its_own_transcript() {
        // The path is what survives a worktree move, but a path left over from
        // a previous conversation must not resume it, so the file name has to
        // carry the id the row holds.
        let temp = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
        let other = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
        let mine = temp
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::write(&mine, "{}\n").unwrap();
        let theirs = temp
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{other}.jsonl"));
        std::fs::write(&theirs, "{}\n").unwrap();

        let mut inst = Instance::new("pi-path", "/tmp/pi-path");
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some(id.to_string());

        assert_eq!(
            inst.pi_resumable_transcript(),
            None,
            "no path published yet"
        );

        inst.pi_session_path = Some(mine.to_string_lossy().to_string());
        assert_eq!(
            inst.pi_resumable_transcript().as_deref(),
            Some(mine.to_string_lossy().as_ref()),
            "the pane's own transcript resumes by path"
        );

        inst.pi_session_path = Some(theirs.to_string_lossy().to_string());
        assert_eq!(
            inst.pi_resumable_transcript(),
            None,
            "a path for another conversation must not be resumed"
        );

        // A partial pin, which `set-session-id` accepts, must not match by
        // substring: the id segment has to be the whole uuid.
        inst.agent_session_id = Some("aaaaaaaa".to_string());
        inst.pi_session_path = Some(mine.to_string_lossy().to_string());
        assert_eq!(inst.pi_resumable_transcript(), None, "partial pin");
        inst.agent_session_id = Some(id.to_string());

        inst.pi_session_path = Some(
            temp.path()
                .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl.gone"))
                .to_string_lossy()
                .to_string(),
        );
        assert_eq!(inst.pi_resumable_transcript(), None, "the file must exist");
    }

    #[test]
    fn pi_relaunch_of_an_unwritten_pin_uses_the_creating_flag() {
        // pi writes its session file on the first message, so a pane that was
        // pinned and never prompted has an id the store has never recorded.
        // `--session` exits 1 on such an id; the pinning arm recreates it.
        let mut inst = Instance::new("pi-pinned", "/tmp/pi-pinned");
        inst.tool = "pi".to_string();

        let minted = Some("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa");
        // (label, pinnable, id, user-pinned) -> takes the `existing` arm.
        // `--session-id` creates when the id is absent and searches this
        // project only, so it is for ids AoE minted or captured; anything the
        // user handed us keeps `--session`, which resolves partials and
        // searches wider.
        for (label, pinnable, sid, explicit, expected) in [
            ("minted, pinnable", true, minted, false, false),
            ("minted, old binary", false, minted, true, true),
            ("user-pinned partial", true, Some("aaaaaaaa"), true, true),
            ("user-pinned full uuid", true, minted, true, true),
            ("no id", false, None, false, false),
        ] {
            assert_eq!(
                inst.resume_flag_arm_is_existing(sid.is_some(), pinnable, sid, explicit),
                expected,
                "{label}"
            );
        }

        // Every other agent tracks is_existing whatever the pi probe says,
        // and never reaches the probe: `pi_session_id_pinnable` is gated on
        // the tool so no other launch spawns `pi --help`.
        let mut claude = Instance::new("claude-pinned", "/tmp/pi-pinned");
        claude.tool = "claude".to_string();
        assert!(claude.resume_flag_arm_is_existing(true, true, minted, false));
        assert!(!claude.pi_session_id_pinnable());
    }

    #[test]
    fn test_acquire_session_id_idempotence() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();

        let (first, first_existing) = inst.acquire_session_id();
        let (second, second_existing) = inst.acquire_session_id();

        // Repeated acquire yields a STABLE id. The first mint reports fresh; a
        // second acquire with no transcript on disk stays fresh-pinned (an empty
        // thread's sid is not resumable) but returns the same id, so a later
        // relaunch keeps `--session-id <same>` rather than a doomed `--resume`.
        assert!(first.is_some());
        assert!(!first_existing);
        assert!(!second_existing);
        assert_eq!(first, second);
    }

    #[test]
    fn opencode_fresh_arm_uses_preassign_seam() {
        // opencode's fresh launch adopts the id the preassign seam returns and
        // stores it, exactly like Claude's pre-minted UUID (fresh, not resumed).
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        let (sid, is_existing) =
            inst.acquire_session_id_with(&|_| Some("ses_preassigned".to_string()));
        assert_eq!(sid, Some("ses_preassigned".to_string()));
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, Some("ses_preassigned".to_string()));
    }

    #[test]
    fn opencode_fresh_arm_falls_back_when_preassign_returns_none() {
        // A disabled setting or a failed preassign yields None, leaving the id
        // unpinned so the background SQLite poller captures it post-launch.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        let (sid, is_existing) = inst.acquire_session_id_with(&|_| None);
        assert_eq!(sid, None);
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, None);
    }

    #[test]
    fn non_opencode_fresh_arm_never_calls_preassign_seam() {
        // The seam is opencode-only: Claude mints its own UUID and every other
        // agent starts unpinned, so the seam must not run for them.
        let mut claude = Instance::new("Test", "/tmp/test");
        claude.tool = "claude".to_string();
        let (claude_sid, _) =
            claude.acquire_session_id_with(&|_| panic!("preassign seam ran for claude"));
        assert!(claude_sid.is_some());

        let mut codex = Instance::new("Test", "/tmp/test");
        codex.tool = "codex".to_string();
        let (codex_sid, _) =
            codex.acquire_session_id_with(&|_| panic!("preassign seam ran for codex"));
        assert_eq!(codex_sid, None);
    }

    #[test]
    fn opencode_cleared_intent_also_uses_preassign_seam() {
        // A forced-fresh restart (ResumeIntent::Cleared) is still a new launch,
        // so it preassigns too rather than starting unpinned.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        inst.resume_intent = ResumeIntent::Cleared;
        let (sid, is_existing) = inst.acquire_session_id_with(&|_| Some("ses_cleared".to_string()));
        assert_eq!(sid, Some("ses_cleared".to_string()));
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, Some("ses_cleared".to_string()));
    }

    #[test]
    fn opencode_preassign_skips_when_launch_not_mirrorable() {
        // Plain ambient opencode (no command override, no profile host env):
        // the ephemeral serve provably matches the launch, so preassign is
        // allowed to run.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        assert!(inst.opencode_launch_mirrorable_by_ambient_serve());

        // A command override points the launch at a different binary/store,
        // which the ambient `opencode serve` cannot mirror, so preassign is
        // skipped (falls back to the poller) rather than risking a launch that
        // fails "Session not found".
        inst.command = "opencode-wrapper".to_string();
        assert!(!inst.opencode_launch_mirrorable_by_ambient_serve());
    }

    /// A subcommand-shaped resume has to land immediately after the binary,
    /// and the splice finds that spot by taking the first word. A wrapper
    /// command puts something else there, so the flags are dropped rather
    /// than spliced onto the wrong program (#3638).
    #[test]
    fn codex_wrapper_command_never_takes_a_spliced_subcommand() {
        const PROFILE: &str = "codex-wrapper-splice-test";
        let _registry = install_aliases(PROFILE, &[("codex-remote", "codex")]);
        let sid = "11111111-2222-3333-4444-555555555555";

        // The documented custom-agent shape: a multi-token launcher.
        let mut wrapped = Instance::new("wrapper", "/tmp/codex-splice");
        wrapped.source_profile = PROFILE.to_string();
        wrapped.tool = "codex-remote".to_string();
        wrapped.command = "ssh -t lenovo codex".to_string();
        wrapped.agent_session_id = Some(sid.to_string());
        wrapped.resume_intent = ResumeIntent::Use(sid.to_string());
        let mut cmd = wrapped.command.clone();
        assert!(!wrapped.apply_session_flags(&mut cmd, "test"));
        assert_eq!(
            cmd, "ssh -t lenovo codex",
            "the resume token must not be spliced onto the launcher"
        );

        // An override that does open with the binary keeps its resume.
        let mut direct = Instance::new("direct", "/tmp/codex-splice");
        direct.source_profile = PROFILE.to_string();
        direct.tool = "codex-remote".to_string();
        direct.command = "codex --model o3".to_string();
        direct.agent_session_id = Some(sid.to_string());
        direct.resume_intent = ResumeIntent::Use(sid.to_string());
        let mut direct_cmd = direct.command.clone();
        assert!(direct.apply_session_flags(&mut direct_cmd, "test"));
        assert_eq!(direct_cmd, format!("codex resume {sid} --model o3"));

        // A bare wrapper binary is the program itself, so the splice reaches
        // the agent it execs. This is what `agent_command_override` documents
        // and what a `custom_agents` script is, and dropping it would take
        // resume away from a plain `codex` session that only renamed its
        // binary.
        for command in ["mycodex", "/opt/bin/mycodex", "codex-personal"] {
            let mut bare = Instance::new("bare", "/tmp/codex-splice");
            bare.source_profile = PROFILE.to_string();
            bare.tool = "codex-remote".to_string();
            bare.command = command.to_string();
            bare.agent_session_id = Some(sid.to_string());
            bare.resume_intent = ResumeIntent::Use(sid.to_string());
            let mut bare_cmd = bare.command.clone();
            assert!(bare.apply_session_flags(&mut bare_cmd, "test"), "{command}");
            assert_eq!(bare_cmd, format!("{command} resume {sid}"));
        }
    }

    /// A custom agent that wraps a supported one has no `AgentDef` of its
    /// own, so every capture and resume path used to miss on `tool` raw: no
    /// id was pinned at launch and no resume flag was emitted, and each
    /// restart silently started a fresh conversation (#3638).
    #[test]
    fn custom_agent_pins_and_resumes_through_its_detect_as_base() {
        const PROFILE: &str = "custom-agent-resume-test";
        let _registry = install_aliases(
            PROFILE,
            &[("claude-personal", "claude"), ("cursor-personal", "cursor")],
        );

        let mut inst = Instance::new("wrapper", "/tmp/custom-agent-resume");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "claude-personal".to_string();
        inst.command = "claude-personal".to_string();

        let mut fresh = "claude-personal".to_string();
        assert!(!inst.apply_session_flags(&mut fresh, "test"));
        let sid = inst
            .agent_session_id
            .clone()
            .expect("a wrapper launch must pin the conversation it is about to start");
        assert_eq!(fresh, format!("claude-personal --session-id {sid}"));

        let mut restart = "claude-personal".to_string();
        assert!(inst.apply_session_flags(&mut restart, "test"));
        assert_eq!(restart, format!("claude-personal --resume {sid}"));
        assert_eq!(inst.agent_session_id.as_deref(), Some(sid.as_str()));

        // A wrapper whose base cannot resume stays silent, pinned id and all.
        let mut unsupported = Instance::new("wrapper", "/tmp/custom-agent-resume");
        unsupported.source_profile = PROFILE.to_string();
        unsupported.tool = "cursor-personal".to_string();
        unsupported.command = "cursor-personal".to_string();
        unsupported.resume_intent = ResumeIntent::Use(sid.clone());
        let mut cursor = "cursor-personal".to_string();
        assert!(!unsupported.apply_session_flags(&mut cursor, "test"));
        assert_eq!(cursor, "cursor-personal");
    }

    #[test]
    fn apply_session_flags_returns_acquire_is_existing() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();
        // Fresh mint (no prior transcript): acquire reports a new session
        // (`--session-id`), so apply_session_flags returns false.
        let mut cmd = String::from("claude");
        assert!(!inst.apply_session_flags(&mut cmd, "test"));
        // A user-pinned resume intent reports an existing session
        // unconditionally, so apply_session_flags returns true.
        inst.resume_intent = ResumeIntent::Use("019342ab-1234-7def-8901-abcdef012345".to_string());
        let mut cmd2 = String::from("claude");
        assert!(inst.apply_session_flags(&mut cmd2, "test"));
    }

    struct TmuxSessionGuard(String);

    impl TmuxSessionGuard {
        fn create(inst: &Instance) -> Option<Self> {
            let tmux_available = crate::tmux::tmux_command()
                .arg("-V")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !tmux_available {
                eprintln!("Skipping: tmux not available");
                return None;
            }

            let session = inst.tmux_session().unwrap();
            session
                .create(&inst.project_path, Some("sleep 60"), "default")
                .expect("create tmux session");
            Some(Self(session.name().to_string()))
        }
    }

    impl Drop for TmuxSessionGuard {
        fn drop(&mut self) {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &self.0])
                .output();
            crate::tmux::refresh_session_cache();
        }
    }

    fn seed_opencode_db(db_path: &std::path::Path, sid: &str, project_path: &str) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT NOT NULL,
                time_updated INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, directory, time_updated) VALUES (?1, ?2, ?3)",
            rusqlite::params![sid, project_path, 1_000_000_i64],
        )
        .unwrap();
    }

    #[test]
    #[serial]
    fn resume_intent_use_returns_pinned_sid_without_observation() {
        let mut inst = Instance::new("intent-use", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Use("user-pinned".to_string());

        let (sid, is_existing) = inst.acquire_session_id();
        assert_eq!(sid.as_deref(), Some("user-pinned"));
        assert!(is_existing);
        assert_eq!(inst.agent_session_id.as_deref(), Some("user-pinned"));
    }

    #[test]
    #[serial]
    fn resume_intent_use_overrides_observation() {
        let mut inst = Instance::new("intent-use-override", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("observed".to_string());
        inst.resume_intent = ResumeIntent::Use("user-pinned".to_string());

        let (sid, is_existing) = inst.acquire_session_id();
        assert_eq!(sid.as_deref(), Some("user-pinned"));
        assert!(is_existing);
    }

    #[test]
    #[serial]
    fn resume_intent_cleared_for_claude_generates_fresh_uuid() {
        let mut inst = Instance::new("intent-cleared-claude", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("observed".to_string());
        inst.resume_intent = ResumeIntent::Cleared;

        let (sid, is_existing) = inst.acquire_session_id();
        assert!(
            sid.is_some(),
            "Claude must always have a session id at launch"
        );
        assert!(!is_existing, "Cleared intent must not report is_existing");
        assert_ne!(sid.as_deref(), Some("observed"));
        assert_eq!(inst.agent_session_id, sid);
    }

    #[test]
    #[serial]
    fn resume_intent_cleared_for_opencode_returns_none() {
        let mut inst = Instance::new("intent-cleared-opencode", "/tmp/x");
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some("observed".to_string());
        inst.resume_intent = ResumeIntent::Cleared;

        let (sid, is_existing) = inst.acquire_session_id();
        assert_eq!(sid, None);
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, None);
    }

    #[test]
    #[serial]
    fn resume_intent_default_uses_observed() {
        // Isolate HOME and CLAUDE_CONFIG_DIR at an empty tempdir so
        // `acquire_session_id`'s freshest-observation probe reads scratch
        // state, never the caller's real `~/.claude`. Without this the
        // probe scans `~/.claude/projects/-tmp-x`, and any live transcript
        // there (present in a Claude dev environment) supersedes the stored
        // sid, so the assertion below fails deterministically. Mirrors the
        // `verify_on_resume` submodule's `claude_home_guard`.
        let temp = tempdir().unwrap();
        let mut pairs: Vec<(&'static str, std::path::PathBuf)> =
            vec![("HOME", temp.path().to_path_buf())];
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
        pairs.push(("CLAUDE_CONFIG_DIR", temp.path().join(".claude")));
        let _home = EnvGuard::set(&pairs);

        let mut inst = Instance::new("intent-default", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("observed".to_string());
        inst.resume_intent = ResumeIntent::Default;

        // Default intent keeps the observed sid as the owned session. With
        // the isolated home holding no transcript for it, the empty thread
        // launches fresh-pinned (`is_existing = false`, `--session-id`)
        // rather than a certain-to-fail `--resume`.
        let (sid, is_existing) = inst.acquire_session_id();
        assert_eq!(sid.as_deref(), Some("observed"));
        assert!(!is_existing);
    }

    #[test]
    fn acquire_default_with_no_observation_generates_uuid_for_claude() {
        let mut inst = Instance::new("acquire-default-fresh", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Default;

        let (sid, is_existing) = inst.acquire_session_id();
        assert!(sid.is_some());
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, sid);
    }

    #[test]
    #[serial]
    fn acquire_session_id_default_picks_up_retroactive_capture() {
        let temp = tempdir().unwrap();
        let project_path = temp.path().join("opencode-project");
        std::fs::create_dir_all(&project_path).unwrap();
        let project_path = project_path.to_string_lossy().to_string();
        let db_path = temp.path().join("opencode.db");
        let captured_sid = "ses_retroactive_capture";
        seed_opencode_db(&db_path, captured_sid, &project_path);
        let _opencode_db = EnvGuard::set(&[("OPENCODE_DB", &db_path)]);

        let mut inst = Instance::new("retroactive-opencode", &project_path);
        inst.tool = "opencode".to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Default;
        let Some(_tmux) = TmuxSessionGuard::create(&inst) else {
            return;
        };

        let (sid, is_existing) = inst.acquire_session_id();

        assert_eq!(sid.as_deref(), Some(captured_sid));
        assert!(is_existing);
        assert_eq!(inst.agent_session_id.as_deref(), Some(captured_sid));
    }

    mod verify_on_resume {
        use super::*;
        use crate::session::capture::encode_claude_project_path;
        use crate::session::test_support::isolate_app_dir_at;
        use std::fs;
        use std::path::PathBuf;
        use std::time::{Duration, SystemTime};
        use tempfile::{tempdir, TempDir};

        /// Points `HOME`, `CLAUDE_CONFIG_DIR` (and, on Linux/macOS,
        /// `XDG_CONFIG_HOME`) at `temp` for the current test body.
        /// See [`crate::session::test_support`]: the snapshot/restore
        /// is `EnvGuard`'s, so a non-UTF-8 prior value round-trips
        /// instead of being dropped (#2751).
        fn claude_home_guard(temp: &TempDir) -> EnvGuard {
            let mut pairs: Vec<(&'static str, PathBuf)> = vec![("HOME", temp.path().to_path_buf())];
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
            pairs.push(("CLAUDE_CONFIG_DIR", temp.path().join(".claude")));
            EnvGuard::set(&pairs)
        }

        fn write_jsonl_with_mtime(path: &std::path::Path, mtime: SystemTime) {
            fs::write(path, "").unwrap();
            let f = fs::File::options().write(true).open(path).unwrap();
            f.set_times(fs::FileTimes::new().set_modified(mtime))
                .unwrap();
        }

        #[test]
        #[serial]
        fn supersedes_stale_claude_sid_after_clear() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2291-claude-bascule";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let stale = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
            let fresh = "11111111-2222-3333-4444-555555555555";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{stale}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{fresh}.jsonl")),
                now - Duration::from_secs(10),
            );

            let mut inst = Instance::new("verify-claude-bascule", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stale.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(fresh));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        }

        #[test]
        #[serial]
        fn no_bascule_when_claude_stored_matches_freshest() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2291-claude-steady";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let live = "ffffffff-eeee-dddd-cccc-bbbbbbbbbbbb";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{live}.jsonl")),
                now - Duration::from_secs(10),
            );

            let mut inst = Instance::new("verify-claude-steady", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(live.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(live));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(live));
        }

        /// The empty-thread downgrade must cover an id that arrived as a
        /// live observation, not just one loaded from storage: SessionStart
        /// fires before Claude writes any content, so the sidecar can name
        /// a thread with no transcript, and `--resume` on it is a dead pane
        /// on every restart.
        #[test]
        #[serial]
        fn observed_sid_without_transcript_downgrades_to_fresh() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-observed-no-transcript";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let stored = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
            let empty_thread = "11111111-2222-3333-4444-555555555555";
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{stored}.jsonl")),
                SystemTime::now() - Duration::from_secs(120),
            );
            // No .jsonl for `empty_thread`.

            let mut inst = Instance::new("verify-observed-no-transcript", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let dir = super::write_sidecar(&inst.id, empty_thread);
            let (sid, is_existing) = inst.acquire_session_id();
            std::fs::remove_dir_all(&dir).ok();

            assert_eq!(sid.as_deref(), Some(empty_thread));
            assert!(
                !is_existing,
                "an observed sid with no transcript must launch as \
                 --session-id, never --resume"
            );
        }

        // An empty Claude thread killed before its first prompt has a
        // stored sid but no transcript on disk. `claude --resume <sid>`
        // would fail for it every time (the "resume failed for sid ...;
        // preserved for explicit retry" loop), so acquire must launch it as
        // a fresh pinned session (`--session-id <sid>`, is_existing=false)
        // while keeping the id stable for a later first prompt.
        #[test]
        #[serial]
        fn stored_sid_without_transcript_launches_fresh_pinned() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2291-no-jsonl";
            let stored = "12121212-3434-5656-7878-9a9a9a9a9a9a";

            let mut inst = Instance::new("verify-claude-no-jsonl", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(stored));
            assert!(
                !is_existing,
                "a stored sid with no transcript must launch fresh-pinned, not --resume"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(stored));
        }

        // Regression guard for the existence-only transcript check: an idle
        // but real conversation whose jsonl is older than the 5-minute
        // live-capture window must still resume. The mtime scan returns
        // nothing (stale), so acquire falls through to the transcript check,
        // which is age-agnostic and confirms the sid is resumable.
        #[test]
        #[serial]
        fn stored_sid_with_stale_transcript_still_resumes() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-stale-transcript";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let stored = "12121212-3434-5656-7878-9a9a9a9a9a9a";
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{stored}.jsonl")),
                SystemTime::now() - Duration::from_secs(3600),
            );

            let mut inst = Instance::new("verify-claude-stale", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(stored));
            assert!(
                is_existing,
                "a real (if idle) transcript on disk must resume with --resume"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(stored));
        }

        /// #3399: two sessions share a `project_path` but sit in profiles
        /// pinned to different `CLAUDE_CONFIG_DIR`s. Each must resume its
        /// own conversation. Resolving the default `~/.claude` instead
        /// reports both transcripts absent and downgrades every launch to
        /// `--session-id <sid>`, which the agent rejects as already in use.
        #[test]
        #[serial]
        fn same_cwd_sessions_resume_their_own_profile_scoped_conversation() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-3399-shared-cwd";
            let cases = [
                ("aoe-3399-personal", "11111111-1111-4111-8111-111111111111"),
                ("aoe-3399-work", "22222222-2222-4222-8222-222222222222"),
            ];
            for (profile, sid) in cases {
                let claude_home = temp.path().join(format!(".claude-{profile}"));
                let dir = claude_home
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&dir).unwrap();
                // Older than the live-capture window, so the mtime scan
                // stays out of it and the transcript gate is what decides.
                write_jsonl_with_mtime(
                    &dir.join(format!("{sid}.jsonl")),
                    SystemTime::now() - Duration::from_secs(3600),
                );

                let config_path = crate::session::get_profile_dir_path(profile)
                    .unwrap()
                    .join("config.toml");
                fs::create_dir_all(config_path.parent().unwrap()).unwrap();
                fs::write(
                    &config_path,
                    format!(
                        "environment = [\"CLAUDE_CONFIG_DIR={}\"]\n",
                        claude_home.display()
                    ),
                )
                .unwrap();
            }

            for (profile, sid) in cases {
                let mut inst = Instance::new(profile, project_path);
                inst.source_profile = profile.to_string();
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(sid.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (acquired, is_existing) = inst.acquire_session_id();
                assert_eq!(acquired.as_deref(), Some(sid));
                assert!(
                    is_existing,
                    "{profile}: transcript under the profile's own CLAUDE_CONFIG_DIR \
                     must resume with --resume, not launch fresh-pinned"
                );
            }

            // A `before_session` hook minting CLAUDE_CONFIG_DIR is the
            // documented account-switcher pattern, and its value wins over
            // the profile's on the launched pane. Reading the shadowed
            // profile value here would resolve a config dir the agent
            // never opens, reintroducing the same downgrade.
            let (shadowed_profile, other_sid) = (cases[0].0, cases[1].1);
            let mut inst = Instance::new("minted-switcher", project_path);
            inst.source_profile = shadowed_profile.to_string();
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(other_sid.to_string());
            inst.resume_intent = ResumeIntent::Default;
            inst.pending_host_env = vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                temp.path()
                    .join(format!(".claude-{}", cases[1].0))
                    .to_string_lossy()
                    .into_owned(),
            )];

            let (acquired, is_existing) = inst.acquire_session_id();
            assert_eq!(acquired.as_deref(), Some(other_sid));
            assert!(
                is_existing,
                "a before_session-minted CLAUDE_CONFIG_DIR must win over the \
                 profile's, matching what the launch injects into the pane"
            );
        }

        #[test]
        #[serial]
        fn unaffected_for_unsupported_tool() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let mut inst = Instance::new("verify-cursor", "/tmp/aoe-test-2291-cursor");
            inst.tool = "cursor".to_string();
            inst.agent_session_id = Some("stored-cursor-sid".to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some("stored-cursor-sid"));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some("stored-cursor-sid"));
        }

        // #2344: when several AoE Claude sessions share one cwd, the
        // most-recent jsonl in the shared `~/.claude/projects/<encoded-cwd>/`
        // dir is often a *peer* session's conversation. The mtime scan would
        // pick it and clobber this instance's stored sid on resume. The
        // per-instance hook sidecar is authoritative and must win over the
        // mtime guess: here the sidecar names the instance's own conversation
        // while a peer's jsonl is strictly fresher on disk.
        #[test]
        #[serial]
        fn sidecar_wins_over_fresher_peer_jsonl() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2344-shared-cwd";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            // `mine` is this instance's real conversation (named by its
            // sidecar). `peer` is a co-located peer's conversation that is
            // strictly freshest on disk. `stored` is a stale id distinct
            // from `mine`, so asserting `sid == mine` proves the sidecar
            // actively overrode the stored value rather than the stored
            // value passing through unchanged.
            let mine = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
            let peer = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
            let stored = "cccccccc-3333-4333-8333-cccccccccccc";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{mine}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{peer}.jsonl")),
                now - Duration::from_secs(5),
            );

            let mut inst = Instance::new("verify-2344-shared-cwd", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let dir = super::write_sidecar(&inst.id, mine);
            let (sid, is_existing) = inst.acquire_session_id();
            std::fs::remove_dir_all(&dir).ok();

            // The authoritative sidecar overrides the stale stored sid;
            // the peer's fresher jsonl never wins.
            assert_eq!(sid.as_deref(), Some(mine));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        // #2344 follow-up: a sandboxed Claude session must also consult the
        // sidecar. Its SessionStart hook writes through the
        // `/tmp/aoe-hooks/<id>` bind-mount onto the host path, so
        // `read_hook_session_id` reads it the same way a host session's is
        // read. Without the sidecar short-circuit the sandbox-aware mtime
        // branch would pick a peer's fresher jsonl in the shared cwd.
        #[test]
        #[serial]
        fn sidecar_consulted_for_sandboxed_claude() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2344-sandbox";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            // `stored` is distinct from the sidecar `mine`, so the assertion
            // proves the sidecar actively overrode the stale stored value.
            let mine = "eeeeeeee-5555-4555-8555-eeeeeeeeeeee";
            let peer = "ffffffff-6666-4666-8666-ffffffffffff";
            let stored = "dddddddd-7777-4777-8777-dddddddddddd";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{mine}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{peer}.jsonl")),
                now - Duration::from_secs(5),
            );

            let mut inst = Instance::new("verify-2344-sandbox", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;
            inst.sandbox_info = Some(crate::session::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test-image".to_string(),
                container_name: "verify-2344-sandbox".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            assert!(inst.is_sandboxed());

            let dir = super::write_sidecar(&inst.id, mine);
            let (sid, is_existing) = inst.acquire_session_id();
            std::fs::remove_dir_all(&dir).ok();

            // Sidecar (host-readable) names this instance's conversation, so
            // the peer's fresher jsonl does not win even though sandbox would
            // otherwise route through the container-aware mtime branch.
            assert_eq!(sid.as_deref(), Some(mine));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        // Companion to the above: without a sidecar (e.g. a session resumed
        // after the 5-minute sidecar window) the mtime fallback still
        // applies, preserving the #2291 daemon-mode fix.
        #[test]
        #[serial]
        fn mtime_fallback_applies_without_sidecar() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2344-no-sidecar";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let stale = "cccccccc-3333-4333-8333-cccccccccccc";
            let fresh = "dddddddd-4444-4444-8444-dddddddddddd";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{stale}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{fresh}.jsonl")),
                now - Duration::from_secs(5),
            );

            let mut inst = Instance::new("verify-2344-no-sidecar", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stale.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(fresh));
            assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        }

        // #2355: when a co-located stopped peer leaves a fresher jsonl in
        // the shared `~/.claude/projects/<encoded-cwd>/` dir, the mtime
        // fallback must skip the peer's sid. `build_exclusion_set` only
        // sees live tmux peers; `compose_exclusion_with_persisted_peers`
        // adds the stopped peer's sid from `sessions.json` so this
        // instance's own (older) jsonl wins.
        #[test]
        #[serial]
        fn mtime_fallback_skips_stopped_peer_sid() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2355-stopped-peer";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let mine = "11111111-1111-4111-8111-111111111111";
            let peer = "22222222-2222-4222-8222-222222222222";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{mine}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{peer}.jsonl")),
                now - Duration::from_secs(5),
            );

            let profile = "verify-2355-stopped-peer";
            let mut peer_inst = Instance::new("stopped-peer-id", project_path);
            peer_inst.source_profile = profile.to_string();
            peer_inst.tool = "claude".to_string();
            peer_inst.agent_session_id = Some(peer.to_string());
            peer_inst.status = Status::Stopped;
            super::seed_disk_for_sidecar_test(profile, &peer_inst);

            let mut inst = Instance::new("verify-2355", project_path);
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(mine.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(mine));
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        /// A wrapper and the built-in it wraps write into one transcript
        /// dir, so an inactive plain-`claude` peer must be excluded from a
        /// `claude-personal` session's mtime scan. Resolving the poller for
        /// wrappers (#3638) is what makes that scan reachable at all, and a
        /// raw `tool` compare would leave this direction unguarded.
        #[test]
        #[serial]
        fn exclusion_set_spans_a_wrapper_and_the_agent_it_wraps() {
            const PROFILE: &str = "verify-wrapper-peer";
            let _registry = install_aliases(PROFILE, &[("claude-personal", "claude")]);
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-wrapper-peer";
            let peer_sid = "77777777-7777-4777-8777-777777777777";
            let mut peer = Instance::new("stopped-claude-peer", project_path);
            peer.source_profile = PROFILE.to_string();
            peer.tool = "claude".to_string();
            peer.agent_session_id = Some(peer_sid.to_string());
            peer.status = Status::Stopped;
            super::seed_disk_for_sidecar_test(PROFILE, &peer);

            let mut wrapper = Instance::new("wrapper-instance", project_path);
            wrapper.source_profile = PROFILE.to_string();
            wrapper.tool = "claude-personal".to_string();
            wrapper.command = "claude-personal".to_string();

            assert!(
                wrapper
                    .retroactive_capture_exclusion_set()
                    .contains(peer_sid),
                "a stopped claude peer's conversation must be off limits to the wrapper"
            );
        }

        /// `swap_tool` parks a conversation under the raw outgoing tool, so
        /// an exact-key lookup hides a wrapper's parked Claude thread from a
        /// plain `claude` session sharing the cwd, and the MRU scan adopts
        /// what the peer means to resume on a swap back (#3638).
        #[test]
        #[serial]
        fn parked_conversations_are_matched_across_aliases() {
            const PROFILE: &str = "verify-parked-alias";
            let _registry = install_aliases(PROFILE, &[("claude-personal", "claude")]);
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-parked-alias";
            let parked_sid = "88888888-8888-4888-8888-888888888888";

            // A wrapper session that has since swapped engines: its Claude
            // conversation now lives under the `claude-personal` key.
            let mut peer = Instance::new("swapped-wrapper", project_path);
            peer.source_profile = PROFILE.to_string();
            peer.tool = "claude-personal".to_string();
            peer.command = "claude-personal".to_string();
            peer.agent_session_id = Some(parked_sid.to_string());
            peer.status = Status::Running;
            peer.swap_tool("codex");
            assert_eq!(peer.tool, "codex");
            super::seed_disk_for_sidecar_test(PROFILE, &peer);

            let mut plain = Instance::new("plain-claude", project_path);
            plain.source_profile = PROFILE.to_string();
            plain.tool = "claude".to_string();

            assert!(
                plain
                    .retroactive_capture_exclusion_set()
                    .contains(parked_sid),
                "a conversation parked under an alias must be off limits to its base agent"
            );
        }

        // Companion to the above for the engine swap: the peer is not a
        // Claude session any more (it swapped to pi), so it no longer
        // passes the `tool` filter in
        // `compose_exclusion_with_persisted_peers`, and its Claude sid moved
        // out of `agent_session_id` into `prior_tool_session_ids`. Unless
        // parked ids are excluded too, the peer's Claude transcript is in no
        // exclusion set at all and the mtime fallback hands it to this
        // instance, which both steals the conversation the peer intends to
        // resume on a swap back and leaks its context.
        #[test]
        #[serial]
        fn mtime_fallback_skips_peer_sid_parked_by_a_tool_swap() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-parked-peer";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let mine = "55555555-5555-4555-8555-555555555555";
            let parked = "66666666-6666-4666-8666-666666666666";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{mine}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{parked}.jsonl")),
                now - Duration::from_secs(5),
            );

            let profile = "verify-parked-peer";
            let mut peer_inst = Instance::new("swapped-peer-id", project_path);
            peer_inst.source_profile = profile.to_string();
            peer_inst.tool = "claude".to_string();
            peer_inst.agent_session_id = Some(parked.to_string());
            // The peer is mid-life and running: only its Claude
            // conversation is parked, not the row.
            peer_inst.status = Status::Running;
            peer_inst.swap_tool("pi");
            peer_inst.agent_session_id = Some("pi-session-parked".to_string());
            peer_inst.swap_tool("codex");
            assert_eq!(peer_inst.tool, "codex");
            super::seed_disk_for_sidecar_test(profile, &peer_inst);

            let pi_exclusion = crate::session::capture::compose_exclusion_with_persisted_peers(
                "other-pi-instance",
                project_path,
                "pi",
                false,
                profile,
                &std::collections::HashSet::new(),
            );
            assert!(
                pi_exclusion.contains("pi-session-parked"),
                "parked ids must be protected for every resumable tool"
            );

            let mut inst = Instance::new("verify-parked", project_path);
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(mine.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(
                sid.as_deref(),
                Some(mine),
                "the parked peer's fresher transcript must not be adopted"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        // Companion to the above for #2858: the stopped peer's stored
        // `project_path` is an UNNORMALIZED spelling of the same
        // directory (`<parent>/decoy/../wt` vs `<parent>/wt`), as the
        // default `../{repo-name}-worktrees/{branch}` template used to
        // produce. A raw string comparison in
        // `compose_exclusion_with_persisted_peers` drops the peer from the
        // exclusion and re-opens the #2355 steal; the canonicalized
        // comparison must keep it.
        #[test]
        #[serial]
        fn mtime_fallback_skips_stopped_peer_with_unnormalized_path() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let parent = temp.path().join("proj");
            fs::create_dir_all(parent.join("decoy")).unwrap();
            fs::create_dir_all(parent.join("wt")).unwrap();
            let project_path = parent.join("wt").to_string_lossy().to_string();
            let unnormalized = parent
                .join("decoy")
                .join("..")
                .join("wt")
                .to_string_lossy()
                .to_string();

            // `acquire_session_id` canonicalizes before encoding, so the
            // transcript dir must be keyed by the canonical path (on
            // macOS `/tmp` itself resolves to `/private/tmp`).
            let canonical = std::fs::canonicalize(&project_path).unwrap();
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(&canonical.to_string_lossy()));
            fs::create_dir_all(&claude_dir).unwrap();

            let mine = "55555555-5555-4555-8555-555555555555";
            let peer = "66666666-6666-4666-8666-666666666666";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{mine}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{peer}.jsonl")),
                now - Duration::from_secs(5),
            );

            let profile = "verify-2858-unnormalized-peer";
            let mut peer_inst = Instance::new("unnormalized-peer-id", &unnormalized);
            peer_inst.source_profile = profile.to_string();
            peer_inst.tool = "claude".to_string();
            peer_inst.agent_session_id = Some(peer.to_string());
            peer_inst.status = Status::Stopped;
            super::seed_disk_for_sidecar_test(profile, &peer_inst);

            let mut inst = Instance::new("verify-2858", &project_path);
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(mine.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(
                sid.as_deref(),
                Some(mine),
                "peer with an unnormalized spelling of the same dir must still be excluded"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        // Companion to the above: same setup but the peer is archived
        // instead of stopped, exercising the `is_archived()` branch of
        // `compose_exclusion_with_persisted_peers`.
        #[test]
        #[serial]
        fn mtime_fallback_skips_archived_peer_sid() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2355-archived-peer";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let mine = "33333333-3333-4333-8333-333333333333";
            let peer = "44444444-4444-4444-8444-444444444444";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{mine}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{peer}.jsonl")),
                now - Duration::from_secs(5),
            );

            let profile = "verify-2355-archived-peer";
            let mut peer_inst = Instance::new("archived-peer-id", project_path);
            peer_inst.source_profile = profile.to_string();
            peer_inst.tool = "claude".to_string();
            peer_inst.agent_session_id = Some(peer.to_string());
            peer_inst.archive();

            super::seed_disk_for_sidecar_test(profile, &peer_inst);

            let mut inst = Instance::new("verify-2355-archived", project_path);
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(mine.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(mine));
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        // Companion to the above: same setup but the peer carries the
        // default `Status::Idle` and is not archived, exercising the
        // `!inst.has_live_tmux_pane_in()` branch on its own. The peer has
        // never spawned a tmux pane in the test, so it counts as
        // pane-less even though its Status field does not flag it.
        #[test]
        #[serial]
        fn mtime_fallback_skips_pane_less_peer_sid() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2355-paneless-peer";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let mine = "55555555-5555-4555-8555-555555555555";
            let peer = "66666666-6666-4666-8666-666666666666";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{mine}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{peer}.jsonl")),
                now - Duration::from_secs(5),
            );

            let profile = "verify-2355-paneless-peer";
            let mut peer_inst = Instance::new("paneless-peer-id", project_path);
            peer_inst.source_profile = profile.to_string();
            peer_inst.tool = "claude".to_string();
            peer_inst.agent_session_id = Some(peer.to_string());
            assert!(!peer_inst.is_archived());
            assert!(matches!(peer_inst.status, Status::Idle));
            assert!(!peer_inst.has_live_tmux_pane_in(&crate::tmux::LiveSessionSnapshot::new()));

            super::seed_disk_for_sidecar_test(profile, &peer_inst);

            let mut inst = Instance::new("verify-2355-paneless", project_path);
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(mine.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(mine));
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        // ── Per-tool bascule coverage (#2304) ────────────────────────────
        //
        // The Claude bascule above proves `acquire_session_id`'s Default arm
        // supersedes a stale stored sid with a fresher live observation. The
        // other six live-tracked agents inherit that behaviour through the
        // same `try_retroactive_capture` dispatch, but a regression in an
        // individual match arm (an accidental arm deletion or signature
        // drift) would not be caught by the Claude test alone. Each test
        // below seeds two on-disk sessions for one tool (older = stored,
        // newer = fresh) and asserts acquire replaces the stored sid with
        // the fresher one, exercising that tool's dispatch arm end-to-end.
        //
        // Each points `HOME` at a tempdir via `isolate_app_dir_at` so the
        // exclusion-set scan reads an empty storage rather than the
        // developer's real sessions.json. The tempdir is declared before
        // the guard so the guard drops first, restoring the env before the
        // directory `HOME` points at is removed.

        fn write_with_mtime(path: &std::path::Path, content: &str, mtime: SystemTime) {
            fs::write(path, content).unwrap();
            let f = fs::File::options().write(true).open(path).unwrap();
            f.set_times(fs::FileTimes::new().set_modified(mtime))
                .unwrap();
        }

        #[test]
        #[serial]
        fn supersedes_stale_opencode_sid() {
            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());

            let project_path = temp.path().join("opencode-project");
            fs::create_dir_all(&project_path).unwrap();
            let project_path = project_path.to_string_lossy().to_string();

            let db_path = temp.path().join("opencode.db");
            let stale = "ses_opencode_stored";
            let fresh = "ses_opencode_fresh";
            seed_opencode_db(&db_path, stale, &project_path);
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO session (id, directory, time_updated) VALUES (?1, ?2, ?3)",
                rusqlite::params![fresh, project_path, 2_000_000_i64],
            )
            .unwrap();
            drop(conn);
            let _db = EnvGuard::set(&[("OPENCODE_DB", &db_path)]);

            let mut inst = Instance::new("verify-opencode-bascule", &project_path);
            inst.tool = "opencode".to_string();
            inst.agent_session_id = Some(stale.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(fresh));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        }

        #[test]
        #[serial]
        fn supersedes_stale_vibe_sid() {
            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());
            let _vibe = EnvGuard::set(&[("VIBE_HOME", temp.path())]);

            let project_path = temp.path().join("vibe-project");
            fs::create_dir_all(&project_path).unwrap();
            let project_path = project_path.to_string_lossy().to_string();

            let sessions_dir = temp.path().join("logs").join("session");
            let stale = "vibe-stored-sid";
            let fresh = "vibe-fresh-sid";
            let now = SystemTime::now();
            for (sid, dir, age) in [(stale, "session-stale", 120), (fresh, "session-fresh", 10)] {
                let sdir = sessions_dir.join(dir);
                fs::create_dir_all(&sdir).unwrap();
                let meta = serde_json::json!({
                    "session_id": sid,
                    "environment": {"working_directory": project_path},
                });
                write_with_mtime(
                    &sdir.join("meta.json"),
                    &meta.to_string(),
                    now - Duration::from_secs(age),
                );
            }

            let mut inst = Instance::new("verify-vibe-bascule", &project_path);
            inst.tool = "vibe".to_string();
            inst.agent_session_id = Some(stale.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(fresh));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        }

        #[test]
        #[serial]
        fn supersedes_stale_codex_sid() {
            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());
            let _codex = EnvGuard::set(&[("CODEX_HOME", temp.path())]);

            let project_path = temp.path().join("codex-project");
            fs::create_dir_all(&project_path).unwrap();
            let project_path = project_path.to_string_lossy().to_string();

            let sessions_dir = temp.path().join("sessions");
            fs::create_dir_all(&sessions_dir).unwrap();
            let stale = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
            let fresh = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
            let now = SystemTime::now();
            for (uuid, age) in [(stale, 120), (fresh, 10)] {
                let body =
                    format!(r#"{{"type":"session_meta","payload":{{"cwd":"{project_path}"}}}}"#);
                write_with_mtime(
                    &sessions_dir.join(format!("rollout-2025-03-06T10-30-00-{uuid}.jsonl")),
                    &body,
                    now - Duration::from_secs(age),
                );
            }

            let mut inst = Instance::new("verify-codex-bascule", &project_path);
            inst.tool = "codex".to_string();
            inst.agent_session_id = Some(stale.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(fresh));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        }

        #[test]
        #[serial]
        fn codex_mtime_fallback_skips_stopped_host_peer_sid() {
            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());
            let _codex = EnvGuard::set(&[("CODEX_HOME", temp.path())]);

            let project_path = temp.path().join("codex-project");
            fs::create_dir_all(&project_path).unwrap();
            let project_path = project_path.to_string_lossy().to_string();

            let sessions_dir = temp.path().join("sessions");
            fs::create_dir_all(&sessions_dir).unwrap();
            let mine = "11111111-1111-4111-8111-111111111111";
            let peer = "22222222-2222-4222-8222-222222222222";
            let now = SystemTime::now();
            for (uuid, age) in [(mine, 120), (peer, 5)] {
                let body =
                    format!(r#"{{"type":"session_meta","payload":{{"cwd":"{project_path}"}}}}"#);
                write_with_mtime(
                    &sessions_dir.join(format!("rollout-2025-03-06T10-30-00-{uuid}.jsonl")),
                    &body,
                    now - Duration::from_secs(age),
                );
            }

            let profile = "verify-codex-stopped-host-peer";
            let mut peer_inst = Instance::new("stopped-codex-peer-id", &project_path);
            peer_inst.source_profile = profile.to_string();
            peer_inst.tool = "codex".to_string();
            peer_inst.agent_session_id = Some(peer.to_string());
            peer_inst.status = Status::Stopped;
            super::seed_disk_for_sidecar_test(profile, &peer_inst);

            let mut inst = Instance::new("verify-codex-host-peer", &project_path);
            inst.source_profile = profile.to_string();
            inst.tool = "codex".to_string();
            inst.agent_session_id = Some(mine.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(mine));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        #[test]
        #[serial]
        fn supersedes_stale_gemini_sid() {
            use sha2::{Digest, Sha256};

            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());
            let _gemini = EnvGuard::set(&[("GEMINI_CLI_HOME", temp.path())]);

            let project_dir = temp.path().join("gemini-project");
            fs::create_dir_all(&project_dir).unwrap();
            let project_path = project_dir.to_string_lossy().to_string();

            // Directory name is sha256 of the canonicalized cwd, matching the
            // capture function's exact-match branch.
            let canonical = fs::canonicalize(&project_dir).unwrap();
            let hash = Sha256::digest(canonical.to_string_lossy().as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let chats_dir = temp.path().join("tmp").join(&hash).join("chats");
            fs::create_dir_all(&chats_dir).unwrap();

            let stale = "gemini-stored-id";
            let fresh = "gemini-fresh-id";
            let now = SystemTime::now();
            for (sid, age) in [(stale, 120), (fresh, 10)] {
                let body =
                    format!(r#"{{"sessionId":"{sid}","projectHash":"{hash}","kind":"main"}}"#);
                write_with_mtime(
                    &chats_dir.join(format!("session-{sid}.json")),
                    &body,
                    now - Duration::from_secs(age),
                );
            }

            let mut inst = Instance::new("verify-gemini-bascule", &project_path);
            inst.tool = "gemini".to_string();
            inst.agent_session_id = Some(stale.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(fresh));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        }

        // A fresh launch pins the id AoE minted, so the pane's
        // conversation is known before pi writes anything. An unpinnable
        // launch (old binary, command override, sandbox) mints nothing and
        // defers to the floored poller, exactly as it did before pinning.
        #[test]
        fn pi_fresh_launch_pins_the_minted_id() {
            let pinned = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";

            let mut inst = Instance::new("pi-fresh", "/tmp/pi-fresh");
            inst.tool = "pi".to_string();
            let (sid, is_existing) = inst.acquire_session_id_with(&|_| Some(pinned.to_string()));
            assert_eq!(sid.as_deref(), Some(pinned));
            assert!(
                !is_existing,
                "a pinned launch is a new session, not a resume"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(pinned));
            assert_eq!(
                crate::session::instance::launch_command::build_resume_flags(
                    "pi",
                    pinned,
                    is_existing
                ),
                format!("--session-id {pinned}")
            );

            let mut unpinnable = Instance::new("pi-unpinnable", "/tmp/pi-fresh");
            unpinnable.tool = "pi".to_string();
            assert_eq!(unpinnable.acquire_session_id_with(&|_| None), (None, false));
            assert_eq!(unpinnable.agent_session_id, None);
        }

        #[test]
        #[serial]
        fn supersedes_stale_hermes_sid() {
            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());
            let _hermes = EnvGuard::set(&[("HERMES_HOME", temp.path())]);

            let db_path = temp.path().join("state.db");
            let stale = "20260101_000000_stored";
            let fresh = "20260101_000000_fresh";
            let project = "/tmp/aoe-test-2304-hermes";
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL, cwd TEXT, git_repo_root TEXT);
                 INSERT INTO sessions (id, source, started_at, ended_at, cwd, git_repo_root) VALUES ('{stale}','cli',1000.0,NULL,'{project}',NULL);
                 INSERT INTO sessions (id, source, started_at, ended_at, cwd, git_repo_root) VALUES ('{fresh}','cli',2000.0,NULL,'{project}',NULL);",
            ))
            .unwrap();
            drop(conn);

            // Both rows carry this project's cwd, so the scoped capture
            // sees them and supersedes the stale stored sid with the fresh
            // conversation.
            let mut inst = Instance::new("verify-hermes-bascule", project);
            inst.tool = "hermes".to_string();
            inst.agent_session_id = Some(stale.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(fresh));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        }

        #[test]
        #[serial]
        fn keeps_stored_hermes_sid_on_legacy_ambiguous_state() {
            // A legacy (column-less) state.db with two active conversations
            // is ambiguous: capture fails closed, so the stored sid is kept
            // instead of being replaced by a guess. The stored sid is the
            // OLDER row, which the pre-fix MRU code would have overridden
            // with the fresh one; this test pins the fail-closed behavior.
            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());
            let _hermes = EnvGuard::set(&[("HERMES_HOME", temp.path())]);

            let db_path = temp.path().join("state.db");
            let stored = "20260101_000000_stored";
            let other = "20260101_000000_other";
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL);
                 INSERT INTO sessions VALUES ('{stored}','cli',1000.0,NULL);
                 INSERT INTO sessions VALUES ('{other}','cli',2000.0,NULL);",
            ))
            .unwrap();
            drop(conn);

            let mut inst =
                Instance::new("verify-hermes-legacy-ambiguous", "/tmp/aoe-test-hermes-2");
            inst.tool = "hermes".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(stored));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(stored));
        }
        //
        // The per-tool bascule tests above run one instance against its
        // store. Kimi's store is a single append-only
        // `session_index.jsonl` keyed by workDir with no per-instance
        // signal, so when two AoE sessions share one cwd the MRU pick can
        // name either pane's conversation. These fixtures stage the mass
        // recovery from #3516: every pane holds a stored sid, no peer has
        // a live tmux pane yet, and the freshest record belongs to a
        // sibling.

        /// Stage two Kimi index records for one `workDir`, the "fresh"
        /// one newer than the "stored" one. The recorded `sessionDir`
        /// paths are regular files rather than directories so their
        /// mtimes are deterministic through `File::set_times`; the
        /// selector only stats whatever path the index records.
        fn seed_kimi_index(home: &std::path::Path, project: &str, stored: &str, fresh: &str) {
            let now = SystemTime::now();
            let stored_dir = home.join("sessions").join("stored");
            let fresh_dir = home.join("sessions").join("fresh");
            fs::create_dir_all(stored_dir.parent().unwrap()).unwrap();
            write_with_mtime(&stored_dir, "", now - Duration::from_secs(120));
            write_with_mtime(&fresh_dir, "", now - Duration::from_secs(5));
            fs::write(
                home.join("session_index.jsonl"),
                format!(
                    "{{\"sessionId\":\"{stored}\",\"sessionDir\":\"{}\",\"workDir\":\"{project}\"}}\n\
                     {{\"sessionId\":\"{fresh}\",\"sessionDir\":\"{}\",\"workDir\":\"{project}\"}}\n",
                    stored_dir.display(),
                    fresh_dir.display(),
                ),
            )
            .unwrap();
        }

        #[test]
        #[serial]
        fn acquire_kimi_respects_store_ownership() {
            #[derive(Clone, Copy)]
            enum PeerKind {
                None,
                CurrentKimi,
                ParkedKimi,
                ArchivedKimi,
                TrashedKimi,
                SandboxedKimi,
            }

            struct Case {
                label: &'static str,
                peer: PeerKind,
                cross_profile: bool,
                same_cwd: bool,
                fresh_sid: &'static str,
                expected: &'static str,
            }

            let cases = [
                Case {
                    label: "same-profile-current",
                    peer: PeerKind::CurrentKimi,
                    cross_profile: false,
                    same_cwd: true,
                    fresh_sid: "kimi-peer-fresh",
                    expected: "kimi-stored",
                },
                Case {
                    label: "sole-owner",
                    peer: PeerKind::None,
                    cross_profile: false,
                    same_cwd: true,
                    fresh_sid: "kimi-fresh",
                    expected: "kimi-fresh",
                },
                Case {
                    label: "cross-profile-current",
                    peer: PeerKind::CurrentKimi,
                    cross_profile: true,
                    same_cwd: true,
                    fresh_sid: "kimi-peer-fresh",
                    expected: "kimi-stored",
                },
                Case {
                    label: "different-cwd",
                    peer: PeerKind::CurrentKimi,
                    cross_profile: false,
                    same_cwd: false,
                    fresh_sid: "kimi-fresh",
                    expected: "kimi-fresh",
                },
                Case {
                    label: "cross-profile-parked",
                    peer: PeerKind::ParkedKimi,
                    cross_profile: true,
                    same_cwd: true,
                    fresh_sid: "kimi-parked-peer",
                    expected: "kimi-stored",
                },
                Case {
                    label: "cross-profile-archived",
                    peer: PeerKind::ArchivedKimi,
                    cross_profile: true,
                    same_cwd: true,
                    fresh_sid: "kimi-archived-peer",
                    expected: "kimi-stored",
                },
                Case {
                    label: "cross-profile-trashed",
                    peer: PeerKind::TrashedKimi,
                    cross_profile: true,
                    same_cwd: true,
                    fresh_sid: "kimi-trashed-peer",
                    expected: "kimi-stored",
                },
                Case {
                    label: "sandbox-private",
                    peer: PeerKind::SandboxedKimi,
                    cross_profile: false,
                    same_cwd: true,
                    fresh_sid: "kimi-unrelated-host-fresh",
                    expected: "kimi-unrelated-host-fresh",
                },
            ];

            for case in cases {
                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let project = temp.path().join(format!("project-{}", case.label));
                let other_project = temp.path().join(format!("other-{}", case.label));
                fs::create_dir_all(&project).unwrap();
                fs::create_dir_all(&other_project).unwrap();
                let project = project.to_string_lossy().to_string();
                let other_project = other_project.to_string_lossy().to_string();
                let kimi_home = temp.path().join("kimi-home");
                fs::create_dir_all(&kimi_home).unwrap();
                let _kimi = EnvGuard::set(&[("KIMI_CODE_HOME", kimi_home.to_str().unwrap())]);
                seed_kimi_index(&kimi_home, &project, "kimi-stored", case.fresh_sid);

                let caller_profile = format!("kimi-owner-{}-caller", case.label);
                if !matches!(case.peer, PeerKind::None) {
                    let peer_profile = if case.cross_profile {
                        format!("kimi-owner-{}-peer", case.label)
                    } else {
                        caller_profile.clone()
                    };
                    let peer_path = if case.same_cwd {
                        project.as_str()
                    } else {
                        other_project.as_str()
                    };
                    let mut peer = Instance::new("ownership-peer", peer_path);
                    peer.source_profile = peer_profile.clone();
                    match case.peer {
                        PeerKind::None => unreachable!(),
                        PeerKind::CurrentKimi => {
                            peer.tool = "kimi".to_string();
                            peer.agent_session_id = Some(case.fresh_sid.to_string());
                        }
                        PeerKind::ParkedKimi => {
                            peer.tool = "claude".to_string();
                            peer.prior_tool_session_ids.insert(
                                "kimi".to_string(),
                                crate::session::instance::PriorToolSession {
                                    agent_session_id: Some(case.fresh_sid.to_string()),
                                    acp_session_id: None,
                                },
                            );
                        }
                        PeerKind::ArchivedKimi => {
                            peer.tool = "kimi".to_string();
                            peer.agent_session_id = Some(case.fresh_sid.to_string());
                            peer.archive();
                        }
                        PeerKind::TrashedKimi => {
                            peer.tool = "kimi".to_string();
                            peer.agent_session_id = Some(case.fresh_sid.to_string());
                            peer.trash();
                        }
                        PeerKind::SandboxedKimi => {
                            peer.tool = "kimi".to_string();
                            peer.agent_session_id = Some("kimi-sandbox-only".to_string());
                            peer.sandbox_info = Some(crate::session::SandboxInfo {
                                enabled: true,
                                container_id: None,
                                image: "test-image".to_string(),
                                container_name: format!("aoe-test-{}", case.label),
                                extra_env: None,
                                custom_instruction: None,
                                container_workdir: None,
                                before_start_env: Vec::new(),
                            });
                        }
                    }
                    super::seed_disk_for_sidecar_test(&peer_profile, &peer);
                }

                let mut inst = Instance::new("ownership-caller", &project);
                inst.source_profile = caller_profile;
                inst.tool = "kimi".to_string();
                inst.agent_session_id = Some("kimi-stored".to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(case.expected), "{}", case.label);
                assert!(is_existing, "{}", case.label);
                assert_eq!(
                    inst.agent_session_id.as_deref(),
                    Some(case.expected),
                    "{}",
                    case.label
                );
            }
        }
        #[test]
        #[serial]
        fn kimi_inactive_same_profile_sids_are_excluded() {
            // Restorable inactive rows retain their Kimi conversation.
            // The same-profile exclusion feeds both acquire and the live
            // poller snapshot, while the cross-profile table above proves
            // the all-profile ownership predicate independently.
            for (label, peer_sid) in [
                ("trashed", "kimi-trashed-peer"),
                ("archived", "kimi-archived-peer"),
            ] {
                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let project = temp.path().join(format!("inactive-project-{label}"));
                fs::create_dir_all(&project).unwrap();
                let project = project.to_string_lossy().to_string();
                let kimi_home = temp.path().join("kimi-home");
                fs::create_dir_all(&kimi_home).unwrap();
                let _kimi = EnvGuard::set(&[("KIMI_CODE_HOME", kimi_home.to_str().unwrap())]);
                seed_kimi_index(&kimi_home, &project, "kimi-stored", peer_sid);

                let profile = format!("kimi-inactive-{label}");
                let mut peer = Instance::new("inactive-peer", &project);
                peer.source_profile = profile.clone();
                peer.tool = "kimi".to_string();
                peer.agent_session_id = Some(peer_sid.to_string());
                match label {
                    "trashed" => peer.trash(),
                    "archived" => peer.archive(),
                    _ => unreachable!(),
                }
                super::seed_disk_for_sidecar_test(&profile, &peer);

                let mut inst = Instance::new("inactive-caller", &project);
                inst.source_profile = profile;
                inst.tool = "kimi".to_string();
                inst.agent_session_id = Some("kimi-stored".to_string());
                inst.resume_intent = ResumeIntent::Default;

                let exclusion = inst.retroactive_capture_exclusion_set();
                assert!(exclusion.contains(peer_sid), "{label} exclusion");
                let (sid, _is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some("kimi-stored"), "{label} anchor");
            }
        }

        #[test]
        #[serial]
        fn kimi_var_form_homes_still_detect_shared_store() {
            // Profiles spell homes in launch's environment grammar: a peer
            // profile writing `KIMI_CODE_HOME=$KIMI_SHARED` resolves to
            // the same physical store as the caller's ambient spelling,
            // and must count as shared rather than compare as the literal
            // text `$KIMI_SHARED` (#3516 review cycle).
            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());
            let project = temp.path().join("var-project");
            fs::create_dir_all(&project).unwrap();
            let project = project.to_string_lossy().to_string();
            let kimi_home = temp.path().join("kimi-home");
            fs::create_dir_all(&kimi_home).unwrap();
            let _kimi = EnvGuard::set(&[("KIMI_CODE_HOME", kimi_home.to_str().unwrap())]);
            let _shared = EnvGuard::set(&[("KIMI_SHARED", kimi_home.to_str().unwrap())]);

            let mut peer_config =
                crate::session::profile_config::load_profile_config("kimi-var-peer").unwrap();
            peer_config.overrides.insert(
                "environment".to_string(),
                serde_json::json!(["KIMI_CODE_HOME=$KIMI_SHARED"]),
            );
            crate::session::profile_config::save_profile_config("kimi-var-peer", &peer_config)
                .unwrap();

            seed_kimi_index(&kimi_home, &project, "kimi-stored", "kimi-peer-fresh");
            let mut peer = Instance::new("var-peer", &project);
            peer.source_profile = "kimi-var-peer".to_string();
            peer.tool = "kimi".to_string();
            peer.agent_session_id = Some("kimi-peer-fresh".to_string());
            super::seed_disk_for_sidecar_test("kimi-var-peer", &peer);

            let mut inst = Instance::new("var-caller", &project);
            inst.source_profile = "kimi-var-caller".to_string();
            inst.tool = "kimi".to_string();
            inst.agent_session_id = Some("kimi-stored".to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(
                sid.as_deref(),
                Some("kimi-stored"),
                "a $VAR-spelled peer home resolving to the same store must count as shared"
            );
        }
        #[test]
        #[serial]
        fn kimi_shared_store_refuses_id_less_retroactive_fill() {
            // The shared-store refusal lives in try_retroactive_capture,
            // so it also covers the id-less doors (recovery of a session
            // that never captured, read-command self-heal): the scan is
            // refused entirely, yielding a fresh start instead of an
            // unattributable adoption, while a sole-owner store keeps
            // filling from the index.
            for (label, peer_state, expected) in [
                ("shared-unattributed", Some("unattributed"), None),
                ("shared-stopped-known", Some("stopped-known"), None),
                ("solo", None, Some("kimi-fresh")),
            ] {
                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let project = temp.path().join(format!("idless-project-{label}"));
                fs::create_dir_all(&project).unwrap();
                let project = project.to_string_lossy().to_string();
                let kimi_home = temp.path().join("kimi-home");
                fs::create_dir_all(&kimi_home).unwrap();
                let _kimi = EnvGuard::set(&[("KIMI_CODE_HOME", kimi_home.to_str().unwrap())]);
                let old_sid = if peer_state == Some("stopped-known") {
                    "kimi-stale-peer"
                } else {
                    "kimi-stored"
                };
                seed_kimi_index(&kimi_home, &project, old_sid, "kimi-fresh");

                if let Some(peer_state) = peer_state {
                    let profile = format!("kimi-idless-{label}");
                    let mut peer = Instance::new("idless-peer", &project);
                    peer.source_profile = profile.clone();
                    peer.tool = "kimi".to_string();
                    if peer_state == "stopped-known" {
                        peer.status = Status::Stopped;
                        peer.agent_session_id = Some(old_sid.to_string());
                    }
                    super::seed_disk_for_sidecar_test(&profile, &peer);
                }

                let mut inst = Instance::new("idless-caller", &project);
                inst.tool = "kimi".to_string();

                assert_eq!(
                    inst.try_retroactive_capture(),
                    expected.map(str::to_string),
                    "{label} store"
                );
            }
        }

        #[test]
        #[serial]
        fn kimi_anchor_kept_when_profile_list_unreadable() {
            // Fail-closed branch: an erroring profile walk must report
            // shared, keeping the anchored sid rather than licensing the
            // MRU retarget. Driven through the existing list_profiles
            // injection seam.
            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());
            let project = temp.path().join("failclosed-project");
            fs::create_dir_all(&project).unwrap();
            let project = project.to_string_lossy().to_string();
            let kimi_home = temp.path().join("kimi-home");
            fs::create_dir_all(&kimi_home).unwrap();
            let _kimi = EnvGuard::set(&[("KIMI_CODE_HOME", kimi_home.to_str().unwrap())]);
            seed_kimi_index(&kimi_home, &project, "kimi-stored", "kimi-peer-fresh");

            let profile = "kimi-failclosed";
            let mut peer = Instance::new("failclosed-peer", &project);
            peer.source_profile = profile.to_string();
            peer.tool = "kimi".to_string();
            peer.agent_session_id = Some("kimi-peer-fresh".to_string());
            super::seed_disk_for_sidecar_test(profile, &peer);

            let mut inst = Instance::new("failclosed-caller", &project);
            inst.source_profile = profile.to_string();
            inst.tool = "kimi".to_string();
            inst.agent_session_id = Some("kimi-stored".to_string());
            inst.resume_intent = ResumeIntent::Default;

            let _failure = crate::session::FailNextListProfilesGuard::new();
            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(
                sid.as_deref(),
                Some("kimi-stored"),
                "an unreadable profile list must count as shared"
            );
        }

        #[test]
        #[serial]
        fn kimi_invalid_peer_config_fails_closed() {
            let temp = tempdir().unwrap();
            let _home = isolate_app_dir_at(temp.path());
            let project = temp.path().join("invalid-config-project");
            fs::create_dir_all(&project).unwrap();
            let project = project.to_string_lossy().to_string();
            let kimi_home = temp.path().join("kimi-home");
            let ambient_home = temp.path().join("ambient-kimi-home");
            fs::create_dir_all(&kimi_home).unwrap();
            fs::create_dir_all(&ambient_home).unwrap();
            let _kimi = EnvGuard::set(&[("KIMI_CODE_HOME", ambient_home.to_str().unwrap())]);
            seed_kimi_index(&kimi_home, &project, "kimi-stored", "kimi-peer-fresh");

            let caller_profile = "kimi-invalid-config-caller";
            let mut caller_config =
                crate::session::profile_config::load_profile_config(caller_profile).unwrap();
            caller_config.overrides.insert(
                "environment".to_string(),
                serde_json::json!([format!("KIMI_CODE_HOME={}", kimi_home.display())]),
            );
            crate::session::profile_config::save_profile_config(caller_profile, &caller_config)
                .unwrap();

            let peer_profile = "kimi-invalid-config-peer";
            let mut peer = Instance::new("invalid-config-peer", &project);
            peer.source_profile = peer_profile.to_string();
            peer.tool = "kimi".to_string();
            peer.agent_session_id = Some("kimi-peer-fresh".to_string());
            super::seed_disk_for_sidecar_test(peer_profile, &peer);
            fs::write(
                crate::session::get_profile_dir_path(peer_profile)
                    .unwrap()
                    .join("config.toml"),
                "environment = [",
            )
            .unwrap();

            let mut inst = Instance::new("invalid-config-caller", &project);
            inst.source_profile = caller_profile.to_string();
            inst.tool = "kimi".to_string();
            inst.agent_session_id = Some("kimi-stored".to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(
                sid.as_deref(),
                Some("kimi-stored"),
                "invalid peer config must not license ambient-home MRU"
            );
        }
    }
}
