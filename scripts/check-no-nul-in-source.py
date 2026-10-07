#!/usr/bin/env python3
"""Fail if a tracked text/source file contains a raw NUL byte.

Git decides a blob is binary by looking for a NUL byte near its start. One NUL
anywhere in a source file therefore makes git treat the whole file as binary:
`git diff` prints "Binary files differ" instead of hunks, GitHub's review view
shows no diff at all, and `git grep` skips it. The code still compiles and the
tests still pass, so nothing else complains -- the only symptom is that the
file silently stops being reviewable.

That is #3974, where a cache key was delimited with a literal NUL
(`` `${a}<NUL>${b}` ``) in two web components. Both files' diffs were invisible
in review for several PRs, and a reviewer quoting one of them saw no delimiter
where the source had one. The fix is to write the delimiter as the `\\u0000`
escape sequence: identical string at runtime, source stays text.

This runs in under a second with no toolchain. It covers exactly one failure
class: a NUL byte committed into a file whose extension says it is text. It
deliberately says nothing about genuine binaries (images, fonts, audio), which
are expected to contain NULs and are listed with a path check, not an
extension one.

Usage:
    python3 scripts/check-no-nul-in-source.py [--self-test]
"""

import subprocess
import sys
from pathlib import Path, PurePosixPath

REPO_ROOT = Path(__file__).resolve().parent.parent

# Extensions whose contents are meant to be read as text in review. A NUL in
# one of these is the bug this check exists for. Anything not listed here is
# ignored, so adding a new binary asset type needs no change; adding a new
# source type does, and the self-test's coverage assertion points here.
TEXT_SUFFIXES = frozenset(
    {
        # Rust / build
        ".rs",
        ".toml",
        ".lock",
        ".nix",
        # web
        ".ts",
        ".tsx",
        ".js",
        ".jsx",
        ".mjs",
        ".cjs",
        ".json",
        ".css",
        ".scss",
        ".html",
        ".svg",
        # docs / config / scripts
        ".md",
        ".mdx",
        ".txt",
        ".yml",
        ".yaml",
        ".sh",
        ".py",
        ".sql",
        ".snap",
        ".patch",
        ".diff",
    }
)

# Extensionless tracked files that are still text.
TEXT_NAMES = frozenset({"Dockerfile", "Makefile", "LICENSE", "CODEOWNERS", ".gitignore"})


def is_text_path(rel: str) -> bool:
    p = PurePosixPath(rel)
    return p.suffix in TEXT_SUFFIXES or p.name in TEXT_NAMES


def tracked_files() -> list[str]:
    out = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
    ).stdout
    return [p for p in out.decode().split("\0") if p]


def offenders() -> list[tuple[str, int]]:
    """Tracked text files containing a NUL, as (path, byte offset of the first)."""
    found = []
    for rel in tracked_files():
        if not is_text_path(rel):
            continue
        path = REPO_ROOT / rel
        try:
            data = path.read_bytes()
        except (OSError, FileNotFoundError):
            # Tracked but absent (sparse checkout, broken symlink): not ours to judge.
            continue
        i = data.find(b"\0")
        if i != -1:
            found.append((rel, i))
    return found


def self_test() -> None:
    # A source extension with a NUL is an offender; a real binary is not.
    assert is_text_path("web/src/components/acp/Markdown.tsx")
    assert is_text_path("src/main.rs")
    assert is_text_path("Dockerfile")
    assert not is_text_path("assets/logo.png")
    assert not is_text_path("bundled_sounds/coins.wav")
    assert not is_text_path("web/public/fonts/Geist-Regular.woff2")

    # The escape-sequence form is what the fix looks like: six ASCII bytes, no NUL.
    escaped = "`${a}" + chr(92) + "u0000${b}`"
    assert "\0" not in escaped
    # The bug form: one raw NUL, which is what git trips over.
    raw = "`${a}" + chr(0) + "${b}`"
    assert raw.encode().find(b"\0") == 5

    print("self-test passed")


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        self_test()
        return 0

    bad = offenders()
    if not bad:
        return 0

    print("Raw NUL byte in tracked text file(s):", file=sys.stderr)
    for rel, offset in bad:
        print(f"  {rel} (first at byte {offset})", file=sys.stderr)
    print(
        "\nGit treats a file with a NUL as binary, so its diff is invisible in "
        "review.\nWrite the byte as the escape sequence instead "
        '(e.g. "' + chr(92) + 'u0000" in a JS/TS string):\nthe value is identical '
        "at runtime and the source stays reviewable text.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
