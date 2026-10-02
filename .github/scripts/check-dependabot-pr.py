#!/usr/bin/env python3
"""Supply-chain checks a Dependabot PR must pass before auto-merge.

Reads the PR through git only (`git diff` / `git show` on fetched commits),
so nothing from the PR is executed. Fails closed on anything it does not
recognise:

- Only manifests, lockfiles and workflow/action YAML may change, plus the
  `npmDepsHash` line in flake.nix that nix-npm-hash-fix-pr.yml pushes.
- npm: every new or changed `package-lock.json` entry resolves from
  registry.npmjs.org, and no entry gains `hasInstallScript`.
- cargo: every new or changed `Cargo.lock` package comes from crates.io.
- GitHub Actions: every added `uses:` pins a 40-hex SHA with a `# <tag>`
  comment, and that tag resolves upstream to the same commit.

Usage:
    python3 .github/scripts/check-dependabot-pr.py --base <sha> --head <sha>
    python3 .github/scripts/check-dependabot-pr.py --self-test
"""

import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import PurePosixPath

NPM_REGISTRY = "https://registry.npmjs.org/"
CRATES_IO_SOURCES = {
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
}
PINNED_USES = re.compile(
    r"^\+\s*(?:-\s*)?uses:\s*([\w.-]+)/([\w.-]+)(?:/[^@\s]*)?@([0-9a-f]{40})\s+#\s*(\S+)\s*$"
)
ANY_USES = re.compile(r"^\+\s*(?:-\s*)?uses:")
NPM_DEPS_HASH = re.compile(r'^[+-]\s*npmDepsHash = "sha256-[A-Za-z0-9+/]{43}=";$')


def is_workflow_yaml(path):
    p = PurePosixPath(path)
    if p.suffix not in (".yml", ".yaml"):
        return False
    if p.parent == PurePosixPath(".github/workflows"):
        return True
    return p.parts[:2] == (".github", "actions") and p.stem == "action"


def classify(path):
    name = PurePosixPath(path).name
    if name == "package-lock.json":
        return "npm-lock"
    if name == "Cargo.lock":
        return "cargo-lock"
    if name in ("package.json", "Cargo.toml"):
        return "manifest"
    if path == "flake.nix":
        return "nix-hash"
    if is_workflow_yaml(path):
        return "actions"
    return None


def check_npm_lock(path, base_text, head_text):
    problems = []
    base = json.loads(base_text).get("packages", {}) if base_text else {}
    head = json.loads(head_text).get("packages", {})
    for key, entry in head.items():
        if key == "":
            continue
        before = base.get(key)
        if before == entry:
            continue
        where = f"{path}: {key}"
        if entry.get("link") or entry.get("inBundle"):
            if before is None:
                problems.append(f"{where}: new linked or bundled entry")
            continue
        resolved = entry.get("resolved", "")
        if not resolved.startswith(NPM_REGISTRY):
            problems.append(f"{where}: resolves from {resolved or '<missing>'}")
        if not entry.get("integrity"):
            problems.append(f"{where}: no integrity hash")
        if entry.get("hasInstallScript") and not (before or {}).get("hasInstallScript"):
            problems.append(f"{where}: adds an install script")
    return problems


def check_cargo_lock(path, base_text, head_text):
    problems = []

    def packages(text):
        return tomllib.loads(text).get("package", []) if text else []

    base = packages(base_text)
    workspace = {p["name"] for p in base if "source" not in p}
    seen = {(p["name"], p["version"], p.get("source")) for p in base}
    for pkg in packages(head_text):
        key = (pkg["name"], pkg["version"], pkg.get("source"))
        if key in seen:
            continue
        where = f"{path}: {pkg['name']} {pkg['version']}"
        source = pkg.get("source")
        if source is None:
            if pkg["name"] not in workspace:
                problems.append(f"{where}: new package with no source")
        elif source not in CRATES_IO_SOURCES:
            problems.append(f"{where}: comes from {source}")
        elif not pkg.get("checksum"):
            problems.append(f"{where}: no checksum")
    return problems


def check_actions_diff(path, diff_text, resolve_tag):
    """`resolve_tag(owner, repo, tag)` returns the commit SHA the tag points at."""
    problems = []
    for line in diff_text.splitlines():
        if line.startswith("+++") or not ANY_USES.match(line):
            continue
        m = PINNED_USES.match(line)
        if not m:
            problems.append(f"{path}: not a SHA-pinned `uses:` with a tag comment: {line[1:].strip()}")
            continue
        owner, repo, sha, tag = m.groups()
        try:
            actual = resolve_tag(owner, repo, tag)
        except Exception as e:  # noqa: BLE001 - any lookup failure fails closed
            problems.append(f"{path}: cannot resolve {owner}/{repo}@{tag}: {e}")
            continue
        if actual != sha:
            problems.append(f"{path}: {owner}/{repo} {tag} is {actual}, PR pins {sha}")
    return problems


def check_nix_hash_diff(path, diff_text):
    changes = [
        line for line in diff_text.splitlines()
        if line[:1] in "+-" and not line.startswith(("+++", "---"))
    ]
    if len(changes) == 2 and all(NPM_DEPS_HASH.match(line) for line in changes):
        return []
    return [f"{path}: changes more than the npmDepsHash line"]


def check(changed, read, diff, resolve_tag):
    """`changed` lists paths; `read(rev, path)` returns text or None; `diff(path)` returns a -U0 diff."""
    problems = []
    for path in changed:
        kind = classify(path)
        if kind is None:
            problems.append(f"{path}: not a file Dependabot auto-merge may change")
        elif kind == "npm-lock":
            head = read("head", path)
            if head is None:
                problems.append(f"{path}: deleted")
            else:
                problems += check_npm_lock(path, read("base", path), head)
        elif kind == "cargo-lock":
            head = read("head", path)
            if head is None:
                problems.append(f"{path}: deleted")
            else:
                problems += check_cargo_lock(path, read("base", path), head)
        elif kind == "actions":
            problems += check_actions_diff(path, diff(path), resolve_tag)
        elif kind == "nix-hash":
            problems += check_nix_hash_diff(path, diff(path))
    return problems


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def gh_api(endpoint):
    out = subprocess.run(["gh", "api", endpoint], check=True, capture_output=True, text=True).stdout
    return json.loads(out)


def resolve_tag_upstream(owner, repo, tag):
    obj = gh_api(f"repos/{owner}/{repo}/git/ref/tags/{tag}")["object"]
    # Annotated tags point at a tag object; peel to the commit.
    for _ in range(5):
        if obj["type"] == "commit":
            return obj["sha"]
        if obj["type"] != "tag":
            raise ValueError(f"tag points at a {obj['type']}")
        obj = gh_api(f"repos/{owner}/{repo}/git/tags/{obj['sha']}")["object"]
    raise ValueError("tag chain too deep")


def run(base, head):
    merge_base = git("merge-base", base, head).strip()
    revs = {"base": merge_base, "head": head}

    def read(rev, path):
        try:
            return git("show", f"{revs[rev]}:{path}")
        except subprocess.CalledProcessError:
            return None

    def diff(path):
        return git("diff", "-U0", merge_base, head, "--", path)

    changed = git("diff", "--name-only", merge_base, head).split()
    return check(changed, read, diff, resolve_tag_upstream)


def self_test():
    sha_a, sha_b = "a" * 40, "b" * 40
    npm_base = json.dumps({"packages": {
        "": {"name": "web"},
        "node_modules/left-pad": {"version": "1.0.0", "resolved": NPM_REGISTRY + "left-pad/-/left-pad-1.0.0.tgz", "integrity": "sha512-x"},
        "node_modules/esbuild": {"version": "0.1.0", "resolved": NPM_REGISTRY + "esbuild/-/esbuild-0.1.0.tgz", "integrity": "sha512-x", "hasInstallScript": True},
    }})

    def npm_head(**changes):
        data = json.loads(npm_base)
        for key, entry in changes.items():
            data["packages"][f"node_modules/{key}"] = entry
        return json.dumps(data)

    good_entry = {"version": "1.1.0", "resolved": NPM_REGISTRY + "left-pad/-/left-pad-1.1.0.tgz", "integrity": "sha512-y"}
    cargo_base = (
        '[[package]]\nname = "agent-of-empires"\nversion = "1.0.0"\n\n'
        '[[package]]\nname = "serde"\nversion = "1.0.0"\n'
        'source = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "aa"\n'
    )
    cargo_ok = cargo_base.replace('version = "1.0.0"\nsource', 'version = "1.0.1"\nsource')
    cargo_git = cargo_base + '\n[[package]]\nname = "evil"\nversion = "0.1.0"\nsource = "git+https://example.com/evil#abc"\n'

    def resolver(owner, repo, tag):
        return {"v2.0.0": sha_a}[tag]

    cases = [
        ("npm registry bump", ["web/package-lock.json"], {"web/package-lock.json": (npm_base, npm_head(**{"left-pad": good_entry}))}, "", 0),
        ("npm foreign tarball", ["web/package-lock.json"], {"web/package-lock.json": (npm_base, npm_head(**{"left-pad": {**good_entry, "resolved": "https://evil.example/x.tgz"}}))}, "", 1),
        ("npm new install script", ["web/package-lock.json"], {"web/package-lock.json": (npm_base, npm_head(**{"left-pad": {**good_entry, "hasInstallScript": True}}))}, "", 1),
        ("npm existing install script", ["web/package-lock.json"], {"web/package-lock.json": (npm_base, npm_head(esbuild={"version": "0.2.0", "resolved": NPM_REGISTRY + "esbuild/-/esbuild-0.2.0.tgz", "integrity": "sha512-z", "hasInstallScript": True}))}, "", 0),
        ("npm missing integrity", ["web/package-lock.json"], {"web/package-lock.json": (npm_base, npm_head(**{"left-pad": {**good_entry, "integrity": ""}}))}, "", 1),
        ("cargo crates.io bump", ["Cargo.lock", "Cargo.toml"], {"Cargo.lock": (cargo_base, cargo_ok)}, "", 0),
        ("cargo git source", ["Cargo.lock"], {"Cargo.lock": (cargo_base, cargo_git)}, "", 1),
        ("actions matching tag", [".github/workflows/ci.yml"], {}, f"+      - uses: actions/checkout@{sha_a} # v2.0.0\n", 0),
        ("actions subpath", [".github/actions/x/action.yml"], {}, f"+    - uses: github/codeql-action/init@{sha_a} # v2.0.0\n", 0),
        ("actions mismatched tag", [".github/workflows/ci.yml"], {}, f"+      - uses: actions/checkout@{sha_b} # v2.0.0\n", 1),
        ("actions unknown tag", [".github/workflows/ci.yml"], {}, f"+      - uses: actions/checkout@{sha_a} # v9\n", 1),
        ("actions unpinned", [".github/workflows/ci.yml"], {}, "+      - uses: actions/checkout@v2\n", 1),
        ("nix hash only", ["flake.nix"], {}, '-  npmDepsHash = "sha256-' + "A" * 43 + '=";\n+  npmDepsHash = "sha256-' + "B" * 43 + '=";\n', 0),
        ("nix other line", ["flake.nix"], {}, '-  npmDepsHash = "sha256-' + "A" * 43 + '=";\n+  npmDepsHash = "sha256-' + "B" * 43 + '=";\n+  src = ./evil;\n', 1),
        ("unexpected file", ["build.rs"], {}, "", 1),
    ]
    failed = 0
    for name, changed, files, diff_text, expected in cases:
        def read(rev, path, files=files):
            base, head = files.get(path, (None, None))
            return base if rev == "base" else head

        problems = check(changed, read, lambda _p, d=diff_text: d, resolver)
        if (len(problems) > 0) != bool(expected):
            failed += 1
            print(f"FAIL {name}: {problems}")
    print(f"{len(cases) - failed}/{len(cases)} self-test cases passed")
    return 1 if failed else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base")
    parser.add_argument("--head")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not (args.base and args.head):
        parser.error("--base and --head are required")
    problems = run(args.base, args.head)
    for p in problems:
        print(f"::error::{p}")
    if problems:
        return 1
    print("All Dependabot supply-chain checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
