//! Tool sessions: user-configured dev tools (lazygit, yazi, tig, etc.) that
//! run in persistent tmux sessions tied to an agent session's working directory.

use anyhow::{bail, Result};
use std::process::Command;

use super::utils::{
    append_clipboard_passthrough_args, append_mouse_on_args, append_pane_base_index_args,
    append_remain_on_exit_args, append_window_size_args, is_pane_dead, sanitize_session_name,
};
use super::{refresh_session_cache, session_exists_from_cache, TOOL_PREFIX};
use crate::cli::truncate_id;
use crate::process;
use crate::session::config::should_apply_tmux_clipboard;

pub struct ToolSession {
    name: String,
}

impl ToolSession {
    pub fn new(session_id: &str, session_title: &str, tool_name: &str) -> Self {
        let safe_title = sanitize_session_name(session_title);
        let safe_tool = sanitize_session_name(tool_name);
        let name = format!(
            "{}{}_{}_{}",
            TOOL_PREFIX,
            safe_tool,
            safe_title,
            truncate_id(session_id, 8)
        );
        Self { name }
    }

    pub fn session_name(&self) -> &str {
        &self.name
    }

    pub fn exists(&self) -> bool {
        if let Some(exists) = session_exists_from_cache(&self.name) {
            return exists;
        }

        Command::new("tmux")
            .args(["has-session", "-t", &self.name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn is_pane_dead(&self) -> bool {
        is_pane_dead(&self.name)
    }

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: &str,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        if self.exists() {
            return Ok(());
        }

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
        append_mouse_on_args(&mut args, &self.name);
        append_window_size_args(&mut args, &self.name);
        if should_apply_tmux_clipboard() {
            append_clipboard_passthrough_args(&mut args, &self.name);
        }

        let output = Command::new("tmux").args(&args).output()?;

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

    pub fn attach(&self) -> Result<()> {
        if !self.exists() {
            bail!("Tool session does not exist: {}", self.name);
        }

        if std::env::var("TMUX").is_ok() {
            let status = Command::new("tmux")
                .args(["switch-client", "-t", &self.name])
                .status()?;

            if !status.success() {
                let status = Command::new("tmux")
                    .args(["attach-session", "-t", &self.name])
                    .status()?;

                if !status.success() {
                    bail!("Failed to attach to tool session '{}'", self.name);
                }
            }
        } else {
            let status = Command::new("tmux")
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

    let output = Command::new("tmux")
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
                    let _ = Command::new("tmux")
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
    use super::*;

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
