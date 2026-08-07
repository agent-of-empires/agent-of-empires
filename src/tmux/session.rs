//! tmux session management

use anyhow::{bail, Result};
use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::{
    composite::{CapturedPane, PaneGeom, WindowLayout},
    probe_session_existence, refresh_session_cache,
    utils::{
        append_pane_base_index_args, append_remain_on_exit_args, append_tmux_setting_args,
        append_window_size_args, is_pane_dead, is_pane_running_shell,
    },
    SessionExistence, SESSION_PREFIX,
};
use crate::cli::truncate_id;
use crate::process;
use crate::session::Status;
use crate::util::now_ms;

pub struct Session {
    name: String,
}

/// tmux user options holding the cross-process size-owner lock (see
/// [`Session::claim_size_owner`]). User options ride on the session itself, so
/// the web daemon and the native TUI read and write the same state.
const SIZE_OWNER_OPT: &str = "@aoe_size_owner";
const SIZE_OWNER_HB_OPT: &str = "@aoe_size_owner_hb";

/// tmux user options holding the cross-process VT-pipe owner lock. `tmux
/// pipe-pane` is exclusive per pane: a second process arming it silently
/// kills the first process's forwarder, so two aoe processes previewing the
/// same pane (a second TUI, the serve daemon's web live view) used to fight
/// over the pipe on their re-arm throttles, each flipping the other back to
/// the capture fallback every few seconds. The lock makes arming cooperative:
/// only the holder pipes; everyone else stays on `capture-pane`, which every
/// consumer already falls back to.
const VT_OWNER_OPT: &str = "@aoe_vt_owner";
const VT_OWNER_HB_OPT: &str = "@aoe_vt_owner_hb";

/// How long a VT-pipe owner lock survives without a heartbeat before another
/// process may arm over it. The holder refreshes from its sample loop (every
/// viewer samples at least at idle cadence), so a live holder keeps the pipe
/// and a crashed one frees it within this window.
pub const VT_OWNER_TTL: Duration = Duration::from_secs(4);

/// How long a size-owner lock survives without a heartbeat before another
/// client may steal it. Shared by every surface that drives window size (the
/// web PTY relay, the mobile live view, the native TUI) so they age the lock
/// the same and a connected owner is never stolen from mid-use.
pub const SIZE_OWNER_TTL: Duration = Duration::from_secs(4);
/// How often a connected size owner refreshes its heartbeat. Well under
/// [`SIZE_OWNER_TTL`] so a live-but-idle owner keeps the lock while connected;
/// the lock only frees on disconnect/crash (TTL expiry) or explicit take-over.
pub const SIZE_OWNER_HEARTBEAT: Duration = Duration::from_millis(1500);

/// The active pane's cursor, queried alongside a `capture-pane` so the
/// live-send preview can paint a real cursor (`capture-pane` returns cell
/// text only; tmux's own client draws the cursor from these pane fields).
/// `pane_height` rides along so the renderer can map `y` (counted from the
/// top of the visible screen) onto the bottom-anchored preview output rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneCursor {
    pub x: u16,
    pub y: u16,
    /// `#{cursor_flag}`: 0 when the application hid the cursor (DECTCEM),
    /// e.g. an agent that parks it while "working". Don't paint when false.
    pub visible: bool,
    pub pane_height: u16,
    /// `#{history_size}`: lines currently in the pane's scrollback. The
    /// web live view sizes its virtual scroll spacer off this; absent in
    /// older format strings, in which case it parses as 0.
    pub history_size: u32,
    /// `#{pane_width}`: the live web view compares this against the
    /// viewer's requested grid to detect another writer (e.g. the TUI's
    /// preview sync) resizing the window out from under it. Optional in
    /// the format line; parses as 0 when absent.
    pub pane_width: u16,
    /// `#{alternate_on}`: the pane is on the alternate screen (a
    /// full-screen / TUI app). The alternate screen has no scrollback, so
    /// the live preview's capture-window scroll can't reach the app's own
    /// history; the TUI forwards the wheel to the app instead. Optional in
    /// the format line; parses as `false` when absent.
    pub alternate_on: bool,
    /// `#{mouse_any_flag}`: the foreground app has requested some mouse
    /// tracking mode (it wants mouse events at all). Optional; parses as
    /// `false`.
    pub mouse_tracking: bool,
    /// `#{mouse_sgr_flag}`: the app is in SGR (1006) mouse encoding, so it
    /// will parse the `\e[<..M` wheel bytes the TUI forwards as a mouse
    /// event rather than garbage keystrokes. The wheel is only forwarded
    /// when BOTH this and `mouse_tracking` are set: `mouse_tracking` alone
    /// can mean the legacy X10 encoding, which our SGR bytes would corrupt.
    /// Optional; parses as `false`.
    pub mouse_sgr: bool,
    /// `#{mouse_all_flag}`: the app is in any-event tracking (DEC 1003), so
    /// it wants bare mouse-motion reports even with no button held (hover).
    /// Gates the live preview's motion forwarding: a 1000/1002 app never
    /// expects bare-motion bytes. Optional; parses as `false`.
    pub mouse_all: bool,
    /// Whether `x`/`y` can be trusted to index the captured content. The
    /// terminal-mode flags above (`alternate_on`, `mouse_tracking`,
    /// `mouse_sgr`) are always valid, but `capture_pane_with_cursor` probes
    /// the cursor twice and, if the pane scrolled mid-capture, the row no
    /// longer maps onto the captured rows. It then publishes the cursor with
    /// this `false` so the render skips painting it (avoiding the row-drift
    /// bug), while the wheel forward, which reads only the mode flags, still
    /// works while an agent streams. `parse` sets it `true`; only the
    /// cross-probe check downgrades it.
    pub position_reliable: bool,
    /// Pane 0's `(width, height)` within a COMPOSITED preview, or `None` when
    /// the preview shows a single pane.
    ///
    /// Mouse forwarding maps the hovered cell into the previewed app's
    /// coordinate space by treating the preview rect as the pane, which holds
    /// while the two describe the same rectangle. A composite makes the rect the
    /// whole window, so a pointer over a neighbouring pane maps to a column past
    /// pane 0's right edge and is reported to the agent as though its own pane
    /// were that wide. Pane 0 is the only pane that receives input (#435, #488),
    /// so this carries its extent and the forward clamps to it, dropping events
    /// that land outside.
    ///
    /// Only the extent is needed, never the origin: tmux keeps pane 0 at the
    /// window origin, because pane indices follow layout order and closing pane
    /// 0 renumbers whichever pane takes that corner.
    pub composite_pane0: Option<(u16, u16)>,
}

/// tmux format line every cursor probe requests, parsed by
/// [`PaneCursor::parse`]. Shared so the plain capture and the composited one
/// cannot drift into asking for different fields.
const CURSOR_FMT: &str = "#{cursor_x} #{cursor_y} #{cursor_flag} #{pane_height} #{history_size} #{pane_width} #{alternate_on} #{mouse_any_flag} #{mouse_sgr_flag} #{mouse_all_flag}";

impl PaneCursor {
    /// Parse the single space-separated line emitted by the
    /// `#{cursor_x} #{cursor_y} #{cursor_flag} #{pane_height}
    /// #{history_size} #{pane_width} #{alternate_on} #{mouse_any_flag}
    /// #{mouse_sgr_flag} #{mouse_all_flag}` format. The trailing fields are
    /// optional so an older four-field line still parses (numeric fields as
    /// 0, flag fields as `false`).
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split_whitespace();
        let x = fields.next()?.parse().ok()?;
        let y = fields.next()?.parse().ok()?;
        let flag: u8 = fields.next()?.parse().ok()?;
        let pane_height = fields.next()?.parse().ok()?;
        let history_size = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
        let pane_width = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
        let alternate_on = fields.next().map(|f| f != "0").unwrap_or(false);
        let mouse_tracking = fields.next().map(|f| f != "0").unwrap_or(false);
        let mouse_sgr = fields.next().map(|f| f != "0").unwrap_or(false);
        let mouse_all = fields.next().map(|f| f != "0").unwrap_or(false);
        Some(Self {
            x,
            y,
            visible: flag != 0,
            pane_height,
            history_size,
            pane_width,
            alternate_on,
            mouse_tracking,
            mouse_sgr,
            mouse_all,
            // A single probe's own position is self-consistent; the
            // cross-probe check in `capture_pane_with_cursor` is the only
            // thing that downgrades this.
            position_reliable: true,
            // A probe describes one pane. The composited paths overwrite this
            // once they know the window really is split.
            composite_pane0: None,
        })
    }
}

/// Reconcile the two cursor probes `capture_pane_with_cursor` takes around the
/// capture. Only the VERTICAL-mapping inputs must be stable across the
/// capture: if `history_size` or `pane_height` changed, the screen scrolled or
/// resized mid-capture and the cursor's row no longer indexes the captured
/// content (the row-drift bug). A blinking cursor or horizontal jitter from an
/// animated TUI (claude's spinner) changes `visible`/`x` every frame but never
/// moves the row, so comparing the whole struct would suppress the cursor on
/// every frame of an actively repainting agent. Keep the post-capture cursor
/// (closest to the freshest content); when the mapping moved, flag the
/// POSITION as unreliable rather than dropping the whole cursor, so the wheel
/// forward (which reads only the always-valid mode flags) still works while an
/// agent streams, while the render skips painting on the drifted row. A probe
/// that didn't parse (pane gone / malformed) carries no trustworthy mode flags
/// either, so the result is `None`.
fn merge_cursor_probes(
    before: Option<PaneCursor>,
    after: Option<PaneCursor>,
) -> Option<PaneCursor> {
    match (before, after) {
        (Some(b), Some(a)) => {
            let position_reliable =
                b.history_size == a.history_size && b.pane_height == a.pane_height;
            Some(PaneCursor {
                position_reliable,
                ..a
            })
        }
        _ => None,
    }
}

/// Split the chained multi-pane capture into one [`CapturedPane`] per pane.
///
/// The output is a flat byte stream of `<sentinel + geometry>` lines each
/// followed by that pane's `capture-pane` rows, so the sentinel is the only
/// frame marker. A pane whose geometry line does not parse is dropped rather
/// than shifting every later pane's content onto the wrong rectangle.
fn parse_pane_segments(raw: &str, sentinel: &str) -> Vec<CapturedPane> {
    let mut panes: Vec<CapturedPane> = Vec::new();
    let mut current: Option<(PaneGeom, Vec<&str>)> = None;

    let flush = |panes: &mut Vec<CapturedPane>, entry: Option<(PaneGeom, Vec<&str>)>| {
        if let Some((geom, lines)) = entry {
            let body = lines.join("\n");
            panes.push(CapturedPane {
                rows: crate::tmux::vt::capture_rows_padded(
                    body.as_bytes(),
                    geom.width,
                    geom.height,
                ),
                geom,
            });
        }
    };

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix(sentinel) {
            flush(&mut panes, current.take());
            current = PaneGeom::parse(rest).map(|geom| (geom, Vec::new()));
        } else if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    flush(&mut panes, current.take());
    panes
}

/// Flag a cursor's POSITION as untrustworthy while keeping its always-valid
/// mode flags, for the composite paths that fall through to a scrollback-bearing
/// pane-0 capture.
///
/// [`Session::capture_pane_with_cursor`] earns the right to trust a position by
/// probing twice around its capture and comparing; a single probe against
/// content that includes scrollback has no such evidence, so the render skips
/// painting rather than risk the row-drift bug while the wheel forward (which
/// reads only the mode flags) keeps working.
fn unreliable_position(cursor: Option<PaneCursor>) -> Option<PaneCursor> {
    cursor.map(|c| PaneCursor {
        position_reliable: false,
        ..c
    })
}

/// A delta beyond this many rows between a window and its pane is a multi-pane
/// split (the missing rows are other panes), not window chrome.
const MAX_CHROME_ROWS: u16 = 5;

/// Rows of vertical window chrome (the tmux status bar) that sit outside the
/// pane, so a window sized to `H` yields a pane of `H - chrome`. Derived live
/// from `window_height - pane_height` rather than assumed, because whether a
/// detached window's pane reserves the status row varies by tmux version and
/// status setting (off, one line, or multi-line). A delta larger than
/// [`MAX_CHROME_ROWS`] is a split layout, not chrome, and resolves to 0 so a
/// caller never balloons the window chasing a pane height `resize-window`
/// cannot deliver.
fn chrome_rows(window_height: u16, pane_height: u16) -> u16 {
    let delta = window_height.saturating_sub(pane_height);
    if delta <= MAX_CHROME_ROWS {
        delta
    } else {
        0
    }
}

impl Session {
    pub fn new(id: &str, title: &str) -> Result<Self> {
        Ok(Self {
            name: Self::resolve_name(id, title),
        })
    }

    /// Construct a Session from a pre-computed tmux session name.
    pub fn from_name(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }

    /// The name of the tmux session to ACT on for this session id: the
    /// title-derived name normally, or the live session carrying this id's
    /// `_<id8>` tail when the stored title has moved out from under it (a
    /// smart rename, or a manual rename whose tmux rename failed). See
    /// `crate::tmux::live_session_name` (crate-private); every lifecycle operation
    /// resolves through here so trash/archive/attach/status target the pane
    /// that is actually running and `create` adopts it instead of spawning a
    /// second agent beside it.
    ///
    /// Use [`Self::generate_name`] instead only to compute the name a session
    /// should be renamed TO.
    pub fn resolve_name(id: &str, title: &str) -> String {
        crate::tmux::live_agent_session_name(id, &Self::generate_name(id, title))
    }

    /// Purely derive the tmux session name from a session id and title, with no
    /// reference to what is live. Callers that want the session's CURRENT name
    /// want [`Self::resolve_name`].
    pub fn generate_name(id: &str, title: &str) -> String {
        let safe_title = sanitize_session_name(title);
        format!("{}{}_{}", SESSION_PREFIX, safe_title, truncate_id(id, 8))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn exists(&self) -> bool {
        crate::tmux::session_exists(&self.name)
    }

    /// Tri-state existence probe that distinguishes "the tmux server
    /// confirmed this session is gone" from "the tmux server was
    /// unreachable, so we don't actually know". See [`SessionExistence`].
    /// Callers that would otherwise latch a destructive or error state on a
    /// plain `false` from [`Self::exists`] should use this instead.
    pub fn existence(&self) -> SessionExistence {
        probe_session_existence(&self.name)
    }

    pub fn create(&self, working_dir: &str, command: Option<&str>, profile: &str) -> Result<()> {
        self.create_with_size(working_dir, command, None, profile)
    }

    /// `profile` selects which config layer governs the `[tmux]` options this
    /// applies (see `crate::tmux::tmux_option_config`); pass the session's own
    /// profile so its overrides win over the global config.
    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
        profile: &str,
    ) -> Result<()> {
        self.create_with_size_env(working_dir, command, size, profile, &[])
    }

    /// Like [`Self::create_with_size`], but also sets `extra_env` on the new
    /// session via `new-session -e KEY=VALUE`.
    ///
    /// This is the channel for values that must not appear in the pane
    /// command's argv: `host_hooks.before_session` mints secrets, and the
    /// shell-assignment prefix used for the static `environment` list would
    /// publish them to `ps` for the pane's whole lifetime. The `-e` flags ride
    /// the short-lived `tmux` client invocation instead, and the tmux server
    /// hands the value to the pane's process environment.
    pub fn create_with_size_env(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
        profile: &str,
        extra_env: &[(String, String)],
    ) -> Result<()> {
        if self.exists() {
            return Ok(());
        }
        let config = super::tmux_option_config(profile);

        // Forward the inherited host env (DISPLAY, XDG_*, DBUS, ... plus every
        // other var when `session.inherit_host_environment` is on) so an agent
        // and any browser it launches, e.g. for OIDC, can reach the user's
        // desktop. tmux otherwise carries only its narrow `update-environment`
        // set plus the server's frozen base env (#3075, #3262).
        let inherited_env = crate::session::environment::inherited_host_env(profile);
        let mut env_refs: Vec<(&str, &str)> = inherited_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        // Appended last so a minted value overrides a same-keyed desktop entry.
        env_refs.extend(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

        let mut args = build_create_args(&self.name, working_dir, &env_refs, command, size);
        append_remain_on_exit_args(&mut args, &self.name);
        append_pane_base_index_args(&mut args, &self.name);
        append_window_size_args(&mut args, &self.name);
        append_tmux_setting_args(&mut args, &self.name, &config);

        let output = crate::tmux::tmux_command().args(&args).output()?;

        // Note: With -d flag, tmux new-session returns 0 even if the shell command fails.
        // Log args at debug level for troubleshooting.
        tracing::debug!(target: "tmux.command",
            "tmux new-session args: {:?}",
            args.iter()
                .map(|a| crate::session::environment::redact_env_values(a))
                .collect::<Vec<_>>()
        );

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to create tmux session: {}", stderr);
        }

        super::refresh_session_cache();

        Ok(())
    }

    pub fn is_pane_dead(&self) -> bool {
        is_pane_dead(&self.name)
    }

    pub fn is_pane_running_shell(&self) -> bool {
        is_pane_running_shell(&self.name)
    }

    /// Revive a dead pane in place via `tmux respawn-pane -k` without
    /// tearing down the surrounding tmux session.
    ///
    /// When `remain-on-exit on` is set, a pane whose process has exited
    /// stays around as a dead pane and the tmux session remains. The
    /// normal restart flow (kill-session + new-session) is correct for
    /// that case, but kill-session can race against the session cache:
    /// process-tree kill of a defunct pid stalls on macOS, and the
    /// subsequent kill can run while exists() still sees the cached
    /// entry, leaving the dead pane in place. Respawning first puts the
    /// pane back into a live state so the kill path proceeds cleanly.
    ///
    /// Returns `Ok(true)` if the first window's pane was dead and was
    /// respawned with `command` (using `working_dir` as the cwd). Returns
    /// `Ok(false)` if the pane is alive (no action taken) or the session
    /// does not exist. Returns `Err` if tmux respawn-pane fails.
    pub fn respawn_dead_pane(&self, working_dir: &str, command: Option<&str>) -> Result<bool> {
        if !self.exists() {
            return Ok(false);
        }
        if !self.is_pane_dead() {
            return Ok(false);
        }

        // `^.0` targets the first window's first pane: `^` picks the
        // first winlink (base-index agnostic), but the `.0` index
        // resolves only when `pane-base-index` is 0. Production pins
        // that on every session via `append_pane_base_index_args`
        // (see #488, #2231). The `-k` flag forces respawn past the
        // remembered exit status; without it tmux refuses to respawn.
        let target = format!("{}:^.0", self.name);
        let mut args: Vec<String> = vec![
            "respawn-pane".to_string(),
            "-k".to_string(),
            "-t".to_string(),
            target,
            "-c".to_string(),
            working_dir.to_string(),
        ];
        if let Some(cmd) = command {
            args.push(cmd.to_string());
        }

        let output = crate::tmux::tmux_command().args(&args).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to respawn dead pane: {}", stderr);
        }

        super::refresh_session_cache();
        Ok(true)
    }

    pub fn kill(&self) -> Result<()> {
        if !self.exists() {
            return Ok(());
        }

        // Kill the entire process tree first to ensure child processes are terminated.
        // This handles cases where tools like Claude spawn subprocesses that may
        // survive tmux's SIGHUP signal.
        if let Some(pane_pid) = self.get_pane_pid() {
            process::kill_process_tree(pane_pid);
        }

        super::utils::kill_session_if_present(&self.name)?;

        refresh_session_cache();

        Ok(())
    }

    pub fn rename(&self, new_name: &str) -> Result<()> {
        if !self.exists() {
            return Ok(());
        }

        let output = crate::tmux::tmux_command()
            .args(["rename-session", "-t", &self.name, new_name])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to rename tmux session: {}", stderr);
        }

        Ok(())
    }

    pub fn attach(&self) -> Result<()> {
        if !self.exists() {
            bail!("Session does not exist: {}", self.name);
        }

        if std::env::var("TMUX").is_ok() {
            let status = crate::tmux::tmux_command()
                .args(["switch-client", "-t", &self.name])
                .status()?;

            if !status.success() {
                // Fall back to attach-session if switch-client fails.
                // This handles cases where TMUX env var is inherited but we're
                // not actually inside a tmux client (e.g., terminal spawned
                // from within tmux via `open -a Terminal`).
                let status = crate::tmux::tmux_command()
                    .args(["attach-session", "-t", &self.name])
                    .status()?;

                if !status.success() {
                    let diag = self.diagnose_attach_failure();
                    bail!(
                        "Failed to attach to tmux session '{}' (exit {}): {}",
                        self.name,
                        status.code().unwrap_or(-1),
                        diag
                    );
                }
            }
        } else {
            let status = crate::tmux::tmux_command()
                .args(["attach-session", "-t", &self.name])
                .status()?;

            if !status.success() {
                let diag = self.diagnose_attach_failure();
                bail!(
                    "Failed to attach to tmux session '{}' (exit {}): {}",
                    self.name,
                    status.code().unwrap_or(-1),
                    diag
                );
            }
        }

        Ok(())
    }

    /// Collect diagnostic info after a failed attach attempt.
    fn diagnose_attach_failure(&self) -> String {
        let mut info = Vec::new();
        info.push(format!("exists={}", self.exists()));
        info.push(format!("pane_dead={}", self.is_pane_dead()));

        if let Ok(output) = crate::tmux::tmux_command()
            .args([
                "display-message",
                "-t",
                &self.name,
                "-p",
                "#{session_attached} #{pane_pid} #{pane_dead}",
            ])
            .output()
        {
            let msg = String::from_utf8_lossy(&output.stdout);
            info.push(format!("tmux_info={}", msg.trim()));
        }

        if let Ok(pane) = self.capture_pane(5) {
            let trimmed = pane.trim();
            if !trimmed.is_empty() {
                info.push(format!("pane_content={}", trimmed));
            }
        }

        info.join(", ")
    }

    pub fn capture_pane(&self, lines: usize) -> Result<String> {
        if !self.exists() {
            return Ok(String::new());
        }

        // Use `^.0` to target the first window's first pane regardless of
        // base-index or which pane is active.  See #435, #488.
        let target = format!("{}:^.0", self.name);
        let output = crate::tmux::tmux_command()
            .args([
                "capture-pane",
                "-t",
                &target,
                "-p",
                "-e",
                "-S",
                &format!("-{}", lines),
            ])
            .output()?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Ok(String::new())
        }
    }

    /// Capture the whole first window, panes composited, for the passive
    /// preview.
    ///
    /// [`capture_pane`](Self::capture_pane) shows `^.0` and nothing else, so a
    /// user who splits the window watches aoe go blind to everything but the
    /// agent's own pane. This reads every pane and lays them back out on the
    /// window grid. Input is untouched and still pinned to `^.0` (#435, #488):
    /// compositing is read-only, so the mis-targeted-keystroke class of bug
    /// that the pin exists to prevent cannot arise here.
    ///
    /// The single-pane case, which is almost every session, costs the same one
    /// fork as before: the pane count rides along as a header on the capture
    /// that was already being taken, and its bytes are returned verbatim
    /// (scrollback and all). Only a genuinely split window pays a second fork,
    /// and that one is chained so it stays a single `tmux` invocation no matter
    /// how many panes there are.
    ///
    /// A zoomed pane (`C-b z`) is treated as unsplit and takes the same
    /// single-pane path, because tmux reports zoomed panes at overlapping
    /// rectangles that the compositor cannot tile.
    ///
    /// Splits lose scrollback: panes have independent histories, so there is no
    /// coherent way to stack them, and the composite covers the visible window
    /// only. The preview's scroll offset clamps itself to the shorter capture,
    /// so this reads as "a split window doesn't scroll back" rather than
    /// misbehaving.
    pub fn capture_window_composited(&self, lines: usize) -> Result<String> {
        Ok(self.capture_window_composited_with_cursor(lines)?.0)
    }

    /// [`capture_window_composited`](Self::capture_window_composited) plus pane
    /// 0's cursor, for the live preview.
    ///
    /// Pane 0 owns the cursor because it is the pane that receives input, and
    /// tmux puts it at the window origin, so its coordinates index the
    /// composite untranslated. The probe targets `^.0` explicitly rather than
    /// the window, whose format fields would resolve against whichever pane the
    /// user happens to have selected.
    ///
    /// On a composite the cursor is rebased onto the window's dimensions: the
    /// renderer anchors it by `pane_height` against the painted line count,
    /// which is now the whole window rather than one pane.
    pub fn capture_window_composited_with_cursor(
        &self,
        lines: usize,
    ) -> Result<(String, Option<PaneCursor>)> {
        /// Gates the window-dimensions line. A chained `display-message` can
        /// silently produce nothing while the invocation still exits 0 (the
        /// same hazard `is_probe_line` guards in the vt seed path), and without
        /// a sentinel the capture's first row would be mistaken for the header
        /// and dropped from the fallback content.
        const WINDOW_SENTINEL: &str = "@@aoe-win@@";
        /// Gates the cursor line, for the same reason.
        const CURSOR_SENTINEL: &str = "@@aoe-cur@@";

        if !self.exists() {
            return Ok((String::new(), None));
        }

        let window = format!("{}:^", self.name);
        let pane0 = format!("{}:^.0", self.name);
        let output = crate::tmux::tmux_command()
            .args([
                "display-message",
                "-p",
                "-t",
                &window,
                "-F",
                &format!(
                    "{WINDOW_SENTINEL} #{{window_panes}} #{{window_width}} #{{window_height}} #{{window_zoomed_flag}}"
                ),
                ";",
                "display-message",
                "-p",
                "-t",
                &pane0,
                "-F",
                &format!("{CURSOR_SENTINEL} {CURSOR_FMT}"),
                ";",
                "capture-pane",
                "-t",
                &pane0,
                "-p",
                "-e",
                "-S",
                &format!("-{}", lines),
            ])
            .output()?;

        if !output.status.success() {
            return Ok((String::new(), None));
        }

        // Consume the sentinel-tagged preamble line by line; the first line
        // that carries neither sentinel is where the capture starts. Either
        // probe going missing costs only its own information, never a row of
        // pane content.
        let raw = String::from_utf8_lossy(&output.stdout);
        let mut rest: &str = &raw;
        let mut dims: Option<(u16, u16, u16)> = None;
        let mut zoomed = false;
        let mut cursor: Option<PaneCursor> = None;
        while let Some((line, tail)) = rest.split_once('\n') {
            if let Some(fields) = line.strip_prefix(WINDOW_SENTINEL) {
                let mut f = fields.split_whitespace();
                dims = match (f.next(), f.next(), f.next()) {
                    (Some(c), Some(w), Some(h)) => {
                        match (c.parse().ok(), w.parse().ok(), h.parse().ok()) {
                            (Some(c), Some(w), Some(h)) => Some((c, w, h)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                // Absent (older tmux, or a truncated line) reads as not zoomed,
                // which keeps the composite path rather than disabling it.
                zoomed = f.next().is_some_and(|z| z != "0");
            } else if let Some(fields) = line.strip_prefix(CURSOR_SENTINEL) {
                cursor = PaneCursor::parse(fields.trim());
            } else {
                break;
            }
            rest = tail;
        }
        let pane0_content = rest.to_string();

        let Some((count, window_width, window_height)) = dims else {
            return Ok((pane0_content, unreliable_position(cursor)));
        };
        if count <= 1 || window_width == 0 || window_height == 0 {
            return Ok((pane0_content, unreliable_position(cursor)));
        }
        // A zoomed pane (`C-b z`) keeps `window_panes` at its real count but
        // reports every pane at the window's full rectangle, so the panes
        // OVERLAP. The compositor's walk assumes a tiling, and handed overlap it
        // paints one pane and fills the rest of the row with border glyphs,
        // hiding the zoomed pane's content, which is the only thing the user is
        // looking at in tmux. Treat zoomed as unsplit: pane 0's bytes are already
        // in hand, scrollback included, which is what the preview showed before
        // compositing existed.
        if zoomed {
            return Ok((pane0_content, unreliable_position(cursor)));
        }

        // Any failure in the split path (fork error, unparseable layout) falls
        // back to the pane-0 bytes already in hand, so a composite that cannot
        // be built is never worse than the old single-pane preview.
        let Some(layout) = self.capture_window_layout(count) else {
            return Ok((pane0_content, unreliable_position(cursor)));
        };
        // The composite is the visible window with no scrollback, so the row a
        // single probe reported cannot have drifted underneath it, and the
        // position stands.
        let cursor = cursor.map(|mut c| {
            c.pane_height = layout.window_height;
            c.pane_width = layout.window_width;
            c.history_size = 0;
            // Rebasing the frame onto the window is what the renderer needs, but
            // it also erases the only record of how wide the input pane is, which
            // mouse forwarding maps into. Carry pane 0's extent alongside.
            c.composite_pane0 = layout.first_pane().map(|p| (p.width, p.height));
            c
        });
        Ok((layout.composite(), cursor))
    }

    /// Second fork of [`capture_window_composited`](Self::capture_window_composited):
    /// window dimensions plus geometry and visible capture for each of `count`
    /// panes, chained into one `tmux` invocation.
    ///
    /// Pane indices are contiguous from 0 within a window (tmux renumbers them
    /// as the layout changes, unlike window indices) and `pane-base-index` is
    /// forced to 0 when the session is created, so `^.0..^.{count-1}` addresses
    /// every pane without a prior `list-panes` round trip.
    ///
    /// Returned rather than composited on the spot so the live preview can
    /// cache a layout across frames and re-render only pane 0 from its VT grid
    /// (see [`WindowLayout::composite_with_first_pane_rows`]).
    pub(crate) fn capture_window_layout(&self, count: u16) -> Option<WindowLayout> {
        /// Marks the start of each pane's segment in the chained output. Pane
        /// content could in principle contain this line, which would split one
        /// pane's rows in two; the cost is a single garbled preview frame, and
        /// the string is unusual enough to make that a non-event.
        const SENTINEL: &str = "@@aoe-pane@@";
        /// Leading line carrying the window's own dimensions, so a cached
        /// layout is self-contained and needs no separate probe.
        const WINDOW_SENTINEL: &str = "@@aoe-win@@";

        let mut args: Vec<String> = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            format!("{}:^", self.name),
            "-F".to_string(),
            format!("{WINDOW_SENTINEL} #{{window_width}} #{{window_height}}"),
        ];
        for i in 0..count {
            let target = format!("{}:^.{}", self.name, i);
            args.push(";".to_string());
            args.extend([
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                target.clone(),
                "-F".to_string(),
                format!("{SENTINEL} #{{pane_left}} #{{pane_top}} #{{pane_width}} #{{pane_height}}"),
                ";".to_string(),
                "capture-pane".to_string(),
                "-t".to_string(),
                target,
                "-p".to_string(),
                "-e".to_string(),
            ]);
        }

        let output = crate::tmux::tmux_command().args(&args).output().ok()?;
        if !output.status.success() {
            return None;
        }

        let raw = String::from_utf8_lossy(&output.stdout);
        let (header, rest) = raw.split_once('\n')?;
        let dims = header.strip_prefix(WINDOW_SENTINEL)?;
        let mut fields = dims.split_whitespace();
        let window_width: u16 = fields.next().and_then(|f| f.parse().ok())?;
        let window_height: u16 = fields.next().and_then(|f| f.parse().ok())?;
        if window_width == 0 || window_height == 0 {
            return None;
        }

        let mut panes = parse_pane_segments(rest, SENTINEL);
        if panes.is_empty() {
            return None;
        }
        // Backstop for the zoom guard in `capture_window_composited_with_cursor`
        // and `probe_pane_count`: if any overlapping layout still reaches here,
        // keep the first pane of each overlapping set rather than handing the
        // compositor a non-tiling layout it would paint as border garbage. Pane 0
        // comes first, so the pane that survives is always the one receiving
        // input, and the frame degrades to "pane 0 plus empty space".
        let mut kept: Vec<CapturedPane> = Vec::with_capacity(panes.len());
        for pane in panes.drain(..) {
            if !kept.iter().any(|k| k.geom.overlaps(&pane.geom)) {
                kept.push(pane);
            }
        }
        let panes = kept;
        Some(WindowLayout {
            window_width,
            window_height,
            panes,
        })
    }

    /// Capture the pane's full scrollback (from session start) with wrapped
    /// lines joined (`-J`) and no escape sequences (`-e` omitted), for
    /// summarizing the first turn in smart-rename. Unlike
    /// [`capture_pane`](Self::capture_pane), which caps at the last N lines,
    /// this uses `-S -` so a first prompt that has scrolled up is still
    /// included.
    pub fn capture_pane_full(&self) -> Result<String> {
        if !self.exists() {
            return Ok(String::new());
        }
        let target = format!("{}:^.0", self.name);
        let output = crate::tmux::tmux_command()
            .args(["capture-pane", "-t", &target, "-p", "-J", "-S", "-"])
            .output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Ok(String::new())
        }
    }

    /// Capture the pane like [`capture_pane`](Self::capture_pane), but in the
    /// same `tmux` fork also query the cursor position + visibility, so the
    /// live-send preview can paint a real cursor without paying a second fork
    /// per capture cycle. Returns `None` for the cursor if the pane is gone
    /// or the header didn't parse, in which case the caller simply paints no
    /// cursor.
    ///
    /// The chained commands are NOT atomic: tmux processes pane output
    /// between them, so while an agent streams (scrolling the pane), the
    /// cursor/history read before the capture can describe a different
    /// screen than the captured content. A renderer that maps the cursor
    /// onto the content via `history + y` then paints the cursor on the
    /// wrong row, one row per scroll that slipped in (measured at ~100% of
    /// frames against a pane printing 50 lines/s). The probe therefore runs
    /// TWICE, before and after the capture, and the cursor is reported only
    /// when both probes agree; a raced frame paints content with no cursor,
    /// which beats painting it on the wrong row. At rest the first try
    /// agrees and the cursor never blinks.
    pub fn capture_pane_with_cursor(&self, lines: usize) -> Result<(String, Option<PaneCursor>)> {
        if !self.exists() {
            return Ok((String::new(), None));
        }

        let target = format!("{}:^.0", self.name);
        let start = format!("-{}", lines);
        const HEADER_FMT: &str = CURSOR_FMT;
        let output = crate::tmux::tmux_command()
            .args([
                "display-message",
                "-p",
                "-t",
                &target,
                "-F",
                HEADER_FMT,
                ";",
                "capture-pane",
                "-t",
                &target,
                "-p",
                "-e",
                "-S",
                &start,
                ";",
                "display-message",
                "-p",
                "-t",
                &target,
                "-F",
                HEADER_FMT,
            ])
            .output()?;

        if !output.status.success() {
            return Ok((String::new(), None));
        }

        let raw = String::from_utf8_lossy(&output.stdout);
        // First line: pre-capture cursor header. Last line: post-capture
        // header. Everything between is the verbatim `capture-pane` output
        // (same bytes the plain `capture_pane` path returns).
        let mut parts = raw.splitn(2, '\n');
        let cursor_line = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        let (content, after_line) = match rest.rfind('\n') {
            // `rest` ends with the trailing '\n' of the post-header line, so
            // search for the newline that PRECEDES it to split content from
            // the post-header.
            Some(_) => {
                let trimmed = rest.strip_suffix('\n').unwrap_or(rest);
                match trimmed.rfind('\n') {
                    Some(idx) => (&trimmed[..=idx], &trimmed[idx + 1..]),
                    // Single line: no content, just the post-header.
                    None => ("", trimmed),
                }
            }
            None => ("", rest),
        };
        let before = PaneCursor::parse(cursor_line);
        let after = PaneCursor::parse(after_line);
        Ok((content.to_string(), merge_cursor_probes(before, after)))
    }

    /// Deliver raw bytes to the session's active pane via `tmux send-keys
    /// -H`, one hex argument per byte, chunked so a large paste cannot
    /// overflow `execve` ARG_MAX (the same bound the TUI's live-send path
    /// uses; macOS caps total argv at 256KB and per-byte hex args burn it
    /// ~13x faster than the payload size). tmux injects the bytes in
    /// order, so a bracketed paste split across forks reassembles
    /// transparently on the agent's PTY. This is the web live view's
    /// input path: raw bytes from the browser (printables, CSI sequences,
    /// control bytes) all ride the same encoding.
    pub fn send_raw_bytes(&self, bytes: &[u8]) -> Result<()> {
        // `^.0` pins the first window's first pane, matching capture_pane:
        // a bare session name follows the ACTIVE pane, which would let
        // input land in a different pane than the one being captured.
        let target = format!("{}:^.0", self.name);
        for batch in raw_byte_batches(bytes) {
            let output = crate::tmux::tmux_command()
                .args(["send-keys", "-t", &target, "-H"])
                .args(&batch)
                .output()?;
            if !output.status.success() {
                anyhow::bail!(
                    "tmux send-keys -H exited non-zero for {} bytes",
                    bytes.len()
                );
            }
        }
        Ok(())
    }

    pub fn get_pane_pid(&self) -> Option<u32> {
        process::get_pane_pid(&self.name)
    }

    pub fn get_foreground_pid(&self) -> Option<u32> {
        let pane_pid = self.get_pane_pid()?;
        process::get_foreground_pid(pane_pid).or(Some(pane_pid))
    }

    pub fn detect_status(&self, tool: &str) -> Result<Status> {
        let content = self.capture_pane(50)?;
        Ok(super::status_detection::detect_status_from_content(
            &content, tool,
        ))
    }

    /// Send literal text to the session's first window pane, followed by Enter.
    /// Short single-line text is delivered via `send-keys -l`; multi-line or
    /// long payloads route through `paste-buffer -p` (bracketed paste) so the
    /// receiving agent ingests the whole block as a paste rather than
    /// submitting per line. See `send_keys_with_delay` for the threshold and
    /// `send_via_paste_buffer` for the bracketed-paste contract.
    pub fn send_keys(&self, text: &str) -> Result<()> {
        self.send_keys_with_delay(text, 0)
    }

    /// Like [`send_keys`](Self::send_keys), but waits `enter_delay_ms` between
    /// the literal text and the final Enter. Agents with paste-burst detection
    /// (e.g. Codex) swallow Enter keys that arrive within their burst window,
    /// treating them as newlines instead of submit. The delay lets the
    /// suppression window expire before Enter is sent.
    pub fn send_keys_with_delay(&self, text: &str, enter_delay_ms: u64) -> Result<()> {
        if !self.exists() {
            bail!("Session does not exist: {}", self.name);
        }

        let target = format!("{}:^.0", self.name);
        let byte_len = text.len();
        let line_count = text.lines().count();
        let max_line = text.lines().map(str::len).max().unwrap_or(0);

        // Non-trivial or multi-line messages go through the tmux paste-buffer
        // path (load-buffer over stdin, then paste-buffer with bracketed-paste
        // markers). The per-line `send-keys -l` + ESC+CR path encodes
        // newlines as Shift+Enter, which is brittle compared to the
        // bracketed-paste contract claude-code (and most agents in raw mode)
        // are designed to ingest.
        //
        // The threshold is intentionally small: bracketed paste is also what
        // prevents the receiving agent's input-burst detector from treating
        // the trailing Enter as part of the keystroke stream and inserting a
        // newline instead of submitting. Empirically, on Mosh sessions
        // (bracketed-paste stripped end-to-end) a single-line ~365-byte
        // VoiceInk dictation that took the `send-keys -l` path was followed
        // by `tmux send-keys Enter` at 0ms and the agent rendered the text
        // but never submitted, because the Enter arrived inside the burst
        // window. Routing anything beyond a handful of characters through
        // the bracketed-paste path frames it as a paste, after which the
        // trailing Enter reliably submits. See gemini-cli#26114 for
        // independent confirmation that claude-code handles paste correctly
        // only when bracketed-paste markers are present.
        const PASTE_BYTE_THRESHOLD: usize = 16;
        let use_paste_buffer = byte_len >= PASTE_BYTE_THRESHOLD || text.contains('\n');

        tracing::debug!(target: "tmux.command",
            "send_keys_with_delay: bytes={} lines={} max_line={} use_paste_buffer={} target={}",
            byte_len,
            line_count,
            max_line,
            use_paste_buffer,
            target
        );

        if use_paste_buffer {
            Self::send_via_paste_buffer(&target, text)?;
        } else {
            // `--` ends option parsing so lines beginning with `-` (markdown
            // bullets, CLI flags in prompts) are not misread as tmux flags.
            Self::tmux_send(&target, &["-l", "--", text])?;
        }

        if enter_delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(enter_delay_ms));
        }

        // Enter to submit
        Self::tmux_send(&target, &["Enter"])?;

        Ok(())
    }

    /// Sends exactly the given token sequence to the pane, in order, with no
    /// implicit trailing key. Unlike [`send_keys_with_delay`](Self::send_keys_with_delay),
    /// which always appends a submitting `Enter`, the caller's token list
    /// fully controls what reaches the pane: a bare menu-digit selection
    /// needs zero `Enter`s, while a multi-step button navigation needs
    /// exactly as many as its shape requires. Used to answer an agent CLI's
    /// own interactive permission prompt; see
    /// [`crate::agents::PermissionResponse`].
    pub fn send_key_tokens(&self, tokens: &[crate::agents::KeyToken]) -> Result<()> {
        if !self.exists() {
            bail!("Session does not exist: {}", self.name);
        }

        let target = format!("{}:^.0", self.name);
        for token in tokens {
            match token {
                crate::agents::KeyToken::Literal(text) => {
                    Self::tmux_send(&target, &["-l", "--", text])?;
                }
                crate::agents::KeyToken::Named(name) => {
                    Self::tmux_send(&target, &[name])?;
                }
            }
        }

        Ok(())
    }

    /// Restore automatic window sizing after live-send forced a manual
    /// size. tmux's `resize-window -x -y` silently switches the window-
    /// size option to `manual`, so without this call a later
    /// `attach-session` from a full-size terminal would keep the window
    /// at the small preview dimensions live-send left behind. Re-setting
    /// the option to `latest` is the documented escape hatch and matches
    /// the policy `append_window_size_args` installs at session create.
    /// Best-effort: failures (session gone, tmux ENOENT) are swallowed
    /// so a stuck pane never blocks the user's exit from live mode.
    pub fn reset_size_to_latest_client(&self) {
        if !self.exists() {
            return;
        }
        let _ = crate::tmux::tmux_command()
            .args(["set-option", "-t", &self.name, "window-size", "latest"])
            .output();
    }

    /// Resize the session's first window so its pane's visible content area
    /// becomes `cols` x `rows`. Best-effort: a missing session or a tmux ENOENT
    /// is swallowed so a transient failure never blocks a render.
    ///
    /// Every caller (the web live view, the mobile live view, the TUI's passive
    /// preview sync) works in pane/content geometry, not tmux window geometry:
    /// they render the pane, not the tmux status bar. tmux `resize-window` sizes
    /// the *window*, and vertical chrome (the status bar) shrinks the pane below
    /// it, so a naive `resize-window -y rows` yields a `rows - chrome` pane and
    /// the live owner loop then re-asserts forever against a target it can never
    /// reach (#2766). We measure the chrome live and add it back, so the pane
    /// lands at exactly `rows`. Cols need no adjustment: a single pane spans the
    /// full window width, and the status bar is horizontal.
    ///
    /// Also used to keep a detached agent's pane sized to the visible preview
    /// area: a full-screen agent is sized to whatever terminal it was last
    /// attached from, so without this it renders taller than the preview window
    /// and the bottom-anchored capture clips the top rows (worse when the info
    /// header steals rows). Mirrors what live-send does through its worker.
    ///
    /// NOTE: tmux's `resize-window -x -y` silently flips the window-size option
    /// to `manual`, so any later `attach-session` must call
    /// [`reset_size_to_latest_client`](Self::reset_size_to_latest_client) first
    /// or the window stays pinned at these preview dimensions.
    pub fn resize_window(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || !self.exists() {
            return;
        }
        // Query the same window/pane the capture streams (`:^.0`), so the
        // measured chrome matches the pane whose height the owner loop checks.
        let pane_target = format!("{}:^.0", self.name);
        let window_rows = self
            .pane_chrome_rows(&pane_target)
            .map(|chrome| rows.saturating_add(chrome))
            .unwrap_or(rows);
        let window_target = format!("{}:^", self.name);
        let _ = crate::tmux::tmux_command()
            .args([
                "resize-window",
                "-t",
                &window_target,
                "-x",
                &cols.to_string(),
                "-y",
                &window_rows.to_string(),
            ])
            .output();
    }

    /// Read the live vertical chrome (status-bar rows) for `pane_target` from
    /// tmux. `None` when the geometry can't be read; callers then size the
    /// window with no chrome adjustment (the pre-#2766 behavior).
    fn pane_chrome_rows(&self, pane_target: &str) -> Option<u16> {
        let output = crate::tmux::tmux_command()
            .args([
                "display-message",
                "-p",
                "-t",
                pane_target,
                "-F",
                "#{window_height} #{pane_height}",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&output.stdout);
        let mut fields = line.split_whitespace();
        let window_height: u16 = fields.next()?.parse().ok()?;
        let pane_height: u16 = fields.next()?.parse().ok()?;
        Some(chrome_rows(window_height, pane_height))
    }

    /// Try to become the sole size owner of this session. Returns true if we
    /// hold the lock afterward.
    ///
    /// One tmux window has one size, but three writers resize it (the web PTY
    /// attach, the mobile capture viewer, and the TUI's preview sync), each
    /// living in a different process. The lock lives in tmux user options so
    /// every process sees the same owner and only the owner calls
    /// [`resize_window`](Self::resize_window); non-owners render best-effort.
    ///
    /// Steals the lock when the current holder's heartbeat is older than
    /// `ttl`, so a crashed or disconnected owner self-heals. The confirm-read
    /// after the write resolves the race where two processes both observe a
    /// vacant lock and both write: the last write wins and only its author
    /// reads its own id back.
    pub fn claim_size_owner(&self, owner_id: &str, ttl: Duration) -> bool {
        self.claim_owner_at(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, owner_id, ttl)
    }

    /// Bump the heartbeat iff we still own the lock. Returns false when
    /// ownership was lost (another client took over), so the caller can demote
    /// itself. Cheap enough to call on each capture/render tick.
    pub fn refresh_size_owner(&self, owner_id: &str) -> bool {
        self.refresh_owner_at(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, owner_id)
    }

    /// The shared claim protocol behind the size- and VT-owner locks:
    /// claimable when vacant, already ours, or stale past `ttl`; the
    /// confirm-read after the write resolves the race where two processes
    /// both observe a vacant lock and both write (last write wins and only
    /// its author reads its own id back).
    fn claim_owner_at(&self, opt: &str, hb_opt: &str, owner_id: &str, ttl: Duration) -> bool {
        if !self.exists() {
            return false;
        }
        // Heartbeats are compared across processes, so this must be wall-clock
        // (crate::util::now_ms), never a per-process monotonic clock.
        let now = now_ms();
        let claimable = match self.owner_at(opt, hb_opt) {
            None => true,
            Some((id, _)) if id == owner_id => true,
            Some((_, hb)) => now.saturating_sub(hb) > ttl.as_millis() as u64,
        };
        if !claimable {
            return false;
        }
        self.set_user_option(opt, owner_id);
        self.set_user_option(hb_opt, &now.to_string());
        matches!(self.owner_at(opt, hb_opt), Some((id, _)) if id == owner_id)
    }

    fn refresh_owner_at(&self, opt: &str, hb_opt: &str, owner_id: &str) -> bool {
        match self.owner_at(opt, hb_opt) {
            Some((id, _)) if id == owner_id => {
                self.set_user_option(hb_opt, &now_ms().to_string());
                true
            }
            _ => false,
        }
    }

    fn owner_at(&self, opt: &str, hb_opt: &str) -> Option<(String, u64)> {
        let id = self.show_user_option(opt)?;
        let hb = self
            .show_user_option(hb_opt)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Some((id, hb))
    }

    /// Try to become the pane's sole `pipe-pane` owner. Same protocol as
    /// [`claim_size_owner`](Self::claim_size_owner) over a separate option
    /// pair; see `VT_OWNER_OPT` for why the pipe needs an owner at all.
    pub fn claim_vt_owner(&self, owner_id: &str, ttl: Duration) -> bool {
        self.claim_owner_at(VT_OWNER_OPT, VT_OWNER_HB_OPT, owner_id, ttl)
    }

    /// Bump the VT-owner heartbeat iff we still hold the lock. Rate-limited
    /// by the caller (the channel's sample loop), not here.
    pub fn refresh_vt_owner(&self, owner_id: &str) -> bool {
        self.refresh_owner_at(VT_OWNER_OPT, VT_OWNER_HB_OPT, owner_id)
    }

    /// Release the VT-owner lock iff we hold it, so another viewer can arm
    /// immediately instead of waiting out the TTL.
    pub fn release_vt_owner(&self, owner_id: &str) {
        if matches!(self.owner_at(VT_OWNER_OPT, VT_OWNER_HB_OPT), Some((id, _)) if id == owner_id) {
            self.unset_user_option(VT_OWNER_OPT);
            self.unset_user_option(VT_OWNER_HB_OPT);
        }
    }

    /// Force ownership to `owner_id`, even over a live holder. Used by the
    /// explicit "take over" action: a user tap is an intentional steal, not
    /// the passive flap the heartbeat guards against.
    pub fn steal_size_owner(&self, owner_id: &str) -> bool {
        if !self.exists() {
            return false;
        }
        self.set_user_option(SIZE_OWNER_OPT, owner_id);
        self.set_user_option(SIZE_OWNER_HB_OPT, &now_ms().to_string());
        matches!(self.size_owner(), Some((id, _)) if id == owner_id)
    }

    /// Resize the window iff `owner_id` still holds the size-owner lock,
    /// verifying ownership in the same call. Returns whether we still own it.
    ///
    /// This is the only resize entry point loops with a cached "am I owner"
    /// flag may use: a local flag is stale for up to a heartbeat after another
    /// client steals the lock, and an unverified resize in that window stomps
    /// the new owner's grid (the flap this lock exists to kill). Re-reading
    /// the lock here closes that window; the caller demotes itself on false.
    pub fn resize_window_if_owner(&self, owner_id: &str, cols: u16, rows: u16) -> bool {
        match self.size_owner() {
            Some((id, _)) if id == owner_id => {
                self.resize_window(cols, rows);
                true
            }
            _ => false,
        }
    }

    /// Whether at least one tmux client is attached to this session, from
    /// `#{session_attached}` (the attached client count, per session).
    ///
    /// The TUI's passive preview resize checks this so it stops sizing a
    /// session the user just attached to. `has_active_size_owner` does not
    /// cover that case: it only sees surfaces that claim the size-owner lock
    /// (the web/mobile live views), and a plain `switch-client` attach claims
    /// nothing, so the passive resize shrank the window back to the preview
    /// pane's dimensions right after the attach (#3071).
    ///
    /// Best-effort: a tmux call that fails to spawn, exits non-zero, or prints
    /// something unparseable reports "not attached". The caller leaves its
    /// dedup unset when it skips, so a transient glitch costs one poll
    /// interval of a clipped preview rather than wedging the resize forever.
    pub fn is_attached(&self) -> bool {
        let out = crate::tmux::tmux_command()
            .args([
                "display-message",
                "-t",
                &self.name,
                "-p",
                "#{session_attached}",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout);
                s.trim().parse::<u32>().unwrap_or(0) > 0
            }
            _ => false,
        }
    }

    /// Whether a non-stale size owner currently holds the lock. A passive
    /// writer (the TUI's detached preview sync) checks this to defer to an
    /// active owner without claiming the lock itself.
    pub fn has_active_size_owner(&self) -> bool {
        match self.size_owner() {
            Some((_, hb)) => now_ms().saturating_sub(hb) <= SIZE_OWNER_TTL.as_millis() as u64,
            None => false,
        }
    }

    /// Read the current size owner and its last heartbeat (unix millis), if a
    /// lock is held.
    pub fn size_owner(&self) -> Option<(String, u64)> {
        self.owner_at(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT)
    }

    /// Release the lock iff we own it. Restores `window-size latest` once the
    /// lock is vacant so a later out-of-band `tmux attach` from a real terminal
    /// sizes the window to itself instead of staying pinned at our grid.
    pub fn release_size_owner(&self, owner_id: &str) {
        if let Some((id, _)) = self.size_owner() {
            if id == owner_id {
                self.unset_user_option(SIZE_OWNER_OPT);
                self.unset_user_option(SIZE_OWNER_HB_OPT);
                self.reset_size_to_latest_client();
            }
        }
    }

    fn show_user_option(&self, opt: &str) -> Option<String> {
        let out = crate::tmux::tmux_command()
            .args(["show-options", "-v", "-t", &self.name, opt])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    fn set_user_option(&self, opt: &str, value: &str) {
        let _ = crate::tmux::tmux_command()
            .args(["set-option", "-t", &self.name, opt, value])
            .output();
    }

    fn unset_user_option(&self, opt: &str) {
        let _ = crate::tmux::tmux_command()
            .args(["set-option", "-u", "-t", &self.name, opt])
            .output();
    }

    /// Deliver `text` to `target` via tmux's load-buffer + paste-buffer.
    /// Buffer names are scoped by pid + a per-call counter so concurrent
    /// senders (and retries) cannot clobber each other. `-p` enables
    /// bracketed-paste markers when the receiving pane has DECSET 2004 set;
    /// `-d` deletes the buffer after the paste. If paste-buffer fails after
    /// load-buffer succeeded we issue an explicit `delete-buffer` so a
    /// partial failure cannot leak a buffer.
    ///
    /// Bracketed-paste assumption: this replaces the old per-line `send-keys
    /// -l` + `ESC+CR` (Shift+Enter) encoding. The old path worked against any
    /// pane regardless of paste-mode support. The new path relies on the
    /// receiving agent enabling DECSET 2004 (claude-code, codex, opencode,
    /// gemini, and most modern TUI agent CLIs do). For panes that do *not*
    /// enable bracketed paste (raw shells, simple REPLs), embedded newlines
    /// will arrive as literal CRs and submit per line. If a future agent
    /// integration hits this, the fallback is to short-circuit the
    /// `use_paste_buffer` branch above for that agent and keep the per-line
    /// Shift+Enter path.
    fn send_via_paste_buffer(target: &str, text: &str) -> Result<()> {
        static SEND_COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = SEND_COUNTER.fetch_add(1, Ordering::Relaxed);
        let buf_name = format!("aoe-send-{}-{}", std::process::id(), seq);

        let mut child = crate::tmux::tmux_command()
            .args(["load-buffer", "-b", &buf_name, "-"])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if !status.success() {
            bail!("tmux load-buffer failed (status={:?})", status.code());
        }

        let output = crate::tmux::tmux_command()
            .args(["paste-buffer", "-d", "-p", "-b", &buf_name, "-t", target])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // paste-buffer's `-d` only deletes on success; on failure the
            // buffer survives, so clean it up explicitly. Ignore errors
            // from the cleanup so the original failure isn't masked.
            let _ = crate::tmux::tmux_command()
                .args(["delete-buffer", "-b", &buf_name])
                .output();
            bail!("tmux paste-buffer failed: {}", stderr);
        }

        Ok(())
    }

    fn tmux_send(target: &str, args: &[&str]) -> Result<()> {
        let output = crate::tmux::tmux_command()
            .arg("send-keys")
            .args(["-t", target])
            .args(args)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to send keys: {}", stderr);
        }

        Ok(())
    }
}

fn sanitize_session_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(20)
        .collect()
}

/// Max bytes per `send-keys -H` fork. Each byte becomes one two-char
/// argv entry, so a bound well under ARG_MAX keeps the spawn safe on
/// every platform (macOS caps argv+envp at 256KB). Matches the TUI
/// live-send chunking bound.
const MAX_RAW_BYTES_PER_SEND: usize = 4096;

/// Split a raw byte payload into per-fork hex argument batches for
/// [`Session::send_raw_bytes`]. Pure so the chunk bound and byte order
/// are unit-testable without tmux.
fn raw_byte_batches(bytes: &[u8]) -> Vec<Vec<String>> {
    bytes
        .chunks(MAX_RAW_BYTES_PER_SEND)
        .map(|chunk| chunk.iter().map(|b| format!("{:02x}", b)).collect())
        .collect()
}

/// Build the argument list for tmux new-session command. Shared by the
/// agent session and the paired/container terminal sessions (their
/// invocations are identical; only the session-name prefix differs).
/// Extracted for testability.
pub(crate) fn build_create_args(
    session_name: &str,
    working_dir: &str,
    env: &[(&str, &str)],
    command: Option<&str>,
    size: Option<(u16, u16)>,
) -> Vec<String> {
    let mut args = vec![
        "new-session".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        session_name.to_string(),
        "-c".to_string(),
        working_dir.to_string(),
    ];

    // Explicit per-session environment (`-e KEY=VAL`). `new-session -e`
    // requires tmux 3.2+; aoe already assumes newer tmux elsewhere (clipboard
    // passthrough needs 3.3, the VT channel 3.4), so no extra gate is added.
    // Set so a pane never inherits a stale value from the shared tmux server's
    // frozen base environment; see the host-terminal call site for why.
    for (key, value) in env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }

    if let Some((width, height)) = size {
        args.push("-x".to_string());
        args.push(width.to_string());
        args.push("-y".to_string());
        args.push(height.to_string());
    }

    if let Some(cmd) = command {
        args.push(cmd.to_string());
    }

    args
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::TmuxTestSession;
    use super::*;

    /// Helper: check if tmux is available for tests that need it
    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Create a detached session for the composite tests, applying the guards
    /// the rest of this module treats as mandatory:
    ///
    /// * `pane-base-index 0` chained into the create, so the `^.0` targets
    ///   these paths use resolve on a host that sets `pane-base-index 1`
    ///   globally (#488, #2231).
    /// * [`refresh_session_cache`] afterwards, because every capture entry point
    ///   is `exists()`-gated and a cache refreshed concurrently by another test
    ///   would make the call a silent `Ok("")` rather than a visible failure.
    fn start_composite_session(name: &str, cols: u16, rows: u16, cmd: &str) -> Session {
        let status = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
                cmd,
                ";",
                "set-option",
                "-t",
                name,
                "pane-base-index",
                "0",
            ])
            .status()
            .expect("tmux new-session");
        assert!(status.success(), "failed to create {name}");
        refresh_session_cache();
        Session::from_name(name)
    }

    /// Split `session` horizontally and refresh the cache, mirroring
    /// [`start_composite_session`]'s guards for the second pane.
    fn split_composite_session(session: &Session, cmd: &str) {
        let status = crate::tmux::tmux_command()
            .args(["split-window", "-h", "-t", &session.name, cmd])
            .status()
            .expect("tmux split-window");
        assert!(status.success(), "failed to split {}", session.name);
        refresh_session_cache();
    }

    /// Poll until the pane has painted `needle`. A fixed sleep is flaky under
    /// parallel suite load: the shell must spawn and the command run before a
    /// capture sees anything. Mirrors
    /// `capture_pane_with_cursor_returns_content_and_cursor`.
    fn wait_for_pane_text(session: &Session, needle: &str) {
        wait_for_text(session, needle, "pane", |s| s.capture_pane(20));
    }

    /// Poll until the composited capture contains `needle`, for the panes a
    /// plain `capture_pane` cannot see.
    fn wait_for_composite_text(session: &Session, needle: &str) {
        wait_for_text(session, needle, "composite", |s| {
            s.capture_window_composited(20)
        });
    }

    /// Shared poll loop. Reports the LAST OBSERVED capture on timeout rather
    /// than taking a fresh one, which is the thing you want when this trips in
    /// CI: a re-capture at panic time can show different content than the poll
    /// ever saw, which sends the reader chasing the wrong thing.
    fn wait_for_text(
        session: &Session,
        needle: &str,
        what: &str,
        capture: impl Fn(&Session) -> Result<String>,
    ) {
        let mut last = None;
        for _ in 0..50 {
            let seen = capture(session).unwrap_or_default();
            if seen.contains(needle) {
                return;
            }
            last = Some(seen);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!(
            "{what} for {} never painted {needle:?}; last seen: {last:?}",
            session.name
        );
    }

    #[test]
    fn raw_byte_batches_chunks_and_preserves_order() {
        let payload: Vec<u8> = (0..=255u8)
            .cycle()
            .take(MAX_RAW_BYTES_PER_SEND + 10)
            .collect();
        let batches = raw_byte_batches(&payload);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), MAX_RAW_BYTES_PER_SEND);
        assert_eq!(batches[1].len(), 10);
        assert_eq!(batches[0][0], "00");
        assert_eq!(batches[0][255], "ff");
        // Last byte of the payload survives in order at the tail.
        let last = payload[payload.len() - 1];
        assert_eq!(batches[1][9], format!("{:02x}", last));
    }

    #[test]
    fn raw_byte_batches_empty_payload_sends_nothing() {
        assert!(raw_byte_batches(&[]).is_empty());
    }

    #[test]
    fn chrome_rows_accounts_for_status_bar_and_ignores_splits() {
        // #2766: the reporter's tmux yields a pane one row shorter than the
        // window (status bar), so a window sized to `rows` leaves a `rows - 1`
        // pane and the owner loop re-asserts forever. chrome=1 here lets the
        // caller size the window to rows+1 and land the pane at `rows`.
        assert_eq!(chrome_rows(67, 66), 1, "one status row");
        // No status bar (or a tmux that doesn't reserve the row when detached):
        // window == pane, chrome 0, pre-#2766 behavior preserved.
        assert_eq!(chrome_rows(66, 66), 0, "no chrome");
        // Multi-line status bar.
        assert_eq!(chrome_rows(68, 66), 2, "two status rows");
        assert_eq!(chrome_rows(71, 66), 5, "max plausible chrome");
        // A large delta is a multi-pane split, not chrome: resolve to 0 rather
        // than balloon the window chasing an unreachable pane size.
        assert_eq!(chrome_rows(40, 18), 0, "split layout is not chrome");
        // Degenerate: pane taller than window can't underflow.
        assert_eq!(chrome_rows(10, 20), 0, "saturating, no panic");
    }

    #[test]
    fn pane_segments_split_the_chained_capture_by_sentinel() {
        // Shape of the chained fork's stdout: a geometry line per pane, each
        // followed by that pane's rows.
        let raw = "@@s@@ 0 0 6 2\nleft1\nleft2\n@@s@@ 7 0 6 2\nright1\nright2\n";
        let panes = parse_pane_segments(raw, "@@s@@");
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].geom.left, 0);
        assert_eq!(panes[1].geom.left, 7);
        // Rows come back padded to the pane's width, which is what lets the
        // compositor concatenate them without measuring.
        assert_eq!(panes[0].rows.len(), 2);
        assert!(panes[0].rows[0].contains("left1"));
        assert!(panes[1].rows[1].contains("right2"));
    }

    #[test]
    fn pane_segments_drop_a_pane_with_unparseable_geometry() {
        // A bad geometry line must not push its rows onto the next pane's
        // rectangle; the pane is dropped and the rest still parse.
        let raw = "@@s@@ bogus\norphan\n@@s@@ 0 0 4 1\nkeep\n";
        let panes = parse_pane_segments(raw, "@@s@@");
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].geom.width, 4);
        assert!(panes[0].rows[0].contains("keep"));
    }

    #[test]
    fn pane_segments_are_empty_when_no_sentinel_appears() {
        // A tmux that emitted nothing recognisable must yield no panes, so the
        // caller falls back to its already-captured pane-0 bytes.
        assert!(parse_pane_segments("just some output\n", "@@s@@").is_empty());
    }

    #[test]
    fn raw_byte_batches_large_paste_roundtrips_in_order() {
        // Regression for the silently-dropped large paste (#1942-era
        // live-send bug, now shared with the web live view): a ~100 KB
        // bracketed paste encoded one hex arg per byte overflows execve
        // ARG_MAX in a single fork. Verify it splits, every batch stays
        // under the bound, and the bytes reconstruct in order.
        let payload: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let batches = raw_byte_batches(&payload);
        assert!(batches.len() > 1);
        for batch in &batches {
            assert!(batch.len() <= MAX_RAW_BYTES_PER_SEND);
        }
        let roundtrip: Vec<u8> = batches
            .iter()
            .flatten()
            .map(|h| u8::from_str_radix(h, 16).unwrap())
            .collect();
        assert_eq!(roundtrip, payload);
    }

    #[test]
    fn pane_cursor_parses_format_line() {
        let c = PaneCursor::parse("3 2 1 24 120 74 1 1 1 1").expect("parses");
        assert_eq!(
            c,
            PaneCursor {
                x: 3,
                y: 2,
                visible: true,
                pane_height: 24,
                history_size: 120,
                pane_width: 74,
                alternate_on: true,
                mouse_tracking: true,
                mouse_sgr: true,
                mouse_all: true,
                position_reliable: true,
                composite_pane0: None,
            }
        );
        // Legacy mouse (tracking on, SGR off) parses with mouse_sgr false.
        let c = PaneCursor::parse("3 2 1 24 120 74 1 1 0 0").expect("parses");
        assert!(c.mouse_tracking);
        assert!(!c.mouse_sgr);
        assert!(!c.mouse_all);
        // Button-only tracking (1000/1002): any + SGR set, all-motion off.
        let c = PaneCursor::parse("3 2 1 24 120 74 1 1 1 0").expect("parses");
        assert!(c.mouse_tracking && c.mouse_sgr);
        assert!(!c.mouse_all);
        // The six-field (pre-alternate/mouse) line still parses, the new
        // flags defaulting to false.
        let c = PaneCursor::parse("3 2 1 24 120 74").expect("parses");
        assert!(!c.alternate_on);
        assert!(!c.mouse_tracking);
        assert!(!c.mouse_sgr);
        assert!(!c.mouse_all);
        // Four-field (pre-history) lines still parse, trailing fields 0.
        let c = PaneCursor::parse("3 2 0 24").expect("parses");
        assert!(!c.visible);
        assert_eq!(c.history_size, 0);
        assert_eq!(c.pane_width, 0);
        assert!(!c.alternate_on);
        assert!(!c.mouse_tracking);
        assert!(!c.mouse_sgr);
        // cursor_flag 0 => hidden.
        assert!(!PaneCursor::parse("0 0 0 10").unwrap().visible);
        // Garbage / short input yields None rather than a bogus cursor.
        assert!(PaneCursor::parse("").is_none());
        assert!(PaneCursor::parse("1 2 3").is_none());
        assert!(PaneCursor::parse("a b c d").is_none());
        // A freshly parsed probe trusts its own position.
        assert!(
            PaneCursor::parse("3 2 1 24 120 74 1 1 1")
                .unwrap()
                .position_reliable
        );
    }

    #[test]
    fn merge_cursor_probes_stable_mapping_keeps_after_and_trusts_position() {
        // Cursor moved (x/y) but the vertical mapping (history_size,
        // pane_height) held: the post-capture probe wins and is trusted.
        let before = PaneCursor::parse("3 2 1 24 120 80 1 1 1").unwrap();
        let after = PaneCursor::parse("5 4 1 24 120 80 1 1 1").unwrap();
        let merged = merge_cursor_probes(Some(before), Some(after)).expect("both probes => Some");
        assert_eq!((merged.x, merged.y), (5, 4));
        assert!(merged.position_reliable);
    }

    #[test]
    fn merge_cursor_probes_drift_keeps_modes_but_drops_position_trust() {
        // history_size changed mid-capture (the pane scrolled): keep the mode
        // flags so the wheel forward still works while the agent streams, but
        // mark the row untrustworthy so the render won't paint on it.
        let before = PaneCursor::parse("3 2 1 24 120 80 1 1 1").unwrap();
        let after = PaneCursor::parse("3 2 1 24 137 80 1 1 1").unwrap();
        let merged = merge_cursor_probes(Some(before), Some(after)).expect("both probes => Some");
        assert!(!merged.position_reliable);
        assert!(merged.alternate_on && merged.mouse_tracking && merged.mouse_sgr);

        // pane_height change (resize mid-capture) is the other vertical-drift
        // trigger and likewise drops position trust.
        let before = PaneCursor::parse("3 2 1 24 120 80 1 0 0").unwrap();
        let after = PaneCursor::parse("3 2 1 30 120 80 1 0 0").unwrap();
        let merged = merge_cursor_probes(Some(before), Some(after)).expect("both probes => Some");
        assert!(!merged.position_reliable);
    }

    #[test]
    fn merge_cursor_probes_none_when_either_probe_missing() {
        let c = PaneCursor::parse("3 2 1 24 120 80 1 1 1").unwrap();
        assert!(merge_cursor_probes(None, Some(c)).is_none());
        assert!(merge_cursor_probes(Some(c), None).is_none());
        assert!(merge_cursor_probes(None, None).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn capture_with_cursor_stays_consistent_under_streaming_load() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = TmuxTestSession::new("aoe_test_race");
        // A pane that scrolls as fast as tmux can ingest. The trailing
        // `set-option pane-base-index 0` chain mirrors `append_pane_base_index_args`
        // so `^.0` resolves on hosts with `pane-base-index 1` set globally (see #2231).
        let out = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                guard.name(),
                "-x",
                "80",
                "-y",
                "24",
                "bash -c 'i=0; while true; do echo line-$((i++)); done'",
                ";",
                "set-option",
                "-t",
                guard.name(),
                "pane-base-index",
                "0",
            ])
            .output()
            .expect("tmux new-session");
        assert!(out.status.success());
        refresh_session_cache();
        let session = Session::from_name(guard.name());
        std::thread::sleep(Duration::from_millis(300));

        // tmux dispatches the chained probe/capture/probe in one event-loop
        // turn, so locally every frame is consistent and the suppression
        // never fires; the guard exists for loaded/remote tmux servers
        // where output processing can interleave. Under load the call must
        // never error, and a reported cursor must always have matching
        // probes by construction. (The idle-pane Some-cursor case is
        // covered by capture_pane_with_cursor_returns_content_and_cursor.)
        for _ in 0..30 {
            let (content, _cursor) = session
                .capture_pane_with_cursor(50)
                .expect("capture should not error under load");
            assert!(!content.is_empty(), "streaming pane captures content");
        }
    }

    #[test]
    #[serial_test::serial]
    fn size_owner_lock_claims_rejects_steals_and_releases() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = TmuxTestSession::new("aoe_test_owner");
        let out = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                guard.name(),
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
            ])
            .output()
            .expect("tmux new-session");
        assert!(out.status.success());
        // The session was created behind the existence cache's back; an
        // earlier test may have warmed the cache without it, which would
        // make every exists()-guarded lock call a false no-op.
        refresh_session_cache();
        let session = Session::from_name(guard.name());

        // Vacant -> first claimer wins and is recorded.
        assert!(session.claim_size_owner("a", Duration::from_secs(10)));
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("a".to_string())
        );
        // Re-claiming as the same owner is idempotent (stays true).
        assert!(session.claim_size_owner("a", Duration::from_secs(10)));

        // A different client cannot claim while the owner's heartbeat is fresh.
        assert!(!session.claim_size_owner("b", Duration::from_secs(10)));
        assert!(session.refresh_size_owner("a"));
        assert!(!session.refresh_size_owner("b"));

        // A stale heartbeat is stealable through the normal claim path.
        std::thread::sleep(Duration::from_millis(5));
        assert!(session.claim_size_owner("c", Duration::from_millis(1)));
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("c".to_string())
        );

        // An explicit take-over steals even a fresh lock.
        assert!(session.steal_size_owner("d"));
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("d".to_string())
        );

        // A non-owner release is a no-op; the owner's release clears the lock.
        session.release_size_owner("not-d");
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("d".to_string())
        );
        session.release_size_owner("d");
        assert!(session.size_owner().is_none());
    }

    #[test]
    #[serial_test::serial]
    fn vt_owner_lock_claims_rejects_and_releases_independently() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = TmuxTestSession::new("aoe_test_vt_owner");
        let out = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                guard.name(),
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
            ])
            .output()
            .expect("tmux new-session");
        assert!(out.status.success());
        refresh_session_cache();
        let session = Session::from_name(guard.name());

        // Same claim protocol as the size owner: vacant -> first claimer
        // wins, idempotent re-claim, fresh lock rejects others, stale lock
        // steals through the normal claim path.
        assert!(session.claim_vt_owner("pid-1", Duration::from_secs(10)));
        assert!(session.claim_vt_owner("pid-1", Duration::from_secs(10)));
        assert!(!session.claim_vt_owner("pid-2", Duration::from_secs(10)));
        assert!(session.refresh_vt_owner("pid-1"));
        assert!(!session.refresh_vt_owner("pid-2"));
        std::thread::sleep(Duration::from_millis(5));
        assert!(session.claim_vt_owner("pid-3", Duration::from_millis(1)));

        // The two locks are independent option pairs: holding the VT pipe
        // must not block a size claim or vice versa.
        assert!(session.claim_size_owner("sz", Duration::from_secs(10)));
        assert!(session.refresh_vt_owner("pid-3"));

        // Non-owner release is a no-op; owner release clears it.
        session.release_vt_owner("pid-2");
        assert!(session.refresh_vt_owner("pid-3"));
        session.release_vt_owner("pid-3");
        assert!(!session.refresh_vt_owner("pid-3"));
        session.release_size_owner("sz");
    }

    #[test]
    #[serial_test::serial]
    fn capture_pane_with_cursor_returns_content_and_cursor() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_cursor");
        let name = guard.name().to_string();
        // `printf` (no trailing newline, no shell prompt, no input echo) parks
        // the cursor deterministically just past the written text: "hello" is
        // 5 columns, so the cursor lands at (5, 0). `sleep` keeps the pane
        // alive across the capture; generous so a test thread starved by
        // parallel suite load can't outlive the pane before capturing.
        // Pin `pane-base-index 0` so `^.0` resolves on hosts with
        // `pane-base-index 1` set globally (see #488, #2231).
        let status = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &name,
                "-x",
                "40",
                "-y",
                "10",
                "sh -c 'printf hello; sleep 60'",
                ";",
                "set-option",
                "-t",
                &name,
                "pane-base-index",
                "0",
            ])
            .status()
            .expect("tmux new-session");
        assert!(status.success());

        // Poll until the pane has painted; a fixed sleep is flaky under
        // parallel test load (the pane needs the shell to spawn and printf
        // to run before capture sees anything).
        let session = Session::from_name(&name);
        let mut painted = (String::new(), None);
        for _ in 0..50 {
            let (content, cursor) = session
                .capture_pane_with_cursor(5)
                .expect("capture with cursor");
            if content.contains("hello") {
                painted = (content, cursor);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let (content, cursor) = painted;

        // The capture content is the same text the plain path would return:
        // the cursor line must have been split off, not leak into the body.
        assert!(
            content.contains("hello"),
            "capture content should hold the written text, got: {content:?}"
        );
        let cursor = cursor.expect("a live session reports a cursor");
        assert!(cursor.visible, "default cursor is visible");
        assert_eq!(cursor.pane_height, 10, "pane was created 10 rows tall");
        assert_eq!(
            (cursor.x, cursor.y),
            (5, 0),
            "cursor parks just past 'hello'"
        );
    }

    #[test]
    #[serial_test::serial]
    fn send_key_tokens_appends_no_implicit_enter() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_tokens_no_enter");
        let name = guard.name().to_string();
        // `read -r` blocks on a full line: it only prints once Enter arrives.
        // A bare literal with no Enter token must leave it blocked.
        let status = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &name,
                "-x",
                "40",
                "-y",
                "10",
                "sh -c 'read -r line; printf \"got:%s\" \"$line\"; sleep 60'",
                ";",
                "set-option",
                "-t",
                &name,
                "pane-base-index",
                "0",
            ])
            .status()
            .expect("tmux new-session");
        assert!(status.success());
        // The global session-existence cache has a short TTL and can be
        // refreshed by unrelated concurrent tests between session creation
        // and this check; inject directly so `exists()` can't false-negative.
        crate::tmux::test_inject_session_into_cache(&name);

        let session = Session::from_name(&name);
        session
            .send_key_tokens(&[crate::agents::KeyToken::Literal("hi")])
            .expect("send_key_tokens");

        // Give the shell a beat to react if it were (incorrectly) going to.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let content = session.capture_pane(20).expect("capture_pane");
        assert!(
            !content.contains("got:"),
            "no trailing Enter should have been sent, but read() completed: {content:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn send_key_tokens_sends_exact_sequence_in_order() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_tokens_sequence");
        let name = guard.name().to_string();
        let status = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &name,
                "-x",
                "40",
                "-y",
                "10",
                "sh -c 'read -r line; printf \"got:%s\" \"$line\"; sleep 60'",
                ";",
                "set-option",
                "-t",
                &name,
                "pane-base-index",
                "0",
            ])
            .status()
            .expect("tmux new-session");
        assert!(status.success());
        // See the comment in send_key_tokens_appends_no_implicit_enter above:
        // avoid a race against the global session-existence cache's TTL.
        crate::tmux::test_inject_session_into_cache(&name);

        let session = Session::from_name(&name);
        session
            .send_key_tokens(&[
                crate::agents::KeyToken::Literal("hi"),
                crate::agents::KeyToken::Named("Enter"),
            ])
            .expect("send_key_tokens");

        let mut content = String::new();
        for _ in 0..50 {
            content = session.capture_pane(20).expect("capture_pane");
            if content.contains("got:hi") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            content.contains("got:hi"),
            "literal text followed by a named Enter token should submit the line, got: {content:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_remain_on_exit_and_pane_dead() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_remain");
        let session_name = guard.name().to_string();
        // Chain set-option -p with new-session to avoid race condition
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 1",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        // Wait for the sleep command to finish
        std::thread::sleep(std::time::Duration::from_millis(1500));

        // Session should still exist (remain-on-exit keeps it)
        let exists = crate::tmux::tmux_command()
            .args(["has-session", "-t", &session_name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(exists, "Session should still exist due to remain-on-exit");

        // Pane should be dead (process exited)
        let pane_dead = crate::tmux::tmux_command()
            .args(["display-message", "-t", &session_name, "-p", "#{pane_dead}"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        assert!(pane_dead, "Pane should be dead after command exits");
    }

    #[test]
    #[serial_test::serial]
    fn test_create_forwards_desktop_env_to_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        // A var only this test reads, caught by the `XDG_` forwarding rule, so
        // it never collides with real config or another test's assertions.
        let key = "XDG_AOE_ENV_TEST_3075";
        let original = std::env::var(key).ok();
        std::env::set_var(key, "sentinel-value");

        let guard = TmuxTestSession::new("aoe_test_env_fwd");
        let session = super::Session::from_name(guard.name());
        let created = session.create_with_size("/tmp", Some("sleep 5"), Some((80, 24)), "default");

        let shown = crate::tmux::tmux_command()
            .args(["show-environment", "-t", guard.name(), key])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());

        match original {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }

        created.expect("create session");
        assert_eq!(
            shown.as_deref(),
            Some("XDG_AOE_ENV_TEST_3075=sentinel-value"),
            "a created agent session must carry the forwarded desktop/session env (#3075)"
        );
    }

    /// #3071: `is_attached` gates the TUI's passive preview resize, so it has
    /// to be right in both directions. The detached half is the cheap one; the
    /// attached half needs a real tmux client, which the sibling test below
    /// gets by running one inside a second tmux session.
    #[test]
    #[serial_test::serial]
    fn test_is_attached_false_for_detached_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_attached");
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                guard.name(),
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        let session = Session::from_name(guard.name());
        assert!(
            !session.is_attached(),
            "Detached session should report is_attached() == false"
        );
    }

    /// The attached half of the #3071 guard. A `-d` session can host a real
    /// tmux client without a controlling terminal: give a second session a
    /// command that unsets `$TMUX` and attaches to the first, and the first
    /// session's `session_attached` count goes to 1. Without this the detached
    /// test alone would pass against a hard-coded `false`, which is the exact
    /// inversion that reintroduces the bug.
    #[test]
    #[serial_test::serial]
    fn test_is_attached_true_with_live_client() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let target = TmuxTestSession::new("aoe_test_attached_target");
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                target.name(),
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
            ])
            .output()
            .expect("tmux new-session");
        assert!(created.status.success());

        // Rebuild this build's tmux argv (program plus any `-S`/`-L` socket
        // flags) so the nested client lands on the same server the test
        // isolates onto, and force a usable TERM: the server's base env can
        // carry `dumb` in CI, which tmux refuses to attach with.
        let probe = crate::tmux::tmux_command();
        let mut argv = vec![probe.get_program().to_string_lossy().into_owned()];
        argv.extend(probe.get_args().map(|a| a.to_string_lossy().into_owned()));
        let attach_cmd = format!(
            "unset TMUX; TERM=xterm-256color exec {} attach-session -t {}",
            argv.join(" "),
            target.name()
        );

        let client = TmuxTestSession::new("aoe_test_attached_client");
        let spawned = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                client.name(),
                "-x",
                "100",
                "-y",
                "40",
                &attach_cmd,
            ])
            .output()
            .expect("tmux new-session (client)");
        assert!(spawned.status.success());

        let session = Session::from_name(target.name());
        let mut attached = false;
        for _ in 0..40 {
            if session.is_attached() {
                attached = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            attached,
            "a session with a live tmux client must report is_attached() == true"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_is_pane_dead_on_running_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_alive");
        let session_name = guard.name().to_string();

        // Create a session with a long-running command
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        // Pane should NOT be dead (sleep is still running)
        let pane_dead = crate::tmux::tmux_command()
            .args(["display-message", "-t", &session_name, "-p", "#{pane_dead}"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        assert!(!pane_dead, "Pane should be alive while command is running");
    }

    /// Regression test for #435: with multiple tmux windows, pane health
    /// checks must target window 0 pane 0 explicitly so that a dead pane in
    /// a second window does not cause the agent pane to be killed.
    #[test]
    #[serial_test::serial]
    fn test_is_pane_dead_targets_window_zero_with_multiple_windows() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_multiwin");
        let session_name = guard.name().to_string();

        // Create session with a long-running command in window 0
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        // Force base-index 1 and pane-base-index 1 to simulate users who
        // have both set in their tmux.conf.
        let output = crate::tmux::tmux_command()
            .args(["set-option", "-t", &session_name, "base-index", "1"])
            .output()
            .expect("tmux set-option base-index");
        assert!(output.status.success());
        let output = crate::tmux::tmux_command()
            .args(["set-option", "-t", &session_name, "pane-base-index", "1"])
            .output()
            .expect("tmux set-option pane-base-index");
        assert!(output.status.success());

        // Create a second window with a command that exits immediately
        let output = crate::tmux::tmux_command()
            .args([
                "new-window",
                "-t",
                &session_name,
                "true", // exits immediately
            ])
            .output()
            .expect("tmux new-window");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(300));

        // The agent pane (first window) is still alive, so is_pane_dead should
        // return false even though the second window's pane has exited.
        assert!(
            !is_pane_dead(&session_name),
            "is_pane_dead should check the first window's pane, not the active window"
        );
    }

    /// Regression test: capture_pane must target the first window's pane
    /// regardless of which window is currently active, and regardless of
    /// the user's tmux base-index setting.
    #[test]
    #[serial_test::serial]
    fn test_capture_pane_targets_first_window_with_multiple_windows() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_capture_multiwin");
        let session_name = guard.name().to_string();

        // Create session running sleep in the first window
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        // Force base-index 1 to simulate users who have set base-index 1 in
        // their tmux.conf. With base-index 1, window 0 does not exist, so any
        // target using :0.0 silently fails.
        let output = crate::tmux::tmux_command()
            .args(["set-option", "-t", &session_name, "base-index", "1"])
            .output()
            .expect("tmux set-option base-index");
        assert!(output.status.success());

        // Open a second window running a shell, and make it the active window
        let output = crate::tmux::tmux_command()
            .args(["new-window", "-t", &session_name, "sh"])
            .output()
            .expect("tmux new-window");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        let session = Session {
            name: session_name.clone(),
        };

        // capture_pane must succeed -- with base-index 1, a :0.0 target does
        // not exist and the tmux command fails silently returning empty content.
        let _content = session
            .capture_pane(10)
            .expect("capture_pane should not return an error for a valid session");

        // The command in the first window is 'sleep', not a shell.
        // is_pane_running_shell must return false even though the active
        // window is running sh. With a :0.0 target and base-index 1 this
        // would return false for the wrong reason (silent failure), but with
        // ^ it correctly reads the first window's pane_current_command.
        assert!(
            !session.is_pane_running_shell(),
            "is_pane_running_shell should check first window (sleep), not active window (sh)"
        );
    }

    /// An unsplit window must composite to exactly what `capture_pane`
    /// returns, so the overwhelmingly common case is provably unchanged.
    #[test]
    #[serial_test::serial]
    fn composited_capture_matches_capture_pane_when_unsplit() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_composite_single");
        let session = start_composite_session(guard.name(), 80, 24, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");

        let plain = session.capture_pane(10).expect("capture_pane");
        let composited = session
            .capture_window_composited(10)
            .expect("capture_window_composited");
        assert!(plain.contains("ALPHA"), "control capture empty: {plain:?}");
        assert_eq!(
            composited, plain,
            "an unsplit window must pass the pane bytes through untouched"
        );
    }

    /// The point of the feature: a pane the user split off by hand shows up in
    /// the preview instead of being invisible.
    #[test]
    #[serial_test::serial]
    fn composited_capture_includes_a_split_off_pane() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_composite_split");
        let session = start_composite_session(guard.name(), 80, 24, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
        // `C-b %`: a second pane beside the first.
        split_composite_session(&session, "sh -c 'echo BRAVO; sleep 30'");
        wait_for_composite_text(&session, "BRAVO");

        let plain = session.capture_pane(10).expect("capture_pane");
        let composited = session
            .capture_window_composited(10)
            .expect("capture_window_composited");

        // The old behaviour: pane 0 only, split pane invisible.
        assert!(plain.contains("ALPHA"));
        assert!(
            !plain.contains("BRAVO"),
            "control: capture_pane should not see the split pane"
        );
        // The new behaviour: both panes, side by side on the same rows.
        assert!(
            composited.contains("ALPHA") && composited.contains("BRAVO"),
            "composite missed a pane:\n{composited}"
        );
        let seam_row = composited
            .lines()
            .find(|l| l.contains("ALPHA"))
            .expect("row with ALPHA");
        assert!(
            seam_row.contains("BRAVO"),
            "panes should share a row, not stack:\n{seam_row:?}"
        );
    }

    /// `C-b z` keeps `window_panes` at its real count while reporting every pane
    /// at the window's FULL rectangle, so the rectangles overlap and the
    /// compositor's tiling assumption breaks. Compositing that painted pane 0 at
    /// its unzoomed width and filled the rest of every row with `─`, hiding the
    /// zoomed pane, which is the only thing the user sees in tmux. The frame was
    /// strictly worse than the pane-0-only preview it replaced, and permanently
    /// so, since nothing self-heals a zoom.
    ///
    /// Zoomed must therefore be treated as unsplit, byte-for-byte identical to
    /// the plain capture.
    #[test]
    #[serial_test::serial]
    fn a_zoomed_pane_falls_back_to_the_plain_capture() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_composite_zoom");
        let session = start_composite_session(guard.name(), 40, 8, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
        split_composite_session(&session, "sh -c 'echo BRAVO; sleep 30'");
        wait_for_composite_text(&session, "BRAVO");

        // Control: unzoomed, both panes, and one line per window row.
        let unzoomed = session
            .capture_window_composited(10)
            .expect("composite unzoomed");
        assert!(
            unzoomed.contains("ALPHA") && unzoomed.contains("BRAVO"),
            "control: split should composite both panes:\n{unzoomed}"
        );

        let zoom = crate::tmux::tmux_command()
            .args(["resize-pane", "-Z", "-t", &format!("{}:^.1", session.name)])
            .status()
            .expect("tmux resize-pane -Z");
        assert!(zoom.success(), "zoom must land or this tests nothing");
        assert_eq!(
            String::from_utf8_lossy(
                &crate::tmux::tmux_command()
                    .args([
                        "display-message",
                        "-p",
                        "-t",
                        &format!("{}:^", session.name),
                        "-F",
                        "#{window_zoomed_flag}",
                    ])
                    .output()
                    .expect("zoom probe")
                    .stdout
            )
            .trim(),
            "1",
            "tmux did not report the window as zoomed"
        );

        let zoomed = session
            .capture_window_composited(10)
            .expect("composite zoomed");
        assert!(
            !zoomed.contains('─') && !zoomed.contains('│'),
            "zoomed frame painted border fill over the window:\n{zoomed}"
        );
        assert_eq!(
            zoomed,
            session.capture_pane(10).expect("plain capture"),
            "zoomed must be byte-identical to the pane-0 capture"
        );

        // Unzooming restores the composite rather than latching the fallback.
        assert!(crate::tmux::tmux_command()
            .args(["resize-pane", "-Z", "-t", &format!("{}:^.1", session.name)])
            .status()
            .expect("tmux unzoom")
            .success());
        let restored = session
            .capture_window_composited(10)
            .expect("composite after unzoom");
        assert!(
            restored.contains("ALPHA") && restored.contains("BRAVO"),
            "unzoom did not restore the composite:\n{restored}"
        );
    }

    /// A composited capture must carry one line per window row. It is handed to
    /// the preview cache like a `capture-pane` result and the cursor is rebased
    /// onto `window_height`, so a row lost off the bottom paints the cursor one
    /// row too high. A stacked split with an idle shell underneath is the case
    /// that produced it: the bottom row is blank, and joining rows rather than
    /// terminating them let the renderer drop it.
    #[test]
    #[serial_test::serial]
    fn a_stacked_split_composites_one_line_per_window_row() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_composite_rows");
        let session = start_composite_session(guard.name(), 30, 10, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
        // `C-b "`: stacked, so the bottom pane's last row is blank.
        let split = crate::tmux::tmux_command()
            .args([
                "split-window",
                "-v",
                "-t",
                &session.name,
                "sh -c 'sleep 30'",
            ])
            .status()
            .expect("tmux split-window -v");
        assert!(split.success(), "failed to split {}", session.name);
        refresh_session_cache();
        wait_for_composite_text(&session, "ALPHA");

        let composited = session.capture_window_composited(10).expect("composite");
        assert_eq!(
            composited.lines().count(),
            10,
            "composite must be window_height lines:\n{composited}"
        );
    }

    /// The live path caches a layout and re-renders only pane 0 from its VT
    /// grid, so the layout must come back with pane 0 first, at the window
    /// origin, and with rectangles that tile the real window.
    #[test]
    #[serial_test::serial]
    fn captured_layout_puts_pane_zero_first_at_the_origin() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_layout_order");
        let session = start_composite_session(guard.name(), 80, 24, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
        split_composite_session(&session, "sh -c 'echo BRAVO; sleep 30'");
        wait_for_composite_text(&session, "BRAVO");
        // Make the SECOND pane active: pane 0 must still come back first. The
        // select must be asserted, or a failure leaves pane 0 active and the
        // assertions below pass for the boring reason, silently retiring the
        // premise this test exists to check.
        let selected = crate::tmux::tmux_command()
            .args(["select-pane", "-t", &format!("{}:^.1", session.name)])
            .output()
            .expect("tmux select-pane");
        assert!(
            selected.status.success(),
            "select-pane must land, or this degrades to the pane-0-already-active case"
        );
        let active = crate::tmux::tmux_command()
            .args([
                "display-message",
                "-p",
                "-t",
                &format!("{}:^", session.name),
                "-F",
                "#{pane_index}",
            ])
            .output()
            .expect("tmux display-message");
        assert_eq!(
            String::from_utf8_lossy(&active.stdout).trim(),
            "1",
            "pane 1 should be the active pane before the layout is captured"
        );
        let layout = session
            .capture_window_layout(2)
            .expect("layout for a split window");
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.window_width, 80);
        let first = layout.first_pane().expect("first pane");
        assert_eq!(
            (first.left, first.top),
            (0, 0),
            "pane 0 must sit at the window origin for the cursor math to hold"
        );
        // Pane 0 is the agent's, even though pane 1 is the active one.
        assert!(
            layout.panes[0].rows.iter().any(|r| r.contains("ALPHA")),
            "pane 0 rows: {:?}",
            layout.panes[0].rows
        );
        assert!(layout.panes[1].rows.iter().any(|r| r.contains("BRAVO")));
        // Every row is padded to its own pane's width, which is what lets the
        // compositor concatenate them.
        for (i, pane) in layout.panes.iter().enumerate() {
            for row in &pane.rows {
                assert_eq!(
                    crate::tmux::utils::strip_ansi(row).chars().count(),
                    pane.geom.width as usize,
                    "pane {i} row not padded to {}: {row:?}",
                    pane.geom.width
                );
            }
        }
    }

    /// A composited capture must return the pane's cursor, and on an unsplit
    /// window it must return the same bytes and cursor mode flags the plain
    /// cursor-bearing capture does.
    ///
    /// The cursor matters because this is live-send's transport whenever no VT
    /// channel is available: without it a split preview loses both its painted
    /// cursor and the alternate-screen / mouse flags the wheel forward reads.
    #[test]
    #[serial_test::serial]
    fn composited_capture_carries_the_pane_cursor() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_composite_cursor");
        let session = start_composite_session(guard.name(), 80, 24, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");

        let (content, cursor) = session
            .capture_window_composited_with_cursor(20)
            .expect("composited capture");
        let plain = session.capture_pane(20).expect("capture_pane");
        assert_eq!(
            content, plain,
            "unsplit window must still pass pane bytes through untouched"
        );
        assert!(
            content.contains("ALPHA"),
            "first captured row went missing: {content:?}"
        );
        let cursor = cursor.expect("a cursor for a live pane");
        assert_eq!(cursor.pane_width, 80, "cursor carries the pane geometry");

        // Now split, and the cursor must be rebased onto the window so the
        // renderer's `pane_height` anchoring still lines up with the composite.
        split_composite_session(&session, "sh -c 'echo BRAVO; sleep 30'");
        wait_for_composite_text(&session, "BRAVO");

        let (content, cursor) = session
            .capture_window_composited_with_cursor(20)
            .expect("composited capture");
        assert!(content.contains("ALPHA") && content.contains("BRAVO"));
        let cursor = cursor.expect("a cursor for the split window");
        assert_eq!(
            cursor.pane_width, 80,
            "rebased onto the window, not pane 0 (which is now ~39 wide)"
        );
        assert_eq!(
            cursor.history_size, 0,
            "a composite has no scrollback to advertise"
        );
        assert!(
            cursor.position_reliable,
            "visible-only composite cannot have drifted"
        );
    }

    /// The two composite transports must agree byte for byte on a static
    /// window.
    ///
    /// The live path renders a cached layout with pane 0 swapped for its VT
    /// grid rows, while the passive fallback re-forks every pane. Any
    /// divergence in shape between them shows up as the preview flickering
    /// between two renderings as one path takes over from the other, which is
    /// exactly the bug that shipped when the fallback still captured pane 0
    /// alone: the split was visible for one frame after each keystroke and
    /// vanished during idle.
    #[test]
    #[serial_test::serial]
    fn both_composite_transports_agree_on_a_static_window() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_composite_agree");
        let session = start_composite_session(guard.name(), 80, 24, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
        split_composite_session(&session, "sh -c 'echo BRAVO; sleep 30'");
        wait_for_composite_text(&session, "BRAVO");

        // Passive fallback: one fork per pane, composited on the spot.
        let fallback = session
            .capture_window_composited(24)
            .expect("capture_window_composited");
        let layout = session.capture_window_layout(2).expect("layout");
        // Live path, minus the grid: swapping pane 0's own captured rows back
        // in must be a no-op, which is what makes the swap safe to do with
        // fresher rows every frame.
        let swapped = layout.composite_with_first_pane_rows(&layout.panes[0].rows.clone());

        assert_eq!(
            fallback,
            layout.composite(),
            "fork-per-frame and cached-layout renderings diverged"
        );
        assert_eq!(
            fallback, swapped,
            "swapping pane 0's rows for identical rows changed the frame"
        );
    }

    /// Regression test: is_pane_running_shell must target the first window's
    /// pane even when the active window is a shell, and even with base-index 1.
    #[test]
    #[serial_test::serial]
    fn test_is_pane_running_shell_targets_first_window_with_multiple_windows() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_shell_multiwin");
        let session_name = guard.name().to_string();

        // Create session running sleep (not a shell) in the first window
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        // Force base-index 1 to simulate users who have set base-index 1 in
        // their tmux.conf. With base-index 1, window 0 does not exist, so any
        // target using :0.0 silently fails.
        let output = crate::tmux::tmux_command()
            .args(["set-option", "-t", &session_name, "base-index", "1"])
            .output()
            .expect("tmux set-option base-index");
        assert!(output.status.success());

        // Open a second window running a shell and make it active
        let output = crate::tmux::tmux_command()
            .args(["new-window", "-t", &session_name, "sh"])
            .output()
            .expect("tmux new-window");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        // Should be false: first window runs 'sleep', not a shell.
        // Would incorrectly return true if the active second window (sh) were checked.
        // With base-index 1 and a :0.0 target the call silently fails and
        // returns false for the wrong reason; ^ correctly reads the first pane.
        assert!(
            !is_pane_running_shell(&session_name),
            "is_pane_running_shell should target first window (sleep), not active window (sh)"
        );
    }

    /// Regression test for #488: when a user creates a split pane and makes it
    /// active, is_pane_dead and is_pane_running_shell must still target the
    /// agent's pane (pane 0), not the active split pane.
    #[test]
    #[serial_test::serial]
    fn test_status_checks_target_pane_zero_with_split_panes() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_splitpane");
        let session_name = guard.name().to_string();

        // Create session with a long-running command (the "agent")
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        // Split the window -- this creates a new pane running a shell
        let output = crate::tmux::tmux_command()
            .args(["split-window", "-t", &session_name])
            .output()
            .expect("tmux split-window");
        assert!(output.status.success());

        // The split pane is now active. Select it explicitly to be sure.
        let output = crate::tmux::tmux_command()
            .args(["select-pane", "-t", &format!("{session_name}:.1")])
            .output()
            .expect("tmux select-pane");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        // The agent pane (pane 0) is still alive
        assert!(
            !is_pane_dead(&session_name),
            "is_pane_dead should check pane 0 (sleep), not the active split pane"
        );

        // The agent pane runs 'sleep', not a shell
        assert!(
            !is_pane_running_shell(&session_name),
            "is_pane_running_shell should check pane 0 (sleep), not the active split pane (shell)"
        );
    }

    /// Regression test for #488: ensure status checks work correctly when both
    /// pane-base-index 1 and split panes are in play.
    #[test]
    #[serial_test::serial]
    fn test_status_checks_with_split_panes_and_pane_base_index_1() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_splitpbi");
        let session_name = guard.name().to_string();

        // Create session with pane-base-index 0 pinned (as aoe does)
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
                ";",
                "set-option",
                "-t",
                &session_name,
                "pane-base-index",
                "0",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        // Simulate a user with pane-base-index 1 globally by setting it on the
        // window -- but aoe has already pinned pane-base-index 0 on the session,
        // so pane 0 should still be valid.
        // Note: we set it on the session to verify our pinning takes precedence.
        // Actually, set pane-base-index 1 globally to simulate user config, then
        // verify our session-level override keeps pane 0 valid.

        // Split the window and make the new pane active
        let output = crate::tmux::tmux_command()
            .args(["split-window", "-t", &session_name])
            .output()
            .expect("tmux split-window");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        assert!(
            !is_pane_dead(&session_name),
            "is_pane_dead should check pane 0 (sleep) with pane-base-index pinned to 0"
        );

        assert!(
            !is_pane_running_shell(&session_name),
            "is_pane_running_shell should check pane 0 (sleep) with pane-base-index pinned to 0"
        );
    }

    #[test]
    fn test_sanitize_session_name() {
        assert_eq!(sanitize_session_name("my-project"), "my-project");
        assert_eq!(sanitize_session_name("my project"), "my_project");
        assert_eq!(sanitize_session_name("a".repeat(30).as_str()).len(), 20);
    }

    #[test]
    fn test_generate_name() {
        let name = Session::generate_name("abc123def456", "My Project");
        assert!(name.starts_with(SESSION_PREFIX));
        assert!(name.contains("My_Project"));
        assert!(name.contains("abc123de"));
    }

    #[test]
    fn test_build_create_args_without_size() {
        let args = build_create_args("test_session", "/tmp/work", &[], None, None);
        assert_eq!(
            args,
            vec!["new-session", "-d", "-s", "test_session", "-c", "/tmp/work"]
        );
        assert!(!args.contains(&"-x".to_string()));
        assert!(!args.contains(&"-y".to_string()));
    }

    #[test]
    fn test_build_create_args_empty_env_adds_no_e_flag() {
        // Byte-for-byte unchanged args when no env is supplied: the agent
        // session and container terminals must not regress.
        let args = build_create_args("s", "/tmp/work", &[], Some("claude"), None);
        assert!(!args.contains(&"-e".to_string()));
        assert_eq!(args.last().unwrap(), "claude");
    }

    #[test]
    fn test_build_create_args_env_emits_e_flags_before_command() {
        let args = build_create_args(
            "s",
            "/tmp/work",
            &[("HOME", "/Users/me"), ("SHELL", "/bin/zsh")],
            Some("'/bin/zsh' -l"),
            None,
        );
        // Each pair becomes an adjacent `-e KEY=VAL`.
        let e_idx = args.iter().position(|a| a == "-e").unwrap();
        assert_eq!(args[e_idx + 1], "HOME=/Users/me");
        assert_eq!(args[e_idx + 3], "SHELL=/bin/zsh");
        // Env flags precede the trailing command.
        assert!(e_idx < args.iter().position(|a| a == "'/bin/zsh' -l").unwrap());
        // `-c` still precedes the env flags.
        assert!(args.iter().position(|a| a == "-c").unwrap() < e_idx);
    }

    /// `create_with_size_env` appends the caller's `extra_env` after the
    /// forwarded desktop env precisely so a `host_hooks.before_session` value
    /// overrides a same-keyed desktop entry. Arg construction must preserve that
    /// order rather than dedupe or reorder, or the intent is lost before tmux
    /// ever sees it.
    #[test]
    fn test_build_create_args_preserves_duplicate_key_order() {
        let args = build_create_args(
            "s",
            "/tmp/work",
            &[
                ("CLAUDE_CONFIG_DIR", "/desktop"),
                ("CLAUDE_CONFIG_DIR", "/minted"),
            ],
            Some("claude"),
            None,
        );
        let values: Vec<&String> = args
            .iter()
            .filter(|a| a.starts_with("CLAUDE_CONFIG_DIR="))
            .collect();
        assert_eq!(
            values,
            vec!["CLAUDE_CONFIG_DIR=/desktop", "CLAUDE_CONFIG_DIR=/minted"],
            "both -e flags must survive, minted last"
        );
    }

    #[test]
    fn test_build_create_args_with_size() {
        let args = build_create_args("test_session", "/tmp/work", &[], None, Some((120, 40)));
        assert!(args.contains(&"-x".to_string()));
        assert!(args.contains(&"120".to_string()));
        assert!(args.contains(&"-y".to_string()));
        assert!(args.contains(&"40".to_string()));

        // Verify order: -x should come before width, -y before height
        let x_idx = args.iter().position(|a| a == "-x").unwrap();
        let y_idx = args.iter().position(|a| a == "-y").unwrap();
        assert_eq!(args[x_idx + 1], "120");
        assert_eq!(args[y_idx + 1], "40");
    }

    #[test]
    fn test_build_create_args_with_command() {
        let args = build_create_args("test_session", "/tmp/work", &[], Some("claude"), None);
        assert_eq!(args.last().unwrap(), "claude");
    }

    #[test]
    fn test_build_create_args_with_size_and_command() {
        let args = build_create_args(
            "test_session",
            "/tmp/work",
            &[],
            Some("claude"),
            Some((80, 24)),
        );

        // Size args should be present
        assert!(args.contains(&"-x".to_string()));
        assert!(args.contains(&"80".to_string()));
        assert!(args.contains(&"-y".to_string()));
        assert!(args.contains(&"24".to_string()));

        // Command should be last
        assert_eq!(args.last().unwrap(), "claude");
    }

    #[test]
    #[serial_test::serial]
    fn test_is_pane_running_shell_on_shell_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_shell");
        let session_name = guard.name().to_string();

        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sh",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        assert!(
            is_pane_running_shell(&session_name),
            "Session running sh should be detected as a shell"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_is_pane_running_shell_on_non_shell_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_noshell");
        let session_name = guard.name().to_string();

        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep",
                "30",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        assert!(
            !is_pane_running_shell(&session_name),
            "Session running 'sleep' should not be detected as a shell"
        );
    }

    /// Regression test for the dead-pane restart bug: a session whose pane
    /// has died (remain-on-exit kept the session) must be revivable via
    /// respawn_dead_pane without tearing down the tmux session.
    #[test]
    #[serial_test::serial]
    fn test_respawn_dead_pane_revives_dead_pane() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_respawn");
        let session_name = guard.name().to_string();

        // Start a session with a command that exits immediately and
        // remain-on-exit set, so we end up with a dead pane. Pin
        // pane-base-index 0 to match what aoe does in production;
        // without this, users with `pane-base-index 1` in their
        // tmux.conf cause the `^.0` target to miss.
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "true",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
                ";",
                "set-option",
                "-t",
                &session_name,
                "pane-base-index",
                "0",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(500));

        let session = Session::from_name(&session_name);
        super::refresh_session_cache();

        assert!(session.exists(), "Session should exist via remain-on-exit");
        assert!(session.is_pane_dead(), "Pane should be dead after `true`");

        let respawned = session
            .respawn_dead_pane("/tmp", Some("sleep 30"))
            .expect("respawn_dead_pane should succeed");
        assert!(respawned, "respawn_dead_pane should report it acted");

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(session.exists(), "Session should still exist after respawn");
        assert!(
            !session.is_pane_dead(),
            "Pane should be alive after respawn"
        );

        let respawned_again = session
            .respawn_dead_pane("/tmp", Some("sleep 30"))
            .expect("respawn_dead_pane on live pane should not error");
        assert!(
            !respawned_again,
            "respawn_dead_pane should report no-op on live pane"
        );
    }

    /// respawn_dead_pane on a non-existent session is a safe no-op.
    #[test]
    #[serial_test::serial]
    fn test_respawn_dead_pane_no_session() {
        let session = Session::from_name("aoe_test_nonexistent_session_xyz");
        let result = session
            .respawn_dead_pane("/tmp", Some("zsh"))
            .expect("respawn_dead_pane should not error on missing session");
        assert!(
            !result,
            "respawn_dead_pane should return false for missing session"
        );
    }
}
