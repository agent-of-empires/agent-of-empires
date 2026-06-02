import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  clearStoredComments,
  EMPTY_STORAGE,
  isEmptyState,
  loadComments,
  saveComments,
  storageKey,
} from "./storage";
import type { DiffComment, DiffCommentsStorageV1 } from "./types";

// Vitest default env is node; install a minimal in-memory localStorage
// for these tests so we can exercise the storage layer end-to-end.
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
  // Use globalThis to avoid window typing in node env.
  (globalThis as { localStorage: Storage }).localStorage = fake;
  return data;
}

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

describe("storage", () => {
  beforeEach(() => {
    installFakeLocalStorage();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("returns the empty envelope when key is absent", () => {
    const state = loadComments("sess-1");
    expect(state).toEqual(EMPTY_STORAGE);
  });

  it("round-trips a full envelope", () => {
    const original: DiffCommentsStorageV1 = {
      version: 1,
      comments: [mkComment({ id: "a" }), mkComment({ id: "b" })],
      clearAfterSend: false,
      introDraft: "hi",
      outroDraft: "bye",
    };
    saveComments("sess-1", original);
    const loaded = loadComments("sess-1");
    expect(loaded).toEqual(original);
  });

  it("scopes by sessionId", () => {
    saveComments("sess-1", {
      ...EMPTY_STORAGE,
      comments: [mkComment({ id: "a" })],
    });
    saveComments("sess-2", {
      ...EMPTY_STORAGE,
      comments: [mkComment({ id: "b" })],
    });
    expect(loadComments("sess-1").comments.map((c) => c.id)).toEqual(["a"]);
    expect(loadComments("sess-2").comments.map((c) => c.id)).toEqual(["b"]);
  });

  it("returns empty envelope when JSON is corrupt", () => {
    localStorage.setItem(storageKey("sess-1"), "not json");
    expect(loadComments("sess-1")).toEqual(EMPTY_STORAGE);
  });

  it("returns empty envelope when version is unknown", () => {
    localStorage.setItem(
      storageKey("sess-1"),
      JSON.stringify({
        version: 99,
        comments: [mkComment({})],
        clearAfterSend: true,
        introDraft: "",
        outroDraft: "",
      }),
    );
    expect(loadComments("sess-1")).toEqual(EMPTY_STORAGE);
  });

  it("filters out malformed comment records", () => {
    localStorage.setItem(
      storageKey("sess-1"),
      JSON.stringify({
        version: 1,
        clearAfterSend: true,
        introDraft: "",
        outroDraft: "",
        comments: [
          mkComment({ id: "good" }),
          { id: "bad", filePath: 12, side: "new", startLine: 1, endLine: 1, body: "", capturedSnippet: "", createdAt: "x" },
          { id: "no-side", filePath: "x", side: "left", startLine: 1, endLine: 1, body: "", capturedSnippet: "", createdAt: "x" },
        ],
      }),
    );
    expect(loadComments("sess-1").comments.map((c) => c.id)).toEqual(["good"]);
  });

  it("falls back to defaults for missing top-level fields", () => {
    localStorage.setItem(
      storageKey("sess-1"),
      JSON.stringify({
        version: 1,
        comments: [],
      }),
    );
    const state = loadComments("sess-1");
    expect(state.clearAfterSend).toBe(true);
    expect(state.introDraft).toBe("");
    expect(state.outroDraft).toBe("");
  });

  it("survives a write that throws (quota-exceeded etc.)", () => {
    const spy = vi.spyOn(localStorage, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceeded");
    });
    // Non-empty state still routes through setItem; should not throw despite
    // the failing write.
    expect(() =>
      saveComments("sess-1", { ...EMPTY_STORAGE, comments: [mkComment({})] }),
    ).not.toThrow();
    expect(spy).toHaveBeenCalled();
  });

  it("uses a deterministic, versioned key", () => {
    expect(storageKey("abc")).toBe("aoe:diff-comments:v1:abc");
  });

  describe("isEmptyState", () => {
    it("is true for the empty envelope", () => {
      expect(isEmptyState(EMPTY_STORAGE)).toBe(true);
    });

    it("ignores a non-default clearAfterSend toggle", () => {
      expect(isEmptyState({ ...EMPTY_STORAGE, clearAfterSend: false })).toBe(
        true,
      );
    });

    it("is false with a comment", () => {
      expect(
        isEmptyState({ ...EMPTY_STORAGE, comments: [mkComment({})] }),
      ).toBe(false);
    });

    it("is false with draft text", () => {
      expect(isEmptyState({ ...EMPTY_STORAGE, introDraft: "hi" })).toBe(false);
      expect(isEmptyState({ ...EMPTY_STORAGE, outroDraft: "bye" })).toBe(false);
    });
  });

  describe("saveComments empty-removal", () => {
    it("removes the key instead of writing an empty record", () => {
      const data = installFakeLocalStorage();
      saveComments("sess-1", EMPTY_STORAGE);
      expect(data.has(storageKey("sess-1"))).toBe(false);
    });

    it("removes an existing key when state goes back to empty", () => {
      const data = installFakeLocalStorage();
      saveComments("sess-1", { ...EMPTY_STORAGE, comments: [mkComment({})] });
      expect(data.has(storageKey("sess-1"))).toBe(true);
      saveComments("sess-1", EMPTY_STORAGE);
      expect(data.has(storageKey("sess-1"))).toBe(false);
    });

    it("treats a lone clearAfterSend toggle as empty and removes the key", () => {
      const data = installFakeLocalStorage();
      saveComments("sess-1", { ...EMPTY_STORAGE, clearAfterSend: false });
      expect(data.has(storageKey("sess-1"))).toBe(false);
    });

    it("still persists non-empty state", () => {
      const data = installFakeLocalStorage();
      saveComments("sess-1", { ...EMPTY_STORAGE, comments: [mkComment({})] });
      expect(data.has(storageKey("sess-1"))).toBe(true);
      expect(loadComments("sess-1").comments).toHaveLength(1);
    });
  });

  describe("clearStoredComments", () => {
    it("removes the key for a single session", () => {
      const data = installFakeLocalStorage();
      saveComments("sess-1", { ...EMPTY_STORAGE, comments: [mkComment({})] });
      saveComments("sess-2", { ...EMPTY_STORAGE, comments: [mkComment({})] });
      clearStoredComments("sess-1");
      expect(data.has(storageKey("sess-1"))).toBe(false);
      expect(data.has(storageKey("sess-2"))).toBe(true);
    });

    it("is a no-op for a session with no stored comments", () => {
      const data = installFakeLocalStorage();
      expect(() => clearStoredComments("absent")).not.toThrow();
      expect(data.has(storageKey("absent"))).toBe(false);
    });
  });
});
