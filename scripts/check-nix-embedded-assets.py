#!/usr/bin/env python3
"""Fail if a compile-time embedded asset would be dropped from the Nix build source.

The crate embeds files at compile time with `include_bytes!` / `include_str!` /
`include!`. A git checkout has every one of them, so `cargo build` always sees
them. The Nix build does not: it compiles from `commonArgs.src` in `flake.nix`,
a `lib.fileset` union of crane's `commonCargoSources` (which keeps only `*.rs`,
`*.toml` and `Cargo.lock`) plus a short list of explicitly unioned extra paths.
An asset that is neither a `.rs`/`.toml` nor under one of those extra paths is
therefore absent from the Nix source, and the build fails on an unreadable
include while local `cargo build` stays green. That is #3204, where the six
`acp-worker/adapters/*/package{,-lock}.json` manifests were dropped.

This runs in seconds with no toolchain, so the expensive `nix build` CI job only
has to cover changes to the flake recipe itself.

Usage:
    python3 scripts/check-nix-embedded-assets.py [--self-test]
"""

import re
import sys
from pathlib import Path, PurePosixPath

REPO_ROOT = Path(__file__).resolve().parent.parent

# Trees compiled into the `agent-of-empires` package. `xtask` is deliberately
# absent: the Nix build is `--package agent-of-empires`, so an xtask-only include
# is never read from the Nix source.
SCAN_ROOTS = [
    "src",
    "aoe-plugin-api",
    "aoe-settings-derive",
    "build.rs",
    "build_git_watch.rs",
]

# Extensions crane's `commonCargoSources` keeps anywhere in the tree. Kept in
# sync with lib/fileset/{rust,toml,cargoTomlAndLock}.nix upstream; the flake
# shape assertion below fails loudly if the flake stops using that fileset.
CARGO_KEPT_SUFFIXES = (".rs", ".toml")
CARGO_KEPT_NAMES = ("Cargo.lock",)

INCLUDE_MACRO = re.compile(r"include(?:_bytes|_str)?!")
# `include_str!("relative/path")`
LITERAL_ARG = re.compile(r'\(\s*"([^"]*)"\s*\)')
# `include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/absolute/from/crate/root"))`
MANIFEST_DIR_ARG = re.compile(
    r'\(\s*concat!\s*\(\s*env!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)\s*,\s*"([^"]*)"\s*\)\s*\)'
)


def parse_extra_fileset_paths(flake_text):
    """Extra paths unioned into `commonArgs.src` beyond crane's Cargo sources.

    Returns repo-relative POSIX strings (e.g. `acp-worker/adapters`). Raises if
    the flake no longer has the shape this checker understands, so a rewrite
    fails the check instead of silently passing everything.
    """
    if "commonCargoSources" not in flake_text and "cleanCargoSource" not in flake_text:
        raise ValueError(
            "flake.nix uses neither commonCargoSources nor cleanCargoSource; "
            "this checker's assumption that *.rs and *.toml survive no longer "
            "holds. Update scripts/check-nix-embedded-assets.py."
        )

    start = flake_text.find("fileset.unions")
    if start == -1:
        # No union means no extra paths: only crane's Cargo sources survive.
        return []

    open_bracket = flake_text.find("[", start)
    if open_bracket == -1:
        raise ValueError("fileset.unions with no list literal in flake.nix")
    depth = 0
    end = None
    for i in range(open_bracket, len(flake_text)):
        if flake_text[i] == "[":
            depth += 1
        elif flake_text[i] == "]":
            depth -= 1
            if depth == 0:
                end = i
                break
    if end is None:
        raise ValueError("unterminated fileset.unions list in flake.nix")

    block = flake_text[open_bracket : end + 1]
    extras = []
    for token in re.findall(r"\./[A-Za-z0-9._/-]+", block):
        rel = token[2:].rstrip("/")
        # `./.` is the whole-tree argument to commonCargoSources, not an extra.
        if rel in ("", "."):
            continue
        if rel not in extras:
            extras.append(rel)
    return extras


def survives_nix_source(rel_path, extra_paths):
    """Whether `rel_path` (repo-relative) is present in the Nix build source."""
    p = PurePosixPath(rel_path)
    if p.suffix in CARGO_KEPT_SUFFIXES or p.name in CARGO_KEPT_NAMES:
        return True
    for extra in extra_paths:
        if rel_path == extra or rel_path.startswith(extra + "/"):
            return True
    return False


def crate_root_for(rel_file):
    """Crate root a `CARGO_MANIFEST_DIR` include resolves against."""
    for crate in ("aoe-plugin-api", "aoe-settings-derive"):
        if rel_file.startswith(crate + "/"):
            return PurePosixPath(crate)
    return PurePosixPath(".")


def rust_files():
    for root in SCAN_ROOTS:
        path = REPO_ROOT / root
        if path.is_file():
            yield path
        elif path.is_dir():
            yield from sorted(path.rglob("*.rs"))


def collect_includes():
    """Every embedded asset path, plus any include this checker cannot resolve."""
    found = []
    unresolved = []
    for path in rust_files():
        rel_file = path.relative_to(REPO_ROOT).as_posix()
        text = path.read_text(encoding="utf-8")
        for match in INCLUDE_MACRO.finditer(text):
            rest = text[match.end() :]
            stripped = rest.lstrip()
            if not stripped.startswith("("):
                # Prose mentioning the macro in a doc comment, not a call.
                continue
            literal = LITERAL_ARG.match(stripped)
            manifest = MANIFEST_DIR_ARG.match(stripped)
            line = text.count("\n", 0, match.start()) + 1
            if manifest:
                rel = (crate_root_for(rel_file) / manifest.group(1).lstrip("/")).as_posix()
            elif literal:
                base = PurePosixPath(rel_file).parent
                rel = str(PurePosixPath(*_normalize(base / literal.group(1))))
            else:
                unresolved.append((rel_file, line))
                continue
            found.append((rel_file, line, rel))
    return found, unresolved


def _normalize(path):
    """Resolve `..` segments textually; the tree may not exist under --self-test."""
    parts = []
    for part in PurePosixPath(path).parts:
        if part == "..":
            if parts:
                parts.pop()
        elif part not in (".", ""):
            parts.append(part)
    return parts


def self_test():
    sample = """
          commonArgs = {
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                (craneLib.fileset.commonCargoSources ./.)
                ./acp-worker/adapters
                ./docker
              ];
            };
          };
    """
    extras = parse_extra_fileset_paths(sample)
    assert extras == ["acp-worker/adapters", "docker"], extras

    assert parse_extra_fileset_paths("src = craneLib.cleanCargoSource ./.;") == []

    try:
        parse_extra_fileset_paths("src = ./some-other-thing;")
    except ValueError:
        pass
    else:
        raise AssertionError("a flake with no crane source filter must fail loudly")

    cases = [
        # (path, extras, survives)
        ("src/lib.rs", [], True),
        ("themes/builtin/zinc.toml", [], True),
        ("Cargo.lock", [], True),
        ("docker/Dockerfile", [], False),
        ("docker/Dockerfile", ["docker"], True),
        ("acp-worker/adapters/pi-acp/package.json", [], False),
        ("acp-worker/adapters/pi-acp/package.json", ["acp-worker/adapters"], True),
        # A prefix match must respect path boundaries.
        ("dockerfiles/x.png", ["docker"], False),
    ]
    for path, extras, expected in cases:
        actual = survives_nix_source(path, extras)
        assert actual == expected, f"{path} with {extras}: {actual} != {expected}"

    assert _normalize("src/acp/../../acp-worker/adapters/x.json") == [
        "acp-worker",
        "adapters",
        "x.json",
    ]
    print("self-test passed")


def main():
    if "--self-test" in sys.argv[1:]:
        self_test()
        return 0

    flake = (REPO_ROOT / "flake.nix").read_text(encoding="utf-8")
    extras = parse_extra_fileset_paths(flake)
    found, unresolved = collect_includes()

    violations = [
        (f, line, rel) for (f, line, rel) in found if not survives_nix_source(rel, extras)
    ]

    if unresolved:
        for rel_file, line in unresolved:
            print(
                f"::error file={rel_file},line={line}::include macro argument could not "
                f"be resolved by scripts/check-nix-embedded-assets.py; teach it this form "
                f"rather than leaving the asset unchecked"
            )
    for rel_file, line, rel in violations:
        print(
            f"::error file={rel_file},line={line}::`{rel}` is embedded at compile time "
            f"but is not in the Nix build source: it is not a *.rs/*.toml and is not "
            f"under any path unioned into `commonArgs.src` in flake.nix "
            f"(currently: {', '.join(extras) or 'none'}). `nix build` will fail on it "
            f"while `cargo build` succeeds (#3204). Add its directory to the "
            f"`fileset.unions` list in flake.nix."
        )

    if unresolved or violations:
        print(
            f"\nFAIL: {len(violations)} embedded asset(s) missing from the Nix build "
            f"source, {len(unresolved)} unresolvable include(s)."
        )
        return 1

    print(
        f"OK: {len(found)} embedded asset(s) all survive the Nix build source "
        f"(extra fileset paths: {', '.join(extras) or 'none'})."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
