//! Tool sessions: user-configured dev tools (lazygit, yazi, tig, etc.) that
//! run in persistent tmux sessions tied to an agent session's working directory.

use anyhow::{bail, Result};

use super::utils::{
    append_session_setup_args, attach_client, create_session_tolerating_duplicate, is_pane_dead,
    kill_session_tree, kill_sessions_matching, sanitize_session_name,
};
use super::TOOL_PREFIX;
use crate::cli::truncate_id;

pub struct ToolSession {
    name: String,
}

impl ToolSession {
    /// The sub-session to act on, following a retitle onto the live pane. Tool
    /// `git` + title `log_T` and tool `git_log` + title `T` share a name, so
    /// resolution is skipped whenever more than one candidate matches.
    pub fn new(session_id: &str, session_title: &str, tool_name: &str) -> Self {
        Self::from_resolution(session_id, session_title, tool_name, false)
    }

    /// Snapshot-only [`Self::new`] for render paths.
    pub fn for_display(session_id: &str, session_title: &str, tool_name: &str) -> Self {
        Self::from_resolution(session_id, session_title, tool_name, true)
    }

    fn from_resolution(
        session_id: &str,
        session_title: &str,
        tool_name: &str,
        display_only: bool,
    ) -> Self {
        let prefix = Self::name_prefix(tool_name);
        let suffix = format!("_{}", truncate_id(session_id, 8));
        let derived = Self::generate_name(session_id, session_title, tool_name);
        let shape = crate::tmux::NameShape {
            prefix: &prefix,
            suffix: &suffix,
            kind: crate::tmux::SessionKind::Tool,
        };
        let name = if display_only {
            crate::tmux::session_name_for_display(&derived, &shape)
        } else {
            crate::tmux::live_session_name(&derived, &shape)
        };
        Self { name }
    }

    pub fn generate_name(session_id: &str, session_title: &str, tool_name: &str) -> String {
        format!(
            "{}{}_{}",
            Self::name_prefix(tool_name),
            sanitize_session_name(session_title),
            truncate_id(session_id, 8)
        )
    }

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
        let mut args: Vec<String> = ["new-session", "-d", "-s", &self.name, "-c", working_dir]
            .map(str::to_string)
            .to_vec();
        if let Some((width, height)) = size {
            args.extend(["-x".to_string(), width.to_string()]);
            args.extend(["-y".to_string(), height.to_string()]);
        }
        args.push(command.to_string());
        append_session_setup_args(
            &mut args,
            &self.name,
            &config,
            None,
            crate::tmux::SessionKind::Tool,
        );
        create_session_tolerating_duplicate(&args, |stderr| {
            format!("Failed to create tool session '{}': {}", self.name, stderr)
        })
    }

    pub fn kill(&self) -> Result<()> {
        kill_session_tree(&self.name)
    }

    /// Error if the pane dies within ~200ms: attaching to a dead
    /// `remain-on-exit` pane would leave Ctrl+C as the only way out.
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
        if attach_client(&self.name)?.is_some() {
            bail!("Failed to attach to tool session '{}'", self.name);
        }
        Ok(())
    }

    pub fn capture_pane(&self, lines: usize) -> Result<String> {
        super::Session::from_name(&self.name).capture_pane(lines)
    }
}

/// Kill every tool session for an agent session id, matched by suffix so tools
/// removed from config since are reaped too.
pub fn kill_all_tool_sessions_for_id(session_id: &str) {
    let id_suffix = format!("_{}", truncate_id(session_id, 8));
    kill_sessions_matching(|name| name.starts_with(TOOL_PREFIX) && name.ends_with(&id_suffix));
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::TmuxTestSession;
    use super::*;
    use crate::tmux::test_helpers::require_tmux;

    const ID: &str = "abc12345deadbeef";

    #[test]
    #[serial_test::serial]
    fn new_adopts_a_retitled_tool_session_but_not_another_tools() {
        let guard = crate::tmux::SessionCacheGuard::capture();
        let stale_lazygit = ToolSession::generate_name(ID, "Vikings", "lazygit");
        guard.force_present(&[stale_lazygit.as_str()]);

        assert_eq!(
            ToolSession::new(ID, "Refactor billing", "lazygit").session_name(),
            stale_lazygit
        );
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
    #[test]
    #[serial_test::serial]
    fn wait_until_ready_errs_with_pane_tail_when_pane_dies_immediately() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        require_tmux!();

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
    fn wait_until_ready_ok_when_pane_stays_alive() {
        require_tmux!();

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
