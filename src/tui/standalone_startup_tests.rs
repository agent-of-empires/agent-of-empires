use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn modes(fd: i32) -> libc::termios {
    let mut settings = std::mem::MaybeUninit::uninit();
    // SAFETY: the private controlling PTY initializes this value on success.
    assert_eq!(unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) }, 0);
    // SAFETY: tcgetattr succeeded above.
    unsafe { settings.assume_init() }
}

fn startup_case(test: &str, remote: bool, unwind: bool) {
    const CHILD: &str = "AOE_STANDALONE_STARTUP_TEST";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        let before = modes(0);
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
                    assert_eq!(modes(0).c_lflag & libc::ICANON, 0);
                    entered = true;
                    panic!("controlled unwind while the standalone entrypoint owns raw input");
                } else {
                    assert!(startup.await.is_err(), "read-only output must refuse startup");
                }
            });
        }));
        assert_eq!(result.is_err(), unwind);
        assert!(!unwind || entered, "the real entrypoint must reach raw-mode ownership");
        let after = modes(0);
        assert_eq!(
            (after.c_iflag, after.c_oflag, after.c_cflag, after.c_lflag, after.c_cc),
            (before.c_iflag, before.c_oflag, before.c_cflag, before.c_lflag, before.c_cc),
            "standalone startup must restore input on errors and unwind"
        );
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (mut master, slave) = terminal_output::tests::private_terminal();
    let mut settings = modes(slave.as_raw_fd());
    settings.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
    settings.c_iflag |= libc::ICRNL | libc::IXON;
    settings.c_oflag |= libc::OPOST;
    // SAFETY: only this case owns the private PTY.
    assert_eq!(unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &settings) }, 0);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, test)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("AOE_DAEMON_URL", format!("http://{}", listener.local_addr().unwrap()))
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
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut emitted = Vec::new();
    let status = loop {
        terminal_output::tests::drain_terminal(&mut master, &mut emitted);
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("standalone startup cleanup exceeded its private process deadline");
        }
        std::thread::yield_now();
    };
    let output = child.wait_with_output().unwrap();
    assert!(
        status.success(),
        "startup child failed: {}\n{}",
        String::from_utf8_lossy(&emitted),
        String::from_utf8_lossy(&output.stderr)
    );
    let after = modes(slave.as_raw_fd());
    assert_eq!(after.c_lflag, settings.c_lflag);
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
