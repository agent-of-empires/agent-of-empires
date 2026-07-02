//! tmux session management

use anyhow::{bail, Result};
use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::{
    refresh_session_cache, session_exists_from_cache,
    utils::{
        append_clipboard_passthrough_args, append_mouse_on_args, append_pane_base_index_args,
        append_remain_on_exit_args, append_window_size_args, is_pane_dead, is_pane_running_shell,
    },
    SESSION_PREFIX,
};
use crate::cli::truncate_id;
use crate::process;
use crate::session::config::should_apply_tmux_clipboard;
use crate::session::Status;

pub struct Session {
    name: String,
}

/// tmux user options holding the cross-process size-owner lock (see
/// [`Session::claim_size_owner`]). User options ride on the session itself, so
/// the web daemon and the native TUI read and write the same state.
const SIZE_OWNER_OPT: &str = "@aoe_size_owner";
const SIZE_OWNER_HB_OPT: &str = "@aoe_size_owner_hb";

/// How long a size-owner lock survives without a heartbeat before another
/// client may steal it. Shared by every surface that drives window size (the
/// web PTY relay, the mobile live view, the native TUI) so they age the lock
/// the same and a connected owner is never stolen from mid-use.
pub const SIZE_OWNER_TTL: Duration = Duration::from_secs(4);
/// How often a connected size owner refreshes its heartbeat. Well under
/// [`SIZE_OWNER_TTL`] so a live-but-idle owner keeps the lock while connected;
/// the lock only frees on disconnect/crash (TTL expiry) or explicit take-over.
pub const SIZE_OWNER_HEARTBEAT: Duration = Duration::from_millis(1500);

/// Wall-clock millis since the unix epoch. The size-owner heartbeat is compared
/// across processes, so it must be wall-clock, not a per-process monotonic.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
}

impl PaneCursor {
    /// Parse the single space-separated line emitted by the
    /// `#{cursor_x} #{cursor_y} #{cursor_flag} #{pane_height}
    /// #{history_size} #{pane_width} #{alternate_on} #{mouse_any_flag}
    /// #{mouse_sgr_flag}` format. The trailing fields are optional so an
    /// older four-field line still parses (numeric fields as 0, flag
    /// fields as `false`).
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
            // A single probe's own position is self-consistent; the
            // cross-probe check in `capture_pane_with_cursor` is the only
            // thing that downgrades this.
            position_reliable: true,
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

impl Session {
    pub fn new(id: &str, title: &str) -> Result<Self> {
        Ok(Self {
            name: Self::generate_name(id, title),
        })
    }

    /// Construct a Session from a pre-computed tmux session name.
    pub fn from_name(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }

    pub fn generate_name(id: &str, title: &str) -> String {
        let safe_title = sanitize_session_name(title);
        format!("{}{}_{}", SESSION_PREFIX, safe_title, truncate_id(id, 8))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn exists(&self) -> bool {
        if let Some(exists) = session_exists_from_cache(&self.name) {
            return exists;
        }

        crate::tmux::tmux_command()
            .args(["has-session", "-t", &self.name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn create(&self, working_dir: &str, command: Option<&str>) -> Result<()> {
        self.create_with_size(working_dir, command, None)
    }

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        if self.exists() {
            return Ok(());
        }

        let mut args = build_create_args(&self.name, working_dir, &[], command, size);
        append_remain_on_exit_args(&mut args, &self.name);
        append_pane_base_index_args(&mut args, &self.name);
        append_mouse_on_args(&mut args, &self.name);
        append_window_size_args(&mut args, &self.name);
        if should_apply_tmux_clipboard() {
            append_clipboard_passthrough_args(&mut args, &self.name);
        }

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
        const HEADER_FMT: &str =
            "#{cursor_x} #{cursor_y} #{cursor_flag} #{pane_height} #{history_size} #{pane_width} #{alternate_on} #{mouse_any_flag} #{mouse_sgr_flag}";
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

    /// Resize the (detached) window to `cols`x`rows`. Best-effort: a missing
    /// session or a tmux ENOENT is swallowed so a transient failure never
    /// blocks a render.
    ///
    /// Used to keep a detached agent's pane sized to the visible preview area:
    /// a full-screen agent is sized to whatever terminal it was last attached
    /// from, so without this it renders taller than the preview window and the
    /// bottom-anchored capture clips the top rows (worse when the info header
    /// steals rows). Mirrors what live-send does through its worker.
    ///
    /// NOTE: tmux's `resize-window -x -y` silently flips the window-size option
    /// to `manual`, so any later `attach-session` must call
    /// [`reset_size_to_latest_client`](Self::reset_size_to_latest_client) first
    /// or the window stays pinned at these preview dimensions.
    pub fn resize_window(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || !self.exists() {
            return;
        }
        let _ = crate::tmux::tmux_command()
            .args([
                "resize-window",
                "-t",
                &self.name,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
            ])
            .output();
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
        if !self.exists() {
            return false;
        }
        let now = now_ms();
        let claimable = match self.size_owner() {
            None => true,
            Some((id, _)) if id == owner_id => true,
            Some((_, hb)) => now.saturating_sub(hb) > ttl.as_millis() as u64,
        };
        if !claimable {
            return false;
        }
        self.set_user_option(SIZE_OWNER_OPT, owner_id);
        self.set_user_option(SIZE_OWNER_HB_OPT, &now.to_string());
        matches!(self.size_owner(), Some((id, _)) if id == owner_id)
    }

    /// Bump the heartbeat iff we still own the lock. Returns false when
    /// ownership was lost (another client took over), so the caller can demote
    /// itself. Cheap enough to call on each capture/render tick.
    pub fn refresh_size_owner(&self, owner_id: &str) -> bool {
        match self.size_owner() {
            Some((id, _)) if id == owner_id => {
                self.set_user_option(SIZE_OWNER_HB_OPT, &now_ms().to_string());
                true
            }
            _ => false,
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
        let id = self.show_user_option(SIZE_OWNER_OPT)?;
        let hb = self
            .show_user_option(SIZE_OWNER_HB_OPT)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Some((id, hb))
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
        let c = PaneCursor::parse("3 2 1 24 120 74 1 1 1").expect("parses");
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
                position_reliable: true,
            }
        );
        // Legacy mouse (tracking on, SGR off) parses with mouse_sgr false.
        let c = PaneCursor::parse("3 2 1 24 120 74 1 1 0").expect("parses");
        assert!(c.mouse_tracking);
        assert!(!c.mouse_sgr);
        // The six-field (pre-alternate/mouse) line still parses, the new
        // flags defaulting to false.
        let c = PaneCursor::parse("3 2 1 24 120 74").expect("parses");
        assert!(!c.alternate_on);
        assert!(!c.mouse_tracking);
        assert!(!c.mouse_sgr);
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
