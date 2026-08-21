"""GitHub API client for listing open pull requests."""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True, slots=True)
class PullRequest:
    number: int
    title: str
    state: str
    author: str
    html_url: str
    draft: bool
    created_at: str


def parse_repo_slug(
    project_path: str | None,
    default_owner: str,
    overrides: dict[str, str],
) -> str | None:
    """Resolve a project path to an 'owner/repo' slug.

    Overrides are keyed by a fragment that may appear anywhere in the path.
    """
    if not project_path:
        return None

    normalized = os.path.normpath(project_path)
    for fragment, slug in overrides.items():
        if fragment in normalized:
            return slug

    repo = os.path.basename(normalized) or None
    if not repo:
        return None
    owner = default_owner.strip()
    if not owner:
        return None
    return f"{owner}/{repo}"


def parse_overrides(raw: str) -> dict[str, str]:
    """Parse 'path-fragment=owner/repo,...' into a dict."""
    result: dict[str, str] = {}
    for piece in raw.split(","):
        piece = piece.strip()
        if not piece:
            continue
        if "=" not in piece:
            continue
        fragment, slug = piece.split("=", 1)
        fragment = fragment.strip()
        slug = slug.strip()
        if fragment and slug:
            result[fragment] = slug
    return result


def _make_request(url: str) -> Any:
    req = urllib.request.Request(
        url,
        headers={"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"},
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        return json.loads(resp.read().decode("utf-8")), dict(resp.headers)


def _next_page_url(headers: dict[str, str]) -> str | None:
    """Extract the next page URL from a GitHub Link header."""
    link = headers.get("Link", "")
    if not link:
        return None
    for part in link.split(","):
        try:
            url, rel = part.split(";", 1)
        except ValueError:
            continue
        if 'rel="next"' in rel:
            return url.strip().strip("<>").strip()
    return None


def _item_to_pr(item: Any) -> PullRequest | None:
    if not isinstance(item, dict):
        return None
    user = item.get("user") or {}
    return PullRequest(
        number=int(item.get("number", 0)),
        title=str(item.get("title", "")),
        state=str(item.get("state", "")),
        author=str(user.get("login", "")) if isinstance(user, dict) else "",
        html_url=str(item.get("html_url", "")),
        draft=bool(item.get("draft", False)),
        created_at=str(item.get("created_at", "")),
    )


def list_open_prs(slug: str) -> list[PullRequest]:
    """List open PRs for an owner/repo slug, following GitHub pagination."""
    url: str | None = f"https://api.github.com/repos/{slug}/pulls?state=open&per_page=100"
    prs: list[PullRequest] = []
    seen: set[int] = set()

    while url:
        try:
            data, headers = _make_request(url)
        except urllib.error.HTTPError as exc:
            raise RuntimeError(f"GitHub API error for {slug}: {exc.code} {exc.reason}") from exc
        except urllib.error.URLError as exc:
            raise RuntimeError(f"GitHub API unreachable for {slug}: {exc.reason}") from exc

        if not isinstance(data, list):
            raise RuntimeError(f"unexpected GitHub response for {slug}: not a list")

        for item in data:
            pr = _item_to_pr(item)
            if pr is None or pr.number in seen:
                continue
            seen.add(pr.number)
            prs.append(pr)

        url = _next_page_url(headers)

    return prs
