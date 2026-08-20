"""Group sessions into Kanban columns."""

from __future__ import annotations

from dataclasses import dataclass, field

from .session_model import Session, STATUS_ORDER, STATUS_TONES


@dataclass(frozen=True)
class Column:
    title: str
    tone: str
    sessions: list[Session] = field(default_factory=list)

    @property
    def count(self) -> int:
        return len(self.sessions)


def parse_excluded_statuses(raw: str) -> set[str]:
    """Parse a comma-separated string of statuses into a normalized set."""
    return {piece.strip().lower() for piece in raw.split(",") if piece.strip()}


def group_by_status(sessions: list[Session], excluded: set[str]) -> list[Column]:
    """Group sessions into fixed status columns, omitting excluded ones."""
    # Build a map for quick lookup while preserving order.
    groups: dict[str, list[Session]] = {status: [] for status in STATUS_ORDER}
    for session in sessions:
        status = session.kanban_status
        groups.setdefault(status, []).append(session)

    columns: list[Column] = []
    for status in STATUS_ORDER:
        if status in excluded:
            continue
        sessions_in_group = groups.get(status, [])
        sessions_in_group.sort(key=lambda s: s.title.lower())
        columns.append(
            Column(
                title=status.capitalize(),
                tone=STATUS_TONES.get(status, "neutral"),
                sessions=sessions_in_group,
            )
        )
    return columns


def group_by_repo(sessions: list[Session], max_columns: int = 12) -> list[Column]:
    """Group sessions by repository, capped at max_columns with overflow."""
    groups: dict[str, list[Session]] = {}
    scratch: list[Session] = []

    for session in sessions:
        repo = session.repo_name
        if repo == "Scratch / no repo":
            scratch.append(session)
        else:
            groups.setdefault(repo, []).append(session)

    # Sort repo names alphabetically; scratch goes first.
    sorted_repos = sorted(groups.keys(), key=str.lower)

    def make_column(title: str, sessions_in_group: list[Session]) -> Column:
        sessions_in_group.sort(key=lambda s: s.title.lower())
        return Column(title=title, tone="neutral", sessions=sessions_in_group)

    columns: list[Column] = []
    # Reserve one column for Scratch so total columns never exceed max_columns.
    repo_capacity = max_columns - 1 if scratch else max_columns

    if scratch:
        columns.append(make_column("Scratch / no repo", scratch))

    if len(sorted_repos) <= repo_capacity:
        for repo in sorted_repos:
            columns.append(make_column(repo, groups[repo]))
    else:
        for repo in sorted_repos[: repo_capacity - 1]:
            columns.append(make_column(repo, groups[repo]))
        overflow: list[Session] = []
        for repo in sorted_repos[repo_capacity - 1 :]:
            overflow.extend(groups[repo])
        columns.append(make_column(f"More repos ({len(overflow)})", overflow))

    return columns
