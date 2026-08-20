"""Tests for uistate payload construction."""

import json

from aoe_kanban_lite.board import Column, group_by_status
from aoe_kanban_lite.session_model import Session
from aoe_kanban_lite.uistate import (
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
