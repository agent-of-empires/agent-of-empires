//! Clipboard write for preview drag-select.
//!
//! Two paths fire on every copy:
//!
//! 1. **Platform subprocess** — `pbcopy` on macOS, `wl-copy` on Wayland,
//!    `xclip`/`xsel` on X11. This is the load-bearing path when AoE
//!    runs locally: it's how every other terminal app copies, it
//!    doesn't care whether the process owns a GUI handle, and it
//!    survives the TUI dropping back into raw mode mid-write.
//! 2. **OSC 52** — `\x1b]52;c;<base64>\x07` written to stdout, wrapped
//!    in tmux's DCS passthrough when `$TMUX` is set. This is what
//!    carries the bytes back through SSH and Mosh, and through tmux
//!    when `allow-passthrough on` is configured (default on tmux 3.3+).
//!
//! Both are best-effort: OSC 52 gives us no acknowledgement, and the
//! subprocess binaries may not exist on a stripped-down system. Each
//! path's outcome is logged at `tracing::info` so a future "clipboard
//! didn't update" bug report can be diagnosed with
//! `AGENT_OF_EMPIRES_DEBUG=1`.
use std::io::Write;
use std::process::{Command, Stdio};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

/// Maximum OSC 52 payload (raw bytes before base64). Many terminals
/// cap the sequence around 100 KiB; we pick 1 MiB so single-screen
/// selections always fit and oversized selections truncate cleanly
/// instead of corrupting the terminal stream.
const MAX_BYTES: usize = 1024 * 1024;

/// Push `text` to the user's clipboard via every mechanism this
/// platform supports. Returns the number of bytes (post-truncation)
/// emitted.
pub fn copy_to_clipboard(text: &str) -> usize {
    let truncated = if text.len() > MAX_BYTES {
        &text.as_bytes()[..MAX_BYTES]
    } else {
        text.as_bytes()
    };
    let truncated_str = match std::str::from_utf8(truncated) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(&truncated[..e.valid_up_to()]).unwrap_or(""),
    };

    let subprocess = try_subprocess(truncated_str);
    let osc52 = write_osc52(truncated);

    tracing::info!(
        target: "tui.clipboard",
        bytes = truncated.len(),
        subprocess = format!("{:?}", subprocess).as_str(),
        osc52_ok = osc52.is_ok(),
        "preview drag-select copy"
    );

    truncated.len()
}

/// Run a platform-specific clipboard subprocess. `Ok(cmd_name)` on
/// success so the caller can show which one landed; `Err(reason)`
/// otherwise.
#[cfg(target_os = "macos")]
fn try_subprocess(text: &str) -> Result<&'static str, String> {
    run_subprocess("pbcopy", &[], text)
}

#[cfg(target_os = "linux")]
fn try_subprocess(text: &str) -> Result<&'static str, String> {
    let mut last_err: Option<String> = None;
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match run_subprocess("wl-copy", &[], text) {
            Ok(name) => return Ok(name),
            Err(reason) => last_err = Some(reason),
        }
    }
    match run_subprocess("xclip", &["-selection", "clipboard"], text) {
        Ok(name) => Ok(name),
        Err(xclip_err) => match run_subprocess("xsel", &["--clipboard", "--input"], text) {
            Ok(name) => Ok(name),
            Err(xsel_err) => Err(last_err
                .map(|e| format!("{e}; {xclip_err}; {xsel_err}"))
                .unwrap_or_else(|| format!("{xclip_err}; {xsel_err}"))),
        },
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn try_subprocess(_text: &str) -> Result<&'static str, String> {
    Err("no subprocess clipboard for this platform".to_string())
}

/// Upper bound on how long we wait for a clipboard subprocess to
/// finish. `pbcopy` / `wl-copy` / `xclip` normally exit in low
/// milliseconds; anything slower is a sign that the helper is hung
/// (no display server, X selection contention, etc.), and the
/// drag-select UX is much better off killing the helper and falling
/// through to OSC 52 than freezing the TUI on `child.wait()`.
const SUBPROCESS_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

fn run_subprocess(cmd: &'static str, args: &[&str], text: &str) -> Result<&'static str, String> {
    use std::io::Seek;
    // A helper that never reads must not block before the wait deadline starts.
    let mut input = tempfile::tempfile().map_err(|e| format!("{cmd} stdin: {e}"))?;
    input
        .write_all(text.as_bytes())
        .and_then(|_| input.rewind())
        .map_err(|e| format!("{cmd} stdin write: {e}"))?;
    let mut child = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::from(input))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("{cmd} spawn: {e}")),
    };
    match crate::process::wait_with_timeout(&mut child, SUBPROCESS_WAIT) {
        Ok(Some(status)) if status.success() => Ok(cmd),
        Ok(Some(status)) => Err(format!("{cmd} exit {status}")),
        Ok(None) => Err(format!("{cmd} timed out after {:?}", SUBPROCESS_WAIT)),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("{cmd} wait: {error}"))
        }
    }
}

/// Write OSC 52 to stdout, wrapping in tmux's DCS passthrough when
/// `$TMUX` is set. Without the passthrough wrapper, tmux either
/// swallows the escape (default `set-clipboard off`) or buffers it
/// internally without forwarding to the parent terminal — neither
/// of which puts bytes on the user's local clipboard. The wrapper
/// (`\x1bPtmux;\x1b<inner>\x1b\\`) tells tmux to pass the inner
/// sequence through verbatim, provided `set -g allow-passthrough on`
/// is in the user's tmux.conf (default on tmux 3.3+).
///
/// We emit BOTH the plain and the wrapped form: the wrapped form
/// reaches the parent terminal through tmux passthrough; the plain
/// form covers the case where the user isn't in tmux despite
/// `$TMUX` being set (e.g. forwarded env var from a previous shell)
/// and any terminal that ignores tmux DCS sequences will just drop
/// the wrapped one as garbage.
fn write_osc52(bytes: &[u8]) -> std::io::Result<()> {
    let encoded = STANDARD.encode(bytes);
    let in_tmux = std::env::var_os("TMUX").is_some();
    let mut stdout = std::io::stdout().lock();
    if in_tmux {
        // Tmux DCS passthrough: \x1bPtmux;<escaped inner>\x1b\\
        // where each ESC inside the inner sequence is doubled.
        // OSC 52 contains exactly one ESC (the opening), so we
        // emit it as `\x1b\x1b]52;c;...;\x07`.
        write!(stdout, "\x1bPtmux;\x1b\x1b]52;c;{}\x07\x1b\\", encoded)?;
    }
    write!(stdout, "\x1b]52;c;{}\x07", encoded)?;
    stdout.flush()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn a_clipboard_helper_that_never_reads_cannot_hold_its_input_writer() {
        use std::os::unix::fs::OpenOptionsExt;

        struct ReleaseGate(std::fs::File);
        impl Drop for ReleaseGate {
            fn drop(&mut self) {
                let _ = self.0.write_all(b"release\n");
            }
        }

        let _environment = crate::session::test_support::EnvGuard::read_lock();
        let fixture = tempfile::tempdir().unwrap();
        let entered = fixture.path().join("entered");
        let path = fixture.path().join("gate");
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let marker = entered.clone();
        let (finished, received) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let release = ReleaseGate(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(nix::libc::O_NONBLOCK)
                    .open(&path)
                    .unwrap(),
            );
            let worker = scope.spawn(move || {
                let payload = "x".repeat(MAX_BYTES);
                let result = run_subprocess(
                    "sh",
                    &[
                        "-c",
                        "printf '%s\\n' \"$$\" > \"$1\"; read -r line < \"$2\"",
                        "sh",
                        marker.to_str().unwrap(),
                        path.to_str().unwrap(),
                    ],
                    &payload,
                );
                assert!(finished.send(result).is_ok());
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let pid = loop {
                if let Some(pid) = std::fs::read_to_string(&entered)
                    .ok()
                    .and_then(|raw| raw.trim().parse::<i32>().ok())
                    .filter(|pid| *pid > 0)
                {
                    break nix::unistd::Pid::from_raw(pid);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "clipboard fixture did not enter"
                );
                std::thread::yield_now();
            };
            let result = received.recv_timeout(std::time::Duration::from_secs(2));
            let reaped_before_release =
                nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
            if matches!(
                reaped_before_release,
                Ok(nix::sys::wait::WaitStatus::StillAlive)
            ) {
                let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                let _ = nix::sys::wait::waitpid(pid, None);
            }
            drop(release);
            worker.join().unwrap();
            assert!(
                matches!(reaped_before_release, Err(nix::errno::Errno::ECHILD)),
                "helper must exit and be reaped before FIFO release: {reaped_before_release:?}"
            );
            assert!(result
                .expect("stdin write outlived the helper deadline")
                .is_err());
        });
    }
}
