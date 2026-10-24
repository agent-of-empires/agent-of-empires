#!/usr/bin/env python3
"""Measure spawn-to-first-frame and first-frame-to-quit on an isolated PTY.

Build with `cargo build --profile dev-release`, then run:
    python3 scripts/bench-startup.py --runs 20 --sessions 100 --profiles 4
    python3 scripts/bench-startup.py --upgrade --stall-updates --slow-probes

For a build with debug assertions enabled, add `--namespace dev`.

Requires Python 3.11+, Unix, and tmux. Uses synthetic data, a private HOME and
tmux socket, no telemetry, and optionally a local HTTP endpoint that never
responds. Every launch, including the first, counts against the budget.
"""

import argparse
import fcntl
import json
import math
import os
from pathlib import Path
import pty
import re
import select
import signal
import socket
import statistics
import struct
import subprocess
import tempfile
import termios
import threading
import time
import tomllib
import uuid

ROOT = Path(__file__).resolve().parent.parent
FRAME_END = b"\x1b[?2026l"


def positive(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def environment(home):
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("AOE_") or key in {
            "AGENT_OF_EMPIRES_DEBUG", "TMUX", "TMUX_PANE", "MOSH_CONNECTION",
            "CLAUDE_CONFIG_DIR", "CODEX_HOME", "CURSOR_CONFIG_DIR",
            "GEMINI_CONFIG_DIR", "QWEN_CONFIG_DIR",
        }:
            env.pop(key)
    env.update(
        HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
        XDG_DATA_HOME=str(home / ".local/share"),
        XDG_CACHE_HOME=str(home / ".cache"),
        AOE_TMUX_SOCKET=f"startup-bench-{uuid.uuid4().hex}",
        TERM="xterm-256color", NO_PROXY="127.0.0.1,localhost",
    )
    return env


def fixture(app, home, sessions, profiles, version, upgrade):
    app.mkdir(parents=True, exist_ok=True)
    schema = re.search(r"const CURRENT_VERSION: u32 = (\d+);",
                       (ROOT / "src/migrations/mod.rs").read_text())[1]
    (app / ".schema_version").write_text(schema)
    (app / "config.toml").write_text(
        '[telemetry]\nenabled = false\n[systemd]\nenabled = false\n'
        '[session]\nconfirm_before_quit = false\n'
        'custom_agents = { startup-bench = "/usr/bin/false" }\n'
    )
    previous = "0.0.0" if upgrade else version
    (app / "state.toml").write_text(
        f'has_seen_welcome = true\nlast_seen_version = "{previous}"\n'
        'has_responded_to_telemetry = true\n'
    )
    for index in range(profiles):
        profile = app / "profiles" / ("default" if index == 0 else f"bench-{index}")
        profile.mkdir(parents=True, exist_ok=True)
        rows = [dict(
            id=str(uuid.uuid5(uuid.NAMESPACE_OID, f"startup-bench-{row}")),
            title=f"Session {row}", project_path=str(home / "project"),
            tool="startup-bench", status="stopped", created_at="2026-01-01T00:00:00Z",
        ) for row in range(index, sessions, profiles)]
        (profile / "sessions.json").write_text(json.dumps(rows))


def terminate(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def run(binary, env, wait_for_background, timeout):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    process = None
    try:
        started = time.perf_counter()
        process = subprocess.Popen([str(binary)], stdin=slave, stdout=slave,
                                   stderr=slave, env=env, start_new_session=True)
        os.close(slave)
        slave = None
        output = b""
        deadline = started + timeout
        while FRAME_END not in output:
            remaining = deadline - time.perf_counter()
            if remaining <= 0 or not select.select([master], [], [], remaining)[0]:
                raise RuntimeError("first frame timed out")
            try:
                chunk = os.read(master, 65536)
            except OSError as error:
                raise RuntimeError("exited before first frame") from error
            if not chunk:
                raise RuntimeError("exited before first frame")
            output += chunk
            if b"\x1b[6n" in chunk:
                os.write(master, b"\x1b[1;1R")
        first_frame = (time.perf_counter() - started) * 1000
        wait_for_background()
        # Dismiss the upgrade changelog, then quit. Fixtures disable confirmation.
        quit_started = time.perf_counter()
        os.write(master, b"qq")
        deadline = quit_started + timeout
        while process.poll() is None:
            if time.perf_counter() >= deadline:
                raise RuntimeError("quit timed out after the first frame")
            if select.select([master], [], [], 0.01)[0]:
                try:
                    os.read(master, 65536)
                except OSError:
                    try:
                        process.wait(timeout=max(0.01, deadline - time.perf_counter()))
                    except subprocess.TimeoutExpired:
                        raise RuntimeError("quit timed out after the first frame") from None
        if process.returncode != 0:
            raise RuntimeError(f"exited with status {process.returncode}")
        return first_frame, (time.perf_counter() - quit_started) * 1000
    finally:
        if process is not None:
            terminate(process)
        os.close(master)
        if slave is not None:
            os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/dev-release/aoe")
    parser.add_argument("--runs", type=positive, default=10)
    parser.add_argument("--sessions", type=int, default=0)
    parser.add_argument("--profiles", type=positive, default=1)
    parser.add_argument("--budget-ms", type=positive, default=300)
    parser.add_argument("--upgrade", action="store_true")
    parser.add_argument("--stall-updates", action="store_true")
    parser.add_argument("--slow-probes", action="store_true")
    parser.add_argument("--namespace", choices=["release", "dev"], default="release")
    args = parser.parse_args()
    if args.sessions < 0:
        parser.error("sessions cannot be negative")
    binary = args.binary.expanduser().resolve(strict=True)
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    # /private avoids macOS's /var symlink and FSEvents path mismatches.
    temp_base = Path(tempfile.gettempdir()).resolve()
    with tempfile.TemporaryDirectory(prefix="aoe-startup-", dir=temp_base) as directory:
        home = Path(directory)
        env = environment(home)
        app_name = "agent-of-empires-dev" if args.namespace == "dev" else "agent-of-empires"
        app = home / ".config" / app_name
        other_name = "agent-of-empires" if args.namespace == "dev" else "agent-of-empires-dev"
        (home / "project").mkdir()
        accepted = []
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(0.1)
        stopping = threading.Event()
        contacted = threading.Event()

        def accept_connections():
            while not stopping.is_set():
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                accepted.append(connection)
                contacted.set()

        thread = threading.Thread(target=accept_connections, daemon=True)
        thread.start()
        if args.stall_updates:
            env["AOE_UPDATE_API_BASE"] = f"http://127.0.0.1:{listener.getsockname()[1]}"
        children = home / "probe-pids"
        if args.slow_probes:
            bindir = home / "bin"
            bindir.mkdir()
            fifo = home / "probe-gate"
            os.mkfifo(fifo)
            script = f'#!/bin/sh\necho $$ >> "{children}"\nread gate < "{fifo}"\n'
            for name in ["vibe", "login-shell"]:
                executable = bindir / name
                executable.write_text(script)
                executable.chmod(0o755)
            env.update(PATH=f"{bindir}:{env.get('PATH', '')}", SHELL=str(bindir / "login-shell"))

        def wait_for_background():
            if (home / ".config" / other_name).exists():
                raise RuntimeError("binary used the wrong namespace; set --namespace to match its build")
            if args.stall_updates and not contacted.wait(timeout=2):
                raise RuntimeError("background update check did not contact fixture endpoint")
            if args.slow_probes:
                deadline = time.perf_counter() + 2
                while not children.exists() or not children.read_text().strip():
                    if time.perf_counter() >= deadline:
                        raise RuntimeError("background probe did not reach fixture gate")
                    stopping.wait(0.005)

        measurements = []
        failed = False
        try:
            for index in range(args.runs):
                fixture(app, home, args.sessions, args.profiles, version, args.upgrade)
                contacted.clear()
                try:
                    first, quit_ms = run(binary, env, wait_for_background, 10)
                    if children.exists():
                        for pid in children.read_text().splitlines():
                            try:
                                os.killpg(int(pid), 0)
                            except ProcessLookupError:
                                continue
                            raise RuntimeError("agent probe survived TUI shutdown")
                    measurements.append(first)
                    passed = first < args.budget_ms and quit_ms < args.budget_ms
                    print(f"{index + 1:2}: first_frame={first:7.2f}ms quit={quit_ms:7.2f}ms "
                          f"{'PASS' if passed else 'FAIL'}", flush=True)
                    failed |= not passed
                except RuntimeError as error:
                    print(f"{index + 1:2}: FAIL: {error}", flush=True)
                    failed = True
                finally:
                    if children.exists():
                        for pid in children.read_text().splitlines():
                            try:
                                os.killpg(int(pid), signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                        children.unlink()
                    for connection in accepted:
                        connection.close()
                    accepted.clear()
            if measurements:
                ordered = sorted(measurements)
                p95 = ordered[math.ceil(0.95 * len(ordered)) - 1]
                print(f"First frame: median={statistics.median(ordered):.2f}ms "
                      f"p95={p95:.2f}ms max={max(ordered):.2f}ms")
        finally:
            stopping.set()
            thread.join(timeout=1)
            listener.close()
            for connection in accepted:
                connection.close()
        return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
