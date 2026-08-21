"""Tests for pane payload construction."""

from __future__ import annotations

from aoe_github_lite.github import PullRequest
from aoe_github_lite.pane import build_pane_payload
from aoe_github_lite.session_model import Session


def _session(sid, title, project_path=None):
    return Session(
        id=sid,
        title=title,
        project_path=project_path,
        status="Running",
        archived=False,
        snoozed=False,
    )


def test_build_pane_payload_no_path():
    payload = build_pane_payload(_session("s1", "A"), None, [])
    assert payload["title"] == "GitHub"
    assert payload["default_location"] == "right"
    assert any("No project path" in b.get("text", "") for b in payload["blocks"])


def test_build_pane_payload_empty_prs():
    payload = build_pane_payload(_session("s1", "A", "/work/app"), "acme/app", [])
    assert any("No open pull requests" in b.get("text", "") for b in payload["blocks"])


def test_build_pane_payload_with_prs():
    prs = [
        PullRequest(
            number=1,
            title="Fix bug",
            state="open",
            author="bob",
            html_url="https://github.com/acme/app/pull/1",
            draft=False,
            created_at="2026-08-20T10:00:00Z",
        )
    ]
    payload = build_pane_payload(_session("s1", "A", "/work/app"), "acme/app", prs)
    rows = [b for b in payload["blocks"] if b.get("kind") == "row"]
    assert len(rows) == 1
    assert rows[0]["label"] == "Fix bug"
    assert rows[0]["href"] == "https://github.com/acme/app/pull/1"


def test_build_pane_payload_draft_tone():
    prs = [
        PullRequest(
            number=2,
            title="WIP",
            state="open",
            author="bob",
            html_url="https://github.com/acme/app/pull/2",
            draft=True,
            created_at="2026-08-20T10:00:00Z",
        )
    ]
    payload = build_pane_payload(_session("s1", "A", "/work/app"), "acme/app", prs)
    rows = [b for b in payload["blocks"] if b.get("kind") == "row"]
    assert rows[0]["tone"] == "warn"
