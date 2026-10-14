import { describe, expect, it } from "vitest";

import { deriveRowModel } from "../sidebar/rowModel";
import { EMPTY_OPTIMISTIC } from "../../lib/sidebarOptimistic";
import type { SessionResponse } from "../../lib/types";
import { makeSession, makeWorkspace } from "./fixtures";

const derive = (sessions: SessionResponse[], pluginAttention: ReadonlySet<string>, unread = false) =>
  deriveRowModel(makeWorkspace("w", sessions), EMPTY_OPTIMISTIC, {
    idleDecayWindowMs: 0,
    isActive: false,
    unreadIndicatorEnabled: unread,
    pluginAttention,
  });

describe("deriveRowModel plugin attention (#4446)", () => {
  it.each<[string, Partial<SessionResponse>, string[], boolean, string]>([
    ["a toned idle session", {}, ["s1"], true, "needs your attention"],
    ["an untoned idle session", {}, [], false, "needs your attention"],
    ["a toned Error session keeps the error hint", { status: "Error" }, ["s1"], true, "needs attention (error)"],
    ["a toned Waiting session keeps the waiting hint", { status: "Waiting" }, ["s1"], true, "waiting for your input"],
    ["a toned archived session", { archived_at: "2025-01-02T00:00:00Z" }, ["s1"], false, "needs your attention"],
    ["a toned snoozed session", { snoozed_until: "2099-01-01T00:00:00Z" }, ["s1"], false, "needs your attention"],
  ])("%s", (_name, over, toned, needsAttention, attentionHint) => {
    const model = derive([makeSession(over)], new Set(toned));
    expect(model.needsAttention).toBe(needsAttention);
    expect(model.attentionHint).toBe(attentionHint);
  });

  it("flags the row when any of its sessions is toned", () => {
    const sessions = [makeSession({ id: "a" }), makeSession({ id: "b" })];
    expect(derive(sessions, new Set(["b"])).needsAttention).toBe(true);
    expect(derive(sessions, new Set(["other"])).needsAttention).toBe(false);
  });

  it("flags an unread toned row, where the unread glyph takes over the pulse", () => {
    const model = derive([makeSession({ unread: true })], new Set(["s1"]), true);
    expect(model.needsAttention).toBe(true);
    expect(model.showUnreadGlyph).toBe(true);
  });
});
