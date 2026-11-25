#!/usr/bin/env python3
"""Summarize `projected` events from the daemon log: runs/s, mean and p99 ms.

Run the daemon with AOE_LOG_LEVEL=debug, then:
    aoe logs --no-pager | python3 scripts/bench_publish.py

The logger appends across daemon restarts, and `aoe logs` prints the whole file.
Move the file printed by `aoe logs --path` aside, then start the daemon, so each
run reads only its own events.
"""

import math
import re
import sys
from datetime import datetime, timezone

EVENT = re.compile(r"(\d{4}-\d\d-\d\dT[\d:.]+)Z?.*projected.*project_ms=([\d.]+).*changed=(true|false)")


def stamp(text):
    return datetime.fromisoformat(text).replace(tzinfo=timezone.utc).timestamp()


def main():
    rows = [(stamp(m[1]), float(m[2]), m[3]) for m in map(EVENT.search, sys.stdin) if m]
    if len(rows) < 2:
        sys.exit("no projected events; is AOE_LOG_LEVEL=debug set?")
    span = rows[-1][0] - rows[0][0]
    ms = sorted(r[1] for r in rows)
    changed = sum(r[2] == "true" for r in rows)
    print(f"runs={len(rows)} span_s={span:.1f} runs_per_s={len(rows) / span if span else 0:.2f}")
    print(f"mean_ms={sum(ms) / len(ms):.2f} p99_ms={ms[min(len(ms) - 1, math.ceil(0.99 * len(ms)) - 1)]:.2f}")
    print(f"changed={changed} unchanged={len(rows) - changed}")


main()
