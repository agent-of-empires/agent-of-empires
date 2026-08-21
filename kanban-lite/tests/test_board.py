"""Tests for board grouping."""

from aoe_kanban_lite.board import group_by_repo, group_by_status, parse_excluded_statuses
from aoe_kanban_lite.session_model import Session


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


def test_group_by_status_orders_columns():
    sessions = [
        _session("1", "A", "Idle"),
        _session("2", "B", "Running"),
        _session("3", "C", "Waiting"),
    ]
    columns = group_by_status(sessions, set())
    titles = [c.title for c in columns]
    assert titles == ["Running", "Waiting", "Idle", "Error", "Starting", "Stopped", "Unknown", "Deleting", "Creating"]


def test_group_by_status_excludes_columns():
    sessions = [_session("1", "A", "Running"), _session("2", "B", "Stopped")]
    columns = group_by_status(sessions, {"stopped"})
    titles = [c.title for c in columns]
    assert "Stopped" not in titles
    running = next(c for c in columns if c.title == "Running")
    assert len(running.sessions) == 1


def test_group_by_status_sorts_sessions_by_title():
    sessions = [
        _session("1", "Zebra", "Running"),
        _session("2", "Alpha", "Running"),
    ]
    columns = group_by_status(sessions, set())
    running = next(c for c in columns if c.title == "Running")
    assert [s.title for s in running.sessions] == ["Alpha", "Zebra"]


def test_group_by_repo():
    sessions = [
        _session("1", "A", "Running", "/work/repo-a"),
        _session("2", "B", "Idle", "/work/repo-b"),
        _session("3", "C", "Running", "/work/repo-a"),
        _session("4", "D", "Waiting", None),
    ]
    columns = group_by_repo(sessions)
    titles = [c.title for c in columns]
    assert titles == ["Scratch / no repo", "repo-a", "repo-b"]
    repo_a = next(c for c in columns if c.title == "repo-a")
    assert len(repo_a.sessions) == 2


def test_group_by_repo_overflow():
    sessions = [_session(str(i), f"S{i}", "Running", f"/work/repo-{i}") for i in range(15)]
    columns = group_by_repo(sessions, max_columns=5)
    assert len(columns) == 5
    assert columns[-1].title.startswith("More repos")
    assert len(columns[-1].sessions) == 15 - 4


def test_group_by_repo_scratch_counts_toward_max_columns():
    sessions = [_session(str(i), f"S{i}", "Running", f"/work/repo-{i}") for i in range(12)]
    sessions.append(_session("scratch", "No repo", "Idle", None))
    columns = group_by_repo(sessions)  # default max_columns=12
    assert len(columns) == 12
    assert columns[0].title == "Scratch / no repo"
    assert columns[-1].title.startswith("More repos")


def test_group_by_repo_single_column_combines_scratch_and_repos():
    sessions = [
        _session("1", "A", "Running", "/work/repo-a"),
        _session("2", "B", "Idle", "/work/repo-b"),
        _session("3", "C", "Waiting", None),
    ]
    columns = group_by_repo(sessions, max_columns=1)
    assert len(columns) == 1
    assert columns[0].title == "Sessions"
    assert len(columns[0].sessions) == 3


def test_parse_excluded_statuses():
    assert parse_excluded_statuses("stopped, unknown") == {"stopped", "unknown"}
    assert parse_excluded_statuses("") == set()
