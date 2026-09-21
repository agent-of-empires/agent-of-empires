//! tmux session management

use anyhow::{bail, Result};
use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{
    composite::{CapturedPane, PaneGeom, WindowLayout},
    probe_session_existence,
    utils::{append_session_setup_args, is_pane_dead, is_pane_running_shell, PANE_ENV_FILE_PREFIX},
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

/// Cross-process size-owner lock, stored as tmux user options on the session.
const SIZE_OWNER_OPT: &str = "@aoe_size_owner";
const SIZE_OWNER_HB_OPT: &str = "@aoe_size_owner_hb";

/// Cross-process VT-pipe owner lock: `pipe-pane` is exclusive per pane, so only
/// the holder pipes and everyone else stays on `capture-pane`.
const VT_OWNER_OPT: &str = "@aoe_vt_owner";
const VT_OWNER_HB_OPT: &str = "@aoe_vt_owner_hb";
const VT_PIPE_OWNER_OPT: &str = "@aoe_vt_pipe_owner";

/// A crashed VT-pipe holder frees the lock within this window.
pub const VT_OWNER_TTL: Duration = Duration::from_secs(4);

/// Shared by every surface that drives window size so they age the lock alike.
pub const SIZE_OWNER_TTL: Duration = Duration::from_secs(4);
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

/// The pane cursor and terminal modes, probed alongside a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneCursor {
    pub x: u16,
    pub y: u16,
    /// `#{cursor_flag}`: false when the application hid the cursor.
    pub visible: bool,
    pub pane_height: u16,
    pub history_size: u32,
    pub pane_width: u16,
    /// `#{alternate_on}`: no scrollback, so the wheel goes to the app.
    pub alternate_on: bool,
    pub mouse_tracking: bool,
    /// `#{mouse_sgr_flag}`: the wheel is forwarded only with SGR encoding, since
    /// X10 would be corrupted by SGR bytes.
    pub mouse_sgr: bool,
    /// `#{mouse_all_flag}`: the app wants bare motion reports (DEC 1003).
    pub mouse_all: bool,
    /// False when the pane scrolled between the two probes around a capture, so
    /// the row no longer indexes the content; the mode flags stay valid.
    pub position_reliable: bool,
    /// Pane 0's rectangle within a composited window, `None` for a single pane.
    /// Cursor and input stay pane-relative.
    pub composite_pane0: Option<PaneGeom>,
}

const CURSOR_FMT: &str = "#{cursor_x} #{cursor_y} #{cursor_flag} #{pane_height} #{history_size} #{pane_width} #{alternate_on} #{mouse_any_flag} #{mouse_sgr_flag} #{mouse_all_flag}";

impl PaneCursor {
    /// Trailing fields are optional (numbers parse as 0, flags as false).
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
            position_reliable: true,
            composite_pane0: None,
        })
    }
}

/// Keep the post-capture cursor, marking its position unreliable if
/// `history_size` or `pane_height` moved (x/visibility jitter is harmless).
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

/// Split the chained multi-pane capture at each sentinel line; a pane whose
/// geometry does not parse is dropped rather than misplaced.
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

fn unreliable_position(cursor: Option<PaneCursor>) -> Option<PaneCursor> {
    cursor.map(|c| PaneCursor {
        position_reliable: false,
        ..c
    })
}

const MAX_CHROME_ROWS: u16 = 5;

/// Status-bar rows outside the pane, measured live since it varies by tmux
/// version and config; a larger delta is a split and reads as 0.
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

    pub fn from_name(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }

    /// The session to act on: the live session carrying this id's tail when the
    /// title has moved (smart rename), else the derived name. Use
    /// [`Self::generate_name`] only for the name to rename TO.
    pub fn resolve_name(id: &str, title: &str) -> String {
        crate::tmux::live_agent_session_name(id, &Self::generate_name(id, title))
    }

    /// Snapshot-only [`Self::resolve_name`] for render paths.
    pub(crate) fn resolve_name_for_display(id: &str, title: &str) -> String {
        crate::tmux::agent_session_name_for_display(id, &Self::generate_name(id, title))
    }

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

    pub fn existence(&self) -> SessionExistence {
        probe_session_existence(&self.name)
    }

    pub fn create(&self, working_dir: &str, command: Option<&str>, profile: &str) -> Result<()> {
        self.create_with_size(working_dir, command, None, profile)
    }

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
        profile: &str,
    ) -> Result<()> {
        self.create_with_size_env(working_dir, command, size, profile, &[])
    }

    /// Like [`Self::create_with_size`], applying `extra_env` through a one-shot
    /// protected file so values and the command never enter tmux argv or session
    /// env. The non-secret OMP launch ID stays a tmux `-e` value.
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

    /// Container target env values are read from an inherited env-file descriptor.
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

        // tmux silently falls back to $HOME for a missing `-c` directory.
        let working_dir_path = std::path::Path::new(working_dir);
        if !working_dir_path.is_dir() {
            bail!(
                "Cannot create tmux session '{}': working directory '{}' does not exist \
                 or is not a directory (tmux would otherwise silently fall back to $HOME)",
                self.name,
                working_dir
            );
        }

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

        // Forward the host desktop env so agents and browsers they open reach it.
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
        append_session_setup_args(
            &mut args,
            &self.name,
            &config,
            None,
            crate::tmux::SessionKind::Agent,
        );

        let output = crate::tmux::tmux_command().args(&args).output()?;

        // Never log argv: the pane command can contain legacy credentials.
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

        // The pane unlinking the file acknowledges it; tmux `-d` can return first.
        if !env_file.wait_until_consumed(Duration::from_secs(5)) {
            crate::tmux::refresh_session_cache();
            let _ = self.kill();
            bail!("Pane did not consume its protected launch script");
        }
        env_file.disarm();
        crate::tmux::refresh_session_cache();

        Ok(())
    }

    pub fn is_pane_dead(&self) -> bool {
        is_pane_dead(&self.name)
    }

    pub fn is_pane_running_shell(&self) -> bool {
        is_pane_running_shell(&self.name)
    }

    /// Revive a dead pane with `respawn-pane -k`, keeping the session. Returns
    /// whether a dead pane was respawned.
    pub fn respawn_dead_pane(&self, working_dir: &str, command: Option<&str>) -> Result<bool> {
        if !self.exists() {
            return Ok(false);
        }
        if !self.is_pane_dead() {
            return Ok(false);
        }

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

        crate::tmux::refresh_session_cache();
        Ok(true)
    }

    pub fn kill(&self) -> Result<()> {
        super::utils::kill_session_tree(&self.name)
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
        if let Some(status) = super::utils::attach_client(&self.name)? {
            bail!(
                "Failed to attach to tmux session '{}' (exit {}): {}",
                self.name,
                status.code().unwrap_or(-1),
                self.diagnose_attach_failure()
            );
        }
        Ok(())
    }

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

    /// Creation time in epoch ms, rounded to the end of tmux's one-second
    /// precision so a same-second breadcrumb is not mistaken for a later write.
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

    /// Wait for a known marker, or for two stable captures when no marker exists.
    /// The polling budget bounds readiness checks, not each native subprocess.
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
            #[cfg(test)]
            tests::observe_ready_capture(&now);
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

    /// Capture the first window with panes composited, plus pane 0's cursor.
    /// Single-pane and zoomed windows cost one fork and keep scrollback; a split
    /// window takes a second chained fork and shows only the visible window.
    /// Input stays pinned to `^.0`.
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
        /// Sentinels guard each probe line: a chained `display-message` can print
        /// nothing while tmux still exits 0.
        const WINDOW_SENTINEL: &str = "@@aoe-win@@";
        const CURSOR_SENTINEL: &str = "@@aoe-cur@@";
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
            // Keep trailing bg fills, matching the VT path.
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

        // The first line carrying neither sentinel starts the capture.
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
                zoomed = f.next().is_some_and(|z| z != "0");
            } else if let Some(fields) = line.strip_prefix(CURSOR_SENTINEL) {
                cursor_before = PaneCursor::parse(fields.trim());
            } else {
                break;
            }
            rest = tail;
        }
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
        // Zoomed panes overlap, which the compositor cannot tile.
        if zoomed {
            return Ok((pane0_content, cursor));
        }

        let Some(layout) = self.capture_window_layout_with_deadline(count, deadline) else {
            return Ok((pane0_content, cursor));
        };
        // Reuse the pane-0 bytes bracketed by the cursor probes, not the layout's
        // later copy, so the cursor matches the content.
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
            c.composite_pane0 = layout.first_pane();
            c
        });
        let content = pane0_rows.as_deref().map_or_else(
            || layout.composite(),
            |rows| layout.composite_with_first_pane_rows(rows),
        );
        Ok((content, cursor))
    }

    /// Window dimensions plus each pane's geometry and visible capture, in one
    /// chained invocation. `pane-base-index` is pinned to 0, so `^.0..^.{count-1}`
    /// addresses every pane.
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
        const SENTINEL: &str = "@@aoe-pane@@";
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
                // Keep trailing bg fills, matching the VT path.
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
        // Backstop for zoomed layouts: keep the first of each overlapping set.
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

    #[cfg(test)]
    fn capture_window_composited(&self, lines: usize) -> Result<String> {
        Ok(self.capture_window_composited_with_cursor(lines)?.0)
    }

    /// Full scrollback with wrapped lines joined and no escapes, for smart rename.
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

    /// Capture plus cursor in one fork. The chain is not atomic, so the cursor is
    /// probed before and after and marked unreliable if the pane scrolled.
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
            // Keep trailing bg fills, matching the VT path.
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
        let mut parts = raw.splitn(2, '\n');
        let cursor_line = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        let (content, after_line) = match rest.rfind('\n') {
            Some(_) => {
                let trimmed = rest.strip_suffix('\n').unwrap_or(rest);
                match trimmed.rfind('\n') {
                    Some(idx) => (&trimmed[..=idx], &trimmed[idx + 1..]),
                    None => ("", trimmed),
                }
            }
            None => ("", rest),
        };
        let before = PaneCursor::parse(cursor_line);
        let after = PaneCursor::parse(after_line);
        Ok((content.to_string(), merge_cursor_probes(before, after)))
    }

    /// Deliver raw bytes via `send-keys -H`, chunked to stay under ARG_MAX.
    pub fn send_raw_bytes(&self, bytes: &[u8]) -> Result<()> {
        // A bare session target follows the active pane; pin `^.0` like capture.
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

    /// Paste through tmux's paste path so bracketed-paste markers are emitted only
    /// when the program enabled DECSET 2004.
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

    /// Send text then Enter; longer or multi-line text goes via bracketed paste.
    pub fn send_keys(&self, text: &str) -> Result<()> {
        self.send_keys_with_delay(text, 0)
    }

    /// Waits `enter_delay_ms` before Enter, for agents whose paste-burst detection
    /// swallows an early Enter.
    pub fn send_keys_with_delay(&self, text: &str, enter_delay_ms: u64) -> Result<()> {
        if !self.exists() {
            bail!("Session does not exist: {}", self.name);
        }

        let target = format!("{}:^.0", self.name);
        let byte_len = text.len();
        let line_count = text.lines().count();
        let max_line = text.lines().map(str::len).max().unwrap_or(0);

        // Anything beyond a few characters goes via bracketed paste: an Enter right
        // after literal keystrokes can land inside the agent's burst window and insert
        // a newline instead of submitting.
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
            // `--` so lines starting with `-` are not read as tmux flags.
            Self::tmux_send(&target, &["-l", "--", &payload])?;
        }

        if enter_delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(enter_delay_ms));
        }

        Self::tmux_send(&target, &["Enter"])?;

        Ok(())
    }

    /// Send exactly these tokens, with no implicit Enter, to answer an agent's own
    /// permission prompt.
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

    /// `resize-window` switches `window-size` to manual; restore `latest` so a
    /// later attach sizes the window to itself. Best-effort.
    pub fn reset_size_to_latest_client(&self) {
        if !self.exists() {
            return;
        }
        let mut command = crate::tmux::tmux_command();
        command.args(["set-option", "-t", &self.name, "window-size", "latest"]);
        let _ = crate::tmux::run_tmux_command_with_timeout(&mut command);
    }

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
    /// Try to become the sole size owner. Three surfaces in different processes
    /// resize one window, so the lock lives in tmux user options; a stale heartbeat
    /// (older than `ttl`) may be stolen.
    pub fn claim_size_owner(&self, owner_id: &str, ttl: Duration) -> bool {
        self.claim_owner_at(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, owner_id, ttl)
    }

    /// Bump the heartbeat iff we still own the lock.
    pub fn refresh_size_owner(&self, owner_id: &str) -> bool {
        self.refresh_owner_at(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT, owner_id)
    }

    /// Claims compare-and-set the observed owner pair in one tmux queue, so a
    /// stale claimant cannot overwrite a renewal or a winner.
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

    /// Returns the applied window rows (including chrome), `None` when declined.
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
        // `if-shell -F` checks the guard and resizes in one command queue. Target the
        // first window (`:^`), which the chrome probe and capture also use.
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
    /// Arm pipe-pane only if this channel generation still owns the lease when
    /// tmux runs it.
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

    /// Disable the pipe and release its lease iff this generation still owns it;
    /// a replacement channel may share the session name.
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

    /// Force ownership, for the explicit "take over" action.
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

    /// Resize iff `owner_id` holds the lock when tmux executes the resize.
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
    /// Resize a detached pane only if the inactive owner state observed here is
    /// unchanged when tmux executes the resize.
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

    /// Whether a client is attached (`#{session_attached}`), so passive resize
    /// leaves an attached session alone. `None` unless tmux answers authoritatively.
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

    /// Whether a non-stale size owner holds the lock.
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
    pub fn size_owner(&self) -> Option<(String, u64)> {
        self.owner_at(SIZE_OWNER_OPT, SIZE_OWNER_HB_OPT)
    }

    /// Release the lock iff we own it, restoring `window-size latest`.
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

    /// load-buffer + paste-buffer with a per-process, per-call buffer name. `-p`
    /// adds bracketed-paste markers when the pane enabled them; `-d` deletes the
    /// buffer on success.
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
            // `-d` only deletes on success.
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

/// Each byte is one argv entry; macOS caps argv+envp at 256KB.
const MAX_RAW_BYTES_PER_SEND: usize = 4096;

fn raw_byte_batches(bytes: &[u8]) -> Vec<Vec<String>> {
    bytes
        .chunks(MAX_RAW_BYTES_PER_SEND)
        .map(|chunk| chunk.iter().map(|b| format!("{:02x}", b)).collect())
        .collect()
}

/// A one-shot, mode-0600 environment script for a pane command. The guard
/// owns cleanup until the pane unlinks the file.
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

        // One short script invocation; exports and the command body stay in the file.
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

/// A leading `/` opens some agents' autocomplete, which would eat the Enter;
/// a trailing space closes it.
fn pad_slash_command_for_autocomplete(text: &str) -> std::borrow::Cow<'_, str> {
    if text.trim_start().starts_with('/') {
        std::borrow::Cow::Owned(format!("{text} "))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// tmux `new-session` argv shared by agent and terminal sessions.
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

    // `-e` needs tmux 3.2+, already assumed elsewhere.
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
    use super::super::test_helpers::{
        only_pane_id, pane_field, wait_for_pane_command, wait_for_pane_dead, TmuxTestSession,
    };
    use super::*;
    use crate::tmux::refresh_session_cache;
    use crate::tmux::test_helpers::require_tmux;
    use crate::tmux::utils::{append_pane_base_index_args, append_remain_on_exit_args};
    struct ReadyCaptureProbe {
        captured: std::sync::mpsc::Sender<String>,
        resume: std::sync::mpsc::Receiver<()>,
    }

    thread_local! {
        static READY_CAPTURE: std::cell::RefCell<Option<ReadyCaptureProbe>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn observe_ready_capture(content: &str) {
        READY_CAPTURE.with(|slot| {
            if let Some(probe) = slot.borrow().as_ref() {
                if probe.captured.send(content.to_owned()).is_ok() {
                    let _ = probe.resume.recv();
                }
            }
        });
    }

    struct ReadyCaptureGuard(Option<ReadyCaptureProbe>);

    impl ReadyCaptureGuard {
        fn install(probe: ReadyCaptureProbe) -> Self {
            Self(READY_CAPTURE.with(|slot| slot.replace(Some(probe))))
        }
    }

    impl Drop for ReadyCaptureGuard {
        fn drop(&mut self) {
            READY_CAPTURE.with(|slot| slot.replace(self.0.take()));
        }
    }
    fn rebase_first_window_to_index_one(session_name: &str) {
        let window = pane_field(session_name, "#{window_id}");
        let index = pane_field(session_name, "#{window_index}");
        let set = crate::tmux::tmux_command()
            .args(["set-option", "-t", session_name, "base-index", "1"])
            .output()
            .expect("set base-index");
        assert!(set.status.success());
        if index != "1" {
            let moved = crate::tmux::tmux_command()
                .args([
                    "move-window",
                    "-d",
                    "-s",
                    &window,
                    "-t",
                    &format!("{session_name}:1"),
                ])
                .output()
                .expect("move window");
            assert!(
                moved.status.success(),
                "move window: {}",
                String::from_utf8_lossy(&moved.stderr)
            );
        }
        let listed = crate::tmux::tmux_command()
            .args(["list-windows", "-t", session_name, "-F", "#{window_index}"])
            .output()
            .expect("list windows");
        assert!(listed.status.success());
        assert_eq!(String::from_utf8_lossy(&listed.stdout).trim(), "1");
    }

    struct GlobalPaneBaseIndex(String);

    impl GlobalPaneBaseIndex {
        fn set(value: &str) -> Self {
            let read = crate::tmux::tmux_command()
                .args(["show-options", "-g", "-v", "pane-base-index"])
                .output()
                .expect("tmux show-options -g pane-base-index");
            assert!(
                read.status.success(),
                "failed to read the global pane-base-index: {}",
                String::from_utf8_lossy(&read.stderr)
            );
            let previous = String::from_utf8_lossy(&read.stdout).trim().to_string();
            assert!(
                !previous.is_empty(),
                "tmux reported no global pane-base-index to restore"
            );
            let applied = crate::tmux::tmux_command()
                .args(["set-option", "-g", "pane-base-index", value])
                .output()
                .expect("tmux set-option -g pane-base-index");
            assert!(
                applied.status.success(),
                "failed to set a global pane-base-index of {value}"
            );
            Self(previous)
        }
    }

    impl Drop for GlobalPaneBaseIndex {
        fn drop(&mut self) {
            let _ = crate::tmux::tmux_command()
                .args(["set-option", "-g", "pane-base-index", &self.0])
                .output();
        }
    }

    /// `tmux new-session -d -s <name> -x <cols> -y <rows> <command…>` with the
    /// case's trailing argv appended verbatim.
    fn start_test_session(
        name: &str,
        size: (&str, &str),
        command: &[&str],
        extra: &[&str],
    ) -> std::process::Output {
        let mut args = vec!["new-session", "-d", "-s", name, "-x", size.0, "-y", size.1];
        args.extend_from_slice(command);
        args.extend_from_slice(extra);
        crate::tmux::tmux_command()
            .args(&args)
            .output()
            .expect("tmux new-session")
    }

    /// The same argv as owned strings, for cases that append option args to it.
    fn new_session_argv(name: &str, size: (&str, &str), command: &str) -> Vec<String> {
        [
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            size.0,
            "-y",
            size.1,
            command,
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect()
    }

    fn start_composite_session(name: &str, cols: u16, rows: u16, cmd: &str) -> Session {
        let status = start_test_session(
            name,
            (&cols.to_string(), &rows.to_string()),
            &[cmd],
            &[";", "set-option", "-t", name, "pane-base-index", "0"],
        )
        .status;
        assert!(status.success(), "failed to create {name}");
        refresh_session_cache();
        Session::from_name(name)
    }

    fn split_composite_session(session: &Session, cmd: &str) {
        let status = crate::tmux::tmux_command()
            .args(["split-window", "-h", "-t", &session.name, cmd])
            .status()
            .expect("tmux split-window");
        assert!(status.success(), "failed to split {}", session.name);
        refresh_session_cache();
    }

    fn wait_for_pane_text(session: &Session, needle: &str) {
        wait_for_text(session, needle, "pane", |s| s.capture_pane(20));
    }

    fn wait_for_composite_text(session: &Session, needle: &str) {
        wait_for_text(session, needle, "composite", |s| {
            s.capture_window_composited(20)
        });
    }

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

    #[test]
    #[serial_test::serial]
    fn wait_until_ready_blocks_until_the_marker_appears() {
        require_tmux!();
        let guard = TmuxTestSession::new("aoe_test_ready_marker");
        let name = guard.name().to_string();
        let temp = tempfile::tempdir().expect("release tempdir");
        let release = temp.path().join("release");
        let quote =
            |p: &std::path::Path| format!("'{}'", p.to_string_lossy().replace('\'', r#"'\''"#));
        let script = format!(
            "echo 'booting, please wait ...'; until [ -f {} ]; do sleep 0.02; done; echo 'ask anything...'; sleep 30",
            quote(&release)
        );
        let status = start_test_session(
            &name,
            ("80", "24"),
            &["sh", "-c", &script],
            &[";", "set-option", "-t", &name, "pane-base-index", "0"],
        )
        .status;
        assert!(status.success());
        refresh_session_cache();

        struct ReleaseOnDrop<'a>(&'a std::path::Path);
        impl Drop for ReleaseOnDrop<'_> {
            fn drop(&mut self) {
                let _ = std::fs::write(self.0, b"");
            }
        }
        let (captured_tx, captured_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let (stable, last, premature, returned_on_marker, released, joined) =
            std::thread::scope(|scope| {
                let name = &name;
                let waiter = scope.spawn(move || {
                    let _probe = ReadyCaptureGuard::install(ReadyCaptureProbe {
                        captured: captured_tx,
                        resume: resume_rx,
                    });
                    Session::from_name(name)
                        .wait_until_ready(std::time::Duration::from_secs(60), Some("ask anything"));
                });
                let _release_on_unwind = ReleaseOnDrop(&release);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let mut stable = 0;
                let mut last: Option<String> = None;
                let mut premature = false;
                while let Some(remaining) =
                    deadline.checked_duration_since(std::time::Instant::now())
                {
                    let Ok(now) = captured_rx.recv_timeout(remaining) else {
                        break;
                    };
                    premature |= now.to_lowercase().contains("ask anything");
                    stable = if now.trim().len() <= 20 {
                        0
                    } else if last.as_deref() == Some(now.as_str()) {
                        stable + 1
                    } else {
                        1
                    };
                    last = Some(now);
                    if stable >= 3 || premature {
                        break;
                    }
                    let _ = resume_tx.send(());
                }
                let released = std::fs::write(&release, b"");
                let _ = resume_tx.send(());
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let mut marker_seen = false;
                while let Some(remaining) =
                    deadline.checked_duration_since(std::time::Instant::now())
                {
                    let Ok(now) = captured_rx.recv_timeout(remaining) else {
                        break;
                    };
                    marker_seen = now.to_lowercase().contains("ask anything");
                    let _ = resume_tx.send(());
                    if marker_seen {
                        break;
                    }
                }
                let returned = captured_rx.recv_timeout(std::time::Duration::from_secs(2));
                drop(resume_tx);
                drop(captured_rx);
                (
                    stable,
                    last,
                    premature,
                    marker_seen
                        && matches!(
                            returned,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                        ),
                    released,
                    waiter.join(),
                )
            });
        joined.expect("readiness waiter exits");
        released.expect("release the pane");
        assert!(!premature, "the marker appeared before release");
        assert!(
            stable >= 3,
            "waiter did not reject consecutive stable captures: {last:?}"
        );
        assert!(
            returned_on_marker,
            "the waiter must return upon observing the released marker"
        );
    }

    #[test]
    fn chrome_rows_accounts_for_status_bar_and_ignores_splits() {
        assert_eq!(chrome_rows(67, 66), 1, "one status row");
        assert_eq!(chrome_rows(66, 66), 0, "no chrome");
        assert_eq!(chrome_rows(68, 66), 2, "two status rows");
        assert_eq!(chrome_rows(71, 66), 5, "max plausible chrome");
        assert_eq!(chrome_rows(40, 18), 0, "split layout is not chrome");
        assert_eq!(chrome_rows(10, 20), 0, "saturating, no panic");
    }

    #[test]
    fn pane_segments_split_the_chained_capture_by_sentinel() {
        let raw = "@@s@@ 0 0 6 2\nleft1\nleft2\n@@s@@ 7 0 6 2\nright1\nright2\n";
        let panes = parse_pane_segments(raw, "@@s@@");
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].geom.left, 0);
        assert_eq!(panes[1].geom.left, 7);
        assert_eq!(panes[0].rows.len(), 2);
        assert!(panes[0].rows[0].contains("left1"));
        assert!(panes[1].rows[1].contains("right2"));
    }

    #[test]
    fn pane_segments_drop_a_pane_with_unparseable_geometry() {
        let raw = "@@s@@ bogus\norphan\n@@s@@ 0 0 4 1\nkeep\n";
        let panes = parse_pane_segments(raw, "@@s@@");
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].geom.width, 4);
        assert!(panes[0].rows[0].contains("keep"));
    }

    #[test]
    fn pane_segments_are_empty_when_no_sentinel_appears() {
        assert!(parse_pane_segments("just some output\n", "@@s@@").is_empty());
    }

    #[test]
    fn raw_byte_batches_large_paste_roundtrips_in_order() {
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
        let c = PaneCursor::parse("3 2 1 24 120 74 1 1 0 0").expect("parses");
        assert!(c.mouse_tracking);
        assert!(!c.mouse_sgr);
        assert!(!c.mouse_all);
        let c = PaneCursor::parse("3 2 1 24 120 74 1 1 1 0").expect("parses");
        assert!(c.mouse_tracking && c.mouse_sgr);
        assert!(!c.mouse_all);
        let c = PaneCursor::parse("3 2 1 24 120 74").expect("parses");
        assert!(!c.alternate_on);
        assert!(!c.mouse_tracking);
        assert!(!c.mouse_sgr);
        assert!(!c.mouse_all);
        let c = PaneCursor::parse("3 2 0 24").expect("parses");
        assert!(!c.visible);
        assert_eq!(c.history_size, 0);
        assert_eq!(c.pane_width, 0);
        assert!(!c.alternate_on);
        assert!(!c.mouse_tracking);
        assert!(!c.mouse_sgr);
        assert!(!PaneCursor::parse("0 0 0 10").unwrap().visible);
        assert!(PaneCursor::parse("").is_none());
        assert!(PaneCursor::parse("1 2 3").is_none());
        assert!(PaneCursor::parse("a b c d").is_none());
        assert!(
            PaneCursor::parse("3 2 1 24 120 74 1 1 1")
                .unwrap()
                .position_reliable
        );
    }

    #[test]
    fn merge_cursor_probes_stable_mapping_keeps_after_and_trusts_position() {
        let before = PaneCursor::parse("3 2 1 24 120 80 1 1 1").unwrap();
        let after = PaneCursor::parse("5 4 1 24 120 80 1 1 1").unwrap();
        let merged = merge_cursor_probes(Some(before), Some(after)).expect("both probes => Some");
        assert_eq!((merged.x, merged.y), (5, 4));
        assert!(merged.position_reliable);
    }

    #[test]
    fn merge_cursor_probes_drift_keeps_modes_but_drops_position_trust() {
        let before = PaneCursor::parse("3 2 1 24 120 80 1 1 1").unwrap();
        let after = PaneCursor::parse("3 2 1 24 137 80 1 1 1").unwrap();
        let merged = merge_cursor_probes(Some(before), Some(after)).expect("both probes => Some");
        assert!(!merged.position_reliable);
        assert!(merged.alternate_on && merged.mouse_tracking && merged.mouse_sgr);

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
        require_tmux!();
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
    fn every_kind_is_marked_at_creation_and_keeps_its_mark_across_a_rename() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        require_tmux!();
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().to_string_lossy().to_string();
        let id = format!("kindmark{}", std::process::id());
        let title = "Vikings";

        let agent = Session::new(&id, title).expect("agent session");
        let _agent_guard = TmuxTestSession::from_name(agent.name());
        agent
            .create(&dir, Some("sleep 30"), "default")
            .expect("create the agent session");

        let terminal = crate::tmux::TerminalSession::new(&id, title).expect("terminal session");
        let _terminal_guard = TmuxTestSession::from_name(terminal.name());
        terminal
            .create_with_size(&dir, Some("sleep 30"), None, "default")
            .expect("create the paired terminal");

        let tool = crate::tmux::ToolSession::new(&id, title, "lazygit");
        let _tool_guard = TmuxTestSession::from_name(tool.session_name());
        tool.create_with_size(&dir, "sleep 30", None, "default")
            .expect("create the tool sub-session");

        let scan = crate::tmux::probe_live_sessions().expect("the scan reaches tmux");
        assert_eq!(
            scan.get(agent.name()).and_then(|s| s.kind),
            Some(crate::tmux::SessionKind::Agent),
        );
        assert_eq!(
            scan.get(terminal.name()).and_then(|s| s.kind),
            Some(crate::tmux::SessionKind::Terminal),
        );
        assert_eq!(
            scan.get(tool.session_name()).and_then(|s| s.kind),
            Some(crate::tmux::SessionKind::Tool),
        );

        let renamed = TmuxTestSession::new("aoe_test_kind_renamed");
        crate::tmux::tmux_command()
            .args(["rename-session", "-t", agent.name(), renamed.name()])
            .output()
            .expect("tmux rename-session");
        let scan = crate::tmux::probe_live_sessions().expect("the scan reaches tmux");
        assert_eq!(
            scan.get(renamed.name()).and_then(|s| s.kind),
            Some(crate::tmux::SessionKind::Agent),
            "the mark travels with the session, unlike its name"
        );
    }

    #[test]
    #[serial_test::serial]
    fn capture_remains_available_under_streaming_load() {
        require_tmux!();
        let guard = TmuxTestSession::new("aoe_test_race");
        let out = start_test_session(
            guard.name(),
            ("80", "24"),
            &["bash -c 'i=0; while true; do echo line-$((i++)); done'"],
            &[
                ";",
                "set-option",
                "-t",
                guard.name(),
                "pane-base-index",
                "0",
            ],
        );
        assert!(out.status.success());
        refresh_session_cache();
        let session = Session::from_name(guard.name());
        wait_for_pane_text(&session, "line-");

        for _ in 0..30 {
            let (content, _cursor) = session
                .capture_pane_with_cursor(50)
                .expect("capture should not error under load");
            assert!(
                content.contains("line-"),
                "capture must contain producer output"
            );
        }
    }

    fn owner_lock_session(prefix: &str) -> (TmuxTestSession, Session) {
        let guard = TmuxTestSession::new(prefix);
        let out = start_test_session(
            guard.name(),
            ("80", "24"),
            &["sleep 30"],
            &[
                ";",
                "set-window-option",
                "-t",
                guard.name(),
                "pane-base-index",
                "0",
            ],
        );
        assert!(out.status.success());
        refresh_session_cache();
        let session = Session::from_name(guard.name());
        (guard, session)
    }

    #[test]
    #[serial_test::serial]
    fn size_owner_lock_claims_rejects_steals_and_releases() {
        require_tmux!();
        let (guard, session) = owner_lock_session("aoe_test_owner");

        assert!(session.claim_size_owner("a", Duration::from_secs(10)));
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("a".to_string())
        );
        assert!(session.claim_size_owner("a", Duration::from_secs(10)));

        assert!(!session.claim_size_owner("b", Duration::from_secs(10)));
        assert!(session.refresh_size_owner("a"));
        assert!(!session.refresh_size_owner("b"));

        std::thread::sleep(Duration::from_millis(5));
        assert!(session.claim_size_owner("c", Duration::from_millis(1)));
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("c".to_string())
        );

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

        let _ = crate::tmux::fork_probe::take();
        {
            let _probe = crate::tmux::fork_probe::arm();
            assert!(session.resize_window_if_owner("d", 90, 30));
            assert_eq!(crate::tmux::fork_probe::take(), 3);
        }
        assert_eq!(pane_size(), (90, 30));
        assert!(!session.resize_window_if_owner("not-d", 91, 31));
        assert_eq!(pane_size(), (90, 30));

        assert!(!session.resize_window_if_owner("d", 0, 24));
        assert!(session.size_owner().is_none());
        assert!(session.steal_size_owner("d"));

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

        session.set_user_option(SIZE_OWNER_OPT, "partial");
        session.unset_user_option(SIZE_OWNER_HB_OPT);
        assert_eq!(session.has_active_size_owner(), None);
        assert!(session.claim_size_owner("recovered", Duration::from_secs(10)));
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("recovered".to_string())
        );
        session.release_size_owner("recovered");

        let literal_owner = "owner{with:literal";
        assert!(session.claim_size_owner(literal_owner, Duration::from_secs(10)));
        assert!(session.refresh_size_owner(literal_owner));
        session.release_size_owner(literal_owner);
        assert!(session.size_owner().is_none());
    }
    #[test]
    #[serial_test::serial]
    fn resize_window_if_owner_keeps_timeout_recovery_in_one_deadline() {
        require_tmux!();
        let (_guard, session) = owner_lock_session("aoe_test_owner_resize_deadline");
        assert!(session.steal_size_owner("owner"));

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
        require_tmux!();
        let (guard, session) = owner_lock_session("aoe_test_owner_resize_heartbeat");
        assert!(session.steal_size_owner("owner"));

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
        require_tmux!();
        let (guard, session) = owner_lock_session("aoe_test_vt_owner");

        assert!(session.claim_vt_owner("pid-1", Duration::from_secs(10)));
        assert!(session.claim_vt_owner("pid-1", Duration::from_secs(10)));
        assert!(!session.claim_vt_owner("pid-2", Duration::from_secs(10)));
        assert!(session.refresh_vt_owner("pid-1"));
        assert!(!session.refresh_vt_owner("pid-2"));
        std::thread::sleep(Duration::from_millis(5));
        assert!(session.claim_vt_owner("pid-3", Duration::from_millis(1)));

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

        session.set_user_option(VT_OWNER_OPT, "pid-4");
        session.set_user_option(VT_OWNER_HB_OPT, &now_ms().to_string());
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        session.release_vt_pipe_owner_with_deadline("pid-3", &deadline);
        assert!(session.refresh_vt_owner("pid-4"));
        assert!(!pane_is_piped());

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
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_cursor");
        let name = guard.name().to_string();
        let status = start_test_session(
            &name,
            ("40", "10"),
            &["sh -c 'printf hello; sleep 60'"],
            &[";", "set-option", "-t", &name, "pane-base-index", "0"],
        )
        .status;
        assert!(status.success());

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
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_tokens_no_enter");
        let name = guard.name().to_string();
        let status = start_test_session(
            &name,
            ("40", "10"),
            &[r#"sh -c 'read -r line; printf "got:<%s>" "$line"; sleep 60'"#],
            &[";", "set-option", "-t", &name, "pane-base-index", "0"],
        )
        .status;
        assert!(status.success());
        crate::tmux::test_inject_session_into_cache(&name);

        let session = Session::from_name(&name);
        session
            .send_key_tokens(&[crate::agents::KeyToken::Literal("hi")])
            .expect("send_key_tokens");

        let pane = only_pane_id(&name);
        for args in [
            vec!["send-keys", "-t", &pane, "-l", "--", "-tail"],
            vec!["send-keys", "-t", &pane, "Enter"],
        ] {
            assert!(crate::tmux::tmux_command()
                .args(args)
                .status()
                .expect("complete input line")
                .success());
        }
        wait_for_text(
            &session,
            "got:<hi-tail>",
            "one complete input line",
            |session| session.capture_pane(20),
        );
    }

    #[test]
    #[serial_test::serial]
    fn send_key_tokens_sends_exact_sequence_in_order() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_tokens_sequence");
        let name = guard.name().to_string();
        let status = start_test_session(
            &name,
            ("40", "10"),
            &["sh -c 'read -r line; printf \"got:%s\" \"$line\"; sleep 60'"],
            &[";", "set-option", "-t", &name, "pane-base-index", "0"],
        )
        .status;
        assert!(status.success());
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
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_remain");
        let session_name = guard.name().to_string();
        let output = start_test_session(
            &session_name,
            ("80", "24"),
            &["sleep 1"],
            &[
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ],
        );
        assert!(output.status.success());

        wait_for_pane_dead(&only_pane_id(&session_name));

        let exists = crate::tmux::tmux_command()
            .args(["has-session", "-t", &session_name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(exists, "Session should still exist due to remain-on-exit");

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
        require_tmux!();

        let key = "XDG_AOE_ENV_TEST_3075";
        let _env = crate::session::test_support::EnvGuard::set(&[(key, "sentinel-value")]);

        let guard = TmuxTestSession::new("aoe_test_env_fwd");
        let session = super::Session::from_name(guard.name());
        let created = session.create_with_size("/tmp", Some("sleep 5"), Some((80, 24)), "default");

        let shown = crate::tmux::tmux_command()
            .args(["show-environment", "-t", guard.name(), key])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());

        created.expect("create session");
        assert_eq!(
            shown.as_deref(),
            Some("XDG_AOE_ENV_TEST_3075=sentinel-value"),
            "a created agent session must carry the forwarded desktop/session env (#3075)"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_create_with_size_env_rejects_missing_working_dir() {
        require_tmux!();

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
        let _env = crate::session::test_support::EnvGuard::read_lock();
        let deadline = crate::tmux::TmuxCommandDeadline::with_timeout(Duration::ZERO);
        let before = crate::tmux::TMUX_COMMAND_EXECUTIONS.with(std::cell::Cell::get);
        let session = Session::from_name("aoe_test_expired_owner");
        assert!(!session.claim_vt_owner_with_deadline("owner", Duration::from_secs(10), &deadline,));
        session.release_vt_owner_with_deadline("owner", &deadline);
        session.release_vt_pipe_owner_with_deadline("owner", &deadline);
        assert_eq!(
            crate::tmux::TMUX_COMMAND_EXECUTIONS.with(std::cell::Cell::get),
            before,
            "expired owner operations must not attempt a subprocess"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_is_attached_false_for_detached_session() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_attached");
        let output = start_test_session(guard.name(), ("80", "24"), &["sleep 30"], &[]);
        assert!(output.status.success());

        let session = Session::from_name(guard.name());
        assert_eq!(
            session.is_attached(),
            Some(false),
            "Detached session should report is_attached() == false",
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_is_attached_true_with_live_client() {
        require_tmux!();

        let target = TmuxTestSession::new("aoe_test_attached_target");
        let created = start_test_session(target.name(), ("80", "24"), &["sleep 30"], &[]);
        assert!(created.status.success());

        let probe = crate::tmux::tmux_command();
        let mut argv = vec![probe.get_program().to_string_lossy().into_owned()];
        argv.extend(probe.get_args().map(|a| a.to_string_lossy().into_owned()));
        let attach_cmd = format!(
            "unset TMUX; TERM=xterm-256color exec {} attach-session -t {}",
            argv.join(" "),
            target.name()
        );

        let client = TmuxTestSession::new("aoe_test_attached_client");
        let spawned = start_test_session(client.name(), ("100", "40"), &[&attach_cmd], &[]);
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
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_alive");
        let session_name = guard.name().to_string();

        let output = start_test_session(
            &session_name,
            ("80", "24"),
            &["sleep 30"],
            &[
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ],
        );
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        let pane_dead = crate::tmux::tmux_command()
            .args(["display-message", "-t", &session_name, "-p", "#{pane_dead}"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        assert!(!pane_dead, "Pane should be alive while command is running");

        use crate::tmux::utils::{is_pane_dead, probe_pane, PaneProbe};
        assert_eq!(probe_pane(&session_name), PaneProbe::Alive);
        let absent = format!("{session_name}_absent");
        assert_eq!(probe_pane(&absent), PaneProbe::Missing);
        assert!(
            !is_pane_dead(&absent),
            "a missing session is not a dead pane"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_is_pane_dead_targets_window_zero_with_multiple_windows() {
        use crate::tmux::test_helpers::pane_field;

        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_multiwin");
        let session_name = guard.name().to_string();

        let mut args = new_session_argv(&session_name, ("80", "24"), "sleep 30");
        append_remain_on_exit_args(&mut args, &session_name);
        append_pane_base_index_args(&mut args, &session_name);
        let output = crate::tmux::tmux_command()
            .args(&args)
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());
        let first_pane = only_pane_id(&session_name);

        rebase_first_window_to_index_one(&session_name);

        let temp = tempfile::tempdir().unwrap();
        let release = temp.path().join("release");
        let output = crate::tmux::tmux_command()
            .args([
                "new-window",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                &session_name,
                &format!("until [ -e '{}' ]; do sleep 0.02; done", release.display()),
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-window");
        assert!(output.status.success());
        let second_pane = String::from_utf8(output.stdout).unwrap().trim().to_string();
        assert!(
            second_pane.starts_with('%'),
            "expected a pane id from new-window -P, got {second_pane:?}"
        );
        std::fs::write(&release, b"go").unwrap();
        wait_for_pane_dead(&second_pane);
        assert_eq!(pane_field(&first_pane, "#{pane_dead}"), "0");
        assert_eq!(pane_field(&session_name, "#{pane_id}"), second_pane);

        assert!(
            !is_pane_dead(&session_name),
            "is_pane_dead should check the first window's pane, not the active window"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_capture_pane_targets_first_window_with_multiple_windows() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_capture_multiwin");
        let session_name = guard.name().to_string();

        let mut args = new_session_argv(
            &session_name,
            ("80", "24"),
            "sh -c 'echo AOE_FIRST_WINDOW; exec sleep 30'",
        );
        append_pane_base_index_args(&mut args, &session_name);
        let output = crate::tmux::tmux_command()
            .args(&args)
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());
        let agent_pane = only_pane_id(&session_name);

        rebase_first_window_to_index_one(&session_name);

        let output = crate::tmux::tmux_command()
            .args(["new-window", "-t", &session_name, "sh"])
            .output()
            .expect("tmux new-window");
        assert!(output.status.success());

        wait_for_pane_command(&agent_pane, "sleep");

        let session = Session {
            name: session_name.clone(),
        };

        let content = session
            .capture_pane(10)
            .expect("capture_pane should not return an error for a valid session");
        assert!(
            content.contains("AOE_FIRST_WINDOW"),
            "capture_pane must read the first window's pane: {content:?}"
        );

        assert!(
            !session.is_pane_running_shell(),
            "is_pane_running_shell should check first window (sleep), not active window (sh)"
        );
    }

    #[test]
    #[serial_test::serial]
    fn composited_capture_matches_capture_pane_when_unsplit() {
        require_tmux!();

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

    #[test]
    #[serial_test::serial]
    fn composited_capture_includes_a_split_off_pane() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_composite_split");
        let session = start_composite_session(guard.name(), 80, 24, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
        split_composite_session(&session, "sh -c 'echo BRAVO; sleep 30'");
        wait_for_composite_text(&session, "BRAVO");

        let plain = session.capture_pane(10).expect("capture_pane");
        let composited = session
            .capture_window_composited(10)
            .expect("capture_window_composited");

        assert!(plain.contains("ALPHA"));
        assert!(
            !plain.contains("BRAVO"),
            "control: capture_pane should not see the split pane"
        );
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

    #[test]
    #[serial_test::serial]
    fn composited_cursor_and_pane0_origin_track_pane_border_offset() {
        require_tmux!();

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

    #[test]
    #[serial_test::serial]
    fn preview_captures_preserve_trailing_bg_fill() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_bg_fill");
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

    #[test]
    #[serial_test::serial]
    fn a_zoomed_pane_falls_back_to_the_plain_capture() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_composite_zoom");
        let session = start_composite_session(guard.name(), 40, 8, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
        split_composite_session(&session, "sh -c 'echo BRAVO; sleep 30'");
        wait_for_composite_text(&session, "BRAVO");

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

    #[test]
    #[serial_test::serial]
    fn a_stacked_split_composites_one_line_per_window_row() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_composite_rows");
        let session = start_composite_session(guard.name(), 30, 10, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
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

    #[test]
    #[serial_test::serial]
    fn captured_layout_puts_pane_zero_first_at_the_origin() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_layout_order");
        let session = start_composite_session(guard.name(), 80, 24, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
        split_composite_session(&session, "sh -c 'echo BRAVO; sleep 30'");
        wait_for_composite_text(&session, "BRAVO");
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
        assert!(
            layout.panes[0].rows.iter().any(|r| r.contains("ALPHA")),
            "pane 0 rows: {:?}",
            layout.panes[0].rows
        );
        assert!(layout.panes[1].rows.iter().any(|r| r.contains("BRAVO")));
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

    #[test]
    #[serial_test::serial]
    fn composited_capture_carries_the_pane_cursor() {
        require_tmux!();

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

    #[test]
    #[serial_test::serial]
    fn both_composite_transports_agree_on_a_static_window() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_composite_agree");
        let session = start_composite_session(guard.name(), 80, 24, "sh -c 'echo ALPHA; sleep 30'");
        wait_for_pane_text(&session, "ALPHA");
        split_composite_session(&session, "sh -c 'echo BRAVO; sleep 30'");
        wait_for_composite_text(&session, "BRAVO");

        let fallback = session
            .capture_window_composited(24)
            .expect("capture_window_composited");
        let layout = session.capture_window_layout(2).expect("layout");
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

    #[test]
    #[serial_test::serial]
    fn test_is_pane_running_shell_targets_first_window_with_multiple_windows() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_shell_multiwin");
        let session_name = guard.name().to_string();

        let mut args = new_session_argv(&session_name, ("80", "24"), "sleep 30");
        append_pane_base_index_args(&mut args, &session_name);
        let output = crate::tmux::tmux_command()
            .args(&args)
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());
        let agent_pane = only_pane_id(&session_name);

        rebase_first_window_to_index_one(&session_name);

        let output = crate::tmux::tmux_command()
            .args(["new-window", "-t", &session_name, "sh"])
            .output()
            .expect("tmux new-window");
        assert!(output.status.success());

        wait_for_pane_command(&agent_pane, "sleep");

        assert!(
            !is_pane_running_shell(&session_name),
            "is_pane_running_shell should target first window (sleep), not active window (sh)"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_status_checks_target_pane_zero_with_split_panes() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_splitpane");
        let session_name = guard.name().to_string();

        let mut args = new_session_argv(&session_name, ("80", "24"), "sleep 30");
        append_remain_on_exit_args(&mut args, &session_name);
        append_pane_base_index_args(&mut args, &session_name);
        let output = crate::tmux::tmux_command()
            .args(&args)
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());
        let agent_pane = only_pane_id(&session_name);

        let output = crate::tmux::tmux_command()
            .args([
                "split-window",
                "-t",
                &session_name,
                "-P",
                "-F",
                "#{pane_id}",
                "/bin/bash --noprofile --norc -c 'read -r line'",
            ])
            .output()
            .expect("tmux split-window");
        assert!(output.status.success());
        let split = String::from_utf8(output.stdout)
            .expect("pane ID")
            .trim()
            .to_string();
        assert!(split.starts_with('%'));
        assert!(crate::tmux::tmux_command()
            .args([
                "set-option",
                "-p",
                "-t",
                &split,
                "remain-on-exit",
                "on",
                ";",
                "select-pane",
                "-t",
                &split
            ])
            .status()
            .expect("retain and select split")
            .success());
        assert_eq!(pane_field(&session_name, "#{pane_id}"), split);
        wait_for_pane_command(&agent_pane, "sleep");
        wait_for_pane_command(&split, "bash");
        assert!(
            !is_pane_running_shell(&session_name),
            "status must target the agent, not the active shell"
        );
        assert!(crate::tmux::tmux_command()
            .args(["send-keys", "-t", &split, "Enter"])
            .status()
            .expect("release split shell")
            .success());
        wait_for_pane_dead(&split);
        assert_eq!(pane_field(&agent_pane, "#{pane_dead}"), "0");
        assert_eq!(pane_field(&session_name, "#{pane_id}"), split);
        assert!(
            !is_pane_dead(&session_name),
            "status must target the live agent, not the dead active split"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_status_checks_with_split_panes_and_pane_base_index_1() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_splitpbi");
        let session_name = guard.name().to_string();

        let mut args = new_session_argv(&session_name, ("80", "24"), "sleep 30");
        append_remain_on_exit_args(&mut args, &session_name);
        append_pane_base_index_args(&mut args, &session_name);
        let output = crate::tmux::tmux_command()
            .args(&args)
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());
        let agent_pane = only_pane_id(&session_name);

        let _global_pane_base_index = GlobalPaneBaseIndex::set("1");

        let listed = crate::tmux::tmux_command()
            .args(["list-panes", "-t", &session_name, "-F", "#{pane_index}"])
            .output()
            .expect("tmux list-panes");
        let indices = String::from_utf8_lossy(&listed.stdout);
        assert!(
            indices.lines().any(|line| line.trim() == "0"),
            "the session pin must keep pane 0 addressable under a global \
             pane-base-index of 1: {indices:?}"
        );

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

    /// The whole `new-session` argv, so a reordering cannot slip through.
    #[test]
    fn build_create_args_argv_table() {
        let launch_id = crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY;
        let launch_env = format!("{launch_id}=non-secret-generation");
        let base = ["new-session", "-d", "-s", "test_session", "-c", "/tmp/work"];
        let cases: Vec<(&str, Vec<String>, Vec<String>)> = vec![
            (
                "no size, no command",
                build_create_args("test_session", "/tmp/work", &[], None, None),
                base.iter().map(|a| a.to_string()).collect(),
            ),
            (
                "command only",
                build_create_args("test_session", "/tmp/work", &[], Some("claude"), None),
                base.iter()
                    .chain(["claude"].iter())
                    .map(|a| a.to_string())
                    .collect(),
            ),
            (
                "size only",
                build_create_args("test_session", "/tmp/work", &[], None, Some((120, 40))),
                base.iter()
                    .chain(["-x", "120", "-y", "40"].iter())
                    .map(|a| a.to_string())
                    .collect(),
            ),
            (
                "size and command",
                build_create_args(
                    "test_session",
                    "/tmp/work",
                    &[],
                    Some("claude"),
                    Some((80, 24)),
                ),
                base.iter()
                    .chain(["-x", "80", "-y", "24", "claude"].iter())
                    .map(|a| a.to_string())
                    .collect(),
            ),
            (
                "non-secret launch id rides in -e",
                build_create_args(
                    "test_session",
                    "/tmp/work",
                    &[(launch_id, "non-secret-generation")],
                    Some("omp"),
                    None,
                ),
                base.iter()
                    .map(|a| a.to_string())
                    .chain(["-e".to_string(), launch_env.clone(), "omp".to_string()])
                    .collect(),
            ),
        ];
        for (label, got, want) in cases {
            assert_eq!(got, want, "{label}");
        }
    }

    #[test]
    fn test_protected_env_file_keeps_secret_out_of_pane_argv_and_rejects_invalid_keys() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
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
        let _env = crate::session::test_support::EnvGuard::read_lock();
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

    #[test]
    fn test_container_env_file_does_not_mutate_host_process_environment() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
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
        let mut expected = host_path.as_encoded_bytes().to_vec();
        expected.extend_from_slice(b"\nunset");
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
    fn test_protected_env_reaches_child_without_exposing_secret_in_ps() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        require_tmux!();
        let guard = TmuxTestSession::new("aoe_test_protected_env");
        let session = Session::from_name(guard.name());
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("value");
        let secret_value = format!("AOE_PROTECTED_{} secret", std::process::id());
        let script = format!(
            "printf '%s' \"$AOE_TEST_PROTECTED_VALUE\" > {}; \
             printf 'protected-ready\\n'; read -r proceed; exec sleep 30",
            crate::session::environment::shell_escape(&output.to_string_lossy())
        );
        let command = format!(
            "exec /bin/bash --noprofile --norc -c {}",
            crate::session::environment::shell_escape(&script)
        );

        session
            .create_with_size_env(
                "/tmp",
                Some(&command),
                Some((80, 24)),
                "default",
                &[PaneEnvMutation::set(
                    "AOE_TEST_PROTECTED_VALUE".to_string(),
                    secret_value.clone(),
                )],
            )
            .unwrap();

        wait_for_pane_text(&session, "protected-ready");
        let pane_id = only_pane_id(guard.name());
        wait_for_pane_command(&pane_id, "bash");
        assert_eq!(std::fs::read_to_string(output).unwrap(), secret_value);
        assert!(
            !session.is_pane_running_shell(),
            "a live protected shell must not look like an exited agent"
        );

        session.send_keys("continue").unwrap();
        wait_for_pane_command(&pane_id, "sleep");
        let pane_pid = pane_field(&pane_id, "#{pane_pid}")
            .parse::<u32>()
            .expect("numeric pane PID");
        let ps_output = std::process::Command::new("ps")
            .args(["auxww"])
            .output()
            .expect("ps auxww");
        assert!(
            ps_output.status.success(),
            "ps failed: {}",
            String::from_utf8_lossy(&ps_output.stderr)
        );
        let ps_text = String::from_utf8_lossy(&ps_output.stdout);
        assert!(
            ps_text.lines().skip(1).any(|line| {
                line.split_whitespace()
                    .nth(1)
                    .and_then(|pid| pid.parse::<u32>().ok())
                    == Some(pane_pid)
            }),
            "the exec process must be present in the inspected ps snapshot"
        );
        assert!(
            !ps_text.contains(&secret_value),
            "the protected value must not appear in process arguments"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_is_pane_running_shell_on_shell_session() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_shell");
        let session_name = guard.name().to_string();

        let output = start_test_session(
            &session_name,
            ("80", "24"),
            &["/bin/bash --noprofile --norc"],
            &[],
        );
        assert!(output.status.success());

        wait_for_pane_command(&only_pane_id(&session_name), "bash");

        assert!(
            is_pane_running_shell(&session_name),
            "Session running bash should be detected as a shell"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_is_pane_running_shell_on_non_shell_session() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_noshell");
        let session_name = guard.name().to_string();

        let output = start_test_session(&session_name, ("80", "24"), &["sleep", "30"], &[]);
        assert!(output.status.success());

        wait_for_pane_command(&only_pane_id(&session_name), "sleep");

        assert!(
            !is_pane_running_shell(&session_name),
            "Session running 'sleep' should not be detected as a shell"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_respawn_dead_pane_revives_dead_pane() {
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_respawn");
        let session_name = guard.name().to_string();

        let output = start_test_session(
            &session_name,
            ("80", "24"),
            &["true"],
            &[
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
            ],
        );
        assert!(output.status.success());

        let pane_id = only_pane_id(&session_name);
        wait_for_pane_dead(&pane_id);

        let session = Session::from_name(&session_name);
        crate::tmux::refresh_session_cache();

        assert!(session.exists(), "Session should exist via remain-on-exit");
        assert!(session.is_pane_dead(), "Pane should be dead after `true`");

        let respawned = session
            .respawn_dead_pane("/tmp", Some("sleep 30"))
            .expect("respawn_dead_pane should succeed");
        assert!(respawned, "respawn_dead_pane should report it acted");

        wait_for_pane_command(&pane_id, "sleep");
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
