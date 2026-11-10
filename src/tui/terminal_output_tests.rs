use super::*;

#[cfg(unix)]
#[test]
fn stalled_terminal_bounds_actual_writes_and_recovers_framing_before_later_bytes() {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    let (mut master_fd, mut slave_fd) = (-1, -1);
    // SAFETY: openpty initializes two owned descriptors; no optional termios or name buffer.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let mut slave = unsafe { File::from_raw_fd(slave_fd) };
    for file in [&master, &slave] {
        // SAFETY: this isolated PTY belongs only to this test.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        // SAFETY: no stdout descriptor shares this test's file description.
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
    }
    // SAFETY: pause this owned PTY so kernel queue movement cannot reopen capacity.
    assert_eq!(unsafe { libc::tcflow(slave.as_raw_fd(), libc::TCOOFF) }, 0);
    let mut filled = 0;
    let bytes = [b'x'; 8192];
    loop {
        match slave.write(&bytes) {
            Ok(0) => panic!("PTY closed before becoming backpressured"),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("fill isolated PTY: {error}"),
        }
    }
    let beginning = Instant::now();
    let mut writer = DeadlineWriter {
        file: &mut slave,
        deadline: beginning + Duration::from_millis(20),
        written: 0,
    };
    assert_eq!(
        writer
            .write_all(b"payload that cannot fit")
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert!(beginning.elapsed() < Duration::from_secs(1));
    let mut state = State::new(slave);
    state.interrupted(true);
    assert!(
        state.recovery.is_some(),
        "framing must stay pending while output cannot drain"
    );
    // SAFETY: resume only after proving the actual writer and recovery both stopped.
    assert_eq!(
        unsafe { libc::tcflow(state.file.as_raw_fd(), libc::TCOON) },
        0
    );
    let mut scratch = [0; 8192];
    let mut drained = 0;
    let deadline = Instant::now() + Duration::from_secs(1);
    while drained < filled {
        assert!(Instant::now() < deadline, "filled PTY did not drain");
        match master.read(&mut scratch) {
            Ok(0) => panic!("PTY closed"),
            Ok(count) => drained += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::yield_now(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("drain isolated PTY: {error}"),
        }
    }
    state
        .recover(Instant::now() + Duration::from_secs(1))
        .unwrap();
    assert!(state.recovery.is_none());
    let mut writer = DeadlineWriter {
        file: &mut state.file,
        deadline: Instant::now() + Duration::from_secs(1),
        written: 0,
    };
    writer.write_all(b"AFTER-RECOVERY").unwrap();
    let mut recovered = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !recovered.ends_with(b"AFTER-RECOVERY") {
        assert!(Instant::now() < deadline, "recovery output did not arrive");
        match master.read(&mut scratch) {
            Ok(count) => recovered.extend_from_slice(&scratch[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::yield_now(),
            Err(error) => panic!("read recovery: {error}"),
        }
    }
    assert!(recovered.starts_with(b"\x18\x1b\\\x1bPtmux;\x18\x1b\\"));
    assert!(recovered.ends_with(b"\x1b[0mAFTER-RECOVERY"));
}

#[cfg(unix)]
pub(crate) fn private_terminal() -> (File, File) {
    use std::os::fd::{AsRawFd, FromRawFd};
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: both initialized descriptors are exclusively owned below.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: openpty returned two distinct owned descriptors.
    let master = unsafe { File::from_raw_fd(master) };
    // SAFETY: ownership of this second descriptor has not been transferred.
    let slave = unsafe { File::from_raw_fd(slave) };
    let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr initializes settings on success.
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), settings.as_mut_ptr()) },
        0
    );
    // SAFETY: tcgetattr above succeeded.
    let mut settings = unsafe { settings.assume_init() };
    // SAFETY: these settings and the isolated PTY belong only to this test.
    unsafe { libc::cfmakeraw(&mut settings) };
    // SAFETY: no other thread changes this test's terminal settings.
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &settings) },
        0
    );
    for file in [&master, &slave] {
        // SAFETY: both descriptors are valid and private to this test.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        // SAFETY: this does not alter any process stdout description.
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
    }
    (master, slave)
}

#[cfg(unix)]
pub(crate) fn drain_terminal(master: &mut File, bytes: &mut Vec<u8>) {
    use std::io::Read;
    let mut scratch = [0; 4096];
    loop {
        match master.read(&mut scratch) {
            Ok(0) => panic!("private terminal closed"),
            Ok(count) => bytes.extend_from_slice(&scratch[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("drain private terminal: {error}"),
        }
    }
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn slow_terminal_finishes_one_frame_suffix_before_osc_or_handoff() {
    let (mut master, slave) = private_terminal();
    let output = Mutex::new(State::new(slave));
    let payload = format!("\x1b[?2026h{}\x1b[?2026l", "FRAME-ROW|".repeat(32_768));
    let first = frame_in(&output, |_| {
        let mut writer = Output::default();
        writer.write_all(payload.as_bytes())?;
        writer.flush()?;
        Ok(())
    })
    .unwrap();
    let frame_id = first.captured.expect("frame was captured");
    assert_eq!(
        first.presented, None,
        "undrained PTY must backpressure this frame"
    );
    assert!(output.lock().unwrap().pending.as_ref().unwrap().offset > 0);
    assert_eq!(
        suspend_in(&output).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let mut osc_called = false;
    assert_eq!(
        osc52_in(&output, Duration::from_millis(5), false, |_| {
            osc_called = true;
            Ok(())
        })
        .unwrap_err()
        .kind(),
        io::ErrorKind::TimedOut
    );
    assert!(
        !osc_called,
        "OSC must not interrupt a partially emitted frame"
    );
    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "pending frame never made progress"
        );
        drain_terminal(&mut master, &mut received);
        let started = Instant::now();
        let outcome = frame_in(&output, |_| panic!("pending frame was rebuilt")).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "input would be blocked"
        );
        assert_eq!(outcome.captured, None);
        if outcome.presented == Some(frame_id) {
            break;
        }
        assert_eq!(outcome.presented, None);
    }
    osc52_in(&output, Duration::from_secs(1), false, |writer| {
        writer.write_all(b"AFTER-FRAME")
    })
    .unwrap();
    while !received.ends_with(b"AFTER-FRAME") {
        assert!(Instant::now() < deadline);
        drain_terminal(&mut master, &mut received);
        std::thread::yield_now();
    }
    assert!(received.starts_with(payload.as_bytes()));
    assert_eq!(
        received
            .windows(b"FRAME-ROW|".len())
            .filter(|bytes| *bytes == b"FRAME-ROW|")
            .count(),
        32_768
    );
    assert!(received.ends_with(b"AFTER-FRAME"));
    assert!(output.lock().unwrap().pending.is_none());
    suspend_in(&output).unwrap();
    assert!(output.lock().unwrap().suspended);
}

#[cfg(unix)]
pub(crate) fn block_terminal(slave: &mut File) {
    use std::os::fd::AsRawFd;
    // SAFETY: only this private terminal's output is paused.
    assert_eq!(unsafe { libc::tcflow(slave.as_raw_fd(), libc::TCOOFF) }, 0);
    loop {
        match slave.write(&[b'?'; 4096]) {
            Ok(0) => panic!("private terminal closed while filling"),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("fill private terminal: {error}"),
        }
    }
}

#[cfg(unix)]
pub(crate) fn resume_terminal(slave: &File) {
    use std::os::fd::AsRawFd;
    // SAFETY: this resumes only the private terminal paused above.
    assert_eq!(unsafe { libc::tcflow(slave.as_raw_fd(), libc::TCOON) }, 0);
}

#[cfg(unix)]
pub(crate) fn read_terminal_until(master: &mut File, bytes: &mut Vec<u8>, needle: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !bytes.windows(needle.len()).any(|window| window == needle) {
        assert!(
            Instant::now() < deadline,
            "expected terminal bytes did not arrive"
        );
        drain_terminal(master, bytes);
        std::thread::yield_now();
    }
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn pending_frame_flush_then_control_backpressure_retains_ordered_restore() {
    let (mut master, slave) = private_terminal();
    let mut control = slave.try_clone().unwrap();
    let (_guard, output) = install_test_output(slave);
    let payload = "OLD-FRAME|".repeat(32_768);
    let first = frame(|_| {
        let mut writer = Output::default();
        writer.write_all(payload.as_bytes())?;
        writer.flush()?;
        Ok(())
    })
    .unwrap();
    assert!(first.captured.is_some() && first.presented.is_none());
    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut admitted = loop {
        assert!(Instant::now() < deadline);
        drain_terminal(&mut master, &mut received);
        match suspend() {
            Ok(admitted) => break admitted,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("admit after pending frame: {error}"),
        }
    };
    assert!(
        output.0.lock().unwrap().pending.is_none(),
        "admission flushed the real old frame"
    );
    block_terminal(&mut control);
    assert!(
        !admitted.begin_handoff().unwrap(),
        "fresh control backpressure must refuse child entry"
    );
    drop(admitted);
    assert!(!output.0.lock().unwrap().suspended);
    assert!(output.0.lock().unwrap().control.is_some());
    assert!(frame(|_| panic!("frame overtook restoration"))
        .unwrap()
        .captured
        .is_none());
    resume_terminal(&control);
    let mut captured = None;
    let mut draws = 0;
    loop {
        assert!(Instant::now() < deadline);
        drain_terminal(&mut master, &mut received);
        let result = frame(|_| {
            draws += 1;
            let mut writer = Output::default();
            writer.write_all(b"AFTER-CONTROL-RESTORE")?;
            writer.flush()?;
            Ok(())
        })
        .unwrap();
        if let Some(frame) = result.captured {
            captured = Some(frame);
        }
        if captured.is_some() && result.presented == captured {
            break;
        }
    }
    read_terminal_until(&mut master, &mut received, b"AFTER-CONTROL-RESTORE");
    assert_eq!(draws, 1);
    assert!(
        received.starts_with(payload.as_bytes()),
        "old frame must not be truncated or repeated"
    );
    let leave = received
        .windows(b"\x1b[?1049l".len())
        .position(|bytes| bytes == b"\x1b[?1049l")
        .unwrap();
    let restore = received
        .windows(b"\x1b[?1049h".len())
        .position(|bytes| bytes == b"\x1b[?1049h")
        .unwrap();
    let after = received
        .windows(b"AFTER-CONTROL-RESTORE".len())
        .position(|bytes| bytes == b"AFTER-CONTROL-RESTORE")
        .unwrap();
    assert!(leave < restore && restore < after);
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn child_result_survives_backpressured_terminal_restoration() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("child-count");
    let (mut master, slave) = private_terminal();
    let mut control = slave.try_clone().unwrap();
    let (_guard, output) = install_test_output(slave);
    let mut admitted = suspend().unwrap();
    assert!(admitted.begin_handoff().unwrap());
    let result = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "printf child >> \"$1\"; printf CHILD-OUTPUT",
            "controlled-child",
        ])
        .arg(&marker)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(control.try_clone().unwrap()))
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    let mut received = Vec::new();
    read_terminal_until(&mut master, &mut received, b"CHILD-OUTPUT");
    block_terminal(&mut control);
    drop(admitted);
    assert!(output.0.lock().unwrap().control.is_some());
    assert!(frame(|_| panic!("draw overtook post-child restore"))
        .unwrap()
        .captured
        .is_none());
    resume_terminal(&control);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut captured = None;
    loop {
        assert!(Instant::now() < deadline);
        drain_terminal(&mut master, &mut received);
        let result = frame(|_| {
            let mut writer = Output::default();
            writer.write_all(b"AFTER-CHILD-RESTORE")?;
            writer.flush()?;
            Ok(())
        })
        .unwrap();
        if let Some(frame) = result.captured {
            captured = Some(frame);
        }
        if captured.is_some() && result.presented == captured {
            break;
        }
    }
    read_terminal_until(&mut master, &mut received, b"AFTER-CHILD-RESTORE");
    assert!(
        result.success(),
        "real child result survives delayed restoration"
    );
    assert_eq!(std::fs::read(marker).unwrap(), b"child");
    let restore = received
        .windows(b"\x1b[?1049h".len())
        .position(|bytes| bytes == b"\x1b[?1049h")
        .unwrap();
    let after = received
        .windows(b"AFTER-CHILD-RESTORE".len())
        .position(|bytes| bytes == b"AFTER-CHILD-RESTORE")
        .unwrap();
    assert!(restore < after);
}
