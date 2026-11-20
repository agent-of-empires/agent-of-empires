#!/usr/bin/env python3
"""Require project dependency pins and all real Unix Mio consumer regressions."""

import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[2]
FORK = ROOT / "vendor" / "crossterm"
MANIFEST = FORK / "Cargo.toml"
EXPECTED_TESTS = 21 + int(sys.platform.startswith("linux"))


def main():
    project_lock = ROOT / "Cargo.lock"
    expected = {
        (package["name"], package["version"], package.get("source"))
        for package in tomllib.loads(project_lock.read_text())["package"]
    }
    # The fork is excluded from the workspace; its generated lock stays ignored.
    shutil.copyfile(project_lock, FORK / "Cargo.lock")
    metadata = json.loads(
        subprocess.run(
            [
                "cargo", "metadata", "--manifest-path", str(MANIFEST),
                "--format-version", "1", "--features", "event-stream",
            ],
            check=True, text=True, stdout=subprocess.PIPE, cwd=ROOT,
        ).stdout
    )
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    root = metadata["resolve"]["root"]
    if Path(packages[root]["manifest_path"]).resolve() != MANIFEST:
        raise RuntimeError("standalone tests did not resolve the maintained Crossterm source")
    features = set(nodes[root]["features"])
    if not {"default", "events", "event-stream"} <= features:
        raise RuntimeError(f"missing production Crossterm features: {sorted(features)}")
    if features & {"libc", "use-dev-tty"}:
        raise RuntimeError(f"consumer backend changed: {sorted(features)}")

    # Metadata unifies dev features; tree's normal edges select the host production graph.
    tree = subprocess.run(
        [
            "cargo", "tree", "--manifest-path", str(MANIFEST), "--locked",
            "--features", "event-stream", "--edges", "normal",
            "--prefix", "none", "--format", "{p}", "--no-dedupe",
        ],
        check=True, text=True, stdout=subprocess.PIPE, cwd=ROOT,
    ).stdout
    production = set()
    for line in tree.splitlines():
        match = re.fullmatch(r"([A-Za-z0-9_-]+) v([^ ]+)(?: .*)?", line)
        if not match:
            raise RuntimeError(f"unrecognized normal dependency identity: {line}")
        name, version = match.groups()
        identities = {
            (package["name"], package["version"], package.get("source"))
            for package in packages.values()
            if package["name"] == name and package["version"] == version
        }
        if not identities or not identities <= expected:
            raise RuntimeError(f"production dependency drift from project lock: {identities}")
        production.update(identities)
    print(f"Verified {len(production)} host production pins and Rustix backend", flush=True)

    for test_filter, expected_count in (
        ("forced_batch_", EXPECTED_TESTS),
        ("event::read::tests::", 16),
        ("event::stream::tests::", 2),
    ):
        command = [
            "cargo", "test", "--manifest-path", str(MANIFEST), "--locked", "--lib",
            "--features", "event-stream", test_filter, "--",
        ]
        listed = subprocess.run(
            command + ["--list"], check=True, text=True, stdout=subprocess.PIPE, cwd=ROOT,
        ).stdout
        tests = [line for line in listed.splitlines() if line.endswith(": test")]
        if len(tests) != expected_count or len(set(tests)) != expected_count:
            raise RuntimeError(f"expected {expected_count} {test_filter} tests, found {tests}")
        result = subprocess.run(
            command + ["--test-threads=1", "--nocapture"], text=True,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, cwd=ROOT,
        )
        print(result.stdout, end="", flush=True)
        result.check_returncode()
        summary = f"test result: ok. {expected_count} passed; 0 failed; 0 ignored;"
        if summary not in result.stdout:
            raise RuntimeError("all input regressions must execute without skips")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
