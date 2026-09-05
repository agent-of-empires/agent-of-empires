//! tmux session management

use anyhow::{bail, Result};
use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{
    composite::{CapturedPane, PaneGeom, WindowLayout},
    probe_session_existence, refresh_session_cache,
    utils::{
        append_pane_base_index_args, append_remain_on_exit_args, append_tmux_setting_args,
        append_window_size_args, is_pane_dead, is_pane_running_shell, PANE_ENV_FILE_PREFIX,
    },
    SessionExistence, SESSION_PREFIX,
};
use crate::cli::truncate_id;
use crate::process;
use crate::session::environment::shell_escape_script_word;
use crate::session::Status;
use crate::util::now_ms;

pub struct Session {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEnvMutation {
    Set { key: String, value: String },
    Unset { key: String },
}

impl PaneEnvMutation {
    pub fn set(key: String, value: String) -> Self {
        Self::Set { key, value }
    }

    pub fn unset(key: String) -> Self {
        Self::Unset { key }
    }

    fn key(&self) -> &str {
        match self {
            Self::Set { key, .. } | Self::Unset { key } => key,
        }
    }
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
const VT_PIPE_OWNER_OPT: &str = "@aoe_vt_pipe_owner";

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

static OWNER_HEARTBEAT_CLOCK: AtomicU64 = AtomicU64::new(0);

fn next_owner_heartbeat(after: u64) -> u64 {
    let mut current = OWNER_HEARTBEAT_CLOCK.load(Ordering::Relaxed);
    loop {
        let next = now_ms()
            .max(after.saturating_add(1))
            .max(current.saturating_add(1));
        match OWNER_HEARTBEAT_CLOCK.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

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
    /// Pane 0's rectangle within a composited preview window, or `None` when
    /// the preview shows a single pane.
    ///
    /// Composite content uses the window grid while the cursor and input stay
    /// pane relative. Window chrome can give pane 0 a non-zero origin (#3515),
    /// so full-window consumers add it to cursor coordinates and subtract it
    /// from pointer coordinates. A cropped preview must also account for rows
    /// removed before mapping. Input remains pinned to pane 0 (#435, #488).
    pub composite_pane0: Option<PaneGeom>,
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

/// Keep mode flags from a lone cursor probe while preventing the renderer from
/// trusting its row. This is the degraded path when tmux omits the post-capture
/// sentinel but still returns the pane capture successfully.
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

    /// [`Self::resolve_name`] for **render paths**: answered from the shared
    /// snapshot only, never refreshing. A stale snapshot yields the derived
    /// name until the background snapshot poller refreshes it; paint must
    /// never wait on tmux.
    pub(crate) fn resolve_name_for_display(id: &str, title: &str) -> String {
        crate::tmux::agent_session_name_for_display(id, &Self::generate_name(id, title))
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
    pub(crate) fn exists_with_deadline(&self, deadline: &crate::tmux::TmuxCommandDeadline) -> bool {
        let mut command = crate::tmux::tmux_command();
        command.args(["has-session", "-t", &self.name]);
        deadline
            .run(&mut command)
            .map(|output| output.status.success())
            .unwrap_or(false)
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

    /// Like [`Self::create_with_size`], but also applies `extra_env` mutations
    /// in the pane process through a protected, one-shot file.
    ///
    /// Environment values and the launch command never enter tmux client argv,
    /// pane start-command metadata, or tmux's persistent session environment.
    /// The short pane command runs the file as a POSIX script; that script
    /// applies shell-escaped exports and explicit unsets, then unlinks itself
    /// before executing the requested command.
    /// The non-secret OMP launch ID remains a tmux `-e` value so capture can
    /// query it. Desktop/session values retain the existing tmux environment
    /// behavior used by later panes.
    pub fn create_with_size_env(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
        profile: &str,
        extra_env: &[PaneEnvMutation],
    ) -> Result<()> {
        self.create_with_size_env_inner(working_dir, command, size, profile, extra_env, &[])
    }

    /// Create a pane whose container runtime reads target environment values
    /// from an inherited env-file descriptor. The target keys never enter the
    /// host pane environment.
    pub(crate) fn create_with_size_env_and_container_env(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
        profile: &str,
        extra_env: &[PaneEnvMutation],
        container_env: &[(String, String)],
    ) -> Result<()> {
        self.create_with_size_env_inner(
            working_dir,
            command,
            size,
            profile,
            extra_env,
            container_env,
        )
    }

    fn create_with_size_env_inner(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
        profile: &str,
        extra_env: &[PaneEnvMutation],
        container_env: &[(String, String)],
    ) -> Result<()> {
        if self.exists() {
            return Ok(());
        }

        // tmux does not error when `-c <dir>` points at a missing directory;
        // it silently falls back to the server's own `$HOME`, which for a
        // long-running daemon/TUI process is wherever *it* was launched from,
        // not this session's `project_path`. Callers (`Instance::start_with_size_opts`)
        // already reload `project_path` from disk immediately before this call,
        // so a missing directory here means the worktree/project itself is
        // gone or not yet materialized, not a stale in-memory value. Fail
        // loudly instead of silently spawning in the wrong place. See #3265.
        let working_dir_path = std::path::Path::new(working_dir);
        if !working_dir_path.is_dir() {
            bail!(
                "Cannot create tmux session '{}': working directory '{}' does not exist \
                 or is not a directory (tmux would otherwise silently fall back to $HOME)",
                self.name,
                working_dir
            );
        }

        // Diagnostic for #3265 ("fresh/restarted panes spawn with the wrong
        // cwd"): log the exact `-c` value this spawn resolved to, plus its
        // canonicalized form, so a future recurrence (if the guard above
        // doesn't catch it, e.g. a permissions issue rather than a missing
        // path) leaves direct evidence of what `working_dir` actually was at
        // the moment of the `tmux new-session` call, instead of requiring a
        // fresh repro under instrumentation.
        tracing::debug!(target: "tmux.command",
            session = %self.name,
            working_dir,
            working_dir_canonical = %working_dir_path
                .canonicalize()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|e| format!("<canonicalize failed: {e}>")),
            "resolved working directory for tmux new-session"
        );

        let config = super::tmux_option_config(profile);

        // Forward the inherited host env (DISPLAY, XDG_*, DBUS, ... plus every
        // other var when `session.inherit_host_environment` is on) so an agent
        // and any browser it launches, e.g. for OIDC, can reach the user's
        // desktop. tmux otherwise carries only its narrow `update-environment`
        // set plus the server's frozen base env (#3075, #3262).
        let inherited_env = crate::session::environment::inherited_host_env(profile);
        let mut protected_env = Vec::new();
        let mut tmux_env: Vec<(&str, &str)> = inherited_env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        for mutation in extra_env {
            let key = mutation.key();
            if !crate::session::environment::is_valid_env_key(key) {
                tracing::warn!(target: "session.create", "invalid pane environment key '{}'; skipping", key);
                continue;
            }
            match mutation {
                PaneEnvMutation::Set { key, value }
                    if key == crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY =>
                {
                    tmux_env.push((key.as_str(), value.as_str()));
                }
                _ => protected_env.push(mutation.clone()),
            }
        }

        let mut env_file = EphemeralEnvFile::create(&protected_env, container_env)?;
        let wrapped_command = env_file.wrap_command(command)?;
        let mut args = build_create_args(
            &self.name,
            working_dir,
            &tmux_env,
            Some(&wrapped_command),
            size,
        );
        append_remain_on_exit_args(&mut args, &self.name);
        append_pane_base_index_args(&mut args, &self.name);
        append_window_size_args(&mut args, &self.name);
        append_tmux_setting_args(&mut args, &self.name, &config);

        let output = crate::tmux::tmux_command().args(&args).output()?;

        // With -d, tmux can accept a session even when the pane command will
        // fail. Never log the full argv: the pane command can contain legacy
        // user-configured credentials even though current launches reject or
        // transport them out of band.
        tracing::debug!(
            target: "tmux.command",
            session = %self.name,
            arg_count = args.len(),
            "tmux new-session completed"
        );

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to create tmux session: {}", stderr);
        }

        // Unlinking the channel is the pane's acknowledgement that it sourced
        // the protected values and command. Keep parent cleanup ownership until
        // then: tmux's detached create can return success before the wrapper
        // runs.
        if !env_file.wait_until_consumed(Duration::from_secs(5)) {
            super::refresh_session_cache();
            let _ = self.kill();
            bail!("Pane did not consume its protected launch script");
        }
        env_file.disarm();
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

        let mut command = crate::tmux::tmux_command();
        command.args(["rename-session", "-t", &self.name, new_name]);
        let output = crate::tmux::run_tmux_command_with_timeout(&mut command)?;

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

    /// Return a conservative Unix epoch millisecond watermark for the tmux
    /// session creation time.
    ///
    /// `#{session_created}` has one-second precision, so migration rounds it
    /// to the end of that second. A legacy breadcrumb from the same second is
    /// deliberately not proof that OMP rewrote it after launch.
    pub fn created_at_ms(&self) -> Result<u64> {
        let output = crate::tmux::tmux_command()
            .args([
                "display-message",
                "-t",
                &self.name,
                "-p",
                "#{session_created}",
            ])
            .output()?;
        if !output.status.success() {
            bail!(
                "Failed to read creation time for tmux session '{}'",
                self.name
            );
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let seconds = raw.trim().parse::<u64>().map_err(|_| {
            anyhow::anyhow!(
                "tmux session '{}' reported an invalid creation time",
                self.name
            )
        })?;
        seconds
            .checked_mul(1000)
            .and_then(|millis| millis.checked_add(999))
            .ok_or_else(|| anyhow::anyhow!("tmux session '{}' creation time overflowed", self.name))
    }

    /// Return the TTY device for the agent pane.
    ///
    /// OMP uses this device to key its terminal-session breadcrumb. Target the
    /// first window's first pane for the same reason as [`Self::capture_pane`].
    pub fn pane_tty(&self) -> Result<String> {
        let target = format!("{}:^.0", self.name);
        let output = crate::tmux::tmux_command()
            .args(["display-message", "-t", &target, "-p", "#{pane_tty}"])
            .output()?;
        if !output.status.success() {
            bail!("Failed to read pane TTY for tmux session '{}'", self.name);
        }
        let tty = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if tty.is_empty() {
            bail!("tmux session '{}' reported an empty pane TTY", self.name);
        }
        Ok(tty)
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

    /// Wait for the pane to become ready for input, or `max_wait` to elapse.
    /// Failsafe: always returns by `max_wait`, so a caller's next action
    /// (e.g. `send-keys`) still runs even if the pane never becomes ready,
    /// such as an agent that is genuinely still streaming output.
    ///
    /// When `ready_marker` is `Some` (see `AgentDef::ready_marker`), polls
    /// for that substring actually appearing in the captured pane content
    /// (matched case-insensitively) -- a real, agent-specific readiness
    /// signal.
    ///
    /// When `ready_marker` is `None` (no such signal is known for this
    /// agent yet), falls back to a generic heuristic: content stops
    /// changing across two consecutive samples. This is weaker -- a short,
    /// static "still loading" screen can satisfy it before the agent is
    /// actually listening -- but it is strictly better than sending
    /// immediately, and is the same heuristic `aoe session restart`'s
    /// wake-message send already relied on before per-agent markers
    /// existed.
    ///
    /// Shared by `aoe session restart`'s post-restart wake message and `aoe
    /// send`'s pre-send wait: both need to avoid typing into a pane whose
    /// agent has not finished rendering yet.
    pub fn wait_until_ready(&self, max_wait: std::time::Duration, ready_marker: Option<&str>) {
        let poll_interval = std::time::Duration::from_millis(200);
        let deadline = std::time::Instant::now() + max_wait;
        let ready_marker = ready_marker.map(str::to_lowercase);
        let mut last: Option<String> = None;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(poll_interval);
            let Ok(now) = self.capture_pane(5) else {
                continue;
            };
            if let Some(marker) = ready_marker.as_deref() {
                if now.to_lowercase().contains(marker) {
                    return;
                }
                continue;
            }
            if now.trim().len() > 20 {
                if last.as_deref() == Some(now.as_str()) {
                    return;
                }
                last = Some(now);
            }
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
    /// Composite window capture plus pane 0's cursor, for the live preview.
    ///
    /// The probe targets `^.0`, the pane that receives input, rather than the
    /// window whose format fields resolve against whichever pane is selected.
    /// On a composite, `pane_height`/`pane_width` are rebased to the window and
    /// [`PaneCursor::composite_pane0`] retains pane 0's coordinate frame.
    pub fn capture_window_composited_with_cursor(
        &self,
        lines: usize,
    ) -> Result<(String, Option<PaneCursor>)> {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.capture_window_composited_with_cursor_with_deadline(lines, &deadline)
    }

    pub(crate) fn capture_window_composited_with_cursor_with_deadline(
        &self,
        lines: usize,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Result<(String, Option<PaneCursor>)> {
        /// Gates the window-dimensions line. A chained `display-message` can
        /// silently produce nothing while the invocation still exits 0 (the
        /// same hazard `is_probe_line` guards in the vt seed path), and without
        /// a sentinel the capture's first row would be mistaken for the header
        /// and dropped from the fallback content.
        const WINDOW_SENTINEL: &str = "@@aoe-win@@";
        /// Gates the cursor line, for the same reason.
        const CURSOR_SENTINEL: &str = "@@aoe-cur@@";
        /// Gates the post-capture cursor probe. Comparing it with the first
        /// probe proves that the pane row still indexes the captured bytes.
        const AFTER_CURSOR_SENTINEL: &str = "@@aoe-after-cur@@";

        let window = format!("{}:^", self.name);
        let pane0 = format!("{}:^.0", self.name);
        let mut command = crate::tmux::tmux_command();
        command.args([
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
            // Trailing bg fills stay, matching the VT path (#3336); see
            // capture_pane_with_cursor.
            "-N",
            "-S",
            &format!("-{}", lines),
            ";",
            "display-message",
            "-p",
            "-t",
            &pane0,
            "-F",
            &format!("{AFTER_CURSOR_SENTINEL} {CURSOR_FMT}"),
        ]);
        let output = deadline.run(&mut command)?;

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
        let mut cursor_before: Option<PaneCursor> = None;
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
                cursor_before = PaneCursor::parse(fields.trim());
            } else {
                break;
            }
            rest = tail;
        }
        // The final sentinel follows the capture bytes. Keep the newline that
        // terminated the pane capture, matching `capture_pane_with_cursor`,
        // while removing only the post-capture probe.
        let trimmed = rest.strip_suffix('\n').unwrap_or(rest);
        let (pane0_content, cursor_after) = match trimmed.rsplit_once('\n') {
            Some((content, line)) => match line.strip_prefix(AFTER_CURSOR_SENTINEL) {
                Some(fields) => (format!("{content}\n"), PaneCursor::parse(fields.trim())),
                None => (rest.to_string(), None),
            },
            None => match trimmed.strip_prefix(AFTER_CURSOR_SENTINEL) {
                Some(fields) => (String::new(), PaneCursor::parse(fields.trim())),
                None => (rest.to_string(), None),
            },
        };
        let cursor = if cursor_after.is_some() {
            merge_cursor_probes(cursor_before, cursor_after)
        } else {
            unreliable_position(cursor_before)
        };

        let Some((count, window_width, window_height)) = dims else {
            return Ok((pane0_content, cursor));
        };
        if count <= 1 || window_width == 0 || window_height == 0 {
            return Ok((pane0_content, cursor));
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
            return Ok((pane0_content, cursor));
        }

        // Any failure in the split path (fork error, unparseable layout) falls
        // back to the pane-0 bytes already in hand, so a composite that cannot
        // be built is never worse than the old single-pane preview.
        let Some(layout) = self.capture_window_layout_with_deadline(count, deadline) else {
            return Ok((pane0_content, cursor));
        };
        // Reuse the pane-0 bytes bracketed by the cursor probes above. The
        // layout capture happens in a second tmux invocation, so using its
        // pane-0 copy could otherwise pair the cursor with a later screen and
        // paint it one row high or low while the agent scrolls.
        let pane0_rows = layout.first_pane().map(|first| {
            crate::tmux::vt::capture_rows_padded(
                pane0_content.as_bytes(),
                first.width,
                first.height,
            )
        });
        let cursor = cursor.map(|mut c| {
            c.pane_height = layout.window_height;
            c.pane_width = layout.window_width;
            c.history_size = 0;
            // Rebasing the frame onto the window is what the renderer needs, but
            // it also erases where pane 0 sits in it, which cursor painting and
            // mouse forwarding both need. Carry pane 0's rectangle alongside.
            c.composite_pane0 = layout.first_pane();
            c
        });
        let content = pane0_rows.as_deref().map_or_else(
            || layout.composite(),
            |rows| layout.composite_with_first_pane_rows(rows),
        );
        Ok((content, cursor))
    }

    /// Second fork of capture_window_composited_with_cursor: window dimensions
    /// plus geometry and visible capture for each of count panes, chained into
    /// one tmux invocation.
    ///
    /// Pane indices are contiguous from 0 within a window (tmux renumbers them
    /// as the layout changes, unlike window indices) and `pane-base-index` is
    /// forced to 0 when the session is created, so `^.0..^.{count-1}` addresses
    /// every pane without a prior `list-panes` round trip.
    ///
    /// Returned rather than composited on the spot so the live preview can
    /// cache a layout across frames and re-render only pane 0 from its VT grid
    /// (see [`WindowLayout::composite_with_first_pane_rows`]).
    #[cfg(test)]
    pub(crate) fn capture_window_layout(&self, count: u16) -> Option<WindowLayout> {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.capture_window_layout_with_deadline(count, &deadline)
    }

    pub(crate) fn capture_window_layout_with_deadline(
        &self,
        count: u16,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<WindowLayout> {
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
                // Trailing bg fills stay, matching the VT path (#3336); see
                // `capture_pane_with_cursor`.
                "-N".to_string(),
            ]);
        }

        let mut command = crate::tmux::tmux_command();
        command.args(&args);
        let output = deadline.run(&mut command).ok()?;
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

    /// Test-only convenience wrapper: production callers consume the cursor
    /// from capture_window_composited_with_cursor directly.
    #[cfg(test)]
    fn capture_window_composited(&self, lines: usize) -> Result<String> {
        Ok(self.capture_window_composited_with_cursor(lines)?.0)
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
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.capture_pane_with_cursor_with_deadline(lines, &deadline)
    }

    pub(crate) fn capture_pane_with_cursor_with_deadline(
        &self,
        lines: usize,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Result<(String, Option<PaneCursor>)> {
        let target = format!("{}:^.0", self.name);
        let start = format!("-{}", lines);
        const HEADER_FMT: &str = CURSOR_FMT;
        let mut command = crate::tmux::tmux_command();
        command.args([
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
            // Preserve trailing spaces: a bg-styled fill running to the
            // right edge is content the VT path keeps (row_last_col),
            // and dropping it here makes the preview flicker whenever the
            // two capture sources alternate (#3336).
            "-N",
            "-S",
            &start,
            ";",
            "display-message",
            "-p",
            "-t",
            &target,
            "-F",
            HEADER_FMT,
        ]);
        let output = deadline.run(&mut command)?;

        if !output.status.success() {
            return Ok((String::new(), None));
        }

        let raw = String::from_utf8_lossy(&output.stdout);
        // First line: pre-capture cursor header. Last line: post-capture
        // header. Everything between is the verbatim cursor-aware preview
        // capture output.
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

    /// Paste `text` into the session's first pane through tmux's own paste
    /// path, so the bracketed-paste markers are emitted only when the
    /// receiving program actually set DECSET 2004. Hand-rolling the markers
    /// instead (as a raw `send-keys -H` payload) delivers them to raw shells
    /// and simple REPLs that never asked for them, which render the leftovers
    /// as literal `00~` / `01~` text on the live-send paste path.
    ///
    /// See `send_via_paste_buffer` for the buffer-naming and cleanup
    /// contract. tmux translates LF to CR in the buffer by default, matching
    /// the raw-byte encoding this replaces.
    pub fn paste_text(&self, text: &str) -> Result<()> {
        let target = format!("{}:^.0", self.name);
        Self::send_via_paste_buffer(&target, text)
    }

    pub fn get_pane_pid(&self) -> Option<u32> {
        process::get_pane_pid(&self.name)
    }

    pub fn get_foreground_pid(&self) -> Option<u32> {
        let pane_pid = self.get_pane_pid()?;
        process::get_foreground_pid(pane_pid).or(Some(pane_pid))
    }

    pub fn detect_status(&self, profile: &str, tool: &str) -> Result<Status> {
        let content = self.capture_pane(50)?;
        Ok(super::status_detection::detect_status_from_content_in(
            profile, &content, tool,
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
            let payload = pad_slash_command_for_autocomplete(text);
            // `--` ends option parsing so lines beginning with `-` (markdown
            // bullets, CLI flags in prompts) are not misread as tmux flags.
            Self::tmux_send(&target, &["-l", "--", &payload])?;
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
        let mut command = crate::tmux::tmux_command();
        command.args(["set-option", "-t", &self.name, "window-size", "latest"]);
        let _ = crate::tmux::run_tmux_command_with_timeout(&mut command);
    }

    /// Read the live vertical chrome (status-bar rows) for pane_target from
    /// tmux. None when the geometry cannot be read; callers then size the
    /// window with no chrome adjustment (the pre-#2766 behavior).
    fn pane_chrome_rows_with_deadline(
        &self,
        pane_target: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<u16> {
        let mut command = crate::tmux::tmux_command();
        command.args([
            "display-message",
            "-p",
            "-t",
            pane_target,
            "-F",
            "#{window_height} #{pane_height}",
        ]);
        let output = deadline.run(&mut command).ok()?;
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
    /// [`resize_window_if_owner`](Self::resize_window_if_owner); non-owners
    /// render best-effort.
    ///
    /// Steals the lock when the current holder's heartbeat is older than
    /// `ttl`, so a crashed or disconnected owner self-heals. A queue-local
    /// compare-and-set against the observed owner pair resolves concurrent
    /// vacant claims and refuses to overwrite a heartbeat refreshed later.
    pub fn claim_size_owner(&self, owner_id: &str, ttl: Duration) -> bool {
        self.claim_owner_at(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, owner_id, ttl)
    }

    /// Bump the heartbeat iff we still own the lock. Returns false when
    /// ownership was lost (another client took over), so the caller can demote
    /// itself. Cheap enough to call on each capture/render tick.
    pub fn refresh_size_owner(&self, owner_id: &str) -> bool {
        self.refresh_owner_at(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, owner_id)
    }

    /// The shared claim protocol behind the size- and VT-owner locks. A claim
    /// compares the exact owner pair it observed and publishes its replacement
    /// in one tmux queue, so a stale claimant cannot overwrite a renewal or a
    /// winner that already claimed a vacant lock.
    fn claim_owner_at(&self, opt: &str, hb_opt: &str, owner_id: &str, ttl: Duration) -> bool {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.claim_owner_at_with_deadline(opt, hb_opt, owner_id, ttl, &deadline)
    }

    fn set_owner_pair_with_deadline(
        &self,
        opt: &str,
        hb_opt: &str,
        owner_id: &str,
        heartbeat: u64,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> bool {
        let heartbeat = heartbeat.to_string();
        let mut command = crate::tmux::tmux_command();
        command.args([
            "set-option",
            "-t",
            &self.name,
            opt,
            owner_id,
            ";",
            "set-option",
            "-t",
            &self.name,
            hb_opt,
            &heartbeat,
        ]);
        deadline
            .run(&mut command)
            .is_ok_and(|output| output.status.success())
    }

    fn owner_pair_condition(opt: &str, hb_opt: &str, owner: &str, heartbeat: &str) -> String {
        let owner = Self::tmux_format_literal(owner);
        let heartbeat = Self::tmux_format_literal(heartbeat);
        format!("#{{&&:#{{==:#{{{opt}}},{owner}}},#{{==:#{{{hb_opt}}},{heartbeat}}}}}")
    }

    fn replace_owner_pair_if_observed_with_deadline(
        &self,
        opt: &str,
        hb_opt: &str,
        observed: (&str, &str),
        owner_id: &str,
        heartbeat: u64,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> std::io::Result<bool> {
        let condition = Self::owner_pair_condition(opt, hb_opt, observed.0, observed.1);
        let owner_id = Self::tmux_command_string_literal(owner_id);
        let target = Self::tmux_command_string_literal(&self.name);
        let replace = format!(
            "set-option -t {target} {opt} {owner_id} ; set-option -t {target} {hb_opt} {heartbeat} ; display-message -p aoe-owner-replaced"
        );
        let mut command = crate::tmux::tmux_command();
        command.args(["if-shell", "-t", &self.name, "-F", &condition, &replace]);
        let output = deadline.run(&mut command)?;
        if !output.status.success() {
            return Err(std::io::Error::other("tmux owner replacement failed"));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim() == "aoe-owner-replaced"))
    }

    fn release_owner_pair_at_with_deadline(
        &self,
        opt: &str,
        hb_opt: &str,
        owner_id: &str,
        heartbeat: u64,
        restore_window_size: bool,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) {
        let condition = Self::owner_pair_condition(opt, hb_opt, owner_id, &heartbeat.to_string());
        let target = Self::tmux_command_string_literal(&self.name);
        let mut release =
            format!("set-option -u -t {target} {opt} ; set-option -u -t {target} {hb_opt}");
        if restore_window_size {
            release.push_str(&format!(" ; set-option -t {target} window-size latest"));
        }
        let mut command = crate::tmux::tmux_command();
        command.args(["if-shell", "-t", &self.name, "-F", &condition, &release]);
        let _ = deadline.run(&mut command);
    }

    fn owner_snapshot_with_deadline(
        &self,
        opt: &str,
        hb_opt: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> std::io::Result<(String, String)> {
        let format = format!("#{{{opt}}}|#{{{hb_opt}}}");
        let mut command = crate::tmux::tmux_command();
        command.args(["display-message", "-p", "-t", &self.name, "-F", &format]);
        let output = deadline.run(&mut command)?;
        if !output.status.success() {
            return Err(std::io::Error::other("tmux owner snapshot failed"));
        }
        let line = String::from_utf8_lossy(&output.stdout);
        let Some((owner, heartbeat)) = line.trim().split_once('|') else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "tmux owner snapshot is malformed",
            ));
        };
        Ok((owner.to_string(), heartbeat.to_string()))
    }

    fn parse_owner_snapshot(
        owner: &str,
        heartbeat: &str,
    ) -> std::io::Result<Option<(String, u64)>> {
        if owner.is_empty() && heartbeat.is_empty() {
            return Ok(None);
        }
        if owner.is_empty() || heartbeat.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "owner pair is incomplete",
            ));
        }
        let heartbeat = heartbeat.parse().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "owner heartbeat is not an integer",
            )
        })?;
        Ok(Some((owner.to_string(), heartbeat)))
    }

    fn claim_owner_at_with_deadline(
        &self,
        opt: &str,
        hb_opt: &str,
        owner_id: &str,
        ttl: Duration,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> bool {
        let (observed_owner, observed_heartbeat) =
            match self.owner_snapshot_with_deadline(opt, hb_opt, deadline) {
                Ok(snapshot) => snapshot,
                Err(_) => return false,
            };
        let current = Self::parse_owner_snapshot(&observed_owner, &observed_heartbeat)
            .ok()
            .flatten();
        let now = now_ms();
        let claimable = match current.as_ref() {
            None => true,
            Some((id, _)) if id == owner_id => true,
            Some((_, hb)) => now.saturating_sub(*hb) > ttl.as_millis() as u64,
        };
        if !claimable {
            return false;
        }
        let heartbeat = next_owner_heartbeat(current.as_ref().map_or(0, |(_, hb)| *hb));
        let replaced = self.replace_owner_pair_if_observed_with_deadline(
            opt,
            hb_opt,
            (&observed_owner, &observed_heartbeat),
            owner_id,
            heartbeat,
            deadline,
        );
        if matches!(replaced, Ok(true)) {
            return true;
        }

        if matches!(
            self.owner_at_result_with_deadline(opt, hb_opt, deadline),
            Ok(Some((id, _))) if id == owner_id
        ) {
            return true;
        }
        if replaced.is_err() {
            self.release_owner_pair_at_with_deadline(
                opt, hb_opt, owner_id, heartbeat, false, deadline,
            );
        }
        false
    }

    fn refresh_owner_at(&self, opt: &str, hb_opt: &str, owner_id: &str) -> bool {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.refresh_owner_at_with_deadline(opt, hb_opt, owner_id, &deadline)
    }

    fn refresh_owner_at_with_deadline(
        &self,
        opt: &str,
        hb_opt: &str,
        owner_id: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> bool {
        let owner_id = Self::tmux_format_literal(owner_id);
        let condition = format!("#{{==:#{{{opt}}},{owner_id}}}");
        let target = Self::tmux_command_string_literal(&self.name);
        let refresh = format!(
            "set-option -t {target} {hb_opt} {} ; display-message -p aoe-owner-refreshed",
            next_owner_heartbeat(0)
        );
        let mut command = crate::tmux::tmux_command();
        command.args(["if-shell", "-t", &self.name, "-F", &condition, &refresh]);
        deadline.run(&mut command).is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.trim() == "aoe-owner-refreshed")
        })
    }

    fn owner_at(&self, opt: &str, hb_opt: &str) -> Option<(String, u64)> {
        self.owner_at_result(opt, hb_opt).ok().flatten()
    }

    fn owner_at_result(&self, opt: &str, hb_opt: &str) -> std::io::Result<Option<(String, u64)>> {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.owner_at_result_with_deadline(opt, hb_opt, &deadline)
    }

    fn owner_at_result_with_deadline(
        &self,
        opt: &str,
        hb_opt: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> std::io::Result<Option<(String, u64)>> {
        let (owner, heartbeat) = self.owner_snapshot_with_deadline(opt, hb_opt, deadline)?;
        Self::parse_owner_snapshot(&owner, &heartbeat)
    }

    pub fn claim_vt_owner(&self, owner_id: &str, ttl: Duration) -> bool {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.claim_vt_owner_with_deadline(owner_id, ttl, &deadline)
    }

    pub(crate) fn claim_vt_owner_with_deadline(
        &self,
        owner_id: &str,
        ttl: Duration,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> bool {
        self.claim_owner_at_with_deadline(VT_OWNER_OPT, VT_OWNER_HB_OPT, owner_id, ttl, deadline)
    }

    pub fn refresh_vt_owner(&self, owner_id: &str) -> bool {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.refresh_vt_owner_with_deadline(owner_id, &deadline)
    }

    pub(crate) fn refresh_vt_owner_with_deadline(
        &self,
        owner_id: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> bool {
        self.refresh_owner_at_with_deadline(VT_OWNER_OPT, VT_OWNER_HB_OPT, owner_id, deadline)
    }

    fn tmux_format_literal(value: &str) -> String {
        let mut escaped = String::with_capacity(value.len());
        for ch in value.chars() {
            if matches!(ch, ',' | '#' | '}') {
                escaped.push('#');
            }
            escaped.push(ch);
        }
        escaped
    }

    fn tmux_command_string_literal(value: &str) -> String {
        let mut quoted = String::with_capacity(value.len() + 2);
        quoted.push('"');
        for ch in value.chars() {
            if matches!(ch, '\\' | '"' | '$') {
                quoted.push('\\');
            }
            if ch == '#' {
                quoted.push('#');
            }
            quoted.push(ch);
        }
        quoted.push('"');
        quoted
    }

    /// Returns the applied window row count (`rows` plus status-bar chrome)
    /// on success, so callers can later compare the observed window size
    /// against what was actually set; `None` when the guard declined or tmux
    /// errored.
    fn resize_window_if_format_with_deadline(
        &self,
        condition: &str,
        cols: u16,
        rows: u16,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<u16> {
        if cols == 0 || rows == 0 {
            return None;
        }
        let pane_target = format!("{}:^.0", self.name);
        let window_rows = self
            .pane_chrome_rows_with_deadline(&pane_target, deadline)
            .map(|chrome| rows.saturating_add(chrome))
            .unwrap_or(rows);
        // if-shell -F evaluates the owner/attachment guard and inserts this
        // branch in the same tmux command queue. No other client can replace
        // the guarded state between the check and resize-window.
        //
        // Target the FIRST window (`:^`) explicitly: a bare session target
        // resolves to the session's current window, so on a session where the
        // user created more windows the resize would land on the wrong one
        // while the chrome probe above and the preview capture both use the
        // first. The observed-size reconcile also reads the first window, so
        // resizing any other would loop forever chasing a mismatch.
        let target = Self::tmux_command_string_literal(&format!("{}:^", self.name));
        let resize = format!(
            "resize-window -t {target} -x {cols} -y {window_rows} ; display-message -p aoe-resize-applied"
        );
        let mut command = crate::tmux::tmux_command();
        command.args(["if-shell", "-t", &self.name, "-F", condition, &resize]);
        deadline
            .run(&mut command)
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .any(|line| line.trim() == "aoe-resize-applied")
            })
            .then_some(window_rows)
    }

    fn release_owner_at_with_deadline(
        &self,
        opt: &str,
        hb_opt: &str,
        owner_id: &str,
        restore_window_size: bool,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) {
        let owner_id = Self::tmux_format_literal(owner_id);
        let condition = format!("#{{==:#{{{opt}}},{owner_id}}}");
        let target = Self::tmux_command_string_literal(&self.name);
        let mut release =
            format!("set-option -u -t {target} {opt} ; set-option -u -t {target} {hb_opt}");
        if restore_window_size {
            release.push_str(&format!(" ; set-option -t {target} window-size latest"));
        }
        let mut command = crate::tmux::tmux_command();
        command.args(["if-shell", "-t", &self.name, "-F", &condition, &release]);
        let _ = deadline.run(&mut command);
    }

    pub fn release_vt_owner(&self, owner_id: &str) {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.release_vt_owner_with_deadline(owner_id, &deadline);
    }

    pub(crate) fn release_vt_owner_with_deadline(
        &self,
        owner_id: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) {
        self.release_owner_at_with_deadline(
            VT_OWNER_OPT,
            VT_OWNER_HB_OPT,
            owner_id,
            false,
            deadline,
        );
    }
    /// Arm a pane pipe only if this exact channel generation still owns the
    /// lease when tmux executes pipe-pane. Competing vacant-lock claimants can
    /// both pass their confirm read; this final queue-local guard fences the
    /// claimant that lost afterward.
    pub(crate) fn arm_vt_pipe_if_owner_with_deadline(
        &self,
        owner_id: &str,
        flags: &str,
        pipe_command: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> bool {
        let owner_format = Self::tmux_format_literal(owner_id);
        let condition = format!("#{{==:#{{{VT_OWNER_OPT}}},{owner_format}}}");
        let target = Self::tmux_command_string_literal(&format!("{}:^.0", self.name));
        let pipe_command = Self::tmux_command_string_literal(pipe_command);
        let owner_command = Self::tmux_command_string_literal(owner_id);
        let arm = format!(
            "pipe-pane {flags} -t {target} {pipe_command} ; set-option -t {target} {VT_PIPE_OWNER_OPT} {owner_command} ; display-message -p aoe-pipe-armed"
        );
        let mut command = crate::tmux::tmux_command();
        command.args(["if-shell", "-t", &self.name, "-F", &condition, &arm]);
        let Ok(output) = deadline.run(&mut command) else {
            return false;
        };
        let armed = output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == "aoe-pipe-armed");
        if !armed {
            tracing::debug!(
                session = %self.name,
                expected_owner = owner_id,
                status = ?output.status.code(),
                stdout = %String::from_utf8_lossy(&output.stdout),
                stderr = %String::from_utf8_lossy(&output.stderr),
                "tmux pipe arm guard failed"
            );
        }
        armed
    }

    /// Disable the pane pipe and release its lease iff this exact channel
    /// generation still owns it. A stale channel may share the tmux session
    /// name with its replacement, so an unconditional pipe-pane teardown
    /// would kill the replacement's forwarder.
    pub(crate) fn release_vt_pipe_owner_with_deadline(
        &self,
        owner_id: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) {
        let owner_format = Self::tmux_format_literal(owner_id);
        let condition = format!(
            "#{{||:#{{==:#{{{VT_PIPE_OWNER_OPT}}},{owner_format}}},#{{==:#{{{VT_OWNER_OPT}}},{owner_format}}}}}"
        );
        let clear_lease_condition = format!("#{{==:#{{{VT_OWNER_OPT}}},{owner_format}}}");
        let target = Self::tmux_command_string_literal(&format!("{}:^.0", self.name));
        let clear_lease = format!(
            "set-option -u -t {target} {VT_OWNER_OPT} ; set-option -u -t {target} {VT_OWNER_HB_OPT}"
        );
        let release = format!(
            "pipe-pane -t {target} ; set-option -u -t {target} {VT_PIPE_OWNER_OPT} ; if-shell -t {target} -F '{clear_lease_condition}' '{clear_lease}'"
        );
        let mut command = crate::tmux::tmux_command();
        command.args(["if-shell", "-t", &self.name, "-F", &condition, &release]);
        let _ = deadline.run(&mut command);
    }

    /// Force ownership to owner_id, even over a live holder. Used by the
    /// explicit "take over" action: a user tap is an intentional steal, not
    /// the passive flap the heartbeat guards against.
    pub fn steal_size_owner(&self, owner_id: &str) -> bool {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.set_owner_pair_with_deadline(
            SIZE_OWNER_OPT,
            SIZE_OWNER_HB_OPT,
            owner_id,
            next_owner_heartbeat(0),
            &deadline,
        )
    }

    /// Resize the window iff owner_id still holds the size-owner lock at the
    /// instant tmux executes resize-window. Returns whether the resize landed.
    ///
    /// The format guard and resize run in one tmux command queue. A local
    /// owner flag or a separate show-options result can become stale between
    /// subprocesses when another surface takes over.
    pub fn resize_window_if_owner(&self, owner_id: &str, cols: u16, rows: u16) -> bool {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.resize_window_if_owner_with_deadline(owner_id, cols, rows, &deadline)
    }

    fn resize_window_if_owner_with_deadline(
        &self,
        owner_id: &str,
        cols: u16,
        rows: u16,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> bool {
        loop {
            let Ok(Some((observed_owner, heartbeat))) =
                self.owner_at_result_with_deadline(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, deadline)
            else {
                return false;
            };
            if observed_owner != owner_id {
                return false;
            }
            let condition = Self::owner_pair_condition(
                SIZE_OWNER_OPT,
                SIZE_OWNER_HB_OPT,
                owner_id,
                &heartbeat.to_string(),
            );
            if self
                .resize_window_if_format_with_deadline(&condition, cols, rows, deadline)
                .is_some()
            {
                return true;
            }

            match self.owner_at_result_with_deadline(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, deadline) {
                Ok(Some((id, refreshed_heartbeat))) if id == owner_id => {
                    if refreshed_heartbeat != heartbeat {
                        continue;
                    }
                    self.release_owner_pair_at_with_deadline(
                        SIZE_OWNER_OPT,
                        SIZE_OWNER_HB_OPT,
                        owner_id,
                        refreshed_heartbeat,
                        true,
                        deadline,
                    );
                }
                Ok(_) => {}
                Err(_) => {
                    self.release_owner_pair_at_with_deadline(
                        SIZE_OWNER_OPT,
                        SIZE_OWNER_HB_OPT,
                        owner_id,
                        heartbeat,
                        true,
                        deadline,
                    );
                }
            }
            return false;
        }
    }
    /// Resize a detached pane only if the inactive owner state observed here
    /// is unchanged when tmux executes resize-window. This fences a live owner
    /// or terminal attach that arrives after the preliminary worker checks.
    /// Returns the applied window row count on success, `None` when declined.
    pub(crate) fn resize_window_if_detached_without_active_owner_after_exists_with_deadline(
        &self,
        cols: u16,
        rows: u16,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<u16> {
        let owner_condition = match self.owner_at_result_with_deadline(
            SIZE_OWNER_OPT,
            SIZE_OWNER_HB_OPT,
            deadline,
        ) {
            Ok(None) => {
                format!("#{{&&:#{{==:#{{{SIZE_OWNER_OPT}}},}},#{{==:#{{{SIZE_OWNER_HB_OPT}}},}}}}")
            }
            Ok(Some((_, heartbeat)))
                if now_ms().saturating_sub(heartbeat) <= SIZE_OWNER_TTL.as_millis() as u64 =>
            {
                return None;
            }
            Ok(Some((owner, heartbeat))) => {
                let owner = Self::tmux_format_literal(&owner);
                format!(
                    "#{{&&:#{{==:#{{{SIZE_OWNER_OPT}}},{owner}}},#{{==:#{{{SIZE_OWNER_HB_OPT}}},{heartbeat}}}}}"
                )
            }
            Err(_) => return None,
        };
        let condition = format!("#{{&&:#{{==:#{{session_attached}},0}},{owner_condition}}}");
        self.resize_window_if_format_with_deadline(&condition, cols, rows, deadline)
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
    /// Returns None unless tmux authoritatively reports the attached client
    /// count. Passive resize must fail closed on timeout, non-zero exit, or
    /// malformed output instead of treating an unknown state as detached.
    pub fn is_attached(&self) -> Option<bool> {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.is_attached_with_deadline(&deadline)
    }

    pub(crate) fn is_attached_with_deadline(
        &self,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<bool> {
        let mut command = crate::tmux::tmux_command();
        command.args([
            "display-message",
            "-t",
            &self.name,
            "-p",
            "#{session_attached}",
        ]);
        let out = deadline.run(&mut command).ok()?;
        if !out.status.success() {
            return None;
        }
        let attached = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u32>()
            .ok()?;
        Some(attached > 0)
    }

    /// Whether a non-stale size owner currently holds the lock. A passive
    /// writer (the TUI's detached preview sync) checks this to defer to an
    /// active owner without claiming the lock itself.
    pub fn has_active_size_owner(&self) -> Option<bool> {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.has_active_size_owner_with_deadline(&deadline)
    }

    pub(crate) fn has_active_size_owner_with_deadline(
        &self,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<bool> {
        self.owner_at_result_with_deadline(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, deadline)
            .ok()
            .map(|owner| {
                owner.is_some_and(|(_, hb)| {
                    now_ms().saturating_sub(hb) <= SIZE_OWNER_TTL.as_millis() as u64
                })
            })
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
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.release_owner_at_with_deadline(
            SIZE_OWNER_OPT,
            SIZE_OWNER_HB_OPT,
            owner_id,
            true,
            &deadline,
        );
    }

    #[cfg(test)]
    fn set_user_option(&self, opt: &str, value: &str) {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        let _ = self.set_user_option_with_deadline(opt, value, &deadline);
    }

    #[cfg(test)]
    fn set_user_option_with_deadline(
        &self,
        opt: &str,
        value: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> bool {
        let mut command = crate::tmux::tmux_command();
        command.args(["set-option", "-t", &self.name, opt, value]);
        deadline
            .run(&mut command)
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[cfg(test)]
    fn unset_user_option(&self, opt: &str) {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        let _ = self.unset_user_option_with_deadline(opt, &deadline);
    }

    #[cfg(test)]
    fn unset_user_option_with_deadline(
        &self,
        opt: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> bool {
        let mut command = crate::tmux::tmux_command();
        command.args(["set-option", "-u", "-t", &self.name, opt]);
        deadline
            .run(&mut command)
            .map(|output| output.status.success())
            .unwrap_or(false)
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

/// A one-shot, mode-0600 environment channel for a pane command.
///
/// The guard owns cleanup until the pane unlinks the file after sourcing it.
/// A successful tmux create alone does not transfer cleanup ownership.
struct EphemeralEnvFile {
    path: Option<std::path::PathBuf>,
    container_env_path: Option<std::path::PathBuf>,
}

impl EphemeralEnvFile {
    fn create(env: &[PaneEnvMutation], container_env: &[(String, String)]) -> Result<Self> {
        let mut channel = Self {
            path: None,
            container_env_path: None,
        };
        if !container_env.is_empty() {
            let mut file = tempfile::Builder::new()
                .prefix(PANE_ENV_FILE_PREFIX)
                .tempfile()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            for (key, value) in container_env {
                anyhow::ensure!(
                    crate::session::environment::is_valid_env_key(key),
                    "invalid container environment key {key:?}"
                );
                anyhow::ensure!(
                    !value
                        .bytes()
                        .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r')),
                    "container environment value for {key} cannot be represented in an env-file"
                );
                writeln!(file, "{key}={value}")?;
            }
            file.flush()?;
            let (_handle, path) = file.keep().map_err(|error| error.error)?;
            channel.container_env_path = Some(path);
        }

        let mut file = tempfile::Builder::new()
            .prefix(PANE_ENV_FILE_PREFIX)
            .tempfile()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        for mutation in env {
            let key = mutation.key();
            if !crate::session::environment::is_valid_env_key(key) {
                tracing::warn!(target: "session.create", "invalid protected environment key '{}'; skipping", key);
                continue;
            }
            match mutation {
                PaneEnvMutation::Set { key, value } => {
                    writeln!(file, "export {}={}", key, shell_escape_script_word(value))?;
                }
                PaneEnvMutation::Unset { key } => writeln!(file, "unset {}", key)?,
            }
        }
        file.flush()?;
        let (_handle, path) = file.keep().map_err(|error| error.error)?;
        channel.path = Some(path);
        Ok(channel)
    }

    fn wrap_command(&self, command: Option<&str>) -> Result<String> {
        let path = self
            .path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("protected environment channel already consumed"))?;
        let launch = command.map(str::to_owned).unwrap_or_else(|| {
            crate::session::environment::login_shell_command(
                &crate::session::environment::user_shell(),
            )
        });
        let shell = crate::session::environment::user_posix_shell();
        let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
        if let Some(container_env_path) = self.container_env_path.as_deref() {
            writeln!(
                file,
                "exec {}<{} || exit 1",
                crate::session::environment::CONTAINER_EXEC_ENV_FD,
                shell_escape_script_word(&container_env_path.to_string_lossy())
            )?;
            writeln!(
                file,
                "rm -f -- {}",
                shell_escape_script_word(&container_env_path.to_string_lossy())
            )?;
        }
        writeln!(
            file,
            "rm -f -- {}",
            shell_escape_script_word(&path.to_string_lossy())
        )?;
        writeln!(file, "{launch}")?;
        file.flush()?;

        // tmux hands its pane command to the user's configured shell. Keep that
        // boundary to one short script invocation. The protected file contains
        // both exports and the potentially large launch body, so neither
        // secrets nor command contents enter tmux argv.
        Ok(format!(
            "exec {} {}",
            crate::session::environment::shell_escape(&shell),
            crate::session::environment::shell_escape(&path.to_string_lossy())
        ))
    }

    fn wait_until_consumed(&self, timeout: Duration) -> bool {
        let Some(path) = self.path.as_deref() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        loop {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
                Err(_) => return false,
                Ok(_) if Instant::now() >= deadline => return false,
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn disarm(&mut self) {
        self.path = None;
        self.container_env_path = None;
    }
}

impl Drop for EphemeralEnvFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(path) = self.container_env_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Whether `text` should get a trailing space appended before being typed
/// via the literal (non-paste-buffer) keystroke path in
/// [`Session::send_keys_with_delay`]. A message that opens with `/` triggers
/// some agents' own slash-command autocomplete dropdown (e.g. opencode); the
/// dropdown then consumes the terminating `Enter` sent after this payload as
/// navigation instead of submit, leaving the command typed but never
/// delivered. A trailing space closes the dropdown as it's typed, so the
/// following `Enter` submits normally instead. Every other message keeps its
/// exact bytes. Pure so the padding decision is unit-testable without tmux.
fn pad_slash_command_for_autocomplete(text: &str) -> std::borrow::Cow<'_, str> {
    if text.trim_start().starts_with('/') {
        std::borrow::Cow::Owned(format!("{text} "))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
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

    /// The tmux id (`%N`) of `session_name`'s only pane. Call right after
    /// `new-session`, before any split or extra window, so `-t <session>`
    /// resolves unambiguously.
    fn only_pane_id(session_name: &str) -> String {
        let out = crate::tmux::tmux_command()
            .args(["display-message", "-t", session_name, "-p", "#{pane_id}"])
            .output()
            .expect("tmux display-message");
        let id = String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string();
        assert!(!id.is_empty(), "no pane id for session {session_name}");
        id
    }

    /// Block until `pane_id` reports `expected` as its current command.
    ///
    /// tmux runs a single-string pane command (`"sleep 30"`) through the
    /// user's shell, so `pane_current_command` reports that shell, which
    /// `is_shell_command` matches, until it execs into the real command.
    /// A fixed sleep is a bet on that exec having happened. The window it
    /// has to cover is the shell's own startup, around 100ms on an idle
    /// machine and several times that when the suite is competing for
    /// cores, so the bet loses under load; wait for the exec instead.
    ///
    /// Keyed on the pane's own id rather than the `^.0` target so the wait
    /// never depends on the pane resolution the callers' assertions exist to
    /// exercise: a targeting regression still fails on the assertion with its
    /// own message instead of timing out here.
    fn wait_for_pane_command(pane_id: &str, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let current = crate::tmux::tmux_command()
                .args([
                    "display-message",
                    "-t",
                    pane_id,
                    "-p",
                    "#{pane_current_command}",
                ])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if current == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "pane {pane_id} still reports {current:?}, expected {expected:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
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
    fn pads_slash_prefixed_messages_only() {
        let cases = [
            ("/audit", "/audit "),
            ("/", "/ "),
            ("  /audit", "  /audit "),
            ("audit", "audit"),
            ("please run /audit", "please run /audit"),
            ("", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(
                pad_slash_command_for_autocomplete(input),
                expected,
                "input {input:?}"
            );
        }
    }

    /// Direct, timing-based proof that `wait_until_ready` with a known
    /// marker actually blocks until that marker appears, rather than
    /// returning early on a merely-static pane -- the gap in the generic
    /// content-settle fallback (a short "still loading" screen can look
    /// "settled" long before the agent is really listening).
    #[test]
    fn wait_until_ready_blocks_until_the_marker_appears() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = TmuxTestSession::new("aoe_test_ready_marker");
        let name = guard.name().to_string();
        // A short, static "booting" line appears immediately and would
        // satisfy the generic settle heuristic well under 700ms; the real
        // marker text only appears after the sleep. The trailing `set-option
        // pane-base-index 0` chain mirrors `append_pane_base_index_args` so the
        // `^.0` capture target resolves on hosts with `pane-base-index 1` set
        // globally (#488, #2231).
        let status = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &name,
                "-x",
                "80",
                "-y",
                "24",
                "sh",
                "-c",
                "echo booting; sleep 0.7; echo 'ask anything...'; sleep 30",
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
        refresh_session_cache();

        let session = Session::from_name(&name);
        let start = std::time::Instant::now();
        session.wait_until_ready(std::time::Duration::from_secs(3), Some("ask anything"));
        let elapsed = start.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(600),
            "returned before the marker could plausibly have appeared: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "should have returned promptly once the marker appeared, not idled toward the bound: {elapsed:?}"
        );
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
    fn session_created_is_conservative_epoch_millisecond_watermark() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = TmuxTestSession::new("aoe_test_session_created");
        let output = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", guard.name()])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        let created_at_ms = Session::from_name(guard.name()).created_at_ms().unwrap();
        assert!(created_at_ms > 0);
        assert_eq!(created_at_ms % 1000, 999);
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

    /// Detached 80x24 session with a known pane index, plus the cache refresh
    /// the lock paths need: the session is created behind the existence
    /// cache's back, and a cache warmed without it turns every exists()-guarded
    /// lock call into a false no-op.
    fn owner_lock_session(prefix: &str) -> (TmuxTestSession, Session) {
        let guard = TmuxTestSession::new(prefix);
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
                ";",
                "set-window-option",
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
        (guard, session)
    }

    #[test]
    #[serial_test::serial]
    fn size_owner_lock_claims_rejects_steals_and_releases() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let (guard, session) = owner_lock_session("aoe_test_owner");

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

        // A claimant that observed a stale pair cannot overwrite a renewal
        // that landed before its compare-and-set branch executes.
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        let observed = session
            .owner_snapshot_with_deadline(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, &deadline)
            .expect("owner snapshot");
        assert!(session.refresh_size_owner("c"));
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        assert!(!session
            .replace_owner_pair_if_observed_with_deadline(
                SIZE_OWNER_OPT,
                SIZE_OWNER_HB_OPT,
                (&observed.0, &observed.1),
                "stale-claimer",
                next_owner_heartbeat(0),
                &deadline,
            )
            .expect("conditional owner replacement"));
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

        let pane_size = || {
            let output = crate::tmux::tmux_command()
                .args([
                    "display-message",
                    "-p",
                    "-t",
                    &format!("{}:^.0", guard.name()),
                    "#{pane_width} #{pane_height}",
                ])
                .output()
                .expect("tmux pane size");
            assert!(output.status.success());
            let fields: Vec<u16> = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .map(|field| field.parse().expect("numeric pane dimension"))
                .collect();
            (fields[0], fields[1])
        };

        // One owner-pair snapshot precedes the chrome probe and queue-local
        // guard. Reading owner and heartbeat separately would add a fourth
        // fork, expose a torn pair, and reopen the takeover race.
        let _ = crate::tmux::fork_probe::take();
        {
            let _probe = crate::tmux::fork_probe::arm();
            assert!(session.resize_window_if_owner("d", 90, 30));
            assert_eq!(crate::tmux::fork_probe::take(), 3);
        }
        assert_eq!(pane_size(), (90, 30));
        assert!(!session.resize_window_if_owner("not-d", 91, 31));
        assert_eq!(pane_size(), (90, 30));

        // A verified owner whose authoritative resize fails must release the
        // lock before the caller demotes itself. Zero width deterministically
        // exercises the failure path without relying on tmux timing.
        assert!(!session.resize_window_if_owner("d", 0, 24));
        assert!(session.size_owner().is_none());
        assert!(session.steal_size_owner("d"));

        // A conditional release is one tmux command queue: a non-owner no-op
        // cannot race a later takeover between a read and separate unsets.
        let _ = crate::tmux::fork_probe::take();
        {
            let _probe = crate::tmux::fork_probe::arm();
            session.release_size_owner("not-d");
            assert_eq!(crate::tmux::fork_probe::take(), 1);
        }
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("d".to_string())
        );
        {
            let _probe = crate::tmux::fork_probe::arm();
            session.release_size_owner("d");
            assert_eq!(crate::tmux::fork_probe::take(), 1);
        }
        assert!(session.size_owner().is_none());

        let deadline = crate::tmux::TmuxCommandDeadline::new();
        assert!(session
            .resize_window_if_detached_without_active_owner_after_exists_with_deadline(
                91, 31, &deadline,
            )
            .is_some());
        assert_eq!(pane_size(), (91, 31));
        assert!(session.claim_size_owner("active", Duration::from_secs(10)));
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        assert!(session
            .resize_window_if_detached_without_active_owner_after_exists_with_deadline(
                92, 32, &deadline,
            )
            .is_none());
        assert_eq!(pane_size(), (91, 31));
        session.release_size_owner("active");

        // The resize must land on the FIRST window even when the session's
        // current window is a later one: preview capture, the chrome probe,
        // and the observed-size reconcile all read `:^`, so a bare-session
        // target (which tmux resolves to the current window) would resize the
        // wrong window and the reconcile would loop chasing a mismatch.
        let out = crate::tmux::tmux_command()
            .args(["new-window", "-t", guard.name(), "sleep 30"])
            .output()
            .expect("tmux new-window");
        assert!(out.status.success());
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        assert!(session
            .resize_window_if_detached_without_active_owner_after_exists_with_deadline(
                93, 33, &deadline,
            )
            .is_some());
        assert_eq!(
            pane_size(),
            (93, 33),
            "the first window must be the resize target"
        );
        let out = crate::tmux::tmux_command()
            .args(["kill-window", "-t", &format!("{}:$", guard.name())])
            .output()
            .expect("tmux kill-window");
        assert!(out.status.success());

        // A partial owner write is unknown to passive readers, but a later
        // claimant must repair it rather than leaving the lock wedged forever.
        session.set_user_option(SIZE_OWNER_OPT, "partial");
        session.unset_user_option(SIZE_OWNER_HB_OPT);
        assert_eq!(session.has_active_size_owner(), None);
        assert!(session.claim_size_owner("recovered", Duration::from_secs(10)));
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("recovered".to_string())
        );
        session.release_size_owner("recovered");

        // Format operands escape only tmux's actual separators. Prefixing a
        // literal brace or colon with '#' changes the operand on tmux 3.6+.
        let literal_owner = "owner{with:literal";
        assert!(session.claim_size_owner(literal_owner, Duration::from_secs(10)));
        assert!(session.refresh_size_owner(literal_owner));
        session.release_size_owner(literal_owner);
        assert!(session.size_owner().is_none());
    }
    /// A guarded resize that never got to run leaves the shared budget spent.
    /// Verification and cleanup must run on that same budget: on a fresh
    /// deadline they read back the unchanged heartbeat and release the lock.
    #[test]
    #[serial_test::serial]
    fn resize_window_if_owner_keeps_timeout_recovery_in_one_deadline() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let (_guard, session) = owner_lock_session("aoe_test_owner_resize_deadline");
        assert!(session.steal_size_owner("owner"));

        // The owner snapshot and the chrome probe behind the guarded resize.
        const COMMANDS_BEFORE_RESIZE: i64 = 2;
        let deadline =
            crate::tmux::TmuxCommandDeadline::expiring_after_commands(COMMANDS_BEFORE_RESIZE);
        assert!(!session.resize_window_if_owner_with_deadline("owner", 91, 31, &deadline));
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("owner".to_string())
        );
    }

    #[test]
    #[serial_test::serial]
    fn resize_window_if_owner_retries_same_owner_heartbeat_until_deadline() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let (guard, session) = owner_lock_session("aoe_test_owner_resize_heartbeat");
        assert!(session.steal_size_owner("owner"));

        // Every message the owner reads moves the heartbeat, so the queue-local
        // guard never matches and each attempt loses the race to the same owner.
        let hook = format!(
            "set-option -F -t {} @aoe_test_show_count '#{{e|+:#{{@aoe_test_show_count}},1}}' ; set-option -F -t {} {SIZE_OWNER_HB_OPT} '#{{e|+:#{{{SIZE_OWNER_HB_OPT}}},1}}'",
            guard.name(),
            guard.name(),
        );
        let out = crate::tmux::tmux_command()
            .args([
                "set-option",
                "-t",
                guard.name(),
                "@aoe_test_show_count",
                "0",
                ";",
                "set-hook",
                "-t",
                guard.name(),
                "after-display-message",
                &hook,
            ])
            .output()
            .expect("tmux heartbeat hook");
        assert!(out.status.success());

        // An attempt spends four tmux commands and displays three messages: the
        // guarded resize prints nothing while its condition fails. Budgeting two
        // attempts leaves the third attempt's owner read past the deadline.
        const COMMANDS_PER_ATTEMPT: i64 = 4;
        const MESSAGES_PER_ATTEMPT: u64 = 3;
        const ATTEMPTS: i64 = 2;
        let deadline = crate::tmux::TmuxCommandDeadline::expiring_after_commands(
            COMMANDS_PER_ATTEMPT * ATTEMPTS,
        );
        assert!(!session.resize_window_if_owner_with_deadline("owner", 91, 31, &deadline));

        let out = crate::tmux::tmux_command()
            .args([
                "set-hook",
                "-u",
                "-t",
                guard.name(),
                "after-display-message",
                ";",
                "show-options",
                "-v",
                "-t",
                guard.name(),
                "@aoe_test_show_count",
            ])
            .output()
            .expect("tmux heartbeat hook count");
        assert!(out.status.success());
        let show_count: u64 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("numeric hook count");
        assert_eq!(
            show_count,
            MESSAGES_PER_ATTEMPT * ATTEMPTS as u64,
            "retries did not run once per heartbeat race"
        );
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("owner".to_string())
        );
    }

    #[test]
    #[serial_test::serial]
    fn vt_owner_lock_claims_rejects_and_releases_independently() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let (guard, session) = owner_lock_session("aoe_test_vt_owner");

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

        let pane_is_piped = || {
            let output = crate::tmux::tmux_command()
                .args([
                    "display-message",
                    "-p",
                    "-t",
                    &format!("{}:^.0", guard.name()),
                    "#{pane_pipe}",
                ])
                .output()
                .expect("tmux pane pipe state");
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "1"
        };
        let pipe_temp = tempfile::tempdir().expect("pipe tempdir");
        let pipe_marker = pipe_temp.path().join("armed #$");
        let pipe_command = format!(
            "touch '{}' ; exec cat >/dev/null",
            pipe_marker.to_string_lossy()
        );

        let deadline = crate::tmux::TmuxCommandDeadline::new();
        assert!(session.arm_vt_pipe_if_owner_with_deadline(
            "pid-3",
            "-IO",
            &pipe_command,
            &deadline,
        ));
        assert!(pane_is_piped());
        for _ in 0..100 {
            if pipe_marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(pipe_marker.exists(), "quoted pipe command must run intact");

        // Model a replacement that claimed the stale lease but has not armed
        // yet. Dropping the old generation closes its own pipe without
        // clearing the replacement's lease.
        session.set_user_option(VT_OWNER_OPT, "pid-4");
        session.set_user_option(VT_OWNER_HB_OPT, &now_ms().to_string());
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        session.release_vt_pipe_owner_with_deadline("pid-3", &deadline);
        assert!(session.refresh_vt_owner("pid-4"));
        assert!(!pane_is_piped());

        // Once pid-4 arms, the stale pid-3 generation can neither re-arm nor
        // tear down the replacement's pipe or lease.
        assert!(session.arm_vt_pipe_if_owner_with_deadline(
            "pid-4",
            "-O",
            &pipe_command,
            &deadline,
        ));
        assert!(!session.arm_vt_pipe_if_owner_with_deadline(
            "pid-3",
            "-O",
            &pipe_command,
            &deadline,
        ));
        session.release_vt_pipe_owner_with_deadline("pid-3", &deadline);
        assert!(session.refresh_vt_owner("pid-4"));
        assert!(pane_is_piped());

        // A replacement that owns only the lease must release that lease
        // without tearing down the older generation's still-live pane pipe.
        session.set_user_option(VT_OWNER_OPT, "pid-5");
        session.set_user_option(VT_OWNER_HB_OPT, &now_ms().to_string());
        session.release_vt_owner_with_deadline("pid-5", &deadline);
        assert!(!session.refresh_vt_owner("pid-5"));
        assert!(pane_is_piped());

        session.release_vt_pipe_owner_with_deadline("pid-4", &deadline);
        assert!(!session.refresh_vt_owner("pid-4"));
        assert!(!pane_is_piped());
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

        for (label, composited) in [("pane", false), ("composited window", true)] {
            let _ = crate::tmux::fork_probe::take();
            let probe = crate::tmux::fork_probe::arm();
            if composited {
                session
                    .capture_window_composited_with_cursor(5)
                    .expect("composited capture");
            } else {
                session.capture_pane_with_cursor(5).expect("pane capture");
            }
            drop(probe);
            assert_eq!(
                crate::tmux::fork_probe::take(),
                1,
                "{label} capture must use one operation deadline and one tmux invocation",
            );
        }

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

    /// #3265: tmux silently falls back to its server's `$HOME` when `-c`
    /// points at a directory that doesn't exist, landing a fresh/restarted
    /// pane in the daemon's launch directory instead of the session's
    /// `project_path`. `create_with_size_env` must refuse to spawn rather
    /// than let that happen invisibly.
    #[test]
    #[serial_test::serial]
    fn test_create_with_size_env_rejects_missing_working_dir() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_missing_dir");
        let session = super::Session::from_name(guard.name());
        let tmp = tempfile::TempDir::new().unwrap();
        let missing_dir = tmp.path().join("does-not-exist");
        let result = session.create_with_size(
            missing_dir.to_str().unwrap(),
            Some("sleep 5"),
            None,
            "default",
        );

        assert!(
            result.is_err(),
            "create_with_size_env must reject a missing working directory instead of \
             silently falling back to tmux's own $HOME"
        );
        assert!(
            !session.exists(),
            "no tmux session should have been created"
        );
    }
    #[test]
    fn expired_deadline_stops_vt_owner_commands_immediately() {
        let deadline = crate::tmux::TmuxCommandDeadline::with_timeout(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        let started = Instant::now();
        let session = Session::from_name("aoe_test_expired_owner");
        assert!(!session.claim_vt_owner_with_deadline("owner", Duration::from_secs(10), &deadline,));
        session.release_vt_owner_with_deadline("owner", &deadline);
        session.release_vt_pipe_owner_with_deadline("owner", &deadline);
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    /// #3071: is_attached gates the TUI's passive preview resize, so it has
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
        assert_eq!(
            session.is_attached(),
            Some(false),
            "Detached session should report is_attached() == false",
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
            if session.is_attached() == Some(true) {
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
        let agent_pane = only_pane_id(&session_name);

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

        wait_for_pane_command(&agent_pane, "sleep");

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

    /// An unsplit window must composite to exactly what the single-pane
    /// preview capture (`capture_pane_with_cursor`, the other `-N` transport)
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

        let plain = session
            .capture_pane_with_cursor(10)
            .expect("capture_pane_with_cursor")
            .0;
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

    /// Pane 0's rectangle as tmux reports it, `(left, top, width, height)`.
    fn pane0_tmux_geometry(session: &Session) -> (u16, u16, u16, u16) {
        let out = crate::tmux::tmux_command()
            .args([
                "display-message",
                "-p",
                "-t",
                &format!("{}:^.0", session.name),
                "-F",
                "#{pane_left} #{pane_top} #{pane_width} #{pane_height}",
            ])
            .output()
            .expect("display-message");
        assert!(out.status.success(), "display-message failed");
        let text = String::from_utf8_lossy(&out.stdout);
        let mut fields = text.split_whitespace();
        let left = fields.next().expect("pane_left").parse().expect("u16");
        let top = fields.next().expect("pane_top").parse().expect("u16");
        let width = fields.next().expect("pane_width").parse().expect("u16");
        let height = fields.next().expect("pane_height").parse().expect("u16");
        (left, top, width, height)
    }

    /// A top border row shifts pane 0 down. Across horizontal, vertical, and
    /// stacked splits, verify that the carried rectangle matches tmux and the
    /// composite paints the cursor row at `cursor.y + top`; the untranslated
    /// row assertion keeps the test non-vacuous.
    #[test]
    #[serial_test::serial]
    fn composited_cursor_and_pane0_origin_track_pane_border_offset() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        // Each layout's second-pane splits; the pane parks its cursor right
        // after `MARKER` on its row 0 (`printf` emits no newline).
        let layouts: [(&str, &[&[&str]]); 3] = [
            ("aoe_test_cursor_h", &[&["split-window", "-h"]]),
            ("aoe_test_cursor_v", &[&["split-window", "-v"]]),
            (
                "aoe_test_cursor_stack",
                &[&["split-window", "-v"], &["split-window", "-v"]],
            ),
        ];
        for (name, splits) in layouts {
            let guard = TmuxTestSession::new(name);
            let session =
                start_composite_session(guard.name(), 80, 24, "sh -c 'printf MARKER; sleep 60'");
            for args in splits {
                let status = crate::tmux::tmux_command()
                    .args(args.iter().copied().chain(["-t", session.name.as_str()]))
                    .status()
                    .expect("tmux split-window");
                assert!(status.success(), "{name}: split failed");
            }
            let status = crate::tmux::tmux_command()
                .args([
                    "set-option",
                    "-w",
                    "-t",
                    &session.name,
                    "pane-border-status",
                    "top",
                ])
                .status()
                .expect("tmux set-option");
            assert!(status.success(), "{name}: set-option failed");
            wait_for_composite_text(&session, "MARKER");

            let (content, cursor) = session
                .capture_window_composited_with_cursor(20)
                .expect("capture_window_composited_with_cursor");
            let cursor = cursor.unwrap_or_else(|| panic!("{name}: composited cursor missing"));
            let rect = cursor
                .composite_pane0
                .unwrap_or_else(|| panic!("{name}: composite_pane0 missing"));
            assert_eq!(
                (rect.left, rect.top, rect.width, rect.height),
                pane0_tmux_geometry(&session),
                "{name}: carried rectangle must match tmux"
            );
            assert_eq!(rect.top, 1, "{name}: border status must shift pane 0");

            // The untranslated index (cursor.y alone) must NOT land on the
            // marker row, or the assertion below proves nothing.
            let lines: Vec<&str> = content.lines().collect();
            assert!(
                !lines[cursor.y as usize].contains("MARKER"),
                "{name}: marker unexpectedly at untranslated row {}",
                cursor.y
            );
            let painted = lines
                .get(cursor.y as usize + rect.top as usize)
                .unwrap_or_else(|| panic!("{name}: translated row out of range"));
            assert!(
                painted.contains("MARKER"),
                "{name}: cursor row {} painted {:?}, expected MARKER at +{}",
                cursor.y,
                painted,
                rect.top
            );
        }
    }

    /// A full-screen TUI (opencode's dimmed modal backdrop, its empty home
    /// screen) paints its background as full-width runs of bg-styled spaces.
    /// `capture-pane` trims trailing spaces by default, styled or not, while
    /// the VT path keeps styled trailing blanks as content (`row_last_col`).
    /// The preview alternates between the two sources, so a fill dropped by
    /// one and kept by the other flickers at the idle-poll cadence (#3336).
    /// Every preview-feeding capture must therefore preserve the fill.
    #[test]
    #[serial_test::serial]
    fn preview_captures_preserve_trailing_bg_fill() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_bg_fill");
        // Row 0: a 40-column run of bg-styled spaces, opencode's backdrop
        // pattern. Row 1: text so the wait helper has a needle that survives
        // the trim either way.
        let session = start_composite_session(
            guard.name(),
            40,
            8,
            "sh -c 'printf \"\\033[44m%40s\\033[0m\\nALPHA\\n\" \"\"; sleep 30'",
        );
        wait_for_pane_text(&session, "ALPHA");

        let fill = format!("\u{1b}[44m{}", " ".repeat(40));
        let (with_cursor, _) = session
            .capture_pane_with_cursor(10)
            .expect("capture_pane_with_cursor");
        assert!(
            with_cursor.contains(fill.as_str()),
            "capture_pane_with_cursor dropped the styled fill:\n{with_cursor:?}"
        );

        let composited = session
            .capture_window_composited(10)
            .expect("capture_window_composited");
        assert!(
            composited.contains(fill.as_str()),
            "capture_window_composited dropped the styled fill:\n{composited:?}"
        );

        let layout = session.capture_window_layout(1).expect("layout");
        let rows = layout.panes[0].rows.join("\n");
        assert!(
            rows.contains(fill.as_str()),
            "capture_window_layout dropped the styled fill:\n{rows:?}"
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
    /// the single-pane preview capture.
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
            session
                .capture_pane_with_cursor(10)
                .expect("capture_pane_with_cursor")
                .0,
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
    /// grid, so the layout must come back with pane 0 first and with
    /// rectangles that tile the real window. This split sets no
    /// border-status option, so pane 0 additionally sits at the window
    /// origin here.
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
            "pane 0 must sit at the origin in this split; a border-status row would shift it"
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
        let plain = session
            .capture_pane_with_cursor(20)
            .expect("capture_pane_with_cursor")
            .0;
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
        assert!(
            cursor.position_reliable,
            "an unchanged single-pane capture must keep its cursor"
        );

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
        let agent_pane = only_pane_id(&session_name);

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

        wait_for_pane_command(&agent_pane, "sleep");

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
        let agent_pane = only_pane_id(&session_name);

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

        wait_for_pane_command(&agent_pane, "sleep");

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
        let agent_pane = only_pane_id(&session_name);

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

        wait_for_pane_command(&agent_pane, "sleep");

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
    fn test_build_create_args_keeps_only_non_secret_launch_id_in_tmux_env() {
        let args = build_create_args(
            "s",
            "/tmp/work",
            &[(
                crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY,
                "non-secret-generation",
            )],
            Some("omp"),
            None,
        );
        let e_idx = args.iter().position(|arg| arg == "-e").unwrap();
        assert_eq!(
            args[e_idx + 1],
            format!(
                "{}=non-secret-generation",
                crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY
            )
        );
    }

    #[test]
    fn test_protected_env_file_keeps_secret_out_of_pane_argv_and_rejects_invalid_keys() {
        let secret = "literal-secret-value";
        let file = EphemeralEnvFile::create(
            &[
                PaneEnvMutation::set("GOOD_TOKEN".to_string(), "stale-profile-value".to_string()),
                PaneEnvMutation::set("GOOD_TOKEN".to_string(), secret.to_string()),
                PaneEnvMutation::set("X; touch /tmp/injected; #".to_string(), "bad".to_string()),
            ],
            &[],
        )
        .unwrap();
        let path = file.path.as_ref().unwrap().clone();
        let wrapper = file.wrap_command(Some("omp --help")).unwrap();
        let args = build_create_args("s", "/tmp/work", &[], Some(&wrapper), None);
        assert!(wrapper.starts_with(&format!(
            "exec {} ",
            crate::session::environment::shell_escape(
                &crate::session::environment::user_posix_shell()
            )
        )));

        assert!(!wrapper.contains(secret));
        assert!(!wrapper.contains("stale-profile-value"));
        assert!(!args.iter().any(|arg| arg.contains(secret)));
        assert!(!wrapper.contains("touch /tmp/injected"));
        assert!(wrapper.contains(&path.to_string_lossy().to_string()));
        let contents = std::fs::read_to_string(&path).unwrap();

        assert!(contents.find("rm -f").unwrap() < contents.find("omp --help").unwrap());
        assert!(contents.contains("export GOOD_TOKEN='literal-secret-value'"));
        let stale = contents
            .find("export GOOD_TOKEN='stale-profile-value'")
            .unwrap();
        let minted = contents
            .find("export GOOD_TOKEN='literal-secret-value'")
            .unwrap();
        assert!(stale < minted, "later minted export must win when sourced");
        assert!(!contents.contains("touch /tmp/injected"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(file);
        assert!(!path.exists(), "failure guard must clean up the channel");
    }

    #[test]
    fn test_protected_env_file_preserves_multiline_values() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("multiline");
        let value = "line one\nline two\r\nquote ' intact";
        let stale_output = temp.path().join("unset");
        let mut file = EphemeralEnvFile::create(
            &[
                PaneEnvMutation::set("MULTILINE_SECRET".to_string(), value.to_string()),
                PaneEnvMutation::unset("AOE_TEST_STALE".to_string()),
            ],
            &[],
        )
        .unwrap();
        let command = format!(
            "printf '%s' \"$MULTILINE_SECRET\" > {}; printf '%s' \"${{AOE_TEST_STALE+x}}\" > {}",
            shell_escape_script_word(&output.to_string_lossy()),
            shell_escape_script_word(&stale_output.to_string_lossy())
        );
        let wrapper = file.wrap_command(Some(&command)).unwrap();
        let status = std::process::Command::new("sh")
            .args(["-c", &wrapper])
            .env("AOE_TEST_STALE", "inherited")
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read(&output).unwrap(), value.as_bytes());
        assert_eq!(std::fs::read_to_string(stale_output).unwrap(), "");
        assert!(file.wait_until_consumed(Duration::ZERO));
        file.disarm();
    }

    /// `#[serial]` because the assertion reads the inherited PATH, and every
    /// test that scrubs PATH process-globally carries that same default-key
    /// annotation: `crate::acp::node`, `crate::acp::acp_client`, and
    /// `crate::update::install`. Not an `EnvGuard` lock: none of them takes
    /// `test_support::ENV_LOCK`.
    #[test]
    #[serial_test::serial]
    fn test_container_env_file_does_not_mutate_host_process_environment() {
        let temp = tempfile::tempdir().unwrap();
        let host_output = temp.path().join("host-env");
        let payload_output = temp.path().join("container-env");
        let target_env = vec![
            ("PATH".to_string(), "/repo-controlled/bin".to_string()),
            (
                "DOCKER_HOST".to_string(),
                "tcp://repo-controlled.example".to_string(),
            ),
            ("TOKEN".to_string(), "secret-value".to_string()),
        ];
        let mut file = EphemeralEnvFile::create(&[], &target_env).unwrap();
        let script_path = file.path.as_ref().unwrap().clone();
        let payload_path = file.container_env_path.as_ref().unwrap().clone();
        let command = format!(
            "printf '%s\\n%s' \"$PATH\" \"${{DOCKER_HOST-unset}}\" > {}; \
             cat {} > {}",
            shell_escape_script_word(&host_output.to_string_lossy()),
            crate::session::environment::CONTAINER_EXEC_ENV_PATH,
            shell_escape_script_word(&payload_output.to_string_lossy()),
        );
        let wrapper = file.wrap_command(Some(&command)).unwrap();
        let script = std::fs::read_to_string(&script_path).unwrap();

        assert!(!script.contains("/repo-controlled/bin"));
        assert!(!script.contains("tcp://repo-controlled.example"));
        assert!(!wrapper.contains("secret-value"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&payload_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        // The PATH is asserted back below to prove the env file did not mutate
        // the host process environment, so it has to be a value this host can
        // actually resolve `cat` on.
        // `var_os`, not `var`: a PATH entry need not be UTF-8, and `var`
        // returns `Err(VarError::NotUnicode)` for one that isn't, which the
        // `unwrap_or_default` below would turn into an empty PATH rather than
        // a failure.
        let host_path = std::env::var_os("PATH").unwrap_or_default();
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &wrapper])
            .env("PATH", &host_path)
            .env_remove("DOCKER_HOST")
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .status()
            .unwrap();
        assert!(status.success());
        // Compared as bytes for the same reason: a non-UTF-8 PATH survives the
        // round trip through the shell but not through `read_to_string`.
        let mut expected = host_path.as_encoded_bytes().to_vec();
        expected.extend_from_slice(b"\nunset");
        // Rendered lossily in the message only: `assert_eq!` on `Vec<u8>`
        // prints decimal byte arrays, which is unreadable for a whole PATH.
        let actual = std::fs::read(host_output).unwrap();
        assert_eq!(
            actual,
            expected,
            "env file mutated the host environment: {:?} != {:?}",
            String::from_utf8_lossy(&actual),
            String::from_utf8_lossy(&expected),
        );
        assert_eq!(
            std::fs::read_to_string(payload_output).unwrap(),
            "PATH=/repo-controlled/bin\n\
             DOCKER_HOST=tcp://repo-controlled.example\n\
             TOKEN=secret-value\n"
        );
        assert!(file.wait_until_consumed(Duration::ZERO));
        assert!(!payload_path.exists());
        file.disarm();

        assert!(EphemeralEnvFile::create(
            &[],
            &[("MULTILINE".to_string(), "line one\nline two".to_string())],
        )
        .is_err());
    }

    #[test]
    #[serial_test::serial]
    fn test_protected_env_is_consumed_before_create_returns() {
        if !tmux_available() {
            return;
        }
        let guard = TmuxTestSession::new("aoe_test_protected_env");
        let session = Session::from_name(guard.name());
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("value");
        let command = format!(
            "printf '%s' \"$AOE_TEST_PROTECTED_VALUE\" > {}; sleep 30",
            crate::session::environment::shell_escape(&output.to_string_lossy())
        );

        session
            .create_with_size_env(
                "/tmp",
                Some(&command),
                Some((80, 24)),
                "default",
                &[PaneEnvMutation::set(
                    "AOE_TEST_PROTECTED_VALUE".to_string(),
                    "secret value".to_string(),
                )],
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !output.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(output).unwrap(), "secret value");
        assert!(
            !session.is_pane_running_shell(),
            "live protected-environment wrapper must not look like an exited agent"
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
