"""Build UI-state payloads for the Kanban board."""

from __future__ import annotations

import json
from typing import Any

from .board import Column
from .session_model import Session, STATUS_ORDER, STATUS_TONES


# The host caps pane/settings-page payloads at 64 KiB.
MAX_PAYLOAD_BYTES = 64 * 1024


def ui_state_set(
    slot: str,
    entry_id: str,
    payload: dict[str, Any],
    session_id: str | None = None,
) -> dict[str, Any]:
    msg: dict[str, Any] = {
        "jsonrpc": "2.0",
        "method": "ui.state.set",
        "params": {
            "slot": slot,
            "id": entry_id,
            "payload": payload,
        },
    }
    if session_id is not None:
        msg["params"]["session_id"] = session_id
    return msg


def ui_state_remove(
    slot: str,
    entry_id: str,
    session_id: str | None = None,
) -> dict[str, Any]:
    msg: dict[str, Any] = {
        "jsonrpc": "2.0",
        "method": "ui.state.remove",
        "params": {
            "slot": slot,
            "id": entry_id,
        },
    }
    if session_id is not None:
        msg["params"]["session_id"] = session_id
    return msg


def _clamp_text(value: str | None, max_len: int = 200) -> str | None:
    """Clamp long strings so a single row cannot blow the payload budget."""
    if value is None:
        return None
    if len(value) <= max_len:
        return value
    return value[: max_len - 1] + "…"


def _row_block(session: Session) -> dict[str, Any]:
    """Build a row block for a session inside a column."""
    sublabel_parts = [_clamp_text(session.tool)] if session.tool else []
    if session.project_path:
        sublabel_parts.append(_clamp_text(session.project_path))
    return {
        "kind": "row",
        "label": _clamp_text(session.title) or "Untitled",
        "sublabel": " · ".join(sublabel_parts) if sublabel_parts else None,
        "icon": "play" if session.kanban_status == "running" else "circle",
        "tone": STATUS_TONES.get(session.kanban_status, "neutral"),
    }


def build_settings_page_payload(
    grouping: str,
    columns: list[Column],
) -> dict[str, Any]:
    """Build the full payload for the settings-page slot."""
    column_blocks: list[dict[str, Any]] = []
    for column in columns:
        children = [_row_block(session) for session in column.sessions]
        if not children:
            children.append(
                {
                    "kind": "note",
                    "text": "No sessions in this column.",
                    "tone": "neutral",
                }
            )
        column_blocks.append(
            {
                "kind": "section",
                "title": f"{column.title} ({column.count})",
                "badges": [{"text": str(column.count), "tone": column.tone}],
                "boxed": True,
                "children": children,
            }
        )

    blocks: list[dict[str, Any]] = [
        {"kind": "heading", "text": "Session Board"},
        {
            "kind": "note",
            "tone": "info",
            "text": (
                "This board is read-only. Session rows are not clickable until "
                "the dashboard exposes a way for plugins to open a session."
            ),
        },
        {"kind": "note", "tone": "neutral", "text": f"Grouping: {grouping}"},
        {"kind": "columns", "children": column_blocks},
    ]

    payload: dict[str, Any] = {
        "title": "Kanban",
        "blocks": blocks,
    }
    return _truncate_settings_payload(payload)


def _count_rows(section: dict[str, Any]) -> int:
    return len([c for c in section.get("children", []) if c.get("kind") == "row"])


def _truncate_settings_payload(
    payload: dict[str, Any],
    max_bytes: int = MAX_PAYLOAD_BYTES,
) -> dict[str, Any]:
    """Trim the payload to fit within max_bytes.

    Keeps at least one row per non-empty column. Adds a note when truncation
    occurs. Performs a final size check after adding the warning and, if the
    payload is still too large, drops whole columns from the end.
    """
    encoded = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    if len(encoded) <= max_bytes:
        return payload

    blocks = list(payload.get("blocks", []))
    columns_block = None
    for block in blocks:
        if block.get("kind") == "columns":
            columns_block = block
            break

    if columns_block is None:
        return payload

    total_sessions = sum(
        _count_rows(section)
        for section in columns_block.get("children", [])
        if section.get("kind") == "section"
    )

    # Iteratively drop the last row from the longest column until we fit.
    while True:
        encoded = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        if len(encoded) <= max_bytes:
            break

        longest_section: dict[str, Any] | None = None
        longest_len = 0
        for section in columns_block.get("children", []):
            rows = _count_rows(section)
            if rows > longest_len:
                longest_len = rows
                longest_section = section

        if longest_section is None or longest_len <= 1:
            # Cannot trim further without dropping columns.
            break

        children = list(longest_section.get("children", []))
        # Drop the last row block (keep any note blocks).
        for i in range(len(children) - 1, -1, -1):
            if children[i].get("kind") == "row":
                children.pop(i)
                break
        longest_section["children"] = children
        # Update title/badge count.
        row_count = _count_rows(longest_section)
        title = longest_section.get("title", "")
        base = title.split(" (")[0] if " (" in title else title
        longest_section["title"] = f"{base} ({row_count})"
        badge = longest_section.get("badges", [{}])[0]
        badge["text"] = str(row_count)

    remaining = sum(
        _count_rows(section)
        for section in columns_block.get("children", [])
        if section.get("kind") == "section"
    )
    if remaining < total_sessions:
        blocks.append(
            {
                "kind": "note",
                "tone": "warn",
                "text": (
                    f"Showing {remaining} of {total_sessions} sessions; "
                    "reduce excluded columns or filter by repo to see more."
                ),
            }
        )
        payload["blocks"] = blocks

    # Final safety pass: if the warning note pushed us back over the limit,
    # drop columns from the end until we fit.
    while True:
        encoded = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        if len(encoded) <= max_bytes:
            break
        children = columns_block.get("children", [])
        if len(children) <= 1:
            break
        children.pop()

    return payload


def _sort_value(status: str) -> int:
    try:
        return STATUS_ORDER.index(status)
    except ValueError:
        return len(STATUS_ORDER)


def build_row_column_payload(session: Session) -> dict[str, Any]:
    status = session.kanban_status
    return {
        "text": status,
        "tone": STATUS_TONES.get(status, "neutral"),
        "tooltip": f"Kanban status: {status}",
        "sort_value": _sort_value(status),
        "filter_values": [status],
    }


def build_sort_key_payload() -> dict[str, Any]:
    return {
        "label": "Kanban status",
        "column": "kanban_status",
        "direction": "asc",
    }


def build_filter_facet_payload() -> dict[str, Any]:
    return {
        "label": "Kanban status",
        "column": "kanban_status",
        "options": [
            {"value": status, "label": status.capitalize(), "tone": STATUS_TONES.get(status, "neutral")}
            for status in STATUS_ORDER
        ],
    }
