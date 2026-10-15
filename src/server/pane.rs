//! Shared tmux-pane helpers for the live (capture-streaming) WebSocket handlers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket};

use super::AppState;

/// Upper bound on the paired-terminal index a client may request.
pub(crate) const MAX_TERMINAL_INDEX: u32 = 31;

/// Close code we send when the live capture loop found the underlying pane gone.
pub(crate) const CLOSE_CODE_PTY_DEAD: u16 = 4001;

/// WebSocket close code 1001 ("going away").
pub(crate) const CLOSE_CODE_GOING_AWAY: u16 = 1001;

/// WebSocket close code 1013 ("try again later").
pub(crate) const CLOSE_CODE_TRY_AGAIN_LATER: u16 = 1013;

/// Total time we'll spend waiting for the tmux session + pane to be attachable before
/// giving up and closing 1013.
const TMUX_READY_TIMEOUT: Duration = Duration::from_millis(2000);

/// Poll interval for the readiness wait.
const TMUX_READY_POLL: Duration = Duration::from_millis(50);

/// Revive a dead paired host-shell pane (or recreate a missing session) so a live-view
/// reconnect recovers instead of hot-looping.
pub(crate) async fn respawn_paired_if_dead(
    state: &Arc<AppState>,
    id: &str,
    inst: &crate::session::Instance,
    index: u32,
) -> anyhow::Result<String> {
    let tmux_name =
        crate::tmux::TerminalSession::resolve_name_indexed(&inst.id, &inst.title, index);

    // Serialize concurrent reconnects for the same session so two
    // simultaneous WS attaches don't both try to recreate the pane.
    let lock = state.instance_lock(id).await;
    let _guard = lock.lock().await;

    let mut inst_for_blocking = inst.clone();
    let tmux_name_clone = tmux_name.clone();
    // Two failure modes the user can land in.
    let respawned = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let killed_dead = inst_for_blocking.kill_terminal_if_dead_indexed(index)?;
        let session_missing = !inst_for_blocking
            .terminal_tmux_session_indexed(index)?
            .exists();
        if !killed_dead && !session_missing {
            return Ok(false);
        }
        if killed_dead {
            tracing::warn!(
                target: "terminal.ws",
                tmux = %tmux_name_clone,
                "paired terminal pane dead at WS upgrade, killing and respawning"
            );
        } else {
            tracing::warn!(
                target: "terminal.ws",
                tmux = %tmux_name_clone,
                "paired terminal session missing at WS upgrade, recreating"
            );
        }
        inst_for_blocking.start_terminal_with_size_indexed(index, None)?;
        Ok(true)
    })
    .await
    .map_err(|e| anyhow::anyhow!("respawn task panicked: {e}"))??;

    // Only index 0 has an in-memory cache flag; additional terminals are
    // tmux-queried, so there is nothing to write back for them.
    if respawned && index == 0 {
        let mut instances = state.instances.write().await;
        if let Some(stored) = instances.iter_mut().find(|i| i.id == id) {
            stored.terminal_info = Some(crate::session::TerminalInfo { created: true });
        }
    }

    Ok(tmux_name)
}

/// Container-terminal counterpart of [`respawn_paired_if_dead`].
pub(crate) async fn respawn_container_if_dead(
    state: &Arc<AppState>,
    id: &str,
    inst: &crate::session::Instance,
    index: u32,
) -> anyhow::Result<String> {
    let tmux_name =
        crate::tmux::ContainerTerminalSession::resolve_name_indexed(&inst.id, &inst.title, index);

    let lock = state.instance_lock(id).await;
    let _guard = lock.lock().await;

    let mut inst_for_blocking = inst.clone();
    let tmux_name_clone = tmux_name.clone();
    // No in-memory cache to update for container terminal.
    let _respawned = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let killed_dead = inst_for_blocking.kill_container_terminal_if_dead_indexed(index)?;
        let session_missing = !inst_for_blocking
            .container_terminal_tmux_session_indexed(index)?
            .exists();
        if !killed_dead && !session_missing {
            return Ok(false);
        }
        if killed_dead {
            tracing::warn!(
                target: "terminal.ws",
                tmux = %tmux_name_clone,
                "container terminal pane dead at WS upgrade, killing and respawning"
            );
        } else {
            tracing::warn!(
                target: "terminal.ws",
                tmux = %tmux_name_clone,
                "container terminal session missing at WS upgrade, recreating"
            );
        }
        inst_for_blocking.start_container_terminal_with_size_indexed(index, None)?;
        Ok(true)
    })
    .await
    .map_err(|e| anyhow::anyhow!("respawn task panicked: {e}"))??;

    Ok(tmux_name)
}

/// Send a close frame on a socket we're about to drop before the main loop.
pub(crate) async fn close_early(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// Outcome of one tmux-readiness probe.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PaneReadiness {
    Ready,
    NotReady,
    Dead,
}

/// Parse `tmux list-panes -F "#{pane_dead}"` output.
fn parse_pane_dead_output(output: &str) -> PaneReadiness {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if lines.is_empty() {
        return PaneReadiness::NotReady;
    }
    if lines.contains(&"0") {
        PaneReadiness::Ready
    } else {
        PaneReadiness::Dead
    }
}

/// Poll `tmux has-session` + `tmux list-panes` at TMUX_READY_POLL until the session has at
/// least one alive pane, or until TMUX_READY_TIMEOUT expires.
pub(crate) async fn wait_for_tmux_ready(tmux_name: &str) -> PaneReadiness {
    let deadline = Instant::now() + TMUX_READY_TIMEOUT;
    loop {
        match probe_tmux_readiness(tmux_name).await {
            PaneReadiness::Ready => return PaneReadiness::Ready,
            PaneReadiness::Dead => return PaneReadiness::Dead,
            PaneReadiness::NotReady => {
                if Instant::now() >= deadline {
                    return PaneReadiness::NotReady;
                }
                tokio::time::sleep(TMUX_READY_POLL).await;
            }
        }
    }
}

/// One probe iteration.
async fn probe_tmux_readiness(tmux_name: &str) -> PaneReadiness {
    let name = tmux_name.to_string();
    tokio::task::spawn_blocking(move || {
        let has_session = crate::tmux::tmux_command()
            .args(["has-session", "-t", &name])
            .output();
        let has_session_ok = match has_session {
            Ok(o) => o.status.success(),
            Err(_) => false,
        };
        if !has_session_ok {
            return PaneReadiness::NotReady;
        }
        let panes = crate::tmux::tmux_command()
            .args(["list-panes", "-t", &name, "-F", "#{pane_dead}"])
            .output();
        match panes {
            Ok(o) if o.status.success() => {
                parse_pane_dead_output(&String::from_utf8_lossy(&o.stdout))
            }
            _ => PaneReadiness::NotReady,
        }
    })
    .await
    .unwrap_or(PaneReadiness::NotReady)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window is ready while any pane is alive; an empty answer means tmux has not
    /// created the panes yet, which is not the same as a dead window.
    #[test]
    fn parse_pane_dead_output_reads_the_whole_window() {
        for (out, want) in [
            ("", PaneReadiness::NotReady),
            ("   \n  \n", PaneReadiness::NotReady),
            ("0\n", PaneReadiness::Ready),
            ("1\n", PaneReadiness::Dead),
            ("1\n0\n1\n", PaneReadiness::Ready),
            ("1\n1\n", PaneReadiness::Dead),
        ] {
            assert_eq!(parse_pane_dead_output(out), want, "{out:?}");
        }
    }
}
