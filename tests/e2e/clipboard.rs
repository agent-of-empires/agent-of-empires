#![cfg(unix)]

use std::{
    fs::{self, OpenOptions},
    io::Write,
    time::{Duration, Instant},
};

use serial_test::parallel;

use crate::harness::{require_tmux, wait_until, write_executable, TuiTestHarness};

#[test]
#[parallel]
fn active_preview_copy_keeps_input_responsive_with_hung_helpers() {
    require_tmux!();
    let mut harness = TuiTestHarness::new_in_tmp("clipboard_hung_helper");
    harness.append_config("[session]\ndefault_attach_mode = \"live_send\"");
    let bin = harness.home_path().join("clipboard-bin");
    fs::create_dir(&bin).unwrap();
    let gate = harness.home_path().join("clipboard-gate");
    nix::unistd::mkfifo(
        &gate,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let mut release = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&gate)
        .unwrap();
    let started = harness.home_path().join("clipboard-started");
    for command in ["pbcopy", "wl-copy", "xclip", "xsel"] {
        write_executable(
            &bin.join(command),
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$AOE_CLIPBOARD_STARTED\"\nIFS= read -r line < \"$AOE_CLIPBOARD_GATE\"\n",
        );
    }
    harness.add_path_dir(&bin);
    harness.set_env("WAYLAND_DISPLAY", "aoe-clipboard-test");
    harness.set_env("AOE_CLIPBOARD_STARTED", &started);
    harness.set_env("AOE_CLIPBOARD_GATE", &gate);
    let emitter = harness.home_path().join("emit-copy.sh");
    write_executable(&emitter, "#!/bin/sh\nprintf 'ACTIVE-COPY-MARKER\\n'\n");
    harness.run_cli_ok(&[
        "add",
        harness.project_path().to_str().unwrap(),
        "-t",
        "ClipboardControl",
    ]);
    harness.spawn_tui();
    harness.wait_for("ClipboardControl");
    harness.send_keys("t");
    harness.send_keys("Enter");
    harness.wait_for_timeout("LIVE", Duration::from_secs(15));
    harness.type_text(&format!("clear; sh {}", emitter.display()));
    harness.send_keys("Enter");
    harness.wait_for("ACTIVE-COPY-MARKER");
    let screen = harness.capture_screen();
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("ACTIVE-COPY-MARKER"))
        .unwrap();
    let byte = line.find("ACTIVE-COPY-MARKER").unwrap();
    let column = line[..byte].chars().count() + 1;
    let row = row + 1;
    let end = column + "ACTIVE-COPY-MARKER".len() - 1;
    harness.send_keys_unfenced(&format!(
        "\x1b[<0;{column};{row}M\x1b[<32;{end};{row}M\x1b[<0;{end};{row}m"
    ));
    let mut original_pid = None;
    wait_until(Duration::from_secs(10), Duration::from_millis(10), || {
        let pid = fs::read_to_string(&started)
            .map_err(|error| error.to_string())?
            .trim()
            .parse::<i32>()
            .map_err(|error| error.to_string())?;
        original_pid = Some(pid);
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
            .map_err(|error| error.to_string())
    });
    let original_pid = original_pid.expect("original helper started");
    harness.send_keys_unfenced("C-q");
    let beginning = Instant::now();
    let deadline = beginning + Duration::from_secs(3);
    let (responsive, last_screen) = loop {
        let screen = harness.capture_screen();
        if !screen.contains("LIVE") {
            let still_original =
                fs::read_to_string(&started).unwrap().trim() == original_pid.to_string();
            let still_alive =
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(original_pid), None).is_ok();
            break (still_original && still_alive, screen);
        }
        if Instant::now() >= deadline {
            break (false, screen);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Keep the helper blocked throughout the input-observation window.
    writeln!(release, "release").unwrap();
    eprintln!(
        "active clipboard input responsive={responsive}, observed in {:?}",
        beginning.elapsed()
    );
    assert!(
        responsive,
        "hung clipboard helper blocked live-mode exit:\n{last_screen}"
    );
}

#[test]
#[parallel]
fn app_processes_input_and_exits_while_terminal_output_stays_stalled() {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
    use std::process::{Child, Stdio};
    require_tmux!();
    let mut harness = TuiTestHarness::new_in_tmp("terminal_stalled_input");
    harness.stop_daemon_on_drop();
    let barrier = harness.home_path().join("processed-input");
    harness.set_env("AOE_E2E_INPUT_BARRIER", &barrier);
    harness.set_env("AOE_TMUX_SOCKET", harness.home_path().join("tmux.sock"));
    let mut size = nix::libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut master_fd, mut slave_fd) = (-1, -1);
    // SAFETY: openpty initializes owned descriptors using this valid window size.
    assert_eq!(
        unsafe {
            nix::libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::from_mut(&mut size),
            )
        },
        0
    );
    let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
    for file in [&master, &slave] {
        // SAFETY: these private descriptors must not leak through child exec.
        assert_eq!(
            unsafe {
                nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_SETFD, nix::libc::FD_CLOEXEC)
            },
            0
        );
    }
    let mut name = [0 as nix::libc::c_char; 4096];
    assert_eq!(
        unsafe { nix::libc::ttyname_r(slave_fd, name.as_mut_ptr(), name.len()) },
        0
    );
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .unwrap();
    let mut filler = OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(name)
        .unwrap();
    // SAFETY: this private master is not inherited by the child.
    let flags = unsafe { nix::libc::fcntl(master.as_raw_fd(), nix::libc::F_GETFL) };
    assert_eq!(
        unsafe {
            nix::libc::fcntl(
                master.as_raw_fd(),
                nix::libc::F_SETFL,
                flags | nix::libc::O_NONBLOCK,
            )
        },
        0
    );
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut command = harness.cli_command(&[]);
    command
        .env_remove("TMUX")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: only async-signal-safe session/TTY syscalls run before exec.
    unsafe {
        command.pre_exec(|| {
            if nix::libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = OwnedChild(command.spawn().unwrap());
    let mut initial = Vec::new();
    wait_until(Duration::from_secs(20), Duration::from_millis(10), || {
        let mut bytes = [0; 8192];
        loop {
            match master.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => initial.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.to_string()),
            }
        }
        if initial
            .windows(b"aoe-e2e-0".len())
            .any(|bytes| bytes == b"aoe-e2e-0")
        {
            Ok(())
        } else {
            Err(format!(
                "waiting for initial coordinated App frame: {:?}",
                child.0.try_wait().unwrap()
            ))
        }
    });
    // Stop output and prove no capacity remains before asking App to process another input.
    assert_eq!(
        unsafe { nix::libc::tcflow(slave.as_raw_fd(), nix::libc::TCOOFF) },
        0
    );
    loop {
        match filler.write(&[b'x'; 8192]) {
            Ok(0) => panic!("private terminal closed"),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("filling private terminal: {error}"),
        }
    }
    master.write_all(b"\x1b[24~").unwrap();
    wait_until(
        Duration::from_secs(3),
        Duration::from_millis(10),
        || match fs::read_to_string(&barrier) {
            Ok(sequence) if sequence.trim() == "1" => Ok(()),
            result => Err(format!(
                "input did not complete behind stalled draw: {result:?}"
            )),
        },
    );
    master.write_all(b"q").unwrap();
    wait_until(Duration::from_secs(3), Duration::from_millis(10), || {
        child
            .0
            .try_wait()
            .unwrap()
            .map(|status| {
                assert!(status.success(), "App exit failed: {status}");
            })
            .ok_or_else(|| "App exit blocked on permanently stalled terminal".into())
    });
}
