//! Tool sessions: user-configured dev tools (lazygit, yazi, tig, etc.) that
//! run in persistent tmux sessions tied to an agent session's working directory.

use anyhow::{bail, Result};

use super::utils::{
    append_pane_base_index_args, append_remain_on_exit_args, append_tmux_setting_args,
    append_window_size_args, is_pane_dead, sanitize_session_name,
};
use super::{refresh_session_cache, TOOL_PREFIX};
use crate::cli::truncate_id;
use crate::process;

pub struct ToolSession {
    name: String,
}

impl ToolSession {
    /// The tool sub-session name to ACT on. Resolves onto the live sub-session
    /// for this session id and tool when the stored title has moved out from
    /// under its name, so reopening a tool after a retitle reattaches to the
    /// running pane instead of spawning a second one beside it (the same defect
    /// #3157 fixed for the agent pane).
    ///
    /// Known limit, inherited from the name format rather than introduced here:
    /// the tool/title boundary is not recoverable from the name, so tool `git`
    /// with title `log_T` and tool `git_log` with title `T` produce the same
    /// name. Resolving `git` can therefore see a `git_log` pane as a candidate.
    /// When both tools' panes are live the ambiguity guard in
    /// `crate::tmux::resolve_session_name` keeps the derived name, so the only
    /// exposure is a retitled session where the extension-named tool's pane is
    /// live and the shorter one's is not. Resolution is skipped entirely rather
    /// than guessing whenever more than one candidate matches.
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
        // The tool name sits in the prefix, so it discriminates between a
        // session's several tool sub-sessions without reference to the title.
        let prefix = Self::name_prefix(tool_name);
        let suffix = format!("_{}", truncate_id(session_id, 8));
        let derived = Self::generate_name(session_id, session_title, tool_name);
        let shape = crate::tmux::NameShape {
            prefix: &prefix,
            suffix: &suffix,
            excluded_prefixes: &[],
        };
        let name = if display_only {
            crate::tmux::session_name_for_display(&derived, &shape)
        } else {
            crate::tmux::live_session_name(&derived, &shape)
        };
        Self { name }
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
    fn name_prefix(tool_name: &str) -> String {
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
    ) -> Result<()> {
        if self.exists() {
            return Ok(());
        }
        let config = crate::tmux::tmux_option_config(profile);

        let mut args = vec![
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            self.name.clone(),
            "-c".to_string(),
            working_dir.to_string(),
        ];

        if let Some((width, height)) = size {
            args.push("-x".to_string());
            args.push(width.to_string());
            args.push("-y".to_string());
            args.push(height.to_string());
        }

        args.push(command.to_string());

        append_remain_on_exit_args(&mut args, &self.name);
        append_pane_base_index_args(&mut args, &self.name);
        append_window_size_args(&mut args, &self.name);
        append_tmux_setting_args(&mut args, &self.name, &config);

        let output = crate::tmux::tmux_command().args(&args).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("duplicate session") {
                refresh_session_cache();
                return Ok(());
            }
            bail!("Failed to create tool session '{}': {}", self.name, stderr);
        }

        refresh_session_cache();
        Ok(())
    }

    pub fn kill(&self) -> Result<()> {
        if !self.exists() {
            return Ok(());
        }

        if let Some(pane_pid) = self.get_pane_pid() {
            process::kill_process_tree(pane_pid);
        }

        super::utils::kill_session_if_present(&self.name)?;

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
        if !self.exists() {
            bail!("Tool session does not exist: {}", self.name);
        }

        if crate::tmux::utils::inside_tmux() {
            let status = crate::tmux::tmux_command()
                .args(["switch-client", "-t", &self.name])
                .status()?;

            if !status.success() {
                let status = crate::tmux::tmux_command()
                    .args(["attach-session", "-t", &self.name])
                    .status()?;

                if !status.success() {
                    bail!("Failed to attach to tool session '{}'", self.name);
                }
            }
        } else {
            let status = crate::tmux::tmux_command()
                .args(["attach-session", "-t", &self.name])
                .status()?;

            if !status.success() {
                bail!("Failed to attach to tool session '{}'", self.name);
            }
        }

        Ok(())
    }

    pub fn capture_pane(&self, lines: usize) -> Result<String> {
        super::Session::from_name(&self.name).capture_pane(lines)
    }

    fn get_pane_pid(&self) -> Option<u32> {
        process::get_pane_pid(&self.name)
    }
}

/// Kill all tool sessions associated with a given agent session ID.
/// Uses tmux list-sessions to find matches by ID suffix, so it works
/// even if tools have been removed from the config since creation.
pub fn kill_all_tool_sessions_for_id(session_id: &str) {
    let id_suffix = format!("_{}", truncate_id(session_id, 8));

    let output = crate::tmux::tmux_query_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if line.starts_with(TOOL_PREFIX) && line.ends_with(&id_suffix) {
                    if let Some(pid) = process::get_pane_pid(line) {
                        process::kill_process_tree(pid);
                    }
                    let _ = crate::tmux::tmux_command()
                        .args(["kill-session", "-t", line])
                        .output();
                }
            }
        }
    }

    refresh_session_cache();
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::TmuxTestSession;
    use super::*;

    /// A session id long enough that `truncate_id(.., 8)` truncates.
    const ID: &str = "abc12345deadbeef";

    #[test]
    #[serial_test::serial]
    fn new_adopts_a_retitled_tool_session_but_not_another_tools() {
        // #3157 for tool sub-sessions: the title moved, the tool's tmux session
        // kept the name it was created under. Reopening lazygit must reattach to
        // the running pane rather than spawn a second one, and must never adopt
        // a different tool's pane, which the tool name in the prefix guarantees.
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
            "yazi must not adopt lazygit's pane: {yazi}"
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
        )
        .expect("create_with_size");

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
    fn wait_until_ready_ok_when_pane_stays_alive() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let guard = TmuxTestSession::new("aoe_test_tool_alive");
        let tool = ToolSession {
            name: guard.name().to_string(),
        };
        tool.create_with_size(
            dir.path().to_str().expect("utf8 path"),
            "sleep 5",
            Some((80, 24)),
            "default",
        )
        .expect("create_with_size");

        let result = tool.wait_until_ready();

        assert!(
            result.is_ok(),
            "wait_until_ready should succeed for a still-running pane, got: {result:?}"
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
    fn new_name_sanitizes_unsafe_characters() {
        // tmux session names can't contain ':' or '.'
        let s = ToolSession::new("abc12345", "feature/foo:bar", "my tool.v2");
        let name = s.session_name();
        assert!(!name.contains(':'), "name was {}", name);
        assert!(!name.contains('.'), "name was {}", name);
        assert!(!name.contains(' '), "name was {}", name);
    }

    #[test]
    fn distinct_tools_on_same_session_have_distinct_names() {
        let id = "0123456789abcdef";
        let lazygit = ToolSession::new(id, "x", "lazygit");
        let yazi = ToolSession::new(id, "x", "yazi");
        assert_ne!(lazygit.session_name(), yazi.session_name());
    }

    #[test]
    fn distinct_sessions_for_same_tool_have_distinct_names() {
        let a = ToolSession::new("aaaaaaaa1111", "x", "lazygit");
        let b = ToolSession::new("bbbbbbbb2222", "x", "lazygit");
        assert_ne!(a.session_name(), b.session_name());
    }
}
