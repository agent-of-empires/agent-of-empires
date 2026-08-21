"""Session model for GitHub Lite."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True, slots=True)
class Session:
    id: str
    title: str
    project_path: str | None
    status: str
    archived: bool
    snoozed: bool

    @property
    def repo_name(self) -> str | None:
        """Best-effort repo name derived from the project path."""
        if not self.project_path:
            return None
        import os

        return os.path.basename(os.path.normpath(self.project_path)) or None


def session_from_json(data: dict[str, Any]) -> Session:
    return Session(
        id=data["id"],
        title=data.get("title", "Untitled"),
        project_path=data.get("project_path") or None,
        status=data.get("status", ""),
        archived=bool(data.get("archived", False)),
        snoozed=bool(data.get("snoozed", False)),
    )
