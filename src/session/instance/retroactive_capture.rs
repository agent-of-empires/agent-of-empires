//! Adopting a session id an agent minted on its own, after launch.

use super::*;

impl Instance {
    /// Full set of session IDs capture must skip for this instance: live tmux
    /// ownership, cascade-cleared ids, conversations same-project peers parked
    /// while running another tool, and inactive peers that still own records
    /// in a shared host store.
    pub(super) fn retroactive_capture_exclusion_set(&self) -> HashSet<String> {
        crate::session::capture::compose_exclusion_with_persisted_peers(
            &self.id,
            &self.project_path,
            &self.tool,
            self.tool == "claude"
                || (matches!(self.tool.as_str(), "codex" | "kimi") && !self.is_sandboxed()),
            &self.effective_profile(),
            &self.retroactive_capture_excludes,
        )
    }

    /// Whether another AoE session shares this one's Kimi store, which makes
    /// the session index useless for attributing a conversation to a pane.
    /// Both own homes are supplied so a hook-minted `KIMI_CODE_HOME` still
    /// counts static-profile siblings as sharing.
    fn kimi_store_is_shared(&self) -> bool {
        crate::session::capture::kimi_store_is_shared(
            &self.id,
            &self.project_path,
            &self.resolved_host_environment(),
            &self.profile_host_environment(),
        )
    }

    pub(crate) fn try_retroactive_capture(&self) -> Option<String> {
        let result: Option<String> = match self.tool.as_str() {
            "claude" => {
                // Claude additionally extends the common live and parked-id
                // exclusion with stopped, archived, or pane-less peer sids so
                // the mtime fallback skips peers whose jsonl outlived their
                // tmux session (#2355).
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    capture_claude_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                        None,
                    )
                    .ok()
                } else {
                    capture_claude_session_id(
                        &self.project_path,
                        None,
                        &exclusion,
                        &self.resolved_host_environment(),
                    )
                    .ok()
                }
            }
            "opencode" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_opencode_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                        None,
                    )
                    .ok()
                } else {
                    try_capture_opencode_session_id(&self.project_path, &exclusion, None).ok()
                }
            }
            "vibe" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_vibe_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    capture_vibe_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "pi" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_pi_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    capture_pi_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "omp" => {
                let options = self.omp_capture_options()?;
                let exclusion = self.retroactive_capture_exclusion_set();
                let tmux_session_name = self
                    .tmux_env_session_name()
                    .or_else(|| self.tmux_session().ok().map(|s| s.name().to_string()))?;
                let metadata = self.omp_capture_metadata(&tmux_session_name, &options, None)?;
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    let marker = omp_sandbox_launch_marker(&self.id);
                    try_capture_omp_session_id_in_container(
                        &container_name,
                        &metadata,
                        &exclusion,
                        Some(&marker),
                    )
                    .ok()
                } else {
                    capture_omp_session_id(&metadata, &exclusion, &tmux_session_name).ok()
                }
            }
            "codex" => {
                if self.is_sandboxed() {
                    // Sandboxed Codex sessions have instance-private homes, so
                    // their transcript stores cannot contain a sibling's
                    // rollout (#3317). The common helper therefore omits
                    // inactive same-tool peers on this path.
                    let exclusion = self.retroactive_capture_exclusion_set();
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_codex_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    // Host Codex sessions share `~/.codex/sessions/`. Include
                    // stopped and pane-less same-directory peers so the mtime
                    // scan cannot adopt a sibling's newer conversation.
                    let exclusion = self.retroactive_capture_exclusion_set();
                    capture_codex_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "gemini" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_gemini_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    capture_gemini_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "hermes" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_hermes_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    capture_hermes_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "copilot" => {
                // Copilot stores sessions in a SQLite db. Host capture reads it
                // directly; sandbox resume is a follow-up (the container's db is
                // not read over `docker exec`), so a sandboxed Copilot session
                // simply starts fresh on restart.
                if self.is_sandboxed() {
                    None
                } else {
                    let exclusion = self.retroactive_capture_exclusion_set();
                    capture_copilot_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "kimi" => {
                // Kimi records sessions in `session_index.jsonl` under the
                // resolved `KIMI_CODE_HOME`, keyed by workDir. Host capture
                // reads it through the launched pane's environment; sandbox
                // resume is a follow-up (the container's index is not read
                // over `docker exec`), so a sandboxed Kimi session starts
                // fresh on restart, mirroring Copilot.
                if self.is_sandboxed() {
                    None
                } else if self.kimi_store_is_shared() {
                    // A shared store names no pane: its newest same-workDir
                    // record is as likely to be a co-located peer's
                    // conversation as this one's, so the MRU scan is refused
                    // entirely (#3516). An anchored sid keeps its value on
                    // the freshest path; an id-less session starts fresh
                    // rather than adopt a peer conversation. Sole-owner
                    // stores keep the MRU retarget, which stays the
                    // new-conversation promotion path (#2291).
                    None
                } else {
                    let exclusion = self.retroactive_capture_exclusion_set();
                    // Retroactive recovery is unrestricted (no launch floor):
                    // resuming an older session on restart is the goal here.
                    capture_kimi_session_id(
                        &self.project_path,
                        &exclusion,
                        None,
                        &self.resolved_host_environment(),
                    )
                    .ok()
                }
            }
            "prime-agent" => {
                // Prime Agent writes one JSONL per session under
                // `~/.prime/agent/sessions`, header line keyed by cwd. Host
                // capture reads it directly; sandbox resume is a follow-up
                // (the container's sessions dir is not read over `docker
                // exec`), so a sandboxed Prime Agent session starts fresh on
                // restart, mirroring Copilot and Kimi.
                if self.is_sandboxed() {
                    None
                } else {
                    let exclusion = self.retroactive_capture_exclusion_set();
                    // Retroactive recovery is unrestricted (no launch floor):
                    // resuming an older session on restart is the goal here.
                    capture_prime_agent_session_id(&self.project_path, &exclusion, None).ok()
                }
            }
            _ => None,
        };
        result.and_then(validated_session_id)
    }

    /// Canonical `(tool, project_path)` keys shared by two or more id-less
    /// sessions. A read-command self-heal must abstain on these: the
    /// capture-deferred stores are keyed by directory (opencode indexes its
    /// SQLite `session` rows by `directory`, codex/gemini/... by cwd), so when
    /// several co-located id-less sessions of the same tool share one cwd, AoE
    /// cannot attribute a store entry to a specific instance and guessing risks
    /// resuming the wrong conversation. `foreign_sid_holder` already blocks a
    /// duplicate write under the flock; this declines the guess one step
    /// earlier so no instance mis-adopts. Keyed on the canonicalized path so a
    /// symlinked and a realpath spelling of the same dir count as one.
    pub(crate) fn contended_capture_cwds(instances: &[Instance]) -> HashSet<(String, String)> {
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut contended: HashSet<(String, String)> = HashSet::new();
        for inst in instances {
            // Only a peer that still owns no id AND has a live tmux pane can
            // cause a real attribution ambiguity: a stopped/dead peer's agent
            // is no longer writing to the tool's store, so it cannot be the
            // owner of a freshly-observed entry. Counting it would strand the
            // live session forever behind a ghost. Mirrors the liveness gate
            // `self_heal_session_id` applies to `self`.
            if inst.agent_session_id.is_some() || !inst.tmux_alive_cached() {
                continue;
            }
            let key = inst.contended_capture_key();
            if !seen.insert(key.clone()) {
                contended.insert(key);
            }
        }
        contended
    }

    /// Cache-only tmux liveness for the self-heal gates. Only a HIT
    /// (`Some(true)`) counts as live; a fresh-cache miss, a TTL-expired
    /// snapshot, or an unreachable server all read as not-live. Both self-heal
    /// call sites `refresh_session_cache()` immediately before, so a genuinely
    /// live session is always `Some(true)` here; treating the rest as not-live
    /// at worst DEFERS a best-effort heal, and never forks a `has-session`
    /// subprocess per dead id-less session (which `Session::exists` would, since
    /// it only short-circuits on `Some(true)` and falls through otherwise).
    pub(super) fn tmux_alive_cached(&self) -> bool {
        let name = crate::tmux::Session::resolve_name(&self.id, &self.title);
        crate::tmux::session_exists_from_cache(&name) == Some(true)
    }

    /// The `(tool, canonical cwd)` identity used for shared-cwd contention.
    /// Canonicalized so a symlinked and a realpath spelling of the same dir
    /// count as one, matching the directory match in `filter_agent_sessions`.
    pub(super) fn contended_capture_key(&self) -> (String, String) {
        (
            self.tool.clone(),
            crate::session::capture::canonicalize_or_raw(&self.project_path)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn contended_capture_cwds_flags_only_live_colocated_idless_same_tool() {
        let cwd = std::env::current_dir().unwrap();
        let p = cwd.to_str().unwrap();
        let canon = crate::session::capture::canonicalize_or_raw(p)
            .to_string_lossy()
            .into_owned();
        let mk = |title: &str, tool: &str, sid: Option<&str>| {
            let mut i = Instance::new(title, p);
            i.tool = tool.to_string();
            i.agent_session_id = sid.map(str::to_string);
            i
        };
        let instances = vec![
            mk("a", "opencode", None),          // id-less opencode, live, same cwd
            mk("b", "opencode", None),          // -> contends with a (both live)
            mk("c", "codex", None),             // lone codex -> not contended
            mk("d", "opencode", Some("ses_x")), // has an id -> ignored
            mk("e", "opencode", None),          // id-less opencode, DEAD -> uncounted
        ];
        // Start from a clean, fresh, empty cache so name resolution is
        // deterministic (a prior test's residual cache could otherwise make
        // `resolve_name` pick a variant name shape). Resolve names the same way
        // `tmux_alive_cached` does, then mark a, b, c, d present and leave e out.
        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[]);
        let name_of = |i: &Instance| crate::tmux::Session::resolve_name(&i.id, &i.title);
        let live: Vec<String> = instances[..4].iter().map(name_of).collect();
        let live_refs: Vec<&str> = live.iter().map(String::as_str).collect();
        guard.force_present(&live_refs);

        let contended = Instance::contended_capture_cwds(&instances);

        // a + b: two live id-less opencode in one cwd -> contended.
        assert!(contended.contains(&("opencode".to_string(), canon.clone())));
        // c: a single live codex -> not contended (proves the >=2 + tool key).
        assert!(!contended.contains(&("codex".to_string(), canon.clone())));

        // A live opencode session sharing its cwd with only a DEAD id-less
        // opencode peer must NOT be contended: the dead peer's agent is no
        // longer writing to the store, so it cannot cause a mis-attribution.
        // Rebuild with just one live + one dead to isolate that path.
        let live_only = mk("live", "opencode", None);
        let dead = mk("dead", "opencode", None);
        guard.force_present(&[name_of(&live_only).as_str()]);
        let contended = Instance::contended_capture_cwds(&[live_only, dead]);
        assert!(
            !contended.contains(&("opencode".to_string(), canon)),
            "a dead id-less peer must not force the live session to abstain"
        );
    }
}
