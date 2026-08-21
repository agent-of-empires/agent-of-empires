"""Tests for GitHub API helpers."""

from __future__ import annotations

import json
import urllib.request
from unittest import mock

import pytest

from aoe_github_lite.github import PullRequest, list_open_prs, parse_overrides, parse_repo_slug


def test_parse_repo_slug_with_default_owner():
    assert parse_repo_slug("/work/acme-app", "acme", {}) == "acme/acme-app"


def test_parse_repo_slug_with_override():
    overrides = {"work/app": "acme/app", "work/lib": "acme/lib"}
    assert parse_repo_slug("/Users/karl/work/app", "other", overrides) == "acme/app"


def test_parse_repo_slug_no_path():
    assert parse_repo_slug(None, "acme", {}) is None


def test_parse_repo_slug_no_owner():
    assert parse_repo_slug("/work/app", "", {}) is None


def test_parse_overrides():
    raw = "work/app=acme/app, work/lib=acme/lib"
    assert parse_overrides(raw) == {"work/app": "acme/app", "work/lib": "acme/lib"}


def test_parse_overrides_ignores_invalid():
    assert parse_overrides("nonsense, a=b") == {"a": "b"}


def _mock_response(data: list[dict[str, Any]], headers: dict[str, str] | None = None) -> mock.Mock:
    mock_response = mock.Mock()
    mock_response.read.return_value = json.dumps(data).encode("utf-8")
    mock_response.headers = headers or {}
    mock_response.__enter__ = mock.Mock(return_value=mock_response)
    mock_response.__exit__ = mock.Mock(return_value=False)
    return mock_response


@mock.patch("aoe_github_lite.github.urllib.request.urlopen")
def test_list_open_prs_parses_response(mock_urlopen):
    mock_urlopen.return_value = _mock_response(
        [
            {
                "number": 42,
                "title": "Add feature",
                "state": "open",
                "user": {"login": "alice"},
                "html_url": "https://github.com/acme/app/pull/42",
                "draft": False,
                "created_at": "2026-08-20T10:00:00Z",
            }
        ]
    )

    prs = list_open_prs("acme/app")
    assert len(prs) == 1
    assert prs[0] == PullRequest(
        number=42,
        title="Add feature",
        state="open",
        author="alice",
        html_url="https://github.com/acme/app/pull/42",
        draft=False,
        created_at="2026-08-20T10:00:00Z",
    )


@mock.patch("aoe_github_lite.github.urllib.request.urlopen")
def test_list_open_prs_follows_pagination(mock_urlopen):
    def urlopen_side_effect(req: urllib.request.Request, **_kwargs: Any):
        url = req.full_url
        if "page=2" in url:
            return _mock_response(
                [
                    {
                        "number": 2,
                        "title": "Second page",
                        "state": "open",
                        "user": {"login": "bob"},
                        "html_url": "https://github.com/acme/app/pull/2",
                        "draft": False,
                        "created_at": "2026-08-20T11:00:00Z",
                    }
                ]
            )
        return _mock_response(
            [
                {
                    "number": 1,
                    "title": "First page",
                    "state": "open",
                    "user": {"login": "alice"},
                    "html_url": "https://github.com/acme/app/pull/1",
                    "draft": False,
                    "created_at": "2026-08-20T10:00:00Z",
                }
            ],
            {"Link": '<https://api.github.com/repos/acme/app/pulls?state=open&per_page=100&page=2>; rel="next"'},
        )

    mock_urlopen.side_effect = urlopen_side_effect

    prs = list_open_prs("acme/app")
    assert len(prs) == 2
    assert prs[0].number == 1
    assert prs[1].number == 2


@mock.patch("aoe_github_lite.github.urllib.request.urlopen")
def test_list_open_prs_raises_on_http_error(mock_urlopen):
    from urllib.error import HTTPError

    mock_urlopen.side_effect = HTTPError(
        url="https://api.github.com/repos/acme/app/pulls",
        code=404,
        msg="Not Found",
        hdrs=None,
        fp=None,
    )
    with pytest.raises(RuntimeError):
        list_open_prs("acme/app")
