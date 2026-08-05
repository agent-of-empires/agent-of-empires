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

This runs in seconds with no toolchain. It covers exactly one failure class: an
asset embedded via `include!` / `include_bytes!` / `include_str!` that the Cargo
source filter drops, caught pre-merge on every PR. It does not catch every
src-driven build break (a bad buildInput, a toolchain bump, a non-include source
reference); those still surface post-merge in the expensive `nix build` job,
which is why that job only has to fire when the flake recipe itself changes.

Usage:
    python3 scripts/check-nix-embedded-assets.py [--self-test]
"""

import re
import sys
import tomllib
from pathlib import Path, PurePosixPath

REPO_ROOT = Path(__file__).resolve().parent.parent

# The root package `.` (Cargo.toml `[workspace] members`) expands to its own
# compile roots, not the repo root: scanning `.` as REPO_ROOT would rglob into
# web/, target/, .git/ and the sibling crates, re-adding the deliberately
# excluded xtask and double-counting the sub-crates.
ROOT_PACKAGE_ROOTS = ["src", "build.rs", "build_git_watch.rs"]

# Extensions crane's `commonCargoSources` keeps anywhere in the tree. Kept in
# sync with lib/fileset/{rust,toml,cargoTomlAndLock}.nix upstream; the flake
# shape assertion below fails loudly if the flake stops using that fileset.
CARGO_KEPT_SUFFIXES = (".rs", ".toml")
CARGO_KEPT_NAMES = ("Cargo.lock",)

INCLUDE_MACRO = re.compile(r"include(?:_bytes|_str)?!")
# `include_str!("relative/path")`, tolerating a legal single-arg trailing comma
# `include_bytes!("path",)`. The optional comma is followed by `\s*\)`, so a
# two-argument call keeps a token after the comma and stays unmatched (unresolved).
LITERAL_ARG = re.compile(r'\(\s*"([^"]*)"\s*,?\s*\)')
# `include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/absolute/from/crate/root"))`,
# tolerating a legal trailing comma inside `concat!` and after it, like LITERAL_ARG.
MANIFEST_DIR_ARG = re.compile(
    r'\(\s*concat!\s*\(\s*env!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)\s*,\s*"([^"]*)"\s*,?\s*\)\s*,?\s*\)'
)


def workspace_members(cargo_toml_text):
    """Ordered `[workspace] members` entries from a Cargo.toml.

    Raises if there is no `[workspace] members` array, so a workspace layout
    change fails the check instead of silently narrowing the scan.
    """
    members = tomllib.loads(cargo_toml_text).get("workspace", {}).get("members")
    if not isinstance(members, list) or not members:
        raise ValueError(
            "Cargo.toml has no [workspace] members array; "
            "scripts/check-nix-embedded-assets.py can no longer derive its scan "
            "roots. Update the checker."
        )
    return [str(PurePosixPath(m)) for m in members]


def assert_members_on_disk(repo_root, members):
    """Fail loudly on a member this checker would silently skip.

    `[workspace] members` entries may be globs (`crates/*`), which `rust_files`
    would resolve to no directory at all: the crate would drop out of the scan
    with no error, which is the silent narrowing this derivation exists to
    prevent. A plain typo lands in the same place.
    """
    missing = [m for m in members if m != "." and not (repo_root / m).is_dir()]
    if missing:
        raise ValueError(
            f"Cargo.toml [workspace] members that are not directories: "
            f"{', '.join(missing)}. A glob member (`crates/*`) would otherwise be "
            f"skipped silently and stop being scanned for embedded assets. Teach "
            f"scripts/check-nix-embedded-assets.py this form."
        )


def subcrate_members(members):
    """Members that carry their own `CARGO_MANIFEST_DIR` (a `Cargo.toml`).

    Excludes the root package `.` (its manifest dir is the repo root) and
    `xtask` (never compiled by `--package agent-of-empires`, so never scanned).
    """
    return [m for m in members if m not in (".", "xtask")]


def scan_roots(members):
    """Trees scanned for embedded assets, in deterministic order.

    `xtask` is dropped (the Nix build is `--package agent-of-empires`, so an
    xtask-only include is never read from the Nix source), `.` expands to the
    root package's own compile roots, and `tests`, `benches` and `examples` are
    always added because the `aoe-clippy` (--all-targets) and `aoe-test` checks
    compile those targets from the same `commonArgs.src`.
    """
    roots = []
    for member in members:
        if member == "xtask":
            continue
        if member == ".":
            roots.extend(ROOT_PACKAGE_ROOTS)
        else:
            roots.append(member)
    roots.extend(("tests", "benches", "examples"))
    seen = set()
    ordered = []
    for root in roots:
        if root not in seen:
            seen.add(root)
            ordered.append(root)
    return ordered


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


def crate_root_for(rel_file, subcrates):
    """Crate root a `CARGO_MANIFEST_DIR` include resolves against."""
    for crate in subcrates:
        if rel_file.startswith(crate + "/"):
            return PurePosixPath(crate)
    return PurePosixPath(".")


def rust_files(repo_root, roots):
    for root in roots:
        path = repo_root / root
        if path.is_file():
            yield path
        elif path.is_dir():
            yield from sorted(path.rglob("*.rs"))


def collect_includes(repo_root, roots, subcrates):
    """Every embedded asset path, plus any include this checker cannot resolve."""
    found = []
    unresolved = []
    for path in rust_files(repo_root, roots):
        rel_file = path.relative_to(repo_root).as_posix()
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
                rel = (crate_root_for(rel_file, subcrates) / manifest.group(1).lstrip("/")).as_posix()
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

    # A leading `./` and a trailing slash on a member are normalized away (so
    # the `xtask` and `.` filters and the subcrate prefix match still work).
    members = workspace_members(
        '[workspace]\nmembers = [".", "./xtask", "aoe-settings-derive", "aoe-plugin-api/"]\n'
    )
    assert members == [".", "xtask", "aoe-settings-derive", "aoe-plugin-api"], members
    assert subcrate_members(members) == ["aoe-settings-derive", "aoe-plugin-api"], members
    # `.` expands to the root package's compile roots, `xtask` drops, and the
    # `--all-targets` roots (tests, benches, examples) are appended.
    assert scan_roots(members) == [
        "src",
        "build.rs",
        "build_git_watch.rs",
        "aoe-settings-derive",
        "aoe-plugin-api",
        "tests",
        "benches",
        "examples",
    ], scan_roots(members)
    try:
        workspace_members('[package]\nname = "x"\n')
    except ValueError:
        pass
    else:
        raise AssertionError("Cargo.toml with no [workspace] members must fail loudly")

    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "aoe-plugin-api").mkdir()
        assert_members_on_disk(root, [".", "aoe-plugin-api"])
        # A glob member resolves to no directory, so it would drop out of the
        # scan silently. It has to fail the check instead.
        for bad in ("crates/*", "typo-crate"):
            try:
                assert_members_on_disk(root, [".", bad])
            except ValueError:
                pass
            else:
                raise AssertionError(f"member {bad!r} must fail loudly")

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "tests").mkdir()
        (root / "tests" / "fixture.bin").write_bytes(b"x")
        # A single-arg trailing comma resolves, and a `tests/` root is scanned
        # because the aoe-clippy/aoe-test checks compile the integration tests.
        (root / "tests" / "embed.rs").write_text(
            'const _: &[u8] = include_bytes!("fixture.bin",);\n', encoding="utf-8"
        )
        # A CARGO_MANIFEST_DIR include with a legal trailing comma resolves too.
        (root / "tests" / "manifest.rs").write_text(
            'const _: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), '
            '"/tests/fixture.bin",));\n',
            encoding="utf-8",
        )
        found, unresolved = collect_includes(root, ["tests"], [])
        assert unresolved == [], unresolved
        assert found == [
            ("tests/embed.rs", 1, "tests/fixture.bin"),
            ("tests/manifest.rs", 1, "tests/fixture.bin"),
        ], found
        # A `.bin` under no extra fileset path is a violation the checker catches.
        assert not survives_nix_source(found[0][2], []), found

    print("self-test passed")


def main():
    if "--self-test" in sys.argv[1:]:
        self_test()
        return 0

    flake = (REPO_ROOT / "flake.nix").read_text(encoding="utf-8")
    extras = parse_extra_fileset_paths(flake)
    members = workspace_members((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    assert_members_on_disk(REPO_ROOT, members)
    found, unresolved = collect_includes(
        REPO_ROOT, scan_roots(members), subcrate_members(members)
    )

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
