"""Tests for session_model."""

import pytest

from aoe_kanban_lite.session_model import normalize_status, repo_name, session_from_json


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("Running", "running"),
        ("Waiting", "waiting"),
        ("Idle", "idle"),
        ("Error", "error"),
        ("Starting", "starting"),
        ("Stopped", "stopped"),
        ("Unknown", "unknown"),
        ("Deleting", "deleting"),
        ("Creating", "creating"),
        ("InstanceStatus::Running", "running"),
        ("SomethingNew", "unknown"),
        ("", "unknown"),
    ],
)
def test_normalize_status(raw, expected):
    assert normalize_status(raw) == expected


def test_repo_name_with_path():
    assert repo_name("/Users/karl/work/sec-etf") == "sec-etf"


def test_repo_name_empty():
    assert repo_name(None) == "Scratch / no repo"
    assert repo_name("") == "Scratch / no repo"


def test_session_from_json():
    data = {
        "id": "sess-1",
        "title": "My session",
        "project_path": "/path/to/repo",
        "tool": "claude",
        "status": "Running",
        "archived": False,
        "snoozed": False,
    }
    session = session_from_json(data)
    assert session.id == "sess-1"
    assert session.kanban_status == "running"
    assert session.repo_name == "repo"
