use std::{collections::VecDeque, io, time::Duration};
#[cfg(feature = "event-stream")]
use std::{
    io::Read,
    os::{fd::AsRawFd, unix::net::UnixStream},
};

use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use signal_hook_mio::v1_0::Signals;

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{
    source::EventSource, sys::unix::parse::parse_event, timeout::PollTimeout, Event, InternalEvent,
};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

// Tokens to identify file descriptor
const TTY_TOKEN: Token = Token(0);
const SIGNAL_TOKEN: Token = Token(1);
#[cfg(feature = "event-stream")]
const WAKE_TOKEN: Token = Token(2);

// I (@zrzka) wasn't able to read more than 1_022 bytes when testing
// reading on macOS/Linux -> we don't need bigger buffer and 1k of bytes
// is enough.
const TTY_BUFFER_SIZE: usize = 1_024;

#[cfg(all(
    test,
    feature = "event-stream",
    not(any(feature = "libc", feature = "use-dev-tty"))
))]
#[derive(Clone, Copy, Debug)]
enum ForcedOrder {
    WakeFirst,
    TtyFirst,
    SignalWakeTty,
    WakeSignalTty,
    TtySignalWake,
    SignalTtyWake,
}

#[cfg(all(
    test,
    feature = "event-stream",
    not(any(feature = "libc", feature = "use-dev-tty"))
))]
#[derive(Debug, Default)]
struct ConsumerProbe {
    next_order: Option<ForcedOrder>,
    raw_batches: Vec<Vec<Token>>,
    dispatched_batches: Vec<Vec<Token>>,
    reads: Vec<Result<usize, io::ErrorKind>>,
    current_batch: Vec<Token>,
    read_attempts: usize,
    poll_attempts: usize,
    poll_timeouts: Vec<Option<Duration>>,
    refresh_attempts: usize,
    refresh_results: Vec<Result<(), io::ErrorKind>>,
    refresh_error_once: Option<io::ErrorKind>,
    poll_error_once: Option<io::ErrorKind>,
    size_error_once: Option<io::ErrorKind>,
    size_queries: usize,
}

pub(crate) struct UnixInternalEventSource {
    poll: Poll,
    events: Events,
    next_event: usize,
    #[cfg(all(test, target_os = "linux", feature = "event-stream"))]
    first_test_token: Option<Token>,
    tty_needs_refresh: bool,
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty_fd: FileDesc<'static>,
    signals: Signals,
    #[cfg(feature = "event-stream")]
    waker: Waker,
    #[cfg(feature = "event-stream")]
    wake_receiver: UnixStream,
    #[cfg(all(
        test,
        feature = "event-stream",
        not(any(feature = "libc", feature = "use-dev-tty"))
    ))]
    probe: ConsumerProbe,
}

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        UnixInternalEventSource::from_file_descriptor(tty_fd()?)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc<'static>) -> io::Result<Self> {
        let poll = Poll::new()?;
        let registry = poll.registry();

        let tty_raw_fd = input_fd.raw_fd();
        let mut tty_ev = SourceFd(&tty_raw_fd);
        registry.register(&mut tty_ev, TTY_TOKEN, Interest::READABLE)?;

        let mut signals = Signals::new([signal_hook::consts::SIGWINCH])?;
        registry.register(&mut signals, SIGNAL_TOKEN, Interest::READABLE)?;

        #[cfg(feature = "event-stream")]
        let (waker, wake_receiver) = Waker::new(registry, WAKE_TOKEN)?;

        Ok(UnixInternalEventSource {
            poll,
            events: Events::with_capacity(3),
            next_event: 0,
            #[cfg(all(test, target_os = "linux", feature = "event-stream"))]
            first_test_token: None,
            tty_needs_refresh: false,
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty_fd: input_fd,
            signals,
            #[cfg(feature = "event-stream")]
            waker,
            #[cfg(feature = "event-stream")]
            wake_receiver,
            #[cfg(all(
                test,
                feature = "event-stream",
                not(any(feature = "libc", feature = "use-dev-tty"))
            ))]
            probe: ConsumerProbe::default(),
        })
    }
}

impl EventSource for UnixInternalEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        if let Some(event) = self.parser.next() {
            return Ok(Some(event));
        }

        let immediate = timeout == Some(Duration::ZERO);
        #[cfg(feature = "event-stream")]
        let wakeable = timeout.is_none();
        let timeout = PollTimeout::new(timeout);
        let mut fresh_batch = false;
        let mut retried_at_deadline = false;

        loop {
            let mut read_input = false;
            let mut retry_read = false;
            loop {
                let mut events = self.events.iter();
                let index = self.next_event;
                #[cfg(all(test, target_os = "linux", feature = "event-stream"))]
                let index = match self.first_test_token {
                    Some(token)
                        if index < 2
                            && self.events.iter().count() == 2
                            && self.events.iter().nth(1).unwrap().token() == token =>
                    {
                        index ^ 1
                    }
                    _ => index,
                };
                #[cfg(all(
                    test,
                    feature = "event-stream",
                    not(any(feature = "libc", feature = "use-dev-tty"))
                ))]
                let index = match self.probe.current_batch.get(index) {
                    Some(token) => self
                        .events
                        .iter()
                        .position(|event| event.token() == *token)
                        .unwrap(),
                    None => index,
                };
                let Some(event) = events.nth(index) else {
                    break;
                };
                let token = event.token();
                self.next_event += 1;
                match token {
                    TTY_TOKEN => {
                        // A saved readiness notification cannot authorize a blocking read.
                        if !fresh_batch {
                            continue;
                        }
                        #[cfg(all(
                            test,
                            feature = "event-stream",
                            not(any(feature = "libc", feature = "use-dev-tty"))
                        ))]
                        {
                            self.probe.read_attempts += 1;
                            eprintln!("TTY READ ATTEMPT {}", self.probe.read_attempts);
                        }
                        let read_result = self.tty_fd.read(&mut self.tty_buffer);
                        #[cfg(all(
                            test,
                            feature = "event-stream",
                            not(any(feature = "libc", feature = "use-dev-tty"))
                        ))]
                        self.probe.reads.push(match &read_result {
                            Ok(count) => Ok(*count),
                            Err(error) => Err(error.kind()),
                        });
                        match read_result {
                            Ok(0) if event.is_read_closed() || event.is_error() => {
                                return Err(io::ErrorKind::UnexpectedEof.into());
                            }
                            Ok(0) => read_input = true,
                            Ok(count) => {
                                read_input = true;
                                self.parser
                                    .advance(&self.tty_buffer[..count], count == TTY_BUFFER_SIZE);
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                let elapsed = timeout.elapsed();
                                retry_read = !elapsed || !retried_at_deadline;
                                retried_at_deadline |= elapsed;
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                            Err(error) => return Err(error),
                        }
                        if let Some(event) = self.parser.next() {
                            return Ok(Some(event));
                        }
                    }
                    SIGNAL_TOKEN => {
                        if self.signals.pending().next() == Some(signal_hook::consts::SIGWINCH) {
                            #[cfg(all(
                                test,
                                feature = "event-stream",
                                not(any(feature = "libc", feature = "use-dev-tty"))
                            ))]
                            let new_size = {
                                self.probe.size_queries += 1;
                                match self.probe.size_error_once.take() {
                                    Some(kind) => {
                                        Err(io::Error::new(kind, "private size-query fault"))
                                    }
                                    None => crate::terminal::size(),
                                }
                            }?;
                            #[cfg(not(all(
                                test,
                                feature = "event-stream",
                                not(any(feature = "libc", feature = "use-dev-tty"))
                            )))]
                            let new_size = crate::terminal::size()?;
                            return Ok(Some(InternalEvent::Event(Event::Resize(
                                new_size.0, new_size.1,
                            ))));
                        }
                    }
                    #[cfg(feature = "event-stream")]
                    WAKE_TOKEN => {
                        match self.wake_receiver.read(&mut [0; 1024]) {
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                                ) => {}
                            Err(error) => return Err(error),
                        }
                        self.poll.registry().reregister(
                            &mut SourceFd(&self.wake_receiver.as_raw_fd()),
                            WAKE_TOKEN,
                            Interest::READABLE,
                        )?;
                        if wakeable {
                            return Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "Poll operation was woken up by `Waker::wake`",
                            ));
                        }
                    }
                    _ => unreachable!("Synchronize Evented handle registration & token handling"),
                }
            }

            if let Some(event) = self.parser.next() {
                return Ok(Some(event));
            }
            if fresh_batch && timeout.elapsed() && !(retry_read || immediate && read_input) {
                return Ok(None);
            }

            // Recheck skipped or partially consumed TTY readiness without waiting for a new edge.
            if self.tty_needs_refresh {
                let registry = self.poll.registry();
                let fd = self.tty_fd.raw_fd();
                let mut source = SourceFd(&fd);
                #[cfg(all(
                    test,
                    feature = "event-stream",
                    not(any(feature = "libc", feature = "use-dev-tty"))
                ))]
                let result = {
                    self.probe.refresh_attempts += 1;
                    eprintln!("TTY REFRESH ATTEMPT {}", self.probe.refresh_attempts);
                    match self.probe.refresh_error_once.take() {
                        Some(kind) => Err(io::Error::new(kind, "private reregister fault")),
                        None => {
                            registry.reregister(&mut SourceFd(&fd), TTY_TOKEN, Interest::READABLE)
                        }
                    }
                };
                #[cfg(not(all(
                    test,
                    feature = "event-stream",
                    not(any(feature = "libc", feature = "use-dev-tty"))
                )))]
                let result = registry.reregister(&mut SourceFd(&fd), TTY_TOKEN, Interest::READABLE);
                #[cfg(all(
                    test,
                    feature = "event-stream",
                    not(any(feature = "libc", feature = "use-dev-tty"))
                ))]
                self.probe
                    .refresh_results
                    .push(result.as_ref().map(|_| ()).map_err(|error| error.kind()));
                match result {
                    // Mio's poll(2) backend removes the registration on hangup.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        registry.register(&mut source, TTY_TOKEN, Interest::READABLE)?;
                    }
                    result => result?,
                }
                self.tty_needs_refresh = false;
            }
            self.events.clear();
            self.next_event = 0;
            fresh_batch = false;
            #[cfg(all(
                test,
                feature = "event-stream",
                not(any(feature = "libc", feature = "use-dev-tty"))
            ))]
            self.probe.current_batch.clear();
            let remaining = timeout.leftover();
            #[cfg(all(
                test,
                feature = "event-stream",
                not(any(feature = "libc", feature = "use-dev-tty"))
            ))]
            let poll_result = {
                self.probe.poll_attempts += 1;
                self.probe.poll_timeouts.push(remaining);
                match self.probe.poll_error_once.take() {
                    Some(kind) => Err(io::Error::new(kind, "private Poll fault")),
                    None => self.poll.poll(&mut self.events, remaining),
                }
            };
            #[cfg(not(all(
                test,
                feature = "event-stream",
                not(any(feature = "libc", feature = "use-dev-tty"))
            )))]
            let poll_result = self.poll.poll(&mut self.events, remaining);
            match poll_result {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    self.events.clear();
                    if timeout.elapsed() {
                        if retried_at_deadline {
                            return Ok(None);
                        }
                        retried_at_deadline = true;
                    }
                    continue;
                }
                Err(error) => {
                    self.events.clear();
                    return Err(error);
                }
            }
            #[cfg(all(
                test,
                feature = "event-stream",
                not(any(feature = "libc", feature = "use-dev-tty"))
            ))]
            {
                let raw: Vec<_> = self.events.iter().map(|event| event.token()).collect();
                self.probe.raw_batches.push(raw.clone());
                let mut dispatched = raw;
                if let Some(order) = self.probe.next_order.take() {
                    eprintln!("RAW CO-BATCH PRECONDITION: order={order:?}, tokens={dispatched:?}");
                    assert!(
                        dispatched.contains(&TTY_TOKEN) && dispatched.contains(&WAKE_TOKEN),
                        "PRECONDITION FAILURE: actual TTY+WAKE co-batch required: {dispatched:?}"
                    );
                    let ordering = match order {
                        ForcedOrder::WakeFirst => [WAKE_TOKEN, TTY_TOKEN, SIGNAL_TOKEN],
                        ForcedOrder::TtyFirst => [TTY_TOKEN, WAKE_TOKEN, SIGNAL_TOKEN],
                        ForcedOrder::SignalWakeTty => [SIGNAL_TOKEN, WAKE_TOKEN, TTY_TOKEN],
                        ForcedOrder::WakeSignalTty => [WAKE_TOKEN, SIGNAL_TOKEN, TTY_TOKEN],
                        ForcedOrder::TtySignalWake => [TTY_TOKEN, SIGNAL_TOKEN, WAKE_TOKEN],
                        ForcedOrder::SignalTtyWake => [SIGNAL_TOKEN, TTY_TOKEN, WAKE_TOKEN],
                    };
                    if !matches!(order, ForcedOrder::WakeFirst | ForcedOrder::TtyFirst) {
                        assert!(
                            dispatched.contains(&SIGNAL_TOKEN),
                            "PRECONDITION FAILURE: actual three-source batch required: {dispatched:?}"
                        );
                    }
                    dispatched.sort_by_key(|token| {
                        ordering
                            .iter()
                            .position(|candidate| candidate == token)
                            .unwrap()
                    });
                }
                self.probe.dispatched_batches.push(dispatched.clone());
                self.probe.current_batch = dispatched;
            }
            if self.events.is_empty() {
                return Ok(None);
            }
            fresh_batch = true;
            self.tty_needs_refresh = self.events.iter().any(|event| event.token() == TTY_TOKEN);
        }
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.waker.clone()
    }
}

//
// Following `Parser` structure exists for two reasons:
//
//  * mimic anes Parser interface
//  * move the advancing, parsing, ... stuff out of the `try_read` method
//
#[derive(Debug)]
struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            // This buffer is used for -> 1 <- ANSI escape sequence. Are we
            // aware of any ANSI escape sequence that is bigger? Can we make
            // it smaller?
            //
            // Probably not worth spending more time on this as "there's a plan"
            // to use the anes crate parser.
            buffer: Vec::with_capacity(256),
            // TTY_BUFFER_SIZE is 1_024 bytes. How many ANSI escape sequences can
            // fit? What is an average sequence length? Let's guess here
            // and say that the average ANSI escape sequence length is 8 bytes. Thus
            // the buffer size should be 1024/8=128 to avoid additional allocations
            // when processing large amounts of data.
            //
            // There's no need to make it bigger, because when you look at the `try_read`
            // method implementation, all events are consumed before the next TTY_BUFFER
            // is processed -> events pushed.
            internal_events: VecDeque::with_capacity(128),
        }
    }
}

impl Parser {
    fn advance(&mut self, buffer: &[u8], more: bool) {
        for (idx, byte) in buffer.iter().enumerate() {
            let more = idx + 1 < buffer.len() || more;

            self.buffer.push(*byte);

            match parse_event(&self.buffer, more) {
                Ok(Some(ie)) => {
                    self.internal_events.push_back(ie);
                    self.buffer.clear();
                }
                Ok(None) => {
                    // Event can't be parsed, because we don't have enough bytes for
                    // the current sequence. Keep the buffer and process next bytes.
                }
                Err(_) => {
                    // Event can't be parsed (not enough parameters, parameter is not a number,
                    // ...). Clear the buffer and continue with another
                    // sequence.
                    self.buffer.clear();
                }
            }
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal_events.pop_front()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{io::Write, os::unix::net::UnixStream};

    use super::*;
    use crate::event::{KeyCode, KeyEvent, KeyModifiers};

    #[cfg(feature = "event-stream")]
    #[test]
    fn co_ready_wake_and_input_are_both_delivered() {
        if !run_with_watchdog(concat!(
            module_path!(),
            "::co_ready_wake_and_input_are_both_delivered"
        )) {
            return;
        }

        for first in [WAKE_TOKEN, TTY_TOKEN] {
            check_co_ready_order(first);
        }
    }

    #[cfg(feature = "event-stream")]
    fn check_co_ready_order(first_token: Token) {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let mut source =
            UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(receiver.into()))
                .unwrap();
        source.first_test_token = Some(first_token);
        source.waker().wake().unwrap();
        sender.write_all(b"x").unwrap();

        let key = InternalEvent::Event(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        let first = source.try_read(None);
        assert!(source
            .events
            .iter()
            .any(|event| event.token() == WAKE_TOKEN));
        assert!(source.events.iter().any(|event| event.token() == TTY_TOKEN));
        if first_token == WAKE_TOKEN {
            assert_eq!(first.unwrap_err().kind(), io::ErrorKind::Interrupted);
            assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), Some(key));
        } else {
            assert_eq!(first.unwrap(), Some(key));
            let wake = source.try_read(None).unwrap_err();
            assert_eq!(wake.kind(), io::ErrorKind::Interrupted);
        }
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    }

    #[cfg(feature = "event-stream")]
    #[test]
    fn decoded_cursor_reply_precedes_saved_control_readiness() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let mut source =
            UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(receiver.into()))
                .unwrap();
        source.waker().wake().unwrap();
        source
            .poll
            .poll(&mut source.events, Some(Duration::ZERO))
            .unwrap();
        assert!(source
            .events
            .iter()
            .any(|event| event.token() == WAKE_TOKEN));
        source.parser.advance(b"\x1b[3;4R", false);
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            Some(InternalEvent::CursorPosition(3, 2)),
        );
        sender.write_all(b"x").unwrap();
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))),
        );
    }

    #[cfg(feature = "bracketed-paste")]
    #[test]
    fn canonical_pty_distinguishes_eof_and_reads_short_paste_chunks() {
        use std::{ffi::OsStr, fs::File, os::unix::ffi::OsStrExt, path::Path};

        use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};

        let master = openpt(OpenptFlags::RDWR).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = File::open(Path::new(OsStr::from_bytes(name.to_bytes()))).unwrap();
        rustix::io::ioctl_fionbio(&slave, true).unwrap();
        let mut master = File::from(master);
        let mut source =
            UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(slave.into())).unwrap();

        let paste = b"\x1b[200~x\ny\x1b[201~\n";
        master.write_all(b"\x04").unwrap();
        master.write_all(paste).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while rustix::io::ioctl_fionread(&source.tty_fd).unwrap() < paste.len() as u64 {
            assert!(std::time::Instant::now() < deadline, "paste was not queued");
            std::thread::yield_now();
        }
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            Some(InternalEvent::Event(Event::Paste("x\ny".into()))),
        );
    }

    #[test]
    fn incomplete_input_returns_to_poll_and_keeps_the_prefix() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut source =
            UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(receiver.into()))
                .unwrap();
        sender.write_all(b"\x1b[").unwrap();

        let start = std::time::Instant::now();
        assert_eq!(
            source.try_read(Some(Duration::from_millis(20))).unwrap(),
            None
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "read blocked after a partial CSI"
        );

        sender.write_all(b"A").unwrap();
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )))),
        );
    }

    #[test]
    fn hangup_does_not_hide_the_tail_of_a_full_read() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let mut source =
            UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(receiver.into()))
                .unwrap();
        let mut input = [b'x'; TTY_BUFFER_SIZE + 1];
        input[TTY_BUFFER_SIZE] = b'y';
        sender.write_all(&input).unwrap();
        drop(sender);

        for expected in input {
            assert_eq!(
                source.try_read(Some(Duration::ZERO)).unwrap(),
                Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char(char::from(expected)),
                    KeyModifiers::NONE,
                )))),
            );
        }
        let eof = source.try_read(Some(Duration::ZERO)).unwrap_err();
        assert_eq!(eof.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[cfg(feature = "event-stream")]
    #[test]
    fn wake_survives_refresh_of_drained_input() {
        if !run_with_watchdog(concat!(
            module_path!(),
            "::wake_survives_refresh_of_drained_input"
        )) {
            return;
        }

        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let mut source =
            UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(receiver.into()))
                .unwrap();
        sender.write_all(b"x").unwrap();
        assert!(source.try_read(Some(Duration::ZERO)).unwrap().is_some());
        source.waker().wake().unwrap();
        let error = source.try_read(None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[cfg(feature = "event-stream")]
    fn run_with_watchdog(test: &str) -> bool {
        const CHILD: &str = "CROSSTERM_TEST_MIO_WAKE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test.split_once("::").unwrap().1])
                .env(CHILD, "1")
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "wake test child failed: {status}");
                    return false;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    child.wait().unwrap();
                    panic!("wake did not interrupt the blocking read");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        true
    }

    #[cfg(feature = "bracketed-paste")]
    #[test]
    fn zero_timeout_reads_a_complete_queued_paste() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let mut source =
            UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(receiver.into()))
                .unwrap();
        let text = "x".repeat(TTY_BUFFER_SIZE);
        write!(sender, "\x1b[200~{text}\x1b[201~").unwrap();
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            Some(InternalEvent::Event(Event::Paste(text))),
        );
    }
}

#[cfg(all(
    test,
    feature = "event-stream",
    not(any(feature = "libc", feature = "use-dev-tty"))
))]
mod forced_batch_tests {
    use super::*;
    use crate::event::{KeyCode, KeyEvent, KeyModifiers};
    use libc as probe_libc;
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        mem::MaybeUninit,
        os::{
            fd::OwnedFd,
            unix::{
                io::{AsRawFd, FromRawFd},
                net::UnixStream,
            },
        },
        process::{Child, Command, Stdio},
        thread,
        time::Instant,
    };

    const POLL_TIMEOUT: Duration = Duration::from_millis(250);

    struct PrivateInput {
        writer: File,
        alias: File,
        source: UnixInternalEventSource,
    }

    impl PrivateInput {
        fn new() -> Self {
            Self::with_mode(false, false)
        }

        fn with_mode(blocking: bool, controlling: bool) -> Self {
            let mut master_fd = -1;
            let mut slave_fd = -1;
            let opened = unsafe {
                probe_libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(opened, 0, "openpty: {}", io::Error::last_os_error());
            // Own both descriptors immediately; neither replaces stdin nor becomes a ctty.
            let writer = unsafe { File::from_raw_fd(master_fd) };
            let slave = unsafe { File::from_raw_fd(slave_fd) };
            let mut settings = MaybeUninit::<probe_libc::termios>::uninit();
            assert_eq!(
                unsafe { probe_libc::tcgetattr(slave.as_raw_fd(), settings.as_mut_ptr()) },
                0,
                "private tcgetattr: {}",
                io::Error::last_os_error()
            );
            let mut settings = unsafe { settings.assume_init() };
            unsafe { probe_libc::cfmakeraw(&mut settings) };
            settings.c_cc[probe_libc::VMIN] = 1;
            settings.c_cc[probe_libc::VTIME] = 0;
            assert_eq!(
                unsafe { probe_libc::tcsetattr(slave.as_raw_fd(), probe_libc::TCSANOW, &settings) },
                0,
                "private tcsetattr: {}",
                io::Error::last_os_error()
            );
            let flags = unsafe { probe_libc::fcntl(slave.as_raw_fd(), probe_libc::F_GETFL) };
            assert!(
                flags >= 0,
                "private F_GETFL: {}",
                io::Error::last_os_error()
            );
            assert_eq!(
                unsafe {
                    probe_libc::fcntl(
                        slave.as_raw_fd(),
                        probe_libc::F_SETFL,
                        if blocking {
                            flags & !probe_libc::O_NONBLOCK
                        } else {
                            flags | probe_libc::O_NONBLOCK
                        },
                    )
                },
                0,
                "private F_SETFL: {}",
                io::Error::last_os_error()
            );
            if controlling {
                // This owned exec must survive its own controlling-PTY cleanup.
                assert_ne!(
                    unsafe { probe_libc::signal(probe_libc::SIGHUP, probe_libc::SIG_IGN) },
                    probe_libc::SIG_ERR,
                    "private child SIGHUP disposition"
                );
                assert!(unsafe { probe_libc::setsid() } > 0, "private setsid");
                assert_eq!(
                    unsafe { probe_libc::ioctl(slave.as_raw_fd(), probe_libc::TIOCSCTTY as _, 0) },
                    0,
                    "private TIOCSCTTY: {}",
                    io::Error::last_os_error()
                );
                let geometry = probe_libc::winsize {
                    ws_row: 37,
                    ws_col: 91,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                assert_eq!(
                    unsafe {
                        probe_libc::ioctl(slave.as_raw_fd(), probe_libc::TIOCSWINSZ, &geometry)
                    },
                    0,
                    "private geometry"
                );
            }
            let alias = slave
                .try_clone()
                .expect("private legitimate-consumer alias");
            let source =
                UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(slave.into()))
                    .expect("register actual private PTY with actual Mio Poll");
            let fixture = Self {
                writer,
                alias,
                source,
            };
            let actual_flags =
                unsafe { probe_libc::fcntl(fixture.source.tty_fd.raw_fd(), probe_libc::F_GETFL) };
            assert!(actual_flags >= 0);
            assert_eq!(actual_flags & probe_libc::O_NONBLOCK == 0, blocking);
            let mut actual = MaybeUninit::<probe_libc::termios>::uninit();
            assert_eq!(
                unsafe {
                    probe_libc::tcgetattr(fixture.source.tty_fd.raw_fd(), actual.as_mut_ptr())
                },
                0
            );
            let actual = unsafe { actual.assume_init() };
            assert_eq!(
                actual.c_lflag & (probe_libc::ICANON | probe_libc::ECHO | probe_libc::ISIG),
                0
            );
            assert_eq!(actual.c_cc[probe_libc::VMIN], 1);
            assert_eq!(actual.c_cc[probe_libc::VTIME], 0);
            eprintln!(
                "ACTUAL PRIVATE PTY MODE: blocking={blocking}, flags={actual_flags}, VMIN={}, VTIME={}",
                actual.c_cc[probe_libc::VMIN], actual.c_cc[probe_libc::VTIME]
            );
            if controlling {
                assert_eq!(
                    crate::terminal::size().expect("private controlling geometry"),
                    (91, 37)
                );
            }
            fixture
        }

        fn with_socketpair() -> Self {
            let (writer, receiver) = UnixStream::pair().expect("real private stream socketpair");
            writer
                .set_nonblocking(false)
                .expect("blocking socket writer");
            receiver
                .set_nonblocking(false)
                .expect("blocking socket receiver");
            let flags = unsafe { probe_libc::fcntl(receiver.as_raw_fd(), probe_libc::F_GETFL) };
            assert!(
                flags >= 0,
                "socket receiver F_GETFL: {}",
                io::Error::last_os_error()
            );
            assert_eq!(flags & probe_libc::O_NONBLOCK, 0);
            let alias = receiver
                .try_clone()
                .expect("duplicate actual receiving socket");
            let writer = File::from(OwnedFd::from(writer));
            let alias = File::from(OwnedFd::from(alias));
            let source =
                UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(receiver.into()))
                    .expect("register actual socket input with actual Mio Poll");
            let actual_flags =
                unsafe { probe_libc::fcntl(source.tty_fd.raw_fd(), probe_libc::F_GETFL) };
            assert!(
                actual_flags >= 0,
                "registered socket F_GETFL: {}",
                io::Error::last_os_error()
            );
            assert_eq!(actual_flags & probe_libc::O_NONBLOCK, 0);
            let alias_flags = unsafe { probe_libc::fcntl(alias.as_raw_fd(), probe_libc::F_GETFL) };
            assert!(alias_flags >= 0);
            assert_eq!(alias_flags & probe_libc::O_NONBLOCK, 0);
            eprintln!("ACTUAL PRIVATE SOCKETPAIR MODE: blocking=true, flags={actual_flags}");
            Self {
                writer,
                alias,
                source,
            }
        }

        fn pending(&self) -> usize {
            let mut count: probe_libc::c_int = -1;
            assert_eq!(
                unsafe {
                    probe_libc::ioctl(
                        self.source.tty_fd.raw_fd(),
                        probe_libc::FIONREAD,
                        &mut count,
                    )
                },
                0,
                "private FIONREAD: {}",
                io::Error::last_os_error()
            );
            usize::try_from(count).expect("FIONREAD must be nonnegative")
        }

        fn prequeue_key_and_wake(&mut self, order: ForcedOrder) {
            self.prequeue_and_wake(b"x", order, false);
        }

        fn prequeue_and_wake(&mut self, bytes: &[u8], order: ForcedOrder, signal: bool) {
            let written = self.writer.write(bytes).expect("one actual input write");
            assert_eq!(
                written,
                bytes.len(),
                "PRECONDITION FAILURE: fixed input must be queued by one complete write"
            );
            self.wait_pending(bytes.len());
            self.source.probe.next_order = Some(order);
            if signal {
                assert_eq!(unsafe { probe_libc::raise(probe_libc::SIGWINCH) }, 0);
            }
            self.source.waker().wake().expect("actual Mio Waker");
        }

        fn wait_pending(&self, expected: usize) {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let pending = self.pending();
                assert!(pending <= expected, "unexpected private input: {pending}");
                if pending == expected {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "PRECONDITION FAILURE: {expected} prequeued input bytes never became readable"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }

        fn report(
            &self,
            stage: &str,
            result: &io::Result<Option<InternalEvent>>,
            elapsed: Duration,
        ) {
            eprintln!(
                "{stage}: result={result:?}, elapsed={elapsed:?}, FIONREAD={}, trace={:?}",
                self.pending(),
                self.source.probe
            );
        }

        fn consume_exact(&mut self, expected: &[u8]) {
            let mut consumed = vec![0; expected.len()];
            self.alias
                .read_exact(&mut consumed)
                .expect("completed legitimate private consumer");
            assert_eq!(consumed, expected, "legitimate-consumer exact-byte receipt");
            assert_eq!(
                self.pending(),
                0,
                "consumption must complete before source resumes"
            );
            eprintln!("LEGITIMATE CONSUMER COMPLETE: {consumed:?}, FIONREAD0");
        }

        fn call(&mut self, stage: &str, timeout: Duration) -> io::Result<Option<InternalEvent>> {
            let start = Instant::now();
            let result = self.source.try_read(Some(timeout));
            self.report(stage, &result, start.elapsed());
            result
        }
    }

    fn cancelled(input: &mut PrivateInput) -> io::Result<Option<InternalEvent>> {
        input.source.try_read(None)
    }

    fn assert_char(result: &io::Result<Option<InternalEvent>>, key: char) {
        assert_eq!(
            result.as_ref().expect("actual consumer IO result"),
            &Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char(key),
                KeyModifiers::NONE,
            ))))
        );
    }

    fn assert_interrupted(result: &io::Result<Option<InternalEvent>>) {
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::Interrupted));
    }

    fn assert_key(result: &io::Result<Option<InternalEvent>>, stage: &str) {
        let expected = Some(InternalEvent::Event(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        ))));
        assert_eq!(
            result.as_ref().expect("actual consumer IO result"),
            &expected,
            "DELIVERY CONTRACT FAILURE: {stage}"
        );
    }

    fn run_wake_before_tty_retains_prequeued_key() {
        let mut pty = PrivateInput::new();
        pty.prequeue_key_and_wake(ForcedOrder::WakeFirst);
        let start = Instant::now();
        let first = pty.source.try_read(None);
        pty.report("wake-first call1", &first, start.elapsed());
        assert!(
            matches!(&first, Err(error) if error.kind() == io::ErrorKind::Interrupted),
            "first actual consumer call must preserve wake cancellation: {first:?}"
        );
        assert!(
            pty.source.probe.reads.is_empty(),
            "wake must precede any real TTY read"
        );
        assert_eq!(pty.source.probe.read_attempts, 0);
        assert_eq!(
            pty.pending(),
            1,
            "the sole byte must still be in the real PTY"
        );
        assert!(
            pty.source.probe.next_order.is_none(),
            "ordering seam must be one-shot"
        );
        assert_eq!(pty.source.probe.dispatched_batches[0][0], WAKE_TOKEN);

        // No new byte/wake or TEST-initiated registration; internal refresh is observed.
        let start = Instant::now();
        let second = pty.source.try_read(Some(POLL_TIMEOUT));
        pty.report("wake-first call2, no new input", &second, start.elapsed());
        assert_key(
            &second,
            "prequeued key must survive wake-first real co-batch",
        );
        assert_eq!(pty.pending(), 0);
    }

    fn run_tty_before_wake_delivers_key() {
        let mut pty = PrivateInput::new();
        pty.prequeue_key_and_wake(ForcedOrder::TtyFirst);
        let start = Instant::now();
        let first = pty.source.try_read(None);
        pty.report("tty-first positive control", &first, start.elapsed());
        assert_key(
            &first,
            "TTY-first actual consumer must parse the sole real key",
        );
        assert_eq!(pty.source.probe.dispatched_batches[0][0], TTY_TOKEN);
        assert_eq!(pty.source.probe.reads, vec![Ok(1)]);
        assert_eq!(pty.pending(), 0);
        assert!(pty.source.probe.next_order.is_none());
        let wake = cancelled(&mut pty);
        assert_interrupted(&wake);
        assert_eq!(pty.source.probe.read_attempts, 1);
        assert!(
            pty.source.probe.refresh_results.is_empty(),
            "cancellation precedes refresh"
        );
        let empty = pty.call("after saved cancellation", POLL_TIMEOUT);
        assert_eq!(empty.unwrap(), None);
        assert_eq!(pty.source.probe.read_attempts, 1);
    }

    fn run_wake_only_preserves_cancellation() {
        let mut pty = PrivateInput::new();
        assert_eq!(pty.pending(), 0);
        pty.source
            .waker()
            .wake()
            .expect("actual wake without input");
        let start = Instant::now();
        let first = pty.source.try_read(None);
        let elapsed = start.elapsed();
        pty.report("wake-only cancellation control", &first, elapsed);
        assert!(
            matches!(&first, Err(error) if error.kind() == io::ErrorKind::Interrupted),
            "wake-only actual consumer must preserve cancellation: {first:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "wake-only consumer was not prompt"
        );
        assert_eq!(pty.source.probe.raw_batches.len(), 1);
        assert!(pty.source.probe.raw_batches[0].contains(&WAKE_TOKEN));
        assert!(
            !pty.source.probe.raw_batches[0].contains(&TTY_TOKEN),
            "PRECONDITION FAILURE: wake-only fixture unexpectedly had TTY readiness"
        );
        assert!(pty.source.probe.reads.is_empty());
        assert_eq!(pty.source.probe.read_attempts, 0);
        assert_eq!(pty.pending(), 0);
        let empty = pty.call("wake-only next finite poll", POLL_TIMEOUT);
        assert_eq!(empty.unwrap(), None, "no duplicate saved cancellation");
        assert_eq!(pty.source.probe.read_attempts, 0);
    }

    const CHILD_CASE: &str = "CROSSTERM_MIO_CHILD_CASE";

    struct OwnedChild(Option<Child>);

    impl OwnedChild {
        fn stop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn run_owned(case: &str, test_name: &str) {
        if let Ok(selected) = std::env::var(CHILD_CASE) {
            assert_eq!(selected, case, "private subprocess selector");
            run_child_case(case);
            return;
        }
        let directory = tempfile::tempdir().expect("owned consumer test directory");
        let output = directory.path().join(format!("{case}.log"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .expect("exclusive private regular diagnostic file");
        let child = Command::new(std::env::current_exe().expect("actual test executable"))
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD_CASE, case)
            .stdin(Stdio::null())
            .stdout(Stdio::from(file.try_clone().expect("private stdout file")))
            .stderr(Stdio::from(file))
            .spawn()
            .expect("owned actual-consumer subprocess");
        let mut owned = OwnedChild(Some(child));
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = owned
                .0
                .as_mut()
                .unwrap()
                .try_wait()
                .expect("owned try_wait")
            {
                let _ = owned.0.take();
                break status;
            }
            if Instant::now() >= deadline {
                owned.stop();
                let log = fs::read_to_string(&output).unwrap_or_else(|error| error.to_string());
                panic!(
                    "SAFETY CONTRACT FAILURE: owned child exceeded 5s watchdog; killed/reaped only that child, no rescue input. Log {}:\n{log}",
                    output.display()
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        // Read only after reaping; no diagnostic pipe can block the watchdog.
        let log = fs::read_to_string(&output).expect("read completed private diagnostic file");
        eprintln!("OWNED CASE {case}, output {}:\n{log}", output.display());
        assert!(
            status.success(),
            "owned actual-consumer case {case} failed: {status}"
        );
    }

    fn run_live_stale(stale: bool, timeout: Duration) {
        let mut pty = PrivateInput::with_mode(true, false);
        pty.prequeue_key_and_wake(ForcedOrder::WakeFirst);
        assert_interrupted(&cancelled(&mut pty));
        assert_eq!(pty.source.probe.read_attempts, 0);
        assert_eq!(pty.pending(), 1);
        assert!(pty.source.tty_needs_refresh);
        assert_eq!(pty.source.probe.refresh_attempts, 0);
        if stale {
            pty.consume_exact(b"x");
        }
        let second = pty.call("blocking resume; no new input or wake", timeout);
        assert_eq!(pty.source.probe.refresh_results, vec![Ok(())]);
        assert_eq!(
            pty.source.probe.raw_batches.len(),
            2,
            "resume must actually poll"
        );
        if timeout.is_zero() {
            assert_eq!(
                pty.source.probe.poll_timeouts.last(),
                Some(&Some(Duration::ZERO))
            );
        }
        if stale {
            assert_eq!(
                second.unwrap(),
                None,
                "stale empty source must not enter blocking read"
            );
            assert_eq!(
                pty.source.probe.read_attempts, 0,
                "not just zero completed reads"
            );
            assert_eq!(pty.pending(), 0);
            eprintln!("NEGATIVE STALE PHASE PASSED; only now queue separate liveness y");
            pty.writer.write_all(b"y").expect("post-negative new input");
            pty.wait_pending(1);
            let next = pty.call("separate future liveness, no wake", POLL_TIMEOUT);
            assert_char(&next, 'y');
            assert_eq!(pty.source.probe.read_attempts, 1);
        } else {
            assert_char(&second, 'x');
            assert_eq!(pty.source.probe.read_attempts, 1);
        }
        assert_eq!(pty.source.probe.reads, vec![Ok(1)]);
        assert_eq!(pty.pending(), 0);
    }

    fn run_two_keys() {
        let mut pty = PrivateInput::with_mode(true, false);
        pty.prequeue_and_wake(b"xy", ForcedOrder::TtyFirst, false);
        assert_char(&pty.call("two-key first actual read", POLL_TIMEOUT), 'x');
        assert_eq!(
            pty.source.probe.reads,
            vec![Ok(2)],
            "PRECONDITION FAILURE: require read2"
        );
        assert!(pty.source.tty_needs_refresh, "short read is not WouldBlock");
        assert_eq!(pty.source.probe.read_attempts, 1);
        assert_eq!(pty.source.probe.refresh_attempts, 0);
        assert_char(
            &pty.call("decoded parser y precedes saved wake", POLL_TIMEOUT),
            'y',
        );
        assert_interrupted(&cancelled(&mut pty));
        assert_eq!(pty.source.probe.read_attempts, 1);
        assert_eq!(
            pty.source.probe.refresh_attempts, 0,
            "do not refresh merely for backlog"
        );
        assert_eq!(
            pty.call("zero-time empty after backlog", Duration::ZERO)
                .unwrap(),
            None
        );
        assert_eq!(pty.source.probe.read_attempts, 1);
        assert_eq!(pty.source.probe.refresh_results, vec![Ok(())]);
        assert_eq!(
            pty.source.probe.poll_timeouts.last(),
            Some(&Some(Duration::ZERO))
        );
        assert_eq!(pty.source.probe.raw_batches.len(), 2);
    }

    fn run_full_buffer(consumed: bool) {
        let mut fixture = PrivateInput::with_socketpair();
        let mut input = vec![b'x'; TTY_BUFFER_SIZE];
        input.push(b'y');
        fixture.prequeue_and_wake(&input, ForcedOrder::TtyFirst, false);
        assert_char(
            &fixture.call(
                "1025 queued socket bytes, first actual buffer",
                POLL_TIMEOUT,
            ),
            'x',
        );
        assert_eq!(
            fixture.source.probe.reads,
            vec![Ok(TTY_BUFFER_SIZE)],
            "PRECONDITION FAILURE: require actual full1024 read"
        );
        assert_eq!(fixture.pending(), 1);
        assert!(fixture.source.tty_needs_refresh);
        if consumed {
            fixture.consume_exact(b"y");
        }
        for _ in 0..TTY_BUFFER_SIZE - 1 {
            assert_char(&fixture.source.try_read(Some(POLL_TIMEOUT)), 'x');
            assert_eq!(fixture.source.probe.read_attempts, 1);
            assert_eq!(fixture.source.probe.refresh_attempts, 0);
            assert!(fixture.source.tty_needs_refresh);
        }
        // Exactly1024 total x events, no write or wake after original fixed input.
        assert_interrupted(&cancelled(&mut fixture));
        let next = fixture.call(
            "visited undrained socket input after parser exhaustion",
            POLL_TIMEOUT,
        );
        if consumed {
            assert_eq!(next.unwrap(), None);
            assert_eq!(
                fixture.source.probe.read_attempts, 1,
                "no replay of visited stale input"
            );
            assert_eq!(fixture.source.probe.reads, vec![Ok(TTY_BUFFER_SIZE)]);
        } else {
            assert_char(&next, 'y');
            assert_eq!(fixture.source.probe.reads, vec![Ok(TTY_BUFFER_SIZE), Ok(1)]);
            assert_eq!(fixture.source.probe.read_attempts, 2);
        }
        assert_eq!(fixture.source.probe.refresh_results, vec![Ok(())]);
        let attempts = fixture.source.probe.read_attempts;
        assert_eq!(
            fixture
                .call("final zero-time no duplicates", Duration::ZERO)
                .unwrap(),
            None
        );
        assert_eq!(fixture.source.probe.read_attempts, attempts);
        assert_eq!(
            fixture.source.probe.poll_timeouts.last(),
            Some(&Some(Duration::ZERO))
        );
        assert_eq!(fixture.pending(), 0);
    }

    fn run_signal(order: ForcedOrder, size_error: bool, consumed: bool) {
        let mut pty = PrivateInput::with_mode(true, true);
        if size_error {
            pty.source.probe.size_error_once = Some(io::ErrorKind::PermissionDenied);
        }
        pty.prequeue_and_wake(b"x", order, true);
        let first = if matches!(order, ForcedOrder::WakeSignalTty) {
            cancelled(&mut pty)
        } else {
            pty.call("real three-source first return", POLL_TIMEOUT)
        };
        match order {
            ForcedOrder::TtySignalWake => assert_char(&first, 'x'),
            ForcedOrder::WakeSignalTty => assert_interrupted(&first),
            _ if size_error => assert!(
                matches!(&first, Err(error) if error.kind() == io::ErrorKind::PermissionDenied),
                "actual pending signal must reach exact fallible size query: {first:?}"
            ),
            _ => assert_eq!(
                first.unwrap(),
                Some(InternalEvent::Event(Event::Resize(91, 37)))
            ),
        }
        if !matches!(order, ForcedOrder::TtySignalWake) {
            assert_eq!(pty.source.probe.read_attempts, 0);
            assert_eq!(pty.pending(), 1);
        }
        assert!(pty.source.tty_needs_refresh);
        if consumed {
            pty.consume_exact(b"x");
        }
        if matches!(
            order,
            ForcedOrder::TtySignalWake | ForcedOrder::WakeSignalTty
        ) {
            let resize = pty.call("actual saved signal before backlog/refresh", POLL_TIMEOUT);
            assert_eq!(
                resize.unwrap(),
                Some(InternalEvent::Event(Event::Resize(91, 37)))
            );
            assert_eq!(pty.source.probe.refresh_attempts, 0);
        }
        if !matches!(order, ForcedOrder::WakeSignalTty) {
            assert_interrupted(&cancelled(&mut pty));
            assert_eq!(pty.source.probe.refresh_attempts, 0);
        }
        assert_eq!(
            pty.source.probe.size_queries, 1,
            "failed/successful signal retires once"
        );
        let read_attempts = pty.source.probe.read_attempts;
        let last = pty.call("after saved signal/control tail", POLL_TIMEOUT);
        if consumed || matches!(order, ForcedOrder::TtySignalWake) {
            assert_eq!(last.unwrap(), None);
            assert_eq!(pty.source.probe.read_attempts, read_attempts);
        } else {
            assert_char(&last, 'x');
            assert_eq!(pty.source.probe.read_attempts, 1);
        }
        assert_eq!(pty.source.probe.refresh_results, vec![Ok(())]);
        assert_eq!(pty.pending(), 0);
        assert_eq!(pty.source.probe.size_queries, 1);
    }

    fn run_refresh_error() {
        let mut pty = PrivateInput::with_mode(true, false);
        pty.prequeue_key_and_wake(ForcedOrder::WakeFirst);
        assert_interrupted(&cancelled(&mut pty));
        pty.source.probe.refresh_error_once = Some(io::ErrorKind::PermissionDenied);
        let failed = pty.call("exact reregister error branch", POLL_TIMEOUT);
        assert!(matches!(failed, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
        assert!(pty.source.tty_needs_refresh);
        assert_eq!(
            pty.source.probe.poll_attempts, 1,
            "failed refresh must not poll"
        );
        assert_eq!(pty.source.probe.read_attempts, 0);
        assert_eq!(pty.pending(), 1);
        assert_eq!(
            pty.source.probe.refresh_results,
            vec![Err(io::ErrorKind::PermissionDenied)]
        );
        let recovered = pty.call("retry real registration; no new input/wake", POLL_TIMEOUT);
        assert_char(&recovered, 'x');
        assert_eq!(
            pty.source.probe.refresh_results,
            vec![Err(io::ErrorKind::PermissionDenied), Ok(())]
        );
        assert_eq!(pty.source.probe.read_attempts, 1);
    }

    fn run_poll_error() {
        let mut pty = PrivateInput::with_mode(true, false);
        pty.prequeue_key_and_wake(ForcedOrder::WakeFirst);
        assert_interrupted(&cancelled(&mut pty));
        pty.source.probe.poll_error_once = Some(io::ErrorKind::Other);
        let failed = pty.call("exact Poll error branch after refresh", POLL_TIMEOUT);
        assert!(matches!(failed, Err(error) if error.kind() == io::ErrorKind::Other));
        assert!(pty.source.events.is_empty());
        assert!(pty.source.probe.current_batch.is_empty());
        assert_eq!(pty.source.next_event, 0);
        assert_eq!(pty.source.probe.read_attempts, 0);
        assert_eq!(
            pty.source.probe.raw_batches.len(),
            1,
            "fault is not fetched readiness"
        );
        assert_char(
            &pty.call("real Poll after failure; no replayed WAKE", POLL_TIMEOUT),
            'x',
        );
        assert_eq!(pty.source.probe.read_attempts, 1);

        let mut empty = PrivateInput::with_mode(true, false);
        empty.source.probe.poll_error_once = Some(io::ErrorKind::Interrupted);
        assert_eq!(
            empty
                .call("interrupted Poll retries same budget", POLL_TIMEOUT)
                .unwrap(),
            None
        );
        assert_eq!(empty.source.probe.poll_attempts, 2);
        assert_eq!(empty.source.probe.raw_batches, vec![Vec::<Token>::new()]);
        let budgets = &empty.source.probe.poll_timeouts;
        assert!(budgets[1].unwrap() <= budgets[0].unwrap());
        assert_eq!(empty.source.probe.read_attempts, 0);
    }

    fn run_incomplete_input(wake: bool) {
        let mut pty = PrivateInput::with_mode(true, false);
        if wake {
            pty.prequeue_and_wake(b"\x1b[", ForcedOrder::TtyFirst, false);
        } else {
            pty.writer.write_all(b"\x1b[").unwrap();
            pty.wait_pending(2);
        }
        let result = if wake {
            cancelled(&mut pty)
        } else {
            pty.call("incomplete CSI before completion", POLL_TIMEOUT)
        };
        if wake {
            assert_interrupted(&result);
        } else {
            assert_eq!(result.unwrap(), None);
        }
        assert_eq!(pty.pending(), 0);
        pty.writer.write_all(b"A").unwrap();
        pty.wait_pending(1);
        assert_eq!(
            pty.call("complete the retained CSI", POLL_TIMEOUT).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            ))))
        );
    }

    fn run_closed_input(socket: bool) {
        let mut source = if socket {
            let PrivateInput { writer, source, .. } = PrivateInput::with_socketpair();
            drop(writer);
            source
        } else {
            let PrivateInput {
                writer,
                alias,
                source,
            } = PrivateInput::with_mode(true, false);
            drop(source);
            drop(alias);
            // A PTY master with no open slaves yields a real EIO on Linux.
            UnixInternalEventSource::from_file_descriptor(FileDesc::Owned(writer.into())).unwrap()
        };
        let error = source.try_read(Some(POLL_TIMEOUT)).unwrap_err();
        if socket {
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        } else {
            assert_eq!(error.raw_os_error(), Some(probe_libc::EIO));
        }
    }

    fn run_child_case(case: &str) {
        match case {
            "wake-only" => run_wake_only_preserves_cancellation(),
            "tty-before-wake" => run_tty_before_wake_delivers_key(),
            "wake-before-tty" => run_wake_before_tty_retains_prequeued_key(),
            "incomplete-timeout" => run_incomplete_input(false),
            "incomplete-wake" => run_incomplete_input(true),
            "input-eof" => run_closed_input(true),
            #[cfg(target_os = "linux")]
            "input-eio" => run_closed_input(false),
            "blocking-live-finite" => run_live_stale(false, POLL_TIMEOUT),
            "blocking-stale-finite" => run_live_stale(true, POLL_TIMEOUT),
            "blocking-live-zero" => run_live_stale(false, Duration::ZERO),
            "blocking-stale-zero" => run_live_stale(true, Duration::ZERO),
            "two-keys" => run_two_keys(),
            "full-buffer-live" => run_full_buffer(false),
            "full-buffer-consumed" => run_full_buffer(true),
            "signal-first" => run_signal(ForcedOrder::SignalWakeTty, false, false),
            "wake-signal" => run_signal(ForcedOrder::WakeSignalTty, false, false),
            "tty-signal" => run_signal(ForcedOrder::TtySignalWake, false, false),
            "signal-consumed" => run_signal(ForcedOrder::SignalWakeTty, false, true),
            "signal-error" => run_signal(ForcedOrder::SignalTtyWake, true, false),
            "signal-error-consumed" => run_signal(ForcedOrder::SignalTtyWake, true, true),
            "refresh-error" => run_refresh_error(),
            "poll-error" => run_poll_error(),
            _ => panic!("unknown private owned case: {case}"),
        }
    }

    macro_rules! owned_case {
        ($name:ident, $case:literal) => {
            #[test]
            fn $name() {
                run_owned(
                    $case,
                    concat!(
                        "event::source::unix::mio::forced_batch_tests::",
                        stringify!($name)
                    ),
                );
            }
        };
    }

    owned_case!(
        forced_batch_wake_before_tty_retains_prequeued_key,
        "wake-before-tty"
    );
    owned_case!(forced_batch_tty_before_wake_delivers_key, "tty-before-wake");
    owned_case!(forced_batch_wake_only_preserves_cancellation, "wake-only");
    owned_case!(
        forced_batch_input_incomplete_timeout_preserves_sequence,
        "incomplete-timeout"
    );
    owned_case!(
        forced_batch_input_incomplete_wake_preserves_sequence,
        "incomplete-wake"
    );
    owned_case!(forced_batch_input_eof_returns_error, "input-eof");
    #[cfg(target_os = "linux")]
    owned_case!(forced_batch_input_eio_returns_error, "input-eio");

    owned_case!(forced_batch_blocking_live_finite, "blocking-live-finite");
    owned_case!(forced_batch_blocking_stale_finite, "blocking-stale-finite");
    owned_case!(forced_batch_blocking_live_zero, "blocking-live-zero");
    owned_case!(forced_batch_blocking_stale_zero, "blocking-stale-zero");
    owned_case!(forced_batch_two_keys_saved_cancellation, "two-keys");
    owned_case!(forced_batch_full_buffer_kernel_tail, "full-buffer-live");
    owned_case!(
        forced_batch_full_buffer_consumed_tail,
        "full-buffer-consumed"
    );
    owned_case!(forced_batch_signal_before_wake, "signal-first");
    owned_case!(forced_batch_wake_before_signal, "wake-signal");
    owned_case!(forced_batch_key_before_signal, "tty-signal");
    owned_case!(forced_batch_signal_then_consumed_tty, "signal-consumed");
    owned_case!(forced_batch_size_error_preserves_tail, "signal-error");
    owned_case!(
        forced_batch_size_error_consumed_tty,
        "signal-error-consumed"
    );
    owned_case!(
        forced_batch_refresh_error_preserves_obligation,
        "refresh-error"
    );
    owned_case!(forced_batch_poll_error_does_not_replay_tail, "poll-error");
}
