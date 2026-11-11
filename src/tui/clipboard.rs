//! Clipboard output through desktop helpers and streamed OSC 52.
//! OSC 52 has no acknowledgement; terminal-specific limits still apply.

use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;
use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    process::{Command, Stdio},
};
use tokio_util::sync::CancellationToken;

static ACTIVE: Mutex<Weak<Shared>> = Mutex::new(Weak::new());

struct Pending {
    ticket: u64,
    file: File,
    timeout: Duration,
    reply: tokio::sync::oneshot::Sender<io::Result<CopyReport>>,
}
struct Queue {
    latest: u64,
    pending: Option<Pending>,
    live: Option<(u64, tokio::sync::oneshot::Receiver<io::Result<CopyReport>>)>,
}
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    cancel: CancellationToken,
}

pub(crate) struct Executor {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub(super) struct StopHandle(Weak<Shared>);

impl StopHandle {
    pub(super) fn stop(&self) {
        if let Some(shared) = self.0.upgrade() {
            shared.stop();
        }
    }
}

impl Shared {
    fn stop(&self) {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        self.cancel.cancel();
        queue.pending = None;
        self.ready.notify_all();
    }
}
impl Executor {
    pub fn new() -> io::Result<Self> {
        let executor = Self::start(copy_file_with_timeout)?;
        *ACTIVE.lock().unwrap_or_else(|error| error.into_inner()) =
            Arc::downgrade(&executor.shared);
        Ok(executor)
    }

    #[cfg(test)]
    pub(super) fn activate_for_test(&self) {
        *ACTIVE.lock().unwrap_or_else(|error| error.into_inner()) = Arc::downgrade(&self.shared);
    }

    pub(super) fn stop_handle(&self) -> StopHandle {
        StopHandle(Arc::downgrade(&self.shared))
    }
    pub(super) fn start(
        mut output: impl FnMut(File, Duration, &CancellationToken) -> io::Result<CopyReport>
            + Send
            + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                latest: 0,
                pending: None,
                live: None,
            }),
            ready: Condvar::new(),
            cancel: CancellationToken::new(),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("aoe-clipboard".into())
            .spawn(move || loop {
                let pending = {
                    let mut queue = worker
                        .queue
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    while !worker.cancel.is_cancelled() && queue.pending.is_none() {
                        queue = worker
                            .ready
                            .wait(queue)
                            .unwrap_or_else(|error| error.into_inner());
                    }
                    if worker.cancel.is_cancelled() {
                        break;
                    }
                    let pending = queue.pending.take().expect("clipboard request available");
                    if pending.ticket != queue.latest {
                        continue;
                    }
                    pending
                };
                let result = output(pending.file, pending.timeout, &worker.cancel);
                let _ = pending.reply.send(result);
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub fn feedback(&self) -> Option<String> {
        use tokio::sync::oneshot::error::TryRecvError;
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (number, mut receiver) = queue.live.take()?;
        if number != queue.latest {
            return None;
        }
        match receiver.try_recv() {
            Ok(Ok(report)) => Some(report.message()),
            Ok(Err(error)) => Some(format!("Copy failed: {error}")),
            Err(TryRecvError::Closed) => Some("Copy cancelled before output completed.".into()),
            Err(TryRecvError::Empty) => {
                queue.live = Some((number, receiver));
                None
            }
        }
    }
}
impl Drop for Executor {
    fn drop(&mut self) {
        self.shared.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
pub(crate) struct Ticket {
    number: u64,
    shared: Weak<Shared>,
}
pub(crate) fn reserve() -> io::Result<Ticket> {
    let shared = ACTIVE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .upgrade()
        .ok_or_else(|| io::Error::other("clipboard executor unavailable"))?;
    reserve_for(&shared)
}
fn reserve_for(shared: &Arc<Shared>) -> io::Result<Ticket> {
    let mut queue = shared
        .queue
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if shared.cancel.is_cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    queue.latest = queue
        .latest
        .checked_add(1)
        .ok_or_else(|| io::Error::other("clipboard ticket overflow"))?;
    queue.pending = None;
    Ok(Ticket {
        number: queue.latest,
        shared: Arc::downgrade(shared),
    })
}
impl Ticket {
    pub fn submit(
        &self,
        file: File,
        timeout: Duration,
    ) -> io::Result<tokio::sync::oneshot::Receiver<io::Result<CopyReport>>> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| io::Error::from(io::ErrorKind::Interrupted))?;
        let mut queue = shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if shared.cancel.is_cancelled() || queue.latest != self.number {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let (reply, result) = tokio::sync::oneshot::channel();
        queue.pending = Some(Pending {
            ticket: self.number,
            file,
            timeout,
            reply,
        });
        shared.ready.notify_one();
        Ok(result)
    }
}

use base64::{engine::general_purpose::STANDARD, write::EncoderWriter};

const OSC52_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub(crate) enum Osc52Outcome {
    Emitted,
    SkippedForSize,
    Failed(io::Error),
}

#[derive(Debug)]
pub(crate) struct CopyReport {
    pub bytes: u64,
    pub desktop: Option<&'static str>,
    pub osc52: Osc52Outcome,
}

impl CopyReport {
    pub(crate) fn message(&self) -> String {
        let mut message = if let Some(desktop) = self.desktop {
            format!("Copied {} bytes via {desktop}.", self.bytes)
        } else if matches!(self.osc52, Osc52Outcome::Emitted) {
            format!(
                "Emitted {} bytes via OSC 52; terminal clipboard support required.",
                self.bytes
            )
        } else {
            "No clipboard output completed.".into()
        };
        match &self.osc52 {
            Osc52Outcome::Emitted => {}
            Osc52Outcome::SkippedForSize => {
                use std::fmt::Write;
                let _ = write!(
                    message,
                    " OSC 52 skipped: payload exceeds {OSC52_MAX_BYTES} bytes."
                );
            }
            Osc52Outcome::Failed(error) => {
                use std::fmt::Write;
                let _ = write!(message, " OSC 52 failed: {error}.");
            }
        }
        message
    }
}

pub(crate) fn finalize_export(file: tempfile::NamedTempFile) -> io::Result<File> {
    let input = File::open(file.path())?;
    file.close()?;
    Ok(input)
}

pub fn copy_to_clipboard(text: &str) -> String {
    let result = (|| {
        let ticket = reserve()?;
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(text.as_bytes())?;
        let receiver = ticket.submit(finalize_export(file)?, Duration::from_millis(500))?;
        let shared = ticket
            .shared
            .upgrade()
            .ok_or_else(|| io::Error::from(io::ErrorKind::Interrupted))?;
        let mut queue = shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if queue.latest != ticket.number {
            return Err(io::ErrorKind::Interrupted.into());
        }
        queue.live = Some((ticket.number, receiver));
        Ok::<_, io::Error>(())
    })();
    match result {
        Ok(()) => "Copy queued; waiting for clipboard output.".into(),
        Err(error) => {
            tracing::warn!(target: "tui.clipboard", %error, "clipboard admission failed");
            format!("Copy failed: {error}")
        }
    }
}

fn copy_file_with_timeout(
    mut file: File,
    timeout: Duration,
    cancel: &CancellationToken,
) -> io::Result<CopyReport> {
    if cancel.is_cancelled() {
        return Err(io::ErrorKind::ConnectionAborted.into());
    }
    let bytes = file.metadata()?.len();
    let desktop = try_subprocess(&mut file, timeout, cancel);
    if cancel.is_cancelled() {
        return Err(io::ErrorKind::ConnectionAborted.into());
    }
    let osc52 = if bytes > OSC52_MAX_BYTES {
        Osc52Outcome::SkippedForSize
    } else {
        let tmux = std::env::var_os("TMUX").is_some();
        match super::terminal_output::osc52(timeout, tmux, cancel, |output| {
            write_osc52(&mut file, output, tmux, bytes)
        }) {
            Ok(()) => Osc52Outcome::Emitted,
            Err(error) => Osc52Outcome::Failed(error),
        }
    };
    tracing::info!(target: "tui.clipboard", bytes, desktop = ?desktop, osc52 = ?osc52, "clipboard output");
    if let Err(desktop) = &desktop {
        match &osc52 {
            Osc52Outcome::Emitted => {}
            Osc52Outcome::SkippedForSize => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("{desktop}; OSC 52 skipped: payload exceeds {OSC52_MAX_BYTES} bytes"),
                ));
            }
            Osc52Outcome::Failed(error) => {
                return Err(io::Error::other(format!("{desktop}; OSC 52: {error}")));
            }
        }
    }
    Ok(CopyReport {
        bytes,
        desktop: desktop.ok(),
        osc52,
    })
}

fn try_subprocess(
    file: &mut File,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<&'static str, String> {
    let mut errors = String::new();
    for (command, arguments) in crate::process::clipboard_commands() {
        if cancel.is_cancelled() {
            return Err("clipboard output cancelled".into());
        }
        match run_subprocess(command, arguments, file, timeout, cancel) {
            Ok(()) => return Ok(command),
            Err(error) => {
                if !errors.is_empty() {
                    errors.push_str("; ");
                }
                errors.push_str(&format!("{command}: {error}"));
            }
        }
    }
    Err(if errors.is_empty() {
        "no desktop clipboard helper for this platform".into()
    } else {
        errors
    })
}
fn run_subprocess(
    command: &str,
    arguments: &[&str],
    file: &mut File,
    timeout: std::time::Duration,
    cancel: &CancellationToken,
) -> io::Result<()> {
    file.rewind()?;
    let mut command = Command::new(command);
    command
        .args(arguments)
        .stdin(file.try_clone()?)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = crate::process::run_status_with_timeout_process_group(
        &mut command,
        timeout,
        Some(cancel),
        crate::process::ProcessGroupCleanup::OnFailure,
    )?;
    if cancel.is_cancelled() {
        return Err(io::ErrorKind::ConnectionAborted.into());
    }
    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(io::Error::other(format!("exit {status}"))),
        None => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("clipboard helper exceeded {timeout:?}"),
        )),
    }
}

fn write_osc52(
    input: &mut (impl Read + Seek),
    output: &mut (impl Write + ?Sized),
    in_tmux: bool,
    bytes: u64,
) -> io::Result<()> {
    if bytes > OSC52_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "OSC 52 payload exceeds the byte limit",
        ));
    }
    if in_tmux {
        input.rewind()?;
        sequence(
            input,
            output,
            bytes,
            b"\x1bPtmux;\x1b\x1b]52;c;",
            b"\x07\x1b\\",
        )?;
    }
    input.rewind()?;
    sequence(input, output, bytes, b"\x1b]52;c;", b"\x07")?;
    output.flush()
}
fn sequence(
    input: &mut impl Read,
    output: &mut (impl Write + ?Sized),
    bytes: u64,
    prefix: &[u8],
    suffix: &[u8],
) -> io::Result<()> {
    output.write_all(prefix)?;
    let result: io::Result<()> = (|| {
        let mut encoded = EncoderWriter::new(&mut *output, &STANDARD);
        let count = io::copy(&mut (&mut *input).take(bytes), &mut encoded)?;
        let mut extra = [0];
        if count != bytes || input.read(&mut extra)? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "clipboard payload changed during OSC 52 output",
            ));
        }
        encoded.finish()?;
        Ok(())
    })();
    result?;
    output.write_all(suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[serial_test::serial]
    fn executor_stop_reaps_held_helper_without_fallback_or_terminal_emission() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Instant;
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let started = root.path().join("started");
        let calls = root.path().join("calls");
        let gate = root.path().join("gate");
        nix::unistd::mkfifo(
            &gate,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        struct ReleaseGate(File);
        impl Drop for ReleaseGate {
            fn drop(&mut self) {
                let _ = writeln!(self.0, "release");
            }
        }
        let mut release = ReleaseGate(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&gate)
                .unwrap(),
        );
        let commands: Vec<_> = crate::process::clipboard_commands().collect();
        for (command, _) in &commands {
            let path = bin.join(command);
            std::fs::write(&path, format!(
                "#!/bin/sh\ntrap '' TERM\nprintf '%s\\n' \"$0\" >> {}\nprintf '%s\\n' \"$$\" > {}\nIFS= read -r release < {}\n",
                shell_words::quote(calls.to_str().unwrap()),
                shell_words::quote(started.to_str().unwrap()),
                shell_words::quote(gate.to_str().unwrap()),
            )).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        let _path = crate::session::test_support::EnvGuard::set(&[("PATH", path)]);
        let (mut master, slave) = super::super::terminal_output::tests::private_terminal();
        let _control = slave.try_clone().unwrap();
        let executor = Executor::start(move |file, timeout, cancel| {
            let (_guard, _) =
                super::super::terminal_output::install_test_output(slave.try_clone()?);
            copy_file_with_timeout(file, timeout, cancel)
        })
        .unwrap();
        let ticket = reserve_for(&executor.shared).unwrap();
        let mut export = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        export.write_all(b"owned export").unwrap();
        let result = ticket
            .submit(finalize_export(export).unwrap(), Duration::from_secs(30))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&started)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "clipboard helper did not start");
            std::thread::yield_now();
        };
        let birth = crate::process::process_birth_identity(pid)
            .unwrap()
            .unwrap();
        let helper = crate::process::CapturedProcess::capture(pid, &birth)
            .unwrap()
            .unwrap();
        executor.stop_handle().stop();
        let (finished, stopped) = std::sync::mpsc::channel();
        let joining = std::thread::spawn(move || {
            drop(executor);
            let _ = finished.send(());
        });
        let prompt_stop = stopped.recv_timeout(Duration::from_secs(2));
        if prompt_stop.is_err() {
            writeln!(release.0, "release").unwrap();
        }
        joining.join().unwrap();
        prompt_stop.expect("shutdown must not wait for the retained copy's 30s timeout");
        assert_eq!(
            result.blocking_recv().unwrap().unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert!(helper.exited().unwrap());
        let mut status = 0;
        // SAFETY: this was the acknowledged direct helper; no signal is sent.
        assert_eq!(
            unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(
            std::fs::read_to_string(calls).unwrap(),
            format!("{}\n", bin.join(commands[0].0).display())
        );
        let mut byte = [0];
        assert_eq!(
            master.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn osc52_respects_raw_byte_limit_in_plain_and_tmux_sequences() {
        for bytes in [OSC52_MAX_BYTES - 1, OSC52_MAX_BYTES, OSC52_MAX_BYTES + 1] {
            let mut text = "界".repeat(bytes as usize / 3);
            for _ in 0..bytes % 3 {
                text.push('a');
            }
            for in_tmux in [false, true] {
                let mut output = Vec::new();
                let result = write_osc52(
                    &mut io::Cursor::new(text.as_bytes()),
                    &mut output,
                    in_tmux,
                    bytes,
                );
                if bytes > OSC52_MAX_BYTES {
                    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
                    assert!(output.is_empty());
                    continue;
                }
                result.unwrap();
                let encoded = STANDARD.encode(text.as_bytes());
                let plain = format!("\x1b]52;c;{encoded}\x07");
                let expected = if in_tmux {
                    format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\{plain}")
                } else {
                    plain
                };
                assert_eq!(output, expected.as_bytes());
            }
        }
    }

    #[test]
    fn osc52_rejects_changed_payload_without_completing_the_sequence() {
        for text in [b"ab".as_slice(), b"abcd"] {
            for in_tmux in [false, true] {
                let mut output = Vec::new();
                let error =
                    write_osc52(&mut io::Cursor::new(text), &mut output, in_tmux, 3).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(!output.ends_with(b"\x07"));
                assert!(!output.ends_with(b"\x07\x1b\\"));
                assert!(output.len() <= b"\x1bPtmux;\x1b\x1b]52;c;".len() + 4);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn oversized_export_reaches_desktop_read_only_without_any_osc_output() {
        use crate::tui::terminal_output::{install_test_output, tests::private_terminal};
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let _path = crate::session::test_support::EnvGuard::set(&[("PATH", bin.clone())]);
        for (command, _) in crate::process::clipboard_commands() {
            let path = bin.join(command);
            std::fs::write(
                &path,
                "#!/bin/sh\n/bin/cat > \"$0.copied\"\nif printf mutation >&0; then exit 91; fi\nexit 0\n",
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut payload = vec![b'x'; OSC52_MAX_BYTES as usize + 1];
        payload.extend_from_slice(b"\nDESKTOP-FULL-TAIL\0");
        let (mut master, slave) = private_terminal();
        let (_output_guard, _) = install_test_output(slave);
        for tmux in [false, true] {
            let _tmux = if tmux {
                crate::session::test_support::EnvGuard::set(&[(
                    "TMUX",
                    std::path::PathBuf::from("private-test"),
                )])
            } else {
                crate::session::test_support::EnvGuard::unset(&["TMUX"])
            };
            let mut export = tempfile::NamedTempFile::new_in(root.path()).unwrap();
            export.write_all(&payload).unwrap();
            let mut input = finalize_export(export).unwrap();
            assert!(input.write_all(b"must not mutate").is_err());
            let report =
                copy_file_with_timeout(input, Duration::from_secs(2), &CancellationToken::new())
                    .unwrap();
            assert!(matches!(report.osc52, Osc52Outcome::SkippedForSize));
            assert_eq!(report.bytes, payload.len() as u64);
            let helper = report.desktop.expect("full desktop copy completed");
            assert_eq!(
                std::fs::read(bin.join(format!("{helper}.copied"))).unwrap(),
                payload
            );
            let mut byte = [0];
            assert_eq!(
                master.read(&mut byte).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
        for (command, _) in crate::process::clipboard_commands() {
            std::fs::remove_file(bin.join(command)).unwrap();
        }
        let mut export = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        export.write_all(&payload).unwrap();
        let error = copy_file_with_timeout(
            finalize_export(export).unwrap(),
            Duration::from_secs(2),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn ordered_executor_coalesces_pending_copies_and_rejects_late_preparation() {
        let (entered, started) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&written);
        let executor = Executor::start(move |mut file, _, _| {
            file.rewind()?;
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            entered.send(text.clone()).unwrap();
            if text == "first" {
                gate.recv().unwrap();
            }
            output.lock().unwrap().push(text);
            Ok(CopyReport {
                bytes: file.metadata()?.len(),
                desktop: Some("controlled writer"),
                osc52: Osc52Outcome::Failed(io::ErrorKind::Unsupported.into()),
            })
        })
        .unwrap();
        let file = |text: &str| {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(text.as_bytes()).unwrap();
            file
        };
        let first = reserve_for(&executor.shared).unwrap();
        let first_done = first.submit(file("first"), Duration::from_secs(1)).unwrap();
        assert_eq!(
            started.recv_timeout(Duration::from_secs(1)).unwrap(),
            "first"
        );
        let second = reserve_for(&executor.shared).unwrap();
        let second_done = second
            .submit(file("second"), Duration::from_secs(1))
            .unwrap();
        let third = reserve_for(&executor.shared).unwrap();
        let third_done = third.submit(file("third"), Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        assert!(first_done.blocking_recv().unwrap().is_ok());
        assert!(third_done.blocking_recv().unwrap().is_ok());
        assert!(second_done.blocking_recv().is_err());
        assert_eq!(*written.lock().unwrap(), ["first", "third"]);

        let old_preparation = reserve_for(&executor.shared).unwrap();
        let newest = reserve_for(&executor.shared).unwrap();
        newest
            .submit(file("newest"), Duration::from_secs(1))
            .unwrap()
            .blocking_recv()
            .unwrap()
            .unwrap();
        assert!(old_preparation
            .submit(file("obsolete prepared content"), Duration::from_secs(1))
            .is_err());
        drop(executor);
        assert_eq!(*written.lock().unwrap(), ["first", "third", "newest"]);
    }
}

#[cfg(test)]
#[test]
#[serial_test::serial]
fn live_feedback_waits_for_actual_output_and_reports_failure() {
    let (entered, started) = std::sync::mpsc::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let executor = Executor::start(move |_, _, _| {
        entered.send(()).unwrap();
        gate.recv().unwrap();
        Err(io::Error::other("desktop and terminal output refused"))
    })
    .unwrap();
    executor.activate_for_test();
    let admission = copy_to_clipboard("must not claim copied");
    assert!(admission.starts_with("Copy queued"), "{admission}");
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        executor.feedback().is_none(),
        "admission is not transport completion"
    );
    release.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let message = loop {
        if let Some(message) = executor.feedback() {
            break message;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    };
    assert!(message.starts_with("Copy failed:"), "{message}");
    assert!(message.contains("desktop and terminal output refused"));
    drop(executor);
}
