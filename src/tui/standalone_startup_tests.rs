use super::*;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn modes(fd: i32) -> io::Result<libc::termios> {
    let mut settings = std::mem::MaybeUninit::uninit();
    // SAFETY: the private controlling PTY initializes this value on success.
    if unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: tcgetattr succeeded above.
    Ok(unsafe { settings.assume_init() })
}

struct StartupChild {
    process: std::process::Child,
    stderr: Option<std::thread::JoinHandle<(Vec<u8>, Option<io::Error>)>>,
}

impl Drop for StartupChild {
    fn drop(&mut self) {
        if !matches!(self.process.try_wait(), Ok(Some(_))) {
            let _ = self.process.kill();
        }
        let _ = self.process.wait();
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

fn drain_startup_terminal(
    master: &mut File,
    emitted: &mut Vec<u8>,
    exiting: bool,
) -> io::Result<bool> {
    let mut buffer = [0; 4096];
    loop {
        match master.read(&mut buffer) {
            Ok(0) if exiting => return Ok(true),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "private controlling terminal closed before parent release",
                ));
            }
            Ok(count) => emitted.extend_from_slice(&buffer[..count]),
            Err(error) if exiting && error.raw_os_error() == Some(libc::EIO) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn startup_case(test: &str, remote: bool, unwind: bool) {
    const CHILD: &str = "AOE_STANDALONE_STARTUP_TEST";
    const RESTORED: &[u8] = b"RESTORED";
    const RELEASE: &[u8] = b"PARENTRELEASE";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        let before = modes(0).unwrap();
        assert_ne!(before.c_lflag & libc::ICANON, 0);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(unwind)
            .open("/dev/tty")
            .unwrap();
        let (_output, _) = terminal_output::install_test_output(file);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let endpoint = crate::daemon::DaemonEndpoint::new(
            std::env::var("AOE_DAEMON_URL").unwrap(),
            Some("startup-fixture-token".into()),
            crate::daemon::Source::Env,
        );
        let mut entered = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let mut startup: std::pin::Pin<
                    Box<dyn std::future::Future<Output = anyhow::Result<()>>>,
                > = if remote {
                    Box::pin(remote_home::run_standalone(endpoint))
                } else {
                    Box::pin(structured_view::run_standalone("startup-fixture"))
                };
                if unwind {
                    assert!(futures_util::poll!(startup.as_mut()).is_pending());
                    assert_eq!(modes(0).unwrap().c_lflag & libc::ICANON, 0);
                    entered = true;
                    panic!("controlled unwind while the standalone entrypoint owns raw input");
                } else {
                    assert!(
                        startup.await.is_err(),
                        "read-only output must refuse startup"
                    );
                }
            });
        }));
        assert_eq!(result.is_err(), unwind);
        assert!(
            !unwind || entered,
            "the real entrypoint must reach raw-mode ownership"
        );
        let after = modes(0).unwrap();
        assert_eq!(
            (
                after.c_iflag,
                after.c_oflag,
                after.c_cflag,
                after.c_lflag,
                after.c_cc
            ),
            (
                before.c_iflag,
                before.c_oflag,
                before.c_cflag,
                before.c_lflag,
                before.c_cc
            ),
            "standalone startup must restore input on errors and unwind"
        );
        let socket = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
            .join("startup-custody.sock");
        let mut custody = UnixStream::connect(socket).unwrap();
        custody
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        custody
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        custody.write_all(RESTORED).unwrap();
        let mut release = [0; RELEASE.len()];
        custody.read_exact(&mut release).unwrap();
        assert_eq!(
            &release, RELEASE,
            "the parent must explicitly release terminal custody"
        );
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (mut master, slave) = terminal_output::tests::private_terminal();
    let mut settings = modes(slave.as_raw_fd()).unwrap();
    settings.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
    settings.c_iflag |= libc::ICRNL | libc::IXON;
    settings.c_oflag |= libc::OPOST;
    // SAFETY: only this case owns the private PTY.
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &settings) },
        0
    );
    let listener_path = home.path().join("startup-custody.sock");
    let custody_listener = UnixListener::bind(listener_path).unwrap();
    custody_listener.set_nonblocking(true).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, test)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env(
            "AOE_DAEMON_URL",
            format!("http://{}", listener.local_addr().unwrap()),
        )
        .env("AOE_DAEMON_TOKEN", "startup-fixture-token")
        .env_remove("AOE_DAEMON_PASSPHRASE")
        .env_remove("TMUX")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe session/controlling-terminal operations run before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = StartupChild {
        process: command.spawn().unwrap(),
        stderr: None,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stderr = child.process.stderr.take().unwrap();
    child.stderr = Some(std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let error = stderr.read_to_end(&mut bytes).err();
        (bytes, error)
    }));
    let mut emitted = Vec::new();
    let mut custody = None;
    let mut restored = [0; RESTORED.len()];
    let mut received = 0;
    let mut released = false;
    let mut terminal_closed = false;
    let mut parent_modes = None;
    let mut failure = None;
    let status = loop {
        if !terminal_closed {
            match drain_startup_terminal(&mut master, &mut emitted, released) {
                Ok(closed) => terminal_closed = closed,
                Err(error) => {
                    failure = Some(format!(
                        "terminal drain before released={released}: {error}"
                    ));
                    break None;
                }
            }
        }
        if !released {
            if custody.is_none() {
                match custody_listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(error) = stream.set_nonblocking(true) {
                            failure = Some(format!("configure custody channel: {error}"));
                            break None;
                        }
                        custody = Some(stream);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => {
                        failure = Some(format!("accept custody channel: {error}"));
                        break None;
                    }
                }
            }
            if let Some(channel) = custody.as_mut() {
                match channel.read(&mut restored[received..]) {
                    Ok(0) => {
                        failure = Some("custody channel closed before restored proof".into());
                        break None;
                    }
                    Ok(count) => received += count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => {
                        failure = Some(format!("read restored proof: {error}"));
                        break None;
                    }
                }
                if received == RESTORED.len() {
                    if restored.as_slice() != RESTORED {
                        failure = Some("unexpected restored proof".into());
                        break None;
                    }
                    // Darwin revokes the PTY on session-leader exit: observe it while held alive.
                    match modes(slave.as_raw_fd()) {
                        Ok(actual) => {
                            let restored = actual.c_lflag == settings.c_lflag;
                            parent_modes = Some(actual);
                            if !restored {
                                failure = Some("parent observed unrestored terminal flags".into());
                                break None;
                            }
                        }
                        Err(error) => {
                            failure = Some(format!("parent terminal observation: {error}"));
                            break None;
                        }
                    }
                    if let Err(error) = channel.write_all(RELEASE) {
                        failure = Some(format!("release terminal custody: {error}"));
                        break None;
                    }
                    released = true;
                }
            }
        }
        match child.process.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => {
                failure = Some(format!("inspect startup child: {error}"));
                break None;
            }
        }
        if Instant::now() >= deadline {
            failure =
                Some("standalone startup cleanup exceeded its private process deadline".into());
            break None;
        }
        std::thread::yield_now();
    };
    let kill_error = if status.is_none() {
        child.process.kill().err()
    } else {
        None
    };
    let reaped = child.process.wait();
    let (stderr, stderr_error) = child.stderr.take().unwrap().join().unwrap();
    assert!(
        failure.is_none()
            && released
            && status.is_some_and(|status| status.success())
            && reaped.as_ref().is_ok_and(|status| status.success())
            && stderr_error.is_none(),
        "startup child failed: status={status:?}, reaped={reaped:?}, released={released}, proof_bytes={received}, terminal_closed={terminal_closed}, failure={failure:?}, kill_error={kill_error:?}, stderr_error={stderr_error:?}\n{}\n{}",
        String::from_utf8_lossy(&emitted),
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(parent_modes.unwrap().c_lflag, settings.c_lflag);
}

#[test]
fn structured_startup_error_restores_private_tty_input() {
    startup_case(
        "tui::standalone_startup_tests::structured_startup_error_restores_private_tty_input",
        false,
        false,
    );
}

#[test]
fn remote_startup_error_restores_private_tty_input() {
    startup_case(
        "tui::standalone_startup_tests::remote_startup_error_restores_private_tty_input",
        true,
        false,
    );
}

#[test]
fn structured_startup_unwind_restores_private_tty_input() {
    startup_case(
        "tui::standalone_startup_tests::structured_startup_unwind_restores_private_tty_input",
        false,
        true,
    );
}

#[test]
fn remote_startup_unwind_restores_private_tty_input() {
    startup_case(
        "tui::standalone_startup_tests::remote_startup_unwind_restores_private_tty_input",
        true,
        true,
    );
}
