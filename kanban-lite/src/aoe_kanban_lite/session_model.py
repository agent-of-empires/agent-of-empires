"""Session model and status normalization for the Kanban board."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any


# Column display order and sort rank.
STATUS_ORDER = [
    "running",
    "waiting",
    "idle",
    "error",
    "starting",
    "stopped",
    "unknown",
    "deleting",
    "creating",
]

STATUS_TONES = {
    "running": "success",
    "starting": "info",
    "creating": "info",
    "waiting": "warn",
    "idle": "neutral",
    "error": "danger",
    "stopped": "neutral",
    "deleting": "neutral",
    "unknown": "neutral",
}


@dataclass(frozen=True, slots=True)
class Session:
    id: str
    title: str
    project_path: str | None
    tool: str
    status: str  # raw Status Debug string from sessions.list
    archived: bool
    snoozed: bool

    @property
    def kanban_status(self) -> str:
        return normalize_status(self.status)

    @property
    def repo_name(self) -> str:
        return repo_name(self.project_path)


def normalize_status(raw: str) -> str:
    """Map a Rust Status Debug string to a stable Kanban status category.

    The host currently emits the Debug representation of `Status` (e.g.
    "Running"). We map known values defensively and fall back to "unknown".
    """
    normalized = raw.strip().lower()
    # Handle both "Running" and "InstanceStatus::Running" forms defensively.
    for status in STATUS_ORDER:
        if status in normalized:
            return status
    return "unknown"


def repo_name(project_path: str | None) -> str:
    """Return a display name for a project path."""
    if not project_path:
        return "Scratch / no repo"
    import os

    return os.path.basename(os.path.normpath(project_path)) or project_path


def session_from_json(data: dict[str, Any]) -> Session:
    return Session(
        id=data["id"],
        title=data.get("title", "Untitled"),
        project_path=data.get("project_path") or None,
        tool=data.get("tool", ""),
        status=data.get("status", ""),
        archived=bool(data.get("archived", False)),
        snoozed=bool(data.get("snoozed", False)),
    )
