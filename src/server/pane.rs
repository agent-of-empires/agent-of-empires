//! Shared tmux-pane helpers for the live (capture-streaming) WebSocket
//! handlers: dead-pane rescue for the paired host/container shells, the
//! readiness probe used before a session is rendered, and the close codes /
//! early-close helper. The old PTY-relay renderer that lived here was removed
//! when the web dashboard unified on the capture-snapshot live view.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket};

use super::AppState;

/// Upper bound on the paired-terminal index a client may request. The web
/// dashboard owns the live set of terminal tabs, so a stray or hostile request
/// could otherwise spawn unbounded tmux sessions; this caps the blast radius.
/// 31 is far above any plausible tab count. See #2437.
pub(crate) const MAX_TERMINAL_INDEX: u32 = 31;

/// Close code we send when the live capture loop found the underlying pane
/// gone. The web live hook treats this as "stop retrying immediately, surface
/// the manual reconnect banner" rather than burning the retry budget against a
/// permanently broken pane. Picked from the application-reserved 4000-4999
/// range; not used elsewhere. See #1107.
pub(crate) const CLOSE_CODE_PTY_DEAD: u16 = 4001;

/// WebSocket close code 1001 ("going away"). Sent when the daemon is
/// shutting down so the client can distinguish a server-side exit from
/// a transient transport error and skip its reconnect backoff for one
/// cycle. See #1198.
pub(crate) const CLOSE_CODE_GOING_AWAY: u16 = 1001;

/// WebSocket close code 1013 ("try again later"). Sent when the tmux
/// pane is not ready within the bounded readiness window. Browser
/// retries on the fast-start ladder. Distinct from 4001 (permanently dead
/// pane) so logs separate transient warm-up from genuine failure. See #1455.
pub(crate) const CLOSE_CODE_TRY_AGAIN_LATER: u16 = 1013;

/// Total time we'll spend waiting for the tmux session + pane to be
/// attachable before giving up and closing 1013. 2s covers tmux warm-up
/// across the slow machines we've seen reports from while staying short
/// enough that a truly dead pane doesn't hold the upgrade open for the
/// user. See #1455.
const TMUX_READY_TIMEOUT: Duration = Duration::from_millis(2000);

/// Poll interval for the readiness wait. 50ms gives ~40 probes inside
/// the 2s window; each probe shells out to `tmux has-session` and (if
/// that passes) `tmux list-panes`, which is cheap.
const TMUX_READY_POLL: Duration = Duration::from_millis(50);

/// Revive a dead paired host-shell pane (or recreate a missing session) so a
/// live-view reconnect recovers instead of hot-looping. Returns the tmux
/// session name to capture.
pub(crate) async fn respawn_paired_if_dead(
    state: &Arc<AppState>,
    id: &str,
    inst: &crate::session::Instance,
    index: u32,
) -> anyhow::Result<String> {
    let tmux_name =
        crate::tmux::TerminalSession::generate_name_indexed(&inst.id, &inst.title, index);

    // Serialize concurrent reconnects for the same session so two
    // simultaneous WS attaches don't both try to recreate the pane.
    let lock = state.instance_lock(id).await;
    let _guard = lock.lock().await;

    let mut inst_for_blocking = inst.clone();
    let tmux_name_clone = tmux_name.clone();
    // Two failure modes the user can land in:
    //   1. Pane is dead but the tmux session still exists (shell exit
    //      under `remain-on-exit on`). `kill_terminal_if_dead` clears
    //      the tombstone, then we respawn.
    //   2. The whole tmux session is gone (`tmux kill-session`, daemon
    //      reaped on aoe restart, etc). `kill_terminal_if_dead`
    //      returns false here because there's nothing to kill, but the
    //      next capture finds no session and the WS closes 4001. Recreate
    //      the session in that case too so the retry click recovers
    //      instead of hot-looping. See #1107 follow-up.
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
        crate::tmux::ContainerTerminalSession::generate_name_indexed(&inst.id, &inst.title, index);

    let lock = state.instance_lock(id).await;
    let _guard = lock.lock().await;

    let mut inst_for_blocking = inst.clone();
    let tmux_name_clone = tmux_name.clone();
    // No in-memory cache to update for container terminal: `has_container_terminal()`
    // queries tmux directly, so unlike the paired variant we don't need to write
    // back a `terminal_info` flag after a successful respawn.
    //
    // See `respawn_paired_if_dead` for the missing-session branch: a
    // `tmux kill-session` on a paired container terminal also has to
    // recreate from scratch, not just kill-then-respawn the pane.
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

/// Outcome of one tmux-readiness probe. `Ready` lets the caller proceed
/// to capture the pane; `NotReady` means try again after the poll
/// interval; `Dead` short-circuits the wait when every pane is reported
/// dead (no point in polling further).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PaneReadiness {
    Ready,
    NotReady,
    Dead,
}

/// Parse `tmux list-panes -F "#{pane_dead}"` output: one line per pane,
/// each line `0` (alive) or `1` (dead). Empty output means the session
/// exists but has no panes yet (not ready). All-dead means the pane has
/// permanently exited.
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

/// Poll `tmux has-session` + `tmux list-panes` at TMUX_READY_POLL until
/// the session has at least one alive pane, or until TMUX_READY_TIMEOUT
/// expires. Returns the final outcome so the caller can distinguish a
/// transient warm-up (`NotReady` -> retryable 1013) from a permanently
/// dead pane (`Dead` -> 4001 short-circuit). Bails out early on `Dead`
/// rather than polling further because no amount of waiting will make
/// an exited pane reattachable.
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

/// One probe iteration: `tmux has-session` then (on success) `tmux
/// list-panes -F "#{pane_dead}"`. Both shell out to the tmux binary;
/// they're cheap (microseconds in the happy path) so the 50ms poll
/// floor dominates wall time, not subprocess overhead.
async fn probe_tmux_readiness(tmux_name: &str) -> PaneReadiness {
    let name = tmux_name.to_string();
    tokio::task::spawn_blocking(move || {
        let has_session = std::process::Command::new("tmux")
            .args(["has-session", "-t", &name])
            .output();
        let has_session_ok = match has_session {
            Ok(o) => o.status.success(),
            Err(_) => false,
        };
        if !has_session_ok {
            return PaneReadiness::NotReady;
        }
        let panes = std::process::Command::new("tmux")
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

    #[test]
    fn parse_pane_dead_empty_is_not_ready() {
        assert_eq!(parse_pane_dead_output(""), PaneReadiness::NotReady);
        assert_eq!(parse_pane_dead_output("   \n  \n"), PaneReadiness::NotReady);
    }

    #[test]
    fn parse_pane_dead_single_alive_is_ready() {
        assert_eq!(parse_pane_dead_output("0\n"), PaneReadiness::Ready);
    }

    #[test]
    fn parse_pane_dead_single_dead_is_dead() {
        assert_eq!(parse_pane_dead_output("1\n"), PaneReadiness::Dead);
    }

    #[test]
    fn parse_pane_dead_mixed_is_ready() {
        assert_eq!(parse_pane_dead_output("1\n0\n1\n"), PaneReadiness::Ready);
    }

    #[test]
    fn parse_pane_dead_all_dead_is_dead() {
        assert_eq!(parse_pane_dead_output("1\n1\n"), PaneReadiness::Dead);
    }
}
