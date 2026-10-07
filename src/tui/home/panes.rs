//! Bringing a session's tmux panes up: terminal, container terminal, and
//! tool panes.

use super::*;

impl HomeView {
    pub fn start_terminal_for_instance_with_size(
        &mut self,
        id: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        self.try_mutate_instance(id, |inst| inst.start_terminal_with_size(size))?;
        self.save()?;
        Ok(())
    }

    fn cloned_instance(&self, session_id: &str) -> anyhow::Result<Instance> {
        self.get_instance(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", session_id))
    }

    /// Respawn the host-terminal pane when it is missing or dead, for live-send.
    pub(super) fn ensure_terminal_pane_ready(
        &mut self,
        session_id: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        let term = self.cloned_instance(session_id)?.terminal_tmux_session()?;
        if !term.exists() || term.is_pane_dead() {
            if term.exists() {
                let _ = term.kill();
            }
            self.start_terminal_for_instance_with_size(session_id, size)?;
        }
        Ok(())
    }

    /// Respawn the container-terminal pane when it is missing or dead, for live-send.
    pub(super) fn ensure_container_terminal_pane_ready(
        &mut self,
        session_id: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        let inst = self.cloned_instance(session_id)?;
        if !inst.is_sandboxed() {
            anyhow::bail!("Cannot prepare container terminal for non-sandboxed session");
        }
        let term = inst.container_terminal_tmux_session()?;
        if !term.exists() || term.is_pane_dead() {
            // A stopped container may need a minutes-long store move first;
            // it runs on the worker and the send is not queued behind it.
            if self.needs_store_move_before_launch(session_id)
                && !crate::containers::DockerContainer::from_session_id(session_id)
                    .is_running()
                    .unwrap_or(false)
            {
                self.begin_store_move(session_id, None);
                anyhow::bail!(
                    "its agent store is being moved first; retry once the status line clears"
                );
            }
            if term.exists() {
                let _ = term.kill();
            }
            self.start_container_terminal_for_instance_with_size(session_id, size)?;
        }
        Ok(())
    }

    /// Create the tool pane on demand so live-send can target a tool that
    /// hasn't been launched yet.
    pub(super) fn ensure_tool_pane_ready(
        &mut self,
        session_id: &str,
        tool_name: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        let inst = self.cloned_instance(session_id)?;
        let tool_config = self
            .tool_configs
            .get(tool_name)
            .ok_or_else(|| anyhow::anyhow!("tool '{}' is not configured", tool_name))?;
        if tool_config.command.is_empty() {
            anyhow::bail!("Tool '{}' has no command configured", tool_name);
        }
        let tool = crate::tmux::ToolSession::new(&inst.id, &inst.title, tool_name);
        if !tool.exists() || tool.is_pane_dead() {
            if tool.exists() {
                let _ = tool.kill();
            }
            tool.create_with_size(
                &inst.project_path,
                &tool_config.command,
                size,
                &inst.effective_profile(),
            )?;
        }
        Ok(())
    }

    /// Restart `id` on the restart worker and attach once it launches (see
    /// `take_restarted_attaches`). A restart already in flight is joined.
    pub fn restart_then_attach(
        &mut self,
        id: &str,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) {
        if self.get_instance(id).is_none() {
            return;
        }
        self.attach_after_restart.insert(id.to_string());
        if !self.restart_in_flight.insert(id.to_string()) {
            return;
        }
        self.mutate_instance(id, |inst| {
            inst.status = crate::session::Status::Starting;
            inst.last_error = None;
            inst.last_start_time = Some(std::time::Instant::now());
        });
        let Some(instance) = self.get_instance(id).cloned() else {
            return;
        };
        self.restart_poller
            .request_restart(crate::session::restart::RestartRequest {
                session_id: id.to_string(),
                instance,
                size,
                wake_message: String::new(),
                skip_on_launch,
                bound_hooks: false,
                discard_sandbox_container: false,
                conversation_carry: None,
            });
    }

    pub fn get_terminal_mode(&self, session_id: &str) -> TerminalMode {
        self.terminal_modes
            .get(session_id)
            .copied()
            .unwrap_or(self.default_terminal_mode)
    }

    /// The terminal a session's Terminal view shows: its chosen mode when
    /// sandboxed, otherwise always the host terminal.
    pub(super) fn effective_terminal_mode(&self, session_id: &str) -> TerminalMode {
        match self.get_instance(session_id) {
            Some(inst) if inst.is_sandboxed() => self.get_terminal_mode(session_id),
            _ => TerminalMode::Host,
        }
    }

    pub fn toggle_terminal_mode(&mut self, session_id: &str) {
        let new_mode = match self.get_terminal_mode(session_id) {
            TerminalMode::Container => TerminalMode::Host,
            TerminalMode::Host => TerminalMode::Container,
        };
        self.terminal_modes.insert(session_id.to_string(), new_mode);
    }

    pub fn start_container_terminal_for_instance_with_size(
        &mut self,
        id: &str,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        self.try_mutate_instance(id, |inst| inst.start_container_terminal_with_size(size))
            .map(|_| ())
    }
}
