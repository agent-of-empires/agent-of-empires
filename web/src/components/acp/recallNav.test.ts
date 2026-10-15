// Branch coverage for the pure queue-recall navigation (#2147).

import { describe, expect, it } from "vitest";

import { nextRecallTarget, recallBannerInfo, type RecallCursor } from "./recallNav";
import type { QueuedPrompt } from "../../lib/acpTypes";

const q = (...ids: string[]): QueuedPrompt[] =>
  ids.map((id) => ({ id, text: `text-${id}`, queuedAt: "2026-01-01T00:00:00Z" }));

const cursor = (id: string, stashedDraft = "draft"): RecallCursor => ({ id, stashedDraft });

describe("recallNav", () => {
  it("nextRecallTarget walks, stops, exits, and restores", () => {
    const cases: [string, Parameters<typeof nextRecallTarget>, ReturnType<typeof nextRecallTarget>][] = [
      ["older: empty queue exits", [[], null, "older", "draft"], { kind: "exit" }],
      [
        "older: enters at the newest, stashing the draft",
        [q("a", "b"), null, "older", "my draft"],
        { kind: "load", cursor: { id: "b", stashedDraft: "my draft" }, text: "text-b" },
      ],
      [
        "older: walks back, preserving the stash",
        [q("a", "b"), cursor("b", "kept"), "older", "ignored"],
        { kind: "load", cursor: { id: "a", stashedDraft: "kept" }, text: "text-a" },
      ],
      ["older: stays at the oldest", [q("a", "b"), cursor("a"), "older", "draft"], { kind: "none" }],
      ["older: drained entry exits", [q("a", "b"), cursor("gone"), "older", "draft"], { kind: "exit" }],
      ["newer: no-op when not browsing", [q("a", "b"), null, "newer", "draft"], { kind: "none" }],
      ["newer: drained entry exits", [q("a", "b"), cursor("gone"), "newer", "draft"], { kind: "exit" }],
      [
        "newer: walks forward",
        [q("a", "b"), cursor("a", "kept"), "newer", "draft"],
        { kind: "load", cursor: { id: "b", stashedDraft: "kept" }, text: "text-b" },
      ],
      [
        "newer: restores the stash past the newest",
        [q("a", "b"), cursor("b", "my draft"), "newer", "draft"],
        { kind: "restore", text: "my draft" },
      ],
    ];
    for (const [label, args, expected] of cases) expect(nextRecallTarget(...args), label).toEqual(expected);
  });

  it("recallBannerInfo counts from the newest and is null without a live cursor", () => {
    expect(recallBannerInfo(q("a", "b"), null)).toBeNull();
    expect(recallBannerInfo(q("a", "b", "c"), cursor("c"))).toEqual({ pos: 1, total: 3 });
    expect(recallBannerInfo(q("a", "b", "c"), cursor("a"))).toEqual({ pos: 3, total: 3 });
    expect(recallBannerInfo(q("a", "b"), cursor("gone"))).toBeNull();
  });
});
