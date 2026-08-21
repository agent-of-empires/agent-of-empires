"""Tests for GitHub API helpers."""

from __future__ import annotations

import json
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


@mock.patch("aoe_github_lite.github.urllib.request.urlopen")
def test_list_open_prs_parses_response(mock_urlopen):
    mock_response = mock.Mock()
    mock_response.read.return_value = json.dumps(
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
    ).encode("utf-8")
    mock_urlopen.return_value.__enter__ = mock.Mock(return_value=mock_response)
    mock_urlopen.return_value.__exit__ = mock.Mock(return_value=False)

    prs = list_open_prs("acme/app", "")
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
def test_list_open_prs_adds_auth_header(mock_urlopen):
    mock_response = mock.Mock()
    mock_response.read.return_value = json.dumps([]).encode("utf-8")
    mock_urlopen.return_value.__enter__ = mock.Mock(return_value=mock_response)
    mock_urlopen.return_value.__exit__ = mock.Mock(return_value=False)

    list_open_prs("acme/app", "ghp_secret")
    req = mock_urlopen.call_args[0][0]
    assert req.get_header("Authorization") == "Bearer ghp_secret"


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
        list_open_prs("acme/app", "")
