"""Tests for uistate payload construction."""

import json

from aoe_kanban_lite.board import Column, group_by_repo, group_by_status
from aoe_kanban_lite.session_model import Session
from aoe_kanban_lite.uistate import (
    _truncate_settings_payload,
    build_filter_facet_payload,
    build_row_column_payload,
    build_settings_page_payload,
    build_sort_key_payload,
)


def _session(sid, title, status, project_path=None):
    return Session(
        id=sid,
        title=title,
        project_path=project_path,
        tool="claude",
        status=status,
        archived=False,
        snoozed=False,
    )


def test_build_settings_page_payload_shape():
    sessions = [
        _session("1", "Alpha", "Running", "/work/repo-a"),
        _session("2", "Beta", "Idle", "/work/repo-b"),
    ]
    columns = group_by_status(sessions, {"stopped"})
    payload = build_settings_page_payload("status", columns)

    assert payload["title"] == "Kanban"
    blocks = payload["blocks"]
    assert blocks[0]["kind"] == "heading"
    assert blocks[1]["kind"] == "note"
    assert blocks[-1]["kind"] == "columns"

    columns_block = blocks[-1]
    running = next(c for c in columns_block["children"] if c["title"].startswith("Running"))
    assert running["children"][0]["kind"] == "row"


def test_row_column_payload_has_sort_value():
    session = _session("1", "A", "Running")
    payload = build_row_column_payload(session)
    assert payload["text"] == "running"
    assert payload["sort_value"] == 0
    assert payload["filter_values"] == ["running"]


def test_sort_key_payload():
    payload = build_sort_key_payload()
    assert payload["column"] == "kanban_status"
    assert payload["direction"] == "asc"


def test_filter_facet_payload_covers_all_statuses():
    payload = build_filter_facet_payload()
    values = {opt["value"] for opt in payload["options"]}
    expected = {"running", "waiting", "idle", "error", "starting", "stopped", "unknown", "deleting", "creating"}
    assert values == expected


def test_settings_page_payload_fits_64k():
    sessions = [_session(str(i), f"Session {i}", "Running", f"/work/repo-{i}") for i in range(500)]
    columns = group_by_status(sessions, set())
    payload = build_settings_page_payload("status", columns)
    encoded = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    assert len(encoded) <= 64 * 1024


def test_settings_page_payload_clamps_oversized_fields():
    long_title = "x" * 10_000
    long_path = "/work/" + "y" * 10_000
    sessions = [_session("1", long_title, "Running", long_path)]
    columns = group_by_status(sessions, set())
    payload = build_settings_page_payload("status", columns)
    encoded = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    assert len(encoded) <= 64 * 1024


def test_settings_page_payload_adds_truncation_warning():
    sessions = [_session(str(i), f"Session {i}", "Running", f"/work/repo-{i}") for i in range(2000)]
    columns = group_by_status(sessions, set())
    payload = build_settings_page_payload("status", columns)
    encoded = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    assert len(encoded) <= 64 * 1024
    # The warning note is only appended when rows were dropped.
    kinds = [b["kind"] for b in payload["blocks"]]
    assert "note" in kinds
    notes = [b for b in payload["blocks"] if b["kind"] == "note"]
    assert any("Showing" in b.get("text", "") for b in notes)


def test_settings_page_payload_preserves_title_parentheses():
    """Regression: repo titles like 'api (legacy)' must keep the suffix."""
    sessions = [
        _session("1", "A", "Running", "/work/api"),
        _session("2", "B", "Idle", "/work/api"),
    ]
    columns = group_by_repo(sessions)
    api_column = next(c for c in columns if "api" in c.title)
    payload = build_settings_page_payload("repo", [api_column])
    section = payload["blocks"][-1]["children"][0]
    assert section["title"].startswith("api (")
    assert section["title"].endswith("(2)")


def test_truncation_warning_count_updates_after_column_drop():
    """If whole columns are dropped, the warning must report the final count."""
    sessions = [_session(str(i), f"S{i}", "Running", f"/work/repo-{i}") for i in range(50)]
    columns = group_by_repo(sessions, max_columns=40)
    payload = build_settings_page_payload("repo", columns)
    # Force a small byte budget so the final safety pass drops whole columns.
    truncated = _truncate_settings_payload(payload, max_bytes=2000)
    encoded = json.dumps(truncated, separators=(",", ":")).encode("utf-8")
    assert len(encoded) <= 2000

    notes = [b for b in truncated["blocks"] if b["kind"] == "note" and "Showing" in b.get("text", "")]
    assert len(notes) == 1
    warning = notes[0]["text"]
    # Extract the displayed count and ensure it is less than 50.
    import re

    match = re.search(r"Showing (\d+) of 50", warning)
    assert match is not None
    displayed = int(match.group(1))
    assert displayed < 50
