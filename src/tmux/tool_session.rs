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

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ToolStopReceipt {
    pub name: String,
    pub identity: crate::session::LegacyToolIdentity,
    pub owner: String,
    pub tree: crate::process::CapturedProcessTreeReceipt,
}

#[cfg(test)]
thread_local! {
    static DROP_RETIREMENT_ACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_SERVER_BIRTH_PROBE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum RetirementCheckpoint {
    Prepared,
    Proven,
}

#[cfg(test)]
static RETIREMENT_INTERRUPTION: std::sync::Mutex<
    Option<(uuid::Uuid, String, RetirementCheckpoint)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
struct RetirementInterruption(uuid::Uuid);

#[cfg(test)]
impl Drop for RetirementInterruption {
    fn drop(&mut self) {
        let mut pending = RETIREMENT_INTERRUPTION
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending
            .as_ref()
            .is_some_and(|(token, _, _)| token == &self.0)
        {
            *pending = None;
        }
    }
}

#[cfg(test)]
fn retirement_checkpoint(id: &str, checkpoint: RetirementCheckpoint) -> Result<()> {
    let interrupted = {
        let mut pending = RETIREMENT_INTERRUPTION
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let matches = pending
            .as_ref()
            .is_some_and(|(_, owner, point)| owner == id && *point == checkpoint);
        if matches {
            *pending = None;
        }
        matches
    };
    if interrupted {
        bail!(match checkpoint {
            RetirementCheckpoint::Prepared =>
                "injected interruption after durable tool retirement preparation",
            RetirementCheckpoint::Proven =>
                "injected interruption after original retirement proof before acknowledgement",
        });
    }
    Ok(())
}

impl ToolSession {
    #[cfg(test)]
    pub(crate) fn interrupt_retirement_after_prepare_for_test(id: &str) -> impl Drop {
        Self::interrupt_retirement_for_test(id, RetirementCheckpoint::Prepared)
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn interrupt_retirement_before_ack_for_test(id: &str) -> impl Drop {
        Self::interrupt_retirement_for_test(id, RetirementCheckpoint::Proven)
    }

    #[cfg(test)]
    fn interrupt_retirement_for_test(
        id: &str,
        checkpoint: RetirementCheckpoint,
    ) -> RetirementInterruption {
        let mut pending = RETIREMENT_INTERRUPTION
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(
            pending.is_none(),
            "another retirement interruption fixture is active"
        );
        let token = uuid::Uuid::new_v4();
        *pending = Some((token, id.to_owned(), checkpoint));
        RetirementInterruption(token)
    }

    pub(crate) fn pending_retirement_targets() -> Result<Vec<(String, String)>> {
        Ok(super::tool_retirement::LockedJournal::open()?.targets())
    }

    pub(crate) fn settle_pending_retirements_for(
        id: &str,
    ) -> Result<(std::collections::HashSet<String>, Vec<anyhow::Error>)> {
        let mut journal = super::tool_retirement::LockedJournal::open()?;
        let targets = journal.retirements_for_instance(id);
        let mut protected = std::collections::HashSet::with_capacity(targets.len());
        let mut errors = Vec::new();
        for (token, tool_name) in targets {
            protected.insert(tool_name);
            if let Err(error) = Self::finish_retirement(&mut journal, token, None) {
                errors.push(error);
            }
        }
        Ok((protected, errors))
    }

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

    fn valid_pane_ids(session_id: &str, pane_id: &str) -> bool {
        [(session_id, '$'), (pane_id, '%')]
            .into_iter()
            .all(|(id, prefix)| {
                id.strip_prefix(prefix).is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|ch| ch.is_ascii_digit())
                })
            })
    }

    pub(super) fn valid_identity(identity: &crate::session::LegacyToolIdentity) -> bool {
        identity
            .pane_pid
            .is_some_and(|pid| pid > 0 && i32::try_from(pid).is_ok())
            && Self::valid_pane_ids(&identity.session_id, &identity.pane_id)
    }

    fn validate_identity(metadata: &crate::tmux::PaneMetadata) -> Result<Option<u32>> {
        anyhow::ensure!(
            Self::valid_pane_ids(&metadata.session_id, &metadata.pane_id),
            LegacyToolUnavailable("immutable tmux IDs unavailable")
        );
        if metadata.pane_dead {
            return Ok(None);
        }
        let pane_pid = metadata
            .pane_pid
            .filter(|pid| *pid > 0 && i32::try_from(*pid).is_ok())
            .ok_or(LegacyToolUnavailable("pane process identity unavailable"))?;
        Ok(Some(pane_pid))
    }

    pub(crate) fn server_identity() -> Result<crate::session::TmuxServerIdentity> {
        let (socket, pid) =
            super::configured_tmux_server_identity(&super::TmuxCommandDeadline::new())?
                .ok_or(LegacyToolUnavailable("original tmux server is absent"))?;
        let birth = crate::process::process_birth_identity(pid)?.ok_or(LegacyToolUnavailable(
            "original tmux server birth is unavailable",
        ))?;
        anyhow::ensure!(
            crate::process::process_birth_is_live(pid, &birth)?,
            LegacyToolUnavailable("original tmux server is not live")
        );
        Ok(crate::session::TmuxServerIdentity { socket, pid, birth })
    }

    fn validate_server(server: &crate::session::TmuxServerIdentity) -> Result<()> {
        anyhow::ensure!(
            super::configured_tmux_server_identity(&super::TmuxCommandDeadline::new())?
                .is_some_and(|(socket, pid)| socket == server.socket && pid == server.pid)
                && crate::process::process_birth_is_live(server.pid, &server.birth)?,
            LegacyToolUnavailable("captured tmux server was replaced")
        );
        Ok(())
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

    fn pane_condition(
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
        format!("#{{&&:#{{&&:#{{==:#{{session_id}},{session_id}}},#{{==:#{{pane_id}},{pane_id}}}}},#{{&&:#{{==:#{{session_name}},{name}}},#{{&&:#{{==:#{{@aoe_tool_owner}},{owner}}},{kind}}}}}}}")
    }

    fn snapshot_condition(
        name: &str,
        identity: &crate::session::LegacyToolIdentity,
        owner: &str,
    ) -> String {
        let pane = Self::pane_condition(name, identity, owner);
        let leader = match identity.pane_pid {
            Some(pid) => format!("#{{==:#{{pane_pid}},{pid}}}"),
            None => "#{==:#{pane_dead},1}".to_owned(),
        };
        format!("#{{&&:{pane},{leader}}}")
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
        Self::validate_server(&adoption.server)?;
        let (name, metadata) =
            Self::legacy_metadata_in(id, title, tool_name, profile, panes, scope)?
                .ok_or(LegacyToolUnavailable("captured legacy pane is absent"))?;
        anyhow::ensure!(
            name == adoption.tmux_session
                && Self::legacy_identity(metadata)? == adoption.identity
                && metadata.server.as_ref() == Some(&adoption.server),
            LegacyToolUnavailable("captured pane was replaced")
        );
        let owner = serde_json::to_string(&(id, tool_name))?;
        let condition = Self::snapshot_condition(name, &adoption.identity, "");
        let pid = adoption.server.pid;
        let socket = super::Session::tmux_format_literal(&adoption.server.socket);
        let condition = format!(
            "#{{&&:{condition},#{{&&:#{{==:#{{pid}},{pid}}},#{{==:#{{socket_path}},{socket}}}}}}}"
        );
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

    /// Resolve debt before looking up today's pane. `true` means this stop consumed
    /// its original operation; callers must not then stop a same-name replacement.
    pub(crate) fn settle_pending_retirement(
        id: &str,
        tool_name: &str,
        adoption: Option<&crate::session::LegacyToolAdoption>,
    ) -> Result<bool> {
        let mut journal = super::tool_retirement::LockedJournal::open()?;
        let Some(pending) = journal.pending(id, tool_name) else {
            return Ok(false);
        };
        if let Some(adoption) = adoption {
            anyhow::ensure!(
                (adoption.tmux_session == pending.receipt.name
                    && adoption.identity == pending.receipt.identity)
                    || Self::retirement_boot_changed(pending)?,
                LegacyToolUnavailable("pending retirement belongs to another captured tool")
            );
        }
        let token = pending.token;
        Self::finish_retirement(&mut journal, token, None)?;
        Ok(true)
    }

    pub(crate) fn has_pending_retirement(id: &str, tool_name: &str) -> Result<bool> {
        Ok(super::tool_retirement::LockedJournal::open()?
            .pending(id, tool_name)
            .is_some())
    }

    fn retirement_boot_changed(pending: &super::tool_retirement::Retirement) -> Result<bool> {
        let boot = crate::process::boot_id()
            .filter(|boot| !boot.is_empty())
            .ok_or_else(|| anyhow::anyhow!("process boot identity unavailable"))?;
        Ok(boot != pending.server_birth.boot_id)
    }

    fn original_server_alive(pending: &super::tool_retirement::Retirement) -> Result<bool> {
        // A reboot proves every original birth obsolete before a recycled PID
        // can refer to an inaccessible foreign process on this boot.
        if Self::retirement_boot_changed(pending)? {
            return Ok(false);
        }
        #[cfg(test)]
        if FAIL_SERVER_BIRTH_PROBE.replace(false) {
            bail!("injected uninspectable recorded server PID");
        }
        crate::process::process_birth_is_live(pending.server_pid, &pending.server_birth)
    }

    fn original_session_present(pending: &super::tool_retirement::Retirement) -> Result<bool> {
        Self::session_present_at(
            &pending.socket,
            pending.server_pid,
            &pending.receipt.identity.session_id,
            || Self::original_server_alive(pending),
        )
    }

    fn session_present_at(
        socket: &str,
        server_pid: u32,
        session_id: &str,
        server_alive: impl Fn() -> Result<bool>,
    ) -> Result<bool> {
        if !server_alive()? {
            return Ok(false);
        }
        let mut command = super::tmux_query_command_for_socket(socket);
        command.args([
            "display-message",
            "-p",
            "#{pid}\t#{socket_path}\t-",
            ";",
            "list-sessions",
            "-F",
            "#{pid}\t#{socket_path}\t#{session_id}",
        ]);
        let output = super::TmuxCommandDeadline::new().run(&mut command)?;
        if !server_alive()? {
            return Ok(false);
        }
        anyhow::ensure!(
            output.status.success(),
            LegacyToolUnavailable("original tmux server census is unavailable")
        );
        let text = String::from_utf8(output.stdout)?;
        let mut present = false;
        let mut server_attested = false;
        for line in text.lines() {
            let mut fields = line.split('\t');
            let pid = fields.next().and_then(|pid| pid.parse::<u32>().ok());
            let observed_socket = fields.next();
            let session = fields
                .next()
                .ok_or(LegacyToolUnavailable("original tmux census is malformed"))?;
            anyhow::ensure!(
                pid == Some(server_pid)
                    && observed_socket == Some(socket)
                    && (session == "-"
                        || session.strip_prefix('$').is_some_and(
                            |id| !id.is_empty() && id.bytes().all(|ch| ch.is_ascii_digit())
                        ))
                    && fields.next().is_none(),
                LegacyToolUnavailable("original tmux server identity changed")
            );
            server_attested |= session == "-";
            present |= session == session_id;
        }
        anyhow::ensure!(
            server_attested,
            LegacyToolUnavailable("original tmux census lacks server attestation")
        );
        Ok(present)
    }

    fn finish_retirement(
        journal: &mut super::tool_retirement::LockedJournal,
        token: uuid::Uuid,
        captured: Option<crate::process::CapturedProcessTree>,
    ) -> Result<()> {
        // A failed publication fsync may leave a visible record. Re-publish and
        // sync it before permitting any effect on recovery.
        journal.sync()?;
        let pending = journal
            .pending_by_token(token)
            .ok_or(LegacyToolUnavailable(
                "selected tool retirement disappeared",
            ))?;
        if Self::original_session_present(pending)? {
            let receipt = &pending.receipt;
            let root_current = receipt.tree.root_is_current();
            let identity = &receipt.identity;
            let live = Self::snapshot_condition(&receipt.name, identity, &receipt.owner);
            let current = if matches!(&root_current, Ok(true)) {
                "1"
            } else {
                "0"
            };
            let live = format!("#{{&&:#{{&&:{live},#{{==:#{{pane_dead}},0}}}},{current}}}");
            let pane = Self::pane_condition(&receipt.name, identity, &receipt.owner);
            let dead = format!("#{{&&:{pane},#{{==:#{{pane_dead}},1}}}}");
            let condition = format!("#{{||:{live},{dead}}}");
            let token = super::Session::tmux_format_literal(&pending.token.to_string());
            let socket = super::Session::tmux_format_literal(&pending.socket);
            let condition = format!(
                concat!(
                    "#{{&&:{condition},",
                    "#{{&&:#{{==:#{{@aoe_tool_retirement}},{token}}},",
                    "#{{&&:#{{==:#{{pid}},{server_pid}}},",
                    "#{{==:#{{socket_path}},{socket}}}",
                    "}}",
                    "}}",
                    "}}"
                ),
                condition = condition,
                token = token,
                server_pid = pending.server_pid,
                socket = socket,
            );
            let target = super::Session::tmux_command_string_literal(&identity.session_id);
            let effect = format!("kill-session -t {target} ; display-message -p aoe-tool-stopped");
            let mut command = super::tmux_query_command_for_socket(&pending.socket);
            command.args([
                "if-shell",
                "-t",
                &identity.pane_id,
                "-F",
                &condition,
                &effect,
            ]);
            let result = super::TmuxCommandDeadline::new().run(&mut command);
            let acknowledged = result.as_ref().is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .any(|line| line.trim() == "aoe-tool-stopped")
            });
            #[cfg(test)]
            let acknowledged = !DROP_RETIREMENT_ACK.replace(false) && acknowledged;
            if !acknowledged && Self::original_session_present(pending)? {
                root_current?;
                return Err(LegacyToolUnavailable(
                    "captured identity or owner changed before tmux effect",
                )
                .into());
            }
        }
        // Settle only the original birth list, including when tmux hides a dead leader PID.
        let tree = match captured {
            Some(tree) => tree,
            None => crate::process::CapturedProcessTree::resume(
                pending.receipt.tree.clone(),
                pending
                    .receipt
                    .identity
                    .pane_pid
                    .ok_or(LegacyToolUnavailable("original tool PID unavailable"))?,
            )?,
        };
        tree.settle(std::time::Duration::from_millis(200))?;
        #[cfg(test)]
        retirement_checkpoint(&pending.instance_id, RetirementCheckpoint::Proven)?;
        journal.acknowledge(token)?;
        refresh_session_cache();
        Ok(())
    }

    fn kill_dead_pane_verified(
        &self,
        identity: &crate::session::LegacyToolIdentity,
        owner: &str,
        server: &crate::session::TmuxServerIdentity,
    ) -> Result<()> {
        Self::validate_server(server)?;
        let socket = server.socket.as_str();
        let server_pid = server.pid;
        let birth = &server.birth;
        let condition = Self::snapshot_condition(&self.name, identity, owner);
        let socket_literal = super::Session::tmux_format_literal(socket);
        let condition = format!(
            concat!(
                "#{{&&:{condition},",
                "#{{&&:#{{==:#{{@aoe_tool_stop}},}},",
                "#{{&&:#{{==:#{{@aoe_tool_retirement}},}},",
                "#{{&&:#{{==:#{{pid}},{server_pid}}},",
                "#{{==:#{{socket_path}},{socket}}}",
                "}}",
                "}}",
                "}}",
                "}}"
            ),
            condition = condition,
            server_pid = server_pid,
            socket = socket_literal,
        );
        let target = super::Session::tmux_command_string_literal(&identity.session_id);
        let effect = format!("kill-session -t {target} ; display-message -p aoe-tool-stopped");
        let mut command = super::tmux_query_command_for_socket(socket);
        command.args([
            "if-shell",
            "-t",
            &identity.pane_id,
            "-F",
            &condition,
            &effect,
        ]);
        let result = super::TmuxCommandDeadline::new().run(&mut command);
        let acknowledged = result.as_ref().is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.trim() == "aoe-tool-stopped")
        });
        anyhow::ensure!(
            acknowledged
                || !Self::session_present_at(socket, server_pid, &identity.session_id, || {
                    crate::process::process_birth_is_live(server_pid, birth)
                },)?,
            LegacyToolUnavailable("dead tool identity or owner changed before tmux effect")
        );
        refresh_session_cache();
        Ok(())
    }

    pub(crate) fn kill_verified(
        &self,
        id: &str,
        tool_name: &str,
        identity: &crate::session::LegacyToolIdentity,
        expected_server: Option<&crate::session::TmuxServerIdentity>,
    ) -> Result<()> {
        let mut journal = super::tool_retirement::LockedJournal::open()?;
        if let Some(pending) = journal.pending(id, tool_name) {
            anyhow::ensure!(
                pending.receipt.name == self.name
                    && pending.receipt.identity.session_id == identity.session_id
                    && pending.receipt.identity.pane_id == identity.pane_id
                    && identity
                        .pane_pid
                        .is_none_or(|pid| pending.receipt.identity.pane_pid == Some(pid)),
                LegacyToolUnavailable("pending retirement belongs to another tool incarnation")
            );
            let token = pending.token;
            return Self::finish_retirement(&mut journal, token, None);
        }
        anyhow::ensure!(
            Self::valid_pane_ids(&identity.session_id, &identity.pane_id),
            LegacyToolUnavailable("immutable tmux IDs unavailable")
        );
        if let Some(server) = expected_server {
            Self::validate_server(server)?;
        }
        let owner = serde_json::to_string(&(id, tool_name))?;
        let Some(pane_pid) = identity.pane_pid else {
            let server = expected_server.ok_or(LegacyToolUnavailable(
                "dead pane has no captured tmux server proof",
            ))?;
            return self.kill_dead_pane_verified(identity, &owner, server);
        };
        anyhow::ensure!(
            Self::valid_identity(identity),
            LegacyToolUnavailable("live pane process identity unavailable")
        );
        let condition = Self::snapshot_condition(&self.name, identity, &owner);
        // Refuse historical tmux-only debt before even sampling a current cohort.
        // This read is not authorization: capture must precede the fresh CAS below.
        let capture_condition = format!(
            concat!(
                "#{{&&:{condition},",
                "#{{&&:#{{==:#{{pane_dead}},0}},#{{==:#{{@aoe_tool_stop}},}}",
                "}}",
                "}}"
            ),
            condition = condition,
        );
        Self::run_snapshot_command(
            &identity.pane_id,
            &capture_condition,
            "display-message -p aoe-tool-capture-ready",
            "aoe-tool-capture-ready",
        )?;
        let tree = crate::process::CapturedProcessTree::capture(pane_pid)?;
        let (socket, server_pid) =
            super::configured_tmux_server_identity(&super::TmuxCommandDeadline::new())?
                .ok_or(LegacyToolUnavailable("original tmux server is absent"))?;
        let server_birth = crate::process::process_birth_identity(server_pid)?.ok_or(
            LegacyToolUnavailable("original tmux server birth is unavailable"),
        )?;
        if let Some(expected) = expected_server {
            anyhow::ensure!(
                socket == expected.socket
                    && server_pid == expected.pid
                    && server_birth == expected.birth,
                LegacyToolUnavailable("captured tmux server changed before retirement")
            );
        }
        let token = uuid::Uuid::new_v4();
        let socket_literal = super::Session::tmux_format_literal(&socket);
        // An older tmux-only receipt remains evidence, not an alternative authority.
        // Never overwrite or recapture its original cohort.
        let condition = format!(
            concat!(
                "#{{&&:#{{&&:{condition},#{{==:#{{pane_dead}},0}}}},",
                "#{{&&:#{{==:#{{@aoe_tool_stop}},}},",
                "#{{&&:#{{==:#{{pid}},{server_pid}}},",
                "#{{==:#{{socket_path}},{socket}}}",
                "}}",
                "}}",
                "}}"
            ),
            condition = condition,
            server_pid = server_pid,
            socket = socket_literal,
        );
        let target = super::Session::tmux_command_string_literal(&identity.session_id);
        let effect = format!("set-option -t {target} @aoe_tool_retirement {token} ; display-message -p aoe-tool-verified");
        Self::run_snapshot_command(&identity.pane_id, &condition, &effect, "aoe-tool-verified")?;
        anyhow::ensure!(
            crate::process::process_birth_identity(server_pid)?.as_ref() == Some(&server_birth),
            LegacyToolUnavailable("original tmux server birth changed before authorization")
        );
        let pending = super::tool_retirement::Retirement {
            token,
            instance_id: id.to_owned(),
            tool_name: tool_name.to_owned(),
            socket,
            server_pid,
            server_birth,
            receipt: ToolStopReceipt {
                name: self.name.clone(),
                identity: identity.clone(),
                owner,
                tree: tree.receipt().clone(),
            },
        };
        journal.publish(pending)?;
        #[cfg(test)]
        retirement_checkpoint(id, RetirementCheckpoint::Prepared)?;
        Self::finish_retirement(&mut journal, token, Some(tree))
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
            server: None,
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

    #[cfg(target_os = "linux")]
    #[test]
    #[serial_test::serial]
    fn verified_stop_retires_original_tool_and_hup_ignoring_descendant() {
        fn valid_session_id(id: &str) -> bool {
            id.strip_prefix('$').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
        }

        fn live_session_ids(sentinel_id: &str) -> std::collections::HashSet<String> {
            let mut command = crate::tmux::tmux_query_command();
            command.args(["list-sessions", "-F", "#{session_id}"]);
            let output = crate::tmux::TmuxCommandDeadline::new()
                .run(&mut command)
                .expect("independent live session census must complete");
            assert!(
                output.status.success(),
                "independent live session census failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).expect("session census must be UTF-8");
            let mut ids = std::collections::HashSet::new();
            for id in text.lines() {
                assert!(valid_session_id(id), "malformed live session ID: {id:?}");
                assert!(ids.insert(id.to_owned()), "duplicate live session ID: {id}");
            }
            assert!(
                ids.contains(sentinel_id),
                "private sentinel must remain present; an outage is not an empty census"
            );
            ids
        }

        if !tmux_available() {
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        super::super::tool_retirement::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let sentinel = TmuxTestSession::new("retirement_census");
        let mut command = crate::tmux::tmux_command();
        command.args([
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_id}",
            "-s",
            sentinel.name(),
            "sleep 600",
        ]);
        let output = crate::tmux::TmuxCommandDeadline::new()
            .run(&mut command)
            .expect("private census sentinel creation must complete");
        assert!(
            output.status.success(),
            "private census sentinel creation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut sentinel_id = String::from_utf8(output.stdout).expect("sentinel ID must be UTF-8");
        assert_eq!(sentinel_id.pop(), Some('\n'));
        assert!(valid_session_id(&sentinel_id), "malformed sentinel ID");
        for (fail_first, legacy_replacement) in [(false, false), (true, false), (true, true)] {
            let temp = tempfile::tempdir().unwrap();
            let pid_file = temp.path().join("descendant.pid");
            let fifo = temp.path().join("wait.fifo");
            let start = temp.path().join("start.fifo");
            nix::unistd::mkfifo(
                &start,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .unwrap();
            nix::unistd::mkfifo(
                &fifo,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .unwrap();
            let script = temp.path().join("tool.sh");
            std::fs::write(&script, format!(
            "read -r line < '{}'\ntrap '' HUP TERM\nsetsid sh -c 'trap \"\" HUP TERM; echo $$ > \"$1\"; read -r line < \"$2\"' sh '{}' '{}' &\nwait\n",
            start.display(), pid_file.display(), fifo.display(),
        )).unwrap();
            let unique = TmuxTestSession::new("verified_tree");
            let name = ToolSession::generate_name(ID, unique.name(), "probe");
            let guard = TmuxTestSession::from_name(&name);
            let tool = ToolSession {
                name: guard.name().into(),
            };
            tool.create_with_size(
                temp.path().to_str().unwrap(),
                &format!("sh '{}'", script.display()),
                None,
                "default",
                ID,
                "probe",
            )
            .unwrap();
            let panes = crate::tmux::batch_pane_metadata().unwrap();
            let identity = ToolSession::legacy_identity(panes.get(guard.name()).unwrap()).unwrap();
            struct Cleanup {
                tree: Option<crate::process::CapturedProcessTree>,
                descendant: Option<crate::process::CapturedProcessTree>,
                pid: u32,
                birth: crate::process::ProcessBirthIdentity,
            }
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    if crate::process::process_birth_identity(self.pid)
                        .ok()
                        .flatten()
                        .as_ref()
                        == Some(&self.birth)
                    {
                        if let Ok(tree) = crate::process::CapturedProcessTree::capture(self.pid) {
                            let _ = tree.settle(std::time::Duration::ZERO);
                        }
                    }
                    if let Some(tree) = self.tree.take() {
                        let _ = tree.settle(std::time::Duration::ZERO);
                    }
                    if let Some(tree) = self.descendant.take() {
                        let _ = tree.settle(std::time::Duration::ZERO);
                    }
                }
            }
            let mut cleanup = Cleanup {
                tree: Some(
                    crate::process::CapturedProcessTree::capture(
                        identity.pane_pid.expect("live tool fixture"),
                    )
                    .unwrap(),
                ),
                descendant: None,
                pid: identity.pane_pid.expect("live tool fixture"),
                birth: crate::process::process_birth_identity(
                    identity.pane_pid.expect("live tool fixture"),
                )
                .unwrap()
                .unwrap(),
            };
            std::fs::write(&start, "start\n").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let descendant = loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    .and_then(|text| text.trim().parse::<u32>().map_err(std::io::Error::other))
                {
                    break pid;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "descendant did not enter its barrier"
                );
                std::thread::yield_now();
            };
            cleanup.tree = Some(
                crate::process::CapturedProcessTree::capture(
                    identity.pane_pid.expect("live tool fixture"),
                )
                .unwrap(),
            );
            cleanup.descendant =
                Some(crate::process::CapturedProcessTree::capture(descendant).unwrap());
            let descendant_proof = cleanup.descendant.as_ref().unwrap();
            let mut retired_replacement = None;
            if fail_first {
                crate::process::FAIL_CAPTURED_TREE_AFTER_ROOT.set(true);
                let error = tool
                    .kill_verified(ID, "probe", &identity, None)
                    .unwrap_err();
                assert!(error
                    .to_string()
                    .contains("injected retirement proof failure after root exit"));
                assert!(
                    !descendant_proof.all_exited().unwrap(),
                    "fixture must retain unproven descendant debt"
                );
                assert!(ToolSession::has_pending_retirement(ID, "probe").unwrap());
                assert!(
                    !live_session_ids(&sentinel_id).contains(&identity.session_id),
                    "original tmux target must retire before cohort settlement"
                );
                if legacy_replacement {
                    assert!(crate::tmux::tmux_command()
                        .args(["new-session", "-d", "-s", tool.session_name(), "sleep 600",])
                        .status()
                        .unwrap()
                        .success());
                } else {
                    tool.create_with_size(
                        temp.path().to_str().unwrap(),
                        "sleep 600",
                        None,
                        "default",
                        ID,
                        "probe",
                    )
                    .unwrap();
                }
                let replacement = crate::tmux::batch_pane_metadata().unwrap();
                let replacement_identity =
                    ToolSession::legacy_identity(replacement.get(tool.session_name()).unwrap())
                        .unwrap();
                assert_ne!(replacement_identity, identity);
                let replacement_tree = crate::process::CapturedProcessTree::capture(
                    replacement_identity.pane_pid.expect("live tool fixture"),
                )
                .unwrap();
                assert!(ToolSession::settle_pending_retirement(ID, "probe", None).unwrap());
                assert!(descendant_proof.all_exited().unwrap());
                assert!(
                    !replacement_tree.all_exited().unwrap(),
                    "retry must not signal same-name replacement"
                );
                let live = live_session_ids(&sentinel_id);
                assert!(!live.contains(&identity.session_id));
                assert!(
                    live.contains(&replacement_identity.session_id),
                    "original debt drain must preserve the replacement's immutable tmux target"
                );
                if legacy_replacement {
                    assert_eq!(
                        crate::tmux::test_helpers::pane_field(
                            tool.session_name(),
                            "#{@aoe_tool_owner}"
                        ),
                        ""
                    );
                    assert!(crate::tmux::tmux_command()
                        .args(["kill-session", "-t", tool.session_name(),])
                        .status()
                        .unwrap()
                        .success());
                } else {
                    tool.kill_verified(ID, "probe", &replacement_identity, None)
                        .unwrap();
                }
                retired_replacement = Some(replacement_identity.session_id);
            }
            if !fail_first {
                DROP_RETIREMENT_ACK.set(true);
                tool.kill_verified(ID, "probe", &identity, None).unwrap();
            }
            assert!(cleanup.tree.as_ref().unwrap().all_exited().unwrap());
            assert!(descendant_proof.all_exited().unwrap());
            let live = live_session_ids(&sentinel_id);
            assert!(
                !live.contains(&identity.session_id),
                "original immutable tmux target remains present"
            );
            if let Some(id) = retired_replacement {
                assert!(
                    !live.contains(&id),
                    "explicitly cleaned replacement target remains present"
                );
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn prepared_retirement_refuses_owner_change_and_stale_adoption_without_signals() {
        if !tmux_available() {
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        super::super::tool_retirement::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let unique = TmuxTestSession::new("prepared_retirement");
        let name = ToolSession::generate_name(ID, unique.name(), "probe");
        let _guard = TmuxTestSession::from_name(&name);
        let tool = ToolSession { name };
        tool.create_with_size("/tmp", "sleep 600", None, "default", ID, "probe")
            .unwrap();
        let panes = crate::tmux::batch_pane_metadata().unwrap();
        let identity =
            ToolSession::legacy_identity(panes.get(tool.session_name()).unwrap()).unwrap();
        let proof = crate::process::CapturedProcessTree::capture(
            identity.pane_pid.expect("live tool fixture"),
        )
        .unwrap();
        let _interruption = ToolSession::interrupt_retirement_after_prepare_for_test(ID);
        assert!(tool
            .kill_verified(ID, "probe", &identity, None)
            .unwrap_err()
            .to_string()
            .contains("after durable"));
        let path = crate::session::get_app_dir()
            .unwrap()
            .join(super::super::tool_retirement::FILE_NAME);
        let evidence = std::fs::read(&path).unwrap();
        let foreign = serde_json::to_string(&("other", "probe")).unwrap();
        assert!(crate::tmux::tmux_command()
            .args([
                "set-option",
                "-t",
                &identity.session_id,
                "@aoe_tool_owner",
                &foreign,
            ])
            .status()
            .unwrap()
            .success());
        assert!(tool.kill_verified(ID, "probe", &identity, None).is_err());
        assert!(!proof.all_exited().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), evidence);
        let mut stale = crate::session::LegacyToolAdoption {
            tmux_session: tool.name.clone(),
            identity: identity.clone(),
            server: std::sync::Arc::new(ToolSession::server_identity().unwrap()),
            profile: "default".into(),
            lifecycle_generation: 0,
        };
        stale.identity.pane_id = "%999999".into();
        assert!(ToolSession::settle_pending_retirement(ID, "probe", Some(&stale)).is_err());
        assert!(!proof.all_exited().unwrap());
        let owner = serde_json::to_string(&(ID, "probe")).unwrap();
        assert!(crate::tmux::tmux_command()
            .args([
                "set-option",
                "-t",
                &identity.session_id,
                "@aoe_tool_owner",
                &owner,
            ])
            .status()
            .unwrap()
            .success());
        assert!(ToolSession::settle_pending_retirement(ID, "probe", None).unwrap());
        assert!(proof.all_exited().unwrap());
        assert!(!ToolSession::has_pending_retirement(ID, "probe").unwrap());
        assert!(!ToolSession::settle_pending_retirement(ID, "probe", None).unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn prepared_retirement_requires_socket_server_birth_and_operation_token() {
        if !tmux_available() {
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        super::super::tool_retirement::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let unique = TmuxTestSession::new("server_fence");
        let name = ToolSession::generate_name(ID, unique.name(), "probe");
        let _guard = TmuxTestSession::from_name(&name);
        let tool = ToolSession { name };
        tool.create_with_size("/tmp", "sleep 600", None, "default", ID, "probe")
            .unwrap();
        let panes = crate::tmux::batch_pane_metadata().unwrap();
        let identity =
            ToolSession::legacy_identity(panes.get(tool.session_name()).unwrap()).unwrap();
        let proof = crate::process::CapturedProcessTree::capture(
            identity.pane_pid.expect("live tool fixture"),
        )
        .unwrap();
        let _interruption = ToolSession::interrupt_retirement_after_prepare_for_test(ID);
        assert!(tool.kill_verified(ID, "probe", &identity, None).is_err());
        let journal = super::super::tool_retirement::LockedJournal::open().unwrap();
        let pending = journal.pending(ID, "probe").unwrap().clone();
        drop(journal);
        let mut redirected = pending.clone();
        let missing = tempfile::tempdir().unwrap();
        redirected.socket = missing
            .path()
            .join("absent.sock")
            .to_string_lossy()
            .into_owned();
        assert!(ToolSession::original_session_present(&redirected).is_err());
        let mut reused_server = pending.clone();
        reused_server
            .server_birth
            .start_id
            .push_str("-different-birth");
        assert!(!ToolSession::original_server_alive(&reused_server).unwrap());
        assert!(!ToolSession::original_session_present(&reused_server).unwrap());
        assert!(crate::tmux::tmux_command()
            .args([
                "set-option",
                "-t",
                &identity.session_id,
                "@aoe_tool_retirement",
                &uuid::Uuid::new_v4().to_string(),
            ])
            .status()
            .unwrap()
            .success());
        assert!(ToolSession::settle_pending_retirement(ID, "probe", None).is_err());
        assert!(!proof.all_exited().unwrap());
        assert!(crate::tmux::tmux_command()
            .args([
                "set-option",
                "-t",
                &identity.session_id,
                "@aoe_tool_retirement",
                &pending.token.to_string(),
            ])
            .status()
            .unwrap()
            .success());
        assert!(ToolSession::settle_pending_retirement(ID, "probe", None).unwrap());
        assert!(proof.all_exited().unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn obsolete_boot_debt_skips_uninspectable_server_and_replacement_adoption() {
        let _home = crate::session::test_support::isolate_app_dir();
        super::super::tool_retirement::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let boot = format!("obsolete-{}", crate::process::boot_id().unwrap());
        let pid = i32::MAX as u32;
        let tree = serde_json::from_value(serde_json::json!({
            "boot_id": boot,
            "processes": [{"pid":pid, "start_id":"uninspectable-original"}],
        }))
        .unwrap();
        let pending = super::super::tool_retirement::Retirement {
            token: uuid::Uuid::new_v4(),
            instance_id: ID.into(),
            tool_name: "probe".into(),
            socket: "/unreachable/original-tmux.sock".into(),
            server_pid: pid,
            server_birth: crate::process::ProcessBirthIdentity {
                boot_id: boot,
                start_id: "uninspectable-original-server".into(),
            },
            receipt: ToolStopReceipt {
                name: ToolSession::generate_name(ID, "old", "probe"),
                identity: crate::session::LegacyToolIdentity {
                    session_id: "$1".into(),
                    pane_id: "%1".into(),
                    pane_pid: Some(pid),
                },
                owner: serde_json::to_string(&(ID, "probe")).unwrap(),
                tree,
            },
        };
        let mut journal = super::super::tool_retirement::LockedJournal::open().unwrap();
        journal.publish(pending).unwrap();
        drop(journal);
        let replacement = crate::session::LegacyToolAdoption {
            tmux_session: ToolSession::generate_name(ID, "replacement", "probe"),
            identity: crate::session::LegacyToolIdentity {
                session_id: "$2".into(),
                pane_id: "%2".into(),
                pane_pid: Some(std::process::id()),
            },
            server: std::sync::Arc::new(crate::session::TmuxServerIdentity {
                socket: "/unreachable/replacement.sock".into(),
                pid: std::process::id(),
                birth: crate::process::process_birth_identity(std::process::id())
                    .unwrap()
                    .unwrap(),
            }),
            profile: "default".into(),
            lifecycle_generation: 0,
        };
        FAIL_SERVER_BIRTH_PROBE.set(true);
        let result = ToolSession::settle_pending_retirement(ID, "probe", Some(&replacement));
        let skipped_probe = FAIL_SERVER_BIRTH_PROBE.replace(false);
        assert!(result.unwrap());
        assert!(
            skipped_probe,
            "positive boot change must precede every recorded PID/socket lookup"
        );
        assert!(!ToolSession::has_pending_retirement(ID, "probe").unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn old_tmux_only_retirement_receipt_is_not_overwritten_or_recaptured() {
        if !tmux_available() {
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        super::super::tool_retirement::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let unique = TmuxTestSession::new("old_receipt");
        let name = ToolSession::generate_name(ID, unique.name(), "probe");
        let _guard = TmuxTestSession::from_name(&name);
        let tool = ToolSession { name };
        tool.create_with_size("/tmp", "sleep 600", None, "default", ID, "probe")
            .unwrap();
        let panes = crate::tmux::batch_pane_metadata().unwrap();
        let identity =
            ToolSession::legacy_identity(panes.get(tool.session_name()).unwrap()).unwrap();
        let proof = crate::process::CapturedProcessTree::capture(
            identity.pane_pid.expect("live tool fixture"),
        )
        .unwrap();
        let receipt = serde_json::to_string(&ToolStopReceipt {
            name: tool.name.clone(),
            identity: identity.clone(),
            owner: serde_json::to_string(&(ID, "probe")).unwrap(),
            tree: proof.receipt().clone(),
        })
        .unwrap();
        assert!(crate::tmux::tmux_command()
            .args([
                "set-option",
                "-t",
                &identity.session_id,
                "@aoe_tool_stop",
                &receipt,
            ])
            .status()
            .unwrap()
            .success());
        assert!(tool.kill_verified(ID, "probe", &identity, None).is_err());
        assert!(!proof.all_exited().unwrap());
        assert_eq!(
            crate::tmux::test_helpers::pane_field(&identity.pane_id, "#{@aoe_tool_stop}"),
            receipt
        );
        assert!(!ToolSession::has_pending_retirement(ID, "probe").unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn legacy_adoption_and_verified_stop_fence_replacement_owner_and_pane_races() {
        use crate::tmux::test_helpers::{pane_field, wait_for_pane_command};
        if !tmux_available() {
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        super::super::tool_retirement::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
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
                server: metadata.server.clone().expect("legacy census server proof"),
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
                        adoption
                            .identity
                            .pane_pid
                            .expect("captured live pane PID")
                            .to_string()
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
                    assert!(tool
                        .kill_verified(&id, "probe", &verified, Some(&adoption.server))
                        .is_err());
                    assert_eq!(pane_field(&name, "#{@aoe_tool_owner}"), "");
                } else {
                    tool.kill_verified(&id, "probe", &verified, Some(&adoption.server))
                        .unwrap();
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
                server: None,
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
    #[serial_test::serial]
    fn legacy_dead_pane_consent_never_adopts_recycled_ids_from_a_replacement_server(
    ) -> anyhow::Result<()> {
        if !tmux_available() {
            return Ok(());
        }
        let home = crate::session::test_support::isolate_app_dir();
        let socket = home.path().join("server-replacement.sock");
        let _environment = crate::session::test_support::EnvGuard::set(&[(
            crate::tmux::TMUX_SOCKET_ENV,
            socket.as_os_str(),
        )]);
        struct PrivateServer<'a>(&'a std::path::Path);
        impl Drop for PrivateServer<'_> {
            fn drop(&mut self) {
                let _ = std::process::Command::new("tmux")
                    .arg("-S")
                    .arg(self.0)
                    .arg("kill-server")
                    .output();
            }
        }
        let _server = PrivateServer(&socket);
        let name = ToolSession::generate_name(ID, "Row", "probe");
        let create_dead =
            || -> anyhow::Result<std::collections::HashMap<String, crate::tmux::PaneMetadata>> {
                anyhow::ensure!(
                    crate::tmux::tmux_command()
                        .args([
                            "-f",
                            "/dev/null",
                            "new-session",
                            "-d",
                            "-s",
                            &name,
                            "sleep 600",
                            ";",
                            "set-option",
                            "-w",
                            "-t",
                            &name,
                            "remain-on-exit",
                            "on",
                            ";",
                            "respawn-pane",
                            "-k",
                            "-t",
                            &name,
                            "true",
                        ])
                        .status()?
                        .success(),
                    "private dead pane creation failed"
                );
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let panes = crate::tmux::batch_pane_metadata()?;
                    if panes.get(&name).is_some_and(|pane| pane.pane_dead) {
                        return Ok(panes);
                    }
                    anyhow::ensure!(
                        std::time::Instant::now() < deadline,
                        "private pane did not exit"
                    );
                    std::thread::yield_now();
                }
            };
        let original = create_dead()?;
        let original_server =
            crate::tmux::configured_tmux_server_identity(&crate::tmux::TmuxCommandDeadline::new())?
                .expect("original private server");
        let adoption = crate::session::LegacyToolAdoption {
            tmux_session: name.clone(),
            identity: ToolSession::legacy_identity(original.get(&name).unwrap())?,
            server: original
                .get(&name)
                .unwrap()
                .server
                .clone()
                .expect("original census server proof"),
            profile: "work".into(),
            lifecycle_generation: 0,
        };
        anyhow::ensure!(crate::tmux::tmux_command()
            .arg("kill-server")
            .status()?
            .success());
        let replacement = create_dead()?;
        let replacement_server =
            crate::tmux::configured_tmux_server_identity(&crate::tmux::TmuxCommandDeadline::new())?
                .expect("replacement private server");
        assert_ne!(original_server.1, replacement_server.1);
        assert_eq!(
            ToolSession::legacy_identity(replacement.get(&name).unwrap())?,
            adoption.identity,
            "the replacement server reused the same dead-pane IDs"
        );
        let result = ToolSession::adopt_legacy_snapshot(
            ID,
            "Row",
            "probe",
            "work",
            &adoption,
            &replacement,
            &legacy_scope(ID, "probe"),
        );
        let after = crate::tmux::batch_pane_metadata()?;
        assert!(
            matches!(
                after.get(&name).expect("replacement survives").tool_owner,
                crate::tmux::ToolPaneOwner::Unmarked
            ),
            "old consent modified a replacement server"
        );
        assert!(
            result.is_err(),
            "old consent was accepted for a replacement server"
        );
        Ok(())
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
