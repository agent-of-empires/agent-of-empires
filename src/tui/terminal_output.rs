//! Serialize terminal output without letting a stalled clipboard block input.

use std::{
    cell::RefCell,
    fs::File,
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        LazyLock, Mutex, TryLockError,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

static OUTPUT: LazyLock<io::Result<Mutex<State>>> = LazyLock::new(|| {
    crate::process::independent_terminal_output().map(|file| Mutex::new(State::new(file)))
});
static REDRAW: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "e2e-tests")]
static ACK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

#[cfg(feature = "e2e-tests")]
pub(crate) fn acknowledge(sequence: u64) {
    ACK.store(sequence, Ordering::Release);
    REDRAW.store(true, Ordering::Release);
}
thread_local! { static FRAME: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) }; }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameId(u64);

#[derive(Default)]
pub(crate) struct FrameOutcome {
    pub captured: Option<FrameId>,
    pub presented: Option<FrameId>,
}

/// Unique semantic owners share receipt identity without allocating per frame.
pub(super) fn next_interaction_revision() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut current = NEXT.load(Ordering::Relaxed);
    loop {
        let revision = current
            .checked_add(1)
            .expect("interaction identity overflow");
        match NEXT.compare_exchange_weak(current, revision, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return revision,
            Err(observed) => current = observed,
        }
    }
}

/// Stage Ratatui's cursor cleanup before attempting bounded terminal output.
pub(crate) struct Terminal<B: ratatui::backend::Backend> {
    inner: Option<ratatui::Terminal<B>>,
}

impl<B: ratatui::backend::Backend> Terminal<B> {
    pub(crate) fn new(backend: B) -> Result<Self, B::Error> {
        ratatui::Terminal::new(backend).map(|inner| Self { inner: Some(inner) })
    }
}

impl<B: ratatui::backend::Backend> std::ops::Deref for Terminal<B> {
    type Target = ratatui::Terminal<B>;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().expect("terminal owned until drop")
    }
}

impl<B: ratatui::backend::Backend> std::ops::DerefMut for Terminal<B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.as_mut().expect("terminal owned until drop")
    }
}

impl<B: ratatui::backend::Backend> Drop for Terminal<B> {
    fn drop(&mut self) {
        let terminal = self.inner.take();
        if FRAME.with(|frame| frame.borrow().is_some()) {
            drop(terminal);
            return;
        }
        FRAME.with(|frame| *frame.borrow_mut() = Some(Vec::new()));
        drop(terminal);
        let buffer =
            FRAME.with(|frame| frame.borrow_mut().take().expect("cursor cleanup captured"));
        if !buffer.is_empty() {
            let mut output = Output { buffer };
            if let Err(error) = output.flush() {
                tracing::debug!(target: "tui.output", %error, "cursor cleanup did not complete");
            }
        }
    }
}

struct PendingFrame {
    id: FrameId,
    bytes: Vec<u8>,
    offset: usize,
    #[cfg(feature = "e2e-tests")]
    acknowledgement: u64,
}

struct PendingControl {
    bytes: Vec<u8>,
    offset: usize,
}

struct State {
    file: File,
    recovery: Option<(Vec<u8>, usize)>,
    suspended: bool,
    pending: Option<PendingFrame>,
    control: Option<PendingControl>,
    completed: Option<FrameId>,
    next_frame: u64,
}

fn state() -> io::Result<&'static Mutex<State>> {
    OUTPUT
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
}

struct DeadlineWriter<'a> {
    file: &'a mut File,
    deadline: Instant,
    cancel: Option<&'a CancellationToken>,
    written: usize,
}
impl Write for DeadlineWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut ready_without_progress = false;
        loop {
            if self.cancel.is_some_and(CancellationToken::is_cancelled) {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.file.write(bytes) {
                Ok(count) => {
                    self.written += count;
                    return Ok(count);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    #[cfg(test)]
                    observe_blocked_output();
                    if ready_without_progress {
                        std::thread::park_timeout(remaining.min(Duration::from_millis(1)));
                        ready_without_progress = false;
                        continue;
                    }
                    let wait = if self.cancel.is_some() {
                        remaining.min(Duration::from_millis(10))
                    } else {
                        remaining
                    };
                    match crate::process::terminal_writable(self.file, wait) {
                        Ok(()) => ready_without_progress = true,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error)
                            if self.cancel.is_some() && error.kind() == io::ErrorKind::TimedOut =>
                        {
                            continue
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        if Instant::now() >= self.deadline {
            Err(io::ErrorKind::TimedOut.into())
        } else {
            Ok(())
        }
    }
}
impl State {
    fn new(file: File) -> Self {
        Self {
            file,
            recovery: None,
            suspended: false,
            pending: None,
            control: None,
            completed: None,
            next_frame: 0,
        }
    }

    fn write_control(
        &mut self,
        deadline: Instant,
        cancel: Option<&CancellationToken>,
    ) -> io::Result<bool> {
        let Some(control) = self.control.as_mut() else {
            return Ok(true);
        };
        let mut writer = DeadlineWriter {
            file: &mut self.file,
            deadline,
            cancel,
            written: 0,
        };
        let result = writer
            .write_all(&control.bytes[control.offset..])
            .and_then(|_| writer.flush());
        control.offset += writer.written;
        match result {
            Ok(()) => {
                self.control = None;
                Ok(true)
            }
            Err(error) if is_backpressure(&error) => {
                REDRAW.store(true, Ordering::Release);
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn write_pending(
        &mut self,
        deadline: Instant,
        cancel: Option<&CancellationToken>,
    ) -> io::Result<bool> {
        let Some(pending) = self.pending.as_mut() else {
            return Ok(true);
        };
        let mut writer = DeadlineWriter {
            file: &mut self.file,
            deadline,
            cancel,
            written: 0,
        };
        let result = writer
            .write_all(&pending.bytes[pending.offset..])
            .and_then(|_| writer.flush());
        pending.offset += writer.written;
        match result {
            Ok(()) => {
                let pending = self.pending.take().expect("completed frame");
                self.completed = Some(pending.id);
                #[cfg(feature = "e2e-tests")]
                let _ = ACK.compare_exchange(
                    pending.acknowledgement,
                    u64::MAX,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                Ok(true)
            }
            Err(error) if is_backpressure(&error) => {
                REDRAW.store(true, Ordering::Release);
                Ok(false)
            }
            Err(error) => {
                let pending = self.pending.take().expect("failed frame");
                if pending.offset != 0 {
                    self.interrupted(std::env::var_os("TMUX").is_some(), deadline);
                }
                Err(error)
            }
        }
    }

    fn recover(&mut self, deadline: Instant, cancel: Option<&CancellationToken>) -> io::Result<()> {
        let Some((bytes, offset)) = self.recovery.as_mut() else {
            return Ok(());
        };
        let mut writer = DeadlineWriter {
            file: &mut self.file,
            deadline,
            cancel,
            written: 0,
        };
        let result = writer
            .write_all(&bytes[*offset..])
            .and_then(|_| writer.flush());
        *offset += writer.written;
        if result.is_ok() {
            self.recovery = None;
            REDRAW.store(true, Ordering::Release);
        }
        result
    }
    fn interrupted(&mut self, tmux: bool, deadline: Instant) {
        // CAN cancels the inner string; ST closes any outer tmux DCS.
        // A separately wrapped CAN reaches terminals behind tmux passthrough.
        let mut bytes = b"\x18\x1b\\".to_vec();
        if tmux {
            bytes.extend_from_slice(b"\x1bPtmux;\x18\x1b\\");
        }
        bytes.extend_from_slice(b"\x1b[?2026l\x1b]8;;\x1b\\\x1b[0m");
        self.recovery = Some((bytes, 0));
        REDRAW.store(true, Ordering::Release);
        let _ = self.recover(
            deadline.min(Instant::now() + Duration::from_millis(10)),
            None,
        );
    }
}

pub(crate) fn needs_redraw() -> bool {
    REDRAW.load(Ordering::Acquire)
}

pub(crate) fn request_redraw() {
    REDRAW.store(true, Ordering::Release);
}

pub(crate) struct Suspension {
    restore: Option<Vec<u8>>,
    #[cfg(test)]
    output: Option<std::sync::Arc<Mutex<State>>>,
}

fn is_backpressure(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn suspend_in(output: &Mutex<State>) -> io::Result<()> {
    let mut state = output.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
        TryLockError::Poisoned(error) => io::Error::other(error.to_string()),
    })?;
    if state.suspended {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let deadline = Instant::now() + Duration::from_millis(5);
    if let Err(error) = state.recover(deadline, None) {
        return Err(if is_backpressure(&error) {
            io::ErrorKind::WouldBlock.into()
        } else {
            error
        });
    }
    if !state.write_control(deadline, None)? {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    if !state.write_pending(deadline, None)? {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    state.suspended = true;
    Ok(())
}

pub(crate) fn suspend() -> io::Result<Suspension> {
    #[cfg(test)]
    if let Some(output) = TEST_OUTPUT.with(|output| output.borrow().clone()) {
        suspend_in(&output)?;
        return Ok(Suspension {
            restore: None,
            output: Some(output),
        });
    }
    suspend_in(state()?)?;
    Ok(Suspension {
        restore: None,
        #[cfg(test)]
        output: None,
    })
}

impl Suspension {
    fn output(&self) -> io::Result<&Mutex<State>> {
        #[cfg(test)]
        if let Some(output) = &self.output {
            return Ok(output);
        }
        state()
    }

    pub(crate) fn begin_handoff(&mut self) -> io::Result<bool> {
        let mut leave = Vec::new();
        let mut restore = Vec::new();
        #[cfg(unix)]
        crossterm::queue!(&mut leave, crossterm::event::PopKeyboardEnhancementFlags)?;
        crossterm::queue!(
            &mut leave,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Show
        )?;
        crossterm::queue!(
            &mut restore,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Hide
        )?;
        #[cfg(unix)]
        crossterm::queue!(
            &mut restore,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
        self.restore = Some(restore);
        let mut state = self
            .output()?
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?;
        debug_assert!(state.suspended && state.control.is_none());
        state.control = Some(PendingControl {
            bytes: leave,
            offset: 0,
        });
        state.write_control(Instant::now() + Duration::from_millis(20), None)
    }
}

impl Drop for Suspension {
    fn drop(&mut self) {
        let restore = self.restore.take();
        if let Ok(output) = self.output() {
            let mut state = output.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(bytes) = restore {
                if let Some(control) = state.control.as_mut() {
                    control.bytes.extend(bytes);
                } else {
                    state.control = Some(PendingControl { bytes, offset: 0 });
                }
            }
            state.suspended = false;
            REDRAW.store(true, Ordering::Release);
        }
    }
}

/// Finish output after restoring cooked input; drawing no longer owns the terminal.
pub(crate) fn restore_after_exit(
    disable_mouse: bool,
    title: Option<&str>,
    deadline: Instant,
) -> io::Result<bool> {
    let result = (|| {
        #[cfg(test)]
        if let Some(output) = TEST_OUTPUT.with(|output| output.borrow().clone()) {
            return restore_after_exit_in(&output, disable_mouse, title, deadline);
        }
        restore_after_exit_in(state()?, disable_mouse, title, deadline)
    })();
    match &result {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            target: "tui.output",
            "terminal display cleanup incomplete before bounded exit"
        ),
        Err(error) => tracing::warn!(
            target: "tui.output",
            %error,
            "terminal display cleanup failed"
        ),
    }
    result
}

fn restore_after_exit_in(
    output: &Mutex<State>,
    disable_mouse: bool,
    title: Option<&str>,
    deadline: Instant,
) -> io::Result<bool> {
    let mut bytes = Vec::new();
    #[cfg(unix)]
    crossterm::queue!(&mut bytes, crossterm::event::PopKeyboardEnhancementFlags)?;
    if disable_mouse {
        crossterm::queue!(&mut bytes, crossterm::event::DisableMouseCapture)?;
    }
    crossterm::queue!(
        &mut bytes,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableBracketedPaste,
        crossterm::cursor::Show
    )?;
    if let Some(title) = title {
        crossterm::queue!(&mut bytes, crossterm::terminal::SetTitle(title))?;
    }
    let (mut state, poisoned) = loop {
        match output.try_lock() {
            Ok(state) => break (state, false),
            Err(TryLockError::WouldBlock) => {
                #[cfg(test)]
                EXIT_CONTENDED.with(|observer| {
                    if let Some(observer) = observer.borrow_mut().take() {
                        let _ = observer.send(());
                    }
                });
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(false);
                }
                std::thread::park_timeout(remaining.min(Duration::from_millis(1)));
            }
            Err(TryLockError::Poisoned(error)) => break (error.into_inner(), true),
        }
    };
    FRAME.with(|frame| *frame.borrow_mut() = None);
    // Exit cancels obsolete drawing without acknowledging an unseen frame.
    let partial = state.pending.take().is_some_and(|frame| frame.offset != 0);
    if poisoned || partial {
        state.interrupted(std::env::var_os("TMUX").is_some(), deadline);
    }
    state.completed = None;
    if let Some(control) = state.control.as_mut() {
        control.bytes.extend(bytes);
    } else {
        state.control = Some(PendingControl { bytes, offset: 0 });
    }
    if let Err(error) = state.recover(deadline, None) {
        return if is_backpressure(&error) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    state.write_control(deadline, None)
}

/// Capture at most one frame; retain a partial write until its own receipt is emitted.
pub(crate) fn frame(
    operation: impl FnOnce(bool) -> anyhow::Result<()>,
) -> anyhow::Result<FrameOutcome> {
    #[cfg(test)]
    if let Some(output) = TEST_OUTPUT.with(|output| output.borrow().clone()) {
        return frame_in(&output, operation);
    }
    frame_in(state()?, operation)
}

fn frame_in(
    output: &Mutex<State>,
    operation: impl FnOnce(bool) -> anyhow::Result<()>,
) -> anyhow::Result<FrameOutcome> {
    let mut state = match output.try_lock() {
        Ok(state) => state,
        Err(TryLockError::WouldBlock) => {
            REDRAW.store(true, Ordering::Release);
            return Ok(FrameOutcome::default());
        }
        Err(TryLockError::Poisoned(error)) => {
            return Err(io::Error::other(error.to_string()).into())
        }
    };
    if state.suspended {
        return Ok(FrameOutcome::default());
    }
    if let Err(error) = state.recover(Instant::now() + Duration::from_millis(5), None) {
        if is_backpressure(&error) {
            return Ok(FrameOutcome::default());
        }
        return Err(error.into());
    }
    if !state.write_control(Instant::now() + Duration::from_millis(20), None)? {
        return Ok(FrameOutcome::default());
    }
    if state.pending.is_some() || state.completed.is_some() {
        state.write_pending(Instant::now() + Duration::from_millis(20), None)?;
        let presented = state.completed.take();
        REDRAW.store(true, Ordering::Release);
        return Ok(FrameOutcome {
            captured: None,
            presented,
        });
    }
    let redraw = REDRAW.swap(false, Ordering::AcqRel);
    FRAME.with(|frame| *frame.borrow_mut() = Some(Vec::new()));
    let result = operation(redraw);
    let bytes = FRAME.with(|frame| frame.borrow_mut().take().expect("frame capture installed"));
    result?;
    state.next_frame = state
        .next_frame
        .checked_add(1)
        .ok_or_else(|| io::Error::other("terminal frame identity overflow"))?;
    let id = FrameId(state.next_frame);
    #[cfg(feature = "e2e-tests")]
    let mut bytes = bytes;
    #[cfg(feature = "e2e-tests")]
    let acknowledgement = ACK.load(Ordering::Acquire);
    #[cfg(feature = "e2e-tests")]
    if acknowledgement != u64::MAX {
        write!(&mut bytes, "\x1b]0;aoe-e2e-{acknowledgement}\x07")?;
    }
    state.pending = Some(PendingFrame {
        id,
        bytes,
        offset: 0,
        #[cfg(feature = "e2e-tests")]
        acknowledgement,
    });
    state.write_pending(Instant::now() + Duration::from_millis(20), None)?;
    Ok(FrameOutcome {
        captured: Some(id),
        presented: state.completed.take(),
    })
}

pub(crate) fn osc52(
    timeout: Duration,
    tmux: bool,
    cancel: &CancellationToken,
    operation: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    #[cfg(test)]
    if let Some(output) = TEST_OUTPUT.with(|output| output.borrow().clone()) {
        return osc52_in(&output, timeout, tmux, cancel, operation);
    }
    osc52_in(state()?, timeout, tmux, cancel, operation)
}

fn osc52_in(
    output: &Mutex<State>,
    timeout: Duration,
    tmux: bool,
    cancel: &CancellationToken,
    operation: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut state = loop {
        if cancel.is_cancelled() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match output.try_lock() {
            Ok(state) => break state,
            Err(TryLockError::Poisoned(error)) => return Err(io::Error::other(error.to_string())),
            Err(TryLockError::WouldBlock) => {
                #[cfg(test)]
                observe_blocked_output();
                std::thread::park_timeout(remaining.min(Duration::from_millis(1)));
            }
        }
    };
    if state.suspended {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "terminal belongs to an attached child",
        ));
    }
    state.recover(deadline, Some(cancel))?;
    if !state.write_control(deadline, Some(cancel))? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal restoration is still pending",
        ));
    }
    if !state.write_pending(deadline, Some(cancel))? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal frame is still pending",
        ));
    }
    let mut writer = DeadlineWriter {
        file: &mut state.file,
        deadline,
        cancel: Some(cancel),
        written: 0,
    };
    let result = operation(&mut writer).and_then(|_| writer.flush());
    if result.is_err() && writer.written != 0 {
        state.interrupted(tmux, deadline);
    }
    result
}

#[derive(Default)]
pub struct Output {
    buffer: Vec<u8>,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let captured = FRAME.with(|frame| {
            if let Some(frame) = frame.borrow_mut().as_mut() {
                frame.append(&mut self.buffer);
                true
            } else {
                false
            }
        });
        if captured {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(output) = TEST_OUTPUT.with(|output| output.borrow().clone()) {
            return self.flush_in(&output);
        }
        self.flush_in(state()?)
    }
}

impl Output {
    fn flush_in(&mut self, output: &Mutex<State>) -> io::Result<()> {
        let mut state = output
            .try_lock()
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        let deadline = Instant::now() + Duration::from_millis(20);
        state.recover(deadline, None)?;
        if !state.write_control(deadline, None)? {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if !state.write_pending(deadline, None)? {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let bytes = std::mem::take(&mut self.buffer);
        let mut writer = DeadlineWriter {
            file: &mut state.file,
            deadline,
            cancel: None,
            written: 0,
        };
        let result = writer.write_all(&bytes).and_then(|_| writer.flush());
        if result.is_err() {
            if writer.written == 0 {
                self.buffer = bytes;
            } else {
                state.interrupted(std::env::var_os("TMUX").is_some(), deadline);
            }
        }
        result
    }
}

#[cfg(test)]
thread_local! {
    static TEST_OUTPUT: RefCell<Option<std::sync::Arc<Mutex<State>>>> = const { RefCell::new(None) };
    static EXIT_CONTENDED: RefCell<Option<std::sync::mpsc::Sender<()>>> = const { RefCell::new(None) };
    static BLOCKED_OUTPUT: RefCell<Option<std::sync::mpsc::Sender<()>>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn observe_blocked_output() {
    BLOCKED_OUTPUT.with(|observer| {
        if let Some(observer) = observer.borrow_mut().take() {
            let _ = observer.send(());
        }
    });
}

#[cfg(test)]
pub(crate) struct TestOutputGuard;
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestOutput(std::sync::Arc<Mutex<State>>);

#[cfg(test)]
pub(crate) fn install_test_output(file: File) -> (TestOutputGuard, TestOutput) {
    let output = std::sync::Arc::new(Mutex::new(State::new(file)));
    TEST_OUTPUT.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(std::sync::Arc::clone(&output));
    });
    (TestOutputGuard, TestOutput(output))
}

#[cfg(test)]
impl TestOutput {
    pub(crate) fn osc52(
        &self,
        timeout: Duration,
        cancel: &CancellationToken,
        operation: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        osc52_in(&self.0, timeout, false, cancel, operation)
    }
}

#[cfg(test)]
impl Drop for TestOutputGuard {
    fn drop(&mut self) {
        TEST_OUTPUT.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
#[path = "terminal_output_tests.rs"]
pub(crate) mod tests;
