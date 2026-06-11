// @vitest-environment jsdom
//
// sweepOrphanComments iterates window.localStorage, so it needs a real DOM
// storage (jsdom) rather than the node-env fake used by storage.test.ts.
// Mirrors the sweepOrphanDrafts coverage in acpDrafts.test.ts (#1842).

import { beforeEach, describe, expect, it } from "vitest";

import { EMPTY_STORAGE, saveComments, storageKey, sweepOrphanComments } from "./storage";
import type { DiffComment } from "./types";

function mkComment(overrides: Partial<DiffComment> = {}): DiffComment {
  return {
    id: "c1",
    filePath: "src/foo.rs",
    side: "new",
    startLine: 5,
    endLine: 5,
    body: "review",
    capturedSnippet: "snippet",
    createdAt: "2025-01-01T00:00:00Z",
    ...overrides,
  };
}

beforeEach(() => {
  window.localStorage.clear();
});

describe("sweepOrphanComments", () => {
  it("removes keys for sessions not in the active set", () => {
    saveComments("active", { ...EMPTY_STORAGE, comments: [mkComment({})] });
    saveComments("orphan", { ...EMPTY_STORAGE, comments: [mkComment({})] });
    sweepOrphanComments(new Set(["active"]));
    expect(window.localStorage.getItem(storageKey("active"))).not.toBeNull();
    expect(window.localStorage.getItem(storageKey("orphan"))).toBeNull();
  });

  it("leaves unrelated keys untouched", () => {
    window.localStorage.setItem("acp:draft:foo", "keep me");
    saveComments("orphan", { ...EMPTY_STORAGE, comments: [mkComment({})] });
    sweepOrphanComments(new Set());
    expect(window.localStorage.getItem("acp:draft:foo")).toBe("keep me");
    expect(window.localStorage.getItem(storageKey("orphan"))).toBeNull();
  });

  it("is a no-op when every key is active", () => {
    saveComments("a", { ...EMPTY_STORAGE, comments: [mkComment({})] });
    saveComments("b", { ...EMPTY_STORAGE, comments: [mkComment({})] });
    sweepOrphanComments(new Set(["a", "b"]));
    expect(window.localStorage.getItem(storageKey("a"))).not.toBeNull();
    expect(window.localStorage.getItem(storageKey("b"))).not.toBeNull();
  });
});
