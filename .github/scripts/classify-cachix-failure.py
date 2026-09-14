#!/usr/bin/env python3
"""Decide whether a failed cachix-push run should open an issue.

A failure in the build step means main no longer builds under Nix, and alerts
at once. A failure anywhere else (installing Nix, configuring or pushing to
Cachix) is usually a runner or Cachix blip, so it only alerts when the previous
completed run on main also failed; a missing secret fails every run and is
still reported on the second one. A run this cannot classify alerts, so a
renamed build step cannot silence real breakage.

Reads the `GET /repos/{owner}/{repo}/actions/runs/{run_id}/jobs` payload on
stdin and prints `kind=` and `alert=` lines for `$GITHUB_OUTPUT`.

Usage:
    gh api "repos/$REPO/actions/runs/$RUN_ID/jobs" \\
      | python3 .github/scripts/classify-cachix-failure.py \\
          --build-step "Build aoe-with-web" --previous-conclusion "$PREVIOUS"

    python3 .github/scripts/classify-cachix-failure.py --self-test
"""

import argparse
import json
import sys

FAILED = {"failure", "timed_out"}
# A user cancel cancels the whole job, so a cancelled build step in a failed
# job means the build was cut off, e.g. by the job timeout.
BUILD_FAILED = FAILED | {"cancelled"}


class ClassificationError(Exception):
    pass


def classify(payload, build_step):
    """`build`, `infra` or `unknown` for the failed jobs in `payload`."""
    if not isinstance(payload, dict) or not isinstance(payload.get("jobs"), list):
        raise ClassificationError("expected an object with a `jobs` array")

    failed = [job for job in payload["jobs"] if isinstance(job, dict) and job.get("conclusion") in FAILED]
    if not failed:
        return "unknown"

    kinds = set()
    for job in failed:
        steps = job.get("steps") if isinstance(job.get("steps"), list) else []
        build = [s for s in steps if isinstance(s, dict) and s.get("name") == build_step]
        if not build:
            kinds.add("unknown")
        elif build[0].get("conclusion") in BUILD_FAILED:
            kinds.add("build")
        else:
            kinds.add("infra")

    for kind in ("build", "unknown"):
        if kind in kinds:
            return kind
    return "infra"


def should_alert(kind, previous_conclusion):
    return kind != "infra" or previous_conclusion in FAILED


def self_test():
    step = "Build aoe-with-web"

    def job(conclusion, *steps):
        return {"conclusion": conclusion, "steps": [{"name": n, "conclusion": c} for n, c in steps]}

    ok = job("success", ("Set up Cachix", "success"), (step, "success"))
    build_failed = job("failure", ("Set up Cachix", "success"), (step, "failure"))
    # Observed on 2026-09-14: the Nix install failed on macOS, skipping the build.
    install_failed = job("failure", ("Run ./.github/actions/install-nix", "failure"), (step, "skipped"))
    push_failed = job("failure", (step, "success"), ("Push to Cachix", "failure"))
    timed_out = job("failure", (step, "cancelled"))
    renamed = job("failure", ("Build aoe", "failure"))
    # The notify job itself is still running when it lists the run's jobs.
    running = {"conclusion": None, "steps": []}

    cases = [
        ([ok, build_failed, running], "build"),
        ([install_failed, build_failed], "build"),
        ([timed_out], "build"),
        ([ok, install_failed, running], "infra"),
        ([push_failed], "infra"),
        ([renamed], "unknown"),
        ([install_failed, renamed], "unknown"),
        ([{"conclusion": "failure"}], "unknown"),
        ([ok, running], "unknown"),
    ]
    for jobs, expected in cases:
        actual = classify({"jobs": jobs}, step)
        assert actual == expected, f"{jobs!r}: {actual} != {expected}"

    for payload in ([], {"jobs": None}, {}):
        try:
            classify(payload, step)
        except ClassificationError:
            continue
        raise AssertionError(f"{payload!r} should not classify")

    alerts = [
        ("build", "success", True),
        ("unknown", "success", True),
        ("infra", "success", False),
        ("infra", "", False),
        ("infra", "cancelled", False),
        ("infra", "failure", True),
    ]
    for kind, previous, expected in alerts:
        assert should_alert(kind, previous) == expected, f"{kind} after {previous!r}"

    print("OK: build failures alert, a lone infra failure does not, unclassifiable runs alert.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-step", help="name of the step that runs the Nix build")
    parser.add_argument("--previous-conclusion", default="", help="conclusion of the previous completed run")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return 0

    if not args.build_step:
        parser.error("missing required argument: --build-step")

    try:
        kind = classify(json.load(sys.stdin), args.build_step)
    except (ValueError, ClassificationError) as exc:
        print(f"::warning::could not classify the failed run, alerting anyway: {exc}", file=sys.stderr)
        kind = "unknown"

    print(f"kind={kind}")
    print(f"alert={'true' if should_alert(kind, args.previous_conclusion) else 'false'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
