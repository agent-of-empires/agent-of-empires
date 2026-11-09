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
    wait_until(Duration::from_secs(10), Duration::from_millis(10), || {
        let pid = fs::read_to_string(&started)
            .map_err(|error| error.to_string())?
            .trim()
            .parse::<i32>()
            .map_err(|error| error.to_string())?;
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
            .map_err(|error| error.to_string())
    });
    harness.send_keys_unfenced("C-q");
    let beginning = Instant::now();
    let deadline = beginning + Duration::from_secs(3);
    let (responsive, last_screen) = loop {
        let screen = harness.capture_screen();
        if !screen.contains("LIVE") {
            break (true, screen);
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
