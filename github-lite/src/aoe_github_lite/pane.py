"""Build pane UI-state payloads for GitHub pull requests."""

from __future__ import annotations

from typing import Any

from .github import PullRequest
from .session_model import Session


def ui_state_set(session_id: str, payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "method": "ui.state.set",
        "params": {
            "slot": "pane",
            "id": "github_prs",
            "session_id": session_id,
            "payload": payload,
        },
    }


def ui_state_remove(session_id: str) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "method": "ui.state.remove",
        "params": {
            "slot": "pane",
            "id": "github_prs",
            "session_id": session_id,
        },
    }


def _relative_time(created_at: str) -> str:
    """Crude relative time for display; full ISO string on parse failure."""
    if not created_at:
        return ""
    try:
        from datetime import datetime, timezone

        dt = datetime.fromisoformat(created_at.replace("Z", "+00:00"))
        delta = datetime.now(timezone.utc) - dt
        if delta.days > 1:
            return f"{delta.days} days ago"
        if delta.days == 1:
            return "1 day ago"
        hours = delta.seconds // 3600
        if hours > 1:
            return f"{hours} hours ago"
        if hours == 1:
            return "1 hour ago"
        minutes = delta.seconds // 60
        return f"{minutes} min ago" if minutes > 1 else "just now"
    except ValueError:
        return created_at


def build_pane_payload(session: Session, slug: str | None, prs: list[PullRequest]) -> dict[str, Any]:
    """Build the per-session pane payload."""
    blocks: list[dict[str, Any]] = [
        {
            "kind": "heading",
            "text": "Pull Requests",
        }
    ]

    if slug is None:
        blocks.append(
            {
                "kind": "note",
                "tone": "neutral",
                "text": "No project path is set for this session.",
            }
        )
        return {"title": "GitHub", "default_location": "right", "blocks": blocks}

    blocks.append(
        {
            "kind": "note",
            "tone": "info",
            "text": f"Repository: {slug}",
        }
    )

    if not prs:
        blocks.append(
            {
                "kind": "note",
                "tone": "neutral",
                "text": "No open pull requests.",
            }
        )
        return {"title": "GitHub", "default_location": "right", "blocks": blocks}

    for pr in prs:
        sublabel_parts = [f"#{pr.number}"]
        if pr.author:
            sublabel_parts.append(f"by {pr.author}")
        sublabel_parts.append(_relative_time(pr.created_at))
        tone = "neutral"
        if pr.draft:
            tone = "warn"
        elif pr.state == "open":
            tone = "success"
        blocks.append(
            {
                "kind": "row",
                "label": pr.title,
                "sublabel": " · ".join(sublabel_parts),
                "icon": "git-pull-request",
                "tone": tone,
                "href": pr.html_url,
            }
        )

    return {"title": "GitHub", "default_location": "right", "blocks": blocks}
