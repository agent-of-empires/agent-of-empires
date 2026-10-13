//! Tool sessions: user-configured dev tools (lazygit, yazi, tig, etc.) that
//! run in persistent tmux sessions tied to an agent session's working directory.

use anyhow::{bail, Result};

use super::utils::{append_session_setup_args, is_pane_dead, sanitize_session_name};
use super::{refresh_session_cache, TOOL_PREFIX};
use crate::cli::truncate_id;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LegacyToolScope {
    pub rows: Vec<(String, String, String)>, // profile, full ID, derived agent name
    pub tools: Vec<(String, String)>,        // raw tool name, sanitized name prefix
}

#[derive(Debug, thiserror::Error)]
#[error("Legacy tool ownership cannot be adopted: {0}")]
pub(crate) struct LegacyToolUnavailable(pub &'static str);

pub struct ToolSession {
    name: String,
}

impl ToolSession {
    /// Transport name only; lifecycle operations require verified snapshot ownership.
    pub fn new(session_id: &str, session_title: &str, tool_name: &str) -> Self {
        Self::from_resolution(session_id, session_title, tool_name, false)
    }

    /// [`Self::new`] for **render paths**: resolved from the shared snapshot
    /// only, never refreshing. A stale snapshot yields the derived name until
    /// the background snapshot poller refreshes it; paint must never wait on
    /// tmux.
    pub fn for_display(session_id: &str, session_title: &str, tool_name: &str) -> Self {
        Self::from_resolution(session_id, session_title, tool_name, true)
    }

    fn from_resolution(
        session_id: &str,
        session_title: &str,
        tool_name: &str,
        display_only: bool,
    ) -> Self {
        let name = Self::with_name_shape(session_id, session_title, tool_name, |derived, shape| {
            if display_only {
                crate::tmux::session_name_for_display(&derived, shape)
            } else {
                crate::tmux::live_session_name(&derived, shape)
            }
        });
        Self { name }
    }

    fn with_name_shape<R>(
        session_id: &str,
        session_title: &str,
        tool_name: &str,
        resolve: impl FnOnce(String, &crate::tmux::NameShape<'_>) -> R,
    ) -> R {
        let prefix = Self::name_prefix(tool_name);
        let suffix = format!("_{}", truncate_id(session_id, 8));
        let derived = Self::generate_name(session_id, session_title, tool_name);
        resolve(
            derived,
            &crate::tmux::NameShape {
                prefix: &prefix,
                suffix: &suffix,
                kind: crate::tmux::SessionKind::Tool,
            },
        )
    }

    fn resolve_snapshot<'a>(
        id: &str,
        title: &str,
        tool_name: &str,
        panes: &'a std::collections::HashMap<String, crate::tmux::PaneMetadata>,
    ) -> Result<(
        std::borrow::Cow<'a, str>,
        Option<&'a crate::tmux::PaneMetadata>,
    )> {
        use crate::tmux::ToolPaneOwner;
        let owns = |metadata: &crate::tmux::PaneMetadata| matches!(&metadata.tool_owner, ToolPaneOwner::Named { instance_id, tool_name: owner } if instance_id == id && owner == tool_name);
        Self::with_name_shape(id, title, tool_name, |derived, shape| {
            if let Some((name, metadata)) = panes
                .get_key_value(&derived)
                .filter(|(_, metadata)| owns(metadata))
            {
                return Ok((std::borrow::Cow::Borrowed(name.as_str()), Some(metadata)));
            }
            let mut found = None;
            for (name, metadata) in panes.iter().filter(|(name, _)| shape.matches(name)) {
                match &metadata.tool_owner {
                    ToolPaneOwner::Unmarked | ToolPaneOwner::Invalid => {
                        bail!("Tool session ownership is unavailable")
                    }
                    ToolPaneOwner::Named { .. } if owns(metadata) => {
                        anyhow::ensure!(found.is_none(), "Tool session ownership is ambiguous");
                        found = Some((name, metadata));
                    }
                    ToolPaneOwner::Named { .. } => {}
                }
            }
            if let Some((name, metadata)) = found {
                return Ok((std::borrow::Cow::Borrowed(name.as_str()), Some(metadata)));
            }
            anyhow::ensure!(
                !panes.contains_key(&derived),
                "Tool session name belongs to another owner"
            );
            Ok((std::borrow::Cow::Owned(derived), None))
        })
    }

    pub(crate) fn metadata_in<'a>(
        id: &str,
        title: &str,
        tool_name: &str,
        panes: &'a std::collections::HashMap<String, crate::tmux::PaneMetadata>,
    ) -> Result<Option<(std::borrow::Cow<'a, str>, &'a crate::tmux::PaneMetadata)>> {
        let (name, metadata) = Self::resolve_snapshot(id, title, tool_name, panes)?;
        Ok(metadata.map(|metadata| (name, metadata)))
    }

    pub(crate) fn from_snapshot(
        id: &str,
        title: &str,
        tool_name: &str,
        panes: &std::collections::HashMap<String, crate::tmux::PaneMetadata>,
    ) -> Result<Self> {
        let (name, _) = Self::resolve_snapshot(id, title, tool_name, panes)?;
        Ok(Self {
            name: name.into_owned(),
        })
    }

    pub(crate) fn legacy_metadata_in<'a>(
        id: &str,
        _title: &str,
        tool_name: &str,
        profile: &str,
        panes: &'a std::collections::HashMap<String, crate::tmux::PaneMetadata>,
        scope: &LegacyToolScope,
    ) -> Result<Option<(&'a str, &'a crate::tmux::PaneMetadata)>> {
        use crate::tmux::ToolPaneOwner;
        let prefix = Self::name_prefix(tool_name);
        let suffix = format!("_{}", truncate_id(id, 8));
        let shape = crate::tmux::NameShape {
            prefix: &prefix,
            suffix: &suffix,
            kind: crate::tmux::SessionKind::Tool,
        };
        {
            let mut found = None;
            for (name, metadata) in panes.iter().filter(|(name, _)| shape.matches(name)) {
                match &metadata.tool_owner {
                    ToolPaneOwner::Invalid => bail!(LegacyToolUnavailable("invalid owner marker")),
                    ToolPaneOwner::Named { .. } => {
                        bail!(LegacyToolUnavailable(
                            "named owner conflicts with legacy adoption"
                        ));
                    }
                    ToolPaneOwner::Unmarked => {
                        anyhow::ensure!(
                            found.is_none(),
                            LegacyToolUnavailable("ambiguous legacy panes")
                        );
                        found = Some((name.as_str(), metadata));
                    }
                }
            }
            let Some((name, metadata)) = found else {
                return Ok(None);
            };
            Self::validate_identity(metadata)?;
            anyhow::ensure!(
                metadata
                    .session_kind
                    .as_deref()
                    .is_none_or(|kind| kind == "tool"),
                LegacyToolUnavailable("candidate has a non-tool kind marker")
            );
            anyhow::ensure!(
                !scope.rows.iter().any(|(_, _, agent)| agent == name),
                LegacyToolUnavailable("candidate is an agent session")
            );
            let mut matched = false;
            for (owner_profile, owner_id, _) in &scope.rows {
                if name.rsplit_once('_').map(|(_, suffix)| suffix) != Some(truncate_id(owner_id, 8))
                {
                    continue;
                }
                for (raw_tool, prefix) in &scope.tools {
                    let matches = name.starts_with(prefix);
                    if matches {
                        anyhow::ensure!(
                            !matched
                                && owner_profile == profile
                                && owner_id == id
                                && raw_tool == tool_name,
                            LegacyToolUnavailable("ambiguous row or raw tool name across profiles")
                        );
                        matched = true;
                    }
                }
            }
            anyhow::ensure!(
                matched,
                LegacyToolUnavailable("owner missing from complete catalogue")
            );
            Ok(Some((name, metadata)))
        }
    }

    fn validate_identity(metadata: &crate::tmux::PaneMetadata) -> Result<u32> {
        anyhow::ensure!(
            metadata
                .session_id
                .strip_prefix('$')
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|ch| ch.is_ascii_digit()))
                && metadata
                    .pane_id
                    .strip_prefix('%')
                    .is_some_and(|id| !id.is_empty() && id.bytes().all(|ch| ch.is_ascii_digit())),
            LegacyToolUnavailable("immutable tmux IDs unavailable")
        );
        let pane_pid = metadata
            .pane_pid
            .filter(|pid| *pid != 0)
            .ok_or(LegacyToolUnavailable("pane process identity unavailable"))?;
        Ok(pane_pid)
    }

    pub(crate) fn legacy_identity(
        metadata: &crate::tmux::PaneMetadata,
    ) -> Result<crate::session::LegacyToolIdentity> {
        let pane_pid = Self::validate_identity(metadata)?;
        Ok(crate::session::LegacyToolIdentity {
            session_id: metadata.session_id.clone(),
            pane_id: metadata.pane_id.clone(),
            pane_pid,
        })
    }

    fn snapshot_condition(
        name: &str,
        identity: &crate::session::LegacyToolIdentity,
        owner: &str,
    ) -> String {
        let literal = crate::tmux::Session::tmux_format_literal;
        let session_id = literal(&identity.session_id);
        let pane_id = literal(&identity.pane_id);
        let name = literal(name);
        let kind = "#{||:#{==:#{@aoe_kind},},#{==:#{@aoe_kind},tool}}";
        let owner = literal(owner);
        let pid = identity.pane_pid;
        format!("#{{&&:#{{&&:#{{==:#{{session_id}},{session_id}}},#{{==:#{{pane_id}},{pane_id}}}}},#{{&&:#{{&&:#{{==:#{{session_name}},{name}}},#{{==:#{{pane_pid}},{pid}}}}},#{{&&:#{{==:#{{@aoe_tool_owner}},{owner}}},{kind}}}}}}}")
    }

    pub(crate) fn adopt_legacy_snapshot(
        id: &str,
        title: &str,
        tool_name: &str,
        profile: &str,
        adoption: &crate::session::LegacyToolAdoption,
        panes: &std::collections::HashMap<String, crate::tmux::PaneMetadata>,
        scope: &LegacyToolScope,
    ) -> Result<(Self, crate::session::LegacyToolIdentity)> {
        let (name, metadata) =
            Self::legacy_metadata_in(id, title, tool_name, profile, panes, scope)?
                .ok_or(LegacyToolUnavailable("captured legacy pane is absent"))?;
        anyhow::ensure!(
            name == adoption.tmux_session
                && metadata.session_id == adoption.identity.session_id
                && metadata.pane_id == adoption.identity.pane_id
                && metadata.pane_pid == Some(adoption.identity.pane_pid),
            LegacyToolUnavailable("captured pane was replaced")
        );
        let owner = serde_json::to_string(&(id, tool_name))?;
        let condition = Self::snapshot_condition(name, &adoption.identity, "");
        let quote = crate::tmux::Session::tmux_command_string_literal;
        let session_target = quote(&adoption.identity.session_id);
        let replacement = format!("set-option -t {session_target} @aoe_tool_owner {} ; set-option -t {session_target} @aoe_kind tool ; display-message -p aoe-tool-adopted", quote(&owner));
        Self::run_snapshot_command(
            &adoption.identity.pane_id,
            &condition,
            &replacement,
            "aoe-tool-adopted",
        )?;
        Ok((
            Self {
                name: name.to_owned(),
            },
            adoption.identity.clone(),
        ))
    }

    fn run_snapshot_command(
        pane_id: &str,
        condition: &str,
        effect: &str,
        success: &str,
    ) -> Result<()> {
        let mut command = crate::tmux::tmux_command();
        command.args(["if-shell", "-t", pane_id, "-F", condition, effect]);
        let output = crate::tmux::TmuxCommandDeadline::new().run(&mut command)?;
        anyhow::ensure!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.trim() == success),
            LegacyToolUnavailable("captured identity or owner changed before tmux effect")
        );
        Ok(())
    }

    pub(crate) fn kill_verified(
        &self,
        id: &str,
        tool_name: &str,
        identity: &crate::session::LegacyToolIdentity,
    ) -> Result<()> {
        let owner = serde_json::to_string(&(id, tool_name))?;
        let condition = Self::snapshot_condition(&self.name, identity, &owner);
        let target = crate::tmux::Session::tmux_command_string_literal(&identity.session_id);
        let effect = format!("kill-session -t {target} ; display-message -p aoe-tool-stopped");
        Self::run_snapshot_command(&identity.pane_id, &condition, &effect, "aoe-tool-stopped")?;
        refresh_session_cache();
        Ok(())
    }

    /// Purely derive the sub-session name, with no reference to what is live.
    /// Callers wanting the session's CURRENT name want [`Self::new`].
    pub fn generate_name(session_id: &str, session_title: &str, tool_name: &str) -> String {
        format!(
            "{}{}_{}",
            Self::name_prefix(tool_name),
            sanitize_session_name(session_title),
            truncate_id(session_id, 8)
        )
    }

    /// `aoe_tool_<tool>_`: everything before the (movable) title.
    pub(crate) fn name_prefix(tool_name: &str) -> String {
        format!("{TOOL_PREFIX}{}_", sanitize_session_name(tool_name))
    }

    pub fn session_name(&self) -> &str {
        &self.name
    }

    pub fn exists(&self) -> bool {
        crate::tmux::session_exists(&self.name)
    }

    pub fn is_pane_dead(&self) -> bool {
        is_pane_dead(&self.name)
    }

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: &str,
        size: Option<(u16, u16)>,
        profile: &str,
        instance_id: &str,
        tool_name: &str,
    ) -> Result<()> {
        let config = crate::tmux::tmux_option_config(profile);

        let mut args = Vec::from(
            [
                "new-session",
                "-d",
                "-s",
                self.name.as_str(),
                "-c",
                working_dir,
            ]
            .map(str::to_owned),
        );

        if let Some((width, height)) = size {
            args.extend(["-x".to_string(), width.to_string()]);
            args.extend(["-y".to_string(), height.to_string()]);
        }

        args.push(command.to_string());

        let target = format!("={}:", self.name);
        append_session_setup_args(&mut args, &target, &config, None);
        args.extend([
            ";".into(),
            "set-option".into(),
            "-t".into(),
            target.clone(),
            "@aoe_tool_owner".into(),
            serde_json::to_string(&(instance_id, tool_name))?,
        ]);
        crate::tmux::append_session_kind_args(&mut args, &target, crate::tmux::SessionKind::Tool);

        let output = crate::tmux::tmux_command().args(&args).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to create tool session '{}': {}", self.name, stderr);
        }

        refresh_session_cache();
        Ok(())
    }

    /// Poll the pane for up to ~200ms, checking every ~25ms, and error out
    /// if it's still dead when the budget expires. A tool command that's
    /// misconfigured (e.g. a usage error on an unrecognized flag) exits
    /// near-instantly; without this check, `attach_tool_session` hands the
    /// terminal to a dead, `remain-on-exit`-held pane and the user's only
    /// way out is Ctrl+C, which (absent the SIGINT guard around the attach)
    /// kills aoe itself rather than just the dead pane.
    pub fn wait_until_ready(&self) -> Result<()> {
        const BUDGET: std::time::Duration = std::time::Duration::from_millis(200);
        const STEP: std::time::Duration = std::time::Duration::from_millis(25);

        let deadline = std::time::Instant::now() + BUDGET;
        loop {
            if self.is_pane_dead() {
                let tail = self.capture_pane(20).unwrap_or_default();
                bail!(
                    "Tool session '{}' pane died before becoming ready:\n{}",
                    self.name,
                    tail
                );
            }
            if std::time::Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(STEP);
        }
    }

    pub fn attach(&self) -> Result<()> {
        super::Session::from_name(&self.name).attach()
    }

    pub fn capture_pane(&self, lines: usize) -> Result<String> {
        super::Session::from_name(&self.name).capture_pane(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::TmuxTestSession;
    use super::*;

    /// A session id long enough that `truncate_id(.., 8)` truncates.
    const ID: &str = "abc12345deadbeef";

    fn legacy_metadata() -> crate::tmux::PaneMetadata {
        crate::tmux::PaneMetadata {
            session_id: "$42".into(),
            pane_id: "%42".into(),
            session_kind: None,
            tool_owner: crate::tmux::ToolPaneOwner::Unmarked,
            pane_dead: false,
            pane_current_command: Some("sleep".into()),
            pane_start_command_is_protected: false,
            pane_pid: Some(4242),
            pane_title: None,
            window_activity: None,
            window_size: Some((80, 24)),
        }
    }

    fn legacy_scope(id: &str, tool: &str) -> LegacyToolScope {
        LegacyToolScope {
            rows: vec![(
                "work".into(),
                id.into(),
                crate::tmux::Session::generate_name(id, "Row"),
            )],
            tools: vec![(tool.into(), ToolSession::name_prefix(tool))],
        }
    }

    #[test]
    fn legacy_candidates_are_explicit_and_reject_ambiguous_or_invalid_ownership() {
        let name = ToolSession::generate_name(ID, "Old title", "yazi");
        let scope = legacy_scope(ID, "yazi");
        let panes = std::collections::HashMap::from([(name.clone(), legacy_metadata())]);
        assert!(ToolSession::from_snapshot(ID, "New title", "yazi", &panes).is_err());
        assert_eq!(
            ToolSession::legacy_metadata_in(ID, "New title", "yazi", "work", &panes, &scope)
                .unwrap()
                .unwrap()
                .0,
            name
        );
        for owner in [
            crate::tmux::ToolPaneOwner::Invalid,
            crate::tmux::ToolPaneOwner::Named {
                instance_id: ID.into(),
                tool_name: "other".into(),
            },
            crate::tmux::ToolPaneOwner::Named {
                instance_id: "abc12345different".into(),
                tool_name: "yazi".into(),
            },
        ] {
            let mut bad = panes.clone();
            bad.get_mut(&name).unwrap().tool_owner = owner;
            assert!(matches!(
                ToolSession::legacy_metadata_in(ID, "Old title", "yazi", "work", &bad, &scope),
                Err(_) | Ok(None)
            ));
        }
        let mut duplicate = panes.clone();
        duplicate.insert(
            ToolSession::generate_name(ID, "Other title", "yazi"),
            legacy_metadata(),
        );
        assert!(ToolSession::legacy_metadata_in(
            ID,
            "New title",
            "yazi",
            "work",
            &duplicate,
            &scope
        )
        .is_err());
        for kind in ["agent", "term", "cterm", "broken"] {
            let mut bad = panes.clone();
            bad.get_mut(&name).unwrap().session_kind = Some(kind.into());
            assert!(
                ToolSession::legacy_metadata_in(ID, "New title", "yazi", "work", &bad, &scope)
                    .is_err()
            );
        }
        let mut no_ids = panes.clone();
        no_ids.get_mut(&name).unwrap().pane_id.clear();
        assert!(
            ToolSession::legacy_metadata_in(ID, "New title", "yazi", "work", &no_ids, &scope)
                .is_err()
        );
        let mut no_pid = panes;
        no_pid.get_mut(&name).unwrap().pane_pid = None;
        assert!(
            ToolSession::legacy_metadata_in(ID, "New title", "yazi", "work", &no_pid, &scope)
                .is_err()
        );
    }

    #[test]
    fn legacy_candidates_check_all_profile_ids_and_raw_tool_name_shapes() {
        for (requested, raw_names, title) in [
            ("git|log", vec!["git|log", "git_log"], "T"),
            ("git", vec!["git", "git_log"], "log_T"),
        ] {
            let name = ToolSession::generate_name(ID, title, requested);
            let panes = std::collections::HashMap::from([(name, legacy_metadata())]);
            let mut scope = legacy_scope(ID, requested);
            scope.tools = raw_names
                .into_iter()
                .map(|name| (name.to_owned(), ToolSession::name_prefix(name)))
                .collect();
            assert!(
                ToolSession::legacy_metadata_in(ID, title, requested, "work", &panes, &scope)
                    .is_err()
            );
        }
        let name = ToolSession::generate_name(ID, "Old title", "yazi");
        let panes = std::collections::HashMap::from([(name.clone(), legacy_metadata())]);
        for other_id in ["abc12345different", ID] {
            let mut scope = legacy_scope(ID, "yazi");
            scope.rows.push((
                "other-profile".into(),
                other_id.into(),
                crate::tmux::Session::generate_name(other_id, "Other"),
            ));
            assert!(ToolSession::legacy_metadata_in(
                ID,
                "New title",
                "yazi",
                "work",
                &panes,
                &scope
            )
            .is_err());
        }
        let mut scope = legacy_scope(ID, "yazi");
        scope.rows[0].2 = name;
        assert!(
            ToolSession::legacy_metadata_in(ID, "New title", "yazi", "work", &panes, &scope)
                .is_err()
        );
    }

    #[test]
    #[serial_test::serial]
    fn legacy_adoption_and_verified_stop_fence_replacement_owner_and_pane_races() {
        use crate::tmux::test_helpers::{pane_field, wait_for_pane_command};
        if !tmux_available() {
            return;
        }
        let unique = TmuxTestSession::new("legacy_cas");
        let id = format!("{:08x}legacy", std::process::id());
        let name = ToolSession::generate_name(&id, unique.name(), "probe");
        let _guard = TmuxTestSession::from_name(&name);
        let agent_name = crate::tmux::Session::generate_name(&id, "Agent");
        let _agent = TmuxTestSession::from_name(&agent_name);
        assert!(crate::tmux::tmux_command()
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                &agent_name,
                "sleep 600"
            ])
            .status()
            .unwrap()
            .success());
        let scope = legacy_scope(&id, "probe");
        for race in [
            "none",
            "owner",
            "kind",
            "rename",
            "respawn",
            "replacement",
            "after_stamp",
        ] {
            assert!(crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", &name, "sleep 600"])
                .status()
                .unwrap()
                .success());
            let panes = crate::tmux::batch_pane_metadata().unwrap();
            let metadata = panes.get(&name).unwrap();
            let adoption = crate::session::LegacyToolAdoption {
                tmux_session: name.clone(),
                identity: ToolSession::legacy_identity(metadata).unwrap(),
                profile: "work".into(),
                lifecycle_generation: 0,
            };
            let pane = adoption.identity.pane_id.as_str();
            match race {
                "owner" => {
                    let other = serde_json::to_string(&("other-full-id", "other")).unwrap();
                    assert!(crate::tmux::tmux_command()
                        .args([
                            "set-option",
                            "-t",
                            &adoption.identity.session_id,
                            "@aoe_tool_owner",
                            &other
                        ])
                        .status()
                        .unwrap()
                        .success());
                }
                "kind" => {
                    assert!(crate::tmux::tmux_command()
                        .args([
                            "set-option",
                            "-t",
                            &adoption.identity.session_id,
                            "@aoe_kind",
                            "agent"
                        ])
                        .status()
                        .unwrap()
                        .success());
                }
                "rename" => {
                    let renamed = format!("{name}_renamed");
                    let _rename_guard = TmuxTestSession::from_name(&renamed);
                    assert!(crate::tmux::tmux_command()
                        .args([
                            "rename-session",
                            "-t",
                            &adoption.identity.session_id,
                            &renamed
                        ])
                        .status()
                        .unwrap()
                        .success());
                    assert!(ToolSession::adopt_legacy_snapshot(
                        &id,
                        unique.name(),
                        "probe",
                        "work",
                        &adoption,
                        &panes,
                        &scope
                    )
                    .is_err());
                    assert_eq!(pane_field(pane, "#{@aoe_tool_owner}"), "");
                    assert!(crate::tmux::tmux_command()
                        .args(["rename-session", "-t", &adoption.identity.session_id, &name])
                        .status()
                        .unwrap()
                        .success());
                }
                "respawn" => {
                    assert!(crate::tmux::tmux_command()
                        .args(["respawn-pane", "-k", "-t", pane, "sleep 600"])
                        .status()
                        .unwrap()
                        .success());
                    wait_for_pane_command(pane, "sleep");
                    assert_ne!(
                        pane_field(pane, "#{pane_pid}"),
                        adoption.identity.pane_pid.to_string()
                    );
                }
                "replacement" => {
                    assert!(crate::tmux::tmux_command()
                        .args(["kill-session", "-t", &adoption.identity.session_id])
                        .status()
                        .unwrap()
                        .success());
                    assert!(crate::tmux::tmux_command()
                        .args(["new-session", "-d", "-s", &name, "sleep 600"])
                        .status()
                        .unwrap()
                        .success());
                }
                _ => {}
            }
            if race == "rename" {
                // The restored name is now the same identity; only the stale renamed attempt above was refused.
            } else if matches!(race, "none" | "after_stamp") {
                let (tool, verified) = ToolSession::adopt_legacy_snapshot(
                    &id,
                    unique.name(),
                    "probe",
                    "work",
                    &adoption,
                    &panes,
                    &scope,
                )
                .unwrap();
                let stamped = serde_json::to_string(&(&id, "probe")).unwrap();
                assert_eq!(pane_field(pane, "#{@aoe_tool_owner}"), stamped);
                assert!(
                    ToolSession::adopt_legacy_snapshot(
                        &id,
                        unique.name(),
                        "probe",
                        "work",
                        &adoption,
                        &panes,
                        &scope
                    )
                    .is_err(),
                    "another stale unmarked claimant must not restamp"
                );
                if race == "after_stamp" {
                    assert!(crate::tmux::tmux_command()
                        .args(["kill-session", "-t", &adoption.identity.session_id])
                        .status()
                        .unwrap()
                        .success());
                    assert!(crate::tmux::tmux_command()
                        .args(["new-session", "-d", "-s", &name, "sleep 600"])
                        .status()
                        .unwrap()
                        .success());
                    assert!(tool.kill_verified(&id, "probe", &verified).is_err());
                    assert_eq!(pane_field(&name, "#{@aoe_tool_owner}"), "");
                } else {
                    tool.kill_verified(&id, "probe", &verified).unwrap();
                }
            } else {
                assert!(
                    ToolSession::adopt_legacy_snapshot(
                        &id,
                        unique.name(),
                        "probe",
                        "work",
                        &adoption,
                        &panes,
                        &scope
                    )
                    .is_err(),
                    "{race}"
                );
                if race != "owner" {
                    assert_eq!(pane_field(&name, "#{@aoe_tool_owner}"), "");
                }
            }
            assert!(
                crate::tmux::tmux_command()
                    .args(["has-session", "-t", &agent_name])
                    .status()
                    .unwrap()
                    .success(),
                "agent survives {race}"
            );
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &name])
                .output();
        }
    }

    #[test]
    fn snapshot_resolution_rejects_other_tool_ownership_but_keeps_retitles() {
        let sibling = ToolSession::generate_name(ID, "T", "git_log");
        let mut panes = std::collections::HashMap::from([(
            sibling.clone(),
            crate::tmux::PaneMetadata {
                session_id: "$42".into(),
                pane_id: "%42".into(),
                session_kind: None,
                tool_owner: crate::tmux::ToolPaneOwner::Unmarked,
                pane_dead: false,
                pane_current_command: None,
                pane_start_command_is_protected: false,
                pane_pid: None,
                pane_title: None,
                window_activity: None,
                window_size: None,
            },
        )]);
        assert!(ToolSession::from_snapshot(ID, "T", "git", &panes).is_err());
        assert!(ToolSession::metadata_in(ID, "T", "git_log", &panes).is_err());
        panes.get_mut(&sibling).unwrap().tool_owner = crate::tmux::ToolPaneOwner::Named {
            instance_id: ID.into(),
            tool_name: "git_log".into(),
        };
        assert!(ToolSession::metadata_in(ID, "T", "git", &panes)
            .unwrap()
            .is_none());
        assert!(ToolSession::from_snapshot(ID, "log_T", "git", &panes).is_err());
        assert!(ToolSession::from_snapshot("abc12345different", "T", "git_log", &panes).is_err());
        assert_eq!(
            ToolSession::from_snapshot(ID, "T", "git_log", &panes)
                .unwrap()
                .session_name(),
            sibling
        );
        let mut metadata = panes.remove(&sibling).unwrap();
        metadata.tool_owner = crate::tmux::ToolPaneOwner::Named {
            instance_id: ID.into(),
            tool_name: "yazi".into(),
        };
        let old_name = ToolSession::generate_name(ID, "Old title", "yazi");
        panes.insert(old_name.clone(), metadata);
        assert_eq!(
            ToolSession::from_snapshot(ID, "New title", "yazi", &panes)
                .unwrap()
                .session_name(),
            old_name
        );
    }

    #[test]
    #[serial_test::serial]
    fn new_resolves_retitled_transport_without_assigning_ownership() {
        // #3157 for tool sub-sessions: the title moved, the tool's tmux session
        // kept the name it was created under. This transport-only constructor
        // locates its existing name; ownership authorization belongs to the
        // verified snapshot or explicit legacy-adoption path.
        let guard = crate::tmux::SessionCacheGuard::capture();
        let stale_lazygit = ToolSession::generate_name(ID, "Vikings", "lazygit");
        guard.force_present(&[stale_lazygit.as_str()]);

        assert_eq!(
            ToolSession::new(ID, "Refactor billing", "lazygit").session_name(),
            stale_lazygit
        );
        // yazi was never opened, so it keeps the name it will be spawned under.
        let yazi = ToolSession::new(ID, "Refactor billing", "yazi")
            .session_name()
            .to_string();
        assert!(
            yazi.starts_with(&format!("{TOOL_PREFIX}yazi_")),
            "yazi must not resolve lazygit's transport name: {yazi}"
        );
        assert!(yazi.contains("Refactor_billing"));
    }

    #[test]
    #[serial_test::serial]
    fn new_keeps_the_derived_name_when_an_extension_named_tool_is_ambiguous() {
        // The tool/title boundary is not recoverable from the name: tool `git`
        // with title `log_x` and tool `git_log` with title `x` collide. Guard
        // the reachable half of that: when both panes are live, resolution must
        // see two candidates and keep the derived name rather than pick one.
        let guard = crate::tmux::SessionCacheGuard::capture();
        let git = ToolSession::generate_name(ID, "Vikings", "git");
        let git_log = ToolSession::generate_name(ID, "Vikings", "git_log");
        assert!(
            git_log.starts_with(&ToolSession::name_prefix("git")),
            "the collision this guards only exists because `git_log` matches \
             `git`'s prefix: {git_log}"
        );
        guard.force_present(&[git.as_str(), git_log.as_str()]);

        let derived = ToolSession::generate_name(ID, "Refactor billing", "git");
        assert_eq!(
            ToolSession::new(ID, "Refactor billing", "git").session_name(),
            derived,
            "two candidates are ambiguous, so neither pane is adopted"
        );
    }

    /// Helper: check if tmux is available for tests that need it
    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    #[serial_test::serial]
    fn wait_until_ready_errs_with_pane_tail_when_pane_dies_immediately() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let guard = TmuxTestSession::new("aoe_test_tool_dead");
        let tool = ToolSession {
            name: guard.name().to_string(),
        };
        tool.create_with_size(
            dir.path().to_str().expect("utf8 path"),
            "sh -c 'echo boom; exit 1'",
            Some((80, 24)),
            "default",
            ID,
            "dead",
        )
        .expect("create_with_size");

        let pane_id = crate::tmux::test_helpers::only_pane_id(tool.session_name());
        crate::tmux::test_helpers::wait_for_pane_dead(&pane_id);
        let result = tool.wait_until_ready();

        assert!(
            result.is_err(),
            "wait_until_ready should error when the pane dies before the budget expires"
        );
        let message = result.unwrap_err().to_string();
        assert!(
            message.contains("boom"),
            "error should include the captured pane tail, got: {message:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn creation_owns_the_tool_not_the_inherited_tmux_context() {
        if !tmux_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ui = TmuxTestSession::new("aoe_test_tool_context");
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                ui.name(),
                "-P",
                "-F",
                "#{socket_path},#{pid},#{session_id}\t#{pane_id}",
                "sleep 30",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context = String::from_utf8(output.stdout).unwrap();
        let (context, pane) = context.trim().split_once('\t').unwrap();
        let context = context.replace(",$", ",");
        let guard =
            TmuxTestSession::from_name(ToolSession::generate_name(ID, ui.name(), "context"));
        let tool = ToolSession {
            name: guard.name().into(),
        };
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("TMUX", context.as_str()),
            ("TMUX_PANE", pane),
        ]);
        tool.create_with_size(
            dir.path().to_str().unwrap(),
            "sleep 30",
            Some((80, 24)),
            "default",
            ID,
            "context",
        )
        .unwrap();
        let panes = crate::tmux::batch_pane_metadata().unwrap();
        let resolved = ToolSession::from_snapshot(ID, "retitled", "context", &panes).unwrap();
        assert_eq!(resolved.session_name(), tool.session_name());
        let ui_owner = crate::tmux::tmux_command()
            .args([
                "show-options",
                "-qv",
                "-t",
                &format!("={}:", ui.name()),
                "@aoe_tool_owner",
            ])
            .output()
            .unwrap();
        assert!(ui_owner.status.success());
        assert!(
            ui_owner.stdout.is_empty(),
            "creation changed the inherited session owner"
        );
    }

    #[test]
    fn new_name_includes_prefix_tool_title_and_truncated_id() {
        let s = ToolSession::new("0123456789abcdef", "my-session", "lazygit");
        let name = s.session_name();
        assert!(name.starts_with(TOOL_PREFIX), "name was {}", name);
        assert!(name.contains("lazygit"));
        assert!(name.contains("my-session"));
        assert!(name.ends_with("_01234567"), "name was {}", name);
    }

    #[test]
    fn distinct_sessions_and_tools_get_distinct_names() {
        let names: Vec<String> = [
            ("0123456789abcdef", "lazygit"),
            ("0123456789abcdef", "yazi"),
            ("aaaaaaaa1111", "lazygit"),
            ("bbbbbbbb2222", "lazygit"),
        ]
        .iter()
        .map(|(id, tool)| ToolSession::new(id, "x", tool).session_name().to_string())
        .collect();
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "{names:?}");
    }
}
