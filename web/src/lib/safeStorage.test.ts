// @vitest-environment jsdom
// Callers mock quota failures with:
//   vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
//     throw new DOMException("...", "QuotaExceededError");
//   });

import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { isQuotaExceededError, safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";

const KEY = "test:safe-storage";

beforeEach(() => window.localStorage.clear());
afterEach(() => vi.restoreAllMocks());

it("round-trips values through localStorage", () => {
  expect(safeGetItem(KEY)).toBeNull();
  expect(safeSetItem(KEY, "hello")).toBe(true);
  expect(window.localStorage.getItem(KEY)).toBe("hello");
  expect(safeGetItem(KEY)).toBe("hello");
  safeRemoveItem(KEY);
  expect(window.localStorage.getItem(KEY)).toBeNull();
});

it("swallows every storage error and returns a fallback", () => {
  for (const error of [
    new DOMException("quota", "QuotaExceededError"),
    new DOMException("private mode", "SecurityError"),
    new Error("anything else"),
  ]) {
    const fail = () => {
      throw error;
    };
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(fail);
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(fail);
    vi.spyOn(Storage.prototype, "removeItem").mockImplementation(fail);
    expect(safeSetItem(KEY, "v"), error.name).toBe(false);
    expect(safeGetItem(KEY), error.name).toBeNull();
    expect(() => safeRemoveItem(KEY), error.name).not.toThrow();
  }
});

it("isQuotaExceededError matches only quota DOMExceptions", () => {
  expect(isQuotaExceededError(new DOMException("q", "QuotaExceededError"))).toBe(true);
  expect(isQuotaExceededError(new DOMException("q", "NS_ERROR_DOM_QUOTA_REACHED"))).toBe(true);
  for (const other of [new DOMException("s", "SecurityError"), new Error("nope"), "string", null, undefined]) {
    expect(isQuotaExceededError(other), String(other)).toBe(false);
  }
});
