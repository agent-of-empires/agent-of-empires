import { describe, expect, it } from "vitest";

import { STATUS_TEXT_CLASS } from "../session";
import { statusLegend, statusMeaning } from "../statusLegend";
import type { SessionStatus } from "../types";

describe("statusLegend", () => {
  it("gives every SessionStatus exactly one legend row, in the legend's order", () => {
    const rows = statusLegend();
    // STATUS_TEXT_CLASS is a Record over SessionStatus, so a new status lands in it first.
    const statuses = Object.keys(STATUS_TEXT_CLASS) as SessionStatus[];
    const covered = rows.flatMap((row) => row.statuses);
    expect([...covered].sort()).toEqual([...statuses].sort());
    expect(new Set(covered).size).toBe(covered.length);
    expect(rows.map((row) => row.label)).toEqual([
      "Running",
      "Waiting",
      "Starting",
      "Creating",
      "Idle, just finished",
      "Idle",
      "Dormant",
      "Stopped",
      "Unknown",
      "Error",
    ]);
    // A glyph's tooltip reads the same words as its legend row.
    for (const row of rows) {
      const opts = { fresh: row.variant === "fresh", dormant: row.variant === "dormant" };
      for (const status of row.variant === "status" ? row.statuses : (["Idle"] as SessionStatus[])) {
        expect(statusMeaning(status, opts), `${row.label} ${status}`).toEqual({
          label: row.label,
          description: row.description,
        });
      }
    }
  });
});
