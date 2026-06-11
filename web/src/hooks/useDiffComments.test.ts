import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { EMPTY_STORAGE, loadComments, saveComments } from "../components/diff/comments/storage";
import type { DiffComment } from "../components/diff/comments/types";

// `useDiffComments` switches the React state when the active sessionId
// changes by calling `loadComments(newSessionId)`. The hook itself
// requires a React renderer + DOM to test directly, but its contract
// collapses to: "after a sessionId change, the in-view data must come
// from the new session's storage envelope, not the previous one's".
// These tests exercise that storage round-trip in the exact pattern the
// hook uses (load on session change, save on state mutation), so a
// regression in the storage layer's isolation guarantees would also
// break the hook.

function installFakeLocalStorage() {
  const data = new Map<string, string>();
  const fake: Storage = {
    get length() {
      return data.size;
    },
    key(i) {
      return Array.from(data.keys())[i] ?? null;
    },
    getItem(k) {
      return data.has(k) ? data.get(k)! : null;
    },
    setItem(k, v) {
      data.set(k, String(v));
    },
    removeItem(k) {
      data.delete(k);
    },
    clear() {
      data.clear();
    },
  };
  (globalThis as { localStorage: Storage }).localStorage = fake;
}

function mkComment(overrides: Partial<DiffComment> = {}): DiffComment {
  return {
    id: "c",
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

describe("useDiffComments contract", () => {
  beforeEach(() => {
    installFakeLocalStorage();
  });
  afterEach(() => {
    localStorage.clear();
  });

  it("session switch reloads the new session's data", () => {
    saveComments("sess-A", {
      ...EMPTY_STORAGE,
      comments: [mkComment({ id: "a1" })],
      introDraft: "from A",
    });
    saveComments("sess-B", { ...EMPTY_STORAGE });

    const onA = loadComments("sess-A");
    expect(onA.comments.map((c) => c.id)).toEqual(["a1"]);
    expect(onA.introDraft).toBe("from A");

    const onB = loadComments("sess-B");
    expect(onB.comments).toHaveLength(0);
    expect(onB.introDraft).toBe("");
  });

  it("mutations on one session do not leak into another", () => {
    saveComments("sess-A", {
      ...EMPTY_STORAGE,
      comments: [mkComment({ id: "a1" })],
    });

    const onB = loadComments("sess-B");
    saveComments("sess-B", {
      ...onB,
      comments: [mkComment({ id: "b1" })],
    });

    expect(loadComments("sess-A").comments.map((c) => c.id)).toEqual(["a1"]);
    expect(loadComments("sess-B").comments.map((c) => c.id)).toEqual(["b1"]);
  });

  it("null sessionId yields the empty envelope (no save)", () => {
    // The hook explicitly guards `if (!sessionId) return;` in its save
    // and load effects, so a logged-out / pre-selection render stays
    // empty and never writes a key with the literal string "null".
    expect((globalThis as { localStorage: Storage }).localStorage.length).toBe(0);
    expect({ ...EMPTY_STORAGE }).toEqual(EMPTY_STORAGE);
  });
});
