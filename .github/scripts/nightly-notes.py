#!/usr/bin/env python3
"""Release notes for a nightly: the grouped changelog bullets a stable release gets.

release-plz writes stable notes from `release-plz.toml`'s `[changelog]` section on a
release PR. A nightly has no release PR, so this script renders the same bullets
between two revisions. Which commits are listed, and in which group, comes from that
section's `commit_parsers` and `commit_preprocessors`. The bullet and section shape
below is a copy of its `body` template, since nothing renders that template for a
nightly: editing the template in `release-plz.toml` means editing `render()` too. The
in-app "What's New" dialog parses these bullets, so keep the shape intact. Author
handles are omitted: they would need one GitHub API call per commit, and the dialog
shows only the PR number.

Usage:
    python3 .github/scripts/nightly-notes.py --base nightly-20261120 --head <sha>
    python3 .github/scripts/nightly-notes.py --self-test
"""

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path

CONFIG = Path(__file__).resolve().parents[2] / "release-plz.toml"
DEFAULT_REPO_URL = "https://github.com/agent-of-empires/agent-of-empires"
# git-cliff strips the Conventional Commits prefix and keeps the scope.
PREFIX_RE = re.compile(r"^[A-Za-z]+(?:\(([^)]*)\))?!?:\s*")
BREAKING_RE = re.compile(r"^[A-Za-z]+(?:\([^)]*\))?!:")
BREAKING_FOOTER_RE = re.compile(r"^BREAKING[ -]CHANGE:", re.M)
# GitHub appends the PR to squash subjects; release-plz.toml strips it as well.
PR_SUFFIX_RE = re.compile(r" \(#(\d+)\)$")


def load_rules(config=CONFIG):
    changelog = tomllib.load(open(config, "rb"))["changelog"]
    return changelog.get("commit_parsers", []), changelog.get("commit_preprocessors", [])


def commits(base, head="HEAD"):
    """One (sha, subject, body) triple per commit between the two revisions."""
    out = subprocess.run(
        ["git", "log", "--reverse", "--format=%H%x1f%s%x1f%b%x1e", f"{base}..{head}"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    for record in out.split("\x1e"):
        if record.strip():
            yield record.strip("\n").split("\x1f", 2)


def changelog_items(records, parsers, preprocessors):
    """(group, items) pairs, in the alphabetical group order git-cliff emits."""
    grouped = {}
    for sha, subject, body in records:
        pr = int(m.group(1)) if (m := PR_SUFFIX_RE.search(subject)) else None
        for pre in preprocessors:
            subject = re.sub(pre["pattern"], pre["replace"], subject)
        parser = next((p for p in parsers if re.search(p["message"], subject)), None)
        breaking = bool(BREAKING_RE.match(subject) or BREAKING_FOOTER_RE.search(body))
        # Mirrors protect_breaking_commits: a breaking commit is listed even when
        # its type is otherwise skipped.
        if not breaking and (parser is None or parser.get("skip")):
            continue
        prefix = PREFIX_RE.match(subject)
        scope = prefix.group(1) if prefix and prefix.group(1) else None
        message = subject[prefix.end() :] if prefix else subject
        grouped.setdefault(parser.get("group", "Other") if parser else "Other", []).append(
            {
                "sha": sha,
                "scope": scope,
                "message": message[:1].upper() + message[1:],
                "pr": pr,
            }
        )
    return sorted(grouped.items())


def render(groups, repo_url=DEFAULT_REPO_URL):
    blocks = []
    for group, items in groups:
        lines = [f"### {group}", ""]
        for item in items:
            scope = f"**{item['scope']}:** " if item["scope"] else ""
            pr = f" in [#{item['pr']}]({repo_url}/pull/{item['pr']})" if item["pr"] else ""
            commit = f" ([`{item['sha'][:7]}`]({repo_url}/commit/{item['sha']}))"
            lines.append(f"- {scope}{item['message']}{pr}{commit}")
        blocks.append("\n".join(lines))
    return "\n\n".join(blocks)


def self_test():
    def ok(condition, label):
        if not condition:
            sys.exit(f"FAIL: {label}")
        print(f"ok: {label}")

    parsers, preprocessors = load_rules()
    records = [
        ("a" * 40, "fix(tmux): treat no-current-target as missing pane list (#5701)", ""),
        ("b" * 40, "feat(web): add a knob (#5702)", ""),
        ("c" * 40, "docs: rewrite the guide", ""),
        ("d" * 40, "chore!: drop the old flag", ""),
        ("e" * 40, "chore: bump the lockfile", ""),
        ("f" * 40, "De-flake a flaky test (#5703)", ""),
        ("9" * 40, "ci: retire a job", "BREAKING CHANGE: needs a new runner"),
    ]
    groups = changelog_items(records, parsers, preprocessors)
    ok([g for g, _ in groups] == ["Bug Fixes", "Features", "Other"], "groups are alphabetical")
    items = dict(groups)
    ok(
        items["Bug Fixes"]
        == [
            {
                "sha": "a" * 40,
                "scope": "tmux",
                "message": "Treat no-current-target as missing pane list",
                "pr": 5701,
            }
        ],
        "a scoped fix loses its prefix, keeps scope and PR",
    )
    ok(
        [i["scope"] for i in items["Features"]] == ["web"],
        "a feat lands in Features with its scope",
    )
    ok(
        [i["pr"] for i in items["Other"]] == [None, 5703, None],
        "chore! and an unmatched subject are listed; chore, docs and ci are skipped",
    )
    ok(
        [i["scope"] for i in items["Other"]] == [None, None, None],
        "unconventional subjects carry no scope",
    )
    ok(
        render(groups).splitlines()[-1]
        == f"- Retire a job ([`{'9' * 7}`]({DEFAULT_REPO_URL}/commit/{'9' * 40}))",
        "a rendering bullet carries the PR only when GitHub appended one",
    )
    ok(
        render([("Bug Fixes", items["Bug Fixes"])])
        == "### Bug Fixes\n\n"
        "- **tmux:** Treat no-current-target as missing pane list"
        f" in [#5701]({DEFAULT_REPO_URL}/pull/5701)"
        f" ([`{'a' * 7}`]({DEFAULT_REPO_URL}/commit/{'a' * 40}))",
        "bullets match the template in release-plz.toml",
    )
    ok(render([]) == "", "no commits render nothing")
    print("self-test passed.")


def main():
    parser = argparse.ArgumentParser(description="Render a nightly's release notes.")
    parser.add_argument("--base", help="previous nightly tag, else the last stable tag")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--repo-url", default=DEFAULT_REPO_URL)
    args = parser.parse_args()
    if not args.base:
        parser.error("--base is required")
    parsers, preprocessors = load_rules()
    groups = changelog_items(commits(args.base, args.head), parsers, preprocessors)
    print(render(groups, args.repo_url))


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    else:
        main()
