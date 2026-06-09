// @vitest-environment jsdom
//
// Test fixtures for safeStorage and any caller that mocks quota errors:
//
// - `// @vitest-environment jsdom` at the top of a test file enables a real
//   `window.localStorage`. Most component and hook tests use this.
// - Module-level `installFakeLocalStorage()` polyfills `globalThis.localStorage`
//   with a Map-backed Storage for node-env tests (see
//   `web/src/components/diff/comments/storage.test.ts`). The helpers resolve
//   storage via `globalThis.localStorage`, so both fixtures work unchanged.
//
// Canonical failure-path mock:
//   vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
//     throw new DOMException("...", "QuotaExceededError");
//   });

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { isQuotaExceededError, safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";

const KEY = "test:safe-storage";

function makeQuotaError(): DOMException {
  return new DOMException("The quota has been exceeded.", "QuotaExceededError");
}

function makeSecurityError(): DOMException {
  return new DOMException("Storage is disabled.", "SecurityError");
}

describe("safeSetItem", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("returns true and persists the value on success", () => {
    expect(safeSetItem(KEY, "hello")).toBe(true);
    expect(window.localStorage.getItem(KEY)).toBe("hello");
  });

  it("returns false when localStorage throws QuotaExceededError", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw makeQuotaError();
    });
    expect(safeSetItem(KEY, "v")).toBe(false);
  });

  it("returns false when localStorage throws SecurityError (private mode)", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw makeSecurityError();
    });
    expect(safeSetItem(KEY, "v")).toBe(false);
  });

  it("never re-throws on any storage error", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("anything else");
    });
    expect(() => safeSetItem(KEY, "v")).not.toThrow();
    expect(safeSetItem(KEY, "v")).toBe(false);
  });
});

describe("safeRemoveItem", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("removes the value on success", () => {
    window.localStorage.setItem(KEY, "x");
    safeRemoveItem(KEY);
    expect(window.localStorage.getItem(KEY)).toBeNull();
  });

  it("does not throw when removeItem throws", () => {
    vi.spyOn(Storage.prototype, "removeItem").mockImplementation(() => {
      throw new Error("nope");
    });
    expect(() => safeRemoveItem(KEY)).not.toThrow();
  });
});

describe("safeGetItem", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("returns the value when present", () => {
    window.localStorage.setItem(KEY, "hello");
    expect(safeGetItem(KEY)).toBe("hello");
  });

  it("returns null when the key is absent", () => {
    expect(safeGetItem(KEY)).toBeNull();
  });

  it("returns null on read failure", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("disabled");
    });
    expect(safeGetItem(KEY)).toBeNull();
  });
});

describe("isQuotaExceededError", () => {
  it("matches QuotaExceededError by name", () => {
    expect(isQuotaExceededError(makeQuotaError())).toBe(true);
  });

  it("matches NS_ERROR_DOM_QUOTA_REACHED by name (Firefox)", () => {
    const err = new DOMException("quota", "NS_ERROR_DOM_QUOTA_REACHED");
    expect(isQuotaExceededError(err)).toBe(true);
  });

  it("rejects non-DOMException errors", () => {
    expect(isQuotaExceededError(new Error("nope"))).toBe(false);
    expect(isQuotaExceededError("string")).toBe(false);
    expect(isQuotaExceededError(null)).toBe(false);
    expect(isQuotaExceededError(undefined)).toBe(false);
  });

  it("rejects unrelated DOMException", () => {
    expect(isQuotaExceededError(makeSecurityError())).toBe(false);
  });
});
