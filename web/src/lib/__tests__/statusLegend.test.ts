import { describe, expect, it } from "vitest";

import { STATUS_TEXT_CLASS } from "../session";
import { glyphTooltip, statusLegend, statusMeaning } from "../statusLegend";
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

describe("glyphTooltip", () => {
  it.each<[string, Parameters<typeof glyphTooltip>, ReturnType<typeof glyphTooltip>]>([
    ["a one-line error", ["Error", { lastError: "pane exited" }], { text: "Error", detail: "pane exited" }],
    [
      "the first non-blank line of a multi-line error",
      ["Error", { lastError: "\n  resume failed  \r\nsecond" }],
      { text: "Error", detail: "resume failed" },
    ],
    [
      "a whitespace-only error",
      ["Error", { lastError: " \n\t" }],
      { text: "Error · no details recorded", detail: null },
    ],
    ["no recorded error", ["Error", {}], { text: "Error · no details recorded", detail: null }],
    [
      "a Structured view Error keeps its meaning",
      ["Error", { lastError: "pane exited", structured: true }],
      { text: "Error: the Session failed; also shown while deleting", detail: null },
    ],
    ["Deleting", ["Deleting", { lastError: "pane exited" }], { text: "Deleting…", detail: null }],
    [
      "another status reads its meaning",
      ["Idle", { lastError: "stale" }],
      {
        text: "Idle: the Agent is ready for your next message",
        detail: null,
      },
    ],
  ])("%s", (_name, args, expected) => {
    expect(glyphTooltip(...args)).toEqual(expected);
  });
});
