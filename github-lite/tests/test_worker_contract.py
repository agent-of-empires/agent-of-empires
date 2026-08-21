"""Worker contract test: drive the worker via stdio."""

from __future__ import annotations

import json
import select
import subprocess
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
WORKER = ROOT / ".aoe-build" / "venv" / "bin" / "aoe-github-lite-worker"

CONFIG_VALUES = {
    "default_owner": "",
    "repo_overrides": "",
    "refresh_secs": 60,
}

SAMPLE_SESSIONS = {
    "sessions": [
        {
            "id": "sess-1",
            "title": "First session",
            "project_path": "/work/app",
            "status": "Running",
            "archived": False,
            "snoozed": False,
        }
    ]
}


@pytest.fixture(scope="module")
def worker_path() -> Path:
    if not WORKER.exists():
        pytest.skip("worker not built yet; run pip install in the plugin directory")
    return WORKER


def _read_line(proc: subprocess.Popen, deadline: float = 5.0) -> dict | None:
    end = time.monotonic() + deadline
    while True:
        remaining = end - time.monotonic()
        if remaining <= 0:
            break
        ready, _, _ = select.select([proc.stdout], [], [], min(remaining, 0.01))
        if not ready:
            continue
        line = proc.stdout.readline()
        if line:
            line = line.strip()
            if line:
                return json.loads(line)
        elif line == "":
            return None
    raise TimeoutError(f"no worker output within {deadline}s")


def _write_line(proc: subprocess.Popen, msg: dict) -> None:
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()


def _reply_to_config_get(proc: subprocess.Popen, msg: dict) -> None:
    key = msg["params"]["key"]
    _write_line(
        proc,
        {
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {"value": CONFIG_VALUES[key]},
        },
    )


def test_worker_startup_and_refresh(worker_path: Path) -> None:
    proc = subprocess.Popen(
        [str(worker_path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )

    try:
        for _ in range(len(CONFIG_VALUES)):
            msg = _read_line(proc)
            assert msg is not None
            assert msg["method"] == "config.get"
            _reply_to_config_get(proc, msg)

        msg = _read_line(proc)
        assert msg is not None
        assert msg["method"] == "sessions.list"
        sessions_id = msg["id"]

        _write_line(
            proc,
            {"jsonrpc": "2.0", "id": sessions_id, "result": SAMPLE_SESSIONS},
        )

        # The worker should emit a pane ui.state.set for the session.
        msg = _read_line(proc)
        assert msg is not None
        assert msg["method"] == "ui.state.set"
        params = msg["params"]
        assert params["slot"] == "pane"
        assert params["id"] == "github_prs"
        assert params["session_id"] == "sess-1"
        assert params["payload"]["title"] == "GitHub"

    finally:
        proc.stdin.close()
        proc.terminate()
        proc.wait(timeout=5)
