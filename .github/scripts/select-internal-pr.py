#!/usr/bin/env python3
"""Pick the one open PR from a base-repo branch, or fail closed.

`gh pr list --head <branch>` matches on the branch name alone; its help states
that `<owner>:<branch>` is not supported. A fork can open a PR from a branch
with the same name, and automation that takes `.[0]` from that list can then
edit, label and `gh pr merge --auto` a contributor's PR instead of its own.

This reads a `gh pr list --json ...` array on stdin and keeps only candidates
whose head repository is the base repository and whose head and base branches
are the expected ones. Exactly one match prints its number; no match prints
nothing (the caller opens the PR); anything else is an error, so an ambiguous
list stops the workflow before it mutates a PR.

Usage:
    gh pr list --head "$BRANCH" --state open \\
      --json number,headRefName,headRepository,baseRefName \\
      | python3 .github/scripts/select-internal-pr.py \\
          --repo "$BASE_REPO" --head "$BRANCH" --base main

    python3 .github/scripts/select-internal-pr.py --self-test
"""

import argparse
import json
import sys


class SelectionError(Exception):
    pass


def _head_repo(candidate):
    """`headRepository.nameWithOwner`, or None when the field is absent or null.

    A candidate with no readable head repository can never be proven internal,
    so it is dropped rather than treated as a match.
    """
    repo = candidate.get("headRepository")
    if not isinstance(repo, dict):
        return None
    name = repo.get("nameWithOwner")
    return name if isinstance(name, str) else None


def select(payload, repo, head, base):
    """Number of the single internal candidate, or None when there is none."""
    if not isinstance(payload, list):
        raise SelectionError(f"expected a JSON array of PRs, got {type(payload).__name__}")

    matches = []
    for candidate in payload:
        if not isinstance(candidate, dict):
            raise SelectionError(f"expected PR objects, got {type(candidate).__name__}")
        # Owner and repository names are case insensitive on GitHub; branch
        # names are not.
        if (_head_repo(candidate) or "").casefold() != repo.casefold():
            continue
        if candidate.get("headRefName") != head or candidate.get("baseRefName") != base:
            continue
        number = candidate.get("number")
        if not isinstance(number, int) or isinstance(number, bool):
            raise SelectionError(f"matching PR has no usable number: {candidate!r}")
        matches.append(number)

    if len(matches) > 1:
        raise SelectionError(
            f"{len(matches)} open PRs in {repo} share {head} -> {base} "
            f"(numbers: {', '.join(str(n) for n in sorted(matches))}); "
            f"refusing to guess which one the automation owns"
        )
    return matches[0] if matches else None


def self_test():
    repo, head, base = "agent-of-empires/agent-of-empires", "chore/nix-npm-hash-update", "main"

    internal = {
        "number": 100,
        "headRefName": head,
        "baseRefName": base,
        "headRepository": {"nameWithOwner": repo},
    }
    # A fork PR from an identically named branch: what `gh pr list --head`
    # cannot filter out and `.[0]` could otherwise select.
    fork = {
        "number": 200,
        "headRefName": head,
        "baseRefName": base,
        "headRepository": {"nameWithOwner": "attacker/agent-of-empires"},
    }

    cases = [
        ([], None),
        ([internal], 100),
        ([fork], None),
        ([fork, internal], 100),
        ([internal, fork], 100),
        # Casing differences in the repository name are not a fork.
        ([{**internal, "headRepository": {"nameWithOwner": repo.upper()}}], 100),
        # Same owner, different repository.
        ([{**fork, "headRepository": {"nameWithOwner": "agent-of-empires/other"}}], None),
        # An internal PR targeting another base branch is not this PR.
        ([{**internal, "baseRefName": "release"}], None),
        ([{**internal, "headRefName": "chore/nix-npm-hash-update-2"}], None),
        # A head repository that cannot be read is never assumed internal.
        ([{**internal, "headRepository": None}], None),
        ([{k: v for k, v in internal.items() if k != "headRepository"}], None),
    ]
    for payload, expected in cases:
        actual = select(payload, repo, head, base)
        assert actual == expected, f"{payload!r}: {actual} != {expected}"

    failures = [
        # Two internal candidates: impossible through the GitHub UI, so treat
        # it as a broken assumption rather than picking one.
        [internal, {**internal, "number": 101}],
        # Malformed payloads must not silently look like "no match".
        {"number": 100},
        [None],
        [{**internal, "number": None}],
        [{**internal, "number": "100"}],
    ]
    for payload in failures:
        try:
            select(payload, repo, head, base)
        except SelectionError:
            continue
        raise AssertionError(f"{payload!r} should have failed closed")

    print("OK: internal PR selection picks the base-repo PR and fails closed otherwise.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", help="base repository as <owner>/<name>")
    parser.add_argument("--head", help="expected head branch")
    parser.add_argument("--base", help="expected base branch")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return 0

    missing = [n for n in ("repo", "head", "base") if not getattr(args, n)]
    if missing:
        parser.error(f"missing required argument(s): {', '.join('--' + n for n in missing)}")

    try:
        payload = json.load(sys.stdin)
    except ValueError as exc:
        print(f"::error::could not parse the `gh pr list` output: {exc}", file=sys.stderr)
        return 1

    try:
        number = select(payload, args.repo, args.head, args.base)
    except SelectionError as exc:
        print(f"::error::{exc}", file=sys.stderr)
        return 1

    if number is not None:
        print(number)
    return 0


if __name__ == "__main__":
    sys.exit(main())
