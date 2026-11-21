#!/usr/bin/env python3
"""Time one-shot aoe commands on an isolated HOME and tmux socket (epic #5787).

    python3 scripts/bench-oneshot.py --aoe target/dev-release/aoe --bench ps
    python3 scripts/bench-oneshot.py --aoe target/dev-release/aoe --bench list
    python3 scripts/bench-oneshot.py --aoe target/dev-release/aoe --bench stop

ps:   `aoe ps` with --sessions Terminal sessions; also counts tmux execs.
list: `aoe list` with an update endpoint that accepts and never answers.
stop: `aoe session stop` of a running Sandbox session (needs docker and the
      sandbox image); each run restarts the session untimed first.

Run the same command against a baseline binary and the changed binary, both
built with `cargo build --profile dev-release`. The daemon refuses a binary not
named `aoe`, so keep each copy in its own directory. Needs Python 3.11+ and tmux.
"""

import argparse
import json
import math
import os
from pathlib import Path
import shutil
import socket
import statistics
import subprocess
import tempfile
import threading
import time


def percentile(values, pct):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(pct / 100 * len(ordered)) - 1)]


# `--cmd` must name a supported agent; this keeps a shell pane alive.
AGENT_CMD = "sh -c 'echo claude; exec sh'"


class Bench:
    def __init__(self, aoe):
        self.aoe = str(Path(aoe).resolve())
        self.root = Path(tempfile.mkdtemp(prefix="ab", dir="/tmp"))
        self.log = self.root / "tmux.log"
        self.socket = str(self.root / "tmux.sock")
        shim = self.root / "bin"
        shim.mkdir()
        (shim / "tmux").write_text(
            f'#!/bin/sh\necho x >> "{self.log}"\nexec "{shutil.which("tmux")}" "$@"\n'
        )
        (shim / "tmux").chmod(0o755)
        self.env = dict(
            os.environ,
            HOME=str(self.root / "home"),
            AOE_TMUX_SOCKET=self.socket,
            PATH=f"{shim}:{os.environ['PATH']}",
        )
        (self.root / "home").mkdir()
        self.project = self.root / "project"
        self.project.mkdir()
        subprocess.run(["git", "init", "-q", str(self.project)], check=True)

    def aoe_cmd(self, *args, check=True):
        result = subprocess.run(
            [self.aoe, *args], env=self.env, capture_output=True, text=True
        )
        if check and result.returncode:
            raise RuntimeError(f"aoe {' '.join(args)}: {result.stderr.strip()}")
        return result

    def time_cmd(self, *args):
        start = time.perf_counter()
        self.aoe_cmd(*args)
        return (time.perf_counter() - start) * 1000

    def tmux_execs(self):
        return len(self.log.read_text().splitlines()) if self.log.exists() else 0

    def close(self):
        self.aoe_cmd("session", "stop", "sandboxed", check=False)
        self.aoe_cmd("serve", "--stop", check=False)
        subprocess.run(
            [shutil.which("tmux"), "-S", self.socket, "kill-server"], capture_output=True
        )
        shutil.rmtree(self.root, ignore_errors=True)


def summarize(name, samples, extra):
    result = {
        "bench": name,
        "runs": len(samples),
        "p50_ms": round(statistics.median(samples), 1),
        "p95_ms": round(percentile(samples, 95), 1),
        "samples_ms": [round(s, 1) for s in samples],
        **extra,
    }
    print(json.dumps(result, indent=2))


def bench_ps(bench, runs, sessions):
    bench.aoe_cmd("hooks", "approve")
    for i in range(sessions):
        bench.aoe_cmd("add", str(bench.project), "-t", f"s{i}", "--cmd", AGENT_CMD, "--launch")
    bench.time_cmd("ps")
    samples, execs = [], []
    for _ in range(runs):
        before = bench.tmux_execs()
        samples.append(bench.time_cmd("ps"))
        execs.append(bench.tmux_execs() - before)
    summarize("ps", samples, {"sessions": sessions, "tmux_execs_per_run": execs})


def bench_list(bench, runs):
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(64)
    held = []
    threading.Thread(
        target=lambda: [held.append(listener.accept()[0]) for _ in iter(int, 1)],
        daemon=True,
    ).start()
    bench.env["AOE_UPDATE_API_BASE"] = f"http://127.0.0.1:{listener.getsockname()[1]}"
    bench.aoe_cmd("add", str(bench.project), "-t", "s0", "--cmd", AGENT_CMD)
    bench.time_cmd("list")
    summarize("list", [bench.time_cmd("list") for _ in range(runs)], {})


def bench_stop(bench, runs):
    bench.aoe_cmd(
        "add", str(bench.project), "-t", "sandboxed", "--cmd", AGENT_CMD, "--sandbox", "--launch"
    )
    samples = []
    for _ in range(runs):
        bench.aoe_cmd("session", "start", "sandboxed")
        samples.append(bench.time_cmd("session", "stop", "sandboxed"))
    summarize("stop", samples, {})


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--aoe", required=True)
    parser.add_argument("--bench", required=True, choices=["ps", "list", "stop"])
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--sessions", type=int, default=30)
    args = parser.parse_args()
    bench = Bench(args.aoe)
    try:
        if args.bench == "ps":
            bench_ps(bench, args.runs, args.sessions)
        elif args.bench == "list":
            bench_list(bench, args.runs)
        else:
            bench_stop(bench, args.runs)
    finally:
        bench.close()


if __name__ == "__main__":
    main()
