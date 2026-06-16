// Branch coverage for the pure queue-recall navigation (#2147).

import { describe, expect, it } from "vitest";

import { nextRecallTarget, recallBannerInfo, type RecallCursor } from "./recallNav";
import type { QueuedPrompt } from "../../lib/acpTypes";

const q = (...ids: string[]): QueuedPrompt[] =>
  ids.map((id) => ({ id, text: `text-${id}`, queuedAt: "2026-01-01T00:00:00Z" }));

const cursor = (id: string, stashedDraft = "draft"): RecallCursor => ({ id, stashedDraft });

describe("nextRecallTarget — older (ArrowUp)", () => {
  it("exits when the queue is empty", () => {
    expect(nextRecallTarget([], null, "older", "draft")).toEqual({ kind: "exit" });
  });

  it("enters at the newest entry, stashing the current draft", () => {
    expect(nextRecallTarget(q("a", "b"), null, "older", "my draft")).toEqual({
      kind: "load",
      cursor: { id: "b", stashedDraft: "my draft" },
      text: "text-b",
    });
  });

  it("walks to the older entry, preserving the stashed draft", () => {
    expect(nextRecallTarget(q("a", "b"), cursor("b", "kept"), "older", "ignored")).toEqual({
      kind: "load",
      cursor: { id: "a", stashedDraft: "kept" },
      text: "text-a",
    });
  });

  it("stays put at the oldest entry (no wrap)", () => {
    expect(nextRecallTarget(q("a", "b"), cursor("a"), "older", "draft")).toEqual({ kind: "none" });
  });

  it("exits when the browsed entry has drained", () => {
    expect(nextRecallTarget(q("a", "b"), cursor("gone"), "older", "draft")).toEqual({ kind: "exit" });
  });
});

describe("nextRecallTarget — newer (ArrowDown)", () => {
  it("no-ops when not browsing", () => {
    expect(nextRecallTarget(q("a", "b"), null, "newer", "draft")).toEqual({ kind: "none" });
  });

  it("exits when the browsed entry has drained", () => {
    expect(nextRecallTarget(q("a", "b"), cursor("gone"), "newer", "draft")).toEqual({ kind: "exit" });
  });

  it("walks to the newer entry", () => {
    expect(nextRecallTarget(q("a", "b"), cursor("a", "kept"), "newer", "draft")).toEqual({
      kind: "load",
      cursor: { id: "b", stashedDraft: "kept" },
      text: "text-b",
    });
  });

  it("restores the stashed draft past the newest entry", () => {
    expect(nextRecallTarget(q("a", "b"), cursor("b", "my draft"), "newer", "draft")).toEqual({
      kind: "restore",
      text: "my draft",
    });
  });
});

describe("recallBannerInfo", () => {
  it("is null without a cursor", () => {
    expect(recallBannerInfo(q("a", "b"), null)).toBeNull();
  });

  it("counts position from the newest entry", () => {
    expect(recallBannerInfo(q("a", "b", "c"), cursor("c"))).toEqual({ pos: 1, total: 3 });
    expect(recallBannerInfo(q("a", "b", "c"), cursor("a"))).toEqual({ pos: 3, total: 3 });
  });

  it("is null when the cursor entry has drained", () => {
    expect(recallBannerInfo(q("a", "b"), cursor("gone"))).toBeNull();
  });
});
